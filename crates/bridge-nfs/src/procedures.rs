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
//! export or a foreign volume is refused before any effect, exactly as at the FUSE edge. The
//! served set is the metadata path (MOUNT `MNT`, `NULL`, `GETATTR`, `SETATTR`, `LOOKUP`, `ACCESS`,
//! `FSSTAT`, `FSINFO`), the file I/O path (`READ`, `WRITE`), and the namespace (`CREATE`, `MKDIR`,
//! `SYMLINK`, `READLINK`, `REMOVE`, `RMDIR`, `RENAME`, `LINK`) and the listings (`READDIR`,
//! `READDIRPLUS`) — all over the shared inode-addressed interface.
//!
//! **Access control.** Every procedure that reads, changes or names an object first applies the POSIX
//! permission rules to the request's caller ([`crate::access`]: the uid the `AUTH_SYS` credential
//! names and its groups): search on a directory to resolve a name, write and search on a directory to
//! add, remove or rename an entry (the sticky bit deciding who may remove what), read or write on a
//! file for its bytes, and the ownership rules for `SETATTR`. A refusal is typed (`NFS3ERR_ACCES` for a
//! missing permission bit, `NFS3ERR_PERM` for an ownership rule) before any effect, and `ACCESS`
//! reports the same verdict the procedures apply, so the client's own `open(2)` checks agree with the
//! server. This is what `default_permissions` gives the FUSE mount from the kernel; an NFS server must
//! do it itself (`docs/wip/EQUIVALENCE.md` §8).

use slates_bridge_core::{
  AttachmentId, Attachments, Bridge, FsStat, NodeAttr, ObjectId, OpContext, RenameFlags, Rights,
  SetAttr, View,
};
use slates_db::catalog::{Principal, VolumeId};
use slates_vfs::error::VfsError;
use slates_vfs::inode::Kind;

use crate::access::{self, Caller, Denial, UnixGroups, Want};
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
/// Format: NFSPROC3_CREATE — create a regular file.
pub const NFSPROC3_CREATE: u32 = 8;
/// Format: NFSPROC3_MKDIR — create a directory.
pub const NFSPROC3_MKDIR: u32 = 9;
/// Format: NFSPROC3_SYMLINK — create a symbolic link.
pub const NFSPROC3_SYMLINK: u32 = 10;
/// Format: NFSPROC3_MKNOD (RFC 1813 procedure 11) — create a special device, FIFO or socket node.
/// slates is a RAM copy-on-write filesystem for regular files, directories and links; it does not
/// create special nodes, so this is refused `NFS3ERR_NOTSUPP` (a typed refusal, not `PROC_UNAVAIL`).
pub const NFSPROC3_MKNOD: u32 = 11;
/// Format: NFSPROC3_LOOKUP — resolve a name in a directory to a handle.
pub const NFSPROC3_LOOKUP: u32 = 3;
/// Format: NFSPROC3_ACCESS — which operations the caller may perform on an object.
pub const NFSPROC3_ACCESS: u32 = 4;
/// Format: NFSPROC3_READLINK — read the target path of a symbolic link.
pub const NFSPROC3_READLINK: u32 = 5;
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
/// Format: NFSPROC3_LINK (RFC 1813 procedure 15) — create a hard link, a second name in a directory
/// for an existing non-directory object.
pub const NFSPROC3_LINK: u32 = 15;
/// Format: NFSPROC3_READDIR — list a directory's entries (names and ids).
pub const NFSPROC3_READDIR: u32 = 16;
/// Format: NFSPROC3_READDIRPLUS — list a directory's entries with each one's attributes and handle.
pub const NFSPROC3_READDIRPLUS: u32 = 17;
/// Format: NFSPROC3_FSSTAT — dynamic filesystem statistics (space and file counts).
pub const NFSPROC3_FSSTAT: u32 = 18;
/// Format: NFSPROC3_FSINFO — static filesystem limits and capabilities.
pub const NFSPROC3_FSINFO: u32 = 19;
/// Format: NFSPROC3_COMMIT (RFC 1813 procedure 21) — flush a file's writes to stable storage. Every
/// slates write already lands `FILE_SYNC`, so a commit is a no-op that confirms the file and returns
/// the write verifier; a client `fsync` maps to it.
pub const NFSPROC3_COMMIT: u32 = 21;
/// Format: NFSPROC3_PATHCONF (RFC 1813 procedure 20) — the POSIX pathconf limits of the filesystem an
/// object lives in (name and link maxima, truncation, chown restriction, case behaviour).
pub const NFSPROC3_PATHCONF: u32 = 20;
/// Format: the maximum bytes in a filename slates resolves (§4.5's name cap), refused before
/// allocating.
pub const NFS_MAXNAMELEN: usize = 255;
/// Format: the maximum bytes in a symlink target the server accepts — POSIX `PATH_MAX` (4096). RFC
/// 1813 leaves `nfspath3` unbounded, so the server caps it and refuses a longer target before
/// allocating; the volume core does not yet enforce its own symlink-target cap (owed).
const NFS_MAXPATHLEN: usize = 4096;
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
/// Derived: the maximum hard links PATHCONF reports (RFC 1813 `linkmax`). slates stores an object's
/// link count as a `u32` and imposes no tighter cap, so the maximum is the counter's range, `u32::MAX`.
/// Shared with the synthetic root's PATHCONF ([`crate::multi`]) so the mount reports one uniform value.
pub const PATHCONF_LINKMAX: u32 = u32::MAX;
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
/// Format: `UNSTABLE` (`stable_how` = 0, RFC 1813 §3.3.7): the server may reply before the data is
/// stable; the client keeps its copy until a COMMIT (or a later stable WRITE) answers with the same
/// write verifier, and re-sends it when the verifier changed (the server restarted in between).
/// An `UNSTABLE` write is answered `UNSTABLE` — the bytes are in the volume, not yet in the
/// recovery image — and the client's COMMIT is the barrier that publishes them.
const UNSTABLE: u32 = 0;
/// Format: `FILE_SYNC` (`stable_how` = 2, RFC 1813 §3.3.7): the data and its metadata are stable
/// before the reply. slates makes a write stable by publishing the shard's recovery image into
/// anchor-owned RAM (§4.8, D-18: daemon-restart survival); the host that runs this export does that
/// at its barrier after the write and before it sends the reply, and answers a refused barrier with
/// `NFS3ERR_IO` ([`io_failure_reply`]) rather than claim a stability the bytes do not have. A
/// `DATA_SYNC` request (`stable_how` = 1) is answered `FILE_SYNC` too: the barrier publishes data
/// and metadata together, a stronger level than asked, which the protocol allows.
const FILE_SYNC: u32 = 2;
/// Format: `time_how` DONT_CHANGE (RFC 1813 §3.3.2): leave the time field unchanged.
const TIME_DONT_CHANGE: u32 = 0;
/// Format: `time_how` SET_TO_SERVER_TIME: set the time to the server's current wall clock.
const TIME_SET_TO_SERVER: u32 = 1;
/// Format: `time_how` SET_TO_CLIENT_TIME: set the time to the client-supplied `nfstime3`.
const TIME_SET_TO_CLIENT: u32 = 2;
/// Format: nanoseconds per second, for converting an `nfstime3` to the volume core's `i64` nanos.
const NS_PER_SEC: i64 = 1_000_000_000;
/// Format: `createmode3` UNCHECKED (RFC 1813 §3.3.8): create the file, or succeed on an existing
/// one (an `open` with `O_CREAT` and no `O_EXCL`).
const CREATE_UNCHECKED: u32 = 0;
/// Format: `createmode3` GUARDED: create the file, or fail `NFS3ERR_EXIST` if the name exists.
const CREATE_GUARDED: u32 = 1;
/// Format: `createmode3` EXCLUSIVE: an idempotent create keyed by an 8-byte verifier. slates keeps
/// no create-verifier table yet, so it is refused `NFS3ERR_NOTSUPP` (owed); a client retries GUARDED.
const CREATE_EXCLUSIVE: u32 = 2;
/// Format: the mode a CREATE falls back to when the client's `sattr3` omits one — a regular file,
/// `rw-r--r--`. A client sets the mode in practice, so this is only a defensive default.
const DEFAULT_FILE_MODE: u32 = 0o644;
/// Format: the mode a MKDIR falls back to when the client's `sattr3` omits one — `rwxr-xr-x`.
const DEFAULT_DIR_MODE: u32 = 0o755;
/// Format: the fixed XDR wire size of a `fattr3` (RFC 1813 §2.3.5): five `u32` (type, mode, nlink,
/// uid, gid) = 20, two `size3` (size, used) = 16, one `specdata3` (rdev) = 8, `fsid` + `fileid` = 16,
/// three `nfstime3` (atime, mtime, ctime) = 24 — 84 bytes. Used to budget a READDIR reply.
const FATTR3_BYTES: usize = 84;
/// Format: the fixed XDR bytes of a READDIR `entry3` besides its name — the value-follows bool (4),
/// the `fileid` (8) and the `cookie` (8) — for budgeting a reply against the client's `count`.
const READDIR_ENTRY_FIXED: usize = 4 + size_of::<u64>() + size_of::<u64>();
/// Format: the fixed XDR bytes of a READDIR reply besides its entries — the status (4), a present
/// `post_op_attr` (its bool plus a `fattr3`), the `cookieverf` (8), and the trailing end-of-list
/// and `eof` bools (8) — reserved from the client's `count` so the reply stays within it.
const READDIR_REPLY_OVERHEAD: usize = 4 + 4 + FATTR3_BYTES + size_of::<u64>() + 4 + 4;
/// Format: the fixed XDR bytes a READDIRPLUS `entryplus3` adds over a READDIR `entry3` besides the
/// variable handle — a present `name_attributes` (its bool plus a `fattr3`) and the `name_handle`
/// present bool — for budgeting a reply against the client's `maxcount`.
const PLUS_ENTRY_FIXED: usize = 4 + FATTR3_BYTES + 4;

