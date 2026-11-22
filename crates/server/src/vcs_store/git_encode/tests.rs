//! A-718 continued's tests: a fetch's bounded wait for its repository's encoding, and the task that finishes it.

use super::{AHEAD_STOPPED, BEHIND, Turned, UNITS_PAST_BUDGET, encode_within, turn};

/// A-718 continued (a fetch never waits past its window), A-856 (made ahead only while a fetch is answered, within the
/// repository's learned grant). Do: push the fixtures' `create` and `update` into a git repository, leaving its versions
/// unencoded; answer a fetch of it (its guard held live, a step lent so its curve and grant are learned); run a fetch's
/// encoding with a wait of nothing; then let the shard run until the encoding is idle; then let the fetch end. Expect:
/// the wait ends with the encoding not idle and one fetch counted behind; the task it started encodes every version
/// (none unencoded), then lets the repository go from the set it keeps.
#[test]
fn t_a718_a_fetch_that_stops_waiting_leaves_a_task_that_finishes_the_encoding() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let id = crate::daemon::observe_first(&daemon, |state| {
    let id = crate::git_push::tests::git_repository(state, "behind");
    crate::git_push::tests::pushed_from_the_fixtures(state, id, &["create", "update"]);
    answered_live(state, id);
    let unencoded = state
      .vcs_stores
      .git_ref(id)
      .and_then(|(_, store)| store.repository.first_unencoded());
    assert!(unencoded.is_some(), "the pushes left versions unencoded");
    let waited = slates_rt::futures::spawn(async move {
      assert!(
        !encode_within(id, 0).await,
        "a wait of nothing ends before the encoding"
      );
    })
    .unwrap();
    let _ = slates_rt::futures::detach(waited);
    id
  });
  let mut done = None;
  // Derived: 600 looks 20 ms apart, the wait the encoding is given to end.
  let deadline =
    std::time::Instant::now() + std::time::Duration::from_millis(20).saturating_mul(600);
  while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
    let seen = crate::daemon::observe_first(&daemon, move |state| {
      let unencoded = state
        .vcs_stores
        .git_ref(id)
        .and_then(|(_, store)| store.repository.first_unencoded());
      (
        state.refusals.get(BEHIND).copied().unwrap_or(0),
        unencoded,
        state.vcs_stores.encodings.1.contains(&id),
      )
    });
    if seen.0 > 0 && seen.1.is_none() && !seen.2 {
      done = Some(seen);
      break;
    }
    std::thread::park_timeout(std::time::Duration::from_millis(20).min(left));
  }
  daemon.stop();
  assert_eq!(done, Some((1, None, false)));
}

/// Shape: how long a test's fetch is answered (held live), past every wait the tests here give the shard.
const ANSWERED_NS: u64 = 60_000_000_000;

/// A fetch of repository `id` answered now, as an edge's would be: a clone of `main` answered whole first (its reads
/// teach its usage row's curve, so its grant is learned from them), and its guard counted live by a task of this shard
/// for [`ANSWERED_NS`].
fn answered_live(state: &mut crate::state::ShardState, id: slates_db::catalog::RepositoryId) {
  let fetching = crate::vcs_share::Live::fetch(id);
  let (_, refused) = crate::vcs_store::upload::tests::answered(
    state,
    (id, &crate::git_push::tests::owner()),
    slates_git::upload::Asked::Request(&crate::vcs_store::upload::tests::fetch(
      &[],
      &["refs/heads/main"],
    )),
  );
  assert_eq!(refused, None, "the clone answered");
  let answering = slates_rt::futures::spawn(async move {
    let _fetching = fetching;
    let _ = slates_rt::futures::sleep(ANSWERED_NS).await;
  })
  .expect("a task");
  let _ = slates_rt::futures::detach(answering);
}

