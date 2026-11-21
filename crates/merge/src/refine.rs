//! A whole-file rewrite recovered as the edit it was (D-O17; §4.16 "Composition at seal", A-776).
//!
//! An editor saves by writing a temporary file and renaming it over the original; a formatter or a codemod writes the
//! whole file. The deriver composes either route into one `Delete` of the base length and one `Insert` of the new bytes
//! (§4.16 "Declared operations", T-1.19), so a one-token change judged against a concurrent edit anywhere in the file
//! is an `Overlap` conflict. This module recovers the writer's own edit: the minimal byte ranges in which the writer's
//! file differs from the base the writer was given, cut at token boundaries, composed into the same op kinds a declared
//! edit composes into ([`crate::derive`]'s canonical forms). The verdict then judges the edit the tool made.
//!
//! **What this is not (D-27).** It compares the writer's own two states, the base it was handed and the file it wrote,
//! never three. No other agent's bytes are read and nothing is merged: the result is still a set of declared ranges the
//! verdict judges by the same sweep and memcmp, a range two agents both touched is still a conflict, and the post-state
//! is the writer's file byte for byte. The never-diff clause bans reconstructing *a merge* from file states; the edit
//! this recovers is one any SDK `edit` could have declared, and applying it to the base yields the writer's file
//! exactly (the property test `refining_a_rewrite_reproduces_the_post_state`).
//!
//! **The shape, settled by A/B (`examples/rewrite_ab.rs`, BENCHMARKS "Whole-file rewrites recovered").** Tokens (runs of
//! word bytes, runs of blanks, every other byte alone) over Myers' linear-space O(ND) algorithm, after a byte-level
//! trim of the common prefix and suffix snapped out to token boundaries. Lines conflict two edits to different tokens of
//! one line; bytes merge two edits inside one identifier into a word neither wrote; histogram and patience anchor on
//! lines and cost more for nothing a token grain does not already give.
//!
//! **Bounds.** The trim is a linear scan and allocates nothing; only the differing middle is cut into units (four bytes
//! a unit, in scratch the caller keeps and reuses, [`Refiner`]). The search is charged a step per diagonal visited and
//! per unit compared against a budget linear in the middle's units ([`STEPS_PER_UNIT`] a unit plus [`STEPS_BASE`]). When
//! Myers alone runs out (edits spread over the whole file, a formatter's pass), the middle is anchored on the units that
//! occur exactly once in each file, kept in an order both files share (patience sorting), and only the gaps between
//! anchors are searched, under a second budget ([`ANCHORED_STEPS_PER_UNIT`] a unit); a gap that budget does not cover
//! is kept as one replaced range, never searched past it, so the cost of a save is O(file) whatever the tool did. A middle past `u32::MAX` bytes is kept as
//! one range.

use std::borrow::Cow;

use crate::increment::PostStatePlan;
use crate::ops_doc::{Op, OpKind, OpsDoc};

/// Shape: search steps a unit of the differing middle buys Myers alone before it gives up and anchors. Measured
/// (`examples/rewrite_ab.rs`, M3 Ultra, 2026-11-20): at 1, 2, 4 and 64 every workload's verdicts are the same; the p99
/// a save is 50.5, 53.8, 59.7 and 230 µs/KiB (higher buys a longer futile search before anchoring), the median 0.47.
pub const STEPS_PER_UNIT: u64 = 1;
/// Shape: search steps a unit buys the anchored search between unique units. Measured as above: with the budget at 16
/// no formatter pass of the corpus is kept whole (0 of 240 formatter pairs conflict); at a shared budget of 4 a unit,
/// 28 of 240 did, at 1, 78.
pub const ANCHORED_STEPS_PER_UNIT: u64 = 16;
/// Shape: search steps every refinement may spend, however small its middle: 2^16, so a middle of up to 256 units
/// (about a kilobyte of code) is searched to the end whatever its edits (Myers' worst case is (N+M)^2 / 4 steps).
pub const STEPS_BASE: u64 = 1 << 16;

/// The unit a rewrite's difference is found in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grain {
  /// Every byte its own unit (measured and rejected: two edits inside one identifier merge).
  Byte,
  /// Runs of word bytes, runs of blanks, every other byte alone: the grain the seal uses.
  Token,
  /// Lines, each ending after its `\n` (measured and rejected: two edits to one line conflict).
  Line,
}

/// One range the writer replaced: base bytes `[base_at, base_at + base_len)` by the writer's bytes
/// `[post_at, post_at + post_len)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hunk {
  /// Where the replaced base bytes start.
  pub base_at: u64,
  /// How many base bytes are replaced.
  pub base_len: u64,
  /// Where the writer's bytes start in its file.
  pub post_at: u64,
  /// How many bytes the writer put there.
  pub post_len: u64,
}

