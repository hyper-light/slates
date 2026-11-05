//! The retained receive journal (A-198, A-215). One owner receives a batch into held pages, preserves a response
//! before sending it, and sends every response but a batch's last alone. The last rides the call that receives the next
//! batch into the journal's other buffer (A-215). A single-owner state byte names the buffer a batch may be in, and
//! whether its last response was handed off with a receive into the other. Readers inspect it only after the previous
//! owner ended; this is crash publication, not a shared atomic or lock on a request path. A published saved response is
//! replayed first, so a second crash cannot lose an already-issued open by overwriting its only complete response.
//!
//! After a death in a hand-off, the other buffer decides. WinFsp v2.1 finishes a call's responses before it delivers
//! any request (`src/sys/volume.c:1079–1271`) and writes each request into these pages before the IRP is processing
//! (1224, 1228). So a request there proves the saved response was consumed: nothing of the old batch is answered
//! again, and the delivered batch is settled. An empty buffer proves nothing was delivered: the old batch is settled
//! as A-198 settles it, since a reused address can only be pending, which the driver never matches (`src/sys/ioq.c`
//! 673–680). Every mark is stored behind a compiler fence, since a kill can fall between any two stores.

use super::{
  BATCH_BYTES, RESPONSE_BASE, RESPONSE_MAX, Refusal, Request, put, read_u32, read_u64, requests,
};
use slates_wire::crc32c::crc32c;
use std::collections::VecDeque;
use std::sync::atomic::{Ordering, compiler_fence};

/// Format: a phase byte and seven reserved zero bytes keep the direct-output request buffers eight-byte aligned.
const PHASE_BYTES: usize = size_of::<u64>();
/// Format: saved-response state and three reserved bytes, length, CRC32C and four reserved bytes.
const SAVED_HEADER: usize = size_of::<u32>() * 4;
/// Shape: two receive buffers, so the call that sends a batch's last response receives into the one not holding that
/// batch, and the batch stays readable for recovery (A-215).
const SIDES: usize = 2;
/// Format: the fixed capture region appended after the aligned open-ledger frames: the phase, both receive buffers
/// and the saved response.
pub const BYTES: usize = PHASE_BYTES + BATCH_BYTES * SIDES + SAVED_HEADER + RESPONSE_MAX;
/// Format: no batch is in hand; every request taken was answered and acknowledged.
const IDLE: u8 = 0;
/// Format: a receive into the first buffer may have taken requests, and its batch is in hand.
const RECEIVING_FIRST: u8 = 1;
/// Format: as RECEIVING_FIRST, for the second buffer.
const RECEIVING_SECOND: u8 = 2;
/// Format: the first buffer's batch has every response sent but its last, which is saved and was handed to the call
/// receiving into the second buffer (A-215).
const HANDING_OFF_FIRST: u8 = 3;
/// Format: as HANDING_OFF_FIRST, from the second buffer into the first.
const HANDING_OFF_SECOND: u8 = 4;
/// Format: one marks a complete saved response; zero marks an unfinished save. It is written only after its body/CRC.
const PUBLISHED: u8 = 1;
/// Format: the saved response's length follows its state word, and its CRC32C follows the length.
const LENGTH_AT: usize = size_of::<u32>();
/// Format: see LENGTH_AT.
const CRC_AT: usize = LENGTH_AT + size_of::<u32>();
/// Format: the last reserved word follows the CRC32C in the saved-response header.
const RESERVED_AT: usize = CRC_AT + size_of::<u32>();

/// One of the journal's two receive buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
  First,
  Second,
}

impl Side {
  fn other(self) -> Self {
    match self {
      Self::First => Self::Second,
      Self::Second => Self::First,
    }
  }
}

/// The journal's state, decoded from its phase byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
  Idle,
  Receiving(Side),
  HandingOff(Side),
}

impl Phase {
  fn decode(byte: u8) -> Result<Self, Refusal> {
    match byte {
      IDLE => Ok(Self::Idle),
      RECEIVING_FIRST => Ok(Self::Receiving(Side::First)),
      RECEIVING_SECOND => Ok(Self::Receiving(Side::Second)),
      HANDING_OFF_FIRST => Ok(Self::HandingOff(Side::First)),
      HANDING_OFF_SECOND => Ok(Self::HandingOff(Side::Second)),
      _ => Err(Refusal::State),
    }
  }

  fn encode(self) -> u8 {
    match self {
      Self::Idle => IDLE,
      Self::Receiving(Side::First) => RECEIVING_FIRST,
      Self::Receiving(Side::Second) => RECEIVING_SECOND,
      Self::HandingOff(Side::First) => HANDING_OFF_FIRST,
      Self::HandingOff(Side::Second) => HANDING_OFF_SECOND,
    }
  }
}

/// Where a death left the journal, as a successor finds it. The live oracle counts these to show its kills landed
/// inside the combined call (A-215).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Landing {
  /// No batch in hand.
  Idle,
  /// A batch in hand, or a receive that may have taken one; none of its last response was handed off.
  Receiving,
  /// Inside a combined call that delivered nothing, or before it: the old batch is settled again.
  HandingOffUndelivered,
  /// A combined call delivered: its response was consumed, and the delivered batch is settled.
  HandingOffDelivered,
}

/// Stores a one-byte mark so that every store before it lands first and none after it lands before it. The fences emit
/// no instruction: they stop the compiler from reordering stores to different addresses. A kill can fall between any
/// two of a thread's stores, and the successor sees every store that thread retired (A-215).
fn mark(byte: Option<&mut u8>, value: u8) -> Result<(), Refusal> {
  let byte = byte.ok_or(Refusal::Length)?;
  compiler_fence(Ordering::SeqCst);
  *byte = value;
  compiler_fence(Ordering::SeqCst);
  Ok(())
}

/// Zeroes what a buffer's last batch filled, or all of it when that is unknown (an owner's first use, A-212).
fn clear(buffer: &mut [u8], used: Option<usize>) -> Result<(), Refusal> {
  match used {
    Some(filled) => buffer.get_mut(..filled).ok_or(Refusal::Length)?.fill(0),
    None => buffer.fill(0),
  }
  Ok(())
}

