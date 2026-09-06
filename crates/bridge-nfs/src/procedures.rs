//! The NFSv3 and MOUNT procedures over the shared operation layer (§4.6, Phase 4 task 3): a parsed
//! RPC call is turned into a call on the transport-independent [`Bridge`] and its neutral result is
//! encoded into the procedure's XDR reply. This is the NFS analogue of the FUSE `dispatch`, so the
//! NFS server and the FUSE mount serve one volume core and the differential oracle can compare
//! their abstract states (§4.6 "the fallback is also the differential oracle").
//!
//! An NFSv3 file handle names its object by identity ([`crate::handle`]), so these procedures are
//! stateless in the NFS sense: they decode a handle to an [`ObjectId`], act, and mint handles for
//! the objects they return — no server-side open table. Every call rides the export's authenticated
//! [`OpContext`], built from the attachment the export edge admits at mount time for the enrolled
//! subject (§4.13): the seam checks the volume, rights and view, so a read against a read-only
//! export or a foreign volume is refused before any effect, exactly as at the FUSE edge. This slice
//! serves the metadata path a client walks first — MOUNT `MNT`, `NULL`, `GETATTR`, `LOOKUP`,
//! `ACCESS`, `FSSTAT`, `FSINFO` — and the file I/O path, `READ` and `WRITE`, over the shared
//! inode-addressed interface. The remaining namespace and directory procedures are owed.

use slates_bridge_core::{
  AttachmentId, Attachments, Bridge, FsStat, NodeAttr, ObjectId, OpContext, RenameFlags, Rights,
  SetAttr, View,
};
use slates_db::catalog::{Principal, VolumeId};
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
/// Format: NFSPROC3_SETATTR — set some of an object's attributes.
pub const NFSPROC3_SETATTR: u32 = 2;
/// Format: NFSPROC3_LOOKUP — resolve a name in a directory to a handle.
pub const NFSPROC3_LOOKUP: u32 = 3;
/// Format: NFSPROC3_ACCESS — which operations the caller may perform on an object.
pub const NFSPROC3_ACCESS: u32 = 4;
/// Format: NFSPROC3_READ — read data from a file.
pub const NFSPROC3_READ: u32 = 6;
/// Format: NFSPROC3_WRITE — write data to a file.
pub const NFSPROC3_WRITE: u32 = 7;
/// Format: NFSPROC3_REMOVE — remove a file (a directory entry) from a directory.
pub const NFSPROC3_REMOVE: u32 = 12;
/// Format: NFSPROC3_RMDIR — remove a directory from its parent.
pub const NFSPROC3_RMDIR: u32 = 13;
/// Format: NFSPROC3_RENAME — rename an entry from one directory to another.
pub const NFSPROC3_RENAME: u32 = 14;
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
/// Format: `FILE_SYNC` (`stable_how` = 2, RFC 1813 §3.3.7): the data and its metadata are committed
/// to stable storage before the reply. slates lands every write in the anchor segment synchronously
/// (there is no write-back buffer), so a WRITE is always `FILE_SYNC` and a later COMMIT is a no-op.
const FILE_SYNC: u32 = 2;
/// Format: `time_how` DONT_CHANGE (RFC 1813 §3.3.2): leave the time field unchanged.
const TIME_DONT_CHANGE: u32 = 0;
/// Format: `time_how` SET_TO_SERVER_TIME: set the time to the server's current wall clock.
const TIME_SET_TO_SERVER: u32 = 1;
/// Format: `time_how` SET_TO_CLIENT_TIME: set the time to the client-supplied `nfstime3`.
const TIME_SET_TO_CLIENT: u32 = 2;
/// Format: nanoseconds per second, for converting an `nfstime3` to the volume core's `i64` nanos.
const NS_PER_SEC: i64 = 1_000_000_000;

/// An NFSv3 export of one volume over the shared operation layer. Handles it mints and accepts name
/// objects of `volume`; a handle for another volume is refused stale. The export edge admits one
/// attachment at mount time for the enrolled subject (§4.13), and every procedure rides the
/// [`OpContext`] built from it, so the seam enforces the export's rights and view.
pub struct Export<'b> {
  bridge: &'b mut dyn Bridge,
  volume: VolumeId,
  /// The owner-side attachment registry for this export. A real daemon shares one registry across
  /// its exports; a single export holds its own, admitted at construction.
  attachments: Attachments,
  /// The attachment this export admitted for its mount. Every procedure builds its context from it.
  attachment: AttachmentId,
}

