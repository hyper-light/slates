//! The cluster plane's register commit, live over the simulated UDP fabric (§4.8): an owner ships a
//! record to its candidate holders over the fleet transport and commits at a quorum, holders running
//! the real db acceptance rules over mutually-authenticated sessions. "Production endpoints and holder
//! logic over simulated UDP" — one process, several holders, an `f = 1` protocol topology; real
//! network/process deployment is a further gate. Test by use (R5).

// Test harness: an unwrap here is a failed test.
#![allow(clippy::unwrap_used)]

use std::sync::mpsc::{Receiver, channel};

use rustix::net::{Ipv4Addr, SocketAddrV4};
use rustls::pki_types::PrivateKeyDer;
use slates_cluster::{
  ClusterError, CommitBudget, commit_record, commit_under_configuration, serve_record,
};
use slates_db::register::{
  Acceptor, Authority, Configuration, HostEpoch, HostId, ObjectId, Placement, Quorum, Record,
};
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::SimRuntime;
use slates_rt::udp::UdpSocket;
use slates_transport::endpoint::Endpoint;
use slates_transport::handshake::Identity;

const NAME: &str = "slates-node";
const FRAME_CAP: usize = 16;
const GENERATION: u64 = 0;
const OBJECT: ObjectId = ObjectId::new(OWNER, 7);
const OWNER: HostId = HostId(1);
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

fn authority() -> Authority {
  Authority {
    generation: GENERATION,
    owner: OWNER,
  }
}

fn record(value: &[u8]) -> Record {
  Record {
    owner: OWNER,
    object: OBJECT,
    sequence: 0,
    epoch: HostEpoch(1),
    generation: GENERATION,
    value: value.to_vec(),
  }
}

/// Runs an `f = 1` commit — owner `1`, candidate holders `2` and `3` — over the sim fabric. `serve[i]`
/// says whether holder `i + 2` serves the record (an available holder) or handshakes but never serves
/// it (an unavailable/slow holder the dispatch must not wait for). `holder_generation` is the configuration
/// generation the holders' acceptors run under — equal to the owner's for a normal commit, or higher to model
/// a holder on a **newer** configuration, which refuses the owner's record `ConfigurationStale` and rides its
/// version back (§4.8 the piggyback rule). Returns the commit's placement (or the `ClusterError` display
/// string) and the newest stale version any holder rode back.
fn run_commit_full(
  serve: [bool; 2],
  holder_generation: u64,
) -> (Result<Placement, String>, Option<u64>) {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];

  let owner_identity = self_signed(NAME);
  let owner_cert = owner_identity.certificate();
  let holder_identities = [self_signed(NAME), self_signed(NAME)];
  let holder_certs = [
    holder_identities[0].certificate(),
    holder_identities[1].certificate(),
  ];

  // Port exchange, one pair of channels per holder: the owner sends the port of the socket it dials
  // from (`owner_port`), and holder i sends the port it listens on (`holder_port`).
  let mut owner_port_tx = Vec::new();
  let mut owner_port_rx = Vec::new();
  let mut holder_port_tx = Vec::new();
  let mut holder_port_rx = Vec::new();
  for _ in 0..2 {
    let (otx, orx) = channel::<u16>();
    let (htx, hrx) = channel::<u16>();
    owner_port_tx.push(otx);
    owner_port_rx.push(orx);
    holder_port_tx.push(htx);
    holder_port_rx.push(hrx);
  }
  let (result_tx, result_rx) = channel();

  // The holder tasks (each owns its identity, its receive-owner-port and send-own-port ends).
  let holder_setup = holder_identities
    .into_iter()
    .zip(owner_port_rx)
    .zip(holder_port_tx)
    .zip(serve);
  for (index, (((holder_identity, orx), htx), should_serve)) in holder_setup.enumerate() {
    let owner_cert = owner_cert.clone();
    let holder_id = HostId(u64::try_from(index).unwrap_or(0) + 2);
    sim
      .spawn_on(id, async move {
        let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        let _ = htx.send(socket.local_addr().unwrap().port());
        let owner_port = recv_port(orx).await;
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
        if should_serve {
          let mut acceptor = Acceptor::new(
            holder_id,
            Authority {
              generation: holder_generation,
              owner: OWNER,
            },
          );
          serve_record(&mut endpoint, &mut acceptor).await.unwrap();
        }
        // An unavailable holder handshakes then leaves without serving; the owner must not block on it.
      })
      .unwrap();
  }

  // The owner task: dial each holder, establish, then commit.
  sim
    .spawn_on(id, async move {
      let dial = owner_port_tx
        .into_iter()
        .zip(holder_port_rx)
        .zip(holder_certs);
      let mut remotes = Vec::new();
      for (index, ((owner_port_tx, holder_port_rx), holder_cert)) in dial.enumerate() {
        let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        let _ = owner_port_tx.send(socket.local_addr().unwrap().port());
        let holder_port = recv_port(holder_port_rx).await;
        let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, holder_port);
        let mut endpoint =
          Endpoint::client(socket, peer, &owner_identity, &holder_cert, NAME, FRAME_CAP).unwrap();
        endpoint.establish().await.unwrap();
        remotes.push((HostId(u64::try_from(index).unwrap_or(0) + 2), endpoint));
      }

      let mut owner_acceptor = Acceptor::new(OWNER, authority());
      let candidates = [OWNER, HostId(2), HostId(3)];
      let committed = commit_record(
        OWNER,
        &mut owner_acceptor,
        &candidates,
        &record(b"head@v1"),
        Quorum { f: 1 },
        remotes,
        CommitBudget::hard(DEADLINE_NS, POLL_NS),
      )
      .await;
      let stale = committed.stale_version;
      let outcome = committed.outcome.map_err(|e: ClusterError| e.to_string());
      let _ = result_tx.send((outcome, stale));
    })
    .unwrap();

  sim.run_until_idle();
  result_rx.try_recv().unwrap()
}

