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

/// Records on `repository`'s usage row one intake begun at `at`, stepped, and answered after bringing `bytes`, as a
/// push's steps do.
fn answered_intake_at(state: &mut ShardState, repository: RepositoryId, bytes: u64, at: u64) {
  state
    .vcs_usage
    .begun(&repository, &owner(), Work::Intake, at);
  state
    .vcs_usage
    .stepped(&repository, Work::Intake, (1, bytes), at + 1);
  state
    .vcs_usage
    .answered(&repository, Work::Intake, bytes, at + 2);
}

/// [`answered_intake_at`] at the start of the shard's clock.
fn answered_intake(state: &mut ShardState, repository: RepositoryId, bytes: u64) {
  answered_intake_at(state, repository, bytes, 1);
}

/// A-724 piece 3. Do: ask a repository's room with no other intake under way. Expect: all the shard can admit, by the
/// fast path, counted, and nothing divided.
#[test]
fn t_a724_a_repository_alone_is_admitted_all_the_shard_can_hold_by_the_fast_path() {
  crate::test_store::audit_on_stored_shard("vcs_share", |state| {
    let toy = git_repository(state, "toy");
    let own = Live::begin(toy);
    let admittable = state.store.admittable();
    let slot_room = state.db.snapshot_room(&state.segment);
    assert_eq!(room(state, toy), super::alone(admittable, slot_room));
    assert_eq!(room(state, toy).bytes, admittable.min(slot_room));
    assert_eq!((counted(state, FAST), counted(state, DIVIDED)), (2, 0));
    drop(own);
  });
}

/// A-724 piece 3. Do: push into a monorepo with no answered intake yet (its claim is everything the shard can admit)
/// while a toy repository asks for room. Expect: the toy's room is its equal half of the shard's lesser free resource
/// (memory or the snapshot slot's room), bound by its share and counted; once the monorepo's intakes are known to bring
/// little, the toy is given all but that little; and once the monorepo's intake ends, the toy is alone again and given
/// everything by the fast path.
#[test]
fn t_a724_a_repositorys_room_is_its_share_while_others_push_and_learns_from_their_traffic() {
  crate::test_store::audit_on_stored_shard("vcs_share", |state| {
    let monorepo = git_repository(state, "monorepo");
    let toy = git_repository(state, "toy");
    let admittable = state.store.admittable();
    // Memory and the snapshot slot are divided together (A-724 piece 8a): the lesser room binds.
    let free = admittable.min(state.db.snapshot_room(&state.segment));
    let pushing = Live::begin(monorepo);
    let halved = room(state, toy);
    assert!(
      matches!(halved.bound, Bound::Share { held: 0, .. }),
      "the toy's room is bound by its share: {halved:?}"
    );
    assert!(
      halved.bytes.abs_diff(free / 2) <= 1,
      "an equal half of {free}: {halved:?}"
    );
    assert_eq!((counted(state, DIVIDED), counted(state, NARROWED)), (1, 1));
    let typical = free / 64;
    answered_intake(state, monorepo, typical);
    let learned = room(state, toy);
    assert!(
      learned.bytes.abs_diff(free - typical) <= 1,
      "all but the monorepo's typical intake of {typical}: {learned:?}"
    );
    drop(pushing);
    let slot_room = state.db.snapshot_room(&state.segment);
    assert_eq!(room(state, toy), super::alone(admittable, slot_room));
    assert_eq!(counted(state, FAST), 1);
  });
}

