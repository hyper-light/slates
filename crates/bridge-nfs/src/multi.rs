//! Serving many volumes from one NFS server under a single root mount (§4.6): the design's mount model
//! is "the single kernel mount point per host under which volumes appear as directories"
//! (SLATES_DESIGN §4.6, "Root mount"), so one loopback server serves every volume the daemon holds,
//! not one server per volume. This module is that whole shape at the NFS layer:
//!
//! * [`NfsService`] is what the serve loop and `dispatch` serve NFS/MOUNT over — a single-volume
//!   [`Export`], or a [`MultiExport`] over several — so the transport is unchanged either way.
//! * [`VolumeSet`] is where the volumes come from. The daemon holds many volumes sharing one store on a
//!   shard ([`ShardState`](../../server) — one `store`, a slab of volumes), so a volume is served through
//!   a *transient* bridge built for the request (the design's "marshal each operation into the bridge
//!   queue of the owning shard", the shape `bridge-fskit`'s `MountSession` already takes); the set
//!   resolves a volume id to that transient serve. The test's [`OwnedVolumeSet`] is the same shape — one
//!   store, several volumes — so the shared-store path the daemon uses is exercised here.
//! * Routing needs no table: every served NFSv3 procedure begins with a file handle, and the handle
//!   already encodes `(volume, inode, gen)` ([`crate::handle`]), so the router reads the leading
//!   handle's volume id and asks the set to serve it. A handle for a volume not in the set is
//!   `NFS3ERR_STALE`.
//! * The **synthetic root** is a read-only directory whose entries are the volumes: `MNT /` returns its
//!   handle, `READDIR`/`READDIRPLUS` list the volume names, `LOOKUP` a name returns that volume's root
//!   handle (the same a direct mount gives), and every mutation is `NFS3ERR_ROFS` — a volume appears by
//!   a metadata operation, never a client `mkdir` (design line 129). So one `mount_nfs localhost:/` lets
//!   a client `ls` the volumes and `cd` into any of them.
//!
//! What is owed: this serves the volumes of one [`VolumeSet`] (the daemon's per-shard set, or a test's).
//! Serving volumes that live on *different* shards needs the cross-shard bridge queue (§4.3, D-7
//! "bridge queues pinned to the owner") to route a request to a volume's owning shard; the routing and
//! the root here sit unchanged above whichever supplies the volumes.

use slates_bridge_core::{Rights, VolumeBridge};
use slates_db::catalog::{Principal, VolumeId};
use slates_vfs::volume::{Store, Volume};

use crate::handle::FileHandle;
use crate::mount::{MountReply, Mountstat3};
use crate::nfs::{Fattr3, Ftype3, Nfsfh3, Nfsstat3, Nfstime3, PostOpAttr, Specdata3};
use crate::procedures::{
  Export, NFS_MAXNAMELEN, NFSPROC3_ACCESS, NFSPROC3_COMMIT, NFSPROC3_CREATE, NFSPROC3_FSINFO,
  NFSPROC3_FSSTAT, NFSPROC3_GETATTR, NFSPROC3_LINK, NFSPROC3_LOOKUP, NFSPROC3_MKDIR,
  NFSPROC3_MKNOD, NFSPROC3_NULL, NFSPROC3_READ, NFSPROC3_READDIR, NFSPROC3_READDIRPLUS,
  NFSPROC3_READLINK, NFSPROC3_REMOVE, NFSPROC3_RENAME, NFSPROC3_RMDIR, NFSPROC3_SETATTR,
  NFSPROC3_SYMLINK, NFSPROC3_WRITE,
};
use crate::xdr::{XdrReader, XdrWriter};

