//! `WriteBatch`: RocksDB's `include/rocksdb/write_batch.h`, `db/write_batch.cc` and
//! `db/write_batch_internal.h` (docs/research/24 §1.4).
//!
//! A batch is `fixed64 sequence ‖ fixed32 count` and then records, each a tag byte and its
//! payload:
//!
//! | Tag (default CF / other CF) | Payload |
//! |---|---|
//! | 0x01 / 0x05 Put | [CF] LP key, LP value |
//! | 0x00 / 0x04 Delete | [CF] LP key |
//! | 0x07 / 0x08 SingleDelete | [CF] LP key |
//! | 0x0F / 0x0E DeleteRange | [CF] LP begin key, LP end key |
//! | 0x02 / 0x06 Merge | [CF] LP key, LP operand |
//! | 0x11 / 0x10 PutBlobIndex | [CF] LP key, LP blob index |
//! | 0x16 / 0x17 PutEntity | [CF] LP key, LP wide-column entity |
//! | 0x18 / 0x19 TimedPut | [CF] LP key, LP (value ‖ fixed64 write time) |
//! | 0x03 LogData | LP blob |
//! | 0x0D Noop | — |
//! | 0x09 / 0x12 / 0x13 BeginPrepare | — |
//! | 0x0A EndPrepare, 0x0B Commit, 0x0C Rollback | LP xid |
//! | 0x15 CommitWithTimestamp | LP commit timestamp, LP xid |
//!
//! `[CF]` is a varint32 column-family id, present only in the other-CF tag, which is written
//! only for a column family other than 0. LP is a varint32 length and the bytes. `count` counts
//! the data records (the first eight rows), not LogData, Noop or the transaction markers.
//!
//! The port decodes every tag. It writes every record a ported test or the oracle comparison
//! needs; the two-phase-commit markers come from RocksDB's transactions (P17) and applying a
//! batch that holds one to a memtable is refused (docs/research/24 §1.4 DECISION, §1.18).
//! Per-key protection information (`protection_bytes_per_key`) lives beside a batch, never in
//! its bytes [R include/rocksdb/write_batch.h:492, :537]; it is ported with the write path
//! (P7), whose tests exercise it.

use std::sync::atomic::{AtomicU32, Ordering};

use crate::db::dbformat::{NUM_INTERNAL_BYTES, SequenceNumber, ValueType};
use crate::db::memtable::MemTable;
use crate::db::wide::wide_column_serialization::serialize;
use crate::db::wide::wide_columns::{AttributeGroup, WideColumn};
use crate::db::wide::wide_columns_helper::sort_columns;
use crate::error::{Error, Malformed};
use crate::util::coding::{
    decode_fixed32, decode_fixed64, encode_fixed32, encode_fixed64, get_length_prefixed_slice,
    get_varint32, put_length_prefixed_slice, put_length_prefixed_slice_parts, put_varint32,
};

/// `WriteBatchInternal::kHeader` [R db/write_batch_internal.h:81]: 8-byte sequence number and
/// 4-byte count.
pub const HEADER: usize = 12;

/// Where the count sits in the header.
const COUNT_OFFSET: usize = 8;

/// `kMaxWriteBatchKeySize` [R db/write_batch.cc:99-100]: a key and its 8-byte trailer must fit
/// the varint32 length of a memtable entry.
pub const MAX_KEY_SIZE: usize = u32::MAX as usize - NUM_INTERNAL_BYTES;

/// The longest value, operand, entity or end key: its length is a varint32
/// [R db/write_batch.cc:863-865].
pub const MAX_VALUE_SIZE: usize = u32::MAX as usize;

/// The most save points a batch holds at once (CLAUDE.md §2). RocksDB's stack is unbounded.
/// A caller sets one per nested scope it may roll back (a transaction's savepoints); mantle's
/// own writes set none, since a range applies whole batches. 2^16 nested scopes hold 1.5 MiB
/// of offsets and pass any nesting a caller builds by hand; past it `set_save_point` refuses.
pub const MAX_SAVE_POINTS: usize = 1 << 16;

/// The content flags of `ContentFlags` [R db/write_batch.cc:80-95]: which record kinds a batch
/// holds, kept as records are added, or computed on demand (`DEFERRED`) for a batch built
/// from bytes.
mod flags {
    pub const DEFERRED: u32 = 1 << 0;
    pub const HAS_PUT: u32 = 1 << 1;
    pub const HAS_DELETE: u32 = 1 << 2;
    pub const HAS_SINGLE_DELETE: u32 = 1 << 3;
    pub const HAS_MERGE: u32 = 1 << 4;
    pub const HAS_BEGIN_PREPARE: u32 = 1 << 5;
    pub const HAS_END_PREPARE: u32 = 1 << 6;
    pub const HAS_COMMIT: u32 = 1 << 7;
    pub const HAS_ROLLBACK: u32 = 1 << 8;
    pub const HAS_DELETE_RANGE: u32 = 1 << 9;
    pub const HAS_BLOB_INDEX: u32 = 1 << 10;
    pub const HAS_BEGIN_UNPREPARE: u32 = 1 << 11;
    pub const HAS_PUT_ENTITY: u32 = 1 << 12;
    pub const HAS_TIMED_PUT: u32 = 1 << 13;
}

/// `SavePoint` [R include/rocksdb/write_batch_base.h; db/write_batch_internal.h]: a batch's
/// size, count and content flags at a moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SavePoint {
    size: usize,
    count: u32,
    content_flags: u32,
}

/// One decoded record: `ReadRecordFromWriteBatch` [R db/write_batch.cc:377-515].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Record<'a> {
    Put {
        cf: u32,
        key: &'a [u8],
        value: &'a [u8],
    },
    TimedPut {
        cf: u32,
        key: &'a [u8],
        value: &'a [u8],
        write_unix_time: u64,
    },
    Delete {
        cf: u32,
        key: &'a [u8],
    },
    SingleDelete {
        cf: u32,
        key: &'a [u8],
    },
    DeleteRange {
        cf: u32,
        begin: &'a [u8],
        end: &'a [u8],
    },
    Merge {
        cf: u32,
        key: &'a [u8],
        value: &'a [u8],
    },
    BlobIndex {
        cf: u32,
        key: &'a [u8],
        value: &'a [u8],
    },
    PutEntity {
        cf: u32,
        key: &'a [u8],
        entity: &'a [u8],
    },
    LogData {
        blob: &'a [u8],
    },
    Noop,
    /// A BeginPrepare marker; its tag names the transaction write policy that wrote it.
    BeginPrepare {
        tag: ValueType,
    },
    EndPrepare {
        xid: &'a [u8],
    },
    Commit {
        xid: &'a [u8],
    },
    CommitWithTimestamp {
        xid: &'a [u8],
        commit_ts: &'a [u8],
    },
    Rollback {
        xid: &'a [u8],
    },
}

/// `PackValueAndWriteTime` [R db/seqno_to_time_mapping.cc:521-526]: a TimedPut's stored value.
pub fn pack_value_and_write_time(value: &[u8], write_unix_time: u64) -> Vec<u8> {
    let mut buf = value.to_vec();
    buf.extend_from_slice(&encode_fixed64(write_unix_time));
    buf
}