/// A-856 (main's ruling of 2026-11-22 00:30: "nothing grows while no client is connected"; clients measured git/git's
/// clone at 917 MiB as it ended and 1,695 MiB 90 s later with no client, the task behind it still encoding). Do: push the
/// fixtures' `create` and `update`, leaving its versions unencoded; with no fetch of it answered, run a fetch's encoding
/// with a wait of nothing, as a client that went away leaves it; then let the shard run. Expect: the task behind it stops
/// at its first turn, counted `ahead_stopped`, and lets the repository go from the set it keeps; versions are left
/// unencoded and no encoding is held under way, so nothing grows until a fetch asks again. Red before: the task encoded
/// every version with no fetch there.
#[test]
fn t_a856_with_no_fetch_answered_nothing_is_encoded_ahead() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let id = crate::daemon::observe_first(&daemon, |state| {
    let id = crate::git_push::tests::git_repository(state, "nobody");
    crate::git_push::tests::pushed_from_the_fixtures(state, id, &["create", "update"]);
    let waited = slates_rt::futures::spawn(async move {
      let _ = encode_within(id, 0).await;
    })
    .unwrap();
    let _ = slates_rt::futures::detach(waited);
    id
  });
  let mut seen = None;
  // Derived: 600 looks 20 ms apart, the wait the task is given to stop.
  let deadline =
    std::time::Instant::now() + std::time::Duration::from_millis(20).saturating_mul(600);
  while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
    let now = crate::daemon::observe_first(&daemon, move |state| {
      let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
      (
        count(BEHIND),
        count(AHEAD_STOPPED),
        super::ahead_under_way(state, id),
        state.vcs_stores.encodings.0.contains_key(&id),
        state
          .vcs_stores
          .git_ref(id)
          .and_then(|(_, store)| store.repository.first_unencoded())
          .is_some(),
      )
    });
    if now.0 > 0 && !now.2 {
      seen = Some(now);
      break;
    }
    std::thread::park_timeout(std::time::Duration::from_millis(20).min(left));
  }
  daemon.stop();
  assert_eq!(seen, Some((1, 1, false, false, true)));
}

/// A-718 (§4.3's step budget; found by use by w-interop: a 56 MB push of git/git cut at v2.0.0 left the shard spinning
/// "3534 ms of CPU without a step" in the encoding's task, at its poll 162167, and the daemon restarting into the same
/// encoding). Do: on a daemon's shard, in a task as an edge's would run it, push the pack `SLATES_ENCODE_PACK` names
/// (git/git's v2.0.0, 55.9 MB: `echo v2.0.0 | git pack-objects --revs --stdout`) as `main` at `SLATES_ENCODE_HEAD`, a
/// step a poll; then turn the repository's encoding a turn a poll until it is idle, as the task that finishes it does.
/// Expect: the daemon lives (the watchdog finds no poll spinning: before this, one unit, an object's whole delta
/// trials and level samples, spun the shard 3,508 ms and 3,541 ms in two runs, and a unit read 32,768 rows in 315 ms);
/// and no unit of the encoding reads, searches, deflates or puts more than one content window, nor reads more than one
/// row (units held by their work, not by debug wall time, as A-751's are; the time past the quantum is printed, the
/// by-use signal in release).
#[test]
#[ignore = "needs SLATES_ENCODE_PACK and SLATES_ENCODE_HEAD: a 56 MB pack and its head"]
fn a718_no_unit_of_a_56_mb_packs_encoding_outgrows_the_step_quantum() {
  let (Some(path), Some(head)) = (
    std::env::var_os("SLATES_ENCODE_PACK"),
    std::env::var("SLATES_ENCODE_HEAD").ok(),
  ) else {
    panic!("set SLATES_ENCODE_PACK and SLATES_ENCODE_HEAD");
  };
  let pack = std::fs::read(path).expect("the pack");
  let command = format!("{} {head} refs/heads/main\0report-status\n", "0".repeat(40));
  let mut request = Vec::new();
  request.extend_from_slice(format!("{:04x}", command.len() + 4).as_bytes());
  request.extend_from_slice(command.as_bytes());
  request.extend_from_slice(b"0000");
  request.extend_from_slice(&pack);
  drop(pack);
  // The shard's task sends the outcome once; this test thread waits for it.
  let (told, done) = std::sync::mpsc::channel();
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  crate::daemon::observe_first(&daemon, move |state| {
    let id = crate::git_push::tests::git_repository(state, "large");
    let task = async move {
      let _ = told.send(drive(id, request).await);
    };
    let _ = slates_rt::futures::detach(slates_rt::futures::spawn(task).unwrap());
  });
  // Derived: a debug build's push and encoding of 56 MB, an hour at most.
  let seen = done
    .recv_timeout(std::time::Duration::from_secs(3_600))
    .ok();
  daemon.stop();
  let (ok, past, turns, longest, quantum, counters) =
    seen.expect("the push and its encoding ended");
  eprintln!(
    "pushed ok {ok}; quantum {quantum} ns, {turns} turns, longest turn {longest} ns, units past {past}, {counters:?}"
  );
  assert!(ok, "the push was taken");
  let (counters, window) = counters.expect("the encoding's counters");
  assert!(counters.objects > 0, "objects encoded: {counters:?}");
  assert!(
    counters.most_bytes_a_unit <= window,
    "a unit's bytes past a window of {window}: {counters:?}"
  );
  assert!(counters.most_rows_a_unit <= 1, "rows a unit: {counters:?}");
}

