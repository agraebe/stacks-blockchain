/*
 copyright: (c) 2013-2020 by Blockstack PBC, a public benefit corporation.

 This file is part of Blockstack.

 Blockstack is free software. You may redistribute or modify
 it under the terms of the GNU General Public License as published by
 the Free Software Foundation, either version 3 of the License or
 (at your option) any later version.

 Blockstack is distributed in the hope that it will be useful,
 but WITHOUT ANY WARRANTY, including without the implied warranty of
 MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 GNU General Public License for more details.

 You should have received a copy of the GNU General Public License
 along with Blockstack. If not, see <http://www.gnu.org/licenses/>.
*/

use std::fmt;
use std::error;
use std::io;
use std::io::{
    Read,
    Write,
    Seek,
    SeekFrom,
    Cursor,
    BufWriter,
};

use std::char::from_digit;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::collections::{VecDeque, HashMap, HashSet};

use std::fs;
use std::path::{
    Path,
    PathBuf
};

use std::os;
use std::iter::FromIterator;

use regex::Regex;

use chainstate::burn::BlockHeaderHash;
use chainstate::burn::BLOCK_HEADER_HASH_ENCODED_SIZE;

use chainstate::stacks::index::{
    TrieHash,
    TRIEHASH_ENCODED_SIZE,
    BlockMap,
    trie_sql
};

use chainstate::stacks::index::storage::{
    TrieFileStorage,
};

use chainstate::stacks::index::bits::{
    get_node_byte_len,
    write_nodetype_bytes,
    read_hash_bytes,
    read_block_identifier,
    read_node_hash_bytes as bits_read_node_hash_bytes,
    read_nodetype,
    get_node_hash,
};

use chainstate::stacks::index::node::{
    is_backptr,
    clear_backptr,
    set_backptr,
    TrieNodeType,
    TrieNode4,
    TrieNode16,
    TrieNode48,
    TrieNode256,
    TrieLeaf,
    TrieNodeID,
    TriePtr,
    TriePath,
    TrieNode
};

use rusqlite::{
    Connection, OptionalExtension,
    types::{ FromSql,
             ToSql },
    NO_PARAMS,
    Error as SqliteError
};

use std::convert::{
    TryFrom,
    TryInto
};

use chainstate::stacks::index::Error as Error;

use util::log;

static SQL_MARF_DATA_TABLE: &str = "
CREATE TABLE IF NOT EXISTS marf_data (
   block_id INTEGER PRIMARY KEY, 
   block_hash TEXT UNIQUE NOT NULL,
   data BLOB NOT NULL
);

CREATE INDEX IF NOT EXISTS block_hash_marf_data ON marf_data(block_hash);
";
static SQL_MARF_MINED_TABLE: &str = "
CREATE TABLE IF NOT EXISTS mined_blocks (
   block_id INTEGER PRIMARY KEY, 
   block_hash TEXT UNIQUE NOT NULL,
   data BLOB NOT NULL
);

CREATE INDEX IF NOT EXISTS block_hash_mined_blocks ON mined_blocks(block_hash);
";
static SQL_EXTENSION_LOCKS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS block_extension_locks (block_hash TEXT PRIMARY KEY);
";

pub fn create_tables_if_needed(conn: &mut Connection) -> Result<(), Error> {
    let tx = conn.transaction()?;

    tx.execute_batch(SQL_MARF_DATA_TABLE)?;
    tx.execute_batch(SQL_MARF_MINED_TABLE)?;
    tx.execute_batch(SQL_EXTENSION_LOCKS_TABLE)?;

    tx.commit().map_err(|e| e.into())
}

pub fn get_block_identifier(conn: &Connection, bhh: &BlockHeaderHash) -> Result<u32, Error> {
    conn.query_row("SELECT block_id FROM marf_data WHERE block_hash = ?", &[bhh],
                   |row| row.get("block_id"))
        .map_err(|e| e.into())
}

pub fn get_block_hash(conn: &Connection, local_id: u32) -> Result<BlockHeaderHash, Error> {
    let result = conn.query_row("SELECT block_hash FROM marf_data WHERE block_id = ?", &[local_id],
                                |row| row.get("block_hash"))
        .optional()?;
    result.ok_or_else(|| {
        error!("Failed to get block header hash of local ID {}", local_id);
        Error::NotFoundError
    })
}

pub fn write_trie_blob(conn: &Connection, block_hash: &BlockHeaderHash, data: &[u8]) -> Result<u32, Error> {
    let args: &[&dyn ToSql] = &[block_hash, &data];
    let mut s = conn.prepare("INSERT INTO marf_data (block_hash, data) VALUES (?, ?)")?;
    let block_id = s.insert(args)?
        .try_into()
        .expect("EXHAUSTION: MARF cannot track more than 2**31 - 1 blocks");
    Ok(block_id)
}

pub fn write_trie_blob_to_mined(conn: &Connection, block_hash: &BlockHeaderHash, data: &[u8]) -> Result<u32, Error> {
    let args: &[&dyn ToSql] = &[block_hash, &data];
    let mut s = conn.prepare("INSERT OR REPLACE INTO mined_blocks (block_hash, data) VALUES (?, ?)")?;
    let block_id = s.insert(args)?
        .try_into()
        .expect("EXHAUSTION: MARF cannot track more than 2**31 - 1 blocks");
    Ok(block_id)
}