/// `ParsePackedValueWithWriteTime` [R db/seqno_to_time_mapping.cc:535-548]. RocksDB asserts
/// the packed value holds the 8-byte time; a shorter one is corruption here.
pub fn parse_packed_value_with_write_time(packed: &[u8]) -> Result<(&[u8], u64), Error> {
    let split = packed
        .len()
        .checked_sub(8)
        .ok_or(Error::truncated("TimedPut write time"))?;
    let (value, time) = packed
        .split_at_checked(split)
        .ok_or(Error::truncated("TimedPut write time"))?;
    Ok((value, decode_fixed64(time)?))
}

/// Relabels a decode failure with the record it failed in, as RocksDB reports
/// "bad WriteBatch Put" and the like.
fn bad(what: &'static str) -> impl Fn(Error) -> Error {
    move |e| match e {
        Error::Corruption { why, .. } => Error::corruption(what, why),
        other => other,
    }
}

/// `ReadRecordFromWriteBatch` [R db/write_batch.cc:377-515]: the record at the front of
/// `input`, which advances past it. On error `input` is left where the record's tag was read.
pub fn read_record<'a>(input: &mut &'a [u8]) -> Result<Record<'a>, Error> {
    let (&tag, rest) = input
        .split_first()
        .ok_or(Error::truncated("WriteBatch record"))?;
    let mut p: &'a [u8] = rest;
    let t = ValueType::from_u8(tag);
    let cf_of = |p: &mut &'a [u8], what: &'static str, cf_tag: bool| -> Result<u32, Error> {
        if cf_tag {
            get_varint32(p).map_err(bad(what))
        } else {
            Ok(0)
        }
    };
    let lp = |p: &mut &'a [u8], what: &'static str| get_length_prefixed_slice(p).map_err(bad(what));
    use ValueType as V;
    let record = match t {
        Some(v @ (V::Value | V::ColumnFamilyValue)) => {
            let what = "WriteBatch Put";
            let cf = cf_of(&mut p, what, v == V::ColumnFamilyValue)?;
            Record::Put {
                cf,
                key: lp(&mut p, what)?,
                value: lp(&mut p, what)?,
            }
        }
        Some(
            v @ (V::Deletion
            | V::ColumnFamilyDeletion
            | V::SingleDeletion
            | V::ColumnFamilySingleDeletion),
        ) => {
            let what = "WriteBatch Delete";
            let cf = cf_of(
                &mut p,
                what,
                matches!(v, V::ColumnFamilyDeletion | V::ColumnFamilySingleDeletion),
            )?;
            let key = lp(&mut p, what)?;
            if matches!(v, V::Deletion | V::ColumnFamilyDeletion) {
                Record::Delete { cf, key }
            } else {
                Record::SingleDelete { cf, key }
            }
        }
        Some(v @ (V::RangeDeletion | V::ColumnFamilyRangeDeletion)) => {
            let what = "WriteBatch DeleteRange";
            let cf = cf_of(&mut p, what, v == V::ColumnFamilyRangeDeletion)?;
            Record::DeleteRange {
                cf,
                begin: lp(&mut p, what)?,
                end: lp(&mut p, what)?,
            }
        }
        Some(v @ (V::Merge | V::ColumnFamilyMerge)) => {
            let what = "WriteBatch Merge";
            let cf = cf_of(&mut p, what, v == V::ColumnFamilyMerge)?;
            Record::Merge {
                cf,
                key: lp(&mut p, what)?,
                value: lp(&mut p, what)?,
            }
        }
        Some(v @ (V::BlobIndex | V::ColumnFamilyBlobIndex)) => {
            let what = "WriteBatch BlobIndex";
            let cf = cf_of(&mut p, what, v == V::ColumnFamilyBlobIndex)?;
            Record::BlobIndex {
                cf,
                key: lp(&mut p, what)?,
                value: lp(&mut p, what)?,
            }
        }
        Some(V::LogData) => Record::LogData {
            blob: lp(&mut p, "WriteBatch Blob")?,
        },
        Some(V::Noop) => Record::Noop,
        Some(v @ (V::BeginPrepareXid | V::BeginPersistedPrepareXid | V::BeginUnprepareXid)) => {
            Record::BeginPrepare { tag: v }
        }
        Some(V::EndPrepareXid) => Record::EndPrepare {
            xid: lp(&mut p, "EndPrepare XID")?,
        },
        Some(V::CommitXidAndTimestamp) => {
            let commit_ts = lp(&mut p, "commit timestamp")?;
            Record::CommitWithTimestamp {
                commit_ts,
                xid: lp(&mut p, "Commit XID")?,
            }
        }
        Some(V::CommitXid) => Record::Commit {
            xid: lp(&mut p, "Commit XID")?,
        },
        Some(V::RollbackXid) => Record::Rollback {
            xid: lp(&mut p, "Rollback XID")?,
        },
        Some(v @ (V::WideColumnEntity | V::ColumnFamilyWideColumnEntity)) => {
            let what = "WriteBatch PutEntity";
            let cf = cf_of(&mut p, what, v == V::ColumnFamilyWideColumnEntity)?;
            Record::PutEntity {
                cf,
                key: lp(&mut p, what)?,
                entity: lp(&mut p, what)?,
            }
        }
        Some(v @ (V::ValuePreferredSeqno | V::ColumnFamilyValuePreferredSeqno)) => {
            let what = "WriteBatch TimedPut";
            let cf = cf_of(&mut p, what, v == V::ColumnFamilyValuePreferredSeqno)?;
            let key = lp(&mut p, what)?;
            let packed = lp(&mut p, what)?;
            let (value, write_unix_time) =
                parse_packed_value_with_write_time(packed).map_err(bad(what))?;
            Record::TimedPut {
                cf,
                key,
                value,
                write_unix_time,
            }
        }
        _ => {
            return Err(Error::corruption("WriteBatch", Malformed::UnknownTag(tag)));
        }
    };
    *input = p;
    Ok(record)
}

/// `ReadKeyFromWriteBatchEntry` [R db/write_batch.cc:320-333]: the key of the record at the
/// front of `input`, skipping its tag and, for an other-CF record, its column family.
pub fn read_key_from_write_batch_entry<'a>(
    input: &mut &'a [u8],
    cf_record: bool,
) -> Result<&'a [u8], Error> {
    let mut p: &'a [u8] = input
        .get(1..)
        .ok_or(Error::truncated("WriteBatch record"))?;
    if cf_record {
        get_varint32(&mut p)?;
    }
    let key = get_length_prefixed_slice(&mut p)?;
    *input = p;
    Ok(key)
}

/// A transaction write policy's view of the BeginPrepare tags: `Handler::OptionState`
/// [R include/rocksdb/write_batch.h:365-375].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionState {
    Unknown,
    Disabled,
    Enabled,
}

/// The handler error RocksDB returns for a call its handler does not implement.
const fn not_implemented(what: &'static str) -> Error {
    Error::InvalidArgument { what }
}

