//! The I/O completion port driver (Windows): `PostQueuedCompletionStatus` for kicks and no-ops,
//! `GetQueuedCompletionStatusEx` with a timeout for the wait [B: Microsoft Learn].

use std::time::Instant;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT};
use windows_sys::Win32::Networking::WinSock::SOCKET;
use windows_sys::Win32::System::IO::{
  CreateIoCompletionPort, GetQueuedCompletionStatusEx, OVERLAPPED_ENTRY, PostQueuedCompletionStatus,
};

use crate::afd::{Afd, Block, READABLE_EVENTS, WRITABLE_EVENTS, base_socket};
use crate::driver::{Completion, Driver, DriverKind, Kick, nanos_since};
use crate::error::RtError;

/// Format: the completion key that marks a kick.
const KICK_KEY: usize = usize::MAX;
/// Format: the completion key that marks a no-op; its user word rides in the overlapped pointer.
const NOP_KEY: usize = usize::MAX - 1;
/// Format: the completion key the AFD readiness device is associated under. Its completions carry a
/// poll block in the overlapped pointer (its head is the block), not a user word in the key.
const AFD_KEY: usize = usize::MAX - 2;

/// Shape: entries drained per wait (see the kqueue driver for the reasoning).
const EVENTS_PER_WAIT: usize = 64;

/// The driver.
pub struct IocpDriver {
  port: HANDLE,
  epoch: Instant,
  entries: Vec<OVERLAPPED_ENTRY>,
  /// The AFD readiness device, opened and associated with `port` on the first socket registration
  /// (a shard that only kicks and times out never opens it). Socket readiness is polled through it.
  afd: Option<Afd>,
}

impl std::fmt::Debug for IocpDriver {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("IocpDriver")
      .field("port", &self.port)
      .field("entries", &self.entries.len())
      .finish()
  }
}

/// Creates the port with one concurrent thread (the shard); returns its exposed address, which
/// the kick carries and the shard's thread builds the driver from.
pub fn prepare() -> Result<usize, RtError> {
  // SAFETY: creating a fresh port; the result is checked.
  let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1) };
  if port.is_null() {
    return Err(RtError::os("CreateIoCompletionPort"));
  }
  Ok(port.expose_provenance())
}

impl IocpDriver {
  /// Builds the driver over a prepared port, on the shard's thread.
  pub fn from_prepared(port: usize) -> IocpDriver {
    // SAFETY: an all-zero OVERLAPPED_ENTRY is a valid, empty record.
    let entries = (0..EVENTS_PER_WAIT)
      .map(|_| unsafe { std::mem::zeroed() })
      .collect();
    IocpDriver {
      port: std::ptr::with_exposed_provenance_mut(port),
      epoch: Instant::now(),
      entries,
      afd: None,
    }
  }

  /// The AFD readiness device, opened and associated with this port on first use (§4.6). Lazily,
  /// because a driver that never awaits a socket needs no AFD handle.
  fn afd(&mut self) -> Result<&Afd, RtError> {
    if let Some(ref afd) = self.afd {
      return Ok(afd);
    }
    let afd = Afd::open()?;
    // Associate the AFD device with this port so its polls complete here under `AFD_KEY`.
    // SAFETY: both handles are live and ours; `CreateIoCompletionPort` with an existing `port`
    // associates `afd.handle()` with it and returns the port (null on failure).
    let associated = unsafe { CreateIoCompletionPort(afd.handle(), self.port, AFD_KEY, 0) };
    if associated.is_null() {
      return Err(RtError::os("CreateIoCompletionPort(AFD)"));
    }
    // `insert` stores the device and hands back a reference to it — no `expect` on a re-read.
    Ok(self.afd.insert(afd))
  }

  /// Arms a one-shot AFD poll for `events` on the socket `raw` names, waking `user_data` when it
  /// fires. A Windows `SOCKET` fits in a positive `i32` in practice (kernel handle-table values), so
  /// the readiness seam carries it as the same `i32` a Unix fd uses; it is reconstructed here as the
  /// low 32 bits, unsigned. The leaked poll block is owned by the kernel until `wait` reclaims it.
  fn arm(&mut self, raw: i32, events: u32, user_data: u64) -> Result<(), RtError> {
    // The inverse of `netsys::Socket::raw_id`: reinterpret the `i32` as its 32 bits, then widen to the
    // pointer-width `SOCKET` — bit-for-bit the handle the seam narrowed. No sign-losing `as` cast.
    let socket = u32::from_ne_bytes(raw.to_ne_bytes()) as SOCKET;
    let base = base_socket(socket)?;
    let afd = self.afd()?;
    // SAFETY: `afd()` associated the device with this port before returning it, so the poll's
    // completion is delivered here and `wait` reclaims the block exactly once.
    let _block = unsafe { afd.poll(base, events, user_data)? };
    Ok(())
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
        AFD_KEY => {
          // A socket readiness poll fired: the overlapped pointer is the leaked poll block (its head
          // is the `OVERLAPPED`), so reclaim it and wake the word it carried. The fired events are not
          // needed — the readiness future just retries its non-blocking syscall (a spurious wake, e.g.
          // for a since-dropped task, harmlessly wakes nothing).
          if !entry.lpOverlapped.is_null() {
            let block = entry.lpOverlapped.cast::<Block>();
            // SAFETY: `block` is a `Block` leaked by `Afd::poll` (its `OVERLAPPED` head is what was
            // armed) and delivered here exactly once by the port; `reclaim` takes ownership back.
            let user_data = unsafe { Block::reclaim(block) };
            out.push(Completion {
              user_data,
              result: 0,
            });
          }
        }
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

  fn register_readable(&mut self, raw: i32, user_data: u64) -> Result<(), RtError> {
    // One-shot AFD poll for the read edges (data, an incoming connection, EOF, an error): it completes
    // on this port when any fires, and `wait` wakes `user_data`. The readiness-native drivers (kqueue,
    // epoll) do this with `EVFILT_READ`/`EPOLLIN`; AFD is the Windows equivalent.
    self.arm(raw, READABLE_EVENTS, user_data)
  }

  fn register_writable(&mut self, raw: i32, user_data: u64) -> Result<(), RtError> {
    // One-shot AFD poll for the write edges (send-buffer space, a connect result, an error).
    self.arm(raw, WRITABLE_EVENTS, user_data)
  }
}
