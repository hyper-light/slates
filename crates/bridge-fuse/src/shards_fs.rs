//! Serving a slates volume to a `shards` microVM's virtio-fs device (§4.6, the Shards route; A-161; `docs/wip/shards/`).
//! Shards' VMM forwards each guest FUSE request to a backend over a Unix stream it is handed, framed as a little-endian
//! `u32` length then the request, and reads one reply frame back for each, an empty one for a request that takes no
//! reply (Shards' `gpu` branch, `crates/vmm/src/devices/virtio/fs/mod.rs`). Its own asks are control frames, the length
//! word's top bit set ([`CONTROL_FRAME`]). This module is the backend's byte work, which the daemon's serve task
//! (`crates/server/src/shards.rs`) runs on the end it keeps: [`split_frame`] takes a frame from the bytes read so far,
//! its length bounded before anything is allocated for it; [`answer`] dispatches a request through the bridge the kernel
//! mount uses ([`crate::bridge::dispatch`], the one FUSE seam) and writes the reply frame; [`control_refusal`] and
//! [`descriptor_refusal`] answer the asks slates does not serve, in Shards' forms.
//!
//! These are functions over bytes, holding no socket and no OS type, so they stay inside the lint wall's RAM-only rule
//! (R1) and run on every platform; the stream, its readiness and its end are the daemon's. Every failure is a typed
//! [`std::io::Error`] or a FUSE error reply, never a panic.
//!
//! The non-DAX path: a frame carries a FUSE request, and a DAX window's descriptor ask is refused `EOPNOTSUPP`, because a
//! file's descriptor would carry its whole content object (A-135); a window over a backing that authorizes only one
//! attachment's bytes is Shards part 3.

use std::io;

use slates_bridge_core::{Bridge, OpContext};

use crate::abi::{OUT_HEADER_LEN, Opcode};
use crate::bridge::{EIO, dispatch, reply_error, takes_reply};
use crate::init::{MAX_WRITE, max_pages};
use crate::reply::ReplyHeader;
use crate::request::{InHeader, ReadIn, Request};

/// Format: the bit of a frame's length word that marks a frame of the VM process's own, never a guest's: Shards' fs
/// device sets it on its control asks (its snapshot tables, D176; a DAX window's descriptor, D189), and no guest
/// request, at most a write and its headers, can reach it.
pub const CONTROL_FRAME: u32 = 1 << 31;
/// Format: Shards' control ask for the backend's tables, saved with a snapshot of the VM (D176).
pub const ASK_SAVE: u8 = 1;
/// Format: Shards' control ask that the backend take the tables a restored VM brought (D176).
pub const ASK_RESTORE: u8 = 2;
/// Format: Shards' control ask for a file's descriptor, for its DAX window's mapping (D189); its answer is a
/// `shards_ipc` message, not a control frame.
pub const ASK_DESCRIPTOR: u8 = 3;
/// Format: the bytes of a frame's length word.
pub const LENGTH_BYTES: usize = 4;
/// Format: the headers around the largest payload a frame carries: a FUSE in-header (40 bytes) and a write's own
/// header (40), or an out-header and a read's bytes, within one page, as the FUSE device's own buffer bounds them.
const HEADERS: usize = 4096;
/// Derived: the largest frame slates reads or writes: a request or a reply carrying the largest write it accepts
/// ([`MAX_WRITE`], which the guest is told at `INIT` and never exceeds), with the headers around it.
pub const FRAME_BYTES: usize = MAX_WRITE as usize + HEADERS;
/// Format: a `shards_ipc` message's header: its kind (1 byte), then its payload's length (big-endian `u32`).
const IPC_HEADER_BYTES: usize = 5;
/// Format: the `shards_ipc` kind of a refused descriptor ask: any but zero, which carries the descriptor.
const IPC_REFUSED: u8 = 1;

