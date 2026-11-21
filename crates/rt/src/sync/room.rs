//! The senders a bounded channel holds waiting for room (A-767 piece C2, ported from hyper-raft's `hyper-rt`
//! `sync/room.rs` at `5e40eb9`): one place each, at most the channel's `waiters`, with no lock on any path.
//!
//! **A place per waiting sender, freed in place.** A sender task that finds the channel full takes a free place
//! (compare-and-swap, free → waiting), registers its task word there and waits. A sender that stops waiting
//! without a grant (its own retry found room, or its future was dropped) frees its place at once, so the bound is
//! exactly the senders waiting now.
//!
//! **Grants go round the places.** Whoever makes room (the receiver after each take, a sender handing on room it
//! was granted and did not use) grants one waiting place (waiting → granted) and wakes its task. The search starts
//! one past the last place granted, so a waiting sender is passed over at most once a round.
//!
//! **No lost room.** The waiter takes its place, then checks the channel again; the receiver frees a value's slot,
//! then looks at the waiters. Each side writes, then reads what the other wrote, so each puts a `SeqCst` fence
//! between its write and its read: either the sender's check finds the room or the grant finds the sender. A waiter
//! counts itself before its fence, so the receiver reads one word after its fence while no one waits.
//!
//! **Where the words live.** Upstream keeps the places in an allocation the channel's ends share through a
//! hand-counted block (`unsafe`). Here every place, and the control word (the waiting count and the cursor), is a
//! cell of the process-wide table ([`super::cell`]), so the ends share only `Copy` handles, each end holding its
//! own list of them.

use std::sync::atomic::{Ordering, fence};

use slates_mem::Encoded;

use super::SyncRefusal;
use super::cell::{CellRef, claim};

/// Format: a place no sender holds.
const FREE: u64 = 0;
/// Format: a place whose sender waits for room.
const WAITING: u64 = 1;
/// Format: a place whose sender was granted room and has not yet let the place go.
const GRANTED: u64 = 2;

/// The senders waiting for room: one end's handles on the places and on the control cell (its `state` the count of
/// places waiting, its `aux` where the next grant's search starts).
#[derive(Debug)]
pub(crate) struct Room {
  places: Box<[CellRef]>,
  control: CellRef,
}

impl Room {
  /// Room for `waiters` senders waiting at once, its cells held by `ends` handles. Refused `BadConfig` for none, and
  /// `Capacity` past the cell table's bound (every cell claimed so far is given back).
  pub(crate) fn new(waiters: usize, ends: u32) -> Result<Self, SyncRefusal> {
    if waiters == 0 {
      return Err(SyncRefusal::BadConfig {
        what: "a channel with no room for a waiting sender",
      });
    }
    let control = claim(ends)?;
    let mut places = Vec::new();
    places
      .try_reserve_exact(waiters)
      .map_err(|_| SyncRefusal::BadConfig {
        what: "a channel with more waiting places than an allocation holds",
      })?;
    for _ in 0..waiters {
      match claim(ends) {
        Ok(place) => places.push(place),
        Err(refused) => {
          for place in places {
            for _ in 0..ends {
              place.release();
            }
          }
          for _ in 0..ends {
            control.release();
          }
          return Err(refused);
        }
      }
    }
    Ok(Self {
      places: places.into_boxed_slice(),
      control,
    })
  }

  /// Another end's handles on the same places (the cells' counts raised by one).
  pub(crate) fn share(&self) -> Self {
    self.control.retain();
    for place in &*self.places {
      place.retain();
    }
    Self {
      places: self.places.clone(),
      control: self.control,
    }
  }

  /// A second end's list of the same handles, raising no count: for the other end of a room whose cells were
  /// claimed with every end's handle already ([`Room::new`]'s `ends`).
  pub(crate) fn list_copy(&self) -> Self {
    Self {
      places: self.places.clone(),
      control: self.control,
    }
  }

  /// Takes a free place for a sender about to wait, counted and fenced: the caller's next look at the channel is
  /// ordered after its place shows waiting to every grant. `None` when every place is held.
  pub(crate) fn hold(&self) -> Option<usize> {
    let index = self.places.iter().position(|place| {
      place.cell().is_some_and(|cell| {
        cell
          .state
          .compare_exchange(FREE, WAITING, Ordering::AcqRel, Ordering::Acquire)
          .is_ok()
      })
    })?;
    self.counted();
    Some(index)
  }

  /// The held place `index`, granted, waits again: its room went to another sender. Counted and fenced as
  /// [`Room::hold`].
  pub(crate) fn wait_again(&self, index: usize) {
    if let Some(cell) = self.places.get(index).and_then(|place| place.cell()) {
      // A granted place is its holder's alone: no grant or holder writes it meanwhile.
      cell.state.store(WAITING, Ordering::Release);
      self.counted();
    }
  }

  /// Counts a place that went to waiting, then fences (module doc).
  fn counted(&self) {
    if let Some(control) = self.control.cell() {
      control.state.fetch_add(1, Ordering::AcqRel);
    }
    fence(Ordering::SeqCst);
  }

