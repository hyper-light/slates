//! A growable array that never moves what it holds: it grows a segment at a time, so a push allocates at most one
//! segment and copies nothing, and it shrinks a segment at a time, so a pop frees at most one. A `Vec` that doubles
//! copies its whole length now and then, in one call; on a tree built a unit at a time (§4.3, bounded work) that call
//! is one unit whose cost follows the tree: 271 µs at 16,384 nodes (`image_bench`, least of five rounds, Apple M5 Max,
//! 2026-10-24), against a daemon's slice of 23.7 µs. The shape is `slates_mem::Segmented`'s (§4.2, "Growth without
//! realloc" [A: Brodnik, Carlsson, Demaine, Munro, Sedgewick 1999]), kept here because this crate depends on nothing.

/// Shape: the elements per segment, as a power of two so an index splits into a segment and an offset by shift and
/// mask: 1,024 keeps a segment of the largest element this crate holds (a node, 96 bytes) at 96 KiB, the most one push
/// allocates, while the list of segments stays a thousandth of the elements long.
const SEGMENT_SHIFT: u32 = 10;
/// Derived: the elements per segment, `1 << SEGMENT_SHIFT`.
const SEGMENT_LEN: usize = 1 << SEGMENT_SHIFT;
/// Derived: the offset bits of an index, `SEGMENT_LEN - 1`.
const SEGMENT_MASK: usize = SEGMENT_LEN - 1;

/// A growable array whose elements never move once pushed (see the module doc).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segments<T> {
  /// Every segment full but the last, which holds at least one element: the structure follows the length alone, so
  /// two arrays of equal elements are equal.
  segments: Vec<Vec<T>>,
  len: usize,
}

impl<T> Default for Segments<T> {
  fn default() -> Self {
    Self::new()
  }
}

impl<T> FromIterator<T> for Segments<T> {
  fn from_iter<I: IntoIterator<Item = T>>(items: I) -> Self {
    let mut array = Segments::new();
    for item in items {
      array.push(item);
    }
    array
  }
}

impl<T> Segments<T> {
  /// An empty array; nothing is allocated until the first push.
  pub const fn new() -> Self {
    Segments {
      segments: Vec::new(),
      len: 0,
    }
  }

  /// The elements held.
  pub const fn len(&self) -> usize {
    self.len
  }

  /// Whether no element is held.
  pub const fn is_empty(&self) -> bool {
    self.len == 0
  }

  /// Appends `value`, allocating a segment when the last is full.
  pub fn push(&mut self, value: T) {
    let room = self
      .segments
      .last()
      .is_some_and(|last| last.len() < SEGMENT_LEN);
    if room {
      if let Some(last) = self.segments.last_mut() {
        last.push(value);
      }
    } else {
      let mut segment = Vec::with_capacity(SEGMENT_LEN);
      segment.push(value);
      self.segments.push(segment);
    }
    self.len = self.len.saturating_add(1);
  }

  /// Removes the last element, freeing its segment once that is empty.
  pub fn pop(&mut self) -> Option<T> {
    let last = self.segments.last_mut()?;
    let value = last.pop();
    if last.is_empty() {
      self.segments.pop();
    }
    if value.is_some() {
      self.len = self.len.saturating_sub(1);
    }
    value
  }

  /// Releases one piece of the array and says whether there was one: the last whole segment at once when the elements
  /// own nothing to free (`needs_drop` is false: dropping them is nothing, so the segment's one free is the whole cost),
  /// else the last element. Either way a call frees one segment at most and drops one element that owns memory at most,
  /// so an array of any length is released in bounded calls, and an array of plain values in a thousandth as many.
  pub fn shed(&mut self) -> bool {
    if std::mem::needs_drop::<T>() {
      return self.pop().is_some();
    }
    match self.segments.pop() {
      Some(last) => {
        self.len = self.len.saturating_sub(last.len());
        true
      }
      None => false,
    }
  }

  /// The element at `index`.
  pub fn get(&self, index: usize) -> Option<&T> {
    let (segment, offset) = split(index);
    self.segments.get(segment)?.get(offset)
  }

  /// The element at `index`, mutably.
  pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
    let (segment, offset) = split(index);
    self.segments.get_mut(segment)?.get_mut(offset)
  }

  /// The last element.
  pub fn last(&self) -> Option<&T> {
    self.segments.last()?.last()
  }

  /// The last element, mutably.
  pub fn last_mut(&mut self) -> Option<&mut T> {
    self.segments.last_mut()?.last_mut()
  }

  /// The elements in index order.
  pub fn iter(&self) -> impl Iterator<Item = &T> {
    self.segments.iter().flatten()
  }
}

/// The segment and offset of element `index`.
const fn split(index: usize) -> (usize, usize) {
  let segment = match index.checked_shr(SEGMENT_SHIFT) {
    Some(segment) => segment,
    None => 0,
  };
  (segment, index & SEGMENT_MASK)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Do: push past several segments, read every element, pop back to empty. Expect each element where it was pushed,
  /// every pop the last element, and an emptied array equal to a fresh one.
  #[test]
  fn elements_stay_where_they_were_pushed_and_pop_in_reverse() {
    let count = SEGMENT_LEN * 3 + 7;
    let mut array: Segments<usize> = (0..count).collect();
    assert_eq!(array.len(), count);
    assert!((0..count).all(|index| array.get(index) == Some(&index)));
    assert_eq!(array.get(count), None);
    assert!(array.iter().copied().eq(0..count));
    assert!(
      (0..count)
        .rev()
        .all(|expected| array.pop() == Some(expected))
    );
    assert_eq!(array.pop(), None);
    assert_eq!(array, Segments::new());
  }

  /// Do: shed an array of plain values and one of owning values, each past several segments. Expect the plain one gone
  /// a segment a call and the owning one an element a call, each ending equal to a fresh array.
  #[test]
  fn plain_values_shed_a_segment_a_call_and_owning_ones_an_element() {
    let count = SEGMENT_LEN * 3 + 7;
    let mut plain: Segments<usize> = (0..count).collect();
    let mut owning: Segments<Vec<u8>> = (0..count).map(|value| vec![0u8; value % 7]).collect();
    let mut calls = 0;
    while plain.shed() {
      calls += 1;
    }
    assert_eq!(calls, count.div_ceil(SEGMENT_LEN));
    assert_eq!(plain, Segments::new());
    calls = 0;
    while owning.shed() {
      calls += 1;
    }
    assert_eq!(calls, count);
    assert_eq!(owning, Segments::new());
  }
}
