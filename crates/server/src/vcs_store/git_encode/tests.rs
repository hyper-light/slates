//! A-718 continued's tests: a fetch's bounded wait for its repository's encoding, and the task that finishes it.

use super::{BEHIND, Turned, UNITS_PAST_BUDGET, encode_within, turn};

/// A-718 continued (a fetch never waits past its window). Do: push the fixtures' `create` and `update` into a git
/// repository, leaving its versions unencoded; run a fetch's encoding with a wait of nothing; then let the shard run
/// until the encoding is idle. Expect: the wait ends with the encoding not idle and one fetch counted behind; the task
/// it started encodes every version (none unencoded), then lets the repository go from the set it keeps.
#[test]
fn t_a718_a_fetch_that_stops_waiting_leaves_a_task_that_finishes_the_encoding() {
  let daemon = crate::daemon::audit_daemon();
  let id = crate::daemon::observe_first(&daemon, |state| {
    let id = crate::git_push::tests::git_repository(state, "behind");
    crate::git_push::tests::pushed_from_the_fixtures(state, id, &["create", "update"]);
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
  // D-8's test-harness exception: the outcome is shared by two owners, the shard's task that writes it once and this
  // test thread that reads it.
  let done = std::sync::Arc::new(std::sync::Mutex::new(None));
  let daemon = crate::daemon::audit_daemon();
  let told = std::sync::Arc::clone(&done);
  crate::daemon::observe_first(&daemon, move |state| {
    let id = crate::git_push::tests::git_repository(state, "large");
    let task = async move {
      let outcome = drive(id, request).await;
      *told.lock().unwrap() = Some(outcome);
    };
    let _ = slates_rt::futures::detach(slates_rt::futures::spawn(task).unwrap());
  });
  // Derived: a debug build's push and encoding of 56 MB, looked at a second apart for an hour at most.
  let mut seen = None;
  for _ in 0..3_600 {
    if let Some(outcome) = done.lock().unwrap().take() {
      seen = Some(outcome);
      break;
    }
    std::thread::sleep(std::time::Duration::from_secs(1));
  }
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

/// Pushes `request` to repository `id` a step a poll, then turns its encoding a turn a poll until it is idle.
async fn drive(id: slates_db::catalog::RepositoryId, request: Vec<u8>) -> Driven {
  use crate::git_push::{Answering, ReceivePack, Unpresented};
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
  let mut answer = Vec::new();
  let ok = loop {
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
      Some(Some(ok)) => break ok,
      Some(None) => slates_rt::futures::yield_now().await,
      None => break false,
    }
  };
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
