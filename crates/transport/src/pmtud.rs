//! Datagram packetization-layer path MTU discovery (§4.10a; RFC 8899, as RFC 9000 §14.3 applies it to
//! QUIC): the largest datagram a session sends is found by probing the path, not assumed. A session
//! starts at the floor every QUIC path carries ([`BASE_PLPMTU`], 1,200 bytes) and searches upward with
//! probe packets — ack-eliciting, carrying no stream data, padded to the size under test — up to the
//! largest datagram the peer declared it reads (`crate::params`). An acknowledged probe confirms its
//! size; a size whose probe is lost [`MAX_PROBES`] times, or refused by the local stack at the send
//! (`EMSGSIZE`: the interface's MTU, or macOS's UDP datagram cap), bounds the search from above. The
//! search is a binary search between the confirmed size and that bound, and ends when what is left to
//! gain is no more than one packet's fixed overhead ([`SEARCH_GRANULARITY`]). A completed search is
//! resumed after [`RAISE_TIMER_NS`], because a path's MTU can grow: it rechecks the smallest size that failed
//! before, alone, and reopens the search upward only if that size now crosses — so an unchanged path costs at
//! most [`MAX_PROBES`] lost probes per raise. Restarting the whole search from the peer's limit instead cost
//! 36 lost probes per raise on a floor path, and each lost probe leaves a gap the peer reports as an extra
//! ACK range; on a 64 kbit/s link that raised the ping p99 22 % (measured 2026-09-28, `congestion_bench`).
//!
//! Black holes (RFC 8899 §4.3): a path whose MTU shrank drops every packet above its new size, silently.
//! [`BLACK_HOLE_LOSSES`] consecutive losses of packets above the floor, with no such packet acknowledged in
//! between, fall the session back to the floor and search again, the lost size bounding the search.
//!
//! This module is sans-io and clock-injected, like the connection that drives it: it says which size to
//! probe next and folds in what became of each probe. Loss of a probe is never a congestion signal (RFC
//! 9000 §14.4) — the connection keeps probes out of the controller.

use crate::endpoint::MIN_DATAGRAM_BYTES;

/// Format: RFC 8899 §5.1.2 `BASE_PLPMTU` for QUIC — the floor every QUIC path carries (RFC 9000 §14.1),
/// confirmed by the handshake, which travels in datagrams of this size.
pub const BASE_PLPMTU: usize = MIN_DATAGRAM_BYTES;
/// Format: RFC 8899 §5.1.2 `MAX_PROBES` — how many probes of one size may be lost before the size is
/// taken to exceed the path (the RFC's default).
pub const MAX_PROBES: u32 = 3;
/// Format: RFC 8899 §5.1.1 `PMTU_RAISE_TIMER` — how long a completed search waits before probing for a
/// larger size again (the RFC's default, 600 seconds).
pub const RAISE_TIMER_NS: u64 = 600_000_000_000;
/// Derived: the search ends when the gap between the confirmed size and the smallest size known too large
/// is no more than one packet's fixed overhead — the short header (1 + 8 + 4 bytes) and the AEAD tag (16
/// bytes), 29 bytes. A probe to close a smaller gap costs a whole packet to win less than a packet's own
/// overhead in stream data per packet.
pub const SEARCH_GRANULARITY: usize = crate::endpoint::PACKET_OVERHEAD_BYTES;
/// Derived: [`MAX_PROBES`] — the losses of above-floor packets in a row, with none acknowledged between,
/// that declare a black hole: the same evidence a probe needs before its size is judged too large.
pub const BLACK_HOLE_LOSSES: u32 = MAX_PROBES;

/// The probe in flight: its packet number, its size, and how many probes of that size were lost before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Probe {
  pn: u64,
  size: usize,
}

/// Counted over a session's life: the evidence the tests and the status read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PathMtuStats {
  /// Probes sent.
  pub probes_sent: u64,
  /// Probes acknowledged (sizes confirmed).
  pub probes_acked: u64,
  /// Probes lost.
  pub probes_lost: u64,
  /// Probe sends the local stack refused as too large.
  pub probes_refused: u64,
  /// Black holes detected (falls back to the floor).
  pub black_holes: u64,
}

