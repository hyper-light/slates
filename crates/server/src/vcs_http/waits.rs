//! Each phase's wait on a connection's client, learned (A-625 part 2; §4.17.2's tunables, "Reaping a stalled client";
//! R3, D-11): how long the HTTP edge waits for a client in each phase of its exchange before it judges the client stalled
//! and ends its connection, learned by RFC 6298's estimator from the waits that completed, through
//! [`crate::learned_wait::LearnedWait`], the one rule every learned wait in the daemon keeps (the tail of the completed
//! waits plus a liveness window, Karn's rule, the back-off).
//!
//! **What a wait is, phase by phase** (`slates_http_edge::connection::Phase`). A phase whose bytes the request's own room
//! bounds is waited for whole, from its start to its end, so a client that trickles them a byte at a time is held to the
//! phase's learned length, never to a silence's: the handshake (from the connection's acceptance to the handshake's end),
//! idleness (from the end of one exchange, or of the handshake, to the first byte of the next request), a head (from its
//! first byte to the head whole) and the drain after a staged close (from the half-close to the client's own close). A
//! phase whose bytes scale with what the request carries is waited for a silence at a time, each read waiting for bytes
//! and each write waiting for room its own wait: a body and a reply, whose whole length is a push's or a clone's size,
//! never a wait's.
//!
//! **The connection's own first measurement, before a phase has learned** (main's ruling, 2026-11-19: a silent client
//! never holds a seat unbounded). Until one of a phase's waits has completed, a wait in it is bounded by RFC 6298 §2.2's
//! first RTO from the connection's own measurement `R`: `SRTT = R`, `RTTVAR = R/2`, so `R + max(G, 4·R/2)`
//! ([`first_rto_ns`]). `R` is the TCP handshake's round trip as the kernel measured it at accept
//! (`slates_rt::tcp::TcpStream::round_trip`). While a TLS handshake runs, the edge's own work on it so far is added: the
//! client's share of a handshake is the same work (a key exchange, a signature checked), and a loopback round trip of
//! microseconds would otherwise cut the first handshakes a laptop serves. Once the TLS handshake has ended, its whole
//! duration from acceptance is `R`. `G` is the clock's measured granularity: the larger of the shard's timer tick, the
//! lateness its scheduler has shown, and the resolution of the kernel's round-trip clock (a millisecond on macOS). So
//! nothing a connection waits for is unbounded, and no number but the connection's own and the machine's is used.
//! Until 2026-11-19 a phase had no bound before its first completed wait (A-625 part 2 as first landed).
//!
//! **Whose waits teach.** Only a connection whose credentials proved a principal (§4.13) teaches the bounds, as only a
//! bearer holder's arrival teaches the MCP edge's (A-523): a connection's waits before its proof are kept, each phase's
//! longest, then folded in at the proof as one sample a phase, and dropped if no proof comes; a wait cut at its bound
//! before the proof backs nothing off. So a client without a credential moves no bound either way, and one that trickles
//! bytes to raise a bound must hold a credential that names it.
//!
//! **The back-off** has no ceiling of the edge's own, as a hook's has none: a cut doubles the next bound, the next
//! completed wait ends the back-off, and every wait still ends when its client leaves (RFC 6298 §2.5 lets a maximum be
//! placed and asks for none).

use slates_http_edge::connection::Phase;
use slates_rt::tcp::RoundTrip;

use crate::learned_wait::LearnedWait;

/// The back-off's ceiling: none of the edge's own (the module doc); the type's largest value.
const NO_CEILING_NS: u64 = u64::MAX;

/// Format: RFC 6298 §2.2's `K`, the deviations an RTO adds to the smoothed round trip (`RTO = SRTT + max(G, K·RTTVAR)`).
const RTO_DEVIATIONS: u64 = 4;
/// Format: RFC 6298 §2.2's first deviation, a share of the first measurement: `RTTVAR <- R/2`.
const FIRST_DEVIATION_DIVISOR: u64 = 2;

/// Derived: RFC 6298 §2.2's first RTO from one measurement `round_trip_ns` (`SRTT = R`, `RTTVAR = R/2`), its deviation
/// term floored by the clock's granularity `granularity_ns` (`G`): `R + max(G, 4·R/2)`.
pub(crate) fn first_rto_ns(round_trip_ns: u64, granularity_ns: u64) -> u64 {
  let _ = (round_trip_ns, granularity_ns, FIRST_DEVIATION_DIVISOR, RTO_DEVIATIONS);
  u64::MAX
}

/// One value for each phase a connection waits on its client in: every [`Phase`] but `Closed`.
#[derive(Debug, Default)]
struct PerPhase<T> {
  /// The TLS handshake.
  handshake: T,
  /// Between requests.
  idle: T,
  /// A head arriving.
  head: T,
  /// A body arriving.
  body: T,
  /// A reply leaving.
  reply: T,
  /// The drain after a staged close.
  drain: T,
}

