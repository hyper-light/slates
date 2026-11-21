//! RocksDB's `file/` directory and the file classes of `include/rocksdb/file_system.h`, onto
//! hyper-block, the block layer mantle shares with hyper-raft (docs/research/24 §2.2).
//!
//! RocksDB writes through `FSWritableFile` wrapped in a buffering `WritableFileWriter`, and reads
//! logs through `FSSequentialFile` wrapped in `SequentialFileReader`. Here the file roles are
//! the traits [`WritableFile`] and [`SequentialFile`], implemented on a hyper-block
//! [`BlockFile`](hyper_block::block::BlockFile), so a test puts `SimFile` where the device file
//! goes (docs/research/24 §5 R16).

pub mod sequence_file_reader;
pub mod writable_file_writer;

pub use sequence_file_reader::{BlockSequentialFile, ReadFailure, SequentialFile};
pub use writable_file_writer::{BlockWritableFile, WritableFile, WritableFileWriter};

use hyper_block::DiskError;
use hyper_block::block::BlockFile;

use crate::error::Error;

pub(crate) fn io_error(op: &'static str, e: &DiskError) -> Error {
    Error::Io {
        op,
        detail: e.to_string(),
    }
}

/// The log's files are byte-addressed: RocksDB writes and reads WALs and MANIFESTs through the
/// page cache (`use_direct_io_for_flush_and_compaction` and `use_direct_reads` do not apply to
/// them [R include/rocksdb/options.h]), so an append or read of any length at any offset is
/// legal. A direct-I/O file would need its padded tail cut on close, which `BlockFile` cannot
/// do yet (docs/research/24 §2.2, the `Close` row), so one is refused.
pub(crate) fn require_byte_addressed(file: &impl BlockFile) -> Result<(), Error> {
    let align = file.alignment().get();
    if align == 1 {
        return Ok(());
    }
    Err(Error::Unsupported {
        feature: "log file alignment above one byte (direct I/O)",
        value: u64::try_from(align).unwrap_or(u64::MAX),
    })
}
