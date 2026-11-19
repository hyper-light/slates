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
// The panic rule: a print macro panics on a refused write; lines go through `slates_machine::log_line!` (A-213).
#![deny(clippy::print_stdout, clippy::print_stderr)]
#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

pub mod access;
pub mod appledouble;
pub mod authority;
pub mod changed;
pub mod dax_backing;
pub mod delivery;
pub mod durable_bound;
pub mod group_rule;
pub mod links;
pub mod posix_acl;
pub mod scoped;
pub mod volume_bridge;

pub use authority::{
  AttachmentId, Attachments, Barrier, CacheCoherence, ObjectId, OpContext, Rights, View,
};
pub use changed::{DataChange, TO_THE_END};
pub use group_rule::GroupRule;
pub use volume_bridge::{LostWrites, VolumeBridge, new_handle_store};

use slates_vfs::error::VfsError;
use slates_vfs::inode::{DeviceNumber, Kind, SpecialNode};

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
  /// The birth time: when the object was made, unless a host whose API sets a creation time set another (WinFsp's
  /// `SetFileTime`, A-259).
  pub btime: i64,
  /// The Windows attribute bits kept beside the mode, at Windows' values (A-262): `HIDDEN`, `SYSTEM`, `ARCHIVE`,
  /// `OFFLINE`, `NOT_CONTENT_INDEXED`, and a directory's `READONLY` (a file's is its mode). Only the WinFsp drive
  /// reports them.
  pub windows: u16,
  /// The change counter: it moves on every change to the object and never repeats, whatever the
  /// wall clock does (A-38). A transport with a change attribute (NFSv4 `change`) serves it.
  pub change: u64,
  /// A device node's number (A-294), which each transport reports in its own encoding (FUSE's `rdev`, NFSv3's
  /// `rdev`, NFSv4's `rawdev`); `None` for every other kind.
  pub device: Option<DeviceNumber>,
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
  /// A new birth time (nanoseconds since the Unix epoch), if set: only a host whose API sets a creation time asks for
  /// it (WinFsp's `SetFileTime`, A-259); POSIX has no call that sets one, so FUSE, NFS and FSKit leave it `None`.
  pub btime: Option<i64>,
  /// New Windows attribute bits, if set (A-262): only a host whose API sets them asks (WinFsp's `SetBasicInfo` and
  /// create); the volume keeps those [`slates_vfs::inode::WindowsAttributes::kept`] allows for the object's kind.
  pub windows: Option<u32>,
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

/// How a guest's run sees the owners of the volume's files (§4.6, the Shards route; A-281): the identity on every
/// transport but a Shards stream whose guest named its run's owner with Shards' owner word (`trusted.shards.owner`).
/// There the owner the volume keeps for what its host made (the stream's root's owner when the word came) is shown to
/// the guest as the run's owner, and the run's owner as that one, uid and gid each: a swap of two ids, so it is its own
/// inverse. A chown the guest asks and the stat it reads back agree, two ids never collapse into one, and every other
/// route (a host mount, NFS, the SDK, a landing) sees the owners the volume keeps.
///
/// Why a swap and not Shards' own rule (its server shows every file "the host made" as the run's): slates keeps a real
/// owner on every file, the host's for a base file and the creator's for the rest, where Shards' server keeps the
/// guest's owner in an attribute beside a host file the host user owns. A one-way rule (the host's id shown as the
/// run's) would show the run's own files and the host's alike, and leave nothing for a chown to the host's id to store.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OwnerView {
  /// The two owners swapped, none for the identity.
  swap: Option<OwnerSwap>,
}

/// The two owners an [`OwnerView`] swaps: the volume's and the run's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OwnerSwap {
  /// The owner (uid, gid) the volume keeps for what its host made.
  kept: (u32, u32),
  /// The owner (uid, gid) the run named.
  run: (u32, u32),
}

impl OwnerView {
  /// The view every transport has until a guest names its run's owner: each id shown as the volume keeps it.
  pub const IDENTITY: OwnerView = OwnerView { swap: None };

  /// The view that shows the volume's owner `kept` (uid, gid) as the run's owner `run`, and `run` as `kept`.
  pub const fn swapping(kept: (u32, u32), run: (u32, u32)) -> OwnerView {
    OwnerView {
      swap: Some(OwnerSwap { kept, run }),
    }
  }

  /// The uid the guest is shown for a file the volume keeps as `kept`'s.
  pub fn shown_uid(&self, kept: u32) -> u32 {
    self.swap.map_or(kept, |s| swapped(kept, s.kept.0, s.run.0))
  }

