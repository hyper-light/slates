//! The slot and the ring (§4.7 `Slot`, `Ring`).
//!
//! Format, one slot, 64 bytes: the sequence word (8), the kind (2), the payload length (2),
//! the request id word (8: client in the high half, sequence in the low half, as
//! `slates_wire::RequestId`), the payload (40), the producer's stamp (4). One cache line, so a
//! request or a reply is one line transfer between cores (§2.2 of the design). A payload that
//! does not fit names a range of the client's bulk region instead (`SlotKind::Bulk`).
//!
//! **The stamp (A-559).** The producer stamps each slot as it writes it: the low 32 bits of the
//! host's monotonic clock (`slates_machine::clock::monotonic_ns`, the clock the daemon stamps its
//! spans in), with the lowest bit set, so a slot no producer stamped reads zero ([`stamp_now`]).
//! The client's requests and the daemon's replies are stamped alike, so whoever takes a slot
//! knows when it landed, not only when it was taken; [`landed_ns`] widens a stamp back to the
//! clock's full reading from any reading taken within 2³² ns (about 4.3 s) after it. A harness
//! splits a latency by the two: everything before its request landed and after its reply did is
//! its own time (`slates_machine::attribution`). Before A-559 the four bytes were padding, so a
//! producer of that layout writes zero, which reads as no stamp.

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
/// Format: the producer's stamp's offset, right after the payload (A-559).
const AT_STAMP: usize = AT_PAYLOAD + PAYLOAD_BYTES;
/// Format: the stamp's width.
const STAMP_BYTES: usize = size_of::<u32>();
/// Format: a slot's body after its sequence word — kind, length, request word, payload and stamp — copied
/// in and out whole (AUD-29-09: the sequence word is atomic, the body plain, and they never meet).
const BODY_BYTES: usize = AT_STAMP + STAMP_BYTES - AT_KIND;
/// Format: the bits of a monotonic reading a stamp keeps, its low 32.
const STAMP_BITS: u64 = 0xFFFF_FFFF;
/// Format: the bit every stamp sets, so a zero stamp is a slot no producer stamped.
const STAMPED: u32 = 1;
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
  /// The producer's stamp, as [`Ring::peek`] read it (module docs, "The stamp"): zero for a slot no producer stamped.
  /// A slot to be pushed carries zero; [`Ring::push`] writes the stamp it is given.
  pub stamp: u32,
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
      stamp: 0,
    })
  }
}

/// The stamp a producer writes into the slot it pushes now (module docs, "The stamp"): the low 32 bits of the host's
/// monotonic clock, with the lowest bit set.
pub fn stamp_now() -> u32 {
  u32::try_from(slates_machine::clock::monotonic_ns() & STAMP_BITS).map_or(u32::MAX, |low| low | STAMPED)
}

/// When a slot stamped `stamp` landed, on the host's monotonic clock, given `by_ns`, a reading of that clock taken at
/// or after the landing and less than 2³² ns after it (module docs, "The stamp"): the latest instant at or before `by_ns`
/// whose low 32 bits are the stamp's, to within the one nanosecond the stamp's set bit may add. `None` for a slot no
/// producer stamped, or a landing that would precede the clock's start.
pub fn landed_ns(stamp: u32, by_ns: u64) -> Option<u64> {
  if stamp == 0 {
    return None;
  }
  // Both compared without the set bit, which the stamp may have added: a reading taken in the very nanosecond of the
  // landing then widens to it rather than to a whole wrap before.
  let low = u32::try_from(by_ns & STAMP_BITS).ok()? & !STAMPED;
  let back = u64::from(low.wrapping_sub(stamp & !STAMPED));
  by_ns.checked_sub(back)
}

