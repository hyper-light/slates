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
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// These integration tests drive the daemon's NFS-loopback transport, the fleet's TCP
// transport and rustix syscalls — all macOS/Linux; on Windows the daemon mounts through WinFsp and
// the fleet transport is QUIC-over-UDP, so these particular tests are unix (as `virtiofs.rs` is).
#![cfg(unix)]

use std::time::{Duration, Instant};

use rustls::pki_types::PrivateKeyDer;
use slates_anchor::{AnchorSegment, Geometry};
use slates_db::HostId;
use slates_db::register::{DomainId, ObjectId, Quorum, RegionId, candidates_for, rendezvous_first};
use slates_ipc::protocol::{
  Direction, NamePolicy, Refusal, ReplyBody, RequestBody, Scope, SizeClass, SnapshotId, VolumeId,
  pack, unpack,
};
use slates_ipc::{ClientEnd, IpcError, connect};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_rt::tcp::{Ipv4Addr, SocketAddrV4};
use slates_server::daemon::{HEARTBEAT_NS, LIVENESS_BUDGET_NS, host_id_of};
use slates_server::deploy::member_id;
use slates_server::head::HeadValue;
use slates_server::observe::ObserveError;
use slates_server::{
  Daemon, DaemonConfig, DurabilityBound, FleetMembership, FleetPeer, FleetTransport, SegmentSource,
};

mod common;
use std::net::TcpStream;

use common::nfs::{create, lookup, mount, owner_and_mode, read, write};
use common::trace;
use common::wait::{ProgressCharge, Verdict, verdict};
use slates_server::fleet::{FLEET_FRAME_CAP, POLL_PER_PERIOD};
use slates_transport::endpoint::{Endpoint, EndpointError};
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
/// past the probe timeout plus the suspicion window, with generous headroom so a transient CPU-load spike or a
/// loaded shared-tenant machine (which stretches the real-timer detection wall-clock) does not flake it. The
/// fleet tests are serialised (see [`serialize_fleet_tests`]); the deadline is not tight even so.
const RETIREMENT_DEADLINE: Duration = Duration::from_secs(30);
/// Shape: how long to wait for a survivor to re-admit a peer that has come back (restarted as itself). Wider
/// than retirement: the returning node must establish, learn of its own death from the survivor's echo,
/// self-refute, and have its refutation adopted — a few protocol periods past a fresh formation.
const REJOIN_DEADLINE: Duration = Duration::from_secs(30);
/// Shape: how long to wait for an N-node fleet to fully form (every node seeing every peer alive) before the
/// test fails. Polled, not a fixed settle, so it returns the instant the mesh is up; the deadline is wide
/// because a larger mesh has more sessions to establish (each node dials and accepts every peer) and the
/// daemons start one after another.
const FORMATION_DEADLINE: Duration = Duration::from_secs(30);
/// Shape: how long to wait, after the mesh forms, for the configuration council to elect a single leader over
/// the transport — several election timeouts (ELECTION_HEARTBEATS heartbeat periods plus per-node jitter, and
/// a retry or two if a jittered collision splits the first vote), measured against the serialised quiet
/// machine.
const COUNCIL_ELECTION_DEADLINE: Duration = Duration::from_secs(30);
/// Shape: the window a single council leader must hold unbroken for the council to count as settled — a
/// couple of dozen heartbeat periods, long enough to tell a converged election from one still churning.
const COUNCIL_STABILITY_WINDOW: Duration = Duration::from_secs(2);
/// Shape: how long to keep looking for that unbroken window before giving up. Wider than the window itself
/// because a loaded test machine can starve a leader's heartbeat and trigger a legitimate re-election (which
/// pre-vote minimizes but cannot forbid when the leader is genuinely unreachable), so the settle is retried
/// across such transients until the council quiesces.
const COUNCIL_SETTLE_DEADLINE: Duration = Duration::from_secs(40);
/// Shape: how long to wait for a follower's council leader-contact to climb past its baseline — a few
/// heartbeat periods, so the leader's replication reaching the followers over the transport is observed live.
const COUNCIL_HEARTBEAT_WINDOW: Duration = Duration::from_secs(5);
/// Shape: how long to wait for the council to commit a membership **retirement** after a member dies — the
/// SWIM death detection (the retirement deadline) plus a few heartbeat periods for the leader to propose the
/// retire and replicate it to a committing majority. Wider than [`RETIREMENT_DEADLINE`] for that commit tail.
const COUNCIL_RETIRE_DEADLINE: Duration = Duration::from_secs(60);
/// Shape: the budget of fleet-coordinator **periods** [`poll_until`] gives the slowest observed daemon to
/// satisfy its condition before the operation is judged genuinely non-convergent. The wait is charged in the
/// daemon's own periods ([`slates_server::Daemon::fleet_progress`]), not wall-clock — that is what makes it
/// robust to noisy, heavy CPU load: fleet convergence is **period-driven** (a bounded number of probe/commit
/// rounds), so it completes in about the same number of periods however slowly those periods run under load,
/// whereas a fixed wall-clock deadline breaks the moment load stretches those periods out. This is many times
/// the periods any fleet operation needs — formation, election, retirement and takeover each converge in tens
/// of periods — so a correct operation never reaches it however starved, and only a genuine non-convergence
/// does. Gating on the **minimum** progress across the observed daemons (not a sum) is what keeps a single
/// starved daemon holding the wait open rather than being masked by peers that keep ticking. Sized well past
/// the periods any operation needs even with leadership flap under heavy load — measured: the operations that
/// converge do so in far fewer, and only a genuine non-convergence reaches this.
const PERIOD_BUDGET: u64 = 4000;
/// Shape: the wall-clock window of **zero** fleet-coordinator progress after which [`poll_until`] calls the
/// fleet stalled (frozen or dead) rather than merely slow. This is the sole wall-clock bound in the wait and
/// it fires only when the min observed `fleet_progress` does not advance at all for this long — i.e. every
/// observed coordinator got no CPU for the whole window. It is generous on purpose: under heavy CPU load a
/// live coordinator can be starved of the scheduler for tens of seconds while still being perfectly alive, so
/// a tight window (the per-operation deadline) false-declares it dead; only a truly wedged or dead coordinator
/// makes no progress at all for this many minutes.
const FROZEN_CAP: Duration = Duration::from_secs(300);
/// Shape: how often a wait in progress is sampled into the opt-in trace ([`trace`]) — once a second of wall
/// time, so a stall leaves a bounded, legible record (a wait frozen for [`FROZEN_CAP`] is three hundred
/// lines) and a healthy wait a handful; a change of verdict is always recorded at once.
const TRACE_SAMPLE: Duration = Duration::from_secs(1);
/// Shape: the pace a wait keeps after an ask its daemon could not answer (a shard starved past the observe
/// budget): a tenth of a coordinator period, the tree's collection-loop cadence
/// ([`slates_server::fleet::POLL_PER_PERIOD`]), so a refused ask is not re-submitted in a tight loop against
/// the very shard it starves. The ask itself already waited the observe budget; this only spaces the next.
const UNAVAILABLE_PACE: Duration = Duration::from_nanos(HEARTBEAT_NS / POLL_PER_PERIOD);

/// The trace of one wait ([`trace`], on only when `SLATES_FLEET_TRACE` names a file): its site (the file
/// and line that called the wait, so no call site needs a label), when it began and last sampled, how
/// many times the condition was asked since, the slowest ask it saw, and how many asks the daemons could
/// not answer (with the last such refusal) — so a stall names the wait, what the observed coordinators
/// and shards did meanwhile, and whether the condition itself was the slow part (an observation reaching
/// its budget on a starved shard shows as an ask of about that budget, and as an unavailable ask).
struct WaitTrace {
  site: &'static std::panic::Location<'static>,
  began: Instant,
  last_sample: Instant,
  asks_since_sample: u64,
  slowest_ask: Duration,
  unavailable: u64,
  last_unavailable: Option<String>,
}

impl WaitTrace {
  /// Opens the trace of a `kind` of wait at `site`, recording the observed daemons' state as it begins.
  fn begin(
    kind: &str,
    site: &'static std::panic::Location<'static>,
    daemons: &[&Daemon],
  ) -> WaitTrace {
    if trace::enabled() {
      trace::record(format_args!(
        "{kind} begin {} {}",
        site_of(site),
        describe(daemons)
      ));
    }
    let now = Instant::now();
    WaitTrace {
      site,
      began: now,
      last_sample: now,
      asks_since_sample: 0,
      slowest_ask: Duration::ZERO,
      unavailable: 0,
      last_unavailable: None,
    }
  }

  /// One more ask the daemons could not answer, and why: counted for the sample lines and the end line.
  fn unavailable(&mut self, refusal: &ObserveError) {
    self.unavailable += 1;
    self.last_unavailable = Some(refusal.to_string());
  }

  /// One more ask of the condition that `took` this long and did not hold; samples a line once
  /// [`TRACE_SAMPLE`] has passed since the last, with the wait's daemon-time position (each observed
  /// coordinator's periods since the wait began, the least of them — what the wait is charged — and how
  /// long that least advance has been frozen).
  fn asked(&mut self, took: Duration, daemons: &[&Daemon], advances: &[u64], frozen_for: Duration) {
    if !trace::enabled() {
      return;
    }
    self.asks_since_sample += 1;
    self.slowest_ask = self.slowest_ask.max(took);
    if self.last_sample.elapsed() < TRACE_SAMPLE {
      return;
    }
    trace::record(format_args!(
      "poll {} wall={:.3}s advances={advances:?} charged={} frozen_for={:.3}s asks={} \
       slowest_ask={:.3}s unavailable={} last_unavailable={:?} {}",
      site_of(self.site),
      self.began.elapsed().as_secs_f64(),
      advances.iter().min().copied().unwrap_or(0),
      frozen_for.as_secs_f64(),
      self.asks_since_sample,
      self.slowest_ask.as_secs_f64(),
      self.unavailable,
      self.last_unavailable,
      describe(daemons)
    ));
    self.last_sample = Instant::now();
    self.asks_since_sample = 0;
    self.slowest_ask = Duration::ZERO;
  }

  /// Closes the trace with the wait's `verdict` and the daemons' state at its end.
  fn end(&self, kind: &str, verdict: &str, daemons: &[&Daemon], advances: &[u64]) {
    if trace::enabled() {
      trace::record(format_args!(
        "{kind} end {} verdict={verdict} wall={:.3}s advances={advances:?} unavailable={} \
         last_unavailable={:?} {}",
        site_of(self.site),
        self.began.elapsed().as_secs_f64(),
        self.unavailable,
        self.last_unavailable,
        describe(daemons)
      ));
    }
  }
}

/// A wait's site as `file:line` (the basename only), from the caller location a `#[track_caller]` wait
/// captures — the label of the wait in the trace, needing no change at any call site.
fn site_of(site: &std::panic::Location<'_>) -> String {
  let file = site.file().rsplit('/').next().unwrap_or(site.file());
  format!("{file}:{}", site.line())
}

/// Every observed daemon for a trace line: its instance, its coordinator's period count
/// ([`Daemon::fleet_progress`]) and each of its shards' pulse ([`Daemon::shard_pulses`]) — both read
/// directly, so a daemon too starved to answer a query is still described.
fn describe(daemons: &[&Daemon]) -> String {
  daemons
    .iter()
    .map(|daemon| {
      let shards: Vec<String> = daemon
        .shard_pulses()
        .iter()
        .map(|pulse| {
          format!(
            "{}:steps={} waits={} spawns={} done={} adm_refused={} longest_step_ms={} parked={} \
             kicks_skipped={} ring_full={} overrun_ms={}",
            pulse.shard,
            pulse.steps,
            pulse.waits,
            pulse.spawns,
            pulse.completed,
            pulse.admission_refused,
            pulse.longest_step_ns / 1_000_000,
            pulse.parked,
            pulse.kicks_skipped,
            pulse.ring_full_events,
            pulse.scheduler_overrun_ns / 1_000_000
          )
        })
        .collect();
      format!(
        "{}[periods={} {}]",
        daemon.config().instance,
        daemon.fleet_progress(),
        shards.join(" ")
      )
    })
    .collect::<Vec<_>>()
    .join(" ")
}

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
  /// This node's manifest routing placeholder (`member_id(origin_anchor, 0)`).
  /// Tests observe the fresh voting identity from the started daemon.
  host: HostId,
  /// This node's stable anchor (`HostId(host_id_of(machine identity))`) — the daemon derives its runtime
  /// member id `member_id(origin_anchor, boot_nonce)` from it and keys completion records on it.
  origin_anchor: HostId,
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
  // The machine identity supplies the stable anchor; nonce zero names its routing placeholder.
  let origin_anchor = HostId(host_id_of(&profile.facts.identity));
  let host = member_id(origin_anchor, 0);
  Node {
    host,
    origin_anchor,
    identity: self_signed(),
    address: SocketAddrV4::new(Ipv4Addr::LOCALHOST, probe_port),
    record_address: SocketAddrV4::new(Ipv4Addr::LOCALHOST, record_port),
    profile,
  }
}

/// The addresses and certificate of a node's one peer.
#[derive(Clone)]
struct Peer {
  /// The peer's stable anchor (its machine-identity hash on an in-process fleet), from which every member id
  /// it announces derives (task #22).
  anchor: HostId,
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
  start_with_policy(this, peer, shards, None)
}

/// [`start_sharded`] under the operator's `durability` policy (§4.8 "Placement"): the accepted coincident-loss
/// probability and the failure count it is stated under, which gate the fleet's writes.
fn start_with_policy(
  this: Node,
  peer: Peer,
  shards: u16,
  durability: Option<DurabilityBound>,
) -> Daemon {
  let pid = std::process::id();
  let instance = format!("fleet-{}-{pid}", this.host.0);
  let config =
    DaemonConfig::derive(&this.profile, &instance, Some(shards)).with_fleet(FleetMembership {
      quorum: Quorum { f: 1 },
      peers: vec![peer.host],
      host: this.host,
      origin_anchor: this.origin_anchor,
      domains: std::collections::BTreeMap::new(),
      regions: std::collections::BTreeMap::new(),
      durability,
      region_mirrors: std::collections::BTreeMap::new(),
    });
  let transport = FleetTransport {
    identity: this.identity,
    advertise: this.address.into(),
    enrollment_roots: Vec::new(),
    name: NAME.to_owned(),
    probe_bind: this.address,
    record_bind: this.record_address,
    peers: vec![FleetPeer {
      anchor: peer.anchor,
      host: peer.host,
      address: peer.address.into(),
      record_address: peer.record_address.into(),
      certificate: peer.certificate,
    }],
    resolver: None,
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

  let peer_of_a = Peer {
    anchor: b.origin_anchor,
    host: b.host,
    address: b.address,
    record_address: b.record_address,
    certificate: b.identity.certificate(),
  };
  let peer_of_b = Peer {
    anchor: a.origin_anchor,
    host: a.host,
    address: a.address,
    record_address: a.record_address,
    certificate: a.identity.certificate(),
  };
  let daemon_a = start(a, peer_of_a);
  let daemon_b = start(b, peer_of_b);
  let host_b = daemon_b.member_identity().unwrap();

  // Let the fleet form: the loops establish their sessions and exchange probes over the transport. The
  // test thread is not a runtime task, so it waits by yielding on the clock (as the other daemon tests do
  // — the runtime's `futures::sleep` is unavailable off a shard); formation is polled, then a settle.
  let formed = form_and_settle(&[&daemon_a, &daemon_b]);
  assert!(
    formed
      && daemon_a
        .fleet_members()
        .is_ok_and(|members| members.contains(&host_b)),
    "the fleet is up: the direct probe mesh formed and A holds B in its membership"
  );

  // B dies — its shards, and so its serve loop, stop — so A's probes of B now time out.
  daemon_b.stop();

  // A's membership loop detects the timeouts, ages the suspicion to death, and retires B: a transition only
  // the loop can make over the transport.
  let retired = poll_until(&[&daemon_a], RETIREMENT_DEADLINE, || {
    daemon_a
      .fleet_members()
      .map(|members| !members.contains(&host_b))
  });

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

/// Polls `condition` (yielding between checks — the test thread is not a runtime task) until it holds, then
/// returns `true`. **Robust to noisy and heavy CPU load:** the wait is charged against the fleet's own forward
/// progress — the fleet-coordinator **periods** each of `daemons` has executed ([`Daemon::fleet_progress`],
/// read directly off a lock-free atomic, so it is reported even when a shard is too CPU-starved to answer a
/// query) — and **not** raw wall-clock. It returns `false` only when the operation has genuinely failed to
/// converge: either the **slowest** observed daemon executed a whole [`PERIOD_BUDGET`] of its own periods with
/// the condition never holding (a real non-convergence, judged in the daemon's own time), or no observed
/// daemon made **any** forward progress for the `within` window (every observed coordinator frozen or dead —
/// the only wall-clock bound, and it never fires while the fleet is still progressing).
///
/// This is why it does not flake under load where a fixed wall-clock deadline did. Fleet convergence is
/// period-driven, so under a CPU-load spike or sustained shared-tenant load the daemons run *fewer periods per
/// wall-second* but still converge in about the same number of periods; charging the budget in periods lets
/// the wait stretch in exact proportion to the slowdown, however large, and only a fleet that stops making
/// progress fails. Gating on the **minimum** progress across `daemons` (not a sum) is what keeps one starved
/// daemon holding the wait open rather than being masked by peers that keep ticking — the flaw a process-wide
/// tick count has. The two schemes that measured a *proxy* for load — the poll thread's own responsiveness,
/// and a load factor sampled once at formation — were measured-and-rejected (see
/// `docs/bugs/2026-09-12-retirement-tests-gate-on-real-swim-detection-under-load.md`); charging the daemons'
/// own continuous progress is what neither did. Pass every daemon whose state the condition reads; a laptop
/// (no coordinator, progress stuck at zero) makes the wait fall through to the `within` frozen-window bound.
///
/// **An observation the daemon could not make never satisfies `condition`.** Every observation accessor
/// returns `Result<T, ObserveError>` — the typed refusal naming the stage the observation reached when the
/// daemon was stopping, had no such shard, or stayed unresponsive for the observe budget — and `condition`
/// returns the observed truth or that refusal (`?` composes several observations; [`all_hold`] folds many).
/// The wait takes a [`Verdict`] of each ask: a refusal that can clear (a starved shard) keeps the wait
/// going, paced by [`UNAVAILABLE_PACE`] and counted in the trace, so a poll never reads a shard's silence
/// as the change it waits for (a "no longer contains" predicate on an *empty default* was a false pass on a
/// wedged shard); a refusal that can never clear (the daemon or its shard gone) ends the wait at once,
/// naming why, instead of spending the whole budget on a question nobody will answer.
///
/// The charge is per daemon ([`ProgressCharge`]): each daemon's periods since the wait began, and the wait
/// charged the least of them — so a daemon that began far ahead never pays for one that has stalled, which
/// the earlier "least absolute count" rule let happen under unequal starts (`tests/observe.rs` proves the
/// rule with numbers).
#[track_caller]
fn poll_until(
  daemons: &[&Daemon],
  within: Duration,
  mut condition: impl FnMut() -> Result<bool, ObserveError>,
) -> bool {
  let mut wait = WaitTrace::begin("poll", std::panic::Location::caller(), daemons);
  let charge = ProgressCharge::begin(each_progress(daemons));
  let mut last_charged = 0;
  let mut last_advance = Instant::now();
  // The pace after an unavailable ask is a timed wait on a channel nobody sends on: the test thread parks,
  // it does not spin against the shard it waits on.
  let (_pace_sender, pace) = std::sync::mpsc::channel::<()>();
  loop {
    let asked = Instant::now();
    let seen = verdict(condition());
    let took = asked.elapsed();
    let advances = charge.each(each_progress(daemons));
    match seen {
      Verdict::Holds => {
        wait.end("poll", "held", daemons, &advances);
        return true;
      }
      Verdict::Terminal(refusal) => {
        // Nothing observed here will ever answer: the wait ends now, and says why on stderr so a failed
        // assertion at the call site carries the reason.
        eprintln!(
          "poll {} ended: the daemon can never answer — {refusal}",
          site_of(wait.site)
        );
        wait.end("poll", &format!("terminal: {refusal}"), daemons, &advances);
        return false;
      }
      Verdict::Unavailable(refusal) => {
        wait.unavailable(&refusal);
        let _ = pace.recv_timeout(UNAVAILABLE_PACE);
      }
      Verdict::Observed => {}
    }
    let charged = advances.iter().min().copied().unwrap_or(0);
    if charged != last_charged {
      // The slowest observed daemon advanced — the fleet is alive (however slow under load); reset the
      // frozen-window timer and keep waiting until it has had a full period budget to converge.
      last_charged = charged;
      last_advance = Instant::now();
    }
    wait.asked(took, daemons, &advances, last_advance.elapsed());
    if charged >= PERIOD_BUDGET {
      // Every observed daemon ran a whole budget of its own periods, condition never met: a genuine
      // non-convergence (judged in daemon-time, so CPU load cannot cause it — periods, not wall-clock).
      wait.end("poll", "period budget spent", daemons, &advances);
      return false;
    }
    if last_advance.elapsed() >= within.max(FROZEN_CAP) {
      // The slowest observed daemon made NO forward progress for the frozen cap: that coordinator is frozen
      // or dead (a wedge), not merely slow. The only wall-clock bound; generous ([`FROZEN_CAP`]) so a
      // coordinator merely starved of the scheduler for tens of seconds under heavy load is not called dead.
      wait.end("poll", "frozen", daemons, &advances);
      return false;
    }
    std::thread::yield_now();
  }
}

/// Each daemon's fleet-coordinator progress ([`Daemon::fleet_progress`]), in the order given — the counts a
/// wait's [`ProgressCharge`] is begun from and advanced against.
fn each_progress(daemons: &[&Daemon]) -> Vec<u64> {
  daemons.iter().map(|d| d.fleet_progress()).collect()
}

/// Folds several observed conditions into one: holds when every one holds; a refusal that can never clear
/// outranks one that can, and either outranks a plain "not yet" — so a poll over many daemons ends at once
/// when one is gone, keeps waiting when one is starved, and holds only on observed truth from all.
fn all_hold(
  observations: impl IntoIterator<Item = Result<bool, ObserveError>>,
) -> Result<bool, ObserveError> {
  let mut holds = true;
  let mut refusal: Option<ObserveError> = None;
  for observed in observations {
    match observed {
      Ok(true) => {}
      Ok(false) => holds = false,
      Err(seen) => {
        let outranks = refusal.as_ref().is_none_or(|kept| !kept.is_terminal());
        if outranks {
          refusal = Some(seen);
        }
      }
    }
  }
  match refusal {
    Some(refusal) => Err(refusal),
    None => Ok(holds),
  }
}

/// Folds several observed conditions into one that holds when **any** holds, with [`all_hold`]'s
/// precedence for refusals: a refusal that can never clear outranks one that can, and either outranks a
/// plain "none yet".
fn any_holds(
  observations: impl IntoIterator<Item = Result<bool, ObserveError>>,
) -> Result<bool, ObserveError> {
  let mut holds = false;
  let mut refusal: Option<ObserveError> = None;
  for observed in observations {
    match observed {
      Ok(true) => holds = true,
      Ok(false) => {}
      Err(seen) => {
        if refusal.as_ref().is_none_or(|kept| !kept.is_terminal()) {
          refusal = Some(seen);
        }
      }
    }
  }
  match refusal {
    Some(refusal) => Err(refusal),
    None => Ok(holds),
  }
}

/// Whether exactly one of `daemons` believes itself the leader `leads` asks about (the council's or the
/// root group's), on observed state from every one.
fn exactly_one_leads<'a>(
  daemons: impl IntoIterator<Item = &'a Daemon>,
  leads: impl Fn(&Daemon) -> Result<bool, ObserveError>,
) -> Result<bool, ObserveError> {
  let mut leaders = 0;
  for daemon in daemons {
    if leads(daemon)? {
      leaders += 1;
    }
  }
  Ok(leaders == 1)
}

/// Whether exactly one of `daemons` believes itself the council leader.
fn one_leader<'a>(daemons: impl IntoIterator<Item = &'a Daemon>) -> Result<bool, ObserveError> {
  exactly_one_leads(daemons, Daemon::council_leads)
}

/// Whether exactly one of `daemons` believes itself the root group's leader.
fn one_root_leader<'a>(
  daemons: impl IntoIterator<Item = &'a Daemon>,
) -> Result<bool, ObserveError> {
  exactly_one_leads(daemons, Daemon::root_leads)
}

/// The index of the daemon that believes itself the council leader, read once (a daemon that could not
/// be observed is not the leader for this choice); `None` when none does.
fn leader_index(daemons: &[Daemon]) -> Option<usize> {
  daemons
    .iter()
    .position(|daemon| daemon.council_leads() == Ok(true))
}

/// Injects `dead`'s death into every one of `daemons` and asserts each injection landed and folded — a
/// cue the test relies on is proven delivered, never assumed.
fn inject_death_into<'a>(daemons: impl IntoIterator<Item = &'a Daemon>, dead: HostId) {
  for daemon in daemons {
    let landed = daemon.observe_peer_dead(dead, FALSE_DEATH_INCARNATION);
    assert!(
      landed.is_ok(),
      "the injected death of {dead:?} reached {}'s control shard: {landed:?}",
      daemon.config().instance
    );
  }
}

/// Polls that `condition` stays true for the whole `window`; returns whether it never broke — the stability
/// check a "did not flap" assertion needs. As with [`poll_until`], an observation the daemon could not make
/// never satisfies `condition`: a hold is only proven over observed samples, so an unanswered shard fails
/// the hold rather than passing it, and the trace and stderr say which refusal broke it.
#[track_caller]
fn holds_for(window: Duration, mut condition: impl FnMut() -> Result<bool, ObserveError>) -> bool {
  let site = std::panic::Location::caller();
  let began = Instant::now();
  let deadline = began + window;
  // How many samples the hold rests on: under load each ask can take seconds, so a hold proven over one
  // sample is a weak proof, and the trace says so.
  let mut asks: u64 = 0;
  while Instant::now() < deadline {
    asks += 1;
    let broke = match verdict(condition()) {
      Verdict::Holds => None,
      Verdict::Observed => Some("the condition did not hold".to_owned()),
      Verdict::Unavailable(refusal) | Verdict::Terminal(refusal) => {
        Some(format!("unobserved: {refusal}"))
      }
    };
    if let Some(why) = broke {
      eprintln!(
        "hold {} broke after {:.3}s of {:.3}s: {why}",
        site_of(site),
        began.elapsed().as_secs_f64(),
        window.as_secs_f64()
      );
      if trace::enabled() {
        trace::record(format_args!(
          "hold {} broke after {:.3}s of {:.3}s asks={asks} why={why:?}",
          site_of(site),
          began.elapsed().as_secs_f64(),
          window.as_secs_f64()
        ));
      }
      return false;
    }
    std::thread::yield_now();
  }
  if trace::enabled() {
    trace::record(format_args!(
      "hold {} held {:.3}s asks={asks}",
      site_of(site),
      window.as_secs_f64()
    ));
  }
  true
}

/// A fixed [`FORMATION_SETTLE`] window of yielding on the clock — the test thread is not a runtime task, so
/// it cannot `futures::sleep`; used only *after* a polled formation, to let freshly formed sessions steady
/// (a few probes answered, a measured round trip seeded) before the test acts on them.
#[track_caller]
fn settle() {
  let site = std::panic::Location::caller();
  let began = Instant::now();
  let deadline = began + FORMATION_SETTLE;
  while Instant::now() < deadline {
    std::thread::yield_now();
  }
  if trace::enabled() {
    trace::record(format_args!(
      "settle {} took {:.3}s",
      site_of(site),
      began.elapsed().as_secs_f64()
    ));
  }
}

/// Waits for every daemon's direct probe mesh to form — polled in daemon time ([`poll_until`]), so a
/// formation slowed by CPU load is waited out rather than asserted early — and then a fixed [`settle`] so
/// the sessions steady. Returns whether the mesh formed. A fixed settle alone, followed by an assertion
/// that the mesh had formed, failed the rejoin test at load average 48 on 2026-09-14 (a formation that
/// takes longer than two seconds under load is slow, not wrong): only the steadying is a fixed window.
fn form_and_settle(daemons: &[&Daemon]) -> bool {
  if let Some(first) = daemons.first() {
    match first.bootstrap(true) {
      Ok(()) | Err(slates_ipc::protocol::Refusal::ConsensusAlreadyInitialized) => {}
      Err(_) => return false,
    }
  }
  let formed = poll_until(daemons, FORMATION_DEADLINE, || {
    all_hold(daemons.iter().map(|daemon| daemon.fleet_meshed()))
  });
  settle();
  formed
}

