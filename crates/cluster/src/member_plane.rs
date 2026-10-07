//! The membership plane (A-67 H-2; `docs/wip/transport-quic.md` §6): one owner for a node's failure detector
//! (hyper-swim's `Detector`) and the sealed datagram plane its probes ride (hyper-datagram's `Plane`). It replaces the
//! per-peer probe tasks, each of which held a detector and a view of its own over a QUIC probe session.
//!
//! **Sans-io.** The owner feeds it the time, received datagrams and each peer's epoch keys. It returns the datagrams
//! to send and the events the fleet folds:
//! - an acknowledgement, with its announced identity, standing and round trip;
//! - an identity heard from a peer, for learn-on-contact.
//!
//! The shape is hyper-swim's own five-process test (hyper-raft `crates/hyper-swim/tests/cluster.rs`):
//! 1. `step` polls the detector and queues the probe, its relay requests and its anti-entropy chunks;
//! 2. `receive` opens a datagram and hands each message to the detector;
//! 3. `flush` seals at most one datagram per peer.
//!
//! **Why one owner.** A per-peer detector held a full view each: O(N) detectors and views a node. One detector holds
//! one measured estimator per pair and a view bounded by the placement (hyper-raft `docs/timing.md` §2.7). Its period
//! costs 17–28 % less than slates' detector, with no allocations (hyper-raft `docs/benchmarks.md`). Probes leave the
//! QUIC congestion window (RFC 9221 §5) for a socket of their own, so bulk traffic cannot delay one.
//!
//! **Who sent it, and whom the fleet believes.**
//! - A datagram is authenticated by its epoch's key, expanded from the TLS exporter of a session whose certificate the
//!   owner authenticated and rostered. A message whose claimed sender is not the datagram's sender is refused and
//!   counted.
//! - Before anything reaches the detector, the fleet decides ([`Fleet`]), as it did on a probe session:
//!   - a probe is answered only for an identity learn-on-contact accepts, and the fleet records the answer for the
//!     owner lease's holder side;
//!   - an acknowledgement, an indirect answer or an anti-entropy chunk is folded only from an admitted identity;
//!   - a relay request is served only for a target this node keeps direct contact with.
//!
//! **The owner lease's renewals** ride the same plane ([`crate::lease_renewal`]): each step the plane probes every
//! holder the fleet names ([`Fleet::lease_holders`]) that it has not probed within the renewal interval, and an
//! answer to one is an [`PlaneEvent::Acked`] like the detector's own, timed from its own send.

use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroUsize;

use hyper_datagram::{AdmitAll, Epoch, LENGTH_BYTES, OVERHEAD_BYTES, Plane, PlaneLimits};
use hyper_datagram::{ExporterSecret, Role};
use hyper_swim::codec::{Coordinate, GossipBatch, SwimMessage, gossip_capacity};
use hyper_swim::detector::{Detector, PingReq};
use hyper_swim::membership::MemberState;
use hyper_timing::Exposure;
use slates_db::register::HostId;

use crate::lease_renewal::{LeaseRenewal, Renewal, Renewals};

pub use hyper_datagram::Refusal as PlaneRefusal;
pub use hyper_datagram::UNMEASURED_DATAGRAM_BYTES;

/// Shape: the epochs one peer keeps. Two let a new connection's datagrams and the last of the old one's overlap for a
/// handshake round trip (hyper-datagram `PlaneLimits::epochs_per_peer`).
const EPOCHS_PER_PEER: usize = 2;

/// Shape: the widest a replay window may grow, in counters. RFC 4303 §3.4.3's default is 64; the plane widens to the
/// reordering it measures, within this. 1,024 counters is 128 bytes of bitmap a peer.
const WINDOW_LIMIT: usize = 1_024;