/// What one refinement did, for the counters and the A/B.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Refined {
  /// Search steps spent.
  pub steps: u64,
  /// Sub-problems the budget did not cover, each kept as one replaced range.
  pub kept_whole: u64,
  /// Whether Myers alone ran out and the search was anchored on unique units.
  pub anchored: bool,
}

/// A unit's tallies by its hash.
/// Shape: its occurrences in the base and in the post, then where it last occurred in each (four).
type UnitCounts = [u32; 4];

/// A sub-problem of the search: units `[b0, b1)` of the base's middle against `[p0, p1)` of the writer's.
#[derive(Clone, Copy, Debug)]
struct Region {
  b0: usize,
  b1: usize,
  p0: usize,
  p1: usize,
}

/// The scratch a refinement works in, kept by the caller and reused (one a shard): the unit boundaries of the two
/// middles, Myers' two diagonal vectors and the search stack. Each grows to what the largest middle it has refined
/// needed; [`Refiner::release`] gives it back. No allocation-free shape exists for the units: a token's boundaries are
/// found by scanning, and the search revisits them by index.
#[derive(Debug, Default)]
pub struct Refiner {
  base_units: Vec<u32>,
  post_units: Vec<u32>,
  forward: Vec<usize>,
  backward: Vec<usize>,
  stack: Vec<Region>,
  hunks: Vec<Hunk>,
  counts: std::collections::HashMap<u64, UnitCounts, std::hash::BuildHasherDefault<Fnv>>,
  anchors: Vec<(u32, u32)>,
  piles: Vec<u32>,
  previous: Vec<u32>,
}

/// FNV-1a over a unit's bytes: the anchors' table key (deterministic, so the same files anchor the same way on every
/// platform; a collision only costs an anchor, since a candidate's bytes are compared before it is used).
fn fnv(bytes: &[u8]) -> u64 {
  /// Format: FNV-1a's 64-bit offset basis and prime (Fowler, Noll and Vo).
  const BASIS: u64 = 0xcbf2_9ce4_8422_2325;
  /// Format: see `BASIS`.
  const PRIME: u64 = 0x0000_0100_0000_01b3;
  bytes.iter().fold(BASIS, |hash, byte| {
    (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
  })
}

/// The anchors' table hasher: the key is already [`fnv`]'s hash, so it passes through.
#[derive(Debug, Default)]
struct Fnv(u64);

impl std::hash::Hasher for Fnv {
  fn finish(&self) -> u64 {
    self.0
  }
  fn write(&mut self, bytes: &[u8]) {
    self.0 = fnv(bytes);
  }
  fn write_u64(&mut self, value: u64) {
    self.0 = value;
  }
}

/// Which class a byte is in, for the token grain: runs of one joinable class are one token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
  Word,
  Blank,
  Alone,
}

fn class(byte: u8) -> Class {
  // Format: UTF-8; a byte at or past 0x80 belongs to a character outside ASCII, a word's.
  if byte.is_ascii_alphanumeric() || byte == b'_' || byte >= 0x80 {
    Class::Word
  } else if byte == b' ' || byte == b'\t' {
    Class::Blank
  } else {
    Class::Alone
  }
}

fn at(text: &[u8], index: usize) -> Option<u8> {
  text.get(index).copied()
}

/// Whether a unit boundary of `grain` falls at `index` of `text` (`index` in `1..text.len()`).
fn boundary(grain: Grain, text: &[u8], index: usize) -> bool {
  let (Some(before), Some(after)) = (at(text, index.wrapping_sub(1)), at(text, index)) else {
    return true;
  };
  match grain {
    Grain::Byte => true,
    Grain::Line => before == b'\n',
    Grain::Token => {
      let c = class(before);
      c == Class::Alone || c != class(after)
    }
  }
}

/// Cuts `text` into units of `grain`: the start of each, then its end.
fn cut(grain: Grain, text: &[u8], out: &mut Vec<u32>) {
  out.clear();
  out.push(0);
  for index in 1..text.len() {
    if boundary(grain, text, index) {
      out.push(u32::try_from(index).unwrap_or(u32::MAX));
    }
  }
  if !text.is_empty() {
    out.push(u32::try_from(text.len()).unwrap_or(u32::MAX));
  }
}

/// The bytes of unit `index` of `text`, cut at `units`.
fn unit<'t>(text: &'t [u8], units: &[u32], index: usize) -> &'t [u8] {
  let start = units.get(index).map_or(0, |s| *s as usize);
  let end = units
    .get(index.saturating_add(1))
    .map_or(start, |e| *e as usize);
  text.get(start..end).unwrap_or_default()
}

