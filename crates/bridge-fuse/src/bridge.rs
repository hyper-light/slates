//! The FUSE wire edge over the shared operation layer (§4.6). The kernel's requests, once parsed
//! by the codec, are turned into calls on the transport-independent [`Bridge`] trait, which lives
//! in `slates-bridge-core` and has one implementation over the volume core (and a mock in the
//! tests here). This module is the seam between the FUSE wire and those neutral semantics: it
//! parses a request, resolves the kernel's node id to a real inode number (node id 1 is the
//! root, resolved through [`Bridge::root`]), calls the matching method, maps the volume core's
//! typed [`VfsError`] to the Linux errno the kernel expects, and encodes the neutral result
//! ([`NodeAttr`], [`DirEntry`], [`FsStat`]) into the FUSE reply. So the transport (the
//! `/dev/fuse` read/write loop, Linux only) is a thin loop over [`dispatch`], and the semantics
//! are tested on every host without a mount. An opcode slates does not serve is answered
//! `ENOSYS` without reaching the bridge.

use crate::abi::{OUT_HEADER_LEN, Opcode};
use crate::init::negotiate;
use crate::reply::{Attr, AttrOut, DirBuffer, EntryOut, OpenOut, ReplyHeader, StatfsOut, WriteOut};
use crate::request::{ReadIn, RenameIn, Request, SetAttrIn, WriteIn, parse_name};

pub use slates_bridge_core::{Bridge, DirEntry};
use slates_bridge_core::{
  CacheCoherence, CacheLifetime, FsStat, NodeAttr, ObjectId, OpContext, RenameFlags, SetAttr,
  whole_cookie_groups,
};
use slates_vfs::error::VfsError;
use slates_vfs::inode::Kind;

/// Format: `ENOSYS`, the errno for an opcode the bridge does not implement.
pub const ENOSYS: i32 = 38;
/// Format: `EIO`, the errno for a request the codec could not parse.
pub const EIO: i32 = 5;
/// Format: `EILSEQ` (Linux, asm-generic `errno.h`), the errno for a name that is not UTF-8: the request was well
/// formed and the name is one a volume cannot hold (D-4), as APFS answers the same name.
pub const EILSEQ: i32 = 84;
/// Format: how long the kernel may cache an entry or attributes of the volume's own objects:
/// forever, since slates invalidates explicitly on every mutation (§4.6 "Cache posture"). A live
/// base entry gets the seam's bounded lifetime instead ([`Bridge::cache_lifetime`]).
pub const CACHE_FOREVER: u64 = u64::MAX;
/// Format: the FUSE node id of the root directory; the kernel always names the root by it, and
/// the edge resolves it to the volume's real root inode number.
pub(crate) const FUSE_ROOT_ID: u64 = 1;
/// Format: the Unix `d_type` values a `readdir` entry carries.
const DT_DIR: u32 = 4;
const DT_REG: u32 = 8;
const DT_LNK: u32 = 10;
/// Format: POSIX directory entry type for a FIFO.
const DT_FIFO: u32 = 1;
/// Format: POSIX directory entry type for a socket.
const DT_SOCK: u32 = 12;
/// Format: Linux EOPNOTSUPP, a refused special-file operation.
pub(crate) const EOPNOTSUPP: i32 = 95;
/// Format: Linux ENODATA, the attribute a get or remove names is not set (`getxattr(2)`'s ENOATTR).
const ENODATA: i32 = 61;
/// Format: Linux ERANGE, the caller's buffer is too small for the attribute value or name list.
const ERANGE: i32 = 34;
/// Format: Linux EAGAIN, an operation to try again (A-64: the arena waits on the shard's next publication).
const EAGAIN: i32 = 11;
// The Linux errno values the volume core's refusals map to (the FUSE ABI is Linux, so the
// numbers are the kernel's regardless of the host the codec is tested on; the dispatch negates
// them). Each is a Format constant.
/// Format: ENOENT, no such file or directory.
const ENOENT: i32 = 2;
/// Format: EPERM, operation not permitted.
const EPERM: i32 = 1;
/// Format: Linux EACCES, a symlink out of the volume refused to a caller who does not own it (A-107).
const EACCES: i32 = 13;
/// Format: EEXIST, the name already exists.
const EEXIST: i32 = 17;
/// Format: ENOTDIR, not a directory.
const ENOTDIR: i32 = 20;
/// Format: EISDIR, is a directory.
const EISDIR: i32 = 21;
/// Format: EINVAL, invalid argument.
const EINVAL: i32 = 22;
/// Format: EFBIG, file too large.
const EFBIG: i32 = 27;
/// Format: ENOSPC, no space left.
const ENOSPC: i32 = 28;
/// Format: EMLINK, too many links.
const EMLINK: i32 = 31;
/// Format: ENOTEMPTY, directory not empty.
const ENOTEMPTY: i32 = 39;
/// Format: ENAMETOOLONG, a component longer than `NAME_MAX` (Linux's value; macOS's is 63).
const ENAMETOOLONG: i32 = 36;
/// Format: EMFILE, too many open files (the bridge's handle table is full).
const EMFILE: i32 = 24;
/// Format: the block unit `fuse_attr.blocks` counts in (512-byte blocks, the stat convention).
const BYTES_PER_BLOCK: u64 = 512;
/// Shape: the block size reported to the kernel: one page, the volume core's chunk unit.
const BLKSIZE: u32 = 4096;

/// Dispatches one parsed message to `bridge`, writing the reply into `out`; returns the bytes
/// written. A parse failure replies `EIO`; an unserved opcode replies `ENOSYS`; a refusal
/// replies its mapped errno. `INIT` is answered here (it negotiates, it is not a Bridge method).
/// The transport calls this for every message and writes `out[..n]` back to the kernel.
pub fn dispatch(message: &[u8], bridge: &mut dyn Bridge, cx: &OpContext, out: &mut [u8]) -> usize {
  let request = match Request::parse(message) {
    Ok(r) => r,
    // A message the codec cannot parse: reply EIO with the unique the header would carry when
    // it is at least readable, else drop (the caller sends nothing for a zero return).
    Err(_) => {
      return recover_unique(message)
        .map(|u| write_or_drop(ReplyHeader::write_error(u, EIO, out), out))
        .unwrap_or(0);
    }
  };
  let unique = request.header.unique;
  let Some(opcode) = request.opcode else {
    return write_or_drop(ReplyHeader::write_error(unique, ENOSYS, out), out);
  };
  // A request whose success reply cannot fit the room it was given is refused before any effect (AUD-29-85):
  // a create, a lookup or an open would otherwise take its handle or reference and then lose the reply that
  // names it, and a write would change bytes the caller is then told failed.
  if success_reply_bytes(opcode).is_some_and(|need| out.len() < need) {
    return write_or_drop(ReplyHeader::write_error(unique, EIO, out), out);
  }
  // A creating request stamps the creating process's uid and gid on what it makes (AUD-29-81): ownership
  // metadata from the kernel's request header, never authority — the attachment's enrolled subject is
  // unchanged, and the shared seam applies the set-group-ID parent rule.
  let creator;
  let cx = if creates(opcode) {
    creator = OpContext {
      owner_uid: Some(request.header.uid),
      owner_gid: Some(request.header.gid),
      ..cx.clone()
    };
    &creator
  } else {
    cx
  };
  match opcode {
    Opcode::Init => serve_init(request.body, unique, cx.coherence, out),
    Opcode::Lookup => serve_lookup(bridge, &request, cx, out),
    Opcode::GetAttr => serve_getattr(bridge, &request, cx, out),
    Opcode::Open => serve_open(bridge, &request, cx, Opcode::Open, out),
    Opcode::OpenDir => serve_open(bridge, &request, cx, Opcode::OpenDir, out),
    Opcode::Read => serve_read(bridge, &request, cx, out),
    Opcode::Write => serve_write(bridge, &request, cx, out),
    Opcode::ReadDir => serve_readdir(bridge, &request, cx, out),
    Opcode::ReadDirPlus => serve_readdirplus(bridge, &request, cx, out),
    Opcode::Create => serve_create(bridge, &request, cx, out),
    Opcode::Release | Opcode::ReleaseDir => serve_release(bridge, &request, cx, out),
    // FSYNC/FSYNCDIR are flush-equivalent for slates: the data is already in the anchor segment,
    // which is the source of truth (R1), so there is nothing to force to a lower tier — a success
    // no-op, the same as FLUSH. They carry `fh` first, exactly as FLUSH does (audit BUG-7).
    Opcode::Flush => serve_flush(bridge, &request, cx, out, false),
    Opcode::FSync | Opcode::FSyncDir => serve_flush(bridge, &request, cx, out, true),
    Opcode::Forget => serve_forget(bridge, &request, cx),
    Opcode::BatchForget => serve_batch_forget(bridge, &request, cx),
    // DESTROY is the kernel's last request at unmount: the attachment's references are swept (the
    // kernel does not guarantee a FORGET per outstanding reference, §4.6 `sweep_attachment`) and the
    // reply is an empty success; it was ENOSYS before, so nothing was ever swept at unmount.
    Opcode::Destroy => serve_destroy(bridge, &request, cx, out),
    Opcode::MkNod => serve_mknod(bridge, &request, cx, out),
    Opcode::MkDir => serve_mkdir(bridge, &request, cx, out),
    Opcode::Unlink => serve_unlink(bridge, &request, cx, false, out),
    Opcode::RmDir => serve_unlink(bridge, &request, cx, true, out),
    Opcode::SymLink => serve_symlink(bridge, &request, cx, out),
    Opcode::Link => serve_link(bridge, &request, cx, out),
    Opcode::ReadLink => serve_readlink(bridge, &request, cx, out),
    Opcode::Rename => serve_rename(bridge, &request, cx, false, out),
    Opcode::Rename2 => serve_rename(bridge, &request, cx, true, out),
    Opcode::SetAttr => serve_setattr(bridge, &request, cx, out),
    Opcode::StatFs => serve_statfs(bridge, &request, cx, out),
    Opcode::SetXattr => serve_setxattr(bridge, &request, cx, out),
    Opcode::GetXattr => serve_getxattr(bridge, &request, cx, out, false),
    Opcode::ListXattr => serve_getxattr(bridge, &request, cx, out, true),
    Opcode::RemoveXattr => serve_removexattr(bridge, &request, cx, out),
    // An allocation is user-scaled work that must yield between slices, which one synchronous call cannot: the
    // stepping channel serves it (`crate::allocate`, A-108), and a transport that only dispatches answers that it is
    // not supported, which glibc's `posix_fallocate` answers by writing zeros itself.
    Opcode::Fallocate => write_or_drop(ReplyHeader::write_error(unique, EOPNOTSUPP, out), out),
  }
}

