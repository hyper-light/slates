//! The macOS FSKit bridge (FSKit-first on macOS 26+, Amendment A-1 / D-O9): the shim wire codec that
//! the Swift `FSVolume` handler forwards its operations over the app-group-shared ring, decoded here
//! and dispatched onto the one [`Bridge`] operation seam (§4.6, D-2; Phase 4). Like the NFS bridge, the
//! **wire codec comes first** — it is pure and directly confirmable on any host with no socket, no
//! mount, and no Swift: a request is a byte message, `serve` decodes it, calls the bridge, and encodes
//! the reply, so the whole read path is testable here before the platform half (the Swift shim, the
//! `Slates.app` bundle, the FSKit entitlement, the mount spike) exists.
//!
//! The wire is deliberately minimal and self-describing: an operation tag then its little-endian
//! fields, every variable field length-prefixed and checked against a cap before anything is allocated,
//! so a hostile message is a typed [`ShimWireError`], never a panic or an over-read (the parser handles
//! bytes that crossed a process boundary from a sandboxed extension). A reply is a status byte then
//! either the result or a [`ShimError`] tag; the Swift shim maps that tag to the `NSError`/POSIX errno
//! FSKit returns, so no errno numbers live in this Rust codec.
//!
//! This first slice carries the identity-addressed read path — `lookup`, `getattr`, `read` — which need
//! no open-handle state (the volume core addresses objects by identity, §4.6). The handle path
//! (`opendir`/`readdir`/`open`/`release`), the write and namespace operations, the ring transport
//! itself, and the Swift shim are the owed continuation.

use slates_bridge_core::{Bridge, NodeAttr, ObjectId, OpContext};
use slates_vfs::error::VfsError;
use slates_vfs::inode::Kind;

/// Format: the operation tag that opens every shim request — one byte, then the operation's fields.
const OP_LOOKUP: u8 = 1;
/// Format: see [`OP_LOOKUP`].
const OP_GETATTR: u8 = 2;
/// Format: see [`OP_LOOKUP`].
const OP_READ: u8 = 3;

/// Format: the reply status byte — the result follows, or a [`ShimError`] tag does.
const STATUS_OK: u8 = 0;
/// Format: see [`STATUS_OK`] — an error reply, whose one following byte is the [`ShimError`] tag.
const STATUS_ERR: u8 = 1;

/// Format: the `Kind` tag in an encoded attribute — a file, a directory, or a symlink.
const KIND_FILE: u8 = 0;
/// Format: see [`KIND_FILE`].
const KIND_DIR: u8 = 1;
/// Format: see [`KIND_FILE`].
const KIND_SYMLINK: u8 = 2;

/// Shape: the largest name a lookup may carry. A path component on Apple's filesystems is at most 255
/// bytes (`NAME_MAX`); the codec refuses a longer one before allocating rather than trust the sender.
const MAX_NAME_BYTES: usize = 255;

/// Shape: the largest single read the shim may request in one message. FSKit issues reads in bounded
/// chunks; a request over this cap is refused so a hostile length cannot force a large allocation. One
/// mebibyte covers a page-cluster read with margin; the real cap is tuned against the ring frame in the
/// spike (owed) and only ever lowered here.
const MAX_READ_BYTES: u32 = 1 << 20;

/// A malformed shim message — a request whose bytes are truncated, over-claiming, or unknown. Every one
/// is typed; the codec never panics or over-reads on input that crossed the extension boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShimWireError {
  /// The message ended before a field it declared could be read.
  Truncated,
  /// The leading operation tag is not one this codec serves.
  UnknownOp {
    /// The tag byte seen.
    tag: u8,
  },
  /// A length-prefixed field declared more bytes than its class allows, or than remain in the message.
  BadLength,
  /// Bytes remained after the message's fields were read — a framing error.
  TrailingBytes,
}