/// The fleet's decisions the plane defers to before it touches the detector (§4.8): whom it answers, whom it believes,
/// and what an answer announces.
pub trait Fleet {
  /// The newest configuration version this node knows (installed, or announced by a peer): what its probes announce.
  fn announced_version(&self) -> u64;
  /// Whether to answer `prober`'s probe, which announced `boot_nonce` and `configuration_version`. `None` refuses: a
  /// forged identity (learn-on-contact refused it) or a peer this node is deaf to; neither its gossip nor the probe
  /// is folded. `Some((version, standing))` answers with the owner-lease evidence: the configuration version this
  /// node read the prober's `standing` at (§4.8 "Leases and reads"). The fleet records that it answered.
  fn answer(
    &mut self,
    prober: HostId,
    boot_nonce: u64,
    configuration_version: u64,
  ) -> Option<(u64, Option<u64>)>;
  /// Whether `from`, announcing `boot_nonce`, is believed: an acknowledgement, an indirect answer or an anti-entropy
  /// chunk from it is folded only then.
  fn admit(&mut self, from: HostId, boot_nonce: u64) -> bool;
  /// Whether this node relays a probe to `target` for another member: only a member it keeps direct contact with, so
  /// no authenticated peer can name arbitrary targets (AUD-15's bound).
  fn relays_to(&self, target: HostId) -> bool;
  /// The holders whose answers confirm this node's owner lease, this node excluded: its neighbourhood, settled and
  /// current (§4.8 "Leases and reads"). Written into `into`, which arrives empty.
  fn lease_holders(&self, into: &mut Vec<HostId>);
}

/// What a received datagram told the fleet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaneEvent {
  /// A peer acknowledged this node's probe.
  Acked {
    /// The peer, as the datagram's key authenticated it.
    from: HostId,
    /// The boot nonce it announced: what its member id is validated against.
    boot_nonce: u64,
    /// The configuration version it read `standing` from.
    configuration_version: u64,
    /// Its view of this node's standing.
    standing: Option<u64>,
    /// When this node sent the probe, on the owner's clock.
    sent_ns: u64,
    /// The probe's round trip: the acknowledgement's receive time less the probe's send time.
    rtt_ns: u64,
  },
  /// A peer announced its identity in a probe, an indirect answer or an anti-entropy exchange.
  Heard {
    /// The peer, as the datagram's key authenticated it.
    from: HostId,
    /// The boot nonce it announced.
    boot_nonce: u64,
    /// The configuration version it announced (zero where the message carries none).
    configuration_version: u64,
  },
}

/// Why the plane refused, counted by kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlaneCounts {
  /// Datagrams the plane refused to open: unknown sender or epoch, replay, a bad tag, corruption.
  pub refused_open: u64,
  /// Messages that did not decode.
  pub malformed: u64,
  /// Messages whose claimed sender was not the datagram's.
  pub impersonated: u64,
  /// Messages the fleet refused: a forged identity, or a peer this node is deaf to.
  pub refused_by_fleet: u64,
  /// Messages the plane refused to queue: no epoch for the peer, or a datagram already full.
  pub refused_queue: u64,
  /// Membership updates the detector's bounded view refused.
  pub view_full: u64,
  /// Relay requests this node sent: its direct probe went unanswered by its deadline (the indirect stage, §4.8).
  pub relays_asked: u64,
  /// Probes this node relayed for another member.
  pub relayed: u64,
  /// Indirect answers this node credited: a relay reached the target this node's direct probe could not.
  pub indirect_acked: u64,
  /// Lease renewals this node sent its holders ([`crate::lease_renewal`]).
  pub renewals_sent: u64,
  /// Lease renewals its holders answered, each an [`PlaneEvent::Acked`].
  pub renewals_answered: u64,
  /// Whether renewals stopped because a detector nonce reached their range (1) or not (0).
  pub renewals_exhausted: u64,
}

