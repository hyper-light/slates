//! The membership plane by use (A-67 H-2; `docs/wip/transport-quic.md` §6): members exchange sealed datagrams over an
//! in-memory network on simulated time, the way the control shard's task drives one plane over its UDP socket.
// Test harness code: an unwrap here is a failed test.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::collections::BTreeMap;
use std::num::NonZeroUsize;

use slates_cluster::fleet::{FleetNode, apply_peer_state};
use slates_cluster::lease_renewal::LeaseRenewal;
use slates_cluster::member_plane::{Fleet, MemberPlane, PlaneEvent};
use slates_db::register::{
  Configuration, HostId, Lineage, ObjectId, Quorum, RegionalConfiguration,
};

use hyper_datagram::{ExporterSecret, Role, SECRET_BYTES};
use hyper_swim::membership::Liveness;

/// Shape: the members of the simulated fleet.
/// Shape: the simulated clock's resolution: its stamps are whole nanoseconds.
const SIM_RESOLUTION: std::time::Duration = std::time::Duration::from_nanos(1);

const MEMBERS: u64 = 3;
/// Shape: the one-way latency of the simulated network, nanoseconds (a datacentre round trip of 1 ms).
const LATENCY_NS: u64 = 500_000;
/// Shape: a timer's least lateness, nanoseconds: 50 µs, the order of a runtime's timer granularity; a wake fires up
/// to twice this late.
const TIMER_LATENESS_NS: u64 = 50_000;
/// Shape: how many of the dead member's objects the survivors back: enough that some rank first to each survivor.
const BACKED_OBJECTS: u64 = 64;
/// Shape: the jitter's bound as a divisor of the latency: up to a fifth of it.
const JITTER_DIVISOR: u64 = 5;
/// Shape: the extra one-way latency of a far link, nanoseconds: a cross-region path (Japan East to East US is
/// about 80 ms one way; the escape and KIND lanes shape 80-100 ms).
const FAR_LATENCY_NS: u64 = 100_000_000;
/// Shape: the owner lease's bound in the daemon, nanoseconds: three coordinator periods of 100 ms, each dilated
/// three times (`slates-server` `lease::horizon_ns`, less its 0.1 % clock tolerance).
const LEASE_BOUND_NS: u64 = 900_000_000;
/// Shape: the simulated time the lease test observes, nanoseconds: many probe rounds.
const LEASE_OBSERVED_NS: u64 = 30_000_000_000;
/// Shape: the simulated time a run may take before it is a failure: long past the detector's evidence and its
/// detection bound at this latency (hundreds of periods).
const HORIZON_NS: u64 = 600_000_000_000;

/// A fleet that believes and answers every member (but one it refuses to answer, as for a forged identity), announcing
/// version 7 and standing 3.
#[derive(Default)]
struct Believing {
  refuses: Option<u64>,
  /// The holders this member's owner lease is renewed with.
  holders: Vec<u64>,
}
impl Fleet for Believing {
  fn announced_version(&self) -> u64 {
    7
  }
  fn answer(&mut self, prober: HostId, _: u64, _: u64) -> Option<(u64, Option<u64>)> {
    (self.refuses != Some(prober.0)).then_some((7, Some(3)))
  }
  fn admit(&mut self, _: HostId, _: u64) -> bool {
    true
  }
  fn relays_to(&self, _: HostId) -> bool {
    true
  }
  fn lease_holders(&self, into: &mut Vec<HostId>) {
    into.extend(self.holders.iter().map(|holder| HostId(*holder)));
  }
}

/// Shape: the lease renewal the daemon runs: every 100 ms coordinator period, within the lease bound.
const RENEWAL: LeaseRenewal = LeaseRenewal {
  interval_ns: 100_000_000,
  bound_ns: LEASE_BOUND_NS,
};

