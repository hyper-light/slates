//! The Windows transport of the anchor's hold (A-198; the counterpart of `crate::held`'s Unix socketpair): a
//! message-mode named-pipe pair (`slates_ipc::stream::message_pair`) over which a daemon asks the anchor to hold its
//! WinFsp handles across its death, and the duplication that moves each handle into the anchor.
//!
//! **Why a round trip, where Unix sends one way.** The Unix channel moves a descriptor with its message, by
//! `SCM_RIGHTS`: once the daemon's `sendmsg` returns, the kernel holds the descriptor for the anchor's socket, so the
//! daemon need not wait. Windows has no such transfer. The anchor must duplicate the handle *out of* the daemon
//! (`DuplicateHandle` with the daemon's process handle, which the anchor holds from the spawn with `PROCESS_DUP_HANDLE`,
//! `slates_ipc::win_spawn::duplicate_from`), so a hold is a request the daemon makes and an answer the anchor gives:
//! the daemon names its handle values, the anchor duplicates each and answers [`HandleMessage::Held`] or
//! [`HandleMessage::Refused`], and the daemon commits the record that names the hold only once the anchor has answered
//! that it holds it. A daemon that dies before the anchor duplicates leaves nothing held; one that dies after leaves a
//! hold whose record never committed, which the next daemon releases.
//!
//! **Handing the held handles on.** The anchor keeps each held handle non-inheritable, for the entry's life. At each
//! spawn it makes an inheritable duplicate of every held handle and of the channel's daemon end, names them in the
//! spawn's handle list (so the child inherits exactly those and nothing else, A-183) and in [`ENV_HANDLES`] at the
//! values the duplicates took, spawns, and closes the duplicates: the child now holds its own inherited copies at the
//! named values (`crate::handles::parse_env`).
//!
//! The message formats are `crate::handles`, pure and tested on every host; this module is the Windows I/O around
//! them, and the real daemon's end rides the runtime's pipe driver (`slates_rt::pipe`) in its own change (Step 9.3).

