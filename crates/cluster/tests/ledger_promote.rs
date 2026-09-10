//! The cluster plane's **ledger** phase-one promotion, live over the simulated UDP fabric (§4.8
//! "Promotion and takeover"; the ledger's committed-prefix adoption, the generalization of the
//! single-value `promote.rs`): after a dead owner is taken over, the new owner runs phase one for a
//! *multi-entry* register over the fleet transport — it sends a prepare to the surviving candidate
//! holders, they raise their fence to the new epoch and report their **whole log**, and the new owner
//! adopts, per position, the record under the highest epoch across the quorum, all over
//! mutually-authenticated sessions. This is where a committed record that only one survivor still holds
//! (the dead owner held the other copy) is recovered: the phase-one quorum intersects every prior commit
//! quorum, so the adopted log's committed prefix is every record that had committed (Continuity). "Real
//! endpoints and holder logic over simulated UDP" — one process, an `f = 1` topology; real
//! network/process deployment is a further gate. The ledger invariants (Agreement, TotalOrder,
//! Continuity, StaleNeverCommits) are proven sans-io in the db oracle (`crates/db/src/ledger.rs`); here
//! we prove the multi-entry round rides the transport. Test by use (R5).

// Test harness: an unwrap or expect here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc::{Receiver, channel};

use rustix::net::{Ipv4Addr, SocketAddrV4};
use rustls::pki_types::PrivateKeyDer;
use slates_cluster::{CommitBudget, LedgerPromoted, promote_ledger_record, serve_ledger_promotion};
use slates_db::ledger::{LedgerAcceptor, Record};
use slates_db::register::{
  Authority, HostEpoch, HostId, ObjectId, Prepare, Quorum, rendezvous_first,
};
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::SimRuntime;
use slates_rt::udp::UdpSocket;
use slates_transport::endpoint::Endpoint;
use slates_transport::handshake::Identity;

const NAME: &str = "slates-node";
const FRAME_CAP: usize = 16;
const OBJECT: ObjectId = ObjectId::new(DEAD, 7);
// The dead owner D and its two candidate holders (D, H2, H3 at f = 1). Two records committed under the
// old owner: r0 to {D, H2, H3} and r1 to {D, H2}. So H2 holds the whole log [r0, r1]; H3 lagged and holds
// only [r0]. After D dies the survivors are {H2, H3}; the configuration named the rendezvous-first of
// them the new owner. r1's only surviving copy is on H2 — recovering it is the Continuity property.
const DEAD: HostId = HostId(1);
const H2: HostId = HostId(2);
const H3: HostId = HostId(3);
/// The two committed records' payload identities (blake3 digests; here distinct fixed patterns).
const R0: [u8; 32] = [0x11; 32];
const R1: [u8; 32] = [0x22; 32];
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

/// A record accepted under the old epoch (the state a holder recovered from anchor RAM entering takeover).
fn old(identity: [u8; 32]) -> Record {
  Record {
    epoch: OLD_EPOCH,
    identity,
  }
}

/// The surviving holder `id`'s ledger acceptor as it stands entering the takeover: it holds the prefix of
/// the old owner's log it accepted (H2 the whole `[r0, r1]`, H3 only `[r0]`), and it has installed the new
/// configuration authority the group distributed (generation advanced, owner = the successor). The epoch
/// fence is still the old one — the promotion is what raises it.
fn survivor(id: HostId, successor: HostId) -> LedgerAcceptor {
  let old_authority = Authority {
    generation: OLD_GENERATION,
    owner: DEAD,
  };
  let log = if id == H2 {
    vec![old(R0), old(R1)]
  } else {
    vec![old(R0)]
  };
  let mut acceptor = LedgerAcceptor::recovered(id, OBJECT, old_authority, OLD_EPOCH, log);
  acceptor
    .install_authority(Authority {
      generation: NEW_GENERATION,
      owner: successor,
    })
    .expect("the survivor installs the new authority");
  acceptor
}

/// What the live promotion produced at the new owner: whether it promoted, and the log it adopted.
struct Outcome {
  promoted: bool,
  adopted: Vec<[u8; 32]>,
}