/// The two middles being compared, cut into units.
struct Texts<'a> {
  base: &'a [u8],
  post: &'a [u8],
  base_units: &'a [u32],
  post_units: &'a [u32],
}

impl Texts<'_> {
  fn same(&self, b: usize, p: usize, steps: &mut u64) -> bool {
    *steps = steps.saturating_add(1);
    unit(self.base, self.base_units, b) == unit(self.post, self.post_units, p)
  }
}

/// A diagonal vector indexed by `k` in `-limit-1 ..= limit+1`.
fn slot(k: isize, offset: usize) -> usize {
  offset.wrapping_add_signed(k)
}

fn get(v: &[usize], k: isize, offset: usize) -> usize {
  v.get(slot(k, offset)).copied().unwrap_or(0)
}

fn set(v: &mut [usize], k: isize, offset: usize, x: usize) {
  if let Some(cell) = v.get_mut(slot(k, offset)) {
    *cell = x;
  }
}

/// A middle-snake search's fixed shape (Myers 1986, §4b): the region's two lengths, their difference and its parity,
/// and the diagonal vectors' offset.
#[derive(Clone, Copy, Debug)]
struct Snake {
  n: usize,
  m: usize,
  delta: isize,
  odd: bool,
  offset: usize,
}

/// A length as a diagonal coordinate: a middle's units, far below `isize::MAX`.
fn signed(value: usize) -> isize {
  isize::try_from(value).unwrap_or(isize::MAX)
}

/// A diagonal coordinate the caller holds non-negative, as an index.
fn unsigned(value: isize) -> usize {
  usize::try_from(value).unwrap_or(0)
}

/// The furthest x a round `d` reaches on diagonal `k` of `v`, before its snake: from `k + 1` down, or `k - 1` across.
fn furthest(v: &[usize], k: isize, d: isize, offset: usize) -> usize {
  if k == d.saturating_neg()
    || (k != d && get(v, k.saturating_sub(1), offset) < get(v, k.saturating_add(1), offset))
  {
    get(v, k.saturating_add(1), offset)
  } else {
    get(v, k.saturating_sub(1), offset).saturating_add(1)
  }
}

/// Round `d` of the forward search over region `r`: each diagonal's furthest reach and its snake, into `vf`; the
/// middle snake's start where it overlaps the backward search's reach in `vb` (odd `delta`).
fn forward_round(
  vf: &mut [usize],
  vb: &[usize],
  texts: &Texts<'_>,
  r: Region,
  s: Snake,
  d: isize,
  steps: &mut u64,
) -> Option<(usize, usize)> {
  let mut k = d.saturating_neg();
  while k <= d {
    *steps = steps.saturating_add(1);
    let mut x = furthest(vf, k, d, s.offset);
    let y0 = signed(x).saturating_sub(k);
    let x0 = x;
    if y0 >= 0 {
      let mut y = unsigned(y0);
      while x < s.n && y < s.m && texts.same(r.b0.saturating_add(x), r.p0.saturating_add(y), steps)
      {
        x = x.saturating_add(1);
        y = y.saturating_add(1);
      }
    }
    set(vf, k, s.offset, x);
    let other = k.saturating_sub(s.delta);
    if s.odd
      && other.abs() < d
      && x.saturating_add(get(vb, other.saturating_neg(), s.offset)) >= s.n
      && y0 >= 0
    {
      return Some((r.b0.saturating_add(x0), r.p0.saturating_add(unsigned(y0))));
    }
    k = k.saturating_add(2);
  }
  None
}

/// Round `d` of the backward search over region `r`, from the ends: each diagonal's furthest reach into `vb`; the
/// middle snake's point where it overlaps the forward search's reach in `vf` (even `delta`).
fn backward_round(
  vb: &mut [usize],
  vf: &[usize],
  texts: &Texts<'_>,
  r: Region,
  s: Snake,
  d: isize,
  steps: &mut u64,
) -> Option<(usize, usize)> {
  let mut k = d.saturating_neg();
  while k <= d {
    *steps = steps.saturating_add(1);
    let mut x = furthest(vb, k, d, s.offset);
    let mut y = signed(x).saturating_sub(k);
    if y >= 0 {
      while x < s.n
        && unsigned(y) < s.m
        && texts.same(
          r.b0.saturating_add(s.n.saturating_sub(x).saturating_sub(1)),
          r.p0
            .saturating_add(s.m.saturating_sub(unsigned(y)).saturating_sub(1)),
          steps,
        )
      {
        x = x.saturating_add(1);
        y = y.saturating_add(1);
      }
    }
    set(vb, k, s.offset, x);
    let other = k.saturating_sub(s.delta);
    if !s.odd
      && other.abs() <= d
      && x.saturating_add(get(vf, other.saturating_neg(), s.offset)) >= s.n
      && y >= 0
      && unsigned(y) <= s.m
      && x <= s.n
    {
      return Some((
        r.b0.saturating_add(s.n.saturating_sub(x)),
        r.p0.saturating_add(s.m.saturating_sub(unsigned(y))),
      ));
    }
    k = k.saturating_add(2);
  }
  None
}

