//! The declared atomic words of a shared memory object's layout (§4.2, §4.7; audit AUD-29-09).
//!
//! Two processes reach a shared object at once, so its bytes fall in two disjoint classes, fixed by the
//! layout and declared when the object is created or opened: **words**, which both sides read and write
//! concurrently and only through atomics of the declared width (a ring's sequence words and hints, a
//! wake word, a heartbeat, a seqlock generation), and **everything else**, which one side owns at a time
//! under the protocol those words carry and which crosses the object boundary only by copy. An atomic
//! view is granted only for a declared word of its exact width, and a copy that touches any declared
//! word is refused, so no safe caller can mix a plain access with an atomic one, or two atomic widths,
//! on the same bytes. Until 2026-09-30 the object handed out a byte slice of the whole map beside atomic
//! views of words inside it, with only a comment asking callers not to alias them.
//!
//! A third class serves a seqlock's payload, which a reader copies while a writer may be rewriting it and
//! then validates: **racy bytes**, declared as a run of 8-bit words and copied only byte by byte through
//! `AtomicU8` on both sides ([`Width::U8`]; `SharedObject::read_racy`), so the race the seqlock tolerates
//! is between atomics of one width — never a plain access, and never a mixed-size one.
//!
//! A run is `count` words of one width, the first at `first`, each `stride` bytes after the one before:
//! a single word is a run of one; a ring's per-slot sequence words are one strided run; a racy span is a
//! run of 8-bit words with stride one. Every question
//! here is arithmetic on runs — no per-word table — so a layout of thousands of slots is checked in
//! constant time per question.

use crate::error::{LayoutRefusal, MemError};

/// The width of a declared atomic word.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Width {
  /// A racy byte (`AtomicU8`): a seqlock payload's bytes.
  U8,
  /// A 32-bit word (`AtomicU32`).
  U32,
  /// A 64-bit word (`AtomicU64`).
  U64,
}

impl Width {
  /// The word's bytes (also its alignment).
  pub const fn bytes(self) -> usize {
    match self {
      Width::U8 => size_of::<u8>(),
      Width::U32 => size_of::<u32>(),
      Width::U64 => size_of::<u64>(),
    }
  }
}

/// A run of declared words: `count` words of `width`, the first at `first`, each `stride` bytes after the
/// one before.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WordRun {
  first: usize,
  stride: usize,
  count: usize,
  width: Width,
}

impl WordRun {
  /// One word of `width` at `offset`.
  pub const fn one(offset: usize, width: Width) -> WordRun {
    WordRun {
      first: offset,
      stride: width.bytes(),
      count: 1,
      width,
    }
  }

  /// `len` racy bytes at `offset` (a seqlock payload's span).
  pub const fn racy(offset: usize, len: usize) -> WordRun {
    WordRun {
      first: offset,
      stride: 1,
      count: len,
      width: Width::U8,
    }
  }

  /// `count` words of `width`, the first at `first`, each `stride` bytes after the one before.
  pub const fn strided(first: usize, stride: usize, count: usize, width: Width) -> WordRun {
    WordRun {
      first,
      stride,
      count,
      width,
    }
  }

  /// One past the last byte of the last word, or `None` if the run does not fit the address space.
  fn end(&self) -> Option<usize> {
    self
      .count
      .checked_sub(1)?
      .checked_mul(self.stride)?
      .checked_add(self.first)?
      .checked_add(self.width.bytes())
  }

  /// `value / stride` and whether it divides exactly: a shift and a mask for a power-of-two stride (every
  /// layout here: 8, 64, 128), a division otherwise.
  fn index_of(&self, value: usize) -> (usize, bool) {
    if self.stride.is_power_of_two() {
      let shift = self.stride.trailing_zeros();
      (value >> shift, value & (self.stride - 1) == 0)
    } else {
      (value / self.stride, value.is_multiple_of(self.stride))
    }
  }

  /// The run's first byte.
  pub const fn first_offset(&self) -> usize {
    self.first
  }

  /// The offset of word `index` of this run, if the run has that word and it is `width` wide.
  pub fn word(&self, index: usize, width: Width) -> Option<usize> {
    if width != self.width || index >= self.count {
      return None;
    }
    index.checked_mul(self.stride)?.checked_add(self.first)
  }

  /// Whether a word of this run is exactly `width` at `offset`.
  fn holds(&self, offset: usize, width: Width) -> bool {
    if width != self.width || offset < self.first || self.stride == 0 {
      return false;
    }
    let (index, exact) = self.index_of(offset.saturating_sub(self.first));
    exact && index < self.count
  }

