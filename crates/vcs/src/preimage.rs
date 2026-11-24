//! An image cut's pre-images of one family of entries (A-870 piece 2, F2: copy-on-write by entry; main 01a20182's
//! rulings of 2026-11-23).
//!
//! **Why.** A shard's image of its paged families is encoded a slice a turn, so the publication that carries a cut
//! carries pages encoded over several turns. Read as the entries stand at each turn, a change after the cut lands in
//! some pages and not others: a window let go after the cut sits placed in the windows page and away in the aways page,
//! two moments in one image, whose restore can claim a block another window now holds. Encoding all of the cut's pages
//! in one step instead is user-scaled work in a step (a whole push's dirty pages), counted as a bug signal.
//!
//! **What.** A cut freezes the coordinate ranges of its pages ([`Preimages::freeze`]). A store about to change an entry
//! whose coordinate lies in a frozen range keeps that entry's value at the cut first, once ([`Preimages::before`]): its
//! old value, or none where the cut had no entry under its key (the absent marker keeps the key alone). The cut's pages
//! are read as the store's entries now with the pre-images laid over them ([`Preimages::overlay`]), so every page is the
//! cut's moment whatever is changed after it. A page's range is thawed and its pre-images dropped once it is encoded
//! ([`Preimages::thaw`]).
//!
//! **Bound.** The pre-images held at once are at most E + I: E the entries in the cut's pages at the cut (each kept once,
//! by key, its old value), I the keys inserted into those pages while the cut is encoded (each kept once, its key).
//! Derived: I is at most the inserts one turn's step budget admits into the cut's pages, times the turns the cut lasts,
//! which is at most its pages and the pages its splits put back, since a turn encodes or splits one at least (a page a
//! gap left is read a cap at a time, A-870 F2); those are at most twice the cut's bytes over the page size, plus its
//! pages at the cut, piece 1's bound on a family's pages. Each factor is known at the cut.

use std::collections::BTreeMap;

use crate::ContentId;

/// A key of a paged family: its coordinate (the page space's) and the range of keys a coordinate range holds.
pub trait Paged: Ord + Copy {
  /// The key's coordinate.
  fn coordinate(&self) -> u64;
  /// The first and last keys whose coordinates lie in `lo..=hi`.
  fn keys_in(lo: u64, hi: u64) -> core::ops::RangeInclusive<Self>;
}

impl Paged for u64 {
  fn coordinate(&self) -> u64 {
    *self
  }

  fn keys_in(lo: u64, hi: u64) -> core::ops::RangeInclusive<u64> {
    lo..=hi
  }
}

impl Paged for ContentId {
  /// Format: an identity's first eight bytes read big-endian (A-870 piece 1).
  fn coordinate(&self) -> u64 {
    let mut first = [0u8; 8];
    first.copy_from_slice(&self.0[..8]);
    u64::from_be_bytes(first)
  }

  fn keys_in(lo: u64, hi: u64) -> core::ops::RangeInclusive<ContentId> {
    let mut first = [0u8; crate::content::IDENTITY_BYTES];
    let mut last = [u8::MAX; crate::content::IDENTITY_BYTES];
    first[..8].copy_from_slice(&lo.to_be_bytes());
    last[..8].copy_from_slice(&hi.to_be_bytes());
    ContentId(first)..=ContentId(last)
  }
}

/// The pre-images kept since a family's counts were last taken ([`Preimages::take_counts`]): old values and absent
/// markers apart (main 01a20182, 2026-11-23: the oracle bounds each), and the bytes the old values copied.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PreimageCounts {
  /// Old values kept: at most the entries in the cut's pages at the cut (E).
  pub old: u64,
  /// Absent markers kept: at most the keys inserted into the cut's pages while it is encoded (I).
  pub absent: u64,
  /// The bytes the old values copied, as their owner sizes them.
  pub bytes: u64,
}

impl core::ops::AddAssign for PreimageCounts {
  fn add_assign(&mut self, other: PreimageCounts) {
    self.old = self.old.saturating_add(other.old);
    self.absent = self.absent.saturating_add(other.absent);
    self.bytes = self.bytes.saturating_add(other.bytes);
  }
}

/// One family's frozen ranges and pre-images (module docs).
#[derive(Debug)]
pub struct Preimages<K, V> {
  /// The frozen coordinate ranges, by first coordinate, each to its last; disjoint (a cut's pages are). Bound: the
  /// cut's pages of the family.
  frozen: BTreeMap<u64, u64>,
  /// Each key changed in a frozen range since its freeze, with its value at the cut (none: absent). Bound: E + I
  /// (module docs).
  kept: BTreeMap<K, Option<V>>,
  /// The pre-images kept since the counts were last taken.
  counts: PreimageCounts,
}

impl<K, V> Default for Preimages<K, V> {
  fn default() -> Self {
    Preimages {
      frozen: BTreeMap::new(),
      kept: BTreeMap::new(),
      counts: PreimageCounts::default(),
    }
  }
}

