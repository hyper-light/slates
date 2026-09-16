//! The client-side completion bridge (§4.7 "signal the completion fd (eventfd / pipe / socket)").
//!
//! An async SDK event loop needs a descriptor it can poll that becomes readable when a reply
//! lands, so `add_reader` / `uv_poll` fires (D-19, R6). On Linux the daemon writes a shared
//! eventfd directly — the rendezvous passed it by `SCM_RIGHTS` — which Python's `asyncio` polls
//! through epoll with no bridge. macOS and Windows pass no descriptor: Mach messages and
//! filesystem-named sockets are both refused (D-10, "rendezvous with zero filesystem entries"), so
//! there is no shared fd for the daemon to write. The completion fd there is instead a *client-local*
//! signal — a self-pipe on macOS, a loopback socket pair on Windows — and this bridge is the thread
//! that makes it readable: it parks on the very wake the daemon already raises on a reply to a parked
//! client (§4.7 "Wake strategy"), and on each woken change signals its end.
//!
//! Linux gains a third bridge shape for one narrow reason: an SDK that *adopts* the completion fd
//! into a stream (Node's `net.Socket`) cannot adopt an eventfd — libuv's `uv_guess_handle` returns
//! `UV_UNKNOWN_HANDLE` and the constructor throws `ERR_INVALID_FD_TYPE`. So `enable_async_completion_dup`
//! on Linux starts a bridge that converts the eventfd's readiness to a self-pipe (see the Linux arm
//! below and `docs/bugs/2026-09-16-node-async-sdk-cannot-poll-the-linux-completion-eventfd.md`); the
//! plain path still hands Python the raw eventfd.
//!
//! The three shapes differ only in the OS primitive each waits on and each signals:
//!
//! * **macOS** — the poll descriptor is a self-pipe (`pipe`); the thread parks on the region's wake
//!   *word* through its own mapping (`wake::wait`, a `__ulock` compare-and-wait that the daemon's
//!   `wake_one` reaches cross-process on this platform).
//! * **Linux** — the poll descriptor is a self-pipe; the thread parks in `poll` on the shared
//!   completion *eventfd* (the daemon writes it only for a parked client, so no `client_parked` gate
//!   is needed here) and a stop pipe for a prompt join.
//! * **Windows** — the poll descriptor is a loopback `TcpStream` pair (a socket the async runtimes
//!   can poll: libuv's `uv_poll` and a Python selector both accept a Windows `SOCKET`); the thread
//!   parks on the region's named auto-reset *Event* (`ClientRegion::wake_wait`), because
//!   `WaitOnAddress` is process-local (D-10) so the daemon signals the Event, not the word, to reach
//!   another process. The word still carries the reply count and the `client_parked` flag, read here
//!   exactly as on macOS to tell a real reply from a bare timeout and to gate on an armed client.
//!
//! The daemon is unchanged in shape. It bumps the wake word on every reply and raises the wake only
//! when the client is parked (`client_parked != 0`); the async SDK sets that flag when it yields to
//! the event loop (the slow path) and clears it once the reply is taken. So a fast-path reply —
//! taken during the spin, the client never parked — costs the daemon no wake and this bridge no
//! signal, and the async fast path stays free of any event-loop wakeup (§4.7 worked example: the
//! extension "returns the reply without ever touching the event loop"). The bridge signals only for
//! an armed reply, gating on the same `client_parked` word the daemon reads.
//!
//! One thread per async client, owned by the [`crate::endpoint::ClientEnd`] and joined on drop (no
//! fire-and-forget, banned item 9). It is the `DoorbellThread` pattern from `slates-server`: the
//! thread holds its own second mapping of the region, so a stop wakes it to unpark it; there is no
//! `Arc` (R2) — the stop flag is a `Box::leak`'d `&'static AtomicBool` the thread and the owner
//! share, exactly as the doorbell's is, and it lives for the client's own lifetime.

// The wake-word/Event bridges (macOS, Windows) hold a `&'static AtomicBool` stop flag and park on the
// region; the Linux bridge stops through a pipe and waits on the eventfd, so it needs neither.
#[cfg(any(target_os = "macos", windows))]
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use crate::error::IpcError;
#[cfg(any(target_os = "macos", windows))]
use crate::region::ClientRegion;

#[cfg(target_os = "macos")]
use crate::wake;
#[cfg(unix)]
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

#[cfg(windows)]
use std::io::{Read, Write};
#[cfg(windows)]
use std::net::{TcpListener, TcpStream};
#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, IntoRawSocket, RawSocket};

