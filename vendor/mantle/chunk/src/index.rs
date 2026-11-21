//! The in-memory index from chunk key to the fragments that hold its bytes, and the table of
//! segment states. Haystack keeps its needle index in memory so a read costs one disk
//! operation (Beaver et al., OSDI 2010, §3.4); this index does the same, and its size is
//! bounded by the volume's chunk budget.
//!
//! Beside the map the index keeps where each live record starts, ordered by segment and
//! offset: LFS's segment summary, kept in memory (Rosenblum and Ousterhout, SOSP 1991,
//! §3.3). The scrubber and cleaner find a segment's records there, in the order they lie on
//! the device, without walking the map. It costs about 20 bytes a fragment: 19.6 measured
//! with a counting allocator for 4M records appended in order, 15.9 once a third are
//! deleted at random (docs/design/chunk-store.md §8).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;

use crate::frame::{PutRecord, SegmentState};
use crate::key::ChunkKey;
use crate::record::FLAG_FINAL;

/// Where one contiguous piece of a chunk's bytes lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fragment {
    pub segment: u32,
    pub incarnation: u64,
    /// Byte offset of the data record within its segment.
    pub offset: u32,
    /// Bytes of the whole data record.
    pub record_len: u32,
    pub chunk_offset: u64,
    pub payload_len: u32,
    pub payload_crc: u32,
    pub sequence: u64,
}

impl Fragment {
    pub fn end(&self) -> u64 {
        self.chunk_offset
            .saturating_add(u64::from(self.payload_len))
    }

    pub fn from_put(p: &PutRecord) -> Self {
        Self {
            segment: p.segment,
            incarnation: p.incarnation,
            offset: p.offset,
            record_len: p.record_len,
            chunk_offset: p.chunk_offset,
            payload_len: p.payload_len,
            payload_crc: p.payload_crc,
            sequence: p.sequence,
        }
    }
}

/// A chunk: its fragments in chunk order, contiguous from offset zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub fragments: Vec<Fragment>,
    pub sealed: bool,
    /// When the last fragment was written.
    pub time_ns: u64,
}

impl Entry {
    /// The chunk's length so far.
    pub fn len(&self) -> u64 {
        self.fragments.last().map_or(0, Fragment::end)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The fragment starting at `chunk_offset`, if one does.
    pub fn fragment_at(&self, chunk_offset: u64) -> Option<&Fragment> {
        self.fragments
            .binary_search_by_key(&chunk_offset, |f| f.chunk_offset)
            .ok()
            .and_then(|i| self.fragments.get(i))
    }

    /// The fragments overlapping `[start, end)`.
    pub fn covering(&self, start: u64, end: u64) -> impl Iterator<Item = &Fragment> {
        self.fragments
            .iter()
            .filter(move |f| f.chunk_offset < end && f.end() > start)
    }
}

/// What an insert did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inserted {
    Added,
    /// The fragment moved: this is where it was.
    Replaced(Fragment),
    Unchanged,
}

/// Why an index update was refused. The writer validates every request before it writes,
/// so these arise only from a log whose frames contradict each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inconsistent {
    /// A fragment that does not start where the chunk currently ends.
    NotContiguous { expected: u64, got: u64 },
    /// A fragment for a chunk already sealed.
    Sealed,
    /// More fragments than one chunk may have.
    TooManyFragments,
}

/// The bytes an index of chunks of one fragment each asks of its allocator a fragment, at most:
/// its table's bucket and control byte, its fragment list, and its place with its record's
/// length, at the peak a table's growth reaches while it holds its old buckets and its new.
/// Measured with hyper-measure's counting allocator through twenty doublings
/// (tests/index_memory.rs): 352.0 bytes a fragment at 917,505 fragments, the first past a growth,
/// which each growth repeats in proportion; 236.3 once the old buckets go. The lengths, which the
/// cleaner plans its passes from (clean.rs), are 6.7 of those (345.3 and 229.6 without them).
pub const FRAGMENT_BYTES: u64 = 352;

