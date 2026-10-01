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
//!
//! A device presents what its [`GuestView`] names (AUD-29-76): the head, one directory of it (served through
//! `ScopedBridge`, so nothing outside is reached), or a snapshot read-only (its own view, admitted with no
//! write, closed with the device).

use std::future::Future;

use slates_bridge_core::{Bridge, Rights, VolumeBridge, new_handle_store};
use slates_bridge_virtiofs::admission::{
  AdmissionError, GuestAttachRequest, GuestTransport, ReclaimError, VmmSeam, admit,
};
use slates_bridge_virtiofs::capability::{TransportCapability, host_capability};
use slates_bridge_virtiofs::device::{DeviceConfig, FsTag};
use slates_bridge_virtiofs::serve::{BridgeAccess, LoopError, ServeEnd, register, serve_loop};
use slates_db::catalog::{Principal, VolumeId as DbVolumeId};
use slates_ipc::protocol::{Refusal, VolumeId};
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
  /// The view asked for was refused before admission (typed, AUD-29-76): a subtree naming nothing or no
  /// directory, a snapshot that is gone or of an overlay, or both at once. The seam was released.
  ViewRefused(Refusal),
  /// The vhost-user front end did not configure the device within the harness's handshake bound, closed its
  /// end, or broke the protocol (AUD-29-68); `None` for the bound passing. Nothing was admitted.
  #[cfg(target_os = "linux")]
  HandshakeRefused(Option<slates_bridge_virtiofs::vhost_user::VhostError>),
  /// The device's attachment record could not be committed (typed); the admitted device was reclaimed.
  RecordRefused(Refusal),
}

/// What a guest device presents of its volume (§4.4 `attach(volume|snapshot, ...)`, §4.6 scoped exports;
/// AUD-29-76): the head (the default), one directory of it, or a snapshot read-only. A subtree of a snapshot
/// is refused, as for a host mount.
#[derive(Clone, Debug, Default)]
pub struct GuestView {
  /// The directory presented, as a path from the volume's root: the device's root, nothing outside it reached.
  pub subtree: Option<String>,
  /// The snapshot presented, read-only, through the device's own copy-on-write view that pins it.
  pub snapshot: Option<slates_ipc::protocol::SnapshotId>,
}

/// The harness's callback for the outcome.
pub type OnEnd = Box<dyn FnOnce(GuestDeviceOutcome) + Send>;
/// The harness's callback for the admitted device's attachment record id (`detach` ends the device; a snapshot
/// device's `advance` moves its view).
pub type OnAdmitted = Box<dyn FnOnce(u64) + Send>;

/// What the harness learns of a device it attached: its attachment record once admitted, and its outcome.
pub struct GuestHarness {
  /// Called once the device is admitted and recorded, with the record's id.
  pub on_admitted: OnAdmitted,
  /// Called once, with what became of the device.
  pub on_end: OnEnd,
}

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
  /// The directory the device presents, if one (AUD-29-76).
  scope: Option<u64>,
  /// The device's attachment record id: the key of its snapshot view, when it presents one.
  attachment: u64,
  /// Whether the device presents a snapshot (through the view kept under `attachment`).
  presents_snapshot: bool,
}

/// Closes device `attachment`'s snapshot view, if it has one, and removes its record, if it has one (its loop's
/// end, or a refusal); a refused removal is counted.
fn close_device(attachment: u64) {
  let _ = state::with_state(|s| {
    crate::snapshot_view::end(s, attachment);
    if let Some(record) = s.db.partition().attachment(attachment).cloned()
      && crate::verbs::end_attachment(s, &record).is_err()
    {
      *s.refusals.entry(RECORD_END_REFUSED).or_insert(0) += 1;
    }
  });
}

/// Format: the refusal-ledger name of a guest record whose removal at the device's end was refused.
const RECORD_END_REFUSED: &str = "virtiofs.record_end_refused";

