//! Serving many volumes from one NFS server (§4.6): the design's mount model is "the single kernel
//! mount point per host under which volumes appear as directories" (SLATES_DESIGN §4.6, "Root mount"),
//! so one loopback server serves every volume the daemon holds, not one server per volume. This module
//! is the routing half of that: [`NfsService`] is what the serve loop and `dispatch` serve NFS/MOUNT
//! over — either a single-volume [`Export`] or a [`MultiExport`] that routes among several — and
//! [`MultiExport`] sends each request to the volume its file handle names.
//!
//! Routing is exact and needs no table: an NFSv3 file handle already encodes `(volume, inode, gen)`
//! ([`crate::handle`]), and every served procedure begins with a file handle, so the router reads the
//! leading handle's volume id and hands the untouched request to that volume's [`Export`] (which
//! re-decodes the handle and validates it, exactly as for a single-volume export). A handle for a
//! volume this server does not hold is answered `NFS3ERR_STALE` by any export — the inode is not here.
//!
//! What this slice is, and is not: it routes an already-addressed request to the right volume, and
//! `MNT` resolves a volume's mount name to that volume's root handle, so a client mounts a named
//! volume and every deeper request routes by the handle. The synthetic root *directory* — one `MNT /`
//! whose entries are the volumes, so a client browses `alpha/`, `beta/` under one mount — is the next
//! slice (owed); it needs a read-only directory synthesized above the volumes, not just routing.

use slates_db::catalog::VolumeId;

use crate::handle::FileHandle;
use crate::mount::{MountReply, Mountstat3};
use crate::nfs::Nfsfh3;
use crate::procedures::{Export, NFSPROC3_NULL};
use crate::xdr::XdrReader;

/// What the loopback server serves NFS and MOUNT over: a single-volume [`Export`], or a [`MultiExport`]
/// that routes among several. The serve loop and [`crate::server::serve_connection`] work against this
/// trait, so one server serves one volume or many with no change to the transport.
pub trait NfsService {
  /// MOUNT `MNT`: resolve an export path to a root file handle, or a typed mount refusal.
  fn serve_mount(&mut self, path: &str) -> MountReply;
  /// Dispatch one NFSv3 procedure, returning the accepted reply's result bytes, or `None` for a
  /// procedure this service does not serve (the caller answers `PROC_UNAVAIL`).
  fn serve_procedure(&mut self, procedure: u32, args: &mut XdrReader<'_>) -> Option<Vec<u8>>;
}

impl NfsService for Export<'_> {
  fn serve_mount(&mut self, path: &str) -> MountReply {
    self.mnt(path)
  }

  fn serve_procedure(&mut self, procedure: u32, args: &mut XdrReader<'_>) -> Option<Vec<u8>> {
    self.serve_nfs(procedure, args)
  }
}

/// One volume mounted in a [`MultiExport`]: the name it answers `MNT` under (its chosen path, design
/// §4.6 "Chosen path"), its id for routing, and the export that serves it.
struct Mount<'b> {
  name: String,
  volume: VolumeId,
  export: Export<'b>,
}

/// An NFS service over several volumes, routing each request to the volume its file handle names. The
/// daemon holds one of these per host and serves it on the anchor's loopback listener (owed); here it
/// is driven directly and by the loopback server in tests.
#[derive(Default)]
pub struct MultiExport<'b> {
  mounts: Vec<Mount<'b>>,
}

impl<'b> MultiExport<'b> {
  /// An empty service; add volumes with [`MultiExport::mount`].
  pub fn new() -> MultiExport<'b> {
    MultiExport { mounts: Vec::new() }
  }

  /// Adds `export` (of `volume`) under the mount `name`. The caller supplies the name — a volume's
  /// chosen path — and is responsible for its uniqueness; a duplicate name is simply never matched
  /// after the first, and a duplicate volume routes to whichever mount holds it first.
  pub fn mount(&mut self, name: impl Into<String>, volume: VolumeId, export: Export<'b>) {
    self.mounts.push(Mount {
      name: name.into(),
      volume,
      export,
    });
  }

  /// The number of volumes mounted.
  pub fn len(&self) -> usize {
    self.mounts.len()
  }

  /// Whether no volume is mounted.
  pub fn is_empty(&self) -> bool {
    self.mounts.is_empty()
  }

  /// The index of the mount whose volume matches `volume`, if any.
  fn index_of(&self, volume: VolumeId) -> Option<usize> {
    self.mounts.iter().position(|m| m.volume == volume)
  }
}

impl NfsService for MultiExport<'_> {
  fn serve_mount(&mut self, path: &str) -> MountReply {
    let name = path.trim_matches('/');
    if let Some(mount) = self.mounts.iter_mut().find(|m| m.name == name) {
      return mount.export.mnt(path);
    }
    // A bare "/" with a single volume mounts it — the single-volume case a client mounts as the root.
    if name.is_empty() && self.mounts.len() == 1 {
      return self.mounts[0].export.mnt(path);
    }
    MountReply::Err(Mountstat3::Noent)
  }

  fn serve_procedure(&mut self, procedure: u32, args: &mut XdrReader<'_>) -> Option<Vec<u8>> {
    if procedure == NFSPROC3_NULL {
      return Some(Vec::new());
    }
    // Route by the volume id in the request's leading file handle. An unknown or unparseable volume
    // routes to the first export, which answers the handle `NFS3ERR_STALE`/`NFS3ERR_BADHANDLE` with the
    // procedure's correct reply shape — never a made-up reply here.
    let target = peek_handle_volume(args)
      .and_then(|volume| self.index_of(volume))
      .unwrap_or(0);
    self
      .mounts
      .get_mut(target)?
      .export
      .serve_nfs(procedure, args)
  }
}

/// The volume id named by the leading file handle of a request, read through a fresh reader so the
/// original is untouched for the chosen export; `None` if no valid handle leads the request.
fn peek_handle_volume(args: &XdrReader<'_>) -> Option<VolumeId> {
  let mut peek = XdrReader::new(args.rest());
  let handle = Nfsfh3::decode(&mut peek).ok()?;
  FileHandle::from_fh(&handle)
    .ok()
    .map(|decoded| decoded.volume)
}
