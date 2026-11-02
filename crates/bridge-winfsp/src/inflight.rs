//! Captured WinFsp transactions (A-198, §4.6 Windows). The kernel's direct-output batch is retained in held RAM;
//! recovery settles it before any new receive, so a recycled IRP address cannot yet name a new processing request.
//! This module checks the bounded wire frames without dereferencing their process-relative addresses. Its oracle
//! completes modeled kernel reads through the resulting cancellation responses. The live boundary is the host's
//! taken-read test; WinFsp v2.1 fsctl.h and sys/volume.c define these layouts and copy-before-processing order.

pub mod capture;

/// Format: the public FSP_FSCTL_TRANSACT_BATCH_BUFFER_SIZEMIN, in bytes (WinFsp v2.1 fsctl.h).
pub const BATCH_BYTES: usize = 64 * 1024;
/// Format: the public maximum request, 16 KiB minus its 64-byte internal header (fsctl.h).
pub const REQUEST_MAX: usize = 16 * 1024 - 64;
/// Format: fixed request header, largest request union and filename descriptor, rounded to eight-byte wire alignment.
/// The SetInformation union holds two context words, a class with padding, and the 40-byte Basic information record.
pub const REQUEST_BASE: usize = 16 + 64 + 8;
/// Format: the common response header (24 bytes) followed by the 104-byte Create.Opened union (fsctl.h).
pub const RESPONSE_BASE: usize = 24 + 104;
/// Format: the public maximum response (FSP_FSCTL_TRANSACT_RSP_SIZEMAX).
pub const RESPONSE_MAX: usize = 16 * 1024;
/// Format: the number of defined request kinds, including reserved zero (fsctl.h's FspFsctlTransactKindCount).
pub const KIND_COUNT: u32 = 22;
/// Format: the Create/Open kind and the per-open Cleanup and Close kinds, in fsctl.h's enum order.
pub const CREATE_KIND: u32 = 1;
/// Format: see CREATE_KIND.
pub const CLEANUP_KIND: u32 = 3;
/// Format: see CREATE_KIND.
pub const CLOSE_KIND: u32 = 4;
/// Format: all request and response frames are rounded to FSP_FSCTL_DEFAULT_ALIGNMENT bytes.
const ALIGNMENT: usize = 8;
/// Format: the size follows the two-byte version; the kind and hint follow that size.
const SIZE_AT: usize = size_of::<u16>();
/// Format: see SIZE_AT.
const KIND_AT: usize = SIZE_AT + size_of::<u16>();
/// Format: see SIZE_AT.
const HINT_AT: usize = KIND_AT + size_of::<u32>();
/// Format: the request union follows the common header; Create.AccessToken is 24 bytes into that union.
const CREATE_TOKEN_AT: usize = HINT_AT + size_of::<u64>() + 24;
/// Format: IoStatus.Information precedes IoStatus.Status after the response's common header.
const STATUS_AT: usize = HINT_AT + size_of::<u64>() + size_of::<u32>();
/// Format: Create.Opened.UserContext2 follows the common 24-byte header and its UserContext word.
const OPEN_CONTEXT_AT: usize = 24 + size_of::<u64>();
/// Format: STATUS_CANCELLED, the typed cancellation returned without touching an old process's buffer.
const CANCELLED: u32 = 0xC000_0120;

/// A malformed capture is refused before any response or process-relative address can be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
  /// A frame or batch is outside its protocol cap or its checked range.
  Length,
  /// A future header version cannot be interpreted as this format.
  Version,
  /// A reserved or unknown operation kind.
  Kind,
  /// A missing or duplicate processing-request hint.
  Hint,
  /// A zero-sized terminal frame has a nonzero tail.
  Tail,
  /// The bounded frame index could not be allocated.
  NoMemory,
  /// The receive journal has an invalid publication state or reserved bytes.
  State,
  /// A prior batch or its current response has not been acknowledged yet.
  Unsettled,
  /// A published response fails its checksum or belongs to no captured request.
  Checksum,
  /// A saved complete response cannot be replaced by a different result for the same request.
  ResponseChanged,
}