  /// Whether the held place `index` was granted room.
  pub(crate) fn granted(&self, index: usize) -> bool {
    self
      .places
      .get(index)
      .and_then(|place| place.cell())
      .is_some_and(|cell| cell.state.load(Ordering::Acquire) == GRANTED)
  }

  /// Registers the task `word` as the held place's waiter (fenced, [`super::cell::Cell::register`]).
  pub(crate) fn register_task(&self, index: usize, word: Encoded) {
    if let Some(place) = self.places.get(index) {
      place.register(word);
    }
  }

  /// A sender lets its place go. Room granted to the place goes on to another waiting sender unless the sender
  /// `used` it: saw the grant, then sent. A grant the sender did not see may be for room it did not take (a grant
  /// can reach a place whose sender has just sent on room made earlier), and keeping it would leave another sender
  /// waiting with room free and no grant coming (upstream's loom model found it).
  pub(crate) fn leave(&self, index: usize, used: bool) {
    if self.release(index) && !used {
      self.grant_one();
    }
  }

  /// Lets the held place `index` go, and says whether it had been granted room.
  fn release(&self, index: usize) -> bool {
    let Some(cell) = self.places.get(index).and_then(|place| place.cell()) else {
      return false;
    };
    if cell
      .state
      .compare_exchange(WAITING, FREE, Ordering::AcqRel, Ordering::Acquire)
      .is_ok()
    {
      if let Some(control) = self.control.cell() {
        control.state.fetch_sub(1, Ordering::AcqRel);
      }
      return false;
    }
    // Granted: only a grant leaves waiting, and the holder alone leaves granted.
    cell.state.store(FREE, Ordering::Release);
    true
  }

  /// Grants room to one waiting sender and wakes it, if one waits; for whoever just made room, after the room is
  /// published. Whether one was granted.
  pub(crate) fn grant_one(&self) -> bool {
    fence(Ordering::SeqCst);
    let Some(control) = self.control.cell() else {
      return false;
    };
    if control.state.load(Ordering::Acquire) == 0 {
      return false;
    }
    let count = self.places.len();
    let start = usize::try_from(control.aux.load(Ordering::Relaxed)).unwrap_or(0);
    for step in 0..count {
      let index = start.wrapping_add(step).checked_rem(count).unwrap_or(0);
      let Some(place) = self.places.get(index) else {
        continue;
      };
      let Some(cell) = place.cell() else {
        continue;
      };
      if cell.state.load(Ordering::Acquire) == WAITING
        && cell
          .state
          .compare_exchange(WAITING, GRANTED, Ordering::AcqRel, Ordering::Acquire)
          .is_ok()
      {
        control.state.fetch_sub(1, Ordering::AcqRel);
        control.aux.store(
          u64::try_from(index.wrapping_add(1)).unwrap_or(0),
          Ordering::Relaxed,
        );
        cell.wake();
        return true;
      }
    }
    false
  }

  /// Wakes every waiting sender to see the channel closed (the receiver went, after saying so).
  pub(crate) fn wake_all(&self) {
    fence(Ordering::SeqCst);
    for place in &*self.places {
      if let Some(cell) = place.cell()
        && cell.state.load(Ordering::Acquire) == WAITING
      {
        cell.wake();
      }
    }
  }
}

impl Drop for Room {
  fn drop(&mut self) {
    for place in &*self.places {
      place.release();
    }
    self.control.release();
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Do: hold both places of a two-place room, grant, wait again, let go. Expect: a full room refuses, grants go
  /// round the places, and both places are free again after.
  #[test]
  fn places_are_held_exactly_and_granted_round_the_room() {
    let room = Room::new(2, 1).unwrap();
    assert!(!room.grant_one(), "no one waits");
    let first = room.hold().unwrap();
    let second = room.hold().unwrap();
    assert_eq!(room.hold(), None, "two places, two holders");
    assert!(room.grant_one());
    assert!(room.granted(first) && !room.granted(second));
    assert!(room.grant_one(), "the next grant goes to the other holder");
    assert!(room.granted(second));
    assert!(!room.grant_one(), "no one waits now");
    room.wait_again(first);
    assert!(!room.granted(first));
    assert!(!room.release(first), "let go without a grant");
    assert!(room.release(second), "let go after its grant");
    assert!(
      room.hold().is_some() && room.hold().is_some(),
      "both places free again"
    );
    assert!(Room::new(0, 1).is_err());
  }

  /// Do: make a room shared by two ends, drop both. Expect: every cell it claimed is given back.
  #[test]
  fn a_room_gives_its_cells_back_with_its_last_end() {
    let room = Room::new(3, 1).unwrap();
    let cells: Vec<CellRef> = room.places.iter().copied().chain([room.control]).collect();
    let other = room.share();
    drop(room);
    assert!(
      cells.iter().all(|cell| cell.cell().is_some()),
      "the other end still holds every cell"
    );
    drop(other);
    assert!(
      cells.iter().all(|cell| cell.cell().is_none()),
      "the last end gave every cell back"
    );
  }
}