/// What a receive owner knows of its two buffers: the one its next receive fills, and what its last batch in each
/// filled. A new owner knows neither, so its first receive into each buffer clears all of it, since a predecessor may
/// have died part-way through (A-212).
#[derive(Debug)]
pub struct Fill {
  side: Side,
  used: [Option<usize>; SIDES],
}

impl Default for Fill {
  fn default() -> Self {
    Self::new()
  }
}

impl Fill {
  /// An owner's first knowledge: nothing of either buffer.
  pub fn new() -> Self {
    Self {
      side: Side::First,
      used: [None; SIDES],
    }
  }

  fn used(&self, side: Side) -> Option<usize> {
    match side {
      Side::First => self.used.first().copied().flatten(),
      Side::Second => self.used.get(1).copied().flatten(),
    }
  }

  fn record(&mut self, side: Side, filled: usize) -> Result<(), Refusal> {
    let slot = match side {
      Side::First => self.used.first_mut(),
      Side::Second => self.used.get_mut(1),
    };
    *slot.ok_or(Refusal::Length)? = Some(filled);
    Ok(())
  }
}

/// A borrow of one capture region. Its owner is the only thread touching these bytes until it ends or returns them.
pub struct Capture<'a> {
  phase: &'a mut [u8],
  first: &'a mut [u8],
  second: &'a mut [u8],
  saved: &'a mut [u8],
}

/// One batch's buffer, the other buffer, and the journal's phase and saved response, split for a batch's borrow.
struct Parts<'a> {
  phase: &'a mut [u8],
  side: Side,
  frames: &'a mut [u8],
  other: &'a mut [u8],
  saved: &'a mut [u8],
}

impl<'a> Capture<'a> {
  /// Refuses a wrong-sized region or a phase with undefined bits before exposing a receive buffer.
  pub fn new(bytes: &'a mut [u8]) -> Result<Self, Refusal> {
    if bytes.len() != BYTES {
      return Err(Refusal::Length);
    }
    let (phase, body) = bytes
      .split_at_mut_checked(PHASE_BYTES)
      .ok_or(Refusal::Length)?;
    let (first, body) = body
      .split_at_mut_checked(BATCH_BYTES)
      .ok_or(Refusal::Length)?;
    let (second, saved) = body
      .split_at_mut_checked(BATCH_BYTES)
      .ok_or(Refusal::Length)?;
    Phase::decode(phase.first().copied().ok_or(Refusal::Length)?)?;
    if phase
      .get(1..)
      .ok_or(Refusal::Length)?
      .iter()
      .any(|byte| *byte != 0)
    {
      return Err(Refusal::State);
    }
    Ok(Self {
      phase,
      first,
      second,
      saved,
    })
  }

  fn phase(&self) -> Result<Phase, Refusal> {
    Phase::decode(self.phase.first().copied().ok_or(Refusal::Length)?)
  }

  fn buffer(&self, side: Side) -> &[u8] {
    match side {
      Side::First => self.first,
      Side::Second => self.second,
    }
  }

  fn split(self, side: Side) -> Parts<'a> {
    let (frames, other) = match side {
      Side::First => (self.first, self.second),
      Side::Second => (self.second, self.first),
    };
    Parts {
      phase: self.phase,
      side,
      frames,
      other,
      saved: self.saved,
    }
  }

  /// Where a death left this journal. Only a successor asks, after the owner that wrote it has exited.
  pub fn landing(&self) -> Result<Landing, Refusal> {
    Ok(match self.phase()? {
      Phase::Idle => Landing::Idle,
      Phase::Receiving(_) => Landing::Receiving,
      Phase::HandingOff(side) if delivered(self.buffer(side.other())) => {
        Landing::HandingOffDelivered
      }
      Phase::HandingOff(_) => Landing::HandingOffUndelivered,
    })
  }

  /// Clears an already-settled buffer, then records the new receive before lending it to the driver. A dropped
  /// unfinished batch stays receiving, so it cannot be silently replaced by a fresh receive. The buffer is the one
  /// `fill` names, cleared as far as its last batch there filled. Only the saved response's header is cleared: its
  /// state word says whether a body exists, and no reader goes past a body's own length (A-212).
  pub fn begin(self, fill: &Fill) -> Result<Receiving<'a>, Refusal> {
    if self.phase()? != Phase::Idle {
      return Err(Refusal::Unsettled);
    }
    let parts = self.split(fill.side);
    clear(parts.frames, fill.used(parts.side))?;
    parts
      .saved
      .get_mut(..SAVED_HEADER)
      .ok_or(Refusal::Length)?
      .fill(0);
    mark(
      parts.phase.first_mut(),
      Phase::Receiving(parts.side).encode(),
    )?;
    Ok(Receiving { parts })
  }

  /// Takes the retained batch before the first new receive. Reset memory is ignored, including a partially cleared
  /// prior buffer whose responses all reached the driver before its owner recorded reset. A hand-off is decided by
  /// its other buffer (A-215): a delivered batch is taken, its predecessor's saved response cleared first, so that a
  /// request at a reused address can never be matched with it; otherwise the handing batch is taken whole again.
  pub fn resume(self) -> Result<Option<Batch<'a>>, Refusal> {
    match self.phase()? {
      Phase::Idle => Ok(None),
      Phase::Receiving(side) => Batch::checked(self.split(side), BATCH_BYTES).map(Some),
      Phase::HandingOff(side) => {
        let receiving = side.other();
        if !delivered(self.buffer(receiving)) {
          return Batch::checked(self.split(side), BATCH_BYTES).map(Some);
        }
        let parts = self.split(receiving);
        mark(parts.saved.first_mut(), 0)?;
        let mut batch = Batch::checked(parts, BATCH_BYTES)?;
        batch.in_hand()?;
        Ok(Some(batch))
      }
    }
  }

  /// The successful open a successor must keep, because the kernel has or will be sent its context: the saved
  /// response of a batch in hand, or the response a hand-off sent. Every other unconfirmed open is retired (A-198).
  pub fn retained_open(self) -> Result<Option<(u64, u64)>, Refusal> {
    match self.phase()? {
      Phase::Idle => Ok(None),
      Phase::Receiving(side) => Batch::checked(self.split(side), BATCH_BYTES)?.saved_open(),
      Phase::HandingOff(_) => match saved_response(self.saved)? {
        Some(response) => super::opened_context(response),
        None => Ok(None),
      },
    }
  }

  /// After a combined call returns: the hand-off's open has been confirmed, so its saved response is cleared, and the
  /// buffer it received into becomes the batch in hand, or none when the call brought nothing (A-215). `reported` is
  /// the transfer the driver counted.
  pub fn handed_off(self, reported: usize, fill: &mut Fill) -> Result<Batch<'a>, Refusal> {
    if self.phase()? != Phase::HandingOff(fill.side.other()) {
      return Err(Refusal::State);
    }
    let filled = filled(reported)?;
    fill.record(fill.side, filled)?;
    let parts = self.split(fill.side);
    mark(parts.saved.first_mut(), 0)?;
    let mut batch = Batch::checked(parts, filled)?;
    batch.in_hand()?;
    Ok(batch)
  }
}

