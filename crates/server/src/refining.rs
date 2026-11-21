//! A work's whole-file rewrites refined a slice at a time, off the verb that submits it (A-787 part 2, close item R3;
//! A-776, D-O17; §4.3's step budget, CLAUDE.md §3 "Bounded work everywhere").
//!
//! A submit or a rebase whose composition replaces a file whole is judged as the edit the writer made
//! (`slates_merge::refine`). That search costs time that grows with the file: 14 ms for a 300 KiB file re-indented,
//! about 200 ms at 12 MiB (M4 Max, release, A-787), against a shard's quantum of its measured wake. So it never runs
//! inside the verb. The verb that finds one owed parks:
//! - on the green's shard, a local submit or rebase joins this module's parked list and runs again through
//!   `verbs::resume_all`;
//! - on a work's shard off its green, the gate parks the verb on `Need::Refine` and the job's end resolves it, as an
//!   owner's answer does (A-247).
//!
//! The verb starts the work's refinement job, a task of this shard. Each turn the job copies the next slice of the
//! two files from where they are held (the green's version or the bytes the owner sent, and the work's file),
//! re-borrowed through the shard's state, since a borrow cannot cross a turn. With both copied, it drives
//! `refine_rewrites_paced` over the document a `Pace` turn at a time, each turn bounded by half the shard's step
//! quantum, yielding to the shard between turns. Its result (the refined document and post-state plan) is kept under
//! the work's base version and journal length, which the parked verb, run again, composes with
//! (`verbs::compose_increment`). A verb whose work changed since (another journal length) finds none and owes one
//! again.
//!
//! **Bounds.**
//! - A copy slice is the shard's derived copy slice (its measured memcpy rate over half its quantum).
//! - A refinement unit is `refine::WORK_PER_TICK` of work.
//! - One job runs per work, and the parked verbs are bounded by the shard's client seats, as the whole-build park is.
//! - A job whose work, green or bytes went away gives up: its verbs compose the replacement unrefined, never wait
//!   again.
//! - The copies and the refiner's scratch live while the job runs and are freed a buffer a turn at its end. A result is
//!   kept until the work's next refinement replaces it or the work is gone.
//! - Measured (`a_large_rewrite_is_refined_over_many_turns_off_the_submit`, release, M3 Ultra): longest turn 69 to 79 µs
//!   for a 2 MiB re-indent, 133 to 157 µs at 8 MiB, the tail the largest single free (GAPS D-O17).

use std::collections::BTreeMap;

use slates_db::catalog::VolumeId as DbVolumeId;
use slates_merge::increment::{PostStatePlan, VolumeOp};
use slates_merge::ops_doc::OpsDoc;

use crate::state::ShardState;

/// Format: a refinement job started, the non-vacuity count of this module.
pub(crate) const REFINE_STARTED: &str = "merge.refine.started";
/// Format: the turns refinement jobs took (copying and refining), their slicing's non-vacuity count.
pub(crate) const REFINE_TURNS: &str = "merge.refine.turns";
/// Format: job turns that ran past the shard's whole step quantum (a turn's budget is half of it), and past two, four
/// and sixteen quanta: the slicing's measured tail, which a test or a bench reads beside the turns.
pub(crate) const REFINE_PAST_QUANTUM: [&str; 4] = [
  "merge.refine.past_quantum",
  "merge.refine.past_2_quanta",
  "merge.refine.past_4_quanta",
  "merge.refine.past_16_quanta",
];
/// Format: each job's longest turn in microseconds, added once a job (one job's run reads its own).
pub(crate) const REFINE_LONGEST_TURN_US: &str = "merge.refine.longest_turn_us";
/// Shape: the multiples of the quantum [`REFINE_PAST_QUANTUM`] counts turns past.
const PAST_MULTIPLES: [u64; 4] = [1, 2, 4, 16];

