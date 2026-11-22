//! A work past its room waits for room, never refused while room may come (A-857, main 01a20182's ruling of
//! 2026-11-22 03:03: "an intake past its admission or room waits for room and is never refused; a piece larger than the
//! repository's whole room gets a typed refusal, never a wait that can't end"; one mechanism for git and hg, A-855's).
//!
//! **What.** A [`RoomWait`] is a work's wait after it ran past the room it was admitted: each turn it asks its
//! repository's room again ([`super::room`]) and answers [`Waited::Again`] once the room has grown past what the work was
//! admitted and covers what it is known to need, so the work is made again (or resumed, where the work can be) within
//! it; [`Waited::Wait`] until then, a turn's interval at a time; and [`Waited::GiveUp`] once its patience has run out, or
//! at once when what it needs is past the most its repository could ever be lent on its shard ([`ceiling`]), since no
//! wait could end. The caller answers its typed refusal on `GiveUp`, as it did before it waited.
//!
//! **Patience.** The daemon's observation budget (`crate::daemon::OBSERVE_BUDGET_NS`), the longest it waits on any
//! party before judging it, as A-855's fetch held past its room waits.
//!
//! **Its callers.** git's deferred build past its room (`crate::green_ready`), and git's fetch answer held past its room
//! (`crate::vcs_http::git::upload`, A-855); hg's intake past its admission (w-hg's).

use slates_db::catalog::RepositoryId;

use crate::state::ShardState;

/// How long a work past its room waits for more (the module doc, "Patience").
pub(crate) const PATIENCE_NS: u64 = crate::daemon::OBSERVE_BUDGET_NS;

/// Counter: a work past its room began to wait for room. Format: a counter name.
pub(crate) const WAITED: &str = "vcs_share.room_waits";
/// Counter: a wait for room that ended taken, the work made again in the room that came (the wait's non-vacuity
/// counter: a wait happened and its work went on). Format: a counter name.
pub(crate) const TAKEN: &str = "vcs_share.room_waits_taken";
/// Counter: a wait for room given up, past its patience or past its repository's ceiling. Format: a counter name.
pub(crate) const GIVEN_UP: &str = "vcs_share.room_waits_given_up";

/// What a turn of a [`RoomWait`] came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Waited {
  /// The room has grown: make the work again within these bytes.
  Again(u64),
  /// Not yet: wait this many nanoseconds, then turn again.
  Wait(u64),
  /// No room will come in time, or none could: answer the work's refusal.
  GiveUp,
}

/// A work's wait for room past what it was admitted (the module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RoomWait {
  /// The room the work was admitted, which a room it is made again within must pass.
  admitted: u64,
  /// When it began to wait.
  since_ns: u64,
  /// Whether its beginning is counted ([`WAITED`]).
  counted: bool,
}

impl RoomWait {
  /// A wait beginning at `now_ns` for a work admitted `admitted` bytes.
  pub(crate) fn new(admitted: u64, now_ns: u64) -> RoomWait {
    RoomWait {
      admitted,
      since_ns: now_ns,
      counted: false,
    }
  }

  /// A turn of the wait at `now_ns` for `repository`'s work, known to need at least `needed` bytes where its refusal
  /// said, waiting at most `interval_ns` before the next (the caller's keepalive, or a turn): what it came to (the module
  /// doc). `Again` admits the room it gives as the work's next admission.
  pub(crate) fn turn(
    &mut self,
    state: &mut ShardState,
    repository: RepositoryId,
    (needed, now_ns, interval_ns): (Option<u64>, u64, u64),
  ) -> Waited {
    if !self.counted {
      self.counted = true;
      state.count(WAITED, 1);
    }
    if needed.is_some_and(|needed| needed > ceiling(state, repository)) {
      state.count(GIVEN_UP, 1);
      return Waited::GiveUp;
    }
    let room = super::room(state, repository).bytes;
    if room > self.admitted && needed.is_none_or(|needed| room >= needed) {
      self.admitted = room;
      self.since_ns = now_ns;
      self.counted = false;
      state.count(TAKEN, 1);
      return Waited::Again(room);
    }
    let waited = now_ns.saturating_sub(self.since_ns);
    if waited >= PATIENCE_NS {
      state.count(GIVEN_UP, 1);
      return Waited::GiveUp;
    }
    Waited::Wait(interval_ns.min(PATIENCE_NS.saturating_sub(waited)).max(1))
  }
}

/// The most `repository` could ever be lent on its shard (the module doc): all the shard could admit now with nothing
/// else held, and what it holds itself, within the snapshot slot's room left; a work needing more than this is refused at
/// once, never waited for.
pub(crate) fn ceiling(state: &mut ShardState, repository: RepositoryId) -> u64 {
  let memory = state
    .store
    .admittable()
    .saturating_add(super::held(state, repository));
  memory.min(state.db.snapshot_room(&state.segment))
}

#[cfg(test)]
mod tests {
  use super::{PATIENCE_NS, RoomWait, Waited};

  /// A-857 (the shared wait's decisions). Do: on an audit shard, turn a wait for a work admitted one byte, then one
  /// admitted past all the shard could ever lend, then one whose patience has run out, then one needing more than the
  /// shard's ceiling. Expect: `Again` with the shard's room (past one byte) and the wait's admission moved to it; `Wait`
  /// for the turn's interval while the room has not grown; `GiveUp` past patience; `GiveUp` at once past the ceiling.
  #[test]
  fn a_wait_answers_again_when_the_room_grows_waits_until_then_and_gives_up_past_patience_or_the_ceiling()
   {
    crate::test_store::audit_on_stored_shard("room-wait", |state| {
      let repository = crate::git_push::tests::git_repository(state, "waiting");
      let room = super::super::room(state, repository).bytes;
      let mut small = RoomWait::new(1, 0);
      assert_eq!(
        small.turn(state, repository, (None, 0, 5)),
        Waited::Again(room)
      );
      let mut full = RoomWait::new(room, 0);
      assert_eq!(full.turn(state, repository, (None, 10, 5)), Waited::Wait(5));
      assert_eq!(
        full.turn(state, repository, (None, PATIENCE_NS, 5)),
        Waited::GiveUp
      );
      let ceiling = super::ceiling(state, repository);
      let mut past = RoomWait::new(1, 0);
      assert_eq!(
        past.turn(state, repository, (Some(ceiling.saturating_add(1)), 0, 5)),
        Waited::GiveUp
      );
    });
  }
}
