//! The chunk store: one log-structured volume per device (docs/design/chunk-store.md).
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros,
        clippy::cast_possible_truncation
    )
)]

mod clean;
// Test devices shared with the integration tests (tests/common), which use what these do not.
#[cfg(test)]
#[allow(dead_code)]
#[path = "../tests/common/device.rs"]
mod device;
mod error;
pub mod frame;
pub mod index;
pub mod key;
pub mod layout;
mod log;
mod read;
mod reader;
pub mod record;
mod recover;
mod scrub;
pub mod superblock;
mod volume;
mod writer;

pub use clean::CleanReport;
pub use error::ChunkError;
pub use key::ChunkKey;
pub use layout::{Config, Limits, Reads};
pub use read::ReadStats;
pub use reader::{ReadAnswer, ReadId, Reader};
pub use recover::RecoveryReport;
pub use volume::{Answer, ChunkStat, ReadBuffers, Usage, Volume, formatted};
