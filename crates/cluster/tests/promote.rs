//! The cluster plane's phase-one promotion, live over the simulated UDP fabric (§4.8 "Promotion and
//! takeover"): after a dead owner is taken over, the new owner runs phase one over the fleet transport
//! — it sends a prepare to the surviving candidate holders, they raise their fence to the new epoch and
//! report the highest record they hold, and the new owner adopts the newest across the quorum, all over
//! mutually-authenticated sessions. "Production endpoints and holder logic over simulated UDP" — one
//! process, the surviving holders, an `f = 1` topology; real network/process deployment is a further
//! gate. The register invariants (Continuity, StaleNeverCommits) are proven sans-io in the db oracle
//! (`crates/db/src/register.rs`); here we prove the round rides the transport. Test by use (R5).

// Test harness: an unwrap or expect here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc::{Receiver, channel};

use rustix::net::{Ipv4Addr, SocketAddrV4};
use rustls::pki_types::PrivateKeyDer;
use slates_cluster::{CommitBudget, Promoted, promote_record, serve_promotion};
use slates_db::register::{
  Acceptor, Authority, HostEpoch, HostId, Prepare, Quorum, rendezvous_first,
};
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::SimRuntime;
use slates_rt::udp::UdpSocket;
use slates_transport::endpoint::Endpoint;
use slates_transport::handshake::Identity;

const NAME: &str = "slates-node";
const FRAME_CAP: usize = 16;
const OBJECT: u64 = 7;
// The dead owner D and its two candidate holders (D, H2, H3 at f = 1); H2 held the committed head, H3
// lagged. After D dies the survivors are {H2, H3}; the configuration named the rendezvous-first of them
// the new owner.
const DEAD: HostId = HostId(1);
const H2: HostId = HostId(2);
const H3: HostId = HostId(3);
// The head that committed under the old owner (epoch 1, generation 0) to the quorum {D, H2}.
const HEAD: &[u8] = b"head@v1";
// The takeover advanced the configuration: a bumped host epoch and a new generation, owner = successor.
const OLD_EPOCH: HostEpoch = HostEpoch(1);
const NEW_EPOCH: HostEpoch = HostEpoch(2);
const OLD_GENERATION: u64 = 0;
const NEW_GENERATION: u64 = 1;
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

/// The surviving holder `id`'s acceptor as it stands entering the takeover: it accepted the old owner's
/// head if it was in the commit quorum (H2 did, H3 lagged), and it has installed the new configuration
/// authority the group distributed (generation advanced, owner = the successor). The epoch fence is
/// still the old one — the promotion is what raises it.
fn survivor(id: HostId, successor: HostId) -> Acceptor {
  let old_authority = Authority {
    generation: OLD_GENERATION,
    owner: DEAD,
  };
  let accepted = if id == H2 {
    vec![(OBJECT, 0u64, OLD_EPOCH, HEAD.to_vec())]
  } else {
    Vec::new()
  };
  let mut acceptor = Acceptor::recovered(id, old_authority, OLD_EPOCH, accepted);
  acceptor
    .install_authority(Authority {
      generation: NEW_GENERATION,
      owner: successor,
    })
    .expect("the survivor installs the new authority");
  acceptor
}

/// What the live promotion produced at the new owner.
struct Outcome {
  promoted: bool,
  adopted_head: Option<Vec<u8>>,
}

