//! The retained receive journal (A-198). One owner receives a batch into held pages, preserves a response before
//! sending it, and acknowledges every response before any new receive. A single-owner state byte distinguishes
//! reset memory from a receive that may have taken requests. Readers inspect it only after the previous owner ended;
//! this is crash publication, not a shared atomic or lock on a request path. A published saved response is replayed
//! first, so a second crash cannot lose an already-issued open by overwriting its only complete response.

use super::{
  BATCH_BYTES, RESPONSE_BASE, RESPONSE_MAX, Refusal, Request, put, read_u32, read_u64, requests,
};
use slates_wire::crc32c::crc32c;
use std::collections::VecDeque;

/// Format: a phase byte and seven reserved zero bytes keep the direct-output request buffer eight-byte aligned.
const PHASE_BYTES: usize = size_of::<u64>();
/// Format: saved-response state and three reserved bytes, length, CRC32C and four reserved bytes.
const SAVED_HEADER: usize = size_of::<u32>() * 4;
/// Format: the fixed capture region appended after the aligned open-ledger frames.
pub const BYTES: usize = PHASE_BYTES + BATCH_BYTES + SAVED_HEADER + RESPONSE_MAX;
/// Format: zero means reset/no processing request; one means a receive may have taken requests. No other phase exists.
const RECEIVING: u8 = 1;
/// Format: one marks a complete saved response; zero marks an unfinished save. It is written only after its body/CRC.
const PUBLISHED: u8 = 1;
/// Format: the saved response's length follows its state word, and its CRC32C follows the length.
const LENGTH_AT: usize = size_of::<u32>();
/// Format: see LENGTH_AT.
const CRC_AT: usize = LENGTH_AT + size_of::<u32>();
/// Format: the last reserved word follows the CRC32C in the saved-response header.
const RESERVED_AT: usize = CRC_AT + size_of::<u32>();

/// A borrow of one capture region. Its owner is the only thread touching these bytes until it ends or returns them.
pub struct Capture<'a> {
  phase: &'a mut [u8],
  request: &'a mut [u8],
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
    let (request, saved) = body
      .split_at_mut_checked(BATCH_BYTES)
      .ok_or(Refusal::Length)?;
    if !matches!(phase.first().copied(), Some(0 | RECEIVING))
      || phase
        .get(1..)
        .ok_or(Refusal::Length)?
        .iter()
        .any(|byte| *byte != 0)
    {
      return Err(Refusal::State);
    }
    Ok(Self {
      phase,
      request,
      saved,
    })
  }

  /// Clears an already-settled buffer, then records the new receive before lending it to the driver. A dropped
  /// unfinished batch stays RECEIVING, so it cannot be silently replaced by a fresh receive. `used` is what this owner's
  /// last batch filled ([`filled`]); `None`, an owner's first receive, clears all of it, since a predecessor may have
  /// died part-way through. Only the saved response's header is cleared: its state word says whether a body exists, and
  /// no reader goes past a body's own length (A-212).
  pub fn begin(self, used: Option<usize>) -> Result<Receiving<'a>, Refusal> {
    if self.phase.first() != Some(&0) {
      return Err(Refusal::Unsettled);
    }
    match used {
      Some(filled) => self
        .request
        .get_mut(..filled)
        .ok_or(Refusal::Length)?
        .fill(0),
      None => self.request.fill(0),
    }
    self
      .saved
      .get_mut(..SAVED_HEADER)
      .ok_or(Refusal::Length)?
      .fill(0);
    *self.phase.first_mut().ok_or(Refusal::Length)? = RECEIVING;
    Ok(Receiving { capture: self })
  }

  /// Takes the retained batch before the first new receive. Reset memory is ignored, including a partially cleared
  /// prior buffer whose responses all reached the driver before its owner recorded reset.
  pub fn resume(self) -> Result<Option<Batch<'a>>, Refusal> {
    if self.phase.first() == Some(&0) {
      return Ok(None);
    }
    Batch::checked(self, BATCH_BYTES).map(Some)
  }
}

/// The buffer of one issued receive. The caller must settle its kernel I/O before consuming it as a batch.
pub struct Receiving<'a> {
  capture: Capture<'a>,
}

