//! The compression codecs the engine reads and writes RocksDB's files with, its own
//! (docs/design/engine.md §5).

pub mod deflate;
pub mod lz4;
pub mod snappy;
pub mod zstd;
