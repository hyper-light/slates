//! Internal keys: RocksDB's `db/dbformat.h`, `db/dbformat.cc` and `db/lookup_key.h`
//! (docs/research/24 §1.3).
//!
//! An internal key is `user_key ‖ fixed64(sequence << 8 | type)`. Internal keys sort by user key
//! ascending under the user comparator, then by the 8-byte trailer as a u64 descending, so the
//! newest entry for a user key comes first.
//!
//! Where RocksDB asserts a precondition (a sequence number above 2^56 − 1, an internal key under
//! 8 bytes), the port returns a typed error (docs/research/24 §5 R6). Comparison is total: a
//! slice under 8 bytes, which the port never forms because every internal key is checked when
//! it is built or decoded, compares as a user key with a zero trailer.

use std::cmp::Ordering;

use crate::error::{Error, Malformed};
use crate::util::coding::{decode_fixed64, encode_fixed64, put_fixed64, put_varint32};
use crate::util::comparator::Comparator;

/// A sequence number. Only the low 56 bits are stored; see [`MAX_SEQUENCE_NUMBER`].
pub type SequenceNumber = u64;

/// `kMaxSequenceNumber` [R db/dbformat.h:129]: the low 8 bits of the trailer hold the type.
pub const MAX_SEQUENCE_NUMBER: u64 = (1 << 56) - 1;

/// `kDisableGlobalSequenceNumber` [R db/dbformat.h:131-132]: "no global sequence number" for
/// an ingested file.
pub const DISABLE_GLOBAL_SEQUENCE_NUMBER: u64 = u64::MAX;

/// `kNumInternalBytes` [R db/dbformat.h:134]: the trailer's length.
pub const NUM_INTERNAL_BYTES: usize = 8;

/// The width of the type in the packed trailer.
const TYPE_BITS: u32 = 8;

/// `ValueType` [R db/dbformat.h:41-78]: the last component of an internal key, and the tag of a
/// WriteBatch record. The values are on-disk format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum ValueType {
    Deletion = 0x00,
    Value = 0x01,
    Merge = 0x02,
    LogData = 0x03,
    ColumnFamilyDeletion = 0x04,
    ColumnFamilyValue = 0x05,
    ColumnFamilyMerge = 0x06,
    SingleDeletion = 0x07,
    ColumnFamilySingleDeletion = 0x08,
    BeginPrepareXid = 0x09,
    EndPrepareXid = 0x0A,
    CommitXid = 0x0B,
    RollbackXid = 0x0C,
    Noop = 0x0D,
    ColumnFamilyRangeDeletion = 0x0E,
    RangeDeletion = 0x0F,
    ColumnFamilyBlobIndex = 0x10,
    BlobIndex = 0x11,
    BeginPersistedPrepareXid = 0x12,
    BeginUnprepareXid = 0x13,
    DeletionWithTimestamp = 0x14,
    CommitXidAndTimestamp = 0x15,
    WideColumnEntity = 0x16,
    ColumnFamilyWideColumnEntity = 0x17,
    ValuePreferredSeqno = 0x18,
    ColumnFamilyValuePreferredSeqno = 0x19,
    /// `kTypeMaxValid`: one past the last type; appears only in keys a range-deletion iterator
    /// forms, never stored.
    MaxValid = 0x1A,
    /// `kMaxValue`: never stored.
    MaxValue = 0x7F,
}

/// `kValueTypeForSeek` [R db/dbformat.cc:28]: the highest type a stored key carries, so a seek
/// key with it sorts before every entry of its user key and sequence number.
pub const VALUE_TYPE_FOR_SEEK: ValueType = ValueType::ValuePreferredSeqno;

/// `kValueTypeForSeekForPrev` [R db/dbformat.cc:29].
pub const VALUE_TYPE_FOR_SEEK_FOR_PREV: ValueType = ValueType::Deletion;

