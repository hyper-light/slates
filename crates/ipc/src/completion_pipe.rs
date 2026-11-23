//! The completion pipe on Windows for an SDK that opens its completion signal by name (§4.7, D-19; A-875): Node's.
//!
//! **Why a pipe opened by name.** Node's `net.Socket({ fd })` adopts what libuv's `uv_guess_handle` names, and on
//! Windows that takes a C runtime file descriptor (`uv__get_osfhandle`) and answers a pipe, a console or a file, never
//! a socket (libuv `src/win/handle.c`; Node `lib/net.js` `createHandle`, which throws `ERR_INVALID_FD_TYPE` for
//! anything else). The Windows bridge's loopback socket is no C runtime descriptor at all, so the Node SDK's
//! `AsyncClient` died at its first slow call with "Unsupported fd type: UNKNOWN" on both Windows runners
//! (`docs/bugs/2026-11-23-the-node-sdk-could-not-adopt-the-windows-completion-socket.md`). A descriptor made by this
//! addon's C runtime would not help: a module's C runtime keeps its own descriptor table, so Node's lookup would miss
//! it. What Node opens on Windows without a descriptor is a named pipe, by name (`net.connect(name)`, libuv's
//! `uv_pipe_connect`), as node-pty's ConPTY pipes are opened [C: microsoft/node-pty `windowsPtyAgent.ts`].
//!
//! **Why only this process's reader reaches it.** The name is the pipe's only way in, and it must be handed to the
//! SDK before the SDK connects, so A-179's protections are kept and one is added:
//! - a fresh name, 128 bits from the OS's random source ([`crate::stream::fresh_name`]);
//! - `FILE_FLAG_FIRST_PIPE_INSTANCE`, so no squatter's server end can be the one the SDK reaches;
//! - one instance, so a second reader is refused `ERROR_PIPE_BUSY` while one is connected;
//! - `PIPE_REJECT_REMOTE_CLIENTS`, and a descriptor that admits the owner alone ([`crate::stream::OWNER_ONLY_SDDL`]);
//! - and, since the SDK connects after the pipe is made, the bridge admits a reader only once the kernel names it this
//!   process (`GetNamedPipeClientProcessId`): any other is disconnected at once and the pipe listens again, so a
//!   process that raced the SDK to the name learns nothing and holds nothing.
//!
//! What the reader gets is the signal alone: eight nudge bytes whenever the bridge's level holds (the client armed and
//! a reply waiting), never a reply. The pipe buffers one nudge each way ([`SIGNAL_BUFFER`]): a nudge the pipe has no
//! room for means one is unread, so the reader is readable already.
//!
//! The server end is overlapped. Its connect pends on an event of its own, which the bridge's thread waits on beside
//! the region's wake until a reader is admitted (`ClientRegion::wake_wait_or_exit`), so a reply that lands before the
//! SDK has connected is signalled as soon as it does, not at the bridge's next timed look.

use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};

use windows_sys::Win32::Foundation::{
  ERROR_BROKEN_PIPE, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, ERROR_NO_DATA, ERROR_OPERATION_ABORTED,
  ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
  FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
  ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
  PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent};

use crate::error::IpcError;
use crate::stream::{OwnerOnly, fresh_name, last_error, os};

/// Format: the prefix of every completion pipe's name, in the local pipe namespace; the name's random part follows.
pub const NAME_PREFIX: &str = r"\\.\pipe\slates-completion-";

/// Derived: the bytes a completion pipe buffers each way: one nudge (`crate::completion`'s eight bytes). A nudge the
/// pipe cannot take means one is unread, and an unread nudge is the whole signal (module docs).
pub const SIGNAL_BUFFER: u32 = 8;

/// Format: the instances a completion pipe allows: one, its reader's.
const ONE_INSTANCE: u32 = 1;

/// Format: the default time-out `CreateNamedPipeW` takes: zero, the system's 50 ms, which only a client's
/// `WaitNamedPipe` reads (libuv's connect, when the one instance is busy).
const NO_WAIT_TIMEOUT: u32 = 0;

/// A completion pipe's server end and its name, made by the owner and moved into the bridge's thread, which listens
/// on it ([`Listener`]).
#[derive(Debug)]
pub struct CompletionPipe {
  /// The server end: overlapped, not inheritable.
  pub server: OwnedHandle,
  /// The name the SDK opens it by (`\\.\pipe\slates-completion-…`).
  pub name: String,
}

