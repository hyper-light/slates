//! One bit per page or granule of a region: the record a region keeps of its freed pages still resident (A-105), which
//! the idle purge gives back, and of its pages that may carry the OS's reusable mark (A-110, macOS), which an
//! allocation clears before it hands them out.
//!
//! A bit set, allocated once at the region's size, because the arena's allocation and free mark and take ranges on their
//! hot path, which must not reach the system allocator (`crates/mem/tests/no_alloc.rs`: an ordered map of ranges made
//! 233 allocations in its loop); a bit set touches only its words. Its size is the region's: a 2.7 GiB region of 16
//! KiB pages is 21 KiB of bits. Adjacent recorded units walk as one run, so the purge gives back coalesced ranges.

use std::collections::BTreeMap;

/// Bits per word of a [`UnitSet`].
const WORD_BITS: usize = u64::BITS as usize;

/// Shape: words per sparse bitmap block (64 × 8 bytes = 512 bytes, covering 4,096 units = 64 MiB at
/// 16 KiB pages), so a whole-object reusable-mark set over a 146 GiB sparse mapping allocates zero
/// bytes at open and only 512 bytes per 64 MiB block actually written.
const SPARSE_BLOCK_WORDS: usize = 64;

/// Backing storage of a [`UnitSet`]: flat dense words for bounded arena regions (zero allocation on
/// the arena's hot path, `crates/mem/tests/no_alloc.rs`), or a sparse block map defaulting to all
/// units recorded for multi-gigabyte sparse objects (`UnitSet::whole`).
#[derive(Clone, Debug)]
enum Storage {
  Dense(Vec<u64>),
  Sparse(BTreeMap<usize, Box<[u64; SPARSE_BLOCK_WORDS]>>),
}

/// All valid bits set in word `word` of a set with `units` total units.
fn full_word(word: usize, units: usize) -> u64 {
  let start = word.saturating_mul(WORD_BITS);
  if start >= units {
    return 0;
  }
  let valid = units.saturating_sub(start).min(WORD_BITS);
  span_mask(0, valid)
}

/// One bit per `unit` bytes of a region (see the module doc): set where the unit is recorded.
/// `UnitSet::new` allocates dense words once at the region's size so marking, taking and walking
/// touch only words and the arena's hot path allocates nothing; `UnitSet::whole` uses sparse blocks
/// defaulting to all-set so opening a multi-gigabyte sparse object costs zero heap bytes until
/// specific ranges are cleared.
#[derive(Clone, Debug)]
pub(crate) struct UnitSet {
  storage: Storage,
  unit: usize,
  units: usize,
  recorded: usize,
}

impl PartialEq for UnitSet {
  fn eq(&self, other: &Self) -> bool {
    if self.unit != other.unit || self.units != other.units || self.recorded != other.recorded {
      return false;
    }
    let words = self.units.div_ceil(WORD_BITS);
    match (&self.storage, &other.storage) {
      (Storage::Dense(a), Storage::Dense(b)) => a == b,
      (Storage::Sparse(a), Storage::Sparse(b)) => a == b,
      _ => (0..words).all(|word| self.word_at(word) == other.word_at(word)),
    }
  }
}

impl Eq for UnitSet {}

impl UnitSet {
  /// An empty set over `len` bytes in units of `unit` bytes.
  pub(crate) fn new(len: usize, unit: usize) -> UnitSet {
    let unit = unit.max(1);
    let units = len.div_ceil(unit);
    UnitSet {
      storage: Storage::Dense(vec![0; units.div_ceil(WORD_BITS)]),
      unit,
      units,
      recorded: 0,
    }
  }

  /// Every unit of `len` bytes recorded, without allocating a dense bitmap over multi-gigabyte sparse
  /// objects: an absent sparse block represents all valid bits set, so opening a 146 GiB macOS
  /// content object costs zero heap bytes instead of 1.09 MiB per open (A-110).
  pub(crate) fn whole(len: usize, unit: usize) -> UnitSet {
    let unit = unit.max(1);
    let units = len.div_ceil(unit);
    UnitSet {
      storage: Storage::Sparse(BTreeMap::new()),
      unit,
      units,
      recorded: units,
    }
  }

