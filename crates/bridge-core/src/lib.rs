//! `slates-bridge-core` — the one transport-independent VFS operation layer (§4.6 "Bridge trait
//! (one VFS operation layer)"). Every OS bridge — the Linux FUSE driver, the macOS FSKit module
//! and its NFSv3 fallback, the Windows WinFsp binding, virtio-fs — turns its own wire protocol
//! into calls on the single [`Bridge`] trait defined here and encodes the neutral results back
//! into its own wire form. There is one implementation over the volume core, [`VolumeBridge`],
//! exercised on every host against an in-memory scratch volume with no mount, so the semantics
//! are written and tested once and every transport shares them (the design's "one implementation
//! in the core"; a second, parallel operation layer is banned, Part 2 item 7).
//!
//! The trait is neutral by construction. It is addressed by real inode numbers, never a
//! transport's own node convention: FUSE's "node id 1 is the root" and NFS's opaque file handles
//! are each translated at their own edge, both resolving the root through [`Bridge::root`]. Its
//! results are neutral POSIX attributes ([`NodeAttr`]), directory entries ([`DirEntry`]) and
//! filesystem statistics ([`FsStat`]). Its refusals are the volume core's own typed [`VfsError`],
//! which each transport maps to its wire error (a Linux errno, an `nfsstat3`); the neutral layer
//! never invents an errno, so no transport inherits another's numbering.

// The no-panic law (CLAUDE.md, banned item 6): shipped code never overflows or divides by zero. Test builds
// are exempt. Once a crate is clean this holds it there; out-of-bounds indexing and slicing are denied
// workspace-wide.
#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

pub mod access;
pub mod appledouble;
pub mod authority;
pub mod changed;
#[cfg(unix)]
pub mod dax_backing;
pub mod delivery;
pub mod links;
pub mod scoped;
pub mod volume_bridge;

pub use authority::{
  AttachmentId, Attachments, Barrier, CacheCoherence, ObjectId, OpContext, Rights, View,
};
pub use changed::{DataChange, TO_THE_END};
pub use volume_bridge::{LostWrites, VolumeBridge, new_handle_store};

use slates_vfs::error::VfsError;
use slates_vfs::inode::Kind;

/// The neutral POSIX attributes of a filesystem object, independent of any transport's wire form.
/// Times are nanoseconds since the Unix epoch (the volume core's native form); each transport
/// splits or narrows them as its protocol requires. The kind is carried explicitly because the
/// volume core's `mode` is permission bits alone, and a transport such as NFS needs the type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeAttr {
  /// The inode number.
  pub ino: u64,
  /// The generation, so a reused inode number is a distinct object.
  pub generation: u64,
  /// The object kind.
  pub kind: Kind,
  /// The permission bits (the type is [`NodeAttr::kind`]).
  pub mode: u32,
  /// The hard-link count.
  pub nlink: u32,
  /// The owner uid.
  pub uid: u32,
  /// The owner gid.
  pub gid: u32,
  /// The size in bytes.
  pub size: u64,
  /// The access time (nanoseconds since the Unix epoch).
  pub atime: i64,
  /// The modification time.
  pub mtime: i64,
  /// The change time.
  pub ctime: i64,
  /// The change counter: it moves on every change to the object and never repeats, whatever the
  /// wall clock does (A-38). A transport with a change attribute (NFSv4 `change`) serves it.
  pub change: u64,
}

/// One entry of a directory listing: the child's inode number, its kind, its name, and the cookie that
/// resumes the listing after it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
  /// The child inode number.
  pub ino: u64,
  /// The child kind.
  pub kind: Kind,
  /// The name.
  pub name: String,
  /// The cookie a listing resumed after this entry passes back: `.` 1, `..` 2, a child its name's hash
  /// cookie ([`slates_vfs::dir_cookie`]) — stable across other names' inserts and removals (§4.5; AUD-29-86).
  pub cookie: u64,
}

/// How many of `entries` a transport sends when the first `fits` of them fit its reply: the fitted entries
/// less any trailing ones sharing a cookie with the first entry that did not fit, since a listing resumed
/// from that cookie would skip them (§4.5; AUD-29-86). Zero when one group of a shared cookie is larger than
/// the reply — the transport refuses the page rather than skip an entry.
pub fn whole_cookie_groups(entries: &[DirEntry], fits: usize) -> usize {
  let Some(next) = entries.get(fits) else {
    return fits.min(entries.len());
  };
  let mut sent = fits;
  while let Some(before) = sent.checked_sub(1)
    && entries
      .get(before)
      .is_some_and(|entry| entry.cookie == next.cookie)
  {
    sent = before;
  }
  sent
}

