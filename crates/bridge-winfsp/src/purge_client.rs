//! The coherent purge's client, in a process of its own (A-190, §4.6 Windows, the rule of 2026-11-06): no daemon
//! thread opens a path on a volume the daemon serves.
//!
//! **Why a child.** A-190's purge opens each changed file through the drive and writes and reads its range, and only
//! the daemon's own receive worker answers those requests. A delivered request sits in WinFsp's processing queue, which
//! is never cancelled or expired (`FSP_IOQ_PROCESS_NO_CANCEL`, WinFsp v2.1 `src/sys/ioq.c:572-575`); a dying thread
//! waits for its cancelled IRPs, which Windows abandons only after 5 minutes (Microsoft Learn, "Canceling IRPs"); and
//! the volume's control handle closes only at process rundown. So the mount thread's own open strands a killed daemon
//! (`docs/bugs/2026-11-06-a-daemons-own-io-on-its-volume-strands-its-exit.md`). Here the opens, writes and reads are
//! made by a child, a role of the program the daemon's owner installs ([`install`]). The mount thread only writes a
//! request to the child's pipe and reads its answer, which a dying thread's cancellation ends at once. A killed
//! daemon's job ends the child, whose delivered request the successor settles (A-198), or the volume's deletion does.
//!
//! **No other mechanism.** WinFsp purges a mapped file's pages only on a non-cached write
//! (`FspFsvolWriteNonCached` → `CcCoherencyFlushAndPurgeCache`); its notification purges with `CcPurgeCacheSection`,
//! which leaves mapped files (A-190). Every way to reach the coherent purge is a request on the volume, so whoever
//! makes it must not be a daemon thread.
//!
//! **Recognition.** The child makes its opens under a restricted copy of its own token carrying this mount's private
//! marker ([`crate::purge_token`]), which it is told at its start. The mount thread publishes each purge, with the
//! child's process id, before it asks the child (`crate::purge_intents`); the receiver takes a Create as the purge's only
//! when its process is that child's and its token carries the marker (`crate::dispatcher::PrivateOpens`).
//!
//! **The protocol.** One message pipe (`slates_ipc::stream::message_pair`). The child answers its start with one
//! ready byte once its token is made. Each request is the purge's range, piece size and full drive path; each answer is
//! one outcome byte. The pipe's end at the parent closing (the daemon gone) ends the child.

use std::ffi::{OsStr, OsString};
use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::OnceLock;

use slates_ipc::stream::{message_pair, read_waiting, write_waiting};
use slates_ipc::win_spawn::{
  ClosedOnDrop, Process, Spawn, StandardOutput, clear_inheritance, duplicate_inheritable,
  environment_block, kill_on_close_job,
};
use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
  CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_NO_BUFFERING, FILE_FLAG_WRITE_THROUGH,
  FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile, WriteFile,
};
use windows_sys::Win32::System::IO::OVERLAPPED;

use crate::host::WinFspError;
use crate::purge::{Purge, PurgeOutcome, PurgeTarget, purge_through};
use crate::purge_token::{Marker, PurgeToken};

/// Format: the first argument that selects the purge client's role in the `slates` binary (`slates
/// winfsp-purge-client`, never listed in its usage: only a daemon starts it).
pub const ROLE_ARG: &str = "winfsp-purge-client";

/// Format: the variable that hands the child its pipe end and the mount's marker, `<end in hex>,<marker in hex>`.
pub const PURGE_CLIENT_ENV: &str = "SLATES_WINFSP_PURGE_CLIENT";

/// Format: the child's answer that its token is made and it takes requests.
const READY: u8 = 0xA5;
/// Format: an answer byte per outcome.
const TAKEN: u8 = 1;
/// Format: the purge's pages were kept (`PurgeOutcome::Untaken`).
const UNTAKEN: u8 = 2;
/// Format: the purge's open, a write or a read back was refused (`PurgeOutcome::Refused`).
const REFUSED: u8 = 3;
/// Format: a request's piece size follows its offset and length (each a `u64`).
const PIECE_AT: usize = 2 * WORD;
/// Format: the path's length follows the piece size (a `u32`).
const PATH_LEN_AT: usize = PIECE_AT + HALF;
/// Format: a request's fixed head: offset, length, piece and path length.
const HEAD_BYTES: usize = PATH_LEN_AT + HALF;
/// Derived: the longest drive path a request carries, Windows' 32,767 UTF-16 units of an extended path at three UTF-8
/// bytes each (Microsoft Learn, "Maximum Path Length Limitation").
const PATH_BYTES: usize = 32_767 * 3;
/// Derived: the largest message either end sends: a request's head and its longest path.
const FRAME_BYTES: usize = HEAD_BYTES + PATH_BYTES;
/// Format: the child's exit when its parent closed the pipe, its normal end.
pub const EXIT_ENDED: i32 = 0;
/// Format: the child's exit when its variable is missing or malformed.
pub const EXIT_UNSET: i32 = 2;
/// Format: the child's exit when its token, its ready byte or a request failed.
pub const EXIT_FAILED: i32 = 3;
/// Format: the base the handle value and the marker are written in.
const HEX: u32 = 16;
/// Format: the bytes of a request's `u64` fields.
const WORD: usize = size_of::<u64>();
/// Format: the bytes of a request's `u32` fields.
const HALF: usize = size_of::<u32>();
/// Format: the sector unbuffered transfers align to (the volume's `sector_size`, `crate::host`).
const SECTOR_BYTES: usize = 4096;

