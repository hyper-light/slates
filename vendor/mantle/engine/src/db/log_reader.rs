//! The log reader: `db/log_reader.{h,cc}` (docs/research/24 §1.5).
//!
//! [`Reader`] reads a finished log, as recovery and `ldb dump_wal` do; [`FragmentBufferedReader`]
//! tails a log another writer is still appending to, keeping a partial record across calls. Both
//! read 32 KiB blocks, check each physical record's CRC, reassemble fragments, decompress a ZSTD
//! WAL fragment by fragment, and report what they drop to a [`Reporter`] with RocksDB's reasons
//! and byte counts, which depend on the [`WalRecoveryMode`] [R db/log_reader.cc:75-354].
//!
//! Departures from the C++, each where RocksDB's behaviour is a defect and never where its
//! reading of a well-formed log differs:
//! - A recyclable record in a log whose earlier records were not recyclable: RocksDB returns
//!   `kBadRecord` without consuming it, so `ReadRecord` loops on the same header forever
//!   [R :567-571, :289-296]. Here the rest of the block is dropped and reported
//!   ([`LogCorruption::RecyclableInLegacyLog`]), as a checksum mismatch drops it.
//! - A record whose type byte is 132–137 collides with RocksDB's internal outcome codes (kEof,
//!   kBadRecord, ...) and is acted on as that outcome [R db/log_reader.h:193-208]; here it is an
//!   unknown type with the safe-ignore bit, skipped as `kRecordTypeSafeIgnoreMask` intends.
//! - RocksDB can return an XXH3 of each record, computed in a streamed state that is not reset
//!   when a partial record is abandoned [R :101-171]; a caller here hashes the record it gets
//!   (`util::xxhash::xxh3_64bits`).
//! - [`FragmentBufferedReader`] stops at an old record of a recycled log instead of looping on
//!   it [R :1004-1008], so a later call reads it once the writer has overwritten it; and after
//!   reading a new block to complete a header it parses the new block rather than the replaced
//!   one [R :990-1024].

use std::collections::HashMap;
use std::fmt;

use crate::db::log_format::{
    BLOCK_SIZE, FIRST_TYPE, FULL_TYPE, HEADER_SIZE, LAST_TYPE, MIDDLE_TYPE,
    PREDECESSOR_WAL_INFO_TYPE, PredecessorWalInfo, RECORD_TYPE_SAFE_IGNORE_MASK,
    RECYCLABLE_FIRST_TYPE, RECYCLABLE_FULL_TYPE, RECYCLABLE_HEADER_SIZE, RECYCLABLE_LAST_TYPE,
    RECYCLABLE_MIDDLE_TYPE, RECYCLABLE_USER_DEFINED_TIMESTAMP_SIZE_TYPE,
    RECYCLE_PREDECESSOR_WAL_INFO_TYPE, SET_COMPRESSION_TYPE, USER_DEFINED_TIMESTAMP_SIZE_TYPE,
    WalRecoveryMode, ZERO_TYPE, decode_timestamp_size_record, is_meta_type, is_recyclable_type,
};
use crate::error::Error;
use crate::file::{ReadFailure, SequentialFile};
use crate::util::coding::decode_fixed32;
use crate::util::compression::{
    CompressionType, StreamingUncompress, decode_compression_type_record,
};
use crate::util::crc32c;

/// Why a reader dropped bytes: the text of RocksDB's `Status::Corruption` for each case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogCorruption {
    TruncatedHeader,
    TruncatedRecordBody,
    BadRecordLength,
    ChecksumMismatch,
    ErrorInMiddleOfRecord,
    EofInTrailingData,
    OldRecordInTrailingData,
    /// A Full record after an unfinished First.
    PartialRecordWithoutEnd1,
    /// A First record after an unfinished First.
    PartialRecordWithoutEnd2,
    /// A Middle record with no First.
    MissingStart1,
    /// A Last record with no First.
    MissingStart2,
    MultipleSetCompressionType,
    SetCompressionTypeNotFirst,
    UndecodableSetCompressionType,
    UndecodablePredecessorWalInfo,
    TimestampSizeInterspersed,
    UndecodableTimestampSize,
    ZeroTimestampSize,
    TimestampSizeUpdate,
    UnknownRecordType(u8),
    MissingWal {
        log_number: u64,
    },
    MismatchedPredecessorLogNumber {
        file_name: String,
        recorded: u64,
        observed: u64,
    },
    MismatchedPredecessorLastSeqno {
        log_number: u64,
        recorded: u64,
        observed: u64,
    },
    MismatchedPredecessorSize {
        log_number: u64,
        recorded: u64,
        observed: u64,
    },
    /// The port's own: see the module comment.
    RecyclableInLegacyLog,
}