impl std::fmt::Display for Refusal {
  fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let reason = match self {
      Self::Length => "a length is outside the transaction format",
      Self::Version => "the transaction version is unsupported",
      Self::Kind => "the operation kind is unknown",
      Self::Hint => "a kernel hint is missing, repeated, or mismatched",
      Self::Tail => "nonzero bytes follow the transaction's end",
      Self::NoMemory => "the bounded batch index could not be allocated",
      Self::State => "the receive journal has an invalid publication state",
      Self::Unsettled => "the preceding request or batch has not been settled",
      Self::Checksum => "a saved response failed its checksum",
      Self::ResponseChanged => "an already saved response cannot be changed",
    };
    write!(out, "receive journal: {reason}")
  }
}

impl std::error::Error for Refusal {}

/// The context in a complete successful Create/Open response. Reparse and failure responses contain no issued open.
pub fn opened_context(response: &[u8]) -> Result<Option<(u64, u64)>, Refusal> {
  capture::check_response(response)?;
  if read_u32(response, KIND_AT)? != CREATE_KIND || read_u32(response, STATUS_AT)? != 0 {
    return Ok(None);
  }
  let hint = read_u64(response, HINT_AT)?;
  let context = read_u64(response, OPEN_CONTEXT_AT)?;
  if context == 0 {
    return Err(Refusal::Hint);
  }
  Ok(Some((hint, context)))
}

/// One checked request in a retained batch. Its payload remains opaque unless a specific checked accessor reads it.
pub struct Request<'a> {
  bytes: &'a [u8],
  kind: u32,
  hint: u64,
}

impl Request<'_> {
  /// The original bounded wire frame, lent only while its capture is still owned.
  pub fn bytes(&self) -> &[u8] {
    self.bytes
  }
  /// The defined operation kind used to dispatch or settle the request.
  pub fn kind(&self) -> u32 {
    self.kind
  }
  /// The opaque processing identity, never a pointer the host dereferences.
  pub fn hint(&self) -> u64 {
    self.hint
  }
  /// The Create/Open caller's pid from the token's high word; zero for other requests, as WinFsp's own helper returns.
  pub fn process_id(&self) -> Result<u32, Refusal> {
    if self.kind != CREATE_KIND {
      return Ok(0);
    }
    u32::try_from(read_u64(self.bytes, CREATE_TOKEN_AT)? >> u32::BITS).map_err(|_| Refusal::Length)
  }
  /// Refuses this original operation without using its old buffer address or token handle.
  pub fn cancelled_response(&self) -> Result<[u8; RESPONSE_BASE], Refusal> {
    let mut response = [0; RESPONSE_BASE];
    let length = u16::try_from(RESPONSE_BASE).map_err(|_| Refusal::Length)?;
    put(&mut response, SIZE_AT, &length.to_le_bytes())?;
    put(&mut response, KIND_AT, &self.kind.to_le_bytes())?;
    put(&mut response, HINT_AT, &self.hint.to_le_bytes())?;
    put(&mut response, STATUS_AT, &CANCELLED.to_le_bytes())?;
    Ok(response)
  }
}

/// Checks the complete bounded batch before returning any frame. A zeroed tail ends the batch; a malformed tail or
/// repeated hint refuses it whole. The live receive owner must settle these frames before receiving any fresh one.
pub fn requests(bytes: &[u8]) -> Result<Vec<Request<'_>>, Refusal> {
  if bytes.len() > BATCH_BYTES || !bytes.len().is_multiple_of(ALIGNMENT) {
    return Err(Refusal::Length);
  }
  let bound = bytes.len() / REQUEST_BASE;
  let mut requests = Vec::new();
  let mut hints = Vec::new();
  requests
    .try_reserve_exact(bound)
    .map_err(|_| Refusal::NoMemory)?;
  hints
    .try_reserve_exact(bound)
    .map_err(|_| Refusal::NoMemory)?;
  let mut at = 0usize;
  while at < bytes.len() {
    let tail = bytes.get(at..).ok_or(Refusal::Length)?;
    let length = usize::from(read_u16(tail, SIZE_AT)?);
    if length == 0 {
      if tail.iter().any(|byte| *byte != 0) {
        return Err(Refusal::Tail);
      }
      break;
    }
    let (request, stride) = checked_request(tail, length)?;
    hints.push(request.hint);
    requests.push(request);
    at = at.checked_add(stride).ok_or(Refusal::Length)?;
  }
  // A kernel batch cannot contain one processing IRP twice. Sort a bounded index rather than changing request
  // order: cleanup and close effects must still settle in the order the driver supplied them.
  hints.sort_unstable();
  if hints.windows(2).any(|pair| pair.first() == pair.last()) {
    return Err(Refusal::Hint);
  }
  Ok(requests)
}

