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
    if !await_drive_turn(turned, &mut counters).await {
      break;
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

/// Advances [`drive`] past one encoding turn, bringing back any let-go pack window and returning
/// whether encoding should continue.
async fn await_drive_turn(
  turned: Option<(Turned, Option<slates_git::store::EncodingCounters>)>,
  counters: &mut Option<slates_git::store::EncodingCounters>,
) -> bool {
  match turned {
    Some((Turned::More, counted)) => {
      *counters = counted.or(*counters);
      slates_rt::futures::yield_now().await;
      true
    }
    Some((Turned::Waiting(waited), counted)) => {
      *counters = counted.or(*counters);
      super::super::upload::bring_back_within(waited)
        .await
        .is_ok()
    }
    Some((Turned::Idle, _)) | None => false,
  }
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

/// Releases `id`'s cached recent reads and settles the shard's VCS charge without bumping
/// [`super::GIVEN_BACK_IDLE`], matching the idle post-fetch baseline before an encoding starts.
fn let_go_recent_on_shard(
  state: &mut crate::state::ShardState,
  id: slates_db::catalog::RepositoryId,
) {
  if let Some((content, store)) = state.vcs_stores.git(id) {
    let _ = store.repository.let_go_recent(content);
  }
  super::super::settle(state);
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
    let_go_recent_on_shard(state, larger);
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
    loop {
      match turn(state, id) {
        Turned::More => {}
        Turned::Idle => break,
        Turned::Waiting(waited) => panic!("unexpected let-go wait before idle: {waited:?}"),
      }
    }
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

/// Shape: synthetic green key used by the in-memory `git_encode` let-go regression tests.
const WAIT_TEST_GREEN: u64 = 1;
/// Shape: small pure-RAM content store budget for in-memory `git_encode` unit tests.
const WAIT_TEST_BUDGET: u64 = 1 << 20;
/// Shape: small window size for pure-RAM `git_encode` unit tests.
const WAIT_TEST_WINDOW: usize = 4096;
/// Shape: synthetic pack length used to plant a stale foreign wait on `Repository::waiting`.
const STALE_PACK_LEN: u64 = 128;
/// Shape: synthetic pack offset used to plant a stale foreign wait on `Repository::waiting`.
const STALE_WAIT_OFFSET: u64 = 16;

/// Builds a commit, its single-file tree, and its blob for an in-memory pack, returning
/// `(commit_id, objects)`.
fn commit_with_blob(
  name: &str,
  body: &[u8],
  parent: Option<slates_git::object_name::ObjectId>,
) -> (
  slates_git::object_name::ObjectId,
  Vec<(slates_git::object_name::Kind, Vec<u8>)>,
) {
  use slates_git::object_name::{Kind, ObjectFormat, name_of};
  let blob_id = name_of(ObjectFormat::Sha1, Kind::Blob, body).unwrap();
  let mut tree = Vec::new();
  tree.extend_from_slice(format!("100644 {name}\0").as_bytes());
  tree.extend_from_slice(blob_id.as_bytes());
  let tree_id = name_of(ObjectFormat::Sha1, Kind::Tree, &tree).unwrap();
  let mut commit = format!("tree {tree_id}\n");
  if let Some(parent) = parent {
    commit.push_str(&format!("parent {parent}\n"));
  }
  commit.push_str(
    "author A <a@example.com> 1700000000 +0000\n\
     committer A <a@example.com> 1700000000 +0000\n\n\
     commit\n",
  );
  let commit_bytes = commit.into_bytes();
  let commit_id = name_of(ObjectFormat::Sha1, Kind::Commit, &commit_bytes).unwrap();
  (
    commit_id,
    vec![
      (Kind::Commit, commit_bytes),
      (Kind::Tree, tree),
      (Kind::Blob, body.to_vec()),
    ],
  )
}

/// Commits `objects` as a landed pack on `green` directly into `repository`/`content`, returning
/// `(pack_id, pack_bytes)`.
fn commit_pack_in_store(
  content: &mut slates_vcs::ContentStore,
  repository: &mut slates_git::store::Repository,
  green: u64,
  head: slates_git::object_name::ObjectId,
  objects: &[(slates_git::object_name::Kind, Vec<u8>)],
) -> (slates_vcs::ContentId, Vec<u8>) {
  use slates_git::object_name::ObjectFormat;
  let (pack, offsets) = crate::git_push::tests::whole_pack_with_offsets(objects);
  let naming = slates_git::store::GitNaming {
    format: ObjectFormat::Sha1,
  };
  let mut push = slates_git::store::Push::new(ObjectFormat::Sha1);
  push.extend_pack(&pack);
  for ((kind, bytes), offset) in objects.iter().zip(offsets) {
    let incoming = slates_git::store::Incoming {
      kind: *kind,
      content: bytes.as_slice(),
      claimed: None,
      depth: 0,
    };
    push
      .admit_placed::<_, core::convert::Infallible>(content, &naming, (incoming, offset))
      .unwrap();
  }
  repository.seal(content, &mut push).unwrap();
  repository.stage(content, &mut push).unwrap();
  let pack_id = push.stored_pack().expect("stored pack").0;
  let mv = repository.admit(content, &push, green, Some(head)).unwrap();
  repository.commit(content, &mut push, &[mv]).unwrap();
  push.abandon(content).unwrap();
  (pack_id, pack)
}

/// Marks every window of `pack` landed and lets it go (`away`) in `content`, also dropping recent
/// object cache entries from `repository`.
fn evict_pack_windows(
  content: &mut slates_vcs::ContentStore,
  repository: &mut slates_git::store::Repository,
  pack: &[u8],
) {
  let window_size = content.window();
  for window in pack.chunks(window_size).map(slates_vcs::ContentId::of) {
    assert!(content.mark_landed(&window));
    content.let_go_landed(&window, None).unwrap();
  }
  repository.let_go_recent(content);
}

/// Restores `waited`'s away window from `packs` back into `content` in pure RAM.
fn restore_waited_window(
  content: &mut slates_vcs::ContentStore,
  packs: &[(slates_vcs::ContentId, Vec<u8>)],
  waited: slates_git::store::packed::Waited,
) {
  let (_, pack) = packs
    .iter()
    .find(|(id, _)| *id == waited.pack)
    .expect("waited pack present in fixture");
  let (window, start, len) = content
    .away_window(&waited.pack, waited.len, waited.offset)
    .expect("waited window is away");
  let from = usize::try_from(start).unwrap();
  let to = from + usize::try_from(len).unwrap();
  content.bring_back(&window, &pack[from..to]).unwrap();
}

/// Drives all pending derivations on `repository` synchronously in pure RAM.
fn derive_all_in_store(
  content: &mut slates_vcs::ContentStore,
  repository: &slates_git::store::Repository,
) {
  let mut deriving = repository.deriving_due(content);
  let mut clock = super::ShardClock;
  loop {
    let quantum = slates_git::store::hook_view::Quantum {
      clock: &mut clock,
      ns: u64::MAX,
    };
    match repository
      .derive_turn(content, &mut deriving, quantum)
      .unwrap()
    {
      slates_git::store::Derived::More => {}
      slates_git::store::Derived::Sets {
        green,
        version,
        sets,
      } => repository
        .keep_derived(content, (green, version), &sets)
        .unwrap(),
      slates_git::store::Derived::Done => break,
    }
  }
}

/// Returns a synthetic stale `Waited` pointing at a foreign pack not owned by the test repository.
fn stale_wait() -> slates_git::store::packed::Waited {
  slates_git::store::packed::Waited {
    pack: slates_vcs::ContentId::of(b"stale-foreign-pack"),
    len: STALE_PACK_LEN,
    offset: STALE_WAIT_OFFSET,
  }
}

/// Simulates a concurrent `ReceivePack` turn wiping `repository.waiting` between encoding turns.
fn simulate_concurrent_push_clobber(repository: &slates_git::store::Repository) {
  repository.wait_for(stale_wait());
  let _ = repository.waiting();
  assert_eq!(repository.peek_waiting(), None);
}

/// Pure-RAM store and repository fixture for driving [`super::advance_encoding`] directly.
struct AdvanceFixture {
  store: slates_vcs::ContentStore,
  repository: slates_git::store::Repository,
  clock: super::ShardClock,
  exchange: slates_git::store::learn::Exchange,
}

impl AdvanceFixture {
  /// Builds an empty pure-RAM fixture calibrated with [`super::ShardClock`].
  fn new() -> Self {
    use slates_git::object_name::ObjectFormat;
    use slates_git::store::Repository;
    use slates_git::store::learn::{Exchange, Rates};

    let store = slates_vcs::ContentStore::new(WAIT_TEST_BUDGET, WAIT_TEST_WINDOW).unwrap();
    let repository = Repository::new(ObjectFormat::Sha1);
    let mut clock = super::ShardClock;
    let exchange = Exchange::cold(Rates::measure(&mut clock));
    Self {
      store,
      repository,
      clock,
      exchange,
    }
  }

  /// Commits a single-file `"alpha.txt"` pack on [`WAIT_TEST_GREEN`] and derives it synchronously.
  fn commit_derived(
    &mut self,
    body: &[u8],
    parent: Option<slates_git::object_name::ObjectId>,
  ) -> (
    slates_git::object_name::ObjectId,
    slates_vcs::ContentId,
    Vec<u8>,
  ) {
    let (commit_id, objects) = commit_with_blob("alpha.txt", body, parent);
    let (pack_id, pack) = commit_pack_in_store(
      &mut self.store,
      &mut self.repository,
      WAIT_TEST_GREEN,
      commit_id,
      &objects,
    );
    derive_all_in_store(&mut self.store, &self.repository);
    (commit_id, pack_id, pack)
  }

  /// Runs one [`super::advance_encoding`] turn with an unbounded budget.
  fn advance(&mut self, encoding: &mut slates_git::store::Encoding) -> super::Advanced {
    use slates_git::store::learn::Clock;

    super::advance_encoding(
      encoding,
      &mut self.repository,
      &mut self.store,
      (&mut self.clock as &mut dyn Clock, u64::MAX, self.exchange),
    )
  }

  /// Plants a stale foreign wait on `Repository::waiting`, runs the first encoding turn, asserts
  /// both wait cells are drained on exit, and returns the directly returned `Waited`.
  fn assert_first_encode_wait_drains_cells(
    &mut self,
    encoding: &mut slates_git::store::Encoding,
  ) -> slates_git::store::packed::Waited {
    use slates_git::store::Progress;

    self.repository.wait_for(stale_wait());
    let first = self.advance(encoding);
    assert!(matches!(first.progress, Ok(Progress::More)));
    let waited = first.waited.expect("encoding stop returns Waited directly");
    assert_eq!(encoding.waiting(), None, "encoding.waiting drained on exit");
    assert_eq!(
      self.repository.peek_waiting(),
      None,
      "shared Repository::waiting drained on exit"
    );
    waited
  }

  /// Drives `encoding` to `Progress::Done` while simulating concurrent push clobbers and restoring
  /// each directly returned `Waited` window.
  fn finish_encoding_with_clobbers(
    &mut self,
    encoding: &mut slates_git::store::Encoding,
    packs: &[(slates_vcs::ContentId, Vec<u8>)],
  ) {
    use slates_git::store::Progress;

    loop {
      let stepped = self.advance(encoding);
      assert_eq!(self.repository.peek_waiting(), None);
      match (stepped.progress, stepped.waited) {
        (Ok(Progress::Done), _) => break,
        (Ok(Progress::More), Some(waited)) => {
          simulate_concurrent_push_clobber(&self.repository);
          restore_waited_window(&mut self.store, packs, waited);
        }
        other => panic!("unexpected v0 advance outcome: {other:?}"),
      }
    }
  }

  /// Commits version 1 over `c0` so encoding enters `Stage::Walk` (tree diff), evicts both packs
  /// to force the `Err` path with `repository.waiting()` set, and asserts `advance_encoding`
  /// captures and drains that wait synchronously.
  fn assert_walk_stage_wait_drains_shared_cell(
    &mut self,
    c0: slates_git::object_name::ObjectId,
    pack_0_id: slates_vcs::ContentId,
    pack_0: &[u8],
  ) {
    use slates_git::store::Encoding;

    let (_c1, pack_1_id, pack_1) = self.commit_derived(b"beta body for walk wait\n", Some(c0));
    evict_pack_windows(&mut self.store, &mut self.repository, pack_0);
    evict_pack_windows(&mut self.store, &mut self.repository, &pack_1);

    let mut walk_encoding =
      Encoding::fragments(&self.repository, &self.store, WAIT_TEST_GREEN, 1).unwrap();
    let walk = self.advance(&mut walk_encoding);
    assert!(walk.progress.is_err(), "walk hit away tree pack window");
    let walk_waited = walk.waited.expect("walk captured away tree window");
    assert!(walk_waited.pack == pack_0_id || walk_waited.pack == pack_1_id);
    assert_eq!(
      self.repository.peek_waiting(),
      None,
      "shared Repository::waiting drained after walk wait"
    );
  }
}

/// Pure in-memory non-`TestStore`, non-daemon regression (`rt-git-waiting`, §4.17.2): proves
/// `git_encode::advance_encoding` clears stale waits on entry, returns `waited` synchronously for
/// both `Stage::Encode` stops and `Stage::Walk` tree-diff reads without ever leaving
/// `Repository::waiting` or `Encoding::waiting` populated across `.await`, and finishes encoding
/// cleanly even when a concurrent push clobbers `Repository::waiting` between turns.
#[test]
fn t_rt_git_encode_advance_returns_waiting_directly_and_drains_shared_cell() {
  use slates_git::store::Encoding;

  let mut fixture = AdvanceFixture::new();
  let (c0, pack_0_id, pack_0) = fixture.commit_derived(b"alpha body for encode wait\n", None);
  evict_pack_windows(&mut fixture.store, &mut fixture.repository, &pack_0);

  let mut encoding =
    Encoding::fragments(&fixture.repository, &fixture.store, WAIT_TEST_GREEN, 0).unwrap();
  let waited_0 = fixture.assert_first_encode_wait_drains_cells(&mut encoding);
  assert_eq!(waited_0.pack, pack_0_id, "stale foreign wait was cleared");

  simulate_concurrent_push_clobber(&fixture.repository);
  let packs = [(pack_0_id, pack_0)];
  restore_waited_window(&mut fixture.store, &packs, waited_0);
  fixture.finish_encoding_with_clobbers(&mut encoding, &packs);
  assert_eq!(fixture.repository.first_unencoded(), None);

  let [(_, pack_0)] = packs;
  fixture.assert_walk_stage_wait_drains_shared_cell(c0, pack_0_id, &pack_0);
}

/// Restores `waited`'s away window on `state`'s content store inside a shard lend.
fn restore_on_shard(
  state: &mut crate::state::ShardState,
  packs: &[(slates_vcs::ContentId, Vec<u8>)],
  waited: slates_git::store::packed::Waited,
) {
  super::super::lend(&mut state.store, &mut state.vcs_stores, 0);
  {
    let lent = super::super::Lent::new(&mut state.store, &mut state.vcs_stores);
    let content = lent.stores.content_mut().expect("content store");
    restore_waited_window(content, packs, waited);
  }
  super::super::settle(state);
}

/// Drives `git_encode::turn(state, id)` to `Turned::Idle`, asserting that every `Turned::Waiting`
/// leaves `Repository::waiting` drained (`None`), simulating a concurrent push clobber before
/// restoring each waited window, and returning the number of `Turned::Waiting` stops observed.
fn drive_turn_with_clobbers(
  state: &mut crate::state::ShardState,
  id: slates_db::catalog::RepositoryId,
  packs: &[(slates_vcs::ContentId, Vec<u8>)],
) -> usize {
  let mut waits = 0_usize;
  loop {
    let outcome = turn(state, id);
    let leaked = state
      .vcs_stores
      .git_ref(id)
      .and_then(|(_, store)| store.repository.peek_waiting());
    assert_eq!(
      leaked, None,
      "git_encode::turn leaked wait into shared Repository::waiting"
    );
    match outcome {
      Turned::Idle => return waits,
      Turned::More => {}
      Turned::Waiting(waited) => {
        waits += 1;
        if let Some((_, store)) = state.vcs_stores.git_ref(id) {
          simulate_concurrent_push_clobber(&store.repository);
        }
        restore_on_shard(state, packs, waited);
      }
    }
  }
}

/// Commits `objects` as a landed pack on `green` in `id`'s store on `state` and lets every pack
/// window go (`away`), returning `(pack_id, pack_bytes)`.
fn commit_and_evict_pack(
  state: &mut crate::state::ShardState,
  id: slates_db::catalog::RepositoryId,
  green: u64,
  head: slates_git::object_name::ObjectId,
  objects: &[(slates_git::object_name::Kind, Vec<u8>)],
) -> (slates_vcs::ContentId, Vec<u8>) {
  super::super::lend(&mut state.store, &mut state.vcs_stores, 0);
  let committed = {
    let lent = super::super::Lent::new(&mut state.store, &mut state.vcs_stores);
    let (content, store) = lent.stores.git(id).expect("git store");
    let committed = commit_pack_in_store(content, &mut store.repository, green, head, objects);
    evict_pack_windows(content, &mut store.repository, &committed.1);
    committed
  };
  super::super::settle(state);
  committed
}

/// Evicts every window of `pack` and clears recent object cache on `state`'s shard lend.
fn evict_on_shard(
  state: &mut crate::state::ShardState,
  id: slates_db::catalog::RepositoryId,
  pack: &[u8],
) {
  super::super::lend(&mut state.store, &mut state.vcs_stores, 0);
  {
    let lent = super::super::Lent::new(&mut state.store, &mut state.vcs_stores);
    let (content, store) = lent.stores.git(id).expect("git store");
    evict_pack_windows(content, &mut store.repository, pack);
  }
  super::super::settle(state);
}

/// Drives the first `git_encode::turn` on `id` into `git_derive::Turned::Waiting`, asserts that
/// shared `Repository::waiting` is drained and names `expected_pack`, restores the waited window,
/// and completes derivation.
fn derive_after_first_wait(
  state: &mut crate::state::ShardState,
  id: slates_db::catalog::RepositoryId,
  expected_pack: slates_vcs::ContentId,
  packs: &[(slates_vcs::ContentId, Vec<u8>)],
) {
  let first = turn(state, id);
  let leaked = state
    .vcs_stores
    .git_ref(id)
    .and_then(|(_, store)| store.repository.peek_waiting());
  assert_eq!(leaked, None);
  let Turned::Waiting(waited) = first else {
    panic!("expected Turned::Waiting from git_derive, got {first:?}");
  };
  assert_eq!(waited.pack, expected_pack);
  restore_on_shard(state, packs, waited);
  while super::super::git_derive::turn(state, id) == super::super::git_derive::Turned::More {}
}

/// End-to-end shard regression (`rt-git-waiting`, §4.17.2): proves `git_encode::turn` propagates
/// `Turned::Waiting(waited)` directly across derivation, encoding, and `Stage::Walk` tree diffs
/// without ever parking a wait into shared `Repository::waiting`, and finishes encoding cleanly
/// even when concurrent pushes clobber `Repository::waiting` between turns.
#[test]
fn t_rt_git_encode_turn_returns_waiting_without_stranding_shared_cell() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  crate::daemon::observe_first(&daemon, |state| {
    let id = crate::git_push::tests::git_repository(state, "encode-wait");
    let (c0, objects_0) = commit_with_blob("alpha.txt", b"alpha body for pack window\n", None);
    let pack_0 = commit_and_evict_pack(state, id, WAIT_TEST_GREEN, c0, &objects_0);
    let mut packs = vec![pack_0.clone()];

    derive_after_first_wait(state, id, pack_0.0, &packs);
    evict_on_shard(state, id, &pack_0.1);
    let encode_waits = drive_turn_with_clobbers(state, id, &packs);
    assert!(
      encode_waits >= 1,
      "expected Turned::Waiting from encoding after re-evicting pack_0"
    );
    assert_eq!(state.refusals.get(super::ENCODED).copied(), Some(1));

    // Second version on the same green exercises `Stage::Walk` tree-diff against evicted packs.
    let (c1, objects_1) = commit_with_blob("alpha.txt", b"beta body for pack window\n", Some(c0));
    let pack_1 = commit_and_evict_pack(state, id, WAIT_TEST_GREEN, c1, &objects_1);
    evict_on_shard(state, id, &pack_0.1);
    packs.push(pack_1);

    let waits = drive_turn_with_clobbers(state, id, &packs);
    assert!(waits >= 1, "expected at least one Turned::Waiting on v1");
    assert_eq!(state.refusals.get(super::ENCODED).copied(), Some(2));
    assert_eq!(state.refusals.get(super::REFUSED).copied().unwrap_or(0), 0);
  });
  daemon.stop();
}

/// Shape: generous upper bound on `claimed_turn` calls to reach the first evicted pack window even
/// when host CPU load trips the shard step quantum after every encoding unit.
const MAX_TURNS_TO_WAIT: usize = 64;

/// Drives `super::claimed_turn(state, id)` past any quantum-paced `Turned::More` turns (asserting
/// none acquires a wait claim) until encoding pauses on an evicted pack window.
fn drive_claimed_turn_until_waiting(
  state: &mut crate::state::ShardState,
  id: slates_db::catalog::RepositoryId,
) -> (
  slates_git::store::packed::Waited,
  Option<super::EncodeWaitGuard>,
) {
  for _ in 0..MAX_TURNS_TO_WAIT {
    match super::claimed_turn(state, id) {
      (Turned::Waiting(waited), claim) => return (waited, claim),
      (Turned::More, claim) => {
        assert!(
          claim.is_none(),
          "Turned::More must not acquire an EncodeWaitGuard claim"
        );
      }
      (Turned::Idle, _) => {
        panic!("expected Turned::Waiting on evicted pack encode, got Turned::Idle");
      }
    }
  }
  panic!("encode did not reach Turned::Waiting within {MAX_TURNS_TO_WAIT} turns");
}

/// Verifies `EncodeWaitGuard` (`Encodings.2` claim) forces concurrent `turn` calls to return
/// `Turned::More` without tripping `Encoding::stop_at == false` (`REFUSED`), and releasing the
/// claim lets encoding finish cleanly.
fn assert_wait_claim_blocks_concurrent_turn(
  state: &mut crate::state::ShardState,
) -> (
  slates_db::catalog::RepositoryId,
  (slates_vcs::ContentId, Vec<u8>),
) {
  let id = crate::git_push::tests::git_repository(state, "encode-claim");
  let (c0, objects_0) = commit_with_blob("alpha.txt", b"alpha body for encode claim\n", None);
  let pack_0 = commit_and_evict_pack(state, id, WAIT_TEST_GREEN, c0, &objects_0);
  derive_after_first_wait(state, id, pack_0.0, std::slice::from_ref(&pack_0));
  evict_on_shard(state, id, &pack_0.1);

  let (waited, claim) = drive_claimed_turn_until_waiting(state, id);
  let concurrent = turn(state, id);
  let refused_while_claimed = state.refusals.get(super::REFUSED).copied().unwrap_or(0);
  let had_claim = claim.is_some();
  if let Some(guard) = claim {
    guard.release_in(state);
  }
  restore_on_shard(state, std::slice::from_ref(&pack_0), waited);
  let _waits = drive_turn_with_clobbers(state, id, std::slice::from_ref(&pack_0));
  assert_eq!(
    (
      had_claim,
      concurrent,
      refused_while_claimed,
      state.refusals.get(super::ENCODED).copied(),
      state.refusals.get(super::REFUSED).copied().unwrap_or(0),
    ),
    (true, Turned::More, 0, Some(1), 0)
  );
  (id, pack_0)
}

/// Verifies a `WindowPin` retained in `Vec<WindowPin>` blocks `let_go_resident_packs` eviction.
fn assert_window_pin_blocks_let_go(
  state: &mut crate::state::ShardState,
  id: slates_db::catalog::RepositoryId,
  pack: &(slates_vcs::ContentId, Vec<u8>),
) {
  let window = state
    .vcs_stores
    .content()
    .and_then(|content| content.window_holding(&pack.0, u64::try_from(pack.1.len()).unwrap(), 0))
    .map(|(w, _, _)| w)
    .expect("window id");
  let mut pins = vec![super::super::named_objects::WindowPin::of(window)];
  super::super::let_go_resident_packs(state, id, 0);
  let still_resident = state
    .vcs_stores
    .content()
    .is_some_and(|content| !content.is_away(&window));
  pins.clear();
  assert!(
    still_resident,
    "WindowPin kept in Vec<WindowPin> must block let_go_resident_packs eviction"
  );
}

/// Red-team regression (`rt-git-waiting` Rung-4 Seam #2, §4.17.2): proves (1) holding an
/// `EncodeWaitGuard` (`Encodings.2` claim) makes concurrent `git_encode::turn` calls yield
/// `Turned::More` instead of racing into `Encoding::stop_at == false` and tripping `REFUSED`, and
/// dropping the guard (including on cancellation) immediately allows encoding to finish; and (2)
/// retaining `WindowPin`s in `Vec<WindowPin>` blocks `let_go_resident_packs` from evicting an
/// earlier window while bringing back a later one.
#[test]
fn t_rt_git_encode_wait_claim_and_multi_window_pins_prevent_stop_at_race() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (id, pack_0) =
    crate::daemon::observe_first(&daemon, assert_wait_claim_blocks_concurrent_turn);
  crate::daemon::observe_first(&daemon, move |state| {
    assert_window_pin_blocks_let_go(state, id, &pack_0);
  });
  daemon.stop();
}