/// One session's path MTU: the confirmed datagram size and the search for a larger one.
#[derive(Clone, Debug)]
pub struct PathMtu {
  /// The largest datagram confirmed to cross the path (the packetization layer's PLPMTU).
  confirmed: usize,
  /// The smallest datagram known not to cross (lost [`MAX_PROBES`] times, refused locally, or past the
  /// peer's limit — one past it); the search probes strictly between `confirmed` and this.
  too_large: usize,
  /// The largest datagram the peer reads: the ceiling a raise restarts the search from.
  peer_max: usize,
  /// The smallest datagram the local stack refused, once one was: a raise never probes past it again.
  local_cap: Option<usize>,
  /// The probe in flight.
  probe: Option<Probe>,
  /// The size the last probes were for, and how many of them were lost.
  attempts: Option<(usize, u32)>,
  /// When a completed search resumes.
  raise_at: Option<u64>,
  /// A resumed search's recheck: the smallest size that failed before, probed first and alone. Only if it
  /// now crosses does the search reopen up to the peer's limit; a path that has not changed costs at most
  /// [`MAX_PROBES`] lost probes per raise, not a whole search from the top (RFC 8899 §5.3: a raise searches
  /// up from the current size).
  recheck: Option<usize>,
  /// Consecutive losses of above-floor packets with none acknowledged between.
  large_losses: u32,
  stats: PathMtuStats,
}

impl PathMtu {
  /// A session's path MTU, starting at the floor and free to search up to `peer_max` bytes (the peer's
  /// declared largest UDP payload, clamped to at least the floor).
  pub fn new(peer_max: usize) -> PathMtu {
    let peer_max = peer_max.max(BASE_PLPMTU);
    PathMtu {
      confirmed: BASE_PLPMTU,
      too_large: peer_max.saturating_add(1),
      peer_max,
      local_cap: None,
      probe: None,
      attempts: None,
      raise_at: None,
      recheck: None,
      large_losses: 0,
      stats: PathMtuStats::default(),
    }
  }

  /// The largest datagram the session sends now.
  pub fn current(&self) -> usize {
    self.confirmed
  }

  /// What the search has counted.
  pub fn stats(&self) -> PathMtuStats {
    self.stats
  }

  /// Whether the search has converged (nothing is worth probing until the raise timer).
  pub fn searching(&self) -> bool {
    self.too_large.saturating_sub(self.confirmed) > SEARCH_GRANULARITY
  }

  /// The size to probe at `now`, or `None` when a probe is in flight, the search has converged and the
  /// raise timer has not fired, or nothing larger could be tried. A fired raise timer reopens the search up
  /// to the peer's limit (and below any size the local stack refused).
  pub fn next_probe(&mut self, now: u64) -> Option<usize> {
    if self.probe.is_some() {
      return None;
    }
    if let Some(size) = self.recheck {
      return Some(size);
    }
    if !self.searching() {
      match self.raise_at {
        Some(at) if now >= at => {
          self.raise_at = None;
          // The size that failed last is rechecked alone; one past the peer's limit or at a size the local
          // stack refused, nothing larger could cross, and the search stays converged.
          let bound = self.too_large;
          let refused_here = self.local_cap.is_some_and(|cap| bound >= cap);
          if bound > self.peer_max || refused_here {
            return None;
          }
          self.recheck = Some(bound);
          self.attempts = None;
          return Some(bound);
        }
        _ => return None,
      }
    }
    // Retry a size whose probes were lost fewer than MAX_PROBES times; otherwise the midpoint.
    let size = match self.attempts {
      Some((size, lost)) if lost < MAX_PROBES && size > self.confirmed && size < self.too_large => {
        size
      }
      _ => {
        let gap = self.too_large.saturating_sub(self.confirmed);
        self.confirmed.saturating_add(gap / 2)
      }
    };
    Some(size)
  }

  /// A probe of `size` bytes went out as packet `pn`.
  pub fn on_probe_sent(&mut self, pn: u64, size: usize) {
    self.probe = Some(Probe { pn, size });
    if !matches!(self.attempts, Some((attempted, _)) if attempted == size) {
      self.attempts = Some((size, 0));
    }
    self.stats.probes_sent = self.stats.probes_sent.saturating_add(1);
  }