/// A job's turns, and of them those past each of [`PAST_MULTIPLES`] of the shard's step quantum.
type TurnCounts = (u64, [u64; PAST_MULTIPLES.len()]);
/// Format: a refinement job that gave up (its work, green or bytes gone): its verbs compose unrefined.
pub(crate) const REFINE_GAVE_UP: &str = "merge.refine.gave_up";
/// Format: a verb parked behind its work's refinement.
pub(crate) const REFINE_PARKED: &str = "merge.refine.parked";
/// Format: a verb refused `Overloaded` because the refinement park was at its bound.
const REFINE_PARK_FULL: &str = "merge.refine.park_full";

/// What a refinement is kept for: the work's base version and its journal's length, the composition it refines.
pub(crate) type Key = (u64, usize);

/// A finished refinement: the refined document and plan, or none when the job gave up.
struct Done {
  key: Key,
  refined: Option<(OpsDoc, PostStatePlan)>,
}

/// The shard's refinements: the jobs running, their results, and the local verbs waiting on them.
#[derive(Default)]
pub(crate) struct Refining {
  running: BTreeMap<DbVolumeId, Key>,
  done: BTreeMap<DbVolumeId, Done>,
  parked: BTreeMap<DbVolumeId, Vec<crate::verbs::ParkedSubmit>>,
  /// The longest turn a job has taken on this shard, nanoseconds (its slicing's measured bound).
  pub(crate) longest_turn_ns: u64,
}

/// A refinement a composition owes: its key, the unrefined document and plan, and the `(base path, path)` pairs it
/// replaces whole.
pub(crate) struct Owed {
  key: Key,
  doc: OpsDoc,
  plan: PostStatePlan,
  pairs: Vec<(String, String)>,
}

/// The base a work's journal composes against at `base_version`: its green's or a replica's when this shard holds one,
/// else the entries the work fetched (A-247).
fn compose_base(
  state: &ShardState,
  (work, green, base_version): (DbVolumeId, DbVolumeId, u64),
  journal: &[VolumeOp],
) -> Option<slates_merge::increment::Base> {
  if let Some(engine) = state.greens.get(&green) {
    return Some(engine.base_for(base_version, journal));
  }
  if let Some(replica) = state
    .merge
    .replicas
    .get(&slates_db::register::ObjectId(green.bytes))
  {
    return Some(replica.base_for(base_version, journal));
  }
  let w = state.works.get(&work)?;
  let named: std::collections::BTreeSet<&str> =
    journal.iter().flat_map(VolumeOp::names).flatten().collect();
  slates_merge::increment::Base::for_names(&named, &w.entries)
}

/// The refinement `work`'s composition owes, at `base_version` over `journal`: none when it replaces nothing whole, when
/// its refinement is kept already (or gave up) under this key, or when it does not compose (the verb refuses it typed).
pub(crate) fn owed(
  state: &ShardState,
  (work, green, base_version): (DbVolumeId, DbVolumeId, u64),
  journal: &[VolumeOp],
) -> Option<Owed> {
  let key = (base_version, journal.len());
  if state
    .refining
    .done
    .get(&work)
    .is_some_and(|done| done.key == key)
  {
    return None;
  }
  let base = compose_base(state, (work, green, base_version), journal)?;
  let (doc, plan) = slates_merge::increment::compose_volume_planned(&base, journal).ok()?;
  let pairs = slates_merge::refine::whole_replaced(&doc);
  (!pairs.is_empty()).then_some(Owed {
    key,
    doc,
    plan,
    pairs,
  })
}

/// The refined document and plan kept for `work` under `key`, for its composition; none when there is none (nothing
/// was owed, or the job gave up), and the composition is then unrefined.
pub(crate) fn kept(
  state: &ShardState,
  work: DbVolumeId,
  key: Key,
) -> Option<(OpsDoc, PostStatePlan)> {
  let done = state.refining.done.get(&work)?;
  (done.key == key).then(|| done.refined.clone()).flatten()
}

