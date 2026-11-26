//! The refinement job as the shard's own task runs it ([`super::job`], A-787 part 2): a recorded submit parked on its
//! refinement until the task's turns end, and a base whose windows were let go brought back and copied again (A-760 (b)
//! 4d). The test-only driver that took the turns on a held shard (`refined_now`) is gone: it gave such a job up (main's
//! ruling of 2026-11-26).

use slates_ipc::protocol::{ReplyBody, RequestBody, VolumeId};
use slates_wire::request::{RequestId, Seen};

use crate::state::ShardState;

/// Shape: how long the history may run on its shard before the test fails.
const PATIENCE: std::time::Duration = std::time::Duration::from_secs(120);
/// Shape: the client id this module's recorded requests carry.
const TEST_CLIENT: u32 = 9_002;
/// Shape: the lines of the file each work rewrites whole: enough that its refinement copies and refines over turns.
const LINES: u32 = 2_000;

/// The shard's state, borrowed for one step of the history.
fn on<R>(step: impl FnOnce(&mut ShardState) -> R) -> R {
  crate::state::with_state(step).expect("the shard's state")
}

/// `body` asked of `state` as the owner, outside a recorded request.
fn ask(state: &mut ShardState, body: RequestBody) -> ReplyBody {
  crate::test_store::create(state, &crate::vcs_store::tests::owner(), body)
}

/// The file the green holds, a generated function a line pair.
fn original() -> Vec<u8> {
  (0..LINES)
    .map(|line| format!("fn f{line}(a: u32) -> u32 {{\n  a + {line}\n}}\n"))
    .collect::<String>()
    .into_bytes()
}

/// [`original`] re-indented: what a formatter writes, a whole-file rewrite.
fn reindented() -> Vec<u8> {
  (0..LINES)
    .map(|line| format!("fn f{line}(a: u32) -> u32 {{\n    a + {line}\n}}\n"))
    .collect::<String>()
    .into_bytes()
}

/// A work on `green` named `name` whose file `/f` has its first `delete_len` bytes replaced by `bytes`: created, from
/// none; replaced whole, from all of it.
fn work_writing(
  state: &mut ShardState,
  (green, name): (VolumeId, &str),
  (delete_len, bytes): (u64, Vec<u8>),
) -> VolumeId {
  let made = ask(
    state,
    RequestBody::CreateWork {
      green,
      name: name.to_owned(),
    },
  );
  let ReplyBody::WorkCreated { id: work, .. } = made else {
    panic!("no work: {made:?}")
  };
  let edited = ask(
    state,
    RequestBody::Edit {
      work,
      path: "/f".to_owned(),
      at: 0,
      delete_len,
      bytes,
    },
  );
  assert!(!matches!(edited, ReplyBody::Refused { .. }), "{edited:?}");
  work
}

/// `work` submitted as a client's recorded request `sequence`: its answer now, if it was not parked.
fn submit(state: &mut ShardState, work: VolumeId, sequence: u32) -> (RequestId, Option<ReplyBody>) {
  let id = RequestId {
    client: TEST_CLIENT,
    sequence,
  };
  let answered = crate::verbs::run_recorded_for_test(
    state,
    (u64::from(TEST_CLIENT), id),
    &crate::vcs_store::tests::owner(),
    crate::identities::authored(work, "rewrite"),
  );
  (id, answered)
}

/// The answer recorded for `id`, once it completed.
fn completed(state: &ShardState, id: RequestId) -> Option<ReplyBody> {
  match state
    .db
    .partition()
    .completion(u64::from(TEST_CLIENT), id.client, id.sequence)
  {
    Seen::Completed(bytes) => <ReplyBody as slates_wire::Wire>::from_bytes(&bytes).ok(),
    _ => None,
  }
}

/// What a work's finished refinement kept: the refined document and plan.
type Kept = Option<(
  slates_merge::ops_doc::OpsDoc,
  slates_merge::increment::PostStatePlan,
)>;

/// `work`'s kept refinement.
fn kept_of(state: &ShardState, work: VolumeId) -> Kept {
  state
    .refining
    .done
    .get(&crate::verbs::to_db_volume(work))
    .and_then(|done| done.refined.clone())
}

