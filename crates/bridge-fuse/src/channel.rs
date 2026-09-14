//! The `/dev/fuse` transport (§4.6 "Linux (own /dev/fuse driver)"; Phase 3 task 1b). The kernel
//! and the daemon exchange messages over the character device `/dev/fuse`: a `read` returns one
//! request (or a batch the driver splits), a `write` sends one reply. This module owns the
//! device descriptor and turns it into the request/reply stream the [`crate::dispatch`] serves.
//!
//! Linux only: `/dev/fuse` and the FUSE ABI are the Linux kernel's. The serve loop and the
//! device I/O run in the CI Linux lane against a real mount; this file compiles and cross-lints
//! everywhere. The blocking loop here is the fallback the design names; the io_uring command
//! path and the per-shard `FUSE_DEV_IOC_CLONE` channels are the driver's next pieces (owed), and
//! the mount establishment (the new mount API, or `fusermount3`) is [`crate::mount`].
//!
//! Kernel coherence (§4.6 "Cache posture"): before each request is served, the loop asks the seam
//! for every invalidation owed since it last looked — a change through the SDK or another
//! attachment, or an outsider's change beneath a base directory — and writes each to the device
//! as an unsolicited notification, so the kernel never answers a lookup or a stat from a cache
//! newer than the daemon's view. The loop's own request needs no invalidation in its own kernel,
//! so the cursor is taken again after it. What the kernel negotiated at `INIT` decides whether an
//! entry is expired (`FUSE_EXPIRE_ONLY`, the live-source case) or dropped.
//!
//! No `unsafe`: the device is opened, read and written through rustix's I/O-safe wrappers over
//! an owned descriptor.

#![cfg(target_os = "linux")]

use std::os::fd::{AsFd, OwnedFd};

use rustix::fs::{Mode, OFlags};

use crate::abi::{IN_HEADER_LEN, OUT_HEADER_LEN, Opcode, flags};
use crate::bridge::Bridge;
use crate::dispatch;
use crate::error::FuseError;
use crate::init::negotiate;
use crate::notify::{EXPIRE_ONLY, inval_entry, inval_inode};
use crate::request::Request;
use slates_bridge_core::{AttachmentId, Attachments, Invalidation, InvalidationCursor};

/// Format: the device the kernel's FUSE client and the daemon exchange messages over.
const FUSE_DEVICE: &str = "/dev/fuse";

/// Shape: the request buffer size: the negotiated maximum write (256 KiB) plus a page for the
/// headers, the size the kernel expects a reader to offer so a large write arrives in one read.
const BUFFER_BYTES: usize = 256 * 1024 + 4096;

/// Format: the longest notification: the header, the 24-byte inode or delete body, and a name up
/// to the volume's name cap with its NUL.
const NOTIFY_BYTES: usize = OUT_HEADER_LEN + 24 + 255 + 1;

/// Format: `fuse_notify_inval_inode_out.off` of -1 — invalidate the attributes only, no data.
const ATTRIBUTES_ONLY: i64 = -1;
/// Format: `fuse_notify_inval_inode_out.len` of -1 with a zero offset — the whole data range.
const WHOLE_FILE: i64 = -1;

/// A refusal from the transport.
#[derive(Debug)]
pub enum ChannelError {
  /// The device could not be opened (the FUSE module is absent, or the caller lacks access).
  Open {
    /// The OS error code.
    code: Option<i32>,
  },
  /// A read or write on the device refused.
  Device {
    /// The call.
    call: &'static str,
    /// The OS error code.
    code: Option<i32>,
  },
  /// The kernel closed the connection (the mount is gone).
  Disconnected,
}

impl std::fmt::Display for ChannelError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Open { code } => write!(f, "cannot open {FUSE_DEVICE} (code {code:?})"),
      Self::Device { call, code } => write!(f, "{call} on {FUSE_DEVICE} refused (code {code:?})"),
      Self::Disconnected => f.write_str("the FUSE connection is gone"),
    }
  }
}

impl std::error::Error for ChannelError {}

/// The daemon's end of one FUSE connection: the device descriptor and a reusable buffer.
pub struct FuseChannel {
  device: OwnedFd,
  buffer: Vec<u8>,
}

