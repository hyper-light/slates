//! A work past its room waits for room, never refused while room may come (A-857, main 01a20182's ruling of
//! 2026-11-22 03:03: "an intake past its admission or room waits for room and is never refused; a piece larger than the
//! repository's whole room gets a typed refusal, never a wait that can't end"; one mechanism for git and hg, A-855's).
//!
//! **What.** A [`RoomWait`] is a work's wait after it ran past the room it was admitted: each turn it asks its
//! repository's room again ([`super::room`]) and answers [`Waited::Again`] once the room has grown past what the work was
//! admitted and covers what it is known to need, so the work is made again (or resumed, where the work can be) within
//! it; [`Waited::Wait`] until then, a turn's interval at a time; [`Waited::GiveUp`] once its patience has run out; and
//! [`Waited::PastCeiling`] at once, on whichever turn finds it, when what it needs is past the most its repository could
//! ever be lent on its shard ([`ceiling`]), since no wait could end: the lesser of the shard's whole grantable memory (its
//! budget's capacity and what its arena could still claim from its pool, less the operation headroom no admission takes)
//! and its partition's whole snapshot slot, what it could be lent were every other holder to let go (main 01a20182's
//! ruling of 2026-11-22 05:04: the room admittable now falls as others hold, so a ceiling of it named a wait that could
//! end as one that could not). The caller answers its refusal on `GiveUp`, as it
//! did before it waited, and the typed words that it could never be taken on `PastCeiling` (main 01a20182's (ii) of
//! 2026-11-22 05:01: the two were one answer, so a push past the ceiling was answered its step's bare refusal).
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
/// Counter: of the waits given up ([`GIVEN_UP`]), those past their repository's ceiling ([`Waited::PastCeiling`]).
/// Format: a counter name.
pub(crate) const PAST_CEILING: &str = "vcs_share.room_waits_past_ceiling";

/// What a turn of a [`RoomWait`] came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Waited {
  /// The room has grown: make the work again within these bytes.
  Again(u64),
  /// Not yet: wait this many nanoseconds, then turn again.
  Wait(u64),
  /// No room came within the wait's patience: answer the work's refusal.
  GiveUp,
  /// What the work needs is past the most its repository could ever be lent ([`ceiling`]): answer that it could never
  /// be taken here, never a refusal that names a wait.
  PastCeiling,
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
    if needed.is_some_and(|needed| needed > ceiling(state)) {
      state.count(GIVEN_UP, 1);
      state.count(PAST_CEILING, 1);
      return Waited::PastCeiling;
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

/// The most any repository could ever be lent on `state`'s shard (the module doc): the lesser of the shard's whole
/// grantable memory, `budget.capacity() + arena.claimable() - budget.headroom()` (every commitment, the pressure hold and
/// its debt let go; the headroom is never lent), and the partition's whole snapshot slot (`Geometry::snapshot_bytes`, as
/// a snapshot taken anew lets go of every record since). A work needing more than this is refused at once, never waited
/// for; one needing less waits, to its patience, for others to let go.
pub(crate) fn ceiling(state: &ShardState) -> u64 {
  #[cfg(test)]
  {
    let set = TEST_CEILING.with(core::cell::Cell::get);
    if set > 0 {
      return set;
    }
  }
  let budget = &state.store.budget;
  let claimable = u64::try_from(state.store.content.arena().claimable()).unwrap_or(u64::MAX);
  let memory = budget
    .capacity()
    .saturating_add(claimable)
    .saturating_sub(budget.headroom());
  memory.min(state.segment.geometry().snapshot_bytes)
}

// The tests' hook, kept on the shard that serves the work, so a test's daemon alone meets it.
#[cfg(test)]
thread_local! {
  /// A test's ceiling for every repository on this shard (0: none): a work's need made past the most it could ever be
  /// lent without a shard that small.
  pub(crate) static TEST_CEILING: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
  use super::{PATIENCE_NS, RoomWait, Waited};

  /// A-857 (the shared wait's decisions). Do: on an audit shard, turn a wait for a work admitted one byte, then one
  /// admitted past all the shard could ever lend, then one whose patience has run out, then one needing more than the
  /// shard's ceiling. Expect: `Again` with the shard's room (past one byte) and the wait's admission moved to it; `Wait`
  /// for the turn's interval while the room has not grown; `GiveUp` past patience; `PastCeiling` at once past the ceiling,
  /// told from patience so its caller answers the typed refusal (main 01a20182's (ii) of 2026-11-22 05:01).
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
      let ceiling = super::ceiling(state);
      let mut past = RoomWait::new(1, 0);
      assert_eq!(
        past.turn(state, repository, (Some(ceiling.saturating_add(1)), 0, 5)),
        Waited::PastCeiling
      );
    });
  }

  /// Main 01a20182's ruling of 2026-11-22 05:04 (the ceiling a could-ever figure, never the room admittable now). Do: on
  /// an audit shard, withhold half of what it admits as a pressure hold does (`ShardBudget::set_hold`); turn a wait for
  /// a work needing one byte past what the repository could be lent with that withheld (the room admittable now and what
  /// it holds itself, the ceiling before); then let the hold go and turn it again. Expect: `Wait` while it is held, since
  /// the need is under the shard's whole; then `Again` with the room that came back, which covers the need. Red before:
  /// `PastCeiling` on the first turn, a wait that could end refused as one that could not.
  #[test]
  fn a_need_past_the_room_admittable_now_but_within_the_shards_whole_waits_and_is_taken_once_a_holder_lets_go()
   {
    crate::test_store::audit_on_stored_shard("room-wait-released", |state| {
      let repository = crate::git_push::tests::git_repository(state, "released");
      let before = super::super::room(state, repository).bytes;
      state.store.budget.set_hold(state.store.admittable() / 2);
      let now = super::super::room(state, repository).bytes;
      assert!(now < before, "the hold shrank the room: {now} of {before}");
      let needed = now
        .saturating_add(super::super::held(state, repository))
        .saturating_add(1);
      assert!(
        needed <= before,
        "the need is within the room that comes back"
      );
      let mut wait = RoomWait::new(now, 0);
      assert_eq!(
        wait.turn(state, repository, (Some(needed), 0, 5)),
        Waited::Wait(5)
      );
      state.store.budget.set_hold(0);
      let back = super::super::room(state, repository).bytes;
      assert_eq!(
        wait.turn(state, repository, (Some(needed), 10, 5)),
        Waited::Again(back)
      );
    });
  }
}