/// `WriteBatch::Handler` [R include/rocksdb/write_batch.h:236-376]: what `iterate` calls for
/// each record, with RocksDB's defaults.
pub trait Handler {
    /// `PutCF`: the default column family goes to `put`; another is refused.
    fn put_cf(&mut self, cf: u32, key: &[u8], value: &[u8]) -> Result<(), Error> {
        if cf == 0 {
            self.put(key, value);
            Ok(())
        } else {
            Err(not_implemented(
                "non-default column family and PutCF not implemented",
            ))
        }
    }
    fn put(&mut self, _key: &[u8], _value: &[u8]) {}

    fn timed_put_cf(
        &mut self,
        _cf: u32,
        _key: &[u8],
        _value: &[u8],
        _write_unix_time: u64,
    ) -> Result<(), Error> {
        Err(not_implemented("TimedPutCF not implemented"))
    }

    /// `PutEntityCF`: RocksDB's default is `NotSupported`.
    fn put_entity_cf(&mut self, cf: u32, _key: &[u8], _entity: &[u8]) -> Result<(), Error> {
        Err(Error::Unsupported {
            feature: "handler without PutEntityCF",
            value: u64::from(cf),
        })
    }

    fn delete_cf(&mut self, cf: u32, key: &[u8]) -> Result<(), Error> {
        if cf == 0 {
            self.delete(key);
            Ok(())
        } else {
            Err(not_implemented(
                "non-default column family and DeleteCF not implemented",
            ))
        }
    }
    fn delete(&mut self, _key: &[u8]) {}

    fn single_delete_cf(&mut self, cf: u32, key: &[u8]) -> Result<(), Error> {
        if cf == 0 {
            self.single_delete(key);
            Ok(())
        } else {
            Err(not_implemented(
                "non-default column family and SingleDeleteCF not implemented",
            ))
        }
    }
    fn single_delete(&mut self, _key: &[u8]) {}

    fn delete_range_cf(&mut self, _cf: u32, _begin: &[u8], _end: &[u8]) -> Result<(), Error> {
        Err(not_implemented("DeleteRangeCF not implemented"))
    }

    fn merge_cf(&mut self, cf: u32, key: &[u8], value: &[u8]) -> Result<(), Error> {
        if cf == 0 {
            self.merge(key, value);
            Ok(())
        } else {
            Err(not_implemented(
                "non-default column family and MergeCF not implemented",
            ))
        }
    }
    fn merge(&mut self, _key: &[u8], _value: &[u8]) {}

    fn put_blob_index_cf(&mut self, _cf: u32, _key: &[u8], _value: &[u8]) -> Result<(), Error> {
        Err(not_implemented("PutBlobIndexCF not implemented"))
    }

    fn log_data(&mut self, _blob: &[u8]) {}

    fn mark_begin_prepare(&mut self, _unprepare: bool) -> Result<(), Error> {
        Err(not_implemented("MarkBeginPrepare() handler not defined."))
    }
    fn mark_end_prepare(&mut self, _xid: &[u8]) -> Result<(), Error> {
        Err(not_implemented("MarkEndPrepare() handler not defined."))
    }
    fn mark_noop(&mut self, _empty_batch: bool) -> Result<(), Error> {
        Err(not_implemented("MarkNoop() handler not defined."))
    }
    fn mark_rollback(&mut self, _xid: &[u8]) -> Result<(), Error> {
        Err(not_implemented(
            "MarkRollbackPrepare() handler not defined.",
        ))
    }
    fn mark_commit(&mut self, _xid: &[u8]) -> Result<(), Error> {
        Err(not_implemented("MarkCommit() handler not defined."))
    }
    fn mark_commit_with_timestamp(&mut self, _xid: &[u8], _commit_ts: &[u8]) -> Result<(), Error> {
        Err(not_implemented(
            "MarkCommitWithTimestamp() handler not defined.",
        ))
    }

    /// `Continue`: false stops the iteration before the next record, without error.
    fn should_continue(&mut self) -> bool {
        true
    }

    fn write_after_commit(&self) -> OptionState {
        OptionState::Unknown
    }
    fn write_before_prepare(&self) -> OptionState {
        OptionState::Unknown
    }
}

/// The refusal of a BeginPrepare tag written under another transaction write policy
/// [R db/write_batch.cc:636-689].
const fn write_policy_mismatch(tag: ValueType) -> Error {
    Error::Unsupported {
        feature: "transaction write policy of the WAL's prepare marker",
        value: tag as u64,
    }
}

/// `BatchContentClassifier` [R db/write_batch.cc:102-175].
#[derive(Default)]
struct ContentClassifier {
    content_flags: u32,
}

impl Handler for ContentClassifier {
    fn put_cf(&mut self, _: u32, _: &[u8], _: &[u8]) -> Result<(), Error> {
        self.content_flags |= flags::HAS_PUT;
        Ok(())
    }
    fn timed_put_cf(&mut self, _: u32, _: &[u8], _: &[u8], _: u64) -> Result<(), Error> {
        self.content_flags |= flags::HAS_TIMED_PUT;
        Ok(())
    }
    fn put_entity_cf(&mut self, _: u32, _: &[u8], _: &[u8]) -> Result<(), Error> {
        self.content_flags |= flags::HAS_PUT_ENTITY;
        Ok(())
    }
    fn delete_cf(&mut self, _: u32, _: &[u8]) -> Result<(), Error> {
        self.content_flags |= flags::HAS_DELETE;
        Ok(())
    }
    fn single_delete_cf(&mut self, _: u32, _: &[u8]) -> Result<(), Error> {
        self.content_flags |= flags::HAS_SINGLE_DELETE;
        Ok(())
    }
    fn delete_range_cf(&mut self, _: u32, _: &[u8], _: &[u8]) -> Result<(), Error> {
        self.content_flags |= flags::HAS_DELETE_RANGE;
        Ok(())
    }
    fn merge_cf(&mut self, _: u32, _: &[u8], _: &[u8]) -> Result<(), Error> {
        self.content_flags |= flags::HAS_MERGE;
        Ok(())
    }
    fn put_blob_index_cf(&mut self, _: u32, _: &[u8], _: &[u8]) -> Result<(), Error> {
        self.content_flags |= flags::HAS_BLOB_INDEX;
        Ok(())
    }
    fn mark_begin_prepare(&mut self, unprepare: bool) -> Result<(), Error> {
        self.content_flags |= flags::HAS_BEGIN_PREPARE;
        if unprepare {
            self.content_flags |= flags::HAS_BEGIN_UNPREPARE;
        }
        Ok(())
    }
    fn mark_end_prepare(&mut self, _: &[u8]) -> Result<(), Error> {
        self.content_flags |= flags::HAS_END_PREPARE;
        Ok(())
    }
    fn mark_commit(&mut self, _: &[u8]) -> Result<(), Error> {
        self.content_flags |= flags::HAS_COMMIT;
        Ok(())
    }
    fn mark_commit_with_timestamp(&mut self, _: &[u8], _: &[u8]) -> Result<(), Error> {
        self.content_flags |= flags::HAS_COMMIT;
        Ok(())
    }
    fn mark_rollback(&mut self, _: &[u8]) -> Result<(), Error> {
        self.content_flags |= flags::HAS_ROLLBACK;
        Ok(())
    }
    // RocksDB's classifier leaves MarkNoop at the handler default, which fails; its
    // `PermitUncheckedError` then keeps the flags gathered so far. A Noop carries no content,
    // so the port's classifier accepts it and reads on.
    fn mark_noop(&mut self, _: bool) -> Result<(), Error> {
        Ok(())
    }
}

