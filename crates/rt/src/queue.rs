//! The local run queue: a fixed-capacity list of ready slot indices with one pending flag per
//! slot, so a task woken many times between polls appears once and the list never grows past
//! the arena (§4.3, "intrusive run queue"; no allocation on the wake path).
//!
//! Single-threaded by construction: only the owning shard's thread pushes and drains. The
//! draining swap uses a second pre-sized list so wakes that arrive while ready tasks run (a task
//! waking another) land in the next batch, which bounds one loop iteration's work.

use std::cell::{Cell, UnsafeCell};

/// The queue.
pub struct LocalQueue {
  pending: Box<[Cell<bool>]>,
  ready: UnsafeCell<Vec<u32>>,
  draining: UnsafeCell<Vec<u32>>,
  overflow: Cell<u64>,
}

impl std::fmt::Debug for LocalQueue {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("LocalQueue")
      .field("capacity", &self.pending.len())
      .finish()
  }
}

impl LocalQueue {
  /// A queue for an arena of `capacity` slots.
  pub fn new(capacity: usize) -> Self {
    Self {
      pending: (0..capacity).map(|_| Cell::new(false)).collect(),
      ready: UnsafeCell::new(Vec::with_capacity(capacity)),
      draining: UnsafeCell::new(Vec::with_capacity(capacity)),
      overflow: Cell::new(0),
    }
  }

  /// Marks a slot ready; a slot already pending is not listed twice. A slot beyond the arena is
  /// counted as overflow and ignored (a stale or foreign word).
  pub fn push(&self, slot: u32) {
    let Some(flag) = self
      .pending
      .get(usize::try_from(slot).unwrap_or(usize::MAX))
    else {
      self.overflow.set(self.overflow.get() + 1);
      return;
    };
    if flag.replace(true) {
      return;
    }
    // SAFETY: single-threaded by construction; no reference into `ready` is live across a push
    // (the drain swaps the vector out before iterating).
    unsafe { (*self.ready.get()).push(slot) };
  }

  /// Takes the ready list for one iteration; the caller iterates the returned slice and calls
  /// `finish_drain` afterwards so the buffer returns for reuse.
  pub fn take_ready(&self) -> Vec<u32> {
    // SAFETY: single-threaded; the two vectors are swapped, not borrowed across calls.
    unsafe {
      let ready = &mut *self.ready.get();
      let draining = &mut *self.draining.get();
      std::mem::swap(ready, draining);
      std::mem::take(draining)
    }
  }

  /// Returns the list taken by `take_ready`, cleared, so the next drain allocates nothing.
  pub fn finish_drain(&self, mut list: Vec<u32>) {
    list.clear();
    // SAFETY: single-threaded; `draining` is empty (taken) and receives the buffer back.
    unsafe { *self.draining.get() = list };
  }

  /// Clears a slot's pending flag when its poll begins, so a wake during the poll re-queues it.
  pub fn clear_pending(&self, slot: u32) {
    if let Some(flag) = self
      .pending
      .get(usize::try_from(slot).unwrap_or(usize::MAX))
    {
      flag.set(false);
    }
  }

  /// Whether nothing is ready.
  pub fn is_empty(&self) -> bool {
    // SAFETY: single-threaded read of the length.
    unsafe { (*self.ready.get()).is_empty() }
  }

  /// Ready entries.
  pub fn len(&self) -> usize {
    // SAFETY: single-threaded read of the length.
    unsafe { (*self.ready.get()).len() }
  }

  /// Wakes for slots beyond the arena, ignored.
  pub fn overflow(&self) -> u64 {
    self.overflow.get()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn duplicates_collapse_and_drain_returns_in_order() {
    let q = LocalQueue::new(4);
    q.push(2);
    q.push(0);
    q.push(2);
    q.push(3);
    assert_eq!(q.len(), 3);
    let list = q.take_ready();
    assert_eq!(list, vec![2, 0, 3]);
    assert!(q.is_empty());
    for slot in &list {
      q.clear_pending(*slot);
    }
    q.finish_drain(list);
    q.push(2);
    assert_eq!(q.take_ready(), vec![2]);
  }

  #[test]
  fn a_slot_beyond_the_arena_is_counted_not_listed() {
    let q = LocalQueue::new(2);
    q.push(9);
    assert_eq!(q.overflow(), 1);
    assert!(q.is_empty());
  }

  #[test]
  fn a_wake_during_a_drain_lands_in_the_next_batch() {
    let q = LocalQueue::new(3);
    q.push(1);
    let batch = q.take_ready();
    q.clear_pending(1);
    q.push(1);
    q.finish_drain(batch);
    assert_eq!(q.take_ready(), vec![1]);
  }
}