  /// Word `word` of the bit set (0 past the set's length).
  fn word_at(&self, word: usize) -> u64 {
    match &self.storage {
      Storage::Dense(words) => words.get(word).copied().unwrap_or(0),
      Storage::Sparse(blocks) => {
        let block_idx = word.checked_div(SPARSE_BLOCK_WORDS).unwrap_or(0);
        let offset = word.checked_rem(SPARSE_BLOCK_WORDS).unwrap_or(0);
        match blocks.get(&block_idx) {
          Some(block) => block.get(offset).copied().unwrap_or(0),
          None => full_word(word, self.units),
        }
      }
    }
  }

  /// Whether no unit is recorded: the whole cost of a check against the set then.
  pub(crate) fn is_empty(&self) -> bool {
    self.recorded == 0
  }

  /// The bytes the recorded units cover: what an idle purge's walk leaves resident and free when it ends
  /// (`ChunkArena::purge`).
  pub(crate) fn recorded_bytes(&self) -> usize {
    self.recorded.saturating_mul(self.unit)
  }

  /// The bytes one word of the set covers: the span its owner clears a mark over at once, so a region's first
  /// allocations pay one call per word of pages rather than one per block (A-110).
  pub(crate) fn word_bytes(&self) -> usize {
    self.unit.saturating_mul(WORD_BITS)
  }

  /// The units `start .. end` covers, whole: from the unit `start` is in to the one `end - 1` is in.
  fn units_of(&self, start: usize, end: usize) -> (usize, usize) {
    if start >= end {
      return (0, 0);
    }
    let first = start.checked_div(self.unit).unwrap_or(0);
    let last = end.div_ceil(self.unit).min(self.units);
    (first.min(last), last)
  }

  /// Records every unit `start .. end` touches.
  pub(crate) fn mark(&mut self, start: usize, end: usize) {
    let (first, last) = self.units_of(start, end);
    self.set_units(first, last, true);
  }

  /// Takes every unit `start .. end` touches out of the set: the bytes of the units that were recorded.
  pub(crate) fn take(&mut self, start: usize, end: usize) -> usize {
    let (first, last) = self.units_of(start, end);
    self.set_units(first, last, false).saturating_mul(self.unit)
  }

  /// Sets or clears units `first .. last` a word at a time: the units that changed.
  fn set_units(&mut self, first: usize, last: usize, on: bool) -> usize {
    let units = self.units;
    let changed = match &mut self.storage {
      Storage::Dense(words) => set_units_dense(words, first, last, on),
      Storage::Sparse(blocks) => set_units_sparse(blocks, units, first, last, on),
    };
    self.recorded = if on {
      self.recorded.saturating_add(changed)
    } else {
      self.recorded.saturating_sub(changed)
    };
    changed
  }

  /// The first run of recorded units at or after byte `at`, as bytes `(start, end)`, the end clamped to `len`: where a
  /// walk resumes. A run is maximal, so adjacent recorded units come back as one range.
  pub(crate) fn next_run_from(&self, at: usize, len: usize) -> Option<(usize, usize)> {
    self.next_run_within(at, len)
  }

  /// [`UnitSet::next_run_from`] with the run cut at byte `until` (rounded up to a unit): the search for its end stops
  /// there. A caller that needs only the part of a run inside its span asks this; walking a fresh region's one run to
  /// its end cost every allocation a scan of the whole bit set (measured: 8% of a writing daemon's samples on macOS).
  pub(crate) fn next_run_within(&self, at: usize, until: usize) -> Option<(usize, usize)> {
    if self.recorded == 0 {
      return None;
    }
    let limit = until.div_ceil(self.unit).min(self.units);
    let start = self.find(at.checked_div(self.unit).unwrap_or(0), true, limit)?;
    let end = self.find(start, false, limit).unwrap_or(limit);
    Some((
      start.saturating_mul(self.unit),
      end.saturating_mul(self.unit).min(until),
    ))
  }

