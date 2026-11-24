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
//! The job is a state machine, [`RefineJob`], whose [`RefineJob::turn`] takes one turn on the shard's state; its task
//! calls it between yields and brings back what a turn's read met let go. A test that holds the shard calls the same
//! turn through `refined_now`, and `tests` holds the two drivers equal on one recorded submit each.
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

#[cfg(test)]
mod tests;

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
  // A test that holds the shard takes the job's turns itself (`refined_now`).
  #[cfg(test)]
  if HELD_FOR_TEST.with(std::cell::Cell::get) {
    let key = owed.key;
    HELD_JOBS.with(|held| held.borrow_mut().push(RefineJob::new(work, green, owed)));
    state.refining.running.insert(work, key);
    state.count(REFINE_STARTED, 1);
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

/// Format: copy steps that met a named text's let-go window, its windows brought back and the step copied again
/// (A-760 (b) 4d).
const COPY_BROUGHT_BACK: &str = "refining.copy_brought_back";
/// Format: copy steps whose let-go windows the node's store did not give back: the job given up, unrefined.
const COPY_NOT_BROUGHT_BACK: &str = "refining.copy_not_brought_back";
/// Format: a copy step copied again over windows brought back that met one let go again: a bug, the job given up.
const COPY_AWAY_AGAIN: &str = "refining.copy_away_again";

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

/// The next slice of the named text `identity` into `into`, at most `slice` bytes, read through the shard's content plane
/// (A-760 (b) 4d): whether `into` now holds it all; `None` where the plane refused (a let-go window noted for the job).
fn copy_named(
  state: &ShardState,
  identity: slates_merge::named::Identity,
  into: &mut Vec<u8>,
  slice: usize,
) -> Option<bool> {
  let total = usize::try_from(identity.len).ok()?;
  if into.capacity() < total {
    into.reserve_exact(total.saturating_sub(into.len()));
  }
  let from = into.len();
  let take = slice.min(total.saturating_sub(from));
  let plane = crate::vcs_store::named_objects::plane(&state.vcs_stores, &state.store.content);
  let page = crate::vcs_store::named_objects::page_of(
    ("", None),
    slates_merge::engine::ContentAt::Named(identity),
    (u64::try_from(from).ok()?, u64::try_from(take).ok()?),
    plane.as_ref(),
  )
  .ok()?;
  into.extend_from_slice(&page);
  Some(into.len() == total)
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
        let content = engine.content_ref_at(&copy.base_path, base_version)?;
        // A base held by its content-plane object alone is read through the plane a slice a step (A-760 (b) 4d); a
        // let-go window is noted (`named_objects::away_scope`) for the job to bring back and copy again.
        let bytes = match content {
          slates_merge::engine::ContentAt::Named(identity) => {
            return copy_named(state, identity, &mut copy.base, slice);
          }
          bytes => crate::vcs_store::named_objects::bytes_of(&copy.base_path, bytes).ok()?,
        };
        return copy_from(bytes, &mut copy.base, slice);
      }
      if let Some(replica) = state
        .merge
        .replicas
        .get(&slates_db::register::ObjectId(green.bytes))
      {
        let content = replica.content_ref_at(&copy.base_path, base_version)?;
        let bytes = crate::vcs_store::named_objects::bytes_of(&copy.base_path, content).ok()?;
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
      green_id: Some(green),
      fetched: None,
      version,
      entries: None,
      plane: crate::vcs_store::named_objects::plane(&state.vcs_stores, &state.store.content),
    });
  }
  if let Some(replica) = state
    .merge
    .replicas
    .get(&slates_db::register::ObjectId(green.bytes))
  {
    return f(crate::work_files::Base {
      green: Some(replica),
      green_id: None,
      fetched: None,
      version,
      entries: None,
      plane: None,
    });
  }
  f(crate::work_files::Base {
    green: None,
    green_id: None,
    fetched: crate::work_home::parked_fetched(state, work),
    version,
    entries: None,
    plane: None,
  })
}