  /// Whether a probe is in flight (at most one ever is).
  pub fn probe_in_flight(&self) -> bool {
    self.probe.is_some()
  }

  /// Whether packet `pn` is the probe in flight.
  pub fn is_probe(&self, pn: u64) -> bool {
    self.probe.is_some_and(|probe| probe.pn == pn)
  }

  /// The probe `pn` was acknowledged at `now`: its size is confirmed. `true` when the size the session
  /// sends grew.
  pub fn on_probe_acked(&mut self, pn: u64, now: u64) -> bool {
    let Some(probe) = self.probe.filter(|probe| probe.pn == pn) else {
      return false;
    };
    self.probe = None;
    self.attempts = None;
    self.stats.probes_acked = self.stats.probes_acked.saturating_add(1);
    let grew = probe.size > self.confirmed;
    if grew {
      self.confirmed = probe.size;
    }
    if self.recheck.take().is_some() {
      // The path grew past its old bound: reopen the search up to the peer's limit (and below any size the
      // local stack refused).
      let ceiling = self.local_cap.unwrap_or(self.peer_max.saturating_add(1));
      self.too_large = ceiling.min(self.peer_max.saturating_add(1));
    }
    self.large_losses = 0;
    self.arm_raise_if_done(now);
    grew
  }

  /// The probe `pn` was declared lost at `now`. After [`MAX_PROBES`] losses of its size, the size bounds
  /// the search from above.
  pub fn on_probe_lost(&mut self, pn: u64, now: u64) {
    let Some(probe) = self.probe.filter(|probe| probe.pn == pn) else {
      return;
    };
    self.probe = None;
    self.stats.probes_lost = self.stats.probes_lost.saturating_add(1);
    let lost = match self.attempts {
      Some((size, lost)) if size == probe.size => lost.saturating_add(1),
      _ => 1,
    };
    if lost >= MAX_PROBES {
      self.too_large = self.too_large.min(probe.size);
      self.attempts = None;
      self.recheck = None;
      self.arm_raise_if_done(now);
    } else {
      self.attempts = Some((probe.size, lost));
    }
  }

  /// The local stack refused to send the probe `pn` as too large (`EMSGSIZE`) at `now`: its size bounds
  /// the search at once, and bounds every later raise too.
  pub fn on_probe_refused(&mut self, pn: u64, now: u64) {
    let Some(probe) = self.probe.filter(|probe| probe.pn == pn) else {
      return;
    };
    self.probe = None;
    self.attempts = None;
    self.stats.probes_refused = self.stats.probes_refused.saturating_add(1);
    self.recheck = None;
    self.too_large = self.too_large.min(probe.size);
    self.local_cap = Some(self.local_cap.map_or(probe.size, |cap| cap.min(probe.size)));
    self.arm_raise_if_done(now);
  }

  /// A packet above the floor (not a probe) was acknowledged: the path carries the confirmed size.
  pub fn on_large_packet_acked(&mut self) {
    self.large_losses = 0;
  }

  /// A packet above the floor (not a probe) was declared lost at `now`. `true` when this makes a black
  /// hole — the session falls back to the floor and searches again below the size that stopped crossing.
  pub fn on_large_packet_lost(&mut self, now: u64) -> bool {
    if self.confirmed <= BASE_PLPMTU {
      return false;
    }
    self.large_losses = self.large_losses.saturating_add(1);
    if self.large_losses < BLACK_HOLE_LOSSES {
      return false;
    }
    self.too_large = self.confirmed;
    self.confirmed = BASE_PLPMTU;
    self.large_losses = 0;
    self.probe = None;
    self.attempts = None;
    self.raise_at = None;
    self.recheck = None;
    self.stats.black_holes = self.stats.black_holes.saturating_add(1);
    self.arm_raise_if_done(now);
    true
  }