impl Refiner {
  /// A refiner with empty scratch.
  pub fn new() -> Refiner {
    Refiner::default()
  }

  /// Gives back the scratch the largest refinement grew.
  pub fn release(&mut self) {
    *self = Refiner::default();
  }

  /// The ranges in which `post` (the writer's file) differs from `base` (the file it was given), at the seal's grain.
  pub fn refine(&mut self, base: &[u8], post: &[u8]) -> (&[Hunk], Refined) {
    self.refine_grain(base, post, Grain::Token)
  }

  /// [`Refiner::refine`] at `grain` (the A/B's other grains).
  pub fn refine_grain(&mut self, base: &[u8], post: &[u8], grain: Grain) -> (&[Hunk], Refined) {
    self.refine_bounded(base, post, grain, None)
  }

  /// The bytes of scratch this refiner holds now (the A/B's memory column).
  pub fn scratch_bytes(&self) -> usize {
    let units = self
      .base_units
      .capacity()
      .saturating_add(self.post_units.capacity());
    let diagonals = self
      .forward
      .capacity()
      .saturating_add(self.backward.capacity());
    units
      .saturating_mul(size_of::<u32>())
      .saturating_add(diagonals.saturating_mul(size_of::<usize>()))
      .saturating_add(self.stack.capacity().saturating_mul(size_of::<Region>()))
      .saturating_add(self.hunks.capacity().saturating_mul(size_of::<Hunk>()))
      .saturating_add(
        self
          .counts
          .capacity()
          .saturating_mul(size_of::<(u64, UnitCounts)>()),
      )
      .saturating_add(
        self
          .anchors
          .capacity()
          .saturating_mul(size_of::<(u32, u32)>()),
      )
      .saturating_add(
        self
          .piles
          .capacity()
          .saturating_add(self.previous.capacity())
          .saturating_mul(size_of::<u32>()),
      )
  }

  /// [`Refiner::refine_grain`] with the search's step budget given (`None`: the derived one; `Some(0)`: the trim
  /// alone, the A/B's trim-only arm).
  pub fn refine_bounded(
    &mut self,
    base: &[u8],
    post: &[u8],
    grain: Grain,
    steps: Option<u64>,
  ) -> (&[Hunk], Refined) {
    self.hunks.clear();
    let mut done = Refined::default();
    let (start, base_end, post_end) = trimmed(grain, base, post);
    let base_mid = base.get(start..base_end).unwrap_or_default();
    let post_mid = post.get(start..post_end).unwrap_or_default();
    if base_mid.is_empty() && post_mid.is_empty() {
      return (&self.hunks, done);
    }
    let fits = u32::try_from(base_mid.len()).is_ok() && u32::try_from(post_mid.len()).is_ok();
    if base_mid.is_empty() || post_mid.is_empty() || !fits {
      self.push(start, (base_mid.len(), post_mid.len()), (0, 0));
      return (&self.hunks, done);
    }
    cut(grain, base_mid, &mut self.base_units);
    cut(grain, post_mid, &mut self.post_units);
    let base_units = std::mem::take(&mut self.base_units);
    let post_units = std::mem::take(&mut self.post_units);
    let texts = Texts {
      base: base_mid,
      post: post_mid,
      base_units: &base_units,
      post_units: &post_units,
    };
    let counts = (
      base_units.len().saturating_sub(1),
      post_units.len().saturating_sub(1),
    );
    let units = (counts.0 as u64).saturating_add(counts.1 as u64);
    let budget =
      steps.unwrap_or_else(|| STEPS_BASE.saturating_add(units.saturating_mul(STEPS_PER_UNIT)));
    self.stack.clear();
    self.stack.push(Region {
      b0: 0,
      b1: counts.0,
      p0: 0,
      p1: counts.1,
    });
    self.search(&texts, budget, &mut done);
    if done.kept_whole > 0 && budget > 0 {
      // Myers alone ran out (edits spread over the whole file: a formatter's pass). Anchor on the units that occur
      // once in each file and search only the gaps between them, under a fresh budget of the same size.
      self.hunks.clear();
      done.kept_whole = 0;
      done.anchored = true;
      let spent = done.steps;
      let anchored = STEPS_BASE.saturating_add(units.saturating_mul(ANCHORED_STEPS_PER_UNIT));
      self.anchor(&texts, counts);
      self.search(&texts, anchored.saturating_add(spent), &mut done);
    }
    // The hunks are in units of the middles: turn them into bytes of the files.
    for hunk in &mut self.hunks {
      let byte = |units: &[u32], index: u64| -> u64 {
        let index = usize::try_from(index).unwrap_or(usize::MAX);
        u64::from(units.get(index).copied().unwrap_or(0))
      };
      let b0 = byte(&base_units, hunk.base_at);
      let b1 = byte(&base_units, hunk.base_at.saturating_add(hunk.base_len));
      let p0 = byte(&post_units, hunk.post_at);
      let p1 = byte(&post_units, hunk.post_at.saturating_add(hunk.post_len));
      let start = start as u64;
      *hunk = Hunk {
        base_at: start.saturating_add(b0),
        base_len: b1.saturating_sub(b0),
        post_at: start.saturating_add(p0),
        post_len: p1.saturating_sub(p0),
      };
    }
    self.base_units = base_units;
    self.post_units = post_units;
    (&self.hunks, done)
  }

