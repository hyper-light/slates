//! Byte ranges and the per-path set of ranges an increment touches (§4.16 "Position mapping",
//! "The verdict"). A range is a half-open `[start, start + len)`; a `RangeSet` is the ranges one
//! path's declared operations cover, kept sorted and non-overlapping so the verdict's sweep line
//! is a linear merge.

/// A half-open byte range `[start, start + len)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
  /// The first byte.
  pub start: u64,
  /// The length in bytes (a zero-length range is an anchor point, e.g. an insert position).
  pub len: u64,
}

impl Range {
  /// A range from `start` of `len` bytes.
  pub fn new(start: u64, len: u64) -> Range {
    Range { start, len }
  }

  /// The byte past the end.
  pub fn end(self) -> u64 {
    self.start.saturating_add(self.len)
  }

  /// Whether this range and `other` share any byte. Two zero-length ranges overlap only when
  /// they sit at the same position (two inserts at one point); a zero-length range and a
  /// non-empty one overlap when the point lies strictly inside the non-empty one (an insert at
  /// the very edge of an overwrite does not conflict, §4.16).
  pub fn overlaps(self, other: Range) -> bool {
    if self.len == 0 && other.len == 0 {
      return self.start == other.start;
    }
    if self.len == 0 {
      return self.start > other.start && self.start < other.end();
    }
    if other.len == 0 {
      return other.start > self.start && other.start < self.end();
    }
    self.start < other.end() && other.start < self.end()
  }

  /// Whether this range and `other` are the same span (a same-range candidate for pass two).
  pub fn same_span(self, other: Range) -> bool {
    self.start == other.start && self.len == other.len
  }
}

/// The ranges one path's operations cover, sorted by start and non-overlapping within the set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RangeSet {
  ranges: Vec<Range>,
}

impl RangeSet {
  /// An empty set.
  pub fn new() -> RangeSet {
    RangeSet { ranges: Vec::new() }
  }

  /// A set from ranges, sorted by start (the caller's operations are already non-overlapping
  /// within one path after the deriver composes them, §4.16).
  pub fn from_ranges(mut ranges: Vec<Range>) -> RangeSet {
    ranges.sort_by(|a, b| a.start.cmp(&b.start).then(a.len.cmp(&b.len)));
    RangeSet { ranges }
  }

  /// The ranges, sorted.
  pub fn ranges(&self) -> &[Range] {
    &self.ranges
  }

  /// Whether the set is empty.
  pub fn is_empty(&self) -> bool {
    self.ranges.is_empty()
  }
}