/// The program the daemon's owner runs as a mount's purge client: a role of its own binary (`slates
/// winfsp-purge-client`), or of a test binary.
#[derive(Clone, Debug)]
pub struct Program {
  /// The executable.
  pub program: OsString,
  /// Its arguments, which select the role.
  pub args: Vec<OsString>,
}

static PROGRAM: OnceLock<Program> = OnceLock::new();

/// Installs the purge client's program for this process, once: whether this call installed it. A mount whose process
/// has none makes no purge and counts each one unavailable (`crate::host::Purged::unavailable`).
pub fn install(program: Program) -> bool {
  PROGRAM.set(program).is_ok()
}

/// The daemon's side: the running child, its pipe end and the job that ends it with this process.
pub(crate) struct Client {
  marker: Marker,
  running: Option<Running>,
}

struct Running {
  process: Process,
  end: OwnedHandle,
  /// Held only here: when this process ends, by any means, the kernel closes it and the child ends too.
  _job: ClosedOnDrop,
}

impl Client {
  pub(crate) fn new(marker: Marker) -> Self {
    Self {
      marker,
      running: None,
    }
  }

  /// The running child's process id, starting it first when none runs: `None` when no program is installed.
  pub(crate) fn ready(&mut self) -> Result<Option<u32>, WinFspError> {
    if let Some(running) = self.running.as_mut()
      && running
        .process
        .try_wait()
        .map_err(|_| os_error("purge client wait"))?
        .is_none()
    {
      return Ok(Some(running.process.id()));
    }
    self.running = None;
    let Some(program) = PROGRAM.get() else {
      return Ok(None);
    };
    let running = start(program, self.marker)?;
    let id = running.process.id();
    self.running = Some(running);
    Ok(Some(id))
  }

  /// Has the running child purge `purge` through `path` (the full drive path) in pieces of `piece` bytes. A child that
  /// cannot be asked or does not answer is let go (the next purge starts another) and the purge is refused.
  pub(crate) fn purge(&mut self, path: &str, purge: &Purge, piece: u32) -> PurgeOutcome {
    let Some(running) = self.running.as_mut() else {
      return PurgeOutcome::Refused;
    };
    let mut frame = Vec::with_capacity(HEAD_BYTES.saturating_add(path.len()));
    frame.extend_from_slice(&purge.offset.to_le_bytes());
    frame.extend_from_slice(&purge.len.to_le_bytes());
    frame.extend_from_slice(&piece.to_le_bytes());
    let Ok(path_len) = u32::try_from(path.len()) else {
      return PurgeOutcome::Refused;
    };
    frame.extend_from_slice(&path_len.to_le_bytes());
    frame.extend_from_slice(path.as_bytes());
    let mut answer = [0u8; 1];
    let asked = frame.len() <= FRAME_BYTES
      && write_waiting(running.end.as_handle(), &frame).is_ok_and(|n| n == frame.len())
      && read_waiting(running.end.as_handle(), &mut answer).is_ok_and(|n| n == 1);
    match (asked, answer) {
      (true, [TAKEN]) => PurgeOutcome::Taken,
      (true, [UNTAKEN]) => PurgeOutcome::Untaken,
      (true, [REFUSED]) => PurgeOutcome::Refused,
      _ => {
        self.let_go();
        PurgeOutcome::Refused
      }
    }
  }

  fn let_go(&mut self) {
    if let Some(mut running) = self.running.take()
      && running.process.kill().is_err()
    {
      slates_machine::log_line!("slates-winfsp: the purge client could not be ended");
    }
  }
}