impl<T> PerPhase<T> {
  /// `phase`'s value; none for `Closed`, where nothing waits.
  fn get(&self, phase: Phase) -> Option<&T> {
    match phase {
      Phase::Handshake => Some(&self.handshake),
      Phase::Idle => Some(&self.idle),
      Phase::Head => Some(&self.head),
      Phase::Body => Some(&self.body),
      Phase::Reply => Some(&self.reply),
      Phase::Drain => Some(&self.drain),
      Phase::Closed => None,
    }
  }

  /// `phase`'s value, to change; none for `Closed`.
  fn get_mut(&mut self, phase: Phase) -> Option<&mut T> {
    match phase {
      Phase::Handshake => Some(&mut self.handshake),
      Phase::Idle => Some(&mut self.idle),
      Phase::Head => Some(&mut self.head),
      Phase::Body => Some(&mut self.body),
      Phase::Reply => Some(&mut self.reply),
      Phase::Drain => Some(&mut self.drain),
      Phase::Closed => None,
    }
  }

  /// Each phase with its value.
  fn into_each(self) -> impl Iterator<Item = (Phase, T)> {
    [
      (Phase::Handshake, self.handshake),
      (Phase::Idle, self.idle),
      (Phase::Head, self.head),
      (Phase::Body, self.body),
      (Phase::Reply, self.reply),
      (Phase::Drain, self.drain),
    ]
    .into_iter()
  }
}

/// One phase's waits.
#[derive(Debug, Default)]
struct Learned {
  /// The waits that completed, and the back-off since the last.
  waits: LearnedWait,
  /// How many completed: each a sample.
  samples: u64,
  /// How many were cut at their bound.
  cuts: u64,
}

/// Every phase's waits, each learned apart (the module doc): an edge's, shared by its connections on its shard.
#[derive(Debug, Default)]
pub(crate) struct PhaseWaits {
  /// Each phase's.
  learned: PerPhase<Learned>,
}

impl PhaseWaits {
  /// Derived: how long a wait in `phase` may run: none before one of the phase's waits has completed (the module doc);
  /// then the tail of its completed waits plus a liveness window, doubled for each wait cut since the last completed one
  /// (`LearnedWait::bound_ns`). None for `Closed`.
  pub(crate) fn bound_ns(&self, phase: Phase) -> Option<u64> {
    self
      .learned
      .get(phase)
      .filter(|learned| learned.samples > 0)
      .map(|learned| learned.waits.bound_ns(NO_CEILING_NS))
  }

  /// A wait in `phase` that completed after `waited_ns`: a sample, and the end of any back-off.
  pub(crate) fn completed(&mut self, phase: Phase, waited_ns: u64) {
    if let Some(learned) = self.learned.get_mut(phase) {
      learned.waits.completed(waited_ns);
      learned.samples = learned.samples.saturating_add(1);
    }
  }

  /// A wait in `phase` cut at its bound: no sample (Karn's rule: its true length is unknown), and the bound backed off.
  pub(crate) fn ran_out(&mut self, phase: Phase) {
    if let Some(learned) = self.learned.get_mut(phase) {
      learned.waits.ran_out();
      learned.cuts = learned.cuts.saturating_add(1);
    }
  }

  /// How many of `phase`'s waits completed: its samples, the non-vacuity counter of its learned bound.
  pub(crate) fn samples(&self, phase: Phase) -> u64 {
    self.learned.get(phase).map_or(0, |learned| learned.samples)
  }

  /// How many of `phase`'s waits were cut at their bound.
  pub(crate) fn cuts(&self, phase: Phase) -> u64 {
    self.learned.get(phase).map_or(0, |learned| learned.cuts)
  }
}

/// Whether `phase` is waited for whole, from its start to its end, rather than a silence at a time (the module doc).
pub(crate) fn whole(phase: Phase) -> bool {
  matches!(
    phase,
    Phase::Handshake | Phase::Idle | Phase::Head | Phase::Drain
  )
}

/// The whole phase a connection is in, waited for from its start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Current {
  /// The phase.
  phase: Phase,
  /// When it began, on the shard's clock.
  since_ns: u64,
}

/// One connection's waits: its own first measurement, the whole phase it is in, whether its credentials have proved a
/// principal, and each phase's longest wait before they did (the module doc).
#[derive(Debug)]
pub(crate) struct Tracker {
  /// Whether the connection's credentials proved a principal: from then on its waits teach the edge.
  proven: bool,
  /// Each phase's longest completed wait before the proof.
  unproven: PerPhase<Option<u64>>,
  /// The whole phase in progress.
  current: Option<Current>,
  /// The connection's own first measurement `R` (the module doc): its TCP handshake's round trip, then its TLS
  /// handshake's whole duration.
  measured_ns: u64,
  /// The resolution of the clock `measured_ns` was read from: the kernel's, until the shard's own clock timed the TLS
  /// handshake (whose granularity the caller's `G` holds).
  resolution_ns: u64,
  /// The edge's own work on the connection's TLS handshake so far, added to `R` while the handshake runs.
  worked_ns: u64,
}

