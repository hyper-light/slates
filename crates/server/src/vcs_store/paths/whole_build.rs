//! A git green's whole map built in slices after a push, with the submits that need it parked until it ends (A-664's
//! live whole builds, part 1: a push's; main's ruling of 2026-11-20; CLAUDE.md §3 "Bounded work everywhere").
//!
//! **Why.** A push that leaves a green with no map of the version before (its first push, one past a copy) built the
//! map and trees of the whole head in the push's own step (`super::advance`): every file put as git's blob, then the
//! map and trees built of them. A push of a large repository's history is one such step as long as the build, which
//! the watchdog judges a stuck shard. A push carries its own commits, so nothing of its answer needs the map: the map
//! serves the next submit's made commit alone.
//!
//! **What.** [`start`] keeps a build of the green's head as a record ([`Build`]) and a task of the shard drives it
//! ([`drive`], as `super::super::git_encode::settle_encodings` drives an encoding): each [`slice`] puts the head's files,
//! one a unit, lent the room each brings, until the shard's step budget is spent (§4.3's quantum, at least one file),
//! settles the charge, and yields; the last builds the map and trees of what was put and keeps them as the green's head
//! (`Plan::Put`, as a recovery's sliced remake does). The files are read from the green at each slice by their paths,
//! so nothing borrowed outlives a step.
//!
//! **A push meanwhile** (main's ruling of 2026-11-20: tens of thousands of concurrent pushes, so a busy green's build
//! must finish). A push to the green while its build runs leaves the build alone; at its next slice's boundary the build
//! moves to the green's newest head once ([`moved`]): each path the change index names since its version loses what
//! was put of it and is put again from the head, and everything else put stays. So a storm of pushes adds its changed
//! paths to the build, never its whole head again, and the build ends. Only a change holding a copy, which the index
//! names at its destination alone, starts the build again from nothing ([`RESTARTED`]). A commit a submit owed of the
//! version the build began at is then not made over the newer map (`super::super::commits::make_owed` checks its
//! version) and is settled by the push's own move, counted unmade.
//!
//! **The submits that need it wait, parked.** A submit to a green whose build is under way would build the map whole
//! in its own step, the cost this module moves off the shard's steps: so it is parked ([`park`]), its verb's inputs and
//! its reply's route kept with the build, its request deferred as an acceptance that waits is (no completion, no
//! reply). The build's end runs each parked submit again in its own transaction, in the order they came, and answers it
//! on its route (`crate::verbs::resume_parked`); a parked submit is woken by that end alone, never by a poll.
//!
//! **Bounds.** One build a green, so the builds are bounded by the partition's greens; each holds the head's paths and
//! the entries put so far, a version's files at most. The parked submits of a green are bounded by the shard's client
//! credit (`clients_per_shard × slots`, the bound `crate::base_wait` keeps), every request that can be in flight; a
//! submit past it is refused `Overloaded`, counted ([`PARK_REFUSED`]), and a retry of a parked request joins its entry.
//! A parked submit waits at most the node's failover delay (`ShardState::failover_ns`, the most a reply is held, the
//! bound `crate::base_wait` keeps): at each slice one that has waited longer is answered `Overloaded`, unrecorded, so its
//! retry runs, counted ([`OVERDUE`]).
//!
//! **Cancellation.** The build lives in the store's map, keyed by its green, and its task holds the key alone: a task
//! cancelled between slices leaks nothing, and freeing the repository gives back every hold the build took and runs its
//! parked submits again ([`abandon`]), which build the map in their own step as before.
//!
//! **Counters.** [`BEGUN`], [`SLICES`] (the non-vacuity count: a build that ran in one slice did not slice), [`PARKED`],
//! [`PARK_REFUSED`], and the map's own `paths::BUILT` and `paths::REFUSED` at its end.

use std::collections::BTreeMap;
use std::fmt;

use slates_db::catalog::{RepositoryId, VolumeId as DbVolumeId};
use slates_ipc::protocol::Refusal;
use slates_vcs::ContentId;
use slates_vcs::derived::paths::Entry;

use super::{BUILT, Plan, StepRefused, count, give_back, made_holding, map_path};
use crate::state::ShardState;
use crate::verbs::ParkedSubmit;