impl<'a> Receiving<'a> {
  /// The direct-output buffer, borrowed for the lifetime of the synchronous/owned overlapped receive.
  pub fn buffer_mut(&mut self) -> &mut [u8] {
    self.capture.request
  }
  /// Validates the bytes after kernel I/O completion, preserving their original order and any complete saved result.
  /// `reported` is the transfer the driver counted. Only those bytes are read: [`Capture::begin`] cleared the rest, and
  /// recovery, which knows no count, still checks the whole buffer (A-212).
  pub fn received(self, reported: usize) -> Result<Batch<'a>, Refusal> {
    Batch::checked(self.capture, filled(reported)?)
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
/// Dropping this value preserves its receive state for the successor. Only the last acknowledged response resets it.
pub struct Batch<'a> {
  phase: &'a mut [u8],
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
  fn checked(capture: Capture<'a>, filled: usize) -> Result<Self, Refusal> {
    let frames: &'a [u8] = capture.request;
    let pending = requests(frames.get(..filled).ok_or(Refusal::Length)?)?;
    let saved = saved_response(capture.saved)?;
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
      *capture.phase.first_mut().ok_or(Refusal::Length)? = 0;
    }
    Ok(Self {
      phase: capture.phase,
      pending: pending.into(),
      saved: capture.saved,
      saved_first,
      current: None,
    })
  }

  /// The saved response's original request first, then the remaining requests in driver order. A current unacknowledged
  /// request refuses another take, so its response cannot be overwritten while it is still in kernel I/O.
  /// DIAG: the requests left in this batch.
  pub fn pending_len(&self) -> usize {
    self.pending.len()
  }

  /// The next request to settle, in the order recovery requires.
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
    *self.saved.first_mut().ok_or(Refusal::Length)? = 0;
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
    *self.saved.first_mut().ok_or(Refusal::Length)? = PUBLISHED;
    Ok(())
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
      *self.phase.first_mut().ok_or(Refusal::Length)? = 0;
    }
    Ok(())
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
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
  )]
  use super::*;
  use std::collections::BTreeMap;

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
      let mut receive = Capture::new(&mut memory).unwrap().begin(None).unwrap();
      receive
        .buffer_mut()
        .copy_from_slice(&reads(&[0x1000, 0x2000]));
      let mut batch = receive.received(BATCH_BYTES).unwrap();
      let request = batch.next_request().unwrap().unwrap();
      let response = request.cancelled_response().unwrap();
      batch.record_response(&request, &response).unwrap();
      assert_eq!(complete(&response, &mut kernel), Some("first"));
      // The process dies here: no acknowledgement and no next receive.
    }
    assert!(matches!(
      Capture::new(&mut memory).unwrap().begin(None),
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
    assert!(Capture::new(&mut memory).unwrap().begin(None).is_ok());
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
      let mut receive = Capture::new(&mut memory).unwrap().begin(None).unwrap();
      receive
        .buffer_mut()
        .copy_from_slice(&reads(&[0x1000, 0x2000, 0x3000]));
      let mut batch = receive.received(BATCH_BYTES).unwrap();
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
    assert!(Capture::new(&mut memory).unwrap().begin(None).is_ok());
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
      let mut receive = Capture::new(&mut memory).unwrap().begin(None).unwrap();
      receive.buffer_mut().copy_from_slice(&reads(&[0x1000]));
      let mut batch = receive.received(BATCH_BYTES).unwrap();
      let request = batch.next_request().unwrap().unwrap();
      assert_eq!(batch.acknowledge(request.hint()), Err(Refusal::Unsettled));
      assert!(matches!(batch.next_request(), Err(Refusal::Unsettled)));
      let response = request.cancelled_response().unwrap();
      batch.record_response(&request, &response).unwrap();
      assert_eq!(batch.acknowledge(0x2000), Err(Refusal::Unsettled));
      batch.acknowledge(request.hint()).unwrap();
      assert_eq!(batch.acknowledge(request.hint()), Err(Refusal::Unsettled));
    }
    assert!(Capture::new(&mut memory).unwrap().begin(None).is_ok());
  }

  /// A-198. Do: crash at every byte of clearing the prior request header after its response was acknowledged.
  /// Expect: no replay, even when that old buffer is torn; the next request can be received without a stale completion.
  #[test]
  fn a_crash_while_clearing_a_settled_buffer_cannot_revive_its_requests() {
    let mut complete_memory = vec![0; BYTES];
    {
      let mut receive = Capture::new(&mut complete_memory)
        .unwrap()
        .begin(None)
        .unwrap();
      receive.buffer_mut().copy_from_slice(&reads(&[0x1000]));
      let mut batch = receive.received(BATCH_BYTES).unwrap();
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
      assert!(Capture::new(&mut memory).unwrap().begin(None).is_ok());
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
      let mut receive = Capture::new(&mut memory).unwrap().begin(None).unwrap();
      receive.buffer_mut().copy_from_slice(&bytes);
      let mut batch = receive.received(super::super::REQUEST_BASE).unwrap();
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
    {
      let mut receive = Capture::new(&mut memory).unwrap().begin(None).unwrap();
      receive
        .buffer_mut()
        .copy_from_slice(&reads(&[0x1000, 0x2000, 0x3000]));
      let mut batch = receive.received(used).unwrap();
      while let Some(request) = batch.next_request().unwrap() {
        batch
          .record_response(&request, &request.cancelled_response().unwrap())
          .unwrap();
        batch.acknowledge(request.hint()).unwrap();
      }
    }
    {
      let mut receive = Capture::new(&mut memory)
        .unwrap()
        .begin(Some(used))
        .unwrap();
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
    let receive = Capture::new(&mut memory).unwrap().begin(None).unwrap();
    assert!(matches!(
      receive.received(BATCH_BYTES + 1),
      Err(Refusal::Length)
    ));
  }
}