/// What the index of one fragment asks beyond [`FRAGMENT_BYTES`]: its table's first buckets and
/// its place map's first node (532 bytes measured for one on x86_64, whose table keeps its control
/// bytes in SSE2's groups of 16; 524 on aarch64, in groups of 8).
pub const INDEX_BYTES: u64 = 180;

/// The most bytes an index of `fragments` fragments, one a chunk, asks of its allocator.
pub fn most_bytes(fragments: u64) -> Option<u64> {
    FRAGMENT_BYTES
        .checked_mul(fragments)?
        .checked_add(INDEX_BYTES)
}

/// The most fragments, one a chunk, an index of at most `bytes` holds.
pub fn most_fragments(bytes: u64) -> u64 {
    bytes
        .saturating_sub(INDEX_BYTES)
        .checked_div(FRAGMENT_BYTES)
        .unwrap_or(0)
}

#[derive(Debug, Default)]
pub struct Index {
    map: HashMap<ChunkKey, Entry>,
    /// `(segment, offset)` of every fragment's record, with its payload's length. A live
    /// record's place names it: only the current incarnation of a segment holds live records.
    /// The lengths let the cleaner lay out a pass's relocations before it moves a byte.
    placed: BTreeMap<(u32, u32), u32>,
}

impl Index {
    pub fn get(&self, key: &ChunkKey) -> Option<&Entry> {
        self.map.get(key)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&ChunkKey, &Entry)> {
        self.map.iter()
    }

    /// Offsets of the live records that start in `offsets` of `segment`, in order.
    pub fn placed_in(&self, segment: u32, offsets: Range<u32>) -> Vec<u32> {
        self.placed
            .range((segment, offsets.start)..(segment, offsets.end))
            .map(|(&(_, offset), _)| offset)
            .collect()
    }