/// Counter: a whole build begun after a push. Format: a counter name in the daemon's status report.
pub(crate) const BEGUN: &str = "vcs_store.paths.whole_build.begun";
/// Counter: a whole build's slices. Format: a counter name in the daemon's status report.
pub(crate) const SLICES: &str = "vcs_store.paths.whole_build.slices";
/// Counter: a build moved to its green's newest head at a slice's boundary, a push having landed meanwhile. Format: a
/// counter name in the daemon's status report.
pub(crate) const MOVED: &str = "vcs_store.paths.whole_build.moved";
/// Counter: a build started again from nothing, the change since it began holding a copy: zero under any storm of
/// pushes that copy nothing. Format: a counter name in the daemon's status report.
pub(crate) const RESTARTED: &str = "vcs_store.paths.whole_build.restarted";
/// Counter: a submit parked behind a whole build. Format: a counter name in the daemon's status report.
pub(crate) const PARKED: &str = "vcs_store.paths.whole_build.parked";
/// Counter: a parked submit answered `Overloaded` once it waited past the node's failover delay; zero is the healthy
/// reading. Format: a counter name in the daemon's status report.
pub(crate) const OVERDUE: &str = "vcs_store.paths.whole_build.overdue";
/// Counter: an overdue submit's answer not handed on (its route's shard did not take it). Format: a counter name in the
/// daemon's status report.
const UNDELIVERED: &str = "vcs_store.paths.whole_build.overdue_undelivered";
/// Counter: a submit refused at the parked bound. Format: a counter name in the daemon's status report.
pub(crate) const PARK_REFUSED: &str = "vcs_store.paths.whole_build.park_refused";

/// The builds under way, one a green (module docs, "Bounds").
#[derive(Default)]
pub(crate) struct Builds(BTreeMap<DbVolumeId, Build>);

impl fmt::Debug for Builds {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.debug_set().entries(self.0.keys()).finish()
  }
}

/// One green's whole build, between its slices.
struct Build {
  /// The green's repository.
  repository: RepositoryId,
  /// The version it builds the map of: the green's head when it began.
  version: u64,
  /// The head's paths that git holds a file at, in the map's order, read from the green when it began.
  paths: Vec<String>,
  /// How many of them are put.
  at: usize,
  /// What each put path's map entry is.
  entries: BTreeMap<Vec<u8>, Entry>,
  /// The holds the puts took, given back once the map holds them, or at a refusal or an abandon.
  held: Vec<ContentId>,
  /// The submits waiting for it, in the order they came.
  parked: Vec<ParkedSubmit>,
  /// The task driving it.
  task: Option<slates_rt::TaskId>,
  /// Whether the green's map of `version` is kept already (a recovery made it again, and a commit is owed of it): the
  /// build then puts nothing and keeps nothing, and its end makes the owed commit alone.
  kept: bool,
  /// Whether it makes a map again after a store was made again from the log (`super::super::rebuild`): counted
  /// `paths::REMADE` at its end, as a recovery's remake is, not `paths::BUILT`.
  remade: bool,
}

/// Whether `green`'s map is being built whole: a submit to it is parked ([`park`]).
pub(crate) fn building(state: &ShardState, green: DbVolumeId) -> bool {
  state.vcs_stores.whole_builds.0.contains_key(&green)
}

/// Begins `green`'s whole build at `version`, its head, and the task that drives it; a build of it under way is
/// replaced, its holds given back and its parked submits carried over to the new one.
pub(crate) fn start(
  state: &mut ShardState,
  repository: RepositoryId,
  green: DbVolumeId,
  version: u64,
) {
  let parked = state
    .vcs_stores
    .whole_builds
    .0
    .remove(&green)
    .map(|old| retire(state, old))
    .unwrap_or_default();
  let kept = kept_at(state, repository, green) == Some(version);
  let Some(paths) = state.greens.get(&green).map(|engine| {
    if kept {
      Vec::new()
    } else {
      head_paths(&engine)
    }
  }) else {
    crate::verbs::resume_all(state, parked);
    return;
  };
  let task = slates_rt::futures::spawn(drive(green)).ok();
  if let Some(task) = task {
    let _ = slates_rt::futures::detach(task);
  }
  let build = Build {
    repository,
    version,
    paths,
    at: 0,
    entries: BTreeMap::new(),
    held: Vec::new(),
    parked,
    task,
    kept,
    remade: false,
  };
  state.vcs_stores.whole_builds.0.insert(green, build);
  state.count(BEGUN, 1);
}

