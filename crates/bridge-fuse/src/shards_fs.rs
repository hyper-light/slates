//! Serving a slates volume to a `shards` microVM's virtio-fs device (§4.6, the Shards route; A-161; `docs/wip/shards/`).
//! Shards' VMM forwards each guest FUSE request to a backend over a Unix stream it is handed, framed as a little-endian
//! `u32` length then the request, and reads one reply frame back for each, an empty one for a request that takes no
//! reply (Shards' `gpu` branch, `crates/vmm/src/devices/virtio/fs/mod.rs`). Its own asks are control frames, the length
//! word's top bit set ([`CONTROL_FRAME`]). This module is the backend's byte work, which the daemon's serve task
//! (`crates/server/src/shards.rs`) runs on the end it keeps: [`split_frame`] takes a frame from the bytes read so far,
//! its length bounded before anything is allocated for it; [`answer`] dispatches a request through the bridge the kernel
//! mount uses ([`crate::bridge::dispatch`], the one FUSE seam) and writes the reply frame; [`descriptor_ask`] reads a DAX
//! window's descriptor ask and [`descriptor_answer`] heads the answer that carries the descriptor; [`control_refusal`]
//! and [`descriptor_refusal`] answer what slates refuses, in Shards' forms.
//!
//! These are functions over bytes, holding no socket and no OS type, so they stay inside the lint wall's RAM-only rule
//! (R1) and run on every platform; the stream, its readiness and its end are the daemon's. Every failure is a typed
//! [`std::io::Error`] or a FUSE error reply, never a panic.
//!
//! The DAX path (Shards part 3; `docs/wip/shards/dax-protocol.md`, agreed with the Shards owner 2026-10-24): a descriptor
//! ask is answered with a descriptor of the asked file's DAX object, one per attachment and file, which holds only that
//! file's bytes (`slates_mem::DaxObject`). A descriptor of slates' content itself would carry its whole content object
//! (A-135). The daemon makes, fills and bounds the object (`crates/server/src/shards_dax.rs`); this module reads the ask,
//! in its 10-byte form or with the agreed range, and heads the answer.

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

/// Format: a DESCRIPTOR ask's body after its ask byte, as Shards' device sends it today: the node (le64) and the
/// writable byte (`mod.rs` `descriptor_on`).
pub const DESCRIPTOR_ASK_BODY: usize = 9;
/// Format: a DESCRIPTOR ask's body with the mapping's range appended: its offset in the file and its length, `le64`
/// each, the `fuse_setupmapping_in` fields of the mapping it is for (agreed with the Shards owner 2026-10-24,
/// `docs/wip/shards/dax-protocol.md`).
pub const DESCRIPTOR_ASK_RANGED_BODY: usize = 25;
/// Format: the `shards_ipc` kind of a descriptor answer that carries the descriptor.
const IPC_DESCRIPTOR: u8 = 0;

/// A DAX window's descriptor ask (Shards D189): the node whose file the mapping maps, whether the guest asked to write
/// it, and, from a device that names it, the mapping's range in the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DescriptorAsk {
  /// The node, as the guest's FUSE requests name it.
  pub node: u64,
  /// Whether the mapping is to be written (`FUSE_SETUPMAPPING_FLAG_WRITE`).
  pub writable: bool,
  /// The mapping's offset in the file and its length, `None` from a device that names no range.
  pub range: Option<(u64, u64)>,
}

