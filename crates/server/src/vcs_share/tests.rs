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

/// Turns repository `id`'s encoding until every version's fragments are kept, as a fetch's request does.
fn encoded(state: &mut ShardState, id: RepositoryId) {
  use crate::vcs_store::git_encode::{Turned, turn};
  while turn(state, id) == Turned::More {}
}

/// Encodes repository `id`'s versions and expects the encodings' time and bytes learned on its usage row and the room
/// it holds counted with its fragments: the bytes of the fragments kept.
fn encoded_and_learned(state: &mut ShardState, id: RepositoryId) -> u64 {
  use crate::vcs_store::git_encode::kept_bytes;
  encoded(state, id);
  let row = state.vcs_usage.usage(&id).expect("a row");
  assert!(
    row.derive_ns > 0 && row.derive_bytes > 0,
    "the encodings learned: {row:?}"
  );
  let kept = kept_bytes(state, id);
  assert!(kept > 0, "fragments kept");
  assert_eq!(
    super::held_by(state, id),
    state.vcs_stores.objects_charged(id) + kept
  );
  kept
}

/// A-724 piece 4 (§4.17.5 "The cold trigger"). Do: push stock git's fixtures into a repository and encode its versions
/// (as its first fetch would), then ask it to give back more than it holds of fragments, at a pressure, with no fetch
/// ever seen; then the same once its fetches outnumber its pushes a thousandfold. Expect: the encodings' time and bytes
/// learned on its usage row; the room it holds counted with its fragments; with no reads to come every version's
/// fragments dropped, their bytes given back exactly and counted, and the versions encoded again; and once its reads are
/// many, at the same pressure, its fragments kept.
#[test]
fn t_a724_a_repository_over_its_share_drops_fragments_only_where_rebuilding_costs_less_than_the_room()
 {
  use crate::vcs_store::git_encode::{DROPPED, kept_bytes, relieve};
  crate::daemon::audit_on_shard(|state| {
    let pushed = git_repository(state, "pushed");
    crate::git_push::tests::pushed_from_the_fixtures(state, pushed, &["create", "update"]);
    let kept = encoded_and_learned(state, pushed);
    let given = relieve(state, pushed, kept.saturating_mul(2), 2.0);
    assert_eq!(given, kept, "every version's fragments given back, exactly");
    assert_eq!(kept_bytes(state, pushed), 0);
    assert!(counted(state, DROPPED) >= 1);
    encoded(state, pushed);
    // Encoded again: its size may differ, since the encoder's learned rules moved with its first encodings.
    let kept_again = kept_bytes(state, pushed);
    assert!(kept_again > 0, "encoded again");
    for _ in 0..2_000 {
      state.vcs_usage.begun(&pushed, &owner(), Work::Fetch, 10);
    }
    assert_eq!(
      relieve(state, pushed, kept.saturating_mul(2), 2.0),
      0,
      "kept: reads make rebuilding dear"
    );
    assert_eq!(kept_bytes(state, pushed), kept_again);
  });
}