/// The changes a `setattr` applies: a field is `Some` when the caller asks to set it. Each
/// transport translates its own protocol's "which fields are valid" (FUSE's `valid` bitmask, an
/// NFS `sattr3`) into this neutral form, so the operation layer needs no transport's flag bits.
/// Every settable POSIX field is present: a field the caller did not ask for is `None`, and an
/// implementation applies or refuses each requested field — it never acknowledges one it ignored
/// (§4.6 "POSIX and transparency acceptance"; audit BUG-8, "the reduced Bridge signature").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SetAttr {
  /// A new size (a truncate), if set.
  pub size: Option<u64>,
  /// New permission bits, if set.
  pub mode: Option<u32>,
  /// A new owner uid, if set.
  pub uid: Option<u32>,
  /// A new owner gid, if set.
  pub gid: Option<u32>,
  /// A new access time (nanoseconds since the Unix epoch), if set.
  pub atime: Option<i64>,
  /// A new modification time (nanoseconds since the Unix epoch), if set.
  pub mtime: Option<i64>,
  /// A new change time (nanoseconds since the Unix epoch), if set. Only a kernel flushing the
  /// timestamps it kept under a writeback cache asks for this (FUSE `FATTR_CTIME`); NFSv3's `sattr3`
  /// and FSKit carry no such field and leave it `None`, and the change time then advances to the
  /// volume's clock as for any attribute change.
  pub ctime: Option<i64>,
}

/// The `renameat2` flags a rename carries. The operation layer preserves them and an
/// implementation applies or refuses them — it never silently drops one and performs an ordinary
/// rename (§4.6; audit BUG-10, "`Bridge::rename` drops the flags"). `RENAME_WHITEOUT` is an
/// overlay concern of the landing plane, not a mount operation, and is not represented here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenameFlags {
  /// `RENAME_NOREPLACE`: fail if the destination already exists, rather than replacing it.
  pub no_replace: bool,
  /// `RENAME_EXCHANGE`: atomically exchange the two paths, both of which must exist.
  pub exchange: bool,
}

/// Neutral filesystem statistics (a `statvfs`), which a transport encodes into its own `statfs`
/// or `FSSTAT` reply.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FsStat {
  /// Total blocks, of [`FsStat::bsize`] bytes each.
  pub blocks: u64,
  /// Free blocks.
  pub bfree: u64,
  /// Blocks available to an unprivileged caller.
  pub bavail: u64,
  /// Total file slots (0 when the volume does not cap them).
  pub files: u64,
  /// Free file slots.
  pub ffree: u64,
  /// The block size in bytes.
  pub bsize: u32,
  /// The maximum name length.
  pub namelen: u32,
  /// The fragment size in bytes.
  pub frsize: u32,
  /// Whether the volume distinguishes names differing only in case (or normalization): true for an
  /// exact-name volume (Linux/POSIX), false for a case-folding one (APFS's default). A transport that
  /// reports case behaviour reads it here — NFS `PATHCONF`'s `case_insensitive`, and WinFsp/FSKit as
  /// they mature — so the property lives once, in the neutral filesystem-info the volume already fills.
  pub case_sensitive: bool,
}

/// How long a transport's kernel may cache an object's name and attributes (§4.6 "Cache
/// posture"): forever for an object only the daemon mutates — it invalidates explicitly through
/// [`Bridge::invalidations`] — and a bounded window for a live base source, which an outsider may
/// change beneath the volume without a hint ("watchers may miss outsider writes"), so the window
/// is the base filesystem's timestamp granularity: the span inside which a revalidation could not
/// tell a change apart anyway.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheLifetime {
  /// Until an explicit invalidation.
  Forever,
  /// At most `ns` nanoseconds, then revalidate through the seam.
  Bounded {
    /// The window in nanoseconds.
    ns: u64,
  },
}

/// Whether a transport offers its guest per-inode DAX (§4.6; the Shards route, part 3, `docs/wip/shards/dax-protocol.md`):
/// what its `FUSE_INIT` reply keeps of the kernel's DAX flags, and which files' attributes say `FUSE_ATTR_DAX`.
/// Withheld everywhere but the Shards route, whose daemon answers a DAX window's descriptor ask with a confined object.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DaxOffer {
  /// No DAX: the `INIT` reply keeps neither `FUSE_HAS_INODE_DAX` nor `FUSE_MAP_ALIGNMENT`, and no file is marked.
  #[default]
  Withheld,
  /// Per-inode DAX: the `INIT` reply keeps `FUSE_HAS_INODE_DAX` and `FUSE_MAP_ALIGNMENT` where the kernel offers them,
  /// and a regular file of at least `smallest` bytes is marked `FUSE_ATTR_DAX` while `new_files` holds (the daemon's
  /// backing budget has room for another file).
  PerInode {
    /// The smallest file marked: one of the ranges a guest maps a file in (`FUSE_DAX_SZ`).
    smallest: u64,
    /// Whether files are marked now.
    new_files: bool,
  },
}