impl fmt::Display for LogCorruption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TruncatedHeader => f.write_str("truncated header"),
            Self::TruncatedRecordBody => f.write_str("truncated record body"),
            Self::BadRecordLength => f.write_str("bad record length"),
            Self::ChecksumMismatch => f.write_str("checksum mismatch"),
            Self::ErrorInMiddleOfRecord => f.write_str("error in middle of record"),
            Self::EofInTrailingData => {
                f.write_str("error reading trailing data due to encountering EOF")
            }
            Self::OldRecordInTrailingData => {
                f.write_str("error reading trailing data due to encountering old record")
            }
            Self::PartialRecordWithoutEnd1 => f.write_str("partial record without end(1)"),
            Self::PartialRecordWithoutEnd2 => f.write_str("partial record without end(2)"),
            Self::MissingStart1 => f.write_str("missing start of fragmented record(1)"),
            Self::MissingStart2 => f.write_str("missing start of fragmented record(2)"),
            Self::MultipleSetCompressionType => {
                f.write_str("read multiple SetCompressionType records")
            }
            Self::SetCompressionTypeNotFirst => {
                f.write_str("SetCompressionType not the first record")
            }
            Self::UndecodableSetCompressionType => {
                f.write_str("could not decode SetCompressionType record")
            }
            Self::UndecodablePredecessorWalInfo => {
                f.write_str("could not decode PredecessorWALInfoType record")
            }
            Self::TimestampSizeInterspersed => {
                f.write_str("user-defined timestamp size record interspersed partial record")
            }
            Self::UndecodableTimestampSize => {
                f.write_str("could not decode user-defined timestamp size record")
            }
            Self::ZeroTimestampSize => {
                f.write_str("User-defined timestamp size record contains zero timestamp size.")
            }
            Self::TimestampSizeUpdate => f.write_str(
                "User-defined timestamp size record contains update to recorded column family.",
            ),
            Self::UnknownRecordType(t) => write!(f, "unknown record type {t}"),
            Self::MissingWal { log_number } => write!(f, "Missing WAL of log number {log_number}"),
            Self::MismatchedPredecessorLogNumber {
                file_name,
                recorded,
                observed,
            } => write!(
                f,
                "Mismatched predecessor log number of WAL file {file_name} Recorded \
                 {recorded}. Observed {observed}"
            ),
            Self::MismatchedPredecessorLastSeqno {
                log_number,
                recorded,
                observed,
            } => write!(
                f,
                "Mismatched last sequence number recorded in the WAL of log number \
                 {log_number}. Recorded {recorded}. Observed {observed}. (Last sequence number \
                 equal to 0 indicates no WAL records)"
            ),
            Self::MismatchedPredecessorSize {
                log_number,
                recorded,
                observed,
            } => write!(
                f,
                "Mismatched size of the WAL of log number {log_number}. Recorded {recorded} \
                 bytes. Observed {observed} bytes."
            ),
            Self::RecyclableInLegacyLog => {
                f.write_str("recyclable record in a log that is not recycled")
            }
        }
    }
}

/// What a [`Reporter`] is told: a corruption, or the file's failed read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropReason {
    Corruption(LogCorruption),
    Read(Error),
}

/// As RocksDB's `Status::ToString`: "Corruption: <reason>" for a corruption.
impl fmt::Display for DropReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Corruption(c) => write!(f, "Corruption: {c}"),
            Self::Read(e) => write!(f, "{e}"),
        }
    }
}

/// `log::Reader::Reporter` [R db/log_reader.h:43-54].
pub trait Reporter {
    /// `bytes` (approximately) were dropped for `reason`; `log_number` names the WAL a
    /// predecessor check concerns (RocksDB passes `kMaxSequenceNumber` otherwise).
    fn corruption(&mut self, bytes: usize, reason: &DropReason, log_number: Option<u64>);

    /// A point-in-time recovery reached a record of an older log in a recycled file.
    fn old_log_record(&mut self, _bytes: usize) {}
}

/// The `track_and_verify_wals` inputs of RocksDB's reader [R db/log_reader.h:63-68].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalVerification {
    pub track_and_verify_wals: bool,
    pub stop_replay_for_corruption: bool,
    pub min_wal_number_to_keep: u64,
    /// The WAL recovery read before this one, if any.
    pub observed_predecessor_wal_info: Option<PredecessorWalInfo>,
}

impl Default for WalVerification {
    fn default() -> Self {
        Self {
            track_and_verify_wals: false,
            stop_replay_for_corruption: false,
            min_wal_number_to_keep: u64::MAX,
            observed_predecessor_wal_info: None,
        }
    }
}

/// Where a physical record's payload is.
#[derive(Debug, Clone, Copy)]
enum Loc {
    Block { start: usize, end: usize },
    Uncompressed,
}

/// `ReadPhysicalRecord`'s result: a record type, or one of RocksDB's extended codes
/// [R db/log_reader.h:193-208].
#[derive(Debug, Clone, Copy)]
enum Physical {
    Record { record_type: u8, loc: Loc },
    Eof,
    BadRecord,
    BadHeader,
    OldRecord,
    BadRecordLen,
    BadRecordChecksum,
    RecyclableInLegacyLog,
}

/// The state both readers share: RocksDB's `Reader` fields [R db/log_reader.h:130-190].
struct Core<F, R> {
    file: F,
    reporter: R,
    checksum: bool,
    /// One block; `buffer_` is `backing[start..end]`, and `end` is where the bytes read into
    /// the current block end.
    backing: Vec<u8>,
    start: usize,
    end: usize,
    /// The last read returned less than asked.
    eof: bool,
    read_error: bool,
    /// Where in the block the file ended when `eof` was set.
    eof_offset: usize,
    last_record_offset: u64,
    /// The file offset just past `backing[end]`.
    end_of_buffer_offset: u64,
    log_number: u64,
    verification: WalVerification,
    recycled: bool,
    first_record_read: bool,
    compression_type: CompressionType,
    compression_type_record_read: bool,
    uncompress: Option<StreamingUncompress>,
    uncompressed_buffer: Vec<u8>,
    uncompressed_record: Vec<u8>,
    /// One entry per column family a timestamp-size record names; each costs 6 bytes of the
    /// file, so the map is bounded by the file.
    recorded_cf_to_ts_sz: HashMap<u32, usize>,
}

