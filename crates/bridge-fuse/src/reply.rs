//! Encoding the daemon's replies (§4.6). Every reply is the fixed `fuse_out_header` (16 bytes:
//! len, error, unique) then an opcode-specific body; an error reply is the header alone with a
//! negative errno and no body. Bodies are appended in field order through [`crate::wire::Writer`],
//! so no byte offset appears as a literal, and the header is written into a caller buffer with a
//! bounds check, so an undersized buffer is a typed refusal, never an overflow.

use crate::abi::OUT_HEADER_LEN;
use crate::error::FuseError;
use crate::wire::Writer;

/// A reply header (`struct fuse_out_header`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplyHeader;

impl ReplyHeader {
  /// Writes an error reply (the header alone, no body) into `out`; returns the bytes written.
  pub fn write_error(unique: u64, errno: i32, out: &mut [u8]) -> Result<usize, FuseError> {
    write_message(unique, errno, &[], out)
  }

  /// Writes a success reply: the header then `body`, with `len` set to the total.
  pub fn write_ok(unique: u64, body: &[u8], out: &mut [u8]) -> Result<usize, FuseError> {
    write_message(unique, 0, body, out)
  }
}

/// The one place a reply header is laid out: len, then the errno (negated for the kernel),
/// then the unique id, then the body.
fn write_message(unique: u64, errno: i32, body: &[u8], out: &mut [u8]) -> Result<usize, FuseError> {
  let total = OUT_HEADER_LEN.saturating_add(body.len());
  if out.len() < total {
    return Err(FuseError::ReplyTooSmall {
      have: out.len(),
      need: total,
    });
  }
  let mut w = Writer::new();
  w.u32(u32::try_from(total).unwrap_or(u32::MAX));
  w.u32(errno.saturating_neg().cast_unsigned());
  w.u64(unique);
  w.bytes(body);
  out[..total].copy_from_slice(w.as_bytes());
  Ok(total)
}

/// An inode's attributes on the wire (`struct fuse_attr`): ino, size, blocks, atime, mtime,
/// ctime, their nsec parts, mode, nlink, uid, gid, rdev, blksize, flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attr {
  /// The inode number.
  pub ino: u64,
  /// The size in bytes.
  pub size: u64,
  /// The size in 512-byte blocks.
  pub blocks: u64,
  /// The modification time (seconds and nanoseconds).
  pub mtime: (u64, u32),
  /// The change time (seconds and nanoseconds).
  pub ctime: (u64, u32),
  /// The access time (seconds and nanoseconds).
  pub atime: (u64, u32),
  /// The mode (type and permission bits).
  pub mode: u32,
  /// The link count.
  pub nlink: u32,
  /// The owner uid.
  pub uid: u32,
  /// The owner gid.
  pub gid: u32,
  /// The preferred block size.
  pub blksize: u32,
}

impl Attr {
  /// Format: the wire size of `fuse_attr`.
  pub const LEN: usize = 88;

  /// Appends the attributes to `w` in `fuse_attr` field order.
  fn write(&self, w: &mut Writer) {
    w.u64(self.ino);
    w.u64(self.size);
    w.u64(self.blocks);
    w.u64(self.atime.0);
    w.u64(self.mtime.0);
    w.u64(self.ctime.0);
    w.u32(self.atime.1);
    w.u32(self.mtime.1);
    w.u32(self.ctime.1);
    w.u32(self.mode);
    w.u32(self.nlink);
    w.u32(self.uid);
    w.u32(self.gid);
    w.u32(0); // rdev
    w.u32(self.blksize);
    w.u32(0); // flags
  }

  /// The attributes as a byte block.
  pub fn to_bytes(&self) -> Vec<u8> {
    let mut w = Writer::new();
    self.write(&mut w);
    w.into_bytes()
  }
}

/// A `LOOKUP`/`CREATE`/`MKDIR` entry reply (`struct fuse_entry_out`): nodeid, generation,
/// entry_valid, attr_valid, their nsec parts, then `fuse_attr`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EntryOut {
  /// The node id (inode).
  pub nodeid: u64,
  /// The generation (so a reused node id is distinguished).
  pub generation: u64,
  /// How long the kernel may cache the name → node mapping (seconds); slates caches forever.
  pub entry_valid: u64,
  /// How long the kernel may cache the attributes (seconds); slates caches forever.
  pub attr_valid: u64,
  /// The attributes.
  pub attr: Attr,
}