  /// The gid the guest is shown for a file the volume keeps as `kept`'s.
  pub fn shown_gid(&self, kept: u32) -> u32 {
    self.swap.map_or(kept, |s| swapped(kept, s.kept.1, s.run.1))
  }

  /// The uid the volume keeps for an id the guest names (a caller, a creator, a chown): the inverse of
  /// [`OwnerView::shown_uid`], which, a swap being its own inverse, is the same exchange.
  pub fn kept_uid(&self, shown: u32) -> u32 {
    self.shown_uid(shown)
  }

  /// The gid the volume keeps for a gid the guest names: the inverse of [`OwnerView::shown_gid`].
  pub fn kept_gid(&self, shown: u32) -> u32 {
    self.shown_gid(shown)
  }

  /// The two owners swapped, the volume's (uid, gid) then the run's, none for the identity: what a stream's tables keep
  /// so that a restored stream shows its guest the owners the saved one did (A-303), as
  /// [`OwnerView::swapping`] makes the view again.
  pub fn swapped(&self) -> Option<((u32, u32), (u32, u32))> {
    self.swap.map(|swap| (swap.kept, swap.run))
  }
}

/// `id` with `a` and `b` exchanged: `b` for `a`, `a` for `b`, any other unchanged.
fn swapped(id: u32, a: u32, b: u32) -> u32 {
  if id == a {
    b
  } else if id == b {
    a
  } else {
    id
  }
}

/// Format: the attribute namespace every route reads and writes: Linux's user namespace (xattr(7)), and what RFC
/// 8276's names are stored under.
pub const USER_NAMESPACE: &[u8] = b"user.";

/// Format: the attribute namespaces a guest's kernel keeps on its files and no host's mount reaches (xattr(7), A-290):
/// its administrator's (`trusted.`, which its kernel lets only a process with `CAP_SYS_ADMIN` read or set) and its
/// security modules' (`security.`: `security.capability` from `setcap`, an SELinux label, an IMA hash).
pub const GUEST_NAMESPACES: [&[u8]; 2] = [b"trusted.", b"security."];

/// Format: the two names under which a Linux kernel keeps a file's POSIX ACLs (acl(5), `include/uapi/linux/xattr.h`
/// `XATTR_NAME_POSIX_ACL_ACCESS` and `_DEFAULT`): the access ACL, and a directory's default ACL its new entries
/// inherit. A guest's `setfacl` sets them; they are the guest's own like [`GUEST_NAMESPACES`]' names (A-302), while
/// every other `system.` name stays refused on every route.
pub const POSIX_ACL_NAMES: [&[u8]; 2] = [POSIX_ACL_ACCESS, POSIX_ACL_DEFAULT];
/// Format: a file's access ACL (`XATTR_NAME_POSIX_ACL_ACCESS`), one of [`POSIX_ACL_NAMES`].
pub const POSIX_ACL_ACCESS: &[u8] = b"system.posix_acl_access";
/// Format: a directory's default ACL (`XATTR_NAME_POSIX_ACL_DEFAULT`), which its new entries inherit; Linux refuses
/// one on anything but a directory (`EACCES`). One of [`POSIX_ACL_NAMES`].
pub const POSIX_ACL_DEFAULT: &[u8] = b"system.posix_acl_default";

/// Which extended-attribute namespaces a transport's caller reaches (§4.5 "Extended attributes", §4.6; A-290, A-302).
///
/// Every route reads and writes `user.` names. A guest kernel's route (a Shards stream, slates' own virtio-fs device)
/// also reaches the guest's own namespaces, [`GUEST_NAMESPACES`], and its POSIX ACLs, [`POSIX_ACL_NAMES`]: what a
/// microVM keeps there (`security.capability` on a binary it gave a capability, `trusted.overlay.*` on an overlayfs
/// upper directory it made on its share, an ACL its `setfacl` set) is the guest's data, kept in the volume like any
/// attribute. A host's mount (a FUSE or NFS mount, the AppleDouble view of a macOS NFSv3 mount) neither shows nor sets
/// them: there the host's own kernel would act on them, a capability on a binary the host runs, an overlay's whiteout
/// on a directory the host reads, an ACL naming the guest's users.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AttributeNamespaces {
  /// A host's mount: `user.` alone.
  #[default]
  Host,
  /// A guest kernel's route: `user.`, the guest's own namespaces and its POSIX ACLs.
  Guest,
}