/// Whether the driver wrote into a buffer cleared before its hand-off: any byte set is a delivered request.
fn delivered(buffer: &[u8]) -> bool {
  buffer.iter().any(|byte| *byte != 0)
}

/// The buffer of one issued receive. The caller must settle its kernel I/O before consuming it as a batch.
pub struct Receiving<'a> {
  parts: Parts<'a>,
}

impl<'a> Receiving<'a> {
  /// The direct-output buffer, borrowed for the lifetime of the synchronous/owned overlapped receive.
  pub fn buffer_mut(&mut self) -> &mut [u8] {
    self.parts.frames
  }
  /// Validates the bytes after kernel I/O completion, preserving their original order and any complete saved result.
  /// `reported` is the transfer the driver counted. Only those bytes are read: [`Capture::begin`] cleared the rest, and
  /// recovery, which knows no count, still checks the whole buffer (A-212).
  pub fn received(self, reported: usize, fill: &mut Fill) -> Result<Batch<'a>, Refusal> {
    let filled = filled(reported)?;
    fill.record(self.parts.side, filled)?;
    Batch::checked(self.parts, filled)
  }
}

/// The bytes of the receive buffer a driver's reported transfer filled: the whole frames it counts. A frame's stride is
/// aligned, so a count that stops at the end of a frame's bytes rounds up over its zero padding.
pub fn filled(reported: usize) -> Result<usize, Refusal> {
  reported
    .checked_next_multiple_of(super::ALIGNMENT)
    .filter(|filled| *filled <= BATCH_BYTES)
    .ok_or(Refusal::Length)
}

/// One received batch. Its original request bytes stay immutable; DLL operations use an aligned working copy.
/// Dropping this value preserves its receive state for the successor. Only the last acknowledged response, or the
/// hand-off of the last response, ends it.
pub struct Batch<'a> {
  phase: &'a mut [u8],
  side: Side,
  other: &'a mut [u8],
  pending: VecDeque<Request<'a>>,
  saved: &'a mut [u8],
  saved_first: Option<usize>,
  current: Option<u64>,
}

impl<'a> Batch<'a> {
  /// The saved successful open whose response may have reached the kernel. It is the only unconfirmed context that
  /// recovery may retain; earlier responses were acknowledged before their saved bytes could be overwritten.
  pub fn saved_open(&self) -> Result<Option<(u64, u64)>, Refusal> {
    match saved_response(self.saved)? {
      Some(response) => super::opened_context(response),
      None => Ok(None),
    }
  }

  fn checked(parts: Parts<'a>, filled: usize) -> Result<Self, Refusal> {
    let frames: &'a [u8] = parts.frames;
    let pending = requests(frames.get(..filled).ok_or(Refusal::Length)?)?;
    let saved = saved_response(parts.saved)?;
    let saved_first = match saved {
      Some(saved) => {
        let hint = read_u64(saved, super::HINT_AT)?;
        let kind = read_u32(saved, super::KIND_AT)?;
        Some(
          pending
            .iter()
            .position(|request| request.hint() == hint && request.kind() == kind)
            .ok_or(Refusal::Hint)?,
        )
      }
      None => None,
    };
    if pending.is_empty() {
      mark(parts.phase.first_mut(), IDLE)?;
    }
    Ok(Self {
      phase: parts.phase,
      side: parts.side,
      other: parts.other,
      pending: pending.into(),
      saved: parts.saved,
      saved_first,
      current: None,
    })
  }

  /// Records this batch's buffer as the one in hand, after its predecessor's saved response was cleared. An empty
  /// batch was already recorded idle by [`Batch::checked`].
  fn in_hand(&mut self) -> Result<(), Refusal> {
    if self.pending.is_empty() {
      return Ok(());
    }
    mark(self.phase.first_mut(), Phase::Receiving(self.side).encode())
  }

  /// Whether no request is left after the current one: its response is the one to hand off.
  pub fn is_last(&self) -> bool {
    self.pending.is_empty() && self.saved_first.is_none()
  }

  /// DIAG: the requests left in this batch.
  pub fn pending_len(&self) -> usize {
    self.pending.len()
  }