/// One entry of a READDIR or READDIRPLUS reply, gathered before encoding so the reply can be
/// budgeted against the client's `count`/`maxcount`: the child's inode number (the `fileid`), its
/// name, and the resume `cookie` the next call passes to continue after it. For a READDIRPLUS
/// listing, `attr` and `handle` carry the child's attributes and file handle (the handle is always
/// derivable; the attributes are best-effort); both are `None` for a plain READDIR.
struct ReaddirEntry {
  fileid: u64,
  name: String,
  cookie: u64,
  attr: Option<Fattr3>,
  handle: Option<Nfsfh3>,
}

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
  /// The caller every request runs as, for the POSIX permission rules ([`crate::access`]): the uid of
  /// the enrolled subject, and the groups the `AUTH_SYS` credential named — the primary group also
  /// overlaid onto every request's [`OpContext`] so a created object takes it. The groups are `None`
  /// when the credential named none (`AUTH_NONE`, a caller in no group whose created objects inherit
  /// the parent's group), set after construction ([`Self::set_groups`]) so `new`'s many call sites
  /// stay unchanged — the groups are not part of the authenticated identity, only of its permissions.
  caller: Caller,
  /// The write verifier (RFC 1813 `writeverf3`) every WRITE and COMMIT reply carries: eight bytes a
  /// client compares across calls to learn whether the server lost its unstable writes in between
  /// (a restart), in which case it re-sends them. The host that can restart sets it per boot
  /// ([`Self::set_write_verifier`]); a standalone export, which has no restart to survive, keeps the
  /// volume-derived default.
  write_verifier: [u8; size_of::<u64>()],
}

