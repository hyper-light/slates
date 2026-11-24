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

/// The counters a job moves whatever its timing: started, gave up, prepared, its check, a mismatch.
fn counted(state: &ShardState) -> [u64; 5] {
  [
    super::REFINE_STARTED,
    super::REFINE_GAVE_UP,
    super::PREPARED,
    super::PREPARED_CHECKED,
    super::PREPARED_MISMATCH,
  ]
  .map(|name| state.refusals.get(name).copied().unwrap_or(0))
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
  let (store, daemon) = crate::test_store::audit_daemon_stored();
  let history = || async move {
    let green = on(|state| {
      let made = ask(
        state,
        RequestBody::CreateGreen {
          name: "refined".to_owned(),
          require_evidence: false,
          base: None,
        },
      );
      let ReplyBody::GreenCreated { id } = made else {
        panic!("no green: {made:?}")
      };
      id
    });
    let seeded = on(|state| {
      let seed = work_writing(state, (green, "seed"), (0, original()));
      submit(state, seed, 1).1
    });
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
    let (by_task, by_turns) = on(|state| {
      (
        work_writing(state, (green, "by-task"), (whole, reindented())),
        work_writing(state, (green, "by-turns"), (whole, reindented())),
      )
    });

    // The shard's own task.
    let (before, turns_before) = on(|state| {
      (
        counted(state),
        state.refusals.get(super::REFINE_TURNS).copied(),
      )
    });
    let (task_id, parked) = on(|state| submit(state, by_task, 2));
    assert!(
      parked.is_none(),
      "the submit parks on its refinement: {parked:?}"
    );
    let deadline = slates_rt::futures::now_ns()
      .saturating_add(u64::try_from(PATIENCE.as_nanos()).unwrap_or(u64::MAX));
    let task_answer = loop {
      if let Some(answer) = on(|state| completed(state, task_id)) {
        break answer;
      }
      assert!(
        slates_rt::futures::now_ns() < deadline,
        "the task's submit was never answered"
      );
      slates_rt::futures::yield_now().await;
    };
    let (task_counted, task_turns, task_kept) = on(|state| {
      (
        counted(state),
        state.refusals.get(super::REFINE_TURNS).copied(),
        kept_of(state, by_task),
      )
    });

    // The held shard's turns.
    let held = on(|state| {
      super::HELD_FOR_TEST.with(|held| held.set(true));
      let (id, parked) = submit(state, by_turns, 3);
      let unanswered = completed(state, id);
      super::refined_now(state);
      super::HELD_FOR_TEST.with(|held| held.set(false));
      (
        parked,
        unanswered,
        completed(state, id),
        counted(state),
        state.refusals.get(super::REFINE_TURNS).copied(),
        kept_of(state, by_turns),
      )
    });
    let (held_parked, held_unanswered, held_answer, held_counted, held_turns, held_kept) = held;
    (
      (before, turns_before),
      (task_answer, task_counted, task_turns, task_kept),
      (
        held_parked,
        held_unanswered,
        held_answer,
        held_counted,
        held_turns,
        held_kept,
      ),
    )
  };
  let (
    (before, turns_before),
    (task_answer, task_counted, task_turns, task_kept),
    (held_parked, held_unanswered, held_answer, held_counted, held_turns, held_kept),
  ) = crate::test_store::run_on_first(&daemon, history, PATIENCE);
  daemon.stop();
  drop(store);

  assert!(
    matches!(
      task_answer,
      ReplyBody::Submitted {
        version: Some(_),
        ..
      }
    ),
    "{task_answer:?}"
  );
  assert!(
    held_parked.is_none(),
    "the held submit parks on its refinement: {held_parked:?}"
  );
  assert!(
    held_unanswered.is_none(),
    "answered before its job ran: {held_unanswered:?}"
  );
  assert!(
    held_answer.is_some(),
    "the held submit was answered once its job ended"
  );
  assert!(task_kept.is_some(), "the task kept a refinement");
  assert_eq!(task_kept, held_kept, "the two drivers refined alike");
  let by_task: Vec<u64> = task_counted
    .iter()
    .zip(before)
    .map(|(after, before)| after.saturating_sub(before))
    .collect();
  let by_turns: Vec<u64> = held_counted
    .iter()
    .zip(task_counted)
    .map(|(after, before)| after.saturating_sub(before))
    .collect();
  assert_eq!(
    by_task,
    vec![1, 0, 1, 1, 0],
    "the task's job: started, prepared, checked"
  );
  assert_eq!(by_turns, by_task, "the held job moved the task's counters");
  let task_took = task_turns
    .unwrap_or(0)
    .saturating_sub(turns_before.unwrap_or(0));
  let held_took = held_turns
    .unwrap_or(0)
    .saturating_sub(task_turns.unwrap_or(0));
  assert!(
    task_took > 1 && held_took > 1,
    "both took turns: {task_took} and {held_took}"
  );
}
