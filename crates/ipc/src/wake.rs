//! The wake word per OS (§4.7 "Wake strategy"; `research/low-latency-ipc-and-runtime.md`
//! §2.2): a 32-bit word in the shared region the client waits on and the daemon bumps.
//!
//! - Linux: a shared futex (`FUTEX_WAIT` without the private flag works across processes on
//!   a shared mapping, whatever the virtual addresses).
//! - macOS: `os_sync_wait_on_address` with `OS_SYNC_WAIT_ON_ADDRESS_SHARED` and the matching
//!   `os_sync_wake_by_address_any` (macOS 14.4+); a spurious wake surfaces as `EINTR` and the
//!   caller re-checks.
//! - Windows: `WaitOnAddress` wakes only threads of the same process, so the cross-process wake is a
//!   named auto-reset [`Event`] per client (below), derived from the region's object name so both ends
//!   open the one Event; the endpoint waits on and signals it rather than the wake word. The word-based
//!   [`wait`]/[`wake_one`] below stay `Unsupported` there — they are the Linux/macOS path.
//!
//! Every function takes the word by atomic reference and an `expected` value: the wait
//! returns at once when the word no longer equals it, which is what closes the race between
//! the parked flag and the reply.

use std::sync::atomic::AtomicU32;

use crate::error::IpcError;

/// Waits until the word differs from `expected` or `timeout_ns` passes (`None`: forever).
/// Returns `Ok(true)` when woken or the word moved, `Ok(false)` at the timeout.
pub fn wait(word: &AtomicU32, expected: u32, timeout_ns: Option<u64>) -> Result<bool, IpcError> {
  platform::wait(word, expected, timeout_ns)
}

/// Wakes one waiter on the word.
pub fn wake_one(word: &AtomicU32) -> Result<(), IpcError> {
  platform::wake_one(word)
}

#[cfg(target_os = "linux")]
mod platform {
  use std::sync::atomic::AtomicU32;

  use rustix::thread::futex;

  use crate::error::IpcError;

  /// Format: nanoseconds per second, for the timeout.
  const NS_PER_S: u64 = 1_000_000_000;

  pub(super) fn wait(
    word: &AtomicU32,
    expected: u32,
    timeout_ns: Option<u64>,
  ) -> Result<bool, IpcError> {
    let timeout = timeout_ns.map(|ns| rustix::thread::Timespec {
      tv_sec: i64::try_from(ns / NS_PER_S).unwrap_or(i64::MAX),
      tv_nsec: i64::try_from(ns % NS_PER_S).unwrap_or(0),
    });
    match futex::wait(word, futex::Flags::empty(), expected, timeout.as_ref()) {
      Ok(()) => Ok(true),
      Err(rustix::io::Errno::AGAIN) | Err(rustix::io::Errno::INTR) => Ok(true),
      Err(rustix::io::Errno::TIMEDOUT) => Ok(false),
      Err(e) => Err(IpcError::OsRefused {
        call: "futex_wait",
        code: Some(e.raw_os_error()),
      }),
    }
  }

  pub(super) fn wake_one(word: &AtomicU32) -> Result<(), IpcError> {
    futex::wake(word, futex::Flags::empty(), 1)
      .map(|_| ())
      .map_err(|e| IpcError::OsRefused {
        call: "futex_wake",
        code: Some(e.raw_os_error()),
      })
  }
}

#[cfg(target_os = "macos")]
mod platform {
  use std::sync::atomic::AtomicU32;

  use crate::error::IpcError;

  pub(super) fn wait(
    word: &AtomicU32,
    expected: u32,
    timeout_ns: Option<u64>,
  ) -> Result<bool, IpcError> {
    let addr = word.as_ptr().cast::<std::ffi::c_void>();
    // SAFETY: `addr` is a live 4-byte word in a shared mapping for the call's duration; the
    // flags and size describe exactly that word; the call writes nothing to memory.
    let rc = unsafe {
      match timeout_ns {
        None => libc::os_sync_wait_on_address(
          addr,
          u64::from(expected),
          size_of::<u32>(),
          libc::OS_SYNC_WAIT_ON_ADDRESS_SHARED,
        ),
        Some(ns) => libc::os_sync_wait_on_address_with_timeout(
          addr,
          u64::from(expected),
          size_of::<u32>(),
          libc::OS_SYNC_WAIT_ON_ADDRESS_SHARED,
          libc::OS_CLOCK_MACH_ABSOLUTE_TIME,
          ns,
        ),
      }
    };
    if rc >= 0 {
      return Ok(true);
    }
    match std::io::Error::last_os_error().raw_os_error() {
      Some(libc::ETIMEDOUT) => Ok(false),
      Some(libc::EINTR) => Ok(true),
      code => Err(IpcError::OsRefused {
        call: "os_sync_wait_on_address",
        code,
      }),
    }
  }

