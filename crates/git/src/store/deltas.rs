//! git's delta of one object against another (A-614 part 2; `gitformat-pack`, "Deltified
//! representation"; git's `diff-delta.c`): the source indexed in blocks, the target scanned with
//! a rolling hash, each block that matches extended as far as the bytes agree and written as a
//! copy, the rest as inserts, through A-582's writers ([`crate::delta`]).
//!
//! **No tuning number.** A block is as long as a copy instruction can be ([`COPY_BYTES`]): a match
//! no longer saves nothing as a copy, so no shorter block is worth indexing and no longer one is
//! needed to find a longer match, which extends past it. A copy is at most what git writes
//! ([`COPY_MAX`]). A delta not shorter than its target is no delta ([`delta_of`] answers `None`).
//!
//! **A bounded amount a step (A-653 continued (3); CLAUDE.md §3 "Bounded work everywhere").** A
//! search ([`Search`]) indexes the source, then scans the target, a step at a time: a step indexes
//! at most its budget's whole blocks of the source, one at least, or scans the target until it has
//! passed its budget, one byte at least, where a match found at its last position may carry it one
//! copy further, and the target's last bytes, short of a block, go with the step that reaches them.
//! Between steps it holds the source's index, the delta written so far and where the scan stands,
//! and the caller hands every step the same source and target. Each step runs the one pass's own
//! loop from where the last stopped, so a search over any budgets writes the delta one pass writes,
//! byte for byte: [`delta_of`] is one search run to its end.

use std::collections::BTreeMap;

use crate::delta::{Sizes, write_copy, write_insert, write_sizes};
use crate::to_u64;

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

/// The bytes of `source`'s whole blocks: where its index ends.
fn whole_blocks(source: &[u8]) -> usize {
  source
    .len()
    .checked_div(COPY_BYTES)
    .unwrap_or(0)
    .saturating_mul(COPY_BYTES)
}

/// Indexes `source`'s blocks from byte `from`, a block's start, on: at most `budget`'s whole
/// blocks, one at least, each hash keeping its first offset, as one pass over every block keeps
/// it. Where the next step goes on from.
fn index_from(
  blocks: &mut BTreeMap<u64, usize>,
  source: &[u8],
  from: usize,
  budget: usize,
) -> usize {
  let most = budget
    .checked_div(COPY_BYTES)
    .unwrap_or(0)
    .max(1)
    .saturating_mul(COPY_BYTES);
  let end = from.saturating_add(most).min(whole_blocks(source));
  let region = source.get(from..end).unwrap_or_default();
  for (at, block) in region.as_chunks::<COPY_BYTES>().0.iter().enumerate() {
    let offset = from.saturating_add(at.saturating_mul(COPY_BYTES));
    blocks.entry(hash(block)).or_insert(offset);
  }
  end.max(from)
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

/// Where a search stands.
enum Stage {
  /// Indexing the source's blocks from byte `from` on.
  Index { from: usize },
  /// Scanning the target from byte `at` on, `rolling` the hash of the block there.
  Scan { at: usize, rolling: u64 },
  /// Done.
  Done,
}

/// Where a step of the scan left it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scanned {
  /// Stopped at `at`, `rolling` the hash of the block there.
  From { at: usize, rolling: u64 },
  /// The target's end reached.
  End,
}

/// git's delta of a source and a target searched a bounded amount a step (module docs).
pub struct Search {
  /// The lengths of the source and the target the first step was handed.
  lens: Option<(usize, usize)>,
  /// The source's blocks by hash: the first offset of each.
  blocks: BTreeMap<u64, usize>,
  stage: Stage,
  /// The delta written so far.
  out: Vec<u8>,
  /// Where the target's bytes waiting to be inserted begin.
  inserts_from: usize,
  counters: SearchCounters,
}

impl Default for Search {
  fn default() -> Search {
    Search::new()
  }
}

impl Search {
  /// A search with nothing done.
  pub fn new() -> Search {
    Search {
      lens: None,
      blocks: BTreeMap::new(),
      stage: Stage::Index { from: 0 },
      out: Vec::new(),
      inserts_from: 0,
      counters: SearchCounters::default(),
    }
  }

  /// What the search did.
  pub fn counters(&self) -> SearchCounters {
    self.counters
  }

