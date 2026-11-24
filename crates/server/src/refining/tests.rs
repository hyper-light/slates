//! The refinement job's two drivers held equal (A-787 part 2; main 01a20182's ruling of 2026-11-24 02:17): the task a
//! shard runs ([`super::job`]) and the turns a test holding the shard takes ([`super::refined_now`]) are the same
//! [`super::RefineJob::turn`], so a test that refines under a held shard certifies what the shard does.

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

/// A green whose `/f` is [`original`], seeded through a recorded submit, and two works on it re-indenting `/f` whole.
fn two_rewrites(state: &mut ShardState) -> (VolumeId, VolumeId) {
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
  (
    work_writing(state, (green, "by-task"), (whole, reindented())),
    work_writing(state, (green, "by-turns"), (whole, reindented())),
  )
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

/// `work` submitted as request `sequence` under a held shard, its refinement's turns taken through `refined_now`.
fn by_turns(state: &mut ShardState, work: VolumeId, sequence: u32) -> Driven {
  super::HELD_FOR_TEST.with(|held| held.set(true));
  let (id, parked) = submit(state, work, sequence);
  let before_job = completed(state, id);
  super::refined_now(state);
  super::HELD_FOR_TEST.with(|held| held.set(false));
  Driven {
    parked,
    before_job,
    answer: completed(state, id),
    counts: counted(state),
    kept: kept_of(state, work),
  }
}

/// What `driven` moved of the counters since `before`: each job-moved counter's delta, and the turns'.
fn moved(before: Counts, driven: &Driven) -> ([u64; 5], u64) {
  let mut deltas = [0; 5];
  for ((delta, after), earlier) in deltas.iter_mut().zip(driven.counts.0).zip(before.0) {
    *delta = after.saturating_sub(earlier);
  }
  (deltas, driven.counts.1.saturating_sub(before.1))
}

/// What each driver moved of the counters, and what each left ([`by_task`], [`by_turns`]).
type BothDriven = (([u64; 5], u64), ([u64; 5], u64), Driven, Driven);

/// Two whole-file rewrites of one green on a stored audit daemon's shard ([`two_rewrites`]), the first refined by the
/// shard's own task and the second by `refined_now`: what each moved and left.
fn driven_both_ways() -> BothDriven {
  let (store, daemon) = crate::test_store::audit_daemon_stored();
  let history = || async move {
    let (task_work, turns_work) = on(two_rewrites);
    let before = on(|state| counted(state));
    let task = by_task(task_work, 2).await;
    let held = on(|state| by_turns(state, turns_work, 3));
    (moved(before, &task), moved(task.counts, &held), task, held)
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

/// A-787 part 2 (the job's drivers; main's ruling of 2026-11-24 02:17). Do: make two works on one green, each
/// re-indenting the green's 2,000-function file whole; submit the first as a client's recorded request and let the
/// shard's own refinement task run it, awaited; then submit the second the same way under a held shard and take its
/// job's turns through `refined_now`. Expect: each submit parked on its refinement and was answered once the job ended,
/// the first accepted; both jobs kept the same refined document and plan; and each moved the same counters (one job
/// started, its submit's increment prepared and checked, none given up or mismatched), and took more than one turn
/// (the slicing's non-vacuity count), so the held driver takes the turns the task takes.
#[test]
fn t_a787_a_recorded_submits_refinement_by_its_task_and_by_refined_now_agree() {
  let (task_moved, held_moved, task, held) = driven_both_ways();
  assert_parked_until_its_job(&task);
  assert_parked_until_its_job(&held);
  let accepted = matches!(
    task.answer,
    Some(ReplyBody::Submitted {
      version: Some(_),
      ..
    })
  );
  assert!(accepted, "{:?}", task.answer);
  assert!(
    held.answer.is_some(),
    "the held submit was answered once its job ended"
  );
  assert!(task.kept.is_some(), "the task kept a refinement");
  assert_eq!(task.kept, held.kept, "the two drivers refined alike");
  assert_eq!(
    task_moved.0,
    [1, 0, 1, 1, 0],
    "the task's job: started, prepared, checked"
  );
  assert_eq!(
    held_moved.0, task_moved.0,
    "the held job moved the task's counters"
  );
  assert_eq!(
    (task_moved.1 > 1, held_moved.1 > 1),
    (true, true),
    "both took turns: {} and {}",
    task_moved.1,
    held_moved.1
  );
}