fn checked_request(tail: &[u8], length: usize) -> Result<(Request<'_>, usize), Refusal> {
  if !(REQUEST_BASE..=REQUEST_MAX).contains(&length) {
    return Err(Refusal::Length);
  }
  let stride = length
    .checked_next_multiple_of(ALIGNMENT)
    .ok_or(Refusal::Length)?;
  let bytes = tail.get(..length).ok_or(Refusal::Length)?;
  let padding = tail.get(length..stride).ok_or(Refusal::Length)?;
  if padding.iter().any(|byte| *byte != 0) {
    return Err(Refusal::Tail);
  }
  if read_u16(bytes, 0)? != 0 {
    return Err(Refusal::Version);
  }
  let kind = read_u32(bytes, KIND_AT)?;
  if kind == 0 || kind >= KIND_COUNT {
    return Err(Refusal::Kind);
  }
  let hint = read_u64(bytes, HINT_AT)?;
  if hint == 0 {
    return Err(Refusal::Hint);
  }
  Ok((Request { bytes, kind, hint }, stride))
}

fn read_u16(bytes: &[u8], at: usize) -> Result<u16, Refusal> {
  Ok(u16::from_le_bytes(field(bytes, at)?))
}

fn read_u32(bytes: &[u8], at: usize) -> Result<u32, Refusal> {
  Ok(u32::from_le_bytes(field(bytes, at)?))
}

fn read_u64(bytes: &[u8], at: usize) -> Result<u64, Refusal> {
  Ok(u64::from_le_bytes(field(bytes, at)?))
}

fn field<const BYTES: usize>(bytes: &[u8], at: usize) -> Result<[u8; BYTES], Refusal> {
  let end = at.checked_add(BYTES).ok_or(Refusal::Length)?;
  bytes
    .get(at..end)
    .ok_or(Refusal::Length)?
    .try_into()
    .map_err(|_| Refusal::Length)
}

fn put(bytes: &mut [u8], at: usize, from: &[u8]) -> Result<(), Refusal> {
  let end = at.checked_add(from.len()).ok_or(Refusal::Length)?;
  bytes
    .get_mut(at..end)
    .ok_or(Refusal::Length)?
    .copy_from_slice(from);
  Ok(())
}

