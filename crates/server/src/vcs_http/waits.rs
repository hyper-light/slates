//! Each phase's wait on a connection's client, learned (A-625 part 2; §4.17.2's tunables, "Reaping a stalled client";
//! R3, D-11): how long the HTTP edge waits for a client in each phase of its exchange before the connection is overdue,
//! learned by RFC 6298's estimator from the waits that completed, through [`crate::learned_wait::LearnedWait`], the one
//! rule every learned wait in the daemon keeps (the tail of the completed waits plus a liveness window, Karn's rule, the
//! back-off).
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
//! **Overdue, then reaped when a seat is needed** (main's ruling, 2026-11-19, the rule g294 found for SSH). A wait past
//! its bound does not end its connection: a git client can be silent for long mid-request, compressing a large push or
//! waiting on a credential helper's prompt, and a cut at the bound would fail it and never let it teach the bound. The
//! wait goes on, and the connection is marked overdue ([`Overdue`]) from its deadline. The edge reaps overdue
//! connections, most overdue first, only while every seat is taken and a new connection needs one (`super::serve`). A
//! wait that ends late is still a completed wait and a sample, so a slow client teaches the bound; a reap teaches
//! nothing (Karn's rule) and backs the phase off ([`PhaseWaits::reap`]). So a silent client never holds a seat a new
//! connection needs, and never loses one nobody needs.
//!
//! **The connection's own first measurement, before a phase has learned** (main's ruling, 2026-11-19). Until one of a
//! phase's waits has completed, a wait in it is bounded by RFC 6298 §2.2's first RTO from the connection's own
//! measurement `R`: `SRTT = R`, `RTTVAR = R/2`, so `R + max(G, 4·R/2)` ([`first_rto_ns`]), doubled for each reap the phase
//! has backed off. `R` is the TCP handshake's round trip as the kernel measured it at accept
//! (`slates_rt::tcp::TcpStream::round_trip`). While a TLS handshake runs, the edge's own work on it so far is added: the
//! client's share of a handshake is the same work (a key exchange, a signature checked), and a loopback round trip of
//! microseconds would otherwise mark every first handshake a laptop serves overdue at once. Once the TLS handshake has
//! ended, its whole duration from acceptance is `R`. `G` is the clock's measured granularity: the larger of the shard's
//! timer tick, the lateness its scheduler has shown, and the resolution of the kernel's round-trip clock (a millisecond on
//! macOS). As first landed, A-625 part 2 had no bound before a phase's first completed wait.
//!
//! **Whose waits teach.** Only a connection whose credentials proved a principal (§4.13) teaches the bounds, as only a
//! bearer holder's arrival teaches the MCP edge's (A-523): a connection's waits before its proof are kept, each phase's
//! longest, then folded in at the proof as one sample a phase, and dropped if no proof comes; a reap of a connection that
//! proved no one backs nothing off. So a client without a credential moves no bound either way.
//!
//! **The back-off** has no ceiling of the edge's own, as a hook's has none (RFC 6298 §2.5 lets a maximum be placed and
//! asks for none): a reap doubles the phase's bound and the next completed wait ends the back-off. A reap backs the phase
//! off only when its wait was armed under the bound now in force, as RFC 6298 §5.5 doubles the timer that expired, so
//! connections reaped together under one bound back it off once and the bound grows by one doubling a bound's length at
//! most: no burst of reaps arms a deadline years away.

use slates_http_edge::connection::Phase;
use slates_rt::tcp::RoundTrip;

use crate::learned_wait::LearnedWait;

/// The back-off's ceiling: none of the edge's own (the module doc); the type's largest value.
const NO_CEILING_NS: u64 = u64::MAX;

/// Format: RFC 6298 §2.2's `K`, the deviations an RTO adds to the smoothed round trip (`RTO = SRTT + max(G, K·RTTVAR)`).
const RTO_DEVIATIONS: u64 = 4;
/// Format: RFC 6298 §2.2's first deviation, a share of the first measurement: `RTTVAR <- R/2`.
const FIRST_DEVIATION_DIVISOR: u64 = 2;
/// Format: RFC 6298 §5.5's back-off: each expiry doubles the timer.
const BACKOFF_FACTOR: u64 = 2;

/// Derived: RFC 6298 §2.2's first RTO from one measurement `round_trip_ns` (`SRTT = R`, `RTTVAR = R/2`), its deviation
/// term floored by the clock's granularity `granularity_ns` (`G`): `R + max(G, 4·R/2)`.
pub(crate) fn first_rto_ns(round_trip_ns: u64, granularity_ns: u64) -> u64 {
  let deviation_ns = round_trip_ns
    .checked_div(FIRST_DEVIATION_DIVISOR)
    .unwrap_or(0);
  round_trip_ns.saturating_add(granularity_ns.max(deviation_ns.saturating_mul(RTO_DEVIATIONS)))
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
  /// How many connections were reaped overdue in this phase.
  reaps: u64,
  /// How many back-offs the phase has taken: a wait armed under an earlier count was armed under a bound since doubled,
  /// and its reap backs nothing off again.
  generation: u64,
}

/// Every phase's waits, each learned apart (the module doc): an edge's, shared by its connections on its shard.
#[derive(Debug, Default)]
pub(crate) struct PhaseWaits {
  /// Each phase's.
  learned: PerPhase<Learned>,
}

