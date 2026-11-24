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
//!
//! **What a search holds (A-653 continued (4)).** Besides the source and the target its caller
//! hands it, a search holds its index and its delta, and [`Search::held_most`] bounds both before
//! it begins, so a caller can charge them to a request's admission first. The index keeps at most
//! one entry a whole block of the source in a flat open-addressed power-of-two table at load factor
//! at most 1/2 (Knuth TAOCP Vol. 3 §6.4), each slot an 8-byte `(tag, offset)` pair verified against
//! the full 64-bit block hash on tag match so first-offset tie-breaking is byte-for-byte identical
//! to a map keyed by the 64-bit hash. The delta is reserved whole when the scan begins, at the most
//! the scan can write: its two sizes, the target's bytes, an insert opcode for every [`INSERT_MAX`]
//! bytes and one more, since a copy costs at most [`COPY_BYTES`] and covers more, and each run of
//! inserts a copy ends may cost an opcode; a `Vec` made with a capacity has exactly that capacity
//! (`Vec`'s "Guarantees" \[B\]), so the delta never grows past what was charged.

use std::mem::size_of;

use crate::delta::{INSERT_MAX, Sizes, write_copy, write_insert, write_sizes};
use crate::out::Measure;
use crate::to_u64;

/// Format: the most bytes a copy instruction takes: its opcode and seven operand bytes, four of
/// offset and three of length (`diff-delta.c`, `patch-delta.c`).
const COPY_BYTES: usize = 8;

/// Format: the longest copy git writes, 64 KiB (`diff-delta.c`: "limited to 64KB (pack v2)").
const COPY_MAX: usize = 1 << 16;

/// Derived: the rolling hash's multiplier, the 64-bit golden ratio (Knuth's multiplicative
/// hashing): odd, so multiplying by it loses no bit.
const MULTIPLIER: u64 = 0x9e37_79b9_7f4a_7c15;

/// Format: bit 0 of a [`BlockTable`] slot tag, set on a home slot only when at least one key
/// hashing to that home slot overflowed to a subsequent slot during linear probing.
const OVERFLOW_BIT: u32 = 1;

/// Format: bit 1 of a [`BlockTable`] slot tag, set on every occupied slot and clear on empty slots
/// (`EMPTY_TAG == 0`), so a single tag comparison distinguishes empty or non-matching slots from
/// tag matches or overflowed buckets without reading offsets or adjacent slots.
const OCCUPIED_BIT: u32 = 2;

/// Format: mask of the two metadata bits reserved in the low bits of each 32-bit slot tag.
const TAG_META_MASK: u32 = OVERFLOW_BIT | OCCUPIED_BIT;

/// Format: tag value marking an unoccupied slot in [`BlockTable::tags`].
const EMPTY_TAG: u32 = 0;

/// Derived: open-addressing load-factor denominator keeping α ≤ 1/4 (Knuth TAOCP Vol. 3 §6.4:
/// 1.17 expected unsuccessful probes and ~2.6% bucket overflow rate, so >97% of rolling-hash bytes
/// reject on a single 4-byte tag load in L1D cache without touching offsets or entering slow probe).
const LOAD_FACTOR_DENOM: usize = 4;

/// Flat power-of-two structure-of-arrays open-addressed index of a source's 8-byte blocks, keeping
/// each distinct 64-bit block hash's first byte offset in the source.
///
/// Storing `tags` separately from `offsets` packs 16 tags into a single 64-byte cache line so the
/// per-byte rolling scan touches only `tags` on the >97% miss path and loads `offsets` only on a
/// 30-bit tag hit or genuinely overflowed home bucket.
#[derive(Debug, Default)]
pub(crate) struct BlockTable {
  tags: Vec<u32>,
  offsets: Vec<u32>,
  count: usize,
}

impl BlockTable {
  /// Power-of-two slot count keeping load factor at most 1/4 ([`LOAD_FACTOR_DENOM`]): zero when
  /// the source has no whole block.
  fn capacity_for(source_len: usize) -> usize {
    let entries = source_len.checked_div(COPY_BYTES).unwrap_or(0);
    if entries == 0 {
      return 0;
    }
    entries
      .saturating_mul(LOAD_FACTOR_DENOM)
      .checked_next_power_of_two()
      .unwrap_or(usize::MAX)
  }

