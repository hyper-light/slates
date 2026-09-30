//! The slot and the ring (§4.7 `Slot`, `Ring`).
//!
//! Format, one slot, 64 bytes: the sequence word (8), the kind (2), the payload length (2),
//! the request id word (8: client in the high half, sequence in the low half, as
//! `slates_wire::RequestId`), the payload (40), padding (4). One cache line, so a request or
//! a reply is one line transfer between cores (§2.2 of the design). A payload that does not
//! fit names a range of the client's bulk region instead (`SlotKind::Bulk`).

use std::sync::atomic::{AtomicU64, Ordering};

use slates_mem::{RunId, SharedObject, SpanId, SpanRun, Width, WordRun, Words};

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
/// Format: a slot's body after its sequence word — kind, length, request word and payload — copied in
/// and out whole (AUD-29-09: the sequence word is atomic, the body plain, and they never meet).
const BODY_BYTES: usize = AT_PAYLOAD + PAYLOAD_BYTES - AT_KIND;
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

/// A slot's body as the producer writes it: kind, length, request word, payload zero-padded to its field.
fn encode_body(slot: &Slot) -> [u8; BODY_BYTES] {
  let mut body = [0u8; BODY_BYTES];
  let len = u16::try_from(slot.payload.len()).unwrap_or(u16::MAX);
  let fields: [(usize, &[u8]); 4] = [
    (AT_KIND, &slot.kind.word().to_le_bytes()),
    (AT_LEN, &len.to_le_bytes()),
    (AT_REQUEST, &slot.request.to_le_bytes()),
    (AT_PAYLOAD, &slot.payload),
  ];
  for (at, bytes) in fields {
    let start = at.saturating_sub(AT_KIND);
    if let Some(field) = body.get_mut(start..start.saturating_add(bytes.len())) {
      field.copy_from_slice(bytes);
    }
  }
  body
}

/// A slot's body as the consumer reads it; a hostile kind or length is a typed refusal.
fn decode_body(body: &[u8; BODY_BYTES]) -> Result<Slot, IpcError> {
  // Every field lies inside the body by the format's constants; a missing one is the one refusal below,
  // built only when it happens (an unused refusal would be built and dropped on every pop).
  let field = |at: usize, len: usize| {
    let start = at.saturating_sub(AT_KIND);
    body.get(start..start.saturating_add(len))
  };
  let word16 = |at: usize| {
    field(at, size_of::<u16>()).map(|bytes| {
      u16::from_le_bytes([
        bytes.first().copied().unwrap_or(0),
        bytes.get(1).copied().unwrap_or(0),
      ])
    })
  };
  let past = || IpcError::BadSlot {
    reason: "a field past the slot",
  };
  let kind = SlotKind::from_word(word16(AT_KIND).ok_or_else(past)?)?;
  let len = usize::from(word16(AT_LEN).ok_or_else(past)?);
  if len > PAYLOAD_BYTES {
    return Err(IpcError::BadSlot {
      reason: "length past the payload",
    });
  }
  let mut request = [0u8; size_of::<u64>()];
  request.copy_from_slice(field(AT_REQUEST, size_of::<u64>()).ok_or_else(past)?);
  Ok(Slot {
    kind,
    request: u64::from_le_bytes(request),
    payload: field(AT_PAYLOAD, len).ok_or_else(past)?.to_vec(),
  })
}

/// A ring of slots inside a shared object, at a byte offset: one producer, one consumer, in
/// different processes.
#[derive(Clone, Copy, Debug)]
pub struct Ring {
  /// The ring's offset in the object.
  offset: usize,
  /// Slots (a power of two).
  slots: usize,
  /// Its words' runs, resolved in the object it lives in ([`Ring::resolved`]), so each word is reached in
  /// constant time; `None` until then, when each access resolves them (the slow path).
  ids: Option<RingIds>,
}

/// A ring's resolved runs: its tail hint, its head hint, its slots' sequence words.
#[derive(Clone, Copy, Debug)]
struct RingIds {
  tail: RunId,
  head: RunId,
  seq: RunId,
  body: SpanId,
}

impl Ring {
  /// The bytes a ring of `slots` takes.
  pub fn bytes(slots: usize) -> usize {
    RING_HEADER_BYTES.saturating_add(slots.saturating_mul(SLOT_BYTES))
  }

  /// A ring at `offset` with `slots` slots.
  pub fn at(offset: usize, slots: usize) -> Ring {
    Ring {
      offset,
      slots,
      ids: None,
    }
  }

