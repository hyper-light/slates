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
//!
//! **Sliced under the shard's quantum (§4.3's step budget, CLAUDE.md §3 "Bounded work everywhere"; A-787).** Every piece
//! of work, a byte scanned or compared, a diagonal visited, a unit counted into the anchors' table, is charged to a
//! meter that asks the owner's turn for the next unit ([`crate::pace::Ticks`]) every [`WORK_PER_TICK`] of it, so a
//! refinement driven by a [`crate::pace::Pace`] spans turns and none of its units is longer than that much work. No
//! single charge can exceed it: a token is cut at [`WORK_PER_TICK`] bytes, so one comparison is bounded too, and the
//! sort and canonical order of a refined document are the paced ones. The sync entry points run the same code under no
//! turn ([`crate::pace::finish`]).

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

/// Shape: the work a unit of a paced refinement may do before it asks its turn for the next (a byte scanned, compared
/// or hashed, a diagonal visited, a word hashed for the anchors as [`TABLE_WORK`]); also the longest a token is cut,
/// so one comparison stays within a unit. Measured (`examples/rewrite_slices.rs`, Apple M4 Max, release, load 13,
/// 2026-11-20): the longest unit is 10 to 46 µs on the crate's sources, 31 to 99 µs at 1 and 4 MiB, and typically 40 to
/// 300 µs at 12 MiB, under the shard's quantum (its measured wake), and an ask's clock read is a hundredth of a unit.
pub const WORK_PER_TICK: u64 = 1 << 12;
/// Shape: the work charged for counting one unit into the anchors' table (its bytes are charged as well): a hash-table
/// insertion costs about what comparing 32 bytes does (measured with the figure above).
const TABLE_WORK: u64 = 32;

/// Charges `$work` to `$meter`, asking for the next unit once a unit's worth is done (inside an `async` body).
macro_rules! charge {
  ($meter:expr, $work:expr) => {
    if $meter.add($work) {
      $meter.ask().await;
    }
  };
}

/// The work a paced refinement has done since it last asked for a unit, and the ask ([`WORK_PER_TICK`]).
struct Meter<'t> {
  ticks: &'t crate::pace::Ticks,
  since: u64,
  longest: u64,
  asks: u64,
}

impl Meter<'_> {
  fn new(ticks: &crate::pace::Ticks) -> Meter<'_> {
    Meter {
      ticks,
      since: 0,
      longest: 0,
      asks: 0,
    }
  }

  /// Adds `work`; whether a unit's worth is done, so the caller asks for the next ([`Meter::ask`]). Synchronous, so
  /// the hot loops build no future for a charge that asks nothing ([`charge!`]).
  #[inline]
  fn add(&mut self, work: u64) -> bool {
    self.since = self.since.saturating_add(work);
    self.since >= WORK_PER_TICK
  }

  /// The work left in this unit before it asks: what a synchronous inner loop may do before it charges (at least one).
  #[inline]
  fn room(&self) -> u64 {
    WORK_PER_TICK.saturating_sub(self.since).max(1)
  }

  /// Asks the turn for the next unit.
  async fn ask(&mut self) {
    self.longest = self.longest.max(self.since);
    self.since = 0;
    self.asks = self.asks.saturating_add(1);
    self.ticks.tick().await;
  }

  /// The longest stretch of work between two asks, the one in progress included.
  fn longest(&self) -> u64 {
    self.longest.max(self.since)
  }
}

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
  /// The most work done between two asks for a unit ([`WORK_PER_TICK`]; the non-vacuity figure of the slicing).
  pub longest_unit_work: u64,
  /// The asks for a unit made.
  pub asks: u64,
}

/// The ranges a refinement found, kept in chunks that double from [`FIRST_CHUNK`] ranges up to [`WORK_PER_TICK`], so a
/// push never moves the ones before it: a vector's doubling would copy every range in one step, which grows with the
/// file (A-787), and a small file's ranges cost a small chunk.
#[derive(Debug, Default)]
pub struct HunkList {
  chunks: Vec<Vec<Hunk>>,
  len: usize,
}

/// Shape: the ranges a [`HunkList`]'s first chunk holds: 64, two kilobytes, about what one save of a source file makes.
const FIRST_CHUNK: usize = 64;

impl HunkList {
  /// The capacity of the next chunk: double the last, at most a unit's work of ranges.
  fn next_chunk(&self) -> usize {
    let cap = usize::try_from(WORK_PER_TICK).unwrap_or(usize::MAX).max(1);
    self
      .chunks
      .last()
      .map_or(FIRST_CHUNK, |last| last.capacity().saturating_mul(2))
      .clamp(1, cap)
  }

  fn clear(&mut self) {
    self.chunks.clear();
    self.len = 0;
  }

  fn push(&mut self, hunk: Hunk) {
    let room = self
      .chunks
      .last()
      .is_some_and(|chunk| chunk.len() < chunk.capacity());
    if !room {
      let next = self.next_chunk();
      self.chunks.push(Vec::with_capacity(next));
    }
    if let Some(chunk) = self.chunks.last_mut() {
      chunk.push(hunk);
      self.len = self.len.saturating_add(1);
    }
  }

  fn last_mut(&mut self) -> Option<&mut Hunk> {
    self.chunks.last_mut().and_then(|chunk| chunk.last_mut())
  }

  /// The ranges found.
  pub fn len(&self) -> usize {
    self.len
  }

  /// Whether none was found (the files are equal).
  pub fn is_empty(&self) -> bool {
    self.len == 0
  }

  /// The `index`th range (a walk of the chunks: for a caller that wants one or two, not a loop).
  pub fn get(&self, index: usize) -> Option<Hunk> {
    self.iter().nth(index).copied()
  }

  /// The chunks, in order: what a paced loop walks.
  pub fn chunks(&self) -> impl Iterator<Item = &[Hunk]> {
    self.chunks.iter().map(Vec::as_slice)
  }

