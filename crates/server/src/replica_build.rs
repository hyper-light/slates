//! A holder's replica of a green that owes its push, made when a later version needs it (A-813's limit; main 01a20182's
//! ruling of 2026-11-23 20:00, after w-chaos's `v3_takeover` frame).
//!
//! **Why.** A created ref's green owes its push (P2, A-812), and so does a holder's replica of it (A-813). A version after
//! the owed one is an increment the holder judges on its replica before it accepts the version's record (§4.16 "Apply
//! on holders"); a replica that owes cannot judge one, so the record was refused for good
//! (`merge_service::REPLICA_OWES`) and the version never placed at `f > 0`: a branch's second push was never held at
//! `f + 1`, and a takeover lost it.
//!
//! **What.** When such a record finds the replica owing ([`wanted`]), the holder makes it through the same door the
//! owner makes its green (`crate::green_ready`): the owed push's `Building` advanced a turn at a time over the
//! repository's objects, then `Green::materialize`, which is the eager create's green by construction, so its head
//! identity is the owner's. The objects are the repository's replica store on this node (`vcs_store::replica`), made
//! first from the names log's parts its acceptors hold. The record is refused meanwhile and shipped again by its owner,
//! and is judged once the replica is made. Lazily: a holder makes nothing for a branch that never moves again.
//!
//! **Bounds.** One build a green, driven by a task of the shard a turn at a time under half the shard's step quantum, as
//! a materialization is; the store it reads is charged and refused typed past the shard's room
//! (`vcs_store::replica::ReplicaRefused`), which ends the build counted ([`REFUSED`]); the record then waits as before.
//!
//! **Counted** (the non-vacuity counters of `v3_takeover` and the fleet's double pushes): builds begun ([`BEGUN`]),
//! their turns ([`SLICES`]), replicas made ([`MADE`]), and the records that waited on one ([`VERSIONS_WAITED`]).

use std::collections::BTreeMap;

use slates_db::catalog::{RepositoryId, RepositoryRecord};
use slates_db::register::ObjectId;

use crate::state::ShardState;

/// Counter: replica builds begun on a holder. Format: a counter name.
pub(crate) const BEGUN: &str = "merge.replica_build.begun";
/// Counter: turns of replica builds. Format: a counter name.
pub(crate) const SLICES: &str = "merge.replica_build.slices";
/// Counter: replicas made. Format: a counter name.
pub(crate) const MADE: &str = "merge.replica_build.made";
/// Counter: the longest a replica build took from its beginning to its replica made, nanoseconds (main 01a20182: this
/// number decides whether a build starts at placement rather than when a later version needs it). Format: a counter
/// name, a peak, not a sum.
pub(crate) const MADE_NS: &str = "merge.replica_build.made_ns";
/// Counter: merge records refused because their green's replica owed its push, each waiting on its build. Format: a
/// counter name.
pub(crate) const VERSIONS_WAITED: &str = "merge.replica_build.versions_waited";
/// Counter: replica builds that could not run (no repository held here, its store refused past the shard's room, or
/// the owed push's build refused). Format: a counter name.
pub(crate) const REFUSED: &str = "merge.replica_build.refused";

/// The shard's replica builds under way, by green (one field of `ShardState`).
#[derive(Default)]
pub(crate) struct ReplicaBuilds {
  building: BTreeMap<ObjectId, Under>,
}

/// One replica build under way: its repository, and the owed push's build once the repository's store is whole.
struct Under {
  repository: RepositoryId,
  building: Option<slates_merge::push::Building>,
  /// The turns done, rising while it runs: what a waiting record's answer reports
  /// (`slates_cluster::content::ContentMessage::ReplicaBuilding`).
  slices_done: u64,
  /// When it began, on the shard's clock: its time is counted once it is made ([`MADE_NS`]).
  began_ns: u64,
}

/// The turns `green`'s replica build under way has done, if one is under way.
pub(crate) fn slices_done(state: &ShardState, green: ObjectId) -> Option<u64> {
  state
    .replica_builds
    .building
    .get(&green)
    .map(|under| under.slices_done)
}

/// What a turn of a build left.
enum Turned {
  /// More to do: another turn after a yield.
  More,
  /// A read met a let-go window: brought back before the next turn.
  Waiting(slates_git::store::packed::Waited),
  /// Made, or refused: the build is over.
  Done,
}

/// A merge record of `green` found its replica owing (`merge_service::REPLICA_OWES`): its build begun where none runs,
/// counted [`VERSIONS_WAITED`].
pub(crate) fn wanted(state: &mut ShardState, green: ObjectId) {
  state.count(VERSIONS_WAITED, 1);
  if state.replica_builds.building.contains_key(&green) {
    return;
  }
  let Some(record) = repository_of(state, green) else {
    state.count(REFUSED, 1);
    return;
  };
  if !matches!(
    state.vcs_stores.store(record.id),
    Some(crate::vcs_store::RepositoryStore::Git(_))
  ) {
    let parts = crate::takeover::held_parts(state, record.id);
    let records = crate::vcs_store::replica::records_of(&parts);
    if let Err(refused) = crate::vcs_store::replica::begin(state, &record, records) {
      slates_machine::log_line!(
        "slates-server: partition {}: a replica's repository store was refused: {refused:?}",
        state.partition
      );
      state.count(REFUSED, 1);
      return;
    }
  }
  state.replica_builds.building.insert(
    green,
    Under {
      repository: record.id,
      building: None,
      slices_done: 0,
      began_ns: slates_rt::futures::now_ns(),
    },
  );
  match slates_rt::futures::spawn(drive(green)).and_then(slates_rt::futures::detach) {
    Ok(()) => state.count(BEGUN, 1),
    Err(_) => {
      // No task to run it: the next record that finds the replica owing begins it again.
      state.replica_builds.building.remove(&green);
      state.count(REFUSED, 1);
    }
  }
}

