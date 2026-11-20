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
//! client (§4.7 "Wake strategy"), and after every return signals its end while the client is armed and a
//! reply waits (a level; `crate::park`).
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
//!   another process. The level is read exactly as on macOS: the `client_parked` flag and the completion
//!   ring's depth.
//!
//! The daemon bumps the wake word on every reply and raises the wake only when the client is parked
//! (`client_parked != 0`, read after the park protocol's fence; `crate::park`), bumping the word once
//! more first. The async SDK sets that flag when it yields to the event loop (the slow path) and clears it
//! once the reply is taken. So a fast-path reply — taken during the spin, the client never parked — costs
//! the daemon no wake and this bridge no signal, and the async fast path stays free of any event-loop
//! wakeup (§4.7 worked example: the extension "returns the reply without ever touching the event loop").
//! The bridge signals only while the client is armed and a reply waits: a level, judged afresh after
//! every return from its wait. Until 2026-09-29 it signalled on a change of the word instead, and a change
//! seen while the client was not yet armed was used up — a lost wake that hung the async lifecycle on the
//! macOS runner (docs/bugs/2026-09-29-an-async-client-waited-forever-for-a-reply-that-had-landed.md).
//!
//! One thread per async client, owned by the [`crate::endpoint::ClientEnd`] and joined on drop (no
//! fire-and-forget, banned item 9). It is the `DoorbellThread` pattern from `slates-server`: the
//! thread holds its own second mapping of the region, so a stop wakes it to unpark it; there is no
//! `Arc` (R2). The macOS and Windows threads stop on a channel: the owner holds its sender and drops
//! it, then wakes the thread, which finds its receiver disconnected; the channel is freed with the
//! last of its two ends, so nothing outlives the bridge. Until 2026-11-18 the stop was a
//! `Box::leak`'d `&'static AtomicBool`, as the doorbell's is, and so was the synchronous park's mark
//! below: two blocks every start kept for good, so a client turning its async completion on and off
//! (a host's `AsyncInstance` handing its client back, an SDK's `completionFd()`) grew without bound
//! (`tests/completion_memory.rs`). A thread the OS refuses refuses the bridge's start, typed: before
//! 2026-11-18 the refusal was dropped, and the bridge started with no thread, its descriptor never
//! readable, so every async wait ran to its deadline.
//!
//! **A wake the bridge takes from a parked synchronous call is passed on** (macOS, Windows). The
//! macOS bridge waits on the wake word, and the Windows one on the named Event. A synchronous call on
//! the same client parks there too, and the daemon wakes one waiter per reply. So the kernel could
//! hand the parked call's wake to the bridge, and the call slept to its reply deadline: 16 to 19 of 20
//! parked calls did, on the Mac Studio (2026-11-18). The parked flag says which: a synchronous park
//! names a stamp slot in it (1 or 2), and the async arm has a value of its own
//! ([`crate::region::PARKED_ASYNC`]). A bridge woken while the flag reads a synchronous park
//! ([`crate::park::parks_synchronously`]) wakes once more, and the only waiter left is the parked
//! call. On Linux a synchronous call that waits on its daemon's departure parks on a descriptor (A-456): the eventfd
//! when no bridge runs, and the bridge's pipe while one does, since the bridge is then the eventfd's only reader and
//! writes its pipe after every read. So the Linux bridge takes no wake from a parked call either; the call gives the
//! pipe's level back when it has read it ([`crate::endpoint::ClientEnd::wait_while_daemon_lives`]).

// The wake-word/Event bridges (macOS, Windows) park on the region and stop on a channel; the Linux bridge stops
// through a pipe and waits on the eventfd, so it needs neither.
#[cfg(any(target_os = "macos", windows))]
use std::sync::atomic::Ordering;
#[cfg(any(target_os = "macos", windows))]
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
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

/// How a bridge's thread is started: the OS's own spawn ([`os_spawn`]) in the product; in the test of its refusal, one
/// that refuses, since an OS refuses a thread only under an exhaustion no test arranges alone.
pub(crate) type Spawn = fn(Box<dyn FnOnce() + Send>) -> std::io::Result<JoinHandle<()>>;

/// The OS's spawn of a bridge's thread, named for the bridge.
pub(crate) fn os_spawn(run: Box<dyn FnOnce() + Send>) -> std::io::Result<JoinHandle<()>> {
  std::thread::Builder::new()
    .name("slates-completion".to_owned())
    .spawn(run)
}