    /// The live records of `segment` in the order they lie: each one's offset and payload
    /// length.
    pub fn records_in(&self, segment: u32) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.placed
            .range((segment, 0)..=(segment, u32::MAX))
            .map(|(&(_, offset), &len)| (offset, len))
    }

    /// Whether a live record starts at `offset` of `segment`.
    pub fn is_placed(&self, segment: u32, offset: u32) -> bool {
        self.placed.contains_key(&(segment, offset))
    }

    /// The chunk and fragment whose record starts at `offset` of `segment`. It walks the
    /// whole map, so it serves only to name the owner of a record too damaged to name itself.
    pub fn owner(&self, segment: u32, offset: u32) -> Option<(ChunkKey, Fragment)> {
        self.map.iter().find_map(|(key, entry)| {
            entry
                .fragments
                .iter()
                .find(|f| f.segment == segment && f.offset == offset)
                .map(|f| (*key, *f))
        })
    }

    /// The fragment of `key` at `chunk_offset` if it is the record written at `offset` of
    /// `segment` with `incarnation` and `sequence`: that record is live.
    pub fn live(
        &self,
        key: &ChunkKey,
        chunk_offset: u64,
        at: (u32, u32),
        incarnation: u64,
        sequence: u64,
    ) -> Option<Fragment> {
        self.map
            .get(key)?
            .fragment_at(chunk_offset)
            .filter(|f| {
                (f.segment, f.offset) == at
                    && f.incarnation == incarnation
                    && f.sequence == sequence
            })
            .copied()
    }

    /// Adds a fragment written for `key`. The same bytes at the same chunk offset replace the
    /// fragment already there when written no earlier (a relocated copy, or the same record
    /// replayed twice); the outcome says which happened.
    pub fn insert(
        &mut self,
        key: ChunkKey,
        put: &PutRecord,
        max_fragments: usize,
    ) -> Result<Inserted, Inconsistent> {
        let fragment = Fragment::from_put(put);
        let sealed = put.flags & FLAG_FINAL != 0;
        let entry = self.map.entry(key).or_insert_with(|| Entry {
            fragments: Vec::with_capacity(1),
            sealed: false,
            time_ns: put.time_ns,
        });
        if let Some(existing) = entry.fragment_at(fragment.chunk_offset)
            && existing.payload_len == fragment.payload_len
            && existing.payload_crc == fragment.payload_crc
        {
            let existing = *existing;
            entry.sealed |= sealed;
            if existing == fragment || fragment.sequence < existing.sequence {
                return Ok(Inserted::Unchanged);
            }
            // The same bytes at a newer location: a relocated copy supersedes the old one.
            if let Ok(i) = entry
                .fragments
                .binary_search_by_key(&fragment.chunk_offset, |f| f.chunk_offset)
                && let Some(slot) = entry.fragments.get_mut(i)
            {
                *slot = fragment;
                self.placed.remove(&(existing.segment, existing.offset));
                self.placed
                    .insert((fragment.segment, fragment.offset), fragment.payload_len);
                return Ok(Inserted::Replaced(existing));
            }
            return Ok(Inserted::Unchanged);
        }
        if entry.sealed {
            return Err(Inconsistent::Sealed);
        }
        if entry.len() != fragment.chunk_offset {
            return Err(Inconsistent::NotContiguous {
                expected: entry.len(),
                got: fragment.chunk_offset,
            });
        }
        if entry.fragments.len() >= max_fragments {
            return Err(Inconsistent::TooManyFragments);
        }
        // A fragment may start where an empty one did only if that one ended the chunk,
        // which the sealed check above already refused.
        if entry.fragments.last().is_some_and(|f| f.payload_len == 0) {
            return Err(Inconsistent::NotContiguous {
                expected: entry.len(),
                got: fragment.chunk_offset,
            });
        }
        entry.fragments.push(fragment);
        entry.sealed = sealed;
        entry.time_ns = put.time_ns;
        self.placed
            .insert((fragment.segment, fragment.offset), fragment.payload_len);
        Ok(Inserted::Added)
    }

    pub fn remove(&mut self, key: &ChunkKey) -> Option<Entry> {
        let entry = self.map.remove(key)?;
        for f in &entry.fragments {
            self.placed.remove(&(f.segment, f.offset));
        }
        Some(entry)
    }

    /// Puts `fragment` back in place of the copy of the same bytes that replaced it; true if
    /// there was such a copy to put it back over.
    pub fn restore_fragment(&mut self, key: &ChunkKey, fragment: &Fragment) -> bool {
        let Some(entry) = self.map.get_mut(key) else {
            return false;
        };
        let Ok(i) = entry
            .fragments
            .binary_search_by_key(&fragment.chunk_offset, |f| f.chunk_offset)
        else {
            return false;
        };
        match entry.fragments.get_mut(i) {
            Some(slot)
                if slot.payload_len == fragment.payload_len
                    && slot.payload_crc == fragment.payload_crc =>
            {
                self.placed.remove(&(slot.segment, slot.offset));
                self.placed
                    .insert((fragment.segment, fragment.offset), fragment.payload_len);
                *slot = *fragment;
                true
            }
            _ => false,
        }
    }
}

/// One segment's state as the writer tracks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentInfo {
    pub state: SegmentState,
    pub incarnation: u64,
    /// Bytes of the segment written in this incarnation, a multiple of the block size.
    pub write_pos: u32,
    /// Bytes of records in this segment that some chunk still references.
    pub live: u64,
    /// When the youngest live record was written, for cleaning by age.
    pub youngest_ns: u64,
}

impl SegmentInfo {
    pub const FREE: Self = Self {
        state: SegmentState::Free,
        incarnation: 0,
        write_pos: 0,
        live: 0,
        youngest_ns: 0,
    };

