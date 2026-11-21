//! The log writer: `db/log_writer.{h,cc}` (docs/research/24 §1.5).
//!
//! A logical record is written as physical records that never cross a 32 KiB block: a record
//! that does not fit in what is left of the block is cut into First, Middle and Last fragments,
//! and a block tail too short for a header is zero-filled [R db/log_writer.cc:87-189]. With WAL
//! compression each record is one ZSTD frame and each compressed chunk becomes one or more
//! fragments [R :139-156]. Meta records (compression type, timestamp sizes, predecessor WAL)
//! are never fragmented: a block without room for one is zero-filled first [R :377-398].
//!
//! Two departures from the C++, neither visible in the bytes. A writer dropped without
//! [`Writer::write_buffer`] or [`Writer::close`] does not flush (RocksDB's destructor flushes
//! and discards the error, [R :35-43]); unflushed records are then lost as in a crash. And the
//! C++ `assert`s on a record's length and its block's room are typed errors
//! (docs/research/24 §5 R6).

use std::collections::HashMap;

use crate::db::log_format::{
    BLOCK_SIZE, FIRST_TYPE, FULL_TYPE, HEADER_SIZE, LAST_TYPE, MIDDLE_TYPE,
    PREDECESSOR_WAL_INFO_TYPE, PredecessorWalInfo, RECYCLABLE_FIRST_TYPE, RECYCLABLE_FULL_TYPE,
    RECYCLABLE_HEADER_SIZE, RECYCLABLE_LAST_TYPE, RECYCLABLE_MIDDLE_TYPE,
    RECYCLABLE_USER_DEFINED_TIMESTAMP_SIZE_TYPE, RECYCLE_PREDECESSOR_WAL_INFO_TYPE,
    SET_COMPRESSION_TYPE, USER_DEFINED_TIMESTAMP_SIZE_TYPE, encode_timestamp_size_record,
};
use crate::error::Error;
use crate::file::{WritableFile, WritableFileWriter};
use crate::util::coding::encode_fixed32;
use crate::util::compression::{
    CompressionType, StreamingCompress, encode_compression_type_record,
};
use crate::util::crc32c;

/// Zeros for a block's tail.
static ZEROS: [u8; BLOCK_SIZE] = [0; BLOCK_SIZE];

/// How a [`Writer`] writes: the arguments of RocksDB's constructor
/// [R db/log_writer.h:83-89].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriterOptions {
    /// The log's file number; a recyclable record stores its low 32 bits.
    pub log_number: u64,
    /// Write recyclable records, so a reader can tell this log's records from an older log's
    /// left in a reused file.
    pub recycle_log_files: bool,
    /// Leave records buffered until [`Writer::write_buffer`] instead of flushing each one.
    pub manual_flush: bool,
    /// `kNoCompression` or `kZSTD`; takes effect with [`Writer::add_compression_type_record`].
    pub compression_type: CompressionType,
    /// Record the predecessor WAL (`Options::track_and_verify_wals`).
    pub track_and_verify_wals: bool,
    /// Where in its 32 KiB block the file's end lies, when appending to an existing log
    /// (`VersionSet::ReopenManifestForAppend`); 0 for a new one.
    pub initial_block_offset: usize,
}

impl WriterOptions {
    /// A writer for log `log_number` with RocksDB's defaults for the rest.
    pub const fn new(log_number: u64, recycle_log_files: bool) -> Self {
        Self {
            log_number,
            recycle_log_files,
            manual_flush: false,
            compression_type: CompressionType::NoCompression,
            track_and_verify_wals: false,
            initial_block_offset: 0,
        }
    }
}

/// `log::Writer` [R db/log_writer.h:75-172].
#[derive(Debug)]
pub struct Writer<F: WritableFile> {
    /// `None` once closed.
    dest: Option<WritableFileWriter<F>>,
    /// Where the next record starts within its block.
    block_offset: usize,
    log_number: u64,
    recycle_log_files: bool,
    header_size: usize,
    manual_flush: bool,
    compression_type: CompressionType,
    compress: Option<StreamingCompress>,
    /// One compressed chunk; `kBlockSize - header_size` bytes once compression starts.
    compressed_buffer: Vec<u8>,
    /// The timestamp sizes recorded in this log, one per column family the caller names, so
    /// no larger than the caller's set of column families.
    recorded_cf_to_ts_sz: HashMap<u32, usize>,
    track_and_verify_wals: bool,
    last_seqno_recorded: u64,
}