/// The job's task and the key it runs under.
type Job = (
  core::pin::Pin<Box<dyn core::future::Future<Output = ()>>>,
  Key,
);

/// `work`'s refinement job for `owed` (module docs): its task runs [`RefineJob::turn`] on the shard's state a turn at a
/// time, yielding between turns, and brings back what a turn's read met let go before the turn is taken again. A test
/// that holds the shard takes the same turns through [`refined_now`].
fn job(work: DbVolumeId, green: DbVolumeId, owed: Owed) -> Job {
  let key = owed.key;
  (Box::pin(task_of(RefineJob::new(work, green, owed))), key)
}

/// `job`'s task (module docs).
async fn task_of(mut job: RefineJob) {
  loop {
    match crate::state::with_state(|s| job.turn(s)) {
      Some(Turned::More) => slates_rt::futures::yield_now().await,
      // A read of a let-go window (A-760 (b) 4d) is never lost: its windows are brought back and kept, and the same
      // slice copied again at once; a store that cannot give them back gives the job up, counted.
      Some(Turned::BringBack(away)) => {
        let brought = crate::vcs_store::named_objects::bring_back_reads(&away)
          .await
          .ok();
        let _ = crate::state::with_state(|s| job.brought(s, brought));
      }
      Some(Turned::Done) | None => return,
    }
  }
}

