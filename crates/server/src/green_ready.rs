//! The one door to a green's content (P2, main's ruling of 2026-11-21 05:12; w-git's request,
//! inbox/w-rewrite-ops-from-w-git-p2.md): a green may owe a push (`slates_merge::push::DeferredPush`, a created ref's
//! tree not yet built), and every request that reaches a green's content comes through [`green_ready`] first.
//!
//! **Why.** A git create composed its ref's whole tree at the push: git/git's history push makes 1,027 greens of a
//! 4,876-file tree each, which could not land. A created green now owes its push instead (`BranchChange::CreateDeferred`),
//! and its tree is built only when the green is first reached: derived data, built a unit a turn (A-647).
//!
//! **What.**
//! - [`green_ready`] answers [`Ready`](GreenReady::Ready) for a green that holds its content, and for one that owes a
//!   push starts (or joins) its materialization and answers [`Materializing`](GreenReady::Materializing).
//! - The materialization is a task of the shard: the owed push's `Building` advanced a turn at a time over the objects
//!   the green's tool gives ([`ObjectsOf`], which the tool registers; git's over its store), then `Green::materialize`
//!   commits the built work as the owed version. It is today's eager create, deferred: one builder over one input.
//! - A recorded request on an owing green, or on a work over one, parks (`verbs::run_recorded` through
//!   [`park_request`]); an owner's step of a submit parks too (`verbs::owner_judge`). Each runs again, in the order they
//!   came, once the green is materialized. A request no answer can wait on (no route, a bare dispatch) is answered
//!   `Overloaded`, unrecorded, so its retry finds the green made.
//! - `Greens::get` / `get_mut` lend no owing green (counted, `Greens::owing_reached`): nothing reads its content
//!   around this door, and a background caller (a checkpoint, a fold) does nothing to it.
//!
//! **Bounds.** The parked requests of the shard are bounded by its client seats, as a whole build's are; a request past
//! that is refused `Overloaded`. A materialization's turn runs under half the shard's step quantum.
//!
//! **Retention.** A build's work is secured against the green's retention room before it commits, as a submit's
//! admission secures one: past it the green keeps owing, counted [`MATERIALIZE_OVER_ROOM`], its requests answered
//! `BudgetExceeded`. Settled after.

use std::collections::BTreeMap;

use slates_db::catalog::{RepositoryId, VolumeId as DbVolumeId};
use slates_ipc::protocol::{Refusal, ReplyBody, RequestBody};
use slates_merge::push::{Building, PushObjects};

use crate::state::ShardState;
use crate::verbs::ParkedSubmit;

/// Counter: a green materialized (its owed push built and committed). Format: a counter name in the status report.
pub(crate) const MATERIALIZED: &str = "merge.green.materialized";
/// Counter: a materialization whose build or commit was refused, the green left owing. Format: a counter name.
pub(crate) const MATERIALIZE_REFUSED: &str = "merge.green.materialize_refused";
/// Counter: a materialization refused at the green's retention room before it committed (its history and change index
/// secured as a submit's admission secures them), the green left owing and its requests answered `BudgetExceeded`.
/// Format: a counter name.
pub(crate) const MATERIALIZE_OVER_ROOM: &str = "merge.green.materialize_over_room";
/// Counter: a request parked on a green's materialization. Format: a counter name.
pub(crate) const PARKED: &str = "merge.green.materialize_parked";
/// Counter: a request refused `Overloaded` at the parked bound, or with no route to park on. Format: a counter name.
pub(crate) const PARK_REFUSED: &str = "merge.green.materialize_park_refused";
/// Counter: a materialized green nobody wrote to, dropped back to owing its push ([`keep_within_bound`]). Format: a
/// counter name.
pub(crate) const DEMATERIALIZED: &str = "merge.green.dematerialized";