/// Format: the reserved volume id of the synthetic root directory that lists the exported volumes.
/// All-zero, which no real volume carries (a `VolumeId` is a creator-routable 128-bit id whose high
/// half is a real host id, never zero), so a file handle carrying it names the root, not a volume.
const ROOT_VOLUME: VolumeId = VolumeId { bytes: [0u8; 16] };
/// Format: the inode number of the synthetic root directory (the conventional filesystem root, 1).
const ROOT_INODE: u64 = 1;
/// Format: the root directory's mode — `r-xr-xr-x`, listable and searchable by everyone, writable by
/// no one; a volume appears under the root by a metadata operation (an attachment), never a client
/// create, so every mutation on the root is refused `NFS3ERR_ROFS`.
const ROOT_MODE: u32 = 0o555;
/// Format: the first synthetic `fileid` a volume entry reports in the root listing. The root's own
/// inode is 1; each volume entry gets a stable id past it by position (base + index). A volume's real
/// root inode belongs to the volume's own filesystem (a different `fsid`), so it is not reused here —
/// a client crossing from the root into a volume sees the `fsid` change, as at any mount point.
const ROOT_ENTRY_FILEID_BASE: u64 = 2;
/// Format: the synthetic root's own filesystem id, distinct from any volume's fsid (a volume's fsid is
/// the leading half of its non-zero id), so a client crossing into a volume observes the fsid change.
const ROOT_FSID: u64 = 0;
/// Format: the `AUTH_SYS` authentication flavor a mount reply offers (RFC 5531).
const AUTH_SYS: u32 = 1;
/// Format: the `AUTH_NONE` authentication flavor a mount reply offers (RFC 5531).
const AUTH_NONE: u32 = 0;
/// Format: ACCESS3_READ — read file data or list a directory (RFC 1813 §3.3.4).
const ACCESS3_READ: u32 = 0x1;
/// Format: ACCESS3_LOOKUP — look a name up in a directory.
const ACCESS3_LOOKUP: u32 = 0x2;
/// Format: ACCESS3_EXECUTE — search (execute) a directory.
const ACCESS3_EXECUTE: u32 = 0x20;
/// Format: the fixed XDR bytes of one present `post_op_attr` — the value-follows bool (4) plus a
/// `fattr3` (84) — reserved when budgeting a READDIR reply against the client's `count`.
const POST_OP_ATTR_BYTES: usize = 4 + 84;
/// Format: the fixed XDR bytes a READDIR reply carries besides its entries — the status (4), a present
/// directory `post_op_attr`, the `cookieverf` (8), and the trailing list-end and `eof` bools (8).
const READDIR_OVERHEAD: usize = 4 + POST_OP_ATTR_BYTES + 8 + 8;
/// Format: transfer sizes the synthetic root advertises in FSINFO — one page, since the root serves
/// only directory listings, never bulk file I/O (that happens inside a volume, over its own FSINFO).
const ROOT_TRANSFER: u32 = 4096;
/// Format: FSF3_HOMOGENEOUS — PATHCONF is uniform across the (empty, read-only) root filesystem.
const FSF3_HOMOGENEOUS: u32 = 0x8;
/// Format: one nanosecond, the time granularity the root reports (its times are synthetic zeros).
const ONE_NANOSECOND: Nfstime3 = Nfstime3 {
  seconds: 0,
  nseconds: 1,
};

/// The volumes an NFS service serves, resolved per request. The daemon's volumes share one store on a
/// shard, so a volume is served through a *transient* bridge built for the request (the design's
/// "marshal each operation into the bridge queue of the owning shard"); this trait is that seam — the
/// daemon implements it over its `ShardState`, a test over an owned set — so the routing and the
/// synthetic root above it are written once.
pub trait VolumeSet {
  /// The mounted volumes as `(mount name, id)`, in listing order — the root directory's entries.
  fn entries(&self) -> Vec<(String, VolumeId)>;

  /// Serves one NFSv3 procedure against `volume` (routing has already chosen it), building a transient
  /// bridge over the shared store under `subject`/`rights`. `None` if the volume is not in the set (the
  /// router then answers the handle `NFS3ERR_STALE`). `args` is positioned at the procedure arguments.
  fn serve(
    &mut self,
    volume: VolumeId,
    subject: Principal,
    rights: Rights,
    procedure: u32,
    args: &mut XdrReader<'_>,
  ) -> Option<Vec<u8>>;

