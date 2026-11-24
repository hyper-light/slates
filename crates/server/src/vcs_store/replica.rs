//! A holder's replica of a repository's store (A-813's limit; main 01a20182's ruling of 2026-11-23 20:00, after w-chaos's
//! `v3_takeover` frame): what a node that holds a repository's greens, and does not own the repository, makes its
//! replicas of them over.
//!
//! **Why.** A created ref's green owes its push (P2, A-812): its version 1 is a [`slates_merge::push::DeferredPush`],
//! and a holder's replica of the green owes the same push (A-813). A later version is an increment a holder must judge on
//! its replica before it accepts the version's record (§4.16 "Apply on holders"), and a replica that owes cannot judge
//! one (`merge_service::REPLICA_OWES`). So no version after an owed one placed at `f > 0`: at `f = 1` a branch's
//! second push was never held at `f + 1`, and a takeover lost it (`v3_takeover`, 7 of 7). The replica is made through the
//! same door the owner's green is (`crate::green_ready`), which builds the owed push's tree over the repository's
//! objects; a holder has none until it holds this store.
//!
//! **What.** The store is the owner's, made by the same steps: the repository's push records, as the names log's parts
//! this node's acceptors hold carry them ([`records_of`]), replayed a record a turn by the sliced rebuild
//! (`vcs_store::rebuild_sliced_from`), each record's names and pack read from this node's changeset store and the
//! region's homes (A-767 C5, record v3), as a takeover's rebuild reads them. Nothing is fetched from the owner that this
//! node holds, and content is held once on this node. Every replay reads its records through `vcs_store::object_record`,
//! the partition's or these, so the two sources make one store (the equality oracle, this module's tests).
//!
//! **Bounds.** The records are charged to the shard as the owner's are (`vcs_store::settle_objects`), and a store whose
//! records pass what the shard could admit is refused [`ReplicaRefused::PastRoom`], typed, before anything is kept. One
//! store a repository, replayed by the rebuild's slices.
//!
//! **A takeover adopts it** (`takeover::rebuild_repository`): where the partition's records after the takeover's are the
//! records it was made from, the store stands and is no longer a replica; otherwise it is made again from the partition.

use slates_db::catalog::{LoggedChange, RepositoryId, RepositoryRecord, StateLog, StateLogRecord};

use crate::state::ShardState;

/// Counter: replica stores begun on a holder ([`begin`]). Format: a counter name.
pub(crate) const BEGUN: &str = "vcs_store.replica.begun";
/// Counter: replica stores refused past the shard's room ([`ReplicaRefused::PastRoom`]). Format: a counter name.
pub(crate) const REFUSED: &str = "vcs_store.replica.refused";
/// Counter: replica stores a takeover adopted as they stood ([`adopt`]). Format: a counter name.
pub(crate) const ADOPTED: &str = "vcs_store.replica.adopted";

/// Why a replica store was not begun.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplicaRefused {
  /// Its records are `asked` bytes, past the `room` the shard could admit now.
  PastRoom { asked: u64, room: u64 },
}

/// The push records `parts` carry, in the names log's order: each `LoggedChange::Objects` record of each names-log part,
/// the parts taken by the position each leaves the log at, as a takeover's rebuild records them.
pub(crate) fn records_of(parts: &[StateLogRecord]) -> Vec<Vec<u8>> {
  let mut names: Vec<&StateLogRecord> = parts
    .iter()
    .filter(|part| part.log == StateLog::Names)
    .collect();
  names.sort_by_key(|part| part.to);
  names
    .into_iter()
    .flat_map(|part| part.changes.iter())
    .filter_map(|change| match change {
      LoggedChange::Objects { record } => Some(record.clone()),
      _ => None,
    })
    .collect()
}

/// Begins repository `record`'s replica store from `records` ([`records_of`]), replayed a record a turn: whether one was
/// begun (none where this shard holds a store of it already, the owner's or a replica). Refused typed past the shard's
/// room, nothing kept.
pub(crate) fn begin(
  state: &mut ShardState,
  record: &RepositoryRecord,
  records: Vec<Vec<u8>>,
) -> Result<bool, ReplicaRefused> {
  if state.vcs_stores.store(record.id).is_some() {
    return Ok(false);
  }
  let asked = records
    .iter()
    .map(Vec::len)
    .fold(0usize, usize::saturating_add);
  let asked = u64::try_from(asked).unwrap_or(u64::MAX);
  let room = state.store.admittable();
  if asked > room {
    state.count(REFUSED, 1);
    return Err(ReplicaRefused::PastRoom { asked, room });
  }
  super::free(state, record.id);
  state.vcs_replica_records.insert(record.id, records);
  super::settle_objects(state, record.id);
  super::rebuild_sliced_from(state, record);
  state.count(BEGUN, 1);
  Ok(true)
}