impl ValueType {
    /// The type a byte names, or `None` for a byte no type has.
    pub const fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            0x00 => Self::Deletion,
            0x01 => Self::Value,
            0x02 => Self::Merge,
            0x03 => Self::LogData,
            0x04 => Self::ColumnFamilyDeletion,
            0x05 => Self::ColumnFamilyValue,
            0x06 => Self::ColumnFamilyMerge,
            0x07 => Self::SingleDeletion,
            0x08 => Self::ColumnFamilySingleDeletion,
            0x09 => Self::BeginPrepareXid,
            0x0A => Self::EndPrepareXid,
            0x0B => Self::CommitXid,
            0x0C => Self::RollbackXid,
            0x0D => Self::Noop,
            0x0E => Self::ColumnFamilyRangeDeletion,
            0x0F => Self::RangeDeletion,
            0x10 => Self::ColumnFamilyBlobIndex,
            0x11 => Self::BlobIndex,
            0x12 => Self::BeginPersistedPrepareXid,
            0x13 => Self::BeginUnprepareXid,
            0x14 => Self::DeletionWithTimestamp,
            0x15 => Self::CommitXidAndTimestamp,
            0x16 => Self::WideColumnEntity,
            0x17 => Self::ColumnFamilyWideColumnEntity,
            0x18 => Self::ValuePreferredSeqno,
            0x19 => Self::ColumnFamilyValuePreferredSeqno,
            0x1A => Self::MaxValid,
            0x7F => Self::MaxValue,
            _ => return None,
        })
    }

    /// The type's byte.
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// `IsValueType` [R db/dbformat.h:108-112]: a type an entry of a memtable or a data block
    /// carries.
    pub const fn is_value_type(self) -> bool {
        matches!(
            self,
            Self::Deletion
                | Self::Value
                | Self::Merge
                | Self::SingleDeletion
                | Self::BlobIndex
                | Self::DeletionWithTimestamp
                | Self::WideColumnEntity
                | Self::ValuePreferredSeqno
        )
    }

    /// `IsExtendedValueType` [R db/dbformat.h:118-120]: a value type, a range deletion, or the
    /// `MaxValid` a range-deletion iterator's keys carry.
    pub const fn is_extended_value_type(self) -> bool {
        self.is_value_type() || matches!(self, Self::RangeDeletion | Self::MaxValid)
    }
}

/// `PackSequenceAndType` [R db/dbformat.h:181-187]. RocksDB asserts the sequence number fits
/// 56 bits and the type is an extended value type; here either failing is an error.
pub fn pack_sequence_and_type(seq: SequenceNumber, t: ValueType) -> Result<u64, Error> {
    if seq > MAX_SEQUENCE_NUMBER {
        return Err(Error::InvalidArgument {
            what: "sequence number above 2^56 - 1",
        });
    }
    if !t.is_extended_value_type() {
        return Err(Error::InvalidArgument {
            what: "value type not stored in an internal key",
        });
    }
    Ok((seq << TYPE_BITS) | u64::from(t.as_u8()))
}

/// `UnPackSequenceAndType` [R db/dbformat.h:191-200]: the sequence number and the raw type
/// byte, unchecked, as RocksDB leaves them for callers that verify checksums first.
pub const fn unpack_sequence_and_type(packed: u64) -> (SequenceNumber, u8) {
    let [t, ..] = packed.to_le_bytes();
    (packed >> TYPE_BITS, t)
}

/// `kRangeTombstoneSentinel` [R db/dbformat.h:201-202]: `Pack(kMaxSequenceNumber,
/// kTypeRangeDeletion)`, the trailer of a file boundary a range tombstone extends to.
pub const RANGE_TOMBSTONE_SENTINEL: u64 =
    (MAX_SEQUENCE_NUMBER << TYPE_BITS) | ValueType::RangeDeletion as u64;

/// `ParsedInternalKey` [R db/dbformat.h:137-176]: an internal key's three parts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedInternalKey<'a> {
    pub user_key: &'a [u8],
    pub sequence: SequenceNumber,
    pub value_type: ValueType,
}

