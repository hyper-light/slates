#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Daemons form a **live fleet** (§4.8 "Membership"; §2.6 boot step 6, R5). Each daemon is started with a
//! `FleetTransport` naming its peers, and its control shard runs the membership loop: it dials each peer's
//! advertised socket, accepts each peer on its own per-peer socket, probes over the transport each protocol
//! period, replicates its volume heads to the peer holders, and folds the acknowledgements into the
//! `FleetNode` the verbs read for placement. The tests observe the whole boot-step-6 path in the daemon,
//! over real (loopback) UDP sessions with mutual TLS, using `Endpoint::accept` so no node is told a peer's
//! dial address in advance (only its advertised one). Daemons run concurrently in one process (the runtime's
//! shard ids are process-global, so their shards do not collide); each is given a distinct machine identity
//! so its host id — the fleet member id — is distinct.
//!
//! Coverage here: two daemons detect a dead peer and retire it; a provisioned head replicates across a
//! two-node fleet to the `f = 1` quorum and a holder durably holds it; **three** daemons form one fleet over
//! the per-peer socket mesh, the two survivors each retire a dead node, and the first-ranked survivor takes
//! over the dead owner's head (**five** at `f = 2`, over a multi-holder promotion quorum); a sealed
//! snapshot's **content** — written over the daemon's real NFS port — replicates to its holder by missing
//! set and places (§4.10); and a takeover successor **serves** the dead owner's bytes back over its own NFS
//! port. Real multi-process deployment and the connection-ID demux (many peers on one socket) are further
//! gates.

use std::time::{Duration, Instant};

use rustls::pki_types::PrivateKeyDer;
use slates_db::HostId;
use slates_db::register::{ObjectId, Quorum, rendezvous_first};
use slates_ipc::protocol::{
  Direction, NamePolicy, ReplyBody, RequestBody, Scope, SizeClass, SnapshotId, VolumeId, pack,
  unpack,
};
use slates_ipc::{ClientEnd, IpcError, connect};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_rt::tcp::{Ipv4Addr, SocketAddrV4};
use slates_server::daemon::host_id_of;
use slates_server::head::HeadValue;
use slates_server::{
  Daemon, DaemonConfig, FleetMembership, FleetPeer, FleetTransport, SegmentSource,
};

mod common;
use std::net::TcpStream;

use common::nfs::{create, lookup, mount, read, write};
use slates_transport::handshake::Identity;
use slates_wire::request::RequestId;

/// Shape: the reply deadline (nanoseconds): five seconds, far past any served verb.
const DEADLINE_NS: u64 = 5_000_000_000;
/// Shape: how long a client waits for the daemon or a full ring before giving up.
const CREDIT_WAIT: Duration = Duration::from_secs(5);

/// The TLS server name every fleet node presents (a single fleet's shared name; the certificate pins who).
const NAME: &str = "slates-fleet";
/// Shape: the profile probe budget (milliseconds); an input to derivations, not a gate.
const PROBE_MS: u64 = 5;
/// Shape: how long to let the fleet form — the loops establish their sessions and exchange several probes,
/// each confirming the other alive over the transport — before the peer is killed. Far past the handshake
/// and a few protocol periods on loopback.
const FORMATION_SETTLE: Duration = Duration::from_secs(2);
/// Shape: how long to wait for the survivor to detect and retire the dead peer before the test fails — far
/// past the probe timeout plus the suspicion window. The fleet tests are serialised (see
/// [`serialize_fleet_tests`]), so this is measured against a quiet machine.
const RETIREMENT_DEADLINE: Duration = Duration::from_secs(10);
/// Shape: how long to wait for an N-node fleet to fully form (every node seeing every peer alive) before the
/// test fails. Polled, not a fixed settle, so it returns the instant the mesh is up; the deadline is wide
/// because a larger mesh has more sessions to establish (each node dials and accepts every peer) and the
/// daemons start one after another.
const FORMATION_DEADLINE: Duration = Duration::from_secs(15);

/// Each fleet test starts several daemons — every daemon is a shard thread plus its doorbell thread — so
/// running the tests concurrently oversubscribes the machine and stretches the probe and commit timing
/// enough to flake. The test threads wait by polling; since the runtime's `sleep` is unavailable off a shard
/// and `std::thread::sleep` is disallowed, they `std::thread::yield_now()` between checks rather than
/// `std::hint::spin_loop()` — yielding the core to the daemon shard threads they are waiting on, instead of
/// pinning it and starving the very daemons whose progress the poll is waiting for. This lock serialises the
/// heavy fleet tests so each runs against a quiet machine — the test-harness exception to R2's no-`Mutex`
/// rule (D-8 exception 3).
#[allow(clippy::disallowed_types)]
static FLEET_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquires the fleet-test lock, recovering it if a previous test poisoned it by panicking, so one
/// failure reports itself rather than cascading into every later test.
fn serialize_fleet_tests() -> std::sync::MutexGuard<'static, ()> {
  FLEET_TEST_LOCK
    .lock()
    .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A machine profile for fleet node `node`, given a distinct machine identity so its host id — the fleet
/// member id, derived from the identity's hash — is distinct: two daemons on one machine would otherwise be
/// one fleet member. Only the identity string changes; the measured derivations (from memory, cores, page)
/// are untouched.
fn profile(node: &str) -> MachineProfile {
  let mut profile = MachineProfile::measure(ProfileOptions {
    budget_per_probe: Duration::from_millis(PROBE_MS),
    codecs: false,
    core_matrix: false,
  });
  profile.facts.identity.cpu = format!("fleet-node-{node}");
  profile
}

/// A self-signed fleet TLS identity, minted with `rcgen` — the test's stand-in for the operator-provisioned
/// certificate (§4.8 "certificates provisioned by the operator").
fn self_signed() -> Identity {
  let key = rcgen::KeyPair::generate().unwrap();
  let cert = rcgen::CertificateParams::new(vec![NAME.to_owned()])
    .unwrap()
    .self_signed(&key)
    .unwrap();
  Identity::from_der(
    cert.der().clone(),
    PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
  )
}

/// Two **distinct** free localhost UDP ports: both sockets are bound at once (so the OS gives two different
/// ports) and dropped before the daemons rebind them — binding each separately could hand back the same
/// port twice, which would make the two nodes collide on one address. Tests may use `std::net` (as
/// `nfs_mount.rs` does); the daemon itself never links it (R1).
fn four_free_ports() -> [u16; 4] {
  // All four sockets bound at once, so the OS hands back four distinct ports; dropped before the daemons
  // rebind them (each node needs a probe address and a record address).
  let sockets: Vec<std::net::UdpSocket> = (0..4)
    .map(|_| std::net::UdpSocket::bind("127.0.0.1:0").unwrap())
    .collect();
  let mut ports = [0u16; 4];
  for (slot, socket) in ports.iter_mut().zip(&sockets) {
    *slot = socket.local_addr().unwrap().port();
  }
  ports
}

