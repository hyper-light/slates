//! The senders a bounded channel holds waiting for room (A-767 piece C2, ported from hyper-raft's `hyper-rt`
//! `sync/room.rs` at `5e40eb9`): one place each, at most the channel's `waiters`, with no lock on any path.
//!
//! **On-demand places in a lock-free FIFO queue.** A channel claims only three cells up front (its control cell, its
//! queue-ends cell, and one sentinel node), never `waiters` cells and never a boxed slice per `Sender`. Cloning or
//! dropping a `Sender` touches two atomics and allocates zero bytes (before this change, `StoreOwner`'s channel of
//! ~123k waiters cloned and freed a ~1 MiB `Box<[CellRef]>` with 123k atomic retain/release ops on every
//! `StoreHandle::clone`, consuming 89.9% of CPU in Mercurial partial clone and 13 GiB of large-mapping churn on push).
//!
//! **FIFO grants without ABA or hazard pointers.** A sender task that finds the channel full reserves a slot in
//! `control.state`'s held count, claims a fresh [`CellRef`] with two handles (one for the sender, one for the queue),
//! and enqueues it at `queue.aux` (the Michael-Scott queue tail). Every cell lives in a `'static` segment that is
//! never deallocated (`super::cell`), and every node's empty-next sentinel and node-state words carry that node's
//! 32-bit `generation` in their upper 32 bits and bit 31 set in their lower 32 bits (disjoint from every cell index
//! `< 2^24` and from every other primitive's `state`/`aux` values). Therefore a stalled compare-and-swap on a freed
//! and reused cell always fails, and a dequeued dummy head is released immediately when the next dequeue promotes its
//! successor.
//!
//! **No lost room.** A waiter increments the waiting count, enqueues itself, and executes a `SeqCst` fence before
//! checking the channel again; a receiver (or a sender handing on an unused grant) publishes free room, executes a
//! `SeqCst` fence, and checks the waiting count: either the sender's check finds the room or the grant finds the
//! sender. When no sender waits (the common case), `grant_one` reads one atomic word after its fence and returns.

use std::sync::atomic::{Ordering, fence};

use slates_mem::Encoded;

use super::cell::{CellRef, MAX_CELLS, claim};
use crate::error::RtError;

/// Format: shift placing a cell's 32-bit generation (or the waiting count) in the high half of a `u64` word.
const HIGH_SHIFT: u32 = 32;
/// Format: low 32-bit mask extracting a cell index, state tag, or held-waiter count from a `u64` word.
const LOW_MASK: u64 = u32::MAX as u64;
/// Format: increment of the waiting count stored in the high 32 bits of `control.state`.
const WAITING_ONE: u64 = 1_u64 << HIGH_SHIFT;

/// Format: low-word tag for an unlinked queue tail (`aux == (gen << 32) | EMPTY_NEXT_TAG`), disjoint from every valid
/// cell index (`< MAX_CELLS = 2^24`).
const EMPTY_NEXT_TAG: u32 = u32::MAX;
/// Format: bit 31 set on every waiter-node `state` tag so waiter states are disjoint from `0`, `RECEIVER_GONE (1)`,
/// packed cell indices (`< 2^24`), and held counts (`<= MAX_CELLS`).
const NODE_STATE_BIT: u32 = 1_u32 << 31;
/// Format: a queued waiter node waiting for room.
const WAITING: u32 = NODE_STATE_BIT | 1;
/// Format: a waiter node that was granted room and has not yet been left by its sender.
const GRANTED: u32 = NODE_STATE_BIT | 2;
/// Format: a waiter node whose sender stopped waiting before a grant reached it.
const CANCELLED: u32 = NODE_STATE_BIT | 3;
/// Format: a waiter node woken because the receiver closed the channel.
const CLOSED: u32 = NODE_STATE_BIT | 4;

/// Format: handles a newly enqueued waiter node starts with (one for the waiting sender, one for the FIFO queue).
const NODE_HANDLES: u32 = 2;

/// Packs `cell` into a `u64` link word: `(generation << 32) | index`.
fn pack(cell: CellRef) -> u64 {
  (u64::from(cell.generation()) << HIGH_SHIFT) | u64::from(cell.index())
}