impl<'a> ParsedInternalKey<'a> {
    pub const fn new(user_key: &'a [u8], sequence: SequenceNumber, value_type: ValueType) -> Self {
        Self {
            user_key,
            sequence,
            value_type,
        }
    }

    /// `InternalKeyEncodingLength` [R db/dbformat.h:176-179].
    pub fn encoding_length(&self) -> usize {
        self.user_key.len().saturating_add(NUM_INTERNAL_BYTES)
    }
}

/// `AppendInternalKey` [R db/dbformat.cc:57-60].
pub fn append_internal_key(result: &mut Vec<u8>, key: &ParsedInternalKey<'_>) -> Result<(), Error> {
    let footer = pack_sequence_and_type(key.sequence, key.value_type)?;
    result.extend_from_slice(key.user_key);
    put_fixed64(result, footer);
    Ok(())
}

/// `AppendInternalKeyFooter` [R db/dbformat.cc:76-79]: appends the trailer to a user key
/// already in `result`.
pub fn append_internal_key_footer(
    result: &mut Vec<u8>,
    s: SequenceNumber,
    t: ValueType,
) -> Result<(), Error> {
    put_fixed64(result, pack_sequence_and_type(s, t)?);
    Ok(())
}

/// `ParseInternalKey` [R db/dbformat.h:519-541]: fails on a key under 8 bytes and on a type
/// that is not an extended value type.
pub fn parse_internal_key(internal_key: &[u8]) -> Result<ParsedInternalKey<'_>, Error> {
    let (user_key, footer) = split_checked(internal_key)?;
    let (sequence, t) = unpack_sequence_and_type(footer);
    match ValueType::from_u8(t) {
        Some(value_type) if value_type.is_extended_value_type() => Ok(ParsedInternalKey {
            user_key,
            sequence,
            value_type,
        }),
        _ => Err(Error::corruption("internal key", Malformed::UnknownTag(t))),
    }
}

/// The user key and the trailer of an internal key of at least 8 bytes.
fn split_checked(internal_key: &[u8]) -> Result<(&[u8], u64), Error> {
    let split = internal_key
        .len()
        .checked_sub(NUM_INTERNAL_BYTES)
        .ok_or(Error::truncated("internal key"))?;
    let (user_key, footer) = internal_key
        .split_at_checked(split)
        .ok_or(Error::truncated("internal key"))?;
    Ok((user_key, decode_fixed64(footer)?))
}

/// The user key and trailer for comparison: a slice under 8 bytes is all user key, trailer 0.
#[inline]
fn split_for_compare(internal_key: &[u8]) -> (&[u8], u64) {
    let split = internal_key.len().saturating_sub(NUM_INTERNAL_BYTES);
    match internal_key.split_at_checked(split) {
        Some((user_key, footer)) => match <[u8; NUM_INTERNAL_BYTES]>::try_from(footer) {
            Ok(bytes) => (user_key, u64::from_le_bytes(bytes)),
            Err(_) => (internal_key, 0),
        },
        None => (internal_key, 0),
    }
}

/// `ExtractUserKey` [R db/dbformat.h:340-343].
pub fn extract_user_key(internal_key: &[u8]) -> Result<&[u8], Error> {
    Ok(split_checked(internal_key)?.0)
}

/// `ExtractInternalKeyFooter` [R db/dbformat.h:376-381].
pub fn extract_internal_key_footer(internal_key: &[u8]) -> Result<u64, Error> {
    Ok(split_checked(internal_key)?.1)
}

/// `GetInternalKeySeqno` [R db/dbformat.h:555-560].
pub fn get_internal_key_seqno(internal_key: &[u8]) -> Result<SequenceNumber, Error> {
    Ok(extract_internal_key_footer(internal_key)? >> TYPE_BITS)
}