impl BridgeAccess for ShardBridge {
  fn with_bridge<R>(
    &mut self,
    f: impl FnOnce(&mut dyn Bridge, &mut slates_bridge_core::Attachments) -> R,
  ) -> Result<R, VfsError> {
    let volume = self.volume;
    let (scope, attachment, presents_snapshot) =
      (self.scope, self.attachment, self.presents_snapshot);
    let handles = &mut self.handles;
    state::with_state(|s| {
      let handle = *s.by_id.get(&volume)?;
      let ShardState {
        store,
        volumes,
        attachments,
        snapshot_views,
        ..
      } = s;
      // A snapshot device serves its read-only view (moved by `advance`); every other device, the head.
      let mut bridge = if presents_snapshot {
        let view = snapshot_views.get_mut(&attachment)?;
        VolumeBridge::attached(volume, &mut view.volume, store, handles, None)
      } else {
        let slot = volumes.get_mut(handle).ok()?;
        VolumeBridge::attached(
          volume,
          &mut slot.volume,
          store,
          handles,
          slot
            .host
            .as_mut()
            .map(|host| host as &mut dyn slates_vfs::host::HostFs),
        )
      };
      let mut scoped;
      let served: &mut dyn Bridge = match scope {
        Some(scope) => {
          scoped = slates_bridge_core::scoped::ScopedBridge::new(&mut bridge, scope);
          &mut scoped
        }
        None => &mut bridge,
      };
      Some(f(served, attachments))
    })
    .flatten()
    .ok_or(VfsError::NotFound)
  }

  /// The §4.8 barrier the daemon's other transports run before a mutation's reply (NFS's stable
  /// procedures, a FUSE mount's turn): the shard's recovery image is published, and the guest's reply is
  /// released only when it captured this volume (AUD-29-82). A refusal is counted.
  fn barrier(&mut self) -> bool {
    let volume = self.volume;
    state::with_state(|s| {
      let captured =
        crate::verbs::publish_shard(s).is_ok_and(|published| published.captured(volume));
      if !captured {
        *s.refusals.entry(BARRIER_REFUSED).or_insert(0) += 1;
      }
      captured
    })
    .unwrap_or(false)
  }

  /// The owner shard's attachment registry, reachable when the volume is not (AUD-29-70).
  fn with_registry<R>(
    &mut self,
    f: impl FnOnce(&mut slates_bridge_core::Attachments) -> R,
  ) -> Option<R> {
    state::with_state(|s| f(&mut s.attachments))
  }

  /// The live-tree fence the NFS mount applies (AUD-29-83): while it stands the guest's requests wait in its
  /// rings, asked again each heartbeat — the cadence at which the confirmations that restore the lease arrive.
  fn fenced(&mut self) -> Option<u64> {
    let volume = self.volume;
    state::with_state(|s| crate::verbs::live_tree_fenced(s, volume))
      .unwrap_or(false)
      .then_some(crate::daemon::HEARTBEAT_NS)
  }
}

/// Format: the refusal-ledger name of a refused device's terminal step whose sweep could not run.
const RECLAIM_INCOMPLETE: &str = "virtiofs.reclaim_incomplete";
/// Format: the refusal-ledger name of a guest barrier that did not capture its volume.
const BARRIER_REFUSED: &str = "virtiofs.barrier_refused";

