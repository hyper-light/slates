//! The memtable: RocksDB's `db/memtable.{h,cc}` with the `SkipListRep` of
//! `memtable/skiplistrep.cc` (docs/research/24 §4.1).
//!
//! Point entries and range deletions each live in an [`InlineSkipList`] over one [`Arena`], as
//! RocksDB keeps `table_` and `range_del_table_`. One writer adds through `&mut MemTable`;
//! readers on any thread read through `&MemTable` or a [`MemTableReader`], and see each entry
//! whole or not at all (memtable/inlineskiplist.rs). RocksDB's concurrent insert
//! (`allow_concurrent_memtable_write`) is not ported (docs/research/24 §4.1 DECISION).
//!
//! An entry is stored as `fixed64 internal_key_len ‖ internal_key ‖ value`. RocksDB stores
//! `varint32 ikey_len ‖ ikey ‖ varint32 value_len ‖ value` [R db/memtable.cc:1123-1154]; the
//! layout is not an on-disk format (docs/research/24 §4.1), and a whole header word puts the
//! user key on a word boundary, where the skiplist compares it a word at a time. The value's
//! length is the entry's less the key's.
//!
//! Not yet ported, each with the phase that brings it: per-key protection bytes
//! (`memtable_protection_bytes_per_key`, P7 with the write path's checksums), the memtable
//! Bloom filter and prefix extractor (P5), in-place updates (not used by mantle), merge
//! operators (P12: [`MemTable::get`] collects merge operands and leaves the merge to its
//! caller), and fragmentation of range tombstones (P12: the range-deletion iterator yields
//! tombstones as written, and [`MemTable::get`] finds the newest covering one directly).

use std::cmp::Ordering;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering as AtomicOrdering};

use crate::db::dbformat::{
    InternalKeyComparator, LookupKey, MAX_SEQUENCE_NUMBER, NUM_INTERNAL_BYTES, SequenceNumber,
    ValueType, pack_sequence_and_type, unpack_sequence_and_type,
};
use crate::error::{Error, Malformed};
use crate::memory::arena::{Allocator, Arena, Store};
use crate::memtable::inlineskiplist::{
    self, KeyComparator, ListRef, ListShared, ListWriter, StoredKey,
};
use crate::util::comparator::Comparator;

/// `write_buffer_size`'s default, 64 MiB [R include/rocksdb/options.h:191].
pub const DEFAULT_WRITE_BUFFER_SIZE: usize = 64 << 20;
/// The largest arena block `SanitizeOptions` derives from the write buffer size, 1 MiB
/// [R db/column_family.cc:241-243].
const MAX_DERIVED_ARENA_BLOCK_SIZE: usize = 1 << 20;
/// The alignment `SanitizeOptions` rounds a derived arena block up to, 4 KiB
/// [R db/column_family.cc:245-248].
const ARENA_BLOCK_ALIGN: usize = 4096;
/// `kAllowOverAllocationRatio` = 0.6 [R db/memtable.cc:316] as the fraction 3/5, so the flush
/// test is exact integer arithmetic.
const OVER_ALLOCATION_NUMERATOR: u128 = 3;
const OVER_ALLOCATION_DENOMINATOR: u128 = 5;
/// The header word before an entry's internal key.
const HEADER: usize = 8;
/// A value of `kTypeValuePreferredSeqno` ends with its fixed64 write time
/// [R db/seqno_to_time_mapping.cc:567-570].
const PACKED_WRITE_TIME: usize = 8;

/// The options of one memtable (RocksDB's `MutableCFOptions` fields it reads).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemTableOptions {
    /// `write_buffer_size`: the flush trigger's target.
    pub write_buffer_size: usize,
    /// `arena_block_size`, as sanitized.
    pub arena_block_size: usize,
    /// `memtable_max_range_deletions`: 0 for no limit, the default
    /// [R include/rocksdb/advanced_options.h].
    pub max_range_deletions: u64,
    /// The arena bytes past which an add is refused. RocksDB has none: its write path
    /// switches the memtable once the flush trigger fires. The port bounds it
    /// (CLAUDE.md §2); [`MemTableOptions::new`] sets it to twice the write buffer size, so a
    /// caller that ignores [`MemTable::should_flush`] is refused before the memtable grows
    /// without bound, and one that honours it never meets the limit.
    pub memory_limit: usize,
}

