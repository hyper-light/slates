//! mantle's conversion of RocksDB 11.8.1 (tag `v11.8.1`, commit `abeebd963`) to Rust.
//!
//! The engine reads every file RocksDB 11.8.1 writes in the configurations it keeps, and RocksDB
//! reads every file this engine writes (docs/research/24 §1.18; docs/design/engine.md). Modules
//! follow RocksDB's source tree: `util::coding` converts `util/coding.h`, `table::format`
//! converts `table/format.{h,cc}`, and so on, each citing the files it converts.
//!
//! Where RocksDB returns a `Status` or a `bool`, or reads past a buffer it trusts, this crate
//! returns a typed [`Error`] (docs/research/24 §2.3).
// slates: the async read paths the left-out `ranges` used (`branch`, `scan`, `store`) are kept for slates-store's
// shard-hosted reads (A-767 C3); until it calls them they are unused, not wrong (vendor/mantle/README.md, local change 2).
#![cfg_attr(not(test), allow(dead_code))]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]

pub mod branch;
pub mod codec;
pub mod db;
pub mod error;
pub mod file;
pub mod fst;
pub mod kept;
pub mod memory;
pub mod memtable;
pub mod port;
// slates: `pub mod ranges;` (ranges hosted on hyper-rt's runtime) is left out: slates hosts each `ShardDb` on its own shard (vendor/mantle/README.md, local change 2).
pub mod records;
pub mod rows;
pub mod scan;
pub mod shard_db;
pub mod store;
pub mod table;
pub mod trunk;
pub mod util;
pub mod version;

pub use error::{Error, Malformed};
