//! A-653 continued (3) (CLAUDE.md §3 "Bounded work everywhere"): git's delta searched a bounded amount a step
//! (`store::Search`), each step indexing at most its budget's blocks of the source or scanning at most its budget of the
//! target and one copy past it; and every search, at any budget, the delta the search before it wrote in one pass
//! ([`reference`]), byte for byte.

use proptest::prelude::*;

use crate::Admission;
use crate::delta::apply;
use crate::store::{Search, SearchCounters, SearchStep, delta_of};

use super::fragments::text;

/// Format: the bytes a block of the source holds, a copy instruction's most (`diff-delta.c`).
const BLOCK: u64 = 8;

/// Format: the longest copy git writes, 64 KiB (`diff-delta.c`): how far a match found at a step's last position may
/// carry its scan past the budget.
const COPY_MAX: u64 = 1 << 16;

/// Shape: a step's budget, small beside the texts searched, so a search takes many steps.
const BUDGET: usize = 64;

/// Shape: the lines of the text the bounded test searches, about 44 KB.
const LINES: u32 = 2000;

/// The search before A-653 continued (3), whole in one pass (A-614 part 2's `deltas.rs` as it was): the serial
/// specification every stepped search must equal.
mod reference {
  use std::collections::BTreeMap;

  use crate::delta::{Sizes, write_copy, write_insert, write_sizes};

  /// Format: the most bytes a copy instruction takes (`diff-delta.c`, `patch-delta.c`).
  const COPY_BYTES: usize = 8;

  /// Format: the longest copy git writes, 64 KiB (`diff-delta.c`).
  const COPY_MAX: usize = 1 << 16;

  /// Derived: the rolling hash's multiplier, the 64-bit golden ratio (Knuth's multiplicative hashing).
  const MULTIPLIER: u64 = 0x9e37_79b9_7f4a_7c15;

  fn hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0_u64, |hash, byte| {
      hash.wrapping_mul(MULTIPLIER).wrapping_add(u64::from(*byte))
    })
  }

  fn outgoing_weight() -> u64 {
    (1..COPY_BYTES).fold(1_u64, |weight, _| weight.wrapping_mul(MULTIPLIER))
  }

  fn index(source: &[u8]) -> BTreeMap<u64, usize> {
    let mut blocks = BTreeMap::new();
    for (at, block) in source.chunks_exact(COPY_BYTES).enumerate() {
      blocks
        .entry(hash(block))
        .or_insert(at.saturating_mul(COPY_BYTES));
    }
    blocks
  }

  fn agreeing(source: &[u8], from: usize, target: &[u8], at: usize) -> usize {
    let (Some(source), Some(target)) = (source.get(from..), target.get(at..)) else {
      return 0;
    };
    source
      .iter()
      .zip(target)
      .take(COPY_MAX)
      .take_while(|(a, b)| a == b)
      .count()
  }

  struct Scan<'a> {
    source: &'a [u8],
    target: &'a [u8],
    out: Vec<u8>,
    inserts_from: usize,
  }

  impl Scan<'_> {
    fn flush(&mut self, at: usize) {
      if let Some(bytes) = self.target.get(self.inserts_from..at) {
        write_insert(bytes, &mut self.out);
      }
    }

    fn copy(&mut self, at: usize, from: usize, len: usize) -> Option<()> {
      self.flush(at);
      let offset = u64::try_from(from).ok()?;
      write_copy(offset, u64::try_from(len).ok()?, &mut self.out).ok()?;
      self.inserts_from = at.checked_add(len)?;
      Some(())
    }

    fn matched(
      &self,
      blocks: &BTreeMap<u64, usize>,
      rolling: u64,
      at: usize,
    ) -> Option<(usize, usize)> {
      let from = *blocks.get(&rolling)?;
      let len = agreeing(self.source, from, self.target, at);
      (len > COPY_BYTES).then_some((from, len))
    }

    fn run(&mut self, blocks: &BTreeMap<u64, usize>) -> Option<()> {
      let weight = outgoing_weight();
      let mut at = 0_usize;
      let mut rolling = hash(self.target.get(..COPY_BYTES)?);
      while let Some(end) = at
        .checked_add(COPY_BYTES)
        .filter(|end| *end <= self.target.len())
      {
        if let Some((from, len)) = self.matched(blocks, rolling, at) {
          self.copy(at, from, len)?;
          at = at.checked_add(len)?;
          rolling = self
            .target
            .get(at..at.saturating_add(COPY_BYTES))
            .map_or(0, hash);
          continue;
        }
        if let (Some(leaving), Some(entering)) = (self.target.get(at), self.target.get(end)) {
          let without = rolling.wrapping_sub(u64::from(*leaving).wrapping_mul(weight));
          rolling = without
            .wrapping_mul(MULTIPLIER)
            .wrapping_add(u64::from(*entering));
        }
        at = at.checked_add(1)?;
      }
      Some(())
    }
  }

  /// The delta turning `source` into `target` in one pass, or `None` when it would not be shorter than `target`.
  pub(super) fn delta_of(source: &[u8], target: &[u8]) -> Option<Vec<u8>> {
    let blocks = index(source);
    if blocks.is_empty() || target.len() <= COPY_BYTES {
      return None;
    }
    let mut scan = Scan {
      source,
      target,
      out: Vec::new(),
      inserts_from: 0,
    };
    let sizes = Sizes {
      source: u64::try_from(source.len()).ok()?,
      target: u64::try_from(target.len()).ok()?,
    };
    write_sizes(sizes, &mut scan.out);
    scan.run(&blocks)?;
    scan.flush(target.len());
    (scan.out.len() < target.len()).then_some(scan.out)
  }
}

