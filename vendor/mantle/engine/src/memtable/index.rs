//! A memtable's index of each key's newest entry: a SwissTable of the entries' arena offsets,
//! growing a few buckets a write, as `util::incmap` grows.
//!
//! `IncMap` keeps a 64-bit key and a 64-bit value a bucket, 17 bytes with its control byte, so a
//! 64 MiB memtable of 560 Ki entries spent 17 MiB on it. Here a bucket holds only the entry's
//! 32-bit offset, 5 bytes: the key a probe compares is the entry's own, read from the arena, and
//! a migration takes each entry's hash again from its key. Keys compare by their bytes, so two
//! keys of one 64-bit hash are two entries, with no list of clashes beside the table.
//!
//! Probing is `IncMap`'s (Abseil's SwissTable: a control byte a bucket, 7 bits of the hash, read
//! eight at a time; hashbrown's portable group); growth is `IncMap`'s too, a full table starting a
//! new one and each write moving `MOVE` of the old one's buckets (Redis's `dict.c`
//! `_dictRehashStep`), so no write rehashes the table whole.

// Probing's and migration's constants are `util::incmap`'s, derived and cited there: this index
// differs only in what a bucket holds.
use crate::util::incmap::{DELETED, EMPTY, FIB, GROUP, HIGH, LOW, MOVE, repeat};

/// One table: a power-of-two count of buckets, at least a group.
#[derive(Debug, Default)]
struct Table {
    /// Each group's control bytes as one word, byte `b` of word `g` for bucket `8 g + b`.
    ctrl: Vec<u64>,
    /// Each bucket's entry offset.
    slots: Vec<u32>,
    full: usize,
    /// From a hash to its group: the bits below the tag's, `(m >> shift) & mask`.
    shift: u32,
    mask: usize,
}

impl Table {
    fn new(buckets: usize) -> Self {
        let groups = (buckets / GROUP).max(1);
        Self {
            ctrl: vec![repeat(EMPTY); groups],
            slots: vec![0; groups.saturating_mul(GROUP)],
            full: 0,
            shift: 57u32.saturating_sub(groups.trailing_zeros()),
            mask: groups.wrapping_sub(1),
        }
    }

    fn buckets(&self) -> usize {
        self.slots.len()
    }

    /// A hash's tag, its top 7 bits after mixing, and its first group.
    #[inline]
    fn place(&self, hash: u64) -> (u8, usize) {
        let m = hash.wrapping_mul(FIB);
        let tag = u8::try_from(m >> 57).unwrap_or(0);
        let group = usize::try_from(m.wrapping_shr(self.shift)).unwrap_or(0) & self.mask;
        (tag, group)
    }

    /// The bucket holding an entry whose key `same` accepts, or else the first empty bucket the
    /// probe passed.
    #[inline]
    fn seek(&self, hash: u64, same: impl Fn(u32) -> bool) -> Result<usize, Option<usize>> {
        let (tag, mut g) = self.place(hash);
        let pattern = repeat(tag);
        let mut free = None;
        // Each round reads a group; triangular steps visit each group once.
        for stride in 1..=self.ctrl.len() {
            let w = self.ctrl.get(g).copied().unwrap_or(0);
            let at = g.wrapping_mul(GROUP);
            let cmp = w ^ pattern;
            let mut m = cmp.wrapping_sub(LOW) & !cmp & HIGH;
            while m != 0 {
                let i = at.wrapping_add(usize::try_from(m.trailing_zeros() / 8).unwrap_or(0));
                if self.slots.get(i).is_some_and(|&e| same(e)) {
                    return Ok(i);
                }
                m &= m.wrapping_sub(1);
            }
            if free.is_none() && w & HIGH != 0 {
                let b = usize::try_from((w & HIGH).trailing_zeros() / 8).unwrap_or(0);
                free = Some(at.wrapping_add(b));
            }
            // An empty bucket ends the probe: the key would have been placed by then.
            if w & (w << 1) & HIGH != 0 {
                return Err(free);
            }
            g = g.wrapping_add(stride) & self.mask;
        }
        Err(free)
    }

