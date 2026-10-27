//! The stream a guest's transport rides on Windows (§4.6, the Shards fs route; A-161, A-179): a duplex named pipe
//! connected to itself, the Windows arm of `slates_ipc::stream` (the Unix arm is an anonymous socket pair). The
//! daemon keeps the server end and serves one attachment on it; the client end is the attachment's capability, handed
//! to whoever attached by the handle route (`crate::handle_route::hand_stream`, A-175's duplication).
//!
//! **Why a named pipe.** Windows has no `socketpair`, and an AF_UNIX socket at an abstract address cannot be connected
//! to, so an AF_UNIX pair needs a socket file on disk, which R1 forbids. A named pipe lives in the kernel's pipe
//! namespace (NPFS), not on any volume, and a pair costs 9.9 to 14.3 µs at p50 against an AF_UNIX pair's 0.6 ms, with
//! the shorter round trip of the two on every host measured (Shards' PM M340: jupiter, and GitHub's windows-2022 and
//! windows-2025 runners, 2026-10-26).
//!
//! **Why only the handed end reaches the attachment.** A name in the pipe namespace can be opened by anyone who knows
//! it and whom the pipe's descriptor admits, so the pair closes every way in before it is handed:
//! - the name is fresh, 128 bits from the OS's random source (`BCryptGenRandom` with the system's preferred generator),
//!   so no one can guess it before the pair connects;
//! - `FILE_FLAG_FIRST_PIPE_INSTANCE` refuses the create if any pipe of that name exists, so no squatter's server end
//!   can be the one the daemon's client end connects to;
//! - one instance (`nMaxInstances` 1), connected at once by the daemon's own open of the client end, so any later open
//!   of the name, by any process, is refused `ERROR_PIPE_BUSY`;
//! - `PIPE_REJECT_REMOTE_CLIENTS`, so no other machine connects over SMB;
//! - a descriptor that admits the owner alone ([`OWNER_ONLY_SDDL`]);
//! - and the client end is opened with `SECURITY_SQOS_PRESENT | SECURITY_ANONYMOUS`, so the daemon's server end can
//!   never impersonate whoever holds the client end.
//!
//! Ending the daemon's end without closing it (a detach, a revocation, a destroy) is the runtime's
//! (`slates_rt::pipe::PipeEnd::disconnect`), since the runtime's driver owns every operation in the kernel on that end.
//!
//! Neither end is inheritable, so no child the daemon or a client spawns gets one by accident (a client that hands its
//! end to a VMM duplicates it into that process on purpose). Both ends are opened for overlapped I/O
//! (`FILE_FLAG_OVERLAPPED`): the daemon's because its shard's completion driver serves it (`slates_rt::pipe`), the
//! handed one because Shards' device waits on an event per direction (agreed with the Shards owner, 2026-10-26). The mode
//! is fixed per open, so a holder of the handed end must pass an `OVERLAPPED` to every read and write; [`read_waiting`]
//! and [`write_waiting`] do that for a client that only wants to wait.

use std::os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};

