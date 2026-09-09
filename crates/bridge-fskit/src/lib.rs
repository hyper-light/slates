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
//! The codec covers the **whole `Bridge` operation set** — the read path (`lookup`, `getattr`, `read`),
//! the write path (`write`), files and handles (`open`, `flush`), directory enumeration (`opendir`,
//! `readdir`, `release`), the namespace (`create`, `mkdir`, `unlink`, `rmdir`, `rename`), links
//! (`symlink`, `readlink`, `link`) and the transport-lifetime references (`reference`, `forget`) — with
//! a golden vector pinning the wire. What is owed is the **platform half**: the app-group ring transport
//! (this crate is the message codec, not the ring), and the Swift `FSVolume` shim (with the
//! `Slates.app` bundle, the FSKit entitlement, and the mount spike).

// The daemon's per-mount serve session (§4.6): holds the open-handle map across requests and serves
// shim requests against a shard's volume through a transient bridge.
pub mod mount;

// The in-process transport harness (a C ABI over `serve` for the Swift handler's end-to-end test),
// built only under the `test-harness` feature; the shipped crate does not include it.
#[cfg(feature = "test-harness")]
pub mod ffi;

use slates_bridge_core::{Bridge, DirEntry, NodeAttr, ObjectId, OpContext, RenameFlags, SetAttr};
use slates_vfs::error::VfsError;
use slates_vfs::inode::Kind;

/// Format: the operation tag that opens every shim request — one byte, then the operation's fields.
const OP_LOOKUP: u8 = 1;
/// Format: see [`OP_LOOKUP`].
const OP_GETATTR: u8 = 2;
/// Format: see [`OP_LOOKUP`].
const OP_READ: u8 = 3;
/// Format: see [`OP_LOOKUP`].
const OP_WRITE: u8 = 4;
/// Format: see [`OP_LOOKUP`].
const OP_OPENDIR: u8 = 5;
/// Format: see [`OP_LOOKUP`].
const OP_READDIR: u8 = 6;
/// Format: see [`OP_LOOKUP`].
const OP_RELEASE: u8 = 7;
/// Format: see [`OP_LOOKUP`].
const OP_CREATE: u8 = 8;
/// Format: see [`OP_LOOKUP`].
const OP_MKDIR: u8 = 9;
/// Format: see [`OP_LOOKUP`].
const OP_UNLINK: u8 = 10;
/// Format: see [`OP_LOOKUP`].
const OP_RMDIR: u8 = 11;
/// Format: see [`OP_LOOKUP`].
const OP_OPEN: u8 = 12;
/// Format: see [`OP_LOOKUP`].
const OP_FLUSH: u8 = 13;
/// Format: see [`OP_LOOKUP`].
const OP_SYMLINK: u8 = 14;
/// Format: see [`OP_LOOKUP`].
const OP_READLINK: u8 = 15;
/// Format: see [`OP_LOOKUP`].
const OP_LINK: u8 = 16;
/// Format: see [`OP_LOOKUP`].
const OP_RENAME: u8 = 17;
/// Format: see [`OP_LOOKUP`].
const OP_REFERENCE: u8 = 18;
/// Format: see [`OP_LOOKUP`].
const OP_FORGET: u8 = 19;
/// Format: see [`OP_LOOKUP`] — set attributes on an object (chmod/chown/truncate/utimes); the object
/// is followed by the `SETATTR_*` field mask and the present values in field order.
const OP_SETATTR: u8 = 20;
/// Format: see [`OP_LOOKUP`] — return the volume's root object. Carries no fields; the reply is the
/// root's attribute record, so the handler learns the real root object id (`compose(prefix, 1)`, not a
/// constant) at activate time rather than assuming inode 1.
const OP_ROOT: u8 = 21;

