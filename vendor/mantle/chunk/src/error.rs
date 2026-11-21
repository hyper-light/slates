use hyper_block::DiskError;

use crate::key::ChunkKey;

#[derive(Debug, thiserror::Error)]
pub enum ChunkError {
    /// The device failed a write or flush. The volume is fenced: nothing more is written
    /// until it is reopened and recovered (Rebello et al., ATC 2020).
    #[error("device error; volume fenced: {0}")]
    Device(#[source] DiskError),
    #[error("volume is fenced after an earlier device error")]
    Fenced,
    /// Bytes read back failed a checksum, identity or incarnation check, or the device
    /// returned an error reading them. The caller reads another copy.
    #[error("chunk {key} failed verification: {detail}")]
    Corrupt { key: ChunkKey, detail: String },
    #[error("chunk {0} not found")]
    NotFound(ChunkKey),
    #[error("chunk {0} already holds different bytes")]
    Exists(ChunkKey),
    #[error("chunk {0} is sealed")]
    Sealed(ChunkKey),
    #[error("append to {key} at offset {offset}, but the chunk ends at {end}")]
    Gap {
        key: ChunkKey,
        offset: u64,
        end: u64,
    },
    #[error("append to {key} at offset {offset} differs from the bytes already there")]
    Conflict { key: ChunkKey, offset: u64 },
    #[error("{len} bytes exceed the largest fragment, {max}")]
    TooLarge { len: u64, max: u64 },
    #[error("range {offset}+{len} is outside chunk {key} of {chunk_len} bytes")]
    Range {
        key: ChunkKey,
        offset: u64,
        len: u64,
        chunk_len: u64,
    },
    #[error("chunk {0} has the most fragments a chunk may have")]
    TooManyFragments(ChunkKey),
    #[error("volume is full")]
    Full,
    /// The writer's queue holds all it takes (`Limits::queue_requests`, `queue_bytes`), or
    /// as many reads wait for a turn at the device as may (`Reads::waiting`): nothing was
    /// written or read, and the caller retries later or elsewhere.
    #[error("volume is busy: its queue is full")]
    Busy,
    #[error("volume is closed")]
    Closed,
    /// The bytes at the start of the file are not a mantle volume this code can open.
    #[error("not an openable volume: {0}")]
    Format(String),
    /// A valid index frame follows an invalid one: acknowledged state was damaged, which a
    /// crash cannot cause (Alagappan et al., FAST 2018, §3.3.3).
    #[error("index log corrupt at lsn {lsn}")]
    CorruptLog { lsn: u64 },
    #[error("invalid configuration: {0}")]
    Config(String),
    /// The bytes submitted do not match the CRC-32C their sender computed: they changed on
    /// the way, and nothing was written.
    #[error("chunk {key}: data has CRC-32C {actual:#010x}, sender computed {expected:#010x}")]
    Checksum {
        key: ChunkKey,
        expected: u32,
        actual: u32,
    },
    /// The writer's own bookkeeping disagreed with itself; the volume is fenced.
    #[error("internal inconsistency: {0}")]
    Internal(&'static str),
}