impl MemTableOptions {
    /// Options for `write_buffer_size`, with the arena block `SanitizeOptions` derives:
    /// `min(1 MiB, write_buffer_size / 8)` rounded up to 4 KiB [R db/column_family.cc:239-249].
    pub fn new(write_buffer_size: usize) -> Self {
        let derived = MAX_DERIVED_ARENA_BLOCK_SIZE.min(write_buffer_size / 8);
        let aligned = derived
            .div_ceil(ARENA_BLOCK_ALIGN)
            .saturating_mul(ARENA_BLOCK_ALIGN);
        Self {
            write_buffer_size,
            arena_block_size: aligned,
            max_range_deletions: 0,
            memory_limit: write_buffer_size.saturating_mul(2),
        }
    }
}

impl Default for MemTableOptions {
    fn default() -> Self {
        Self::new(DEFAULT_WRITE_BUFFER_SIZE)
    }
}

/// Orders entries by their internal keys under an [`InternalKeyComparator`]
/// (`MemTable::KeyComparator` [R db/memtable.cc:454-470]). Both sides are entries in the
/// stored layout; a lookup's key is an entry without a value.
#[derive(Debug, Clone, Copy)]
pub struct EntryComparator {
    user: Comparator,
}

/// The internal key of an entry given as bytes, or `None` if the header disagrees with them.
fn probe_internal_key(entry: &[u8]) -> Option<&[u8]> {
    let header = entry.get(..HEADER)?;
    let len = usize::try_from(u64::from_le_bytes(header.try_into().ok()?)).ok()?;
    entry.get(HEADER..HEADER.checked_add(len)?)
}

impl KeyComparator for EntryComparator {
    #[inline]
    fn compare(&self, stored: StoredKey<'_>, key: &[u8]) -> Ordering {
        // Malformed bytes are never stored or probed (both are built by this module); they
        // order first so the comparison stays total.
        let Some(probe) = probe_internal_key(key) else {
            return Ordering::Greater;
        };
        let Some(ikey_len) = stored.word(0).and_then(|w| usize::try_from(w).ok()) else {
            return Ordering::Less;
        };
        let Some(user_len) = ikey_len.checked_sub(NUM_INTERNAL_BYTES) else {
            return Ordering::Less;
        };
        let probe_user_len = probe.len().saturating_sub(NUM_INTERNAL_BYTES);
        let (probe_user, probe_trailer) = probe
            .split_at_checked(probe_user_len)
            .unwrap_or((probe, &[]));
        let bytewise = stored.compare_range(HEADER, user_len, probe_user);
        let user_order = match self.user {
            Comparator::Bytewise => bytewise,
            Comparator::ReverseBytewise => bytewise.reverse(),
        };
        user_order.then_with(|| {
            let stored_trailer = HEADER
                .checked_add(user_len)
                .and_then(|at| stored.read_u64_le(at))
                .unwrap_or(0);
            let probe_trailer = <[u8; 8]>::try_from(probe_trailer).map_or(0, u64::from_le_bytes);
            // Trailers descend: the newer entry first.
            probe_trailer.cmp(&stored_trailer)
        })
    }
}

/// Appends an entry (or, with an empty value, a lookup probe) in the stored layout.
fn encode_entry(
    out: &mut Vec<u8>,
    user_key: &[u8],
    trailer: u64,
    value: &[u8],
) -> Result<(), Error> {
    let ikey_len =
        user_key
            .len()
            .checked_add(NUM_INTERNAL_BYTES)
            .ok_or(Error::InvalidArgument {
                what: "memtable key too large",
            })?;
    out.clear();
    out.extend_from_slice(&u64::try_from(ikey_len).unwrap_or(u64::MAX).to_le_bytes());
    out.extend_from_slice(user_key);
    out.extend_from_slice(&trailer.to_le_bytes());
    out.extend_from_slice(value);
    Ok(())
}

/// The memtable's counters, which readers see while the writer adds. Statistics and flags,
/// never used to order other memory: relaxed, except `range_del_empty` (below).
#[derive(Debug)]
struct Counters {
    num_entries: AtomicU64,
    num_deletes: AtomicU64,
    num_range_deletes: AtomicU64,
    data_size: AtomicU64,
    /// `first_seqno_`: the first sequence number added, 0 while empty.
    first_seqno: AtomicU64,
    /// `earliest_seqno_`: at most the smallest sequence number this memtable may hold.
    earliest_seqno: AtomicU64,
    memory_allocated: AtomicUsize,
    approximate_memory_usage: AtomicUsize,
    /// Whether the range-deletion table is empty: stored false with release ordering after
    /// the first tombstone is published and loaded with acquire, so a reader that skips the
    /// table on `true` misses only tombstones not yet published when it looked — the same
    /// prefix of inserts the lists themselves promise.
    range_del_empty: AtomicBool,
    flush_requested: AtomicBool,
}

