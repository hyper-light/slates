//! The client-side completion bridge (§4.7 "signal the completion fd (eventfd / pipe / socket)").
//!
//! An async SDK event loop needs a descriptor it can poll that becomes readable when a reply
//! lands, so `add_reader` / `uv_poll` fires (D-19, R6). On Linux the daemon writes a shared
//! eventfd directly — the rendezvous passed it by `SCM_RIGHTS` — and no bridge is needed. macOS
//! and Windows pass no descriptor: Mach messages and filesystem-named sockets are both refused
//! (D-10, "rendezvous with zero filesystem entries"), so there is no shared fd for the daemon to
//! write. The completion fd there is instead a *client-local* self-pipe (macOS) — a loopback
//! socket on Windows, owed — and this bridge is the thread that makes it readable: it parks on the
//! very wake word the daemon already signals on a reply to a parked client (§4.7 "Wake strategy"),
//! and on each woken change writes the pipe.
//!
//! The daemon is unchanged. It bumps the wake word on every reply and wakes it only when the
//! client is parked (`client_parked != 0`); the async SDK sets that flag when it yields to the
//! event loop (the slow path) and clears it once the reply is taken. So a fast-path reply — taken
//! during the spin, the client never parked — costs the daemon no wake and this bridge no pipe
//! write, and the async fast path stays free of any event-loop wakeup (§4.7 worked example: the
//! extension "returns the reply without ever touching the event loop"). The bridge writes only for
//! an armed reply, gating on the same `client_parked` word the daemon reads.
//!
//! One thread per async client, owned by the [`crate::endpoint::ClientEnd`] and joined on drop (no
//! fire-and-forget, banned item 9). It is the `DoorbellThread` pattern from `slates-server`: the
//! thread holds its own second mapping of the region, so a stop bumps and wakes the word to unpark
//! it; there is no `Arc` (R2) — the stop flag is a `Box::leak`'d `&'static AtomicBool` the thread
//! and the owner share, exactly as the doorbell's is, and it lives for the client's own lifetime.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use crate::error::IpcError;
use crate::region::ClientRegion;
use crate::wake;

/// Shape: the bridge's wait upper bound in nanoseconds, so a stop is observed within it even with
/// no intervening reply to wake the word; matched to the daemon doorbell thread's bound (a second),
/// far above any reply cadence, so it never adds a spurious wakeup on a live client.
const POLL_NS: u64 = 1_000_000_000;

/// Format: the eight bytes the bridge writes to make the completion pipe readable. The value is
/// never read — the pipe's readability is the whole signal — so any nonzero bytes serve; `1`
/// matches the eventfd add-one convention the Linux path uses for the same purpose.
const NUDGE: [u8; 8] = 1u64.to_ne_bytes();

/// The client-side completion bridge for a platform whose rendezvous passes no completion fd
/// (macOS today; Windows owed). Owns the poll descriptor and the thread that feeds it.
pub struct CompletionBridge {
  /// The self-pipe's read end: the descriptor an async SDK event loop polls.
  read: OwnedFd,
  /// The thread's stop flag, shared as `&'static` (the doorbell pattern); set on drop.
  stop: &'static AtomicBool,
  /// The bridge thread, joined on drop.
  handle: Option<JoinHandle<()>>,
  /// A second mapping of the region, so [`Self::stop`] can bump and wake the word the thread parks
  /// on and unpark it for the join.
  waker: ClientRegion,
}

impl std::fmt::Debug for CompletionBridge {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("CompletionBridge")
      .field("running", &self.handle.is_some())
      .finish()
  }
}

impl CompletionBridge {
  /// Starts the bridge over `region`: a self-pipe whose write end the thread nudges on an armed
  /// reply, whose read end the SDK polls. The thread parks on the region's wake word through its
  /// own mapping.
  pub fn start(region: &ClientRegion) -> Result<CompletionBridge, IpcError> {
    let (handoff, len) = region.handoff()?;
    // Two independent second mappings of the one shared region: the thread waits on one, the owner
    // wakes the other to stop it. Both see the same physical wake word (the doorbell pattern).
    let waiter = ClientRegion::open(&handoff, len)?;
    let waker = ClientRegion::open(&handoff, len)?;
    let (read, write) = self_pipe()?;
    // The flag lives for the client: the thread holds it as `&'static` and the owner stops the
    // thread through it, as the daemon's doorbell thread does. No `Arc` (R2).
    let stop: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
    let handle = std::thread::Builder::new()
      .name("slates-completion".to_owned())
      .spawn(move || run(waiter, write, stop))
      .ok();
    Ok(CompletionBridge {
      read,
      stop,
      handle,
      waker,
    })
  }