/// A node's failure detector and the datagram plane its probes ride.
pub struct MemberPlane {
  local: HostId,
  boot_nonce: u64,
  detector: Detector,
  plane: Plane,
  /// Gossip entries a message carries: what fits an unmeasured path beside the largest message.
  gossip_room: usize,
  /// Members one anti-entropy chunk carries.
  view_room: usize,
  batch: Vec<(hyper_swim::HostId, MemberState)>,
  encoded: Vec<u8>,
  requests: Vec<PingReq>,
  /// Probes this node relays: by target, the relay's nonce, who asked and the asker's nonce. One a target, so bounded
  /// by the view.
  relaying: BTreeMap<u64, (u64, hyper_swim::HostId, u64)>,
  /// This node's probes awaiting an answer: nonce, target and send time, oldest first, at most the view's bound.
  sent: VecDeque<(u64, u64, u64)>,
  sent_bound: usize,
  /// The peers [`join`](Self::join)ed and not removed: keyed and addressable. Bounded by the plane's peers.
  joined: std::collections::BTreeSet<u64>,
  /// The owner lease's renewals over its holders.
  renewals: Renewals,
  /// The holders the fleet named this step, and the renewals due: reused, so a step allocates nothing.
  holders: Vec<HostId>,
  due: Vec<Renewal>,
  counts: PlaneCounts,
}

impl std::fmt::Debug for MemberPlane {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("MemberPlane")
      .field("local", &self.local)
      .field("counts", &self.counts)
      .finish_non_exhaustive()
  }
}

impl MemberPlane {
  /// A plane for `local`, announcing `boot_nonce`, whose view holds at most `members` hosts (itself included: what the
  /// placement lets this node know) and whose plane keeps keys for at most `members − 1` peers.
  /// `resolution` is the owner's clock's: the least step of the readings it stamps the plane with (A-67; hyper-swim
  /// bounds its wake lateness `G` below by it). `renewal` is how the owner lease is renewed ([`crate::lease_renewal`]).
  pub fn new(
    local: HostId,
    boot_nonce: u64,
    members: NonZeroUsize,
    resolution: std::time::Duration,
    renewal: LeaseRenewal,
  ) -> Result<Self, PlaneRefusal> {
    let limits = PlaneLimits {
      max_peers: members.get().saturating_sub(1).max(1),
      epochs_per_peer: EPOCHS_PER_PEER,
      window_limit: WINDOW_LIMIT,
    };
    let plane = Plane::new(local.0, limits)?;
    let detector = Detector::new(
      hyper_swim::HostId(local.0),
      Exposure::new(),
      members,
      resolution,
    );
    let mut member = MemberPlane {
      local,
      boot_nonce,
      detector,
      plane,
      gossip_room: 0,
      view_room: 0,
      batch: Vec::new(),
      encoded: Vec::new(),
      requests: Vec::new(),
      relaying: BTreeMap::new(),
      sent: VecDeque::with_capacity(members.get()),
      sent_bound: members.get(),
      joined: std::collections::BTreeSet::new(),
      renewals: Renewals::new(renewal),
      holders: Vec::new(),
      due: Vec::new(),
      counts: PlaneCounts::default(),
    };
    member.gossip_room = member.room_beside_ack();
    member.view_room = member.room_beside_sync();
    Ok(member)
  }

  /// The room an unmeasured path leaves a datagram's single message.
  fn message_room() -> usize {
    UNMEASURED_DATAGRAM_BYTES
      .saturating_sub(OVERHEAD_BYTES)
      .saturating_sub(LENGTH_BYTES)
  }

  /// The gossip entries that fit beside the largest message, an acknowledgement with this node's coordinate.
  fn room_beside_ack(&mut self) -> usize {
    let coordinate = *self.detector.coordinate();
    SwimMessage::Ack {
      from: hyper_swim::HostId(self.local.0),
      nonce: u64::MAX,
      boot_nonce: self.boot_nonce,
      configuration_version: u64::MAX,
      standing: Some(u64::MAX),
      gossip: GossipBatch::Entries(&[]),
      coordinate: Coordinate::Held(&coordinate),
    }
    .encode_into(&mut self.encoded);
    gossip_capacity(Self::message_room(), self.encoded.len())
  }

