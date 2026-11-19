//! git's delta of one object against another (A-614 part 2; `gitformat-pack`, "Deltified
//! representation"; git's `diff-delta.c`): the source indexed in blocks, the target scanned with
//! a rolling hash, each block that matches extended as far as the bytes agree and written as a
//! copy, the rest as inserts, through A-582's writers ([`crate::delta`]).
//!
//! **No tuning number.** A block is as long as a copy instruction can be ([`COPY_BYTES`]): a match
//! no longer saves nothing as a copy, so no shorter block is worth indexing and no longer one is
//! needed to find a longer match, which extends past it. A copy is at most what git writes
//! ([`COPY_MAX`]). A delta not shorter than its target is no delta ([`delta_of`] answers `None`).

use std::collections::BTreeMap;

use crate::delta::{Sizes, write_copy, write_insert, write_sizes};

/// Format: the most bytes a copy instruction takes: its opcode and seven operand bytes, four of
/// offset and three of length (`diff-delta.c`, `patch-delta.c`).
const COPY_BYTES: usize = 8;

/// Format: the longest copy git writes, 64 KiB (`diff-delta.c`: "limited to 64KB (pack v2)").
const COPY_MAX: usize = 1 << 16;

/// Derived: the rolling hash's multiplier, the 64-bit golden ratio (Knuth's multiplicative
/// hashing): odd, so multiplying by it loses no bit.
const MULTIPLIER: u64 = 0x9e37_79b9_7f4a_7c15;

/// The hash of `bytes`, a block.
fn hash(bytes: &[u8]) -> u64 {
  bytes.iter().fold(0_u64, |hash, byte| {
    hash.wrapping_mul(MULTIPLIER).wrapping_add(u64::from(*byte))
  })
}

/// The multiplier to the power of a block's length less one: what the byte leaving a block weighs.
fn outgoing_weight() -> u64 {
  (1..COPY_BYTES).fold(1_u64, |weight, _| weight.wrapping_mul(MULTIPLIER))
}

/// The source's blocks by hash: the first offset of each, at multiples of a block's length.
fn index(source: &[u8]) -> BTreeMap<u64, usize> {
  let mut blocks = BTreeMap::new();
  for (at, block) in source.chunks_exact(COPY_BYTES).enumerate() {
    blocks
      .entry(hash(block))
      .or_insert(at.saturating_mul(COPY_BYTES));
  }
  blocks
}

/// How many bytes `source` from `from` and `target` from `at` agree on, at most a copy's length.
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

/// A delta being written.
struct Scan<'a> {
  source: &'a [u8],
  target: &'a [u8],
  out: Vec<u8>,
  /// Where the target's bytes waiting to be inserted begin.
  inserts_from: usize,
}

impl Scan<'_> {
  /// Writes the waiting bytes before `at` as inserts.
  fn flush(&mut self, at: usize) {
    if let Some(bytes) = self.target.get(self.inserts_from..at) {
      write_insert(bytes, &mut self.out);
    }
  }

  /// Writes a copy of `len` bytes from `from`, after the waiting inserts; `None` if the format
  /// cannot say it.
  fn copy(&mut self, at: usize, from: usize, len: usize) -> Option<()> {
    self.flush(at);
    let offset = u64::try_from(from).ok()?;
    write_copy(offset, u64::try_from(len).ok()?, &mut self.out).ok()?;
    self.inserts_from = at.checked_add(len)?;
    Some(())
  }

  /// The length of a match worth a copy at `at`, found through the source's blocks.
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
}

/// git's delta turning `source` into `target`, or `None` when it would not be shorter than
/// `target` itself (module docs).
pub fn delta_of(source: &[u8], target: &[u8]) -> Option<Vec<u8>> {
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

impl Scan<'_> {
  /// Scans the target: a copy where a block matches, the rolling hash moved a byte where none does.
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

/// What a step of a search left.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchStep {
  /// More of the search to do.
  More,
  /// The search is done: the delta, or `None` where [`delta_of`] answers `None`.
  Done(Option<Vec<u8>>),
}

/// What a search did, for a test to see each step bounded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchCounters {
  /// Steps taken.
  pub steps: u64,
  /// The source's bytes indexed.
  pub indexed: u64,
  /// The target's bytes scanned.
  pub scanned: u64,
}

/// git's delta of a source and a target searched a step at a time (A-653 continued (3)). This first state is the
/// failing test's: the whole search in one step.
#[derive(Default)]
pub struct Search {
  done: bool,
  counters: SearchCounters,
}

impl Search {
  /// A search with nothing done.
  pub fn new() -> Search {
    Search::default()
  }

  /// What the search did.
  pub fn counters(&self) -> SearchCounters {
    self.counters
  }

  /// One step: today's whole search.
  pub fn step(&mut self, source: &[u8], target: &[u8], _budget: usize) -> SearchStep {
    self.counters.steps = self.counters.steps.saturating_add(1);
    if self.done {
      return SearchStep::Done(None);
    }
    self.done = true;
    self.counters.indexed = u64::try_from(source.len()).unwrap_or(u64::MAX);
    self.counters.scanned = u64::try_from(target.len()).unwrap_or(u64::MAX);
    SearchStep::Done(delta_of(source, target))
  }
}