/// The secret two members' canonical connection exports: any 32 bytes both derive alike.
fn secret_between(a: u64, b: u64) -> ExporterSecret {
  let (low, high) = (a.min(b), a.max(b));
  let mut bytes = [0u8; SECRET_BYTES];
  for (index, byte) in bytes.iter_mut().enumerate() {
    *byte = u8::try_from((low * 31 + high * 17 + index as u64) % 251).unwrap();
  }
  ExporterSecret::new(bytes)
}

/// A datagram in flight: who it is for, when it lands, and its bytes.
struct InFlight {
  to: u64,
  lands_ns: u64,
  bytes: Vec<u8>,
}

struct Network {
  /// Members whose datagrams, either way, cross [`FAR_LATENCY_NS`] more: another region.
  far: Vec<u64>,
  members: BTreeMap<u64, MemberPlane>,
  fleets: BTreeMap<u64, Believing>,
  in_flight: Vec<InFlight>,
  events: BTreeMap<u64, Vec<PlaneEvent>>,
  /// Members whose datagrams are dropped both ways: killed.
  killed: Vec<u64>,
  now_ns: u64,
  /// The jitter generator's state (xorshift64), deterministic.
  jitter: u64,
  /// Every condemnation a member's own probes made, as `(member, condemned)`, gathered after each step.
  condemned: Vec<(u64, u64)>,
}

impl Network {
  fn new() -> Network {
    Network::sized(MEMBERS, &[])
  }

  /// `count` members, those in `far` across the far link.
  fn sized(count: u64, far: &[u64]) -> Network {
    let mut members = BTreeMap::new();
    for me in 1..=count {
      let mut plane = MemberPlane::new(
        HostId(me),
        me * 1_000,
        NonZeroUsize::new(usize::try_from(count).unwrap()).unwrap(),
        SIM_RESOLUTION,
        RENEWAL,
      )
      .unwrap();
      for peer in (1..=count).filter(|peer| *peer != me) {
        // The lower member id dials the canonical connection (transport-quic.md §6).
        let role = if me < peer {
          Role::Initiator
        } else {
          Role::Acceptor
        };
        plane
          .install_epoch(HostId(peer), 1, &secret_between(me, peer), role)
          .unwrap();
        // As the daemon does: each peer joins with the round trip its keying handshake measured.
        let crossing = if far.contains(&me) != far.contains(&peer) {
          FAR_LATENCY_NS
        } else {
          0
        };
        plane.join(
          HostId(peer),
          Some(std::time::Duration::from_nanos(2 * (LATENCY_NS + crossing))),
        );
      }
      members.insert(me, plane);
    }
    let fleets = (1..=count).map(|me| (me, Believing::default())).collect();
    Network {
      far: far.to_vec(),
      members,
      fleets,
      in_flight: Vec::new(),
      events: BTreeMap::new(),
      killed: Vec::new(),
      now_ns: 1,
      jitter: 0x9E37_79B9_7F4A_7C15,
      condemned: Vec::new(),
    }
  }

  /// The next latency: [`LATENCY_NS`] plus up to a fifth of it, from a deterministic generator.
  fn next_latency(&mut self) -> u64 {
    let mut x = self.jitter;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    self.jitter = x;
    LATENCY_NS + x % (LATENCY_NS / JITTER_DIVISOR)
  }

  fn flush(&mut self, me: u64) {
    let now = self.now_ns;
    let killed = self.killed.contains(&me);
    let mut sealed = Vec::new();
    self.members.get_mut(&me).unwrap().flush(|to, bytes| {
      if !killed {
        sealed.push((to.0, bytes.to_vec()));
      }
    });
    for (to, bytes) in sealed {
      let crossing = if self.far.contains(&me) != self.far.contains(&to) {
        FAR_LATENCY_NS
      } else {
        0
      };
      let lands_ns = now + self.next_latency() + crossing;
      self.in_flight.push(InFlight {
        to,
        lands_ns,
        bytes,
      });
    }
  }