  /// Appends a replaced range (in whatever coordinates the caller works in), joining it to the last one when they
  /// touch.
  fn push(&mut self, offset: usize, (base_len, post_len): (usize, usize), (b, p): (usize, usize)) {
    let hunk = Hunk {
      base_at: offset.saturating_add(b) as u64,
      base_len: base_len as u64,
      post_at: offset.saturating_add(p) as u64,
      post_len: post_len as u64,
    };
    if let Some(last) = self.hunks.last_mut()
      && last.base_at.saturating_add(last.base_len) == hunk.base_at
      && last.post_at.saturating_add(last.post_len) == hunk.post_at
    {
      last.base_len = last.base_len.saturating_add(hunk.base_len);
      last.post_len = last.post_len.saturating_add(hunk.post_len);
      return;
    }
    self.hunks.push(hunk);
  }

  /// Counts each unit of the two middles (`n` and `m` units) by its hash: how often it occurs in each, and where last.
  fn count_units(&mut self, texts: &Texts<'_>, (n, m): (usize, usize)) {
    self.counts.clear();
    for (side, units) in [(0usize, n), (1, m)] {
      for index in 0..units {
        let bytes = if side == 0 {
          unit(texts.base, texts.base_units, index)
        } else {
          unit(texts.post, texts.post_units, index)
        };
        let entry = self.counts.entry(fnv(bytes)).or_insert([0, 0, 0, 0]);
        let at = u32::try_from(index).unwrap_or(u32::MAX);
        if let Some(count) = entry.get_mut(side) {
          *count = count.saturating_add(1);
        }
        if let Some(slot) = entry.get_mut(side.saturating_add(2)) {
          *slot = at;
        }
      }
    }
  }

  /// Fills the stack with the gaps between the anchors of the two middles' `counts` units: the units that occur exactly
  /// once in each, in an order both files keep (the longest increasing run of their positions, patience sorting, as
  /// Bram Cohen's patience diff anchors on unique lines). The gaps are pushed last first, so they are searched in order.
  fn anchor(&mut self, texts: &Texts<'_>, (n, m): (usize, usize)) {
    self.count_units(texts, (n, m));
    // The candidates in base order, then the longest run increasing in post order.
    self.anchors.clear();
    for index in 0..n {
      let bytes = unit(texts.base, texts.base_units, index);
      if let Some([1, 1, b, p]) = self.counts.get(&fnv(bytes)).copied()
        && bytes == unit(texts.post, texts.post_units, p as usize)
      {
        self.anchors.push((b, p));
      }
    }
    self.piles.clear();
    self.previous.clear();
    for (index, (_, post)) in self.anchors.iter().enumerate() {
      let pile = self.piles.partition_point(|top| {
        self
          .anchors
          .get(*top as usize)
          .is_some_and(|(_, top_post)| top_post < post)
      });
      let below = pile
        .checked_sub(1)
        .and_then(|below| self.piles.get(below).copied())
        .unwrap_or(u32::MAX);
      self.previous.push(below);
      let index = u32::try_from(index).unwrap_or(u32::MAX);
      match self.piles.get_mut(pile) {
        Some(top) => *top = index,
        None => self.piles.push(index),
      }
    }
    // Walk the run back from its last anchor, pushing each gap after an anchor: last gap first.
    self.stack.clear();
    let (mut b1, mut p1) = (n, m);
    let mut at = self.piles.last().copied().unwrap_or(u32::MAX);
    while let Some((b, p)) = self.anchors.get(at as usize).copied() {
      let (b, p) = (b as usize, p as usize);
      self.stack.push(Region {
        b0: b.saturating_add(1),
        b1,
        p0: p.saturating_add(1),
        p1,
      });
      (b1, p1) = (b, p);
      at = self.previous.get(at as usize).copied().unwrap_or(u32::MAX);
    }
    self.stack.push(Region {
      b0: 0,
      b1,
      p0: 0,
      p1,
    });
  }