impl std::fmt::Debug for FuseChannel {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("FuseChannel").finish()
  }
}

impl FuseChannel {
  /// Opens `/dev/fuse`. The mount ([`crate::mount`]) then attaches this descriptor to a mount
  /// point; several channels over one connection are made by cloning the descriptor
  /// (`FUSE_DEV_IOC_CLONE`, owed).
  pub fn open() -> Result<FuseChannel, ChannelError> {
    // /dev/fuse is a character device the kernel exposes, opened read-write to exchange FUSE
    // messages; it is not a disk file and creates nothing (R1, §4.6).
    // structural: allow — device open, not a disk-file write.
    let device = rustix::fs::open(FUSE_DEVICE, OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())
      .map_err(|e| ChannelError::Open {
        code: Some(e.raw_os_error()),
      })?;
    Ok(FuseChannel::from_device(device))
  }

  /// A channel over a device descriptor the mount already opened (the anchor hands it back on a
  /// restart, §2.6 step 4).
  pub fn from_device(device: OwnedFd) -> FuseChannel {
    FuseChannel {
      device,
      buffer: vec![0u8; BUFFER_BYTES],
    }
  }

  /// The device descriptor (for the mount and for cloning).
  pub fn device(&self) -> impl AsFd + '_ {
    self.device.as_fd()
  }

  /// Reads the next request into the internal buffer; returns the bytes read. `ENODEV` means
  /// the kernel unmounted (the caller stops); `EINTR`/`EAGAIN` are retried by the caller.
  pub fn read_request(&mut self) -> Result<&[u8], ChannelError> {
    match rustix::io::read(&self.device, self.buffer.as_mut_slice()) {
      Ok(n) => Ok(&self.buffer[..n]),
      Err(rustix::io::Errno::NODEV) => Err(ChannelError::Disconnected),
      Err(e) => Err(ChannelError::Device {
        call: "read",
        code: Some(e.raw_os_error()),
      }),
    }
  }

  /// Writes one reply to the device.
  pub fn write_reply(&self, reply: &[u8]) -> Result<(), ChannelError> {
    // A zero-length reply is a request that needs none (FORGET); write nothing.
    if reply.is_empty() {
      return Ok(());
    }
    rustix::io::write(&self.device, reply)
      .map(|_| ())
      .map_err(|e| ChannelError::Device {
        call: "write",
        code: Some(e.raw_os_error()),
      })
  }

  /// Writes one kernel invalidation to the device as an unsolicited notification (§4.6
  /// `notify`). `expire_only` says the kernel negotiated `FUSE_HAS_EXPIRE_ONLY`, so a live-source
  /// entry is expired rather than dropped. The kernel answers `ENOENT` when it holds no such
  /// entry and `ENOTEMPTY` when a directory it would drop is in use — neither is a fault of the
  /// mount (the kernel has nothing stale, or will revalidate on its next use), so both are
  /// absorbed; any other refusal is the transport's.
  pub fn write_invalidation(
    &self,
    invalidation: &Invalidation,
    expire_only: bool,
  ) -> Result<(), ChannelError> {
    let mut out = [0u8; NOTIFY_BYTES];
    let encoded = match invalidation {
      Invalidation::Entry {
        parent,
        name,
        expire,
      } => {
        let flags = if *expire && expire_only {
          EXPIRE_ONLY
        } else {
          0
        };
        inval_entry(*parent, name, flags, &mut out)
      }
      Invalidation::Inode { ino, data } => {
        if *data {
          inval_inode(*ino, 0, WHOLE_FILE, &mut out)
        } else {
          inval_inode(*ino, ATTRIBUTES_ONLY, 0, &mut out)
        }
      }
    };
    let Ok(n) = encoded else {
      // A name past the wire cap cannot be cached by the kernel either: nothing to drop.
      return Ok(());
    };
    match rustix::io::write(&self.device, &out[..n]) {
      Ok(_) | Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTEMPTY) => Ok(()),
      Err(e) => Err(ChannelError::Device {
        call: "notify",
        code: Some(e.raw_os_error()),
      }),
    }
  }
}

