//! RocksDB's `db/blob/` directory: the blob index a table or memtable entry holds in place of a
//! value stored in a blob file (docs/research/24 §1.16). Blob files are read from P13.

pub mod blob_index;