/// `WriteBatch` [R include/rocksdb/write_batch.h; db/write_batch.cc].
#[derive(Debug)]
pub struct WriteBatch {
    /// The encoded batch; never shorter than [`HEADER`].
    rep: Vec<u8>,
    /// `content_flags_`: atomic because `has_*` computes deferred flags through `&self`.
    content_flags: AtomicU32,
    /// `max_bytes_`: 0 is unlimited.
    max_bytes: usize,
    save_points: Vec<SavePoint>,
    wal_term_point: Option<SavePoint>,
}

impl Default for WriteBatch {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for WriteBatch {
    fn clone(&self) -> Self {
        Self {
            rep: self.rep.clone(),
            content_flags: AtomicU32::new(self.content_flags.load(Ordering::Relaxed)),
            max_bytes: self.max_bytes,
            save_points: self.save_points.clone(),
            wal_term_point: self.wal_term_point,
        }
    }
}

impl PartialEq for WriteBatch {
    /// Batches are equal when their bytes are.
    fn eq(&self, other: &Self) -> bool {
        self.rep == other.rep
    }
}

impl WriteBatch {
    /// An empty batch: a zero header.
    pub fn new() -> Self {
        Self::with_max_bytes(0)
    }

    /// An empty batch refusing a record that would take it past `max_bytes` (0: no limit)
    /// [R db/write_batch.cc:177-189].
    pub fn with_max_bytes(max_bytes: usize) -> Self {
        Self {
            rep: vec![0; HEADER],
            content_flags: AtomicU32::new(0),
            max_bytes,
            save_points: Vec::new(),
            wal_term_point: None,
        }
    }

    /// `WriteBatch(std::string&& rep)` [R db/write_batch.cc:191-197]: a batch over encoded
    /// bytes, which are kept, not copied. Bytes shorter than the header are refused where
    /// RocksDB would read past them.
    pub fn from_rep(rep: Vec<u8>) -> Result<Self, Error> {
        if rep.len() < HEADER {
            return Err(Error::truncated("WriteBatch header"));
        }
        Ok(Self {
            rep,
            content_flags: AtomicU32::new(flags::DEFERRED),
            max_bytes: 0,
            save_points: Vec::new(),
            wal_term_point: None,
        })
    }

    /// `Data`: the encoded batch.
    pub fn data(&self) -> &[u8] {
        &self.rep
    }

    /// `GetDataSize`.
    pub fn data_size(&self) -> usize {
        self.rep.len()
    }

    /// `Release`: the encoded batch, leaving this one cleared.
    pub fn release(&mut self) -> Vec<u8> {
        let ret = std::mem::replace(&mut self.rep, vec![0; HEADER]);
        self.clear();
        ret
    }

    /// `Clear` [R db/write_batch.cc:248-266].
    pub fn clear(&mut self) {
        self.rep.clear();
        self.rep.resize(HEADER, 0);
        self.content_flags.store(0, Ordering::Relaxed);
        self.save_points.clear();
        self.wal_term_point = None;
    }

    /// `Count`: the header's record count.
    pub fn count(&self) -> u32 {
        self.rep
            .get(COUNT_OFFSET..HEADER)
            .and_then(|b| decode_fixed32(b).ok())
            .unwrap_or(0)
    }

    /// `WriteBatchInternal::SetCount`.
    pub fn set_count(&mut self, n: u32) {
        write_at(&mut self.rep, COUNT_OFFSET, &encode_fixed32(n));
    }

    /// `WriteBatchInternal::Sequence`.
    pub fn sequence(&self) -> SequenceNumber {
        self.rep
            .get(..COUNT_OFFSET)
            .and_then(|b| decode_fixed64(b).ok())
            .unwrap_or(0)
    }

    /// `WriteBatchInternal::SetSequence`.
    pub fn set_sequence(&mut self, seq: SequenceNumber) {
        write_at(&mut self.rep, 0, &encode_fixed64(seq));
    }

    /// `WriteBatchInternal::SetContents` [R db/write_batch.cc:3464-3471]: replaces the bytes.
    pub fn set_contents(&mut self, contents: &[u8]) -> Result<(), Error> {
        if contents.len() < HEADER {
            return Err(Error::truncated("WriteBatch header"));
        }
        self.rep.clear();
        self.rep.extend_from_slice(contents);
        self.content_flags.store(flags::DEFERRED, Ordering::Relaxed);
        Ok(())
    }

    fn compute_content_flags(&self) -> u32 {
        let rv = self.content_flags.load(Ordering::Relaxed);
        if rv & flags::DEFERRED == 0 {
            return rv;
        }
        let mut classifier = ContentClassifier::default();
        // As RocksDB, a batch that fails to iterate keeps the flags gathered before the
        // failure; the failure itself surfaces to whoever iterates to apply the batch.
        let _ = self.iterate(&mut classifier);
        self.content_flags
            .store(classifier.content_flags, Ordering::Relaxed);
        classifier.content_flags
    }

    fn has(&self, flag: u32) -> bool {
        self.compute_content_flags() & flag != 0
    }

    pub fn has_put(&self) -> bool {
        self.has(flags::HAS_PUT)
    }
    pub fn has_timed_put(&self) -> bool {
        self.has(flags::HAS_TIMED_PUT)
    }
    pub fn has_put_entity(&self) -> bool {
        self.has(flags::HAS_PUT_ENTITY)
    }
    pub fn has_delete(&self) -> bool {
        self.has(flags::HAS_DELETE)
    }
    pub fn has_single_delete(&self) -> bool {
        self.has(flags::HAS_SINGLE_DELETE)
    }
    pub fn has_delete_range(&self) -> bool {
        self.has(flags::HAS_DELETE_RANGE)
    }
    pub fn has_merge(&self) -> bool {
        self.has(flags::HAS_MERGE)
    }
    pub fn has_blob_index(&self) -> bool {
        self.has(flags::HAS_BLOB_INDEX)
    }
    pub fn has_begin_prepare(&self) -> bool {
        self.has(flags::HAS_BEGIN_PREPARE)
    }
    pub fn has_end_prepare(&self) -> bool {
        self.has(flags::HAS_END_PREPARE)
    }
    pub fn has_commit(&self) -> bool {
        self.has(flags::HAS_COMMIT)
    }
    pub fn has_rollback(&self) -> bool {
        self.has(flags::HAS_ROLLBACK)
    }

    fn add_flags(&self, f: u32) {
        let old = self.content_flags.load(Ordering::Relaxed);
        self.content_flags.store(old | f, Ordering::Relaxed);
    }

    fn save_point(&self) -> SavePoint {
        SavePoint {
            size: self.rep.len(),
            count: self.count(),
            content_flags: self.content_flags.load(Ordering::Relaxed),
        }
    }

    fn restore(&mut self, sp: SavePoint) {
        self.rep.truncate(sp.size);
        self.set_count(sp.count);
        self.content_flags
            .store(sp.content_flags, Ordering::Relaxed);
    }