    fn set_ctrl(&mut self, i: usize, c: u8) {
        let shift = u32::try_from(i % GROUP).unwrap_or(0).wrapping_mul(8);
        if let Some(w) = self.ctrl.get_mut(i / GROUP) {
            *w = (*w & !0xFFu64.wrapping_shl(shift)) | u64::from(c).wrapping_shl(shift);
        }
    }

    fn is_full(&self, i: usize) -> bool {
        let shift = u32::try_from(i % GROUP).unwrap_or(0).wrapping_mul(8);
        self.ctrl
            .get(i / GROUP)
            .is_some_and(|w| w.wrapping_shr(shift) & 0x80 == 0)
    }

    /// Puts `entry` in the bucket an entry `same` accepts holds, or a new one: the entry it
    /// replaced. `None` with no bucket free, which a table under its threshold always has.
    fn put(&mut self, hash: u64, entry: u32, same: impl Fn(u32) -> bool) -> Option<Option<u32>> {
        match self.seek(hash, same) {
            Ok(i) => {
                let e = self.slots.get_mut(i)?;
                Some(Some(std::mem::replace(e, entry)))
            }
            Err(free) => {
                let at = free?;
                let (tag, _) = self.place(hash);
                self.set_ctrl(at, tag);
                *self.slots.get_mut(at)? = entry;
                self.full = self.full.saturating_add(1);
                Some(None)
            }
        }
    }

    /// Whether one more entry passes `7/8` of the buckets.
    fn crowded(&self) -> bool {
        self.full.saturating_add(1).saturating_mul(8) > self.buckets().saturating_mul(7)
    }
}

/// The index: its table, the old one a migration is emptying, and how far it has come.
#[derive(Debug, Default)]
pub struct KeyIndex {
    table: Table,
    old: Option<Table>,
    moved: usize,
}

impl KeyIndex {
    /// An index whose table holds `entries` at most half full, the size a migration starts
    /// (`rebuild`): a memtable that fills to as many entries as the one before it neither grows
    /// nor migrates, as a cleared one refilled does not.
    pub fn for_entries(entries: usize) -> Self {
        if entries == 0 {
            return Self::default();
        }
        let want = entries.saturating_mul(2).max(GROUP);
        let buckets = want.checked_next_power_of_two().unwrap_or(want);
        Self {
            table: Table::new(buckets),
            old: None,
            moved: 0,
        }
    }

    /// Reads `hash`'s first group in each table, its control word and its first bucket's entry,
    /// so a probe made after finds them cached: several indexes' misses overlapped rather than
    /// one after another (the shard's kept memtables, research/51).
    #[inline]
    pub fn touch(&self, hash: u64) -> u64 {
        let first = |t: &Table| {
            let (_, g) = t.place(hash);
            let w = t.ctrl.get(g).copied().unwrap_or(0);
            let e = t
                .slots
                .get(g.wrapping_mul(GROUP))
                .copied()
                .map_or(0, u64::from);
            w ^ e
        };
        first(&self.table) ^ self.old.as_ref().map_or(0, first)
    }

    /// Each key's newest entry, in no order: every entry the index holds, in either table while a
    /// migration runs.
    pub fn each_entry(&self, mut each: impl FnMut(u32)) {
        for t in std::iter::once(&self.table).chain(self.old.as_ref()) {
            for (i, &e) in t.slots.iter().enumerate() {
                if t.is_full(i) {
                    each(e);
                }
            }
        }
    }

    /// Empties the index, keeping its table: refilled to the size it last held, it neither grows
    /// nor migrates again.
    pub fn clear(&mut self) {
        self.old = None;
        self.moved = 0;
        self.table.ctrl.fill(repeat(EMPTY));
        self.table.full = 0;
    }

    /// The bytes its tables take, the old one's while a migration runs included.
    pub fn bytes(&self) -> usize {
        let of = |t: &Table| {
            t.buckets()
                .saturating_mul(size_of::<u32>().saturating_add(1))
        };
        of(&self.table).saturating_add(self.old.as_ref().map_or(0, of))
    }

    /// The newest entry of a key of hash `hash` that `same` accepts.
    #[inline]
    pub fn get(&self, hash: u64, same: impl Fn(u32) -> bool) -> Option<u32> {
        let found = |t: &Table| {
            t.seek(hash, &same)
                .ok()
                .and_then(|i| t.slots.get(i).copied())
        };
        found(&self.table).or_else(|| self.old.as_ref().and_then(found))
    }