fn range_error() -> Error {
    Error::InvalidArgument {
        what: "log reader position outside its block",
    }
}

impl<F: SequentialFile, R: Reporter> Core<F, R> {
    fn new(
        file: F,
        reporter: R,
        checksum: bool,
        log_number: u64,
        verification: WalVerification,
    ) -> Self {
        Self {
            file,
            reporter,
            checksum,
            backing: vec![0; BLOCK_SIZE],
            start: 0,
            end: 0,
            eof: false,
            read_error: false,
            eof_offset: 0,
            last_record_offset: 0,
            end_of_buffer_offset: 0,
            log_number,
            verification,
            recycled: false,
            first_record_read: false,
            compression_type: CompressionType::NoCompression,
            compression_type_record_read: false,
            uncompress: None,
            uncompressed_buffer: Vec::new(),
            uncompressed_record: Vec::new(),
            recorded_cf_to_ts_sz: HashMap::new(),
        }
    }

    fn buffer_len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    fn clear_buffer(&mut self) {
        self.start = self.end;
    }

    fn buffer(&self) -> &[u8] {
        self.backing.get(self.start..self.end).unwrap_or_default()
    }

    fn fragment(&self, loc: Loc) -> &[u8] {
        match loc {
            Loc::Block { start, end } => self.backing.get(start..end).unwrap_or_default(),
            Loc::Uncompressed => &self.uncompressed_record,
        }
    }

    fn add_offset(&mut self, n: usize) {
        // A block is 32 KiB and a file offset 64 bits: the sum cannot wrap before the file
        // system's own limits do.
        self.end_of_buffer_offset = self
            .end_of_buffer_offset
            .saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
    }

    /// The file offset of the buffer's first byte.
    fn buffer_offset(&self) -> u64 {
        self.end_of_buffer_offset
            .saturating_sub(u64::try_from(self.buffer_len()).unwrap_or(0))
    }

    fn report(&mut self, bytes: usize, c: LogCorruption) {
        self.reporter
            .corruption(bytes, &DropReason::Corruption(c), None);
    }

    fn report_for_log(&mut self, bytes: usize, c: LogCorruption, log_number: u64) {
        self.reporter
            .corruption(bytes, &DropReason::Corruption(c), Some(log_number));
    }

    /// Reads up to `into.len()` bytes into `backing[at..]`: the count, or the count and the
    /// failure.
    fn read_into(&mut self, at: usize, len: usize) -> Result<usize, ReadFailure> {
        let stop = at.checked_add(len).filter(|&s| s <= BLOCK_SIZE);
        let Some(into) = stop.and_then(|s| self.backing.get_mut(at..s)) else {
            return Err(ReadFailure {
                filled: 0,
                error: range_error(),
            });
        };
        self.file.read(into)
    }

    /// Reads the next whole block: the non-EOF half of `ReadMore` and `TryReadMore`
    /// [R db/log_reader.cc:469-491]. False after a read error, reported.
    fn read_block(&mut self) -> bool {
        self.start = 0;
        self.end = 0;
        match self.read_into(0, BLOCK_SIZE) {
            Ok(n) => {
                self.end = n;
                self.add_offset(n);
                if n < BLOCK_SIZE {
                    self.eof = true;
                    self.eof_offset = n;
                }
                true
            }
            Err(failure) => {
                self.add_offset(failure.filled);
                self.reporter
                    .corruption(BLOCK_SIZE, &DropReason::Read(failure.error), None);
                self.read_error = true;
                false
            }
        }
    }

    /// `ReadMore` [R db/log_reader.cc:469-508]: `Err` is the extended code to return.
    fn read_more(&mut self, drop_size: &mut usize) -> Result<(), Physical> {
        if !self.eof && !self.read_error {
            // The last read was a full block, so what is left is a trailer to skip.
            return if self.read_block() {
                Ok(())
            } else {
                Err(Physical::Eof)
            };
        }
        // A non-empty buffer here is a header the writer did not finish: not an error
        // unless the recovery mode says so.
        let len = self.buffer_len();
        self.clear_buffer();
        if len > 0 {
            *drop_size = len;
            return Err(Physical::BadHeader);
        }
        Err(Physical::Eof)
    }

    /// `UnmarkEOFInternal` [R db/log_reader.cc:382-433]: reads the rest of the block the file
    /// ended in, keeping the unread bytes before it.
    fn unmark_eof_internal(&mut self) {
        let remaining = BLOCK_SIZE.saturating_sub(self.eof_offset);
        let at = self.eof_offset;
        match self.read_into(at, remaining) {
            Ok(added) => {
                self.add_offset(added);
                self.end = at.saturating_add(added);
                if added < remaining {
                    self.eof = true;
                    self.eof_offset = self.end;
                } else {
                    self.eof_offset = 0;
                }
            }
            Err(failure) => {
                self.add_offset(failure.filled);
                if failure.filled > 0 {
                    self.reporter.corruption(
                        failure.filled,
                        &DropReason::Read(failure.error),
                        None,
                    );
                }
                self.read_error = true;
            }
        }
    }

    /// The header at the buffer's start: (length, type).
    fn header(&self) -> (usize, u8) {
        let b = self.buffer();
        let len = usize::from(b.get(4).copied().unwrap_or(0))
            | usize::from(b.get(5).copied().unwrap_or(0)) << 8;
        (len, b.get(6).copied().unwrap_or(0))
    }