/// Runs one live takeover: the successor (the rendezvous-first survivor of {H2, H3}) runs ledger phase one
/// over the transport against the other survivor, and returns whether it promoted and the log it adopted.
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

  // The other survivor serves the prepare through its ledger acceptor (raising its fence, reporting its
  // whole log).
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
      serve_ledger_promotion(&mut endpoint, &mut acceptor)
        .await
        .unwrap();
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
      let LedgerPromoted { outcome, .. } = promote_ledger_record(
        successor,
        &mut owner_acceptor,
        &candidates,
        &prepare,
        Quorum { f: 1 },
        vec![(other, endpoint)],
        CommitBudget::hard(DEADLINE_NS, POLL_NS),
      )
      .await;
      let result = match outcome {
        Ok(promotion) => Outcome {
          promoted: true,
          adopted: promotion.adopted,
        },
        Err(_) => Outcome {
          promoted: false,
          adopted: Vec::new(),
        },
      };
      let _ = result_tx.send(result);
    })
    .unwrap();

  sim.run_until_idle();
  result_rx.try_recv().unwrap()
}

/// AC (§4.8 "Promotion and takeover", the ledger committed-prefix adoption): the new owner running ledger
/// phase one over the transport promises a quorum (itself plus the other survivor) and adopts the whole
/// committed prefix `[r0, r1]`. r1 committed under the old owner to {D, H2} and its only surviving copy is
/// on H2; that the adopted log includes it proves Continuity — the phase-one quorum intersects the commit
/// quorum, so no committed record is lost when the owner that held the other copy dies.
#[test]
fn a_ledger_takeover_over_the_transport_adopts_the_whole_committed_prefix() {
  let outcome = run_promotion();
  assert!(
    outcome.promoted,
    "the successor and the other survivor are an f=1 promotion quorum"
  );
  assert_eq!(
    outcome.adopted,
    vec![R0, R1],
    "the committed prefix [r0, r1] is adopted over the transport — r1, held now only by H2, is recovered \
     (Continuity)"
  );
}

/// AC (§4.8 laptop degenerate): the same `promote_ledger_record` at `f = 0` promotes on the new owner's
/// own hold alone — one candidate, a quorum of one — with no dispatch, adopting its own log. The
/// observable outcome (promoted, the log adopted) is the same as the `f = 1` case; the same code path
/// (R8). Run on the sim only to drive the async function; no sockets, no remote holders.
#[test]
fn f0_promotes_locally_with_no_dispatch() {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];
  let (tx, rx) = channel::<Outcome>();
  sim
    .spawn_on(id, async move {
      // A solo new owner that already holds the whole log, under its installed authority.
      let mut owner_acceptor = LedgerAcceptor::recovered(
        H2,
        OBJECT,
        Authority {
          generation: NEW_GENERATION,
          owner: H2,
        },
        OLD_EPOCH,
        vec![old(R0), old(R1)],
      );
      let prepare = Prepare {
        owner: H2,
        object: OBJECT,
        epoch: NEW_EPOCH,
        generation: NEW_GENERATION,
      };
      let LedgerPromoted { outcome, .. } = promote_ledger_record(
        H2,
        &mut owner_acceptor,
        &[H2],
        &prepare,
        Quorum { f: 0 },
        Vec::new(),
        CommitBudget::hard(DEADLINE_NS, POLL_NS),
      )
      .await;
      let result = match outcome {
        Ok(promotion) => Outcome {
          promoted: true,
          adopted: promotion.adopted,
        },
        Err(_) => Outcome {
          promoted: false,
          adopted: Vec::new(),
        },
      };
      let _ = tx.send(result);
    })
    .unwrap();
  sim.run_until_idle();
  let outcome = rx.try_recv().unwrap();
  assert!(outcome.promoted, "f=0 promotes on the local hold alone");
  assert_eq!(
    outcome.adopted,
    vec![R0, R1],
    "the solo owner adopts its own whole log — the same observable outcome as f=1 (R8)"
  );
}
