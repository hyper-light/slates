//! The daemon's per-mount FSKit serve session (§4.6).
//!
//! One of these lives per mounted volume on its owning shard. It owns the open-handle map that must
//! persist across requests — the transient per-request bridge cannot keep it, because the volume lives
//! in the shard's slab and the bridge borrows it for a single operation. The open *reference* each
//! handle takes lives in the `Volume` (which persists in the shard regardless); only this `fh`→inode
//! map needed a home outside the transient bridge, so it lives here. Each request is served by building
//! a bridge with [`VolumeBridge::attached`] over the store and volume the shard supplies, so the session
//! borrows nothing durable from them.
//!
//! This is the Rust shape of what a garbage-collected mount handler does with one long-lived object
//! (a cgofuse handler, say): the per-mount state lives here, and the volume access is threaded in per
//! call rather than held — the borrow checker's price for not having a GC, and a small one.

use crate::{ShimWireError, serve};
use slates_bridge_core::{OpContext, VolumeBridge, new_handle_store};
use slates_db::catalog::VolumeId;
use slates_mem::Slab;
use slates_vfs::volume::{Store, Volume};

/// One mounted volume's serve session: the volume id and the open-handle map that outlives any single
/// request.
pub struct MountSession {
  id: VolumeId,
  handles: Slab<u64>,
}

impl MountSession {
  /// A fresh session for the volume `id`, with an empty handle map.
  pub fn new(id: VolumeId) -> MountSession {
    MountSession {
      id,
      handles: new_handle_store(),
    }
  }

  /// Serve one encoded shim `request` against `volume` in `store` under `cx`, returning the encoded
  /// reply. A request the bridge refuses becomes an error reply *inside* the returned bytes; only a
  /// malformed request (bad framing) is an `Err` the caller refuses without touching the volume. The
  /// bridge is built fresh for this one call over the caller's store and volume; the handle map the
  /// session owns is lent to it, so an `fh` an earlier request opened is honored here.
  pub fn serve(
    &mut self,
    store: &mut Store,
    volume: &mut Volume,
    cx: &OpContext,
    request: &[u8],
  ) -> Result<Vec<u8>, ShimWireError> {
    let mut bridge = VolumeBridge::attached(self.id, volume, store, &mut self.handles);
    serve(request, &mut bridge, cx)
  }
}