/// `UpdateInternalKey` [R db/dbformat.h:543-552]: rewrites the trailer in place.
pub fn update_internal_key(
    ikey: &mut [u8],
    seq: SequenceNumber,
    t: ValueType,
) -> Result<(), Error> {
    let newval = pack_sequence_and_type(seq, t)?;
    let split = ikey
        .len()
        .checked_sub(NUM_INTERNAL_BYTES)
        .ok_or(Error::truncated("internal key"))?;
    let footer = ikey
        .get_mut(split..)
        .ok_or(Error::truncated("internal key"))?;
    for (dst, src) in footer.iter_mut().zip(encode_fixed64(newval)) {
        *dst = src;
    }
    Ok(())
}

/// `PadInternalKeyWithMinTimestamp` [R db/dbformat.cc:118-127]: inserts `ts_sz` zero bytes
/// between the user key and the trailer.
pub fn pad_internal_key_with_min_timestamp(
    result: &mut Vec<u8>,
    key: &[u8],
    ts_sz: usize,
) -> Result<(), Error> {
    let split = key
        .len()
        .checked_sub(NUM_INTERNAL_BYTES)
        .ok_or(Error::truncated("internal key"))?;
    let (user_key, footer) = key
        .split_at_checked(split)
        .ok_or(Error::truncated("internal key"))?;
    result.extend_from_slice(user_key);
    result.resize(result.len().saturating_add(ts_sz), 0);
    result.extend_from_slice(footer);
    Ok(())
}

/// The user key without its last `ts_sz` bytes, and the trailer, of an internal key.
fn split_timestamped(key: &[u8], ts_sz: usize) -> Result<(&[u8], &[u8]), Error> {
    let user_len = key
        .len()
        .checked_sub(NUM_INTERNAL_BYTES)
        .and_then(|n| n.checked_sub(ts_sz))
        .ok_or(Error::truncated("internal key with timestamp"))?;
    let footer_at = key.len().saturating_sub(NUM_INTERNAL_BYTES);
    match (key.get(..user_len), key.get(footer_at..)) {
        (Some(user), Some(footer)) => Ok((user, footer)),
        _ => Err(Error::truncated("internal key with timestamp")),
    }
}

/// `StripTimestampFromInternalKey` [R db/dbformat.cc:140-147].
pub fn strip_timestamp_from_internal_key(
    result: &mut Vec<u8>,
    key: &[u8],
    ts_sz: usize,
) -> Result<(), Error> {
    let (user, footer) = split_timestamped(key, ts_sz)?;
    result.extend_from_slice(user);
    result.extend_from_slice(footer);
    Ok(())
}

/// `ReplaceInternalKeyWithMinTimestamp` [R db/dbformat.cc:149-158].
pub fn replace_internal_key_with_min_timestamp(
    result: &mut Vec<u8>,
    key: &[u8],
    ts_sz: usize,
) -> Result<(), Error> {
    let (user, footer) = split_timestamped(key, ts_sz)?;
    result.extend_from_slice(user);
    result.resize(result.len().saturating_add(ts_sz), 0);
    result.extend_from_slice(footer);
    Ok(())
}

/// `InternalKeyComparator` [R db/dbformat.h:383-428, :1159-1256; db/dbformat.cc:198-241].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InternalKeyComparator {
    user: Comparator,
}

impl InternalKeyComparator {
    pub const fn new(user: Comparator) -> Self {
        Self { user }
    }

    pub const fn user_comparator(&self) -> Comparator {
        self.user
    }

    /// `Compare(Slice, Slice)`: user key ascending, then trailer descending.
    #[inline]
    pub fn compare(&self, a: &[u8], b: &[u8]) -> Ordering {
        let (a_user, a_num) = split_for_compare(a);
        let (b_user, b_num) = split_for_compare(b);
        self.user
            .compare(a_user, b_user)
            .then_with(|| b_num.cmp(&a_num))
    }

    /// `Equal`: `Compare(a, b) == 0`.
    pub fn equal(&self, a: &[u8], b: &[u8]) -> bool {
        self.compare(a, b) == Ordering::Equal
    }

    /// `CompareKeySeq`: as `compare`, without the type.
    pub fn compare_key_seq(&self, a: &[u8], b: &[u8]) -> Ordering {
        let (a_user, a_num) = split_for_compare(a);
        let (b_user, b_num) = split_for_compare(b);
        self.user
            .compare(a_user, b_user)
            .then_with(|| (b_num >> TYPE_BITS).cmp(&(a_num >> TYPE_BITS)))
    }