/// A read view of a memtable, borrowed from its owner: `Copy`, for any number of readers on any
/// threads (the owner outlives every view, which keeps the arena's blocks alive).
#[derive(Debug)]
pub struct MemTableRef<'a> {
    table: ListRef<'a, EntryComparator>,
    range_del: ListRef<'a, EntryComparator>,
    counters: &'a Counters,
    cmp: InternalKeyComparator,
}

impl Clone for MemTableRef<'_> {
    fn clone(&self) -> Self {
        *self
    }
}

impl Copy for MemTableRef<'_> {}

/// What a memtable holds for a key, found by [`MemTable::get`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// A value (`kTypeValue`, or `kTypeValuePreferredSeqno` without its write time).
    Value(Vec<u8>),
    /// A serialized wide-column entity (docs/research/24 §1.17).
    Entity(Vec<u8>),
    /// A blob index (docs/research/24 §1.16), which the caller resolves.
    BlobIndex(Vec<u8>),
    /// A point or covering range deletion: the key is not found at this sequence number.
    Deleted,
}

/// The newest entry for a key at or below the lookup's sequence number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// The entry's sequence number, or the covering range deletion's when one hides it.
    pub seq: SequenceNumber,
    pub found: Found,
}

/// `MergeContext` [R db/merge_context.h]: the merge operands a lookup collected, newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeContext {
    operands: Vec<Vec<u8>>,
}

impl MergeContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.operands.clear();
    }

    /// The operands, newest first, as `GetOperandsDirectionBackward`.
    pub fn operands(&self) -> &[Vec<u8>] {
        &self.operands
    }

    pub fn num_operands(&self) -> usize {
        self.operands.len()
    }
}

impl<'a> MemTableRef<'a> {
    /// `Get` [R db/memtable.cc:1568-1648, :1319-1566 `SaveValue`]: the newest entry for
    /// `key`'s user key at or below its sequence number.
    ///
    /// `Ok(Some(hit))` is RocksDB's `true`: a value, an entity, a blob index, or a deletion
    /// (point or covering range). `Ok(None)` is `false`: nothing final here, and the caller
    /// looks in older memtables and tables. Merge operands passed on the way are pushed onto
    /// `merge_context`; with operands present, a hit is the merge's base and `None` means the
    /// merge is still in progress. Merging is the caller's (P12). `max_covering_tombstone_seq`
    /// is raised to the newest range deletion here covering the key, as RocksDB raises it.
    pub fn get(
        &self,
        key: &LookupKey,
        merge_context: &mut MergeContext,
        max_covering_tombstone_seq: &mut SequenceNumber,
    ) -> Result<Option<Hit>, Error> {
        let (read_seq, _) = unpack_sequence_and_type(
            crate::db::dbformat::extract_internal_key_footer(key.internal_key())?,
        );
        let covering = self.max_covering_tombstone_seq(key.user_key(), read_seq)?;
        if covering > *max_covering_tombstone_seq {
            *max_covering_tombstone_seq = covering;
        }
        let max_cov = *max_covering_tombstone_seq;

        let mut probe = Vec::new();
        encode_entry(
            &mut probe,
            key.user_key(),
            crate::db::dbformat::extract_internal_key_footer(key.internal_key())?,
            &[],
        )?;
        let mut iter = self.table.iter();
        iter.seek(&probe);
        let mut entry = DecodedEntry::default();
        while iter.valid() {
            entry.decode(iter.key())?;
            if !self
                .cmp
                .user_comparator()
                .equal(entry.user_key(), key.user_key())
            {
                return Ok(None);
            }
            let (seq, t) = unpack_sequence_and_type(entry.trailer);
            let mut value_type = ValueType::from_u8(t).ok_or(Error::corruption(
                "memtable entry",
                Malformed::UnknownTag(t),
            ))?;
            let hidden = matches!(
                value_type,
                ValueType::Value
                    | ValueType::Merge
                    | ValueType::BlobIndex
                    | ValueType::WideColumnEntity
                    | ValueType::Deletion
                    | ValueType::SingleDeletion
                    | ValueType::DeletionWithTimestamp
                    | ValueType::ValuePreferredSeqno
            ) && max_cov > seq;
            if hidden {
                value_type = ValueType::RangeDeletion;
            }
            let hit_seq = if hidden { max_cov } else { seq };
            let found = match value_type {
                ValueType::BlobIndex => {
                    if merge_context.num_operands() > 0 {
                        return Err(Error::Unsupported {
                            feature: "merge onto a blob index",
                            value: u64::from(t),
                        });
                    }
                    Found::BlobIndex(entry.value().to_vec())
                }
                ValueType::Value => Found::Value(entry.value().to_vec()),
                ValueType::ValuePreferredSeqno => {
                    let v = entry.value();
                    let end = v
                        .len()
                        .checked_sub(PACKED_WRITE_TIME)
                        .ok_or(Error::truncated("value with write time"))?;
                    Found::Value(v.get(..end).unwrap_or(&[]).to_vec())
                }
                ValueType::WideColumnEntity => Found::Entity(entry.value().to_vec()),
                ValueType::Deletion
                | ValueType::DeletionWithTimestamp
                | ValueType::SingleDeletion
                | ValueType::RangeDeletion => Found::Deleted,
                ValueType::Merge => {
                    merge_context.operands.push(entry.value().to_vec());
                    iter.next();
                    continue;
                }
                _ => {
                    return Err(Error::corruption(
                        "memtable entry",
                        Malformed::UnknownTag(t),
                    ));
                }
            };
            return Ok(Some(Hit {
                seq: hit_seq,
                found,
            }));
        }
        Ok(None)
    }

