//! A snapshot presented through a host mount (§4.4 `attach(volume|snapshot, ...)`, §4.6; AUD-29-76): the
//! attachment's own read-only view of the snapshot's tree, served by the NFS edge for exactly that
//! attachment's mount capability.
//!
//! What it is. A host mount serves the volume its record names; a mount of the live head cannot present a
//! snapshot, and attaching it for one would show the head where the snapshot is asked for (refused until
//! 2026-10-01 with `SnapshotNotPresentedByHostMount`). A view is a copy-on-write clone of the snapshot
//! (`Volume::clone_of`: O(1), sharing every version with the snapshot, which it pins so the snapshot cannot be
//! destroyed under it). It lives beside the shard's volumes, not among them — a publish images only the
//! volumes, and a view holds nothing a restart must recover but its record — keyed by the attachment.
//!
//! What holds it to the snapshot. A view is read-only: only a read intent may attach a snapshot, the record's
//! granted rights carry no write, and the export refuses every mutation before any effect. Reads change
//! nothing (no access time is written), so a view never makes a version of its own, and releasing it frees
//! nothing but its own records (`Volume::discard_partial`). Its journal's records are reserved against the
//! shard's metadata ledger before it exists and returned with it (§4.2), the same charge a clone pays.
//!
//! Lifetimes. A view opens with its attachment (before the record commits, so a refusal changes nothing),
//! closes with it (`detach`, a revocation, an unmount — `verbs::end_attachment`) and before its volume's
//! destroy, which unpins the snapshot. A restart rebuilds the views of the recorded snapshot attachments after
//! the volumes and their pins are recovered. A volume over a host base (an overlay) presents base content the
//! snapshot does not hold, so its snapshots are refused typed rather than presented partly.
//!
//! Moving. `advance` re-pins a mount to another snapshot (§4.4 `Bound → Advancing → Bound`): the new view is
//! opened, the move recorded (`Op::AttachmentRepinned`, which moves the mount's container binds with it), the
//! views swapped and the old closed, and the reply names the paths that differ
//! (`Volume::paths_changed_between`). A guest device's view (`crate::virtiofs::GuestView`) is kept here too,
//! under the device's attachment record (AUD-29-68), so `advance` moves it the same way; recovery ends a guest's
//! record rather than rebuilding its view, since its device died with the process.

use slates_db::catalog::{AttachmentRecord, Consumer, VolumeId as DbVolumeId};
use slates_ipc::protocol::{AttachTransport, NamePolicy, Refusal, SizeClass, UnsupportedReason};
use slates_mem::budget::MetadataCredit;
use slates_vfs::clock::Clock;
use slates_vfs::ids::SnapshotId;
use slates_vfs::volume::Volume;

use crate::state::ShardState;

/// One attachment's view of a snapshot.
pub(crate) struct SnapshotView {
  /// The read-only clone of the snapshot's tree the edge serves.
  pub(crate) volume: Volume,
  /// The volume whose snapshot this is.
  origin: DbVolumeId,
  /// The snapshot, pinned on the origin while the view lives.
  snapshot: SnapshotId,
  /// The view's journal records, reserved against the metadata ledger.
  metadata: MetadataCredit,
}

impl std::fmt::Debug for SnapshotView {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("SnapshotView")
      .field("origin", &self.origin)
      .field("snapshot", &self.snapshot)
      .finish()
  }
}

/// Whether `record` is a host mount of a snapshot: a bridge consumer's attachment naming one (an SDK record
/// reads its version by `ReadAt`, and a container bind borrows its parent mount's view).
pub(crate) fn presents_a_snapshot(record: &AttachmentRecord) -> bool {
  record.snapshot.is_some()
    && matches!(record.consumer, Consumer::Bridge)
    && !matches!(record.form, slates_db::catalog::AttachForm::Oci { .. })
}

/// The refusal for a snapshot no host mount can present.
fn unpresentable() -> Refusal {
  Refusal::AttachmentUnsupported {
    transport: AttachTransport::NfsLoopback,
    reason: UnsupportedReason::SnapshotNotPresentedByHostMount,
  }
}

