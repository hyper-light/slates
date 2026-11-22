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
//! The verb starts the work's refinement job, a task of this shard. Each unit the job copies the next slice of the
//! two files from where they are held (the green's version or the bytes the owner sent, and the work's file),
//! re-borrowed through the shard's state, since a borrow cannot cross a yield. With both copied, it drives
//! `refine_rewrites_paced` over the document a `Pace` drive of half the shard's step quantum a unit. The units run in
//! paced turns (A-783's `vcs_store::paced_turn`, [`JobTurns`]; goal 84a): a turn takes the next unit while its quantum
//! has room for the unit's measured cost, and at a quantum's end while no one else waits on the shard, up to the
//! shard's ceiling, and yields only then. A job that yielded after every unit (10 to 12 for a small rewrite) kept a
//! submit parked behind it for that many trips round the shard's ready tasks: on CI macos-latest (3 CPUs, run
//! 45000596198) the job's span p999 381-516 µs and the parked submits' taken -> written p999 850-1,394 µs, the loaded
//! tail of an edit and merge. Its result (the refined document and post-state plan) is kept under
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
//! - The copies and the refiner's scratch live while the job runs and are freed a buffer a unit at its end. A result is
//!   kept until the work's next refinement replaces it or the work is gone.
//! - Measured (`a_large_rewrite_is_refined_over_many_turns_off_the_submit`, release, M3 Ultra, before the paced turns):
//!   longest turn 69 to 79 µs for a 2 MiB re-indent, 133 to 157 µs at 8 MiB, the tail the largest single free (GAPS
//!   D-O17). A paced turn runs to the shard's ceiling while no one waits (debug, MacBook: 205,014 units in 115 turns), and
//!   to the quantum's end and one unit while someone does (`ShardContext::others_waiting`: an inbound ring's word, a
//!   ready poller, a due timer, a ready task).

use std::collections::BTreeMap;

use slates_db::catalog::VolumeId as DbVolumeId;
use slates_merge::increment::{PostStatePlan, VolumeOp};
use slates_merge::ops_doc::OpsDoc;

use crate::state::ShardState;

/// Format: a refinement job started, the non-vacuity count of this module.
pub(crate) const REFINE_STARTED: &str = "merge.refine.started";
/// Format: the turns refinement jobs took (copying, refining, freeing and preparing): a turn ends where the job yields to
/// the shard ([`JobTurns`]).
pub(crate) const REFINE_TURNS: &str = "merge.refine.turns";
/// Format: the units of work refinement jobs ran (a copy slice, a refinement drive of half the quantum, a buffer freed,
/// a prepare slice): their slicing's non-vacuity count, beside [`REFINE_TURNS`], whose units a paced turn gathers.
pub(crate) const REFINE_UNITS: &str = "merge.refine.units";
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
  /// What the job prepared for the parked submit off its step (A-787 part 3), none when nothing was.
  prepared: Option<Prepared>,
}