    /// The record whose header and payload fill `header_size + length` bytes at the buffer's
    /// start, after its log number was checked: the zero-type, checksum and decompression
    /// steps common to both readers [R db/log_reader.cc:611-693].
    fn finish_physical(
        &mut self,
        record_type: u8,
        header_size: usize,
        length: usize,
        drop_size: &mut usize,
    ) -> Physical {
        if record_type == ZERO_TYPE && length == 0 {
            // Preallocated space, which RocksDB's writers no longer produce: skipped
            // without a report.
            self.clear_buffer();
            return Physical::BadRecord;
        }
        let total = header_size.saturating_add(length);
        if self.checksum {
            let buffer = self.buffer();
            let expected = decode_fixed32(buffer).map(crc32c::unmask).ok();
            let actual = buffer.get(6..total).map(crc32c::value);
            if expected.is_none() || expected != actual {
                // Drop the rest of the block: the length itself may be corrupt, and trusting
                // it could find a fragment of a real record that looks valid.
                *drop_size = self.buffer_len();
                self.clear_buffer();
                return Physical::BadRecordChecksum;
            }
        }
        let payload_start = self.start.saturating_add(header_size);
        let payload_end = self.start.saturating_add(total);
        self.start = payload_end.min(self.end);
        let loc = Loc::Block {
            start: payload_start,
            end: payload_end,
        };
        if is_meta_type(record_type) {
            return Physical::Record { record_type, loc };
        }
        let Some(uncompress) = self.uncompress.as_mut() else {
            return Physical::Record { record_type, loc };
        };
        self.uncompressed_record.clear();
        let input = self
            .backing
            .get(payload_start..payload_end)
            .unwrap_or_default();
        let mut pos = 0usize;
        loop {
            match uncompress.uncompress(input, &mut pos, &mut self.uncompressed_buffer) {
                Ok(out) => {
                    let produced = self
                        .uncompressed_buffer
                        .get(..out.output_len)
                        .unwrap_or_default();
                    self.uncompressed_record.extend_from_slice(produced);
                    if out.remaining == 0 && out.output_len != BLOCK_SIZE {
                        break;
                    }
                }
                Err(_) => {
                    self.clear_buffer();
                    return Physical::BadRecord;
                }
            }
        }
        Physical::Record {
            record_type,
            loc: Loc::Uncompressed,
        }
    }

    /// `ReadPhysicalRecord` [R db/log_reader.cc:510-695].
    fn read_physical_record(&mut self, drop_size: &mut usize) -> Physical {
        loop {
            if self.buffer_len() < HEADER_SIZE {
                match self.read_more(drop_size) {
                    Ok(()) => continue,
                    Err(r) => return r,
                }
            }
            let (length, record_type) = self.header();
            let mut header_size = HEADER_SIZE;
            let recyclable = is_recyclable_type(record_type);
            if recyclable {
                header_size = RECYCLABLE_HEADER_SIZE;
                if self.first_record_read && !self.recycled {
                    *drop_size = self.buffer_len();
                    self.clear_buffer();
                    return Physical::RecyclableInLegacyLog;
                }
                self.recycled = true;
                if self.buffer_len() < RECYCLABLE_HEADER_SIZE {
                    match self.read_more(drop_size) {
                        Ok(()) => continue,
                        Err(r) => return r,
                    }
                }
            }
            let total = header_size.saturating_add(length);
            if total > self.buffer_len() {
                // The end of the read came before the payload did; the caller decides from
                // the recovery mode and EOF what that means.
                *drop_size = self.buffer_len();
                self.clear_buffer();
                return Physical::BadRecordLen;
            }
            if recyclable {
                let log_num = self
                    .buffer()
                    .get(7..11)
                    .and_then(|b| decode_fixed32(b).ok());
                // The writer stores the log number's low 32 bits [R db/log_writer.cc:337], so
                // they are what is compared. RocksDB compares them with the whole 64-bit number
                // [R :603-605], and a recycled log numbered 2^32 or more reads there as all old:
                // every record lost.
                if log_num.map(u64::from) != Some(self.log_number & u64::from(u32::MAX)) {
                    self.start = self.start.saturating_add(total);
                    return Physical::OldRecord;
                }
            }
            return self.finish_physical(record_type, header_size, length, drop_size);
        }
    }

    /// `InitCompression` [R db/log_reader.cc:697-707].
    fn init_compression(&mut self, compression_type: CompressionType) -> Result<(), Error> {
        self.compression_type = compression_type;
        self.compression_type_record_read = true;
        self.uncompress = match compression_type {
            CompressionType::Zstd => Some(StreamingUncompress::zstd(BLOCK_SIZE)?),
            CompressionType::NoCompression => None,
        };
        self.uncompressed_buffer = vec![0; BLOCK_SIZE];
        Ok(())
    }

    /// The `kSetCompressionType` case of `ReadRecord` after its offsets are set
    /// [R db/log_reader.cc:173-194].
    fn on_set_compression_type(&mut self, payload: &[u8]) {
        if self.compression_type_record_read {
            self.report(payload.len(), LogCorruption::MultipleSetCompressionType);
        }
        if self.first_record_read {
            self.report(payload.len(), LogCorruption::SetCompressionTypeNotFirst);
        }
        let mut input = payload;
        match decode_compression_type_record(&mut input) {
            Ok(t) => {
                if let Err(e) = self.init_compression(t) {
                    self.reporter.corruption(0, &DropReason::Read(e), None);
                }
            }
            // RocksDB reports what the failed decode left of the payload.
            Err(_) => self.report(input.len(), LogCorruption::UndecodableSetCompressionType),
        }
    }