/// `source` and `target`'s delta searched `budget` bytes a step: the delta, and the counters after each step, the
/// first before any.
fn stepped(source: &[u8], target: &[u8], budget: usize) -> (Option<Vec<u8>>, Vec<SearchCounters>) {
  let mut search = Search::new();
  let mut seen = vec![search.counters()];
  loop {
    let step = search.step(source, target, budget);
    seen.push(search.counters());
    if let SearchStep::Done(delta) = step {
      return (delta, seen);
    }
  }
}

/// The first step past its budget, as the counters before and after it: one that indexed more than its budget's whole
/// blocks of the source (one block at least), or scanned more than its budget of the target, one copy past it and the
/// target's last bytes short of a block.
fn first_unbounded_step(
  seen: &[SearchCounters],
  budget: usize,
) -> Option<(SearchCounters, SearchCounters)> {
  let budget = u64::try_from(budget).unwrap_or(u64::MAX);
  let blocks = budget
    .checked_div(BLOCK)
    .unwrap_or(0)
    .max(1)
    .saturating_mul(BLOCK);
  let scan_most = budget
    .max(1)
    .saturating_add(COPY_MAX)
    .saturating_add(BLOCK);
  seen.windows(2).find_map(|pair| match pair {
    [before, after]
      if after.indexed.saturating_sub(before.indexed) > blocks
        || after.scanned.saturating_sub(before.scanned) > scan_most =>
    {
      Some((*before, *after))
    }
    _ => None,
  })
}

/// A-653 continued (3) (CLAUDE.md §3 "Bounded work everywhere": git's delta search sliced). Do: search the delta of a
/// 2,000-line text against the same text with three lines edited, 64 bytes a step. Expect: no step indexes more than
/// its budget's blocks or scans more than its budget and one copy past it; at least a step for each budget of the
/// source; and the delta the one-pass reference's, byte for byte, applying back to the target, and `delta_of`'s.
#[test]
fn a_delta_is_searched_a_budget_a_step_as_one_pass_searches_it() {
  let source = text(LINES, &[]);
  let target = text(LINES, &[100, 900, 1500]);
  let (delta, seen) = stepped(&source, &target, BUDGET);
  assert_eq!(first_unbounded_step(&seen, BUDGET), None);
  let steps = seen.last().map_or(0, |last| last.steps);
  let least = u64::try_from(source.len().checked_div(BUDGET).unwrap()).unwrap();
  assert!(steps >= least, "{steps} steps for {least} budgets of the source");
  assert_eq!(delta, reference::delta_of(&source, &target));
  let delta = delta.unwrap();
  assert_eq!(
    apply(&source, &delta, &mut Admission::new(u64::MAX)).unwrap(),
    target
  );
  assert_eq!(delta_of(&source, &target), Some(delta));
}

/// The target A-614 part 2's delta property builds: from each cut, the source's bytes it names, then its inserted bytes.
fn cut_target(source: &[u8], cuts: &[(usize, usize, Vec<u8>)]) -> Vec<u8> {
  let mut target = Vec::new();
  for (from, len, inserted) in cuts {
    if let Some(from) = from.checked_rem(source.len()) {
      let end = from.saturating_add(*len).min(source.len());
      target.extend_from_slice(source.get(from..end).unwrap_or_default());
    }
    target.extend_from_slice(inserted);
  }
  target
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig { cases: slates_test_seeds::cases(128), ..ProptestConfig::default() }))]

  /// A-653 continued (3). Do: build targets from random cuts of a random source and random inserts, as A-614 part 2's
  /// delta property does, and search each target's delta at a random budget, or one budget for the whole. Expect:
  /// every step bounded, and the delta the one-pass reference's, byte for byte.
  #[test]
  fn every_search_at_any_budget_writes_the_reference_delta(source in proptest::collection::vec(any::<u8>(), 0..600), cuts in proptest::collection::vec((any::<usize>(), 0usize..80, proptest::collection::vec(any::<u8>(), 0..6)), 0..8), small in 1usize..200, whole in any::<bool>()) {
    let target = cut_target(&source, &cuts);
    let budget = if whole { usize::MAX } else { small };
    let (delta, seen) = stepped(&source, &target, budget);
    prop_assert_eq!(first_unbounded_step(&seen, budget), None);
    prop_assert_eq!(delta, reference::delta_of(&source, &target));
  }
}