/// Format: the `SetAttr` field-present bits — a little-endian `u32` mask that follows the object; a set
/// bit means that field's value follows, in the order size, mode, uid, gid, atime, mtime. Mirrors the
/// Swift shifts and the `SetAttr` optionals in `bridge-core`. This bit: a new size (a truncate).
const SETATTR_SIZE: u32 = 1 << 0;
/// Format: see [`SETATTR_SIZE`] — new permission bits.
const SETATTR_MODE: u32 = 1 << 1;
/// Format: see [`SETATTR_SIZE`] — a new owner uid.
const SETATTR_UID: u32 = 1 << 2;
/// Format: see [`SETATTR_SIZE`] — a new owner gid.
const SETATTR_GID: u32 = 1 << 3;
/// Format: see [`SETATTR_SIZE`] — a new access time (Unix nanoseconds).
const SETATTR_ATIME: u32 = 1 << 4;
/// Format: see [`SETATTR_SIZE`] — a new modification time (Unix nanoseconds).
const SETATTR_MTIME: u32 = 1 << 5;

/// Format: the reply status byte — the result follows, or a [`ShimError`] tag does.
const STATUS_OK: u8 = 0;
/// Format: see [`STATUS_OK`] — an error reply, whose one following byte is the [`ShimError`] tag.
const STATUS_ERR: u8 = 1;

/// Format: the rename flag bit that refuses replacing an existing destination (`RENAME_NOREPLACE`).
const RENAME_NO_REPLACE: u8 = 1;
/// Format: the rename flag bit that atomically exchanges the two names (`RENAME_EXCHANGE`).
const RENAME_EXCHANGE: u8 = 1 << 1;

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

/// Shape: the largest single write the shim may carry in one message — the same page-cluster bound as
/// [`MAX_READ_BYTES`], checked before the data is read so a hostile length allocates nothing.
const MAX_WRITE_BYTES: usize = 1 << 20;