use windows_sys::Win32::Foundation::{
  ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, GetLastError,
  INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
  ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::Cryptography::{
  BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::Storage::FileSystem::{
  CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FileNameInfo,
  GetFileInformationByHandleEx, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, SECURITY_ANONYMOUS,
  SECURITY_SQOS_PRESENT, WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
  ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
  PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::CreateEventW;

use crate::error::IpcError;

/// Format: the prefix of every Shards stream's pipe name, in the local pipe namespace. What follows is the name's
/// random part in hex.
const NAME_PREFIX: &str = r"\\.\pipe\slates-shards-";

/// Format: the bytes of a pipe name's random part: 128 bits, as the plan for the Shards route on Windows asks (A-179).
/// At 2^-64 odds of a collision after 2^32 names, a guess is hopeless and a collision is refused anyway
/// (`FILE_FLAG_FIRST_PIPE_INSTANCE`).
const NAME_RANDOM_BYTES: usize = 16;

/// Format: the instances a Shards stream's pipe allows: one, which the daemon's own open of the client end connects at
/// once, so every later open of the name is refused `ERROR_PIPE_BUSY`.
const ONE_INSTANCE: u32 = 1;

/// Format: the default time-out `CreateNamedPipeW` takes: zero, the system's 50 ms, which only `WaitNamedPipe` reads,
/// and nothing here calls it.
const NO_WAIT_TIMEOUT: u32 = 0;

/// Format: the descriptor of a Shards stream's pipe: a protected DACL whose one entry grants the owner every access
/// (`OW`, the owner-rights SID, which also takes the place of the owner's implicit rights), so no other user may open
/// the name and no one but the owner may change the descriptor.
pub const OWNER_ONLY_SDDL: &str = "D:P(A;;GA;;;OW)";

/// Format: the largest `FILE_NAME_INFO` the kernel returns: its length word and a name of at most the 32,767 UTF-16
/// units a Windows path holds, in 32-bit words so the record is aligned as the structure asks.
const NAME_INFO_WORDS: usize = (std::mem::size_of::<u32>() + 2 * 32_767).div_ceil(4);

/// This thread's last error code, read before any other call.
fn last_error() -> u32 {
  // SAFETY: reads this thread's last error; no memory is touched.
  unsafe { GetLastError() }
}

/// The OS refused `call`: its error code, read before any other call.
fn os(call: &'static str) -> IpcError {
  IpcError::OsRefused {
    call,
    code: i32::try_from(last_error()).ok(),
  }
}

/// A connected duplex pipe: the daemon's end (the server end) and the end handed out (the client end). Both are
/// overlapped and neither is inheritable. The name is fresh and the pipe allows one instance, already connected, so the
/// handed end is the only way in (see the module doc).
///
/// Each direction buffers `frame_bytes`, the largest frame its owner sends, so a frame's write completes without
/// waiting for its reader. The size is the pipe's quota: a write past what the reader has left buffered waits until it
/// reads. With the system's default of zero every write waited for its reader, and a client that writes a request and
/// then reads its reply on one thread never finished its write (measured on jupiter 2026-10-26: this module's own
/// round-trip test hung). Refused `Layout` for a size past what `CreateNamedPipeW` takes (a `u32`).
pub fn pair(frame_bytes: usize) -> Result<(OwnedHandle, OwnedHandle), IpcError> {
  let buffer = u32::try_from(frame_bytes).map_err(|_| IpcError::Layout {
    reason: "a pipe buffer past what CreateNamedPipeW takes",
  })?;
  let name = fresh_name()?;
  let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
  let descriptor = OwnerOnly::new()?;
  let attributes = SECURITY_ATTRIBUTES {
    nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(u32::MAX),
    lpSecurityDescriptor: descriptor.0,
    bInheritHandle: 0,
  };
  // SAFETY: a NUL-terminated name and attributes whose descriptor outlives the call; the handle returned is checked and
  // owned below.
  let server = unsafe {
    CreateNamedPipeW(
      wide.as_ptr(),
      PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
      PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
      ONE_INSTANCE,
      buffer,
      buffer,
      NO_WAIT_TIMEOUT,
      &attributes,
    )
  };
  if server == INVALID_HANDLE_VALUE || server.is_null() {
    return Err(os("CreateNamedPipeW"));
  }
  // SAFETY: the server end just made, owned here alone and closed on drop.
  let server = unsafe { OwnedHandle::from_raw_handle(server) };
  // SAFETY: the same NUL-terminated name; no attributes, so the handle is not inheritable; the handle returned is
  // checked and owned below.
  let client = unsafe {
    CreateFileW(
      wide.as_ptr(),
      GENERIC_READ | GENERIC_WRITE,
      0,
      std::ptr::null(),
      OPEN_EXISTING,
      FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_ANONYMOUS,
      std::ptr::null_mut(),
    )
  };
  if client == INVALID_HANDLE_VALUE || client.is_null() {
    return Err(os("CreateFileW of the pipe's client end"));
  }
  // SAFETY: the client end just opened, owned here alone and closed on drop.
  let client = unsafe { OwnedHandle::from_raw_handle(client) };
  connected(&server)?;
  Ok((server, client))
}

/// A fresh pipe name: [`NAME_PREFIX`] and [`NAME_RANDOM_BYTES`] from the OS's random source, in hex.
fn fresh_name() -> Result<String, IpcError> {
  let mut bytes = [0u8; NAME_RANDOM_BYTES];
  let length = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
  // SAFETY: no algorithm handle with the system's preferred generator, and a buffer of `length` bytes.
  let status = unsafe {
    BCryptGenRandom(
      std::ptr::null_mut(),
      bytes.as_mut_ptr(),
      length,
      BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    )
  };
  if status != 0 {
    return Err(IpcError::OsRefused {
      call: "BCryptGenRandom",
      code: Some(status),
    });
  }
  let mut name = String::with_capacity(NAME_PREFIX.len().saturating_add(2 * NAME_RANDOM_BYTES));
  name.push_str(NAME_PREFIX);
  for byte in bytes {
    use std::fmt::Write as _;
    let _infallible = write!(name, "{byte:02x}");
  }
  Ok(name)
}

/// The owner-only descriptor, freed on drop.
struct OwnerOnly(PSECURITY_DESCRIPTOR);

impl OwnerOnly {
  fn new() -> Result<OwnerOnly, IpcError> {
    let text: Vec<u16> = OWNER_ONLY_SDDL
      .encode_utf16()
      .chain(std::iter::once(0))
      .collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: a NUL-terminated SDDL string and an out-pointer to a local; the descriptor is freed by `Drop`.
    let parsed = unsafe {
      ConvertStringSecurityDescriptorToSecurityDescriptorW(
        text.as_ptr(),
        SDDL_REVISION_1,
        &mut descriptor,
        std::ptr::null_mut(),
      )
    };
    if parsed == 0 {
      return Err(os("ConvertStringSecurityDescriptorToSecurityDescriptorW"));
    }
    Ok(OwnerOnly(descriptor))
  }
}

impl Drop for OwnerOnly {
  fn drop(&mut self) {
    // SAFETY: the descriptor `ConvertStringSecurityDescriptorToSecurityDescriptorW` allocated, freed once.
    unsafe { LocalFree(self.0) };
  }
}

/// Confirms the server end is connected: the client end was opened before this, so `ConnectNamedPipe` answers
/// `ERROR_PIPE_CONNECTED` and starts nothing. Any other answer refuses the pair. One that pends cannot happen with the
/// client already open; if it ever did, the wait is cancelled and its end awaited here, so no `OVERLAPPED` on this
/// stack frame is left with the kernel.
fn connected(server: &OwnedHandle) -> Result<(), IpcError> {
  let mut overlapped = OVERLAPPED::default();
  // SAFETY: the live server end and an OVERLAPPED on this frame, which outlives any operation it starts (awaited below
  // before return).
  let done = unsafe { ConnectNamedPipe(server.as_raw_handle(), &mut overlapped) };
  let code = if done == 0 { last_error() } else { 0 };
  if done != 0 || code == ERROR_PIPE_CONNECTED {
    return Ok(());
  }
  if code == ERROR_IO_PENDING {
    let mut moved = 0u32;
    // SAFETY: cancels the one operation the OVERLAPPED above started, then waits for its end, so the kernel holds it
    // no longer when this frame returns.
    unsafe {
      CancelIoEx(server.as_raw_handle(), &overlapped);
      GetOverlappedResult(server.as_raw_handle(), &overlapped, &mut moved, 1);
    }
  }
  Err(IpcError::OsRefused {
    call: "ConnectNamedPipe",
    code: i32::try_from(code).ok(),
  })
}

/// The name of the pipe `end` is an end of, as the kernel reports it from the handle (`FileNameInfo`, a
/// `FILE_NAME_INFO`): the pipe's
/// name in its namespace, without the `\\.\pipe` device prefix (`\slates-shards-…`). Any handle of the pipe answers,
/// a duplicate or an inherited copy, which is how a holder of an attachment's stream is told from one that names
/// another pipe.
pub fn name_of(end: BorrowedHandle<'_>) -> Result<String, IpcError> {
  let mut record = vec![0u32; NAME_INFO_WORDS];
  let bytes =
    u32::try_from(record.len().saturating_mul(std::mem::size_of::<u32>())).unwrap_or(u32::MAX);
  // SAFETY: a live handle and a buffer of `bytes` bytes, aligned for `FILE_NAME_INFO` (32-bit words).
  let answered = unsafe {
    GetFileInformationByHandleEx(
      end.as_raw_handle(),
      FileNameInfo,
      record.as_mut_ptr().cast(),
      bytes,
    )
  };
  if answered == 0 {
    return Err(os("GetFileInformationByHandleEx(FileNameInfo)"));
  }
  let name_bytes = record.first().copied().unwrap_or(0);
  let units = usize::try_from(name_bytes / 2).unwrap_or(0);
  // The name's UTF-16 units follow the length word; read out of the words as little-endian pairs, so nothing is cast.
  let wide: Vec<u16> = record
    .iter()
    .skip(1)
    .flat_map(|word| {
      let le = word.to_le_bytes();
      [
        u16::from_le_bytes([le[0], le[1]]),
        u16::from_le_bytes([le[2], le[3]]),
      ]
    })
    .take(units)
    .collect();
  String::from_utf16(&wide).map_err(|_| IpcError::Layout {
    reason: "a pipe name that is not UTF-16",
  })
}

/// Which way a waited operation goes.
#[derive(Clone, Copy)]
enum Way {
  Read,
  Write,
}

/// Reads what has arrived on `end`, an overlapped end (the handed one), into `into`, waiting until something has: the
/// bytes read, or zero once the peer has gone. For a client that only wants to wait, such as a test or a probe; the
/// daemon's end is served by its shard's driver instead.
pub fn read_waiting(end: BorrowedHandle<'_>, into: &mut [u8]) -> Result<usize, IpcError> {
  waited(end, Way::Read, into.as_mut_ptr(), into.len())
}

/// Writes `bytes` to `end`, an overlapped end (the handed one), waiting until the kernel has taken them: the bytes
/// written. A byte-mode pipe's write finishes whole unless the peer goes first, which is refused.
pub fn write_waiting(end: BorrowedHandle<'_>, bytes: &[u8]) -> Result<usize, IpcError> {
  waited(end, Way::Write, bytes.as_ptr().cast_mut(), bytes.len())
}

/// One overlapped read or write on `end` of `length` bytes at `buffer`, waited for on a manual-reset event of its own,
/// as an overlapped handle requires (a null `OVERLAPPED` on one is invalid). A gone peer reads zero.
fn waited(
  end: BorrowedHandle<'_>,
  way: Way,
  buffer: *mut u8,
  length: usize,
) -> Result<usize, IpcError> {
  // SAFETY: a nameless manual-reset event, unsignalled, not inheritable; checked and owned below.
  let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
  if event.is_null() {
    return Err(os("CreateEventW"));
  }
  // SAFETY: the event just made, owned here alone and closed on drop, after the operation it waits for has ended.
  let event = unsafe { OwnedHandle::from_raw_handle(event) };
  let mut overlapped = OVERLAPPED {
    hEvent: event.as_raw_handle(),
    ..OVERLAPPED::default()
  };
  let length = u32::try_from(length).unwrap_or(u32::MAX);
  // SAFETY: the caller's buffer holds `length` bytes and outlives the call, and the OVERLAPPED is on this frame; the
  // operation is waited for below before either goes, so the kernel never writes into either after return.
  let started = unsafe {
    match way {
      Way::Read => ReadFile(
        end.as_raw_handle(),
        buffer,
        length,
        std::ptr::null_mut(),
        &mut overlapped,
      ),
      Way::Write => WriteFile(
        end.as_raw_handle(),
        buffer.cast_const(),
        length,
        std::ptr::null_mut(),
        &mut overlapped,
      ),
    }
  };
  let code = if started == 0 { last_error() } else { 0 };
  if started == 0 && code != ERROR_IO_PENDING {
    return gone_or_refused(code, way);
  }
  let mut moved = 0u32;
  // SAFETY: waits for the operation the OVERLAPPED above started on this live handle; `moved` is a local.
  let finished = unsafe { GetOverlappedResult(end.as_raw_handle(), &overlapped, &mut moved, 1) };
  if finished == 0 {
    return gone_or_refused(last_error(), way);
  }
  Ok(usize::try_from(moved).unwrap_or(0))
}

/// After a refused or failed operation with `code`: a read of a gone peer is zero bytes, as on Unix; anything else is
/// refused.
fn gone_or_refused(code: u32, way: Way) -> Result<usize, IpcError> {
  use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_NOT_CONNECTED,
  };
  let gone = matches!(
    code,
    ERROR_BROKEN_PIPE | ERROR_NO_DATA | ERROR_PIPE_NOT_CONNECTED
  );
  if gone && matches!(way, Way::Read) {
    return Ok(0);
  }
  Err(IpcError::OsRefused {
    call: match way {
      Way::Read => "ReadFile",
      Way::Write => "WriteFile",
    },
    code: i32::try_from(code).ok(),
  })
}

/// A program [`spawn_with`] started holding a stream's end, inside a job that ends it when this process ends. This
/// process's own copy of the end is held here too until [`HandedChild::let_go_of_end`], or until this value goes.
pub struct HandedChild {
  process: crate::win_spawn::Process,
  /// This process's copy of the end, until it lets go (see [`spawn_with`] for why it is kept at first).
  stream: Option<OwnedHandle>,
  /// The end's handle value in the child, as its arguments named it.
  handed: u64,
  /// The kill-on-close job the child is in; held, never used, until this value goes.
  _job: crate::win_spawn::ClosedOnDrop,
}

impl std::fmt::Debug for HandedChild {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("HandedChild")
      .field("id", &self.process.id())
      .field("handed", &self.handed)
      .finish()
  }
}

impl HandedChild {
  /// The child's process id.
  pub fn id(&self) -> u32 {
    self.process.id()
  }

  /// The end's handle value in the child: what replaced the placeholder in its arguments.
  pub fn handed(&self) -> u64 {
    self.handed
  }

  /// Closes this process's own copy of the end, so the child's is the only one and the attachment ends when the
  /// child's closes. Done once the daemon knows the child as a holder (`slates shards` tells it first); a second call
  /// does nothing.
  pub fn let_go_of_end(&mut self) {
    drop(self.stream.take());
  }

  /// Waits for the child to end: its exit code, as Windows reports it (a `u32` read back as the `i32` with the same
  /// bits, which `std::process::exit` hands on unchanged).
  pub fn wait(&mut self) -> Result<i32, IpcError> {
    self.process.wait().map(|code| code.unwrap_or(0))
  }
}

/// Starts `program` with `args`, handing it `stream` (§4.6, A-179): what `slates shards` does on Windows, where no
/// process replaces itself as `crate::stream::exec_with` does on Unix, so a Shards spawner, or a test, gets its stream
/// end without linking slates.
///
/// Exactly one copy of the end reaches the program. An inheritable duplicate of it is made for this spawn alone, the
/// child created with a handle list naming that duplicate and its standard three (`crate::win_spawn`), and each
/// occurrence of `placeholder` in `args` replaced by its value, which inheritance keeps; the duplicate is closed here
/// at once. No other child of this process inherits it: neither the end nor this process's own handles are ever made
/// inheritable. In the child the handle stays inheritable, as a Unix end at its number is open across exec; Shards'
/// spawner clears that before it hands the end on.
///
/// This process keeps its own copy, `stream`, until it calls [`HandedChild::let_go_of_end`] or the [`HandedChild`] goes.
/// The daemon judges a VMM's DAX claim of the end by object identity against a holder it knows (A-180): a name query,
/// the other way to tell a pipe, hung past 3 s on jupiter, 2026-10-26, on a synchronous pipe handle with a read
/// pending, so a VMM could wedge a shard by naming one. The first holder it knows is this process, the attaching
/// client. So the caller tells the daemon the child holds the end ([`HandedChild::id`], [`HandedChild::handed`]) while
/// its own copy still stands for the daemon to judge that against, then lets go, after which the child's end is the
/// only one and the attachment ends when it closes (`slates shards`, A-181). If the child cannot be started, the end
/// is closed here, with nothing started.
///
/// What exec gives on Unix — a supervisor that ends the launched process ends the program — is kept by a job: the child
/// is created inside a kill-on-close job only this process holds, so however this process ends, its program ends with
/// it. And this process outlives console control events from then on, so Ctrl-C is the child's to act on before this
/// process, and with it the job, goes (`crate::win_spawn::outlive_console_events`). The caller waits with
/// [`HandedChild::wait`] and exits with the child's code.
///
/// Refused `Layout` when no argument names the placeholder (the child would hold a handle it is never told of); refused
/// `OsRefused` when the job, the duplicate or the child cannot be made, the end then closed with nothing started.
pub fn spawn_with(
  program: &std::ffi::OsStr,
  args: &[String],
  stream: OwnedHandle,
  placeholder: &str,
) -> Result<HandedChild, IpcError> {
  use crate::win_spawn::{Spawn, StandardOutput, duplicate_inheritable, kill_on_close_job, spawn};
  if !args.iter().any(|argument| argument.contains(placeholder)) {
    return Err(IpcError::Layout {
      reason: "a handed stream end is named in the program's arguments",
    });
  }
  let job = kill_on_close_job()?;
  let inherited = duplicate_inheritable(stream.as_raw_handle())?;
  let handed = u64::try_from(inherited.0.expose_provenance()).map_err(|_| IpcError::Layout {
    reason: "a handle value wider than 64 bits",
  })?;
  let value = handed.to_string();
  let args: Vec<std::ffi::OsString> = args
    .iter()
    .map(|argument| std::ffi::OsString::from(argument.replace(placeholder, &value)))
    .collect();
  let spawned = spawn(&Spawn {
    program,
    args: &args,
    environment: None,
    inherited: &[inherited.0],
    output: StandardOutput::Inherited,
    job: Some(job.0),
  });
  drop(inherited);
  let process = spawned?;
  let child = HandedChild {
    process,
    stream: Some(stream),
    handed,
    _job: job,
  };
  crate::win_spawn::outlive_console_events()?;
  Ok(child)
}

#[cfg(test)]
mod tests {
  use super::*;

  use std::os::windows::io::AsHandle;

  use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_PIPE_BUSY};

  /// Shape: the frame the tests' pairs buffer: larger than any message they send, so no write waits for its reader.
  const TEST_FRAME: usize = 4096;

  /// Opens `name` (with its `\\.\pipe` prefix) as a client would, returning the refusal's code.
  fn open_by_name(name: &str) -> Result<OwnedHandle, u32> {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: a NUL-terminated name; the handle returned is checked and owned.
    let handle = unsafe {
      CreateFileW(
        wide.as_ptr(),
        GENERIC_READ | GENERIC_WRITE,
        0,
        std::ptr::null(),
        OPEN_EXISTING,
        FILE_FLAG_OVERLAPPED,
        std::ptr::null_mut(),
      )
    };
    if handle == INVALID_HANDLE_VALUE {
      return Err(last_error());
    }
    // SAFETY: the handle just opened, owned by the test.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
  }

  /// The full name of the pipe `end` belongs to, as a client opens it.
  fn full_name(end: &OwnedHandle) -> String {
    format!(r"\\.\pipe{}", name_of(end.as_handle()).unwrap())
  }

  /// A-179: do make a pair and write each way with the waiting helpers (the daemon's end is overlapped too, so they
  /// serve it here). Expect the bytes across both ways, and both ends naming the same pipe under the Shards prefix.
  #[test]
  fn a_pair_carries_bytes_both_ways_and_both_ends_name_one_pipe() {
    let (daemon, handed) = pair(TEST_FRAME).unwrap();
    assert_eq!(write_waiting(handed.as_handle(), b"request").unwrap(), 7);
    let mut buf = [0u8; 16];
    let read = read_waiting(daemon.as_handle(), &mut buf).unwrap();
    assert_eq!(buf.get(..read), Some(&b"request"[..]));
    assert_eq!(write_waiting(daemon.as_handle(), b"reply").unwrap(), 5);
    let read = read_waiting(handed.as_handle(), &mut buf).unwrap();
    assert_eq!(buf.get(..read), Some(&b"reply"[..]));
    let name = name_of(daemon.as_handle()).unwrap();
    assert_eq!(name, name_of(handed.as_handle()).unwrap());
    assert!(name.starts_with(r"\slates-shards-"), "{name}");
    assert_eq!(
      name.len(),
      r"\slates-shards-".len() + 2 * NAME_RANDOM_BYTES,
      "{name}"
    );
  }

  /// A-179: do open the name of a connected pair again, as another client would, and create a second server end of
  /// it. Expect the open refused `ERROR_PIPE_BUSY` (one instance, already connected) and the create refused, so only
  /// the handed end reaches the daemon's.
  #[test]
  fn a_connected_pairs_name_admits_no_second_client_and_no_second_server() {
    let (daemon, _handed) = pair(TEST_FRAME).unwrap();
    let name = full_name(&daemon);
    assert_eq!(open_by_name(&name).err(), Some(ERROR_PIPE_BUSY));
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: a NUL-terminated name and no attributes; a handle made is owned and closed below.
    let second = unsafe {
      CreateNamedPipeW(
        wide.as_ptr(),
        PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
        PIPE_TYPE_BYTE,
        ONE_INSTANCE,
        0,
        0,
        NO_WAIT_TIMEOUT,
        std::ptr::null(),
      )
    };
    let code = last_error();
    if second != INVALID_HANDLE_VALUE {
      // SAFETY: a handle the call above made, closed once here.
      drop(unsafe { OwnedHandle::from_raw_handle(second) });
      panic!("a second server end of a connected pair's name was made");
    }
    assert!(
      matches!(code, ERROR_PIPE_BUSY | ERROR_ACCESS_DENIED),
      "refused {code}"
    );
  }

  /// A-179: do drop the handed end, then read the daemon's end. Expect zero (the peer gone), as a Unix socket's read
  /// answers at its peer's close.
  #[test]
  fn a_dropped_handed_end_reads_as_the_end_of_the_stream() {
    let (daemon, handed) = pair(TEST_FRAME).unwrap();
    write_waiting(handed.as_handle(), b"last").unwrap();
    drop(handed);
    let mut buf = [0u8; 8];
    let read = read_waiting(daemon.as_handle(), &mut buf).unwrap();
    assert_eq!(
      buf.get(..read),
      Some(&b"last"[..]),
      "every byte written before the close arrives"
    );
    assert_eq!(read_waiting(daemon.as_handle(), &mut buf).unwrap(), 0);
  }

  /// A-179: do make two pairs. Expect two names (fresh randomness each time), so no pair's name is another's.
  #[test]
  fn every_pair_has_a_fresh_name() {
    let (first, _a) = pair(TEST_FRAME).unwrap();
    let (second, _b) = pair(TEST_FRAME).unwrap();
    assert_ne!(
      name_of(first.as_handle()).unwrap(),
      name_of(second.as_handle()).unwrap()
    );
  }
}