    /// The `kPredecessorWALInfoType` case [R db/log_reader.cc:195-210].
    fn on_predecessor_wal_info(&mut self, payload: &[u8], mode: WalRecoveryMode) {
        let mut input = payload;
        match PredecessorWalInfo::decode_from(&mut input) {
            Ok(info) => self.maybe_verify_predecessor_wal_info(mode, input.len(), &info),
            Err(_) => self.report(input.len(), LogCorruption::UndecodablePredecessorWalInfo),
        }
    }

    /// The decode and update of the timestamp-size case [R db/log_reader.cc:211-235].
    fn on_timestamp_size(&mut self, payload: &[u8]) {
        match decode_timestamp_size_record(payload) {
            // A decoded record has consumed its payload: RocksDB reports 0 bytes.
            Ok(entries) => {
                if let Err(c) = self.update_recorded_timestamp_size(&entries) {
                    self.report(0, c);
                }
            }
            Err(_) => self.report(payload.len(), LogCorruption::UndecodableTimestampSize),
        }
    }

    /// `UpdateRecordedTimestampSize` [R db/log_reader.cc:709-730]: entries before a bad one
    /// stay recorded.
    fn update_recorded_timestamp_size(
        &mut self,
        entries: &[(u32, usize)],
    ) -> Result<(), LogCorruption> {
        for &(cf, ts_sz) in entries {
            if ts_sz == 0 {
                return Err(LogCorruption::ZeroTimestampSize);
            }
            if self.recorded_cf_to_ts_sz.contains_key(&cf) {
                return Err(LogCorruption::TimestampSizeUpdate);
            }
            self.recorded_cf_to_ts_sz.insert(cf, ts_sz);
        }
        Ok(())
    }

    /// `MaybeVerifyPredecessorWALInfo` [R db/log_reader.cc:356-409]. `left` is what the
    /// decode left of the payload, which RocksDB reports as the bytes dropped.
    fn maybe_verify_predecessor_wal_info(
        &mut self,
        mode: WalRecoveryMode,
        left: usize,
        recorded: &PredecessorWalInfo,
    ) {
        let v = self.verification;
        if !v.track_and_verify_wals
            || mode == WalRecoveryMode::SkipAnyCorruptedRecords
            || v.stop_replay_for_corruption
        {
            return;
        }
        let log_number = recorded.log_number;
        let reason = match v.observed_predecessor_wal_info {
            // The first WAL recovered: there is no predecessor to compare with.
            None if log_number >= v.min_wal_number_to_keep => {
                Some(LogCorruption::MissingWal { log_number })
            }
            None => None,
            Some(observed) if observed.log_number != log_number => {
                Some(LogCorruption::MismatchedPredecessorLogNumber {
                    file_name: self.file.file_name().to_owned(),
                    recorded: log_number,
                    observed: observed.log_number,
                })
            }
            Some(observed) if observed.last_seqno_recorded != recorded.last_seqno_recorded => {
                Some(LogCorruption::MismatchedPredecessorLastSeqno {
                    log_number,
                    recorded: recorded.last_seqno_recorded,
                    observed: observed.last_seqno_recorded,
                })
            }
            Some(observed) if observed.size_bytes != recorded.size_bytes => {
                Some(LogCorruption::MismatchedPredecessorSize {
                    log_number,
                    recorded: recorded.size_bytes,
                    observed: observed.size_bytes,
                })
            }
            Some(_) => None,
        };
        if let Some(reason) = reason {
            self.report_for_log(left, reason, log_number);
        }
    }

    /// Resets the decompressor at the start of a logical record. A failure is reported as a
    /// read failure and ends the read.
    fn reset_uncompress(&mut self) -> bool {
        let Some(u) = self.uncompress.as_mut() else {
            return true;
        };
        match u.reset() {
            Ok(()) => true,
            Err(e) => {
                self.reporter.corruption(0, &DropReason::Read(e), None);
                self.read_error = true;
                false
            }
        }
    }
}

macro_rules! reader_accessors {
    () => {
        /// `IsEOF`.
        pub fn is_eof(&self) -> bool {
            self.core.eof
        }

        /// `hasReadError`.
        pub fn has_read_error(&self) -> bool {
            self.core.read_error
        }

        /// `LastRecordOffset`: the physical offset of the last record returned.
        pub fn last_record_offset(&self) -> u64 {
            self.core.last_record_offset
        }

        /// `LastRecordEnd`: the offset just past the last record returned.
        pub fn last_record_end(&self) -> u64 {
            self.core.buffer_offset()
        }

        /// `GetReadOffset`.
        pub fn read_offset(&self) -> u64 {
            self.core.end_of_buffer_offset
        }

        pub fn log_number(&self) -> u64 {
            self.core.log_number
        }

        /// `GetRecordedTimestampSize`: the timestamp sizes read so far.
        pub fn recorded_timestamp_size(&self) -> &HashMap<u32, usize> {
            &self.core.recorded_cf_to_ts_sz
        }

        /// `IsCompressedAndEmptyFile`.
        pub fn is_compressed_and_empty_file(&self) -> bool {
            !self.core.first_record_read && self.core.compression_type_record_read
        }

        pub fn compression_type(&self) -> CompressionType {
            self.core.compression_type
        }

        pub fn reporter(&self) -> &R {
            &self.core.reporter
        }

        pub fn reporter_mut(&mut self) -> &mut R {
            &mut self.core.reporter
        }

        pub fn file(&self) -> &F {
            &self.core.file
        }

        pub fn file_mut(&mut self) -> &mut F {
            &mut self.core.file
        }
    };
}