  /// The ranges in order.
  pub fn iter(&self) -> impl Iterator<Item = &Hunk> {
    self.chunks.iter().flatten()
  }

  fn iter_mut(&mut self) -> impl Iterator<Item = &mut Hunk> {
    self.chunks.iter_mut().flatten()
  }

  fn capacity_bytes(&self) -> usize {
    self
      .chunks
      .iter()
      .map(Vec::capacity)
      .sum::<usize>()
      .saturating_mul(size_of::<Hunk>())
  }
}

impl<'a> IntoIterator for &'a HunkList {
  type Item = &'a Hunk;
  type IntoIter = core::iter::Flatten<core::slice::Iter<'a, Vec<Hunk>>>;

  fn into_iter(self) -> Self::IntoIter {
    self.chunks.iter().flatten()
  }
}

/// A unit of either middle as the anchors' sort orders it, in one word: its bytes' hash in the high 32 bits, then its
/// side (bit 31: the writer's) and its index in the low 31. Eight bytes a unit; a hash collision only costs an anchor,
/// since a candidate's bytes are compared before it is used.
type Tagged = u64;
/// Format: the bit of a [`Tagged`] word naming the writer's side.
const POST_SIDE: u64 = 1 << 31;
/// Format: the low 31 bits of a [`Tagged`] word, the unit's index.
const INDEX_BITS: u64 = POST_SIDE - 1;

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
  hunks: HunkList,
  entries: Vec<Tagged>,
  spare: Vec<Tagged>,
  digits: Vec<u32>,
  post_of: Vec<u32>,
  anchors: Vec<(u32, u32)>,
  piles: Vec<u32>,
  previous: Vec<u32>,
  gaps: Vec<Region>,
}

/// FNV-1a over a unit's bytes: the anchors' sort key (deterministic, so the same files anchor the same way on every
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

/// Which class a byte is in, for the token grain: runs of one joinable class are one token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
  Word,
  Blank,
  Alone,
}

/// Every byte's class ([`classify`]), looked up rather than branched on in the cut's scan.
const CLASSES: [Class; 256] = {
  let mut table = [Class::Alone; 256];
  let mut byte = 0usize;
  while byte < 256 {
    table[byte] = classify(byte as u8);
    byte += 1;
  }
  table
};

fn class(byte: u8) -> Class {
  CLASSES[byte as usize]
}