  /// The members that fit beside an empty anti-entropy chunk.
  fn room_beside_sync(&mut self) -> usize {
    SwimMessage::Sync {
      from: hyper_swim::HostId(self.local.0),
      boot_nonce: self.boot_nonce,
      digest: u64::MAX,
      pull: true,
      gossip: GossipBatch::Entries(&[]),
    }
    .encode_into(&mut self.encoded);
    gossip_capacity(Self::message_room(), self.encoded.len())
  }

  /// Installs `peer`'s epoch from its canonical connection (`docs/wip/transport-quic.md` §6, H-2), with this node's
  /// `role` on it. The peer is not judged until it is also [`join`](Self::join)ed, once the owner can address it.
  pub fn install_epoch(
    &mut self,
    peer: HostId,
    epoch: Epoch,
    secret: &ExporterSecret,
    role: Role,
  ) -> Result<(), PlaneRefusal> {
    self.plane.install_epoch(peer.0, epoch, secret, role)
  }

  /// Joins `peer` to the detector's view, so its probes are judged: the owner calls it once the peer is both keyed and
  /// addressable. A probe that cannot even be sent is never a miss: a member joined while unaddressable would be
  /// suspected and condemned for the owner's own lack of an address (KIND, 2026-10-04, a peer's DNS name not yet
  /// published at a fresh install). Counted when the bounded view refuses it.
  ///
  /// `handshake_rtt` is the round trip the owner measured on the session that keyed `peer`: until this member measures
  /// one of its own, its first probes of `peer` wait on it instead of hyper-swim's 1 s initial wait (a LAN's round
  /// trip is about 100 µs, so detection at join is no longer bounded by a default).
  pub fn join(&mut self, peer: HostId, handshake_rtt: Option<std::time::Duration>) {
    let peer_id = hyper_swim::HostId(peer.0);
    let joined = match handshake_rtt {
      Some(rtt) => self.detector.join_measured(peer_id, rtt),
      None => self.detector.join(peer_id),
    };
    if joined.is_err() {
      self.counts.view_full = self.counts.view_full.saturating_add(1);
    }
    self.joined.insert(peer.0);
  }

  /// Forgets `peer`'s keys and pending messages (a retired member id).
  pub fn remove_peer(&mut self, peer: HostId) {
    self.plane.remove_peer(peer.0);
    self.relaying.remove(&peer.0);
    self.joined.remove(&peer.0);
  }

  /// Tells the detector a belief the fleet holds about `peer` (a death it learned outside the detector: a restarted
  /// peer's old id, or a retirement), so the detector gossips it and a live peer refutes it with a higher incarnation.
  /// The detector's own incarnation order decides whether it takes it. Counted when the bounded view refuses it.
  pub fn apply(&mut self, peer: HostId, state: MemberState) {
    if self
      .detector
      .apply(hyper_swim::HostId(peer.0), state)
      .is_err()
    {
      self.counts.view_full = self.counts.view_full.saturating_add(1);
    }
  }

  /// The largest datagram the path to `peer` carries, from the transport's measurement of it.
  pub fn set_path(&mut self, peer: HostId, max_datagram: usize) -> Result<(), PlaneRefusal> {
    self.plane.set_path(peer.0, max_datagram)
  }

  /// When to [`step`](Self::step) next, on the owner's clock (`None`: on the next datagram).
  pub fn wake(&self) -> Option<u64> {
    match (self.detector.wake(), self.renewals.next_due()) {
      (Some(detector), Some(renewal)) => Some(detector.min(renewal)),
      (detector, renewal) => detector.or(renewal),
    }
  }

  /// The detector: its view, verdicts and reports.
  pub fn detector(&self) -> &Detector {
    &self.detector
  }

  /// The detector's belief about `peer`, if it holds one.
  pub fn belief(&self, peer: HostId) -> Option<MemberState> {
    self.detector.membership().state(hyper_swim::HostId(peer.0))
  }