    /// Appends one data record: `LocalSavePoint`, the count increment, the tag with the column
    /// family when not 0, the payload, the flag, and `LocalSavePoint::commit`'s size check
    /// [R db/write_batch_internal.h:300-335].
    fn append_record(
        &mut self,
        cf: u32,
        default_tag: ValueType,
        cf_tag: ValueType,
        flag: u32,
        payload: impl FnOnce(&mut Vec<u8>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let sp = self.save_point();
        let count = sp.count.checked_add(1).ok_or(Error::LimitExceeded {
            what: "WriteBatch record count",
            limit: u64::from(u32::MAX),
        })?;
        if cf == 0 {
            self.rep.push(default_tag.as_u8());
        } else {
            self.rep.push(cf_tag.as_u8());
            put_varint32(&mut self.rep, cf);
        }
        if let Err(e) = payload(&mut self.rep) {
            self.restore(sp);
            return Err(e);
        }
        self.set_count(count);
        self.add_flags(flag);
        self.commit_local(sp)
    }

    /// `LocalSavePoint::commit`: past `max_bytes`, the record is taken back and refused.
    fn commit_local(&mut self, sp: SavePoint) -> Result<(), Error> {
        if self.max_bytes != 0 && self.rep.len() > self.max_bytes {
            self.restore(sp);
            return Err(Error::LimitExceeded {
                what: "WriteBatch size in bytes",
                limit: u64::try_from(self.max_bytes).unwrap_or(u64::MAX),
            });
        }
        Ok(())
    }

    /// `Put` to the default column family.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), Error> {
        self.put_cf(0, key, value)
    }

    /// `WriteBatchInternal::Put` [R db/write_batch.cc:858-892].
    pub fn put_cf(&mut self, cf: u32, key: &[u8], value: &[u8]) -> Result<(), Error> {
        check_key(key)?;
        check_value(value)?;
        self.append_record(
            cf,
            ValueType::Value,
            ValueType::ColumnFamilyValue,
            flags::HAS_PUT,
            |rep| {
                put_length_prefixed_slice(rep, key)?;
                put_length_prefixed_slice(rep, value)
            },
        )
    }

    /// `Put(SliceParts, SliceParts)` [R db/write_batch.cc:1003-1050]: key and value each the
    /// concatenation of their parts.
    pub fn put_parts(&mut self, cf: u32, key: &[&[u8]], value: &[&[u8]]) -> Result<(), Error> {
        check_key_len(parts_len(key)?)?;
        check_value_len(parts_len(value)?)?;
        self.append_record(
            cf,
            ValueType::Value,
            ValueType::ColumnFamilyValue,
            flags::HAS_PUT,
            |rep| {
                put_length_prefixed_slice_parts(rep, key)?;
                put_length_prefixed_slice_parts(rep, value)
            },
        )
    }

    /// `WriteBatchInternal::TimedPut` [R db/write_batch.cc:894-934]. A write time of
    /// `u64::MAX` means "unknown" and is written as a plain Put.
    pub fn timed_put(
        &mut self,
        cf: u32,
        key: &[u8],
        value: &[u8],
        write_unix_time: u64,
    ) -> Result<(), Error> {
        check_key(key)?;
        check_value(value)?;
        if write_unix_time == u64::MAX {
            return self.put_cf(cf, key, value);
        }
        let packed = pack_value_and_write_time(value, write_unix_time);
        self.append_record(
            cf,
            ValueType::ValuePreferredSeqno,
            ValueType::ColumnFamilyValuePreferredSeqno,
            flags::HAS_TIMED_PUT,
            |rep| {
                put_length_prefixed_slice(rep, key)?;
                put_length_prefixed_slice(rep, &packed)
            },
        )
    }