/// Unpacks a link word into a [`CellRef`], returning `None` for any generation-tagged empty-next sentinel.
fn unpack(bits: u64) -> Option<CellRef> {
  let index = u32::try_from(bits & LOW_MASK).ok()?;
  if usize::try_from(index).is_ok_and(|idx| idx < MAX_CELLS) {
    let generation = u32::try_from(bits >> HIGH_SHIFT).ok()?;
    Some(CellRef::from_parts(index, generation))
  } else {
    None
  }
}

/// Generation-tagged empty-next sentinel for a queue node at `generation`.
fn empty_next(generation: u32) -> u64 {
  (u64::from(generation) << HIGH_SHIFT) | u64::from(EMPTY_NEXT_TAG)
}

/// Generation-tagged node state word for a waiter node at `generation`.
fn node_state(generation: u32, tag: u32) -> u64 {
  (u64::from(generation) << HIGH_SHIFT) | u64::from(tag)
}

/// The senders waiting for room: `control` holds `(waiting_count << 32) | held_count` in its `state`, `queue` holds
/// the lock-free FIFO queue's `head` (`state`) and `tail` (`aux`), and `waiters` is the channel's waiter ceiling.
#[derive(Debug)]
pub(crate) struct Room {
  control: CellRef,
  queue: CellRef,
  waiters: u32,
}

impl Room {
  /// Room for up to `waiters` senders waiting at once, its shared cells held by `ends` handles. Refused `BadConfig`
  /// for none (or past `u32::MAX`), and `Capacity` past the cell table's bound.
  pub(crate) fn new(waiters: usize, ends: u32) -> Result<Self, RtError> {
    if waiters == 0 {
      return Err(RtError::BadConfig {
        what: "a channel with no room for a waiting sender",
      });
    }
    let waiters = u32::try_from(waiters).map_err(|_| RtError::BadConfig {
      what: "a channel with more waiting places than the cell table names",
    })?;
    let control = claim(ends)?;
    let queue = match claim(ends) {
      Ok(queue) => queue,
      Err(refused) => {
        for _ in 0..ends {
          control.release();
        }
        return Err(refused);
      }
    };
    let dummy = match claim(1) {
      Ok(dummy) => dummy,
      Err(refused) => {
        for _ in 0..ends {
          queue.release();
          control.release();
        }
        return Err(refused);
      }
    };
    if let Some(cell) = dummy.cell() {
      cell
        .aux
        .store(empty_next(dummy.generation()), Ordering::Release);
    }
    let dummy_bits = pack(dummy);
    if let Some(q) = queue.cell() {
      q.state.store(dummy_bits, Ordering::Release);
      q.aux.store(dummy_bits, Ordering::Release);
    }
    Ok(Self {
      control,
      queue,
      waiters,
    })
  }

  /// Another end's handles on the same room (two atomics, zero allocations).
  pub(crate) fn share(&self) -> Self {
    self.control.retain();
    self.queue.retain();
    Self {
      control: self.control,
      queue: self.queue,
      waiters: self.waiters,
    }
  }

  /// A second end's copy of the same handles, raising no count: for the other end of a room whose cells were claimed
  /// with every end's handle already ([`Room::new`]'s `ends`).
  pub(crate) fn list_copy(&self) -> Self {
    Self {
      control: self.control,
      queue: self.queue,
      waiters: self.waiters,
    }
  }

  /// Takes a place for a sender about to wait, counted, enqueued, and fenced: the caller's next look at the channel is
  /// ordered after its place shows waiting to every grant. `None` when `waiters` senders already hold places or the
  /// cell table is at its limit.
  pub(crate) fn hold(&self) -> Option<CellRef> {
    let control = self.control.cell()?;
    let limit = u64::from(self.waiters);
    loop {
      let current = control.state.load(Ordering::Acquire);
      let held = current & LOW_MASK;
      if held >= limit {
        return None;
      }
      if control
        .state
        .compare_exchange_weak(
          current,
          current.wrapping_add(1),
          Ordering::AcqRel,
          Ordering::Acquire,
        )
        .is_ok()
      {
        break;
      }
    }
    match self.enqueue_fresh() {
      Some(place) => Some(place),
      None => {
        control.state.fetch_sub(1, Ordering::AcqRel);
        None
      }
    }
  }

