//! The one [`Bridge`] implementation over the volume core (§4.6 "one implementation in the
//! core"). A [`VolumeBridge`] borrows a `Volume` and its `Store` and turns a transport's
//! requests, by real inode number, into volume operations. Inode number is the volume's own
//! (node id 1 is a FUSE convention resolved to [`VolumeBridge::root`] at the FUSE edge, not here).
//! File handles are a small counter into a table naming the inode they were opened on, so a read
//! or write finds its inode in O(1). Nothing here writes a host path; a scratch volume lives
//! entirely in RAM, so this is exercised on every host without a mount or a bridge transport, and
//! every transport shares the semantics it proves.

use slates_base::OsHost;
use slates_db::catalog::VolumeId;
use slates_mem::{Handle, Slab};
use slates_vfs::error::VfsError;
use slates_vfs::inode::{Attrs, Kind};
use slates_vfs::volume::{Store, Volume};

use crate::{Bridge, DirEntry, FsStat, NodeAttr, ObjectId, OpContext, RenameFlags, SetAttr, View};

/// Shape: the read cap, one arena chunk (256 KiB), matched to the INIT negotiation; here it
/// bounds a single reply buffer.
const MAX_READ: usize = 256 * 1024;
/// Shape: the block size reported in filesystem statistics: one page, the volume core's chunk unit.
const BLOCK_SIZE: u32 = 4096;
/// Format: the maximum name length the volume core allows (§4.5's name cap).
const NAME_MAX: u32 = 255;
/// Shape: the bound on concurrently open handles per bridge — more than any realistic
/// concurrent-open working set (a large build holds a few thousand files open at once), few
/// enough that the handle table stays about a mebibyte, so a runaway is a typed
/// `MemError::SlabFull` refusal rather than unbounded growth (audit BUG-4). The precise
/// per-volume budget-derived cap is owed to the §4.2 admission wiring (GAP-A9-1).
const MAX_OPEN_HANDLES: usize = 1 << 16;
/// Shape: the handle slab's segment size — about one page of slots, so the table grows a page at
/// a time up to [`MAX_OPEN_HANDLES`] and an idle bridge holds one small segment.
const HANDLE_SEGMENT: usize = 256;

/// The `Bridge` over one volume.
pub struct VolumeBridge<'v> {
  /// The id of the volume this bridge serves. A request's [`OpContext`] names the volume its
  /// attachment binds; the seam refuses one that does not match this, so an attachment for another
  /// volume can never act here (the [`OpContext`] volume invariant).
  volume_id: VolumeId,
  volume: &'v mut Volume,
  store: &'v mut Store,
  /// The read-only host of the base directory, for an overlay volume; `None` for a scratch
  /// volume. Base entries (§4.5) are looked up, listed, stat-ed and read through it.
  host: Option<OsHost>,
  /// Open handles: a bounded generational slab whose value is the inode the handle names (files
  /// and dirs share one space; a transport never confuses them). A released handle's slot is
  /// reused and its generation bumped, so repeated open/close does not grow memory (audit BUG-4)
  /// and a stale handle is a typed miss, never a wrong inode. The wire handle packs the slot index
  /// and generation into one word.
  handles: Slab<u64>,
  /// Shape: the size a read is capped at when a request asks for more than one arena chunk.
  max_read: usize,
}

impl std::fmt::Debug for VolumeBridge<'_> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("VolumeBridge")
      .field("open_handles", &self.handles.len())
      .finish()
  }
}