/// [`start`] for a map made again after `repository`'s store was made again from the log (`super::super::rebuild`):
/// counted as a remake at its end.
pub(crate) fn start_remade(
  state: &mut ShardState,
  repository: RepositoryId,
  green: DbVolumeId,
  version: u64,
) {
  start(state, repository, green, version);
  if let Some(build) = state.vcs_stores.whole_builds.0.get_mut(&green) {
    build.remade = true;
  }
}

/// The version of `green`'s map the store keeps, if any.
fn kept_at(state: &ShardState, repository: RepositoryId, green: DbVolumeId) -> Option<u64> {
  let key = super::super::green_key(green);
  let (_, git) = state.vcs_stores.git_ref(repository)?;
  git.heads.get(&key).map(|head| head.version)
}

/// Starts a build for each commit `repository`'s store owes once it is made again from the log (`super::super::owed`):
/// the green's map is kept already, made again by the recovery, so the build's end makes the commit alone, from what
/// was staged. A commit owed of a version that is not its green's head is left owed, counted unmade.
pub(crate) fn resume_owed(state: &mut ShardState, repository: RepositoryId) {
  let owed: Vec<(u64, u64)> = state
    .vcs_stores
    .git_ref(repository)
    .map(|(_, git)| {
      git
        .owed
        .values()
        .map(|owed| (owed.key, owed.version))
        .collect()
    })
    .unwrap_or_default();
  for (key, version) in owed {
    let green = state
      .db
      .partition()
      .repositories()
      .members_of(repository)
      .map(|member| member.green)
      .find(|green| super::super::green_key(*green) == key);
    let head = green.and_then(|green| state.greens.get(&green).map(|engine| engine.head()));
    match (green, head) {
      (Some(green), Some(head)) if head == version => start(state, repository, green, version),
      _ => state.count(super::super::commits::UNMADE, 1),
    }
  }
}

/// The head's paths that git holds a file at, in the map's order (`super::whole_head`'s names).
fn head_paths(engine: &slates_merge::engine::Green) -> Vec<String> {
  let named = engine
    .files()
    .map(|(path, _)| path)
    .chain(engine.symlinks().map(|(path, _)| path))
    .chain(engine.hardlinks().map(|(path, _)| path))
    .chain(engine.specials().map(|(path, _)| path));
  let mut paths: BTreeMap<Vec<u8>, String> = BTreeMap::new();
  for path in named {
    let name = map_path(&path).to_vec();
    if !name.is_empty() && super::git_file(engine, &path).is_some() {
      paths.insert(name, path);
    }
  }
  paths.into_values().collect()
}

/// Drives `green`'s build a slice at a time, yielding between slices, until it ends or is replaced.
async fn drive(green: DbVolumeId) {
  while crate::state::with_state_counted(|state| {
    let budget = slates_rt::futures::step_budget_ns().unwrap_or(0);
    slice(state, green, budget)
  }) == Some(true)
  {
    slates_rt::futures::yield_now().await;
  }
}

/// One slice of `green`'s build (module docs), putting files until `budget_ns` is spent (at least one): whether more
/// remain. A build no longer kept, or whose green is gone, has none.
fn slice(state: &mut ShardState, green: DbVolumeId, budget_ns: u64) -> bool {
  let Some(mut build) = state.vcs_stores.whole_builds.0.remove(&green) else {
    return false;
  };
  state.count(SLICES, 1);
  let now = slates_vfs::clock::Clock::monotonic_ns(&mut state.clock);
  answer_overdue(state, &mut build, (now, state.failover_ns));
  let head = state.greens.get(&green).map(|engine| engine.head());
  match head {
    Some(head) if head == build.version => {}
    Some(head) => {
      // Pushes moved the green meanwhile: the build moves to its newest head here, at a slice's boundary, carrying what
      // the change since left standing (module docs, "A push meanwhile").
      if !moved(state, green, &mut build, head) {
        state.vcs_stores.whole_builds.0.insert(green, build);
        let repository = build_repository(state, green);
        if let Some(repository) = repository {
          state.count(RESTARTED, 1);
          start(state, repository, green, head);
        }
        return false;
      }
    }
    None => {
      let parked = retire(state, build);
      crate::verbs::resume_all(state, parked);
      return false;
    }
  }
  match put_some(state, (green, budget_ns), &mut build) {
    Ok(()) if build.at < build.paths.len() => {
      state.vcs_stores.whole_builds.0.insert(green, build);
      true
    }
    Ok(()) => {
      finish(state, green, build);
      false
    }
    Err(refused) => {
      count(state, Err(refused), BUILT);
      let parked = retire(state, build);
      crate::verbs::resume_all(state, parked);
      false
    }
  }
}

