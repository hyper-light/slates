//! The one [`Bridge`] implementation over the volume core (§4.6 "one implementation in the
//! core"). A [`VolumeBridge`] borrows a `Volume` and its `Store` and turns a transport's
//! requests, by real inode number, into volume operations. Inode number is the volume's own
//! (node id 1 is a FUSE convention resolved to [`VolumeBridge::root`] at the FUSE edge, not here).
//! File handles are a small counter into a table naming the inode they were opened on, so a read
//! or write finds its inode in O(1). Nothing here writes a host path; a scratch volume lives
//! entirely in RAM, so this is exercised on every host without a mount or a bridge transport, and
//! every transport shares the semantics it proves.
//!
//! For an overlay volume every verb — reads and writes, and every namespace or metadata mutation —
//! goes through the base plane's `Overlay` (§4.6 "Base lookups and metadata mutations go through the
//! same overlay rules as reads and writes"; AC-1.17), never the plain `Volume` verbs: a base entry
//! looked up without a prior listing is found by loading the listing on demand, a metadata change to
//! an untouched base file copies its witness up, a removed or renamed base name leaves a whiteout
//! with the listing reloaded first, and a name the base holds refuses a create. Before the sweep of
//! 2026-09-14 the mutating verbs used the plain forms, so a chmod, truncate, chown, utimes, rename
//! or link on an untouched base file recorded no witness (and the next live-disk stat undid it), a
//! rename left the base name showing beside the new one, an unlink after a watcher hint lost its
//! whiteout, and a create over a base name succeeded (`docs/bugs/2026-09-14-base-mutations-bypass-overlay.md`).
//! The host is reached through the read-only seam (`dyn HostFs`), so the base plane's simulated host
//! — outsider edits, a controllable clock, a watcher that overflows — drives this bridge in the
//! by-use tests (`tests/base_overlay.rs`) exactly as the real host does in the daemon.

mod view;

use slates_db::catalog::{Principal, VolumeId};
use slates_mem::{Handle, MemError, Slab};
use slates_vfs::error::VfsError;
use slates_vfs::host::HostFs;
use slates_vfs::ids::{InodeNo, RefOwner};
use slates_vfs::inode::{Kind, SpecialNode};

use slates_vfs::journal::Op;
use slates_vfs::volume::{Observed, Store, Volume};

use crate::access::S_ISGID;
use crate::posix_acl::{Acl, MODE_PERMISSION_BITS};
use crate::{
  Bridge, CacheLifetime, CachedBytes, DirEntry, FsStat, Invalidation, InvalidationCursor, NodeAttr,
  ObjectId, OpContext, POSIX_ACL_ACCESS, POSIX_ACL_DEFAULT, RenameFlags, SetAttr, View,
};

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
/// per-volume budget-derived cap is owed to the §4.2 admission wiring (GAP-A9-1). Public because
/// what a transport keeps per open handle is bounded by it too (the WinFsp mount's purges owed
/// again, `slates_bridge_winfsp::retry`).
pub const MAX_OPEN_HANDLES: usize = 1 << 16;
/// Shape: the handle slab's segment size — about one page of slots, so the table grows a page at
/// a time up to [`MAX_OPEN_HANDLES`] and an idle bridge holds one small segment.
const HANDLE_SEGMENT: usize = 256;

/// A transport's write log (A-410): handed the inode, the offset and the bytes a write took, it says whether it kept
/// them.
pub type WriteLogger<'v> = &'v mut dyn FnMut(u64, u64, &[u8]) -> bool;

/// The `Bridge` over one volume.
pub struct VolumeBridge<'v> {
  /// The id of the volume this bridge serves. A request's [`OpContext`] names the volume its
  /// attachment binds; the seam refuses one that does not match this, so an attachment for another
  /// volume can never act here (the [`OpContext`] volume invariant).
  volume_id: VolumeId,
  volume: &'v mut Volume,
  store: &'v mut Store,
  /// The read-only host of the base directory, for an overlay volume; `None` for a scratch
  /// volume. Base entries (§4.5) are looked up, listed, stat-ed, read and mutated through it. Owned
  /// when the bridge lives for the mount (FUSE, NFS); borrowed when it is rebuilt per request (the
  /// daemon's serve paths); see [`HostRef`].
  host: HostRef<'v>,
  /// Open handles: a bounded generational slab whose value is the inode the handle names (files
  /// and dirs share one space; a transport never confuses them). A released handle's slot is
  /// reused and its generation bumped, so repeated open/close does not grow memory (audit BUG-4)
  /// and a stale handle is a typed miss, never a wrong inode. The wire handle packs the slot index
  /// and generation into one word. Owned when the bridge lives for the mount (FUSE, NFS); borrowed
  /// from the caller when the bridge is rebuilt per request (the daemon FSKit path), so the map
  /// survives across requests in the owning shard's mount session (see [`HandleStore`]).
  handles: HandleStore<'v>,
  /// Shape: the size a read is capped at when a request asks for more than one arena chunk.
  max_read: usize,
  /// For a mount taken over from a daemon that died (A-61), the files whose acknowledged writes the death lost:
  /// a `flush` or `fsync` of one through a handle the dead daemon issued answers `EIO` once ([`LostWrites`]).
  lost: Option<&'v mut LostWrites>,
  /// Whether the transport serving through this bridge offers its guest per-inode DAX ([`crate::DaxOffer`]): withheld
  /// unless the Shards route's serve task says otherwise ([`VolumeBridge::offering_dax`]).
  dax: crate::DaxOffer,
  /// How the transport shows its guest the volume's owners ([`crate::OwnerView`]): the identity unless the Shards route's
  /// serve task says otherwise ([`VolumeBridge::viewing_owners`]; A-281).
  owners: crate::OwnerView,
  /// Which attribute namespaces the transport reaches ([`crate::AttributeNamespaces`]): a host's mount's unless a guest
  /// kernel's route says otherwise ([`VolumeBridge::serving_a_guest`]; A-290).
  namespaces: crate::AttributeNamespaces,
  /// Whether the transport's kernel sends the extended `fuse_setxattr_in` (A-378), as the transport last said
  /// ([`Bridge::set_setxattr_ext`]): a guest connection that kept `FUSE_SETXATTR_EXT`.
  setxattr_ext: bool,
  /// Where a transport whose writes outlive its daemon only through the shard's write log keeps each write this bridge
  /// takes (A-410; [`VolumeBridge::logging_writes`]): handed the inode, the offset and the bytes written, it says
  /// whether it kept them. None for a transport that keeps its writes otherwise, or not at all.
  write_log: Option<WriteLogger<'v>>,
  /// Whether a write this bridge took was not kept by its [`VolumeBridge::logging_writes`] log, so the reply must wait
  /// for the publication that carries it ([`Bridge::awaits_publication`]; A-410).
  write_unlogged: bool,
}

/// The files whose acknowledged writes a daemon's death lost, for a mount the anchor held across it (A-61;
/// `docs/wip/SLATES_DESIGN.md` §4.6). A plain write is acknowledged before the owner's barrier (D-18), so the
/// writes since the last publication died with the daemon while the writer ran on. Linux reports such a loss as
/// a writeback error: an `fsync` of a file description open at the time answers `EIO` once, and a close may
/// answer it too. So does this: a handle the dead daemon issued — one this daemon never did — that names a lost
/// file answers every `flush` `EIO` until an `fsync` through it has been answered `EIO`, once; after that both
/// succeed. A `flush` comes with every close of any copy of the descriptor (a child's exec-time close of an
/// inherited copy included), so it reports without consuming, or a close nobody saw would take the report the
/// writer's own `fsync` needed (measured 2026-10-03: the in-flight takeover test's writer `fsync`ed clean after
/// a spawned process's close had taken it). A handle opened after the takeover sees no error, its writes being
/// this daemon's. Bounded by the handles alive: the handles this daemon issued leave at their release, and a
/// told handle's entries with it.
#[derive(Debug, Default)]
pub struct LostWrites {
  inodes: std::collections::BTreeSet<u64>,
  everything: bool,
  /// The handles this daemon issued, each with its file: a successor reissues its predecessor's values, so a value
  /// alone cannot tell a handle from before the takeover from one issued since (A-148).
  issued: std::collections::BTreeSet<(u64, u64)>,
  told: std::collections::BTreeSet<(u64, u64)>,
}

impl LostWrites {
  /// The files `inodes` lost writes, or every file when `everything` (the record of them overflowed).
  pub fn new(inodes: std::collections::BTreeSet<u64>, everything: bool) -> LostWrites {
    LostWrites {
      inodes,
      everything,
      issued: std::collections::BTreeSet::new(),
      told: std::collections::BTreeSet::new(),
    }
  }

  /// Whether no file lost writes.
  pub fn is_empty(&self) -> bool {
    self.inodes.is_empty() && !self.everything
  }

  fn issued(&mut self, fh: u64, inode: u64) {
    if !self.is_empty() {
      self.issued.insert((fh, inode));
    }
  }

  fn released(&mut self, fh: u64, inode: u64) {
    self.issued.remove(&(fh, inode));
    self.told.remove(&(inode, fh));
  }

  /// Whether a `flush` or `fsync` of `inode` through `fh` is answered `EIO`: the handle predates the takeover, the
  /// file lost writes, and no `fsync` through this handle has been told yet.
  fn reports(&self, inode: u64, fh: u64) -> bool {
    !self.issued.contains(&(fh, inode))
      && (self.everything || self.inodes.contains(&inode))
      && !self.told.contains(&(inode, fh))
  }

  /// [`LostWrites::reports`] for an `fsync`, which consumes the report.
  fn reports_and_tells(&mut self, inode: u64, fh: u64) -> bool {
    self.reports(inode, fh) && self.told.insert((inode, fh))
  }
}