/// A fleet node's whole setup: its profile (with a distinct identity), its host id, its fleet TLS identity,
/// and its two advertised addresses (probe and record).
struct Node {
  profile: MachineProfile,
  host: HostId,
  identity: Identity,
  address: SocketAddrV4,
  record_address: SocketAddrV4,
}

/// A process-unique token, so nodes across concurrently-running tests get distinct host ids (and so
/// distinct segment and instance names, which the host id keys) — the tests run in parallel by default.
fn unique() -> u64 {
  static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
  NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn node(name: &str, probe_port: u16, record_port: u16) -> Node {
  let mut profile = profile(name);
  profile.facts.identity.cpu = format!("{}-{}", profile.facts.identity.cpu, unique());
  let host = HostId(host_id_of(&profile.facts.identity));
  Node {
    host,
    identity: self_signed(),
    address: SocketAddrV4::new(Ipv4Addr::LOCALHOST, probe_port),
    record_address: SocketAddrV4::new(Ipv4Addr::LOCALHOST, record_port),
    profile,
  }
}

/// The addresses and certificate of a node's one peer.
struct Peer {
  host: HostId,
  address: SocketAddrV4,
  record_address: SocketAddrV4,
  certificate: rustls::pki_types::CertificateDer<'static>,
}

/// Starts the daemon for `this`, configured to join a fleet with `peer` as its one peer.
fn start(this: Node, peer: Peer) -> Daemon {
  let pid = std::process::id();
  let instance = format!("fleet-{}-{pid}", this.host.0);
  let config = DaemonConfig::derive(&this.profile, &instance)
    .with_shards(1)
    .with_fleet(FleetMembership {
      quorum: Quorum { f: 1 },
      peers: vec![peer.host],
    });
  let transport = FleetTransport {
    identity: this.identity,
    name: NAME.to_owned(),
    // One peer, so this node serves it on this node's own advertised addresses (the per-peer serve socket
    // is this node's single advertised pair). An N-node fleet gives each peer its own serve pair.
    peers: vec![FleetPeer {
      host: peer.host,
      address: peer.address,
      record_address: peer.record_address,
      probe_bind: this.address,
      record_bind: this.record_address,
      certificate: peer.certificate,
    }],
  };
  Daemon::start_with_fleet(
    &this.profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-fleet-{}-{pid}", this.host.0),
    },
    Some(transport),
  )
  .expect("the fleet daemon starts")
}

/// AC (§4.8, boot step 6): two daemons form a live fleet — their control-shard membership loops dial,
/// accept and probe each other over the transport (using `Endpoint::accept`, so neither is told the other's
/// dial address in advance) — and when one dies, the survivor **detects it over the transport and retires
/// it**. The retirement is the non-vacuous proof the loop ran end to end: the seeded configuration would
/// hold the peer alive forever, so a peer that transitions from alive to gone did so only because the loop
/// probed it, timed out, aged the suspicion to death, and folded that into the `FleetNode` the verbs read.
#[test]
fn a_daemon_detects_its_dead_peer_over_the_transport_and_retires_it() {
  let _serial = serialize_fleet_tests();
  let [pa_probe, pa_record, pb_probe, pb_record] = four_free_ports();
  let a = node("a", pa_probe, pa_record);
  let b = node("b", pb_probe, pb_record);
  assert_ne!(
    a.host, b.host,
    "distinct machine identities give distinct host ids"
  );

  let host_b = b.host;
  let peer_of_a = Peer {
    host: b.host,
    address: b.address,
    record_address: b.record_address,
    certificate: b.identity.certificate(),
  };
  let peer_of_b = Peer {
    host: a.host,
    address: a.address,
    record_address: a.record_address,
    certificate: a.identity.certificate(),
  };
  let daemon_a = start(a, peer_of_a);
  let daemon_b = start(b, peer_of_b);

  // Let the fleet form: the loops establish their sessions and exchange probes over the transport. The
  // test thread is not a runtime task, so it waits by yielding on the clock (as the other daemon tests do
  // — the runtime's `futures::sleep` is unavailable off a shard).
  let settle = Instant::now() + FORMATION_SETTLE;
  while Instant::now() < settle {
    std::thread::yield_now();
  }
  assert!(
    daemon_a.fleet_members().contains(&host_b),
    "the fleet is up: A holds B in its membership"
  );

  // B dies — its shards, and so its serve loop, stop — so A's probes of B now time out.
  daemon_b.stop();

  // A's membership loop detects the timeouts, ages the suspicion to death, and retires B: a transition only
  // the loop can make over the transport.
  let deadline = Instant::now() + RETIREMENT_DEADLINE;
  let mut retired = false;
  while Instant::now() < deadline {
    if !daemon_a.fleet_members().contains(&host_b) {
      retired = true;
      break;
    }
    std::thread::yield_now();
  }

  daemon_a.stop();
  assert!(
    retired,
    "daemon A's membership loop detected B's death over the transport and retired it"
  );
}

/// `count` distinct free localhost UDP ports, all bound at once so the OS hands back distinct ports, then
/// dropped before the daemons rebind them (binding one at a time could repeat a port). The generalization of
/// [`four_free_ports`] the N-node mesh needs.
fn free_ports(count: usize) -> Vec<u16> {
  let sockets: Vec<std::net::UdpSocket> = (0..count)
    .map(|_| std::net::UdpSocket::bind("127.0.0.1:0").unwrap())
    .collect();
  sockets
    .iter()
    .map(|s| s.local_addr().unwrap().port())
    .collect()
}

/// A fleet node's identity parts (no addresses — an N-node node serves each peer on its own socket, so
/// addresses are per ordered pair, allocated in the mesh below, not per node).
fn fleet_node(name: &str) -> (MachineProfile, HostId, Identity) {
  let mut profile = profile(name);
  profile.facts.identity.cpu = format!("{}-{}", profile.facts.identity.cpu, unique());
  let host = HostId(host_id_of(&profile.facts.identity));
  (profile, host, self_signed())
}

fn loopback(port: u16) -> SocketAddrV4 {
  SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)
}

/// One (probe, record) port pair per ordered pair (server `i` serves client `j`, `i != j`): `serve[i][j]`.
/// The full-mesh grid is naturally two-index, so the range loops are kept.
#[allow(clippy::needless_range_loop)]
fn mesh_serve_ports(n: usize) -> Vec<Vec<(u16, u16)>> {
  let flat = free_ports(2 * n * (n - 1));
  let mut serve = vec![vec![(0u16, 0u16); n]; n];
  let mut cursor = 0;
  for i in 0..n {
    for j in 0..n {
      if i != j {
        serve[i][j] = (flat[cursor], flat[cursor + 1]);
        cursor += 2;
      }
    }
  }
  serve
}