    /// `MaxCoveringTombstoneSeqnum`: the newest range deletion at or below `read_seq` whose
    /// range holds `user_key`, or 0. The range-deletion table is ordered by start key, so the
    /// scan stops at the first tombstone starting after the key.
    fn max_covering_tombstone_seq(
        &self,
        user_key: &[u8],
        read_seq: SequenceNumber,
    ) -> Result<SequenceNumber, Error> {
        if self.counters.range_del_empty.load(AtomicOrdering::Acquire) {
            return Ok(0);
        }
        let user = self.cmp.user_comparator();
        let mut best = 0;
        let mut iter = self.range_del.iter();
        iter.seek_to_first();
        let mut entry = DecodedEntry::default();
        while iter.valid() {
            entry.decode(iter.key())?;
            if user.compare(entry.user_key(), user_key) == Ordering::Greater {
                break;
            }
            let (seq, _) = unpack_sequence_and_type(entry.trailer);
            if seq <= read_seq
                && seq > best
                && user.compare(user_key, entry.value()) == Ordering::Less
            {
                best = seq;
            }
            iter.next();
        }
        Ok(best)
    }

    /// An iterator over the point entries (`NewIterator`).
    pub fn iter(&self) -> MemTableIter<'a> {
        MemTableIter::new(self.table.iter())
    }

    /// An iterator over the range deletions as written, or `None` when there are none
    /// (`NewRangeTombstoneIterator` returns null then). Tombstones are not fragmented (P12).
    pub fn range_del_iter(&self) -> Option<MemTableIter<'a>> {
        if self.counters.range_del_empty.load(AtomicOrdering::Acquire) {
            None
        } else {
            Some(MemTableIter::new(self.range_del.iter()))
        }
    }

    pub fn num_entries(&self) -> u64 {
        self.counters.num_entries.load(AtomicOrdering::Relaxed)
    }

    /// `NumDeletion`: point deletions (`Delete`, `SingleDelete`).
    pub fn num_deletes(&self) -> u64 {
        self.counters.num_deletes.load(AtomicOrdering::Relaxed)
    }

    pub fn num_range_deletes(&self) -> u64 {
        self.counters
            .num_range_deletes
            .load(AtomicOrdering::Relaxed)
    }

    /// `get_data_size`: the entries' bytes in RocksDB's encoding, as its write path counts.
    pub fn data_size(&self) -> u64 {
        self.counters.data_size.load(AtomicOrdering::Relaxed)
    }

    /// `IsEmpty`.
    pub fn is_empty(&self) -> bool {
        self.first_sequence_number() == 0
    }

    /// `GetFirstSequenceNumber`: 0 while empty.
    pub fn first_sequence_number(&self) -> SequenceNumber {
        self.counters.first_seqno.load(AtomicOrdering::Relaxed)
    }

    /// `GetEarliestSequenceNumber`.
    pub fn earliest_sequence_number(&self) -> SequenceNumber {
        self.counters.earliest_seqno.load(AtomicOrdering::Relaxed)
    }

    /// `MemoryAllocatedBytes`: the arena's blocks.
    pub fn memory_allocated_bytes(&self) -> usize {
        self.counters.memory_allocated.load(AtomicOrdering::Relaxed)
    }

    /// `ApproximateMemoryUsage`, as of the last add.
    pub fn approximate_memory_usage(&self) -> usize {
        self.counters
            .approximate_memory_usage
            .load(AtomicOrdering::Relaxed)
    }

    /// `ShouldScheduleFlush`: the flush trigger has fired.
    pub fn should_flush(&self) -> bool {
        self.counters.flush_requested.load(AtomicOrdering::Relaxed)
    }

    pub fn internal_key_comparator(&self) -> InternalKeyComparator {
        self.cmp
    }
}