impl<K: Paged, V: Clone> Preimages<K, V> {
  /// Freezes the coordinates `lo..=hi`: a page of the cut.
  pub fn freeze(&mut self, lo: u64, hi: u64) {
    self.frozen.insert(lo, hi);
  }

  /// Whether `coordinate` lies in a frozen range.
  pub fn is_frozen(&self, coordinate: u64) -> bool {
    !self.frozen.is_empty()
      && self
        .frozen
        .range(..=coordinate)
        .next_back()
        .is_some_and(|(_, hi)| coordinate <= *hi)
  }

  /// Before the entry under `key` changes: where its coordinate is frozen and it has no pre-image yet, its value now
  /// (`now`, none where absent) kept as its pre-image, `bytes` sizing an old one for the counts.
  pub fn before(&mut self, key: K, now: impl FnOnce() -> Option<V>, bytes: impl Fn(&V) -> usize) {
    if !self.is_frozen(key.coordinate()) || self.kept.contains_key(&key) {
      return;
    }
    let value = now();
    match &value {
      Some(old) => {
        self.counts.old = self.counts.old.saturating_add(1);
        let len = u64::try_from(bytes(old)).unwrap_or(u64::MAX);
        self.counts.bytes = self.counts.bytes.saturating_add(len);
      }
      None => self.counts.absent = self.counts.absent.saturating_add(1),
    }
    self.kept.insert(key, value);
  }

  /// The entries whose coordinates lie in `lo..=hi` at the cut, in key order: `now` (the store's entries in that range,
  /// in key order) with every pre-image in it laid over them, an absent one taking its key out.
  pub fn overlay(&self, range: (u64, u64), now: impl Iterator<Item = (K, V)>) -> Vec<(K, V)> {
    self.overlay_within(range, now, (usize::MAX, &|_, _| 0)).0
  }

  /// [`Preimages::overlay`], read until the entries visited pass `cap` bytes (`size` each; a key the cut lacks and now
  /// holds is visited and passed over, so it counts too, else a page whose keys were inserted after the cut is walked
  /// whole; a key absent both at the cut and now counts its key's size): what a turn reads of a page at most (A-870 F2: a
  /// page a gap left can hold a family's every entry). Where the read stopped short of the range, the coordinate of the
  /// last key it visited: every entry before it is read, those at it perhaps not all.
  pub fn overlay_within(
    &self,
    (lo, hi): (u64, u64),
    now: impl Iterator<Item = (K, V)>,
    (cap, size): (usize, &dyn Fn(&K, &V) -> usize),
  ) -> (Vec<(K, V)>, Option<u64>) {
    let mut kept = self.kept.range(K::keys_in(lo, hi)).peekable();
    let mut now = now.peekable();
    let mut cut = Vec::new();
    let mut bytes = 0usize;
    loop {
      let (visited, cost) = match (now.peek(), kept.peek()) {
        (Some((at, _)), Some((pre, _))) if at < *pre => match now.next() {
          Some((key, value)) => {
            let cost = size(&key, &value);
            cut.push((key, value));
            (key, cost)
          }
          None => return (cut, None),
        },
        (Some((at, _)), Some((pre, _))) if at == *pre => {
          // Changed since the cut: what is held now is passed over, its pre-image taken next.
          match now.next() {
            Some((key, value)) => (key, size(&key, &value)),
            None => return (cut, None),
          }
        }
        (_, Some(_)) => match kept.next() {
          Some((key, Some(old))) => {
            let cost = size(key, old);
            cut.push((*key, old.clone()));
            (*key, cost)
          }
          Some((key, None)) => (*key, core::mem::size_of::<K>()),
          None => return (cut, None),
        },
        (Some(_), None) => match now.next() {
          Some((key, value)) => {
            let cost = size(&key, &value);
            cut.push((key, value));
            (key, cost)
          }
          None => return (cut, None),
        },
        (None, None) => return (cut, None),
      };
      bytes = bytes.saturating_add(cost);
      if bytes > cap && (now.peek().is_some() || kept.peek().is_some()) {
        return (cut, Some(visited.coordinate()));
      }
    }
  }

  /// Thaws `lo..=hi`, a page of the cut now encoded: its coordinates taken out of the frozen ranges (a page split while
  /// the cut is encoded thaws a half at a time, the other still frozen), and the pre-images in it dropped.
  pub fn thaw(&mut self, lo: u64, hi: u64) {
    let holding = self
      .frozen
      .range(..=lo)
      .next_back()
      .map(|(start, end)| (*start, *end))
      .filter(|(_, end)| lo <= *end);
    if let Some((start, end)) = holding {
      self.frozen.remove(&start);
      if start < lo {
        self.frozen.insert(start, lo.saturating_sub(1));
      }
      if hi < end {
        self.frozen.insert(hi.saturating_add(1), end);
      }
    }
    let keys: Vec<K> = self
      .kept
      .range(K::keys_in(lo, hi))
      .map(|(key, _)| *key)
      .collect();
    for key in keys {
      self.kept.remove(&key);
    }
  }