  /// Runs every member's step, then delivers and steps until `horizon_ns` or `done` holds.
  fn run_until(&mut self, horizon_ns: u64, done: impl Fn(&Network) -> bool) -> bool {
    while self.now_ns < horizon_ns {
      let ids: Vec<u64> = self.members.keys().copied().collect();
      for me in &ids {
        if self.killed.contains(me) {
          continue;
        }
        let wake = self.members[me].wake();
        if wake.is_none_or(|at| at <= self.now_ns) {
          let fleet = self.fleets.get_mut(me).unwrap();
          self.members.get_mut(me).unwrap().step(self.now_ns, fleet);
          self.gather_condemnations(*me);
          self.flush(*me);
        }
      }
      // Deliver what has landed.
      let mut landed = Vec::new();
      self.in_flight.retain(|datagram| {
        if datagram.lands_ns <= self.now_ns {
          landed.push((datagram.to, datagram.bytes.clone()));
          false
        } else {
          true
        }
      });
      for (to, mut bytes) in landed {
        if self.killed.contains(&to) {
          continue;
        }
        let events = self.events.entry(to).or_default();
        let member = self.members.get_mut(&to).unwrap();
        let fleet = self.fleets.get_mut(&to).unwrap();
        member.receive(&mut bytes, self.now_ns, fleet, events);
        // The detector is polled after every message it is fed, as well as at its wake (hyper-swim `Detector::poll`).
        member.step(self.now_ns, fleet);
        self.gather_condemnations(to);
        self.flush(to);
      }
      if done(self) {
        return true;
      }
      // The next instant anything happens: a wake or a landing.
      // A timer fires late by its granularity, as every runtime's does; the detector measures that lateness (`G`)
      // and samples no round trip while it reads zero (hyper-swim `Detector::granularity`).
      let lateness = TIMER_LATENESS_NS + self.next_latency() % TIMER_LATENESS_NS;
      let next_wake = ids
        .iter()
        .filter(|me| !self.killed.contains(me))
        .filter_map(|me| self.members[me].wake())
        .map(|at| at + lateness)
        .min();
      let next_landing = self
        .in_flight
        .iter()
        .map(|datagram| datagram.lands_ns)
        .min();
      let next = [next_wake, next_landing]
        .into_iter()
        .flatten()
        .filter(|at| *at > self.now_ns)
        .min()
        .unwrap_or(self.now_ns + LATENCY_NS);
      self.now_ns = next;
    }
    false
  }

  /// Records the condemnations member `me`'s last poll found.
  fn gather_condemnations(&mut self, me: u64) {
    for finding in self.members[&me].detector().findings() {
      if let hyper_swim::detector::Finding::Condemned { target, .. } = finding {
        self.condemned.push((me, target.0));
      }
    }
  }

  fn liveness(&self, of: u64, peer: u64) -> Option<Liveness> {
    self.members[&of]
      .detector()
      .membership()
      .state(hyper_swim::HostId(peer))
      .map(|state| state.liveness)
  }
}

/// An `Acked` event at member `me` names another member, its announced identity, the fleet's standing, and the
/// network's round trip.
fn check_acked(me: u64, event: &PlaneEvent) {
  let PlaneEvent::Acked {
    from,
    boot_nonce,
    configuration_version,
    standing,
    rtt_ns,
    ..
  } = event
  else {
    return;
  };
  assert_ne!(from.0, me);
  assert_eq!(
    *boot_nonce,
    from.0 * 1_000,
    "the peer's announced boot nonce"
  );
  assert_eq!((*configuration_version, *standing), (7, Some(3)));
  let bound = 2 * (LATENCY_NS + LATENCY_NS / JITTER_DIVISOR);
  assert!(
    (2 * LATENCY_NS..=bound).contains(rtt_ns),
    "the probe's round trip is the network's: {rtt_ns}"
  );
}

