//! The **root configuration group** live over the simulated UDP fabric (§4.8, D-14 — "a root group across
//! regions holds region membership and cross-region promotions"): a multi-voter [`RootGroup`] elects a leader
//! through the full **pre-vote** round (Raft §9.6) over the transport, and a region-loss **promotion**
//! proposed on the leader replicates to the voter, commits at a majority, and applies to the root
//! configuration at *both* — the root-group Raft driven over real mutually-authenticated sessions, not the
//! lone-voter degenerate. The Raft dialect's safety is proven in `tests/raft.rs`; here we prove the root
//! group's own changes ride the transport and commit. Test by use (R5); the cross-region daemon deployment
//! that carries this in production is a further gate (owed).

// Test harness: an unwrap or expect here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc::{Receiver, channel};

use rustix::net::{Ipv4Addr, SocketAddrV4};
use rustls::pki_types::PrivateKeyDer;
use slates_cluster::raft_wire::{RaftMessage, request_raft};
use slates_cluster::root_group::{RootCommand, RootGroup};
use slates_db::register::{HostId, ObjectId, RegionId};
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::SimRuntime;
use slates_rt::udp::UdpSocket;
use slates_transport::endpoint::Endpoint;
use slates_transport::handshake::Identity;

const NAME: &str = "slates-node";
const FRAME_CAP: usize = 16;
const LEADER: HostId = HostId(1);
const VOTER: HostId = HostId(2);
/// The two regions the root group starts with: `LOST` is the region that fails, `MIRROR` the one promoted to
/// serve it.
const LOST: RegionId = RegionId(0);
const MIRROR: RegionId = RegionId(1);

fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 1,
    tasks_per_shard: 64,
    timers_per_shard: 64,
    ring_entries: 64,
    step_budget_ns: 1_000_000_000,
    timer_tick_ns: 100_000,
    batch: 64,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
  }
}

fn self_signed(name: &str) -> Identity {
  let key = rcgen::KeyPair::generate().unwrap();
  let cert = rcgen::CertificateParams::new(vec![name.to_owned()])
    .unwrap()
    .self_signed(&key)
    .unwrap();
  Identity::from_der(
    cert.der().clone(),
    PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
  )
}

async fn recv_port(rx: Receiver<u16>) -> u16 {
  loop {
    if let Ok(p) = rx.try_recv() {
      return p;
    }
    slates_rt::futures::sleep(1_000).await;
  }
}

/// A two-voter root group on `node`, initial regions `[LOST, MIRROR]` and voter hosts `[LEADER, VOTER]`.
fn group(node: HostId) -> RootGroup {
  RootGroup::new(node, vec![LOST, MIRROR], vec![LEADER, VOTER])
}

/// A volume created in the `LOST` region (its creator host is `LEADER`); after the promotion it must be served
/// from `MIRROR`.
fn volume() -> ObjectId {
  ObjectId::new(LEADER, 7)
}

/// What the live distributed promotion produced at the leader and the voter: whether each dropped the lost
/// region and now homes the volume in the mirror.
struct Outcome {
  leader_leads: bool,
  leader_promoted: bool,
  voter_promoted: bool,
}

/// Whether `group` has applied the promotion: the lost region is gone from the membership and the volume homed
/// there is now served from the mirror.
fn promoted(group: &RootGroup) -> bool {
  !group.configuration().regions.contains(&LOST)
    && group.configuration().home_of(volume(), LOST) == MIRROR
}

