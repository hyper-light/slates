//! The SWIM failure detector (§4.8, Part 4 "Cluster plane") — the protocol-period machine that probes
//! members and drives the [`Membership`] view from alive to suspect to dead. Sans-io and driven by
//! `tick` (one protocol period per call) and message events, so it is deterministic and oracle-tested
//! at N=1 before any timer or datagram is involved; the caller sends the [`Ping`]/[`Ack`] it returns
//! over the control plane and feeds received messages back in.
//!
//! Evidence: SWIM (Das/Gupta/Motivala, DSN 2002) and Lifeguard (Dadgar/Phillips/Currey, DSN 2018),
//! tier A. Built: **direct probing** (each period pings the next member round-robin; an unanswered
//! probe suspects, a suspicion held for the window kills); the **indirect probe** (a ping-request
//! through `k` peers, so a lost packet is not a failure); **infection-style gossip** (each membership
//! change piggybacks on ping/ack a bounded number of times, spreading the view); and the **Lifeguard
//! local-health multiplier** (a bounded score, raised when the node's own probes fail or it is falsely
//! suspected and lowered when they succeed, that dilates the suspicion window — and, through
//! [`health_multiplier`](Detector::health_multiplier), the caller's probe cadence — so a node that
//! itself looks unhealthy is slower to declare others dead); and the **confirmation-count suspicion
//! timeout** `max − (max−min)·log(C+1)/log(K+1)` (the window shrinks from the base toward its floor as
//! `C` distinct peers independently suspect a member — corroboration declares a real failure sooner while
//! a lone suspicion waits the full window; the logarithm is computed in deterministic fixed point,
//! [`crate::fixed`]). The two window mechanisms compose: corroboration shortens it, local ill-health
//! lengthens it. And **randomized probe order** (SWIM §4): each round probes a fresh shuffled permutation
//! of the members rather than a fixed rotation, so every member is probed once per round and the
//! worst-case wait for a first probe is one round; the shuffle is a deterministic xorshift seeded from
//! the node id, so the simulation stays reproducible. This completes the §4.8 membership mechanism set.

use std::collections::{BTreeMap, BTreeSet};

use slates_db::register::HostId;

use crate::membership::{Change, Liveness, MemberState, Membership};

/// A ping to send to `to` — probe it this period.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ping {
  /// The member to probe.
  pub to: HostId,
}

/// An acknowledgement to send to `to` — the reply to a received [`Ping`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ack {
  /// The member that pinged us.
  pub to: HostId,
}

/// A ping-request: ask `relay` to ping `target` on our behalf and relay the acknowledgement back. SWIM
/// sends these to a few peers when a direct ping goes unanswered, so a single lost packet — rather than
/// a failed member — does not cause a false suspicion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PingReq {
  /// The peer asked to probe on our behalf.
  pub relay: HostId,
  /// The member to probe indirectly.
  pub target: HostId,
}

/// The derived SWIM/Lifeguard timing parameters (§4.8 "Derived constants") — each measured from the
/// fleet, never a hidden constant. The caller derives them and hands them in.
#[derive(Clone, Copy, Debug)]
pub struct DetectorTiming {
  /// The base suspicion window: periods a member stays suspected, at full local health, before it is
  /// declared dead. Derived from RTT p99 × k. Dilated by the local-health multiplier (an unhealthy node
  /// waits longer).
  pub suspicion_periods: u32,
  /// How many times each membership change is disseminated by gossip — SWIM's `λ·ln(n+1)` infection
  /// bound. Derived from the measured convergence and fleet size.
  pub gossip_transmits: u32,
  /// The cap on the Lifeguard local-health multiplier: the most the suspicion window (and the caller's
  /// probe cadence) may be dilated when the local node itself looks unhealthy. The effective multiplier
  /// is `health + 1` up to `health_max + 1`, so this is the cap minus one.
  ///
  /// Derived: keep it **small** — 2 or 3, a cap of 3×–4×. The raw Lifeguard formula `(LHM + 1)` at the
  /// paper's saturation `S = 8` reaches 9×, which pushes suspicion timers and probe timeouts off the
  /// cliff under sustained probe failure; hyperscale's SWIM measured this and softened its multiplier to
  /// `1 + score × 0.25` (a 3× cap at S = 8). This integer form reaches the same modest cap by deriving a
  /// small `health_max` instead of a fractional weight — determinism-clean, and matching the design's
  /// "bounded local-health multiplier" (§4.8). Anchor: hyperscale `local_health_multiplier.py`
  /// (`get_multiplier`), the Backpressure & Degradation table (NORMAL 1× … CRITICAL 3×).
  pub health_max: u32,
  /// The floor of the suspicion window: the fewest periods a member stays suspected even when its
  /// failure is fully corroborated, from the confirmation-count timeout `max − (max−min)·log(C+1)/log(K+1)`
  /// (§4.8). Set equal to `suspicion_periods` to switch the confirmation curve off. Derived from RTT.
  pub suspicion_min: u32,
  /// The number of independent suspicions (`K`) at which the suspicion window reaches its floor — the
  /// count of confirming peers that makes a failure certain enough to declare sooner. Derived from the
  /// indirect-probe fan-out.
  pub confirmations_expected: u32,
}