/// The store's parts a backed DAX session works with, lent by a bridge that has a store ([`Bridge::dax_state`]).
pub struct DaxState<'a> {
  /// The store's DAX backings, which the volume's write boundary writes every change of a backed file through.
  pub backings: &'a mut slates_vfs::shards_backing::ShardsBackings,
  /// The in-process DAX registry: a file a guest maps writable there cannot also be backed.
  pub registry: &'a slates_vfs::dax::DaxRegistry,
  /// The prefix of the volume the bridge serves ([`slates_vfs::volume::Volume::prefix`]): with an inode number, what
  /// names a file in the store, since a clone shares its origin's numbers.
  pub volume: u16,
}

/// One kernel cache entry a transport must drop or expire (§4.6 `notify`): the seam produces
/// these for every change the transport did not make itself — a mutation through the SDK or
/// another attachment (from the volume's journal) or an outsider's change beneath a base
/// directory (from a watcher hint) — and the transport writes them to its kernel before it
/// serves the next request, so a tool never reads a name or attributes newer than the daemon's
/// view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Invalidation {
  /// The cached mapping of `name` in directory `parent` is stale. `expire` asks the kernel to
  /// revalidate the name on its next use rather than drop it now — the live-source case, where a
  /// busy directory must not be torn down (FUSE `FUSE_EXPIRE_ONLY`); a mutation the daemon applied
  /// drops it outright.
  Entry {
    /// The directory.
    parent: u64,
    /// The name.
    name: String,
    /// Expire rather than drop.
    expire: bool,
  },
  /// The cached attributes of `ino` are stale, and the cached bytes `data` names with them.
  Inode {
    /// The inode.
    ino: u64,
    /// Which of its cached bytes are stale as well.
    data: CachedBytes,
  },
  /// Anything the transport's kernel holds of this volume may be stale: the journal no longer holds every record since
  /// the transport's cursor, so which names, attributes and pages changed can no longer be told
  /// (`docs/bugs/2026-10-22-a-mount-that-falls-behind-the-journal-never-hears-of-the-changes-it-missed.md`). The
  /// transport invalidates every name it handed out and every inode its kernel still references, then resumes from the
  /// cursor returned with it. Writers never wait for a transport that falls behind; this is what it costs instead.
  Everything,
}

/// Which of an inode's cached bytes an [`Invalidation::Inode`] makes stale along with its attributes (§4.6 "Cache
/// posture"; A-146). Only the bytes a change touched are named. A guest's DAX map of a file shared through a mount is
/// held in the kernel's cache, and a whole-file drop for every change made it re-fetch the file: measured on Linux
/// 7.2.9 under crosvm (2026-10-22), each one-byte change to a 128 MiB file cost the guest's next scan 356–382 ms, against
/// 0.2–0.3 ms warm. A transport drops at least the pages holding the bytes named; dropping more is never stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CachedBytes {
  /// None: a metadata change (mode, ownership, an extended attribute) leaves the pages alone.
  Kept,
  /// The bytes in `[at, at + len)`: an overwrite in place, or bytes appended past the old end.
  Range {
    /// The first byte.
    at: u64,
    /// How many bytes.
    len: u64,
  },
  /// Every byte from `at` to the end of the file. A truncate drops from its new length, and an insert or a delete from
  /// where it shifts the rest; [`CachedBytes::ALL`] is used where what changed has no byte range — a directory's
  /// listing, or a base file an outsider changed.
  From {
    /// The first byte.
    at: u64,
  },
}

impl CachedBytes {
  /// The whole file.
  pub const ALL: CachedBytes = CachedBytes::From { at: 0 };
}

/// Where a transport has delivered invalidations up to: its position in the volume's journal and
/// in the base plane's hint sequence. The transport owns it (it owns the kernel cache), takes it
/// from [`Bridge::seen`] after its own requests — its own changes need no invalidation in its own
/// kernel — and passes it to [`Bridge::invalidations`] before the next; the seam keeps no
/// per-transport state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InvalidationCursor {
  /// The newest journal sequence already accounted for.
  pub journal_seq: u64,
  /// The newest hint sequence already accounted for.
  pub hint_seq: u64,
}