/// The placement of a normal `f = 1` commit — holders on the owner's own configuration generation, so they
/// accept — discarding the (`None`) stale hint.
fn run_commit(serve: [bool; 2]) -> Result<Placement, String> {
  run_commit_full(serve, GENERATION).0
}

/// AC (acceptance history 1): the same `commit_record` at `f = 0` places on the owner's local hold
/// alone — one candidate, commit at one — with no dispatch (the laptop degenerate; the observable
/// outcome, placed, is the same as the successful `f = 1` case). Run on the sim only to drive the
/// async function; no sockets, no holders.
#[test]
fn f0_commits_locally_with_no_dispatch() {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];
  let (tx, rx) = channel();
  sim
    .spawn_on(id, async move {
      let mut owner_acceptor = Acceptor::new(OWNER, authority());
      let outcome = commit_record(
        OWNER,
        &mut owner_acceptor,
        &[OWNER],
        &record(b"head@v1"),
        Quorum { f: 0 },
        Vec::new(),
        CommitBudget::hard(DEADLINE_NS, POLL_NS),
      )
      .await
      .outcome
      .map_err(|e: ClusterError| e.to_string());
      let _ = tx.send(outcome);
    })
    .unwrap();
  sim.run_until_idle();
  let placement = rx.try_recv().unwrap().expect("f=0 places locally");
  assert!(
    placement.placed(Quorum { f: 0 }),
    "f=0 is placed at one: {placement:?}"
  );
  assert_eq!(placement.acked, vec![OWNER], "only the owner holds at f=0");
}

