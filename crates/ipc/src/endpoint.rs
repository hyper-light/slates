//! The two ends of one client's rings (§4.7 "Protocol", "Wake strategy"): the client end
//! writes requests and waits for replies, spinning for its wake estimate before parking on the
//! wake word; the daemon end drains requests and writes replies, waking a parked client. The
//! estimate starts at the window the daemon published (its boot probe's mean wake) and learns
//! from each park a reply ended: the daemon stamps the host clock just before it wakes a parked
//! client, and the client times the stamp to its own return (§4.3's online estimate, the same
//! rule as the shard's). Each end owns its sequence cursors; the slots' sequence words carry the
//! protocol, so neither end touches a shared counter on the hot path.
//!
//! **A reply is waited for while its daemon lives, and the wait ends at the daemon's departure (§4.7 "Failure matrix";
//! A-232, A-456).** [`ClientEnd::wait_while_daemon_lives`] parks on the reply and on the daemon's going together where
//! the platform can wait on both. On Linux that is a `poll` of the completion eventfd beside the control socket, whose
//! peer end closes as the daemon goes, its process ending or the daemon stopping inside it. On Windows it is
//! `WaitForMultipleObjects` on the reply's Event and the daemon's process handle, which ends the wait at the process's
//! exit. macOS wakes a park through the wake word (`os_sync_wait_on_address`), which no wait takes beside a descriptor
//! or a process. So on macOS, and on Windows for a daemon restarted inside its own process, the park asks the daemon's
//! liveness (`Liveness::daemon_gone`: its exit watch, and the start stamp in the instance's bootstrap object, which such
//! a restart changes and nothing else) on A-443's doubling: the first look once the wait has lasted twice the longer of
//! the time it had waited when it first parked and the client's wake estimate, each later one at twice the time waited
//! (`crate::looks`). The looks cost the logarithm of the wait, counted in wakes, and such a departure is seen within the time
//! already waited. Until A-456 every wait re-armed at the client's reply deadline, the anchor's liveness budget (a
//! second), and asked the daemon's liveness there, so a call whose daemon died waited up to that second to learn it.
//! [`ClientEnd::wait`] keeps its caller's deadline and asks nothing of the daemon.

#[cfg(target_os = "linux")]
use std::os::fd::AsFd;
#[cfg(unix)]
use std::os::fd::{AsRawFd, IntoRawFd, OwnedFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::RawSocket;
use std::sync::atomic::Ordering;
use std::time::Instant;

use slates_machine::wake::WakeEstimate;

#[cfg(any(target_os = "macos", windows))]
use crate::looks::Looks;

use crate::error::IpcError;
use crate::region::{
  ClientRegion, PARKED_ASYNC, PARKED_STAMP_SLOT_ONE, PARKED_STAMP_SLOT_ZERO, REPLY_STAMP_CONFIRMED,
  REPLY_STAMP_SLOTS,
};
use crate::slot::{Slot, SlotKind};
// The word-based wake serves Linux/macOS; Windows waits on and signals the region's named Event
// instead (`ClientRegion::wake_wait`/`wake_signal`), so this import is dead there.
#[cfg(not(windows))]
use crate::wake;

/// Format: the eight bytes a completion signal writes (an eventfd counter increment on Linux; a byte
/// stream elsewhere reads them as one nudge). The value is not read — the fd's readability is the
/// signal — so any nonzero eight bytes serve; `1` matches the eventfd add-one convention.
#[cfg(unix)]
const COMPLETION_NUDGE: [u8; 8] = 1u64.to_ne_bytes();

/// A request as the daemon end hands it to the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
  /// The request id word.
  pub request: u64,
  /// The kind.
  pub kind: SlotKind,
  /// The payload.
  pub payload: Vec<u8>,
}

/// A reply as the client end returns it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
  /// The request id word it answers.
  pub request: u64,
  /// The kind.
  pub kind: SlotKind,
  /// The payload.
  pub payload: Vec<u8>,
  /// The daemon's stamp as it wrote the reply into the ring (`crate::slot`, "The stamp"; A-559): zero for a reply no
  /// daemon stamped. [`crate::slot::landed_ns`] widens it to when the reply landed.
  pub stamp: u32,
}

/// What a wait for the next reply while the daemon lives ended with ([`ClientEnd::wait_while_daemon_lives`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Waited {
  /// The next reply.
  Reply(Reply),
  /// The daemon went with no reply left in the ring: its process ended, or it stopped and another started inside the
  /// same process (Linux: its control socket closed; macOS and Windows: a new start stamp). The caller reconnects and
  /// sends again under the same id, as it does for a daemon found gone (§4.7 "Failure matrix"). A reply the daemon
  /// wrote before it went is taken first, as [`Waited::Reply`].
  Departed,
}

/// The client's end.
pub struct ClientEnd {
  region: ClientRegion,
  doorbell: Option<crate::rendezvous::Doorbell>,
  liveness: Option<crate::rendezvous::Liveness>,
  /// A spin window the caller chose over the daemon's published one (a caller that wants
  /// the reply without a wake spins for its own latency floor).
  spin_override_ns: Option<u64>,
  next_request: u64,
  next_reply: u64,
  /// The stamp this end wrote into the last request it pushed (`crate::slot`, "The stamp"; A-559): when that request
  /// landed in the command ring, for a caller that splits its latency ([`Self::request_stamp`]); zero before any.
  request_stamp: u32,
  /// Wakes the client had to park for (the spin-to-park ratio's numerator).
  parks: u64,
  /// Replies taken.
  replies: u64,
  /// Parks a wake ended with no reply in the ring — a spurious return from the OS wait, or a previous
  /// reply's wake that landed after its reply was taken and found this park — so the client parked
  /// again. Every park ends in a reply, one of these, the deadline, a look, or the daemon's departure.
  unanswered_wakes: u64,
  /// Parks a departure wait's look ended (macOS, Windows; A-456, A-506): the park's timeout was the next look at the
  /// daemon's life, which came before any wake, and the look found the daemon alive, so the client parked again. Neither
  /// a reply's wake nor an unanswered one, and its wait teaches the estimate nothing; zero on Linux, whose park waits on
  /// the departure itself.
  parks_ended_by_looks: u64,
  /// The client's online wake estimate, its spin window: seeded with the window the daemon published
  /// and fed the wake of each park a reply ended asleep, from the daemon's reply stamp to the client running
  /// again ([`Self::wait`], [`wait_may_teach`]).
  wake: WakeEstimate,
  /// Parks whose wake fed the estimate (a non-vacuity count).
  wake_samples: u64,
  /// Parks a reply ended, by the reply stamp slot each named, whose stamp could not be judged when the client read it
  /// (a waker's confirmation not yet landed): settled at a later park's arming, and given up when the slot is armed
  /// again (`region::REPLY_STAMP_SLOTS`).
  unsettled: [Option<Unsettled>; REPLY_STAMP_SLOTS],
  /// Looks made at the daemon's life by a departure wait (macOS's and Windows' doubling, A-456): a non-vacuity count,
  /// zero on Linux, whose park waits on every departure itself.
  liveness_looks: u64,
  /// The current departure wait's look schedule (macOS, Windows; A-456; reset as each wait begins).
  #[cfg(any(target_os = "macos", windows))]
  looks: Looks,
  /// Whether a departure park read the running completion bridge's pipe (Linux, A-456): the level the async loop
  /// polls, which the wait gives back as it ends ([`Self::give_back_bridge_level`]).
  #[cfg(target_os = "linux")]
  bridge_level_owed: bool,
  /// The reply stamp slot the next park names: the slots alternate, so a confirmation that lands after
  /// the next park armed finds its own slot intact.
  next_stamp_slot: usize,
  /// The completion fd an async SDK event loop polls for reply-readiness (§4.7, D-19): the daemon
  /// makes it readable when it writes a reply to a parked client, so an `asyncio`/`uv_poll` loop wakes
  /// without spinning. `None` for the sync client (it parks on the wake word) and where the platform
  /// has no completion channel yet. The fd is read-drained and the reply taken with [`Self::try_take`].
  #[cfg(unix)]
  completion: Option<OwnedFd>,
  /// The completion bridge (a self-pipe fed by a thread), started on demand for an SDK that adopts the
  /// completion fd into a stream; `None` for a sync client and until the first such async use. On
  /// macOS the thread parks on the wake word (macOS passes no shared completion fd, D-10) and both the
  /// plain and dup async paths use it. On Linux it polls the rendezvous eventfd (`self.completion`):
  /// Python's asyncio polls that eventfd directly and needs no bridge, so only the dup path (Node,
  /// whose `net.Socket` refuses an eventfd) starts one, to convert the eventfd to a pollable pipe.
  #[cfg(any(target_os = "macos", target_os = "linux"))]
  bridge: Option<crate::completion::CompletionBridge>,
  /// The Windows completion bridge (a loopback socket fed by a thread parking on the region's named
  /// Event), started on demand by [`Self::enable_async_completion`]; `None` for a sync client and
  /// until the first async use. Windows passes no shared completion fd (Mach and named sockets are
  /// refused, D-10), so the async SDK polls this bridge's socket, exactly as macOS polls its pipe.
  #[cfg(windows)]
  bridge: Option<crate::completion::CompletionBridge>,
}

impl std::fmt::Debug for ClientEnd {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ClientEnd")
      .field("region", &self.region)
      .field("parks", &self.parks)
      .finish()
  }
}

impl ClientEnd {
  /// The client's end over an opened region.
  pub fn new(region: ClientRegion) -> ClientEnd {
    let wake = WakeEstimate::new(u64::from(region.spin_ns()), region.spin_shift());
    ClientEnd {
      wake,
      wake_samples: 0,
      unsettled: [None; REPLY_STAMP_SLOTS],
      liveness_looks: 0,
      #[cfg(any(target_os = "macos", windows))]
      looks: Looks::default(),
      #[cfg(target_os = "linux")]
      bridge_level_owed: false,
      next_stamp_slot: 0,
      region,
      doorbell: None,
      liveness: None,
      spin_override_ns: None,
      next_request: 0,
      next_reply: 0,
      request_stamp: 0,
      parks: 0,
      replies: 0,
      unanswered_wakes: 0,
      parks_ended_by_looks: 0,
      #[cfg(unix)]
      completion: None,
      #[cfg(any(target_os = "macos", target_os = "linux"))]
      bridge: None,
      #[cfg(windows)]
      bridge: None,
    }
  }

  /// The client's end over everything the rendezvous handed over: the region, the doorbell,
  /// the liveness check, and (where the platform has one) the completion fd an async SDK polls.
  /// These resources must move together: on Linux the liveness handle owns the control socket;
  /// dropping it while retaining the ring tells the daemon to reap a client that is still in use.
  // `connected` is mutated only to take the completion fd, which is a Unix concern; on Windows it is
  // read-only, so the `mut` is dead there.
  #[cfg_attr(windows, allow(unused_mut))]
  pub fn connected(mut connected: crate::rendezvous::Connected) -> ClientEnd {
    #[cfg(unix)]
    let completion = connected.take_completion();
    let mut end = ClientEnd::new(connected.region);
    end.doorbell = Some(connected.doorbell);
    end.liveness = Some(connected.liveness);
    #[cfg(unix)]
    {
      end.completion = completion;
    }
    end
  }

  /// Sets the completion fd (the rendezvous's, or a paired fd a test injects). The daemon's end makes
  /// it readable on a reply to a parked client; an async SDK polls it.
  #[cfg(unix)]
  pub fn set_completion(&mut self, completion: OwnedFd) {
    self.completion = Some(completion);
  }