/// A-67 H-2. Do: run three members on the plane. Expect: every member's probes are acknowledged, each
/// acknowledgement yields an `Acked` event announcing the peer's boot nonce, configuration version and standing,
/// with the probe's round trip within the network's two latencies and their jitter.
#[test]
fn acknowledgements_carry_the_peers_identity_standing_and_round_trip() {
  let mut fleet = Network::new();
  let acked = fleet.run_until(HORIZON_NS, |fleet| {
    (1..=MEMBERS).all(|me| {
      fleet.events.get(&me).is_some_and(|events| {
        events
          .iter()
          .any(|event| matches!(event, PlaneEvent::Acked { .. }))
      })
    })
  });
  assert!(acked, "every member saw an acknowledgement");
  for (me, events) in &fleet.events {
    events.iter().for_each(|event| check_acked(*me, event));
  }
  for me in 1..=MEMBERS {
    assert_eq!(fleet.members[&me].counts().refused_open, 0);
    assert_eq!(fleet.members[&me].counts().impersonated, 0);
  }
}

/// Prints every pair's detector evidence: what a failure to judge is diagnosed from.
fn print_evidence(fleet: &Network) {
  for me in 1..=MEMBERS {
    for peer in (1..=MEMBERS).filter(|peer| *peer != me) {
      let detector = fleet.members[&me].detector();
      let peer = hyper_swim::HostId(peer);
      eprintln!(
        "evidence {me}->{}: taken {:?} verdict {:?} report {:?} bound {:?} liveness {:?}",
        peer.0,
        detector.round_trips_taken(peer),
        detector.verdict(peer),
        detector.report(peer),
        detector.detection_bound(fleet.now_ns),
        fleet.liveness(me, peer.0)
      );
    }
    eprintln!(
      "evidence {me} counts {:?} now {}",
      fleet.members[&me].counts(),
      fleet.now_ns
    );
  }
}

/// The survivors whose own probes condemned `victim`, each checked to have done so within the bound its detector
/// stated. A survivor that learned the death from another's gossip (SWIM's dissemination) is not counted.
fn condemnations_by_own_probes(fleet: &Network, victim: u64) -> usize {
  let mut own = 0;
  for (survivor, member) in fleet
    .members
    .iter()
    .filter(|(id, _)| !fleet.killed.contains(id))
  {
    let Some(report) = member.detector().report(hyper_swim::HostId(victim)) else {
      continue;
    };
    if report.condemnations == 0 {
      continue;
    }
    own += 1;
    let (after, within) = (report.condemned_after, report.condemned_within);
    // A peer that never answered has no last answer to measure from: its bound is checked by the test, on the
    // network's clock from the kill.
    let never_answered = report.last_answer_ns.is_none() && after.is_none() && within.is_some();
    assert!(
      never_answered || matches!((after, within), (Some(after), Some(within)) if after <= within),
      "member {survivor} condemned within its stated bound: after {after:?}, within {within:?}"
    );
  }
  own
}

/// §4.8 membership, the liveness half of the far-link fix (the hyper-raft owner's review of `swim-pair-deadline`,
/// 2026-10-07): a pair the pool does not fit must still be judged, within a bound, before its own estimator has a
/// verdict. Do: run two near members and one far one, the far one killed from the start, so neither survivor ever
/// measures it and every survivor is far from it. Expect: both survivors come to hold it dead, at least one by its
/// own probes within the bound its detector stated, and neither survivor is condemned. Before the fix, a misfit
/// pair's probes were measurement only and an unanswered one condemned nothing: the dead far member was never held
/// dead.
#[test]
fn a_far_member_killed_before_any_far_pair_configures_is_condemned_by_every_survivor() {
  let far = [3u64];
  let members = 3u64;
  let victim = 3u64;
  let mut fleet = Network::sized(members, &far);
  fleet.killed.push(victim);
  let started = fleet.now_ns;
  let condemned = fleet.run_until(HORIZON_NS, |fleet| {
    (1..members).all(|me| matches!(fleet.liveness(me, victim), Some(Liveness::Dead) | None))
  });
  let took = std::time::Duration::from_nanos(fleet.now_ns.saturating_sub(started));
  // The victim never answered, so each condemning survivor's stated bound runs from the kill.
  let stated = (1..members)
    .filter_map(|me| {
      fleet.members[&me]
        .detector()
        .report(hyper_swim::HostId(victim))
        .and_then(|report| report.condemned_within)
    })
    .max();
  let wrongly: Vec<(u64, u64)> = fleet
    .condemned
    .iter()
    .copied()
    .filter(|(_, target)| *target != victim)
    .collect();
  assert!(condemned, "every survivor holds the far member dead");
  assert!(wrongly.is_empty(), "live members condemned: {wrongly:?}");
  assert!(
    condemnations_by_own_probes(&fleet, victim) > 0,
    "a survivor's own probes condemned the far member"
  );
  assert!(
    stated.is_some_and(|stated| took <= stated),
    "every survivor held it dead within the bound its detector stated: took {took:?}, stated {stated:?}"
  );
}