    /// `Compare(ParsedInternalKey, ParsedInternalKey)`: user key ascending, sequence
    /// descending, type descending.
    pub fn compare_parsed(&self, a: &ParsedInternalKey<'_>, b: &ParsedInternalKey<'_>) -> Ordering {
        self.user
            .compare(a.user_key, b.user_key)
            .then_with(|| b.sequence.cmp(&a.sequence))
            .then_with(|| b.value_type.cmp(&a.value_type))
    }

    /// `Compare(Slice, ParsedInternalKey)`.
    pub fn compare_with_parsed(&self, a: &[u8], b: &ParsedInternalKey<'_>) -> Ordering {
        let (a_user, a_num) = split_for_compare(a);
        let b_num = (b.sequence << TYPE_BITS) | u64::from(b.value_type.as_u8());
        self.user
            .compare(a_user, b.user_key)
            .then_with(|| b_num.cmp(&a_num))
    }

    /// `Compare(a, a_global_seqno, b, b_global_seqno)`: a global sequence number other than
    /// [`DISABLE_GLOBAL_SEQUENCE_NUMBER`] replaces the key's own.
    pub fn compare_with_global_seqno(
        &self,
        a: &[u8],
        a_global_seqno: SequenceNumber,
        b: &[u8],
        b_global_seqno: SequenceNumber,
    ) -> Ordering {
        let footer = |key: &[u8], global: SequenceNumber| {
            let (_, num) = split_for_compare(key);
            if global == DISABLE_GLOBAL_SEQUENCE_NUMBER {
                num
            } else {
                // Only the low 56 bits of a global sequence number are stored, as RocksDB's
                // unchecked `PackSequenceAndType` in release builds leaves them.
                (global << TYPE_BITS) | (num & 0xFF)
            }
        };
        let (a_user, _) = split_for_compare(a);
        let (b_user, _) = split_for_compare(b);
        self.user
            .compare(a_user, b_user)
            .then_with(|| footer(b, b_global_seqno).cmp(&footer(a, a_global_seqno)))
    }
}

/// `InternalKey` [R db/dbformat.h:431-509]: an internal key in its encoded form. Empty means
/// unset.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InternalKey {
    rep: Vec<u8>,
}

impl InternalKey {
    pub fn new(user_key: &[u8], s: SequenceNumber, t: ValueType) -> Result<Self, Error> {
        let mut rep = Vec::new();
        append_internal_key(&mut rep, &ParsedInternalKey::new(user_key, s, t))?;
        Ok(Self { rep })
    }

    /// `SetMaxPossibleForUserKey`: at or after every internal key of `user_key`.
    pub fn set_max_possible_for_user_key(&mut self, user_key: &[u8]) {
        self.rep.extend_from_slice(user_key);
        put_fixed64(&mut self.rep, 0);
    }

    /// `SetMinPossibleForUserKey`: at or before every internal key of `user_key`.
    pub fn set_min_possible_for_user_key(&mut self, user_key: &[u8]) {
        self.rep.extend_from_slice(user_key);
        put_fixed64(
            &mut self.rep,
            (MAX_SEQUENCE_NUMBER << TYPE_BITS) | u64::from(VALUE_TYPE_FOR_SEEK.as_u8()),
        );
    }

    /// `Valid`: whether the bytes parse as an internal key.
    pub fn valid(&self) -> bool {
        parse_internal_key(&self.rep).is_ok()
    }

    /// `DecodeFrom`: takes the bytes as they are; `valid` says whether they parse.
    pub fn decode_from(&mut self, s: &[u8]) {
        self.rep.clear();
        self.rep.extend_from_slice(s);
    }

    /// `Encode`: the encoded key.
    pub fn encode(&self) -> &[u8] {
        &self.rep
    }