impl<'v> VolumeBridge<'v> {
  /// A bridge over the volume `volume_id` names, backed by `volume` and its `store`.
  pub fn new(
    volume_id: VolumeId,
    volume: &'v mut Volume,
    store: &'v mut Store,
  ) -> VolumeBridge<'v> {
    VolumeBridge {
      volume_id,
      volume,
      store,
      host: None,
      handles: Slab::new(HANDLE_SEGMENT, MAX_OPEN_HANDLES),
      max_read: MAX_READ,
    }
  }

  /// A bridge over an overlay `volume` (the volume `volume_id` names) whose base is served through
  /// `host` (§4.5, §4.6 "Base files"): untouched base entries are looked up, listed, stat-ed and
  /// read from the disk.
  pub fn with_base(
    volume_id: VolumeId,
    volume: &'v mut Volume,
    store: &'v mut Store,
    host: OsHost,
  ) -> VolumeBridge<'v> {
    VolumeBridge {
      volume_id,
      volume,
      store,
      host: Some(host),
      handles: Slab::new(HANDLE_SEGMENT, MAX_OPEN_HANDLES),
      max_read: MAX_READ,
    }
  }

  /// Refuses unless `cx` authorizes a read on this bridge's volume: its attachment must bind this
  /// volume and carry read rights. The attachment's liveness and epoch were already checked when
  /// the owner built the context ([`crate::Attachments::context`]); this is the object-side half —
  /// the volume the context binds must be the one this bridge serves, and the operation must be
  /// within the granted rights.
  fn authorize_read(&self, cx: &OpContext) -> Result<(), VfsError> {
    if cx.volume != self.volume_id {
      return Err(VfsError::NotPermitted);
    }
    if !cx.rights.read {
      return Err(VfsError::NotPermitted);
    }
    Ok(())
  }

  /// Refuses unless `cx` authorizes a write on this bridge's volume: its attachment must bind this
  /// volume, carry write rights, and view the current head. A write against a read-only attachment
  /// or a pinned immutable view is refused before any effect (§4.6 EROFS/EACCES).
  fn authorize_write(&self, cx: &OpContext) -> Result<(), VfsError> {
    if cx.volume != self.volume_id {
      return Err(VfsError::NotPermitted);
    }
    if !cx.rights.write {
      return Err(VfsError::NotPermitted);
    }
    if !matches!(cx.view, View::Current) {
      return Err(VfsError::NotPermitted);
    }
    Ok(())
  }

  /// Refuses unless `cx`'s attachment binds this bridge's volume. The lifecycle operations
  /// (release, forget, flush) need no particular right — dropping a handle or a lookup reference is
  /// always the holder's to do — but they must still name this volume, so a context for another
  /// volume can never touch its handle table or references.
  fn authorize_volume(&self, cx: &OpContext) -> Result<(), VfsError> {
    if cx.volume != self.volume_id {
      return Err(VfsError::NotPermitted);
    }
    Ok(())
  }

  /// Assigns a handle naming `inode`, or a typed refusal (`MemError::SlabFull`) once the bridge
  /// already holds [`MAX_OPEN_HANDLES`] (audit BUG-4). The wire handle packs the slot and its
  /// generation.
  fn open_handle(&mut self, inode: u64) -> Result<u64, VfsError> {
    // An open takes an open reference on the inode (dropped by release), so a file open across an
    // unlink keeps its content until the last reference. Reference first; undo it if the slot
    // cannot be allocated, so the count never leaks (the lifecycle rule).
    self
      .volume
      .reference(self.store, slates_vfs::ids::InodeNo(inode))?;
    match self.handles.insert(inode) {
      Ok(handle) => Ok(pack_handle(handle)),
      Err(e) => {
        let _ = self
          .volume
          .unreference(self.store, slates_vfs::ids::InodeNo(inode));
        Err(e.into())
      }
    }
  }

  /// The neutral attributes of inode `no`: its stat (through the host for an overlay's base
  /// entry) and its kind (structural, always in the store).
  fn attr_of(&mut self, no: u64) -> Result<NodeAttr, VfsError> {
    let inode = slates_vfs::ids::InodeNo(no);
    let attrs = match self.host.as_mut() {
      Some(host) => self.volume.with_host(host).stat(self.store, inode),
      None => self.volume.stat(self.store, inode),
    }?;
    let kind = self.volume.kind(self.store, inode)?;
    Ok(node_attr(no, kind, &attrs))
  }
}

/// A neutral [`NodeAttr`] from the volume's attributes, kind and inode number. Generation is 0
/// until generation-tracked reuse lands (§4.6 `(no, gen)`).
fn node_attr(no: u64, kind: Kind, attrs: &Attrs) -> NodeAttr {
  NodeAttr {
    ino: no,
    generation: 0,
    kind,
    mode: attrs.mode,
    nlink: attrs.nlink,
    uid: attrs.uid,
    gid: attrs.gid,
    size: attrs.size,
    atime: attrs.atime,
    mtime: attrs.mtime,
    ctime: attrs.ctime,
  }
}

/// Packs a slab handle into one wire word: the slot index in the high half, the generation in the
/// low half. The inverse is [`unpack_handle`]; the slab's generation check refuses a stale word.
fn pack_handle(handle: Handle<u64>) -> u64 {
  (u64::from(handle.index()) << u32::BITS) | u64::from(handle.generation())
}

/// Unpacks a wire word into a slab handle.
fn unpack_handle(word: u64) -> Handle<u64> {
  let index = u32::try_from(word >> u32::BITS).unwrap_or(u32::MAX);
  let generation = u32::try_from(word & u64::from(u32::MAX)).unwrap_or(u32::MAX);
  Handle::from_raw(index, generation)
}

