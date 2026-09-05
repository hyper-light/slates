//! The bridge semantics and the codec-to-bridge dispatch (§4.6). The kernel's requests, once
//! parsed by the codec, are turned into calls on a [`Bridge`] — the trait with one real
//! implementation in the daemon (over the volume core) and a mock in the tests here. The
//! [`dispatch`] function is the seam between the wire and the semantics: it parses a request,
//! calls the matching method, and encodes the reply or the error, so the transport (the
//! `/dev/fuse` read/write loop, Linux only) is a thin loop over it and the semantics are tested
//! on every host without a mount.
//!
//! Every reply is written into a caller buffer; a method returns either the reply value or a
//! POSIX errno (positive), which becomes the negated errno the kernel expects. An opcode slates
//! does not serve is answered `ENOSYS` without reaching the bridge.

use crate::abi::Opcode;
use crate::init::negotiate;
use crate::reply::{Attr, AttrOut, DirBuffer, EntryOut, OpenOut, ReplyHeader, WriteOut};
use crate::request::{ReadIn, Request, WriteIn, parse_name};

/// Format: `ENOSYS`, the errno for an opcode the bridge does not implement.
pub const ENOSYS: i32 = 38;
/// Format: `EIO`, the errno for a request the codec could not parse.
pub const EIO: i32 = 5;
/// Format: how long the kernel may cache an entry or attributes: forever, since slates
/// invalidates explicitly on every mutation (§4.6 "Cache posture").
pub const CACHE_FOREVER: u64 = u64::MAX;

/// One directory entry a `readdir` yields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
  /// The inode number.
  pub ino: u64,
  /// The entry kind as a Unix `d_type` (`DT_REG`, `DT_DIR`, `DT_LNK`).
  pub kind: u32,
  /// The name.
  pub name: String,
}

/// What the daemon presents to the kernel (§4.6 "Bridge trait"). One implementation over the
/// volume core lives in the daemon; a `Result::Err(errno)` becomes the kernel's negated errno.
/// Only the methods the codec dispatches are here; the rest of the trait grows with the driver.
pub trait Bridge {
  /// Look `name` up in directory `parent`; the entry (node id, generation, attributes).
  fn lookup(&mut self, parent: u64, name: &str) -> Result<EntryOut, i32>;
  /// The attributes of `nodeid`.
  fn getattr(&mut self, nodeid: u64) -> Result<Attr, i32>;
  /// Open `nodeid`; the file handle.
  fn open(&mut self, nodeid: u64, flags: u32) -> Result<u64, i32>;
  /// Read `size` bytes at `offset` from handle `fh` of `nodeid` into `out`; the bytes read.
  fn read(
    &mut self,
    nodeid: u64,
    fh: u64,
    offset: u64,
    size: u32,
    out: &mut Vec<u8>,
  ) -> Result<(), i32>;
  /// Write `data` at `offset` to handle `fh` of `nodeid`; the bytes written.
  fn write(&mut self, nodeid: u64, fh: u64, offset: u64, data: &[u8]) -> Result<u32, i32>;
  /// Open directory `nodeid`; the handle.
  fn opendir(&mut self, nodeid: u64) -> Result<u64, i32>;
  /// The entries of directory `nodeid` from `offset` (each entry's `off` is the cookie to
  /// resume from).
  fn readdir(&mut self, nodeid: u64, fh: u64, offset: u64) -> Result<Vec<DirEntry>, i32>;
  /// Create `name` in `parent` and open it; the entry and the handle.
  fn create(
    &mut self,
    parent: u64,
    name: &str,
    mode: u32,
    flags: u32,
  ) -> Result<(EntryOut, u64), i32>;
  /// Release handle `fh` of `nodeid`.
  fn release(&mut self, nodeid: u64, fh: u64) -> Result<(), i32>;
  /// The kernel drops `nlookup` references to `nodeid`.
  fn forget(&mut self, nodeid: u64, nlookup: u64);
  /// Flush handle `fh` of `nodeid` (no disk write; success once the data is in the anchor).
  fn flush(&mut self, nodeid: u64, fh: u64) -> Result<(), i32>;
}

