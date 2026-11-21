//! `FSWritableFile` [R include/rocksdb/file_system.h:1217-1433] and the buffering
//! `WritableFileWriter` of `file/writable_file_writer.{h,cc}`, as far as the log writer uses
//! them: append, flush, sync, close and the sticky error (docs/research/24 §2.2).
//!
//! Dropped from RocksDB's writer: the rate limiter, the file checksum generator, listeners,
//! direct-I/O alignment and `RangeSync` (a write-back hint with no durability meaning, 24 §2.2).

use hyper_block::block::BlockFile;

use crate::error::Error;
use crate::file::{io_error, require_byte_addressed};

/// The file role under [`WritableFileWriter`]: RocksDB's `FSWritableFile`.
pub trait WritableFile {
    /// Appends `data` at the end of what was written.
    fn append(&mut self, data: &[u8]) -> Result<(), Error>;

    /// Hands appended data to the file (`FSWritableFile::Flush`); not a durability point.
    fn flush(&mut self) -> Result<(), Error>;

    /// Makes everything appended durable: the platform's full flush (CLAUDE.md §6).
    fn sync(&mut self) -> Result<(), Error>;

    fn close(&mut self) -> Result<(), Error>;
}

/// `writable_file_max_buffer_size`'s default, 1 MiB [R include/rocksdb/options.h:1292]: the
/// most a [`WritableFileWriter`] holds before writing to its file.
pub const MAX_BUFFER_SIZE: usize = 1 << 20;

/// `WritableFileWriter` [R file/writable_file_writer.h:40-373]: buffers appends up to
/// [`MAX_BUFFER_SIZE`] and passes them to the file on flush. After any failure it takes no more
/// writes (`seen_error`), since what reached the file is then unknown.
#[derive(Debug)]
pub struct WritableFileWriter<F: WritableFile> {
    file: F,
    buf: Vec<u8>,
    /// Bytes appended, buffered or not: `GetFileSize`.
    file_size: u64,
    seen_error: bool,
    closed: bool,
}

impl<F: WritableFile> WritableFileWriter<F> {
    pub fn new(file: F) -> Self {
        Self {
            file,
            buf: Vec::new(),
            file_size: 0,
            seen_error: false,
            closed: false,
        }
    }

    pub fn file(&self) -> &F {
        &self.file
    }

    pub fn file_mut(&mut self) -> &mut F {
        &mut self.file
    }

    pub fn file_size(&self) -> u64 {
        self.file_size
    }

    pub fn seen_error(&self) -> bool {
        self.seen_error
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn buffer_is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Refuses a write after an error or close: RocksDB's
    /// `GetWriterHasPreviousErrorStatus` [R file/writable_file_writer.h:338-341].
    fn check_open(&self, op: &'static str) -> Result<(), Error> {
        if self.seen_error {
            return Err(Error::Io {
                op,
                detail: "writer has previous error".to_owned(),
            });
        }
        if self.closed {
            return Err(Error::Io {
                op,
                detail: "writer is closed".to_owned(),
            });
        }
        Ok(())
    }

    /// Records a failure so no later write is attempted.
    fn fence<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if result.is_err() {
            self.seen_error = true;
        }
        result
    }

    /// `WritableFileWriter::Append` [R file/writable_file_writer.cc:64-249].
    pub fn append(&mut self, data: &[u8]) -> Result<(), Error> {
        self.check_open("append")?;
        let len = u64::try_from(data.len()).map_err(|_| Error::InvalidArgument {
            what: "append longer than a file offset",
        })?;
        let size = self
            .file_size
            .checked_add(len)
            .ok_or(Error::InvalidArgument {
                what: "append past the largest file offset",
            })?;
        if self
            .buf
            .len()
            .checked_add(data.len())
            .is_none_or(|total| total > MAX_BUFFER_SIZE)
        {
            self.flush_buffer()?;
        }
        if data.len() >= MAX_BUFFER_SIZE {
            let written = self.file.append(data);
            self.fence(written)?;
        } else {
            self.buf.extend_from_slice(data);
        }
        self.file_size = size;
        Ok(())
    }

    fn flush_buffer(&mut self) -> Result<(), Error> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let written = self.file.append(&self.buf);
        self.fence(written)?;
        self.buf.clear();
        Ok(())
    }

    /// `WritableFileWriter::Flush` [R file/writable_file_writer.cc:358-466]: the buffer to the
    /// file, then the file's own flush.
    pub fn flush(&mut self) -> Result<(), Error> {
        self.check_open("flush")?;
        self.flush_buffer()?;
        let flushed = self.file.flush();
        self.fence(flushed)
    }

    /// `WritableFileWriter::Sync` [R file/writable_file_writer.cc:468-490]: flush, then the
    /// platform's full flush. RocksDB's `use_fsync` choice has no counterpart: hyper-block's
    /// `sync_data` is always the full flush (docs/research/24 §2.2).
    pub fn sync(&mut self) -> Result<(), Error> {
        self.flush()?;
        let synced = self.file.sync();
        self.fence(synced)
    }

    /// `WritableFileWriter::Close` [R file/writable_file_writer.cc:251-356]: flushes and closes
    /// the file without syncing it, as RocksDB does.
    pub fn close(&mut self) -> Result<(), Error> {
        if self.closed {
            return Ok(());
        }
        self.check_open("close")?;
        let flushed = self.flush();
        let closed = self.file.close();
        self.closed = true;
        flushed?;
        self.fence(closed)
    }
}

/// A [`WritableFile`] over a hyper-block file, appending at a tracked offset: RocksDB's
/// `PosixWritableFile` (docs/research/24 §2.2). Appends go straight to the file, so the
/// [`WritableFileWriter`] above it is the only buffer.
#[derive(Debug)]
pub struct BlockWritableFile<B: BlockFile> {
    file: B,
    offset: u64,
}

impl<B: BlockFile> BlockWritableFile<B> {
    /// Writes from `offset`: the file's length to append to it (`ReopenWritableFile`), 0 for a
    /// new file or to overwrite a recycled one (`ReuseWritableFile`, whose old records are then
    /// told apart by their log number, docs/research/24 §1.5).
    pub fn new(file: B, offset: u64) -> Result<Self, Error> {
        require_byte_addressed(&file)?;
        Ok(Self { file, offset })
    }

    pub fn block_file(&self) -> &B {
        &self.file
    }
}

impl<B: BlockFile> WritableFile for BlockWritableFile<B> {
    fn append(&mut self, data: &[u8]) -> Result<(), Error> {
        let len = u64::try_from(data.len()).map_err(|_| Error::InvalidArgument {
            what: "append longer than a file offset",
        })?;
        let end = self.offset.checked_add(len).ok_or(Error::InvalidArgument {
            what: "append past the largest file offset",
        })?;
        self.file
            .write_all_at(data, self.offset)
            .map_err(|e| io_error("append", &e))?;
        self.offset = end;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Error> {
        Ok(())
    }

    fn sync(&mut self) -> Result<(), Error> {
        self.file.sync_data().map_err(|e| io_error("sync", &e))
    }

    fn close(&mut self) -> Result<(), Error> {
        Ok(())
    }
}