/// The bytes a success reply of `opcode` needs, header included, for the opcodes whose reply has a fixed size
/// and whose service has an effect a lost reply would orphan (a reference, an open handle, a created object,
/// changed bytes or attributes); `None` for the rest — a reply sized by its request (read, directory pages:
/// clamped to the room instead) or one with no effect to lose. A transport whose reply room is the caller's
/// (a virtio guest's posted buffers) refuses below it before dispatch (AUD-29-85).
pub fn success_reply_bytes(opcode: Opcode) -> Option<usize> {
  let body = match opcode {
    Opcode::Lookup | Opcode::MkDir | Opcode::MkNod | Opcode::SymLink | Opcode::Link => {
      EntryOut::LEN
    }
    Opcode::Create => EntryOut::LEN.saturating_add(OpenOut::LEN),
    Opcode::GetAttr | Opcode::SetAttr => AttrOut::LEN,
    Opcode::Open | Opcode::OpenDir => OpenOut::LEN,
    Opcode::Write => WriteOut::LEN,
    _ => return None,
  };
  Some(OUT_HEADER_LEN.saturating_add(body))
}

/// Whether a request that answered `error` changed the volume in a way its caller is promised survives a
/// daemon restart once the reply arrives (§4.8 barrier, D-18): a namespace or attribute change, an allocation (A-108), and the commit
/// points of data — `fsync` and the `flush` every close sends — succeeded. A plain `write` is not one: like an
/// NFS `UNSTABLE` write, its bytes are in the daemon when it returns and are made stable by the `flush` or
/// `fsync` that follows. Every transport that serves this dispatch (the kernel's `/dev/fuse`, a virtio-fs
/// guest) holds such a reply until its owner's barrier has captured the volume (AUD-29-82).
pub fn needs_barrier(opcode: Option<Opcode>, error: i32) -> bool {
  error == 0
    && matches!(
      opcode,
      Some(
        Opcode::SetAttr
          | Opcode::SymLink
          | Opcode::MkNod
          | Opcode::MkDir
          | Opcode::Unlink
          | Opcode::RmDir
          | Opcode::Rename
          | Opcode::Rename2
          | Opcode::Link
          | Opcode::Create
          | Opcode::FSync
          | Opcode::FSyncDir
          | Opcode::Flush
          | Opcode::Fallocate
      )
    )
}

/// What [`reclaim_unreported`] gave back: the lookup references forgotten and the open handles released.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reclaimed {
  /// Lookup references forgotten (one per entry the lost reply named).
  pub references: u64,
  /// Open handles released (the lost reply's `fh`).
  pub handles: u64,
}

/// Gives back what a success reply granted when that reply never reached its caller (AUD-29-85): the owner
/// replaced it with an error (a refused barrier's `EIO`), the kernel refused it (`ENOENT`: the request was
/// interrupted and is no longer waiting), or the guest's buffers could not take it. The caller cannot send the
/// matching FORGET or RELEASE for a node id or handle it never learned, so the server does — the rule libfuse
/// keeps in `fuse.c` (`reply_entry` forgets and `fuse_reply_open` failures release on `ENOENT`). `opcode` and
/// `nodeid` are the request's; `reply` is the success reply as encoded. An error reply granted nothing.
pub fn reclaim_unreported(
  opcode: Option<Opcode>,
  nodeid: u64,
  reply: &[u8],
  bridge: &mut dyn Bridge,
  cx: &OpContext,
) -> Reclaimed {
  let mut reclaimed = Reclaimed::default();
  let (Some(opcode), Some(body)) = (opcode, success_body(reply)) else {
    return reclaimed;
  };
  let mut forget = |entry: u64, reclaimed: &mut Reclaimed| {
    // A node id of zero is a negative entry: the kernel takes no reference on it.
    if entry != 0 {
      bridge.forget(ObjectId::new(entry, 0), cx, 1);
      reclaimed.references = reclaimed.references.saturating_add(1);
    }
  };
  match opcode {
    Opcode::Lookup | Opcode::MkDir | Opcode::MkNod | Opcode::SymLink | Opcode::Link => {
      if let Some(entry) = u64_at(body, 0) {
        forget(entry, &mut reclaimed);
      }
    }
    Opcode::Create => {
      let entry = u64_at(body, 0);
      if let Some(entry) = entry {
        forget(entry, &mut reclaimed);
      }
      if let (Some(entry), Some(fh)) = (entry, u64_at(body, EntryOut::LEN)) {
        release(bridge, cx, entry, fh, &mut reclaimed);
      }
    }
    Opcode::Open | Opcode::OpenDir => {
      if let Some(fh) = u64_at(body, 0) {
        release(bridge, cx, nodeid, fh, &mut reclaimed);
      }
    }
    Opcode::ReadDirPlus => {
      for (entry, name) in plus_entries(body) {
        // "." and ".." are never referenced by READDIRPLUS (the kernel never forgets them).
        if name != b"." && name != b".." {
          forget(entry, &mut reclaimed);
        }
      }
    }
    _ => {}
  }
  reclaimed
}

/// Releases the handle `fh` on `nodeid`, counting it when the seam accepted the release.
fn release(
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  nodeid: u64,
  fh: u64,
  reclaimed: &mut Reclaimed,
) {
  if bridge.release(ObjectId::new(nodeid, 0), cx, fh).is_ok() {
    reclaimed.handles = reclaimed.handles.saturating_add(1);
  }
}

/// The body of a success reply: `None` for an error reply or one too short for its header.
fn success_body(reply: &[u8]) -> Option<&[u8]> {
  /// Format: `fuse_out_header`: `len` (u32) then `error` (i32) — the errno field's offset.
  const ERROR_AT: usize = size_of::<u32>();
  let error = reply.get(ERROR_AT..ERROR_AT.saturating_add(size_of::<i32>()))?;
  if i32::from_le_bytes(error.try_into().ok()?) != 0 {
    return None;
  }
  reply.get(OUT_HEADER_LEN..)
}

/// The little-endian `u64` at `at` in `bytes`, when it is all there.
fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
  let field = bytes.get(at..at.checked_add(size_of::<u64>())?)?;
  Some(u64::from_le_bytes(field.try_into().ok()?))
}

