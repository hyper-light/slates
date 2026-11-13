//! Captured WinFsp transactions (A-198, §4.6 Windows). The kernel's direct-output batch is retained in held RAM;
//! recovery settles it before any new receive, so a recycled IRP address cannot yet name a new processing request.
//! This module checks the bounded wire frames without dereferencing their process-relative addresses. Its oracle
//! completes modeled kernel reads through the resulting cancellation responses. The live boundary is the host's
//! taken-read test; WinFsp v2.1 fsctl.h and sys/volume.c define these layouts and copy-before-processing order.

pub mod capture;
#[cfg(test)]
mod oracle;
pub mod settle;

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
/// Format: FspFsctlTransactSetInformationKind, whose Rename completion uses only the original kernel-owned request.
const SET_INFORMATION_KIND: u32 = 8;
/// Format: FILE_INFORMATION_CLASS FileRenameInformation and FileRenameInformationEx (ntifs.h).
const RENAME_CLASS: u32 = 10;
/// Format: see RENAME_CLASS.
const RENAME_EX_CLASS: u32 = 65;
/// Format: SetInformation's two context words precede its class; its aligned union starts eight bytes later.
const INFORMATION_CLASS_AT: usize = 16 + size_of::<u64>() * 2;
/// Format: the first member of SetInformation.Info.Rename is its four-byte name descriptor.
const NEW_NAME_AT: usize = INFORMATION_CLASS_AT + size_of::<u64>();
/// Format: the original FileName descriptor follows the fixed request union.
const FILE_NAME_AT: usize = REQUEST_BASE - ALIGNMENT;
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
  /// A retained kernel pathname has an invalid length, terminator or UTF-16 encoding.
  Path,
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
      Self::Path => "a retained kernel pathname is malformed",
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