const fn classify(byte: u8) -> Class {
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

/// Where unit boundaries of `grain` fall in `text[from..to]` (each `at` in `1..text.len()`), handed to `found` in
/// order, with `last` the previous boundary (for the length cap); one pass, each byte's class computed once.
fn boundaries(
  grain: Grain,
  text: &[u8],
  (from, to): (usize, usize),
  last: &mut usize,
  mut found: impl FnMut(usize),
) {
  let cap = usize::try_from(WORK_PER_TICK).unwrap_or(usize::MAX);
  let mut before = from.checked_sub(1).and_then(|at| text.get(at).copied());
  for (offset, byte) in text.get(from..to).unwrap_or_default().iter().enumerate() {
    let at = from.saturating_add(offset);
    let natural = match (grain, before) {
      (_, None) => false,
      (Grain::Byte, Some(_)) => true,
      (Grain::Line, Some(prev)) => prev == b'\n',
      (Grain::Token, Some(prev)) => {
        let c = class(prev);
        c == Class::Alone || c != class(*byte)
      }
    };
    if at > 0 && (natural || at.saturating_sub(*last) >= cap) {
      found(at);
      *last = at;
    }
    before = Some(*byte);
  }
}

/// Cuts `text` into units of `grain`: the start of each, then its end. A unit reaching [`WORK_PER_TICK`] bytes is cut
/// there, so one unit's comparison stays within a unit of work; its cuts fall at the same offsets from its start in
/// both texts, so equal long tokens still match piece by piece.
async fn cut(grain: Grain, text: &[u8], out: &mut Vec<u32>, meter: &mut Meter<'_>) {
  // Counted first, then laid into storage of exactly that size: a vector grown by doubling would copy every unit found
  // so far in one step.
  // Each pass scans a unit's room of bytes synchronously, then charges it.
  let mut count = 2usize;
  let mut last = 0usize;
  let mut index = 1usize;
  while index < text.len() {
    let end = index
      .saturating_add(usize::try_from(meter.room()).unwrap_or(usize::MAX))
      .min(text.len());
    boundaries(grain, text, (index, end), &mut last, |_| {
      count = count.saturating_add(1)
    });
    charge!(meter, end.saturating_sub(index) as u64);
    index = end;
  }
  if out.capacity() < count {
    *out = Vec::with_capacity(count);
  }
  out.clear();
  out.push(0);
  last = 0;
  index = 1;
  while index < text.len() {
    let end = index
      .saturating_add(usize::try_from(meter.room()).unwrap_or(usize::MAX))
      .min(text.len());
    boundaries(grain, text, (index, end), &mut last, |at| {
      out.push(u32::try_from(at).unwrap_or(u32::MAX));
    });
    charge!(meter, end.saturating_sub(index) as u64);
    index = end;
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
  /// Compares unit pairs `at(0)`, `at(1)`, ... while they match and the work (each comparison's bytes and one) stays
  /// under `room`: how many matched, the work done, and whether it stopped for room (so more may match). Synchronous, so
  /// the hot loop keeps its state in registers; the caller charges the work between runs ([`Texts::run_all`]).
  fn run(
    &self,
    room: u64,
    steps: &mut u64,
    at: impl Fn(usize) -> Option<(usize, usize)>,
  ) -> (usize, u64, bool) {
    let mut matched = 0usize;
    let mut work = 0u64;
    loop {
      if work >= room {
        return (matched, work, true);
      }
      let Some((b, p)) = at(matched) else {
        return (matched, work, false);
      };
      *steps = steps.saturating_add(1);
      let (a, z) = (
        unit(self.base, self.base_units, b),
        unit(self.post, self.post_units, p),
      );
      work = work
        .saturating_add(a.len().min(z.len()) as u64)
        .saturating_add(1);
      if a != z {
        return (matched, work, false);
      }
      matched = matched.saturating_add(1);
    }
  }

  /// [`Texts::run`] to its end, a unit's work at a time: how many pairs matched.
  async fn run_all(
    &self,
    (steps, meter): (&mut u64, &mut Meter<'_>),
    at: impl Fn(usize) -> Option<(usize, usize)>,
  ) -> usize {
    let mut total = 0usize;
    loop {
      let (matched, work, more) = self.run(meter.room(), steps, |i| at(total.saturating_add(i)));
      total = total.saturating_add(matched);
      charge!(meter, work);
      if !more {
        return total;
      }
    }
  }
}

/// [`Texts::run`] synchronously within the unit's room, then, only if it stopped for room, on a unit at a time
/// ([`Texts::run_all`]); the work charged (one for the call). How many pairs matched.
async fn run_sync_first(
  texts: &Texts<'_>,
  (steps, meter): (&mut u64, &mut Meter<'_>),
  at: impl Fn(usize) -> Option<(usize, usize)> + Copy,
) -> usize {
  let (mut matched, work, more) = texts.run(meter.room(), steps, at);
  charge!(meter, work.saturating_add(1));
  if more {
    let from = matched;
    matched = matched.saturating_add(
      texts
        .run_all((steps, meter), |i| at(from.saturating_add(i)))
        .await,
    );
  }
  matched
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

/// Where a round of the search stands between units: the next diagonal, and the snake in progress on it (its start
/// `(x0, y0)` and how far it has reached), if one stopped for room.
#[derive(Clone, Copy, Debug)]
struct Round {
  k: isize,
  snake: Option<(usize, isize, usize)>,
}

/// What a round's synchronous run ended on.
enum RoundEnd {
  /// The middle snake's point.
  Found((usize, usize)),
  /// Every diagonal of the round visited, no overlap.
  Finished,
  /// The unit's room spent: resume from the [`Round`].
  Paused,
}

/// Round `d` of the search over region `r` in one direction (`backward`: from the ends), run synchronously from `at`
/// until it ends or `room` is spent: each diagonal's furthest reach and its snake, into `mine`; the middle snake's point
/// where it overlaps the other direction's reach in `theirs` (odd `delta` forward, even backward). The work done.
#[allow(clippy::too_many_arguments)]
fn round_within(
  (mine, theirs): (&mut [usize], &[usize]),
  texts: &Texts<'_>,
  (r, s, d, backward): (Region, Snake, isize, bool),
  (steps, room): (&mut u64, u64),
  at: &mut Round,
) -> (RoundEnd, u64) {
  let mut work = 0u64;
  while at.k <= d {
    if work >= room {
      return (RoundEnd::Paused, work);
    }
    let k = at.k;
    let (x0, y0, mut x) = match at.snake.take() {
      Some(snake) => snake,
      None => {
        *steps = steps.saturating_add(1);
        work = work.saturating_add(1);
        let x = furthest(mine, k, d, s.offset);
        (x, signed(x).saturating_sub(k), x)
      }
    };
    if y0 >= 0 {
      let y = unsigned(y0).saturating_add(x.saturating_sub(x0));
      let (x1, y1) = (x, y);
      let pair = |i: usize| {
        let (b, p) = (x1.saturating_add(i), y1.saturating_add(i));
        (b < s.n && p < s.m).then(|| {
          if backward {
            (
              r.b0.saturating_add(s.n.saturating_sub(b).saturating_sub(1)),
              r.p0.saturating_add(s.m.saturating_sub(p).saturating_sub(1)),
            )
          } else {
            (r.b0.saturating_add(b), r.p0.saturating_add(p))
          }
        })
      };
      let (matched, done, more) = texts.run(room.saturating_sub(work).max(1), steps, pair);
      work = work.saturating_add(done);
      x = x.saturating_add(matched);
      if more {
        at.snake = Some((x0, y0, x));
        return (RoundEnd::Paused, work);
      }
    }
    set(mine, k, s.offset, x);
    let other = k.saturating_sub(s.delta);
    let y = y0.saturating_add(signed(x.saturating_sub(x0)));
    let reached = x.saturating_add(get(theirs, other.saturating_neg(), s.offset)) >= s.n;
    if !backward && s.odd && other.abs() < d && reached && y0 >= 0 {
      return (
        RoundEnd::Found((r.b0.saturating_add(x0), r.p0.saturating_add(unsigned(y0)))),
        work,
      );
    }
    if backward && !s.odd && other.abs() <= d && reached && y >= 0 && unsigned(y) <= s.m && x <= s.n
    {
      return (
        RoundEnd::Found((
          r.b0.saturating_add(s.n.saturating_sub(x)),
          r.p0.saturating_add(s.m.saturating_sub(unsigned(y))),
        )),
        work,
      );
    }
    at.k = k.saturating_add(2);
  }
  (RoundEnd::Finished, work)
}

/// Round `d` of the search in one direction ([`round_within`]), a unit's room at a time.
async fn round(
  (mine, theirs): (&mut [usize], &[usize]),
  texts: &Texts<'_>,
  shape: (Region, Snake, isize, bool),
  (steps, meter): (&mut u64, &mut Meter<'_>),
) -> Option<(usize, usize)> {
  let mut at = Round {
    k: shape.2.saturating_neg(),
    snake: None,
  };
  loop {
    let (end, work) = round_within(
      (&mut *mine, theirs),
      texts,
      shape,
      (&mut *steps, meter.room()),
      &mut at,
    );
    charge!(meter, work);
    match end {
      RoundEnd::Found(point) => return Some(point),
      RoundEnd::Finished => return None,
      RoundEnd::Paused => {}
    }
  }
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
  pub fn refine(&mut self, base: &[u8], post: &[u8]) -> (&HunkList, Refined) {
    self.refine_grain(base, post, Grain::Token)
  }

  /// [`Refiner::refine_bounded`] a unit at a time under `ticks` (module docs, "Sliced"): its owner drives it a turn at a
  /// time ([`crate::pace::Pace::drive`]); the same ranges whatever the pace.
  pub async fn refine_paced(
    &mut self,
    (base, post): (&[u8], &[u8]),
    (grain, steps): (Grain, Option<u64>),
    ticks: &crate::pace::Ticks,
  ) -> Refined {
    let mut meter = Meter::new(ticks);
    let mut done = self
      .refine_metered((base, post), (grain, steps), &mut meter)
      .await;
    done.longest_unit_work = meter.longest();
    done.asks = meter.asks;
    done
  }

  /// The ranges the last refinement found.
  pub fn hunks(&self) -> &HunkList {
    &self.hunks
  }

  /// [`Refiner::refine`] at `grain` (the A/B's other grains).
  pub fn refine_grain(&mut self, base: &[u8], post: &[u8], grain: Grain) -> (&HunkList, Refined) {
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
      .saturating_add(self.hunks.capacity_bytes())
      .saturating_add(
        self
          .entries
          .capacity()
          .saturating_add(self.spare.capacity())
          .saturating_mul(size_of::<Tagged>()),
      )
      .saturating_add(
        self
          .digits
          .capacity()
          .saturating_add(self.post_of.capacity())
          .saturating_mul(size_of::<u32>()),
      )
      .saturating_add(self.gaps.capacity().saturating_mul(size_of::<Region>()))
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
  ) -> (&HunkList, Refined) {
    let ticks = crate::pace::Ticks;
    let done = crate::pace::finish(self.refine_paced((base, post), (grain, steps), &ticks));
    (&self.hunks, done)
  }

  /// [`Refiner::refine_paced`]'s work, charged to `meter`.
  async fn refine_metered(
    &mut self,
    (base, post): (&[u8], &[u8]),
    (grain, steps): (Grain, Option<u64>),
    meter: &mut Meter<'_>,
  ) -> Refined {
    self.hunks.clear();
    let mut done = Refined::default();
    let (start, base_end, post_end) = trimmed(grain, base, post, meter).await;
    let base_mid = base.get(start..base_end).unwrap_or_default();
    let post_mid = post.get(start..post_end).unwrap_or_default();
    if base_mid.is_empty() && post_mid.is_empty() {
      return done;
    }
    let fits = u32::try_from(base_mid.len()).is_ok() && u32::try_from(post_mid.len()).is_ok();
    if base_mid.is_empty() || post_mid.is_empty() || !fits {
      self.push(start, (base_mid.len(), post_mid.len()), (0, 0));
      return done;
    }
    cut(grain, base_mid, &mut self.base_units, meter).await;
    cut(grain, post_mid, &mut self.post_units, meter).await;
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
    self.search(&texts, budget, &mut done, meter).await;
    if done.kept_whole > 0 && budget > 0 {
      // Myers alone ran out (edits spread over the whole file: a formatter's pass). Anchor on the units that occur
      // once in each file and search only the gaps between them, under a fresh budget of the same size.
      self.hunks.clear();
      done.kept_whole = 0;
      done.anchored = true;
      let spent = done.steps;
      let anchored = STEPS_BASE.saturating_add(units.saturating_mul(ANCHORED_STEPS_PER_UNIT));
      self.anchor(&texts, counts, meter).await;
      // Each gap searched in order, alone, so the search's stack holds one gap's sub-problems, never every gap's.
      let gaps = std::mem::take(&mut self.gaps);
      for gap in gaps.iter().rev() {
        self.stack.clear();
        self.stack.push(*gap);
        self
          .search(&texts, anchored.saturating_add(spent), &mut done, meter)
          .await;
      }
      self.gaps = gaps;
    }
    // The hunks are in units of the middles: turn them into bytes of the files.
    for hunk in self.hunks.iter_mut() {
      charge!(meter, 1);
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
    done
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

  /// Fills `gaps`, last first, with the gaps between the anchors of the two middles' `counts` units: the units that occur exactly
  /// once in each, in an order both files keep (the longest increasing run of their positions, patience sorting, as
  /// Bram Cohen's patience diff anchors on unique lines). Every unit is hashed into `entries`, which are sorted in place
  /// (a paced heapsort: no step grows with the file, as a hash table's rehash did) so equal units fall together; each
  /// vector is laid at exactly the size it needs.
  async fn anchor(&mut self, texts: &Texts<'_>, (n, m): (usize, usize), meter: &mut Meter<'_>) {
    self.gaps.clear();
    if n as u64 > INDEX_BITS || m as u64 > INDEX_BITS {
      // Past what a word's index holds: one gap, the whole middle, searched as Myers alone was.
      self.gaps.push(Region {
        b0: 0,
        b1: n,
        p0: 0,
        p1: m,
      });
      return;
    }
    // The words counted first, so their entries are laid at exactly that size.
    let mut words = 0usize;
    for (count, text, units) in [
      (n, texts.base, texts.base_units),
      (m, texts.post, texts.post_units),
    ] {
      let mut index = 0usize;
      while index < count {
        let end = index
          .saturating_add(usize::try_from(meter.room()).unwrap_or(usize::MAX))
          .min(count);
        for at in index..end {
          let first = unit(text, units, at).first().copied();
          words = words.saturating_add(usize::from(
            first.is_some_and(|byte| class(byte) == Class::Word),
          ));
        }
        charge!(meter, end.saturating_sub(index) as u64);
        index = end;
      }
    }
    if self.entries.capacity() < words {
      self.entries = Vec::with_capacity(words);
    }
    self.entries.clear();
    // Each unit hashed into its word, a unit's room of them at a time.
    for (side, count, text, units) in [
      (0, n, texts.base, texts.base_units),
      (POST_SIDE, m, texts.post, texts.post_units),
    ] {
      let mut index = 0usize;
      while index < count {
        let room = meter.room();
        let mut work = 0u64;
        while index < count && work < room {
          let bytes = unit(text, units, index);
          // Only words anchor: a blank run or a punctuation byte is almost never unique to each file, and leaving them
          // out halves the sort (A-787).
          if bytes
            .first()
            .is_some_and(|byte| class(*byte) == Class::Word)
          {
            work = work
              .saturating_add(TABLE_WORK)
              .saturating_add(bytes.len() as u64);
            self
              .entries
              .push((fnv(bytes) >> 32) << 32 | side | index as u64);
          } else {
            work = work.saturating_add(1);
          }
          index = index.saturating_add(1);
        }
        charge!(meter, work);
      }
    }
    radix_sort_by_hash(
      &mut self.entries,
      (&mut self.spare, &mut self.digits),
      meter,
    )
    .await;
    // A unit unique to each side is a hash met exactly twice, the base's first (the sort is stable, and every base
    // entry was laid before every writer's). Each such pair is noted at its base index, plus one (zero: none): the
    // map is zeroed by the allocator, so laying it is no step of ours.
    let unique = |entries: &[Tagged], at: usize| -> Option<(u32, u32)> {
      let hash_of = |word: &Tagged| *word >> 32;
      let b = *entries.get(at)?;
      let p = *entries.get(at.saturating_add(1))?;
      let hash = hash_of(&b);
      let alone_before = at == 0
        || entries
          .get(at.wrapping_sub(1))
          .is_none_or(|e| hash_of(e) != hash);
      let alone_after = entries
        .get(at.saturating_add(2))
        .is_none_or(|e| hash_of(e) != hash);
      (hash_of(&p) == hash
        && alone_before
        && alone_after
        && b & POST_SIDE == 0
        && p & POST_SIDE != 0)
        .then(|| {
          (
            u32::try_from(b & INDEX_BITS).unwrap_or(u32::MAX),
            u32::try_from(p & INDEX_BITS).unwrap_or(u32::MAX),
          )
        })
    };
    zeroed(&mut self.post_of, n, meter).await;
    let mut found = 0usize;
    for at in 0..self.entries.len() {
      charge!(meter, 1);
      if let Some((b, p)) = unique(&self.entries, at)
        && unit(texts.base, texts.base_units, b as usize)
          == unit(texts.post, texts.post_units, p as usize)
        && let Some(slot) = self.post_of.get_mut(b as usize)
      {
        *slot = p.saturating_add(1);
        found = found.saturating_add(1);
      }
    }
    // In base order, then the longest run increasing in post order.
    self.anchors = Vec::with_capacity(found);
    for b in 0..n {
      charge!(meter, 1);
      if let Some(p) = self.post_of.get(b).copied().filter(|p| *p != 0) {
        self
          .anchors
          .push((u32::try_from(b).unwrap_or(u32::MAX), p.saturating_sub(1)));
      }
    }
    self.piles = Vec::with_capacity(self.anchors.len());
    self.previous = Vec::with_capacity(self.anchors.len());
    for index in 0..self.anchors.len() {
      // A binary search over the piles: their count's logarithm, at most 32 for any u32 index.
      charge!(meter, TABLE_WORK);
      let Some((_, post)) = self.anchors.get(index) else {
        continue;
      };
      let post = *post;
      let pile = self.piles.partition_point(|top| {
        self
          .anchors
          .get(*top as usize)
          .is_some_and(|(_, top_post)| *top_post < post)
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
    // The run's length, then its gaps laid from the last anchor back: last gap at the end.
    let mut length = 0usize;
    let mut at = self.piles.last().copied().unwrap_or(u32::MAX);
    while self.anchors.get(at as usize).is_some() {
      charge!(meter, 1);
      length = length.saturating_add(1);
      at = self.previous.get(at as usize).copied().unwrap_or(u32::MAX);
    }
    // Laid last first, at exactly their count; searched from the end of the list, so in order.
    self.gaps = Vec::with_capacity(length.saturating_add(1));
    let (mut b1, mut p1) = (n, m);
    let mut at = self.piles.last().copied().unwrap_or(u32::MAX);
    while let Some((b, p)) = self.anchors.get(at as usize).copied() {
      charge!(meter, 1);
      let (b, p) = (b as usize, p as usize);
      self.gaps.push(Region {
        b0: b.saturating_add(1),
        b1,
        p0: p.saturating_add(1),
        p1,
      });
      (b1, p1) = (b, p);
      at = self.previous.get(at as usize).copied().unwrap_or(u32::MAX);
    }
    self.gaps.push(Region {
      b0: 0,
      b1,
      p0: 0,
      p1,
    });
  }

  /// Myers' linear-space search over the units, depth first and left first so the ranges come out in order.
  async fn search(
    &mut self,
    texts: &Texts<'_>,
    budget: u64,
    done: &mut Refined,
    meter: &mut Meter<'_>,
  ) {
    while let Some(mut r) = self.stack.pop() {
      let front = r;
      let at_front = |i: usize| {
        let (b, p) = (front.b0.saturating_add(i), front.p0.saturating_add(i));
        (b < front.b1 && p < front.p1).then_some((b, p))
      };
      let matched = run_sync_first(texts, (&mut done.steps, &mut *meter), at_front).await;
      r.b0 = r.b0.saturating_add(matched);
      r.p0 = r.p0.saturating_add(matched);
      let back = r;
      let at_back = |i: usize| {
        let (b, p) = (
          back.b1.checked_sub(i.saturating_add(1))?,
          back.p1.checked_sub(i.saturating_add(1))?,
        );
        (b >= back.b0 && p >= back.p0).then_some((b, p))
      };
      let matched = run_sync_first(texts, (&mut done.steps, &mut *meter), at_back).await;
      r.b1 = r.b1.saturating_sub(matched);
      r.p1 = r.p1.saturating_sub(matched);
      let lens = (r.b1.saturating_sub(r.b0), r.p1.saturating_sub(r.p0));
      if lens == (0, 0) {
        continue;
      }
      let split = if lens.0 == 0 || lens.1 == 0 {
        None
      } else {
        self.middle_snake(texts, r, budget, (done, meter)).await
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
  async fn middle_snake(
    &mut self,
    texts: &Texts<'_>,
    r: Region,
    budget: u64,
    (done, meter): (&mut Refined, &mut Meter<'_>),
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
    // Zeroed a unit's work at a time: the diagonals grow with the budget's square root, past a unit on a large file.
    if self.forward.capacity() < width {
      self.forward = Vec::with_capacity(width);
      self.backward = Vec::with_capacity(width);
    }
    self.forward.clear();
    self.backward.clear();
    let slice = usize::try_from(WORK_PER_TICK / 2)
      .unwrap_or(usize::MAX)
      .max(1);
    while self.forward.len() < width {
      let len = self.forward.len().saturating_add(slice).min(width);
      let added = len.saturating_sub(self.forward.len());
      self.forward.resize(len, 0);
      self.backward.resize(len, 0);
      charge!(meter, added.saturating_mul(2) as u64);
    }
    let snake = Snake {
      n,
      m,
      delta,
      odd,
      offset,
    };
    for d in 0..=signed(rounds) {
      if let Some(at) = round(
        (&mut self.forward, &self.backward),
        texts,
        (r, snake, d, false),
        (&mut done.steps, &mut *meter),
      )
      .await
      {
        return Some(at);
      }
      if let Some(at) = round(
        (&mut self.backward, &self.forward),
        texts,
        (r, snake, d, true),
        (&mut done.steps, &mut *meter),
      )
      .await
      {
        return Some(at);
      }
      if done.steps > budget {
        return None;
      }
    }
    None
  }
}

/// `buffer` made `len` zeros, written in order a unit's room at a time (its pages first touched in order, so their
/// faults spread a few to a unit rather than following a later random write).
async fn zeroed<T: Copy + Default>(buffer: &mut Vec<T>, len: usize, meter: &mut Meter<'_>) {
  if buffer.capacity() < len {
    *buffer = Vec::with_capacity(len);
  }
  buffer.clear();
  while buffer.len() < len {
    let end = buffer
      .len()
      .saturating_add(usize::try_from(meter.room()).unwrap_or(usize::MAX))
      .min(len);
    let added = end.saturating_sub(buffer.len());
    buffer.resize(end, T::default());
    charge!(meter, added as u64);
  }
}

/// Shape: the bits of one radix digit for a sort of fewer than 2^16 words: three passes cover a [`Tagged`] word's 32-bit
/// hash over a counts table of 2^11 (8 KiB).
const SMALL_DIGIT_BITS: u32 = 11;
/// Shape: the bits of one radix digit from 2^16 words on: two passes over a table of 2^16 (256 KiB), at most half the
/// words' own 512 KiB, for one pass fewer.
const LARGE_DIGIT_BITS: u32 = 16;

/// `entries` sorted by their hash (a [`Tagged`] word's high 32 bits), stably: three passes of a least-significant-digit
/// radix sort over 11-bit digits, through `spare` (as long as `entries`) and `digits` (the counts, 2,048 of them, cleared
/// and summed as one charge each pass). Each element counted or moved is a unit of work, so a sort of millions spans turns, and no step is
/// longer than a unit's work (A-787). A heapsort in place was measured and rejected: 12 times slower on a 1 MiB rewrite.
async fn radix_sort_by_hash(
  entries: &mut Vec<Tagged>,
  (spare, digits): (&mut Vec<Tagged>, &mut Vec<u32>),
  meter: &mut Meter<'_>,
) {
  let len = entries.len();
  // Laid and written a unit's room at a time, in order: written first by the scatter, a fresh buffer's pages would
  // fault in the scatter's random order, thousands in one unit.
  zeroed(spare, len, meter).await;
  let digit_bits = if len >= 1 << LARGE_DIGIT_BITS {
    LARGE_DIGIT_BITS
  } else {
    SMALL_DIGIT_BITS
  };
  let buckets = 1usize << digit_bits;
  if digits.len() < buckets {
    *digits = vec![0; buckets];
  }
  let mask = (buckets as u64).saturating_sub(1);
  let room = |meter: &Meter<'_>| usize::try_from(meter.room()).unwrap_or(usize::MAX);
  let mut shift = 32u32;
  while shift < 64 {
    let digit = |word: u64| usize::try_from((word >> shift) & mask).unwrap_or(0);
    let mut start = 0usize;
    while start < buckets {
      let end = start.saturating_add(room(meter)).min(buckets);
      for count in digits.get_mut(start..end).unwrap_or_default() {
        *count = 0;
      }
      charge!(meter, end.saturating_sub(start) as u64);
      start = end;
    }
    let mut at = 0usize;
    while at < len {
      let end = at.saturating_add(room(meter)).min(len);
      for word in entries.get(at..end).unwrap_or_default() {
        if let Some(count) = digits.get_mut(digit(*word)) {
          *count = count.saturating_add(1);
        }
      }
      charge!(meter, end.saturating_sub(at) as u64);
      at = end;
    }
    let mut sum = 0u32;
    let mut start = 0usize;
    while start < buckets {
      let end = start.saturating_add(room(meter)).min(buckets);
      for count in digits.get_mut(start..end).unwrap_or_default() {
        let here = *count;
        *count = sum;
        sum = sum.saturating_add(here);
      }
      charge!(meter, end.saturating_sub(start) as u64);
      start = end;
    }
    let mut at = 0usize;
    while at < len {
      let end = at.saturating_add(room(meter)).min(len);
      for word in entries.get(at..end).unwrap_or_default() {
        if let Some(next) = digits.get_mut(digit(*word)) {
          if let Some(slot) = spare.get_mut(*next as usize) {
            *slot = *word;
          }
          *next = next.saturating_add(1);
        }
      }
      charge!(meter, end.saturating_sub(at) as u64);
      at = end;
    }
    core::mem::swap(entries, spare);
    // The swap moved the spare's length into `entries`: keep exactly the sorted words.
    entries.truncate(len);
    shift = shift.saturating_add(digit_bits);
  }
}

/// The differing middle of `base` and `post` at `grain`: the common prefix and suffix trimmed byte by byte, then
/// widened out to unit boundaries both texts share. Returns the middle's start and its end in each. Compared a unit's
/// work at a time.
async fn trimmed(
  grain: Grain,
  base: &[u8],
  post: &[u8],
  meter: &mut Meter<'_>,
) -> (usize, usize, usize) {
  let slice = usize::try_from(WORK_PER_TICK).unwrap_or(usize::MAX).max(1);
  let shorter = base.len().min(post.len());
  let mut prefix = 0usize;
  while prefix < shorter {
    let end = prefix.saturating_add(slice).min(shorter);
    let (a, z) = (
      base.get(prefix..end).unwrap_or_default(),
      post.get(prefix..end).unwrap_or_default(),
    );
    charge!(meter, a.len() as u64);
    let same = a.iter().zip(z).take_while(|(x, y)| x == y).count();
    prefix = prefix.saturating_add(same);
    if same < a.len() {
      break;
    }
  }
  let room = shorter.saturating_sub(prefix);
  let mut suffix = 0usize;
  while suffix < room {
    let take = slice.min(room.saturating_sub(suffix));
    let (a_end, z_end) = (
      base.len().saturating_sub(suffix),
      post.len().saturating_sub(suffix),
    );
    let a = base
      .get(a_end.saturating_sub(take)..a_end)
      .unwrap_or_default();
    let z = post
      .get(z_end.saturating_sub(take)..z_end)
      .unwrap_or_default();
    charge!(meter, a.len() as u64);
    let same = a
      .iter()
      .rev()
      .zip(z.iter().rev())
      .take_while(|(x, y)| x == y)
      .count();
    suffix = suffix.saturating_add(same);
    if same < a.len() {
      break;
    }
  }
  let mut start = prefix;
  // The bytes before `start` are equal in both, so a boundary there is one both share only when each text has one.
  while start > 0 && !(boundary(grain, base, start) && boundary(grain, post, start)) {
    charge!(meter, 1);
    start = start.saturating_sub(1);
  }
  let mut base_end = base.len().saturating_sub(suffix);
  let mut post_end = post.len().saturating_sub(suffix);
  while base_end < base.len()
    && post_end < post.len()
    && !(boundary(grain, base, base_end) && boundary(grain, post, post_end))
  {
    charge!(meter, 1);
    base_end = base_end.saturating_add(1);
    post_end = post_end.saturating_add(1);
  }
  (start, base_end.max(start), post_end.max(start))
}

/// The canonical net ops of `hunks` over a base of `base_len` bytes and a writer's file of `post_len`, as
/// [`crate::derive`] composes a declared edit: an equal-length replacement is an `Overwrite`, an unequal one a
/// `Delete` then an `Insert`, an addition at the base end an `Extend`, a removal running to both ends a `Truncate`.
/// Each content-adding op's `src` is the offset of its bytes in the writer's file.
pub fn ops_of<'h>(
  hunks: impl IntoIterator<Item = &'h Hunk>,
  (base_len, post_len): (u64, u64),
  path: u64,
  out: &mut Vec<Op>,
) {
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
  /// The most work done between two asks for a unit by the refinements ([`WORK_PER_TICK`]).
  pub longest_unit_work: u64,
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
/// flags keep theirs (a Subversion commit judges a node whole, A-542). Runs to its end in one call: see
/// [`refine_rewrites_paced`] for the sliced form.
pub fn refine_rewrites<'f>(
  doc: &mut OpsDoc,
  plan: &mut PostStatePlan,
  refiner: &mut Refiner,
  files: &mut dyn FnMut(&str) -> Option<BaseAndPost<'f>>,
) -> Rewrites {
  let ticks = crate::pace::Ticks;
  crate::pace::finish(refine_rewrites_paced(doc, plan, refiner, (files, &ticks)))
}

/// The whole-file replacements of `doc` with their path's ops: `(path index, base length, laid at, post length)`.
async fn replacements(
  doc: &OpsDoc,
  meter: &mut Meter<'_>,
) -> Vec<(usize, usize, u64, (u64, u64, u64))> {
  let mut found = Vec::new();
  let mut start = 0;
  while start < doc.ops.len() {
    let path = doc.ops.get(start).map_or(0, |op| op.path);
    let mut end = start;
    while doc.ops.get(end).is_some_and(|op| op.path == path) {
      charge!(meter, 1);
      end = end.saturating_add(1);
    }
    if let Some(shape) = whole_replacement(doc.ops.get(start..end).unwrap_or_default()) {
      found.push((start, end, path, shape));
    }
    start = end;
  }
  found
}

/// The paths `doc` replaces whole, the ones [`refine_rewrites`] refines when it has their bytes (A-776): what a work's
/// shard asks its green's owner for the base of, besides the files it does not hold (C1, A-247). One walk of the
/// document's ops, as its composition's own walks are.
pub fn whole_replaced_paths(doc: &OpsDoc) -> Vec<String> {
  let mut found = Vec::new();
  let mut start = 0;
  while start < doc.ops.len() {
    let path = doc.ops.get(start).map_or(0, |op| op.path);
    let mut end = start;
    while doc.ops.get(end).is_some_and(|op| op.path == path) {
      end = end.saturating_add(1);
    }
    if whole_replacement(doc.ops.get(start..end).unwrap_or_default()).is_some()
      && let Some(name) = doc.paths.path(path)
    {
      found.push(name.to_owned());
    }
    start = end;
  }
  found
}

/// [`refine_rewrites`] a unit at a time under `ticks` (module docs, "Sliced"): each replacement's refinement, the
/// relaid post-state's sort and the document's canonical order are paced; the same document and plan whatever the
/// pace. The document's operations are walked once to find the replacements before any is refined.
pub async fn refine_rewrites_paced<'f>(
  doc: &mut OpsDoc,
  plan: &mut PostStatePlan,
  refiner: &mut Refiner,
  (files, ticks): (
    &mut dyn FnMut(&str) -> Option<BaseAndPost<'f>>,
    &crate::pace::Ticks,
  ),
) -> Rewrites {
  let mut done = Rewrites::default();
  let mut meter = Meter::new(ticks);
  let found = replacements(doc, &mut meter).await;
  // Each refined path's ops, by its first op's index.
  let mut refined_at: Vec<(usize, usize, Vec<(Op, Option<u64>)>)> = Vec::new();
  for (start, end, path, (base_len, laid_at, post_len)) in found {
    let Some(name) = doc.paths.path(path) else {
      continue;
    };
    let Some(at_in_file) = plan.get(&laid_at).copied() else {
      continue;
    };
    let Some((base, post)) = files(name) else {
      continue;
    };
    if base.len() as u64 != base_len || post.len() as u64 != post_len || at_in_file != 0 {
      continue;
    }
    let cost = refiner
      .refine_metered((&base, &post), (Grain::Token, None), &mut meter)
      .await;
    done.steps = done.steps.saturating_add(cost.steps);
    done.kept_whole = done.kept_whole.saturating_add(cost.kept_whole);
    done.paths = done.paths.saturating_add(1);
    done.declared_bytes = done.declared_bytes.saturating_add(base_len);
    // Each refined op's bytes sat inside the replacement's run, at its offset in the writer's file. A hunk a unit.
    // A range composes into two ops at most, so this is laid once at its bound.
    let mut with_old: Vec<(Op, Option<u64>)> =
      Vec::with_capacity(refiner.hunks().len().saturating_mul(2));
    let mut ops = Vec::with_capacity(2);
    for chunk in refiner.hunks().chunks() {
      for hunk in chunk {
        done.refined_bytes = done.refined_bytes.saturating_add(hunk.base_len);
        ops.clear();
        ops_of([hunk], (base_len, post_len), path, &mut ops);
        with_old.extend(
          ops
            .iter()
            .map(|op| (*op, laid(op).then(|| laid_at.saturating_add(op.src)))),
        );
      }
      // A chunk holds a unit's work of ranges at most.
      charge!(meter, chunk.len() as u64);
    }
    refined_at.push((start, end, with_old));
  }
  done.longest_unit_work = meter.longest();
  if done.paths == 0 {
    return done;
  }
  // Each op, with where its bytes sat in the old layout (`None` when not laid), the refined paths' ops in place of
  // their replacements.
  let replaced: usize = refined_at
    .iter()
    .map(|(start, end, _)| end.saturating_sub(*start))
    .sum();
  let added: usize = refined_at.iter().map(|(_, _, ops)| ops.len()).sum();
  let mut out: Vec<(Op, Option<u64>)> =
    Vec::with_capacity(doc.ops.len().saturating_sub(replaced).saturating_add(added));
  let mut refined_at = refined_at.into_iter().peekable();
  let mut index = 0;
  while index < doc.ops.len() {
    charge!(meter, 1);
    if let Some((start, end, _)) = refined_at.peek()
      && *start == index
    {
      let end = *end;
      if let Some((_, _, ops)) = refined_at.next() {
        out.extend(ops);
      }
      index = end;
      continue;
    }
    if let Some(op) = doc.ops.get(index) {
      out.push((*op, laid(op).then_some(op.src)));
    }
    index = index.saturating_add(1);
  }
  // Relay the post-state: the laid runs in their old order, each at the next offset; a content op's bytes still come
  // from its offset in the writer's file (the old plan's, or the refined op's own `src` there).
  let mut laid_ops: Vec<(u64, usize)> = Vec::with_capacity(out.len());
  for (index, (_, old)) in out.iter().enumerate() {
    charge!(meter, 1);
    if let Some(old) = old {
      laid_ops.push((*old, index));
    }
  }
  let order = crate::pace::sort_by(laid_ops, ticks, |a, b| a.0.cmp(&b.0)).await;
  let mut relaid = PostStatePlan::new();
  let mut cursor = 0u64;
  for (old, index) in order {
    charge!(meter, 1);
    let Some((op, _)) = out.get_mut(index) else {
      continue;
    };
    if matches!(op.kind, OpKind::Overwrite | OpKind::Insert | OpKind::Extend) {
      // An unrefined op's file offset is the old plan's; a refined op carries it in `src` and its old position is
      // `laid_at + src`, which the old plan does not name.
      let in_file = plan.get(&old).copied().unwrap_or(op.src);
      relaid.insert(cursor, in_file);
    }
    op.src = cursor;
    cursor = cursor.saturating_add(op.len);
  }
  doc.ops = Vec::with_capacity(out.len());
  for (op, _) in out {
    charge!(meter, 1);
    doc.ops.push(op);
  }
  doc.canonicalize_paced(ticks).await;
  *plan = relaid;
  done.longest_unit_work = meter.longest();
  done
}

#[cfg(test)]
mod tests;
