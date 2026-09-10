//! The commit dispatch's **progress-based deadline extension** (§4.8 "late work"), live over the
//! simulated UDP fabric: a commit whose quorum is still filling as it passes its deadline is given a
//! bounded extension and commits, while a stalled one (no new acknowledgement within the stall window)
//! is left to time out and reported uncertain. This is the [`CommitBudget::with_extension`] policy the
//! [`commit_record`] collection loop consumes, exercised by use — real endpoints and holder logic over
//! simulated UDP, one process simulating several holders (still an `f = 2` protocol topology; real
//! multi-process deployment is a further gate). The extender's decision logic is unit-tested in
//! `progress.rs`; this proves the dispatch *consumes* it — a slow-but-progressing commit survives, a
//! stuck one does not. Test by use (R5).

// Test harness: an unwrap here is a failed test.
#![allow(clippy::unwrap_used)]

use std::sync::mpsc::{Receiver, channel};

use rustix::net::{Ipv4Addr, SocketAddrV4};
use rustls::pki_types::PrivateKeyDer;
use slates_cluster::{ClusterError, CommitBudget, commit_record, serve_record};
use slates_db::register::{
  Acceptor, Authority, HostEpoch, HostId, ObjectId, Placement, Quorum, Record,
};
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::SimRuntime;
use slates_rt::udp::UdpSocket;
use slates_transport::endpoint::Endpoint;
use slates_transport::handshake::Identity;

const NAME: &str = "slates-node";
const FRAME_CAP: usize = 16;
const GENERATION: u64 = 0;
const OWNER: HostId = HostId(1);
const OBJECT: ObjectId = ObjectId::new(OWNER, 7);
/// The quorum: `f = 2`, so a commit needs `f + 1 = 3` distinct acknowledgements — the owner and both
/// holders. That the last holder acknowledges only past the base deadline is what makes the extension
/// observable: a hard deadline would report the commit uncertain before that third acknowledgement.
const QUORUM: Quorum = Quorum { f: 2 };

// Test timing values (nanoseconds); a production caller derives these from a measured RTT budget.
/// The base deadline before extension — deliberately shorter than the second holder's acknowledgement,
/// so a hard budget would time the commit out before the quorum fills.
const BASE_DEADLINE_NS: u64 = 15_000_000;
/// The collection loop's poll interval.
const POLL_NS: u64 = 1_000_000;
/// When the first holder acknowledges (well within the base deadline) — the quorum starts filling.
const FIRST_ACK_NS: u64 = 10_000_000;
/// When the second holder acknowledges (past the base deadline) — the acknowledgement only an extension
/// lets the commit collect.
const SECOND_ACK_NS: u64 = 25_000_000;
/// How much each granted extension adds to the deadline.
const EXTENSION_NS: u64 = 15_000_000;
/// How many extensions a commit may be granted.
const MAX_EXTENSIONS: u32 = 3;
/// The lookahead ratio 3/4 (hyperscale's 0.75, AD-26): an extension is considered in the last quarter of
/// the current deadline.
const LOOKAHEAD_NUM: u64 = 3;
const LOOKAHEAD_DEN: u64 = 4;
/// The stall window — wider than the gap between the staggered acknowledgements, so a steadily filling
/// quorum reads as progressing while a quorum with no new acknowledgement for this long reads as stalled.
const STALL_WINDOW_NS: u64 = 20_000_000;
/// How long a stalled holder keeps its connection open before exiting — past the commit's stall-driven
/// expiry, so the owner's request genuinely waits (and times out) rather than seeing the connection drop
/// (which would be reported "not placed", a different path). Bounded so the simulation reaches idle.
const KEEPALIVE_NS: u64 = 60_000_000;

/// A holder's plan: whether it acknowledges the record, and at what delay after its handshake completes.
#[derive(Clone, Copy)]
struct HolderPlan {
  /// Whether the holder serves (acknowledges) the record. A holder that does not serve keeps its
  /// connection open for [`KEEPALIVE_NS`] and then exits, so the owner's request to it genuinely waits.
  serves: bool,
  /// The delay after the handshake before the holder acts (serves, or simply stays alive).
  at_ns: u64,
}

/// The commit's outcome (its placement or the `ClusterError` string) and the sim time the dispatch took,
/// so a test can assert the commit ran past its base deadline (the extension is observable) or expired.
struct Outcome {
  placement: Result<Placement, String>,
  elapsed_ns: u64,
}

fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 1,
    tasks_per_shard: 64,
    timers_per_shard: 64,
    ring_entries: 64,
    step_budget_ns: 2_000_000_000,
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
    slates_rt::futures::sleep(POLL_NS).await;
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

