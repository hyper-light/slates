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
//! store a repository, replayed by the rebuild's slices. A store whose replay was refused is begun again only once its
//! records or a window brought back on the shard change; until then each begin is refused [`ReplicaRefused::Unchanged`]
//! at once, counted [`UNCHANGED_REFUSED`] (a holder's store was begun again about 8,000 times in one fleet test, each
//! replay refused the same way; GAPS, (d)).
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
/// Counter: a replica store dropped to be made again, made from fewer push records than this node's names-log parts now
/// carry (A-921 part 4, [`drop_if_stale`]). Format: a counter name in the daemon's status report.
pub(crate) const MADE_AGAIN: &str = "vcs_store.replica.made_again";

/// Counter: a replica store's begin refused at once, its last replay refused over the same records with no window
/// brought back on the shard since ([`ReplicaRefused::Unchanged`]). Format: a counter name in the daemon's status
/// report.
pub(crate) const UNCHANGED_REFUSED: &str = "vcs_store.replica.unchanged_refused";

/// Why a replica store was not begun.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplicaRefused {
  /// Its records are `asked` bytes, past the `room` the shard could admit now.
  PastRoom { asked: u64, room: u64 },
  /// Its last replay was refused over these same records, and no window has been brought back on the shard since: the
  /// same replay would meet the same refusal, which the store keeps (`VcsStores::unrecovered`). Begun again only once
  /// the records or a brought-back window change (CLAUDE.md §2 item 8).
  Unchanged,
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
  // A store whose replay was refused keeps its refusal and the records it was made from; the same records replayed
  // with nothing brought back meet the same refusal, so it is refused at once, counted (main 01a20182's ruling of
  // 2026-11-27 05:52: a holder's store was begun again about 8,000 times in one fleet test; CLAUDE.md §2 item 8).
  let brought_back = brought_back_here(state);
  if state.vcs_stores.unrecovered(record.id).is_some()
    && state.vcs_replica_records.get(&record.id) == Some(&records)
    && state.vcs_stores.replica_begun.get(&record.id) == Some(&brought_back)
  {
    state.count(UNCHANGED_REFUSED, 1);
    return Err(ReplicaRefused::Unchanged);
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
  state
    .vcs_stores
    .replica_begun
    .insert(record.id, brought_back);
  super::settle_objects(state, record.id);
  super::rebuild_sliced_from(state, record);
  state.count(BEGUN, 1);
  Ok(true)
}

/// The windows brought back on this shard so far, by a read through the store (`read_through::BROUGHT_BACK`) and by a
/// home's record of a landed pack (`homes::LANDED_BROUGHT_BACK`): what a refused replica's store is begun again after
/// ([`begin`]), since a window brought back may be what its replay lacked.
fn brought_back_here(state: &ShardState) -> u64 {
  [
    super::read_through::BROUGHT_BACK,
    super::homes::LANDED_BROUGHT_BACK,
  ]
  .iter()
  .map(|name| state.refusals.get(name).copied().unwrap_or(0))
  .fold(0, u64::saturating_add)
}