/// The (node id, name) of each entry in a READDIRPLUS page: `fuse_entry_out` (node id first), then a
/// `fuse_dirent` (ino, off, namelen, type, name) padded to its alignment. A truncated entry ends the walk.
fn plus_entries(page: &[u8]) -> impl Iterator<Item = (u64, &[u8])> {
  /// Format: `fuse_dirent`'s fixed part (ino, off, namelen, type) and the namelen field's offset in it.
  const DIRENT_HEAD: usize = 24;
  /// Format: see [`DIRENT_HEAD`].
  const NAMELEN_AT: usize = 16;
  /// Format: the alignment each directory entry is padded to.
  const ALIGN: usize = 8;
  let mut at = 0usize;
  std::iter::from_fn(move || {
    let entry = u64_at(page, at)?;
    let dirent = at.checked_add(EntryOut::LEN)?;
    let namelen_at = dirent.checked_add(NAMELEN_AT)?;
    let namelen = page.get(namelen_at..namelen_at.checked_add(size_of::<u32>())?)?;
    let namelen = usize::try_from(u32::from_le_bytes(namelen.try_into().ok()?)).ok()?;
    let name_at = dirent.checked_add(DIRENT_HEAD)?;
    let name = page.get(name_at..name_at.checked_add(namelen)?)?;
    at = dirent.checked_add(DIRENT_HEAD.checked_add(namelen)?.next_multiple_of(ALIGN))?;
    Some((entry, name))
  })
}

/// The reply body room `out` leaves after the header, as a request size: a read or a directory page is
/// clamped to it, so its reply always fits what the caller posted (AUD-29-85).
fn body_room(out: &[u8]) -> u32 {
  u32::try_from(out.len().saturating_sub(OUT_HEADER_LEN)).unwrap_or(u32::MAX)
}

/// Whether a request carries its caller's uid and gid into the seam: a creating request stamps them on what it makes
/// (AUD-29-81), and a `READLINK` is answered by whether its caller owns a link that leads out of the volume (A-107).
fn creates(opcode: Opcode) -> bool {
  matches!(
    opcode,
    Opcode::Create | Opcode::MkNod | Opcode::MkDir | Opcode::SymLink | Opcode::ReadLink
  )
}

/// A creation mode with the request's umask applied (AUD-29-80): the permission bits less the umask's.
/// Under `FUSE_DONT_MASK` the kernel sends the mode unmasked and the umask beside it; without it the kernel
/// masked the mode already and masking again changes nothing (`m & !u & !u == m & !u`), so the creation
/// policy is applied exactly once in effect on either negotiated path. A body too short to carry the umask
/// is refused before this (`EIO`).
fn masked(mode: u32, umask: u32) -> u32 {
  permission_bits(mode) & !umask
}

/// The little-endian `u32` at `at` in a request body, if the body holds it.
fn body_u32(body: &[u8], at: usize) -> Option<u32> {
  let end = at.checked_add(size_of::<u32>())?;
  body
    .get(at..end)
    .and_then(|field| field.try_into().ok())
    .map(u32::from_le_bytes)
}

/// The object the kernel's node id names: node id 1 is the root (resolved through the bridge under
/// `cx`), every other node id is already the inode number. The FUSE node id carries no generation
/// (generation-tracked node-id reuse is owed), so the object's generation is zero.
pub(crate) fn resolve(
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  nodeid: u64,
) -> Result<ObjectId, VfsError> {
  let inode = if nodeid == FUSE_ROOT_ID {
    bridge.root(cx)?
  } else {
    nodeid
  };
  Ok(ObjectId::new(inode, 0))
}

/// Maps a volume refusal to its POSIX errno. This is the FUSE edge's error vocabulary; each other
/// transport maps the same [`VfsError`] to its own wire error.
pub(crate) fn errno(e: VfsError) -> i32 {
  match e {
    VfsError::NotFound => ENOENT,
    VfsError::AlreadyExists => EEXIST,
    VfsError::NotDirectory => ENOTDIR,
    VfsError::IsDirectory => EISDIR,
    VfsError::NotEmpty => ENOTEMPTY,
    VfsError::NoSpace => ENOSPC,
    VfsError::FileTooLarge => EFBIG,
    VfsError::TooManyLinks => EMLINK,
    VfsError::NotPermitted => EPERM,
    VfsError::LinkProtected => EACCES,
    VfsError::SpecialFileOperation => EOPNOTSUPP,
    VfsError::NoAttribute => ENODATA,
    VfsError::Invalid | VfsError::InvalidName => EINVAL,
    VfsError::NameTooLong => ENAMETOOLONG,
    VfsError::BaseUnavailable(code) => code,
    // The bridge's open-handle table is full (audit BUG-4); the kernel's errno for it is EMFILE.
    VfsError::Memory(slates_mem::MemError::SlabFull { .. }) => EMFILE,
    VfsError::PublishNeeded => EAGAIN,
    // The file is delegated to an NFSv4 client; its recall was asked for (A-79).
    VfsError::Delegated => EAGAIN,
    _ => EIO,
  }
}

/// The FUSE `d_type` for a volume entry kind.
fn dtype(kind: Kind) -> u32 {
  match kind {
    Kind::Dir => DT_DIR,
    Kind::File => DT_REG,
    Kind::Symlink => DT_LNK,
    Kind::Fifo => DT_FIFO,
    Kind::Socket => DT_SOCK,
  }
}

/// Splits a signed nanosecond time into (seconds, nanoseconds), clamping a negative time to
/// zero (the kernel takes unsigned seconds).
fn split_ns(ns: i64) -> (u64, u32) {
  /// Format: nanoseconds per second, splitting a time into (seconds, nanoseconds).
  const NS_PER_SEC: u64 = 1_000_000_000;
  let ns = u64::try_from(ns).unwrap_or(0);
  (ns / NS_PER_SEC, u32::try_from(ns % NS_PER_SEC).unwrap_or(0))
}

/// A FUSE `fuse_attr` from neutral attributes: each wire time is the neutral time of the same
/// name. (Until the GAP-A9-3 sweep of 2026-09-14 the wire change time was taken from the
/// modification time, so a `stat` through a FUSE mount reported `ctime == mtime` — a chmod, which
/// moves only the change time, was invisible to a tool watching it.)
fn fuse_attr(node: &NodeAttr) -> Attr {
  Attr {
    ino: node.ino,
    size: node.size,
    blocks: node.size.div_ceil(BYTES_PER_BLOCK),
    mtime: split_ns(node.mtime),
    ctime: split_ns(node.ctime),
    atime: split_ns(node.atime),
    mode: wire_mode(node.kind, node.mode),
    nlink: node.nlink,
    uid: node.uid,
    gid: node.gid,
    blksize: BLKSIZE,
  }
}

/// Format: `S_IFREG`, the `<sys/stat.h>` file-type bits of `st_mode` for a regular file.
const S_IFREG: u32 = 0o100_000;
/// Format: `S_IFDIR`, the file-type bits for a directory.
const S_IFDIR: u32 = 0o040_000;
/// Format: `S_IFLNK`, the file-type bits for a symbolic link.
const S_IFLNK: u32 = 0o120_000;
/// Format: the permission bits below the type bits (`07777`: the permission triads and the set-id
/// and sticky bits).
const PERMISSION_BITS: u32 = 0o7777;

/// The `st_mode` the FUSE wire carries for an object: its type bits from `kind` and its permission
/// bits — the seam reports the two apart (`NodeAttr::kind` and the permission-bits `mode` the volume
/// keeps, as the NFS edge's `ftype3`/`mode` do), while the kernel validates every attribute reply's
/// mode for a file type it knows (`fs/fuse/dir.c` `fuse_invalid_attr` → `fuse_valid_type`) and marks
/// the inode **bad** — `EIO` on everything after — when there is none. Before, the reply carried the
/// permission bits alone, so the root's first `GETATTR` made the mount unusable on a real kernel
/// (`docs/bugs/2026-09-19-fuse-attribute-replies-carry-no-file-type-bits.md`).
fn wire_mode(kind: Kind, permissions: u32) -> u32 {
  let kind_bits = match kind {
    Kind::File => S_IFREG,
    Kind::Dir => S_IFDIR,
    Kind::Symlink => S_IFLNK,
    Kind::Fifo => slates_vfs::export::MODE_FIFO,
    Kind::Socket => slates_vfs::export::MODE_SOCKET,
  };
  kind_bits | (permissions & PERMISSION_BITS)
}

