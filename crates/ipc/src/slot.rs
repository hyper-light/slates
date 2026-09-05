//! The slot and the ring (§4.7 `Slot`, `Ring`).
//!
//! Format, one slot, 64 bytes: the sequence word (8), the kind (2), the payload length (2),
//! the request id word (8: client in the high half, sequence in the low half, as
//! `slates_wire::RequestId`), the payload (40), padding (4). One cache line, so a request or
//! a reply is one line transfer between cores (§2.2 of the design). A payload that does not
//! fit names a range of the client's bulk region instead (`SlotKind::Bulk`).

use std::sync::atomic::{AtomicU64, Ordering};

use slates_mem::SharedObject;

use crate::error::IpcError;

/// Format: a slot's size, one cache line.
pub const SLOT_BYTES: usize = 64;
/// Format: the sequence word's offset in a slot.
const AT_SEQ: usize = 0;
/// Format: the kind's offset.
const AT_KIND: usize = 8;
/// Format: the length's offset.
const AT_LEN: usize = 10;
/// Format: the request word's offset.
const AT_REQUEST: usize = 12;
/// Format: the payload's offset.
const AT_PAYLOAD: usize = 20;
/// Format: the payload bytes a slot carries.
pub const PAYLOAD_BYTES: usize = 40;
/// Format: the ring's words before its slots: the producer's tail hint and the consumer's head
/// hint, each on its own cache line (the slots' sequences carry the protocol; the hints let
/// the other side see depth without scanning).
pub const RING_HEADER_BYTES: usize = 128;
/// Format: the tail hint's offset.
const AT_TAIL_HINT: usize = 0;
/// Format: the head hint's offset.
const AT_HEAD_HINT: usize = 64;

/// What a slot carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotKind {
  /// A request or reply whose payload is inline.
  Inline,
  /// A request or reply whose payload sits in the bulk region: the payload holds the offset
  /// and length as two little-endian `u64`s.
  Bulk,
  /// A cancellation of the request named by the request word.
  Cancel,
  /// A heartbeat (a client's liveness; no reply).
  Heartbeat,
}

/// Format: the kind words, in the order the design lists them.
const KIND_INLINE: u16 = 1;
/// Format: see `KIND_INLINE`.
const KIND_BULK: u16 = 2;
/// Format: see `KIND_INLINE`.
const KIND_CANCEL: u16 = 3;
/// Format: see `KIND_INLINE`.
const KIND_HEARTBEAT: u16 = 4;

impl SlotKind {
  fn word(self) -> u16 {
    match self {
      SlotKind::Inline => KIND_INLINE,
      SlotKind::Bulk => KIND_BULK,
      SlotKind::Cancel => KIND_CANCEL,
      SlotKind::Heartbeat => KIND_HEARTBEAT,
    }
  }

  fn from_word(word: u16) -> Result<SlotKind, IpcError> {
    match word {
      KIND_INLINE => Ok(SlotKind::Inline),
      KIND_BULK => Ok(SlotKind::Bulk),
      KIND_CANCEL => Ok(SlotKind::Cancel),
      KIND_HEARTBEAT => Ok(SlotKind::Heartbeat),
      _ => Err(IpcError::BadSlot {
        reason: "unknown kind",
      }),
    }
  }
}

/// A slot's contents, as read or written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot {
  /// The kind.
  pub kind: SlotKind,
  /// The request id word.
  pub request: u64,
  /// The payload.
  pub payload: Vec<u8>,
}

impl Slot {
  /// An inline slot; refuses a payload the slot cannot carry.
  pub fn inline(request: u64, payload: &[u8]) -> Result<Slot, IpcError> {
    if payload.len() > PAYLOAD_BYTES {
      return Err(IpcError::PayloadTooLarge {
        offered: payload.len(),
        capacity: PAYLOAD_BYTES,
      });
    }
    Ok(Slot {
      kind: SlotKind::Inline,
      request,
      payload: payload.to_vec(),
    })
  }
}

/// A ring of slots inside a shared object, at a byte offset: one producer, one consumer, in
/// different processes.
#[derive(Clone, Copy, Debug)]
pub struct Ring {
  /// The ring's offset in the object.
  offset: usize,
  /// Slots (a power of two).
  slots: usize,
}

impl Ring {
  /// The bytes a ring of `slots` takes.
  pub fn bytes(slots: usize) -> usize {
    RING_HEADER_BYTES.saturating_add(slots.saturating_mul(SLOT_BYTES))
  }

  /// A ring at `offset` with `slots` slots.
  pub fn at(offset: usize, slots: usize) -> Ring {
    Ring { offset, slots }
  }

  /// Slots.
  pub fn slots(&self) -> usize {
    self.slots
  }

  fn slot_offset(&self, index: u64) -> usize {
    let position =
      usize::try_from(index % u64::try_from(self.slots.max(1)).unwrap_or(1)).unwrap_or(0);
    self.offset + RING_HEADER_BYTES + position * SLOT_BYTES
  }