/// Starts the child in a job of its own, hands it its pipe end and the marker, and waits for its ready byte.
fn start(program: &Program, marker: Marker) -> Result<Running, WinFspError> {
  let (end, handed) = message_pair(FRAME_BYTES).map_err(|_| os_error("purge client pipe"))?;
  let inheritable =
    duplicate_inheritable(handed.as_raw_handle()).map_err(|_| os_error("purge client end"))?;
  drop(handed);
  let job = kill_on_close_job().map_err(|_| os_error("purge client job"))?;
  let value = format!("{:x},{}", inheritable.raw().addr(), marker.to_hex());
  let block = environment_block(&[(OsStr::new(PURGE_CLIENT_ENV), OsStr::new(&value))]);
  let process = slates_ipc::win_spawn::spawn(&Spawn {
    program: &program.program,
    args: &program.args,
    environment: Some(&block),
    inherited: &[inheritable.raw()],
    output: StandardOutput::Inherited,
    job: Some(job.raw()),
    process_security: None,
  })
  .map_err(|_| os_error("purge client spawn"))?;
  drop(inheritable);
  let mut running = Running {
    process,
    end,
    _job: job,
  };
  let mut ready = [0u8; 1];
  if read_waiting(running.end.as_handle(), &mut ready).ok() != Some(1) || ready != [READY] {
    if running.process.kill().is_err() {
      slates_machine::log_line!(
        "slates-winfsp: a purge client that did not start could not be ended"
      );
    }
    return Err(WinFspError::Os {
      call: "purge client start",
      code: None,
    });
  }
  Ok(running)
}

/// The child's role: takes its pipe end and marker from [`PURGE_CLIENT_ENV`], makes its restricted token, says it is
/// ready, then makes each purge it is asked under that token until the parent closes the pipe. Its exit code: one of
/// [`EXIT_ENDED`], [`EXIT_UNSET`] and [`EXIT_FAILED`].
pub fn serve_from_env() -> i32 {
  let Some((end, marker)) = std::env::var(PURGE_CLIENT_ENV)
    .ok()
    .as_deref()
    .and_then(parse_env)
  else {
    return EXIT_UNSET;
  };
  let Ok(token) = PurgeToken::with_marker(marker) else {
    return EXIT_FAILED;
  };
  if write_waiting(end.as_handle(), &[READY]).ok() != Some(1) {
    return EXIT_FAILED;
  }
  let mut frame = vec![0u8; FRAME_BYTES];
  let mut buffers = Buffers::new();
  loop {
    let read = match read_waiting(end.as_handle(), &mut frame) {
      Ok(0) | Err(_) => return EXIT_ENDED,
      Ok(read) => read,
    };
    let answer = match frame.get(..read).and_then(decode) {
      Some((path, purge, piece)) => {
        match token.during(|| Ok(purge_one(&path, &purge, piece, &mut buffers))) {
          Ok(PurgeOutcome::Taken) => TAKEN,
          Ok(PurgeOutcome::Untaken) => UNTAKEN,
          Ok(PurgeOutcome::Refused) | Err(_) => REFUSED,
        }
      }
      None => REFUSED,
    };
    if write_waiting(end.as_handle(), &[answer]).ok() != Some(1) {
      return EXIT_ENDED;
    }
  }
}

/// The pipe end and marker [`PURGE_CLIENT_ENV`] names; the end is taken over, no longer inheritable.
fn parse_env(text: &str) -> Option<(OwnedHandle, Marker)> {
  let (end, marker) = text.split_once(',')?;
  let marker = Marker::from_hex(marker)?;
  let value = usize::from_str_radix(end, HEX).ok()?;
  // SAFETY: the parent duplicated this end into this process at this value for this role alone, which takes it once.
  let end = unsafe { OwnedHandle::from_raw_handle(std::ptr::without_provenance_mut(value)) };
  clear_inheritance(&end).ok()?;
  Some((end, marker))
}

/// A request's full drive path, range and piece.
fn decode(frame: &[u8]) -> Option<(String, Purge, u32)> {
  let word = |at: usize| -> Option<u64> {
    Some(u64::from_le_bytes(*frame.get(at..)?.first_chunk::<WORD>()?))
  };
  let half = |at: usize| -> Option<u32> {
    Some(u32::from_le_bytes(*frame.get(at..)?.first_chunk::<HALF>()?))
  };
  let offset = word(0)?;
  let len = word(size_of::<u64>())?;
  let piece = half(PIECE_AT)?;
  let path_len = usize::try_from(half(PATH_LEN_AT)?).ok()?;
  let path = frame.get(HEAD_BYTES..HEAD_BYTES.checked_add(path_len)?)?;
  let path = String::from_utf8(path.to_vec()).ok()?;
  let purge = Purge {
    object: slates_bridge_core::ObjectId::new(0, 0),
    path: String::new(),
    offset,
    len,
  };
  Some((path, purge, piece))
}

