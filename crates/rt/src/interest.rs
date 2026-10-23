//! Which readiness registration fired (§4.3 "Drivers"; A-149). A driver's completion carries the word its
//! registration was given and nothing else, so the word names the registration: an interest word
//! ([`slates_mem::Encoded::interest`]) holds the descriptor and its direction, and this table maps that key to the
//! task waiting on it. A readiness future then answers ready only when its own descriptor fired, so a task that races
//! two waits learns which one happened. Examples are a FUSE mount's device against its change signal, or any wait
//! against a deadline.
//!
//! Until A-149 the completion carried the task's word, so a readiness future answered ready on any wake of its task.
//! A daemon serving two FUSE mounts of a volume spun a core after one write, because the change signal's wake was read
//! as the device's and the signal never drained. And a deadline raced against a bare readiness wait could never be
//! reported (`docs/bugs/2026-10-22-a-readiness-wait-took-any-wake-of-its-task-for-its-own.md`). Probing the
//! descriptor with a zero-timeout `poll(2)` on each wake was rejected: it attributes as exactly, but costs one syscall
//! per wake on every socket and device, where the interest word costs a map lookup.
//!
//! **The bound.** The table holds one record per descriptor and direction. A registration replaces the key's earlier
//! holder, as the drivers themselves do (epoll's `EPOLL_CTL_MOD`, kqueue's `EV_ADD` on a live filter), so the table
//! never holds more than twice the descriptors the process may open. A record leaves when its future answers ready or
//! is dropped.
//!
//! Pure and single-threaded: the shard owns it, and its tests need no driver.

use std::collections::BTreeMap;

/// Which readiness a registration awaits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Direction {
  /// The descriptor has data to read, or a listener a connection to accept.
  Readable,
  /// The descriptor has room for a write.
  Writable,
}

impl Direction {
  /// Whether this is write interest (the interest word's direction bit).
  pub(crate) const fn writable(self) -> bool {
    matches!(self, Direction::Writable)
  }

  /// The direction an interest word's bit names.
  pub(crate) const fn from_writable(writable: bool) -> Direction {
    if writable {
      Direction::Writable
    } else {
      Direction::Readable
    }
  }
}

/// What a registration's future finds when its task is polled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Taken {
  /// Its descriptor fired: the record is taken, and the caller retries its syscall.
  Fired,
  /// Its descriptor has not fired: something else woke the task.
  Waiting,
  /// The record is gone, replaced by a later registration of the same descriptor and direction: register again.
  Lost,
}

/// One registration: the task it wakes, its own token, and whether its descriptor fired.
#[derive(Debug)]
struct Registration {
  task: u64,
  token: u64,
  fired: bool,
}

/// A shard's readiness registrations, by descriptor and direction.
#[derive(Debug, Default)]
pub(crate) struct Interests {
  held: BTreeMap<(i32, Direction), Registration>,
  next_token: u64,
  unclaimed: u64,
}

impl Interests {
  /// Records that the task whose word is `task` awaits `raw`'s `direction`, replacing the key's earlier holder,
  /// and returns the registration's token. Returns `None` once every token has been issued: refused, never
  /// reused, since a reused token could let an old future's drop forget a live registration.
  pub(crate) fn register(&mut self, raw: i32, direction: Direction, task: u64) -> Option<u64> {
    let token = self.next_token;
    self.next_token = token.checked_add(1)?;
    self.held.insert(
      (raw, direction),
      Registration {
        task,
        token,
        fired: false,
      },
    );
    Some(token)
  }

  /// `raw`'s `direction` fired: marks its registration and names the task to wake. Returns `None`, and counts
  /// it, when no registration holds the key because its future was dropped first.
  pub(crate) fn fire(&mut self, raw: i32, direction: Direction) -> Option<u64> {
    match self.held.get_mut(&(raw, direction)) {
      Some(registration) => {
        registration.fired = true;
        Some(registration.task)
      }
      None => {
        self.unclaimed = self.unclaimed.saturating_add(1);
        None
      }
    }
  }

  /// What registration `token` of `raw`'s `direction` finds; a fired one is taken.
  pub(crate) fn take(&mut self, raw: i32, direction: Direction, token: u64) -> Taken {
    let key = (raw, direction);
    let fired = match self.held.get(&key) {
      Some(registration) if registration.token == token => registration.fired,
      _ => return Taken::Lost,
    };
    if fired {
      self.held.remove(&key);
      Taken::Fired
    } else {
      Taken::Waiting
    }
  }

  /// Forgets registration `token` of `raw`'s `direction`, because its future was dropped. A later holder of the
  /// key is kept.
  pub(crate) fn forget(&mut self, raw: i32, direction: Direction, token: u64) {
    let key = (raw, direction);
    if self
      .held
      .get(&key)
      .is_some_and(|registration| registration.token == token)
    {
      self.held.remove(&key);
    }
  }