/// The counters a job moves whatever its timing (started, gave up, prepared, its check, a mismatch), and the turns jobs
/// took in all.
type Counts = ([u64; 5], u64);

/// The shard's [`Counts`] now.
fn counted(state: &ShardState) -> Counts {
  let of = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
  (
    [
      super::REFINE_STARTED,
      super::REFINE_GAVE_UP,
      super::PREPARED,
      super::PREPARED_CHECKED,
      super::PREPARED_MISMATCH,
    ]
    .map(of),
    of(super::REFINE_TURNS),
  )
}

/// What one driver of a recorded submit's refinement left: what the submit was answered at once (none: parked), its
/// completion before the job ran and after, the counters after, and the work's kept refinement.
struct Driven {
  parked: Option<ReplyBody>,
  before_job: Option<ReplyBody>,
  answer: Option<ReplyBody>,
  counts: Counts,
  kept: Kept,
}

/// A green whose `/f` is [`original`], seeded through a recorded submit, and a work on it re-indenting `/f` whole.
fn a_rewrite(state: &mut ShardState) -> VolumeId {
  let made = ask(
    state,
    RequestBody::CreateGreen {
      name: "refined".to_owned(),
      require_evidence: false,
      base: None,
    },
  );
  let ReplyBody::GreenCreated { id: green } = made else {
    panic!("no green: {made:?}")
  };
  let seed = work_writing(state, (green, "seed"), (0, original()));
  let seeded = submit(state, seed, 1).1;
  assert!(
    matches!(
      seeded,
      Some(ReplyBody::Submitted {
        version: Some(_),
        ..
      })
    ),
    "{seeded:?}"
  );
  let whole = u64::try_from(original().len()).expect("a length");
  work_writing(state, (green, "by-task"), (whole, reindented()))
}

/// `work` submitted as request `sequence`, its refinement run by the shard's own task, awaited until it is answered.
async fn by_task(work: VolumeId, sequence: u32) -> Driven {
  let (id, parked) = on(|state| submit(state, work, sequence));
  let before_job = on(|state| completed(state, id));
  let deadline = slates_rt::futures::now_ns()
    .saturating_add(u64::try_from(PATIENCE.as_nanos()).unwrap_or(u64::MAX));
  while on(|state| completed(state, id)).is_none() && slates_rt::futures::now_ns() < deadline {
    slates_rt::futures::yield_now().await;
  }
  on(|state| Driven {
    parked,
    before_job,
    answer: completed(state, id),
    counts: counted(state),
    kept: kept_of(state, work),
  })
}

/// What `driven` moved of the counters since `before`: each job-moved counter's delta, and the turns'.
fn moved(before: Counts, driven: &Driven) -> ([u64; 5], u64) {
  let mut deltas = [0; 5];
  for ((delta, after), earlier) in deltas.iter_mut().zip(driven.counts.0).zip(before.0) {
    *delta = after.saturating_sub(earlier);
  }
  (deltas, driven.counts.1.saturating_sub(before.1))
}

/// A whole-file rewrite of a green on a stored audit daemon's shard ([`a_rewrite`]), refined by the shard's own task:
/// what it moved of the counters, and what it left.
fn driven_by_its_task() -> (([u64; 5], u64), Driven) {
  let (store, daemon) = crate::test_store::audit_daemon_stored();
  let history = || async move {
    let work = on(a_rewrite);
    let before = on(|state| counted(state));
    let task = by_task(work, 2).await;
    (moved(before, &task), task)
  };
  let driven = crate::test_store::run_on_first(&daemon, history, PATIENCE);
  daemon.stop();
  drop(store);
  driven
}

/// `driven`'s submit parked on its refinement and was not answered before its job ran.
fn assert_parked_until_its_job(driven: &Driven) {
  assert!(
    driven.parked.is_none(),
    "parked on its refinement: {:?}",
    driven.parked
  );
  assert!(
    driven.before_job.is_none(),
    "answered before its job: {:?}",
    driven.before_job
  );
}