/// The child's zeros and its aligned read buffer, made once and reused, grown to the largest piece asked.
struct Buffers {
  zeros: AlignedBytes,
  aligned: AlignedBytes,
}

impl Buffers {
  fn new() -> Self {
    Self {
      zeros: AlignedBytes::new(0, SECTOR_BYTES),
      aligned: AlignedBytes::new(0, SECTOR_BYTES),
    }
  }

  fn fit(&mut self, piece: usize) {
    if self.zeros.len < piece {
      self.zeros = AlignedBytes::new(piece, SECTOR_BYTES);
      self.aligned = AlignedBytes::new(piece, SECTOR_BYTES);
    }
  }
}

/// One file's purge (`crate::purge::purge_through`) through an unbuffered open of it, which writes the range and reads
/// it around Windows' cache, and a cached open made for the read through the cache.
fn purge_one(path: &str, purge: &Purge, piece: u32, buffers: &mut Buffers) -> PurgeOutcome {
  let Ok(piece_bytes) = usize::try_from(piece) else {
    return PurgeOutcome::Refused;
  };
  buffers.fit(piece_bytes);
  let path: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
  let Some(unbuffered) = open_through_drive(
    &path,
    GENERIC_READ | GENERIC_WRITE,
    FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH,
  ) else {
    return PurgeOutcome::Refused;
  };
  let Buffers { zeros, aligned } = buffers;
  let mut file = DriveFile {
    path: &path,
    unbuffered,
    cached: None,
    zeros: zeros.bytes(),
    aligned,
  };
  purge_through(&mut file, purge, piece)
}

/// Opens the file at `path` (a NUL-terminated UTF-16 path through the drive) with `access` and `flags`, sharing it
/// whole: its handle, or `None` when Windows refused the open.
fn open_through_drive(path: &[u16], access: u32, flags: u32) -> Option<OwnedHandle> {
  // SAFETY: `path` is a live NUL-terminated UTF-16 string for the call; no security attributes and no template.
  let raw = unsafe {
    CreateFileW(
      path.as_ptr(),
      access,
      FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
      std::ptr::null(),
      OPEN_EXISTING,
      flags,
      std::ptr::null_mut(),
    )
  };
  if raw == INVALID_HANDLE_VALUE || raw.is_null() {
    return None;
  }
  // SAFETY: a handle the call above just opened and nothing else holds; owned from here and closed on drop.
  Some(unsafe { OwnedHandle::from_raw_handle(raw) })
}

/// A file a purge works on through the drive ([`PurgeTarget`]): its unbuffered open, which writes the purge and reads
/// around Windows' cache into a sector-aligned buffer, and a cached open for the read through the cache, made at the
/// first such read.
struct DriveFile<'p> {
  path: &'p [u16],
  unbuffered: OwnedHandle,
  cached: Option<OwnedHandle>,
  zeros: &'p [u8],
  aligned: &'p mut AlignedBytes,
}

impl PurgeTarget for DriveFile<'_> {
  fn write_zeros(&mut self, offset: u64, len: u32) -> bool {
    write_zeros_at(&self.unbuffered, self.zeros, offset, len)
  }

  fn read_cached(&mut self, offset: u64, len: u32, out: &mut Vec<u8>) -> bool {
    if self.cached.is_none() {
      self.cached = open_through_drive(self.path, GENERIC_READ, FILE_ATTRIBUTE_NORMAL);
    }
    let Some(handle) = self.cached.as_ref() else {
      return false;
    };
    out.clear();
    out.resize(usize::try_from(len).unwrap_or(0), 0);
    let Some(read) = read_at(handle, out, offset) else {
      return false;
    };
    out.truncate(read);
    true
  }

  fn read_uncached(&mut self, offset: u64, len: u32, out: &mut Vec<u8>) -> bool {
    let Some(buffer) = usize::try_from(len)
      .ok()
      .and_then(|len| self.aligned.bytes_mut().get_mut(..len))
    else {
      return false;
    };
    let Some(read) = read_at(&self.unbuffered, buffer, offset) else {
      return false;
    };
    out.clear();
    out.extend_from_slice(buffer.get(..read).unwrap_or_default());
    true
  }
}