use std::collections::BTreeMap;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use slates_ipc::win_spawn::{ClosedOnDrop, duplicate_from, duplicate_inheritable};
use windows_sys::Win32::Foundation::{
  ERROR_IO_PENDING, GetLastError, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::handles::{
  HandedEntry, HandleMessage, HandleRefusal, MESSAGE_BYTES, Reason, decode, encode, encode_env,
};

/// This thread's last error code, read before any other call.
fn last_error() -> u32 {
  // SAFETY: reads this thread's last error; touches no memory.
  unsafe { GetLastError() }
}

/// An OS call refused, as a [`HandleRefusal`].
fn os(call: &'static str) -> HandleRefusal {
  HandleRefusal::Os {
    call,
    code: last_error(),
  }
}

/// One end of the hold channel: a message-mode, overlapped named-pipe end. A send writes one whole message; a receive
/// reads one, within a timeout. Owned, closed on drop.
pub struct Channel {
  end: OwnedHandle,
}

impl Channel {
  /// Wraps a pipe end (from [`channel`] or inherited and adopted by a daemon).
  pub fn adopt(end: OwnedHandle) -> Channel {
    Channel { end }
  }

  /// The end's raw handle, for a duplicate of it or to name its value; the channel keeps ownership.
  pub fn raw(&self) -> HANDLE {
    self.end.as_raw_handle()
  }

  /// Sends `message`, one whole write. Refused when the peer is gone or the write is short.
  pub fn send(&self, message: &HandleMessage) -> Result<(), HandleRefusal> {
    let bytes = encode(message)?;
    let moved = self.overlapped(Way::Write, &mut { bytes }, INFINITE)?;
    if moved != MESSAGE_BYTES {
      return Err(HandleRefusal::Malformed {
        reason: "a short write of a hold message",
      });
    }
    Ok(())
  }

  /// Receives one message, waiting at most `timeout_ms`: the message, `None` on a timeout (nothing waited), refused on a
  /// gone peer or a malformed message.
  pub fn recv(&self, timeout_ms: u32) -> Result<Option<HandleMessage>, HandleRefusal> {
    let mut bytes = [0u8; MESSAGE_BYTES];
    let moved = self.overlapped(Way::Read, &mut bytes, timeout_ms)?;
    if moved == 0 {
      return Ok(None);
    }
    if moved != MESSAGE_BYTES {
      return Err(HandleRefusal::Malformed {
        reason: "a short read of a hold message",
      });
    }
    decode(&bytes).map(Some)
  }

  /// One overlapped read or write of `buffer` on the end, waited for on an event of its own for at most `timeout_ms`
  /// (`INFINITE` to block). A read that times out is cancelled and its bytes settled: how many bytes moved (zero when a
  /// read timed out with nothing, or a gone peer read zero).
  fn overlapped(
    &self,
    way: Way,
    buffer: &mut [u8],
    timeout_ms: u32,
  ) -> Result<usize, HandleRefusal> {
    // SAFETY: a nameless manual-reset event, unsignalled, not inheritable; owned and closed below.
    let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if event.is_null() {
      return Err(os("CreateEventW"));
    }
    // SAFETY: the event just made, owned here and closed on drop after the operation it waits for has settled.
    let event = unsafe { OwnedHandle::from_raw_handle(event) };
    let mut overlapped = OVERLAPPED {
      hEvent: event.as_raw_handle(),
      ..OVERLAPPED::default()
    };
    let length = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
    let handle = self.end.as_raw_handle();
    // SAFETY: `buffer` holds `length` bytes and outlives this call (the operation is settled before return), and the
    // OVERLAPPED is on this frame.
    let started = unsafe {
      match way {
        Way::Read => ReadFile(
          handle,
          buffer.as_mut_ptr(),
          length,
          std::ptr::null_mut(),
          &mut overlapped,
        ),
        Way::Write => WriteFile(
          handle,
          buffer.as_ptr(),
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
    // SAFETY: waits on the OVERLAPPED's event for the operation on this live handle, at most `timeout_ms`.
    let waited = unsafe { WaitForSingleObject(event.as_raw_handle(), timeout_ms) };
    if waited == WAIT_TIMEOUT {
      // SAFETY: cancels the one operation this frame's OVERLAPPED started on this handle, then settles it; a message
      // the kernel delivered just before the cancel is returned by GetOverlappedResult and taken, never dropped.
      unsafe { CancelIoEx(handle, &overlapped) };
    } else if waited != WAIT_OBJECT_0 {
      return Err(os("WaitForSingleObject"));
    }
    let mut moved = 0u32;
    // SAFETY: settles the operation on this live handle, waiting for the cancel or completion to finish.
    let finished = unsafe { GetOverlappedResult(handle, &overlapped, &mut moved, 1) };
    if finished == 0 {
      let code = last_error();
      if waited == WAIT_TIMEOUT && is_cancelled(code) {
        return Ok(0);
      }
      return gone_or_refused(code, way);
    }
    Ok(usize::try_from(moved).unwrap_or(0))
  }
}

/// Format: block on an overlapped operation (no timeout).
const INFINITE: u32 = u32::MAX;

/// A read or a write.
#[derive(Clone, Copy)]
enum Way {
  Read,
  Write,
}

/// Whether `code` is the cancellation of an overlapped operation (`ERROR_OPERATION_ABORTED`).
fn is_cancelled(code: u32) -> bool {
  code == windows_sys::Win32::Foundation::ERROR_OPERATION_ABORTED
}

/// A refused or failed operation with `code`: a read of a gone peer is zero bytes (its end closed), as on Unix;
/// anything else is refused.
fn gone_or_refused(code: u32, way: Way) -> Result<usize, HandleRefusal> {
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
  Err(HandleRefusal::Os {
    call: match way {
      Way::Read => "ReadFile of a hold message",
      Way::Write => "WriteFile of a hold message",
    },
    code,
  })
}

/// A connected message-mode pipe pair for the hold channel: the anchor's end (kept) and the daemon's end (handed to
/// each spawned daemon by an inheritable duplicate). Each message is [`MESSAGE_BYTES`], so the pipe buffers one each
/// way. Both ends are overlapped and non-inheritable until a spawn makes a duplicate inheritable.
pub fn channel() -> Result<(Channel, Channel), HandleRefusal> {
  let (anchor, daemon) =
    slates_ipc::stream::message_pair(MESSAGE_BYTES).map_err(|e| HandleRefusal::Os {
      call: "message_pair",
      code: match e {
        slates_ipc::IpcError::OsRefused {
          code: Some(code), ..
        } => u32::try_from(code).unwrap_or(0),
        _ => 0,
      },
    })?;
  Ok((Channel::adopt(anchor), Channel::adopt(daemon)))
}

/// One entry the anchor holds for a daemon: what it is, the handles (duplicated out of the daemon, kept
/// non-inheritable for the entry's life), and the record the next daemon adopts it by.
struct Entry {
  what: u8,
  handles: Vec<OwnedHandle>,
  record: Vec<u8>,
}

/// The entries the anchor holds across daemon restarts on Windows (A-198), by id, at most `bound` at once. The channel
/// lives beside it in the supervisor; `apply` takes a message read from it and answers.
pub struct Held {
  entries: BTreeMap<u64, Entry>,
  bound: usize,
  refused: u64,
}

impl Held {
  /// An empty hold of at most `bound` entries.
  pub fn new(bound: usize) -> Held {
    Held {
      entries: BTreeMap::new(),
      bound,
      refused: 0,
    }
  }

  /// How many entries are held.
  pub fn len(&self) -> usize {
    self.entries.len()
  }

  /// Whether nothing is held.
  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }

  /// Holds against the bound.
  pub fn bound(&self) -> usize {
    self.bound
  }

  /// Entries the anchor refused (the bound reached, or a handle it could not duplicate).
  pub fn refusals(&self) -> u64 {
    self.refused
  }

  /// Applies one message from `daemon` (its process handle, which the anchor duplicates handles out of), and returns
  /// the answer to send back, if any: a hold is answered [`HandleMessage::Held`] or [`HandleMessage::Refused`]; a
  /// release drops the entry and is not answered.
  pub fn apply(&mut self, daemon: HANDLE, message: HandleMessage) -> Option<HandleMessage> {
    match message {
      HandleMessage::Hold {
        id,
        what,
        values,
        record,
      } => Some(self.hold(daemon, id, what, &values, record)),
      HandleMessage::Release { id } => {
        self.entries.remove(&id);
        None
      }
      // The anchor never receives an answer; a daemon that sends one is ignored (it is not the protocol's direction).
      HandleMessage::Held { .. } | HandleMessage::Refused { .. } => None,
    }
  }

  /// Duplicates each of `values` out of `daemon` and holds them as entry `id`, or refuses: the bound reached, the id
  /// already held, or a handle that would not duplicate (a value that is not a handle of the daemon, or the daemon
  /// gone). On refusal nothing of the entry is kept.
  fn hold(
    &mut self,
    daemon: HANDLE,
    id: u64,
    what: u8,
    values: &[u64],
    record: Vec<u8>,
  ) -> HandleMessage {
    if self.entries.contains_key(&id) {
      self.refused = self.refused.saturating_add(1);
      return HandleMessage::Refused {
        id,
        reason: Reason::Malformed,
      };
    }
    if self.entries.len() >= self.bound {
      self.refused = self.refused.saturating_add(1);
      return HandleMessage::Refused {
        id,
        reason: Reason::Bound,
      };
    }
    let mut handles = Vec::with_capacity(values.len());
    for value in values {
      let Some(raw) = handle_from_value(*value) else {
        self.refused = self.refused.saturating_add(1);
        return HandleMessage::Refused {
          id,
          reason: Reason::Duplicate,
        };
      };
      match duplicate_from(daemon, raw) {
        // The duplicate is this process's own handle; move it into an `OwnedHandle` so it closes with the entry rather
        // than at the end of this call (`ClosedOnDrop` would close it now).
        Ok(duplicate) => handles.push(owned(duplicate)),
        Err(_) => {
          self.refused = self.refused.saturating_add(1);
          return HandleMessage::Refused {
            id,
            reason: Reason::Duplicate,
          };
        }
      }
    }
    self.entries.insert(
      id,
      Entry {
        what,
        handles,
        record,
      },
    );
    HandleMessage::Held { id }
  }

  /// The handoff to a spawn: an inheritable duplicate of `daemon_end` (the daemon's end of the anchor's one channel,
  /// which the anchor keeps for its life and hands each daemon a duplicate of, as the Unix anchor hands its socketpair's
  /// daemon end) and of every held handle (kept alive in the returned guard until after the spawn), and the
  /// [`ENV_HANDLES`] value naming the channel's and each entry's values. The caller names the guard's handles in the
  /// spawn's handle list, passes the env, spawns, then drops the guard.
  ///
  /// [`ENV_HANDLES`]: crate::handles::ENV_HANDLES
  pub fn handoff(&self, daemon_end: &Channel) -> Result<Handoff, HandleRefusal> {
    let mut guard = Vec::new();
    let channel_dup =
      duplicate_inheritable(daemon_end.raw()).map_err(|_| os("DuplicateHandle of the channel"))?;
    let channel_value = handle_value(channel_dup.raw());
    guard.push(channel_dup);
    let mut entries = Vec::with_capacity(self.entries.len());
    for (id, entry) in &self.entries {
      let mut values = Vec::with_capacity(entry.handles.len());
      for handle in &entry.handles {
        let dup = duplicate_inheritable(handle.as_raw_handle())
          .map_err(|_| os("DuplicateHandle of a held handle"))?;
        values.push(handle_value(dup.raw()));
        guard.push(dup);
      }
      entries.push(HandedEntry {
        id: *id,
        what: entry.what,
        values,
        record: entry.record.clone(),
      });
    }
    let env = encode_env(channel_value, &entries);
    Ok(Handoff {
      handles: guard,
      env,
    })
  }
}

/// A handoff of the held handles to one spawn: the inheritable duplicates (kept alive until after the spawn) and the
/// [`ENV_HANDLES`] value naming them. Dropped after the spawn, which closes the duplicates; the child keeps its own
/// inherited copies.
///
/// [`ENV_HANDLES`]: crate::handles::ENV_HANDLES
pub struct Handoff {
  handles: Vec<ClosedOnDrop>,
  env: String,
}

impl Handoff {
  /// The handles the spawn names in its inherit list.
  pub fn handles(&self) -> Vec<HANDLE> {
    self.handles.iter().map(ClosedOnDrop::raw).collect()
  }

  /// The [`ENV_HANDLES`] value for the spawn's environment.
  ///
  /// [`ENV_HANDLES`]: crate::handles::ENV_HANDLES
  pub fn env(&self) -> &str {
    &self.env
  }
}

/// A handle value as a `u64`, for naming it across the channel or in the environment.
pub fn handle_value(handle: HANDLE) -> u64 {
  // A HANDLE is an opaque table index; its address bits are the value, widened to the wire's u64.
  handle as usize as u64
}

/// The `HANDLE` a `u64` from the wire names, or `None` when it does not fit this machine's pointer width (a corrupt
/// message; never on the 64-bit targets slates ships).
pub fn handle_from_value(value: u64) -> Option<HANDLE> {
  usize::try_from(value).ok().map(|bits| bits as HANDLE)
}

/// Moves a `ClosedOnDrop`'s handle into an `OwnedHandle`: the `ClosedOnDrop` gives up the handle without closing it
/// (`into_raw`), and the `OwnedHandle` closes it instead, at the end of the entry's life rather than this call's. Both
/// close through the OS's `CloseHandle`, so the transfer changes only when, not how.
fn owned(dup: ClosedOnDrop) -> OwnedHandle {
  // SAFETY: a handle this process owns (from `DuplicateHandle` into it), no longer closed by `dup`; the `OwnedHandle`
  // is its sole owner now.
  unsafe { OwnedHandle::from_raw_handle(dup.into_raw()) }
}

#[cfg(test)]
mod tests {
  use windows_sys::Win32::System::Threading::{CreateEventW, GetCurrentProcess};

  use super::*;

  /// This process's pseudo-handle, used as the "daemon" to duplicate handles out of (a process may duplicate out of
  /// itself), so a unit test needs no second process.
  fn myself() -> HANDLE {
    // SAFETY: the current-process pseudo-handle needs no closing.
    unsafe { GetCurrentProcess() }
  }

  /// `n` nameless manual-reset events, owned here and closed on drop, standing in for a daemon's handles.
  fn events(n: usize) -> Vec<OwnedHandle> {
    (0..n).map(|_| one_event()).collect()
  }

  /// One nameless manual-reset event, owned.
  fn one_event() -> OwnedHandle {
    // SAFETY: a nameless manual-reset (TRUE), unsignalled (FALSE) event; its handle is owned below.
    let raw = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    // SAFETY: a handle this process owns from `CreateEventW`, closed on drop.
    unsafe { OwnedHandle::from_raw_handle(raw) }
  }

  /// A hold of `values` as entry `id` with `record`.
  fn hold(id: u64, values: Vec<u64>, record: &[u8]) -> HandleMessage {
    HandleMessage::Hold {
      id,
      what: crate::handles::WHAT_WINFSP_MOUNT,
      values,
      record: record.to_vec(),
    }
  }

  /// A-198. Do: create the channel, send a hold one way and an answer back the other. Expect: each arrives whole, and a
  /// receive with nothing waiting times out as `None`.
  #[test]
  fn the_channel_carries_a_hold_and_its_answer() {
    let (anchor, daemon) = channel().unwrap();
    assert!(anchor.recv(0).unwrap().is_none(), "nothing waiting");
    let hold = HandleMessage::Hold {
      id: 42,
      what: crate::handles::WHAT_WINFSP_MOUNT,
      values: vec![0x10, 0x20, 0x30],
      record: b"S:".to_vec(),
    };
    daemon.send(&hold).unwrap();
    assert_eq!(anchor.recv(1000).unwrap(), Some(hold));
    anchor.send(&HandleMessage::Held { id: 42 }).unwrap();
    assert_eq!(
      daemon.recv(1000).unwrap(),
      Some(HandleMessage::Held { id: 42 })
    );
  }

  /// A-198. Do: hold three real handles (events) duplicated out of this process (standing in for the daemon), release
  /// one, and build the handoff. Expect: the held count tracks, the handoff names the surviving entry and the channel,
  /// and the entry's record round-trips.
  #[test]
  fn a_hold_duplicates_handles_and_the_handoff_names_them() {
    let (_anchor, daemon) = channel().unwrap();
    let mut held = Held::new(8);
    let evs = events(3);
    let values: Vec<u64> = evs
      .iter()
      .map(|e| handle_value(e.as_raw_handle()))
      .collect();
    let held_1 = held.apply(myself(), hold(1, values.clone(), b"record"));
    assert_eq!(held_1, Some(HandleMessage::Held { id: 1 }));
    let held_2 = held.apply(myself(), hold(2, values, b""));
    assert_eq!(held_2, Some(HandleMessage::Held { id: 2 }));
    assert_eq!(held.len(), 2);
    let released = held.apply(myself(), HandleMessage::Release { id: 2 });
    assert_eq!(released, None);
    assert_eq!(held.len(), 1);
    let handoff = held.handoff(&daemon).unwrap();
    // The channel plus the one entry's three handles.
    assert_eq!(handoff.handles().len(), 1 + 3);
    let (_channel, entries) = crate::handles::parse_env(handoff.env(), 8).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, 1);
    assert_eq!(entries[0].record, b"record");
    assert_eq!(entries[0].values.len(), 3);
  }

  /// A-198. Do: hold up to the bound, then one more; and an id already held. Expect: the bound and the duplicate id are
  /// refused, counted, and nothing of the refused entry is kept.
  #[test]
  fn the_bound_and_a_duplicate_id_are_refused() {
    let event = one_event();
    let value = handle_value(event.as_raw_handle());
    let mut held = Held::new(1);
    let first = held.apply(myself(), hold(1, vec![value], b""));
    assert_eq!(first, Some(HandleMessage::Held { id: 1 }));
    let again = held.apply(myself(), hold(1, vec![value], b""));
    let bound = held.apply(myself(), hold(2, vec![value], b""));
    assert_eq!(
      again,
      Some(HandleMessage::Refused {
        id: 1,
        reason: Reason::Malformed
      }),
      "an id already held"
    );
    assert_eq!(
      bound,
      Some(HandleMessage::Refused {
        id: 2,
        reason: Reason::Bound
      }),
      "past the bound"
    );
    assert_eq!(held.len(), 1);
    assert_eq!(held.refusals(), 2);
  }

  /// A-198. Do: ask to hold a value that is not a handle of the daemon. Expect: refused `Duplicate`, counted, nothing
  /// kept.
  #[test]
  fn a_value_that_will_not_duplicate_is_refused() {
    let mut held = Held::new(8);
    // An odd, tiny value that is not a live handle of this process.
    let answer = held.apply(myself(), hold(1, vec![0x3], b""));
    assert_eq!(
      answer,
      Some(HandleMessage::Refused {
        id: 1,
        reason: Reason::Duplicate
      })
    );
    assert!(held.is_empty());
    assert_eq!(held.refusals(), 1);
  }
}