/// `log::Reader` [R db/log_reader.h:36-240].
pub struct Reader<F, R> {
    core: Core<F, R>,
}

impl<F: SequentialFile, R: Reporter> Reader<F, R> {
    /// A reader of log `log_number`; `checksum` verifies each record's CRC.
    pub fn new(file: F, reporter: R, checksum: bool, log_number: u64) -> Self {
        Self::with_verification(
            file,
            reporter,
            checksum,
            log_number,
            WalVerification::default(),
        )
    }

    pub fn with_verification(
        file: F,
        reporter: R,
        checksum: bool,
        log_number: u64,
        verification: WalVerification,
    ) -> Self {
        Self {
            core: Core::new(file, reporter, checksum, log_number, verification),
        }
    }

    reader_accessors!();

    /// `UnmarkEOF` [R db/log_reader.cc:372-380]: looks again past the end once more has been
    /// written.
    pub fn unmark_eof(&mut self) {
        if self.core.read_error {
            return;
        }
        self.core.eof = false;
        if self.core.eof_offset == 0 {
            return;
        }
        self.core.unmark_eof_internal();
    }

    /// `ReadRecord` [R db/log_reader.cc:75-354]: the next logical record into `record`, or
    /// false at the end of what `mode` reads. Dropped bytes go to the reporter.
    pub fn read_record(&mut self, record: &mut Vec<u8>, mode: WalRecoveryMode) -> bool {
        record.clear();
        if !self.core.reset_uncompress() {
            return false;
        }
        let core = &mut self.core;
        let mut in_fragmented_record = false;
        // The offset of the logical record being read.
        let mut prospective_record_offset = 0u64;
        loop {
            let physical_record_offset = core.buffer_offset();
            let mut drop_size = 0usize;
            let physical = core.read_physical_record(&mut drop_size);
            match physical {
                Physical::Record { record_type, loc } => match record_type {
                    FULL_TYPE | RECYCLABLE_FULL_TYPE => {
                        if in_fragmented_record && !record.is_empty() {
                            // An earlier writer could leave an empty First at a block's end
                            // followed by a Full or First in the next block.
                            core.report(record.len(), LogCorruption::PartialRecordWithoutEnd1);
                        }
                        record.clear();
                        record.extend_from_slice(core.fragment(loc));
                        core.last_record_offset = physical_record_offset;
                        core.first_record_read = true;
                        return true;
                    }
                    FIRST_TYPE | RECYCLABLE_FIRST_TYPE => {
                        if in_fragmented_record && !record.is_empty() {
                            core.report(record.len(), LogCorruption::PartialRecordWithoutEnd2);
                        }
                        prospective_record_offset = physical_record_offset;
                        record.clear();
                        record.extend_from_slice(core.fragment(loc));
                        in_fragmented_record = true;
                    }
                    MIDDLE_TYPE | RECYCLABLE_MIDDLE_TYPE => {
                        if in_fragmented_record {
                            record.extend_from_slice(core.fragment(loc));
                        } else {
                            let n = core.fragment(loc).len();
                            core.report(n, LogCorruption::MissingStart1);
                        }
                    }
                    LAST_TYPE | RECYCLABLE_LAST_TYPE => {
                        if in_fragmented_record {
                            record.extend_from_slice(core.fragment(loc));
                            core.last_record_offset = prospective_record_offset;
                            core.first_record_read = true;
                            return true;
                        }
                        let n = core.fragment(loc).len();
                        core.report(n, LogCorruption::MissingStart2);
                    }
                    SET_COMPRESSION_TYPE => {
                        record.clear();
                        core.last_record_offset = physical_record_offset;
                        let payload = core.fragment(loc).to_vec();
                        core.on_set_compression_type(&payload);
                    }
                    PREDECESSOR_WAL_INFO_TYPE | RECYCLE_PREDECESSOR_WAL_INFO_TYPE => {
                        record.clear();
                        core.last_record_offset = physical_record_offset;
                        let payload = core.fragment(loc).to_vec();
                        core.on_predecessor_wal_info(&payload, mode);
                    }
                    USER_DEFINED_TIMESTAMP_SIZE_TYPE
                    | RECYCLABLE_USER_DEFINED_TIMESTAMP_SIZE_TYPE => {
                        if in_fragmented_record && !record.is_empty() {
                            core.report(record.len(), LogCorruption::TimestampSizeInterspersed);
                        }
                        record.clear();
                        core.last_record_offset = physical_record_offset;
                        let payload = core.fragment(loc).to_vec();
                        core.on_timestamp_size(&payload);
                    }
                    unknown => {
                        if unknown & RECORD_TYPE_SAFE_IGNORE_MASK == 0 {
                            let n =
                                core.fragment(loc)
                                    .len()
                                    .saturating_add(if in_fragmented_record {
                                        record.len()
                                    } else {
                                        0
                                    });
                            core.report(n, LogCorruption::UnknownRecordType(unknown));
                        }
                        in_fragmented_record = false;
                        record.clear();
                    }
                },
                Physical::BadHeader | Physical::Eof => {
                    if matches!(physical, Physical::BadHeader) && mode.reports_tail() {
                        // A clean shutdown leaves no torn header; in point-in-time recovery a
                        // torn tail may hide a hole, which higher layers can rule out.
                        core.report(drop_size, LogCorruption::TruncatedHeader);
                    }
                    if in_fragmented_record {
                        if mode.reports_tail() {
                            core.report(record.len(), LogCorruption::EofInTrailingData);
                        }
                        // The writer died between a record's fragments: drop the record.
                        record.clear();
                    }
                    return false;
                }
                Physical::OldRecord if mode != WalRecoveryMode::SkipAnyCorruptedRecords => {
                    // A record of the file's previous log: the end of this one.
                    if in_fragmented_record {
                        if mode.reports_tail() {
                            core.report(record.len(), LogCorruption::OldRecordInTrailingData);
                        }
                        record.clear();
                    } else if mode == WalRecoveryMode::PointInTimeRecovery {
                        core.reporter.old_log_record(record.len());
                    }
                    return false;
                }
                Physical::OldRecord | Physical::BadRecord => {
                    if in_fragmented_record {
                        core.report(record.len(), LogCorruption::ErrorInMiddleOfRecord);
                        in_fragmented_record = false;
                        record.clear();
                    }
                }
                Physical::BadRecordLen if core.eof => {
                    if mode.reports_tail() {
                        core.report(drop_size, LogCorruption::TruncatedRecordBody);
                    }
                    return false;
                }
                Physical::BadRecordLen
                | Physical::BadRecordChecksum
                | Physical::RecyclableInLegacyLog => {
                    if core.recycled && mode == WalRecoveryMode::TolerateCorruptedTailRecords {
                        record.clear();
                        return false;
                    }
                    let reason = match physical {
                        Physical::BadRecordLen => LogCorruption::BadRecordLength,
                        Physical::RecyclableInLegacyLog => LogCorruption::RecyclableInLegacyLog,
                        _ => LogCorruption::ChecksumMismatch,
                    };
                    core.report(drop_size, reason);
                    if in_fragmented_record {
                        core.report(record.len(), LogCorruption::ErrorInMiddleOfRecord);
                        in_fragmented_record = false;
                        record.clear();
                    }
                }
            }
        }
    }
}