  /// The saved response's original request first, then the remaining requests in driver order. A current unacknowledged
  /// request refuses another take, so its response cannot be overwritten while it is still in kernel I/O.
  pub fn next_request(&mut self) -> Result<Option<Request<'a>>, Refusal> {
    if self.current.is_some() {
      return Err(Refusal::Unsettled);
    }
    let request = match self.saved_first.take() {
      Some(index) => Some(self.pending.remove(index).ok_or(Refusal::Hint)?),
      None => self.pending.pop_front(),
    };
    self.current = request.as_ref().map(Request::hint);
    Ok(request)
  }

  /// A complete saved result for this request. Other requests in the same captured batch have no saved result.
  pub fn response_for(&self, request: &Request<'_>) -> Result<Option<&[u8]>, Refusal> {
    if let Some(response) = saved_response(self.saved)?
      && read_u64(response, super::HINT_AT)? == request.hint()
      && read_u32(response, super::KIND_AT)? == request.kind()
    {
      return Ok(Some(response));
    }
    Ok(None)
  }

  /// Publishes a complete bounded response before sending it. A prior saved result for this same request is immutable.
  pub fn record_response(&mut self, request: &Request<'_>, response: &[u8]) -> Result<(), Refusal> {
    if self.current != Some(request.hint()) {
      return Err(Refusal::Unsettled);
    }
    check_response(response)?;
    if read_u64(response, super::HINT_AT)? != request.hint()
      || read_u32(response, super::KIND_AT)? != request.kind()
    {
      return Err(Refusal::Hint);
    }
    if let Some(saved) = self.response_for(request)? {
      return if saved == response {
        Ok(())
      } else {
        Err(Refusal::ResponseChanged)
      };
    }
    mark(self.saved.first_mut(), 0)?;
    let end = SAVED_HEADER
      .checked_add(response.len())
      .ok_or(Refusal::Length)?;
    self
      .saved
      .get_mut(SAVED_HEADER..end)
      .ok_or(Refusal::Length)?
      .copy_from_slice(response);
    put(
      self.saved,
      LENGTH_AT,
      &u32::try_from(response.len())
        .map_err(|_| Refusal::Length)?
        .to_le_bytes(),
    )?;
    put(self.saved, CRC_AT, &crc32c(response).to_le_bytes())?;
    mark(self.saved.first_mut(), PUBLISHED)
  }

  /// The driver accepted this response (or had completed it already), and the owner settled its context effects.
  /// Only the last acknowledgement permits reset and a fresh receive. A foreign or duplicate acknowledgement refuses.
  pub fn acknowledge(&mut self, hint: u64) -> Result<(), Refusal> {
    if self.current != Some(hint) {
      return Err(Refusal::Unsettled);
    }
    let saved = saved_response(self.saved)?.ok_or(Refusal::Unsettled)?;
    if read_u64(saved, super::HINT_AT)? != hint {
      return Err(Refusal::Hint);
    }
    self.current = None;
    if self.pending.is_empty() {
      mark(self.phase.first_mut(), IDLE)?;
    }
    Ok(())
  }

  /// Hands the batch's last response to a call that also receives (A-215). The response must be saved, and this batch
  /// the one in hand. The other buffer is cleared as far as its last batch filled before the hand-off is recorded, so a
  /// successor that finds a byte there knows the driver delivered it. `fill` then names the receiving buffer, which
  /// [`Capture::handed_off`] takes after the call.
  pub fn hand_off(self, request: &Request<'_>, fill: &mut Fill) -> Result<HandOff<'a>, Refusal> {
    if self.current != Some(request.hint()) || !self.is_last() {
      return Err(Refusal::Unsettled);
    }
    if Phase::decode(self.phase.first().copied().ok_or(Refusal::Length)?)?
      != Phase::Receiving(self.side)
    {
      return Err(Refusal::State);
    }
    let saved: &'a [u8] = self.saved;
    let response = saved_response(saved)?.ok_or(Refusal::Unsettled)?;
    if read_u64(response, super::HINT_AT)? != request.hint()
      || read_u32(response, super::KIND_AT)? != request.kind()
    {
      return Err(Refusal::Hint);
    }
    let receiving = self.side.other();
    clear(self.other, fill.used(receiving))?;
    mark(
      self.phase.first_mut(),
      Phase::HandingOff(self.side).encode(),
    )?;
    fill.side = receiving;
    Ok(HandOff {
      response,
      buffer: self.other,
    })
  }
}

/// A batch's last response and the cleared buffer the same call receives into (A-215).
pub struct HandOff<'a> {
  response: &'a [u8],
  buffer: &'a mut [u8],
}

impl HandOff<'_> {
  /// The saved response the call sends, whose effects (a confirmed open) follow the call.
  pub fn response(&self) -> &[u8] {
    self.response
  }

  /// The call's input and direct-output buffer, borrowed until the call has settled.
  pub fn transaction(&mut self) -> (&[u8], &mut [u8]) {
    (self.response, self.buffer)
  }
}

fn saved_response(bytes: &[u8]) -> Result<Option<&[u8]>, Refusal> {
  match bytes.first() {
    Some(0) => return Ok(None),
    Some(&PUBLISHED) => {}
    _ => return Err(Refusal::State),
  }
  if bytes
    .get(1..LENGTH_AT)
    .ok_or(Refusal::Length)?
    .iter()
    .any(|byte| *byte != 0)
    || bytes
      .get(RESERVED_AT..SAVED_HEADER)
      .ok_or(Refusal::Length)?
      .iter()
      .any(|byte| *byte != 0)
  {
    return Err(Refusal::State);
  }
  let length = usize::try_from(read_u32(bytes, LENGTH_AT)?).map_err(|_| Refusal::Length)?;
  if !(RESPONSE_BASE..=RESPONSE_MAX).contains(&length) {
    return Err(Refusal::Length);
  }
  let end = SAVED_HEADER.checked_add(length).ok_or(Refusal::Length)?;
  let response = bytes.get(SAVED_HEADER..end).ok_or(Refusal::Length)?;
  if crc32c(response) != read_u32(bytes, CRC_AT)? {
    return Err(Refusal::Checksum);
  }
  check_response(response)?;
  Ok(Some(response))
}