  /// The held place `old`, granted, waits again: its room went to a racing sender. Releases the granted cell and
  /// enqueues a fresh cell under the same held slot, counted and fenced as [`Room::hold`]. Returns `None` (releasing
  /// the held slot) only if the process-wide cell table is exhausted.
  pub(crate) fn wait_again(&self, old: CellRef) -> Option<CellRef> {
    old.release();
    match self.enqueue_fresh() {
      Some(place) => Some(place),
      None => {
        if let Some(control) = self.control.cell() {
          control.state.fetch_sub(1, Ordering::AcqRel);
        }
        None
      }
    }
  }

  /// Claims a fresh waiter cell, counts it as waiting, enqueues it in FIFO order, and fences `SeqCst`.
  fn enqueue_fresh(&self) -> Option<CellRef> {
    let place = claim(NODE_HANDLES).ok()?;
    let Some(cell) = place.cell() else {
      place.release();
      place.release();
      return None;
    };
    let generation = place.generation();
    cell
      .state
      .store(node_state(generation, WAITING), Ordering::Release);
    cell.aux.store(empty_next(generation), Ordering::Release);
    if let Some(control) = self.control.cell() {
      // Count before linking into the queue so `waiting_count` never underflows under a racing `grant_one`.
      control.state.fetch_add(WAITING_ONE, Ordering::AcqRel);
    }
    self.enqueue(place);
    fence(Ordering::SeqCst);
    Some(place)
  }

  /// Appends `node` to the lock-free Michael-Scott queue anchored in `self.queue`.
  fn enqueue(&self, node: CellRef) {
    let Some(queue) = self.queue.cell() else {
      return;
    };
    let node_bits = pack(node);
    loop {
      let tail_bits = queue.aux.load(Ordering::Acquire);
      let Some(tail) = unpack(tail_bits) else {
        return;
      };
      let Some(tail_cell) = tail.cell() else {
        continue;
      };
      let next_bits = tail_cell.aux.load(Ordering::Acquire);
      if tail_bits != queue.aux.load(Ordering::Acquire) {
        continue;
      }
      let expected_empty = empty_next(tail.generation());
      if next_bits == expected_empty {
        if tail_cell
          .aux
          .compare_exchange(
            expected_empty,
            node_bits,
            Ordering::AcqRel,
            Ordering::Acquire,
          )
          .is_ok()
        {
          let _ =
            queue
              .aux
              .compare_exchange(tail_bits, node_bits, Ordering::AcqRel, Ordering::Acquire);
          return;
        }
      } else if unpack(next_bits).is_some() {
        let _ =
          queue
            .aux
            .compare_exchange(tail_bits, next_bits, Ordering::AcqRel, Ordering::Acquire);
      }
    }
  }