/// The failure detector for one node: it owns the node's [`Membership`] view, a cursor over the current
/// randomized probe permutation, the member being probed this period and whether it has acknowledged,
/// how many periods each suspected member has gone unrefuted, and its Lifeguard **local health** — a
/// bounded multiplier, raised when the node's own probes fail (or it is falsely suspected) and lowered
/// when they succeed, that dilates the suspicion window so a node that itself looks unhealthy is slower
/// to declare others dead. A member suspected for `suspicion_periods × (health + 1)` periods is dead.
pub struct Detector {
  membership: Membership,
  local: HostId,
  order: Vec<HostId>,
  cursor: usize,
  probing: Option<HostId>,
  acked: bool,
  suspicion: BTreeMap<HostId, u32>,
  confirmations: BTreeMap<HostId, BTreeSet<HostId>>,
  gossip: BTreeMap<HostId, (MemberState, u32)>,
  health: u32,
  shuffler: RandomizedOrder,
  timing: DetectorTiming,
}

/// A deterministic pseudo-random order over the members to probe (SWIM §4): each protocol period probes
/// the next member of a shuffled permutation, and a fresh permutation is drawn each round, so every
/// member is probed once per round and the worst-case time to first probe is one round — not the
/// unbounded wait a fixed rotation can suffer. The generator is a deterministic xorshift seeded from the
/// node id, so the simulation replays failure histories identically (the timing must be portable).
struct RandomizedOrder {
  state: u64,
}

/// Format: the golden-ratio odd constant (2^64 / φ), the standard seed mixer, so distinct node ids seed
/// visibly different sequences.
const SEED_MIXER: u64 = 0x9E37_79B9_7F4A_7C15;
/// Format: Marsaglia's xorshift64 shift triple (`Xorshift RNGs`, 2003) — the three shifts of the
/// full-period 64-bit generator.
const XORSHIFT_TRIPLE: [u32; 3] = [13, 7, 17];

impl RandomizedOrder {
  /// A generator seeded from `local`, never zero (xorshift stays at zero forever from a zero seed).
  fn seeded(local: HostId) -> RandomizedOrder {
    RandomizedOrder {
      state: (local.0 ^ SEED_MIXER) | 1,
    }
  }

  /// The next pseudo-random word (xorshift64).
  fn next(&mut self) -> u64 {
    let mut x = self.state;
    x ^= x << XORSHIFT_TRIPLE[0];
    x ^= x >> XORSHIFT_TRIPLE[1];
    x ^= x << XORSHIFT_TRIPLE[2];
    self.state = x;
    x
  }

  /// Shuffles `items` in place with a Fisher–Yates pass driven by the generator.
  fn shuffle(&mut self, items: &mut [HostId]) {
    let len = items.len();
    for index in (1..len).rev() {
      let span = u64::try_from(index).unwrap_or(0).saturating_add(1);
      let pick = usize::try_from(self.next() % span).unwrap_or(0);
      items.swap(index, pick);
    }
  }
}

impl Detector {
  /// A detector for `local` under the derived [`DetectorTiming`]. The node starts at full health
  /// (multiplier zero); a member is declared dead after it has been suspected for
  /// `suspicion_periods × (health + 1)` protocol periods.
  pub fn new(local: HostId, timing: DetectorTiming) -> Detector {
    Detector {
      membership: Membership::new(local),
      local,
      order: Vec::new(),
      cursor: 0,
      probing: None,
      acked: false,
      suspicion: BTreeMap::new(),
      confirmations: BTreeMap::new(),
      gossip: BTreeMap::new(),
      health: 0,
      shuffler: RandomizedOrder::seeded(local),
      timing,
    }
  }