  /// One step over `source` and `target`, the same two every step (module docs): at most
  /// `budget`'s whole blocks of the source indexed, one at least, or the target scanned past
  /// `budget` bytes, one at least. Handed other lengths than the first step was, the search ends
  /// with no delta; after its end, every step answers `Done(None)`.
  pub fn step(&mut self, source: &[u8], target: &[u8], budget: usize) -> SearchStep {
    self.counters.steps = self.counters.steps.saturating_add(1);
    let lens = (source.len(), target.len());
    if *self.lens.get_or_insert(lens) != lens {
      return self.end(None);
    }
    match self.stage {
      Stage::Index { from } => self.index(source, target, from, budget),
      Stage::Scan { at, rolling } => self.scan(source, target, (at, rolling), budget),
      Stage::Done => SearchStep::Done(None),
    }
  }

  /// The search ended with `delta`, its index let go.
  fn end(&mut self, delta: Option<Vec<u8>>) -> SearchStep {
    self.stage = Stage::Done;
    self.blocks = BTreeMap::new();
    SearchStep::Done(delta)
  }

  /// A step of the index (module docs); with the index whole, the scan begun.
  fn index(&mut self, source: &[u8], target: &[u8], from: usize, budget: usize) -> SearchStep {
    let end = index_from(&mut self.blocks, source, from, budget);
    let indexed = to_u64(end.saturating_sub(from));
    self.counters.indexed = self.counters.indexed.saturating_add(indexed);
    if end < whole_blocks(source) {
      self.stage = Stage::Index { from: end };
      return SearchStep::More;
    }
    self.begin_scan(source, target)
  }

  /// The source indexed: the scan begun at the target's first block, or no delta (no block of the
  /// source to copy, or a target no longer than a copy instruction).
  fn begin_scan(&mut self, source: &[u8], target: &[u8]) -> SearchStep {
    if self.blocks.is_empty() || target.len() <= COPY_BYTES {
      return self.end(None);
    }
    let (Ok(source_len), Ok(target_len), Some(first)) = (
      u64::try_from(source.len()),
      u64::try_from(target.len()),
      target.get(..COPY_BYTES),
    ) else {
      return self.end(None);
    };
    let sizes = Sizes {
      source: source_len,
      target: target_len,
    };
    write_sizes(sizes, &mut self.out);
    self.stage = Stage::Scan {
      at: 0,
      rolling: hash(first),
    };
    SearchStep::More
  }

  /// A step of the scan (module docs); with the target's end reached, the delta, when it is
  /// shorter than the target.
  fn scan(
    &mut self,
    source: &[u8],
    target: &[u8],
    (at, rolling): (usize, u64),
    budget: usize,
  ) -> SearchStep {
    let mut scan = Scan {
      source,
      target,
      out: core::mem::take(&mut self.out),
      inserts_from: self.inserts_from,
    };
    let scanned = scan.run(&self.blocks, (at, rolling), at.saturating_add(budget.max(1)));
    if scanned == Some(Scanned::End) {
      scan.flush(target.len());
    }
    self.inserts_from = scan.inserts_from;
    let out = scan.out;
    match scanned {
      Some(Scanned::From { at: next, rolling }) => {
        let moved = to_u64(next.saturating_sub(at));
        self.counters.scanned = self.counters.scanned.saturating_add(moved);
        self.out = out;
        self.stage = Stage::Scan { at: next, rolling };
        SearchStep::More
      }
      Some(Scanned::End) => {
        let moved = to_u64(target.len().saturating_sub(at));
        self.counters.scanned = self.counters.scanned.saturating_add(moved);
        let delta = (out.len() < target.len()).then_some(out);
        self.end(delta)
      }
      None => self.end(None),
    }
  }
}

/// A delta being written over the source and the target one step holds.
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

  /// Scans the target from `at`, `rolling` the hash of the block there: a copy where a block
  /// matches, the rolling hash moved a byte where none does, until the scan has reached `stop` or
  /// the target's end; `None` if a copy cannot be said.
  fn run(
    &mut self,
    blocks: &BTreeMap<u64, usize>,
    (mut at, mut rolling): (usize, u64),
    stop: usize,
  ) -> Option<Scanned> {
    let weight = outgoing_weight();
    while let Some(end) = at
      .checked_add(COPY_BYTES)
      .filter(|end| *end <= self.target.len())
    {
      if at >= stop {
        return Some(Scanned::From { at, rolling });
      }
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
    Some(Scanned::End)
  }
}

/// git's delta turning `source` into `target`, or `None` when it would not be shorter than
/// `target` itself (module docs): one [`Search`] run to its end, a stage a step.
pub fn delta_of(source: &[u8], target: &[u8]) -> Option<Vec<u8>> {
  let mut search = Search::new();
  loop {
    if let SearchStep::Done(delta) = search.step(source, target, usize::MAX) {
      return delta;
    }
  }
}