  /// Dequeues the oldest waiter node from the FIFO queue, releasing the previous dummy head.
  fn dequeue(&self) -> Option<CellRef> {
    let queue = self.queue.cell()?;
    loop {
      let head_bits = queue.state.load(Ordering::Acquire);
      let tail_bits = queue.aux.load(Ordering::Acquire);
      let head = unpack(head_bits)?;
      let Some(head_cell) = head.cell() else {
        continue;
      };
      let next_bits = head_cell.aux.load(Ordering::Acquire);
      if head_bits != queue.state.load(Ordering::Acquire) {
        continue;
      }
      let next = unpack(next_bits)?;
      if head_bits == tail_bits {
        let _ =
          queue
            .aux
            .compare_exchange(tail_bits, next_bits, Ordering::AcqRel, Ordering::Acquire);
        continue;
      }
      if queue
        .state
        .compare_exchange(head_bits, next_bits, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
      {
        head.release();
        return Some(next);
      }
    }
  }

  /// Opportunistically trims leading cancelled tombstones at the front of the queue without dequeuing a live waiter.
  fn sweep_cancelled(&self) {
    let Some(queue) = self.queue.cell() else {
      return;
    };
    loop {
      let head_bits = queue.state.load(Ordering::Acquire);
      let tail_bits = queue.aux.load(Ordering::Acquire);
      let Some(head) = unpack(head_bits) else {
        return;
      };
      let Some(head_cell) = head.cell() else {
        continue;
      };
      let next_bits = head_cell.aux.load(Ordering::Acquire);
      if head_bits != queue.state.load(Ordering::Acquire) {
        continue;
      }
      let Some(next) = unpack(next_bits) else {
        return;
      };
      let is_cancelled = next.cell().is_none_or(|cell| {
        cell.state.load(Ordering::Acquire) == node_state(next.generation(), CANCELLED)
      });
      if !is_cancelled {
        return;
      }
      if head_bits == tail_bits {
        let _ =
          queue
            .aux
            .compare_exchange(tail_bits, next_bits, Ordering::AcqRel, Ordering::Acquire);
        continue;
      }
      if queue
        .state
        .compare_exchange(head_bits, next_bits, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
      {
        head.release();
      }
    }
  }

  /// Whether the held place `place` was granted room.
  pub(crate) fn granted(&self, place: CellRef) -> bool {
    place.cell().is_some_and(|cell| {
      cell.state.load(Ordering::Acquire) == node_state(place.generation(), GRANTED)
    })
  }

  /// Registers the task `word` as the held place's waiter (fenced, [`super::cell::Cell::register`]).
  pub(crate) fn register_task(&self, place: CellRef, word: Encoded) {
    place.register(word);
  }

  /// A sender lets its place go. Room granted to the place goes on to another waiting sender unless the sender
  /// `used` it: saw the grant, then sent. A grant the sender did not see may be for room it did not take (a grant
  /// can reach a place whose sender has just sent on room made earlier), and keeping it would leave another sender
  /// waiting with room free and no grant coming (upstream's loom model found it).
  pub(crate) fn leave(&self, place: CellRef, used: bool) {
    if self.release_place(place) && !used {
      self.grant_one();
    }
  }

  /// Lets the held place `place` go, and says whether it had been granted room.
  fn release_place(&self, place: CellRef) -> bool {
    let generation = place.generation();
    let waiting = node_state(generation, WAITING);
    let cancelled = node_state(generation, CANCELLED);
    let granted = node_state(generation, GRANTED);
    let was_granted = match place.cell() {
      Some(cell) => {
        match cell
          .state
          .compare_exchange(waiting, cancelled, Ordering::AcqRel, Ordering::Acquire)
        {
          Ok(_) => {
            if let Some(control) = self.control.cell() {
              control
                .state
                .fetch_sub(WAITING_ONE.wrapping_add(1), Ordering::AcqRel);
            }
            false
          }
          Err(seen) => {
            if let Some(control) = self.control.cell() {
              control.state.fetch_sub(1, Ordering::AcqRel);
            }
            seen == granted
          }
        }
      }
      None => {
        if let Some(control) = self.control.cell() {
          control.state.fetch_sub(1, Ordering::AcqRel);
        }
        false
      }
    };
    place.release();
    if !was_granted {
      self.sweep_cancelled();
    }
    was_granted
  }

  /// Grants room to the oldest waiting sender and wakes it, if one waits; for whoever just made room, after the room
  /// is published. Whether one was granted.
  pub(crate) fn grant_one(&self) -> bool {
    fence(Ordering::SeqCst);
    let Some(control) = self.control.cell() else {
      return false;
    };
    if (control.state.load(Ordering::Acquire) >> HIGH_SHIFT) == 0 {
      return false;
    }
    while let Some(place) = self.dequeue() {
      let generation = place.generation();
      if let Some(cell) = place.cell()
        && cell
          .state
          .compare_exchange(
            node_state(generation, WAITING),
            node_state(generation, GRANTED),
            Ordering::AcqRel,
            Ordering::Acquire,
          )
          .is_ok()
      {
        control.state.fetch_sub(WAITING_ONE, Ordering::AcqRel);
        place.wake();
        return true;
      }
    }
    false
  }

  /// Wakes every waiting sender to see the channel closed (the receiver went, after saying so).
  pub(crate) fn wake_all(&self) {
    fence(Ordering::SeqCst);
    let control = self.control.cell();
    while let Some(place) = self.dequeue() {
      let generation = place.generation();
      if let Some(cell) = place.cell()
        && cell
          .state
          .compare_exchange(
            node_state(generation, WAITING),
            node_state(generation, CLOSED),
            Ordering::AcqRel,
            Ordering::Acquire,
          )
          .is_ok()
      {
        if let Some(control) = control {
          control.state.fetch_sub(WAITING_ONE, Ordering::AcqRel);
        }
        place.wake();
      }
    }
  }
}

impl Drop for Room {
  fn drop(&mut self) {
    if self.control.release_last()
      && let Some(queue) = self.queue.cell()
    {
      // Last end: no concurrent sender or receiver remains, so drain every node still linked from `head` (including
      // the dummy head) and release its queue handle before freeing `self.queue`.
      let mut cursor = queue.state.load(Ordering::Acquire);
      while let Some(node) = unpack(cursor) {
        let next = node
          .cell()
          .map_or(empty_next(0), |cell| cell.aux.load(Ordering::Acquire));
        node.release();
        cursor = next;
      }
    }
    self.queue.release();
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Two holders of `room` granted in FIFO order: the first, then the other, then no one waits.
  fn grants_go_round(room: &Room, (first, second): (CellRef, CellRef)) {
    assert!(room.grant_one());
    assert!(room.granted(first) && !room.granted(second));
    assert!(room.grant_one(), "the next grant goes to the other holder");
    assert!(room.granted(second));
    assert!(!room.grant_one(), "no one waits now");
  }

  /// Do: hold both places of a two-place room, grant, wait again, let go. Expect: a full room refuses, grants go
  /// in FIFO order, and both places are free again after.
  #[test]
  fn places_are_held_exactly_and_granted_round_the_room() {
    let room = Room::new(2, 1).unwrap();
    assert!(!room.grant_one(), "no one waits");
    let first = room.hold().unwrap();
    let second = room.hold().unwrap();
    assert_eq!(room.hold(), None, "two places, two holders");
    grants_go_round(&room, (first, second));
    let first = room.wait_again(first).unwrap();
    assert!(!room.granted(first));
    assert!(!room.release_place(first), "let go without a grant");
    assert!(room.release_place(second), "let go after its grant");
    let a = room.hold().unwrap();
    let b = room.hold().unwrap();
    room.leave(a, false);
    room.leave(b, false);
    assert!(Room::new(0, 1).is_err());
  }

  /// Do: make a room shared by two ends, hold and cancel/grant places, then drop both ends. Expect: every cell it
  /// claimed (control, queue, dummy, and waiter nodes) is given back.
  #[test]
  fn a_room_gives_its_cells_back_with_its_last_end() {
    let room = Room::new(3, 1).unwrap();
    let dummy = unpack(room.queue.cell().unwrap().state.load(Ordering::Acquire)).unwrap();
    let place = room.hold().unwrap();
    let cells = [room.control, room.queue, dummy, place];
    let other = room.share();
    room.leave(place, false);
    drop(room);
    assert!(
      other.control.cell().is_some() && other.queue.cell().is_some(),
      "the other end still holds the room"
    );
    drop(other);
    assert!(
      cells.iter().all(|cell| cell.cell().is_none()),
      "the last end gave every cell back"
    );
  }

  /// Do: four threads concurrently share, hold, wait_again, grant_one, and leave on one room. Expect: zero deadlocks,
  /// zero leaked cells, and every cell returned when the last end drops.
  #[test]
  fn concurrent_hold_grant_leave_and_share_returns_every_cell() {
    let room = Room::new(32, 1).unwrap();
    let control = room.control;
    let queue = room.queue;
    std::thread::scope(|scope| {
      for _ in 0..4 {
        let shared = room.share();
        scope.spawn(move || {
          for step in 0..500_u32 {
            let extra = shared.share();
            if let Some(mut place) = extra.hold() {
              if step % 3 == 0 {
                let _ = extra.grant_one();
              }
              if extra.granted(place) && step % 2 == 0 {
                if let Some(again) = extra.wait_again(place) {
                  place = again;
                } else {
                  continue;
                }
              }
              let used = extra.granted(place);
              extra.leave(place, used);
            } else {
              let _ = extra.grant_one();
            }
          }
        });
      }
      for _ in 0..1_000 {
        let _ = room.grant_one();
      }
    });
    drop(room);
    assert!(control.cell().is_none() && queue.cell().is_none());
  }
}