  /// The first unit in `from .. to` whose bit is `on`, a word at a time (skipping absent all-ones
  /// sparse blocks in O(log B)).
  fn find(&self, from: usize, on: bool, to: usize) -> Option<usize> {
    let to = to.min(self.units);
    let mut at = from;
    match &self.storage {
      Storage::Dense(words) => {
        while at < to {
          let word = at.checked_div(WORD_BITS).unwrap_or(0);
          let bit = at.checked_rem(WORD_BITS).unwrap_or(0);
          let bits = words.get(word).copied().unwrap_or(0);
          let candidates = if on { bits } else { !bits } >> bit;
          if candidates != 0 {
            let found =
              at.saturating_add(usize::try_from(candidates.trailing_zeros()).unwrap_or(0));
            return (found < to).then_some(found);
          }
          at = word.saturating_add(1).saturating_mul(WORD_BITS);
        }
        None
      }
      Storage::Sparse(blocks) => {
        let block_units = SPARSE_BLOCK_WORDS.saturating_mul(WORD_BITS);
        while at < to {
          let word = at.checked_div(WORD_BITS).unwrap_or(0);
          let block_idx = word.checked_div(SPARSE_BLOCK_WORDS).unwrap_or(0);
          let Some(block) = blocks.get(&block_idx) else {
            if on {
              return Some(at);
            }
            // Absent block is all-ones: jump directly to the next materialized block holding any 0 bit.
            let (&next_block, _) = blocks.range(block_idx.saturating_add(1)..).next()?;
            at = next_block.saturating_mul(block_units);
            continue;
          };
          let block_end = block_idx
            .saturating_add(1)
            .saturating_mul(block_units)
            .min(to);
          while at < block_end {
            let cur_word = at.checked_div(WORD_BITS).unwrap_or(0);
            let offset = cur_word.checked_rem(SPARSE_BLOCK_WORDS).unwrap_or(0);
            let bit = at.checked_rem(WORD_BITS).unwrap_or(0);
            let bits = block.get(offset).copied().unwrap_or(0);
            let candidates = if on { bits } else { !bits } >> bit;
            if candidates != 0 {
              let found =
                at.saturating_add(usize::try_from(candidates.trailing_zeros()).unwrap_or(0));
              return (found < block_end).then_some(found);
            }
            at = cur_word.saturating_add(1).saturating_mul(WORD_BITS);
          }
        }
        None
      }
    }
  }
}

/// Sets or clears units `first .. last` in a dense word table: the units that changed.
fn set_units_dense(words: &mut [u64], first: usize, last: usize, on: bool) -> usize {
  let mut changed = 0usize;
  let mut at = first;
  while at < last {
    let word = at.checked_div(WORD_BITS).unwrap_or(0);
    let bit = at.checked_rem(WORD_BITS).unwrap_or(0);
    let span = (WORD_BITS.saturating_sub(bit)).min(last.saturating_sub(at));
    let mask = span_mask(bit, span);
    if let Some(slot) = words.get_mut(word) {
      let before = *slot;
      *slot = if on { before | mask } else { before & !mask };
      let flipped = usize::try_from((before ^ *slot).count_ones()).unwrap_or(0);
      changed = changed.saturating_add(flipped);
    }
    at = at.saturating_add(span.max(1));
  }
  changed
}