/// Starts one daemon per node over the per-peer socket mesh: node `i` serves each peer `j` on `serve[i][j]`
/// and dials peer `j` at peer `j`'s serve-for-`i` socket `serve[j][i]`. Returns the daemons in node order.
/// The fleet's fault tolerance is `f = 1` (a three-node fleet's shape); [`start_mesh_with_f`] takes a larger
/// `f` for a fleet that keeps a quorum through more deaths (2f + 1 nodes).
fn start_mesh(
  nodes: Vec<(MachineProfile, HostId, Identity)>,
  hosts: &[HostId],
  certs: &[rustls::pki_types::CertificateDer<'static>],
  serve: &[Vec<(u16, u16)>],
) -> Vec<Daemon> {
  start_mesh_with_f(nodes, hosts, certs, serve, 1)
}

/// [`start_mesh`] with an explicit fault tolerance `f`: the fleet's quorum is `f + 1` and every object has
/// `2f + 1` candidate holders, so a fleet of `2f + 1` nodes keeps a quorum through `f` deaths. A five-node
/// `f = 2` fleet is the smallest whose takeover promotion spans **several** surviving holders (a quorum of
/// three: the successor plus two others), the multi-holder promotion the record-plane coordinator drives.
fn start_mesh_with_f(
  nodes: Vec<(MachineProfile, HostId, Identity)>,
  hosts: &[HostId],
  certs: &[rustls::pki_types::CertificateDer<'static>],
  serve: &[Vec<(u16, u16)>],
  f: u32,
) -> Vec<Daemon> {
  let pid = std::process::id();
  let n = hosts.len();
  nodes
    .into_iter()
    .enumerate()
    .map(|(i, (profile, host, identity))| {
      let peers: Vec<FleetPeer> = (0..n)
        .filter(|&j| j != i)
        .map(|j| FleetPeer {
          host: hosts[j],
          address: loopback(serve[j][i].0),
          record_address: loopback(serve[j][i].1),
          probe_bind: loopback(serve[i][j].0),
          record_bind: loopback(serve[i][j].1),
          certificate: certs[j].clone(),
        })
        .collect();
      let member_peers: Vec<HostId> = (0..n).filter(|&j| j != i).map(|j| hosts[j]).collect();
      let instance = format!("fleet3-{}-{pid}", host.0);
      let config = DaemonConfig::derive(&profile, &instance)
        .with_shards(1)
        .with_fleet(FleetMembership {
          quorum: Quorum { f },
          peers: member_peers,
        });
      let transport = FleetTransport {
        identity,
        name: NAME.to_owned(),
        peers,
      };
      Daemon::start_with_fleet(
        &profile,
        config,
        SegmentSource::Create {
          name: format!("slates-seg-fleet3-{}-{pid}", host.0),
        },
        Some(transport),
      )
      .expect("the fleet daemon starts")
    })
    .collect()
}

/// AC (§4.8, boot step 6, N-node): three daemons form the full **direct probe mesh** — each of the three
/// nodes establishes a live probe session to each of its two peers, N·(N−1) = 6 sessions over
/// `Endpoint::accept`, and every node reports its own mesh complete ([`Daemon::fleet_meshed`], every
/// configured peer probed, not merely believed alive by the seeded membership). This is the N-node
/// formation the two-node fleet tests exercise at their single-peer degenerate, now at the smallest fleet
/// whose mesh is non-trivial: each node dials two peers on distinct sockets and serves two on its own
/// per-peer sockets, and all six handshakes complete concurrently as the daemons boot one after another.
/// It asserts **formation only** — the death-and-retirement half is
/// [`three_daemons_form_a_fleet_and_the_survivors_retire_a_dead_node`], kept as its own test so a formation
/// regression is distinguishable from a retirement one.
#[test]
fn three_daemons_form_a_full_mesh() {
  let _serial = serialize_fleet_tests();
  let names = ["a", "b", "c"];
  let n = names.len();
  let nodes: Vec<(MachineProfile, HostId, Identity)> =
    names.iter().map(|name| fleet_node(name)).collect();
  let hosts: Vec<HostId> = nodes.iter().map(|(_, host, _)| *host).collect();
  let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
    nodes.iter().map(|(_, _, id)| id.certificate()).collect();
  let serve = mesh_serve_ports(n);
  let daemons = start_mesh(nodes, &hosts, &certs, &serve);

  // Poll until every node's full direct mesh has formed (all six probe sessions established), bounded by
  // the formation deadline.
  let deadline = Instant::now() + FORMATION_DEADLINE;
  while Instant::now() < deadline && !daemons.iter().all(Daemon::fleet_meshed) {
    std::thread::yield_now();
  }
  let meshed: Vec<(&str, bool)> = names
    .iter()
    .copied()
    .zip(daemons.iter().map(Daemon::fleet_meshed))
    .collect();
  let all_meshed = meshed.iter().all(|&(_, m)| m);
  // Stop the daemons before asserting, so a failure leaves none running.
  for daemon in daemons {
    daemon.stop();
  }
  assert!(
    all_meshed,
    "the three-node direct probe mesh did not fully form within the deadline: {meshed:?}"
  );
}

/// AC (§4.8, boot step 6, N-node): **three** daemons form one live fleet over the per-peer socket mesh —
/// each node serves each of its two peers on its own advertised socket pair (since `Endpoint::accept` pins
/// one peer per socket) and dials each peer's — and when one node dies, the **two survivors each detect it
/// over the transport and retire it**. Three nodes is the smallest fleet that keeps a quorum through a single
/// death at `f = 1` (2f + 1 = 3), so it is the shape a fault-tolerant fleet actually runs; the retirement by
/// both survivors is the non-vacuous proof each ran its membership loop end to end over real UDP sessions.
/// It waits for the real direct mesh ([`assert_fleet_forms`], every probe session formed) before killing
/// C, so a survivor is retiring a peer it actually established a session with — not one the seeded
/// membership merely believes alive.
///
/// Formerly `#[ignore]`d and now reliable (measured 27/27) once three defects were fixed: the transport's
/// handshake confirmation + fast establish retransmission (formation — `Endpoint::establish`), and a
/// **runtime timer bug** — the wheel's `cancel` unlinked by bare index before validating the id's
/// generation, so a stale cancel (a fired timer whose slot a later timer had reused) orphaned that live
/// timer, stranding a survivor's SWIM probe of the dead node so it never timed out under a fleet's own
/// load (`crates/rt/src/timer.rs`, regressed by `a_stale_cancel_does_not_orphan_the_timer_that_reused_the_slot`).
#[test]
fn three_daemons_form_a_fleet_and_the_survivors_retire_a_dead_node() {
  let _serial = serialize_fleet_tests();
  let names = ["a", "b", "c"];
  let n = names.len();
  let nodes: Vec<(MachineProfile, HostId, Identity)> =
    names.iter().map(|name| fleet_node(name)).collect();
  let hosts: Vec<HostId> = nodes.iter().map(|(_, host, _)| *host).collect();
  let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
    nodes.iter().map(|(_, _, id)| id.certificate()).collect();
  assert_eq!(
    hosts
      .iter()
      .collect::<std::collections::BTreeSet<_>>()
      .len(),
    n,
    "the three machine identities give three distinct host ids"
  );

  let serve = mesh_serve_ports(n);
  let mut daemons = start_mesh(nodes, &hosts, &certs, &serve);

  assert_fleet_forms(&daemons, &hosts, &names);

  // Node C (index 2) dies; its serve loops stop, so A's and B's probes of C time out.
  let dead = hosts[2];
  daemons.pop().expect("three daemons").stop();
  let all_retired = poll_survivors_retire(&daemons, dead);
  // Scaling down must not strand new work: with C retired (the configuration version advanced), a volume
  // provisioned on A now must still place — over B, the one remaining candidate — under the new generation.
  // Before the owner's acceptor followed the version, A's own hold refused its record `ForeignGeneration`
  // and nothing provisioned after a membership change ever placed; the head placing is the proof it does.
  let pid = std::process::id();
  let placed_after_retirement = all_retired && {
    let mut client = Client::connect(&format!("fleet3-{}-{pid}", hosts[0].0));
    match client.call(&scratch("after-retirement")) {
      ReplyBody::Created { id } => poll_head_placed(&daemons[0], ObjectId(id.bytes)),
      _ => false,
    }
  };
  for daemon in daemons {
    daemon.stop();
  }
  assert!(
    all_retired,
    "both survivors detected C's death over the transport and retired it"
  );
  assert!(
    placed_after_retirement,
    "a head provisioned on a survivor after the retirement places under the advanced generation"
  );
}

/// Polls until every daemon's **direct probe mesh** has formed — each node has a live probe session to
/// each of its peers ([`Daemon::fleet_meshed`]) — or fails at the formation deadline naming who is still
/// unmeshed. It waits for the real mesh, not the membership's optimistically seeded alive set (every
/// configured peer is believed alive from boot, so [`Daemon::fleet_members`] reports a full fleet before a
/// single session forms): a survivor can only detect a peer's death over a session it actually formed, so
/// a node killed before its peers meshed to it could never be retired. Polling rather than a fixed settle
/// returns as soon as the mesh is up and tolerates a slower-forming larger mesh.
fn assert_fleet_forms(daemons: &[Daemon], _hosts: &[HostId], names: &[&str]) {
  let deadline = Instant::now() + FORMATION_DEADLINE;
  while Instant::now() < deadline {
    if daemons.iter().all(Daemon::fleet_meshed) {
      return;
    }
    std::thread::yield_now();
  }
  // Past the deadline and still not meshed: assert with a message naming the first unmeshed node (its
  // seeded members view is shown to make the seeded-vs-formed distinction legible on a failure).
  for (i, daemon) in daemons.iter().enumerate() {
    assert!(
      daemon.fleet_meshed(),
      "node {} did not form its full probe mesh within the formation deadline (it sees members {:?})",
      names[i],
      daemon.fleet_members()
    );
  }
}

/// Polls until every survivor has retired `dead` from its membership, or the retirement deadline passes;
/// returns whether they all did.
fn poll_survivors_retire(survivors: &[Daemon], dead: HostId) -> bool {
  let deadline = Instant::now() + RETIREMENT_DEADLINE;
  let mut retired = vec![false; survivors.len()];
  while Instant::now() < deadline && !retired.iter().all(|&r| r) {
    for (survivor, done) in survivors.iter().zip(retired.iter_mut()) {
      if !survivor.fleet_members().contains(&dead) {
        *done = true;
      }
    }
    std::thread::yield_now();
  }
  retired.iter().all(|&r| r)
}

/// A minimal client of a daemon's own rendezvous (as in the other daemon tests): connect and call verbs.
struct Client {
  end: ClientEnd,
  client: u32,
  sequence: u32,
}

impl Client {
  fn connect(instance: &str) -> Client {
    let started = Instant::now();
    loop {
      match connect(instance) {
        Ok(connected) => {
          let client = connected.region.client_id();
          return Client {
            end: ClientEnd::with_doorbell(connected.region, connected.doorbell),
            client,
            sequence: 0,
          };
        }
        Err(IpcError::DaemonUnavailable { .. }) if started.elapsed() < CREDIT_WAIT => {
          std::thread::yield_now();
        }
        Err(e) => panic!("{e}"),
      }
    }
  }

  fn call(&mut self, body: &RequestBody) -> ReplyBody {
    self.sequence += 1;
    let id = RequestId {
      client: self.client,
      sequence: self.sequence,
    };
    let index = self.end.next_request_index();
    let slot = pack(
      self.end.region_mut(),
      Direction::Request,
      index,
      id.word(),
      body,
    )
    .unwrap();
    let started = Instant::now();
    loop {
      match self.end.send(&slot) {
        Ok(()) => break,
        Err(IpcError::RingFull) if started.elapsed() < CREDIT_WAIT => std::thread::yield_now(),
        Err(e) => panic!("{e}"),
      }
    }
    let reply = self.end.wait(Some(DEADLINE_NS)).unwrap();
    unpack(self.end.region(), reply.kind, &reply.payload).unwrap()
  }
}

/// A scratch-volume create request.
fn scratch(name: &str) -> RequestBody {
  RequestBody::Create {
    name: name.to_owned(),
    size: SizeClass::Bounded { limit: 1 << 20 },
    names: NamePolicy::Exact,
    require_locked: false,
    base: None,
  }
}

/// AC (§4.8, boot step 6, the cross-node commit): in a two-node `f = 1` fleet, a volume provisioned on one
/// daemon has its head **replicated to the peer holder** — the owner's control-shard loop ships the head
/// record over the transport and the peer serves it, reaching the `f + 1` quorum — so the head becomes
/// region-placed. Non-vacuous: at `f = 1` a solo head is not region-placed (the owner's local hold is one
/// of the two acknowledgements the quorum needs), so `fleet_head_placed` turning true is the proof the peer
/// acknowledged the replicated record over the wire.
#[test]
fn a_provisioned_head_replicates_across_the_fleet() {
  let _serial = serialize_fleet_tests();
  let [pa_probe, pa_record, pb_probe, pb_record] = four_free_ports();
  let a = node("a", pa_probe, pa_record);
  let b = node("b", pb_probe, pb_record);
  let pid = std::process::id();
  let instance_a = format!("fleet-{}-{pid}", a.host.0);
  let peer_of_a = Peer {
    host: b.host,
    address: b.address,
    record_address: b.record_address,
    certificate: b.identity.certificate(),
  };
  let peer_of_b = Peer {
    host: a.host,
    address: a.address,
    record_address: a.record_address,
    certificate: a.identity.certificate(),
  };
  let daemon_a = start(a, peer_of_a);
  let daemon_b = start(b, peer_of_b);

  // Let the fleet form before provisioning: the loops dial and establish their sessions over the transport,
  // undisturbed by the client and the placement polling below (which run on the same control shard).
  let settle = Instant::now() + FORMATION_SETTLE;
  while Instant::now() < settle {
    std::thread::yield_now();
  }

  // Provision a volume on A; its object is the volume id.
  let mut client = Client::connect(&instance_a);
  let ReplyBody::Created { id } = client.call(&scratch("replicated")) else {
    daemon_a.stop();
    daemon_b.stop();
    panic!("the volume was not created");
  };
  let object = ObjectId(id.bytes);

  // A's control-shard loop ships the head to B over the record connection; B serves it; the quorum is
  // reached and A records the placement. Poll until A reports the head region-placed.
  let deadline = Instant::now() + Duration::from_secs(15);
  let mut placed = false;
  while Instant::now() < deadline {
    if daemon_a.fleet_head_placed(object) {
      placed = true;
      break;
    }
    std::thread::yield_now();
  }
  // The verbs read the same recorded placement (§4.8 D-18, `await placed(region)`): a client asking the
  // owner for the region scope is told it is placed. Before the verbs consulted the recorded
  // acknowledgements they recomputed the owner's local placement — the owner alone — so a fleet's head was
  // reported unplaced forever, however many holders held it; `placed: true` here is the proof they now read
  // what the fleet committed.
  let placed_by_verb = matches!(
    client.call(&RequestBody::AwaitPlaced {
      volume: id,
      snapshot: None,
      scope: Scope::Region,
    }),
    ReplyBody::Placed { placed: true, .. }
  );

  daemon_a.stop();
  daemon_b.stop();
  assert!(
    placed,
    "the provisioned head replicated to the peer holder and reached the f=1 quorum"
  );
  assert!(
    placed_by_verb,
    "the owner's `await placed(region)` verb reports the replicated head placed"
  );
}

/// AC (§4.8, boot step 6, durable holds): in a two-node `f = 1` fleet, the peer holder **durably holds**
/// the head the owner replicates to it — the accepted record survives in the shard state (in the object's
/// per-object acceptor and the routing view), not only in the transient serve task that received it. This
/// is exactly the state a survivor's phase-one recovery reads on a takeover: the newest committed record,
/// recoverable from a holder. Non-vacuous: the holder holds nothing for the object until the owner's record
/// commit reaches it over the transport, so `fleet_holder_head` turning `Some` — naming the owner and the
/// head's value — is the proof the holder accepted and stored the replicated record, distinct from the
/// owner's own `fleet_head_placed` (which reports the quorum, not what any one holder retains).
#[test]
fn a_holder_durably_holds_the_owners_replicated_head() {
  let _serial = serialize_fleet_tests();
  let [pa_probe, pa_record, pb_probe, pb_record] = four_free_ports();
  let a = node("a", pa_probe, pa_record);
  let b = node("b", pb_probe, pb_record);
  let host_a = a.host;
  let pid = std::process::id();
  let instance_a = format!("fleet-{}-{pid}", a.host.0);
  let peer_of_a = Peer {
    host: b.host,
    address: b.address,
    record_address: b.record_address,
    certificate: b.identity.certificate(),
  };
  let peer_of_b = Peer {
    host: a.host,
    address: a.address,
    record_address: a.record_address,
    certificate: a.identity.certificate(),
  };
  let daemon_a = start(a, peer_of_a);
  let daemon_b = start(b, peer_of_b);

  // Let the fleet form before provisioning, as the replication test does.
  let settle = Instant::now() + FORMATION_SETTLE;
  while Instant::now() < settle {
    std::thread::yield_now();
  }

  // Provision a volume on A; its object is the volume id, and its head's value is the id bytes.
  let mut client = Client::connect(&instance_a);
  let ReplyBody::Created { id } = client.call(&scratch("held")) else {
    daemon_a.stop();
    daemon_b.stop();
    panic!("the volume was not created");
  };
  let object = ObjectId(id.bytes);

  // A ships the head to B (the holder) over the record connection; B accepts it into the object's durable
  // acceptor and tracks it in its routing view. Poll until B reports it durably holds A's head.
  let deadline = Instant::now() + Duration::from_secs(15);
  let mut held = None;
  while Instant::now() < deadline {
    if let Some(record) = daemon_b.fleet_holder_head(object) {
      held = Some(record);
      break;
    }
    std::thread::yield_now();
  }

  daemon_a.stop();
  daemon_b.stop();
  let held = held.expect("B durably holds the head A replicated to it");
  assert_eq!(
    held.0, host_a,
    "B records A as the owner of the held object"
  );
  let head = HeadValue::from_record_bytes(&held.1).expect("B holds a well-formed head value");
  assert!(
    head.manifest.is_none(),
    "an unsealed volume's head names no content: {head:?}"
  );
  assert!(
    matches!(head.size, slates_db::catalog::SizeClass::Bounded { limit } if limit == 1 << 20),
    "B holds the head's catalog essentials (the size class the volume was created with): {head:?}"
  );
}

/// Polls until both survivor daemons durably hold `object`'s head (the owner shipped it to each candidate
/// holder), or the hold deadline passes; returns whether they both did — [`poll_all_hold`] over the pair.
fn poll_both_hold(first: &Daemon, second: &Daemon, object: ObjectId) -> bool {
  poll_all_hold(&[first, second], object)
}

/// Polls until `daemon` reports `object` region-placed — the takeover re-committed the adopted head under
/// its ownership — or the takeover deadline passes; returns whether it did.
fn poll_head_placed(daemon: &Daemon, object: ObjectId) -> bool {
  let deadline = Instant::now() + Duration::from_secs(25);
  while Instant::now() < deadline {
    if daemon.fleet_head_placed(object) {
      return true;
    }
    std::thread::yield_now();
  }
  false
}

/// AC (§4.8 "Promotion and takeover", boot step 6, N-node): three daemons form one `f = 1` fleet; a volume
/// is provisioned on the node that then dies, and the **survivor rendezvous ranks first takes over its
/// head** — it runs phase one over the surviving candidate holder, adopts the head that committed under the
/// old owner, re-commits it under the new epoch, and serves it region-placed **under its own ownership**.
/// This is the smallest real takeover: three nodes keep a quorum through one death at `f = 1` (2f + 1 = 3),
/// and the successor plus the remaining holder are exactly the `f + 1 = 2` promises phase one needs, so the
/// adopted head is at least as new as anything that ever committed (Continuity). Non-vacuous on two counts:
/// the successor holds the head only as a candidate holder before the death (it is not the owner, so its
/// `placed_heads` has no record for the object — `fleet_head_placed` is false), and the seeded membership
/// would never reassign ownership; so the successor reporting the object **region-placed and owned by
/// itself** after the death is a transition only the takeover drive can make over the transport.
#[test]
fn three_daemons_take_over_a_dead_owners_head() {
  let _serial = serialize_fleet_tests();
  let names = ["a", "b", "c"];
  let n = names.len();
  let nodes: Vec<(MachineProfile, HostId, Identity)> =
    names.iter().map(|name| fleet_node(name)).collect();
  let hosts: Vec<HostId> = nodes.iter().map(|(_, host, _)| *host).collect();
  let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
    nodes.iter().map(|(_, _, id)| id.certificate()).collect();
  assert_eq!(
    hosts
      .iter()
      .collect::<std::collections::BTreeSet<_>>()
      .len(),
    n,
    "the three machine identities give three distinct host ids"
  );

  let pid = std::process::id();
  // Node A (index 0) is the owner that will die; the client provisions the volume on it.
  let instance_a = format!("fleet3-{}-{pid}", hosts[0].0);
  let serve = mesh_serve_ports(n);
  let mut daemons = start_mesh(nodes, &hosts, &certs, &serve);

  assert_fleet_forms(&daemons, &hosts, &names);

  // Provision a volume on A; its object is the volume id, and its head's value is the id bytes.
  let mut client = Client::connect(&instance_a);
  let ReplyBody::Created { id } = client.call(&scratch("taken-over")) else {
    for daemon in daemons {
      daemon.stop();
    }
    panic!("the volume was not created");
  };
  let object = ObjectId(id.bytes);

  // Wait until BOTH survivors hold A's head (A ships it to each) — so after A dies, the successor and the
  // remaining holder both have the committed record phase-one recovery reads.
  assert!(
    poll_both_hold(&daemons[1], &daemons[2], object),
    "both survivors hold A's head before A dies (the record replicated to each candidate holder)"
  );

  // A dies. The survivor rendezvous ranks first for the object takes it over.
  let owner = daemons.remove(0);
  owner.stop();
  let successor = rendezvous_first(&[hosts[1], hosts[2]], object).expect("a survivor takes over");
  // Map the successor host back to its (now index-shifted) daemon: survivors are daemons[0]=hosts[1],
  // daemons[1]=hosts[2].
  let successor_index = if successor == hosts[1] { 0 } else { 1 };

  // The successor drives phase one over the surviving holder, adopts the committed head, re-commits it under
  // the new epoch, and reports it region-placed under its own ownership.
  let served = poll_head_placed(&daemons[successor_index], object);
  let held = daemons[successor_index].fleet_holder_head(object);

  for daemon in daemons {
    daemon.stop();
  }
  assert!(
    served,
    "the successor took over the dead owner's head and served it region-placed under its ownership"
  );
  let held = held.expect("the successor still holds the taken-over head");
  assert_eq!(
    held.0, successor,
    "the successor is now the object's owner (the takeover reassigned ownership)"
  );
  let head = HeadValue::from_record_bytes(&held.1).expect("a well-formed head value");
  assert_eq!(
    head.name, "taken-over",
    "the taken-over head's value survived the promotion and re-commit"
  );
}

/// Polls until every survivor in `survivors` holds `object`'s head as a candidate holder
/// ([`Daemon::fleet_holder_head`]), or the deadline passes; returns whether they all did. Used to confirm a
/// dead owner's head reached every surviving candidate before the death, so the takeover's promotion quorum
/// (the successor plus `f` other holders) is available.
fn poll_all_hold(survivors: &[&Daemon], object: ObjectId) -> bool {
  let deadline = Instant::now() + Duration::from_secs(25);
  while Instant::now() < deadline {
    if survivors
      .iter()
      .all(|daemon| daemon.fleet_holder_head(object).is_some())
    {
      return true;
    }
    std::thread::yield_now();
  }
  false
}

/// AC (§4.8 "Promotion and takeover", one batched phase-one round across the neighbourhood): **five** daemons
/// form one `f = 2` fleet; a volume is provisioned on the node that then dies, and the survivor rendezvous
/// ranks first takes over its head by promoting over **several** surviving holders — the `f + 1 = 3` promise
/// quorum a five-node fleet needs (the successor plus two other holders), reached over the record-plane
/// coordinator's several sessions. This is the multi-holder promotion a per-peer ship task could not drive: it
/// held only its own peer's session and could reach a one-holder (`f = 1`) quorum only, so at `f = 2` it would
/// never assemble three promises and the takeover would starve. Five nodes keep a quorum through one death at
/// `f = 2` (2f + 1 = 5), and every node is a candidate (the neighbourhood is the fleet), so the owner ships the
/// head to all four others and, after the death, the successor adopts it from the quorum and re-commits it
/// under the new epoch. Non-vacuous on the same two counts as the three-node takeover — the successor holds the
/// head only as a candidate before the death, and the seeded membership never reassigns ownership — with the
/// added force that the promotion **must** span more than one remote holder.
#[test]
fn five_daemons_take_over_a_dead_owners_head_over_a_multi_holder_quorum() {
  let _serial = serialize_fleet_tests();
  let names = ["a", "b", "c", "d", "e"];
  let n = names.len();
  let nodes: Vec<(MachineProfile, HostId, Identity)> =
    names.iter().map(|name| fleet_node(name)).collect();
  let hosts: Vec<HostId> = nodes.iter().map(|(_, host, _)| *host).collect();
  let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
    nodes.iter().map(|(_, _, id)| id.certificate()).collect();
  assert_eq!(
    hosts
      .iter()
      .collect::<std::collections::BTreeSet<_>>()
      .len(),
    n,
    "the five machine identities give five distinct host ids"
  );

  let pid = std::process::id();
  // Node A (index 0) is the owner that will die; the client provisions the volume on it.
  let instance_a = format!("fleet3-{}-{pid}", hosts[0].0);
  let serve = mesh_serve_ports(n);
  let mut daemons = start_mesh_with_f(nodes, &hosts, &certs, &serve, 2);

  assert_fleet_forms(&daemons, &hosts, &names);

  // Provision a volume on A; its object is the volume id, and its head's value is the id bytes.
  let mut client = Client::connect(&instance_a);
  let ReplyBody::Created { id } = client.call(&scratch("taken-over-5")) else {
    for daemon in daemons {
      daemon.stop();
    }
    panic!("the volume was not created");
  };
  let object = ObjectId(id.bytes);

  // Wait until all four other candidates hold A's head, so after A dies the successor plus two other holders
  // — the f + 1 = 3 promise quorum — are all available.
  let survivors: Vec<&Daemon> = daemons[1..].iter().collect();
  assert!(
    poll_all_hold(&survivors, object),
    "all four surviving candidates hold A's head before A dies (the record replicated to every candidate)"
  );

  // A dies. The survivor rendezvous ranks first for the object takes it over.
  let owner = daemons.remove(0);
  owner.stop();
  let successor =
    rendezvous_first(&hosts[1..], object).expect("a survivor takes over the dead owner's object");
  let successor_index = hosts[1..]
    .iter()
    .position(|host| *host == successor)
    .expect("the successor is one of the survivors");

  // The successor drives phase one over the surviving holders (a quorum of three), adopts the committed head,
  // re-commits it under the new epoch, and reports it region-placed under its own ownership.
  let served = poll_head_placed(&daemons[successor_index], object);
  let held = daemons[successor_index].fleet_holder_head(object);

  for daemon in daemons {
    daemon.stop();
  }
  assert!(
    served,
    "the successor took over the dead owner's head over a multi-holder quorum and served it region-placed"
  );
  let held = held.expect("the successor still holds the taken-over head");
  assert_eq!(
    held.0, successor,
    "the successor is now the object's owner (the takeover reassigned ownership)"
  );
  let head = HeadValue::from_record_bytes(&held.1).expect("a well-formed head value");
  assert_eq!(
    head.name, "taken-over-5",
    "the taken-over head's value survived the multi-holder promotion and re-commit"
  );
}

/// Shape: the bytes a fleet test writes into a volume over NFS and expects back — under the NFS
/// client's 400-byte read, and distinctive.
const CONTENT: &[u8] =
  b"sealed on the owner, replicated to its candidate holders, served after its death\n";
/// Shape: how long to wait for a sealed snapshot's content and head to place across the fleet — the
/// archive walk, the offer/put rounds and the head commit, each a few protocol periods on loopback.
const PLACEMENT_DEADLINE: Duration = Duration::from_secs(20);
/// Shape: how long to wait for a takeover successor to materialize and serve the taken-over content —
/// the takeover, a possible fetch from the recorded holder, and the restore.
const SERVE_DEADLINE: Duration = Duration::from_secs(25);

/// Writes [`CONTENT`] as `hello.txt` into the volume mounted at `/<name>` on `daemon`'s NFS port.
fn write_hello_over_nfs(daemon: &Daemon, name: &str) {
  let port = daemon.nfs_port().expect("the daemon serves NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to the NFS port");
  let root_fh = mount(&mut stream, &format!("/{name}"), 1);
  let file_fh = create(&mut stream, &root_fh, "hello.txt", 2);
  write(&mut stream, &file_fh, CONTENT, 3);
}

/// Reads `hello.txt` back from the volume mounted at `/<name>` on `daemon`'s NFS port.
fn read_hello_over_nfs(daemon: &Daemon, name: &str) -> Vec<u8> {
  let port = daemon.nfs_port().expect("the daemon serves NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to the NFS port");
  let root_fh = mount(&mut stream, &format!("/{name}"), 1);
  let file_fh = lookup(&mut stream, &root_fh, "hello.txt", 2);
  read(&mut stream, &file_fh, 3)
}

/// Provisions `name` on the owner (`daemons[0]`, reached at `instance`), writes [`CONTENT`] into it over
/// NFS, seals it with a snapshot, and waits for the snapshot to place and for every other daemon to hold
/// the head — the state a takeover test needs before the owner dies. Returns the volume id, or why the
/// setup did not complete (the caller stops the daemons and fails).
fn seal_hello_on_owner(instance: &str, daemons: &[Daemon], name: &str) -> Result<VolumeId, String> {
  let mut client = Client::connect(instance);
  let ReplyBody::Created { id } = client.call(&scratch(name)) else {
    return Err("the volume was not created".to_owned());
  };
  write_hello_over_nfs(&daemons[0], name);
  let ReplyBody::Snapshotted { id: snapshot } = client.call(&RequestBody::Snapshot { volume: id })
  else {
    return Err("the snapshot was not taken".to_owned());
  };
  let placed = poll_snapshot_placed(&mut client, id, snapshot);
  let survivors: Vec<&Daemon> = daemons[1..].iter().collect();
  let all_hold = poll_all_hold(&survivors, ObjectId(id.bytes));
  if placed && all_hold {
    Ok(id)
  } else {
    Err(format!(
      "placed={placed}, every survivor holds the head={all_hold}"
    ))
  }
}

/// Polls `status` for `volume` at the daemon reached at `instance` until it answers with a report (the
/// volume is served there) or the serve deadline passes; returns whether it did.
fn poll_status_answers(instance: &str, volume: VolumeId) -> bool {
  let mut client = Client::connect(instance);
  let deadline = Instant::now() + SERVE_DEADLINE;
  while Instant::now() < deadline {
    if matches!(
      client.call(&RequestBody::Status { volume }),
      ReplyBody::Status { .. }
    ) {
      return true;
    }
    std::thread::yield_now();
  }
  false
}

/// Polls the owner's `await placed(snapshot, region)` verb until it answers placed, or the placement
/// deadline passes; returns whether it did.
fn poll_snapshot_placed(client: &mut Client, volume: VolumeId, snapshot: SnapshotId) -> bool {
  let deadline = Instant::now() + PLACEMENT_DEADLINE;
  while Instant::now() < deadline {
    let reply = client.call(&RequestBody::AwaitPlaced {
      volume,
      snapshot: Some(snapshot),
      scope: Scope::Region,
    });
    if matches!(reply, ReplyBody::Placed { placed: true, .. }) {
      return true;
    }
    std::thread::yield_now();
  }
  false
}

/// AC (§4.10 "Content replication"; §4.8 mechanism 1 — "content to `f + 1` … the acknowledging set is
/// written into the object's head record"; AC-8.2 "no head record names content that is not placed"): in
/// a two-node `f = 1` fleet, a file written over NFS into a volume on A and sealed by a snapshot has its
/// **content** replicated to the peer — A archives the snapshot in bounded slices, offers the archive, ships
/// exactly the chunks B lacks, B verifies and holds them whole — and only then does the head naming it
/// commit, so A's `await placed(snapshot, region)` answers placed and B holds the manifest. Non-vacuous: at
/// `f = 1` a sealed snapshot is `Local` (its `await placed` false) until B acknowledges the content and the
/// head places, and B holds nothing until the put reaches it and verifies.
#[test]
fn a_sealed_snapshots_content_replicates_to_the_holder_and_places() {
  let _serial = serialize_fleet_tests();
  let [pa_probe, pa_record, pb_probe, pb_record] = four_free_ports();
  let a = node("a", pa_probe, pa_record);
  let b = node("b", pb_probe, pb_record);
  let pid = std::process::id();
  let instance_a = format!("fleet-{}-{pid}", a.host.0);
  let peer_of_a = Peer {
    host: b.host,
    address: b.address,
    record_address: b.record_address,
    certificate: b.identity.certificate(),
  };
  let peer_of_b = Peer {
    host: a.host,
    address: a.address,
    record_address: a.record_address,
    certificate: a.identity.certificate(),
  };
  let daemon_a = start(a, peer_of_a);
  let daemon_b = start(b, peer_of_b);
  let settle = Instant::now() + FORMATION_SETTLE;
  while Instant::now() < settle {
    std::thread::yield_now();
  }

  let mut client = Client::connect(&instance_a);
  let ReplyBody::Created { id } = client.call(&scratch("sealed")) else {
    daemon_a.stop();
    daemon_b.stop();
    panic!("the volume was not created");
  };
  let object = ObjectId(id.bytes);
  write_hello_over_nfs(&daemon_a, "sealed");
  let ReplyBody::Snapshotted { id: snapshot } = client.call(&RequestBody::Snapshot { volume: id })
  else {
    daemon_a.stop();
    daemon_b.stop();
    panic!("the snapshot was not taken");
  };

  let placed = poll_snapshot_placed(&mut client, id, snapshot);
  let manifest = daemon_a.fleet_head_manifest(object);
  let held = manifest.is_some_and(|manifest| daemon_b.fleet_holder_content(manifest));

  daemon_a.stop();
  daemon_b.stop();
  assert!(
    placed,
    "the sealed snapshot's content and head placed at the f=1 quorum: `await placed(snapshot, region)`"
  );
  assert!(
    manifest.is_some(),
    "the snapshot's manifest identity was recorded once its content placed"
  );
  assert!(
    held,
    "B holds the snapshot's content whole, by the manifest identity the head names"
  );
}

/// AC (§4.8 "Promotion and takeover" — the successor "adopts the newest records, and serves"; §4.10
/// clone-from-archive; R8 one code path): three daemons form an `f = 1` fleet; a file is written over NFS
/// into a volume on A and sealed; its content places (A plus one content candidate) and its head reaches
/// both survivors; A dies; the survivor rendezvous ranks first takes the head over **and serves the
/// content** — it materializes the volume under its original id and mount name from the archive it holds,
/// or fetches the archive by identity from the recorded content holder when it was not the content
/// candidate — and a client mounting `/served` on the successor's NFS port reads the file back byte for
/// byte. Non-vacuous: before the takeover the successor has no such volume (its `status` refuses
/// `NotFound`), and only the content plane can put the bytes there.
#[test]
fn a_takeover_successor_serves_the_dead_owners_content_over_nfs() {
  let _serial = serialize_fleet_tests();
  let names = ["a", "b", "c"];
  let n = names.len();
  let nodes: Vec<(MachineProfile, HostId, Identity)> =
    names.iter().map(|name| fleet_node(name)).collect();
  let hosts: Vec<HostId> = nodes.iter().map(|(_, host, _)| *host).collect();
  let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
    nodes.iter().map(|(_, _, id)| id.certificate()).collect();
  let pid = std::process::id();
  let instance_a = format!("fleet3-{}-{pid}", hosts[0].0);
  let serve = mesh_serve_ports(n);
  let mut daemons = start_mesh(nodes, &hosts, &certs, &serve);
  assert_fleet_forms(&daemons, &hosts, &names);

  // Provision, write and seal on A; wait for the content and head to place and for both survivors to
  // hold the head (the promotion quorum after A dies).
  let sealed = seal_hello_on_owner(&instance_a, &daemons, "served");
  let id = match sealed {
    Ok(id) => id,
    Err(why) => {
      for daemon in daemons {
        daemon.stop();
      }
      panic!("setup: {why}");
    }
  };
  let object = ObjectId(id.bytes);

  // A dies. The first-ranked survivor takes over the head, then serves the content.
  let owner = daemons.remove(0);
  owner.stop();
  let successor = rendezvous_first(&[hosts[1], hosts[2]], object).expect("a survivor takes over");
  let successor_index = if successor == hosts[1] { 0 } else { 1 };
  let head_placed = poll_head_placed(&daemons[successor_index], object);

  // The successor serves the volume once it materialized it: its `status` answers instead of refusing.
  let served = poll_status_answers(&format!("fleet3-{}-{pid}", successor.0), id);
  let got = if served {
    Some(read_hello_over_nfs(&daemons[successor_index], "served"))
  } else {
    None
  };

  for daemon in daemons {
    daemon.stop();
  }
  assert!(head_placed, "the successor took over the dead owner's head");
  assert!(
    served,
    "the successor materialized the taken-over volume and serves it under its id"
  );
  assert_eq!(
    got.as_deref(),
    Some(CONTENT),
    "the file written on the dead owner reads back byte for byte over the successor's NFS port"
  );
}

/// AC (§4.14; banned item 9 — no swallowed error): a fleet peer whose serve socket this node cannot bind at
/// boot is not silently skipped — the refusal is **counted** in the daemon's status (`fleet.bind`), so an
/// operator can see why the mesh never formed to that peer. A's probe serve port is already held by another
/// socket when A boots, so A cannot serve B's probes: A's status must report the refusal. Non-vacuous:
/// without the count, the status showed nothing and the only symptom was a mesh that never formed.
#[test]
fn a_peer_whose_serve_socket_cannot_be_bound_is_counted_not_silently_skipped() {
  let _serial = serialize_fleet_tests();
  let [pa_probe, pa_record, pb_probe, pb_record] = four_free_ports();
  // Hold A's probe serve port before A boots, so A's bind of it fails (released when the test ends).
  let _squatter = std::net::UdpSocket::bind(("127.0.0.1", pa_probe))
    .expect("the port the allocator just released is free to hold");
  let a = node("a", pa_probe, pa_record);
  let b = node("b", pb_probe, pb_record);
  let pid = std::process::id();
  let instance_a = format!("fleet-{}-{pid}", a.host.0);
  let peer_of_a = Peer {
    host: b.host,
    address: b.address,
    record_address: b.record_address,
    certificate: b.identity.certificate(),
  };
  let daemon_a = start(a, peer_of_a);

  // The fleet loop counts the refusal on its first run on the control shard; poll the status for it,
  // bounded, since that run and this client's request are queued on the same shard.
  let mut client = Client::connect(&instance_a);
  let deadline = Instant::now() + Duration::from_secs(5);
  let mut counted = false;
  while Instant::now() < deadline && !counted {
    if let ReplyBody::DaemonStatus { report } = client.call(&RequestBody::DaemonStatus) {
      counted = report
        .shards
        .iter()
        .flat_map(|shard| shard.refusals.iter())
        .any(|refusal| refusal.kind == "fleet.bind" && refusal.count >= 1);
    }
    std::thread::yield_now();
  }
  daemon_a.stop();
  assert!(
    counted,
    "the serve socket A could not bind is counted as a `fleet.bind` refusal in A's status"
  );
}