/// Parses a DESCRIPTOR ask's body after its ask byte: `None` for a body of neither length the protocol has, a range of
/// no bytes, or one whose end overflows. The device's own kinds of ask are fixed lengths, so anything else is hostile or
/// cut short, and the caller refuses it.
pub fn descriptor_ask(body: &[u8]) -> Option<DescriptorAsk> {
  let word = |at: usize| -> Option<u64> {
    let bytes = body.get(at..at.checked_add(size_of::<u64>())?)?;
    Some(u64::from_le_bytes(bytes.try_into().ok()?))
  };
  let node = word(0)?;
  let writable = *body.get(size_of::<u64>())? != 0;
  let range = match body.len() {
    DESCRIPTOR_ASK_BODY => None,
    DESCRIPTOR_ASK_RANGED_BODY => {
      let (offset, len) = (
        word(DESCRIPTOR_ASK_BODY)?,
        word(DESCRIPTOR_ASK_BODY.checked_add(size_of::<u64>())?)?,
      );
      if len == 0 || offset.checked_add(len).is_none() {
        return None;
      }
      Some((offset, len))
    }
    _ => return None,
  };
  Some(DescriptorAsk {
    node,
    writable,
    range,
  })
}

/// The answer that carries a descriptor: a `shards_ipc` header of kind zero and an empty payload. The descriptor rides
/// its first byte by `SCM_RIGHTS`, which the daemon's send attaches; Shards' `shards_ipc::recv` takes it from there.
pub fn descriptor_answer() -> [u8; IPC_HEADER_BYTES] {
  [IPC_DESCRIPTOR, 0, 0, 0, 0]
}

/// Format: a DESCRIPTOR ask's body on Windows, after its ask byte (plan G-shards-windows step 5, agreed with the Shards
/// device's author 2026-10-26): the ranged body, then the asking process's id (`le32`, `GetCurrentProcessId`) and the
/// value in that process of its handle of the stream (`le64`). Windows passes no descriptor, so the section is
/// duplicated into the process the ask names, and the stream handle is how the daemon checks that the process holds
/// this attachment's stream.
pub const DESCRIPTOR_ASK_CLAIMED_BODY: usize =
  DESCRIPTOR_ASK_RANGED_BODY + size_of::<u32>() + size_of::<u64>();

/// A Windows DESCRIPTOR ask: the ask, and the process that claims to hold the stream with the value of its handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DescriptorClaim {
  /// The ask, always with its range.
  pub ask: DescriptorAsk,
  /// The process the section is to be duplicated into.
  pub pid: u32,
  /// The value, in that process, of its handle of the stream.
  pub stream: u64,
}

/// Parses a Windows DESCRIPTOR ask's body after its ask byte: `None` for a body of any other length than
/// [`DESCRIPTOR_ASK_CLAIMED_BODY`] or a range [`descriptor_ask`] refuses. The device is new on Windows, so it has no
/// shorter form to accept.
pub fn descriptor_claim(body: &[u8]) -> Option<DescriptorClaim> {
  if body.len() != DESCRIPTOR_ASK_CLAIMED_BODY {
    return None;
  }
  let (ranged, claim) = body.split_at_checked(DESCRIPTOR_ASK_RANGED_BODY)?;
  let ask = descriptor_ask(ranged)?;
  let (pid, stream) = claim.split_at_checked(size_of::<u32>())?;
  Some(DescriptorClaim {
    ask,
    pid: u32::from_le_bytes(pid.try_into().ok()?),
    stream: u64::from_le_bytes(stream.try_into().ok()?),
  })
}

/// The answer that hands a section on Windows: a `shards_ipc` message of kind zero whose payload is the value of the
/// section's handle in the asking process, `le64`.
pub fn descriptor_handle_answer(value: u64) -> Vec<u8> {
  let payload = value.to_le_bytes();
  let mut message = Vec::with_capacity(IPC_HEADER_BYTES.saturating_add(payload.len()));
  message.push(IPC_DESCRIPTOR);
  message.extend_from_slice(&u32::try_from(payload.len()).unwrap_or(0).to_be_bytes());
  message.extend_from_slice(&payload);
  message
}