impl<'b> Export<'b> {
  /// An export of the volume `bridge` serves, identified by `volume` for the file handles, admitted
  /// for the enrolled `subject` with `rights` on the volume's current head. Building the attachment
  /// is the NFS analogue of the FUSE mount edge: the credentials are established once, at mount, and
  /// the seam checks every later request against the resulting context. Refuses (`NotPermitted` at
  /// the registry bound) if the attachment cannot be admitted. The mounting user's group is set
  /// separately with [`Self::set_owner_gid`] (it defaults to none — a parent-inherited group).
  pub fn new(
    bridge: &'b mut dyn Bridge,
    volume: VolumeId,
    subject: Principal,
    rights: Rights,
  ) -> Result<Export<'b>, VfsError> {
    // The caller's uid is the enrolled subject's; a subject that is not a Unix user owns nothing and
    // is judged by the other class of every object.
    let caller_uid = match &subject {
      Principal::Uid { uid } => *uid,
      _ => access::INVALID_UID,
    };
    let mut attachments = Attachments::new();
    let attachment = attachments.attach(volume, View::Current, subject, rights)?;
    let mut fsid = [0u8; size_of::<u64>()];
    fsid.copy_from_slice(&volume.bytes[..size_of::<u64>()]);
    Ok(Export {
      bridge,
      volume,
      attachments,
      attachment,
      caller: Caller {
        uid: caller_uid,
        groups: None,
      },
      write_verifier: fsid,
    })
  }

  /// Sets the caller's groups (from the call's `AUTH_SYS` credential): the primary group a created
  /// object takes (overlaid onto every request's context), and with the supplementary groups the group
  /// class of every permission check. The daemon's export path calls this per request; a mount with no
  /// such credential leaves it `None` — a parent-inherited group, a caller in no group.
  pub fn set_groups(&mut self, groups: Option<UnixGroups>) {
    self.caller.groups = groups;
  }

  /// Sets the write verifier (RFC 1813 `writeverf3`) this export answers WRITE and COMMIT with: the
  /// host's per-boot value, unique to the running instance, so a client that holds unstable writes
  /// from before a daemon restart sees it change and re-sends them (§3.3.7: "unique between
  /// instances of the NFS version 3 protocol server, where uncommitted data may be lost").
  pub fn set_write_verifier(&mut self, verifier: [u8; size_of::<u64>()]) {
    self.write_verifier = verifier;
  }

  /// The authenticated context for a request, built from the export's attachment. Refuses when the
  /// attachment is revoked or fenced (a superseded owner epoch) — the export edge's "authority can
  /// no longer be established" case, which the caller maps to a `STALE`/`ACCES` NFS status.
  fn op_context(&self) -> Result<OpContext, VfsError> {
    let mut context = self.attachments.context(self.attachment)?;
    // Overlay the caller's primary group onto the authenticated context: the attachment registry
    // carries only the authenticated identity (uid-only), and the group is file ownership the export
    // edge supplies.
    context.owner_gid = self.caller.groups.as_ref().map(|groups| groups.gid);
    Ok(context)
  }

  /// The attributes of the directory a namespace change (a create, remove, rename or link) names, once
  /// the caller is allowed to change it — search and write permission (POSIX); `Acces` otherwise, the
  /// directory's attributes carried either way for the reply's `wcc_data`.
  fn writable_directory(
    &mut self,
    identity: &FileHandle,
  ) -> Result<NodeAttr, (Nfsstat3, Option<Fattr3>)> {
    let node = self.attrs_of(identity).map_err(|status| (status, None))?;
    let attr = Some(self.fattr3(&node));
    if !access::permits(&self.caller, &node, Want::Search)
      || !access::permits(&self.caller, &node, Want::Write)
    {
      return Err((Nfsstat3::Acces, attr));
    }
    Ok(node)
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

  /// This export's volume root as a file handle and its attributes — what a multi-volume root
  /// ([`crate::multi::MultiExport`]) answers `LOOKUP` and `READDIRPLUS` of this volume's name with, so
  /// the handle and attributes a client gets by browsing into the volume match a direct mount of it.
  /// `None` if the root cannot be established (a revoked or fenced attachment).
  pub fn root_object(&mut self) -> Option<(Nfsfh3, Fattr3)> {
    let cx = self.op_context().ok()?;
    let root = self.bridge.root(&cx).ok()?;
    let identity = FileHandle {
      volume: self.volume,
      inode: root,
      generation: 0,
    };
    let node = self.attrs_of(&identity).ok()?;
    Some((self.handle_for(root, 0), self.fattr3(&node)))
  }

  /// Dispatches one NFSv3 procedure, returning the accepted reply's result bytes, or `None` for a
  /// procedure this slice does not serve (the caller answers `PROC_UNAVAIL`).
  pub fn serve_nfs(&mut self, procedure: u32, args: &mut XdrReader<'_>) -> Option<Vec<u8>> {
    match procedure {
      NFSPROC3_NULL => Some(Vec::new()),
      NFSPROC3_GETATTR => Some(self.getattr(args)),
      NFSPROC3_SETATTR => Some(self.setattr(args)),
      NFSPROC3_LOOKUP => Some(self.lookup(args)),
      NFSPROC3_READLINK => Some(self.readlink(args)),
      NFSPROC3_CREATE => Some(self.create(args)),
      NFSPROC3_MKDIR => Some(self.mkdir(args)),
      NFSPROC3_SYMLINK => Some(self.symlink(args)),
      NFSPROC3_MKNOD => Some(self.mknod_unsupported(args)),
      NFSPROC3_ACCESS => Some(self.access(args)),
      NFSPROC3_READ => Some(self.read(args)),
      NFSPROC3_WRITE => Some(self.write(args)),
      NFSPROC3_REMOVE => Some(self.remove(args, false)),
      NFSPROC3_RMDIR => Some(self.remove(args, true)),
      NFSPROC3_RENAME => Some(self.rename(args)),
      NFSPROC3_LINK => Some(self.link(args)),
      NFSPROC3_READDIR => Some(self.readdir(args)),
      NFSPROC3_READDIRPLUS => Some(self.readdirplus(args)),
      NFSPROC3_FSSTAT => Some(self.fsstat(args)),
      NFSPROC3_FSINFO => Some(self.fsinfo(args)),
      NFSPROC3_COMMIT => Some(self.commit(args)),
      NFSPROC3_PATHCONF => Some(self.pathconf(args)),
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
    // POSIX: resolving a name needs search permission on the directory.
    if !access::permits(&self.caller, &dir_node, Want::Search) {
      return Err((Nfsstat3::Acces, dir_attr));
    }
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

  /// NFSPROC3_ACCESS: which requested operations the caller may perform on an object — the exact POSIX
  /// class verdict for the request's caller ([`granted_access`]), which is what the client answers its
  /// own `open(2)` and `access(2)` from; the reply carries the object's attributes so the client caches
  /// them.
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
    Ok((
      self.fattr3(&node),
      granted_access(&self.caller, &node, requested),
    ))
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
    // POSIX: reading a file's bytes needs read permission (the owner reads its own file whatever the
    // bits say — the I/O owner override, `crate::access`).
    if !access::permits_io(&self.caller, &node, Want::Read) {
      return Err((Nfsstat3::Acces, Some(post)));
    }
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
  /// shared interface under the export's context. The reply's `committed` level is what the write
  /// asked for, made true by the host: an `UNSTABLE` write is answered `UNSTABLE` (the bytes are in
  /// the volume; the client's later COMMIT publishes them), and a `DATA_SYNC` or `FILE_SYNC` write is
  /// answered `FILE_SYNC` because the host's barrier publishes the shard's recovery image before it
  /// sends the reply (§4.8) — and replaces this reply with `NFS3ERR_IO` when that barrier is refused
  /// ([`io_failure_reply`]). A write against a read-only export or a pinned view is refused by the
  /// seam before any effect.
  pub fn write(&mut self, args: &mut XdrReader<'_>) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    match self.write_result(args) {
      Ok((post, count, committed)) => {
        Nfsstat3::Ok.encode(&mut writer);
        encode_wcc(&mut writer, Some(post));
        writer.u32(count);
        writer.u32(committed);
        writer.fixed(&self.write_verifier);
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
  ) -> Result<(Fattr3, u32, u32), (Nfsstat3, Option<Fattr3>)> {
    // WRITE3args: the file handle, the offset, the byte count, the requested stability, the data.
    // The count is advisory (the data length is authoritative); the stability decides the reply's
    // `committed` level. Both are decoded so a malformed request is a typed refusal, and the data
    // length is capped at the offered transfer size so a hostile length is refused before allocating.
    let handle = Nfsfh3::decode(args).map_err(|_| (Nfsstat3::Badhandle, None))?;
    let offset = args.u64().map_err(|_| (Nfsstat3::Inval, None))?;
    let _count = args.u32().map_err(|_| (Nfsstat3::Inval, None))?;
    let stable = args.u32().map_err(|_| (Nfsstat3::Inval, None))?;
    let data = args
      .opaque(usize::try_from(MAX_TRANSFER).unwrap_or(0))
      .map_err(|_| (Nfsstat3::Inval, None))?
      .to_vec();
    let identity = self.resolve_handle(&handle).map_err(|s| (s, None))?;
    let node = self.attrs_of(&identity).map_err(|s| (s, None))?;
    // POSIX: writing a file's bytes needs write permission (the owner writes its own file whatever the
    // bits say — the I/O owner override, `crate::access`).
    if !access::permits_io(&self.caller, &node, Want::Write) {
      return Err((Nfsstat3::Acces, Some(self.fattr3(&node))));
    }
    let cx = self.op_context().map_err(|e| (nfsstat_of(&e), None))?;
    let object = ObjectId::new(identity.inode, identity.generation);
    let written = self
      .bridge
      .write(object, &cx, offset, &data)
      .map_err(|e| (nfsstat_of(&e), None))?;
    // POSIX `write(2)`: a write by a caller other than the superuser clears the file's set-user-id and
    // set-group-id bits, so a privileged binary cannot be altered and keep its privilege.
    if written > 0
      && let Some(mode) = access::mode_after_write(&self.caller, &node)
    {
      let strip = SetAttr {
        mode: Some(mode),
        ..SetAttr::default()
      };
      self
        .bridge
        .setattr(object, &cx, strip)
        .map_err(|e| (nfsstat_of(&e), None))?;
    }
    // The post-op attributes reflect the file after the write (the wcc's post half).
    let node = self.attrs_of(&identity).map_err(|s| (s, None))?;
    let committed = if stable == UNSTABLE {
      UNSTABLE
    } else {
      FILE_SYNC
    };
    Ok((self.fattr3(&node), written, committed))
  }

  /// NFSPROC3_COMMIT: make a file's unstable writes stable (RFC 1813 §3.3.21). The bytes an
  /// `UNSTABLE` write left in the volume become stable when the shard's recovery image is published
  /// into anchor-owned RAM (§4.8); the host that runs this export does that at its barrier after the
  /// commit and before it sends the reply (and answers a refused barrier with `NFS3ERR_IO`,
  /// [`io_failure_reply`]). The commit itself resolves the handle and returns the file's current
  /// `wcc_data` and the same `writeverf3` a write returns, so a client `fsync` (which the kernel
  /// issues as COMMIT) succeeds over the mount, as the git, sqlite and editor workloads require.
  /// Without this a COMMIT is `PROC_UNAVAIL` and the client's `fsync` fails.
  pub fn commit(&mut self, args: &mut XdrReader<'_>) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    match self.commit_result(args) {
      Ok(post) => {
        Nfsstat3::Ok.encode(&mut writer);
        encode_wcc(&mut writer, Some(post));
        writer.fixed(&self.write_verifier);
      }
      Err((status, post)) => {
        status.encode(&mut writer);
        encode_wcc(&mut writer, post);
      }
    }
    writer.into_bytes()
  }

  fn commit_result(
    &mut self,
    args: &mut XdrReader<'_>,
  ) -> Result<Fattr3, (Nfsstat3, Option<Fattr3>)> {
    // COMMIT3args: the file handle, the offset, the byte count. Both range fields are advisory — the
    // host's barrier publishes the whole shard image, so every byte of the file is made stable
    // regardless of the requested range — but they are decoded so a malformed request is a typed
    // refusal.
    let handle = Nfsfh3::decode(args).map_err(|_| (Nfsstat3::Badhandle, None))?;
    let _offset = args.u64().map_err(|_| (Nfsstat3::Inval, None))?;
    let _count = args.u32().map_err(|_| (Nfsstat3::Inval, None))?;
    let identity = self
      .resolve_handle(&handle)
      .map_err(|status| (status, None))?;
    // The commit changes nothing in the volume; the file's current attributes are the wcc's post half.
    let node = self.attrs_of(&identity).map_err(|status| (status, None))?;
    Ok(self.fattr3(&node))
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
    // POSIX: removing an entry needs write and search permission on the directory, and in a sticky
    // directory only the entry's owner, the directory's owner or the superuser may remove it.
    let dir_node = self.writable_directory(&dir_identity)?;
    let dir_attr = Some(self.fattr3(&dir_node));
    let entry = self
      .bridge
      .lookup(parent, &cx, &name)
      .map_err(|e| (nfsstat_of(&e), dir_attr))?;
    if access::sticky_forbids(&self.caller, &dir_node, &entry) {
      return Err((Nfsstat3::Perm, dir_attr));
    }
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
    if let Err(status) =
      self.rename_permission(&from_identity, &to_identity, &from_name, &to_name, &cx)
    {
      let from_post = self.attrs_of(&from_identity).ok().map(|n| self.fattr3(&n));
      let to_post = self.attrs_of(&to_identity).ok().map(|n| self.fattr3(&n));
      return (status, from_post, to_post);
    }
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

  /// The POSIX permission rules of a rename, checked before any effect: write and search permission
  /// on both directories; the sticky bit of the source directory for the entry moved (and of the
  /// destination directory for an entry it replaces) — the entry's owner, the directory's owner or the
  /// superuser only; and, for a directory moved to a new parent, write permission on the directory
  /// itself, since its `..` entry is rewritten. `Acces` for a missing permission bit, `Perm` for a
  /// sticky-bit refusal; a source that does not exist is `Noent`, as the rename itself would report.
  fn rename_permission(
    &mut self,
    from_identity: &FileHandle,
    to_identity: &FileHandle,
    from_name: &str,
    to_name: &str,
    cx: &OpContext,
  ) -> Result<(), Nfsstat3> {
    let from_node = self
      .writable_directory(from_identity)
      .map_err(|(status, _)| status)?;
    let to_node = self
      .writable_directory(to_identity)
      .map_err(|(status, _)| status)?;
    let from_parent = ObjectId::new(from_identity.inode, from_identity.generation);
    let to_parent = ObjectId::new(to_identity.inode, to_identity.generation);
    let source = self
      .bridge
      .lookup(from_parent, cx, from_name)
      .map_err(|e| nfsstat_of(&e))?;
    if access::sticky_forbids(&self.caller, &from_node, &source) {
      return Err(Nfsstat3::Perm);
    }
    if source.kind == Kind::Dir
      && from_identity.inode != to_identity.inode
      && !access::permits(&self.caller, &source, Want::Write)
    {
      return Err(Nfsstat3::Acces);
    }
    if let Ok(target) = self.bridge.lookup(to_parent, cx, to_name)
      && access::sticky_forbids(&self.caller, &to_node, &target)
    {
      return Err(Nfsstat3::Perm);
    }
    Ok(())
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
    let (changes, explicit_times) = match self.decode_sattr3(args) {
      Ok(decoded) => decoded,
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
    // The POSIX ownership and permission rules for each field the request sets: the mode and explicit
    // times need ownership, the owner and group follow `_POSIX_CHOWN_RESTRICTED`, the size needs write
    // permission — refused typed (`PERM`/`ACCES`) before any effect. An allowed change carries the
    // set-id side effects a non-superuser's chown or chmod has (`crate::access`).
    if let Some(denial) = access::setattr_denial(&self.caller, &node, &changes, explicit_times) {
      return (status_of_denial(denial), Some(self.fattr3(&node)));
    }
    let changes = access::with_setid_side_effects(&self.caller, &node, changes);
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
  /// The second value says whether a time was `SET_TO_CLIENT_TIME` — an explicit time, which POSIX
  /// lets only the owner set, where "now" needs only write permission (`crate::access`).
  fn decode_sattr3(&mut self, args: &mut XdrReader<'_>) -> Result<(SetAttr, bool), Nfsstat3> {
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
    let explicit_times = atime_how == TIME_SET_TO_CLIENT || mtime_how == TIME_SET_TO_CLIENT;
    Ok((
      SetAttr {
        size,
        mode,
        uid,
        gid,
        atime: resolve_set_time(atime_how, atime_client, server_now)?,
        mtime: resolve_set_time(mtime_how, mtime_client, server_now)?,
        // An NFSv3 `sattr3` has no change time (RFC 1813 §2.3.5): it advances to the server clock.
        ctime: None,
      },
      explicit_times,
    ))
  }

  /// NFSPROC3_CREATE: create a regular file in a directory over the shared interface under the
  /// export's context, then reply the new file's handle and attributes and the directory's wcc. The
  /// `createhow3` mode selects the semantics: GUARDED fails `NFS3ERR_EXIST` on an existing name,
  /// UNCHECKED succeeds by opening it, EXCLUSIVE is refused `NFS3ERR_NOTSUPP` (owed). The `sattr3`
  /// initial attributes are applied — the mode at creation, the rest through the seam's `setattr`.
  pub fn create(&mut self, args: &mut XdrReader<'_>) -> Vec<u8> {
    let (status, made, dir_post) = self.do_create(args);
    let mut writer = XdrWriter::new();
    encode_create_reply(&mut writer, status, made, dir_post);
    writer.into_bytes()
  }

  fn do_create(
    &mut self,
    args: &mut XdrReader<'_>,
  ) -> (Nfsstat3, Option<(Nfsfh3, Fattr3)>, Option<Fattr3>) {
    // CREATE3args: where (diropargs3: dir handle then name), then how (createhow3).
    let dir_fh = match Nfsfh3::decode(args) {
      Ok(fh) => fh,
      Err(_) => return (Nfsstat3::Badhandle, None, None),
    };
    let name = match args.string(NFS_MAXNAMELEN) {
      Ok(n) => n.to_owned(),
      Err(_) => return (Nfsstat3::Inval, None, None),
    };
    let mode_kind = match args.u32() {
      Ok(m) => m,
      Err(_) => return (Nfsstat3::Inval, None, None),
    };
    let (changes, explicit_times) = match mode_kind {
      CREATE_UNCHECKED | CREATE_GUARDED => match self.decode_sattr3(args) {
        Ok(decoded) => decoded,
        Err(status) => return (status, None, None),
      },
      // EXCLUSIVE's createverf3 is an 8-byte verifier slates does not yet persist to make the
      // create idempotent; refuse it typed rather than silently degrade to a plain create.
      CREATE_EXCLUSIVE => return (Nfsstat3::Notsupp, None, None),
      _ => return (Nfsstat3::Inval, None, None),
    };
    let dir_identity = match self.resolve_handle(&dir_fh) {
      Ok(id) => id,
      Err(status) => return (status, None, None),
    };
    let cx = match self.op_context() {
      Ok(cx) => cx,
      Err(e) => return (nfsstat_of(&e), None, None),
    };
    let parent = ObjectId::new(dir_identity.inode, dir_identity.generation);
    // POSIX: resolving the name needs search permission on the directory; an existing name is then an
    // open (UNCHECKED, no write permission on the directory needed) or a refusal (GUARDED, `EXIST`);
    // creating a new one needs write permission on the directory too.
    let dir_node = match self.attrs_of(&dir_identity) {
      Ok(node) => node,
      Err(status) => return (status, None, None),
    };
    let dir_post = Some(self.fattr3(&dir_node));
    if !access::permits(&self.caller, &dir_node, Want::Search) {
      return (Nfsstat3::Acces, None, dir_post);
    }
    let existing = match self.bridge.lookup(parent, &cx, &name) {
      Ok(node) if mode_kind == CREATE_UNCHECKED => Some(ObjectId::new(node.ino, node.generation)),
      Ok(_) => return (Nfsstat3::Exist, None, dir_post),
      Err(VfsError::NotFound) => None,
      Err(e) => return (nfsstat_of(&e), None, dir_post),
    };
    let object = match existing {
      // UNCHECKED succeeds on an existing name by opening it.
      Some(object) => object,
      None => {
        if !access::permits(&self.caller, &dir_node, Want::Write) {
          return (Nfsstat3::Acces, None, dir_post);
        }
        let mode = changes.mode.unwrap_or(DEFAULT_FILE_MODE);
        match self.bridge.create(parent, &cx, &name, mode, 0) {
          Ok((node, fh)) => {
            // NFS keeps no open state; drop the open reference the create took (the client's handle
            // is identity-based, not this open handle). The lookup reference persists until the
            // teardown sweep (owed), since NFSv3 has no FORGET.
            let object = ObjectId::new(node.ino, node.generation);
            let _ = self.bridge.release(object, &cx, fh);
            object
          }
          Err(e) => {
            let dir_post = self.attrs_of(&dir_identity).ok().map(|n| self.fattr3(&n));
            return (nfsstat_of(&e), None, dir_post);
          }
        }
      }
    };
    let post_changes = SetAttr {
      mode: None,
      ..changes
    };
    self.finish_create(object, &dir_identity, &cx, post_changes, explicit_times)
  }

  /// NFSPROC3_MKDIR: create a directory in a parent over the shared interface under the export's
  /// context, then reply the new directory's handle and attributes and the parent's wcc.
  pub fn mkdir(&mut self, args: &mut XdrReader<'_>) -> Vec<u8> {
    let (status, made, dir_post) = self.do_mkdir(args);
    let mut writer = XdrWriter::new();
    encode_create_reply(&mut writer, status, made, dir_post);
    writer.into_bytes()
  }

  fn do_mkdir(
    &mut self,
    args: &mut XdrReader<'_>,
  ) -> (Nfsstat3, Option<(Nfsfh3, Fattr3)>, Option<Fattr3>) {
    // MKDIR3args: where (diropargs3), then the attributes (sattr3).
    let dir_fh = match Nfsfh3::decode(args) {
      Ok(fh) => fh,
      Err(_) => return (Nfsstat3::Badhandle, None, None),
    };
    let name = match args.string(NFS_MAXNAMELEN) {
      Ok(n) => n.to_owned(),
      Err(_) => return (Nfsstat3::Inval, None, None),
    };
    let (changes, explicit_times) = match self.decode_sattr3(args) {
      Ok(decoded) => decoded,
      Err(status) => return (status, None, None),
    };
    let dir_identity = match self.resolve_handle(&dir_fh) {
      Ok(id) => id,
      Err(status) => return (status, None, None),
    };
    let cx = match self.op_context() {
      Ok(cx) => cx,
      Err(e) => return (nfsstat_of(&e), None, None),
    };
    let parent = ObjectId::new(dir_identity.inode, dir_identity.generation);
    // POSIX: adding an entry needs write and search permission on the directory.
    if let Err((status, dir_post)) = self.writable_directory(&dir_identity) {
      return (status, None, dir_post);
    }
    let mode = changes.mode.unwrap_or(DEFAULT_DIR_MODE);
    let object = match self.bridge.mkdir(parent, &cx, &name, mode) {
      Ok(node) => ObjectId::new(node.ino, node.generation),
      Err(e) => {
        let dir_post = self.attrs_of(&dir_identity).ok().map(|n| self.fattr3(&n));
        return (nfsstat_of(&e), None, dir_post);
      }
    };
    // A directory has no size to set; apply the remaining fields (uid/gid/times).
    let post_changes = SetAttr {
      mode: None,
      size: None,
      ..changes
    };
    self.finish_create(object, &dir_identity, &cx, post_changes, explicit_times)
  }

  /// NFSPROC3_SYMLINK: create a symbolic link in a directory over the shared interface under the
  /// export's context, then reply the new link's handle and attributes and the parent's wcc. A
  /// symlink's mode is fixed at creation, so only the `sattr3` ownership and times are applied.
  pub fn symlink(&mut self, args: &mut XdrReader<'_>) -> Vec<u8> {
    let (status, made, dir_post) = self.do_symlink(args);
    let mut writer = XdrWriter::new();
    encode_create_reply(&mut writer, status, made, dir_post);
    writer.into_bytes()
  }

  fn do_symlink(
    &mut self,
    args: &mut XdrReader<'_>,
  ) -> (Nfsstat3, Option<(Nfsfh3, Fattr3)>, Option<Fattr3>) {
    // SYMLINK3args: where (diropargs3), then symlinkdata3 (the sattr3 attributes, then the target
    // path). The target is capped before allocating.
    let dir_fh = match Nfsfh3::decode(args) {
      Ok(fh) => fh,
      Err(_) => return (Nfsstat3::Badhandle, None, None),
    };
    let name = match args.string(NFS_MAXNAMELEN) {
      Ok(n) => n.to_owned(),
      Err(_) => return (Nfsstat3::Inval, None, None),
    };
    let (changes, explicit_times) = match self.decode_sattr3(args) {
      Ok(decoded) => decoded,
      Err(status) => return (status, None, None),
    };
    let target = match args.string(NFS_MAXPATHLEN) {
      Ok(t) => t.to_owned(),
      Err(_) => return (Nfsstat3::Inval, None, None),
    };
    let dir_identity = match self.resolve_handle(&dir_fh) {
      Ok(id) => id,
      Err(status) => return (status, None, None),
    };
    let cx = match self.op_context() {
      Ok(cx) => cx,
      Err(e) => return (nfsstat_of(&e), None, None),
    };
    let parent = ObjectId::new(dir_identity.inode, dir_identity.generation);
    // POSIX: adding an entry needs write and search permission on the directory.
    if let Err((status, dir_post)) = self.writable_directory(&dir_identity) {
      return (status, None, dir_post);
    }
    let object = match self.bridge.symlink(parent, &cx, &name, &target) {
      Ok(node) => ObjectId::new(node.ino, node.generation),
      Err(e) => {
        let dir_post = self.attrs_of(&dir_identity).ok().map(|n| self.fattr3(&n));
        return (nfsstat_of(&e), None, dir_post);
      }
    };
    // A symlink's mode is fixed and it has no size to set; apply the ownership and times.
    let post_changes = SetAttr {
      mode: None,
      size: None,
      ..changes
    };
    self.finish_create(object, &dir_identity, &cx, post_changes, explicit_times)
  }

  /// NFSPROC3_LINK: create a hard link (RFC 1813 §3.3.15) — a second name `new_name` in a directory
  /// for the existing non-directory object a handle names, over the shared interface under the
  /// export's context. The reply carries the linked-to file's post-operation attributes (its
  /// incremented link count) and the directory's `wcc_data`. A directory target is refused by the
  /// volume core (mapped to its `nfsstat3`), as NFSv3 requires. Without this a client's `ln` (a hard
  /// link) fails `PROC_UNAVAIL` even though the volume supports links.
  pub fn link(&mut self, args: &mut XdrReader<'_>) -> Vec<u8> {
    let (status, file_attr, dir_post) = self.do_link(args);
    let mut writer = XdrWriter::new();
    status.encode(&mut writer);
    PostOpAttr(file_attr).encode(&mut writer); // file_attributes
    encode_wcc(&mut writer, dir_post); // linkdir_wcc
    writer.into_bytes()
  }

  fn do_link(&mut self, args: &mut XdrReader<'_>) -> (Nfsstat3, Option<Fattr3>, Option<Fattr3>) {
    // LINK3args: the existing file handle, then diropargs3 (the target directory handle, the name).
    let file_fh = match Nfsfh3::decode(args) {
      Ok(fh) => fh,
      Err(_) => return (Nfsstat3::Badhandle, None, None),
    };
    let dir_fh = match Nfsfh3::decode(args) {
      Ok(fh) => fh,
      Err(_) => return (Nfsstat3::Badhandle, None, None),
    };
    let name = match args.string(NFS_MAXNAMELEN) {
      Ok(n) => n.to_owned(),
      Err(_) => return (Nfsstat3::Inval, None, None),
    };
    let file_identity = match self.resolve_handle(&file_fh) {
      Ok(id) => id,
      Err(status) => return (status, None, None),
    };
    let dir_identity = match self.resolve_handle(&dir_fh) {
      Ok(id) => id,
      Err(status) => {
        // The target file resolved; report its attributes even though the directory did not.
        let file_attr = self.attrs_of(&file_identity).ok().map(|n| self.fattr3(&n));
        return (status, file_attr, None);
      }
    };
    let cx = match self.op_context() {
      Ok(cx) => cx,
      Err(e) => return (nfsstat_of(&e), None, None),
    };
    let target = ObjectId::new(file_identity.inode, file_identity.generation);
    let new_parent = ObjectId::new(dir_identity.inode, dir_identity.generation);
    // POSIX: adding an entry needs write and search permission on the directory.
    if let Err((status, dir_post)) = self.writable_directory(&dir_identity) {
      let file_attr = self.attrs_of(&file_identity).ok().map(|n| self.fattr3(&n));
      return (status, file_attr, dir_post);
    }
    let outcome = self.bridge.link(target, new_parent, &cx, &name);
    // The directory's post-op attributes go in the wcc either way, and they are read *after* the
    // link: the client caches them in place of a GETATTR, so the times before the link would keep
    // `stat` of the directory a whole attribute-cache period behind (pjdfstest `link/00.t` through
    // a live mount, 2026-09-15). REMOVE, RENAME, CREATE, MKDIR and SYMLINK read theirs after too.
    let dir_post = self.attrs_of(&dir_identity).ok().map(|n| self.fattr3(&n));
    match outcome {
      // The bridge returns the target's attributes with the incremented link count.
      Ok(node) => (Nfsstat3::Ok, Some(self.fattr3(&node)), dir_post),
      Err(e) => {
        let file_attr = self.attrs_of(&file_identity).ok().map(|n| self.fattr3(&n));
        (nfsstat_of(&e), file_attr, dir_post)
      }
    }
  }

  /// NFSPROC3_MKNOD: slates does not create special (device, FIFO or socket) nodes — it is a RAM
  /// copy-on-write filesystem for regular files, directories, symbolic and hard links — so the
  /// operation is refused `NFS3ERR_NOTSUPP` (the typed refusal per RFC 1813 §3.3.11, not the
  /// `PROC_UNAVAIL` an unhandled procedure gives). The reply is the directory's `wcc_data`
  /// (MKNOD3resfail); the leading handle is resolved for the directory's post-op attributes, and the
  /// node type and attributes that follow it are not decoded — the refusal is unconditional.
  pub fn mknod_unsupported(&mut self, args: &mut XdrReader<'_>) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    Nfsstat3::Notsupp.encode(&mut writer);
    let dir_post = match Nfsfh3::decode(args) {
      Ok(fh) => match self.resolve_handle(&fh) {
        Ok(identity) => self.attrs_of(&identity).ok().map(|node| self.fattr3(&node)),
        Err(_) => None,
      },
      Err(_) => None,
    };
    encode_wcc(&mut writer, dir_post);
    writer.into_bytes()
  }

  /// NFSPROC3_READLINK: read the target path of a symbolic link a handle names, over the shared
  /// interface under the export's context. The reply carries the link's attributes and its target;
  /// a handle that does not name a symlink is `NFS3ERR_INVAL`.
  pub fn readlink(&mut self, args: &mut XdrReader<'_>) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    match self.readlink_result(args) {
      Ok((attr, target)) => {
        Nfsstat3::Ok.encode(&mut writer);
        PostOpAttr(Some(attr)).encode(&mut writer);
        writer.opaque(target.as_bytes());
      }
      Err((status, attr)) => {
        status.encode(&mut writer);
        PostOpAttr(attr).encode(&mut writer);
      }
    }
    writer.into_bytes()
  }

  fn readlink_result(
    &mut self,
    args: &mut XdrReader<'_>,
  ) -> Result<(Fattr3, String), (Nfsstat3, Option<Fattr3>)> {
    let handle = Nfsfh3::decode(args).map_err(|_| (Nfsstat3::Badhandle, None))?;
    let identity = self
      .resolve_handle(&handle)
      .map_err(|status| (status, None))?;
    let node = self.attrs_of(&identity).map_err(|status| (status, None))?;
    let attr = self.fattr3(&node);
    // READLINK is meaningful only on a symbolic link (RFC 1813 §3.3.5).
    if node.kind != Kind::Symlink {
      return Err((Nfsstat3::Inval, Some(attr)));
    }
    let cx = self
      .op_context()
      .map_err(|e| (nfsstat_of(&e), Some(attr)))?;
    let object = ObjectId::new(identity.inode, identity.generation);
    let target = self
      .bridge
      .readlink(object, &cx)
      .map_err(|e| (nfsstat_of(&e), Some(attr)))?;
    Ok((attr, target))
  }

  /// NFSPROC3_READDIR: list a directory's entries over the shared interface under the export's
  /// context. The reply carries the directory's attributes, a cookieverf, and as many entries as
  /// fit the client's `count` — each with a resume cookie — then the end-of-directory flag. A
  /// `count` too small for even one entry is `NFS3ERR_TOOSMALL`. The `.` (the directory) and `..`
  /// (its parent, the directory itself at the root, POSIX) entries lead the listing — the shared
  /// `readdir` emits them as positions 0 and 1 (`VolumeBridge::readdir`), so this passes them
  /// through like any child (proven by `tests/procedures.rs` `readdir_lists_entries_and_paginates`,
  /// which sees `[".", "..", "a", "b", "c"]`).
  pub fn readdir(&mut self, args: &mut XdrReader<'_>) -> Vec<u8> {
    self.encode_readdir(args, false)
  }

  /// NFSPROC3_READDIRPLUS: like READDIR, but each entry also carries the child's attributes and file
  /// handle, so a client that lists a directory needs no follow-up GETATTR/LOOKUP per entry. The
  /// reply is budgeted against `maxcount` (`dircount` is advisory and ignored). The handle is always
  /// derivable from the identity; the attributes are best-effort (`post_op_attr` absent on a miss).
  pub fn readdirplus(&mut self, args: &mut XdrReader<'_>) -> Vec<u8> {
    self.encode_readdir(args, true)
  }

  fn encode_readdir(&mut self, args: &mut XdrReader<'_>, plus: bool) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    match self.readdir_result(args, plus) {
      Ok((dir_attr, entries, eof, verf)) => {
        Nfsstat3::Ok.encode(&mut writer);
        PostOpAttr(Some(dir_attr)).encode(&mut writer);
        writer.fixed(&verf);
        for entry in entries {
          writer.bool(true); // an entry follows
          writer.u64(entry.fileid);
          writer.opaque(entry.name.as_bytes());
          writer.u64(entry.cookie);
          if plus {
            PostOpAttr(entry.attr).encode(&mut writer); // name_attributes
            match entry.handle {
              Some(handle) => {
                writer.bool(true); // name_handle follows
                handle.encode(&mut writer);
              }
              None => writer.bool(false),
            }
          }
        }
        writer.bool(false); // no more entries
        writer.bool(eof);
      }
      Err(status) => {
        status.encode(&mut writer);
        PostOpAttr(None).encode(&mut writer);
      }
    }
    writer.into_bytes()
  }

  fn readdir_result(
    &mut self,
    args: &mut XdrReader<'_>,
    plus: bool,
  ) -> Result<(Fattr3, Vec<ReaddirEntry>, bool, [u8; size_of::<u64>()]), Nfsstat3> {
    // READDIR3args: dir handle, cookie, cookieverf, count. READDIRPLUS3args replaces the trailing
    // count with dircount (advisory, ignored) then maxcount (the reply budget).
    let dir_fh = Nfsfh3::decode(args).map_err(|_| Nfsstat3::Badhandle)?;
    let cookie = args.u64().map_err(|_| Nfsstat3::Inval)?;
    let cookieverf: [u8; size_of::<u64>()] = args
      .fixed(size_of::<u64>())
      .map_err(|_| Nfsstat3::Inval)?
      .try_into()
      .unwrap_or_default();
    let budget = if plus {
      let _dircount = args.u32().map_err(|_| Nfsstat3::Inval)?;
      usize::try_from(args.u32().map_err(|_| Nfsstat3::Inval)?).unwrap_or(0)
    } else {
      usize::try_from(args.u32().map_err(|_| Nfsstat3::Inval)?).unwrap_or(0)
    };
    let identity = self.resolve_handle(&dir_fh)?;
    let dir_node = self.attrs_of(&identity)?;
    let dir_attr = self.fattr3(&dir_node);
    // POSIX: listing a directory needs read permission on it.
    if !access::permits(&self.caller, &dir_node, Want::Read) {
      return Err(Nfsstat3::Acces);
    }
    let cx = self.op_context().map_err(|e| nfsstat_of(&e))?;
    let dir_object = ObjectId::new(identity.inode, identity.generation);
    // The cookieverf is the directory's monotonic change version (§4.5): a continuation (cookie
    // != 0) whose verf no longer matches means the directory changed since the listing began, so
    // the client must restart from the beginning (RFC 1813 §3.3.16). The version is collision-free,
    // unlike a change-time verf: a mutation always advances it, so a change is never missed.
    let verf = self
      .bridge
      .change_token(dir_object, &cx)
      .map_err(|e| nfsstat_of(&e))?
      .to_be_bytes();
    if cookie != 0 && cookieverf != verf {
      return Err(Nfsstat3::BadCookie);
    }
    // The cookie is the number of entries already returned; the shared readdir skips that many.
    let rows = self
      .bridge
      .readdir(dir_object, &cx, 0, cookie)
      .map_err(|e| nfsstat_of(&e))?;
    let mut used = READDIR_REPLY_OVERHEAD;
    let mut entries = Vec::new();
    let mut eof = true;
    for (index, row) in rows.into_iter().enumerate() {
      // For a plus listing, gather the child's attributes (best-effort) and handle (always
      // derivable) before budgeting, since the handle's encoded length varies with the fh.
      let (attr, handle) = if plus {
        let child = ObjectId::new(row.ino, 0);
        let attr = self
          .bridge
          .getattr(child, &cx)
          .ok()
          .map(|n| self.fattr3(&n));
        (attr, Some(self.handle_for(row.ino, 0)))
      } else {
        (None, None)
      };
      let mut entry_bytes = READDIR_ENTRY_FIXED + xdr_str_len(&row.name);
      if plus {
        entry_bytes = entry_bytes
          .saturating_add(PLUS_ENTRY_FIXED)
          .saturating_add(handle.as_ref().map_or(0, |h| xdr_len(h.0.len())));
      }
      if used.saturating_add(entry_bytes) > budget {
        if entries.is_empty() {
          // Not even one entry fits the client's count (RFC 1813 §3.3.16-17).
          return Err(Nfsstat3::Toosmall);
        }
        eof = false; // more entries remain for the next call
        break;
      }
      used = used.saturating_add(entry_bytes);
      let entry_cookie = cookie
        .saturating_add(u64::try_from(index).unwrap_or(u64::MAX))
        .saturating_add(1);
      entries.push(ReaddirEntry {
        fileid: row.ino,
        name: row.name,
        cookie: entry_cookie,
        attr,
        handle,
      });
    }
    Ok((dir_attr, entries, eof, verf))
  }

  /// The shared tail of CREATE, MKDIR and SYMLINK: apply the `sattr3` fields the creation did not
  /// set (the mode is set at creation) under the same POSIX ownership rules a SETATTR has — the
  /// creator owns the new object, so only a `uid`/`gid` it may not take (`_POSIX_CHOWN_RESTRICTED`) or
  /// explicit times on an existing UNCHECKED-opened object of another owner refuse — then fetch the
  /// object's final attributes, mint its handle, and gather the parent directory's post-op attributes
  /// for the wcc.
  fn finish_create(
    &mut self,
    object: ObjectId,
    dir_identity: &FileHandle,
    cx: &OpContext,
    post_changes: SetAttr,
    explicit_times: bool,
  ) -> (Nfsstat3, Option<(Nfsfh3, Fattr3)>, Option<Fattr3>) {
    if post_changes != SetAttr::default() {
      let node = match self.bridge.getattr(object, cx) {
        Ok(node) => node,
        Err(e) => {
          let dir_post = self.attrs_of(dir_identity).ok().map(|n| self.fattr3(&n));
          return (nfsstat_of(&e), None, dir_post);
        }
      };
      if let Some(denial) =
        access::setattr_denial(&self.caller, &node, &post_changes, explicit_times)
      {
        let dir_post = self.attrs_of(dir_identity).ok().map(|n| self.fattr3(&n));
        return (status_of_denial(denial), None, dir_post);
      }
      let post_changes = access::with_setid_side_effects(&self.caller, &node, post_changes);
      if let Err(e) = self.bridge.setattr(object, cx, post_changes) {
        let dir_post = self.attrs_of(dir_identity).ok().map(|n| self.fattr3(&n));
        return (nfsstat_of(&e), None, dir_post);
      }
    }
    let attr = match self.bridge.getattr(object, cx) {
      Ok(node) => self.fattr3(&node),
      Err(e) => {
        let dir_post = self.attrs_of(dir_identity).ok().map(|n| self.fattr3(&n));
        return (nfsstat_of(&e), None, dir_post);
      }
    };
    let handle = self.handle_for(object.inode, object.generation);
    let dir_post = self.attrs_of(dir_identity).ok().map(|n| self.fattr3(&n));
    (Nfsstat3::Ok, Some((handle, attr)), dir_post)
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

  /// NFSPROC3_PATHCONF: the POSIX pathconf limits for the filesystem an object lives in (RFC 1813
  /// §3.3.20), reported from the volume's own policy rather than a fixed guess. The maximum name
  /// length and the case behaviour come from the volume through `statfs`; the link maximum is the
  /// `u32` link counter's range ([`PATHCONF_LINKMAX`]); an over-long name is refused, never truncated
  /// (`no_trunc` true); ownership changes are restricted to the superuser (`chown_restricted` true —
  /// `_POSIX_CHOWN_RESTRICTED`, the rule [`crate::access::may_chown`] applies to SETATTR); and case is
  /// always preserved. Without this a client's `pathconf` is `PROC_UNAVAIL` and it falls back to
  /// conservative defaults.
  pub fn pathconf(&mut self, args: &mut XdrReader<'_>) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    match self.pathconf_result(args) {
      Ok((attr, stat)) => {
        Nfsstat3::Ok.encode(&mut writer);
        PostOpAttr(Some(attr)).encode(&mut writer);
        writer.u32(PATHCONF_LINKMAX); // linkmax
        writer.u32(stat.namelen); // name_max
        writer.bool(true); // no_trunc: an over-long name is refused, never truncated
        writer.bool(true); // chown_restricted: only the superuser changes an owner (POSIX)
        writer.bool(!stat.case_sensitive); // case_insensitive: the inverse of the volume's policy
        writer.bool(true); // case_preserving: the stored case is always kept
      }
      Err(status) => {
        status.encode(&mut writer);
        PostOpAttr(None).encode(&mut writer);
      }
    }
    writer.into_bytes()
  }

  fn pathconf_result(&mut self, args: &mut XdrReader<'_>) -> Result<(Fattr3, FsStat), Nfsstat3> {
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

/// The ACCESS bits granted to `caller` on `node`, intersected with the `requested` bits: the exact
/// POSIX class verdict ([`access::permits`]) with the superuser's exemptions — so a client's own
/// `open(2)` and `access(2)` checks, which it answers from this reply, are right for every user, not
/// only the owner. (Before 2026-09-15 this read the owner's bits whoever asked, the per-principal
/// check being owed; a caller other than the owner was then granted the owner's access.)
fn granted_access(caller: &Caller, node: &NodeAttr, requested: u32) -> u32 {
  let mut granted = 0;
  if access::permits(caller, node, Want::Read) {
    granted |= ACCESS3_READ;
  }
  if access::permits(caller, node, Want::Search) {
    granted |= ACCESS3_LOOKUP | ACCESS3_EXECUTE;
  }
  if access::permits(caller, node, Want::Write) {
    granted |= ACCESS3_MODIFY | ACCESS3_EXTEND | ACCESS3_DELETE;
  }
  granted & requested
}

/// The NFSv3 status an ownership-rule refusal maps to: `NFS3ERR_PERM` for "not the owner",
/// `NFS3ERR_ACCES` for a missing permission bit.
fn status_of_denial(denial: Denial) -> Nfsstat3 {
  match denial {
    Denial::NotOwner => Nfsstat3::Perm,
    Denial::NoAccess => Nfsstat3::Acces,
  }
}

/// Encodes an NFSv3 `wcc_data`: the pre-operation attributes (slates keeps none, so absent) then
/// the post-operation attributes. A mutating reply (WRITE) carries it so the client updates its
/// cache without a follow-up GETATTR; the absent pre-op half means the client cannot detect a
/// racing outside change, which slates has none of on a head it owns (§4.6 cache posture).
fn encode_wcc(writer: &mut XdrWriter, post: Option<Fattr3>) {
  writer.bool(false);
  PostOpAttr(post).encode(writer);
}

/// The `stable_how` a WRITE call asks for (RFC 1813 §3.3.7: `UNSTABLE`, `DATA_SYNC` or
/// `FILE_SYNC`), decoded from its arguments — the file handle, the offset and the count precede
/// it — or `None` for a call that does not decode that far. The host's barrier reads it to decide
/// whether the write's reply may go out before the shard's recovery image is published: only an
/// `UNSTABLE` write may (its client commits later); any other level is published first.
pub fn write_stable_how(args: &mut XdrReader<'_>) -> Option<u32> {
  Nfsfh3::decode(args).ok()?;
  args.u64().ok()?;
  args.u32().ok()?;
  args.u32().ok()
}

/// Whether a WRITE's decoded `stable_how` asks for a reply before the data is stable (`UNSTABLE`).
pub fn is_unstable(stable_how: u32) -> bool {
  stable_how == UNSTABLE
}

/// The reply for a mutating procedure whose effect the host could not make stable — its barrier
/// (publishing the shard's recovery image, §4.8) was refused after the effect took place in the
/// volume: `NFS3ERR_IO` with the procedure's failure shape, every attribute absent, so the client
/// learns the operation is not stable rather than being told it is (D-18: an acknowledgement
/// promises daemon-restart survival only when the bytes are recoverable). `None` for a procedure
/// that mutates nothing, which never needs a barrier. The failure shapes are RFC 1813's `resfail`
/// arms: `wcc_data` for SETATTR, WRITE, CREATE, MKDIR, SYMLINK, MKNOD, REMOVE, RMDIR and COMMIT;
/// two `wcc_data` for RENAME; a `post_op_attr` and a `wcc_data` for LINK.
pub fn io_failure_reply(procedure: u32) -> Option<Vec<u8>> {
  let mut writer = XdrWriter::new();
  Nfsstat3::Io.encode(&mut writer);
  match procedure {
    NFSPROC3_SETATTR | NFSPROC3_WRITE | NFSPROC3_CREATE | NFSPROC3_MKDIR | NFSPROC3_SYMLINK
    | NFSPROC3_MKNOD | NFSPROC3_REMOVE | NFSPROC3_RMDIR | NFSPROC3_COMMIT => {
      encode_wcc(&mut writer, None)
    }
    NFSPROC3_RENAME => {
      encode_wcc(&mut writer, None);
      encode_wcc(&mut writer, None);
    }
    NFSPROC3_LINK => {
      PostOpAttr(None).encode(&mut writer);
      encode_wcc(&mut writer, None);
    }
    _ => return None,
  }
  Some(writer.into_bytes())
}

/// Encodes a CREATE/MKDIR/SYMLINK reply (they share the shape, RFC 1813 §3.3.8-10): on success the
/// new object's `post_op_fh3` and `post_op_attr` then the parent's `wcc_data`; on failure just the
/// parent's `wcc_data`.
fn encode_create_reply(
  writer: &mut XdrWriter,
  status: Nfsstat3,
  made: Option<(Nfsfh3, Fattr3)>,
  dir_post: Option<Fattr3>,
) {
  status.encode(writer);
  if let Some((handle, attr)) = made {
    writer.bool(true); // post_op_fh3: a handle follows
    handle.encode(writer);
    PostOpAttr(Some(attr)).encode(writer);
  }
  encode_wcc(writer, dir_post);
}

/// The XDR-encoded byte length of a variable-length field of `len` bytes: a `u32` length prefix plus
/// the bytes padded up to XDR's 4-byte (one `u32`) boundary. Used to budget a READDIR reply.
fn xdr_len(len: usize) -> usize {
  const UNIT: usize = size_of::<u32>();
  UNIT + len.div_ceil(UNIT) * UNIT
}

/// The XDR-encoded byte length of a variable-length string or opaque.
fn xdr_str_len(s: &str) -> usize {
  xdr_len(s.len())
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