/// Format: the access-mode bits of an open's flags (`O_ACCMODE`), as the FUSE protocol carries them (Linux `O_*`).
const OPEN_ACCESS_MODE: u32 = 0o3;
/// Format: `O_RDONLY`, the one access mode that reads alone.
const OPEN_READ_ONLY: u32 = 0;
/// Format: `O_TRUNC` (Linux `0o1000`), an open that empties the file.
const OPEN_TRUNCATE: u32 = 0o1000;

/// Whether an open with `flags` (the FUSE protocol's, Linux `O_*`; a transport without open flags passes `O_RDONLY`)
/// would change the file: a write access mode, or a truncation.
fn opens_for_writing(flags: u32) -> bool {
  flags & OPEN_ACCESS_MODE != OPEN_READ_ONLY || flags & OPEN_TRUNCATE != 0
}

impl std::fmt::Debug for VolumeBridge<'_> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("VolumeBridge")
      .field("open_handles", &self.handles.len())
      .finish()
  }
}

/// The open-handle slab: owned by the bridge, or borrowed from the caller.
///
/// A bridge that lives for the whole mount (the FUSE and NFS servers hold one) owns its slab. A bridge
/// rebuilt for each request owns nothing durable — the daemon's FSKit serve path is like this: the
/// volume lives in the owning shard's slab and the bridge borrows it for one operation, so it cannot
/// keep persistent handle state. There the mount session owns the slab and lends it here, so open
/// handles survive across requests. The open *reference* each handle takes lives in the `Volume` (which
/// persists regardless); only this `fh`→inode map needed a home outside the transient bridge.
enum HandleStore<'h> {
  Owned(Slab<u64>),
  Borrowed(&'h mut Slab<u64>),
}

impl HandleStore<'_> {
  fn insert(&mut self, inode: u64) -> Result<Handle<u64>, MemError> {
    match self {
      Self::Owned(slab) => slab.insert(inode),
      Self::Borrowed(slab) => slab.insert(inode),
    }
  }
  fn get(&self, handle: Handle<u64>) -> Result<&u64, MemError> {
    match self {
      Self::Owned(slab) => slab.get(handle),
      Self::Borrowed(slab) => slab.get(handle),
    }
  }
  fn remove(&mut self, handle: Handle<u64>) -> Result<u64, MemError> {
    match self {
      Self::Owned(slab) => slab.remove(handle),
      Self::Borrowed(slab) => slab.remove(handle),
    }
  }
  fn len(&self) -> usize {
    match self {
      Self::Owned(slab) => slab.len(),
      Self::Borrowed(slab) => slab.len(),
    }
  }
}

/// The overlay base host, behind the read-only seam (`dyn HostFs`, §4.5): owned by the bridge (FUSE
/// and NFS hold one for the mount), borrowed from the caller (the daemon's serve paths, where the host
/// lives in the shard's volume slot and the transient bridge borrows it per request), or `None` for a
/// scratch volume. The seam, not the concrete host, so the simulated host drives the bridge in tests
/// and the real one in the daemon through one code path (R8).
enum HostRef<'h> {
  None,
  Owned(Box<dyn HostFs>),
  Borrowed(&'h mut dyn HostFs),
}

impl HostRef<'_> {
  fn as_mut(&mut self) -> Option<&mut dyn HostFs> {
    match self {
      Self::None => None,
      Self::Owned(host) => Some(host.as_mut()),
      Self::Borrowed(host) => Some(&mut **host),
    }
  }
}

/// The host a verb dispatches through: the lent or owned host when there is one; none for a scratch
/// volume; refused `BaseUnavailable` for an overlay volume served without its host (AUD-29-16 sibling).
/// Without the host, the volume layer cannot see a base name it has not materialized, so a rename,
/// create or link would shadow a disk entry and a lookup would answer "absent" for one that exists: the
/// verb refuses unchanged instead. Every verb that chooses between the overlay and the bare volume asks
/// here, so no mount path can take the hostless arm for an overlay.
fn host_for<'h>(
  host: &'h mut HostRef<'_>,
  overlay: bool,
) -> Result<Option<&'h mut dyn HostFs>, VfsError> {
  match host.as_mut() {
    None if overlay => Err(VfsError::BaseUnavailable(0)),
    served => Ok(served),
  }
}

/// The cached bytes a journal record's change made stale (A-146): exactly the bytes an overwrite or an extension wrote,
/// everything from the new end after a truncate and from the shift point after an insert or a delete, the whole file
/// for a base witness or drift (which carry no range), and none for a change that leaves the bytes alone.
fn journal_bytes(op: &Op) -> CachedBytes {
  match op {
    Op::Overwrite { at, len } | Op::Extend { at, len } => CachedBytes::Range { at: *at, len: *len },
    Op::Truncate { len } => CachedBytes::From { at: *len },
    Op::Insert { at, .. } | Op::Delete { at, .. } => CachedBytes::From { at: *at },
    Op::Witness | Op::Drift => CachedBytes::ALL,
    Op::Create
    | Op::Mknod
    | Op::Mkdir
    | Op::Unlink
    | Op::Rmdir
    | Op::Rename { .. }
    | Op::Link
    | Op::Symlink
    | Op::Setattr
    | Op::SetXattr { .. }
    | Op::RemoveXattr { .. }
    | Op::Whiteout
    | Op::Redirect { .. }
    | Op::Snapshot => CachedBytes::Kept,
  }
}