/// A takeover's adoption of repository `id`'s replica store, its record now the partition's: whether it stands as made
/// (the partition's records are the ones it was made from: no longer a replica, its greens' maps made as a rebuild's end
/// makes them); otherwise it is dropped, and the caller makes the store again from the partition.
pub(crate) fn adopt(state: &mut ShardState, id: RepositoryId) -> bool {
  let Some(records) = state.vcs_replica_records.get(&id) else {
    return false;
  };
  let same = state
    .db
    .partition()
    .repositories()
    .objects(id)
    .eq(records.iter().map(Vec::as_slice));
  if !same || state.vcs_stores.rebuilding.contains(&id) {
    super::free(state, id);
    return false;
  }
  state.vcs_replica_records.remove(&id);
  super::rebuild_ended(state, id);
  super::settle_objects(state, id);
  state.count(ADOPTED, 1);
  true
}

#[cfg(test)]
mod tests {
  use slates_db::catalog::{RepositoryId, StateLog, StateLogRecord};
  use slates_git::object_name::ObjectId;
  use slates_git::store::rows::ObjectRow;

  use super::{adopt, begin, records_of};
  use crate::state::ShardState;
  use crate::vcs_store::{green_key, settle_rebuild};

  /// What a repository's git store holds, compared whole by the oracle: each member green's store versions, the head
  /// and tree and the head's row (its position among the store's objects) at each, the store's map entries and charge,
  /// and the names log's position.
  type Digest = (
    Vec<(
      u64,
      Vec<u64>,
      Vec<(Option<ObjectId>, Option<ObjectId>, Option<ObjectRow>)>,
    )>,
    (u64, u64),
    Option<u64>,
  );

  fn digest(state: &ShardState, id: RepositoryId) -> Digest {
    let (content, git) = state.vcs_stores.git_ref(id).expect("a git store");
    let members: Vec<u64> = state
      .db
      .partition()
      .repositories()
      .members_of(id)
      .map(|member| green_key(member.green))
      .collect();
    let greens = members
      .into_iter()
      .map(|key| {
        let heads = (0..git.repository.versions(key))
          .map(|at| {
            let head = git.repository.head(content, key, at);
            let tree = git.repository.tree(content, key, at);
            let row = head.and_then(|head| git.repository.object(content, &head));
            (head, tree, row)
          })
          .collect();
        (key, git.versions(key).to_vec(), heads)
      })
      .collect();
    let held = git.repository.held();
    let position = state
      .db
      .partition()
      .repositories()
      .log_position(id, StateLog::Names);
    (greens, (held.map_entries, held.charged), position)
  }

  /// The replica store's equality oracle (main 01a20182's ruling of 2026-11-23 20:03: a second source of a store's
  /// records must be held to the first, or it drifts unseen). Do: make a git repository and push the fixtures' six
  /// pushes to it in order; after each, take the names-log parts the partition hands on, free the repository's store and
  /// make it again as a replica from the parts held so far ([`records_of`], [`begin`]), the replay run to its end; then
  /// let a takeover's adoption take it ([`adopt`]). Expect: at every cut the replica's store, and the store adopted from
  /// it, equal to the owner's store made from the partition, compared whole (each green's versions, heads, trees and
  /// their rows, the store's entries and charge, the names log's position); the replica's records the partition's; and
  /// every adoption keeping the store as made.
  #[test]
  fn a_replica_store_from_the_names_log_equals_the_owners_at_every_push_and_after_adoption() {
    let mismatches = crate::test_store::audit_on_stored_shard("replica", |state| {
      let repository = crate::git_push::tests::git_repository(state, "replica");
      let mut parts: Vec<StateLogRecord> = state.db.take_state_log_records();
      let mut mismatches = Vec::new();
      for (scenario, _) in crate::git_push::tests::chain() {
        let _ = crate::git_push::tests::pushed(state, repository, scenario.request);
        parts.extend(state.db.take_state_log_records());
        let owner = digest(state, repository);
        let records = records_of(&parts);
        let partition: Vec<Vec<u8>> = state
          .db
          .partition()
          .repositories()
          .objects(repository)
          .map(<[u8]>::to_vec)
          .collect();
        if records != partition {
          mismatches.push(format!("{}: the parts' records", scenario.name));
        }
        let record = state
          .db
          .partition()
          .repositories()
          .repository(repository)
          .cloned()
          .expect("the repository");
        crate::vcs_store::free(state, repository);
        assert_eq!(
          begin(state, &record, records),
          Ok(true),
          "{}",
          scenario.name
        );
        settle_rebuild(state, repository);
        if digest(state, repository) != owner {
          mismatches.push(format!("{}: the replica", scenario.name));
        }
        if !adopt(state, repository) {
          mismatches.push(format!("{}: not adopted", scenario.name));
        }
        if digest(state, repository) != owner {
          mismatches.push(format!("{}: the adopted store", scenario.name));
        }
      }
      mismatches
    });
    assert_eq!(mismatches, Vec::<String>::new());
  }
}
