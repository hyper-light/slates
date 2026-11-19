//! Each phase's wait on an SSH connection's client, learned (A-651; §4.17.2's tunables, "Reaping a stalled client";
//! R3, D-11): how long the SSH edge waits for a client in each phase of its connection before the connection is overdue,
//! learned by RFC 6298's estimator from the waits that completed, through [`crate::learned_wait::LearnedWait`], the one
//! rule every learned wait in the daemon keeps. The rule is the HTTP edge's (`crate::vcs_http`'s waits, A-625 part 2,
//! main's rulings of 2026-11-19) over SSH's phases.
//!
//! **The phases** ([`Phase`]). The handshake runs from the connection's acceptance to the end of its first key exchange
//! (the identification lines, the KEXINITs, the exchange and both NEWKEYS); authentication from there to the proof of a
//! principal; setup from there to the command's `exec`; then the command runs; and closing runs from the command's exit
//! to the channel's close. Every phase but the command's has bytes a protocol fixes, so it is waited for whole, from its
//! start to its end, and a client that trickles them is held to the phase's learned length. The command's bytes scale
//! with what it carries (a clone's pack, a push's), so it is waited for a silence at a time: a read the command needs
//! the client's bytes for, or a write that waits for the client's room. A silence keeps its start across the session's
//! own timer (a rekey by time), which interrupts the read but not the client's silence.
//!
//! **Overdue, then reaped when a seat is needed.** A wait past its bound does not end its connection: an SSH client can
//! be silent for long while a person types a key's passphrase after the server's `PK_OK`, confirms an unknown host key
//! before its NEWKEYS, or while its push compresses. The wait goes on, and the connection is marked overdue
//! ([`Overdue`]) from its deadline; the edge reaps overdue connections, most overdue first, only while every seat is taken
//! and a new connection needs one (`super::serve`). A wait that ends late is still a sample, so a slow client teaches the
//! bound; a reap teaches nothing (Karn's rule) and backs the phase off ([`PhaseWaits::reap`]).
//!
//! **The connection's own first measurement, before a phase has learned.** Until one of a phase's waits has completed, a
//! wait in it is bounded by RFC 6298 §2.2's first RTO from the connection's own measurement `R`
//! (`crate::vcs_http`'s `first_rto_ns`, `R + max(G, 4·R/2)`), doubled for each reap the phase has backed off. `R` is the
//! TCP handshake's round trip as the kernel measured it at accept (`slates_rt::tcp::TcpStream::round_trip`), the edge's
//! own work on the SSH handshake so far added while it runs (its key exchange and its signature: the client's share is
//! the same work); once the handshake has ended, its whole duration from acceptance is `R`. `G` is the clock's measured
//! granularity: the shard's timer tick, the lateness its scheduler has shown, or the kernel round-trip clock's resolution.
//!
//! **Whose waits teach.** Only a connection whose key proved a principal (§4.13) teaches the bounds: its waits before the
//! proof are kept, each phase's longest, and folded in at the proof as one sample a phase, dropped if no proof comes; a
//! reap of a connection that proved no one backs nothing off. A reap backs a phase off only when its wait was armed under
//! the bound now in force (RFC 6298 §5.5 doubles the timer that expired), so connections reaped together back it off once.

use slates_rt::tcp::RoundTrip;

use crate::learned_wait::LearnedWait;
use crate::vcs_http::waits::first_rto_ns;

/// The back-off's ceiling: none of the edge's own (as the HTTP edge's, and a hook's); the type's largest value.
const NO_CEILING_NS: u64 = u64::MAX;

/// Format: RFC 6298 §5.5's back-off: each expiry doubles the timer.
const BACKOFF_FACTOR: u64 = 2;

/// A phase of an SSH connection (the module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
  /// From acceptance to the first key exchange's end.
  Handshake,
  /// From there to the proof of a principal.
  Authentication,
  /// From there to the command's `exec`.
  Setup,
  /// The command running: its silences.
  Running,
  /// From the command's exit to the channel's close.
  Closing,
}