/// Starts `work`'s refinement job for `owed`, unless one runs already under its key. Whether it runs.
pub(crate) fn start(
  state: &mut ShardState,
  (work, green): (DbVolumeId, DbVolumeId),
  owed: Owed,
) -> bool {
  if state.refining.running.get(&work) == Some(&owed.key) {
    return true;
  }
  let job = job(work, green, owed);
  match slates_rt::futures::spawn(job.0) {
    Ok(task) => {
      let _ = slates_rt::futures::detach(task);
      state.refining.running.insert(work, job.1);
      state.count(REFINE_STARTED, 1);
      true
    }
    Err(_) => false,
  }
}

/// Parks the running local submit or rebase of `work` behind its refinement, to run again through
/// `verbs::resume_all`. The reply to send now: the parked acknowledgement, or `Overloaded` at the bound.
pub(crate) fn park(
  state: &mut ShardState,
  work: DbVolumeId,
  submit: crate::verbs::ParkedSubmit,
) -> Result<(), slates_ipc::protocol::Refusal> {
  // The same bound as a whole build's parked submits: the shard's client seats.
  let bound = state
    .config
    .clients_per_shard
    .saturating_mul(usize::try_from(state.config.region.slots).unwrap_or(1));
  // A retry of a parked request (its completion key) takes its place, answered on the retry's route.
  if let Some(joined) = state.refining.parked.get_mut(&work).and_then(|parked| {
    parked
      .iter_mut()
      .find(|parked| parked.key() == submit.key())
  }) {
    *joined = submit;
    return Ok(());
  }
  let held: usize = state.refining.parked.values().map(Vec::len).sum();
  if held >= bound {
    state.count(REFINE_PARK_FULL, 1);
    return Err(slates_ipc::protocol::Refusal::Overloaded { shard: state.shard });
  }
  state.refining.parked.entry(work).or_default().push(submit);
  state.count(REFINE_PARKED, 1);
  Ok(())
}

/// A pair's two files as copied so far.
struct Copy {
  base_path: String,
  path: String,
  base: Vec<u8>,
  post: Vec<u8>,
}

/// Which file of a pair a copy step reads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
  Base,
  Post,
}

/// The next slice of `bytes` into `into`, at most `slice` bytes: a whole owned version (an older version materialized)
/// taken as it is. Whether `into` now holds all of `bytes`.
fn copy_from(bytes: std::borrow::Cow<'_, [u8]>, into: &mut Vec<u8>, slice: usize) -> Option<bool> {
  match bytes {
    std::borrow::Cow::Owned(owned) if into.is_empty() => {
      *into = owned;
      Some(true)
    }
    bytes => {
      if into.capacity() < bytes.len() {
        into.reserve_exact(bytes.len().saturating_sub(into.len()));
      }
      let from = into.len();
      let to = from.saturating_add(slice).min(bytes.len());
      into.extend_from_slice(bytes.get(from..to)?);
      Some(into.len() == bytes.len())
    }
  }
}

/// One copy step: the next slice of `side` of `copy` from where this shard holds it, at most `slice` bytes: the base
/// from its green's version, a replica's, or the bytes the owner sent the work's parked verb (A-247); the post from the
/// work's file. `Some(true)` once that side is whole, `Some(false)` while more is to come, `None` when the source is gone
/// or shrank.
fn copy_step(
  state: &ShardState,
  (work, green, base_version): (DbVolumeId, DbVolumeId, u64),
  (copy, side, slice): (&mut Copy, Side, usize),
) -> Option<bool> {
  match side {
    Side::Post => with_base(state, (work, green, base_version), |base| {
      // The work's file read a slice at a time over its base: a changed file may be pieces over the base (A-790).
      let w = state.works.get(&work)?;
      let total = usize::try_from(w.files.len(base, &copy.path).ok()??).ok()?;
      if copy.post.capacity() < total {
        copy
          .post
          .reserve_exact(total.saturating_sub(copy.post.len()));
      }
      let from = copy.post.len();
      let to = from.saturating_add(slice).min(total);
      copy.post.resize(to, 0);
      let into = copy.post.get_mut(from..to)?;
      let got = w
        .files
        .copy_range(base, &copy.path, u64::try_from(from).ok()?, into)
        .ok()??;
      (got == to.saturating_sub(from)).then_some(to == total)
    }),
    Side::Base => {
      if let Some(engine) = state.greens.get(&green) {
        let bytes = engine.content_ref_at(&copy.base_path, base_version)?;
        return copy_from(bytes, &mut copy.base, slice);
      }
      if let Some(replica) = state
        .merge
        .replicas
        .get(&slates_db::register::ObjectId(green.bytes))
      {
        let bytes = replica.content_ref_at(&copy.base_path, base_version)?;
        return copy_from(bytes, &mut copy.base, slice);
      }
      let bytes = crate::work_home::parked_fetch(state, work, &copy.base_path)?;
      copy_from(std::borrow::Cow::Borrowed(bytes), &mut copy.base, slice)
    }
  }
}