  /// What the plane refused, by kind.
  pub fn counts(&self) -> PlaneCounts {
    self.counts
  }

  /// Queues `message` for `to`; a refusal is a lost message, which the detector measures as one, and is counted.
  fn send(&mut self, to: u64, message: &SwimMessage<'_>) {
    message.encode_into(&mut self.encoded);
    if self.plane.queue(to, &self.encoded).is_err() {
      self.counts.refused_queue = self.counts.refused_queue.saturating_add(1);
    }
  }

  /// Remembers a probe's send time, dropping the oldest past the bound.
  fn remember_sent(&mut self, nonce: u64, to: u64, sent_ns: u64) {
    if self.sent.len() >= self.sent_bound {
      self.sent.pop_front();
    }
    self.sent.push_back((nonce, to, sent_ns));
  }

  /// The send time of this node's probe `nonce` to `to`, taken out.
  fn take_sent(&mut self, nonce: u64, to: u64) -> Option<u64> {
    let at = self
      .sent
      .iter()
      .position(|(held, target, _)| *held == nonce && *target == to)?;
    self.sent.remove(at).map(|(_, _, sent_ns)| sent_ns)
  }

  /// Polls the detector at `now_ns` and queues what it asks: its relay requests, the period's probe (carrying this
  /// node's gossip, its suspicion of the target first) and its anti-entropy chunks.
  pub fn step(&mut self, now_ns: u64, fleet: &mut impl Fleet) {
    let mut requests = std::mem::take(&mut self.requests);
    let ping = self.detector.poll(now_ns, &mut requests);
    let local = hyper_swim::HostId(self.local.0);
    self.counts.relays_asked = self
      .counts
      .relays_asked
      .saturating_add(u64::try_from(requests.len()).unwrap_or(u64::MAX));
    for request in &requests {
      self.send(
        request.relay.0,
        &SwimMessage::PingReq {
          from: local,
          target: request.target,
          nonce: request.nonce,
          gossip: GossipBatch::Entries(&[]),
        },
      );
    }
    self.requests = requests;
    let mut batch = std::mem::take(&mut self.batch);
    if let Some(ping) = ping {
      self
        .detector
        .ping_gossip_into(ping.to, self.gossip_room, &mut batch);
      self.send(
        ping.to.0,
        &SwimMessage::Ping {
          from: local,
          nonce: ping.nonce,
          boot_nonce: self.boot_nonce,
          configuration_version: fleet.announced_version(),
          gossip: GossipBatch::Entries(&batch),
        },
      );
      self.remember_sent(ping.nonce, ping.to.0, now_ns);
      self
        .renewals
        .detector_probed(HostId(ping.to.0), ping.nonce, now_ns);
    }
    self.renew(now_ns, fleet);
    while let Some(chunk) = self.detector.sync_into(self.view_room, &mut batch) {
      self.send(
        chunk.to.0,
        &SwimMessage::Sync {
          from: local,
          boot_nonce: self.boot_nonce,
          digest: chunk.digest,
          pull: chunk.pull,
          gossip: GossipBatch::Entries(&batch),
        },
      );
    }
    self.batch = batch;
  }