pub(super) fn check_response(bytes: &[u8]) -> Result<(), Refusal> {
  if !(RESPONSE_BASE..=RESPONSE_MAX).contains(&bytes.len())
    || !bytes.len().is_multiple_of(super::ALIGNMENT)
  {
    return Err(Refusal::Length);
  }
  if super::read_u16(bytes, 0)? != 0 {
    return Err(Refusal::Version);
  }
  if usize::from(super::read_u16(bytes, super::SIZE_AT)?) != bytes.len() {
    return Err(Refusal::Length);
  }
  let kind = read_u32(bytes, super::KIND_AT)?;
  if kind == 0 || kind >= super::KIND_COUNT {
    return Err(Refusal::Kind);
  }
  if read_u64(bytes, super::HINT_AT)? == 0 {
    return Err(Refusal::Hint);
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  //! A driver response and its acknowledgement are distinct actions; a crash between them must keep the saved result.
  #![allow(
    clippy::unwrap_used,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
  )]
  use super::*;
  use std::collections::{BTreeMap, BTreeSet};

  fn reads(hints: &[u64]) -> Vec<u8> {
    let mut bytes = vec![0; BATCH_BYTES];
    for (index, hint) in hints.iter().enumerate() {
      let at = index * super::super::REQUEST_BASE;
      bytes[at + 2..at + 4].copy_from_slice(&88u16.to_le_bytes());
      bytes[at + 4..at + 8].copy_from_slice(&5u32.to_le_bytes());
      bytes[at + 8..at + 16].copy_from_slice(&hint.to_le_bytes());
    }
    bytes
  }

  fn complete(response: &[u8], kernel: &mut BTreeMap<u64, &'static str>) -> Option<&'static str> {
    let hint = u64::from_le_bytes(response[8..16].try_into().unwrap());
    assert_eq!(
      u32::from_le_bytes(response[20..24].try_into().unwrap()),
      0xC000_0120
    );
    kernel.remove(&hint)
  }

  /// A-198 Step 9.4. Do: complete a read, crash before acknowledging it, and leave a recycled address pending.
  /// Expect: recovery sends its exact saved response first, settles the other original read, and only then admits a
  /// fresh receive; the recycled caller is never completed by that replay.
  #[test]
  fn a_response_completed_before_the_crash_is_replayed_before_a_fresh_receive() {
    let mut memory = vec![0; BYTES];
    let mut kernel = BTreeMap::from([(0x1000, "first"), (0x2000, "second")]);
    {
      let mut receive = Capture::new(&mut memory)
        .unwrap()
        .begin(&Fill::new())
        .unwrap();
      receive
        .buffer_mut()
        .copy_from_slice(&reads(&[0x1000, 0x2000]));
      let mut batch = receive.received(BATCH_BYTES, &mut Fill::new()).unwrap();
      let request = batch.next_request().unwrap().unwrap();
      let response = request.cancelled_response().unwrap();
      batch.record_response(&request, &response).unwrap();
      assert_eq!(complete(&response, &mut kernel), Some("first"));
      // The process dies here: no acknowledgement and no next receive.
    }
    assert!(matches!(
      Capture::new(&mut memory).unwrap().begin(&Fill::new()),
      Err(Refusal::Unsettled)
    ));
    let mut pending = BTreeMap::from([(0x1000, "new caller")]);
    {
      let mut batch = Capture::new(&mut memory)
        .unwrap()
        .resume()
        .unwrap()
        .unwrap();
      let request = batch.next_request().unwrap().unwrap();
      let saved = batch.response_for(&request).unwrap().unwrap();
      assert_eq!(complete(saved, &mut kernel), None);
      batch.acknowledge(request.hint()).unwrap();
      let request = batch.next_request().unwrap().unwrap();
      let response = request.cancelled_response().unwrap();
      batch.record_response(&request, &response).unwrap();
      assert_eq!(complete(&response, &mut kernel), Some("second"));
      batch.acknowledge(request.hint()).unwrap();
      assert!(batch.next_request().unwrap().is_none());
    }
    assert!(
      Capture::new(&mut memory)
        .unwrap()
        .begin(&Fill::new())
        .is_ok()
    );
    kernel.append(&mut pending);
    assert_eq!(kernel.into_values().collect::<Vec<_>>(), vec!["new caller"]);
  }

  /// A-198. Do: crash with a complete saved response for the second request, then crash again just after replaying
  /// it. Expect: neither recovery overwrites that result by first replaying an earlier completed request.
  #[test]
  fn a_saved_result_in_the_middle_of_a_batch_survives_a_second_crash() {
    let mut memory = vec![0; BYTES];
    let mut kernel = BTreeMap::from([(0x1000, "first"), (0x2000, "second"), (0x3000, "third")]);
    {
      let mut receive = Capture::new(&mut memory)
        .unwrap()
        .begin(&Fill::new())
        .unwrap();
      receive
        .buffer_mut()
        .copy_from_slice(&reads(&[0x1000, 0x2000, 0x3000]));
      let mut batch = receive.received(BATCH_BYTES, &mut Fill::new()).unwrap();
      let first = batch.next_request().unwrap().unwrap();
      let response = first.cancelled_response().unwrap();
      batch.record_response(&first, &response).unwrap();
      assert_eq!(complete(&response, &mut kernel), Some("first"));
      batch.acknowledge(first.hint()).unwrap();
      let second = batch.next_request().unwrap().unwrap();
      batch
        .record_response(&second, &second.cancelled_response().unwrap())
        .unwrap();
    }
    {
      let mut batch = Capture::new(&mut memory)
        .unwrap()
        .resume()
        .unwrap()
        .unwrap();
      let saved_request = batch.next_request().unwrap().unwrap();
      let saved = batch.response_for(&saved_request).unwrap().unwrap();
      assert_eq!(complete(saved, &mut kernel), Some("second"));
      batch.acknowledge(saved_request.hint()).unwrap();
      // A second death before any other saved frame is overwritten.
    }
    let mut new_pending = BTreeMap::from([(0x2000, "recycled caller")]);
    {
      let mut batch = Capture::new(&mut memory)
        .unwrap()
        .resume()
        .unwrap()
        .unwrap();
      let saved_request = batch.next_request().unwrap().unwrap();
      assert_eq!(
        complete(
          batch.response_for(&saved_request).unwrap().unwrap(),
          &mut kernel
        ),
        None
      );
      batch.acknowledge(saved_request.hint()).unwrap();
      while let Some(request) = batch.next_request().unwrap() {
        let response = request.cancelled_response().unwrap();
        batch.record_response(&request, &response).unwrap();
        let caller = complete(&response, &mut kernel);
        assert!(caller.is_none() || caller == Some("third"));
        batch.acknowledge(request.hint()).unwrap();
      }
    }
    assert!(
      Capture::new(&mut memory)
        .unwrap()
        .begin(&Fill::new())
        .is_ok()
    );
    kernel.append(&mut new_pending);
    assert_eq!(
      kernel.into_values().collect::<Vec<_>>(),
      vec!["recycled caller"]
    );
  }

  /// A-198. Do: acknowledge a response that was never saved, a foreign hint, and a duplicate acknowledgement.
  /// Expect: refusal at each wrong step, and a fresh receive only after the original response's proper acknowledgement.
  #[test]
  fn an_unpublished_or_foreign_acknowledgement_cannot_admit_a_new_receive() {
    let mut memory = vec![0; BYTES];
    {
      let mut receive = Capture::new(&mut memory)
        .unwrap()
        .begin(&Fill::new())
        .unwrap();
      receive.buffer_mut().copy_from_slice(&reads(&[0x1000]));
      let mut batch = receive.received(BATCH_BYTES, &mut Fill::new()).unwrap();
      let request = batch.next_request().unwrap().unwrap();
      assert_eq!(batch.acknowledge(request.hint()), Err(Refusal::Unsettled));
      assert!(matches!(batch.next_request(), Err(Refusal::Unsettled)));
      let response = request.cancelled_response().unwrap();
      batch.record_response(&request, &response).unwrap();
      assert_eq!(batch.acknowledge(0x2000), Err(Refusal::Unsettled));
      batch.acknowledge(request.hint()).unwrap();
      assert_eq!(batch.acknowledge(request.hint()), Err(Refusal::Unsettled));
    }
    assert!(
      Capture::new(&mut memory)
        .unwrap()
        .begin(&Fill::new())
        .is_ok()
    );
  }

  /// A-198. Do: crash at every byte of clearing the prior request header after its response was acknowledged.
  /// Expect: no replay, even when that old buffer is torn; the next request can be received without a stale completion.
  #[test]
  fn a_crash_while_clearing_a_settled_buffer_cannot_revive_its_requests() {
    let mut complete_memory = vec![0; BYTES];
    {
      let mut receive = Capture::new(&mut complete_memory)
        .unwrap()
        .begin(&Fill::new())
        .unwrap();
      receive.buffer_mut().copy_from_slice(&reads(&[0x1000]));
      let mut batch = receive.received(BATCH_BYTES, &mut Fill::new()).unwrap();
      let request = batch.next_request().unwrap().unwrap();
      batch
        .record_response(&request, &request.cancelled_response().unwrap())
        .unwrap();
      batch.acknowledge(request.hint()).unwrap();
    }
    for cleared in 0..=super::super::REQUEST_BASE {
      let mut memory = complete_memory.clone();
      memory[PHASE_BYTES..PHASE_BYTES + cleared].fill(0);
      assert!(
        Capture::new(&mut memory)
          .unwrap()
          .resume()
          .unwrap()
          .is_none()
      );
      assert!(
        Capture::new(&mut memory)
          .unwrap()
          .begin(&Fill::new())
          .is_ok()
      );
    }
  }

  /// A-212. Do: receive one request of a reported length, with a byte past that length set. Expect: the live receive
  /// reads only the reported bytes, and a crash's recovery, which knows no length, still refuses the whole buffer.
  /// Before A-212 every live receive scanned all 64 KiB (26.4 µs at p50 on arm64, run 41977769822).
  #[test]
  fn a_live_receive_reads_only_the_bytes_the_driver_reported() {
    let mut memory = vec![0; BYTES];
    let mut bytes = reads(&[0x1000]);
    *bytes.last_mut().unwrap() = 1;
    {
      let mut receive = Capture::new(&mut memory)
        .unwrap()
        .begin(&Fill::new())
        .unwrap();
      receive.buffer_mut().copy_from_slice(&bytes);
      let mut batch = receive
        .received(super::super::REQUEST_BASE, &mut Fill::new())
        .unwrap();
      assert_eq!(batch.next_request().unwrap().unwrap().hint(), 0x1000);
      assert!(batch.pending.is_empty());
    }
    assert!(matches!(
      Capture::new(&mut memory).unwrap().resume(),
      Err(Refusal::Tail)
    ));
  }

  /// A-212. Do: settle a batch of three with a saved response, begin with what it used, take a batch of one, and crash.
  /// Expect: recovery finds exactly the one request and no saved response: clearing only the used bytes leaves nothing
  /// of the longer batch for a whole-buffer scan to refuse or replay.
  #[test]
  fn a_shorter_batch_after_a_longer_one_recovers_alone() {
    let mut memory = vec![0; BYTES];
    let used = 3 * super::super::REQUEST_BASE;
    let mut fill = Fill::new();
    {
      let mut receive = Capture::new(&mut memory).unwrap().begin(&fill).unwrap();
      receive
        .buffer_mut()
        .copy_from_slice(&reads(&[0x1000, 0x2000, 0x3000]));
      let mut batch = receive.received(used, &mut fill).unwrap();
      while let Some(request) = batch.next_request().unwrap() {
        batch
          .record_response(&request, &request.cancelled_response().unwrap())
          .unwrap();
        batch.acknowledge(request.hint()).unwrap();
      }
    }
    {
      let mut receive = Capture::new(&mut memory).unwrap().begin(&fill).unwrap();
      let one = reads(&[0x4000]);
      receive.buffer_mut()[..super::super::REQUEST_BASE]
        .copy_from_slice(&one[..super::super::REQUEST_BASE]);
    }
    let mut batch = Capture::new(&mut memory)
      .unwrap()
      .resume()
      .unwrap()
      .unwrap();
    let request = batch.next_request().unwrap().unwrap();
    assert_eq!(request.hint(), 0x4000);
    assert!(batch.response_for(&request).unwrap().is_none());
    assert!(batch.pending.is_empty());
  }

  /// A-212. Do: report more than the buffer holds. Expect: a typed refusal, before any byte is read.
  #[test]
  fn a_reported_length_past_the_buffer_is_refused() {
    let mut memory = vec![0; BYTES];
    let receive = Capture::new(&mut memory)
      .unwrap()
      .begin(&Fill::new())
      .unwrap();
    assert!(matches!(
      receive.received(BATCH_BYTES + 1, &mut Fill::new()),
      Err(Refusal::Length)
    ));
  }

  // A-215: the combined call against a serial model of WinFsp's queues, crashed at every step.

  /// Shape: the model's IRPs, enough for batches of one to three, several hand-offs from each buffer, and an address
  /// freed by a hand-off's response taken by a request delivered in the same call.
  const MODEL_IRPS: u64 = 10;
  /// Format: where a model request carries its IRP's identity, inside the request union past the common header.
  const MODEL_ID_AT: usize = 16;
  /// Format: IoStatus.Information, where a model response carries the identity of the request it answers.
  const INFORMATION_AT: usize = 16;
  /// Format: IoStatus.Status in the response header.
  const STATUS_FIELD_AT: usize = 20;
  /// Format: Create.Opened.UserContext2, the open's ledger context.
  const CONTEXT_AT: usize = 32;
  /// Format: STATUS_CANCELLED.
  const CANCELLED_STATUS: u32 = 0xC000_0120;

  #[derive(Clone, Copy, Debug)]
  struct Irp {
    id: u64,
    address: u64,
  }

  /// WinFsp's queues as A-215's facts state them: a call finishes its responses before it delivers; a response matches
  /// an address only in processing; a delivered request is in the caller's buffer before it is processing; and a freed
  /// address goes to the very next IRP, the worst reuse the allocator could make.
  #[derive(Default)]
  struct Kernel {
    pending: VecDeque<Irp>,
    processing: BTreeMap<u64, Irp>,
    free: Vec<u64>,
    arrived: u64,
    calls: u64,
    /// Each completed IRP: the context of a successful open, or none for a cancellation.
    completed: BTreeMap<u64, Option<u64>>,
    freed_in_call: Vec<u64>,
    wrong: u64,
    twice: u64,
    bogus: u64,
    reused_in_call: u64,
  }

  impl Kernel {
    fn arrive(&mut self, count: u64) {
      for _ in 0..count {
        if self.arrived == MODEL_IRPS {
          return;
        }
        let address = self.free.pop().unwrap_or(0x1000 * (self.arrived + 1));
        self.pending.push_back(Irp {
          id: self.arrived,
          address,
        });
        self.arrived += 1;
      }
    }

    fn respond(&mut self, input: &[u8]) {
      let mut at = 0;
      while at + 4 <= input.len() {
        let size = usize::from(u16::from_le_bytes(
          input[at + 2..at + 4].try_into().unwrap(),
        ));
        if size == 0 {
          break;
        }
        let response = &input[at..at + size];
        let hint = u64::from_le_bytes(response[8..16].try_into().unwrap());
        match self.processing.remove(&hint) {
          None => self.bogus += 1,
          Some(irp) => {
            let status = u32::from_le_bytes(
              response[STATUS_FIELD_AT..STATUS_FIELD_AT + 4]
                .try_into()
                .unwrap(),
            );
            let outcome = if status == 0 {
              let answered = u32::from_le_bytes(
                response[INFORMATION_AT..INFORMATION_AT + 4]
                  .try_into()
                  .unwrap(),
              );
              if u64::from(answered) != irp.id {
                self.wrong += 1;
              }
              Some(u64::from_le_bytes(
                response[CONTEXT_AT..CONTEXT_AT + 8].try_into().unwrap(),
              ))
            } else {
              assert_eq!(status, CANCELLED_STATUS);
              None
            };
            if self.completed.insert(irp.id, outcome).is_some() {
              self.twice += 1;
            }
            self.free.push(irp.address);
            self.freed_in_call.push(irp.address);
          }
        }
        at += size.next_multiple_of(8);
      }
    }

    fn deliver(&mut self, output: &mut [u8]) -> usize {
      let mut at = 0;
      while at + super::super::REQUEST_BASE <= output.len() {
        let Some(irp) = self.pending.pop_front() else {
          break;
        };
        let frame = &mut output[at..at + super::super::REQUEST_BASE];
        frame[2..4].copy_from_slice(
          &u16::try_from(super::super::REQUEST_BASE)
            .unwrap()
            .to_le_bytes(),
        );
        frame[4..8].copy_from_slice(&super::super::CREATE_KIND.to_le_bytes());
        frame[8..16].copy_from_slice(&irp.address.to_le_bytes());
        frame[MODEL_ID_AT..MODEL_ID_AT + 8].copy_from_slice(&irp.id.to_le_bytes());
        if self.freed_in_call.contains(&irp.address) {
          self.reused_in_call += 1;
        }
        assert!(self.processing.insert(irp.address, irp).is_none());
        at += super::super::REQUEST_BASE;
      }
      at
    }
  }

  /// The ledger's one unconfirmed open, as `crate::opens` keeps it (A-198): recorded by the operation, confirmed once
  /// its response reached the driver, and at recovery kept only if the journal says the kernel has or will get it.
  #[derive(Default)]
  struct Opens {
    pending: Option<(u64, u64)>,
    confirmed: BTreeSet<u64>,
    retired: BTreeSet<u64>,
  }

  struct Crashed;

  /// The steps an owner may take before it dies; `None` lives to the end.
  struct Budget {
    left: Option<usize>,
    used: usize,
  }

  impl Budget {
    fn step(&mut self) -> Result<(), Crashed> {
      if let Some(left) = &mut self.left {
        if *left == 0 {
          return Err(Crashed);
        }
        *left -= 1;
      }
      self.used += 1;
      Ok(())
    }
  }

  /// One control call. Other threads' IRPs arrive before it, and while it waits after its responses, where one may take
  /// an address a response just freed. A death may land before it, in its wait, or after its delivery.
  fn transact(
    kernel: &mut Kernel,
    budget: &mut Budget,
    input: &[u8],
    output: Option<&mut [u8]>,
  ) -> Result<usize, Crashed> {
    let call = kernel.calls;
    kernel.calls += 1;
    kernel.arrive(call % 3);
    budget.step()?;
    kernel.freed_in_call.clear();
    kernel.respond(input);
    if output.is_some() {
      kernel.arrive(1 - call % 2);
    }
    budget.step()?;
    let delivered = match output {
      Some(output) => kernel.deliver(output),
      None => 0,
    };
    budget.step()?;
    Ok(delivered)
  }

  /// The DLL's Create: a successful open whose response names the request it answers and its own context.
  fn operate(request: &Request<'_>, opens: &mut Opens) -> Vec<u8> {
    let id = u64::from_le_bytes(
      request.bytes()[MODEL_ID_AT..MODEL_ID_AT + 8]
        .try_into()
        .unwrap(),
    );
    let mut response = request.cancelled_response().unwrap().to_vec();
    response[STATUS_FIELD_AT..STATUS_FIELD_AT + 4].fill(0);
    response[INFORMATION_AT..INFORMATION_AT + 4]
      .copy_from_slice(&u32::try_from(id).unwrap().to_le_bytes());
    response[CONTEXT_AT..CONTEXT_AT + 8].copy_from_slice(&(id + 1).to_le_bytes());
    assert!(opens.pending.is_none(), "an open was left unconfirmed");
    opens.pending = Some((id + 1, request.hint()));
    response
  }

  fn confirm(response: &[u8], opens: &mut Opens) {
    if let Some((_, context)) = super::super::opened_context(response).unwrap() {
      assert_eq!(opens.pending.map(|(pending, _)| pending), Some(context));
      opens.confirmed.insert(context);
      opens.pending = None;
    }
  }

  /// The live serving of one batch, as the dispatcher does it: every response but the last alone, the last handed off.
  fn serve_batch<'a>(
    mut batch: Batch<'a>,
    kernel: &mut Kernel,
    opens: &mut Opens,
    budget: &mut Budget,
    fill: &mut Fill,
  ) -> Result<Option<HandOff<'a>>, Crashed> {
    while let Some(request) = batch.next_request().unwrap() {
      budget.step()?;
      let response = operate(&request, opens);
      budget.step()?;
      batch.record_response(&request, &response).unwrap();
      budget.step()?;
      if batch.is_last() {
        return Ok(Some(batch.hand_off(&request, fill).unwrap()));
      }
      transact(kernel, budget, &response, None)?;
      confirm(&response, opens);
      budget.step()?;
      batch.acknowledge(request.hint()).unwrap();
    }
    Ok(None)
  }

  /// One owner's life: recovery as the host and the dispatcher do it, then live serving until every IRP has arrived
  /// and none is pending.
  fn own(
    memory: &mut [u8],
    kernel: &mut Kernel,
    opens: &mut Opens,
    budget: &mut Budget,
  ) -> Result<(), Crashed> {
    let mut copy = memory.to_vec();
    let retained = Capture::new(&mut copy).unwrap().retained_open().unwrap();
    if let Some((context, hint)) = opens.pending.take() {
      if retained == Some((hint, context)) {
        opens.confirmed.insert(context);
      } else {
        opens.retired.insert(context);
      }
    }
    budget.step()?;
    if let Some(mut batch) = Capture::new(memory).unwrap().resume().unwrap() {
      while let Some(request) = batch.next_request().unwrap() {
        budget.step()?;
        let response = match batch.response_for(&request).unwrap() {
          Some(saved) => saved.to_vec(),
          None => {
            let cancelled = request.cancelled_response().unwrap().to_vec();
            batch.record_response(&request, &cancelled).unwrap();
            cancelled
          }
        };
        budget.step()?;
        transact(kernel, budget, &response, None)?;
        budget.step()?;
        batch.acknowledge(request.hint()).unwrap();
      }
    }
    let mut fill = Fill::new();
    let mut handed: Option<usize> = None;
    loop {
      budget.step()?;
      let next = {
        let batch = match handed.take() {
          Some(reported) => Capture::new(memory)
            .unwrap()
            .handed_off(reported, &mut fill)
            .unwrap(),
          None => {
            if kernel.arrived == MODEL_IRPS && kernel.pending.is_empty() {
              return Ok(());
            }
            let mut receiving = Capture::new(memory).unwrap().begin(&fill).unwrap();
            let reported = transact(kernel, budget, &[], Some(receiving.buffer_mut()))?;
            receiving.received(reported, &mut fill).unwrap()
          }
        };
        match serve_batch(batch, kernel, opens, budget, &mut fill)? {
          None => None,
          Some(mut hand_off) => {
            budget.step()?;
            confirm(hand_off.response(), opens);
            budget.step()?;
            let (input, output) = hand_off.transaction();
            let reported = transact(kernel, budget, input, Some(output))?;
            budget.step()?;
            Some(reported)
          }
        }
      };
      handed = next;
    }
  }

  #[derive(Default, Debug)]
  struct Tally {
    scenarios: u64,
    lost: u64,
    wrong: u64,
    twice: u64,
    wrong_context: u64,
    bogus: u64,
    reused_in_call: u64,
    delivered_hand_offs: u64,
    undelivered_hand_offs: u64,
  }

  /// Runs owners in turn, each dying after its budget of steps, then one that lives; adds the outcome to the tally.
  /// Returns the steps the living owner took.
  fn scenario(crashes: &[usize], tally: &mut Tally) -> usize {
    let mut memory = vec![0; BYTES];
    let mut kernel = Kernel::default();
    let mut opens = Opens::default();
    for crash in crashes {
      let mut budget = Budget {
        left: Some(*crash),
        used: 0,
      };
      if own(&mut memory, &mut kernel, &mut opens, &mut budget).is_ok() {
        break;
      }
      match Capture::new(&mut memory.clone())
        .unwrap()
        .landing()
        .unwrap()
      {
        Landing::HandingOffDelivered => tally.delivered_hand_offs += 1,
        Landing::HandingOffUndelivered => tally.undelivered_hand_offs += 1,
        Landing::Idle | Landing::Receiving => {}
      }
    }
    let mut budget = Budget {
      left: None,
      used: 0,
    };
    assert!(own(&mut memory, &mut kernel, &mut opens, &mut budget).is_ok());
    tally.scenarios += 1;
    tally.lost += MODEL_IRPS - u64::try_from(kernel.completed.len()).unwrap();
    tally.wrong += kernel.wrong;
    tally.twice += kernel.twice;
    tally.bogus += kernel.bogus;
    tally.reused_in_call += kernel.reused_in_call;
    for context in kernel.completed.values().flatten() {
      if !opens.confirmed.contains(context) || opens.retired.contains(context) {
        tally.wrong_context += 1;
      }
    }
    budget.used
  }

  /// A-215. Do: serve ten IRPs through the combined call, killing the owner at every step it takes, and again with its
  /// successor killed at every step too; the last owner lives. Expect: every IRP completed once, every success
  /// carrying its own request's identity and a context the ledger kept, none lost; and the kills landed in hand-offs
  /// both delivered and not, with a freed address reused in the same call, so the oracle saw the case it guards.
  #[test]
  fn every_irp_is_answered_once_with_its_own_response_whatever_step_an_owner_dies_at() {
    let mut tally = Tally::default();
    let steps = scenario(&[], &mut tally);
    for first in 0..steps {
      scenario(&[first], &mut tally);
      for second in 0..steps {
        scenario(&[first, second], &mut tally);
      }
    }
    eprintln!("A-215 model over {steps} steps: {tally:?}");
    assert_eq!(
      (tally.lost, tally.wrong, tally.twice, tally.wrong_context),
      (0, 0, 0, 0),
      "{tally:?}"
    );
    assert!(tally.delivered_hand_offs > 0 && tally.undelivered_hand_offs > 0);
    assert!(tally.reused_in_call > 0 && tally.bogus > 0);
  }
}