/// The file and the end of an allocation of `[offset, offset + len)` (A-108). A derived view's bytes are its owner's
/// rendering, with no holes of their own to hold, so it is refused as a special file is; a range past the largest
/// offset is `FileTooLarge`.
fn allocation_range(object: ObjectId, offset: u64, len: u64) -> Result<(InodeNo, u64), VfsError> {
  let inode = InodeNo(object.inode);
  if inode.derived_from().is_some() {
    return Err(VfsError::SpecialFileOperation);
  }
  let end = offset.checked_add(len).ok_or(VfsError::FileTooLarge)?;
  Ok((inode, end))
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
      host: HostRef::None,
      handles: HandleStore::Owned(Slab::new(HANDLE_SEGMENT, MAX_OPEN_HANDLES)),
      max_read: MAX_READ,
      lost: None,
      dax: crate::DaxOffer::Withheld,
      owners: crate::OwnerView::IDENTITY,
      namespaces: crate::AttributeNamespaces::Host,
      setxattr_ext: false,
      write_log: None,
      write_unlogged: false,
    }
  }

  /// A bridge over an overlay `volume` (the volume `volume_id` names) whose base is served through
  /// `host` (§4.5, §4.6 "Base files"): untouched base entries are looked up, listed, stat-ed, read
  /// and mutated through it.
  pub fn with_base(
    volume_id: VolumeId,
    volume: &'v mut Volume,
    store: &'v mut Store,
    host: Box<dyn HostFs>,
  ) -> VolumeBridge<'v> {
    VolumeBridge {
      volume_id,
      volume,
      store,
      host: HostRef::Owned(host),
      handles: HandleStore::Owned(Slab::new(HANDLE_SEGMENT, MAX_OPEN_HANDLES)),
      max_read: MAX_READ,
      lost: None,
      dax: crate::DaxOffer::Withheld,
      owners: crate::OwnerView::IDENTITY,
      namespaces: crate::AttributeNamespaces::Host,
      setxattr_ext: false,
      write_log: None,
      write_unlogged: false,
    }
  }

  /// A bridge whose open-handle map — and, for an overlay, whose base `host` — live *outside* it, lent
  /// by the caller across requests. This is the daemon's serve path: the volume lives in the owning
  /// shard's slab and the bridge is rebuilt per request, so it cannot own persistent handle state or
  /// the base host; the mount session owns the slab, and the shard's volume slot owns the host, both
  /// lent here (see [`HandleStore`], [`HostRef`]). Pass `host: None` for a scratch volume.
  pub fn attached(
    volume_id: VolumeId,
    volume: &'v mut Volume,
    store: &'v mut Store,
    handles: &'v mut Slab<u64>,
    host: Option<&'v mut dyn HostFs>,
  ) -> VolumeBridge<'v> {
    VolumeBridge {
      volume_id,
      volume,
      store,
      host: match host {
        Some(host) => HostRef::Borrowed(host),
        None => HostRef::None,
      },
      handles: HandleStore::Borrowed(handles),
      max_read: MAX_READ,
      lost: None,
      dax: crate::DaxOffer::Withheld,
      owners: crate::OwnerView::IDENTITY,
      namespaces: crate::AttributeNamespaces::Host,
      setxattr_ext: false,
      write_log: None,
      write_unlogged: false,
    }
  }

  /// This bridge, reporting the writes `lost` names (A-61): a mount taken over from a daemon that died.
  #[must_use]
  pub fn with_lost_writes(mut self, lost: &'v mut LostWrites) -> VolumeBridge<'v> {
    self.lost = Some(lost);
    self
  }

  /// This bridge, every write it takes kept by `log` before its reply (A-410): `log` is handed the inode, the offset and
  /// the bytes the volume took, and answers whether it kept them, as the shard's write log does for slates' own virtio-fs
  /// device (A-63's rule, A-358's for a reply). One it did not keep makes the bridge owe a publication, which a transport
  /// reads as [`Bridge::awaits_publication`] and runs before the reply.
  #[must_use]
  pub fn logging_writes(mut self, log: WriteLogger<'v>) -> VolumeBridge<'v> {
    self.write_log = Some(log);
    self
  }

  /// This bridge, its transport offering its guest per-inode DAX as `dax` says (the Shards route, part 3).
  #[must_use]
  pub fn offering_dax(mut self, dax: crate::DaxOffer) -> VolumeBridge<'v> {
    self.dax = dax;
    self
  }

  /// This bridge, its transport showing its guest the volume's owners as `owners` says (the Shards route, A-281).
  #[must_use]
  pub fn viewing_owners(mut self, owners: crate::OwnerView) -> VolumeBridge<'v> {
    self.owners = owners;
    self
  }

  /// This bridge, its transport a guest kernel's (a Shards stream, slates' own virtio-fs device): it reaches the guest's
  /// own attribute namespaces as well as `user.` (A-290).
  #[must_use]
  pub fn serving_a_guest(mut self) -> VolumeBridge<'v> {
    self.namespaces = crate::AttributeNamespaces::Guest;
    self
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
    // An attachment that may read but not write, or a snapshot's view, is a read-only file system to its client, and
    // answers as one (`EROFS`, POSIX), on every transport; one with no rights at all is refused for permission. Until
    // 2026-11-08 both answered `NotPermitted`, so a read-only mount's client was told `EPERM`/`PERM`.
    if !cx.rights.write {
      return Err(if cx.rights.read {
        VfsError::ReadOnly
      } else {
        VfsError::NotPermitted
      });
    }
    if !matches!(cx.view, View::Current) {
      return Err(VfsError::ReadOnly);
    }
    Ok(())
  }

  /// The inode whose attributes an attribute verb on `object` names: the object itself. An AppleDouble
  /// view (a derived number) is rendered from attributes, and holds none of its own.
  fn xattr_owner(&self, object: ObjectId) -> Result<InodeNo, VfsError> {
    let inode = InodeNo(object.inode);
    if inode.derived_from().is_some() {
      return Err(VfsError::Invalid);
    }
    Ok(inode)
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
  fn open_handle(&mut self, inode: u64, attachment: RefOwner) -> Result<u64, VfsError> {
    // An open takes an open reference on the inode (dropped by release), attributed to the opening
    // attachment so a teardown sweep releases it. Reference first; undo it if the slot cannot be
    // allocated, so the count never leaks (the lifecycle rule).
    self
      .volume
      .reference_for(self.store, InodeNo(inode), attachment)?;
    match self.handles.insert(inode) {
      Ok(handle) => {
        let fh = pack_handle(handle);
        if let Some(lost) = self.lost.as_deref_mut() {
          lost.issued(fh, inode);
        }
        // Every handle is counted open, a file's, a created file's and a directory's alike, so the image carries each
        // one's open beside its reference and a successor gives it back at the handle's release (A-152). Until
        // 2026-10-22 only `open` counted one, so a created file's or a directory's handle left nothing to give back.
        self.volume.open_for(InodeNo(inode), attachment);
        Ok(fh)
      }
      Err(e) => {
        let _ = self
          .volume
          .forget_for(self.store, InodeNo(inode), attachment, 1);
        Err(e.into())
      }
    }
  }

  /// The neutral attributes of inode `no`: its stat (through the host for an overlay's base
  /// entry) and its kind (structural, always in the store).
  /// Gives inode `inode` the Windows attribute bits a Win32 word `bits` asks (A-262), those
  /// [`slates_vfs::inode::WindowsAttributes::kept`] keeps for its kind; an untouched base file is copied up first, its
  /// metadata alone, as a `chmod` copies it.
  fn set_windows_attributes(&mut self, inode: InodeNo, bits: u32) -> Result<(), VfsError> {
    let kind = self.volume.kind(self.store, inode)?;
    let kept = slates_vfs::inode::WindowsAttributes::kept(bits, kind);
    match host_for(&mut self.host, self.volume.is_overlay())? {
      Some(host) => self
        .volume
        .with_host(host)
        .set_windows_attributes(self.store, inode, kept),
      None => self.volume.set_windows_attributes(self.store, inode, kept),
    }
  }

  fn attr_of(&mut self, no: u64) -> Result<NodeAttr, VfsError> {
    let inode = InodeNo(no);
    let observed = match host_for(&mut self.host, self.volume.is_overlay())? {
      Some(host) => self.volume.with_host(host).observe(self.store, inode),
      None => self.volume.observe(self.store, inode),
    }?;
    Ok(node_attr(no, &observed))
  }

  /// The kernel invalidations one journal record owes (§4.6): a content or attribute change on an
  /// inode drops that inode's cached attributes, and with a content change the cached bytes it
  /// touched ([`journal_bytes`]); a namespace change drops the name's cached mapping in its parent
  /// and the parent's attributes (a rename both names). A record whose path no longer resolves over
  /// the volume's nodes (the entry has since gone) owes nothing the kernel could still hold under
  /// that name.
  fn journal_invalidations(
    &mut self,
    op: &Op,
    path: &str,
    inode: Option<InodeNo>,
    out: &mut Vec<Invalidation>,
  ) {
    let data = journal_bytes(op);
    match op {
      Op::Overwrite { .. }
      | Op::Extend { .. }
      | Op::Truncate { .. }
      | Op::Insert { .. }
      | Op::Delete { .. }
      | Op::Setattr
      | Op::SetXattr { .. }
      | Op::RemoveXattr { .. }
      | Op::Witness
      | Op::Drift => {
        if let Some(no) = inode {
          out.push(Invalidation::Inode { ino: no.0, data });
        }
      }
      Op::Create
      | Op::Mknod
      | Op::Mkdir
      | Op::Unlink
      | Op::Rmdir
      | Op::Link
      | Op::Symlink
      | Op::Whiteout
      | Op::Redirect { .. } => self.entry_invalidation(path, out),
      Op::Rename { from } => {
        self.entry_invalidation(from, out);
        self.entry_invalidation(path, out);
      }
      Op::Snapshot => {}
    }
  }

  /// The entry and parent-attribute invalidations for the name at `path`.
  fn entry_invalidation(&mut self, path: &str, out: &mut Vec<Invalidation>) {
    let (dir, name) = match path.rsplit_once('/') {
      Some(("", name)) => ("/", name),
      Some((dir, name)) => (dir, name),
      None => ("/", path),
    };
    if name.is_empty() {
      return;
    }
    if let Ok(located) = self.volume.resolve(self.store, dir) {
      out.push(Invalidation::Entry {
        parent: located.inode.0,
        name: name.to_owned(),
        expire: false,
      });
      // A directory's cached listing has no byte range a name maps to: all of it.
      out.push(Invalidation::Inode {
        ino: located.inode.0,
        data: CachedBytes::ALL,
      });
    }
  }

  /// Whether `name` exists in directory `dir` — through the overlay rules for an overlay volume, so
  /// a base name not yet looked up counts as existing. `NotFound` is the one refusal that means
  /// "absent"; any other (a base directory unreadable) is propagated, never read as absence.
  fn name_exists(&mut self, dir: InodeNo, name: &str) -> Result<bool, VfsError> {
    let located = match host_for(&mut self.host, self.volume.is_overlay())? {
      Some(host) => self.volume.with_host(host).lookup_no(self.store, dir, name),
      None => self.volume.lookup_no(self.store, dir, name),
    };
    match located {
      Ok(_) => Ok(true),
      Err(VfsError::NotFound) => Ok(false),
      Err(e) => Err(e),
    }
  }

  /// Stamps a freshly created inode `new` with the owner a mount must show: its uid is the mounting
  /// user — the request's Unix uid, or its enrolled account without a Unix credential — and its
  /// group is what the request's rule gives it ([`OpContext::group_rule`], A-328): under System V's
  /// rule (a Linux kernel's request, and a Linux host's NFS export) the request's own group when its
  /// credential names one (an NFS `AUTH_SYS` gid or a FUSE header's, on [`OpContext::owner_gid`]),
  /// unless the parent directory `parent` is set-group-ID, whose group it takes and whose bit a new
  /// directory takes too (AUD-29-81); under BSD's (a macOS host's NFS export, as APFS gives it) the
  /// parent's group always and no inherited bit. Without this the
  /// object keeps the volume core's born default (uid 0, gid 0), so a file an ordinary user made
  /// listed as `root wheel` regardless of who made it (§4.13 "each request runs as the mounting user";
  /// the root:wheel mount bug fixed in docs/bugs/2026-09-09-root-wheel-mount.md). A subject that is
  /// not a Unix user (a Windows SID or a certificate — never on the POSIX mount path) carries no uid,
  /// so the born owner stands and that platform's own ownership model governs. The parent's group is
  /// read host-aware so an overlay's base directory reports its real group.
  fn stamp_created_owner(
    &mut self,
    new: InodeNo,
    parent: InodeNo,
    cx: &OpContext,
  ) -> Result<(), VfsError> {
    let Some(uid) = creating_uid(cx) else {
      return Ok(());
    };
    let parent_attrs = match host_for(&mut self.host, self.volume.is_overlay())? {
      Some(host) => self.volume.with_host(host).stat(self.store, parent),
      None => self.volume.stat(self.store, parent),
    }?;
    // The route's rule (A-328): System V's, the creator's group unless a set-group-ID parent gives its own and its bit
    // to a directory (AUD-29-81); or BSD's, the parent's group and no bit.
    let directory = self.volume.kind(self.store, new)? == Kind::Dir;
    let created =
      cx.group_rule
        .created(parent_attrs.mode, parent_attrs.gid, cx.owner_gid, directory);
    self.volume.chown(self.store, new, uid, created.gid)?;
    if created.set_group_id {
      let made = self.volume.stat(self.store, new)?;
      self.volume.chmod(self.store, new, made.mode | S_ISGID)?;
    }
    Ok(())
  }

  /// What a node asked for with `mode` by a creator whose umask is `umask` takes from directory `parent`, `directory`
  /// saying whether the node is one (A-314; Linux's `posix_acl_create`, `fs/posix_acl.c`). Under a default ACL the umask is not applied: the
  /// mode keeps only what the default's owner, mask and other entries grant, and the node is given the default's
  /// entries as its access ACL when they say more than its mode, a directory the default as well. Without one, the
  /// mode less the umask, and no ACL. A default ACL the volume keeps that no parse takes is refused `Integrity`: only a
  /// guest's route sets one, and it sets only what parses.
  fn creation(
    &mut self,
    parent: InodeNo,
    mode: u32,
    umask: Option<u32>,
    directory: bool,
  ) -> Result<Creation, VfsError> {
    let masked = Creation {
      mode: mode & !umask.unwrap_or(0),
      access: None,
      default: None,
    };
    // The parent as a namespace object first, so a number that names none (an attribute's value among them) is refused
    // as the create itself refuses it, never as the attribute verbs would; and only a directory holds a default ACL,
    // the create refusing anything else itself.
    if self.volume.kind(self.store, parent)? != Kind::Dir {
      return Ok(masked);
    }
    let value = match self.attribute(parent, POSIX_ACL_DEFAULT) {
      Ok(value) => value,
      Err(VfsError::NoAttribute) => return Ok(masked),
      Err(refusal) => return Err(refusal),
    };
    let default = Acl::parse(&value).map_err(|_| VfsError::Integrity)?;
    if default.is_empty() {
      return Ok(masked);
    }
    let (mode, access) = default.given(mode);
    Ok(Creation {
      mode,
      access: access.map(|access| access.to_bytes()),
      default: directory.then_some(value),
    })
  }

  /// Gives node `no`, just made, the ACLs its [`Creation`] names (A-314). The caller takes the node back out of its
  /// directory if this is refused, so a create either makes the node with what its directory gives it or makes
  /// nothing, as Linux's does.
  fn give_acls(&mut self, no: InodeNo, creation: &Creation) -> Result<(), VfsError> {
    for (name, value) in [
      (POSIX_ACL_ACCESS, &creation.access),
      (POSIX_ACL_DEFAULT, &creation.default),
    ] {
      if let Some(value) = value {
        self.volume.xattr_set(
          self.store,
          no,
          name,
          value,
          slates_vfs::xattr::XattrSet::Either,
        )?;
      }
    }
    Ok(())
  }

  /// The value of attribute `name` on inode `no`, whole (`NoAttribute` when it has none).
  fn attribute(&mut self, no: InodeNo, name: &[u8]) -> Result<Vec<u8>, VfsError> {
    let len = self.volume.xattr_len(self.store, no, name)?;
    let mut value = vec![0u8; usize::try_from(len).map_err(|_| VfsError::FileTooLarge)?];
    let read = self
      .volume
      .xattr_read(self.store, no, name, 0, &mut value)?;
    value.truncate(read);
    Ok(value)
  }

  /// `value`, `owner`'s access ACL as the volume keeps it, as a reader is shown it (A-314): its owner, mask and other
  /// entries (the owning group's where it has no mask) taken from the mode now, so a chmod through any route, which
  /// changes only the mode, is the ACL's change too (what Linux's `posix_acl_chmod` writes into a stored ACL). A kept
  /// value no parse takes is refused `Integrity`, as a kept default ACL is.
  fn access_acl_shown(&mut self, owner: InodeNo, value: Vec<u8>) -> Result<Vec<u8>, VfsError> {
    let mut acl = Acl::parse(&value).map_err(|_| VfsError::Integrity)?;
    if acl.is_empty() {
      return Ok(value);
    }
    acl.showing(self.attr_of(owner.0)?.mode);
    Ok(acl.to_bytes())
  }

  /// Sets `owner`'s ACL `name` to `value` as Linux keeps one (A-314; `set_posix_acl`), whatever `XATTR_CREATE` or
  /// `XATTR_REPLACE` asked, as Linux's own attribute path ignores them for an ACL: an access ACL with the mode
  /// ([`VolumeBridge::set_access_acl`]), a default ACL as it is. A value no ACL parse takes is refused `Invalid`: a
  /// guest's route checks one before it gets here.
  fn set_acl(&mut self, owner: InodeNo, name: &[u8], value: &[u8]) -> Result<(), VfsError> {
    let acl = Acl::parse(value).map_err(|_| VfsError::Invalid)?;
    if name == POSIX_ACL_ACCESS {
      self.set_access_acl(owner, &acl, value)
    } else {
      self.keep_acl(owner, name, &acl, value)
    }
  }

  /// Keeps `acl` (whose bytes are `value`) as `owner`'s ACL `name`, or none for the empty ACL, which a kernel reads as
  /// none (`posix_acl_from_xattr` of no entries).
  fn keep_acl(
    &mut self,
    owner: InodeNo,
    name: &[u8],
    acl: &Acl,
    value: &[u8],
  ) -> Result<(), VfsError> {
    if acl.is_empty() {
      return self.remove_attribute_if_set(owner, name);
    }
    self.volume.xattr_set(
      self.store,
      owner,
      name,
      value,
      slates_vfs::xattr::XattrSet::Either,
    )
  }

  /// Sets `owner`'s access ACL to `acl` (whose bytes are `value`) as `posix_acl_update_mode` does (A-314): the mode's
  /// three classes from its owner, mask (or owning group) and other entries, and the ACL kept only when it says more
  /// than a mode can, removed otherwise. The mode changes first, through the overlay rules, which may need room; if
  /// the attribute then cannot be kept, the mode is put back, which needs none, the inode being current by then.
  fn set_access_acl(&mut self, owner: InodeNo, acl: &Acl, value: &[u8]) -> Result<(), VfsError> {
    let before = self.attr_of(owner.0)?.mode;
    let mode = if acl.is_empty() {
      before
    } else {
      (before & !MODE_PERMISSION_BITS) | acl.permission_bits()
    };
    if mode != before {
      self.chmod_through_overlay(owner, mode)?;
    }
    let kept = if acl.exceeds_a_mode() {
      self.keep_acl(owner, POSIX_ACL_ACCESS, acl, value)
    } else {
      self.remove_attribute_if_set(owner, POSIX_ACL_ACCESS)
    };
    match kept {
      Err(refusal) if mode != before => self.chmod_through_overlay(owner, before).and(Err(refusal)),
      kept => kept,
    }
  }

  /// Removes attribute `name` from `owner`, and is done if it had none: an ACL set to say no more than a mode, or to
  /// nothing, is kept as none (`posix_acl_update_mode`; `posix_acl_from_xattr` of no entries).
  fn remove_attribute_if_set(&mut self, owner: InodeNo, name: &[u8]) -> Result<(), VfsError> {
    match self.volume.xattr_remove(self.store, owner, name) {
      Ok(()) | Err(VfsError::NoAttribute) => Ok(()),
      Err(refusal) => Err(refusal),
    }
  }

  /// Sets inode `no`'s mode through the overlay rules, as a `setattr` does: an untouched base entry's witness is copied
  /// up first, so the live-disk stat that follows it cannot undo the change.
  fn chmod_through_overlay(&mut self, no: InodeNo, mode: u32) -> Result<(), VfsError> {
    match host_for(&mut self.host, self.volume.is_overlay())? {
      Some(host) => self.volume.with_host(host).chmod(self.store, no, mode),
      None => self.volume.chmod(self.store, no, mode),
    }
  }
}

