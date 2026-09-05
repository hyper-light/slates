//! The local run queue: a fixed-capacity list of ready slot indices with one pending flag per
//! slot, so a task woken many times between polls appears once and the list never grows past
//! the arena (§4.3, "intrusive run queue"; no allocation on the wake path).
//!
//! Single-threaded by construction: only the owning shard's thread pushes and drains, so the
//! lists sit in `RefCell`s and a re-entrant access is refused (counted), never undefined. The
//! draining swap uses a second pre-sized list so wakes that arrive while ready tasks run (a task
//! waking another) land in the next batch, which bounds one loop iteration's work.

use std::cell::{Cell, RefCell};

/// The queue.
pub struct LocalQueue {
  pending: Box<[Cell<bool>]>,
  ready: RefCell<Vec<u32>>,
  draining: RefCell<Vec<u32>>,
  overflow: Cell<u64>,
  refused: Cell<u64>,
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
      ready: RefCell::new(Vec::with_capacity(capacity)),
      draining: RefCell::new(Vec::with_capacity(capacity)),
      overflow: Cell::new(0),
      refused: Cell::new(0),
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
    match self.ready.try_borrow_mut() {
      Ok(mut ready) => ready.push(slot),
      Err(_) => {
        flag.set(false);
        self.refused.set(self.refused.get() + 1);
      }
    }
  }

  /// Takes the ready list for one iteration; the caller iterates the returned list and calls
  /// `finish_drain` afterwards so the buffer returns for reuse.
  pub fn take_ready(&self) -> Vec<u32> {
    if self.ready.try_borrow().is_ok_and(|r| r.is_empty()) {
      return Vec::new();
    }
    match (self.ready.try_borrow_mut(), self.draining.try_borrow_mut()) {
      (Ok(mut ready), Ok(mut draining)) => {
        std::mem::swap(&mut *ready, &mut *draining);
        std::mem::take(&mut *draining)
      }
      _ => {
        self.refused.set(self.refused.get() + 1);
        Vec::new()
      }
    }
  }

  /// Returns the list taken by `take_ready`, cleared, so the next drain allocates nothing.
  pub fn finish_drain(&self, mut list: Vec<u32>) {
    if list.capacity() == 0 {
      // The empty fast path of `take_ready` handed out no buffer; keep the pre-sized one.
      return;
    }
    list.clear();
    if let Ok(mut draining) = self.draining.try_borrow_mut() {
      *draining = list;
    }
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
    self.ready.try_borrow().is_ok_and(|r| r.is_empty())
  }

  /// Ready entries.
  pub fn len(&self) -> usize {
    self.ready.try_borrow().map_or(0, |r| r.len())
  }

  /// Wakes for slots beyond the arena, ignored.
  pub fn overflow(&self) -> u64 {
    self.overflow.get()
  }

  /// Accesses refused because the list was already borrowed (a bug signal).
  pub fn refused(&self) -> u64 {
    self.refused.get()
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
    assert_eq!(q.refused(), 0);
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