  /// Whether any word of this run shares a byte with `[offset, offset + len)`.
  fn touches(&self, offset: usize, len: usize) -> bool {
    let (Some(end), Some(run_end)) = (offset.checked_add(len), self.end()) else {
      return true;
    };
    if len == 0 || end <= self.first || run_end <= offset || self.stride == 0 {
      return false;
    }
    // The first word whose end lies past `offset`: word i ends at first + i·stride + width.
    let width = self.width.bytes();
    let first_after = match offset.checked_sub(self.first.saturating_add(width).saturating_sub(1)) {
      None | Some(0) => 0,
      Some(past) => {
        let (index, exact) = self.index_of(past);
        if exact {
          index
        } else {
          index.saturating_add(1)
        }
      }
    };
    first_after < self.count
      && first_after
        .checked_mul(self.stride)
        .and_then(|at| at.checked_add(self.first))
        .is_some_and(|start| start < end)
  }

  /// The layout refusal of this run inside an object of `len` bytes, if any: every word inside the object
  /// and aligned to its width, words not overlapping one another.
  fn refusal(&self, len: usize) -> Option<LayoutRefusal> {
    let width = self.width.bytes();
    if self.count == 0 || self.stride < width {
      return Some(LayoutRefusal::Overlap);
    }
    if !self.first.is_multiple_of(width) || !self.stride.is_multiple_of(width) {
      return Some(LayoutRefusal::Misaligned);
    }
    match self.end() {
      Some(end) if end <= len => None,
      _ => Some(LayoutRefusal::OutOfRange),
    }
  }

  /// Whether this run and `other` share a byte: disjoint bounding ranges first (constant time), then each
  /// word of the shorter run against the longer one.
  fn overlaps(&self, other: &WordRun) -> bool {
    let (Some(end), Some(other_end)) = (self.end(), other.end()) else {
      return true;
    };
    if end <= other.first || other_end <= self.first {
      return false;
    }
    let (short, long) = if self.count <= other.count {
      (self, other)
    } else {
      (other, self)
    };
    (0..short.count).any(|index| {
      index
        .checked_mul(short.stride)
        .and_then(|at| at.checked_add(short.first))
        .is_none_or(|at| long.touches(at, short.width.bytes()))
    })
  }
}

/// A run of plain spans: `count` spans of `len` bytes, the first at `first`, each `stride` bytes after the
/// one before — a ring's slot bodies. Declared so a hot path copies a span in constant time
/// ([`crate::SharedObject::write_span`]): the layout refuses it if any span would touch a declared word or
/// racy byte, so a span copy needs no search.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SpanRun {
  first: usize,
  stride: usize,
  count: usize,
  len: usize,
}

impl SpanRun {
  /// `count` spans of `len` bytes, the first at `first`, each `stride` after the one before.
  pub const fn strided(first: usize, stride: usize, count: usize, len: usize) -> SpanRun {
    SpanRun {
      first,
      stride,
      count,
      len,
    }
  }

  /// The offset of span `index`, if the run has it and a copy of `len` bytes fits in it.
  pub fn span(&self, index: usize, len: usize) -> Option<usize> {
    if index >= self.count || len > self.len {
      return None;
    }
    index.checked_mul(self.stride)?.checked_add(self.first)
  }
}

/// A declared span run of a validated [`Layout`], named by its place there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SpanId(usize);

/// A declared run of a validated [`Layout`], named by its place there: resolved once
/// ([`Layout::resolve`]), it reaches the run's words in constant time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RunId(usize);

/// A shared object's declared atomic words, as its creator or opener states them: the layout's
/// concurrent part. Every other byte is reached by copy. Validated into a [`Layout`] before use.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Words {
  runs: Vec<WordRun>,
  spans: Vec<SpanRun>,
}

impl Words {
  /// No words: an object reached only by copy.
  pub fn new() -> Words {
    Words::default()
  }

  /// These words and `run`.
  #[must_use]
  pub fn with(mut self, run: WordRun) -> Words {
    self.runs.push(run);
    self
  }

  /// These words and the plain span run `span`.
  #[must_use]
  pub fn with_span(mut self, span: SpanRun) -> Words {
    self.spans.push(span);
    self
  }

  /// These words and `other`'s.
  #[must_use]
  pub fn and(mut self, other: Words) -> Words {
    self.runs.extend(other.runs);
    self.spans.extend(other.spans);
    self
  }

  /// The validated, indexed layout inside an object of `len` bytes: every run in range and aligned, and
  /// no two runs sharing a byte (two widths on one byte would be mixed-size atomics).
  pub fn layout(mut self, len: usize) -> Result<Layout, MemError> {
    if let Some((run, reason)) = self
      .runs
      .iter()
      .find_map(|run| run.refusal(len).map(|reason| (run, reason)))
    {
      return Err(refused(run.first, run.width.bytes(), reason));
    }
    self.runs.sort_by_key(|run| run.first);
    let mut reach = Vec::with_capacity(self.runs.len());
    let mut furthest = 0usize;
    for run in &self.runs {
      furthest = furthest.max(run.end().unwrap_or(usize::MAX));
      reach.push(furthest);
    }
    let mut layout = Layout {
      runs: self.runs,
      reach,
      spans: Vec::new(),
    };
    layout.refuse_overlaps()?;
    for span in &self.spans {
      layout.refuse_span(span, len)?;
    }
    layout.spans = self.spans;
    Ok(layout)
  }
}