/// The one VFS operation layer (§4.6). Every request identifies its object by [`ObjectId`] (a
/// real inode number and a generation, §4.6 `(no, gen)`) and rides an authenticated [`OpContext`]
/// built by the owner from a validated attachment: the seam checks the context's volume, granted
/// rights and view before any effect, so authority is enforced once, for every transport, at the
/// seam — not re-invented at each edge. A refusal is the volume core's typed [`VfsError`], which
/// the transport maps to its own wire error. Only the operations the transports dispatch are here;
/// the set grows with the drivers.
pub trait Bridge {
  /// The root directory's inode number, under `cx` (the attachment's volume must be this bridge's).
  /// FUSE resolves node id 1 to it; NFS mints the export's root file handle from it.
  fn root(&mut self, cx: &OpContext) -> Result<u64, VfsError>;
  /// Whether an effect a client keeps across a restart waits for the volume's next publication
  /// ([`slates_vfs::volume::Volume::awaits_publication`]): a transport's reply then waits for the owner's barrier (D-18).
  fn awaits_publication(&self) -> bool;
  /// Look `name` up in directory `parent` under `cx`; the child's attributes.
  fn lookup(&mut self, parent: ObjectId, cx: &OpContext, name: &str) -> Result<NodeAttr, VfsError>;
  /// The attributes of `object` under `cx`.
  fn getattr(&mut self, object: ObjectId, cx: &OpContext) -> Result<NodeAttr, VfsError>;
  /// Open `object` under `cx`; the file handle.
  fn open(&mut self, object: ObjectId, cx: &OpContext, flags: u32) -> Result<u64, VfsError>;
  /// Read `size` bytes at `offset` from `object` into `out`, under the authenticated `cx`. The
  /// object is addressed by identity (§4.6), not an open handle; `cx` carries the view and the
  /// granted access, and a read the context does not authorize is refused.
  fn read(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    offset: u64,
    size: u32,
    out: &mut Vec<u8>,
  ) -> Result<(), VfsError>;
  /// The value of `object`'s extended attribute `name`, under `cx` (which must allow reading);
  /// `VfsError::NoAttribute` when it has none by that name (§4.5 "Extended attributes").
  fn xattr_get(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    name: &[u8],
  ) -> Result<Vec<u8>, VfsError>;
  /// Sets `object`'s extended attribute `name` to `value` as `how` allows (create, replace or either),
  /// under `cx` (which must allow writing).
  fn xattr_set(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    name: &[u8],
    value: &[u8],
    how: slates_vfs::xattr::XattrSet,
  ) -> Result<(), VfsError>;
  /// The names of `object`'s extended attributes, ascending by bytes, under `cx` (which must allow
  /// reading).
  fn xattr_list(&mut self, object: ObjectId, cx: &OpContext) -> Result<Vec<Box<[u8]>>, VfsError>;
  /// Removes `object`'s extended attribute `name` (`VfsError::NoAttribute` when it has none), under
  /// `cx` (which must allow writing).
  fn xattr_remove(&mut self, object: ObjectId, cx: &OpContext, name: &[u8])
  -> Result<(), VfsError>;
  /// The first offset at or after `offset`, before the end of `object`, that holds data (`data`) or
  /// lies in a hole (`!data`), under the authenticated `cx` (which must allow reading); `None` when
  /// there is none (RFC 7862 §15.11 SEEK; `slates_vfs::volume::Volume::seek`).
  fn seek(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    offset: u64,
    data: bool,
  ) -> Result<Option<u64>, VfsError>;
  /// Write `data` at `offset` to `object` under the authenticated `cx`; the bytes written. A write
  /// against a read-only attachment or a pinned view is refused before any effect.
  fn write(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    offset: u64,
    data: &[u8],
  ) -> Result<u32, VfsError>;
  /// Admits an allocation of `[offset, offset + len)` of file `object` under `cx` (which must allow writing): the
  /// quota is charged for every byte of the range not yet held, all of it or none (A-108, `fallocate` mode 0).
  fn admit_allocation(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    offset: u64,
    len: u64,
  ) -> Result<(), VfsError>;
  /// Materializes the holes of `[offset, offset + len)` of file `object` with zeros under `cx`, extending its size to
  /// the range's end (A-108). The caller sizes the range to one cooperative slice and admits the whole request
  /// first with [`Bridge::admit_allocation`].
  fn allocate(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    offset: u64,
    len: u64,
  ) -> Result<(), VfsError>;
  /// Open directory `object` under `cx`; the handle.
  fn opendir(&mut self, object: ObjectId, cx: &OpContext) -> Result<u64, VfsError>;
  /// One page of directory `object`'s listing under `cx`, resumed from `cookie` (0 starts it; each entry
  /// carries the cookie that resumes after it): at most `limit` entries, then every further entry sharing
  /// the last one's cookie, so a transport can cut the page between cookies ([`whole_cookie_groups`]). A
  /// page is one descent and its own entries, so a listing paged to its end visits each entry once (§4.5;
  /// AUD-29-86).
  fn readdir(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    fh: u64,
    cookie: u64,
    limit: usize,
  ) -> Result<Vec<DirEntry>, VfsError>;
  /// Create `name` in `parent` and open it, under `cx`; the attributes and the handle.
  fn create(
    &mut self,
    parent: ObjectId,
    cx: &OpContext,
    name: &str,
    mode: u32,
    flags: u32,
  ) -> Result<(NodeAttr, u64), VfsError>;
  /// Creates a FIFO/socket name, with no open reference or kernel endpoint (A-26).
  fn mknod(
    &mut self,
    parent: ObjectId,
    cx: &OpContext,
    name: &str,
    mode: u32,
    kind: Kind,
  ) -> Result<NodeAttr, VfsError>;
  /// Release handle `fh` of `object`, under `cx`.
  fn release(&mut self, object: ObjectId, cx: &OpContext, fh: u64) -> Result<(), VfsError>;
  /// Takes one lookup reference on `object`, under `cx`: the transport is handed an object it may
  /// address later, so it pins the object's lifetime until it forgets it. This is a per-transport
  /// action — **FUSE** takes one per `LOOKUP`/`CREATE`/`MKDIR`/readdirplus entry and drops them on
  /// `FORGET`; **NFS adds no references** (it has no `FORGET`, §3 of the inode-addressed-io design),
  /// so the NFS edge never calls this. The shared operations therefore do not reference implicitly;
  /// the transport that owns references calls this explicitly.
  fn reference(&mut self, object: ObjectId, cx: &OpContext) -> Result<(), VfsError>;
  /// The transport drops `nlookup` references to `object`, under `cx` (its attachment's volume).
  fn forget(&mut self, object: ObjectId, cx: &OpContext, nlookup: u64);
  /// Flush handle `fh` of `object` under `cx` (no disk write; success once the data is in the
  /// anchor).
  fn flush(&mut self, object: ObjectId, cx: &OpContext, fh: u64) -> Result<(), VfsError>;
  /// `fsync` of handle `fh` of `object` under `cx`: what [`Bridge::flush`] does, and also the point a writeback
  /// error is consumed — reported here once, where every `flush` (each close of any copy of the descriptor,
  /// a child's exec-time close included) reports it without consuming it (A-61, [`LostWrites`]).
  fn fsync(&mut self, object: ObjectId, cx: &OpContext, fh: u64) -> Result<(), VfsError> {
    self.flush(object, cx, fh)
  }
  /// Create directory `name` in `parent` under `cx`; the attributes.
  fn mkdir(
    &mut self,
    parent: ObjectId,
    cx: &OpContext,
    name: &str,
    mode: u32,
  ) -> Result<NodeAttr, VfsError>;
  /// Remove `name` from `parent` under `cx`.
  fn unlink(&mut self, parent: ObjectId, cx: &OpContext, name: &str) -> Result<(), VfsError>;
  /// Remove directory `name` from `parent` under `cx`.
  fn rmdir(&mut self, parent: ObjectId, cx: &OpContext, name: &str) -> Result<(), VfsError>;
  /// Create a symlink `name` in `parent` pointing at `target`, under `cx`; the attributes.
  fn symlink(
    &mut self,
    parent: ObjectId,
    cx: &OpContext,
    name: &str,
    target: &str,
  ) -> Result<NodeAttr, VfsError>;
  /// Create a hard link `new_name` in `new_parent` pointing at the existing `target`, under `cx`;
  /// the target's attributes (with the incremented link count). A directory target is refused.
  fn link(
    &mut self,
    target: ObjectId,
    new_parent: ObjectId,
    cx: &OpContext,
    new_name: &str,
  ) -> Result<NodeAttr, VfsError>;
  /// The target of symlink `object` under `cx`.
  fn readlink(&mut self, object: ObjectId, cx: &OpContext) -> Result<String, VfsError>;
  /// Rename `old_name` under `old_parent` to `new_name` under `new_parent`, under `cx`, honoring
  /// or refusing the `renameat2` `flags` (never silently dropping them).
  fn rename(
    &mut self,
    old_parent: ObjectId,
    new_parent: ObjectId,
    cx: &OpContext,
    old_name: &str,
    new_name: &str,
    flags: RenameFlags,
  ) -> Result<(), VfsError>;
  /// Apply `changes` to `object` under `cx`; the new attributes.
  fn setattr(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    changes: SetAttr,
  ) -> Result<NodeAttr, VfsError>;
  /// Filesystem statistics for the volume `object` lives in, under `cx`.
  fn statfs(&mut self, object: ObjectId, cx: &OpContext) -> Result<FsStat, VfsError>;
  /// The volume's current wall-clock time (nanoseconds since the Unix epoch). A transport uses it
  /// to resolve a "set to now" time — NFS `SET_TO_SERVER_TIME`, FUSE `UTIME_NOW` — into the explicit
  /// value `setattr` takes; the NOW resolution the design places at the transport (AC-3.10).
  fn now(&mut self) -> i64;
  /// A token that changes whenever `object` is mutated — its monotonic change version (§4.5). A
  /// transport that must detect a change between two calls uses it: the NFS `READDIR` cookieverf is
  /// this token, so a continuation against a directory mutated since the listing began is refused.
  fn change_token(&mut self, object: ObjectId, cx: &OpContext) -> Result<u64, VfsError>;
  /// Releases every reference the attachment named by `cx` holds, in one bounded batch, reclaiming
  /// the inodes that reach zero references and no links (§3 the teardown sweep). The transport edge
  /// calls this once when an attachment ends — a FUSE unmount, a lost connection — since FUSE does
  /// not guarantee a `FORGET` per outstanding reference. Idempotent.
  fn sweep_attachment(&mut self, cx: &OpContext) -> Result<(), VfsError>;