/// One entry decoded from the list: its internal key and value, copied out of the arena.
#[derive(Debug, Default)]
struct DecodedEntry {
    bytes: Vec<u8>,
    ikey_len: usize,
    trailer: u64,
}

impl DecodedEntry {
    fn decode(&mut self, stored: StoredKey<'_>) -> Result<(), Error> {
        let ikey_len = stored
            .word(0)
            .and_then(|w| usize::try_from(w).ok())
            .ok_or(Error::truncated("memtable entry"))?;
        if ikey_len < NUM_INTERNAL_BYTES || HEADER.saturating_add(ikey_len) > stored.len() {
            return Err(Error::corruption("memtable entry", Malformed::TooLarge));
        }
        self.bytes.clear();
        stored.read_range(HEADER, stored.len().saturating_sub(HEADER), &mut self.bytes)?;
        self.ikey_len = ikey_len;
        let trailer_at = ikey_len.saturating_sub(NUM_INTERNAL_BYTES);
        self.trailer = self
            .bytes
            .get(trailer_at..ikey_len)
            .and_then(|b| <[u8; 8]>::try_from(b).ok())
            .map_or(0, u64::from_le_bytes);
        Ok(())
    }

    fn internal_key(&self) -> &[u8] {
        self.bytes.get(..self.ikey_len).unwrap_or(&[])
    }

    fn user_key(&self) -> &[u8] {
        self.bytes
            .get(..self.ikey_len.saturating_sub(NUM_INTERNAL_BYTES))
            .unwrap_or(&[])
    }

    fn value(&self) -> &[u8] {
        self.bytes.get(self.ikey_len..).unwrap_or(&[])
    }
}

/// `MemTableIterator` [R db/memtable.cc]: internal keys and values in internal-key order.
/// Positioning copies the entry out of the arena; a decode failure ends the iteration and is
/// reported by [`MemTableIter::status`].
pub struct MemTableIter<'a> {
    inner: inlineskiplist::Iter<'a, EntryComparator>,
    entry: DecodedEntry,
    probe: Vec<u8>,
    status: Result<(), Error>,
    valid: bool,
}

impl<'a> MemTableIter<'a> {
    fn new(inner: inlineskiplist::Iter<'a, EntryComparator>) -> Self {
        Self {
            inner,
            entry: DecodedEntry::default(),
            probe: Vec::new(),
            status: Ok(()),
            valid: false,
        }
    }

    fn settle(&mut self) {
        self.valid = false;
        if !self.inner.valid() {
            return;
        }
        match self.entry.decode(self.inner.key()) {
            Ok(()) => self.valid = true,
            Err(e) => self.status = Err(e),
        }
    }

    pub fn valid(&self) -> bool {
        self.valid
    }

    /// The first decode failure, if any.
    pub fn status(&self) -> Result<(), Error> {
        self.status.clone()
    }

    /// The current internal key. Empty when not `valid`.
    pub fn key(&self) -> &[u8] {
        if self.valid {
            self.entry.internal_key()
        } else {
            &[]
        }
    }

    /// The current value. Empty when not `valid`.
    pub fn value(&self) -> &[u8] {
        if self.valid { self.entry.value() } else { &[] }
    }

    pub fn seek_to_first(&mut self) {
        self.inner.seek_to_first();
        self.settle();
    }

    pub fn seek_to_last(&mut self) {
        self.inner.seek_to_last();
        self.settle();
    }

    /// Positions at the first entry at or after the internal key `target`.
    pub fn seek(&mut self, target: &[u8]) {
        if self.encode_probe(target) {
            self.inner.seek(&self.probe);
            self.settle();
        }
    }

    /// Positions at the last entry at or before the internal key `target`.
    pub fn seek_for_prev(&mut self, target: &[u8]) {
        if self.encode_probe(target) {
            self.inner.seek_for_prev(&self.probe);
            self.settle();
        }
    }

