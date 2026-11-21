//! `FSSequentialFile` [R include/rocksdb/file_system.h:869-930] and `SequentialFileReader`
//! [R file/sequence_file_reader.{h,cc}], as the log reader uses them (docs/research/24 §2.2).

use hyper_block::block::BlockFile;

use crate::error::Error;
use crate::file::{io_error, require_byte_addressed};

/// A read that failed after filling `filled` bytes of the caller's buffer. RocksDB's `Read`
/// returns the bytes and the failed status together, and the log reader counts both
/// [R db/log_reader.cc:436-462].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadFailure {
    pub filled: usize,
    pub error: Error,
}

/// The file role under the log reader: RocksDB's `FSSequentialFile` behind a
/// `SequentialFileReader`.
pub trait SequentialFile {
    /// Reads up to `scratch.len()` bytes into `scratch` from the current position and returns
    /// how many; fewer means the file ended there. A later call sees bytes appended since.
    fn read(&mut self, scratch: &mut [u8]) -> Result<usize, ReadFailure>;

    /// The name corruption reports carry (`file_name()`).
    fn file_name(&self) -> &str;
}

/// A [`SequentialFile`] over a hyper-block file, reading from a tracked offset.
#[derive(Debug)]
pub struct BlockSequentialFile<B: BlockFile> {
    file: B,
    offset: u64,
    name: String,
}

impl<B: BlockFile> BlockSequentialFile<B> {
    pub fn new(file: B, name: String) -> Result<Self, Error> {
        require_byte_addressed(&file)?;
        Ok(Self {
            file,
            offset: 0,
            name,
        })
    }
}

impl<B: BlockFile> SequentialFile for BlockSequentialFile<B> {
    fn read(&mut self, scratch: &mut [u8]) -> Result<usize, ReadFailure> {
        let fail = |error| ReadFailure { filled: 0, error };
        let len = self.file.len().map_err(|e| fail(io_error("stat", &e)))?;
        // A file cut shorter than the position has nothing more to read.
        let avail = usize::try_from(len.saturating_sub(self.offset)).unwrap_or(usize::MAX);
        let n = scratch.len().min(avail);
        let into = scratch.get_mut(..n).ok_or_else(|| {
            fail(Error::InvalidArgument {
                what: "read length past the buffer",
            })
        })?;
        self.file
            .read_exact_at(into, self.offset)
            .map_err(|e| fail(io_error("read", &e)))?;
        self.offset = u64::try_from(n)
            .ok()
            .and_then(|n| self.offset.checked_add(n))
            .ok_or_else(|| {
                fail(Error::InvalidArgument {
                    what: "read past the largest file offset",
                })
            })?;
        Ok(n)
    }

    fn file_name(&self) -> &str {
        &self.name
    }
}