    /// `WriteBatchInternal::PutEntity` [R db/write_batch.cc:1054-1084]: the columns sorted by
    /// name and serialized (version 1).
    pub fn put_entity(
        &mut self,
        cf: u32,
        key: &[u8],
        columns: &[WideColumn<'_>],
    ) -> Result<(), Error> {
        check_key(key)?;
        let mut sorted = columns.to_vec();
        sort_columns(&mut sorted);
        let mut entity = Vec::new();
        serialize(&sorted, &mut entity)?;
        self.put_entity_serialized(cf, key, &entity)
    }

    /// `PutEntity(key, AttributeGroups)` [R db/write_batch.cc:1150-1165]: one entity record per
    /// group, each in its column family. Groups before a refused one stay in the batch, as in
    /// RocksDB.
    pub fn put_entity_attribute_groups(
        &mut self,
        key: &[u8],
        groups: &[AttributeGroup<'_>],
    ) -> Result<(), Error> {
        if groups.is_empty() {
            return Err(Error::InvalidArgument {
                what: "Cannot call this method with empty attribute groups",
            });
        }
        for group in groups {
            self.put_entity(group.column_family, key, &group.columns)?;
        }
        Ok(())
    }

    /// `WriteBatchInternal::PutEntitySerialized` [R db/write_batch.cc:1086-1123]: an entity
    /// already serialized.
    pub fn put_entity_serialized(
        &mut self,
        cf: u32,
        key: &[u8],
        entity: &[u8],
    ) -> Result<(), Error> {
        if key.len() > MAX_VALUE_SIZE {
            return Err(Error::InvalidArgument {
                what: "key is too large",
            });
        }
        if entity.len() > MAX_VALUE_SIZE {
            return Err(Error::InvalidArgument {
                what: "wide column entity is too large",
            });
        }
        self.append_record(
            cf,
            ValueType::WideColumnEntity,
            ValueType::ColumnFamilyWideColumnEntity,
            flags::HAS_PUT_ENTITY,
            |rep| {
                put_length_prefixed_slice(rep, key)?;
                put_length_prefixed_slice(rep, entity)
            },
        )
    }

    /// `Delete` from the default column family.
    pub fn delete(&mut self, key: &[u8]) -> Result<(), Error> {
        self.delete_cf(0, key)
    }

    /// `WriteBatchInternal::Delete` [R db/write_batch.cc:1281-1307].
    pub fn delete_cf(&mut self, cf: u32, key: &[u8]) -> Result<(), Error> {
        check_key(key)?;
        self.append_record(
            cf,
            ValueType::Deletion,
            ValueType::ColumnFamilyDeletion,
            flags::HAS_DELETE,
            |rep| put_length_prefixed_slice(rep, key),
        )
    }

    /// `SingleDelete` from the default column family.
    pub fn single_delete(&mut self, key: &[u8]) -> Result<(), Error> {
        self.single_delete_cf(0, key)
    }

    /// `WriteBatchInternal::SingleDelete` [R db/write_batch.cc:1413-1441].
    pub fn single_delete_cf(&mut self, cf: u32, key: &[u8]) -> Result<(), Error> {
        check_key(key)?;
        self.append_record(
            cf,
            ValueType::SingleDeletion,
            ValueType::ColumnFamilySingleDeletion,
            flags::HAS_SINGLE_DELETE,
            |rep| put_length_prefixed_slice(rep, key),
        )
    }

    /// `DeleteRange` in the default column family.
    pub fn delete_range(&mut self, begin: &[u8], end: &[u8]) -> Result<(), Error> {
        self.delete_range_cf(0, begin, end)
    }

    /// `WriteBatchInternal::DeleteRange` [R db/write_batch.cc:1549-1581].
    pub fn delete_range_cf(&mut self, cf: u32, begin: &[u8], end: &[u8]) -> Result<(), Error> {
        check_key(begin)?;
        if end.len() > MAX_KEY_SIZE {
            return Err(Error::InvalidArgument {
                what: "end key is too large",
            });
        }
        self.append_record(
            cf,
            ValueType::RangeDeletion,
            ValueType::ColumnFamilyRangeDeletion,
            flags::HAS_DELETE_RANGE,
            |rep| {
                put_length_prefixed_slice(rep, begin)?;
                put_length_prefixed_slice(rep, end)
            },
        )
    }

    /// `Merge` into the default column family.
    pub fn merge(&mut self, key: &[u8], value: &[u8]) -> Result<(), Error> {
        self.merge_cf(0, key, value)
    }

    /// `WriteBatchInternal::Merge` [R db/write_batch.cc:1695-1725].
    pub fn merge_cf(&mut self, cf: u32, key: &[u8], value: &[u8]) -> Result<(), Error> {
        check_key(key)?;
        check_value(value)?;
        self.append_record(
            cf,
            ValueType::Merge,
            ValueType::ColumnFamilyMerge,
            flags::HAS_MERGE,
            |rep| {
                put_length_prefixed_slice(rep, key)?;
                put_length_prefixed_slice(rep, value)
            },
        )
    }

    /// `WriteBatchInternal::PutBlobIndex` [R db/write_batch.cc:1833-1858]. RocksDB checks no
    /// size here; the varint32 length prefixes refuse what they cannot state.
    pub fn put_blob_index(&mut self, cf: u32, key: &[u8], value: &[u8]) -> Result<(), Error> {
        self.append_record(
            cf,
            ValueType::BlobIndex,
            ValueType::ColumnFamilyBlobIndex,
            flags::HAS_BLOB_INDEX,
            |rep| {
                put_length_prefixed_slice(rep, key)?;
                put_length_prefixed_slice(rep, value)
            },
        )
    }

    /// `PutLogData` [R db/write_batch.cc:1860-1865]: a blob the WAL keeps and the memtable
    /// never sees; not counted.
    pub fn put_log_data(&mut self, blob: &[u8]) -> Result<(), Error> {
        let sp = self.save_point();
        self.rep.push(ValueType::LogData.as_u8());
        if let Err(e) = put_length_prefixed_slice(&mut self.rep, blob) {
            self.restore(sp);
            return Err(e);
        }
        self.commit_local(sp)
    }

    /// `WriteBatchInternal::InsertNoop` [R db/write_batch.cc:1190-1193].
    pub fn insert_noop(&mut self) {
        self.rep.push(ValueType::Noop.as_u8());
    }

    /// `WriteBatchInternal::GetBeginPrepareType` [R db/write_batch.cc:1195-1201].
    pub const fn begin_prepare_type(write_after_commit: bool, unprepared_batch: bool) -> ValueType {
        if write_after_commit {
            ValueType::BeginPrepareXid
        } else if unprepared_batch {
            ValueType::BeginUnprepareXid
        } else {
            ValueType::BeginPersistedPrepareXid
        }
    }

    /// `WriteBatchInternal::MarkEndPrepare` [R db/write_batch.cc:1223-1248]: turns the Noop
    /// the batch begins with into a BeginPrepare and appends an EndPrepare. Written by
    /// RocksDB's transactions (P17); a batch not beginning with a Noop is refused where
    /// RocksDB asserts.
    pub fn mark_end_prepare(
        &mut self,
        xid: &[u8],
        write_after_commit: bool,
        unprepared_batch: bool,
    ) -> Result<(), Error> {
        if self.rep.get(HEADER) != Some(&ValueType::Noop.as_u8()) {
            return Err(Error::InvalidArgument {
                what: "MarkEndPrepare needs a batch that begins with a Noop",
            });
        }
        self.save_points.clear();
        let tag = Self::begin_prepare_type(write_after_commit, unprepared_batch);
        write_at(&mut self.rep, HEADER, &[tag.as_u8()]);
        self.add_flags(flags::HAS_BEGIN_PREPARE);
        if unprepared_batch {
            self.add_flags(flags::HAS_BEGIN_UNPREPARE);
        }
        self.rep.push(ValueType::EndPrepareXid.as_u8());
        put_length_prefixed_slice(&mut self.rep, xid)?;
        self.add_flags(flags::HAS_END_PREPARE);
        Ok(())
    }

    /// `WriteBatchInternal::MarkCommit` [R db/write_batch.cc:1250-1257].
    pub fn mark_commit(&mut self, xid: &[u8]) -> Result<(), Error> {
        self.rep.push(ValueType::CommitXid.as_u8());
        put_length_prefixed_slice(&mut self.rep, xid)?;
        self.add_flags(flags::HAS_COMMIT);
        Ok(())
    }

    /// `WriteBatchInternal::MarkCommitWithTimestamp` [R db/write_batch.cc:1259-1270]. RocksDB
    /// asserts the timestamp is not empty; an empty one is refused.
    pub fn mark_commit_with_timestamp(
        &mut self,
        xid: &[u8],
        commit_ts: &[u8],
    ) -> Result<(), Error> {
        if commit_ts.is_empty() {
            return Err(Error::InvalidArgument {
                what: "commit timestamp is empty",
            });
        }
        self.rep.push(ValueType::CommitXidAndTimestamp.as_u8());
        put_length_prefixed_slice(&mut self.rep, commit_ts)?;
        put_length_prefixed_slice(&mut self.rep, xid)?;
        self.add_flags(flags::HAS_COMMIT);
        Ok(())
    }

    /// `WriteBatchInternal::MarkRollback` [R db/write_batch.cc:1272-1279].
    pub fn mark_rollback(&mut self, xid: &[u8]) -> Result<(), Error> {
        self.rep.push(ValueType::RollbackXid.as_u8());
        put_length_prefixed_slice(&mut self.rep, xid)?;
        self.add_flags(flags::HAS_ROLLBACK);
        Ok(())
    }

    /// `SetSavePoint` [R db/write_batch.cc:1867-1874], refused past [`MAX_SAVE_POINTS`].
    pub fn set_save_point(&mut self) -> Result<(), Error> {
        if self.save_points.len() >= MAX_SAVE_POINTS {
            return Err(Error::LimitExceeded {
                what: "WriteBatch save points",
                limit: MAX_SAVE_POINTS as u64,
            });
        }
        let sp = self.save_point();
        self.save_points.push(sp);
        Ok(())
    }

    /// `RollbackToSavePoint` [R db/write_batch.cc:1876-1903]: `Ok(false)` where RocksDB returns
    /// `NotFound` because no save point is set.
    pub fn rollback_to_save_point(&mut self) -> Result<bool, Error> {
        let Some(sp) = self.save_points.pop() else {
            return Ok(false);
        };
        if sp.size > self.rep.len() || sp.count > self.count() {
            return Err(Error::corruption(
                "WriteBatch save point",
                Malformed::CountMismatch,
            ));
        }
        if sp.size == self.rep.len() {
            // Nothing to roll back.
        } else if sp.size == 0 {
            self.clear();
        } else {
            self.restore(sp);
        }
        Ok(true)
    }

    /// `PopSavePoint` [R db/write_batch.cc:1905-1914]: `false` where RocksDB returns
    /// `NotFound`.
    pub fn pop_save_point(&mut self) -> bool {
        self.save_points.pop().is_some()
    }

    /// `MarkWalTerminationPoint` [R db/write_batch.cc:280-284]: what an append with `wal_only`
    /// takes of this batch.
    pub fn mark_wal_termination_point(&mut self) {
        self.wal_term_point = Some(self.save_point());
    }

    /// `WriteBatchInternal::Append` [R db/write_batch.cc:3473-3517]: `src`'s records after
    /// `dst`'s, up to `src`'s WAL termination point when `wal_only` and one is set.
    pub fn append(dst: &mut Self, src: &Self, wal_only: bool) -> Result<(), Error> {
        let (src_end, src_count, src_flags) = match (wal_only, src.wal_term_point) {
            (true, Some(p)) => (p.size, p.count, p.content_flags),
            _ => (
                src.rep.len(),
                src.count(),
                src.content_flags.load(Ordering::Relaxed),
            ),
        };
        let records = src
            .rep
            .get(HEADER..src_end)
            .ok_or(Error::truncated("WriteBatch termination point"))?;
        let count = dst
            .count()
            .checked_add(src_count)
            .ok_or(Error::LimitExceeded {
                what: "WriteBatch record count",
                limit: u64::from(u32::MAX),
            })?;
        dst.set_count(count);
        dst.rep.extend_from_slice(records);
        dst.add_flags(src_flags);
        Ok(())
    }

    /// `WriteBatchInternal::AppendedByteSize` [R db/write_batch.cc:3519-3526].
    pub fn appended_byte_size(left: usize, right: usize) -> usize {
        if left == 0 || right == 0 {
            left.saturating_add(right)
        } else {
            left.saturating_add(right).saturating_sub(HEADER)
        }
    }

    /// `Iterate` [R db/write_batch.cc:517-524]: every record to `handler`, in order.
    pub fn iterate<H: Handler + ?Sized>(&self, handler: &mut H) -> Result<(), Error> {
        if self.rep.len() < HEADER {
            return Err(Error::truncated("WriteBatch"));
        }
        self.iterate_range(handler, HEADER, self.rep.len())
    }

    /// `WriteBatchInternal::Iterate` [R db/write_batch.cc:526-764]: the records in
    /// `rep[begin..end]`. The count is checked only over the whole batch, and only when the
    /// handler did not stop early.
    pub fn iterate_range<H: Handler + ?Sized>(
        &self,
        handler: &mut H,
        begin: usize,
        end: usize,
    ) -> Result<(), Error> {
        let mut input = self.rep.get(begin..end).ok_or(Error::corruption(
            "WriteBatch Iterate bounds",
            Malformed::TooLarge,
        ))?;
        let whole_batch = begin == HEADER && end == self.rep.len();
        let mut empty_batch = true;
        let mut found: u32 = 0;
        let mut handler_continue = true;
        // A record the handler answered `Duplicate` (RocksDB's TryAgain) is offered once more;
        // two in a row are corruption [R db/write_batch.cc:543-574].
        let mut pending: Option<Record<'_>> = None;
        let mut last_was_try_again = false;
        while !input.is_empty() || pending.is_some() {
            handler_continue = handler.should_continue();
            if !handler_continue {
                break;
            }
            let record = match pending.take() {
                Some(r) => {
                    if last_was_try_again {
                        return Err(Error::corruption(
                            "WriteBatch handler",
                            Malformed::CountMismatch,
                        ));
                    }
                    last_was_try_again = true;
                    r
                }
                None => {
                    last_was_try_again = false;
                    read_record(&mut input)?
                }
            };
            let (result, counted, empties) = dispatch(handler, record, empty_batch);
            match result {
                Ok(()) => {
                    if counted {
                        found = found.saturating_add(1);
                    }
                    if let Some(e) = empties {
                        empty_batch = e;
                    }
                }
                Err(Error::Duplicate) => pending = Some(record),
                Err(e) => return Err(e),
            }
        }
        if handler_continue && whole_batch && found != self.count() {
            return Err(Error::corruption("WriteBatch", Malformed::CountMismatch));
        }
        Ok(())
    }
}

/// Offers one record to the handler: its result, whether it counts toward the batch's count,
/// and what `empty_batch` becomes when it succeeds [R db/write_batch.cc:576-751].
fn dispatch<H: Handler + ?Sized>(
    handler: &mut H,
    record: Record<'_>,
    empty_batch: bool,
) -> (Result<(), Error>, bool, Option<bool>) {
    match record {
        Record::Put { cf, key, value } => (handler.put_cf(cf, key, value), true, Some(false)),
        Record::TimedPut {
            cf,
            key,
            value,
            write_unix_time,
        } => (
            handler.timed_put_cf(cf, key, value, write_unix_time),
            true,
            Some(false),
        ),
        Record::Delete { cf, key } => (handler.delete_cf(cf, key), true, Some(false)),
        Record::SingleDelete { cf, key } => (handler.single_delete_cf(cf, key), true, Some(false)),
        Record::DeleteRange { cf, begin, end } => {
            (handler.delete_range_cf(cf, begin, end), true, Some(false))
        }
        Record::Merge { cf, key, value } => (handler.merge_cf(cf, key, value), true, Some(false)),
        Record::BlobIndex { cf, key, value } => {
            (handler.put_blob_index_cf(cf, key, value), true, Some(false))
        }
        Record::PutEntity { cf, key, entity } => {
            (handler.put_entity_cf(cf, key, entity), true, Some(false))
        }
        Record::LogData { blob } => {
            handler.log_data(blob);
            (Ok(()), false, Some(false))
        }
        Record::BeginPrepare { tag } => {
            let unprepare = tag == ValueType::BeginUnprepareXid;
            let mut s = handler.mark_begin_prepare(unprepare);
            if s.is_ok() {
                let wac = handler.write_after_commit();
                let wbp = handler.write_before_prepare();
                let mismatch = match tag {
                    ValueType::BeginPrepareXid => {
                        wac == OptionState::Disabled || wbp == OptionState::Enabled
                    }
                    ValueType::BeginPersistedPrepareXid => wac == OptionState::Enabled,
                    _ => wac == OptionState::Enabled || wbp == OptionState::Disabled,
                };
                if mismatch {
                    s = Err(write_policy_mismatch(tag));
                }
            }
            (s, false, Some(false))
        }
        Record::EndPrepare { xid } => (handler.mark_end_prepare(xid), false, Some(true)),
        Record::Commit { xid } => (handler.mark_commit(xid), false, Some(true)),
        Record::CommitWithTimestamp { xid, commit_ts } => (
            handler.mark_commit_with_timestamp(xid, commit_ts),
            false,
            Some(true),
        ),
        Record::Rollback { xid } => (handler.mark_rollback(xid), false, Some(true)),
        Record::Noop => (handler.mark_noop(empty_batch), false, Some(true)),
    }
}

/// `MemTableInserter` [R db/write_batch.cc:2024-3257] over one memtable, the default column
/// family's (`ColumnFamilyMemTablesDefault` [R db/write_batch_internal.h:44-66]): each data
/// record is added at the next sequence number, one per record, as RocksDB does without
/// `seq_per_batch`. The inserter borrows the memtable for as long as it writes, which is the
/// single-writer rule of docs/research/24 §4.1 stated in the types.
///
/// Refused, where RocksDB applies them: a record for another column family ("Invalid column
/// family specified in write batch", as RocksDB without `ignore_missing_column_families`), a
/// Merge when no merge operator is configured (RocksDB's own refusal), and every two-phase
/// commit marker (docs/research/24 §1.4 DECISION: they come only from transactions, P17, whose
/// recovery logic applying them needs). A Noop is accepted, as RocksDB accepts it.
pub struct MemTableInserter<'m> {
    mem: &'m mut MemTable,
    sequence: SequenceNumber,
    /// Whether the column family has a merge operator (`moptions->merge_operator != nullptr`).
    /// The operator itself runs on reads and compactions (P12); inserting needs only this.
    has_merge_operator: bool,
}

impl<'m> MemTableInserter<'m> {
    pub fn new(mem: &'m mut MemTable, sequence: SequenceNumber, has_merge_operator: bool) -> Self {
        Self {
            mem,
            sequence,
            has_merge_operator,
        }
    }

