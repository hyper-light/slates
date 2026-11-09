//! The local run queue: a fixed-capacity list of ready slot indices with one pending flag per
//! slot, so a task woken many times between polls appears once and the list never grows past
//! the arena (§4.3, "intrusive run queue"; no allocation on the wake path).
//!
//! Single-threaded by construction: only the owning shard's thread pushes and drains, so the
//! lists sit in `RefCell`s and a re-entrant access is refused (counted), never undefined. A drain
//! takes at most one batch from the front of the ready queue into a second pre-sized list, so a step
//! costs its batch, never the whole ready set, and the tasks left behind keep their places (strict
//! FIFO); wakes that arrive while the batch runs (a task waking another) queue behind them. Until
//! 2026-09-26 a drain took the whole ready set and re-queued all but a batch, so a step cost
//! O(ready) and draining N ready tasks cost O(N² / batch): `observe.rs`'s full-arena fill of 82,245
//! yielding tasks took 95–97 s.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

/// The queue.
pub struct LocalQueue {
  pending: Box<[Cell<bool>]>,
  ready: RefCell<VecDeque<u32>>,
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
      ready: RefCell::new(VecDeque::with_capacity(capacity)),
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
      self.overflow.set(self.overflow.get().saturating_add(1));
      return;
    };
    if flag.replace(true) {
      return;
    }
    match self.ready.try_borrow_mut() {
      Ok(mut ready) => ready.push_back(slot),
      Err(_) => {
        flag.set(false);
        self.refused.set(self.refused.get().saturating_add(1));
      }
    }
  }

  /// Takes at most `limit` ready slots, the oldest first, for one iteration; the rest stay queued in
  /// order with their pending flags set, so a wake for one of them is still collapsed. The caller
  /// polls the returned list and calls `finish_drain` afterwards so the buffer returns for reuse.
  pub fn take_ready(&self, limit: usize) -> Vec<u32> {
    if self.ready.try_borrow().is_ok_and(|r| r.is_empty()) {
      return Vec::new();
    }
    match (self.ready.try_borrow_mut(), self.draining.try_borrow_mut()) {
      (Ok(mut ready), Ok(mut draining)) => {
        let take = limit.min(ready.len());
        draining.extend(ready.drain(..take));
        std::mem::take(&mut *draining)
      }
      _ => {
        self.refused.set(self.refused.get().saturating_add(1));
        Vec::new()
      }
    }
  }

  /// Takes `slot` alone when it is ready, leaving every other ready slot queued in its place: the drain of a shard a
  /// task holds alone ([`crate::futures::spawn_alone`]). Empty when `slot` is not ready. Its pending flag stays set
  /// until its poll begins, as with [`Self::take_ready`]; finding it costs the ready list's length, which a shard held
  /// alone keeps to what arrived meanwhile.
  pub fn take_slot(&self, slot: u32) -> Vec<u32> {
    let pending = self
      .pending
      .get(usize::try_from(slot).unwrap_or(usize::MAX))
      .is_some_and(Cell::get);
    if !pending {
      return Vec::new();
    }
    match (self.ready.try_borrow_mut(), self.draining.try_borrow_mut()) {
      (Ok(mut ready), Ok(mut draining)) => {
        if let Some(at) = ready.iter().position(|queued| *queued == slot) {
          let _ = ready.remove(at);
          draining.push(slot);
        }
        std::mem::take(&mut *draining)
      }
      _ => {
        self.refused.set(self.refused.get().saturating_add(1));
        Vec::new()
      }
    }
  }

  /// Puts slots a drain took and did not poll back at the front of the ready list, in their order, with their pending
  /// flags still set: the rest of a batch when a task polled in it began to hold the shard alone.
  pub fn put_back(&self, slots: &[u32]) {
    match self.ready.try_borrow_mut() {
      Ok(mut ready) => {
        for slot in slots.iter().rev() {
          ready.push_front(*slot);
        }
      }
      Err(_) => {
        // The slots stay pending with no place in the list, so a wake would not list them: each is a lost task,
        // counted as every refused access is.
        self.refused.set(self.refused.get().saturating_add(1));
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
    let list = q.take_ready(usize::MAX);
    assert_eq!(list, vec![2, 0, 3]);
    assert!(q.is_empty());
    for slot in &list {
      q.clear_pending(*slot);
    }
    q.finish_drain(list);
    q.push(2);
    assert_eq!(q.take_ready(usize::MAX), vec![2]);
    assert_eq!(q.refused(), 0);
  }

  /// §4.3, a shard held alone (`ShardContext::spawn_alone`). Do: queue four slots, take the third alone, then put
  /// back a batch's tail as a step that began a hold does. Expect: taking a slot takes it alone and leaves the others
  /// in their order; a slot not ready is not taken; slots put back go to the front in their order, still pending, so a
  /// wake for one is collapsed, and nothing is refused.
  #[test]
  fn a_slot_taken_alone_leaves_the_rest_in_order_and_a_put_back_tail_goes_first() {
    let q = LocalQueue::new(8);
    for slot in [4, 5, 6, 7] {
      q.push(slot);
    }
    let alone = q.take_slot(6);
    assert_eq!(alone, vec![6]);
    q.clear_pending(6);
    q.finish_drain(alone);
    assert!(q.take_slot(6).is_empty(), "a slot not ready is not taken");
    let batch = q.take_ready(2);
    assert_eq!(batch, vec![4, 5]);
    q.put_back(batch.get(1..).unwrap_or_default());
    q.push(5);
    q.finish_drain(batch);
    assert_eq!(
      q.take_ready(8),
      vec![5, 7],
      "the put-back slot first, its wake collapsed"
    );
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
    let batch = q.take_ready(usize::MAX);
    q.clear_pending(1);
    q.push(1);
    q.finish_drain(batch);
    assert_eq!(q.take_ready(usize::MAX), vec![1]);
  }

  /// §4.3 bounded work: a drain hands out at most its batch, oldest first; the slots left behind keep
  /// their places ahead of any wake that arrives while the batch runs, and a wake for one of them is
  /// still collapsed. Do: queue five, take two, wake one of the left behind and a new one. Expect:
  /// the first two, then the other three in order, then the new one.
  #[test]
  fn a_drain_takes_at_most_its_batch_and_the_rest_keep_their_places() {
    let q = LocalQueue::new(8);
    for slot in 0..5 {
      q.push(slot);
    }
    let first = q.take_ready(2);
    assert_eq!(first, vec![0, 1]);
    assert_eq!(q.len(), 3, "the other three wait, not re-queued");
    for slot in &first {
      q.clear_pending(*slot);
    }
    q.push(3);
    q.push(7);
    q.finish_drain(first);
    assert_eq!(
      q.take_ready(8),
      vec![2, 3, 4, 7],
      "FIFO; the repeated wake collapsed"
    );
    assert_eq!(q.refused(), 0);
  }
}