/// Moves `build` from its version to the green's `head` (module docs, "A push meanwhile"): each path the change index
/// names between them loses what was put of it and is put again from the head, where git holds a file there; what the
/// change left alone stays put. False where the change holds a copy, which the index names at its destination alone, so
/// what it brought cannot be told from the change: the caller starts the build again (counted, [`RESTARTED`]).
fn moved(state: &mut ShardState, green: DbVolumeId, build: &mut Build, head: u64) -> bool {
  let Some(engine) = state.greens.get(&green) else {
    return false;
  };
  let Some(changed) = super::named(&engine, build.version, head) else {
    return false;
  };
  for path in changed {
    build.entries.remove(map_path(&path));
    if !map_path(&path).is_empty() && super::git_file(&engine, &path).is_some() {
      build.paths.push(path);
    }
  }
  build.version = head;
  drop(engine);
  state.count(MOVED, 1);
  true
}

/// The repository of the build kept for `green`.
fn build_repository(state: &ShardState, green: DbVolumeId) -> Option<RepositoryId> {
  state
    .vcs_stores
    .whole_builds
    .0
    .get(&green)
    .map(|build| build.repository)
}

/// Puts the build's next files, one a unit, each lent the room it brings, until the shard's step budget is spent (at
/// least one), then settles the charge.
fn put_some(
  state: &mut ShardState,
  (green, budget): (DbVolumeId, u64),
  build: &mut Build,
) -> Result<(), StepRefused> {
  let began = slates_rt::futures::now_ns();
  let ShardState {
    store,
    vcs_stores,
    greens,
    ..
  } = &mut *state;
  let engine = greens.get(&green).ok_or(StepRefused::NoMap)?;
  let mut answer = Ok(());
  while let Some(path) = build.paths.get(build.at) {
    let Some(file) = super::git_file(&engine, path) else {
      build.at = build.at.saturating_add(1);
      continue;
    };
    let brought = u64::try_from(file.bytes.len()).unwrap_or(u64::MAX);
    super::super::lend(store, vcs_stores, brought);
    let put = super::super::Lent::new(&mut *store, vcs_stores)
      .stores
      .git(build.repository)
      .ok_or(StepRefused::NoMap)
      .and_then(|(content, _)| super::put_file(content, &file, &mut build.held));
    match put {
      Ok(entry) => {
        build.entries.insert(map_path(path).to_vec(), entry);
        build.at = build.at.saturating_add(1);
      }
      Err(refused) => {
        answer = Err(refused);
        break;
      }
    }
    if slates_rt::futures::now_ns().saturating_sub(began) >= budget {
      break;
    }
  }
  let charged_whole = super::super::settle_in(store, vcs_stores);
  drop(engine);
  if !charged_whole {
    state.count(super::super::CHARGE_SHORT, 1);
  }
  answer
}

/// The build's end: the map and trees built of what was put and kept as the green's head, counted; then its parked
/// submits run again.
fn finish(state: &mut ShardState, green: DbVolumeId, build: Build) {
  let key = super::super::green_key(green);
  let Build {
    repository,
    version,
    entries,
    held,
    parked,
    kept,
    remade,
    ..
  } = build;
  if !kept {
    // The map's and trees' nodes are lent the room the shard could admit now, as a step's are (`super::stepped`).
    super::super::lend(&mut state.store, &mut state.vcs_stores, 0);
    let answer = made_holding(
      super::super::Lent::new(&mut state.store, &mut state.vcs_stores).stores,
      repository,
      (key, version),
      Plan::Put(entries),
      held,
    );
    if !super::super::settle_in(&mut state.store, &mut state.vcs_stores) {
      state.count(super::super::CHARGE_SHORT, 1);
    }
    count(state, answer, if remade { super::REMADE } else { BUILT });
  }
  // The commit a submit owes of this version, made from what it staged, before the submits parked behind it run, so
  // each names it as its parent (A-664's live whole builds, part 2).
  super::super::commits::make_owed(state, green);
  crate::verbs::resume_all(state, parked);
}

