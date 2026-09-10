//! The owner runtime committing one of its heads live over the simulated UDP fabric (§4.8; boot step
//! 6): a [`FleetNode`] — the composed SWIM view, configuration authority, and owner acceptor — ships a
//! volume head to its candidate holders through the register path and commits at a quorum, the holders
//! running the real db acceptance rules over mutually-authenticated sessions. This proves the authority
//! core and the async dispatch are joined: the record is committed against the *same* configuration and
//! acceptor the runtime holds, so authority and dispatch cannot diverge. "Production endpoints and
//! holder logic over simulated UDP" — one process, an `f = 1` topology; real network/process deployment
//! is a further gate. Test by use (R5).

// Test harness: an unwrap here is a failed test.
#![allow(clippy::unwrap_used)]

use std::sync::mpsc::{Receiver, channel};

use rustix::net::{Ipv4Addr, SocketAddrV4};
use rustls::pki_types::PrivateKeyDer;
use slates_cluster::fleet::FleetNode;
use slates_cluster::{CommitBudget, serve_record};
use slates_db::register::{Acceptor, Authority, HostId, ObjectId, Quorum, Record};
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::SimRuntime;
use slates_rt::udp::UdpSocket;
use slates_transport::endpoint::Endpoint;
use slates_transport::handshake::Identity;

const NAME: &str = "slates-node";
const FRAME_CAP: usize = 16;
const OBJECT: ObjectId = ObjectId::new(OWNER, 7);
const OWNER: HostId = HostId(1);
const A: HostId = HostId(2);
const B: HostId = HostId(3);
// Test values; a production caller derives the deadline from a measured RTT budget (owed).
const DEADLINE_NS: u64 = 20_000_000;
const POLL_NS: u64 = 1_000;

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

/// An `f = 1` owner runtime — [`FleetNode`] with peers `A` and `B` — commits a head through the
/// register path over the sim fabric. Holder `A` serves the record; the owner's local hold plus `A`'s
/// acknowledgement reach the `f = 1` quorum. Returns whether the commit placed.
fn run_owner_commit() -> bool {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];

  let owner_identity = self_signed(NAME);
  let owner_cert = owner_identity.certificate();
  let holder_identity = self_signed(NAME);
  let holder_cert = holder_identity.certificate();

  let (owner_port_tx, owner_port_rx) = channel::<u16>();
  let (holder_port_tx, holder_port_rx) = channel::<u16>();
  let (result_tx, result_rx) = channel::<bool>();

  // The owner runtime's configuration version after admitting A and B is what the holder must serve
  // under; the owner and the test agree on it by construction (two admits from the solo neighbourhood).
  let generation = 2;

  // Holder A: handshake, then serve the record under the owner's authority.
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = holder_port_tx.send(socket.local_addr().unwrap().port());
      let owner_port = recv_port(owner_port_rx).await;
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, owner_port);
      let mut endpoint = Endpoint::server(
        socket,
        peer,
        &holder_identity,
        std::slice::from_ref(&owner_cert),
        FRAME_CAP,
      )
      .unwrap();
      endpoint.establish().await.unwrap();
      let mut acceptor = Acceptor::new(
        A,
        Authority {
          generation,
          owner: OWNER,
        },
      );
      serve_record(&mut endpoint, &mut acceptor).await.unwrap();
    })
    .unwrap();

  // The owner: build the runtime, dial holder A, and commit a head through commit_head.
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = owner_port_tx.send(socket.local_addr().unwrap().port());
      let holder_port = recv_port(holder_port_rx).await;
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, holder_port);
      let mut endpoint =
        Endpoint::client(socket, peer, &owner_identity, &holder_cert, NAME, FRAME_CAP).unwrap();
      endpoint.establish().await.unwrap();

      // The owner runtime: f = 1, peers A and B. Its configuration version is 2 (A and B admitted),
      // matching the holder's authority; its neighbourhood is {OWNER, A, B}, three candidates.
      let mut node = FleetNode::new(OWNER, Quorum { f: 1 }, &[A, B]);
      let record = Record {
        owner: node.host(),
        object: OBJECT,
        sequence: 0,
        epoch: node.configuration().host_epoch,
        generation: node.configuration().version,
        value: b"head@v1".to_vec(),
      };
      let placed = node
        .commit_head(
          &record,
          vec![(A, endpoint)],
          CommitBudget::hard(DEADLINE_NS, POLL_NS),
        )
        .await
        .outcome
        .map(|placement| placement.placed(Quorum { f: 1 }))
        .unwrap_or(false);
      let _ = result_tx.send(placed);
    })
    .unwrap();

  sim.run_until_idle();
  result_rx.try_recv().unwrap()
}

/// AC (§4.8, boot step 6): the owner runtime commits a head over the transport — the owner's local hold
/// plus one serving candidate reach the `f = 1` quorum — so the composed authority core drives a real
/// register commit against its own configuration and acceptor.
#[test]
fn the_owner_runtime_commits_a_head_over_the_transport() {
  assert!(
    run_owner_commit(),
    "the owner runtime's head commits at the f=1 quorum over the transport"
  );
}

/// AC (§4.8 laptop degenerate, R8): the same [`FleetNode::commit_head`] at `f = 0` places on the
/// owner's local hold alone, no dispatch — the identical code path, observably placed. Run on the sim
/// only to drive the async function; no sockets, no holders.
#[test]
fn the_solo_runtime_commits_a_head_locally() {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];
  let (tx, rx) = channel::<bool>();
  sim
    .spawn_on(id, async move {
      let mut node = FleetNode::solo(OWNER);
      let record = Record {
        owner: node.host(),
        object: OBJECT,
        sequence: 0,
        epoch: node.configuration().host_epoch,
        generation: node.configuration().version,
        value: b"head@v1".to_vec(),
      };
      let placed = node
        .commit_head(
          &record,
          Vec::new(),
          CommitBudget::hard(DEADLINE_NS, POLL_NS),
        )
        .await
        .outcome
        .map(|placement| placement.placed(Quorum { f: 0 }))
        .unwrap_or(false);
      let _ = tx.send(placed);
    })
    .unwrap();
  sim.run_until_idle();
  assert!(
    rx.try_recv().unwrap(),
    "the solo runtime's head commits on the local hold at f=0"
  );
}