#[cfg(test)]
thread_local! {
  /// A test's bound on the untouched greens kept materialized, in place of the derived one.
  pub(crate) static TEST_KEEP_BOUND: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
thread_local! {
  /// A test's objects for owed pushes, read in place of the tools' (`vcs_store::owed_push_objects`).
  pub(crate) static TEST_OBJECTS: std::cell::Cell<Option<TestObjects>> = const { std::cell::Cell::new(None) };
}

/// A test's objects for owed pushes: where they come from, for any repository.
#[cfg(test)]
pub(crate) type TestObjects = fn() -> Box<dyn PushObjects>;

/// Hands `read` the objects a green's owed push reads: the tool's store, for the repository the green is a member of
/// (`vcs_store::with_owed_push_objects`; git's lands with w-git's half of P2), or a test's. Handed over rather than
/// returned, so a view that borrows the shard's state and its own empty push outlives nothing (w-git's ask). Whether
/// there were any.
fn with_objects(
  state: &ShardState,
  repository: Option<RepositoryId>,
  read: &mut dyn FnMut(&dyn PushObjects),
) -> bool {
  #[cfg(test)]
  if let Some(objects) = TEST_OBJECTS.with(std::cell::Cell::get) {
    read(&*objects());
    return true;
  }
  crate::vcs_store::with_owed_push_objects(state, repository, read)
}

/// The shard's materializations: the builds under way and the requests parked on each green.
#[derive(Default)]
pub(crate) struct Materializations {
  /// The build under way for each green that owes a push and has been reached.
  building: BTreeMap<DbVolumeId, Building>,
  /// The requests parked on each green, in the order they came.
  parked: BTreeMap<DbVolumeId, Vec<ParkedSubmit>>,
  /// The greens materialized here, least recently read first: those nobody wrote to since may be dropped back to owing
  /// ([`keep_within_bound`]).
  kept: std::collections::VecDeque<DbVolumeId>,
}

/// Whether a green's content can be read now ([`green_ready`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GreenReady {
  /// It holds its content (or is not a green here): go on.
  Ready,
  /// It owes a push, whose build is under way: wait for it.
  Materializing,
}

/// The door (module docs): [`GreenReady::Ready`] for a green that holds its content or is no green here; for one that
/// owes a push, its materialization started where none runs, and [`GreenReady::Materializing`].
pub(crate) fn green_ready(state: &mut ShardState, green: DbVolumeId) -> GreenReady {
  if !state.greens.is_owing(&green) {
    // Read now: the most recently read of the greens kept materialized.
    let kept = &mut state.materializations.kept;
    if let Some(at) = kept.iter().position(|held| *held == green) {
      kept.remove(at);
      kept.push_back(green);
    }
    return GreenReady::Ready;
  }
  if !state.materializations.building.contains_key(&green) {
    start(state, green);
  }
  GreenReady::Materializing
}

/// Whether `green`'s materialization is under way: started by [`green_ready`] and neither made nor refused yet.
pub(crate) fn is_building(state: &ShardState, green: DbVolumeId) -> bool {
  state.materializations.building.contains_key(&green)
}

/// The green owing a push that `body` reaches, if any: its volume when that is such a green, or the green of the work it
/// names; for a branch made from another, the green it is made from, named by its repository and branch (git's names:
/// only a git create owes its push).
pub(crate) fn owing_green_of(state: &ShardState, body: &RequestBody) -> Option<DbVolumeId> {
  if let RequestBody::CreateBranch {
    repository, from, ..
  } = body
  {
    let repositories = state.db.partition().repositories();
    let green = repositories
      .repository_named(repository)
      .and_then(|record| repositories.branch(record.id, from))?;
    return state.greens.is_owing(&green).then_some(green);
  }
  let volume = crate::verbs::to_db_volume(body.volume()?);
  let green = state.works.get(&volume).map_or(volume, |work| work.green);
  state.greens.is_owing(&green).then_some(green)
}

/// Parks `parked` on `green`'s materialization, which [`green_ready`] has started; refused `Overloaded` at the bound.
pub(crate) fn park(
  state: &mut ShardState,
  green: DbVolumeId,
  parked: ParkedSubmit,
) -> Result<(), Refusal> {
  let shard = state.shard;
  let bound = state
    .config
    .clients_per_shard
    .saturating_mul(usize::try_from(state.config.region.slots).unwrap_or(1));
  if let Some(joined) = state
    .materializations
    .parked
    .get_mut(&green)
    .and_then(|parked_here| {
      parked_here
        .iter_mut()
        .find(|held| held.key() == parked.key())
    })
  {
    *joined = parked;
    return Ok(());
  }
  let held: usize = state.materializations.parked.values().map(Vec::len).sum();
  if held >= bound {
    state.count(PARK_REFUSED, 1);
    return Err(Refusal::Overloaded { shard });
  }
  state
    .materializations
    .parked
    .entry(green)
    .or_default()
    .push(parked);
  state.count(PARKED, 1);
  Ok(())
}