/// The permission bits of a mode the kernel sent (`fuse_create_in.mode` carries `S_IFREG`, a
/// `FATTR_MODE` the inode's type bits): the volume keeps permission bits alone, so the type bits are
/// the wire's and stop here.
fn permission_bits(mode: u32) -> u32 {
  mode & PERMISSION_BITS
}

/// Format: the set-user-id, set-group-id and group-execute mode bits (`<sys/stat.h>` `S_ISUID`,
/// `S_ISGID`, `S_IXGRP`), for `FATTR_KILL_SUIDGID`.
const S_ISUID: u32 = 0o4000;
const S_ISGID: u32 = 0o2000;
const S_IXGRP: u32 = 0o010;

/// The mode `FATTR_KILL_SUIDGID` asks for: the set-user-id bit cleared, and the set-group-id bit
/// cleared when the group-execute bit is set (a set-group-id bit without group execute is
/// mandatory locking, not a privilege — the kernel's own `should_remove_suid` rule).
fn kill_privileges(mode: u32) -> u32 {
  let mut mode = mode & !S_ISUID;
  if mode & S_IXGRP != 0 {
    mode &= !S_ISGID;
  }
  mode
}

/// Takes the FUSE lookup reference on an entry the mount returns (LOOKUP/CREATE/MKDIR/SYMLINK): the
/// kernel is handed a node id it may address until it forgets it, so the object is pinned now (§3
/// of the inode-addressed-io design; NFS, which has no FORGET, takes no such reference and never
/// calls this). Called after a successful entry-returning op; a reference failure fails the reply,
/// because the kernel must never receive a node id the mount did not reference.
fn referenced(
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  node: NodeAttr,
) -> Result<NodeAttr, VfsError> {
  bridge.reference(ObjectId::new(node.ino, node.generation), cx)?;
  Ok(node)
}

/// The wire cache lifetime (whole seconds, nanoseconds) for `object` — the seam's posture (§4.6):
/// forever for the volume's own objects, which slates invalidates explicitly on every mutation
/// through another transport, and the base filesystem's timestamp granularity for a live base
/// entry, which an outsider may change with no hint.
fn valid_for(bridge: &mut dyn Bridge, cx: &OpContext, ino: u64) -> (u64, u32) {
  /// Format: nanoseconds per second, splitting a lifetime into (seconds, nanoseconds).
  const NS_PER_SEC: u64 = 1_000_000_000;
  match bridge.cache_lifetime(ObjectId::new(ino, 0), cx) {
    CacheLifetime::Forever => (CACHE_FOREVER, 0),
    CacheLifetime::Bounded { ns } => (ns / NS_PER_SEC, u32::try_from(ns % NS_PER_SEC).unwrap_or(0)),
  }
}

/// A FUSE `fuse_entry_out` from neutral attributes with the object's cache lifetime.
fn entry_out(node: &NodeAttr, valid: (u64, u32)) -> EntryOut {
  EntryOut {
    nodeid: node.ino,
    generation: node.generation,
    entry_valid: valid.0,
    attr_valid: valid.0,
    entry_valid_nsec: valid.1,
    attr_valid_nsec: valid.1,
    attr: fuse_attr(node),
  }
}

/// An entry reply for a successful entry-returning operation: the lifetime is read from the seam
/// for the object the entry names.
fn entry_reply(
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  result: Result<NodeAttr, VfsError>,
) -> Result<EntryOut, VfsError> {
  let node = result?;
  let valid = valid_for(bridge, cx, node.ino);
  Ok(entry_out(&node, valid))
}

/// An attribute reply (`GETATTR`/`SETATTR`) with the object's cache lifetime.
fn attr_reply(
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  result: Result<NodeAttr, VfsError>,
) -> Result<AttrOut, VfsError> {
  let node = result?;
  let (attr_valid, attr_valid_nsec) = valid_for(bridge, cx, node.ino);
  Ok(AttrOut {
    attr_valid,
    attr_valid_nsec,
    attr: fuse_attr(&node),
  })
}

/// A FUSE `fuse_statfs_out` from neutral filesystem statistics.
fn statfs_out(fs: &FsStat) -> StatfsOut {
  StatfsOut {
    blocks: fs.blocks,
    bfree: fs.bfree,
    bavail: fs.bavail,
    files: fs.files,
    ffree: fs.ffree,
    bsize: fs.bsize,
    namelen: fs.namelen,
    frsize: fs.frsize,
  }
}

/// Writes the reply the codec produced, or drops it (returns 0) when even the header did not
/// fit — the caller sends nothing and the kernel times the request out rather than reading a
/// malformed reply.
fn write_or_drop(written: Result<usize, crate::error::FuseError>, _out: &mut [u8]) -> usize {
  written.unwrap_or(0)
}

/// Recovers the unique id from a message long enough to hold the header's first two words plus
/// the unique, so a parse failure can still be answered.
fn recover_unique(message: &[u8]) -> Option<u64> {
  // Format: the unique id's offset in fuse_in_header (after len and opcode, two u32).
  const AT_UNIQUE: usize = 2 * size_of::<u32>();
  message
    .get(AT_UNIQUE..AT_UNIQUE.saturating_add(size_of::<u64>()))
    .map(|b| u64::from_le_bytes(b.try_into().unwrap_or_default()))
}

fn serve_init(body: &[u8], unique: u64, coherence: CacheCoherence, out: &mut [u8]) -> usize {
  match negotiate(body, coherence) {
    Ok(n) => write_or_drop(ReplyHeader::write_ok(unique, &n.to_bytes(), out), out),
    Err(_) => write_or_drop(ReplyHeader::write_error(unique, EIO, out), out),
  }
}

/// Replies with a body the bridge produced, or the errno its refusal maps to.
fn reply<T>(
  unique: u64,
  result: Result<T, VfsError>,
  encode: impl FnOnce(&T) -> Vec<u8>,
  out: &mut [u8],
) -> usize {
  match result {
    Ok(value) => write_or_drop(ReplyHeader::write_ok(unique, &encode(&value), out), out),
    Err(e) => write_or_drop(ReplyHeader::write_error(unique, errno(e), out), out),
  }
}

/// Replies with only an error (no body).
fn reply_err(unique: u64, e: VfsError, out: &mut [u8]) -> usize {
  write_or_drop(ReplyHeader::write_error(unique, errno(e), out), out)
}

/// The reply to a request whose body the codec refused: `EILSEQ` for a name that is not UTF-8 (a well-formed request
/// naming what a volume cannot hold), `EIO` for a malformed message.
fn refuse_unparsed(unique: u64, refusal: crate::error::FuseError, out: &mut [u8]) -> usize {
  let errno = match refusal {
    crate::error::FuseError::NameNotUtf8 => EILSEQ,
    _ => EIO,
  };
  write_or_drop(ReplyHeader::write_error(unique, errno, out), out)
}

fn serve_lookup(
  bridge: &mut dyn Bridge,
  req: &Request<'_>,
  cx: &OpContext,
  out: &mut [u8],
) -> usize {
  let name = match parse_name(req.body) {
    Ok(name) => name,
    Err(refusal) => return refuse_unparsed(req.header.unique, refusal, out),
  };
  let parent = match resolve(bridge, cx, req.header.nodeid) {
    Ok(object) => object,
    Err(e) => return reply_err(req.header.unique, e, out),
  };
  let looked = bridge.lookup(parent, cx, name);
  let valid = valid_for(bridge, cx, parent.inode);
  if matches!(looked, Err(VfsError::NotFound)) && valid != (0, 0) {
    // A miss is a negative entry (node id 0, no reference taken), cached for the directory's lifetime: the
    // kernel answers the next probe of the name itself. Every way a name appears through another attachment
    // invalidates it (`Invalidation::Entry` for create, mknod, mkdir, link, symlink, a rename's target), and a
    // live base directory's lifetime is bounded, as its positive entries' are (§4.6 "Cache posture"). With no
    // lifetime (a virtio-fs guest, which has no invalidation channel) there is nothing to cache, so the miss is
    // `ENOENT`, as it was: the same to the kernel, and what a guest has always been told.
    let negative = EntryOut {
      entry_valid: valid.0,
      entry_valid_nsec: valid.1,
      ..EntryOut::default()
    };
    return reply(req.header.unique, Ok(negative), |e| e.to_bytes(), out);
  }
  let result = looked.and_then(|n| referenced(bridge, cx, n));
  let result = entry_reply(bridge, cx, result);
  reply(req.header.unique, result, |e| e.to_bytes(), out)
}