/// What [`drive`] found: whether the push was taken, the units past the quantum, the encoding's turns, its longest turn,
/// the quantum, and the encoding's counters before its last turn.
type Driven = (
  bool,
  u64,
  u64,
  u64,
  u64,
  Option<(slates_git::store::EncodingCounters, u64)>,
);

/// `push` answered a step a poll, its carries awaited between: whether every ref it updated was accepted.
async fn answered(push: &mut crate::git_push::ReceivePack) -> bool {
  use crate::git_push::Answering;
  let mut answer = Vec::new();
  loop {
    let step =
      crate::state::with_state_counted(|state| match push.answer_step(state, &mut answer) {
        Ok(Answering::Answered(received)) => Some(
          received
            .outcomes
            .iter()
            .all(|outcome| matches!(outcome, slates_git::receive::Outcome::Ok(_))),
        ),
        Ok(_) => None,
        Err(_) => Some(false),
      });
    match step {
      Some(Some(ok)) => return ok,
      Some(None) => {
        slates_rt::futures::yield_now().await;
        if crate::git_push::tests::carry_awaited(push).await.is_err() {
          return false;
        }
      }
      None => return false,
    }
  }
}

/// Pushes `request` to repository `id` a step a poll, then turns its encoding a turn a poll until it is idle.
async fn drive(id: slates_db::catalog::RepositoryId, request: Vec<u8>) -> Driven {
  use crate::git_push::{ReceivePack, Unpresented};
  let started = crate::state::with_state_counted(|state| {
    let mut advertised = Vec::new();
    ReceivePack::start(
      state,
      (&crate::git_push::tests::owner(), id),
      true,
      &mut advertised,
      Unpresented,
    )
    .ok()
  });
  let Some(Some(mut push)) = started else {
    return (false, 0, 0, 0, 0, None);
  };
  for piece in request.chunks(1 << 16) {
    let _ = crate::state::with_state_counted(|state| push.input(state, piece).is_ok());
    slates_rt::futures::yield_now().await;
  }
  drop(request);
  let _ = crate::state::with_state_counted(|state| push.end(state).is_ok());
  let ok = answered(&mut push).await;
  let quantum =
    crate::state::with_state_counted(|state| state.config.step_quantum_ns()).unwrap_or(0);
  let (mut turns, mut longest, mut counters) = (0u64, 0u64, None);
  let mut stage = None;
  loop {
    let began = slates_rt::futures::now_ns();
    let turned = crate::state::with_state_counted(|state| {
      let turned = turn(state, id);
      let counted = state
        .vcs_stores
        .encodings
        .0
        .get(&id)
        .map(|(encoding, _)| (encoding.counters(), encoding.stage_index()));
      (turned, counted)
    });
    let staged = turned
      .as_ref()
      .and_then(|(_, counted)| counted.map(|(_, stage)| stage));
    if staged != stage || turns % 50_000 == 0 {
      eprintln!(
        "turn {turns}: stage {staged:?}, {:?}",
        turned.as_ref().and_then(|(_, counted)| *counted)
      );
      stage = staged;
    }
    let turned = turned.map(|(turned, counted)| (turned, counted.map(|(counters, _)| counters)));
    let took = slates_rt::futures::now_ns().saturating_sub(began);
    turns += 1;
    if took > longest.saturating_mul(2).max(quantum.saturating_mul(100)) {
      eprintln!("turn {turns} took {took} ns, counters {counters:?}");
    }
    longest = longest.max(took);
    match turned {
      Some((Turned::More, counted)) => {
        if counted.is_some() {
          counters = counted;
        }
        slates_rt::futures::yield_now().await;
      }
      Some((Turned::Idle, _)) | None => break,
    }
  }
  let past = crate::state::with_state_counted(|state| {
    state.refusals.get(UNITS_PAST_BUDGET).copied().unwrap_or(0)
  })
  .unwrap_or(0);
  let window = crate::state::with_state_counted(|state| {
    state.vcs_stores.content.as_ref().map_or(0, |content| {
      u64::try_from(content.window()).unwrap_or(u64::MAX)
    })
  })
  .unwrap_or(0);
  (
    ok,
    past,
    turns,
    longest,
    quantum,
    counters.map(|counters| (counters, window)),
  )
}