  /// Probes the lease holders due a renewal at `now_ns` ([`crate::lease_renewal`]): the detector's probe, without the
  /// detector, and without gossip.
  fn renew(&mut self, now_ns: u64, fleet: &mut impl Fleet) {
    let mut holders = std::mem::take(&mut self.holders);
    let mut due = std::mem::take(&mut self.due);
    holders.clear();
    due.clear();
    fleet.lease_holders(&mut holders);
    // Only a joined peer is keyed and addressable: a probe to any other could not be sent. Whether the detector
    // still holds it is no matter: a holder it condemned or forgot can still answer, and its answer still confirms.
    holders.retain(|holder| *holder != self.local && self.joined.contains(&holder.0));
    self.renewals.due(now_ns, &holders, &mut due);
    let local = hyper_swim::HostId(self.local.0);
    let announced = fleet.announced_version();
    for renewal in &due {
      self.send(
        renewal.to.0,
        &SwimMessage::Ping {
          from: local,
          nonce: renewal.nonce,
          boot_nonce: self.boot_nonce,
          configuration_version: announced,
          gossip: GossipBatch::Entries(&[]),
        },
      );
    }
    self.counts.renewals_sent = self
      .counts
      .renewals_sent
      .saturating_add(u64::try_from(due.len()).unwrap_or(u64::MAX));
    self.counts.renewals_exhausted = u64::from(self.renewals.exhausted());
    self.holders = holders;
    self.due = due;
  }

  /// Opens one received `datagram`, stamped `stamp_ns` on the owner's clock, hands each message to the detector, queues
  /// the answers it owes, and appends what the fleet must fold to `events`.
  pub fn receive(
    &mut self,
    datagram: &mut [u8],
    stamp_ns: u64,
    fleet: &mut impl Fleet,
    events: &mut Vec<PlaneEvent>,
  ) {
    // The opened messages borrow the datagram, not the plane, so each is decoded in place and handled while the
    // detector and the plane are borrowed mutably: nothing is copied (hyper-swim's period allocates nothing).
    let Ok(opened) = self.plane.open(datagram, &AdmitAll) else {
      self.counts.refused_open = self.counts.refused_open.saturating_add(1);
      return;
    };
    let sender = opened.sender;
    for bytes in opened.messages() {
      match SwimMessage::decode(bytes) {
        Ok(message) if message_sender(&message) == sender => {
          self.handle(sender, message, stamp_ns, fleet, events);
        }
        Ok(_) => self.counts.impersonated = self.counts.impersonated.saturating_add(1),
        Err(_) => self.counts.malformed = self.counts.malformed.saturating_add(1),
      }
    }
  }