    fn encode_probe(&mut self, target: &[u8]) -> bool {
        let split = target.len().saturating_sub(NUM_INTERNAL_BYTES);
        let result = match target.split_at_checked(split) {
            Some((user, trailer)) if trailer.len() == NUM_INTERNAL_BYTES => {
                let t = <[u8; 8]>::try_from(trailer).map_or(0, u64::from_le_bytes);
                encode_entry(&mut self.probe, user, t, &[])
            }
            _ => Err(Error::truncated("seek key")),
        };
        if let Err(e) = result {
            self.status = Err(e);
            self.valid = false;
            return false;
        }
        true
    }

    pub fn next(&mut self) {
        if self.valid {
            self.inner.next();
            self.settle();
        }
    }

    pub fn prev(&mut self) {
        if self.valid {
            self.inner.prev();
            self.settle();
        }
    }
}

/// The writer's own state: the two lists' writers, the entry buffer and the flush arithmetic's
/// inputs.
#[derive(Debug)]
struct WriterState {
    table: ListWriter,
    range_del: ListWriter,
    options: MemTableOptions,
    /// `kArenaBlockSize`: the optimized block size the flush arithmetic uses.
    arena_block_size: usize,
    scratch: Vec<u8>,
}

/// `MemTable`: owns the arena, both lists, the counters and the writer state. Reads borrow it
/// (`&self`, or a [`MemTableRef`] from [`MemTable::view`]); the one writer borrows it mutably
/// ([`MemTable::add`]); [`MemTable::split`] lends a writer and a view together, so the writer
/// and readers run on scoped threads. Nothing is reference-counted: the owner outliving its
/// borrows is the whole lifetime contract, and a memtable that has become immutable is owned
/// by its `MemTableList` (db/memtable_list.rs), whose readers borrow it the same way. Readers
/// that must outlive a list's changes (a `SuperVersion`) are P7–P8's, which will state how.
#[derive(Debug)]
pub struct MemTable {
    arena: Arena,
    table: ListShared<EntryComparator>,
    range_del: ListShared<EntryComparator>,
    counters: Counters,
    cmp: InternalKeyComparator,
    writer: WriterState,
    id: u64,
    /// Flush bookkeeping `MemTableList` keeps on each memtable (`flush_in_progress_`,
    /// `flush_completed_`, `file_number_`), changed only through the list's `&mut`.
    pub(crate) flush_in_progress: bool,
    pub(crate) flush_completed: bool,
    pub(crate) file_number: u64,
}

/// The writing handle of a split [`MemTable`].
#[derive(Debug)]
pub struct MemTableWriter<'a> {
    store: &'a Store,
    alloc: &'a mut Allocator,
    table: &'a ListShared<EntryComparator>,
    range_del: &'a ListShared<EntryComparator>,
    counters: &'a Counters,
    state: &'a mut WriterState,
}

impl MemTable {
    /// A memtable ordered by `cmp`. `latest_seq` is the sequence number the database had when
    /// it was created, RocksDB's `earliest_seqno_` until the first add.
    pub fn new(
        cmp: InternalKeyComparator,
        options: MemTableOptions,
        latest_seq: SequenceNumber,
    ) -> Result<Self, Error> {
        let arena = Arena::new(options.arena_block_size, Some(options.memory_limit))?;
        let entry_cmp = EntryComparator {
            user: cmp.user_comparator(),
        };
        let counters = Counters {
            num_entries: AtomicU64::new(0),
            num_deletes: AtomicU64::new(0),
            num_range_deletes: AtomicU64::new(0),
            data_size: AtomicU64::new(0),
            first_seqno: AtomicU64::new(0),
            earliest_seqno: AtomicU64::new(latest_seq),
            memory_allocated: AtomicUsize::new(arena.memory_allocated_bytes()),
            approximate_memory_usage: AtomicUsize::new(arena.approximate_memory_usage()),
            range_del_empty: AtomicBool::new(true),
            flush_requested: AtomicBool::new(false),
        };
        let arena_block_size = Arena::optimize_block_size(options.arena_block_size);
        let mut mem = Self {
            arena,
            table: ListShared::new(entry_cmp),
            range_del: ListShared::new(entry_cmp),
            counters,
            cmp,
            writer: WriterState {
                table: ListWriter::default(),
                range_del: ListWriter::default(),
                options,
                arena_block_size,
                scratch: Vec::new(),
            },
            id: 0,
            flush_in_progress: false,
            flush_completed: false,
            file_number: 0,
        };
        mem.split().0.update_flush_state();
        Ok(mem)
    }