/// Shape: [`t_a856_a_grant_is_learned_from_reads_and_given_back_when_idle`]'s larger repository's one file: a few of the
/// store's windows (256 KiB here), more than the fixtures' clone reads, and small enough that a debug build pushes and
/// encodes it within one poll.
const LARGER_FILE: usize = 1 << 20;

/// [`LARGER_FILE`] bytes no deflate shrinks (a xorshift stream, Marsaglia 2003), so its fragments are as large as it.
fn incompressible() -> Vec<u8> {
  let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
  (0..LARGER_FILE)
    .map(|_| {
      state ^= state << 13;
      state ^= state >> 7;
      state ^= state << 17;
      state.to_le_bytes()[0]
    })
    .collect()
}

/// What repository `id`'s fetch of `main` is answered with `arguments` besides, whole: asserted not refused.
fn fetched_main(
  state: &mut crate::state::ShardState,
  id: slates_db::catalog::RepositoryId,
  arguments: &[&str],
) {
  let mut asked = vec!["want-ref refs/heads/main".to_owned()];
  asked.extend(arguments.iter().map(|argument| (*argument).to_owned()));
  asked.extend(["no-progress".to_owned(), "done".to_owned()]);
  let (_, refused) = crate::vcs_store::upload::tests::answered(
    state,
    (id, &crate::git_push::tests::owner()),
    slates_git::upload::Asked::Request(&crate::vcs_store::upload::tests::request("fetch", &asked)),
  );
  assert_eq!(refused, None);
}

/// A-856 (main's check of 2026-11-22 02:29: a grant learned, not a floor, a cap or a region's size passed through; and
/// what is held given back when idle). Do: clone the fixtures' small repository whole, and a repository of one
/// incompressible [`LARGER_FILE`]-byte file whole, reading each one's grant (`vcs_share::budget`: the shard's room
/// divided by the repositories' curves, each curve the content its fetches read); then
/// begin the larger's version's encoding, turning it until its parts hold bytes, as a fetch that went away leaves it;
/// then let the repository go idle. Expect: the larger's grant past the smaller's, each the bytes its clone's reads
/// touched (an object's last window counting its own length); the encoding's parts charged to the content store while
/// held; and, idle, the encoding dropped, the store's charge back at its figure before the encoding began, counted
/// `given_back_idle`.
#[test]
fn t_a856_a_grant_is_learned_from_reads_and_given_back_when_idle() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let seen = crate::daemon::observe_first(&daemon, |state| {
    let small = crate::git_push::tests::git_repository(state, "small");
    crate::git_push::tests::pushed_from_the_fixtures(state, small, &["create", "update"]);
    let larger = crate::git_push::tests::git_repository(state, "larger");
    let (create, _) = crate::git_push::tests::create_of_file(incompressible());
    let received = crate::git_push::tests::pushed(state, larger, &create);
    assert!(
      received
        .outcomes
        .iter()
        .all(|outcome| matches!(outcome, slates_git::receive::Outcome::Ok(_)))
    );
    fetched_main(state, small, &[]);
    fetched_main(state, larger, &[]);
    let grants = (
      crate::vcs_share::budget(state, small).unwrap(),
      crate::vcs_share::budget(state, larger).unwrap(),
    );
    while crate::vcs_store::git_derive::turn(state, larger)
      == crate::vcs_store::git_derive::Turned::More
    {}
    let idle_charged = state
      .vcs_stores
      .content()
      .map_or(0, |content| content.charged());
    super::super::lend(&mut state.store, &mut state.vcs_stores, 0);
    let exchange = super::exchange(state);
    let (held, charged) = {
      // Read through a lend: the pack's windows lie in the shard's chunk store.
      let lent = super::super::Lent::new(&mut state.store, &mut state.vcs_stores);
      let (content, store) = lent.stores.git(larger).expect("the store");
      let (green, version) = store
        .repository
        .first_unencoded()
        .expect("a version unencoded");
      let mut encoding =
        slates_git::store::Encoding::fragments(&store.repository, content, green, version).unwrap();
      let mut clock = super::ShardClock;
      while encoding.held_bytes() == 0 {
        let turned = encoding.advance(
          &mut store.repository,
          content,
          slates_git::store::Turn {
            clock: &mut clock,
            exchange,
            quantum_ns: 0,
          },
        );
        assert_eq!(turned, Ok(slates_git::store::Progress::More));
      }
      let held = encoding.held_bytes();
      let charged = content.charged();
      lent
        .stores
        .encodings
        .0
        .insert(larger, (encoding, (green, version)));
      (held, charged)
    };
    super::super::settle(state);
    super::give_back_idle(state, larger);
    let after = state
      .vcs_stores
      .content()
      .map_or(0, |content| content.charged());
    let given = state
      .refusals
      .get(super::GIVEN_BACK_IDLE)
      .copied()
      .unwrap_or(0);
    let under_way = state.vcs_stores.encodings.0.contains_key(&larger);
    (
      grants,
      (idle_charged, held, charged, after, given, under_way),
    )
  });
  daemon.stop();
  let ((small, larger), (idle_charged, held, charged, after, given, under_way)) = seen;
  assert!(larger > small, "learned from reads: {small} then {larger}");
  assert!(
    charged >= idle_charged + held,
    "held parts charged: {charged} past {idle_charged}, {held} held"
  );
  // What it gave back is the parts it held and the objects its reads kept (A-860), so at least the parts.
  assert_eq!(
    (after, under_way),
    (idle_charged, false),
    "given back when idle"
  );
  assert!(given >= held, "given back {given}, the parts {held}");
}