  /// The cut ended (its publication committed): every range thawed and every pre-image dropped. How many were left,
  /// none where every page was encoded and thawed as it should be.
  pub fn end(&mut self) -> usize {
    let left = self.frozen.len().saturating_add(self.kept.len());
    self.frozen.clear();
    self.kept.clear();
    left
  }

  /// The pre-images kept since the counts were last taken, the counts started again.
  pub fn take_counts(&mut self) -> PreimageCounts {
    core::mem::take(&mut self.counts)
  }

  /// The pre-images held now.
  pub fn held(&self) -> usize {
    self.kept.len()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Do: freeze a range; change an entry in it twice, insert a new one in it, remove one, and change one outside it;
  /// overlay the entries now. Expect: the overlay reads every entry in the range as at the freeze (the first value
  /// kept, the insert absent, the removed one back), the one outside as now; one old value counted a key changed in
  /// range (not twice), one absent marker; after a thaw nothing kept.
  #[test]
  fn a_frozen_range_reads_as_at_its_freeze_whatever_changes_after() {
    let mut now: BTreeMap<u64, u32> = [(10, 1), (12, 2), (14, 3), (30, 4)].into_iter().collect();
    let at_freeze = now.clone();
    let mut pre: Preimages<u64, u32> = Preimages::default();
    pre.freeze(8, 15);
    let mut change = |now: &mut BTreeMap<u64, u32>, key: u64, value: Option<u32>| {
      let was = now.get(&key).copied();
      pre.before(key, || was, |_| 4);
      match value {
        Some(value) => now.insert(key, value),
        None => now.remove(&key),
      };
    };
    change(&mut now, 10, Some(5));
    change(&mut now, 10, Some(6));
    change(&mut now, 11, Some(7));
    change(&mut now, 14, None);
    change(&mut now, 30, Some(8));
    let cut = pre.overlay((8, 15), now.range(8..=15).map(|(k, v)| (*k, *v)));
    let expected: Vec<(u64, u32)> = at_freeze.range(8..=15).map(|(k, v)| (*k, *v)).collect();
    assert_eq!(cut, expected);
    assert_eq!(now.get(&30), Some(&8), "outside the range, as now");
    let counts = pre.take_counts();
    assert_eq!((counts.old, counts.absent, counts.bytes), (2, 1, 8));
    let (capped, stopped) = pre.overlay_within(
      (8, 15),
      now.range(8..=15).map(|(k, v)| (*k, *v)),
      (1, &|_, _| 1),
    );
    // Key 10 as held now, changed since the cut: visited and passed over (counted), then its pre-image read; the cap
    // passed there, the read stops at 10.
    assert_eq!((capped.as_slice(), stopped), (&expected[..1], Some(10)));
    // A half thawed, the other still frozen: a change there still keeps its pre-image.
    pre.thaw(8, 11);
    assert!(!pre.is_frozen(10) && pre.is_frozen(12) && pre.is_frozen(15));
    let was = now.get(&12).copied();
    pre.before(12, || was, |_| 4);
    assert_eq!(pre.take_counts().old, 1);
    pre.thaw(12, 15);
    assert_eq!((pre.held(), pre.end()), (0, 0));
  }

  /// Shape: the keys inserted into a frozen range after its freeze in the capped read's test: far past its cap.
  const INSERTED_SINCE: u64 = 10_000;

  /// A-870 F2, the capped read's hole (main 01a20182, 2026-11-23 20:26: name it with its bound). Do: freeze a range
  /// holding two entries; insert [`INSERTED_SINCE`] keys into it after the freeze, each kept as an absent marker; read
  /// the range as at the freeze with a cap of a few entries. Expect: the read stops within the cap and an entry's worth
  /// of keys visited, at a coordinate short of the range's end, having passed over the inserted keys it visited; read
  /// again from that stop on, it goes on. Failing first: the read counted only the entries it kept, so it walked every
  /// inserted key (the whole tail of a frozen page a gap left, where every handle put after the cut lands) and answered
  /// no stop.
  #[test]
  fn a_capped_read_of_a_frozen_range_stops_within_its_cap_however_much_was_inserted_since() {
    const CAP: usize = 4;
    let mut now: BTreeMap<u64, u32> = [(0, 1), (1, 2)].into_iter().collect();
    let mut pre: Preimages<u64, u32> = Preimages::default();
    pre.freeze(0, u64::MAX);
    for key in 2..2 + INSERTED_SINCE {
      pre.before(key, || None, |_| 1);
      now.insert(key, 0);
    }
    let visited = std::cell::Cell::new(0usize);
    let (read, stopped) = pre.overlay_within(
      (0, u64::MAX),
      now.iter().map(|(key, value)| {
        visited.set(visited.get() + 1);
        (*key, *value)
      }),
      (CAP, &|_, _| 1),
    );
    assert_eq!(read, vec![(0, 1), (1, 2)], "the entries at the freeze");
    let stop = stopped.expect("the read stopped short of the range");
    assert!(
      visited.get() <= CAP + 2,
      "the read visited {} keys at a cap of {CAP}",
      visited.get()
    );
    assert!(stop < 2 + INSERTED_SINCE, "stopped at {stop}");
  }
}