  /// Myers' linear-space search over the units, depth first and left first so the ranges come out in order.
  fn search(&mut self, texts: &Texts<'_>, budget: u64, done: &mut Refined) {
    while let Some(mut r) = self.stack.pop() {
      while r.b0 < r.b1 && r.p0 < r.p1 && texts.same(r.b0, r.p0, &mut done.steps) {
        r.b0 = r.b0.saturating_add(1);
        r.p0 = r.p0.saturating_add(1);
      }
      while r.b0 < r.b1
        && r.p0 < r.p1
        && texts.same(
          r.b1.saturating_sub(1),
          r.p1.saturating_sub(1),
          &mut done.steps,
        )
      {
        r.b1 = r.b1.saturating_sub(1);
        r.p1 = r.p1.saturating_sub(1);
      }
      let lens = (r.b1.saturating_sub(r.b0), r.p1.saturating_sub(r.p0));
      if lens == (0, 0) {
        continue;
      }
      let split = if lens.0 == 0 || lens.1 == 0 {
        None
      } else {
        self.middle_snake(texts, r, budget, done)
      };
      match split {
        Some((x, y)) if (x, y) != (r.b0, r.p0) && (x, y) != (r.b1, r.p1) => {
          self.stack.push(Region {
            b0: x,
            b1: r.b1,
            p0: y,
            p1: r.p1,
          });
          self.stack.push(Region {
            b0: r.b0,
            b1: x,
            p0: r.p0,
            p1: y,
          });
        }
        _ => {
          if lens.0 != 0 && lens.1 != 0 {
            done.kept_whole = done.kept_whole.saturating_add(u64::from(split.is_none()));
          }
          self.push(0, lens, (r.b0, r.p0));
        }
      }
    }
  }

  /// The point a shortest edit script of region `r` passes through on its middle snake, or `None` once the budget is
  /// spent (Myers 1986, §4b).
  fn middle_snake(
    &mut self,
    texts: &Texts<'_>,
    r: Region,
    budget: u64,
    done: &mut Refined,
  ) -> Option<(usize, usize)> {
    let n = r.b1.saturating_sub(r.b0);
    let m = r.p1.saturating_sub(r.p0);
    let delta = signed(n).saturating_sub(signed(m));
    let odd = delta & 1 != 0;
    let most = n.saturating_add(m).saturating_add(1) / 2;
    // A round d visits d + 1 diagonals each way, so the budget left bounds d by its square root.
    let left = budget.saturating_sub(done.steps);
    let rounds = most.min(usize::try_from(left.isqrt()).unwrap_or(usize::MAX));
    let offset = rounds.saturating_add(2);
    let width = offset.saturating_mul(2).saturating_add(1);
    self.forward.clear();
    self.forward.resize(width, 0);
    self.backward.clear();
    self.backward.resize(width, 0);
    let snake = Snake {
      n,
      m,
      delta,
      odd,
      offset,
    };
    for d in 0..=signed(rounds) {
      if let Some(at) = forward_round(
        &mut self.forward,
        &self.backward,
        texts,
        r,
        snake,
        d,
        &mut done.steps,
      ) {
        return Some(at);
      }
      if let Some(at) = backward_round(
        &mut self.backward,
        &self.forward,
        texts,
        r,
        snake,
        d,
        &mut done.steps,
      ) {
        return Some(at);
      }
      if done.steps > budget {
        return None;
      }
    }
    None
  }
}