    /// The read view.
    pub fn view(&self) -> MemTableRef<'_> {
        MemTableRef {
            table: ListRef::new(self.arena.store(), &self.table),
            range_del: ListRef::new(self.arena.store(), &self.range_del),
            counters: &self.counters,
            cmp: self.cmp,
        }
    }

    /// The writing handle and a read view, borrowed together.
    pub fn split(&mut self) -> (MemTableWriter<'_>, MemTableRef<'_>) {
        let (store, alloc) = self.arena.split();
        let view = MemTableRef {
            table: ListRef::new(store, &self.table),
            range_del: ListRef::new(store, &self.range_del),
            counters: &self.counters,
            cmp: self.cmp,
        };
        (
            MemTableWriter {
                store,
                alloc,
                table: &self.table,
                range_del: &self.range_del,
                counters: &self.counters,
                state: &mut self.writer,
            },
            view,
        )
    }

    /// `Add`; see [`MemTableWriter::add`].
    pub fn add(
        &mut self,
        s: SequenceNumber,
        value_type: ValueType,
        key: &[u8],
        value: &[u8],
    ) -> Result<(), Error> {
        self.split().0.add(s, value_type, key, value)
    }

    /// `Get`; see [`MemTableRef::get`].
    pub fn get(
        &self,
        key: &LookupKey,
        merge_context: &mut MergeContext,
        max_covering_tombstone_seq: &mut SequenceNumber,
    ) -> Result<Option<Hit>, Error> {
        self.view()
            .get(key, merge_context, max_covering_tombstone_seq)
    }

    /// An iterator over the point entries.
    pub fn iter(&self) -> MemTableIter<'_> {
        self.view().iter()
    }

    /// An iterator over the range deletions, or `None` when there are none.
    pub fn range_del_iter(&self) -> Option<MemTableIter<'_>> {
        self.view().range_del_iter()
    }

    pub fn num_entries(&self) -> u64 {
        self.view().num_entries()
    }

    pub fn num_deletes(&self) -> u64 {
        self.view().num_deletes()
    }

    pub fn num_range_deletes(&self) -> u64 {
        self.view().num_range_deletes()
    }

    pub fn data_size(&self) -> u64 {
        self.view().data_size()
    }

    pub fn is_empty(&self) -> bool {
        self.view().is_empty()
    }

    pub fn first_sequence_number(&self) -> SequenceNumber {
        self.view().first_sequence_number()
    }

    pub fn earliest_sequence_number(&self) -> SequenceNumber {
        self.view().earliest_sequence_number()
    }

    pub fn memory_allocated_bytes(&self) -> usize {
        self.view().memory_allocated_bytes()
    }

    pub fn approximate_memory_usage(&self) -> usize {
        self.view().approximate_memory_usage()
    }

    pub fn should_flush(&self) -> bool {
        self.view().should_flush()
    }

    pub fn arena(&self) -> &Arena {
        &self.arena
    }

    /// `SetID`/`GetID`: the order `MemTableList` keeps memtables in.
    pub fn set_id(&mut self, id: u64) {
        self.id = id;
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    /// `GetFileNumber`: the table a completed flush wrote, 0 before.
    pub fn file_number(&self) -> u64 {
        self.file_number
    }
}