/// A-67 H-2, T-8.5 (a death report crosses live neighbours; its transmission budget is hyper-swim's gossip_transmits,
/// tested in hyper-raft's suite). Do: run three members until every pair is judged, then kill one (its datagrams
/// dropped both ways).
/// Expect: both survivors come to hold it dead (by their own probes within the detection bound their detector stated,
/// or by the other's gossip), at least one by its own probes, and neither ever holds the other survivor dead.
#[test]
fn a_killed_member_is_condemned_by_every_survivor_and_no_live_one_is() {
  let mut fleet = Network::new();
  let judged = fleet.run_until(HORIZON_NS, |fleet| {
    (1..=MEMBERS).all(|me| {
      (1..=MEMBERS).filter(|peer| *peer != me).all(|peer| {
        fleet.members[&me]
          .detector()
          .verdict(hyper_swim::HostId(peer))
          .is_some()
      })
    })
  });
  if !judged {
    print_evidence(&fleet);
  }
  assert!(judged, "every pair's detector configured from its evidence");
  fleet.killed.push(3);
  let started = fleet.now_ns;
  let condemned = fleet.run_until(started + HORIZON_NS, |fleet| {
    assert_ne!(
      fleet.liveness(1, 2),
      Some(Liveness::Dead),
      "member 1 condemned live member 2"
    );
    assert_ne!(
      fleet.liveness(2, 1),
      Some(Liveness::Dead),
      "member 2 condemned live member 1"
    );
    [1, 2]
      .iter()
      .all(|me| matches!(fleet.liveness(*me, 3), Some(Liveness::Dead) | None))
  });
  assert!(condemned, "both survivors condemned the killed member");
  let own = condemnations_by_own_probes(&fleet, 3);
  assert!(
    own > 0,
    "a survivor's own probes condemned the killed member"
  );
}

/// A-67 H-2. Do: deliver to member 1 a datagram member 2 sealed whose message claims member 3 as its sender.
/// Expect: the message is refused and counted, and member 1's detector learns nothing from it.
#[test]
fn a_message_claiming_another_sender_is_refused_and_counted() {
  let mut sender = MemberPlane::new(
    HostId(2),
    2_000,
    NonZeroUsize::new(3).unwrap(),
    SIM_RESOLUTION,
    RENEWAL,
  )
  .unwrap();
  sender
    .install_epoch(HostId(1), 1, &secret_between(1, 2), Role::Acceptor)
    .unwrap();
  let mut receiver = MemberPlane::new(
    HostId(1),
    1_000,
    NonZeroUsize::new(3).unwrap(),
    SIM_RESOLUTION,
    RENEWAL,
  )
  .unwrap();
  receiver
    .install_epoch(HostId(2), 1, &secret_between(1, 2), Role::Initiator)
    .unwrap();
  // Member 2's plane seals a probe claiming to be from member 3 (a forged `from` inside an authentic datagram).
  let forged = hyper_swim::codec::SwimMessage::Ping {
    from: hyper_swim::HostId(3),
    nonce: 9,
    boot_nonce: 3_000,
    configuration_version: 0,
    gossip: hyper_swim::codec::GossipBatch::Entries(&[]),
  };
  let mut encoded = Vec::new();
  forged.encode_into(&mut encoded);
  let mut sealed = Vec::new();
  let mut plane = hyper_datagram::Plane::new(
    2,
    hyper_datagram::PlaneLimits {
      max_peers: 2,
      epochs_per_peer: 2,
      window_limit: 64,
    },
  )
  .unwrap();
  plane
    .install_epoch(1, 1, &secret_between(1, 2), Role::Acceptor)
    .unwrap();
  plane.queue(1, &encoded).unwrap();
  plane.flush(|_, datagram| sealed = datagram.unwrap().to_vec());
  let mut events = Vec::new();
  receiver.receive(&mut sealed, 1, &mut Believing::default(), &mut events);
  assert!(
    events.is_empty(),
    "nothing folded from a forged sender: {events:?}"
  );
  assert_eq!(receiver.counts().impersonated, 1);
  assert_eq!(
    receiver.counts().refused_open,
    0,
    "the datagram itself was authentic"
  );
  drop(sender);
}