  /// The root file handle and attributes of `volume`, for the synthetic root's `LOOKUP` and
  /// `READDIRPLUS` of the volume's name. `None` if the volume is not in the set or its root cannot be
  /// established.
  fn root_object(
    &mut self,
    volume: VolumeId,
    subject: Principal,
    rights: Rights,
  ) -> Option<(Nfsfh3, Fattr3)>;
}

/// What the loopback server serves NFS and MOUNT over: a single-volume [`Export`], or a [`MultiExport`]
/// over a [`VolumeSet`]. The serve loop and [`crate::server::serve_connection`] work against this trait,
/// so one server serves one volume or many with no change to the transport.
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

/// An NFS service over a [`VolumeSet`] under one read-only root directory, routing each request to the
/// volume its file handle names. The mount credentials (`subject`/`rights`) are established once and
/// carried on every request the set serves — the export edge's role (§4.13).
pub struct MultiExport<V: VolumeSet> {
  set: V,
  subject: Principal,
  rights: Rights,
}

impl<V: VolumeSet> MultiExport<V> {
  /// A service over `set`, whose requests run under `subject` with `rights`.
  pub fn new(set: V, subject: Principal, rights: Rights) -> MultiExport<V> {
    MultiExport {
      set,
      subject,
      rights,
    }
  }

  /// The number of volumes the set holds.
  pub fn len(&self) -> usize {
    self.set.entries().len()
  }

  /// Whether the set holds no volume.
  pub fn is_empty(&self) -> bool {
    self.set.entries().is_empty()
  }

  /// The volume id mounted under `name`, if any.
  fn volume_of(&self, name: &str) -> Option<VolumeId> {
    self
      .set
      .entries()
      .into_iter()
      .find(|(mount, _)| mount == name)
      .map(|(_, id)| id)
  }

  // ---------------------------------------------------------------- the synthetic root directory

  /// The `fattr3` of the synthetic root directory: a directory owned by root, its link count one per
  /// volume plus the two every directory has (itself and its parent), read-only, its own filesystem.
  fn root_fattr3(&self) -> Fattr3 {
    Fattr3 {
      kind: Ftype3::Dir,
      mode: ROOT_MODE,
      nlink: u32::try_from(self.set.entries().len().saturating_add(2)).unwrap_or(u32::MAX),
      uid: 0,
      gid: 0,
      size: 0,
      used: 0,
      rdev: Specdata3::default(),
      fsid: ROOT_FSID,
      fileid: ROOT_INODE,
      atime: Nfstime3::default(),
      mtime: Nfstime3::default(),
      ctime: Nfstime3::default(),
    }
  }

  /// A cookie-verifier for the root listing: it changes when the set of volumes changes, so a client
  /// continuing a listing across such a change is told to restart (a per-add/remove monotone token is
  /// owed; the volume count catches the common add/remove).
  fn root_verf(&self) -> [u8; size_of::<u64>()] {
    u64::try_from(self.set.entries().len())
      .unwrap_or(0)
      .to_be_bytes()
  }