    /// `user_key`.
    pub fn user_key(&self) -> Result<&[u8], Error> {
        extract_user_key(&self.rep)
    }

    pub fn size(&self) -> usize {
        self.rep.len()
    }

    pub fn unset(&self) -> bool {
        self.rep.is_empty()
    }

    /// `Set`/`SetFrom`.
    pub fn set(&mut self, user_key: &[u8], s: SequenceNumber, t: ValueType) -> Result<(), Error> {
        let footer = pack_sequence_and_type(s, t)?;
        self.rep.clear();
        self.rep.extend_from_slice(user_key);
        put_fixed64(&mut self.rep, footer);
        Ok(())
    }

    pub fn clear(&mut self) {
        self.rep.clear();
    }

    /// `ConvertFromUserKey`: appends the trailer to a user key already in the buffer.
    pub fn convert_from_user_key(&mut self, s: SequenceNumber, t: ValueType) -> Result<(), Error> {
        append_internal_key_footer(&mut self.rep, s, t)
    }

    /// `rep()`: the buffer, for building a user key in place before `convert_from_user_key`.
    pub fn rep_mut(&mut self) -> &mut Vec<u8> {
        &mut self.rep
    }
}

/// `LookupKey` [R db/lookup_key.h:19-66; db/dbformat.cc:243-266]: `varint32(klen) ‖ user_key
/// ‖ fixed64(seq << 8 | kValueTypeForSeek)`, the key a memtable lookup seeks to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupKey {
    buf: Vec<u8>,
    /// Where the internal key starts, after the varint length.
    kstart: usize,
}

impl LookupKey {
    /// A key for looking up `user_key` as of sequence number `s`. RocksDB truncates a length
    /// above 2^32 into its varint32; the port refuses it.
    pub fn new(user_key: &[u8], s: SequenceNumber) -> Result<Self, Error> {
        let footer = pack_sequence_and_type(s, VALUE_TYPE_FOR_SEEK)?;
        let klen = user_key
            .len()
            .checked_add(NUM_INTERNAL_BYTES)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or(Error::InvalidArgument {
                what: "lookup key longer than a varint32 length",
            })?;
        let mut buf = Vec::new();
        put_varint32(&mut buf, klen);
        let kstart = buf.len();
        buf.extend_from_slice(user_key);
        put_fixed64(&mut buf, footer);
        Ok(Self { buf, kstart })
    }

    /// `memtable_key`: the whole encoding, length prefix first.
    pub fn memtable_key(&self) -> &[u8] {
        &self.buf
    }

    /// `internal_key`: the encoding after the length prefix.
    pub fn internal_key(&self) -> &[u8] {
        self.buf.get(self.kstart..).unwrap_or(&[])
    }

    /// `user_key`: the internal key without its trailer.
    pub fn user_key(&self) -> &[u8] {
        let end = self.buf.len().saturating_sub(NUM_INTERNAL_BYTES);
        self.buf.get(self.kstart..end).unwrap_or(&[])
    }
}

/// `IterKey` [R db/dbformat.h:568-1040; db/dbformat.cc:268-289]: the reusable key buffer of an
/// iterator, holding a user key or an internal key.
///
/// RocksDB's `IterKey` either copies a key or points at memory it does not own ("pinned"), and
/// keeps a second buffer for the timestamp-padding path. The port always owns the bytes; a
/// caller that would pin a key keeps ownership of the block it came from (docs/research/24
/// §4.6: pinning is ownership), so the observable key is the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IterKey {
    buf: Vec<u8>,
    is_user_key: bool,
}

impl Default for IterKey {
    fn default() -> Self {
        Self::new()
    }
}

impl IterKey {
    pub const fn new() -> Self {
        Self {
            buf: Vec::new(),
            is_user_key: true,
        }
    }

    pub fn set_is_user_key(&mut self, is_user_key: bool) {
        self.is_user_key = is_user_key;
    }

    pub fn is_user_key(&self) -> bool {
        self.is_user_key
    }

    /// `GetKey`: the key in the form it was set.
    pub fn key(&self) -> &[u8] {
        &self.buf
    }