fn serve_getattr(
  bridge: &mut dyn Bridge,
  req: &Request<'_>,
  cx: &OpContext,
  out: &mut [u8],
) -> usize {
  let object = match resolve(bridge, cx, req.header.nodeid) {
    Ok(object) => object,
    Err(e) => return reply_err(req.header.unique, e, out),
  };
  let result = bridge.getattr(object, cx);
  let result = attr_reply(bridge, cx, result);
  reply(req.header.unique, result, |a| a.to_bytes(), out)
}

/// The `fuse_open_out.open_flags` for opening `object`: the kernel keeps its pages (and a directory's listing) across
/// opens when the object is the volume's own, whose changes are all invalidated explicitly, and sends no `FLUSH` at
/// the close of a read-only handle (§4.6 "Cache posture"). Without them every open dropped the file's pages and
/// every close of any handle was a round trip (open, read and close of a cached file: 4 requests where 2 suffice).
fn open_flags_for(
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  object: ObjectId,
  directory: bool,
  flags: u32,
) -> u32 {
  use crate::abi::open::{CACHE_DIR, KEEP_CACHE, NOFLUSH};
  let mut open_flags = 0;
  if matches!(bridge.cache_lifetime(object, cx), CacheLifetime::Forever) {
    open_flags |= if directory {
      KEEP_CACHE | CACHE_DIR
    } else {
      KEEP_CACHE
    };
  }
  if !directory && flags & crate::abi::ACCESS_MODE == crate::abi::READ_ONLY {
    open_flags |= NOFLUSH;
  }
  open_flags
}

fn serve_open(
  bridge: &mut dyn Bridge,
  req: &Request<'_>,
  cx: &OpContext,
  opcode: Opcode,
  out: &mut [u8],
) -> usize {
  // `fuse_open_in`: flags (4), open_flags (4). slates reads the open flags.
  let flags = req
    .body
    .get(..size_of::<u32>())
    .map(|b| u32::from_le_bytes(b.try_into().unwrap_or_default()))
    .unwrap_or(0);
  let object = match resolve(bridge, cx, req.header.nodeid) {
    Ok(object) => object,
    Err(e) => return reply_err(req.header.unique, e, out),
  };
  let directory = opcode == Opcode::OpenDir;
  let opened = if directory {
    bridge.opendir(object, cx)
  } else {
    bridge.open(object, cx, flags)
  };
  let open_flags = open_flags_for(bridge, cx, object, directory, flags);
  reply(
    req.header.unique,
    opened.map(|fh| OpenOut { fh, open_flags }),
    |o| o.to_bytes(),
    out,
  )
}

fn serve_read(bridge: &mut dyn Bridge, req: &Request<'_>, cx: &OpContext, out: &mut [u8]) -> usize {
  let Ok(r) = ReadIn::parse(Opcode::Read.to_wire(), req.body) else {
    return write_or_drop(ReplyHeader::write_error(req.header.unique, EIO, out), out);
  };
  // A read is never on the root, so the node id is the file's inode; the FUSE node id carries no
  // generation (generation-tracked node-id reuse is owed), so it is zero.
  let object = ObjectId {
    inode: req.header.nodeid,
    generation: 0,
  };
  let mut data = Vec::new();
  match bridge.read(object, cx, r.offset, r.size.min(body_room(out)), &mut data) {
    Ok(()) => write_or_drop(ReplyHeader::write_ok(req.header.unique, &data, out), out),
    Err(e) => reply_err(req.header.unique, e, out),
  }
}

fn serve_write(
  bridge: &mut dyn Bridge,
  req: &Request<'_>,
  cx: &OpContext,
  out: &mut [u8],
) -> usize {
  let Ok(w) = WriteIn::parse(Opcode::Write.to_wire(), req.body) else {
    return write_or_drop(ReplyHeader::write_error(req.header.unique, EIO, out), out);
  };
  let object = ObjectId {
    inode: req.header.nodeid,
    generation: 0,
  };
  reply(
    req.header.unique,
    bridge
      .write(object, cx, w.offset, w.data)
      .map(|size| WriteOut { size }),
    |o| o.to_bytes(),
    out,
  )
}

/// Format: `EOVERFLOW` (Linux) — what a directory page answers when the entries sharing one cookie are more
/// than the reply can hold: a page cut inside the group would skip its tail on resume (AUD-29-86).
const EOVERFLOW: i32 = 75;

/// How many of `entries`, in order, fit `room` bytes when each takes `size(name length)` bytes.
fn fitting(entries: &[DirEntry], room: usize, size: fn(usize) -> usize) -> usize {
  let mut used = 0usize;
  let mut count = 0usize;
  for entry in entries {
    match used.checked_add(size(entry.name.len())) {
      Some(total) if total <= room => {
        used = total;
        count = count.saturating_add(1);
      }
      _ => break,
    }
  }
  count
}

/// The page a directory read returns (AUD-29-86): the room the request and the reply allow; the bridge asked
/// for at most as many entries as the smallest could fill, plus one so the page's end shows; the entries that
/// fit, cut between cookies ([`whole_cookie_groups`]). `Err(EOVERFLOW)` when one group of a shared cookie
/// is larger than the page.
fn directory_page(
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  object: ObjectId,
  r: &ReadIn,
  room: usize,
  size: fn(usize) -> usize,
) -> Result<Vec<DirEntry>, i32> {
  /// Shape: the shortest name an entry has — one byte — which sizes the smallest entry a page holds.
  const SHORTEST_NAME: usize = 1;
  let limit = room
    .checked_div(size(SHORTEST_NAME))
    .unwrap_or(0)
    .saturating_add(1);
  let mut entries = bridge
    .readdir(object, cx, r.fh, r.offset, limit)
    .map_err(errno)?;
  let sent = whole_cookie_groups(&entries, fitting(&entries, room, size));
  if sent == 0 && !entries.is_empty() {
    return Err(EOVERFLOW);
  }
  entries.truncate(sent);
  Ok(entries)
}

fn serve_readdir(
  bridge: &mut dyn Bridge,
  req: &Request<'_>,
  cx: &OpContext,
  out: &mut [u8],
) -> usize {
  let Ok(r) = ReadIn::parse(Opcode::ReadDir.to_wire(), req.body) else {
    return write_or_drop(ReplyHeader::write_error(req.header.unique, EIO, out), out);
  };
  let object = match resolve(bridge, cx, req.header.nodeid) {
    Ok(object) => object,
    Err(e) => return reply_err(req.header.unique, e, out),
  };
  let room = usize::try_from(r.size.min(body_room(out))).unwrap_or(0);
  let entries = match directory_page(bridge, cx, object, &r, room, DirBuffer::dirent_len) {
    Ok(entries) => entries,
    Err(code) => return write_or_drop(ReplyHeader::write_error(req.header.unique, code, out), out),
  };
  let mut dir = DirBuffer::new(room);
  for entry in &entries {
    // Each entry's cookie resumes the listing after it, stable across other names' changes.
    if !dir.push(entry.ino, entry.cookie, dtype(entry.kind), &entry.name) {
      break;
    }
  }
  write_or_drop(
    ReplyHeader::write_ok(req.header.unique, dir.as_bytes(), out),
    out,
  )
}

