//! The NFSv3 and MOUNT procedures over the shared operation layer (§4.6, Phase 4 task 3): a parsed
//! RPC call is turned into a call on the transport-independent [`Bridge`] and its neutral result is
//! encoded into the procedure's XDR reply. This is the NFS analogue of the FUSE `dispatch`, so the
//! NFS server and the FUSE mount serve one volume core and the differential oracle can compare
//! their abstract states (§4.6 "the fallback is also the differential oracle").
//!
//! An NFSv3 file handle names its object by identity ([`crate::handle`]), so these procedures are
//! stateless: they decode a handle to an inode, act, and mint handles for the objects they return —
//! no server-side open table. This slice serves the metadata path a client walks first: MOUNT
//! `MNT` (the export's root handle), `NULL`, `GETATTR` and `LOOKUP`. `READ` and `WRITE`, which the
//! design keys by inode but the current `Bridge` keys by an open handle, are the next slice (owed);
//! so are the remaining namespace and directory procedures.

use slates_bridge_core::{Bridge, FsStat, NodeAttr};
use slates_db::catalog::VolumeId;
use slates_vfs::error::VfsError;
use slates_vfs::inode::Kind;

use crate::handle::{FileHandle, FileHandleError};
use crate::mount::{MountReply, Mountstat3};
use crate::nfs::{Fattr3, Ftype3, Nfsfh3, Nfsstat3, Nfstime3, PostOpAttr, Specdata3};
use crate::xdr::{XdrReader, XdrWriter};

/// Format: the NFS program number (RFC 1813).
pub const NFS_PROGRAM: u32 = 100_003;
/// Format: the NFS protocol version this bridge speaks.
pub const NFS_VERSION: u32 = 3;
/// Format: NFSPROC3_NULL — a ping, no arguments and no results.
pub const NFSPROC3_NULL: u32 = 0;
/// Format: NFSPROC3_GETATTR — the attributes of the object a handle names.
pub const NFSPROC3_GETATTR: u32 = 1;
/// Format: NFSPROC3_LOOKUP — resolve a name in a directory to a handle.
pub const NFSPROC3_LOOKUP: u32 = 3;
/// Format: NFSPROC3_ACCESS — which operations the caller may perform on an object.
pub const NFSPROC3_ACCESS: u32 = 4;
/// Format: NFSPROC3_FSSTAT — dynamic filesystem statistics (space and file counts).
pub const NFSPROC3_FSSTAT: u32 = 18;
/// Format: NFSPROC3_FSINFO — static filesystem limits and capabilities.
pub const NFSPROC3_FSINFO: u32 = 19;
/// Format: the maximum bytes in a filename slates resolves (§4.5's name cap), refused before
/// allocating.
pub const NFS_MAXNAMELEN: usize = 255;
/// Format: the `AUTH_SYS` authentication flavor (RFC 5531).
const AUTH_SYS: u32 = 1;
/// Format: the `AUTH_NONE` authentication flavor (RFC 5531).
const AUTH_NONE: u32 = 0;
/// Shape: the largest transfer the server offers, one arena chunk (256 KiB), matched to the volume
/// core's read cap and the design's "readahead = large chunk size" (§4.6).
const MAX_TRANSFER: u32 = 256 * 1024;
/// Format: the transfer-size multiple the server prefers (one page).
const TRANSFER_MULTIPLE: u32 = 4096;
/// Format: FSF3_LINK, the filesystem supports hard links.
const FSF3_LINK: u32 = 0x1;
/// Format: FSF3_SYMLINK, the filesystem supports symbolic links.
const FSF3_SYMLINK: u32 = 0x2;
/// Format: FSF3_HOMOGENEOUS, PATHCONF is uniform across the filesystem.
const FSF3_HOMOGENEOUS: u32 = 0x8;
/// Format: FSF3_CANSETTIME, the server can set times through SETATTR.
const FSF3_CANSETTIME: u32 = 0x10;
/// Format: ACCESS3_READ, read file data or list a directory.
const ACCESS3_READ: u32 = 0x1;
/// Format: ACCESS3_LOOKUP, look a name up in a directory.
const ACCESS3_LOOKUP: u32 = 0x2;
/// Format: ACCESS3_MODIFY, change existing file data.
const ACCESS3_MODIFY: u32 = 0x4;
/// Format: ACCESS3_EXTEND, add to a file or directory.
const ACCESS3_EXTEND: u32 = 0x8;
/// Format: ACCESS3_DELETE, remove a directory entry.
const ACCESS3_DELETE: u32 = 0x10;
/// Format: ACCESS3_EXECUTE, execute a file or search a directory.
const ACCESS3_EXECUTE: u32 = 0x20;
/// Format: the owner read permission bit.
const OWNER_READ: u32 = 0o400;
/// Format: the owner write permission bit.
const OWNER_WRITE: u32 = 0o200;
/// Format: the owner execute permission bit.
const OWNER_EXECUTE: u32 = 0o100;