impl CompletionPipe {
  /// Makes a completion pipe under a fresh name, no reader connected yet (module docs).
  pub fn new() -> Result<CompletionPipe, IpcError> {
    let name = fresh_name(NAME_PREFIX)?;
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let descriptor = OwnerOnly::new()?;
    let attributes = SECURITY_ATTRIBUTES {
      nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(u32::MAX),
      lpSecurityDescriptor: descriptor.0,
      bInheritHandle: 0,
    };
    // SAFETY: a NUL-terminated name and attributes whose descriptor outlives the call; the handle returned is checked
    // and owned below.
    let server = unsafe {
      CreateNamedPipeW(
        wide.as_ptr(),
        PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
        PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
        ONE_INSTANCE,
        SIGNAL_BUFFER,
        SIGNAL_BUFFER,
        NO_WAIT_TIMEOUT,
        &attributes,
      )
    };
    if server == INVALID_HANDLE_VALUE || server.is_null() {
      return Err(os("CreateNamedPipeW"));
    }
    // SAFETY: the server end just made, owned here alone and closed on drop.
    let server = unsafe { OwnedHandle::from_raw_handle(server) };
    Ok(CompletionPipe { server, name })
  }
}

/// What a nudge met.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nudged {
  /// The pipe took it, or had a nudge unread already: the reader is readable.
  Readable,
  /// The reader has gone: the next [`Listener::settle`] listens again.
  ReaderGone,
}

/// A completion pipe's server end as its bridge's thread holds it: listening for its one reader, admitting it only
/// when it is this process, and nudging it. Made on the thread and never moved off it, so the `OVERLAPPED` its connect
/// pends on stays where the kernel was told it is; on drop a pending connect is cancelled and awaited, so the kernel
/// writes into neither the `OVERLAPPED` nor the event after they go.
pub struct Listener {
  /// The server end.
  server: OwnedHandle,
  /// The manual-reset event the pending connect signals.
  event: OwnedHandle,
  /// The pending connect's record, boxed so its address holds while the listener does.
  overlapped: Box<OVERLAPPED>,
  /// Whether a connect is with the kernel.
  pending: bool,
  /// Whether a reader of this process is connected.
  admitted: bool,
  /// Readers refused for being another process, for the tests' non-vacuity.
  refused: u64,
}

impl std::fmt::Debug for Listener {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Listener")
      .field("pending", &self.pending)
      .field("admitted", &self.admitted)
      .field("refused", &self.refused)
      .finish()
  }
}

impl Listener {
  /// Listens on `server`, the server end of a [`CompletionPipe`].
  pub fn new(server: OwnedHandle) -> Result<Listener, IpcError> {
    // SAFETY: a nameless manual-reset event, unsignalled, not inheritable; checked and owned below.
    let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if event.is_null() {
      return Err(os("CreateEventW"));
    }
    // SAFETY: the event just made, owned here alone and closed on drop, after any connect it waits for has ended.
    let event = unsafe { OwnedHandle::from_raw_handle(event) };
    let overlapped = Box::new(OVERLAPPED {
      hEvent: event.as_raw_handle(),
      ..OVERLAPPED::default()
    });
    let mut listener = Listener {
      server,
      event,
      overlapped,
      pending: false,
      admitted: false,
      refused: 0,
    };
    listener.listen_once()?;
    Ok(listener)
  }

