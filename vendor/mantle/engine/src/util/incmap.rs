//! A map of `u64` to `u64` that grows without stopping an operation to rehash it whole.
//!
//! A cache's index grows with the cache, on the read path: a table that doubles at once rehashes
//! every entry in one get, and touches a new table's pages there (measured: 546 to 2178 page
//! faults, 1.3 to 5.2 ms, in single gets of a 64 MiB record cache filling; research/37). Here a
//! full table starts a new one and each write then moves `MOVE` of the old table's buckets, as
//! Redis's dict rehashes a step an operation (`dict.c`, `_dictRehashStep`), so no single
//! operation pays for more than that.
//!
//! Each table is a SwissTable (Abseil's design, `abseil.io/about/design/swisstables`): a control
//! byte a bucket, holding 7 bits of the key's hash, read eight at a time as one word and matched
//! with word arithmetic (hashbrown's portable group, `src/control/group/generic.rs`), so a lookup
//! reads a group or two at a load up to 7/8 where linear probing reads tens of buckets. A key's
//! hash by Fibonacci hashing (Knuth, TAOCP vol. 3, §6.4), its tag and its group from different
//! high bits. Removal leaves a tombstone, so a migration or a sweep never moves an entry it has
//! not reached; tombstones are dropped when the table is rebuilt, which they count toward.

/// Buckets of the old table moved each write while a migration runs. Derived: a migration from
/// an old table of `C` buckets takes `C / MOVE` writes, each adding at most an entry and a
/// tombstone to the new table, so the new table ends it holding at most `len + C / 4` for
/// `MOVE = 8`. Built with `N >= 8/7 (len + 1 + C / 4)` buckets, it stays under its rebuild
/// threshold, `7/8` of `N`: a migration always ends before the next begins.
pub(crate) const MOVE: usize = 8;
/// Buckets a window's sweep looks at each time an entry is added (see `IncMap::sweep`).
/// Derived: an entry leaves the window after `W` newer ones, and a pass over the map's `B` buckets
/// takes `B / SWEEP` additions, each adding at most one entry, so expired entries number at most
/// `2 B / SWEEP = B / 8` (two passes). The map grows only when entries and tombstones pass `7/8`
/// of `B`; with expired entries within `B / 8`, that needs the window's entries near `3/4` of `B`.
/// So a window's map stays within a few times `W`.
pub const SWEEP: usize = 16;
/// Fibonacci hashing's multiplier, 2^64 over the golden ratio (Knuth, TAOCP vol. 3, §6.4).
pub(crate) const FIB: u64 = 0x9E37_79B9_7F4A_7C15;

/// Control bytes: a bucket never used since its table was built, a removed entry's, and a full
/// bucket's holding its tag, below 0x80 (hashbrown's encoding).
pub(crate) const EMPTY: u8 = 0xFF;
pub(crate) const DELETED: u8 = 0x80;
/// Buckets in a group: the control bytes in a word.
pub(crate) const GROUP: usize = 8;
/// A word of eight bytes each `b`.
pub(crate) const fn repeat(b: u8) -> u64 {
    u64::from_le_bytes([b; GROUP])
}
pub(crate) const LOW: u64 = repeat(0x01);
pub(crate) const HIGH: u64 = repeat(0x80);

/// One table: a power-of-two count of buckets, at least a group.
#[derive(Debug, Default)]
struct Table {
    /// Each group's control bytes as one word, byte `b` of word `g` for bucket `8 g + b`.
    ctrl: Vec<u64>,
    /// Each bucket's key and value side by side: a hit reads the control word's line and the
    /// entry's, as hashbrown's buckets are laid out.
    slots: Vec<[u64; 2]>,
    full: usize,
    tombs: usize,
    /// From a hash to its group: the bits below the tag's, `(m >> shift) & mask`.
    shift: u32,
    mask: usize,
}

