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
  /// Format: FUSE_BATCH_FORGET — the kernel drops references to several inodes at once (the batched
  /// FORGET; on virtio-fs a high-priority-queue request, virtio 1.2 §5.11.6.2).
  BatchForget = 42,
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
  Opcode::BatchForget,
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
  /// Format: FUSE_WRITEBACK_CACHE — the kernel holds dirty pages and writes them back in bulk, and
  /// owns a regular file's size and times while it does. The Linux ABI bit is `1 << 16`
  /// (`<linux/fuse.h>`); `1 << 8` is `FUSE_SPLICE_MOVE`, so the old value advertised the wrong
  /// capability and never negotiated writeback (source audit BUG-6, 2026-09-05). Since 2026-09-19 it
  /// is deliberately **not requested** (`init::wanted`): the kernel's ownership of the size defeats
  /// the invalidation of a change made through another attachment.
  pub const WRITEBACK_CACHE: u64 = 1 << 16;
  /// Format: FUSE_PARALLEL_DIROPS — several directory operations may be in flight at once.
  pub const PARALLEL_DIROPS: u64 = 1 << 18;
  /// Format: FUSE_DO_READDIRPLUS — readdirplus is available (entries carry attributes).
  pub const DO_READDIRPLUS: u64 = 1 << 13;
  /// Format: FUSE_READDIRPLUS_AUTO — the kernel adaptively chooses readdir vs readdirplus.
  pub const READDIRPLUS_AUTO: u64 = 1 << 14;
  /// Format: FUSE_AUTO_INVAL_DATA — the kernel drops an inode's cached pages when a revalidation shows its
  /// size or mtime changed (what a server that sends no notifications asks for; libfuse's default).
  pub const AUTO_INVAL_DATA: u64 = 1 << 12;
  /// Format: FUSE_EXPLICIT_INVAL_DATA — invalidation may name a data range, not the whole inode.
  pub const EXPLICIT_INVAL_DATA: u64 = 1 << 25;
  /// Format: FUSE_BIG_WRITES — the kernel accepts writes larger than one page per request.
  pub const BIG_WRITES: u64 = 1 << 5;
  /// Format: FUSE_INIT_EXT — the second 32 bits of the flags word (`flags2`) are present.
  pub const INIT_EXT: u64 = 1 << 30;
  /// Format: FUSE_DONT_MASK — the kernel does **not** apply the creating process's umask: the mode arrives
  /// unmasked and the umask travels beside it in the request (`fuse_create_in`, `fuse_mkdir_in`,
  /// `fuse_mknod_in`), for the filesystem to apply (Linux `fs/fuse/dir.c`). Corrected 2026-10-01
  /// (AUD-29-80): this said the opposite, and the handlers discarded the umask.
  pub const DONT_MASK: u64 = 1 << 6;
  /// Format: FUSE_HAS_EXPIRE_ONLY — the kernel honours `FUSE_EXPIRE_ONLY` on an entry
  /// invalidation (revalidate the name on its next use rather than drop it now; 6.2+). In the
  /// second flags word (`flags2`), hence the bit above 31.
  pub const HAS_EXPIRE_ONLY: u64 = 1 << 35;
  /// Format: FUSE_HAS_RESEND — the kernel can resend the requests a daemon read but never answered
  /// (`FUSE_NOTIFY_RESEND`), marking each resent one's unique id with [`super::UNIQUE_RESEND`]; 7.40, Linux 6.9+.
  /// The kernel only advertises it: a daemon learns it from the kernel's `INIT` and never echoes it. In
  /// `flags2`, hence the bit above 31.
  pub const HAS_RESEND: u64 = 1 << 39;
}

/// The `fuse_open_out.open_flags` slates sets on an `OPEN`, `OPENDIR` or `CREATE` reply (`<linux/fuse.h>`; a kernel
/// that does not know one ignores it).
pub mod open {
  /// Format: FOPEN_KEEP_CACHE — the kernel keeps the file's cached pages across opens instead of dropping them at
  /// each one. Set for the volume's own objects, whose every change through another attachment is invalidated
  /// explicitly (`Invalidation::Inode { data: true }`); never for a live base object an outsider may change.
  pub const KEEP_CACHE: u32 = 1 << 1;
  /// Format: FOPEN_CACHE_DIR — the kernel caches the directory's listing (7.28, Linux 4.20+); a change of a name in
  /// it invalidates the directory's data as well as the name (`entry_invalidation`).
  pub const CACHE_DIR: u32 = 1 << 3;
  /// Format: FOPEN_NOFLUSH — the kernel sends no `FLUSH` when the handle's file descriptor closes (7.35, Linux
  /// 5.18+). Set for a read-only handle, which has nothing to make durable at close: its `FLUSH` was a round trip
  /// and a barrier for nothing. slates serves no POSIX lock operations, so no lock is released through it.
  pub const NOFLUSH: u32 = 1 << 5;
}

/// Format: the access-mode bits of an open's `flags` (`O_ACCMODE`), and the read-only mode (`O_RDONLY`).
pub const ACCESS_MODE: u32 = 0o3;
/// Format: `O_RDONLY`.
pub const READ_ONLY: u32 = 0;

/// Format: FUSE_UNIQUE_RESEND — the top bit of a request's unique id marks a request the kernel resent after a
/// `FUSE_NOTIFY_RESEND` (`include/uapi/linux/fuse.h`). A reply carries the unique exactly as the request did,
/// bit included, since the kernel matches the whole word.
pub const UNIQUE_RESEND: u64 = 1 << 63;