/// An NFSv3 export of one volume over the shared operation layer. Handles it mints and accepts name
/// objects of `volume`; a handle for another volume is refused stale.
pub struct Export<'b> {
  bridge: &'b mut dyn Bridge,
  volume: VolumeId,
}

impl<'b> Export<'b> {
  /// An export of the volume `bridge` serves, identified by `volume` for the file handles.
  pub fn new(bridge: &'b mut dyn Bridge, volume: VolumeId) -> Export<'b> {
    Export { bridge, volume }
  }

  /// The filesystem id the export reports: the leading 64 bits of the volume id, stable per volume.
  fn fsid(&self) -> u64 {
    let mut eight = [0u8; size_of::<u64>()];
    eight.copy_from_slice(&self.volume.bytes[..size_of::<u64>()]);
    u64::from_be_bytes(eight)
  }

  /// Mints a file handle for an inode of this export's volume.
  fn handle_for(&self, ino: u64, generation: u64) -> Nfsfh3 {
    FileHandle {
      volume: self.volume,
      inode: ino,
      generation,
    }
    .to_fh()
  }

  /// The identity a file handle names — its inode *and* generation — or a status refusal: a
  /// malformed handle is `Badhandle`, an incompatible-version or foreign-volume handle is `Stale`.
  /// The generation is carried, never discarded, so a reused inode is caught by [`Export::attrs_of`].
  fn resolve_handle(&self, fh: &Nfsfh3) -> Result<FileHandle, Nfsstat3> {
    let decoded = FileHandle::from_fh(fh).map_err(|e| match e {
      FileHandleError::Malformed => Nfsstat3::Badhandle,
      FileHandleError::UnknownVersion => Nfsstat3::Stale,
    })?;
    if decoded.volume != self.volume {
      return Err(Nfsstat3::Stale);
    }
    Ok(decoded)
  }

  /// The attributes of the object a resolved handle names, refusing a handle whose generation no
  /// longer matches the object's — a stale handle to a reused inode is `Stale`, never answered from
  /// whatever now holds the number. (Generation tracking in the volume core is owed, §4.6 `(no,
  /// gen)`; until then every live generation is zero and the check is exact but trivial.)
  fn attrs_of(&mut self, identity: &FileHandle) -> Result<NodeAttr, Nfsstat3> {
    let node = match self.bridge.getattr(identity.inode) {
      Ok(node) => node,
      // A handle to an inode the volume no longer has is *stale*, not "no such entry": inode
      // numbers are never reused (D-4), so a gone number means the object the handle named is
      // gone, which is exactly `NFS3ERR_STALE`.
      Err(VfsError::NotFound) => return Err(Nfsstat3::Stale),
      Err(e) => return Err(nfsstat_of(&e)),
    };
    if node.generation != identity.generation {
      return Err(Nfsstat3::Stale);
    }
    Ok(node)
  }

  /// The `fattr3` for a neutral attribute set of this export.
  fn fattr3(&self, node: &NodeAttr) -> Fattr3 {
    Fattr3 {
      kind: ftype3_of(node.kind),
      mode: node.mode,
      nlink: node.nlink,
      uid: node.uid,
      gid: node.gid,
      size: node.size,
      used: node.size,
      rdev: Specdata3::default(),
      fsid: self.fsid(),
      fileid: node.ino,
      atime: nfstime_of(node.atime),
      mtime: nfstime_of(node.mtime),
      ctime: nfstime_of(node.ctime),
    }
  }

  /// MOUNT `MNT`: resolve an export path to its root file handle. A single-volume export answers
  /// any path with its own root; the path-to-volume resolution of a multi-volume export is owed.
  pub fn mnt(&mut self, _path: &str) -> MountReply {
    match self.bridge.root() {
      Ok(root) => MountReply::Ok {
        handle: self.handle_for(root, 0),
        auth_flavors: vec![AUTH_SYS, AUTH_NONE],
      },
      Err(_) => MountReply::Err(Mountstat3::ServerFault),
    }
  }

  /// Dispatches one NFSv3 procedure, returning the accepted reply's result bytes, or `None` for a
  /// procedure this slice does not serve (the caller answers `PROC_UNAVAIL`).
  pub fn serve_nfs(&mut self, procedure: u32, args: &mut XdrReader<'_>) -> Option<Vec<u8>> {
    match procedure {
      NFSPROC3_NULL => Some(Vec::new()),
      NFSPROC3_GETATTR => Some(self.getattr(args)),
      NFSPROC3_LOOKUP => Some(self.lookup(args)),
      NFSPROC3_ACCESS => Some(self.access(args)),
      NFSPROC3_FSSTAT => Some(self.fsstat(args)),
      NFSPROC3_FSINFO => Some(self.fsinfo(args)),
      _ => None,
    }
  }

  /// NFSPROC3_GETATTR: the attributes of the object a handle names.
  pub fn getattr(&mut self, args: &mut XdrReader<'_>) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    match self.getattr_result(args) {
      Ok(attr) => {
        Nfsstat3::Ok.encode(&mut writer);
        attr.encode(&mut writer);
      }
      Err(status) => status.encode(&mut writer),
    }
    writer.into_bytes()
  }

