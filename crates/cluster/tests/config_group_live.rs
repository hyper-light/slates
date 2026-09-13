//! The **regional configuration council** live over the simulated UDP fabric (§4.8, D-14): a multi-voter
//! [`RegionalCouncil`] elects a leader through the full **pre-vote** round (Raft §9.6) over the transport,
//! and a membership change proposed on the leader replicates to the voter, commits at a majority, and
//! applies to the regional configuration at *both* — the config-group Raft driven over real
//! mutually-authenticated sessions, not the lone-voter degenerate. The Raft dialect's safety is proven in
//! `tests/raft.rs`; here we prove the council's own changes ride the transport and commit. Test by use (R5);
//! real network/process deployment is a further gate (the daemon fleet-loop config plane).

// Test harness: an unwrap or expect here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, channel};

use rustix::net::{Ipv4Addr, SocketAddrV4};
use rustls::pki_types::PrivateKeyDer;
use slates_cluster::config_group::{Reconfiguration, RegionalCouncil};
use slates_cluster::raft_wire::{RaftMessage, request_raft};
use slates_db::register::{HostId, Quorum};
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::SimRuntime;
use slates_rt::udp::UdpSocket;
use slates_transport::endpoint::Endpoint;
use slates_transport::handshake::Identity;

const NAME: &str = "slates-node";
const FRAME_CAP: usize = 16;
const LEADER: HostId = HostId(1);
const VOTER: HostId = HostId(2);
const ADMITTED: HostId = HostId(3);
/// The candidate floor at f=1 (2f+1), the scatter the two-host council's neighbourhoods are bounded to.
const SCATTER: u64 = 3;

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

/// A two-voter council on `node`, members and voters both `[LEADER, VOTER]`, at f=1.
fn council(node: HostId) -> RegionalCouncil {
  RegionalCouncil::new(
    node,
    vec![LEADER, VOTER],
    vec![LEADER, VOTER],
    Quorum { f: 1 },
    BTreeMap::new(),
    SCATTER,
    false,
  )
}

/// What the live distributed membership change produced at the leader and the voter.
struct Outcome {
  leader_leads: bool,
  leader_admitted: bool,
  voter_admitted: bool,
}

/// Drives one live round: the leader (`1`) wins the voter's (`2`) pre-vote then vote over an authenticated
/// session, proposes admitting host `3` to the region, and replicates it — the change commits at the
/// majority and applies to the regional configuration at both nodes, all over the transport.
fn run_distributed_membership_change() -> Outcome {
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

  // The voter: handshake, then answer the pre-vote, the vote request, and the two appends into its council.
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

      let mut voter = council(VOTER);
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
      let _ = voter_tx.send(voter.configuration().members.contains(&ADMITTED));
    })
    .unwrap();

  // The leader: dial, win the pre-vote and the vote, propose the change, and replicate it to commit.
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = leader_port_tx.send(socket.local_addr().unwrap().port());
      let voter_port = recv_port(voter_port_rx).await;
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, voter_port);
      let mut endpoint =
        Endpoint::client(socket, peer, &leader_identity, &voter_cert, NAME, FRAME_CAP).unwrap();
      endpoint.establish().await.unwrap();

      let mut leader = council(LEADER);

      // The full pre-vote → vote election over the transport: drain every follow-on message (the pre-votes,
      // then the real vote requests the granted pre-vote yields) until the exchange settles.
      let mut pending = leader.election_timeout();
      while let Some(request) = pending.pop() {
        if let Some(reply) = request_raft(&mut endpoint, &request).await.unwrap() {
          pending.extend(leader.fold_reply(reply));
        }
      }

      // Propose a membership change; at more than one voter it commits and applies only over the wire.
      let proposed = leader.propose(Reconfiguration::Admit(ADMITTED));

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
        leader_admitted: leader.configuration().members.contains(&ADMITTED),
        voter_admitted: false,
      });
    })
    .unwrap();

  sim.run_until_idle();
  let mut outcome = result_rx.try_recv().unwrap();
  outcome.voter_admitted = voter_rx.try_recv().unwrap();
  outcome
}