  /// Arms the raise timer once the search has converged.
  fn arm_raise_if_done(&mut self, now: u64) {
    if !self.searching() && self.raise_at.is_none() {
      self.raise_at = Some(now.saturating_add(RAISE_TIMER_NS));
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The path's side of the oracle: a probe of `size` crosses when it is no larger than the path MTU,
  /// and is refused at the send when larger than the local cap.
  #[derive(Clone, Copy)]
  struct Path {
    mtu: usize,
    local_cap: usize,
  }

  enum Fate {
    Acked,
    Lost,
    Refused,
  }

  impl Path {
    fn fate(&self, size: usize) -> Fate {
      if size > self.local_cap {
        Fate::Refused
      } else if size <= self.mtu {
        Fate::Acked
      } else {
        Fate::Lost
      }
    }
  }

  /// Drives the search against `path` until it asks for nothing more, returning probes spent.
  fn converge(pmtu: &mut PathMtu, path: Path, now: u64) -> u64 {
    let mut pn = 0u64;
    while let Some(size) = pmtu.next_probe(now) {
      pmtu.on_probe_sent(pn, size);
      match path.fate(size) {
        Fate::Acked => {
          pmtu.on_probe_acked(pn, now);
        }
        Fate::Lost => pmtu.on_probe_lost(pn, now),
        Fate::Refused => pmtu.on_probe_refused(pn, now),
      }
      pn += 1;
      assert!(pn < 10_000, "the search ends");
    }
    pn
  }

  /// The best size the oracle's path allows the session: the path MTU, capped by the local stack and by
  /// the peer's declaration.
  fn best(path: Path, peer_max: usize) -> usize {
    path.mtu.min(path.local_cap).min(peer_max).max(BASE_PLPMTU)
  }

  /// RFC 8899 §5.3 (the search, as an oracle over many paths): for every path MTU, local cap and peer
  /// limit, the search confirms a size the path carries — never more than the best — within
  /// [`SEARCH_GRANULARITY`] of the best, and spends at most `MAX_PROBES` probes per halving of the range.
  #[test]
  fn the_search_converges_within_the_granularity_of_every_path() {
    let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = |bound: usize| {
      seed ^= seed << 13;
      seed ^= seed >> 7;
      seed ^= seed << 17;
      usize::try_from(seed % u64::try_from(bound).unwrap()).unwrap()
    };
    for _ in 0..2_000 {
      let peer_max = BASE_PLPMTU + next(65_527 - BASE_PLPMTU + 1);
      let path = Path {
        mtu: BASE_PLPMTU + next(65_527 - BASE_PLPMTU + 1),
        local_cap: BASE_PLPMTU + next(65_527 - BASE_PLPMTU + 1),
      };
      let mut pmtu = PathMtu::new(peer_max);
      let probes = converge(&mut pmtu, path, 0);
      let target = best(path, peer_max);
      assert!(pmtu.current() <= target, "never more than the path carries");
      assert!(
        target - pmtu.current() <= SEARCH_GRANULARITY,
        "within the granularity: confirmed {} of {target}",
        pmtu.current()
      );
      let halvings = u64::from(usize::BITS - (peer_max - BASE_PLPMTU + 1).leading_zeros()) + 1;
      assert!(
        probes <= halvings * u64::from(MAX_PROBES),
        "{probes} probes for {halvings} halvings"
      );
    }
  }

  /// RFC 8899 §5.1.2: a size is judged too large only after `MAX_PROBES` of its probes are lost — a
  /// single lost probe (congestion, not size) is retried at the same size.
  #[test]
  fn a_size_is_abandoned_only_after_max_probes_losses() {
    let mut pmtu = PathMtu::new(9_000);
    let size = pmtu.next_probe(0).unwrap();
    for attempt in 0..MAX_PROBES {
      assert_eq!(
        pmtu.next_probe(0),
        Some(size),
        "attempt {attempt} retries the size"
      );
      pmtu.on_probe_sent(u64::from(attempt), size);
      pmtu.on_probe_lost(u64::from(attempt), 0);
    }
    assert!(
      pmtu.next_probe(0).unwrap() < size,
      "after MAX_PROBES losses the search moves below the size"
    );
    assert_eq!(pmtu.stats().probes_lost, u64::from(MAX_PROBES));
  }

  /// RFC 8899 §4.3: after the path's MTU shrinks, `BLACK_HOLE_LOSSES` consecutive losses of above-floor
  /// packets fall the session to the floor, and the new search converges below the size that stopped
  /// crossing; an acknowledgement between losses resets the count.
  #[test]
  fn a_black_hole_falls_back_to_the_floor_and_searches_again() {
    let mut pmtu = PathMtu::new(9_000);
    converge(
      &mut pmtu,
      Path {
        mtu: 9_000,
        local_cap: 65_527,
      },
      0,
    );
    assert!(pmtu.current() > 8_900);
    assert!(!pmtu.on_large_packet_lost(1));
    pmtu.on_large_packet_acked();
    for loss in 1..BLACK_HOLE_LOSSES {
      assert!(!pmtu.on_large_packet_lost(u64::from(loss)), "not yet");
    }
    assert!(
      pmtu.on_large_packet_lost(5),
      "the third consecutive loss is a black hole"
    );
    assert_eq!(pmtu.current(), BASE_PLPMTU);
    assert_eq!(pmtu.stats().black_holes, 1);
    converge(
      &mut pmtu,
      Path {
        mtu: 1_500,
        local_cap: 65_527,
      },
      5,
    );
    assert!(pmtu.current() <= 1_500 && 1_500 - pmtu.current() <= SEARCH_GRANULARITY);
  }

  /// RFC 8899 §5.1.1: a converged search waits `RAISE_TIMER_NS`, then searches again up to the peer's limit
  /// and finds a path that grew — but never past a size the local stack refused.
  #[test]
  fn the_raise_timer_finds_a_path_that_grew_but_not_past_the_local_cap() {
    let mut pmtu = PathMtu::new(9_000);
    converge(
      &mut pmtu,
      Path {
        mtu: 1_500,
        local_cap: 4_000,
      },
      0,
    );
    let first = pmtu.current();
    assert!(first <= 1_500);
    assert_eq!(
      pmtu.next_probe(RAISE_TIMER_NS - 1),
      None,
      "quiet until the raise timer"
    );
    converge(
      &mut pmtu,
      Path {
        mtu: 9_000,
        local_cap: 4_000,
      },
      RAISE_TIMER_NS,
    );
    assert!(pmtu.current() > first, "the grown path is found");
    assert!(
      pmtu.current() <= 4_000 && 4_000 - pmtu.current() <= SEARCH_GRANULARITY,
      "bounded by the local cap: {}",
      pmtu.current()
    );
  }

  /// RFC 8899 §5.3: a raise on a path that has not changed rechecks the size that failed before and nothing
  /// else — at most `MAX_PROBES` probes, all at that size — and the search stays where it was.
  #[test]
  fn a_raise_on_an_unchanged_path_costs_at_most_max_probes() {
    let path = Path {
      mtu: BASE_PLPMTU,
      local_cap: 65_527,
    };
    let mut pmtu = PathMtu::new(65_527);
    converge(&mut pmtu, path, 0);
    let confirmed = pmtu.current();
    let sent_before = pmtu.stats().probes_sent;
    let first = pmtu.next_probe(RAISE_TIMER_NS).unwrap();
    assert!(
      first - confirmed <= SEARCH_GRANULARITY + 1,
      "the recheck is the old bound: {first}"
    );
    pmtu.on_probe_sent(1_000, first);
    pmtu.on_probe_lost(1_000, RAISE_TIMER_NS);
    let rest = converge(&mut pmtu, path, RAISE_TIMER_NS);
    assert_eq!(pmtu.current(), confirmed);
    let spent = pmtu.stats().probes_sent - sent_before;
    assert!(
      spent <= u64::from(MAX_PROBES),
      "{spent} probes for a raise ({rest} after the first)"
    );
    assert_eq!(
      pmtu.next_probe(RAISE_TIMER_NS + 1),
      None,
      "quiet until the next raise"
    );
  }

  /// A peer that declares no more than the floor leaves nothing to search; a stale probe number's outcome
  /// changes nothing.
  #[test]
  fn nothing_is_probed_past_the_peers_floor_and_stale_outcomes_are_ignored() {
    let mut pmtu = PathMtu::new(BASE_PLPMTU);
    assert_eq!(pmtu.next_probe(0), None);
    let mut searching = PathMtu::new(9_000);
    let size = searching.next_probe(0).unwrap();
    searching.on_probe_sent(7, size);
    assert!(
      !searching.on_probe_acked(8, 0),
      "another packet number is not the probe"
    );
    searching.on_probe_lost(9, 0);
    assert_eq!(searching.current(), BASE_PLPMTU);
    assert!(searching.is_probe(7));
  }
}