/// Opens a view of `snapshot` of volume `origin`: the snapshot pinned, the view's records charged. Refused, with
/// nothing changed, when the volume is not held here, is over a host base, or the snapshot is gone.
pub(crate) fn open(
  state: &mut ShardState,
  origin: DbVolumeId,
  snapshot: SnapshotId,
  names: NamePolicy,
) -> Result<SnapshotView, Refusal> {
  let handle = *state.by_id.get(&origin).ok_or(Refusal::NotFound)?;
  if state
    .volumes
    .get(handle)
    .map_err(|_| Refusal::NotFound)?
    .host
    .is_some()
  {
    return Err(unpresentable());
  }
  // A view never grows: its quota admits nothing, and every mutation is refused before it is admitted.
  let quota = crate::verbs::quota_for(SizeClass::Bounded { limit: 0 });
  let config = crate::verbs::volume_config(state, names, quota);
  let metadata = crate::verbs::reserve_metadata(state, config.journal_bytes)?;
  let cloned = match state.volumes.get_mut(handle) {
    Ok(slot) => Volume::clone_of(&state.store, &mut slot.volume, snapshot, config),
    Err(_) => Err(slates_vfs::VfsError::NotFound),
  };
  match cloned {
    Ok(volume) => Ok(SnapshotView {
      volume,
      origin,
      snapshot,
      metadata,
    }),
    Err(e) => {
      state.store.metadata.release(metadata);
      Err(crate::error::refusal_of_vfs(&e))
    }
  }
}

/// Closes `view`: its own records freed, its snapshot unpinned (when the origin is still held), its records'
/// credit returned. Each step is attempted whatever the others answered; a failure is counted, never silent.
pub(crate) fn close(state: &mut ShardState, view: SnapshotView) {
  let SnapshotView {
    volume,
    origin,
    snapshot,
    metadata,
  } = view;
  if volume.discard_partial(&mut state.store).is_err() {
    state.count(VIEW_RELEASE_REFUSED, 1);
  }
  if let Some(&handle) = state.by_id.get(&origin)
    && let Ok(slot) = state.volumes.get_mut(handle)
    && slot.volume.unpin(snapshot).is_err()
  {
    state.count(VIEW_RELEASE_REFUSED, 1);
  }
  state.store.metadata.release(metadata);
}

/// Format: the status refusal a view's release counts when a step of it was refused.
pub(crate) const VIEW_RELEASE_REFUSED: &str = "snapshot_view.release_refused";
/// Format: the status refusal recovery counts for a recorded snapshot mount whose view could not be rebuilt
/// (its attachment is ended instead, so its capability reaches nothing rather than the head).
pub(crate) const VIEW_REBUILD_REFUSED: &str = "snapshot_view.rebuild_refused";

/// Ends attachment `attachment`'s view, if it has one.
pub(crate) fn end(state: &mut ShardState, attachment: u64) {
  if let Some(view) = state.snapshot_views.remove(&attachment) {
    close(state, view);
  }
}

/// Ends every view of volume `origin`, a mount's or a guest device's (before its destroy, which frees the
/// snapshots they pin). A guest device whose view is gone is answered `NotFound` until its record's end revokes it.
pub(crate) fn end_all_of(state: &mut ShardState, origin: DbVolumeId) {
  let attachments: Vec<u64> = state
    .snapshot_views
    .iter()
    .filter(|(_, view)| view.origin == origin)
    .map(|(attachment, _)| *attachment)
    .collect();
  for attachment in attachments {
    end(state, attachment);
  }
}

/// Rebuilds the views of every recorded snapshot mount after a restart (the volumes and their pins recovered
/// first). A view that cannot be rebuilt ends its attachment as a recorded operation, so the capability reaches
/// nothing rather than the head; counted. The views rebuilt.
pub(crate) fn rebuild(state: &mut ShardState) -> usize {
  let partition = state.db.partition();
  let records: Vec<AttachmentRecord> = partition
    .volumes()
    .into_iter()
    .flat_map(|volume| partition.attachments_of(volume.id))
    .filter(|record| presents_a_snapshot(record))
    .cloned()
    .collect();
  let mut rebuilt: usize = 0;
  for record in records {
    let Some(snapshot) = record.snapshot else {
      continue;
    };
    let names = state
      .db
      .partition()
      .volume(record.volume)
      .map_or(NamePolicy::Exact, |volume| {
        crate::verbs::wire_names(volume.policy.names)
      });
    match open(
      state,
      record.volume,
      crate::verbs::core_snapshot(slates_ipc::protocol::SnapshotId {
        value: snapshot.value,
      }),
      names,
    ) {
      Ok(view) => {
        state.snapshot_views.insert(record.id, view);
        rebuilt = rebuilt.saturating_add(1);
      }
      Err(_) => {
        state.count(VIEW_REBUILD_REFUSED, 1);
        let _ = crate::verbs::end_attachment(state, &record, crate::verbs::Ending::Otherwise);
      }
    }
  }
  rebuilt
}