/// Starts a bridge's thread through `spawn`; the OS's refusal is refused typed, so no bridge stands without its thread.
fn bridge_thread(spawn: Spawn, run: Box<dyn FnOnce() + Send>) -> Result<JoinHandle<()>, IpcError> {
  spawn(run).map_err(|error| IpcError::OsRefused {
    call: "spawn the completion bridge's thread",
    code: error.raw_os_error(),
  })
}

/// Whether the owner has stopped a wake-word or Event bridge: it drops the stop channel's sender, on which nothing is
/// ever sent, so anything but an empty, connected channel is the stop.
#[cfg(any(target_os = "macos", windows))]
fn stopped(stop: &Receiver<()>) -> bool {
  !matches!(stop.try_recv(), Err(TryRecvError::Empty))
}

/// Whether a synchronous call of the client is parked where the bridge waits, read from the client's parked flag after
/// the bridge's wait returned ([`crate::park::parks_synchronously`]): a wake the bridge took then may be the call's.
#[cfg(any(target_os = "macos", windows))]
fn synchronous_park_waiting(region: &ClientRegion) -> bool {
  region
    .client_parked()
    .is_ok_and(|parked| crate::park::parks_synchronously(parked.load(Ordering::SeqCst)))
}

// ------------------------------------------------------------------- macOS: self-pipe over the word