impl Bridge for VolumeBridge<'_> {
  fn root(&mut self, cx: &OpContext) -> Result<u64, VfsError> {
    self.authorize_read(cx)?;
    self.volume.root_inode(self.store).map(|no| no.0)
  }

  fn lookup(&mut self, parent: ObjectId, cx: &OpContext, name: &str) -> Result<NodeAttr, VfsError> {
    self.authorize_read(cx)?;
    let located =
      self
        .volume
        .lookup_no(self.store, slates_vfs::ids::InodeNo(parent.inode), name)?;
    let attr = self.attr_of(located.inode.0)?;
    // No implicit reference: a transport that owns lookup references (FUSE) takes one explicitly
    // through `reference`; NFS takes none (§3). Fixes the NFS lookup-reference leak.
    Ok(attr)
  }

  fn getattr(&mut self, object: ObjectId, cx: &OpContext) -> Result<NodeAttr, VfsError> {
    self.authorize_read(cx)?;
    self.attr_of(object.inode)
  }

  fn open(&mut self, object: ObjectId, cx: &OpContext, _flags: u32) -> Result<u64, VfsError> {
    self.authorize_read(cx)?;
    // A directory is opened through opendir; open refuses it.
    if self
      .volume
      .kind(self.store, slates_vfs::ids::InodeNo(object.inode))?
      == Kind::Dir
    {
      return Err(VfsError::IsDirectory);
    }
    self.open_handle(object.inode)
  }

  fn read(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    offset: u64,
    size: u32,
    out: &mut Vec<u8>,
  ) -> Result<(), VfsError> {
    self.authorize_read(cx)?;
    let want = usize::try_from(size).unwrap_or(0).min(self.max_read);
    let mut buf = vec![0u8; want];
    let inode = slates_vfs::ids::InodeNo(object.inode);
    let read = match self.host.as_mut() {
      Some(host) => self
        .volume
        .with_host(host)
        .read(self.store, inode, offset, &mut buf),
      None => self.volume.read(self.store, inode, offset, &mut buf),
    }?;
    out.extend_from_slice(&buf[..read]);
    Ok(())
  }

  fn write(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    offset: u64,
    data: &[u8],
  ) -> Result<u32, VfsError> {
    // A write against a read-only attachment, or a pinned immutable view, is refused before any
    // effect (§4.6 EROFS/EACCES; the precise errno per case is a taxonomy refinement).
    self.authorize_write(cx)?;
    let inode = slates_vfs::ids::InodeNo(object.inode);
    // An overlay write copies the base up first (through the host); a scratch write does not.
    let written = match self.host.as_mut() {
      Some(host) => self
        .volume
        .with_host(host)
        .write(self.store, inode, offset, data),
      None => self.volume.write(self.store, inode, offset, data),
    }?;
    u32::try_from(written).map_err(|_| VfsError::FileTooLarge)
  }

  fn opendir(&mut self, object: ObjectId, cx: &OpContext) -> Result<u64, VfsError> {
    self.authorize_read(cx)?;
    if self
      .volume
      .kind(self.store, slates_vfs::ids::InodeNo(object.inode))?
      != Kind::Dir
    {
      return Err(VfsError::NotDirectory);
    }
    self.open_handle(object.inode)
  }

  fn readdir(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    _fh: u64,
    offset: u64,
  ) -> Result<Vec<DirEntry>, VfsError> {
    self.authorize_read(cx)?;
    let dir_no = slates_vfs::ids::InodeNo(object.inode);
    // The parent for `..`; the root has none, so `..` is the root itself (POSIX). Resolved before
    // the listing (a separate read); a directory whose parent cannot be resolved falls back to
    // itself rather than failing the whole listing.
    let parent = self
      .volume
      .parent_no(self.store, dir_no)
      .unwrap_or(dir_no)
      .0;
    let rows = match self.host.as_mut() {
      Some(host) => self.volume.with_host(host).readdir_no(self.store, dir_no),
      None => self.volume.readdir_no(self.store, dir_no),
    }?;
    // POSIX `readdir` lists "." (the directory) and ".." (its parent) before the children; the
    // volume core returns children only, so the shared bridge synthesizes them here — one code
    // path, so the FUSE mount and the NFS export list them identically (R8). The cookie/offset is
    // over the full list, so ".".and "..".are positions 0 and 1 and a resume skips them.
    let dot = DirEntry {
      ino: object.inode,
      kind: Kind::Dir,
      name: ".".to_owned(),
    };
    let dotdot = DirEntry {
      ino: parent,
      kind: Kind::Dir,
      name: "..".to_owned(),
    };
    let children = rows.into_iter().map(|row| DirEntry {
      ino: row.inode.0,
      kind: row.kind,
      name: row.name.to_owned(),
    });
    let start = usize::try_from(offset).unwrap_or(0);
    Ok(
      [dot, dotdot]
        .into_iter()
        .chain(children)
        .skip(start)
        .collect(),
    )
  }

  fn create(
    &mut self,
    parent: ObjectId,
    cx: &OpContext,
    name: &str,
    mode: u32,
    _flags: u32,
  ) -> Result<(NodeAttr, u64), VfsError> {
    self.authorize_write(cx)?;
    let no = self.volume.create_file_no(
      self.store,
      slates_vfs::ids::InodeNo(parent.inode),
      name,
      mode,
    )?;
    let attrs = self.volume.stat(self.store, no)?;
    let entry = node_attr(no.0, Kind::File, &attrs);
    // A create takes only an open reference (dropped by release); it does not implicitly take a
    // lookup reference — a transport that owns lookup references (FUSE) takes one through
    // `reference`, NFS takes none (§3). `open_handle` references then allocates, undoing on failure.
    let fh = self.open_handle(no.0)?;
    Ok((entry, fh))
  }

  fn release(&mut self, _object: ObjectId, cx: &OpContext, fh: u64) -> Result<(), VfsError> {
    self.authorize_volume(cx)?;
    // Drop the open reference the handle held, then free the slot for reuse. An unknown or
    // already-freed handle is a no-op (the kernel may release one the bridge already dropped).
    if let Ok(inode) = self.handles.get(unpack_handle(fh)).copied() {
      let _ = self
        .volume
        .unreference(self.store, slates_vfs::ids::InodeNo(inode));
    }
    let _ = self.handles.remove(unpack_handle(fh));
    Ok(())
  }

  fn reference(&mut self, object: ObjectId, cx: &OpContext) -> Result<(), VfsError> {
    self.authorize_read(cx)?;
    // Takes one lookup reference on the object (the FUSE edge calls this; NFS does not, §3). An
    // inode a transport still references keeps its content and table entry across an unlink until
    // the last reference drops.
    self
      .volume
      .reference(self.store, slates_vfs::ids::InodeNo(object.inode))
  }

  fn forget(&mut self, object: ObjectId, cx: &OpContext, nlookup: u64) {
    // A forget for another volume's context touches nothing here (it cannot signal an error, so it
    // is a safe no-op). Drop the lookup references the transport held; at the last reference an
    // already-unlinked inode is reclaimed (§4.6 the reference model). A bulk forget is one bounded
    // step.
    if cx.volume != self.volume_id {
      return;
    }
    let _ = self
      .volume
      .unreference_n(self.store, slates_vfs::ids::InodeNo(object.inode), nlookup);
  }

  fn flush(&mut self, _object: ObjectId, cx: &OpContext, _fh: u64) -> Result<(), VfsError> {
    self.authorize_read(cx)?;
    // No disk write: the data is already in the anchor segment (§4.6). Success.
    Ok(())
  }

  fn mkdir(
    &mut self,
    parent: ObjectId,
    cx: &OpContext,
    name: &str,
    mode: u32,
  ) -> Result<NodeAttr, VfsError> {
    self.authorize_write(cx)?;
    let no = self.volume.mkdir_no(
      self.store,
      slates_vfs::ids::InodeNo(parent.inode),
      name,
      mode,
    )?;
    let attrs = self.volume.stat(self.store, no)?;
    let attr = node_attr(no.0, Kind::Dir, &attrs);
    // No implicit lookup reference (see `lookup`): FUSE takes one through `reference`, NFS none.
    Ok(attr)
  }

  fn unlink(&mut self, parent: ObjectId, cx: &OpContext, name: &str) -> Result<(), VfsError> {
    self.authorize_write(cx)?;
    self
      .volume
      .unlink_no(self.store, slates_vfs::ids::InodeNo(parent.inode), name)
  }

  fn rmdir(&mut self, parent: ObjectId, cx: &OpContext, name: &str) -> Result<(), VfsError> {
    self.authorize_write(cx)?;
    self
      .volume
      .rmdir_no(self.store, slates_vfs::ids::InodeNo(parent.inode), name)
  }

  fn symlink(
    &mut self,
    parent: ObjectId,
    cx: &OpContext,
    name: &str,
    target: &str,
  ) -> Result<NodeAttr, VfsError> {
    self.authorize_write(cx)?;
    let no = self.volume.symlink_no(
      self.store,
      slates_vfs::ids::InodeNo(parent.inode),
      name,
      target,
    )?;
    let attrs = self.volume.stat(self.store, no)?;
    let attr = node_attr(no.0, Kind::Symlink, &attrs);
    // No implicit lookup reference (see `lookup`): FUSE takes one through `reference`, NFS none.
    Ok(attr)
  }

  fn readlink(&mut self, object: ObjectId, cx: &OpContext) -> Result<String, VfsError> {
    self.authorize_read(cx)?;
    self
      .volume
      .readlink(self.store, slates_vfs::ids::InodeNo(object.inode))
      .map(|t| t.into_string())
  }

  fn rename(
    &mut self,
    old_parent: ObjectId,
    new_parent: ObjectId,
    cx: &OpContext,
    old_name: &str,
    new_name: &str,
    flags: RenameFlags,
  ) -> Result<(), VfsError> {
    self.authorize_write(cx)?;
    // EXCHANGE (atomically swap two existing entries) is not yet expressible over the volume
    // core; it is refused, never silently downgraded to a plain rename (audit BUG-10). `EINVAL`
    // is the errno `renameat2` itself returns where a flag is unsupported (D-26).
    if flags.exchange {
      return Err(VfsError::Invalid);
    }
    let to_dir = slates_vfs::ids::InodeNo(new_parent.inode);
    // NOREPLACE must fail if the destination exists rather than replacing it. The owning shard
    // runs one operation at a time, so this check and the rename are atomic against other work.
    if flags.no_replace && self.volume.lookup_no(self.store, to_dir, new_name).is_ok() {
      return Err(VfsError::AlreadyExists);
    }
    self.volume.rename_no(
      self.store,
      slates_vfs::ids::InodeNo(old_parent.inode),
      old_name,
      to_dir,
      new_name,
    )
  }

  fn setattr(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    changes: SetAttr,
  ) -> Result<NodeAttr, VfsError> {
    self.authorize_write(cx)?;
    let ino = object.inode;
    let inode = slates_vfs::ids::InodeNo(ino);
    if let Some(size) = changes.size {
      self.volume.truncate(self.store, inode, size)?;
    }
    if let Some(mode) = changes.mode {
      self.volume.chmod(self.store, inode, mode)?;
    }
    // Ownership and times honor each requested field, filling the unset half of a pair from the
    // current attributes, so setting only the uid (or only the mtime) leaves the other unchanged;
    // an ignored field is never acknowledged (§4.6; audit BUG-8).
    if changes.uid.is_some() || changes.gid.is_some() {
      let current = self.volume.stat(self.store, inode)?;
      self.volume.chown(
        self.store,
        inode,
        changes.uid.unwrap_or(current.uid),
        changes.gid.unwrap_or(current.gid),
      )?;
    }
    if changes.atime.is_some() || changes.mtime.is_some() {
      let current = self.volume.stat(self.store, inode)?;
      self.volume.set_times(
        self.store,
        inode,
        changes.atime.unwrap_or(current.atime),
        changes.mtime.unwrap_or(current.mtime),
      )?;
    }
    self.attr_of(ino)
  }

  fn now(&mut self) -> i64 {
    self.volume.wall_ns()
  }

  fn change_token(&mut self, object: ObjectId, cx: &OpContext) -> Result<u64, VfsError> {
    self.authorize_read(cx)?;
    self
      .volume
      .change_version(self.store, slates_vfs::ids::InodeNo(object.inode))
  }

  fn statfs(&mut self, _object: ObjectId, cx: &OpContext) -> Result<FsStat, VfsError> {
    self.authorize_read(cx)?;
    let accounting = self.volume.accounting();
    // Blocks are the volume's referenced bytes over the block size; the volume core does not
    // expose a hard cap here (a dynamic volume grows), so free is reported generously and the
    // quota is enforced on write, not by statfs. A transport uses this only for `df`.
    let block = u64::from(BLOCK_SIZE);
    let used = accounting.referenced_bytes.div_ceil(block);
    Ok(FsStat {
      blocks: used.saturating_mul(2).max(1),
      bfree: used,
      bavail: used,
      files: 0,
      ffree: 0,
      bsize: BLOCK_SIZE,
      namelen: NAME_MAX,
      frsize: BLOCK_SIZE,
    })
  }
}
