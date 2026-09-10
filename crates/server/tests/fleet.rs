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
use slates_db::register::{ObjectId, Quorum};
use slates_ipc::protocol::{
  Direction, NamePolicy, ReplyBody, RequestBody, SizeClass, pack, unpack,
};
use slates_ipc::{ClientEnd, IpcError, connect};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_rt::tcp::{Ipv4Addr, SocketAddrV4};
use slates_server::daemon::host_id_of;
use slates_server::{
  Daemon, DaemonConfig, FleetMembership, FleetPeer, FleetTransport, SegmentSource,
};
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
    bind: this.address,
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
/// accept and probe each other over the transport (using `Endpoint::accept`, so neither is told the other's
/// dial address in advance) — and when one dies, the survivor **detects it over the transport and retires
/// it**. The retirement is the non-vacuous proof the loop ran end to end: the seeded configuration would
/// hold the peer alive forever, so a peer that transitions from alive to gone did so only because the loop
/// probed it, timed out, aged the suspicion to death, and folded that into the `FleetNode` the verbs read.
#[test]
fn a_daemon_detects_its_dead_peer_over_the_transport_and_retires_it() {
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
          std::hint::spin_loop();
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
        Err(IpcError::RingFull) if started.elapsed() < CREDIT_WAIT => std::hint::spin_loop(),
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
    std::hint::spin_loop();
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
    std::hint::spin_loop();
  }

  daemon_a.stop();
  daemon_b.stop();
  assert!(
    placed,
    "the provisioned head replicated to the peer holder and reached the f=1 quorum"
  );
}