/// The client-side completion bridge on macOS, whose rendezvous passes no completion fd. Owns the
/// self-pipe the async SDK polls and the thread that feeds it.
#[cfg(target_os = "macos")]
pub struct CompletionBridge {
  /// The self-pipe's read end: the descriptor an async SDK event loop polls.
  read: OwnedFd,
  /// The stop channel's sender, whose receiver the thread holds: dropped on drop to stop it (see the module doc).
  stop: Option<Sender<()>>,
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
    Self::start_with(region, os_spawn)
  }

  /// [`Self::start`] with its thread started through `spawn`: refused typed when the OS refuses the thread.
  pub(crate) fn start_with(
    region: &ClientRegion,
    spawn: Spawn,
  ) -> Result<CompletionBridge, IpcError> {
    // Two independent second mappings of the one shared region: the thread waits on one, the owner
    // wakes the other to stop it. Both see the same physical wake word (the doorbell pattern). Each maps a
    // duplicate of the region's descriptor, since the region has no name to open (GAP-A9-9).
    let waiter = region.open_again()?;
    let waker = region.open_again()?;
    let (read, write) = self_pipe()?;
    // The stop is a channel the two ends own and free: the thread takes the receiver, the owner keeps the sender. No
    // `Arc` (R2), and nothing kept past the bridge.
    let (stop, stopped) = std::sync::mpsc::channel::<()>();
    let handle = bridge_thread(spawn, Box::new(move || run(waiter, write, stopped)))?;
    Ok(CompletionBridge {
      read,
      stop: Some(stop),
      handle: Some(handle),
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

  /// Stops the thread and joins it: drop the stop channel's sender, then bump and wake the word so a parked thread
  /// returns and finds its receiver disconnected.
  fn stop(&mut self) {
    drop(self.stop.take());
    if let Ok(word) = self.waker.wake_word() {
      word.fetch_add(1, Ordering::AcqRel);
      let _ = wake::wake_one(word);
    }
    if let Some(handle) = self.handle.take() {
      let _ = handle.join();
    }
  }
}

/// The macOS bridge thread: park on the wake word; after every return, whether woken or timed out, make
/// the completion pipe readable while the client is armed and a reply waits in its ring
/// (`armed_with_reply_waiting`, a level). A fast-path reply, taken during the spin, finds the client
/// unarmed, so the async fast path never wakes the event loop.
#[cfg(target_os = "macos")]
fn run(region: ClientRegion, write: OwnedFd, stop: Receiver<()>) {
  let Ok(word) = region.wake_word() else {
    return;
  };
  // The value the next wait compares against: it returns at once when the word has moved past it, so a
  // wake raised while this thread is between waits is not missed (the daemon advances the word before
  // every wake; `crate::park`).
  let mut seen = word.load(Ordering::Acquire);
  while !stopped(&stop) {
    let _ = wake::wait(word, seen, Some(POLL_NS));
    if stopped(&stop) {
      break;
    }
    let moved = word.load(Ordering::Acquire);
    // The word moved, so the daemon may have woken this thread with the wake a parked synchronous call waits for: pass
    // it on. The parked call is the only waiter left, and a wake it did not need costs it one look at its ring. A
    // refused wake leaves the call to its deadline's re-check, as a lost wake would.
    if moved != seen && synchronous_park_waiting(&region) {
      let _ = wake::wake_one(word);
    }
    seen = moved;
    if armed_with_reply_waiting(&region) {
      // Non-blocking: a full pipe is already readable, so a dropped nudge only means the SDK's next
      // poll finds the reply, never a lost one — the same best-effort as the Linux eventfd nudge.
      let _ = rustix::io::write(&write, &NUDGE);
    }
  }
}

/// A bridge's level, judged after every return from its wait (`crate::park::reply_waiting_for_armed`):
/// the client is armed — waiting on its event loop — and a reply waits in its completion ring. Before
/// 2026-09-29 the bridges nudged on a change of the wake word instead, and a change seen while the client
/// was not yet armed was used up: a reply whose client armed a moment later never woke its event loop (the
/// async lifecycle hung on the macOS runner). A fast-path reply, taken during the spin, finds the client
/// unarmed and costs no nudge, as before.
#[cfg(any(target_os = "macos", windows))]
fn armed_with_reply_waiting(region: &ClientRegion) -> bool {
  crate::park::reply_waiting_for_armed(
    || {
      region
        .client_parked()
        .is_ok_and(|parked| parked.load(Ordering::Acquire) != 0)
    },
    || {
      region
        .cpl()
        .depth(region.object())
        .is_ok_and(|depth| depth > 0)
    },
  )
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
    Self::start_from_eventfd_with(eventfd, os_spawn)
  }

  /// [`Self::start_from_eventfd`] with its thread started through `spawn`: refused typed when the OS refuses the thread.
  pub(crate) fn start_from_eventfd_with(
    eventfd: OwnedFd,
    spawn: Spawn,
  ) -> Result<CompletionBridge, IpcError> {
    let (read, write) = self_pipe()?;
    let (stop_read, stop_write) = self_pipe()?;
    let handle = bridge_thread(spawn, Box::new(move || run(eventfd, write, stop_read)))?;
    Ok(CompletionBridge {
      read,
      stop_write,
      handle: Some(handle),
    })
  }

  /// The descriptor the async SDK polls (`add_reader` / `uv_poll`) — a pipe read end, adoptable by a
  /// Node stream.
  pub fn completion_fd(&self) -> RawFd {
    self.read.as_raw_fd()
  }

  /// The pipe's read end, borrowed for a synchronous park's own wait (A-456): while the bridge runs it is the eventfd's
  /// only reader and writes this pipe after every read, so a park that waits here meets every nudge, where one that
  /// waited on the eventfd beside the bridge could find its nudge already read.
  pub(crate) fn level(&self) -> std::os::fd::BorrowedFd<'_> {
    std::os::fd::AsFd::as_fd(&self.read)
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
  /// The stop channel's sender, whose receiver the thread holds: dropped on drop to stop it (see the module doc).
  stop: Option<Sender<()>>,
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
    Self::start_with(region, os_spawn)
  }

  /// [`Self::start`] with its thread started through `spawn`: refused typed when the OS refuses the thread.
  pub(crate) fn start_with(
    region: &ClientRegion,
    spawn: Spawn,
  ) -> Result<CompletionBridge, IpcError> {
    let (handoff, len) = region.handoff()?;
    // Two independent second mappings of the one shared region: the thread waits on the Event
    // through one, the owner signals it through the other to stop the thread (the doorbell pattern).
    let waiter = ClientRegion::open(&handoff, len)?;
    let waker = ClientRegion::open(&handoff, len)?;
    let (read, write) = loopback_pair()?;
    // The stop is a channel the two ends own and free: the thread takes the receiver, the owner keeps the sender. No
    // `Arc` (R2), and nothing kept past the bridge.
    let (stop, stopped) = std::sync::mpsc::channel::<()>();
    let handle = bridge_thread(spawn, Box::new(move || run(waiter, write, stopped)))?;
    Ok(CompletionBridge {
      read,
      stop: Some(stop),
      handle: Some(handle),
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

  /// Stops the thread and joins it: drop the stop channel's sender, then signal the named Event so a parked thread
  /// returns and finds its receiver disconnected.
  fn stop(&mut self) {
    drop(self.stop.take());
    let _ = self.waker.wake_signal();
    if let Some(handle) = self.handle.take() {
      let _ = handle.join();
    }
  }
}

/// The Windows bridge thread: park on the region's named Event; after every return, whether signalled or
/// timed out, make the completion socket readable while the client is armed and a reply waits in its ring
/// (`armed_with_reply_waiting`, a level). A fast-path reply, taken during the spin, finds the client
/// unarmed, so the async fast path never wakes the event loop.
#[cfg(windows)]
fn run(region: ClientRegion, mut write: TcpStream, stop: Receiver<()>) {
  // The auto-reset Event holds a signal until a waiter consumes it, so a wake raised while this thread
  // is between waits returns the next `wake_wait` at once.
  while !stopped(&stop) {
    let signaled = region.wake_wait(Some(POLL_NS)).unwrap_or(false);
    if stopped(&stop) {
      break;
    }
    // This thread consumed the Event's one signal, which may have been a parked synchronous call's: pass it on. The
    // parked call is the only waiter left, and a signal it did not need costs it one look at its ring. A refused
    // signal leaves the call to its deadline's re-check, as a lost wake would.
    if signaled && synchronous_park_waiting(&region) {
      let _ = region.wake_signal();
    }
    if armed_with_reply_waiting(&region) {
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

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
  use std::thread::JoinHandle;

  use super::CompletionBridge;
  use crate::error::IpcError;

  /// Format: the errno an OS answers a thread past its limit with (`pthread_create`'s `EAGAIN`).
  const THREAD_LIMIT: rustix::io::Errno = rustix::io::Errno::AGAIN;

  /// A spawn the OS refuses, as it does at its thread limit.
  fn refused(_run: Box<dyn FnOnce() + Send>) -> std::io::Result<JoinHandle<()>> {
    Err(std::io::Error::from_raw_os_error(
      THREAD_LIMIT.raw_os_error(),
    ))
  }

  /// Expect `started` refused typed, naming the spawn and the OS's errno.
  fn expect_refused_typed(started: Result<CompletionBridge, IpcError>) {
    assert!(
      matches!(
        &started,
        Err(IpcError::OsRefused {
          call: "spawn the completion bridge's thread",
          code: Some(code),
        }) if *code == THREAD_LIMIT.raw_os_error()
      ),
      "{started:?}"
    );
  }

  /// CLAUDE.md §2 item 9 (§4.7, D-19): a completion bridge whose thread the OS refuses is refused typed. Do: start the
  /// macOS bridge over a region with a spawn that refuses `EAGAIN`. Expect the start refused `OsRefused` naming the
  /// spawn and the errno. Before 2026-11-18 the refusal was dropped, and the bridge started with no thread: its
  /// descriptor never became readable, and every async wait ran to its deadline.
  #[cfg(target_os = "macos")]
  #[test]
  fn a_bridge_whose_thread_the_os_refuses_is_refused_typed() {
    use crate::region::{ClientRegion, RegionGeometry};
    let geometry = RegionGeometry {
      slots: 8,
      spin_ns: 200_000,
      spin_shift: 4,
      bulk_bytes: 4096,
      page: 4096,
    };
    let region = ClientRegion::create(
      &format!("slates-completion-refused-{}", std::process::id()),
      7,
      0,
      geometry,
    )
    .unwrap();
    expect_refused_typed(CompletionBridge::start_with(&region, refused));
  }

  /// CLAUDE.md §2 item 9 (§4.7, D-19): the Linux bridge over the completion eventfd, whose thread the OS refuses, is
  /// refused typed. Do: start it over an eventfd with a spawn that refuses `EAGAIN`. Expect the start refused
  /// `OsRefused` naming the spawn and the errno, where the bridge once started with no thread.
  #[cfg(target_os = "linux")]
  #[test]
  fn a_bridge_whose_thread_the_os_refuses_is_refused_typed() {
    use rustix::event::{EventfdFlags, eventfd};
    let completion = eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK).unwrap();
    expect_refused_typed(CompletionBridge::start_from_eventfd_with(
      completion, refused,
    ));
  }
}