impl AttributeNamespaces {
  /// Whether a caller of this kind reaches attribute `name`: a name in a namespace it reads and writes, with
  /// something after the namespace's prefix, or, on a guest's route, one of [`POSIX_ACL_NAMES`].
  pub fn reaches(self, name: &[u8]) -> bool {
    let named_in = |prefix: &[u8]| name.len() > prefix.len() && name.starts_with(prefix);
    named_in(USER_NAMESPACE)
      || (self == AttributeNamespaces::Guest
        && (GUEST_NAMESPACES.iter().any(|prefix| named_in(prefix)) || is_posix_acl(name)))
  }
}

/// Whether attribute `name` is a guest's own (A-290, A-302): in one of [`GUEST_NAMESPACES`] or one of
/// [`POSIX_ACL_NAMES`], which no host's mount shows or sets, whatever that mount's own rule for names (the AppleDouble
/// view's are any of macOS's).
pub fn guest_only(name: &[u8]) -> bool {
  GUEST_NAMESPACES
    .iter()
    .any(|prefix| name.starts_with(prefix))
    || is_posix_acl(name)
}

/// Whether attribute `name` is one of [`POSIX_ACL_NAMES`] (A-302).
pub fn is_posix_acl(name: &[u8]) -> bool {
  POSIX_ACL_NAMES.contains(&name)
}

/// The store's parts a backed DAX session works with, lent by a bridge that has a store ([`Bridge::dax_state`]).
pub struct DaxState<'a> {
  /// The store's DAX backings, which the volume's write boundary writes every change of a backed file through.
  pub backings: &'a mut slates_vfs::shards_backing::ShardsBackings,
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

