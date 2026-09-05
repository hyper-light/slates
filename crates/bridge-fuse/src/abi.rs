//! The FUSE ABI constants and opcodes slates speaks (`include/uapi/linux/fuse.h`). The version
//! floor is 7.31; the kernel's is negotiated at `FUSE_INIT`. Only the opcodes the bridge
//! serves (§4.6's Bridge trait) are named; an opcode outside this set is a typed refusal, so a
//! kernel that sends one gets `ENOSYS` rather than a panic.

/// Format: the FUSE kernel major version slates speaks.
pub const FUSE_KERNEL_VERSION: u32 = 7;
/// Format: the FUSE kernel minor version slates offers as its floor (7.31: parallel dirops,
/// `EXPLICIT_INVAL_DATA` at 7.30, writeback cache, readdirplus; later flags negotiate up).
pub const FUSE_KERNEL_MINOR_VERSION: u32 = 31;

/// Format: the fixed FUSE request header (`struct fuse_in_header`): len (4), opcode (4),
/// unique (8), nodeid (8), uid (4), gid (4), pid (4), total_extlen (2), padding (2). 40 bytes.
pub const IN_HEADER_LEN: usize = 40;
/// Format: the fixed FUSE reply header (`struct fuse_out_header`): len (4), error (4),
/// unique (8). 16 bytes.
pub const OUT_HEADER_LEN: usize = 16;

/// The FUSE opcodes slates serves (a subset of the ABI; each maps to a Bridge method, §4.6).
/// The discriminant is the wire value from `include/uapi/linux/fuse.h`; an opcode outside this
/// set is a typed miss, so a kernel that sends one gets `ENOSYS` rather than a panic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
#[non_exhaustive]
pub enum Opcode {
  /// Format: FUSE_LOOKUP — look a name up in a directory.
  Lookup = 1,
  /// Format: FUSE_FORGET — the kernel drops references to an inode.
  Forget = 2,
  /// Format: FUSE_GETATTR — read an inode's attributes.
  GetAttr = 3,
  /// Format: FUSE_SETATTR — change an inode's attributes.
  SetAttr = 4,
  /// Format: FUSE_READLINK — read a symlink's target.
  ReadLink = 5,
  /// Format: FUSE_SYMLINK — create a symlink.
  SymLink = 6,
  /// Format: FUSE_MKNOD — create a device node or regular file.
  MkNod = 8,
  /// Format: FUSE_MKDIR — create a directory.
  MkDir = 9,
  /// Format: FUSE_UNLINK — remove a name.
  Unlink = 10,
  /// Format: FUSE_RMDIR — remove a directory.
  RmDir = 11,
  /// Format: FUSE_RENAME — rename a name.
  Rename = 12,
  /// Format: FUSE_LINK — create a hard link.
  Link = 13,
  /// Format: FUSE_OPEN — open a file.
  Open = 14,
  /// Format: FUSE_READ — read from an open file.
  Read = 15,
  /// Format: FUSE_WRITE — write to an open file.
  Write = 16,
  /// Format: FUSE_STATFS — report filesystem statistics.
  StatFs = 17,
  /// Format: FUSE_RELEASE — release an open file.
  Release = 18,
  /// Format: FUSE_FSYNC — flush cached data of an open file.
  FSync = 20,
  /// Format: FUSE_FLUSH — the kernel flushes a file descriptor.
  Flush = 25,
  /// Format: FUSE_INIT — negotiate the connection.
  Init = 26,
  /// Format: FUSE_OPENDIR — open a directory.
  OpenDir = 27,
  /// Format: FUSE_READDIR — read directory entries.
  ReadDir = 28,
  /// Format: FUSE_RELEASEDIR — release an open directory.
  ReleaseDir = 29,
  /// Format: FUSE_FSYNCDIR — flush a directory's cached data.
  FSyncDir = 30,
  /// Format: FUSE_CREATE — look up or create a file, returning it open.
  Create = 35,
  /// Format: FUSE_DESTROY — tear the connection down.
  Destroy = 38,
  /// Format: FUSE_READDIRPLUS — read directory entries with attributes.
  ReadDirPlus = 44,
  /// Format: FUSE_RENAME2 — rename with flags (`RENAME_EXCHANGE`, `RENAME_NOREPLACE`).
  Rename2 = 45,
}

/// Every opcode slates serves, so `from_wire` needs no number of its own.
const ALL: &[Opcode] = &[
  Opcode::Lookup,
  Opcode::Forget,
  Opcode::GetAttr,
  Opcode::SetAttr,
  Opcode::ReadLink,
  Opcode::SymLink,
  Opcode::MkNod,
  Opcode::MkDir,
  Opcode::Unlink,
  Opcode::RmDir,
  Opcode::Rename,
  Opcode::Link,
  Opcode::Open,
  Opcode::Read,
  Opcode::Write,
  Opcode::StatFs,
  Opcode::Release,
  Opcode::FSync,
  Opcode::Flush,
  Opcode::Init,
  Opcode::OpenDir,
  Opcode::ReadDir,
  Opcode::ReleaseDir,
  Opcode::FSyncDir,
  Opcode::Create,
  Opcode::Destroy,
  Opcode::ReadDirPlus,
  Opcode::Rename2,
];

impl Opcode {
  /// The opcode for a wire value, or `None` when slates does not serve it (the caller replies
  /// `ENOSYS`).
  pub fn from_wire(value: u32) -> Option<Opcode> {
    ALL.iter().copied().find(|op| op.to_wire() == value)
  }

  /// The wire value of an opcode (its ABI discriminant).
  pub fn to_wire(self) -> u32 {
    self as u32
  }
}

/// The `FUSE_INIT` flags slates negotiates (a subset; the connection keeps the intersection of
/// what it asks and what the kernel offers, §4.6 "Cache posture").
pub mod flags {
  /// Format: FUSE_WRITEBACK_CACHE — the kernel holds dirty pages and writes them back in bulk. The
  /// Linux ABI bit is `1 << 16` (`<linux/fuse.h>`); `1 << 8` is `FUSE_SPLICE_MOVE`, so the old value
  /// advertised the wrong capability and never negotiated writeback (source audit BUG-6, 2026-09-05).
  pub const WRITEBACK_CACHE: u64 = 1 << 16;
  /// Format: FUSE_PARALLEL_DIROPS — several directory operations may be in flight at once.
  pub const PARALLEL_DIROPS: u64 = 1 << 18;
  /// Format: FUSE_DO_READDIRPLUS — readdirplus is available (entries carry attributes).
  pub const DO_READDIRPLUS: u64 = 1 << 13;
  /// Format: FUSE_READDIRPLUS_AUTO — the kernel adaptively chooses readdir vs readdirplus.
  pub const READDIRPLUS_AUTO: u64 = 1 << 14;
  /// Format: FUSE_EXPLICIT_INVAL_DATA — invalidation may name a data range, not the whole inode.
  pub const EXPLICIT_INVAL_DATA: u64 = 1 << 25;
  /// Format: FUSE_BIG_WRITES — the kernel accepts writes larger than one page per request.
  pub const BIG_WRITES: u64 = 1 << 5;
  /// Format: FUSE_INIT_EXT — the second 32 bits of the flags word (`flags2`) are present.
  pub const INIT_EXT: u64 = 1 << 30;
  /// Format: FUSE_DONT_MASK — the kernel applies the umask itself, so the mode arrives unmasked.
  pub const DONT_MASK: u64 = 1 << 6;
}