/// Shape: the bridge's wait upper bound in nanoseconds — a second, far above any reply cadence, so a
/// stop is seen within it with no spurious wakeup. Only the wake-word (macOS) and Event (Windows)
/// bridges use it; the Linux bridge waits with no timeout (its stop pipe is in the poll set).
#[cfg(any(target_os = "macos", windows))]
const POLL_NS: u64 = 1_000_000_000;

/// Format: the eight bytes the bridge writes to make the completion descriptor readable. The value
/// is never read — the descriptor's readability is the whole signal — so any nonzero bytes serve;
/// `1` matches the eventfd add-one convention the Linux path uses for the same purpose.
const NUDGE: [u8; 8] = 1u64.to_ne_bytes();

// ------------------------------------------------------------------- macOS: self-pipe over the word

/// The client-side completion bridge on macOS, whose rendezvous passes no completion fd. Owns the
/// self-pipe the async SDK polls and the thread that feeds it.
#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
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

/// The macOS bridge thread: park on the wake word; on a woken change, if the client is armed
/// (parked), make the completion pipe readable. A disarmed change — a fast-path reply the client
/// took during its spin — is skipped, so the async fast path never wakes the event loop.
#[cfg(target_os = "macos")]
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
#[cfg(unix)]
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

/// Sets the descriptor non-blocking (`std::io::pipe` sets close-on-exec but not this).
#[cfg(unix)]
fn set_nonblocking(fd: &OwnedFd) -> Result<(), IpcError> {
  rustix::io::ioctl_fionbio(fd, true).map_err(|e| IpcError::OsRefused {
    call: "ioctl_fionbio",
    code: Some(e.raw_os_error()),
  })
}

// -------------------------------------------------- Linux: self-pipe fed by the completion eventfd

/// The client-side completion bridge on Linux, for an SDK that adopts the completion descriptor into
/// a stream. The rendezvous hands Linux the daemon's shared completion *eventfd* (§4.7), which
/// Python's `asyncio` polls directly through epoll — no bridge is needed there. Node's `net.Socket`,
/// though, refuses an eventfd (`ERR_INVALID_FD_TYPE`: libuv's `uv_guess_handle` returns
/// `UV_UNKNOWN_HANDLE`, recognizing only sockets, pipes and ttys), so an SDK that adopts the fd needs
/// a *pipe* instead. This bridge is that adapter: a thread polls the eventfd and, when the daemon
/// makes it readable, drains it and nudges a self-pipe whose read end the SDK polls. The daemon
/// writes the eventfd only for a parked client (`DaemonEnd::reply` under the `client_parked` check),
/// so the async fast path never reaches here and the bridge needs no armed-flag check of its own —
/// unlike the macOS bridge, which parks on a wake word that bumps on every reply and must gate itself.
///
/// One thread per async client that adopts the fd, owned here and joined on drop (no fire-and-forget,
/// banned item 9). It stops through a second pipe: a byte on the stop pipe unparks the thread's
/// `poll` for a prompt join, the analogue of the macOS bridge waking its wake word.
#[cfg(target_os = "linux")]
pub struct CompletionBridge {
  /// The self-pipe's read end: the pipe descriptor an async SDK event loop polls (Node's `net.Socket`
  /// accepts a pipe, unlike the eventfd). The bridge thread writes the paired end on a parked reply.
  read: OwnedFd,
  /// The stop pipe's write end; a byte here unparks the thread's `poll` so a drop joins promptly.
  stop_write: OwnedFd,
  /// The bridge thread, joined on drop.
  handle: Option<JoinHandle<()>>,
}

#[cfg(target_os = "linux")]
impl CompletionBridge {
  /// Starts the bridge over a dup of the completion `eventfd` (the caller keeps its own copy, so the
  /// eventfd object outlives the bridge either way). The thread polls the eventfd and a stop pipe;
  /// when the eventfd is readable — a reply to a parked client — it drains it and writes the
  /// self-pipe, whose read end the SDK polls.
  pub fn start_from_eventfd(eventfd: OwnedFd) -> Result<CompletionBridge, IpcError> {
    let (read, write) = self_pipe()?;
    let (stop_read, stop_write) = self_pipe()?;
    let handle = std::thread::Builder::new()
      .name("slates-completion".to_owned())
      .spawn(move || run(eventfd, write, stop_read))
      .ok();
    Ok(CompletionBridge {
      read,
      stop_write,
      handle,
    })
  }

  /// The descriptor the async SDK polls (`add_reader` / `uv_poll`) — a pipe read end, adoptable by a
  /// Node stream.
  pub fn completion_fd(&self) -> RawFd {
    self.read.as_raw_fd()
  }