fn closed() -> Error {
    Error::Io {
        op: "log write",
        detail: "the log writer is closed".to_owned(),
    }
}

fn bad_offset() -> Error {
    Error::InvalidArgument {
        what: "log block offset past the block",
    }
}

impl<F: WritableFile> Writer<F> {
    /// `Writer::Writer` [R db/log_writer.cc:20-37].
    pub fn new(dest: WritableFileWriter<F>, options: WriterOptions) -> Result<Self, Error> {
        if options.initial_block_offset >= BLOCK_SIZE {
            return Err(bad_offset());
        }
        Ok(Self {
            dest: Some(dest),
            block_offset: options.initial_block_offset,
            log_number: options.log_number,
            recycle_log_files: options.recycle_log_files,
            header_size: if options.recycle_log_files {
                RECYCLABLE_HEADER_SIZE
            } else {
                HEADER_SIZE
            },
            manual_flush: options.manual_flush,
            compression_type: options.compression_type,
            compress: None,
            compressed_buffer: Vec::new(),
            recorded_cf_to_ts_sz: HashMap::new(),
            track_and_verify_wals: options.track_and_verify_wals,
            last_seqno_recorded: 0,
        })
    }

    pub fn file(&self) -> Option<&WritableFileWriter<F>> {
        self.dest.as_ref()
    }

    pub fn file_mut(&mut self) -> Option<&mut WritableFileWriter<F>> {
        self.dest.as_mut()
    }

    pub fn log_number(&self) -> u64 {
        self.log_number
    }

    /// `TEST_block_offset`.
    pub fn block_offset(&self) -> usize {
        self.block_offset
    }

    /// `GetLastSeqnoRecorded`.
    pub fn last_seqno_recorded(&self) -> u64 {
        self.last_seqno_recorded
    }

    fn dest(&mut self) -> Result<&mut WritableFileWriter<F>, Error> {
        self.dest.as_mut().ok_or_else(closed)
    }

    /// `MaybeHandleSeenFileWriterError` [R db/log_writer.cc:362-375].
    fn check_seen_error(&mut self) -> Result<(), Error> {
        if self.dest()?.seen_error() {
            return Err(Error::Io {
                op: "log write",
                detail: "Seen error. Skip writing buffer.".to_owned(),
            });
        }
        Ok(())
    }

    /// `WriteBuffer` [R db/log_writer.cc:45-56]: flushes buffered records to the file.
    pub fn write_buffer(&mut self) -> Result<(), Error> {
        self.check_seen_error()?;
        self.dest()?.flush()
    }

    /// `Close` [R db/log_writer.cc:58-67].
    pub fn close(&mut self) -> Result<(), Error> {
        match self.dest.take() {
            Some(mut dest) => dest.close(),
            None => Ok(()),
        }
    }

    /// `PublishIfClosed` [R db/log_writer.cc:69-76]: forgets the file if it was closed
    /// through [`Self::file_mut`].
    pub fn publish_if_closed(&mut self) -> bool {
        if self
            .dest
            .as_ref()
            .is_some_and(WritableFileWriter::is_closed)
        {
            self.dest = None;
            return true;
        }
        false
    }

    pub fn buffer_is_empty(&self) -> bool {
        self.dest
            .as_ref()
            .is_none_or(WritableFileWriter::buffer_is_empty)
    }

    /// `AddRecord` [R db/log_writer.cc:78-189]: writes one logical record, the payload of a
    /// WAL write batch or a MANIFEST edit, and flushes it unless `manual_flush`.
    pub fn add_record(&mut self, slice: &[u8], seqno: u64) -> Result<(), Error> {
        let mut buffer = std::mem::take(&mut self.compressed_buffer);
        let written = self.write_fragments(slice, &mut buffer);
        self.compressed_buffer = buffer;
        written?;
        if !self.manual_flush {
            self.dest()?.flush()?;
        }
        self.last_seqno_recorded = self.last_seqno_recorded.max(seqno);
        Ok(())
    }