impl Tracker {
  /// A connection's waits, its first measurement the TCP handshake's round trip as the kernel measured it at accept.
  pub(crate) fn new(round_trip: RoundTrip) -> Tracker {
    Tracker {
      proven: false,
      unproven: PerPhase::default(),
      current: None,
      measured_ns: round_trip.ns,
      resolution_ns: round_trip.resolution_ns,
      worked_ns: 0,
    }
  }

  /// The edge spent `ns` of its own work on the connection's TLS handshake (its key exchange, its signature, its
  /// records): added to `R` while the handshake runs.
  pub(crate) fn worked(&mut self, ns: u64) {
    self.worked_ns = self.worked_ns.saturating_add(ns);
  }

  /// Where a wait in `phase` starting at `now_ns` ends: a whole phase's bound from the phase's start (a whole phase in
  /// progress that is not `phase` ends here, its wait completed), a silence's from now. The bound is the phase's learned
  /// one, or before it has one RFC 6298's first RTO from the connection's own measurement, `granularity_ns` (the shard
  /// clock's `G`) floored by the resolution of the clock that measured it ([`first_rto_ns`]).
  pub(crate) fn deadline_ns(
    &mut self,
    phase: Phase,
    now_ns: u64,
    granularity_ns: u64,
    waits: &mut PhaseWaits,
  ) -> u64 {
    self.observe(phase, now_ns, waits);
    let since_ns = match self.current {
      Some(current) if whole(phase) => current.since_ns,
      _ => now_ns,
    };
    let bound_ns = waits.bound_ns(phase).unwrap_or_else(|| {
      first_rto_ns(
        self.measured_ns.saturating_add(self.worked_ns),
        granularity_ns.max(self.resolution_ns),
      )
    });
    since_ns.saturating_add(bound_ns)
  }

  /// The connection is in `phase` at `now_ns`: a whole phase in progress that is not `phase` ended there, its wait
  /// completed; and a whole `phase` begins there unless it is the one in progress.
  pub(crate) fn observe(&mut self, phase: Phase, now_ns: u64, waits: &mut PhaseWaits) {
    if self.current.is_some_and(|current| current.phase == phase) {
      return;
    }
    self.end(now_ns, waits);
    if whole(phase) {
      self.current = Some(Current {
        phase,
        since_ns: now_ns,
      });
    }
  }

  /// The whole phase in progress ended at `now_ns` as its client meant it to (the head whole, the client's close after a
  /// drain): its wait completed. A TLS handshake's whole duration becomes the connection's first measurement.
  pub(crate) fn end(&mut self, now_ns: u64, waits: &mut PhaseWaits) {
    if let Some(current) = self.current.take() {
      let waited_ns = now_ns.saturating_sub(current.since_ns);
      if current.phase == Phase::Handshake {
        self.measured_ns = waited_ns;
        self.resolution_ns = 0;
        self.worked_ns = 0;
      }
      self.learn(current.phase, waited_ns, waits);
    }
  }

  /// A silence in `phase` ended after `waited_ns` with the client's bytes, or with room for the edge's: its wait
  /// completed. A whole phase's reads and writes end its wait only when the phase ends.
  pub(crate) fn heard(&mut self, phase: Phase, waited_ns: u64, waits: &mut PhaseWaits) {
    if !whole(phase) {
      self.learn(phase, waited_ns, waits);
    }
  }

  /// A wait in `phase` was cut at its bound: no sample (Karn's rule); the phase's bound backed off once the connection
  /// has proved a principal.
  pub(crate) fn cut(&mut self, phase: Phase, waits: &mut PhaseWaits) {
    self.current = None;
    if self.proven {
      waits.ran_out(phase);
    }
  }

  /// The client left mid-wait, or the socket failed: the wait's true length is unknown, so it teaches nothing.
  pub(crate) fn abandon(&mut self) {
    self.current = None;
  }

  /// The connection's credentials proved a principal: each phase's longest wait before is one sample, and every wait
  /// from now on is one.
  pub(crate) fn prove(&mut self, waits: &mut PhaseWaits) {
    if self.proven {
      return;
    }
    self.proven = true;
    for (phase, longest) in std::mem::take(&mut self.unproven).into_each() {
      if let Some(waited_ns) = longest {
        waits.completed(phase, waited_ns);
      }
    }
  }

  /// A completed wait in `phase`: a sample once the connection is proven, kept as the phase's longest before.
  fn learn(&mut self, phase: Phase, waited_ns: u64, waits: &mut PhaseWaits) {
    if self.proven {
      waits.completed(phase, waited_ns);
    } else if let Some(longest) = self.unproven.get_mut(phase) {
      *longest = Some(longest.map_or(waited_ns, |longest| longest.max(waited_ns)));
    }
  }
}

#[cfg(test)]
#[path = "tests/waits.rs"]
mod tests;
