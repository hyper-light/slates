//! The membership plane by use (A-67 H-2; `docs/wip/transport-quic.md` §6): members exchange sealed datagrams over an
//! in-memory network on simulated time, the way the control shard's task drives one plane over its UDP socket.
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::num::NonZeroUsize;

use slates_cluster::fleet::{FleetNode, apply_peer_state};
use slates_cluster::member_plane::{Fleet, MemberPlane, PlaneEvent};
use slates_db::register::{
  Configuration, HostId, Lineage, ObjectId, Quorum, RegionalConfiguration,
};

use hyper_datagram::{ExporterSecret, Role, SECRET_BYTES};
use hyper_swim::membership::Liveness;

/// Shape: the members of the simulated fleet.
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
/// Shape: the simulated time a run may take before it is a failure: long past the detector's evidence and its
/// detection bound at this latency (hundreds of periods).
const HORIZON_NS: u64 = 600_000_000_000;

/// A fleet that believes and answers every member (but one it refuses to answer, as for a forged identity), announcing
/// version 7 and standing 3.
#[derive(Default)]
struct Believing {
  refuses: Option<u64>,
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
}

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
  members: BTreeMap<u64, MemberPlane>,
  fleets: BTreeMap<u64, Believing>,
  in_flight: Vec<InFlight>,
  events: BTreeMap<u64, Vec<PlaneEvent>>,
  /// Members whose datagrams are dropped both ways: killed.
  killed: Vec<u64>,
  now_ns: u64,
  /// The jitter generator's state (xorshift64), deterministic.
  jitter: u64,
}

impl Network {
  fn new() -> Network {
    let mut members = BTreeMap::new();
    for me in 1..=MEMBERS {
      let mut plane = MemberPlane::new(
        HostId(me),
        me * 1_000,
        NonZeroUsize::new(usize::try_from(MEMBERS).unwrap()).unwrap(),
      )
      .unwrap();
      for peer in (1..=MEMBERS).filter(|peer| *peer != me) {
        // The lower member id dials the canonical connection (transport-quic.md §6).
        let role = if me < peer {
          Role::Initiator
        } else {
          Role::Acceptor
        };
        plane
          .install_epoch(HostId(peer), 1, &secret_between(me, peer), role)
          .unwrap();
        plane.join(HostId(peer));
      }
      members.insert(me, plane);
    }
    let fleets = (1..=MEMBERS).map(|me| (me, Believing::default())).collect();
    Network {
      members,
      fleets,
      in_flight: Vec::new(),
      events: BTreeMap::new(),
      killed: Vec::new(),
      now_ns: 1,
      jitter: 0x9E37_79B9_7F4A_7C15,
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
      let lands_ns = now + self.next_latency();
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
    assert!(
      matches!((after, within), (Some(after), Some(within)) if after <= within),
      "member {survivor} condemned within its stated bound: after {after:?}, within {within:?}"
    );
  }
  own
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
  let mut sender = MemberPlane::new(HostId(2), 2_000, NonZeroUsize::new(3).unwrap()).unwrap();
  sender
    .install_epoch(HostId(1), 1, &secret_between(1, 2), Role::Acceptor)
    .unwrap();
  let mut receiver = MemberPlane::new(HostId(1), 1_000, NonZeroUsize::new(3).unwrap()).unwrap();
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
