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

use slates_db::catalog::{AttachmentRecord, Consumer, VolumeId as DbVolumeId};
use slates_ipc::protocol::{AttachTransport, NamePolicy, Refusal, SizeClass, UnsupportedReason};
use slates_mem::budget::MetadataCredit;
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
    *state.refusals.entry(VIEW_RELEASE_REFUSED).or_insert(0) += 1;
  }
  if let Some(&handle) = state.by_id.get(&origin)
    && let Ok(slot) = state.volumes.get_mut(handle)
    && slot.volume.unpin(snapshot).is_err()
  {
    *state.refusals.entry(VIEW_RELEASE_REFUSED).or_insert(0) += 1;
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

/// Ends every view of volume `origin` (before its destroy, which frees the snapshots they pin).
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
  let mut rebuilt = 0;
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
        rebuilt += 1;
      }
      Err(_) => {
        *state.refusals.entry(VIEW_REBUILD_REFUSED).or_insert(0) += 1;
        let _ = crate::verbs::end_attachment(state, &record);
      }
    }
  }
  rebuilt
}
