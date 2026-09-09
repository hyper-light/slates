//! The daemon's NFS transport (§4.6): one loopback listener whose connections serve the shard's own
//! volumes over the shared operation layer, so a `mount_nfs localhost:PORT` reaches the volumes this
//! daemon provisioned — the signing-free macOS mount path (D-O9), run against real state.
//!
//! The volumes live in the owning shard's [`ShardState`](crate::state::ShardState) — one store, a slab
//! of volume slots — so a request is served through a *transient* bridge built for it over that store
//! and the routed volume ([`VolumeBridge::attached`], "the daemon's serve path"): the shard owns the
//! volume and its base host, the bridge is rebuilt per request (§4.6, "marshal each operation into the
//! bridge queue of the owning shard"). [`ShardVolumeSet`] is that seam for the NFS server's
//! [`MultiExport`], resolved through [`state::with_state`], so the routing and the synthetic root the
//! server carries sit unchanged above it.
//!
//! Scope: this serves the volumes on the shard the connection is accepted on. On a single-shard daemon
//! that is every volume (the laptop-degenerate case, R8). Serving volumes on *other* shards needs the
//! cross-shard bridge queue (§4.3, D-7 "bridge queues pinned to the owner"), which routes a request to
//! its volume's owner and returns the reply the way the client path's `forward`/`deliver` already do —
//! owed. Per-mount authentication (§4.13) is owed too; every request runs as root read-write for now.

use slates_bridge_core::{Rights, VolumeBridge, new_handle_store};
use slates_bridge_nfs::nfs::{Fattr3, Nfsfh3};
use slates_bridge_nfs::procedures::Export;
use slates_bridge_nfs::xdr::XdrReader;
use slates_bridge_nfs::{MultiExport, VolumeSet, serve_connection_async};
use slates_db::catalog::{Principal, VolumeId};
use slates_rt::futures;
use slates_rt::tcp::{TcpListener, TcpStream};

use crate::state::{self, ShardState};

/// The shard's volumes as an NFS [`VolumeSet`]: resolved through [`state::with_state`], each served by a
/// transient bridge over the shard's store and the routed volume's slot. A zero-size handle — the state
/// it reads is the current shard's, so a connection task carries one and the state stays thread-local.
struct ShardVolumeSet;

impl VolumeSet for ShardVolumeSet {
  fn entries(&self) -> Vec<(String, VolumeId)> {
    state::with_state(|s| s.by_id.keys().map(|id| (hex(id), *id)).collect()).unwrap_or_default()
  }

  fn serve(
    &mut self,
    volume: VolumeId,
    subject: Principal,
    rights: Rights,
    procedure: u32,
    args: &mut XdrReader<'_>,
  ) -> Option<Vec<u8>> {
    state::with_state(|s| {
      with_export(s, volume, subject, rights, |export| {
        export.serve_nfs(procedure, args)
      })
    })
    .flatten()
    .flatten()
  }

  fn root_object(
    &mut self,
    volume: VolumeId,
    subject: Principal,
    rights: Rights,
  ) -> Option<(Nfsfh3, Fattr3)> {
    state::with_state(|s| with_export(s, volume, subject, rights, |export| export.root_object()))
      .flatten()
      .flatten()
  }
}

/// Builds a transient export for `volume` over the shard's store and the volume's slot, and runs `f`
/// with it; `None` if the shard does not hold the volume or its attachment cannot be admitted. The
/// handle slab is fresh per request — NFS keeps no open state across requests — and the volume's base
/// host (for an overlay) is lent from the slot, both through [`VolumeBridge::attached`].
fn with_export<R>(
  s: &mut ShardState,
  volume: VolumeId,
  subject: Principal,
  rights: Rights,
  f: impl FnOnce(&mut Export<'_>) -> R,
) -> Option<R> {
  let handle = *s.by_id.get(&volume)?;
  let ShardState { store, volumes, .. } = s;
  let slot = volumes.get_mut(handle).ok()?;
  let mut handles = new_handle_store();
  let mut bridge = VolumeBridge::attached(
    volume,
    &mut slot.volume,
    store,
    &mut handles,
    slot.host.as_mut(),
  );
  let mut export = Export::new(&mut bridge, volume, subject, rights).ok()?;
  Some(f(&mut export))
}

/// The mount name a volume appears under in the root listing: its id in hex. A friendly chosen-path
/// name (§4.6 "Chosen path") is owed; the id is unique and stable, so `ls /` and `cd <id>` work today.
fn hex(id: &VolumeId) -> String {
  use std::fmt::Write as _;
  let mut out = String::with_capacity(id.bytes.len().saturating_mul(2));
  for byte in id.bytes {
    // Two lowercase hex digits per byte; the write into a String cannot fail.
    let _ = write!(out, "{byte:02x}");
  }
  out
}

/// The credentials every NFS request runs under until per-mount authentication lands (§4.13): the
/// machine's root, read-write, so a mount can read and write the volumes it reaches.
fn mount_rights() -> Rights {
  Rights {
    read: true,
    write: true,
  }
}

/// Serves NFS/MOUNT/portmap over `listener` on the current shard until the daemon stops: each
/// connection becomes a detached task serving the shard's volumes, so connections are concurrent (each
/// awaits its own socket; a request borrows the shard state only for the synchronous serve). `port`
/// answers a portmap `GETPORT` (this server serves every program on the one port).
pub async fn serve(listener: TcpListener, port: u16) {
  while let Ok(stream) = listener.accept().await {
    if let Ok(task) = futures::spawn(serve_one(stream, port)) {
      let _ = futures::detach(task);
    }
  }
}

/// Serves one accepted connection over the shard's volumes.
async fn serve_one(mut stream: TcpStream, port: u16) {
  let mut service = MultiExport::new(ShardVolumeSet, Principal::Uid { uid: 0 }, mount_rights());
  let _ = serve_connection_async(&mut stream, &mut service, port).await;
}
