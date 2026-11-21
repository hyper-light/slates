//! One index over the memtables a shard keeps readable for gets (research/51 §7): each key's
//! newest entry among the memtables it covers, by the key's 64-bit filter hash
//! ([`crate::branch::filter::hash`]), as the memtable's number and the entry's arena offset.
//!
//! A packing worker walks each key's newest entry of a memtable's copy as it builds the branch,
//! and hands back the (hash, offset) pairs ([`KeyList`]); a maintenance worker adds the lists of
//! the memtables kept since to an index it was handed ([`KeptIndex::extend`]), the newer
//! memtable's entry replacing an older one's, and hands it back; the shard only probes it. A
//! probe's answer is the shard's to check: the memtable still kept, and its key at that offset
//! the one asked. Two keys of one hash share an entry, the newer memtable's: a get of the other
//! finds a key that is not its own, a miss, and reads the trunk, which holds every kept
//! memtable's records.
//!
//! Memtables are numbered by a counter that wraps: the memtables an index covers and the shard
//! keeps span far fewer than 2^31 numbers, so "older" is decided modulo 2^32 ([`older`]).

use crate::util::incmap::IncMap;

/// A memtable's keys as its packing worker walked them: each key's newest entry, its filter
/// hash and its arena offset, in two arrays so an entry takes 12 bytes.
#[derive(Debug, Default)]
pub struct KeyList {
    pub hashes: Vec<u64>,
    pub offsets: Vec<u32>,
}

impl KeyList {
    /// Empties the list, keeping its buffers for the next memtable's walk.
    pub fn clear(&mut self) {
        self.hashes.clear();
        self.offsets.clear();
    }

    pub fn len(&self) -> usize {
        self.hashes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }

    /// Adds an entry.
    pub fn push(&mut self, hash: u64, offset: u32) {
        self.hashes.push(hash);
        self.offsets.push(offset);
    }

    /// The bytes its buffers take, at their capacities.
    pub fn bytes(&self) -> usize {
        self.hashes
            .capacity()
            .saturating_mul(8)
            .saturating_add(self.offsets.capacity().saturating_mul(4))
    }
}

/// Whether memtable `a` was numbered before `b`, the counter having wrapped or not.
pub fn older(a: u32, b: u32) -> bool {
    let d = b.wrapping_sub(a);
    d != 0 && d < 1 << 31
}

/// A memtable's number and an entry's offset in its arena, as the index holds them.
fn pack(memtable: u32, offset: u32) -> u64 {
    (u64::from(memtable) << 32) | u64::from(offset)
}

fn unpack(v: u64) -> (u32, u32) {
    (
        u32::try_from(v >> 32).unwrap_or(0),
        u32::try_from(v & u64::from(u32::MAX)).unwrap_or(0),
    )
}

/// The index: each key's newest entry among the memtables it covers, and the newest memtable it
/// covers.
#[derive(Debug, Default)]
pub struct KeptIndex {
    map: IncMap,
    covers: Option<u32>,
    /// The entries of given-back memtables the shard had counted (its `kept_gone`) when this
    /// index was last swept: what has gone stale in it since is the count now less this.
    swept_at: u64,
}