/// Boots and serves one guest device on the owner shard: the view and the record's id resolved, admission,
/// the durable record, then the loop until it ends; the record and the view end with it.
async fn serve_guest_device<S: VmmSeam + Send + 'static>(
  volume: DbVolumeId,
  tag: FsTag,
  view: GuestView,
  (mut seam, transport): (S, GuestTransport),
  harness: GuestHarness,
) {
  let GuestHarness {
    on_admitted,
    on_end,
  } = harness;
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
  let mut bridge = match state::with_state(|s| open_view(s, volume, &view)) {
    Some(Ok(bridge)) => bridge,
    Some(Err(refusal)) => {
      seam.release();
      on_end(GuestDeviceOutcome::ViewRefused(refusal));
      return;
    }
    None => {
      seam.release();
      on_end(GuestDeviceOutcome::VolumeUnknown);
      return;
    }
  };
  let attachment = bridge.attachment;
  // A snapshot is immutable: its device is admitted with no write, whatever the access list grants.
  let writable = !bridge.presents_snapshot;
  let request = GuestAttachRequest {
    transport,
    volume,
    dax: false,
    notification_queue: false,
  };
  // The volume's access list grants three rights (§4.13); the seam enforces the two a device can
  // exercise (`admin` is a control-channel matter, never a filesystem effect).
  let granted = record.clone();
  let rights = move |principal: &Principal| {
    let granted = rights_of(&granted, principal);
    Rights {
      read: granted.read,
      write: granted.write && writable,
    }
  };
  // Admitted into the owner shard's attachment registry — the one every transport on the volume
  // rides, so the owner's barriers close the device's generation with the mounts' (GAP-A9-4). The
  // seam is held in a slot so that a shard whose state is out of reach still releases it.
  let mut seam_slot = Some(seam);
  let admission = state::with_state(|s| {
    seam_slot.take().map(|seam| {
      admit(
        request,
        seam,
        DeviceConfig::new(tag),
        credits,
        rights,
        &mut s.attachments,
      )
    })
  })
  .flatten();
  let mut admitted = match admission {
    Some(Ok(admitted)) => admitted,
    Some(Err(refused)) => {
      close_device(attachment);
      on_end(GuestDeviceOutcome::Refused(refused.error));
      return;
    }
    None => {
      if let Some(mut seam) = seam_slot.take() {
        seam.release();
      }
      close_device(attachment);
      on_end(GuestDeviceOutcome::VolumeUnknown);
      return;
    }
  };
  // The device's durable record (AUD-29-68): its consumer, rights, version and tag, under the id resolved
  // with its view, so `detach`, `advance`, `status` and recovery see it as they see every attachment. Then its
  // loop's slot: a refusal of either reclaims the admitted device.
  let consumer = admitted.consumer().clone();
  let recorded = state::with_state(|s| {
    record_guest(
      s,
      &record,
      (&bridge, &view),
      (tag, consumer.clone(), writable),
    )
  })
  .unwrap_or(Err(Refusal::NotFound));
  let (registered, refused) = match recorded {
    Err(refusal) => (None, GuestDeviceOutcome::RecordRefused(refusal)),
    Ok(()) => match register(bound) {
      Ok(id) => (Some(id), GuestDeviceOutcome::VolumeUnknown),
      Err(refusal) => (None, GuestDeviceOutcome::LoopRefused(refusal)),
    },
  };
  let id = match registered {
    Some(id) => id,
    None => {
      let outcome = refused;
      // The terminal step still runs to its end: nothing admitted outlives a refused loop (AUD-29-70) — through
      // the bridge, or the registry alone when the volume is gone; a sweep that could not run is counted.
      let reclaimed = match bridge.with_bridge(|b, registry| admitted.reclaim(b, registry)) {
        Ok(reclaimed) => reclaimed,
        Err(_) => bridge
          .with_registry(|registry| admitted.abandon(registry))
          .unwrap_or(Err(ReclaimError::VolumeGone)),
      };
      if !reclaimed.is_ok_and(|r| r.references_swept) {
        let _ = state::with_state(|s| *s.refusals.entry(RECLAIM_INCOMPLETE).or_insert(0) += 1);
      }
      close_device(attachment);
      on_end(outcome);
      return;
    }
  };
  // The device is known to its consumer's revocation and to its record's detach for as long as its loop runs
  // (AUD-29-73).
  let _ = state::with_state(|s| s.guest_devices.push((id, consumer, attachment)));
  on_admitted(attachment);
  let end = serve_loop(id, admitted, bridge).await;
  // The loop's terminal step has swept the device's references; now its record and view end.
  let _ = state::with_state(|s| s.guest_devices.retain(|(device, _, _)| *device != id));
  close_device(attachment);
  on_end(GuestDeviceOutcome::Ended(end));
}

