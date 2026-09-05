//! The I/O completion port driver (Windows): `PostQueuedCompletionStatus` for kicks and no-ops,
//! `GetQueuedCompletionStatusEx` with a timeout for the wait [B: Microsoft Learn].

use std::time::Instant;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT};
use windows_sys::Win32::System::IO::{
  CreateIoCompletionPort, GetQueuedCompletionStatusEx, OVERLAPPED_ENTRY, PostQueuedCompletionStatus,
};

use crate::driver::{Completion, Driver, DriverKind, Kick, nanos_since};
use crate::error::RtError;

/// Format: the completion key that marks a kick.
const KICK_KEY: usize = usize::MAX;
/// Format: the completion key that marks a no-op; its user word rides in the overlapped pointer.
const NOP_KEY: usize = usize::MAX - 1;

/// Shape: entries drained per wait (see the kqueue driver for the reasoning).
const EVENTS_PER_WAIT: usize = 64;

/// The driver.
pub struct IocpDriver {
  port: HANDLE,
  epoch: Instant,
  entries: Vec<OVERLAPPED_ENTRY>,
}

impl std::fmt::Debug for IocpDriver {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("IocpDriver")
      .field("port", &self.port)
      .field("entries", &self.entries.len())
      .finish()
  }
}

impl IocpDriver {
  /// Creates the port with one concurrent thread (the shard).
  pub fn new() -> Result<IocpDriver, RtError> {
    // SAFETY: creating a fresh port; the result is checked.
    let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1) };
    if port.is_null() {
      return Err(RtError::os("CreateIoCompletionPort"));
    }
    // SAFETY: an all-zero OVERLAPPED_ENTRY is a valid, empty record.
    let entries = (0..EVENTS_PER_WAIT)
      .map(|_| unsafe { std::mem::zeroed() })
      .collect();
    Ok(IocpDriver {
      port,
      epoch: Instant::now(),
      entries,
    })
  }
}

/// Posts a kick packet; safe from any thread.
pub fn post_kick(port: usize) {
  let handle: HANDLE = std::ptr::with_exposed_provenance_mut(port);
  // SAFETY: a valid port handle (a closed one fails harmlessly).
  unsafe { PostQueuedCompletionStatus(handle, 0, KICK_KEY, std::ptr::null_mut()) };
}

impl Drop for IocpDriver {
  fn drop(&mut self) {
    // SAFETY: ours to close.
    unsafe { CloseHandle(self.port) };
  }
}

impl Driver for IocpDriver {
  fn kind(&self) -> DriverKind {
    DriverKind::Iocp
  }

  fn kick_handle(&self) -> Kick {
    Kick::Iocp(self.port.expose_provenance())
  }

  fn now_ns(&self) -> u64 {
    nanos_since(self.epoch)
  }

  fn wait(&mut self, timeout_ns: Option<u64>, out: &mut Vec<Completion>) -> Result<(), RtError> {
    /// Format: nanoseconds per millisecond.
    const NANOS_PER_MILLI: u64 = 1_000_000;
    /// Format: INFINITE.
    const INFINITE: u32 = u32::MAX;
    let timeout_ms = timeout_ns.map_or(INFINITE, |ns| {
      u32::try_from(ns.div_ceil(NANOS_PER_MILLI)).unwrap_or(INFINITE - 1)
    });
    let mut count: u32 = 0;
    let capacity = u32::try_from(self.entries.len()).unwrap_or(u32::MAX);
    // SAFETY: the entry buffer holds `capacity` records and `count` is writable.
    let ok = unsafe {
      GetQueuedCompletionStatusEx(
        self.port,
        self.entries.as_mut_ptr(),
        capacity,
        &raw mut count,
        timeout_ms,
        0,
      )
    };
    if ok == 0 {
      let code = std::io::Error::last_os_error().raw_os_error();
      return if code == Some(i32::try_from(WAIT_TIMEOUT).unwrap_or(0)) {
        Ok(())
      } else {
        Err(RtError::DriverRefused {
          call: "GetQueuedCompletionStatusEx",
          code,
        })
      };
    }
    for entry in self
      .entries
      .iter()
      .take(usize::try_from(count).unwrap_or(0))
    {
      match entry.lpCompletionKey {
        KICK_KEY => {}
        NOP_KEY => out.push(Completion {
          user_data: u64::try_from(entry.lpOverlapped.addr()).unwrap_or(0),
          result: 0,
        }),
        key => out.push(Completion {
          user_data: u64::try_from(key).unwrap_or(0),
          result: i32::try_from(entry.dwNumberOfBytesTransferred).unwrap_or(i32::MAX),
        }),
      }
    }
    Ok(())
  }

  fn has_pending(&self) -> bool {
    false
  }

  fn submit_nop(&mut self, user_data: u64) -> Result<(), RtError> {
    let overlapped = std::ptr::without_provenance_mut(usize::try_from(user_data).unwrap_or(0));
    // SAFETY: the overlapped pointer is a tag the wait reads back as an address, never dereferenced.
    if unsafe { PostQueuedCompletionStatus(self.port, 0, NOP_KEY, overlapped) } == 0 {
      return Err(RtError::os("PostQueuedCompletionStatus"));
    }
    Ok(())
  }
}
