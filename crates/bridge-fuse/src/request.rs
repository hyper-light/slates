//! Parsing the kernel's requests (§4.6, §4.9's hostile-input rule). Every request is the fixed
//! `fuse_in_header` (40 bytes) then an opcode-specific body; the header's `len` bounds the
//! whole message. Fields are read in order through the bounds-checked [`crate::wire::Reader`],
//! so a truncated or oversized message is a typed refusal, never a panic or an out-of-bounds
//! read; no byte offset appears as a literal.

use crate::abi::{IN_HEADER_LEN, Opcode};
use crate::error::FuseError;
use crate::wire::Reader;

/// The fixed FUSE request header (`struct fuse_in_header`): len, opcode, unique, nodeid, uid,
/// gid, pid, then a padding word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InHeader {
  /// The whole message's length, header included.
  pub len: u32,
  /// The opcode's wire value.
  pub opcode: u32,
  /// The request's unique id; the reply echoes it.
  pub unique: u64,
  /// The inode the request is about (the FUSE node id).
  pub nodeid: u64,
  /// The caller's user id.
  pub uid: u32,
  /// The caller's group id.
  pub gid: u32,
  /// The caller's process id.
  pub pid: u32,
}

impl InHeader {
  /// Parses the header from the front of a message; refuses a message shorter than the header
  /// or whose `len` is smaller than the header or larger than the buffer.
  pub fn parse(message: &[u8]) -> Result<InHeader, FuseError> {
    if message.len() < IN_HEADER_LEN {
      return Err(FuseError::ShortHeader {
        have: message.len(),
        need: IN_HEADER_LEN,
      });
    }
    let mut r = Reader::new(message);
    // The header can never be short here (checked above), so the opcode passed to the reader
    // for its refusals is only cosmetic; use zero.
    let len = r.u32(0)?;
    let opcode = r.u32(0)?;
    let unique = r.u64(0)?;
    let nodeid = r.u64(0)?;
    let uid = r.u32(0)?;
    let gid = r.u32(0)?;
    let pid = r.u32(0)?;
    let claimed = usize::try_from(len).unwrap_or(usize::MAX);
    if claimed < IN_HEADER_LEN || claimed > message.len() {
      return Err(FuseError::BadLength {
        claimed: len,
        have: message.len(),
      });
    }
    Ok(InHeader {
      len,
      opcode,
      unique,
      nodeid,
      uid,
      gid,
      pid,
    })
  }
}

/// A parsed request: the header, the opcode (when slates serves it), and the body bytes.
#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
  /// The header.
  pub header: InHeader,
  /// The opcode, or `None` when slates does not serve it (reply `ENOSYS`).
  pub opcode: Option<Opcode>,
  /// The opcode-specific body.
  pub body: &'a [u8],
}

impl<'a> Request<'a> {
  /// Parses a whole message; the body is the bytes between the header and the header's `len`,
  /// so trailing bytes in an over-read buffer are ignored.
  pub fn parse(message: &'a [u8]) -> Result<Request<'a>, FuseError> {
    let header = InHeader::parse(message)?;
    let end = usize::try_from(header.len).unwrap_or(message.len());
    let body = &message[IN_HEADER_LEN..end];
    Ok(Request {
      header,
      opcode: Opcode::from_wire(header.opcode),
      body,
    })
  }
}

/// A `LOOKUP`, `UNLINK`, `RMDIR` body: a single NUL-terminated name.
pub fn parse_name(body: &[u8]) -> Result<&str, FuseError> {
  let end = body
    .iter()
    .position(|b| *b == 0)
    .ok_or(FuseError::UnterminatedName)?;
  std::str::from_utf8(&body[..end]).map_err(|_| FuseError::UnterminatedName)
}

/// A `READ`/`READDIR` body (`struct fuse_read_in`): fh, offset, size, then fields slates does
/// not use. The fields slates reads are fh, offset, size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadIn {
  /// The open file handle.
  pub fh: u64,
  /// The offset to read from.
  pub offset: u64,
  /// The bytes to read.
  pub size: u32,
}

impl ReadIn {
  /// Parses a read body; refuses a short one.
  pub fn parse(opcode: u32, body: &[u8]) -> Result<ReadIn, FuseError> {
    let mut r = Reader::new(body);
    Ok(ReadIn {
      fh: r.u64(opcode)?,
      offset: r.u64(opcode)?,
      size: r.u32(opcode)?,
    })
  }
}

/// A `WRITE` body (`struct fuse_write_in` then the data): fh, offset, size, write_flags, then
/// fields slates does not use, then `size` bytes of data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriteIn<'a> {
  /// The open file handle.
  pub fh: u64,
  /// The offset to write at.
  pub offset: u64,
  /// The data.
  pub data: &'a [u8],
}

impl<'a> WriteIn<'a> {
  /// Shape: the fields of `fuse_write_in` after `size` that slates skips (write_flags,
  /// lock_owner, flags, padding: five 32-bit words).
  const SKIP_AFTER_SIZE: usize = 20;

  /// Parses a write body; refuses one shorter than the header plus the declared data size.
  pub fn parse(opcode: u32, body: &'a [u8]) -> Result<WriteIn<'a>, FuseError> {
    let mut r = Reader::new(body);
    let fh = r.u64(opcode)?;
    let offset = r.u64(opcode)?;
    let size = usize::try_from(r.u32(opcode)?).unwrap_or(usize::MAX);
    r.skip(Self::SKIP_AFTER_SIZE, opcode)?;
    if r.remaining() < size {
      return Err(FuseError::ShortBody {
        opcode,
        have: body.len(),
        need: body
          .len()
          .saturating_add(size.saturating_sub(r.remaining())),
      });
    }
    Ok(WriteIn {
      fh,
      offset,
      data: &r.rest()[..size],
    })
  }
}