/// Commits the device's attachment record: refused typed (the caller then reclaims the device).
fn record_guest(
  s: &mut ShardState,
  volume: &slates_db::catalog::VolumeRecord,
  (bridge, view): (&ShardBridge, &GuestView),
  (tag, principal, writable): (FsTag, Principal, bool),
) -> Result<(), Refusal> {
  let granted = rights_of(volume, &principal);
  let record = slates_db::catalog::AttachmentRecord {
    id: bridge.attachment,
    volume: volume.id,
    consumer: slates_db::catalog::Consumer::Guest,
    snapshot: view.snapshot.map(crate::verbs::to_db_snapshot),
    form: slates_db::catalog::AttachForm::GuestTag {
      tag: tag.as_str().to_owned(),
      scope: bridge.scope,
    },
    principal,
    rights: slates_db::catalog::Rights {
      read: granted.read,
      write: granted.write && writable,
      admin: false,
    },
    // A guest presents no mount capability: the zero token is refused by every edge that checks one.
    token: [0; 16],
  };
  let now = slates_vfs::clock::Clock::monotonic_ns(&mut s.clock);
  s.db
    .mutate(
      &mut s.segment,
      &slates_db::op::Op::AttachmentAdded { record },
      now,
    )
    .map(|_| ())
    .map_err(|e| crate::error::refusal_of_db(&e))
}

/// Adopts and negotiates a vhost-user front end on the owning shard, then serves the device as any other.
#[cfg(target_os = "linux")]
async fn serve_vhost_user_device(
  volume: DbVolumeId,
  tag: FsTag,
  view: GuestView,
  (socket, handshake_ns): (std::os::fd::OwnedFd, u64),
  harness: GuestHarness,
) {
  use slates_bridge_virtiofs::vhost_user::VhostUserSeam;
  let config = DeviceConfig::new(tag);
  let seam = match VhostUserSeam::adopt(socket, &config) {
    Ok(seam) => seam,
    Err(refused) => {
      (harness.on_end)(GuestDeviceOutcome::HandshakeRefused(Some(refused)));
      return;
    }
  };
  // The front end configures the device as its guest boots its driver, within the harness's bound.
  let seam = match slates_rt::futures::within(handshake_ns, seam.negotiate()).await {
    Ok(Some(Ok(seam))) => seam,
    Ok(Some(Err(refused))) => {
      (harness.on_end)(GuestDeviceOutcome::HandshakeRefused(Some(refused)));
      return;
    }
    Ok(None) | Err(_) => {
      (harness.on_end)(GuestDeviceOutcome::HandshakeRefused(None));
      return;
    }
  };
  serve_guest_device(
    volume,
    tag,
    view,
    (seam, GuestTransport::InheritedDescriptor),
    harness,
  )
  .await;
}

/// Resolves what a device presents, before admission: its attachment record's id (allocated now), the scope's
/// inode (a subtree of the head), and its snapshot view (opened here under that id, pinning the snapshot).
/// Refused typed with nothing opened.
fn open_view(
  s: &mut ShardState,
  volume: DbVolumeId,
  view: &GuestView,
) -> Result<ShardBridge, Refusal> {
  let scope = match (&view.subtree, view.snapshot) {
    (Some(_), Some(_)) => {
      return Err(Refusal::AttachmentUnsupported {
        transport: slates_ipc::protocol::AttachTransport::VirtioFsInProcess,
        reason: slates_ipc::protocol::UnsupportedReason::SnapshotNotPresentedByHostMount,
      });
    }
    (Some(subtree), None) => Some(crate::verbs::resolve_scope(s, volume, subtree)?),
    (None, _) => None,
  };
  let attachment = crate::verbs::next_attachment_id(s);
  if let Some(snapshot) = view.snapshot {
    let names = s
      .db
      .partition()
      .volume(volume)
      .map_or(slates_ipc::protocol::NamePolicy::Exact, |record| {
        crate::verbs::wire_names(record.policy.names)
      });
    let opened =
      crate::snapshot_view::open(s, volume, crate::verbs::core_snapshot(snapshot), names)?;
    s.snapshot_views.insert(attachment, opened);
  }
  Ok(ShardBridge {
    volume,
    handles: new_handle_store(),
    scope,
    attachment,
    presents_snapshot: view.snapshot.is_some(),
  })
}