/// The differing middle of `base` and `post` at `grain`: the common prefix and suffix trimmed byte by byte, then
/// widened out to unit boundaries both texts share. Returns the middle's start and its end in each.
fn trimmed(grain: Grain, base: &[u8], post: &[u8]) -> (usize, usize, usize) {
  let prefix = base
    .iter()
    .zip(post.iter())
    .take_while(|(a, b)| a == b)
    .count();
  let room = base.len().min(post.len()).saturating_sub(prefix);
  let suffix = base
    .iter()
    .rev()
    .zip(post.iter().rev())
    .take(room)
    .take_while(|(a, b)| a == b)
    .count();
  let mut start = prefix;
  // The bytes before `start` are equal in both, so a boundary there is one both share only when each text has one.
  while start > 0 && !(boundary(grain, base, start) && boundary(grain, post, start)) {
    start = start.saturating_sub(1);
  }
  let mut base_end = base.len().saturating_sub(suffix);
  let mut post_end = post.len().saturating_sub(suffix);
  while base_end < base.len()
    && post_end < post.len()
    && !(boundary(grain, base, base_end) && boundary(grain, post, post_end))
  {
    base_end = base_end.saturating_add(1);
    post_end = post_end.saturating_add(1);
  }
  (start, base_end.max(start), post_end.max(start))
}

/// The canonical net ops of `hunks` over a base of `base_len` bytes and a writer's file of `post_len`, as
/// [`crate::derive`] composes a declared edit: an equal-length replacement is an `Overwrite`, an unequal one a
/// `Delete` then an `Insert`, an addition at the base end an `Extend`, a removal running to both ends a `Truncate`.
/// Each content-adding op's `src` is the offset of its bytes in the writer's file.
pub fn ops_of(hunks: &[Hunk], (base_len, post_len): (u64, u64), path: u64, out: &mut Vec<Op>) {
  let op = |kind: OpKind, at: u64, len: u64, src: u64| Op {
    kind,
    flags: 0,
    path,
    at,
    len,
    src,
  };
  for hunk in hunks {
    let base_end = hunk.base_at.saturating_add(hunk.base_len);
    let post_end = hunk.post_at.saturating_add(hunk.post_len);
    let at_end = base_end == base_len && post_end == post_len;
    match (hunk.base_len, hunk.post_len) {
      (0, 0) => {}
      (skip, 0) => {
        let kind = if at_end {
          OpKind::Truncate
        } else {
          OpKind::Delete
        };
        out.push(op(kind, hunk.base_at, skip, u64::MAX));
      }
      (0, add) => {
        let kind = if at_end && hunk.base_at == base_len {
          OpKind::Extend
        } else {
          OpKind::Insert
        };
        out.push(op(kind, hunk.base_at, add, hunk.post_at));
      }
      (skip, add) if skip == add => {
        out.push(op(OpKind::Overwrite, hunk.base_at, add, hunk.post_at))
      }
      (skip, add) => {
        out.push(op(OpKind::Delete, hunk.base_at, skip, u64::MAX));
        out.push(op(OpKind::Insert, hunk.base_at, add, hunk.post_at));
      }
    }
  }
}

/// What [`refine_rewrites`] did to a document.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rewrites {
  /// Paths whose whole-file replacement was recovered as the edit it was.
  pub paths: u64,
  /// The base bytes those replacements declared replaced, before.
  pub declared_bytes: u64,
  /// The base bytes the recovered edits replace, after.
  pub refined_bytes: u64,
  /// Search steps spent.
  pub steps: u64,
  /// Sub-problems kept as one range for the budget.
  pub kept_whole: u64,
}

/// Whether an op's `src` is an offset into the post-state (its bytes laid there), as the seal lays them.
fn laid(op: &Op) -> bool {
  op.src != u64::MAX
    && matches!(
      op.kind,
      OpKind::Overwrite | OpKind::Insert | OpKind::Extend | OpKind::SetXattr | OpKind::Mknod
    )
}

/// The whole-file replacement a path's ops declare, if that is all they declare about its content and namespace:
/// `(base length, the post-state offset its bytes were laid at, the writer's length)`.
fn whole_replacement(ops: &[Op]) -> Option<(u64, u64, u64)> {
  let mut content = ops.iter().filter(|op| {
    matches!(
      op.kind,
      OpKind::Overwrite | OpKind::Insert | OpKind::Extend | OpKind::Delete | OpKind::Truncate
    )
  });
  let namespace = ops.iter().any(|op| {
    matches!(
      op.kind,
      OpKind::Create
        | OpKind::Unlink
        | OpKind::Rename
        | OpKind::Link
        | OpKind::Symlink
        | OpKind::Mknod
    )
  });
  if namespace || ops.iter().any(|op| op.flags != 0) {
    return None;
  }
  let first = content.next()?;
  let second = content.next();
  if content.next().is_some() {
    return None;
  }
  match (first.kind, second.map(|op| op.kind)) {
    (OpKind::Overwrite, None) if first.at == 0 && first.len > 0 => {
      Some((first.len, first.src, first.len))
    }
    (OpKind::Insert, Some(OpKind::Delete)) | (OpKind::Delete, Some(OpKind::Insert)) => {
      let second = second?;
      let (insert, delete) = if first.kind == OpKind::Insert {
        (first, second)
      } else {
        (second, first)
      };
      (insert.at == 0 && delete.at == 0 && delete.len > 0 && insert.len > 0)
        .then_some((delete.len, insert.src, insert.len))
    }
    _ => None,
  }
}