fn serve_readdirplus(
  bridge: &mut dyn Bridge,
  req: &Request<'_>,
  cx: &OpContext,
  out: &mut [u8],
) -> usize {
  let Ok(r) = ReadIn::parse(Opcode::ReadDirPlus.to_wire(), req.body) else {
    return write_or_drop(ReplyHeader::write_error(req.header.unique, EIO, out), out);
  };
  let object = match resolve(bridge, cx, req.header.nodeid) {
    Ok(object) => object,
    Err(e) => return reply_err(req.header.unique, e, out),
  };
  let room = usize::try_from(r.size.min(body_room(out))).unwrap_or(0);
  let entries = match directory_page(bridge, cx, object, &r, room, DirBuffer::plus_len) {
    Ok(entries) => entries,
    Err(code) => return write_or_drop(ReplyHeader::write_error(req.header.unique, code, out), out),
  };
  let mut dir = DirBuffer::new(room);
  for entry in &entries {
    let child = ObjectId::new(entry.ino, 0);
    // Each entry carries its attributes so the kernel needs no follow-up LOOKUP. Attributes are
    // best-effort; an entry whose attributes cannot be fetched is skipped rather than failing the
    // whole listing.
    let Ok(node) = bridge.getattr(child, cx) else {
      continue;
    };
    // READDIRPLUS takes a lookup reference on each child it returns (like LOOKUP), except the
    // synthetic "." and ".." which the kernel handles specially and never forgets. Whether the
    // entry fits is asked first, so an entry the page cannot return is never referenced (AUD-29-85).
    let synthetic = entry.name == "." || entry.name == "..";
    if !dir.fits_plus(&entry.name) {
      break;
    }
    if !synthetic && bridge.reference(child, cx).is_err() {
      break;
    }
    let valid = valid_for(bridge, cx, entry.ino);
    if !dir.push_plus(
      &entry_out(&node, valid),
      entry.cookie,
      dtype(entry.kind),
      &entry.name,
    ) {
      if !synthetic {
        bridge.forget(child, cx, 1);
      }
      break;
    }
  }
  write_or_drop(
    ReplyHeader::write_ok(req.header.unique, dir.as_bytes(), out),
    out,
  )
}

fn serve_create(
  bridge: &mut dyn Bridge,
  req: &Request<'_>,
  cx: &OpContext,
  out: &mut [u8],
) -> usize {
  // Format: fuse_create_in's fixed part before the name: flags, mode, umask, open_flags.
  const HEAD: usize = 4 * size_of::<u32>();
  if req.body.len() < HEAD {
    return write_or_drop(ReplyHeader::write_error(req.header.unique, EIO, out), out);
  }
  let flags = body_u32(req.body, 0).unwrap_or_default();
  let mode = masked(
    body_u32(req.body, size_of::<u32>()).unwrap_or_default(),
    body_u32(req.body, size_of::<u32>().saturating_mul(2)).unwrap_or_default(),
  );
  let name = match parse_name(req.body.get(HEAD..).unwrap_or_default()) {
    Ok(name) => name,
    Err(refusal) => return refuse_unparsed(req.header.unique, refusal, out),
  };
  let parent = match resolve(bridge, cx, req.header.nodeid) {
    Ok(object) => object,
    Err(e) => return reply_err(req.header.unique, e, out),
  };
  match bridge.create(parent, cx, name, mode, flags) {
    Ok((node, fh)) => {
      // Take the FUSE lookup reference on the created entry; if it fails, fail the reply (the
      // kernel must not receive an unreferenced node id). The open reference the create already
      // took is dropped by the matching release regardless.
      if let Err(e) = bridge.reference(ObjectId::new(node.ino, node.generation), cx) {
        return reply_err(req.header.unique, e, out);
      }
      let valid = valid_for(bridge, cx, node.ino);
      let open_flags = open_flags_for(bridge, cx, ObjectId::new(node.ino, 0), false, flags);
      let mut body = entry_out(&node, valid).to_bytes();
      body.extend_from_slice(&OpenOut { fh, open_flags }.to_bytes());
      write_or_drop(ReplyHeader::write_ok(req.header.unique, &body, out), out)
    }
    Err(e) => reply_err(req.header.unique, e, out),
  }
}

fn serve_release(
  bridge: &mut dyn Bridge,
  req: &Request<'_>,
  cx: &OpContext,
  out: &mut [u8],
) -> usize {
  // `fuse_release_in`: fh (8), then fields slates does not use.
  let fh = req
    .body
    .get(..size_of::<u64>())
    .map(|b| u64::from_le_bytes(b.try_into().unwrap_or_default()))
    .unwrap_or(0);
  // A release is on an open file, never the root; the node id is the inode (generation zero).
  let object = ObjectId::new(req.header.nodeid, 0);
  reply(
    req.header.unique,
    bridge.release(object, cx, fh),
    |()| Vec::new(),
    out,
  )
}

fn serve_flush(
  bridge: &mut dyn Bridge,
  req: &Request<'_>,
  cx: &OpContext,
  out: &mut [u8],
  fsync: bool,
) -> usize {
  let fh = req
    .body
    .get(..size_of::<u64>())
    .map(|b| u64::from_le_bytes(b.try_into().unwrap_or_default()))
    .unwrap_or(0);
  let object = ObjectId::new(req.header.nodeid, 0);
  let flushed = if fsync {
    bridge.fsync(object, cx, fh)
  } else {
    bridge.flush(object, cx, fh)
  };
  reply(req.header.unique, flushed, |()| Vec::new(), out)
}

fn serve_forget(bridge: &mut dyn Bridge, req: &Request<'_>, cx: &OpContext) -> usize {
  // `fuse_forget_in`: nlookup (8). FORGET has no reply.
  let nlookup = req
    .body
    .get(..size_of::<u64>())
    .map(|b| u64::from_le_bytes(b.try_into().unwrap_or_default()))
    .unwrap_or(0);
  bridge.forget(ObjectId::new(req.header.nodeid, 0), cx, nlookup);
  0
}

fn serve_batch_forget(bridge: &mut dyn Bridge, req: &Request<'_>, cx: &OpContext) -> usize {
  // `fuse_batch_forget_in`: count (4), dummy (4); then `count` × `fuse_forget_one`: nodeid (8),
  // nlookup (8). BATCH_FORGET has no reply. The count is bounded by the body: only the complete
  // entries present are applied, so an overclaimed count never reads past the message.
  const HEAD: usize = 2 * size_of::<u32>();
  const ENTRY: usize = 2 * size_of::<u64>();
  let Some(entries) = req.body.get(HEAD..) else {
    return 0;
  };
  let claimed = req
    .body
    .get(..size_of::<u32>())
    .map(|b| u32::from_le_bytes(b.try_into().unwrap_or_default()))
    .map_or(0, |count| usize::try_from(count).unwrap_or(usize::MAX));
  let (whole, _partial) = entries.as_chunks::<ENTRY>();
  for entry in whole.iter().take(claimed) {
    let (nodeid, nlookup) = entry.split_at(size_of::<u64>());
    let nodeid = u64::from_le_bytes(nodeid.try_into().unwrap_or_default());
    let nlookup = u64::from_le_bytes(nlookup.try_into().unwrap_or_default());
    bridge.forget(ObjectId::new(nodeid, 0), cx, nlookup);
  }
  0
}

fn serve_destroy(
  bridge: &mut dyn Bridge,
  req: &Request<'_>,
  cx: &OpContext,
  out: &mut [u8],
) -> usize {
  reply(
    req.header.unique,
    bridge.sweep_attachment(cx),
    |()| Vec::new(),
    out,
  )
}

/// Linux fuse_mknod_in (four u32 words), followed by the NUL-terminated name (A-26).
fn serve_mknod(
  bridge: &mut dyn Bridge,
  req: &Request<'_>,
  cx: &OpContext,
  out: &mut [u8],
) -> usize {
  /// Format: mode, rdev, umask, padding in Linux include/uapi/linux/fuse.h.
  const HEAD: usize = 4 * size_of::<u32>();
  let Some(head) = req.body.get(..HEAD) else {
    return write_or_drop(ReplyHeader::write_error(req.header.unique, EIO, out), out);
  };
  let mode = u32::from_le_bytes(
    head
      .get(..size_of::<u32>())
      .unwrap_or_default()
      .try_into()
      .unwrap_or_default(),
  );
  let kind = match slates_vfs::export::kind_of_mode(mode) {
    Some(kind @ (Kind::Fifo | Kind::Socket)) => kind,
    _ => return reply_err(req.header.unique, VfsError::SpecialFileOperation, out),
  };
  let name = match parse_name(req.body.get(HEAD..).unwrap_or_default()) {
    Ok(name) => name,
    Err(refusal) => return refuse_unparsed(req.header.unique, refusal, out),
  };
  let parent = match resolve(bridge, cx, req.header.nodeid) {
    Ok(parent) => parent,
    Err(error) => return reply_err(req.header.unique, error, out),
  };
  let result = bridge
    .mknod(
      parent,
      cx,
      name,
      masked(
        mode,
        body_u32(head, size_of::<u32>().saturating_mul(2)).unwrap_or_default(),
      ),
      kind,
    )
    .and_then(|node| referenced(bridge, cx, node));
  let result = entry_reply(bridge, cx, result);
  reply(req.header.unique, result, |entry| entry.to_bytes(), out)
}