/// The closed set of refusals a shim reply can carry (mirroring the volume core's [`VfsError`]). The
/// Swift shim maps each to the `NSError`/POSIX errno FSKit returns; keeping it a typed tag here means no
/// errno numbers live in the Rust codec. Its wire tag is the variant's `#[repr(u8)]` discriminant (its
/// declaration order): append new variants at the end, never reorder — a golden reply test pins the
/// wire (owed with the write path).
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShimError {
  /// No such object.
  NotFound,
  /// The name already exists.
  AlreadyExists,
  /// A directory was expected.
  NotDirectory,
  /// A directory was not expected.
  IsDirectory,
  /// The directory is not empty.
  NotEmpty,
  /// The request is invalid.
  Invalid,
  /// The operation is not permitted for this context.
  NotPermitted,
  /// The name is not a legal path component.
  InvalidName,
  /// The object's link count is at its maximum.
  TooManyLinks,
  /// The volume is out of space.
  NoSpace,
  /// The file would exceed the maximum size.
  FileTooLarge,
  /// A rename would cross volumes.
  CrossVolumeMove,
  /// The handle is stale.
  StaleHandle,
  /// The volume is being destroyed.
  Destroying,
  /// The view is pinned; a write is refused.
  Pinned,
  /// The overlay's base is unavailable; the read cannot be served.
  BaseUnavailable,
  /// Any refusal without a dedicated tag (a forward-compatible catch-all).
  Other,
}

impl ShimError {
  /// The shim error a volume refusal maps to (§4.6 "refusing with the volume core's own `VfsError`,
  /// which each transport maps to its wire error").
  pub fn from_vfs(error: &VfsError) -> ShimError {
    match error {
      VfsError::NotFound => ShimError::NotFound,
      VfsError::AlreadyExists => ShimError::AlreadyExists,
      VfsError::NotDirectory => ShimError::NotDirectory,
      VfsError::IsDirectory => ShimError::IsDirectory,
      VfsError::NotEmpty => ShimError::NotEmpty,
      VfsError::Invalid => ShimError::Invalid,
      VfsError::NotPermitted => ShimError::NotPermitted,
      VfsError::InvalidName => ShimError::InvalidName,
      VfsError::TooManyLinks => ShimError::TooManyLinks,
      VfsError::NoSpace => ShimError::NoSpace,
      VfsError::FileTooLarge => ShimError::FileTooLarge,
      VfsError::CrossVolumeMove => ShimError::CrossVolumeMove,
      VfsError::StaleHandle => ShimError::StaleHandle,
      VfsError::Destroying => ShimError::Destroying,
      VfsError::Pinned => ShimError::Pinned,
      VfsError::BaseUnavailable(_) => ShimError::BaseUnavailable,
      _ => ShimError::Other,
    }
  }

  /// The one-byte tag this error rides on the wire — the variant's `#[repr(u8)]` discriminant, an exact
  /// widening from the representation, so there is no numeric table to drift from the enum.
  fn tag(self) -> u8 {
    self as u8
  }
}

/// One decoded shim request — an `FSVolume` handler operation the Swift shim forwarded. This slice
/// carries the identity-addressed read path; the handle, write and namespace operations are owed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShimRequest {
  /// Look `name` up in directory `parent`.
  Lookup {
    /// The parent directory.
    parent: ObjectId,
    /// The child's name (at most [`MAX_NAME_BYTES`]).
    name: String,
  },
  /// Read the attributes of `object`.
  GetAttr {
    /// The object.
    object: ObjectId,
  },
  /// Read `size` bytes at `offset` from `object`.
  Read {
    /// The object.
    object: ObjectId,
    /// The byte offset.
    offset: u64,
    /// The byte count (at most [`MAX_READ_BYTES`]).
    size: u32,
  },
}

