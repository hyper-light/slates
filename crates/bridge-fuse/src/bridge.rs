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

use crate::abi::Opcode;
use crate::init::negotiate;
use crate::reply::{Attr, AttrOut, DirBuffer, EntryOut, OpenOut, ReplyHeader, StatfsOut, WriteOut};
use crate::request::{ReadIn, RenameIn, Request, SetAttrIn, WriteIn, parse_name};

pub use slates_bridge_core::{Bridge, DirEntry};
use slates_bridge_core::{FsStat, NodeAttr, ObjectId, OpContext, RenameFlags, SetAttr};
use slates_vfs::error::VfsError;
use slates_vfs::inode::Kind;

/// Format: `ENOSYS`, the errno for an opcode the bridge does not implement.
pub const ENOSYS: i32 = 38;
/// Format: `EIO`, the errno for a request the codec could not parse.
pub const EIO: i32 = 5;
/// Format: how long the kernel may cache an entry or attributes: forever, since slates
/// invalidates explicitly on every mutation (§4.6 "Cache posture").
pub const CACHE_FOREVER: u64 = u64::MAX;
/// Format: the FUSE node id of the root directory; the kernel always names the root by it, and
/// the edge resolves it to the volume's real root inode number.
const FUSE_ROOT_ID: u64 = 1;
/// Format: the Unix `d_type` values a `readdir` entry carries.
const DT_DIR: u32 = 4;
const DT_REG: u32 = 8;
const DT_LNK: u32 = 10;
// The Linux errno values the volume core's refusals map to (the FUSE ABI is Linux, so the
// numbers are the kernel's regardless of the host the codec is tested on; the dispatch negates
// them). Each is a Format constant.
/// Format: ENOENT, no such file or directory.
const ENOENT: i32 = 2;
/// Format: EPERM, operation not permitted.
const EPERM: i32 = 1;
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
  match opcode {
    Opcode::Init => serve_init(request.body, unique, out),
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
    Opcode::Flush | Opcode::FSync | Opcode::FSyncDir => serve_flush(bridge, &request, cx, out),
    Opcode::Forget => serve_forget(bridge, &request, cx),
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
    // The rest of the Bridge trait is dispatched as the driver grows; until then the kernel
    // is told the operation is not implemented, never left waiting.
    _ => write_or_drop(ReplyHeader::write_error(unique, ENOSYS, out), out),
  }
}

/// The object the kernel's node id names: node id 1 is the root (resolved through the bridge under
/// `cx`), every other node id is already the inode number. The FUSE node id carries no generation
/// (generation-tracked node-id reuse is owed), so the object's generation is zero.
fn resolve(bridge: &mut dyn Bridge, cx: &OpContext, nodeid: u64) -> Result<ObjectId, VfsError> {
  let inode = if nodeid == FUSE_ROOT_ID {
    bridge.root(cx)?
  } else {
    nodeid
  };
  Ok(ObjectId::new(inode, 0))
}

/// Maps a volume refusal to its POSIX errno. This is the FUSE edge's error vocabulary; each other
/// transport maps the same [`VfsError`] to its own wire error.
fn errno(e: VfsError) -> i32 {
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
    VfsError::Invalid | VfsError::InvalidName => EINVAL,
    VfsError::BaseUnavailable(code) => code,
    // The bridge's open-handle table is full (audit BUG-4); the kernel's errno for it is EMFILE.
    VfsError::Memory(slates_mem::MemError::SlabFull { .. }) => EMFILE,
    _ => EIO,
  }
}