  /// The completion fd for an async SDK event loop to poll (`asyncio.add_reader` / `uv_poll`, D-19),
  /// or `None` for a sync client or a platform without one. The loop waits for it to become readable,
  /// drains it ([`Self::drain_completion`]), then takes the reply with [`Self::try_take`].
  #[cfg(unix)]
  pub fn completion_fd(&self) -> Option<RawFd> {
    // Where a bridge was started (macOS always, Linux only for the fd-adopting dup path), it is the
    // descriptor the loop polls; otherwise it is the raw completion fd (Linux's rendezvous eventfd).
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if let Some(bridge) = &self.bridge {
      return Some(bridge.completion_fd());
    }
    self.completion.as_ref().map(AsRawFd::as_raw_fd)
  }

  /// Clears the completion fd's readiness bytes after the event loop reports it readable, so the next
  /// wait blocks again rather than seeing a stale nudge. Best-effort — a non-blocking read that takes
  /// the buffered bytes (an eventfd resets its counter; a socket drains its nudges); an empty fd is a
  /// no-op.
  #[cfg(unix)]
  pub fn drain_completion(&self) {
    // A started bridge owns the descriptor the loop polls, so drain its pipe; otherwise drain the raw
    // completion fd (Linux's eventfd, whose read resets its counter).
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if let Some(bridge) = &self.bridge {
      bridge.drain();
      return;
    }
    if let Some(fd) = &self.completion {
      let mut scratch = [0u8; 64];
      let _ = rustix::io::read(fd, &mut scratch);
    }
  }

  /// Enables the async completion channel and returns the descriptor an event loop polls
  /// (`add_reader` / `uv_poll`, D-19). Idempotent — the same descriptor each call. On Linux this is
  /// the eventfd the rendezvous passed (the daemon writes it directly); on macOS it starts the
  /// completion bridge (a self-pipe fed by a thread parking on the wake word) on first use. The SDK
  /// registers the descriptor once, arms the parked flag with [`Self::arm_async`] before it yields,
  /// re-checks [`Self::try_take`] to close the race, and drains the fd with [`Self::drain_completion`]
  /// when the loop reports it readable.
  #[cfg(unix)]
  pub fn enable_async_completion(&mut self) -> Result<RawFd, IpcError> {
    #[cfg(target_os = "macos")]
    {
      if let Some(bridge) = &self.bridge {
        return Ok(bridge.completion_fd());
      }
      let bridge = crate::completion::CompletionBridge::start(&self.region)?;
      let fd = bridge.completion_fd();
      self.bridge = Some(bridge);
      Ok(fd)
    }
    #[cfg(not(target_os = "macos"))]
    {
      self.completion_fd().ok_or(IpcError::Unsupported {
        feature: "async completion fd (the rendezvous passed no completion descriptor)",
      })
    }
  }