    fn write_fragments(&mut self, slice: &[u8], compressed: &mut [u8]) -> Result<(), Error> {
        self.check_seen_error()?;
        // Where the next fragment's bytes come from: `slice`, or with compression the chunk
        // last written to `compressed`.
        let mut from_compressed = false;
        let mut pos = 0usize;
        let mut left = slice.len();
        // Even an empty record is one (zero-length) physical record.
        let mut begin = true;
        let mut compress_remaining = 0usize;
        let mut compress_start = false;
        if let Some(c) = self.compress.as_mut() {
            c.reset()?;
            compress_start = true;
        }
        loop {
            let leftover = BLOCK_SIZE
                .checked_sub(self.block_offset)
                .ok_or_else(bad_offset)?;
            if leftover < self.header_size {
                // Switch to a new block, zero-filling the trailer.
                if leftover > 0 {
                    let zeros = ZEROS.get(..leftover).ok_or_else(bad_offset)?;
                    self.dest()?.append(zeros)?;
                }
                self.block_offset = 0;
            }
            // Invariant: a block never has less than a header left.
            let avail = BLOCK_SIZE
                .checked_sub(self.block_offset)
                .and_then(|n| n.checked_sub(self.header_size))
                .ok_or_else(bad_offset)?;

            // Compress at the start and after each compressed chunk is written out.
            if let Some(c) = self.compress.as_mut()
                && (compress_start || left == 0)
            {
                let out = c.compress(slice, compressed).map_err(|e| Error::Io {
                    op: "WAL compression",
                    detail: format!("Unexpected WAL compression error: {e}"),
                })?;
                compress_remaining = out.remaining;
                left = out.output_len;
                if left == 0 && !compress_start {
                    // Nothing left to compress.
                    break;
                }
                compress_start = false;
                from_compressed = true;
                pos = 0;
            }

            let fragment_length = left.min(avail);
            let end = left == fragment_length && compress_remaining == 0;
            let record_type = match (begin, end, self.recycle_log_files) {
                (true, true, false) => FULL_TYPE,
                (true, true, true) => RECYCLABLE_FULL_TYPE,
                (true, false, false) => FIRST_TYPE,
                (true, false, true) => RECYCLABLE_FIRST_TYPE,
                (false, true, false) => LAST_TYPE,
                (false, true, true) => RECYCLABLE_LAST_TYPE,
                (false, false, false) => MIDDLE_TYPE,
                (false, false, true) => RECYCLABLE_MIDDLE_TYPE,
            };
            let stop = pos.checked_add(fragment_length).ok_or_else(bad_offset)?;
            let source: &[u8] = if from_compressed { compressed } else { slice };
            let fragment = source.get(pos..stop).ok_or(Error::InvalidArgument {
                what: "log fragment past its record",
            })?;
            self.emit_physical_record(record_type, fragment)?;
            pos = stop;
            left = left.checked_sub(fragment_length).ok_or_else(bad_offset)?;
            begin = false;
            if left == 0 && compress_remaining == 0 {
                break;
            }
        }
        Ok(())
    }

    /// `AddCompressionTypeRecord` [R db/log_writer.cc:191-234]: with compression configured,
    /// writes the `kSetCompressionType` record that must open the log and starts compressing
    /// every record after it.
    pub fn add_compression_type_record(&mut self) -> Result<(), Error> {
        // Should be the first record.
        if self.block_offset != 0 {
            return Err(Error::InvalidArgument {
                what: "the compression type record must be a log's first record",
            });
        }
        if self.compression_type == CompressionType::NoCompression {
            return Ok(());
        }
        self.check_seen_error()?;
        let mut encoded = Vec::new();
        encode_compression_type_record(&mut encoded, self.compression_type);
        if let Err(e) = self.emit_physical_record(SET_COMPRESSION_TYPE, &encoded) {
            // Disable compression if the record could not be added.
            self.compression_type = CompressionType::NoCompression;
            return Err(e);
        }
        let flushed = if self.manual_flush {
            Ok(())
        } else {
            self.dest()?.flush()
        };
        let max_output_len = BLOCK_SIZE
            .checked_sub(self.header_size)
            .ok_or_else(bad_offset)?;
        self.compress = Some(StreamingCompress::zstd(max_output_len)?);
        self.compressed_buffer = vec![0; max_output_len];
        flushed
    }

    /// `MaybeAddPredecessorWALInfo` [R db/log_writer.cc:236-272]: with
    /// `track_and_verify_wals`, records the WAL before this one.
    pub fn maybe_add_predecessor_wal_info(
        &mut self,
        info: Option<&PredecessorWalInfo>,
    ) -> Result<(), Error> {
        self.check_seen_error()?;
        let Some(info) = info.filter(|_| self.track_and_verify_wals) else {
            return Ok(());
        };
        let mut encoded = Vec::new();
        info.encode_to(&mut encoded);
        self.maybe_switch_to_new_block(encoded.len())?;
        let record_type = if self.recycle_log_files {
            RECYCLE_PREDECESSOR_WAL_INFO_TYPE
        } else {
            PREDECESSOR_WAL_INFO_TYPE
        };
        self.emit_physical_record(record_type, &encoded)?;
        if !self.manual_flush {
            self.dest()?.flush()?;
        }
        Ok(())
    }