/// A-67 H-2 (learn-on-contact, §4.8). Do: run three members where member 1's fleet refuses to answer member 2 (a forged
/// identity, or a peer it is deaf to). Expect: member 2 sees acknowledgements from member 3 but never from member 1, and
/// member 1 counts every refused probe.
#[test]
fn a_probe_the_fleet_refuses_is_neither_answered_nor_folded() {
  let mut fleet = Network::new();
  fleet.fleets.get_mut(&1).unwrap().refuses = Some(2);
  let heard_from_3 = fleet.run_until(HORIZON_NS, |fleet| {
    fleet.events.get(&2).is_some_and(|events| {
      events
        .iter()
        .filter(|event| matches!(event, PlaneEvent::Acked { from, .. } if from.0 == 3))
        .count()
        > 3
    })
  });
  assert!(heard_from_3, "member 2 is answered by member 3");
  let from_1 = fleet.events[&2]
    .iter()
    .filter(|event| matches!(event, PlaneEvent::Acked { from, .. } if from.0 == 1))
    .count();
  assert_eq!(from_1, 0, "member 1 never answered member 2");
  assert!(
    fleet.members[&1].counts().refused_by_fleet > 0,
    "member 1 counted its refusals"
  );
}

/// Installs the configuration the council would commit for `dead`'s retirement and takes over, for `survivor`, each of
/// `dead`'s objects that ranks to it; every object must go to a survivor (`survivor` or `other`). How many `survivor`
/// took.
fn install_retirement(
  fleet_node: &mut FleetNode,
  placement: &RegionalConfiguration,
  (survivor, other, dead): (HostId, HostId, HostId),
) -> usize {
  let mut retired = placement.clone();
  retired.take_over(dead, 3);
  let configuration = retired
    .configuration_for(survivor)
    .unwrap_or_else(|| Configuration::solo(survivor));
  fleet_node.install_configuration(configuration, &retired.members);
  let mut taken = 0;
  for index in 0..BACKED_OBJECTS {
    let object = ObjectId::new(dead, index);
    match retired.lineage(dead, object) {
      Lineage::Successor { successor, .. } if successor == survivor => {
        fleet_node.track_object_owner(object, survivor);
        assert_eq!(fleet_node.object_owner(object), Some(survivor));
        taken += 1;
      }
      Lineage::Successor { successor, .. } => assert_eq!(successor, other, "a survivor succeeds"),
      lineage => panic!("{object:?} was not handed to a survivor: {lineage:?}"),
    }
  }
  taken
}