impl ShimRequest {
  /// The canonical bytes: the operation tag, then the operation's fields, little-endian. An object is
  /// its inode then its generation; a name is a `u32` length then the bytes.
  pub fn encode(&self) -> Vec<u8> {
    let mut out = Vec::new();
    match self {
      ShimRequest::Lookup { parent, name } => {
        out.push(OP_LOOKUP);
        put_object(&mut out, *parent);
        put_bytes(&mut out, name.as_bytes());
      }
      ShimRequest::GetAttr { object } => {
        out.push(OP_GETATTR);
        put_object(&mut out, *object);
      }
      ShimRequest::Read {
        object,
        offset,
        size,
      } => {
        out.push(OP_READ);
        put_object(&mut out, *object);
        out.extend_from_slice(&offset.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
      }
    }
    out
  }

  /// Decodes a request, refusing a truncated, over-claiming, unknown or trailing-byte message with a
  /// typed [`ShimWireError`] — never a panic or an over-read.
  pub fn decode(bytes: &[u8]) -> Result<ShimRequest, ShimWireError> {
    let (&tag, mut rest) = bytes.split_first().ok_or(ShimWireError::Truncated)?;
    let request = match tag {
      OP_LOOKUP => {
        let parent = take_object(&mut rest)?;
        let name_bytes = take_bytes(&mut rest, MAX_NAME_BYTES)?;
        let name = String::from_utf8(name_bytes.to_vec()).map_err(|_| ShimWireError::BadLength)?;
        ShimRequest::Lookup { parent, name }
      }
      OP_GETATTR => {
        let object = take_object(&mut rest)?;
        ShimRequest::GetAttr { object }
      }
      OP_READ => {
        let object = take_object(&mut rest)?;
        let offset = take_u64(&mut rest)?;
        let size = take_u32(&mut rest)?;
        if size > MAX_READ_BYTES {
          return Err(ShimWireError::BadLength);
        }
        ShimRequest::Read {
          object,
          offset,
          size,
        }
      }
      other => return Err(ShimWireError::UnknownOp { tag: other }),
    };
    if rest.is_empty() {
      Ok(request)
    } else {
      Err(ShimWireError::TrailingBytes)
    }
  }
}

/// Decodes `request_bytes`, dispatches the operation onto `bridge` under `cx`, and encodes the reply
/// (§4.6 — the one operation seam). A well-formed request that the bridge refuses becomes an error
/// reply carrying the mapped [`ShimError`]; a malformed request is refused before the bridge is touched,
/// returned as a wire error the caller reports rather than answering. This is the whole read path,
/// exercised with no ring and no mount.
pub fn serve(
  request_bytes: &[u8],
  bridge: &mut dyn Bridge,
  cx: &OpContext,
) -> Result<Vec<u8>, ShimWireError> {
  let request = ShimRequest::decode(request_bytes)?;
  let reply = match request {
    ShimRequest::Lookup { parent, name } => match bridge.lookup(parent, cx, &name) {
      Ok(attr) => ok_attr(&attr),
      Err(error) => err_reply(&error),
    },
    ShimRequest::GetAttr { object } => match bridge.getattr(object, cx) {
      Ok(attr) => ok_attr(&attr),
      Err(error) => err_reply(&error),
    },
    ShimRequest::Read {
      object,
      offset,
      size,
    } => {
      let mut out = Vec::new();
      match bridge.read(object, cx, offset, size, &mut out) {
        Ok(()) => ok_bytes(&out),
        Err(error) => err_reply(&error),
      }
    }
  };
  Ok(reply)
}

/// An OK reply carrying an encoded attribute.
fn ok_attr(attr: &NodeAttr) -> Vec<u8> {
  let mut out = vec![STATUS_OK];
  put_attr(&mut out, attr);
  out
}

/// An OK reply carrying read bytes (a `u32` length then the bytes).
fn ok_bytes(bytes: &[u8]) -> Vec<u8> {
  let mut out = vec![STATUS_OK];
  put_bytes(&mut out, bytes);
  out
}

/// An error reply carrying the mapped [`ShimError`] tag.
fn err_reply(error: &VfsError) -> Vec<u8> {
  vec![STATUS_ERR, ShimError::from_vfs(error).tag()]
}

/// Appends an object id: its inode then its generation, little-endian.
fn put_object(out: &mut Vec<u8>, object: ObjectId) {
  out.extend_from_slice(&object.inode.to_le_bytes());
  out.extend_from_slice(&object.generation.to_le_bytes());
}

/// Appends a length-prefixed byte field: a `u32` length then the bytes.
fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
  let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
  out.extend_from_slice(&len.to_le_bytes());
  out.extend_from_slice(bytes);
}

