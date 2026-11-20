//! A-724 piece 3's tests (§4.17.5 "Shares" and "Admission and backpressure"): the one room every version-control
//! work is admitted, on a real shard's state.

use slates_db::catalog::RepositoryId;
use slates_vcs::Work;

use super::{Bound, DIVIDED, FAST, Live, NARROWED, Room, room};
use crate::git_push::tests::{git_repository, owner};
use crate::state::ShardState;

/// A counter's value on `state`'s shard: zero when it never moved.
fn counted(state: &ShardState, name: &str) -> u64 {
  state.refusals.get(name).copied().unwrap_or(0)
}

/// Records on `repository`'s usage row one intake answered after bringing `bytes`, as a push's steps do.
fn answered_intake(state: &mut ShardState, repository: RepositoryId, bytes: u64) {
  state
    .vcs_usage
    .begun(&repository, &owner(), Work::Intake, 1);
  state
    .vcs_usage
    .stepped(&repository, Work::Intake, (1, bytes), 2);
  state.vcs_usage.answered(&repository, Work::Intake, 3);
}

/// A-724 piece 3. Do: ask a repository's room with no other intake under way. Expect: all the shard can admit, by the
/// fast path, counted, and nothing divided.
#[test]
fn t_a724_a_repository_alone_is_admitted_all_the_shard_can_hold_by_the_fast_path() {
  crate::daemon::audit_on_shard(|state| {
    let toy = git_repository(state, "toy");
    let own = Live::begin(toy);
    let admittable = state.store.admittable();
    assert_eq!(
      room(state, toy),
      Room {
        bytes: admittable,
        bound: Bound::Shard
      }
    );
    assert_eq!((counted(state, FAST), counted(state, DIVIDED)), (1, 0));
    drop(own);
  });
}

/// A-724 piece 3. Do: push into a monorepo with no answered intake yet (its claim is everything the shard can admit)
/// while a toy repository asks for room. Expect: the toy's room is its equal half of the shard, bound by its share and
/// counted; once the monorepo's intakes are known to bring little, the toy is given all but that little; and once the
/// monorepo's intake ends, the toy is alone again and given everything by the fast path.
#[test]
fn t_a724_a_repositorys_room_is_its_share_while_others_push_and_learns_from_their_traffic() {
  crate::daemon::audit_on_shard(|state| {
    let monorepo = git_repository(state, "monorepo");
    let toy = git_repository(state, "toy");
    let admittable = state.store.admittable();
    let pushing = Live::begin(monorepo);
    let halved = room(state, toy);
    assert!(
      matches!(halved.bound, Bound::Share { held: 0, .. }),
      "the toy's room is bound by its share: {halved:?}"
    );
    assert!(
      halved.bytes.abs_diff(admittable / 2) <= 1,
      "an equal half of {admittable}: {halved:?}"
    );
    assert_eq!((counted(state, DIVIDED), counted(state, NARROWED)), (1, 1));
    let typical = admittable / 64;
    answered_intake(state, monorepo, typical);
    let learned = room(state, toy);
    assert!(
      learned.bytes.abs_diff(admittable - typical) <= 1,
      "all but the monorepo's typical intake of {typical}: {learned:?}"
    );
    drop(pushing);
    assert_eq!(
      room(state, toy),
      Room {
        bytes: admittable,
        bound: Bound::Shard
      }
    );
    assert_eq!(counted(state, FAST), 1);
  });
}

/// A-724 piece 3. Do: start a real git push into one repository and ask another's room while it is under way, then drop
/// the push unanswered. Expect: the push counts as live (the other's room is divided) and its end, however it ended,
/// leaves the other alone again.
#[test]
fn t_a724_a_push_is_live_from_its_start_until_it_drops() {
  crate::daemon::audit_on_shard(|state| {
    let pushed = git_repository(state, "pushed");
    let other = git_repository(state, "other");
    let mut advertised = Vec::new();
    let push = crate::git_push::ReceivePack::start(
      state,
      (&owner(), pushed),
      true,
      &mut advertised,
      crate::git_push::Unpresented,
    )
    .expect("started");
    assert!(matches!(room(state, other).bound, Bound::Share { .. }));
    drop(push);
    assert_eq!(room(state, other).bound, Bound::Shard);
  });
}
