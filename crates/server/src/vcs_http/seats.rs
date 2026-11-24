//! O(1) connection seat table with O(log K) overdue-reap selection and strict-FIFO wake-one waiters (§4.3, §4.17).
//!
//! Shared by the HTTP (`crate::vcs_http`), SSH (`crate::vcs_ssh`), and `svn://` (`crate::vcs_svn`) edges so
//! 10,000+ concurrent connections incur zero O(N) seat-array scans and zero thundering-herd waiter storms, with
//! zero locks, zero `Arc`, and zero `unsafe`.

use std::collections::{BTreeMap, BTreeSet};
use std::task::Waker;

use slates_rt::TaskId;

/// An overdue mark held by a taken seat: exposes the deadline timestamp it passed so the earliest deadline passed
/// is always chosen first when reaping.
pub(crate) trait OverdueMark: Copy {
  /// The deadline this wait passed, in nanoseconds on the shard's clock.
  fn since_ns(self) -> u64;
}

impl OverdueMark for u64 {
  fn since_ns(self) -> u64 {
    self
  }
}

/// A taken seat: its connection's task once spawned, whether its wait on its client is past its bound, and whether
/// it is already being reaped.
#[derive(Clone, Copy, Debug)]
struct Taken<O> {
  task: Option<TaskId>,
  overdue: Option<O>,
  reaping: bool,
}

/// How a connection waiting for a seat stands ([`SeatTable::seating`]).
pub(crate) enum Seating {
  /// This seat has been atomically claimed for the caller.
  Free(usize),
  /// Every seat is taken and this connection's task, the most overdue not yet being reaped, has been chosen to be
  /// reaped for the caller (none when it was never spawned, whose seat comes back as its spawn is refused).
  Reap(Option<TaskId>),
  /// Every seat is taken and none is overdue: the caller's waker is queued in FIFO order.
  Wait,
}

/// O(1) free-list seat table with an ordered overdue index and strict-FIFO wake-one wait queue.
/// Grows `slots` lazily up to `capacity` so idle shards retain zero heap memory regardless of seat ceiling.
pub(crate) struct SeatTable<O> {
  /// Maximum simultaneous seats permitted on this shard.
  capacity: usize,
  /// Each allocated seat's live occupant, indexed by slot (`slots.len() <= capacity`).
  slots: Vec<Option<Taken<O>>>,
  /// Free previously-allocated seat indices, popped/pushed in O(1).
  free: Vec<usize>,
  /// Overdue non-reaping seats ordered by `(since_ns, slot)` so `pop_first()` finds the most overdue in O(log K).
  overdue: BTreeSet<(u64, usize)>,
  /// Strict-FIFO waiters keyed by monotonic waiter id; waking pops one live waiter (`pop_first()`), and dropping
  /// a cancelled wait removes its entry and passes the baton if a seat or overdue target is available.
  waiting: BTreeMap<u64, Waker>,
  /// Next waiter id.
  next_waiter: u64,
  /// Live occupied seats.
  serving: usize,
}

impl<O: OverdueMark> SeatTable<O> {
  /// Creates a table permitting up to `capacity` concurrent seats, allocating zero heap slots until used.
  pub(crate) fn new(capacity: usize) -> Self {
    Self {
      capacity,
      slots: Vec::new(),
      free: Vec::new(),
      overdue: BTreeSet::new(),
      waiting: BTreeMap::new(),
      next_waiter: 0,
      serving: 0,
    }
  }

  /// Number of seats currently taken, in O(1).
  pub(crate) fn serving(&self) -> usize {
    self.serving
  }

  /// Allocates a unique monotonic waiter id for one wait operation.
  pub(crate) fn allocate_waiter(&mut self) -> u64 {
    let id = self.next_waiter;
    self.next_waiter = self.next_waiter.wrapping_add(1);
    id
  }

  /// Claims a free seat immediately in O(1), if one is available.
  pub(crate) fn claim_free(&mut self) -> Option<usize> {
    let taken = Some(Taken {
      task: None,
      overdue: None,
      reaping: false,
    });
    if let Some(slot) = self.free.pop() {
      if let Some(entry) = self.slots.get_mut(slot) {
        *entry = taken;
        self.serving = self.serving.saturating_add(1);
      }
      return Some(slot);
    }
    if self.slots.len() < self.capacity {
      let slot = self.slots.len();
      self.slots.push(taken);
      self.serving = self.serving.saturating_add(1);
      return Some(slot);
    }
    None
  }

  /// Whether at least one seat is immediately free.
  fn has_free(&self) -> bool {
    !self.free.is_empty() || self.slots.len() < self.capacity
  }

  /// Claims a free seat, picks the most overdue seat to reap, or queues `(waiter, waker)` to wait.
  pub(crate) fn seating(&mut self, waiter: u64, waker: &Waker) -> (Seating, Option<O>) {
    if let Some(slot) = self.claim_free() {
      self.waiting.remove(&waiter);
      return (Seating::Free(slot), None);
    }
    self.waiting.insert(waiter, waker.clone());
    while let Some((_, slot)) = self.overdue.pop_first() {
      if let Some(Some(taken)) = self.slots.get_mut(slot)
        && !taken.reaping
        && let Some(mark) = taken.overdue.take()
      {
        taken.reaping = true;
        return (Seating::Reap(taken.task), Some(mark));
      }
    }
    (Seating::Wait, None)
  }

  /// Removes `waiter` on drop/cancellation and returns the next waiter to wake if a free or overdue seat could
  /// otherwise go unnoticed because `waiter` had already been popped before cancellation.
  pub(crate) fn cancel_waiter(&mut self, waiter: u64) -> Option<Waker> {
    self.waiting.remove(&waiter);
    if self.has_free() || !self.overdue.is_empty() {
      return self.waiting.pop_first().map(|(_, waker)| waker);
    }
    None
  }

  /// Records the task serving claimed seat `slot`.
  pub(crate) fn set_task(&mut self, slot: usize, task: TaskId) {
    if let Some(Some(taken)) = self.slots.get_mut(slot) {
      taken.task = Some(task);
    }
  }

  /// Marks `slot` overdue (`Some(mark)`) or clears its mark (`None`). Returns `(updated, wake)` where `wake` is
  /// one waiting task to wake outside the borrow when `slot` became overdue.
  pub(crate) fn mark(&mut self, slot: usize, mark: Option<O>) -> (bool, Option<Waker>) {
    let Some(Some(taken)) = self.slots.get_mut(slot) else {
      return (false, None);
    };
    if taken.reaping {
      return (false, None);
    }
    if let Some(old) = taken.overdue.take() {
      self.overdue.remove(&(old.since_ns(), slot));
    }
    taken.overdue = mark;
    if let Some(new_mark) = mark {
      self.overdue.insert((new_mark.since_ns(), slot));
      return (true, self.waiting.pop_first().map(|(_, waker)| waker));
    }
    (true, None)
  }

  /// Gives claimed seat `slot` back in O(1) and returns one waiting task to wake outside the borrow.
  pub(crate) fn give_back(&mut self, slot: usize) -> Option<Waker> {
    let taken = self.slots.get_mut(slot).and_then(Option::take)?;
    self.serving = self.serving.saturating_sub(1);
    if let Some(old) = taken.overdue {
      self.overdue.remove(&(old.since_ns(), slot));
    }
    self.free.push(slot);
    self.waiting.pop_first().map(|(_, waker)| waker)
  }
}