/// Every phase, in a connection's order.
pub(crate) const PHASES: [Phase; 5] = [
  Phase::Handshake,
  Phase::Authentication,
  Phase::Setup,
  Phase::Running,
  Phase::Closing,
];

/// Whether `phase` is waited for whole, from its start to its end, rather than a silence at a time (the module doc).
pub(crate) fn whole(phase: Phase) -> bool {
  phase != Phase::Running
}

/// One value for each phase.
#[derive(Debug, Default)]
struct PerPhase<T> {
  /// The handshake's.
  handshake: T,
  /// Authentication's.
  authentication: T,
  /// Setup's.
  setup: T,
  /// The running command's.
  running: T,
  /// Closing's.
  closing: T,
}

impl<T> PerPhase<T> {
  /// `phase`'s value.
  fn get(&self, phase: Phase) -> &T {
    match phase {
      Phase::Handshake => &self.handshake,
      Phase::Authentication => &self.authentication,
      Phase::Setup => &self.setup,
      Phase::Running => &self.running,
      Phase::Closing => &self.closing,
    }
  }

  /// `phase`'s value, to change.
  fn get_mut(&mut self, phase: Phase) -> &mut T {
    match phase {
      Phase::Handshake => &mut self.handshake,
      Phase::Authentication => &mut self.authentication,
      Phase::Setup => &mut self.setup,
      Phase::Running => &mut self.running,
      Phase::Closing => &mut self.closing,
    }
  }

  /// Each phase with its value.
  fn into_each(self) -> impl Iterator<Item = (Phase, T)> {
    [
      (Phase::Handshake, self.handshake),
      (Phase::Authentication, self.authentication),
      (Phase::Setup, self.setup),
      (Phase::Running, self.running),
      (Phase::Closing, self.closing),
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

/// Every phase's waits, each learned apart: an edge's, shared by its connections on its shard.
#[derive(Debug, Default)]
pub(crate) struct PhaseWaits {
  /// Each phase's.
  learned: PerPhase<Learned>,
}

impl PhaseWaits {
  /// Derived: how long a wait in `phase` may run before its connection is overdue, as learned: none before one of the
  /// phase's waits has completed (then the connection's own first measurement bounds it, [`Tracker::deadline_ns`]); then
  /// the tail of its completed waits plus a liveness window, doubled for each reap backed off since the last completed
  /// one (`LearnedWait::bound_ns`).
  pub(crate) fn bound_ns(&self, phase: Phase) -> Option<u64> {
    let learned = self.learned.get(phase);
    (learned.samples > 0).then(|| learned.waits.bound_ns(NO_CEILING_NS))
  }

  /// A wait in `phase` that completed after `waited_ns`: a sample, and the end of any back-off.
  pub(crate) fn completed(&mut self, phase: Phase, waited_ns: u64) {
    let learned = self.learned.get_mut(phase);
    learned.waits.completed(waited_ns);
    learned.samples = learned.samples.saturating_add(1);
  }

  /// A connection overdue as `overdue` says was reaped: it teaches nothing (Karn's rule), it is counted, and its phase
  /// backs off when its key proved a principal and its wait was armed under the bound now in force (the module doc).
  pub(crate) fn reap(&mut self, overdue: &Overdue) {
    let learned = self.learned.get_mut(overdue.phase);
    learned.reaps = learned.reaps.saturating_add(1);
    if overdue.proven && overdue.armed_generation == learned.generation {
      learned.waits.ran_out();
      learned.generation = learned.generation.saturating_add(1);
    }
  }

  /// How many back-offs `phase` has taken: what a wait armed now is armed under.
  pub(crate) fn generation(&self, phase: Phase) -> u64 {
    self.learned.get(phase).generation
  }

  /// How many of `phase`'s waits completed: the non-vacuity counter of its learned bound.
  pub(crate) fn samples(&self, phase: Phase) -> u64 {
    self.learned.get(phase).samples
  }

  /// How many connections were reaped overdue in `phase`.
  pub(crate) fn reaps(&self, phase: Phase) -> u64 {
    self.learned.get(phase).reaps
  }
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
  /// Whether the connection's key proved a principal.
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

/// One connection's waits: its own first measurement, the whole phase it is in or the silence it waits through, whether
/// its key has proved a principal, and each phase's longest wait before it did (the module doc).
#[derive(Debug)]
pub(crate) struct Tracker {
  /// Whether the connection's key proved a principal: from then on its waits teach the edge.
  proven: bool,
  /// Each phase's longest completed wait before the proof.
  unproven: PerPhase<Option<u64>>,
  /// The whole phase in progress.
  current: Option<Current>,
  /// The running command's silence in progress: when it began.
  silence_since_ns: Option<u64>,
  /// The connection's own first measurement `R`: its TCP handshake's round trip, then its SSH handshake's whole
  /// duration.
  measured_ns: u64,
  /// The resolution of the clock `measured_ns` was read from: the kernel's, until the shard's own clock timed the SSH
  /// handshake.
  resolution_ns: u64,
  /// The edge's own work on the connection's SSH handshake so far, added to `R` while the handshake runs.
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
      silence_since_ns: None,
      measured_ns: round_trip.ns,
      resolution_ns: round_trip.resolution_ns,
      worked_ns: 0,
      armed_generation: 0,
    }
  }

  /// The edge spent `ns` of its own work on the connection's SSH handshake: added to `R` while the handshake runs.
  pub(crate) fn worked(&mut self, ns: u64) {
    if self
      .current
      .is_some_and(|current| current.phase == Phase::Handshake)
    {
      self.worked_ns = self.worked_ns.saturating_add(ns);
    }
  }

  /// Where a wait in `phase` starting at `now_ns` is past its bound: a whole phase's bound from the phase's start (a
  /// whole phase in progress that is not `phase` ends here, its wait completed); a silence's from the silence's start,
  /// which this call begins if none is in progress. The bound is the phase's learned one, or before it has one RFC
  /// 6298's first RTO from the connection's own measurement, `granularity_ns` (the shard clock's `G`) floored by the
  /// resolution of the clock that measured it, doubled for each back-off the phase took.
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
      _ => *self.silence_since_ns.get_or_insert(now_ns),
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
  /// reap of it backs off.
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
      self.silence_since_ns = None;
      self.current = Some(Current {
        phase,
        since_ns: now_ns,
      });
    }
  }