/// The path `path` (`\`-separated from the volume's root, as a WinFsp drive names a file) with each component named as
/// the volume stores it (§4.4's name policy; A-543): every component looked up from the root and named as its directory
/// stores it ([`Bridge::stored_entry`]). On a folding volume every spelling of one file answers one path, the spellings it
/// was made with; on an exact volume, the path as asked. Refused as the first component that cannot be looked up is.
pub fn stored_path(
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  path: &str,
) -> Result<String, VfsError> {
  let mut parent = ObjectId::new(bridge.root(cx)?, 0);
  let mut stored = String::with_capacity(path.len());
  for component in path.split('\\').filter(|component| !component.is_empty()) {
    let (name, attr) = bridge.stored_entry(parent, cx, component)?;
    stored.push('\\');
    stored.push_str(&name);
    parent = ObjectId::new(attr.ino, 0);
  }
  if stored.is_empty() {
    stored.push('\\');
  }
  Ok(stored)
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
  /// ([`slates_vfs::volume::Volume::awaits_publication`]), or a write this bridge took was not kept by the write log
  /// its transport keeps writes in (A-410, [`VolumeBridge::logging_writes`]): a transport's reply then waits for the
  /// owner's barrier (D-18).
  fn awaits_publication(&self) -> bool;
  /// Whether `cx` may change this volume at all (`ReadOnly` for an attachment that may read but not write or a
  /// snapshot's view, `NotPermitted` for one with no rights or of another volume), asked before a transport grants
  /// something that becomes a change later: WinFsp's `CanDelete`, whose delete happens at the cleanup that has no reply.
  fn authorize_change(&self, cx: &OpContext) -> Result<(), VfsError>;
  /// Look `name` up in directory `parent` under `cx`; the child's attributes.
  fn lookup(&mut self, parent: ObjectId, cx: &OpContext, name: &str) -> Result<NodeAttr, VfsError>;
  /// Look `name` up in directory `parent` under `cx` as [`Bridge::lookup`] does, answering besides the child's
  /// attributes its name as the volume stores it (§4.4's name policy; A-543): the asked spelling on an exact volume, and
  /// on a folding one the spelling the entry was made with, which every spelling that folds alike finds. A WinFsp drive
  /// names each open by the stored spelling of every component ([`stored_path`]), so two spellings of one file are one
  /// file to Windows. The default answers the name as asked, as an exact volume stores it.
  fn stored_entry(
    &mut self,
    parent: ObjectId,
    cx: &OpContext,
    name: &str,
  ) -> Result<(String, NodeAttr), VfsError> {
    self
      .lookup(parent, cx, name)
      .map(|attr| (name.to_owned(), attr))
  }
  /// The attributes of `object` under `cx`.
  fn getattr(&mut self, object: ObjectId, cx: &OpContext) -> Result<NodeAttr, VfsError>;
  /// Open `object` under `cx`; the file handle. `flags` are the open's flags as the FUSE protocol carries them (Linux
  /// `O_*`), or `O_RDONLY` (0) from a transport without them; an open for writing (a write access mode, or `O_TRUNC`)
  /// is a change, refused `ReadOnly` before any handle on an attachment that may not write.
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
  /// Creates a name with no content under `cx`, with no open reference or kernel endpoint: a FIFO or socket name
  /// (A-26), or a device node whose number is metadata that nothing slates serves opens (A-294). A device node other
  /// than a whiteout (a character device 0/0) is made only for a caller acting as root, as a local file system makes
  /// one only for a caller holding `CAP_MKNOD`; any other caller is refused `NotPermitted` (`EPERM`).
  fn mknod(
    &mut self,
    parent: ObjectId,
    cx: &OpContext,
    name: &str,
    mode: u32,
    node: SpecialNode,
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

  /// How this bridge's transport shows its guest the owners of the volume's files ([`OwnerView`]): the identity by
  /// default, a swap only on a Shards stream whose guest named its run's owner (A-281).
  fn owner_view(&mut self) -> OwnerView {
    OwnerView::IDENTITY
  }

  /// Which extended-attribute namespaces this bridge's transport reaches ([`AttributeNamespaces`]): a host's mount's
  /// by default, a guest's only on a guest kernel's route (A-290).
  fn attribute_namespaces(&mut self) -> AttributeNamespaces {
    AttributeNamespaces::Host
  }

  /// Whether this bridge's transport's kernel sends FUSE's extended `fuse_setxattr_in`, its setter's flags beside the
  /// size and the `setxattr(2)` flags: a guest kernel's connection that kept `FUSE_SETXATTR_EXT` at its `INIT` (A-378),
  /// as the transport last said ([`Bridge::set_setxattr_ext`]). No by default.
  fn setxattr_ext(&mut self) -> bool {
    false
  }

  /// Tells this bridge whether its transport's kernel sends the extended `fuse_setxattr_in` ([`Bridge::setxattr_ext`]),
  /// before the transport hands it a request (A-378). The transport reads it from the `INIT` reply its connection kept,
  /// so it can change between two requests one bridge serves: a virtio-fs device serves its guest's `INIT` and the
  /// requests after it in one pass over one bridge. A bridge that keeps no form ignores it.
  fn set_setxattr_ext(&mut self, _extended: bool) {}

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
  ///
  /// **Slices** (A-562). The gather runs on the serving thread, so it takes what its turn allows: `more` is asked
  /// before each journal record after the first, and at [`std::ops::ControlFlow::Break`] the gather stops there and
  /// returns the cursor through the last record it took, the rest owed to the next gather, which resumes from it. A
  /// gather that `more` never stops reports everything owed. The changes beneath a base directory are gathered only once
  /// the journal's are whole, in the same gather.
  fn invalidations(
    &mut self,
    _cx: &OpContext,
    cursor: InvalidationCursor,
    _out: &mut Vec<Invalidation>,
    _more: &mut dyn FnMut() -> std::ops::ControlFlow<()>,
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

#[cfg(test)]
mod tests {
  use super::{AttributeNamespaces, OwnerView, guest_only};

  /// A-281. Do: swap the host's owner (501:20) with a run's (1000:1001), and read every id through the view both ways.
  /// Expect each of the two shown as the other, uid and gid apart, every other id (root's among them) as it is, the way
  /// back the same exchange so a chown and a stat agree, and the identity changing nothing.
  #[test]
  fn an_owner_view_swaps_the_hosts_owner_with_the_runs_and_leaves_every_other_id() {
    let view = OwnerView::swapping((501, 20), (1000, 1001));
    assert_eq!((view.shown_uid(501), view.shown_gid(20)), (1000, 1001));
    assert_eq!((view.shown_uid(1000), view.shown_gid(1001)), (501, 20));
    assert_eq!((view.shown_uid(0), view.shown_gid(0)), (0, 0));
    assert_eq!(
      (view.shown_uid(20), view.shown_gid(501)),
      (20, 501),
      "uid and gid swap apart"
    );
    expect_each_id_back_the_way_it_came(view);
    let same = OwnerView::swapping((1000, 1000), (1000, 1000));
    assert_eq!(
      (same.shown_uid(1000), same.kept_gid(1000)),
      (1000, 1000),
      "a run of the host's own owner changes nothing"
    );
    // A-303: the pair a stream's tables keep makes the same view again, and the identity keeps none.
    let (kept, run) = view.swapped().unwrap();
    assert_eq!(OwnerView::swapping(kept, run), view);
    assert_eq!(OwnerView::IDENTITY.swapped(), None);
  }

  /// A-290, A-302. Do: ask a host's mount and a guest's route about names in each namespace, the two ACL names, bare
  /// prefixes, names that only begin like one, and macOS names. Expect `user.` names reached by both; `trusted.` and
  /// `security.` names and the two ACL names by the guest's alone and marked a guest's own; a bare prefix reached by
  /// neither; every other `system.` name and every macOS name by neither and no guest's own.
  #[test]
  fn a_guests_namespaces_are_reached_by_its_route_alone() {
    let (host, guest) = (AttributeNamespaces::Host, AttributeNamespaces::Guest);
    for name in [&b"user.x"[..], b"user.trusted.y"] {
      assert!(host.reaches(name) && guest.reaches(name), "{name:?}");
      assert!(!guest_only(name), "{name:?}");
    }
    expect_a_guests_own(&[
      &b"trusted.overlay.opaque"[..],
      b"security.capability",
      b"security.selinux",
    ]);
    for name in [&b"user."[..], b"trusted.", b"security."] {
      assert!(
        !host.reaches(name) && !guest.reaches(name),
        "a bare prefix: {name:?}"
      );
    }
    expect_a_guests_own(&[&b"system.posix_acl_access"[..], b"system.posix_acl_default"]);
    expect_reached_by_neither_and_no_guests_own(&[
      &b"system.posix_acl_accessx"[..],
      b"system.sockprotoname",
      b"system.",
      b"trustedx",
      b"securityx",
      b"com.apple.FinderInfo",
      b"",
    ]);
    assert_eq!(AttributeNamespaces::default(), host);
  }

  /// The round trips of `an_owner_view_swaps_the_hosts_owner_with_the_runs_and_leaves_every_other_id`: expect every id,
  /// the swapped pair's and others, read back through `view` the way it came, and the identity changing none.
  fn expect_each_id_back_the_way_it_came(view: OwnerView) {
    for id in [0, 20, 501, 1000, 1001, 4242, u32::MAX] {
      assert_eq!(view.kept_uid(view.shown_uid(id)), id);
      assert_eq!(view.kept_gid(view.shown_gid(id)), id);
      assert_eq!(OwnerView::IDENTITY.shown_uid(id), id);
      assert_eq!(OwnerView::IDENTITY.kept_gid(id), id);
    }
  }

  /// A guest's own names in `a_guests_namespaces_are_reached_by_its_route_alone`: expect each of `names` reached by the
  /// guest's route alone, never by a host's mount, and marked a guest's own.
  fn expect_a_guests_own(names: &[&[u8]]) {
    let (host, guest) = (AttributeNamespaces::Host, AttributeNamespaces::Guest);
    for &name in names {
      assert!(!host.reaches(name), "{name:?}");
      assert!(guest.reaches(name), "{name:?}");
      assert!(guest_only(name), "{name:?}");
    }
  }

  /// The names no route reaches in `a_guests_namespaces_are_reached_by_its_route_alone`: expect each of `names` reached
  /// by neither a host's mount nor a guest's route, and no guest's own.
  fn expect_reached_by_neither_and_no_guests_own(names: &[&[u8]]) {
    let (host, guest) = (AttributeNamespaces::Host, AttributeNamespaces::Guest);
    for &name in names {
      assert!(!host.reaches(name) && !guest.reaches(name), "{name:?}");
      assert!(!guest_only(name), "{name:?}");
    }
  }
}

#[cfg(test)]
mod stored_path_tests {
  use super::*;

  /// Shape: the store's page in the test.
  const PAGE: usize = 4096;
  /// Shape: the pages the store's region maps: room for the test's few entries.
  const REGION_PAGES: usize = 256;
  /// Shape: a journal that holds every record the test makes.
  const JOURNAL: usize = 1 << 16;

  /// A store and a volume in it whose names follow `names`.
  fn store_and_volume(
    names: slates_vfs::names::NameEquivalence,
  ) -> (slates_vfs::volume::Store, slates_vfs::volume::Volume) {
    let mut arena = slates_mem::arena::ChunkArena::new(PAGE);
    arena
      .add_region(slates_mem::region::Region::map(PAGE * REGION_PAGES, PAGE, false).unwrap())
      .unwrap();
    let mut store = slates_vfs::volume::Store::new(
      &slates_vfs::volume::StoreConfig {
        page: PAGE,
        cache_line: 128,
        max_dirs: 16,
        max_inodes: 64,
        max_chunks: REGION_PAGES,
        max_dir_blocks: 16,
        dir_cutover: 16,
      },
      arena,
      0,
    );
    let volume = slates_vfs::volume::Volume::create(
      &mut store,
      slates_vfs::volume::VolumeConfig {
        prefix: 1,
        names,
        quota: slates_vfs::quota::Quota::Bounded { limit: 1 << 30 },
        journal_bytes: JOURNAL,
        clock: Box::new(slates_vfs::clock::HostClock::default()),
      },
    )
    .unwrap();
    (store, volume)
  }

  /// A read-write context of an attachment to the volume of id zero.
  fn context() -> OpContext {
    let mut attachments = Attachments::new();
    let attachment = attachments
      .attach(
        slates_db::catalog::VolumeId { bytes: [0; 16] },
        View::Current,
        slates_db::catalog::Principal::Uid {
          host: slates_db::HostId(0),
          uid: 0,
        },
        Rights {
          read: true,
          write: true,
        },
      )
      .unwrap();
    attachments.context(attachment).unwrap()
  }

  /// The volume `names` makes, with `\Src\Main.RS` made in it through a bridge. A step that fails stops the test here,
  /// so a refused setup can never pass for the `NotFound` a test expects from the path it names.
  fn with_main_rs(
    names: slates_vfs::names::NameEquivalence,
  ) -> (slates_vfs::volume::Store, slates_vfs::volume::Volume) {
    let (mut store, mut volume) = store_and_volume(names);
    {
      let cx = context();
      let mut bridge = VolumeBridge::new(
        slates_db::catalog::VolumeId { bytes: [0; 16] },
        &mut volume,
        &mut store,
      );
      let root = bridge.root(&cx).unwrap();
      let dir = bridge
        .mkdir(ObjectId::new(root, 0), &cx, "Src", 0o755)
        .unwrap();
      bridge
        .create(ObjectId::new(dir.ino, 0), &cx, "Main.RS", 0o644, 0)
        .unwrap();
    }
    (store, volume)
  }

  /// `path` named as the volume `names` makes, with `\Src\Main.RS` in it, stores each component: the answer under test
  /// alone, through a bridge made afresh as the daemon's serve paths make one per request.
  fn stored_in(names: slates_vfs::names::NameEquivalence, path: &str) -> Result<String, VfsError> {
    let (mut store, mut volume) = with_main_rs(names);
    let cx = context();
    let mut bridge = VolumeBridge::new(
      slates_db::catalog::VolumeId { bytes: [0; 16] },
      &mut volume,
      &mut store,
    );
    stored_path(&mut bridge, &cx, path)
  }

  /// §4.4 name policy, T-4.6, A-543 (B2): a WinFsp drive names each open by the path its volume stores, so on a folding
  /// volume every spelling of a file is one file to Windows. Do: on a folding volume and on an exact one, make `\Src` and
  /// `\Src\Main.RS`, then name paths as the volume stores them: the made spelling, two others, the root, and a missing
  /// file. Expect the folding volume to answer `\Src\Main.RS` for every spelling, the exact one only for the spelling it
  /// was made with and `NotFound` for the others, the root as itself, and a missing file `NotFound` on both. Before,
  /// nothing named an open's path as stored, so a folding volume's two spellings were two files to WinFsp's driver.
  #[test]
  fn every_spelling_of_a_file_on_a_folding_volume_names_the_path_it_was_made_with() {
    use slates_vfs::names::NameEquivalence::{Exact, Fold};
    let made = Ok("\\Src\\Main.RS".to_owned());
    assert_eq!(stored_in(Fold, "\\Src\\Main.RS"), made);
    assert_eq!(
      stored_in(Fold, "\\src\\MAIN.rs"),
      made,
      "another spelling folds to it"
    );
    assert_eq!(stored_in(Fold, "\\SRC\\main.rs"), made, "and a third");
    assert_eq!(
      stored_in(Fold, "\\"),
      Ok("\\".to_owned()),
      "the root as itself"
    );
    assert_eq!(stored_in(Fold, "\\src\\gone.rs"), Err(VfsError::NotFound));
    assert_eq!(
      stored_in(Exact, "\\Src\\Main.RS"),
      made,
      "an exact volume's own spelling"
    );
    assert_eq!(
      stored_in(Exact, "\\src\\MAIN.rs"),
      Err(VfsError::NotFound),
      "an exact volume finds no other spelling"
    );
  }
}