  /// The descriptor the async SDK polls (`add_reader` / `uv_poll`).
  pub fn completion_fd(&self) -> RawFd {
    self.read.as_raw_fd()
  }

  /// A dup of the read end, owned by the caller — for an SDK whose event loop closes the descriptor
  /// it polls (Node's `net.Socket` does; `asyncio` does not, so it polls [`Self::completion_fd`]). The
  /// dup refers to the same pipe, so closing it leaves this bridge's read end intact. A safe dup: the
  /// read end is an owned fd, not a raw one.
  pub fn dup_fd(&self) -> Result<OwnedFd, IpcError> {
    rustix::io::dup(&self.read).map_err(|error| IpcError::OsRefused {
      call: "dup",
      code: Some(error.raw_os_error()),
    })
  }

  /// Clears the pipe's readiness after the loop reports it readable: a non-blocking read that takes
  /// whatever nudges are buffered. The read end is non-blocking, so an empty pipe is a no-op.
  pub fn drain(&self) {
    let mut scratch = [0u8; 64];
    let _ = rustix::io::read(&self.read, &mut scratch);
  }

  /// Stops the thread and joins it: set the flag, then bump and wake the word so a parked thread
  /// returns and sees the flag.
  fn stop(&mut self) {
    self.stop.store(true, Ordering::Release);
    if let Ok(word) = self.waker.wake_word() {
      word.fetch_add(1, Ordering::AcqRel);
      let _ = wake::wake_one(word);
    }
    if let Some(handle) = self.handle.take() {
      let _ = handle.join();
    }
  }
}

impl Drop for CompletionBridge {
  fn drop(&mut self) {
    self.stop();
  }
}

/// The bridge thread: park on the wake word; on a woken change, if the client is armed (parked),
/// make the completion pipe readable. A disarmed change — a fast-path reply the client took during
/// its spin — is skipped, so the async fast path never wakes the event loop.
fn run(region: ClientRegion, write: OwnedFd, stop: &'static AtomicBool) {
  let Ok(word) = region.wake_word() else {
    return;
  };
  // The value last acted on: a reply that lands between two waits shows as a change on the next
  // comparison, never lost (the wait compares against `seen`, and returns at once when the word has
  // already moved past it), so no wake is dropped even off the parked instant.
  let mut seen = word.load(Ordering::Acquire);
  while !stop.load(Ordering::Acquire) {
    let _ = wake::wait(word, seen, Some(POLL_NS));
    if stop.load(Ordering::Acquire) {
      break;
    }
    let now = word.load(Ordering::Acquire);
    if now == seen {
      // A bare timeout with no reply: nothing to signal.
      continue;
    }
    seen = now;
    // Only an armed client is one waiting on its event loop; a fast-path reply (parked == 0, taken
    // during the spin) needs no nudge and gets none, so the daemon's no-wake fast path stays free.
    let armed = region
      .client_parked()
      .is_ok_and(|parked| parked.load(Ordering::Acquire) != 0);
    if armed {
      // Non-blocking: a full pipe is already readable, so a dropped nudge only means the SDK's next
      // poll finds the reply, never a lost one — the same best-effort as the Linux eventfd nudge.
      let _ = rustix::io::write(&write, &NUDGE);
    }
  }
}

/// A non-blocking, close-on-exec self-pipe: `(read, write)`. The thread writes the write end to
/// signal readiness; the SDK polls the read end. Both are non-blocking so [`CompletionBridge::drain`]
/// never blocks and the thread never stalls on a full pipe.
fn self_pipe() -> Result<(OwnedFd, OwnedFd), IpcError> {
  // `std::io::pipe` sets close-on-exec atomically (no fd leak across a concurrent exec); macOS has
  // no `pipe2`, so the non-blocking bit is set after with an ioctl, as the endpoint's socketpair is.
  let (reader, writer) = std::io::pipe().map_err(|e| IpcError::OsRefused {
    call: "pipe",
    code: e.raw_os_error(),
  })?;
  let read = OwnedFd::from(reader);
  let write = OwnedFd::from(writer);
  set_nonblocking(&read)?;
  set_nonblocking(&write)?;
  Ok((read, write))
}

/// Sets the descriptor non-blocking (macOS lacks the creation-time flag).
fn set_nonblocking(fd: &OwnedFd) -> Result<(), IpcError> {
  rustix::io::ioctl_fionbio(fd, true).map_err(|e| IpcError::OsRefused {
    call: "ioctl_fionbio",
    code: Some(e.raw_os_error()),
  })
}