#[cfg(test)]
thread_local! {
  /// Set by a test that holds the shard: a job [`start`]s held for [`refined_now`], not spawned.
  pub(crate) static HELD_FOR_TEST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
  /// The jobs a test's shard holds for [`refined_now`] ([`HELD_FOR_TEST`]).
  static HELD_JOBS: std::cell::RefCell<Vec<RefineJob>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Every held job run to its end now, a turn at a time through [`RefineJob::turn`], as its task takes them: for a test
/// that holds the shard, so the task cannot run. A test's shard keeps its landed packs, so no copy meets a let-go window;
/// were one met, the job gives up, as one whose windows the store cannot give back does.
#[cfg(test)]
pub(crate) fn refined_now(state: &mut ShardState) {
  while let Some(mut job) = HELD_JOBS.with(|held| held.borrow_mut().pop()) {
    loop {
      match job.turn(state) {
        Turned::More => {}
        Turned::BringBack(_) => job.brought(state, None),
        Turned::Done => break,
      }
    }
  }
}

/// What a turn of a job left ([`RefineJob::turn`]).
pub(crate) enum Turned {
  /// More turns are owed: the task yields first.
  More,
  /// A copy step met these let-go windows: brought back, then the same step taken again.
  BringBack(Vec<crate::vcs_store::named_objects::AwayRead>),
  /// The job ended: its result kept, or given up.
  Done,
}

/// What a job's refinement gives back: the refined document and plan, the refiner's scratch, and the copies it read.
type Refined = (
  OpsDoc,
  PostStatePlan,
  slates_merge::refine::Refiner,
  Vec<Copy>,
);

/// A refinement job between its turns (module docs), its stages in order: the copies a slice a turn, the refinement a
/// pace turn a step, what it held given back a buffer a turn, and the parked submit's increment, verdict and encoding
/// a bounded slice a turn (A-787 part 3). Each turn is one call over the shard's state ([`RefineJob::turn`]), so the
/// task and a test's driver take the same turns.
pub(crate) struct RefineJob {
  work: DbVolumeId,
  green: DbVolumeId,
  key: Key,
  stage: Stage,
  turns: TurnCounts,
  longest: u64,
  /// The windows a copy step's read brought back, kept until that step is copied again (A-760 (b) 4d).
  pins: Vec<crate::vcs_store::named_objects::ReadPin>,
}

/// Where a job stands ([`RefineJob`]).
enum Stage {
  /// Copying pair `index`'s `side`.
  Copying {
    copies: Vec<Copy>,
    index: usize,
    side: Side,
    doc: OpsDoc,
    plan: PostStatePlan,
  },
  /// The refinement, driven a pace turn a step.
  Refining {
    refining: core::pin::Pin<Box<dyn core::future::Future<Output = Refined>>>,
    pace: slates_merge::pace::Pace,
  },
  /// What it held given back a buffer a turn.
  Releasing {
    doc: OpsDoc,
    plan: PostStatePlan,
    copies: std::vec::IntoIter<Copy>,
    refiner: slates_merge::refine::Refiner,
  },
  /// The parked submit's increment, verdict and encoding.
  Preparing {
    doc: OpsDoc,
    plan: PostStatePlan,
    preparing: Preparing,
  },
  /// Its result to keep at its next turn.
  Finishing {
    refined: Option<(OpsDoc, PostStatePlan)>,
    prepared: Option<Prepared>,
  },
  /// Ended.
  Done,
}

/// Where a preparation stands ([`Stage::Preparing`]).
enum Preparing {
  /// Its evidence not yet read.
  Begun,
  /// The post-state laid from op `next` on.
  Laying {
    evidence: Vec<[u8; 32]>,
    post: Vec<u8>,
    next: usize,
  },
  /// The document's encoding next.
  Encoding {
    evidence: Vec<[u8; 32]>,
    post: Vec<u8>,
  },
  /// The identity hashed from byte `hashed` of the document's encoding and the post-state.
  Hashing {
    evidence: Vec<[u8; 32]>,
    post: Vec<u8>,
    doc_bytes: Vec<u8>,
    hasher: Box<blake3::Hasher>,
    hashed: usize,
  },
  /// The increment made; checked whole against the one the submit's step composes (a debug build's oracle, A-821).
  Checking {
    evidence: Vec<[u8; 32]>,
    increment: Box<slates_merge::engine::Increment>,
  },
  /// Its verdict next.
  Computing {
    evidence: Vec<[u8; 32]>,
    increment: Box<slates_merge::engine::Increment>,
  },
  /// Its encoding next.
  Encoded {
    evidence: Vec<[u8; 32]>,
    increment: Box<slates_merge::engine::Increment>,
    computed: Box<slates_merge::engine::Computed>,
  },
}

impl RefineJob {
  /// `work`'s job for `owed`, at its first copy.
  fn new(work: DbVolumeId, green: DbVolumeId, owed: Owed) -> RefineJob {
    let Owed {
      key,
      doc,
      plan,
      pairs,
    } = owed;
    let copies = pairs
      .into_iter()
      .map(|(base_path, path)| Copy {
        base_path,
        path,
        base: Vec::new(),
        post: Vec::new(),
      })
      .collect();
    RefineJob {
      work,
      green,
      key,
      stage: Stage::Copying {
        copies,
        index: 0,
        side: Side::Base,
        doc,
        plan,
      },
      turns: (0, [0; PAST_MULTIPLES.len()]),
      longest: 0,
      pins: Vec::new(),
    }
  }

  /// Takes the job's next turn over `state` (module docs): what it then owes. Steps that end within a turn go on in it,
  /// as the task's one poll went on through them; a turn that ends yields, or brings back what its read met let go.
  pub(crate) fn turn(&mut self, state: &mut ShardState) -> Turned {
    loop {
      match std::mem::replace(&mut self.stage, Stage::Done) {
        Stage::Copying {
          mut copies,
          index,
          side,
          doc,
          plan,
        } => {
          if index >= copies.len() {
            self.stage = Stage::Refining {
              refining: refining_of(doc, plan, copies),
              pace: slates_merge::pace::Pace::new(slates_rt::futures::now_ns),
            };
            continue;
          }
          let began = slates_rt::futures::now_ns();
          // Half the quantum's copying: the turn's budget, as a refinement turn's is.
          let slice = state
            .config
            .copy_slice_bytes_within(state.config.step_quantum_ns() / 2);
          let slice = usize::try_from(slice).unwrap_or(usize::MAX).max(1);
          let at = (self.work, self.green, self.key.0);
          let (step, away) = crate::vcs_store::named_objects::away_scope(|| {
            copies
              .get_mut(index)
              .and_then(|copy| copy_step(state, at, (copy, side, slice)))
          });
          self.counted(state, began);
          if step.is_none() && !away.is_empty() && self.pins.is_empty() {
            self.stage = Stage::Copying {
              copies,
              index,
              side,
              doc,
              plan,
            };
            return Turned::BringBack(away);
          }
          if step.is_none() && !away.is_empty() {
            state.count(COPY_AWAY_AGAIN, 1);
          }
          self.pins = Vec::new();
          match step {
            Some(true) => {
              let (index, side) = match side {
                Side::Base => (index, Side::Post),
                Side::Post => (index.saturating_add(1), Side::Base),
              };
              self.stage = Stage::Copying {
                copies,
                index,
                side,
                doc,
                plan,
              };
            }
            Some(false) => {
              self.stage = Stage::Copying {
                copies,
                index,
                side,
                doc,
                plan,
              };
              return Turned::More;
            }
            None => {
              self.stage = Stage::Finishing {
                refined: None,
                prepared: None,
              };
            }
          }
        }
        Stage::Refining { mut refining, pace } => {
          let budget = state.config.step_quantum_ns() / 2;
          let began = slates_rt::futures::now_ns();
          pace.turn(began.saturating_add(budget.max(1)));
          let polled = pace.drive(refining.as_mut());
          self.counted(state, began);
          match polled {
            core::task::Poll::Ready((doc, plan, refiner, copies)) => {
              self.stage = Stage::Releasing {
                doc,
                plan,
                copies: copies.into_iter(),
                refiner,
              };
            }
            core::task::Poll::Pending => {
              self.stage = Stage::Refining { refining, pace };
              return Turned::More;
            }
          }
        }
        // What the job held, given back a buffer a turn: freed together, the copies and the scratch of a 2 MiB rewrite
        // cost about a millisecond in one step (A-787 part 2).
        Stage::Releasing {
          doc,
          plan,
          mut copies,
          mut refiner,
        } => {
          let began = slates_rt::futures::now_ns();
          let done = match copies.next() {
            Some(copy) => {
              drop(copy);
              false
            }
            None => refiner.release_step(),
          };
          self.counted(state, began);
          if done {
            self.stage = Stage::Preparing {
              doc,
              plan,
              preparing: Preparing::Begun,
            };
            continue;
          }
          self.stage = Stage::Releasing {
            doc,
            plan,
            copies,
            refiner,
          };
          return Turned::More;
        }
        Stage::Preparing {
          doc,
          plan,
          preparing,
        } => match self.prepare_step(state, (&doc, &plan), preparing) {
          Prepared_::Next(preparing, yielded) => {
            self.stage = Stage::Preparing {
              doc,
              plan,
              preparing,
            };
            if yielded {
              return Turned::More;
            }
          }
          Prepared_::Ended(prepared, yielded) => {
            self.stage = Stage::Finishing {
              refined: Some((doc, plan)),
              prepared,
            };
            if yielded {
              return Turned::More;
            }
          }
        },
        Stage::Finishing { refined, prepared } => {
          finish(
            state,
            self.work,
            (self.key, refined, prepared),
            (self.turns, self.longest),
          );
          return Turned::Done;
        }
        Stage::Done => return Turned::Done,
      }
    }
  }

  /// What the task brought back for the copy step a turn stopped at ([`Turned::BringBack`]): kept, and the step taken
  /// again; none brought, the job given up, counted.
  pub(crate) fn brought(
    &mut self,
    state: &mut ShardState,
    brought: Option<Vec<crate::vcs_store::named_objects::ReadPin>>,
  ) {
    match brought {
      Some(pins) => {
        self.pins = pins;
        state.count(COPY_BROUGHT_BACK, 1);
      }
      None => {
        state.count(COPY_NOT_BROUGHT_BACK, 1);
        self.pins = Vec::new();
        finish(
          state,
          self.work,
          (self.key, None, None),
          (self.turns, self.longest),
        );
        self.stage = Stage::Done;
      }
    }
  }

  /// Counts a job turn begun at `began` ([`turn`]).
  fn counted(&mut self, state: &ShardState, began: u64) {
    turn(
      &mut self.turns,
      &mut self.longest,
      (began, state.config.step_quantum_ns()),
    );
  }

  /// One step of the parked submit's preparation over `(doc, plan)`, the refinement's result (A-787 part 3): the
  /// post-state a copy slice a turn (the shard's measured memcpy rate within its quantum), the identity a hash slice a
  /// turn (its measured BLAKE3 rate), and the document's encoding, the verdict and the increment's encoding a turn each.
  /// Whether the step was a bounded turn the task yields after, as each of the task's `prepare_turn` did; ended with
  /// none where nothing waits for it or the work or the green changed, the submit then composing and judging in its step.
  fn prepare_step(
    &mut self,
    state: &mut ShardState,
    (doc, plan): (&OpsDoc, &PostStatePlan),
    preparing: Preparing,
  ) -> Prepared_ {
    let (work, green, key) = (self.work, self.green, self.key);
    let at = (work, green, key);
    match preparing {
      Preparing::Begun => {
        let evidence = still_owed(state, at)
          .and_then(|()| crate::verbs::parked_local_evidence(state.refining.parked.get(&work)?));
        let size = crate::verbs::post_state_size(doc);
        match (evidence, usize::try_from(size).ok()) {
          (Some(evidence), Some(size)) => Prepared_::Next(
            Preparing::Laying {
              evidence,
              post: vec![0u8; size],
              next: 0,
            },
            false,
          ),
          _ => Prepared_::Ended(None, false),
        }
      }
      Preparing::Laying {
        evidence,
        mut post,
        next,
      } => {
        let ops = doc.ops.len();
        if next >= ops {
          return Prepared_::Next(Preparing::Encoding { evidence, post }, false);
        }
        let laid = self.prepare_turn(state, |s| {
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
            plane: crate::vcs_store::named_objects::plane(&s.vcs_stores, &s.store.content),
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
        });
        match laid {
          Some(next) => Prepared_::Next(
            Preparing::Laying {
              evidence,
              post,
              next,
            },
            true,
          ),
          None => Prepared_::Ended(None, true),
        }
      }
      Preparing::Encoding { evidence, post } => {
        match self.prepare_turn(state, |s| {
          still_owed(s, at)?;
          Some(doc.encode())
        }) {
          Some(doc_bytes) => Prepared_::Next(
            Preparing::Hashing {
              evidence,
              post,
              doc_bytes,
              hasher: Box::new(crate::verbs::identity_hasher(work, key.0)),
              hashed: 0,
            },
            true,
          ),
          None => Prepared_::Ended(None, true),
        }
      }
      Preparing::Hashing {
        evidence,
        post,
        doc_bytes,
        mut hasher,
        hashed,
      } => {
        let whole = doc_bytes.len().saturating_add(post.len());
        if hashed >= whole {
          let id = *hasher.finalize().as_bytes();
          drop(doc_bytes);
          let increment = slates_merge::engine::Increment {
            id,
            base: key.0,
            doc: doc.clone(),
            post_state: post,
            evidence: evidence.clone(),
            named: Default::default(),
          };
          let increment = Box::new(increment);
          return if cfg!(debug_assertions) {
            Prepared_::Next(
              Preparing::Checking {
                evidence,
                increment,
              },
              false,
            )
          } else {
            Prepared_::Next(
              Preparing::Computing {
                evidence,
                increment,
              },
              false,
            )
          };
        }
        let end = self.prepare_turn(state, |s| {
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
        });
        match end {
          Some(hashed) => Prepared_::Next(
            Preparing::Hashing {
              evidence,
              post,
              doc_bytes,
              hasher,
              hashed,
            },
            true,
          ),
          None => Prepared_::Ended(None, true),
        }
      }
      // The oracle, in a debug build: the increment composed whole as the submit's own step composes it (main's
      // `compose_increment`), compared whole with the sliced one, in a turn of its own (A-821).
      Preparing::Checking {
        evidence,
        increment,
      } => {
        let _ = self.prepare_turn(state, |s| {
          let whole = crate::verbs::composed_whole(
            s,
            (work, green, key.0),
            (doc.clone(), plan.clone()),
            evidence.clone(),
          )?;
          s.count(PREPARED_CHECKED, 1);
          if whole != *increment {
            s.count(PREPARED_MISMATCH, 1);
          }
          Some(())
        });
        Prepared_::Next(
          Preparing::Computing {
            evidence,
            increment,
          },
          true,
        )
      }
      Preparing::Computing {
        evidence,
        increment,
      } => {
        match self.prepare_turn(state, |s| {
          still_owed(s, at)?;
          s.greens.get(&green)?.compute(&increment).ok()
        }) {
          Some(computed) => Prepared_::Next(
            Preparing::Encoded {
              evidence,
              increment,
              computed: Box::new(computed),
            },
            true,
          ),
          None => Prepared_::Ended(None, true),
        }
      }
      Preparing::Encoded {
        evidence,
        increment,
        computed,
      } => {
        match self.prepare_turn(state, |s| {
          still_owed(s, at)?;
          Some(increment.encode())
        }) {
          Some(encoded) => {
            let id = increment.id;
            Prepared_::Ended(
              Some(Prepared {
                evidence,
                increment: Some(*increment),
                id,
                computed: Some(*computed),
                encoded: Some(encoded),
              }),
              true,
            )
          }
          None => Prepared_::Ended(None, true),
        }
      }
    }
  }

  /// One bounded turn of a preparation: runs `step` on the shard's state and counts the turn and the slice; the task
  /// yields after it ([`Prepared_`]).
  fn prepare_turn<T>(
    &mut self,
    state: &mut ShardState,
    step: impl FnOnce(&mut ShardState) -> Option<T>,
  ) -> Option<T> {
    let began = slates_rt::futures::now_ns();
    let done = step(state);
    state.count(PREPARE_SLICES, 1);
    self.counted(state, began);
    done
  }
}

/// What a preparation's step left ([`RefineJob::prepare_step`]): its next stage, or its end (what it prepared, or none);
/// each with whether it was a bounded turn the task yields after.
enum Prepared_ {
  Next(Preparing, bool),
  Ended(Option<Prepared>, bool),
}

/// The refinement of `doc` and `plan` over `copies` (`slates_merge::refine::refine_rewrites_paced`), as one future that
/// owns them and gives them back with the refiner's scratch ([`Refined`]), driven a pace turn a step.
fn refining_of(
  doc: OpsDoc,
  plan: PostStatePlan,
  copies: Vec<Copy>,
) -> core::pin::Pin<Box<dyn core::future::Future<Output = Refined>>> {
  Box::pin(async move {
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
  })
}

/// Counts a job turn begun at `began`, the shard's step quantum `quantum`: `turns` the turns and those past each
/// multiple of the quantum, `longest` the longest, nanoseconds.
fn turn(turns: &mut TurnCounts, longest: &mut u64, (began, quantum): (u64, u64)) {
  let took = slates_rt::futures::now_ns().saturating_sub(began);
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
  (key, refined, prepared): (Key, Option<(OpsDoc, PostStatePlan)>, Option<Prepared>),
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

/// What the work still is and what the green holds for a prepare's turn: `None` once the work changed (another key),
/// the green is not on this shard, or no local submit waits for the refinement.
fn still_owed(s: &ShardState, (work, green, key): (DbVolumeId, DbVolumeId, Key)) -> Option<()> {
  let w = s.works.get(&work)?;
  ((w.base_version, w.journal.len()) == key && s.greens.contains_key(&green)).then_some(())
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