  fn handle(
    &mut self,
    sender: u64,
    message: SwimMessage<'_>,
    stamp_ns: u64,
    fleet: &mut impl Fleet,
    events: &mut Vec<PlaneEvent>,
  ) {
    let local = hyper_swim::HostId(self.local.0);
    let from = HostId(sender);
    match message {
      SwimMessage::Ping {
        nonce,
        boot_nonce,
        configuration_version,
        gossip,
        ..
      } => {
        let Some((version, standing)) = fleet.answer(from, boot_nonce, configuration_version)
        else {
          self.counts.refused_by_fleet = self.counts.refused_by_fleet.saturating_add(1);
          return;
        };
        self.detector.apply_gossip(gossip);
        let ack = self.detector.on_ping(hyper_swim::HostId(sender));
        let mut batch = std::mem::take(&mut self.batch);
        self
          .detector
          .ack_gossip_into(ack.to, self.gossip_room, &mut batch);
        let coordinate = *self.detector.coordinate();
        self.send(
          ack.to.0,
          &SwimMessage::Ack {
            from: local,
            nonce,
            boot_nonce: self.boot_nonce,
            configuration_version: version,
            standing,
            gossip: GossipBatch::Entries(&batch),
            coordinate: Coordinate::Held(&coordinate),
          },
        );
        self.batch = batch;
        events.push(PlaneEvent::Heard {
          from,
          boot_nonce,
          configuration_version,
        });
      }
      SwimMessage::Ack {
        nonce,
        boot_nonce,
        configuration_version,
        standing,
        gossip,
        coordinate,
        ..
      } => {
        if !fleet.admit(from, boot_nonce) {
          self.counts.refused_by_fleet = self.counts.refused_by_fleet.saturating_add(1);
          return;
        }
        self.detector.apply_gossip(gossip);
        self
          .detector
          .learn_coordinate(hyper_swim::HostId(sender), coordinate);
        match self.relaying.get(&sender) {
          // An answer to a probe this node relayed goes back to the member that asked.
          Some(&(relayed, asker, asked)) if relayed == nonce => {
            self.relaying.remove(&sender);
            self.send(
              asker.0,
              &SwimMessage::IndirectAck {
                from: local,
                target: hyper_swim::HostId(sender),
                nonce: asked,
                boot_nonce: self.boot_nonce,
                gossip: GossipBatch::Entries(&[]),
              },
            );
          }
          _ => {
            self
              .detector
              .on_ack(hyper_swim::HostId(sender), nonce, stamp_ns);
            let renewal = self.renewals.answered(from, nonce);
            if renewal.is_some() {
              self.counts.renewals_answered = self.counts.renewals_answered.saturating_add(1);
            }
            let credited = renewal.or_else(|| self.take_sent(nonce, sender));
            if let Some(sent_ns) = credited {
              events.push(PlaneEvent::Acked {
                from,
                boot_nonce,
                configuration_version,
                standing,
                sent_ns,
                rtt_ns: stamp_ns.saturating_sub(sent_ns),
              });
            }
          }
        }
      }
      SwimMessage::PingReq {
        target,
        nonce,
        gossip,
        ..
      } => {
        if !fleet.relays_to(HostId(target.0)) {
          self.counts.refused_by_fleet = self.counts.refused_by_fleet.saturating_add(1);
          return;
        }
        self.detector.apply_gossip(gossip);
        let ping = self.detector.on_ping_req(target);
        self.counts.relayed = self.counts.relayed.saturating_add(1);
        self
          .relaying
          .insert(target.0, (ping.nonce, hyper_swim::HostId(sender), nonce));
        self.send(
          target.0,
          &SwimMessage::Ping {
            from: local,
            nonce: ping.nonce,
            boot_nonce: self.boot_nonce,
            configuration_version: fleet.announced_version(),
            gossip: GossipBatch::Entries(&[]),
          },
        );
      }
      SwimMessage::IndirectAck {
        target,
        nonce,
        boot_nonce,
        gossip,
        ..
      } => {
        if !fleet.admit(from, boot_nonce) {
          self.counts.refused_by_fleet = self.counts.refused_by_fleet.saturating_add(1);
          return;
        }
        self.detector.apply_gossip(gossip);
        self.detector.on_indirect_ack(target, nonce, stamp_ns);
        self.counts.indirect_acked = self.counts.indirect_acked.saturating_add(1);
        events.push(PlaneEvent::Heard {
          from,
          boot_nonce,
          configuration_version: 0,
        });
      }
      SwimMessage::Sync {
        boot_nonce,
        digest,
        pull,
        gossip,
        ..
      } => {
        if !fleet.admit(from, boot_nonce) {
          self.counts.refused_by_fleet = self.counts.refused_by_fleet.saturating_add(1);
          return;
        }
        self
          .detector
          .on_sync(hyper_swim::HostId(sender), digest, pull, gossip);
        events.push(PlaneEvent::Heard {
          from,
          boot_nonce,
          configuration_version: 0,
        });
      }
    }
  }

  /// Seals what is queued, at most one datagram per peer, and hands each to `out` with its destination. A peer whose
  /// datagram the plane refused (its key spent) is counted; the detector measures the loss.
  pub fn flush(&mut self, mut out: impl FnMut(HostId, &[u8])) {
    let counts = &mut self.counts;
    self.plane.flush(|peer, datagram| match datagram {
      Ok(bytes) => out(HostId(peer), bytes),
      Err(_) => counts.refused_queue = counts.refused_queue.saturating_add(1),
    });
  }
}

/// The member a message claims to come from.
fn message_sender(message: &SwimMessage<'_>) -> u64 {
  match message {
    SwimMessage::Ping { from, .. }
    | SwimMessage::Ack { from, .. }
    | SwimMessage::PingReq { from, .. }
    | SwimMessage::IndirectAck { from, .. }
    | SwimMessage::Sync { from, .. } => from.0,
  }
}