/// A-724 piece 3. Do: start a real git push into one repository and ask another's room while it is under way, then drop
/// the push unanswered. Expect: the push counts as live (the other's room is divided) and its end, however it ended,
/// leaves the other alone again.
#[test]
fn t_a724_a_push_is_live_from_its_start_until_it_drops() {
  crate::test_store::audit_on_stored_shard("vcs_share", |state| {
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
    assert!(matches!(
      room(state, other).bound,
      Bound::Shard | Bound::Snapshot { .. }
    ));
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
  crate::test_store::audit_on_stored_shard("vcs_share", |state| {
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

/// A-724 piece 8a in admission. Do: on a daemon whose snapshot slot is smaller than what its shard can admit, ask a
/// repository's room alone, then while another repository pushes. Expect: alone, the slot's room left, bound by the
/// snapshot and naming it; beside another, at most that, the slot divided between them.
#[test]
fn t_a724_a_repositorys_room_is_bounded_by_its_partitions_snapshot_slot() {
  crate::test_store::audit_on_stored_shard_configured(
    "vcs_share",
    |state| {
      let toy = git_repository(state, "toy");
      let other = git_repository(state, "other");
      let admittable = state.store.admittable();
      let slot_room = state.db.snapshot_room(&state.segment);
      assert!(
        slot_room < admittable,
        "the fixture's slot ({slot_room}) not below the admittable room ({admittable})"
      );
      assert_eq!(
        room(state, toy),
        Room {
          bytes: slot_room,
          bound: Bound::Snapshot { room: slot_room }
        }
      );
      let pushing = Live::begin(other);
      let shared = room(state, toy);
      assert!(shared.bytes <= slot_room, "{shared:?}");
      assert!(
        shared.bytes.abs_diff(slot_room / 2) <= 1 || matches!(shared.bound, Bound::Snapshot { .. }),
        "the slot is divided between the two: {shared:?} of {slot_room}"
      );
      drop(pushing);
    },
    |config| {
      // The slot shrunk below the shard's admittable room, as a giant repository's partition meets it.
      config.geometry.snapshot_bytes = config.geometry.page.saturating_mul(1 << 8);
    },
  );
}

/// A-724 piece 2 (§4.17.5 "Learned demand"). Do: answer a hundred small intakes of a monorepo at a steady rhythm, then
/// three intakes 64 times larger at the same rhythm, and ask a toy repository's room while the monorepo pushes again.
/// Expect: the monorepo claims its learned next intake, near the large size, so the toy is left at most the free room
/// less half of it; its lifetime mean (a hundredth or so of the large size above the small) would have left the toy
/// nearly everything, overcommitting the shard to the monorepo's next push.
#[test]
fn t_a724_a_repositorys_claim_follows_its_learned_demand_not_its_lifetime_mean() {
  crate::test_store::audit_on_stored_shard("vcs_share", |state| {
    let monorepo = git_repository(state, "monorepo");
    let toy = git_repository(state, "toy");
    let free = state
      .store
      .admittable()
      .min(state.db.snapshot_room(&state.segment));
    let (small, large, gap) = (free / 4096, free / 64, 1_000_000);
    let mut at = 1;
    for _ in 0..100 {
      answered_intake_at(state, monorepo, small, at);
      at += gap;
    }
    for _ in 0..3 {
      answered_intake_at(state, monorepo, large, at);
      at += gap;
    }
    let pushing = Live::begin(monorepo);
    let left = room(state, toy);
    assert!(
      left.bytes <= free - large / 2,
      "the toy is left {} of {free}: the monorepo claimed less than half its learned {large}",
      left.bytes
    );
    drop(pushing);
  });
}

/// Feeds `request` to a push of `repository`'s by its owner, piece by piece as an edge does: the first refusal, if any.
fn first_refusal(
  state: &mut ShardState,
  repository: RepositoryId,
  request: &[u8],
) -> Option<crate::git_push::PushFailed> {
  let mut advertised = Vec::new();
  let mut push = crate::git_push::ReceivePack::start(
    state,
    (&owner(), repository),
    true,
    &mut advertised,
    crate::git_push::Unpresented,
  )
  .expect("started");
  for piece in request.chunks(64) {
    if let Err(refused) = push.input(state, piece) {
      return Some(refused);
    }
  }
  if let Err(refused) = push.end(state) {
    return Some(refused);
  }
  let mut answer = Vec::new();
  loop {
    match push.answer_step(state, &mut answer) {
      Err(refused) => return Some(refused),
      Ok(crate::git_push::Answering::Answered(_)) => return None,
      Ok(crate::git_push::Answering::Turned) => {
        crate::git_push::tests::carry_blocking(state, &mut push)
      }
      #[cfg(unix)]
      Ok(step @ crate::git_push::Answering::Hook(_)) => panic!("no hook: {step:?}"),
    }
  }
}

/// A-724 piece 3b (§4.17.5 "Admission and backpressure": past its share, a work is refused typed and retryable, naming
/// the share). Do: on a daemon whose partition's snapshot slot has 461 bytes of room (a shard's memory is too large to
/// divide this small in a test, and the slot is divided with it, A-724 piece 8a), push stock git's first fixture into a
/// toy repository while a monorepo pushes, so the toy's room is its share of the slot (230 bytes), then again once the
/// monorepo's push has ended. Expect: beside the monorepo, refused past its share, typed so, its words naming the share
/// and what it holds and saying a retry may pass once others' pushes end, never git's words for a push too large for the
/// server; alone, not refused for a share.
#[test]
fn t_a724_a_push_past_its_share_is_refused_retryable_naming_the_share() {
  crate::test_store::audit_on_stored_shard_configured(
    "vcs_share",
    |state| {
      let monorepo = git_repository(state, "monorepo");
      let toy = git_repository(state, "toy");
      let request = crate::git_push::tests::chain()[0].0.request;
      let pushing = Live::begin(monorepo);
      assert!(matches!(room(state, toy).bound, Bound::Share { .. }));
      let refused = first_refusal(state, toy, request).expect("refused past its share");
      assert!(
        matches!(refused, crate::git_push::PushFailed::PastShare { .. }),
        "typed as past its share: {refused:?}"
      );
      let words = crate::git_push::words::failed(&refused, "toy");
      assert!(
        words.contains("share") && words.contains("retry"),
        "a share's refusal names the share and says a retry may pass: {words:?}"
      );
      drop(pushing);
      let alone = first_refusal(state, toy, request);
      assert!(
        !matches!(alone, Some(crate::git_push::PushFailed::PastShare { .. })),
        "alone, not refused for a share: {alone:?}"
      );
    },
    |config| {
      // The slot shrunk so its room, divided, binds a push of a few hundred bytes (6,400 bytes less the partition's
      // state leaves 461 here).
      config.geometry.snapshot_bytes = 6_400;
    },
  );
}

/// A-724 piece 3d (§4.17.5 "Admission and backpressure": a fetch's room is a claim of its own). Do: push stock git's
/// fixtures into a repository and hold its generation as a fetch's answer does (`ShardStores::hold`), then ask another
/// repository's room while the hold lasts, and again once it is given back. Expect: while the fetch is in flight, the
/// other's room divided with it (bound by its share, counted divided); once it ends, the other alone again, by the fast
/// path.
#[test]
fn t_a724_a_fetch_in_flight_is_a_claim_on_the_room() {
  crate::test_store::audit_on_stored_shard("vcs_share", |state| {
    let fetched = git_repository(state, "fetched");
    let toy = git_repository(state, "toy");
    crate::git_push::tests::pushed_from_the_fixtures(state, fetched, &["create", "update"]);
    let held = crate::vcs_store::upload::hold_in(state, fetched, &owner()).expect("held");
    let divided = room(state, toy);
    assert!(
      matches!(divided.bound, Bound::Share { .. }),
      "a fetch in flight divides the room: {divided:?}"
    );
    drop(held);
    assert!(matches!(
      room(state, toy).bound,
      Bound::Shard | Bound::Snapshot { .. }
    ));
  });
}

/// A-724 piece 3d. Do: on a store-holding daemon, push stock git's fixtures into a repository; then, from a task on its
/// shard as an edge's answer runs, hold its generation, report 5,000 bytes written by the answer, and give the hold
/// back. Expect: the fetch answered on the repository's usage row, its 5,000 bytes learned as its fetch demand.
#[test]
fn t_a724_a_fetchs_answer_is_counted_and_learned_on_its_repositorys_usage_row() {
  use crate::vcs_http::git::upload::StoreLender;
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let fetched = crate::daemon::observe_first(&daemon, |state| {
    let fetched = git_repository(state, "fetched");
    crate::git_push::tests::pushed_from_the_fixtures(state, fetched, &["create", "update"]);
    fetched
  });
  let (sent, done) = std::sync::mpsc::sync_channel(1);
  crate::daemon::observe_first(&daemon, move |_| {
    let task = slates_rt::futures::spawn(async move {
      let mut lender = crate::vcs_store::upload::ShardStores;
      let held = lender.hold(fetched, &owner());
      if let Some(held) = held.as_ref() {
        lender.wrote(held, 5_000);
      }
      let _ = sent.send(held.is_some());
    })
    .unwrap();
    slates_rt::futures::detach(task).unwrap();
  });
  assert!(
    done
      .recv_timeout(std::time::Duration::from_secs(60))
      .unwrap(),
    "held"
  );
  let row = crate::daemon::observe_first(&daemon, move |state| {
    state.vcs_usage.usage(&fetched).map(|row| {
      (
        row.fetch.answered,
        row.learned(Work::Fetch).bytes_per_work(),
      )
    })
  });
  daemon.stop();
  assert_eq!(row, Some((1, Some(5_000))));
}

/// Makes Mercurial repository `name`, empty, on the shard: its id.
fn hg_repository(state: &mut ShardState, name: &str) -> RepositoryId {
  use slates_ipc::protocol::{ObjectFormat, RequestBody, VcsTool};
  let reply = crate::test_store::create(
    state,
    &owner(),
    RequestBody::CreateRepository {
      name: name.to_owned(),
      tool: VcsTool::Mercurial,
      format: ObjectFormat::Sha1,
      default_branch: crate::repository::hg_push::line_name(b"default", 0),
      green: None,
    },
  );
  assert!(
    matches!(reply, slates_ipc::protocol::ReplyBody::GreenCreated { .. }),
    "{reply:?}"
  );
  state
    .db
    .partition()
    .repositories()
    .repository_named(name)
    .expect("made")
    .id
}

/// A-724 piece 3d for Mercurial (w-hg's leave, 2026-11-21). Do: make a Mercurial repository and hold its generation as a
/// fetch's streamed answer does (`hg_view::hold_in`), then ask a git repository's room while the hold lasts, and again
/// once it is given back. Expect: while the fetch is in flight, the other's room divided with it; once it ends, alone.
#[test]
fn t_a724_a_mercurial_fetch_in_flight_is_a_claim_on_the_room() {
  crate::test_store::audit_on_stored_shard("vcs_share", |state| {
    let fetched = hg_repository(state, "fetched");
    let toy = git_repository(state, "toy");
    let mut held_room = slates_hg::Admission::new(u64::MAX);
    let held = crate::vcs_store::hg_view::hold_in(state, fetched, &mut held_room).expect("held");
    let divided = room(state, toy);
    assert!(
      matches!(divided.bound, Bound::Share { .. }),
      "a Mercurial fetch in flight divides the room: {divided:?}"
    );
    drop(held);
    assert!(matches!(
      room(state, toy).bound,
      Bound::Shard | Bound::Snapshot { .. }
    ));
  });
}

/// A-724 piece 3d for Mercurial. Do: on a store-holding daemon, make a Mercurial repository; from a task on its shard,
/// hold its generation and give it back writing nothing (a batch's lookup), then hold it again, report 5,000 bytes
/// written by a streamed answer, and give it back. Expect: one fetch answered on the repository's usage row, its 5,000
/// bytes learned: the lookup's hold taught nothing.
#[test]
fn t_a724_a_mercurial_fetchs_answer_is_learned_and_a_lookups_hold_teaches_nothing() {
  use crate::vcs_http::hg::HgStores;
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let fetched = crate::daemon::observe_first(&daemon, |state| hg_repository(state, "fetched"));
  let (sent, done) = std::sync::mpsc::sync_channel(1);
  crate::daemon::observe_first(&daemon, move |_| {
    let task = slates_rt::futures::spawn(async move {
      let mut stores = crate::vcs_store::hg_view::ServedViews;
      let looked = stores.hold(fetched, &mut slates_hg::Admission::new(u64::MAX));
      let held_once = looked.is_some();
      drop(looked);
      let streamed = stores.hold(fetched, &mut slates_hg::Admission::new(u64::MAX));
      if let Some(held) = streamed.as_ref() {
        stores.wrote(held, 5_000);
      }
      let _ = sent.send(held_once && streamed.is_some());
    })
    .unwrap();
    slates_rt::futures::detach(task).unwrap();
  });
  assert!(
    done
      .recv_timeout(std::time::Duration::from_secs(60))
      .unwrap(),
    "held twice"
  );
  let row = crate::daemon::observe_first(&daemon, move |state| {
    state.vcs_usage.usage(&fetched).map(|row| {
      (
        row.fetch.answered,
        row.learned(Work::Fetch).bytes_per_work(),
      )
    })
  });
  daemon.stop();
  assert_eq!(row, Some((1, Some(5_000))));
}