/// Drives one live round: the leader (`1`) wins the voter's (`2`) pre-vote then vote over an authenticated
/// session, proposes promoting the mirror for the lost region, and replicates it — the promotion commits at
/// the majority and applies to the root configuration at both nodes, all over the transport.
fn run_distributed_promotion() -> Outcome {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];

  let leader_identity = self_signed(NAME);
  let leader_cert = leader_identity.certificate();
  let voter_identity = self_signed(NAME);
  let voter_cert = voter_identity.certificate();

  let (leader_port_tx, leader_port_rx) = channel::<u16>();
  let (voter_port_tx, voter_port_rx) = channel::<u16>();
  let (result_tx, result_rx) = channel::<Outcome>();
  let (voter_tx, voter_rx) = channel::<bool>();

  // The voter: handshake, then answer the pre-vote, the vote request, and the two appends into its group.
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = voter_port_tx.send(socket.local_addr().unwrap().port());
      let leader_port = recv_port(leader_port_rx).await;
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, leader_port);
      let mut endpoint = Endpoint::server(
        socket,
        peer,
        &voter_identity,
        std::slice::from_ref(&leader_cert),
        FRAME_CAP,
      )
      .unwrap();
      endpoint.establish().await.unwrap();

      let mut voter = group(VOTER);
      // Serve the pre-vote, the vote request, the append carrying the entry, then the commit heartbeat.
      for _ in 0..4 {
        endpoint
          .serve_once(|_, request| match RaftMessage::decode(&request) {
            Ok(message) => voter
              .answer(message)
              .map(|reply| reply.encode())
              .unwrap_or_default(),
            Err(_) => Vec::new(),
          })
          .await
          .unwrap();
      }
      let _ = voter_tx.send(promoted(&voter));
    })
    .unwrap();

  // The leader: dial, win the pre-vote and the vote, propose the promotion, and replicate it to commit.
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = leader_port_tx.send(socket.local_addr().unwrap().port());
      let voter_port = recv_port(voter_port_rx).await;
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, voter_port);
      let mut endpoint =
        Endpoint::client(socket, peer, &leader_identity, &voter_cert, NAME, FRAME_CAP).unwrap();
      endpoint.establish().await.unwrap();

      let mut leader = group(LEADER);

      // The full pre-vote → vote election over the transport: drain every follow-on message (the pre-votes,
      // then the real vote requests the granted pre-vote yields) until the exchange settles.
      let mut pending = leader.election_timeout();
      while let Some(request) = pending.pop() {
        if let Some(reply) = request_raft(&mut endpoint, &request).await.unwrap() {
          pending.extend(leader.fold_reply(reply));
        }
      }

      // Propose the cross-region promotion; at more than one voter it commits and applies only over the wire.
      let proposed = leader.propose(RootCommand::PromoteRegion {
        lost: LOST,
        mirror: MIRROR,
      });

      // Round one replicates the entry (the voter appends, the leader commits at the majority); round two's
      // heartbeat carries the advanced commit index, so the voter applies too.
      for _ in 0..2 {
        if let Some(append) = leader.replication_for(VOTER)
          && let Some(reply) = request_raft(&mut endpoint, &RaftMessage::AppendEntries(append))
            .await
            .unwrap()
        {
          leader.fold_reply(reply);
        }
      }

      let _ = result_tx.send(Outcome {
        leader_leads: leader.is_leader() && proposed,
        leader_promoted: promoted(&leader),
        voter_promoted: false,
      });
    })
    .unwrap();

  sim.run_until_idle();
  let mut outcome = result_rx.try_recv().unwrap();
  outcome.voter_promoted = voter_rx.try_recv().unwrap();
  outcome
}

/// A root group elects a leader through the pre-vote round over the transport and commits a region-loss
/// promotion at the majority, applying it to the root configuration at both voters — the region's volumes are
/// re-homed to the mirror by consensus over the wire, not the lone-voter degenerate.
#[test]
fn a_root_group_commits_a_region_promotion_across_the_transport() {
  let outcome = run_distributed_promotion();
  assert!(
    outcome.leader_leads,
    "the leader won the pre-vote and the vote over the transport and proposed the promotion"
  );
  assert!(
    outcome.leader_promoted,
    "the promotion committed at the majority and applied at the leader (the lost region re-homed to the mirror)"
  );
  assert!(
    outcome.voter_promoted,
    "and replicated then applied at the voter — the root group commits the cross-region promotion across the transport"
  );
}