  /// Like [`Self::enable_async_completion`] but returns a **dup** the caller owns and must close — for
  /// an SDK whose event loop adopts and closes the descriptor it polls (Node's `net.Socket`; `asyncio`
  /// only polls it, so it uses `enable_async_completion`). The dup and the client's own descriptor
  /// refer to the same pipe, so closing the dup leaves the client's intact — no double close.
  ///
  /// On Linux this is also where the fd-adopting SDK diverges from the direct one: the rendezvous
  /// eventfd is not adoptable as a Node stream (`net.Socket` refuses an eventfd, `ERR_INVALID_FD_TYPE`),
  /// so this starts a [`crate::completion::CompletionBridge`] that converts the eventfd's readiness to
  /// a pollable self-pipe and returns a dup of the pipe. The plain [`Self::enable_async_completion`]
  /// still hands Python the raw eventfd, which its `asyncio` selector polls directly with no bridge.
  #[cfg(unix)]
  pub fn enable_async_completion_dup(&mut self) -> Result<RawFd, IpcError> {
    // Ensure the channel exists (starts the macOS bridge; Linux already holds the eventfd).
    self.enable_async_completion()?;
    #[cfg(target_os = "linux")]
    {
      // Start the pipe bridge over a dup of the eventfd (the client keeps its own copy), then hand the
      // caller a dup of the bridge's pipe read end — the descriptor Node's `net.Socket` can adopt.
      if self.bridge.is_none() {
        let eventfd = self.completion.as_ref().ok_or(IpcError::Unsupported {
          feature: "async completion fd (the rendezvous passed no completion descriptor)",
        })?;
        let eventfd_dup = rustix::io::dup(eventfd).map_err(|error| IpcError::OsRefused {
          call: "dup",
          code: Some(error.raw_os_error()),
        })?;
        self.bridge = Some(crate::completion::CompletionBridge::start_from_eventfd(
          eventfd_dup,
        )?);
      }
      match &self.bridge {
        Some(bridge) => Ok(bridge.dup_fd()?.into_raw_fd()),
        None => Err(IpcError::Unsupported {
          feature: "async completion fd (the completion bridge did not start)",
        }),
      }
    }
    #[cfg(target_os = "macos")]
    {
      // The plain path already started the bridge; a dup of its pipe is the caller-owned descriptor.
      match &self.bridge {
        Some(bridge) => Ok(bridge.dup_fd()?.into_raw_fd()),
        None => Err(IpcError::Unsupported {
          feature: "async completion fd (the completion bridge did not start)",
        }),
      }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    match &self.completion {
      Some(fd) => Ok(
        rustix::io::dup(fd)
          .map_err(|error| IpcError::OsRefused {
            call: "dup",
            code: Some(error.raw_os_error()),
          })?
          .into_raw_fd(),
      ),
      None => Err(IpcError::Unsupported {
        feature: "async completion fd (the rendezvous passed no completion descriptor)",
      }),
    }
  }

  /// The completion socket for an async SDK event loop to poll (`uv_poll` / a Python selector, D-19),
  /// or `None` for a sync client or until [`Self::enable_async_completion`] starts the bridge. The
  /// loop waits for it to become readable, drains it ([`Self::drain_completion`]), then takes the
  /// reply with [`Self::try_take`]. The Windows analogue of `Self::completion_fd`, which is the Unix arm and so never exists where this one does.
  #[cfg(windows)]
  pub fn completion_socket(&self) -> Option<RawSocket> {
    self
      .bridge
      .as_ref()
      .map(crate::completion::CompletionBridge::completion_socket)
  }

  /// Clears the completion socket's readiness after the event loop reports it readable, so the next
  /// wait blocks again rather than seeing a stale nudge. Best-effort — a non-blocking read of the
  /// buffered nudges; a no-op before the bridge starts or when the socket is already drained.
  #[cfg(windows)]
  pub fn drain_completion(&self) {
    if let Some(bridge) = &self.bridge {
      bridge.drain();
    }
  }

  /// Enables the async completion channel and returns the socket an event loop polls (`uv_poll` / a
  /// selector, D-19). Idempotent — the same socket each call. Windows passes no shared completion fd
  /// (Mach and named sockets are refused, D-10), so this starts the completion bridge (a loopback
  /// socket fed by a thread parking on the region's named Event) on first use, exactly as macOS
  /// starts its self-pipe bridge. The SDK registers the socket once, arms the parked flag with
  /// [`Self::arm_async`] before it yields, re-checks [`Self::try_take`] to close the race, and drains
  /// the socket with [`Self::drain_completion`] when the loop reports it readable.
  #[cfg(windows)]
  pub fn enable_async_completion(&mut self) -> Result<RawSocket, IpcError> {
    if let Some(bridge) = &self.bridge {
      return Ok(bridge.completion_socket());
    }
    let bridge = crate::completion::CompletionBridge::start(&self.region)?;
    let socket = bridge.completion_socket();
    self.bridge = Some(bridge);
    Ok(socket)
  }

  /// Like [`Self::enable_async_completion`] but returns a **dup** the caller owns and must close — for
  /// an SDK whose event loop closes the socket it polls (Node's `net.Socket` adopts and closes it;
  /// asyncio only polls it, so it uses `enable_async_completion`). The dup and the client's own socket
  /// refer to the same loopback endpoint, so closing the dup leaves the client's intact.
  #[cfg(windows)]
  pub fn enable_async_completion_dup(&mut self) -> Result<RawSocket, IpcError> {
    self.enable_async_completion()?;
    match &self.bridge {
      Some(bridge) => bridge.dup_socket(),
      None => Err(IpcError::Unsupported {
        feature: "async completion socket (the bridge did not start)",
      }),
    }
  }

  /// Arms the completion signal before the SDK yields to its event loop: the daemon wakes a parked
  /// client on a reply, making the completion fd readable, and the bridges nudge their pipe or socket
  /// only while armed. This is the same `parked` flag the sync [`Self::wait`] sets, with a value of its own
  /// ([`PARKED_ASYNC`]), so a running bridge tells a synchronous park, whose wake it passes on, from the
  /// arm; the async path manages it explicitly because it never calls `wait`. A caller re-checks
  /// [`Self::try_take`] right after arming to close the race with a reply that landed during the spin; the
  /// park protocol's fence ([`crate::park::after_arming`]) orders that re-check after the flag, so a reply
  /// it misses is one the daemon sees the flag for and wakes on (before 2026-09-29 there was no fence, and
  /// both could miss).
  pub fn arm_async(&self) -> Result<(), IpcError> {
    // The async path times no wake, so the stamp slot the daemon reads its value as (slot 0) is never read.
    self
      .region
      .client_parked()?
      .store(PARKED_ASYNC, Ordering::Release);
    crate::park::after_arming();
    Ok(())
  }

  /// Clears the arm ([`Self::arm_async`]) once a reply is taken, so a later fast-path reply costs the
  /// daemon no wake and the bridge no pipe write.
  pub fn disarm_async(&self) -> Result<(), IpcError> {
    self.region.client_parked()?.store(0, Ordering::Release);
    Ok(())
  }

  /// Ends the async completion channel ([`Self::enable_async_completion`]) for a client going back to the synchronous
  /// form: the arm cleared and the bridge stopped and joined, its descriptor closed. Where the bridge waits on what the
  /// synchronous park waits on (the wake word on macOS, the named Event on Windows), the daemon's one wake per reply
  /// went to whichever waiter the kernel picked, so a parked synchronous call beside a running bridge slept to its
  /// whole reply deadline: 16 to 19 of 20 parked calls did, each up to 2.0 s, on the Mac Studio (2026-11-18). On Linux
  /// the plain path holds no bridge and the eventfd stays the client's. A later [`Self::enable_async_completion`]
  /// starts a new bridge.
  pub fn disable_async_completion(&mut self) -> Result<(), IpcError> {
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    {
      self.bridge = None;
    }
    self.disarm_async()
  }

  /// Whether the daemon this end was connected to is gone (dead or restarted); true for an
  /// end that has no liveness check, so a caller reconnects rather than waits forever.
  pub fn daemon_gone(&self) -> bool {
    self.liveness.as_ref().is_none_or(|l| l.daemon_gone())
  }

  /// Looks made at the daemon's life by [`Self::wait_while_daemon_lives`] where its park cannot wait on every departure
  /// (macOS, Windows; A-456): logarithmic in the time waited; none on Linux, whose park waits on every departure itself.
  pub fn liveness_looks(&self) -> u64 {
    self.liveness_looks
  }

  /// Gives this end a liveness check, as [`Self::connected`] does: a test's stand-in for the daemon's life.
  #[cfg(test)]
  pub(crate) fn set_liveness(&mut self, liveness: crate::rendezvous::Liveness) {
    self.liveness = Some(liveness);
  }

  /// A descriptor that reports the daemon's departure when polled for `POLLRDHUP` (`Liveness::watch`, Linux), or `None`
  /// for an end with no liveness check.
  #[cfg(target_os = "linux")]
  #[must_use]
  pub fn daemon_watch(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
    self
      .liveness
      .as_ref()
      .map(crate::rendezvous::Liveness::watch)
  }

  /// Receives a descriptor the daemon sent over the control socket (§4.6: a content map, A-135; a stream end,
  /// A-161), or `None` if none is waiting (or this end has no control socket). Non-blocking; the descriptor arrives
  /// by `SCM_RIGHTS` with what it is for and the request it answers.
  #[cfg(unix)]
  pub fn recv_delivery(
    &self,
  ) -> Result<Option<crate::rendezvous::ControlDelivery>, crate::error::IpcError> {
    match self.liveness.as_ref() {
      Some(liveness) => liveness.recv_delivery(),
      None => Ok(None),
    }
  }

  /// Waits, with no clock, until a delivery may be read or the daemon has gone (Linux, A-523;
  /// `Liveness::wait_delivery`). macOS has no control socket, so no delivery ever comes over one there: refused
  /// `Unsupported`. An end with no liveness check has no daemon to wait on, and returns at once.
  #[cfg(unix)]
  pub fn wait_delivery(&self) -> Result<(), crate::error::IpcError> {
    match self.liveness.as_ref() {
      #[cfg(target_os = "linux")]
      Some(liveness) => liveness.wait_delivery(),
      #[cfg(not(target_os = "linux"))]
      Some(_) => Err(crate::error::IpcError::Unsupported {
        feature: "a delivery over a control socket",
      }),
      None => Ok(()),
    }
  }

  /// The region.
  pub fn region(&self) -> &ClientRegion {
    &self.region
  }

  /// The region, mutably (the bulk area).
  pub fn region_mut(&mut self) -> &mut ClientRegion {
    &mut self.region
  }

  /// Chooses the spin window (`None`: the client's wake estimate, seeded with the daemon's published
  /// window and refined from the client's own parks).
  pub fn set_spin_ns(&mut self, spin_ns: Option<u64>) {
    self.spin_override_ns = spin_ns;
  }

  /// The client's online wake estimate now, nanoseconds: its spin window unless one was chosen.
  pub fn wake_estimate_ns(&self) -> u64 {
    self.wake.mean_ns()
  }

  /// The stamp this end wrote into the last request it pushed: when that request landed in the command ring
  /// (`crate::slot`, "The stamp"; A-559), widened by [`crate::slot::landed_ns`]; zero before any request.
  pub fn request_stamp(&self) -> u32 {
    self.request_stamp
  }

  /// Parks whose measured wake fed the estimate.
  pub fn wake_samples(&self) -> u64 {
    self.wake_samples
  }

  /// Parks so far and replies so far (the measured spin-to-park ratio).
  pub fn park_ratio(&self) -> (u64, u64) {
    (self.parks, self.replies)
  }

  /// Parks a wake ended with no reply waiting, so the client parked again (see the field).
  pub fn unanswered_wakes(&self) -> u64 {
    self.unanswered_wakes
  }

  /// Parks a departure wait's look ended, the daemon found alive and the client parked again (macOS, Windows; A-506; see
  /// the field). With the replies and [`Self::unanswered_wakes`] they account for every park a wait made that neither a
  /// deadline nor the daemon's departure ended.
  pub fn parks_ended_by_looks(&self) -> u64 {
    self.parks_ended_by_looks
  }

  /// The ring index the next request takes (its bulk chunk is chosen by it).
  pub fn next_request_index(&self) -> u64 {
    self.next_request
  }

  /// Sends `message` as request `request`: packed for the next ring position (a body past a slot's inline payload goes
  /// to that position's bulk chunk) and pushed; `RingFull` when the daemon has not taken the slot the ring wraps onto.
  /// Room is checked before anything is packed: the position's bulk chunk is the oldest unread request's until the
  /// daemon takes it, and a body packed there before a push refused `RingFull` was written over that request's
  /// (`a_request_refused_ring_full_never_writes_over_an_unread_body`). The only way a request is packed.
  pub fn send_message<M: slates_wire::Wire>(
    &mut self,
    request: u64,
    message: &M,
  ) -> Result<(), IpcError> {
    let index = self.next_request;
    if !self.region.cmd().can_push(self.region.object(), index)? {
      return Err(IpcError::RingFull);
    }
    let slot = crate::protocol::pack(
      &mut self.region,
      crate::protocol::Direction::Request,
      index,
      request,
      message,
    )?;
    self.send(&slot)
  }

  /// Writes a request slot; `RingFull` when the daemon has not taken the slot the ring wraps
  /// onto (the caller blocks on credit and retries; nothing is dropped). Rings the doorbell for this region's shard
  /// when that shard has announced it is going idle — read after the doorbell protocol's fence ([`crate::doorbell`]), so
  /// a shard that missed this request on its re-check is always rung, and only that shard is woken (A-381).
  pub fn send(&mut self, slot: &Slot) -> Result<(), IpcError> {
    let cmd = self.region.cmd();
    let stamp = crate::slot::stamp_now();
    cmd.push(self.region.object_mut(), self.next_request, slot, stamp)?;
    self.request_stamp = stamp;
    self.next_request = self.next_request.wrapping_add(1);
    let announced = self.region.daemon_parked()?;
    let parked =
      crate::doorbell::after_publishing_idle_announced(|| announced.load(Ordering::Relaxed) != 0);
    if parked {
      self.region.doorbell()?.fetch_add(1, Ordering::AcqRel);
      if let Some(bell) = &self.doorbell {
        bell.ring(self.region.shard())?;
      }
    }
    Ok(())
  }

  /// Takes the next reply if one is there, its body owned: a bulk body is copied out before the slot is released, so
  /// a daemon replying on into the freed slot's chunk never changes it ([`crate::protocol::owned`]).
  pub fn try_take(&mut self) -> Result<Option<Reply>, IpcError> {
    let cpl = self.region.cpl();
    let Some(peeked) = cpl.peek(self.region.object(), self.next_reply)? else {
      return Ok(None);
    };
    let owned = peeked.and_then(|slot| crate::protocol::owned(&self.region, slot));
    cpl.release(self.region.object(), self.next_reply)?;
    let slot = owned?;
    self.next_reply = self.next_reply.wrapping_add(1);
    self.replies = self.replies.saturating_add(1);
    Ok(Some(Reply {
      request: slot.request,
      kind: slot.kind,
      payload: slot.payload,
      stamp: slot.stamp,
    }))
  }

  /// Waits for the next reply: spins for the wake estimate, then parks on the wake word
  /// (setting the parked flag and re-checking the slot to close the race), until the reply
  /// arrives or `deadline_ns` (from the call) passes. A park a reply ended asleep teaches the
  /// estimate its wake ([`wait_may_teach`]). It asks nothing of the daemon's life: a caller that
  /// waits while the daemon lives uses [`Self::wait_while_daemon_lives`].
  pub fn wait(&mut self, deadline_ns: Option<u64>) -> Result<Reply, IpcError> {
    match self.wait_ending(Ends::At(deadline_ns))? {
      Waited::Reply(reply) => Ok(reply),
      // A deadline's parks never wait on the daemon, so none of them reports its departure.
      Waited::Departed => Err(IpcError::DeadlineExceeded),
    }
  }

  /// Waits for the next reply while the daemon lives, with no deadline (§4.7 "Failure matrix"; A-232, A-456): spins
  /// for the wake estimate, then parks on the reply and the daemon's departure together (the module doc), until the
  /// reply arrives ([`Waited::Reply`]) or the daemon's process ends with none in the ring ([`Waited::Departed`]). A
  /// daemon that lives but never answers is ended by its supervisor, or by its own watchdog when it runs alone (A-232),
  /// and the wait ends there. An end with no liveness check has no daemon to wait on: its wait ends `Departed` once the
  /// ring holds nothing, as [`Self::daemon_gone`] reads it gone. On Linux an end with a liveness check and no
  /// completion descriptor is refused `Unsupported` (every rendezvous hands one over).
  pub fn wait_while_daemon_lives(&mut self) -> Result<Waited, IpcError> {
    #[cfg(any(target_os = "macos", windows))]
    {
      self.looks = Looks::default();
    }
    let waited = self.wait_ending(Ends::Departure);
    #[cfg(target_os = "linux")]
    self.give_back_bridge_level();
    waited
  }

  /// The wait both forms share: the spin, then one park at a time until the reply, the end `ends` names, or a refusal.
  fn wait_ending(&mut self, ends: Ends) -> Result<Waited, IpcError> {
    let started = Instant::now();
    let spin_ns = self.spin_override_ns.unwrap_or_else(|| self.wake.mean_ns());
    loop {
      if let Some(reply) = self.try_take()? {
        return Ok(Waited::Reply(reply));
      }
      let elapsed = elapsed_ns(started);
      if elapsed < spin_ns {
        std::hint::spin_loop();
        continue;
      }
      let look_timeout_ns = match self.before_park(ends, elapsed)? {
        Before::Park(timeout_ns) => timeout_ns,
        Before::Ended(waited) => return Ok(waited),
      };
      if let Some(waited) = self.park_once(ends, started, look_timeout_ns)? {
        return Ok(waited);
      }
    }
  }

  /// What comes before a park: the deadline's end ([`Ends::At`]), or the departure's look where the park cannot wait on
  /// the departure itself ([`Self::departure_due`]).
  fn before_park(&mut self, ends: Ends, elapsed_ns: u64) -> Result<Before, IpcError> {
    match ends {
      Ends::At(Some(deadline_ns)) if elapsed_ns >= deadline_ns => Err(IpcError::DeadlineExceeded),
      Ends::At(_) => Ok(Before::Park(None)),
      Ends::Departure => self.departure_due(elapsed_ns),
    }
  }

  /// One park: armed, the reply looked for again, then the platform's wait for the end `ends` names: what is left of a
  /// deadline; for a departure, until its next look (`look_timeout_ns`, macOS) or for good (Linux, Windows). The reply,
  /// the end, or `None` to wait on.
  fn park_once(
    &mut self,
    ends: Ends,
    started: Instant,
    look_timeout_ns: Option<u64>,
  ) -> Result<Option<Waited>, IpcError> {
    let (slot, expected) = self.arm_park()?;
    if let Some(reply) = self.try_take()? {
      self.region.client_parked()?.store(0, Ordering::Release);
      return Ok(Some(Waited::Reply(reply)));
    }
    self.parks = self.parks.saturating_add(1);
    let switches_before = slates_machine::switches::switches_now();
    let waiting_ns = slates_machine::clock::monotonic_ns();
    let parked = match ends {
      Ends::At(deadline_ns) => {
        let remaining = deadline_ns.map(|deadline| deadline.saturating_sub(elapsed_ns(started)));
        self.park_until(expected, remaining)?
      }
      Ends::Departure => self.park_until_departure(expected, look_timeout_ns)?,
    };
    let returned_ns = slates_machine::clock::monotonic_ns();
    let teaches = wait_may_teach(
      parked.waker_confirms,
      switches_before,
      slates_machine::switches::switches_now(),
    );
    self.region.client_parked()?.store(0, Ordering::Release);
    let woke = teaches.then_some(Unsettled {
      waiting_ns,
      returned_ns,
      waker_confirms: parked.waker_confirms,
    });
    self.after_park(ends, slot, parked, woke, look_timeout_ns.is_some())
  }

  /// Arms a park (§4.7; `crate::park`): the flag first, then the wake word's value, both before the caller's re-check
  /// that closes the race with a reply written between the last look and the wait. A word's wait (Linux, macOS)
  /// returns at once if the word moved past the value read here; Windows' named auto-reset Event and a Linux departure
  /// park's descriptor keep a signal made before the wait, so the value goes unused there. The park's reply stamp slot is
  /// cleared before the flag names it (the flag's release orders it), so the stamp read after the wake is the first one
  /// the daemon took for this park. Earlier parks' unsettled stamps are settled first; the one in this slot is given up,
  /// as one daemon serves the ring in order and its reply call returned before the reply this client took since. The
  /// park's stamp slot and the word's value.
  fn arm_park(&mut self) -> Result<(usize, u32), IpcError> {
    let slot = self.next_stamp_slot;
    self.next_stamp_slot = usize::from(slot == 0);
    self.settle_wakes()?;
    if let Some(given_up) = self.unsettled.get_mut(slot) {
      *given_up = None;
    }
    self.region.reply_stamp(slot)?.store(0, Ordering::Relaxed);
    // A running completion bridge waits where this park does: the flag's value, a stamp slot's and never the async
    // arm's, has it pass on a wake it takes while this park waits (see `crate::completion`).
    self
      .region
      .client_parked()?
      .store(parked_flag(slot), Ordering::Release);
    // The park protocol's client half: the word's value and the re-check after it are ordered after the flag, so a
    // reply the re-check misses is one the daemon sees the flag for (§4.7; `crate::park`).
    crate::park::after_arming();
    let expected = self.region.wake_word()?.load(Ordering::Acquire);
    Ok((slot, expected))
  }

  /// What a park's end means: a wake teaches its wait (`woke`, when [`wait_may_teach`] let it) and the reply is looked
  /// for; a departure ends the wait, a reply written before it taken first; a deadline's park that timed out ends its
  /// wait `DeadlineExceeded`; a departure park that waited until its next look (`timed_for_a_look`) and timed out was
  /// ended by that look, counted as such and teaching nothing (A-506); anything else waits on (`None`).
  fn after_park(
    &mut self,
    ends: Ends,
    slot: usize,
    parked: Parked,
    woke: Option<Unsettled>,
    timed_for_a_look: bool,
  ) -> Result<Option<Waited>, IpcError> {
    if parked.woken
      && let Some(reply) = self.after_wake(slot, woke)?
    {
      return Ok(Some(Waited::Reply(reply)));
    }
    if parked.departed {
      return self.after_departure().map(Some);
    }
    if parked.woken {
      self.unanswered_wakes = self.unanswered_wakes.saturating_add(1);
      return Ok(None);
    }
    match ends {
      Ends::At(Some(_)) => self.after_deadline().map(Some),
      Ends::Departure if timed_for_a_look => {
        self.parks_ended_by_looks = self.parks_ended_by_looks.saturating_add(1);
        Ok(None)
      }
      Ends::At(None) | Ends::Departure => Ok(None),
    }
  }

  /// A park a wake ended: its wait kept for the estimate when it may teach, the stamps settled, then the reply taken.
  fn after_wake(
    &mut self,
    slot: usize,
    woke: Option<Unsettled>,
  ) -> Result<Option<Reply>, IpcError> {
    if let Some(woke) = woke
      && let Some(unsettled) = self.unsettled.get_mut(slot)
    {
      *unsettled = Some(woke);
    }
    self.settle_wakes()?;
    self.try_take()
  }

  /// A deadline's park that timed out: a reply that landed meanwhile is taken, else the deadline has passed.
  fn after_deadline(&mut self) -> Result<Waited, IpcError> {
    match self.try_take()? {
      Some(reply) => Ok(Waited::Reply(reply)),
      None => Err(IpcError::DeadlineExceeded),
    }
  }

  /// The daemon's process ended: a reply it wrote before is taken, else the wait ends `Departed`.
  fn after_departure(&mut self) -> Result<Waited, IpcError> {
    Ok(match self.try_take()? {
      Some(reply) => Waited::Reply(reply),
      None => Waited::Departed,
    })
  }

  /// macOS and Windows: the departure's look, when the doubling makes one due (A-443, A-456; `Looks`). A macOS park waits
  /// on the wake word alone, and a Windows park's process handle does not fire for a daemon restarted inside its
  /// process, so each asks the daemon's liveness on that schedule (`Liveness::daemon_gone`: the exit watch, and the
  /// start stamp such a restart changes). `Park` until the next look, made now when one was due and found the daemon
  /// alive (the park re-checks the ring as it arms); `Ended` once a look finds it gone (a reply it wrote before taken
  /// first), or at once for an end with no liveness check.
  #[cfg(any(target_os = "macos", windows))]
  fn departure_due(&mut self, elapsed_ns: u64) -> Result<Before, IpcError> {
    let Some(liveness) = self.liveness.as_ref() else {
      return Ok(Before::Ended(Waited::Departed));
    };
    if self.looks.due(elapsed_ns, self.wake.mean_ns()) {
      self.liveness_looks = self.liveness_looks.saturating_add(1);
      if liveness.daemon_gone() {
        return self.after_departure().map(Before::Ended);
      }
      self.looks.looked(elapsed_ns);
    }
    Ok(Before::Park(self.looks.until(elapsed_ns)))
  }

  /// Linux: the park waits on the departure itself (the control socket closes as the daemon goes, a stop inside its
  /// process too), so no look is ever due and the park has no timeout; an end with no liveness check has no daemon to
  /// wait on.
  #[cfg(not(any(target_os = "macos", windows)))]
  fn departure_due(&mut self, _elapsed_ns: u64) -> Result<Before, IpcError> {
    Ok(if self.liveness.is_none() {
      Before::Ended(Waited::Departed)
    } else {
      Before::Park(None)
    })
  }

  /// A deadline's park (and macOS's departure park between its looks): the wake word's wait, whose wake confirms a
  /// sleeper found (Linux's futex, macOS's ulock), up to `timeout_ns` (`None`: until woken).
  #[cfg(not(windows))]
  fn park_until(&self, expected: u32, timeout_ns: Option<u64>) -> Result<Parked, IpcError> {
    Ok(Parked {
      woken: wake::wait(self.region.wake_word()?, expected, timeout_ns)?,
      departed: false,
      waker_confirms: true,
    })
  }

  /// A deadline's park (Windows): the region's named Event, which cannot tell a signal that met a sleeper from one
  /// consumed later, so the client judges its own sleep; up to `timeout_ns` (`None`: until signaled).
  #[cfg(windows)]
  fn park_until(&self, _expected: u32, timeout_ns: Option<u64>) -> Result<Parked, IpcError> {
    Ok(Parked {
      woken: self.region.wake_wait(timeout_ns)?,
      departed: false,
      waker_confirms: false,
    })
  }

  /// Linux: one `poll` of the wake's level beside the control socket, with no timeout (A-456). The level is the
  /// completion eventfd, which the daemon writes for a parked client, or, while a completion bridge runs, the bridge's
  /// pipe: the bridge is then the eventfd's only reader and writes its pipe after every read, so no nudge is lost to it.
  /// The socket's peer end closes as the daemon's process ends (`POLLRDHUP`, `POLLHUP`); its deliveries make it
  /// readable, which this wait does not ask about. The daemon's futex wake finds no sleeper here, so the client judges
  /// its own sleep (`slates_machine::switches`).
  #[cfg(target_os = "linux")]
  fn park_until_departure(
    &mut self,
    _expected: u32,
    _timeout_ns: Option<u64>,
  ) -> Result<Parked, IpcError> {
    use rustix::event::{PollFd, PollFlags, poll};
    let (woken, departed) = {
      let Some(liveness) = self.liveness.as_ref() else {
        return Ok(Parked::DEPARTED);
      };
      let level = match (&self.bridge, &self.completion) {
        (Some(bridge), _) => bridge.level(),
        (None, Some(eventfd)) => eventfd.as_fd(),
        (None, None) => {
          return Err(IpcError::Unsupported {
            feature: "a wait on the daemon's departure without the completion descriptor",
          });
        }
      };
      let watch = liveness.watch();
      let mut interest = [
        PollFd::new(&level, PollFlags::IN),
        PollFd::new(&watch, PollFlags::RDHUP),
      ];
      match poll(&mut interest, None) {
        Ok(_) => {}
        // A signal: the caller looks at the ring, then parks again.
        Err(rustix::io::Errno::INTR) => return Ok(Parked::INTERRUPTED),
        Err(error) => {
          return Err(IpcError::OsRefused {
            call: "poll",
            code: Some(error.raw_os_error()),
          });
        }
      }
      let [level_ready, watch_ready] = interest.map(|polled| polled.revents());
      (
        level_ready.contains(PollFlags::IN),
        watch_ready
          .intersects(PollFlags::RDHUP | PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL),
      )
    };
    if woken {
      self.take_level();
    }
    Ok(Parked {
      woken,
      departed,
      waker_confirms: false,
    })
  }

  /// macOS: the wake word's wait until the next look at the daemon's exit watch, which comes before the park
  /// ([`Self::departure_due`]).
  #[cfg(target_os = "macos")]
  fn park_until_departure(
    &mut self,
    expected: u32,
    timeout_ns: Option<u64>,
  ) -> Result<Parked, IpcError> {
    self.park_until(expected, timeout_ns)
  }

  /// Windows: the region's Event and the daemon's process handle in one wait (A-456), as a claim waits beside its
  /// answer's (A-447), until the next look at the daemon's start stamp ([`Self::departure_due`]). The Event cannot tell
  /// a signal that met a sleeper from one consumed later, so the client judges its own sleep.
  #[cfg(windows)]
  fn park_until_departure(
    &mut self,
    _expected: u32,
    timeout_ns: Option<u64>,
  ) -> Result<Parked, IpcError> {
    use crate::wake::EventOrExit;
    let Some(process) = self
      .liveness
      .as_ref()
      .and_then(crate::rendezvous::Liveness::exit_handle)
    else {
      return Ok(Parked::DEPARTED);
    };
    let ended = self.region.wake_wait_or_exit(process, timeout_ns)?;
    Ok(Parked {
      woken: ended == EventOrExit::Signaled,
      departed: ended == EventOrExit::Exited,
      waker_confirms: false,
    })
  }

  /// No rendezvous on this platform, so no daemon lives to wait on.
  #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
  fn park_until_departure(
    &mut self,
    _expected: u32,
    _timeout_ns: Option<u64>,
  ) -> Result<Parked, IpcError> {
    Ok(Parked::DEPARTED)
  }

  /// Clears the level a Linux departure park woke on: the bridge's pipe while one runs, whose level is then owed back to
  /// the async loop that polls it ([`Self::give_back_bridge_level`]), else the eventfd, whose read resets its counter.
  /// Both are non-blocking, and a read of an empty one (`EAGAIN`) leaves nothing to clear.
  #[cfg(target_os = "linux")]
  fn take_level(&mut self) {
    if let Some(bridge) = &self.bridge {
      bridge.drain();
      self.bridge_level_owed = true;
    } else if let Some(eventfd) = &self.completion {
      let mut counter = [0u8; size_of::<u64>()];
      let _ = rustix::io::read(eventfd, &mut counter);
    }
  }

  /// Gives the bridge's pipe back the level a departure wait read from it (Linux, A-456): a nudge on the eventfd, which
  /// the bridge reads and passes to its pipe, so the async loop still learns of the replies the synchronous wait drained
  /// past it. A nudge the kernel refuses (a counter at its maximum) leaves the level the eventfd already holds.
  #[cfg(target_os = "linux")]
  fn give_back_bridge_level(&mut self) {
    if std::mem::take(&mut self.bridge_level_owed)
      && let Some(eventfd) = &self.completion
    {
      let _ = rustix::io::write(eventfd, &COMPLETION_NUDGE);
    }
  }
}

/// How a wait ends besides its reply ([`ClientEnd::wait`], [`ClientEnd::wait_while_daemon_lives`]).
#[derive(Clone, Copy, Debug)]
enum Ends {
  /// At the caller's deadline, counted from the wait's start (`None`: never), the daemon's life unasked.
  At(Option<u64>),
  /// At the daemon's departure (A-456).
  Departure,
}

/// What a wait does before its next park.
#[derive(Debug)]
enum Before {
  /// Park, at most this long (`None`: until woken).
  Park(Option<u64>),
  /// The wait is over.
  Ended(Waited),
}

/// How one park ended.
#[derive(Clone, Copy, Debug)]
struct Parked {
  /// The wait returned for a wake (or a moved word, or a signal), not at its timeout.
  woken: bool,
  /// The daemon's process ended while the client waited (Linux: the control socket's peer end closed; Windows: its
  /// process handle signaled), or there was no daemon to wait on.
  departed: bool,
  /// Whether the daemon's wake of this park reports finding the client asleep, so a stamp is learned only once
  /// confirmed: Linux's futex and macOS's ulock wakes answer it. Windows' auto-reset Event cannot tell a signal that met
  /// a sleeper from one consumed later, and a Linux departure park waits on a descriptor the futex wake never reaches,
  /// so for those the client confirms its own sleep by its switch count across the wait ([`wait_may_teach`]), as the
  /// shards judge their parks and the boot probe its waiter (A-223), and a stamp it confirmed is learned once it falls
  /// inside the wait.
  waker_confirms: bool,
}

impl Parked {
  /// No wait made: the daemon has no life to wait on (no liveness check, or its process gone when the watch was taken).
  #[cfg(not(target_os = "macos"))]
  const DEPARTED: Parked = Parked {
    woken: false,
    departed: true,
    waker_confirms: false,
  };
  /// A Linux departure park a signal ended: a wake with nothing to say, after which the caller looks and parks again.
  #[cfg(target_os = "linux")]
  const INTERRUPTED: Parked = Parked {
    woken: true,
    departed: false,
    waker_confirms: false,
  };
}

/// A park a reply ended whose wake is not yet folded into the estimate (`ClientEnd::settle_wakes`).
#[derive(Clone, Copy, Debug)]
struct Unsettled {
  /// When the client began its wait (the host clock, nanoseconds).
  waiting_ns: u64,
  /// When the wait returned.
  returned_ns: u64,
  /// Whether the park's waker confirms a sleeper found ([`Parked::waker_confirms`]): its stamp is then learned only
  /// once confirmed; otherwise the client's switch count judged the sleep before the park was left unsettled.
  waker_confirms: bool,
}

/// Whether a park's wake goes on to be judged by its stamp ([`ClientEnd::settle_wakes`]), from the client's
/// own switch count across its wait (`slates_machine::switches`). Where the daemon's wake confirms a sleeper
/// found (`waker_confirms`), that confirmation judges and every park goes on. Elsewhere (Windows' Event, and a Linux
/// park that waits on its daemon's departure, A-456) the client judges itself: only a wait across which its count
/// rose slept. A wait that found the reply's signal already set returned without switching, and timing it taught an
/// on-CPU handoff (0.1–0.8 µs) for a sleeper's wake (6–9 µs on CI's Windows runners, A-223); a count the OS refused
/// proves nothing, so the client learns nothing and keeps its seed.
fn wait_may_teach(
  waker_confirms: bool,
  switches_before: Option<u64>,
  switches_after: Option<u64>,
) -> bool {
  waker_confirms
    || matches!((switches_before, switches_after), (Some(before), Some(after)) if after > before)
}

/// The client's parked flag for a park that names `slot`.
fn parked_flag(slot: usize) -> u32 {
  if slot == 0 {
    PARKED_STAMP_SLOT_ZERO
  } else {
    PARKED_STAMP_SLOT_ONE
  }
}

/// The reply stamp slot a parked flag's value names (any armed value other than slot one's names slot 0).
fn stamp_slot(parked: u32) -> usize {
  usize::from(parked == PARKED_STAMP_SLOT_ONE)
}

impl ClientEnd {
  /// Folds each unsettled park's wake into the estimate once its stamp can be judged: the daemon's reply
  /// stamp to the client's return, when the client was confirmed asleep (by the daemon's wake, or by its own
  /// switch count before the park was left unsettled where the park's waker cannot confirm, [`Parked::waker_confirms`])
  /// and the stamp falls inside the wait — a sleeper woken, the event the boot probe times. A reply that landed
  /// while the park was being set up met a waiter not yet asleep: its wake finds no sleeper and is never
  /// confirmed, and timing it would teach a wake that never happened (measured: the bench's daemon,
  /// replying the instant the flag rose, taught 611–974 ns against the boot probe's 2.0–3.3 µs, macOS,
  /// 2026-09-25). A stamp not yet confirmed stays unsettled: the daemon confirms after its wake call
  /// returns, which the woken client can outrun by a whole further park under load (measured: 26 to 29 of
  /// 32 parks on two busy CPUs, 2026-11-04), so it stays in its slot until that slot is armed again.
  fn settle_wakes(&mut self) -> Result<(), IpcError> {
    for slot in 0..REPLY_STAMP_SLOTS {
      let Some(Some(park)) = self.unsettled.get(slot).copied() else {
        continue;
      };
      let stamp = self.region.reply_stamp(slot)?.load(Ordering::Acquire);
      let confirmed = stamp & REPLY_STAMP_CONFIRMED != 0;
      if park.waker_confirms && !confirmed {
        continue;
      }
      if let Some(settled) = self.unsettled.get_mut(slot) {
        *settled = None;
      }
      let sent_ns = stamp & !REPLY_STAMP_CONFIRMED;
      if park.waiting_ns <= sent_ns && sent_ns <= park.returned_ns {
        self.wake.record(park.returned_ns.saturating_sub(sent_ns));
        self.wake_samples = self.wake_samples.saturating_add(1);
      }
    }
    Ok(())
  }
}

/// The daemon's end.
pub struct DaemonEnd {
  region: ClientRegion,
  next_request: u64,
  next_reply: u64,
  /// Wakes issued.
  wakes: u64,
  /// The completion fd to make readable when a reply reaches a parked client, so an async SDK event
  /// loop polling it wakes (§4.7, D-19). `None` where the platform has no completion channel. Signaled
  /// only under the same parked check as the wake-word wake, so a spinning client costs no extra syscall.
  #[cfg(unix)]
  completion: Option<OwnedFd>,
}

impl std::fmt::Debug for DaemonEnd {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("DaemonEnd")
      .field("region", &self.region)
      .field("wakes", &self.wakes)
      .finish()
  }
}