    /// `GetInternalKey`.
    pub fn internal_key(&self) -> &[u8] {
        &self.buf
    }

    /// `GetUserKey`: the key, less its trailer when it holds an internal key.
    pub fn user_key(&self) -> &[u8] {
        if self.is_user_key {
            &self.buf
        } else {
            let end = self.buf.len().saturating_sub(NUM_INTERNAL_BYTES);
            self.buf.get(..end).unwrap_or(&[])
        }
    }

    pub fn size(&self) -> usize {
        self.buf.len()
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }

    /// `TrimAppend` [R db/dbformat.h:627-654]: keeps the first `shared_len` bytes and appends
    /// `non_shared`, the delta decoding of a block's keys. A `shared_len` past the key is
    /// corruption of the block that named it.
    pub fn trim_append(&mut self, shared_len: usize, non_shared: &[u8]) -> Result<(), Error> {
        if shared_len > self.buf.len() {
            return Err(Error::corruption("delta-encoded key", Malformed::TooLarge));
        }
        self.buf.truncate(shared_len);
        self.buf.extend_from_slice(non_shared);
        Ok(())
    }

    /// `TrimAppendWithTimestamp` [R db/dbformat.h:656-704]: as `trim_append` for keys stored
    /// without their timestamp, padding a zero timestamp of `ts_sz` bytes back in: at the end
    /// of a user key, or before the trailer of an internal key. `shared_len` counts bytes of
    /// the previous key as stored, that is without the timestamp this buffer holds.
    pub fn trim_append_with_timestamp(
        &mut self,
        shared_len: usize,
        non_shared: &[u8],
        ts_sz: usize,
    ) -> Result<(), Error> {
        // The previous key as stored. A user key's timestamp is at its end, past any shared
        // prefix, so the buffer serves as it is; an internal key's sits before the trailer and
        // is taken out.
        let mut stored = if self.is_user_key {
            self.buf.clone()
        } else {
            let footer_at = self
                .buf
                .len()
                .checked_sub(NUM_INTERNAL_BYTES)
                .ok_or(Error::truncated("internal key"))?;
            let ts_at = footer_at
                .checked_sub(ts_sz)
                .ok_or(Error::truncated("internal key with timestamp"))?;
            let mut v = self.buf.get(..ts_at).unwrap_or(&[]).to_vec();
            v.extend_from_slice(self.buf.get(footer_at..).unwrap_or(&[]));
            v
        };
        if shared_len > stored.len() {
            return Err(Error::corruption("delta-encoded key", Malformed::TooLarge));
        }
        stored.truncate(shared_len);
        stored.extend_from_slice(non_shared);
        let insert_at = if self.is_user_key {
            stored.len()
        } else {
            stored
                .len()
                .checked_sub(NUM_INTERNAL_BYTES)
                .ok_or(Error::truncated("internal key"))?
        };
        let (head, tail) = stored
            .split_at_checked(insert_at)
            .ok_or(Error::truncated("internal key"))?;
        let mut out = head.to_vec();
        out.resize(out.len().saturating_add(ts_sz), 0);
        out.extend_from_slice(tail);
        self.buf = out;
        Ok(())
    }

    /// `SetUserKey`.
    pub fn set_user_key(&mut self, key: &[u8]) {
        self.is_user_key = true;
        self.buf.clear();
        self.buf.extend_from_slice(key);
    }

    /// `SetInternalKey(Slice)`.
    pub fn set_internal_key(&mut self, key: &[u8]) {
        self.is_user_key = false;
        self.buf.clear();
        self.buf.extend_from_slice(key);
    }

    /// `SetInternalKey(user_key, s, value_type)`: builds the internal key in place.
    pub fn set_internal_key_from_parts(
        &mut self,
        user_key: &[u8],
        s: SequenceNumber,
        value_type: ValueType,
    ) -> Result<(), Error> {
        let footer = pack_sequence_and_type(s, value_type)?;
        self.buf.clear();
        self.buf.extend_from_slice(user_key);
        put_fixed64(&mut self.buf, footer);
        self.is_user_key = false;
        Ok(())
    }