/// AC (§4.8, boot step 6 live; ported from `membership_takeover.rs`'s probe-session test to the membership plane, A-67
/// H-2b). Do: three members back member 3's objects; once every pair is judged, member 3 falls silent. Member 1 folds
/// its detector's view into its `FleetNode` as the membership task does, and installs the configuration the council
/// would commit for member 3's retirement. Expect: member 1 holds member 3 dead, member 3 leaves its neighbourhood,
/// every one of member 3's objects goes to a survivor, and member 1 owns those that rank to it, at least one.
#[test]
fn a_silent_member_is_condemned_and_its_objects_are_taken_over() {
  let (survivor, dead, other) = (HostId(1), HostId(3), HostId(2));
  let placement = RegionalConfiguration::formed(
    vec![survivor, other, dead],
    Quorum { f: 1 },
    std::collections::BTreeMap::new(),
    u64::try_from(Quorum { f: 1 }.candidates()).unwrap(),
    false,
  );
  let mut fleet_node = FleetNode::new(survivor, Quorum { f: 1 }, &[other, dead]);
  for index in 0..BACKED_OBJECTS {
    fleet_node
      .track_object(ObjectId::new(dead, index), dead, &placement)
      .unwrap();
  }
  let mut network = Network::new();
  let judged = network.run_until(HORIZON_NS, |network| {
    network.members[&1]
      .detector()
      .verdict(hyper_swim::HostId(3))
      .is_some()
  });
  assert!(judged, "member 1's detector judges member 3");
  network.killed.push(3);
  let started = network.now_ns;
  let condemned = network.run_until(started + HORIZON_NS, |network| {
    matches!(network.liveness(1, 3), Some(Liveness::Dead))
  });
  assert!(condemned, "member 1 condemned member 3");
  // The membership task's fold: the detector's belief about member 3, into the authority table.
  let belief = network.members[&1]
    .detector()
    .membership()
    .state(hyper_swim::HostId(3))
    .unwrap();
  apply_peer_state(&mut fleet_node, dead, Some(belief));
  assert_eq!(
    fleet_node
      .membership()
      .state(dead)
      .map(|state| state.liveness),
    Some(Liveness::Dead)
  );
  let taken = install_retirement(&mut fleet_node, &placement, (survivor, other, dead));
  assert!(
    !fleet_node.configuration().neighbourhood.contains(&dead),
    "member 3 left the neighbourhood"
  );
  assert!(taken > 0, "member 1 took over the objects that rank to it");
}

/// §4.8 "Leases and reads" (AUD-08), found on two Docker networks joined by a shaped router (2026-10-07,
/// `docs/wip/bench/multiregion/run.sh`). Do: run an owner, one near holder of its objects and four members across
/// a far link, for many probe rounds, and record when the owner's probes the holder answered were sent. Expect:
/// the owner is never without a fresh answer from its holder for longer than the lease bound. The owner's lease
/// counts only answers to probes sent within the bound; if those answers come only from the detector's probe
/// rotation, a round that spends periods on far members leaves the near holder unprobed past the bound, and the
/// owner refuses its own objects' latest state while every member is alive.
#[test]
fn an_owner_is_answered_by_its_holder_within_the_lease_bound_while_far_members_stretch_the_round() {
  let (owner, holder) = (1u64, 2u64);
  let far = [3u64, 4, 5, 6];
  let mut fleet = Network::sized(6, &far);
  fleet.fleets.get_mut(&owner).unwrap().holders = vec![holder];
  fleet.run_until(LEASE_OBSERVED_NS, |_| false);
  let mut sent: Vec<u64> = fleet.events[&owner]
    .iter()
    .filter_map(|event| match event {
      PlaneEvent::Acked { from, sent_ns, .. } if from.0 == holder => Some(*sent_ns),
      _ => None,
    })
    .collect();
  sent.sort_unstable();
  // From the first answer on: before it the pair is still being measured.
  let longest_gap = sent
    .windows(2)
    .map(|pair| pair[1] - pair[0])
    .chain(sent.last().map(|last| fleet.now_ns - last))
    .max()
    .unwrap_or(u64::MAX);
  eprintln!(
    "answered probes {}, longest gap {} ms, now {} ms",
    sent.len(),
    longest_gap / 1_000_000,
    fleet.now_ns / 1_000_000
  );
  // Non-vacuity: the renewals ran and were answered.
  let counts = fleet.members[&owner].counts();
  assert!(
    counts.renewals_answered > 0,
    "the holder answered renewals: {counts:?}"
  );
  assert!(
    longest_gap <= LEASE_BOUND_NS,
    "the owner went {} ms without a fresh answer from its holder, past the {} ms lease bound",
    longest_gap / 1_000_000,
    LEASE_BOUND_NS / 1_000_000
  );
}