impl MemTableWriter<'_> {
    /// `Add` [R db/memtable.cc:1116-1239]: adds `key` (a user key) at sequence number `s` with
    /// `value_type`. A range deletion (`value` its end key) goes to the range-deletion table.
    /// An entry with the same internal key already present is [`Error::Duplicate`], RocksDB's
    /// `TryAgain("key+seq exists")`.
    pub fn add(
        &mut self,
        s: SequenceNumber,
        value_type: ValueType,
        key: &[u8],
        value: &[u8],
    ) -> Result<(), Error> {
        if !value_type.is_value_type() && value_type != ValueType::RangeDeletion {
            return Err(Error::InvalidArgument {
                what: "value type not stored in a memtable",
            });
        }
        // RocksDB's sizes are u32 [R db/memtable.cc:1127-1129]; the batch refuses larger ones
        // first (docs/research/24 §1.4), and so does the memtable.
        let key_ok = key
            .len()
            .checked_add(NUM_INTERNAL_BYTES)
            .is_some_and(|n| u32::try_from(n).is_ok());
        if !key_ok || u32::try_from(value.len()).is_err() {
            return Err(Error::InvalidArgument {
                what: "memtable key or value larger than u32",
            });
        }
        let trailer = pack_sequence_and_type(s, value_type)?;
        let state = &mut *self.state;
        encode_entry(&mut state.scratch, key, trailer, value)?;
        let inserted = if value_type == ValueType::RangeDeletion {
            state
                .range_del
                .insert(self.range_del, self.store, self.alloc, &state.scratch)?
        } else {
            state
                .table
                .insert(self.table, self.store, self.alloc, &state.scratch)?
        };
        if !inserted {
            return Err(Error::Duplicate);
        }

        let c = self.counters;
        c.num_entries.fetch_add(1, AtomicOrdering::Relaxed);
        c.data_size.fetch_add(
            rocksdb_encoded_len(key.len(), value.len()),
            AtomicOrdering::Relaxed,
        );
        match value_type {
            ValueType::Deletion | ValueType::SingleDeletion | ValueType::DeletionWithTimestamp => {
                c.num_deletes.fetch_add(1, AtomicOrdering::Relaxed);
            }
            ValueType::RangeDeletion => {
                c.num_range_deletes.fetch_add(1, AtomicOrdering::Relaxed);
                c.range_del_empty.store(false, AtomicOrdering::Release);
            }
            _ => {}
        }
        // One writer, so a load then a store is the C++'s compare-exchange loop.
        let first = c.first_seqno.load(AtomicOrdering::Relaxed);
        if first == 0 || s < first {
            c.first_seqno.store(s, AtomicOrdering::Relaxed);
        }
        let earliest = c.earliest_seqno.load(AtomicOrdering::Relaxed);
        if earliest == MAX_SEQUENCE_NUMBER || s < earliest {
            c.earliest_seqno.store(s, AtomicOrdering::Relaxed);
        }
        self.update_flush_state();
        Ok(())
    }

    /// `UpdateFlushState` [R db/memtable.cc:378-388]: records the trigger once it fires.
    fn update_flush_state(&mut self) {
        let c = self.counters;
        c.memory_allocated
            .store(self.alloc.memory_allocated_bytes(), AtomicOrdering::Relaxed);
        c.approximate_memory_usage.store(
            self.alloc.approximate_memory_usage(),
            AtomicOrdering::Relaxed,
        );
        if !c.flush_requested.load(AtomicOrdering::Relaxed) && self.should_flush_now() {
            c.flush_requested.store(true, AtomicOrdering::Relaxed);
        }
    }

    /// `ShouldFlushNow` [R db/memtable.cc:295-367].
    fn should_flush_now(&self) -> bool {
        let max = self.state.options.max_range_deletions;
        if max > 0
            && self
                .counters
                .num_range_deletes
                .load(AtomicOrdering::Relaxed)
                >= max
        {
            return true;
        }
        let wide = |x: usize| u128::try_from(x).unwrap_or(u128::MAX >> 8);
        let wbs = wide(self.state.options.write_buffer_size);
        let block = wide(self.state.arena_block_size);
        let allocated = wide(self.alloc.memory_allocated_bytes());
        let den = OVER_ALLOCATION_DENOMINATOR;
        let num = OVER_ALLOCATION_NUMERATOR;
        // allocated + block < wbs + 0.6·block, times 5. Each term is below 2^64, so the
        // products and sums fit u128.
        let limit = den
            .saturating_mul(wbs)
            .saturating_add(num.saturating_mul(block));
        if den.saturating_mul(allocated.saturating_add(block)) < limit {
            return false;
        }
        if den.saturating_mul(allocated) > limit {
            return true;
        }
        // The last block is allocated: stop once it is three-quarters full.
        self.alloc.allocated_and_unused() < self.state.arena_block_size / 4
    }
}

/// The bytes RocksDB's entry encoding takes (`encoded_len` [R db/memtable.cc:1130-1132]):
/// `data_size` counts these, so statistics match RocksDB's.
fn rocksdb_encoded_len(key_len: usize, value_len: usize) -> u64 {
    let wide = |x: usize| u64::try_from(x).unwrap_or(u64::MAX);
    let ikey = wide(key_len.saturating_add(NUM_INTERNAL_BYTES));
    let v = wide(value_len);
    let varint = |x: u64| wide(crate::util::coding::varint_length(x));
    varint(ikey)
        .saturating_add(ikey)
        .saturating_add(varint(v))
        .saturating_add(v)
}