/// A-787 part 2 (the job's task; main's ruling of 2026-11-24 02:17). Do: make a work on a green re-indenting the
/// green's 2,000-function file whole, and submit it as a client's recorded request, the shard's own refinement task
/// running its job, awaited. Expect: the submit parked on its refinement and was answered once the job ended, accepted;
/// the job kept a refined document and plan; it moved the counters a job moves (one started, its submit's increment
/// prepared and checked, none given up or mismatched), and took more than one turn (the slicing's non-vacuity count).
#[test]
fn t_a787_a_recorded_submit_parks_until_its_refinements_task_ends_in_turns() {
  let ((counters, turns), task) = driven_by_its_task();
  assert_parked_until_its_job(&task);
  let accepted = matches!(
    task.answer,
    Some(ReplyBody::Submitted {
      version: Some(_),
      ..
    })
  );
  assert!(accepted, "{:?}", task.answer);
  assert!(task.kept.is_some(), "the task kept a refinement");
  assert_eq!(
    counters,
    [1, 0, 1, 1, 0],
    "the task's job: started, prepared, checked"
  );
  assert!(turns > 1, "the job took turns: {turns}");
}

/// The copy steps that met a let-go window: brought back, and not brought back (the job given up).
fn copy_counts(state: &mut ShardState) -> [u64; 2] {
  [super::COPY_BROUGHT_BACK, super::COPY_NOT_BROUGHT_BACK]
    .map(|name| state.refusals.get(name).copied().unwrap_or(0))
}

/// A work on the green of `repository`'s `trunk` re-indenting `/trunk/a.bin`, a named text of [`original`] whose
/// windows are away (`test_store::a_text_whose_windows_are_away`), whole: the work.
fn a_rewrite_over_a_let_go_base(
  state: &mut ShardState,
  repository: slates_db::catalog::RepositoryId,
) -> VolumeId {
  let trunk = state
    .db
    .partition()
    .repositories()
    .branch(repository, "trunk")
    .expect("trunk");
  let green = crate::verbs::to_wire_volume(trunk);
  let made = ask(
    state,
    RequestBody::CreateWork {
      green,
      name: "rewrite".to_owned(),
    },
  );
  let ReplyBody::WorkCreated { id: work, .. } = made else {
    panic!("no work: {made:?}")
  };
  let edited = ask(
    state,
    RequestBody::Edit {
      work,
      path: "/trunk/a.bin".to_owned(),
      at: 0,
      delete_len: u64::try_from(original().len()).expect("a length"),
      bytes: reindented(),
    },
  );
  assert!(!matches!(edited, ReplyBody::Refused { .. }), "{edited:?}");
  work
}

/// A-760 (b) 4d (a refinement's copy over a let-go window), and why `refined_now` went (main's ruling of 2026-11-26).
/// Do: commit [`original`] as a Subversion repository's `/trunk/a.bin`, a named text, every window of it then let go
/// (`test_store::a_text_whose_windows_are_away`); submit a work re-indenting it whole as a client's recorded request,
/// its refinement run by the shard's own task. Expect: the job met the base's let-go window, brought it back and copied
/// the slice again (counted brought back, never not brought back), and kept its refinement. Red with the test driver
/// `refined_now` in place of the task: brought back 0, not brought back 1, and the submit never answered, where the task
/// accepted it at version 2 (`~/.local/state/w-hg-logs/2026-11-26-refined-now-red.txt`).
#[test]
fn a_refinement_whose_base_was_let_go_brings_it_back_and_refines() {
  let (store, daemon, repository) = crate::test_store::a_text_whose_windows_are_away(
    &crate::vcs_store::tests::owner(),
    "away",
    &original(),
  );
  let history = move || async move {
    let work = on(|state| a_rewrite_over_a_let_go_base(state, repository));
    let before = on(copy_counts);
    let driven = by_task(work, 1).await;
    (driven, before, on(copy_counts))
  };
  let (driven, before, after) = crate::test_store::run_on_first(&daemon, history, PATIENCE);
  daemon.stop();
  drop(store);
  let moved = [after[0] - before[0], after[1] - before[1]];
  eprintln!(
    "copy over a let-go base: brought back {}, not brought back {}; answered {:?}",
    moved[0], moved[1], driven.answer
  );
  assert!(
    moved[0] >= 1 && moved[1] == 0,
    "brought back, never given up: {moved:?}"
  );
  assert!(
    driven.kept.is_some(),
    "the job kept no refinement: {moved:?}"
  );
}