/// `FragmentBufferedReader` [R db/log_reader.h:242-271]: reads a log that is still being
/// written, keeping the fragments of an unfinished record between calls; at the end of what is
/// written it returns false, and a later call continues.
pub struct FragmentBufferedReader<F, R> {
    core: Core<F, R>,
    fragments: Vec<u8>,
    in_fragmented_record: bool,
}

impl<F: SequentialFile, R: Reporter> FragmentBufferedReader<F, R> {
    pub fn new(file: F, reporter: R, checksum: bool, log_number: u64) -> Self {
        Self {
            core: Core::new(
                file,
                reporter,
                checksum,
                log_number,
                WalVerification::default(),
            ),
            fragments: Vec::new(),
            in_fragmented_record: false,
        }
    }

    reader_accessors!();

    /// `FragmentBufferedReader::UnmarkEOF` [R db/log_reader.cc:916-923].
    pub fn unmark_eof(&mut self) {
        if self.core.read_error {
            return;
        }
        self.core.eof = false;
        self.core.unmark_eof_internal();
    }

    /// `TryReadMore` [R db/log_reader.cc:925-958]: false when nothing more can be read now.
    fn try_read_more(&mut self) -> bool {
        if !self.core.eof && !self.core.read_error {
            return self.core.read_block();
        }
        if !self.core.read_error {
            self.unmark_eof();
        }
        if !self.core.read_error {
            return true;
        }
        self.core.clear_buffer();
        false
    }

    /// Reads until the buffer holds `need` bytes: `Some(true)` if it does, `Some(false)` if a
    /// read replaced the buffer with a new block (so its header must be parsed again), `None`
    /// if no more can be read now [R db/log_reader.cc:966-975].
    fn fill(&mut self, need: usize) -> Option<bool> {
        while self.core.buffer_len() < need {
            let old_len = self.core.buffer_len();
            // Before the end of the file is seen, a read replaces the block.
            let replaces = !self.core.eof && !self.core.read_error;
            if !self.try_read_more() || old_len == self.core.buffer_len() {
                return None;
            }
            if replaces {
                return Some(false);
            }
        }
        Some(true)
    }