    /// `UpdateInternalKey`: rewrites the trailer.
    pub fn update_internal_key(&mut self, seq: SequenceNumber, t: ValueType) -> Result<(), Error> {
        update_internal_key(&mut self.buf, seq, t)
    }

    /// `Swap`.
    pub fn swap(&mut self, other: &mut Self) {
        std::mem::swap(self, other);
    }
}

/// `RangeTombstone` [R db/dbformat.h:1081-1146]: a deletion of the user keys in
/// `[start_key, end_key)` at sequence number `seq`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeTombstone<'a> {
    pub start_key: &'a [u8],
    pub end_key: &'a [u8],
    pub seq: SequenceNumber,
}

impl<'a> RangeTombstone<'a> {
    pub const fn new(start_key: &'a [u8], end_key: &'a [u8], seq: SequenceNumber) -> Self {
        Self {
            start_key,
            end_key,
            seq,
        }
    }

    /// From a range-deletion entry: the key's user key starts the range, the value ends it.
    pub const fn from_entry(parsed_key: &ParsedInternalKey<'a>, value: &'a [u8]) -> Self {
        Self {
            start_key: parsed_key.user_key,
            end_key: value,
            seq: parsed_key.sequence,
        }
    }

    /// `Serialize`: the entry's internal key and value.
    pub fn serialize(&self) -> Result<(InternalKey, &'a [u8]), Error> {
        Ok((self.serialize_key()?, self.end_key))
    }

    /// `SerializeKey`.
    pub fn serialize_key(&self) -> Result<InternalKey, Error> {
        InternalKey::new(self.start_key, self.seq, ValueType::RangeDeletion)
    }

    /// `SerializeEndKey`: the end key at the largest sequence number, which sorts before every
    /// real entry of the end key.
    pub fn serialize_end_key(&self) -> Result<InternalKey, Error> {
        InternalKey::new(self.end_key, MAX_SEQUENCE_NUMBER, ValueType::RangeDeletion)
    }
}

/// `ShortenedIndexBuilder::FindShortestInternalKeySeparator` [R
/// table/block_based/index_builder.cc:78-101]: an internal key in `[start, limit)` shorter than
/// `start` when the user comparator finds one, else `start`. It lives here because it works on
/// internal keys alone; the index builder (P4) calls it.
pub fn find_shortest_internal_key_separator(
    comparator: Comparator,
    start: &[u8],
    limit: &[u8],
) -> Result<Vec<u8>, Error> {
    let user_start = extract_user_key(start)?;
    let user_limit = extract_user_key(limit)?;
    let mut scratch = user_start.to_vec();
    comparator.find_shortest_separator(&mut scratch, user_limit);
    Ok(shortened_or(comparator, start, user_start, scratch))
}

/// `ShortenedIndexBuilder::FindShortInternalKeySuccessor` [R
/// table/block_based/index_builder.cc:103-120].
pub fn find_short_internal_key_successor(
    comparator: Comparator,
    key: &[u8],
) -> Result<Vec<u8>, Error> {
    let user_key = extract_user_key(key)?;
    let mut scratch = user_key.to_vec();
    comparator.find_short_successor(&mut scratch);
    Ok(shortened_or(comparator, key, user_key, scratch))
}

/// The shortened user key with the earliest trailer when it became physically shorter (or no
/// longer) but logically larger; otherwise the original internal key.
fn shortened_or(
    comparator: Comparator,
    key: &[u8],
    user_key: &[u8],
    mut scratch: Vec<u8>,
) -> Vec<u8> {
    if scratch.len() <= user_key.len() && comparator.compare(user_key, &scratch) == Ordering::Less {
        put_fixed64(
            &mut scratch,
            (MAX_SEQUENCE_NUMBER << TYPE_BITS) | u64::from(VALUE_TYPE_FOR_SEEK.as_u8()),
        );
        scratch
    } else {
        key.to_vec()
    }
}