/// A refined submit's increment, verdict and encoding, made by the job a bounded slice a turn so the submit's own step
/// only commits (A-787 part 3; main's ruling of 2026-11-21 09:25 on the 415 ms step): the increment composed as
/// `verbs::compose_increment` composes it (its post-state laid a copy slice a turn, its identity hashed a hash slice a
/// turn), its verdict computed at the head the job saw (`Green::compute`, which the submit takes by position:
/// `Green::submit_computed`, judging again only what a later head changed), and its encoding. For the submit that
/// parked with `evidence`; taken a part at a time as the submit reaches each, and gone with the work's next refinement.
struct Prepared {
  /// The submit's evidence, which the increment carries.
  evidence: Vec<[u8; 32]>,
  /// The increment, until the submit takes it.
  increment: Option<slates_merge::engine::Increment>,
  /// The increment's identity.
  id: [u8; 32],
  /// Its verdict computed at the head the job saw, until the submit takes it.
  computed: Option<slates_merge::engine::Computed>,
  /// Its encoding, until the submit takes it.
  encoded: Option<Vec<u8>>,
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
    let mut turns = JobTurns::begin();
    // The copies, a slice a unit.
    for index in 0..copies.len() {
      for side in [Side::Base, Side::Post] {
        loop {
          turns.unit().await;
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
          match step {
            Some(true) => break,
            Some(false) => {}
            None => {
              let ended = turns.end();
              let _ = crate::state::with_state(|s| finish(s, work, (key, None, None), ended));
              return;
            }
          }
        }
      }
    }
    // The refinement, a drive of half the quantum a unit.
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
      turns.unit().await;
      let budget = crate::state::with_state(|s| s.config.step_quantum_ns() / 2).unwrap_or(1);
      let began = slates_rt::futures::now_ns();
      pace.turn(began.saturating_add(budget.max(1)));
      if let core::task::Poll::Ready(refined) = pace.drive(refining.as_mut()) {
        break refined;
      }
    };
    // What the job held, given back a buffer a unit: freed together, the copies and the scratch of a 2 MiB rewrite cost
    // about a millisecond in one step (A-787 part 2).
    let mut copies = copies.into_iter();
    loop {
      turns.unit().await;
      let done = match copies.next() {
        Some(copy) => {
          drop(copy);
          false
        }
        None => refiner.release_step(),
      };
      if done {
        break;
      }
    }
    // The parked submit's increment, verdict and encoding, a bounded slice a unit (A-787 part 3).
    let prepared = prepare((work, green, key), (&doc, &plan), &mut turns).await;
    let ended = turns.end();
    let _ =
      crate::state::with_state(|s| finish(s, work, (key, Some((doc, plan)), prepared), ended));
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

/// A refinement job's turns (A-783's paced turn, `crate::vcs_store::paced_turn`; goal 84a, w-perf 2026-11-22): before each
/// unit of the job's work (a copy slice, a refinement drive, a buffer freed, a prepare slice) it asks whether the turn
/// has room for it. The turn goes on while the quantum has room for the unit's measured cost, and at the quantum's end
/// while no one else waits on the shard, up to the shard's ceiling; it ends, and the job yields to the shard, only then.
///
/// Found by use (CI macos-latest, 3 CPUs, TB, run 45000596198): the job yielded after every unit, 10 to 12 for a small
/// rewrite, so a submit parked behind it waited out that many trips round the shard's ready tasks. 9,000 parked
/// submits a round: the job's span p99 204-221 µs and p999 381-516 µs, the submits' taken -> written p999 850-1,394 µs,
/// the loaded tail of an edit and merge.
struct JobTurns {
  /// The turns ended, and those past each multiple of the shard's step quantum ([`turn`]).
  counts: TurnCounts,
  /// The longest turn, nanoseconds.
  longest: u64,
  /// The units run ([`REFINE_UNITS`]).
  units: u64,
  /// The turn under way.
  step: slates_pace::Step,
  /// When it began.
  began: u64,
  /// The units' measured cost, across the job's turns.
  costs: crate::vcs_store::TurnCosts,
}

/// What a job's turns came to at its end: the turns and those past the quantum, the longest, and the units.
type Ended = (TurnCounts, u64, u64);

impl JobTurns {
  /// The job's first turn, begun now.
  fn begin() -> JobTurns {
    let began = slates_rt::futures::now_ns();
    JobTurns {
      counts: (0, [0; PAST_MULTIPLES.len()]),
      longest: 0,
      units: 0,
      step: crate::vcs_store::paced_turn(step_quantum_ns(), began),
      began,
      costs: crate::vcs_store::TurnCosts::default(),
    }
  }