impl KeptIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// The newest memtable whose keys the index holds, none before the first.
    pub fn covers(&self) -> Option<u32> {
        self.covers
    }

    /// The shard's count of given-back entries when this index was last swept.
    pub fn swept_at(&self) -> u64 {
        self.swept_at
    }

    /// Notes that the sweep this index is sent for drops what the shard has given back up to
    /// `gone` entries.
    pub fn set_swept_at(&mut self, gone: u64) {
        self.swept_at = gone;
    }

    /// Entries held, those of memtables given back included until a sweep drops them.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// The bytes its table takes.
    pub fn bytes(&self) -> usize {
        self.map.bytes()
    }

    /// The memtable and arena offset of the newest entry of a key of hash `hash`, if the index
    /// holds one.
    #[inline]
    pub fn get(&self, hash: u64) -> Option<(u32, u32)> {
        self.map.get(hash).map(unpack)
    }

    /// Reads `hash`'s first group, so a lookup after finds it cached ([`IncMap::touch`]).
    #[inline]
    pub fn touch(&self, hash: u64) -> u64 {
        self.map.touch(hash)
    }

    /// [`Self::get`] of the index read alone ([`IncMap::get_touched`]).
    #[inline]
    pub fn get_touched(&self, hash: u64) -> Option<(u32, u32)> {
        self.map.get_touched(hash).map(unpack)
    }

    /// Adds each memtable's keys, `lists` oldest first, passing over those it already covers: a
    /// key's entry in a newer memtable replaces its entry in an older one. With
    /// `sweep`, entries of memtables older than `oldest` (given back) are dropped first. The
    /// table ends settled: a lookup probes one table.
    pub fn extend(&mut self, lists: &[(u32, KeyList)], oldest: u32, sweep: bool) {
        if sweep {
            // Settled first, so one pass over the table looks at every entry.
            while self.map.migrating() {
                self.map.settle();
            }
            let buckets = self.map.buckets();
            self.map.sweep(buckets, |_, v| !older(unpack(v).0, oldest));
        }
        for (memtable, list) in lists {
            // A list it took on an earlier trip is passed over: its entries are in, or replaced
            // by newer ones.
            if self.covers.is_some_and(|c| !older(c, *memtable)) {
                continue;
            }
            for (&hash, &offset) in list.hashes.iter().zip(&list.offsets) {
                self.map.insert(hash, pack(*memtable, offset));
            }
            self.covers = Some(*memtable);
        }
        while self.map.migrating() {
            self.map.settle();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::HashMap;

    #[test]
    fn older_is_decided_across_the_counter_wrapping() {
        assert!(older(1, 2));
        assert!(!older(2, 1));
        assert!(!older(5, 5));
        assert!(older(u32::MAX, 0));
        assert!(older(u32::MAX - 3, 2));
        assert!(!older(2, u32::MAX - 3));
    }

    proptest! {
        /// Against a map of the newest entry per hash: lists added a few at a time, the numbers
        /// running past the counter's wrap, and memtables given back with a sweep, which drops
        /// exactly the entries whose newest memtable was given back.
        #[test]
        fn it_holds_each_key_s_newest_entry_among_the_memtables_it_covers(
            batches in prop::collection::vec(
                prop::collection::vec(prop::collection::vec((0u64..400, any::<u32>()), 0..60), 1..4),
                1..8,
            ),
            start in prop_oneof![Just(0u32), Just(u32::MAX - 5)],
            gone in 0u32..6,
        ) {
            let mut index = KeptIndex::new();
            let mut model: HashMap<u64, (u32, u32)> = HashMap::new();
            let mut next = start;
            for batch in &batches {
                let mut lists = Vec::new();
                for keys in batch {
                    let mut list = KeyList::default();
                    // A memtable holds a key once: its newest entry.
                    let mut seen = std::collections::HashSet::new();
                    for &(h, off) in keys {
                        if seen.insert(h) {
                            list.push(h, off);
                            model.insert(h, (next, off));
                        }
                    }
                    lists.push((next, list));
                    next = next.wrapping_add(1);
                }
                // The lists taken again, as a trip that carries them for the other index does:
                // nothing changes.
                let before = index.len();
                index.extend(&lists, start, false);
                index.extend(&lists, start, false);
                prop_assert_eq!(index.covers(), Some(next.wrapping_sub(1)));
                prop_assert!(index.len() >= before);
            }
            for (h, want) in &model {
                prop_assert_eq!(index.get(*h), Some(*want));
            }
            // Give back the oldest `gone` memtables: the sweep drops their entries, no other.
            let oldest = start.wrapping_add(gone);
            index.extend(&[], oldest, true);
            for (h, want) in &model {
                let kept = if older(want.0, oldest) { None } else { Some(*want) };
                prop_assert_eq!(index.get(*h), kept);
            }
        }
    }
}