    /// Written to and holding nothing live, so the writer can free it without copying. That
    /// includes a stream's open segment once every chunk written into it is deleted.
    pub fn reclaimable(&self, block: u64) -> bool {
        self.live == 0
            && (self.state == SegmentState::Sealed
                || (self.state == SegmentState::Open && u64::from(self.write_pos) > block))
    }
}

/// The segment table, with what the writer, the cleaner and the scrubber ask of it kept as
/// segments change: the free segments, the segments the writer can free without copying,
/// the count in each state and the live bytes. A batch, a scrub step or a count of free
/// segments then costs what changed rather than a pass over every segment, of which a 20 TB
/// device of 256 MiB segments has about 75,000 (audit P06).
#[derive(Debug, Clone)]
pub struct Segments {
    table: Vec<SegmentInfo>,
    block: u64,
    /// Free segments, lowest first.
    free: BTreeSet<u32>,
    /// Segments `SegmentInfo::reclaimable` holds for, lowest first.
    reclaimable: BTreeSet<u32>,
    open: u32,
    sealed: u32,
    live: u64,
    /// Segments changed since `take_changed`, each once, so never more than the table.
    changed: BTreeSet<u32>,
}

impl Segments {
    /// The table of `table`'s segments, written in blocks of `block` bytes.
    pub fn new(table: Vec<SegmentInfo>, block: u64) -> Self {
        let mut segments = Self {
            table: Vec::with_capacity(table.len()),
            block,
            free: BTreeSet::new(),
            reclaimable: BTreeSet::new(),
            open: 0,
            sealed: 0,
            live: 0,
            changed: BTreeSet::new(),
        };
        // A volume numbers its segments in a u32 (`Geometry::segments`).
        for info in table {
            let Ok(i) = u32::try_from(segments.table.len()) else {
                break;
            };
            segments.add(i, &info);
            segments.table.push(info);
        }
        segments
    }

    /// The number of segments.
    pub fn count(&self) -> usize {
        self.table.len()
    }

    pub fn get(&self, segment: u32) -> Option<&SegmentInfo> {
        self.table.get(usize::try_from(segment).ok()?)
    }

