//! The two ends of one client's rings (§4.7 "Protocol", "Wake strategy"): the client end
//! writes requests and waits for replies, spinning for the daemon's published window before
//! parking on the wake word; the daemon end drains requests and writes replies, waking a
//! parked client. Each end owns its sequence cursors; the slots' sequence words carry the
//! protocol, so neither end touches a shared counter on the hot path.

use std::sync::atomic::Ordering;
use std::time::Instant;

use crate::error::IpcError;
use crate::region::ClientRegion;
use crate::slot::{Slot, SlotKind};
use crate::wake;

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
    }
  }

  /// The client's end with the doorbell the rendezvous handed over.
  pub fn with_doorbell(region: ClientRegion, doorbell: crate::rendezvous::Doorbell) -> ClientEnd {
    let mut end = ClientEnd::new(region);
    end.doorbell = Some(doorbell);
    end
  }

  /// The client's end over everything the rendezvous handed over: the region, the doorbell
  /// and the liveness check.
  pub fn connected(connected: crate::rendezvous::Connected) -> ClientEnd {
    let mut end = ClientEnd::new(connected.region);
    end.doorbell = Some(connected.doorbell);
    end.liveness = Some(connected.liveness);
    end
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
    }
    Ok(())
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