    /// `TryReadFragment` [R db/log_reader.cc:960-1070]: `None` when the caller should stop.
    fn try_read_fragment(&mut self, drop_size: &mut usize) -> Option<Physical> {
        'parse: loop {
            if !self.fill(HEADER_SIZE)? {
                continue 'parse;
            }
            let (length, record_type) = self.core.header();
            let mut header_size = HEADER_SIZE;
            if is_recyclable_type(record_type) {
                if self.core.first_record_read && !self.core.recycled {
                    *drop_size = self.core.buffer_len();
                    self.core.clear_buffer();
                    return Some(Physical::RecyclableInLegacyLog);
                }
                self.core.recycled = true;
                header_size = RECYCLABLE_HEADER_SIZE;
                if !self.fill(RECYCLABLE_HEADER_SIZE)? {
                    continue 'parse;
                }
                let log_num = self
                    .core
                    .buffer()
                    .get(7..11)
                    .and_then(|b| decode_fixed32(b).ok());
                // The low 32 bits, as in `Reader` (RocksDB again compares all 64 [R :999-1000]).
                if log_num.map(u64::from) != Some(self.core.log_number & u64::from(u32::MAX)) {
                    return Some(Physical::OldRecord);
                }
            }
            if !self.fill(header_size.saturating_add(length))? {
                continue 'parse;
            }
            return Some(
                self.core
                    .finish_physical(record_type, header_size, length, drop_size),
            );
        }
    }

    /// `FragmentBufferedReader::ReadRecord` [R db/log_reader.cc:733-914].
    pub fn read_record(&mut self, record: &mut Vec<u8>, mode: WalRecoveryMode) -> bool {
        record.clear();
        if !self.core.reset_uncompress() {
            return false;
        }
        let mut prospective_record_offset = 0u64;
        let physical_record_offset = self.core.buffer_offset();
        let mut drop_size = 0usize;
        while let Some(physical) = self.try_read_fragment(&mut drop_size) {
            let core = &mut self.core;
            match physical {
                Physical::Record { record_type, loc } => match record_type {
                    FULL_TYPE | RECYCLABLE_FULL_TYPE => {
                        if self.in_fragmented_record && !self.fragments.is_empty() {
                            core.report(
                                self.fragments.len(),
                                LogCorruption::PartialRecordWithoutEnd1,
                            );
                        }
                        self.fragments.clear();
                        record.extend_from_slice(core.fragment(loc));
                        core.last_record_offset = physical_record_offset;
                        core.first_record_read = true;
                        self.in_fragmented_record = false;
                        return true;
                    }
                    FIRST_TYPE | RECYCLABLE_FIRST_TYPE => {
                        if self.in_fragmented_record || !self.fragments.is_empty() {
                            core.report(
                                self.fragments.len(),
                                LogCorruption::PartialRecordWithoutEnd2,
                            );
                        }
                        prospective_record_offset = physical_record_offset;
                        self.fragments.clear();
                        self.fragments.extend_from_slice(core.fragment(loc));
                        self.in_fragmented_record = true;
                    }
                    MIDDLE_TYPE | RECYCLABLE_MIDDLE_TYPE => {
                        if self.in_fragmented_record {
                            self.fragments.extend_from_slice(core.fragment(loc));
                        } else {
                            let n = core.fragment(loc).len();
                            core.report(n, LogCorruption::MissingStart1);
                        }
                    }
                    LAST_TYPE | RECYCLABLE_LAST_TYPE => {
                        if self.in_fragmented_record {
                            self.fragments.extend_from_slice(core.fragment(loc));
                            record.clear();
                            record.append(&mut self.fragments);
                            core.last_record_offset = prospective_record_offset;
                            core.first_record_read = true;
                            self.in_fragmented_record = false;
                            return true;
                        }
                        let n = core.fragment(loc).len();
                        core.report(n, LogCorruption::MissingStart2);
                    }
                    SET_COMPRESSION_TYPE => {
                        self.fragments.clear();
                        core.last_record_offset = physical_record_offset;
                        self.in_fragmented_record = false;
                        let payload = core.fragment(loc).to_vec();
                        core.on_set_compression_type(&payload);
                    }
                    PREDECESSOR_WAL_INFO_TYPE | RECYCLE_PREDECESSOR_WAL_INFO_TYPE => {
                        self.fragments.clear();
                        core.last_record_offset = physical_record_offset;
                        self.in_fragmented_record = false;
                        let payload = core.fragment(loc).to_vec();
                        core.on_predecessor_wal_info(&payload, mode);
                    }
                    USER_DEFINED_TIMESTAMP_SIZE_TYPE
                    | RECYCLABLE_USER_DEFINED_TIMESTAMP_SIZE_TYPE => {
                        // RocksDB tests the caller's scratch here, which is always empty
                        // [R :847-852], so it never reports an interspersed record.
                        self.fragments.clear();
                        core.last_record_offset = physical_record_offset;
                        self.in_fragmented_record = false;
                        let payload = core.fragment(loc).to_vec();
                        core.on_timestamp_size(&payload);
                    }
                    unknown => {
                        if unknown & RECORD_TYPE_SAFE_IGNORE_MASK == 0 {
                            let n = core.fragment(loc).len().saturating_add(
                                if self.in_fragmented_record {
                                    self.fragments.len()
                                } else {
                                    0
                                },
                            );
                            core.report(n, LogCorruption::UnknownRecordType(unknown));
                        }
                        self.in_fragmented_record = false;
                        self.fragments.clear();
                    }
                },
                Physical::OldRecord => {
                    // The writer has not yet overwritten this part of the recycled file.
                    return false;
                }
                Physical::BadHeader
                | Physical::BadRecord
                | Physical::Eof
                | Physical::BadRecordLen => {
                    if self.in_fragmented_record {
                        core.report(self.fragments.len(), LogCorruption::ErrorInMiddleOfRecord);
                        self.in_fragmented_record = false;
                        self.fragments.clear();
                    }
                }
                Physical::BadRecordChecksum | Physical::RecyclableInLegacyLog => {
                    if core.recycled {
                        self.fragments.clear();
                        return false;
                    }
                    let reason = if matches!(physical, Physical::RecyclableInLegacyLog) {
                        LogCorruption::RecyclableInLegacyLog
                    } else {
                        LogCorruption::ChecksumMismatch
                    };
                    core.report(drop_size, reason);
                    if self.in_fragmented_record {
                        core.report(self.fragments.len(), LogCorruption::ErrorInMiddleOfRecord);
                        self.in_fragmented_record = false;
                        self.fragments.clear();
                    }
                }
            }
        }
        false
    }
}