  /// The event a pending connect signals, for a wait beside the region's wake. Unsignalled whenever no connect is
  /// pending, so a wait beside it never returns for a connect already judged.
  pub fn event(&self) -> BorrowedHandle<'_> {
    self.event.as_handle()
  }

  /// Whether a reader of this process is connected.
  pub fn admitted(&self) -> bool {
    self.admitted
  }

  /// Readers refused for being another process.
  pub fn refused(&self) -> u64 {
    self.refused
  }

  /// One step toward an admitted reader, never waiting: settles a pending connect, or starts one where none is with
  /// the kernel. At most one connect is started and one reader judged a step, so a process that connects again and
  /// again is met once a turn of the bridge, never in a loop here. Whether a reader of this process is admitted.
  pub fn settle(&mut self) -> Result<bool, IpcError> {
    if self.admitted {
      return Ok(true);
    }
    if !self.pending {
      self.listen_once()?;
      return Ok(self.admitted);
    }
    let mut moved = 0u32;
    // SAFETY: asks after the connect the boxed OVERLAPPED started on this live handle, without waiting; `moved` is a
    // local.
    let finished = unsafe {
      GetOverlappedResult(
        self.server.as_raw_handle(),
        &*self.overlapped,
        &mut moved,
        0,
      )
    };
    if finished == 0 {
      let code = last_error();
      if code == ERROR_IO_INCOMPLETE {
        return Ok(false);
      }
      self.pending = false;
      self.quiet()?;
      if code == ERROR_NO_DATA || code == ERROR_BROKEN_PIPE {
        // A reader connected and went before it was judged: let it go; the next step listens again.
        self.disconnect();
        return Ok(false);
      }
      return Err(IpcError::OsRefused {
        call: "GetOverlappedResult(ConnectNamedPipe)",
        code: i32::try_from(code).ok(),
      });
    }
    self.pending = false;
    self.quiet()?;
    self.judge()?;
    Ok(self.admitted)
  }

  /// Starts one connect: pends on [`Self::event`], or finds a reader connected already and judges it.
  fn listen_once(&mut self) -> Result<(), IpcError> {
    self.admitted = false;
    self.quiet()?;
    // SAFETY: the live server end and the boxed OVERLAPPED, which outlives any connect it starts (awaited on drop).
    let done = unsafe { ConnectNamedPipe(self.server.as_raw_handle(), &mut *self.overlapped) };
    let code = if done == 0 { last_error() } else { 0 };
    if done != 0 || code == ERROR_PIPE_CONNECTED {
      return self.judge();
    }
    if code == ERROR_IO_PENDING {
      self.pending = true;
      return Ok(());
    }
    if code == ERROR_NO_DATA {
      // A reader connected and closed before this connect: let it go; the next step listens again.
      self.disconnect();
      return Ok(());
    }
    Err(IpcError::OsRefused {
      call: "ConnectNamedPipe",
      code: i32::try_from(code).ok(),
    })
  }

  /// Resets [`Self::event`], so it is signalled only by a connect that is pending.
  fn quiet(&self) -> Result<(), IpcError> {
    // SAFETY: the live manual-reset event this listener owns; no connect is pending when this is called.
    if unsafe { ResetEvent(self.event.as_raw_handle()) } == 0 {
      return Err(os("ResetEvent"));
    }
    Ok(())
  }

  /// A reader is connected: admitted if the kernel names it this process, else counted, disconnected and left for the
  /// next step to listen past.
  fn judge(&mut self) -> Result<(), IpcError> {
    let mut reader = 0u32;
    // SAFETY: the live, connected server end and an out-pointer to a local.
    if unsafe { GetNamedPipeClientProcessId(self.server.as_raw_handle(), &mut reader) } == 0 {
      return Err(os("GetNamedPipeClientProcessId"));
    }
    if reader == std::process::id() {
      self.admitted = true;
      return Ok(());
    }
    self.refused = self.refused.saturating_add(1);
    self.disconnect();
    Ok(())
  }

  /// Cuts the connected reader off, leaving the server end open to listen again.
  fn disconnect(&mut self) {
    self.admitted = false;
    // SAFETY: the live server end; no connect of this listener is with the kernel (one is settled before).
    unsafe { DisconnectNamedPipe(self.server.as_raw_handle()) };
  }

  /// Nudges the admitted reader with `bytes`, never waiting for it (`crate::stream::write_now`). A reader gone is let
  /// go, and the next [`Self::settle`] listens again. A nudge the pipe has no room for is cancelled
  /// (`ERROR_OPERATION_ABORTED`) and leaves the reader readable already. Any other refusal is the caller's.
  pub fn nudge(&mut self, bytes: &[u8]) -> Result<Nudged, IpcError> {
    let code_of = |gone: u32| i32::try_from(gone).ok();
    match crate::stream::write_now(self.server.as_handle(), bytes) {
      Ok(_) => Ok(Nudged::Readable),
      Err(IpcError::OsRefused {
        code: Some(code), ..
      }) if code_of(ERROR_OPERATION_ABORTED) == Some(code) => Ok(Nudged::Readable),
      Err(IpcError::OsRefused {
        code: Some(code), ..
      }) if [ERROR_NO_DATA, ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED]
        .iter()
        .any(|gone| code_of(*gone) == Some(code)) =>
      {
        self.disconnect();
        Ok(Nudged::ReaderGone)
      }
      Err(refused) => Err(refused),
    }
  }
}

impl Drop for Listener {
  fn drop(&mut self) {
    if self.pending {
      let mut moved = 0u32;
      // SAFETY: cancels the one connect the boxed OVERLAPPED started on this live handle, then waits for its end, so
      // the kernel holds neither the OVERLAPPED nor the event when they go.
      unsafe {
        CancelIoEx(self.server.as_raw_handle(), &*self.overlapped);
        GetOverlappedResult(
          self.server.as_raw_handle(),
          &*self.overlapped,
          &mut moved,
          1,
        );
      }
    }
  }
}