  fn getattr_result(&mut self, args: &mut XdrReader<'_>) -> Result<Fattr3, Nfsstat3> {
    self.object_attr(args)
  }

  /// The `fattr3` of the object a leading file handle in `args` names.
  fn object_attr(&mut self, args: &mut XdrReader<'_>) -> Result<Fattr3, Nfsstat3> {
    let handle = Nfsfh3::decode(args).map_err(|_| Nfsstat3::Badhandle)?;
    let identity = self.resolve_handle(&handle)?;
    let node = self.attrs_of(&identity)?;
    Ok(self.fattr3(&node))
  }

  /// NFSPROC3_LOOKUP: resolve a name in a directory to its file handle and attributes.
  pub fn lookup(&mut self, args: &mut XdrReader<'_>) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    match self.lookup_result(args) {
      Ok((handle, object, directory)) => {
        Nfsstat3::Ok.encode(&mut writer);
        handle.encode(&mut writer);
        PostOpAttr(Some(object)).encode(&mut writer);
        PostOpAttr(directory).encode(&mut writer);
      }
      Err((status, directory)) => {
        status.encode(&mut writer);
        PostOpAttr(directory).encode(&mut writer);
      }
    }
    writer.into_bytes()
  }

  #[allow(clippy::type_complexity)]
  fn lookup_result(
    &mut self,
    args: &mut XdrReader<'_>,
  ) -> Result<(Nfsfh3, Fattr3, Option<Fattr3>), (Nfsstat3, Option<Fattr3>)> {
    // `diropargs3`: the directory handle then the name.
    let dir_handle = Nfsfh3::decode(args).map_err(|_| (Nfsstat3::Badhandle, None))?;
    let name = args
      .string(NFS_MAXNAMELEN)
      .map_err(|_| (Nfsstat3::Inval, None))?;
    let dir_identity = self
      .resolve_handle(&dir_handle)
      .map_err(|status| (status, None))?;
    // Validate the directory handle (existence and generation) before the lookup; its attributes
    // are the reply's directory context.
    let dir_node = self
      .attrs_of(&dir_identity)
      .map_err(|status| (status, None))?;
    let dir_attr = Some(self.fattr3(&dir_node));
    let child = self
      .bridge
      .lookup(dir_identity.inode, name)
      .map_err(|e| (nfsstat_of(&e), dir_attr))?;
    let handle = self.handle_for(child.ino, child.generation);
    let object = self.fattr3(&child);
    Ok((handle, object, dir_attr))
  }

  /// NFSPROC3_ACCESS: which requested operations the caller may perform. slates is not a sandbox
  /// (a non-goal) — it serves the filesystem and leaves process isolation to the harness — so it
  /// grants the access requested on an object the caller can already name; the reply carries the
  /// object's attributes so the client caches them.
  pub fn access(&mut self, args: &mut XdrReader<'_>) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    match self.access_result(args) {
      Ok((attr, granted)) => {
        Nfsstat3::Ok.encode(&mut writer);
        PostOpAttr(Some(attr)).encode(&mut writer);
        writer.u32(granted);
      }
      Err(status) => {
        status.encode(&mut writer);
        PostOpAttr(None).encode(&mut writer);
      }
    }
    writer.into_bytes()
  }

  fn access_result(&mut self, args: &mut XdrReader<'_>) -> Result<(Fattr3, u32), Nfsstat3> {
    let handle = Nfsfh3::decode(args).map_err(|_| Nfsstat3::Badhandle)?;
    let requested = args.u32().map_err(|_| Nfsstat3::Inval)?;
    let identity = self.resolve_handle(&handle)?;
    let node = self.attrs_of(&identity)?;
    Ok((self.fattr3(&node), granted_access(node.mode, requested)))
  }

  /// NFSPROC3_FSSTAT: the volume's dynamic statistics — space and file counts — from the shared
  /// seam's `statfs`, in the bytes and counts NFS reports.
  pub fn fsstat(&mut self, args: &mut XdrReader<'_>) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    match self.fsstat_result(args) {
      Ok((attr, stat)) => {
        Nfsstat3::Ok.encode(&mut writer);
        PostOpAttr(Some(attr)).encode(&mut writer);
        let block = u64::from(stat.bsize);
        writer.u64(stat.blocks.saturating_mul(block)); // tbytes
        writer.u64(stat.bfree.saturating_mul(block)); // fbytes
        writer.u64(stat.bavail.saturating_mul(block)); // abytes
        writer.u64(stat.files); // tfiles
        writer.u64(stat.ffree); // ffiles
        writer.u64(stat.ffree); // afiles
        writer.u32(0); // invarsec: statistics may change at any time
      }
      Err(status) => {
        status.encode(&mut writer);
        PostOpAttr(None).encode(&mut writer);
      }
    }
    writer.into_bytes()
  }

  fn fsstat_result(&mut self, args: &mut XdrReader<'_>) -> Result<(Fattr3, FsStat), Nfsstat3> {
    let handle = Nfsfh3::decode(args).map_err(|_| Nfsstat3::Badhandle)?;
    let identity = self.resolve_handle(&handle)?;
    let node = self.attrs_of(&identity)?;
    let stat = self
      .bridge
      .statfs(identity.inode)
      .map_err(|e| nfsstat_of(&e))?;
    Ok((self.fattr3(&node), stat))
  }

  /// NFSPROC3_FSINFO: the server's static limits and capabilities — transfer sizes, the maximum
  /// file size, the time granularity, and the supported features — that a client reads once at
  /// mount to size its I/O.
  pub fn fsinfo(&mut self, args: &mut XdrReader<'_>) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    match self.object_attr(args) {
      Ok(attr) => {
        Nfsstat3::Ok.encode(&mut writer);
        PostOpAttr(Some(attr)).encode(&mut writer);
        writer.u32(MAX_TRANSFER); // rtmax
        writer.u32(MAX_TRANSFER); // rtpref
        writer.u32(TRANSFER_MULTIPLE); // rtmult
        writer.u32(MAX_TRANSFER); // wtmax
        writer.u32(MAX_TRANSFER); // wtpref
        writer.u32(TRANSFER_MULTIPLE); // wtmult
        writer.u32(MAX_TRANSFER); // dtpref (readdir)
        writer.u64(u64::MAX); // maxfilesize
        Nfstime3 {
          seconds: 0,
          nseconds: 1,
        }
        .encode(&mut writer); // time_delta: one-nanosecond granularity
        writer.u32(FSF3_LINK | FSF3_SYMLINK | FSF3_HOMOGENEOUS | FSF3_CANSETTIME); // properties
      }
      Err(status) => {
        status.encode(&mut writer);
        PostOpAttr(None).encode(&mut writer);
      }
    }
    writer.into_bytes()
  }
}