  /// Whether `object` lies in the subtree of directory inode `scope` (§4.6 scoped exports; AUD-29-76): the
  /// question a [`scoped::ScopedBridge`] asks of every object a request names. A bridge that cannot tell admits
  /// only the scope itself — conservative, never wider.
  fn within(&mut self, object: ObjectId, scope: u64, _cx: &OpContext) -> Result<bool, VfsError> {
    Ok(object.inode == scope)
  }

  /// Mint a DAX mapping of the **open chunk** of `object` covering `file_offset` (§4.6 A-9, AC-4.12,
  /// "DAX maps the open frontier"; `slates_vfs::dax`): the daemon pins the arena block and registers the mapping, and
  /// the returned [`slates_vfs::dax::Mapping`] names the block the virtio-fs device places in its DAX window (a
  /// `MAP_FIXED` of the content arena's memfd, or a `SHMEM_MAP` to the front end), so the guest reaches the block
  /// through its own page table with no per-access daemon call. A single-block file maps at `file_offset` 0; a
  /// multi-block file maps the open tail covering `file_offset`, while a sealed chunk (ciphertext at rest) is refused
  /// so the guest FUSE_READs that range. `writable` copies the block up first (`Volume::dax_map_write`) so it is this
  /// version's alone. The default is *no DAX window on this transport* — plain `/dev/fuse` and the scratch bridges
  /// never serve it — refused `SpecialFileOperation` (EOPNOTSUPP); a transport advertises DAX in FUSE_INIT only when
  /// its bridge overrides this behind the gate.
  fn dax_map(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    writable: bool,
    file_offset: usize,
  ) -> Result<slates_vfs::dax::Mapping, VfsError> {
    let _ = (object, cx, writable, file_offset);
    Err(VfsError::SpecialFileOperation)
  }

