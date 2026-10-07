//! One bit per page or granule of a region: the record a region keeps of its freed pages still resident (A-105), which
//! the idle purge gives back, and of its pages that may carry the OS's reusable mark (A-110, macOS), which an
//! allocation clears before it hands them out.
//!
//! A bit set, allocated once at the region's size, because the arena's allocation and free mark and take ranges on their
//! hot path, which must not reach the system allocator (`crates/mem/tests/no_alloc.rs`: an ordered map of ranges made
//! 233 allocations in its loop); a bit set touches only its words. Its size is the region's: a 2.7 GiB region of 16
//! KiB pages is 21 KiB of bits. Adjacent recorded units walk as one run, so the purge gives back coalesced ranges.

/// Bits per word of a [`UnitSet`].
const WORD_BITS: usize = u64::BITS as usize;

/// One bit per `unit` bytes of a region (see the module doc): set where the unit is recorded. Allocated once, at the
/// region's size; marking, taking and walking touch only words, so the arena's hot path allocates nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UnitSet {
  words: Vec<u64>,
  unit: usize,
  units: usize,
  recorded: usize,
}

impl UnitSet {
  /// An empty set over `len` bytes in units of `unit` bytes.
  pub(crate) fn new(len: usize, unit: usize) -> UnitSet {
    let unit = unit.max(1);
    let units = len.div_ceil(unit);
    UnitSet {
      words: vec![0; units.div_ceil(WORD_BITS)],
      unit,
      units,
      recorded: 0,
    }
  }

  /// Every unit of `len` bytes recorded.
  pub(crate) fn whole(len: usize, unit: usize) -> UnitSet {
    let mut set = UnitSet::new(len, unit);
    set.mark(0, len);
    set
  }

  /// Whether no unit is recorded: the whole cost of a check against the set then.
  pub(crate) fn is_empty(&self) -> bool {
    self.recorded == 0
  }

  /// The bytes the recorded units cover.
  #[cfg(test)]
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
    let mut changed = 0usize;
    let mut at = first;
    while at < last {
      let word = at.checked_div(WORD_BITS).unwrap_or(0);
      let bit = at.checked_rem(WORD_BITS).unwrap_or(0);
      let span = (WORD_BITS.saturating_sub(bit)).min(last.saturating_sub(at));
      let mask = span_mask(bit, span);
      if let Some(slot) = self.words.get_mut(word) {
        let before = *slot;
        *slot = if on { before | mask } else { before & !mask };
        let flipped = usize::try_from((before ^ *slot).count_ones()).unwrap_or(0);
        changed = changed.saturating_add(flipped);
      }
      at = at.saturating_add(span.max(1));
    }
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

  /// The first unit in `from .. to` whose bit is `on`, a word at a time.
  fn find(&self, from: usize, on: bool, to: usize) -> Option<usize> {
    let to = to.min(self.units);
    let mut at = from;
    while at < to {
      let word = at.checked_div(WORD_BITS).unwrap_or(0);
      let bit = at.checked_rem(WORD_BITS).unwrap_or(0);
      let bits = self.words.get(word).copied().unwrap_or(0);
      let candidates = if on { bits } else { !bits } >> bit;
      if candidates != 0 {
        let found = at.saturating_add(usize::try_from(candidates.trailing_zeros()).unwrap_or(0));
        return (found < to).then_some(found);
      }
      at = word.saturating_add(1).saturating_mul(WORD_BITS);
    }
    None
  }
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

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::with_cases(512)))]

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
}