impl Table {
    /// A table of `buckets` buckets, a power of two of at least a group. Keys and values are
    /// zeroed memory, touched as entries land in them, not when it is made.
    fn new(buckets: usize) -> Self {
        let groups = (buckets / GROUP).max(1);
        Self {
            ctrl: vec![repeat(EMPTY); groups],
            slots: vec![[0; 2]; groups.saturating_mul(GROUP)],
            full: 0,
            tombs: 0,
            shift: 57u32.saturating_sub(groups.trailing_zeros()),
            mask: groups.wrapping_sub(1),
        }
    }

    fn buckets(&self) -> usize {
        self.slots.len()
    }

    /// The key's tag, its hash's top 7 bits, and its first group, from the bits below them.
    #[inline]
    fn hash(&self, key: u64) -> (u8, usize) {
        let m = key.wrapping_mul(FIB);
        let tag = u8::try_from(m >> 57).unwrap_or(0);
        let group = usize::try_from(m.wrapping_shr(self.shift)).unwrap_or(0) & self.mask;
        (tag, group)
    }

    /// The bucket holding `key`, or else the first empty or deleted bucket its probe passed: a
    /// plain loop over the probe's groups, as hashbrown's, which the iterator form of it ran
    /// several times slower than (benches/incmap.rs).
    #[inline]
    fn seek(&self, key: u64, tag: u8, start: usize) -> Result<usize, Option<usize>> {
        let pattern = repeat(tag);
        let mut free = None;
        let mut g = start;
        // Each round reads a group; triangular steps visit each group once.
        for stride in 1..=self.ctrl.len() {
            let w = self.ctrl.get(g).copied().unwrap_or(0);
            let at = g.wrapping_mul(GROUP);
            // Bytes equal to the tag; a false positive is caught by the key's comparison.
            let cmp = w ^ pattern;
            let mut m = cmp.wrapping_sub(LOW) & !cmp & HIGH;
            while m != 0 {
                let i = at.wrapping_add(usize::try_from(m.trailing_zeros() / 8).unwrap_or(0));
                if self.slots.get(i).is_some_and(|e| e[0] == key) {
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

    #[inline]
    fn find(&self, key: u64) -> Option<usize> {
        let (tag, start) = self.hash(key);
        self.seek(key, tag, start).ok()
    }

    #[inline]
    fn get(&self, key: u64) -> Option<u64> {
        self.find(key).and_then(|i| self.slots.get(i)).map(|e| e[1])
    }

    /// Bucket `i`'s control byte.
    fn ctrl_at(&self, i: usize) -> u8 {
        let shift = u32::try_from(i % GROUP).unwrap_or(0).wrapping_mul(8);
        self.ctrl
            .get(i / GROUP)
            .and_then(|w| u8::try_from(w.wrapping_shr(shift) & 0xFF).ok())
            .unwrap_or(EMPTY)
    }

    fn set_ctrl(&mut self, i: usize, c: u8) {
        let shift = u32::try_from(i % GROUP).unwrap_or(0).wrapping_mul(8);
        if let Some(w) = self.ctrl.get_mut(i / GROUP) {
            *w = (*w & !0xFFu64.wrapping_shl(shift)) | u64::from(c).wrapping_shl(shift);
        }
    }

    /// Sets `key` to `val`; the value it replaced. One probe finds the key or the bucket for it.
    fn insert(&mut self, key: u64, val: u64) -> Option<u64> {
        let (tag, start) = self.hash(key);
        let at = match self.seek(key, tag, start) {
            Ok(i) => {
                let e = self.slots.get_mut(i)?;
                return Some(std::mem::replace(&mut e[1], val));
            }
            Err(free) => free?,
        };
        if self.ctrl_at(at) == DELETED {
            self.tombs = self.tombs.saturating_sub(1);
        }
        self.set_ctrl(at, tag);
        *self.slots.get_mut(at)? = [key, val];
        self.full = self.full.saturating_add(1);
        None
    }

    fn is_full(&self, i: usize) -> bool {
        self.ctrl_at(i) & 0x80 == 0
    }

    /// Takes the entry at bucket `i`, leaving a tombstone.
    fn take(&mut self, i: usize) -> Option<(u64, u64)> {
        if !self.is_full(i) {
            return None;
        }
        self.set_ctrl(i, DELETED);
        self.full = self.full.saturating_sub(1);
        self.tombs = self.tombs.saturating_add(1);
        self.slots.get(i).map(|e| (e[0], e[1]))
    }

    fn remove(&mut self, key: u64) -> Option<u64> {
        let i = self.find(key)?;
        self.take(i).map(|(_, v)| v)
    }

    /// Whether one more entry passes `7/8` of the buckets, tombstones counted.
    fn crowded(&self) -> bool {
        self.full
            .saturating_add(self.tombs)
            .saturating_add(1)
            .saturating_mul(8)
            > self.buckets().saturating_mul(7)
    }
}

/// The map: its table, the old one a migration is emptying, and how far it has come.
#[derive(Debug, Default)]
pub struct IncMap {
    table: Table,
    old: Option<Table>,
    moved: usize,
    /// The next bucket a sweep looks at.
    swept: usize,
}

impl IncMap {
    /// An empty map; its first table is made with its first entry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Empties the map, keeping its table: refilled to the size it last held, it neither grows
    /// nor migrates again.
    pub fn clear(&mut self) {
        self.old = None;
        self.moved = 0;
        self.swept = 0;
        self.table.ctrl.fill(repeat(EMPTY));
        self.table.full = 0;
        self.table.tombs = 0;
    }

    /// Entries held.
    pub fn len(&self) -> usize {
        self.table
            .full
            .saturating_add(self.old.as_ref().map_or(0, |o| o.full))
    }

    /// Buckets in its table, the old one's while a migration runs not counted: a sweep of that
    /// many looks at every entry once.
    pub fn buckets(&self) -> usize {
        self.table.buckets()
    }

    /// The bytes its tables take, the old one's while a migration runs included.
    pub fn bytes(&self) -> usize {
        let of = |t: &Table| t.buckets().saturating_mul(17);
        of(&self.table).saturating_add(self.old.as_ref().map_or(0, of))
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn get(&self, key: u64) -> Option<u64> {
        self.table
            .get(key)
            .or_else(|| self.old.as_ref().and_then(|o| o.get(key)))
    }

    /// Reads `key`'s first group in each table, its control word and its first bucket, so a
    /// lookup made after finds them cached: several maps' misses overlapped rather than one after
    /// another (the shard's kept memtables' index, research/51 §7).
    #[inline]
    pub fn touch(&self, key: u64) -> u64 {
        let first = |t: &Table| {
            let (_, g) = t.hash(key);
            let w = t.ctrl.get(g).copied().unwrap_or(0);
            let e = t.slots.get(g.wrapping_mul(GROUP)).map_or(0, |e| e[1]);
            w ^ e
        };
        first(&self.table) ^ self.old.as_ref().map_or(0, first)
    }

    /// [`Self::get`] of a map read alone: the key's first group's control word and first bucket
    /// read together before the probe, as [`Self::touch`] reads several maps', so its two misses
    /// overlap; the key's group and tag computed once for both.
    #[inline]
    pub fn get_touched(&self, key: u64) -> Option<u64> {
        if self.old.is_some() {
            std::hint::black_box(self.touch(key));
            return self.get(key);
        }
        let t = &self.table;
        let (tag, g) = t.hash(key);
        let w = t.ctrl.get(g).copied().unwrap_or(0);
        let e = t.slots.get(g.wrapping_mul(GROUP)).map_or(0, |e| e[1]);
        std::hint::black_box(w ^ e);
        t.seek(key, tag, g)
            .ok()
            .and_then(|i| t.slots.get(i))
            .map(|e| e[1])
    }

    pub fn contains_key(&self, key: u64) -> bool {
        self.get(key).is_some()
    }

    /// Sets `key` to `val`; the value it replaced.
    pub fn insert(&mut self, key: u64, val: u64) -> Option<u64> {
        self.step();
        if self.table.buckets() == 0 || self.table.crowded() {
            // A migration still running here would break `MOVE`'s bound: it is finished first
            // rather than lose the entry, whatever it costs.
            while self.old.is_some() {
                self.step();
            }
            self.rebuild();
        }
        // After any rebuild: the key may be in the table that just became the old one.
        let was = self.old.as_mut().and_then(|o| o.remove(key));
        self.table.insert(key, val).or(was)
    }

    /// Removes `key`; its value.
    pub fn remove(&mut self, key: u64) -> Option<u64> {
        self.step();
        let old = self.old.as_mut().and_then(|o| o.remove(key));
        let was = self.table.remove(key).or(old);
        self.shrink();
        was
    }

    /// Starts a migration to a smaller table once removals leave this one at most `1/8` full:
    /// the mirror of growing at `7/8`, a table built at least a quarter full never shrinking
    /// at once, so a map that emptied gives its memory back.
    fn shrink(&mut self) {
        let sparse = self.table.full.saturating_mul(8) <= self.table.buckets();
        if self.old.is_none() && self.table.buckets() > GROUP && sparse {
            self.rebuild();
        }
    }

    /// Looks at the next `n` buckets of the table, from where the last sweep stopped, and removes
    /// the entries `keep` refuses: a bounded pass a write, for entries that expire.
    pub fn sweep(&mut self, n: usize, keep: impl Fn(u64, u64) -> bool) {
        let buckets = self.table.buckets();
        if buckets == 0 {
            return;
        }
        for _ in 0..n.min(buckets) {
            let i = self.swept & buckets.wrapping_sub(1);
            self.swept = i.wrapping_add(1);
            let refused =
                self.table.is_full(i) && self.table.slots.get(i).is_some_and(|e| !keep(e[0], e[1]));
            if refused {
                self.table.take(i);
            }
        }
        self.shrink();
    }

    /// A migration's step for a read: the caches' reads call it, so a phase of reads alone
    /// finishes a migration that writes began, and lookups go back to probing one table.
    #[inline]
    pub fn settle(&mut self) {
        if self.old.is_some() {
            self.step();
        }
    }

    /// Whether a migration is running.
    pub fn migrating(&self) -> bool {
        self.old.is_some()
    }

    /// Starts a migration to a new table sized for the entries held and the writes the
    /// migration spans (see `MOVE`).
    fn rebuild(&mut self) {
        let c = self.table.buckets();
        // At most half full when built, and room for the migration's writes (see `MOVE`).
        let len = self.table.full.saturating_add(1);
        let want = len
            .saturating_mul(2)
            .max((len.saturating_add(c / 4).saturating_mul(8) / 7).saturating_add(1));
        let buckets = want.checked_next_power_of_two().unwrap_or(want);
        let old = std::mem::replace(&mut self.table, Table::new(buckets));
        self.swept = 0;
        if old.full > 0 {
            self.old = Some(old);
            self.moved = 0;
        }
    }

    /// Moves the next `MOVE` buckets of the old table, ending the migration at its last.
    fn step(&mut self) {
        let Some(old) = self.old.as_mut() else {
            return;
        };
        for _ in 0..MOVE {
            if self.moved >= old.buckets() || old.full == 0 {
                self.old = None;
                return;
            }
            if let Some((k, v)) = old.take(self.moved) {
                self.table.insert(k, v);
            }
            self.moved = self.moved.saturating_add(1);
        }
    }

    /// Every entry, in no order.
    #[cfg(test)]
    pub fn entries(&self) -> Vec<(u64, u64)> {
        let mut all = Vec::new();
        for t in std::iter::once(&self.table).chain(self.old.iter()) {
            for i in 0..t.buckets() {
                if t.is_full(i) {
                    all.push((t.slots[i][0], t.slots[i][1]));
                }
            }
        }
        all
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn it_answers_as_a_map_does_through_every_migration() {
        let mut m = IncMap::new();
        let mut model: HashMap<u64, u64> = HashMap::new();
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut migrations = 0;
        for step in 0..400_000u64 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            // Keys from a range that grows then shrinks: the map grows, fills with tombstones,
            // and rebuilds smaller.
            let span = 1 + (step % 100_000).min(100_000 - step % 100_000);
            let key = x % span;
            let had = m.old.is_some();
            match x >> 60 {
                0..=9 => assert_eq!(m.insert(key, step), model.insert(key, step), "{step}"),
                10..=13 => assert_eq!(m.remove(key), model.remove(&key), "{step}"),
                _ => assert_eq!(m.get(key), model.get(&key).copied(), "{step}"),
            }
            assert_eq!(m.len(), model.len(), "{step}");
            if !had && m.old.is_some() {
                migrations += 1;
            }
            // A migration ends before the table it fills is crowded.
            assert!(m.old.is_none() || !m.table.crowded(), "{step}");
        }
        // Grown from 16 buckets to 64 Ki, migrating each time while the model was checked.
        assert!(migrations >= 6, "only {migrations} migrations");
        let mut all: Vec<(u64, u64)> = m.entries();
        all.sort_unstable();
        let mut want: Vec<(u64, u64)> = model.into_iter().collect();
        want.sort_unstable();
        assert_eq!(all, want);
    }

    #[test]
    fn a_cleared_map_is_empty_and_refills_without_growing() {
        let mut m = IncMap::new();
        for k in 0..10_000u64 {
            m.insert(k.wrapping_mul(0x9e37_79b9_7f4a_7c15), k);
        }
        // Cleared mid-migration as well as settled: nothing is left in either table.
        m.clear();
        assert!(m.is_empty() && !m.migrating());
        let buckets = m.table.buckets();
        for k in 0..10_000u64 {
            assert_eq!(m.get(k.wrapping_mul(0x9e37_79b9_7f4a_7c15)), None);
            m.insert(k.wrapping_mul(0x9e37_79b9_7f4a_7c15), k + 1);
        }
        assert_eq!(m.table.buckets(), buckets);
        assert!(!m.migrating());
        for k in 0..10_000u64 {
            assert_eq!(m.get(k.wrapping_mul(0x9e37_79b9_7f4a_7c15)), Some(k + 1));
        }
    }

    #[test]
    fn no_write_moves_more_than_its_share() {
        // Growing to a million entries: each insert moves at most `MOVE` old buckets, and every
        // migration has ended before the table is crowded again.
        let mut m = IncMap::new();
        for k in 0..1_000_000u64 {
            let before = m.moved;
            let migrating = m.old.is_some();
            m.insert(k, k);
            if migrating && m.old.is_some() {
                assert!(m.moved - before <= MOVE);
            }
            assert!(m.old.is_none() || !m.table.crowded(), "{k}");
        }
        assert!((0..1_000_000u64).all(|k| m.get(k) == Some(k)));
    }

    #[test]
    fn a_map_emptied_by_removals_rebuilds_smaller() {
        let mut m = IncMap::new();
        for k in 0..100_000u64 {
            m.insert(k, k);
        }
        for k in 0..100_000u64 {
            m.remove(k);
        }
        let big = m.table.buckets();
        // A few keys churned through a table built for 100,000: each removal that leaves it at
        // most 1/8 full starts a smaller one, until it fits what it holds.
        for k in 100_000..400_000u64 {
            m.insert(k, k);
            m.remove(k.saturating_sub(4));
        }
        assert!(
            m.table.buckets() < big / 16,
            "{} of {big}",
            m.table.buckets()
        );
        assert_eq!(m.len(), 4);
    }

    #[test]
    fn a_sweep_removes_what_it_refuses_and_keeps_the_rest() {
        let mut m = IncMap::new();
        for k in 0..1000u64 {
            m.insert(k, k);
        }
        while m.old.is_some() {
            m.insert(5000, 5000);
            m.remove(5000);
        }
        m.sweep(m.table.buckets(), |_, v| v % 2 == 0);
        let mut left: Vec<u64> = m.entries().into_iter().map(|(k, _)| k).collect();
        left.sort_unstable();
        assert_eq!(left, (0..1000).step_by(2).collect::<Vec<_>>());
    }
}