/// The blocking serve loop (the fallback path, §4.6): deliver the invalidations owed since the
/// last request, read a request, dispatch it to `bridge`, write the reply, until the kernel
/// unmounts. The io_uring command path replaces this on 6.14+; both drive the same [`dispatch`].
/// A read shorter than a header is a malformed message the driver drops; a device error other
/// than a disconnect is returned to the caller.
pub fn serve_blocking(
  channel: &mut FuseChannel,
  bridge: &mut dyn Bridge,
  attachments: &Attachments,
  attachment: AttachmentId,
) -> Result<(), ChannelError> {
  let mut reply = vec![0u8; BUFFER_BYTES];
  let mut owed: Vec<Invalidation> = Vec::new();
  // Whether the kernel honours FUSE_EXPIRE_ONLY, learned from its INIT (the negotiation is pure,
  // so re-running it here agrees with the reply the dispatch sends).
  let mut expire_only = false;
  // Where the kernel's cache stands: nothing before the mount's first request can be in it.
  let mut cursor: Option<InvalidationCursor> = None;
  loop {
    let request = match channel.read_request() {
      Ok(r) => r,
      Err(ChannelError::Disconnected) => return Ok(()),
      Err(e) => return Err(e),
    };
    if request.len() < IN_HEADER_LEN {
      // A truncated read: the kernel never sends one, so drop it and read again rather than
      // reply to a message with no header.
      continue;
    }
    // Build the authenticated context from the mount's attachment before each effect, so a
    // revoked or epoch-fenced attachment stops the mount rather than serving a request under
    // stale authority (§4.8; per-request revalidation, the "checked before effects" rule). The
    // registry's concurrent-revoke ownership is the async driver's design (owed).
    let Ok(cx) = attachments.context(attachment) else {
      return Ok(());
    };
    // `dispatch` needs a mutable reply buffer separate from the request buffer the channel
    // owns, so the request is copied out (one memcpy of a small message; the io_uring path
    // avoids it with registered buffers, owed).
    let request = channel.take_request();
    if let Ok(parsed) = Request::parse(&request)
      && parsed.opcode == Some(Opcode::Init)
      && let Ok(negotiated) = negotiate(parsed.body)
    {
      expire_only = negotiated.flags & flags::HAS_EXPIRE_ONLY != 0;
    }
    // Everything that changed under the kernel's cache since this loop last looked is delivered
    // before the request is served, so the reply never coexists with a stale cached name or
    // attribute (§4.6). A seam refusal here leaves the cursor where it was, so the delivery is
    // retried before the next request rather than lost.
    let since = cursor.unwrap_or_else(|| bridge.seen(&cx));
    if let Ok(next) = bridge.invalidations(&cx, since, &mut owed) {
      cursor = Some(next);
    }
    for invalidation in owed.drain(..) {
      channel.write_invalidation(&invalidation, expire_only)?;
    }
    let n = dispatch(&request, bridge, &cx, &mut reply);
    if n > 0 {
      channel.write_reply(&reply[..n])?;
    }
    // The request's own records are this kernel's own doing: take the cursor past them.
    cursor = Some(bridge.seen(&cx));
  }
}

impl FuseChannel {
  /// The last request read, copied out so the reply may be written into the channel's own
  /// buffer without aliasing (the copy is one memcpy of a small message; the io_uring path
  /// avoids it with registered buffers, owed).
  fn take_request(&self) -> Vec<u8> {
    // The blocking loop read into `buffer`; the request is its head up to the header's length.
    let len = self
      .buffer
      .get(..size_of::<u32>())
      .map(|b| u32::from_le_bytes(b.try_into().unwrap_or_default()))
      .map(|l| usize::try_from(l).unwrap_or(0).min(self.buffer.len()))
      .unwrap_or(0);
    self.buffer[..len].to_vec()
  }
}

/// A stable reference for `FuseError` so the transport's error can carry a codec refusal where
/// one is surfaced to callers (kept for the driver's structured errors).
impl From<FuseError> for ChannelError {
  fn from(_: FuseError) -> ChannelError {
    ChannelError::Device {
      call: "codec",
      code: None,
    }
  }
}