/// What a node made in a directory takes from it (A-314; Linux's `posix_acl_create`): the mode it is made with, and
/// the ACLs it is given once made.
struct Creation {
  /// The mode it is made with: the asked mode cut by the directory's default ACL, or less the creator's umask where
  /// the directory has none.
  mode: u32,
  /// The access ACL it is given: the default's entries cut to its mode, where they say more than the mode can.
  access: Option<Vec<u8>>,
  /// The default ACL it is given: a directory made under one takes it as its own.
  default: Option<Vec<u8>>,
}

/// The uid request `cx` creates as: the transport's per-request caller (an NFS `AUTH_SYS` uid, a FUSE request
/// header's), else the enrolled subject's own Unix account; `None` for a subject that is no Unix user (a Windows SID, a
/// certificate — never on the POSIX mount path), whose platform's own ownership model governs.
fn creating_uid(cx: &OpContext) -> Option<u32> {
  match (cx.owner_uid, &cx.subject) {
    (Some(uid), _) => Some(uid),
    (None, Principal::Uid { uid, .. }) => Some(*uid),
    // An enrolled consumer is a workload *within* its host account (§4.13): what it creates is the
    // account's, as the host filesystem will report it.
    (None, Principal::Consumer { account, .. }) => Some(*account),
    (None, Principal::Sid { .. } | Principal::Certificate { .. }) => None,
  }
}

/// A neutral [`NodeAttr`] from what the volume observes of inode `no`. Generation is 0 until
/// generation-tracked reuse lands (§4.6 `(no, gen)`).
fn node_attr(no: u64, observed: &Observed) -> NodeAttr {
  let attrs = &observed.attrs;
  NodeAttr {
    ino: no,
    generation: 0,
    kind: observed.kind,
    mode: attrs.mode,
    nlink: attrs.nlink,
    uid: attrs.uid,
    gid: attrs.gid,
    size: attrs.size,
    atime: attrs.atime,
    mtime: attrs.mtime,
    ctime: attrs.ctime,
    btime: attrs.btime,
    windows: observed.windows.bits(),
    change: observed.change,
    device: observed.device,
  }
}