/// Answers each submit parked behind `build` that has waited past `bound_ns` at `now_ns` (module docs, "Bounds"):
/// `Overloaded`, unrecorded, so its retry runs; counted ([`OVERDUE`]).
fn answer_overdue(state: &mut ShardState, build: &mut Build, (now_ns, bound_ns): (u64, u64)) {
  let (overdue, waiting): (Vec<ParkedSubmit>, Vec<ParkedSubmit>) =
    std::mem::take(&mut build.parked)
      .into_iter()
      .partition(|parked| parked.overdue(now_ns, bound_ns));
  build.parked = waiting;
  for parked in overdue {
    state.count(OVERDUE, 1);
    if !crate::verbs::answer_overdue(state, parked) {
      state.count(UNDELIVERED, 1);
    }
  }
}

/// Parks `submit` behind `green`'s build (module docs): a retry of a parked request joins its entry, taking its route.
/// `Overloaded` past the shard's client credit, or for a green with no build under way.
pub(crate) fn park(
  state: &mut ShardState,
  green: DbVolumeId,
  submit: ParkedSubmit,
) -> Result<(), Refusal> {
  let bound = state
    .config
    .clients_per_shard
    .saturating_mul(usize::try_from(state.config.region.slots).unwrap_or(1));
  let shard = state.shard;
  let Some(build) = state.vcs_stores.whole_builds.0.get_mut(&green) else {
    return Err(Refusal::Overloaded { shard });
  };
  if let Some(joined) = build
    .parked
    .iter_mut()
    .find(|parked| parked.key() == submit.key())
  {
    *joined = submit;
    return Ok(());
  }
  if build.parked.len() >= bound {
    state.count(PARK_REFUSED, 1);
    return Err(Refusal::Overloaded { shard });
  }
  build.parked.push(submit);
  state.count(PARKED, 1);
  Ok(())
}

/// Every build of `repository` given up, its repository freed: its holds given back, its task ended, its parked
/// submits run again.
pub(crate) fn abandon(state: &mut ShardState, repository: RepositoryId) {
  let greens: Vec<DbVolumeId> = state
    .vcs_stores
    .whole_builds
    .0
    .iter()
    .filter(|(_, build)| build.repository == repository)
    .map(|(green, _)| *green)
    .collect();
  for green in greens {
    if let Some(build) = state.vcs_stores.whole_builds.0.remove(&green) {
      let parked = retire(state, build);
      crate::verbs::resume_all(state, parked);
    }
  }
}

/// A build given up: its holds given back and its task ended; its parked submits, for the caller.
fn retire(state: &mut ShardState, build: Build) -> Vec<ParkedSubmit> {
  give_back(&mut state.vcs_stores, build.repository, &build.held);
  if let Some(task) = build.task
    && slates_rt::futures::current_task() != Some(task)
  {
    let _ = slates_rt::futures::cancel(task);
  }
  build.parked
}

/// Drives every build of the shard to its end, a slice at a time, in this step: what a test that pushes and then reads
/// the map does, since a test's step runs no other task between its calls. Each slice is given no budget, so it puts one
/// file whatever the machine's load: a test's count of slices is the build's files, never a measure of time.
#[cfg(test)]
pub(crate) fn settle(state: &mut ShardState) {
  let greens: Vec<DbVolumeId> = state.vcs_stores.whole_builds.0.keys().copied().collect();
  for green in greens {
    while slice(state, green, 0) {}
  }
}

/// One slice of each build of the shard, given no budget (one file): what a test interleaving pushes with a build runs
/// between two pushes.
#[cfg(test)]
pub(crate) fn step_once(state: &mut ShardState) {
  let greens: Vec<DbVolumeId> = state.vcs_stores.whole_builds.0.keys().copied().collect();
  for green in greens {
    slice(state, green, 0);
  }
}

/// Answers the submits parked behind each build of the shard that, at `now_ns`, have waited past `bound_ns`: what a
/// test that cannot wait out a failover delay runs.
#[cfg(test)]
pub(crate) fn answer_overdue_at(state: &mut ShardState, (now_ns, bound_ns): (u64, u64)) {
  let greens: Vec<DbVolumeId> = state.vcs_stores.whole_builds.0.keys().copied().collect();
  for green in greens {
    if let Some(mut build) = state.vcs_stores.whole_builds.0.remove(&green) {
      answer_overdue(state, &mut build, (now_ns, bound_ns));
      state.vcs_stores.whole_builds.0.insert(green, build);
    }
  }
}