#[cfg(test)]
pub fn read_all_block_hashes_and_roots(conn: &Connection) -> Result<Vec<(TrieHash, BlockHeaderHash)>, Error> {
    let mut s = conn.prepare("SELECT block_hash, data FROM marf_data")?;
    let rows = s.query_and_then(NO_PARAMS, |row| {
        let block_hash: BlockHeaderHash = row.get("block_hash");
        let data = row.get_raw("data")
            .as_blob().expect("DB Corruption: MARF data is non-blob");
        let start = TrieFileStorage::root_ptr_disk() as usize;
        let trie_hash = TrieHash(read_hash_bytes(&mut &data[start..])?);
        Ok((trie_hash, block_hash))
    })?;
    rows.collect()
}

pub fn read_node_hash_bytes<W: Write>(conn: &Connection, w: &mut W, block_id: u32, ptr: &TriePtr) -> Result<(), Error> {
    let mut blob = conn.blob_open(rusqlite::DatabaseName::Main, "marf_data", "data", block_id.into(), true)?;
    let hash_buff = bits_read_node_hash_bytes(&mut blob, ptr)?;
    w.write_all(&hash_buff)
        .map_err(|e| e.into())
}

pub fn read_node_hash_bytes_by_bhh<W: Write>(conn: &Connection, w: &mut W, bhh: &BlockHeaderHash, ptr: &TriePtr) -> Result<(), Error> {
    let row_id: i64 = conn.query_row("SELECT block_id FROM marf_data WHERE block_hash = ?",
                                     &[bhh], |r| r.get("block_id"))?;
    let mut blob = conn.blob_open(rusqlite::DatabaseName::Main, "marf_data", "data", row_id, true)?;
    let hash_buff = bits_read_node_hash_bytes(&mut blob, ptr)?;
    w.write_all(&hash_buff)
        .map_err(|e| e.into())
}

pub fn read_node_type(conn: &Connection, block_id: u32, ptr: &TriePtr) -> Result<(TrieNodeType, TrieHash), Error> {
    let mut blob = conn.blob_open(rusqlite::DatabaseName::Main, "marf_data", "data", block_id.into(), true)?;
    read_nodetype(&mut blob, ptr)
}

pub fn get_node_hash_bytes(conn: &Connection, block_id: u32, ptr: &TriePtr) -> Result<TrieHash, Error> {
    let mut blob = conn.blob_open(rusqlite::DatabaseName::Main, "marf_data", "data", block_id.into(), true)?;
    let hash_buff = bits_read_node_hash_bytes(&mut blob, ptr)?;
    Ok(TrieHash(hash_buff))
}

pub fn get_node_hash_bytes_by_bhh(conn: &Connection, bhh: &BlockHeaderHash, ptr: &TriePtr) -> Result<TrieHash, Error> {
    let row_id: i64 = conn.query_row("SELECT block_id FROM marf_data WHERE block_hash = ?",
                                     &[bhh], |r| r.get("block_id"))?;
    let mut blob = conn.blob_open(rusqlite::DatabaseName::Main, "marf_data", "data", row_id, true)?;
    let hash_buff = bits_read_node_hash_bytes(&mut blob, ptr)?;
    Ok(TrieHash(hash_buff))
}

pub fn lock_bhh_for_extension(conn: &mut Connection, bhh: &BlockHeaderHash) -> Result<bool, Error> {
    let tx = conn.transaction()?;
    let is_bhh_committed = tx.query_row("SELECT 1 FROM marf_data WHERE block_hash = ? LIMIT 1", &[bhh],
                                        |_row| ()).optional()?.is_some();
    if is_bhh_committed {
        return Ok(false)
    }

    let is_bhh_locked = tx.query_row("SELECT 1 FROM block_extension_locks WHERE block_hash = ? LIMIT 1", &[bhh],
                                     |_row| ()).optional()?.is_some();
    if is_bhh_locked {
        return Ok(false)
    }

    tx.execute("INSERT INTO block_extension_locks (block_hash) VALUES (?)", &[bhh])?;

    tx.commit()?;
    Ok(true)
}

pub fn count_blocks(conn: &Connection) -> Result<u32, Error> {
    let result = conn.query_row("SELECT IFNULL(MAX(block_id), 0) AS count FROM marf_data", NO_PARAMS, |row| row.get("count"))?;
    Ok(result)
}

pub fn drop_lock(conn: &Connection, bhh: &BlockHeaderHash) -> Result<(), Error> {
    conn.execute("DELETE FROM block_extension_locks WHERE block_hash = ?", &[bhh])?;
    Ok(())
}

pub fn clear_lock_data(conn: &Connection) -> Result<(), Error> {
    conn.execute("DELETE FROM block_extension_locks", NO_PARAMS)?;
    Ok(())
}

pub fn clear_tables(conn: &mut Connection) -> Result<(), Error> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM block_extension_locks", NO_PARAMS)?;
    tx.execute("DELETE FROM marf_data", NO_PARAMS)?;
    tx.execute("DELETE FROM mined_blocks", NO_PARAMS)?;
    tx.commit().map_err(|e| e.into())
}