/// Format: the largest page a Linux guest runs on (arm64's `CONFIG_ARM64_64K_PAGES`; x86-64 has 4 KiB pages only). A
/// guest's read asks for up to its page count in its own pages, and the count the daemon's `INIT` names was sized by
/// the host's page, so a guest with larger pages than its host asks for more bytes than the count suggests.
const LARGEST_GUEST_PAGE: usize = 64 * 1024;
/// Format: the pages a kernel's request carries when the server's `INIT` named no count, because the kernel did not
/// offer `FUSE_MAX_PAGES` (`FUSE_DEFAULT_MAX_PAGES_PER_REQ`, `fs/fuse/fuse_i.h`).
const KERNEL_DEFAULT_PAGES: usize = 32;

/// Derived: the most bytes a guest's read may ask for: the pages a request carries (the count `INIT` names,
/// [`max_pages`], or the kernel's default where it named none, whichever is larger) in the largest page a guest runs
/// on. 2 MiB on a host of 16 KiB pages, 4 MiB on one of 4 KiB pages.
pub fn read_ceiling() -> usize {
  usize::from(max_pages())
    .max(KERNEL_DEFAULT_PAGES)
    .saturating_mul(LARGEST_GUEST_PAGE)
}

/// What one request frame came to: its reply frame, written at the head of the caller's buffer, and what the caller
/// needs to finish it: the barrier a durable change waits for ([`crate::bridge::needs_barrier`]), and the reclaim of a
/// reply refused after it was made ([`crate::bridge::reclaim_unreported`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Answered {
  /// The request's opcode; `None` for one this bridge does not know, or a request with no header.
  pub opcode: Option<Opcode>,
  /// The node the request named.
  pub nodeid: u64,
  /// The request's unique id, which its reply carries.
  pub unique: u64,
  /// The reply's errno: zero for success, and for a request that takes no reply.
  pub error: i32,
  /// The reply frame's bytes at the head of the buffer: the length word, then the reply. Only the word, naming zero
  /// bytes, for a request that takes no reply, so its asker waits on nothing.
  pub frame: usize,
}

impl Answered {
  /// The reply inside `frame` (the buffer [`answer`] wrote), without its length word.
  pub fn reply<'a>(&self, frame: &'a [u8]) -> &'a [u8] {
    frame.get(LENGTH_BYTES..self.frame).unwrap_or(&[])
  }
}

/// Answers one guest request, writing its reply frame at the head of `frame`, which keeps its length from one request
/// to the next so a buffer is zeroed only when it grows. A read's reply has room for every byte asked: a reply shorter
/// than asked is the file's end to the guest's kernel, which then shrinks the size it caches, so a read is never cut to
/// fit a buffer; one asking past [`read_ceiling`], more than any guest is told it may, is answered `EIO` without being
/// served. Every other reply has [`FRAME_BYTES`] of room, which a directory listing (cut short legally, the next resuming
/// at its cookie) and an attribute value (64 KiB at most) fit. A request that takes a reply and produced none (a body
/// too short for its opcode) is answered `EIO`, so its asker never waits forever; a request with no header is answered
/// with an empty frame.
pub fn answer(
  request: &[u8],
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  frame: &mut Vec<u8>,
) -> Answered {
  let header = InHeader::parse(request).ok();
  let opcode = header.as_ref().and_then(|h| Opcode::from_wire(h.opcode));
  let (unique, nodeid) = header.as_ref().map_or((0, 0), |h| (h.unique, h.nodeid));
  let asked = (opcode == Some(Opcode::Read))
    .then(|| read_asked(request))
    .flatten();
  let past_ceiling = asked.is_some_and(|asked| asked > read_ceiling());
  let room = match asked {
    Some(asked) if !past_ceiling => OUT_HEADER_LEN.saturating_add(asked),
    Some(_) => OUT_HEADER_LEN,
    None => FRAME_BYTES,
  };
  let need = LENGTH_BYTES.saturating_add(room);
  if frame.len() < need {
    frame.resize(need, 0);
  }
  let Some(body) = frame.get_mut(LENGTH_BYTES..need) else {
    return Answered {
      opcode,
      nodeid,
      unique,
      error: 0,
      frame: 0,
    };
  };
  let mut len = if past_ceiling {
    ReplyHeader::write_error(unique, EIO, body).unwrap_or(0)
  } else {
    dispatch(request, bridge, cx, body)
  };
  if len == 0 && header.is_some() && takes_reply(opcode) {
    len = ReplyHeader::write_error(unique, EIO, body).unwrap_or(0);
  }
  let error = body.get(..len).map_or(0, reply_error);
  let word = u32::try_from(len).unwrap_or(0).to_le_bytes();
  if let Some(head) = frame.get_mut(..LENGTH_BYTES) {
    head.copy_from_slice(&word);
  }
  Answered {
    opcode,
    nodeid,
    unique,
    error,
    frame: LENGTH_BYTES.saturating_add(len),
  }
}