    /// Makes `entry` the newest of its key, of hash `hash`: the entry it replaced, if the key had
    /// one. `same` accepts the entries of the same key; `hash_of` gives an entry's hash, for those
    /// a migration moves.
    pub fn put(
        &mut self,
        hash: u64,
        entry: u32,
        same: impl Fn(u32) -> bool,
        hash_of: impl Fn(u32) -> u64,
    ) -> Option<u32> {
        self.step(&hash_of);
        if self.table.buckets() == 0 || self.table.crowded() {
            // A migration still running would break `MOVE`'s bound: it is finished first.
            while self.old.is_some() {
                self.step(&hash_of);
            }
            self.rebuild();
        }
        // The key may be in the old table still: its entry there is taken out, so a get finds
        // the newest in the new one.
        let was = self.old.as_mut().and_then(|o| {
            let i = o.seek(hash, &same).ok()?;
            o.set_ctrl(i, DELETED);
            o.full = o.full.saturating_sub(1);
            o.slots.get(i).copied()
        });
        self.table.put(hash, entry, &same).flatten().or(was)
    }

    /// Starts a migration to a table at most half full, with room for the writes the migration
    /// spans (`util::incmap`'s rule).
    fn rebuild(&mut self) {
        let c = self.table.buckets();
        let len = self.table.full.saturating_add(1);
        let want = len
            .saturating_mul(2)
            .max((len.saturating_add(c / 4).saturating_mul(8) / 7).saturating_add(1));
        let buckets = want.checked_next_power_of_two().unwrap_or(want);
        let old = std::mem::replace(&mut self.table, Table::new(buckets));
        if old.full > 0 {
            self.old = Some(old);
            self.moved = 0;
        }
    }

    /// Moves the next `MOVE` buckets of the old table, each entry by its key's hash again.
    fn step(&mut self, hash_of: &impl Fn(u32) -> u64) {
        let Some(old) = self.old.as_mut() else {
            return;
        };
        for _ in 0..MOVE {
            if self.moved >= old.buckets() || old.full == 0 {
                self.old = None;
                return;
            }
            let i = self.moved;
            if old.is_full(i)
                && let Some(&e) = old.slots.get(i)
            {
                old.set_ctrl(i, DELETED);
                old.full = old.full.saturating_sub(1);
                // An entry moving is the only one of its key: nothing for `same` to match.
                self.table.put(hash_of(e), e, |_| false);
            }
            self.moved = self.moved.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Keys as offsets into a list standing for the arena; a key's hash chosen by the test, so
    /// keys of one hash are distinct entries.
    #[test]
    fn it_answers_as_a_map_does_through_every_migration_even_for_keys_of_one_hash() {
        let mut keys: Vec<u64> = Vec::new();
        let hash_of_key = |k: u64| {
            if k.is_multiple_of(7) {
                42
            } else {
                k.wrapping_mul(0x2545_f491)
            }
        };
        let mut index = KeyIndex::default();
        let mut model: HashMap<u64, u32> = HashMap::new();
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..300_000u64 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let k = x % 50_000;
            let entry = u32::try_from(keys.len()).unwrap();
            keys.push(k);
            let h = hash_of_key(k);
            let was = index.put(
                h,
                entry,
                |e| keys[e as usize] == k,
                |e| hash_of_key(keys[e as usize]),
            );
            assert_eq!(was, model.insert(k, entry));
            if x.is_multiple_of(5) {
                let probe = (x >> 20) % 50_000;
                let got = index.get(hash_of_key(probe), |e| keys[e as usize] == probe);
                assert_eq!(got, model.get(&probe).copied());
            }
        }
        for (&k, &e) in &model {
            assert_eq!(
                index.get(hash_of_key(k), |f| keys[f as usize] == k),
                Some(e)
            );
        }
        // Five bytes a bucket, a table at most twice its entries' next power of two.
        assert!(
            index.bytes() <= 5 * 4 * model.len().next_power_of_two(),
            "{}",
            index.bytes()
        );
    }
}
