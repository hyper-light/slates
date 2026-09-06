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

pub mod authority;
pub mod volume_bridge;

pub use authority::{AttachmentId, Attachments, ObjectId, OpContext, Rights, View};
pub use volume_bridge::VolumeBridge;

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
}

/// One entry of a directory listing: the child's inode number, its kind and its name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
  /// The child inode number.
  pub ino: u64,
  /// The child kind.
  pub kind: Kind,
  /// The name.
  pub name: String,
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
}

/// The one VFS operation layer (§4.6). A transport calls these by real inode number; a refusal is
/// the volume core's typed [`VfsError`], which the transport maps to its own wire error. Only the
/// operations the transports dispatch are here; the set grows with the drivers.
pub trait Bridge {
  /// The root directory's inode number. FUSE resolves node id 1 to it; NFS mints the export's
  /// root file handle from it.
  fn root(&mut self) -> Result<u64, VfsError>;
  /// Look `name` up in directory `parent`; the child's attributes.
  fn lookup(&mut self, parent: u64, name: &str) -> Result<NodeAttr, VfsError>;
  /// The attributes of `ino`.
  fn getattr(&mut self, ino: u64) -> Result<NodeAttr, VfsError>;
  /// Open `ino`; the file handle.
  fn open(&mut self, ino: u64, flags: u32) -> Result<u64, VfsError>;
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
  /// Write `data` at `offset` to `object` under the authenticated `cx`; the bytes written. A write
  /// against a read-only attachment or a pinned view is refused before any effect.
  fn write(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    offset: u64,
    data: &[u8],
  ) -> Result<u32, VfsError>;
  /// Open directory `ino`; the handle.
  fn opendir(&mut self, ino: u64) -> Result<u64, VfsError>;
  /// The entries of directory `ino` from `offset` (each entry's position is the resume cookie).
  fn readdir(&mut self, ino: u64, fh: u64, offset: u64) -> Result<Vec<DirEntry>, VfsError>;
  /// Create `name` in `parent` and open it; the attributes and the handle.
  fn create(
    &mut self,
    parent: u64,
    name: &str,
    mode: u32,
    flags: u32,
  ) -> Result<(NodeAttr, u64), VfsError>;
  /// Release handle `fh` of `ino`.
  fn release(&mut self, ino: u64, fh: u64) -> Result<(), VfsError>;
  /// The transport drops `nlookup` references to `ino`.
  fn forget(&mut self, ino: u64, nlookup: u64);
  /// Flush handle `fh` of `ino` (no disk write; success once the data is in the anchor).
  fn flush(&mut self, ino: u64, fh: u64) -> Result<(), VfsError>;
  /// Create directory `name` in `parent`; the attributes.
  fn mkdir(&mut self, parent: u64, name: &str, mode: u32) -> Result<NodeAttr, VfsError>;
  /// Remove `name` from `parent`.
  fn unlink(&mut self, parent: u64, name: &str) -> Result<(), VfsError>;
  /// Remove directory `name` from `parent`.
  fn rmdir(&mut self, parent: u64, name: &str) -> Result<(), VfsError>;
  /// Create a symlink `name` in `parent` pointing at `target`; the attributes.
  fn symlink(&mut self, parent: u64, name: &str, target: &str) -> Result<NodeAttr, VfsError>;
  /// The target of symlink `ino`.
  fn readlink(&mut self, ino: u64) -> Result<String, VfsError>;
  /// Rename `old_name` under `old_parent` to `new_name` under `new_parent`, honoring or refusing
  /// the `renameat2` `flags` (never silently dropping them).
  fn rename(
    &mut self,
    old_parent: u64,
    old_name: &str,
    new_parent: u64,
    new_name: &str,
    flags: RenameFlags,
  ) -> Result<(), VfsError>;
  /// Apply `changes` to `ino`; the new attributes.
  fn setattr(&mut self, ino: u64, changes: SetAttr) -> Result<NodeAttr, VfsError>;
  /// Filesystem statistics for the volume `ino` lives in.
  fn statfs(&mut self, ino: u64) -> Result<FsStat, VfsError>;
}