/// A voter of the three-voter council that has **died**: it has no endpoint, answers nothing, and the two
/// live voters must retire it and carry the council alone.
const DEAD: HostId = HostId(4);
/// Shape: the requests the live voter serves in the voter-removal round — the leader's sequence over the
/// transport is one pre-vote and one vote (the election), then four two-round replications (the takeover,
/// the joint configuration entry, `C_new`, and the further admission — each an entry round and a commit
/// heartbeat).
const REMOVAL_SERVES: usize = 10;

/// A three-voter council on `node`, members and voters `[LEADER, VOTER, DEAD]` at f=1 (the candidate floor:
/// every member votes).
fn council_of_three(node: HostId) -> RegionalCouncil {
  RegionalCouncil::new(
    node,
    vec![LEADER, VOTER, DEAD],
    vec![LEADER, VOTER, DEAD],
    Quorum { f: 1 },
    BTreeMap::new(),
    SCATTER,
    false,
  )
}

/// What the live voter removal produced at the leader and the surviving voter.
struct RemovalOutcome {
  leader_leads: bool,
  dead_retired_at_leader: bool,
  dead_is_voter_at_leader: bool,
  dead_is_voter_at_voter: bool,
  leader_voters: Vec<HostId>,
  admitted_at_leader: bool,
  admitted_at_voter: bool,
}

/// Drives the live voter removal: the leader (`1`) wins the live voter's (`2`) pre-vote then vote — a majority
/// of three with the dead voter (`4`) silent — takes the dead voter over, moves the voter set to the two
/// survivors through the joint change and `C_new` (each committed by the live voter's acknowledgement alone,
/// carried as configuration entries on the wire), and then commits a further change under the new majority
/// of two. Every step rides the transport; the live voter's council applies the same log.
fn run_voter_removal_over_the_transport() -> RemovalOutcome {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];

  let leader_identity = self_signed(NAME);
  let leader_cert = leader_identity.certificate();
  let voter_identity = self_signed(NAME);
  let voter_cert = voter_identity.certificate();

  let (leader_port_tx, leader_port_rx) = channel::<u16>();
  let (voter_port_tx, voter_port_rx) = channel::<u16>();
  let (result_tx, result_rx) = channel::<RemovalOutcome>();
  let (voter_tx, voter_rx) = channel::<(bool, bool)>();

  // The live voter: handshake, then serve the leader's whole sequence into its council.
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

      let mut voter = council_of_three(VOTER);
      for _ in 0..REMOVAL_SERVES {
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
      let _ = voter_tx.send((
        voter.is_voter(DEAD),
        voter.configuration().members.contains(&ADMITTED),
      ));
    })
    .unwrap();

  // The leader: dial the live voter, win the election, retire the dead voter, move the voter set, commit on.
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = leader_port_tx.send(socket.local_addr().unwrap().port());
      let voter_port = recv_port(voter_port_rx).await;
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, voter_port);
      let mut endpoint =
        Endpoint::client(socket, peer, &leader_identity, &voter_cert, NAME, FRAME_CAP).unwrap();
      endpoint.establish().await.unwrap();

      let mut leader = council_of_three(LEADER);

      // The election over the transport: the requests come one per other voter and are identical, so one
      // is sent to the live voter and the rest dropped (the dead voter would never answer); a granted pre-vote
      // from the live voter is a majority of three with the leader's own, as is the real vote.
      let mut pending = leader.election_timeout();
      while let Some(request) = pending.pop() {
        pending.clear();
        if let Some(reply) = request_raft(&mut endpoint, &request).await.unwrap() {
          pending.extend(leader.fold_reply(reply));
        }
      }
      let leader_leads = leader.is_leader();

      // Two replication rounds to the live voter: the entry, then the commit heartbeat.
      let replicate = async |leader: &mut RegionalCouncil, endpoint: &mut Endpoint| {
        for _ in 0..2 {
          if let Some(append) = leader.replication_for(VOTER)
            && let Some(reply) = request_raft(endpoint, &RaftMessage::AppendEntries(append))
              .await
              .unwrap()
          {
            leader.fold_reply(reply);
          }
        }
      };

      // The dead voter is taken over (committed by the live voter's acknowledgement: two of three).
      leader.reconcile_alive(&[LEADER, VOTER]);
      replicate(&mut leader, &mut endpoint).await;
      let dead_retired_at_leader = !leader.configuration().members.contains(&DEAD);
      // The voter set follows: the joint change, then C_new, each committed by the live voter alone.
      leader.reconcile_voters();
      replicate(&mut leader, &mut endpoint).await;
      leader.reconcile_voters();
      replicate(&mut leader, &mut endpoint).await;
      // A further change commits under the new majority of two.
      leader.propose(Reconfiguration::Admit(ADMITTED));
      replicate(&mut leader, &mut endpoint).await;

      let _ = result_tx.send(RemovalOutcome {
        leader_leads,
        dead_retired_at_leader,
        dead_is_voter_at_leader: leader.is_voter(DEAD),
        dead_is_voter_at_voter: true,
        leader_voters: leader.voters(),
        admitted_at_leader: leader.configuration().members.contains(&ADMITTED),
        admitted_at_voter: false,
      });
    })
    .unwrap();

  sim.run_until_idle();
  let mut outcome = result_rx.try_recv().unwrap();
  let (dead_is_voter_at_voter, admitted_at_voter) = voter_rx.try_recv().unwrap();
  outcome.dead_is_voter_at_voter = dead_is_voter_at_voter;
  outcome.admitted_at_voter = admitted_at_voter;
  outcome
}