/// A path's two texts, base and post, as [`refine_rewrites`]' `files` lends them.
pub type BaseAndPost<'f> = (Cow<'f, [u8]>, Cow<'f, [u8]>);

/// Recovers every whole-file replacement in `doc` as the edit it was (this module's docs), rewriting `doc`'s ops and
/// relaying the post-state `plan` compactly (the seal's layout: each laid run at the next offset, in the order the seal
/// laid them). `files` answers a path's base bytes and the writer's bytes; a path it cannot answer, or whose bytes do
/// not match the lengths the document declares, keeps its replacement. Paths with namespace ops or node-exclusive
/// flags keep theirs (a Subversion commit judges a node whole, A-542).
pub fn refine_rewrites<'f>(
  doc: &mut OpsDoc,
  plan: &mut PostStatePlan,
  refiner: &mut Refiner,
  files: &mut dyn FnMut(&str) -> Option<BaseAndPost<'f>>,
) -> Rewrites {
  let mut done = Rewrites::default();
  // Each op, with where its bytes sat in the old layout (`None` when not laid): ops are in canonical order, so a
  // path's ops are contiguous.
  let mut out: Vec<(Op, Option<u64>)> = Vec::with_capacity(doc.ops.len());
  let mut start = 0;
  while start < doc.ops.len() {
    let path = doc.ops.get(start).map_or(0, |op| op.path);
    let end = doc
      .ops
      .iter()
      .skip(start)
      .position(|op| op.path != path)
      .map_or(doc.ops.len(), |n| start.saturating_add(n));
    let ops = doc.ops.get(start..end).unwrap_or_default();
    start = end;
    let refined = whole_replacement(ops).and_then(|(base_len, laid_at, post_len)| {
      let name = doc.paths.path(path)?;
      let at_in_file = *plan.get(&laid_at)?;
      let (base, post) = files(name)?;
      (base.len() as u64 == base_len && post.len() as u64 == post_len && at_in_file == 0)
        .then_some(())?;
      let (hunks, cost) = refiner.refine(&base, &post);
      done.steps = done.steps.saturating_add(cost.steps);
      done.kept_whole = done.kept_whole.saturating_add(cost.kept_whole);
      let mut refined = Vec::new();
      ops_of(hunks, (base_len, post_len), path, &mut refined);
      done.paths = done.paths.saturating_add(1);
      done.declared_bytes = done.declared_bytes.saturating_add(base_len);
      let replaced: u64 = hunks.iter().map(|h| h.base_len).sum();
      done.refined_bytes = done.refined_bytes.saturating_add(replaced);
      // Each refined op's bytes sat inside the replacement's run, at its offset in the writer's file.
      Some(
        refined
          .into_iter()
          .map(|op| {
            let old = laid(&op).then(|| laid_at.saturating_add(op.src));
            (op, old)
          })
          .collect::<Vec<_>>(),
      )
    });
    match refined {
      Some(ops) => out.extend(ops),
      None => out.extend(ops.iter().map(|op| (*op, laid(op).then_some(op.src)))),
    }
  }
  if done.paths == 0 {
    return done;
  }
  // Relay the post-state: the laid runs in their old order, each at the next offset; a content op's bytes still come
  // from its offset in the writer's file (the old plan's, or the refined op's own `src` there).
  let mut order: Vec<usize> = (0..out.len())
    .filter(|i| out.get(*i).is_some_and(|(_, old)| old.is_some()))
    .collect();
  order.sort_by_key(|i| out.get(*i).and_then(|(_, old)| *old));
  let mut relaid = PostStatePlan::new();
  let mut cursor = 0u64;
  for index in order {
    let Some((op, Some(old))) = out.get_mut(index) else {
      continue;
    };
    if matches!(op.kind, OpKind::Overwrite | OpKind::Insert | OpKind::Extend) {
      // An unrefined op's file offset is the old plan's; a refined op carries it in `src` and its old position is
      // `laid_at + src`, which the old plan does not name.
      let in_file = plan.get(old).copied().unwrap_or(op.src);
      relaid.insert(cursor, in_file);
    }
    op.src = cursor;
    cursor = cursor.saturating_add(op.len);
  }
  doc.ops = out.into_iter().map(|(op, _)| op).collect();
  doc.canonicalize();
  *plan = relaid;
  done
}

#[cfg(test)]
mod tests;