  /// A dup of the read end the caller owns and closes (Node's `net.Socket` adopts and closes the fd it
  /// wraps); the dup refers to the same pipe, so closing it leaves this bridge's read end intact.
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

  /// Stops the thread and joins it: write the stop pipe to unpark the poll, then join.
  fn stop(&mut self) {
    let _ = rustix::io::write(&self.stop_write, &NUDGE);
    if let Some(handle) = self.handle.take() {
      let _ = handle.join();
    }
  }
}

/// The Linux bridge thread: poll the completion eventfd and the stop pipe; on the eventfd's
/// readability (the daemon's reply to a parked client) drain it and make the self-pipe readable; on
/// the stop pipe's, return. Draining the eventfd (an eight-byte read resets its counter) clears the
/// level so the next poll blocks again; a full self-pipe already reads as readable, so a dropped
/// nudge only means the SDK's next poll finds the reply, never a lost one — the same best-effort as
/// the macOS bridge.
#[cfg(target_os = "linux")]
fn run(eventfd: OwnedFd, write: OwnedFd, stop: OwnedFd) {
  use rustix::event::{PollFd, PollFlags, poll};
  loop {
    let mut fds = [
      PollFd::new(&eventfd, PollFlags::IN),
      PollFd::new(&stop, PollFlags::IN),
    ];
    // No timeout: the stop pipe is always in the set, so a drop wakes the poll at once.
    match poll(&mut fds, None) {
      Ok(_) => {}
      Err(rustix::io::Errno::INTR) => continue,
      Err(_) => break,
    }
    // The stop pipe readable, or any error/hangup on it: the owner is dropping the bridge.
    if !(fds[1].revents() & (PollFlags::IN | PollFlags::HUP | PollFlags::ERR)).is_empty() {
      break;
    }
    let events = fds[0].revents();
    // The eventfd never reports a hangup (it has no peer), but an error means it is gone: stop.
    if !(events & PollFlags::ERR).is_empty() {
      break;
    }
    if events.contains(PollFlags::IN) {
      // Drain the eventfd (an eight-byte counter read resets its level) so the next poll blocks, then
      // nudge the SDK's pipe.
      let mut word = [0u8; size_of::<u64>()];
      let _ = rustix::io::read(&eventfd, &mut word);
      let _ = rustix::io::write(&write, &NUDGE);
    }
  }
}

// -------------------------------------------------------------- Windows: loopback socket over Event

/// The client-side completion bridge on Windows, whose rendezvous passes no completion fd. Owns the
/// loopback socket the async SDK polls and the thread that feeds it. The two ends of a connected
/// loopback pair: the thread writes `write` (moved into it), the SDK polls `read` — writing one end
/// makes the other readable.
#[cfg(windows)]
pub struct CompletionBridge {
  /// The read end of the loopback pair: the socket an async SDK event loop polls (`uv_poll` / a
  /// selector). Held so [`Self::drain`] can clear its readiness and [`Self::dup_socket`] clone it.
  read: TcpStream,
  /// The thread's stop flag, shared as `&'static` (the doorbell pattern); set on drop.
  stop: &'static AtomicBool,
  /// The bridge thread, joined on drop.
  handle: Option<JoinHandle<()>>,
  /// A second mapping of the region, so [`Self::stop`] can signal the named Event the thread parks
  /// on and unpark it for the join.
  waker: ClientRegion,
}