/// AC (§4.8): a commit driven **through the configuration interface** derives the quorum, candidates
/// and owner from a validated `Configuration` — here the solo (`f = 0`) configuration, which places on
/// the owner's local hold with no dispatch. The commit consumes one validated snapshot, not loose
/// parameters (SWIM/the configuration group will publish it; owed).
#[test]
fn a_commit_under_a_solo_configuration_places_locally() {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];
  let (tx, rx) = channel();
  sim
    .spawn_on(id, async move {
      let configuration = Configuration::solo(OWNER);
      let mut owner_acceptor = Acceptor::new(
        OWNER,
        Authority {
          generation: configuration.version,
          owner: configuration.owner,
        },
      );
      let committed = commit_under_configuration(
        &configuration,
        &record(b"head@v1"),
        &mut owner_acceptor,
        Vec::new(),
        CommitBudget::hard(DEADLINE_NS, POLL_NS),
      )
      .await;
      let placed = committed
        .outcome
        .map(|p| p.placed(configuration.quorum))
        .unwrap_or(false);
      let _ = tx.send(placed);
    })
    .unwrap();
  sim.run_until_idle();
  assert!(
    rx.try_recv().unwrap(),
    "a solo-configuration commit places on the local hold"
  );
}

/// AC (acceptance history 1): with `f = 1` and both remote holders serving, the owner's local hold plus
/// their acknowledgements commit the record — placed with a full quorum of distinct candidates.
#[test]
fn f1_commit_places_with_a_quorum() {
  let placement = run_commit([true, true]).expect("the commit should place");
  assert!(
    placement.placed(Quorum { f: 1 }),
    "f=1 places with owner + two holders: {placement:?}"
  );
}

/// AC (acceptance history 3): one unavailable holder does not prevent an available quorum. Holder 2
/// serves and holder 3 handshakes but never serves; the owner (local) plus holder 2 reach the f=1
/// quorum without waiting on holder 3.
#[test]
fn one_unavailable_holder_does_not_block_the_quorum() {
  let placement = run_commit([true, false]).expect("the available holder should carry the quorum");
  assert!(
    placement.placed(Quorum { f: 1 }),
    "owner + one available holder is the f=1 quorum: {placement:?}"
  );
  assert!(
    placement.acked.contains(&OWNER) && placement.acked.contains(&HostId(2)),
    "the owner and the available holder acknowledged: {placement:?}"
  );
}

/// AC (acceptance history 3): insufficient acknowledgements cannot publish `placed`. With neither
/// remote holder serving, only the owner's local hold acknowledges — one, below the f=1 quorum of two —
/// so the commit times out **uncertain**, never claiming placement.
#[test]
fn insufficient_acknowledgements_do_not_place() {
  let outcome = run_commit([false, false]);
  match outcome {
    Err(uncertain) => assert!(
      uncertain.contains("uncertain"),
      "a sub-quorum commit is reported uncertain, not placed: {uncertain}"
    ),
    Ok(placement) => panic!("a sub-quorum commit must not place: {placement:?}"),
  }
}

/// AC (§4.8 the piggyback rule): when the holders are on a **newer** configuration than the owner, each
/// refuses the owner's record `ConfigurationStale` and rides its current version back; `commit_record`
/// surfaces the newest such version as `stale_version`, the cue for the owner to refresh its configuration
/// and retry within its budget. The record itself does **not** place — every holder answered, but only the
/// owner's own local hold acknowledged (one, below the f=1 quorum of two), so it is a known `NotPlaced`, not
/// a timeout. Non-vacuous: the surfaced version is exactly the holders' newer generation, not a bare flag,
/// and a normal commit (holders on the owner's generation) surfaces `None`.
#[test]
fn a_holder_on_a_newer_configuration_rides_its_version_back_and_the_owner_does_not_place() {
  let (outcome, stale) = run_commit_full([true, true], GENERATION + 1);
  assert_eq!(
    stale,
    Some(GENERATION + 1),
    "both holders on the newer generation refused ConfigurationStale, riding their version back"
  );
  match outcome {
    Err(not_placed) => assert!(
      not_placed.contains("not placed"),
      "a stale-refused commit is a known non-placement, not a timeout: {not_placed}"
    ),
    Ok(placement) => panic!("a stale-refused commit must not place: {placement:?}"),
  }

  // A normal commit — holders on the owner's own generation — accepts and surfaces no stale hint.
  let (placed, none) = run_commit_full([true, true], GENERATION);
  assert!(
    placed.is_ok(),
    "a same-generation commit places: {placed:?}"
  );
  assert_eq!(none, None, "no holder was ahead, so nothing rides back");
}