  /// Revoke a DAX mapping a guest gave up (FUSE_REMOVEMAPPING): unregister it and unpin its block so the block may
  /// be reused. The caller (the device) must have already torn the guest's window range down — the unpin is here,
  /// **after** the window `munmap`, never before, or the guest's page table could outlive the pin and reach a
  /// reused block. `Ok(true)` if the mapping was live, `Ok(false)` if it was already gone. The default holds no
  /// mappings.
  fn dax_unmap(
    &mut self,
    mapping: &slates_vfs::dax::Mapping,
    cx: &OpContext,
  ) -> Result<bool, VfsError> {
    let _ = (mapping, cx);
    Ok(false)
  }

  /// The backing descriptor, byte offset and length of a **live** mapping's block (FUSE_SETUPMAPPING), for the device
  /// to map its pages into the VMM's DAX cache region. `None` for a revoked mapping, a scratch (anonymous-backed)
  /// volume with no descriptor to hand a VMM, a mapping of another volume, or a bridge that serves no DAX. Unix only
  /// (the DAX window is a Linux/vhost-user feature). The default serves none.
  #[cfg(unix)]
  fn dax_backing(
    &self,
    mapping: &slates_vfs::dax::Mapping,
  ) -> Option<(std::os::fd::BorrowedFd<'_>, usize, usize)> {
    let _ = mapping;
    None
  }

  /// Phase one of a live-guest DAX revoke (FUSE_REMOVEMAPPING, or a content-path teardown): unregister the mapping so
  /// the hardware path is inert and no new access is authorized, but **keep the block pinned** until the device has
  /// unmapped the window and the guest's TLB is flushed. `Ok(true)` if it was live. The default holds no mappings.
  fn dax_revoke_pending(
    &mut self,
    mapping: &slates_vfs::dax::Mapping,
    cx: &OpContext,
  ) -> Result<bool, VfsError> {
    let _ = (mapping, cx);
    Ok(false)
  }

