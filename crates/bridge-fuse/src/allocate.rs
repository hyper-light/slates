//! `FUSE_FALLOCATE` served in cooperative slices (A-108).
//!
//! `fallocate(2)` mode 0 promises that a later write into the range never fails for space, and extends the file to
//! the range's end. slates keeps that promise by materializing the range's holes with zeros, which the quota
//! charges now (`Volume::allocate`); held bytes are never touched, so the range reads as before. The whole request
//! is admitted first, all of it or none, so a range larger than the quota is `ENOSPC` before anything changes.
//!
//! An allocation is user-scaled work: a gigabyte is about 16,000 chunk windows of zeros. It therefore runs as an
//! [`Allocation`] the channel keeps between turns, one slice per turn, with the shard free to serve other work
//! between slices (CLAUDE.md "Bounded work everywhere"). The request's reply is written only when its last slice
//! lands. The synchronous [`crate::bridge::dispatch`] cannot yield and so answers `EOPNOTSUPP`; only a transport
//! that steps requests serves it.
//!
//! Modes: `FALLOC_FL_KEEP_SIZE` alone is served when the range ends within the file, where it is mode 0 without the
//! size change. Past the end it is `EOPNOTSUPP`, since a volume holds no bytes beyond a file's size. Punching holes,
//! zeroing, collapsing and inserting ranges are `EOPNOTSUPP` too. The kernel answers the caller with that, and
//! glibc's `posix_fallocate` falls back to writing zeros on it.
//!
//! The promise holds until a snapshot shares the range's windows: a write then takes new space for its copy, as
//! on btrfs, the other copy-on-write filesystem Linux ships.

use slates_bridge_core::{Bridge, ObjectId, OpContext};
use slates_vfs::error::VfsError;

use crate::abi::Opcode;
use crate::reply::ReplyHeader;
use crate::request::{FallocateIn, Request};

/// Format: `FALLOC_FL_KEEP_SIZE` (`<linux/falloc.h>`): allocate without changing the file's size.
pub const KEEP_SIZE: u32 = 0x01;

/// An admitted allocation still running: the request it answers and the part of its range not yet materialized.
#[derive(Debug)]
pub struct Allocation {
  request: Vec<u8>,
  unique: u64,
  object: ObjectId,
  next: u64,
  end: u64,
}

/// What beginning an allocation came to.
#[derive(Debug)]
pub enum Begun {
  /// The request was answered at once (a refusal, or an empty range); the reply's length in the buffer.
  Answered(usize),
  /// The request was admitted and runs in slices.
  Running(Allocation),
}

impl Allocation {
  /// Begins the allocation `request` asks for: parses it, refuses an unserved mode or an unadmittable range with
  /// the reply written into `out`, and otherwise admits the whole range (all or none) and returns it running.
  pub fn begin(request: &[u8], bridge: &mut dyn Bridge, cx: &OpContext, out: &mut [u8]) -> Begun {
    let parsed = match Request::parse(request) {
      Ok(parsed) => parsed,
      Err(_) => return Begun::Answered(0),
    };
    let unique = parsed.header.unique;
    let refuse =
      |errno: i32, out: &mut [u8]| answered(ReplyHeader::write_error(unique, errno, out).ok());
    let Ok(body) = FallocateIn::parse(Opcode::Fallocate.to_wire(), parsed.body) else {
      return refuse(crate::bridge::EIO, out);
    };
    let admitted = admit(bridge, cx, parsed.header.nodeid, &body);
    match admitted {
      Ok(Some((object, end))) => Begun::Running(Allocation {
        request: request.to_vec(),
        unique,
        object,
        next: body.offset,
        end,
      }),
      Ok(None) => answered(ReplyHeader::write_ok(unique, &[], out).ok()),
      Err(errno) => refuse(errno, out),
    }
  }

  /// Materializes the next slice of the range: up to the next multiple of `slice_bytes`, so slices of whole chunk
  /// windows stay on window boundaries whatever the request's offset. `None` while more remains; the reply's length
  /// in `out` once the range is done or a slice was refused (`ENOSPC` when the quota filled meanwhile; the slices
  /// before it stay, as a partial `fallocate` does on Linux).
  pub fn step(
    &mut self,
    bridge: &mut dyn Bridge,
    cx: &OpContext,
    slice_bytes: u64,
    out: &mut [u8],
  ) -> Option<usize> {
    let slice = slice_bytes.max(1);
    let boundary = self
      .next
      .checked_div(slice)
      .unwrap_or(0)
      .saturating_add(1)
      .saturating_mul(slice);
    let stop = self.end.min(boundary);
    let len = stop.saturating_sub(self.next);
    if let Err(e) = bridge.allocate(self.object, cx, self.next, len) {
      let errno = crate::bridge::errno(e);
      return Some(ReplyHeader::write_error(self.unique, errno, out).unwrap_or(0));
    }
    self.next = stop;
    (self.next >= self.end).then(|| ReplyHeader::write_ok(self.unique, &[], out).unwrap_or(0))
  }

  /// The request as the kernel sent it.
  pub fn request(&self) -> &[u8] {
    &self.request
  }

  /// The request, given back when the allocation is done.
  pub fn into_request(self) -> Vec<u8> {
    self.request
  }
}

/// The reply length a written reply came to, or none (zero) when the buffer could not take it.
fn answered(written: Option<usize>) -> Begun {
  Begun::Answered(written.unwrap_or(0))
}

/// Resolves and admits the request's range: the object and the range's end when it runs, `None` for an empty range,
/// or the errno that refuses it.
fn admit(
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  nodeid: u64,
  body: &FallocateIn,
) -> Result<Option<(ObjectId, u64)>, i32> {
  let errno = crate::bridge::errno;
  if body.mode & !KEEP_SIZE != 0 {
    return Err(crate::bridge::EOPNOTSUPP);
  }
  let object = crate::bridge::resolve(bridge, cx, nodeid).map_err(errno)?;
  let end = body
    .offset
    .checked_add(body.length)
    .ok_or(errno(VfsError::FileTooLarge))?;
  if body.mode & KEEP_SIZE != 0 && end > bridge.getattr(object, cx).map_err(errno)?.size {
    return Err(crate::bridge::EOPNOTSUPP);
  }
  if body.length == 0 {
    return Ok(None);
  }
  bridge
    .admit_allocation(object, cx, body.offset, body.length)
    .map_err(errno)?;
  Ok(Some((object, end)))
}
