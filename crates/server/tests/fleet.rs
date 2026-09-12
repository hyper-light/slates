#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Daemons form a **live fleet** (§4.8 "Membership"; §2.6 boot step 6, R5). Each daemon is started with a
//! `FleetTransport` naming its peers, and its control shard runs the membership loop: it dials each peer's
//! advertised socket, accepts each peer on its own per-peer socket, probes over the transport each protocol
//! period, replicates its volume heads to the peer holders, and folds the acknowledgements into the
//! `FleetNode` the verbs read for placement. The tests observe the whole boot-step-6 path in the daemon,
//! over real (loopback) UDP sessions with mutual TLS, using `the demultiplexed serve sockets` so no node is told a peer's
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
use slates_db::register::{ObjectId, Quorum, RegionId, rendezvous_first};
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
/// Shape: how long to wait for a survivor to re-admit a peer that has come back (restarted as itself). Wider
/// than retirement: the returning node must establish, learn of its own death from the survivor's echo,
/// self-refute, and have its refutation adopted — a few protocol periods past a fresh formation.
const REJOIN_DEADLINE: Duration = Duration::from_secs(15);
/// Shape: how long to wait for an N-node fleet to fully form (every node seeing every peer alive) before the
/// test fails. Polled, not a fixed settle, so it returns the instant the mesh is up; the deadline is wide
/// because a larger mesh has more sessions to establish (each node dials and accepts every peer) and the
/// daemons start one after another.
const FORMATION_DEADLINE: Duration = Duration::from_secs(15);
/// Shape: how long to wait, after the mesh forms, for the configuration council to elect a single leader over
/// the transport — several election timeouts (ELECTION_HEARTBEATS heartbeat periods plus per-node jitter, and
/// a retry or two if a jittered collision splits the first vote), measured against the serialised quiet
/// machine.
const COUNCIL_ELECTION_DEADLINE: Duration = Duration::from_secs(15);
/// Shape: the window a single council leader must hold unbroken for the council to count as settled — a
/// couple of dozen heartbeat periods, long enough to tell a converged election from one still churning.
const COUNCIL_STABILITY_WINDOW: Duration = Duration::from_secs(2);
/// Shape: how long to keep looking for that unbroken window before giving up. Wider than the window itself
/// because a loaded test machine can starve a leader's heartbeat and trigger a legitimate re-election (which
/// pre-vote minimizes but cannot forbid when the leader is genuinely unreachable), so the settle is retried
/// across such transients until the council quiesces.
const COUNCIL_SETTLE_DEADLINE: Duration = Duration::from_secs(20);
/// Shape: how long to wait for a follower's council leader-contact to climb past its baseline — a few
/// heartbeat periods, so the leader's replication reaching the followers over the transport is observed live.
const COUNCIL_HEARTBEAT_WINDOW: Duration = Duration::from_secs(5);
/// Shape: how long to wait for the council to commit a membership **retirement** after a member dies — the
/// SWIM death detection (the retirement deadline) plus a few heartbeat periods for the leader to propose the
/// retire and replicate it to a committing majority. Wider than [`RETIREMENT_DEADLINE`] for that commit tail.
const COUNCIL_RETIRE_DEADLINE: Duration = Duration::from_secs(20);

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
  start_sharded(this, peer, 1)
}

