//! The SWIM failure detector (§4.8, Part 4 "Cluster plane") — the protocol-period machine that probes
//! members and drives the [`Membership`] view from alive to suspect to dead. Sans-io and driven by
//! `tick` (one protocol period per call) and message events, so it is deterministic and oracle-tested
//! at N=1 before any timer or datagram is involved; the caller sends the [`Ping`]/[`Ack`] it returns
//! over the control plane and feeds received messages back in.
//!
//! Evidence: SWIM (Das/Gupta/Motivala, DSN 2002) and Lifeguard (Dadgar/Phillips/Currey, DSN 2018),
//! tier A. This slice is **direct probing**: each period the detector pings the next member in a
//! round-robin over the alive peers; a member that does not acknowledge within its period is
//! suspected, and a member that stays suspected for the suspicion window is declared dead. Owed: the
//! **indirect probe** (ping-request through `k` peers, which distinguishes a failed member from a lost
//! packet), gossip piggybacked on the ping/ack, and the Lifeguard local-health multiplier that widens
//! the timeouts when the local node itself looks unhealthy. Round-robin (not random) selection is the
//! deterministic interim; SWIM's randomized order with a per-round shuffle is owed with the tuning.

use std::collections::BTreeMap;

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

/// The failure detector for one node: it owns the node's [`Membership`] view, a round-robin cursor
/// over the members it probes, the member being probed this period and whether it has acknowledged,
/// and how many periods each suspected member has gone unrefuted. A member suspected for
/// `suspicion_periods` periods is declared dead.
pub struct Detector {
  membership: Membership,
  local: HostId,
  order: Vec<HostId>,
  cursor: usize,
  probing: Option<HostId>,
  acked: bool,
  suspicion: BTreeMap<HostId, u32>,
  suspicion_periods: u32,
}

impl Detector {
  /// A detector for `local`, declaring a member dead after it has been suspected for
  /// `suspicion_periods` protocol periods (the caller derives this from the fleet size and period —
  /// SWIM's suspicion timeout grows with `log(N)`; owed as a measured value, a parameter here so it is
  /// never a hidden constant).
  pub fn new(local: HostId, suspicion_periods: u32) -> Detector {
    Detector {
      membership: Membership::new(local),
      local,
      order: Vec::new(),
      cursor: 0,
      probing: None,
      acked: false,
      suspicion: BTreeMap::new(),
      suspicion_periods,
    }
  }

  /// The membership view this detector maintains.
  pub fn membership(&self) -> &Membership {
    &self.membership
  }

  /// Learns a peer (alive at incarnation zero) — a join. The next round will probe it.
  pub fn join(&mut self, peer: HostId) {
    if peer != self.local {
      self.membership.apply(
        peer,
        MemberState {
          liveness: Liveness::Alive,
          incarnation: 0,
        },
      );
    }
  }

  /// Applies a gossiped membership update (from a ping/ack payload), returning the change — the seam
  /// gossip dissemination (owed) will drive; a refutation here clears any local suspicion of that peer.
  pub fn apply(&mut self, subject: HostId, update: MemberState) -> Option<Change> {
    let change = self.membership.apply(subject, update);
    if let Some(Change::Adopted { member, state }) = change
      && state.liveness == Liveness::Alive
    {
      self.suspicion.remove(&member);
    }
    change
  }

  /// Advances one protocol period: resolves the previous probe (a member that did not acknowledge is
  /// suspected at its current incarnation), ages every suspected member toward death (declaring one
  /// dead once it has been suspected for the suspicion window), then picks the next member to probe in
  /// round-robin over the alive peers and returns the [`Ping`] to send — or `None` when there are no
  /// peers to probe.
  pub fn tick(&mut self) -> Option<Ping> {
    if let Some(target) = self.probing.take()
      && !self.acked
      && let Some(current) = self.membership.state(target)
      && current.liveness == Liveness::Alive
    {
      self.membership.apply(
        target,
        MemberState {
          liveness: Liveness::Suspect,
          incarnation: current.incarnation,
        },
      );
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

  /// Ages each suspected member's counter by one period; a member suspected for the whole suspicion
  /// window is declared dead (at the incarnation it was suspected under), and counters for members no
  /// longer suspected (refuted or already dead) are dropped.
  fn age_suspicions(&mut self) {
    let suspects = self.membership.suspects();
    let suspect_ids: Vec<HostId> = suspects.iter().map(|(host, _)| *host).collect();
    self.suspicion.retain(|host, _| suspect_ids.contains(host));
    for (host, incarnation) in suspects {
      let periods = self.suspicion.entry(host).or_insert(0);
      *periods = periods.saturating_add(1);
      if *periods >= self.suspicion_periods {
        self.membership.apply(
          host,
          MemberState {
            liveness: Liveness::Dead,
            incarnation,
          },
        );
        self.suspicion.remove(&host);
      }
    }
  }

  /// The next alive peer to probe, round-robin. Rebuilds the rotation from the current alive set (so a
  /// newly dead member drops out and a new one joins in), skipping the local node.
  fn next_target(&mut self) -> Option<HostId> {
    self.order = self
      .membership
      .alive()
      .into_iter()
      .filter(|host| *host != self.local)
      .collect();
    if self.order.is_empty() {
      return None;
    }
    if self.cursor >= self.order.len() {
      self.cursor = 0;
    }
    let target = self.order[self.cursor];
    self.cursor += 1;
    Some(target)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const LOCAL: HostId = HostId(1);
  const A: HostId = HostId(2);
  const B: HostId = HostId(3);

  /// A member that never acknowledges is suspected after its probe, then declared dead once it has been
  /// suspected for the suspicion window — Alive → Suspect → Dead, driven by ticks.
  #[test]
  fn an_unresponsive_member_is_suspected_then_declared_dead() {
    let mut detector = Detector::new(LOCAL, 2);
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
    let mut detector = Detector::new(LOCAL, 2);
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
    let mut detector = Detector::new(LOCAL, 3);
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
    let detector = Detector::new(LOCAL, 2);
    assert_eq!(detector.on_ping(A), Ack { to: A });
  }

  /// An indirect acknowledgement prevents a false suspicion: A's direct ping is lost, but a relay
  /// reaches A and relays the ack, so the next period does not suspect A — a lost packet is not a
  /// failure. The ping-request is aimed at another alive peer, not the target.
  #[test]
  fn an_indirect_ack_prevents_a_false_suspicion() {
    let mut detector = Detector::new(LOCAL, 2);
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
}