  /// Allocates the parallel tag and offset arrays once sized for `source_len` bytes.
  fn ensure_for(&mut self, source_len: usize) {
    if !self.tags.is_empty() {
      return;
    }
    let cap = Self::capacity_for(source_len);
    if cap > 0 {
      self.tags.resize(cap, EMPTY_TAG);
      self.offsets.resize(cap, 0);
    }
  }

  /// Whether the table holds no indexed block.
  fn is_empty(&self) -> bool {
    self.count == 0
  }

  /// Upper 30 bits of `key` with [`OCCUPIED_BIT`] set and [`OVERFLOW_BIT`] clear, so `EMPTY_TAG`
  /// (`0`) never equals a valid tag and a non-overflowed slot matches `tag` iff its 30-bit hash
  /// matches.
  #[inline]
  fn tag(key: u64) -> u32 {
    (u32::try_from(key.wrapping_shr(u32::BITS)).unwrap_or(0) & !TAG_META_MASK) | OCCUPIED_BIT
  }

  /// Initial probe index for `key` inside a power-of-two table masked by `mask`.
  #[inline]
  fn home(key: u64, mask: usize) -> usize {
    let mixed = key ^ key.rotate_right(u32::BITS);
    let masked = mixed & to_u64(mask);
    usize::try_from(masked).unwrap_or(0)
  }

  /// Records `offset` as the first offset of `key` in `source` when no earlier block had the same
  /// full 64-bit hash.
  #[inline]
  fn insert_first(&mut self, source: &[u8], key: u64, offset: usize) {
    let Ok(off32) = u32::try_from(offset) else {
      return;
    };
    let len = self.tags.len();
    if len == 0 || !len.is_power_of_two() || self.offsets.len() != len {
      return;
    }
    let mask = len.wrapping_sub(1);
    let tag = Self::tag(key);
    let home = Self::home(key, mask);
    let mut index = home;
    for _ in 0..len {
      let Some(&slot_tag) = self.tags.get(index) else {
        return;
      };
      if slot_tag == EMPTY_TAG {
        if let Some(tag_slot) = self.tags.get_mut(index) {
          *tag_slot = tag;
        }
        if let Some(off_slot) = self.offsets.get_mut(index) {
          *off_slot = off32;
        }
        if index != home
          && let Some(home_tag) = self.tags.get_mut(home)
        {
          *home_tag |= OVERFLOW_BIT;
        }
        self.count = self.count.saturating_add(1);
        return;
      }
      if (slot_tag & !OVERFLOW_BIT) == tag
        && let Some(&existing_off) = self.offsets.get(index)
        && hash_at(source, existing_off) == Some(key)
      {
        return;
      }
      index = index.wrapping_add(1) & mask;
    }
  }

  /// The first byte offset in `source` whose 8-byte block hashes to `key`, if any.
  #[inline]
  fn get(&self, source: &[u8], key: u64) -> Option<usize> {
    let len = self.tags.len();
    if len == 0 || !len.is_power_of_two() {
      return None;
    }
    let mask = len.wrapping_sub(1);
    let home = Self::home(key, mask);
    let first_tag = *self.tags.get(home)?;
    let tag = Self::tag(key);
    if (first_tag == tag) | ((first_tag & OVERFLOW_BIT) != 0) {
      self.get_slow(source, key, home, first_tag, tag)
    } else {
      None
    }
  }

  /// Slow probe path for [`Self::get`], entered only on a 30-bit tag hit or an overflowed home
  /// bucket (`OVERFLOW_BIT` set), keeping the per-byte rolling scan compact and branch-predicted.
  #[cold]
  #[inline(never)]
  fn get_slow(
    &self,
    source: &[u8],
    key: u64,
    home: usize,
    first_tag: u32,
    tag: u32,
  ) -> Option<usize> {
    if (first_tag & !OVERFLOW_BIT) == tag {
      let off = *self.offsets.get(home)?;
      if hash_at(source, off) == Some(key) {
        return usize::try_from(off).ok();
      }
    }
    if (first_tag & OVERFLOW_BIT) == 0 {
      return None;
    }
    let len = self.tags.len();
    let mask = len.wrapping_sub(1);
    let mut index = home;
    for _ in 1..len {
      index = index.wrapping_add(1) & mask;
      let slot_tag = *self.tags.get(index)?;
      if slot_tag == EMPTY_TAG {
        return None;
      }
      if (slot_tag & !OVERFLOW_BIT) == tag {
        let off = *self.offsets.get(index)?;
        if hash_at(source, off) == Some(key) {
          return usize::try_from(off).ok();
        }
      }
    }
    None
  }
}