/// A-860 (main's 04:24 note: `git.recent_bytes`, a repository's objects read lately, held at its budget, 53.9 MB, after a
/// git/git v2.0.0 clone had ended, 0.2 MB before it; A-856's rule: given back when idle). Do: push the fixtures'
/// `create` and `update` (their objects read where they lie in their packs); read what the repository's reads keep;
/// encode its versions as a fetch's wait does (its reads keep the objects they read); then hold its generation as a
/// fetch does and let the fetch end from a task of the shard, as an edge's request ends. Expect: the encoding's reads
/// kept more than before; once the fetch ended, what they keep back at or below its figure before, counted
/// `given_back_idle`. Red before: held at what the encoding left.
#[test]
fn t_a860_a_repository_gone_idle_lets_go_of_the_objects_its_reads_keep() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (id, before, during) = crate::daemon::observe_first(&daemon, |state| {
    let id = crate::git_push::tests::git_repository(state, "idle-reads");
    crate::git_push::tests::pushed_from_the_fixtures(state, id, &["create", "update"]);
    let recent = |state: &crate::state::ShardState| {
      state
        .vcs_stores
        .git_ref(id)
        .map_or(0, |(_, store)| store.repository.recent_bytes())
    };
    let before = recent(state);
    while turn(state, id) == Turned::More {}
    let during = recent(state);
    let held = crate::vcs_store::upload::hold_in(state, id, &crate::git_push::tests::owner())
      .expect("a fetch's hold");
    let ended = slates_rt::futures::spawn(async move { drop(held) }).expect("a task");
    let _ = slates_rt::futures::detach(ended);
    (id, before, during)
  });
  let after = crate::daemon::observe_first(&daemon, move |state| {
    (
      state
        .vcs_stores
        .git_ref(id)
        .map_or(0, |(_, store)| store.repository.recent_bytes()),
      state
        .refusals
        .get(super::GIVEN_BACK_IDLE)
        .copied()
        .unwrap_or(0),
    )
  });
  daemon.stop();
  assert!(
    during > before,
    "the encoding's reads kept nothing: {before} then {during}"
  );
  assert!(
    after.0 <= before && after.1 > 0,
    "after the fetch: {after:?}, before {before}"
  );
}