  /// Phase two: unpin the block of a mapping revoked by [`Bridge::dax_revoke_pending`], **after** the device confirms
  /// the window unmap and the cross-core TLB flush, so the block may be reused — never before, or a stale guest page
  /// table could reach a reused block (the one cross-tenant hazard once spatial isolation holds). `Ok(true)` when the
  /// hold was dropped. The default holds no mappings.
  fn dax_release_after_unmap(
    &mut self,
    mapping: &slates_vfs::dax::Mapping,
    cx: &OpContext,
  ) -> Result<bool, VfsError> {
    let _ = (mapping, cx);
    Ok(false)
  }

  /// The inodes a daemon-side revoke marked revoke-pending (§4.6 step 5): a write/truncate of a mapped inode (or a
  /// snapshot/clone of its volume) left the mapping revoked but its block pinned, awaiting the device to munmap the
  /// guest's window. Each is `(inode, writable_only)`: `writable_only` false tears **every** window of the inode (a
  /// write makes all stale), true tears only the writable ones and keeps the read-only (a snapshot shares the block
  /// read-only). The device serve loop drains this each pass, so a block the daemon is about to reuse or freeze is
  /// unmapped from the guest before it is unpinned. A bridge with no DAX registry has none.
  fn take_pending_dax_revokes(&mut self) -> Vec<(u64, bool)> {
    Vec::new()
  }

  /// Whether a live DAX window's `mapping` still names its file's current content block in this bridge's view (§4.6
  /// "revoke on advance"). After a snapshot reader `advance`s its view, the device re-validates each window it holds
  /// through this and tears any that no longer passes — the inodes whose content changed between the old view and
  /// the new — so a hardware guest re-maps against the new view rather than reading stale bytes through a lingering
  /// window. Unlike a write or truncate (which revokes every reader's windows of the inode), an advance moves one
  /// reader, so its device validates only its own windows against its own view here. The default serves no DAX and
  /// holds no window, so nothing is current: `false`.
  fn dax_window_current(&mut self, mapping: &slates_vfs::dax::Mapping, cx: &OpContext) -> bool {
    let _ = (mapping, cx);
    false
  }

  /// The AppleDouble view of `owner_name`'s extended attributes in directory `parent` (§4.6
  /// "Extended attributes over NFSv3"): the `._owner_name` file a macOS NFSv3 client keeps
  /// attributes in, served from the volume's attribute store. Its number is the owner's derived
  /// number, and getattr, read, write and setattr on that number act on the view. `NotFound` when the
  /// owner has no view. A bridge without extended attributes has no views.
  fn appledouble_lookup(
    &mut self,
    _parent: ObjectId,
    _cx: &OpContext,
    _owner_name: &str,
  ) -> Result<NodeAttr, VfsError> {
    Err(VfsError::NotFound)
  }

  /// Creates `owner_name`'s AppleDouble view (a client's create of `._owner_name`), returning an
  /// existing one unless `exclusive`, when it is refused `AlreadyExists`.
  fn appledouble_create(
    &mut self,
    _parent: ObjectId,
    _cx: &OpContext,
    _owner_name: &str,
    _exclusive: bool,
  ) -> Result<NodeAttr, VfsError> {
    Err(VfsError::NotFound)
  }

  /// A rename of `._from_owner` in `from` onto `._to_owner` in `to`: the rename a macOS client
  /// issues to carry a sidecar along with a file it has just renamed. `Ok(true)` when the view
  /// completed it (the sidecar already followed its owner, or an orphan sidecar was merged into the
  /// owner); `Ok(false)` when it is an ordinary rename for the caller to perform.
  fn appledouble_rename(
    &mut self,
    _from: ObjectId,
    _to: ObjectId,
    _cx: &OpContext,
    _from_owner: &str,
    _to_owner: &str,
  ) -> Result<bool, VfsError> {
    Ok(false)
  }

  /// Removes `owner_name`'s AppleDouble view and the attributes it carries.
  fn appledouble_remove(
    &mut self,
    _parent: ObjectId,
    _cx: &OpContext,
    _owner_name: &str,
  ) -> Result<(), VfsError> {
    Err(VfsError::NotFound)
  }
  /// How long the transport's kernel may cache `object`'s name and attributes (§4.6 "Cache
  /// posture"): forever unless the object follows a live base source. A transport stamps every
  /// entry and attribute reply with it. The default is forever, for a bridge with no live source.
  fn cache_lifetime(&mut self, _object: ObjectId, _cx: &OpContext) -> CacheLifetime {
    CacheLifetime::Forever
  }
  /// Whether this bridge's transport offers its guest per-inode DAX ([`DaxOffer`]): withheld by default, offered only by
  /// the Shards route's serve task.
  fn dax_offer(&mut self) -> DaxOffer {
    DaxOffer::Withheld
  }

