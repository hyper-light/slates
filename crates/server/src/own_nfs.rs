//! The macOS NFS mounts this daemon serves, recorded as its own (§4.6, A-227): no daemon thread opens a path on a
//! volume the daemon serves, and on macOS the mount table's source (`slates:/<volume>`) names no daemon, so the daemon
//! keeps the record itself.
//!
//! A mount is recorded when the CLI binds it (`BindMount`, after `mount(2)` returned) and when a successor learns
//! the mounts its predecessor's catalog still names as its shard starts, before any base or landing is served
//! (`learn`, from `daemon::init_shard`, under the process's `slates_base::own_mounts::Learning`). Either way the
//! kernel's table (`getfsstat(MNT_NOWAIT)`, no RPC, nothing opened) names the mount at exactly the bound point (the CLI
//! binds the path the kernel records, `std::fs::canonicalize`) whose source is the volume's, and its filesystem id is
//! what the base plane and the landing walk compare (`slates_base::own_mounts`). A record ends with its attachment
//! (`verbs::end_attachment`); a mount the table no longer names was unmounted while no daemon ran, and nothing is
//! recorded for it.

use crate::state::ShardState;
use slates_base::own_mounts::Unrecorded;
use slates_db::catalog::{AttachmentRecord, Consumer};
use slates_ipc::protocol::Refusal;

/// The refusal of a mount at `mount_point` this daemon could not record.
fn refusal(why: Unrecorded, mount_point: &str) -> Refusal {
  Refusal::TargetUnavailable {
    reason: format!("the NFS mount at {mount_point} {why}"),
  }
}

/// Format: the source a macOS slates mount names in the kernel's table, `slates:/<volume name>`
/// (`crates/cli/src/mount.rs`, `mount_volume`).
fn source_of(volume_name: &str) -> String {
  format!("slates:/{volume_name}")
}

/// One NFS mount of this daemon's, recorded as its own while this value lives. Its drop forgets the mount, so each
/// mount has exactly one: a second for a mount already held is never made ([`held`]).
#[derive(Debug)]
pub(crate) struct OwnNfsMount(u64);

impl OwnNfsMount {
  /// Records mount `id` and holds it.
  fn record(id: u64) -> Result<Self, Unrecorded> {
    slates_base::own_mounts::record(id).map_err(Unrecorded::Full)?;
    Ok(Self(id))
  }
}

/// The id of the mount of the volume named `volume_name` at exactly `mount_point` in the kernel's table, or `None`.
fn id_at(mount_point: &str, volume_name: &str) -> Option<u64> {
  let source = source_of(volume_name);
  slates_base::own_mounts::mount_id_at(mount_point, |named| named == source)
}

/// Whether this shard already holds mount `id`.
fn held(state: &ShardState, id: u64) -> bool {
  state.own_nfs_mounts.values().any(|own| own.0 == id)
}

impl Drop for OwnNfsMount {
  fn drop(&mut self) {
    slates_base::own_mounts::forget(self.0);
  }
}

/// The host mount `record` names and the volume's name, when it is one.
fn mount_of<'a>(state: &'a ShardState, record: &'a AttachmentRecord) -> Option<(&'a str, &'a str)> {
  if !matches!(record.consumer, Consumer::Bridge) {
    return None;
  }
  let point = record.form.mount_point()?;
  let volume = state.db.partition().volume(record.volume)?;
  Some((point, volume.name.as_str()))
}

/// Records the mount the CLI just bound for `record` at `mount_point`, before the bind is committed: refused when the
/// kernel's table names no mount of the volume there (the path the kernel records, which the CLI binds), or at the
/// bound of mounts this daemon records. `None` when this shard holds the mount already (a bind made again), whose
/// holder stays.
pub(crate) fn bound(
  state: &ShardState,
  record: &AttachmentRecord,
  mount_point: &str,
) -> Result<Option<OwnNfsMount>, Refusal> {
  let volume = state
    .db
    .partition()
    .volume(record.volume)
    .ok_or(Refusal::NotFound)?;
  let id =
    id_at(mount_point, &volume.name).ok_or_else(|| refusal(Unrecorded::NotMounted, mount_point))?;
  if held(state, id) {
    return Ok(None);
  }
  OwnNfsMount::record(id)
    .map(Some)
    .map_err(|why| refusal(why, mount_point))
}

/// Learns the mounts this shard's catalog names that the kernel's table still holds, a predecessor's that outlived
/// it (A-227): each recorded before the shard serves any base or landing. A record the table does not hold is a mount
/// that went while no daemon ran, and nothing is recorded for it. Refused at the set's bound, which is the most
/// attachments the shards hold, so a start that meets it is refused rather than serve a mount it does not record.
pub(crate) fn learn(state: &mut ShardState) -> Result<(), Refusal> {
  let mut learned = Vec::new();
  for volume in state.db.partition().volumes() {
    for record in state.db.partition().attachments_of(volume.id) {
      let Some((point, name)) = mount_of(state, record) else {
        continue;
      };
      // Two records naming one point find one mount, which has one holder.
      let Some(id) = id_at(point, name) else {
        continue;
      };
      if held(state, id)
        || learned
          .iter()
          .any(|(_, own): &(u64, OwnNfsMount)| own.0 == id)
      {
        continue;
      }
      learned.push((
        record.id,
        OwnNfsMount::record(id).map_err(|why| refusal(why, point))?,
      ));
    }
  }
  state.own_nfs_mounts.extend(learned);
  Ok(())
}