/// Overwrites the reply frame at the head of `frame` with an error reply to request `unique`: a reply refused after it
/// was made (its change not captured by the barrier). The frame's bytes, the length word included.
pub fn refuse(unique: u64, errno: i32, frame: &mut Vec<u8>) -> usize {
  let need = LENGTH_BYTES.saturating_add(OUT_HEADER_LEN);
  if frame.len() < need {
    frame.resize(need, 0);
  }
  let len = frame.get_mut(LENGTH_BYTES..need).map_or(0, |body| {
    ReplyHeader::write_error(unique, errno, body).unwrap_or(0)
  });
  let word = u32::try_from(len).unwrap_or(0).to_le_bytes();
  if let Some(head) = frame.get_mut(..LENGTH_BYTES) {
    head.copy_from_slice(&word);
  }
  LENGTH_BYTES.saturating_add(len)
}

/// The bytes a read request asks for; `None` for a body too short to say, which the dispatch refuses.
fn read_asked(request: &[u8]) -> Option<usize> {
  let parsed = Request::parse(request).ok()?;
  let read = ReadIn::parse(Opcode::Read.to_wire(), parsed.body).ok()?;
  usize::try_from(read.size).ok()
}

/// One frame split from the bytes read so far: a guest's FUSE request, or a control ask of the VM process's own.
#[derive(Debug, PartialEq, Eq)]
pub enum Frame<'a> {
  /// A guest's FUSE request, its bytes.
  Request(&'a [u8]),
  /// A control ask: its first byte, and what follows.
  Control {
    /// What it asks ([`ASK_SAVE`], [`ASK_RESTORE`], [`ASK_DESCRIPTOR`]).
    ask: u8,
    /// What it brings.
    body: &'a [u8],
  },
}

/// What the bytes read so far hold at their head.
#[derive(Debug, PartialEq, Eq)]
pub enum Split<'a> {
  /// A whole frame, and the bytes it takes, its length word included.
  Whole(Frame<'a>, usize),
  /// Part of a frame: the bytes it takes in all once whole (only its length word's, while that is incomplete), at
  /// most [`LENGTH_BYTES`] past the caller's bound.
  Partial(usize),
}

/// The first frame of `bytes`, whole or in part. A length past `max` (the caller's bound, at most [`FRAME_BYTES`]) is
/// refused `InvalidData` as soon as its length word has arrived, before anything is allocated for it, as is a control
/// frame with no ask byte. A frame of length zero is a request with no bytes, which the answer that follows refuses.
pub fn split_frame(bytes: &[u8], max: usize) -> io::Result<Split<'_>> {
  let Some(word) = bytes.get(..LENGTH_BYTES) else {
    return Ok(Split::Partial(LENGTH_BYTES));
  };
  let word = u32::from_le_bytes(
    <[u8; LENGTH_BYTES]>::try_from(word).map_err(|_| io::ErrorKind::InvalidData)?,
  );
  let control = word & CONTROL_FRAME != 0;
  let len = usize::try_from(word & !CONTROL_FRAME).unwrap_or(usize::MAX);
  if len > max {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "a shards fs frame longer than the buffer bounds",
    ));
  }
  let total = LENGTH_BYTES.saturating_add(len);
  let Some(body) = bytes.get(LENGTH_BYTES..total) else {
    return Ok(Split::Partial(total));
  };
  if !control {
    return Ok(Split::Whole(Frame::Request(body), total));
  }
  let Some((&ask, body)) = body.split_first() else {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "a shards fs control frame with no ask",
    ));
  };
  Ok(Split::Whole(Frame::Control { ask, body }, total))
}

