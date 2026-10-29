//! The daemon's handle route on Windows (§4.6, A-175): how a same-kernel client receives the read-only handle of a
//! direct map's copy, and the client end of a Shards stream's pipe (A-179, [`hand_stream`]). Windows carries no descriptor over a socket. A handle reaches another process when its holder
//! duplicates it into that process's handle table (`DuplicateHandle`, with the target's process handle opened for
//! `PROCESS_DUP_HANDLE`), and only its value travels, in the reply on the client's ring (`DuplicateHandle`: "the source
//! process must use interprocess communication to pass the value of the duplicate handle to the target process").
//!
//! **What is handed.** A handle carries its object to whoever holds it, and a same-user holder of a read-only section
//! handle can duplicate it into one that writes unless the section's security descriptor refuses: measured on Windows
//! 10.0.26200 (`slates-mem`'s `tests/peer_section.rs`), the default descriptor grants a duplicate that writes and one
//! that may rewrite the descriptor, and a peer object's confining descriptor refuses both. So the hand-off first judges
//! the handle as a hostile holder would use it ([`confined_section`]): the section must be exactly the authorized
//! bytes, and a duplicate that writes, one that may rewrite the descriptor and a writable view must each be refused
//! `ERROR_ACCESS_DENIED`, the descriptor's own refusal, so a refusal for some other reason cannot pass for it. Only
//! then is the handle duplicated into the client.
//!
//! **To whom.** The client's process, by the id its claim slot carries, opened to duplicate into and to read its
//! token. The id is the client's own word, so the daemon checks that the process runs as the daemon's own user before
//! it hands anything ([`same_user`]), as Linux checks a peer's uid (`SO_PEERCRED`): a process of another user is
//! refused `PeerRefused`. A same-user process named falsely gains nothing it could not get itself, since it can read
//! the file through the client that named it.
//!
//! **What the client does.** It adopts the value as its own handle ([`adopt_section`]), once, after checking that the
//! handle is live in its process, and maps the section read alone. A value of zero names no handle: the daemon records
//! a hand-off's reply without its value (a handle the client's process holds outlives the daemon that duplicated it, so
//! a reply replayed from the record after a restart must not name one), and a client handed zero fails its map as a
//! client on the other platforms does when its descriptor never comes.

use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};