/// Runs an `f = 2` commit — owner `1`, candidate holders `2` and `3` — over the sim fabric with `budget`
/// and the given per-holder plans. Each serving holder acknowledges after its planned delay, so the
/// owner's quorum fills over time. Returns the commit's [`Outcome`] (placement and elapsed sim time).
fn run_commit(budget: CommitBudget, plans: [HolderPlan; 2]) -> Outcome {
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
  // from, and holder i sends the port it listens on.
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
  let (result_tx, result_rx) = channel::<Outcome>();

  // The holder tasks: each handshakes, waits its planned delay, then either serves the record
  // (acknowledging) or simply stays alive to its keepalive and exits.
  let holder_setup = holder_identities
    .into_iter()
    .zip(owner_port_rx)
    .zip(holder_port_tx)
    .zip(plans);
  for (index, (((holder_identity, orx), htx), plan)) in holder_setup.enumerate() {
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
        slates_rt::futures::sleep(plan.at_ns).await;
        if plan.serves {
          let mut acceptor = Acceptor::new(holder_id, authority());
          // Ignore the result: a holder that serves before the commit resolves acknowledges cleanly; a
          // holder whose owner has already given up serves into a closed connection, which simply fails.
          let _ = serve_record(&mut endpoint, &mut acceptor).await;
        }
        // A non-serving holder has now stayed alive for its keepalive; it exits, closing its socket.
      })
      .unwrap();
  }

  // The owner task: dial each holder, establish, then commit under `budget`, timing the dispatch.
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
      let started = slates_rt::futures::now_ns();
      let placement = commit_record(
        OWNER,
        &mut owner_acceptor,
        &candidates,
        &record(b"head@v1"),
        QUORUM,
        remotes,
        budget,
      )
      .await
      .outcome
      .map_err(|e: ClusterError| e.to_string());
      let elapsed_ns = slates_rt::futures::now_ns().saturating_sub(started);
      let _ = result_tx.send(Outcome {
        placement,
        elapsed_ns,
      });
    })
    .unwrap();

  sim.run_until_idle();
  result_rx.try_recv().unwrap()
}

/// AC (§4.8 "late work"): a commit whose quorum is still filling as it passes its base deadline is
/// granted a bounded extension and commits, rather than being reported uncertain. The second holder
/// acknowledges only past the base deadline, so the commit could not have placed under a hard deadline —
/// that it both places **and** ran longer than the base deadline is the observable proof the extension
/// fired (non-vacuous: a silently-dead extension path would leave this uncertain at the base deadline).
#[test]
fn a_progressing_commit_is_extended_past_its_deadline_and_places() {
  let outcome = run_commit(
    CommitBudget::with_extension(
      BASE_DEADLINE_NS,
      POLL_NS,
      LOOKAHEAD_NUM,
      LOOKAHEAD_DEN,
      EXTENSION_NS,
      MAX_EXTENSIONS,
      STALL_WINDOW_NS,
    ),
    [
      HolderPlan {
        serves: true,
        at_ns: FIRST_ACK_NS,
      },
      HolderPlan {
        serves: true,
        at_ns: SECOND_ACK_NS,
      },
    ],
  );
  let placement = outcome
    .placement
    .expect("a progressing commit places under an extending budget");
  assert!(
    placement.placed(QUORUM),
    "the owner and both holders form the quorum: {placement:?}"
  );
  assert!(
    outcome.elapsed_ns > BASE_DEADLINE_NS,
    "the commit ran past its base deadline ({BASE_DEADLINE_NS} ns), so the extension is what let the \
     late acknowledgement land — elapsed {} ns",
    outcome.elapsed_ns
  );
}

/// AC (§4.8 "late work"): a commit that stalls — no holder ever acknowledges — is **not** extended
/// indefinitely; once its acknowledged set has not advanced for a stall window it is left to time out
/// and reported uncertain (partial acceptance may have occurred; it is never claimed placed). This is
/// the bound that keeps the extension from masking a genuinely stuck commit, and it proves the extension
/// is not an unconditional "wait forever": the dispatch gives up well within its full extension budget.
#[test]
fn a_stalled_commit_expires_and_is_reported_uncertain() {
  let outcome = run_commit(
    CommitBudget::with_extension(
      BASE_DEADLINE_NS,
      POLL_NS,
      LOOKAHEAD_NUM,
      LOOKAHEAD_DEN,
      EXTENSION_NS,
      MAX_EXTENSIONS,
      STALL_WINDOW_NS,
    ),
    [
      HolderPlan {
        serves: false,
        at_ns: KEEPALIVE_NS,
      },
      HolderPlan {
        serves: false,
        at_ns: KEEPALIVE_NS,
      },
    ],
  );
  match outcome.placement {
    Err(e) => assert!(
      e.contains("uncertain") || e.contains("Uncertain"),
      "a stalled commit is reported uncertain, not {e:?}"
    ),
    Ok(placement) => panic!("a stalled commit must not place: {placement:?}"),
  }
  // It gave up on the stall, not by exhausting the whole extension budget: the deadline could have grown
  // to BASE + MAX_EXTENSIONS × EXTENSION, and a stall-driven expiry stops well short of that.
  let full_budget_ns = BASE_DEADLINE_NS + u64::from(MAX_EXTENSIONS) * EXTENSION_NS;
  assert!(
    outcome.elapsed_ns >= BASE_DEADLINE_NS && outcome.elapsed_ns < full_budget_ns,
    "expired on the stall within the extension budget (base {BASE_DEADLINE_NS} ns ≤ elapsed {} ns < \
     full {full_budget_ns} ns)",
    outcome.elapsed_ns
  );
}