  /// Serves a request whose handle names the synthetic root: the read-only directory operations a
  /// client uses to mount `/` and browse the volumes, and a typed refusal for everything that would
  /// change the root (`NFS3ERR_ROFS`) or misread it (`READ`/`READLINK` of a directory).
  fn serve_root(&mut self, procedure: u32, args: &mut XdrReader<'_>) -> Option<Vec<u8>> {
    let reply = match procedure {
      NFSPROC3_GETATTR => self.root_getattr(),
      NFSPROC3_ACCESS => self.root_access(args),
      NFSPROC3_LOOKUP => self.root_lookup(args),
      NFSPROC3_READDIR => self.root_readdir(args, false),
      NFSPROC3_READDIRPLUS => self.root_readdir(args, true),
      NFSPROC3_FSINFO => self.root_fsinfo(),
      NFSPROC3_FSSTAT => self.root_fsstat(),
      // COMMIT of the synthetic root: it holds no unwritten data, so it is a successful no-op.
      NFSPROC3_COMMIT => self.root_commit(),
      // A directory cannot be read as a file or a symlink.
      NFSPROC3_READ => post_attr_failure(Nfsstat3::Isdir, Some(self.root_fattr3())),
      NFSPROC3_READLINK => post_attr_failure(Nfsstat3::Inval, Some(self.root_fattr3())),
      // Everything that would change the root is refused: a volume appears by a metadata operation.
      NFSPROC3_SETATTR | NFSPROC3_WRITE | NFSPROC3_CREATE | NFSPROC3_MKDIR | NFSPROC3_SYMLINK
      | NFSPROC3_REMOVE | NFSPROC3_RMDIR | NFSPROC3_RENAME | NFSPROC3_LINK | NFSPROC3_MKNOD => {
        root_readonly_refusal(procedure)
      }
      _ => return None,
    };
    Some(reply)
  }