/// AC (§4.8, D-14; Raft §6): over the transport, a council whose voter **died** retires it from the Raft
/// voter set — the configuration entries ride the wire, the joint change and `C_new` each commit on the one
/// live voter's acknowledgement, and afterwards the dead voter counts toward no majority: a further change
/// commits with the live voter alone and applies at both. Before this the voter set was fixed at boot: the
/// dead voter stayed a voter for good and the council kept needing two of {leader, live voter, dead}.
#[test]
fn a_dead_voter_is_removed_from_the_council_across_the_transport() {
  let outcome = run_voter_removal_over_the_transport();
  assert!(
    outcome.leader_leads,
    "the leader won the pre-vote and the vote with the live voter — a majority of three"
  );
  assert!(
    outcome.dead_retired_at_leader,
    "the dead voter's takeover committed at the majority"
  );
  assert!(
    !outcome.dead_is_voter_at_leader,
    "the dead voter left the leader's voter set: {:?}",
    outcome.leader_voters
  );
  assert!(
    !outcome.dead_is_voter_at_voter,
    "and the live voter's — the configuration entries replicated over the wire"
  );
  assert_eq!(outcome.leader_voters, vec![LEADER, VOTER]);
  assert!(
    outcome.admitted_at_leader && outcome.admitted_at_voter,
    "a further change commits under the new majority of two and applies at both"
  );
}

/// A regional council elects a leader through the pre-vote round over the transport and commits a membership
/// change at the majority, applying it to the regional configuration at both voters — the config group is
/// genuinely distributed, not the lone-voter degenerate.
#[test]
fn a_regional_council_commits_a_membership_change_across_the_transport() {
  let outcome = run_distributed_membership_change();
  assert!(
    outcome.leader_leads,
    "the leader won the pre-vote and the vote over the transport and proposed the change"
  );
  assert!(
    outcome.leader_admitted,
    "the change committed at the majority and applied at the leader"
  );
  assert!(
    outcome.voter_admitted,
    "and replicated then applied at the voter — the council commits across the transport"
  );
}