    /// `MaybeAddUserDefinedTimestampSizeRecord` [R db/log_writer.cc:274-305]: records the
    /// non-zero timestamp sizes of column families this log has not recorded yet; they apply
    /// to every record after. RocksDB iterates a hash map; here the caller's order is the
    /// order written.
    pub fn maybe_add_user_defined_timestamp_size_record(
        &mut self,
        cf_to_ts_sz: &[(u32, usize)],
    ) -> Result<(), Error> {
        let mut to_record = Vec::new();
        for &(cf_id, ts_sz) in cf_to_ts_sz {
            match self.recorded_cf_to_ts_sz.get(&cf_id) {
                // A column family's timestamp size does not change while the DB runs.
                Some(&recorded) if recorded != ts_sz => {
                    return Err(Error::InvalidArgument {
                        what: "a column family's timestamp size changed within one log",
                    });
                }
                Some(_) => {}
                None if ts_sz != 0 => {
                    to_record.push((cf_id, ts_sz));
                    self.recorded_cf_to_ts_sz.insert(cf_id, ts_sz);
                }
                None => {}
            }
        }
        if to_record.is_empty() {
            return Ok(());
        }
        let mut encoded = Vec::new();
        encode_timestamp_size_record(&mut encoded, &to_record)?;
        let record_type = if self.recycle_log_files {
            RECYCLABLE_USER_DEFINED_TIMESTAMP_SIZE_TYPE
        } else {
            USER_DEFINED_TIMESTAMP_SIZE_TYPE
        };
        self.maybe_switch_to_new_block(encoded.len())?;
        self.emit_physical_record(record_type, &encoded)
    }

    /// `MaybeSwitchToNewBlock` [R db/log_writer.cc:377-398]: zero-fills the block's tail if a
    /// meta record of `len` bytes does not fit.
    fn maybe_switch_to_new_block(&mut self, len: usize) -> Result<(), Error> {
        let leftover = BLOCK_SIZE
            .checked_sub(self.block_offset)
            .ok_or_else(bad_offset)?;
        let needed = self.header_size.checked_add(len).ok_or_else(bad_offset)?;
        if leftover < needed {
            let zeros = ZEROS.get(..leftover).ok_or_else(bad_offset)?;
            self.dest()?.append(zeros)?;
            self.block_offset = 0;
        }
        Ok(())
    }

    /// `EmitPhysicalRecord` [R db/log_writer.cc:309-360]: one header and its payload. The CRC
    /// covers the type, a recyclable record's log number, and the payload.
    fn emit_physical_record(&mut self, record_type: u8, payload: &[u8]) -> Result<(), Error> {
        let n = payload.len();
        let len = u16::try_from(n).map_err(|_| Error::InvalidArgument {
            what: "log record fragment longer than 16 bits",
        })?;
        let legacy = record_type < RECYCLABLE_FULL_TYPE
            || matches!(
                record_type,
                SET_COMPRESSION_TYPE | PREDECESSOR_WAL_INFO_TYPE | USER_DEFINED_TIMESTAMP_SIZE_TYPE
            );
        // Only the low 32 bits of the log number: a record from a log recycled four billion
        // logs ago would be mistaken for this log's [R :334-339].
        let [l0, l1, l2, l3, ..] = self.log_number.to_le_bytes();
        let log_number = [l0, l1, l2, l3];
        let mut crc = crc32c::value(&[record_type]);
        let header_size = if legacy {
            HEADER_SIZE
        } else {
            crc = crc32c::extend(crc, &log_number);
            RECYCLABLE_HEADER_SIZE
        };
        let end = self
            .block_offset
            .checked_add(header_size)
            .and_then(|o| o.checked_add(n))
            .filter(|&e| e <= BLOCK_SIZE)
            .ok_or(Error::InvalidArgument {
                what: "log record past the end of its block",
            })?;
        let [c0, c1, c2, c3] = encode_fixed32(crc32c::mask(crc32c::extend(crc, payload)));
        let [n0, n1] = len.to_le_bytes();
        let header = [c0, c1, c2, c3, n0, n1, record_type, l0, l1, l2, l3];
        let header = header.get(..header_size).ok_or_else(bad_offset)?;
        let dest = self.dest()?;
        let appended = dest.append(header).and_then(|()| dest.append(payload));
        self.block_offset = end;
        appended
    }
}