impl PhaseWaits {
  /// Derived: how long a wait in `phase` may run before its connection is overdue, as learned: none before one of the
  /// phase's waits has completed (then the connection's own first measurement bounds it, [`Tracker::deadline_ns`]); then
  /// the tail of its completed waits plus a liveness window, doubled for each reap backed off since the last completed
  /// one (`LearnedWait::bound_ns`). None for `Closed`.
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

  /// A connection overdue as `overdue` says was reaped: it teaches nothing (Karn's rule: its wait's true length is
  /// unknown), it is counted, and its phase backs off when its credentials proved a principal and its wait was armed
  /// under the bound now in force (the module doc).
  pub(crate) fn reap(&mut self, overdue: &Overdue) {
    let Some(learned) = self.learned.get_mut(overdue.phase) else {
      return;
    };
    learned.reaps = learned.reaps.saturating_add(1);
    if overdue.proven && overdue.armed_generation == learned.generation {
      learned.waits.ran_out();
      learned.generation = learned.generation.saturating_add(1);
    }
  }

  /// How many back-offs `phase` has taken: what a wait armed now is armed under.
  pub(crate) fn generation(&self, phase: Phase) -> u64 {
    self
      .learned
      .get(phase)
      .map_or(0, |learned| learned.generation)
  }

  /// How many of `phase`'s waits completed: its samples, the non-vacuity counter of its learned bound.
  pub(crate) fn samples(&self, phase: Phase) -> u64 {
    self.learned.get(phase).map_or(0, |learned| learned.samples)
  }

  /// How many connections were reaped overdue in `phase`.
  pub(crate) fn reaps(&self, phase: Phase) -> u64 {
    self.learned.get(phase).map_or(0, |learned| learned.reaps)
  }
}

/// Whether `phase` is waited for whole, from its start to its end, rather than a silence at a time (the module doc).
pub(crate) fn whole(phase: Phase) -> bool {
  matches!(
    phase,
    Phase::Handshake | Phase::Idle | Phase::Head | Phase::Drain
  )
}

/// A connection whose wait on its client passed its bound: from when, in which phase, and what a reap of it backs off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Overdue {
  /// The deadline that passed, on the shard's clock: the earliest is the most overdue.
  pub(crate) since_ns: u64,
  /// The phase it waits in.
  phase: Phase,
  /// The back-offs its phase had taken when the wait was armed.
  armed_generation: u64,
  /// Whether the connection's credentials proved a principal.
  proven: bool,
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
  /// The back-offs its phase had taken when the connection's latest wait was armed ([`PhaseWaits::generation`]).
  armed_generation: u64,
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
      armed_generation: 0,
    }
  }

  /// The edge spent `ns` of its own work on the connection's TLS handshake (its key exchange, its signature, its
  /// records): added to `R` while the handshake runs.
  pub(crate) fn worked(&mut self, ns: u64) {
    self.worked_ns = self.worked_ns.saturating_add(ns);
  }

  /// Where a wait in `phase` starting at `now_ns` is past its bound: a whole phase's bound from the phase's start (a whole
  /// phase in progress that is not `phase` ends here, its wait completed), a silence's from now. The bound is the phase's
  /// learned one, or before it has one RFC 6298's first RTO from the connection's own measurement, `granularity_ns` (the
  /// shard clock's `G`) floored by the resolution of the clock that measured it, doubled for each back-off the phase took
  /// ([`first_rto_ns`]).
  pub(crate) fn deadline_ns(
    &mut self,
    phase: Phase,
    now_ns: u64,
    granularity_ns: u64,
    waits: &mut PhaseWaits,
  ) -> u64 {
    self.observe(phase, now_ns, waits);
    let generation = waits.generation(phase);
    self.armed_generation = generation;
    let since_ns = match self.current {
      Some(current) if whole(phase) => current.since_ns,
      _ => now_ns,
    };
    let bound_ns = waits.bound_ns(phase).unwrap_or_else(|| {
      let first_ns = first_rto_ns(
        self.measured_ns.saturating_add(self.worked_ns),
        granularity_ns.max(self.resolution_ns),
      );
      let backoffs = u32::try_from(generation).unwrap_or(u32::MAX);
      first_ns.saturating_mul(BACKOFF_FACTOR.saturating_pow(backoffs))
    });
    since_ns.saturating_add(bound_ns)
  }

  /// What a wait in `phase` past its deadline `deadline_ns` marks its connection: overdue since that deadline, with what a
  /// reap of it backs off (the latest wait's armed back-offs, and whether the connection proved a principal).
  pub(crate) fn overdue(&self, phase: Phase, deadline_ns: u64) -> Overdue {
    Overdue {
      since_ns: deadline_ns,
      phase,
      armed_generation: self.armed_generation,
      proven: self.proven,
    }
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
  /// drain): its wait completed, however late. A TLS handshake's whole duration becomes the connection's first measurement.
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
  /// completed, however late. A whole phase's reads and writes end its wait only when the phase ends.
  pub(crate) fn heard(&mut self, phase: Phase, waited_ns: u64, waits: &mut PhaseWaits) {
    if !whole(phase) {
      self.learn(phase, waited_ns, waits);
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