  /// The store's DAX backings and its in-process DAX registry, lent to a backed DAX session (`dax_backing`) that a
  /// transport drives over this bridge. `None` for a bridge with no store.
  fn dax_state(&mut self) -> Option<DaxState<'_>> {
    None
  }

  /// Makes file `object` ready for a guest to map writable from a copy of its own (Shards part 3, step 4; §4.6 DAX
  /// rights). Beneath an overlay the file is copied up first, as its first write would be
  /// (`slates_vfs::base::Overlay::witness_content`), so what the volume serves for it changes only through the write
  /// boundary or is refused. Needs `cx` to allow writing. The default, a bridge with no base, has nothing to do.
  fn witness_for_writable_map(&mut self, object: ObjectId, cx: &OpContext) -> Result<(), VfsError> {
    let _ = (object, cx);
    Ok(())
  }
  /// Appends to `out` every kernel invalidation owed since `cursor` — changes the transport
  /// named by `cx` did not make itself — and returns the cursor to pass next time. Bounded by the
  /// journal's retention budget and by the entries loaded under the hinted directories. The
  /// default reports nothing, for a bridge that changes only through its own transport.
  fn invalidations(
    &mut self,
    _cx: &OpContext,
    cursor: InvalidationCursor,
    _out: &mut Vec<Invalidation>,
  ) -> Result<InvalidationCursor, VfsError> {
    Ok(cursor)
  }
  /// The cursor at this moment: a transport takes it after serving its own request, so its own
  /// changes are never invalidated in its own kernel, and at mount time, when the kernel holds
  /// nothing yet.
  fn seen(&mut self, _cx: &OpContext) -> InvalidationCursor {
    InvalidationCursor::default()
  }
  /// Appends to `out` every change to a file's bytes the volume's journal recorded after sequence `since`
  /// ([`DataChange`]), oldest first, and answers whether those are all of them: `false` when retention evicted records
  /// after `since`. A transport consumes them alongside its invalidations to know which cached bytes its kernel holds
  /// stale ([`changed`], §4.6 "Cache posture"). The default reports nothing and calls it complete, as
  /// [`Bridge::invalidations`]' does, for a bridge that changes only through its own transport.
  fn data_changes(
    &mut self,
    _cx: &OpContext,
    _since: u64,
    _out: &mut Vec<DataChange>,
  ) -> Result<bool, VfsError> {
    Ok(true)
  }
  /// The most journal records the volume ever retains ([`slates_vfs::journal::OpLog::capacity`]): a transport that
  /// keeps spans of its own records' sequences keeps no more than this, since more are more than the journal can still
  /// be asked about. The default, zero, is a bridge with no journal.
  fn journal_capacity(&mut self, _cx: &OpContext) -> usize {
    0
  }
  /// Reserves `bytes` of the shard's metadata ledger (§4.2, the metadata dimension) for state the transport keeps on
  /// this attachment's behalf: a FUSE mount's byte states. Whole or not at all; `true` when reserved. What a transport
  /// was admitted is its owner's to return to the ledger when the attachment ends. The default, `false`, is a bridge
  /// with no ledger.
  fn admit_states(&mut self, _cx: &OpContext, _bytes: u64) -> bool {
    false
  }
  /// The files this attachment held open before a takeover, as far as the last publication counted, whose opens are
  /// not given back yet (A-152): what a mount taken over by this daemon tracks from its start. The default, none, is a
  /// bridge with nothing restored.
  fn restored_open_files(&mut self, _cx: &OpContext) -> Vec<u64> {
    Vec::new()
  }
  /// Visits the inodes whose cached state this attachment's kernel may hold older than the volume's after a takeover
  /// (A-158), in inode order and each once, starting after `after`, until `visit` breaks; whether every one was
  /// visited. The set is the references the recovery gave back to the attachment, with any taken since, and the inodes
  /// the write-log replay wrote. A mount taken over by this daemon invalidates them in slices from its first turn, so
  /// nothing the dead daemon changed and never told the kernel of outlives the takeover. The default, none, is a bridge
  /// with nothing restored.
  fn takeover_inodes(
    &mut self,
    _cx: &OpContext,
    _after: Option<u64>,
    _visit: &mut dyn FnMut(u64) -> std::ops::ControlFlow<()>,
  ) -> bool {
    true
  }
}
