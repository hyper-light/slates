//! The Raft dialect, live over the simulated UDP fabric (§4.8): a candidate wins a vote from a peer over
//! the fleet transport and then replicates a committed entry to it, both through mutually-authenticated
//! sessions. "Production endpoints and Raft logic over simulated UDP" — one process, two nodes, real
//! sessions; real network/process deployment is a further gate. The safety properties are proven
//! deterministically in the conformance suite (`tests/raft.rs`); here we prove the messages ride the
//! transport — an election and a replication complete over real request/reply. Test by use (R5).

// Test harness: an unwrap or expect here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc::{Receiver, channel};

use rustix::net::{Ipv4Addr, SocketAddrV4};
use rustls::pki_types::PrivateKeyDer;
use slates_cluster::raft::RaftNode;
use slates_cluster::raft_wire::{RaftMessage, request_raft, serve_raft_once};
use slates_db::register::HostId;
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::SimRuntime;
use slates_rt::udp::UdpSocket;
use slates_transport::endpoint::Endpoint;
use slates_transport::handshake::Identity;

const NAME: &str = "slates-node";
const FRAME_CAP: usize = 16;
const CANDIDATE: HostId = HostId(1);
const VOTER: HostId = HostId(2);

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

/// What the live election-and-replication round produced at the candidate and the voter.
struct Outcome {
  candidate_leads: bool,
  candidate_commit: u64,
  voter_last_index: u64,
}

/// Drives one live round: the candidate (`1`) requests a vote from the voter (`2`) over an authenticated
/// session, becomes leader, then replicates one appended entry — all over the transport.
fn run_election_and_replication() -> Outcome {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];

  let candidate_identity = self_signed(NAME);
  let candidate_cert = candidate_identity.certificate();
  let voter_identity = self_signed(NAME);
  let voter_cert = voter_identity.certificate();

  let (candidate_port_tx, candidate_port_rx) = channel::<u16>();
  let (voter_port_tx, voter_port_rx) = channel::<u16>();
  let (result_tx, result_rx) = channel::<Outcome>();
  let (voter_index_tx, voter_index_rx) = channel::<u64>();

  // The voter: handshake, then serve the vote request and then the append.
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = voter_port_tx.send(socket.local_addr().unwrap().port());
      let candidate_port = recv_port(candidate_port_rx).await;
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, candidate_port);
      let mut endpoint = Endpoint::server(
        socket,
        peer,
        &voter_identity,
        std::slice::from_ref(&candidate_cert),
        FRAME_CAP,
      )
      .unwrap();
      endpoint.establish().await.unwrap();

      let mut node = RaftNode::new(VOTER, vec![CANDIDATE, VOTER]);
      serve_raft_once(&mut endpoint, &mut node).await.unwrap(); // the vote request
      serve_raft_once(&mut endpoint, &mut node).await.unwrap(); // the append
      let _ = voter_index_tx.send(node.last_log_index());
    })
    .unwrap();

  // The candidate: dial the voter, win its vote, and replicate an entry.
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = candidate_port_tx.send(socket.local_addr().unwrap().port());
      let voter_port = recv_port(voter_port_rx).await;
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, voter_port);
      let mut endpoint = Endpoint::client(
        socket,
        peer,
        &candidate_identity,
        &voter_cert,
        NAME,
        FRAME_CAP,
      )
      .unwrap();
      endpoint.establish().await.unwrap();

      let mut node = RaftNode::new(CANDIDATE, vec![CANDIDATE, VOTER]);

      // Win the election: send the vote request, feed the reply back.
      let request = node.start_election().into_iter().next().unwrap();
      if let Some(RaftMessage::VoteReply(reply)) =
        request_raft(&mut endpoint, &RaftMessage::RequestVote(request))
          .await
          .unwrap()
      {
        node.on_vote_reply(reply);
      }

      // Append and replicate an entry, feeding the reply back.
      node.append_command(b"first".to_vec());
      let append = node.replicate_to(VOTER).expect("an append to replicate");
      if let Some(RaftMessage::AppendReply(reply)) =
        request_raft(&mut endpoint, &RaftMessage::AppendEntries(append))
          .await
          .unwrap()
      {
        node.on_append_reply(reply);
      }

      let _ = result_tx.send(Outcome {
        candidate_leads: node.is_leader(),
        candidate_commit: node.commit_index(),
        voter_last_index: 0,
      });
    })
    .unwrap();

  sim.run_until_idle();
  let mut outcome = result_rx.try_recv().unwrap();
  outcome.voter_last_index = voter_index_rx.try_recv().unwrap();
  outcome
}

/// A live election is won over the transport, and the appended entry replicates to the voter and commits
/// — the Raft dialect works over real request/reply.
#[test]
fn a_candidate_wins_an_election_and_replicates_over_the_transport() {
  let outcome = run_election_and_replication();
  assert!(
    outcome.candidate_leads,
    "the candidate won the vote and leads"
  );
  assert_eq!(
    outcome.candidate_commit, 1,
    "the appended entry committed once the voter acknowledged it"
  );
  assert_eq!(
    outcome.voter_last_index, 1,
    "the voter holds the replicated entry"
  );
}