/// The answer to a descriptor ask slates refuses: a `shards_ipc` message of a kind other than zero, whose payload is
/// the errno, little-endian, which Shards' device returns to the guest's mapping (its `descriptor_on`).
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

  /// T-shards-windows-5.3. Do: parse a Windows DESCRIPTOR ask of each length around the claimed one, and one whose
  /// range is empty. Expect the claimed body parsed field by field and every other refused; and the handle answer to
  /// be kind zero, an 8-byte big-endian length, then the value little-endian (golden bytes).
  #[test]
  fn a_windows_descriptor_ask_carries_its_claim_and_its_answer_the_handle_value() {
    let mut body = Vec::new();
    body.extend_from_slice(&7u64.to_le_bytes());
    body.push(1);
    body.extend_from_slice(&(2u64 << 20).to_le_bytes());
    body.extend_from_slice(&(2u64 << 20).to_le_bytes());
    body.extend_from_slice(&0x1234u32.to_le_bytes());
    body.extend_from_slice(&0x0abc_u64.to_le_bytes());
    assert_eq!(body.len(), DESCRIPTOR_ASK_CLAIMED_BODY);
    assert_eq!(
      descriptor_claim(&body),
      Some(DescriptorClaim {
        ask: DescriptorAsk {
          node: 7,
          writable: true,
          range: Some((2 << 20, 2 << 20)),
        },
        pid: 0x1234,
        stream: 0x0abc,
      })
    );
    assert_eq!(descriptor_claim(&body[..body.len() - 1]), None);
    assert_eq!(descriptor_claim(&body[..DESCRIPTOR_ASK_RANGED_BODY]), None);
    assert_eq!(descriptor_claim(&body[..DESCRIPTOR_ASK_BODY]), None);
    let mut longer = body.clone();
    longer.push(0);
    assert_eq!(descriptor_claim(&longer), None);
    let mut empty_range = body.clone();
    empty_range[17..25].copy_from_slice(&0u64.to_le_bytes());
    assert_eq!(descriptor_claim(&empty_range), None);
    assert_eq!(
      descriptor_handle_answer(0x0102_0304_0506_0708),
      [0, 0, 0, 0, 8, 8, 7, 6, 5, 4, 3, 2, 1]
    );
  }

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

  /// Do: parse a DESCRIPTOR ask as Shards' device sends it today (node and writable byte), the agreed ranged form, and
  /// the forms the protocol does not have (cut short, a stray byte, a range of nothing, a range whose end overflows).
  /// Expect the two real forms read field for field, and every other refused; and the descriptor answer's header to be
  /// kind zero with an empty payload. Serves Shards part 3, step 2.
  #[test]
  fn descriptor_asks_parse_in_both_forms_and_hostile_ones_are_refused() {
    let mut plain = 7u64.to_le_bytes().to_vec();
    plain.push(1);
    assert_eq!(
      descriptor_ask(&plain),
      Some(DescriptorAsk {
        node: 7,
        writable: true,
        range: None
      })
    );
    let mut ranged = 9u64.to_le_bytes().to_vec();
    ranged.push(0);
    ranged.extend_from_slice(&(4u64 << 20).to_le_bytes());
    ranged.extend_from_slice(&(2u64 << 20).to_le_bytes());
    assert_eq!(
      descriptor_ask(&ranged),
      Some(DescriptorAsk {
        node: 9,
        writable: false,
        range: Some((4 << 20, 2 << 20))
      })
    );
    assert_eq!(descriptor_ask(&plain[..8]), None, "cut short");
    let mut stray = plain.clone();
    stray.push(0);
    assert_eq!(
      descriptor_ask(&stray),
      None,
      "a length the protocol does not have"
    );
    let mut nothing = ranged.clone();
    nothing.splice(17..25, 0u64.to_le_bytes());
    assert_eq!(descriptor_ask(&nothing), None, "a range of no bytes");
    let mut overflowing = ranged.clone();
    overflowing.splice(9..17, (u64::MAX - 1).to_le_bytes());
    assert_eq!(
      descriptor_ask(&overflowing),
      None,
      "a range whose end overflows"
    );
    assert_eq!(descriptor_answer(), [0, 0, 0, 0, 0]);
  }
}