/// Incrementally built [`BlockTable`] owned by a reusable window base (`fragment::Recent`) so the
/// source is indexed at most once across all candidate trials while keeping every step bounded.
#[derive(Debug, Default)]
pub(crate) struct BlockIndex {
  table: BlockTable,
  indexed_to: usize,
}

impl BlockIndex {
  /// Advances indexing of `source` by at most `budget`'s whole blocks (at least one): returns
  /// `(indexed_bytes, complete)`.
  pub(crate) fn step(&mut self, source: &[u8], budget: usize) -> (usize, bool) {
    let whole = whole_blocks(source);
    if self.indexed_to >= whole {
      return (0, true);
    }
    let from = self.indexed_to;
    let end = index_from(&mut self.table, source, from, budget);
    self.indexed_to = end;
    (end.saturating_sub(from), end >= whole)
  }

  /// Whether every whole block of `source` has been indexed.
  pub(crate) fn is_complete(&self, source: &[u8]) -> bool {
    self.indexed_to >= whole_blocks(source)
  }
}

/// The most heap bytes the flat block index of a `source_len`-byte source takes (`tags` + `offsets`).
fn index_most(source_len: usize) -> u64 {
  to_u64(BlockTable::capacity_for(source_len))
    .saturating_mul(to_u64(size_of::<u32>().saturating_mul(2)))
}

/// The most bytes the delta of a `target_len`-byte target against a `source_len`-byte source can
/// take as the scan writes it (module docs): its sizes, the target's bytes, an opcode for every
/// [`INSERT_MAX`] of them, and one more.
pub(crate) fn delta_most(source_len: usize, target_len: usize) -> usize {
  let mut sizes = Measure::default();
  write_sizes(
    Sizes {
      source: to_u64(source_len),
      target: to_u64(target_len),
    },
    &mut sizes,
  );
  usize::try_from(sizes.0)
    .unwrap_or(usize::MAX)
    .saturating_add(target_len)
    .saturating_add(target_len.div_ceil(INSERT_MAX))
    .saturating_add(1)
}

/// The hash of `bytes`, a block.
#[inline]
fn hash(bytes: &[u8]) -> u64 {
  bytes.iter().fold(0_u64, |hash, byte| {
    hash.wrapping_mul(MULTIPLIER).wrapping_add(u64::from(*byte))
  })
}

/// The hash of the [`COPY_BYTES`]-byte block starting at `offset` in `source`.
#[inline]
fn hash_at(source: &[u8], offset: u32) -> Option<u64> {
  let at = usize::try_from(offset).ok()?;
  let block = source.get(at..at.checked_add(COPY_BYTES)?)?;
  Some(hash(block))
}

/// The multiplier to the power of a block's length less one: what the byte leaving a block weighs.
const OUTGOING_WEIGHT: u64 = {
  let mut weight = 1_u64;
  let mut step = 1_usize;
  while step < COPY_BYTES {
    weight = weight.wrapping_mul(MULTIPLIER);
    step += 1;
  }
  weight
};

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
fn index_from(blocks: &mut BlockTable, source: &[u8], from: usize, budget: usize) -> usize {
  blocks.ensure_for(source.len());
  let most = budget
    .checked_div(COPY_BYTES)
    .unwrap_or(0)
    .max(1)
    .saturating_mul(COPY_BYTES);
  let end = from.saturating_add(most).min(whole_blocks(source));
  let region = source.get(from..end).unwrap_or_default();
  for (at, block) in region.as_chunks::<COPY_BYTES>().0.iter().enumerate() {
    let offset = from.saturating_add(at.saturating_mul(COPY_BYTES));
    blocks.insert_first(source, hash(block), offset);
  }
  end.max(from)
}