/// A slot's body as the producer writes it: kind, length, request word, payload zero-padded to its field, and the
/// producer's `stamp`.
fn encode_body(slot: &Slot, stamp: u32) -> [u8; BODY_BYTES] {
  let mut body = [0u8; BODY_BYTES];
  let len = u16::try_from(slot.payload.len()).unwrap_or(u16::MAX);
  let fields: [(usize, &[u8]); 5] = [
    (AT_KIND, &slot.kind.word().to_le_bytes()),
    (AT_LEN, &len.to_le_bytes()),
    (AT_REQUEST, &slot.request.to_le_bytes()),
    (AT_PAYLOAD, &slot.payload),
    (AT_STAMP, &stamp.to_le_bytes()),
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
  let mut stamp = [0u8; STAMP_BYTES];
  stamp.copy_from_slice(field(AT_STAMP, STAMP_BYTES).ok_or_else(past)?);
  Ok(Slot {
    kind,
    request: u64::from_le_bytes(request),
    payload: field(AT_PAYLOAD, len).ok_or_else(past)?.to_vec(),
    stamp: u32::from_le_bytes(stamp),
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

  /// The producer writes `slot` at `index`, stamped `stamp` ([`stamp_now`] as it writes; module docs, "The stamp"), and
  /// releases it; refuses `RingFull` when the consumer has not released the slot.
  pub fn push(
    &self,
    object: &mut SharedObject,
    index: u64,
    slot: &Slot,
    stamp: u32,
  ) -> Result<(), IpcError> {
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
    object.write_span(body, self.position(index), &encode_body(slot, stamp))?;
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
    let Some(peeked) = self.peek(object, index)? else {
      return Ok(None);
    };
    self.release(object, index)?;
    peeked.map(Some)
  }

  /// The consumer reads the slot at `index` without releasing it: `None` when nothing is there, else the slot or the
  /// typed refusal of a hostile one. The producer cannot reuse the slot, or the bulk chunk its position names, until
  /// [`Ring::release`]: a consumer copies a bulk body out first, since a released slot's chunk is the next message's at
  /// that position (`protocol::chunk`), which a producer sending on writes at once.
  pub fn peek(
    &self,
    object: &SharedObject,
    index: u64,
  ) -> Result<Option<Result<Slot, IpcError>>, IpcError> {
    if !self.can_pop(object, index)? {
      return Ok(None);
    }
    let mut body = [0u8; BODY_BYTES];
    object.read_span(self.ids(object)?.body, self.position(index), &mut body)?;
    Ok(Some(decode_body(&body)))
  }

  /// Releases the slot at `index`, which [`Ring::peek`] read, back to the producer.
  pub fn release(&self, object: &SharedObject, index: u64) -> Result<(), IpcError> {
    self.seq(object, index)?.store(
      index.wrapping_add(u64::try_from(self.slots).unwrap_or(0)),
      Ordering::Release,
    );
    self
      .head_hint(object)?
      .store(index.wrapping_add(1), Ordering::Release);
    Ok(())
  }

  /// Messages the producer has published that the consumer has not taken, by the hints.
  pub fn depth(&self, object: &SharedObject) -> Result<u64, IpcError> {
    let tail = self.tail_hint(object)?.load(Ordering::Acquire);
    let head = self.head_hint(object)?.load(Ordering::Acquire);
    Ok(tail.wrapping_sub(head))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The stamp a producer writing at `at` wrote ([`stamp_now`]'s rule over a chosen reading).
  fn stamp_at(at: u64) -> u32 {
    u32::try_from(at & STAMP_BITS).unwrap() | STAMPED
  }

  /// A-559 (the stamp). Do: widen the stamps of landings at even and odd readings, early and late in a wrap of the low 32
  /// bits, from readings taken in the same nanosecond, 300 µs, 29 ms and a nanosecond short of a whole wrap after.
  /// Expect: each widens back to its landing, within the one nanosecond the stamp's set bit may add.
  #[test]
  fn a_stamp_widens_back_to_its_landing_from_any_reading_within_a_wrap() {
    let wrap = STAMP_BITS + 1;
    for landed in [
      7 * wrap + 1_000,
      7 * wrap + 1_001,
      9 * wrap + wrap - 2,
      9 * wrap + wrap - 1,
    ] {
      for after in [0, 1, 300_000, 29_000_000, wrap - 2] {
        let widened = landed_ns(stamp_at(landed), landed + after).unwrap();
        assert!(
          widened.abs_diff(landed) <= 1,
          "landed {landed}, read {after} ns after: widened to {widened}"
        );
      }
    }
  }

  /// A-559. Do: widen a zero stamp, the reading of a slot no producer stamped (a producer of the layout before A-559,
  /// whose padding was zero), and a stamp whose landing would be before the clock started. Expect: no landing for
  /// either.
  #[test]
  fn an_unstamped_slot_or_a_landing_before_the_clock_widens_to_none() {
    assert_eq!(landed_ns(0, 5_000_000_000), None);
    assert_eq!(landed_ns(stamp_at(100), 5), None);
  }

  /// A-559. Do: write a reply's body with a stamp and read it back, and read back a body written with a zero stamp.
  /// Expect: the stamp and every other field come back as written.
  #[test]
  fn a_slot_carries_its_producers_stamp_from_the_writing_to_the_reading() {
    let slot = Slot::inline(0x0001_0002_0000_0003, b"answered").unwrap();
    let stamp = stamp_at(123_456_789);
    let read = decode_body(&encode_body(&slot, stamp)).unwrap();
    assert_eq!(read, Slot { stamp, ..slot.clone() });
    let unstamped = decode_body(&encode_body(&slot, 0)).unwrap();
    assert_eq!(unstamped.stamp, 0);
  }

  /// A-559. Do: take a stamp now, between two readings of the host's monotonic clock. Expect: it is marked stamped and
  /// widens to an instant between the two readings, within the set bit's nanosecond.
  #[test]
  fn a_stamp_taken_now_lands_between_the_readings_around_it() {
    let before = slates_machine::clock::monotonic_ns();
    let stamp = stamp_now();
    let after = slates_machine::clock::monotonic_ns();
    assert_eq!(stamp & STAMPED, STAMPED);
    let landed = landed_ns(stamp, after).unwrap();
    assert!(
      landed + 1 >= before && landed <= after + 1,
      "stamped {landed}, between {before} and {after}"
    );
  }
}