/// §4.8 membership (`docs/bugs/2026-10-07-a-far-member-condemns-the-near-side-by-its-pooled-deadline.md`; fixed in the
/// vendored hyper-swim, the diff owed upstream to `../hyper-raft`). Do: run two near members and four far ones, the far link 100 ms one way and
/// lossless, every member alive throughout. Expect: no member condemns a live one. Measured 2026-10-07: every far member
/// condemned both near members 20 to 49 times per 10 s for the whole run, each crossing probe judged by a deadline of
/// 1.9 to 2.7 ms on a 200 ms path: the far member's pooled estimator, fed mostly by its 1 ms same-side round trips,
/// judged the pair, and the pair's own estimator never took over. Each peer joins with its handshake's round trip,
/// as the daemon's members do.
#[test]
fn no_live_member_is_condemned_across_a_lossless_far_link() {
  let far = [3u64, 4, 5, 6];
  let mut fleet = Network::sized(6, &far);
  fleet.run_until(LEASE_OBSERVED_NS, |_| false);
  let mut by_pair: BTreeMap<(u64, u64), usize> = BTreeMap::new();
  for pair in &fleet.condemned {
    *by_pair.entry(*pair).or_default() += 1;
  }
  assert!(
    by_pair.is_empty(),
    "live members condemned, (member, condemned) -> count: {by_pair:?}"
  );
}

/// §4.8 membership, the counter-case to [`no_live_member_is_condemned_across_a_lossless_far_link`]: a pair the pool
/// does not fit is measured before it is judged, and must still be judged once it has its own verdict. Do: run two
/// near members and four far ones until every pair is judged by its own estimator, then kill a far member. Expect:
/// every survivor comes to hold it dead (by its own probes, within the bound its detector stated, or by gossip),
/// near and far alike, at least one by its own probes, and no live member is ever condemned.
#[test]
fn a_far_member_that_dies_is_condemned_by_every_survivor_and_no_live_one_is() {
  let far = [3u64, 4, 5, 6];
  let members = 6u64;
  let mut fleet = Network::sized(members, &far);
  let judged = fleet.run_until(HORIZON_NS, |fleet| {
    (1..=members).all(|me| {
      (1..=members).filter(|peer| *peer != me).all(|peer| {
        fleet.members[&me]
          .detector()
          .report(hyper_swim::HostId(peer))
          .is_some_and(|report| report.configured)
      })
    })
  });
  let unconfigured: Vec<(u64, u64)> = (1..=members)
    .flat_map(|me| {
      (1..=members)
        .filter(move |peer| *peer != me)
        .map(move |peer| (me, peer))
    })
    .filter(|(me, peer)| {
      !fleet.members[me]
        .detector()
        .report(hyper_swim::HostId(*peer))
        .is_some_and(|report| report.configured)
    })
    .collect();
  assert!(
    judged,
    "every pair's own estimator configured from its evidence; not configured: {unconfigured:?}"
  );
  assert!(
    fleet.condemned.is_empty(),
    "no live member condemned while measuring: {:?}",
    fleet.condemned
  );
  let victim = 6u64;
  fleet.killed.push(victim);
  let started = fleet.now_ns;
  let condemned = fleet.run_until(started + HORIZON_NS, |fleet| {
    (1..members).all(|me| matches!(fleet.liveness(me, victim), Some(Liveness::Dead) | None))
  });
  let wrongly: Vec<(u64, u64)> = fleet
    .condemned
    .iter()
    .copied()
    .filter(|(_, target)| *target != victim)
    .collect();
  assert!(condemned, "every survivor holds the far member dead");
  assert!(wrongly.is_empty(), "live members condemned: {wrongly:?}");
  assert!(
    condemnations_by_own_probes(&fleet, victim) > 0,
    "a survivor's own probes condemned the far member, within its stated bound"
  );
}
