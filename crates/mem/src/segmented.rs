//! A segmented array: grows by appending a segment, never by moving what it holds, so a
//! reference or index handed out stays valid until the element is removed (§4.2, "Growth without
//! realloc"; [A: Brodnik, Carlsson, Demaine, Munro, Sedgewick 1999]).
//!
//! Segments are fixed-length and power-of-two sized, so an index splits into a segment number
//! and an offset by shift and mask. Appending a segment is the only allocation and is a cold
//! path: a shard pre-allocates reserve segments in idle time ([`Segmented::reserve_segments`]) so
//! that `push` on the hot path never reaches the system allocator (AC-0.4).

/// A growable array with stable element addresses.
#[derive(Debug)]
pub struct Segmented<T> {
  segments: Vec<Vec<T>>,
  reserve: Vec<Vec<T>>,
  segment_len: usize,
  shift: u32,
  len: usize,
}

impl<T> Segmented<T> {
  /// A new array whose segments hold `segment_len` elements (rounded up to a power of two, at
  /// least one), with no segments allocated yet.
  pub fn new(segment_len: usize) -> Self {
    let segment_len = segment_len.max(1).next_power_of_two();
    Self {
      segments: Vec::new(),
      reserve: Vec::new(),
      segment_len,
      shift: segment_len.trailing_zeros(),
      len: 0,
    }
  }

  /// Elements per segment.
  pub const fn segment_len(&self) -> usize {
    self.segment_len
  }

  /// Number of elements.
  pub const fn len(&self) -> usize {
    self.len
  }

  /// Whether the array is empty.
  pub const fn is_empty(&self) -> bool {
    self.len == 0
  }

  /// Elements the array can hold before it needs another segment.
  pub fn capacity(&self) -> usize {
    self.segments.len().saturating_mul(self.segment_len)
  }

  /// Pre-allocates `count` segments for later growth (the cold path, run in idle time).
  pub fn reserve_segments(&mut self, count: usize) {
    self.segments.reserve(count);
    self.reserve.reserve(count);
    for _ in 0..count {
      self.reserve.push(Vec::with_capacity(self.segment_len));
    }
  }

  /// Segments held in reserve.
  pub fn reserved_segments(&self) -> usize {
    self.reserve.len()
  }

  /// Appends an element. Takes a reserve segment when the last one is full; allocates one only
  /// when the reserve is empty (`allocated` reports which, so a hot path can count it).
  pub fn push(&mut self, value: T) -> usize {
    let (_, index) = self.push_reporting(value);
    index
  }

  /// Appends an element and reports whether a fresh segment had to be allocated.
  pub fn push_reporting(&mut self, value: T) -> (bool, usize) {
    let mut allocated = false;
    if self.len == self.capacity() {
      let segment = self.reserve.pop().unwrap_or_else(|| {
        allocated = true;
        Vec::with_capacity(self.segment_len)
      });
      self.segments.push(segment);
    }
    let index = self.len;
    let (seg, _) = self.split(index);
    // `seg` names the last segment, which has room: capacity == len only when a segment was
    // just appended above.
    if let Some(segment) = self.segments.get_mut(seg) {
      segment.push(value);
    }
    self.len += 1;
    (allocated, index)
  }

  /// Removes and returns the last element.
  pub fn pop(&mut self) -> Option<T> {
    if self.len == 0 {
      return None;
    }
    let (seg, _) = self.split(self.len - 1);
    let value = self.segments.get_mut(seg).and_then(Vec::pop);
    if value.is_some() {
      self.len -= 1;
      if self.segments.last().is_some_and(Vec::is_empty)
        && self.segments.len() > 1
        && let Some(empty) = self.segments.pop()
      {
        self.reserve.push(empty);
      }
    }
    value
  }

  /// The element at `index`.
  pub fn get(&self, index: usize) -> Option<&T> {
    if index >= self.len {
      return None;
    }
    let (seg, off) = self.split(index);
    self.segments.get(seg).and_then(|s| s.get(off))
  }

  /// The element at `index`, mutably.
  pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
    if index >= self.len {
      return None;
    }
    let (seg, off) = self.split(index);
    self.segments.get_mut(seg).and_then(|s| s.get_mut(off))
  }

  /// Iterates the elements in index order.
  pub fn iter(&self) -> impl Iterator<Item = &T> {
    self.segments.iter().flat_map(|s| s.iter())
  }

  const fn split(&self, index: usize) -> (usize, usize) {
    (index >> self.shift, index & (self.segment_len - 1))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn elements_keep_their_addresses_across_growth() {
    let mut s: Segmented<u64> = Segmented::new(4);
    s.push(10);
    let first = std::ptr::from_ref(s.get(0).unwrap());
    for i in 1..100 {
      s.push(i);
    }
    assert_eq!(std::ptr::from_ref(s.get(0).unwrap()), first);
    assert_eq!(s.len(), 100);
    assert_eq!(s.get(99), Some(&99));
    assert_eq!(s.get(100), None);
    assert_eq!(s.capacity(), 100);
  }

  #[test]
  fn reserve_segments_make_push_allocation_free() {
    let mut s: Segmented<u32> = Segmented::new(8);
    s.reserve_segments(2);
    assert_eq!(s.reserved_segments(), 2);
    let mut allocated = 0;
    for i in 0..16 {
      if s.push_reporting(i).0 {
        allocated += 1;
      }
    }
    assert_eq!(allocated, 0);
    assert_eq!(s.reserved_segments(), 0);
    assert!(
      s.push_reporting(16).0,
      "the third segment had to be allocated"
    );
  }

  #[test]
  fn pop_returns_empty_segments_to_the_reserve() {
    let mut s: Segmented<u8> = Segmented::new(2);
    for i in 0..5 {
      s.push(i);
    }
    assert_eq!(s.pop(), Some(4));
    assert_eq!(s.reserved_segments(), 1);
    assert_eq!(s.pop(), Some(3));
    assert_eq!(s.len(), 3);
    assert_eq!(s.iter().copied().collect::<Vec<_>>(), vec![0, 1, 2]);
    let mut e: Segmented<u8> = Segmented::new(2);
    assert_eq!(e.pop(), None);
  }

  #[test]
  fn segment_length_rounds_to_a_power_of_two() {
    let s: Segmented<u8> = Segmented::new(5);
    assert_eq!(s.segment_len(), 8);
    let z: Segmented<u8> = Segmented::new(0);
    assert_eq!(z.segment_len(), 1);
    assert!(z.is_empty());
  }
}