#[cfg(test)]
mod tests {
  //! A serial kernel-queue oracle: cancellation responses must finish the original reads and no other operation.
  #![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
  )]
  use super::*;
  use std::collections::BTreeMap;

  fn captured_reads(hints: &[u64]) -> Vec<u8> {
    let mut bytes = vec![0; BATCH_BYTES];
    for (index, hint) in hints.iter().enumerate() {
      let frame = &mut bytes[index * REQUEST_BASE..(index + 1) * REQUEST_BASE];
      frame[2..4].copy_from_slice(&u16::try_from(REQUEST_BASE).unwrap().to_le_bytes());
      frame[4..8].copy_from_slice(&5u32.to_le_bytes());
      frame[8..16].copy_from_slice(&hint.to_le_bytes());
    }
    bytes
  }

  fn finish_reads(
    bytes: &[u8],
    processing: &mut BTreeMap<u64, &'static str>,
  ) -> Result<Vec<&'static str>, Refusal> {
    let mut cancelled = Vec::new();
    for request in requests(bytes)? {
      let response = request.cancelled_response()?;
      if let Some(caller) = complete_read(&response, processing) {
        cancelled.push(caller);
      }
    }
    Ok(cancelled)
  }

  fn complete_read(
    response: &[u8; 128],
    processing: &mut BTreeMap<u64, &'static str>,
  ) -> Option<&'static str> {
    // Decode the public response independently: IoStatus.Information is first, Status second.
    assert_eq!(u16::from_le_bytes(response[2..4].try_into().unwrap()), 128);
    assert_eq!(u32::from_le_bytes(response[4..8].try_into().unwrap()), 5);
    assert_eq!(u32::from_le_bytes(response[16..20].try_into().unwrap()), 0);
    assert_eq!(
      u32::from_le_bytes(response[20..24].try_into().unwrap()),
      0xC000_0120
    );
    let hint = u64::from_le_bytes(response[8..16].try_into().unwrap());
    processing.remove(&hint)
  }

  /// A-198 Step 9.4. Do: settle two captured reads. Expect: each original caller gets cancellation, and a request not
  /// in the captured batch is still pending. This is the wire action, not an assertion on parser implementation fields.
  #[test]
  fn a_captured_batch_cancels_its_original_reads_and_no_other_request() {
    let bytes = captured_reads(&[0x1000, 0x2000]);
    let mut kernel = BTreeMap::from([(0x1000, "first"), (0x2000, "second"), (0x3000, "unrelated")]);
    assert_eq!(
      finish_reads(&bytes, &mut kernel).unwrap(),
      vec!["first", "second"]
    );
    assert_eq!(kernel.into_values().collect::<Vec<_>>(), vec!["unrelated"]);
  }

  /// A-198. Do: present a hostile capture while a read is processing. Expect: refusal before the driver sees any
  /// response; the original caller remains pending, instead of another operation being completed by a malformed hint.
  #[test]
  fn a_hostile_capture_cannot_complete_a_kernel_request() {
    let valid = captured_reads(&[0x1000]);
    let mutations: &[(usize, &[u8])] = &[
      (0, &u16::MAX.to_le_bytes()),
      (2, &u16::MAX.to_le_bytes()),
      (2, &1u16.to_le_bytes()),
      (4, &u32::MAX.to_le_bytes()),
      (4, &0u32.to_le_bytes()),
      (8, &0u64.to_le_bytes()),
      (REQUEST_BASE + 8, &0x1000u64.to_le_bytes()),
    ];
    for (at, from) in mutations {
      let mut bytes = valid.clone();
      bytes[*at..*at + from.len()].copy_from_slice(from);
      let mut kernel = BTreeMap::from([(0x1000, "first")]);
      assert!(finish_reads(&bytes, &mut kernel).is_err());
      assert_eq!(kernel.into_values().collect::<Vec<_>>(), vec!["first"]);
    }
    for length in [1, REQUEST_BASE - 1, BATCH_BYTES + ALIGNMENT] {
      let bytes = vec![0; length];
      assert!(requests(&bytes).is_err());
    }
    let mut kernel = BTreeMap::from([(0x1000, "first")]);
    assert!(finish_reads(&captured_reads(&[0x1000, 0x1000]), &mut kernel).is_err());
    assert_eq!(kernel.into_values().collect::<Vec<_>>(), vec!["first"]);
  }

  /// A-198. Do: replay a batch whose first IRP completed already; its recycled address is pending, not processing,
  /// because no receive may precede settlement. Expect: only the original still-processing read is completed.
  #[test]
  fn a_completed_hint_cannot_cancel_its_recycled_pending_request() {
    let bytes = captured_reads(&[0x1000, 0x2000]);
    let mut processing = BTreeMap::from([(0x2000, "original")]);
    let mut pending = BTreeMap::from([(0x1000, "new caller")]);
    assert_eq!(
      finish_reads(&bytes, &mut processing).unwrap(),
      vec!["original"]
    );
    assert!(processing.is_empty());
    processing.append(&mut pending);
    assert_eq!(
      processing.into_values().collect::<Vec<_>>(),
      vec!["new caller"]
    );
  }
}