impl DaemonEnd {
  /// The daemon's end over the region it created.
  pub fn new(region: ClientRegion) -> DaemonEnd {
    DaemonEnd {
      region,
      next_request: 0,
      next_reply: 0,
      wakes: 0,
      #[cfg(unix)]
      completion: None,
    }
  }

  /// Sets the completion fd the rendezvous handed the daemon for this client — the write end the daemon
  /// nudges on a reply to a parked client. `None` clears it.
  #[cfg(unix)]
  pub fn set_completion(&mut self, completion: Option<OwnedFd>) {
    self.completion = completion;
  }

  /// The region.
  pub fn region(&self) -> &ClientRegion {
    &self.region
  }

  /// The region, mutably (the bulk area).
  pub fn region_mut(&mut self) -> &mut ClientRegion {
    &mut self.region
  }

  /// Wakes issued so far.
  pub fn wakes(&self) -> u64 {
    self.wakes
  }

  /// The ring index the next reply takes (its bulk chunk is chosen by it).
  pub fn next_reply_index(&self) -> u64 {
    self.next_reply
  }

  /// Takes the next request if one is there, its body owned: a bulk body is copied out before the slot is released,
  /// so a client sending on into the freed slot's chunk never changes it ([`crate::protocol::owned`]). A hostile slot
  /// is released and reported.
  pub fn try_take(&mut self) -> Result<Option<Request>, IpcError> {
    let cmd = self.region.cmd();
    let peeked = match cmd.peek(self.region.object(), self.next_request) {
      Ok(None) => return Ok(None),
      Ok(Some(peeked)) => Some(peeked.and_then(|slot| crate::protocol::owned(&self.region, slot))),
      Err(e) => {
        self.next_request = self.next_request.wrapping_add(1);
        return Err(e);
      }
    };
    let released = cmd.release(self.region.object(), self.next_request);
    self.next_request = self.next_request.wrapping_add(1);
    released?;
    let Some(slot) = peeked.transpose()? else {
      return Ok(None);
    };
    Ok(Some(Request {
      request: slot.request,
      kind: slot.kind,
      payload: slot.payload,
    }))
  }

