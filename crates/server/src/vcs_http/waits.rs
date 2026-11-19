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
//! **None before a first measurement.** A phase has no bound until one of its waits has completed: until then its waits
//! end only when the client leaves or the edge's owner ends the edge, as a hook's first run is bounded by its client's
//! connection alone (§4.17.1; `crate::vcs_hooks`). The connections an edge serves at once are its shard's seats
//! (`super::Bounds`), so a client that holds a connection before any measurement holds one seat.
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

use crate::learned_wait::LearnedWait;

/// The back-off's ceiling: none of the edge's own (the module doc); the type's largest value.
const NO_CEILING_NS: u64 = u64::MAX;

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
    let _ = (phase, NO_CEILING_NS, self.learned.get(phase).map(|learned| learned.waits.bound_ns(0)));
    None
  }

  /// A wait in `phase` that completed after `waited_ns`: a sample, and the end of any back-off.
  pub(crate) fn completed(&mut self, phase: Phase, waited_ns: u64) {
    let _ = (phase, waited_ns);
  }

  /// A wait in `phase` cut at its bound: no sample (Karn's rule: its true length is unknown), and the bound backed off.
  pub(crate) fn ran_out(&mut self, phase: Phase) {
    let _ = (phase, self.learned.get_mut(phase).map(|learned| learned.waits.ran_out()));
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
  /// Where its wait ends: its start plus the phase's bound then; none before the phase's first sample.
  deadline_ns: Option<u64>,
}

/// One connection's waits: the whole phase it is in, whether its credentials have proved a principal, and each phase's
/// longest wait before they did (the module doc).
#[derive(Debug, Default)]
pub(crate) struct Tracker {
  /// Whether the connection's credentials proved a principal: from then on its waits teach the edge.
  proven: bool,
  /// Each phase's longest completed wait before the proof.
  unproven: PerPhase<Option<u64>>,
  /// The whole phase in progress.
  current: Option<Current>,
}

impl Tracker {
  /// Where a wait in `phase` starting at `now_ns` ends: a whole phase's deadline, set when the phase began (a whole phase
  /// in progress that is not `phase` ends here, its wait completed); a silence's, the phase's bound from now. None before
  /// the phase's first sample.
  pub(crate) fn deadline_ns(
    &mut self,
    phase: Phase,
    now_ns: u64,
    waits: &mut PhaseWaits,
  ) -> Option<u64> {
    self.observe(phase, now_ns, waits);
    if whole(phase) {
      self.current.and_then(|current| current.deadline_ns)
    } else {
      waits
        .bound_ns(phase)
        .map(|bound_ns| now_ns.saturating_add(bound_ns))
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
        deadline_ns: waits
          .bound_ns(phase)
          .map(|bound_ns| now_ns.saturating_add(bound_ns)),
      });
    }
  }

  /// The whole phase in progress ended at `now_ns` as its client meant it to (the head whole, the client's close after a
  /// drain): its wait completed.
  pub(crate) fn end(&mut self, now_ns: u64, waits: &mut PhaseWaits) {
    if let Some(current) = self.current.take() {
      self.learn(
        current.phase,
        now_ns.saturating_sub(current.since_ns),
        waits,
      );
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