  /// The Lifeguard local-health multiplier, `health + 1` — how much the node's own timing is dilated
  /// because it looks unhealthy. One at full health. The caller multiplies its probe-period timer by
  /// this so a degraded node also probes less aggressively (the design's dilation of the probe cadence,
  /// which is the caller's clock; the suspicion window is dilated internally).
  pub fn health_multiplier(&self) -> u32 {
    self.health.saturating_add(1)
  }

  /// Raises the local-health multiplier toward its cap — the node just looked unhealthy (a probe it
  /// sent went wholly unanswered, or it had to refute a suspicion about itself).
  fn worsen_health(&mut self) {
    self.health = self.health.saturating_add(1).min(self.timing.health_max);
  }

  /// Lowers the local-health multiplier toward zero — the node just looked healthy (a probe succeeded).
  fn improve_health(&mut self) {
    self.health = self.health.saturating_sub(1);
  }

  /// The effective suspicion window for a member with `confirmations` independent suspicions this period
  /// (§4.8): the confirmation-count timeout `max − (max−min)·log(C+1)/log(K+1)` — shrinking from the base
  /// window toward the floor as independent peers corroborate the failure — then dilated by the Lifeguard
  /// local-health multiplier (a node that itself looks unhealthy waits longer before declaring a peer
  /// dead). The two mechanisms compose: corroboration shortens the wait, local ill-health lengthens it.
  fn suspicion_window(&self, confirmations: u64) -> u32 {
    let corroborated = crate::fixed::suspicion_window(
      confirmations,
      self.timing.suspicion_periods,
      self.timing.suspicion_min,
      self.timing.confirmations_expected,
    );
    corroborated.saturating_mul(self.health_multiplier())
  }

  /// Records that `by` — a member *other* than this node's own originating probe — independently
  /// suspects `subject`, a distinct confirmation that shortens `subject`'s window through the
  /// confirmation-count timeout. The originator (this node, when its own probe started the suspicion) is
  /// never recorded here: its evidence is the suspicion itself, so counting it would collapse a lone
  /// suspicion to the floor. Only tracked while `subject` is actually suspected.
  fn confirm(&mut self, subject: HostId, by: HostId) {
    self.confirmations.entry(subject).or_default().insert(by);
  }

  /// Applies a membership update and enqueues the resulting change for gossip dissemination.
  fn record(&mut self, subject: HostId, update: MemberState) -> Option<Change> {
    let change = self.membership.apply(subject, update);
    match change {
      Some(Change::Adopted { member, state }) => {
        self
          .gossip
          .insert(member, (state, self.timing.gossip_transmits));
      }
      Some(Change::Refuted { incarnation }) => {
        // A peer suspected us: our acknowledgements are not reaching the fleet, so we look unhealthy —
        // raise the local-health multiplier (Lifeguard) and gossip the refutation so the fleet learns
        // we are alive at the new incarnation.
        self.worsen_health();
        let state = MemberState {
          liveness: Liveness::Alive,
          incarnation,
        };
        self
          .gossip
          .insert(self.local, (state, self.timing.gossip_transmits));
      }
      None => {}
    }
    change
  }