  /// COMMIT of the root: the synthetic root has no unwritten data — its listing is derived on demand
  /// — so the commit succeeds with an absent `wcc_data` and the root's verifier (RFC 1813 §3.3.21).
  fn root_commit(&self) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    Nfsstat3::Ok.encode(&mut writer);
    write_absent_wcc(&mut writer);
    writer.fixed(&self.root_verf());
    writer.into_bytes()
  }

  /// GETATTR of the root: its directory attributes.
  fn root_getattr(&self) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    Nfsstat3::Ok.encode(&mut writer);
    self.root_fattr3().encode(&mut writer);
    writer.into_bytes()
  }

  /// ACCESS of the root: it grants read, lookup and execute (search) among those requested — a
  /// read-only, listable, searchable directory.
  fn root_access(&self, args: &mut XdrReader<'_>) -> Vec<u8> {
    let _fh = Nfsfh3::decode(args);
    let requested = args.u32().unwrap_or(0);
    let granted = (ACCESS3_READ | ACCESS3_LOOKUP | ACCESS3_EXECUTE) & requested;
    let mut writer = XdrWriter::new();
    Nfsstat3::Ok.encode(&mut writer);
    PostOpAttr(Some(self.root_fattr3())).encode(&mut writer);
    writer.u32(granted);
    writer.into_bytes()
  }

  /// LOOKUP of a name in the root: a mounted volume's name resolves to that volume's root handle and
  /// attributes (the same a direct mount gives), so a client `cd`s into the volume; any other name is
  /// `NFS3ERR_NOENT`.
  fn root_lookup(&mut self, args: &mut XdrReader<'_>) -> Vec<u8> {
    let dir_attr = self.root_fattr3();
    let _dir_fh = Nfsfh3::decode(args);
    let name = match args.string(NFS_MAXNAMELEN) {
      Ok(name) => name.to_owned(),
      Err(_) => return lookup_failure(Nfsstat3::Inval, None),
    };
    let object = self.volume_of(&name).and_then(|volume| {
      self
        .set
        .root_object(volume, self.subject.clone(), self.rights)
    });
    match object {
      Some((handle, attr)) => {
        let mut writer = XdrWriter::new();
        Nfsstat3::Ok.encode(&mut writer);
        handle.encode(&mut writer);
        PostOpAttr(Some(attr)).encode(&mut writer);
        PostOpAttr(Some(dir_attr)).encode(&mut writer);
        writer.into_bytes()
      }
      None => lookup_failure(Nfsstat3::Noent, Some(dir_attr)),
    }
  }

  /// READDIR / READDIRPLUS of the root: the mounted volumes as entries, from the client's resume
  /// cookie, as many as the client's `count`/`maxcount` admits (each entry carries a resume cookie),
  /// then the end-of-directory flag. `NFS3ERR_TOOSMALL` if the budget cannot hold even one entry.
  fn root_readdir(&mut self, args: &mut XdrReader<'_>, plus: bool) -> Vec<u8> {
    let dir_attr = self.root_fattr3();
    let verf = self.root_verf();
    let _dir_fh = Nfsfh3::decode(args);
    let cookie = args.u64().unwrap_or(0);
    let _cookieverf = args.fixed(size_of::<u64>());
    // READDIR carries `count`; READDIRPLUS carries `dircount` (advisory, ignored) then `maxcount`.
    let budget = if plus {
      let _dircount = args.u32();
      usize::try_from(args.u32().unwrap_or(0)).unwrap_or(0)
    } else {
      usize::try_from(args.u32().unwrap_or(0)).unwrap_or(0)
    };
    let start = usize::try_from(cookie).unwrap_or(usize::MAX);
    let entries = self.set.entries();

    let mut body = XdrWriter::new();
    let mut used = READDIR_OVERHEAD;
    let mut emitted = 0usize;
    let mut eof = true;
    for (index, (name, volume)) in entries.iter().enumerate().skip(start) {
      let fileid = ROOT_ENTRY_FILEID_BASE.saturating_add(u64::try_from(index).unwrap_or(0));
      let plus_object = if plus {
        self
          .set
          .root_object(*volume, self.subject.clone(), self.rights)
      } else {
        None
      };
      let entry = encode_readdir_entry(name, fileid, index, plus, plus_object);
      if budget != 0 && used.saturating_add(entry.len()) > budget {
        eof = false;
        break;
      }
      used = used.saturating_add(entry.len());
      body.fixed(&entry); // each entry is already four-byte aligned, so no extra padding is added
      emitted += 1;
    }
    // A budget too small for even one pending entry is `TOOSMALL`, per RFC 1813 §3.3.16.
    if emitted == 0 && start < entries.len() {
      return readdir_failure(Nfsstat3::Toosmall, dir_attr);
    }

    let mut writer = XdrWriter::new();
    Nfsstat3::Ok.encode(&mut writer);
    PostOpAttr(Some(dir_attr)).encode(&mut writer);
    writer.fixed(&verf);
    writer.fixed(body.as_slice());
    writer.bool(false); // no more entries
    writer.bool(eof);
    writer.into_bytes()
  }

  /// FSINFO of the root: static limits for the pseudo-filesystem. Transfers are one page (the root
  /// serves only listings), times are settable to a one-nanosecond granularity, and the filesystem is
  /// homogeneous; a real volume answers its own FSINFO once a client crosses into it.
  fn root_fsinfo(&self) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    Nfsstat3::Ok.encode(&mut writer);
    PostOpAttr(Some(self.root_fattr3())).encode(&mut writer);
    for value in [
      ROOT_TRANSFER, // rtmax
      ROOT_TRANSFER, // rtpref
      ROOT_TRANSFER, // rtmult
      ROOT_TRANSFER, // wtmax
      ROOT_TRANSFER, // wtpref
      ROOT_TRANSFER, // wtmult
      ROOT_TRANSFER, // dtpref
    ] {
      writer.u32(value);
    }
    writer.u64(0); // maxfilesize — the root holds no files
    ONE_NANOSECOND.encode(&mut writer); // time_delta
    writer.u32(FSF3_HOMOGENEOUS); // properties: no hard links or symlinks in the read-only root
    writer.into_bytes()
  }

  /// FSSTAT of the root: an empty, read-only pseudo-filesystem — no space and no room to grow.
  fn root_fsstat(&self) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    Nfsstat3::Ok.encode(&mut writer);
    PostOpAttr(Some(self.root_fattr3())).encode(&mut writer);
    writer.u64(0); // tbytes: total bytes in the filesystem
    writer.u64(0); // fbytes: free bytes
    writer.u64(0); // abytes: bytes available to the caller
    writer.u64(0); // tfiles: total file slots
    writer.u64(0); // ffiles: free file slots
    writer.u64(0); // afiles: file slots available to the caller
    writer.u32(0); // invarsec: seconds the stats stay invariant
    writer.into_bytes()
  }
}