  /// Before a unit: it runs in this turn while the turn has room for it; otherwise this turn ends, counted, the job
  /// yields to the shard, and the unit opens the next turn.
  async fn unit(&mut self) {
    if !self
      .step
      .more(crate::vcs_store::ANSWER_STEP, &mut self.costs)
    {
      turn(&mut self.counts, &mut self.longest, self.began);
      slates_rt::futures::yield_now().await;
      self.began = slates_rt::futures::now_ns();
      self.step = crate::vcs_store::paced_turn(step_quantum_ns(), self.began);
      // A turn's first unit always runs; this marks it begun, so its cost is measured at the next ask.
      let _ = self
        .step
        .more(crate::vcs_store::ANSWER_STEP, &mut self.costs);
    }
    self.units = self.units.saturating_add(1);
  }

  /// The job's last turn ended: what its turns came to.
  fn end(mut self) -> Ended {
    turn(&mut self.counts, &mut self.longest, self.began);
    (self.counts, self.longest, self.units)
  }
}

/// The shard's step quantum now, the unit a paced turn runs to while someone waits.
fn step_quantum_ns() -> u64 {
  crate::state::with_state(|s| s.config.step_quantum_ns())
    .unwrap_or(1)
    .max(1)
}

/// Ends `work`'s job: its result kept under its key (none when it gave up), its turns and units counted, and the verbs
/// waiting on it run again.
fn finish(
  state: &mut ShardState,
  work: DbVolumeId,
  (key, refined, prepared): (Key, Option<(OpsDoc, PostStatePlan)>, Option<Prepared>),
  (turns, longest, units): Ended,
) {
  if refined.is_none() {
    state.count(REFINE_GAVE_UP, 1);
  }
  state.count(REFINE_TURNS, turns.0);
  state.count(REFINE_UNITS, units);
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
    if prepared.is_some() {
      state.count(PREPARED, 1);
    }
    state.refining.done.insert(
      work,
      Done {
        key,
        refined,
        prepared,
      },
    );
  } else {
    state.refining.done.remove(&work);
  }
  if let Some(parked) = state.refining.parked.remove(&work) {
    crate::verbs::resume_all(state, parked);
  }
  crate::work_home::refined(state, work);
}

/// Counter: a refined submit's increment, verdict and encoding prepared off its step (A-787 part 3). Format: a counter
/// name in the daemon's status report.
pub(crate) const PREPARED: &str = "merge.refine.prepared";
/// Counter: the slices a prepared submit's increment, verdict and encoding took (its post-state's copy slices, its
/// identity's hash slices, and one each for its document's encoding, its verdict and the increment's encoding): the
/// non-vacuity figure of the slicing. Format: a counter name in the daemon's status report.
pub(crate) const PREPARE_SLICES: &str = "merge.submit.prepare_slices";
/// Counter: a prepared increment compared whole with the one `verbs::compose_increment` composes (a debug build's
/// oracle, A-821), and those that differed. Format: counter names in the daemon's status report.
pub(crate) const PREPARED_CHECKED: &str = "merge.refine.prepared_checked";
/// Counter: see [`PREPARED_CHECKED`].
pub(crate) const PREPARED_MISMATCH: &str = "merge.refine.prepared_mismatch";
/// Counter: a prepared increment, verdict or encoding the submit took (each part once). Format: a counter name in the
/// daemon's status report.
pub(crate) const PREPARED_TAKEN: &str = "merge.submit.prepared_taken";

/// One bounded unit of a prepare, in the job's paced turn ([`JobTurns::unit`]): runs `step` on the shard's state and
/// counts the slice.
async fn prepare_turn<T>(
  turns: &mut JobTurns,
  step: impl FnOnce(&mut ShardState) -> Option<T>,
) -> Option<T> {
  turns.unit().await;
  let done = crate::state::with_state(|s| {
    let done = step(s);
    s.count(PREPARE_SLICES, 1);
    done
  })
  .flatten();
  done
}

/// What the work still is and what the green holds for a prepare's turn: `None` once the work changed (another key),
/// the green is not on this shard, or no local submit waits for the refinement.
fn still_owed(s: &ShardState, (work, green, key): (DbVolumeId, DbVolumeId, Key)) -> Option<()> {
  let w = s.works.get(&work)?;
  ((w.base_version, w.journal.len()) == key && s.greens.contains_key(&green)).then_some(())
}

