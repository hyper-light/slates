//! The two ends of one client's rings (§4.7 "Protocol", "Wake strategy"): the client end
//! writes requests and waits for replies, spinning for the daemon's published window before
//! parking on the wake word; the daemon end drains requests and writes replies, waking a
//! parked client. Each end owns its sequence cursors; the slots' sequence words carry the
//! protocol, so neither end touches a shared counter on the hot path.

#[cfg(unix)]
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::atomic::Ordering;
use std::time::Instant;

use crate::error::IpcError;
use crate::region::ClientRegion;
use crate::slot::{Slot, SlotKind};
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
  /// Wakes the client had to park for (the spin-to-park ratio's numerator).
  parks: u64,
  /// Replies taken.
  replies: u64,
  /// The completion fd an async SDK event loop polls for reply-readiness (§4.7, D-19): the daemon
  /// makes it readable when it writes a reply to a parked client, so an `asyncio`/`uv_poll` loop wakes
  /// without spinning. `None` for the sync client (it parks on the wake word) and where the platform
  /// has no completion channel yet. The fd is read-drained and the reply taken with [`Self::try_take`].
  #[cfg(unix)]
  completion: Option<OwnedFd>,
  /// The macOS completion bridge (a self-pipe fed by a thread parking on the wake word), started on
  /// demand by [`Self::enable_async_completion`]; `None` for a sync client and until the first async
  /// use. macOS passes no shared completion fd (Mach and named sockets are refused, D-10), so the
  /// async SDK polls this bridge's pipe; Linux uses [`Self::completion`] (the rendezvous eventfd).
  #[cfg(target_os = "macos")]
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
    ClientEnd {
      region,
      doorbell: None,
      liveness: None,
      spin_override_ns: None,
      next_request: 0,
      next_reply: 0,
      parks: 0,
      replies: 0,
      #[cfg(unix)]
      completion: None,
      #[cfg(target_os = "macos")]
      bridge: None,
    }
  }

  /// The client's end with the doorbell the rendezvous handed over.
  pub fn with_doorbell(region: ClientRegion, doorbell: crate::rendezvous::Doorbell) -> ClientEnd {
    let mut end = ClientEnd::new(region);
    end.doorbell = Some(doorbell);
    end
  }

  /// The client's end over everything the rendezvous handed over: the region, the doorbell,
  /// the liveness check, and (where the platform has one) the completion fd an async SDK polls.
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
    #[cfg(target_os = "macos")]
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
    #[cfg(target_os = "macos")]
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

  /// Arms the completion signal before the SDK yields to its event loop: the daemon wakes a parked
  /// client on a reply, making the completion fd readable, and the macOS bridge nudges its pipe only
  /// while armed. This is the same `parked` flag the sync [`Self::wait`] sets; the async path manages
  /// it explicitly because it never calls `wait`. A caller re-checks [`Self::try_take`] right after
  /// arming to close the race with a reply that landed during the spin.
  pub fn arm_async(&self) -> Result<(), IpcError> {
    self.region.client_parked()?.store(1, Ordering::Release);
    Ok(())
  }

  /// Clears the arm ([`Self::arm_async`]) once a reply is taken, so a later fast-path reply costs the
  /// daemon no wake and the bridge no pipe write.
  pub fn disarm_async(&self) -> Result<(), IpcError> {
    self.region.client_parked()?.store(0, Ordering::Release);
    Ok(())
  }

  /// Whether the daemon this end was connected to is gone (dead or restarted); true for an
  /// end that has no liveness check, so a caller reconnects rather than waits forever.
  pub fn daemon_gone(&self) -> bool {
    self.liveness.as_ref().is_none_or(|l| l.daemon_gone())
  }

  /// The region.
  pub fn region(&self) -> &ClientRegion {
    &self.region
  }

  /// The region, mutably (the bulk area).
  pub fn region_mut(&mut self) -> &mut ClientRegion {
    &mut self.region
  }

  /// Chooses the spin window (`None`: the daemon's published one, the measured wake cost).
  pub fn set_spin_ns(&mut self, spin_ns: Option<u64>) {
    self.spin_override_ns = spin_ns;
  }

  /// Parks so far and replies so far (the measured spin-to-park ratio).
  pub fn park_ratio(&self) -> (u64, u64) {
    (self.parks, self.replies)
  }

  /// The ring index the next request takes (its bulk chunk is chosen by it).
  pub fn next_request_index(&self) -> u64 {
    self.next_request
  }

  /// Writes a request slot; `RingFull` when the daemon has not taken the slot the ring wraps
  /// onto (the caller blocks on credit and retries; nothing is dropped). Rings the doorbell
  /// when the daemon's shard is parked.
  pub fn send(&mut self, slot: &Slot) -> Result<(), IpcError> {
    let cmd = self.region.cmd();
    cmd.push(self.region.object_mut(), self.next_request, slot)?;
    self.next_request = self.next_request.wrapping_add(1);
    let parked = self.region.daemon_parked()?.load(Ordering::Acquire) != 0;
    if parked {
      self.region.doorbell()?.fetch_add(1, Ordering::AcqRel);
      if let Some(bell) = &self.doorbell {
        bell.ring()?;
      }
    }
    Ok(())
  }

  /// Takes the next reply if one is there.
  pub fn try_take(&mut self) -> Result<Option<Reply>, IpcError> {
    let cpl = self.region.cpl();
    let Some(slot) = cpl.pop(self.region.object(), self.next_reply)? else {
      return Ok(None);
    };
    self.next_reply = self.next_reply.wrapping_add(1);
    self.replies += 1;
    Ok(Some(Reply {
      request: slot.request,
      kind: slot.kind,
      payload: slot.payload,
    }))
  }

  /// Waits for the next reply: spins for the published window, then parks on the wake word
  /// (setting the parked flag and re-checking the slot to close the race), until the reply
  /// arrives or `deadline_ns` (from the call) passes.
  pub fn wait(&mut self, deadline_ns: Option<u64>) -> Result<Reply, IpcError> {
    let started = Instant::now();
    let spin_ns = self
      .spin_override_ns
      .unwrap_or_else(|| u64::from(self.region.spin_ns()));
    loop {
      if let Some(reply) = self.try_take()? {
        return Ok(reply);
      }
      if elapsed_ns(started) < spin_ns {
        std::hint::spin_loop();
        continue;
      }
      if deadline_ns.is_some_and(|d| elapsed_ns(started) >= d) {
        return Err(IpcError::DeadlineExceeded);
      }
      // Park: the flag first, the word's value, then the re-check that closes the race with
      // a reply written between the last poll and the wait.
      self.region.client_parked()?.store(1, Ordering::Release);
      let expected = self.region.wake_word()?.load(Ordering::Acquire);
      if let Some(reply) = self.try_take()? {
        self.region.client_parked()?.store(0, Ordering::Release);
        return Ok(reply);
      }
      self.parks += 1;
      let remaining = deadline_ns.map(|d| d.saturating_sub(elapsed_ns(started)));
      let woken = wake::wait(self.region.wake_word()?, expected, remaining)?;
      self.region.client_parked()?.store(0, Ordering::Release);
      if !woken && deadline_ns.is_some() {
        if let Some(reply) = self.try_take()? {
          return Ok(reply);
        }
        return Err(IpcError::DeadlineExceeded);
      }
    }
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

  /// Takes the next request if one is there. A hostile slot is released and reported.
  pub fn try_take(&mut self) -> Result<Option<Request>, IpcError> {
    let cmd = self.region.cmd();
    let taken = cmd.pop(self.region.object(), self.next_request);
    if !matches!(taken, Ok(None)) {
      self.next_request = self.next_request.wrapping_add(1);
    }
    let Some(slot) = taken? else {
      return Ok(None);
    };
    Ok(Some(Request {
      request: slot.request,
      kind: slot.kind,
      payload: slot.payload,
    }))
  }

  /// Writes a reply and wakes the client if it parked.
  pub fn reply(&mut self, slot: &Slot) -> Result<(), IpcError> {
    let cpl = self.region.cpl();
    cpl.push(self.region.object_mut(), self.next_reply, slot)?;
    self.next_reply = self.next_reply.wrapping_add(1);
    let word = self.region.wake_word()?;
    word.fetch_add(1, Ordering::AcqRel);
    if self.region.client_parked()?.load(Ordering::Acquire) != 0 {
      wake::wake_one(word)?;
      self.wakes += 1;
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

  /// Marks the daemon's shard parked (true) or polling (false), so the client knows whether
  /// to ring the doorbell.
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

#[cfg(all(test, unix))]
mod tests {
  use std::os::fd::{BorrowedFd, RawFd};
  use std::sync::atomic::Ordering;

  use rustix::net::{AddressFamily, SocketFlags, SocketType};

  use super::{ClientEnd, DaemonEnd};
  use crate::region::{ClientRegion, RegionGeometry};
  use crate::slot::Slot;

  fn geometry() -> RegionGeometry {
    RegionGeometry {
      slots: 8,
      spin_ns: 200_000,
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
    let region = ClientRegion::create("slates-endpoint-completion", 7, 0, geometry()).unwrap();
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
  #[cfg(target_os = "macos")]
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
    let region = ClientRegion::create("slates-endpoint-bridge", 7, 0, geometry()).unwrap();
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
}