/// Finishes a DLL operation's response in a packet the receiver reuses: its status, and its size rounded up to the wire's
/// alignment, with the padding that rounding adds cleared, so no byte of an earlier response is sent. Returns the length
/// to send. The callback wrote everything up to its own size; the fixed part before it was cleared as the header.
pub fn finish_response(packet: &mut [u8], status: i32) -> Result<usize, Refusal> {
  put(packet, STATUS_AT, &status.to_le_bytes())?;
  let size = usize::from(read_u16(packet, SIZE_AT)?);
  let aligned = size
    .checked_next_multiple_of(ALIGNMENT)
    .ok_or(Refusal::Length)?;
  if !(RESPONSE_BASE..=RESPONSE_MAX).contains(&aligned) {
    return Err(Refusal::Length);
  }
  packet
    .get_mut(size..aligned)
    .ok_or(Refusal::Length)?
    .fill(0);
  put(
    packet,
    SIZE_AT,
    &u16::try_from(aligned)
      .map_err(|_| Refusal::Length)?
      .to_le_bytes(),
  )?;
  Ok(aligned)
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
  /// The borrowed Create token's process-local handle, valid only in the current receiver's fresh request.
  pub fn access_token(&self) -> Result<u32, Refusal> {
    if self.kind != CREATE_KIND {
      return Err(Refusal::Kind);
    }
    u32::try_from(read_u64(self.bytes, CREATE_TOKEN_AT)? & u64::from(u32::MAX))
      .map_err(|_| Refusal::Length)
  }

  /// A saved successful rename has no old-process output buffer: the driver's completion uses its original
  /// kernel-owned path descriptors (sys/fileinfo.c FspFsvolSetRenameInformationSuccess). It must finish the durable
  /// cached-name updates before replaying that success, rather than cancelling a rename already applied to the VFS.
  pub fn successful_rename(&self, response: &[u8]) -> Result<Option<(String, String)>, Refusal> {
    capture::check_response(response)?;
    if read_u32(response, KIND_AT)? != self.kind || read_u64(response, HINT_AT)? != self.hint {
      return Err(Refusal::Hint);
    }
    if self.kind != SET_INFORMATION_KIND || read_u32(response, STATUS_AT)? != 0 {
      return Ok(None);
    }
    if !matches!(
      read_u32(self.bytes, INFORMATION_CLASS_AT)?,
      RENAME_CLASS | RENAME_EX_CLASS
    ) {
      return Ok(None);
    }
    Ok(Some((self.path(FILE_NAME_AT)?, self.path(NEW_NAME_AT)?)))
  }

  fn path(&self, descriptor: usize) -> Result<String, Refusal> {
    let offset = usize::from(read_u16(self.bytes, descriptor)?);
    let length = usize::from(read_u16(
      self.bytes,
      descriptor
        .checked_add(size_of::<u16>())
        .ok_or(Refusal::Length)?,
    )?);
    let cap = crate::notify::MAX_PATH_UNITS
      .checked_add(1)
      .and_then(|units| units.checked_mul(size_of::<u16>()))
      .ok_or(Refusal::Length)?;
    if length < size_of::<u16>() || length > cap || !length.is_multiple_of(size_of::<u16>()) {
      return Err(Refusal::Path);
    }
    let start = REQUEST_BASE.checked_add(offset).ok_or(Refusal::Length)?;
    let end = start.checked_add(length).ok_or(Refusal::Length)?;
    let bytes = self.bytes.get(start..end).ok_or(Refusal::Length)?;
    let mut units: Vec<u16> = bytes
      .as_chunks::<{ size_of::<u16>() }>()
      .0
      .iter()
      .map(|pair| u16::from_le_bytes(*pair))
      .collect();
    if units.pop() != Some(0) || units.contains(&0) {
      return Err(Refusal::Path);
    }
    let path = String::from_utf16(&units).map_err(|_| Refusal::Path)?;
    if !path.starts_with('\\') {
      return Err(Refusal::Path);
    }
    Ok(path)
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

/// Shape: the most frames one batch can hold, each at least a request's fixed part.
const FRAMES_MAX: usize = BATCH_BYTES / REQUEST_BASE;

/// Where one request sits in its batch's bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Frame {
  at: usize,
  length: usize,
}

impl Frame {
  /// The checked request this frame names in `bytes`, the batch it was indexed from.
  pub(crate) fn request<'b>(self, bytes: &'b [u8]) -> Result<Request<'b>, Refusal> {
    let tail = bytes.get(self.at..).ok_or(Refusal::Length)?;
    checked_request(tail, self.length).map(|(request, _)| request)
  }

  /// The request's hint, read without checking the rest again.
  pub(crate) fn hint(self, bytes: &[u8]) -> Result<u64, Refusal> {
    read_u64(bytes.get(self.at..).ok_or(Refusal::Length)?, HINT_AT)
  }

  /// The request's kind, read without checking the rest again.
  pub(crate) fn kind(self, bytes: &[u8]) -> Result<u32, Refusal> {
    read_u32(bytes.get(self.at..).ok_or(Refusal::Length)?, KIND_AT)
  }
}

/// A receive owner's room for one batch's index: where each request sits, and their hints for the duplicate check. It is
/// reserved for the most frames a batch can hold when the owner starts and kept across its batches, so taking a batch
/// allocates nothing (audit step 7: each batch had made two allocations and a sort, 100 batches 200 calls,
/// `tests/batch_allocations.rs`).
#[derive(Debug)]
pub struct Scratch {
  pub(crate) frames: std::collections::VecDeque<Frame>,
  hints: Vec<u64>,
}

impl Scratch {
  /// Room for the largest batch, reserved once; refused `NoMemory` when the process has none.
  pub fn new() -> Result<Self, Refusal> {
    let mut frames = std::collections::VecDeque::new();
    frames
      .try_reserve_exact(FRAMES_MAX)
      .map_err(|_| Refusal::NoMemory)?;
    let mut hints = Vec::new();
    hints
      .try_reserve_exact(FRAMES_MAX)
      .map_err(|_| Refusal::NoMemory)?;
    Ok(Self { frames, hints })
  }
}