fn serve_mkdir(
  bridge: &mut dyn Bridge,
  req: &Request<'_>,
  cx: &OpContext,
  out: &mut [u8],
) -> usize {
  // `fuse_mkdir_in`: mode (4), umask (4), then the name.
  const HEAD: usize = 2 * size_of::<u32>();
  if req.body.len() < HEAD {
    return write_or_drop(ReplyHeader::write_error(req.header.unique, EIO, out), out);
  }
  let mode = masked(
    body_u32(req.body, 0).unwrap_or_default(),
    body_u32(req.body, size_of::<u32>()).unwrap_or_default(),
  );
  let name = match parse_name(req.body.get(HEAD..).unwrap_or_default()) {
    Ok(name) => name,
    Err(refusal) => return refuse_unparsed(req.header.unique, refusal, out),
  };
  let parent = match resolve(bridge, cx, req.header.nodeid) {
    Ok(object) => object,
    Err(e) => return reply_err(req.header.unique, e, out),
  };
  let result = bridge
    .mkdir(parent, cx, name, mode)
    .and_then(|n| referenced(bridge, cx, n));
  let result = entry_reply(bridge, cx, result);
  reply(req.header.unique, result, |e| e.to_bytes(), out)
}

fn serve_unlink(
  bridge: &mut dyn Bridge,
  req: &Request<'_>,
  cx: &OpContext,
  is_dir: bool,
  out: &mut [u8],
) -> usize {
  let name = match parse_name(req.body) {
    Ok(name) => name,
    Err(refusal) => return refuse_unparsed(req.header.unique, refusal, out),
  };
  let parent = match resolve(bridge, cx, req.header.nodeid) {
    Ok(object) => object,
    Err(e) => return reply_err(req.header.unique, e, out),
  };
  let result = if is_dir {
    bridge.rmdir(parent, cx, name)
  } else {
    bridge.unlink(parent, cx, name)
  };
  reply(req.header.unique, result, |()| Vec::new(), out)
}

fn serve_symlink(
  bridge: &mut dyn Bridge,
  req: &Request<'_>,
  cx: &OpContext,
  out: &mut [u8],
) -> usize {
  // The body is name\0 target\0.
  let name = match parse_name(req.body) {
    Ok(name) => name,
    Err(refusal) => return refuse_unparsed(req.header.unique, refusal, out),
  };
  let rest = req
    .body
    .get(name.len().saturating_add(1)..)
    .unwrap_or_default();
  let target = match parse_name(rest) {
    Ok(target) => target,
    Err(refusal) => return refuse_unparsed(req.header.unique, refusal, out),
  };
  let parent = match resolve(bridge, cx, req.header.nodeid) {
    Ok(object) => object,
    Err(e) => return reply_err(req.header.unique, e, out),
  };
  let result = bridge
    .symlink(parent, cx, name, target)
    .and_then(|n| referenced(bridge, cx, n));
  let result = entry_reply(bridge, cx, result);
  reply(req.header.unique, result, |e| e.to_bytes(), out)
}

fn serve_link(bridge: &mut dyn Bridge, req: &Request<'_>, cx: &OpContext, out: &mut [u8]) -> usize {
  // fuse_link_in: oldnodeid (8) — the existing inode to link — then the new name in this request's
  // directory (the header node id).
  const HEAD: usize = size_of::<u64>();
  let Some(head) = req.body.get(..HEAD) else {
    return write_or_drop(ReplyHeader::write_error(req.header.unique, EIO, out), out);
  };
  let oldnodeid = u64::from_le_bytes(head.try_into().unwrap_or_default());
  let name = match parse_name(req.body.get(HEAD..).unwrap_or_default()) {
    Ok(name) => name,
    Err(refusal) => return refuse_unparsed(req.header.unique, refusal, out),
  };
  let new_parent = match resolve(bridge, cx, req.header.nodeid) {
    Ok(object) => object,
    Err(e) => return reply_err(req.header.unique, e, out),
  };
  // The target is an existing inode, never the root; the FUSE node id carries no generation.
  let target = ObjectId::new(oldnodeid, 0);
  // LINK returns an entry (the new name resolving to the target), so the kernel takes a lookup
  // reference on it — reference it as for LOOKUP/CREATE.
  let result = bridge
    .link(target, new_parent, cx, name)
    .and_then(|n| referenced(bridge, cx, n));
  let result = entry_reply(bridge, cx, result);
  reply(req.header.unique, result, |e| e.to_bytes(), out)
}

fn serve_readlink(
  bridge: &mut dyn Bridge,
  req: &Request<'_>,
  cx: &OpContext,
  out: &mut [u8],
) -> usize {
  let object = match resolve(bridge, cx, req.header.nodeid) {
    Ok(object) => object,
    Err(e) => return reply_err(req.header.unique, e, out),
  };
  match bridge.readlink(object, cx) {
    Ok(target) => write_or_drop(
      ReplyHeader::write_ok(req.header.unique, target.as_bytes(), out),
      out,
    ),
    Err(e) => reply_err(req.header.unique, e, out),
  }
}

fn serve_rename(
  bridge: &mut dyn Bridge,
  req: &Request<'_>,
  cx: &OpContext,
  flagged: bool,
  out: &mut [u8],
) -> usize {
  let opcode = if flagged {
    Opcode::Rename2.to_wire()
  } else {
    Opcode::Rename.to_wire()
  };
  let r = match RenameIn::parse(opcode, req.body, flagged) {
    Ok(r) => r,
    Err(refusal) => return refuse_unparsed(req.header.unique, refusal, out),
  };
  // A flag the seam does not carry (RENAME_WHITEOUT, or any bit the header has not defined) is
  // refused with the errno `renameat2` itself gives an unsupported flag, before anything moves —
  // never dropped and performed as a plain rename (§4.6; audit BUG-10). The kernel's FUSE client
  // refuses these itself; a guest over virtio-fs may not.
  if r.flags & !RenameIn::RENAME_CARRIED != 0 {
    return write_or_drop(
      ReplyHeader::write_error(req.header.unique, EINVAL, out),
      out,
    );
  }
  let from = match resolve(bridge, cx, req.header.nodeid) {
    Ok(object) => object,
    Err(e) => return reply_err(req.header.unique, e, out),
  };
  let to = match resolve(bridge, cx, r.newdir) {
    Ok(object) => object,
    Err(e) => return reply_err(req.header.unique, e, out),
  };
  let flags = RenameFlags {
    no_replace: r.flags & RenameIn::RENAME_NOREPLACE != 0,
    exchange: r.flags & RenameIn::RENAME_EXCHANGE != 0,
  };
  reply(
    req.header.unique,
    bridge.rename(from, to, cx, r.old_name, r.new_name, flags),
    |()| Vec::new(),
    out,
  )
}

fn serve_setattr(
  bridge: &mut dyn Bridge,
  req: &Request<'_>,
  cx: &OpContext,
  out: &mut [u8],
) -> usize {
  let Ok(s) = SetAttrIn::parse(req.body) else {
    return write_or_drop(ReplyHeader::write_error(req.header.unique, EIO, out), out);
  };
  // A `valid` bit the edge does not honour is refused before the seam, never acknowledged with a
  // success that ignored it (§4.6 "Never acknowledge an ignored `setattr` field"; audit BUG-8).
  if s.valid & !SetAttrIn::FATTR_HONOURED != 0 {
    return write_or_drop(
      ReplyHeader::write_error(req.header.unique, EINVAL, out),
      out,
    );
  }
  let object = match resolve(bridge, cx, req.header.nodeid) {
    Ok(object) => object,
    Err(e) => return reply_err(req.header.unique, e, out),
  };
  let changes = match setattr_changes(bridge, object, cx, &s) {
    Ok(changes) => changes,
    Err(e) => return reply_err(req.header.unique, e, out),
  };
  let result = bridge.setattr(object, cx, changes);
  let result = attr_reply(bridge, cx, result);
  reply(req.header.unique, result, |a| a.to_bytes(), out)
}