/// The running recorded request `body`, by `principal`, parked on `green`'s materialization (`verbs::run_recorded`): no
/// reply now, the request run again and answered on its route once the green is made. `Overloaded` with no recorded
/// request to park, or at the bound.
pub(crate) fn park_request(
  state: &mut ShardState,
  green: DbVolumeId,
  (client_id, principal, body): (u32, &slates_db::catalog::Principal, RequestBody),
) -> Option<ReplyBody> {
  let shard = state.shard;
  let Some((origin, id)) = state.current_request else {
    state.count(PARK_REFUSED, 1);
    return Some(crate::verbs::refused(Refusal::Overloaded { shard }));
  };
  let route = state.reply_route.take();
  let parked = crate::verbs::parked_request(
    (origin, id),
    route,
    (client_id, principal.clone(), body),
    state,
  );
  match park(state, green, parked) {
    Ok(()) => None,
    Err(refusal) => Some(crate::verbs::refused(refusal)),
  }
}

/// Runs every materialization under way to its end now, a turn at a time, as its task would: for a test that drives a
/// push or a read synchronously and holds the shard, so the task cannot run until it returns. The task, run later,
/// finds its build done.
#[cfg(test)]
pub(crate) fn make_building_now(state: &mut ShardState) {
  let greens: Vec<DbVolumeId> = state.materializations.building.keys().copied().collect();
  for green in greens {
    while !turn(state, green) {}
  }
}

/// `green` reached through the door and, if it owed its push, made now ([`make_building_now`]): for a test that reads
/// it synchronously.
#[cfg(test)]
pub(crate) fn made_now(state: &mut ShardState, green: DbVolumeId) {
  if green_ready(state, green) == GreenReady::Materializing {
    make_building_now(state);
  }
}

/// Starts `green`'s materialization: its owed push's build, driven by a task of the shard a turn at a time.
fn start(state: &mut ShardState, green: DbVolumeId) {
  let pace = slates_merge::pace::Pace::new(slates_rt::futures::now_ns);
  let Some(building) = state
    .greens
    .lend(&green)
    .and_then(|engine| engine.materializing(pace))
  else {
    return;
  };
  state.materializations.building.insert(green, building);
  let started = slates_rt::futures::spawn(materialize(green))
    .and_then(slates_rt::futures::detach)
    .is_ok();
  if !started {
    state.materializations.building.remove(&green);
    refuse_parked(state, green, Refusal::ContentUnavailable);
  }
}

/// [`start`]'s task: a turn of the build at each of the shard's turns until it is done, then the commit.
async fn materialize(green: DbVolumeId) {
  loop {
    match crate::state::with_state(|state| turn(state, green)) {
      Some(false) => slates_rt::futures::yield_now().await,
      Some(true) | None => return,
    }
  }
}

/// One turn of `green`'s build, under half the shard's step quantum: whether it is done (built and committed, or
/// refused, its parked requests run again or answered).
fn turn(state: &mut ShardState, green: DbVolumeId) -> bool {
  let Some(mut building) = state.materializations.building.remove(&green) else {
    return true;
  };
  let repository = state
    .db
    .partition()
    .repositories()
    .member(green)
    .map(|member| member.repository);
  let quantum = state.config.step_quantum_ns() / 2;
  building
    .pace()
    .turn(slates_rt::futures::now_ns().saturating_add(quantum.max(1)));
  // The build's turn over the green and its tool's objects; why it cannot run, where it cannot. Read with the stores
  // lent room and the shard's chunk store, as a push's build reads them: a tool's objects lie in windows that chunk
  // store keeps, and a settled content store admits no parse.
  let advanced: Option<Result<slates_merge::push::Built, String>> =
    crate::vcs_store::with_room(state, |state| match state.greens.lend(&green) {
      Some(engine) => {
        let mut turned = None;
        let found = with_objects(state, repository, &mut |objects| {
          turned = Some(building.advance(&engine, objects));
        });
        match turned {
          Some(advanced) if found => {
            advanced.map(|built| built.map_err(|refused| format!("{refused:?}")))
          }
          _ => Some(Err("no objects for the green's repository".to_owned())),
        }
      }
      None => Some(Err("the green is not here".to_owned())),
    });
  match advanced {
    None => {
      state.materializations.building.insert(green, building);
      false
    }
    Some(Ok(built)) => {
      // Secured before it commits, as a submit's admission secures a work's history and change index
      // (`verbs::judge_submission`): a materialization past the green's room is refused, typed, never committed over it.
      let reserved = state.greens.lend(&green).map_or(0, |engine| {
        engine
          .history_reservation(&built.increment)
          .saturating_add(engine.changes_reservation(&built.increment))
      });
      let secured = u64::try_from(reserved).unwrap_or(u64::MAX);
      if let Err(available) = crate::verbs::secure_retention(state, green, secured) {
        state.count(MATERIALIZE_OVER_ROOM, 1);
        refuse_parked(state, green, Refusal::BudgetExceeded { available });
        return true;
      }
      let made = state
        .greens
        .lend_mut(&green)
        .map(|mut engine| engine.materialize(&built.increment));
      if let Some(Ok(_)) = made {
        state.count(MATERIALIZED, 1);
        let _ = crate::merge_service::settle_green_retention(state, green);
        // Its tool's derived data follows its first read (P2): a git green's map of paths, in the whole build's slices.
        crate::vcs_store::paths::derive_once_made(state, green);
        let parked = state
          .materializations
          .parked
          .remove(&green)
          .unwrap_or_default();
        crate::verbs::resume_all(state, parked);
        state.materializations.kept.push_back(green);
        keep_within_bound(state);
      } else {
        slates_machine::log_line!(
          "slates-server: partition {}: a green's owed push did not commit: {made:?}",
          state.partition
        );
        refuse_parked(state, green, Refusal::ContentUnavailable);
      }
      true
    }
    Some(Err(refused)) => {
      slates_machine::log_line!(
        "slates-server: partition {}: a green's owed push was refused building: {refused:?}",
        state.partition
      );
      refuse_parked(state, green, Refusal::ContentUnavailable);
      true
    }
  }
}