/// The `OVERLAPPED` that names `offset` for a synchronous transfer.
fn at_offset(offset: u64) -> OVERLAPPED {
  let mut overlapped = OVERLAPPED::default();
  overlapped.Anonymous.Anonymous.Offset = u32::try_from(offset & u64::from(u32::MAX)).unwrap_or(0);
  overlapped.Anonymous.Anonymous.OffsetHigh = u32::try_from(offset >> u32::BITS).unwrap_or(0);
  overlapped
}

/// Reads into `buffer` at `offset` through `handle`, synchronously: how many bytes were read, or `None` when Windows
/// refused the read.
fn read_at(handle: &OwnedHandle, buffer: &mut [u8], offset: u64) -> Option<usize> {
  let len = u32::try_from(buffer.len()).ok()?;
  let mut overlapped = at_offset(offset);
  let mut read = 0u32;
  // SAFETY: a live handle opened synchronously, a buffer of `len` writable bytes, and an OVERLAPPED on this frame that
  // only names the offset (a synchronous handle completes the read before the call returns).
  let done = unsafe {
    ReadFile(
      handle.as_raw_handle(),
      buffer.as_mut_ptr(),
      len,
      &mut read,
      &mut overlapped,
    )
  };
  if done == 0 {
    return None;
  }
  usize::try_from(read).ok()
}

/// Writes `len` bytes of `zeros` at `offset` through `handle`, synchronously: whether the whole length was written.
fn write_zeros_at(handle: &OwnedHandle, zeros: &[u8], offset: u64, len: u32) -> bool {
  let Some(buffer) = usize::try_from(len).ok().and_then(|len| zeros.get(..len)) else {
    return false;
  };
  let mut overlapped = at_offset(offset);
  let mut written = 0u32;
  // SAFETY: a live handle opened synchronously, a buffer of `len` bytes, and an OVERLAPPED on this frame that only
  // names the offset (a synchronous handle completes the write before the call returns).
  let done = unsafe {
    WriteFile(
      handle.as_raw_handle(),
      buffer.as_ptr(),
      len,
      &mut written,
      &mut overlapped,
    )
  };
  done != 0 && written == len
}

/// A buffer, zeroed when made, whose first byte lies at a multiple of the alignment an unbuffered transfer asks (a
/// sector), found within a larger allocation, so no allocator call needs to be unsafe: a purge's zeros, and the bytes
/// an unbuffered read lands in.
struct AlignedBytes {
  storage: Vec<u8>,
  start: usize,
  len: usize,
}

impl AlignedBytes {
  /// `len` zeroed bytes aligned to `align`.
  fn new(len: usize, align: usize) -> AlignedBytes {
    let storage = vec![0u8; len.saturating_add(align)];
    let start = storage.as_ptr().align_offset(align).min(align);
    AlignedBytes {
      storage,
      start,
      len,
    }
  }

  /// The aligned bytes (empty if the allocation could not hold them).
  fn bytes(&self) -> &[u8] {
    self
      .storage
      .get(self.start..self.start.saturating_add(self.len))
      .unwrap_or(&[])
  }

  /// The aligned bytes, to write into (empty if the allocation could not hold them).
  fn bytes_mut(&mut self) -> &mut [u8] {
    let end = self.start.saturating_add(self.len);
    self.storage.get_mut(self.start..end).unwrap_or_default()
  }
}

fn os_error(call: &'static str) -> WinFspError {
  WinFspError::Os {
    call,
    code: std::io::Error::last_os_error().raw_os_error(),
  }
}

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used)]
  use super::*;

  /// A-190 (the client's protocol). Do: encode a request as the parent does and decode it as the child does; then a
  /// marker through its text. Expect: the same range, piece and path, and the same marker, and a truncated request
  /// refused.
  #[test]
  fn a_request_and_a_marker_survive_their_encodings() {
    let path = "\\\\?\\S:\\dir\\file";
    let mut frame = Vec::new();
    frame.extend_from_slice(&8192u64.to_le_bytes());
    frame.extend_from_slice(&4096u64.to_le_bytes());
    frame.extend_from_slice(&65536u32.to_le_bytes());
    frame.extend_from_slice(&u32::try_from(path.len()).unwrap().to_le_bytes());
    frame.extend_from_slice(path.as_bytes());
    let (decoded, purge, piece) = decode(&frame).unwrap();
    assert_eq!(
      (decoded.as_str(), purge.offset, purge.len, piece),
      (path, 8192, 4096, 65536)
    );
    assert!(decode(&frame[..frame.len() - 1]).is_none());
    let marker = Marker::fresh().unwrap();
    assert_eq!(
      Marker::from_hex(&marker.to_hex()).unwrap().to_hex(),
      marker.to_hex()
    );
    assert!(Marker::from_hex("zz").is_none());
  }
}