/// Asks every device loop this shard serves for `consumer` to revoke (§4.13; AUD-29-73). Run by the consumer's
/// revocation on each shard before it is acknowledged: the loops run on this shard's thread and check the request
/// at every pass boundary, so none serves a request after the acknowledgement; each then runs its terminal step,
/// sweeping its references under its still-live attachment. The number asked.
pub(crate) fn revoke_consumer_devices(s: &ShardState, consumer: u64) -> usize {
  s.guest_devices
    .iter()
    .filter(|(_, principal, _)| {
      matches!(principal, Principal::Consumer { consumer: c, .. } if *c == consumer)
    })
    .filter(|(device, _, _)| slates_bridge_virtiofs::serve::request_revoke(*device))
    .count()
}

/// Asks every device loop serving volume `volume` to revoke (its destroy): the number still serving it, each of
/// which ends at its next pass boundary, its terminal step sweeping through its view before the view closes.
pub(crate) fn revoke_volume_devices(s: &ShardState, volume: DbVolumeId) -> usize {
  let partition = s.db.partition();
  s.guest_devices
    .iter()
    .filter(|(_, _, attachment)| {
      partition
        .attachment(*attachment)
        .is_some_and(|record| record.volume == volume)
    })
    .inspect(|(device, _, _)| {
      slates_bridge_virtiofs::serve::request_revoke(*device);
    })
    .count()
}

/// Asks the device loop serving attachment record `attachment` to revoke (its `detach`, or its volume's
/// destroy): the loop checks at its next pass boundary and runs its terminal step. Whether one was asked.
pub(crate) fn revoke_attachment_device(s: &ShardState, attachment: u64) -> bool {
  s.guest_devices
    .iter()
    .filter(|(_, _, recorded)| *recorded == attachment)
    .any(|(device, _, _)| slates_bridge_virtiofs::serve::request_revoke(*device))
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

  /// Attaches a guest device to `volume` under `tag` over the harness's `seam`, presenting `view` (the head, a
  /// subtree, or a snapshot read-only), served on the volume's owning shard for its whole life; the outcome
  /// reaches the harness through `on_end`.
  /// Refuses typed only when the owner shard cannot be reached (gone, or its control channel full).
  pub fn attach_guest_device<S: VmmSeam + Send + 'static>(
    &self,
    volume: VolumeId,
    tag: FsTag,
    view: GuestView,
    seam: S,
    harness: GuestHarness,
  ) -> Result<(), ServerError> {
    let shard = self.owner_shard(volume)?;
    let device = DbVolumeId {
      bytes: volume.bytes,
    };
    let task = SpawnRequest::new(
      Box::pin(serve_guest_device(
        device,
        tag,
        view,
        (seam, GuestTransport::InProcess),
        harness,
      )),
      None,
    );
    registry::send_control(shard, Control::Spawn(Box::new(task))).map_err(ServerError::Runtime)
  }

  /// Attaches a guest device to `volume` under `tag` over a vhost-user front end (the inherited-descriptor form,
  /// AUD-29-68): `socket` is a connected stream socket whose other end the VMM holds (a `socketpair` end, never
  /// a socket on disk). On the owning shard the consumer is read from the socket's peer, the front end is
  /// negotiated within `handshake_ns`, and the device is admitted and served exactly as the in-process form is;
  /// the outcome reaches the harness through `on_end`. The bound is the harness's: a VMM configures the rings
  /// only once its guest's driver starts, after the guest boots, and the harness owns the VM and knows its boot
  /// budget (§4.6 "The harness owns VM and container creation").
  #[cfg(target_os = "linux")]
  pub fn attach_vhost_user_device(
    &self,
    volume: VolumeId,
    tag: FsTag,
    view: GuestView,
    (socket, handshake_ns): (std::os::fd::OwnedFd, u64),
    harness: GuestHarness,
  ) -> Result<(), ServerError> {
    let shard = self.owner_shard(volume)?;
    let device = DbVolumeId {
      bytes: volume.bytes,
    };
    let task = SpawnRequest::new(
      Box::pin(serve_vhost_user_device(
        device,
        tag,
        view,
        (socket, handshake_ns),
        harness,
      )),
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