#[cfg(windows)]
impl CompletionBridge {
  /// Starts the bridge over `region`: a loopback socket pair whose write end the thread nudges on an
  /// armed reply, whose read end the SDK polls. The thread parks on the region's named auto-reset
  /// Event through its own mapping (the daemon signals that Event on a reply to a parked client,
  /// because `WaitOnAddress` on the word is process-local, D-10).
  pub fn start(region: &ClientRegion) -> Result<CompletionBridge, IpcError> {
    let (handoff, len) = region.handoff()?;
    // Two independent second mappings of the one shared region: the thread waits on the Event
    // through one, the owner signals it through the other to stop the thread (the doorbell pattern).
    let waiter = ClientRegion::open(&handoff, len)?;
    let waker = ClientRegion::open(&handoff, len)?;
    let (read, write) = loopback_pair()?;
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

  /// The socket the async SDK polls (`uv_poll` / a Python selector). A Windows `SOCKET`, exposed as
  /// the platform `RawSocket` the SDK layers register.
  pub fn completion_socket(&self) -> RawSocket {
    self.read.as_raw_socket()
  }

  /// A dup of the read end, owned by the caller — for an SDK whose event loop closes the socket it
  /// polls (Node's `net.Socket` does). `try_clone` duplicates the underlying `SOCKET` (a real handle
  /// dup), and `into_raw_socket` hands ownership of that duplicate to the caller, so closing it
  /// leaves this bridge's read end intact. A safe dup: no raw-handle construction.
  pub fn dup_socket(&self) -> Result<RawSocket, IpcError> {
    Ok(
      self
        .read
        .try_clone()
        .map_err(|e| IpcError::OsRefused {
          call: "try_clone",
          code: e.raw_os_error(),
        })?
        .into_raw_socket(),
    )
  }

  /// Clears the socket's readiness after the loop reports it readable: a non-blocking read that
  /// takes whatever nudges are buffered. The read end is non-blocking, so an empty socket is a
  /// no-op (`&TcpStream` reads without a `&mut`, and returns `WouldBlock` when drained).
  pub fn drain(&self) {
    let mut scratch = [0u8; 64];
    let _ = (&self.read).read(&mut scratch);
  }

  /// Stops the thread and joins it: set the flag, then signal the named Event so a parked thread
  /// returns and sees the flag.
  fn stop(&mut self) {
    self.stop.store(true, Ordering::Release);
    let _ = self.waker.wake_signal();
    if let Some(handle) = self.handle.take() {
      let _ = handle.join();
    }
  }
}

/// The Windows bridge thread: park on the region's named Event; on a woken change, if the client is
/// armed (parked), make the completion socket readable. A disarmed change — a fast-path reply the
/// client took during its spin — is skipped, so the async fast path never wakes the event loop. The
/// word carries the reply count (to tell a real reply from a bare Event timeout) and the
/// `client_parked` flag (to gate the nudge), read exactly as the macOS thread reads them.
#[cfg(windows)]
fn run(region: ClientRegion, mut write: TcpStream, stop: &'static AtomicBool) {
  let Ok(word) = region.wake_word() else {
    return;
  };
  // The value last acted on: a reply that lands between two waits shows as a change on the next
  // comparison, never lost — the auto-reset Event holds its signal until a waiter consumes it, so a
  // reply raised off the parked instant returns the next `wake_wait` at once.
  let mut seen = word.load(Ordering::Acquire);
  while !stop.load(Ordering::Acquire) {
    let _ = region.wake_wait(Some(POLL_NS));
    if stop.load(Ordering::Acquire) {
      break;
    }
    let now = word.load(Ordering::Acquire);
    if now == seen {
      // A bare Event timeout with no reply: nothing to signal.
      continue;
    }
    seen = now;
    // Only an armed client is one waiting on its event loop; a fast-path reply (parked == 0, taken
    // during the spin) needs no nudge and gets none, so the daemon's no-wake fast path stays free.
    let armed = region
      .client_parked()
      .is_ok_and(|parked| parked.load(Ordering::Acquire) != 0);
    if armed {
      // Non-blocking: a full send buffer means the socket is already readable at the peer, so a
      // dropped nudge only means the SDK's next poll finds the reply, never a lost one.
      let _ = write.write(&NUDGE);
    }
  }
}

/// A connected, non-blocking loopback socket pair `(read, write)` over `127.0.0.1`: the thread
/// writes `write` to signal readiness, the SDK polls `read`. Connect and accept run blocking (they
/// complete at once on loopback); both ends are then set non-blocking so [`CompletionBridge::drain`]
/// never blocks and the thread never stalls on a full send buffer. Nagle is disabled so a nudge is
/// delivered without the small-packet delay (readability is the whole signal).
#[cfg(windows)]
fn loopback_pair() -> Result<(TcpStream, TcpStream), IpcError> {
  let os = |call: &'static str| {
    move |e: std::io::Error| IpcError::OsRefused {
      call,
      code: e.raw_os_error(),
    }
  };
  let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(os("bind"))?;
  let addr = listener.local_addr().map_err(os("local_addr"))?;
  let write = TcpStream::connect(addr).map_err(os("connect"))?;
  let (read, _peer) = listener.accept().map_err(os("accept"))?;
  read.set_nonblocking(true).map_err(os("set_nonblocking"))?;
  write.set_nonblocking(true).map_err(os("set_nonblocking"))?;
  let _ = read.set_nodelay(true);
  let _ = write.set_nodelay(true);
  Ok((read, write))
}

impl std::fmt::Debug for CompletionBridge {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("CompletionBridge")
      .field("running", &self.handle.is_some())
      .finish()
  }
}

impl Drop for CompletionBridge {
  fn drop(&mut self) {
    self.stop();
  }
}
