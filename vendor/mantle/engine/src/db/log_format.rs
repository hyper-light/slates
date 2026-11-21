//! The log format shared by the WAL and the MANIFEST: `db/log_format.h`, with the payloads of
//! its meta records, `PredecessorWALInfo` of `db/dbformat.h` [R :1260-1319] and
//! `UserDefinedTimestampSizeRecord` of `util/udt_util.h` [R :25-85], and `WALRecoveryMode` of
//! `include/rocksdb/options.h` [R :411-448] (docs/research/24 §1.5).
//!
//! A log is a sequence of 32 KiB blocks. A physical record never crosses a block: it is a
//! header (masked CRC-32C, 16-bit payload length, type, and in a recyclable record the low 32
//! bits of the log number) and its payload; a logical record that does not fit is cut into
//! First/Middle/Last fragments, and a block tail too short for a header is zero-filled.

use crate::error::Error;
use crate::util::coding::{
    get_fixed16, get_fixed32, get_fixed64, put_fixed16, put_fixed32, put_fixed64,
};

/// `kZeroType`: preallocated space [R db/log_format.h:24].
pub const ZERO_TYPE: u8 = 0;
/// `kFullType` [R db/log_format.h:25].
pub const FULL_TYPE: u8 = 1;
/// `kFirstType` [R db/log_format.h:28].
pub const FIRST_TYPE: u8 = 2;
/// `kMiddleType` [R db/log_format.h:29].
pub const MIDDLE_TYPE: u8 = 3;
/// `kLastType` [R db/log_format.h:30].
pub const LAST_TYPE: u8 = 4;
/// `kRecyclableFullType` [R db/log_format.h:33].
pub const RECYCLABLE_FULL_TYPE: u8 = 5;
/// `kRecyclableFirstType` [R db/log_format.h:34].
pub const RECYCLABLE_FIRST_TYPE: u8 = 6;
/// `kRecyclableMiddleType` [R db/log_format.h:35].
pub const RECYCLABLE_MIDDLE_TYPE: u8 = 7;
/// `kRecyclableLastType` [R db/log_format.h:36].
pub const RECYCLABLE_LAST_TYPE: u8 = 8;
/// `kSetCompressionType`: always a legacy header [R db/log_format.h:39].
pub const SET_COMPRESSION_TYPE: u8 = 9;
/// `kUserDefinedTimestampSizeType` [R db/log_format.h:43].
pub const USER_DEFINED_TIMESTAMP_SIZE_TYPE: u8 = 10;
/// `kRecyclableUserDefinedTimestampSizeType` [R db/log_format.h:44].
pub const RECYCLABLE_USER_DEFINED_TIMESTAMP_SIZE_TYPE: u8 = 11;
/// `kPredecessorWALInfoType` [R db/log_format.h:47].
pub const PREDECESSOR_WAL_INFO_TYPE: u8 = 130;
/// `kRecyclePredecessorWALInfoType` [R db/log_format.h:48].
pub const RECYCLE_PREDECESSOR_WAL_INFO_TYPE: u8 = 131;

/// `kRecordTypeSafeIgnoreMask`: a reader skips an unknown type with this bit set and reports
/// any other unknown type as corruption [R db/log_format.h:51].
pub const RECORD_TYPE_SAFE_IGNORE_MASK: u8 = 1 << 7;

/// `kBlockSize` [R db/log_format.h:54].
pub const BLOCK_SIZE: usize = 32768;

/// `kHeaderSize`: checksum (4), length (2), type (1) [R db/log_format.h:57].
pub const HEADER_SIZE: usize = 4 + 2 + 1;

/// `kRecyclableHeaderSize`: the legacy header and a 4-byte log number [R db/log_format.h:61].
pub const RECYCLABLE_HEADER_SIZE: usize = 4 + 2 + 1 + 4;

/// Whether a type carries the recyclable header [R db/log_reader.cc:562-565].
pub const fn is_recyclable_type(t: u8) -> bool {
    matches!(
        t,
        RECYCLABLE_FULL_TYPE
            ..=RECYCLABLE_LAST_TYPE
                | RECYCLABLE_USER_DEFINED_TIMESTAMP_SIZE_TYPE
                | RECYCLE_PREDECESSOR_WAL_INFO_TYPE
    )
}

/// Whether a type is a meta record, whose payload is never compressed or fragmented
/// [R db/log_reader.cc:636-641].
pub const fn is_meta_type(t: u8) -> bool {
    matches!(
        t,
        SET_COMPRESSION_TYPE
            | PREDECESSOR_WAL_INFO_TYPE
            | RECYCLE_PREDECESSOR_WAL_INFO_TYPE
            | USER_DEFINED_TIMESTAMP_SIZE_TYPE
            | RECYCLABLE_USER_DEFINED_TIMESTAMP_SIZE_TYPE
    )
}