/// Drops repository `id`'s replica store where it was made from other push records than the names-log parts this node
/// holds now carry (`records`, [`records_of`]), so the next [`begin`] makes it again from all of them (A-921 part 4):
/// whether it was dropped. Made once, a store never took the objects of a part held after it, so a holder that had
/// missed a part (its owner holding it off, then owing it again after a warm restart, A-921 part 3) built its replica's
/// owed push over a store without that push's objects, refused `Missing` for good. A store still being rebuilt stands,
/// and is judged again at the next build; the owner's own store keeps no replica records and is never dropped.
pub(crate) fn drop_if_stale(state: &mut ShardState, id: RepositoryId, records: &[Vec<u8>]) -> bool {
  let Some(made_from) = state.vcs_replica_records.get(&id) else {
    return false;
  };
  if made_from.as_slice() == records || state.vcs_stores.rebuilding.contains(&id) {
    return false;
  }
  super::free(state, id);
  state.vcs_replica_records.remove(&id);
  state.count(MADE_AGAIN, 1);
  true
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
    let mismatches =
      crate::git_push::tests::on_stored_shard_awaited(["replica"], |[repository]| async move {
        use crate::git_push::tests::on_state;
        let mut parts: Vec<StateLogRecord> = on_state(|state| state.db.take_state_log_records());
        let mut mismatches = Vec::new();
        for (scenario, _) in crate::git_push::tests::chain() {
          let _ = crate::git_push::tests::pushed_awaited(repository, scenario.request).await;
          on_state(|state| {
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
          });
        }
        mismatches
      });
    assert_eq!(mismatches, Vec::<String>::new());
  }

  /// Lets go the windows of every pack repository `id`'s store holds, as a landed pack's let-go does: each pack's home
  /// here told first (`homes::landed_pack_here`, which lets its copy go), each window then marked landed and let go.
  fn let_go_packs_of(state: &mut ShardState, id: RepositoryId) {
    let packs: Vec<slates_vcs::ContentId> = state
      .vcs_stores
      .git_ref(id)
      .map(|(_, git)| git.repository.packs().map(|(pack, _)| pack).collect())
      .unwrap_or_default();
    let windows: Vec<slates_vcs::ContentId> = state
      .vcs_stores
      .content()
      .map(|content| {
        packs
          .iter()
          .flat_map(|pack| {
            content
              .windows(pack)
              .map(|window| slates_vcs::ContentId::of(&window))
          })
          .collect()
      })
      .unwrap_or_default();
    for pack in &packs {
      crate::vcs_store::homes::landed_pack_here(state, *pack);
    }
    crate::vcs_store::within_budget(state, 0, |stores| {
      if let Some(content) = stores.content_mut() {
        for window in &windows {
          content.mark_landed(window);
        }
      }
    });
    for window in &windows {
      let _ = crate::vcs_store::let_go_read(state, window);
    }
    assert!(!packs.is_empty(), "a pack to let go");
  }

  /// Repository `id`'s record, as the partition holds it.
  fn record_of(state: &ShardState, id: RepositoryId) -> slates_db::catalog::RepositoryRecord {
    state
      .db
      .partition()
      .repositories()
      .repository(id)
      .cloned()
      .expect("the repository")
  }

  /// Main 01a20182's ruling of 2026-11-27 06:00 ((d)'s first standing defect; CLAUDE.md §2 item 9: no error dropped
  /// unseen). Do: push the fixtures' first push to repository `a`; let go its pack's windows as a landed let-go does
  /// ([`let_go_packs_of`]); begin a replica store of repository `b` from `a`'s records, whose changesets this node's
  /// store holds none of under `b`, as a holder's store holds none of its owner's ((d) in GAPS); await the rebuild.
  /// Expect: the changeset read's refusal counted (`vcs_store::CHANGESET_UNREAD`), left to the region's homes, which do
  /// not fetch a pack the content store holds let go; and the replay, meeting that pack with nothing to fill it,
  /// refused `Unlanded`. Red before: the read's refusal was dropped unseen, and the replay refused
  /// `Pack(Truncated { at: 0 })`, a pack cut short, which it is not. (Ending the rebuild at the read instead refused
  /// every holder's replica in the fleet tests, whose packs the region's homes give.)
  #[test]
  fn a_replicas_replay_over_a_let_go_pack_nothing_fills_is_refused_unlanded() {
    let (unread, unrecovered) =
      crate::git_push::tests::on_stored_shard_awaited(["a", "b"], |[a, b]| async move {
        use crate::git_push::tests::on_state;
        let mut parts: Vec<StateLogRecord> = on_state(|state| state.db.take_state_log_records());
        let [(scenario, _), ..] = crate::git_push::tests::chain();
        let _ = crate::git_push::tests::pushed_awaited(a, scenario.request).await;
        let begun = on_state(|state| {
          parts.extend(state.db.take_state_log_records());
          let_go_packs_of(state, a);
          let record = record_of(state, b);
          crate::vcs_store::free(state, b);
          begin(state, &record, records_of(&parts))
        });
        assert_eq!(begun, Ok(true));
        crate::vcs_store::rebuilt(b).await;
        on_state(|state| {
          let unread = state
            .refusals
            .get(crate::vcs_store::CHANGESET_UNREAD)
            .copied()
            .unwrap_or(0);
          (unread, state.vcs_stores.unrecovered(b).cloned())
        })
      });
    assert!(unread > 0, "the changeset read's refusal counted");
    assert_eq!(unrecovered, Some(crate::vcs_store::ReplayRefused::Unlanded));
  }

  /// Main 01a20182's ruling of 2026-11-27 05:52 and 06:00 ((d)'s second standing defect; CLAUDE.md §2 item 8: no
  /// unbounded retry; a holder's replica store was begun again about 8,000 times in one fleet test, each replay refused
  /// the same way). Do: begin a replica store of a repository from records that are not push records, so its replay is
  /// refused; begin it again from the same records, nothing brought back between; then from other records. Expect: one
  /// replay refused; the second begin refused `Unchanged` at once and counted (`UNCHANGED_REFUSED`), no replay made,
  /// the store left unrecovered with the first refusal; the third begun. Red before: the second begin replayed the same
  /// records again, refused again.
  #[test]
  fn a_replica_store_refused_is_not_begun_again_from_the_same_records() {
    let (refused, again, unrecovered, counts, other) =
      crate::git_push::tests::on_stored_shard_awaited(["bound"], |[repository]| async move {
        crate::git_push::tests::on_state(|state| {
          let record = record_of(state, repository);
          let records = vec![b"not a push record".to_vec()];
          crate::vcs_store::free(state, repository);
          let first = begin(state, &record, records.clone());
          settle_rebuild(state, repository);
          let refused = (first, state.vcs_stores.unrecovered(repository).is_some());
          let again = begin(state, &record, records);
          settle_rebuild(state, repository);
          let unrecovered = state.vcs_stores.unrecovered(repository).is_some();
          let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
          let counts = (
            count(crate::vcs_store::REPLAY_REFUSED),
            count(super::UNCHANGED_REFUSED),
          );
          let other = begin(state, &record, Vec::new());
          settle_rebuild(state, repository);
          (refused, again, unrecovered, counts, other)
        })
      });
    assert_eq!(refused, (Ok(true), true), "the first begun and refused");
    assert_eq!(again, Err(super::ReplicaRefused::Unchanged));
    assert!(unrecovered, "left unrecovered with the first refusal");
    assert_eq!(
      counts,
      (1, 1),
      "one replay refused, one begin refused unchanged"
    );
    assert_eq!(other, Ok(true), "other records begun");
  }
}