  /// The ring's three runs: tail hint, head hint, sequence words.
  fn runs(&self) -> [WordRun; 3] {
    [
      WordRun::one(self.offset.saturating_add(AT_TAIL_HINT), Width::U64),
      WordRun::one(self.offset.saturating_add(AT_HEAD_HINT), Width::U64),
      WordRun::strided(
        self
          .offset
          .saturating_add(RING_HEADER_BYTES)
          .saturating_add(AT_SEQ),
        SLOT_BYTES,
        self.slots,
        Width::U64,
      ),
    ]
  }

  /// This ring with its runs resolved in `object`, the object it lives in: every word from here on is
  /// reached in constant time, never searched for (measured 2026-09-30: the search cost ~880 of 1,186
  /// instructions a push and a pop took).
  pub fn resolved(self, object: &SharedObject) -> Result<Ring, IpcError> {
    Ok(Ring {
      ids: Some(self.resolve_ids(object)?),
      ..self
    })
  }

  /// The ring's slot bodies: each slot after its sequence word, a plain span owned by one side at a time.
  fn bodies(&self) -> SpanRun {
    SpanRun::strided(
      self
        .offset
        .saturating_add(RING_HEADER_BYTES)
        .saturating_add(AT_KIND),
      SLOT_BYTES,
      self.slots,
      BODY_BYTES,
    )
  }

  fn resolve_ids(&self, object: &SharedObject) -> Result<RingIds, IpcError> {
    let [tail, head, seq] = self.runs();
    Ok(RingIds {
      tail: object.resolve(&tail)?,
      head: object.resolve(&head)?,
      seq: object.resolve(&seq)?,
      body: object.resolve_span(&self.bodies())?,
    })
  }

  /// The ring's resolved runs: kept, or resolved now.
  fn ids(&self, object: &SharedObject) -> Result<RingIds, IpcError> {
    match self.ids {
      Some(ids) => Ok(ids),
      None => self.resolve_ids(object),
    }
  }

  fn tail_hint<'o>(&self, object: &'o SharedObject) -> Result<&'o AtomicU64, IpcError> {
    Ok(object.run_u64(self.ids(object)?.tail, 0)?)
  }

  fn head_hint<'o>(&self, object: &'o SharedObject) -> Result<&'o AtomicU64, IpcError> {
    Ok(object.run_u64(self.ids(object)?.head, 0)?)
  }

  /// Slots.
  pub fn slots(&self) -> usize {
    self.slots
  }

  /// The ring's declared atomic words (AUD-29-09): the two hints and every slot's sequence word; each
  /// slot's body after its sequence word is a declared plain span, owned by one side at a time.
  pub fn words(&self) -> Words {
    self
      .runs()
      .into_iter()
      .fold(Words::new(), |words, run| words.with(run))
      .with_span(self.bodies())
  }

  /// The position of message `index` in the ring.
  fn position(&self, index: u64) -> usize {
    let slots = u64::try_from(self.slots.max(1)).unwrap_or(1);
    usize::try_from(index.checked_rem(slots).unwrap_or(0)).unwrap_or(0)
  }

  fn seq<'o>(&self, object: &'o SharedObject, index: u64) -> Result<&'o AtomicU64, IpcError> {
    Ok(object.run_u64(self.ids(object)?.seq, self.position(index))?)
  }

  /// Initializes every slot's sequence to its index (free) and the hints to zero; the
  /// creator does this once.
  pub fn init(&self, object: &SharedObject) -> Result<(), IpcError> {
    for i in 0..u64::try_from(self.slots).unwrap_or(0) {
      self.seq(object, i)?.store(i, Ordering::Release);
    }
    self.tail_hint(object)?.store(0, Ordering::Release);
    self.head_hint(object)?.store(0, Ordering::Release);
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
    let body = self.ids(object)?.body;
    object.write_span(body, self.position(index), &encode_body(slot))?;
    self
      .seq(object, index)?
      .store(index.wrapping_add(1), Ordering::Release);
    self
      .tail_hint(object)?
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
    let mut body = [0u8; BODY_BYTES];
    object.read_span(self.ids(object)?.body, self.position(index), &mut body)?;
    let decoded = decode_body(&body);
    self.seq(object, index)?.store(
      index.wrapping_add(u64::try_from(self.slots).unwrap_or(0)),
      Ordering::Release,
    );
    self
      .head_hint(object)?
      .store(index.wrapping_add(1), Ordering::Release);
    decoded.map(Some)
  }

  /// Messages the producer has published that the consumer has not taken, by the hints.
  pub fn depth(&self, object: &SharedObject) -> Result<u64, IpcError> {
    let tail = self.tail_hint(object)?.load(Ordering::Acquire);
    let head = self.head_hint(object)?.load(Ordering::Acquire);
    Ok(tail.wrapping_sub(head))
  }
}