/// `advance(attachment, version?)` of a snapshot mount (§4.4 "immutable readers may `Bound → Advancing →
/// Bound`"; AUD-29-76): re-pins the mount, and every container bind borrowing it, to snapshot `version` (its
/// wire value) or, with none, to the volume's newest snapshot; names the paths the move invalidates
/// (`Volume::paths_changed_between`). The new view is opened first (pinning its snapshot), so a refusal
/// changes nothing; the move is recorded as one operation before the views swap, so a restart rebuilds the
/// new one; the old view is closed last, unpinning its snapshot. A request is served whole within one shard
/// turn, so none spans the swap: every request is answered from the old view or the new, never both.
pub(crate) fn advance(
  state: &mut ShardState,
  record: &AttachmentRecord,
  version: Option<u64>,
) -> slates_ipc::protocol::ReplyBody {
  match advanced(state, record, version) {
    Ok((version, invalidated)) => slates_ipc::protocol::ReplyBody::Advanced {
      version,
      invalidated,
    },
    Err(refusal) => crate::verbs::refused(refusal),
  }
}

/// [`advance`]'s work: the snapshot now presented (its wire value) and the paths invalidated.
fn advanced(
  state: &mut ShardState,
  record: &AttachmentRecord,
  version: Option<u64>,
) -> Result<(u64, Vec<String>), Refusal> {
  let current = record.snapshot.ok_or(Refusal::NotFound)?;
  let target = match version {
    Some(value) => slates_db::catalog::SnapshotId { value },
    None => newest_snapshot(state, record.volume).ok_or(Refusal::NotFound)?,
  };
  if target == current {
    return Ok((target.value, Vec::new()));
  }
  let wire = |id: slates_db::catalog::SnapshotId| {
    crate::verbs::core_snapshot(slates_ipc::protocol::SnapshotId { value: id.value })
  };
  let names = state
    .db
    .partition()
    .volume(record.volume)
    .map_or(NamePolicy::Exact, |volume| {
      crate::verbs::wire_names(volume.policy.names)
    });
  let view = open(state, record.volume, wire(target), names)?;
  let invalidated = match invalidated_between(state, record.volume, wire(current), wire(target)) {
    Ok(paths) => paths,
    Err(refusal) => {
      close(state, view);
      return Err(refusal);
    }
  };
  let now = state.clock.monotonic_ns();
  let op = slates_db::op::Op::AttachmentRepinned {
    id: record.id,
    snapshot: target,
  };
  if let Err(e) = state.db.mutate(&mut state.segment, &op, now) {
    close(state, view);
    return Err(crate::error::refusal_of_db(&e));
  }
  if let Some(old) = state.snapshot_views.insert(record.id, view) {
    close(state, old);
  }
  Ok((target.value, invalidated))
}

/// The newest snapshot of `volume` the catalog records (the highest sealed epoch).
fn newest_snapshot(
  state: &ShardState,
  volume: DbVolumeId,
) -> Option<slates_db::catalog::SnapshotId> {
  state
    .db
    .partition()
    .snapshots_of(volume)
    .into_iter()
    .max_by_key(|snapshot| snapshot.epoch)
    .map(|snapshot| snapshot.id)
}

/// The paths that differ between snapshots `from` and `to` of `volume`, as its owner holds them.
fn invalidated_between(
  state: &ShardState,
  volume: DbVolumeId,
  from: SnapshotId,
  to: SnapshotId,
) -> Result<Vec<String>, Refusal> {
  let handle = *state.by_id.get(&volume).ok_or(Refusal::NotFound)?;
  let slot = state.volumes.get(handle).map_err(|_| Refusal::NotFound)?;
  slot
    .volume
    .paths_changed_between(&state.store, from, to)
    .map_err(|e| crate::error::refusal_of_vfs(&e))
}