/// A validated layout: the declared runs sorted by their first byte, with the furthest end reached by any
/// run up to each one, so a question about a span visits only the runs that can reach it (a binary search,
/// then a walk back while a run could still reach the span) — constant work per copy on the IPC and log
/// paths, however many runs the layout declares.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout {
  runs: Vec<WordRun>,
  reach: Vec<usize>,
  spans: Vec<SpanRun>,
}

impl Layout {
  /// Whether any run that can share a byte with `[offset, end)` — starting before `end`, its extent
  /// reaching past `offset` — satisfies `test`: a binary search, then a walk back while a run could reach.
  fn any_candidate(&self, offset: usize, end: usize, test: impl Fn(&WordRun) -> bool) -> bool {
    let mut index = self.runs.partition_point(|run| run.first < end);
    while let Some(previous) = index.checked_sub(1) {
      index = previous;
      match (self.runs.get(index), self.reach.get(index)) {
        (Some(run), Some(reach)) if *reach > offset => {
          if test(run) {
            return true;
          }
        }
        _ => return false,
      }
    }
    false
  }

  /// Refuses a span run with a span past the object or touching a declared word or racy byte.
  fn refuse_span(&self, span: &SpanRun, len: usize) -> Result<(), MemError> {
    for index in 0..span.count {
      let Some(at) = span.span(index, span.len) else {
        return Err(refused(span.first, span.len, LayoutRefusal::OutOfRange));
      };
      if at.checked_add(span.len).is_none_or(|end| end > len) {
        return Err(refused(at, span.len, LayoutRefusal::OutOfRange));
      }
      if self.touches(at, span.len) {
        return Err(refused(at, span.len, LayoutRefusal::TouchesWord));
      }
    }
    Ok(())
  }

  /// The span run `id` names, if this layout has it.
  pub fn span(&self, id: SpanId) -> Option<&SpanRun> {
    self.spans.get(id.0)
  }

  /// The id of the declared span run equal to `span`.
  pub fn resolve_span(&self, span: &SpanRun) -> Option<SpanId> {
    self
      .spans
      .iter()
      .position(|declared| declared == span)
      .map(SpanId)
  }

  /// The run `id` names, if this layout has it.
  pub fn run(&self, id: RunId) -> Option<&WordRun> {
    self.runs.get(id.0)
  }

  /// The id of the declared run equal to `run`, resolved once so a hot path reaches its words in constant
  /// time ([`WordRun::word`]).
  pub fn resolve(&self, run: &WordRun) -> Option<RunId> {
    let at = self
      .runs
      .partition_point(|declared| declared.first < run.first);
    (self.runs.get(at) == Some(run)).then_some(RunId(at))
  }

  /// Refuses two runs sharing a byte: each run against the earlier runs whose extent reaches it.
  fn refuse_overlaps(&self) -> Result<(), MemError> {
    for (index, run) in self.runs.iter().enumerate() {
      let earlier = Layout {
        runs: self.runs.get(..index).unwrap_or_default().to_vec(),
        reach: self.reach.get(..index).unwrap_or_default().to_vec(),
        spans: Vec::new(),
      };
      let end = run.end().unwrap_or(usize::MAX);
      if earlier.any_candidate(run.first, end, |other| run.overlaps(other)) {
        return Err(refused(
          run.first,
          run.width.bytes(),
          LayoutRefusal::Overlap,
        ));
      }
    }
    Ok(())
  }

  /// Whether a declared word is exactly `width` at `offset`.
  pub fn holds(&self, offset: usize, width: Width) -> bool {
    let end = offset.saturating_add(width.bytes());
    self.any_candidate(offset, end, |run| run.holds(offset, width))
  }

  /// Whether `[offset, offset + len)` shares a byte with any declared word.
  pub fn touches(&self, offset: usize, len: usize) -> bool {
    let end = offset.saturating_add(len);
    self.any_candidate(offset, end, |run| run.touches(offset, len))
  }

  /// Whether `[offset, offset + len)` lies wholly inside one declared racy span.
  pub fn racy_covers(&self, offset: usize, len: usize) -> bool {
    let Some(end) = offset.checked_add(len) else {
      return false;
    };
    self.any_candidate(offset, end.max(offset.saturating_add(1)), |run| {
      run.width == Width::U8
        && run.stride == 1
        && run.first <= offset
        && run.end().is_some_and(|run_end| end <= run_end)
    })
  }
}