/// Sets or clears units `first .. last` in a sparse all-ones-default block map: the units that changed.
fn set_units_sparse(
  blocks: &mut BTreeMap<usize, Box<[u64; SPARSE_BLOCK_WORDS]>>,
  units: usize,
  first: usize,
  last: usize,
  on: bool,
) -> usize {
  let block_units = SPARSE_BLOCK_WORDS.saturating_mul(WORD_BITS);
  let mut changed = 0usize;
  let mut at = first;
  while at < last {
    let word = at.checked_div(WORD_BITS).unwrap_or(0);
    let block_idx = word.checked_div(SPARSE_BLOCK_WORDS).unwrap_or(0);
    let block_end = block_idx
      .saturating_add(1)
      .saturating_mul(block_units)
      .min(last);
    if on && !blocks.contains_key(&block_idx) {
      // Every valid bit in an absent block is already set.
      at = block_end;
      continue;
    }
    let block_word_base = block_idx.saturating_mul(SPARSE_BLOCK_WORDS);
    let block = blocks.entry(block_idx).or_insert_with(|| {
      let mut fresh = Box::new([0u64; SPARSE_BLOCK_WORDS]);
      for (offset, slot) in fresh.iter_mut().enumerate() {
        *slot = full_word(block_word_base.saturating_add(offset), units);
      }
      fresh
    });
    while at < block_end {
      let cur_word = at.checked_div(WORD_BITS).unwrap_or(0);
      let offset = cur_word.checked_rem(SPARSE_BLOCK_WORDS).unwrap_or(0);
      let bit = at.checked_rem(WORD_BITS).unwrap_or(0);
      let span = (WORD_BITS.saturating_sub(bit)).min(block_end.saturating_sub(at));
      let mask = span_mask(bit, span);
      if let Some(slot) = block.get_mut(offset) {
        let before = *slot;
        *slot = if on { before | mask } else { before & !mask };
        let flipped = usize::try_from((before ^ *slot).count_ones()).unwrap_or(0);
        changed = changed.saturating_add(flipped);
      }
      at = at.saturating_add(span.max(1));
    }
    if on
      && block
        .iter()
        .enumerate()
        .all(|(offset, &word)| word == full_word(block_word_base.saturating_add(offset), units))
    {
      blocks.remove(&block_idx);
    }
  }
  changed
}

/// A mask of `span` bits from bit `bit` of a word (`bit + span <= 64`).
fn span_mask(bit: usize, span: usize) -> u64 {
  let ones = if span >= WORD_BITS {
    u64::MAX
  } else {
    u32::try_from(span)
      .ok()
      .and_then(|span| 1u64.checked_shl(span))
      .map_or(u64::MAX, |top| top.wrapping_sub(1))
  };
  u32::try_from(bit)
    .ok()
    .and_then(|bit| ones.checked_shl(bit))
    .unwrap_or(0)
}

#[cfg(test)]
mod tests {
  // Test harness code: proptest's strategy types carry `Arc` (D-8's harness exception), and the byte map is indexed by
  // generated positions inside its own length.
  #![allow(clippy::indexing_slicing, clippy::disallowed_types)]
  use super::*;
  use proptest::prelude::*;

  #[derive(Clone, Debug)]
  enum Step {
    Mark(usize, usize),
    Take(usize, usize),
  }