  /// The batch of membership updates to piggyback on an outgoing ping or acknowledgement: up to `max`,
  /// the least-disseminated first, each with its remaining-transmit count decremented and dropped once
  /// exhausted — so the buffer is bounded and each change spreads a fixed number of times (§4.8; SWIM
  /// infection-style dissemination).
  pub fn gossip(&mut self, max: usize) -> Vec<(HostId, MemberState)> {
    let mut ranked: Vec<(HostId, MemberState, u32)> = self
      .gossip
      .iter()
      .map(|(&subject, &(state, remaining))| (subject, state, remaining))
      .collect();
    // Most remaining transmits first — freshest changes propagate soonest.
    ranked.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.0.cmp(&b.0.0)));
    let mut batch = Vec::new();
    for (subject, state, _) in ranked.into_iter().take(max) {
      batch.push((subject, state));
      if let Some((_, remaining)) = self.gossip.get_mut(&subject) {
        *remaining -= 1;
        if *remaining == 0 {
          self.gossip.remove(&subject);
        }
      }
    }
    batch
  }

  /// Applies a received gossip batch, folding each update into the view (and re-enqueueing anything it
  /// adopts so the change spreads onward — the infection continues).
  pub fn apply_gossip(&mut self, updates: &[(HostId, MemberState)]) {
    for &(subject, state) in updates {
      self.apply(subject, state);
    }
  }

  /// Applies a received gossip batch that arrived **from** `sender`, folding each update into the view
  /// and, for every `Suspect` it carries, recording `sender` as an independent confirmer of that
  /// suspicion — so a failure many peers suspect is declared dead sooner (the confirmation-count timeout,
  /// §4.8). A `sender` that holds and gossips a suspicion is corroborating it; the count is of distinct
  /// senders, so re-hearing the same sender does not inflate it.
  pub fn apply_gossip_from(&mut self, sender: HostId, updates: &[(HostId, MemberState)]) {
    for &(subject, state) in updates {
      self.apply(subject, state);
      if state.liveness == Liveness::Suspect
        && self.membership.state(subject).map(|s| s.liveness) == Some(Liveness::Suspect)
      {
        self.confirm(subject, sender);
      }
    }
  }

  /// The membership view this detector maintains.
  pub fn membership(&self) -> &Membership {
    &self.membership
  }

  /// Learns a peer (alive at incarnation zero) — a join. The next round will probe it.
  pub fn join(&mut self, peer: HostId) {
    if peer != self.local {
      self.record(
        peer,
        MemberState {
          liveness: Liveness::Alive,
          incarnation: 0,
        },
      );
    }
  }

  /// Applies a gossiped membership update (from a ping/ack payload), returning the change and enqueuing
  /// it for onward gossip; an alive adoption clears any local suspicion of that peer.
  pub fn apply(&mut self, subject: HostId, update: MemberState) -> Option<Change> {
    let change = self.record(subject, update);
    if let Some(Change::Adopted { member, state }) = change
      && state.liveness == Liveness::Alive
    {
      self.suspicion.remove(&member);
      self.confirmations.remove(&member);
    }
    change
  }

  /// Advances one protocol period: resolves the previous probe (a member that did not acknowledge is
  /// suspected at its current incarnation), ages every suspected member toward death (declaring one
  /// dead once it has been suspected for the suspicion window), then picks the next member to probe in
  /// round-robin over the alive peers and returns the [`Ping`] to send — or `None` when there are no
  /// peers to probe.
  pub fn tick(&mut self) -> Option<Ping> {
    if let Some(target) = self.probing.take() {
      if self.acked {
        // The probe was answered (directly or through a relay): the node looks healthy.
        self.improve_health();
      } else {
        // The probe went wholly unanswered: raise the local-health multiplier (Lifeguard — this is as
        // much a signal about us as about the target) and suspect a still-alive target.
        self.worsen_health();
        if let Some(current) = self.membership.state(target)
          && current.liveness == Liveness::Alive
        {
          self.record(
            target,
            MemberState {
              liveness: Liveness::Suspect,
              incarnation: current.incarnation,
            },
          );
          // We are the ORIGINATOR of this suspicion, not a confirmer: our failed probe is the suspicion
          // itself, so it does not shrink the window. Only *other* members' independent confirmations do
          // (Lifeguard/memberlist; hyperscale's chaos tests showed self-counting collapses a lone
          // suspicion straight to the floor and evicts a peer mid-partition). C therefore starts at zero.
        }
      }
    }
    self.age_suspicions();

    let target = self.next_target()?;
    self.probing = Some(target);
    self.acked = false;
    Some(Ping { to: target })
  }

  /// Records an acknowledgement from `from`: if it is this period's probe target, the probe succeeded.
  pub fn on_ack(&mut self, from: HostId) {
    if self.probing == Some(from) {
      self.acked = true;
    }
  }

  /// Responds to a received ping from `from` with the acknowledgement to send back.
  pub fn on_ping(&self, from: HostId) -> Ack {
    Ack { to: from }
  }

  /// When a direct ping has gone unanswered this period, asks up to `fanout` other alive peers to ping
  /// the current target on our behalf (SWIM's indirect probe). Returns the ping-requests to send;
  /// empty if there is no current target or no eligible relay. The caller sends them, and relays any
  /// acknowledgement back as an indirect ack ([`on_indirect_ack`](Detector::on_indirect_ack)).
  pub fn request_indirect(&self, fanout: usize) -> Vec<PingReq> {
    let Some(target) = self.probing else {
      return Vec::new();
    };
    if self.acked {
      return Vec::new();
    }
    self
      .membership
      .alive()
      .into_iter()
      .filter(|host| *host != self.local && *host != target)
      .take(fanout)
      .map(|relay| PingReq { relay, target })
      .collect()
  }

  /// As a relay, responds to a ping-request for `target` with the ping to send it; the caller relays
  /// the resulting acknowledgement back to the requester (that relay-back routing is the caller's).
  pub fn on_ping_req(&self, target: HostId) -> Ping {
    Ping { to: target }
  }

  /// Records an indirect acknowledgement that `target` is alive (a relay reached it): if `target` is
  /// this period's probe, the probe succeeded, so the target will not be suspected — a lost direct
  /// packet is not mistaken for a failure.
  pub fn on_indirect_ack(&mut self, target: HostId) {
    if self.probing == Some(target) {
      self.acked = true;
    }
  }

  /// Ages each suspected member's counter by one period; a member suspected for its whole suspicion
  /// window — the confirmation-count timeout for how many independent peers suspect it, dilated by the
  /// local-health multiplier — is declared dead (at the incarnation it was suspected under). Counters and
  /// confirmations for members no longer suspected (refuted or already dead) are dropped.
  fn age_suspicions(&mut self) {
    let suspects = self.membership.suspects();
    let suspect_ids: Vec<HostId> = suspects.iter().map(|(host, _)| *host).collect();
    self.suspicion.retain(|host, _| suspect_ids.contains(host));
    self
      .confirmations
      .retain(|host, _| suspect_ids.contains(host));
    for (host, incarnation) in suspects {
      let confirmations = self.confirmations.get(&host).map_or(0, BTreeSet::len);
      let window = self.suspicion_window(u64::try_from(confirmations).unwrap_or(u64::MAX));
      let periods = self.suspicion.entry(host).or_insert(0);
      *periods = periods.saturating_add(1);
      if *periods >= window {
        self.record(
          host,
          MemberState {
            liveness: Liveness::Dead,
            incarnation,
          },
        );
        self.suspicion.remove(&host);
        self.confirmations.remove(&host);
      }
    }
  }

  /// The next alive peer to probe, in **randomized** order (SWIM §4). Each round is a fresh shuffled
  /// permutation of the alive peers, advanced one per period; when the round is exhausted a new
  /// permutation is drawn — so every member is probed once per round and the worst-case wait for a first
  /// probe is one round, not the unbounded delay a fixed rotation can suffer. Members that died mid-round
  /// are skipped, and `None` is returned only when no alive peer remains.
  fn next_target(&mut self) -> Option<HostId> {
    loop {
      // Advance through the current shuffled round, skipping any member that died mid-round.
      while self.cursor < self.order.len() {
        let candidate = self.order[self.cursor];
        self.cursor += 1;
        if self.membership.state(candidate).map(|state| state.liveness) == Some(Liveness::Alive) {
          return Some(candidate);
        }
      }
      // The round is exhausted. Draw a fresh permutation from the current alive peers; if none remain,
      // there is no target. The fresh set is drawn from alive members, so the next pass returns its
      // first member — the loop makes at most one further pass, and terminates without a step count.
      let mut fresh: Vec<HostId> = self
        .membership
        .alive()
        .into_iter()
        .filter(|host| *host != self.local)
        .collect();
      if fresh.is_empty() {
        return None;
      }
      self.shuffler.shuffle(&mut fresh);
      self.order = fresh;
      self.cursor = 0;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const LOCAL: HostId = HostId(1);
  const A: HostId = HostId(2);
  const B: HostId = HostId(3);
  const C: HostId = HostId(4);

  /// Timing for the pure-SWIM tests: the given suspicion window and gossip transmits, with the Lifeguard
  /// local-health multiplier disabled (`health_max = 0`) and the confirmation curve off (the floor equals
  /// the ceiling), so those tests exercise probing, suspicion and gossip in isolation. The LHM tests set
  /// `health_max` and the confirmation test sets `suspicion_min` directly.
  fn timing(suspicion_periods: u32, gossip_transmits: u32) -> DetectorTiming {
    DetectorTiming {
      suspicion_periods,
      gossip_transmits,
      health_max: 0,
      suspicion_min: suspicion_periods,
      confirmations_expected: 1,
    }
  }

  /// A suspicion update about the local node at its current incarnation — the input that forces a
  /// self-refutation (and, with it, a local-health worsening).
  fn self_suspicion(detector: &Detector) -> MemberState {
    MemberState {
      liveness: Liveness::Suspect,
      incarnation: detector.membership().local_incarnation(),
    }
  }

  /// A member that never acknowledges is suspected after its probe, then declared dead once it has been
  /// suspected for the suspicion window — Alive → Suspect → Dead, driven by ticks.
  #[test]
  fn an_unresponsive_member_is_suspected_then_declared_dead() {
    let mut detector = Detector::new(LOCAL, timing(2, 3));
    detector.join(A);

    // Period 1 probes A (the only peer); A never acknowledges.
    assert_eq!(detector.tick(), Some(Ping { to: A }));
    assert_eq!(
      detector.membership().state(A).unwrap().liveness,
      Liveness::Alive
    );

    // Period 2 resolves the missed probe: A is suspected. (It probes A again — still the only peer.)
    detector.tick();
    assert_eq!(
      detector.membership().state(A).unwrap().liveness,
      Liveness::Suspect
    );

    // Two more periods of suspicion reach the window (2), and A is declared dead.
    detector.tick();
    detector.tick();
    assert_eq!(
      detector.membership().state(A).unwrap().liveness,
      Liveness::Dead
    );
    assert_eq!(
      detector.membership().alive(),
      vec![LOCAL],
      "the dead member left the neighbourhood"
    );
  }

  /// A member that acknowledges each probe stays alive — never suspected.
  #[test]
  fn a_responsive_member_stays_alive() {
    let mut detector = Detector::new(LOCAL, timing(2, 3));
    detector.join(A);
    for _ in 0..5 {
      let ping = detector.tick().expect("a peer to probe");
      detector.on_ack(ping.to); // A answers within the period
    }
    assert_eq!(
      detector.membership().state(A).unwrap().liveness,
      Liveness::Alive
    );
  }

  /// Probing is round-robin across the alive peers, so no member starves — over two periods both peers
  /// are probed.
  #[test]
  fn probing_rotates_across_peers() {
    let mut detector = Detector::new(LOCAL, timing(3, 3));
    detector.join(A);
    detector.join(B);
    let first = detector.tick().unwrap().to;
    detector.on_ack(first);
    let second = detector.tick().unwrap().to;
    detector.on_ack(second);
    assert_ne!(first, second, "the two periods probe different peers");
    assert!([A, B].contains(&first) && [A, B].contains(&second));
  }

  /// A ping is answered with an acknowledgement to the sender.
  #[test]
  fn a_ping_is_acknowledged() {
    let detector = Detector::new(LOCAL, timing(2, 3));
    assert_eq!(detector.on_ping(A), Ack { to: A });
  }

  /// An indirect acknowledgement prevents a false suspicion: A's direct ping is lost, but a relay
  /// reaches A and relays the ack, so the next period does not suspect A — a lost packet is not a
  /// failure. The ping-request is aimed at another alive peer, not the target.
  #[test]
  fn an_indirect_ack_prevents_a_false_suspicion() {
    let mut detector = Detector::new(LOCAL, timing(2, 3));
    detector.join(A);
    detector.join(B);

    // Probe A. Suppose its direct ack is lost (we do not call on_ack for A).
    let ping = detector.tick().unwrap();
    let target = ping.to;
    // Ask the other alive peer to probe the target indirectly.
    let requests = detector.request_indirect(1);
    assert_eq!(requests.len(), 1, "one relay is asked");
    assert_eq!(requests[0].target, target);
    assert_ne!(requests[0].relay, target, "the relay is a different peer");
    // The relay reaches the target and relays the acknowledgement.
    detector.on_indirect_ack(target);

    // The next period must not suspect the target — the indirect ack saved it.
    detector.tick();
    assert_eq!(
      detector.membership().state(target).unwrap().liveness,
      Liveness::Alive,
      "an indirectly-acknowledged member is not suspected"
    );
  }

  /// A membership change is disseminated a bounded number of times, then dropped — the gossip buffer
  /// does not grow without end (infection-style dissemination, bounded).
  #[test]
  fn a_change_is_gossiped_a_bounded_number_of_times() {
    let mut detector = Detector::new(LOCAL, timing(2, 2)); // two transmits per change
    detector.join(A); // learning A is a change to disseminate

    assert!(
      detector.gossip(10).iter().any(|(host, _)| *host == A),
      "the change is gossiped (transmit 1)"
    );
    assert!(
      detector.gossip(10).iter().any(|(host, _)| *host == A),
      "and again (transmit 2)"
    );
    assert!(
      detector.gossip(10).iter().all(|(host, _)| *host != A),
      "after its transmit budget the change is dropped — the buffer is bounded"
    );
  }

  /// Gossip carries a change to another node: one node declares a peer dead, gossips it, and a second
  /// node that applies the batch adopts the death — the view spreads.
  #[test]
  fn gossip_carries_a_change_to_another_node() {
    let mut source = Detector::new(LOCAL, timing(2, 2));
    source.join(A);
    // The source declares A dead (adopts it), enqueuing the change for gossip.
    source.apply(
      A,
      MemberState {
        liveness: Liveness::Dead,
        incarnation: 0,
      },
    );
    let batch = source.gossip(10);

    let mut other = Detector::new(B, timing(2, 2));
    other.apply_gossip(&batch);
    assert_eq!(
      other.membership().state(A).map(|s| s.liveness),
      Some(Liveness::Dead),
      "the second node learns A is dead through gossip"
    );
  }

  /// The Lifeguard local health multiplier rises when the node must refute a suspicion about itself and
  /// falls when its probes succeed: a node repeatedly (and falsely) suspected looks unhealthy, then
  /// recovers as it reaches peers again. The multiplier starts at one (full health).
  #[test]
  fn self_refutation_raises_health_and_successful_probes_lower_it() {
    let mut detector = Detector::new(
      LOCAL,
      DetectorTiming {
        suspicion_periods: 10,
        gossip_transmits: 2,
        health_max: 3,
        suspicion_min: 10,
        confirmations_expected: 1,
      },
    );
    assert_eq!(
      detector.health_multiplier(),
      1,
      "the node starts at full health"
    );

    // Two peers keep (falsely) suspecting us; each refutation raises the multiplier.
    let suspicion = self_suspicion(&detector);
    detector.apply(LOCAL, suspicion);
    let suspicion = self_suspicion(&detector);
    detector.apply(LOCAL, suspicion);
    assert_eq!(
      detector.health_multiplier(),
      3,
      "refuting suspicions raised the multiplier"
    );

    // Now our probes of A succeed period after period; the multiplier falls back to full health.
    detector.join(A);
    for _ in 0..3 {
      let ping = detector.tick().expect("a peer to probe");
      detector.on_ack(ping.to);
    }
    assert_eq!(
      detector.health_multiplier(),
      1,
      "successful probes restored full health"
    );
  }

  /// Failed probes raise the multiplier one step each, and it is bounded at `health_max + 1` — a
  /// wholly-isolated node backs off but does not dilate without end.
  #[test]
  fn repeated_failed_probes_raise_the_multiplier_to_its_cap() {
    let mut detector = Detector::new(
      LOCAL,
      DetectorTiming {
        suspicion_periods: 10,
        gossip_transmits: 2,
        health_max: 2,
        suspicion_min: 10,
        confirmations_expected: 1,
      },
    );
    detector.join(A);
    detector.join(B);
    detector.join(C);

    // Never acknowledge: every probe fails, so each period raises the multiplier until it caps.
    for _ in 0..6 {
      detector.tick();
    }
    assert_eq!(
      detector.health_multiplier(),
      3,
      "the multiplier is bounded at health_max + 1 however many probes fail"
    );
  }

  /// An unhealthy node dilates its suspicion window: a peer that would be declared dead after the base
  /// window at full health stays suspected longer while the node's own health multiplier is raised —
  /// so the node does not mass-declare peers dead on its own degradation.
  #[test]
  fn an_unhealthy_node_is_slower_to_declare_a_peer_dead() {
    let mut detector = Detector::new(
      LOCAL,
      DetectorTiming {
        // Base window of one period: at full health a suspect dies the first period it is aged.
        suspicion_periods: 1,
        gossip_transmits: 2,
        health_max: 3,
        suspicion_min: 1,
        confirmations_expected: 1,
      },
    );
    // Make the node unhealthy through self-refutations, so its window dilates past one period.
    for _ in 0..2 {
      let suspicion = self_suspicion(&detector);
      detector.apply(LOCAL, suspicion);
    }
    assert!(detector.health_multiplier() > 1, "the node is unhealthy");

    // A falls silent and is suspected; at full health a single further ageing period would kill it.
    detector.join(A);
    detector.tick();
    detector.tick();
    assert_eq!(
      detector.membership().state(A).map(|s| s.liveness),
      Some(Liveness::Suspect),
      "A is suspected"
    );
    detector.tick();
    detector.tick();
    assert_eq!(
      detector.membership().state(A).map(|s| s.liveness),
      Some(Liveness::Suspect),
      "the dilated window keeps A in doubt past the base window rather than declaring it dead"
    );
  }

  /// The confirmation-count timeout (§4.8): a suspicion many peers independently corroborate is declared
  /// dead sooner than a lone one. With a base window of six and a floor of two reached at three
  /// confirmations, a suspicion confirmed by two more peers dies at the floor, while the same suspicion
  /// held alone is still merely suspected at that point.
  #[test]
  fn a_corroborated_suspicion_dies_sooner_than_a_lone_one() {
    // Base window six, floor two, reached at two *independent* confirmations (the originator's own probe
    // does not count — Lifeguard/memberlist).
    let timing = DetectorTiming {
      suspicion_periods: 6,
      gossip_transmits: 2,
      health_max: 0,
      suspicion_min: 2,
      confirmations_expected: 2,
    };

    // Corroborated: two *other* peers confirm the suspicion, shrinking the window to its floor of two.
    let mut corroborated = Detector::new(LOCAL, timing);
    corroborated.join(A);
    corroborated.tick(); // probe A
    corroborated.tick(); // A unanswered → suspected (originator only, C = 0)
    let incarnation = corroborated.membership().state(A).unwrap().incarnation;
    let suspect = MemberState {
      liveness: Liveness::Suspect,
      incarnation,
    };
    corroborated.apply_gossip_from(B, &[(A, suspect)]); // C = 1
    corroborated.apply_gossip_from(C, &[(A, suspect)]); // C = 2 → the floor
    corroborated.tick(); // window is now two; the second aged period declares A dead
    assert_eq!(
      corroborated.membership().state(A).map(|s| s.liveness),
      Some(Liveness::Dead),
      "a corroborated failure is declared dead at the floor"
    );

    // Lone: only this node's own probe suspects A (C = 0), so the full window of six applies — a lone
    // suspicion rides the honest maximum rather than collapsing to the floor (the eviction bug fixed).
    let mut lone = Detector::new(LOCAL, timing);
    lone.join(A);
    lone.tick();
    lone.tick(); // suspected, C = 0
    lone.tick(); // period 2 of 6 — nowhere near the window
    assert_eq!(
      lone.membership().state(A).map(|s| s.liveness),
      Some(Liveness::Suspect),
      "a lone suspicion waits the full, unshrunk window"
    );
  }

  /// Randomized probe order (SWIM §4) still probes every peer exactly once per round — the coverage
  /// guarantee that bounds the worst-case time to a first probe — and the next round reshuffles into
  /// another full permutation.
  #[test]
  fn every_peer_is_probed_once_per_round() {
    let peers = [A, B, C];
    let mut detector = Detector::new(LOCAL, timing(4, 2));
    for &peer in &peers {
      detector.join(peer);
    }

    // One round is exactly as many probes as there are peers — the loop length is the peer count, not a
    // literal.
    let round = |detector: &mut Detector| {
      let mut probed = Vec::new();
      for _ in 0..peers.len() {
        let ping = detector.tick().expect("a peer to probe");
        detector.on_ack(ping.to); // keep every peer alive so the round is not disturbed
        probed.push(ping.to);
      }
      probed.sort_by_key(|host| host.0);
      probed
    };

    let mut expected = peers.to_vec();
    expected.sort_by_key(|host| host.0);
    assert_eq!(
      round(&mut detector),
      expected,
      "the first round probes every peer once"
    );
    assert_eq!(
      round(&mut detector),
      expected,
      "the next round reshuffles and covers them again"
    );
  }
}