/// Checks the complete bounded batch before any frame is taken, and indexes its frames into `scratch` in driver order. A
/// zeroed tail ends the batch; a malformed tail or repeated hint refuses it whole. The live receive owner must settle
/// these frames before receiving any fresh one.
pub fn index(bytes: &[u8], scratch: &mut Scratch) -> Result<(), Refusal> {
  scratch.frames.clear();
  scratch.hints.clear();
  if bytes.len() > BATCH_BYTES || !bytes.len().is_multiple_of(ALIGNMENT) {
    return Err(Refusal::Length);
  }
  let mut at = 0usize;
  while at < bytes.len() {
    let tail = bytes.get(at..).ok_or(Refusal::Length)?;
    let length = usize::from(read_u16(tail, SIZE_AT)?);
    if length == 0 {
      if tail.iter().any(|byte| *byte != 0) {
        scratch.frames.clear();
        return Err(Refusal::Tail);
      }
      break;
    }
    let (request, stride) = match checked_request(tail, length) {
      Ok(checked) => checked,
      Err(refusal) => {
        scratch.frames.clear();
        return Err(refusal);
      }
    };
    // Within the bound reserved for: a frame is at least REQUEST_BASE bytes of a batch of at most BATCH_BYTES.
    if scratch.frames.len() >= FRAMES_MAX {
      scratch.frames.clear();
      return Err(Refusal::Length);
    }
    scratch.hints.push(request.hint);
    scratch.frames.push_back(Frame { at, length });
    at = at.checked_add(stride).ok_or(Refusal::Length)?;
  }
  // A kernel batch cannot contain one processing IRP twice. Sort the hints' own buffer rather than the frames: cleanup
  // and close effects must still settle in the order the driver supplied them.
  scratch.hints.sort_unstable();
  if scratch
    .hints
    .windows(2)
    .any(|pair| pair.first() == pair.last())
  {
    scratch.frames.clear();
    return Err(Refusal::Hint);
  }
  Ok(())
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
  /// A-198. Do: retain a successful kernel rename with both names inline. Expect: replay can finish its exact
  /// name transition; a failed response cannot commit that transition.
  #[test]
  fn a_saved_rename_recovers_only_its_successful_inline_names() {
    let bytes = rename_request();
    let pending = requests(&bytes).unwrap();
    let request = pending.first().unwrap();
    let mut response = request.cancelled_response().unwrap();
    assert_eq!(request.successful_rename(&response).unwrap(), None);
    response[STATUS_AT..STATUS_AT + size_of::<u32>()].fill(0);
    assert_eq!(
      request.successful_rename(&response).unwrap(),
      Some(("\\old".to_owned(), "\\new".to_owned()))
    );
    response[HINT_AT] ^= 1;
    assert!(matches!(
      request.successful_rename(&response),
      Err(Refusal::Hint)
    ));
  }

  /// A-206 (2026-11-04). Do: read a successful rename whose names are 2,000 units each. Expect: both whole. Before, a name
  /// past 1,025 units refused the parse, and that refusal ended the mount's receive worker.
  #[test]
  fn a_rename_of_a_long_path_is_read_whole() {
    let from = format!("\\{}", "o".repeat(1999));
    let to = format!("\\{}", "n".repeat(1999));
    let bytes = rename_request_of(&from, &to);
    let pending = requests(&bytes).unwrap();
    let request = pending.first().unwrap();
    let mut response = request.cancelled_response().unwrap();
    response[STATUS_AT..STATUS_AT + size_of::<u32>()].fill(0);
    assert_eq!(
      request.successful_rename(&response).unwrap(),
      Some((from, to))
    );
  }

  fn rename_request() -> Vec<u8> {
    rename_request_of("\\old", "\\new")
  }

  fn rename_request_of(from: &str, to: &str) -> Vec<u8> {
    let mut bytes = vec![0; BATCH_BYTES];
    let mut payload = Vec::new();
    for (descriptor, path) in [(FILE_NAME_AT, from), (NEW_NAME_AT, to)] {
      let offset = u16::try_from(payload.len()).unwrap();
      let units: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
      let length = u16::try_from(units.len() * size_of::<u16>()).unwrap();
      bytes[descriptor..descriptor + size_of::<u16>()].copy_from_slice(&offset.to_le_bytes());
      bytes[descriptor + size_of::<u16>()..descriptor + size_of::<u16>() * 2]
        .copy_from_slice(&length.to_le_bytes());
      for unit in units {
        payload.extend_from_slice(&unit.to_le_bytes());
      }
    }
    let length = REQUEST_BASE + payload.len();
    bytes[SIZE_AT..SIZE_AT + size_of::<u16>()]
      .copy_from_slice(&u16::try_from(length).unwrap().to_le_bytes());
    bytes[KIND_AT..KIND_AT + size_of::<u32>()].copy_from_slice(&SET_INFORMATION_KIND.to_le_bytes());
    bytes[HINT_AT..HINT_AT + size_of::<u64>()].copy_from_slice(&0x1000u64.to_le_bytes());
    bytes[INFORMATION_CLASS_AT..INFORMATION_CLASS_AT + size_of::<u32>()]
      .copy_from_slice(&RENAME_CLASS.to_le_bytes());
    bytes[REQUEST_BASE..length].copy_from_slice(&payload);
    bytes
  }

  /// A-198. Do: corrupt a retained rename's path range, cap, terminator and encoding. Expect: typed refusal before
  /// the recovery worker changes any open's name or sends a successful response.
  #[test]
  fn a_hostile_saved_rename_cannot_change_an_opens_name() {
    for corrupt in 0..4 {
      let mut bytes = rename_request();
      match corrupt {
        0 => bytes[FILE_NAME_AT..FILE_NAME_AT + 2].copy_from_slice(&u16::MAX.to_le_bytes()),
        1 => bytes[FILE_NAME_AT + 2..FILE_NAME_AT + 4].copy_from_slice(&u16::MAX.to_le_bytes()),
        2 => bytes[REQUEST_BASE + 8..REQUEST_BASE + 10].copy_from_slice(&1u16.to_le_bytes()),
        _ => bytes[REQUEST_BASE + 2..REQUEST_BASE + 4].copy_from_slice(&0xD800u16.to_le_bytes()),
      }
      let pending = requests(&bytes).unwrap();
      let request = pending.first().unwrap();
      let mut response = request.cancelled_response().unwrap();
      response[STATUS_AT..STATUS_AT + size_of::<u32>()].fill(0);
      assert!(request.successful_rename(&response).is_err());
    }
  }
  use super::*;

  /// A batch's checked requests in driver order, through the index a receive owner takes.
  fn requests(bytes: &[u8]) -> Result<Vec<Request<'_>>, Refusal> {
    let mut scratch = Scratch::new()?;
    index(bytes, &mut scratch)?;
    scratch
      .frames
      .iter()
      .map(|frame| frame.request(bytes))
      .collect()
  }
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

  /// A-215's prologue (step 7). Do: finish a response of 130 bytes in a packet still holding an earlier, longer
  /// response's bytes. Expect: 136 bytes to send, its status and aligned size in the header, the six padding bytes zero,
  /// and the callback's own bytes untouched; a size whose alignment passes the cap, or below the fixed part, is refused.
  #[test]
  fn a_reused_packet_sends_no_byte_of_an_earlier_response() {
    let mut packet = vec![0xAB; RESPONSE_MAX];
    packet[SIZE_AT..SIZE_AT + 2].copy_from_slice(&130u16.to_le_bytes());
    assert_eq!(finish_response(&mut packet, -5), Ok(136));
    assert_eq!(
      u16::from_le_bytes([packet[SIZE_AT], packet[SIZE_AT + 1]]),
      136
    );
    assert_eq!(
      i32::from_le_bytes(packet[STATUS_AT..STATUS_AT + 4].try_into().unwrap()),
      -5
    );
    assert!(packet[130..136].iter().all(|byte| *byte == 0));
    assert!(packet[24..130].iter().all(|byte| *byte == 0xAB));
    assert!(packet[136..].iter().all(|byte| *byte == 0xAB));
    for size in [u16::try_from(RESPONSE_MAX).unwrap() + 1, 64, 0] {
      packet[SIZE_AT..SIZE_AT + 2].copy_from_slice(&size.to_le_bytes());
      assert_eq!(finish_response(&mut packet, 0), Err(Refusal::Length));
    }
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