  /// Replies `message` to request `request`: packed for the next completion-ring position and written, waking the client
  /// if it parked; `RingFull` when the client has not taken the slot the ring wraps onto. Room is checked before
  /// anything is packed, as [`ClientEnd::send_message`] checks it: the position's bulk chunk holds the oldest reply the
  /// client has not taken until it does. The only way a reply is packed.
  pub fn reply_message<M: slates_wire::Wire>(
    &mut self,
    request: u64,
    message: &M,
  ) -> Result<(), IpcError> {
    let index = self.next_reply;
    if !self.region.cpl().can_push(self.region.object(), index)? {
      return Err(IpcError::RingFull);
    }
    let slot = crate::protocol::pack(
      &mut self.region,
      crate::protocol::Direction::Reply,
      index,
      request,
      message,
    )?;
    self.reply(&slot)
  }

  /// Writes a reply and wakes the client if it parked.
  pub fn reply(&mut self, slot: &Slot) -> Result<(), IpcError> {
    let cpl = self.region.cpl();
    cpl.push(
      self.region.object_mut(),
      self.next_reply,
      slot,
      crate::slot::stamp_now(),
    )?;
    self.next_reply = self.next_reply.wrapping_add(1);
    let word = self.region.wake_word()?;
    word.fetch_add(1, Ordering::AcqRel);
    let parked = self.region.client_parked()?;
    let mut armed = 0;
    // The park protocol's daemon half (`crate::park`): the flag is read after the reply's publication in
    // the order the client's fence takes part in, so a parked client whose re-check missed this reply is
    // seen here and woken. Before 2026-09-29 this read had no fence, and a client's re-check and this read
    // could both miss. The value read names the park's reply stamp slot.
    if crate::park::after_publishing_armed(|| {
      armed = parked.load(Ordering::Relaxed);
      armed != 0
    }) {
      // A second advance for the wake: a bridge between two waits has already taken the first advance
      // into the value its next wait compares against, and this one makes that wait return.
      word.fetch_add(1, Ordering::AcqRel);
      // The first wake of this park stamps the host clock, so the client can time its own wake (§4.7);
      // a later reply to the same park keeps the first stamp.
      let stamp = self.region.reply_stamp(stamp_slot(armed))?;
      let sent_ns = slates_machine::clock::monotonic_ns() & !REPLY_STAMP_CONFIRMED;
      let stamped = stamp
        .compare_exchange(0, sent_ns, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok();
      // Wake the parked client: the wake word on Linux/macOS (a spinning client never parked, so it
      // pays no wake), the named auto-reset Event on Windows. Where the wake reports a sleeper found,
      // the stamp it timed is confirmed for the client — that stamp only, in its own slot. The woken client
      // can arm its next park before this confirmation lands; that park names the other slot, so this one
      // is intact (the bug record of 2026-11-04: with one slot the next park cleared it, and the
      // confirmation then left a bare confirmed bit that refused the following park's stamp).
      #[cfg(windows)]
      self.region.wake_signal()?;
      #[cfg(not(windows))]
      if wake::wake_one(word)? && stamped {
        // A failed exchange is this slot armed again before the confirmation, which the ring's order
        // rules out but for a client that gave the reply up past its deadline: nothing left to confirm.
        let _ = stamp.compare_exchange(
          sent_ns,
          sent_ns | REPLY_STAMP_CONFIRMED,
          Ordering::Release,
          Ordering::Relaxed,
        );
      }
      #[cfg(windows)]
      let _ = stamped;
      self.wakes = self.wakes.saturating_add(1);
      // Nudge the completion fd too, so an async SDK event loop polling it (D-19) wakes alongside a
      // futex-parked sync client. Both are under the same parked check, so a spinning client pays for
      // neither.
      #[cfg(unix)]
      self.nudge_completion();
    }
    Ok(())
  }

  /// Makes the completion fd readable (a write of `COMPLETION_NUDGE`), best-effort. A short write, a
  /// full pipe, or an absent fd is ignored: the fd's readability is the signal and the wake word has
  /// already advanced, so a missed nudge only means the SDK falls back to its next poll of the ring,
  /// never a lost reply.
  #[cfg(unix)]
  fn nudge_completion(&self) {
    if let Some(fd) = &self.completion {
      let _ = rustix::io::write(fd, &COMPLETION_NUDGE);
    }
  }

  /// Marks the daemon's shard going idle (true) or polling (false), so the client knows whether
  /// to ring the doorbell. Announcing idle is half of the doorbell protocol: the shard must re-check its
  /// rings through [`crate::doorbell::after_announcing_idle_pending`] before it idles.
  pub fn set_parked(&self, parked: bool) -> Result<(), IpcError> {
    self
      .region
      .daemon_parked()?
      .store(u32::from(parked), Ordering::Release);
    Ok(())
  }

  /// The doorbell's value (the client bumps it per request while the daemon is parked).
  pub fn doorbell(&self) -> Result<u32, IpcError> {
    Ok(self.region.doorbell()?.load(Ordering::Acquire))
  }
}

fn elapsed_ns(since: Instant) -> u64 {
  u64::try_from(since.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod judged {
  use super::wait_may_teach;

  /// §4.7, §4.3 (A-223, sibling (d)): do judge waits by the client's own switch count, as on Windows, and by
  /// the daemon's confirmation, as on Linux and macOS. Expect: under its own count only a wait across which
  /// the count rose goes on to teach, while one it never moved across, or one whose count the OS refused (its
  /// profiling declined), teaches nothing, so the estimate keeps its seed; under the daemon's confirmation
  /// every wait goes on, its stamp judged when the confirmation lands.
  #[test]
  fn only_a_wait_that_switched_out_teaches_where_the_client_judges_itself() {
    let waits = [
      (Some(7), Some(8), true),
      (Some(7), Some(7), false),
      (None, None, false),
      (Some(7), None, false),
      (None, Some(8), false),
    ];
    for (before, after, slept) in waits {
      assert_eq!(
        wait_may_teach(false, before, after),
        slept,
        "the client's own count, {before:?} → {after:?}"
      );
      assert!(
        wait_may_teach(true, before, after),
        "the daemon's confirmation, {before:?} → {after:?}"
      );
    }
  }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos", windows)))]
mod departure {
  use std::time::{Duration, Instant};

  use slates_mem::Handoff;

  use super::{ClientEnd, DaemonEnd, Waited};
  use crate::error::IpcError;
  use crate::region::{ClientRegion, RegionGeometry};
  use crate::rendezvous::Liveness;
  use crate::slot::Slot;

  /// Shape: the reply deadline the client's wait re-armed at before A-456, the anchor's liveness budget
  /// (`slates_server::daemon::LIVENESS_BUDGET_NS`, a second): the late reply comes past it, and a departure is seen
  /// within it, where before the first question of the daemon's life came at it.
  const OLD_REPLY_DEADLINE_NS: u64 = 1_000_000_000;
  /// Shape: how long the late-reply test's daemon holds its answer: half again the old reply deadline.
  const LATE_NS: u64 = OLD_REPLY_DEADLINE_NS + OLD_REPLY_DEADLINE_NS / 2;
  /// Shape: how far into the wait the departure tests' daemon goes: a quarter of the old reply deadline, so even the
  /// doubling looks (macOS, Windows), which see a departure within the time waited, see it within half the old deadline,
  /// short of the lateness their timers allow.
  const DEPARTS_AFTER_NS: u64 = OLD_REPLY_DEADLINE_NS / 4;
  /// Shape: how long a stand-in daemon process lives unless the test ends it, in seconds: past any of these tests.
  #[cfg(any(target_os = "macos", windows))]
  const STAND_IN_LIFE_S: &str = "30";

  fn geometry() -> RegionGeometry {
    RegionGeometry {
      slots: 8,
      spin_ns: 200_000,
      spin_shift: 4,
      bulk_bytes: 4096,
      page: 4096,
    }
  }

  /// A region name unique to this process (the Unix tests' `unique_name` says why).
  fn unique_name(test: &str) -> String {
    format!("{test}-{}", std::process::id())
  }

  /// What a live daemon of `instance` publishes, standing in for one (macOS, Windows): its rendezvous, whose bootstrap
  /// object holds the start stamp a client's liveness check compares. Kept for the test; Linux's check is the control
  /// socket alone.
  #[cfg(any(target_os = "macos", windows))]
  fn published(instance: &str) -> Result<crate::rendezvous::Listener, IpcError> {
    crate::rendezvous::Listener::open(instance, None)
  }

  /// The OS's refusal of `call`, typed as the crate's.
  fn refused(call: &'static str) -> impl FnOnce(std::io::Error) -> IpcError {
    move |error| IpcError::OsRefused {
      call,
      code: error.raw_os_error(),
    }
  }

  /// The daemon's process as these tests stand it in, ended when dropped: on Linux the daemon's end of a control
  /// socket, which the kernel closes as the daemon goes; on macOS and Windows a child process the client end's exit
  /// watch is on.
  struct DaemonLife {
    #[cfg(target_os = "linux")]
    _socket: std::os::fd::OwnedFd,
    #[cfg(any(target_os = "macos", windows))]
    process: std::process::Child,
  }

  #[cfg(any(target_os = "macos", windows))]
  impl Drop for DaemonLife {
    fn drop(&mut self) {
      let _ = self.process.kill();
      let _ = self.process.wait();
    }
  }

  /// A stand-in daemon's life and the liveness check a client end watches it by.
  #[cfg(target_os = "linux")]
  fn daemon_life(_instance: &str) -> Result<(DaemonLife, Liveness), IpcError> {
    use rustix::net::{AddressFamily, SocketFlags, SocketType};
    let (daemon, client) = rustix::net::socketpair(
      AddressFamily::UNIX,
      SocketType::STREAM,
      SocketFlags::CLOEXEC,
      None,
    )
    .map_err(|errno| refused("socketpair")(errno.into()))?;
    Ok((DaemonLife { _socket: daemon }, Liveness::of_socket(client)?))
  }

  /// A stand-in daemon's life and the liveness check a client end watches it by: `instance`'s published stamp and the
  /// child's process.
  #[cfg(target_os = "macos")]
  fn daemon_life(instance: &str) -> Result<(DaemonLife, Liveness), IpcError> {
    let process = std::process::Command::new("/bin/sleep")
      .arg(STAND_IN_LIFE_S)
      .spawn()
      .map_err(refused("spawn the stand-in daemon"))?;
    let pid = process.id();
    let life = DaemonLife { process };
    Ok((life, Liveness::of_daemon(instance, pid)?))
  }

  /// A stand-in daemon's life and the liveness check a client end watches it by: `instance`'s published stamp and the
  /// child's process.
  #[cfg(windows)]
  fn daemon_life(instance: &str) -> Result<(DaemonLife, Liveness), IpcError> {
    let process = std::process::Command::new("cmd")
      .args(["/C", "ping", "-n", STAND_IN_LIFE_S, "127.0.0.1"])
      .stdout(std::process::Stdio::null())
      .spawn()
      .map_err(refused("spawn the stand-in daemon"))?;
    let pid = process.id();
    let life = DaemonLife { process };
    Ok((life, Liveness::of_daemon(instance, pid)?))
  }

  /// A test's rings: the region's creator (kept until the test ends), a client end over a second mapping watching
  /// `liveness`, and what a daemon end opens in another thread: the region's handoff and, on Linux, the daemon's copy of
  /// the completion eventfd the client end holds, as the rendezvous hands both over.
  struct Rings {
    _region: ClientRegion,
    client: ClientEnd,
    handoff: Handoff,
    len: usize,
    #[cfg(target_os = "linux")]
    daemon_completion: Option<std::os::fd::OwnedFd>,
  }

  fn rings(test: &str, liveness: Liveness) -> Result<Rings, IpcError> {
    let region = ClientRegion::create(&unique_name(test), 7, 0, geometry())?;
    let (handoff, len) = region.handoff()?;
    let mut client = ClientEnd::new(ClientRegion::open(&handoff, len)?);
    client.set_liveness(liveness);
    #[cfg(target_os = "linux")]
    let daemon_completion = {
      use rustix::event::{EventfdFlags, eventfd};
      let completion = eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)
        .map_err(|errno| refused("eventfd")(errno.into()))?;
      let daemon_copy =
        rustix::io::dup(&completion).map_err(|errno| refused("dup")(errno.into()))?;
      client.set_completion(completion);
      Some(daemon_copy)
    };
    Ok(Rings {
      _region: region,
      client,
      handoff,
      len,
      #[cfg(target_os = "linux")]
      daemon_completion,
    })
  }

  impl Rings {
    /// A daemon end over its own mapping of the region, handed the completion eventfd's copy where one exists.
    fn daemon_end(&mut self) -> impl FnOnce() -> Result<DaemonEnd, IpcError> + Send + use<> {
      let handoff = self.handoff.clone();
      let len = self.len;
      #[cfg(target_os = "linux")]
      let completion = self.daemon_completion.take();
      move || {
        #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
        let mut daemon = DaemonEnd::new(ClientRegion::open(&handoff, len)?);
        #[cfg(target_os = "linux")]
        daemon.set_completion(completion);
        Ok(daemon)
      }
    }
  }

  /// §4.7 "Failure matrix" (A-232, A-456). Do: wait on a live daemon for the reply it sends half again the old reply
  /// deadline after the request, its process alive throughout. Expect: the reply taken, after the daemon held it; on
  /// Linux no look at the daemon's life at all, the park having waited on its departure itself; on macOS and Windows at
  /// least one look, and no more than the doubling allows: the logarithm of the wait over twice the spin window the
  /// region published (the wake estimate when the wait began), plus two.
  #[test]
  fn a_reply_after_the_old_reply_deadline_is_taken_while_its_daemon_lives() {
    let _gate = crate::descriptor_test_gate();
    let instance = unique_name("slates-endpoint-late-reply-daemon");
    #[cfg(any(target_os = "macos", windows))]
    let _rendezvous = published(&instance).unwrap();
    let (life, liveness) = daemon_life(&instance).unwrap();
    let mut rings = rings("slates-endpoint-late-reply", liveness).unwrap();
    let open_daemon = rings.daemon_end();
    rings.client.send(&Slot::inline(1, b"hi").unwrap()).unwrap();
    let replier = reply_late(open_daemon);
    let asked = Instant::now();
    let waited = rings.client.wait_while_daemon_lives().unwrap();
    let took = asked.elapsed();
    replier.join().unwrap().unwrap();
    drop(life);
    let looks = rings.client.liveness_looks();
    println!("a reply held {LATE_NS} ns: {waited:?} after {took:?}, {looks} looks");
    let Waited::Reply(reply) = &waited else {
      panic!("the wait ended {waited:?} with its daemon alive");
    };
    assert_eq!((reply.request, reply.payload.as_slice()), (1, &b"late"[..]));
    assert!(
      took >= Duration::from_nanos(LATE_NS),
      "taken after {took:?}"
    );
    if cfg!(any(target_os = "macos", windows)) {
      // The first park comes after the spin, the published seed at the wait's start, so the first look comes no sooner
      // than twice it, and every later one at twice the time waited.
      let first = 2 * u64::from(geometry().spin_ns);
      let took_ns = u64::try_from(took.as_nanos()).unwrap();
      let bound = u64::from((took_ns / first).max(1).ilog2()) + 2;
      assert!(
        (1..=bound).contains(&looks),
        "{looks} looks across {took:?} from {first} ns"
      );
    } else {
      assert_eq!(looks, 0, "the park waited on the departure, never looked");
    }
  }

  /// On its own thread, a daemon end opened by `open_daemon` takes the first request and answers it `LATE_NS` after.
  fn reply_late(
    open_daemon: impl FnOnce() -> Result<DaemonEnd, IpcError> + Send + 'static,
  ) -> std::thread::JoinHandle<Result<(), IpcError>> {
    std::thread::spawn(move || {
      let mut daemon = open_daemon()?;
      let request = loop {
        if let Some(request) = daemon.try_take()? {
          break request;
        }
        std::thread::yield_now();
      };
      #[allow(clippy::disallowed_methods)]
      // the daemon holds its answer on purpose: the hold is what is tested.
      std::thread::sleep(Duration::from_nanos(LATE_NS));
      daemon.reply(&Slot::inline(request.request, b"late")?)
    })
  }

  /// A-506 (A-456's accounting). Do: wait on a live daemon for the reply it sends half again the old reply deadline after
  /// the request, and read how each park ended. Expect: every park counted, ended by the reply, by a wake that brought
  /// none, or at its look at the daemon's life; on macOS and Windows at least one park a look ended (its timeout came,
  /// the look found the daemon alive, and the client parked again), as many as the looks made or one more (the reply
  /// taken as the last such park returned needs no look); on Linux none, its park waiting on the departure itself. And a
  /// park a look ended teaches the wake estimate nothing: the estimate gains at most one sample for each park a look did
  /// not end. Before A-506 such a park was counted as no kind, and the client suite's accounting failed on macOS
  /// (`the_typed_verbs_drive_the_lifecycle_and_refusals_are_typed`: 32 parks, 17 replies, 0 unanswered).
  #[test]
  fn a_park_a_liveness_look_ends_is_counted_as_its_own_kind_and_teaches_the_estimate_nothing() {
    let _gate = crate::descriptor_test_gate();
    let instance = unique_name("slates-endpoint-look-park-daemon");
    #[cfg(any(target_os = "macos", windows))]
    let _rendezvous = published(&instance).unwrap();
    let (life, liveness) = daemon_life(&instance).unwrap();
    let mut rings = rings("slates-endpoint-look-park", liveness).unwrap();
    let open_daemon = rings.daemon_end();
    rings.client.send(&Slot::inline(1, b"hi").unwrap()).unwrap();
    let samples_before = rings.client.wake_samples();
    let replier = reply_late(open_daemon);
    let waited = rings.client.wait_while_daemon_lives().unwrap();
    replier.join().unwrap().unwrap();
    drop(life);
    let (parks, replies) = rings.client.park_ratio();
    let unanswered = rings.client.unanswered_wakes();
    let at_looks = rings.client.parks_ended_by_looks();
    let looks = rings.client.liveness_looks();
    let taught = rings.client.wake_samples() - samples_before;
    println!(
      "a reply held {LATE_NS} ns: {parks} parks, {replies} replies, {unanswered} unanswered, {at_looks} ended by \
       looks, {looks} looks, {taught} taught the estimate"
    );
    assert!(
      matches!(waited, Waited::Reply(_)),
      "the wait ended {waited:?} with its daemon alive"
    );
    assert!(
      parks <= replies + unanswered + at_looks,
      "every park is counted: {parks} parks, {replies} replies, {unanswered} unanswered, {at_looks} ended by looks"
    );
    if cfg!(any(target_os = "macos", windows)) {
      assert!(
        at_looks >= 1 && (looks..=looks + 1).contains(&at_looks),
        "{at_looks} parks ended by looks, {looks} looks"
      );
    } else {
      assert_eq!(
        at_looks, 0,
        "the park waited on the departure, never looked"
      );
    }
    assert!(
      taught <= parks - at_looks,
      "{taught} samples taught from {parks} parks, {at_looks} of them ended by looks"
    );
  }

  /// A-456. Do: wait on a daemon that never answers, and end its process a quarter of the old reply deadline into the
  /// wait. Expect: the wait ends `Departed` before the old reply deadline, at which it first asked after the daemon
  /// before A-456: on Linux at the departure itself, never having looked; on Windows at the departure itself, through
  /// the process handle; on macOS at the first look after it, within the time waited.
  #[test]
  fn a_daemons_departure_ends_the_wait_before_the_old_reply_deadline() {
    let _gate = crate::descriptor_test_gate();
    let instance = unique_name("slates-endpoint-departure-daemon");
    #[cfg(any(target_os = "macos", windows))]
    let _rendezvous = published(&instance).unwrap();
    let (life, liveness) = daemon_life(&instance).unwrap();
    let mut rings = rings("slates-endpoint-departure", liveness).unwrap();
    let _daemon = rings.daemon_end()().unwrap();
    rings.client.send(&Slot::inline(1, b"hi").unwrap()).unwrap();
    let ender = std::thread::spawn(move || {
      #[allow(clippy::disallowed_methods)]
      // the daemon lives a while into the wait on purpose: its end is what is tested.
      std::thread::sleep(Duration::from_nanos(DEPARTS_AFTER_NS));
      drop(life);
      Instant::now()
    });
    let asked = Instant::now();
    let waited = rings.client.wait_while_daemon_lives().unwrap();
    let ended = Instant::now();
    let departed = ender.join().unwrap();
    let looks = rings.client.liveness_looks();
    println!(
      "a daemon that departed {:?} into the wait: {waited:?} {:?} after it, {:?} after the ask, {looks} looks",
      departed.duration_since(asked),
      ended.saturating_duration_since(departed),
      ended.duration_since(asked)
    );
    assert_eq!(waited, Waited::Departed);
    assert!(
      ended.duration_since(asked) < Duration::from_nanos(OLD_REPLY_DEADLINE_NS),
      "the wait ended {:?} after it began, not before the old reply deadline",
      ended.duration_since(asked)
    );
    if cfg!(target_os = "linux") {
      assert_eq!(looks, 0, "the park waited on the departure, never looked");
    } else if cfg!(target_os = "macos") {
      assert!(looks >= 1, "the departure was found by a look");
    }
  }

  /// The A-456 regression (2026-11-18): a daemon restarted inside the client's own process (the in-process daemons of
  /// the client and server tests, an embedding that restarts its daemon) changes only its start stamp; its process never
  /// ends. Do: wait on a daemon of this very process for a reply it never sends, and a quarter of the old reply deadline
  /// into the wait restart it inside the process: its rendezvous dropped and opened again, a new stamp published. Expect:
  /// the wait ends `Departed` before the old reply deadline, at a look that finds the new stamp. Before the fix the looks
  /// asked the process's exit alone, and the wait went on until the restarted side answered the old ring, which the
  /// test does once twice the old deadline has passed, so it ended with that reply.
  #[cfg(any(target_os = "macos", windows))]
  #[test]
  fn a_daemon_restarted_inside_its_own_process_ends_the_wait() {
    use std::sync::mpsc::channel;
    type OpenDaemon = Box<dyn FnOnce() -> Result<DaemonEnd, IpcError> + Send>;
    let instance = unique_name("slates-endpoint-in-process-daemon");
    let (published_tx, published_rx) = channel::<()>();
    let (daemon_tx, daemon_rx) = channel::<OpenDaemon>();
    let (ended_tx, ended_rx) = channel::<()>();
    let restarter = {
      let instance = instance.clone();
      std::thread::spawn(move || {
        let first = published(&instance).unwrap();
        published_tx.send(()).unwrap();
        let open_daemon = daemon_rx.recv().unwrap();
        #[allow(clippy::disallowed_methods)]
        // the daemon lives a while into the wait on purpose: its restart is tested.
        std::thread::sleep(Duration::from_nanos(DEPARTS_AFTER_NS));
        drop(first);
        let second = published(&instance).unwrap();
        let restarted = Instant::now();
        // The restarted daemon is held until the wait ends. A wait still going twice the old deadline later missed the
        // restart, and is answered on its old ring, so the test ends rather than hangs.
        let missed = ended_rx
          .recv_timeout(Duration::from_nanos(2 * OLD_REPLY_DEADLINE_NS))
          .is_err();
        if missed {
          let mut daemon = open_daemon().unwrap();
          if let Some(request) = daemon.try_take().unwrap() {
            daemon
              .reply(&Slot::inline(request.request, b"rescued").unwrap())
              .unwrap();
          }
        }
        drop(second);
        (restarted, missed)
      })
    };
    published_rx.recv().unwrap();
    let liveness = Liveness::of_daemon(&instance, std::process::id()).unwrap();
    let mut rings = rings("slates-endpoint-in-process", liveness).unwrap();
    daemon_tx.send(Box::new(rings.daemon_end())).unwrap();
    rings.client.send(&Slot::inline(1, b"hi").unwrap()).unwrap();
    let asked = Instant::now();
    let waited = rings.client.wait_while_daemon_lives().unwrap();
    let ended = Instant::now();
    let _ = ended_tx.send(());
    let (restarted, missed) = restarter.join().unwrap();
    let looks = rings.client.liveness_looks();
    println!(
      "a daemon restarted {:?} into the wait inside this process: {waited:?} {:?} after it, {:?} after the ask, {looks} \
       looks",
      restarted.duration_since(asked),
      ended.saturating_duration_since(restarted),
      ended.duration_since(asked)
    );
    assert!(
      !missed && waited == Waited::Departed,
      "the restart inside the daemon's process went unseen: the wait ended {waited:?} after {:?}",
      ended.duration_since(asked)
    );
    assert!(
      ended.duration_since(asked) < Duration::from_nanos(OLD_REPLY_DEADLINE_NS),
      "the wait ended {:?} after it began, not before the old reply deadline",
      ended.duration_since(asked)
    );
    assert!(looks >= 1, "the restart was found by a look");
  }
}

#[cfg(all(test, unix))]
mod tests {
  use std::os::fd::{BorrowedFd, RawFd};
  use std::sync::atomic::Ordering;

  use rustix::net::{AddressFamily, SocketFlags, SocketType};

  use super::{ClientEnd, DaemonEnd};
  use crate::region::{ClientRegion, RegionGeometry};
  use crate::slot::Slot;

  /// A region name unique to this process: a fixed name collides when two runs of these tests overlap
  /// on a platform that names its shared objects (macOS), and the second run's create removes the
  /// first's name or is refused `EEXIST`.
  fn unique_name(test: &str) -> String {
    format!("{test}-{}", std::process::id())
  }

  fn geometry() -> RegionGeometry {
    RegionGeometry {
      slots: 8,
      spin_ns: 200_000,
      spin_shift: 4,
      bulk_bytes: 4096,
      page: 4096,
    }
  }

  /// Reads whatever the non-blocking fd has right now (`0` when empty), so a test checks readability
  /// without blocking or a poll dependency.
  fn read_available(fd: RawFd) -> usize {
    let mut buf = [0u8; 8];
    // SAFETY: `fd` is the client end's live, non-blocking completion fd for the test's duration.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    rustix::io::read(borrowed, &mut buf).unwrap_or(0)
  }

  /// A parked client's completion fd becomes readable when the daemon replies (§4.7, D-19): the
  /// fd-readiness an async SDK event loop polls instead of spinning. A socketpair stands in for the
  /// platform completion channel (an eventfd on Linux), the same shape, testable on every unix. Do:
  /// pair the ends, mark the client parked, send a request, reply. Expect: the completion fd is quiet
  /// before, carries the nudge after, and the reply is waiting for `try_take`.
  #[test]
  fn a_reply_to_a_parked_client_nudges_the_completion_fd() {
    let region =
      ClientRegion::create(&unique_name("slates-endpoint-completion"), 7, 0, geometry()).unwrap();
    let (handoff, len) = region.handoff().unwrap();
    let client_region = ClientRegion::open(&handoff, len).unwrap();
    let mut daemon = DaemonEnd::new(region);
    let mut client = ClientEnd::new(client_region);

    // macOS's socketpair takes no CLOEXEC/NONBLOCK creation flags (Linux-only), so pass none and set
    // the client end non-blocking with an ioctl — the real completion fds are created non-blocking too,
    // so the readability check never blocks.
    let (daemon_fd, client_fd) = rustix::net::socketpair(
      AddressFamily::UNIX,
      SocketType::STREAM,
      SocketFlags::empty(),
      None,
    )
    .unwrap();
    rustix::io::ioctl_fionbio(&client_fd, true).unwrap();
    daemon.set_completion(Some(daemon_fd));
    client.set_completion(client_fd);
    let poll_fd = client.completion_fd().unwrap();

    assert_eq!(
      read_available(poll_fd),
      0,
      "the completion fd is quiet before any reply"
    );

    // Mark the client parked (the idle condition the daemon signals for), then run one round trip.
    client
      .region()
      .client_parked()
      .unwrap()
      .store(1, Ordering::Release);
    client.send(&Slot::inline(1, b"hi").unwrap()).unwrap();
    let request = daemon.try_take().unwrap().unwrap();
    daemon
      .reply(&Slot::inline(request.request, b"ok").unwrap())
      .unwrap();

    // The daemon nudged the completion fd: an async event loop polling it would wake here.
    assert!(
      read_available(poll_fd) > 0,
      "a reply to a parked client makes the completion fd readable"
    );
    // And the reply is in the ring for the SDK to take once its loop wakes.
    let reply = client.try_take().unwrap().unwrap();
    assert_eq!(reply.request, 1);
    assert_eq!(reply.payload, b"ok");
  }

  /// Waits up to `timeout_ns` for `fd` to become readable without consuming it (a poll, not a read),
  /// so a test asserts readiness and drains separately. `true` if readable within the budget.
  #[cfg(any(target_os = "macos", target_os = "linux"))]
  fn poll_readable(fd: RawFd, timeout_ns: u64) -> bool {
    use rustix::event::{PollFd, PollFlags, Timespec};
    // SAFETY: `fd` is the client's live completion fd, borrowed for one poll within the test.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let mut fds = [PollFd::new(&borrowed, PollFlags::IN)];
    let ts = Timespec {
      tv_sec: i64::try_from(timeout_ns / 1_000_000_000).unwrap_or(0),
      tv_nsec: i64::try_from(timeout_ns % 1_000_000_000).unwrap_or(0),
    };
    rustix::event::poll(&mut fds, Some(&ts)).is_ok_and(|n| n > 0)
  }

  /// The macOS completion bridge signals only an *armed* reply, so the async fast path stays free of
  /// any event-loop wakeup, and it stops clean on drop (§4.7, D-19). Do: enable the bridge; reply to a
  /// disarmed client (the fast path) and then to an armed one (the slow path). Expect: the fd stays
  /// quiet for the disarmed reply, becomes readable for the armed one, both replies wait in the ring,
  /// and dropping the client joins the bridge thread (the test returns rather than hanging).
  #[cfg(target_os = "macos")]
  #[test]
  fn the_completion_bridge_signals_only_an_armed_reply_and_stops_clean() {
    let region =
      ClientRegion::create(&unique_name("slates-endpoint-bridge"), 7, 0, geometry()).unwrap();
    let (handoff, len) = region.handoff().unwrap();
    let client_region = ClientRegion::open(&handoff, len).unwrap();
    let mut daemon = DaemonEnd::new(region);
    let mut client = ClientEnd::new(client_region);
    let fd = client.enable_async_completion().unwrap();

    // Fast path: the client is not armed (parked == 0), so the daemon issues no wake and the bridge
    // writes nothing — no event-loop wakeup for a reply the client would take during its spin.
    client.send(&Slot::inline(1, b"hi").unwrap()).unwrap();
    let request = daemon.try_take().unwrap().unwrap();
    daemon
      .reply(&Slot::inline(request.request, b"one").unwrap())
      .unwrap();
    assert!(
      !poll_readable(fd, 100_000_000),
      "a disarmed (fast-path) reply does not wake the event loop"
    );
    let reply = client.try_take().unwrap().unwrap();
    assert_eq!(reply.payload, b"one");

    // Slow path: the client arms before it would yield, so the daemon wakes the word and the bridge
    // makes the pipe readable for the loop.
    client.arm_async().unwrap();
    client.send(&Slot::inline(2, b"hi").unwrap()).unwrap();
    let request = daemon.try_take().unwrap().unwrap();
    daemon
      .reply(&Slot::inline(request.request, b"two").unwrap())
      .unwrap();
    assert!(
      poll_readable(fd, 2_000_000_000),
      "an armed (slow-path) reply makes the completion fd readable"
    );
    client.drain_completion();
    client.disarm_async().unwrap();
    let reply = client.try_take().unwrap().unwrap();
    assert_eq!(reply.payload, b"two");

    // Clean shutdown: dropping the client joins the bridge thread; returning proves no hang.
    drop(client);
  }

  /// Shape: the synchronous calls the bridge-beside-a-park test makes, each parking at once.
  #[cfg(target_os = "macos")]
  const PARKED_ROUNDS: u64 = 20;
  /// Shape: each parked call's deadline in that test, in nanoseconds: half a second, far past a woken reply and short
  /// enough that a park slept to it fails the test at once.
  #[cfg(target_os = "macos")]
  const PARKED_DEADLINE_NS: u64 = 500_000_000;

  /// §4.7, D-19: a synchronous call parked beside a running completion bridge is woken by its reply, though the bridge
  /// waits on the same wake word. Do: start the bridge; then twenty times send a request and wait for its reply
  /// synchronously, the client spinning for none of it so it parks, while another thread, once the client is parked,
  /// replies as the daemon. Expect every reply taken within half its deadline, and every wait parked. Before
  /// 2026-11-18 the daemon's one wake per reply went to the bridge or to the parked call, whichever the kernel picked,
  /// and a parked call the bridge's wait beat slept to its deadline: 16 to 19 of 20 did on the Mac Studio.
  #[cfg(target_os = "macos")]
  #[test]
  fn a_synchronous_park_beside_a_running_bridge_is_woken_by_its_reply() {
    let region = ClientRegion::create(
      &unique_name("slates-endpoint-park-beside-bridge"),
      7,
      0,
      geometry(),
    )
    .unwrap();
    let (handoff, len) = region.handoff().unwrap();
    let mut client = ClientEnd::new(ClientRegion::open(&handoff, len).unwrap());
    client.enable_async_completion().unwrap();
    client.set_spin_ns(Some(0));
    let replier = std::thread::spawn(move || {
      let mut daemon = DaemonEnd::new(ClientRegion::open(&handoff, len).unwrap());
      for _ in 0..PARKED_ROUNDS {
        let request = loop {
          if let Some(request) = daemon.try_take().unwrap() {
            break request;
          }
          std::thread::yield_now();
        };
        while daemon
          .region()
          .client_parked()
          .unwrap()
          .load(Ordering::Acquire)
          == 0
        {
          std::thread::yield_now();
        }
        daemon
          .reply(&Slot::inline(request.request, b"ok").unwrap())
          .unwrap();
      }
    });
    let (parks_before, _) = client.park_ratio();
    let deadline = std::time::Duration::from_nanos(PARKED_DEADLINE_NS);
    for round in 0..PARKED_ROUNDS {
      client
        .send(&Slot::inline(round.saturating_add(1), b"hi").unwrap())
        .unwrap();
      let asked = std::time::Instant::now();
      let reply = client.wait(Some(PARKED_DEADLINE_NS)).unwrap();
      let took = asked.elapsed();
      assert_eq!(reply.payload, b"ok");
      assert!(
        took < deadline / 2,
        "round {round}: a parked call took {took:?} of its {deadline:?} deadline: the bridge kept its wake"
      );
    }
    replier.join().unwrap();
    let (parks_after, _) = client.park_ratio();
    assert!(
      parks_after.saturating_sub(parks_before) >= PARKED_ROUNDS,
      "every wait parked: {parks_before} parks before, {parks_after} after"
    );
    drop(region);
  }

  /// On Linux the descriptor an fd-adopting SDK receives (`enable_async_completion_dup`) is a **pipe**
  /// fed by a bridge over the completion eventfd, not the eventfd itself — Node's `net.Socket` refuses
  /// an eventfd (`ERR_INVALID_FD_TYPE`, the bug of 2026-09-16), but adopts a pipe. Python's plain path
  /// keeps the raw eventfd. Do: give the client a real completion eventfd, take the dup; reply to a
  /// disarmed client (fast path) then an armed one (slow path). Expect: the dup is a FIFO (a pipe),
  /// stays quiet for the fast-path reply, becomes readable for the armed one, both replies wait in the
  /// ring, and dropping the client joins the bridge thread (the test returns rather than hanging).
  #[cfg(target_os = "linux")]
  #[test]
  fn the_linux_completion_dup_is_a_pollable_pipe_fed_by_the_eventfd() {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    use rustix::event::{EventfdFlags, eventfd};
    use rustix::fs::{FileType, fstat};

    let region = ClientRegion::create("slates-endpoint-linux-bridge", 7, 0, geometry()).unwrap();
    let (handoff, len) = region.handoff().unwrap();
    let client_region = ClientRegion::open(&handoff, len).unwrap();
    let mut daemon = DaemonEnd::new(region);
    let mut client = ClientEnd::new(client_region);

    // A real completion eventfd, the Linux primitive the rendezvous hands over: the daemon nudges a
    // dup, the client holds the original. The daemon writes it only for a parked client.
    let eventfd = eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK).unwrap();
    let daemon_fd = rustix::io::dup(&eventfd).unwrap();
    daemon.set_completion(Some(daemon_fd));
    client.set_completion(eventfd);

    // The descriptor an fd-adopting SDK is handed: own it so the test closes it (net.Socket would).
    // SAFETY: `enable_async_completion_dup` just returned this live dup fd; own it for the test.
    let dup = unsafe { OwnedFd::from_raw_fd(client.enable_async_completion_dup().unwrap()) };
    let poll_fd = dup.as_raw_fd();
    assert_eq!(
      FileType::from_raw_mode(fstat(&dup).unwrap().st_mode),
      FileType::Fifo,
      "the dup is a pipe (adoptable by Node's net.Socket), not the eventfd it refuses"
    );

    // Fast path: the client is not armed, so the daemon nudges nothing and the bridge stays quiet —
    // the async fast path never wakes the event loop (§4.7).
    client.send(&Slot::inline(1, b"hi").unwrap()).unwrap();
    let request = daemon.try_take().unwrap().unwrap();
    daemon
      .reply(&Slot::inline(request.request, b"one").unwrap())
      .unwrap();
    assert!(
      !poll_readable(poll_fd, 100_000_000),
      "a disarmed (fast-path) reply does not wake the event loop"
    );
    assert_eq!(client.try_take().unwrap().unwrap().payload, b"one");

    // Slow path: the client arms before it would yield, so the daemon nudges the eventfd and the
    // bridge makes the pipe readable for the loop.
    client.arm_async().unwrap();
    client.send(&Slot::inline(2, b"hi").unwrap()).unwrap();
    let request = daemon.try_take().unwrap().unwrap();
    daemon
      .reply(&Slot::inline(request.request, b"two").unwrap())
      .unwrap();
    assert!(
      poll_readable(poll_fd, 2_000_000_000),
      "an armed (slow-path) reply makes the completion pipe readable"
    );
    client.drain_completion();
    client.disarm_async().unwrap();
    assert_eq!(client.try_take().unwrap().unwrap().payload, b"two");

    // Clean shutdown: dropping the client joins the bridge thread; returning proves no hang.
    drop(dup);
    drop(client);
  }
}