/// `WALRecoveryMode` [R include/rocksdb/options.h:411-448]: what a WAL reader treats as the
/// end of the log and what it reports as corruption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalRecoveryMode {
    /// Tolerates an incomplete record at the tail of any log (LevelDB's recovery).
    TolerateCorruptedTailRecords,
    /// Expects no corruption at all (a clean shutdown).
    AbsoluteConsistency,
    /// Stops at the first inconsistency, reporting it; RocksDB's default [R :1480].
    PointInTimeRecovery,
    /// Skips every corrupted record and keeps reading.
    SkipAnyCorruptedRecords,
}

impl WalRecoveryMode {
    /// The value an OPTIONS file stores.
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::TolerateCorruptedTailRecords => 0x00,
            Self::AbsoluteConsistency => 0x01,
            Self::PointInTimeRecovery => 0x02,
            Self::SkipAnyCorruptedRecords => 0x03,
        }
    }

    pub const fn from_u8(value: u8) -> Result<Self, Error> {
        match value {
            0x00 => Ok(Self::TolerateCorruptedTailRecords),
            0x01 => Ok(Self::AbsoluteConsistency),
            0x02 => Ok(Self::PointInTimeRecovery),
            0x03 => Ok(Self::SkipAnyCorruptedRecords),
            other => Err(Error::Unsupported {
                feature: "WAL recovery mode",
                value: other as u64,
            }),
        }
    }

    /// The modes that report a torn tail: AbsoluteConsistency and PointInTimeRecovery
    /// [R db/log_reader.cc:254-256].
    pub(crate) const fn reports_tail(self) -> bool {
        matches!(self, Self::AbsoluteConsistency | Self::PointInTimeRecovery)
    }
}

/// `PredecessorWALInfo` [R db/dbformat.h:1260-1319]: the WAL before this one, recorded with
/// `track_and_verify_wals` so recovery can tell a missing or truncated WAL from a clean end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PredecessorWalInfo {
    pub log_number: u64,
    pub size_bytes: u64,
    pub last_seqno_recorded: u64,
}

impl PredecessorWalInfo {
    /// `EncodeTo` [R db/dbformat.h:1292-1298].
    pub fn encode_to(&self, dst: &mut Vec<u8>) {
        put_fixed64(dst, self.log_number);
        put_fixed64(dst, self.size_bytes);
        put_fixed64(dst, self.last_seqno_recorded);
    }

    /// `DecodeFrom` [R db/dbformat.h:1300-1312]: bytes after the three fields are left in
    /// `src`, as RocksDB leaves them.
    pub fn decode_from(src: &mut &[u8]) -> Result<Self, Error> {
        Ok(Self {
            log_number: get_fixed64(src)?,
            size_bytes: get_fixed64(src)?,
            last_seqno_recorded: get_fixed64(src)?,
        })
    }
}

/// One entry of a `UserDefinedTimestampSizeRecord`: a fixed32 column family id and a fixed16
/// timestamp size [R util/udt_util.h:80].
pub const TIMESTAMP_SIZE_ENTRY_SIZE: usize = 4 + 2;

/// `UserDefinedTimestampSizeRecord::EncodeTo` [R util/udt_util.h:38-45]. A size above
/// `u16::MAX` cannot be stored; RocksDB narrows it silently, here it is refused.
pub fn encode_timestamp_size_record(
    dst: &mut Vec<u8>,
    cf_to_ts_sz: &[(u32, usize)],
) -> Result<(), Error> {
    for &(cf_id, ts_sz) in cf_to_ts_sz {
        let size = u16::try_from(ts_sz).map_err(|_| Error::InvalidArgument {
            what: "user-defined timestamp size larger than 16 bits",
        })?;
        put_fixed32(dst, cf_id);
        put_fixed16(dst, size);
    }
    Ok(())
}

/// `UserDefinedTimestampSizeRecord::DecodeFrom` [R util/udt_util.h:47-66]: the whole payload,
/// which must be a multiple of [`TIMESTAMP_SIZE_ENTRY_SIZE`].
pub fn decode_timestamp_size_record(src: &[u8]) -> Result<Vec<(u32, usize)>, Error> {
    if !src.len().is_multiple_of(TIMESTAMP_SIZE_ENTRY_SIZE) {
        return Err(Error::truncated("user-defined timestamp size record"));
    }
    let mut input = src;
    let mut out = Vec::new();
    while !input.is_empty() {
        let cf_id = get_fixed32(&mut input)?;
        let ts_sz = get_fixed16(&mut input)?;
        out.push((cf_id, usize::from(ts_sz)));
    }
    Ok(out)
}