/// The answer to a control ask slates does not serve: a control frame whose first byte is not zero, then the reason
/// in words, as Shards' device reads one (its `control`: zero is a granted ask, anything else a refusal it reports).
pub fn control_refusal(reason: &str) -> Vec<u8> {
  let mut body = Vec::with_capacity(1usize.saturating_add(reason.len()));
  body.push(IPC_REFUSED);
  body.extend_from_slice(reason.as_bytes());
  let len = u32::try_from(body.len()).unwrap_or(0) | CONTROL_FRAME;
  let mut frame = Vec::with_capacity(LENGTH_BYTES.saturating_add(body.len()));
  frame.extend_from_slice(&len.to_le_bytes());
  frame.extend_from_slice(&body);
  frame
}

/// The answer to a descriptor ask slates does not serve: a `shards_ipc` message of a kind other than zero, whose payload
/// is the errno, little-endian, which Shards' device returns to the guest's mapping (its `descriptor_on`).
pub fn descriptor_refusal(errno: i32) -> Vec<u8> {
  let payload = errno.to_le_bytes();
  let mut message = Vec::with_capacity(IPC_HEADER_BYTES.saturating_add(payload.len()));
  message.push(IPC_REFUSED);
  message.extend_from_slice(&u32::try_from(payload.len()).unwrap_or(0).to_be_bytes());
  message.extend_from_slice(&payload);
  message
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Do: split a request, a control ask, partial frames, a frame past the bound, and a control frame with no ask.
  /// Expect each whole frame with its length, a partial one with the bytes it needs in all, and the two hostile ones
  /// refused as soon as their length word has arrived.
  #[test]
  fn frames_split_whole_and_hostile_lengths_are_refused() {
    let mut request = 3u32.to_le_bytes().to_vec();
    request.extend_from_slice(b"abc");
    assert_eq!(
      split_frame(&request, FRAME_BYTES).unwrap(),
      Split::Whole(Frame::Request(b"abc"), 7)
    );
    assert_eq!(
      split_frame(&request[..5], FRAME_BYTES).unwrap(),
      Split::Partial(7)
    );
    assert_eq!(
      split_frame(&request[..2], FRAME_BYTES).unwrap(),
      Split::Partial(LENGTH_BYTES)
    );
    let mut control = (2u32 | CONTROL_FRAME).to_le_bytes().to_vec();
    control.extend_from_slice(&[ASK_SAVE, 9]);
    assert_eq!(
      split_frame(&control, FRAME_BYTES).unwrap(),
      Split::Whole(
        Frame::Control {
          ask: ASK_SAVE,
          body: &[9]
        },
        6
      )
    );
    let huge = (!CONTROL_FRAME).to_le_bytes();
    assert!(split_frame(&huge, FRAME_BYTES).is_err());
    let empty_control = CONTROL_FRAME.to_le_bytes();
    assert!(split_frame(&empty_control, FRAME_BYTES).is_err());
  }

  /// Do: encode the two refusals. Expect a control frame whose first body byte is not zero and whose length word carries
  /// the control bit, and a `shards_ipc` message of a nonzero kind carrying the errno little-endian.
  #[test]
  fn refusals_are_encoded_as_the_device_reads_them() {
    let frame = control_refusal("no tables");
    assert_eq!(
      split_frame(&frame, FRAME_BYTES).unwrap(),
      Split::Whole(
        Frame::Control {
          ask: IPC_REFUSED,
          body: b"no tables"
        },
        frame.len()
      )
    );
    let message = descriptor_refusal(38);
    assert_eq!(message, [IPC_REFUSED, 0, 0, 0, 4, 38, 0, 0, 0]);
  }
}