/// Prepares the parked submit's increment, verdict and encoding over `(doc, plan)`, the refinement's result, a bounded
/// slice a turn ([`Prepared`]): the post-state a copy slice a turn (the shard's measured memcpy rate within its
/// quantum), the identity a hash slice a turn (its measured BLAKE3 rate), and the document's encoding, the verdict and
/// the increment's encoding a turn each. `None`, nothing kept, where nothing waits for it or the work or the green
/// changed: the submit then composes and judges in its step as before.
async fn prepare(
  (work, green, key): (DbVolumeId, DbVolumeId, Key),
  (doc, plan): (&OpsDoc, &PostStatePlan),
  turns: &mut JobTurns,
) -> Option<Prepared> {
  let at = (work, green, key);
  let evidence = crate::state::with_state(|s| {
    still_owed(s, at)?;
    crate::verbs::parked_local_evidence(s.refining.parked.get(&work)?)
  })
  .flatten()?;
  let post = laid_post_state(at, (doc, plan), &mut *turns).await?;
  let doc_bytes = prepare_turn(&mut *turns, |s| {
    still_owed(s, at)?;
    Some(doc.encode())
  })
  .await?;
  let id = hashed_identity(at, (&doc_bytes, &post), &mut *turns).await?;
  drop(doc_bytes);
  let increment = slates_merge::engine::Increment {
    id,
    base: key.0,
    doc: doc.clone(),
    post_state: post,
    evidence: evidence.clone(),
    named: Default::default(),
  };
  // The oracle, in a debug build: the increment composed whole as the submit's own step composes it (main's
  // `compose_increment`), compared whole with the sliced one, in a turn of its own (A-821).
  #[cfg(debug_assertions)]
  let _ = prepare_turn(&mut *turns, |s| {
    let whole = crate::verbs::composed_whole(
      s,
      (work, green, key.0),
      (doc.clone(), plan.clone()),
      evidence.clone(),
    )?;
    s.count(PREPARED_CHECKED, 1);
    if whole != increment {
      s.count(PREPARED_MISMATCH, 1);
    }
    Some(())
  })
  .await;
  let computed = prepare_turn(&mut *turns, |s| {
    still_owed(s, at)?;
    s.greens.get(&green)?.compute(&increment).ok()
  })
  .await?;
  let encoded = prepare_turn(&mut *turns, |s| {
    still_owed(s, at)?;
    Some(increment.encode())
  })
  .await?;
  Some(Prepared {
    evidence,
    increment: Some(increment),
    id,
    computed: Some(computed),
    encoded: Some(encoded),
  })
}

/// The prepared part of `work`'s submit `take` reads (A-787 part 3), counted when taken.
fn take_prepared<T>(
  state: &mut ShardState,
  work: DbVolumeId,
  take: impl FnOnce(&mut Prepared) -> Option<T>,
) -> Option<T> {
  let taken = state
    .refining
    .done
    .get_mut(&work)
    .and_then(|done| done.prepared.as_mut())
    .and_then(take);
  if taken.is_some() {
    state.count(PREPARED_TAKEN, 1);
  }
  taken
}

/// The increment the job prepared for `work`'s submit under `key` with `evidence` (A-787 part 3), taken once.
pub(crate) fn take_increment(
  state: &mut ShardState,
  work: DbVolumeId,
  key: Key,
  evidence: &[[u8; 32]],
) -> Option<slates_merge::engine::Increment> {
  let matches = state
    .refining
    .done
    .get(&work)
    .is_some_and(|done| done.key == key);
  if !matches {
    return None;
  }
  take_prepared(state, work, |prepared| {
    (prepared.evidence == evidence)
      .then(|| prepared.increment.take())
      .flatten()
  })
}