/// The FUSE `d_type` for a volume entry kind.
fn dtype(kind: Kind) -> u32 {
  match kind {
    Kind::Dir => DT_DIR,
    Kind::File => DT_REG,
    Kind::Symlink => DT_LNK,
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

/// A FUSE `fuse_attr` from neutral attributes. Preserves the pre-extraction mapping exactly,
/// including the wire change time taken from the modification time — a quirk carried unchanged
/// for the GAP-A9-3 bridge sweep to correct (NFS reads the true change time from the neutral
/// attributes, so the quirk does not spread).
fn fuse_attr(node: &NodeAttr) -> Attr {
  Attr {
    ino: node.ino,
    size: node.size,
    blocks: node.size.div_ceil(BYTES_PER_BLOCK),
    mtime: split_ns(node.mtime),
    ctime: split_ns(node.mtime),
    atime: split_ns(node.atime),
    mode: node.mode,
    nlink: node.nlink,
    uid: node.uid,
    gid: node.gid,
    blksize: BLKSIZE,
  }
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

/// A FUSE `fuse_entry_out` from neutral attributes, cached forever (slates invalidates on every
/// mutation, §4.6).
fn entry_out(node: &NodeAttr) -> EntryOut {
  EntryOut {
    nodeid: node.ino,
    generation: node.generation,
    entry_valid: CACHE_FOREVER,
    attr_valid: CACHE_FOREVER,
    attr: fuse_attr(node),
  }
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
    .get(AT_UNIQUE..AT_UNIQUE + size_of::<u64>())
    .map(|b| u64::from_le_bytes(b.try_into().unwrap_or_default()))
}

fn serve_init(body: &[u8], unique: u64, out: &mut [u8]) -> usize {
  match negotiate(body) {
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

fn serve_lookup(
  bridge: &mut dyn Bridge,
  req: &Request<'_>,
  cx: &OpContext,
  out: &mut [u8],
) -> usize {
  let Ok(name) = parse_name(req.body) else {
    return write_or_drop(ReplyHeader::write_error(req.header.unique, EIO, out), out);
  };
  let parent = match resolve(bridge, cx, req.header.nodeid) {
    Ok(object) => object,
    Err(e) => return reply_err(req.header.unique, e, out),
  };
  let result = bridge
    .lookup(parent, cx, name)
    .and_then(|n| referenced(bridge, cx, n));
  reply(req.header.unique, result, |n| entry_out(n).to_bytes(), out)
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
  let result = bridge.getattr(object, cx).map(|n| AttrOut {
    attr_valid: CACHE_FOREVER,
    attr: fuse_attr(&n),
  });
  reply(req.header.unique, result, |a| a.to_bytes(), out)
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
  let opened = if opcode == Opcode::OpenDir {
    bridge.opendir(object, cx)
  } else {
    bridge.open(object, cx, flags)
  };
  reply(
    req.header.unique,
    opened.map(|fh| OpenOut { fh, open_flags: 0 }),
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
  match bridge.read(object, cx, r.offset, r.size, &mut data) {
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
  match bridge.readdir(object, cx, r.fh, r.offset) {
    Ok(entries) => {
      let mut dir = DirBuffer::new(usize::try_from(r.size).unwrap_or(0));
      for (index, entry) in entries.iter().enumerate() {
        // The cookie is the one-based index, so the next readdir resumes after this entry.
        let cookie = r.offset.saturating_add(index as u64).saturating_add(1);
        if !dir.push(entry.ino, cookie, dtype(entry.kind), &entry.name) {
          break;
        }
      }
      write_or_drop(
        ReplyHeader::write_ok(req.header.unique, dir.as_bytes(), out),
        out,
      )
    }
    Err(e) => reply_err(req.header.unique, e, out),
  }
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
  let entries = match bridge.readdir(object, cx, r.fh, r.offset) {
    Ok(entries) => entries,
    Err(e) => return reply_err(req.header.unique, e, out),
  };
  let mut dir = DirBuffer::new(usize::try_from(r.size).unwrap_or(0));
  for (index, entry) in entries.iter().enumerate() {
    let cookie = r.offset.saturating_add(index as u64).saturating_add(1);
    let child = ObjectId::new(entry.ino, 0);
    // Each entry carries its attributes so the kernel needs no follow-up LOOKUP. Attributes are
    // best-effort; an entry whose attributes cannot be fetched is skipped rather than failing the
    // whole listing.
    let Ok(node) = bridge.getattr(child, cx) else {
      continue;
    };
    // READDIRPLUS takes a lookup reference on each child it returns (like LOOKUP), except the
    // synthetic "." and ".." which the kernel handles specially and never forgets. If the entry
    // does not fit, undo the reference and stop.
    let synthetic = entry.name == "." || entry.name == "..";
    if !synthetic && bridge.reference(child, cx).is_err() {
      break;
    }
    if !dir.push_plus(&entry_out(&node), cookie, dtype(entry.kind), &entry.name) {
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
  let flags = u32::from_le_bytes(req.body[0..size_of::<u32>()].try_into().unwrap_or_default());
  let mode = u32::from_le_bytes(
    req.body[size_of::<u32>()..2 * size_of::<u32>()]
      .try_into()
      .unwrap_or_default(),
  );
  let Ok(name) = parse_name(&req.body[HEAD..]) else {
    return write_or_drop(ReplyHeader::write_error(req.header.unique, EIO, out), out);
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
      let mut body = entry_out(&node).to_bytes();
      body.extend_from_slice(&OpenOut { fh, open_flags: 0 }.to_bytes());
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
) -> usize {
  let fh = req
    .body
    .get(..size_of::<u64>())
    .map(|b| u64::from_le_bytes(b.try_into().unwrap_or_default()))
    .unwrap_or(0);
  let object = ObjectId::new(req.header.nodeid, 0);
  reply(
    req.header.unique,
    bridge.flush(object, cx, fh),
    |()| Vec::new(),
    out,
  )
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
  let mode = u32::from_le_bytes(req.body[..size_of::<u32>()].try_into().unwrap_or_default());
  let Ok(name) = parse_name(&req.body[HEAD..]) else {
    return write_or_drop(ReplyHeader::write_error(req.header.unique, EIO, out), out);
  };
  let parent = match resolve(bridge, cx, req.header.nodeid) {
    Ok(object) => object,
    Err(e) => return reply_err(req.header.unique, e, out),
  };
  let result = bridge
    .mkdir(parent, cx, name, mode)
    .and_then(|n| referenced(bridge, cx, n));
  reply(req.header.unique, result, |n| entry_out(n).to_bytes(), out)
}

fn serve_unlink(
  bridge: &mut dyn Bridge,
  req: &Request<'_>,
  cx: &OpContext,
  is_dir: bool,
  out: &mut [u8],
) -> usize {
  let Ok(name) = parse_name(req.body) else {
    return write_or_drop(ReplyHeader::write_error(req.header.unique, EIO, out), out);
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
  let Ok(name) = parse_name(req.body) else {
    return write_or_drop(ReplyHeader::write_error(req.header.unique, EIO, out), out);
  };
  let rest = &req.body[name.len() + 1..];
  let Ok(target) = parse_name(rest) else {
    return write_or_drop(ReplyHeader::write_error(req.header.unique, EIO, out), out);
  };
  let parent = match resolve(bridge, cx, req.header.nodeid) {
    Ok(object) => object,
    Err(e) => return reply_err(req.header.unique, e, out),
  };
  let result = bridge
    .symlink(parent, cx, name, target)
    .and_then(|n| referenced(bridge, cx, n));
  reply(req.header.unique, result, |n| entry_out(n).to_bytes(), out)
}

fn serve_link(bridge: &mut dyn Bridge, req: &Request<'_>, cx: &OpContext, out: &mut [u8]) -> usize {
  // fuse_link_in: oldnodeid (8) — the existing inode to link — then the new name in this request's
  // directory (the header node id).
  const HEAD: usize = size_of::<u64>();
  if req.body.len() < HEAD {
    return write_or_drop(ReplyHeader::write_error(req.header.unique, EIO, out), out);
  }
  let oldnodeid = u64::from_le_bytes(req.body[..HEAD].try_into().unwrap_or_default());
  let Ok(name) = parse_name(&req.body[HEAD..]) else {
    return write_or_drop(ReplyHeader::write_error(req.header.unique, EIO, out), out);
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
  reply(req.header.unique, result, |n| entry_out(n).to_bytes(), out)
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
  let Ok(r) = RenameIn::parse(opcode, req.body, flagged) else {
    return write_or_drop(ReplyHeader::write_error(req.header.unique, EIO, out), out);
  };
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
  let object = match resolve(bridge, cx, req.header.nodeid) {
    Ok(object) => object,
    Err(e) => return reply_err(req.header.unique, e, out),
  };
  // Translate the FUSE `valid` bitmask into the neutral "which fields to set".
  let changes = SetAttr {
    size: (s.valid & SetAttrIn::FATTR_SIZE != 0).then_some(s.size),
    mode: (s.valid & SetAttrIn::FATTR_MODE != 0).then_some(s.mode),
    uid: (s.valid & SetAttrIn::FATTR_UID != 0).then_some(s.uid),
    gid: (s.valid & SetAttrIn::FATTR_GID != 0).then_some(s.gid),
    atime: (s.valid & SetAttrIn::FATTR_ATIME != 0).then_some(s.atime),
    mtime: (s.valid & SetAttrIn::FATTR_MTIME != 0).then_some(s.mtime),
  };
  let result = bridge.setattr(object, cx, changes).map(|n| AttrOut {
    attr_valid: CACHE_FOREVER,
    attr: fuse_attr(&n),
  });
  reply(req.header.unique, result, |a| a.to_bytes(), out)
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
