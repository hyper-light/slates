//! The daemon's virtio-fs guest transport (§4.6 A-9, D-2, RQ-20: "Linux guests use an owned
//! FUSE-over-virtio device on the custom runtime, with an in-process or inherited-descriptor
//! integration seam for the host VMM"). A guest device is attached to a provisioned volume and
//! served on the volume's **owning shard**: the attach routes to the owner by the volume's id
//! (`verbs::owner_of`, D-14: ids route to owners) with the same cross-shard spawn the NFS transport
//! uses to reach a volume on another shard (`Control::Spawn`, §4.3) — with one difference that is
//! the point: the device *lives* on the owner for its whole life, so no request ever hops between
//! shards. The device task is `Send` (the seam is; the volume is not captured but reached through
//! the shard's state on every pass), so one spawn boots and serves it: it admits the device
//! (`slates_bridge_virtiofs::admission::admit`: the consumer authenticated first, its rights read from
//! the volume's access list only then, §4.13; the credits derived at boot, `DaemonConfig::guest_credits`)
//! and runs the perpetual loop (`serve_loop`) over a transient `VolumeBridge` built per pass over
//! the shard's store and the volume's slot — the daemon's serve shape (`nfs.rs`, `bridge-fskit`'s
//! `MountSession`) — with a handle slab that lives with the device so the guest's open handles
//! survive across passes.
//!
//! The harness owns the VM (§4.6: "The harness owns VM and container creation; Slates owns the
//! exported attachment and filesystem service"), so it hands the seam in by value and learns the
//! outcome — admission refused, the loop's end and what was reclaimed — through the `on_end`
//! callback it supplied; nothing about the guest is stored in the daemon's records in this leg (the
//! durable `AttachmentRecord` for a guest form is the next leg). `attach`/`status` carry
//! [`guest_transport_capabilities`] over the wire through `crate::transports`, fact for fact, and a
//! guest form asked for over the ring is refused typed there (no seam rides the ring). A guest device
//! is never a privilege: no mount, no socket on disk, no directory (R10).

use std::future::Future;

use slates_bridge_core::{Bridge, Rights, VolumeBridge, new_handle_store};
use slates_bridge_virtiofs::admission::{
  AdmissionError, GuestAttachRequest, GuestTransport, VmmSeam, admit,
};
use slates_bridge_virtiofs::capability::{TransportCapability, host_capability};
use slates_bridge_virtiofs::device::{DeviceConfig, FsTag};
use slates_bridge_virtiofs::serve::{BridgeAccess, LoopError, ServeEnd, register, serve_loop};
use slates_db::catalog::{Principal, VolumeId as DbVolumeId};
use slates_ipc::protocol::VolumeId;
use slates_mem::Slab;
use slates_rt::control::Control;
use slates_rt::error::RtError;
use slates_rt::registry;
use slates_rt::task::SpawnRequest;
use slates_vfs::error::VfsError;

use crate::daemon::Daemon;
use crate::error::ServerError;
use crate::state::{self, ShardState};
use crate::verbs::{owner_of, rights_of};

/// What became of a guest device the harness attached, delivered to its `on_end` callback.
#[derive(Debug)]
pub enum GuestDeviceOutcome {
  /// The volume is not held by its owner shard (destroyed, or never provisioned here).
  VolumeUnknown,
  /// Admission refused (typed); the seam was released.
  Refused(AdmissionError),
  /// The shard already serves its bound of devices; the device was reclaimed.
  LoopRefused(LoopError),
  /// The loop ended: why, and what the terminal step reclaimed.
  Ended(ServeEnd),
}

/// The harness's callback for the outcome.
pub type OnEnd = Box<dyn FnOnce(GuestDeviceOutcome) + Send>;

/// What this daemon offers each guest transport (§4.6: "must be reported by `attach` and
/// `status`"): the in-process seam served, the inherited-descriptor binding not built, DAX not
/// advertised.
pub fn guest_transport_capabilities() -> Vec<TransportCapability> {
  vec![
    host_capability(GuestTransport::InProcess),
    host_capability(GuestTransport::InheritedDescriptor),
  ]
}

/// The bridge a device loop reaches its volume through: a transient `VolumeBridge` over the shard's
/// store and the volume's slot per pass, with the guest's open handles kept here across passes.
struct ShardBridge {
  volume: DbVolumeId,
  handles: Slab<u64>,
}