impl<'b> Export<'b> {
  /// An export of the volume `bridge` serves, identified by `volume` for the file handles, admitted
  /// for the enrolled `subject` with `rights` on the volume's current head. Building the attachment
  /// is the NFS analogue of the FUSE mount edge: the credentials are established once, at mount, and
  /// the seam checks every later request against the resulting context. Refuses (`NotPermitted` at
  /// the registry bound) if the attachment cannot be admitted.
  pub fn new(
    bridge: &'b mut dyn Bridge,
    volume: VolumeId,
    subject: Principal,
    rights: Rights,
  ) -> Result<Export<'b>, VfsError> {
    let mut attachments = Attachments::new();
    let attachment = attachments.attach(volume, View::Current, subject, rights)?;
    Ok(Export {
      bridge,
      volume,
      attachments,
      attachment,
    })
  }

  /// The authenticated context for a request, built from the export's attachment. Refuses when the
  /// attachment is revoked or fenced (a superseded owner epoch) — the export edge's "authority can
  /// no longer be established" case, which the caller maps to a `STALE`/`ACCES` NFS status.
  fn op_context(&self) -> Result<OpContext, VfsError> {
    self.attachments.context(self.attachment)
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
    let cx = self.op_context().map_err(|e| nfsstat_of(&e))?;
    let object = ObjectId::new(identity.inode, identity.generation);
    let node = match self.bridge.getattr(object, &cx) {
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
    let Ok(cx) = self.op_context() else {
      return MountReply::Err(Mountstat3::ServerFault);
    };
    match self.bridge.root(&cx) {
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
      NFSPROC3_SETATTR => Some(self.setattr(args)),
      NFSPROC3_LOOKUP => Some(self.lookup(args)),
      NFSPROC3_ACCESS => Some(self.access(args)),
      NFSPROC3_READ => Some(self.read(args)),
      NFSPROC3_WRITE => Some(self.write(args)),
      NFSPROC3_REMOVE => Some(self.remove(args, false)),
      NFSPROC3_RMDIR => Some(self.remove(args, true)),
      NFSPROC3_RENAME => Some(self.rename(args)),
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
    let cx = self.op_context().map_err(|e| (nfsstat_of(&e), dir_attr))?;
    let parent = ObjectId::new(dir_identity.inode, dir_identity.generation);
    let child = self
      .bridge
      .lookup(parent, &cx, name)
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

  /// NFSPROC3_READ: read up to `count` bytes at `offset` from the file a handle names, over the
  /// shared inode-addressed interface under the export's context. The reply carries the file's
  /// post-read attributes, the byte count, the end-of-file flag, and the data.
  pub fn read(&mut self, args: &mut XdrReader<'_>) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    match self.read_result(args) {
      Ok((post, count, eof, data)) => {
        Nfsstat3::Ok.encode(&mut writer);
        PostOpAttr(Some(post)).encode(&mut writer);
        writer.u32(count);
        writer.bool(eof);
        writer.opaque(&data);
      }
      Err((status, post)) => {
        status.encode(&mut writer);
        PostOpAttr(post).encode(&mut writer);
      }
    }
    writer.into_bytes()
  }

  #[allow(clippy::type_complexity)]
  fn read_result(
    &mut self,
    args: &mut XdrReader<'_>,
  ) -> Result<(Fattr3, u32, bool, Vec<u8>), (Nfsstat3, Option<Fattr3>)> {
    // READ3args: the file handle, the offset, the byte count.
    let handle = Nfsfh3::decode(args).map_err(|_| (Nfsstat3::Badhandle, None))?;
    let offset = args.u64().map_err(|_| (Nfsstat3::Inval, None))?;
    let count = args.u32().map_err(|_| (Nfsstat3::Inval, None))?;
    let identity = self.resolve_handle(&handle).map_err(|s| (s, None))?;
    // The post-op attributes double as the object's validation (existence and generation) and carry
    // the size the end-of-file flag is computed against.
    let node = self.attrs_of(&identity).map_err(|s| (s, None))?;
    let post = self.fattr3(&node);
    let cx = self
      .op_context()
      .map_err(|e| (nfsstat_of(&e), Some(post)))?;
    let object = ObjectId::new(identity.inode, identity.generation);
    let want = count.min(MAX_TRANSFER);
    let mut data = Vec::new();
    self
      .bridge
      .read(object, &cx, offset, want, &mut data)
      .map_err(|e| (nfsstat_of(&e), Some(post)))?;
    let end = offset.saturating_add(u64::try_from(data.len()).unwrap_or(u64::MAX));
    let eof = end >= node.size;
    let read = u32::try_from(data.len()).unwrap_or(u32::MAX);
    Ok((post, read, eof, data))
  }

  /// NFSPROC3_WRITE: write the request's data at `offset` to the file a handle names, over the
  /// shared interface under the export's context. slates lands every write in the anchor
  /// synchronously, so the reply is always `FILE_SYNC`; a write against a read-only export or a
  /// pinned view is refused by the seam before any effect.
  pub fn write(&mut self, args: &mut XdrReader<'_>) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    match self.write_result(args) {
      Ok((post, count)) => {
        Nfsstat3::Ok.encode(&mut writer);
        encode_wcc(&mut writer, Some(post));
        writer.u32(count);
        writer.u32(FILE_SYNC);
        writer.fixed(&self.write_verifier());
      }
      Err((status, post)) => {
        status.encode(&mut writer);
        encode_wcc(&mut writer, post);
      }
    }
    writer.into_bytes()
  }

  fn write_result(
    &mut self,
    args: &mut XdrReader<'_>,
  ) -> Result<(Fattr3, u32), (Nfsstat3, Option<Fattr3>)> {
    // WRITE3args: the file handle, the offset, the byte count, the requested stability, the data.
    // The count and stability are advisory here — the data length is authoritative and slates
    // always commits FILE_SYNC — but they are decoded so a malformed request is a typed refusal,
    // and the data length is capped at the offered transfer size so a hostile length is refused
    // before allocating.
    let handle = Nfsfh3::decode(args).map_err(|_| (Nfsstat3::Badhandle, None))?;
    let offset = args.u64().map_err(|_| (Nfsstat3::Inval, None))?;
    let _count = args.u32().map_err(|_| (Nfsstat3::Inval, None))?;
    let _stable = args.u32().map_err(|_| (Nfsstat3::Inval, None))?;
    let data = args
      .opaque(usize::try_from(MAX_TRANSFER).unwrap_or(0))
      .map_err(|_| (Nfsstat3::Inval, None))?
      .to_vec();
    let identity = self.resolve_handle(&handle).map_err(|s| (s, None))?;
    let cx = self.op_context().map_err(|e| (nfsstat_of(&e), None))?;
    let object = ObjectId::new(identity.inode, identity.generation);
    let written = self
      .bridge
      .write(object, &cx, offset, &data)
      .map_err(|e| (nfsstat_of(&e), None))?;
    // The post-op attributes reflect the file after the write (the wcc's post half).
    let node = self.attrs_of(&identity).map_err(|s| (s, None))?;
    Ok((self.fattr3(&node), written))
  }

  /// The write verifier the export returns (RFC 1813 `writeverf3`): eight bytes a client compares
  /// across a server restart to decide whether to resend unstable writes. slates derives it from
  /// the volume id, stable for the life of the volume; a boot-id-based verifier that also changes
  /// on a daemon restart is owed with the §4.8 recovery wiring. Since every slates write is already
  /// `FILE_SYNC`, no client resend depends on this today.
  fn write_verifier(&self) -> [u8; size_of::<u64>()] {
    self.fsid().to_be_bytes()
  }

  /// NFSPROC3_REMOVE / NFSPROC3_RMDIR: remove a name from a directory over the shared interface
  /// under the export's context (`is_dir` selects `rmdir` for a directory, `unlink` otherwise). The
  /// reply is the directory's `wcc_data` — its post-operation attributes, so the client refreshes
  /// its cached link count without a follow-up GETATTR.
  pub fn remove(&mut self, args: &mut XdrReader<'_>, is_dir: bool) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    match self.remove_result(args, is_dir) {
      Ok(dir_post) => {
        Nfsstat3::Ok.encode(&mut writer);
        encode_wcc(&mut writer, Some(dir_post));
      }
      Err((status, dir_post)) => {
        status.encode(&mut writer);
        encode_wcc(&mut writer, dir_post);
      }
    }
    writer.into_bytes()
  }

  fn remove_result(
    &mut self,
    args: &mut XdrReader<'_>,
    is_dir: bool,
  ) -> Result<Fattr3, (Nfsstat3, Option<Fattr3>)> {
    // `diropargs3`: the directory handle then the name.
    let dir_handle = Nfsfh3::decode(args).map_err(|_| (Nfsstat3::Badhandle, None))?;
    let name = args
      .string(NFS_MAXNAMELEN)
      .map_err(|_| (Nfsstat3::Inval, None))?
      .to_owned();
    let dir_identity = self
      .resolve_handle(&dir_handle)
      .map_err(|status| (status, None))?;
    let cx = self.op_context().map_err(|e| (nfsstat_of(&e), None))?;
    let parent = ObjectId::new(dir_identity.inode, dir_identity.generation);
    let outcome = if is_dir {
      self.bridge.rmdir(parent, &cx, &name)
    } else {
      self.bridge.unlink(parent, &cx, &name)
    };
    // The directory's post-op attributes (its new link count) go in the wcc whether the remove
    // succeeded or failed.
    let dir_post = self.attrs_of(&dir_identity).ok().map(|n| self.fattr3(&n));
    match outcome {
      Ok(()) => dir_post.ok_or((Nfsstat3::ServerFault, None)),
      Err(e) => Err((nfsstat_of(&e), dir_post)),
    }
  }

  /// NFSPROC3_RENAME: move an entry from one directory to another over the shared interface under
  /// the export's context. NFSv3 RENAME carries no `renameat2` flags — it replaces an existing
  /// destination — so the neutral call uses the default flags. The reply is the source and
  /// destination directories' `wcc_data`.
  pub fn rename(&mut self, args: &mut XdrReader<'_>) -> Vec<u8> {
    let (status, from_post, to_post) = self.do_rename(args);
    let mut writer = XdrWriter::new();
    status.encode(&mut writer);
    encode_wcc(&mut writer, from_post); // fromdir_wcc
    encode_wcc(&mut writer, to_post); // todir_wcc
    writer.into_bytes()
  }

  /// Decodes the two `diropargs3` and performs the rename, returning the reply status and both
  /// directories' post-op attributes for the `wcc_data`. The attributes are carried on the failure
  /// path too (RFC 1813 answers RENAME with both dirs' wcc regardless), `None` only when a
  /// directory handle itself does not resolve. A tuple, not a `Result`, so the two-attribute
  /// payload is never a large `Err` variant.
  fn do_rename(&mut self, args: &mut XdrReader<'_>) -> (Nfsstat3, Option<Fattr3>, Option<Fattr3>) {
    // Two `diropargs3`: the source directory and name, then the destination directory and name.
    // The names are owned because a second handle is decoded from `args` between them.
    let from_dir_fh = match Nfsfh3::decode(args) {
      Ok(fh) => fh,
      Err(_) => return (Nfsstat3::Badhandle, None, None),
    };
    let from_name = match args.string(NFS_MAXNAMELEN) {
      Ok(name) => name.to_owned(),
      Err(_) => return (Nfsstat3::Inval, None, None),
    };
    let to_dir_fh = match Nfsfh3::decode(args) {
      Ok(fh) => fh,
      Err(_) => return (Nfsstat3::Badhandle, None, None),
    };
    let to_name = match args.string(NFS_MAXNAMELEN) {
      Ok(name) => name.to_owned(),
      Err(_) => return (Nfsstat3::Inval, None, None),
    };
    let from_identity = match self.resolve_handle(&from_dir_fh) {
      Ok(id) => id,
      Err(status) => return (status, None, None),
    };
    let to_identity = match self.resolve_handle(&to_dir_fh) {
      Ok(id) => id,
      Err(status) => return (status, None, None),
    };
    let cx = match self.op_context() {
      Ok(cx) => cx,
      Err(e) => return (nfsstat_of(&e), None, None),
    };
    let from_parent = ObjectId::new(from_identity.inode, from_identity.generation);
    let to_parent = ObjectId::new(to_identity.inode, to_identity.generation);
    let outcome = self.bridge.rename(
      from_parent,
      to_parent,
      &cx,
      &from_name,
      &to_name,
      RenameFlags::default(),
    );
    let from_post = self.attrs_of(&from_identity).ok().map(|n| self.fattr3(&n));
    let to_post = self.attrs_of(&to_identity).ok().map(|n| self.fattr3(&n));
    let status = match outcome {
      Ok(()) => Nfsstat3::Ok,
      Err(e) => nfsstat_of(&e),
    };
    (status, from_post, to_post)
  }

  /// NFSPROC3_SETATTR: set some of an object's attributes over the shared interface under the
  /// export's context. The `sattr3` union chooses which fields to set (§4.6; the seam applies each
  /// requested field and never acknowledges one it ignored, BUG-8); a `SET_TO_SERVER_TIME` time is
  /// resolved to the volume's wall clock here (AC-3.10). An optional guard (`sattr_guard3`) makes
  /// the update conditional on the object's `ctime` — a compare-and-set the client uses to avoid a
  /// lost update — refused `NFS3ERR_NOT_SYNC` when the guard does not match.
  pub fn setattr(&mut self, args: &mut XdrReader<'_>) -> Vec<u8> {
    let (status, post) = self.do_setattr(args);
    let mut writer = XdrWriter::new();
    status.encode(&mut writer);
    encode_wcc(&mut writer, post);
    writer.into_bytes()
  }

  fn do_setattr(&mut self, args: &mut XdrReader<'_>) -> (Nfsstat3, Option<Fattr3>) {
    // SETATTR3args: the object handle, the new attributes (sattr3), then the guard (sattr_guard3).
    let handle = match Nfsfh3::decode(args) {
      Ok(fh) => fh,
      Err(_) => return (Nfsstat3::Badhandle, None),
    };
    let changes = match self.decode_sattr3(args) {
      Ok(changes) => changes,
      Err(status) => return (status, None),
    };
    // sattr_guard3: a bool, then (if set) the ctime the object must currently have.
    let guarded = match args.bool() {
      Ok(b) => b,
      Err(_) => return (Nfsstat3::Inval, None),
    };
    let guard_ctime = if guarded {
      match Nfstime3::decode(args) {
        Ok(t) => Some(nfstime_to_ns(&t)),
        Err(_) => return (Nfsstat3::Inval, None),
      }
    } else {
      None
    };
    let identity = match self.resolve_handle(&handle) {
      Ok(id) => id,
      Err(status) => return (status, None),
    };
    let node = match self.attrs_of(&identity) {
      Ok(node) => node,
      Err(status) => return (status, None),
    };
    // The guard is a compare-and-set on the object's change time (a stale cache is refused).
    if let Some(guard) = guard_ctime
      && guard != node.ctime
    {
      return (Nfsstat3::NotSync, Some(self.fattr3(&node)));
    }
    let cx = match self.op_context() {
      Ok(cx) => cx,
      Err(e) => return (nfsstat_of(&e), Some(self.fattr3(&node))),
    };
    let object = ObjectId::new(identity.inode, identity.generation);
    match self.bridge.setattr(object, &cx, changes) {
      Ok(updated) => (Nfsstat3::Ok, Some(self.fattr3(&updated))),
      Err(e) => {
        let post = self.attrs_of(&identity).ok().map(|n| self.fattr3(&n));
        (nfsstat_of(&e), post)
      }
    }
  }

  /// Decodes an `sattr3` (RFC 1813 §3.3.2) into the neutral [`SetAttr`]: each optional field becomes
  /// `Some` only when the caller asks to set it, and a `SET_TO_SERVER_TIME` time is resolved to the
  /// volume's wall clock now (AC-3.10), so the seam receives explicit values and never has to guess.
  fn decode_sattr3(&mut self, args: &mut XdrReader<'_>) -> Result<SetAttr, Nfsstat3> {
    let mode = decode_optional_u32(args)?;
    let uid = decode_optional_u32(args)?;
    let gid = decode_optional_u32(args)?;
    let size = if args.bool().map_err(|_| Nfsstat3::Inval)? {
      Some(args.u64().map_err(|_| Nfsstat3::Inval)?)
    } else {
      None
    };
    let atime_how = args.u32().map_err(|_| Nfsstat3::Inval)?;
    let atime_client = if atime_how == TIME_SET_TO_CLIENT {
      Some(nfstime_to_ns(
        &Nfstime3::decode(args).map_err(|_| Nfsstat3::Inval)?,
      ))
    } else {
      None
    };
    let mtime_how = args.u32().map_err(|_| Nfsstat3::Inval)?;
    let mtime_client = if mtime_how == TIME_SET_TO_CLIENT {
      Some(nfstime_to_ns(
        &Nfstime3::decode(args).map_err(|_| Nfsstat3::Inval)?,
      ))
    } else {
      None
    };
    // Resolve any SET_TO_SERVER_TIME to the wall clock once (AC-3.10).
    let server_now = if atime_how == TIME_SET_TO_SERVER || mtime_how == TIME_SET_TO_SERVER {
      Some(self.bridge.now())
    } else {
      None
    };
    Ok(SetAttr {
      size,
      mode,
      uid,
      gid,
      atime: resolve_set_time(atime_how, atime_client, server_now)?,
      mtime: resolve_set_time(mtime_how, mtime_client, server_now)?,
    })
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
    let cx = self.op_context().map_err(|e| nfsstat_of(&e))?;
    let object = ObjectId::new(identity.inode, identity.generation);
    let stat = self
      .bridge
      .statfs(object, &cx)
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

/// Encodes an NFSv3 `wcc_data`: the pre-operation attributes (slates keeps none, so absent) then
/// the post-operation attributes. A mutating reply (WRITE) carries it so the client updates its
/// cache without a follow-up GETATTR; the absent pre-op half means the client cannot detect a
/// racing outside change, which slates has none of on a head it owns (§4.6 cache posture).
fn encode_wcc(writer: &mut XdrWriter, post: Option<Fattr3>) {
  writer.bool(false);
  PostOpAttr(post).encode(writer);
}

/// Decodes an `sattr3` optional `u32` (`set_mode3`/`set_uid3`/`set_gid3`): a bool, then the value
/// when it is set, `None` otherwise.
fn decode_optional_u32(args: &mut XdrReader<'_>) -> Result<Option<u32>, Nfsstat3> {
  if args.bool().map_err(|_| Nfsstat3::Inval)? {
    Ok(Some(args.u32().map_err(|_| Nfsstat3::Inval)?))
  } else {
    Ok(None)
  }
}

/// Resolves an `sattr3` time field to the neutral optional nanosecond value the seam takes:
/// `DONT_CHANGE` is `None`, `SET_TO_CLIENT_TIME` is the client's value, `SET_TO_SERVER_TIME` is the
/// resolved wall-clock `now`. An unknown `time_how` is a typed refusal. The `client` and
/// `server_now` inputs are `Some` exactly when the matching `how` was decoded, so the mapping is
/// total.
fn resolve_set_time(
  how: u32,
  client: Option<i64>,
  server_now: Option<i64>,
) -> Result<Option<i64>, Nfsstat3> {
  match how {
    TIME_DONT_CHANGE => Ok(None),
    TIME_SET_TO_CLIENT => Ok(client),
    TIME_SET_TO_SERVER => Ok(server_now),
    _ => Err(Nfsstat3::Inval),
  }
}

/// An `nfstime3` (seconds and nanoseconds) as the volume core's `i64` nanoseconds since the epoch.
fn nfstime_to_ns(t: &Nfstime3) -> i64 {
  i64::from(t.seconds)
    .saturating_mul(NS_PER_SEC)
    .saturating_add(i64::from(t.nseconds))
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