/// The layout refusal at `offset` for `len` bytes.
pub(crate) fn refused(offset: usize, len: usize, reason: LayoutRefusal) -> MemError {
  MemError::LayoutRefused {
    offset,
    len,
    reason,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Shape: a ring as the IPC layout declares it: two hints on their own lines, then eight slots of 64
  /// bytes with the sequence word first.
  /// Format: the ring layout's object length: the hints' lines and eight slots.
  const RING_LEN: usize = 128 + 8 * 64;

  fn ring() -> Words {
    Words::new()
      .with(WordRun::one(0, Width::U64))
      .with(WordRun::one(64, Width::U64))
      .with(WordRun::strided(128, 64, 8, Width::U64))
  }

  /// A byte-by-byte model of `touches`: the oracle the arithmetic must equal.
  fn touches_model(runs: &[WordRun], offset: usize, len: usize) -> bool {
    (offset..offset + len).any(|byte| {
      runs.iter().any(|run| {
        (0..run.count).any(|index| {
          let start = run.first + index * run.stride;
          (start..start + run.width.bytes()).contains(&byte)
        })
      })
    })
  }

  /// AUD-29-09. Do: ask whether every range up to 72 bytes at every offset of a ring layout touches a
  /// word. Expect: the arithmetic equals the byte-by-byte model on all of them.
  #[test]
  fn touches_equals_the_byte_model_on_every_range() {
    let runs = [
      WordRun::one(0, Width::U64),
      WordRun::one(64, Width::U64),
      WordRun::strided(128, 64, 8, Width::U64),
    ];
    let words = ring().layout(RING_LEN).unwrap();
    for offset in 0..(128 + 8 * 64 + 16) {
      for len in 0..72 {
        assert_eq!(
          words.touches(offset, len),
          touches_model(&runs, offset, len),
          "offset {offset} len {len}"
        );
      }
    }
  }

  /// AUD-29-09. Do: ask for atomic words of the ring. Expect: each declared word of its width holds; a
  /// word's other width, a slot's body, and a word past the last slot do not.
  #[test]
  fn only_a_declared_word_of_its_width_holds() {
    let words = ring().layout(RING_LEN).unwrap();
    assert!(words.holds(0, Width::U64) && words.holds(64, Width::U64));
    assert!(words.holds(128, Width::U64) && words.holds(128 + 7 * 64, Width::U64));
    assert!(!words.holds(128, Width::U32), "the other width");
    assert!(!words.holds(136, Width::U64), "a slot's body");
    assert!(!words.holds(128 + 8 * 64, Width::U64), "past the last slot");
  }

  /// AUD-29-09. Do: declare a racy span beside a word and ask about copies. Expect: a racy copy inside
  /// the span is covered; one straddling its edge or the word is not; a plain copy of it touches.
  #[test]
  fn a_racy_span_covers_only_its_own_bytes() {
    let words = Words::new()
      .with(WordRun::one(0, Width::U64))
      .with(WordRun::racy(8, 56))
      .layout(64)
      .unwrap();
    assert!(words.racy_covers(8, 56) && words.racy_covers(20, 4));
    assert!(!words.racy_covers(4, 8), "straddles the word");
    assert!(!words.racy_covers(60, 8), "past the span");
    assert!(
      words.touches(20, 4),
      "a plain copy of racy bytes is refused"
    );
    assert!(
      !words.holds(8, Width::U64),
      "no wider atomic over racy bytes"
    );
  }

  /// AUD-29-09. Do: check layouts that overlap, misalign or overrun. Expect: each refused by name; the
  /// ring inside its object passes.
  #[test]
  fn a_layout_that_would_mix_accesses_is_refused_by_name() {
    let len = RING_LEN;
    assert!(ring().layout(len).is_ok());
    let reason = |words: Words| match words.layout(len) {
      Err(MemError::LayoutRefused { reason, .. }) => Some(reason),
      _ => None,
    };
    assert_eq!(
      reason(ring().with(WordRun::one(132, Width::U32))),
      Some(LayoutRefusal::Overlap),
      "a 32-bit word inside a 64-bit one"
    );
    assert_eq!(
      reason(Words::new().with(WordRun::one(4, Width::U64))),
      Some(LayoutRefusal::Misaligned)
    );
    assert_eq!(
      reason(Words::new().with(WordRun::strided(128, 64, 9, Width::U64))),
      Some(LayoutRefusal::OutOfRange)
    );
    assert_eq!(
      reason(Words::new().with(WordRun::strided(0, 4, 2, Width::U64))),
      Some(LayoutRefusal::Overlap),
      "a stride shorter than the word"
    );
  }
}