/// The repository whose greens' family `green` is of, by the record its own register holds on this node: the register
/// of the same family whose value is a repository's record (A-636 part 2c-1).
fn repository_of(state: &ShardState, green: ObjectId) -> Option<RepositoryRecord> {
  let family = green.line_key();
  state
    .holder_records
    .iter()
    .filter(|(register, _)| register.line_key() == family && **register != green)
    .find_map(|(register, acceptor)| {
      acceptor
        .values(*register)
        .filter_map(crate::catalog::repository_of)
        .last()
    })
}

/// `green`'s build driven a turn at a time, a yield between, until it is made or refused.
async fn drive(green: ObjectId) {
  loop {
    match crate::state::with_state_counted(|state| turn(state, green)) {
      Some(Turned::More) => slates_rt::futures::yield_now().await,
      Some(Turned::Waiting(waited)) => {
        if crate::vcs_store::upload::bring_back_within(waited)
          .await
          .is_err()
        {
          slates_rt::futures::yield_now().await;
        }
      }
      Some(Turned::Done) | None => break,
    }
  }
  let _ = crate::state::with_state_counted(|state| state.replica_builds.building.remove(&green));
}

/// One turn of `green`'s build: the repository's store waited for while its replay runs; then the owed push's build
/// begun and advanced; then the replica made with what it built.
fn turn(state: &mut ShardState, green: ObjectId) -> Turned {
  let Some((repository, taken)) = state.replica_builds.building.get_mut(&green).map(|under| {
    under.slices_done = under.slices_done.saturating_add(1);
    (under.repository, under.building.take())
  }) else {
    return Turned::Done;
  };
  state.count(SLICES, 1);
  // Its repository's store is made first (`vcs_store::replica`): nothing of the owed push's build is begun until then.
  if state.vcs_stores.is_rebuilding(repository) {
    return Turned::More;
  }
  let Some(replica) = state
    .merge
    .replicas
    .lend(&crate::merge_service::replica_key(green))
  else {
    return Turned::Done;
  };
  if replica.is_materialized() {
    return Turned::Done;
  }
  let mut building = match taken {
    Some(building) => building,
    None => {
      let pace = slates_merge::pace::Pace::new(slates_rt::futures::now_ns);
      match replica.materializing(pace, state.store.admittable()) {
        Some(building) => building,
        None => return Turned::Done,
      }
    }
  };
  drop(replica);
  let quantum = state.config.step_quantum_ns() / 2;
  building
    .pace()
    .turn(slates_rt::futures::now_ns().saturating_add(quantum.max(1)));
  let advanced = crate::vcs_store::with_room(state, |state| {
    let replica = state
      .merge
      .replicas
      .lend(&crate::merge_service::replica_key(green))?;
    let mut turned = None;
    crate::vcs_store::with_owed_push_objects(state, Some(repository), &mut |objects| {
      turned = Some(building.advance(&replica, objects));
    });
    turned
  });
  match advanced {
    // Not found: no replica, or no store to read.
    None => {
      state.count(REFUSED, 1);
      Turned::Done
    }
    Some(None) => {
      let waited = building
        .waiting()
        .and(state.vcs_stores.git(repository))
        .and_then(|(_, store)| store.repository.waiting());
      if let Some(under) = state.replica_builds.building.get_mut(&green) {
        under.building = Some(building);
      }
      waited.map_or(Turned::More, Turned::Waiting)
    }
    Some(Some(Ok(built))) => {
      let made = state
        .merge
        .replicas
        .lend_mut(&crate::merge_service::replica_key(green))
        .map(|mut replica| replica.materialize(&built.increment));
      match made {
        Some(Ok(_)) => {
          state.count(MADE, 1);
          let began = state
            .replica_builds
            .building
            .get(&green)
            .map_or(0, |under| under.began_ns);
          state.count_max(MADE_NS, slates_rt::futures::now_ns().saturating_sub(began));
        }
        other => {
          slates_machine::log_line!(
            "slates-server: partition {}: a holder's replica's owed push did not commit: {other:?}",
            state.partition
          );
          state.count(REFUSED, 1);
        }
      }
      Turned::Done
    }
    Some(Some(Err(refused))) => {
      slates_machine::log_line!(
        "slates-server: partition {}: a holder's replica's owed push was refused building: {refused:?}",
        state.partition
      );
      state.count(REFUSED, 1);
      Turned::Done
    }
  }
}