/// Answers every request parked on `green` `refusal`, unrecorded, its materialization refused: the green still owes,
/// and a retry starts it again.
fn refuse_parked(state: &mut ShardState, green: DbVolumeId, refusal: Refusal) {
  state.count(MATERIALIZE_REFUSED, 1);
  let parked = state
    .materializations
    .parked
    .remove(&green)
    .unwrap_or_default();
  for submit in parked {
    crate::verbs::answer_refused(state, submit, refusal.clone());
  }
}

/// Drops the least recently read greens kept materialized back to owing their push while more are kept than the bound
/// (main's memory-sizing rule for P2: derived content is never kept unbounded). The bound is one green a client seat the
/// shard serves (its `clients_per_shard` across its region's slots), the same derivation that bounds its parked requests:
/// a client reads one green at a time. A green written to since it was made (a version past the owed one, a checkpoint,
/// a work over it) or that cannot be branched again from its origin is kept, and leaves the queue.
fn keep_within_bound(state: &mut ShardState) {
  let derived = state
    .config
    .clients_per_shard
    .saturating_mul(usize::try_from(state.config.region.slots).unwrap_or(1));
  #[cfg(test)]
  let derived = TEST_KEEP_BOUND
    .with(std::cell::Cell::get)
    .unwrap_or(derived);
  while state.materializations.kept.len() > derived {
    let Some(green) = state.materializations.kept.pop_front() else {
      return;
    };
    if dematerialize(state, green) {
      state.count(DEMATERIALIZED, 1);
    }
  }
}

/// `green` dropped back to owing the push its chain records, its content let go: where nobody wrote to it since it was
/// made (its chain is that push's record alone, with no checkpoint, no work over it, nothing parked or building on it)
/// and its origin can be branched again (`from` held, materialized, and reconstructing `version`). Whether it was.
fn dematerialize(state: &mut ShardState, green: DbVolumeId) -> bool {
  let partition = state.db.partition();
  let chain = partition.green_chain(green);
  let push = match chain {
    [record] => slates_merge::push::DeferredPush::decode(record),
    _ => None,
  };
  let Some(push) = push else {
    return false;
  };
  let Some(slates_db::catalog::MemberOrigin::Branch { from, version }) = partition
    .repositories()
    .member(green)
    .map(|member| member.origin)
  else {
    return false;
  };
  let untouched = partition.green_checkpoint(green).is_none()
    && !state.works.values().any(|work| work.green == green)
    && !state.materializations.building.contains_key(&green)
    && !state.materializations.parked.contains_key(&green)
    && state
      .greens
      .get(&green)
      .is_some_and(|engine| engine.head() == push.version());
  let branchable = state.greens.get(&from).is_some_and(|engine| {
    engine.folded_below().max(engine.mappable_from()) <= version && version <= engine.head()
  });
  if !untouched || !branchable {
    return false;
  }
  if let Some((repository, Ok(mut release))) = state.greens.retire(&green) {
    while state
      .greens
      .release_step(repository, &mut release)
      .is_ok_and(|more| more)
    {}
  } else {
    return false;
  }
  state.greens.branch(&from, version, green).is_ok() && state.greens.defer(&green, push).is_some()
}