/// The access bits granted for a `mode`, intersected with the `requested` bits. slates checks the
/// object's permissions instead of granting whatever is asked (audit-flagged); the per-principal
/// check — a caller other than the owner, the `AUTH_SYS` credentials, the §4.13 rights model — is
/// owed, so this reads the owner's permission bits, the common case since a slates volume is the
/// provisioning agent's own.
fn granted_access(mode: u32, requested: u32) -> u32 {
  let mut granted = 0;
  if mode & OWNER_READ != 0 {
    granted |= ACCESS3_READ;
  }
  if mode & OWNER_EXECUTE != 0 {
    granted |= ACCESS3_LOOKUP | ACCESS3_EXECUTE;
  }
  if mode & OWNER_WRITE != 0 {
    granted |= ACCESS3_MODIFY | ACCESS3_EXTEND | ACCESS3_DELETE;
  }
  granted & requested
}

/// The NFSv3 file type for a volume entry kind.
fn ftype3_of(kind: Kind) -> Ftype3 {
  match kind {
    Kind::File => Ftype3::Reg,
    Kind::Dir => Ftype3::Dir,
    Kind::Symlink => Ftype3::Lnk,
  }
}

/// An `nfstime3` from a nanosecond time, clamping a negative value to the epoch (the wire takes
/// unsigned seconds).
fn nfstime_of(ns: i64) -> Nfstime3 {
  /// Format: nanoseconds per second.
  const NS_PER_SEC: i64 = 1_000_000_000;
  let ns = ns.max(0);
  Nfstime3 {
    seconds: u32::try_from(ns / NS_PER_SEC).unwrap_or(u32::MAX),
    nseconds: u32::try_from(ns % NS_PER_SEC).unwrap_or(0),
  }
}

/// Maps a volume refusal to the NFSv3 status a client expects. This is the NFS edge's vocabulary,
/// the same neutral [`VfsError`] the FUSE edge maps to an errno.
fn nfsstat_of(e: &VfsError) -> Nfsstat3 {
  match e {
    VfsError::NotFound => Nfsstat3::Noent,
    VfsError::AlreadyExists => Nfsstat3::Exist,
    VfsError::NotDirectory => Nfsstat3::Notdir,
    VfsError::IsDirectory => Nfsstat3::Isdir,
    VfsError::NotEmpty => Nfsstat3::Notempty,
    VfsError::NoSpace => Nfsstat3::Nospc,
    VfsError::NotPermitted => Nfsstat3::Perm,
    VfsError::Invalid | VfsError::InvalidName => Nfsstat3::Inval,
    VfsError::StaleHandle => Nfsstat3::Stale,
    VfsError::BaseUnavailable(_) => Nfsstat3::Io,
    _ => Nfsstat3::ServerFault,
  }
}