/// Appends an encoded attribute (the fields the read path needs), each little-endian; the kind is a tag.
fn put_attr(out: &mut Vec<u8>, attr: &NodeAttr) {
  out.extend_from_slice(&attr.ino.to_le_bytes());
  out.extend_from_slice(&attr.generation.to_le_bytes());
  out.push(kind_tag(attr.kind));
  out.extend_from_slice(&attr.mode.to_le_bytes());
  out.extend_from_slice(&attr.nlink.to_le_bytes());
  out.extend_from_slice(&attr.uid.to_le_bytes());
  out.extend_from_slice(&attr.gid.to_le_bytes());
  out.extend_from_slice(&attr.size.to_le_bytes());
  out.extend_from_slice(&attr.atime.to_le_bytes());
  out.extend_from_slice(&attr.mtime.to_le_bytes());
  out.extend_from_slice(&attr.ctime.to_le_bytes());
}

/// The wire tag for a node kind.
fn kind_tag(kind: Kind) -> u8 {
  match kind {
    Kind::File => KIND_FILE,
    Kind::Dir => KIND_DIR,
    Kind::Symlink => KIND_SYMLINK,
  }
}

/// Reads an object id (inode then generation) from the front of `rest`, advancing it.
fn take_object(rest: &mut &[u8]) -> Result<ObjectId, ShimWireError> {
  let inode = take_u64(rest)?;
  let generation = take_u64(rest)?;
  Ok(ObjectId::new(inode, generation))
}

/// Reads a little-endian `u64` from the front of `rest`, advancing it, or `Truncated`.
fn take_u64(rest: &mut &[u8]) -> Result<u64, ShimWireError> {
  let (head, tail) = split_at_checked(rest, size_of::<u64>())?;
  *rest = tail;
  let mut word = [0u8; size_of::<u64>()];
  word.copy_from_slice(head);
  Ok(u64::from_le_bytes(word))
}

/// Reads a little-endian `u32` from the front of `rest`, advancing it, or `Truncated`.
fn take_u32(rest: &mut &[u8]) -> Result<u32, ShimWireError> {
  let (head, tail) = split_at_checked(rest, size_of::<u32>())?;
  *rest = tail;
  let mut word = [0u8; size_of::<u32>()];
  word.copy_from_slice(head);
  Ok(u32::from_le_bytes(word))
}

/// Reads a length-prefixed byte field (`u32` length then the bytes) from the front of `rest`, refusing
/// a length past `cap` or past what remains — before allocating — as `BadLength`.
fn take_bytes<'a>(rest: &mut &'a [u8], cap: usize) -> Result<&'a [u8], ShimWireError> {
  let len = take_u32(rest)?;
  let len = usize::try_from(len).map_err(|_| ShimWireError::BadLength)?;
  if len > cap {
    return Err(ShimWireError::BadLength);
  }
  let (head, tail) = split_at_checked(rest, len)?;
  *rest = tail;
  Ok(head)
}

/// Splits `bytes` at `at`, returning `Truncated` when fewer than `at` bytes remain (never a panic).
fn split_at_checked(bytes: &[u8], at: usize) -> Result<(&[u8], &[u8]), ShimWireError> {
  if bytes.len() < at {
    return Err(ShimWireError::Truncated);
  }
  Ok(bytes.split_at(at))
}