/// AC (§4.8, rejoin): a peer the fleet **retired** is **re-admitted when it comes back**, by SWIM
/// refutation, realized to slates' spec — the configuration group is the membership authority, SWIM is only
/// detection, and re-admission needs no separate incarnation tracker or bump because [`Membership::refute`]
/// bumps past the death incarnation it hears. A is made to (falsely) retire B — B is alive throughout, so
/// this drives the pure re-admission path deterministically, without a process kill (the restart path —
/// a stopped daemon's serve ports freed and bound again — is
/// [`a_stopped_daemons_serve_ports_are_freed_so_its_restart_binds_the_same_addresses`]). B keeps probing A; A, believing B dead, echoes that in its acknowledgement
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
  let peer_of_a = Peer {
    anchor: b.origin_anchor,
    host: b.host,
    address: b.address,
    record_address: b.record_address,
    certificate: b.identity.certificate(),
  };
  let peer_of_b = Peer {
    anchor: a.origin_anchor,
    host: a.host,
    address: a.address,
    record_address: a.record_address,
    certificate: a.identity.certificate(),
  };
  let daemon_a = start(a, peer_of_a);
  let daemon_b = start(b, peer_of_b);
  let host_b = daemon_b.member_identity().unwrap();

  // Let the direct probe mesh form (polled, so a formation slowed by load is waited out) and then settle:
  // the seeded membership holds every peer alive from boot, so B must actually be probing A — and their
  // sessions steady — before the injection, so B hears A's echo and refutes over a stable session rather
  // than one mid-formation.
  assert!(
    form_and_settle(&[&daemon_a, &daemon_b]),
    "the fleet's direct probe mesh formed"
  );

  // A falsely retires B — a false positive; B is alive and still probing A. This is the same fold A's
  // detector performs when it ages a peer to death.
  inject_death_into([&daemon_a], host_b);
  let retired = poll_until(&[&daemon_a, &daemon_b], RETIREMENT_DEADLINE, || {
    daemon_a
      .fleet_members()
      .map(|members| !members.contains(&host_b))
  });

  // B, alive and still probing A, learns of its death from A's echo, refutes, and A re-admits it.
  let rejoined = poll_until(&[&daemon_a, &daemon_b], REJOIN_DEADLINE, || {
    daemon_a
      .fleet_members()
      .map(|members| members.contains(&host_b))
  });

  // The re-admission is stable — B does not flap back out over a further settle.
  let stable = rejoined
    && holds_for(FORMATION_SETTLE, || {
      daemon_a
        .fleet_members()
        .map(|members| members.contains(&host_b))
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

/// Derived: how long [`a_starved_but_live_peer_is_not_retired`] starves B's control shard — three anchor
/// liveness budgets ([`LIVENESS_BUDGET_NS`]). Past the silence at which the old fixed probe deadline (one
/// heartbeat, so six misses in a row) declared a peer dead (≈ 1.2 s at rest), and past the suspicion's
/// `λ·ln(n+1)` gossip transmits (four pings at two nodes), so the probe B finally answers carries A's
/// suspicion only by the buddy system; and within what the derived deadline tolerates (its backoff reaches
/// the liveness-budget cap by the fifth probe, so six misses need ≈ 4 s of silence at rest, more under load).
const STARVATION_NS: u64 = 3 * LIVENESS_BUDGET_NS;

/// AC (§4.8 "Derived constants" — "detection timeout for membership from RTT p99 × k; SWIM period = max(k ×
/// RTT p99, scheduler quantum)"): a peer that is **alive but starved of CPU** is not retired. B's control
/// shard is held busy for [`STARVATION_NS`] ([`Daemon::starve_control_shard`]) — the starvation a shared,
/// oversubscribed box inflicts on a live daemon (measured on this box at load average 41: a live root voter
/// retired and re-admitted in a loop, the region count flapping 3→2→3→2→3 with nothing killed), injected
/// deterministically — so B's acknowledgements to A's probes stop for the whole span, then resume. A must
/// keep B a member throughout: its probe deadline is derived from the measured round trip and backs off
/// toward the liveness budget on each miss, so six late acknowledgements are not six misses; the probe B
/// finally answers carries A's suspicion (Lifeguard's buddy system), so B refutes it from that very probe;
/// and B's late answer to an abandoned earlier probe is discarded and the probe re-sent, not counted as a
/// miss. Non-vacuous: the hold is proven to have held (its measured span comes back from the shard it ran
/// on), A's view is watched for the whole span and a settle past it, and the mesh is shown whole after.
#[test]
fn a_starved_but_live_peer_is_not_retired() {
  let _serial = serialize_fleet_tests();
  let [pa_probe, pa_record, pb_probe, pb_record] = four_free_ports();
  let a = node("a", pa_probe, pa_record);
  let b = node("b", pb_probe, pb_record);
  let peer_of_a = Peer {
    anchor: b.origin_anchor,
    host: b.host,
    address: b.address,
    record_address: b.record_address,
    certificate: b.identity.certificate(),
  };
  let peer_of_b = Peer {
    anchor: a.origin_anchor,
    host: a.host,
    address: a.address,
    record_address: a.record_address,
    certificate: a.identity.certificate(),
  };
  let daemon_a = start(a, peer_of_a);
  let daemon_b = start(b, peer_of_b);
  let host_b = daemon_b.member_identity().unwrap();

  // The direct probe mesh forms, then settles: a few answered probes seed A's measured round trip to B.
  let formed = poll_until(&[&daemon_a, &daemon_b], FORMATION_DEADLINE, || {
    all_hold([daemon_a.fleet_meshed(), daemon_b.fleet_meshed()])
  });
  settle();

  // Starve B: its control shard runs nothing else — no acknowledgements to A — for the whole span.
  let hold = daemon_b.starve_control_shard(STARVATION_NS);
  // A must keep B through the span and a settle past it (a retirement that lands late is still one).
  let kept = holds_for(
    Duration::from_nanos(STARVATION_NS) + FORMATION_SETTLE,
    || {
      daemon_a
        .fleet_members()
        .map(|members| members.contains(&host_b))
    },
  );
  let held_ns = hold.and_then(|done| done.answer());
  // B is back: both sides see the mesh whole.
  let meshed_after = poll_until(&[&daemon_a, &daemon_b], FORMATION_DEADLINE, || {
    all_hold([daemon_a.fleet_meshed(), daemon_b.fleet_meshed()])
  });

  daemon_a.stop();
  daemon_b.stop();
  assert!(formed, "the fleet's direct probe mesh formed");
  assert!(
    held_ns.as_ref().is_ok_and(|&held| held >= STARVATION_NS),
    "B's control shard was held for the whole span (measured {held_ns:?} ns of {STARVATION_NS})"
  );
  assert!(
    kept,
    "A kept B a member through B's starvation and a settle past it — a starved live peer is not retired"
  );
  assert!(meshed_after, "the mesh is whole again after B's starvation");
}

/// Shape: one competing runnable thread per daemon. In the opt-in Linux quota fixture their combined
/// demand exceeds the CPU allocation; neither thread occupies a daemon's control-shard poll.
#[cfg(target_os = "linux")]
const PRESSURE_WORKERS: usize = 2;
/// Derived: four of the existing three-liveness-budget starvation windows. Long enough to observe
/// several Linux quota replenishments and probes while every worker remains runnable.
#[cfg(target_os = "linux")]
const PRESSURE_WINDOW: Duration = Duration::from_nanos(4 * STARVATION_NS);

/// AC (§4.8 "Derived constants", R5): under OS CPU contention, a measured scheduling delay actually
/// widens live probe windows while the original peers remain members in every observed sample and
/// continue acknowledging probes. Run opt-in under a Linux CPU quota with a replenishment period above
/// HEARTBEAT_NS; the recorded command and limits are in the scheduler-quantum bug report. This refuses
/// a vacuous pass on an unconstrained host. The workers are finite and joined, even on a failed assertion.
#[test]
#[cfg(target_os = "linux")]
fn a_descheduled_observer_uses_its_quantum_and_keeps_its_live_peer() {
  if std::env::var_os("SLATES_TEST_SCHEDULER_PRESSURE").as_deref()
    != Some(std::ffi::OsStr::new("1"))
  {
    eprintln!(
      "SKIP: set SLATES_TEST_SCHEDULER_PRESSURE=1 inside the documented Linux CPU quota fixture"
    );
    return;
  }
  let _serial = serialize_fleet_tests();
  let (daemon_a, daemon_b, _) = two_node_fleet();
  let host_a = daemon_a.member_identity().unwrap();
  let host_b = daemon_b.member_identity().unwrap();
  let before = [
    daemon_a.fleet_probe_windows().unwrap(),
    daemon_b.fleet_probe_windows().unwrap(),
  ];
  let mut during = before;
  let mut samples = 0u64;
  let mut largest_overrun_ns = 0;
  let kept = std::thread::scope(|scope| {
    let pressure_began = Instant::now();
    let workers: Vec<_> = (0..PRESSURE_WORKERS)
      .map(|_| {
        scope.spawn(|| {
          let began = Instant::now();
          while began.elapsed() < PRESSURE_WINDOW {
            std::hint::black_box(began.elapsed());
          }
        })
      })
      .collect();
    let kept = holds_for(PRESSURE_WINDOW, || {
      let a_kept_b = daemon_a.fleet_members()?.contains(&host_b);
      let b_kept_a = daemon_b.fleet_members()?.contains(&host_a);
      let windows = [
        daemon_a.fleet_probe_windows()?,
        daemon_b.fleet_probe_windows()?,
      ];
      // Only observations completed while the workers are still running prove progress under load;
      // an acknowledgement after the workers finish must not rescue a vacuous pressure window.
      if pressure_began.elapsed() < PRESSURE_WINDOW {
        during = windows;
        samples += 1;
        for daemon in [&daemon_a, &daemon_b] {
          for pulse in daemon.shard_pulses() {
            largest_overrun_ns = largest_overrun_ns.max(pulse.scheduler_overrun_ns);
          }
        }
      }
      Ok(a_kept_b && b_kept_a)
    });
    for worker in workers {
      worker.join().expect("the finite pressure worker completed");
    }
    kept
  });
  eprintln!(
    "scheduler pressure: samples={samples} largest_overrun_ns={largest_overrun_ns} \
     before={before:?} during={during:?}"
  );
  daemon_a.stop();
  daemon_b.stop();
  assert!(
    kept && samples > 1,
    "both members held across observed samples"
  );
  let after = during;
  assert!(
    before
      .iter()
      .zip(&after)
      .all(|(before, after)| after.acknowledged > before.acknowledged),
    "both daemons acknowledged probes during the pressure window"
  );
  assert!(
    before
      .iter()
      .zip(&after)
      .any(|(before, after)| after.deadlines_dilated > before.deadlines_dilated),
    "the measured quantum lengthened an actual probe deadline"
  );
  assert!(
    before
      .iter()
      .zip(&after)
      .any(|(before, after)| after.periods_dilated > before.periods_dilated),
    "the measured quantum lengthened an actual sleep between probes"
  );
  assert!(
    after
      .iter()
      .any(|windows| windows.largest_quantum_ns > HEARTBEAT_NS),
    "OS descheduling exceeded the assumed heartbeat floor"
  );
}

/// Shape: how many times one burst dialer drives its handshake before it gives up — as the daemon's own
/// dialer, a budget that runs out with the peer silent is retried on the same socket (its pending flight
/// resent), so a dialer the demultiplexer refused for want of a slot reaches the daemon once a slot frees;
/// wide enough for every dialer of the burst to be served in turn under load, since each turn waits out a
/// backed-off probe timeout before the next flight.
const BURST_DIAL_ATTEMPTS: usize = 8;
/// Derived: how often a dialer holding its established session looks for its release — a hundredth of the
/// liveness budget (a tenth of the heartbeat, the tree's collection-loop cadence), so the release is seen
/// within milliseconds without spinning the burst runtime's shard.
const HOLD_POLL_NS: u64 = LIVENESS_BUDGET_NS / 100;

/// One dialer of a re-dial burst: dials `address` (a daemon's record socket) from a fresh socket — what a
/// node that re-dials after losing its session does — presenting `identity`, and drives the handshake as
/// the daemon's own dialer does (`fleet::establish_session`): a handshake budget that ran out with the
/// peer silent is retried on the same socket, its pending flight resent; any other failure dials afresh
/// from a new port; [`BURST_DIAL_ATTEMPTS`] in all. Reports the dial's index and its outcome, then — if it
/// established — **holds** its session until `release` says so (or the test is gone), so every session of
/// the burst is live on the daemon while the test serves its client through them: the overlap the burst
/// exists to prove is by construction, not by timing. Released, the endpoint drops, its session on the
/// daemon left for the next dial to replace or the daemon to close.
async fn dial_record_socket(
  identity: Identity,
  certificate: rustls::pki_types::CertificateDer<'static>,
  address: SocketAddrV4,
  report: std::sync::mpsc::Sender<(usize, Result<(), String>)>,
  release: std::sync::mpsc::Receiver<()>,
  index: usize,
) {
  let mut outcome: Result<(), String> = Err("never attempted".to_owned());
  let mut held: Option<Endpoint> = None;
  let mut established: Option<Endpoint> = None;
  for _ in 0..BURST_DIAL_ATTEMPTS {
    let mut dialer = match held.take() {
      Some(dialer) => dialer,
      None => {
        let fresh = slates_rt::udp::UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
          .map_err(|e| format!("bind: {e:?}"))
          .and_then(|socket| {
            Endpoint::client(
              socket,
              address,
              &identity,
              &certificate,
              NAME,
              FLEET_FRAME_CAP,
            )
            .map_err(|e| format!("client: {e:?}"))
          });
        match fresh {
          Ok(dialer) => dialer,
          Err(e) => {
            outcome = Err(e);
            break;
          }
        }
      }
    };
    match dialer.establish().await {
      Ok(()) => {
        outcome = Ok(());
        established = Some(dialer);
        break;
      }
      Err(EndpointError::NotReady) => {
        outcome = Err("NotReady".to_owned());
        if dialer.handshake_budgets_spent() < slates_server::fleet::ESTABLISH_BUDGETS_BEFORE_REDIAL
        {
          held = Some(dialer);
        }
      }
      Err(e) => outcome = Err(format!("{e:?}")),
    }
  }
  let _ = report.send((index, outcome));
  if established.is_some() {
    loop {
      match release.try_recv() {
        Ok(()) | Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
        Err(std::sync::mpsc::TryRecvError::Empty) => slates_rt::futures::sleep(HOLD_POLL_NS).await,
      }
    }
  }
  drop(established);
}

/// §4.3 (a per-shard singleton is owned by its shard and dropped with it) by use, at the fleet: a
/// **stopped daemon's serve ports are free again**, so a daemon started in its place binds the same
/// addresses and joins the fleet. Do: form a two-node fleet; stop B; start a fresh B on B's exact probe and
/// record addresses. Expect: the restart counts no `fleet.bind` refusal (the addresses bound), and the
/// mesh re-forms — A and the restarted B each see their probe session formed — within the formation
/// deadline. Non-vacuous: the first fleet is shown formed, so the addresses were held; before 2026-09-14
/// a daemon's demultiplexers were leaked with their sockets to the process lifetime, so the restart's
/// bind was refused (`fleet.bind`) and it took no part in the fleet — the reason the restart tests dial a
/// restart at a *different* pair (`restart_fleet_forms_and_seals`).
#[test]
fn a_stopped_daemons_serve_ports_are_freed_so_its_restart_binds_the_same_addresses() {
  let _serial = serialize_fleet_tests();
  let [pa_probe, pa_record, pb_probe, pb_record] = four_free_ports();
  let a = node("a", pa_probe, pa_record);
  let b = node("b", pb_probe, pb_record);
  // The restart presents B's certificate again (the operator-provisioned identity does not change) at B's
  // addresses; a fresh segment must still produce a distinct voting identity.
  let mut b_identities = same_identity(2).into_iter();
  let (Some(b_first), Some(b_again)) = (b_identities.next(), b_identities.next()) else {
    panic!("B's identity twice");
  };
  let b = Node {
    identity: b_first,
    ..b
  };
  let b_again = Node {
    identity: b_again,
    ..node("b", pb_probe, pb_record)
  };
  let peer_of_a = Peer {
    anchor: b.origin_anchor,
    host: b.host,
    address: b.address,
    record_address: b.record_address,
    certificate: b.identity.certificate(),
  };
  let peer_of_b = Peer {
    anchor: a.origin_anchor,
    host: a.host,
    address: a.address,
    record_address: a.record_address,
    certificate: a.identity.certificate(),
  };
  let daemon_a = start(a, peer_of_a);
  let daemon_b = start(b, peer_of_b.clone());
  let formed = form_and_settle(&[&daemon_a, &daemon_b]);
  daemon_b.stop();

  let daemon_b_again = start(b_again, peer_of_b);
  // The mesh re-forms only if the restart bound B's addresses; polled for that (the positive event), then
  // the refusal counts are read once — a bind refusal never clears, so it is either there or not.
  let reformed = poll_until(&[&daemon_a, &daemon_b_again], FORMATION_DEADLINE, || {
    all_hold([daemon_a.fleet_meshed(), daemon_b_again.fleet_meshed()])
  });
  let refusals = daemon_b_again.fleet_refusals();
  daemon_a.stop();
  daemon_b_again.stop();

  assert!(formed, "the first fleet formed, so B's addresses were held");
  // The refusal map must be observed: an absent key is the only zero, and an unobserved map proves nothing.
  assert!(
    matches!(&refusals, Ok(refusals) if !refusals.contains_key("fleet.bind")),
    "the restart bound B's addresses (no fleet.bind refusal): {refusals:?}"
  );
  assert!(
    reformed,
    "the mesh re-formed to the restart at the same addresses: {refusals:?}"
  );
}

/// Shape: the index of the record plane in `Daemon::fleet_demux_counters` (the probe plane comes first).
const RECORD_PLANE: usize = 1;
/// Shape: the dials of the re-dial burst — three, the 2026-09-14 burst that overflowed that fixture's
/// two-slot pool (`peers × 2`, one peer). Kept as the burst whose replacement, client availability and
/// reclamation this proves; the enrollment-sized pool (thousands of slots per plane) admits it whole, and
/// the exhaustion it once showed is proven at the transport seam (`crates/transport/tests/session.rs`).
const BURST_DIALS: usize = 3;

/// The two-node fleet a re-dial burst is run against: A (the target — its certificate, record address and
/// instance name), B (whose certificate the burst presents), and the burst's own copies of B's identity.
struct BurstFleet {
  daemon_a: Daemon,
  daemon_b: Daemon,
  host_b: HostId,
  a_certificate: rustls::pki_types::CertificateDer<'static>,
  a_record_address: SocketAddrV4,
  instance_a: String,
  burst_identities: Vec<Identity>,
  formed: bool,
}

/// Forms the burst test's two-node fleet, with `dials` copies of B's identity set aside for the burst.
fn burst_fleet_forms(dials: usize, pid: u32) -> BurstFleet {
  let [pa_probe, pa_record, pb_probe, pb_record] = four_free_ports();
  let a = node("a", pa_probe, pa_record);
  // B's certificate as many times as it is presented: once by daemon B, once per burst dial — the same
  // enrolled identity from a fresh socket each time, as a node that re-dials after a loss presents.
  let mut burst_identities = same_identity(dials + 1);
  let b = Node {
    identity: burst_identities.pop().unwrap(),
    ..node("b", pb_probe, pb_record)
  };
  let a_certificate = a.identity.certificate();
  let a_record_address = a.record_address;
  let instance_a = format!("fleet-{}-{pid}", a.host.0);
  let peer_of_a = Peer {
    anchor: b.origin_anchor,
    host: b.host,
    address: b.address,
    record_address: b.record_address,
    certificate: b.identity.certificate(),
  };
  let peer_of_b = Peer {
    anchor: a.origin_anchor,
    host: a.host,
    address: a.address,
    record_address: a.record_address,
    certificate: a.identity.certificate(),
  };
  let daemon_a = start(a, peer_of_a);
  let daemon_b = start(b, peer_of_b);
  let host_b = daemon_b.member_identity().unwrap();
  let formed = form_and_settle(&[&daemon_a, &daemon_b]);
  BurstFleet {
    daemon_a,
    daemon_b,
    host_b,
    a_certificate,
    a_record_address,
    instance_a,
    burst_identities,
    formed,
  }
}

/// What the burst observed: each dial's outcome; the client's verbs and refusals while the dials were in
/// flight and, separately, while every dial held its established session (the by-construction overlap);
/// the demultiplexers' counters and A's live tasks before and after; whether the replaced sessions' serve
/// tasks ended; A's fleet refusal counts; and whether the fleet still held.
#[derive(Debug)]
struct BurstOutcome {
  burst_done: bool,
  reports: Vec<(usize, Result<(), String>)>,
  verbs_during_burst: u64,
  verbs_while_held: u64,
  refused_during_burst: Vec<String>,
  before: Result<Vec<slates_transport::demux::DemuxCounters>, ObserveError>,
  after: Result<Vec<slates_transport::demux::DemuxCounters>, ObserveError>,
  live_before: Result<usize, ObserveError>,
  live_after: Result<usize, ObserveError>,
  tasks_settled: bool,
  refusals: Result<std::collections::BTreeMap<&'static str, u64>, ObserveError>,
  still_formed: bool,
}

/// One client verb while the burst runs: `Status` on `volume`, counted as run or recorded as refused.
fn status_verb(client: &mut Client, volume: VolumeId, ran: &mut u64, refused: &mut Vec<String>) {
  match client.call(&RequestBody::Status { volume }) {
    ReplyBody::Status { .. } => *ran += 1,
    other => refused.push(format!("{other:?}")),
  }
}

/// Runs the burst against A from a runtime of the test's own (one shard, the daemon's own derived
/// configuration) so the dials run concurrently, while the test thread drives `client` through `Status`
/// verbs on `volume`; once every dial has established and holds its session, serves the client once more
/// through them (the overlap, by construction), releases the dials, then waits for the replaced sessions'
/// serve tasks to end and the fleet to re-settle.
fn run_burst(
  fleet: &mut BurstFleet,
  client: &mut Client,
  volume: VolumeId,
  pid: u32,
) -> BurstOutcome {
  let identities = std::mem::take(&mut fleet.burst_identities);
  let dials = identities.len();
  let daemons = [&fleet.daemon_a, &fleet.daemon_b];
  let live_before = fleet.daemon_a.live_tasks();
  let before = fleet.daemon_a.fleet_demux_counters();
  let burst_config =
    DaemonConfig::derive(&profile("burst"), &format!("burst-{pid}"), Some(1)).runtime;
  let burst = slates_rt::runtime::Runtime::start(&burst_config).expect("the burst runtime starts");
  let burst_shard = burst.shard_ids()[0];
  let (report_tx, report_rx) = std::sync::mpsc::channel();
  let mut releases = Vec::with_capacity(dials);
  for (index, identity) in identities.into_iter().enumerate() {
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    releases.push(release_tx);
    burst
      .spawn_on(
        burst_shard,
        dial_record_socket(
          identity,
          fleet.a_certificate.clone(),
          fleet.a_record_address,
          report_tx.clone(),
          release_rx,
          index,
        ),
      )
      .expect("the burst runtime admits a dialer");
  }
  drop(report_tx);
  let mut reports = Vec::new();
  let mut verbs_during_burst = 0u64;
  let mut refused_during_burst = Vec::new();
  let burst_done = poll_until(&daemons, FORMATION_DEADLINE, || {
    reports.extend(report_rx.try_iter());
    if reports.len() < dials {
      status_verb(
        client,
        volume,
        &mut verbs_during_burst,
        &mut refused_during_burst,
      );
    }
    Ok(reports.len() == dials)
  });
  reports.sort_by_key(|(index, _)| *index);
  // Every dial that established holds its session on A right now: the client is served through the
  // burst's live sessions — the overlap the burst exists to prove, by construction rather than by timing.
  let mut verbs_while_held = 0u64;
  status_verb(
    client,
    volume,
    &mut verbs_while_held,
    &mut refused_during_burst,
  );
  let after = fleet.daemon_a.fleet_demux_counters();
  for release in releases {
    let _ = release.send(());
  }
  // The replaced sessions' serve tasks end as each replacement closes them: A's live tasks return to the
  // pre-burst level (the burst's last session stands in for B's record session, one serve task either way).
  let tasks_settled = poll_until(&daemons, FORMATION_DEADLINE, || {
    let now = fleet.daemon_a.live_tasks()?;
    Ok(live_before.as_ref().is_ok_and(|&earlier| now <= earlier))
  });
  let live_after = fleet.daemon_a.live_tasks();
  let refusals = fleet.daemon_a.fleet_refusals();
  let still_formed = form_and_settle(&daemons)
    && fleet
      .daemon_a
      .fleet_members()
      .is_ok_and(|members| members.contains(&fleet.host_b));
  burst.shutdown();
  BurstOutcome {
    burst_done,
    reports,
    verbs_during_burst,
    verbs_while_held,
    refused_during_burst,
    before,
    after,
    live_before,
    live_after,
    tasks_settled,
    refusals,
    still_formed,
  }
}

/// The burst's first assertions: every dial reported and established, and the client ran verbs through
/// the burst with none refused.
fn assert_burst_dials_and_client(outcome: &BurstOutcome) {
  assert!(
    outcome.burst_done,
    "every dial of the burst reported within the deadline: {:?}",
    outcome.reports
  );
  let failed: Vec<&(usize, Result<(), String>)> = outcome
    .reports
    .iter()
    .filter(|(_, dial)| dial.is_err())
    .collect();
  assert!(
    failed.is_empty(),
    "every dial of the burst established in turn (the refused ones once a slot freed): {failed:?}"
  );
  assert!(
    outcome.verbs_during_burst > 0,
    "the client ran verbs while the burst was in flight (non-vacuity)"
  );
  assert!(
    outcome.verbs_while_held >= 1,
    "the client was served while every dial of the burst held its established session on A (the overlap, \
     by construction)"
  );
  assert!(
    outcome.refused_during_burst.is_empty(),
    "no client verb was refused during the burst ({} ran): {:?}",
    outcome.verbs_during_burst,
    outcome.refused_during_burst
  );
}

/// The burst's bound assertions: the record plane refused the dials past its slots and replaced a session
/// per later dial, the replaced sessions' serve tasks ended, no serve spawn was refused, the fleet holds.
fn assert_burst_bounded(outcome: &BurstOutcome, dials: usize) {
  let (before, after) = match (&outcome.before, &outcome.after) {
    (Ok(before), Ok(after)) => (before[RECORD_PLANE], after[RECORD_PLANE]),
    other => panic!("the demultiplexer counters were observed before and after: {other:?}"),
  };
  let setup_refused = after.setup_refused - before.setup_refused;
  let replaced = after.replaced - before.replaced;
  if trace::enabled() {
    trace::record(format_args!(
      "burst: {dials} dials, {} client verbs during it and {} while held, record plane before {before:?} \
       after {after:?}, live tasks before {:?} after {:?}, refusals {:?}",
      outcome.verbs_during_burst,
      outcome.verbs_while_held,
      outcome.live_before,
      outcome.live_after,
      outcome.refusals
    ));
  }
  // Pending handshakes and authenticated identities now have separate reservations. Either may
  // refuse a burst temporarily; admission.rs proves those exact bounds and release/retry by use.
  assert_eq!(
    setup_refused, 0,
    "no dial failed at its session's setup: before {before:?} after {after:?}"
  );
  assert!(
    replaced >= u64::try_from(dials - 1).unwrap(),
    "every later dial replaced an earlier session ({dials} dials): before {before:?} after {after:?}"
  );
  assert!(
    outcome.tasks_settled,
    "the replaced sessions' serve tasks ended: live tasks before {:?}, after {:?}",
    outcome.live_before, outcome.live_after
  );
  // The refusal counts must have been **observed**: an absent kind in an observed map is a zero count; a
  // daemon that could not be observed is its own failure, never a zero.
  let refusals = outcome.refusals.as_ref().expect(
    "A's fleet refusal counts were observed after the burst (an unobservable daemon is not a zero count)",
  );
  let serve_spawn_refused = refusals.get("fleet.serve_spawn").copied().unwrap_or(0);
  assert_eq!(
    serve_spawn_refused, 0,
    "no serve spawn was refused: the fleet's share of the arena carried the burst ({refusals:?})"
  );
  assert!(outcome.still_formed, "the fleet holds after the burst");
}

/// §4.8 ("reconnection after a mid-run session loss") and §4.3 (the fleet's derived task share), by use: a
/// peer that re-dials a daemon's record socket in a **burst** — several concurrent dials presenting one
/// certificate, each replacing the last as it establishes — is served in turn, replaces its earlier
/// sessions, never costs a client a verb, and leaves no serve task behind. Do: form a two-node fleet; run
/// the burst against A from a runtime of the test's own while a client of A runs `Status` verbs; with every
/// dial established and **holding** its session, serve the client once more through them; release. Expect:
/// every dial establishes; the client is never refused, and was served at least once while the burst's
/// sessions were all live (by construction); every later dial replaced an earlier session (`replaced`);
/// no dial fails at setup. Pending-capacity or per-certificate saturation may refuse a dial until a
/// serve task releases its endpoint; those exact bounds are driven in `transport/tests/admission.rs`.
/// The serve tasks of replaced
/// sessions end (A's live task count returns to its pre-burst level — one serve task per live session,
/// never one per dial); no serve spawn is refused, the refusal counts **observed** and never assumed zero;
/// and the fleet still holds. Before the task share existed the fleet's tasks were admitted against the
/// clients' budget alone: at ~3× oversubscription the arena filled with accept-side handshakes and the
/// shard refused spawns (`adm_refused` 4,554, `docs/wip/fleet-under-load.md`).
/// `docs/bugs/2026-09-16-redial-burst-assumes-a-per-peer-session-limit.md`.
#[test]
fn a_peers_re_dial_burst_replaces_its_sessions_and_never_refuses_a_client() {
  let _serial = serialize_fleet_tests();
  let pid = std::process::id();
  let dials = BURST_DIALS;
  let mut fleet = burst_fleet_forms(dials, pid);
  // The client's volume, provisioned before the burst; its `Status` is the verb the client runs throughout.
  let mut client = Client::connect(&fleet.instance_a);
  let ReplyBody::Created { id: volume } = client.call(&scratch("burst-client")) else {
    fleet.daemon_a.stop();
    fleet.daemon_b.stop();
    panic!("the client's volume was not provisioned before the burst");
  };
  let formed = fleet.formed;
  let outcome = run_burst(&mut fleet, &mut client, volume, pid);
  fleet.daemon_a.stop();
  fleet.daemon_b.stop();
  assert!(formed, "the fleet formed before the burst");
  assert_burst_dials_and_client(&outcome);
  assert_burst_bounded(&outcome, dials);
}

/// AC-8.1, §4.8: inject a restart with a distinct identity into SWIM, then observe the
/// old member retired and the new member present. The three-node wire test below covers
/// actual admission after RAM loss; this test isolates the discovery fold.
#[test]
fn a_restarted_peer_rejoins_under_a_new_member_id_and_the_old_is_retired() {
  let _serial = serialize_fleet_tests();
  let [pa_probe, pa_record, pb_probe, pb_record] = four_free_ports();
  let a = node("a", pa_probe, pa_record);
  let b = node("b", pb_probe, pb_record);
  let b_new = member_id(b.origin_anchor, 1); // B's member id after one restart (generation 1).
  assert_ne!(
    b.host, b_new,
    "the member id is ephemeral — a restart holds a new id, not its old self"
  );
  let peer_of_a = Peer {
    anchor: b.origin_anchor,
    host: b.host,
    address: b.address,
    record_address: b.record_address,
    certificate: b.identity.certificate(),
  };
  let peer_of_b = Peer {
    anchor: a.origin_anchor,
    host: a.host,
    address: a.address,
    record_address: a.record_address,
    certificate: a.identity.certificate(),
  };
  let daemon_a = start(a, peer_of_a);
  let daemon_b = start(b, peer_of_b);
  let b_old = daemon_b
    .member_identity()
    .expect("B has its fresh identity");

  // Form (polled) + settle, then confirm A knows B under its previous id.
  assert!(
    form_and_settle(&[&daemon_a, &daemon_b]),
    "the fleet's direct probe mesh formed before the test acts"
  );
  let knew_old = daemon_a
    .fleet_members()
    .is_ok_and(|members| members.contains(&b_old));

  // B's old process ends (a restart's old incarnation is gone), then its return under a new generation is
  // injected into A: the old id dead, the new id alive.
  daemon_b.stop();
  let restarted = daemon_a.observe_peer_restart(b_old, b_new, FALSE_DEATH_INCARNATION);
  assert!(
    restarted.is_ok(),
    "the injected restart reached A's control shard and folded within the observe budget: {restarted:?}"
  );

  let admitted_new = poll_until(&[&daemon_a], REJOIN_DEADLINE, || {
    daemon_a
      .fleet_members()
      .map(|members| members.contains(&b_new))
  });
  let retired_old = poll_until(&[&daemon_a], RETIREMENT_DEADLINE, || {
    daemon_a
      .fleet_members()
      .map(|members| !members.contains(&b_old))
  });

  daemon_a.stop();
  assert!(
    knew_old,
    "A knew B under its previous member id before the restart"
  );
  assert!(
    admitted_new,
    "A admitted B's new member id — a restart is a join under a new id"
  );
  assert!(
    retired_old,
    "A retired B's old member id — the restart does not rejoin as its old self"
  );
}

/// Two identities from one certificate and key — what a node presents before and after a restart: the same
/// operator-provisioned certificate (its stable anchor), a fresh process (a new generation).
fn same_identity(count: usize) -> Vec<Identity> {
  let key = rcgen::KeyPair::generate().unwrap();
  let cert = rcgen::CertificateParams::new(vec![NAME.to_owned()])
    .unwrap()
    .self_signed(&key)
    .unwrap();
  let der = PrivateKeyDer::try_from(key.serialize_der()).unwrap();
  (0..count)
    .map(|_| Identity::from_der(cert.der().clone(), der.clone_key()))
    .collect()
}

/// One fleet node's configuration at `f = 1` on one shard: its `host` (the generation-0 seed the daemon
/// overrides with its fresh identity), its stable `anchor`, its `peers` by their seed ids, and the failure
/// `domains` the deployment declares by seed id (a node absent is unique-per-host).
fn fleet_config(
  profile: &MachineProfile,
  instance: &str,
  anchor: HostId,
  host: HostId,
  peers: &[FleetPeer],
  domains: &std::collections::BTreeMap<HostId, DomainId>,
) -> DaemonConfig {
  DaemonConfig::derive(profile, instance, Some(1)).with_fleet(FleetMembership {
    quorum: Quorum { f: 1 },
    peers: peers.iter().map(|peer| peer.host).collect(),
    host,
    origin_anchor: anchor,
    domains: domains.clone(),
    regions: std::collections::BTreeMap::new(),
    durability: None,
    region_mirrors: std::collections::BTreeMap::new(),
  })
}

/// An anchor segment with two recorded starts, exercising the handoff restart path.
/// The supervision count is diagnostic; it does not derive voting identity (AUD-07).
/// The returned segment must outlive its attached daemon.
fn incarnation_one_segment(
  name: &str,
  profile: &MachineProfile,
  geometry: Geometry,
  pid: u32,
) -> (SegmentSource, AnchorSegment) {
  let segment = AnchorSegment::create(name, &profile.facts.identity, geometry)
    .expect("the incarnation-one anchor segment");
  let supervision = segment
    .supervision()
    .expect("the segment's supervision block");
  supervision.record_start(u64::from(pid), 0, false);
  supervision.record_start(u64::from(pid), 0, true);
  let (handoff, len) = segment.handoff().expect("the segment hands off");
  (
    SegmentSource::Handoff {
      handoff,
      len,
      content: None,
    },
    segment,
  )
}

/// Starts one fleet daemon serving on its own `bind` pair and dialing each of `peers` where its entry says —
/// so a test may dial a peer at an address it will only serve later — over `source` (a fresh segment, or an
/// anchor segment a restart attaches).
fn start_fleet_node(
  profile: &MachineProfile,
  config: DaemonConfig,
  identity: Identity,
  bind: (u16, u16),
  peers: Vec<FleetPeer>,
  source: SegmentSource,
) -> Daemon {
  let transport = FleetTransport {
    identity,
    name: NAME.to_owned(),
    advertise: loopback(bind.0).into(),
    enrollment_roots: Vec::new(),
    probe_bind: loopback(bind.0),
    record_bind: loopback(bind.1),
    peers,
    resolver: None,
  };
  Daemon::start_with_fleet(profile, config, source, Some(transport))
    .expect("the fleet daemon starts")
}

/// AC-8.1, §4.8: restart a peer under its stable TLS certificate and a fresh member id.
/// The surviving quorum retires its previous id, admits the replacement with the declared
/// failure domain, and serves the predecessor's sealed content through takeover.
#[test]
fn a_restarted_peer_is_learned_on_contact_under_its_fresh_identity() {
  let _serial = serialize_fleet_tests();
  let pid = std::process::id();
  let mut fleet = restart_fleet_forms_and_seals(pid);
  let (daemon_b_again, segment) = restart_b(&mut fleet, pid);
  let observed: Vec<&Daemon> = vec![&fleet.survivors[0], &fleet.survivors[1], &daemon_b_again];
  let learned = observe_learned(&observed, &fleet, &daemon_b_again);
  let served_by = successor_serves(&observed, &fleet);
  // Rejoin design item 4 (2026-09-14): the returned node is a fresh member that holds nothing (R1: its
  // RAM is gone), so it must not serve — or claim — the volume its predecessor owned; the completed
  // takeover is undisturbed by the rejoin. Read once the successor serves, so the two views are
  // contemporaneous.
  let returned_serves_it =
    served_by.served && status_answers_once(&fleet.instance_b_again, fleet.id);

  let (formed, knew_old) = (fleet.formed, fleet.knew_old);
  daemon_b_again.stop();
  for daemon in fleet.survivors {
    daemon.stop();
  }
  drop(segment);
  assert_restart_learned(formed, knew_old, &learned);
  assert_successor_served(&served_by, &fleet.origin_owners);
  assert!(
    !returned_serves_it,
    "the restarted node holds nothing and does not serve its predecessor's volume — the takeover stands"
  );
}

/// Whether `status` for `volume` at `instance` answers with a report right now (one call, no polling).
fn status_answers_once(instance: &str, volume: VolumeId) -> bool {
  let mut client = Client::connect(instance);
  matches!(
    client.call(&RequestBody::Status { volume }),
    ReplyBody::Status { .. }
  )
}

/// The membership half of the restart scenario's verdict: the pre-restart facts and what the survivors
/// learned — each assertion named, so a failure says which step of learn-on-contact did not happen.
fn assert_restart_learned(formed: bool, knew_old: bool, learned: &Learned) {
  assert!(formed, "old B's mesh to A and C formed before it sealed");
  assert!(
    knew_old,
    "A knew B under its previous member id before the restart"
  );
  assert!(
    learned.admitted_new,
    "the survivors admitted B's fresh member id, learned from its own probes"
  );
  assert!(
    learned.retired_old,
    "the survivors retired B's old member id — a restart does not rejoin as its old self"
  );
  assert!(
    learned.committed,
    "the council committed the admission and the takeover into the regional configuration"
  );
  assert!(
    learned.meshed_to_new,
    "the probe mesh formed to the restarted node under its new id — it is probed, not merely believed"
  );
  assert!(
    learned.domain_carried,
    "the admission carried B's declared failure domain to its new id — the domain is the node's, not the incarnation's"
  );
}

/// Shape: the failure domain the restart scenario declares for node B (any id distinct from unique-per-host),
/// so the restarted node's new member id must be seen to inherit it through the council's admission.
const RESTARTED_NODE_DOMAIN: DomainId = 7;

/// The takeover half of the restart scenario's verdict: the old id's volume placed on, served by, and read
/// back from the survivor that took it over, with the origin's ownership reproduced.
fn assert_successor_served(served_by: &ServedBy, origin_owners: &Owners) {
  assert!(
    served_by.head_placed,
    "the survivor rendezvous ranked first took over the volume the old incarnation owned"
  );
  assert!(
    served_by.served,
    "the successor serves the taken-over volume"
  );
  assert_eq!(
    served_by.got.as_deref(),
    Some(CONTENT),
    "the file B sealed reads back byte for byte from the successor over NFS"
  );
  assert_eq!(
    served_by.owners.as_ref(),
    Some(origin_owners),
    "the successor's root and file carry the origin's mode and owner (the archive carries ownership, \
     format minor 2): [root, hello.txt] as (mode, uid, gid)"
  );
}

/// What the takeover successor showed: the head placed, `status` answering, the file's bytes, and the
/// root's and file's (mode, uid, gid).
struct ServedBy {
  head_placed: bool,
  served: bool,
  got: Option<Vec<u8>>,
  owners: Option<Owners>,
}

/// The restart scenario's fleet after old B has sealed its volume and ended: the two survivors (A, then C),
/// what the restart of B needs (its profile, anchor, seed, second identity, peers and serve pair), the ids
/// and instances the assertions read, the sealed volume, and the two facts observed before the restart.
struct RestartFleet {
  survivors: Vec<Daemon>,
  host_a: HostId,
  host_c: HostId,
  host_b: HostId,
  anchor_b: HostId,
  profile_b: MachineProfile,
  /// The restart's identity (B's certificate and key), taken by [`restart_b`] — an `Identity` holds a
  /// private key and is not `Clone`, so it moves.
  b_again: Option<Identity>,
  b_again_serve: (u16, u16),
  /// The restart's peer entries, taken by [`restart_b`].
  peers_of_b_again: Vec<FleetPeer>,
  /// The failure domains the deployment declares, by seed id: B's node in [`RESTARTED_NODE_DOMAIN`].
  domains: std::collections::BTreeMap<HostId, DomainId>,
  instance_a: String,
  instance_c: String,
  instance_b_again: String,
  id: VolumeId,
  object: ObjectId,
  name: String,
  formed: bool,
  knew_old: bool,
  /// The root's and `hello.txt`'s (mode, uid, gid) as the origin served them before it died.
  origin_owners: Owners,
}

/// A peer entry dialed at `at`, pinned to `certificate`, known by its `anchor` and seed `host`.
fn fleet_peer_at(
  anchor: HostId,
  host: HostId,
  at: (u16, u16),
  certificate: &rustls::pki_types::CertificateDer<'static>,
) -> FleetPeer {
  FleetPeer {
    anchor,
    host,
    address: loopback(at.0).into(),
    record_address: loopback(at.1).into(),
    certificate: certificate.clone(),
  }
}

/// Starts A, C and B with reachable discovery addresses, commits their initial voter set,
/// seals a volume on B, then ends B. Its replacement reuses the certificate and addresses
/// with a fresh member id. Address-change discovery is a separate KIND lane proof.
fn restart_fleet_forms_and_seals(pid: u32) -> RestartFleet {
  let (profile_a, host_a, identity_a) = fleet_node("a");
  let (profile_c, host_c, identity_c) = fleet_node("c");
  let (profile_b, host_b, _) = fleet_node("b");
  let mut b_identities = same_identity(2).into_iter();
  let (Some(b_first), Some(b_again)) = (b_identities.next(), b_identities.next()) else {
    panic!("B's identity twice");
  };
  let anchor_a = anchor_of(&profile_a);
  let anchor_b = anchor_of(&profile_b);
  let anchor_c = anchor_of(&profile_c);
  let serve = mesh_serve_ports(3);
  let b_again_serve = serve[1];
  let cert_a = identity_a.certificate();
  let cert_b = b_first.certificate();
  let cert_c = identity_c.certificate();
  let peers_of_a = vec![
    fleet_peer_at(anchor_b, host_b, b_again_serve, &cert_b),
    fleet_peer_at(anchor_c, host_c, serve[2], &cert_c),
  ];
  let peers_of_c = vec![
    fleet_peer_at(anchor_a, host_a, serve[0], &cert_a),
    fleet_peer_at(anchor_b, host_b, b_again_serve, &cert_b),
  ];
  let peers_of_b = vec![
    fleet_peer_at(anchor_a, host_a, serve[0], &cert_a),
    fleet_peer_at(anchor_c, host_c, serve[2], &cert_c),
  ];
  let peers_of_b_again = vec![
    fleet_peer_at(anchor_a, host_a, serve[0], &cert_a),
    fleet_peer_at(anchor_c, host_c, serve[2], &cert_c),
  ];
  let instance_a = format!("fleet3-{}-{pid}", host_a.0);
  let instance_b = format!("fleet3-{}-{pid}", host_b.0);
  let instance_c = format!("fleet3-{}-{pid}", host_c.0);
  let instance_b_again = format!("fleet3-again-{}-{pid}", host_b.0);
  // The deployment declares B's node in a failure domain (by its seed id, as a manifest does); the restart's
  // new id must inherit it through its admission.
  let domains: std::collections::BTreeMap<HostId, DomainId> =
    [(host_b, RESTARTED_NODE_DOMAIN)].into_iter().collect();
  let config_a = fleet_config(
    &profile_a,
    &instance_a,
    anchor_a,
    host_a,
    &peers_of_a,
    &domains,
  );
  let config_b = fleet_config(
    &profile_b,
    &instance_b,
    anchor_b,
    host_b,
    &peers_of_b,
    &domains,
  );
  let config_c = fleet_config(
    &profile_c,
    &instance_c,
    anchor_c,
    host_c,
    &peers_of_c,
    &domains,
  );
  let fresh = |host: HostId| SegmentSource::Create {
    name: format!("slates-seg-fleet3-{}-{pid}", host.0),
  };
  let daemon_a = start_fleet_node(
    &profile_a,
    config_a,
    identity_a,
    serve[0],
    peers_of_a,
    fresh(host_a),
  );
  let daemon_c = start_fleet_node(
    &profile_c,
    config_c,
    identity_c,
    serve[2],
    peers_of_c,
    fresh(host_c),
  );
  let daemon_b = start_fleet_node(
    &profile_b,
    config_b,
    b_first,
    serve[1],
    peers_of_b,
    fresh(host_b),
  );
  let host_a = daemon_a.member_identity().expect("A's live member");
  let host_c = daemon_c.member_identity().expect("C's live member");
  let host_b = daemon_b.member_identity().expect("B's live member");
  daemon_b.bootstrap(true).expect("explicit initial group");
  // Old B dials A and C, so its mesh forms; then it seals the volume the restart's takeover will move.
  let mut daemons = vec![daemon_b, daemon_a, daemon_c];
  let formed = poll_until(&[&daemons[0]], FORMATION_DEADLINE, || {
    daemons[0].fleet_meshed()
  });
  let voters = vec![host_b, host_a, host_c];
  assert!(
    audit_wait(|| audit_voters_match(&daemons, &voters)),
    "initial voters commit before sealing and restart"
  );
  let knew_old = daemons[1]
    .fleet_members()
    .is_ok_and(|members| members.contains(&host_b));
  let name = format!("restarted-{pid}");
  let id = match seal_hello_on_owner(&instance_b, &daemons, &name) {
    Ok(id) => id,
    Err(why) => {
      for daemon in daemons {
        daemon.stop();
      }
      panic!("setup: formed={formed}, {why}");
    }
  };
  let origin_owners = owners_over_nfs(&daemons[0], &name);
  let old_b = daemons.remove(0);
  old_b.stop();
  RestartFleet {
    survivors: daemons,
    origin_owners,
    host_a,
    host_c,
    host_b,
    anchor_b,
    profile_b,
    b_again: Some(b_again),
    b_again_serve,
    peers_of_b_again,
    domains,
    instance_a,
    instance_c,
    instance_b_again,
    id,
    object: ObjectId(id.bytes),
    name,
    formed,
    knew_old,
  }
}

/// Restarts B over a handed-off anchor segment and observes its fresh member identity.
/// The segment is returned so it outlives the daemon.
fn restart_b(fleet: &mut RestartFleet, pid: u32) -> (Daemon, AnchorSegment) {
  let peers = std::mem::take(&mut fleet.peers_of_b_again);
  let identity = fleet
    .b_again
    .take()
    .expect("the restart's identity is taken once");
  let config = fleet_config(
    &fleet.profile_b,
    &fleet.instance_b_again,
    fleet.anchor_b,
    fleet.host_b,
    &peers,
    &fleet.domains,
  );
  let (source, segment) = incarnation_one_segment(
    &format!("slates-seg-fleet3-again-{}-{pid}", fleet.host_b.0),
    &fleet.profile_b,
    config.geometry,
    pid,
  );
  let daemon = start_fleet_node(
    &fleet.profile_b,
    config,
    identity,
    fleet.b_again_serve,
    peers,
    source,
  );
  assert_ne!(
    daemon
      .member_identity()
      .expect("the restarted daemon reports its member id"),
    fleet.host_b
  );
  (daemon, segment)
}

/// What the survivors learned of the restart on contact.
struct Learned {
  admitted_new: bool,
  retired_old: bool,
  committed: bool,
  meshed_to_new: bool,
  domain_carried: bool,
}

/// Whether every survivor's observed alive membership satisfies `predicate` (an unobservable one is its
/// refusal, never a "no").
fn all_members(
  survivors: &[Daemon],
  predicate: impl Fn(&[HostId]) -> bool,
) -> Result<bool, ObserveError> {
  all_hold(
    survivors
      .iter()
      .map(|daemon| daemon.fleet_members().map(|members| predicate(&members))),
  )
}

/// Whether every survivor's observed committed regional membership satisfies `predicate`.
fn all_council(
  survivors: &[Daemon],
  predicate: impl Fn(&[HostId]) -> bool,
) -> Result<bool, ObserveError> {
  all_hold(
    survivors
      .iter()
      .map(|daemon| daemon.council_members().map(|members| predicate(&members))),
  )
}

/// Whether every survivor's observed committed failure-domain map satisfies `predicate`.
fn all_domains(
  survivors: &[Daemon],
  predicate: impl Fn(&std::collections::BTreeMap<HostId, DomainId>) -> bool,
) -> Result<bool, ObserveError> {
  all_hold(
    survivors
      .iter()
      .map(|daemon| daemon.council_domains().map(|domains| predicate(&domains))),
  )
}

/// Polls the survivors until they learn the restart: the new id admitted, the old retired, both committed into
/// the regional configuration, the probe mesh formed to the new incarnation on every side, and the node's
/// declared failure domain carried to the new id by its admission.
fn observe_learned(observed: &[&Daemon], fleet: &RestartFleet, b_again: &Daemon) -> Learned {
  let (host_b, b_new) = (
    fleet.host_b,
    b_again
      .member_identity()
      .expect("the replacement has its fresh member id"),
  );
  let survivors = &fleet.survivors;
  let admitted_new = poll_until(observed, REJOIN_DEADLINE, || {
    all_members(survivors, |members| members.contains(&b_new))
  });
  let retired_old = poll_until(observed, RETIREMENT_DEADLINE, || {
    all_members(survivors, |members| !members.contains(&host_b))
  });
  let committed = poll_until(observed, COUNCIL_RETIRE_DEADLINE, || {
    all_council(survivors, |members| {
      members.contains(&b_new) && !members.contains(&host_b)
    })
  });
  let meshed_to_new = poll_until(observed, COUNCIL_SETTLE_DEADLINE, || {
    all_hold(
      survivors
        .iter()
        .map(|daemon| daemon.fleet_meshed())
        .chain([b_again.fleet_meshed()]),
    )
  });
  let domain_carried = poll_until(observed, COUNCIL_RETIRE_DEADLINE, || {
    all_domains(survivors, |domains| {
      domains.get(&b_new) == Some(&RESTARTED_NODE_DOMAIN)
    })
  });
  Learned {
    admitted_new,
    retired_old,
    committed,
    meshed_to_new,
    domain_carried,
  }
}

/// The old id's volume is taken over by the survivor rendezvous ranks first, which must serve it: whether the
/// head placed there, whether `status` answers there, and the file read back over its NFS port.
fn successor_serves(observed: &[&Daemon], fleet: &RestartFleet) -> ServedBy {
  let successor =
    rendezvous_first(&[fleet.host_a, fleet.host_c], fleet.object).expect("a survivor takes over");
  let (daemon, instance) = if successor == fleet.host_a {
    (&fleet.survivors[0], &fleet.instance_a)
  } else {
    (&fleet.survivors[1], &fleet.instance_c)
  };
  let head_placed = poll_head_placed(observed, daemon, fleet.object);
  let served = poll_status_answers(observed, instance, fleet.id);
  let got = served.then(|| read_hello_over_nfs(daemon, &fleet.name));
  let owners = served.then(|| owners_over_nfs(daemon, &fleet.name));
  ServedBy {
    head_placed,
    served,
    got,
    owners,
  }
}

/// The refusals the serve side counts for a membership announcement it will not fold (task #22), under the
/// keys `Daemon::fleet_refusals` reports them — the same counts `slates status` prints.
const FORGED_ID_REFUSAL: &str = "fleet.member_id_forged";

/// Shape: the anchor the forged announcer is configured with — B's anchor with its lowest bit flipped, any
/// anchor other than the one A's roster holds for B's certificate.
fn forged_anchor_of(anchor: HostId) -> HostId {
  HostId(anchor.0 ^ 1)
}

/// AC-8.1, §4.8: a member announcement that does not derive from its authenticated
/// certificate anchor is refused, counted, and never admitted to discovery membership.
#[test]
fn a_forged_announcement_is_refused_and_counted() {
  let _serial = serialize_fleet_tests();
  let pid = std::process::id();
  let mut fleet = refusal_fleet_forms(pid);
  let (host_a, b_new, anchor_b) = (fleet.host_a, fleet.b_new, fleet.anchor_b);
  let learned_new = poll_until(&[&fleet.daemon_a, &fleet.daemon_b], REJOIN_DEADLINE, || {
    Ok(knows_exactly(&fleet.daemon_a, &[host_a, b_new])? && fleet.daemon_a.fleet_meshed()?)
  });
  let forged = announce_as_b(&mut fleet, forged_anchor_of(anchor_b), pid);
  let refused = observe_refused(&fleet, &forged);
  forged.stop();
  fleet.daemon_b.stop();
  fleet.daemon_a.stop();
  drop(fleet.segment_b);
  assert!(
    learned_new,
    "A learned B's fresh id on contact and its mesh formed to it"
  );
  assert_refused(&refused);
}

/// The refusal scenario's fleet: A and B under their observed fresh ids running, and what the two announcers that
/// present B's certificate need — its profile, two more copies of its identity, the entry for A they dial,
/// and the serve pairs they bind.
struct RefusalFleet {
  daemon_a: Daemon,
  daemon_b: Daemon,
  /// B's incarnation-one segment (the anchor recorded a restart on it), which outlives its daemon.
  segment_b: AnchorSegment,
  host_a: HostId,
  anchor_a: HostId,
  b_new: HostId,
  anchor_b: HostId,
  profile_b: MachineProfile,
  cert_a: rustls::pki_types::CertificateDer<'static>,
  serve_a: (u16, u16),
  /// B's identity twice more — one per announcer, taken in turn.
  b_identities: Vec<Identity>,
  /// The serve pairs the announcers bind, taken in turn.
  announcer_serve: Vec<(u16, u16)>,
}

/// Starts A on a fresh segment and B on an incarnation-one anchor segment (a recorded restart), each dialing
/// the other.
fn refusal_fleet_forms(pid: u32) -> RefusalFleet {
  let (profile_a, host_a, identity_a) = fleet_node("a");
  let (profile_b, host_b, _) = fleet_node("b");
  let mut b_identities = same_identity(3);
  let identity_b = b_identities.pop().expect("B's identity three times");
  let anchor_a = anchor_of(&profile_a);
  let anchor_b = anchor_of(&profile_b);
  let serve = mesh_serve_ports(4);
  let cert_a = identity_a.certificate();
  let cert_b = identity_b.certificate();
  let peers_of_a = vec![fleet_peer_at(anchor_b, host_b, serve[1], &cert_b)];
  let peers_of_b = vec![fleet_peer_at(anchor_a, host_a, serve[0], &cert_a)];
  let no_domains = std::collections::BTreeMap::new();
  let instance_a = format!("refusal-{}-{pid}", host_a.0);
  let instance_b = format!("refusal-{}-{pid}", host_b.0);
  let config_a = fleet_config(
    &profile_a,
    &instance_a,
    anchor_a,
    host_a,
    &peers_of_a,
    &no_domains,
  );
  let config_b = fleet_config(
    &profile_b,
    &instance_b,
    anchor_b,
    host_b,
    &peers_of_b,
    &no_domains,
  );
  let daemon_a = start_fleet_node(
    &profile_a,
    config_a,
    identity_a,
    serve[0],
    peers_of_a,
    SegmentSource::Create {
      name: format!("slates-seg-{instance_a}"),
    },
  );
  let (source_b, segment_b) = incarnation_one_segment(
    &format!("slates-seg-{instance_b}"),
    &profile_b,
    config_b.geometry,
    pid,
  );
  let daemon_b = start_fleet_node(
    &profile_b, config_b, identity_b, serve[1], peers_of_b, source_b,
  );
  let host_a = daemon_a.member_identity().expect("A's live member");
  let b_new = daemon_b.member_identity().expect("B's live member");
  daemon_a.bootstrap(true).expect("explicit initial group");
  RefusalFleet {
    daemon_a,
    daemon_b,
    segment_b,
    host_a,
    anchor_a,
    b_new,
    anchor_b,
    profile_b,
    cert_a,
    serve_a: serve[0],
    b_identities,
    announcer_serve: vec![serve[2], serve[3]],
  }
}

/// Presents B's certificate with a different configured anchor. The resulting member id
/// cannot derive from B's authenticated identity and must be refused.
fn announce_as_b(fleet: &mut RefusalFleet, anchor: HostId, pid: u32) -> Daemon {
  let host = member_id(anchor, 0);
  let identity = fleet.b_identities.pop().expect("an identity per announcer");
  let serve = fleet
    .announcer_serve
    .pop()
    .expect("a serve pair per announcer");
  let instance = format!("refusal-{}-{pid}", host.0);
  let peers = vec![fleet_peer_at(
    fleet.anchor_a,
    fleet.host_a,
    fleet.serve_a,
    &fleet.cert_a,
  )];
  let config = fleet_config(
    &fleet.profile_b,
    &instance,
    anchor,
    host,
    &peers,
    &std::collections::BTreeMap::new(),
  );
  start_fleet_node(
    &fleet.profile_b,
    config,
    identity,
    serve,
    peers,
    SegmentSource::Create {
      name: format!("slates-seg-{instance}"),
    },
  )
}

/// Whether `daemon`'s observed alive membership is exactly `expected` (an unobservable one is its refusal).
fn knows_exactly(daemon: &Daemon, expected: &[HostId]) -> Result<bool, ObserveError> {
  daemon.fleet_members().map(|members| {
    members.len() == expected.len() && expected.iter().all(|host| members.contains(host))
  })
}

/// The count A's control shard reports for one refusal `kind` — an absent key is the only zero; an
/// unobservable map is its refusal, so a poll on it waits.
fn refusal_count(daemon: &Daemon, kind: &str) -> Result<u64, ObserveError> {
  daemon
    .fleet_refusals()
    .map(|refusals| refusals.get(kind).copied().unwrap_or(0))
}

/// What A and the two announcers showed once both had announced.
struct Refused {
  forged_counted: bool,
  membership_held: bool,
  forged_unanswered: bool,
}

/// Polls A until both refusals are counted, then holds its membership over a settle window, and waits for
/// each announcer — never acknowledged — to age A to death in its own view.
fn observe_refused(fleet: &RefusalFleet, forged: &Daemon) -> Refused {
  let a = &fleet.daemon_a;
  let observed: Vec<&Daemon> = vec![a, &fleet.daemon_b, forged];
  let (host_a, b_new) = (fleet.host_a, fleet.b_new);
  let forged_counted = poll_until(&observed, REJOIN_DEADLINE, || {
    refusal_count(a, FORGED_ID_REFUSAL).map(|count| count >= 1)
  });
  let membership_held = holds_for(FORMATION_SETTLE, || knows_exactly(a, &[host_a, b_new]));
  let aged_a = |announcer: &Daemon| {
    poll_until(&observed, RETIREMENT_DEADLINE, || {
      announcer
        .fleet_members()
        .map(|members| !members.contains(&host_a))
    })
  };
  let forged_unanswered = aged_a(forged);
  Refused {
    forged_counted,
    membership_held,
    forged_unanswered,
  }
}

/// The refusal scenario's verdict, each assertion named so a failure says which part of the refusal did not
/// happen.
fn assert_refused(refused: &Refused) {
  assert!(
    refused.forged_counted,
    "A counted the announcement from the wrong anchor as `fleet.member_id_forged`"
  );
  assert!(
    refused.membership_held,
    "A's alive membership still names only A and B; the forged id was never folded"
  );
  assert!(
    refused.forged_unanswered,
    "the forged announcer was never acknowledged — A aged to death in its view"
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
  // Return the routing seed; callers observe the actual id after starting the daemon.
  let host = member_id(anchor_of(&profile), 0);
  (profile, host, self_signed())
}

/// The stable anchor of a test node — the machine-identity hash the daemon uses as a laptop's anchor and from
/// which it derives its runtime member id `member_id(origin_anchor, boot_nonce)` (task #22).
fn anchor_of(profile: &MachineProfile) -> HostId {
  HostId(host_id_of(&profile.facts.identity))
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
  // Each node's stable anchor (the machine-identity hash its member ids derive from, task #22), so every
  // peer entry names the anchor the announced ids are validated against.
  let anchors: Vec<HostId> = nodes
    .iter()
    .map(|(profile, _, _)| anchor_of(profile))
    .collect();
  let daemons: Vec<Daemon> = nodes
    .into_iter()
    .enumerate()
    .map(|(i, (profile, host, identity))| {
      let peers: Vec<FleetPeer> = (0..n)
        .filter(|&j| j != i)
        .map(|j| FleetPeer {
          anchor: anchors[j],
          host: hosts[j],
          address: loopback(serve[j].0).into(),
          record_address: loopback(serve[j].1).into(),
          certificate: certs[j].clone(),
        })
        .collect();
      let member_peers: Vec<HostId> = (0..n).filter(|&j| j != i).map(|j| hosts[j]).collect();
      let instance = format!("fleet3-{}-{pid}", host.0);
      let config =
        DaemonConfig::derive(&profile, &instance, Some(shards)).with_fleet(FleetMembership {
          quorum: Quorum { f },
          peers: member_peers,
          host,
          origin_anchor: anchor_of(&profile),
          domains: std::collections::BTreeMap::new(),
          regions: regions.clone(),
          durability: None,
          region_mirrors: mirrors.clone(),
        });
      let transport = FleetTransport {
        identity,
        name: NAME.to_owned(),
        advertise: loopback(serve[i].0).into(),
        enrollment_roots: Vec::new(),
        probe_bind: loopback(serve[i].0),
        record_bind: loopback(serve[i].1),
        peers,
        resolver: None,
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
    .collect();
  bootstrap_mesh(&daemons, hosts, regions);
  settle_initial_consensus(&daemons, hosts, regions, Quorum { f });
  daemons
}

/// A new test deployment explicitly creates the root and one council per declared region.
/// Every remaining member joins those groups over the transport.
fn bootstrap_mesh(
  daemons: &[Daemon],
  seeds: &[HostId],
  regions: &std::collections::BTreeMap<HostId, RegionId>,
) {
  daemons[0]
    .bootstrap(true)
    .expect("explicit first-time root bootstrap");
  let mut bootstrapped =
    std::collections::BTreeSet::from([regions.get(&seeds[0]).copied().unwrap_or(RegionId(0))]);
  for (daemon, seed) in daemons.iter().zip(seeds).skip(1) {
    let region = regions.get(seed).copied().unwrap_or(RegionId(0));
    if bootstrapped.insert(region) {
      assert!(
        audit_wait(|| daemon
          .root_regions()
          .map(|regions| regions.contains(&region))),
        "the root admits the new region before its regional bootstrap"
      );
      daemon
        .bootstrap(false)
        .expect("explicit first-time regional bootstrap");
    }
  }
}

/// Initial membership is committed before a fixture may remove any voter. Probe formation
/// alone cannot establish this precondition (§4.8, AUD-07).
fn settle_initial_consensus(
  daemons: &[Daemon],
  seeds: &[HostId],
  regions: &std::collections::BTreeMap<HostId, RegionId>,
  quorum: Quorum,
) {
  let assigned: Vec<RegionId> = seeds
    .iter()
    .map(|seed| regions.get(seed).copied().unwrap_or(RegionId(0)))
    .collect();
  let mut regional = std::collections::BTreeMap::<RegionId, Vec<HostId>>::new();
  for (daemon, region) in daemons.iter().zip(&assigned) {
    regional
      .entry(*region)
      .or_default()
      .push(daemon.member_identity().unwrap());
  }
  for members in regional.values_mut() {
    members.sort_unstable();
  }
  let root_voters: Vec<HostId> = regional.values().map(|members| members[0]).collect();
  let settled = audit_wait(|| {
    all_hold(daemons.iter().zip(&assigned).map(|(daemon, region)| {
      let members = &regional[region];
      let voters = slates_cluster::config_group::council_voters(members, quorum);
      initial_member_committed(daemon, members, &voters, &root_voters)
    }))
  });
  assert!(
    settled,
    "initial membership did not commit: expected regions={regional:?} root={root_voters:?}; {}",
    daemons
      .iter()
      .map(|daemon| format!(
        "host={:?} council={:?} voters={:?} root={:?} leaders={:?}/{:?}",
        daemon.member_identity(),
        daemon.council_members(),
        daemon.council_voters(),
        daemon.root_voters(),
        daemon.council_leads(),
        daemon.root_leads()
      ))
      .collect::<Vec<_>>()
      .join("; ")
  );
}

fn initial_member_committed(
  daemon: &Daemon,
  members: &[HostId],
  voters: &[HostId],
  root_voters: &[HostId],
) -> Result<bool, ObserveError> {
  let host = daemon.member_identity()?;
  let members_known = daemon.council_members()? == members;
  let voters_known = !voters.contains(&host)
    || daemon
      .council_voters()?
      .is_some_and(|known| same_voters(&known, voters));
  let root_known = !root_voters.contains(&host)
    || daemon
      .root_voters()?
      .is_some_and(|known| same_voters(&known, root_voters));
  Ok(members_known && voters_known && root_known)
}

fn same_voters(known: &[HostId], expected: &[HostId]) -> bool {
  known.len() == expected.len() && expected.iter().all(|voter| known.contains(voter))
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

  // Poll until every node's full direct mesh has formed (all six probe sessions established). Robust to CPU
  // load: `poll_until` charges the fleet's own forward progress, not wall-clock (see its docs).
  poll_until(
    &daemons.iter().collect::<Vec<_>>(),
    FORMATION_DEADLINE,
    || all_hold(daemons.iter().map(|daemon| daemon.fleet_meshed())),
  );
  // A refusal names a node that could not be observed at all, and the stage it reached — shown as such in
  // the failure, never counted as meshed.
  let meshed: Vec<(&str, Result<bool, ObserveError>)> = names
    .iter()
    .copied()
    .zip(daemons.iter().map(Daemon::fleet_meshed))
    .collect();
  let all_meshed = meshed.iter().all(|(_, m)| *m == Ok(true));
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
  let hosts: Vec<HostId> = daemons
    .iter()
    .map(|daemon| daemon.member_identity().unwrap())
    .collect();

  // The council elects only over live record sessions, so wait for the direct mesh first (the record links
  // come up alongside the probe mesh), then for exactly one leader to emerge over the transport.
  assert_fleet_forms(&daemons, &hosts, &names);
  let observed: Vec<&Daemon> = daemons.iter().collect();
  let elected = poll_until(&observed, COUNCIL_ELECTION_DEADLINE, || {
    one_leader(&daemons)
  });
  // The council settles on a single leader: within the deadline there is a window it holds unbroken. A
  // legitimate re-election under a starved heartbeat is tolerated (pre-vote cannot forbid one when a leader
  // is genuinely unreachable), so the settle is retried until the council quiesces.
  let settled = elected
    && poll_until(&observed, COUNCIL_SETTLE_DEADLINE, || {
      Ok(holds_for(COUNCIL_STABILITY_WINDOW, || one_leader(&daemons)))
    });
  // The leader's heartbeats must reach the followers over the transport: a follower's leader-contact climbs
  // past the baseline just captured (a non-vacuity counter — replication is live, not just an election).
  let baseline: Vec<Result<u64, ObserveError>> =
    daemons.iter().map(Daemon::council_contact).collect();
  let heartbeats_flow = settled
    && poll_until(&observed, COUNCIL_HEARTBEAT_WINDOW, || {
      any_holds(daemons.iter().zip(baseline.iter()).map(|(daemon, base)| {
        let follower = !daemon.council_leads()?;
        let now = daemon.council_contact()?;
        Ok(follower && base.as_ref().is_ok_and(|&base| now > base))
      }))
    });

  // Stop the daemons before asserting, so a failure leaves none running.
  let leads: Vec<Result<bool, ObserveError>> = daemons.iter().map(Daemon::council_leads).collect();
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

/// AC (§4.8 "Derived constants"; R8): on a loopback fleet the council's election timing is **derived at
/// the floor** — every measured voter path sits inside one heartbeat, so the base and span are the ten
/// periods the daemon always ran — and it is measured, not defaulted: each node's timing carries the round
/// trips its probes and consensus rounds sampled (the non-vacuity witness), read through the daemon's
/// observation accessor over the real loopback transport. The WAN case of the same law is proven on the
/// fabric (`crates/cluster/tests/wan_election.rs`); this is the differential that a laptop or LAN fleet is
/// unchanged by construction. The tail each node measured is in the failure message, so a loopback round
/// trip that outgrew a heartbeat under load reads as the measurement it is.
#[test]
fn a_loopback_fleet_derives_its_election_timing_at_the_measured_floor() {
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
  let hosts: Vec<HostId> = daemons
    .iter()
    .map(|daemon| daemon.member_identity().unwrap())
    .collect();

  assert_fleet_forms(&daemons, &hosts, &names);
  let observed: Vec<&Daemon> = daemons.iter().collect();
  let elected = poll_until(&observed, COUNCIL_ELECTION_DEADLINE, || {
    one_leader(&daemons)
  });
  // Every node has sampled its voter paths — its probes are acknowledged each period, and the leader's
  // rounds answered — so its timing is a measurement, not the default.
  let measured = elected
    && poll_until(&observed, COUNCIL_HEARTBEAT_WINDOW, || {
      all_hold(
        daemons
          .iter()
          .map(|d| d.council_timing().map(|timing| timing.samples > 0)),
      )
    });
  let timings: Vec<Result<slates_cluster::timing::ElectionTiming, ObserveError>> =
    daemons.iter().map(Daemon::council_timing).collect();
  let reported = council_reports_over_the_wire(&daemons);
  for daemon in daemons {
    daemon.stop();
  }
  assert!(elected, "the council elected one leader over loopback");
  assert!(
    measured,
    "every node's council timing was derived from measured round trips: {timings:?}"
  );
  let floor = slates_cluster::timing::ElectionTiming::floor();
  for timing in timings.iter().flatten() {
    assert!(
      timing.broadcast_rtt_tail_ns < slates_server::daemon::HEARTBEAT_NS,
      "a loopback voter path's tail is inside one heartbeat: {timing:?}"
    );
    assert_eq!(
      (timing.base_periods, timing.span_periods),
      (floor.base_periods, floor.span_periods),
      "the derived timing is the floor on a loopback fleet: {timing:?}"
    );
  }
  assert_status_carries_the_council(&reported, &floor);
}

/// The council block of every mesh node's `DaemonStatus`, read over the wire — what an operator's
/// `slates status` prints (`fleet_council_*`): each node's control shard's council leadership and derived
/// timing, so a status read on a pod reports what the in-process accessor reports.
fn council_reports_over_the_wire(daemons: &[Daemon]) -> Vec<slates_ipc::protocol::GroupReport> {
  daemons
    .iter()
    .map(|daemon| {
      let mut client = Client::connect(daemon.instance());
      match client.call(&RequestBody::DaemonStatus) {
        ReplyBody::DaemonStatus { report } => report.fleet.council.clone(),
        other => panic!("status answers on every node: {other:?}"),
      }
    })
    .collect()
}

/// Exactly one node reports itself the leader over the wire, and every node's status carries measured
/// samples and the derived timing at `floor`.
fn assert_status_carries_the_council(
  reported: &[slates_ipc::protocol::GroupReport],
  floor: &slates_cluster::timing::ElectionTiming,
) {
  assert_eq!(
    reported.iter().filter(|group| group.leads).count(),
    1,
    "exactly one node reports itself the council's leader over the wire: {reported:?}"
  );
  for group in reported {
    assert!(
      group.samples > 0,
      "the status carries the measured samples, not the default: {reported:?}"
    );
    assert_eq!(
      (group.base_periods, group.span_periods),
      (floor.base_periods, floor.span_periods),
      "the status carries the derived timing: {reported:?}"
    );
  }
}

/// AC (§4.8, D-14): the configuration council **commits a membership change over the real transport**, not
/// just an election. Three daemons elect a leader; when a **follower** dies, the leader — once the death is in
/// its SWIM view — proposes the retirement through the council log (`reconcile_alive`), and it commits at the
/// surviving majority (2 of 3 voters) and applies on every survivor, so the dead member drops from each
/// survivor's `RegionalConfiguration`. This is the transport-driven form of the propose→replicate→commit→apply
/// path the sans-io council (`config_group.rs`) and sim-UDP proof (`config_group_live.rs`) show; here the whole
/// path runs through the daemon's own record sessions and demux. A follower is killed, not the leader, so the
/// leader stays and reconciles (killing the leader would first force a re-election — a separate concern); the
/// leader keeping quorum is what lets the retire commit. The killed host's death is **injected** into the
/// survivors (`observe_peer_dead`) rather than waited on — real SWIM detection of a killed node is slow under
/// the accumulated suite load and is covered by `a_daemon_detects_its_dead_peer_over_the_transport_and_retires_it`;
/// here the transport commit drive is the subject.
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
  let hosts: Vec<HostId> = daemons
    .iter()
    .map(|daemon| daemon.member_identity().unwrap())
    .collect();

  assert_fleet_forms(&daemons, &hosts, &names);
  // Wait for the council to elect one leader, then kill a *follower* so the leader stays and reconciles.
  let elected = poll_until(
    &daemons.iter().collect::<Vec<_>>(),
    COUNCIL_ELECTION_DEADLINE,
    || one_leader(&daemons),
  );
  let leader_idx = leader_index(&daemons);
  let victim_idx = leader_idx.and_then(|lead| (0..daemons.len()).find(|&i| i != lead));

  let retired = match (elected, victim_idx) {
    (true, Some(victim)) => {
      let dead = hosts[victim];
      daemons.remove(victim).stop();
      // Inject the victim's death into every survivor's SWIM view (the same `Dead` fold the detector produces),
      // rather than wait on real detection under the accumulated suite load. The surviving leader then proposes
      // the retire and commits it over the transport; every survivor drops the dead member from BOTH its
      // committed regional membership (`council_members`) AND the neighbourhood it actually places under
      // (`placement_neighbourhood`, installed from the council) — the authority switchover: the council's
      // committed configuration is what placement reads, so the commit reaches the placement path, not just the
      // council's own state. The injection is only the deterministic detection cue; the commit drive is the
      // subject (real detection→retirement is covered by
      // `a_daemon_detects_its_dead_peer_over_the_transport_and_retires_it`).
      inject_death_into(&daemons, dead);
      poll_until(
        &daemons.iter().collect::<Vec<_>>(),
        COUNCIL_RETIRE_DEADLINE,
        || {
          all_hold(daemons.iter().map(|daemon| {
            Ok(
              !daemon.council_members()?.contains(&dead)
                && !daemon.placement_neighbourhood()?.contains(&dead),
            )
          }))
        },
      )
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
  let hosts: Vec<HostId> = daemons
    .iter()
    .map(|daemon| daemon.member_identity().unwrap())
    .collect();

  assert_fleet_forms(&daemons, &hosts, &names);
  let elected = poll_until(
    &daemons.iter().collect::<Vec<_>>(),
    COUNCIL_ELECTION_DEADLINE,
    || one_leader(&daemons),
  );
  let leader_idx = leader_index(&daemons);
  let victim_idx = leader_idx.and_then(|lead| (0..daemons.len()).find(|&i| i != lead));

  let (on_control_shard, on_non_control_shard) = match (elected, victim_idx) {
    (true, Some(victim)) => {
      let dead = hosts[victim];
      daemons.remove(victim).stop();
      inject_death_into(&daemons, dead);
      // The leader reconciles the injected death, commits the retirement, and every survivor's control shard
      // drops the dead member from the neighbourhood it places under...
      let on_control = poll_until(
        &daemons.iter().collect::<Vec<_>>(),
        COUNCIL_RETIRE_DEADLINE,
        || {
          all_hold(daemons.iter().map(|daemon| {
            daemon
              .placement_neighbourhood()
              .map(|hood| !hood.contains(&dead))
          }))
        },
      );
      // ...and so does the non-control shard — the fan-out under test.
      let on_non_control = on_control
        && poll_until(
          &daemons.iter().collect::<Vec<_>>(),
          COUNCIL_RETIRE_DEADLINE,
          || {
            all_hold(daemons.iter().map(|daemon| {
              daemon
                .placement_neighbourhood_on_shard(1)
                .map(|hood| !hood.contains(&dead))
            }))
          },
        );
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
/// leader — once it sees the lost region has no alive host — proposes the region's retirement and commits it
/// over the transport, so every survivor's committed root region membership (`root_regions`) drops the lost
/// region. The whole path runs through the daemon's own record sessions and demultiplexer on [`ROOT_STREAM`],
/// the cross-region counterpart of the regional council's commit path. A follower is killed, not the root
/// leader, so the leader stays and reconciles (killing the leader would force a re-election first — a separate
/// concern). The killed host's death is **injected** into the survivors (`observe_peer_dead`, the same `Dead`
/// fold the detector produces) rather than waited on: real SWIM detection of a killed node is slow under the
/// accumulated suite load and orthogonal to what this proves — the root group's retirement **commit drive** —
/// so the learner and rejoin tests inject for the same reason, and real detection→retirement is covered by
/// `three_daemons_form_a_fleet_and_the_survivors_retire_a_dead_node`. The injection is only the deterministic
/// detection cue; the reconcile, the `ROOT_STREAM` replication, and the committed drop are the subject.
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
  let hosts: Vec<HostId> = daemons
    .iter()
    .map(|daemon| daemon.member_identity().unwrap())
    .collect();

  assert_fleet_forms(&daemons, &hosts, &names);
  // Wait for the root group to elect one leader, then kill a *follower* so the leader stays and reconciles.
  let elected = poll_until(
    &daemons.iter().collect::<Vec<_>>(),
    COUNCIL_ELECTION_DEADLINE,
    || one_root_leader(&daemons),
  );
  let leader_idx = daemons
    .iter()
    .position(|daemon| daemon.root_leads() == Ok(true));
  let victim_idx = leader_idx.and_then(|lead| (0..daemons.len()).find(|&i| i != lead));

  let retired = match (elected, victim_idx) {
    (true, Some(victim)) => {
      let lost = RegionId(u64::try_from(victim).unwrap_or(0));
      let victim_host = hosts[victim];
      daemons.remove(victim).stop();
      // Inject the victim's death into every survivor's SWIM view (the same `Dead` fold the detector produces),
      // rather than wait on real detection under the accumulated suite load. The surviving root leader then
      // sees the lost region has no alive host, proposes its retirement, and commits it over the transport;
      // every survivor drops the region from its committed root membership. The injection is only the
      // deterministic detection cue — the reconcile and the `ROOT_STREAM` commit drive are what this proves.
      inject_death_into(&daemons, victim_host);
      poll_until(
        &daemons.iter().collect::<Vec<_>>(),
        COUNCIL_RETIRE_DEADLINE,
        || {
          all_hold(daemons.iter().map(|daemon| {
            daemon
              .root_regions()
              .map(|regions| !regions.contains(&lost))
          }))
        },
      )
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
  let seeds = hosts;
  let hosts: Vec<HostId> = daemons
    .iter()
    .map(|daemon| daemon.member_identity().unwrap())
    .collect();
  let regions: std::collections::BTreeMap<HostId, RegionId> = seeds
    .iter()
    .zip(&hosts)
    .filter_map(|(seed, host)| regions.get(seed).map(|region| (*host, *region)))
    .collect();

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
  let elected = poll_until(
    &survivors.iter().map(|(_, d)| d).collect::<Vec<_>>(),
    COUNCIL_ELECTION_DEADLINE,
    || one_root_leader(survivors.iter().map(|(_, d)| d)),
  );
  let leader_host = survivors
    .iter()
    .find(|(_, daemon)| daemon.root_leads() == Ok(true))
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
      inject_death_into(survivors.iter().map(|(_, d)| d), victim);
      let learner_pos = survivors
        .iter()
        .position(|(host, _)| *host == learner_host)
        .expect("the learner survives");
      // The surviving root leader detects the death, commits the lost region's retirement over the transport;
      // the learner (a non-voter) drops it from its committed root membership only by fetching from a voter.
      poll_until(
        &survivors.iter().map(|(_, d)| d).collect::<Vec<_>>(),
        COUNCIL_RETIRE_DEADLINE,
        || {
          survivors[learner_pos]
            .1
            .root_regions()
            .map(|regions| !regions.contains(&lost))
        },
      )
    }
    _ => false,
  };

  // The learner never leads the root group — it is not a voter.
  let learner_leads = survivors
    .iter()
    .any(|(host, daemon)| *host == learner_host && daemon.root_leads() == Ok(true));

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
  let seeds = hosts;
  let hosts: Vec<HostId> = daemons
    .iter()
    .map(|daemon| daemon.member_identity().unwrap())
    .collect();
  let regions: std::collections::BTreeMap<HostId, RegionId> = seeds
    .iter()
    .zip(&hosts)
    .filter_map(|(seed, host)| regions.get(seed).map(|region| (*host, *region)))
    .collect();

  assert_fleet_forms(&daemons, &hosts, &names);
  let mut survivors: Vec<(HostId, Daemon)> = hosts.iter().copied().zip(daemons).collect();
  let elected = poll_until(
    &survivors.iter().map(|(_, d)| d).collect::<Vec<_>>(),
    COUNCIL_ELECTION_DEADLINE,
    || one_root_leader(survivors.iter().map(|(_, d)| d)),
  );
  let leader_host = survivors
    .iter()
    .find(|(_, daemon)| daemon.root_leads() == Ok(true))
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
      inject_death_into(survivors.iter().map(|(_, d)| d), victim);
      // The mirrored lost region is NOT auto-retired: it stays a member and its volumes still route to it.
      let not_retired = holds_for(COUNCIL_STABILITY_WINDOW, || {
        all_hold(
          survivors
            .iter()
            .map(|(_, daemon)| daemon.region_home(volume, lost).map(|home| home == lost)),
        )
      });
      // The operator promotes the lost region's mirror on the surviving root leader, re-issued each poll
      // iteration until it commits and re-homes everywhere: root leadership can flap under load between
      // finding the leader and the commit landing, and `PromoteRegion` is idempotent (home_of follows the
      // committed promotion; a second identical promotion is a no-op once applied).
      let promoted = poll_until(
        &survivors.iter().map(|(_, d)| d).collect::<Vec<_>>(),
        COUNCIL_RETIRE_DEADLINE,
        || {
          if let Some((_, leader)) = survivors
            .iter()
            .find(|(_, daemon)| daemon.root_leads() == Ok(true))
          {
            // Re-issued each ask, so a refused issue is asked again rather than judged here.
            let _ = leader.promote_region(lost);
          }
          all_hold(
            survivors
              .iter()
              .map(|(_, daemon)| daemon.region_home(volume, lost).map(|home| home == mirror)),
          )
        },
      );
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

/// AC (§4.8, D-14, §4.12): the operator issues `promote-region` on **any** node, not only the root leader. A
/// client connected to a **follower** sends `RequestBody::PromoteRegion`; the follower forwards it to the
/// leader it knows over the fleet transport ([`FORWARD_STREAM`]), the leader proposes it on the root group,
/// and every node re-homes the region's volumes to the mirror. This proves the client → IPC → `serve` →
/// forward-over-the-mesh → root-leader → root-group path the CLI drives. (`an_operator_promotes...` proves the
/// loss detection and the promotion via the daemon method on the leader; this proves the follower forwarding.)
/// All three stay alive, isolating the forwarding path (the re-home is what is observed).
#[test]
fn a_client_on_a_follower_promotes_a_region_by_forwarding_to_the_leader() {
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
  let daemons =
    start_mesh_with_regions_and_mirrors(nodes, &hosts, &certs, &serve, 1, &regions, &mirrors);
  let hosts: Vec<HostId> = daemons
    .iter()
    .map(|daemon| daemon.member_identity().unwrap())
    .collect();

  assert_fleet_forms(&daemons, &hosts, &names);
  let daemons: Vec<(HostId, Daemon)> = hosts.iter().copied().zip(daemons).collect();
  let elected = poll_until(
    &daemons.iter().map(|(_, d)| d).collect::<Vec<_>>(),
    COUNCIL_ELECTION_DEADLINE,
    || one_root_leader(daemons.iter().map(|(_, d)| d)),
  );
  let lost = RegionId(1);
  let mirror = RegionId(0);
  let volume = ObjectId::new(hosts[1], 7); // a volume created in region 1

  // Pick a follower — a node that does not lead the root group — and promote the region through a client
  // connected to it. The follower cannot propose, so it forwards the operator's PromoteRegion to the leader it
  // knows; the leader proposes it and every node re-homes. Re-issued each poll iteration (idempotent), so a
  // transient leadership change (the follower briefly leading, or its known leader changing) rides through.
  let follower = daemons
    .iter()
    .find(|(_, daemon)| daemon.root_leads() == Ok(false))
    .map(|(_, daemon)| daemon.instance().to_owned());
  let promoted = match (elected, follower) {
    (true, Some(follower)) => {
      let mut client = Client::connect(&follower);
      poll_until(
        &daemons.iter().map(|(_, d)| d).collect::<Vec<_>>(),
        COUNCIL_RETIRE_DEADLINE,
        || {
          let _ = client.call(&RequestBody::PromoteRegion { region: lost.0 });
          all_hold(
            daemons
              .iter()
              .map(|(_, daemon)| daemon.region_home(volume, lost).map(|home| home == mirror)),
          )
        },
      )
    }
    _ => false,
  };

  for (_, daemon) in daemons {
    daemon.stop();
  }
  assert!(elected, "the root group elected a single leader");
  assert!(
    promoted,
    "a client's PromoteRegion on a follower was forwarded to the root leader and committed — every node \
     re-homed the region's volumes to the mirror, so the operator may promote from any node"
  );
}

/// AC (§4.8 "Lookup", slice 2): a client reads a volume homed in **another region**, and the read is served.
/// Three daemons, each its own region; a volume is created on node a (region 0). A client on node b (region 1)
/// reads that volume's `Status`: b's serve path sees it is homed elsewhere and **forwards** the read to the
/// volume's owner — a, its creator — over the fleet transport (`FORWARD_STREAM`), a serves it on the owner
/// shard under the relayed principal, and the reply is relayed back, so the client on b gets the volume's state
/// without connecting to region 0. This is the cross-region routing slice 1 (791f2bd) only refused; the forward
/// makes it a served request (`verbs::serve_forward` + the `homed_elsewhere` guard's forward, over
/// `serve_once_async`). The read is polled: it succeeds once the root configuration has formed on b (so the
/// lookup guard fires) and the b→a session is up.
#[test]
fn a_client_reads_a_cross_region_volume_by_forwarding_to_its_owner() {
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
  let daemons = start_mesh_with_regions(nodes, &hosts, &certs, &serve, 1, &regions);
  let instances: Vec<String> = daemons
    .iter()
    .map(|daemon| daemon.instance().to_owned())
    .collect();
  let hosts: Vec<HostId> = daemons
    .iter()
    .map(|daemon| daemon.member_identity().unwrap())
    .collect();

  assert_fleet_forms(&daemons, &hosts, &names);

  // Create a volume on node a (region 0): it is owned by a, its creator.
  let mut client_a = Client::connect(&instances[0].clone());
  let created = client_a.call(&scratch("cross-region"));
  let id = match created {
    ReplyBody::Created { id } => id,
    other => {
      for daemon in daemons {
        daemon.stop();
      }
      panic!("create on node a did not return an id: {other:?}");
    }
  };

  // A client on node b (region 1) reads the volume's Status. b forwards the read to a and relays the reply.
  // Polled: it turns from a transient refusal (the root configuration not yet formed on b, so the lookup guard
  // does not fire and b routes locally, or the b→a session not yet up) into the served Status.
  let mut client_b = Client::connect(&instances[1].clone());
  let served = poll_until(
    &daemons.iter().collect::<Vec<_>>(),
    COUNCIL_RETIRE_DEADLINE,
    || {
      Ok(matches!(
        client_b.call(&RequestBody::Status { volume: id }),
        ReplyBody::Status { .. }
      ))
    },
  );

  for daemon in daemons {
    daemon.stop();
  }
  assert!(
    served,
    "a client on region 1 read a volume homed in region 0 — the read was forwarded to its owner and served, \
     so the cross-region lookup is a served request, not a refusal"
  );
}

/// AC-8.1 / §4.8 Lookup: a home region has more members than an object's copyset. After its
/// owner dies, a foreign client must reach the copyset successor, even when rendezvous over all
/// live home-region members ranks an unrelated node first. The remote snapshot retry stays exactly-once.
#[test]
fn a_cross_region_client_finds_the_copyset_successor_instead_of_an_unrelated_live_peer() {
  let _serial = serialize_fleet_tests();
  let names = ["a", "b", "c", "d", "foreign"];
  let nodes: Vec<_> = names.iter().map(|name| fleet_node(name)).collect();
  let seeds: Vec<_> = nodes.iter().map(|(_, host, _)| *host).collect();
  let certs: Vec<_> = nodes
    .iter()
    .map(|(_, _, identity)| identity.certificate())
    .collect();
  let regions = seeds
    .iter()
    .enumerate()
    .map(|(index, host)| (*host, RegionId(u64::from(index == names.len() - 1))))
    .collect();
  let serve = mesh_serve_ports(names.len());
  let mut daemons = start_mesh_with_regions(nodes, &seeds, &certs, &serve, 1, &regions);
  let hosts: Vec<_> = daemons
    .iter()
    .map(|daemon| daemon.member_identity().unwrap())
    .collect();
  assert_fleet_forms(&daemons, &hosts, &names);
  let foreign_instance = daemons.last().unwrap().instance().to_owned();
  // Keep the lowest-id regional representative alive, so the root quorum is undisturbed.
  let owner_index = (0..names.len() - 1)
    .max_by_key(|index| hosts[*index])
    .unwrap();
  let owner = hosts[owner_index];
  let neighborhood = daemons[owner_index].placement_neighbourhood().unwrap();
  let survivors: Vec<_> = hosts[..names.len() - 1]
    .iter()
    .copied()
    .filter(|host| *host != owner)
    .collect();
  let (id, successor, unrelated) =
    place_copyset_routing_case(&daemons, owner_index, owner, &neighborhood, &survivors);
  trace::record(format_args!(
    "owner-lookup selected owner={owner:?}, successor={successor:?}, unrelated={unrelated:?}, object={:?}",
    ObjectId(id.bytes)
  ));
  let mut interrupted = Client::connect(&foreign_instance);
  assert!(audit_wait(|| Ok(matches!(
    interrupted.call(&RequestBody::Status { volume: id }),
    ReplyBody::Status { .. }
  ))));
  daemons.remove(owner_index).stop();
  let unavailable = interrupted.call(&RequestBody::Snapshot { volume: id });
  assert!(
    matches!(
      unavailable,
      ReplyBody::Refused {
        refusal: Refusal::HomedElsewhere { .. }
      }
    ),
    "a stopped owner cannot execute the forwarded write: {unavailable:?}"
  );
  let successor_daemon = daemons
    .iter()
    .find(|daemon| daemon.member_identity().unwrap() == successor)
    .unwrap();
  assert_copyset_adopted(&daemons, successor_daemon, ObjectId(id.bytes));
  assert!(audit_wait(|| Ok(status_answers_once(
    successor_daemon.instance(),
    id
  ))));
  let mut foreign = Client::connect(&foreign_instance);
  let mut last = ReplyBody::Refused {
    refusal: Refusal::NotFound,
  };
  let served = audit_wait(|| {
    last = foreign.call(&RequestBody::Status { volume: id });
    Ok(matches!(last, ReplyBody::Status { .. }))
  });
  trace::record(format_args!("owner-lookup served={served}, last={last:?}"));
  let foreign_daemon = daemons
    .iter()
    .find(|daemon| daemon.instance() == foreign_instance)
    .unwrap();
  let counters_before = foreign_daemon.fleet_refusals().unwrap();
  let written = served.then(|| foreign.call(&RequestBody::Snapshot { volume: id }));
  let retry = served.then(|| foreign.call_retry(&RequestBody::Snapshot { volume: id }));
  let counters_after = foreign_daemon.fleet_refusals().unwrap();
  let resumed = retry_snapshot_until_served(&mut interrupted, id);
  let resumed_retry = interrupted.call_retry(&RequestBody::Snapshot { volume: id });
  for daemon in daemons {
    daemon.stop();
  }
  assert!(
    served,
    "foreign lookup must reach the actual copyset successor: {last:?}"
  );
  assert_same_snapshot_reply(
    written.expect("the forward path was available"),
    retry.expect("the retry path was available"),
  );
  assert_same_snapshot_reply(resumed, resumed_retry);
  assert!(
    counters_before
      .get("fleet.owner_location.round")
      .copied()
      .unwrap_or(0)
      > 0,
    "the remote client discovered the actual owner"
  );
  assert_eq!(
    counters_before.get("fleet.owner_location.round"),
    counters_after.get("fleet.owner_location.round"),
    "the two writes reuse the client's route instead of repeating discovery"
  );
  assert!(
    counters_after
      .get("fleet.owner_location.direct")
      .copied()
      .unwrap_or(0)
      > counters_before
        .get("fleet.owner_location.direct")
        .copied()
        .unwrap_or(0),
    "the cached route really forwarded a request"
  );
}

/// Seal a volume that distinguishes a copyset successor from all-member ranking, and establish
/// its real holds before stopping the owner. All selected candidates must hold the head.
fn place_copyset_routing_case(
  daemons: &[Daemon],
  owner_index: usize,
  owner: HostId,
  neighborhood: &[HostId],
  survivors: &[HostId],
) -> (VolumeId, HostId, HostId) {
  let mut local = Client::connect(daemons[owner_index].instance());
  let (id, name, candidates, successor, unrelated) =
    choose_copyset_routing_case(&mut local, owner, neighborhood, survivors);
  write_hello_over_nfs(&daemons[owner_index], &name);
  let ReplyBody::Snapshotted { id: snapshot, .. } =
    local.call(&RequestBody::Snapshot { volume: id })
  else {
    panic!("seal the selected volume")
  };
  assert!(poll_snapshot_placed(
    &daemons.iter().collect::<Vec<_>>(),
    &mut local,
    id,
    snapshot
  ));
  let holders: Vec<_> = daemons
    .iter()
    .filter(|daemon| {
      candidates.contains(&daemon.member_identity().unwrap())
        && daemon.member_identity().unwrap() != owner
    })
    .collect();
  let manifest = daemons[owner_index]
    .fleet_head_manifest(ObjectId(id.bytes))
    .unwrap()
    .unwrap();
  assert!(
    audit_wait(|| all_hold(holders.iter().map(|daemon| {
      daemon.fleet_holder_head(ObjectId(id.bytes)).map(|held| {
        held
          .and_then(|(_, bytes)| HeadValue::from_record_bytes(&bytes))
          .is_some_and(|head| head.manifest == Some(manifest))
      })
    }))),
    "every candidate holds the sealed head, not just its earlier creation record"
  );
  (id, successor, unrelated)
}

/// A routing refusal is temporary; retry the original write id after a successor is available.
fn retry_snapshot_until_served(client: &mut Client, volume: VolumeId) -> ReplyBody {
  let mut reply = ReplyBody::Refused {
    refusal: Refusal::NotFound,
  };
  assert!(
    audit_wait(|| {
      reply = client.call_retry(&RequestBody::Snapshot { volume });
      Ok(matches!(reply, ReplyBody::Snapshotted { .. }))
    }),
    "a routing refusal must not poison the write's completion: {reply:?}"
  );
  reply
}

fn assert_same_snapshot_reply(first: ReplyBody, retry: ReplyBody) {
  let ReplyBody::Snapshotted { id: first, .. } = first else {
    panic!("write not served: {first:?}")
  };
  let ReplyBody::Snapshotted { id: retry, .. } = retry else {
    panic!("retry not served: {retry:?}")
  };
  assert_eq!(
    first, retry,
    "the resumed write executes once at its successor"
  );
}

/// Distinguishes a failed takeover prerequisite from the routing exchange this history tests.
fn assert_copyset_adopted(daemons: &[Daemon], successor_daemon: &Daemon, object: ObjectId) {
  let adopted = audit_wait(|| successor_daemon.fleet_head_placed(object));
  trace::record(format_args!("owner-lookup adopted={adopted}"));
  if !adopted {
    for daemon in daemons {
      trace::record(format_args!(
        "owner-lookup adoption host={:?} members={:?} council={:?} held={:?} refusals={:?}",
        daemon.member_identity(),
        daemon.fleet_members(),
        daemon.council_members(),
        daemon
          .fleet_holder_head(object)
          .map(|head| head.map(|(owner, _)| owner)),
        daemon.fleet_refusals(),
      ));
    }
  }
  assert!(
    adopted,
    "the actual successor must adopt before testing remote lookup"
  );
}

/// Selects a real provisioned id that distinguishes the record's copyset from all alive members.
/// Non-selected volumes are destroyed; the finite shape budget spans rendezvous weights, not time.
fn choose_copyset_routing_case(
  local: &mut Client,
  owner: HostId,
  neighborhood: &[HostId],
  survivors: &[HostId],
) -> (VolumeId, String, Vec<HostId>, HostId, HostId) {
  /// Shape: the same bounded object-history width as the copyset oracle in slates-cluster.
  const OBJECTS: usize = 128;
  for sequence in 0..OBJECTS {
    let name = format!("copyset-route-{sequence}");
    let ReplyBody::Created { id } = local.call(&scratch(&name)) else {
      panic!("create routing candidate")
    };
    let object = ObjectId(id.bytes);
    let candidates = candidates_for(
      owner,
      neighborhood,
      &std::collections::BTreeMap::new(),
      object,
      Quorum { f: 1 },
    );
    let surviving_candidates: Vec<_> = candidates
      .iter()
      .copied()
      .filter(|host| survivors.contains(host))
      .collect();
    let successor = rendezvous_first(&surviving_candidates, object).unwrap();
    let unrelated = rendezvous_first(survivors, object).unwrap();
    if successor != unrelated {
      return (id, name, candidates, successor, unrelated);
    }
    assert!(matches!(
      local.call(&RequestBody::Destroy { volume: id }),
      ReplyBody::Destroyed
    ));
  }
  panic!("the routing history must include a non-holder ranked before its copyset successor")
}

/// AC (§4.8 "Lookup", slice 2 — writes; task #29): a client on one region issues a **write** to a volume
/// homed in another region; it is forwarded to the owner, executed there, and is **exactly-once** on retry.
/// Three daemons, each its own region; a volume is created on node a (region 0). A client on node b (region 1)
/// first reads the volume's `Status` (to know the forward path is up — the root configuration formed on b and
/// the b→a session is live, so the write below is not raced by the still-forming config), then takes a
/// `Snapshot` of it. b forwards the write to a, which runs it through the completion window keyed by b's
/// **authenticated** origin ([`verbs::serve_forward`] → `run_forwarded`), and returns the snapshot id. A
/// **retry of the same request id** returns the **same** snapshot id — a answered from its record, not a
/// second snapshot — proving the forwarded write is idempotent under the globally-unique completion key (the
/// per-node client id could otherwise collide with an owner-local client). Non-vacuous: a second snapshot
/// would carry a different id, so the equality is the exactly-once proof.
#[test]
fn a_client_writes_a_cross_region_volume_by_forwarding_to_its_owner() {
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
  let daemons = start_mesh_with_regions(nodes, &hosts, &certs, &serve, 1, &regions);
  let instances: Vec<String> = daemons
    .iter()
    .map(|daemon| daemon.instance().to_owned())
    .collect();
  let hosts: Vec<HostId> = daemons
    .iter()
    .map(|daemon| daemon.member_identity().unwrap())
    .collect();

  assert_fleet_forms(&daemons, &hosts, &names);

  // Create a volume on node a (region 0): it is owned by a, its creator.
  let mut client_a = Client::connect(&instances[0].clone());
  let id = match client_a.call(&scratch("cross-region-write")) {
    ReplyBody::Created { id } => id,
    other => {
      for daemon in daemons {
        daemon.stop();
      }
      panic!("create on node a did not return an id: {other:?}");
    }
  };

  // A client on node b (region 1). Wait until a cross-region *read* is served — that proves the root
  // configuration has formed on b (the lookup guard fires) and the b→a session is up — so the write below is
  // not raced by the forming config. A read takes no snapshot, so this readiness poll is side-effect free.
  let mut client_b = Client::connect(&instances[1].clone());
  let ready = poll_until(
    &daemons.iter().collect::<Vec<_>>(),
    COUNCIL_RETIRE_DEADLINE,
    || {
      Ok(matches!(
        client_b.call(&RequestBody::Status { volume: id }),
        ReplyBody::Status { .. }
      ))
    },
  );

  // One forwarded write. `call` assigns a fresh request id; a transient forward failure is retried on the
  // **same** id (`call_retry`), so a is never asked for a second snapshot while the first is in flight.
  let mut first = client_b.call(&RequestBody::Snapshot { volume: id });
  poll_until(
    &daemons.iter().collect::<Vec<_>>(),
    COUNCIL_RETIRE_DEADLINE,
    || {
      if matches!(first, ReplyBody::Snapshotted { .. }) {
        return Ok(true);
      }
      first = client_b.call_retry(&RequestBody::Snapshot { volume: id });
      Ok(matches!(first, ReplyBody::Snapshotted { .. }))
    },
  );
  // A further retry of the same id must return the recorded reply.
  let forwards_before = daemons[1].fleet_refusals().unwrap();
  let retry = client_b.call_retry(&RequestBody::Snapshot { volume: id });
  let forwards_after = daemons[1].fleet_refusals().unwrap();

  for daemon in daemons {
    daemon.stop();
  }

  assert!(ready, "b's cross-region forward path to region 0 came up");
  let ReplyBody::Snapshotted { id: snap1, .. } = first else {
    panic!("the forwarded Snapshot of a cross-region volume was not served: {first:?}");
  };
  let ReplyBody::Snapshotted { id: snap2, .. } = retry else {
    panic!("the retried forwarded Snapshot was not served: {retry:?}");
  };
  assert_eq!(
    snap1, snap2,
    "the retried forwarded write returned the owner's recorded reply (the same snapshot id), not a second \
     snapshot — the forwarded write is exactly-once under the globally-unique completion key"
  );
  assert_eq!(
    forwards_after
      .get("fleet.owner_location.direct")
      .copied()
      .unwrap_or(0),
    forwards_before
      .get("fleet.owner_location.direct")
      .copied()
      .unwrap_or(0)
      + 1,
    "the retry reaches the owner: an origin-side completion cannot mask a broken owner replay"
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
  let hosts: Vec<HostId> = daemons
    .iter()
    .map(|daemon| daemon.member_identity().unwrap())
    .collect();

  assert_fleet_forms(&daemons, &hosts, &names);
  let daemons: Vec<(HostId, Daemon)> = hosts.iter().copied().zip(daemons).collect();
  let elected = poll_until(
    &daemons.iter().map(|(_, d)| d).collect::<Vec<_>>(),
    COUNCIL_ELECTION_DEADLINE,
    || one_root_leader(daemons.iter().map(|(_, d)| d)),
  );

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
    && poll_until(
      &daemons.iter().map(|(_, d)| d).collect::<Vec<_>>(),
      COUNCIL_RETIRE_DEADLINE,
      || {
        if let Some((_, leader)) = daemons
          .iter()
          .find(|(_, daemon)| daemon.root_leads() == Ok(true))
        {
          // Re-issued each ask, so a refused issue is asked again rather than judged here.
          let _ = leader.promote_region(lost);
        }
        all_hold(daemons.iter().map(|(_, daemon)| {
          Ok(
            daemon.region_home(volume, lost)? == mirror
              && daemon.region_home_on_shard(1, volume, lost)? == mirror,
          )
        }))
      },
    );

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
  let hosts: Vec<HostId> = daemons
    .iter()
    .map(|daemon| daemon.member_identity().unwrap())
    .collect();

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
    daemons[observed_index].council_leads().map(|leads| !leads)
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
  inject_death_into(survivors.iter().map(|(_, d)| d), dead);
  let observed_pos = survivors
    .iter()
    .position(|(host, _)| *host == observed_host)
    .expect("the observed learner survives");
  let learned = poll_until(
    &survivors.iter().map(|(_, d)| d).collect::<Vec<_>>(),
    COUNCIL_RETIRE_DEADLINE,
    || {
      let daemon = &survivors[observed_pos].1;
      Ok(
        !daemon.council_members()?.contains(&dead)
          && !daemon.placement_neighbourhood()?.contains(&dead),
      )
    },
  );

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
  let instances: Vec<String> = daemons
    .iter()
    .map(|daemon| daemon.instance().to_owned())
    .collect();
  let hosts: Vec<HostId> = daemons
    .iter()
    .map(|daemon| daemon.member_identity().unwrap())
    .collect();

  assert_fleet_forms(&daemons, &hosts, &names);

  // Node C (index 2) dies; its serve loops stop, so A's and B's probes of C time out.
  let dead = hosts[2];
  daemons.pop().expect("three daemons").stop();
  let all_retired = poll_survivors_retire(&daemons, dead);
  // Scaling down must not strand new work: with C retired (the configuration version advanced), a volume
  // provisioned on A now must still place — over B, the one remaining candidate — under the new generation.
  // Before the owner's acceptor followed the version, A's own hold refused its record `ForeignGeneration`
  // and nothing provisioned after a membership change ever placed; the head placing is the proof it does.
  let placed_after_retirement = all_retired && {
    let mut client = Client::connect(&instances[0].clone());
    match client.call(&scratch("after-retirement")) {
      ReplyBody::Created { id } => poll_head_placed(
        &daemons.iter().collect::<Vec<_>>(),
        &daemons[0],
        ObjectId(id.bytes),
      ),
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

/// Shape: how many of A's fleet-coordinator periods B must survive with A's direct path to it lost — well
/// past the six backed-off misses (≈ 4 s at rest, some forty coordinator periods) a peer whose probes all go
/// unanswered takes to be declared dead, so a direct-only implementation would have retired B inside it.
const INDIRECT_HOLD_PERIODS: u64 = 100;

/// AC (§4.8 "direct probe → k indirect proxies → SUSPECT → DEAD"; AUD-15): the live SWIM path runs the
/// **indirect** stage. A cannot reach B directly (B leaves A's probes unanswered — an asymmetric path
/// loss, injected at B's serve side so the transport is untouched), but A reaches C and C reaches B. A's
/// direct probe times out, A asks C to reach B, C's probe of B is acknowledged and C carries that answer
/// back, and A credits it before the suspicion verdict — so B stays a member of A's fleet across a hold far
/// longer than a direct-only detector needs to retire a silent peer. Non-vacuous on both sides: A counted a
/// relayed answer credited (`fleet.probe.indirect.acked`) and C counted itself relaying one
/// (`fleet.probe.indirect.relayed`), so a direct-only implementation cannot pass by B happening to answer.
/// Then B goes silent to C as well — both paths lost — and the fleet **retires** B: the relay stage does
/// not weaken eventual detection, it only spares a peer one path can still reach.
#[test]
fn an_indirect_probe_through_a_relay_keeps_a_peer_the_direct_path_lost_and_losing_both_paths_retires_it()
 {
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
  let hosts: Vec<HostId> = daemons
    .iter()
    .map(|daemon| daemon.member_identity().unwrap())
    .collect();
  assert_fleet_forms(&daemons, &hosts, &names);
  let (a, b, c) = (&daemons[0], &daemons[1], &daemons[2]);
  let (host_a, host_b, host_c) = (hosts[0], hosts[1], hosts[2]);
  let observed = [a, b, c];

  // B stops answering A's direct probes — and only A's. C's probes of B, and everything relayed, still
  // work.
  b.inject_probe_deafness(&[host_a])
    .expect("the asymmetric loss is installed at B");

  // A's direct probes of B now time out; the relay stage must run — A asks C, C reaches B, A credits it.
  let relayed = poll_until(&observed, RETIREMENT_DEADLINE, || {
    Ok(
      refusal_count(a, "fleet.probe.indirect.acked")? >= 1
        && refusal_count(c, "fleet.probe.indirect.relayed")? >= 1,
    )
  });

  // Hold: across INDIRECT_HOLD_PERIODS of A's own periods B must stay a member of A's fleet. The wait ends
  // early if A retires B (a failure the assertion below then names), else at the period budget.
  let start = a.fleet_progress();
  let held = poll_until(&observed, RETIREMENT_DEADLINE, || {
    let still_member = a.fleet_members()?.contains(&host_b);
    Ok(!still_member || a.fleet_progress() >= start + INDIRECT_HOLD_PERIODS)
  });
  let kept = a
    .fleet_members()
    .is_ok_and(|members| members.contains(&host_b));
  let acked_on_a = refusal_count(a, "fleet.probe.indirect.acked").unwrap_or(0);
  let relayed_by_c = refusal_count(c, "fleet.probe.indirect.relayed").unwrap_or(0);
  let requested_by_a = refusal_count(a, "fleet.probe.indirect.requested").unwrap_or(0);

  // Now B goes silent to C too: no path reaches it, and the fleet must retire it.
  b.inject_probe_deafness(&[host_a, host_c])
    .expect("the total loss is installed at B");
  let survivors = [&daemons[0], &daemons[2]];
  let retired = poll_until(&survivors, RETIREMENT_DEADLINE, || {
    all_hold(survivors.iter().map(|survivor| {
      survivor
        .fleet_members()
        .map(|members| !members.contains(&host_b))
    }))
  });

  for daemon in daemons {
    daemon.stop();
  }
  assert!(
    relayed,
    "A's direct probes of B timed out and the indirect stage ran: A credited a relayed answer \
     (acked={acked_on_a}, requested={requested_by_a}) and C relayed one (relayed={relayed_by_c})"
  );
  assert!(
    held && kept,
    "B stayed a member of A's fleet for {INDIRECT_HOLD_PERIODS} of A's periods with A's direct path lost \
     (acked={acked_on_a}, relayed={relayed_by_c}, requested={requested_by_a})"
  );
  assert!(
    retired,
    "with both paths lost, A and C retired B (the relay stage does not weaken eventual detection)"
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
  let observed: Vec<&Daemon> = daemons.iter().collect();
  if poll_until(&observed, FORMATION_DEADLINE, || {
    all_hold(daemons.iter().map(|daemon| daemon.fleet_meshed()))
  }) {
    return;
  }
  // Still not meshed: assert with a message naming every node's state — meshed or not, the members it
  // sees (the seeded view, so seeded-vs-formed is legible), the peers it actually formed probe sessions
  // to, and its coordinator's period count — so a failure says whether the coordinators were ticking
  // (the period budget ran out: a genuine non-convergence) or frozen (no progress: a wedge), and which
  // sessions never formed. The 2026-09-14 formation failure under load carried only "sees members",
  // which could not distinguish the two (`docs/bugs/2026-09-14-handshake-retry-forgets-its-flight.md`).
  let report: Vec<String> = daemons
    .iter()
    .enumerate()
    .map(|(i, daemon)| {
      format!(
        "{}: meshed={:?} members={:?} formed_probe_peers={:?} periods={}",
        names[i],
        daemon.fleet_meshed(),
        daemon.fleet_members(),
        daemon.fleet_formed_probe_peers(),
        daemon.fleet_progress()
      )
    })
    .collect();
  for (i, daemon) in daemons.iter().enumerate() {
    assert!(
      daemon.fleet_meshed() == Ok(true),
      "node {} did not form its full probe mesh within the formation budget ({} periods of the slowest \
       coordinator, or {:?} with no progress at all):\n{}",
      names[i],
      PERIOD_BUDGET,
      FROZEN_CAP,
      report.join("\n")
    );
  }
}

/// Polls until every survivor has retired `dead` from its membership, or the retirement deadline passes;
/// returns whether they all did.
fn poll_survivors_retire(survivors: &[Daemon], dead: HostId) -> bool {
  // Retirement is monotonic (a retired member does not reappear in `fleet_members` without a rejoin, which
  // this never triggers), so "every survivor currently shows `dead` retired" holds from the moment the last
  // one retires — the same verdict the former per-survivor latch reached, now under the load-adaptive poll.
  poll_until(
    &survivors.iter().collect::<Vec<_>>(),
    RETIREMENT_DEADLINE,
    || {
      all_hold(survivors.iter().map(|survivor| {
        survivor
          .fleet_members()
          .map(|members| !members.contains(&dead))
      }))
    },
  )
}

/// Shape: the memory bound the lane's pod runs under (`deploy/kind/values-lane.yaml`, Guaranteed QoS
/// `memory: 1Gi`): the cgroup bound the profile reports as `memory.limit`, from which the daemon derives its
/// client and task budgets — the container's budgets, reproduced on this machine.
const POD_MEMORY_BYTES: u64 = 1 << 30;

/// AC-2.6 (admission stays within the task arena; refusals typed), §4.8 boot step 6: a fleet node under a
/// container's memory bound admits its clients. The daemon derives its task budget from its client bound;
/// the fleet's own tasks (two per unit of peer capacity to dial it, one serve task per slot of each plane's
/// shared session pool — `slates_transport::demux::SESSION_SLOTS_PER_PEER` slots per unit of peer capacity — and its
/// loops) must be inside that budget, or the admission task that seats a client on its shard is
/// refused by the arena and dropped unrun — the client's channel closes under it and `slates status` never
/// answers. The KIND lane hit exactly this on 2026-09-14: one pod of a five-replica fleet (four peers) at
/// 1 GiB never became Ready (`docs/bugs/2026-09-14-fleet-tasks-outside-the-task-budget-poison-client-admission.md`).
/// Here, the pod's shape: node A under the pod's bound has one real peer, B, **which dials in** (A accepts
/// B's sessions and holds a serve task for each — the tasks a peer adds to a node it reaches), plus as many
/// silent peers as A's client-only task budget has slots (none answers; A's dial tasks to them alone fill an
/// arena sized without the fleet). Once B's probe session to A is up, one client asks A for `status`.
/// Non-vacuous: B's session formed (so A serves it) and the premise (A's peers exceed its client-only
/// budget) are both asserted, and the answer counts the client seated on its shard.
///
/// Written failing on the lane's branch (with the arena full the daemons' shutdown never completed, so it
/// hung past its reply deadline rather than failing); green once the fleet's task share landed on main
/// (`DaemonConfig::with_fleet`, 2026-09-14, `docs/bugs/2026-09-14-fleet-tasks-admitted-against-the-clients-budget.md`).
#[test]
fn a_fleet_node_under_a_containers_memory_bound_still_admits_a_client() {
  let pid = std::process::id();
  let (mut profile_a, host_a, identity_a) = fleet_node("bounded-a");
  profile_a.facts.memory.limit = Some(POD_MEMORY_BYTES);
  // The node runs one shard. Derive its budgets from one core as well, so a larger host
  // cannot make the per-shard peer population smaller than the container's.
  profile_a.facts.cores.truncate(1);
  let (profile_b, host_b, identity_b) = fleet_node("bounded-b");
  let anchor_a = anchor_of(&profile_a);
  let anchor_b = anchor_of(&profile_b);
  let instance_a = format!("bounded-{}-{pid}", host_a.0);
  let instance_b = format!("bounded-{}-{pid}", host_b.0);
  // The budget A derives for its clients alone (no fleet joined yet): the silent peers are sized from it.
  let client_only_budget = DaemonConfig::derive(&profile_a, &instance_a, Some(1))
    .runtime
    .tasks_per_shard;
  let serve = mesh_serve_ports(2);
  let cert_a = identity_a.certificate();
  let cert_b = identity_b.certificate();
  let ports = free_ports(2 * client_only_budget);
  let mut peers_of_a = vec![fleet_peer_at(anchor_b, host_b, serve[1], &cert_b)];
  peers_of_a.extend((0..client_only_budget).map(|index| {
    let silent_anchor = HostId(u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1));
    fleet_peer_at(
      silent_anchor,
      member_id(silent_anchor, 0),
      (ports[2 * index], ports[2 * index + 1]),
      &self_signed().certificate(),
    )
  }));
  let peer_count = peers_of_a.len();
  let peers_of_b = vec![fleet_peer_at(anchor_a, host_a, serve[0], &cert_a)];
  let no_domains = std::collections::BTreeMap::new();
  let config_a = fleet_config(
    &profile_a,
    &instance_a,
    anchor_a,
    host_a,
    &peers_of_a,
    &no_domains,
  );
  let config_b = fleet_config(
    &profile_b,
    &instance_b,
    anchor_b,
    host_b,
    &peers_of_b,
    &no_domains,
  );
  let daemon_a = start_fleet_node(
    &profile_a,
    config_a,
    identity_a,
    serve[0],
    peers_of_a,
    SegmentSource::Create {
      name: format!("slates-seg-{instance_a}"),
    },
  );
  let daemon_b = start_fleet_node(
    &profile_b,
    config_b,
    identity_b,
    serve[1],
    peers_of_b,
    SegmentSource::Create {
      name: format!("slates-seg-{instance_b}"),
    },
  );
  let host_a = daemon_a
    .member_identity()
    .expect("A has its fresh identity");
  // B's probe session to A is up — A accepted it and serves it — before the client arrives.
  let served_b = poll_until(&[&daemon_b], FORMATION_DEADLINE, || daemon_b.fleet_meshed());
  let mut client = Client::connect(&instance_a);
  let report = match client.call(&RequestBody::DaemonStatus) {
    ReplyBody::DaemonStatus { report } => report,
    other => panic!("status answers: {other:?}"),
  };
  daemon_a.stop();
  daemon_b.stop();
  assert!(
    served_b,
    "B's probe session to A formed, so A holds a serve task for it"
  );
  assert!(
    peer_count > client_only_budget,
    "the premise: A's peers ({peer_count}) exceed its client-only task budget ({client_only_budget})"
  );
  assert_eq!(
    report.shards.iter().map(|shard| shard.clients).sum::<u32>(),
    1,
    "the client is seated on its shard and counted"
  );
  assert!(
    report.fleet.members.contains(&host_a.0),
    "the node holds itself alive: {:?}",
    report.fleet.members
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
          if trace::enabled() {
            trace::record(format_args!(
              "client connect {instance} took {:.3}s",
              started.elapsed().as_secs_f64()
            ));
          }
          let client = connected.region.client_id();
          return Client {
            end: ClientEnd::connected(connected),
            client,
            sequence: 0,
          };
        }
        Err(IpcError::DaemonUnavailable { .. }) if started.elapsed() < CREDIT_WAIT => {
          std::thread::yield_now();
        }
        Err(e) => {
          trace::record(format_args!(
            "client connect {instance} failed after {:.3}s: {e}",
            started.elapsed().as_secs_f64()
          ));
          panic!("{e}")
        }
      }
    }
  }

  fn call(&mut self, body: &RequestBody) -> ReplyBody {
    if matches!(body, RequestBody::DaemonStatus) {
      let capacity = slates_ipc::status::snapshot_capacity(self.end.region());
      return slates_ipc::status::collect::<IpcError>(capacity, |request| {
        Ok(self.call_once(request))
      })
      .unwrap();
    }
    self.call_once(body)
  }

  fn call_once(&mut self, body: &RequestBody) -> ReplyBody {
    self.sequence += 1;
    self.send_at(self.sequence, body)
  }

  /// Re-sends the **same** request id as the last [`call`](Self::call) (its sequence is not advanced): a
  /// retry. A recorded verb (a write) must answer from its completion record, not re-execute — the
  /// exactly-once check a forwarded write needs.
  fn call_retry(&mut self, body: &RequestBody) -> ReplyBody {
    self.send_at(self.sequence, body)
  }

  /// Sends `body` as the next request and waits at most `deadline_ns` for its reply, returning the
  /// typed error instead of panicking when none came — for a verb whose reply is **expected** to wait
  /// (a submit whose acceptance waits for a fleet commit, AUD-11). A reply that arrives after the
  /// deadline sits in the ring; [`drain`](Self::drain) discards it before the next call.
  fn try_call(&mut self, body: &RequestBody, deadline_ns: u64) -> Result<ReplyBody, IpcError> {
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
    self.end.send(&slot)?;
    let reply = self.end.wait(Some(deadline_ns))?;
    Ok(unpack(self.end.region(), reply.kind, &reply.payload).unwrap())
  }

  /// Re-sends the **same** request id as the last call (a retry, like [`call_retry`](Self::call_retry))
  /// and waits at most `deadline_ns` for its reply, returning the typed error instead of panicking.
  fn try_retry(&mut self, body: &RequestBody, deadline_ns: u64) -> Result<ReplyBody, IpcError> {
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
    self.end.send(&slot)?;
    let reply = self.end.wait(Some(deadline_ns))?;
    Ok(unpack(self.end.region(), reply.kind, &reply.payload).unwrap())
  }

  /// Discards every reply already in the ring (the late reply to a [`try_call`](Self::try_call) that
  /// timed out), so the next call reads its own reply.
  fn drain(&mut self) {
    while self.end.wait(Some(DRAIN_NS)).is_ok() {}
  }

  fn send_at(&mut self, sequence: u32, body: &RequestBody) -> ReplyBody {
    let id = RequestId {
      client: self.client,
      sequence,
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
    // The verb's kind for the trace: the variant name, the first word of its debug form.
    let verb = format!("{body:?}");
    let verb = verb.split([' ', '{', '(']).next().unwrap_or("?").to_owned();
    loop {
      match self.end.send(&slot) {
        Ok(()) => break,
        Err(IpcError::RingFull) if started.elapsed() < CREDIT_WAIT => std::thread::yield_now(),
        Err(e) => {
          trace::record(format_args!(
            "client {verb} send failed after {:.3}s: {e}",
            started.elapsed().as_secs_f64()
          ));
          panic!("{e}")
        }
      }
    }
    let reply = match self.end.wait(Some(DEADLINE_NS)) {
      Ok(reply) => reply,
      Err(e) => {
        trace::record(format_args!(
          "client {verb} reply failed after {:.3}s: {e}",
          started.elapsed().as_secs_f64()
        ));
        panic!("client {verb}: {e}")
      }
    };
    if trace::enabled() {
      trace::record(format_args!(
        "client {verb} replied in {:.3}s",
        started.elapsed().as_secs_f64()
      ));
    }
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
    anchor: b.origin_anchor,
    host: b.host,
    address: b.address,
    record_address: b.record_address,
    certificate: b.identity.certificate(),
  };
  let peer_of_b = Peer {
    anchor: a.origin_anchor,
    host: a.host,
    address: a.address,
    record_address: a.record_address,
    certificate: a.identity.certificate(),
  };
  let daemon_a = start(a, peer_of_a);
  let daemon_b = start(b, peer_of_b);

  // Let the fleet form (polled) before provisioning: the loops dial and establish their sessions over the
  // transport, undisturbed by the client and the placement polling below (which run on the same control
  // shard).
  assert!(
    form_and_settle(&[&daemon_a, &daemon_b]),
    "the fleet's direct probe mesh formed before the test acts"
  );

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
  let placed = poll_until(&[&daemon_a, &daemon_b], FORMATION_DEADLINE, || {
    daemon_a.fleet_head_placed(object)
  });
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
  let pid = std::process::id();
  let instance_a = format!("fleet-{}-{pid}", a.host.0);
  let peer_of_a = Peer {
    anchor: b.origin_anchor,
    host: b.host,
    address: b.address,
    record_address: b.record_address,
    certificate: b.identity.certificate(),
  };
  let peer_of_b = Peer {
    anchor: a.origin_anchor,
    host: a.host,
    address: a.address,
    record_address: a.record_address,
    certificate: a.identity.certificate(),
  };
  let daemon_a = start(a, peer_of_a);
  let host_a = daemon_a.member_identity().unwrap();
  let daemon_b = start(b, peer_of_b);

  // Let the fleet form (polled) before provisioning, as the replication test does.
  assert!(
    form_and_settle(&[&daemon_a, &daemon_b]),
    "the fleet's direct probe mesh formed before the test acts"
  );

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
  let held = if poll_until(&[&daemon_a, &daemon_b], FORMATION_DEADLINE, || {
    daemon_b
      .fleet_holder_head(object)
      .map(|held| held.is_some())
  }) {
    daemon_b.fleet_holder_head(object).ok().flatten()
  } else {
    None
  };

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
fn poll_head_placed(gate: &[&Daemon], observed: &Daemon, object: ObjectId) -> bool {
  // Gate on ALL the daemons the placement depends on (the owner/successor AND its candidate holders), not
  // only the one we read `fleet_head_placed` from: a head places only when a holder acknowledges it, so if a
  // holder is the one starved under load, the wait must be charged against ITS progress too.
  poll_until(gate, SERVE_DEADLINE, || observed.fleet_head_placed(object))
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
  let hosts: Vec<HostId> = daemons
    .iter()
    .map(|daemon| daemon.member_identity().unwrap())
    .collect();

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
  let served = poll_head_placed(
    &daemons.iter().collect::<Vec<_>>(),
    &daemons[successor_index],
    object,
  );
  let held = daemons[successor_index]
    .fleet_holder_head(object)
    .ok()
    .flatten();

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
  poll_until(survivors, SERVE_DEADLINE, || {
    all_hold(
      survivors
        .iter()
        .map(|daemon| daemon.fleet_holder_head(object).map(|held| held.is_some())),
    )
  })
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
  let hosts: Vec<HostId> = daemons
    .iter()
    .map(|daemon| daemon.member_identity().unwrap())
    .collect();

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
  let served = poll_head_placed(
    &daemons.iter().collect::<Vec<_>>(),
    &daemons[successor_index],
    object,
  );
  let held = daemons[successor_index]
    .fleet_holder_head(object)
    .ok()
    .flatten();

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
/// Shape: how long to wait for a sealed snapshot's content and head to place across the fleet — the archive
/// walk, the offer/put rounds and the head commit, each a few protocol periods on loopback (~200–350 ms
/// unloaded, measured). Sixty seconds is deliberate headroom — ~170× the unloaded cost — so a transient CPU
/// spike or a loaded shared-tenant machine that runs the placement slower cannot flake it (the reseal flake
/// that surfaced this was a spike, not a hang: `docs/bugs/2026-09-12-retirement-tests-gate-on-real-swim-detection-under-load.md`).
const PLACEMENT_DEADLINE: Duration = Duration::from_secs(60);
/// Shape: how long to wait for a takeover successor to materialize and serve the taken-over content —
/// the takeover, a possible fetch from the recorded holder, and the restore. Generous headroom for load.
const SERVE_DEADLINE: Duration = Duration::from_secs(60);

/// The NFS mount path carrying an owner mount capability for the volume named `name` on `daemon`
/// (§4.13; AUD-01): `/<name>@<attachment_hex>.<token_hex>` — every mount presents one, since a name
/// alone reaches nothing.
fn capability_path(daemon: &Daemon, name: &str) -> String {
  daemon
    .mount_capability(name)
    .expect("the name's owner shard answers")
    .expect("a volume by that name is served there")
}

/// Writes [`CONTENT`] as `hello.txt` into the volume mounted at `/<name>` on `daemon`'s NFS port.
fn write_hello_over_nfs(daemon: &Daemon, name: &str) {
  let port = daemon.nfs_port().expect("the daemon serves NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to the NFS port");
  let root_fh = mount(&mut stream, &capability_path(daemon, name), 1);
  let file_fh = create(&mut stream, &root_fh, "hello.txt", 2);
  write(&mut stream, &file_fh, CONTENT, 3);
}

/// The (mode, uid, gid) of a volume's root and of `hello.txt`, as an NFS client reads them.
type Owners = [(u32, u32, u32); 2];

/// The mode and owner (`mode, uid, gid`) of the volume's root and of `hello.txt`, read over `daemon`'s
/// NFS port — what a takeover successor must reproduce from the replicated archive (format minor 2
/// carries every node's owner and the root's own metadata; before it, a rebuilt tree came up `0:0`
/// and the export's POSIX access control shut its owner out).
fn owners_over_nfs(daemon: &Daemon, name: &str) -> Owners {
  let port = daemon.nfs_port().expect("the daemon serves NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to the NFS port");
  let root_fh = mount(&mut stream, &capability_path(daemon, name), 1);
  let file_fh = lookup(&mut stream, &root_fh, "hello.txt", 2);
  [
    owner_and_mode(&mut stream, &root_fh, 3),
    owner_and_mode(&mut stream, &file_fh, 4),
  ]
}

/// Reads `hello.txt` back from the volume mounted at `/<name>` on `daemon`'s NFS port.
fn read_hello_over_nfs(daemon: &Daemon, name: &str) -> Vec<u8> {
  let port = daemon.nfs_port().expect("the daemon serves NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to the NFS port");
  let root_fh = mount(&mut stream, &capability_path(daemon, name), 1);
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
  let ReplyBody::Snapshotted { id: snapshot, .. } =
    client.call(&RequestBody::Snapshot { volume: id })
  else {
    return Err("the snapshot was not taken".to_owned());
  };
  let placed = poll_snapshot_placed(
    &daemons.iter().collect::<Vec<_>>(),
    &mut client,
    id,
    snapshot,
  );
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
fn poll_status_answers(daemons: &[&Daemon], instance: &str, volume: VolumeId) -> bool {
  let mut client = Client::connect(instance);
  poll_until(daemons, SERVE_DEADLINE, || {
    Ok(matches!(
      client.call(&RequestBody::Status { volume }),
      ReplyBody::Status { .. }
    ))
  })
}

/// Polls the owner's `await placed(snapshot, region)` verb until it answers placed, or the placement
/// deadline passes; returns whether it did.
fn poll_snapshot_placed(
  daemons: &[&Daemon],
  client: &mut Client,
  volume: VolumeId,
  snapshot: SnapshotId,
) -> bool {
  poll_until(daemons, PLACEMENT_DEADLINE, || {
    Ok(matches!(
      client.call(&RequestBody::AwaitPlaced {
        volume,
        snapshot: Some(snapshot),
        scope: Scope::Region,
      }),
      ReplyBody::Placed { placed: true, .. }
    ))
  })
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
    anchor: b.origin_anchor,
    host: b.host,
    address: b.address,
    record_address: b.record_address,
    certificate: b.identity.certificate(),
  };
  let peer_of_b = Peer {
    anchor: a.origin_anchor,
    host: a.host,
    address: a.address,
    record_address: a.record_address,
    certificate: a.identity.certificate(),
  };
  let daemon_a = start(a, peer_of_a);
  let daemon_b = start(b, peer_of_b);
  assert!(
    form_and_settle(&[&daemon_a, &daemon_b]),
    "the fleet's direct probe mesh formed before the test acts"
  );

  let mut client = Client::connect(&instance_a);
  let ReplyBody::Created { id } = client.call(&scratch("sealed")) else {
    daemon_a.stop();
    daemon_b.stop();
    panic!("the volume was not created");
  };
  let object = ObjectId(id.bytes);
  write_hello_over_nfs(&daemon_a, "sealed");
  let ReplyBody::Snapshotted { id: snapshot, .. } =
    client.call(&RequestBody::Snapshot { volume: id })
  else {
    daemon_a.stop();
    daemon_b.stop();
    panic!("the snapshot was not taken");
  };

  let placed = poll_snapshot_placed(&[&daemon_a, &daemon_b], &mut client, id, snapshot);
  let manifest = daemon_a.fleet_head_manifest(object);
  let held =
    matches!(manifest, Ok(Some(manifest)) if daemon_b.fleet_holder_content(manifest) == Ok(true));

  daemon_a.stop();
  daemon_b.stop();
  assert!(
    placed,
    "the sealed snapshot's content and head placed at the f=1 quorum: `await placed(snapshot, region)`"
  );
  assert!(
    matches!(manifest, Ok(Some(_))),
    "the snapshot's manifest identity was recorded once its content placed: {manifest:?}"
  );
  assert!(
    held,
    "B holds the snapshot's content whole, by the manifest identity the head names"
  );
}

/// Shape: how long the first-round content candidate is starved in
/// [`a_slow_first_round_candidate_is_hedged_after_the_measured_p95`] — three liveness budgets, the same hold
/// the SWIM starvation test uses: far past any measured put p95 on loopback (sub-millisecond to tens of
/// milliseconds), so a hedge that fires on the measured p95 places the seal through the third candidate long
/// before the hold ends, while a first round that waits out the hold itself does not.
const HEDGE_STARVATION_NS: u64 = 3 * LIVENESS_BUDGET_NS;

/// The owner's two remote content candidates, read in the exact order used by its
/// owner shard, including committed failure domains. Return indexes into `hosts`:
/// the first remote is the first-round target and the second is the hedge.
fn remote_candidate_indexes(
  owner: &Daemon,
  hosts: &[HostId],
  object: ObjectId,
) -> Option<(usize, usize)> {
  let candidates = owner.placement_candidates(object).ok()?;
  let remote: Vec<usize> = candidates
    .into_iter()
    .filter(|host| *host != hosts[0])
    .filter_map(|host| hosts.iter().position(|h| *h == host))
    .collect();
  Some((*remote.first()?, *remote.get(1)?))
}

/// AC-8.12 / §4.8 / §4.10: measure a first seal's put, then pause its actual first
/// candidate and seal again. The second seal must place before the pause ends, the
/// other candidate must hold its content, and the successful put must add a latency
/// reading. Candidate selection uses the owner's committed configuration, not a
/// reconstruction without its failure domains.
#[test]
fn a_slow_first_round_candidate_is_hedged_after_the_measured_p95() {
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
  let daemons = start_mesh(nodes, &hosts, &certs, &serve);
  let hosts: Vec<HostId> = daemons
    .iter()
    .map(|daemon| daemon.member_identity().unwrap())
    .collect();

  assert_fleet_forms(&daemons, &hosts, &names);
  let all: Vec<&Daemon> = daemons.iter().collect();

  // The first seal: a prompt first round, so A measures the content class's put latency.
  let setup = seal_hello_on_owner(&instance_a, &daemons, "hedged").and_then(|id| {
    let object = ObjectId(id.bytes);
    remote_candidate_indexes(&daemons[0], &hosts, object)
      .map(|(first_index, hedge_index)| (id, object, first_index, hedge_index))
      .ok_or_else(|| "a three-node f = 1 fleet gives the owner two remote candidates".to_owned())
  });
  let (id, object, first_index, hedge_index) = match setup {
    Ok(setup) => setup,
    Err(why) => {
      for daemon in daemons {
        daemon.stop();
      }
      panic!("setup: {why}");
    }
  };
  let measured = daemons[0].fleet_put_latency(object);
  let mut client = Client::connect(&instance_a);

  // Hold the first-round candidate's control shard, then seal again: the second put cannot be answered
  // by it, so the hedge must carry the seal to the other candidate on the measured p95.
  write_hello_over_nfs(&daemons[0], "hedged");
  let hold = daemons[first_index].starve_control_shard(HEDGE_STARVATION_NS);
  let started = Instant::now();
  let ReplyBody::Snapshotted { id: second, .. } =
    client.call(&RequestBody::Snapshot { volume: id })
  else {
    for daemon in daemons {
      daemon.stop();
    }
    panic!("the second snapshot was not taken");
  };
  let placed = poll_snapshot_placed(&all, &mut client, id, second);
  let placed_after = started.elapsed();
  let manifest = daemons[0].fleet_head_manifest(object);
  let outcome = HedgeOutcome {
    measured,
    placed,
    placed_after,
    hedge_holds: matches!(manifest, Ok(Some(manifest)) if daemons[hedge_index].fleet_holder_content(manifest) == Ok(true)),
    held_ns: hold.and_then(|done| done.answer()),
    after: daemons[0].fleet_put_latency(object),
  };

  for daemon in daemons {
    daemon.stop();
  }
  assert_hedged_through_the_measured_p95(&outcome);
}

/// What the hedge history observed: the owner's put-latency readings before the hold and after the
/// hedged seal, whether and when the second seal placed, whether the hedged candidate holds it, and the
/// measured span of the first-round candidate's hold.
#[derive(Debug)]
struct HedgeOutcome {
  measured: Result<(usize, Option<u64>), ObserveError>,
  placed: bool,
  placed_after: Duration,
  hedge_holds: bool,
  held_ns: Result<u64, ObserveError>,
  after: Result<(usize, Option<u64>), ObserveError>,
}

/// The hedge history's assertions: readings existed before the second seal, the hold held for its span,
/// the seal placed through the hedge while the first-round candidate was still held, and the hedged put
/// left further readings.
fn assert_hedged_through_the_measured_p95(outcome: &HedgeOutcome) {
  let HedgeOutcome {
    measured,
    placed,
    placed_after,
    hedge_holds,
    held_ns,
    after,
  } = outcome;
  let held_still_starved = *placed_after < Duration::from_nanos(HEDGE_STARVATION_NS);
  assert!(
    measured
      .as_ref()
      .is_ok_and(|(count, p95)| *count > 0 && p95.is_some()),
    "the first seal left measured put-latency readings on the owner: {measured:?}"
  );
  assert!(
    held_ns
      .as_ref()
      .is_ok_and(|&held| held >= HEDGE_STARVATION_NS),
    "the first-round candidate's control shard was held for the whole span ({held_ns:?} ns)"
  );
  eprintln!(
    "hedge: measured before the hold {measured:?} (readings, p95 ns); second seal placed after \
     {placed_after:?} against a {HEDGE_STARVATION_NS} ns hold; readings after {after:?}"
  );
  assert!(
    *placed && held_still_starved,
    "the second seal placed through the hedge while the first-round candidate was still held \
     (placed={placed} after {placed_after:?}, hold {HEDGE_STARVATION_NS} ns)"
  );
  assert!(
    hedge_holds,
    "the hedged candidate holds the second seal's content whole — the placement came through the hedge"
  );
  assert!(
    matches!((after, measured), (Ok((count, _)), Ok((before, _))) if count > before),
    "the hedged put left further readings: {measured:?} → {after:?}"
  );
}

/// AC (§4.10 "anti-entropy walks Merkle manifests between recorded holders and repairs only differing
/// subtrees; the healer replays puts that never reached f+1"; §4.8 "Derived constants": "healer cadence from
/// the measured put-failure rate"; §4.8 "Recovery": a restarted holder "holds nothing for others until
/// re-replication fills it"): in a three-node `f = 1` fleet a sealed snapshot places on the owner and a
/// recorded holder; the holder then **loses** that content (`drop_held_content` — the state a RAM-only
/// holder is in after a restart, injected because an in-process daemon cannot restart). The owner's healer
/// must re-offer the placed snapshot to its recorded holders, find the loss (the holder's missing set is
/// non-empty), re-put exactly the missing chunks, and the holder must hold the manifest whole again — with
/// no new seal taken. Non-vacuous three ways: the holder is shown to hold the content, then shown **not** to
/// (the loss took hold), then shown to hold it again while the owner's repair count moved from zero (a fresh
/// seal could refill a holder; only the healer moves that count).
#[test]
fn a_holder_that_lost_placed_content_is_repaired_by_the_healer() {
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
  let daemons = start_mesh(nodes, &hosts, &certs, &serve);
  let hosts: Vec<HostId> = daemons
    .iter()
    .map(|daemon| daemon.member_identity().unwrap())
    .collect();

  assert_fleet_forms(&daemons, &hosts, &names);
  let all: Vec<&Daemon> = daemons.iter().collect();

  // Seal on A until the content places; find the recorded holder that holds the manifest.
  let id = match seal_hello_on_owner(&instance_a, &daemons, "healed") {
    Ok(id) => id,
    Err(why) => {
      for daemon in daemons {
        daemon.stop();
      }
      panic!("setup: {why}");
    }
  };
  let object = ObjectId(id.bytes);
  let manifest = daemons[0].fleet_head_manifest(object);
  let holder_index = manifest
    .as_ref()
    .ok()
    .copied()
    .flatten()
    .and_then(|manifest| {
      (1..n).find(|index| daemons[*index].fleet_holder_content(manifest) == Ok(true))
    });
  let (Ok(Some(manifest)), Some(holder_index)) = (&manifest, holder_index) else {
    for daemon in daemons {
      daemon.stop();
    }
    panic!("the sealed content placed on a recorded holder: {manifest:?} {holder_index:?}");
  };
  let manifest = *manifest;
  let repairs_before = daemons[0].fleet_repairs(object);

  // The holder loses the content; the loss is shown to have taken hold.
  let forgotten = daemons[holder_index].drop_held_content(manifest);
  let lost = daemons[holder_index].fleet_holder_content(manifest) == Ok(false);

  // The healer re-offers, finds the loss and re-puts: the holder holds the manifest whole again, and the
  // owner's repair count moved.
  let repaired = poll_until(&all, PLACEMENT_DEADLINE, || {
    let holds = daemons[holder_index].fleet_holder_content(manifest)?;
    let repairs = daemons[0].fleet_repairs(object)?;
    Ok(
      holds
        && repairs_before
          .as_ref()
          .is_ok_and(|&before| repairs > before),
    )
  });
  let repairs_after = daemons[0].fleet_repairs(object);

  for daemon in daemons {
    daemon.stop();
  }
  assert!(
    forgotten == Ok(true) && lost,
    "the recorded holder lost the placed content (forgotten={forgotten:?}, lost={lost})"
  );
  assert!(
    repaired,
    "the healer re-put the lost content and the holder holds it whole again \
     (repairs {repairs_before:?} → {repairs_after:?})"
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
  let hosts: Vec<HostId> = daemons
    .iter()
    .map(|daemon| daemon.member_identity().unwrap())
    .collect();

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
  // What the origin shows for the root's and the file's mode and owner, before it dies.
  let origin_owners = owners_over_nfs(&daemons[0], "served");

  // A dies. The first-ranked survivor takes over the head, then serves the content.
  let owner = daemons.remove(0);
  owner.stop();
  let successor = rendezvous_first(&[hosts[1], hosts[2]], object).expect("a survivor takes over");
  let successor_index = if successor == hosts[1] { 0 } else { 1 };
  let head_placed = poll_head_placed(
    &daemons.iter().collect::<Vec<_>>(),
    &daemons[successor_index],
    object,
  );

  // The successor serves the volume once it materialized it: its `status` answers instead of refusing.
  let successor_instance = daemons[successor_index].instance().to_owned();
  let served = poll_status_answers(&daemons.iter().collect::<Vec<_>>(), &successor_instance, id);
  let (got, successor_owners) = served_content(served, &daemons[successor_index], "served");
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
  assert_eq!(
    successor_owners,
    Some(origin_owners),
    "the successor's root and file carry the origin's mode and owner (the archive carries ownership, \
     format minor 2): [root, hello.txt] as (mode, uid, gid)"
  );
  assert!(
    resealed,
    "a seal taken on the successor after the takeover places — its head written at the promotion \
     epoch the holders fenced the object at, not the successor's lower host epoch"
  );
}

/// What a successor serves once `status` answers there: the file's bytes and the root's and file's
/// (mode, uid, gid) over its NFS port; nothing when it does not serve.
fn served_content(served: bool, daemon: &Daemon, name: &str) -> (Option<Vec<u8>>, Option<Owners>) {
  if !served {
    return (None, None);
  }
  (
    Some(read_hello_over_nfs(daemon, name)),
    Some(owners_over_nfs(daemon, name)),
  )
}

/// Writes a further file into `name` on `daemon` over NFS and seals it, then polls the snapshot's
/// `await placed(region)` on `instance` until it places or the deadline passes. After a takeover this
/// is the proof the successor keeps **writing** the object: its holders fenced the object at the
/// promotion epoch, so a head written at the successor's lower host epoch would be refused `StaleEpoch`
/// and never place.
fn reseal_places(instance: &str, daemon: &Daemon, name: &str, volume: VolumeId) -> bool {
  let port = daemon.nfs_port().expect("the daemon serves NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to the NFS port");
  let root_fh = mount(&mut stream, &capability_path(daemon, name), 1);
  let file_fh = create(&mut stream, &root_fh, "again.txt", 2);
  write(&mut stream, &file_fh, CONTENT, 3);
  drop(stream);
  let mut client = Client::connect(instance);
  let ReplyBody::Snapshotted { id: snapshot, .. } = client.call(&RequestBody::Snapshot { volume })
  else {
    return false;
  };
  poll_snapshot_placed(&[daemon], &mut client, volume, snapshot)
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
    anchor: b.origin_anchor,
    host: b.host,
    address: b.address,
    record_address: b.record_address,
    certificate: b.identity.certificate(),
  };
  let peer_of_b = Peer {
    anchor: a.origin_anchor,
    host: a.host,
    address: a.address,
    record_address: a.record_address,
    certificate: a.identity.certificate(),
  };
  let daemon_a = start_sharded(a, peer_of_a, TWO_SHARDS);
  let daemon_b = start_sharded(b, peer_of_b, TWO_SHARDS);
  assert!(
    form_and_settle(&[&daemon_a, &daemon_b]),
    "the fleet's direct probe mesh formed before the test acts"
  );

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
  let ReplyBody::Snapshotted { id: snapshot, .. } =
    client.call(&RequestBody::Snapshot { volume: id })
  else {
    daemon_a.stop();
    daemon_b.stop();
    panic!("the snapshot was not taken");
  };

  let placed = poll_snapshot_placed(&[&daemon_a, &daemon_b], &mut client, id, snapshot);
  let manifest = daemon_a.fleet_head_manifest(object);
  let held =
    matches!(manifest, Ok(Some(manifest)) if daemon_b.fleet_holder_content(manifest) == Ok(true));

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

/// Starts a two-node `f = 1` fleet of two-shard daemons under one `durability` policy on both nodes and
/// waits for its direct probe mesh to form; returns the daemons and A's instance name. The policy the two
/// durability tests below differ by is the only input that differs between them.
fn start_policed_pair(durability: DurabilityBound) -> (Daemon, Daemon, String) {
  let [pa_probe, pa_record, pb_probe, pb_record] = four_free_ports();
  let a = node("a", pa_probe, pa_record);
  let b = node("b", pb_probe, pb_record);
  let instance_a = format!("fleet-{}-{}", a.host.0, std::process::id());
  let peer_of_a = Peer {
    anchor: b.origin_anchor,
    host: b.host,
    address: b.address,
    record_address: b.record_address,
    certificate: b.identity.certificate(),
  };
  let peer_of_b = Peer {
    anchor: a.origin_anchor,
    host: a.host,
    address: a.address,
    record_address: a.record_address,
    certificate: a.identity.certificate(),
  };
  let daemon_a = start_with_policy(a, peer_of_a, TWO_SHARDS, Some(durability));
  let daemon_b = start_with_policy(b, peer_of_b, TWO_SHARDS, Some(durability));
  let formed = form_and_settle(&[&daemon_a, &daemon_b]);
  if !formed {
    daemon_a.stop();
    daemon_b.stop();
    panic!("the fleet's direct probe mesh formed");
  }
  (daemon_a, daemon_b, instance_a)
}

/// AC (§4.8 "Placement" — "the operator's accepted ε and the coincident-failure size are the durability policy
/// that gates a refusal"; D-14, D-18; D-7 every shard is an owner): in a live two-node `f = 1` fleet whose
/// policy accepts no loss under two coincident failures — which no `f = 1` copyset survives, so the committed
/// configuration's loss is above ε — a volume create is refused `DurabilityUnmet` with the measured loss on
/// **every** owner shard: the control shard, which measured the configuration it installed, and the other
/// shard, which measured the copy fanned to it (a volume named onto it). Reads continue (`List` answers), and
/// the refusals are counted under their kind on the shards that refused. Non-vacuous: the same fleet with a
/// policy the configuration meets creates and seals (the next test) — only the policy differs.
#[test]
fn a_fleet_refuses_a_write_its_configuration_cannot_hold_to_the_declared_durability() {
  let _serial = serialize_fleet_tests();
  let (daemon_a, daemon_b, instance_a) = start_policed_pair(DurabilityBound {
    accepted_loss: 0.0,
    coincident_failures: 2,
  });
  let mut client = Client::connect(&instance_a);

  let on_control = client.call(&scratch(&name_on_partition(
    "short",
    0,
    usize::from(TWO_SHARDS),
  )));
  let on_other = client.call(&scratch(&name_on_partition(
    "short",
    OTHER_PARTITION,
    usize::from(TWO_SHARDS),
  )));
  let listed = client.call(&RequestBody::List);
  let status = client.call(&RequestBody::DaemonStatus);
  daemon_a.stop();
  daemon_b.stop();

  let short = |reply: &ReplyBody| match reply {
    ReplyBody::Refused {
      refusal:
        Refusal::DurabilityUnmet {
          coincident_loss,
          accepted_loss,
          coincident_failures,
        },
    } => *coincident_loss > *accepted_loss && *accepted_loss == 0.0 && *coincident_failures == 2,
    _ => false,
  };
  assert!(
    short(&on_control),
    "the control shard refuses the write with the shortfall it measured at install: {on_control:?}"
  );
  assert!(
    short(&on_other),
    "the other owner shard refuses with the shortfall it measured from the fanned configuration: {on_other:?}"
  );
  assert!(
    matches!(&listed, ReplyBody::Listed { volumes } if volumes.is_empty()),
    "reads continue, and the refused creates created nothing: {listed:?}"
  );
  let ReplyBody::DaemonStatus { report } = status else {
    panic!("status is served while writes are refused");
  };
  let counted: u64 = report
    .shards
    .iter()
    .flat_map(|shard| shard.refusals.iter())
    .filter(|refusal| refusal.kind == "durability_unmet")
    .map(|refusal| refusal.count)
    .sum();
  assert_eq!(
    counted, 2,
    "one counted refusal per refusing shard: {report:?}"
  );
}

/// AC (§4.8 "Placement"; R8 the policy, not a mode, decides): the same two-node `f = 1` fleet under a policy
/// its configuration meets — no loss accepted under a **single** failure, which holds at most one of a
/// copyset's two copies — creates a volume on the non-control shard and seals it as any unpoliced fleet
/// does. The contrast to the refusing fleet above: identical daemons, only the failure count differs.
#[test]
fn a_fleet_within_its_declared_durability_creates_and_seals() {
  let _serial = serialize_fleet_tests();
  let (daemon_a, daemon_b, instance_a) = start_policed_pair(DurabilityBound {
    accepted_loss: 0.0,
    coincident_failures: 1,
  });
  let mut client = Client::connect(&instance_a);
  let created = client.call(&scratch(&name_on_partition(
    "within",
    OTHER_PARTITION,
    usize::from(TWO_SHARDS),
  )));
  let sealed = match &created {
    ReplyBody::Created { id } => Some(client.call(&RequestBody::Snapshot { volume: *id })),
    _ => None,
  };
  daemon_a.stop();
  daemon_b.stop();
  assert!(
    matches!(created, ReplyBody::Created { .. }),
    "a write within the declared durability is accepted: {created:?}"
  );
  assert!(
    matches!(sealed, Some(ReplyBody::Snapshotted { .. })),
    "a seal within the declared durability is accepted: {sealed:?}"
  );
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
  let hosts: Vec<HostId> = daemons
    .iter()
    .map(|daemon| daemon.member_identity().unwrap())
    .collect();

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
  let head_placed = poll_head_placed(
    &daemons.iter().collect::<Vec<_>>(),
    &daemons[successor_index],
    object,
  );
  let served = poll_status_answers(
    &daemons.iter().collect::<Vec<_>>(),
    daemons[successor_index].instance(),
    id,
  );
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
    anchor: b.origin_anchor,
    host: b.host,
    address: b.address,
    record_address: b.record_address,
    certificate: b.identity.certificate(),
  };
  let daemon_a = start(a, peer_of_a);

  // The fleet loop counts the refusal on its first run on the control shard; poll the status for it,
  // bounded, since that run and this client's request are queued on the same shard.
  let mut client = Client::connect(&instance_a);
  let counted = poll_until(&[&daemon_a], Duration::from_secs(5), || {
    Ok(matches!(
      client.call(&RequestBody::DaemonStatus),
      ReplyBody::DaemonStatus { report }
        if report
          .shards
          .iter()
          .flat_map(|shard| shard.refusals.iter())
          .any(|refusal| refusal.kind == "fleet.bind" && refusal.count >= 1)
    ))
  });
  daemon_a.stop();
  assert!(
    counted,
    "the serve socket A could not bind is counted as a `fleet.bind` refusal in A's status"
  );
}

// ---------------------------------------------------------------------------------------------
// The merge plane across the fleet (§4.16 "Commit", "Apply on holders"; §4.10 placed before
// committed; D-27; AC-8.19/T-8.17): a green's merge record is issued only once the inputs it
// names are placed, and every holder recomputes the version before it accepts the record.

/// Shape: the window over which a merge record must be seen **not** to place while its inputs cannot
/// — a couple of dozen coordinator periods, long enough that a record which was going to place would
/// have (a two-node placement takes a few periods on loopback).
const RECORD_HOLD_WINDOW: Duration = Duration::from_secs(2);

/// A two-node `f = 1` fleet: A (the owner the client reaches) and B (the candidate holder).
fn two_node_fleet() -> (Daemon, Daemon, String) {
  let [pa_probe, pa_record, pb_probe, pb_record] = four_free_ports();
  let a = node("a", pa_probe, pa_record);
  let b = node("b", pb_probe, pb_record);
  let pid = std::process::id();
  let instance_a = format!("fleet-{}-{pid}", a.host.0);
  let peer_of_a = Peer {
    anchor: b.origin_anchor,
    host: b.host,
    address: b.address,
    record_address: b.record_address,
    certificate: b.identity.certificate(),
  };
  let peer_of_b = Peer {
    anchor: a.origin_anchor,
    host: a.host,
    address: a.address,
    record_address: a.record_address,
    certificate: a.identity.certificate(),
  };
  let daemon_a = start(a, peer_of_a);
  let daemon_b = start(b, peer_of_b);
  assert!(
    form_and_settle(&[&daemon_a, &daemon_b]),
    "the merge fixture explicitly forms its groups"
  );
  (daemon_a, daemon_b, instance_a)
}

/// Creates a green and a work over it on the owner, edits `f`, and submits: version 1 is committed on
/// the owner, but its **acceptance waits** for the version's merge record to commit at the quorum
/// (§4.16 "Commit"; AUD-11) — and every caller has installed a fault on the holder that withholds
/// that commit — so the bounded wait returns without a reply; the version's record and the client's
/// eventual answer are what the callers then observe. Returns the green and the work.
fn submit_one_version(client: &mut Client) -> (VolumeId, VolumeId) {
  let ReplyBody::GreenCreated { id: green } = client.call(&RequestBody::CreateGreen {
    name: "green".to_owned(),
    require_evidence: false,
    base: None,
  }) else {
    panic!("create green");
  };
  let ReplyBody::WorkCreated { id: work, .. } = client.call(&RequestBody::CreateWork {
    green,
    name: "work".to_owned(),
  }) else {
    panic!("create work");
  };
  let edited = client.call(&RequestBody::Edit {
    work,
    path: "f".to_owned(),
    at: 0,
    delete_len: 0,
    bytes: b"hello".to_vec(),
  });
  assert!(matches!(edited, ReplyBody::Edited), "{edited:?}");
  let submitted = client.try_call(
    &RequestBody::Submit {
      work,
      evidence: Vec::new(),
    },
    u64::try_from(RECORD_HOLD_WINDOW.as_nanos()).unwrap_or(u64::MAX),
  );
  assert!(
    submitted.is_err(),
    "the acceptance waits for the commit the holder withholds; no reply within the hold: {submitted:?}"
  );
  (green, work)
}

/// AC-8.19/T-8.17 and §4.16 "Commit" ("committed at f+1 acknowledgements … issued only when every
/// identity the version references is placed"; §4.10 placed-before-committed): in a two-node `f = 1`
/// fleet the holder B is made to **refuse every content put** (an injected placement refusal). A green's
/// version 0 — nothing to place — commits; version 1's merge record, which names the increment's inputs,
/// is shown **not** to commit over a whole window while its inputs cannot place, the owner counting the
/// wait (`merge.inputs_unplaced`, the non-vacuity counter) and the holder the refusals it caused. The
/// work that produced the version is destroyed meanwhile — its inputs are retained by the chain, not
/// the work. Once the refusal is lifted the inputs place, the record commits at the quorum, and B's
/// replica recomputes version 1 from exactly the placed inputs. Non-vacuous: the record placed only
/// after the lift, and the holder held version 0 alone before it.
#[test]
fn a_merge_record_is_issued_only_once_its_inputs_are_placed() {
  let _serial = serialize_fleet_tests();
  let (daemon_a, daemon_b, instance_a) = two_node_fleet();
  let installed = daemon_b.inject_merge_fault(slates_server::merge_service::MergeFault {
    refuse_content_puts: true,
    corrupt_next_inputs: false,
    refuse_records: false,
  });
  assert!(installed.is_ok(), "the fault installs on B: {installed:?}");
  let mut client = Client::connect(&instance_a);
  let (green, work) = submit_one_version(&mut client);
  let gate = observe_inputs_gate(&daemon_a, &daemon_b, &mut client, green, work);
  daemon_a.stop();
  daemon_b.stop();
  assert_inputs_gate(&gate);
}

/// What the placed-before-reference run showed, before and after the placement refusal is lifted.
struct InputsGate {
  origin_placed: bool,
  held: bool,
  waits: Result<u64, ObserveError>,
  refusals: Result<u64, ObserveError>,
  holder_before: Result<slates_server::merge_service::HolderMergeState, ObserveError>,
  destroyed: bool,
  lifted: Result<(), ObserveError>,
  placed: bool,
  holder_after: Result<slates_server::merge_service::HolderMergeState, ObserveError>,
  awaited_placed: bool,
}

/// Observes version 0 placing, version 1 held back while B refuses puts (the counters on both sides,
/// B's replica at 0, the work destroyed meanwhile), then lifts the fault and observes version 1 place
/// and B's replica recompute it.
fn observe_inputs_gate(
  daemon_a: &Daemon,
  daemon_b: &Daemon,
  client: &mut Client,
  green: VolumeId,
  work: VolumeId,
) -> InputsGate {
  let both = [daemon_a, daemon_b];
  let origin_placed = poll_until(&both, PLACEMENT_DEADLINE, || {
    daemon_a.merge_record_placed(green, 0)
  });
  let held = holds_for(RECORD_HOLD_WINDOW, || {
    daemon_a.merge_record_placed(green, 1).map(|placed| !placed)
  });
  let waits = refusal_count(daemon_a, "merge.inputs_unplaced");
  let refusals = refusal_count(daemon_b, "merge.content_put_refused");
  let holder_before = daemon_b.merge_holder_state(green);
  let destroyed = matches!(
    client.call(&RequestBody::Destroy { volume: work }),
    ReplyBody::Destroyed
  );
  let lifted = daemon_b.inject_merge_fault(slates_server::merge_service::MergeFault::default());
  let placed = poll_until(&both, PLACEMENT_DEADLINE, || {
    daemon_a.merge_record_placed(green, 1)
  });
  let holder_after = daemon_b.merge_holder_state(green);
  // The commit answered the submit that waited: its late reply is discarded so the next call reads its
  // own (the acceptance is answered exactly once the version commits — AUD-11).
  client.drain();
  let awaited_placed = matches!(
    client.call(&RequestBody::AwaitPlaced {
      volume: green,
      snapshot: None,
      scope: Scope::Region,
    }),
    ReplyBody::Placed { placed: true, .. }
  );
  InputsGate {
    origin_placed,
    held,
    waits,
    refusals,
    holder_before,
    destroyed,
    lifted,
    placed,
    holder_after,
    awaited_placed,
  }
}

/// The placed-before-reference assertions over one run: what held while the puts were refused, then
/// what followed the lift.
fn assert_inputs_gate(gate: &InputsGate) {
  assert_gate_held(gate);
  assert_gate_lifted(gate);
}

/// While B refused every content put: version 0 committed, version 1 never did, both sides counted.
fn assert_gate_held(gate: &InputsGate) {
  assert!(gate.origin_placed, "version 0 (nothing to place) commits");
  assert!(
    gate.held,
    "version 1's record never commits while its inputs cannot place"
  );
  // Both counts must be observed: an absent kind is the only zero, and an unobserved map proves nothing.
  assert!(
    matches!(gate.waits, Ok(waits) if waits > 0),
    "the owner counted the wait: {:?}",
    gate.waits
  );
  assert!(
    matches!(gate.refusals, Ok(refusals) if refusals > 0),
    "the holder counted its refusals: {:?}",
    gate.refusals
  );
  assert_eq!(
    gate.holder_before,
    Ok(slates_server::merge_service::HolderMergeState {
      version: Some(0),
      refused: false,
    }),
    "the holder recomputed version 0 alone before the lift"
  );
}

/// After the lift: the destroyed work retained nothing the record needed, the inputs placed, the
/// record committed, the holder recomputed version 1, and `await placed` says so.
fn assert_gate_lifted(gate: &InputsGate) {
  assert!(
    gate.destroyed,
    "the work is destroyed while its version waits"
  );
  assert!(
    gate.lifted.is_ok(),
    "the fault lifts on B: {:?}",
    gate.lifted
  );
  assert!(
    gate.placed,
    "once the inputs place, the record commits at the quorum"
  );
  assert_eq!(
    gate.holder_after,
    Ok(slates_server::merge_service::HolderMergeState {
      version: Some(1),
      refused: false,
    }),
    "the holder recomputed version 1 from the placed inputs (the destroyed work retained nothing)"
  );
  assert!(
    gate.awaited_placed,
    "await placed(green, region) answers from the merge records"
  );
}

/// §4.16 "Apply on holders" ("recomputes the verdict and the manifest identity … before serving the
/// version … mismatch refuses that version on that holder, fatal-and-loud"; D-27): in a two-node fleet
/// the holder B's copy of a version's inputs is corrupted (the post-state a byte off — the ops document
/// still decodes, the bytes it names differ). B recomputes version 1, its head identity does not match
/// the record's, and it refuses: counted (`merge.recompute_mismatch`), the green refused on B for good,
/// nothing of it served (no replica), and — at `f = 1` — the record cannot reach its quorum, so the owner
/// reports version 1 unplaced (Degraded, never a false placement). Non-vacuous: version 0 commits and
/// replicates first, so the refusal is the recomputation's, not a transport failure.
#[test]
fn a_holder_whose_recomputation_mismatches_refuses_the_version_loudly() {
  let _serial = serialize_fleet_tests();
  let (daemon_a, daemon_b, instance_a) = two_node_fleet();
  let both = [&daemon_a, &daemon_b];
  let installed = daemon_b.inject_merge_fault(slates_server::merge_service::MergeFault {
    refuse_content_puts: false,
    corrupt_next_inputs: true,
    refuse_records: false,
  });
  assert!(installed.is_ok(), "the fault installs on B: {installed:?}");
  let mut client = Client::connect(&instance_a);
  let (green, _) = submit_one_version(&mut client);

  let origin_placed = poll_until(&both, PLACEMENT_DEADLINE, || {
    daemon_a.merge_record_placed(green, 0)
  });
  let refused = poll_until(&both, PLACEMENT_DEADLINE, || {
    refusal_count(&daemon_b, "merge.recompute_mismatch").map(|count| count >= 1)
  });
  let held = holds_for(RECORD_HOLD_WINDOW, || {
    daemon_a.merge_record_placed(green, 1).map(|placed| !placed)
  });
  let holder = daemon_b.merge_holder_state(green);
  let awaited = client.call(&RequestBody::AwaitPlaced {
    volume: green,
    snapshot: None,
    scope: Scope::Region,
  });
  daemon_a.stop();
  daemon_b.stop();

  assert!(origin_placed, "version 0 commits and replicates");
  assert!(
    refused,
    "the holder refused the mismatching recomputation, counted"
  );
  assert!(
    held,
    "a version one holder refused cannot reach the f = 1 quorum: never reported placed"
  );
  assert_eq!(
    holder,
    Ok(slates_server::merge_service::HolderMergeState {
      version: None,
      refused: true,
    }),
    "the green is refused on the holder for good and nothing of it is served"
  );
  assert!(
    matches!(awaited, ReplyBody::Placed { placed: false, .. }),
    "the owner reports the version unplaced: {awaited:?}"
  );
}

/// AC-8.1, §4.8, KIND retirement regression: restart B with the same certificate and
/// addresses but a fresh member id. Retiring its predecessor must preserve the replacement's
/// authenticated serve session and mutual discovery. A two-node loss cannot establish safe
/// voter admission; the three-node test below separately requires that consensus transition.
/// The original same-id failure and measurements remain in the dated retirement bug report.
#[test]
fn a_fresh_restart_keeps_its_serve_session_while_the_predecessor_retires() {
  let _serial = serialize_fleet_tests();
  let [pa_probe, pa_record, pb_probe, pb_record] = four_free_ports();
  let a = node("a", pa_probe, pa_record);
  let b = node("b", pb_probe, pb_record);
  let host_b = b.host;
  let anchor_b = b.origin_anchor;
  let profile_b = b.profile.clone();
  let mut b_identities = same_identity(2).into_iter();
  let (b_first, b_again_id) = (b_identities.next().unwrap(), b_identities.next().unwrap());
  let b = Node {
    identity: b_first,
    ..b
  };
  // Same certificate and addresses, fresh RAM and voting identity; the manifest seed only routes contact.
  let b_again = Node {
    profile: profile_b,
    host: host_b,
    origin_anchor: anchor_b,
    identity: b_again_id,
    address: SocketAddrV4::new(Ipv4Addr::LOCALHOST, pb_probe),
    record_address: SocketAddrV4::new(Ipv4Addr::LOCALHOST, pb_record),
  };
  let peer_of_a = Peer {
    anchor: b.origin_anchor,
    host: b.host,
    address: b.address,
    record_address: b.record_address,
    certificate: b.identity.certificate(),
  };
  let peer_of_b = Peer {
    anchor: a.origin_anchor,
    host: a.host,
    address: a.address,
    record_address: a.record_address,
    certificate: a.identity.certificate(),
  };
  let daemon_a = start(a, peer_of_a);
  let host_a = daemon_a.member_identity().unwrap();
  let daemon_b = start(b, peer_of_b.clone());
  let host_b = daemon_b.member_identity().unwrap();
  let formed = form_and_settle(&[&daemon_a, &daemon_b]);
  let knew_b = daemon_a
    .fleet_members()
    .is_ok_and(|members| members.contains(&host_b));
  daemon_b.stop();
  let daemon_b_again = start(b_again, peer_of_b);
  let fresh_b = daemon_b_again.member_identity().unwrap();
  assert_ne!(fresh_b, host_b);

  // The restart re-forms the mesh; then A ages out its stale probe session to the dead predecessor and
  // **transiently retires B** — the retirement whose cleanup used to close the restart's freshly-bound serve
  // session. That session must survive so A echoes its death belief and B self-refutes; A then re-admits B
  // and the two settle. Mirror the false-death rejoin structure (retire → rejoin → stable), the difference
  // being a natural retirement of a real restart rather than an injected false death.
  let reformed = poll_until(&[&daemon_a, &daemon_b_again], FORMATION_DEADLINE, || {
    daemon_a.fleet_members().map(|m| m.contains(&fresh_b))
  });
  // Non-vacuous: A actually retires B (the stale session ages out), so this exercises the retirement whose
  // cleanup held the bug — not merely a formation that never retired anything.
  let retired = poll_until(&[&daemon_a, &daemon_b_again], RETIREMENT_DEADLINE, || {
    daemon_a.fleet_members().map(|m| !m.contains(&host_b))
  });
  // The restart is re-admitted through the death-echo/self-refutation path (its serve session survived), and
  // A and B settle into stable mutual knowledge — the state the certificate-keyed close destroyed by tearing
  // the restart's session down, leaving the two in a circular wait, both `fleet_meshed` vacuously.
  let mutual = || {
    Ok(
      daemon_a.fleet_members()?.contains(&fresh_b)
        && daemon_b_again.fleet_members()?.contains(&host_a),
    )
  };
  let rejoined = poll_until(&[&daemon_a, &daemon_b_again], REJOIN_DEADLINE, mutual);
  let stable = rejoined && holds_for(FORMATION_SETTLE, mutual);

  daemon_a.stop();
  daemon_b_again.stop();
  assert!(formed, "the fleet formed before the restart");
  assert!(knew_b, "A knew B before the restart");
  assert!(
    reformed,
    "the fresh restart re-formed the mesh (A knows the replacement)"
  );
  assert!(
    retired,
    "A retired the dead incarnation (the natural retirement whose cleanup held the bug ran)"
  );
  assert!(
    rejoined,
    "A re-admitted the restart and B kept A — the restart's serve session survived A's retirement, so the \
     death echo reached it and it self-refuted (the certificate-keyed close of the replacement is fixed)"
  );
  assert!(stable, "the re-admission held — the rejoin did not flap");
}

/// AC-8.1, §4.8, AUD-07: lose a whole anchor, rejoin through the surviving council under a
/// fresh identity, then lose the leader and require the replacement's vote for the next election.
/// The loss is **forced into the interrupted-discovery phase**: the victim holds its discovery replies
/// after their requests arrive (`Daemon::inject_discovery_fault`), so every survivor's refresh exchange to
/// it is pending — its record endpoint borrowed by that exchange — when the victim dies. That is the shape
/// whose unbounded wait stranded the replacement (a datagram socket reports no terminal error for a peer
/// whose keys are gone; the link task that alone notices the replacement and re-dials never returned to its
/// loop: 271 replication attempts, no append, the old voter set on the replacement —
/// `docs/bugs/2026-09-16-discovery-await-strands-a-replacement-raft-voter.md`). Expect, before the voter
/// set converges: each survivor's exchange ends typed (at its deadline, or invalidated by the replacement's
/// identity), each survivor holds a record link to the fresh member, and the leader's appends reach it (its
/// council contact climbs). Non-vacuous: the links to the victim are shown borrowed before it is stopped.
#[test]
fn a_whole_ram_replacement_joins_as_a_fresh_voter_and_commits_after_another_loss() {
  let _serial = serialize_fleet_tests();
  let pid = std::process::id();
  let profiles: Vec<MachineProfile> = ["audit-voter-a", "audit-voter-b", "audit-voter-c"]
    .into_iter()
    .map(profile)
    .collect();
  let anchors: Vec<HostId> = profiles.iter().map(anchor_of).collect();
  let seeds: Vec<HostId> = anchors.iter().map(|anchor| member_id(*anchor, 0)).collect();
  let mut identities: Vec<Vec<Identity>> = profiles.iter().map(|_| same_identity(2)).collect();
  let certificates: Vec<_> = identities
    .iter()
    .map(|pair| pair[0].certificate())
    .collect();
  let ports = mesh_serve_ports(profiles.len());
  let peers = |node: usize| -> Vec<FleetPeer> {
    profiles
      .iter()
      .enumerate()
      .filter(|(peer, _)| *peer != node)
      .map(|(peer, _)| fleet_peer_at(anchors[peer], seeds[peer], ports[peer], &certificates[peer]))
      .collect()
  };
  let configs: Vec<_> = profiles
    .iter()
    .enumerate()
    .map(|(node, profile)| {
      fleet_config(
        profile,
        &format!("audit-voter-{node}-{pid}"),
        anchors[node],
        seeds[node],
        &peers(node),
        &Default::default(),
      )
    })
    .collect();
  let mut daemons: Vec<Daemon> = profiles
    .iter()
    .enumerate()
    .map(|(node, profile)| {
      start_fleet_node(
        profile,
        configs[node].clone(),
        identities[node].pop().unwrap(),
        ports[node],
        peers(node),
        SegmentSource::Create {
          name: format!("audit-voter-{node}-{pid}"),
        },
      )
    })
    .collect();
  let before: Vec<HostId> = daemons
    .iter()
    .map(|daemon| daemon.member_identity().unwrap())
    .collect();
  assert!(
    daemons
      .iter()
      .all(|daemon| daemon.council_leads() == Ok(false) && daemon.root_leads() == Ok(false)),
    "startup cannot bootstrap from the manifest"
  );
  daemons[0]
    .bootstrap(true)
    .expect("the operator explicitly creates this test's new fleet");
  settle_initial_consensus(&daemons, &seeds, &Default::default(), Quorum { f: 1 });
  assert!(
    audit_wait(|| all_hold(daemons.iter().map(|daemon| {
      daemon
        .council_members()
        .map(|members| before.iter().all(|host| members.contains(host)))
    }))),
    "the first bootstrap admits the live identities: {:?}",
    daemons
      .iter()
      .map(Daemon::council_members)
      .collect::<Vec<_>>()
  );
  // Keep the root's only voter alive: losing that sole copy must remain unavailable. This
  // history exercises the regional group's f=1 tolerance and its fresh-member admission.
  let replaced = daemons
    .iter()
    .position(|daemon| daemon.root_leads() == Ok(false))
    .unwrap();
  let old = before[replaced];
  hold_the_victims_discovery_replies(&daemons, replaced, old);
  daemons.remove(replaced).stop();
  let survivors = daemons.len();
  let replacement = start_fleet_node(
    &profiles[replaced],
    configs[replaced].clone(),
    identities[replaced].pop().unwrap(),
    ports[replaced],
    peers(replaced),
    SegmentSource::Create {
      name: format!("audit-voter-replacement-{pid}"),
    },
  );
  let fresh = replacement.member_identity().unwrap();
  assert_ne!(
    old, fresh,
    "the same certificate with a new anchor is a different member"
  );
  assert_eq!(replacement.council_leads(), Ok(false));
  daemons.push(replacement);
  assert!(
    audit_wait(|| all_hold(daemons.iter().map(|daemon| {
      daemon.council_members().map(|members| {
        members.len() == profiles.len() && members.contains(&fresh) && !members.contains(&old)
      })
    }))),
    "the surviving quorum admits the fresh member: {:?}",
    daemons
      .iter()
      .map(Daemon::council_members)
      .collect::<Vec<_>>()
  );
  assert_survivors_reach_the_replacement(&daemons, survivors, fresh);
  let voters: Vec<HostId> = daemons
    .iter()
    .map(|daemon| daemon.member_identity().unwrap())
    .collect();
  assert!(
    audit_wait(|| audit_voters_match(&daemons, &voters)),
    "every replacement voter has received the committed membership transition: {:?}",
    daemons
      .iter()
      .map(Daemon::council_voters)
      .collect::<Vec<_>>()
  );
  assert!(audit_wait(|| one_leader(&daemons)));
  let victim = daemons
    .iter()
    .position(|daemon| daemon.council_leads() == Ok(true) && daemon.member_identity() != Ok(fresh))
    .unwrap_or_else(|| {
      daemons
        .iter()
        .position(|daemon| daemon.member_identity() != Ok(fresh))
        .unwrap()
    });
  let lost = daemons[victim].member_identity().unwrap();
  let lost_was_leader = daemons[victim].council_leads() == Ok(true);
  daemons.remove(victim).stop();
  let remaining: Vec<HostId> = daemons
    .iter()
    .map(|daemon| daemon.member_identity().unwrap())
    .collect();
  // Targeted capture: on failure, dump every survivor's council and SWIM state before stopping them, so a
  // stalled second-loss commit names its stage (detection, election, or replication) instead of a bare
  // timeout (docs/wip/TBD_FIXES.md §1).
  let committed = audit_wait(|| audit_voters_match(&daemons, &remaining));
  let snapshot = council_snapshot(&daemons);
  for daemon in daemons {
    daemon.stop();
  }
  assert!(
    committed,
    "the surviving pair commits another membership change, which requires the fresh voter's \
     acknowledgement; lost {lost:?} (was_leader={lost_was_leader}), fresh {fresh:?}, remaining \
     {remaining:?}\n{snapshot}"
  );
}

/// A per-daemon dump of council and SWIM state for a membership-commit diagnosis: the committed voter set
/// (`council_voters`), the applied regional members, the SWIM alive view (does it still see a lost peer?),
/// leadership, the leader-contact counter (whether appends are flowing), the mesh, the record links, and the
/// non-zero refusal counts. Every field is the typed observation, so an unreachable shard shows as its error
/// rather than a default.
fn council_snapshot(daemons: &[Daemon]) -> String {
  daemons
    .iter()
    .map(|daemon| {
      let refusals = daemon.fleet_refusals().map(|counts| {
        counts
          .into_iter()
          .filter(|(_, count)| *count > 0)
          .collect::<std::collections::BTreeMap<_, _>>()
      });
      format!(
        "  {}: id={:?} leads={:?} voters={:?} members={:?} alive={:?} contact={:?} meshed={:?} \
         links={:?} refusals={:?}\n      raft={}",
        daemon.instance(),
        daemon.member_identity(),
        daemon.council_leads(),
        daemon.council_voters(),
        daemon.council_members(),
        daemon.fleet_members(),
        daemon.council_contact(),
        daemon.fleet_meshed(),
        daemon.fleet_record_links(),
        refusals,
        daemon
          .council_debug()
          .unwrap_or_else(|error| format!("<unobserved: {error}>")),
      )
    })
    .collect::<Vec<_>>()
    .join("\n")
}

/// AC-8.1 / T-2.14, §4.8: restart every process while its anchor retains RAM. Both groups
/// must recover without bootstrap, then commit another retirement after one voter is lost.
#[test]
fn a_warm_fleet_restart_recovers_its_root_and_regional_quorums() {
  let _serial = serialize_fleet_tests();
  let pid = std::process::id();
  let profiles: Vec<_> = ["warm-voter-a", "warm-voter-b", "warm-voter-c"]
    .into_iter()
    .map(profile)
    .collect();
  let anchors: Vec<_> = profiles.iter().map(anchor_of).collect();
  let seeds: Vec<_> = anchors.iter().map(|anchor| member_id(*anchor, 0)).collect();
  let mut identities: Vec<_> = profiles.iter().map(|_| same_identity(2)).collect();
  let certificates: Vec<_> = identities
    .iter()
    .map(|pair| pair[0].certificate())
    .collect();
  let ports = mesh_serve_ports(profiles.len());
  let peers = |node| {
    profiles
      .iter()
      .enumerate()
      .filter(|(peer, _)| *peer != node)
      .map(|(peer, _)| fleet_peer_at(anchors[peer], seeds[peer], ports[peer], &certificates[peer]))
      .collect::<Vec<_>>()
  };
  let configs: Vec<_> = profiles
    .iter()
    .enumerate()
    .map(|(node, profile)| {
      fleet_config(
        profile,
        &format!("warm-voter-{node}-{pid}"),
        anchors[node],
        seeds[node],
        &peers(node),
        &Default::default(),
      )
    })
    .collect();
  let segments: Vec<_> = profiles
    .iter()
    .enumerate()
    .map(|(node, profile)| {
      slates_anchor::AnchorSegment::create(
        &format!("warm-voter-{node}-{pid}"),
        &profile.facts.identity,
        configs[node].geometry,
      )
      .unwrap()
    })
    .collect();
  let source = |node: usize| {
    let (handoff, len) = segments[node].handoff().unwrap();
    SegmentSource::Handoff {
      handoff,
      len,
      content: None,
    }
  };
  let mut daemons: Vec<_> = profiles
    .iter()
    .enumerate()
    .map(|(node, profile)| {
      start_fleet_node(
        profile,
        configs[node].clone(),
        identities[node].pop().unwrap(),
        ports[node],
        peers(node),
        source(node),
      )
    })
    .collect();
  daemons[0].bootstrap(true).unwrap();
  settle_initial_consensus(&daemons, &seeds, &Default::default(), Quorum { f: 1 });
  let members: Vec<_> = daemons
    .iter()
    .map(|daemon| daemon.member_identity().unwrap())
    .collect();
  for daemon in daemons.drain(..) {
    daemon.stop();
  }
  for (node, profile) in profiles.iter().enumerate() {
    daemons.push(start_fleet_node(
      profile,
      configs[node].clone(),
      identities[node].pop().unwrap(),
      ports[node],
      peers(node),
      source(node),
    ));
  }
  for (daemon, member) in daemons.iter().zip(&members) {
    assert_eq!(
      daemon.member_identity(),
      Ok(*member),
      "a warm restart retains its voter"
    );
  }
  assert!(
    audit_wait(|| Ok(
      audit_voters_match(&daemons, &members)?
        && one_root_leader(&daemons)?
        && one_leader(&daemons)?
    )),
    "both retained quorums recover without bootstrap"
  );
  let victim = daemons
    .iter()
    .position(|daemon| daemon.root_leads() == Ok(true))
    .unwrap();
  daemons.remove(victim).stop();
  let survivors: Vec<_> = daemons
    .iter()
    .map(|daemon| daemon.member_identity().unwrap())
    .collect();
  assert!(
    audit_wait(|| audit_voters_match(&daemons, &survivors)),
    "the restarted voters must commit a new retirement"
  );
  assert!(
    daemons
      .iter()
      .all(|daemon| daemon.root_leads() == Ok(false)),
    "losing the root's only retained voter cannot trigger automatic bootstrap"
  );
  recover_root_from_survivors(&daemons);
  let representative = *survivors.iter().min_by_key(|host| host.0).unwrap();
  assert!(
    audit_wait(|| Ok(
      all_hold(daemons.iter().map(|daemon| {
        daemon
          .root_voters()
          .map(|voters| voters == Some(vec![representative]))
      }))?
        && one_root_leader(&daemons)?
    )),
    "operator-reviewed recovery must admit the other survivor and commit the new representative"
  );
  for daemon in daemons {
    daemon.stop();
  }
}

/// Approve the most complete reachable root copy, then explicitly join the other survivors to it.
fn recover_root_from_survivors(daemons: &[Daemon]) {
  let mut clients: Vec<_> = daemons
    .iter()
    .map(|daemon| Client::connect(daemon.instance()))
    .collect();
  let plans: Vec<_> = clients
    .iter_mut()
    .map(|client| {
      let reply = client.call(&RequestBody::RecoveryPlan {
        root: true,
        target: None,
      });
      let ReplyBody::RecoveryPlan { plan } = reply else {
        panic!("{reply:?}");
      };
      plan
    })
    .collect();
  let selected = plans
    .iter()
    .enumerate()
    .max_by_key(|(_, plan)| plan.version)
    .unwrap()
    .0;
  let proof = slates_server::recovery_proof(
    &daemons[selected].segment().issuer_secret().unwrap(),
    &plans[selected].digest,
  );
  let reply = clients[selected].call(&RequestBody::Recover {
    root: true,
    target: None,
    plan: plans[selected].digest,
    proof,
  });
  let ReplyBody::RecoveryStarted {
    group,
    joining: false,
  } = reply
  else {
    panic!("{reply:?}");
  };
  for (node, client) in clients
    .iter_mut()
    .enumerate()
    .filter(|(node, _)| *node != selected)
  {
    let reply = client.call(&RequestBody::RecoveryPlan {
      root: true,
      target: Some(group),
    });
    let ReplyBody::RecoveryPlan { plan } = reply else {
      panic!("{reply:?}");
    };
    let proof = slates_server::recovery_proof(
      &daemons[node].segment().issuer_secret().unwrap(),
      &plan.digest,
    );
    assert!(matches!(
      client.call(&RequestBody::Recover {
        root: true,
        target: Some(group),
        plan: plan.digest,
        proof
      }),
      ReplyBody::RecoveryStarted { joining: true, .. }
    ));
  }
}

/// Forces the interrupted-discovery phase on the whole-RAM history
/// (`docs/bugs/2026-09-16-discovery-await-strands-a-replacement-raft-voter.md`): the victim at `replaced`
/// holds its discovery replies after their requests arrive, and every survivor's record link to it (`old`)
/// is shown **borrowed** by that pending exchange — the state the victim is then stopped in. Non-vacuous:
/// before the exchange was bounded, that borrow held the endpoint and the link task for good.
fn hold_the_victims_discovery_replies(daemons: &[Daemon], replaced: usize, old: HostId) {
  let installed = daemons[replaced].inject_discovery_fault(true);
  assert!(
    installed.is_ok(),
    "the withhold fault installs on the victim: {installed:?}"
  );
  assert!(
    audit_wait(
      || all_hold(daemons.iter().enumerate().map(|(node, daemon)| {
        if node == replaced {
          return Ok(true);
        }
        daemon.fleet_record_links().map(|links| {
          links
            .iter()
            .any(|(host, borrowed)| *host == old && *borrowed)
        })
      }))
    ),
    "each survivor's record link to the victim is borrowed by a pending discovery exchange: {:?}",
    daemons
      .iter()
      .map(Daemon::fleet_record_links)
      .collect::<Vec<_>>()
  );
}

/// Whether a daemon's interrupted discovery exchange ended typed — at its deadline, or invalidated.
fn discovery_ended_typed(daemon: &Daemon) -> Result<bool, ObserveError> {
  daemon.fleet_refusals().map(|refusals| {
    refusals
      .get("fleet.discovery.deadline")
      .copied()
      .unwrap_or(0)
      + refusals
        .get("fleet.discovery.invalidated")
        .copied()
        .unwrap_or(0)
      >= 1
  })
}

/// After the replacement (`daemons[survivors]`, admitted as `fresh`) is up: each survivor's interrupted
/// exchange to the dead incarnation ended typed and released its endpoint, each survivor holds a record
/// link to the fresh member, and the leader's appends reach it (its council contact climbs) — the
/// deliveries the unbounded exchange had stranded (271 replication attempts, no append).
fn assert_survivors_reach_the_replacement(daemons: &[Daemon], survivors: usize, fresh: HostId) {
  assert!(
    audit_wait(|| all_hold(daemons[..survivors].iter().map(discovery_ended_typed))),
    "each survivor's discovery exchange to the dead incarnation ended typed: {:?}",
    daemons[..survivors]
      .iter()
      .map(Daemon::fleet_refusals)
      .collect::<Vec<_>>()
  );
  assert!(
    audit_wait(|| all_hold(daemons[..survivors].iter().map(|daemon| {
      daemon
        .fleet_record_links()
        .map(|links| links.iter().any(|(host, _)| *host == fresh))
    }))),
    "each survivor holds a record link to the fresh member: {:?}",
    daemons[..survivors]
      .iter()
      .map(Daemon::fleet_record_links)
      .collect::<Vec<_>>()
  );
  let contact_at_admission = daemons[survivors]
    .council_contact()
    .expect("the replacement's council contact is observed at its admission");
  assert!(
    audit_wait(|| daemons[survivors]
      .council_contact()
      .map(|contact| contact > contact_at_admission)),
    "the leader's appends reach the replacement: its council contact climbs past {contact_at_admission}"
  );
}

/// The audit run has a strict wall-clock bound, including when the normal fleet period budget
/// would keep diagnosing a live but non-converging coordinator. The bound is the existing formation SLO.
/// The verdicts are [`poll_until`]'s: an ask the daemons could not answer keeps the wait going, paced; one
/// they can never answer ends it at once, saying why.
#[track_caller]
fn audit_wait(mut condition: impl FnMut() -> Result<bool, ObserveError>) -> bool {
  let site = std::panic::Location::caller();
  let began = Instant::now();
  let (_pace_sender, pace) = std::sync::mpsc::channel::<()>();
  while began.elapsed() < FORMATION_DEADLINE {
    match verdict(condition()) {
      Verdict::Holds => return true,
      Verdict::Observed => {}
      Verdict::Unavailable(_) => {
        let _ = pace.recv_timeout(UNAVAILABLE_PACE);
      }
      Verdict::Terminal(refusal) => {
        eprintln!(
          "audit wait {} ended: the daemon can never answer — {refusal}",
          site_of(site)
        );
        return false;
      }
    }
    std::thread::yield_now();
  }
  false
}

/// Every node has received the same committed voter set, with no joint transition remaining (a voter set
/// still in transition is observed state, `Ok(false)`; a node that could not be observed is its refusal).
fn audit_voters_match(daemons: &[Daemon], voters: &[HostId]) -> Result<bool, ObserveError> {
  all_hold(daemons.iter().map(|daemon| {
    daemon.council_voters().map(|current| {
      current.is_some_and(|current| {
        current.len() == voters.len() && voters.iter().all(|voter| current.contains(voter))
      })
    })
  }))
}

/// AC-8.18 / T-8.20: a third node absent from every running seed manifest enrolls under the
/// operator's certificate authority, discovers peers through one seed and joins the existing quorum.
#[test]
fn an_unlisted_node_enrolls_through_one_seed_and_joins_the_existing_quorum() {
  let _serial = serialize_fleet_tests();
  let issuer_key = rcgen::KeyPair::generate().unwrap();
  let mut issuer_params = rcgen::CertificateParams::new(vec![NAME.to_owned()]).unwrap();
  issuer_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
  let issuer = issuer_params.self_signed(&issuer_key).unwrap();
  let addresses = mesh_serve_ports(3);
  let mut identities = Vec::new();
  for domain in 0..addresses.len() {
    let key = rcgen::KeyPair::generate().unwrap();
    let params =
      rcgen::CertificateParams::new(vec![NAME.to_owned(), format!("r0.d{domain}.{NAME}")]).unwrap();
    let cert = params.signed_by(&key, &issuer, &issuer_key).unwrap();
    identities.push(Identity::from_der(
      cert.der().clone(),
      rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()),
    ));
  }
  let certificates: Vec<_> = identities.iter().map(Identity::certificate).collect();
  let anchors: Vec<_> = certificates
    .iter()
    .map(slates_server::deploy::host_id_of_certificate)
    .collect();
  let mut daemons = Vec::new();
  for (node, identity) in identities.into_iter().enumerate() {
    let seed_nodes: Vec<usize> = if node == 0 {
      Vec::new()
    } else {
      vec![node - 1]
    };
    let peers: Vec<_> = seed_nodes
      .iter()
      .map(|&seed| FleetPeer {
        anchor: anchors[seed],
        host: member_id(anchors[seed], 0),
        address: loopback(addresses[seed].0).into(),
        record_address: loopback(addresses[seed].1).into(),
        certificate: certificates[seed].clone(),
      })
      .collect();
    let profile = profile("unlisted");
    let instance = format!("unlisted-{node}-{}", std::process::id());
    let domains = anchors
      .iter()
      .enumerate()
      .map(|(domain, anchor)| (member_id(*anchor, 0), u64::try_from(domain).unwrap()))
      .collect();
    let config = DaemonConfig::derive(&profile, &instance, Some(1)).with_fleet(FleetMembership {
      quorum: Quorum { f: 1 },
      peers: peers.iter().map(|peer| peer.host).collect(),
      host: member_id(anchors[node], 0),
      origin_anchor: anchors[node],
      domains,
      regions: std::collections::BTreeMap::new(),
      durability: None,
      region_mirrors: std::collections::BTreeMap::new(),
    });
    let transport = FleetTransport {
      identity,
      name: NAME.to_owned(),
      advertise: loopback(addresses[node].0).into(),
      probe_bind: loopback(addresses[node].0),
      record_bind: loopback(addresses[node].1),
      peers,
      resolver: None,
      enrollment_roots: vec![issuer.der().clone()],
    };
    let daemon = Daemon::start_with_fleet(
      &profile,
      config,
      SegmentSource::Create {
        name: format!("slates-seg-{instance}"),
      },
      Some(transport),
    )
    .unwrap();
    if node == 0 {
      daemon.bootstrap(true).unwrap();
    }
    daemons.push(daemon);
  }
  let hosts: std::collections::BTreeSet<_> = daemons
    .iter()
    .map(|daemon| daemon.member_identity().unwrap())
    .collect();
  let joined = audit_wait(|| {
    all_hold(daemons.iter().map(|daemon| {
      let voters = daemon.council_voters()?;
      let meshed = daemon.fleet_meshed()?;
      Ok(
        meshed
          && voters.is_some_and(|voters| {
            voters
              .into_iter()
              .collect::<std::collections::BTreeSet<_>>()
              == hosts
          }),
      )
    }))
  });
  for daemon in daemons {
    daemon.stop();
  }
  assert!(
    joined,
    "unlisted peers never joined the existing three-voter group"
  );
}

/// Rejoin design item 3 (2026-09-14; `docs/bugs/2026-09-14-retirement-closes-the-same-id-restarts-serve-session.md`):
/// a dial still in its handshake when its peer is retired is dropped with the probe session, so the
/// peer's return is dialed **afresh** — at the address discovery holds for it by then, as a rescheduled
/// pod's new IP is — instead of the pending flight being driven through the remaining handshake budgets
/// at the stale address first. A starts with B named at addresses nobody serves, so its dial to B pends;
/// B's death is injected (what a third node's gossip carries on a lane) and A retires B — the pending
/// dial must be dropped, and is counted (`fleet.dial.stale_dropped`), the non-vacuity of this test; then
/// B starts at **other** addresses under its certificate and announces them, and A must mesh to it under
/// its fresh id. Whether A's first dial after the resume already finds the announced address or still
/// the manifest's depends on which of B's first two exchanges lands first, so the re-dial count is not
/// asserted; the drop is what the fix adds, and the mesh is what it must not break.
#[test]
fn a_retired_peers_pending_dial_is_dropped_and_its_return_at_new_addresses_is_meshed() {
  let _serial = serialize_fleet_tests();
  let [pa_probe, pa_record, pb_probe, pb_record] = four_free_ports();
  let [pb_probe_again, pb_record_again, _, _] = four_free_ports();
  let a = node("a", pa_probe, pa_record);
  let b = node("b", pb_probe, pb_record);
  let mut b_identities = same_identity(2).into_iter();
  let (Some(b_first), Some(b_again_identity)) = (b_identities.next(), b_identities.next()) else {
    panic!("B's identity twice");
  };
  let peer_of_a = Peer {
    anchor: b.origin_anchor,
    host: b.host,
    address: b.address,
    record_address: b.record_address,
    certificate: b_first.certificate(),
  };
  let peer_of_b = Peer {
    anchor: a.origin_anchor,
    host: a.host,
    address: a.address,
    record_address: a.record_address,
    certificate: a.identity.certificate(),
  };
  let b_seed = b.host;
  let daemon_a = start(a, peer_of_a);
  // Nothing serves B's manifest addresses: A's dial to B stays in its handshake. B's death arrives
  // (injected) and A retires it — with the pending dial.
  inject_death_into([&daemon_a], b_seed);
  let dropped = poll_until(&[&daemon_a], RETIREMENT_DEADLINE, || {
    refusal_count(&daemon_a, STALE_DIAL_DROPPED).map(|count| count >= 1)
  });
  // B returns at other addresses (the same certificate, a fresh member id) and dials A, announcing
  // its addresses over the record plane; A must mesh to it there.
  let b_again = Node {
    identity: b_again_identity,
    address: loopback(pb_probe_again),
    record_address: loopback(pb_record_again),
    ..b
  };
  let daemon_b = start(b_again, peer_of_b);
  let b_new = daemon_b
    .member_identity()
    .expect("the returned B has its fresh member id");
  let meshed = poll_until(&[&daemon_a, &daemon_b], REJOIN_DEADLINE, || {
    Ok(
      daemon_a.fleet_members()?.contains(&b_new)
        && daemon_a.fleet_meshed()?
        && daemon_b.fleet_meshed()?,
    )
  });
  let stale_dropped = refusal_count(&daemon_a, STALE_DIAL_DROPPED);
  daemon_a.stop();
  daemon_b.stop();
  assert!(
    dropped,
    "A dropped its pending dial to B when it retired B (counted {stale_dropped:?} × {STALE_DIAL_DROPPED})"
  );
  assert!(
    meshed,
    "A meshed to B's return at its new addresses under B's fresh member id"
  );
}

/// The refusal key A counts when it drops a dial still in its handshake at its peer's retirement
/// (`fleet::DIAL_STALE_DROPPED`), under the keys `Daemon::fleet_refusals` reports.
const STALE_DIAL_DROPPED: &str = "fleet.dial.stale_dropped";

/// Shape: how long [`Client::drain`] waits for one more stale reply before deciding the ring is empty —
/// a tenth of a second, several serve periods, so a reply the daemon is about to write is caught and an
/// empty ring costs little.
const DRAIN_NS: u64 = 100_000_000;

/// AC (§4.16 "Commit": "committed at f+1 acknowledgements … issued only when every identity the version
/// references is placed"; AUD-11): a submit's **acceptance** is answered only once its version's merge
/// record is committed at the quorum — never from the owner's local append alone. In a two-node `f = 1`
/// fleet the holder B first refuses every content put (the inputs cannot place), then, with the inputs
/// placing, withholds every merge record's acknowledgement; under neither can the submit resolve: the
/// client's bounded wait times out, the owner reports one acceptance waiting, the version unplaced, and
/// the holder counts what it withheld. Once both are lifted the record commits at the quorum, the wait
/// resolves (counted), the version is placed and recomputed on the holder, and a retry of the same
/// request meets the completion record the commit wrote — the same committed result, never a success
/// the quorum had not held.
#[test]
fn a_submit_is_answered_only_once_its_record_commits_at_the_quorum() {
  let _serial = serialize_fleet_tests();
  let (daemon_a, daemon_b, instance_a) = two_node_fleet();
  daemon_b
    .inject_merge_fault(slates_server::merge_service::MergeFault {
      refuse_content_puts: true,
      corrupt_next_inputs: false,
      refuse_records: false,
    })
    .expect("the inputs fault installs on B");
  let mut client = Client::connect(&instance_a);
  let (green, submit) = edited_work_to_submit(&mut client);
  let gate = observe_commit_gate(&daemon_a, &daemon_b, &mut client, green, &submit);
  daemon_a.stop();
  daemon_b.stop();
  assert_commit_gate(&gate);
}

/// A green, a work over it with one edit declared, and the submit request that would advance it.
fn edited_work_to_submit(client: &mut Client) -> (VolumeId, RequestBody) {
  let ReplyBody::GreenCreated { id: green } = client.call(&RequestBody::CreateGreen {
    name: "green".to_owned(),
    require_evidence: false,
    base: None,
  }) else {
    panic!("create green");
  };
  let ReplyBody::WorkCreated { id: work, .. } = client.call(&RequestBody::CreateWork {
    green,
    name: "work".to_owned(),
  }) else {
    panic!("create work");
  };
  assert!(matches!(
    client.call(&RequestBody::Edit {
      work,
      path: "f".to_owned(),
      at: 0,
      delete_len: 0,
      bytes: b"hello".to_vec(),
    }),
    ReplyBody::Edited
  ));
  (
    green,
    RequestBody::Submit {
      work,
      evidence: Vec::new(),
    },
  )
}

/// What the commit gate showed: with the inputs withheld, with the acknowledgements withheld, and once
/// both were lifted.
struct CommitGate {
  withheld_inputs: Result<ReplyBody, IpcError>,
  waiting_on_inputs: Result<usize, ObserveError>,
  unplaced_on_inputs: Result<bool, ObserveError>,
  held_on_records: bool,
  withheld_records: Result<u64, ObserveError>,
  placed: bool,
  resolved: bool,
  holder: Result<slates_server::merge_service::HolderMergeState, ObserveError>,
  retried: ReplyBody,
}

/// Runs the three phases: the submit's bounded wait under withheld inputs (B refusing content puts), a
/// hold under withheld acknowledgements (B refusing records), then both lifted — the commit, the
/// resolution, the holder's recomputation and the retry of the same request.
fn observe_commit_gate(
  daemon_a: &Daemon,
  daemon_b: &Daemon,
  client: &mut Client,
  green: VolumeId,
  submit: &RequestBody,
) -> CommitGate {
  let both = [daemon_a, daemon_b];
  // Inputs withheld: the submit does not resolve within the hold window; one acceptance waits.
  let withheld_inputs = client.try_call(
    submit,
    u64::try_from(RECORD_HOLD_WINDOW.as_nanos()).unwrap_or(u64::MAX),
  );
  let waiting_on_inputs = daemon_a.merge_awaiting(green);
  let unplaced_on_inputs = daemon_a.merge_record_placed(green, 1);

  // Acknowledgements withheld instead: the inputs place, the record still cannot commit.
  daemon_b
    .inject_merge_fault(slates_server::merge_service::MergeFault {
      refuse_content_puts: false,
      corrupt_next_inputs: false,
      refuse_records: true,
    })
    .expect("the record fault installs on B");
  let held_on_records = holds_for(RECORD_HOLD_WINDOW, || {
    Ok(!daemon_a.merge_record_placed(green, 1)? && daemon_a.merge_awaiting(green)? == 1)
  });
  let withheld_records = refusal_count(daemon_b, "merge.record_withheld");

  // Both lifted: the record commits at the quorum and the waiting acceptance resolves.
  daemon_b
    .inject_merge_fault(slates_server::merge_service::MergeFault::default())
    .expect("the faults lift on B");
  let placed = poll_until(&both, PLACEMENT_DEADLINE, || {
    daemon_a.merge_record_placed(green, 1)
  });
  let resolved = poll_until(&both, PLACEMENT_DEADLINE, || {
    Ok(
      daemon_a.merge_awaiting(green)? == 0
        && refusal_count(daemon_a, "merge.acceptance_resolved")? >= 1,
    )
  });
  let holder = daemon_b.merge_holder_state(green);
  // The retry of the same request meets the completion record the commit wrote.
  client.drain();
  let retried = client.call_retry(submit);
  CommitGate {
    withheld_inputs,
    waiting_on_inputs,
    unplaced_on_inputs,
    held_on_records,
    withheld_records,
    placed,
    resolved,
    holder,
    retried,
  }
}

/// The commit-gate assertions over one run: what held while a fault was in place, then what
/// followed the lift.
fn assert_commit_gate(gate: &CommitGate) {
  assert_gate_withheld(gate);
  assert_gate_committed(gate);
}

/// While the holder withheld the inputs, then the acknowledgements: no acceptance resolved, the
/// version stayed unplaced, the owner reported the wait and the holder counted what it withheld.
fn assert_gate_withheld(gate: &CommitGate) {
  assert!(
    gate.withheld_inputs.is_err(),
    "with the inputs withheld the submit does not resolve within the hold: {:?}",
    gate.withheld_inputs
  );
  assert_eq!(
    (&gate.waiting_on_inputs, &gate.unplaced_on_inputs),
    (&Ok(1), &Ok(false)),
    "one acceptance waits on the owner and the version is not placed"
  );
  assert!(
    gate.held_on_records,
    "with the acknowledgements withheld the version stays unplaced and the acceptance keeps waiting"
  );
  assert!(
    matches!(gate.withheld_records, Ok(count) if count > 0),
    "the holder counted the acknowledgements it withheld: {:?}",
    gate.withheld_records
  );
}

/// After both faults lifted: the record committed at the quorum, the wait resolved, the holder
/// recomputed the version, and the retry met the committed result.
fn assert_gate_committed(gate: &CommitGate) {
  assert!(
    gate.placed && gate.resolved,
    "once both are lifted the record commits at the quorum (placed={}) and the waiting acceptance \
     resolves, counted (resolved={})",
    gate.placed,
    gate.resolved
  );
  assert_eq!(
    gate.holder,
    Ok(slates_server::merge_service::HolderMergeState {
      version: Some(1),
      refused: false,
    }),
    "the holder recomputed version 1"
  );
  assert!(
    matches!(
      gate.retried,
      ReplyBody::Submitted {
        version: Some(1),
        ..
      }
    ),
    "the retry meets the committed result: {:?}",
    gate.retried
  );
}

/// AC (§4.16 owner-loss recovery, "adopts the newest records, and serves"; AUD-14): in a three-node
/// `f = 1` fleet a green advances three versions on its owner, each committed at the quorum and
/// recomputed on both holders; the owner dies; the survivor rendezvous ranks first takes the green over
/// and **materializes a servable green** from its own accepted merge records and held inputs — the
/// chain's increment identities on the successor equal the owner's (the full ledger prefix and the
/// original results preserved), and through the public client on the successor every version reads
/// back (version 1 and the head), a new work submits the next version — committed at the quorum with
/// the remaining holder and recomputed there — and a retry of that submit meets its completion record.
/// Non-vacuous: before the death the successor holds the green only as a replica (no owned engine, no
/// placed version reported), and the seeded configuration would never reassign ownership.
#[test]
fn a_taken_over_green_serves_every_version_and_accepts_new_work_on_the_successor() {
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
  let hosts: Vec<HostId> = daemons
    .iter()
    .map(|daemon| daemon.member_identity().unwrap())
    .collect();
  assert_fleet_forms(&daemons, &hosts, &names);

  // Three versions on the owner A, each committed at the quorum (the reply waits for it, AUD-11).
  let mut client = Client::connect(daemons[0].instance());
  let (green, work) = green_with_work(&mut client, "taken-over-green");
  commit_three_versions(&mut client, work);
  let owner_chain = daemons[0].merge_chain_identities(green).unwrap();
  let object = ObjectId(green.bytes);
  let (both_caught_up, successor_unowned_before) = holders_before_owner_death(&daemons, green);

  // A dies; the survivor rendezvous ranks first for the green takes it over.
  daemons.remove(0).stop();
  let successor_host =
    rendezvous_first(&[hosts[1], hosts[2]], object).expect("a survivor takes over the green");
  let successor_index = usize::from(successor_host != hosts[1]);
  let other_index = 1 - successor_index;
  let survivors: Vec<&Daemon> = daemons.iter().collect();
  let materialized = poll_until(&survivors, PLACEMENT_DEADLINE, || {
    daemons[successor_index].merge_record_placed(green, 3)
  });
  let successor_chain = daemons[successor_index].merge_chain_identities(green);
  let served = observe_taken_over_green(&daemons[successor_index], green);
  // The next version, submitted on the successor, commits with the remaining holder and is recomputed
  // there; its retry meets the completion record.
  let next = submit_next_on_successor(&daemons, successor_index, other_index, green);

  for daemon in daemons {
    daemon.stop();
  }
  assert!(
    both_caught_up,
    "both holders recomputed version 3 before the owner died"
  );
  assert!(
    successor_unowned_before,
    "before the death neither holder owned the green (no placed version reported)"
  );
  assert!(
    materialized,
    "the successor materialized the taken-over green at version 3"
  );
  assert_eq!(
    successor_chain,
    Ok(owner_chain.clone()),
    "the successor's chain carries the owner's increment identities, in order"
  );
  assert_eq!(owner_chain.len(), 3, "three versions were committed");
  assert_eq!(
    served,
    TakenOverGreenView {
      head: Ok(3),
      at_one: Ok(b"hello".to_vec()),
      at_head: Ok(b"hello world!".to_vec()),
    },
    "the successor serves the head version, version 1 and the head's bytes"
  );
  assert!(
    next,
    "the successor accepted the next version at the quorum and its retry met the record"
  );
}

/// AC (§4.8 "Leases and reads"; AUD-08): in a three-node `f = 1` fleet a green advances three versions on
/// its owner, then the owner is **isolated** on the probe plane (both directions) — it is not stopped, its
/// client connection stays open. Its owner lease lapses by the host clock (measured across a scheduling
/// pause of its control shard, so the lapse is by the clock, not by a loop running), and every read of the
/// green's **latest state** on that connection — the head version, a head read, a status — refuses
/// `LeaseUnconfirmed`, while an explicitly pinned immutable read (version 1) is still served. Meanwhile the
/// surviving quorum retires the isolated owner, the successor rendezvous ranks first materializes the green
/// and a new work advances it to version 4 — the new committed write the isolated owner must not serve
/// stale. Non-vacuous: before the isolation the owner served all three of those reads; the successor's
/// lease holds (a holder confirms it) so it serves version 4.
#[test]
fn an_isolated_owner_refuses_latest_state_reads_while_the_successor_advances_the_green() {
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
  let hosts: Vec<HostId> = daemons
    .iter()
    .map(|daemon| daemon.member_identity().unwrap())
    .collect();
  assert_fleet_forms(&daemons, &hosts, &names);

  // Owner A (index 0) commits three versions; both holders catch up.
  let mut client = Client::connect(daemons[0].instance());
  let (green, work) = green_with_work(&mut client, "leased-green");
  commit_three_versions(&mut client, work);
  let object = ObjectId(green.bytes);
  let (both_caught_up, _) = holders_before_owner_death(&daemons, green);

  // Before isolation the owner serves its green's latest state on this very connection.
  let before = observe_taken_over_green(&daemons[0], green);

  // Isolate A on the probe plane, both directions: A no longer answers B/C (so the council retires it),
  // and B/C no longer answer A (so A gathers no lease confirmation and its authority becomes uncertain).
  isolate_owner_on_the_probe_plane(&daemons, &hosts);

  // A scheduling pause across expiry: A's control shard runs nothing for two lease bounds. Its lease is
  // measured against the suspend-inclusive host clock, so it lapses *during* the pause — by the clock, not
  // because a loop iterated. Waiting the pause out here (`.answer()` blocks the test thread) is the "pause
  // across expiry" the audit asks for; B and C keep probing A throughout, so the council retires it meanwhile.
  let paused_ns = daemons[0]
    .starve_control_shard(2 * slates_server::lease::lease_bound_ns())
    .and_then(|done| done.answer());

  // A has resumed; its lease has lapsed (no holder confirmed it during or before the pause).
  let a_lease_lapsed = poll_until(&[&daemons[0]], RETIREMENT_DEADLINE, || {
    daemons[0].fleet_lease_holds(object).map(|holds| !holds)
  });

  // The surviving quorum retires A; the successor rendezvous ranks first takes the green over.
  let successor_host =
    rendezvous_first(&[hosts[1], hosts[2]], object).expect("a survivor takes over the green");
  let successor_index = if successor_host == hosts[1] { 1 } else { 2 };
  let other_index = if successor_index == 1 { 2 } else { 1 };
  let survivors = [&daemons[successor_index], &daemons[other_index]];
  let materialized = poll_until(&survivors, PLACEMENT_DEADLINE, || {
    daemons[successor_index].merge_record_placed(green, 3)
  });
  // A new work on the successor commits version 4 — the new committed write A must not serve stale.
  let advanced = submit_next_on_successor(&daemons, successor_index, other_index, green);
  // The isolated owner's latest-state reads now refuse `LeaseUnconfirmed`; its pinned version-1 read serves.
  let after = observe_taken_over_green(&daemons[0], green);
  // The successor serves the advanced green.
  let successor_view = observe_taken_over_green(&daemons[successor_index], green);
  for daemon in daemons {
    daemon.stop();
  }
  assert!(
    both_caught_up,
    "both holders recomputed version 3 before the isolation"
  );
  assert!(
    paused_ns
      .as_ref()
      .is_ok_and(|&held| held >= slates_server::lease::lease_bound_ns()),
    "A's control shard was paused past a lease bound ({paused_ns:?})"
  );
  assert!(
    a_lease_lapsed,
    "the isolated owner's lease lapsed (no holder confirms it any longer)"
  );
  assert!(
    materialized && advanced,
    "the successor took over the green and advanced it to version 4 (materialized={materialized} advanced={advanced})"
  );
  assert_isolated_owner_lease(&before, &after, &successor_view);
}

/// Isolates the owner (daemon 0) from its two peers on the probe plane, both directions, so the council
/// retires it and it gathers no lease confirmation (§4.8 "Leases and reads"; AUD-08).
fn isolate_owner_on_the_probe_plane(daemons: &[Daemon], hosts: &[HostId]) {
  daemons[0]
    .inject_probe_deafness(&[hosts[1], hosts[2]])
    .expect("A is isolated from B and C");
  daemons[1]
    .inject_probe_deafness(&[hosts[0]])
    .expect("B stops answering A");
  daemons[2]
    .inject_probe_deafness(&[hosts[0]])
    .expect("C stops answering A");
}

/// The owner-lease contract on the isolated owner (§4.8 "Leases and reads"; AUD-08): before isolation it
/// served the green's latest state; after, its head version and head read refuse `LeaseUnconfirmed` while
/// its pinned version-1 read still serves; the successor serves the advanced head.
fn assert_isolated_owner_lease(
  before: &TakenOverGreenView,
  after: &TakenOverGreenView,
  successor_view: &TakenOverGreenView,
) {
  assert_eq!(
    before,
    &TakenOverGreenView {
      head: Ok(3),
      at_one: Ok(b"hello".to_vec()),
      at_head: Ok(b"hello world!".to_vec()),
    },
    "before isolation the owner served the green's latest state on this connection"
  );
  assert!(
    refuses_lease(&after.head) && refuses_lease(&after.at_head),
    "the isolated owner refuses the head version and a head read with LeaseUnconfirmed: {after:?}"
  );
  assert_eq!(
    after.at_one,
    Ok(b"hello".to_vec()),
    "the isolated owner still serves the pinned immutable version-1 read (its separate contract)"
  );
  assert_eq!(
    successor_view.head,
    Ok(4),
    "the successor serves the advanced green at version 4"
  );
}

/// Whether an observed reply is the `LeaseUnconfirmed` refusal (§4.8 "Leases and reads"; AUD-08).
fn refuses_lease<T>(reply: &Result<T, Box<ReplyBody>>) -> bool {
  matches!(
    reply,
    Err(boxed) if matches!(
      boxed.as_ref(),
      ReplyBody::Refused {
        refusal: Refusal::LeaseUnconfirmed { .. }
      }
    )
  )
}

/// A green and a work over it, created through `client` on the owner.
fn green_with_work(client: &mut Client, name: &str) -> (VolumeId, VolumeId) {
  let ReplyBody::GreenCreated { id: green } = client.call(&RequestBody::CreateGreen {
    name: name.to_owned(),
    require_evidence: false,
    base: None,
  }) else {
    panic!("create green");
  };
  let ReplyBody::WorkCreated { id: work, .. } = client.call(&RequestBody::CreateWork {
    green,
    name: format!("{name}-work"),
  }) else {
    panic!("create work");
  };
  (green, work)
}

/// Submits three versions from `work` on its owner: each acceptance resolves within the placement
/// deadline once the version commits at the quorum (AUD-11) and answers the expected version.
fn commit_three_versions(client: &mut Client, work: VolumeId) {
  for (version, (at, bytes)) in [(0u64, "hello"), (5, " world"), (11, "!")]
    .into_iter()
    .enumerate()
  {
    edit_work(client, work, at, bytes);
    let started = Instant::now();
    let submitted = client
      .try_call(
        &RequestBody::Submit {
          work,
          evidence: Vec::new(),
        },
        u64::try_from(PLACEMENT_DEADLINE.as_nanos()).unwrap_or(u64::MAX),
      )
      .expect("the acceptance resolves once the version commits at the quorum");
    let expected = u64::try_from(version).unwrap() + 1;
    assert!(
      started.elapsed() < PLACEMENT_DEADLINE,
      "version {expected}'s acceptance resolved within the placement deadline"
    );
    assert!(
      matches!(submitted, ReplyBody::Submitted { version: Some(v), .. } if v == expected),
      "version {expected}: {submitted:?}"
    );
  }
}

/// Before the owner dies: whether both holders recomputed version 3 (the successor's own prefix is
/// what it rebuilds from; the general prefix transfer is owed separately), and whether neither holder
/// reports a placed version — the green is held there, not owned.
fn holders_before_owner_death(daemons: &[Daemon], green: VolumeId) -> (bool, bool) {
  let both_caught_up = poll_until(
    &daemons.iter().collect::<Vec<_>>(),
    PLACEMENT_DEADLINE,
    || {
      all_hold(daemons[1..].iter().map(|holder| {
        holder
          .merge_holder_state(green)
          .map(|state| state.version == Some(3))
      }))
    },
  );
  let unowned = daemons[1..]
    .iter()
    .all(|holder| holder.merge_record_placed(green, 1) == Ok(false));
  (both_caught_up, unowned)
}

/// Declares an insertion of `bytes` at `at` on `work`'s file `f`.
fn edit_work(client: &mut Client, work: VolumeId, at: u64, bytes: &str) {
  let edited = client.call(&RequestBody::Edit {
    work,
    path: "f".to_owned(),
    at,
    delete_len: 0,
    bytes: bytes.as_bytes().to_vec(),
  });
  assert!(matches!(edited, ReplyBody::Edited), "{edited:?}");
}

/// What a successor serves of a taken-over green through the public client; a refusal or a missed
/// deadline is kept as the reply it produced, so the assertion names it.
#[derive(Debug, PartialEq)]
struct TakenOverGreenView {
  /// The head version `Versions` answers.
  head: Result<u64, Box<ReplyBody>>,
  /// The bytes of `f` at version 1.
  at_one: Result<Vec<u8>, Box<ReplyBody>>,
  /// The bytes of `f` at the head.
  at_head: Result<Vec<u8>, Box<ReplyBody>>,
}

/// Observes the taken-over green on `successor`: the head version, the bytes of `f` at version 1,
/// and at the head.
fn observe_taken_over_green(successor: &Daemon, green: VolumeId) -> TakenOverGreenView {
  let mut client = Client::connect(successor.instance());
  let head = match client
    .try_call(&RequestBody::Versions { green }, DEADLINE_NS)
    .unwrap_or_else(|e| refused_placeholder(&e))
  {
    ReplyBody::Versions { head } => Ok(head),
    other => Err(Box::new(other)),
  };
  let at_one = read_green_file(
    &mut client,
    green,
    slates_ipc::protocol::ReadAt::Version { version: 1 },
  );
  let at_head = read_green_file(&mut client, green, slates_ipc::protocol::ReadAt::Head);
  TakenOverGreenView {
    head,
    at_one,
    at_head,
  }
}

/// Reads `f` of `green` at `at` through `client`.
fn read_green_file(
  client: &mut Client,
  green: VolumeId,
  at: slates_ipc::protocol::ReadAt,
) -> Result<Vec<u8>, Box<ReplyBody>> {
  match client
    .try_call(
      &RequestBody::Read {
        volume: green,
        path: "f".to_owned(),
        at,
      },
      DEADLINE_NS,
    )
    .unwrap_or_else(|e| refused_placeholder(&e))
  {
    ReplyBody::ReadBytes { bytes } => Ok(bytes),
    other => Err(Box::new(other)),
  }
}

/// Submits version 4 from a new work on the successor: accepted at the quorum (with the remaining
/// holder, which recomputes it), placed, and the retry of the same request answered from the record.
fn submit_next_on_successor(
  survivors: &[Daemon],
  successor_index: usize,
  other_index: usize,
  green: VolumeId,
) -> bool {
  let mut client = Client::connect(survivors[successor_index].instance());
  let created = client
    .try_call(
      &RequestBody::CreateWork {
        green,
        name: "successor-work".to_owned(),
      },
      DEADLINE_NS,
    )
    .unwrap_or_else(|e| refused_placeholder(&e));
  let ReplyBody::WorkCreated { id: work, base } = created else {
    eprintln!("create work on the successor: {created:?}");
    return false;
  };
  if base != 3 {
    return false;
  }
  edit_work(&mut client, work, 12, "?");
  let submit = RequestBody::Submit {
    work,
    evidence: Vec::new(),
  };
  let submitted = client
    .try_call(
      &submit,
      u64::try_from(PLACEMENT_DEADLINE.as_nanos()).unwrap_or(u64::MAX),
    )
    .unwrap_or_else(|e| refused_placeholder(&e));
  let accepted = matches!(
    submitted,
    ReplyBody::Submitted {
      version: Some(4),
      ..
    }
  );
  if !accepted {
    eprintln!(
      "successor submit: {submitted:?}; awaiting: {:?}; placed(4): {:?}; successor refusals: {:?}; other holder: {:?}",
      survivors[successor_index].merge_awaiting(green),
      survivors[successor_index].merge_record_placed(green, 4),
      survivors[successor_index].fleet_refusals(),
      survivors[other_index].merge_holder_state(green),
    );
  }
  let retried = client
    .try_retry(&submit, DEADLINE_NS)
    .unwrap_or_else(|e| refused_placeholder(&e));
  let same = retried == submitted;
  let observed: Vec<&Daemon> = survivors.iter().collect();
  let placed = poll_until(&observed, PLACEMENT_DEADLINE, || {
    survivors[successor_index].merge_record_placed(green, 4)
  });
  let recomputed = poll_until(&observed, PLACEMENT_DEADLINE, || {
    survivors[other_index]
      .merge_holder_state(green)
      .map(|state| state.version == Some(4))
  });
  if !(same && placed && recomputed) {
    eprintln!(
      "successor next version: accepted={accepted} same={same} placed={placed} recomputed={recomputed}; \
       retried: {retried:?}; other holder: {:?}",
      survivors[other_index].merge_holder_state(green)
    );
  }
  accepted && same && placed && recomputed
}

/// The reply shape a bounded call that got no reply stands in for, so the observation records the
/// refusal (the deadline, a ring fault) rather than panicking mid-history.
fn refused_placeholder(error: &IpcError) -> ReplyBody {
  ReplyBody::Refused {
    refusal: Refusal::BadRequest {
      reason: error.to_string(),
    },
  }
}