impl BridgeAccess for ShardBridge {
  fn with_bridge<R>(&mut self, f: impl FnOnce(&mut dyn Bridge) -> R) -> Result<R, VfsError> {
    let volume = self.volume;
    let handles = &mut self.handles;
    state::with_state(|s| {
      let handle = *s.by_id.get(&volume)?;
      let ShardState { store, volumes, .. } = s;
      let slot = volumes.get_mut(handle).ok()?;
      let mut bridge = VolumeBridge::attached(
        volume,
        &mut slot.volume,
        store,
        handles,
        slot
          .host
          .as_mut()
          .map(|host| host as &mut dyn slates_vfs::host::HostFs),
      );
      Some(f(&mut bridge))
    })
    .flatten()
    .ok_or(VfsError::NotFound)
  }
}

/// Boots and serves one guest device on the owner shard: admission, then the loop until it ends.
async fn serve_guest_device<S: VmmSeam + Send + 'static>(
  volume: DbVolumeId,
  tag: FsTag,
  mut seam: S,
  on_end: OnEnd,
) {
  let found = state::with_state(|s| {
    s.db
      .partition()
      .volume(volume)
      .cloned()
      .map(|record| (record, s.config.guest_credits, s.config.clients_per_shard))
  })
  .flatten();
  let Some((record, credits, bound)) = found else {
    seam.release();
    on_end(GuestDeviceOutcome::VolumeUnknown);
    return;
  };
  let request = GuestAttachRequest {
    transport: GuestTransport::InProcess,
    volume,
    dax: false,
    notification_queue: false,
  };
  // The volume's access list grants three rights (§4.13); the seam enforces the two a device can
  // exercise (`admin` is a control-channel matter, never a filesystem effect).
  let rights = move |principal: &Principal| {
    let granted = rights_of(&record, principal);
    Rights {
      read: granted.read,
      write: granted.write,
    }
  };
  let mut admitted = match admit(request, seam, DeviceConfig::new(tag), credits, rights) {
    Ok(admitted) => admitted,
    Err(refused) => {
      on_end(GuestDeviceOutcome::Refused(refused.error));
      return;
    }
  };
  let mut bridge = ShardBridge {
    volume,
    handles: new_handle_store(),
  };
  let id = match register(bound) {
    Ok(id) => id,
    Err(refused) => {
      // The terminal step still runs: nothing admitted outlives a refused loop.
      let _ = bridge.with_bridge(|b| admitted.reclaim(b));
      on_end(GuestDeviceOutcome::LoopRefused(refused));
      return;
    }
  };
  let end = serve_loop(id, admitted, bridge).await;
  on_end(GuestDeviceOutcome::Ended(end));
}

impl Daemon {
  /// The runtime shard that owns `volume`, or the typed refusal when the partition has no shard.
  fn owner_shard(&self, volume: VolumeId) -> Result<u16, ServerError> {
    let partition = owner_of(volume);
    self
      .shards()
      .get(usize::from(partition))
      .map(|shard| shard.0)
      .ok_or(ServerError::Runtime(RtError::ShardGone {
        shard: partition,
      }))
  }

  /// Attaches a guest device to `volume` under `tag` over the harness's `seam`, served on the
  /// volume's owning shard for its whole life; the outcome reaches the harness through `on_end`.
  /// Refuses typed only when the owner shard cannot be reached (gone, or its control channel full).
  pub fn attach_guest_device<S: VmmSeam + Send + 'static>(
    &self,
    volume: VolumeId,
    tag: FsTag,
    seam: S,
    on_end: OnEnd,
  ) -> Result<(), ServerError> {
    let shard = self.owner_shard(volume)?;
    let device = DbVolumeId {
      bytes: volume.bytes,
    };
    let task = SpawnRequest::new(
      Box::pin(serve_guest_device(device, tag, seam, on_end)),
      None,
    );
    registry::send_control(shard, Control::Spawn(Box::new(task))).map_err(ServerError::Runtime)
  }

  /// Runs `future` on the shard that owns `volume` — where a guest device attached to it is served,
  /// so a harness-side helper (a test's simulated guest driver) shares that shard's thread.
  pub fn spawn_on_owner<F: Future<Output = ()> + Send + 'static>(
    &self,
    volume: VolumeId,
    future: F,
  ) -> Result<(), ServerError> {
    let shard = self.owner_shard(volume)?;
    let task = SpawnRequest::new(Box::pin(future), None);
    registry::send_control(shard, Control::Spawn(Box::new(task))).map_err(ServerError::Runtime)
  }
}