impl EntryOut {
  /// Format: the wire size of `fuse_entry_out`.
  pub const LEN: usize = 40 + Attr::LEN;

  /// The entry as a byte block.
  pub fn to_bytes(&self) -> Vec<u8> {
    let mut w = Writer::new();
    w.u64(self.nodeid);
    w.u64(self.generation);
    w.u64(self.entry_valid);
    w.u64(self.attr_valid);
    w.u32(0); // entry_valid_nsec
    w.u32(0); // attr_valid_nsec
    self.attr.write(&mut w);
    w.into_bytes()
  }
}

/// A `GETATTR`/`SETATTR` reply (`struct fuse_attr_out`): attr_valid, its nsec part, a dummy
/// word, then `fuse_attr`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AttrOut {
  /// How long the kernel may cache the attributes (seconds).
  pub attr_valid: u64,
  /// The attributes.
  pub attr: Attr,
}

impl AttrOut {
  /// Format: the wire size of `fuse_attr_out`.
  pub const LEN: usize = 16 + Attr::LEN;

  /// The reply as a byte block.
  pub fn to_bytes(&self) -> Vec<u8> {
    let mut w = Writer::new();
    w.u64(self.attr_valid);
    w.u32(0); // attr_valid_nsec
    w.u32(0); // dummy
    self.attr.write(&mut w);
    w.into_bytes()
  }
}

/// An `OPEN`/`OPENDIR`/`CREATE` open reply (`struct fuse_open_out`): fh, open_flags, padding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpenOut {
  /// The file handle the daemon assigns.
  pub fh: u64,
  /// The open flags (e.g. keep cache).
  pub open_flags: u32,
}

impl OpenOut {
  /// Format: the wire size of `fuse_open_out`.
  pub const LEN: usize = 16;

  /// The reply as a byte block.
  pub fn to_bytes(&self) -> Vec<u8> {
    let mut w = Writer::new();
    w.u64(self.fh);
    w.u32(self.open_flags);
    w.pad(Self::LEN - w.as_bytes().len());
    w.into_bytes()
  }
}

/// A `WRITE` reply (`struct fuse_write_out`): size, padding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WriteOut {
  /// The bytes written.
  pub size: u32,
}

impl WriteOut {
  /// Format: the wire size of `fuse_write_out`.
  pub const LEN: usize = 8;

  /// The reply as a byte block.
  pub fn to_bytes(&self) -> Vec<u8> {
    let mut w = Writer::new();
    w.u32(self.size);
    w.pad(Self::LEN - w.as_bytes().len());
    w.into_bytes()
  }
}

/// Builds a `readdir` reply buffer one entry at a time (`struct fuse_dirent`: ino, off,
/// namelen, type, then the name, each entry padded to an 8-byte boundary). The kernel gave a
/// maximum size in the read request; entries are added until the next would exceed it, so the
/// buffer never overflows.
#[derive(Debug)]
pub struct DirBuffer {
  bytes: Vec<u8>,
  max: usize,
}

impl DirBuffer {
  /// Shape: the fixed part of `fuse_dirent` before the name (ino, off, namelen, type).
  const DIRENT_HEAD: usize = 24;
  /// Format: the alignment each directory entry is padded to.
  const ALIGN: usize = 8;

  /// A buffer bounded by the `size` the read request asked for.
  pub fn new(max: usize) -> DirBuffer {
    DirBuffer {
      bytes: Vec::new(),
      max,
    }
  }

  /// Adds an entry; returns false when it would exceed the request's size (the caller stops
  /// and replies what fits, resuming from `offset` next time).
  pub fn push(&mut self, ino: u64, offset: u64, kind: u32, name: &str) -> bool {
    let padded = (Self::DIRENT_HEAD + name.len()).next_multiple_of(Self::ALIGN);
    if self.bytes.len().saturating_add(padded) > self.max {
      return false;
    }
    let mut w = Writer::new();
    w.u64(ino);
    w.u64(offset);
    w.u32(u32::try_from(name.len()).unwrap_or(u32::MAX));
    w.u32(kind);
    w.bytes(name.as_bytes());
    w.pad(padded - Self::DIRENT_HEAD - name.len());
    self.bytes.extend_from_slice(w.as_bytes());
    true
  }

  /// The accumulated entries.
  pub fn as_bytes(&self) -> &[u8] {
    &self.bytes
  }
}