/// [`start`] with `shards` shards: a volume then lands on the shard its name routes to, so a fleet test can
/// place a volume on a shard other than the control shard (the one holding the peer sessions) and prove the
/// record plane reaches every owner shard (D-7: one owning shard per volume).
fn start_sharded(this: Node, peer: Peer, shards: u16) -> Daemon {
  let pid = std::process::id();
  let instance = format!("fleet-{}-{pid}", this.host.0);
  let config = DaemonConfig::derive(&this.profile, &instance)
    .with_shards(shards)
    .with_fleet(FleetMembership {
      quorum: Quorum { f: 1 },
      peers: vec![peer.host],
      host: this.host,
      domains: std::collections::BTreeMap::new(),
      regions: std::collections::BTreeMap::new(),
      durability: None,
      region_mirrors: std::collections::BTreeMap::new(),
    });
  let transport = FleetTransport {
    identity: this.identity,
    name: NAME.to_owned(),
    probe_bind: this.address,
    record_bind: this.record_address,
    peers: vec![FleetPeer {
      host: peer.host,
      address: peer.address,
      record_address: peer.record_address,
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
/// accept and probe each other over the transport (using `the demultiplexed serve sockets`, so neither is told the other's
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

/// Shape: the incarnation the test injects a false death at — comfortably above any incarnation a quiet,
/// serialized formation could reach, so the injected death wins over A's current belief about B (a higher
/// incarnation always overrides). Chosen high on purpose; the exact value is immaterial past that.
const FALSE_DEATH_INCARNATION: u64 = 1_000;

/// Polls `condition` (yielding between checks — the test thread is not a runtime task) until it holds or
/// `within` elapses; returns whether it held.
fn poll_until(within: Duration, mut condition: impl FnMut() -> bool) -> bool {
  let deadline = Instant::now() + within;
  while Instant::now() < deadline {
    if condition() {
      return true;
    }
    std::thread::yield_now();
  }
  false
}

/// Polls that `condition` stays true for the whole `window`; returns whether it never broke — the stability
/// check a "did not flap" assertion needs.
fn holds_for(window: Duration, mut condition: impl FnMut() -> bool) -> bool {
  let deadline = Instant::now() + window;
  while Instant::now() < deadline {
    if !condition() {
      return false;
    }
    std::thread::yield_now();
  }
  true
}

/// AC (§4.8, rejoin): a peer the fleet **retired** is **re-admitted when it comes back**, by SWIM
/// refutation, realized to slates' spec — the configuration group is the membership authority, SWIM is only
/// detection, and re-admission needs no separate incarnation tracker or bump because [`Membership::refute`]
/// bumps past the death incarnation it hears. A is made to (falsely) retire B — B is alive throughout, so
/// this drives the pure re-admission path deterministically, without a process kill (a killed daemon's serve
/// socket is leaked to the process lifetime and cannot be rebound in-process, though a real deployment's OS
/// frees it). B keeps probing A; A, believing B dead, echoes that in its acknowledgement
/// ([`serve_peer_probes`]); B self-refutes past the death incarnation and gossips its new life, which A
/// adopts — re-admitting B — after which A's idled [`probe_peer`] resumes. Non-vacuous on three counts: B is
/// shown retired first (the injected death took hold), then shown back, then shown to **stay** back over a
/// further settle, proving the re-admission is stable and not a flap.
#[test]
fn a_falsely_retired_peer_rejoins_by_refutation() {
  let _serial = serialize_fleet_tests();
  let [pa_probe, pa_record, pb_probe, pb_record] = four_free_ports();
  let a = node("a", pa_probe, pa_record);
  let b = node("b", pb_probe, pb_record);
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

  // Let the direct probe mesh form and settle (a fixed wait, not an early return): the seeded membership
  // holds every peer alive from boot, so B must actually be probing A — and their sessions steady — before
  // the injection, so B hears A's echo and refutes over a stable session rather than one mid-formation.
  let settle = Instant::now() + FORMATION_SETTLE;
  while Instant::now() < settle {
    std::thread::yield_now();
  }
  assert!(
    daemon_a.fleet_meshed() && daemon_b.fleet_meshed(),
    "the fleet's direct probe mesh formed"
  );

  // A falsely retires B — a false positive; B is alive and still probing A. This is the same fold A's
  // detector performs when it ages a peer to death.
  daemon_a.observe_peer_dead(host_b, FALSE_DEATH_INCARNATION);
  let retired = poll_until(RETIREMENT_DEADLINE, || {
    !daemon_a.fleet_members().contains(&host_b)
  });

  // B, alive and still probing A, learns of its death from A's echo, refutes, and A re-admits it.
  let rejoined = poll_until(REJOIN_DEADLINE, || {
    daemon_a.fleet_members().contains(&host_b)
  });

  // The re-admission is stable — B does not flap back out over a further settle.
  let stable = rejoined
    && holds_for(FORMATION_SETTLE, || {
      daemon_a.fleet_members().contains(&host_b)
    });

  daemon_a.stop();
  daemon_b.stop();
  assert!(
    retired,
    "A retired B after the injected false death took hold"
  );
  assert!(rejoined, "A re-admitted B after B refuted its false death");
  assert!(
    stable,
    "B stayed admitted after rejoining — the re-admission did not flap"
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

/// One (probe, record) serve port pair per node: `serve[i]` is what node `i` binds and every peer dials.
fn mesh_serve_ports(n: usize) -> Vec<(u16, u16)> {
  let flat = free_ports(2 * n);
  flat.chunks(2).map(|pair| (pair[0], pair[1])).collect()
}

/// Starts one daemon per node: node `i` serves every peer on its own pair `serve[i]` and dials peer `j` at
/// `serve[j]`. Returns the daemons in node order.
/// The fleet's fault tolerance is `f = 1` (a three-node fleet's shape); [`start_mesh_with_f`] takes a larger
/// `f` for a fleet that keeps a quorum through more deaths (2f + 1 nodes).
fn start_mesh(
  nodes: Vec<(MachineProfile, HostId, Identity)>,
  hosts: &[HostId],
  certs: &[rustls::pki_types::CertificateDer<'static>],
  serve: &[(u16, u16)],
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
  serve: &[(u16, u16)],
  f: u32,
) -> Vec<Daemon> {
  start_mesh_with(
    nodes,
    hosts,
    certs,
    serve,
    f,
    1,
    &std::collections::BTreeMap::new(),
    &std::collections::BTreeMap::new(),
  )
}

/// [`start_mesh_with_f`] but assigning each host a **region** (§4.8, D-14), so the root group spans more than
/// one region and its cross-region drive runs over the transport rather than the single-region degenerate.
fn start_mesh_with_regions(
  nodes: Vec<(MachineProfile, HostId, Identity)>,
  hosts: &[HostId],
  certs: &[rustls::pki_types::CertificateDer<'static>],
  serve: &[(u16, u16)],
  f: u32,
  regions: &std::collections::BTreeMap<HostId, slates_db::register::RegionId>,
) -> Vec<Daemon> {
  start_mesh_with(
    nodes,
    hosts,
    certs,
    serve,
    f,
    1,
    regions,
    &std::collections::BTreeMap::new(),
  )
}

/// [`start_mesh_with_regions`] that also declares each region's **mirror** (§4.8 — region-loss promotion), so
/// a lost mirrored region awaits an operator promotion rather than being auto-retired.
fn start_mesh_with_regions_and_mirrors(
  nodes: Vec<(MachineProfile, HostId, Identity)>,
  hosts: &[HostId],
  certs: &[rustls::pki_types::CertificateDer<'static>],
  serve: &[(u16, u16)],
  f: u32,
  regions: &std::collections::BTreeMap<HostId, slates_db::register::RegionId>,
  mirrors: &std::collections::BTreeMap<
    slates_db::register::RegionId,
    slates_db::register::RegionId,
  >,
) -> Vec<Daemon> {
  start_mesh_with(nodes, hosts, certs, serve, f, 1, regions, mirrors)
}

/// [`start_mesh_with_f`] with `shards` shards per daemon (see [`start_sharded`]), each host's `regions`
/// (empty = the single-region default) and each region's `mirrors`. A test-only fixture builder whose many
/// setup inputs are each distinct fleet-shape parameters.
#[allow(clippy::too_many_arguments)]
fn start_mesh_with(
  nodes: Vec<(MachineProfile, HostId, Identity)>,
  hosts: &[HostId],
  certs: &[rustls::pki_types::CertificateDer<'static>],
  serve: &[(u16, u16)],
  f: u32,
  shards: u16,
  regions: &std::collections::BTreeMap<HostId, slates_db::register::RegionId>,
  mirrors: &std::collections::BTreeMap<
    slates_db::register::RegionId,
    slates_db::register::RegionId,
  >,
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
          address: loopback(serve[j].0),
          record_address: loopback(serve[j].1),
          certificate: certs[j].clone(),
        })
        .collect();
      let member_peers: Vec<HostId> = (0..n).filter(|&j| j != i).map(|j| hosts[j]).collect();
      let instance = format!("fleet3-{}-{pid}", host.0);
      let config = DaemonConfig::derive(&profile, &instance)
        .with_shards(shards)
        .with_fleet(FleetMembership {
          quorum: Quorum { f },
          peers: member_peers,
          host,
          domains: std::collections::BTreeMap::new(),
          regions: regions.clone(),
          durability: None,
          region_mirrors: mirrors.clone(),
        });
      let transport = FleetTransport {
        identity,
        name: NAME.to_owned(),
        probe_bind: loopback(serve[i].0),
        record_bind: loopback(serve[i].1),
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
/// `the demultiplexed serve sockets`, and every node reports its own mesh complete ([`Daemon::fleet_meshed`], every
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

/// AC (§4.8, D-14, the distributed configuration council): three daemons' councils, driven from each node's
/// record-plane coordinator over the **real** fleet transport (the council's Raft rides `CONFIG_STREAM` on
/// the same record sessions), **elect a single leader** — the configuration master for the region — and the
/// leader's per-period heartbeats keep the two followers in contact. This is the transport-driven form of
/// the sans-io council proven in `slates-cluster` (`config_group.rs`) and over sim UDP (`config_group_live.rs`);
/// here it runs end to end through the daemon's own sessions and demux. Three nodes at `f = 1` is a majority
/// of two, so the elected leader is genuinely agreed, not a lone self-election.
///
/// The proof is threefold and non-vacuous: **exactly one** leader emerges (an election ran and converged, not
/// zero or a split), the council **settles** on it (a window it holds unbroken — the election converges
/// rather than churning; a loaded machine can still trigger a legitimate re-election, which pre-vote
/// minimizes but cannot forbid when a leader is genuinely starved, so the settle is retried across such
/// transients), and a **follower's leader-contact counter advances** (the leader's heartbeats flow over the
/// transport — replication is live, not merely an election won).
#[test]
fn three_daemons_elect_one_stable_council_leader_over_the_transport() {
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

  // The council elects only over live record sessions, so wait for the direct mesh first (the record links
  // come up alongside the probe mesh), then for exactly one leader to emerge over the transport.
  assert_fleet_forms(&daemons, &hosts, &names);
  let one_leader = || daemons.iter().filter(|d| d.council_leads()).count() == 1;
  let elected = poll_until(COUNCIL_ELECTION_DEADLINE, one_leader);
  // The council settles on a single leader: within the deadline there is a window it holds unbroken. A
  // legitimate re-election under a starved heartbeat is tolerated (pre-vote cannot forbid one when a leader
  // is genuinely unreachable), so the settle is retried until the council quiesces.
  let settled = elected
    && poll_until(COUNCIL_SETTLE_DEADLINE, || {
      holds_for(COUNCIL_STABILITY_WINDOW, one_leader)
    });
  // The leader's heartbeats must reach the followers over the transport: a follower's leader-contact climbs
  // past the baseline just captured (a non-vacuity counter — replication is live, not just an election).
  let baseline: Vec<u64> = daemons.iter().map(Daemon::council_contact).collect();
  let heartbeats_flow = settled
    && poll_until(COUNCIL_HEARTBEAT_WINDOW, || {
      daemons
        .iter()
        .zip(baseline.iter())
        .any(|(daemon, &base)| !daemon.council_leads() && daemon.council_contact() > base)
    });

  // Stop the daemons before asserting, so a failure leaves none running.
  let leads: Vec<bool> = daemons.iter().map(Daemon::council_leads).collect();
  for daemon in daemons {
    daemon.stop();
  }
  assert!(
    elected,
    "the council elected exactly one leader over the transport within the deadline (leads={leads:?})"
  );
  assert!(
    settled,
    "the council settled on a single leader — the election converged, not churned"
  );
  assert!(
    heartbeats_flow,
    "a follower's council leader-contact advanced — the leader's heartbeats flow over the transport"
  );
}

/// AC (§4.8, D-14): the configuration council **commits a membership change over the real transport**, not
/// just an election. Three daemons elect a leader; when a **follower** dies, the leader — which probes every
/// member — detects it via SWIM, proposes the retirement through the council log (`reconcile_alive`), and it
/// commits at the surviving majority (2 of 3 voters) and applies on every survivor, so the dead member drops
/// from each survivor's `RegionalConfiguration`. This is the transport-driven form of the
/// propose→replicate→commit→apply path the sans-io council (`config_group.rs`) and sim-UDP proof
/// (`config_group_live.rs`) show; here the whole path runs through the daemon's own record sessions and demux.
/// A follower is killed, not the leader, so the leader stays and reconciles (killing the leader would first
/// force a re-election — a separate concern); the leader keeping quorum is what lets the retire commit.
#[test]
fn a_council_commits_a_membership_retirement_over_the_transport() {
  let _serial = serialize_fleet_tests();
  let names = ["a", "b", "c"];
  let n = names.len();
  let nodes: Vec<(MachineProfile, HostId, Identity)> =
    names.iter().map(|name| fleet_node(name)).collect();
  let hosts: Vec<HostId> = nodes.iter().map(|(_, host, _)| *host).collect();
  let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
    nodes.iter().map(|(_, _, id)| id.certificate()).collect();
  let serve = mesh_serve_ports(n);
  let mut daemons = start_mesh(nodes, &hosts, &certs, &serve);

  assert_fleet_forms(&daemons, &hosts, &names);
  // Wait for the council to elect one leader, then kill a *follower* so the leader stays and reconciles.
  let elected = poll_until(COUNCIL_ELECTION_DEADLINE, || {
    daemons
      .iter()
      .filter(|daemon| daemon.council_leads())
      .count()
      == 1
  });
  let leader_idx = daemons.iter().position(|daemon| daemon.council_leads());
  let victim_idx = leader_idx.and_then(|lead| (0..daemons.len()).find(|&i| i != lead));

  let retired = match (elected, victim_idx) {
    (true, Some(victim)) => {
      let dead = hosts[victim];
      daemons.remove(victim).stop();
      // The surviving leader detects the death (SWIM), proposes the retire, and commits it over the
      // transport; every survivor then drops the dead member from BOTH its committed regional membership
      // (`council_members`) AND the neighbourhood it actually places under (`placement_neighbourhood`,
      // installed from the council) — the authority switchover: the council's committed configuration is
      // what placement reads, so the commit reaches the placement path, not just the council's own state.
      poll_until(COUNCIL_RETIRE_DEADLINE, || {
        daemons.iter().all(|daemon| {
          !daemon.council_members().contains(&dead)
            && !daemon.placement_neighbourhood().contains(&dead)
        })
      })
    }
    _ => false,
  };

  // Stop the survivors before asserting, so a failure leaves none running.
  for daemon in daemons {
    daemon.stop();
  }
  assert!(elected, "the council elected a leader before the kill");
  assert!(
    retired,
    "the council committed the dead follower's retirement over the transport and it reached placement — \
     every survivor dropped it from both its regional membership and its placement neighbourhood"
  );
}

/// AC (§4.8, D-7 "one owning shard per volume"): the placement verbs (`place`/`region_placed`/`await_placed`/
/// `host_epoch`) run on a volume's **owner** shard, which may not be the control shard that drives the
/// council, so the committed configuration must reach **every** shard. Here each daemon runs two shards; when
/// the council commits a follower's retirement, a **non-control** shard drops the dead member from its
/// placement neighbourhood too — proving the control shard fans its committed configuration out to the others
/// (`sync_config_from_council`'s fan-out). Without it a volume owned on another shard would report a stale
/// placement after the membership change. A follower is retired (the leader stays and reconciles); the death
/// is injected for a deterministic SWIM cue, as the learner test does.
#[test]
fn a_committed_retirement_reaches_every_shards_placement_view() {
  let _serial = serialize_fleet_tests();
  let names = ["a", "b", "c"];
  let n = names.len();
  let nodes: Vec<(MachineProfile, HostId, Identity)> =
    names.iter().map(|name| fleet_node(name)).collect();
  let hosts: Vec<HostId> = nodes.iter().map(|(_, host, _)| *host).collect();
  let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
    nodes.iter().map(|(_, _, id)| id.certificate()).collect();
  let serve = mesh_serve_ports(n);
  let regions = std::collections::BTreeMap::new();
  let mirrors = std::collections::BTreeMap::new();
  // Two shards per daemon: the council runs on the control shard (index 0); shard index 1 is a non-control
  // shard that also owns volumes and answers the placement verbs.
  let mut daemons = start_mesh_with(nodes, &hosts, &certs, &serve, 1, 2, &regions, &mirrors);

  assert_fleet_forms(&daemons, &hosts, &names);
  let elected = poll_until(COUNCIL_ELECTION_DEADLINE, || {
    daemons
      .iter()
      .filter(|daemon| daemon.council_leads())
      .count()
      == 1
  });
  let leader_idx = daemons.iter().position(|daemon| daemon.council_leads());
  let victim_idx = leader_idx.and_then(|lead| (0..daemons.len()).find(|&i| i != lead));

  let (on_control_shard, on_non_control_shard) = match (elected, victim_idx) {
    (true, Some(victim)) => {
      let dead = hosts[victim];
      daemons.remove(victim).stop();
      for daemon in &daemons {
        daemon.observe_peer_dead(dead, FALSE_DEATH_INCARNATION);
      }
      // The leader reconciles the injected death, commits the retirement, and every survivor's control shard
      // drops the dead member from the neighbourhood it places under...
      let on_control = poll_until(COUNCIL_RETIRE_DEADLINE, || {
        daemons
          .iter()
          .all(|daemon| !daemon.placement_neighbourhood().contains(&dead))
      });
      // ...and so does the non-control shard — the fan-out under test.
      let on_non_control = on_control
        && poll_until(COUNCIL_RETIRE_DEADLINE, || {
          daemons
            .iter()
            .all(|daemon| !daemon.placement_neighbourhood_on_shard(1).contains(&dead))
        });
      (on_control, on_non_control)
    }
    _ => (false, false),
  };

  for daemon in daemons {
    daemon.stop();
  }
  assert!(
    elected,
    "the council elected a leader before the retirement"
  );
  assert!(
    on_control_shard,
    "every survivor's control shard dropped the dead member from its placement neighbourhood"
  );
  assert!(
    on_non_control_shard,
    "every survivor's non-control shard dropped it too — the control shard fans its committed configuration to \
     every shard, so placement is consistent on whatever shard owns a volume"
  );
}

/// AC (§4.8, D-14 — the root group across regions, driven over the daemon transport): three daemons, each in
/// its own region and thus its region's representative (so all three are root-group voters), elect one root
/// leader over the transport; when a follower's region is lost (its only host is killed), the surviving root
/// leader detects the death, proposes the region's retirement, and commits it over the transport — every
/// survivor's committed root region membership (`root_regions`) drops the lost region. The whole path runs
/// through the daemon's own record sessions and demultiplexer on [`ROOT_STREAM`], the cross-region counterpart
/// of the regional council's commit path. A follower is killed, not the root leader, so the leader stays and
/// reconciles (killing the leader would force a re-election first — a separate concern).
///
/// The council and the root group are separate consensus planes over the same transport. Here each daemon is
/// its own single-host region purely to exercise the cross-region **drive** with the fewest daemons; a real
/// deployment aligns them (a region's council over its hosts, the root group over region representatives) and
/// declares regions in the manifest — the owed cross-region deployment.
#[test]
fn the_root_group_commits_a_region_retirement_over_the_transport() {
  let _serial = serialize_fleet_tests();
  let names = ["a", "b", "c"];
  let n = names.len();
  let nodes: Vec<(MachineProfile, HostId, Identity)> =
    names.iter().map(|name| fleet_node(name)).collect();
  let hosts: Vec<HostId> = nodes.iter().map(|(_, host, _)| *host).collect();
  let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
    nodes.iter().map(|(_, _, id)| id.certificate()).collect();
  let serve = mesh_serve_ports(n);
  // Each host is its own region, so all three are region representatives (root voters) and the root group
  // spans them; region i is `RegionId(i)`, aligned with the daemon index.
  let regions: std::collections::BTreeMap<HostId, RegionId> = hosts
    .iter()
    .enumerate()
    .map(|(i, &host)| (host, RegionId(u64::try_from(i).unwrap_or(0))))
    .collect();
  let mut daemons = start_mesh_with_regions(nodes, &hosts, &certs, &serve, 1, &regions);

  assert_fleet_forms(&daemons, &hosts, &names);
  // Wait for the root group to elect one leader, then kill a *follower* so the leader stays and reconciles.
  let elected = poll_until(COUNCIL_ELECTION_DEADLINE, || {
    daemons.iter().filter(|daemon| daemon.root_leads()).count() == 1
  });
  let leader_idx = daemons.iter().position(|daemon| daemon.root_leads());
  let victim_idx = leader_idx.and_then(|lead| (0..daemons.len()).find(|&i| i != lead));

  let retired = match (elected, victim_idx) {
    (true, Some(victim)) => {
      let lost = RegionId(u64::try_from(victim).unwrap_or(0));
      daemons.remove(victim).stop();
      // The surviving root leader detects the death (SWIM), sees the lost region has no alive host, proposes
      // its retirement, and commits it over the transport; every survivor drops the region from its committed
      // root membership.
      poll_until(COUNCIL_RETIRE_DEADLINE, || {
        daemons
          .iter()
          .all(|daemon| !daemon.root_regions().contains(&lost))
      })
    }
    _ => false,
  };

  // Stop the survivors before asserting, so a failure leaves none running.
  for daemon in daemons {
    daemon.stop();
  }
  assert!(
    elected,
    "the root group elected a single leader over the transport before the kill"
  );
  assert!(
    retired,
    "the root group committed the lost region's retirement over the transport — every survivor dropped it \
     from its committed root region membership"
  );
}

/// AC (§4.8, D-14 — root learners): a region member that is **not** its region's representative does not vote
/// in the root group; it learns the committed root configuration by **fetching** it from a root voter. Four
/// daemons in three regions — region 0 holds two hosts (its representative, a root voter, and a second member,
/// the **learner**), regions 1 and 2 one host each (both root voters) — so the three representatives form the
/// root group and one region-0 member is a pure learner. When a single-host region is lost (its host killed)
/// the surviving root leader commits its retirement over the transport, and the learner — which cast no vote —
/// drops the region from its committed root membership only by fetching, the cross-region parallel of the
/// regional config learner. A follower voter is killed, not the leader, so the leader stays and reconciles.
#[test]
fn a_root_learner_fetches_the_committed_region_membership_over_the_transport() {
  let _serial = serialize_fleet_tests();
  let names = ["a", "b", "c", "d"];
  let n = names.len();
  let nodes: Vec<(MachineProfile, HostId, Identity)> =
    names.iter().map(|name| fleet_node(name)).collect();
  let hosts: Vec<HostId> = nodes.iter().map(|(_, host, _)| *host).collect();
  let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
    nodes.iter().map(|(_, _, id)| id.certificate()).collect();
  let serve = mesh_serve_ports(n);
  // Regions: hosts 0 and 1 in region 0 (so region 0 has a non-representative member — the learner); host 2 in
  // region 1; host 3 in region 2. Regions 1 and 2 are single-host, so losing either loses a whole region.
  let regions: std::collections::BTreeMap<HostId, RegionId> = [
    (hosts[0], RegionId(0)),
    (hosts[1], RegionId(0)),
    (hosts[2], RegionId(1)),
    (hosts[3], RegionId(2)),
  ]
  .into_iter()
  .collect();
  let daemons = start_mesh_with_regions(nodes, &hosts, &certs, &serve, 1, &regions);

  assert_fleet_forms(&daemons, &hosts, &names);
  // Region 0's representative is its lowest-id host (a root voter); the other region-0 member is the learner.
  let r0_rep = if hosts[0].0 <= hosts[1].0 {
    hosts[0]
  } else {
    hosts[1]
  };
  let learner_host = if r0_rep == hosts[0] {
    hosts[1]
  } else {
    hosts[0]
  };

  let mut survivors: Vec<(HostId, Daemon)> = hosts.iter().copied().zip(daemons).collect();
  // Elect one root leader among the three representatives.
  let elected = poll_until(COUNCIL_ELECTION_DEADLINE, || {
    survivors
      .iter()
      .filter(|(_, daemon)| daemon.root_leads())
      .count()
      == 1
  });
  let leader_host = survivors
    .iter()
    .find(|(_, daemon)| daemon.root_leads())
    .map(|(host, _)| *host);
  // A single-host region's host that is NOT the root leader: kill it so its region is lost while the leader
  // stays and reconciles, and the root group keeps quorum (2 of 3 voters).
  let victim_host = [hosts[2], hosts[3]]
    .into_iter()
    .find(|host| Some(*host) != leader_host);

  let learned = match (elected, victim_host) {
    (true, Some(victim)) => {
      let lost = regions[&victim];
      let victim_pos = survivors
        .iter()
        .position(|(host, _)| *host == victim)
        .expect("the victim is present");
      survivors.remove(victim_pos).1.stop();
      // Inject the victim's death into every survivor for a deterministic SWIM cue (real multi-node detection
      // under load is slow and orthogonal to what this proves — the council learner test injects likewise):
      // the root leader then reconciles the lost region promptly, and the learner's alive view drops it so it
      // fetches. The learning is still the fetch, not the injection.
      for (_, daemon) in &survivors {
        daemon.observe_peer_dead(victim, FALSE_DEATH_INCARNATION);
      }
      let learner_pos = survivors
        .iter()
        .position(|(host, _)| *host == learner_host)
        .expect("the learner survives");
      // The surviving root leader detects the death, commits the lost region's retirement over the transport;
      // the learner (a non-voter) drops it from its committed root membership only by fetching from a voter.
      poll_until(COUNCIL_RETIRE_DEADLINE, || {
        !survivors[learner_pos].1.root_regions().contains(&lost)
      })
    }
    _ => false,
  };

  // The learner never leads the root group — it is not a voter.
  let learner_leads = survivors
    .iter()
    .any(|(host, daemon)| *host == learner_host && daemon.root_leads());

  for (_, daemon) in survivors {
    daemon.stop();
  }
  assert!(
    elected,
    "the root group elected a single leader among the region representatives"
  );
  assert!(
    !learner_leads,
    "the region-0 learner never leads the root group — it is not a voter"
  );
  assert!(
    learned,
    "the learner fetched the committed retirement of the lost region — it dropped from the learner's root \
     membership though it cast no vote"
  );
}

/// AC (§4.8, D-14 — region-loss promotion, at operator cadence): a region with a declared **mirror** is not
/// auto-failed-over when its hosts are lost (promoting a merely-partitioned region would create a second
/// owner — split-brain); it stays in the root membership until an **operator** deliberately promotes it. Three
/// daemons, each its own region and a root voter; regions 1 and 2 both mirror to region 0. A follower's region
/// is lost (its host killed); it is **not** auto-retired (unlike a mirror-less region), and its volumes still
/// route to it. The operator then promotes it on the surviving root leader, which commits `PromoteRegion` over
/// the transport, and every survivor re-homes the lost region's volumes to the mirror
/// (`RootConfiguration::home_of`). The victim's death is injected for a deterministic SWIM cue.
#[test]
fn an_operator_promotes_a_lost_regions_mirror_over_the_transport() {
  let _serial = serialize_fleet_tests();
  let names = ["a", "b", "c"];
  let n = names.len();
  let nodes: Vec<(MachineProfile, HostId, Identity)> =
    names.iter().map(|name| fleet_node(name)).collect();
  let hosts: Vec<HostId> = nodes.iter().map(|(_, host, _)| *host).collect();
  let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
    nodes.iter().map(|(_, _, id)| id.certificate()).collect();
  let serve = mesh_serve_ports(n);
  // Each host is its own region; regions 1 and 2 both mirror to region 0, so whichever follower we lose is a
  // mirrored region that must await an operator promotion rather than being auto-retired.
  let regions: std::collections::BTreeMap<HostId, RegionId> = [
    (hosts[0], RegionId(0)),
    (hosts[1], RegionId(1)),
    (hosts[2], RegionId(2)),
  ]
  .into_iter()
  .collect();
  let mirrors: std::collections::BTreeMap<RegionId, RegionId> =
    [(RegionId(1), RegionId(0)), (RegionId(2), RegionId(0))]
      .into_iter()
      .collect();
  let daemons =
    start_mesh_with_regions_and_mirrors(nodes, &hosts, &certs, &serve, 1, &regions, &mirrors);

  assert_fleet_forms(&daemons, &hosts, &names);
  let mut survivors: Vec<(HostId, Daemon)> = hosts.iter().copied().zip(daemons).collect();
  let elected = poll_until(COUNCIL_ELECTION_DEADLINE, || {
    survivors
      .iter()
      .filter(|(_, daemon)| daemon.root_leads())
      .count()
      == 1
  });
  let leader_host = survivors
    .iter()
    .find(|(_, daemon)| daemon.root_leads())
    .map(|(host, _)| *host);
  // A mirrored region's host (region 1 or 2) that is NOT the root leader, so the leader stays and the root
  // group keeps quorum (2 of 3 voters).
  let victim_host = [hosts[1], hosts[2]]
    .into_iter()
    .find(|host| Some(*host) != leader_host);

  let (not_auto_retired, promoted) = match (elected, victim_host) {
    (true, Some(victim)) => {
      let lost = regions[&victim];
      let mirror = mirrors[&lost];
      let volume = ObjectId::new(victim, 7); // a volume created in the lost region
      let victim_pos = survivors
        .iter()
        .position(|(host, _)| *host == victim)
        .expect("the victim is present");
      survivors.remove(victim_pos).1.stop();
      for (_, daemon) in &survivors {
        daemon.observe_peer_dead(victim, FALSE_DEATH_INCARNATION);
      }
      // The mirrored lost region is NOT auto-retired: it stays a member and its volumes still route to it.
      let not_retired = holds_for(COUNCIL_STABILITY_WINDOW, || {
        survivors
          .iter()
          .all(|(_, daemon)| daemon.region_home(volume, lost) == lost)
      });
      // The operator promotes the lost region's mirror on the surviving root leader, re-issued each poll
      // iteration until it commits and re-homes everywhere: root leadership can flap under load between
      // finding the leader and the commit landing, and `PromoteRegion` is idempotent (home_of follows the
      // committed promotion; a second identical promotion is a no-op once applied).
      let promoted = poll_until(COUNCIL_RETIRE_DEADLINE, || {
        if let Some((_, leader)) = survivors.iter().find(|(_, daemon)| daemon.root_leads()) {
          leader.promote_region(lost);
        }
        survivors
          .iter()
          .all(|(_, daemon)| daemon.region_home(volume, lost) == mirror)
      });
      (not_retired, promoted)
    }
    _ => (false, false),
  };

  for (_, daemon) in survivors {
    daemon.stop();
  }
  assert!(
    elected,
    "the root group elected a leader among the region representatives"
  );
  assert!(
    not_auto_retired,
    "a mirrored lost region is not auto-retired — its volumes still route to it, awaiting an operator promotion"
  );
  assert!(
    promoted,
    "the operator's promotion committed over the transport — every survivor re-homes the lost region's \
     volumes to the mirror"
  );
}

/// AC (§4.8, D-14, §4.12): the operator's region-loss promotion is reachable **over the client protocol** —
/// the `RequestBody::PromoteRegion` the `slates promote-region` verb sends, the surface the design mandates
/// ("the operator issues it (a CLI verb over this)"). A client connected to the root leader promotes a
/// region's declared mirror; the daemon routes the request to its control shard, proposes on the root group,
/// and every node re-homes the region's volumes to the mirror. (`an_operator_promotes...` proves the
/// loss-detection and the promotion via the daemon method; this proves the client → IPC → `serve` →
/// control-shard → root-group path the CLI drives.) All three stay alive — this isolates the client path,
/// not loss detection — so the region is promoted while live (the re-home is what is observed).
#[test]
fn a_client_promotes_a_regions_mirror_over_the_promote_verb() {
  let _serial = serialize_fleet_tests();
  let pid = std::process::id();
  let names = ["a", "b", "c"];
  let n = names.len();
  let nodes: Vec<(MachineProfile, HostId, Identity)> =
    names.iter().map(|name| fleet_node(name)).collect();
  let hosts: Vec<HostId> = nodes.iter().map(|(_, host, _)| *host).collect();
  let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
    nodes.iter().map(|(_, _, id)| id.certificate()).collect();
  let serve = mesh_serve_ports(n);
  let regions: std::collections::BTreeMap<HostId, RegionId> = [
    (hosts[0], RegionId(0)),
    (hosts[1], RegionId(1)),
    (hosts[2], RegionId(2)),
  ]
  .into_iter()
  .collect();
  let mirrors: std::collections::BTreeMap<RegionId, RegionId> =
    [(RegionId(1), RegionId(0)), (RegionId(2), RegionId(0))]
      .into_iter()
      .collect();
  let daemons =
    start_mesh_with_regions_and_mirrors(nodes, &hosts, &certs, &serve, 1, &regions, &mirrors);

  assert_fleet_forms(&daemons, &hosts, &names);
  let daemons: Vec<(HostId, Daemon)> = hosts.iter().copied().zip(daemons).collect();
  let elected = poll_until(COUNCIL_ELECTION_DEADLINE, || {
    daemons
      .iter()
      .filter(|(_, daemon)| daemon.root_leads())
      .count()
      == 1
  });
  let lost = RegionId(1);
  let mirror = RegionId(0);
  let volume = ObjectId::new(hosts[1], 7); // a volume created in region 1

  // Send `PromoteRegion` over a client to the **current** root leader, re-finding it each poll iteration and
  // reconnecting if it changed: root leadership can flap under load, and only the leader can propose, so a
  // client pinned to an ex-leader would be refused `NotRootLeader` forever. `PromoteRegion` is idempotent, so
  // re-sending is safe. This mirrors how the daemon-side test re-issues on the current leader.
  let mut leader_client: Option<(HostId, Client)> = None;
  let promoted = elected
    && poll_until(COUNCIL_RETIRE_DEADLINE, || {
      if let Some((host, _)) = daemons.iter().find(|(_, daemon)| daemon.root_leads()) {
        let host = *host;
        if leader_client.as_ref().map(|(held, _)| *held) != Some(host) {
          leader_client = Some((host, Client::connect(&format!("fleet3-{}-{pid}", host.0))));
        }
        if let Some((_, client)) = leader_client.as_mut() {
          let _ = client.call(&RequestBody::PromoteRegion { region: lost.0 });
        }
      }
      daemons
        .iter()
        .all(|(_, daemon)| daemon.region_home(volume, lost) == mirror)
    });

  for (_, daemon) in daemons {
    daemon.stop();
  }
  assert!(elected, "the root group elected a single leader");
  assert!(
    promoted,
    "a client's PromoteRegion committed on the root leader — every node re-homed the region's volumes to the \
     mirror, so the operator's CLI promotion reaches the root group over the client protocol"
  );
}

/// AC (§4.8 "Lookup", D-14): the cross-region lookup guard (`verbs::home_redirect`) runs on whatever shard a
/// client's request lands on, so the committed root configuration must reach **every** shard of a multi-shard
/// daemon — not only the control shard that drives the root group over the transport. Here each daemon runs
/// two shards; after an operator promotion commits, a **non-control** shard re-homes the promoted region's
/// volumes to the mirror too, proving the control shard fans its committed root configuration out to the
/// others (`sync_root_to_shards`). Without that fan-out a client landing on another shard would read the
/// pre-promotion home. (The lost-region / not-auto-retired semantics are covered by
/// `an_operator_promotes_a_lost_regions_mirror_over_the_transport`; this isolates the multi-shard consistency,
/// so it promotes a region without killing its host — all three stay alive as root voters.)
#[test]
fn a_committed_promotion_reaches_every_shards_lookup_view() {
  let _serial = serialize_fleet_tests();
  let names = ["a", "b", "c"];
  let n = names.len();
  let nodes: Vec<(MachineProfile, HostId, Identity)> =
    names.iter().map(|name| fleet_node(name)).collect();
  let hosts: Vec<HostId> = nodes.iter().map(|(_, host, _)| *host).collect();
  let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
    nodes.iter().map(|(_, _, id)| id.certificate()).collect();
  let serve = mesh_serve_ports(n);
  let regions: std::collections::BTreeMap<HostId, RegionId> = [
    (hosts[0], RegionId(0)),
    (hosts[1], RegionId(1)),
    (hosts[2], RegionId(2)),
  ]
  .into_iter()
  .collect();
  let mirrors: std::collections::BTreeMap<RegionId, RegionId> =
    [(RegionId(1), RegionId(0)), (RegionId(2), RegionId(0))]
      .into_iter()
      .collect();
  // Two shards per daemon: the record plane (and the root group) runs on the control shard (index 0); shard
  // index 1 is a non-control shard that also serves clients and answers the lookup guard.
  let daemons = start_mesh_with(nodes, &hosts, &certs, &serve, 1, 2, &regions, &mirrors);

  assert_fleet_forms(&daemons, &hosts, &names);
  let daemons: Vec<(HostId, Daemon)> = hosts.iter().copied().zip(daemons).collect();
  let elected = poll_until(COUNCIL_ELECTION_DEADLINE, || {
    daemons
      .iter()
      .filter(|(_, daemon)| daemon.root_leads())
      .count()
      == 1
  });

  let lost = RegionId(1);
  let mirror = RegionId(0);
  let volume = ObjectId::new(hosts[1], 7); // a volume created in region 1

  // Promote on the current root leader, re-issued each poll iteration until it takes and propagates. Under
  // suite-end load the root leadership can briefly flap — or a propose can reach a leader that then loses
  // leadership before it commits — and `PromoteRegion` is idempotent (home_of follows the committed promotion;
  // a second identical promotion is a no-op once applied), so retrying through a transient gap is safe. The
  // region must re-home to the mirror on BOTH the control shard (`region_home`) and a non-control shard
  // (`region_home_on_shard` — the fan-out under test), so a client reads the same home wherever it lands.
  let promoted = elected
    && poll_until(COUNCIL_RETIRE_DEADLINE, || {
      if let Some((_, leader)) = daemons.iter().find(|(_, daemon)| daemon.root_leads()) {
        leader.promote_region(lost);
      }
      daemons.iter().all(|(_, daemon)| {
        daemon.region_home(volume, lost) == mirror
          && daemon.region_home_on_shard(1, volume, lost) == mirror
      })
    });

  for (_, daemon) in daemons {
    daemon.stop();
  }
  assert!(
    elected,
    "the root group elected a single leader among the region representatives"
  );
  assert!(
    promoted,
    "the operator's promotion committed and re-homed the region's volumes to the mirror on every shard — \
     control and non-control — so the cross-region lookup guard is consistent wherever a client lands"
  );
}

/// AC (§4.8, D-14, learners): the council votes with a **small** set — the members up to the candidate floor
/// `2f + 1` — and the rest are **learners** that do not vote but fetch the committed configuration over the
/// transport. Five members at `f = 1` gives three voters and two learners. A learner never leads; and when
/// the council commits a change (here a member's retirement), the learner **learns it by fetching** — its
/// regional membership and placement neighbourhood drop the retired member — even though it cast no vote.
/// This is the design's config-learning path that keeps the consensus group small while the region is large.
#[test]
fn a_learner_fetches_the_councils_committed_configuration_over_the_transport() {
  let _serial = serialize_fleet_tests();
  let names = ["a", "b", "c", "d", "e"];
  let n = names.len();
  let nodes: Vec<(MachineProfile, HostId, Identity)> =
    names.iter().map(|name| fleet_node(name)).collect();
  let hosts: Vec<HostId> = nodes.iter().map(|(_, host, _)| *host).collect();
  let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
    nodes.iter().map(|(_, _, id)| id.certificate()).collect();
  let serve = mesh_serve_ports(n);
  let daemons = start_mesh_with_f(nodes, &hosts, &certs, &serve, 1);

  assert_fleet_forms(&daemons, &hosts, &names);
  // The council votes with the three lowest-id members (the candidate floor 2f+1 = 3); the other two are
  // learners (hosts not among the three lowest ids).
  let mut by_id = hosts.clone();
  by_id.sort_by_key(|host| host.0);
  let voters: Vec<HostId> = by_id.iter().take(3).copied().collect();
  let learner_hosts: Vec<HostId> = hosts
    .iter()
    .copied()
    .filter(|h| !voters.contains(h))
    .collect();
  let observed_host = learner_hosts[0];
  let dead = learner_hosts[1];
  let observed_index = hosts
    .iter()
    .position(|h| *h == observed_host)
    .expect("observed learner");

  // A learner never leads the council (it is not a voter) — hold that across a window.
  let learner_never_leads = holds_for(COUNCIL_STABILITY_WINDOW, || {
    !daemons[observed_index].council_leads()
  });

  // Kill the other learner, and inject its death into every survivor. (Real SWIM detection of a killed node
  // under this 5-node load is slow and orthogonal to what this proves; the rejoin test injects deaths for the
  // same reason.) The voters commit its retirement over the transport. The observed learner casts no vote in
  // that commit; it learns the retirement **reactively** (§4.8 the piggyback rule): its own SWIM now sees the
  // dead member, so its membership diverges from its installed configuration and it **fetches** the committed
  // configuration from a voter — its regional membership and placement neighbourhood then drop the dead member,
  // though it took no part in the consensus. The injection is only the deterministic SWIM cue; the learning is
  // the fetch, and an idle learner whose view still matched its configuration would have sent nothing.
  let mut survivors: Vec<(HostId, Daemon)> = hosts.iter().copied().zip(daemons).collect();
  let dead_pos = survivors
    .iter()
    .position(|(host, _)| *host == dead)
    .expect("the killed learner is present");
  survivors.remove(dead_pos).1.stop();
  for (_, daemon) in &survivors {
    daemon.observe_peer_dead(dead, FALSE_DEATH_INCARNATION);
  }
  let observed_pos = survivors
    .iter()
    .position(|(host, _)| *host == observed_host)
    .expect("the observed learner survives");
  let learned = poll_until(COUNCIL_RETIRE_DEADLINE, || {
    let daemon = &survivors[observed_pos].1;
    !daemon.council_members().contains(&dead) && !daemon.placement_neighbourhood().contains(&dead)
  });

  for (_, daemon) in survivors {
    daemon.stop();
  }
  assert!(
    learner_never_leads,
    "the learner {observed_host:?} never leads the council — it is a non-voter"
  );
  assert!(
    learned,
    "the learner {observed_host:?} fetched the council's committed retirement of {dead:?} — its regional \
     membership and placement neighbourhood dropped the dead member, though it cast no vote"
  );
}

/// AC (§4.8, boot step 6, N-node): **three** daemons form one live fleet over the per-peer socket mesh —
/// each node serves each of its two peers on its own advertised socket pair (since `the demultiplexed serve sockets` pins
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
  let successor_instance = format!("fleet3-{}-{pid}", successor.0);
  let served = poll_status_answers(&successor_instance, id);
  let got = if served {
    Some(read_hello_over_nfs(&daemons[successor_index], "served"))
  } else {
    None
  };
  // The successor goes on writing the object: a further seal on it places over the remaining holder.
  let resealed =
    served && reseal_places(&successor_instance, &daemons[successor_index], "served", id);

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
  assert!(
    resealed,
    "a seal taken on the successor after the takeover places — its head written at the promotion \
     epoch the holders fenced the object at, not the successor's lower host epoch"
  );
}

/// Writes a further file into `name` on `daemon` over NFS and seals it, then polls the snapshot's
/// `await placed(region)` on `instance` until it places or the deadline passes. After a takeover this
/// is the proof the successor keeps **writing** the object: its holders fenced the object at the
/// promotion epoch, so a head written at the successor's lower host epoch would be refused `StaleEpoch`
/// and never place.
fn reseal_places(instance: &str, daemon: &Daemon, name: &str, volume: VolumeId) -> bool {
  let port = daemon.nfs_port().expect("the daemon serves NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to the NFS port");
  let root_fh = mount(&mut stream, &format!("/{name}"), 1);
  let file_fh = create(&mut stream, &root_fh, "again.txt", 2);
  write(&mut stream, &file_fh, CONTENT, 3);
  drop(stream);
  let mut client = Client::connect(instance);
  let ReplyBody::Snapshotted { id: snapshot } = client.call(&RequestBody::Snapshot { volume })
  else {
    return false;
  };
  poll_snapshot_placed(&mut client, volume, snapshot)
}

/// Shape: the number of shards the multi-shard fleet tests run — two, the smallest count with a shard other
/// than the control shard.
const TWO_SHARDS: u16 = 2;
/// Shape: the partition the multi-shard tests place their volume on — the one that is not the control
/// shard (partition 0), so the record plane must reach it across shards.
const OTHER_PARTITION: u16 = 1;

/// The mount name whose owner partition (`verbs::owner_of_name`) is `partition` among `partitions` — the
/// first of `prefix-0`, `prefix-1`, … that routes there — so a test places a volume on a chosen shard
/// (a create routes by name, and the id it mints encodes that partition).
fn name_on_partition(prefix: &str, partition: u16, partitions: usize) -> String {
  (0..256u32)
    .map(|attempt| format!("{prefix}-{attempt}"))
    .find(|name| slates_server::verbs::owner_of_name(name, partitions) == partition)
    .expect("some name routes to the partition")
}

/// AC (D-7 "one owning shard per volume"; §4.10; R8): the record plane serves **every** owner shard, not
/// only the control shard that holds the peer sessions. In a two-node `f = 1` fleet of two-shard daemons a
/// volume is placed on the shard that is not the control shard (its name routes there, its id encodes the
/// partition); a file written over NFS (the cross-shard bridge queue) and sealed has its content
/// replicated to the peer and its snapshot placed — the seal walked and recorded on the owner shard, the
/// archive and head moved to the control shard's coordinator by value. Non-vacuous: the volume's partition
/// is asserted not to be the control shard's, and before this the control-shard-only loop never saw it.
#[test]
fn a_volume_on_a_non_control_shard_replicates_its_content_and_places() {
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
  let daemon_a = start_sharded(a, peer_of_a, TWO_SHARDS);
  let daemon_b = start_sharded(b, peer_of_b, TWO_SHARDS);
  let settle = Instant::now() + FORMATION_SETTLE;
  while Instant::now() < settle {
    std::thread::yield_now();
  }

  let name = name_on_partition("sealed2", OTHER_PARTITION, usize::from(TWO_SHARDS));
  let mut client = Client::connect(&instance_a);
  let ReplyBody::Created { id } = client.call(&scratch(&name)) else {
    daemon_a.stop();
    daemon_b.stop();
    panic!("the volume was not created");
  };
  let on_other_shard = slates_server::verbs::owner_of(id) == OTHER_PARTITION;
  let object = ObjectId(id.bytes);
  write_hello_over_nfs(&daemon_a, &name);
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
    on_other_shard,
    "the volume lives on the shard that is not the control shard"
  );
  assert!(
    placed,
    "a snapshot of a volume on another shard placed at the f=1 quorum: `await placed(snapshot, region)`"
  );
  assert!(held, "B holds the snapshot's content whole");
}

/// AC (D-7; §4.8 "Promotion and takeover" → serve; §4.10): a takeover successor materializes a dead
/// owner's volume **on the shard its id routes to**, so every verb for it finds it: three two-shard daemons,
/// the volume on the owner's non-control shard, written over NFS and sealed; the owner dies; the successor's
/// `status` for the id — routed by the id's partition to *its* non-control shard — answers, and the file
/// reads back byte for byte over the successor's NFS port. Non-vacuous: materialized on the control shard
/// (where the holds and the fetched archive live) the id would route to a shard with no such volume.
#[test]
fn a_takeover_successor_serves_a_volume_on_a_non_control_shard() {
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
  let mut daemons = start_mesh_with(
    nodes,
    &hosts,
    &certs,
    &serve,
    1,
    TWO_SHARDS,
    &std::collections::BTreeMap::new(),
    &std::collections::BTreeMap::new(),
  );
  assert_fleet_forms(&daemons, &hosts, &names);

  let name = name_on_partition("served2", OTHER_PARTITION, usize::from(TWO_SHARDS));
  let sealed = seal_hello_on_owner(&instance_a, &daemons, &name);
  let id = match sealed {
    Ok(id) => id,
    Err(why) => {
      for daemon in daemons {
        daemon.stop();
      }
      panic!("setup: {why}");
    }
  };
  let on_other_shard = slates_server::verbs::owner_of(id) == OTHER_PARTITION;
  let object = ObjectId(id.bytes);

  let owner = daemons.remove(0);
  owner.stop();
  let successor = rendezvous_first(&[hosts[1], hosts[2]], object).expect("a survivor takes over");
  let successor_index = if successor == hosts[1] { 0 } else { 1 };
  let head_placed = poll_head_placed(&daemons[successor_index], object);
  let served = poll_status_answers(&format!("fleet3-{}-{pid}", successor.0), id);
  let got = if served {
    Some(read_hello_over_nfs(&daemons[successor_index], &name))
  } else {
    None
  };

  for daemon in daemons {
    daemon.stop();
  }
  assert!(on_other_shard, "the volume lives on the non-control shard");
  assert!(head_placed, "the successor took over the dead owner's head");
  assert!(
    served,
    "the successor serves the taken-over volume on the shard its id routes to"
  );
  assert_eq!(
    got.as_deref(),
    Some(CONTENT),
    "the file reads back byte for byte over the successor's NFS port"
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