    /// Every segment with its number.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &SegmentInfo)> {
        self.table
            .iter()
            .enumerate()
            .filter_map(|(i, s)| Some((u32::try_from(i).ok()?, s)))
    }

    /// Free segments, lowest first.
    pub fn free(&self) -> impl Iterator<Item = u32> + '_ {
        self.free.iter().copied()
    }

    pub fn free_count(&self) -> usize {
        self.free.len()
    }

    /// Segments the writer can free without copying, lowest first.
    pub fn reclaimable(&self) -> impl Iterator<Item = u32> + '_ {
        self.reclaimable.iter().copied()
    }

    pub fn open_count(&self) -> u32 {
        self.open
    }

    pub fn sealed_count(&self) -> u32 {
        self.sealed
    }

    /// Bytes of records some chunk still references, over every segment.
    pub fn live(&self) -> u64 {
        self.live
    }

    /// Changes `segment` through `f`, keeping what the table keeps; a segment the table does
    /// not have is left alone.
    pub fn update(&mut self, segment: u32, f: impl FnOnce(&mut SegmentInfo)) {
        if self.change(segment, f) {
            self.changed.insert(segment);
        }
    }

    /// The segments `update` changed since the last call, lowest first.
    pub fn take_changed(&mut self) -> BTreeSet<u32> {
        std::mem::take(&mut self.changed)
    }

    /// Takes `from`'s state of each of `segments`.
    pub fn copy(&mut self, from: &Self, segments: &BTreeSet<u32>) {
        for &segment in segments {
            if let Some(&info) = from.get(segment) {
                self.change(segment, |s| *s = info);
            }
        }
    }

    fn change(&mut self, segment: u32, f: impl FnOnce(&mut SegmentInfo)) -> bool {
        let Some(slot) = usize::try_from(segment)
            .ok()
            .and_then(|i| self.table.get_mut(i))
        else {
            return false;
        };
        let old = *slot;
        f(slot);
        let new = *slot;
        if old == new {
            return false;
        }
        self.remove(segment, &old);
        self.add(segment, &new);
        true
    }

    fn add(&mut self, segment: u32, info: &SegmentInfo) {
        match info.state {
            SegmentState::Free => {
                self.free.insert(segment);
            }
            SegmentState::Open => self.open = self.open.saturating_add(1),
            SegmentState::Sealed => self.sealed = self.sealed.saturating_add(1),
        }
        if info.reclaimable(self.block) {
            self.reclaimable.insert(segment);
        }
        self.live = self.live.saturating_add(info.live);
    }

    fn remove(&mut self, segment: u32, info: &SegmentInfo) {
        match info.state {
            SegmentState::Free => {
                self.free.remove(&segment);
            }
            SegmentState::Open => self.open = self.open.saturating_sub(1),
            SegmentState::Sealed => self.sealed = self.sealed.saturating_sub(1),
        }
        self.reclaimable.remove(&segment);
        self.live = self.live.saturating_sub(info.live);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mantle_disk::measure::SplitMix64;

    /// What the table keeps, counted from scratch.
    fn recount(t: &Segments, block: u64) {
        let free: Vec<u32> = t
            .iter()
            .filter(|(_, s)| s.state == SegmentState::Free)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(t.free().collect::<Vec<_>>(), free);
        assert_eq!(t.free_count(), free.len());
        let reclaimable: Vec<u32> = t
            .iter()
            .filter(|(_, s)| s.reclaimable(block))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(t.reclaimable().collect::<Vec<_>>(), reclaimable);
        let count = |state| t.iter().filter(|(_, s)| s.state == state).count() as u32;
        assert_eq!(t.open_count(), count(SegmentState::Open));
        assert_eq!(t.sealed_count(), count(SegmentState::Sealed));
        assert_eq!(t.live(), t.iter().map(|(_, s)| s.live).sum::<u64>());
    }

    /// Through any changes the table's sets and sums match a count from scratch, and copying
    /// the segments it changed makes the readers' table equal to the writer's.
    #[test]
    fn the_segment_table_keeps_its_sets_and_sums_through_any_changes() {
        let block = 4096;
        let mut rng = SplitMix64::new(7);
        let mut table = Segments::new(vec![SegmentInfo::FREE; 64], block);
        let mut readers = table.clone();
        let states = [SegmentState::Free, SegmentState::Open, SegmentState::Sealed];
        for round in 0..4_000u64 {
            // Two numbers past the table's end, which change nothing.
            let segment = rng.below(66) as u32;
            let state = states[rng.below(3) as usize];
            let live = rng.below(3) * 4096;
            let write_pos = rng.below(3) as u32 * 4096;
            table.update(segment, |s| {
                s.state = state;
                s.live = live;
                s.write_pos = write_pos;
                s.incarnation = round;
            });
            recount(&table, block);
            if rng.below(8) == 0 {
                let changed = table.take_changed();
                assert!(changed.iter().all(|&s| s < 64));
                readers.copy(&table, &changed);
                assert!(readers.iter().eq(table.iter()));
                recount(&readers, block);
            }
        }
        assert_eq!(table.count(), 64);
    }

    fn put(offset: u64, len: u32, seq: u64, flags: u8) -> PutRecord {
        PutRecord {
            key: ChunkKey {
                block: 1,
                epoch: 0,
                index: 0,
            },
            segment: 0,
            incarnation: 1,
            offset: 4096,
            record_len: len + 100,
            chunk_offset: offset,
            payload_len: len,
            payload_crc: offset as u32 ^ len,
            sequence: seq,
            time_ns: seq,
            flags,
        }
    }

    #[test]
    fn appends_must_be_contiguous_and_stop_at_seal() {
        let key = ChunkKey {
            block: 1,
            epoch: 0,
            index: 0,
        };
        let mut index = Index::default();
        index.insert(key, &put(0, 10, 1, 0), 8).unwrap();
        index.insert(key, &put(10, 5, 2, 0), 8).unwrap();
        assert_eq!(
            index.insert(key, &put(20, 5, 3, 0), 8),
            Err(Inconsistent::NotContiguous {
                expected: 15,
                got: 20
            })
        );
        index.insert(key, &put(15, 5, 3, FLAG_FINAL), 8).unwrap();
        assert_eq!(
            index.insert(key, &put(20, 1, 4, 0), 8),
            Err(Inconsistent::Sealed)
        );
        let e = index.get(&key).unwrap();
        assert_eq!(e.len(), 20);
        assert!(e.sealed);
        assert_eq!(e.covering(12, 16).count(), 2);
    }

    #[test]
    fn replaying_a_fragment_twice_changes_nothing() {
        let key = ChunkKey {
            block: 1,
            epoch: 0,
            index: 0,
        };
        let mut index = Index::default();
        index.insert(key, &put(0, 10, 1, FLAG_FINAL), 8).unwrap();
        index.insert(key, &put(0, 10, 1, FLAG_FINAL), 8).unwrap();
        assert_eq!(index.get(&key).unwrap().fragments.len(), 1);
    }

    #[test]
    fn a_chunk_has_a_bounded_number_of_fragments() {
        let key = ChunkKey {
            block: 1,
            epoch: 0,
            index: 0,
        };
        let mut index = Index::default();
        index.insert(key, &put(0, 1, 1, 0), 2).unwrap();
        index.insert(key, &put(1, 1, 2, 0), 2).unwrap();
        assert_eq!(
            index.insert(key, &put(2, 1, 3, 0), 2),
            Err(Inconsistent::TooManyFragments)
        );
    }

    /// Checks that `placed` names exactly the records the map holds, with their lengths.
    fn check(index: &Index) {
        let expected: BTreeMap<(u32, u32), u32> = index
            .map
            .values()
            .flat_map(|e| {
                e.fragments
                    .iter()
                    .map(|f| ((f.segment, f.offset), f.payload_len))
            })
            .collect();
        assert_eq!(index.placed, expected);
    }

    /// Appends, relocations, restores, pops and removals in a random order keep the placed
    /// records equal to the map's.
    #[test]
    fn placed_records_follow_the_map() {
        let mut index = Index::default();
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut replaced: Vec<(ChunkKey, Fragment)> = Vec::new();
        let (mut place, mut sequence) = (0u32, 0u64);
        let put = |key: ChunkKey, at: u32, chunk_offset: u64, crc: u32, sequence: u64| PutRecord {
            key,
            segment: at % 7,
            incarnation: 1,
            offset: at,
            record_len: 100,
            chunk_offset,
            payload_len: 10,
            payload_crc: crc,
            sequence,
            time_ns: sequence,
            flags: 0,
        };
        for _ in 0..20_000 {
            let key = ChunkKey {
                block: u128::from(next() % 50),
                epoch: 0,
                index: 0,
            };
            place += 1;
            sequence += 1;
            match next() % 5 {
                0 | 1 => {
                    let end = index.get(&key).map_or(0, Entry::len);
                    let _ = index.insert(key, &put(key, place, end, end as u32, sequence), 8);
                }
                2 => {
                    let first = index.get(&key).and_then(|e| e.fragments.first()).copied();
                    if let Some(f) = first {
                        let moved = put(key, place, f.chunk_offset, f.payload_crc, sequence);
                        if let Ok(Inserted::Replaced(old)) = index.insert(key, &moved, 8) {
                            replaced.push((key, old));
                        }
                    }
                }
                3 => {
                    if let Some((key, old)) = replaced.pop() {
                        index.restore_fragment(&key, &old);
                    }
                }
                _ => {
                    index.remove(&key);
                }
            }
            check(&index);
        }
        assert!(!index.is_empty());
    }
}