  pub(super) fn wake_one(word: &AtomicU32) -> Result<(), IpcError> {
    let addr = word.as_ptr().cast::<std::ffi::c_void>();
    // SAFETY: `addr` is a live 4-byte word in a shared mapping; the call reads nothing but
    // the address.
    let rc = unsafe {
      libc::os_sync_wake_by_address_any(
        addr,
        size_of::<u32>(),
        libc::OS_SYNC_WAKE_BY_ADDRESS_SHARED,
      )
    };
    if rc >= 0 {
      return Ok(());
    }
    match std::io::Error::last_os_error().raw_os_error() {
      // No waiter: nothing to wake.
      Some(libc::ENOENT) => Ok(()),
      code => Err(IpcError::OsRefused {
        call: "os_sync_wake_by_address_any",
        code,
      }),
    }
  }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
  use std::sync::atomic::AtomicU32;

  use crate::error::IpcError;

  pub(super) fn wait(
    _word: &AtomicU32,
    _expected: u32,
    _timeout_ns: Option<u64>,
  ) -> Result<bool, IpcError> {
    Err(IpcError::Unsupported {
      feature: "the cross-process wake word (a named Event per client on Windows)",
    })
  }

  pub(super) fn wake_one(_word: &AtomicU32) -> Result<(), IpcError> {
    Err(IpcError::Unsupported {
      feature: "the cross-process wake word (a named Event per client on Windows)",
    })
  }
}

/// The Windows cross-process wake: a named auto-reset [`Event`] per waiter. `WaitOnAddress` wakes only
/// threads of the same process (D-10), so the word-based [`wait`]/[`wake_one`] above cannot cross a
/// process boundary here; the endpoint waits on and signals this Event instead. Both processes
/// `CreateEventW` the same `Local\`-prefixed name, so each holds a handle to the one Event with no
/// handle passing.
#[cfg(windows)]
pub use win_event::Event;

#[cfg(windows)]
mod win_event {
  use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
  use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};

  use crate::error::IpcError;

  /// Format: the no-timeout wait sentinel (`INFINITE`).
  const INFINITE: u32 = u32::MAX;
  /// Format: nanoseconds per millisecond — the Windows wait's unit is the millisecond, so a nanosecond
  /// budget is rounded up (a shorter-than-a-millisecond wait rounds to one, never to zero-poll).
  const NANOS_PER_MILLI: u64 = 1_000_000;

  /// A named auto-reset Event — the cross-process wake on Windows. Auto-reset means a signal made
  /// before the waiter parks is not lost: the Event stays signaled until one wait consumes it, which
  /// closes the park race the wake word closes on Linux/macOS with its value re-check. The handle is
  /// stored as its exposed address so the type is `Send` (a handle belongs to the process, not a
  /// thread) without an unsafe impl — the `slates-mem` idiom.
  pub struct Event {
    handle: usize,
  }

  /// The `Local\`-prefixed, null-terminated wide name the kernel object namespace takes (per-session,
  /// the `slates-mem` shared-object convention).
  fn wide(name: &str) -> Vec<u16> {
    format!("Local\\{name}")
      .encode_utf16()
      .chain(std::iter::once(0))
      .collect()
  }

  fn os(call: &'static str) -> IpcError {
    IpcError::OsRefused {
      call,
      code: std::io::Error::last_os_error().raw_os_error(),
    }
  }

  impl Event {
    /// Creates or opens the named auto-reset Event (default DACL — the `Local\` namespace is the
    /// session's; manual-reset off; initial state non-signaled).
    pub fn open(name: &str) -> Result<Event, IpcError> {
      let name = wide(name);
      // SAFETY: a null attributes pointer (the default per-session security), the two `BOOL`s `0`/`0`
      // (auto-reset, initially non-signaled), and a valid null-terminated wide name.
      let handle = unsafe { CreateEventW(std::ptr::null(), 0, 0, name.as_ptr()) };
      if handle.is_null() {
        return Err(os("CreateEventW"));
      }
      Ok(Event {
        handle: handle.expose_provenance(),
      })
    }

    fn handle(&self) -> HANDLE {
      std::ptr::with_exposed_provenance_mut(self.handle)
    }

    /// Signals the Event, waking one waiter (a signal with no waiter is remembered until the next wait).
    pub fn signal(&self) -> Result<(), IpcError> {
      // SAFETY: the handle is a live Event this struct owns.
      if unsafe { SetEvent(self.handle()) } == 0 {
        return Err(os("SetEvent"));
      }
      Ok(())
    }

    /// Waits up to `timeout_ns` (`None`: forever). `Ok(true)` when signaled, `Ok(false)` at the timeout.
    pub fn wait(&self, timeout_ns: Option<u64>) -> Result<bool, IpcError> {
      let ms = timeout_ns.map_or(INFINITE, |ns| {
        u32::try_from(ns.div_ceil(NANOS_PER_MILLI)).unwrap_or(INFINITE - 1)
      });
      // SAFETY: the handle is a live Event this struct owns.
      let rc = unsafe { WaitForSingleObject(self.handle(), ms) };
      if rc == WAIT_OBJECT_0 {
        Ok(true)
      } else if rc == WAIT_TIMEOUT {
        Ok(false)
      } else {
        Err(os("WaitForSingleObject"))
      }
    }
  }

  impl Drop for Event {
    fn drop(&mut self) {
      // SAFETY: the handle was created by this module and is ours.
      unsafe {
        CloseHandle(self.handle());
      }
    }
  }
}