/// Shape: the largest symlink target (and returned link) the shim may carry — one page, comfortably
/// above Apple's `PATH_MAX` of 1024; a longer one is refused before allocating.
const MAX_TARGET_BYTES: usize = 4096;

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
  /// Write `data` at `offset` to `object`.
  Write {
    /// The object.
    object: ObjectId,
    /// The byte offset.
    offset: u64,
    /// The bytes to write (at most [`MAX_WRITE_BYTES`]).
    data: Vec<u8>,
  },
  /// Open directory `object` for enumeration; the reply is a handle.
  OpenDir {
    /// The directory.
    object: ObjectId,
  },
  /// Read the entries of directory `object` under handle `fh` from `offset` (the resume cookie).
  ReadDir {
    /// The directory.
    object: ObjectId,
    /// The handle a prior [`OpenDir`](ShimRequest::OpenDir) returned.
    fh: u64,
    /// The resume cookie (zero from the start).
    offset: u64,
  },
  /// Release the directory handle `fh` on `object`.
  Release {
    /// The object.
    object: ObjectId,
    /// The handle to release.
    fh: u64,
  },
  /// Create and open file `name` in `parent`; the reply is the new attributes and a handle.
  Create {
    /// The parent directory.
    parent: ObjectId,
    /// The new name (at most [`MAX_NAME_BYTES`]).
    name: String,
    /// The permission bits.
    mode: u32,
    /// The open flags.
    flags: u32,
  },
  /// Create directory `name` in `parent`; the reply is the new attributes.
  Mkdir {
    /// The parent directory.
    parent: ObjectId,
    /// The new name (at most [`MAX_NAME_BYTES`]).
    name: String,
    /// The permission bits.
    mode: u32,
  },
  /// Remove file `name` from `parent`.
  Unlink {
    /// The parent directory.
    parent: ObjectId,
    /// The name to remove (at most [`MAX_NAME_BYTES`]).
    name: String,
  },
  /// Remove directory `name` from `parent`.
  Rmdir {
    /// The parent directory.
    parent: ObjectId,
    /// The name to remove (at most [`MAX_NAME_BYTES`]).
    name: String,
  },
  /// Open file `object`; the reply is a handle.
  Open {
    /// The object.
    object: ObjectId,
    /// The open flags.
    flags: u32,
  },
  /// Flush the handle `fh` on `object`.
  Flush {
    /// The object.
    object: ObjectId,
    /// The handle.
    fh: u64,
  },
  /// Create symlink `name` in `parent` pointing at `target`; the reply is the new attributes.
  Symlink {
    /// The parent directory.
    parent: ObjectId,
    /// The link's name (at most [`MAX_NAME_BYTES`]).
    name: String,
    /// The link target (at most [`MAX_TARGET_BYTES`]).
    target: String,
  },
  /// Read the target of symlink `object`; the reply is the target path.
  Readlink {
    /// The symlink.
    object: ObjectId,
  },
  /// Hard-link `target` into `new_parent` as `new_name`; the reply is the attributes.
  Link {
    /// The object to link to.
    target: ObjectId,
    /// The directory the new name goes in.
    new_parent: ObjectId,
    /// The new name (at most [`MAX_NAME_BYTES`]).
    new_name: String,
  },
  /// Rename `old_name` under `old_parent` to `new_name` under `new_parent`, honoring the flags.
  Rename {
    /// The source parent.
    old_parent: ObjectId,
    /// The destination parent.
    new_parent: ObjectId,
    /// The source name (at most [`MAX_NAME_BYTES`]).
    old_name: String,
    /// The destination name (at most [`MAX_NAME_BYTES`]).
    new_name: String,
    /// Whether to refuse replacing an existing destination.
    no_replace: bool,
    /// Whether to atomically exchange the two names.
    exchange: bool,
  },
  /// Take one lookup reference on `object` (the shim is handed an object to address later).
  Reference {
    /// The object.
    object: ObjectId,
  },
  /// Drop `nlookup` lookup references on `object` (the shim forgot it that many times).
  Forget {
    /// The object.
    object: ObjectId,
    /// How many references to drop.
    nlookup: u64,
  },
  /// Set attributes on `object` (chmod/chown/truncate/utimes); only the `Some` fields change and the
  /// reply is the object's new attributes. Mirrors `bridge-core`'s [`SetAttr`], which fills the unset
  /// half of a uid/gid or atime/mtime pair from the current value so a partial set never clobbers.
  SetAttr {
    /// The object.
    object: ObjectId,
    /// A new size (a truncate), if set.
    size: Option<u64>,
    /// New permission bits, if set.
    mode: Option<u32>,
    /// A new owner uid, if set.
    uid: Option<u32>,
    /// A new owner gid, if set.
    gid: Option<u32>,
    /// A new access time (Unix nanoseconds), if set.
    atime: Option<i64>,
    /// A new modification time (Unix nanoseconds), if set.
    mtime: Option<i64>,
  },
  /// Return the volume's root object; the reply is the root's attributes. The handler calls this at
  /// activate time instead of assuming a fixed root inode — the daemon's root is `compose(prefix, 1)`
  /// (per-volume prefixed, and a clone inherits its origin's), never a constant.
  Root,
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
      ShimRequest::Write {
        object,
        offset,
        data,
      } => {
        out.push(OP_WRITE);
        put_object(&mut out, *object);
        out.extend_from_slice(&offset.to_le_bytes());
        put_bytes(&mut out, data);
      }
      ShimRequest::OpenDir { object } => {
        out.push(OP_OPENDIR);
        put_object(&mut out, *object);
      }
      ShimRequest::ReadDir { object, fh, offset } => {
        out.push(OP_READDIR);
        put_object(&mut out, *object);
        out.extend_from_slice(&fh.to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
      }
      ShimRequest::Release { object, fh } => {
        out.push(OP_RELEASE);
        put_object(&mut out, *object);
        out.extend_from_slice(&fh.to_le_bytes());
      }
      ShimRequest::Create {
        parent,
        name,
        mode,
        flags,
      } => {
        out.push(OP_CREATE);
        put_object(&mut out, *parent);
        put_bytes(&mut out, name.as_bytes());
        out.extend_from_slice(&mode.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
      }
      ShimRequest::Mkdir { parent, name, mode } => {
        out.push(OP_MKDIR);
        put_object(&mut out, *parent);
        put_bytes(&mut out, name.as_bytes());
        out.extend_from_slice(&mode.to_le_bytes());
      }
      ShimRequest::Unlink { parent, name } => {
        out.push(OP_UNLINK);
        put_object(&mut out, *parent);
        put_bytes(&mut out, name.as_bytes());
      }
      ShimRequest::Rmdir { parent, name } => {
        out.push(OP_RMDIR);
        put_object(&mut out, *parent);
        put_bytes(&mut out, name.as_bytes());
      }
      ShimRequest::Open { object, flags } => {
        out.push(OP_OPEN);
        put_object(&mut out, *object);
        out.extend_from_slice(&flags.to_le_bytes());
      }
      ShimRequest::Flush { object, fh } => {
        out.push(OP_FLUSH);
        put_object(&mut out, *object);
        out.extend_from_slice(&fh.to_le_bytes());
      }
      ShimRequest::Symlink {
        parent,
        name,
        target,
      } => {
        out.push(OP_SYMLINK);
        put_object(&mut out, *parent);
        put_bytes(&mut out, name.as_bytes());
        put_bytes(&mut out, target.as_bytes());
      }
      ShimRequest::Readlink { object } => {
        out.push(OP_READLINK);
        put_object(&mut out, *object);
      }
      ShimRequest::Link {
        target,
        new_parent,
        new_name,
      } => {
        out.push(OP_LINK);
        put_object(&mut out, *target);
        put_object(&mut out, *new_parent);
        put_bytes(&mut out, new_name.as_bytes());
      }
      ShimRequest::Rename {
        old_parent,
        new_parent,
        old_name,
        new_name,
        no_replace,
        exchange,
      } => {
        out.push(OP_RENAME);
        put_object(&mut out, *old_parent);
        put_object(&mut out, *new_parent);
        put_bytes(&mut out, old_name.as_bytes());
        put_bytes(&mut out, new_name.as_bytes());
        out.push(rename_flags_byte(*no_replace, *exchange));
      }
      ShimRequest::Reference { object } => {
        out.push(OP_REFERENCE);
        put_object(&mut out, *object);
      }
      ShimRequest::Forget { object, nlookup } => {
        out.push(OP_FORGET);
        put_object(&mut out, *object);
        out.extend_from_slice(&nlookup.to_le_bytes());
      }
      ShimRequest::SetAttr {
        object,
        size,
        mode,
        uid,
        gid,
        atime,
        mtime,
      } => put_setattr(
        &mut out,
        *object,
        SetAttr {
          size: *size,
          mode: *mode,
          uid: *uid,
          gid: *gid,
          atime: *atime,
          mtime: *mtime,
        },
      ),
      ShimRequest::Root => out.push(OP_ROOT),
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
        let name = take_name(&mut rest)?;
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
      OP_WRITE => {
        let object = take_object(&mut rest)?;
        let offset = take_u64(&mut rest)?;
        let data = take_bytes(&mut rest, MAX_WRITE_BYTES)?.to_vec();
        ShimRequest::Write {
          object,
          offset,
          data,
        }
      }
      OP_OPENDIR => {
        let object = take_object(&mut rest)?;
        ShimRequest::OpenDir { object }
      }
      OP_READDIR => {
        let object = take_object(&mut rest)?;
        let fh = take_u64(&mut rest)?;
        let offset = take_u64(&mut rest)?;
        ShimRequest::ReadDir { object, fh, offset }
      }
      OP_RELEASE => {
        let object = take_object(&mut rest)?;
        let fh = take_u64(&mut rest)?;
        ShimRequest::Release { object, fh }
      }
      OP_CREATE => {
        let parent = take_object(&mut rest)?;
        let name = take_name(&mut rest)?;
        let mode = take_u32(&mut rest)?;
        let flags = take_u32(&mut rest)?;
        ShimRequest::Create {
          parent,
          name,
          mode,
          flags,
        }
      }
      OP_MKDIR => {
        let parent = take_object(&mut rest)?;
        let name = take_name(&mut rest)?;
        let mode = take_u32(&mut rest)?;
        ShimRequest::Mkdir { parent, name, mode }
      }
      OP_UNLINK => {
        let parent = take_object(&mut rest)?;
        let name = take_name(&mut rest)?;
        ShimRequest::Unlink { parent, name }
      }
      OP_RMDIR => {
        let parent = take_object(&mut rest)?;
        let name = take_name(&mut rest)?;
        ShimRequest::Rmdir { parent, name }
      }
      OP_OPEN => {
        let object = take_object(&mut rest)?;
        let flags = take_u32(&mut rest)?;
        ShimRequest::Open { object, flags }
      }
      OP_FLUSH => {
        let object = take_object(&mut rest)?;
        let fh = take_u64(&mut rest)?;
        ShimRequest::Flush { object, fh }
      }
      OP_SYMLINK => {
        let parent = take_object(&mut rest)?;
        let name = take_name(&mut rest)?;
        let target = take_string(&mut rest, MAX_TARGET_BYTES)?;
        ShimRequest::Symlink {
          parent,
          name,
          target,
        }
      }
      OP_READLINK => {
        let object = take_object(&mut rest)?;
        ShimRequest::Readlink { object }
      }
      OP_LINK => {
        let target = take_object(&mut rest)?;
        let new_parent = take_object(&mut rest)?;
        let new_name = take_name(&mut rest)?;
        ShimRequest::Link {
          target,
          new_parent,
          new_name,
        }
      }
      OP_RENAME => {
        let old_parent = take_object(&mut rest)?;
        let new_parent = take_object(&mut rest)?;
        let old_name = take_name(&mut rest)?;
        let new_name = take_name(&mut rest)?;
        let flags = take_flags(&mut rest)?;
        ShimRequest::Rename {
          old_parent,
          new_parent,
          old_name,
          new_name,
          no_replace: flags & RENAME_NO_REPLACE != 0,
          exchange: flags & RENAME_EXCHANGE != 0,
        }
      }
      OP_REFERENCE => {
        let object = take_object(&mut rest)?;
        ShimRequest::Reference { object }
      }
      OP_FORGET => {
        let object = take_object(&mut rest)?;
        let nlookup = take_u64(&mut rest)?;
        ShimRequest::Forget { object, nlookup }
      }
      OP_SETATTR => take_setattr(&mut rest)?,
      OP_ROOT => ShimRequest::Root,
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
  // Each arm calls one bridge method and turns its `Result` into a reply through `reply`, which folds
  // the Ok/Err branch so the dispatch reads as one line per operation (and stays under the cognitive
  // budget as operations are added).
  let reply = match request {
    ShimRequest::Lookup { parent, name } => {
      reply(bridge.lookup(parent, cx, &name), |a| ok_attr(&a))
    }
    ShimRequest::GetAttr { object } => reply(bridge.getattr(object, cx), |a| ok_attr(&a)),
    ShimRequest::Read {
      object,
      offset,
      size,
    } => {
      let mut out = Vec::new();
      reply(bridge.read(object, cx, offset, size, &mut out), |()| {
        ok_bytes(&out)
      })
    }
    ShimRequest::Write {
      object,
      offset,
      data,
    } => reply(bridge.write(object, cx, offset, &data), ok_count),
    ShimRequest::OpenDir { object } => reply(bridge.opendir(object, cx), ok_fh),
    ShimRequest::ReadDir { object, fh, offset } => {
      reply(bridge.readdir(object, cx, fh, offset), |e| ok_entries(&e))
    }
    ShimRequest::Release { object, fh } => reply(bridge.release(object, cx, fh), |()| ok_unit()),
    ShimRequest::Create {
      parent,
      name,
      mode,
      flags,
    } => reply(bridge.create(parent, cx, &name, mode, flags), |(a, fh)| {
      ok_attr_fh(&a, fh)
    }),
    ShimRequest::Mkdir { parent, name, mode } => {
      reply(bridge.mkdir(parent, cx, &name, mode), |a| ok_attr(&a))
    }
    ShimRequest::Unlink { parent, name } => reply(bridge.unlink(parent, cx, &name), |()| ok_unit()),
    ShimRequest::Rmdir { parent, name } => reply(bridge.rmdir(parent, cx, &name), |()| ok_unit()),
    ShimRequest::Root => reply(root_attr(bridge, cx), |a| ok_attr(&a)),
    other => serve_rest(other, bridge, cx),
  };
  Ok(reply)
}

/// The root object's attributes: the daemon's root inode (`compose(prefix, 1)`, never a constant) with
/// its attribute record. The handler calls [`OP_ROOT`] at activate time to learn the real root object
/// id rather than assuming inode 1. The shim object generation is a stable 0 (inode numbers are never
/// reused, D-4), so the root object is `(root inode, 0)`.
fn root_attr(bridge: &mut dyn Bridge, cx: &OpContext) -> Result<NodeAttr, VfsError> {
  let root = bridge.root(cx)?;
  bridge.getattr(ObjectId::new(root, 0), cx)
}

/// The remaining operations, split from [`serve`]'s dispatch so neither match exceeds the cognitive
/// budget as the operation set grows. `serve` hands every request it did not handle here; the final
/// arm is unreachable by that construction (a defensive refusal, never a panic).
fn serve_rest(request: ShimRequest, bridge: &mut dyn Bridge, cx: &OpContext) -> Vec<u8> {
  match request {
    ShimRequest::Open { object, flags } => reply(bridge.open(object, cx, flags), ok_fh),
    ShimRequest::Flush { object, fh } => reply(bridge.flush(object, cx, fh), |()| ok_unit()),
    ShimRequest::Symlink {
      parent,
      name,
      target,
    } => reply(bridge.symlink(parent, cx, &name, &target), |a| ok_attr(&a)),
    ShimRequest::Readlink { object } => reply(bridge.readlink(object, cx), |s| ok_str(&s)),
    ShimRequest::Link {
      target,
      new_parent,
      new_name,
    } => reply(bridge.link(target, new_parent, cx, &new_name), |a| {
      ok_attr(&a)
    }),
    ShimRequest::Rename {
      old_parent,
      new_parent,
      old_name,
      new_name,
      no_replace,
      exchange,
    } => {
      let flags = RenameFlags {
        no_replace,
        exchange,
      };
      reply(
        bridge.rename(old_parent, new_parent, cx, &old_name, &new_name, flags),
        |()| ok_unit(),
      )
    }
    ShimRequest::Reference { object } => reply(bridge.reference(object, cx), |()| ok_unit()),
    ShimRequest::Forget { object, nlookup } => {
      // `forget` is fire-and-forget (no result); the reply is a bare ack the shim may ignore.
      bridge.forget(object, cx, nlookup);
      ok_unit()
    }
    ShimRequest::SetAttr {
      object,
      size,
      mode,
      uid,
      gid,
      atime,
      mtime,
    } => {
      let changes = SetAttr {
        size,
        mode,
        uid,
        gid,
        atime,
        mtime,
      };
      reply(bridge.setattr(object, cx, changes), |a| ok_attr(&a))
    }
    _ => err_reply(&VfsError::Invalid),
  }
}

/// Folds a bridge result into a reply: the `ok` closure encodes the success payload, a refusal becomes
/// the mapped [`ShimError`] reply. This keeps every `serve` arm a single expression.
fn reply<T>(result: Result<T, VfsError>, ok: impl FnOnce(T) -> Vec<u8>) -> Vec<u8> {
  match result {
    Ok(value) => ok(value),
    Err(error) => err_reply(&error),
  }
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

/// An OK reply carrying a `u32` count (the bytes a write stored).
fn ok_count(count: u32) -> Vec<u8> {
  let mut out = vec![STATUS_OK];
  out.extend_from_slice(&count.to_le_bytes());
  out
}

/// An OK reply carrying a `u64` handle (an opened directory).
fn ok_fh(fh: u64) -> Vec<u8> {
  let mut out = vec![STATUS_OK];
  out.extend_from_slice(&fh.to_le_bytes());
  out
}

/// An OK reply carrying a string (a `u32` length then the bytes) — a symlink target.
fn ok_str(text: &str) -> Vec<u8> {
  let mut out = vec![STATUS_OK];
  put_bytes(&mut out, text.as_bytes());
  out
}

/// An OK reply with no payload (an operation whose only result is success).
fn ok_unit() -> Vec<u8> {
  vec![STATUS_OK]
}

/// An OK reply carrying an encoded attribute then a `u64` handle (a created-and-opened file).
fn ok_attr_fh(attr: &NodeAttr, fh: u64) -> Vec<u8> {
  let mut out = vec![STATUS_OK];
  put_attr(&mut out, attr);
  out.extend_from_slice(&fh.to_le_bytes());
  out
}

/// An OK reply carrying directory entries: a `u32` count then each entry — its inode (`u64`), its kind
/// tag, and its name (a length-prefixed byte field).
fn ok_entries(entries: &[DirEntry]) -> Vec<u8> {
  let mut out = vec![STATUS_OK];
  let count = u32::try_from(entries.len()).unwrap_or(u32::MAX);
  out.extend_from_slice(&count.to_le_bytes());
  for entry in entries {
    out.extend_from_slice(&entry.ino.to_le_bytes());
    out.push(kind_tag(entry.kind));
    put_bytes(&mut out, entry.name.as_bytes());
  }
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

/// The mask bit for an optional field: the bit if the field is present, zero otherwise. Folding the
/// `SETATTR_*` mask through this keeps [`put_setattr`] branch-free where it builds the mask.
fn mask_bit<T>(field: Option<T>, bit: u32) -> u32 {
  if field.is_some() { bit } else { 0 }
}

/// Encodes the [`OP_SETATTR`] body onto `out`: the object, then the `SETATTR_*` field mask and the
/// present values in field order. Split from [`ShimRequest::encode`] so that match stays under the
/// cognitive-complexity budget.
fn put_setattr(out: &mut Vec<u8>, object: ObjectId, changes: SetAttr) {
  out.push(OP_SETATTR);
  put_object(out, object);
  let valid = mask_bit(changes.size, SETATTR_SIZE)
    | mask_bit(changes.mode, SETATTR_MODE)
    | mask_bit(changes.uid, SETATTR_UID)
    | mask_bit(changes.gid, SETATTR_GID)
    | mask_bit(changes.atime, SETATTR_ATIME)
    | mask_bit(changes.mtime, SETATTR_MTIME);
  out.extend_from_slice(&valid.to_le_bytes());
  if let Some(value) = changes.size {
    out.extend_from_slice(&value.to_le_bytes());
  }
  if let Some(value) = changes.mode {
    out.extend_from_slice(&value.to_le_bytes());
  }
  if let Some(value) = changes.uid {
    out.extend_from_slice(&value.to_le_bytes());
  }
  if let Some(value) = changes.gid {
    out.extend_from_slice(&value.to_le_bytes());
  }
  if let Some(value) = changes.atime {
    out.extend_from_slice(&value.to_le_bytes());
  }
  if let Some(value) = changes.mtime {
    out.extend_from_slice(&value.to_le_bytes());
  }
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

/// Reads a little-endian `i64` from the front of `rest`, advancing it, or `Truncated`. The bytes are a
/// two's-complement bit pattern (a timestamp), so this decodes them directly rather than casting a
/// `u64`, which would trip the wrap lint.
fn take_i64(rest: &mut &[u8]) -> Result<i64, ShimWireError> {
  let (head, tail) = split_at_checked(rest, size_of::<i64>())?;
  *rest = tail;
  let mut word = [0u8; size_of::<i64>()];
  word.copy_from_slice(head);
  Ok(i64::from_le_bytes(word))
}

/// Decodes the [`OP_SETATTR`] body: the object, then the `SETATTR_*` field mask and the present values
/// in field order (size, mode, uid, gid, atime, mtime). Split from `ShimRequest::decode`'s match so
/// that match stays under the cognitive-complexity budget. An absent field consumes no bytes; a
/// truncated present one refuses through `take_*` before the bridge is touched.
fn take_setattr(rest: &mut &[u8]) -> Result<ShimRequest, ShimWireError> {
  let object = take_object(rest)?;
  let valid = take_u32(rest)?;
  let size = if valid & SETATTR_SIZE != 0 {
    Some(take_u64(rest)?)
  } else {
    None
  };
  let mode = if valid & SETATTR_MODE != 0 {
    Some(take_u32(rest)?)
  } else {
    None
  };
  let uid = if valid & SETATTR_UID != 0 {
    Some(take_u32(rest)?)
  } else {
    None
  };
  let gid = if valid & SETATTR_GID != 0 {
    Some(take_u32(rest)?)
  } else {
    None
  };
  let atime = if valid & SETATTR_ATIME != 0 {
    Some(take_i64(rest)?)
  } else {
    None
  };
  let mtime = if valid & SETATTR_MTIME != 0 {
    Some(take_i64(rest)?)
  } else {
    None
  };
  Ok(ShimRequest::SetAttr {
    object,
    size,
    mode,
    uid,
    gid,
    atime,
    mtime,
  })
}

/// Reads a little-endian `u32` from the front of `rest`, advancing it, or `Truncated`.
fn take_u32(rest: &mut &[u8]) -> Result<u32, ShimWireError> {
  let (head, tail) = split_at_checked(rest, size_of::<u32>())?;
  *rest = tail;
  let mut word = [0u8; size_of::<u32>()];
  word.copy_from_slice(head);
  Ok(u32::from_le_bytes(word))
}

/// Reads a length-prefixed path component (at most [`MAX_NAME_BYTES`]) from the front of `rest`,
/// refusing an over-long or non-UTF-8 name (hostile input) as a typed error.
fn take_name(rest: &mut &[u8]) -> Result<String, ShimWireError> {
  take_string(rest, MAX_NAME_BYTES)
}

/// Reads a length-prefixed UTF-8 string (at most `cap` bytes) from the front of `rest`, refusing an
/// over-long or non-UTF-8 value as a typed error — the symlink target and other path fields.
fn take_string(rest: &mut &[u8], cap: usize) -> Result<String, ShimWireError> {
  let bytes = take_bytes(rest, cap)?;
  String::from_utf8(bytes.to_vec()).map_err(|_| ShimWireError::BadLength)
}

/// Reads the one-byte rename flag field from the front of `rest`.
fn take_flags(rest: &mut &[u8]) -> Result<u8, ShimWireError> {
  let (&byte, tail) = rest.split_first().ok_or(ShimWireError::Truncated)?;
  *rest = tail;
  Ok(byte)
}

/// The one-byte rename flag field: the no-replace and exchange bits.
fn rename_flags_byte(no_replace: bool, exchange: bool) -> u8 {
  let no_replace = if no_replace { RENAME_NO_REPLACE } else { 0 };
  let exchange = if exchange { RENAME_EXCHANGE } else { 0 };
  no_replace | exchange
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