impl<V: VolumeSet> NfsService for MultiExport<V> {
  fn serve_mount(&mut self, path: &str) -> MountReply {
    let name = path.trim_matches('/');
    if name.is_empty() {
      // The host root: a client mounts `/` and browses the volumes as subdirectories.
      return MountReply::Ok {
        handle: root_handle(),
        auth_flavors: vec![AUTH_SYS, AUTH_NONE],
      };
    }
    // A client may also mount a specific volume's subtree directly by its name.
    match self.volume_of(name).and_then(|volume| {
      self
        .set
        .root_object(volume, self.subject.clone(), self.rights)
    }) {
      Some((handle, _)) => MountReply::Ok {
        handle,
        auth_flavors: vec![AUTH_SYS, AUTH_NONE],
      },
      None => MountReply::Err(Mountstat3::Noent),
    }
  }

  fn serve_procedure(&mut self, procedure: u32, args: &mut XdrReader<'_>) -> Option<Vec<u8>> {
    if procedure == NFSPROC3_NULL {
      return Some(Vec::new());
    }
    match peek_handle_volume(args) {
      // The synthetic root's own handle.
      Some(volume) if volume == ROOT_VOLUME => self.serve_root(procedure, args),
      // A volume's handle routes to that volume's transient serve; an unknown volume is stale.
      Some(volume) => Some(
        self
          .set
          .serve(volume, self.subject.clone(), self.rights, procedure, args)
          .unwrap_or_else(|| stale_for(procedure)),
      ),
      // An unparseable handle is a bad handle in the procedure's own reply shape.
      None => Some(badhandle_for(procedure)),
    }
  }
}

/// One volume of an [`OwnedVolumeSet`]: its mount name, id, the volume core object, and its overlay
/// host if it has a base. The volumes share the set's one store, the daemon's shape.
pub struct OwnedVolume {
  /// The mount name the volume appears under in the root.
  pub name: String,
  /// The volume id (its routing key and file-handle stamp).
  pub id: VolumeId,
  /// The volume core object.
  pub volume: Volume,
}

/// A [`VolumeSet`] that owns its store and volumes — the shape a shard holds (one store, several
/// volumes), used by tests and examples to drive the shared-store serve the daemon uses. Each request
/// builds a transient [`VolumeBridge`] over the store and the routed volume.
pub struct OwnedVolumeSet {
  store: Store,
  volumes: Vec<OwnedVolume>,
}

impl OwnedVolumeSet {
  /// A set over `store` with no volumes yet.
  pub fn new(store: Store) -> OwnedVolumeSet {
    OwnedVolumeSet {
      store,
      volumes: Vec::new(),
    }
  }

  /// Adds `volume` (id `id`) under the mount `name`. All volumes share the set's store.
  pub fn add(&mut self, name: impl Into<String>, id: VolumeId, volume: Volume) {
    self.volumes.push(OwnedVolume {
      name: name.into(),
      id,
      volume,
    });
  }

  /// Builds a transient export for `volume` over the shared store and runs `f` with it; `None` if the
  /// volume is not in the set or its attachment cannot be admitted.
  fn with_export<R>(
    &mut self,
    volume: VolumeId,
    subject: Principal,
    rights: Rights,
    f: impl FnOnce(&mut Export<'_>) -> R,
  ) -> Option<R> {
    let store = &mut self.store;
    let slot = self.volumes.iter_mut().find(|v| v.id == volume)?;
    let mut bridge = VolumeBridge::new(volume, &mut slot.volume, store);
    let mut export = Export::new(&mut bridge, volume, subject, rights).ok()?;
    Some(f(&mut export))
  }
}

impl VolumeSet for OwnedVolumeSet {
  fn entries(&self) -> Vec<(String, VolumeId)> {
    self
      .volumes
      .iter()
      .map(|v| (v.name.clone(), v.id))
      .collect()
  }