/// The encoding of the increment `id` the job prepared for `work`'s submit (A-787 part 3), taken once.
pub(crate) fn take_encoded(
  state: &mut ShardState,
  work: DbVolumeId,
  id: [u8; 32],
) -> Option<Vec<u8>> {
  take_prepared(state, work, |prepared| {
    (prepared.id == id)
      .then(|| prepared.encoded.take())
      .flatten()
  })
}

/// The verdict on the increment `id` the job computed for `work`'s submit (A-787 part 3), taken once.
pub(crate) fn take_computed(
  state: &mut ShardState,
  work: DbVolumeId,
  id: [u8; 32],
) -> Option<slates_merge::engine::Computed> {
  take_prepared(state, work, |prepared| {
    (prepared.id == id)
      .then(|| prepared.computed.take())
      .flatten()
  })
}

/// The increment's post-state laid over `(doc, plan)` a copy slice a turn ([`prepare`]): the ops whose bytes fit the
/// shard's copy slice within its quantum (its measured memcpy rate), at least one a turn.
async fn laid_post_state(
  at: (DbVolumeId, DbVolumeId, Key),
  (doc, plan): (&OpsDoc, &PostStatePlan),
  turns: &mut JobTurns,
) -> Option<Vec<u8>> {
  let (work, green, key) = at;
  let size = usize::try_from(crate::verbs::post_state_size(doc)).ok()?;
  let mut post = vec![0u8; size];
  let ops = doc.ops.len();
  let mut next = 0usize;
  while next < ops {
    next = prepare_turn(&mut *turns, |s| {
      still_owed(s, at)?;
      let budget = s
        .config
        .copy_slice_bytes_within(s.config.step_quantum_ns())
        .max(1);
      let mut end = next;
      let mut bytes = 0u64;
      while end < ops && (end == next || bytes < budget) {
        bytes = bytes.saturating_add(doc.ops.get(end).map_or(0, |op| op.len));
        end = end.saturating_add(1);
      }
      let w = s.works.get(&work)?;
      let engine = s.greens.get(&green)?;
      let base = crate::work_files::Base {
        green: Some(&*engine),
        fetched: None,
        version: key.0,
        entries: None,
      };
      // A range the green's bytes cannot fill gives the job up, so the composition runs unrefined.
      crate::verbs::lay_ops(
        &mut post,
        (doc, plan),
        (&w.files, base),
        &w.journal,
        next..end,
      )
      .ok()?;
      Some(end)
    })
    .await?;
  }
  Some(post)
}

/// The increment's identity hashed a hash slice a turn ([`prepare`]): the work and its base, then the encoded document
/// and the post-state, as `verbs::compose_increment` hashes them, each turn the shard's hash slice within its quantum
/// (its measured BLAKE3 rate).
async fn hashed_identity(
  at: (DbVolumeId, DbVolumeId, Key),
  (doc_bytes, post): (&[u8], &[u8]),
  turns: &mut JobTurns,
) -> Option<[u8; 32]> {
  let (work, _, key) = at;
  let mut hasher = crate::verbs::identity_hasher(work, key.0);
  let whole = doc_bytes.len().saturating_add(post.len());
  let mut hashed = 0usize;
  while hashed < whole {
    hashed = prepare_turn(&mut *turns, |s| {
      let slice = s
        .config
        .archive_slice_bytes_within(s.config.step_quantum_ns())
        .max(1);
      let end = hashed
        .saturating_add(usize::try_from(slice).unwrap_or(usize::MAX))
        .min(whole);
      let doc_end = doc_bytes.len();
      if hashed < doc_end {
        hasher.update(doc_bytes.get(hashed..end.min(doc_end)).unwrap_or_default());
      }
      if end > doc_end {
        let from = hashed.saturating_sub(doc_end);
        hasher.update(
          post
            .get(from..end.saturating_sub(doc_end))
            .unwrap_or_default(),
        );
      }
      Some(end)
    })
    .await?;
  }
  Some(*hasher.finalize().as_bytes())
}