  /// The whole phase in progress ended at `now_ns` as its client meant it to: its wait completed, however late. The SSH
  /// handshake's whole duration becomes the connection's first measurement.
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

  /// The running command's silence ended at `now_ns` with the client's bytes or room: its wait completed, however late.
  /// A whole phase's reads and writes end its wait only when the phase ends.
  pub(crate) fn heard(&mut self, now_ns: u64, waits: &mut PhaseWaits) {
    if let Some(since_ns) = self.silence_since_ns.take() {
      self.learn(Phase::Running, now_ns.saturating_sub(since_ns), waits);
    }
  }

  /// The client left mid-wait, or the socket failed: the wait's true length is unknown, so it teaches nothing.
  pub(crate) fn abandon(&mut self) {
    self.current = None;
    self.silence_since_ns = None;
  }

  /// The connection's key proved a principal: each phase's longest wait before is one sample, and every wait from now
  /// on is one.
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

  /// Whether the connection's key proved a principal.
  pub(crate) fn proven(&self) -> bool {
    self.proven
  }

  /// A completed wait in `phase`: a sample once the connection is proven, kept as the phase's longest before.
  fn learn(&mut self, phase: Phase, waited_ns: u64, waits: &mut PhaseWaits) {
    if self.proven {
      waits.completed(phase, waited_ns);
    } else {
      let longest = self.unproven.get_mut(phase);
      *longest = Some(longest.map_or(waited_ns, |longest| longest.max(waited_ns)));
    }
  }
}

#[cfg(test)]
#[path = "tests/waits.rs"]
mod tests;