  fn serve(
    &mut self,
    volume: VolumeId,
    subject: Principal,
    rights: Rights,
    procedure: u32,
    args: &mut XdrReader<'_>,
  ) -> Option<Vec<u8>> {
    self
      .with_export(volume, subject, rights, |export| {
        export.serve_nfs(procedure, args)
      })
      .flatten()
  }

  fn root_object(
    &mut self,
    volume: VolumeId,
    subject: Principal,
    rights: Rights,
  ) -> Option<(Nfsfh3, Fattr3)> {
    self
      .with_export(volume, subject, rights, |export| export.root_object())
      .flatten()
  }
}

/// The file handle of the synthetic root directory.
fn root_handle() -> Nfsfh3 {
  FileHandle {
    volume: ROOT_VOLUME,
    inode: ROOT_INODE,
    generation: 0,
  }
  .to_fh()
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

/// The volume id a request's leading file handle names (every served NFSv3 procedure begins with one),
/// for a router that decides which shard owns the request before serving it. `None` if no valid handle
/// leads the request; the synthetic root's own handle yields [`root_volume`].
pub fn request_volume(args: &XdrReader<'_>) -> Option<VolumeId> {
  peek_handle_volume(args)
}

/// The reserved volume id of the synthetic root directory (all-zero). A handle carrying it names the
/// root, not any volume, so a multi-shard router serves it locally rather than routing it to an owner.
pub fn root_volume() -> VolumeId {
  ROOT_VOLUME
}

/// A `NFS3ERR_STALE` reply for a handle whose volume this set does not hold, in the failing
/// procedure's own reply shape. The inode is not here, so the object the handle named is gone (D-4:
/// inode numbers are never reused), which is exactly stale.
fn stale_for(procedure: u32) -> Vec<u8> {
  status_only_or_wcc(procedure, Nfsstat3::Stale)
}

/// A `NFS3ERR_BADHANDLE` reply when the leading handle does not parse, in the procedure's reply shape.
fn badhandle_for(procedure: u32) -> Vec<u8> {
  status_only_or_wcc(procedure, Nfsstat3::Badhandle)
}

/// Encodes `status` in the failing procedure's reply shape (its optional attributes all absent), so a
/// routing-level refusal keeps the stream framed exactly as the per-volume export's own refusals do.
fn status_only_or_wcc(procedure: u32, status: Nfsstat3) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  status.encode(&mut writer);
  match procedure {
    // A leading `post_op_attr` (absent): GETATTR has none; these answer status + one post_op_attr.
    NFSPROC3_LOOKUP | NFSPROC3_ACCESS | NFSPROC3_READLINK | NFSPROC3_READ | NFSPROC3_READDIR
    | NFSPROC3_READDIRPLUS | NFSPROC3_FSINFO | NFSPROC3_FSSTAT => {
      PostOpAttr(None).encode(&mut writer);
    }
    // The create family: post_op_fh (absent), post_op_attr (absent), dir wcc (absent).
    NFSPROC3_CREATE | NFSPROC3_MKDIR | NFSPROC3_SYMLINK => {
      writer.bool(false);
      writer.bool(false);
      write_absent_wcc(&mut writer);
    }
    NFSPROC3_RENAME => {
      write_absent_wcc(&mut writer);
      write_absent_wcc(&mut writer);
    }
    // SETATTR, WRITE, REMOVE, RMDIR, COMMIT: one wcc_data. (COMMIT's success adds a verifier, but a
    // failure — a stale or bad handle at the routing edge — is the bare wcc_data, RFC 1813 §3.3.21.)
    NFSPROC3_SETATTR | NFSPROC3_WRITE | NFSPROC3_REMOVE | NFSPROC3_RMDIR | NFSPROC3_COMMIT
    | NFSPROC3_MKNOD => {
      write_absent_wcc(&mut writer);
    }
    // LINK: the linked-to file's post_op_attr (absent), then the directory's wcc_data (absent).
    NFSPROC3_LINK => {
      PostOpAttr(None).encode(&mut writer);
      write_absent_wcc(&mut writer);
    }
    // GETATTR and anything else: the status alone.
    _ => {}
  }
  writer.into_bytes()
}

/// Encodes one root-listing entry (a mounted volume) as a READDIR `entry3` or a READDIRPLUS
/// `entryplus3`. The result is four-byte aligned (every XDR field is), so it appends to the reply
/// with no extra padding. The resume `cookie` is one past this entry's index.
fn encode_readdir_entry(
  name: &str,
  fileid: u64,
  index: usize,
  plus: bool,
  plus_object: Option<(Nfsfh3, Fattr3)>,
) -> Vec<u8> {
  let mut entry = XdrWriter::new();
  entry.bool(true); // an entry follows
  entry.u64(fileid);
  entry.opaque(name.as_bytes());
  entry.u64(u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1)); // resume cookie
  if plus {
    let attr = plus_object.as_ref().map(|(_, attr)| *attr);
    PostOpAttr(attr).encode(&mut entry); // name_attributes
    match plus_object {
      Some((handle, _)) => {
        entry.bool(true); // name_handle follows
        handle.encode(&mut entry);
      }
      None => entry.bool(false),
    }
  }
  entry.into_bytes()
}

