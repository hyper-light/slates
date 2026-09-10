#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Two daemons form a **live fleet** (§4.8 "Membership"; §2.6 boot step 6, R5). Each daemon is started
//! with a `FleetTransport` naming the other as its one peer, and its control shard runs the membership
//! loop: it dials the peer's advertised socket, accepts the peer on its own, probes over the transport
//! each protocol period, and folds the acknowledgement into the `FleetNode` the verbs read for placement.
//! The test observes that each daemon comes to see the other alive — the whole boot-step-6 path in the
//! daemon, over real (loopback) UDP sessions with mutual TLS, using `Endpoint::accept` so neither node is
//! told the other's dial address in advance (only its advertised one). Two daemons run concurrently in one
//! process (the runtime's shard ids are process-global, so their shards do not collide); each is given a
//! distinct machine identity so its host id — the fleet member id — is distinct. Real multi-process
//! deployment and the N-node connection-ID demux are further gates.

use std::time::{Duration, Instant};

use rustls::pki_types::PrivateKeyDer;
use slates_db::HostId;
use slates_db::register::Quorum;
use slates_machine::{MachineProfile, ProfileOptions};
use slates_rt::tcp::{Ipv4Addr, SocketAddrV4};
use slates_server::daemon::host_id_of;
use slates_server::{
  Daemon, DaemonConfig, FleetMembership, FleetPeer, FleetTransport, SegmentSource,
};
use slates_transport::handshake::Identity;

/// The TLS server name every fleet node presents (a single fleet's shared name; the certificate pins who).
const NAME: &str = "slates-fleet";
/// Shape: the profile probe budget (milliseconds); an input to derivations, not a gate.
const PROBE_MS: u64 = 5;
/// Shape: how long to let the fleet form — the loops establish their sessions and exchange several probes,
/// each confirming the other alive over the transport — before the peer is killed. Far past the handshake
/// and a few protocol periods on loopback.
const FORMATION_SETTLE: Duration = Duration::from_secs(2);
/// Shape: how long to wait for the survivor to detect and retire the dead peer before the test fails — far
/// past the probe timeout plus the suspicion window.
const RETIREMENT_DEADLINE: Duration = Duration::from_secs(10);

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
fn two_free_ports() -> (u16, u16) {
  let first = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
  let second = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
  (
    first.local_addr().unwrap().port(),
    second.local_addr().unwrap().port(),
  )
}

/// A fleet node's whole setup: its profile (with a distinct identity), its host id, and its fleet TLS
/// identity and advertised address.
struct Node {
  profile: MachineProfile,
  host: HostId,
  identity: Identity,
  address: SocketAddrV4,
}

fn node(name: &str, port: u16) -> Node {
  let profile = profile(name);
  let host = HostId(host_id_of(&profile.facts.identity));
  Node {
    host,
    identity: self_signed(),
    address: SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
    profile,
  }
}

/// Starts the daemon for `this`, configured to join a fleet with `peer` as its one peer.
fn start(
  this: Node,
  peer_host: HostId,
  peer_address: SocketAddrV4,
  peer_cert: rustls::pki_types::CertificateDer<'static>,
) -> Daemon {
  let pid = std::process::id();
  let instance = format!("fleet-{}-{pid}", this.host.0);
  let config = DaemonConfig::derive(&this.profile, &instance)
    .with_shards(1)
    .with_fleet(FleetMembership {
      quorum: Quorum { f: 1 },
      peers: vec![peer_host],
    });
  let transport = FleetTransport {
    identity: this.identity,
    name: NAME.to_owned(),
    bind: this.address,
    peers: vec![FleetPeer {
      host: peer_host,
      address: peer_address,
      certificate: peer_cert,
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
  let (port_a, port_b) = two_free_ports();
  let a = node("a", port_a);
  let b = node("b", port_b);
  assert_ne!(
    a.host, b.host,
    "distinct machine identities give distinct host ids"
  );

  let (host_a, addr_a, cert_a) = (a.host, a.address, a.identity.certificate());
  let (host_b, addr_b, cert_b) = (b.host, b.address, b.identity.certificate());

  let daemon_a = start(a, host_b, addr_b, cert_b);
  let daemon_b = start(b, host_a, addr_a, cert_a);

  // Let the fleet form: the loops establish their sessions and exchange probes over the transport. The
  // test thread is not a runtime task, so it waits by spinning on the clock (as the other daemon tests do
  // — the runtime's `futures::sleep` is unavailable off a shard).
  let settle = Instant::now() + FORMATION_SETTLE;
  while Instant::now() < settle {
    std::hint::spin_loop();
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
    std::hint::spin_loop();
  }

  daemon_a.stop();
  assert!(
    retired,
    "daemon A's membership loop detected B's death over the transport and retired it"
  );
}