/// Dispatches one parsed message to `bridge`, writing the reply into `out`; returns the bytes
/// written. A parse failure replies `EIO`; an unserved opcode replies `ENOSYS`; a method's
/// `Err(errno)` replies that errno. `INIT` is answered here (it negotiates, it is not a Bridge
/// method). The transport calls this for every message and writes `out[..n]` back to the
/// kernel.
pub fn dispatch(message: &[u8], bridge: &mut dyn Bridge, out: &mut [u8]) -> usize {
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
    Opcode::Lookup => serve_lookup(bridge, &request, out),
    Opcode::GetAttr => serve_getattr(bridge, &request, out),
    Opcode::Open => serve_open(bridge, &request, Opcode::Open, out),
    Opcode::OpenDir => serve_open(bridge, &request, Opcode::OpenDir, out),
    Opcode::Read => serve_read(bridge, &request, out),
    Opcode::Write => serve_write(bridge, &request, out),
    Opcode::ReadDir => serve_readdir(bridge, &request, out),
    Opcode::Create => serve_create(bridge, &request, out),
    Opcode::Release | Opcode::ReleaseDir => serve_release(bridge, &request, out),
    Opcode::Flush => serve_flush(bridge, &request, out),
    Opcode::Forget => serve_forget(bridge, &request),
    // The rest of the Bridge trait is dispatched as the driver grows; until then the kernel
    // is told the operation is not implemented, never left waiting.
    _ => write_or_drop(ReplyHeader::write_error(unique, ENOSYS, out), out),
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

/// Replies with a body the bridge produced, or the errno it refused with.
fn reply<T>(
  unique: u64,
  result: Result<T, i32>,
  encode: impl FnOnce(&T) -> Vec<u8>,
  out: &mut [u8],
) -> usize {
  match result {
    Ok(value) => write_or_drop(ReplyHeader::write_ok(unique, &encode(&value), out), out),
    Err(errno) => write_or_drop(ReplyHeader::write_error(unique, errno, out), out),
  }
}

fn serve_lookup(bridge: &mut dyn Bridge, req: &Request<'_>, out: &mut [u8]) -> usize {
  let Ok(name) = parse_name(req.body) else {
    return write_or_drop(ReplyHeader::write_error(req.header.unique, EIO, out), out);
  };
  reply(
    req.header.unique,
    bridge.lookup(req.header.nodeid, name),
    |e| e.to_bytes(),
    out,
  )
}

fn serve_getattr(bridge: &mut dyn Bridge, req: &Request<'_>, out: &mut [u8]) -> usize {
  let result = bridge.getattr(req.header.nodeid).map(|attr| AttrOut {
    attr_valid: CACHE_FOREVER,
    attr,
  });
  reply(req.header.unique, result, |a| a.to_bytes(), out)
}

fn serve_open(bridge: &mut dyn Bridge, req: &Request<'_>, opcode: Opcode, out: &mut [u8]) -> usize {
  // `fuse_open_in`: flags (4), open_flags (4). slates reads the open flags.
  let flags = req
    .body
    .get(..size_of::<u32>())
    .map(|b| u32::from_le_bytes(b.try_into().unwrap_or_default()))
    .unwrap_or(0);
  let opened = if opcode == Opcode::OpenDir {
    bridge.opendir(req.header.nodeid)
  } else {
    bridge.open(req.header.nodeid, flags)
  };
  reply(
    req.header.unique,
    opened.map(|fh| OpenOut { fh, open_flags: 0 }),
    |o| o.to_bytes(),
    out,
  )
}

fn serve_read(bridge: &mut dyn Bridge, req: &Request<'_>, out: &mut [u8]) -> usize {
  let Ok(r) = ReadIn::parse(Opcode::Read.to_wire(), req.body) else {
    return write_or_drop(ReplyHeader::write_error(req.header.unique, EIO, out), out);
  };
  let mut data = Vec::new();
  match bridge.read(req.header.nodeid, r.fh, r.offset, r.size, &mut data) {
    Ok(()) => write_or_drop(ReplyHeader::write_ok(req.header.unique, &data, out), out),
    Err(errno) => write_or_drop(ReplyHeader::write_error(req.header.unique, errno, out), out),
  }
}

fn serve_write(bridge: &mut dyn Bridge, req: &Request<'_>, out: &mut [u8]) -> usize {
  let Ok(w) = WriteIn::parse(Opcode::Write.to_wire(), req.body) else {
    return write_or_drop(ReplyHeader::write_error(req.header.unique, EIO, out), out);
  };
  reply(
    req.header.unique,
    bridge
      .write(req.header.nodeid, w.fh, w.offset, w.data)
      .map(|size| WriteOut { size }),
    |o| o.to_bytes(),
    out,
  )
}

fn serve_readdir(bridge: &mut dyn Bridge, req: &Request<'_>, out: &mut [u8]) -> usize {
  let Ok(r) = ReadIn::parse(Opcode::ReadDir.to_wire(), req.body) else {
    return write_or_drop(ReplyHeader::write_error(req.header.unique, EIO, out), out);
  };
  match bridge.readdir(req.header.nodeid, r.fh, r.offset) {
    Ok(entries) => {
      let mut dir = DirBuffer::new(usize::try_from(r.size).unwrap_or(0));
      for (index, entry) in entries.iter().enumerate() {
        // The cookie is the one-based index, so the next readdir resumes after this entry.
        let cookie = r.offset.saturating_add(index as u64).saturating_add(1);
        if !dir.push(entry.ino, cookie, entry.kind, &entry.name) {
          break;
        }
      }
      write_or_drop(
        ReplyHeader::write_ok(req.header.unique, dir.as_bytes(), out),
        out,
      )
    }
    Err(errno) => write_or_drop(ReplyHeader::write_error(req.header.unique, errno, out), out),
  }
}

fn serve_create(bridge: &mut dyn Bridge, req: &Request<'_>, out: &mut [u8]) -> usize {
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
  match bridge.create(req.header.nodeid, name, mode, flags) {
    Ok((entry, fh)) => {
      let mut body = entry.to_bytes();
      body.extend_from_slice(&OpenOut { fh, open_flags: 0 }.to_bytes());
      write_or_drop(ReplyHeader::write_ok(req.header.unique, &body, out), out)
    }
    Err(errno) => write_or_drop(ReplyHeader::write_error(req.header.unique, errno, out), out),
  }
}

fn serve_release(bridge: &mut dyn Bridge, req: &Request<'_>, out: &mut [u8]) -> usize {
  // `fuse_release_in`: fh (8), then fields slates does not use.
  let fh = req
    .body
    .get(..size_of::<u64>())
    .map(|b| u64::from_le_bytes(b.try_into().unwrap_or_default()))
    .unwrap_or(0);
  reply(
    req.header.unique,
    bridge.release(req.header.nodeid, fh),
    |()| Vec::new(),
    out,
  )
}

fn serve_flush(bridge: &mut dyn Bridge, req: &Request<'_>, out: &mut [u8]) -> usize {
  let fh = req
    .body
    .get(..size_of::<u64>())
    .map(|b| u64::from_le_bytes(b.try_into().unwrap_or_default()))
    .unwrap_or(0);
  reply(
    req.header.unique,
    bridge.flush(req.header.nodeid, fh),
    |()| Vec::new(),
    out,
  )
}

fn serve_forget(bridge: &mut dyn Bridge, req: &Request<'_>) -> usize {
  // `fuse_forget_in`: nlookup (8). FORGET has no reply.
  let nlookup = req
    .body
    .get(..size_of::<u64>())
    .map(|b| u64::from_le_bytes(b.try_into().unwrap_or_default()))
    .unwrap_or(0);
  bridge.forget(req.header.nodeid, nlookup);
  0
}