/// Translates a `SETATTR` request's `valid` mask and fields into the neutral "which fields to
/// set" (§4.6): a time flagged `*_NOW` (`UTIME_NOW`) is resolved through the volume's own clock —
/// the NOW resolution the design places at the transport (AC-3.10) — rather than the value the
/// kernel filled in from its clock; `FATTR_KILL_SUIDGID` becomes a mode change that clears the
/// privilege bits of the requested mode, or of the object's current mode when no mode was
/// requested (the one field this needs read back through the seam).
fn setattr_changes(
  bridge: &mut dyn Bridge,
  object: ObjectId,
  cx: &OpContext,
  s: &SetAttrIn,
) -> Result<SetAttr, VfsError> {
  let set = |bit: u32| s.valid & bit != 0;
  let now = if set(SetAttrIn::FATTR_ATIME_NOW) || set(SetAttrIn::FATTR_MTIME_NOW) {
    Some(bridge.now())
  } else {
    None
  };
  // The kernel's requested mode carries the inode's type bits (`ia_mode`); the volume keeps the
  // permission bits alone.
  let mut mode = set(SetAttrIn::FATTR_MODE).then_some(permission_bits(s.mode));
  if set(SetAttrIn::FATTR_KILL_SUIDGID) {
    let current = match mode {
      Some(mode) => mode,
      None => bridge.getattr(object, cx)?.mode,
    };
    mode = Some(kill_privileges(current));
  }
  Ok(SetAttr {
    size: set(SetAttrIn::FATTR_SIZE).then_some(s.size),
    mode,
    uid: set(SetAttrIn::FATTR_UID).then_some(s.uid),
    gid: set(SetAttrIn::FATTR_GID).then_some(s.gid),
    atime: match now {
      Some(now) if set(SetAttrIn::FATTR_ATIME_NOW) => Some(now),
      _ => set(SetAttrIn::FATTR_ATIME).then_some(s.atime),
    },
    mtime: match now {
      Some(now) if set(SetAttrIn::FATTR_MTIME_NOW) => Some(now),
      _ => set(SetAttrIn::FATTR_MTIME).then_some(s.mtime),
    },
    ctime: set(SetAttrIn::FATTR_CTIME).then_some(s.ctime),
  })
}

/// Format: the only attribute namespace a volume stores through a Linux mount, as the NFSv4.2 bridge stores it
/// (RFC 8276 names are `user.` attributes). `security.`, `trusted.` and `system.` belong to the host's LSM, its
/// administrator and its ACLs, none of which a volume carries.
const USER_NAMESPACE: &[u8] = b"user.";
/// Format: `setxattr(2)`'s `XATTR_CREATE` (fail if the name is set) and `XATTR_REPLACE` (fail unless it is).
const XATTR_CREATE: u32 = 1;
/// Format: see [`XATTR_CREATE`].
const XATTR_REPLACE: u32 = 2;

/// The name an attribute request carries (NUL-terminated after the fixed part), when it is a `user.` name.
fn user_attribute(body: &[u8]) -> Option<&[u8]> {
  let end = body.iter().position(|b| *b == 0)?;
  body
    .get(..end)
    .filter(|name| name.starts_with(USER_NAMESPACE) && name.len() > USER_NAMESPACE.len())
}

/// `GETXATTR` (and, `list`, `LISTXATTR`): `fuse_getxattr_in` is the caller's buffer size and padding, then for a
/// get the name. A size of zero asks for the length (`fuse_getxattr_out`); a buffer too small is `ERANGE`; a name
/// outside `user.` is not set (`ENODATA`); a listing names only `user.` attributes, each NUL-terminated.
fn serve_getxattr(
  bridge: &mut dyn Bridge,
  req: &Request<'_>,
  cx: &OpContext,
  out: &mut [u8],
  list: bool,
) -> usize {
  /// Format: `fuse_getxattr_in` — size, then padding — before the name.
  const HEAD: usize = 2 * size_of::<u32>();
  let unique = req.header.unique;
  let Some(size) = body_u32(req.body, 0) else {
    return write_or_drop(ReplyHeader::write_error(unique, EIO, out), out);
  };
  let object = match resolve(bridge, cx, req.header.nodeid) {
    Ok(object) => object,
    Err(e) => return reply_err(unique, e, out),
  };
  let value = if list {
    bridge.xattr_list(object, cx).map(|names| {
      names
        .iter()
        .filter(|name| name.starts_with(USER_NAMESPACE))
        .flat_map(|name| name.iter().copied().chain(std::iter::once(0)))
        .collect::<Vec<u8>>()
    })
  } else {
    match user_attribute(req.body.get(HEAD..).unwrap_or_default()) {
      Some(name) => bridge.xattr_get(object, cx, name),
      None => return write_or_drop(ReplyHeader::write_error(unique, ENODATA, out), out),
    }
  };
  let value = match value {
    Ok(value) => value,
    Err(e) => return reply_err(unique, e, out),
  };
  let length = u32::try_from(value.len()).unwrap_or(u32::MAX);
  if size == 0 {
    let mut answer = Vec::with_capacity(HEAD);
    answer.extend_from_slice(&length.to_le_bytes());
    answer.extend_from_slice(&0u32.to_le_bytes());
    return write_or_drop(ReplyHeader::write_ok(unique, &answer, out), out);
  }
  if length > size {
    return write_or_drop(ReplyHeader::write_error(unique, ERANGE, out), out);
  }
  write_or_drop(ReplyHeader::write_ok(unique, &value, out), out)
}

/// `SETXATTR`: `fuse_setxattr_in` is the value's size and the `setxattr(2)` flags (slates does not negotiate
/// `FUSE_SETXATTR_EXT`, so the short form), then the name, then the value. Only `user.` names are stored; any other
/// is `EOPNOTSUPP`, what a filesystem without that namespace answers.
fn serve_setxattr(
  bridge: &mut dyn Bridge,
  req: &Request<'_>,
  cx: &OpContext,
  out: &mut [u8],
) -> usize {
  /// Format: `fuse_setxattr_in` — size, flags — before the name.
  const HEAD: usize = 2 * size_of::<u32>();
  let unique = req.header.unique;
  let (Some(size), Some(flags)) = (body_u32(req.body, 0), body_u32(req.body, size_of::<u32>()))
  else {
    return write_or_drop(ReplyHeader::write_error(unique, EIO, out), out);
  };
  let rest = req.body.get(HEAD..).unwrap_or_default();
  let Some(name) = user_attribute(rest) else {
    return write_or_drop(ReplyHeader::write_error(unique, EOPNOTSUPP, out), out);
  };
  let value_at = name.len().saturating_add(1);
  let value_end = value_at.saturating_add(usize::try_from(size).unwrap_or(usize::MAX));
  let Some(value) = rest.get(value_at..value_end) else {
    return write_or_drop(ReplyHeader::write_error(unique, EIO, out), out);
  };
  let how = match flags & (XATTR_CREATE | XATTR_REPLACE) {
    XATTR_CREATE => slates_vfs::xattr::XattrSet::Create,
    XATTR_REPLACE => slates_vfs::xattr::XattrSet::Replace,
    0 => slates_vfs::xattr::XattrSet::Either,
    _ => return write_or_drop(ReplyHeader::write_error(unique, EINVAL, out), out),
  };
  let object = match resolve(bridge, cx, req.header.nodeid) {
    Ok(object) => object,
    Err(e) => return reply_err(unique, e, out),
  };
  reply(
    unique,
    bridge.xattr_set(object, cx, name, value, how),
    |()| Vec::new(),
    out,
  )
}

/// `REMOVEXATTR`: the body is the name. A name outside `user.` is `EOPNOTSUPP`; an unset one `ENODATA`.
fn serve_removexattr(
  bridge: &mut dyn Bridge,
  req: &Request<'_>,
  cx: &OpContext,
  out: &mut [u8],
) -> usize {
  let unique = req.header.unique;
  let Some(name) = user_attribute(req.body) else {
    return write_or_drop(ReplyHeader::write_error(unique, EOPNOTSUPP, out), out);
  };
  let object = match resolve(bridge, cx, req.header.nodeid) {
    Ok(object) => object,
    Err(e) => return reply_err(unique, e, out),
  };
  reply(
    unique,
    bridge.xattr_remove(object, cx, name),
    |()| Vec::new(),
    out,
  )
}

fn serve_statfs(
  bridge: &mut dyn Bridge,
  req: &Request<'_>,
  cx: &OpContext,
  out: &mut [u8],
) -> usize {
  let object = match resolve(bridge, cx, req.header.nodeid) {
    Ok(object) => object,
    Err(e) => return reply_err(req.header.unique, e, out),
  };
  reply(
    req.header.unique,
    bridge.statfs(object, cx).map(|fs| statfs_out(&fs)),
    |s| s.to_bytes(),
    out,
  )
}