    /// The sequence number the next record would take.
    pub fn sequence(&self) -> SequenceNumber {
        self.sequence
    }

    /// `PutCFImpl`/`DeleteImpl`: the add, then `MaybeAdvanceSeq` on success.
    fn add(&mut self, cf: u32, t: ValueType, key: &[u8], value: &[u8]) -> Result<(), Error> {
        if cf != 0 {
            return Err(Error::InvalidArgument {
                what: "Invalid column family specified in write batch",
            });
        }
        self.mem.add(self.sequence, t, key, value)?;
        // The add refused any sequence number above 2^56 - 1, so this does not overflow.
        self.sequence = self.sequence.saturating_add(1);
        Ok(())
    }
}

/// The refusal of a two-phase-commit marker applied to a memtable.
const fn two_phase_commit(tag: ValueType) -> Error {
    Error::Unsupported {
        feature: "two-phase commit",
        value: tag as u64,
    }
}

impl Handler for MemTableInserter<'_> {
    fn put_cf(&mut self, cf: u32, key: &[u8], value: &[u8]) -> Result<(), Error> {
        self.add(cf, ValueType::Value, key, value)
    }
    fn timed_put_cf(
        &mut self,
        cf: u32,
        key: &[u8],
        value: &[u8],
        write_unix_time: u64,
    ) -> Result<(), Error> {
        let packed = pack_value_and_write_time(value, write_unix_time);
        self.add(cf, ValueType::ValuePreferredSeqno, key, &packed)
    }
    fn put_entity_cf(&mut self, cf: u32, key: &[u8], entity: &[u8]) -> Result<(), Error> {
        self.add(cf, ValueType::WideColumnEntity, key, entity)
    }
    fn delete_cf(&mut self, cf: u32, key: &[u8]) -> Result<(), Error> {
        self.add(cf, ValueType::Deletion, key, &[])
    }
    fn single_delete_cf(&mut self, cf: u32, key: &[u8]) -> Result<(), Error> {
        self.add(cf, ValueType::SingleDeletion, key, &[])
    }
    /// Without a DB behind it, RocksDB's inserter does not compare the range's ends
    /// [R db/write_batch.cc:2712-2735]; the write path that has one (P7) does.
    fn delete_range_cf(&mut self, cf: u32, begin: &[u8], end: &[u8]) -> Result<(), Error> {
        self.add(cf, ValueType::RangeDeletion, begin, end)
    }
    fn merge_cf(&mut self, cf: u32, key: &[u8], value: &[u8]) -> Result<(), Error> {
        if cf == 0 && !self.has_merge_operator {
            return Err(Error::InvalidArgument {
                what: "Merge requires `ColumnFamilyOptions::merge_operator != nullptr`",
            });
        }
        self.add(cf, ValueType::Merge, key, value)
    }
    fn put_blob_index_cf(&mut self, cf: u32, key: &[u8], value: &[u8]) -> Result<(), Error> {
        self.add(cf, ValueType::BlobIndex, key, value)
    }
    fn mark_noop(&mut self, _empty_batch: bool) -> Result<(), Error> {
        Ok(())
    }
    fn mark_begin_prepare(&mut self, unprepare: bool) -> Result<(), Error> {
        Err(two_phase_commit(if unprepare {
            ValueType::BeginUnprepareXid
        } else {
            ValueType::BeginPrepareXid
        }))
    }
    fn mark_end_prepare(&mut self, _xid: &[u8]) -> Result<(), Error> {
        Err(two_phase_commit(ValueType::EndPrepareXid))
    }
    fn mark_commit(&mut self, _xid: &[u8]) -> Result<(), Error> {
        Err(two_phase_commit(ValueType::CommitXid))
    }
    fn mark_commit_with_timestamp(&mut self, _xid: &[u8], _commit_ts: &[u8]) -> Result<(), Error> {
        Err(two_phase_commit(ValueType::CommitXidAndTimestamp))
    }
    fn mark_rollback(&mut self, _xid: &[u8]) -> Result<(), Error> {
        Err(two_phase_commit(ValueType::RollbackXid))
    }
}

impl WriteBatch {
    /// `WriteBatchInternal::InsertInto(batch, memtables, ...)` [R db/write_batch.cc:3327-3343]:
    /// applies the batch to `mem` from its own sequence number, returning the sequence number
    /// after its last record. Records before a refused one stay in the memtable, as in
    /// RocksDB; the caller discards the memtable or the batch's sequence range.
    pub fn insert_into(
        &self,
        mem: &mut MemTable,
        has_merge_operator: bool,
    ) -> Result<SequenceNumber, Error> {
        let mut inserter = MemTableInserter::new(mem, self.sequence(), has_merge_operator);
        self.iterate(&mut inserter)?;
        Ok(inserter.sequence())
    }
}

/// Writes `bytes` over `rep` at `at`; the header's fields always lie inside `rep`.
fn write_at(rep: &mut [u8], at: usize, bytes: &[u8]) {
    let end = at.saturating_add(bytes.len());
    if let Some(dst) = rep.get_mut(at..end) {
        for (d, s) in dst.iter_mut().zip(bytes) {
            *d = *s;
        }
    }
}

fn parts_len(parts: &[&[u8]]) -> Result<usize, Error> {
    parts
        .iter()
        .try_fold(0usize, |sum, p| sum.checked_add(p.len()))
        .ok_or(Error::InvalidArgument {
            what: "key is too large",
        })
}

fn check_key_len(len: usize) -> Result<(), Error> {
    if len > MAX_KEY_SIZE {
        return Err(Error::InvalidArgument {
            what: "key is too large",
        });
    }
    Ok(())
}

fn check_value_len(len: usize) -> Result<(), Error> {
    if len > MAX_VALUE_SIZE {
        return Err(Error::InvalidArgument {
            what: "value is too large",
        });
    }
    Ok(())
}

fn check_key(key: &[u8]) -> Result<(), Error> {
    check_key_len(key.len())
}

fn check_value(value: &[u8]) -> Result<(), Error> {
    check_value_len(value.len())
}