/// A `LOOKUP3res` failure: the status then the directory's `post_op_attr`.
fn lookup_failure(status: Nfsstat3, dir_attr: Option<Fattr3>) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  status.encode(&mut writer);
  PostOpAttr(dir_attr).encode(&mut writer);
  writer.into_bytes()
}

/// A `READDIR3res`/`READDIRPLUS3res` failure: the status then the directory's `post_op_attr`.
fn readdir_failure(status: Nfsstat3, dir_attr: Fattr3) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  status.encode(&mut writer);
  PostOpAttr(Some(dir_attr)).encode(&mut writer);
  writer.into_bytes()
}

/// A failure reply that is a status then a single `post_op_attr` (READ and READLINK share the shape).
fn post_attr_failure(status: Nfsstat3, attr: Option<Fattr3>) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  status.encode(&mut writer);
  PostOpAttr(attr).encode(&mut writer);
  writer.into_bytes()
}

/// A read-only refusal (`NFS3ERR_ROFS`) for a mutation on the root, encoded in the failing procedure's
/// own reply shape so the stream stays framed: the create family carries `post_op_fh` + object
/// `post_op_attr` + directory `wcc_data`, RENAME carries two `wcc_data`, LINK carries the file's
/// `post_op_attr` + one `wcc_data`, the rest one `wcc_data`. Every optional here is "absent" (a single
/// `false`), which is a valid, minimal `wcc_data`/`post_op_*`.
fn root_readonly_refusal(procedure: u32) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  Nfsstat3::Rofs.encode(&mut writer);
  match procedure {
    NFSPROC3_CREATE | NFSPROC3_MKDIR | NFSPROC3_SYMLINK => {
      writer.bool(false); // post_op_fh: no handle
      writer.bool(false); // post_op_attr: no object attributes
      write_absent_wcc(&mut writer); // dir_wcc
    }
    NFSPROC3_RENAME => {
      write_absent_wcc(&mut writer); // fromdir_wcc
      write_absent_wcc(&mut writer); // todir_wcc
    }
    NFSPROC3_LINK => {
      PostOpAttr(None).encode(&mut writer); // file_attributes: none
      write_absent_wcc(&mut writer); // linkdir_wcc
    }
    // SETATTR, WRITE, REMOVE, RMDIR: one wcc_data.
    _ => write_absent_wcc(&mut writer),
  }
  writer.into_bytes()
}

/// An absent `wcc_data` (RFC 1813 §2.6): no before attributes and no after attributes.
fn write_absent_wcc(writer: &mut XdrWriter) {
  writer.bool(false); // before: absent
  writer.bool(false); // after: absent
}