/// AC (acceptance history 4): a retry of the same record **reuses the connection** — the second commit
/// runs on the endpoint handed back by the first, so the connection's packet numbers advance across the
/// retry (never restart), and the holder acknowledges the same record identity idempotently (§4.8 "a
/// retry preserves record identity"). Both commits place. One owner, one serving holder, one connection.
#[test]
fn a_retry_reuses_the_connection_with_advancing_packet_numbers() {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];

  let owner_identity = self_signed(NAME);
  let owner_cert = owner_identity.certificate();
  let holder_identity = self_signed(NAME);
  let holder_cert = holder_identity.certificate();

  let (owner_port_tx, owner_port_rx) = channel::<u16>();
  let (holder_port_tx, holder_port_rx) = channel::<u16>();
  let (result_tx, result_rx) = channel();

  // The holder serves the same record twice, on one connection and one persistent acceptor.
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
      let mut acceptor = Acceptor::new(HostId(2), authority());
      serve_record(&mut endpoint, &mut acceptor).await.unwrap();
      serve_record(&mut endpoint, &mut acceptor).await.unwrap();
    })
    .unwrap();

  // The owner commits the same record twice, reusing the endpoint the first commit hands back.
  sim
    .spawn_on(id, async move {
      let outcome = async {
        let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        let _ = owner_port_tx.send(socket.local_addr().unwrap().port());
        let holder_port = recv_port(holder_port_rx).await;
        let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, holder_port);
        let mut endpoint =
          Endpoint::client(socket, peer, &owner_identity, &holder_cert, NAME, FRAME_CAP)
            .map_err(|e| format!("{e:?}"))?;
        endpoint.establish().await.map_err(|e| format!("{e:?}"))?;

        let mut owner_acceptor = Acceptor::new(OWNER, authority());
        let candidates = [OWNER, HostId(2), HostId(3)];
        let budget = CommitBudget::hard(DEADLINE_NS, POLL_NS);
        let rec = record(b"head@v1");

        let first = commit_record(
          OWNER,
          &mut owner_acceptor,
          &candidates,
          &rec,
          Quorum { f: 1 },
          vec![(HostId(2), endpoint)],
          budget,
        )
        .await;
        let placed_1 = first
          .outcome
          .map_err(|e| e.to_string())?
          .placed(Quorum { f: 1 });
        let pn_after_1 = first
          .reusable
          .first()
          .map(|(_, ep)| ep.tx_packet_number())
          .ok_or("the holder connection was not returned for reuse")?;

        // Retry the same record, reusing the connection.
        let second = commit_record(
          OWNER,
          &mut owner_acceptor,
          &candidates,
          &rec,
          Quorum { f: 1 },
          first.reusable,
          budget,
        )
        .await;
        let placed_2 = second
          .outcome
          .map_err(|e| e.to_string())?
          .placed(Quorum { f: 1 });
        let pn_after_2 = second
          .reusable
          .first()
          .map(|(_, ep)| ep.tx_packet_number())
          .ok_or("the reused connection was not returned again")?;

        Ok::<_, String>((placed_1, placed_2, pn_after_1, pn_after_2))
      }
      .await;
      let _ = result_tx.send(outcome);
    })
    .unwrap();

  sim.run_until_idle();
  let (placed_1, placed_2, pn_1, pn_2) =
    result_rx.try_recv().unwrap().expect("the retry completed");
  assert!(placed_1, "the first commit placed");
  assert!(
    placed_2,
    "the retry placed (the holder acknowledged the same record idempotently)"
  );
  assert!(
    pn_2 > pn_1,
    "the reused connection's packet numbers advanced across the retry ({pn_1} -> {pn_2}), never reset"
  );
}