  fn seq<'o>(&self, object: &'o SharedObject, index: u64) -> Result<&'o AtomicU64, IpcError> {
    Ok(object.atomic_u64(self.slot_offset(index) + AT_SEQ)?)
  }

  /// Initializes every slot's sequence to its index (free) and the hints to zero; the
  /// creator does this once.
  pub fn init(&self, object: &SharedObject) -> Result<(), IpcError> {
    for i in 0..u64::try_from(self.slots).unwrap_or(0) {
      self.seq(object, i)?.store(i, Ordering::Release);
    }
    object
      .atomic_u64(self.offset + AT_TAIL_HINT)?
      .store(0, Ordering::Release);
    object
      .atomic_u64(self.offset + AT_HEAD_HINT)?
      .store(0, Ordering::Release);
    Ok(())
  }

  /// Whether slot `index` is free for the producer (its sequence equals the index).
  pub fn can_push(&self, object: &SharedObject, index: u64) -> Result<bool, IpcError> {
    Ok(self.seq(object, index)?.load(Ordering::Acquire) == index)
  }

  /// The producer writes `slot` at `index` and releases it; refuses `RingFull` when the
  /// consumer has not released the slot.
  pub fn push(&self, object: &mut SharedObject, index: u64, slot: &Slot) -> Result<(), IpcError> {
    if slot.payload.len() > PAYLOAD_BYTES {
      return Err(IpcError::PayloadTooLarge {
        offered: slot.payload.len(),
        capacity: PAYLOAD_BYTES,
      });
    }
    if !self.can_push(object, index)? {
      return Err(IpcError::RingFull);
    }
    let at = self.slot_offset(index);
    let bytes = object.bytes_mut();
    bytes[at + AT_KIND..at + AT_KIND + 2].copy_from_slice(&slot.kind.word().to_le_bytes());
    let len = u16::try_from(slot.payload.len()).unwrap_or(u16::MAX);
    bytes[at + AT_LEN..at + AT_LEN + 2].copy_from_slice(&len.to_le_bytes());
    bytes[at + AT_REQUEST..at + AT_REQUEST + 8].copy_from_slice(&slot.request.to_le_bytes());
    bytes[at + AT_PAYLOAD..at + AT_PAYLOAD + PAYLOAD_BYTES].fill(0);
    bytes[at + AT_PAYLOAD..at + AT_PAYLOAD + slot.payload.len()].copy_from_slice(&slot.payload);
    self
      .seq(object, index)?
      .store(index.wrapping_add(1), Ordering::Release);
    object
      .atomic_u64(self.offset + AT_TAIL_HINT)?
      .store(index.wrapping_add(1), Ordering::Release);
    Ok(())
  }

  /// Whether slot `index` holds a message for the consumer.
  pub fn can_pop(&self, object: &SharedObject, index: u64) -> Result<bool, IpcError> {
    Ok(self.seq(object, index)?.load(Ordering::Acquire) == index.wrapping_add(1))
  }

  /// The consumer reads the slot at `index` and releases it back to the producer; `None`
  /// when nothing is there. A hostile kind or length is a typed refusal and the slot is
  /// released anyway (a bad message never wedges the ring).
  pub fn pop(&self, object: &SharedObject, index: u64) -> Result<Option<Slot>, IpcError> {
    if !self.can_pop(object, index)? {
      return Ok(None);
    }
    let at = self.slot_offset(index);
    let bytes = object.bytes();
    let kind = u16::from_le_bytes([bytes[at + AT_KIND], bytes[at + AT_KIND + 1]]);
    let len = usize::from(u16::from_le_bytes([
      bytes[at + AT_LEN],
      bytes[at + AT_LEN + 1],
    ]));
    let mut request = [0u8; size_of::<u64>()];
    request.copy_from_slice(&bytes[at + AT_REQUEST..at + AT_REQUEST + 8]);
    let decoded = SlotKind::from_word(kind).and_then(|kind| {
      if len > PAYLOAD_BYTES {
        return Err(IpcError::BadSlot {
          reason: "length past the payload",
        });
      }
      Ok(Slot {
        kind,
        request: u64::from_le_bytes(request),
        payload: bytes[at + AT_PAYLOAD..at + AT_PAYLOAD + len].to_vec(),
      })
    });
    self.seq(object, index)?.store(
      index.wrapping_add(u64::try_from(self.slots).unwrap_or(0)),
      Ordering::Release,
    );
    object
      .atomic_u64(self.offset + AT_HEAD_HINT)?
      .store(index.wrapping_add(1), Ordering::Release);
    decoded.map(Some)
  }

  /// Messages the producer has published that the consumer has not taken, by the hints.
  pub fn depth(&self, object: &SharedObject) -> Result<u64, IpcError> {
    let tail = object
      .atomic_u64(self.offset + AT_TAIL_HINT)?
      .load(Ordering::Acquire);
    let head = object
      .atomic_u64(self.offset + AT_HEAD_HINT)?
      .load(Ordering::Acquire);
    Ok(tail.wrapping_sub(head))
  }
}