use windows_sys::Win32::Foundation::{
  CompareObjectHandles, DUPLICATE_CLOSE_SOURCE, DUPLICATE_SAME_ACCESS, DuplicateHandle,
  ERROR_ACCESS_DENIED, GetHandleInformation, GetLastError, HANDLE,
};
use windows_sys::Win32::Security::{
  EqualSid, GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_QUERY,
  TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::WRITE_DAC;
use windows_sys::Win32::System::Memory::{
  FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_BASIC_INFORMATION, MapViewOfFile, SECTION_EXTEND_SIZE,
  UnmapViewOfFile, VirtualQuery,
};
use windows_sys::Win32::System::Threading::{
  GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_DUP_HANDLE,
  PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::error::IpcError;

/// This thread's last error code, which the call that just failed set; read before any other call.
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

/// Duplicates `section`, a handle of a direct map's copy, into the process `pid` and returns the duplicate's value
/// there, once [`confined_section`] has judged it to authorize `authorized` bytes in `access` and nothing more, and
/// [`same_user`] the process to run as this one's user. Refused `UnconfinedDescriptor`, `PeerRefused`, or `OsRefused`
/// naming the step the OS refused.
pub fn hand_section(
  section: BorrowedHandle<'_>,
  authorized: u64,
  pid: u32,
  access: slates_mem::PeerAccess,
) -> Result<u64, IpcError> {
  confined_section(section, authorized, access)?;
  duplicate_into(section, pid)
}

/// Duplicates `stream`, the client end of a Shards stream's pipe (`crate::stream::pair`, A-179), into the process `pid`
/// and returns the duplicate's value there, once [`same_user`] has judged the process to run as this one's user.
/// Refused `PeerRefused`, or `OsRefused` naming the step the OS refused.
///
/// The end needs none of a section's judging: a holder of it reaches the one conversation the daemon serves on its
/// pipe and nothing else, since the pipe allows one instance and it is connected, so no duplicate of it, whatever its
/// access, opens anything more (the Unix arm hands its socket end unjudged for the same reason, `crate::stream`).
pub fn hand_stream(stream: BorrowedHandle<'_>, pid: u32) -> Result<u64, IpcError> {
  duplicate_into(stream, pid)
}

/// Duplicates `handle` into the process `pid`, with the access it has, once [`same_user`] has judged the process: the
/// duplicate's value there, which only travels in a reply.
fn duplicate_into(handle: BorrowedHandle<'_>, pid: u32) -> Result<u64, IpcError> {
  let process = open_process(pid, PROCESS_DUP_HANDLE | PROCESS_QUERY_LIMITED_INFORMATION)?;
  same_user(process.as_handle())?;
  let mut value: HANDLE = std::ptr::null_mut();
  // SAFETY: this process's pseudo-handle as the source process, a live handle of it, the client's process handle just
  // opened for duplication, and an out-pointer to a local; the duplicate lives in the client's table, not this one's.
  let done = unsafe {
    DuplicateHandle(
      GetCurrentProcess(),
      handle.as_raw_handle(),
      process.as_raw_handle(),
      &mut value,
      0,
      0,
      DUPLICATE_SAME_ACCESS,
    )
  };
  if done == 0 {
    return Err(os("DuplicateHandle into the client"));
  }
  Ok(u64::try_from(value.expose_provenance()).unwrap_or(0))
}

/// A process judged to hold a Shards stream, opened to duplicate into (plan G-shards-windows step 5): what a VMM's
/// descriptor ask is answered into, once [`claim`] has judged it.
#[derive(Debug)]
pub struct Claimant {
  process: OwnedHandle,
  pid: u32,
  stream: u64,
}

impl Claimant {
  /// The claimant's process id.
  #[must_use]
  pub fn pid(&self) -> u32 {
    self.pid
  }

  /// The value of its stream handle, in its process: a known holder from now on.
  #[must_use]
  pub fn stream(&self) -> u64 {
    self.stream
  }
}

/// Judges whether the process `pid` holds the stream whose known holders are `holders` (each a process id and the
/// value of a handle of the stream in it): the handle `stream` it names there must be the very object one of them
/// holds. The claimant's handle and each holder's are duplicated into this process, compared by
/// `CompareObjectHandles`, and closed; nothing is put into the claimant's process. Refused `PeerRefused` for a process
/// of another user, `NotAHolder` where no holder's handle is the same object (a handle of another pipe, of no pipe, or
/// a value that names nothing), and `OsRefused` where the process cannot be opened (gone, or not this user's).
///
/// Why identity and not the pipe's name. A name query waits on the file object's lock when the handle is synchronous,
/// so a claimant naming any synchronous handle with a read pending would hold the asking shard: measured on jupiter
/// (Windows 10.0.26200, 2026-10-26), `GetFileInformationByHandleEx(FileNameInfo)`, `NtQueryObject` with
/// `ObjectNameInformation` and `NtQueryInformationFile` each hung past 3 s on such a pipe's duplicate, while
/// `CompareObjectHandles` and closing the duplicate answered at once. The stream's pipe allows one instance, already
/// connected, so a handle bearing its name is a handle of this one object: the outcome is the name check's.
pub fn claim(pid: u32, stream: u64, holders: &[(u32, u64)]) -> Result<Claimant, IpcError> {
  let process = open_process(pid, PROCESS_DUP_HANDLE | PROCESS_QUERY_LIMITED_INFORMATION)?;
  same_user(process.as_handle())?;
  let Ok(claimed) = duplicate_out(process.as_handle(), stream) else {
    return Err(IpcError::NotAHolder { pid });
  };
  let held = holders.iter().any(|&(holder_pid, holder_value)| {
    open_process(holder_pid, PROCESS_DUP_HANDLE)
      .and_then(|holder| duplicate_out(holder.as_handle(), holder_value))
      .is_ok_and(|known| {
        // SAFETY: two live handles this process owns; the call compares the objects they name and touches neither.
        unsafe { CompareObjectHandles(claimed.as_raw_handle(), known.as_raw_handle()) != 0 }
      })
  });
  if !held {
    return Err(IpcError::NotAHolder { pid });
  }
  Ok(Claimant {
    process,
    pid,
    stream,
  })
}

/// Duplicates `section`, with the access it has, into `claimant`'s process: the duplicate's value there, which only
/// travels in the answer. Refused `OsRefused` where the process is gone or refuses.
pub fn hand_to_claimant(section: BorrowedHandle<'_>, claimant: &Claimant) -> Result<u64, IpcError> {
  let mut value: HANDLE = std::ptr::null_mut();
  // SAFETY: this process's pseudo-handle as the source, a live handle of it, the claimant's process opened for
  // duplication, and an out-pointer to a local; the duplicate lives in the claimant's table.
  let done = unsafe {
    DuplicateHandle(
      GetCurrentProcess(),
      section.as_raw_handle(),
      claimant.process.as_raw_handle(),
      &mut value,
      0,
      0,
      DUPLICATE_SAME_ACCESS,
    )
  };
  if done == 0 {
    return Err(os("DuplicateHandle into the claimant"));
  }
  Ok(u64::try_from(value.expose_provenance()).unwrap_or(0))
}

/// A duplicate in this process of the handle `value` of `process`, with the access it has. Refused `OsRefused` for a
/// value that names no handle there.
fn duplicate_out(process: BorrowedHandle<'_>, value: u64) -> Result<OwnedHandle, IpcError> {
  let source: HANDLE = std::ptr::with_exposed_provenance_mut(usize::try_from(value).unwrap_or(0));
  let mut duplicate: HANDLE = std::ptr::null_mut();
  // SAFETY: a process opened for duplication, a value in its table (checked by the call, refused if it names nothing),
  // this process as the target, and an out-pointer to a local; the duplicate is owned below.
  let done = unsafe {
    DuplicateHandle(
      process.as_raw_handle(),
      source,
      GetCurrentProcess(),
      &mut duplicate,
      0,
      0,
      DUPLICATE_SAME_ACCESS,
    )
  };
  if done == 0 {
    return Err(os("DuplicateHandle out of the claimant"));
  }
  // SAFETY: the duplicate just made, owned here alone and closed on drop.
  Ok(unsafe { OwnedHandle::from_raw_handle(duplicate) })
}

/// Closes the handle `value` that [`hand_section`] duplicated into the process `pid`, in that process: what undoes a
/// hand-off whose reply cannot be given. Refused `OsRefused` where the process or the handle is gone.
pub fn close_handed(pid: u32, value: u64) -> Result<(), IpcError> {
  let process = open_process(pid, PROCESS_DUP_HANDLE)?;
  let handle: HANDLE = std::ptr::with_exposed_provenance_mut(usize::try_from(value).unwrap_or(0));
  // SAFETY: the client's process handle just opened for duplication, the value this daemon duplicated into it, and no
  // target: `DUPLICATE_CLOSE_SOURCE` with no target process closes the handle in its own process.
  let done = unsafe {
    DuplicateHandle(
      process.as_raw_handle(),
      handle,
      std::ptr::null_mut(),
      std::ptr::null_mut(),
      0,
      0,
      DUPLICATE_CLOSE_SOURCE,
    )
  };
  if done == 0 {
    return Err(os("DuplicateHandle to close in the client"));
  }
  Ok(())
}

/// The handle `value` a daemon duplicated into this process for a direct map's reply, taken as this process's own.
/// Refused `Layout` for zero (a reply recorded without its value, replayed after a restart) and `OsRefused` for a value
/// that names no live handle here.
pub fn adopt_section(value: u64) -> Result<OwnedHandle, IpcError> {
  adopt(
    value,
    "a map reply that names no handle (recorded before a restart)",
  )
}

/// The handle `value` a daemon duplicated into this process for a Shards attach's reply (A-179): the client end of
/// the attachment's stream, taken as this process's own. Refused as [`adopt_section`] refuses.
pub fn adopt_stream(value: u64) -> Result<OwnedHandle, IpcError> {
  adopt(
    value,
    "a Shards attach reply that names no handle (its hand-off refused, or recorded before a restart)",
  )
}

/// The handle `value` a daemon duplicated into this process, taken as this process's own once it is checked live
/// here; zero is refused `Layout` with `none`, the reason the reply named no handle.
fn adopt(value: u64, none: &'static str) -> Result<OwnedHandle, IpcError> {
  if value == 0 {
    return Err(IpcError::Layout { reason: none });
  }
  let raw: HANDLE =
    std::ptr::with_exposed_provenance_mut(usize::try_from(value).map_err(|_| {
      IpcError::Layout {
        reason: "a reply's handle wider than an address",
      }
    })?);
  let mut flags = 0u32;
  // SAFETY: asks about a handle value; the answer is written to a local, and a value that names no live handle is
  // refused rather than touched.
  if unsafe { GetHandleInformation(raw, &mut flags) } == 0 {
    return Err(os("GetHandleInformation"));
  }
  // SAFETY: the daemon duplicated this handle into this process for the reply that names it, and nothing else in this
  // process holds it: a reply's value is adopted once, here, and it is live (checked above).
  Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

/// Refuses a section handle that would authorize a peer more than the `authorized` bytes in `access` it is handed for:
/// one whose section is any other size, or that a holder can duplicate into a handle that may rewrite the section's
/// descriptor or extend the section. A read-only handle (A-175) must also refuse a duplicate that writes and a writable
/// view; a writable one (A-186) writes by design, and those two are its grant. Each refusal must be
/// `ERROR_ACCESS_DENIED`, the descriptor's own; any other answer refuses the hand-off too, since then nothing was
/// judged. Refused `UnconfinedDescriptor` naming what the handle would have authorized, or `OsRefused` where the
/// judging could not be done.
pub fn confined_section(
  section: BorrowedHandle<'_>,
  authorized: u64,
  access: slates_mem::PeerAccess,
) -> Result<(), IpcError> {
  let object_bytes = section_bytes(section)?;
  let reaches_further = escalates(section, WRITE_DAC)? || escalates(section, SECTION_EXTEND_SIZE)?;
  let writes_unasked = match access {
    slates_mem::PeerAccess::Read => {
      escalates(section, FILE_MAP_WRITE | FILE_MAP_READ)? || maps_writable(section)?
    }
    slates_mem::PeerAccess::ReadWrite => false,
  };
  let writable = reaches_further || writes_unasked;
  if writable || object_bytes != authorized {
    return Err(IpcError::UnconfinedDescriptor {
      object_bytes,
      authorized_bytes: authorized,
      writable,
    });
  }
  Ok(())
}

/// The size of `handle`'s section: a read-only view of all of it, asked its extent (`VirtualQuery`: a whole number of
/// pages, which a peer object's size is).
fn section_bytes(handle: BorrowedHandle<'_>) -> Result<u64, IpcError> {
  // SAFETY: a read-only view of the whole section at an address the system chooses; unmapped below, never touched.
  let view = unsafe { MapViewOfFile(handle.as_raw_handle(), FILE_MAP_READ, 0, 0, 0) };
  if view.Value.is_null() {
    return Err(os("MapViewOfFile to judge a section"));
  }
  let mut region = MEMORY_BASIC_INFORMATION::default();
  // SAFETY: asks about the view just made; the answer is written to a local of the size passed.
  let answered = unsafe {
    VirtualQuery(
      view.Value,
      &mut region,
      std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
    )
  };
  let refused = os("VirtualQuery");
  // SAFETY: the view made above, unmapped once.
  unsafe { UnmapViewOfFile(view) };
  if answered == 0 {
    return Err(refused);
  }
  Ok(u64::try_from(region.RegionSize).unwrap_or(u64::MAX))
}

/// Whether a duplicate of `handle` in this process with `access` is granted: `false` only for the descriptor's own
/// refusal (`ERROR_ACCESS_DENIED`); a granted duplicate is closed at once.
fn escalates(handle: BorrowedHandle<'_>, access: u32) -> Result<bool, IpcError> {
  let mut duplicate: HANDLE = std::ptr::null_mut();
  // SAFETY: this process's pseudo-handle on both sides, a live handle, and an out-pointer to a local; a granted
  // duplicate is owned and closed below.
  let done = unsafe {
    DuplicateHandle(
      GetCurrentProcess(),
      handle.as_raw_handle(),
      GetCurrentProcess(),
      &mut duplicate,
      access,
      0,
      0,
    )
  };
  if done != 0 {
    // SAFETY: the duplicate just granted, owned here alone and closed on drop.
    drop(unsafe { OwnedHandle::from_raw_handle(duplicate) });
    return Ok(true);
  }
  denied_or_refused("DuplicateHandle to judge a section")
}

/// Whether a writable view of `handle`'s section is granted: `false` only for `ERROR_ACCESS_DENIED`; a granted view is
/// unmapped at once.
fn maps_writable(handle: BorrowedHandle<'_>) -> Result<bool, IpcError> {
  // SAFETY: a writable view at an address the system chooses, never touched; a granted one is unmapped below.
  let view = unsafe { MapViewOfFile(handle.as_raw_handle(), FILE_MAP_WRITE, 0, 0, 0) };
  if !view.Value.is_null() {
    // SAFETY: the view just granted, unmapped once.
    unsafe { UnmapViewOfFile(view) };
    return Ok(true);
  }
  denied_or_refused("MapViewOfFile writable to judge a section")
}

/// After a refused attempt: `false` when the refusal was the descriptor's (`ERROR_ACCESS_DENIED`), else the refusal of
/// the judging itself, since nothing was judged.
fn denied_or_refused(call: &'static str) -> Result<bool, IpcError> {
  let code = last_error();
  if code == ERROR_ACCESS_DENIED {
    return Ok(false);
  }
  Err(IpcError::OsRefused {
    call,
    code: i32::try_from(code).ok(),
  })
}

/// The process `pid`, opened with `access`. Refused `OsRefused` where it is gone or not this user's to open.
fn open_process(pid: u32, access: u32) -> Result<OwnedHandle, IpcError> {
  // SAFETY: opens a process by id; a null handle is refused, and a granted one is owned below.
  let process = unsafe { OpenProcess(access, 0, pid) };
  if process.is_null() {
    return Err(os("OpenProcess"));
  }
  // SAFETY: the process handle just opened, owned here alone and closed on drop.
  Ok(unsafe { OwnedHandle::from_raw_handle(process) })
}

/// Refuses a process that does not run as this process's user: `PeerRefused` naming the last part of its user's SID
/// (its relative id), or `OsRefused` where a token cannot be read.
pub fn same_user(process: BorrowedHandle<'_>) -> Result<(), IpcError> {
  // SAFETY: this process's pseudo-handle, valid for this process's life.
  let this = unsafe { BorrowedHandle::borrow_raw(GetCurrentProcess()) };
  let ours = token_user(this)?;
  let theirs = token_user(process)?;
  let ours_sid = ours.as_ptr().cast::<TOKEN_USER>();
  let theirs_sid = theirs.as_ptr().cast::<TOKEN_USER>();
  // SAFETY: both buffers hold a `TOKEN_USER` that `GetTokenInformation` wrote, whose SIDs point into the same
  // buffers, which outlive the call.
  let equal = unsafe { EqualSid((*ours_sid).User.Sid, (*theirs_sid).User.Sid) };
  if equal != 0 {
    return Ok(());
  }
  // SAFETY: the client's SID, valid as above; the count is read before the sub-authority it bounds.
  let uid = unsafe {
    let sid = (*theirs_sid).User.Sid;
    let count = *GetSidSubAuthorityCount(sid);
    *GetSidSubAuthority(sid, u32::from(count.saturating_sub(1)))
  };
  Err(IpcError::PeerRefused { uid })
}

/// The `TOKEN_USER` of `process`'s token, in a buffer aligned for it (the SID it points to lies inside the buffer).
fn token_user(process: BorrowedHandle<'_>) -> Result<Vec<u64>, IpcError> {
  let mut token: HANDLE = std::ptr::null_mut();
  // SAFETY: a process handle opened for querying and an out-pointer to a local; the token is owned below.
  if unsafe { OpenProcessToken(process.as_raw_handle(), TOKEN_QUERY, &mut token) } == 0 {
    return Err(os("OpenProcessToken"));
  }
  // SAFETY: the token handle just opened, owned here alone and closed on drop.
  let token = unsafe { OwnedHandle::from_raw_handle(token) };
  let mut needed = 0u32;
  // SAFETY: a size query with no buffer; the size is written to a local. It fails by design, with the size set.
  unsafe {
    GetTokenInformation(
      token.as_raw_handle(),
      TokenUser,
      std::ptr::null_mut(),
      0,
      &mut needed,
    )
  };
  let words = usize::try_from(needed)
    .ok()
    .map(|bytes| bytes.div_ceil(size_of::<u64>()))
    .filter(|words| *words > 0)
    .ok_or(IpcError::OsRefused {
      call: "GetTokenInformation",
      code: None,
    })?;
  let mut buffer = vec![0u64; words];
  // SAFETY: `buffer` holds at least `needed` bytes, aligned for `TOKEN_USER`.
  let read = unsafe {
    GetTokenInformation(
      token.as_raw_handle(),
      TokenUser,
      buffer.as_mut_ptr().cast(),
      needed,
      &mut needed,
    )
  };
  if read == 0 {
    return Err(os("GetTokenInformation"));
  }
  Ok(buffer)
}