/// An empty open-handle map sized for [`VolumeBridge::attached`]. A caller that owns the map across
/// requests (the daemon's serve paths) creates it with this, so its capacity is exactly an owned
/// bridge's — no duplicated bound.
pub fn new_handle_store() -> Slab<u64> {
  Slab::new(HANDLE_SEGMENT, MAX_OPEN_HANDLES)
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
  fn authorize_change(&self, cx: &OpContext) -> Result<(), VfsError> {
    self.authorize_write(cx)
  }

  fn awaits_publication(&self) -> bool {
    self.volume.awaits_publication() || self.write_unlogged
  }

  fn root(&mut self, cx: &OpContext) -> Result<u64, VfsError> {
    self.authorize_read(cx)?;
    self.volume.root_inode(self.store).map(|no| no.0)
  }

  fn lookup(&mut self, parent: ObjectId, cx: &OpContext, name: &str) -> Result<NodeAttr, VfsError> {
    self.authorize_read(cx)?;
    let dir = InodeNo(parent.inode);
    // `.` and `..` (POSIX path resolution): the directory itself, and its parent, the root's being the root. NFSv3
    // LOOKUP and NFSv4 LOOKUPP send them, and Linux's client asks `..` to reconnect a directory it holds only by
    // handle (`nfs_get_parent`, after a server restart or for `open_by_handle_at`); the volume core resolves
    // children only, so both answered `ENOENT`, as knfsd does not (found 2026-10-06 by the hostile-names battery).
    if name == "." || name == ".." {
      if self.volume.kind(self.store, dir)? != Kind::Dir {
        return Err(VfsError::NotDirectory);
      }
      let named = if name == "." {
        dir
      } else {
        self.volume.parent_no(self.store, dir)?
      };
      return self.attr_of(named.0);
    }
    // Host-aware for an overlay volume (audit BUG-5): a base entry not yet hydrated into the
    // dirtree is found through the base plane's own lookup, which loads the directory's listing on
    // first use (the OS's bulk call), validates it by the directory's fingerprint on every later
    // use, and materializes a hit — so a direct LOOKUP of an untouched base file works without a
    // prior READDIR (§4.5 "Lookup"). A scratch volume uses the plain lookup.
    let located = match host_for(&mut self.host, self.volume.is_overlay())? {
      Some(host) => self.volume.with_host(host).lookup_no(self.store, dir, name),
      None => self.volume.lookup_no(self.store, dir, name),
    }?;
    let attr = self.attr_of(located.inode.0)?;
    // No implicit reference: a transport that owns lookup references (FUSE) takes one explicitly
    // through `reference`; NFS takes none (§3). Fixes the NFS lookup-reference leak.
    Ok(attr)
  }

  fn stored_entry(
    &mut self,
    parent: ObjectId,
    cx: &OpContext,
    name: &str,
  ) -> Result<(String, NodeAttr), VfsError> {
    // Looked up first, through the base plane on an overlay, so an untouched base entry is in the directory with the
    // name its host gave it (`lookup` materializes a hit). `.` and `..` are no entries a directory stores: they are named
    // as asked.
    let attr = self.lookup(parent, cx, name)?;
    if name == "." || name == ".." {
      return Ok((name.to_owned(), attr));
    }
    let stored = self
      .volume
      .stored_name_no(self.store, InodeNo(parent.inode), name)?;
    Ok((stored, attr))
  }

  fn getattr(&mut self, object: ObjectId, cx: &OpContext) -> Result<NodeAttr, VfsError> {
    self.authorize_read(cx)?;
    if let Some(owner) = InodeNo(object.inode).derived_from() {
      return self.view_getattr(owner);
    }
    self.attr_of(object.inode)
  }

  fn open(&mut self, object: ObjectId, cx: &OpContext, flags: u32) -> Result<u64, VfsError> {
    self.authorize_read(cx)?;
    // An open for writing is itself a change: a read-only file system refuses it at the open (POSIX `EROFS`), never at
    // each write through the handle it granted.
    if opens_for_writing(flags) {
      self.authorize_write(cx)?;
    }
    if self
      .volume
      .kind(self.store, InodeNo(object.inode))?
      .is_special()
    {
      return Err(VfsError::SpecialFileOperation);
    }
    // A directory is opened through opendir; open refuses it.
    if self.volume.kind(self.store, InodeNo(object.inode))? == Kind::Dir {
      return Err(VfsError::IsDirectory);
    }
    // An untouched base file is answered from the daemon's descriptor on the backing file (§4.6
    // "Base files"), opened now so an unlink or a replacement on disk while the file is open still
    // serves the bytes the opener saw.
    if let Some(host) = self.host.as_mut() {
      self
        .volume
        .with_host(host)
        .open_base(self.store, InodeNo(object.inode))?;
    }
    // The open pins the file it opened (`open_handle` counts it): a base file replaced on the disk meanwhile keeps
    // serving this handle what it opened (POSIX), released at `release` or the attachment's teardown sweep.
    self.open_handle(object.inode, cx.owner)
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
    if let Some(owner) = InodeNo(object.inode).derived_from() {
      return self.view_read(owner, offset, want, out);
    }
    // Read straight into the tail of `out`: one buffer, no second copy.
    let start = out.len();
    out.resize(start.saturating_add(want), 0);
    let inode = InodeNo(object.inode);
    let tail = out.get_mut(start..).unwrap_or_default();
    let read = match host_for(&mut self.host, self.volume.is_overlay())? {
      Some(host) => self
        .volume
        .with_host(host)
        .read(self.store, inode, offset, tail),
      None => self.volume.read(self.store, inode, offset, tail),
    };
    match read {
      Ok(read) => {
        out.truncate(start.saturating_add(read));
        Ok(())
      }
      Err(e) => {
        out.truncate(start);
        Err(e)
      }
    }
  }

  fn xattr_get(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    name: &[u8],
  ) -> Result<Vec<u8>, VfsError> {
    self.authorize_read(cx)?;
    let owner = self.xattr_owner(object)?;
    let value = self.attribute(owner, name)?;
    if name == POSIX_ACL_ACCESS {
      return self.access_acl_shown(owner, value);
    }
    Ok(value)
  }

  fn xattr_set(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    name: &[u8],
    value: &[u8],
    how: slates_vfs::xattr::XattrSet,
  ) -> Result<(), VfsError> {
    self.authorize_write(cx)?;
    let owner = self.xattr_owner(object)?;
    if crate::is_posix_acl(name) {
      return self.set_acl(owner, name, value);
    }
    self.volume.xattr_set(self.store, owner, name, value, how)
  }

  fn xattr_list(&mut self, object: ObjectId, cx: &OpContext) -> Result<Vec<Box<[u8]>>, VfsError> {
    self.authorize_read(cx)?;
    let owner = self.xattr_owner(object)?;
    self.volume.xattr_names(self.store, owner)
  }

  fn xattr_remove(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    name: &[u8],
  ) -> Result<(), VfsError> {
    self.authorize_write(cx)?;
    let owner = self.xattr_owner(object)?;
    self.volume.xattr_remove(self.store, owner, name)
  }

  fn seek(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    offset: u64,
    data: bool,
  ) -> Result<Option<u64>, VfsError> {
    self.authorize_read(cx)?;
    // An AppleDouble view (a derived number) is rendered, not stored: it has no inode to search, and
    // only the macOS NFSv3 client addresses it.
    if InodeNo(object.inode).derived_from().is_some() {
      return Err(VfsError::Invalid);
    }
    let want = if data {
      slates_vfs::volume::Seek::Data
    } else {
      slates_vfs::volume::Seek::Hole
    };
    self
      .volume
      .seek(self.store, InodeNo(object.inode), offset, want)
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
    let inode = InodeNo(object.inode);
    if let Some(owner) = inode.derived_from() {
      let written = self.view_write(owner, offset, data)?;
      return u32::try_from(written).map_err(|_| VfsError::FileTooLarge);
    }
    // An overlay write copies the base up first (through the host); a scratch write does not.
    let written = match host_for(&mut self.host, self.volume.is_overlay())? {
      Some(host) => self
        .volume
        .with_host(host)
        .write(self.store, inode, offset, data),
      None => self.volume.write(self.store, inode, offset, data),
    }?;
    // Kept before the reply where the transport keeps its writes in the shard's write log (A-410); a write the log did
    // not keep owes a publication before its reply instead.
    if let Some(log) = self.write_log.as_mut()
      && !log(inode.0, offset, data.get(..written).unwrap_or(data))
    {
      self.write_unlogged = true;
    }
    u32::try_from(written).map_err(|_| VfsError::FileTooLarge)
  }

  fn admit_allocation(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    offset: u64,
    len: u64,
  ) -> Result<(), VfsError> {
    self.authorize_write(cx)?;
    let (inode, end) = allocation_range(object, offset, len)?;
    match host_for(&mut self.host, self.volume.is_overlay())? {
      Some(host) => self
        .volume
        .with_host(host)
        .admit_allocation(self.store, inode, offset, end),
      None => self.volume.admit_allocation(self.store, inode, offset, end),
    }
  }

  fn allocate(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    offset: u64,
    len: u64,
  ) -> Result<(), VfsError> {
    self.authorize_write(cx)?;
    let (inode, end) = allocation_range(object, offset, len)?;
    match host_for(&mut self.host, self.volume.is_overlay())? {
      Some(host) => self
        .volume
        .with_host(host)
        .allocate(self.store, inode, offset, end),
      None => self.volume.allocate(self.store, inode, offset, end),
    }
  }

  fn witness_for_writable_map(&mut self, object: ObjectId, cx: &OpContext) -> Result<(), VfsError> {
    self.authorize_write(cx)?;
    match host_for(&mut self.host, self.volume.is_overlay())? {
      Some(host) => self
        .volume
        .with_host(host)
        .witness_content(self.store, InodeNo(object.inode)),
      None => Ok(()),
    }
  }

  fn opendir(&mut self, object: ObjectId, cx: &OpContext) -> Result<u64, VfsError> {
    self.authorize_read(cx)?;
    if self.volume.kind(self.store, InodeNo(object.inode))? != Kind::Dir {
      return Err(VfsError::NotDirectory);
    }
    self.open_handle(object.inode, cx.owner)
  }

  fn readdir(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    _fh: u64,
    cookie: u64,
    limit: usize,
  ) -> Result<Vec<DirEntry>, VfsError> {
    self.authorize_read(cx)?;
    let dir_no = InodeNo(object.inode);
    // POSIX `readdir` lists "." (the directory) and ".." (its parent) before the children; the volume
    // core returns children only, so the shared bridge synthesizes them here — one code path, so the FUSE
    // mount and the NFS export list them identically (R8). Their cookies are 1 and 2; a child's is its
    // name's hash cookie, never below 3. The parent for `..`: the root has none, so `..` is the root itself
    // (POSIX); a directory whose parent cannot be resolved falls back to itself rather than failing.
    let mut page = Vec::new();
    if cookie == 0 {
      page.push(DirEntry {
        ino: object.inode,
        kind: Kind::Dir,
        name: ".".to_owned(),
        cookie: 1,
      });
    }
    if cookie <= 1 {
      let parent = self
        .volume
        .parent_no(self.store, dir_no)
        .unwrap_or(dir_no)
        .0;
      page.push(DirEntry {
        ino: parent,
        kind: Kind::Dir,
        name: "..".to_owned(),
        cookie: 2,
      });
    }
    // Where the children resume: from the first after the dot entries, or after the cookie's group. A
    // cookie past the last possible group has nothing after it.
    let from_hash = if cookie < slates_vfs::FIRST_CHILD_COOKIE {
      Some(0)
    } else {
      slates_vfs::resume_hash(cookie)
    };
    let Some(from_hash) = from_hash else {
      return Ok(page);
    };
    let room = limit.saturating_sub(page.len());
    let rows = match host_for(&mut self.host, self.volume.is_overlay())? {
      Some(host) => self
        .volume
        .with_host(host)
        .readdir_page_no(self.store, dir_no, from_hash, room),
      None => self
        .volume
        .readdir_page_no(self.store, dir_no, from_hash, room),
    }?;
    page.extend(rows.into_iter().map(|row| DirEntry {
      ino: row.inode.0,
      kind: row.kind,
      name: row.name.to_owned(),
      cookie: slates_vfs::dir_cookie(row.hash),
    }));
    Ok(page)
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
    let parent_no = InodeNo(parent.inode);
    let creation = self.creation(parent_no, mode, cx.umask, false)?;
    // Through the overlay rules for an overlay volume: a name the base holds is `EEXIST`, not a
    // second file shadowing the disk's (§4.6; AC-1.17).
    let no = match host_for(&mut self.host, self.volume.is_overlay())? {
      Some(host) => {
        self
          .volume
          .with_host(host)
          .create_file_no(self.store, parent_no, name, creation.mode)
      }
      None => self
        .volume
        .create_file_no(self.store, parent_no, name, creation.mode),
    }?;
    self.stamp_created_owner(no, parent_no, cx)?;
    if let Err(refusal) = self.give_acls(no, &creation) {
      return self.unlink(parent, cx, name).and(Err(refusal));
    }
    let entry = node_attr(no.0, &self.volume.observe(self.store, no)?);
    // A create takes only an open reference (dropped by release); it does not implicitly take a
    // lookup reference — a transport that owns lookup references (FUSE) takes one through
    // `reference`, NFS takes none (§3). `open_handle` references then allocates, undoing on failure.
    let fh = self.open_handle(no.0, cx.owner)?;
    Ok((entry, fh))
  }

  fn release(&mut self, object: ObjectId, cx: &OpContext, fh: u64) -> Result<(), VfsError> {
    self.authorize_volume(cx)?;
    // Drop the open reference the handle held (attributed to this attachment), then free the slot
    // for reuse. An unknown or already-freed handle is a no-op (the kernel may release one the
    // bridge already dropped).
    //
    // A release names its file, and only a handle of that file is released (A-148). A daemon that takes a mount over
    // (A-61) issues from a fresh store the values its predecessor issued, so the kernel can release a handle it opened
    // before the takeover whose value this bridge has since given to another file. That handle is not this one.
    let unpacked = unpack_handle(fh);
    if self.handles.get(unpacked).ok().copied() == Some(object.inode) {
      self.volume.close_for(InodeNo(object.inode), cx.owner);
      let _ = self
        .volume
        .forget_for(self.store, InodeNo(object.inode), cx.owner, 1);
      let _ = self.handles.remove(unpacked);
    } else {
      // A handle this daemon never issued for this file: one a dead daemon issued before a takeover. It gives back an
      // open the recovery restored for the file, while one is left (A-152); until 2026-10-22 it gave back nothing, and
      // an unlinked file stayed until the mount ended
      // (`docs/bugs/2026-10-22-a-handle-opened-before-a-takeover-gave-back-nothing-at-its-release.md`).
      self
        .volume
        .release_restored_open(self.store, InodeNo(object.inode), cx.owner)?;
    }
    // A handle from before the takeover is forgotten by the lost-write record too, so it holds only handles alive.
    if let Some(lost) = self.lost.as_deref_mut() {
      lost.released(fh, object.inode);
    }
    Ok(())
  }

  fn reference(&mut self, object: ObjectId, cx: &OpContext) -> Result<(), VfsError> {
    self.authorize_read(cx)?;
    // Takes one lookup reference on the object attributed to the calling attachment (the FUSE edge
    // calls this; NFS does not, §3), so a teardown sweep releases it. An inode a transport still
    // references keeps its content and table entry across an unlink until the last reference drops.
    self
      .volume
      .reference_for(self.store, InodeNo(object.inode), cx.owner)
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
      .forget_for(self.store, InodeNo(object.inode), cx.owner, nlookup);
  }

  fn sweep_attachment(&mut self, cx: &OpContext) -> Result<(), VfsError> {
    self.authorize_volume(cx)?;
    // Release every reference this attachment holds in one bounded batch, reclaiming the inodes
    // that reach zero references and no links (§3 the teardown sweep; a FUSE unmount discards a
    // whole mount's lookup references without a per-inode FORGET). Idempotent: a second sweep of a
    // drained attachment releases nothing.
    self.volume.sweep_attachment(self.store, cx.owner)
  }

  fn within(&mut self, object: ObjectId, scope: u64, cx: &OpContext) -> Result<bool, VfsError> {
    self.authorize_read(cx)?;
    // An AppleDouble sidecar's derived number stands for its owner's attributes: it is within when the owner is.
    let no = InodeNo(object.inode);
    let no = no.derived_from().unwrap_or(no);
    match self.volume.within(self.store, no, InodeNo(scope)) {
      Ok(within) => Ok(within),
      // A number the volume no longer holds is in no scope.
      Err(VfsError::NotFound) => Ok(false),
      Err(e) => Err(e),
    }
  }

  fn flush(&mut self, object: ObjectId, cx: &OpContext, fh: u64) -> Result<(), VfsError> {
    self.authorize_read(cx)?;
    // A file whose acknowledged writes a daemon's death lost answers its first flush through a handle opened
    // before the takeover `EIO` (A-61): the writeback error Linux reports, never a success over the hole.
    if let Some(lost) = self.lost.as_deref()
      && lost.reports(object.inode, fh)
    {
      return Err(VfsError::RecoveryIncomplete);
    }
    // Nothing to force at this layer: the bytes are in the volume's live tree. Their survival across a
    // daemon restart is the owner's barrier (§4.8, D-18): every transport holds this flush's success reply
    // until its recovery publication has captured the volume (AUD-29-82), and answers `EIO` when it did not.
    Ok(())
  }
  fn fsync(&mut self, object: ObjectId, cx: &OpContext, fh: u64) -> Result<(), VfsError> {
    self.authorize_read(cx)?;
    // The `fsync` a lost file's writer makes is told `EIO` once, consuming the report (A-61); every later one,
    // and every later close, succeeds.
    if let Some(lost) = self.lost.as_deref_mut()
      && lost.reports_and_tells(object.inode, fh)
    {
      return Err(VfsError::RecoveryIncomplete);
    }
    self.flush(object, cx, fh)
  }

  fn mknod(
    &mut self,
    parent: ObjectId,
    cx: &OpContext,
    name: &str,
    mode: u32,
    node: SpecialNode,
  ) -> Result<NodeAttr, VfsError> {
    self.authorize_write(cx)?;
    // A device node other than a whiteout is made for a caller acting as root alone (A-294,
    // `crate::access::needs_superuser`): a kernel asks `CAP_MKNOD` before a FUSE or virtio-fs request reaches here, and
    // this holds every other transport to the same rule.
    if crate::access::needs_superuser(node) && creating_uid(cx) != Some(crate::access::ROOT_UID) {
      return Err(VfsError::NotPermitted);
    }
    let parent_no = InodeNo(parent.inode);
    let creation = self.creation(parent_no, mode, cx.umask, false)?;
    let no = match host_for(&mut self.host, self.volume.is_overlay())? {
      Some(host) => {
        self
          .volume
          .with_host(host)
          .mknod_no(self.store, parent_no, name, creation.mode, node)
      }
      None => self
        .volume
        .mknod_no(self.store, parent_no, name, creation.mode, node),
    }?;
    self.stamp_created_owner(no, parent_no, cx)?;
    if let Err(refusal) = self.give_acls(no, &creation) {
      return self.unlink(parent, cx, name).and(Err(refusal));
    }
    Ok(node_attr(no.0, &self.volume.observe(self.store, no)?))
  }

  fn mkdir(
    &mut self,
    parent: ObjectId,
    cx: &OpContext,
    name: &str,
    mode: u32,
  ) -> Result<NodeAttr, VfsError> {
    self.authorize_write(cx)?;
    let parent_no = InodeNo(parent.inode);
    let creation = self.creation(parent_no, mode, cx.umask, true)?;
    // Through the overlay rules for an overlay volume: a name the base holds is `EEXIST`, and the
    // new directory is opaque (only overlay entries show), so it is in the diverged set the landing
    // plans from (§4.5 "Mutation"; AC-1.10).
    let no = match host_for(&mut self.host, self.volume.is_overlay())? {
      Some(host) => {
        self
          .volume
          .with_host(host)
          .mkdir_no(self.store, parent_no, name, creation.mode)
      }
      None => self
        .volume
        .mkdir_no(self.store, parent_no, name, creation.mode),
    }?;
    self.stamp_created_owner(no, parent_no, cx)?;
    if let Err(refusal) = self.give_acls(no, &creation) {
      return self.rmdir(parent, cx, name).and(Err(refusal));
    }
    let attr = node_attr(no.0, &self.volume.observe(self.store, no)?);
    // No implicit lookup reference (see `lookup`): FUSE takes one through `reference`, NFS none.
    Ok(attr)
  }

  fn unlink(&mut self, parent: ObjectId, cx: &OpContext, name: &str) -> Result<(), VfsError> {
    self.authorize_write(cx)?;
    let parent_no = InodeNo(parent.inode);
    // Through the overlay rules for an overlay volume: the listing is reloaded first, so a base name
    // leaves its whiteout even after a watcher hint invalidated the cached listing (§4.5).
    match host_for(&mut self.host, self.volume.is_overlay())? {
      Some(host) => self
        .volume
        .with_host(host)
        .unlink_no(self.store, parent_no, name),
      None => self.volume.unlink_no(self.store, parent_no, name),
    }
  }

  fn rmdir(&mut self, parent: ObjectId, cx: &OpContext, name: &str) -> Result<(), VfsError> {
    self.authorize_write(cx)?;
    let parent_no = InodeNo(parent.inode);
    match host_for(&mut self.host, self.volume.is_overlay())? {
      Some(host) => self
        .volume
        .with_host(host)
        .rmdir_no(self.store, parent_no, name),
      None => self.volume.rmdir_no(self.store, parent_no, name),
    }
  }

  fn symlink(
    &mut self,
    parent: ObjectId,
    cx: &OpContext,
    name: &str,
    target: &str,
  ) -> Result<NodeAttr, VfsError> {
    self.authorize_write(cx)?;
    let parent_no = InodeNo(parent.inode);
    let no = match host_for(&mut self.host, self.volume.is_overlay())? {
      Some(host) => self
        .volume
        .with_host(host)
        .symlink_no(self.store, parent_no, name, target),
      None => self.volume.symlink_no(self.store, parent_no, name, target),
    }?;
    self.stamp_created_owner(no, parent_no, cx)?;
    let attr = node_attr(no.0, &self.volume.observe(self.store, no)?);
    // No implicit lookup reference (see `lookup`): FUSE takes one through `reference`, NFS none.
    Ok(attr)
  }

  fn link(
    &mut self,
    target: ObjectId,
    new_parent: ObjectId,
    cx: &OpContext,
    new_name: &str,
  ) -> Result<NodeAttr, VfsError> {
    self.authorize_write(cx)?;
    let dir = InodeNo(new_parent.inode);
    let target_no = InodeNo(target.inode);
    // Through the overlay rules for an overlay volume: a link to an untouched base file copies its
    // witness up (the link count is a metadata change), and a name the base holds is `EEXIST`.
    match host_for(&mut self.host, self.volume.is_overlay())? {
      Some(host) => self
        .volume
        .with_host(host)
        .link_no(self.store, dir, new_name, target_no),
      None => self.volume.link_no(self.store, dir, new_name, target_no),
    }?;
    // Return the target's attributes, now with the incremented link count (a FUSE LINK reply is an
    // entry_out for the new name, which resolves to the same inode).
    self.attr_of(target.inode)
  }

  /// The link's target. A target that leads out of the volume is refused `LinkProtected` (`EACCES`) to a caller the
  /// transport names who does not own the link (A-107, `crate::links`): the kernel asks for the target on every follow,
  /// so this is where Linux's `protected_symlinks` rule applies at the volume's edge. A request with no Unix caller
  /// (the SDK, MCP) reads the text and follows nothing, so it is answered as before.
  fn readlink(&mut self, object: ObjectId, cx: &OpContext) -> Result<String, VfsError> {
    self.authorize_read(cx)?;
    let no = InodeNo(object.inode);
    let target = self.volume.readlink(self.store, no)?.into_string();
    if let Some(caller) = cx.owner_uid {
      let owner = self.volume.stat(self.store, no)?.uid;
      // A link whose place is unknown (none recorded) is judged at the root, where any `..` leaves.
      let place = self
        .volume
        .path_of_inode(self.store, no)
        .unwrap_or_default();
      if caller != owner && crate::links::leaves_volume(&place, &target) {
        return Err(VfsError::LinkProtected);
      }
    }
    Ok(target)
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
    let from_dir = InodeNo(old_parent.inode);
    let to_dir = InodeNo(new_parent.inode);
    // NOREPLACE must fail if the destination exists rather than replacing it — a base name the
    // mount has not looked up yet included, so the existence check goes through the overlay rules
    // (the plain check had replaced such a name silently). The owning shard runs one operation at
    // a time, so this check and the rename are atomic against other work.
    if flags.no_replace && self.name_exists(to_dir, new_name)? {
      return Err(VfsError::AlreadyExists);
    }
    // Through the overlay rules for an overlay volume: a renamed base file copies its witness up and
    // leaves a whiteout at the old name, a renamed base directory records its origin (§4.5 "Rename").
    match host_for(&mut self.host, self.volume.is_overlay())? {
      Some(host) => self
        .volume
        .with_host(host)
        .rename_no(self.store, from_dir, old_name, to_dir, new_name),
      None => self
        .volume
        .rename_no(self.store, from_dir, old_name, to_dir, new_name),
    }
  }

  fn setattr(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    changes: SetAttr,
  ) -> Result<NodeAttr, VfsError> {
    self.authorize_write(cx)?;
    let ino = object.inode;
    let inode = InodeNo(ino);
    if let Some(owner) = inode.derived_from() {
      return self.view_setattr(owner, changes);
    }
    // Every requested field is applied, never acknowledged and ignored (§4.6; audit BUG-8), and for
    // an overlay volume each goes through the overlay rules: a change to an untouched base file
    // copies its witness up first — content for a truncate, metadata-only for the rest (§4.5
    // "Copy-up") — so the landing has the base the change was made against, and the live-disk stat
    // that follows an untouched entry no longer undoes the change.
    if let Some(size) = changes.size {
      match host_for(&mut self.host, self.volume.is_overlay())? {
        Some(host) => self
          .volume
          .with_host(host)
          .truncate(self.store, inode, size),
        None => self.volume.truncate(self.store, inode, size),
      }?;
    }
    if let Some(mode) = changes.mode {
      self.chmod_through_overlay(inode, mode)?;
    }
    // Ownership fills the unset half of the pair from the current attributes, so setting only the
    // uid leaves the gid unchanged; the current attributes are read host-aware.
    if changes.uid.is_some() || changes.gid.is_some() {
      let current = self.attr_of(ino)?;
      let uid = changes.uid.unwrap_or(current.uid);
      let gid = changes.gid.unwrap_or(current.gid);
      match host_for(&mut self.host, self.volume.is_overlay())? {
        Some(host) => self
          .volume
          .with_host(host)
          .chown(self.store, inode, uid, gid),
        None => self.volume.chown(self.store, inode, uid, gid),
      }?;
    }
    if changes.atime.is_some()
      || changes.mtime.is_some()
      || changes.ctime.is_some()
      || changes.btime.is_some()
    {
      match host_for(&mut self.host, self.volume.is_overlay())? {
        Some(host) => self.volume.with_host(host).set_times(
          self.store,
          inode,
          changes.atime,
          changes.mtime,
          changes.ctime,
          changes.btime,
        ),
        None => self.volume.set_times(
          self.store,
          inode,
          changes.atime,
          changes.mtime,
          changes.ctime,
          changes.btime,
        ),
      }?;
    }
    if let Some(bits) = changes.windows {
      self.set_windows_attributes(inode, bits)?;
    }
    self.attr_of(ino)
  }

  fn now(&mut self) -> i64 {
    self.volume.wall_ns()
  }

  fn dax_offer(&mut self) -> crate::DaxOffer {
    self.dax
  }

  fn owner_view(&mut self) -> crate::OwnerView {
    self.owners
  }

  fn attribute_namespaces(&mut self) -> crate::AttributeNamespaces {
    self.namespaces
  }

  fn setxattr_ext(&mut self) -> bool {
    self.setxattr_ext
  }

  fn set_setxattr_ext(&mut self, extended: bool) {
    self.setxattr_ext = extended;
  }

  fn dax_state(&mut self) -> Option<crate::DaxState<'_>> {
    let volume = self.volume.prefix();
    let store = &mut *self.store;
    Some(crate::DaxState {
      backings: &mut store.shards_backings,
      volume,
    })
  }

  fn cache_lifetime(&mut self, object: ObjectId, cx: &OpContext) -> CacheLifetime {
    // A live base source — an untouched base entry, a merged directory — may change beneath the
    // volume without a hint, so the kernel may hold its name and attributes only for the base
    // filesystem's timestamp granularity (§4.6; the racy window of §4.5); the volume's own objects
    // are invalidated explicitly and may be held until then. A context for another volume gets no
    // lifetime worth caching.
    if cx.volume != self.volume_id {
      return CacheLifetime::Bounded { ns: 1 };
    }
    // A transport that cannot deliver an invalidation keeps nothing past its use (AUD-29-79): a promise of
    // caching until an invalidation would be one no mechanism keeps.
    if cx.coherence == crate::CacheCoherence::Revalidated {
      return CacheLifetime::Bounded { ns: 0 };
    }
    let no = InodeNo(object.inode);
    match self.volume.base_plane() {
      Some(plane) if self.volume.is_live_source(self.store, no) => CacheLifetime::Bounded {
        ns: plane.timestamp_granularity_ns(),
      },
      _ => CacheLifetime::Forever,
    }
  }

  fn invalidations(
    &mut self,
    cx: &OpContext,
    cursor: InvalidationCursor,
    out: &mut Vec<Invalidation>,
  ) -> Result<InvalidationCursor, VfsError> {
    self.authorize_read(cx)?;
    // 1. The journal: every record since the cursor is a change this transport did not make (it
    //    advanced past its own after serving them), so each names a kernel entry to drop. A
    //    content record carries its inode; a namespace record carries the new path, whose parent
    //    is resolved over the volume's own nodes (a mutation's path is always loaded). Only a
    //    current-head view sees these. A pinned immutable view (a snapshot or green-volume
    //    attachment) never sees a head mutation, so none is staleness in its frozen view, and its
    //    content changes only through an `advance` re-pin, which carries its own invalidated paths
    //    in the `Advanced` reply (server `snapshot_view::advance`), not here. Emitting a head
    //    change to a pinned view would drop caches still valid for its version and disclose
    //    current-head file names and inodes it must never see (§4.16, §4.6).
    //    A cursor older than the journal's retention cannot be walked: the records after it are partly gone, and
    //    walking from the oldest still held skipped the rest, so the kernel kept serving them stale indefinitely
    //    (`docs/bugs/2026-10-22-a-mount-that-falls-behind-the-journal-never-hears-of-the-changes-it-missed.md`).
    //    The transport is told that anything it holds may be stale instead, and resumes from the head.
    if matches!(cx.view, View::Current) {
      // What waited for this round is the journal budget's input (§4.5: mutation rate × lag × record size, measured as
      // the backlog itself). This is the journal's only consuming round in production, and its start is the earliest
      // record this attachment's reader may read again, so an open reader's point moves to it.
      self
        .volume
        .note_journal_round(cx.attachment.key(), cursor.journal_seq);
      let log = self.volume.op_log();
      if cursor.journal_seq.saturating_add(1) < log.oldest_seq() {
        out.push(Invalidation::Everything);
      } else {
        let records: Vec<(Op, Box<str>, Option<InodeNo>)> = log
          .since(cursor.journal_seq)
          .map(|r| (r.op.clone(), r.path.clone(), r.inode))
          .collect();
        for (op, path, inode) in records {
          self.journal_invalidations(&op, &path, inode, out);
        }
      }
    }
    // 2. The base plane: every directory a watcher hint invalidated since the cursor, with the
    //    live entries beneath it, expired (not dropped: a busy directory must keep working) so the
    //    kernel revalidates each name and re-fetches its attributes on the next use.
    if let Some(host) = self.host.as_mut() {
      let stale = self
        .volume
        .with_host(host)
        .take_stale_base_entries(self.store, cursor.hint_seq)?;
      // An outsider's change carries no byte range: all of each file's cached bytes.
      for dir in stale.dirs {
        out.push(Invalidation::Inode {
          ino: dir.0,
          data: CachedBytes::ALL,
        });
      }
      for entry in stale.entries {
        out.push(Invalidation::Entry {
          parent: entry.dir.0,
          name: entry.name.into_string(),
          expire: true,
        });
        out.push(Invalidation::Inode {
          ino: entry.child.0,
          data: CachedBytes::ALL,
        });
      }
    }
    Ok(self.seen(cx))
  }

  fn seen(&mut self, _cx: &OpContext) -> InvalidationCursor {
    InvalidationCursor {
      journal_seq: self.volume.op_log().head_seq(),
      hint_seq: self.volume.base_plane().map_or(0, |plane| plane.hint_seq()),
    }
  }

  fn data_changes(
    &mut self,
    cx: &OpContext,
    since: u64,
    out: &mut Vec<crate::DataChange>,
  ) -> Result<bool, VfsError> {
    self.authorize_read(cx)?;
    // A pinned view never sees a head change (as `invalidations` holds), so nothing its kernel caches goes stale.
    if !matches!(cx.view, View::Current) {
      return Ok(true);
    }
    let log = self.volume.op_log();
    // A record after `since` that retention evicted is a change nobody can name any more.
    let complete = since.saturating_add(1) >= log.oldest_seq();
    out.extend(log.since(since).filter_map(crate::changed::data_change));
    Ok(complete)
  }

  fn journal_capacity(&mut self, _cx: &OpContext) -> usize {
    self.volume.op_log().capacity()
  }

  fn admit_states(&mut self, _cx: &OpContext, bytes: u64) -> bool {
    self.store.metadata.reserve(bytes).is_ok()
  }

  fn restored_open_files(&mut self, cx: &OpContext) -> Vec<u64> {
    self.volume.restored_open_files(cx.owner)
  }

  fn takeover_inodes(
    &mut self,
    cx: &OpContext,
    after: Option<u64>,
    visit: &mut dyn FnMut(u64) -> std::ops::ControlFlow<()>,
  ) -> bool {
    self.volume.visit_takeover_inodes(cx.owner, after, visit)
  }

  fn change_token(&mut self, object: ObjectId, cx: &OpContext) -> Result<u64, VfsError> {
    self.authorize_read(cx)?;
    // A view changes whenever its owner's attributes or working copy do; the owner's version moves
    // with every attribute change, and the working copy's size and times with every write.
    let inode = InodeNo(object.inode);
    let owner = inode.derived_from().unwrap_or(inode);
    self.volume.change_version(self.store, owner)
  }

  fn appledouble_lookup(
    &mut self,
    parent: ObjectId,
    cx: &OpContext,
    owner_name: &str,
  ) -> Result<NodeAttr, VfsError> {
    self.view_lookup(parent, cx, owner_name)
  }

  fn appledouble_create(
    &mut self,
    parent: ObjectId,
    cx: &OpContext,
    owner_name: &str,
    exclusive: bool,
  ) -> Result<NodeAttr, VfsError> {
    self.view_create(parent, cx, owner_name, exclusive)
  }

  fn appledouble_remove(
    &mut self,
    parent: ObjectId,
    cx: &OpContext,
    owner_name: &str,
  ) -> Result<(), VfsError> {
    self.view_remove(parent, cx, owner_name)
  }

  fn appledouble_rename(
    &mut self,
    from: ObjectId,
    to: ObjectId,
    cx: &OpContext,
    from_owner: &str,
    to_owner: &str,
  ) -> Result<bool, VfsError> {
    self.view_rename(from, to, cx, from_owner, to_owner)
  }

  fn statfs(&mut self, _object: ObjectId, cx: &OpContext) -> Result<FsStat, VfsError> {
    self.authorize_read(cx)?;
    let accounting = self.volume.accounting();
    // Truthful space (audit BUG-9; §4.6 "logical capacity and remaining space that the physical
    // claim can honor"): the total is the capacity the claim can honour now — a bounded volume's
    // whole quota, a dynamic volume's granted bytes plus what the shard budget could still admit,
    // never past its ceiling — the used is the referenced bytes, and the free is their difference.
    // Not an invented multiple of the used amount (the previous `blocks = 2 * used` / `free =
    // used`), and not a dynamic volume's bare `max` either (which showed a total the shard could
    // not back). The total and used are the pair the quota admits writes against, so `df` shows
    // the real capacity a write will be refused past. Block rounding is checked: a `div_ceil` on
    // each side, so the free count is rounded down and never over-promises a partial block.
    let block = u64::from(BLOCK_SIZE);
    let total_blocks = self
      .volume
      .honourable_capacity_bytes(self.store)
      .div_ceil(block);
    let used_blocks = accounting.referenced_bytes.div_ceil(block);
    let free_blocks = total_blocks.saturating_sub(used_blocks);
    // Backed inode availability (§4.2: "statfs includes backed inode availability"): the volume's
    // inode allowance is the total, and the free count is the allowance less its live inodes — the
    // same pair `next_no` admits creates against, so `df -i` shows the real inodes a create will be
    // refused past, not zero. (A dynamic-quota-derived allowance can be `u64::MAX`, an honest "not
    // bounded here"; the fixtures leave it unbounded for the determinism oracle.)
    let (live_inodes, inode_allowance) = self.volume.inode_usage();
    let free_inodes = inode_allowance.saturating_sub(live_inodes);
    Ok(FsStat {
      blocks: total_blocks,
      bfree: free_blocks,
      bavail: free_blocks,
      files: inode_allowance,
      ffree: free_inodes,
      bsize: BLOCK_SIZE,
      namelen: NAME_MAX,
      frsize: BLOCK_SIZE,
      // The volume's own name-equivalence policy (§4.5), so a transport reports the volume's real
      // case behaviour, not a fixed guess.
      case_sensitive: self.volume.policy().case_sensitive(),
    })
  }
}