  /// 84a (boot). Do: make the whole set over every length from none to five words of units and a few more, at units of
  /// 1, 3, 64 and 16,384 bytes, at once and by marking it from its first byte to its last. Expect the two equal, words,
  /// units and the recorded count alike.
  #[test]
  fn a_whole_set_made_at_once_equals_one_marked_over_its_length() {
    for unit in [1usize, 3, 64, 16_384] {
      let longest = unit
        .saturating_mul(WORD_BITS)
        .saturating_mul(5)
        .saturating_add(unit);
      let step = (longest / 97).max(1);
      let lens = (0..=longest)
        .step_by(step)
        .chain([1, unit, unit + 1, longest]);
      for len in lens {
        let mut marked = UnitSet::new(len, unit);
        marked.mark(0, len);
        assert_eq!(UnitSet::whole(len, unit), marked, "len {len}, unit {unit}");
      }
    }
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::with_cases(slates_test_seeds::cases(512))))]

    /// A-105 and A-110's oracle for the bit set. Do: apply generated marks and takes, in units of 3 bytes over 400 bytes
    /// (more than six words), to the set and to a unit map, and walk the runs after each step. Expect: each take returns
    /// the bytes of the units it cleared; the recorded bytes equal the map's; and the walk returns exactly the map's
    /// maximal runs, in order.
    #[test]
    #[cfg_attr(miri, ignore)] // generated histories under Miri take minutes; the plain test below runs there
    fn the_bit_set_equals_a_unit_map_on_every_history(steps in prop::collection::vec(unit_step(), 1..60)) {
      const UNIT: usize = 3;
      const LEN: usize = 400;
      let units = LEN.div_ceil(UNIT);
      let mut set = UnitSet::new(LEN, UNIT);
      let mut model = vec![false; units];
      for step in steps {
        match step {
          Step::Mark(start, end) => {
            set.mark(start, end);
            if start < end {
              model[start / UNIT..end.div_ceil(UNIT)].iter_mut().for_each(|unit| *unit = true);
            }
          }
          Step::Take(start, end) => {
            let taken = set.take(start, end);
            let mut cleared = 0;
            if start < end {
              for unit in &mut model[start / UNIT..end.div_ceil(UNIT)] {
                cleared += usize::from(*unit);
                *unit = false;
              }
            }
            prop_assert_eq!(taken, cleared * UNIT);
          }
        }
        prop_assert_eq!(set.recorded_bytes(), model.iter().filter(|unit| **unit).count() * UNIT);
        let mut runs = Vec::new();
        let mut at = 0;
        while let Some((start, end)) = set.next_run_from(at, LEN) {
          runs.push((start, end));
          at = end;
        }
        let mut expected = Vec::new();
        let mut unit = 0;
        while unit < units {
          if model[unit] {
            let start = unit;
            while unit < units && model[unit] {
              unit += 1;
            }
            expected.push((start * UNIT, (unit * UNIT).min(LEN)));
          } else {
            unit += 1;
          }
        }
        prop_assert_eq!(runs, expected);
      }
    }
  }

  fn unit_step() -> impl Strategy<Value = Step> {
    const LEN: usize = 400;
    prop_oneof![
      (0..LEN, 0..LEN).prop_map(|(a, b)| Step::Mark(a.min(b), a.max(b))),
      (0..LEN, 0..LEN).prop_map(|(a, b)| Step::Take(a.min(b), a.max(b))),
    ]
  }

  /// A-110. Do: a whole bit set over 130 bytes of 2-byte units (three words, the last partial); take a range that runs
  /// past the end, then walk. Expect: the take returns the bytes of the units it cleared (five, the last ending at the
  /// length), the walk returns what is left as one run, and a later mark of the last units walks again.
  #[test]
  fn a_whole_bit_set_splits_across_words() {
    let mut set = UnitSet::whole(130, 2);
    assert_eq!(set.recorded_bytes(), 130);
    assert_eq!(set.take(120, 132), 10);
    assert_eq!(set.next_run_from(0, 130), Some((0, 120)));
    assert_eq!(set.next_run_from(120, 130), None);
    set.mark(126, 130);
    assert_eq!(set.next_run_from(120, 130), Some((126, 130)));
  }

  /// A-110 sparse whole set oracle: spans multiple 64-word (4,096-unit) sparse blocks, clearing and
  /// re-marking ranges across block boundaries and verifying exact equivalence with a dense unit set.
  #[test]
  fn a_sparse_whole_set_matches_a_dense_set_across_blocks() {
    const UNIT: usize = 16;
    const LEN: usize = UNIT * WORD_BITS * SPARSE_BLOCK_WORDS * 3 + UNIT * 19;
    let mut sparse = UnitSet::whole(LEN, UNIT);
    let mut dense = UnitSet::new(LEN, UNIT);
    dense.mark(0, LEN);
    assert_eq!(sparse, dense);
    let block_bytes = UNIT * WORD_BITS * SPARSE_BLOCK_WORDS;
    let ops = [
      (false, block_bytes - UNIT * 5, block_bytes + UNIT * 7),
      (false, 0, UNIT * 3),
      (false, LEN - UNIT * 4, LEN + UNIT * 2),
      (true, block_bytes - UNIT * 5, block_bytes + UNIT * 7),
      (
        false,
        block_bytes * 2 + UNIT * 10,
        block_bytes * 2 + UNIT * 90,
      ),
    ];
    for (mark, start, end) in ops {
      if mark {
        sparse.mark(start, end);
        dense.mark(start, end);
      } else {
        assert_eq!(sparse.take(start, end), dense.take(start, end));
      }
      assert_eq!(sparse.recorded_bytes(), dense.recorded_bytes());
      assert_eq!(sparse, dense);
      let mut at = 0;
      let mut got = Vec::new();
      while let Some((s, e)) = sparse.next_run_from(at, LEN) {
        got.push((s, e));
        at = e;
      }
      let mut at = 0;
      let mut want = Vec::new();
      while let Some((s, e)) = dense.next_run_from(at, LEN) {
        want.push((s, e));
        at = e;
      }
      assert_eq!(got, want);
    }
  }
}