  /// Completions that found no registration holding their key (their futures were dropped first).
  pub(crate) const fn unclaimed(&self) -> u64 {
    self.unclaimed
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Shape: a task's word, as the shard records it.
  const TASK: u64 = 0x0001_0000_0300_0002;
  /// Shape: another task's word.
  const OTHER_TASK: u64 = 0x0001_0000_0400_0002;

  /// A-149. Do: register read interest in a descriptor, take before and after it fires, then take once more.
  /// Expect: waiting, then fired with the task named, then lost (taken once).
  #[test]
  fn a_registration_fires_once_and_is_taken_once() {
    let mut interests = Interests::default();
    let token = interests.register(5, Direction::Readable, TASK).unwrap();
    assert_eq!(
      interests.take(5, Direction::Readable, token),
      Taken::Waiting
    );
    assert_eq!(interests.fire(5, Direction::Readable), Some(TASK));
    assert_eq!(interests.take(5, Direction::Readable, token), Taken::Fired);
    assert_eq!(interests.take(5, Direction::Readable, token), Taken::Lost);
  }

  /// A-149, the bug's own shape. Do: register a task's interest in two descriptors (a device and a change signal),
  /// and fire only the second. Expect: the first is still waiting, the second fired.
  #[test]
  fn a_task_waiting_on_two_descriptors_is_told_which_one_fired() {
    let mut interests = Interests::default();
    let device = interests.register(9, Direction::Readable, TASK).unwrap();
    let signal = interests.register(12, Direction::Readable, TASK).unwrap();
    assert_eq!(interests.fire(12, Direction::Readable), Some(TASK));
    assert_eq!(
      interests.take(9, Direction::Readable, device),
      Taken::Waiting
    );
    assert_eq!(
      interests.take(12, Direction::Readable, signal),
      Taken::Fired
    );
  }

  /// A-149. Do: register read interest, then fire the same descriptor's write side and another descriptor. Expect:
  /// neither reaches the registration, both are counted unclaimed, and it is still waiting.
  #[test]
  fn another_direction_or_descriptor_firing_leaves_a_registration_waiting() {
    let mut interests = Interests::default();
    let token = interests.register(5, Direction::Readable, TASK).unwrap();
    assert_eq!(interests.fire(5, Direction::Writable), None);
    assert_eq!(interests.fire(6, Direction::Readable), None);
    assert_eq!(interests.unclaimed(), 2);
    assert_eq!(
      interests.take(5, Direction::Readable, token),
      Taken::Waiting
    );
  }

  /// A-149. Do: register a descriptor's read interest for one task, then again for another, and fire it. Expect: the
  /// later registration is woken and fired; the earlier one is lost, so its future registers again.
  #[test]
  fn a_later_registration_of_the_same_descriptor_replaces_the_earlier() {
    let mut interests = Interests::default();
    let earlier = interests.register(5, Direction::Readable, TASK).unwrap();
    let later = interests
      .register(5, Direction::Readable, OTHER_TASK)
      .unwrap();
    assert_eq!(interests.fire(5, Direction::Readable), Some(OTHER_TASK));
    assert_eq!(interests.take(5, Direction::Readable, earlier), Taken::Lost);
    assert_eq!(interests.take(5, Direction::Readable, later), Taken::Fired);
  }

  /// A-149. Do: replace a registration, then forget the earlier one by its token, then forget the later one and fire.
  /// Expect: forgetting the earlier leaves the later in place; once both are forgotten, the completion wakes no one
  /// and is counted.
  #[test]
  fn forgetting_removes_only_its_own_registration() {
    let mut interests = Interests::default();
    let earlier = interests.register(5, Direction::Readable, TASK).unwrap();
    let later = interests
      .register(5, Direction::Readable, OTHER_TASK)
      .unwrap();
    interests.forget(5, Direction::Readable, earlier);
    assert_eq!(
      interests.take(5, Direction::Readable, later),
      Taken::Waiting
    );
    interests.forget(5, Direction::Readable, later);
    assert_eq!(interests.fire(5, Direction::Readable), None);
    assert_eq!(interests.unclaimed(), 1);
  }

  /// A-149. Do: start a table one token short of exhaustion; register twice. Expect: the first registration is
  /// issued, the second refused, and the first still holds its key (the last token is never issued, so none is
  /// ever reused).
  #[test]
  fn tokens_are_refused_once_exhausted_never_reused() {
    let mut interests = Interests {
      next_token: u64::MAX - 1,
      ..Interests::default()
    };
    let last = interests.register(5, Direction::Readable, TASK).unwrap();
    assert_eq!(interests.register(6, Direction::Readable, TASK), None);
    assert_eq!(interests.fire(5, Direction::Readable), Some(TASK));
    assert_eq!(interests.take(5, Direction::Readable, last), Taken::Fired);
  }
}