/// `f` over `work`'s base: its green's version on this shard, a replica's, or what the owner sent its parked verb.
fn with_base<R>(
  state: &ShardState,
  (work, green, version): (DbVolumeId, DbVolumeId, u64),
  f: impl FnOnce(crate::work_files::Base<'_>) -> Option<R>,
) -> Option<R> {
  if let Some(engine) = state.greens.get(&green) {
    return f(crate::work_files::Base {
      green: Some(&*engine),
      fetched: None,
      version,
      entries: None,
    });
  }
  if let Some(replica) = state
    .merge
    .replicas
    .get(&slates_db::register::ObjectId(green.bytes))
  {
    return f(crate::work_files::Base {
      green: Some(replica),
      fetched: None,
      version,
      entries: None,
    });
  }
  f(crate::work_files::Base {
    green: None,
    fetched: crate::work_home::parked_fetched(state, work),
    version,
    entries: None,
  })
}

/// The job's task and the key it runs under.
type Job = (
  core::pin::Pin<Box<dyn core::future::Future<Output = ()>>>,
  Key,
);

/// `work`'s refinement job for `owed` (module docs).
fn job(work: DbVolumeId, green: DbVolumeId, owed: Owed) -> Job {
  let Owed {
    key,
    doc,
    plan,
    pairs,
  } = owed;
  let task = async move {
    let at = (work, green, key.0);
    let mut copies: Vec<Copy> = pairs
      .into_iter()
      .map(|(base_path, path)| Copy {
        base_path,
        path,
        base: Vec::new(),
        post: Vec::new(),
      })
      .collect();
    let mut turns = (0u64, [0u64; PAST_MULTIPLES.len()]);
    let mut longest = 0u64;
    // The copies, a slice a turn.
    for index in 0..copies.len() {
      for side in [Side::Base, Side::Post] {
        loop {
          let began = slates_rt::futures::now_ns();
          let step = crate::state::with_state(|s| {
            // Half the quantum's copying: the turn's budget, as a refinement turn's is.
            let slice = s
              .config
              .copy_slice_bytes_within(s.config.step_quantum_ns() / 2);
            let slice = usize::try_from(slice).unwrap_or(usize::MAX).max(1);
            copies
              .get_mut(index)
              .and_then(|copy| copy_step(s, at, (copy, side, slice)))
          })
          .flatten();
          turn(&mut turns, &mut longest, began);
          match step {
            Some(true) => break,
            Some(false) => slates_rt::futures::yield_now().await,
            None => {
              let _ = crate::state::with_state(|s| finish(s, work, (key, None), (turns, longest)));
              return;
            }
          }
        }
      }
    }
    // The refinement, a pace turn a step.
    let refining = async move {
      let (mut doc, mut plan) = (doc, plan);
      let mut refiner = slates_merge::refine::Refiner::new();
      let ticks = slates_merge::pace::Ticks;
      let mut files = |base_path: &str, path: &str| {
        copies
          .iter()
          .find(|copy| copy.base_path == base_path && copy.path == path)
          .map(|copy| {
            (
              std::borrow::Cow::Borrowed(copy.base.as_slice()),
              std::borrow::Cow::Borrowed(copy.post.as_slice()),
            )
          })
      };
      slates_merge::refine::refine_rewrites_paced(
        &mut doc,
        &mut plan,
        &mut refiner,
        (&mut files, &ticks),
      )
      .await;
      // The scratch and the copies go back to the owner, to be given back a slice a turn.
      (doc, plan, refiner, copies)
    };
    let mut refining = core::pin::pin!(refining);
    let pace = slates_merge::pace::Pace::new(slates_rt::futures::now_ns);
    let (doc, plan, mut refiner, copies) = loop {
      let budget = crate::state::with_state(|s| s.config.step_quantum_ns() / 2).unwrap_or(1);
      let began = slates_rt::futures::now_ns();
      pace.turn(began.saturating_add(budget.max(1)));
      let polled = pace.drive(refining.as_mut());
      turn(&mut turns, &mut longest, began);
      if let core::task::Poll::Ready(refined) = polled {
        break refined;
      }
      slates_rt::futures::yield_now().await;
    };
    // What the job held, given back a buffer a turn: freed together, the copies and the scratch of a 2 MiB rewrite cost
    // about a millisecond in one step (A-787 part 2).
    let mut copies = copies.into_iter();
    loop {
      let began = slates_rt::futures::now_ns();
      let done = match copies.next() {
        Some(copy) => {
          drop(copy);
          false
        }
        None => refiner.release_step(),
      };
      turn(&mut turns, &mut longest, began);
      if done {
        break;
      }
      slates_rt::futures::yield_now().await;
    }
    let _ =
      crate::state::with_state(|s| finish(s, work, (key, Some((doc, plan))), (turns, longest)));
  };
  (Box::pin(task), key)
}

/// Counts a job turn begun at `began`: `turns` the turns and those past each multiple of the shard's whole step quantum,
/// `longest` the longest, nanoseconds.
fn turn(turns: &mut TurnCounts, longest: &mut u64, began: u64) {
  let took = slates_rt::futures::now_ns().saturating_sub(began);
  let quantum = crate::state::with_state(|s| s.config.step_quantum_ns()).unwrap_or(u64::MAX);
  turns.0 = turns.0.saturating_add(1);
  for (count, multiple) in turns.1.iter_mut().zip(PAST_MULTIPLES) {
    *count = count.saturating_add(u64::from(took > quantum.saturating_mul(multiple)));
  }
  *longest = (*longest).max(took);
}

/// Ends `work`'s job: its result kept under its key (none when it gave up), its turns counted, and the verbs waiting on
/// it run again.
fn finish(
  state: &mut ShardState,
  work: DbVolumeId,
  (key, refined): (Key, Option<(OpsDoc, PostStatePlan)>),
  (turns, longest): (TurnCounts, u64),
) {
  if refined.is_none() {
    state.count(REFINE_GAVE_UP, 1);
  }
  state.count(REFINE_TURNS, turns.0);
  for (name, count) in REFINE_PAST_QUANTUM.iter().zip(turns.1) {
    state.count(name, count);
  }
  // The job's longest turn, microseconds: summed over jobs, so a run of one job reads its own.
  state.count(
    REFINE_LONGEST_TURN_US,
    longest / crate::daemon::NANOS_PER_MICRO,
  );
  state.refining.longest_turn_ns = state.refining.longest_turn_ns.max(longest);
  if state.refining.running.get(&work) == Some(&key) {
    state.refining.running.remove(&work);
  }
  // A work gone takes its result with it: nothing would read it.
  if state.works.contains_key(&work) {
    state.refining.done.insert(work, Done { key, refined });
  } else {
    state.refining.done.remove(&work);
  }
  if let Some(parked) = state.refining.parked.remove(&work) {
    crate::verbs::resume_all(state, parked);
  }
  crate::work_home::refined(state, work);
}