/// How many bytes `source` from `from` and `target` from `at` agree on, at most a copy's length,
/// compared a machine word at a time. Kept out-of-line so the per-byte rolling scan stays compact.
#[cold]
#[inline(never)]
fn agreeing(source: &[u8], from: usize, target: &[u8], at: usize) -> usize {
  let (Some(source), Some(target)) = (source.get(from..), target.get(at..)) else {
    return 0;
  };
  let limit = source.len().min(target.len()).min(COPY_MAX);
  let (Some(left), Some(right)) = (source.get(..limit), target.get(..limit)) else {
    return 0;
  };
  let (l_words, l_tail) = left.as_chunks::<COPY_BYTES>();
  let (r_words, r_tail) = right.as_chunks::<COPY_BYTES>();
  let mut matched = 0_usize;
  for (lw, rw) in l_words.iter().zip(r_words) {
    let diff = u64::from_ne_bytes(*lw) ^ u64::from_ne_bytes(*rw);
    if diff != 0 {
      #[cfg(target_endian = "little")]
      let equal = usize::try_from(diff.trailing_zeros() / u8::BITS).unwrap_or(0);
      #[cfg(target_endian = "big")]
      let equal = usize::try_from(diff.leading_zeros() / u8::BITS).unwrap_or(0);
      return matched.saturating_add(equal);
    }
    matched = matched.saturating_add(COPY_BYTES);
  }
  for (lb, rb) in l_tail.iter().zip(r_tail) {
    if lb != rb {
      break;
    }
    matched = matched.saturating_add(1);
  }
  matched
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
  /// The source's blocks by hash: the first offset of each (used when no shared [`BlockIndex`] is lent).
  blocks: BlockTable,
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
      blocks: BlockTable::default(),
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

  /// The most heap bytes a search over a `source_len`-byte source and a `target_len`-byte target
  /// holds besides the two (module docs, **What a search holds**): its index and its delta.
  pub fn held_most(source_len: usize, target_len: usize) -> u64 {
    index_most(source_len).saturating_add(to_u64(delta_most(source_len, target_len)))
  }

  /// One step over `source` and `target`, the same two every step (module docs): at most
  /// `budget`'s whole blocks of the source indexed, one at least, or the target scanned past
  /// `budget` bytes, one at least. Handed other lengths than the first step was, the search ends
  /// with no delta; after its end, every step answers `Done(None)`.
  pub fn step(&mut self, source: &[u8], target: &[u8], budget: usize) -> SearchStep {
    self.step_bounded(source, target, budget, usize::MAX)
  }

  /// Like [`Search::step`], aborting the scan early as soon as the delta being constructed can no
  /// longer beat `max_delta` bytes (`usize::MAX` disables early abort).
  pub(crate) fn step_bounded(
    &mut self,
    source: &[u8],
    target: &[u8],
    budget: usize,
    max_delta: usize,
  ) -> SearchStep {
    self.counters.steps = self.counters.steps.saturating_add(1);
    let lens = (source.len(), target.len());
    if *self.lens.get_or_insert(lens) != lens {
      return self.end(None);
    }
    match self.stage {
      Stage::Index { from } => self.index(source, target, from, budget, max_delta),
      Stage::Scan { at, rolling } => {
        let blocks = core::mem::take(&mut self.blocks);
        let step = self.scan(&blocks, source, target, (at, rolling), budget, max_delta);
        if matches!(step, SearchStep::More) {
          self.blocks = blocks;
        }
        step
      }
      Stage::Done => SearchStep::Done(None),
    }
  }

  /// Like [`Search::step_bounded`], reusing an incrementally built [`BlockIndex`] owned by the
  /// caller's window entry so each base is indexed once across all trials that test it.
  pub(crate) fn step_indexed(
    &mut self,
    source: &[u8],
    index: &mut BlockIndex,
    target: &[u8],
    budget: usize,
    max_delta: usize,
  ) -> SearchStep {
    self.counters.steps = self.counters.steps.saturating_add(1);
    let lens = (source.len(), target.len());
    if *self.lens.get_or_insert(lens) != lens {
      return self.end(None);
    }
    match self.stage {
      Stage::Index { .. } => {
        if !index.is_complete(source) {
          let (indexed, done) = index.step(source, budget);
          self.counters.indexed = self.counters.indexed.saturating_add(to_u64(indexed));
          if !done {
            return SearchStep::More;
          }
          return self.begin_scan(index.table.is_empty(), source, target, max_delta);
        }
        // Index already built by an earlier trial: begin and immediately run the first scan step.
        match self.begin_scan(index.table.is_empty(), source, target, max_delta) {
          SearchStep::Done(delta) => SearchStep::Done(delta),
          SearchStep::More => match self.stage {
            Stage::Scan { at, rolling } => self.scan(
              &index.table,
              source,
              target,
              (at, rolling),
              budget,
              max_delta,
            ),
            _ => SearchStep::Done(None),
          },
        }
      }
      Stage::Scan { at, rolling } => self.scan(
        &index.table,
        source,
        target,
        (at, rolling),
        budget,
        max_delta,
      ),
      Stage::Done => SearchStep::Done(None),
    }
  }

  /// The search ended with `delta`, its index let go.
  fn end(&mut self, delta: Option<Vec<u8>>) -> SearchStep {
    self.stage = Stage::Done;
    self.blocks = BlockTable::default();
    SearchStep::Done(delta)
  }

  /// A step of the index (module docs); with the index whole, the scan begun.
  fn index(
    &mut self,
    source: &[u8],
    target: &[u8],
    from: usize,
    budget: usize,
    max_delta: usize,
  ) -> SearchStep {
    let end = index_from(&mut self.blocks, source, from, budget);
    let indexed = to_u64(end.saturating_sub(from));
    self.counters.indexed = self.counters.indexed.saturating_add(indexed);
    if end < whole_blocks(source) {
      self.stage = Stage::Index { from: end };
      return SearchStep::More;
    }
    self.begin_scan(self.blocks.is_empty(), source, target, max_delta)
  }

  /// The source indexed: the scan begun at the target's first block, or no delta (no block of the
  /// source to copy, or a target no longer than a copy instruction).
  fn begin_scan(
    &mut self,
    empty_index: bool,
    source: &[u8],
    target: &[u8],
    max_delta: usize,
  ) -> SearchStep {
    if empty_index || target.len() <= COPY_BYTES || max_delta == 0 {
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
    let cap = delta_most(source.len(), target.len()).min(max_delta.saturating_add(COPY_BYTES));
    self.out = Vec::with_capacity(cap);
    write_sizes(sizes, &mut self.out);
    if self.out.len() >= max_delta {
      return self.end(None);
    }
    self.stage = Stage::Scan {
      at: 0,
      rolling: hash(first),
    };
    SearchStep::More
  }

  /// A step of the scan (module docs); with the target's end reached, the delta, when it is
  /// shorter than both the target and `max_delta`.
  fn scan(
    &mut self,
    blocks: &BlockTable,
    source: &[u8],
    target: &[u8],
    (at, rolling): (usize, u64),
    budget: usize,
    max_delta: usize,
  ) -> SearchStep {
    let mut scan = Scan {
      source,
      target,
      out: core::mem::take(&mut self.out),
      inserts_from: self.inserts_from,
      max_delta,
    };
    let scanned = scan.run(blocks, (at, rolling), at.saturating_add(budget.max(1)));
    let flushed = if scanned == Some(Scanned::End) {
      scan.flush_checked(target.len())
    } else {
      true
    };
    self.inserts_from = scan.inserts_from;
    let out = scan.out;
    if !flushed {
      return self.end(None);
    }
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
        let limit = target.len().min(max_delta);
        let delta = (out.len() < limit).then_some(out);
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
  /// Upper bound (exclusive) on a useful delta's length; `usize::MAX` disables early abort.
  max_delta: usize,
}

impl Scan<'_> {
  /// Writes the waiting bytes before `at` as inserts if they can still fit under `max_delta`.
  fn flush_checked(&mut self, at: usize) -> bool {
    let Some(bytes) = self.target.get(self.inserts_from..at) else {
      return true;
    };
    if self.max_delta < usize::MAX {
      let encoded = bytes.len().saturating_add(bytes.len().div_ceil(INSERT_MAX));
      if self.out.len().saturating_add(encoded) >= self.max_delta {
        return false;
      }
    }
    write_insert(bytes, &mut self.out);
    true
  }

  /// Writes a copy of `len` bytes from `from`, after the waiting inserts; `None` if the format
  /// cannot say it or the delta can no longer beat `max_delta`.
  fn copy(&mut self, at: usize, from: usize, len: usize) -> Option<()> {
    if !self.flush_checked(at) {
      return None;
    }
    let offset = u64::try_from(from).ok()?;
    write_copy(offset, u64::try_from(len).ok()?, &mut self.out).ok()?;
    if self.out.len() >= self.max_delta {
      return None;
    }
    self.inserts_from = at.checked_add(len)?;
    Some(())
  }

  /// Slow path for [`Self::scan_until`] once probe 0 either matches `rolling`'s 30-bit tag or
  /// belongs to an overflowed home bucket (`OVERFLOW_BIT` set). Kept out-of-line so the per-byte
  /// rolling loop executes a single 4-byte tag load and one highly-predicted branch on >97% of bytes.
  #[cold]
  #[inline(never)]
  fn matched_from(
    &self,
    blocks: &BlockTable,
    first_tag: u32,
    home: usize,
    rolling: u64,
    at: usize,
  ) -> Option<(usize, usize)> {
    let from = blocks.get_slow(
      self.source,
      rolling,
      home,
      first_tag,
      BlockTable::tag(rolling),
    )?;
    let len = agreeing(self.source, from, self.target, at);
    (len > COPY_BYTES).then_some((from, len))
  }

  /// The length of a match worth a copy at `at`, found through the source's blocks.
  #[inline]
  fn matched(&self, blocks: &BlockTable, rolling: u64, at: usize) -> Option<(usize, usize)> {
    let from = blocks.get(self.source, rolling)?;
    let len = agreeing(self.source, from, self.target, at);
    (len > COPY_BYTES).then_some((from, len))
  }

  /// Advances `(at, rolling)` across `[at, roll_end)` with a single 4-byte tag load and one
  /// branch per byte on the >97% miss path until a copy match is found or `roll_end` is reached.
  #[inline]
  fn scan_until(
    &self,
    blocks: &BlockTable,
    at: &mut usize,
    rolling: &mut u64,
    roll_end: usize,
  ) -> Option<(usize, usize)> {
    let tags = blocks.tags.as_slice();
    if tags.is_empty() || !tags.len().is_power_of_two() {
      return None;
    }
    let mask = tags.len().wrapping_sub(1);
    let mut cur_at = *at;
    let mut cur_rolling = *rolling;
    let leaving = self.target.get(cur_at..roll_end).unwrap_or_default();
    let entering = self
      .target
      .get(cur_at.saturating_add(COPY_BYTES)..roll_end.saturating_add(COPY_BYTES))
      .unwrap_or_default();
    for (&out_byte, &in_byte) in leaving.iter().zip(entering) {
      let home = BlockTable::home(cur_rolling, mask);
      if let Some(&first_tag) = tags.get(home) {
        let tag = BlockTable::tag(cur_rolling);
        if ((first_tag == tag) | ((first_tag & OVERFLOW_BIT) != 0))
          && let Some(hit) = self.matched_from(blocks, first_tag, home, cur_rolling, cur_at)
        {
          *at = cur_at;
          *rolling = cur_rolling;
          return Some(hit);
        }
      }
      let without = cur_rolling.wrapping_sub(u64::from(out_byte).wrapping_mul(OUTGOING_WEIGHT));
      cur_rolling = without
        .wrapping_mul(MULTIPLIER)
        .wrapping_add(u64::from(in_byte));
      cur_at = cur_at.saturating_add(1);
    }
    *at = cur_at;
    *rolling = cur_rolling;
    None
  }

  /// Scans the target from `at`, `rolling` the hash of the block there: a copy where a block
  /// matches, the rolling hash moved a byte where none does, until the scan has reached `stop` or
  /// the target's end; `None` if a copy cannot be said or `max_delta` is already unreachable.
  fn run(
    &mut self,
    blocks: &BlockTable,
    (mut at, mut rolling): (usize, u64),
    stop: usize,
  ) -> Option<Scanned> {
    let Some(last_start) = self.target.len().checked_sub(COPY_BYTES) else {
      return Some(Scanned::End);
    };
    while at <= last_start {
      if at >= stop {
        return Some(Scanned::From { at, rolling });
      }
      let abort_at = if self.max_delta < usize::MAX {
        let room = self.max_delta.checked_sub(self.out.len())?;
        let limit = self.inserts_from.saturating_add(room);
        if at >= limit {
          return None;
        }
        limit
      } else {
        usize::MAX
      };
      let roll_end = stop.min(abort_at).min(last_start);
      let found = if at < roll_end {
        self.scan_until(blocks, &mut at, &mut rolling, roll_end)
      } else {
        None
      };
      let hit = found.or_else(|| {
        (at == last_start && at < stop && at < abort_at)
          .then(|| self.matched(blocks, rolling, at))
          .flatten()
      });
      if let Some((from, len)) = hit {
        self.copy(at, from, len)?;
        at = at.checked_add(len)?;
        rolling = self
          .target
          .get(at..at.saturating_add(COPY_BYTES))
          .map_or(0, hash);
        continue;
      }
      if at >= stop {
        return Some(Scanned::From { at, rolling });
      }
      if at >= abort_at {
        return None;
      }
      if at == last_start {
        return Some(Scanned::End);
      }
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