/// Runs one live takeover: the successor (the rendezvous-first survivor of {H2, H3}) runs phase one over
/// the transport against the other survivor, and returns whether it promoted and what head it adopted.
fn run_promotion() -> Outcome {
  let survivors = [H2, H3];
  let successor = rendezvous_first(&survivors, OBJECT).expect("a survivor takes over");
  let other = if successor == H2 { H3 } else { H2 };

  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];

  let successor_identity = self_signed(NAME);
  let successor_cert = successor_identity.certificate();
  let holder_identity = self_signed(NAME);
  let holder_cert = holder_identity.certificate();

  let (successor_port_tx, successor_port_rx) = channel::<u16>();
  let (holder_port_tx, holder_port_rx) = channel::<u16>();
  let (result_tx, result_rx) = channel::<Outcome>();

  // The other survivor serves the prepare through its acceptor (raising its fence, reporting its head).
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = holder_port_tx.send(socket.local_addr().unwrap().port());
      let successor_port = recv_port(successor_port_rx).await;
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, successor_port);
      let mut endpoint = Endpoint::server(
        socket,
        peer,
        &holder_identity,
        std::slice::from_ref(&successor_cert),
        FRAME_CAP,
      )
      .unwrap();
      endpoint.establish().await.unwrap();
      let mut acceptor = survivor(other, successor);
      serve_promotion(&mut endpoint, &mut acceptor).await.unwrap();
    })
    .unwrap();

  // The successor dials the other survivor, then runs phase one over [its own hold + that holder].
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = successor_port_tx.send(socket.local_addr().unwrap().port());
      let holder_port = recv_port(holder_port_rx).await;
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, holder_port);
      let mut endpoint = Endpoint::client(
        socket,
        peer,
        &successor_identity,
        &holder_cert,
        NAME,
        FRAME_CAP,
      )
      .unwrap();
      endpoint.establish().await.unwrap();

      let mut owner_acceptor = survivor(successor, successor);
      let candidates = [DEAD, H2, H3];
      let prepare = Prepare {
        owner: successor,
        object: OBJECT,
        epoch: NEW_EPOCH,
        generation: NEW_GENERATION,
      };
      let Promoted { outcome, .. } = promote_record(
        successor,
        &mut owner_acceptor,
        &candidates,
        &prepare,
        Quorum { f: 1 },
        vec![(other, endpoint)],
        CommitBudget {
          deadline_ns: DEADLINE_NS,
          poll_interval_ns: POLL_NS,
        },
      )
      .await;
      let result = match outcome {
        Ok(promotion) => Outcome {
          promoted: promotion.promoted(Quorum { f: 1 }),
          adopted_head: promotion.adopted.map(|record| record.value),
        },
        Err(_) => Outcome {
          promoted: false,
          adopted_head: None,
        },
      };
      let _ = result_tx.send(result);
    })
    .unwrap();

  sim.run_until_idle();
  result_rx.try_recv().unwrap()
}

/// AC (§4.8 "Promotion and takeover"): the new owner running phase one over the transport promises a
/// quorum (itself plus the other survivor) and adopts the head that committed under the old epoch — the
/// promotion round works over real request/reply, and the committed head is recovered from whichever
/// survivor held it.
#[test]
fn a_new_owner_promotes_over_the_transport_and_adopts_the_committed_head() {
  let outcome = run_promotion();
  assert!(
    outcome.promoted,
    "the successor and the other survivor are an f=1 promotion quorum"
  );
  assert_eq!(
    outcome.adopted_head.as_deref(),
    Some(HEAD),
    "the committed head is adopted over the transport, recovered from the survivor that held it"
  );
}

/// AC (§4.8 laptop degenerate): the same `promote_record` at `f = 0` promotes on the new owner's own
/// hold alone — one candidate, a quorum of one — with no dispatch, adopting its own head. The observable
/// outcome (promoted, the head adopted) is the same as the successful `f = 1` case; the same code path
/// (R8). Run on the sim only to drive the async function; no sockets, no remote holders.
#[test]
fn f0_promotes_locally_with_no_dispatch() {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];
  let (tx, rx) = channel::<Outcome>();
  sim
    .spawn_on(id, async move {
      // A solo new owner that already holds the head, under its installed authority.
      let mut owner_acceptor = Acceptor::recovered(
        H2,
        Authority {
          generation: NEW_GENERATION,
          owner: H2,
        },
        OLD_EPOCH,
        vec![(OBJECT, 0u64, OLD_EPOCH, HEAD.to_vec())],
      );
      let prepare = Prepare {
        owner: H2,
        object: OBJECT,
        epoch: NEW_EPOCH,
        generation: NEW_GENERATION,
      };
      let Promoted { outcome, .. } = promote_record(
        H2,
        &mut owner_acceptor,
        &[H2],
        &prepare,
        Quorum { f: 0 },
        Vec::new(),
        CommitBudget {
          deadline_ns: DEADLINE_NS,
          poll_interval_ns: POLL_NS,
        },
      )
      .await;
      let result = match outcome {
        Ok(promotion) => Outcome {
          promoted: promotion.promoted(Quorum { f: 0 }),
          adopted_head: promotion.adopted.map(|record| record.value),
        },
        Err(_) => Outcome {
          promoted: false,
          adopted_head: None,
        },
      };
      let _ = tx.send(result);
    })
    .unwrap();
  sim.run_until_idle();
  let outcome = rx.try_recv().unwrap();
  assert!(outcome.promoted, "f=0 promotes on the local hold alone");
  assert_eq!(
    outcome.adopted_head.as_deref(),
    Some(HEAD),
    "the solo new owner adopts its own head"
  );
}
