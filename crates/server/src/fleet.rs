//! The fleet membership loop the control shard runs (§4.8 "Membership"; §2.6 boot step 6). A fleet node
//! detects its peers' failures on the sealed membership plane, replicates its volume heads to them over record
//! sessions, and folds the detector's view into its [`FleetNode`](slates_cluster::fleet::FleetNode). A dead peer
//! is retired and the objects that rendezvous now ranks first to this node are taken over.
//!
//! **One serve socket per plane, every peer on it.**
//! - The **record** socket is a QUIC demultiplexer (`slates_transport::demux::Demux`). A peer dials it from an
//!   address chosen at dial time, and the demultiplexer routes each datagram to that peer's session by the
//!   connection id in its header (a raw handshake datagram by its source, opening a session for a new dialer). It
//!   hands each new session to the accept loop, and **replaces** a peer's old session when the peer re-dials
//!   after losing it, so a mid-run session loss recovers without anyone being told. This node *dials* each peer's
//!   record socket from a socket of its own, so every session is cleanly one-directional.
//! - The **probe** socket carries the sealed membership plane (A-67 H-2, [`crate::member_task`]): hyper-swim's one
//!   detector, its probes sealed datagrams keyed from each pair's canonical record session.
//!
//! A two-node fleet is the single-peer degenerate and is proven live (`crates/server/tests/fleet.rs`). A fleet of
//! N forms its N·(N−1) record sessions over N serve sockets.
//!
//! **Tasks on the control shard.**
//! - The one **membership** task ([`crate::member_task::run`]) owns the detector, the plane and the probe socket. It
//!   announces each canonical record session's epoch, probes, answers, and folds the detector's view into every
//!   shard's `FleetNode` under the fleet's admission rule.
//! - the per-peer **record serve** tasks answer the peer's register record commits ([`serve_peer_records`]
//!   accepts each into this node's **durable per-object hold** in the shard state, so it backs the peer as a
//!   candidate holder and the record survives for a takeover to read) and its plane epoch announcements;
//! - the per-peer **record link** task ([`establish_record_link`]) keeps this node's client record session
//!   to the peer up in the shard state ([`ShardState::record_sessions`]), retrying its handshake on one
//!   socket each period and re-establishing a lost one — per peer, so one slow link never stalls the rest;
//! - the one **record-plane coordinator** ([`run_record_plane`]) borrows those sessions for each dispatch,
//!   so each period it ships each unplaced head to **all** its candidate holders in one commit (§4.8 "records
//!   are sent to all candidates; committed at `f + 1`") and **drives any owed takeover**, one batched round
//!   per retired host ([`crate::takeover`]), and recovers the sessions of holders still in flight past an
//!   early quorum ([`Stragglers`]).
//!
//! **Takeover** (§4.8 "Promotion and takeover"; [`crate::takeover`]): when the council retires a host, every
//! holder resolves each object it holds to the successor the committed configuration's lineage names
//! ([`crate::takeover::resolve_held_objects`]) and brings its acceptor's authority into step
//! ([`reconcile_held_authority`]); each survivor that owes a confirmation runs one phase-one round for the
//! retired host across its recovery neighbourhoods — holders answering in pages, a holder that holds nothing
//! answering too — adopts each object once its cohorts have promised, re-commits it under its own placement at
//! the round's epoch, and confirms its share to the council. The serve loop answers a page from the same
//! durable holds ([`crate::takeover::serve_host_prepare`]).
//!
//! **Content replication and serve** (§4.10 "Content replication"; §4.8 mechanism 1): each owned volume's
//! newest snapshot is archived in bounded slices ([`advance_home_seal`], the vfs `SnapshotArchiver`), the shard's
//! volumes visited in turns of about a step quantum each ([`seal_walk_slice`]), its
//! archive put to the content candidates by missing set until `f + 1` hold it verified
//! ([`put_seal_content`], the first round to `f + 1`, later rounds hedged to the rest), and only then the
//! head naming the manifest and the acknowledging holders shipped ([`head_value_of`]) and the snapshot
//! recorded placed durably ([`record_placed_seals`]). A holder serves the content exchanges from
//! [`ShardState::held_content`] on the record session's content streams. After a takeover the successor
//! serves the adopted head's content: it materializes the volume under its original id from the archive it
//! holds, or fetches it by identity from a recorded holder ([`materialize_pending`]) — on the shard the
//! taken-over id routes to, with the head's promotion epoch, so the successor's next seals of the object
//! are written at the epoch its holders fenced it at.
//!
//! **Every shard is an owner** (D-7): the control shard alone holds the peer sessions and probes, but a
//! volume lives, seals and records its placement on its owner shard. So the probe loop hands each peer
//! state it folds to every other shard's `FleetNode` (all copies of the configuration advance
//! identically), and the record plane reaches every owner shard each period through [`crate::xshard`]:
//! the seal walk and the head values run there, the archives and heads move here by value for the
//! dispatch, and the acknowledgements and durable placements are recorded back there. The hedge to the
//! remaining candidates fires on the **measured p95** of the content class's put latency ([`LatencyWindow`],
//! [`hedge_delay_ns`]), one period before any reading. **Anti-entropy and the healer** are one step per
//! healer period ([`heal_one_placed_snapshot`]): a placed snapshot is re-offered to its candidates through
//! the same rounds — a holder that lost content is put exactly what it lacks (a repair, counted), one that
//! lost nothing is put no bytes — at a cadence derived from the measured put-failure rate
//! ([`heal_period_ns`]). Owed: content-defined chunking and the compress-or-not cost model (D-17).
//!
//! The `FleetNode` lives in the shard state (the verbs read it for placement), so it is touched only
//! through brief synchronous [`state::with_state`] — never held across an await. At `f = 0` (the laptop)
//! there is no fleet transport and this loop does not run; the placement path still runs the same
//! `FleetNode`, degenerate (R8).

use rustls::pki_types::CertificateDer;
use slates_archive::Archive;
use slates_cluster::config_group::ReportOutcome;
use slates_cluster::content::{
  CONTENT_CHUNK_STREAM, CONTENT_OFFER_STREAM, ContentMessage, fetch, is_content_stream, put_content,
};
use slates_cluster::fleet::apply_peer_state;
use slates_cluster::membership::{Liveness, MemberState};
use slates_cluster::raft_wire::RaftMessage;
use slates_cluster::root_group::root_representatives;
use slates_cluster::timing::{
  ElectionTimer, ElectionTiming, FollowerStep, PathRtt, RoundAnchors, quorum_priority, round_budget,
};
use slates_cluster::{
  ClusterError, CommitBudget, RECORD_STREAM, Stragglers, TimedReply, broadcast, commit_record,
  request_within,
};
use slates_db::Op;
use slates_db::catalog::{
  PlacementState, SnapshotId as DbSnapshotId, Tombstone, VolumeId as DbVolumeId, VolumeRecord,
  VolumeState,
};
use slates_db::register::{
  Acceptor, Authority, DomainId, HostEpoch, HostId, ObjectId, Placement, Quorum, Record, RegionId,
  RegisterClass, RegisterError, encode_refusal,
};
use slates_rt::futures;
use slates_rt::shard::Kept;
use slates_rt::udp::UdpSocket;
use slates_rt::udp::{Ipv4Addr, SocketAddrV4};
use slates_transport::connection::{ConnectionShape, Priority};
use slates_transport::demux::{Demux, DemuxId};
use slates_transport::endpoint::{Endpoint, EndpointError, HandshakeStall};
use slates_transport::handshake::Identity;
use slates_vfs::clock::Clock;
use slates_vfs::export::{Progress, SnapshotArchiver};

use crate::daemon::HEARTBEAT_NS;
use crate::deploy::NodeAddress;
use crate::dns::{self, Resolver};
use crate::head::{HeadValue, PlacedHead, SealJob};
use crate::state::{self, LearnedMember, Making, ShardState};
use crate::verbs;
use crate::xshard::{call_within, run_on};

/// Derived: the fleet session's packet budget — the most encoded frame bytes a 1-RTT packet carries and
/// still fits within [`MIN_DATAGRAM_BYTES`](slates_transport::endpoint::MIN_DATAGRAM_BYTES) with its header and AEAD tag
/// ([`slates_transport::endpoint::MAX_PACKET_PAYLOAD`], 1171). Every frame is counted against it (the
/// acknowledgement, credit and control frames as well as stream data), so a packet never crosses the
/// floor. At this budget a whole fleet message — a SWIM probe, a register record, or its acknowledgement,
/// each at most a few hundred bytes — rides one frame inside one receive window, so a commit completes in
/// one round trip (at the earlier 16-byte cap a 68-byte record fragmented into five frames and needed ten
/// credit-gated round trips). Replaces the earlier `MIN_DATAGRAM_BYTES − 80` estimate, which counted only
/// stream data and let acknowledgement and credit frames push packets past the floor (measured: up to
/// 2,048 bytes, truncated at the receiver and read as losses, 2026-09-28).
pub const FLEET_FRAME_CAP: usize = slates_transport::endpoint::MAX_PACKET_PAYLOAD;

/// Derived: the base suspicion window in protocol periods, at full local health (§4.8 "SWIM with
/// Lifeguard"; SWIM §4.2 uses a small multiple of the period so a lost acknowledgement is retried by the
/// next period before a member is suspected). Two periods: one to miss, one to confirm the miss, before the
/// aging declares death; the Lifeguard multiplier dilates it when this node itself looks unhealthy.
pub(crate) const SUSPICION_PERIODS: u32 = 2;

/// Derived: the Lifeguard local-health multiplier cap minus one — a 3× cap (§4.8 "bounded local-health
/// multiplier"; the raw `(LHM+1)` reaches 9× at the paper's saturation, which pushes timers off a cliff, so
/// hyperscale softened it to a 3× cap). A small integer keeps the dilation determinism-clean.
pub(crate) const LOCAL_HEALTH_CAP: u32 = 2;

/// Derived: how many times the collection loop polls for a reply within one protocol period — ten, so the
/// loop wakes within a tenth of a period of the acknowledgement (10 ms at the default cadence) without
/// spinning. A finer value measured from the RTT is the owed refinement.
pub const POLL_PER_PERIOD: u64 = 10;

/// A fleet peer this node probes and is probed by (§4.8): its host id, the two addresses this node dials to
/// reach it (its probe and record sockets), and the operator-provisioned certificate the mutual-TLS session
/// pins (§4.8 "TLS 1.3 via rustls with certificates provisioned by the operator") — which is also how the
/// serve side tells which peer dialed it (`serve_peer_records`).
pub struct FleetPeer {
  /// The peer's **stable anchor** — what its certificate stands for across restarts: the certificate's hash
  /// in a deployment (`deploy::host_id_of_certificate`), the machine identity's on an in-process fleet. Every
  /// member id the peer ever holds derives from it (`deploy::member_id(anchor, boot_nonce)`), so an id it
  /// announces is validated against this (task #22).
  pub anchor: HostId,
  /// The peer's generation-0 **seed** member id, `member_id(anchor, 0)` — the id the manifest precomputes and
  /// this node uses to route discovery until authenticated contact announces the fresh member id.
  pub host: HostId,
  /// The peer's advertised probe address — where it accepts this node's SWIM probes, and where this node
  /// dials it: an IP, or a DNS name resolved at every fresh dial (`crate::dns`).
  pub address: NodeAddress,
  /// The peer's advertised record address — where it accepts this node's register record commits (a separate
  /// socket from the probe one: the SWIM and register wire formats are not distinguished by content on a
  /// shared stream).
  pub record_address: NodeAddress,
  /// The peer's operator-provisioned certificate, pinned for the mutual-TLS session.
  pub certificate: CertificateDer<'static>,
}

/// The fleet transport material the membership loop drives (§4.8, boot step 6). It is kept out of the
/// Clone-able [`DaemonConfig`](crate::config::DaemonConfig) because [`Identity`] is not `Clone` (it holds a
/// private key): the membership *policy* (quorum + peers) lives in the config and builds the `FleetNode`;
/// this *transport* material is handed to [`Daemon::start`](crate::Daemon) and moved to the control shard.
///
/// `Serve` is where the node serves: a deployment plan carries the pure [`ServeAddresses`] (planning binds
/// nothing), and the daemon takes the bound [`ServeSockets`] — [`FleetTransport::bind`] is the one step
/// between, taken before the daemon starts, so an address in use refuses the start by name rather than
/// ending the membership loop later. A caller that already holds its ports (a test fleet that learned
/// them by binding) hands the held sockets over with [`slates_rt::udp::UdpSocket::adopt`] and never gives
/// a port up between learning and serving on it
/// (`docs/bugs/2026-09-28-a-released-test-port-was-taken-before-the-daemon-bound-it.md`).
pub struct FleetTransport<Serve = ServeSockets> {
  /// This node's fleet TLS identity (operator-provisioned).
  pub identity: Identity,
  /// The probe-plane address this node publishes to authenticated peers: where they dial its probes, its advertised
  /// address and base port.
  pub advertise: NodeAddress,
  /// The record-plane address this node publishes to authenticated peers: where they dial its record links (the
  /// advertised base port's next in a manifest's layout). Never the port its record socket is bound to, which differs
  /// behind a port mapping (A-277): a peer told the bound port redials around the mapping, or to a port the mapping does
  /// not forward (A-330).
  pub advertise_record: NodeAddress,
  /// Operator trust anchors for certificates of nodes absent from the seed manifest.
  pub enrollment_roots: Vec<CertificateDer<'static>>,
  /// The TLS server name this node presents and its peers pin.
  pub name: String,
  /// This node's two serve points: the probe plane (every peer's SWIM probes arrive there) and the record
  /// plane (every peer's record commits, prepares and content exchanges).
  pub serve: Serve,
  /// The peers this node probes and is probed by, each with its dial addresses.
  pub peers: Vec<FleetPeer>,
  /// The host's resolver configuration, when any peer is addressed by a DNS name (the deployment plan
  /// refuses a named peer without one); `None` for a fleet of literal addresses.
  pub resolver: Option<Resolver>,
}

/// Where a node's serve sockets bind — the deployment plan's pure form (planning binds nothing).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServeAddresses {
  /// The probe plane's address.
  pub probe: SocketAddrV4,
  /// The record plane's address.
  pub record: SocketAddrV4,
  /// The network export's TCP address (§4.6 "Kubernetes publication without privilege", AUD-29-75): the
  /// probe plane's address and port, over TCP rather than UDP, so the port block stays two numbers wide.
  /// Present only when the fleet has an operator authority to verify RPC-with-TLS clients against; without
  /// one nothing could be authenticated, so nothing is served.
  pub export: Option<SocketAddrV4>,
}

/// The network export's listener: the runtime's TCP listener, on every platform (A-184; until 2026-10-26 it was
/// uninhabited on Windows, which served no NFS).
pub type ExportListener = slates_rt::tcp::TcpListener;

/// A node's bound serve sockets, one per plane, and its export listener — what the membership loop serves on.
#[derive(Debug)]
pub struct ServeSockets {
  /// The probe plane's socket.
  pub probe: UdpSocket,
  /// The record plane's socket.
  pub record: UdpSocket,
  /// The network export's listener, when the plan has one.
  pub export: Option<ExportListener>,
}

/// A serve socket the daemon could not take up: the plane, the address its plan names, and why — the
/// daemon does not start (an operator reads which address is in use, or what the supervisor handed over).
#[derive(Debug)]
pub struct ServeBindError {
  /// Which plane: `probe` or `record`.
  pub plane: &'static str,
  /// The address the node's plan serves this plane on.
  pub address: SocketAddrV4,
  /// Why the socket could not be taken up.
  pub fault: ServeFault,
}

/// Why a serve socket could not be taken up.
#[derive(Debug)]
pub enum ServeFault {
  /// The runtime refused the bind (the address in use, or refused), or the adoption of an inherited
  /// descriptor (not a datagram socket, not bound, not open).
  Refused(slates_rt::error::RtError),
  /// The supervisor handed over a socket bound somewhere other than the plan's address.
  BoundElsewhere {
    /// Where the inherited socket is bound.
    bound: SocketAddrV4,
  },
  /// The supervisor's descriptor list ([`slates_anchor::ENV_FLEET_SERVE`]) is not two descriptor numbers, and
  /// a third exactly when the plan has an export listener.
  Malformed,
}

impl std::fmt::Display for ServeBindError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match &self.fault {
      ServeFault::Refused(refusal) => write!(
        formatter,
        "the {} serve socket could not be taken up at {}: {refusal:?}",
        self.plane, self.address
      ),
      ServeFault::BoundElsewhere { bound } => write!(
        formatter,
        "the inherited {} serve socket is bound at {bound}, not at the planned {}",
        self.plane, self.address
      ),
      ServeFault::Malformed => write!(
        formatter,
        "the inherited serve sockets ({}) are not two descriptor numbers, and a third exactly when the plan exports",
        slates_anchor::ENV_FLEET_SERVE
      ),
    }
  }
}

impl std::error::Error for ServeBindError {}

impl ServeAddresses {
  /// Takes up both planes' serve sockets: the ones a supervisor holds and handed over
  /// ([`slates_anchor::ENV_FLEET_SERVE`], checked against these addresses), or, when none were handed
  /// over, bound here. Either way the sockets serve exactly the plan's addresses, or the start is refused
  /// naming the plane.
  pub fn bind(self) -> Result<ServeSockets, ServeBindError> {
    if let Some(inherited) = inherited_serve_sockets(self)? {
      return Ok(inherited);
    }
    let bind = |plane: &'static str, address: SocketAddrV4| {
      UdpSocket::bind(address).map_err(|refusal| ServeBindError {
        plane,
        address,
        fault: ServeFault::Refused(refusal),
      })
    };
    Ok(ServeSockets {
      probe: bind("probe", self.probe)?,
      record: bind("record", self.record)?,
      export: self.export.map(bind_export).transpose()?,
    })
  }
}

/// Binds the network export's listener at `address`.
fn bind_export(address: SocketAddrV4) -> Result<ExportListener, ServeBindError> {
  // The loopback listener's backlog: the same kind of client (a kernel NFS client, one connection per server
  // it mounts, reconnected when lost), so the same bound on connections queued before the accept loop runs.
  slates_rt::tcp::TcpListener::bind(address, crate::daemon::NFS_BACKLOG).map_err(|refusal| {
    ServeBindError {
      plane: "export",
      address,
      fault: ServeFault::Refused(refusal),
    }
  })
}

/// The serve sockets a supervisor handed over in [`slates_anchor::ENV_FLEET_SERVE`], adopted and checked
/// against `planned`; `None` when the environment names none (a daemon started alone binds its own).
#[cfg(unix)]
fn inherited_serve_sockets(
  planned: ServeAddresses,
) -> Result<Option<ServeSockets>, ServeBindError> {
  let Ok(value) = std::env::var(slates_anchor::ENV_FLEET_SERVE) else {
    return Ok(None);
  };
  let malformed = ServeBindError {
    plane: "probe",
    address: planned.probe,
    fault: ServeFault::Malformed,
  };
  let mut numbers = value
    .split(',')
    .map(|number| number.trim().parse::<std::os::fd::RawFd>());
  let (Some(Ok(probe)), Some(Ok(record))) = (numbers.next(), numbers.next()) else {
    return Err(malformed);
  };
  let export = match numbers.next() {
    None => None,
    Some(Ok(export)) => Some(export),
    Some(Err(_)) => return Err(malformed),
  };
  let distinct = export.is_none_or(|export| export >= 0 && export != probe && export != record);
  if numbers.next().is_some()
    || probe < 0
    || record < 0
    || probe == record
    || !distinct
    || export.is_some() != planned.export.is_some()
  {
    return Err(malformed);
  }
  Ok(Some(ServeSockets {
    probe: adopt_inherited("probe", probe, planned.probe)?,
    record: adopt_inherited("record", record, planned.record)?,
    export: match (export, planned.export) {
      (Some(raw), Some(address)) => Some(adopt_export(raw, address)?),
      _ => None,
    },
  }))
}

/// A daemon that is not Unix is never handed serve sockets (its supervisor does not spawn it with any):
/// it binds its own.
#[cfg(not(unix))]
fn inherited_serve_sockets(
  _planned: ServeAddresses,
) -> Result<Option<ServeSockets>, ServeBindError> {
  Ok(None)
}

/// Adopts the inherited descriptor `raw` as `plane`'s serve socket and checks it serves `planned`.
#[cfg(unix)]
fn adopt_inherited(
  plane: &'static str,
  raw: std::os::fd::RawFd,
  planned: SocketAddrV4,
) -> Result<UdpSocket, ServeBindError> {
  let refused = |refusal| ServeBindError {
    plane,
    address: planned,
    fault: ServeFault::Refused(refusal),
  };
  let socket = UdpSocket::adopt(inherited_descriptor(raw)).map_err(refused)?;
  let bound = socket.local_addr().map_err(refused)?;
  if bound != planned {
    return Err(ServeBindError {
      plane,
      address: planned,
      fault: ServeFault::BoundElsewhere { bound },
    });
  }
  Ok(socket)
}

/// Takes ownership of a descriptor the supervisor handed over at `raw`: a fleet serve socket
/// (`ENV_FLEET_SERVE`), or the handoff table's object and the descriptors it names, the device channel, the held
/// devices, connections and copies (`slates_anchor::held::ENV_HELD`, A-462; `crate::fuse_hold`).
#[cfg(unix)]
pub(crate) fn inherited_descriptor(raw: std::os::fd::RawFd) -> std::os::fd::OwnedFd {
  use std::os::fd::FromRawFd;
  // SAFETY: the supervisor made this descriptor inheritable and handed it across the spawn at this number
  // (`ENV_FLEET_SERVE`, or `ENV_HELD` and the table it names; every number non-negative and distinct from the others,
  // checked by the parsers); this process adopts each once, at start, before anything else could claim the number, so
  // the `OwnedFd` is its single owner and closes it on drop. The supervisor keeps its own copy.
  unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) }
}

/// Adopts the inherited descriptor `raw` as the network export's listener and checks it listens at `planned`.
#[cfg(unix)]
fn adopt_export(
  raw: std::os::fd::RawFd,
  planned: SocketAddrV4,
) -> Result<ExportListener, ServeBindError> {
  let refused = |refusal| ServeBindError {
    plane: "export",
    address: planned,
    fault: ServeFault::Refused(refusal),
  };
  let listener =
    slates_rt::tcp::TcpListener::from_owned(inherited_descriptor(raw)).map_err(refused)?;
  let bound = listener.local_addr().map_err(refused)?;
  if bound != planned {
    return Err(ServeBindError {
      plane: "export",
      address: planned,
      fault: ServeFault::BoundElsewhere { bound },
    });
  }
  Ok(listener)
}

impl FleetTransport<ServeAddresses> {
  /// The planned transport with its serve sockets taken up ([`ServeAddresses::bind`]: inherited from a
  /// supervisor that holds them, or bound) — the one step between a plan and a daemon.
  pub fn bind(self) -> Result<FleetTransport, ServeBindError> {
    let FleetTransport {
      identity,
      advertise,
      advertise_record,
      enrollment_roots,
      name,
      serve,
      peers,
      resolver,
    } = self;
    Ok(FleetTransport {
      identity,
      advertise,
      advertise_record,
      enrollment_roots,
      name,
      serve: serve.bind()?,
      peers,
      resolver,
    })
  }
}

/// What this node dials to reach one peer on one plane: the peer's stable anchor and its seed member id (the
/// task follows the peer's *current* id from there, task #22), the address on that plane, the certificate
/// to pin, and the resolver for a named address; `name` is the fleet's TLS name the session is verified
/// under.
struct PeerDial {
  anchor: HostId,
  host: HostId,
  name: String,
  address: NodeAddress,
  certificate: CertificateDer<'static>,
  resolver: Option<Kept<Resolver>>,
}

/// A fleet peer as the serve side knows it: the certificate the handshake must present (mutual TLS admits
/// only these), the **stable anchor** that certificate stands for, and the generation-0 **seed** id the
/// manifest precomputed for it. The peer's *current* member id is not here: it is learned on contact and kept
/// in [`ShardState::learned_members`] by anchor (task #22).
#[derive(Clone)]
struct Rostered {
  certificate: CertificateDer<'static>,
  anchor: HostId,
}

/// Refused: a peer announced a member id that is not `member_id(anchor, boot_nonce)` for the certificate it
/// presented — an id it could not have derived (a forgery, or a corrupted announcement). Counted in the
/// status report's refusals, never folded.
const MEMBER_ID_FORGED: &str = "fleet.member_id_forged";
/// What learning an announced identity on contact concluded ([`classify_announced`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LearnedOutcome {
  /// The id already known for this anchor at this boot_nonce — the ordinary contact.
  Current,
  /// A different boot nonce: the peer **restarted** and is a new member; `old` is the id it held
  /// before (its seed, or its previous incarnation's id), to be retired and taken over.
  Restarted { old: HostId },
  /// The announced id is not the one its anchor and boot_nonce derive to, refused.
  Forged,
}

/// Validate the claimed id against the authenticated anchor and its random boot nonce (§4.8,
/// AUD-07). Equal nonces denote current contact; different nonces denote different members.
/// Numeric comparison cannot establish freshness after whole-anchor loss. Raft admission and
/// the immutable group id determine voting authority independently of this discovery view.
fn classify_announced(
  known: Option<&LearnedMember>,
  anchor: HostId,
  boot_nonce: u64,
  announced: HostId,
) -> LearnedOutcome {
  if announced != crate::deploy::member_id(anchor, boot_nonce) {
    return LearnedOutcome::Forged;
  }
  match known {
    Some(known) if boot_nonce == known.boot_nonce => LearnedOutcome::Current,
    Some(known) => LearnedOutcome::Restarted { old: known.host },
    // An anchor never seen: nothing precedes this announcement, so nothing is retired — it is current from
    // here. Only a rostered certificate reaches this (the handshake admits no other), so this is a seed the
    // boot seeding has not yet written, never an unknown node.
    None => LearnedOutcome::Restarted {
      old: crate::deploy::member_id(anchor, 0),
    },
  }
}

/// Learns a peer's announced identity on contact and folds what it implies (task #22): classifies the
/// announcement ([`classify_announced`]); records a current or restarted id under the peer's anchor; and, on
/// a restart, folds the old id **dead** at its current incarnation — the same incarnation-gated fold a
/// detector's death takes, so the council leader's next reconcile takes it over, bumping its fencing epoch
/// and retiring it — and the new id **alive**, so the leader admits it; and carries the peer's region over to
/// the new id (the region is the node's, not the incarnation's). A forged announcement is counted
/// and folds nothing. Idempotent: a repeat of the same announcement is `Current`. Runs on the control shard,
/// which alone keeps the learned map; the probe loop hands the old id's death to the other shards.
pub(crate) fn learn_member(
  state: &mut ShardState,
  anchor: HostId,
  boot_nonce: u64,
  announced: HostId,
) -> LearnedOutcome {
  let outcome = classify_announced(
    state.learned_members.get(&anchor),
    anchor,
    boot_nonce,
    announced,
  );
  match outcome {
    LearnedOutcome::Forged => {
      state.count(MEMBER_ID_FORGED, 1);
    }
    LearnedOutcome::Current => {
      state.authenticated_members.insert(announced);
      let learned = !state.learned_members.contains_key(&anchor);
      state
        .learned_members
        .entry(anchor)
        .or_insert(LearnedMember {
          boot_nonce,
          host: announced,
          previous: None,
        });
      // A member's anchor learned: the departures the control shard judges are counted by anchor (A-222, A-554).
      if learned {
        crate::remote_attach::look_for_departures(state);
      }
    }
    LearnedOutcome::Restarted { old } => {
      state.authenticated_members.remove(&old);
      state.authenticated_members.insert(announced);
      state.learned_members.insert(
        anchor,
        LearnedMember {
          boot_nonce,
          host: announced,
          previous: Some(old),
        },
      );
      // The anchor's member id changed: the departures the control shard judges are counted by anchor (A-222, A-554).
      crate::remote_attach::look_for_departures(state);
      // A discovery exchange pending on this anchor's link addresses the old incarnation: woken now, it
      // ends invalidated and releases its endpoint, so the link re-dials the new one this period.
      wake_link_waiter(state, anchor);
      if let Some(region) = state.node_regions.get(&old).copied() {
        state.node_regions.insert(announced, region);
      }
      let old_incarnation = state
        .fleet
        .membership()
        .state(old)
        .map_or(0, |belief| belief.incarnation);
      apply_peer_state(
        &mut state.fleet,
        old,
        Some(MemberState {
          liveness: Liveness::Dead,
          incarnation: old_incarnation,
        }),
      );
      apply_peer_state(
        &mut state.fleet,
        announced,
        Some(MemberState {
          liveness: Liveness::Alive,
          incarnation: 0,
        }),
      );
    }
  }
  outcome
}

/// The failure domain the fleet configuration declares for the node behind member `host`, if any (§4.8, D-14
/// — copysets form across distinct domains; `None` is unique-per-host). The declaration is keyed by each
/// node's generation-0 seed id (the manifest precomputes `member_id(anchor, 0)` per node), so a member
/// announced with a fresh boot nonce is resolved to its node
/// through the anchor it was learned under (this node's own through its origin anchor) and looked up by that
/// node's seed (task #22: the new id inherits the node's domain, since the domain is the node's, not the
/// incarnation's). No fleet configuration, or no declaration for the node, is `None`.
fn declared_domain(state: &ShardState, host: HostId) -> Option<DomainId> {
  let fleet = state.config.fleet.as_ref()?;
  if let Some(domain) = fleet.domains.get(&host) {
    return Some(*domain);
  }
  let anchor = if host == state.fleet.host() {
    state.origin_anchor
  } else {
    state
      .learned_members
      .iter()
      .find(|(_, learned)| learned.host == host)
      .map(|(anchor, _)| *anchor)?
  };
  fleet
    .domains
    .get(&crate::deploy::member_id(anchor, 0))
    .copied()
}

/// Only identities learned over authenticated contact may enter a voter configuration.
/// Manifest seeds locate certificates and addresses; optimistic SWIM seeding is not admission. The groups'
/// drives rank their elections against it, and `status` reports the same rank (`verbs::group_report`).
pub(crate) fn authenticated_alive(state: &ShardState) -> Vec<HostId> {
  state
    .fleet
    .membership()
    .alive()
    .into_iter()
    .filter(|host| {
      *host == state.fleet.host()
        || state
          .learned_members
          .values()
          .any(|member| member.host == *host)
    })
    .collect()
}

/// Advances the council death watch (`ShardState::council_death_watch`) one period: for each current council
/// member, a member this node's own SWIM view holds **dead** ([`Liveness::Dead`]) has its consecutive-dead
/// count incremented; a member seen as anything else — alive, suspect, or with no state — has its count
/// reset (removed). A host no longer a member is forgotten. Run every period by every node (leader, voter
/// and learner), so the count is monotonic for a genuinely dead member and a leader inherits the fleet-wide
/// death history across an election, rather than restarting the confirmation window each time leadership
/// moves. A suspected member never accumulates (it is not `Dead`), so it can never be retired.
fn update_council_death_watch(state: &mut ShardState) {
  let now = futures::now_ns();
  let started = *state.death_watch_started_ns.get_or_insert(now);
  let members: Vec<HostId> = state.council.configuration().members.clone();
  state
    .council_death_watch
    .retain(|host, _| members.contains(host));
  let local = state.fleet.host();
  for member in members {
    if member == local {
      continue;
    }
    let dead = state
      .fleet
      .membership()
      .state(member)
      .is_some_and(|belief| belief.liveness == Liveness::Dead)
      || unreached_since_start(state, member, now.saturating_sub(started));
    if dead {
      let count = state.council_death_watch.entry(member).or_insert(0);
      *count = count.saturating_add(1);
    } else {
      state.council_death_watch.remove(&member);
    }
  }
}

/// The council members this **leader** may retire now: those the death watch has held continuously **dead**
/// for at least the death-confirmation window — the council's own election-timeout base
/// (`ShardState::council_timing`, floored at [`slates_cluster::timing::ElectionTiming::floor`]). A death must
/// outlast one election timeout — the longest transient membership disruption, a leader loss and its
/// re-election — before the **irreversible** consensus retirement, so a live voter briefly declared dead
/// while its sessions churn (a fresh joiner most of all) is refuted and reset before it can be retired, while
/// a genuinely dead member crosses the window and is taken over (docs/bugs/2026-09-17-council-retires-a-suspected-voter.md).
/// The members are this region's council by construction, so no region filter is needed.
fn stable_dead_council_members(state: &ShardState) -> Vec<HostId> {
  let window = state
    .council_timing
    .base_periods
    .max(slates_cluster::timing::ElectionTiming::floor().base_periods);
  state
    .council_death_watch
    .iter()
    .filter(|(_, count)| **count >= window)
    .map(|(host, _)| *host)
    .collect()
}

/// Whether this node, a membership horizon (`lease::horizon_ns`) after its death watch began, has never keyed
/// `member` (no probe-plane epoch installed) and holds it alive in no view: its own evidence that `member` is gone.
/// A node that restarted while `member` was down never hears from it, so its detector never holds it at all (it
/// joins a member only once keyed), and without this it never voted the member dead: a restarted council leader then
/// never retired it and no takeover began (`a_member_down_through_a_voters_restart_is_still_taken_over`, 2026-11-07).
/// A live member slow to key may draw this vote too, but alone it is one of the `f + 1` a retirement needs.
fn unreached_since_start(state: &ShardState, member: HostId, since_ns: u64) -> bool {
  since_ns >= crate::lease::horizon_ns()
    && !state.plane.keyed.contains_key(&member)
    && state
      .fleet
      .membership()
      .state(member)
      .is_none_or(|belief| belief.liveness != Liveness::Alive)
}

/// The council members this leader may retire now (§4.8 "Promotion and takeover"): those at least `f + 1` voters
/// hold dead, or every voter while the council has fewer than `f + 1` besides the member (`Quorum::committed_within`),
/// this leader's own stable death watch ([`stable_dead_council_members`]) one of them and each other voter's fresh
/// report ([`ShardState::death_reports`]) one more. A report is fresh within a membership horizon, and counts only at
/// an incarnation at or above any this leader holds the member alive at, so a refutation cancels the reports below it.
/// One voter's lone suspicion, the leader's own included, never retires a member while another voter could report;
/// a forming council's sole voter, and the leader at `f = 0` (R8), retire on their own watch.
pub(crate) fn confirmed_dead_council_members(state: &mut ShardState) -> Vec<HostId> {
  let now = futures::now_ns();
  let horizon = crate::lease::horizon_ns();
  for reports in state.death_reports.values_mut() {
    reports.retain(|_, (_, at)| now.saturating_sub(*at) <= horizon);
  }
  state.death_reports.retain(|_, reports| !reports.is_empty());
  let own: Vec<HostId> = stable_dead_council_members(state);
  let quorum = state.fleet.configuration().quorum;
  let local = state.fleet.host();
  let voters = state.council.voters();
  let mut candidates: Vec<HostId> = own.clone();
  for member in state.death_reports.keys() {
    if !candidates.contains(member) {
      candidates.push(*member);
    }
  }
  candidates
    .into_iter()
    .filter(|member| {
      let alive_at = state
        .fleet
        .membership()
        .state(*member)
        .filter(|belief| belief.liveness == Liveness::Alive)
        .map(|belief| belief.incarnation);
      let reported = state.death_reports.get(member).map_or(0, |reports| {
        reports
          .iter()
          .filter(|(voter, (incarnation, _))| {
            **voter != local
              && voters.contains(voter)
              && alive_at.is_none_or(|alive| *incarnation >= alive)
          })
          .count()
      });
      // `f + 1` of the voters that can vote on the member (itself excluded), or every one of them while the council
      // has fewer, as a joint write needs every member of a cohort smaller than `f + 1`: a forming council's sole voter
      // retires on its own view, since no other voter exists to report, and three voters at `f = 1` still need two.
      let electorate = voters.iter().filter(|voter| *voter != member).count();
      let votes = usize::from(own.contains(member)).saturating_add(reported);
      quorum.committed_within(votes, electorate)
    })
    .collect()
}

/// This node's death votes as a report: each council member its stable death watch holds dead, with the incarnation
/// its view holds it at (0 when it holds none). Format: per member, its id and that incarnation, 8 bytes each, little
/// endian.
fn death_report(state: &ShardState) -> Vec<u8> {
  let mut out = Vec::new();
  for member in stable_dead_council_members(state) {
    let incarnation = state
      .fleet
      .membership()
      .state(member)
      .map_or(0, |belief| belief.incarnation);
    out.extend_from_slice(&member.0.to_le_bytes());
    out.extend_from_slice(&incarnation.to_le_bytes());
  }
  out
}

/// Format: one field of a report's entry, a little-endian `u64` (a member id or an incarnation).
const DEATH_REPORT_FIELD_BYTES: usize = size_of::<u64>();

/// Format: one report's entry, a member then its incarnation ([`DEATH_REPORT_FIELD_BYTES`] each).
const DEATH_REPORT_ENTRY_BYTES: usize = 2 * DEATH_REPORT_FIELD_BYTES;

/// Takes a voter's death report ([`death_report`]) on [`DEATH_STREAM`]: kept only from a voter of this region's
/// council, only about current members, at most one entry per member, and answered empty. Malformed bytes are
/// counted and dropped whole.
fn serve_death_report(state: &mut ShardState, reporter: HostId, request: &[u8]) -> Vec<u8> {
  if !same_region(state, reporter) || !state.council.voters().contains(&reporter) {
    state.count(DEATH_REPORT_REFUSED, 1);
    return Vec::new();
  }
  if !request.len().is_multiple_of(DEATH_REPORT_ENTRY_BYTES) {
    state.count(DEATH_REPORT_REFUSED, 1);
    return Vec::new();
  }
  let members = state.council.configuration().members.clone();
  let now = futures::now_ns();
  for entry in request.as_chunks::<DEATH_REPORT_ENTRY_BYTES>().0 {
    let (member, incarnation) = entry.split_at(DEATH_REPORT_FIELD_BYTES);
    let (Ok(member), Ok(incarnation)) = (
      <[u8; DEATH_REPORT_FIELD_BYTES]>::try_from(member),
      <[u8; DEATH_REPORT_FIELD_BYTES]>::try_from(incarnation),
    ) else {
      continue;
    };
    let member = HostId(u64::from_le_bytes(member));
    if !members.contains(&member) || member == reporter {
      continue;
    }
    state
      .death_reports
      .entry(member)
      .or_default()
      .insert(reporter, (u64::from_le_bytes(incarnation), now));
  }
  state.count(DEATH_REPORT_TAKEN, 1);
  Vec::new()
}

/// Counter: a death report taken from a voter.
const DEATH_REPORT_TAKEN: &str = "fleet.council.death_report_taken";
/// Counter: a death report refused (not a voter of this region's council, or malformed bytes).
const DEATH_REPORT_REFUSED: &str = "fleet.council.death_report_refused";

/// Format: the stream a voter's death report rides on a record session (§4.8 "Promotion and takeover"): a
/// membership-path message, sent once a period by a voter that holds a council member dead, never per write
/// (D-14). Distinct from every other stream the session multiplexes.
const DEATH_STREAM: u64 = 24;

/// Sends this voter's death report to the other voters ([`death_report`]), once a period while it holds any member
/// dead: the leader among them counts it toward the `f + 1` a retirement needs.
async fn send_death_report(others: &[HostId], budget: CommitBudget, in_flight: &mut Vec<Dispatch>) {
  let Some(report) = state::with_state(|s| death_report(s)) else {
    return;
  };
  if report.is_empty() {
    return;
  }
  let sessions = take_sessions("fleet.lent_to.send_death_report", |host| {
    others.contains(&host)
  });
  if sessions.is_empty() {
    return;
  }
  let requests: Vec<(HostId, Vec<u8>, Endpoint)> = sessions
    .into_iter()
    .map(|(host, endpoint)| (host, report.clone(), endpoint))
    .collect();
  let sent: Vec<HostId> = requests.iter().map(|(host, _, _)| *host).collect();
  let (replied, stragglers) = broadcast(requests, DEATH_STREAM, Priority::Metadata, budget).await;
  let recovered: Vec<(HostId, Endpoint)> = replied
    .into_iter()
    .map(|(host, _, endpoint)| (host, endpoint))
    .collect();
  in_flight.push(Dispatch::new(
    sent,
    &recovered,
    stragglers,
    LateReplies::Discard,
  ));
  return_sessions(recovered);
}

/// The regional council serves only its declared region. Root traffic spans all regions.
pub(crate) fn same_region(state: &ShardState, peer: HostId) -> bool {
  let region = |host| {
    state
      .node_regions
      .get(&host)
      .copied()
      .unwrap_or(RegionId(0))
  };
  region(state.fleet.host()) == region(peer)
}

/// By-use evidence for the scheduler floor (§4.8): completed probes and live timer decisions whose
/// measured quantum made them longer than the same decision at the heartbeat floor. Stored on the
/// control shard, with no shared counter on a probe's path. The counts saturate for a long-lived node.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProbeWindows {
  /// Probes acknowledged by the member they addressed; progress even while its host is descheduled.
  pub acknowledged: u64,
  /// Probe budgets longer than the identical path estimate and miss count at the heartbeat floor.
  pub deadlines_dilated: u64,
  /// Sleeps between probes longer than the identical Lifeguard health multiplier alone requires.
  pub periods_dilated: u64,
  /// Largest measured quantum that actually lengthened a probe budget or sleep, nanoseconds.
  pub largest_quantum_ns: u64,
}

/// The slowest measured peer path's tail (`None` with no path measured) — what bounds a round to any set of
/// peers this node dispatches to, so the coordinator's period budget is derived from it.
fn slowest_path_tail_ns() -> Option<u64> {
  state::with_state(|s| s.peer_paths.values().filter_map(PathRtt::tail_ns).max()).flatten()
}

/// Folds one completed round trip to `peer` into its shared path estimate.
pub(crate) fn sample_path(state: &mut ShardState, peer: HostId, round_trip_ns: u64) {
  state
    .peer_paths
    .entry(peer)
    .or_default()
    .on_sample(round_trip_ns);
}

/// Samples the path to every voter that answered inside a consensus round (Karn's rule: a timed-out exchange
/// is no sample; a refusal that came back is).
fn sample_voter_paths(state: &mut ShardState, replies: &[(HostId, TimedReply)]) {
  for (host, reply) in replies {
    if let Some(round_trip_ns) = reply.round_trip_ns {
      sample_path(state, *host, round_trip_ns);
    }
  }
}

/// A group's election timing this period, derived from the measured paths to its other voters
/// (`ElectionTiming::derive`, the floor with none measured), recorded on the shard for the daemon's
/// observation accessors through `record`.
fn derive_group_timing(
  others: &[HostId],
  record: impl FnOnce(&mut ShardState, ElectionTiming),
) -> ElectionTiming {
  state::with_state(|s| {
    let timing = ElectionTiming::derive(
      HEARTBEAT_NS,
      others.iter().filter_map(|host| s.peer_paths.get(host)),
    );
    record(s, timing);
    timing
  })
  .unwrap_or_else(ElectionTiming::floor)
}

/// Shape: the numerator of the lookahead fraction (kept a ratio so no float enters the decision) at which a
/// still-progressing consensus/record round is first considered for an extension — 3 of 4, the last quarter
/// of its current deadline (hyperscale's measured 0.75 late-work lookahead, cited by
/// [`slates_cluster::progress::DeadlineExtender`]).
const CONSENSUS_LOOKAHEAD_NUMERATOR: u64 = 3;
/// Shape: the denominator of that same lookahead fraction — 3/4, the last quarter of the deadline (see
/// [`CONSENSUS_LOOKAHEAD_NUMERATOR`]).
const CONSENSUS_LOOKAHEAD_DENOMINATOR: u64 = 4;

/// The anchors the record-plane and consensus round budget is derived from — the coordinator's own periods
/// ([`round_budget`]): the heartbeat as the period, the SWIM suspicion span as the stall window, the
/// collection cadence as the poll, and the last quarter of the deadline as the lookahead.
const ROUND_ANCHORS: RoundAnchors = RoundAnchors {
  heartbeat_ns: HEARTBEAT_NS,
  stall_periods: SUSPICION_PERIODS,
  polls_per_period: POLL_PER_PERIOD,
  lookahead: (
    CONSENSUS_LOOKAHEAD_NUMERATOR,
    CONSENSUS_LOOKAHEAD_DENOMINATOR,
  ),
};

/// The record-plane and configuration-consensus budget for one period (§4.8 "late work"). Unlike a single
/// SWIM probe (a hard deadline — one missed probe is absorbed by the suspicion
/// window), a record commit, a takeover promotion, a Raft replication round, an election or a learner fetch
/// **gathers replies from several holders or voters at once**, and under CPU starvation — a noisy
/// shared-tenant neighbour, a hypervisor steal, the kernel saturated by another process's syscalls — those
/// replies arrive *late but still arrive*. A hard deadline would declare the round uncertain at the period
/// boundary and re-dispatch it every period: a false-timeout storm that adds transport and scheduling load
/// precisely when the machine is already starved, so the round never converges and the fleet thrashes rather
/// than degrading gracefully (task #31; observed as consensus/takeover tests missing their deadlines under
/// load). This budget instead **extends while the round is still making progress** — its acknowledged set
/// advanced within the stall window — and times out only a round that has genuinely stalled (a dead
/// holder), so a slow-but-progressing fleet converges. At `f = 0` (laptop) there are no peers to gather
/// from, so the extender never fires and the behaviour is the hard budget's (R8).
///
/// Every parameter is derived from the protocol's own periods and the **measured path** ([`round_budget`],
/// `slowest_tail_ns` the slowest measured peer path's round-trip tail this period, [`slowest_path_tail_ns`]):
///
/// - **base deadline** = `max(one period, tail)`: a round is given the slowest peer's round-trip tail before
///   it can be judged stalled with no reply at all — a healthy loopback round completes far inside a period,
///   so on one host this is the one period it always was, but a fixed period expired every round to voters
///   more than three quarters of a period away with their replies still in flight, and no council across a
///   WAN could elect (`docs/bugs/2026-09-14-consensus-round-expires-inside-the-wan-rtt.md`);
/// - **poll interval** = a tenth of a period ([`POLL_PER_PERIOD`]), the collection loop's park between
///   wake-ups, as the probe uses;
/// - **extension** = one period per grant, up to `ELECTION_MARGIN` grants (the election margin the timing
///   law fixes at Raft's ten): a progressing round may extend up to about the floor election timeout — the
///   coherent cap, because a round still gathering replies past that point is one the election timer would
///   already be displacing the leader over, so the extension and the election do not fight (a leader that
///   keeps making progress holds its term; one that stalls yields);
/// - **stall window** = `max(the SWIM suspicion span, tail)` ([`SUSPICION_PERIODS`] periods): a round that
///   gathers no new reply for as long as a peer may go unheard before it is suspected — or for one round-trip
///   tail, whichever is longer — is judged stalled, not merely slow, and is left to time out at its current
///   deadline.
pub(crate) fn consensus_budget(slowest_tail_ns: Option<u64>) -> CommitBudget {
  round_budget(&ROUND_ANCHORS, slowest_tail_ns)
}

/// The budget a live-shipped exchange runs under where no holder's session is at hand (A-200, A-444): a hard deadline of
/// the floor of the session plane's own judgment of a gone path, the persistent-congestion duration (RFC 9002 §7.6.1:
/// three probe timeouts, each `smoothed_rtt + max(4·rttvar, kGranularity) + max_ack_delay`, §6.2.1) of a path with
/// nothing measured, where each probe timeout is the initial one (§6.2.2): no holder's session is taken for gone sooner,
/// so no live exchange gives a holder up sooner. Past it a holder that has not answered is not a live one to wait for (its
/// link is dropped and dialed afresh, `live_ship::lose_links`), with the record plane's poll between wake-ups (a reply
/// wakes the collector at once). An exchange that holds its holder's session takes that session's own duration
/// (`live_ship`'s `exchange_budget`). Until A-444 the deadline was one liveness window, under that floor, so the live lane
/// dropped links its sessions still counted live. Not the consensus round's budget: that is derived from the slowest
/// path's tail, which a paused holder inflates, so one exchange to it held the holder's only session past a 40 s wait and
/// the resumed holder never caught up (2026-11-05, `live_fleet`, 1 of 2 runs). A holder is not a voter; each answers on
/// its own.
pub(crate) fn live_budget() -> CommitBudget {
  CommitBudget::hard(
    slates_transport::rtt::RttEstimator::new().persistent_congestion_duration_floored(0),
    (HEARTBEAT_NS / POLL_PER_PERIOD).max(1),
  )
}

/// The bound on one record-plane exchange: the measured control-plane round budget's full span, the bound every
/// record-plane exchange runs under.
pub(crate) fn record_exchange_budget_ns() -> u64 {
  consensus_budget(slowest_path_tail_ns()).max_deadline_ns()
}

/// Serves the network export's listener on this shard (§4.6 "Kubernetes publication without privilege",
/// AUD-29-75), each connection's session built from the kept `identity`. A listener whose port cannot be read,
/// or whose task the arena refused, is counted, never silent.
fn serve_export(export: Option<ExportListener>, identity: Kept<Identity>) {
  let Some(listener) = export else {
    return;
  };
  let Ok(port) = listener.local_addr().map(|address| address.port()) else {
    count_refusal(BIND_REFUSED);
    return;
  };
  match slates_rt::futures::spawn(crate::nfs_tls::serve_network(listener, port, identity)) {
    Ok(task) => {
      let _ = slates_rt::futures::detach(task);
    }
    Err(_) => {
      count_refusal(BIND_REFUSED);
    }
  }
}

/// Runs the fleet membership loop for `transport` on the control shard (§4.8, boot step 6). It serves on
/// this node's two bound sockets (one per plane, every peer on each through a demultiplexer) and spawns their
/// receive and accept loops; for each peer it dials the peer's advertised addresses and spawns the probe and
/// record-link tasks; then the one record-plane coordinator. A two-node fleet is the single-peer degenerate.
/// Detached tasks: they live as long as the shard and are cancelled by the runtime's shutdown.
pub async fn run_membership(transport: FleetTransport) {
  let FleetTransport {
    advertise,
    advertise_record,
    enrollment_roots,
    identity,
    name,
    serve:
      ServeSockets {
        probe: probe_socket,
        record: record_socket,
        export,
      },
    peers,
    resolver,
  } = transport;
  if peers.is_empty() && enrollment_roots.is_empty() {
    // No peers — nothing to probe; the placement path still runs the `FleetNode`, degenerate (R8).
    return;
  }
  // The identity is shared by every serve session and client dial of this shard — it is not `Clone` (it
  // holds a private key), so the control shard owns the one copy for its life (`ShardContext::keep`) and
  // each task names it by its `Kept` handle, reaching it only inside a closure while this shard runs
  // (AUD-29-08); it is dropped with the shard's context, after every task. Before 2026-09-14 it was leaked
  // once per daemon boot, and until 2026-09-30 it was lent as a `&'static` that could outlive the context.
  // The resolver and the node name every dial task shares are kept the same way.
  let Some(Ok(identity)) = slates_rt::registry::with_current(|ctx| {
    ctx.keep(identity.with_authorities(enrollment_roots.clone()))
  }) else {
    count_refusal(BIND_REFUSED);
    return;
  };
  let resolver: Option<Kept<Resolver>> = resolver.and_then(|resolver| {
    slates_rt::registry::with_current(|ctx| ctx.keep(resolver).ok()).flatten()
  });
  let Some(Ok(kept_name)) = slates_rt::registry::with_current(|ctx| ctx.keep(name.clone())) else {
    count_refusal(BIND_REFUSED);
    return;
  };
  let Some(certificate) = identity.with(Identity::certificate) else {
    count_refusal(BIND_REFUSED);
    return;
  };
  serve_export(export, identity);
  let neighbourhood =
    state::with_state(|state| state.config.fleet_peer_capacity.saturating_add(1)).unwrap_or(1);
  let local = state::with_state(|s| s.fleet.host()).unwrap_or(HostId(0));
  // Discovery announces both planes where this node advertises them, never where it binds (A-277, A-330): a peer
  // prefers the announcement at every dial (`resolve_peer_address`), and behind a port mapping the bound record port is
  // reached around the mapping or not at all.
  let initialized = state::with_state(|state| {
    state.discovery = Some(crate::discovery::Discovery::new(
      state,
      name.clone(),
      enrollment_roots.clone(),
      certificate,
      (advertise, advertise_record),
      &peers,
    )?);
    crate::discovery::restore(state)
  });
  let restored = match initialized {
    Some(Ok(restored)) => restored,
    Some(Err(refusal)) => {
      count_refusal(refusal.counter());
      return;
    }
    None => return,
  };
  let driver = PeerDriver {
    identity,
    name: kept_name,
    resolver,
  };
  // The boot line of the fleet's derived timing (R3: every derived value logged with its inputs). Nothing is
  // measured yet, so every value is at its floor; each period re-derives it from the measured paths, and
  // `Daemon::council_timing` / `slates status` read the live values.
  let floor = ElectionTiming::floor();
  let round = consensus_budget(None);
  slates_machine::log_line!(
    "slates-server: fleet timing: election timeout = {} × max(broadcast RTT tail over the voters, heartbeat {} ns) = {} periods at the floor (span the same over the tail's spread: {} periods); round budget base = max(heartbeat, tail) = {} ns, stall = max({} periods, tail) = {} ns, extensions {} × {} ns; no path measured yet",
    slates_cluster::timing::ELECTION_MARGIN,
    HEARTBEAT_NS,
    floor.base_periods,
    floor.span_periods,
    round.deadline_ns,
    SUSPICION_PERIODS,
    HEARTBEAT_NS.saturating_mul(u64::from(SUSPICION_PERIODS)),
    slates_cluster::timing::ELECTION_MARGIN,
    HEARTBEAT_NS,
  );

  // One serve socket per plane, shared by every peer through a demultiplexer; both were bound before the
  // daemon started (`FleetTransport::bind`, or adopted by a caller that holds its ports), so a peer's dial
  // finds a listener and an address in use refused the start by name.
  // The roster: which peer a certificate names — mutual TLS admits only these, and the record serve side
  // resolves the peer it authenticated through it.
  let roster: Vec<Rostered> = peers
    .iter()
    .map(|peer| Rostered {
      certificate: peer.certificate.clone(),
      anchor: peer.anchor,
    })
    .collect();
  let allowed: Vec<CertificateDer<'static>> = roster
    .iter()
    .map(|peer| peer.certificate.clone())
    .chain(enrollment_roots)
    .collect();
  // Both planes use the same peer capacity and transport-owned session multiplier as the task/timer
  // budget: one pending handshake plus the authenticated live/replacement pair per capacity unit.
  let peer_capacity = state::with_state(|state| state.config.fleet_peer_capacity).unwrap_or(0);
  // The demultiplexers are owned by this shard for its life and dropped with it (their sockets closed,
  // the ports free again — a restarted node binds the same addresses); a start refused here means this
  // loop is not on a shard thread, counted like a socket that would not bind.
  let Ok(record_demux) = Demux::start(
    record_socket,
    identity,
    allowed,
    fleet_shape(),
    peer_capacity,
  ) else {
    count_refusal(BIND_REFUSED);
    return;
  };
  state::with_state(|s| s.demuxes = vec![record_demux]);
  spawn_detached(run_demux(record_demux), LOOP_SPAWN_REFUSED);
  // The probe port carries the sealed membership plane (A-67 H-2): one detector for every peer, its probes datagrams
  // keyed from each pair's canonical record session (`crate::member_task`).
  let boot_nonce = state::with_state(|s| s.member_boot_nonce).unwrap_or(0);
  let members = std::num::NonZeroUsize::new(neighbourhood).unwrap_or(std::num::NonZeroUsize::MIN);
  spawn_detached(
    crate::member_task::run(probe_socket, local, boot_nonce, members, resolver),
    LOOP_SPAWN_REFUSED,
  );
  spawn_detached(
    accept_records(record_demux, local, roster, driver),
    LOOP_SPAWN_REFUSED,
  );

  for peer in peers.into_iter().chain(restored) {
    driver.start(peer);
  }

  // One record-plane coordinator for all peers (§4.8 "records are sent to all candidates"): it borrows every
  // holder session the link tasks keep up, so it ships each head to all candidates in one commit and drives
  // each takeover over all surviving holders (the `f > 1` promotion a per-peer ship task could not reach).
  spawn_detached(run_record_plane(local), LOOP_SPAWN_REFUSED);
  // The work route keeper (A-288): parked until a takeover adopts a work's route, then each period asks the greens'
  // owners about the routes this node keeps, on its own task so a slow question never holds the coordinator.
  spawn_detached(crate::work_route::run(local), LOOP_SPAWN_REFUSED);
}

/// Shared immutable dial inputs: handles of what the control shard keeps for its life (its identity, its
/// name, the resolver) and two plain values, so every task holds its own copy (AUD-29-08). Each candidate
/// owns its two tasks.
#[derive(Clone, Copy)]
struct PeerDriver {
  identity: Kept<Identity>,
  name: Kept<String>,
  resolver: Option<Kept<Resolver>>,
}

impl PeerDriver {
  /// Spawns a candidate's probe and record tasks; a node name no longer reachable (the control shard's
  /// context ended) spawns neither, counted as a refused spawn.
  fn start(self, peer: FleetPeer) {
    let Some(name) = self.name.with(String::clone) else {
      count_refusal(LOOP_SPAWN_REFUSED);
      return;
    };
    // The peer's probe-plane address, for the membership task (A-67 H-2): its datagrams go there once the pair's
    // canonical record session has keyed an epoch.
    let plane_peer = crate::member_task::PlanePeer {
      seed: peer.host,
      address: peer.address,
      certificate: peer.certificate.clone(),
      resolved: None,
      stale: false,
      seed_retired: false,
    };
    let _ = state::with_state_counted(|s| {
      s.plane.peers.insert(peer.anchor, plane_peer);
      // A member's seed id now maps to its anchor: the departures the control shard judges are counted by anchor
      // (A-222, A-554).
      crate::remote_attach::look_for_departures(s);
    });
    let record = PeerDial {
      anchor: peer.anchor,
      host: peer.host,
      name,
      address: peer.record_address,
      certificate: peer.certificate,
      resolver: self.resolver,
    };
    spawn_detached(establish_record_link(self, record), LOOP_SPAWN_REFUSED);
  }
}

/// A serve socket's receive loop as a task, for the daemon's life: it routes every datagram to its session.
/// Only the socket gone ends it, counted (`fleet.serve`), so an operator sees a node that stopped accepting
/// rather than a mesh that silently never re-forms; every other refused receive is counted by its kind
/// ([`SERVE_RECEIVE_EARLIER_SEND`] and its siblings) and the loop reads on.
async fn run_demux(demux: DemuxId) {
  if let Err(e) = demux.run().await {
    // Counted for `status`, and said once in the log with its reason: a node that silently stopped
    // accepting sessions on a plane is a mesh that never forms with nothing to read but a count.
    count_refusal(SERVE_REFUSED);
    slates_machine::log_line!("slates-server: fleet: a serve socket's receive loop ended: {e:?}");
  }
}

/// Accepts every record session a peer dials on the record socket and serves it over this node's durable
/// holds (§4.8): one serve task per session, which resolves the peer it authenticated through `roster`.
/// Bounded as the peer accept loops are.
async fn accept_records(demux: DemuxId, local: HostId, roster: Vec<Rostered>, driver: PeerDriver) {
  loop {
    // An unreachable demultiplexer (the shard's context ended) ends the loop, counted as a serve socket
    // that stopped accepting.
    let Ok(session) = demux.accept().await else {
      count_refusal(SERVE_REFUSED);
      return;
    };
    spawn_detached(
      serve_peer_records(session, local, roster.clone(), driver),
      SERVE_SPAWN_REFUSED,
    );
  }
}

/// Binds a fresh socket and builds an **un-established** client endpoint for `address` — the handshake is
/// driven separately ([`Endpoint::establish`]) so the caller can **retry it on this same socket** until the
/// peer's `accept` completes, rather than re-dialing from a fresh port. Retrying on one socket is what lets
/// a handshake finish under contention: `accept` pins the first source it hears, so its half-open state
/// waits for *this* source's next flight; a fresh-port re-dial is a new source it ignores, stranding the
/// session (the record-plane cause of the takeover flaking under load). A peer addressed by a DNS name is
/// resolved here, at this fresh dial, so a peer that moved (a rescheduled pod) is reached at its new address
/// on the next re-dial; a name that does not resolve is counted (`fleet.resolve`) and the caller's next
/// period dials again. The socket binds every interface: a peer on another host is dialed from the address
/// the kernel routes to it (a loopback-bound socket cannot send off the host). `None` if the name did not
/// resolve or the runtime refuses the socket or endpoint.
/// The socket address of a peer's `plane`: discovery's address for its certificate first, else the manifest's
/// `address` (a name resolved through `resolver`). A refusal is counted (the first of each kind logged).
pub(crate) async fn resolve_peer_address(
  address: &NodeAddress,
  plane: crate::deploy::Plane,
  certificate: &CertificateDer<'static>,
  resolver: Option<Kept<Resolver>>,
) -> Option<SocketAddrV4> {
  let discovered = state::with_state(|state| {
    state
      .discovery
      .as_ref()
      .and_then(|discovery| discovery.address(certificate, plane))
  })
  .flatten();
  let address = discovered.as_ref().unwrap_or(address);
  Some(match address {
    NodeAddress::Ip(address) => *address,
    NodeAddress::Name { host, port } => {
      let Some(resolver) = resolver else {
        count_refusal(RESOLVE_NO_RESOLVER);
        return None;
      };
      // The lookup awaits the nameservers, so it runs over its own copy of the configuration (a few
      // addresses and two numbers), never over a borrow of the kept one.
      let Some(resolver) = resolver.with(Resolver::clone) else {
        count_refusal(KEPT_UNREACHABLE);
        return None;
      };
      match dns::lookup(&resolver, host).await {
        Ok(ip) => SocketAddrV4::new(ip, *port),
        Err(e) => {
          // The first refusal of each kind is logged once, so an operator's first look at the daemon's log
          // names the cause; the counts in `status` carry the rest.
          if count_refusal(resolve_refusal(&e)) == 1 {
            slates_machine::log_line!("slates-server: fleet: resolving `{host}`: {e}");
          }
          return None;
        }
      }
    }
  })
}

async fn client_for(
  identity: Kept<Identity>,
  name: &str,
  address: (&NodeAddress, crate::deploy::Plane),
  certificate: &CertificateDer<'static>,
  resolver: Option<Kept<Resolver>>,
) -> Option<Endpoint> {
  let (address, plane) = address;
  let peer = resolve_peer_address(address, plane, certificate, resolver).await?;
  let Ok(socket) = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)) else {
    count_refusal(DIAL_SOCKET_REFUSED);
    return None;
  };
  let shape = fleet_shape();
  let Some(built) =
    identity.with(|identity| Endpoint::client(socket, peer, identity, certificate, name, shape))
  else {
    count_refusal(KEPT_UNREACHABLE);
    return None;
  };
  match built {
    Ok(endpoint) => Some(endpoint),
    Err(_) => {
      count_refusal(DIAL_CLIENT_REFUSED);
      None
    }
  }
}

/// The connection shape every fleet session is built with: the fleet frame cap, the derived receive
/// ceiling (`DaemonConfig::fleet_session_receive_bytes`), and the session plane's congestion controller.
fn fleet_shape() -> ConnectionShape {
  let ceiling = state::with_state(|state| state.config.fleet_session_receive_bytes).unwrap_or(0);
  ConnectionShape::for_frame_cap(FLEET_FRAME_CAP, ceiling)
}

/// The status refusal name for a failed name lookup: `fleet.resolve.<kind>` ([`dns::DnsError::kind`]).
fn resolve_refusal(error: &dns::DnsError) -> &'static str {
  match error.kind() {
    "timeout" => RESOLVE_TIMEOUT,
    "refused" => RESOLVE_NXDOMAIN,
    "no-address" => RESOLVE_NO_ADDRESS,
    "malformed" => RESOLVE_MALFORMED,
    "io" => RESOLVE_IO,
    _ => RESOLVE_REFUSED,
  }
}

/// Advances a session toward established, one handshake attempt per call (the probe and ship loops call it
/// each period): if a `session` is already up it is returned unchanged; otherwise the held un-established
/// `client` is driven one `establish` step **on its same socket** (kept for a retry on failure, so the
/// peer's pinned `accept` completes rather than a fresh-port re-dial being ignored — the establishment
/// fragility that flaked both the probe mesh's formation and the record plane's takeover under load), and a
/// client that was consumed but whose session was then lost is rebuilt, its handshake run in the same call. Built in
/// one call and driven in the next, every redial waited a whole link period first: a median of 101 ms over 487 redials
/// (diag-live-durability-50), on the way of every resumed peer's catch-up. Returns the updated `(client, session)`.
async fn establish_session(
  client: Option<Endpoint>,
  session: Option<Endpoint>,
  identity: Kept<Identity>,
  name: &str,
  address: (&NodeAddress, crate::deploy::Plane),
  certificate: &CertificateDer<'static>,
  resolver: Option<Kept<Resolver>>,
) -> (Option<Endpoint>, Option<Endpoint>) {
  let (address, plane) = address;
  if session.is_some() {
    return (client, session);
  }
  let client = match client {
    Some(endpoint) => Some(endpoint),
    None => client_for(identity, name, (address, plane), certificate, resolver).await,
  };
  match client {
    Some(mut endpoint) => match endpoint.establish().await {
      Ok(()) => (None, Some(endpoint)),
      // The handshake ran out a budget with the peer silent: keep the socket and its pending flight for
      // the next period's call, which resends the flight (`Endpoint::establish`), up to
      // `ESTABLISH_BUDGETS_BEFORE_REDIAL` budgets. Past that — or on any protocol fault (a peer whose
      // half-open state for this source is gone answers a resent flight with a fresh handshake this end
      // cannot continue) — the endpoint is dropped, so the next period dials afresh from a new port (the
      // demultiplexer replaces a peer's old session on a re-dial). Before this, an endpoint whose first
      // budget ran out was kept forever and, its flight being a local of that first call, never sent
      // another byte (`docs/bugs/2026-09-14-handshake-retry-forgets-its-flight.md`).
      Err(EndpointError::NotReady)
        if keeps_its_socket(
          endpoint.handshake_budgets_spent(),
          endpoint.handshake_stall(),
        ) =>
      {
        count_refusal(dial_stall_counter(endpoint.handshake_stall()));
        (Some(endpoint), None)
      }
      Err(EndpointError::NotReady) => {
        count_refusal(dial_stall_counter(endpoint.handshake_stall()));
        // Counted, and said once: a dialer that keeps dialing afresh names the peer it cannot reach.
        if count_refusal(DIAL_REDIAL) == 1 {
          slates_machine::log_line!(
            "slates-server: fleet: no reply from {address} through {ESTABLISH_BUDGETS_BEFORE_REDIAL} handshake budgets; dialing afresh from a new port"
          );
        }
        (None, None)
      }
      Err(e) => {
        count_refusal(dial_fault_counter(&e));
        if count_refusal(DIAL_FAULT) == 1 {
          slates_machine::log_line!(
            "slates-server: fleet: the handshake to {address} faulted: {e:?}"
          );
        }
        (None, None)
      }
    },
    None => (None, None),
  }
}

/// Whether a dialer whose `establish` ended `NotReady` retries on the same socket rather than dialing afresh. A peer
/// that has not answered may still be booting or starved, so it gets [`ESTABLISH_BUDGETS_BEFORE_REDIAL`] budgets. A
/// dialer past its confirmation has heard from its peer and is waiting on an end that gave up: another budget on this
/// socket is another 22 s waiting on state that is gone, which is 44 s of a 41.9 s formation at load 170 (2026-11-08).
fn keeps_its_socket(budgets_spent: u32, stall: Option<HandshakeStall>) -> bool {
  !matches!(
    stall,
    Some(
      HandshakeStall::ConfirmDeadline
        | HandshakeStall::ConfirmSilence
        | HandshakeStall::ConfirmGarbage
    )
  ) && budgets_spent < ESTABLISH_BUDGETS_BEFORE_REDIAL
}

/// Shape: the handshake budgets a dialer spends on one socket before it dials afresh from a new port. The
/// first budget covers a peer not yet listening when first dialed (a fleet forms as its nodes boot one
/// after another); the second resends the pending flight through a whole budget once more, for a peer that
/// was merely starved. A flight resent for a full budget with no reply means the peer's half-open state for
/// this source is gone or the peer is down, and a fresh dial — a new source the demultiplexer opens a fresh
/// session for — is the recovery.
pub const ESTABLISH_BUDGETS_BEFORE_REDIAL: u32 = 2;

/// Counts one refusal or event of `kind` on a shard state already borrowed (the in-closure form of
/// [`count_refusal`], which borrows the state itself).
pub(crate) fn count_refusal_in(state: &mut ShardState, kind: &'static str) {
  let count = state.refusals.entry(kind).or_insert(0);
  *count = count.saturating_add(1);
}

/// A record session `anchor`'s node dialed to this one, counted in [`ShardState::dialed_in`] for as long as this node
/// serves it ([`serve_peer_records`]), however the serve ends (A-337). Made and dropped on the control shard, where
/// sessions are served.
struct DialedIn {
  anchor: HostId,
}

impl DialedIn {
  /// Counts a session `anchor` dialed, for this guard's life.
  fn counted(anchor: HostId) -> DialedIn {
    let _ = state::with_state_counted(|s| {
      let sessions = s.dialed_in.entry(anchor).or_insert(0);
      *sessions = sessions.saturating_add(1);
    });
    DialedIn { anchor }
  }
}

impl Drop for DialedIn {
  fn drop(&mut self) {
    let anchor = self.anchor;
    let _ = state::with_state_counted(|s| {
      let left = s.dialed_in.get_mut(&anchor).map(|sessions| {
        *sessions = sessions.saturating_sub(1);
        *sessions
      });
      if left == Some(0) {
        s.dialed_in.remove(&anchor);
      }
    });
  }
}

/// The stable anchor of the peer that presented `certificate`: the roster's for that certificate, or the one the
/// certificate itself derives when the roster names it not (task #22's two-id model).
fn rostered_anchor(roster: &[Rostered], certificate: &CertificateDer<'static>) -> HostId {
  roster
    .iter()
    .find(|peer| peer.certificate == *certificate)
    .map_or_else(
      || crate::deploy::host_id_of_certificate(certificate),
      |peer| peer.anchor,
    )
}

/// Whether this node's discovery recognizes the peer that presented `certificate` (its enrollment).
fn discovery_recognizes(certificate: &CertificateDer<'static>) -> bool {
  state::with_state(|state| {
    state
      .discovery
      .as_ref()
      .and_then(|discovery| discovery.recognizes(certificate))
  })
  .flatten()
  .is_some()
}

/// The peer's next request, or a forward this node finished for it; none once the session ends (a peer's reset
/// counted).
async fn next_or_end(
  endpoint: &mut Endpoint,
  forwards: &mut crate::forward_lane::ServedForwards,
) -> Option<crate::forward_lane::Next> {
  match crate::forward_lane::next_request_or_forwarded(endpoint, forwards).await {
    Ok(next) => Some(next),
    Err(EndpointError::PeerReset) => {
      count_refusal(SERVE_PEER_RESET);
      None
    }
    Err(_) => None,
  }
}

/// A discovery exchange's reply to the peer of `certificate`, the peer it names started on `driver`; nothing (counted)
/// for a refused or unanswered one.
async fn served_discovery(
  certificate: &CertificateDer<'static>,
  request: &[u8],
  driver: &PeerDriver,
) -> Vec<u8> {
  // Test support: a fault holds this reply after the request arrived (never reachable from the
  // wire), so a peer's exchange is pending — its endpoint borrowed — while this node is then
  // stopped: the interrupted-discovery restart the replacement-voter regression forces.
  while state::with_state(|state| state.discovery_withhold_replies).unwrap_or(false) {
    crate::daemon::pace(HEARTBEAT_NS / POLL_PER_PERIOD).await;
  }
  let outcome = state::with_state(|state| crate::discovery::serve(state, certificate, request));
  match outcome {
    Some(Ok((reply, peer))) => {
      if let Some(peer) = peer {
        driver.start(peer);
      }
      reply
    }
    Some(Err(refusal)) => {
      count_refusal(refusal.counter());
      Vec::new()
    }
    None => Vec::new(),
  }
}

/// Whether a request of `kind` is a takeover's fetch of a live log (g251).
fn is_live_fetch(kind: u64, request: &[u8]) -> bool {
  kind == slates_cluster::live::LIVE_STATE_STREAM && slates_db::live::is_fetch(request)
}

/// Whether a request of `kind` is a live message large enough to be hashed in turns of its own (A-757 part 4).
fn is_large_live_message(kind: u64, request: &[u8]) -> bool {
  (kind == slates_cluster::live::LIVE_ENTRY_STREAM
    || kind == slates_cluster::live::LIVE_ENTRIES_STREAM)
    && crate::live_ship::hashed_in_turns(request.len())
}

/// Where a peer's request is served: on a task of its own (its stream id, and its reply when the task answered at once),
/// or here, one at a time (its stream id, kind and bytes).
enum PeerRequestServed {
  Spawned(u64, Option<Vec<u8>>),
  Here(u64, u64, Vec<u8>),
}

/// Whether the session still holds after a spawned request's reply, `served` when its task answered at once: the
/// reply sent on `stream_id`, or none yet to send.
fn replied(endpoint: &mut Endpoint, stream_id: u64, served: Option<Vec<u8>>) -> bool {
  served.is_none_or(|reply| endpoint.reply(stream_id, reply).is_ok())
}

/// The member id currently learned for `peer_anchor`, or its `seed` until it announces itself.
fn learned_host(peer_anchor: HostId, seed: HostId) -> HostId {
  state::with_state(|s| {
    s.learned_members
      .get(&peer_anchor)
      .map_or(seed, |learned| learned.host)
  })
  .unwrap_or(seed)
}

/// The record session `endpoint` holds, established: the certificate its peer presented, or none (counted) where the
/// handshake failed or the peer presented none.
async fn established_peer(endpoint: &mut Endpoint) -> Option<CertificateDer<'static>> {
  if let Err(e) = endpoint.establish().await {
    count_accept_failure(&e, endpoint.handshake_stall(), "record");
    return None;
  }
  let certificate = endpoint.peer_certificate();
  if certificate.is_none() {
    count_refusal(ACCEPT_REFUSED);
  }
  certificate
}

/// The record serve side (§4.8 "records are sent to all candidates"; "Promotion and takeover"): complete
/// the accepted session's handshake and loop answering the peer over this node's per-object holds — a
/// **record** commit is accepted into the object's durable acceptor (this node backs the peer as a
/// candidate holder), and a **prepare** (a new owner's phase-one message when it takes over an object) is
/// answered from that same hold with a binding promise. Both are served here because the session plane
/// carries both and `serve_once` hands the handler the raw request either way; the two never collide, a
/// [`slates_db::register::Prepare`] being a fixed 40 bytes and a [`Record`] always longer (its prefix alone exceeds that), so
/// the length disambiguates. The acceptor authorizes the record's or prepare's owner and configuration generation against
/// the authority the takeover installed, fences the epoch, and refuses a stale writer. The peer is whoever
/// the handshake authenticated — its certificate names it in `roster` (mutual TLS admits only roster
/// certificates; one not found is counted, never served). A serve failure (the peer's connection dropped
/// when it died, or its session replaced by a re-dial) ends the loop.
async fn serve_peer_records(
  mut endpoint: Endpoint,
  local: HostId,
  roster: Vec<Rostered>,
  driver: PeerDriver,
) {
  let Some(certificate) = established_peer(&mut endpoint).await else {
    return;
  };
  // Two ids for the authenticated peer (task #22 two-id model). `peer_anchor` is its **stable** anchor — the
  // one the roster holds for the certificate it presented, the same anchor its member ids derive from and its
  // announcements are validated against, so every plane names the node by one id. The RIFL completion origin
  // keys on it, so a forwarded write stays exactly-once across the forwarding node's restart. Its **ephemeral**
  // member id — what records and ownership key on — is whatever is currently learned for that anchor (its
  // seed until it announces itself; a restart moves it), resolved per request so a node that restarted
  // mid-session is served under the id it now writes as.
  let peer_anchor = rostered_anchor(&roster, &certificate);
  let seed = crate::deploy::member_id(peer_anchor, 0);
  // The peer dialed this session and keeps it only while it keeps direct contact with this node (A-337).
  let _dialed_in = DialedIn::counted(peer_anchor);
  // The membership plane's secret for this connection (A-67 H-2b), should the peer announce an epoch on it.
  let plane_secret = endpoint.export_secret(hyper_datagram::EXPORTER_LABEL).ok();
  // Serve the peer's commits and prepares against this node's **durable** per-object holds in the shard
  // state, so an accepted record survives past this task — the state a survivor's phase-one recovery reads
  // on a takeover — and a prepare is answered from it. The handler runs synchronously inside `serve_once` (a
  // brief `with_state` borrow, no await held across it) — except a forwarded verb ([`FORWARD_STREAM`]), whose
  // reply comes from an `xshard` call to the volume's owner shard, so it is served asynchronously
  // (`serve_once_async`); the request is acknowledged before the handler awaits, so the peer just waits for the
  // reply. A serve failure ends the loop. Each request kind rides its own stream id, so the dispatch is by
  // kind: a record commit, a phase-one prepare, a content exchange (§4.10), a consensus step, or a forwarded
  // verb — never a guess from the bytes.
  let control = state::with_state(|s| s.shard).unwrap_or_default();
  let mut enrolled = false;
  let mut forwards = crate::forward_lane::ServedForwards::new();
  loop {
    // Enrolled once the discovery recognizes the peer's certificate, and asked again only until then.
    enrolled = enrolled || discovery_recognizes(&certificate);
    // A forwarded verb runs on its own task (`crate::forward_lane::ServedForwards`), so one waiting on its owner shard
    // holds no other request of this peer; every other request is served here, one at a time, as before.
    let Some(next) = next_or_end(&mut endpoint, &mut forwards).await else {
      return;
    };
    let dispatched = match next {
      crate::forward_lane::Next::Forwarded { stream_id, reply } => {
        PeerRequestServed::Spawned(stream_id, Some(reply))
      }
      crate::forward_lane::Next::Request {
        stream_id,
        kind,
        request,
      } if kind == FORWARD_STREAM && enrolled => {
        let served = forwards.spawn(stream_id, async move {
          injected_serve_delay().await;
          verbs::injected_forward_silence(&request).await;
          verbs::serve_forward(control, peer_anchor, &request).await
        });
        PeerRequestServed::Spawned(stream_id, served)
      }
      // A takeover's fetch of a live log is laid out in turns of this shard (g251), so it too runs on its own task: a
      // long log's answer holds no other request of the peer.
      crate::forward_lane::Next::Request {
        stream_id,
        kind,
        request,
      } if enrolled && is_live_fetch(kind, &request) => {
        let peer_host = learned_host(peer_anchor, seed);
        let served = forwards.spawn(stream_id, async move {
          withheld_live_state().await;
          crate::live_ship::serve_fetch(local, peer_host, request).await
        });
        PeerRequestServed::Spawned(stream_id, served)
      }
      // A large live message is taken in turns of its own (A-757 part 4): its entries' identities are hashed a slice a
      // turn before they are taken, so it holds no other request of the peer and no poll of this shard long. The owner
      // sends a holder's next message only once this one is answered, so its entries stay in order.
      crate::forward_lane::Next::Request {
        stream_id,
        kind,
        request,
      } if enrolled && is_large_live_message(kind, &request) => {
        let peer_host = learned_host(peer_anchor, seed);
        let served = forwards.spawn(stream_id, async move {
          let object = slates_db::live::object_of(&request);
          let admission = crate::live_ship::admit_claim(control, local, peer_host, object).await;
          match admission.refusal() {
            Some(refusal) => refusal,
            None => {
              let batch = kind == slates_cluster::live::LIVE_ENTRIES_STREAM;
              crate::live_ship::serve_entries_hashed(local, peer_host, batch, request).await
            }
          }
        });
        PeerRequestServed::Spawned(stream_id, served)
      }
      // A work route's question (A-288) is answered on the green's shard, so it too runs on its own task.
      crate::forward_lane::Next::Request {
        stream_id,
        kind,
        request,
      } if kind == crate::work_route::STREAM && enrolled => {
        let served = forwards.spawn(stream_id, async move {
          crate::work_route::serve(control, &request).await
        });
        PeerRequestServed::Spawned(stream_id, served)
      }
      crate::forward_lane::Next::Request {
        stream_id,
        kind,
        request,
      } => PeerRequestServed::Here(stream_id, kind, request),
    };
    let (stream_id, stream, request) = match dispatched {
      PeerRequestServed::Spawned(stream_id, served) => {
        if !replied(&mut endpoint, stream_id, served) {
          return;
        }
        continue;
      }
      PeerRequestServed::Here(stream_id, stream, request) => (stream_id, stream, request),
    };
    let reply = {
      let certificate = certificate.clone();
      async move {
        if stream == crate::discovery::STREAM {
          return served_discovery(&certificate, &request, &driver).await;
        }
        if !enrolled {
          count_refusal(ACCEPT_REFUSED);
          return Vec::new();
        }
        if stream == slates_cluster::live::LIVE_STATE_STREAM {
          withheld_live_state().await;
        }
        match stream {
          crate::member_task::PLANE_EPOCH_STREAM => plane_secret
            .and_then(|secret| {
              state::with_state(|s| {
                crate::member_task::accept_announcement(s, peer_anchor, &secret, &request)
              })
            })
            .unwrap_or_default(),
          RECORD_STREAM => Record::decode(&request)
            .ok()
            .and_then(|record| {
              state::with_state(|s| {
                let peer_host = s
                  .learned_members
                  .get(&peer_anchor)
                  .map_or(seed, |learned| learned.host);
                accept_held_record(s, local, peer_host, &record)
              })
            })
            .unwrap_or_default(),
          crate::mirror::MIRROR_PROMISE_STREAM => state::with_state(|s| {
            let peer_host = s
              .learned_members
              .get(&peer_anchor)
              .map_or(seed, |learned| learned.host);
            crate::mirror::serve_promise(s, local, peer_host, &request)
          })
          .unwrap_or_default(),
          // A message that puts a live log's entries in a copy first has the volume's claim admitted on this node
          // (§4.2, g229: `live_ship::admit_claim`), answered `NoRoom` when it cannot be.
          slates_cluster::live::LIVE_ENTRY_STREAM | slates_cluster::live::LIVE_ENTRIES_STREAM => {
            let peer_host = learned_host(peer_anchor, seed);
            let object = slates_db::live::object_of(&request);
            let admission = crate::live_ship::admit_claim(control, local, peer_host, object).await;
            match admission.refusal() {
              Some(refusal) => refusal,
              None => state::with_state(|s| {
                if stream == slates_cluster::live::LIVE_ENTRY_STREAM {
                  crate::live_ship::serve_entry(s, local, peer_host, &request)
                } else {
                  crate::live_ship::serve_entries(s, local, peer_host, &request)
                }
              })
              .unwrap_or_default(),
            }
          }
          slates_cluster::live::LIVE_STATE_STREAM => {
            let peer_host = learned_host(peer_anchor, seed);
            // An owner's ask that this candidate make a volume's admission before it turns live shipping on (§4.2, A-401).
            if slates_db::live::is_admit(&request) {
              return crate::live_ship::serve_admit(control, local, peer_host, &request).await;
            }
            // An install puts a whole state in a copy; a report or a fetch takes nothing in.
            let installs =
              !slates_db::live::is_report(&request) && !slates_db::live::is_fetch(&request);
            let object = installs
              .then(|| slates_db::live::object_of_state(&request))
              .flatten();
            let admission = crate::live_ship::admit_claim(control, local, peer_host, object).await;
            match admission.refusal() {
              Some(refusal) => refusal,
              None => {
                state::with_state(|s| crate::live_ship::serve_state(s, local, peer_host, &request))
                  .unwrap_or_default()
              }
            }
          }
          crate::mirror::MIRROR_RECORD_STREAM => state::with_state(|s| {
            let peer_host = s
              .learned_members
              .get(&peer_anchor)
              .map_or(seed, |learned| learned.host);
            crate::mirror::serve(s, local, peer_host, &request)
          })
          .unwrap_or_default(),
          crate::takeover::TAKEOVER_STREAM => state::with_state(|s| {
            let peer_host = s
              .learned_members
              .get(&peer_anchor)
              .map_or(seed, |learned| learned.host);
            crate::takeover::serve_host_prepare(s, local, peer_host, &request)
          })
          .unwrap_or_default(),
          crate::takeover::TAKEOVER_LEDGER_STREAM => state::with_state(|s| {
            let peer_host = s
              .learned_members
              .get(&peer_anchor)
              .map_or(seed, |learned| learned.host);
            crate::takeover::serve_ledger_prepare(s, local, peer_host, &request)
          })
          .unwrap_or_default(),
          // A moved volume's new owner asks its cohort by name (A-291).
          crate::moves::MOVE_STREAM => state::with_state(|s| {
            let peer_host = s
              .learned_members
              .get(&peer_anchor)
              .map_or(seed, |learned| learned.host);
            crate::moves::serve_move_prepare(s, local, peer_host, &request)
          })
          .unwrap_or_default(),
          // A window homed by its identity (A-724 piece 6, `slates_cluster::homes`): kept on this node's home shard
          // for it, apart from the content plane's holds, and answered once its image is published.
          slates_cluster::homes::HOME_STREAM => {
            crate::vcs_store::homes::serve_remote(
              control,
              &request,
              crate::daemon::LIVENESS_BUDGET_NS,
            )
            .await
          }
          // A credential enrolled or revoked elsewhere in the region (A-764, `slates_cluster::credentials`): applied to
          // the partition its id routes to and answered whether it is held as asked.
          slates_cluster::credentials::CREDENTIAL_STREAM => {
            crate::credentials::serve_remote(control, &request, crate::daemon::LIVENESS_BUDGET_NS)
              .await
          }
          stream if is_content_stream(stream) => {
            let reply = state::with_state(|s| {
              let peer_host = s
                .learned_members
                .get(&peer_anchor)
                .map_or(seed, |learned| learned.host);
              serve_content_request(s, local, peer_host, stream, &request)
            })
            .unwrap_or_default();
            injected_content_ack_delay(&reply).await;
            reply
          }
          // A green's merge record with the inputs it names while this holder lacks them (§4.16 "Apply on holders",
          // "Commit" in one exchange a version): the inputs are held, then the record recomputed and accepted.
          crate::merge_service::MERGE_RECORD_STREAM => state::with_state(|s| {
            let peer_host = s
              .learned_members
              .get(&peer_anchor)
              .map_or(seed, |learned| learned.host);
            serve_merge_shipment(s, local, peer_host, &request)
          })
          .unwrap_or_default(),
          CONFIG_STREAM => state::with_state(|s| {
            serve_council(
              s,
              s.learned_members
                .get(&peer_anchor)
                .map_or(seed, |member| member.host),
              &request,
            )
          })
          .unwrap_or_default(),
          CONFIG_FETCH_STREAM => {
            state::with_state(|s| serve_config_fetch(s, &request)).unwrap_or_default()
          }
          RECIPIENT_STREAM => {
            state::with_state(|s| crate::seal_keys::node_recipient_public(s)).unwrap_or_default()
          }
          PAIR_KEY_STREAM => state::with_state(|s| accept_pair_delivery(s, peer_anchor, &request))
            .unwrap_or_default(),
          LINEAGE_STREAM => state::with_state(|s| {
            let peer_host = s
              .learned_members
              .get(&peer_anchor)
              .map_or(seed, |member| member.host);
            serve_lineage(s, peer_host, &request)
          })
          .unwrap_or_default(),
          DEATH_STREAM => state::with_state(|s| {
            let peer_host = s
              .learned_members
              .get(&peer_anchor)
              .map_or(seed, |member| member.host);
            serve_death_report(s, peer_host, &request)
          })
          .unwrap_or_default(),
          REPORT_STREAM => state::with_state(|s| {
            let peer_host = s
              .learned_members
              .get(&peer_anchor)
              .map_or(seed, |member| member.host);
            serve_council_report(s, peer_host, &request)
          })
          .unwrap_or_default(),
          ROOT_STREAM => state::with_state(|s| {
            serve_root(
              s,
              s.learned_members
                .get(&peer_anchor)
                .map_or(seed, |member| member.host),
              &request,
            )
          })
          .unwrap_or_default(),
          ROOT_FETCH_STREAM => {
            state::with_state(|s| serve_root_fetch(s, &request)).unwrap_or_default()
          }
          crate::owner_location::STREAM => {
            injected_serve_delay().await;
            state::with_state(|state| {
              let peer_host = state
                .learned_members
                .get(&peer_anchor)
                .map_or(seed, |member| member.host);
              crate::owner_location::answer(state, peer_host, &request)
            })
            .unwrap_or_default()
          }
          _ => Vec::new(),
        }
      }
    }
    .await;
    let served = endpoint.reply(stream_id, reply);
    // The sender's state on the connection the peer dialed: where its forwards and reads come back from this node.
    let seen = crate::state::RecordLink::seen(peer_anchor, &endpoint);
    state::with_state(|s| s.served_sessions.insert(peer_anchor.0, seen));
    if served.is_err() {
      return;
    }
  }
}

/// Test support: while a fault holds this node's live-state replies (`ShardState::live_state_withhold_replies`, never
/// reachable from the wire), a reply on that kind waits after its request arrived, so a takeover's question to this node
/// misses its budget while the probe plane still answers.
async fn withheld_live_state() {
  while state::with_state(|state| state.live_state_withhold_replies).unwrap_or(false) {
    crate::daemon::pace(HEARTBEAT_NS / POLL_PER_PERIOD).await;
  }
}

/// The status refusal count under which a holder records a register record it refused because a test told
/// it to refuse that owner's records ([`crate::daemon::Daemon::inject_record_refusal`]); never in production.
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
const RECORD_REFUSED_BY_FAULT: &str = "fleet.record.refused_by_fault";

/// Brings every held acceptor's authority into step with the routing view (§4.8 "distributing the
/// taken-over authority to the holders"): for each object this node holds a copy of, installs the object's
/// current owner (as the routing view records it) under the configuration generation. On a takeover the
/// routing owner moved to the survivor rendezvous ranks first, so this installs the successor as the
/// authorized owner — the object's new owner can then commit through this hold and its prepare authorizes.
/// Every survivor computes the same routing winner, so this needs no coordination. Idempotent: an install
/// of the same owner under an equal-or-higher generation is accepted, a lower one refused (ignored), so the
/// authority only advances. An install walks the copies a slice a turn instead ([`install_on_copy`], A-398); this whole
/// pass is the walk's oracle in the daemon's unit tests.
#[cfg(test)]
pub(crate) fn reconcile_held_authority(state: &mut ShardState) {
  let generation = state.fleet.configuration().version;
  let retired = retired_hosts(state);
  let held: Vec<ObjectId> = state.holder_records.keys().copied().collect();
  for object in held {
    reconcile_held_copy(state, object, generation, &retired);
  }
}

/// The hosts the council's configuration keeps a retirement of.
#[cfg(test)]
fn retired_hosts(state: &ShardState) -> std::collections::BTreeSet<HostId> {
  state
    .council
    .configuration()
    .retired
    .keys()
    .copied()
    .collect()
}

/// [`reconcile_held_authority`] for one held copy, `object`'s: its authority moved to the owner this node's routing
/// names, at `generation`. A move away from a retired owner is recorded with the copy (A-251): once the authority names
/// the successor, only that record says the copy was the retired owner's, which a takeover round's listing needs.
fn reconcile_held_copy(
  state: &mut ShardState,
  object: ObjectId,
  generation: u64,
  retired: &std::collections::BTreeSet<HostId>,
) {
  if let Some(owner) = state.fleet.object_owner(object)
    && let Some(acceptor) = state.holder_records.get_mut(&object)
  {
    let from_retired = retired.contains(&acceptor.owner());
    let _ = acceptor.move_authority(Authority { generation, owner }, from_retired);
  }
}

/// Accepts one register record this node holds as a **candidate holder** for the peer that owns it, into
/// the object's durable acceptor in the shard state (§4.8 "records are sent to all candidates"). The
/// acceptor is created on the object's first record under the authority `{owner: peer, generation: the
/// configuration version}` — `peer` is the socket's TLS-authenticated identity, so a record whose owner
/// field is not this socket's peer is refused [`Unauthorized`](slates_db::register::RegisterError) by
/// [`Acceptor::accept`], and one under a foreign generation or a stale epoch is refused likewise. On
/// acceptance the object is tracked in the routing view as backed for that owner, so the owner's death
/// hands the membership fold's takeover computation this object. Returns the binding acknowledgement's bytes; a
/// sender on an *older* configuration is refused `ConfigurationStale` carrying this node's newer version so
/// it refreshes and retries (§4.8 the piggyback rule), and every other refusal is an empty reply the owner
/// counts as no acknowledgement. A record naming a *newer* configuration than this node's flags a reactive
/// refresh (this node is the one behind) before it is refused.
pub(crate) fn accept_held_record(
  state: &mut ShardState,
  local: HostId,
  peer_host: HostId,
  record: &Record,
) -> Vec<u8> {
  // A test's injected refusal: this holder behaves as one that never received the owner's records.
  if state.record_refused_from.contains(&peer_host)
    || (state.merge.fault.refuse_work_catalogs
      && crate::catalog::CatalogValue::from_record_bytes(&record.value)
        .is_some_and(|catalog| catalog.work_of.is_some()))
  {
    state.count(RECORD_REFUSED_BY_FAULT, 1);
    return Vec::new();
  }
  if let Err(error) = check_held_record(state, local, peer_host, record) {
    count_record_refusal(state, &error);
    return encode_refusal(&error);
  }
  hold_checked_record(state, local, peer_host, record)
}

/// The status count of held records accepted into their acceptor whose owner the routing view refused to track (not a
/// member of the council's configuration), so no acknowledgement was given. Format: a refusal name in the daemon's
/// status report.
const RECORD_REFUSED_UNTRACKED: &str = "fleet.record.refused.untracked";

/// Counts a held record this holder refused, by its register refusal kind (`fleet.record.refused.<kind>`), so a sender
/// whose commit never places shows on the holder why: before these, a holder restarted warm refused a successor's
/// adoption `ForeignGeneration` every period without a trace. Format: refusal names in the daemon's status report.
fn count_record_refusal(state: &mut ShardState, error: &RegisterError) {
  let kind = match error {
    RegisterError::StaleEpoch { .. } => "fleet.record.refused.stale_epoch",
    RegisterError::ConfigurationStale { .. } => "fleet.record.refused.configuration_stale",
    RegisterError::Unsupported { .. } => "fleet.record.refused.unsupported",
    RegisterError::MalformedRecord => "fleet.record.refused.malformed",
    RegisterError::Unauthorized => "fleet.record.refused.unauthorized",
    RegisterError::ForeignGeneration { .. } => "fleet.record.refused.foreign_generation",
    RegisterError::ConflictingPosition => "fleet.record.refused.conflicting_position",
  };
  state.count(kind, 1);
}

/// The status count of held records accepted but not acknowledged because the shard's publish carrying them was
/// refused (§4.8 persistence before reply). Format: a refusal name in the daemon's status report.
const RECORD_UNPUBLISHED: &str = "fleet.record.unpublished";

/// The status count of held records accepted in place of a different value held at their position, a lower epoch's,
/// which never committed (A-373): what a green's replica built over the replaced value follows. Format: a refusal name
/// in the daemon's status report.
const RECORD_REPLACED: &str = "fleet.record.replaced";

/// Holds `record` from `peer_host` once its authority was checked ([`check_held_record`]): the acceptance into this
/// holder's register for the object, the routing it learns, and the acknowledgement or the refusal's reply. A record
/// that replaces a different value held at its position (a higher epoch's, A-373) rewinds this holder's replica of the
/// green it belongs to past the value it replaced (`merge_origin::rewind_past_replaced`).
pub(crate) fn hold_checked_record(
  state: &mut ShardState,
  local: HostId,
  peer_host: HostId,
  record: &Record,
) -> Vec<u8> {
  let generation = state.fleet.configuration().version;
  let replaces = state
    .holder_records
    .get(&record.object)
    .and_then(|acceptor| acceptor.value_at(record.object, record.sequence))
    .is_some_and(|held| held != record.value.as_slice());
  let accepted = match state.holder_records.get_mut(&record.object) {
    Some(acceptor) => acceptor.accept(record),
    None => {
      // The object's first record: the acceptor exists only once it has **accepted** one. A refused first
      // record — this holder has not yet installed the generation the record names (the owner installed the
      // committed configuration a heartbeat before this holder did), or it is unauthorized — leaves no
      // acceptor behind. Creating one anyway pinned it at this holder's *stale* generation: the routing view
      // learns an object only on acceptance, so `reconcile_held_authority` never raised that acceptor at the
      // install, and it refused every re-ship of the head `ForeignGeneration` for good — a volume provisioned
      // in the window between the owner's install and the holder's never placed
      // (`docs/bugs/2026-09-13-raft-voter-set-never-shrinks.md`, sibling). With no acceptor left behind, the
      // owner's next re-ship after the install creates it at the current generation and is accepted.
      // A holder's acceptor keeps what changes of its positions (A-367): its shard's recovery image carries each
      // position as its own piece, so a publication carries the records accepted since the last one.
      let mut acceptor = Acceptor::new(
        local,
        Authority {
          generation,
          owner: peer_host,
        },
      )
      .tracking_changes();
      let result = acceptor.accept(record);
      if result.is_ok() {
        state.holder_records.insert(record.object, acceptor);
      }
      result
    }
  };
  match accepted {
    Ok(ack) => {
      // A record accepted under the current configuration comes from a member, so never from the object's
      // departed owner: the object has an authorized live owner again, and the promotion gate this holder
      // kept for it has served its purpose.
      state.departed_owners.remove(&record.object);
      // The register holds the new value from here, acknowledged or not: a replica built over the one it replaced
      // follows it now.
      if replaces {
        state.count(RECORD_REPLACED, 1);
        crate::merge_origin::rewind_past_replaced(state, record);
      }
      match state
        .fleet
        .track_object(record.object, peer_host, state.council.configuration())
      {
        Ok(()) => {
          after_held_record(state, record);
          // The acknowledgement stands behind the shard's publish (§4.8 persistence before reply): a warm restart
          // keeps this node's member id, so a record it acknowledged must be held again after it. A refused
          // publish answers no acknowledgement, and the record rides the next publish that commits.
          if crate::verbs::publish_shard(state).is_err() {
            state.count(RECORD_UNPUBLISHED, 1);
            return Vec::new();
          }
          ack.encode()
        }
        Err(error) => {
          state.count(RECORD_REFUSED_UNTRACKED, 1);
          encode_refusal(&error)
        }
      }
    }
    // A refusal the sender acts on rides back on the wire (a `ConfigurationStale` naming this node's newer
    // version, so a stale sender refreshes and retries); every other refusal is an empty reply the sender
    // counts as no acknowledgement ([`encode_refusal`]).
    Err(error) => {
      count_record_refusal(state, &error);
      encode_refusal(&error)
    }
  }
}

/// What a holder does once `record` is durably accepted (AUD-29-43): a single-value register (a head, a
/// catalog, a tombstone — phase one adopts only its newest position) keeps only positions at or above the
/// accepted one, so a holder's records stay one per register however many seals a volume takes; and a
/// tombstone releases the destroyed volume's content, or at its retirement the volume's records
/// ([`crate::tombstone::on_held`]).
fn after_held_record(state: &mut ShardState, record: &Record) {
  // A state log's part is a ledger position of a repository's own state: nothing to compact, no content to judge, and
  // never a tombstone (A-636 part 2b).
  if let RegisterClass::State(_) = record.object.class() {
    return;
  }
  if crate::catalog::is_single_value(&record.value)
    && let Some(acceptor) = state.holder_records.get_mut(&record.object)
  {
    acceptor.compact_below(record.object, record.sequence);
  }
  if let Some(value) = crate::tombstone::TombstoneValue::from_record_bytes(&record.value) {
    crate::tombstone::on_held(state, record.object, value);
    // A destroyed volume's live log goes with it (A-200): no owner ships to it again.
    crate::live_ship::let_go(state, record.object);
  } else {
    // A newer head or ledger position may supersede content held for the object (AUD-29-43).
    crate::content_retention::retain_object(state, record.object);
  }
}

/// Serves one content request from `peer_host` on this holder (§4.10): the content plane's own stream, or a frame of
/// a merge shipment that carries a record's inputs ([`serve_merge_shipment`]), through the one path.
fn serve_content_request(
  s: &mut ShardState,
  local: HostId,
  peer_host: HostId,
  stream: u64,
  request: &[u8],
) -> Vec<u8> {
  // A test's injected placement refusal (§4.16 placed-before-reference): a holder that refuses every content
  // placement — its offers and its chunks, since a complete stage acknowledges at the offer (AUD-29-55) — counted,
  // so an owner's record is shown to wait on it.
  if s.merge.fault.refuse_content_puts
    && (stream == CONTENT_OFFER_STREAM || stream == CONTENT_CHUNK_STREAM)
  {
    s.count(crate::merge_service::CONTENT_PUT_REFUSED, 1);
    return Vec::new();
  }
  // Authority for the object the request names is decided before any lookup (AUD-29-45), from the committed
  // configuration and this holder's records — never from the content hash.
  crate::content_holder::serve(
    s,
    local,
    request,
    |regional, records, mirror, access, object| {
      content_authorized(regional, records, local, peer_host, access, object)
        || mirror.admits_put(peer_host, access, object)
    },
  )
}

/// Serves a merge shipment (§4.16 "Commit" in one exchange a version): its content frames — the offer of the record's
/// inputs, then their chunks — through the content plane's own path, then the record. The record is accepted only if
/// its inputs are held by then (`merge_service::accept_merge_record`), so a refused or partial placement answers no
/// acknowledgement and the owner ships again. A record not accepted is answered with what this holder holds of its
/// inputs (A-286): a content acknowledgement bound to them when it holds them whole (`merge_service::inputs_held`), to
/// a peer that may place them here; else the content plane's answer to the shipment's offer asked again after its
/// chunks, the chunks it still lacks; else nothing. So inputs larger than one shipment cross in as many as they take,
/// each carrying only what this holder lacks. A record that replaces a different value held at its position (a
/// higher epoch's, A-373) is held before its content frames, whose inputs the door admits only once a held record names
/// them (`merge_service::hold_replacing_record`).
pub(crate) fn serve_merge_shipment(
  s: &mut ShardState,
  local: HostId,
  peer_host: HostId,
  request: &[u8],
) -> Vec<u8> {
  let Some(frames) = crate::merge_service::decode_shipment(request) else {
    return Vec::new();
  };
  let Some((record, content)) = frames.split_first() else {
    return Vec::new();
  };
  let Ok(record) = Record::decode(record) else {
    return Vec::new();
  };
  crate::merge_service::hold_replacing_record(s, local, peer_host, &record);
  if !content.is_empty() {
    s.count(crate::merge_service::INPUTS_CARRIED, 1);
  }
  for (index, frame) in content.iter().enumerate() {
    let stream = if index == 0 {
      CONTENT_OFFER_STREAM
    } else {
      CONTENT_CHUNK_STREAM
    };
    let _ = serve_content_request(s, local, peer_host, stream, frame);
  }
  let accepted = crate::merge_service::accept_merge_record(s, local, peer_host, &record);
  if !accepted.is_empty() {
    return accepted;
  }
  let may_place = content_authorized(
    s.council.configuration(),
    s.holder_records.as_map(),
    local,
    peer_host,
    slates_cluster::content::ContentAccess::Place,
    record.object,
  );
  if may_place && let Some(held) = crate::merge_service::inputs_held(s, local, &record) {
    return held;
  }
  match content.first() {
    Some(offer) => serve_content_request(s, local, peer_host, CONTENT_OFFER_STREAM, offer),
    None => Vec::new(),
  }
}

/// Validates a record against both the transport principal and installed authority before any effect
/// (§4.8, §4.16; AUD-10/AUD-12). The first record and all later records have the same peer binding.
/// A merge holder calls this before recomputing, then accepts in the same synchronous shard turn.
/// Whether `peer` may place (offer, put) or read (fetch) `object`'s content on this holder (§4.13 "Content
/// identity and sharing"; AUD-29-45). A content hash is an identity, never a bearer right: the peer must be a
/// member acting for the object — its current owner, or, once that owner departed, its takeover successor —
/// and a placement must land on one of the acting owner's candidates for the object. A read is also allowed
/// to the object's candidate holders (a re-replication among them). The owner is the one this holder's
/// records name, else the object's creator (content is placed before its first record, §4.16).
pub(crate) fn content_authorized(
  regional: &slates_db::register::RegionalConfiguration,
  records: &std::collections::BTreeMap<ObjectId, slates_db::register::Acceptor>,
  local: HostId,
  peer: HostId,
  access: slates_cluster::content::ContentAccess,
  object: ObjectId,
) -> bool {
  content_authority(regional, records, local, peer, access, object).is_ok()
}

/// [`content_authorized`], with the counter naming why not: `peer` is not a member, the object's acting owner is
/// unknown here, `peer` is not it, or (placing) this node is not among its candidates.
/// Counter and reason: a placement refused because this node is not among the object's candidates in its own
/// configuration, the sender being the acting owner ([`content_authority`]); a live holder the volume's record names
/// admits the entry anyway (`live_ship::admitted`).
pub(crate) const CONTENT_NOT_A_CANDIDATE: &str = "content.refused.not_a_candidate";

pub(crate) fn content_authority(
  regional: &slates_db::register::RegionalConfiguration,
  records: &std::collections::BTreeMap<ObjectId, slates_db::register::Acceptor>,
  local: HostId,
  peer: HostId,
  access: slates_cluster::content::ContentAccess,
  object: ObjectId,
) -> Result<(), &'static str> {
  if !regional.members.contains(&peer) {
    return Err("content.refused.not_a_member");
  }
  let owner = records
    .get(&object)
    .map_or_else(|| object.creator(), slates_db::register::Acceptor::owner);
  let acting = if regional.members.contains(&owner) {
    Some(owner)
  } else {
    regional.successor(owner, object)
  };
  let Some(acting) = acting else {
    return Err("content.refused.no_acting_owner");
  };
  let candidates = regional
    .configuration_for(acting)
    .map(|configuration| configuration.place(object).candidates)
    .unwrap_or_default();
  match access {
    slates_cluster::content::ContentAccess::Place if peer != acting => {
      Err("content.refused.not_the_acting_owner")
    }
    slates_cluster::content::ContentAccess::Place if !candidates.contains(&local) => {
      Err(CONTENT_NOT_A_CANDIDATE)
    }
    slates_cluster::content::ContentAccess::Place => Ok(()),
    slates_cluster::content::ContentAccess::Read => {
      let allowed = peer == acting
        || candidates.contains(&peer)
        || regional
          .recovery_cohorts(owner, object)
          .iter()
          .any(|cohort| cohort.contains(&peer));
      if allowed {
        Ok(())
      } else {
        Err("content.refused.not_a_reader")
      }
    }
  }
}

pub(crate) fn check_held_record(
  state: &mut ShardState,
  local: HostId,
  peer_host: HostId,
  record: &Record,
) -> Result<(), RegisterError> {
  if record.owner != peer_host || !state.council.configuration().members.contains(&peer_host) {
    return Err(RegisterError::Unauthorized);
  }
  // The authenticated owner knows a newer configuration (§4.8 piggyback). Refuse this attempt, and
  // fetch that committed configuration before its retry; a forged owner cannot trigger this work.
  if record.generation > state.council.configuration().version {
    state.config_refresh_wanted = true;
    return Err(RegisterError::ForeignGeneration {
      current: state.council.configuration().version,
    });
  }
  // The placement remembered on acceptance must be the one this record names. The council can
  // advance before the coordinator installs it into FleetNode; refuse that gap and let the sender retry.
  if record.generation != state.council.configuration().version {
    return Err(RegisterError::ConfigurationStale {
      version: state.council.configuration().version,
    });
  }
  match state.holder_records.get(&record.object) {
    Some(acceptor) => acceptor.check(record),
    None => Acceptor::new(
      local,
      Authority {
        generation: state.fleet.configuration().version,
        owner: peer_host,
      },
    )
    .check(record),
  }?;
  // A state log's part is held only where it continues this holder's copy of its log (A-636 part 2b).
  if let RegisterClass::State(log) = record.object.class() {
    crate::state_log::check_held(state, record, log)?;
  }
  Ok(())
}

/// A volume head this node owns that is not yet held by every candidate, with everything the register commit
/// needs and the candidates that already acknowledged it (so the coordinator ships only to the rest).
struct Head {
  /// The shard that owns the volume — where its placement is recorded.
  shard: u16,
  object: ObjectId,
  record: Record,
  /// The placement the head commits against: its candidates — the settled and the current cohorts joined
  /// while this owner's neighbourhood change is in flight — with nothing acknowledged.
  shape: Placement,
  acked: Vec<HostId>,
  quorum: Quorum,
  /// Whether its shard ships its volume live (`live_ship::ships`): then a held durable reply may hold the candidates to
  /// an answer, and [`ship_head`] notes the ask on that shard before its round (A-301).
  live: bool,
}

/// The volume heads this node owns that some candidate has **not yet acknowledged** (§4.8 "records are sent
/// to **all** candidates; committed at `f + 1`"): for each volume whose head at its current sequence still
/// has a remote candidate holder missing it, the record to commit, the full candidate set, the candidates
/// that already hold it, and the quorum the configuration computes. Read under `with_state`.
///
/// The head's sequence is the volume's epoch — 0 at creation (the epoch-one head, naming no content), and
/// the snapshot's epoch once it is sealed — and its value ([`HeadValue`]) names the snapshot's content and
/// the holders that acknowledged it. A sealed head is shipped only once its content is placed ("no head
/// record names content that is not placed", AC-8.2), so until then the volume's head stays at what was
/// last placed; [`head_value_of`] is that gate.
///
/// The gate is per-holder (a candidate has not acked), **not** the object's overall quorum: a head reaches
/// `f + 1` acknowledgements (region-placed, durable) from *some* candidates, but every candidate must still
/// receive it, because after a death the surviving candidates form the promotion quorum a takeover needs —
/// each must hold the head. Gating on the quorum alone would let a head reach only its first `f + 1` holders,
/// so a co-survivor never got it and could not promise for the promotion. The owner's own hold (`local`) is
/// implicit (it commits through its own acceptor), so a head all of whose *remote* candidates hold it is done.
fn unplaced_heads(state: &ShardState, local: HostId) -> Vec<Head> {
  let config = state.fleet.configuration();
  let mut heads = Vec::new();
  for (_, slot) in state.volumes.iter() {
    let Some(record) = state.db.partition().volume(slot.id) else {
      continue;
    };
    // A volume this node lost to a successor (yielded at its re-admission, A-334, or its log fenced, A-338) ships no
    // record (A-382): its successor owns its registers, so each record here is stale, and a holder's copy made afresh
    // (a re-admitted holder lets its own go, `readmission::let_go_own_registers`) takes the first member's record it
    // meets as its owner's. Shipped, a re-admitted creator's stale catalog could reach such a copy before the
    // successor's, and every holder refused the rest of them each period (`fleet.record.refused.unauthorized`, 218 in
    // one nemesis run).
    if crate::verbs::lost_to_a_successor(state, slot.id) {
      continue;
    }
    let object = ObjectId(slot.id.bytes);
    // A volume being destroyed ships its tombstone, never its catalog or a head (AUD-29-43): its registers
    // close at the sequence the destroy's record will keep, so the tombstone is continuous with it.
    if record.state == VolumeState::Destroying {
      if let Some(sequence) = state.db.partition().tombstone_sequence(slot.id) {
        let tombstone = Tombstone {
          volume: slot.id,
          sequence,
        };
        heads.extend(owed_tombstone(state, config, tombstone, local));
      }
      continue;
    }
    // The catalog register first (AUD-29-17): the head waits until the catalog's current version is committed.
    let (catalog_owed, catalog_ready) = owed_catalog(state, config, record, local);
    heads.extend(catalog_owed);
    if !catalog_ready {
      continue;
    }
    let Some((sequence, value)) = head_value_of(state, record, object, config.quorum) else {
      continue; // The newest seal's content is not yet placed: the head waits for it.
    };
    if crate::carry::holds_head_back(state, record.id, record.head, &value) {
      continue; // A captured clone's head waits for its capture's record (A-544), so a successor adopts both.
    }
    if !sealing_covers_candidates(state, config, object, &value, local) {
      continue; // A candidate could not open the sealed content as a successor: the head waits for its pair.
    }
    heads.extend(owed_record(
      state,
      config,
      (object, sequence),
      value.to_record_bytes(),
      local,
    ));
  }
  heads.extend(owed_merge_catalogs(state, config, local));
  heads.extend(owed_adopted_work_catalogs(state, config, local));
  for tombstone in state.db.partition().tombstones() {
    heads.extend(owed_tombstone(state, config, tombstone, local));
  }
  heads.extend(owed_state_log_parts(state, config, local));
  heads.extend(owed_repositories(state, config, local));
  heads
}

/// The repositories' own registers this owner shard still owes some candidate (A-636 part 2c-1): each repository's
/// record (`crate::catalog::repository_value`) at its sequence (`crate::catalog::repository_sequence`), to the
/// candidates its family places it on, its state logs' and its greens' (A-636 part 2b-2), as a volume's catalog ships to
/// its volume's. A repository this node lost to a successor ships nothing, as a lost volume does: its successor owns its
/// registers.
fn owed_repositories(
  state: &ShardState,
  config: &slates_db::register::Configuration,
  local: HostId,
) -> Vec<Head> {
  let repositories = state.db.partition().repositories();
  repositories
    .repositories()
    .filter(|record| {
      !crate::verbs::lost_to_a_successor(
        state,
        DbVolumeId {
          bytes: record.id.bytes,
        },
      )
    })
    .filter_map(|record| {
      let names = repositories
        .log_position(record.id, slates_db::catalog::StateLog::Names)
        .unwrap_or(0);
      owed_record(
        state,
        config,
        (
          ObjectId(record.id.bytes),
          crate::catalog::repository_sequence(record, names),
        ),
        crate::catalog::repository_value(record),
        local,
      )
    })
    .collect()
}

/// The parts of the repositories' state logs this owner shard still owes some candidate (A-636 part 2b;
/// `crate::state_log`), each log's in position order, so a holder meets each part after its predecessor: a log's
/// register at each part's position, to the log's candidates (its repository's), with the candidates that hold it
/// already (their highest acknowledgement at or past it). A part every remote candidate holds is let go when its last
/// acknowledgement comes, so none is owed here.
fn owed_state_log_parts(
  state: &ShardState,
  config: &slates_db::register::Configuration,
  local: HostId,
) -> Vec<Head> {
  let mut heads = Vec::new();
  for (register, parts, acked) in state.state_logs.owed() {
    let placement = config.place(register);
    for (sequence, value) in parts {
      let holding: Vec<HostId> = placement
        .candidates
        .iter()
        .copied()
        .filter(|candidate| acked.get(candidate).is_some_and(|held| held >= sequence))
        .collect();
      let owed = placement
        .candidates
        .iter()
        .any(|candidate| *candidate != local && !holding.contains(candidate));
      if !owed {
        continue;
      }
      heads.push(Head {
        shard: state.shard,
        object: register,
        record: Record {
          owner: local,
          object: register,
          sequence: *sequence,
          epoch: config.host_epoch,
          generation: config.version,
          value: value.clone(),
        },
        shape: Placement {
          acked: Vec::new(),
          ..placement.clone()
        },
        acked: holding,
        quorum: config.quorum,
        live: false,
      });
    }
  }
  heads
}

/// The catalog registers of this shard's merge volumes, which have no store slot, as this owner still owes them.
/// - A green's (AUD-29-17): its access list is what a successor admits by, and the merge plane ships no record of the
///   green before the register's current version is committed (`catalog_committed`).
/// - A work's (A-255): a work's head is never placed, so its catalog is the only record of it a successor holds, and
///   it names the work's green. A resend that names the work reaches the work's successor, which sends it on to the
///   green's owner, where the submit's completion is (in its merge record).
fn owed_merge_catalogs(
  state: &ShardState,
  config: &slates_db::register::Configuration,
  local: HostId,
) -> Vec<Head> {
  state
    .greens
    .keys()
    .chain(state.works.keys())
    .filter_map(|id| state.db.partition().volume(*id))
    .filter_map(|record| owed_catalog(state, config, record, local).0)
    .collect()
}

/// The catalog registers of works this node adopted at a takeover, as it still owes them to some remote candidate
/// (A-255). After its owner's takeover a work has no owner and is never materialized, so its catalog record, which names
/// its green, is the only route a resend of its submit has, and the successor that adopted it is that record's owner. It
/// keeps the record placed as any owner keeps its records, under its own placement at its configuration's epoch: a
/// second failure among the record's holders, within the fault budget, then leaves it held. Kept until the work's
/// tombstone replaces it (`crate::tombstone`).
fn owed_adopted_work_catalogs(
  state: &ShardState,
  config: &slates_db::register::Configuration,
  local: HostId,
) -> Vec<Head> {
  state
    .pending_catalogs
    .iter()
    .filter(|(_, (_, catalog))| catalog.work_of.is_some())
    .map(|(key, (sequence, catalog))| (key.catalog(), *sequence, catalog))
    .filter(|(object, _, _)| state.fleet.object_owner(*object) == Some(local))
    .filter_map(|(object, sequence, catalog)| {
      owed_record(
        state,
        config,
        (object, sequence),
        catalog.to_record_bytes(),
        local,
      )
    })
    .collect()
}

/// The catalog register of `record` (AUD-29-17) as this owner still owes it to some remote candidate, and whether
/// its current version is ready to be built on: owed to nobody, or committed at the quorum. The register is what a
/// successor serves the volume or green as — its owner, policy and access list — shipped at every catalog change,
/// and a record that names the object's later state (a volume's head, a green's merge record) waits until it is
/// ready, so an adopted record always meets an adopted catalog as new as its own object.
fn owed_catalog(
  state: &ShardState,
  config: &slates_db::register::Configuration,
  record: &VolumeRecord,
  local: HostId,
) -> (Option<Head>, bool) {
  let catalog = ObjectId(record.id.bytes).catalog();
  let owed = owed_record(
    state,
    config,
    (catalog, record.catalog_version),
    crate::catalog::CatalogValue::of(
      record,
      state
        .db
        .partition()
        .carried(record.id)
        .map(|(carried, _)| carried),
    )
    .to_record_bytes(),
    local,
  );
  let committed = state.placed_heads.get(&catalog).is_some_and(|head| {
    head.sequence == record.catalog_version && head.placed_under(config, catalog)
  });
  let ready = owed.is_none() || committed;
  (owed, ready)
}

/// A catalog register this owner still owes some candidate, for the merge plane to ship on demand (A-255): a version's
/// merge record waits for its submitting work's catalog ([`catalog_committed`]), and catalogs otherwise ship on the
/// coordinator's period, so a submit made right after its work was created waited out a whole period (median 109.2 ms
/// against 1.24 ms, twenty submits each, an idle Mac Studio, 2026-11-09).
pub(crate) struct OwedCatalog(Head);

/// The catalog register work `work` still owes some candidate, read on its owner shard; `None` when every candidate
/// holds its current version or the work has no record here.
pub(crate) fn owed_work_catalog(
  state: &ShardState,
  work: DbVolumeId,
  local: HostId,
) -> Option<OwedCatalog> {
  let record = state.db.partition().volume(work)?;
  owed_catalog(state, state.fleet.configuration(), record, local)
    .0
    .map(OwedCatalog)
}

/// Ships an owed catalog register as the period's head shipment does ([`ship_head`]): to every candidate still missing
/// it, committed at `f + 1`, its placement recorded on its owner shard.
pub(crate) async fn ship_owed_catalog(
  origin: u16,
  owner_acceptor: &mut Acceptor,
  local: HostId,
  catalog: &OwedCatalog,
  budget: CommitBudget,
) -> HeadShipped {
  ship_head(origin, owner_acceptor, local, &catalog.0, budget).await
}

/// What shipping a head did ([`ship_head`]): the dispatch whose stragglers are still in flight, when anything was sent,
/// and whether a placement came back and was recorded on the head's owner shard. Only that outcome gives the record
/// plane new work at once (a version that waited for a work's catalog, A-255); a ship that found no session to a
/// candidate, or that the runtime refused, sent nothing and is shipped again at the coordinator's next period.
pub(crate) struct HeadShipped {
  /// The dispatch, when anything was sent.
  pub(crate) dispatch: Option<Dispatch>,
  /// Whether the commit's placement was recorded on the head's owner shard.
  pub(crate) recorded: bool,
}

/// Whether the merge volume `id`'s catalog register is committed, so a merge record that depends on it may ship (see
/// [`owed_catalog`]):
/// - a green's (AUD-29-17): a successor materializes a green only with its adopted catalog, so a merge record that
///   could be adopted without one would leave the green unserved;
/// - a submitting work's (A-255): the work's catalog names its green, the only route a resend of the submit has to the
///   completion the record carries.
///
/// A volume with no record here owes no catalog: a destroyed volume's tombstone stands for both registers (AUD-29-43).
/// Read on the shard that holds the volume's record: a green's on its owner shard, and a work's there too when the work
/// shares its green's shard; a work on another shard (A-247) is read on its own shard before its verdict is asked
/// (A-298), so the owner's gate finds no record of it here and passes.
pub(crate) fn catalog_committed(state: &ShardState, id: DbVolumeId, local: HostId) -> bool {
  state
    .db
    .partition()
    .volume(id)
    .is_none_or(|record| owed_catalog(state, state.fleet.configuration(), record, local).1)
}

/// The stage of a destroyed volume's tombstone this owner still owes some candidate (AUD-29-43; see
/// `crate::tombstone`): the tombstone at its sequence until every remote candidate holds it, then the
/// retirement at the next sequence until every remote candidate holds that; `None` once both are held
/// everywhere (at once on a laptop, which has no remote candidate). The recorded placement tells the stages
/// apart: it moves to the retirement's sequence only once that is shipped.
fn owed_tombstone(
  state: &ShardState,
  config: &slates_db::register::Configuration,
  tombstone: Tombstone,
  local: HostId,
) -> Option<Head> {
  let object = ObjectId(tombstone.volume.bytes);
  let retirement = tombstone.sequence.checked_add(1)?;
  let retiring = state
    .placed_heads
    .get(&object)
    .is_some_and(|head| head.sequence >= retirement);
  if !retiring {
    let destroyed = crate::tombstone::TombstoneValue { retired: false };
    if let Some(head) = owed_record(
      state,
      config,
      (object, tombstone.sequence),
      destroyed.to_record_bytes(),
      local,
    ) {
      return Some(head);
    }
  }
  let retired = crate::tombstone::TombstoneValue { retired: true };
  owed_record(
    state,
    config,
    (object, retirement),
    retired.to_record_bytes(),
    local,
  )
}

/// Retires every tombstone this shard's partition owes no candidate any more (AUD-29-43): records
/// `Op::TombstoneRetired` — the tombstone gives back its volume slot — and forgets the placements the record
/// plane kept for the volume and its catalog. Returns the retired objects, so the owner's acceptor can forget
/// them too. Runs on the owner shard each record period and after every destroy, so on a laptop (no remote
/// candidate) a tombstone retires with its destroy. A refused record is counted and retried next time.
pub(crate) fn retire_done_tombstones(state: &mut ShardState, local: HostId) -> Vec<ObjectId> {
  let config = state.fleet.configuration().clone();
  let done: Vec<Tombstone> = state
    .db
    .partition()
    .tombstones()
    .filter(|tombstone| owed_tombstone(state, &config, *tombstone, local).is_none())
    .collect();
  let now = state.clock.monotonic_ns();
  let mut retired = Vec::new();
  for tombstone in done {
    let op = slates_db::op::Op::TombstoneRetired {
      id: tombstone.volume,
    };
    if state.db.mutate(&mut state.segment, &op, now).is_err() {
      count_refusal_in(state, TOMBSTONE_RETIRE_REFUSED);
      continue;
    }
    let object = ObjectId(tombstone.volume.bytes);
    state.placed_heads.remove(&object);
    state.placed_heads.remove(&object.catalog());
    retired.push(object);
  }
  retired
}

/// The status refusal count under which an owner records a retired tombstone its database refused to record
/// (AUD-29-43); it is retried at the next record period or destroy.
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
const TOMBSTONE_RETIRE_REFUSED: &str = "fleet.tombstone.retire_refused";

/// The promotion epoch `object`'s volume was taken over at, as the volume's record keeps it (A-325's
/// `VolumeRecord::taken_over`; `object` its head register or its catalog's): what this owner writes the volume's records
/// and ships its live log under once a warm restart has forgotten `placed_heads` (A-344). Before, a successor restarted
/// warm wrote them at its own host's epoch, below the fence its takeover had raised at every holder, so none placed and
/// its live log never shipped: its write was not answered within the client's 30 s deadline (a Mac Studio, 2026-11-18).
/// `None` for a volume this node did not take over.
pub(crate) fn promotion_epoch(state: &ShardState, object: ObjectId) -> Option<HostEpoch> {
  let volume = DbVolumeId {
    bytes: object.placement_key().0,
  };
  state
    .db
    .partition()
    .volume(volume)?
    .taken_over
    .map(|takeover| HostEpoch(takeover.epoch))
}

/// The record of `object` at `sequence` with `value`, as this owner still owes it to some remote candidate —
/// its full candidate set, the candidates that already hold it and the quorum — or `None` once every remote
/// candidate holds it. The record is written at the greater of the host's epoch, the epoch the object was
/// last written or adopted under, and the promotion epoch its volume's record keeps: a taken-over object's holders
/// fenced it at its promotion epoch (see `PlacedHead`), and a restart forgets `placed_heads` ([`promotion_epoch`]).
fn owed_record(
  state: &ShardState,
  config: &slates_db::register::Configuration,
  (object, sequence): (ObjectId, u64),
  value: Vec<u8>,
  local: HostId,
) -> Option<Head> {
  let placement = config.place(object);
  let recorded = state.placed_heads.get(&object);
  let acked = recorded
    .filter(|head| head.sequence == sequence)
    .map(|head| head.placement.acked.clone())
    .unwrap_or_default();
  let epoch = recorded
    .map_or(config.host_epoch, |head| head.epoch)
    .max(config.host_epoch)
    .max(promotion_epoch(state, object).unwrap_or(config.host_epoch));
  // Every remote candidate that has not acked still needs the record; when none remain it is done.
  let outstanding = placement
    .candidates
    .iter()
    .any(|candidate| *candidate != local && !acked.contains(candidate));
  if !outstanding {
    return None;
  }
  Some(Head {
    shard: state.shard,
    object,
    record: Record {
      owner: local,
      object,
      sequence,
      epoch,
      generation: config.version,
      value,
    },
    shape: Placement {
      acked: Vec::new(),
      ..placement
    },
    acked,
    quorum: config.quorum,
    live: crate::live_ship::ships(state, object),
  })
}

/// Whether a head may ship given what its sealing covers (A-92 pieces 3b and 4c), over the object's remote candidates
/// that are still members of the neighbourhood. A candidate the council has retired stays in the placement while the
/// owner's neighbourhood change is in flight (the settled and the current cohorts are joined), but it can never take
/// the volume over, so it needs no entry; requiring one held the head back from the new cohort, and the change could
/// never settle (`an_owner_settles_its_neighbourhood_only_once_its_head_is_placed_on_the_new_cohort`). See
/// [`sealing_covers`].
fn sealing_covers_candidates(
  state: &ShardState,
  config: &slates_db::register::Configuration,
  object: ObjectId,
  value: &HeadValue,
  local: HostId,
) -> bool {
  let placement = config.place(object);
  let remote = placement
    .candidates
    .iter()
    .copied()
    .filter(|candidate| *candidate != local && config.neighbourhood.contains(candidate));
  sealing_covers(state, object, value, remote)
}

/// Whether a head naming sealed content carries the lineage key for each of `holders` (A-92 pieces 3b and 4c). The
/// volume has a lineage key, so its archive is an envelope; any holder may become the successor (or a promoted mirror),
/// and one without its entry could never open the envelope, so the volume would stay unserved after a takeover. Such a
/// head waits until the owner shard has delivered a pair to every holder. Every holder at once, not each as it is
/// covered: a head's bytes at one sequence are one register value, and shipping it to some holders now and to the
/// rest later with more entries would put two values at one position. The wait is bounded: a pair is delivered once
/// per neighbour and kept, each record period, so only a member new to the neighbourhood, or one unreachable until
/// it is retired, holds a new sealed head back. A head naming no content, or plain content, needs no keys.
pub(crate) fn sealing_covers(
  state: &ShardState,
  object: ObjectId,
  value: &HeadValue,
  mut holders: impl Iterator<Item = HostId>,
) -> bool {
  if value.manifest.is_none()
    || !crate::seal_keys::lineage_recorded(state, DbVolumeId { bytes: object.0 })
  {
    return true;
  }
  let Some(sealing) = value.sealing.as_ref() else {
    return false;
  };
  holders.all(|holder| {
    state
      .member_anchors
      .get(&holder)
      .is_some_and(|anchor| sealing.keys.iter().any(|key| key.anchor == *anchor))
  })
}

/// Whether the owner's head for `object` names sealed content it may not ship yet, because its sealing does not cover
/// every live candidate ([`sealing_covers_candidates`]): the head waits for a pair delivery. Read by
/// `Daemon::fleet_head_awaits_keys`, so a placement that never completes names this cause.
pub(crate) fn head_awaits_keys(state: &ShardState, object: ObjectId) -> bool {
  let Some(record) = state.db.partition().volume(DbVolumeId { bytes: object.0 }) else {
    return false;
  };
  let config = state.fleet.configuration();
  head_value_of(state, record, object, config.quorum).is_some_and(|(_, value)| {
    !sealing_covers_candidates(state, config, object, &value, state.fleet.host())
  })
}

/// The owner's sealed manifest for `object` once its content has placed: from the snapshot's durable record once the
/// seal completed, else from the seal in progress once its content placed; `None` before then, or for a volume this
/// shard does not own (read by `Daemon::fleet_sealed_manifest`).
pub(crate) fn sealed_manifest(state: &ShardState, object: ObjectId) -> Option<[u8; 32]> {
  let record = state
    .db
    .partition()
    .volume(DbVolumeId { bytes: object.0 })?;
  if let Some(snapshot) = state.db.partition().snapshot(record.id, record.head)
    && let PlacementState::Placed { .. } = &snapshot.placed
    && let Some(identity) = snapshot.identity
  {
    return Some(identity);
  }
  let seal = state.seals.get(&object)?;
  if seal.snapshot != record.head || !seal.content.placed(state.fleet.configuration().quorum) {
    return None;
  }
  seal.manifest
}

/// The head this node should be shipping for `record`'s volume — its sequence and value — or `None` while
/// the volume's newest snapshot has content not yet placed. At epoch 0 it is the creation head (no content).
/// For a sealed snapshot it names the manifest and the content holders: from the snapshot's durable record
/// once the seal completed (`SnapshotIdentified` + `SnapshotPlaced`), else from the seal in progress once
/// its content has placed, else nothing yet. The catalog essentials come from the volume record.
fn head_value_of(
  state: &ShardState,
  record: &VolumeRecord,
  object: ObjectId,
  quorum: Quorum,
) -> Option<(u64, HeadValue)> {
  // A head that names content carries the volume's lineage key for successors (A-92 piece 4c).
  let value = |manifest: Option<[u8; 32]>, content_holders: Vec<u64>| HeadValue {
    sealing: manifest.and(crate::seal_keys::head_sealing(
      state,
      record.id,
      &record.owner,
    )),
    manifest,
    content_holders,
  };
  if record.epoch == 0 {
    return Some((0, value(None, Vec::new())));
  }
  if let Some(snapshot) = state.db.partition().snapshot(record.id, record.head)
    && let PlacementState::Placed { region, .. } = &snapshot.placed
    && let Some(identity) = snapshot.identity
  {
    return Some((record.epoch, value(Some(identity), region.clone())));
  }
  let seal = state.seals.get(&object)?;
  if seal.snapshot != record.head || !seal.content.placed(quorum) {
    return None;
  }
  let holders = seal.content.acked.iter().map(|host| host.0).collect();
  Some((seal.sequence, value(seal.manifest, holders)))
}

/// Which content placement a seal job serves: the home region's candidates, or the owner's neighbourhood in its
/// mirror region (§4.10 "Mirroring across regions"; `docs/wip/mirroring.md`). The two run the same rounds over
/// different candidates and are kept apart, each in its own map on the owner shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Track {
  /// The seal of a new snapshot to the home region's candidates ([`ShardState::seals`]).
  Home,
  /// The shipment of a home-placed snapshot to the mirror region ([`ShardState::mirror_seals`]).
  Mirror,
}

/// The seal jobs of `track` on this owner shard.
fn seal_jobs(
  state: &mut ShardState,
  track: Track,
) -> &mut std::collections::BTreeMap<ObjectId, SealJob> {
  match track {
    Track::Home => &mut state.seals,
    Track::Mirror => &mut state.mirror_seals,
  }
}

/// A seal whose archive is complete and whose content is not yet placed: the put to run this period.
struct ContentWork {
  /// Which placement the seal serves: its map on the owner shard, and the first round's size.
  track: Track,
  /// The shard that owns the volume — where the seal lives and its acknowledgements are recorded.
  shard: u16,
  object: ObjectId,
  snapshot: DbSnapshotId,
  sequence: u64,
  archive: Archive,
  candidates: Vec<HostId>,
  acked: Vec<HostId>,
  quorum: Quorum,
  round: u32,
  /// Whether the first round has been outstanding for the measured hedge delay ([`hedge_delay_ns`]) — the
  /// design's trigger for hedging to the remaining candidates. Decided by the clock in [`content_work`],
  /// **not** by the round count: a first round whose only holder's session is unavailable (borrowed by a
  /// straggler, its shard starved) produces no placement and so never advanced the count, and the put
  /// re-aimed at that same holder every period until it came back — 3.2 s against a 3 s hold, traced —
  /// while the hedge delay had long elapsed.
  hedged: bool,
  /// This round's budget ([`content_budget`]): its collection stops at the owner shard's measured hedge
  /// delay, computed from the shard's put-latency window at the moment the work is handed out.
  budget: CommitBudget,
}

/// The status refusal count under which the fleet loop records a snapshot it could not archive (a
/// base-backed entry whose bytes are on disk, not in RAM; a torn read): the seal stays `Local`, reported.
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
const SEAL_REFUSED: &str = "fleet.seal";

/// This node's neighbourhood in its region's mirror region (§4.10 "the owner's neighbourhood in the mirror region";
/// `docs/wip/mirroring.md` decision 1): the mirror region's members this node holds alive and has authenticated,
/// ranked by rendezvous keyed on this node. Empty when its region declares no mirror.
fn mirror_neighbourhood_of(state: &ShardState, local: HostId) -> Vec<HostId> {
  let own = state
    .node_regions
    .get(&local)
    .copied()
    .unwrap_or(RegionId(0));
  let Some(mirror) = state.region_mirrors.get(&own).copied() else {
    return Vec::new();
  };
  if mirror == own {
    return Vec::new();
  }
  let members: Vec<HostId> = state
    .node_regions
    .iter()
    .filter(|(host, region)| {
      **region == mirror
        && state.authenticated_members.contains(host)
        && state
          .fleet
          .membership()
          .state(**host)
          .is_some_and(|belief| belief.liveness != Liveness::Dead)
    })
    .map(|(host, _)| *host)
    .collect();
  slates_db::mirror::mirror_neighbourhood(local, &members, state.fleet.configuration().quorum)
}

/// The opening of a period's mirror walk ([`seal_walk_slice`]): this node's mirror neighbourhood, settled once for the
/// period. Whether there is one to ship to; with none, every mirror job is dropped and the walk visits nothing.
fn open_mirror_walk(state: &mut ShardState, local: HostId) -> bool {
  state.mirror_neighbourhood = mirror_neighbourhood_of(state, local);
  if !state.mirror_neighbourhood.is_empty() {
    return true;
  }
  // A mirror declared for this node's region with no live, authenticated member of it to ship to is counted: it is the
  // one way a declared mirror ships nothing without a refusal anywhere.
  let own = state
    .node_regions
    .get(&local)
    .copied()
    .unwrap_or(RegionId(0));
  if state
    .region_mirrors
    .get(&own)
    .is_some_and(|mirror| *mirror != own)
  {
    state.count(MIRROR_NO_NEIGHBOURHOOD, 1);
  }
  state.mirror_seals.clear();
  false
}

/// One volume's step of a period's mirror shipments (§4.10 "Mirroring across regions"; `docs/wip/mirroring.md` piece
/// M2; [`seal_walk_slice`]): when its head snapshot is recorded placed at home and not yet in the mirror, a mirror job
/// is started (or replaced, when a newer snapshot superseded it), its archive walk advanced one slice, and its content
/// round to the mirror neighbourhood returned once the archive is complete. A volume whose head the mirror already
/// holds, or that is being destroyed, keeps no job.
fn advance_mirror_seal(
  state: &mut ShardState,
  (id, handle): (DbVolumeId, slates_mem::Handle<crate::state::VolumeSlot>),
  slicing: SealSlicing,
  round: CommitBudget,
) -> Option<ContentWork> {
  let object = ObjectId(id.bytes);
  let Some(head) = mirror_due(state, id) else {
    state.mirror_seals.remove(&object);
    return None;
  };
  if state
    .mirror_seals
    .get(&object)
    .is_some_and(|job| job.snapshot != head.0)
  {
    state.mirror_seals.remove(&object);
  }
  if !state.mirror_seals.contains_key(&object) {
    if !start_mirror_seal(state, object, handle, head, slicing.created_unix) {
      return None;
    }
    state.count(MIRROR_JOB_STARTED, 1);
  }
  if !advance_seal(state, Track::Mirror, object, handle, slicing) {
    return None;
  }
  let quorum = state.fleet.configuration().quorum;
  let item = content_work(state, Track::Mirror, object, quorum, round)?;
  state.count(MIRROR_CONTENT_ROUNDS, 1);
  Some(item)
}

/// The head snapshot of `id` and its sequence when it is placed at home and its mirror placement is not yet recorded:
/// what a mirror job ships. `None` before the home placement (content places at home first), once the mirror holds
/// it, and for a volume being destroyed.
fn mirror_due(state: &ShardState, id: DbVolumeId) -> Option<(DbSnapshotId, u64)> {
  let record = state.db.partition().volume(id)?;
  if record.epoch == 0 || record.state == VolumeState::Destroying {
    return None;
  }
  let snapshot = state.db.partition().snapshot(id, record.head)?;
  matches!(snapshot.placed, PlacementState::Placed { mirror: None, .. })
    .then_some((record.head, record.epoch))
}

/// Starts the mirror shipment of `head` for `object`: the archive walk over the volume's snapshot, put to the
/// object's cohort in the mirror neighbourhood (its rendezvous order over it), with nothing acknowledged yet: the
/// owner holds no mirror copy. `false`, counted, if the snapshot cannot be walked.
fn start_mirror_seal(
  state: &mut ShardState,
  object: ObjectId,
  handle: slates_mem::Handle<crate::state::VolumeSlot>,
  (head, sequence): (DbSnapshotId, u64),
  created_unix: u64,
) -> bool {
  let Ok(slot) = state.volumes.get(handle) else {
    return false;
  };
  let archiver = SnapshotArchiver::new(
    &slot.volume,
    &state.store,
    verbs::core_snapshot(slates_ipc::protocol::SnapshotId { value: head.value }),
    object.local(),
    created_unix,
    state.config.codec.clone(),
  );
  let Ok(archiver) = archiver else {
    state.count(SEAL_REFUSED, 1);
    return false;
  };
  let candidates = slates_db::register::rendezvous_ranked(&state.mirror_neighbourhood, object);
  state.mirror_seals.insert(
    object,
    SealJob {
      snapshot: head,
      sequence,
      put_sequence: sequence,
      archiver: Some(archiver),
      wrapping: None,
      archive: None,
      manifest: None,
      content: Placement {
        candidates,
        acked: Vec::new(),
        mirror_acked: None,
        joint: Vec::new(),
      },
      rounds: 0,
      first_round_at_ns: None,
      healing: false,
      republish: false,
      refilled: Vec::new(),
      records_acked: Vec::new(),
      shipment: None,
    },
  );
  true
}

/// Records durably each mirror shipment whose content `f + 1` of its mirror cohort hold
/// (`SnapshotPlaced { mirror: Some(holders) }`, keeping the home placement), and drops it: the fact
/// `await placed(mirror)` and a mirror's lag answer from. Runs on the owner shard.
fn record_mirrored_seals(state: &mut ShardState) {
  let quorum = state.fleet.configuration().quorum;
  let now = state.clock.monotonic_ns();
  let done: Vec<(ObjectId, DbSnapshotId, Vec<u64>)> = state
    .mirror_seals
    .iter()
    .filter(|(_, job)| {
      job.manifest.is_some() && quorum.committed(crate::mirror::holders_of(job).len())
    })
    .map(|(object, job)| {
      (
        *object,
        job.snapshot,
        crate::mirror::holders_of(job)
          .iter()
          .map(|host| host.0)
          .collect(),
      )
    })
    .collect();
  for (object, snapshot, mirror) in done {
    let volume = DbVolumeId { bytes: object.0 };
    let region = match state.db.partition().snapshot(volume, snapshot) {
      Some(record) => match &record.placed {
        PlacementState::Placed { region, .. } => region.clone(),
        PlacementState::Local => Vec::new(),
      },
      None => Vec::new(),
    };
    let placed = Op::SnapshotPlaced {
      volume,
      id: snapshot,
      placed: PlacementState::Placed {
        region,
        mirror: Some(mirror),
      },
    };
    // A snapshot gone meanwhile (destroyed) has nothing to record; the job is dropped either way.
    let recorded = state.db.mutate(&mut state.segment, &placed, now);
    if recorded.is_err() {
      state.count(MIRROR_RECORD_REFUSED, 1);
    }
    state.mirror_seals.remove(&object);
  }
}

/// Counter: periods in which this node's region declares a mirror but no live, authenticated member of the mirror
/// region was found to ship to (`open_mirror_walk`).
const MIRROR_NO_NEIGHBOURHOOD: &str = "fleet.mirror.no_neighbourhood";

/// Counter: home content rounds that found no free session to any of their targets (`put_seal_content`).
const SEAL_NO_HOLDER_SESSION: &str = "fleet.seal.no_holder_session";

/// Counter: mirror content rounds that found no free session to any of their targets (`put_seal_content`).
const MIRROR_NO_HOLDER_SESSION: &str = "fleet.mirror.no_holder_session";

/// Counter: content acknowledgements mirror holders returned within their rounds (`put_seal_content`).
const MIRROR_ACKS: &str = "fleet.mirror.acks";

/// Counter: mirror content rounds that failed outright (neither placed, uncertain nor short of the quorum).
const MIRROR_ROUND_FAILED: &str = "fleet.mirror.round_failed";

/// Counter: home placements recorded again for a snapshot already placed in the mirror, which kept its mirror
/// placement (`record_placed_seals`).
const MIRROR_KEPT_ON_HOME_RECORD: &str = "fleet.mirror.kept_on_home_record";

/// Counter: mirror shipments started (a snapshot placed at home, its archive walk begun for the mirror region).
const MIRROR_JOB_STARTED: &str = "fleet.mirror.job_started";

/// Counter: content rounds sent to the mirror neighbourhood (a mirror shipment's archive complete and put).
const MIRROR_CONTENT_ROUNDS: &str = "fleet.mirror.content_rounds";

/// The status count of mirror placements the database refused to record (the snapshot destroyed meanwhile, or the
/// partition refusing): the shipment is dropped and the next period starts it again if the snapshot still stands.
/// Format: a refusal name in the daemon's status report.
const MIRROR_RECORD_REFUSED: &str = "fleet.mirror.record_refused";

/// One volume's step of a period's seals (§4.10 "auto-seal" → content replication; §4.3 bounded slices;
/// [`seal_walk_slice`]): when its newest snapshot is not yet placed, a seal job is started (or replaced, when a newer
/// snapshot superseded the one being sealed), its archive walk is advanced one slice of `slice_bytes`, and — once the
/// archive is complete and its content not yet placed — the content put to run this period is returned, its archive
/// moved out for the dispatch and put back after ([`put_seal_content`]). A seal whose snapshot the database already
/// records placed is dropped (a restart resumes nothing it need not).
fn advance_home_seal(
  state: &mut ShardState,
  local: HostId,
  (id, handle): (DbVolumeId, slates_mem::Handle<crate::state::VolumeSlot>),
  slicing: SealSlicing,
  round: CommitBudget,
) -> Option<ContentWork> {
  let object = ObjectId(id.bytes);
  // A captured clone's head carries its capture's record (A-544): its carry is stepped whether or not its seal is done,
  // since the head register waits for the record after its content is placed.
  crate::carry::step(state, id);
  let (head, sequence) = sealable_head(state, id, object)?;
  // The volume's lineage and naming keys (`namer` makes the lineage key first), made before its first seal, so the head
  // naming the seal's content carries both for successors (A-92 piece 4c). Off the hot path: once per seal job.
  if !state.seals.contains_key(&object)
    && state.seal_root.is_some()
    && let Some(owner) = state
      .db
      .partition()
      .volume(id)
      .map(|volume| volume.owner.clone())
    && crate::seal_keys::namer(state, id, crate::seal_keys::tenant_of(&owner)).is_err()
  {
    state.count(LINEAGE_UNMADE, 1);
  }
  if !state.seals.contains_key(&object)
    && !start_seal(
      state,
      local,
      object,
      handle,
      head,
      sequence,
      slicing.created_unix,
      None,
    )
  {
    return None;
  }
  if !advance_seal(state, Track::Home, object, handle, slicing) {
    return None;
  }
  let quorum = state.fleet.configuration().quorum;
  content_work(state, Track::Home, object, quorum, round)
}

/// Counter: a period's home seal walk over one owner shard's volumes ([`seal_walk_slice`]), however many turns it took.
const SEAL_WALKS: &str = "fleet.seal.walks";
/// Counter: one owner-shard turn of the home seal walk.
const SEAL_WALK_SLICES: &str = "fleet.seal.walk_slices";
/// Counter: a period's mirror walk over one owner shard's volumes, however many turns it took.
const MIRROR_WALKS: &str = "fleet.mirror.walks";
/// Counter: one owner-shard turn of the mirror walk.
const MIRROR_WALK_SLICES: &str = "fleet.mirror.walk_slices";

/// One owner-shard turn of a period's seal walk for `track` (§4.10 "Content replication"; CLAUDE.md "Bounded work
/// everywhere"): the shard's volumes after `after` in id order, each visited ([`advance_home_seal`],
/// [`advance_mirror_seal`]) until the shard's step quantum is spent (`futures::step_budget_ns`, what a destroy's or an
/// archive's slice is sized by): the content puts made ready, and the last volume visited when any remain after it
/// (`None` once the walk is done). The first volume is always visited, so a turn always moves the walk on. `opening`
/// marks the walk's first turn this period, which settles what a period decides once (the mirror neighbourhood); the
/// home walk's last turn takes the healer's step. Until 2026-11-17 one turn visited every volume and advanced every
/// seal job a slice each: a shard sealing `n` volumes at once was held for `n` archive slices, each sized to a step
/// quantum, while its clients waited, as the live record period was (`live_ship::period_slice`).
fn seal_walk_slice(
  state: &mut ShardState,
  local: HostId,
  (track, opening): (Track, bool),
  after: Option<DbVolumeId>,
  round: CommitBudget,
) -> (Vec<ContentWork>, Option<DbVolumeId>) {
  let (walks, slices) = match track {
    Track::Home => (SEAL_WALKS, SEAL_WALK_SLICES),
    Track::Mirror => (MIRROR_WALKS, MIRROR_WALK_SLICES),
  };
  if opening {
    state.count(walks, 1);
  }
  state.count(slices, 1);
  if opening && track == Track::Mirror && !open_mirror_walk(state, local) {
    return (Vec::new(), None);
  }
  let quantum = seal_quantum_ns(state);
  let slicing = SealSlicing {
    archive_bytes: state.config.archive_slice_bytes(),
    wrap_ns: crate::config::wrap_slice_ns(quantum),
    created_unix: u64::try_from(state.clock.wall_ns()).unwrap_or(0) / NANOS_PER_SECOND,
  };
  let began = futures::now_ns();
  let mut cursor = after;
  let mut work = Vec::new();
  let remaining = loop {
    let next = match cursor {
      Some(last) => state
        .by_id
        .range((std::ops::Bound::Excluded(last), std::ops::Bound::Unbounded))
        .next(),
      None => state.by_id.iter().next(),
    }
    .map(|(id, handle)| (*id, *handle));
    let Some(volume) = next else {
      break None;
    };
    let item = match track {
      Track::Home => advance_home_seal(state, local, volume, slicing, round),
      Track::Mirror => advance_mirror_seal(state, volume, slicing, round),
    };
    work.extend(item);
    cursor = Some(volume.0);
    if futures::now_ns().saturating_sub(began) >= quantum {
      break cursor;
    }
  };
  if remaining.is_none() && track == Track::Home {
    heal_one_placed_snapshot(state, local, slicing.created_unix);
    // The walk visits only the volumes this shard serves, so the carry of one it let go is forgotten here (A-544).
    crate::carry::forget_gone(state);
  }
  (work, remaining)
}

/// The healer's step (§4.10 "anti-entropy walks Merkle manifests between recorded holders and repairs only
/// differing subtrees; the healer replays puts …"; §4.8 "Derived constants": "healer cadence from the
/// measured put-failure rate"): once per healer period ([`heal_period_ns`]) it opens a `healing` seal job for
/// the **next** owned volume whose head snapshot is recorded placed, of the kind [`heal_for`] decides:
/// - **Restore** the holders the placement, and so the shipped head, names: the ordinary content rounds
///   re-**offer** the archive to each of them at once — a holder that lost nothing answers with an empty missing
///   set and is put zero bytes (the Merkle identity diff finds no differing subtree), a holder that lost content
///   is put exactly what it lacks and counts as a repair — and the job ends once every one of them holds it,
///   recording nothing: the placement and the head already name them. Until 2026-11-17 the job re-offered to the
///   current cohort in rendezvous order, placed on whichever candidates answered first and re-recorded that set
///   at the head's sequence, so a cohort member that held no head yet took the content in the named holder's
///   place, the named holder stayed empty, and the head kept naming it (A-276).
/// - **Replace** a named holder that has left the fleet: the content is put on the current cohort at the next
///   sequence, and once recorded the head is republished there ([`record_replacement`]), since a head is never
///   rewritten at its sequence.
///
/// One snapshot per period, in volume-id order, wrapping: a bounded slice of the walk over everything this
/// node has placed (§4.3 bounded work). Nothing to do while a seal for the volume is already in progress, on a
/// laptop (nothing is placed remotely), or between steps.
fn heal_one_placed_snapshot(state: &mut ShardState, local: HostId, created_unix: u64) {
  let now = state.clock.monotonic_ns();
  if state
    .healer
    .last_step_ns
    .is_some_and(|last| now.saturating_sub(last) < heal_period_ns(&state.put_outcomes))
  {
    return;
  }
  let mut volumes: Vec<(DbVolumeId, slates_mem::Handle<crate::state::VolumeSlot>)> =
    state.by_id.iter().map(|(id, h)| (*id, *h)).collect();
  volumes.sort_unstable_by_key(|(id, _)| id.bytes);
  // The next volume after the cursor, wrapping to the first: one step of a round-robin walk.
  let start = state.healer.after.map_or(0, |after| {
    volumes.partition_point(|(id, _)| id.bytes <= after.bytes)
  });
  let candidate = volumes
    .iter()
    .skip(start)
    .chain(volumes.iter().take(start))
    .find_map(|(id, handle)| {
      let object = ObjectId(id.bytes);
      let record = state.db.partition().volume(*id)?;
      // A copy frozen for a move (A-291) is the move's to place: before the switch its handoff places it
      // ([`start_move_handoff`]), and after it the volume is another member's, whose records a heal from here would
      // contend with (a frozen copy's heal republished its head once its target died, 2026-11-18).
      if record.epoch == 0
        || record.state == VolumeState::Destroying
        || state.seals.contains_key(&object)
        || crate::moves::frozen(state, slates_ipc::protocol::VolumeId { bytes: id.bytes })
      {
        return None;
      }
      let snapshot = state.db.partition().snapshot(*id, record.head)?;
      let PlacementState::Placed { region, .. } = &snapshot.placed else {
        return None;
      };
      Some((
        *id,
        *handle,
        object,
        record.head,
        record.epoch,
        region.clone(),
      ))
    });
  state.healer.last_step_ns = Some(now);
  let Some((id, handle, object, head, sequence, region)) = candidate else {
    state.healer.after = None;
    return;
  };
  state.healer.after = Some(id);
  let Some(heal) = heal_for(state, local, object, &region) else {
    return;
  };
  start_seal(
    state,
    local,
    object,
    handle,
    head,
    sequence,
    created_unix,
    Some(heal),
  );
}

/// What a healing job does ([`heal_one_placed_snapshot`]).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Heal {
  /// Re-offer the content at the head's sequence to these holders — the ones the placement names that can be put to,
  /// the owner among them when named — and end once every one of them holds it: the placement and the head stay as
  /// they are.
  Restore(Vec<HostId>),
  /// Place on the current cohort at the next sequence and republish the head there naming that set.
  Replace,
  /// Place on the current cohort without this owner at the next sequence and republish the head there naming that set
  /// ([`start_move_handoff`], A-291): a volume moving to another member is handed over with its head held by `f + 1`
  /// hosts other than the member it leaves. This owner's own copy is its content only while it owns the volume: once
  /// the configuration names the new owner, every successor fetches by identity from the holders the head names, and
  /// this node's frozen copy serves no fetch, so the owner is neither a candidate nor counted.
  Handoff,
}

/// The heal for an owned snapshot whose recorded placement names `region` (§4.10 "the healer"), as [`heal_decision`]
/// decides it from the council's members and `object`'s candidates under the current configuration, with what it found
/// counted.
fn heal_for(
  state: &mut ShardState,
  local: HostId,
  object: ObjectId,
  region: &[u64],
) -> Option<Heal> {
  let configuration = state.fleet.configuration();
  let quorum = configuration.quorum;
  let candidates = configuration.place(object).candidates;
  // While this owner's neighbourhood change is in flight, a placement naming a host outside its current neighbourhood is
  // replaced on the current cohort, so its settle can certify its content (A-546): a restore keeps it named.
  let unsettled = unsettled_replace(
    region,
    local,
    &configuration.neighbourhood,
    configuration.settled.is_some(),
  );
  let adopted = crate::takeover::adopted_here(state, object);
  let (mut heal, found) = heal_decision(
    local,
    region,
    &state.council.configuration().members,
    &candidates,
    (quorum, adopted),
  );
  if adopted && heal == Some(Heal::Replace) && found.named_outside_candidates {
    state.count(HEAL_ADOPTED_REPLACED, 1);
  }
  if unsettled && heal != Some(Heal::Replace) {
    heal = Some(Heal::Replace);
    state.count(HEAL_UNSETTLED_REPLACED, 1);
  }
  for (seen, counter) in [
    (found.named_gone, HEAL_NAMED_GONE),
    (found.named_short, HEAL_NAMED_SHORT),
    (
      found.named_outside_candidates,
      HEAL_NAMED_OUTSIDE_CANDIDATES,
    ),
  ] {
    if seen {
      state.count(counter, 1);
    }
  }
  heal
}

/// Whether the healer replaces a placement naming `region` because this owner's (`local`'s) neighbourhood change is in
/// flight (`unsettled`) and the placement names a host outside its current `neighbourhood` (A-546): the settle report
/// waits for every owned volume to be served from within it ([`content_placed_on_current_neighbourhood`]).
fn unsettled_replace(
  region: &[u64],
  local: HostId,
  neighbourhood: &[HostId],
  unsettled: bool,
) -> bool {
  unsettled && !crate::takeover::served_from_placement(region, local, neighbourhood)
}

/// What the healer found in a placement beside the heal it decided ([`heal_decision`]); each is counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct HealFindings {
  /// A remote holder the placement names is no longer a member ([`HEAL_NAMED_GONE`]).
  named_gone: bool,
  /// The placement names fewer than `f + 1` holders ([`HEAL_NAMED_SHORT`]).
  named_short: bool,
  /// A remote holder the placement names is a member outside the object's candidates
  /// ([`HEAL_NAMED_OUTSIDE_CANDIDATES`]).
  named_outside_candidates: bool,
}

/// The heal for a placement naming `region`, owned by `local`, under the council's `members` and the object's current
/// `candidates` at `quorum`, `adopted` when a takeover round here adopted the volume and has not confirmed (§4.10 "the
/// healer"; A-276, A-529). Pure, so the rule is tested at `f = 0` and on a fleet alike:
/// - a **replace** when a remote holder it names is no longer a member, which no re-offer can reach, or when it names
///   fewer than `f + 1` holders (one recorded before the fleet's `f` grew): the content is placed on the current cohort
///   at the next sequence and the head republished there;
/// - a **replace** too for an adopted volume whose placement names a member outside its candidates (A-529): its round
///   confirms only once it serves the volume from its own placement (`takeover::adopted_served`), and a restore would
///   leave that member named for good;
/// - otherwise a **restore** of the holders it names that can be put to: the owner, which holds its own content, and the
///   named remote holders among the candidates, the only ones that admit a placement from this owner
///   (`content_authority`). A named member outside the candidates keeps the copy its head names and serves reads of it,
///   but cannot be put to, so it is left out;
/// - no heal when no named remote holder can be put to: at `f = 0` the owner's own copy is the whole placement, and
///   nothing remote is named to verify.
///
/// Until 2026-11-17 the replace was taken whenever no remote holder was named, which at `f = 0` replaced a whole
/// placement at every healer step and advanced a one-node fleet's head sequence by one each time, for good.
fn heal_decision(
  local: HostId,
  region: &[u64],
  members: &[HostId],
  candidates: &[HostId],
  (quorum, adopted): (Quorum, bool),
) -> (Option<Heal>, HealFindings) {
  let mut named: Vec<HostId> = Vec::with_capacity(region.len());
  for host in region.iter().map(|host| HostId(*host)) {
    if !named.contains(&host) {
      named.push(host);
    }
  }
  let remote = || named.iter().filter(|host| **host != local);
  let copies = u64::try_from(named.len()).unwrap_or(u64::MAX);
  let found = HealFindings {
    named_gone: remote().any(|host| !members.contains(host)),
    named_short: copies < u64::from(quorum.f).saturating_add(1),
    named_outside_candidates: remote().any(|host| !candidates.contains(host)),
  };
  if found.named_gone || found.named_short || (adopted && found.named_outside_candidates) {
    return (Some(Heal::Replace), found);
  }
  // The holders the restore ends with: the owner when the placement names it, and the named remote holders it can put
  // to, in the placement's order.
  let holders: Vec<HostId> = named
    .iter()
    .copied()
    .filter(|host| *host == local || candidates.contains(host))
    .collect();
  let restore = holders.iter().any(|host| *host != local);
  (restore.then_some(Heal::Restore(holders)), found)
}

/// The status count under which the healer records a step that found a holder the shipped head names no longer a
/// member: a re-offer cannot reach that holder, so the job places on the current cohort and republishes the head
/// (A-276).
/// Format: a refusal name in the daemon's status report.
const HEAL_NAMED_GONE: &str = "fleet.heal.named_gone";

/// The status count under which the healer records a step that found the shipped head naming fewer than `f + 1`
/// holders, as a placement recorded before the fleet's `f` grew does: the job places on the current cohort and
/// republishes the head, as for a departed holder (A-276).
/// Format: a refusal name in the daemon's status report.
const HEAL_NAMED_SHORT: &str = "fleet.heal.named_short";

/// The status count under which the healer records a step that found a holder the shipped head names still a member
/// but no longer among the object's candidates, so no placement to it is admitted: its copy is left as its head names
/// it, and only the named candidates are restored.
/// Format: a refusal name in the daemon's status report.
const HEAL_NAMED_OUTSIDE_CANDIDATES: &str = "fleet.heal.named_outside_candidates";

/// The status count under which the healer records a replace of an adopted volume's placement that named a member
/// outside the volume's candidates (A-529): a restore would have left that member named, and the takeover round that
/// adopted the volume waits to confirm until it serves it from its own placement. Format: a refusal name in the daemon's
/// status report.
const HEAL_ADOPTED_REPLACED: &str = "fleet.heal.adopted_replaced";

/// The status count under which the healer records a replace of a placement naming a host outside this owner's current
/// neighbourhood while its neighbourhood change is in flight (A-546): its settle waits until every owned volume is
/// served from within that neighbourhood. Format: a refusal name in the daemon's status report.
const HEAL_UNSETTLED_REPLACED: &str = "fleet.heal.unsettled_replaced";

/// The status count under which the healer records a heal that ran its bound ([`heal_bound_ns`]) without finishing
/// (every put refused, or a holder to restore unreachable): the job is dropped and the volume healed again on a later
/// step, so one stuck heal never leaves the healer skipping the volume (banned item 8; A-276).
/// Format: a refusal name in the daemon's status report.
const HEAL_UNFINISHED: &str = "fleet.heal.unfinished";

/// The status count under which the healer records a head republished at the next sequence to name the holders that
/// hold its content (A-276). Format: a refusal name in the daemon's status report.
const HEAL_REPUBLISHED: &str = "fleet.heal.republished";

/// The status count under which the healer records a replace whose placement and republished head were not recorded:
/// the database refused the transaction (rolled back whole), or the volume's head moved before it could be advanced.
/// Nothing was recorded, so the shipped head keeps naming the holders it named and the volume is healed again on a later
/// step. Format: a refusal name in the daemon's status report.
const HEAL_REPUBLISH_REFUSED: &str = "fleet.heal.republish_refused";

/// The status count under which the healer records a replace whose head's sequence is already the largest value, so
/// no sequence can follow it to put the content at: the head keeps naming the holders it named.
/// Format: a refusal name in the daemon's status report.
const HEAL_EPOCH_EXHAUSTED: &str = "fleet.heal.epoch_exhausted";

/// The status count under which the healer records a restore that ended with every holder the head names holding the
/// content, whether it put any back or found each whole: the placement and the head were left as they were (§4.10
/// "the healer"). Format: a refusal name in the daemon's status report.
const HEAL_VERIFIED: &str = "fleet.heal.verified";

/// The status count of repairs whose holder's acknowledgement arrived after its round had stopped collecting at the
/// hedge delay ([`fold_late_content`]): a repair all the same, and counted in the shard's repairs; this count shows
/// how many of them took the straggler's path (§4.10 "the healer").
/// Format: a refusal name in the daemon's status report.
const HEAL_REPAIRED_LATE: &str = "fleet.heal.repaired_late";

/// The status count of holders a restoring round put bytes to whose acknowledgement the round did not collect: their
/// refill outlasted the round's collection at the hedge delay, as a large volume's does against small puts' p95 and
/// any refill can under load. Their repair is counted when the acknowledgement is folded later, or in a later round.
/// Format: a refusal name in the daemon's status report.
const HEAL_REFILL_OUTLASTED_ROUND: &str = "fleet.heal.refill_outlasted_round";

/// The head snapshot of `id` that still needs sealing — its id and the head sequence — or `None` when the
/// volume has no snapshot of its own yet, or its head is already recorded placed (a finished seal is dropped). A
/// seal in progress for an older snapshot is dropped too: the newer snapshot supersedes it.
fn sealable_head(
  state: &mut ShardState,
  id: DbVolumeId,
  object: ObjectId,
) -> Option<(DbSnapshotId, u64)> {
  let record = state.db.partition().volume(id)?;
  if record.state == VolumeState::Destroying {
    // A volume being destroyed places nothing more: its registers are closing (AUD-29-43).
    state.seals.remove(&object);
    return None;
  }
  if record.epoch == 0 {
    return None; // Nothing sealed yet: the creation head names no content.
  }
  let (head, sequence) = (record.head, record.epoch);
  // A head that is none of the volume's own snapshots here has nothing to seal: a clone's, until its first snapshot (its
  // record starts as a copy of its origin's, `verbs::publish_clone_as`, so its head names its origin's snapshot, which
  // its own tree cannot walk), or one recovery reset to none (`verbs::reconcile_lost`). Before, `start_seal` was refused
  // for it, counted `fleet.seal`, every period until the volume's next snapshot: for every clone made (A-565's piece).
  let Some(snapshot) = state.db.partition().snapshot(id, head) else {
    state.seals.remove(&object);
    return None;
  };
  let placed = matches!(snapshot.placed, PlacementState::Placed { .. });
  if placed {
    // A finished seal is dropped — unless it is the healer's re-offer of this placed snapshot, which is
    // kept until its round has run (§4.10 "the healer"); it is dropped when it re-records the placement.
    if state
      .seals
      .get(&object)
      .is_some_and(|job| job.healing && job.snapshot == head)
    {
      return Some((head, sequence));
    }
    state.seals.remove(&object);
    return None;
  }
  if state
    .seals
    .get(&object)
    .is_some_and(|job| job.snapshot != head)
  {
    state.seals.remove(&object);
  }
  Some((head, sequence))
}

/// Starts the seal of `head` for `object`: the archive walk over the volume's snapshot, and the content
/// placement with the owner already counted (it holds its own content) when it is a candidate. `false`,
/// counted, if the snapshot cannot be walked. A `healing` seal is the healer's work on an already-placed
/// snapshot ([`heal_one_placed_snapshot`]): a restore puts to the holders its placement names, a replace to the
/// current cohort at the next sequence.
#[allow(clippy::too_many_arguments)]
fn start_seal(
  state: &mut ShardState,
  local: HostId,
  object: ObjectId,
  handle: slates_mem::Handle<crate::state::VolumeSlot>,
  head: DbSnapshotId,
  sequence: u64,
  created_unix: u64,
  healing: Option<Heal>,
) -> bool {
  // A replace (and a move's handoff) puts its content at the next sequence, ahead of every holder's records, since a
  // holder refuses a put at the head's own sequence that the head there does not name it for
  // (`content_retention::admits`).
  let republish = matches!(healing, Some(Heal::Replace | Heal::Handoff));
  let put_sequence = if republish {
    let Some(next) = sequence.checked_add(1) else {
      state.count(HEAL_EPOCH_EXHAUSTED, 1);
      return false;
    };
    next
  } else {
    sequence
  };
  let Ok(slot) = state.volumes.get(handle) else {
    return false;
  };
  let archiver = SnapshotArchiver::new(
    &slot.volume,
    &state.store,
    verbs::core_snapshot(slates_ipc::protocol::SnapshotId { value: head.value }),
    object.local(),
    created_unix,
    state.config.codec.clone(),
  );
  let Ok(archiver) = archiver else {
    state.count(SEAL_REFUSED, 1);
    return false;
  };
  // Content places on the current cohort alone (§4.10): the head names its holders, so a change in flight
  // never needs it joined. A restore puts to the holders the placement names instead: they are the ones the head
  // names and a successor fetches from, and a head is never rewritten at its sequence.
  let mut content = match &healing {
    Some(Heal::Restore(holders)) => Placement::of(holders),
    Some(Heal::Handoff) => {
      let mut cohort = state.fleet.configuration().place_content(object);
      cohort.candidates.retain(|host| *host != local);
      cohort
    }
    Some(Heal::Replace) | None => state.fleet.configuration().place_content(object),
  };
  content.acked = if content.candidates.contains(&local) {
    vec![local]
  } else {
    Vec::new()
  };
  state.seals.insert(
    object,
    SealJob {
      snapshot: head,
      sequence,
      put_sequence,
      archiver: Some(archiver),
      wrapping: None,
      archive: None,
      manifest: None,
      content,
      rounds: 0,
      first_round_at_ns: None,
      healing: healing.is_some(),
      republish,
      refilled: Vec::new(),
      records_acked: Vec::new(),
      shipment: None,
    },
  );
  true
}

/// Begins the handoff of a moving volume's head on its owner shard (A-291; [`Heal::Handoff`]): the head's content put at
/// the next sequence on this owner's cohort without this node, and the head republished there naming the hosts that took
/// it, so the head a move hands over is held by `f + 1` hosts other than the member it leaves (`crate::moves`). `false`,
/// nothing begun, while a seal job for the volume is under way (a handoff begun earlier among them) or when its head
/// cannot be walked ([`start_seal`] counts that).
pub(crate) fn start_move_handoff(state: &mut ShardState, id: DbVolumeId) -> bool {
  let object = ObjectId(id.bytes);
  if state.seals.contains_key(&object) {
    return false;
  }
  let Some(&handle) = state.by_id.get(&id) else {
    return false;
  };
  let Some((head, sequence)) = state
    .db
    .partition()
    .volume(id)
    .map(|record| (record.head, record.epoch))
  else {
    return false;
  };
  let local = state.fleet.host();
  let created_unix = u64::try_from(state.clock.wall_ns()).unwrap_or(0) / NANOS_PER_SECOND;
  start_seal(
    state,
    local,
    object,
    handle,
    head,
    sequence,
    created_unix,
    Some(Heal::Handoff),
  )
}

/// Whether `job`'s content is where the job must leave it: a restoring heal once every holder it restores holds it,
/// since each is one the shipped head names and a successor may fetch from; every other job — a seal, a replace, a
/// mirror shipment — once `f + 1` of its candidates do.
fn seal_complete(job: &SealJob, quorum: Quorum) -> bool {
  if restoring(job) {
    job
      .content
      .candidates
      .iter()
      .all(|holder| job.content.acked.contains(holder))
  } else {
    job.content.placed(quorum)
  }
}

/// Whether `job` is a heal that restores the holders its snapshot's placement names (rather than a replace).
fn restoring(job: &SealJob) -> bool {
  job.healing && !job.republish
}

/// How one turn of the seal walk slices each volume's seal ([`seal_walk_slice`]): the bytes its archive walk may hash,
/// the time its envelope's wrap may take (A-323), and the creation time a new archive carries.
#[derive(Clone, Copy, Debug)]
struct SealSlicing {
  /// The bytes one archive-walk slice may hash (`DaemonConfig::archive_slice_bytes`).
  archive_bytes: u64,
  /// How long one wrap slice may seal chunks for (`config::wrap_slice_ns` of the turn's quantum); a chunk at least.
  wrap_ns: u64,
  /// The informational creation time a new archive carries, Unix seconds.
  created_unix: u64,
}

/// Advances `object`'s seal one slice: its archive walk one slice of `slicing.archive_bytes`, then, once the archive is
/// complete on a node that seals, its envelope one slice of `slicing.wrap_ns` ([`wrap_slice`]); `true` once the archive
/// the content puts carry is complete (this turn or an earlier one). A snapshot that cannot be archived whole (an
/// overlay's base bytes are not in RAM) stays `Local`, counted so an operator sees why it never places (§4.4 coverage is
/// never upgraded). An archive whose envelope is refused drops the job, so the next period starts the seal again
/// (A-322).
fn advance_seal(
  state: &mut ShardState,
  track: Track,
  object: ObjectId,
  handle: slates_mem::Handle<crate::state::VolumeSlot>,
  slicing: SealSlicing,
) -> bool {
  // The track's map is picked field by field, so the job is borrowed beside the volumes and the store.
  let jobs = match track {
    Track::Home => &mut state.seals,
    Track::Mirror => &mut state.mirror_seals,
  };
  let Some(job) = jobs.get_mut(&object) else {
    return false;
  };
  if let Some(archiver) = job.archiver.as_mut() {
    let Ok(slot) = state.volumes.get(handle) else {
      return false;
    };
    match archiver.advance(&slot.volume, &state.store, slicing.archive_bytes) {
      Ok(Progress::More) => return false,
      Ok(Progress::Done(archive)) => {
        job.archiver = None;
        // Where this node cannot seal at all (its key region or root unavailable, reported in status) the archive is
        // placed as it is; where it can, the archive is wrapped first, from this turn on.
        if state.seal_root.is_none() {
          job.manifest = Some(archive.manifest_identity());
          job.archive = Some(archive);
          return true;
        }
        let segment = archive.base_page_size;
        job.wrapping = Some(slates_cluster::envelope::Wrapping::new(archive, segment));
      }
      Err(_) => {
        job.archiver = None;
        state.count(SEAL_REFUSED, 1);
        return false;
      }
    }
  }
  if job.wrapping.is_none() {
    return true;
  }
  wrap_slice(state, track, object, slicing.wrap_ns)
}

/// What one turn's slice of an envelope's wrap came to ([`wrap_slice`]).
enum Wrapped {
  /// The envelope is complete: the archive the content puts carry.
  Envelope(Archive),
  /// Chunks remain for a later turn.
  More,
  /// A key or the seal refused.
  Refused,
}

/// One turn's slice of `object`'s envelope (A-92 piece 3b, A-323): the archive the seal places wrapped under the
/// volume's lineage and naming keys (`slates_cluster::envelope`), so holders keep ciphertext and hold no key, in segments
/// of the volume's base page (seal.md §4: the smallest unit slates reads and seals at). Chunks are sealed until `wrap_ns`
/// has passed on the shard's clock, one at least, so no turn seals the whole archive (64 MiB took 98 ms in one turn,
/// 2026-10-05, and about a GiB ran past the watchdog's window). The keys are found again each turn, which costs a few
/// key unwraps: a volume's lineage and naming keys are made once and never replaced (`crate::seal_keys`), so every turn
/// seals under the keys the first did. `true` once the envelope is complete. Where a key or the seal refuses, the seal
/// is dropped and counted (A-322), and the next period starts it again; content is never placed in the clear by a node
/// that seals.
fn wrap_slice(state: &mut ShardState, track: Track, object: ObjectId, wrap_ns: u64) -> bool {
  let id = DbVolumeId { bytes: object.0 };
  let keys = state
    .db
    .partition()
    .volume(id)
    .map(|volume| crate::seal_keys::tenant_of(&volume.owner))
    .and_then(|account| {
      let lineage = crate::seal_keys::lineage(state, id, account).ok()?;
      let namer = crate::seal_keys::namer(state, id, account).ok()?;
      Some((lineage, namer))
    });
  let began = futures::now_ns();
  let wrapped = match (&keys, seal_jobs(state, track).get_mut(&object)) {
    (Some((lineage, namer)), Some(job)) => match job.wrapping.as_mut() {
      Some(wrapping) => wrap_until(wrapping, (lineage, namer), began, wrap_ns),
      None => return true,
    },
    (None, Some(_)) => Wrapped::Refused,
    (_, None) => return false,
  };
  let slices = match track {
    Track::Home => SEAL_WRAP_SLICES,
    Track::Mirror => MIRROR_WRAP_SLICES,
  };
  state.count(slices, 1);
  match wrapped {
    Wrapped::Envelope(envelope) => {
      let Some(job) = seal_jobs(state, track).get_mut(&object) else {
        return false;
      };
      job.wrapping = None;
      job.manifest = Some(envelope.manifest_identity());
      job.archive = Some(envelope);
      true
    }
    Wrapped::More => false,
    Wrapped::Refused => {
      // A refused envelope drops the seal, so the next period starts it again (A-322). Until 2026-11-18 the job was
      // kept with its walk spent and no archive: every later period found it complete and nothing to put, so its head
      // never placed, whatever the keys did next; a heal kept that way shut the healer out of the volume as well.
      seal_jobs(state, track).remove(&object);
      state.count(SEAL_ENVELOPE_REFUSED, 1);
      false
    }
  }
}

/// Seals `wrapping`'s chunks one at a time under `keys` until `wrap_ns` has passed since `began` on the shard's clock,
/// one chunk at least ([`wrap_slice`]).
fn wrap_until(
  wrapping: &mut slates_cluster::envelope::Wrapping,
  (lineage, namer): (&hyper_seal::keys::WrappingKey, &hyper_seal::name::Namer),
  began: u64,
  wrap_ns: u64,
) -> Wrapped {
  loop {
    // A budget of one byte seals exactly one chunk (or, with none left, the manifest), so the clock is read between
    // chunks.
    match wrapping.advance(lineage, namer, 1) {
      Ok(Some(envelope)) => return Wrapped::Envelope(envelope),
      Ok(None) if futures::now_ns().saturating_sub(began) < wrap_ns => {}
      Ok(None) => return Wrapped::More,
      Err(_) => return Wrapped::Refused,
    }
  }
}

/// Counter: one owner-shard turn's slice of a home seal's envelope wrap ([`wrap_slice`], A-323).
const SEAL_WRAP_SLICES: &str = "fleet.seal.wrap_slices";
/// Counter: one owner-shard turn's slice of a mirror shipment's envelope wrap.
const MIRROR_WRAP_SLICES: &str = "fleet.mirror.wrap_slices";

/// Derived: how many put-latency readings the owner shard keeps for the content class's p95 — the
/// acknowledgements of one recovery budget of seals. A seal is put to at most `2f` remote candidates, one
/// reading each, and the fleet's shape gives `f = 1` a floor of two; a hundred seals' worth
/// ([`PUT_LATENCY_SEALS`]) is the window over which the p95 is stable to one reading (nearest rank at
/// `95 / 100` moves one slot per twenty readings) yet forgets a load regime within a hundred seals — the
/// same "measured window" the probation threshold reads (§4.8 "late count over the measured window").
/// Anchored to [`PUT_LATENCY_SEALS`] × the candidate floor's remote count.
const PUT_LATENCY_WINDOW: usize = PUT_LATENCY_SEALS * 2;

/// Shape: the seals whose acknowledgements the put-latency window spans — a hundred, the smallest count at
/// which the nearest-rank p95 is resolved to a single reading (`(100 − 1) × 95 / 100 = 94`, the 95th of
/// a hundred), so the trigger is a real tail, not the median of a handful.
const PUT_LATENCY_SEALS: usize = 100;

/// A measured latency class on one shard (§4.8 "Derived constants": "hedge delay = measured p95 put latency per
/// class"): the newest [`PUT_LATENCY_WINDOW`] readings in arrival order. The content put class reads the time from
/// a put round's dispatch to one holder's verified acknowledgement; the content fetch class (A-91) reads the time
/// from a chunk's first request to its verified arrival. The p95 over them — the reading nineteen in twenty arrive
/// within — is the class's hedge trigger ([`hedge_delay_ns`]). Bounded: the oldest reading leaves as the newest
/// arrives (banned item 8).
#[derive(Debug, Default)]
pub struct LatencyWindow {
  readings_ns: std::collections::VecDeque<u64>,
}

impl LatencyWindow {
  /// Records one acknowledgement's latency, forgetting the oldest reading past the window.
  pub fn record(&mut self, latency_ns: u64) {
    if self.readings_ns.len() >= PUT_LATENCY_WINDOW {
      self.readings_ns.pop_front();
    }
    self.readings_ns.push_back(latency_ns);
  }

  /// How many readings the window holds.
  pub fn len(&self) -> usize {
    self.readings_ns.len()
  }

  /// Whether no acknowledgement has been timed yet.
  pub fn is_empty(&self) -> bool {
    self.readings_ns.is_empty()
  }

  /// The p95 of the window's readings (nearest rank, the machine crate's percentile law), or `None`
  /// before any reading.
  pub fn p95_ns(&self) -> Option<u64> {
    slates_machine::stats::Sample::new(self.readings_ns.iter().copied().collect())
      .percentile(slates_machine::stats::Percentile::P95)
  }
}

/// The measured put-failure rate of one owner shard's content class (§4.8 "Derived constants": "healer
/// cadence from the measured put-failure rate"): content rounds that ended placed against rounds that
/// ended short. Lifetime counters (monotonic, never reset) — the rate is their ratio, so a long quiet
/// stretch dilutes an old burst of failures exactly as the design's "measured rate" intends.
#[derive(Debug, Default)]
pub struct PutOutcomes {
  /// Content rounds whose placement reached the quorum.
  pub placed: u64,
  /// Content rounds that ended short of quorum — uncertain at the deadline, or every holder short.
  pub short: u64,
}

impl PutOutcomes {
  /// Records one content round's outcome.
  pub fn record(&mut self, placed: bool) {
    if placed {
      self.placed = self.placed.saturating_add(1);
    } else {
      self.short = self.short.saturating_add(1);
    }
  }
}

/// Where the healer is in its walk over this node's placed content (§4.10 "the healer"): the volume it
/// re-offers next (in volume-id order; `None` restarts the walk from the first), and when it last
/// re-offered one, so one snapshot is walked per healer period.
#[derive(Debug, Default)]
pub struct HealerCursor {
  /// The volume after which the next walk step starts (`None`: from the beginning).
  pub after: Option<DbVolumeId>,
  /// When the healer last re-offered a snapshot (the shard clock), or `None` before its first step.
  pub last_step_ns: Option<u64>,
}

/// Derived: the healer walks one placed snapshot per this many coordinator periods when **no** put has
/// ever failed — the slowest cadence, a hundred periods (ten seconds at the default beat), so an idle
/// fleet spends one offer round trip per placed snapshot per hundred periods on verifying what it placed;
/// as the measured put-failure rate rises the cadence quickens toward one snapshot per period
/// ([`heal_period_ns`]). Anchored to the put-latency window's seal count ([`PUT_LATENCY_SEALS`]): the
/// healer covers a window of seals in a window of periods.
const HEAL_PERIODS_AT_REST: u64 = PUT_LATENCY_SEALS as u64;

/// The healer's period (§4.8 "Derived constants": "healer cadence from the measured put-failure rate"):
/// `HEARTBEAT_NS × HEAL_PERIODS_AT_REST × placed / (placed + short × HEAL_PERIODS_AT_REST)` — one
/// snapshot per [`HEAL_PERIODS_AT_REST`] periods while every round places, tightening in proportion to
/// the share of rounds that ended short until, when short rounds are as common as placed ones, it is one
/// snapshot per period, the fastest the coordinator runs. Before any round (a fresh boot, the laptop) it
/// is the at-rest cadence: nothing has failed. Integer arithmetic; never below one period.
fn heal_period_ns(outcomes: &PutOutcomes) -> u64 {
  let placed = outcomes.placed.max(1);
  let weighted_short = outcomes.short.saturating_mul(HEAL_PERIODS_AT_REST);
  let periods = HEAL_PERIODS_AT_REST
    .saturating_mul(placed)
    .checked_div(placed.saturating_add(weighted_short))
    .unwrap_or(HEAL_PERIODS_AT_REST)
    .max(1);
  HEARTBEAT_NS.saturating_mul(periods)
}

/// The hedge delay (§4.8 "Derived constants": "hedge delay = measured p95 put latency per class"): how long
/// a seal's first content round is given before the remaining candidates are hedged — the measured p95 of
/// the content class's put latency on this owner shard. Before any reading (the first seal of a boot, and
/// the laptop, where no round runs) it is one period ([`HEARTBEAT_NS`]) — the cadence the rounds are driven
/// at, so the first hedge waits exactly one round, the same code path with an empty window (R8). Dean &
/// Barroso's hedged requests: "send the request to a replica … after the request has been outstanding for
/// longer than the 95th-percentile expected latency for this class of requests" (CACM 2013).
fn hedge_delay_ns(latency: &LatencyWindow) -> u64 {
  latency.p95_ns().unwrap_or(HEARTBEAT_NS)
}

/// The budget of one content round: its collection **stops at the hedge delay** ([`hedge_delay_ns`]) so the
/// coordinator is free to hedge the remaining candidates the moment the first round has been outstanding
/// for the measured p95 — a hedge is a second request *while the first is still in flight*, which a round
/// awaited to the full span could never make — while every holder task keeps the full span
/// ([`consensus_budget`]'s, the coherent cap) so a slow holder's acknowledgement still arrives, as a
/// straggler, and is folded into the seal ([`LateReplies::Content`]) rather than lost. Derived: base
/// deadline = the hedge delay; one extension of `span − hedge delay` (granted only to a round still
/// gathering acknowledgements at the delay — the ratified late-work rule — so a round with none at the p95
/// expires there and is hedged); the stall window = the hedge delay itself (an acknowledgement within one
/// delay is progress); poll = the collection cadence ([`POLL_PER_PERIOD`]).
fn content_budget(latency: &LatencyWindow, round: CommitBudget) -> CommitBudget {
  content_budget_at(hedge_delay_ns(latency), round)
}

/// [`content_budget`] at a given hedge delay (a mirror round's, floored at its path).
fn content_budget_at(hedge_delay: u64, round: CommitBudget) -> CommitBudget {
  let span = round.max_deadline_ns().max(hedge_delay);
  CommitBudget::with_extension(
    hedge_delay,
    (HEARTBEAT_NS / POLL_PER_PERIOD).max(1),
    1,
    1,
    span.saturating_sub(hedge_delay),
    1,
    hedge_delay,
  )
}

/// Derived: how long a heal may run from its first round before it ends unfinished ([`content_work`]): its content
/// round's whole span (`budget`'s longest deadline, the bound each holder task of a round waits for its reply under) and
/// one coordinator period, in which a reply that came at the span's end is folded (the coordinator settles its rounds'
/// stragglers once a period). So every acknowledgement of the first round is folded into the job before the job can
/// end, a repair among them, while a heal that cannot finish (every put refused, a holder unreachable) still ends and a
/// later healer step takes the volume up again (banned item 8). The span follows the measured round-trip tails the
/// round budget is derived from, so the bound stretches with load as the rounds do. live-fixes' cut bounded a heal by
/// the membership horizon (`lease::horizon_ns`, 900 ms at the default beat), a liveness bound shorter than the span its
/// own rounds' replies may take (1.1 s at least).
fn heal_bound_ns(budget: &CommitBudget) -> u64 {
  budget.max_deadline_ns().saturating_add(HEARTBEAT_NS)
}

/// Folds one holder's bound content acknowledgement into `job` — the same merge whether it arrived in the
/// round that dispatched it or later, as a straggler ([`LateReplies::Content`]); the placement is the
/// distinct acknowledging candidates, so a repeat is inert. Returns whether the fold is a **repair**: the first
/// acknowledgement of a holder a restoring heal refilled ([`SealJob::refilled`]), so each repaired holder counts
/// once whichever path its acknowledgement took. Until 2026-11-17 only an acknowledgement inside its round counted,
/// and a refill that outlasted the round's hedge delay — a large volume's, or any under load — restored the holder
/// with no repair counted.
fn fold_content_ack(job: &mut SealJob, holder: HostId) -> bool {
  if job.content.acked.contains(&holder) {
    return false;
  }
  job.content.acked.push(holder);
  restoring(job) && job.refilled.contains(&holder)
}

/// The content put `object`'s seal calls for this period — its archive moved out for the dispatch — or
/// `None` once the content is placed (the head naming it then ships through `unplaced_heads`), while the
/// archive is out on a put, or while the **first round is still within the hedge delay**: the hedge round
/// to the remaining candidates goes out only once the first round has been outstanding for longer than the
/// measured p95 put latency ([`hedge_delay_ns`]) — the design's trigger, replacing the period the rounds
/// happen to be driven at (§4.8 "hedged to the remaining candidates after the measured p95 put latency").
fn content_work(
  state: &mut ShardState,
  track: Track,
  object: ObjectId,
  quorum: Quorum,
  round: CommitBudget,
) -> Option<ContentWork> {
  // Each track is hedged and judged stalled from its own measured puts, and a mirror round never sooner than the
  // slowest measured round-trip tail to its candidates: until 2026-10-09 both tracks used the home window, whose p95
  // on a local network is far inside a round trip to another region, so every mirror round was judged stalled at its
  // offer and the mirror never placed (350 rounds, 0 acknowledgements, on a 200 ms path).
  let (hedge_delay, budget) = match track {
    Track::Home => (
      hedge_delay_ns(&state.put_latency),
      content_budget(&state.put_latency, round),
    ),
    Track::Mirror => {
      let floor = state
        .mirror_seals
        .get(&object)
        .map(|job| {
          job
            .content
            .candidates
            .iter()
            .filter_map(|host| state.peer_paths.get(host).and_then(PathRtt::tail_ns))
            .max()
            .unwrap_or(0)
        })
        .unwrap_or(0);
      let hedge = hedge_delay_ns(&state.mirror_put_latency).max(floor);
      (hedge, content_budget_at(hedge, round))
    }
  };
  let now = state.clock.monotonic_ns();
  let shard = state.shard;
  let job = seal_jobs(state, track).get_mut(&object)?;
  // A job whose content is where it must be waits for its record, however long it took: a late acknowledgement can
  // complete a heal just past its bound, and a replace dropped then would leave its head unpublished.
  if seal_complete(job, quorum) {
    return None;
  }
  // Every heal is bounded from its first round ([`heal_bound_ns`]), judged between rounds (its archive home), so no
  // round in flight loses its job. A heal past it ends unfinished and is counted: the job is dropped, so a later healer
  // step comes back to the volume rather than skipping it while a job that cannot finish stays open (a replace whose
  // every put was refused stayed open for good, 2026-11-09; A-276).
  let overdue = job.healing
    && job.archive.is_some()
    && job
      .first_round_at_ns
      .is_some_and(|first| now.saturating_sub(first) > heal_bound_ns(&budget));
  if overdue {
    seal_jobs(state, track).remove(&object);
    state.count(HEAL_UNFINISHED, 1);
    return None;
  }
  // A restore offers to every holder it restores at once: each is owed the content, not just the first `f` of them.
  let hedged = restoring(job)
    || match job.first_round_at_ns {
      Some(first_round_at) if now.saturating_sub(first_round_at) < hedge_delay => return None,
      Some(_) => true,
      None => false,
    };
  let archive = job.archive.take()?;
  Some(ContentWork {
    track,
    shard,
    object,
    snapshot: job.snapshot,
    sequence: job.put_sequence,
    archive,
    candidates: job.content.candidates.clone(),
    acked: job.content.acked.clone(),
    quorum,
    round: job.rounds,
    hedged,
    budget,
  })
}

/// The holders one content round goes to (§4.8 mechanism 1: "content is sent to `f + 1` candidates first,
/// hedged to the remaining candidates after the measured p95 put latency"): until the first round has been
/// outstanding for the hedge delay, the first `f` remote candidates in rendezvous order (the owner is the
/// `f + 1`-th copy); once it has — `hedged` — every remaining candidate. Pure, so the rule is tested at N=1:
/// the trigger is the clock, never the count of rounds that placed.
fn hedge_targets(hedged: bool, remote: Vec<HostId>, first_round: usize) -> Vec<HostId> {
  if hedged {
    remote
  } else {
    remote.into_iter().take(first_round).collect()
  }
}

/// Runs one content round for a seal (§4.8 mechanism 1: "content is sent to `f + 1` candidates first,
/// hedged to the remaining candidates after the measured p95 put latency"): the first round goes to the
/// first `f` remote candidates in rendezvous order (the owner is the `f + 1`-th copy), every later round
/// hedges to all remaining candidates — held back by [`content_work`] until the first round has been
/// outstanding for the measured p95 ([`hedge_delay_ns`]). The holders' sessions are borrowed for the put
/// and returned; the acknowledging set is merged into the seal whatever the round's outcome (each
/// acknowledgement is a distinct holder's verified, durable hold), every acknowledgement's latency is
/// recorded into the owner shard's put-latency window (the readings the next hedge is sized from), the
/// first round's dispatch time is noted, and the archive is put back for the next round. Returns the
/// dispatch to settle.
async fn put_seal_content(origin: u16, local: HostId, work: ContentWork) -> Option<Dispatch> {
  let budget = work.budget;
  // The first round reaches `f + 1` copies: where the owner is a candidate (a home seal) it is one of them, so `f`
  // remote candidates; where it is not (the mirror region, a move's handoff, A-291) it holds none, so `f + 1`.
  let owner_copies = usize::from(!work.candidates.contains(&local));
  let hedge = usize::try_from(work.quorum.f)
    .unwrap_or(0)
    .saturating_add(owner_copies);
  let remote: Vec<HostId> = work
    .candidates
    .iter()
    .copied()
    .filter(|host| *host != local && !work.acked.contains(host))
    .collect();
  let targets = hedge_targets(work.hedged, remote, hedge);
  let holders = take_sessions("fleet.lent_to.put_seal_content", |host| {
    targets.contains(&host)
  });
  let dispatched_ns = futures::now_ns();
  let manifest = work.archive.manifest_identity();
  let (placement, latencies_ns, refilled, dispatch) = if holders.is_empty() {
    // No session to any target was free (none held, or every one lent): the round sends nothing, counted per track, so
    // a placement that never progresses names why.
    let counter = match work.track {
      Track::Home => SEAL_NO_HOLDER_SESSION,
      Track::Mirror => MIRROR_NO_HOLDER_SESSION,
    };
    let _ = state::with_state_counted(|s| s.count(counter, 1));
    (None, Vec::new(), Vec::new(), None)
  } else {
    let taken: Vec<HostId> = holders.iter().map(|(host, _)| *host).collect();
    let placed = put_content(
      local,
      &work.archive,
      work.object,
      work.sequence,
      &work.candidates,
      work.quorum,
      holders,
      budget,
    )
    .await;
    // A holder still in flight when the collection stopped at the hedge delay answers later: its
    // acknowledgement is folded into this seal, not dropped, so hedging never costs a slow holder its hold.
    let dispatch = Dispatch::new(
      taken,
      &placed.reusable,
      placed.stragglers,
      LateReplies::Content {
        track: work.track,
        shard: work.shard,
        object: work.object,
        snapshot: work.snapshot,
        sequence: work.sequence,
        manifest,
        dispatched_ns,
      },
    );
    return_sessions(placed.reusable);
    if work.track == Track::Mirror {
      let acks = u64::try_from(placed.latencies_ns.len()).unwrap_or(u64::MAX);
      let failed = matches!(
        placed.outcome,
        Err(ref error) if !matches!(error, ClusterError::Uncertain { .. } | ClusterError::NotPlaced { .. })
      );
      let _ = state::with_state_counted(|s| {
        s.count(MIRROR_ACKS, acks);
        if failed {
          s.count(MIRROR_ROUND_FAILED, 1);
        }
      });
    }
    let placement = match placed.outcome {
      Ok(placement) => Some(placement),
      Err(ClusterError::Uncertain { placement } | ClusterError::NotPlaced { placement }) => {
        Some(placement)
      }
      Err(_) => None,
    };
    (
      placement,
      placed.latencies_ns,
      placed.refilled,
      Some(dispatch),
    )
  };
  // The seal lives on the owner shard: put the archive back, merge the round's acknowledgements, record
  // their latencies (the content class's readings the next hedge is sized from), and note when the first
  // round went out (the hedge is held until it has been outstanding for the measured p95).
  let ContentWork {
    track,
    shard,
    object,
    snapshot,
    sequence,
    archive,
    round,
    quorum,
    ..
  } = work;
  // How long the round has been out, on this shard's own clock: the owner stamps the dispatch on its clock from it.
  // A runtime driver's clock counts from that shard's own start, while the owner judges the hedge delay and a heal's
  // bound by its `ShardState::clock` (`content_work`), the host's monotonic clock. Until 2026-11-17 this shard's driver
  // reading was stamped as it was, the owner's clock read every first round as long past, so the hedge round was never
  // held for the measured p95, and a heal that did not finish in its first round ended at the next period.
  let outstanding_ns = futures::now_ns().saturating_sub(dispatched_ns);
  crate::xshard::run_on_counted(origin, shard, move |s| {
    let dispatched_on_owner_ns = s.clock.monotonic_ns().saturating_sub(outstanding_ns);
    for (_, latency_ns) in &latencies_ns {
      match track {
        Track::Home => s.put_latency.record(*latency_ns),
        Track::Mirror => s.mirror_put_latency.record(*latency_ns),
      }
    }
    // The job is borrowed alone; what the round counts is written after the borrow ends.
    let Some(job) = seal_jobs(s, track).get_mut(&object) else {
      return; // The seal was superseded meanwhile; its archive is dropped with it.
    };
    // The round folds only into the job it was put for: a heal of the same snapshot can complete, be recorded and be
    // followed by another at a different put sequence while a round is out, and its acknowledgements bind the old one.
    if job.snapshot != snapshot || job.put_sequence != sequence {
      return;
    }
    job.archive = Some(archive);
    if round == 0 && job.first_round_at_ns.is_none() {
      job.first_round_at_ns = Some(dispatched_on_owner_ns);
    }
    let Some(fold) = fold_round(job, placement, &refilled, quorum) else {
      return;
    };
    s.repairs = s.repairs.saturating_add(fold.repaired);
    s.put_outcomes.record(fold.placed);
    if fold.outlasted > 0 {
      s.count(HEAL_REFILL_OUTLASTED_ROUND, fold.outlasted);
    }
  });
  dispatch
}

/// What folding one content round's outcome into its seal job found ([`fold_round`]).
struct RoundFold {
  /// Holders whose first acknowledgement completed a repair ([`fold_content_ack`]).
  repaired: u64,
  /// Holders the round, a restoring one, put bytes to whose acknowledgement it did not collect.
  outlasted: u64,
  /// Whether the job's content is now where the job must leave it ([`seal_complete`]).
  placed: bool,
}

/// Folds one content round's outcome into its seal job on the owner shard. A restoring round that shipped bytes to a
/// holder is repairing it: that holder had lost content the placement records it as holding (§4.10 "repairs only
/// differing subtrees"), so it is remembered on the job ([`SealJob::refilled`]) and its acknowledgement counts the
/// repair whenever it is folded, in this round or as a straggler's later. Then the round's acknowledgements are
/// folded. `None` when the round returned no placement: nothing was sent, or the dispatch failed.
fn fold_round(
  job: &mut SealJob,
  placement: Option<Placement>,
  refilled: &[HostId],
  quorum: Quorum,
) -> Option<RoundFold> {
  if restoring(job) {
    for host in refilled {
      if !job.refilled.contains(host) {
        job.refilled.push(*host);
      }
    }
  }
  let placement = placement?;
  job.rounds = job.rounds.saturating_add(1);
  let outlasted = if restoring(job) {
    refilled
      .iter()
      .filter(|host| !placement.acked.contains(host))
      .count()
  } else {
    0
  };
  let mut repaired: u64 = 0;
  for host in placement.acked {
    if fold_content_ack(job, host) {
      repaired = repaired.saturating_add(1);
    }
  }
  Some(RoundFold {
    repaired,
    outlasted: u64::try_from(outlasted).unwrap_or(u64::MAX),
    placed: seal_complete(job, quorum),
  })
}

/// Records durably each seal whose content **and** head have both placed (§4.10; AC-8.2 "no head record names
/// content that is not placed"): the snapshot's manifest identity (`SnapshotIdentified`) and its region
/// placement (`SnapshotPlaced`, the content holders), the facts `status` and `await placed(region)` answer
/// from and a restart resumes from. The seal is then dropped — its archive with it; the volume holds the
/// bytes, and its holders serve them by identity. A replacing heal's placement is recorded with the head republished
/// at the sequence it put the content at, in one transaction ([`record_replacement`]). A restoring heal records nothing: it is dropped once every holder the
/// placement names holds the content, since the placement and the head already name exactly them, and recording
/// the set again at the head's sequence could only change the head's value there. Runs on the owner shard, where
/// the seal and the volume's database partition live.
fn record_placed_seals(state: &mut ShardState) {
  let quorum = state.fleet.configuration().quorum;
  let now = state.clock.monotonic_ns();
  let restored: Vec<ObjectId> = state
    .seals
    .iter()
    .filter(|(_, job)| restoring(job) && job.manifest.is_some() && seal_complete(job, quorum))
    .map(|(object, _)| *object)
    .collect();
  for object in restored {
    state.seals.remove(&object);
    state.count(HEAL_VERIFIED, 1);
  }
  let done: Vec<PlacedSeal> = state
    .seals
    .iter()
    .filter_map(|(object, job)| {
      let identity = job.manifest?;
      if restoring(job) || !job.content.placed(quorum) {
        return None;
      }
      let head = state.placed_heads.get(object)?;
      if head.sequence != job.sequence || !head.placed_under(state.fleet.configuration(), *object) {
        return None;
      }
      let region = job.content.acked.iter().map(|host| host.0).collect();
      Some(PlacedSeal {
        object: *object,
        snapshot: job.snapshot,
        identity,
        region,
        republish: job.republish.then_some(job.put_sequence),
      })
    })
    .collect();
  for PlacedSeal {
    object,
    snapshot,
    identity,
    region,
    republish,
  } in done
  {
    let volume = DbVolumeId { bytes: object.0 };
    // A mirror placement already recorded is kept: a home seal that completes again for a mirrored snapshot (the
    // healer's re-offer) wrote `mirror: None` over it until 2026-10-09, which started a second mirror job for the
    // snapshot, whose shipment named other content holders at the same position and was refused for good.
    let mirror = match state.db.partition().snapshot(volume, snapshot) {
      Some(record) => match &record.placed {
        PlacementState::Placed {
          mirror: Some(mirror),
          ..
        } => Some(mirror.clone()),
        _ => None,
      },
      None => None,
    };
    if mirror.is_some() {
      state.count(MIRROR_KEPT_ON_HOME_RECORD, 1);
    }
    let identified = Op::SnapshotIdentified {
      volume,
      id: snapshot,
      identity,
    };
    let placed = Op::SnapshotPlaced {
      volume,
      id: snapshot,
      placed: PlacementState::Placed { region, mirror },
    };
    match republish {
      Some(epoch) => {
        let republished = Op::VolumeHeadAdvanced {
          id: volume,
          head: snapshot,
          epoch,
        };
        let counter = if record_replacement(state, [identified, placed, republished], now) {
          HEAL_REPUBLISHED
        } else {
          HEAL_REPUBLISH_REFUSED
        };
        state.count(counter, 1);
      }
      None => {
        for op in [&identified, &placed] {
          if state.db.mutate(&mut state.segment, op, now).is_err() {
            break; // The snapshot is gone (destroyed meanwhile): nothing to record; the seal is dropped.
          }
        }
      }
    }
    state.seals.remove(&object);
  }
}

/// A seal whose content and head have both placed, as [`record_placed_seals`] records it.
struct PlacedSeal {
  object: ObjectId,
  snapshot: DbSnapshotId,
  identity: [u8; 32],
  region: Vec<u64>,
  /// For a replacing heal: the sequence its content was put at, where the head is republished naming `region`.
  republish: Option<u64>,
}

/// Records a replacing heal's placement and republishes the head over it, as one durable record (§4.10 "the healer";
/// A-276): `ops` are the snapshot's identity, its placement on the holders that took the content at the next sequence,
/// and the volume's head advanced to that sequence with the same snapshot (`Op::VolumeHeadAdvanced`, as a lost head's
/// advance does), so the next record period ships a head there naming those holders. The head is never given a new
/// value at its old position: a holder refuses that, rightly. So nothing is recorded unless the volume's head is still
/// the snapshot and the advance is to the sequence just after its own: a newer head names its own holders. And the
/// three are one transaction, every operation checked before it opens (inside one, a refused operation would leave the
/// ones before it applied, to be logged at the commit): recorded apart, a refusal or a crash between the placement and
/// the advance left the placement naming the new holders at the old sequence, a head value there that differs from
/// the one its holders hold, which they refuse for good. Whether the transaction committed; a commit that fails is
/// rolled back whole (`Db::commit`).
fn record_replacement(state: &mut ShardState, ops: [Op; 3], now: u64) -> bool {
  let follows = ops.iter().any(|op| match op {
    Op::VolumeHeadAdvanced { id, head, epoch } => state
      .db
      .partition()
      .volume(*id)
      .is_some_and(|record| record.head == *head && record.epoch.checked_add(1) == Some(*epoch)),
    _ => false,
  });
  if !follows
    || ops
      .iter()
      .any(|op| state.db.partition().check(op, now).is_err())
  {
    return false;
  }
  state.db.begin();
  let applied = ops
    .iter()
    .all(|op| state.db.mutate(&mut state.segment, op, now).is_ok());
  // Committed whatever was applied, since an open transaction would take the next caller's operations into it; every
  // operation passed its check above and an apply after a passing check is unconditional, so all three were.
  let committed = state.db.commit(&mut state.segment).is_ok();
  applied && committed
}

/// The status refusal count under which the fleet loop records a taken-over volume it could not
/// materialize (an archive it could not hold, a name already taken locally, admission refused): the
/// takeover's head is placed and served, but its content is not yet served, and the attempt repeats each
/// period while the condition holds — counted so an operator sees it.
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
const MATERIALIZE_REFUSED: &str = "fleet.materialize";

/// Serves the content of each taken-over object whose head this node adopted (§4.8 "adopts the newest
/// records, and serves"; §4.10): if the manifest the head names is held whole (this node was a content
/// candidate) the volume is materialized from it; otherwise the archive is fetched by identity from a
/// recorded content holder over its live session (§4.10 "fetches … by identity from a recorded holder"),
/// held once verified, and then materialized. An object whose content is not yet reachable stays pending
/// and is retried next period.
///
/// **Bounded** (2026-11-18, as [`materialize_pending_greens`] is; CLAUDE.md §2 item 8): each pending volume is
/// attempted once a period, and past [`takeover_attempts`] attempts without progress it is given up on
/// ([`give_up_taken_over`]): its owner shard records it unrecovered, naming what the last attempt waited on
/// ([`VolumeWait`]), so every verb on it answers `Refusal::NotRecovered { cause: TakeoverIncomplete }`. Progress is a
/// fetch that leaves more of the head's content staged here than ever before (`state::PendingVolume::record_staged`),
/// so a large volume whose fetch is cut by its deadline every period is given up only once the fetch stops advancing;
/// and, for a wait on its catalog register while the takeover round that adopted it still pages, an advance of that
/// round ([`paging_round`]), so a volume whose catalog a large host's round brings late is given up only once the round
/// stands still. Before, each wait (`fleet.materialize.waits.*`, a fetch that never completed, a refused or unanswered
/// materialization) retried every period without end.
async fn materialize_pending(origin: u16, local: HostId, budget: CommitBudget) {
  let pending: Vec<(ObjectId, HeadValue, Option<HostId>)> = state::with_state(|s| {
    s.pending_materializations
      .iter()
      .map(|(object, pending)| (*object, pending.head.clone(), pending.departed))
      .collect()
  })
  .unwrap_or_default();
  let paging = state::with_state(|s| crate::takeover::rounds_still_paging(s)).unwrap_or_default();
  for (object, head, departed) in pending {
    // A head that names content waits until this node holds that content; a contentless volume (a scratch
    // volume never sealed) has nothing to fetch and materializes from its adopted head and catalog registers
    // alone. Both then go to `materialize`, which builds the servable db entry. A head-only volume was dropped
    // here unmaterialized before 2026-10-11, so a taken-over scratch volume stayed `owned_unmaterialized`
    // forever (docs/bugs/2026-10-11-taken-over-scratch-volumes-never-become-servable.md).
    let attempted = match head.manifest {
      Some(manifest) => {
        let held = state::with_state(|s| s.held_content.holds_manifest(object, &manifest))
          .unwrap_or(false)
          || fetch_into_hold(object, &head, manifest, budget).await;
        if held {
          materialize(origin, object, head).await
        } else {
          // Not whole yet: what was fetched stays staged for the next period (AUD-29-55), and more of it staged than
          // ever before is progress.
          state::with_state(|s| {
            let staged = s.held_content.staged_of(object, &manifest).unwrap_or(0);
            if let Some(pending) = s.pending_materializations.get_mut(&object) {
              pending.record_staged(staged);
            }
          });
          Err(VolumeWait::Content)
        }
      }
      None => materialize(origin, object, head).await,
    };
    let Err(waited) = attempted else {
      continue;
    };
    let round = paging_round(waited.waits_on_the_round(), departed, &paging);
    let given_up = state::with_state(|s| {
      let bound = takeover_attempts(s.failover_ns);
      let pending = s.pending_materializations.get_mut(&object)?;
      if round.is_some_and(|advances| pending.record_round(advances)) {
        // Its round moved on toward the register it waits for: progress, its attempts start again.
        return None;
      }
      pending.attempts = pending.attempts.saturating_add(1);
      (pending.attempts >= bound).then_some(pending.attempts)
    })
    .flatten();
    if let Some(attempts) = given_up {
      give_up_taken_over(
        origin,
        local,
        (object, TakenOver::Volume),
        waited.named(),
        attempts,
      )
      .await;
    }
  }
}

/// Counter: chunk requests a takeover fetch hedged to a further recorded holder (A-91).
/// Format: a counter name in the daemon's status report.
const FETCH_HEDGED: &str = "fleet.fetch.hedged";
/// Counter: requests an idle recorded holder took from another's outstanding work in a takeover fetch (A-91).
/// Format: a counter name in the daemon's status report.
const FETCH_STOLEN: &str = "fleet.fetch.stolen";
/// Counter: recorded holders a takeover fetch dropped (session lost or a false answer), their chunks moved on (A-91).
/// Format: a counter name in the daemon's status report.
const FETCH_HOLDER_FAILED: &str = "fleet.fetch.holder_failed";

/// Fetches `object`'s content `manifest` from a recorded holder into this node's hold (§4.10 "fetches … by
/// identity from a recorded holder"; AUD-29-55): the manifest, staged as placed for the adopted head's
/// sequence (the retention rule keeps it while the head names this node), then each chunk the stage lacks,
/// one per exchange, verified and kept as it arrives, and the stage completed once whole. A fetch cut by the
/// deadline or a lost session keeps every verified chunk, so the next period asks only for the rest. Returns
/// whether the content is now held whole.
async fn fetch_into_hold(
  object: ObjectId,
  head: &HeadValue,
  manifest: [u8; 32],
  budget: CommitBudget,
) -> bool {
  fetch_manifest_into_hold(
    object,
    &head.holders(),
    |s| s.placed_heads.get(&object).map_or(0, |head| head.sequence),
    manifest,
    budget,
  )
  .await
}

/// [`fetch_into_hold`] from any recorded `holders` of `object`'s content `manifest`, staged as placed for the sequence
/// `placed_for` reads when the manifest arrives: a taken-over volume's adopted head (its head's own holders), or a
/// taken-over green's version, whose inputs the retention rule keeps by that version's ledger position
/// ([`fetch_green_inputs`]).
async fn fetch_manifest_into_hold(
  object: ObjectId,
  holders: &[HostId],
  placed_for: impl Fn(&ShardState) -> u64,
  manifest: [u8; 32],
  budget: CommitBudget,
) -> bool {
  // Every reachable recorded holder: the manifest and the chunks are ranked, hedged and tied across all of them
  // (A-91; §4.10 "hedged fetches by identity from the recorded holders"), so one slow or lost holder is masked.
  let sessions = take_sessions("fleet.lent_to.fetch_into_hold", |host| {
    holders.contains(&host)
  });
  if sessions.is_empty() {
    // No recorded holder reachable this period (none with a session here, or every such session out on loan).
    count_refusal(FETCH_NO_HOLDER_SESSION);
    return false;
  }
  let hedge_after_ns =
    state::with_state(|s| hedge_delay_ns(&s.fetch_latency)).unwrap_or(HEARTBEAT_NS);
  let timing = slates_cluster::content::FetchTiming {
    deadline_ns: budget.max_deadline_ns(),
    hedge_after_ns,
    poll_ns: budget.poll_interval_ns,
  };
  let (mut already_whole, mut stage_refused) = (false, false);
  let fetched = fetch(
    sessions,
    (object, manifest),
    timing,
    |found| {
      let staged = state::with_state(|s| {
        let placed = slates_cluster::content::Placed {
          sequence: placed_for(s),
        };
        past_the_pressure_hold(s, |s| {
          s.held_content.stage_fetched(
            &mut crate::content_holder::hold_space(&mut s.store),
            object,
            placed,
            found,
          )
        })
      });
      match staged {
        Some(Ok(None)) => {
          already_whole = true;
          Some(Vec::new())
        }
        Some(Ok(Some(wanted))) => Some(wanted),
        Some(Err(_)) | None => {
          stage_refused = true;
          None
        }
      }
    },
    |chunk| {
      state::with_state(|s| {
        past_the_pressure_hold(s, |s| {
          s.held_content.stage_piece(
            &mut crate::content_holder::hold_space(&mut s.store),
            object,
            &manifest,
            chunk,
          )
        })
        .is_ok()
      })
      .unwrap_or(false)
    },
  )
  .await;
  return_sessions(fetched.sessions);
  state::with_state(|s| {
    for latency in &fetched.latencies_ns {
      s.fetch_latency.record(*latency);
    }
    for (name, count) in [
      (FETCH_HEDGED, fetched.hedges),
      (FETCH_STOLEN, fetched.steals),
      (FETCH_HOLDER_FAILED, fetched.failed_holders),
    ] {
      if count > 0 {
        s.count(name, count);
      }
    }
  });
  if stage_refused {
    count_refusal(MATERIALIZE_REFUSED);
    count_refusal(FETCH_STAGE_REFUSED);
    return false;
  }
  if already_whole {
    return true;
  }
  if !fetched.complete {
    // Not whole yet: what was fetched stays staged for the next period (AUD-29-55).
    count_refusal(FETCH_INCOMPLETE);
    return false;
  }
  let completed = state::with_state(|s| {
    past_the_pressure_hold(s, |s| {
      s.held_content
        .complete_stage(&mut crate::content_holder::hold_space(&mut s.store), object)
    })
    .is_ok()
  })
  .unwrap_or(false);
  if !completed {
    count_refusal(FETCH_COMPLETE_REFUSED);
  }
  completed
}

/// Counter: a takeover fetch found no session to any recorded holder of the head this period (none kept here, or
/// every one out on loan); it is asked again next period.
/// Format: a counter name in the daemon's status report.
const FETCH_NO_HOLDER_SESSION: &str = "fleet.fetch.no_holder_session";
/// Counter: a takeover fetch's manifest could not be staged in the hold (no capacity, or a refused stage).
/// Format: a counter name in the daemon's status report.
const FETCH_STAGE_REFUSED: &str = "fleet.fetch.stage_refused";
/// Counter: a takeover fetch ended its period without the whole content; what it fetched stays staged.
/// Format: a counter name in the daemon's status report.
const FETCH_INCOMPLETE: &str = "fleet.fetch.incomplete";
/// Counter: a takeover fetch held every chunk, but the hold refused to complete its stage.
/// Format: a counter name in the daemon's status report.
const FETCH_COMPLETE_REFUSED: &str = "fleet.fetch.complete_refused";

/// One period of everything this node adopts: each takeover it owes a confirmation of (one batched phase-one round per
/// retired host across its recovery neighbourhoods, §4.8 "Promotion and takeover"; the holds and rounds arrive over
/// this shard's sessions), a region promoted to this one (`crate::mirror`: the council leader's phase one and
/// assignments, then this node's own assigned adoptions), and the content of each adopted head, served from what this
/// node holds or a recorded holder on the shard the id routes to.
async fn adopt_and_serve(
  origin: u16,
  local: HostId,
  budget: CommitBudget,
  in_flight: &mut Vec<Dispatch>,
) {
  in_flight.extend(crate::takeover::drive_takeovers(local, budget).await);
  // Each volume the configuration moved to this node: phase one over its recorded cohort, adoption, then its seal and
  // the report that it is adopted (A-291).
  crate::moves::drive_adoptions(origin, local, budget, in_flight).await;
  in_flight.extend(crate::mirror::drive_promotion(local, budget).await);
  crate::mirror::adopt_assigned(local, budget, in_flight).await;
  // The claims of the live logs this node holds (§4.2, g229), settled before anything is made from one: a copy let go
  // releases its claim, a warm restart's copies have theirs admitted again.
  crate::live_ship::reconcile_claims(origin).await;
  materialize_adopted_objects(origin, local, budget, in_flight).await;
}

/// One period's materialization of everything this node took over: the plain volumes whose heads it
/// adopted ([`materialize_pending`]), then the greens whose newest merge records it adopted
/// ([`materialize_pending_greens`]).
async fn materialize_adopted_objects(
  origin: u16,
  local: HostId,
  budget: CommitBudget,
  in_flight: &mut Vec<Dispatch>,
) {
  adopt_pending_tombstones(origin).await;
  materialize_pending(origin, local, budget).await;
  materialize_pending_greens(origin, local, budget, in_flight).await;
  rebuild_adopted_repositories(origin).await;
}

/// Rebuilds each repository this node took over once its logs and greens are in (A-636 part 2c-2,
/// [`crate::takeover::repositories_ready`]), on the shard its id routes to ([`crate::takeover::rebuild_repository`]):
/// one that stands there, or that the database refused (logged and counted there), is pending no more; one whose
/// partition has no shard here, or whose shard does not answer within the period, is attempted again next period.
async fn rebuild_adopted_repositories(origin: u16) {
  let ready: Vec<(crate::takeover::ReadyRepository, Option<u16>)> = state::with_state(|s| {
    crate::takeover::repositories_ready(s)
      .into_iter()
      .map(|ready| {
        let partition = verbs::owner_of(slates_ipc::protocol::VolumeId { bytes: ready.0.0 });
        let target = s.shards.get(usize::from(partition)).copied();
        (ready, target)
      })
      .collect()
  })
  .unwrap_or_default();
  for ((object, record, parts), target) in ready {
    let Some(target) = target else {
      continue;
    };
    let answered = call_within(
      origin,
      target,
      move |s| crate::takeover::rebuild_repository(s, record, parts),
      HEARTBEAT_NS,
    )
    .await;
    if answered.is_some() {
      state::with_state(|s| s.pending_repositories.remove(&object));
    }
  }
}

/// Records each adopted tombstone (AUD-29-43) on the shard the destroyed volume's id routes to, with the
/// adopted placement, so that shard's record plane ships the tombstone's remaining stages to the successor's
/// candidates and retires it. A tombstone the shard already holds is already owed there; one the shard refuses (the
/// partition's volume capacity is full, or a live volume already holds the id), one it does not answer within the
/// period, and one whose partition has no shard here are counted by that reason and attempted again next period.
///
/// **Bounded** (2026-11-18, as the taken-over volumes and greens are; CLAUDE.md §2 item 8): past
/// [`takeover_attempts`] it is given up on (`state::PendingTombstone::count_attempt`): counted
/// ([`TOMBSTONE_ADOPT_GIVEN_UP`]), logged with the reason, and attempted no more. Its tombstone record is already
/// placed under this node at the quorum (the adoption re-committed it), so the destroyed volume is never served again;
/// what is lost is the retirement this shard would have shipped, which leaves the candidates holding the tombstone, the
/// safe state. It stays marked in `pending_tombstones`, so the volume is still known destroyed here and its catalog
/// register is never rebuilt. Before, a tombstone whose partition has no shard here was skipped every period, uncounted,
/// and a refused one retried every period, for as long as the daemon ran.
async fn adopt_pending_tombstones(origin: u16) {
  let pending: Vec<(ObjectId, u64, Option<PlacedHead>, Option<u16>)> = state::with_state(|s| {
    s.pending_tombstones
      .iter()
      .filter(|(_, pending)| !pending.given_up)
      .map(|(object, pending)| {
        let partition = verbs::owner_of(slates_ipc::protocol::VolumeId { bytes: object.0 });
        (
          *object,
          pending.sequence,
          s.placed_heads.get(object).cloned(),
          s.shards.get(usize::from(partition)).copied(),
        )
      })
      .collect()
  })
  .unwrap_or_default();
  for (object, sequence, placed, target) in pending {
    let waited = match target {
      None => TOMBSTONE_WAITS_SHARD,
      Some(target) => {
        let adopted = call_within(
          origin,
          target,
          move |s| {
            let tombstone = Tombstone {
              volume: DbVolumeId { bytes: object.0 },
              sequence,
            };
            let now = s.clock.monotonic_ns();
            let recorded = match s.db.mutate(
              &mut s.segment,
              &slates_db::op::Op::TombstoneAdopted { tombstone },
              now,
            ) {
              Ok(_) => true,
              Err(slates_db::error::DbError::AlreadyExists { .. }) => {
                s.db.partition().tombstone(tombstone.volume).is_some()
              }
              Err(_) => false,
            };
            if recorded && let Some(placed) = placed {
              s.placed_heads.insert(object, placed);
            }
            if recorded {
              // A round that gave the object up waiting for its cohorts' promises, then adopted it under a later
              // configuration, left its record here (`takeover::give_up_unpromised`); its tombstone answers now.
              s.unrecovered.remove(&tombstone.volume);
            }
            recorded
          },
          HEARTBEAT_NS,
        )
        .await;
        match adopted {
          Some(true) => {
            state::with_state(|s| s.pending_tombstones.remove(&object));
            continue;
          }
          Some(false) => TOMBSTONE_ADOPT_REFUSED,
          None => TOMBSTONE_ADOPT_UNANSWERED,
        }
      }
    };
    let given_up = state::with_state(|s| {
      s.count(waited, 1);
      let bound = takeover_attempts(s.failover_ns);
      let pending = s.pending_tombstones.get_mut(&object)?;
      let attempts = pending.attempts.saturating_add(1);
      let given_up = pending.count_attempt(bound);
      if given_up {
        s.count(TOMBSTONE_ADOPT_GIVEN_UP, 1);
      }
      given_up.then_some(attempts)
    })
    .flatten();
    if let Some(attempts) = given_up {
      slates_machine::log_line!(
        "slates daemon: a taken-over tombstone {} was not recorded after {attempts} attempts, one a period (the failover \
         SLO's worth), the last counted {waited}; it is attempted no more and its volume stays destroyed here",
        crate::verbs::hex_of(&object.0)
      );
    }
  }
}

/// The status refusal count under which a successor records an adopted tombstone its owner shard refused to record
/// this period (AUD-29-43): the partition's volume capacity is full, or a live volume holds the id. It stays pending,
/// and is attempted again next period up to its bound ([`adopt_pending_tombstones`]).
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
const TOMBSTONE_ADOPT_REFUSED: &str = "fleet.tombstone.adopt_refused";
/// Counter: an adopted tombstone's owner shard did not answer within the period; it is attempted again next period up to
/// its bound. Counted as [`TOMBSTONE_ADOPT_REFUSED`] before 2026-11-18.
/// Format: a counter name in the daemon's status report.
const TOMBSTONE_ADOPT_UNANSWERED: &str = "fleet.tombstone.adopt_unanswered";
/// Counter: an adopted tombstone whose partition has no runtime shard here; it is attempted again next period up to its
/// bound. Skipped without a count before 2026-11-18.
/// Format: a counter name in the daemon's status report.
const TOMBSTONE_WAITS_SHARD: &str = "fleet.tombstone.waits.shard";
/// Counter: an adopted tombstone given up on past its attempts ([`takeover_attempts`]): its retirement is not shipped
/// from here, and the candidates keep the tombstone. Zero is the healthy reading.
/// Format: a counter name in the daemon's status report.
const TOMBSTONE_ADOPT_GIVEN_UP: &str = "fleet.tombstone.given_up";

/// Materializes each taken-over **green** whose newest merge record this node adopted (§4.16 owner-loss recovery;
/// AUD-14) as an owned green **on the shard its id routes to**, by adopting this node's own holder ledger of it (A-333,
/// `crate::green_adopt`): the engine is this node's replica, brought to the adopted version on this control shard (taken
/// as it is, advanced by the increments held here, or rebuilt from the ledger a slice at a time), the ledger's submit
/// completions gathered beside it, then both moved by value to the owner shard, which records the green by its adoption
/// (`verbs::adopt_taken_over_green`) and serves it. Inputs of the versions the replica lacks that this node does not
/// hold are fetched by identity from the survivors that reported the adopted version ([`fetch_green_inputs`]). A green
/// whose catalog is not adopted, whose inputs no survivor serves, whose adopted version lies beyond this node's records
/// (the ledger-prefix transfer of records, GAP-A9-7, is still owed), or whose replica is off the adopted identity, is
/// counted and stays pending, as one the owner shard refused does. Before A-333 the chain was gathered whole in one poll
/// and recorded again in the owner's partition, origin bytes and all, which a kernel-sized origin was refused at.
///
/// **Bounded** (2026-11-09; the supervisor's order: a wait no branch can clear is an unbounded retry, which the rules
/// forbid whatever caused it). Each pending green is attempted once a period; past [`takeover_attempts`] attempts
/// without progress it is given up on ([`give_up_taken_over`]): its owner shard records it unrecovered, naming what the
/// last attempt waited on, so every verb on it answers `Refusal::NotRecovered { cause: TakeoverIncomplete }`, and this
/// node claims it to lookups so a read from elsewhere meets that answer. Progress is a fetch that leaves fewer of its
/// inputs unheld than ever before (`state::PendingGreen::record_unheld`), so the count starts again at most once per
/// input the ledger names; and, for a wait on its catalog register while the takeover round that adopted it still
/// pages, an advance of that round ([`paging_round`]). A period that finds the replica's build or the completions'
/// gathering still running is not counted: each is a finite task of this shard, which the attempt that started it was
/// counted for. Before, every branch retried each period without end: the green-share takeover test once waited out its
/// 4,000-period budget so (GAPS 2026-11-09).
async fn materialize_pending_greens(
  origin: u16,
  local: HostId,
  budget: CommitBudget,
  in_flight: &mut Vec<Dispatch>,
) {
  let pending: Vec<(ObjectId, crate::state::PendingGreen)> = state::with_state(|s| {
    s.pending_green_materializations
      .iter()
      .map(|(object, pending)| (*object, pending.clone()))
      .collect()
  })
  .unwrap_or_default();
  let paging = state::with_state(|s| crate::takeover::rounds_still_paging(s)).unwrap_or_default();
  for (object, pending) in pending {
    let Err(waited) = materialize_green(origin, (object, local), &pending, budget, in_flight).await
    else {
      continue;
    };
    count_refusal(waited.counter());
    if !waited.counts_an_attempt() {
      continue;
    }
    let round = paging_round(waited.waits_on_the_round(), pending.departed, &paging);
    let given_up = state::with_state(|s| {
      let bound = takeover_attempts(s.failover_ns);
      let pending = s.pending_green_materializations.get_mut(&object)?;
      if round.is_some_and(|advances| pending.record_round(advances)) {
        // Its round moved on toward the register it waits for: progress, its attempts start again.
        return None;
      }
      pending.attempts = pending.attempts.saturating_add(1);
      (pending.attempts >= bound).then_some(pending.attempts)
    })
    .flatten();
    if let Some(attempts) = given_up {
      give_up_taken_over(
        origin,
        local,
        (object, TakenOver::Green),
        waited.named(),
        attempts,
      )
      .await;
    }
  }
}

/// What a taken-over green's materialization waited on when an attempt could not finish ([`materialize_green`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GreenWait {
  /// Its catalog register, or that register's placement, is not adopted (AUD-29-17).
  Catalog,
  /// Its chain could not be gathered: a version's record not known here, the adopted version beyond this node's
  /// accepted records (GAP-A9-7's record half, owed).
  Chain,
  /// An input its ledger names is still not held here: no survivor that reported the adopted version served it whole
  /// within the attempt ([`fetch_green_inputs`]).
  Inputs,
  /// No placement of its adopted record here, or no shard for the partition its id names.
  Placement,
  /// The owner shard refused the adoption (a catalog refusal: its name taken, a volume of another role under its id),
  /// did not answer within the period, or this node's replica is off the adopted record's identity.
  Refused,
  /// Its adoption's work began this attempt (A-333): this node's replica of the green being brought to the adopted
  /// version (advanced from its head, or rebuilt from the ledger), or the ledger's completions being gathered; or the
  /// task that does it could not be spawned.
  Started,
  /// The replica's build, or the gathering of the ledger's completions, is still running: a finite task of this shard,
  /// which the attempt that started it was counted for, so not counted again.
  Building,
}

impl GreenWait {
  /// The counter an attempt that waited so moves.
  const fn counter(self) -> &'static str {
    match self {
      GreenWait::Catalog => GREEN_TAKEOVER_WAITS_CATALOG,
      GreenWait::Chain => GREEN_TAKEOVER_INCOMPLETE,
      GreenWait::Inputs => GREEN_TAKEOVER_WAITS_INPUTS,
      GreenWait::Placement => GREEN_TAKEOVER_WAITS_PLACEMENT,
      GreenWait::Refused => GREEN_TAKEOVER_REFUSED,
      GreenWait::Started => GREEN_TAKEOVER_STARTED,
      GreenWait::Building => GREEN_TAKEOVER_BUILDING,
    }
  }

  /// Whether an attempt that waited so counts toward [`takeover_attempts`]: all but a period that found a build or a
  /// gathering this shard runs still running, which the attempt that started it was counted for.
  const fn counts_an_attempt(self) -> bool {
    !matches!(self, GreenWait::Building)
  }

  /// Whether it waits for a register the takeover round that adopted the green may still bring on a later page: its
  /// catalog register ([`paging_round`]).
  const fn waits_on_the_round(self) -> bool {
    matches!(self, GreenWait::Catalog)
  }

  /// Its name in a refusal's cause (`RecoveryCause::TakeoverIncomplete`).
  const fn named(self) -> &'static str {
    match self {
      GreenWait::Catalog => "catalog",
      GreenWait::Chain => "chain",
      GreenWait::Inputs => "inputs",
      GreenWait::Placement => "placement",
      GreenWait::Refused => "refused",
      GreenWait::Started | GreenWait::Building => "adoption",
    }
  }
}

/// Derived: the materialization attempts without progress a taken-over volume or green gets before it is given up on:
/// the failover delay's worth of the fleet's periods (`failover_ns / HEARTBEAT_NS`, at least one), each one attempt.
/// Counted in attempts, not wall time, so a loaded node whose periods run late is given its attempts all the same; the
/// failover delay is how long a takeover is expected to leave an object unserved, this node's live estimate (§4.8,
/// A-363; it was the operator's 10 s placeholder). The same bound counts a
/// learned object's wait for its recovery cohorts' promises in the takeover round itself (`takeover::PromiseWait`).
pub(crate) fn takeover_attempts(failover_ns: u64) -> u32 {
  u32::try_from((failover_ns / HEARTBEAT_NS).max(1)).unwrap_or(u32::MAX)
}

/// The page advances so far of the round a taken-over object's attempt waited on, when it waited on one: a wait for a
/// register that the takeover round which adopted the object (`departed`'s) may still bring, while that round is among
/// the `paging` ones (`takeover::rounds_still_paging`); `None` for every other wait, and once the round stops paging.
/// That wait clears as the round pages, a page a member a period: a catalog register sorts after every head of its
/// creator's partition, so on a host with more than the bound's worth of pages of heads in one partition every catalog
/// comes after the bound, and a bound that counted the wait gave the volume or green up while its catalog was still on
/// its way. So an advance of the round is the waiting object's progress (`state::PendingVolume::record_round`,
/// `state::PendingGreen::record_round`), and the wait counts toward [`takeover_attempts`] only while the round stands
/// still, which keeps a round that never finishes paging from holding the object pending without end.
fn paging_round(
  waits_on_the_round: bool,
  departed: Option<HostId>,
  paging: &std::collections::BTreeMap<HostId, u64>,
) -> Option<u64> {
  if !waits_on_the_round {
    return None;
  }
  paging.get(&departed?).copied()
}

/// One attempt to materialize the taken-over green `object` from its `pending` adoption: `Ok` once it serves on its
/// owner shard (and is no longer pending), else what it waited on.
async fn materialize_green(
  origin: u16,
  (object, local): (ObjectId, HostId),
  pending: &crate::state::PendingGreen,
  budget: CommitBudget,
  in_flight: &mut Vec<Dispatch>,
) -> Result<(), GreenWait> {
  // The green's catalog register, adopted in the same round as its merge record (the owner ships no merge
  // record before its current catalog is committed): the green's access list. Until it and its
  // placement are adopted the materialization waits, as a volume's does (AUD-29-17).
  let catalog = state::with_state(|s| {
    let catalog = s.pending_catalogs.get(&object).cloned();
    let placed = s.placed_heads.get(&object.catalog()).cloned();
    catalog.zip(placed)
  })
  .flatten();
  let Some(((catalog_sequence, catalog), catalog_placed)) = catalog else {
    return Err(GreenWait::Catalog);
  };
  let id = DbVolumeId { bytes: object.0 };
  let partition = verbs::owner_of(slates_ipc::protocol::VolumeId { bytes: object.0 });
  let target = state::with_state(|s| s.shards.get(usize::from(partition)).copied())
    .flatten()
    .ok_or(GreenWait::Placement)?;
  if served_already(origin, (object, target)).await {
    return Ok(());
  }
  // This node's replica at the adopted version (A-333), or what it waits on: its chain and inputs, judged before the
  // adopted record's placement, as the chain was gathered before it.
  ready_engine(object, pending, budget, (local, in_flight)).await?;
  // The takeover's placement of the adopted record (its promotion epoch and acknowledging holders),
  // recorded where the promotion ran; it moves to the owner shard with the green, as a head's does,
  // so the successor's record plane writes the green's next versions at the promotion epoch.
  let placed = state::with_state(|s| s.placed_heads.get(&object).cloned())
    .flatten()
    .ok_or(GreenWait::Placement)?;
  // The promotion epoch the takeover re-committed the green's registers at: the green's records are written at it, and
  // its adoption records it, so this node writes them there after a restart too, when `placed_heads` is forgotten.
  let promotion = placed.epoch.0.max(catalog_placed.epoch.0);
  let handed = state::with_state(|s| {
    handed_green(s, object, pending, (catalog_sequence, &catalog), promotion)
  })
  .ok_or(GreenWait::Building)??;
  let answer = call_within(
    origin,
    target,
    move |s| {
      let adopted = past_the_pressure_hold(s, |s| verbs::adopt_taken_over_green(s, id, handed));
      if adopted.is_ok() {
        s.placed_heads.insert(object, placed);
        s.placed_heads.insert(object.catalog(), catalog_placed);
      }
      adopted
    },
    HEARTBEAT_NS,
  )
  .await;
  match answer {
    Some(Ok(())) => {}
    // Refused: the engine and the completions come back, and wait here for the next attempt.
    Some(Err(back)) => {
      state::with_state(|s| crate::green_adopt::give_back(s, object, *back));
      return Err(GreenWait::Refused);
    }
    // Not answered within the period: the engine went with the call. If the owner shard installs it, the next attempt
    // finds the green served ([`served_already`]); if not, it rebuilds the replica from the ledger.
    None => return Err(GreenWait::Refused),
  }
  finish_green(object);
  Ok(())
}

/// Whether the owner shard (`target`) already serves `object`, asked only when this node holds no replica of it and no
/// build of one runs: an earlier attempt's hand-off whose answer did not come within its period, which the owner shard
/// installed after. The green is then no longer pending.
async fn served_already(origin: u16, (object, target): (ObjectId, u16)) -> bool {
  let replica_gone = state::with_state(|s| {
    !s.merge.replicas.contains_key(&object) && !s.merge.seeding.contains(&object)
  })
  .unwrap_or(false);
  if !replica_gone {
    return false;
  }
  let id = DbVolumeId { bytes: object.0 };
  let served = call_within(
    origin,
    target,
    move |s| s.greens.contains_key(&id),
    HEARTBEAT_NS,
  )
  .await;
  if served == Some(true) {
    finish_green(object);
    return true;
  }
  false
}

/// A taken-over green adopted and served: no longer pending, and what its adoption gathered dropped.
fn finish_green(object: ObjectId) {
  state::with_state(|s| {
    s.pending_green_materializations.remove(&object);
    s.pending_catalogs.remove(&object);
    crate::green_adopt::forget_completions(s, object);
  });
}

/// Brings this node's replica of `object` to the version its takeover adopted (A-333), or says what it waits on: `Ok`
/// once it is there with the adopted identity. The records of versions below the adopted one this node does not know
/// are adopted first, position by position ([`adopt_missing_records`], A-364), and the inputs of the versions the
/// replica lacks that this node does not hold are fetched ([`fetch_green_inputs`]).
async fn ready_engine(
  object: ObjectId,
  pending: &crate::state::PendingGreen,
  budget: CommitBudget,
  (local, in_flight): (HostId, &mut Vec<Dispatch>),
) -> Result<(), GreenWait> {
  use crate::green_adopt::Engine;
  let adopted = &pending.merge;
  let at = (adopted.version, adopted.identity);
  let mut engine = state::with_state(|s| crate::green_adopt::engine_for_adoption(s, object, at))
    .ok_or(GreenWait::Building)?;
  if let Engine::RecordsShort(first) = engine {
    // A successor two or more versions behind the quorum lacks the records between its own and the adopted one, which
    // phase one's pages never report (each holder's highest record alone): adopted position by position (A-364).
    adopt_missing_records(
      object,
      pending,
      (first, adopted.version),
      local,
      budget,
      in_flight,
    )
    .await?;
    engine = state::with_state(|s| crate::green_adopt::engine_for_adoption(s, object, at))
      .ok_or(GreenWait::Building)?;
  }
  if let Engine::Unheld(unheld) = engine {
    // A successor one or more versions behind the quorum holds the adopted record (its adoption committed it here) but
    // not those versions' inputs, which the survivors that reported the adopted version hold (GAPS 2026-11-09).
    fetch_green_inputs(object, pending, unheld, budget).await?;
    engine = state::with_state(|s| crate::green_adopt::engine_for_adoption(s, object, at))
      .ok_or(GreenWait::Building)?;
  }
  match engine {
    Engine::Ready => Ok(()),
    Engine::Building => Err(GreenWait::Building),
    Engine::Started => Err(GreenWait::Started),
    Engine::Unheld(_) => Err(GreenWait::Inputs),
    Engine::RecordsShort(_) => Err(GreenWait::Chain),
    Engine::Refused => Err(GreenWait::Refused),
  }
}

/// Adopts the records of a taken-over green's versions `from..to` this node (`local`) does not know (A-364, GAP-A9-7's
/// record half): `from` the first version whose record its ledger lacks (`green_adopt::Engine::RecordsShort`), `to` the
/// version the takeover round of `pending.departed` adopted, position by position (`takeover::adopt_ledger_positions`),
/// each committed again under this node. `Err(GreenWait::Chain)` while the range is not adopted whole (the attempt
/// waits, toward its bound), and for a green no round adopted (a mirror's promotion, with no round to ask under). The
/// dispatches go to the coordinator (`in_flight`) to recover their stragglers.
async fn adopt_missing_records(
  object: ObjectId,
  pending: &crate::state::PendingGreen,
  (from, to): (u64, u64),
  local: HostId,
  budget: CommitBudget,
  in_flight: &mut Vec<Dispatch>,
) -> Result<(), GreenWait> {
  if from >= to {
    return Err(GreenWait::Chain);
  }
  let departed = pending.departed.ok_or(GreenWait::Chain)?;
  let (adopted, dispatches) =
    crate::takeover::adopt_ledger_positions(departed, object, (from, to), local, budget).await;
  in_flight.extend(dispatches);
  adopted.map(|_| ()).map_err(|_| GreenWait::Chain)
}

/// The adoption of `object` this attempt hands its owner shard (A-333), once its replica is at the adopted version
/// ([`ready_engine`]): the replica taken out of this node's replicas, the submit completions its ledger carried, the
/// catalog facts the adoption records and the takeover's `promotion` epoch; else what it waits on (the completions
/// still being gathered).
fn handed_green(
  state: &mut ShardState,
  object: ObjectId,
  pending: &crate::state::PendingGreen,
  (catalog_sequence, catalog): (u64, &crate::catalog::CatalogValue),
  promotion: u64,
) -> Result<crate::green_adopt::AdoptedGreen, GreenWait> {
  let adopted = &pending.merge;
  let completions = match crate::green_adopt::gathered_completions(state, object, adopted.version) {
    crate::green_adopt::Gathered::Done(completions) => completions,
    crate::green_adopt::Gathered::Began => return Err(GreenWait::Started),
    crate::green_adopt::Gathered::Running => return Err(GreenWait::Building),
  };
  // Version 0's inputs manifest, as the ledger names it: what the adoption records of the origin.
  let origin = crate::merge_origin::ledger_value(state, object, 0).map(|zero| zero.inputs);
  let (Some(origin), Some(engine)) = (origin, crate::green_adopt::take_engine(state, object))
  else {
    return Err(GreenWait::Chain);
  };
  Ok(crate::green_adopt::AdoptedGreen {
    name: adopted.name.clone(),
    require_evidence: adopted.require_evidence,
    owner: adopted.owner.clone(),
    access: catalog.access.clone(),
    catalog_sequence,
    write: catalog.write,
    write_version: catalog.write_version,
    signers: catalog.signers.clone(),
    head: adopted.version,
    identity: adopted.identity,
    origin,
    epoch: promotion,
    engine,
    completions,
  })
}

/// Fetches the inputs the taken-over green `object`'s ledger names for the versions its replica lacks and this node does
/// not hold (`unheld`, each version and manifest, from `crate::green_adopt::engine_for_adoption`) from the survivors that
/// hold them (§4.16 failure matrix: owner loss is "Masked after takeover on a candidate that holds the ledger";
/// GAP-A9-7's input half): the members that reported the adopted version in the takeover's round (`pending.sources`),
/// each of which accepted it, and with it every version before it, only once it held its inputs (A-244). Each manifest is
/// fetched by identity, hedged across them and verified on arrival ([`fetch_manifest_into_hold`]), and staged as placed
/// for its version, so the retention rule keeps it by that version's ledger position. Lowest version first; each one
/// held whole is counted ([`GREEN_TAKEOVER_INPUTS_FETCHED`]), and the inputs still unheld after the attempt are recorded
/// on the pending green (`state::PendingGreen::record_unheld`): fewer than ever before is progress, which starts its
/// attempts again. An attempt starts no new fetch past one commit deadline (`CommitBudget::max_deadline_ns`, each fetch's
/// own bound), so it stays bounded however much this node lacks; what a cut fetch kept stays staged for the next.
/// `Err(GreenWait::Inputs)` when an input is still not held.
async fn fetch_green_inputs(
  object: ObjectId,
  pending: &crate::state::PendingGreen,
  unheld: Vec<(u64, [u8; 32])>,
  budget: CommitBudget,
) -> Result<(), GreenWait> {
  let started = futures::now_ns();
  let mut still_unheld = unheld.len();
  let mut fetched_all = true;
  for (version, manifest) in unheld {
    let fetched = futures::now_ns().saturating_sub(started) < budget.max_deadline_ns()
      && fetch_manifest_into_hold(object, &pending.sources, move |_| version, manifest, budget)
        .await;
    if !fetched {
      fetched_all = false;
      break;
    }
    still_unheld = still_unheld.saturating_sub(1);
    count_refusal(GREEN_TAKEOVER_INPUTS_FETCHED);
  }
  state::with_state(|s| {
    if let Some(green) = s.pending_green_materializations.get_mut(&object) {
      green.record_unheld(still_unheld);
    }
  });
  if fetched_all {
    Ok(())
  } else {
    Err(GreenWait::Inputs)
  }
}

/// Counter (A-412): fetches a holder began of the inputs of versions its own ledger holds without them, its replica behind
/// that ledger ([`fetch_ledger_inputs`]). Format: a refusal name in the daemon's status report.
const CATCH_UP_BEGUN: &str = "merge.catch_up.begun";
/// Counter (A-412): inputs such a fetch brought in whole, one a version. Format: a refusal name in the daemon's status
/// report.
const CATCH_UP_FETCHED: &str = "merge.catch_up.fetched";
/// Counter (A-412): such a fetch whose task the shard's arena refused; the record's next shipment begins it again. Format:
/// a refusal name in the daemon's status report.
const CATCH_UP_UNSPAWNED: &str = "merge.catch_up.unspawned";

/// Fetches by identity from `sender`, a merge record's owner, the inputs of the versions of the green `object` this holder's
/// ledger holds without them (`unheld`, each version and manifest, lowest first), its replica behind that ledger (A-412,
/// `merge_service::catch_up`): a takeover committed those versions again here through the register path, which carries no
/// inputs. Each is staged as placed for its version, so the retention rule keeps it by that version's ledger position
/// ([`fetch_manifest_into_hold`]); the record's next shipment finds them held and advances the replica. One fetch a green
/// at a time (`MergeShardState::catching_up`), in a detached task of this shard, each manifest within one live exchange's
/// budget; it ends at the first manifest not fetched, and the next shipment begins another.
pub(crate) fn fetch_ledger_inputs(
  state: &mut ShardState,
  object: ObjectId,
  sender: HostId,
  unheld: Vec<(u64, [u8; 32])>,
) {
  if !state.merge.catching_up.insert(object) {
    return;
  }
  state.count(CATCH_UP_BEGUN, 1);
  let fetching = async move {
    let budget = live_budget();
    for (version, manifest) in unheld {
      if !fetch_manifest_into_hold(object, &[sender], move |_| version, manifest, budget).await {
        break;
      }
      count_refusal(CATCH_UP_FETCHED);
    }
    let _ = state::with_state_counted(|s| s.merge.catching_up.remove(&object));
  };
  match futures::spawn(fetching) {
    Ok(task) => {
      let _ = futures::detach(task);
    }
    Err(_) => {
      state.merge.catching_up.remove(&object);
      state.count(CATCH_UP_UNSPAWNED, 1);
    }
  }
}

/// Which kind of taken-over object a give-up ends ([`give_up_taken_over`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TakenOver {
  /// A volume, adopted from its head ([`materialize_pending`]).
  Volume,
  /// A green, adopted from its newest merge record ([`materialize_pending_greens`]).
  Green,
}

/// Gives up on the taken-over `object` of its kind after `attempts` without progress, the last of which waited on
/// `waited` ([`materialize_pending`], [`materialize_pending_greens`]): its owner shard records it unrecovered with that
/// cause, so every verb on it answers `Refusal::NotRecovered`; this node stops attempting it and claims it to lookups
/// as its owner (`local`), so a read from another node is routed here and meets the answer. Counted and logged. With no
/// shard for its partition here nothing on this node can serve or record it: it is dropped, counted and logged all the
/// same, and not claimed, so its verbs answer as for any id this node does not own. An owner shard that does not answer
/// the period leaves it pending, given up on again the next.
async fn give_up_taken_over(
  origin: u16,
  local: HostId,
  (object, kind): (ObjectId, TakenOver),
  waited: &'static str,
  attempts: u32,
) {
  let id = DbVolumeId { bytes: object.0 };
  let partition = verbs::owner_of(slates_ipc::protocol::VolumeId { bytes: object.0 });
  let target = state::with_state(|s| s.shards.get(usize::from(partition)).copied()).flatten();
  if let Some(target) = target {
    let recorded = call_within(
      origin,
      target,
      move |s| {
        s.unrecovered.insert(
          id,
          slates_ipc::protocol::RecoveryCause::TakeoverIncomplete {
            waited_on: waited.to_owned(),
          },
        );
        // A live materialization's record is forgotten with the volume: nothing reads its end now (g251).
        s.making_taken_over.remove(&object);
      },
      HEARTBEAT_NS,
    )
    .await
    .is_some();
    if !recorded {
      // The owner shard did not answer this period: the object stays pending, given up on again next period.
      return;
    }
  }
  let (what, counter) = match kind {
    TakenOver::Volume => ("volume", MATERIALIZE_GIVEN_UP),
    TakenOver::Green => ("green", GREEN_TAKEOVER_GIVEN_UP),
  };
  state::with_state(|s| {
    match kind {
      TakenOver::Volume => {
        s.pending_materializations.remove(&object);
      }
      TakenOver::Green => {
        s.pending_green_materializations.remove(&object);
        crate::green_adopt::forget_completions(s, object);
      }
    }
    s.pending_catalogs.remove(&object);
    if target.is_some() {
      crate::takeover::track_owner_on_every_shard(s, object, local);
    }
    s.count(counter, 1);
  });
  slates_machine::log_line!(
    "slates daemon: a taken-over {what} {} was not materialized after {attempts} attempts without progress, one a \
     period (the failover SLO's worth), the last waiting on its {waited}; {}",
    crate::verbs::hex_of(&object.0),
    if target.is_some() {
      "every verb on it answers NotRecovered"
    } else {
      "no shard here serves its partition, so it is dropped"
    }
  );
}

/// Waits out a test's injected serve delay (`Daemon::inject_serve_delay`) before a location answer or a forwarded
/// verb; nothing in production. A sleep the runtime could not time is counted and the answer goes at once.
async fn injected_serve_delay() {
  let Some(delay_ns) = state::with_state(|s| s.injected_serve_delay_ns).flatten() else {
    return;
  };
  if slates_rt::futures::sleep(delay_ns).await.is_err() {
    count_refusal(SERVE_DELAY_UNTIMED);
  }
}

/// Counter: a test's injected serve delay the runtime could not time; the answer went at once.
/// Format: a counter name in the daemon's status report.
const SERVE_DELAY_UNTIMED: &str = "fleet.serve_delay_untimed";

/// Waits out a test's injected acknowledgement delay (`Daemon::inject_content_ack_delay`) before this holder's content
/// `reply` leaves, when the reply is an acknowledgement; nothing in production. The hold took the content before the
/// wait, so the owner meets the condition a holder whose refill outlasts the round meets under load: its round stops
/// collecting at the hedge delay and the acknowledgement arrives as a straggler's. A sleep the runtime could not time is
/// counted and the reply goes at once.
async fn injected_content_ack_delay(reply: &[u8]) {
  let Some(delay_ns) = state::with_state(|s| s.injected_content_ack_delay_ns).flatten() else {
    return;
  };
  if !matches!(ContentMessage::decode(reply), Ok(ContentMessage::Ack(_))) {
    return;
  }
  if slates_rt::futures::sleep(delay_ns).await.is_err() {
    count_refusal(SERVE_DELAY_UNTIMED);
  }
}

/// Runs `recovery` on this shard with the memory-pressure hold lifted for exactly its duration, then sets the hold
/// back. A takeover's materialization (its fetch's staging, the restore, the volume's reservations and the writes
/// that populate it) moves a claim the fleet already admitted and committed to its successor: admission.md §5.5 keeps
/// the hold off every committed claim ("it shrinks admittable only, never revoking a committed claim"), and §4e
/// admits a recovered claim ahead of any new one, which boot recovery gets because its hold starts at zero. Before
/// 2026-10-07 the takeover was refused like a new volume, so under host pressure a successor adopted the head and
/// never served it (a fleet test: 3,913 refusals, the volume unavailable until the pressure lifted). Capacity and the
/// operation headroom still bound it. Exact because a shard runs one task at a time and `recovery` is synchronous:
/// no other admission can run while the hold is lifted, and the pressure refresh is itself a message to this shard.
/// The hold is the daemon's (2026-10-09): its part withheld from the content pool is a set of extents under the hold's
/// own owner, so the recovery may take those too while it runs (`ExtentSource::open_hold`), as it lifts this shard's
/// part and its debt (restored after, with the debt's mark, so the recovery's own growth takes none of it on); the
/// sampler withholds what was taken from the pool again at its next refresh.
fn past_the_pressure_hold<T>(
  state: &mut ShardState,
  recovery: impl FnOnce(&mut ShardState) -> T,
) -> T {
  let (hold, pressure) = (state.store.budget.hold(), state.store.budget.pressure());
  state.store.budget.set_pressure(0, 0);
  state.store.content.arena_mut().open_hold(true);
  let recovered = recovery(state);
  state.store.content.arena_mut().open_hold(false);
  state.store.budget.set_hold(hold);
  state.store.budget.restore_pressure(pressure);
  recovered
}

/// Counter: a taken-over volume's materialization waited a period for its content, which this node does not hold
/// whole yet (the archive of the head's manifest).
/// Format: a counter name in the daemon's status report.
const MATERIALIZE_WAITS_CONTENT: &str = "fleet.materialize.waits.content";
/// Counter: a taken-over volume's materialization waited a period for the takeover's placement of its head.
/// Format: a counter name in the daemon's status report.
const MATERIALIZE_WAITS_PLACEMENT: &str = "fleet.materialize.waits.placement";
/// Counter: a taken-over volume's materialization waited a period for its catalog register's adoption (AUD-29-17).
/// Format: a counter name in the daemon's status report.
const MATERIALIZE_WAITS_CATALOG: &str = "fleet.materialize.waits.catalog";
/// Counter: a taken-over volume's materialization waited a period for the placement of its adopted catalog.
/// Format: a counter name in the daemon's status report.
const MATERIALIZE_WAITS_CATALOG_PLACEMENT: &str = "fleet.materialize.waits.catalog_placement";
/// Counter: a taken-over volume's materialization found no runtime shard for the partition its id names.
/// Format: a counter name in the daemon's status report.
const MATERIALIZE_WAITS_SHARD: &str = "fleet.materialize.waits.shard";
/// Counter: a taken-over volume given up on past its attempts without progress ([`takeover_attempts`]): every verb on
/// it answers `NotRecovered { cause: TakeoverIncomplete }`. Zero is the healthy reading.
/// Format: a counter name in the daemon's status report.
const MATERIALIZE_GIVEN_UP: &str = "fleet.materialize.given_up";

/// A taken-over green whose catalog register (its access list) or that register's placement was not yet
/// adopted this period; it stays pending and is retried.
/// Format: a counter name in the daemon's status report.
const GREEN_TAKEOVER_WAITS_CATALOG: &str = "merge.takeover_waits_catalog";
/// A taken-over green whose chain could not be gathered this period: a version's record not known here (the adopted
/// version beyond this node's accepted records, GAP-A9-7's record half); it stays pending and is retried.
/// Format: a counter name in the daemon's status report.
const GREEN_TAKEOVER_INCOMPLETE: &str = "merge.takeover_incomplete";
/// A taken-over green with an input its ledger names still not held here after this period's fetch from the survivors
/// that reported its adopted version ([`fetch_green_inputs`]); it stays pending and is retried.
/// Format: a counter name in the daemon's status report.
const GREEN_TAKEOVER_WAITS_INPUTS: &str = "merge.takeover_waits_inputs";
/// A taken-over green the owner shard refused to materialize (a recomputed identity that does not
/// match the adopted record's, or a catalog refusal); it stays pending, counted.
const GREEN_TAKEOVER_REFUSED: &str = "merge.takeover_refused";
/// A taken-over green with no placement of its adopted record here, or no shard for its partition, this period; it
/// stays pending and is retried. Before 2026-11-09 this branch was not counted at all.
/// Format: a counter name in the daemon's status report.
const GREEN_TAKEOVER_WAITS_PLACEMENT: &str = "merge.takeover_waits_placement";
/// A taken-over green whose adoption's work began this period (A-333): its replica being brought to the adopted version
/// (advanced from its head, or rebuilt from this node's ledger), or its ledger's completions being gathered; it stays
/// pending and is attempted again.
/// Format: a counter name in the daemon's status report.
const GREEN_TAKEOVER_STARTED: &str = "merge.takeover_started";
/// A taken-over green whose replica's build, or whose ledger's completions' gathering, was still running this period
/// (A-333): not counted against the bound, since each is a finite task.
/// Format: a counter name in the daemon's status report.
const GREEN_TAKEOVER_BUILDING: &str = "merge.takeover_building";
/// A taken-over green given up on past its attempts ([`takeover_attempts`]): every verb on it answers
/// `NotRecovered { cause: TakeoverIncomplete }`. Zero is the healthy reading.
/// Format: a counter name in the daemon's status report.
const GREEN_TAKEOVER_GIVEN_UP: &str = "merge.takeover_given_up";
/// Counter: an input a taken-over green's ledger named and this node did not hold, held whole after a fetch by identity
/// from a survivor that reported the adopted version ([`fetch_green_inputs`]): the non-vacuity counter of a successor's
/// catch-up to the quorum's version (GAPS 2026-11-09: a successor one version behind waited on its chain until given
/// up).
/// Format: a counter name in the daemon's status report.
const GREEN_TAKEOVER_INPUTS_FETCHED: &str = "merge.takeover_inputs_fetched";

/// The hosts a live takeover's phase one asks for `object` (A-240, A-309), and where the retired owner placed it: the
/// takeover's `placed` candidates, then every candidate of the current placement not among them; and the retired
/// owner's cohort hosts for the volume as the round that adopted it recorded them (`PendingVolume::cohort_hosts`),
/// asked and waited for when the recorded holders are past tolerance (`live_ship::fetch`). With this shard's state out of
/// reach (its shutdown) only the `placed` candidates, and no cohort hosts.
fn live_candidates(object: ObjectId, placed: &[HostId]) -> (Vec<HostId>, Vec<HostId>) {
  let mut candidates = placed.to_vec();
  let (current, cohort_hosts) = state::with_state(|s| {
    (
      s.fleet.configuration().place(object).candidates,
      s.pending_materializations
        .get(&object)
        .map(|pending| pending.cohort_hosts.clone())
        .unwrap_or_default(),
    )
  })
  .unwrap_or_default();
  for host in current {
    if !candidates.contains(&host) {
      candidates.push(host);
    }
  }
  (candidates, cohort_hosts)
}

/// The lineage key a taken-over volume's successor opens and seals its `head` with (A-92 piece 4c), wrapped under this
/// node's root: its own copy where the head carried an entry for it; else, for an `envelope` it must open, the key a
/// fellow candidate of the head (`candidates`, this node left out) hands it (A-295), counted; else none.
async fn successor_lineage_of(
  object: ObjectId,
  head: &HeadValue,
  candidates: &[HostId],
  envelope: bool,
) -> Option<([u8; 16], Vec<u8>)> {
  let sealing = head.sealing.as_ref()?;
  if let Some(own) =
    state::with_state(|s| crate::seal_keys::successor_lineage(s, sealing)).flatten()
  {
    return Some(own);
  }
  if !envelope {
    return None;
  }
  let local = state::with_state(|s| s.fleet.configuration().owner);
  let fellows: Vec<HostId> = candidates
    .iter()
    .copied()
    .filter(|host| Some(*host) != local)
    .collect();
  let handed = ask_handed_lineage(object, sealing, &fellows).await;
  if handed.is_some() {
    count_refusal(ENVELOPE_KEY_HANDED);
  }
  handed
}

/// What a takeover's loss report says of its tolerance (A-332): the shortfall the round adopted the volume's head at
/// (`registers`) and the one phase one adopted its live log at (`live_log`), `None` when both were within tolerance.
fn past_tolerance_of(
  registers: Option<slates_db::catalog::RecoveryShortfall>,
  live_log: Option<slates_db::catalog::RecoveryShortfall>,
) -> Option<slates_db::catalog::TakeoverPastTolerance> {
  (registers.is_some() || live_log.is_some()).then_some(slates_db::catalog::TakeoverPastTolerance {
    registers,
    live_log,
  })
}

/// Materializes one taken-over volume from the content this node holds for its head (see
/// [`materialize_pending`]) **on the shard the volume's id routes to** — the partition its id names is
/// where every verb for it will run (`verbs::owner_of`), so that is where it must live. Made by a task of that
/// shard in turns of its clock ([`make_taken`], g251), live log or placed seal alike: an attempt that finds one
/// running joins it, one that finds one ended reads its end, and only one that finds neither gathers what the volume is
/// made from (the live log's fetch, or the held archive copied a slice a turn, an envelope opened in turns) and begins
/// one. On success the object is no longer pending; on a refusal (or a shard that does not answer this period) it is
/// counted and stays pending.
async fn materialize(origin: u16, object: ObjectId, head: HeadValue) -> Result<(), VolumeWait> {
  let id = DbVolumeId { bytes: object.0 };
  let partition = verbs::owner_of(slates_ipc::protocol::VolumeId { bytes: object.0 });
  let taken = state::with_state(|s| {
    // The takeover's placement of the head (its sequence, promotion epoch and acknowledging holders),
    // recorded here where the promotion ran; it moves to the owner shard with the volume.
    let placed = s
      .placed_heads
      .get(&object)
      .cloned()
      .ok_or(VolumeWait::Placement)?;
    // The volume's catalog, adopted in the same round (it is placed before any head that could be adopted
    // is shipped): the volume is served as that catalog describes it (AUD-29-17). Until it is adopted the
    // materialization waits.
    let (catalog_sequence, catalog) = s
      .pending_catalogs
      .get(&object)
      .cloned()
      .ok_or(VolumeWait::Catalog)?;
    let catalog_placed = s
      .placed_heads
      .get(&object.catalog())
      .cloned()
      .ok_or(VolumeWait::CatalogPlacement)?;
    // The partition the id names, as this daemon's runtime shard (the shard list is in partition order).
    let target = s
      .shards
      .get(usize::from(partition))
      .copied()
      .ok_or(VolumeWait::Shard)?;
    // A move this node adopts (A-291): its adoption, kept here until the move is reported adopted, marks the volume's
    // materialization as the move's, whose live log is exactly the head it hands over (A-565).
    let moved = s.move_adoptions.contains_key(&object);
    Ok::<_, VolumeWait>((
      placed,
      catalog_sequence,
      catalog,
      catalog_placed,
      (target, moved),
    ))
  });
  let (placed, catalog_sequence, catalog, catalog_placed, (target, moved)) = match taken {
    Some(Ok(taken)) => taken,
    Some(Err(waits)) => {
      // Each period's wait is counted by what it waits for, so a successor that adopted a head and never serves
      // its volume names the missing piece (a successor's own client was refused for a whole wait in a full
      // fleet suite, 2026-10-07, with nothing counted).
      count_refusal(waits.counter());
      return Err(waits);
    }
    // The control shard's state out of reach (its shutdown): nothing is attempted, nothing is counted.
    None => return Ok(()),
  };
  // A volume's materialization runs in turns on its owner shard, by a task of that shard ([`make_taken`], g251): an
  // attempt that finds one running joins it, one that finds one ended reads its end, and only one that finds neither
  // gathers the volume's content and begins one. Before, every attempt gathered it again and made the volume inside one
  // owner-shard turn, which ran past the watchdog's spin window (a live log's successors, 0.3 to 2.8 s; a plain
  // volume's restore of the 1.99 GB kernel base, seconds), and an attempt that turn outlasted (its call's deadline a
  // period, 100 ms) counted as unanswered, so the next period gathered it and began again.
  match making_of(origin, target, object).await {
    Some(Joined::None) => {}
    Some(Joined::Running(turns)) => return making_runs(object, turns),
    Some(Joined::Ended(ended)) => return taken_ended(object, ended),
    None => return materialize_unanswered(),
  }
  // A live-shipped volume is served from its live log, which holds every acknowledged write (A-200): phase one over its
  // candidates under the takeover's epoch, the greatest head adopted, `f + 1` counted among the holders the catalog
  // records (A-240). A catalog that records none had a shipper that never shipped (it ships only once its holders are
  // placed with the catalog, `live_ship::holders_placed`): no log exists, and the volume is served from its placed seal.
  let live = if let Some(recorded) = catalog
    .live_holders
    .as_ref()
    .filter(|_| catalog.live_shipped)
  {
    let holders: Vec<HostId> = recorded.holders.iter().copied().map(HostId).collect();
    let (candidates, cohort_hosts) = live_candidates(object, &placed.placement.candidates);
    match crate::live_ship::fetch(object, placed.epoch, candidates, holders, cohort_hosts).await {
      Ok(fetched) => fetched,
      Err(waits) => {
        count_refusal(waits);
        return Err(VolumeWait::Live);
      }
    }
  } else {
    None
  };
  let region = head.content_holders.clone();
  // Whether the round adopted the head, or phase one the log, past tolerance (A-309), for the loss report (A-332).
  let registers_short = state::with_state(|s| {
    s.pending_materializations
      .get(&object)
      .and_then(|pending| pending.registers_short)
  })
  .flatten();
  let past_tolerance = past_tolerance_of(
    registers_short,
    live.as_ref().and_then(|fetched| fetched.past_tolerance),
  );
  if let Some(fetched) = live {
    let making = MakeTaken {
      object,
      id,
      head,
      catalog,
      catalog_sequence,
      placed,
      catalog_placed,
      region,
      past_tolerance,
      moved,
    };
    return begin_materializing(origin, target, making, Source::Live(fetched)).await;
  }
  // A volume with content waits until this node holds that content whole, copied out of the held store a slice a turn
  // ([`held_archive_in_turns`]); a contentless volume (never sealed) has no manifest and no archive: it materializes
  // from its adopted head and catalog registers alone.
  let archive = match head.manifest {
    Some(manifest) => match held_archive_in_turns(object, manifest).await {
      Some(archive) => Some(archive),
      None => {
        count_refusal(VolumeWait::Content.counter());
        return Err(VolumeWait::Content);
      }
    },
    None => None,
  };
  // The successor's own copy of the volume's lineage key (A-92 piece 4c), unwrapped here on the control shard, which
  // holds the pair keys, and handed to the owner shard wrapped under this node's root. A successor that holds no entry
  // of its own in a sealed head whose envelope it must open (a node replaced at its anchor before the owner's pair
  // reached it) is handed the key by a fellow candidate of the head, as a live log's successor is (A-295).
  let envelope = archive
    .as_ref()
    .is_some_and(slates_cluster::envelope::is_envelope);
  let successor_lineage =
    successor_lineage_of(object, &head, &placed.placement.candidates, envelope).await;
  let naming = head
    .sealing
    .as_ref()
    .and_then(|sealing| sealing.naming.clone());
  // A sealed envelope is opened before the volume is made, in turns of its owner shard's clock, its lineage key adopted
  // first (A-347); the task that makes the volume then finds a plain archive. Opened in one turn, a GiB volume's open
  // ran about a second, the watchdog's window.
  let (archive, lineage) = if envelope {
    let owner = catalog.owner.clone();
    match open_in_turns(
      origin,
      target,
      id,
      (owner, successor_lineage, naming.clone()),
      archive,
    )
    .await
    {
      Ok(opened) => (opened, None),
      Err(refusal) => return materialize_refused(refusal),
    }
  } else {
    (archive, successor_lineage)
  };
  let making = MakeTaken {
    object,
    id,
    head,
    catalog,
    catalog_sequence,
    placed,
    catalog_placed,
    region,
    past_tolerance,
    moved,
  };
  let source = Source::Archive {
    archive,
    lineage,
    naming,
  };
  begin_materializing(origin, target, making, source).await
}

/// The archive this (control) shard holds whole for `object`'s manifest `identity`, copied out of the held store a
/// slice of chunks a turn (g251): its manifest, then each chunk it names, until a copy slice's bytes are copied
/// (`DaemonConfig::copy_slice_bytes`, a chunk at least), the shard given back between slices. `None` when the
/// manifest or any chunk it names is not held for the object. Copied in one call (`ContentHold::archive_of`), the
/// 1.99 GB kernel base volume was one copy of all its chunks in a turn of the coordinator, every period's attempt.
async fn held_archive_in_turns(object: ObjectId, identity: [u8; 32]) -> Option<Archive> {
  let manifest = state::with_state(|s| {
    s.held_content
      .held_manifest(s.store.content.arena(), object, &identity)
  })
  .flatten()?;
  let referenced = manifest.referenced_chunks();
  let mut chunks = Vec::with_capacity(referenced.len());
  let mut next = referenced.into_iter().peekable();
  while next.peek().is_some() {
    let copied = state::with_state(|s| {
      let slice_bytes = s.config.copy_slice_bytes();
      let mut copied_bytes = 0u64;
      s.count(MATERIALIZE_ARCHIVE_SLICES, 1);
      for chunk in next.by_ref() {
        let held = s
          .held_content
          .held_chunk(s.store.content.arena(), object, &identity, &chunk)?;
        copied_bytes = copied_bytes.saturating_add(held.stored_len);
        chunks.push(held);
        if copied_bytes >= slice_bytes {
          break;
        }
      }
      Some(())
    })
    .flatten();
    copied?;
    futures::yield_now().await;
  }
  Some(Archive { chunks, ..manifest })
}

/// Counter: one turn's slice of a taken-over volume's held archive copied out of the held store on the control shard
/// ([`held_archive_in_turns`], g251).
/// Format: a counter name in the daemon's status report.
const MATERIALIZE_ARCHIVE_SLICES: &str = "fleet.materialize.archive_slices";

/// Counter: materializations of a taken-over volume whose frozen copy (a move this node made, A-291) the database would
/// not let go this period: the volume stays pending and is attempted again next period.
const FROZEN_UNSUPERSEDED: &str = "fleet.materialize.frozen_unsuperseded";

/// Lets a copy this node froze for a move it made (A-291) go before a takeover of the same volume adopts its keys, on
/// the volume's owner shard: the adoption serves what the move's target left, writes included
/// (`crate::moves::supersede_frozen`), and the copy's release erases the volume's keys with its record (a destroy's
/// cryptographic erase), so a release after the adoption erased the keys just adopted and the volume's next seal made
/// keys of its own. Nothing when no copy is frozen here; [`FROZEN_UNSUPERSEDED`], counted, when the database refused the
/// release (attempted again next period).
fn let_frozen_go(state: &mut ShardState, id: DbVolumeId) -> Result<(), &'static str> {
  if state.moves_out.contains_key(&id) && !crate::moves::supersede_frozen(state, id) {
    state.count(FROZEN_UNSUPERSEDED, 1);
    return Err(FROZEN_UNSUPERSEDED);
  }
  Ok(())
}

/// A taken-over volume's materialization as its owner shard holds it, read by an attempt of the control shard
/// ([`making_of`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Joined {
  /// None runs or ended there: the attempt gathers the volume's content and begins one.
  None,
  /// One runs: the turns it has taken.
  Running(u64),
  /// One ended, made or refused by the named counter: read once, and forgotten there.
  Ended(Result<(), &'static str>),
}

/// Where `object`'s materialization stands on its owner shard `target` ([`make_taken`]), an ended one forgotten
/// there once read; `None` when the shard did not answer in its own time.
async fn making_of(origin: u16, target: u16, object: ObjectId) -> Option<Joined> {
  crate::xshard::call_in_shard_time(
    origin,
    target,
    move |s| match s.making_taken_over.get(&object).copied() {
      None => Joined::None,
      Some(Making::Running { turns }) => Joined::Running(turns),
      Some(Making::Made) => {
        s.making_taken_over.remove(&object);
        Joined::Ended(Ok(()))
      }
      Some(Making::Refused(why)) => {
        s.making_taken_over.remove(&object);
        Joined::Ended(Err(why))
      }
    },
    HEARTBEAT_NS,
  )
  .await
}

/// An attempt that found its volume's materialization running on the owner shard: the turns it has taken recorded (more
/// than at the last look is progress, so a materialization that moves is never given up on) and the wait counted.
fn making_runs(object: ObjectId, turns: u64) -> Result<(), VolumeWait> {
  let counted = state::with_state(|s| {
    if let Some(pending) = s.pending_materializations.get_mut(&object) {
      pending.record_turns(turns);
    }
    s.count(MATERIALIZE_WAITS_MAKING, 1);
  });
  match counted {
    Some(()) => Err(VolumeWait::Making),
    // The control shard's state out of reach (its shutdown): nothing more is attempted.
    None => Ok(()),
  }
}

/// The end of a taken-over volume's materialization where the control shard learns it: made, the takeover's pending
/// entries are done with; refused, counted as an owner shard's refusals are, and a live log's refusal remembered at the
/// head it was fetched at ([`crate::live_ship::note_rebuild`]), so the log is not fetched again at a head it cannot
/// rebuild.
fn taken_ended(object: ObjectId, ended: Result<(), &'static str>) -> Result<(), VolumeWait> {
  let adopted = state::with_state(|s| {
    s.pending_materializations
      .get(&object)
      .and_then(|pending| pending.live_adopted.clone())
  })
  .flatten();
  if let Some((head, sealing)) = adopted {
    crate::live_ship::note_rebuild(object, head, ended.err(), sealing);
  }
  let outcome = state::with_state(|s| match ended {
    Ok(()) => {
      // Served: its recovery, from its adoption to now, is a sample of the failover estimate (A-363).
      if let Some(pending) = s.pending_materializations.remove(&object) {
        s.failover
          .volume_served(slates_rt::futures::now_ns().saturating_sub(pending.adopted_ns));
      }
      s.pending_catalogs.remove(&object);
      Ok(())
    }
    Err(refusal) => {
      s.count(MATERIALIZE_REFUSED, 1);
      s.count(refusal, 1);
      Err(VolumeWait::Refused)
    }
  });
  // The control shard's state out of reach (its shutdown): nothing more is attempted.
  outcome.unwrap_or(Ok(()))
}

/// An attempt whose question to the owner shard was not answered in that shard's own time: counted, retried next
/// period.
fn materialize_unanswered() -> Result<(), VolumeWait> {
  let counted = state::with_state(|s| {
    s.count(MATERIALIZE_REFUSED, 1);
    s.count(MATERIALIZE_UNANSWERED, 1);
  });
  match counted {
    Some(()) => Err(VolumeWait::Refused),
    None => Ok(()),
  }
}

/// What a taken-over volume's materialization takes to its owner shard besides its content: the takeover's adopted
/// registers and their placements, the content holders the head recorded and the shortfall it was adopted past (A-332).
struct MakeTaken {
  object: ObjectId,
  id: DbVolumeId,
  head: HeadValue,
  catalog: crate::catalog::CatalogValue,
  catalog_sequence: u64,
  placed: crate::head::PlacedHead,
  catalog_placed: crate::head::PlacedHead,
  region: Vec<u64>,
  past_tolerance: Option<slates_db::catalog::TakeoverPastTolerance>,
  /// Whether this node adopts the volume by a planned move ([`crate::moves`]; `verbs::TakenOver::moved`, A-565).
  moved: bool,
}

/// What a taken-over volume is made from on its owner shard.
enum Source {
  /// Its live log, fetched (A-200, A-386): rebuilt a payload a slice, then laid out as an archive.
  Live(crate::live_ship::Fetched),
  /// Its placed seal's archive (an envelope already opened in turns, A-347; none for a contentless volume), the
  /// successor's lineage key to adopt (A-92 pieces 3b and 4c) and the head's naming key.
  Archive {
    archive: Option<Archive>,
    lineage: Option<([u8; 16], Vec<u8>)>,
    naming: Option<crate::head::HeadNaming>,
  },
}

/// Begins a taken-over volume's materialization on its owner shard `target` as a task of that shard ([`make_taken`])
/// and waits a period for its end: an end within that is read at once; one still running is joined by the next attempt
/// ([`making_of`], which asks in the shard's own time), its content never gathered or made again meanwhile, so the wait
/// needs no longer than the period it holds the control shard's coordinator for. Waited for in the owner shard's own
/// time instead, the coordinator was held for the whole materialization (1.2 s of a test's paused turns), its council
/// and its heads with it. A live log's head and sealing are kept with the pending volume, for its end's
/// [`crate::live_ship::note_rebuild`].
async fn begin_materializing(
  origin: u16,
  target: u16,
  making: MakeTaken,
  source: Source,
) -> Result<(), VolumeWait> {
  let object = making.object;
  if let Source::Live(fetched) = &source {
    let adopted = (fetched.head, fetched.sealing.clone());
    let _ = state::with_state_counted(|s| {
      if let Some(pending) = s.pending_materializations.get_mut(&object) {
        pending.live_adopted = Some(adopted);
      }
    });
  }
  // A wall bound, by its reason (A-397, A-440): the period's, since the making runs on as a task of the owner shard and
  // the next attempt joins it ([`making_of`], in that shard's own time).
  let ended = crate::xshard::task_within(
    origin,
    target,
    move || make_taken(making, source),
    HEARTBEAT_NS,
  )
  .await;
  match ended {
    Some(ended) => {
      // Read here, so the owner shard forgets the record its task left for a later attempt.
      crate::xshard::run_on_counted(origin, target, move |s| {
        if matches!(
          s.making_taken_over.get(&object),
          Some(Making::Made | Making::Refused(_))
        ) {
          s.making_taken_over.remove(&object);
        }
      });
      taken_ended(object, ended)
    }
    // Still running on the owner shard, or not begun there: the next attempt reads which.
    None => making_runs(object, 0),
  }
}

/// A taken-over volume's materialization, a task of its owner shard (g251, CLAUDE.md "Bounded work everywhere"): it
/// claims the volume's record (`ShardState::making_taken_over`), makes the volume in turns of the shard's clock
/// ([`make_in_turns`]), and leaves its end in the record for the control shard to read. Refused [`MAKING_ALREADY`]
/// when a materialization of the volume already runs there, which the control shard joins instead.
async fn make_taken(making: MakeTaken, source: Source) -> Result<(), &'static str> {
  let object = making.object;
  let claimed = state::with_state(|s| {
    if s.making_taken_over.contains_key(&object) {
      return false;
    }
    s.making_taken_over
      .insert(object, Making::Running { turns: 0 });
    s.count(MATERIALIZE_BEGUN, 1);
    true
  });
  match claimed {
    Some(true) => {}
    Some(false) => return Err(MAKING_ALREADY),
    None => return Err(MATERIALIZE_UNANSWERED),
  }
  let id = making.id;
  let ended = make_in_turns(making, source).await;
  // Recorded only while the record is still this task's: a takeover the control shard gave up on meanwhile forgot it
  // ([`give_up_taken_over`]), and nothing would read an end left then. A volume still being made is discarded with
  // its credits given back on any end but its making (`verbs::discard_making` finds none once it is made).
  let recorded = state::with_state(|s| {
    verbs::discard_making(s, id);
    if let Some(record) = s.making_taken_over.get_mut(&object) {
      *record = match ended {
        Ok(()) => Making::Made,
        Err(why) => Making::Refused(why),
      };
    }
  });
  if recorded.is_none() {
    return Err(MATERIALIZE_UNANSWERED);
  }
  // Made here, so found here by its name too: the alias its name's partition holds for it, where its name hashes to
  // another partition (`verbs::claim_name_alias`; goal 91's live move).
  if ended.is_ok() {
    verbs::claim_name_alias(id).await;
  }
  ended
}

/// One more turn taken by `object`'s materialization on this owner shard, counted for the control shard's progress.
fn turned(state: &mut ShardState, object: ObjectId) {
  if let Some(Making::Running { turns }) = state.making_taken_over.get_mut(&object) {
    *turns = turns.saturating_add(1);
  }
}

/// One turn of `object`'s materialization on this owner shard: `turn` run and counted for the control shard's progress
/// ([`turned`]); refused [`MAKING_FORGOTTEN`] without running it once the control shard gave the takeover up and forgot
/// its record ([`give_up_taken_over`]), so a materialization nothing waits for stops.
fn making_turn<T>(
  object: ObjectId,
  turn: impl FnOnce(&mut ShardState) -> T,
) -> Result<T, &'static str> {
  state::with_state(|s| {
    if !matches!(
      s.making_taken_over.get(&object),
      Some(Making::Running { .. })
    ) {
      return Err(MAKING_FORGOTTEN);
    }
    turned(s, object);
    Ok(turn(s))
  })
  .ok_or(MATERIALIZE_UNANSWERED)?
}

/// The span a materialization's turn may run on this shard: the seal turns' share of the shard's quantum
/// (`config::wrap_slice_ns` of [`seal_quantum_ns`], a piece at least), as a live rebuild's.
fn making_slice_ns(state: &ShardState) -> u64 {
  crate::config::wrap_slice_ns(seal_quantum_ns(state))
}

/// A test's wait between a materialization's turns (`Daemon::inject_live_turn_pause`); nothing in production.
async fn injected_turn_pause() {
  if let Some(pause_ns) = state::with_state(|s| s.live_turn_pause_ns).flatten()
    && futures::sleep(pause_ns).await.is_err()
  {
    count_refusal(LIVE_TURN_PAUSE_UNTIMED);
  }
}

/// A placed seal's archive (none for a contentless volume), the successor's lineage key to adopt and the head's
/// naming key ([`Source::Archive`]).
type ArchiveSource = (
  Option<Archive>,
  Option<([u8; 16], Vec<u8>)>,
  Option<crate::head::HeadNaming>,
);

/// A taken-over volume's placed archive, opened in a turn of its owner shard: a copy this node froze for a move it made
/// (A-291) let go before the volume's keys are adopted ([`let_frozen_go`]; an envelope's open let it go in its first
/// turn); the volume's lineage key adopted, so this node seals under the key the dead owner did (A-92 pieces 3b and 4c;
/// an envelope's was adopted before its open, [`open_in_turns`]; a head that carried no entry for this node, having
/// acknowledged before its pair arrived, leaves a plain archive's volume to a new key); and the archive itself. A
/// sealed archive is opened before this task begins, so what arrives here is a plain one, or none for a contentless
/// volume.
async fn archive_opened(
  object: ObjectId,
  id: DbVolumeId,
  catalog: &crate::catalog::CatalogValue,
  (archive, lineage, naming): ArchiveSource,
) -> Result<Option<Archive>, &'static str> {
  futures::yield_now().await;
  making_turn(object, |s| {
    let_frozen_go(s, id)?;
    if let Some(lineage) = lineage
      && crate::seal_keys::adopt_lineage(s, id, &catalog.owner, lineage, naming.as_ref()).is_err()
    {
      s.count(LINEAGE_UNADOPTED, 1);
    }
    match archive {
      Some(archive) => opened_archive(archive).map(Some).ok_or_else(|| {
        s.count(ENVELOPE_UNOPENED, 1);
        ENVELOPE_UNOPENED
      }),
      None => Ok(None),
    }
  })?
}

/// The turns of a taken-over volume's materialization on its owner shard, each a turn of its own:
/// - from a live log, its payloads opened and applied a slice a turn ([`crate::live_ship::Rebuilding`], counted
///   [`LIVE_REBUILD_SLICES`]), then laid out as an archive; from a placed seal, the successor's lineage key adopted and
///   the archive taken as opened;
/// - unless the volume is made here already (a retry after its publication was refused), the archive restored a slice a
///   turn (`slates_archive::Planning`, then `Restoring` once [`verbs::restore_admitted`] admits it; counted
///   [`MATERIALIZE_RESTORE_SLICES`]), the volume begun ([`verbs::begin_making`]) and its tree laid in a slice a turn
///   ([`verbs::populate_making`], writes of at most a copy slice; counted [`MATERIALIZE_POPULATE_SLICES`]);
/// - then the volume recorded and sealed ([`verbs::finish_making`]) and published before it is served (A-343), a live
///   log's shipper made from the head the takeover adopted (A-329), so no client meets it unpublished.
///
/// Before, a live volume's restore and making were each one turn (0.14 to 0.32 s at 300 MB on a Mac Studio), and a
/// plain volume's restore and making were one turn behind a call whose deadline was a period (the 1.99 GB kernel base
/// volume, seconds of one poll, past the watchdog's spin window).
async fn make_in_turns(making: MakeTaken, source: Source) -> Result<(), &'static str> {
  let MakeTaken {
    object,
    id,
    head,
    catalog,
    catalog_sequence,
    placed,
    catalog_placed,
    region,
    past_tolerance,
    moved,
  } = making;
  let (archive, adopted) = match source {
    Source::Live(fetched) => {
      let (archive, adopted) = rebuild_in_turns(object, id, &catalog, fetched).await?;
      (Some(archive), Some(adopted))
    }
    Source::Archive {
      archive,
      lineage,
      naming,
    } => (
      archive_opened(object, id, &catalog, (archive, lineage, naming)).await?,
      None,
    ),
  };
  let from_live_log = adopted.is_some();
  futures::yield_now().await;
  // A volume made here already (a retry after its publication was refused) restores and lays nothing.
  let made = making_turn(object, |s| {
    past_the_pressure_hold(s, |s| verbs::made_already(s, id, &catalog))
      .map_err(|refusal| materialize_refusal_counter(&refusal))
  })??;
  // The takeover is recorded with the volume (AC-8.3, A-325): its loss report then survives this node's restart.
  let (epoch, sequence) = (placed.epoch.0, placed.sequence);
  let taken_over = || verbs::TakenOver {
    head: &head,
    catalog: &catalog,
    catalog_sequence,
    sequence,
    takeover: slates_db::catalog::TakeoverRecord {
      epoch,
      head_sequence: sequence,
      from_live_log,
      past_tolerance,
    },
    moved,
  };
  if !made {
    restore_and_begin((object, id), &catalog, archive, &taken_over()).await?;
  }
  futures::yield_now().await;
  making_turn(object, |s| {
    if !made {
      let taken = taken_over();
      past_the_pressure_hold(s, |s| verbs::finish_making(s, id, &taken, region))
        .map_err(|refusal| materialize_refusal_counter(&refusal))?;
    }
    // A live log's successor goes on from the head it adopted (A-329), past every position durable before it.
    if let Some(adopted) = adopted {
      crate::live_ship::adopt_head(s, id, adopted);
    }
    // The owner shard now owns the head's and the catalog's placements: its record plane writes the object's next
    // records at the promotion epoch and ships only to holders still missing these.
    s.placed_heads.insert(object, placed);
    s.placed_heads.insert(object.catalog(), catalog_placed);
    // Published before it is served (A-343): its image is what a restart rebuilds it from, and a live log's shipper is
    // made from the adopted head at once. A live volume is counted taken over only then, in this turn: a test that
    // restarts the successor once the count rises found a volume made and published.
    published_before_served(s)?;
    if from_live_log {
      crate::live_ship::taken_over(s);
    }
    Ok(())
  })?
}

/// The making of a taken-over volume not made here yet ([`make_in_turns`]): its archive, when there is one, restored a
/// slice a turn; the volume begun under `taken`; then the restored tree laid in a slice a turn. The archive is let go
/// once restored, before the volume is begun.
async fn restore_and_begin(
  (object, id): (ObjectId, DbVolumeId),
  catalog: &crate::catalog::CatalogValue,
  archive: Option<Archive>,
  taken: &verbs::TakenOver<'_>,
) -> Result<(), &'static str> {
  let restored = match archive.as_ref() {
    Some(archive) => Some(restore_in_turns((object, id), catalog, archive).await?),
    None => None,
  };
  drop(archive);
  futures::yield_now().await;
  making_turn(object, |s| {
    past_the_pressure_hold(s, |s| verbs::begin_making(s, id, taken))
      .map_err(|refusal| materialize_refusal_counter(&refusal))
  })??;
  if let Some(restored) = restored.as_ref() {
    populate_in_turns(object, id, restored).await?;
  }
  Ok(())
}

/// A live log's rebuild on its owner shard, its payloads opened and applied and its archive's content hashed a slice a
/// turn, each held to the turn's share of the seal quantum (`config::wrap_slice_ns`, a step at least: a sealed page opened
/// or hashed, a record applied, a run laid over, a page of content hashed: [`crate::live_ship::Rebuilding`], A-771),
/// counted [`LIVE_REBUILD_SLICES`]; then the rebuilt volume laid out as an archive, a turn, its content hashed already. The archive, and the head the
/// takeover adopted. Before, all of it ran in one owner-shard turn: on a Mac Studio the nemesis's successors took 0.68
/// to 1.08 s to rebuild 152 to 306 MB of log, past the watchdog's spin window, which ended the successor's process.
async fn rebuild_in_turns(
  object: ObjectId,
  id: DbVolumeId,
  catalog: &crate::catalog::CatalogValue,
  fetched: crate::live_ship::Fetched,
) -> Result<(Archive, slates_db::live::Head), &'static str> {
  let crate::live_ship::Fetched {
    head: adopted,
    log,
    lineage,
    naming,
    ..
  } = fetched;
  let begun = state::with_state(|s| {
    // A copy this node froze for a move it made (A-291) goes before the rebuild adopts the volume's keys
    // ([`let_frozen_go`]).
    let_frozen_go(s, id)?;
    let begun = crate::live_ship::Rebuilding::begin(s, id, &catalog.owner, (lineage, naming), log);
    begun.map_err(|why| crate::live_ship::rebuild_refused(s, why))
  })
  .ok_or(MATERIALIZE_UNANSWERED)?;
  let mut rebuilding = begun?;
  let _ = state::with_state_counted(|s| s.count(LIVE_REBUILDS_BEGUN, 1));
  loop {
    let slice_ns = state::with_state(|s| making_slice_ns(s)).ok_or(MATERIALIZE_UNANSWERED)?;
    let began = futures::now_ns();
    let before = rebuilding.applied();
    let advanced = rebuilding.advance(|| futures::now_ns().saturating_sub(began) >= slice_ns);
    let applied = rebuilding.applied().saturating_sub(before);
    let done = making_turn(object, |s| {
      s.count(LIVE_REBUILD_SLICES, 1);
      s.count(
        LIVE_REBUILD_PAYLOADS,
        u64::try_from(applied).unwrap_or(u64::MAX),
      );
      advanced.map_err(|why| crate::live_ship::rebuild_refused(s, why))
    })??;
    if done {
      break;
    }
    futures::yield_now().await;
    injected_turn_pause().await;
  }
  futures::yield_now().await;
  let archive = rebuilding.archive();
  let archive = making_turn(object, |s| {
    archive.map_err(|why| crate::live_ship::rebuild_refused(s, why))
  })??;
  Ok((archive, adopted))
}

/// A taken-over volume's archive restored on its owner shard a slice a turn (A-391): its tree planned
/// (`slates_archive::Planning`), the restore admitted against what the volume may hold and the shard can admit once its
/// need is known ([`verbs::restore_admitted`]), then restored (`slates_archive::Restoring`), each turn held to the
/// turn's share of the seal quantum ([`making_slice_ns`], a piece at least) and counted
/// [`MATERIALIZE_RESTORE_SLICES`]. A restore over its budget is refused `BudgetExceeded` naming the shard's room, a
/// malformed archive `BadRequest` ([`verbs::restore_refused`]).
async fn restore_in_turns(
  (object, id): (ObjectId, DbVolumeId),
  catalog: &crate::catalog::CatalogValue,
  archive: &Archive,
) -> Result<slates_archive::Restored, &'static str> {
  let mut planning = slates_archive::Planning::new(archive);
  let planned = loop {
    let slice_ns = state::with_state(|s| making_slice_ns(s)).ok_or(MATERIALIZE_UNANSWERED)?;
    let began = futures::now_ns();
    let stepped = planning.step(|| futures::now_ns().saturating_sub(began) >= slice_ns);
    let planned = making_turn(object, |s| {
      s.count(MATERIALIZE_RESTORE_SLICES, 1);
      stepped.map_err(|e| materialize_refusal_counter(&verbs::restore_refused(e, 0)))
    })??;
    if let Some(planned) = planned {
      break planned;
    }
    futures::yield_now().await;
  };
  futures::yield_now().await;
  let mut restoring = making_turn(object, |s| {
    let (budget, room) = past_the_pressure_hold(s, |s| {
      verbs::restore_admitted(s, id, catalog, planned.needed())
    });
    planned
      .restoring(budget)
      .map_err(|e| materialize_refusal_counter(&verbs::restore_refused(e, room)))
  })??;
  loop {
    futures::yield_now().await;
    let slice_ns = state::with_state(|s| making_slice_ns(s)).ok_or(MATERIALIZE_UNANSWERED)?;
    let began = futures::now_ns();
    let stepped = restoring.step(|| futures::now_ns().saturating_sub(began) >= slice_ns);
    let restored = making_turn(object, |s| {
      s.count(MATERIALIZE_RESTORE_SLICES, 1);
      stepped.map_err(|e| materialize_refusal_counter(&verbs::restore_refused(e, 0)))
    })??;
    if let Some(restored) = restored {
      return Ok(restored);
    }
  }
}

/// A taken-over volume's restored tree laid into the volume being made on its owner shard a slice a turn
/// ([`verbs::populate_making`]): each turn held to the turn's share of the seal quantum ([`making_slice_ns`], a piece at
/// least), each write to a copy slice of the same quantum (`DaemonConfig::copy_slice_bytes_within`, a page at least),
/// counted [`MATERIALIZE_POPULATE_SLICES`].
async fn populate_in_turns(
  object: ObjectId,
  id: DbVolumeId,
  restored: &slates_archive::Restored,
) -> Result<(), &'static str> {
  let mut populating = verbs::Populating::new();
  loop {
    futures::yield_now().await;
    let laid = making_turn(object, |s| {
      let quantum_ns = seal_quantum_ns(s);
      let slice_ns = crate::config::wrap_slice_ns(quantum_ns);
      let slice_bytes = usize::try_from(s.config.copy_slice_bytes_within(quantum_ns))
        .unwrap_or(usize::MAX)
        .max(s.config.page);
      s.count(MATERIALIZE_POPULATE_SLICES, 1);
      let began = futures::now_ns();
      past_the_pressure_hold(s, |s| {
        verbs::populate_making(s, id, &mut populating, restored, slice_bytes, || {
          futures::now_ns().saturating_sub(began) >= slice_ns
        })
      })
      .map_err(|refusal| materialize_refusal_counter(&refusal))
    })??;
    if laid {
      return Ok(());
    }
  }
}

/// Counter: a taken-over volume's materialization begun on its owner shard (g251): once a takeover, however many attempts
/// its control shard makes while it runs.
/// Format: a counter name in the daemon's status report.
const MATERIALIZE_BEGUN: &str = "fleet.materialize.begun";
/// Counter: one owner-shard turn's slice of a taken-over volume's restore, planned or restored (A-391, g251).
/// Format: a counter name in the daemon's status report.
const MATERIALIZE_RESTORE_SLICES: &str = "fleet.materialize.restore_slices";
/// Counter: one owner-shard turn's slice of a taken-over volume's tree laid into it (g251).
/// Format: a counter name in the daemon's status report.
const MATERIALIZE_POPULATE_SLICES: &str = "fleet.materialize.populate_slices";
/// Refusal: a materialization's turn found its record forgotten: the control shard gave the takeover up
/// ([`give_up_taken_over`]), so the volume being made is discarded and the task stops.
/// Format: a counter name in the daemon's status report.
const MAKING_FORGOTTEN: &str = "fleet.materialize.forgotten";

/// Counter: one owner-shard turn's slice of a live volume's rebuild from its log ([`rebuild_in_turns`], g251).
/// Format: a counter name in the daemon's status report.
const LIVE_REBUILD_SLICES: &str = "fleet.live.rebuild_slices";
/// Counter: the payloads a live volume's rebuild applied, in all its turns (g251): with [`LIVE_REBUILD_SLICES`], how
/// many a turn took.
/// Format: a counter name in the daemon's status report.
const LIVE_REBUILD_PAYLOADS: &str = "fleet.live.rebuild_payloads";
/// Counter: a live volume's rebuild from its log begun on its owner shard (g251): once a takeover, however many attempts
/// its control shard makes while it runs.
/// Format: a counter name in the daemon's status report.
const LIVE_REBUILDS_BEGUN: &str = "fleet.live.rebuilds_begun";
/// Counter: a test's wait between a live rebuild's turns the runtime could not time; the next turn went at once.
/// Format: a counter name in the daemon's status report.
const LIVE_TURN_PAUSE_UNTIMED: &str = "fleet.live.turn_pause_untimed";
/// Counter: an attempt that found its volume's live materialization running on the owner shard and joined it (g251).
/// Format: a counter name in the daemon's status report.
const MATERIALIZE_WAITS_MAKING: &str = "fleet.materialize.waits.making";
/// Counter: a live materialization begun while another of the same volume ran on its owner shard, which the control
/// shard joins instead (g251).
/// Format: a counter name in the daemon's status report.
const MAKING_ALREADY: &str = "fleet.materialize.making_already";

/// What a taken-over volume's materialization waited on when an attempt could not finish ([`materialize_pending`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VolumeWait {
  /// The content its head names is not held whole here, and this period's fetch did not complete it.
  Content,
  /// No placement of its adopted head here.
  Placement,
  /// Its catalog register is not adopted (AUD-29-17).
  Catalog,
  /// Its adopted catalog register's placement is not here.
  CatalogPlacement,
  /// No runtime shard for the partition its id names.
  Shard,
  /// Its live log could not be fetched from its holders (A-200, A-240).
  Live,
  /// The owner shard refused the materialization, or did not answer within the period.
  Refused,
  /// Its materialization runs in turns on the owner shard ([`make_taken`], g251).
  Making,
}

impl VolumeWait {
  /// The counter an attempt that waited so moves where [`materialize`] counts the wait itself (the fetch and the live
  /// log count their own reasons, and a refusal its kind).
  const fn counter(self) -> &'static str {
    match self {
      VolumeWait::Content => MATERIALIZE_WAITS_CONTENT,
      VolumeWait::Placement => MATERIALIZE_WAITS_PLACEMENT,
      VolumeWait::Catalog => MATERIALIZE_WAITS_CATALOG,
      VolumeWait::CatalogPlacement => MATERIALIZE_WAITS_CATALOG_PLACEMENT,
      VolumeWait::Shard => MATERIALIZE_WAITS_SHARD,
      VolumeWait::Live | VolumeWait::Refused => MATERIALIZE_REFUSED,
      VolumeWait::Making => MATERIALIZE_WAITS_MAKING,
    }
  }

  /// Whether it waits for a register the takeover round that adopted the volume may still bring on a later page: its
  /// catalog register or that register's placement ([`paging_round`]).
  const fn waits_on_the_round(self) -> bool {
    matches!(self, VolumeWait::Catalog | VolumeWait::CatalogPlacement)
  }

  /// Its name in a refusal's cause (`RecoveryCause::TakeoverIncomplete`).
  const fn named(self) -> &'static str {
    match self {
      VolumeWait::Content => "content",
      VolumeWait::Placement => "placement",
      VolumeWait::Catalog => "catalog",
      VolumeWait::CatalogPlacement => "catalog_placement",
      VolumeWait::Shard => "shard",
      VolumeWait::Live => "live",
      VolumeWait::Refused => "refused",
      VolumeWait::Making => "making",
    }
  }
}

/// The counter a taken-over volume's refused materialization is recorded under, by the owner shard's refusal: a name
/// already taken on this node, an archive or volume past the shard's budget, a restore that refused the archive, or
/// another refusal.
fn materialize_refusal_counter(refusal: &slates_ipc::protocol::ReplyBody) -> &'static str {
  match refusal {
    slates_ipc::protocol::ReplyBody::Refused { refusal } => match refusal {
      slates_ipc::protocol::Refusal::AlreadyExists { .. } => {
        "fleet.materialize.refused.already_exists"
      }
      slates_ipc::protocol::Refusal::BudgetExceeded { .. } => {
        "fleet.materialize.refused.budget_exceeded"
      }
      slates_ipc::protocol::Refusal::BadRequest { .. } => "fleet.materialize.refused.bad_request",
      _ => "fleet.materialize.refused.other",
    },
    _ => "fleet.materialize.refused.other",
  }
}

/// Counter: the owner shard did not answer a taken-over volume's materialization within the period.
/// Format: a counter name in the daemon's status report.
const MATERIALIZE_UNANSWERED: &str = "fleet.materialize.unanswered";
/// Counter: a taken-over volume materialized whose shard image could not be published (`published_before_served`):
/// the materialization is tried again next period, which keeps the volume and publishes again.
/// Format: a counter name in the daemon's status report.
const MATERIALIZE_UNPUBLISHED: &str = "fleet.materialize.unpublished";

/// Publishes the owner shard's image once a taken-over volume is materialized there, before it is served (§4.8
/// "persistence before reply"; A-325, A-343): the image in the anchor is what a restart of this node rebuilds the volume
/// from. Until 2026-11-18 nothing published it until the volume's first write, and a successor restarted before then
/// found no image for it (`NoImage`) and never served it again, though its records named it the owner. A live volume's
/// publication also makes its shipper, from the head its takeover adopted (A-329), so the successor ships its log from
/// the takeover on. A refused publication is [`MATERIALIZE_UNPUBLISHED`]: the materialization stays pending, and its next
/// attempt finds the volume made (`verbs::materialize_taken_over` keeps it) and publishes again.
fn published_before_served(s: &mut crate::state::ShardState) -> Result<(), &'static str> {
  crate::verbs::publish_shard(s)
    .map(|_| ())
    .map_err(|_| MATERIALIZE_UNPUBLISHED)
}

/// The archive a taken-over volume is served from: a plain archive as it is; an envelope, which [`open_in_turns`] opens
/// before this, never: ciphertext is never served as a volume's files.
fn opened_archive(archive: Archive) -> Option<Archive> {
  (!slates_cluster::envelope::is_envelope(&archive)).then_some(archive)
}

/// A taken-over volume's materialization refused before its owner shard made it (an envelope that would not open):
/// counted on this (control) shard as the owner shard's refusals are, and the materialization stays pending.
fn materialize_refused(refusal: &'static str) -> Result<(), VolumeWait> {
  let counted = state::with_state(|s| {
    s.count(MATERIALIZE_REFUSED, 1);
    s.count(refusal, 1);
  });
  match counted {
    Some(()) => Err(VolumeWait::Refused),
    // The control shard's state out of reach (its shutdown): nothing more is attempted.
    None => Ok(()),
  }
}

/// What one owner-shard turn of an envelope's open came to ([`open_slice`]).
enum Sliced {
  /// Every chunk is open: the real archive.
  Opened(Archive),
  /// Chunks remain: the open, to go on with at the next turn (boxed: it carries the envelope and the real archive).
  More(Box<slates_cluster::envelope::Opening>),
}

/// The successor's adoption of a taken-over volume's keys (A-92 pieces 3b and 4c): the volume's owner, its lineage key
/// under this node's root (none when the head carried no entry for this node), and the head's naming key.
type Adoption = (
  slates_db::catalog::Principal,
  Option<([u8; 16], Vec<u8>)>,
  Option<crate::head::HeadNaming>,
);

/// Opens a taken-over volume's sealed envelope on its owner shard `target`, in turns of that shard's clock (A-347): its
/// lineage key adopted first, then the sealed manifest opened (the real archive's tree, never its content), then its
/// chunks a slice a turn ([`open_slice`]), each a turn of the owner shard of its own however the shards fall
/// ([`crate::xshard::Turns`]). The real archive, or the refusal, counted: [`ENVELOPE_UNOPENED`] for a key this node could
/// not adopt or an envelope that would not open, [`MATERIALIZE_UNANSWERED`] for a turn the owner shard did not take
/// within its wait. An open refused or cut short starts again from the envelope at the next attempt.
async fn open_in_turns(
  origin: u16,
  target: u16,
  id: DbVolumeId,
  (owner, lineage, naming): Adoption,
  archive: Option<Archive>,
) -> Result<Option<Archive>, &'static str> {
  let Some(archive) = archive else {
    return Ok(None);
  };
  let mut turns = crate::xshard::Turns::new(origin, target);
  let begun_owner = owner.clone();
  let begun = turns
    .call(
      move |s| {
        // A copy this node froze for a move it made (A-291) goes first: its release erases the volume's keys.
        let_frozen_go(s, id)?;
        // A head that carried no entry for this node leaves the envelope unopened: its keys are not this node's.
        let adopted = lineage.is_some_and(|lineage| {
          let adopted =
            crate::seal_keys::adopt_lineage(s, id, &begun_owner, lineage, naming.as_ref()).is_ok();
          if !adopted {
            s.count(LINEAGE_UNADOPTED, 1);
          }
          adopted
        });
        let opening = adopted
          .then(|| {
            let (lineage, namer) = recorded_keys(s, id, &begun_owner)?;
            slates_cluster::envelope::Opening::new(archive, &lineage, &namer)
              .ok()
              .map(Box::new)
          })
          .flatten();
        if opening.is_none() {
          s.count(ENVELOPE_UNOPENED, 1);
        }
        opening.ok_or(ENVELOPE_UNOPENED)
      },
      HEARTBEAT_NS,
    )
    .await;
  let mut opening = match begun {
    Some(Ok(opening)) => opening,
    Some(Err(refusal)) => return Err(refusal),
    None => return Err(MATERIALIZE_UNANSWERED),
  };
  loop {
    let slice_owner = owner.clone();
    let sliced = turns
      .call(
        move |s| open_slice(s, id, &slice_owner, opening),
        HEARTBEAT_NS,
      )
      .await;
    match sliced {
      Some(Ok(Sliced::Opened(opened))) => return Ok(Some(opened)),
      Some(Ok(Sliced::More(rest))) => opening = rest,
      Some(Err(refusal)) => return Err(refusal),
      None => return Err(MATERIALIZE_UNANSWERED),
    }
  }
}

/// The lineage key and the namer a taken-over volume's envelope opens under, as recorded on this shard; `None` when
/// either is not.
fn recorded_keys(
  state: &mut ShardState,
  id: DbVolumeId,
  owner: &slates_db::catalog::Principal,
) -> Option<(hyper_seal::keys::WrappingKey, hyper_seal::name::Namer)> {
  let account = crate::seal_keys::tenant_of(owner);
  let lineage = crate::seal_keys::recorded_lineage(state, id, account).ok()??;
  let namer = crate::seal_keys::recorded_namer(state, id, account).ok()??;
  Some((lineage, namer))
}

/// The quantum a seal's turns are cut by on this shard (its archive walk's, its envelope's wrap and, on a successor, an
/// envelope's open and a live log's rebuild), a takeover fetch's answer (`live_ship::FetchAnswer`) and a live whole
/// entry's build (`live_ship::WholeBuild`, A-377): the shard's step quantum, or the one a test injected
/// (`Daemon::inject_seal_walk_quantum`).
pub(crate) fn seal_quantum_ns(state: &ShardState) -> u64 {
  state
    .seal_walk_quantum_ns
    .or_else(futures::step_budget_ns)
    .unwrap_or(state.config.runtime.step_budget_ns)
}

/// One owner-shard turn of [`open_in_turns`]: `opening`'s next chunks opened until the turn's share of the seal quantum
/// is spent (the wrap's share, `config::wrap_slice_ns`: an open costs a seal's open and a BLAKE3 pass a byte, as the
/// wrap does), a chunk at least, so a turn always moves the open on; counted [`ENVELOPE_OPEN_SLICES`].
fn open_slice(
  state: &mut ShardState,
  id: DbVolumeId,
  owner: &slates_db::catalog::Principal,
  mut opening: Box<slates_cluster::envelope::Opening>,
) -> Result<Sliced, &'static str> {
  state.count(ENVELOPE_OPEN_SLICES, 1);
  let Some((lineage, namer)) = recorded_keys(state, id, owner) else {
    state.count(ENVELOPE_UNOPENED, 1);
    return Err(ENVELOPE_UNOPENED);
  };
  let open_ns = crate::config::wrap_slice_ns(seal_quantum_ns(state));
  let began = futures::now_ns();
  loop {
    // A budget of one byte opens exactly one chunk, so the clock is read between chunks.
    match opening.advance(&lineage, &namer, 1) {
      Ok(Some(opened)) => return Ok(Sliced::Opened(opened)),
      Ok(None) if futures::now_ns().saturating_sub(began) < open_ns => {}
      Ok(None) => return Ok(Sliced::More(opening)),
      Err(_) => {
        state.count(ENVELOPE_UNOPENED, 1);
        return Err(ENVELOPE_UNOPENED);
      }
    }
  }
}

/// Counter: one owner-shard turn's slice of a taken-over volume's envelope open ([`open_slice`], A-347).
/// Format: a counter name in the daemon's status report.
const ENVELOPE_OPEN_SLICES: &str = "fleet.seal.open_slices";

/// Counter: a volume whose lineage key could not be made at its seal's start (A-92 piece 4c); its head then carries no
/// key for successors until a later seal.
/// Format: a counter name in the daemon's status report.
const LINEAGE_UNMADE: &str = "fleet.seal.lineage_unmade";
/// Counter: a seal whose archive could not be wrapped in its envelope though this node seals (a key that would not
/// unwrap or record, a seal that refused; A-92 piece 3b): the seal is dropped and started again next period.
/// Format: a counter name in the daemon's status report.
const SEAL_ENVELOPE_REFUSED: &str = "fleet.seal.envelope_refused";
/// Counter: a taken-over volume whose envelope this node could not open (no key for it in the head, a key that would
/// not adopt, or a refused open; A-92 piece 3b): the volume is not materialized from it.
/// Format: a counter name in the daemon's status report.
const ENVELOPE_UNOPENED: &str = "fleet.seal.envelope_unopened";
/// Counter: a taken-over volume whose envelope this successor had no entry of its own for, opened with the lineage key
/// a fellow candidate handed it (A-295): once a key handed.
/// Format: a counter name in the daemon's status report.
const ENVELOPE_KEY_HANDED: &str = "fleet.seal.envelope_key_handed";
/// Counter: a taken-over volume whose lineage key the successor could not record (A-92 piece 4c).
/// Format: a counter name in the daemon's status report.
const LINEAGE_UNADOPTED: &str = "fleet.seal.lineage_unadopted";

/// The status refusal count under which the fleet loop records that it could not serve on its bound sockets
/// at boot — the control shard would not keep its identity, a socket could not name its port, or a
/// demultiplexer would not start (§4.14: a refusal is counted, never silent) — the node then takes no part
/// in the fleet. A socket that could not be bound at all refused the daemon's start (`ServeBindError`).
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
const BIND_REFUSED: &str = "fleet.bind";

/// The status refusal count under which the fleet loop records an accepted session whose authenticated
/// peer is not in the roster (never served).
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
const ACCEPT_REFUSED: &str = "fleet.accept";
/// An accepted session whose serve task the shard's arena refused (the fleet's task share, sized for
/// every session the demultiplexer can hold, was exceeded): the session is dropped and the peer
/// re-dials. A count here under a load the arena should carry is a sizing defect, not a peer's.
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
const SERVE_SPAWN_REFUSED: &str = "fleet.serve_spawn";
/// A perpetual fleet loop the shard's arena refused at boot — its task budget spent before the fleet's own
/// loops were admitted: a plane's receive or accept loop, a peer's probe or record link, or the
/// coordinator. Counted, never silent (banned item 9); the node then runs without that loop, which the
/// mesh's failure to form to it makes visible.
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
const LOOP_SPAWN_REFUSED: &str = "fleet.loop_spawn";

/// Spawns a perpetual fleet task and detaches it (a joinable task stays in the arena after it ends, which
/// would hold the shard's shutdown); a spawn the arena refuses is counted under `refused` — a refusal is
/// counted, never swallowed (§4.14, banned item 9). Before this every fleet loop's spawn was `if let Ok`,
/// so a refused loop left a node silently without a plane, a peer link or its coordinator.
/// The loop is labelled with the line that called this (A-740: `#[track_caller]` carries the caller's site into
/// `futures::spawn`), so a shard found stuck in a fleet loop names that loop, not this helper.
#[track_caller]
fn spawn_detached(future: impl std::future::Future<Output = ()> + 'static, refused: &'static str) {
  match futures::spawn(future) {
    Ok(task) => {
      let _ = futures::detach(task);
    }
    Err(_) => {
      count_refusal(refused);
    }
  }
}

/// The status refusal count under which the fleet loop records a serve socket whose receive loop ended
/// because the socket refused — the node no longer accepts sessions on that plane.
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
const SERVE_REFUSED: &str = "fleet.serve";
/// The status refusal counts of the serve sockets' refused receives that did not end their loops, summed over the
/// planes, one per kind (`slates_rt::error::ReceiveRefusal`; `DemuxCounters::receive_refused_earlier_send` and its
/// siblings): an earlier send's report, the OS short of memory, anything else.
/// Format: refusal names in the daemon's status report.
pub(crate) const SERVE_RECEIVE_EARLIER_SEND: &str = "fleet.serve.receive_refused.earlier_send";
/// See [`SERVE_RECEIVE_EARLIER_SEND`].
/// Format: a refusal name in the daemon's status report.
pub(crate) const SERVE_RECEIVE_RESOURCES: &str = "fleet.serve.receive_refused.resources";
/// See [`SERVE_RECEIVE_EARLIER_SEND`].
/// Format: a refusal name in the daemon's status report.
pub(crate) const SERVE_RECEIVE_OTHER: &str = "fleet.serve.receive_refused.other";
/// The status counts of the serve sockets' demultiplexers that status's fleet section does not already carry (it has
/// `unknown_id`, `inbox_full`, `sessions_refused` and `replaced`), summed over the planes (`DemuxCounters`): sessions
/// opened for a new source, handshakes refused because the session could not be built, authenticated dials refused
/// because their certificate still held both its sessions, identities refused at the peer bound, and the pool's
/// high-water mark. A dialer that never hears back may be refused here, and nothing on its own side says so.
/// Format: refusal names in the daemon's status report.
pub(crate) const SERVE_OPENED: &str = "fleet.serve.opened";
/// See [`SERVE_OPENED`].
/// Format: a refusal name in the daemon's status report.
pub(crate) const SERVE_SETUP_REFUSED: &str = "fleet.serve.setup_refused";
/// See [`SERVE_OPENED`].
/// Format: a refusal name in the daemon's status report.
pub(crate) const SERVE_PEER_SESSIONS_REFUSED: &str = "fleet.serve.peer_sessions_refused";
/// See [`SERVE_OPENED`].
/// Format: a refusal name in the daemon's status report.
pub(crate) const SERVE_PEERS_REFUSED: &str = "fleet.serve.peers_refused";
/// See [`SERVE_OPENED`].
/// Format: a refusal name in the daemon's status report.
pub(crate) const SERVE_HIGH_WATER: &str = "fleet.serve.high_water";

/// The record links' dials in status ([`crate::state::RecordDial`]): how many links are dialing for a session they lack
/// now, the longest handshake call running now in milliseconds, the longest first discovery page awaited now in
/// milliseconds, and the handshake calls made since boot. The first three are readings at the report, not counts: a
/// call running past a probe timeout or two is one whose peer has not answered.
/// Format: refusal names in the daemon's status report.
pub(crate) const DIAL_RECORD_DIALING: &str = "fleet.dial.record.dialing";
/// See [`DIAL_RECORD_DIALING`].
/// Format: a refusal name in the daemon's status report.
pub(crate) const DIAL_RECORD_LONGEST_CALL_MS: &str = "fleet.dial.record.longest_call_ms";
/// See [`DIAL_RECORD_DIALING`].
/// Format: a refusal name in the daemon's status report.
pub(crate) const DIAL_RECORD_LONGEST_ENROLL_MS: &str = "fleet.dial.record.longest_enroll_ms";
/// See [`DIAL_RECORD_DIALING`].
/// Format: a refusal name in the daemon's status report.
pub(crate) const DIAL_RECORD_CALLS: &str = "fleet.dial.record.calls";

/// How many record links are dialing for a session they lack now ([`DIAL_RECORD_DIALING`]).
pub(crate) fn record_dials_dialing(state: &ShardState) -> u64 {
  let dialing = state
    .record_dials
    .values()
    .filter(|dial| dial.dialing_since_ns.is_some())
    .count();
  u64::try_from(dialing).unwrap_or(u64::MAX)
}

/// The longest wait running now among the record dials, read by `since` from each, in milliseconds
/// ([`DIAL_RECORD_LONGEST_CALL_MS`], [`DIAL_RECORD_LONGEST_ENROLL_MS`]).
pub(crate) fn record_dials_longest_ms(
  state: &ShardState,
  since: impl Fn(&crate::state::RecordDial) -> Option<u64>,
) -> u64 {
  let now = futures::now_ns();
  state
    .record_dials
    .values()
    .filter_map(since)
    .map(|began| now.saturating_sub(began) / NS_PER_MS)
    .max()
    .unwrap_or(0)
}

/// The handshake calls the record links have made since boot ([`DIAL_RECORD_CALLS`]).
pub(crate) fn record_dials_calls(state: &ShardState) -> u64 {
  state
    .record_dials
    .values()
    .fold(0u64, |sum, dial| sum.saturating_add(dial.calls))
}

/// A dial still in its handshake dropped at its peer's retirement, so the resume dials afresh at the
/// peer's current address.
const DIAL_STALE_DROPPED: &str = "fleet.dial.stale_dropped";
/// A discovery exchange that reached its deadline unanswered: its session released, the link re-dials
/// (`docs/bugs/2026-09-16-discovery-await-strands-a-replacement-raft-voter.md`).
const DISCOVERY_DEADLINE: &str = "fleet.discovery.deadline";
/// A discovery exchange invalidated while it waited: its peer's member id changed, or the peer was retired.
const DISCOVERY_INVALIDATED: &str = "fleet.discovery.invalidated";
/// A discovery exchange whose transport failed terminally.
const DISCOVERY_TRANSPORT: &str = "fleet.discovery.transport";
/// A discovery exchange whose deadline could not be armed ([`DiscoveryFault::Unbounded`]): a tripwire.
const DISCOVERY_UNBOUNDED: &str = "fleet.discovery.unbounded";
/// A record session returned to a slot that no longer expects it — a retired peer's, a slot re-established
/// since, or a session other than the one borrowed — dropped rather than installed over a newer one.
const LINK_STALE_RETURN: &str = "fleet.link.stale_return";

/// Counter: a borrowed record session returned reset by its peer (A-261, `EndpointError::PeerReset`: the peer restarted
/// under the same identity and no longer holds it), its link dropped so the link task dials the new process.
/// Format: a counter name in the daemon's status report.
const LINK_PEER_RESET: &str = "fleet.link.peer_reset";

/// Counter: a served record session its dialer reset (A-261: a process restarted onto the dialer's port, under its
/// identity), its serve loop ended.
/// Format: a counter name in the daemon's status report.
const SERVE_PEER_RESET: &str = "fleet.serve.peer_reset";
/// Why a campaign's round did not ask a voter ([`take_campaign_sessions`]), counted per voter per round: no
/// record link to it (never established, or removed at its retirement), its session lent to a coordinator
/// dispatch, or its session held by the link task itself (a discovery page, or a dial after one ended). So a
/// campaign that asked no one says why (GAPS 2026-09-29, a survivor's record session after a leader's loss).
const ELECTION_NO_LINK: &str = "fleet.election.no_link";
const ELECTION_SESSION_LENT: &str = "fleet.election.session_lent";
const ELECTION_SESSION_HELD: &str = "fleet.election.session_held";
/// A voter whose session was out of its link when a campaign's round began and came back within the round's
/// base deadline, so the round asked it ([`take_campaign_sessions`]): the wait's non-vacuity count.
const ELECTION_SESSION_AWAITED: &str = "fleet.election.session_awaited";
/// A pre-vote grant that arrived after its round ended ([`late_raft_reply`]): dropped, as the design has it
/// (the next campaign re-runs its pre-vote), and counted, so how often a campaign loses a grant to its
/// deadline is measured (GAPS 2026-09-29, a late pre-vote reply is dropped).
const ELECTION_LATE_PRE_VOTE_GRANT: &str = "fleet.election.late_pre_vote_grant";
const RESOLVE_REFUSED: &str = "fleet.resolve";
const RESOLVE_NO_RESOLVER: &str = "fleet.resolve.no-resolver";
/// A value the control shard keeps for its life (its identity, its name, the resolver) named by a task that
/// could not reach it — run off that shard, or after its context ended (AUD-29-08): a tripwire, never an
/// expected count, since every dial task runs on the control shard within its life.
const KEPT_UNREACHABLE: &str = "fleet.kept.unreachable";
const RESOLVE_TIMEOUT: &str = "fleet.resolve.timeout";
const RESOLVE_NXDOMAIN: &str = "fleet.resolve.refused";
const RESOLVE_NO_ADDRESS: &str = "fleet.resolve.no-address";
const RESOLVE_MALFORMED: &str = "fleet.resolve.malformed";
const RESOLVE_IO: &str = "fleet.resolve.io";

/// The status count of a forward that found its session held by the record link task itself (a discovery page or
/// a dial), not lent through [`take_sessions`]: the holder named beside `fleet.forward.session_out`.
pub(crate) const LENT_TO_LINK: &str = "fleet.lent_to.link";

/// The status refusal counts under which a dial task records a handshake that ran out its budgets with
/// the peer silent (`fleet.dial.redial`: the socket is dropped and the next period dials afresh) and one
/// the transport faulted (`fleet.dial.fault`: a peer whose half-open state for this source is gone, a
/// certificate the pin refuses, a malformed flight). A count that keeps rising names a peer this node
/// cannot establish a session with, and the log's first line of each says why.
/// Format: refusal names in the daemon's status report, alongside the verbs' refusal kinds.
const DIAL_REDIAL: &str = "fleet.dial.redial";
const DIAL_FAULT: &str = "fleet.dial.fault";
/// A dial that could not bind its socket.
const DIAL_SOCKET_REFUSED: &str = "fleet.dial.socket_refused";
/// A dial whose client endpoint could not be built.
const DIAL_CLIENT_REFUSED: &str = "fleet.dial.client_refused";

/// The counter for a dial's handshake budget spent, by which of the handshake's bounds spent it
/// (`slates_transport::endpoint::HandshakeStall`): a formation stalled on dials says here why they are not completing.
fn dial_stall_counter(stall: Option<HandshakeStall>) -> &'static str {
  match stall {
    Some(HandshakeStall::Silence) => "fleet.dial.stall.silence",
    Some(HandshakeStall::Repeats) => "fleet.dial.stall.repeats",
    Some(HandshakeStall::Fragments) => "fleet.dial.stall.fragments",
    Some(HandshakeStall::TurnCeiling) => "fleet.dial.stall.turn_ceiling",
    Some(HandshakeStall::ConfirmDeadline) => "fleet.dial.stall.confirm_deadline",
    Some(HandshakeStall::ConfirmSilence) => "fleet.dial.stall.confirm_silence",
    Some(HandshakeStall::ConfirmGarbage) => "fleet.dial.stall.confirm_garbage",
    None => "fleet.dial.stall.unnamed",
  }
}

/// The counter for an accepted handshake that gave up, by the bound that ended it ([`dial_stall_counter`]'s kinds).
fn accept_stall_counter(stall: Option<HandshakeStall>) -> &'static str {
  match stall {
    Some(HandshakeStall::Silence) => "fleet.accept.stall.silence",
    Some(HandshakeStall::Repeats) => "fleet.accept.stall.repeats",
    Some(HandshakeStall::Fragments) => "fleet.accept.stall.fragments",
    Some(HandshakeStall::TurnCeiling) => "fleet.accept.stall.turn_ceiling",
    Some(HandshakeStall::ConfirmDeadline) => "fleet.accept.stall.confirm_deadline",
    Some(HandshakeStall::ConfirmSilence) => "fleet.accept.stall.confirm_silence",
    Some(HandshakeStall::ConfirmGarbage) => "fleet.accept.stall.confirm_garbage",
    None => "fleet.accept.stall.unnamed",
  }
}

/// The counter for a dial's handshake that faulted, by the transport's typed reason.
fn dial_fault_counter(error: &EndpointError) -> &'static str {
  match error {
    EndpointError::Handshake(_) => "fleet.dial.fault.handshake",
    EndpointError::Tls(_) => "fleet.dial.fault.tls",
    EndpointError::Io(_) => "fleet.dial.fault.io",
    EndpointError::Closed => "fleet.dial.fault.closed",
    EndpointError::Admission(_) => "fleet.dial.fault.admission",
    EndpointError::PeerParameters(_) => "fleet.dial.fault.peer_parameters",
    EndpointError::PeerClosed => "fleet.dial.fault.peer_closed",
    _ => "fleet.dial.fault.other",
  }
}

/// The status refusal count under which the serve side records an accepted session whose handshake did
/// not complete (the dialer gave up, or its flight faulted); the log's first line says why.
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
const ACCEPT_HANDSHAKE_REFUSED: &str = "fleet.accept.handshake";

/// An accepted handshake superseded by the same authenticated peer's newer session (§4.10a).
/// Kept apart from TLS and socket failures: learning a fresh boot identity can cancel a seed dial.
const ACCEPT_REPLACED: &str = "fleet.accept.replaced";

/// A certificate still owns its live and replaced endpoints; its next authentication is refused.
const ACCEPT_PEER_SESSIONS: &str = "fleet.accept.peer_sessions";
/// Every authenticated peer reservation is held; a new identity cannot be admitted yet.
const ACCEPT_PEER_CAPACITY: &str = "fleet.accept.peer_capacity";

/// Classifies the transport's typed handshake result without hiding a real TLS or socket failure
/// behind an expected replacement. Both serve planes use the same distinction.
fn count_accept_failure(error: &EndpointError, stall: Option<HandshakeStall>, plane: &str) {
  if matches!(error, EndpointError::NotReady) {
    count_refusal(accept_stall_counter(stall));
  }
  let counter = match error {
    EndpointError::Closed => ACCEPT_REPLACED,
    EndpointError::Admission(slates_transport::demux::SessionRefusal::PeerSessions) => {
      ACCEPT_PEER_SESSIONS
    }
    EndpointError::Admission(slates_transport::demux::SessionRefusal::PeerCapacity) => {
      ACCEPT_PEER_CAPACITY
    }
    _ => ACCEPT_HANDSHAKE_REFUSED,
  };
  if count_refusal(counter) == 1 {
    slates_machine::log_line!(
      "slates-server: fleet: a dialer's {plane}-plane handshake ended: {error:?}"
    );
  }
}

/// Counts a fleet-loop refusal in the shard's status refusal counts, so a peer the loop could not set up is
/// visible to an operator (the mesh will not form to it) rather than a swallowed error (banned item 9).
pub(crate) fn count_refusal(kind: &'static str) -> u64 {
  state::with_state(|s| {
    let count = s.refusals.entry(kind).or_insert(0);
    *count = count.saturating_add(1);
    *count
  })
  .unwrap_or(0)
}

/// This node's **direct probe mesh** (§4.8): the members its membership holds alive that it keeps direct contact with
/// ([`keeps_direct_contact_with`]), this node left out, and those of them with no formed probe session yet. Over the
/// alive membership, not the council's configuration: from the bootstrap until the others are admitted the configuration
/// names the bootstrapping node alone, so a mesh judged over it was formed over nothing, and a fleet test's formation
/// wait could pass before any session existed (live-fixes, 2026-11-07, red in 38 s; status showed `in_contact=0
/// probed=4` the same way, diag-live-fixes-32).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ProbeMesh {
  /// The peers held alive that this node keeps direct contact with.
  pub(crate) in_contact: Vec<HostId>,
  /// Those with no formed probe session.
  pub(crate) unmeshed: Vec<HostId>,
  /// Every member held alive, this node among them.
  pub(crate) alive: Vec<HostId>,
  /// Whether the membership knows any member but this node, alive, suspected or dead.
  pub(crate) others_known: bool,
}

impl ProbeMesh {
  /// Whether the mesh is up: every peer in contact has a formed probe session, and once the membership knows another
  /// member at least one peer is in contact, so a fleet of several members never counts itself meshed over no peer
  /// at all (its peers not reached yet, or all of them dead). A laptop, which knows no other member, is meshed (R8).
  pub(crate) fn meshed(&self) -> bool {
    self.unmeshed.is_empty() && (!self.others_known || !self.in_contact.is_empty())
  }
}

/// This node's probe mesh as it stands ([`ProbeMesh`]).
pub(crate) fn probe_mesh(state: &ShardState) -> ProbeMesh {
  let host = state.fleet.host();
  let membership = state.fleet.membership();
  let alive = membership.alive();
  let others_known = alive.iter().any(|member| *member != host)
    || !membership.suspects().is_empty()
    || !membership.dead().is_empty();
  let in_contact: Vec<HostId> = alive
    .iter()
    .copied()
    .filter(|peer| *peer != host && keeps_direct_contact_with(state, *peer))
    .collect();
  let unmeshed = in_contact
    .iter()
    .copied()
    .filter(|peer| !state.formed_probe_peers.contains(peer))
    .collect();
  ProbeMesh {
    in_contact,
    unmeshed,
    alive,
    others_known,
  }
}

/// Whether this node keeps **direct contact** with `peer` — a record session dialed and kept up for the
/// coordinator ([`establish_record_link`]) and a SWIM probe of its own: the peers the
/// coordinator ever borrows a session to ([`take_sessions`]) and whose liveness it must see first-hand. That
/// is a **candidate holder** in this owner's bounded record neighbourhood (§4.8, D-14 — the copyset,
/// `select_neighbourhood` at the scatter width, so at the candidate floor `2f + 1` an owner reaches just `2f`
/// holders), a **voter of this region's configuration council**, or a **voter of the root group** across
/// regions. The two consensus groups ride the same per-peer record session as the record plane (their
/// streams multiplex on it), but their voter sets are *not* subsets of the copyset: a root voter is another
/// region's representative, and a council voter need not be a candidate holder. Keeping sessions only to the
/// copyset left a consensus voter outside it unreachable from this node for good — an election still
/// succeeded through whichever voters happened to be in-copyset, and the first loss that removed them left a
/// leader that could never again reach a majority (the root leader replicating to none of its live voters
/// for 1,500 periods while its sole reachable voter was the one just killed;
/// `docs/bugs/2026-09-13-consensus-voters-outside-record-neighbourhood.md`); probing only the copyset left
/// that voter's liveness known here by gossip alone. Both voter sets are small and bounded (one
/// representative per region; an elected council).
///
/// A peer this node's membership holds **dead** is excluded whatever its role. The voter sets are Raft's
/// (`all_voters`) and do not shrink when the configuration retires a member (no membership change is
/// wired), so judged by role alone a dead voter stayed probed and linked for good — its probe session still
/// counted as formed (`fleet_peers_probed` in `slates status` stayed at two after the owner's death in the
/// three-process deployment test) and its record link re-dialed into the void, a handshake budget at a
/// time. A retired peer that comes back is re-admitted alive by its own probes and
/// regains contact then — which is what reaching the surviving voters makes possible.
pub(crate) fn keeps_direct_contact_with(state: &ShardState, peer: HostId) -> bool {
  direct_contact(state, peer).kept()
}

/// Why this node keeps or drops direct contact with `peer` ([`keeps_direct_contact_with`]'s rule), named: what a
/// formation failure's report shows for each peer, so a pair that never meshes says which side dropped the other and
/// why (2026-10-09: a pair formed no probe session in either direction for 4,000 periods under load, one side counting
/// itself meshed without the other).
pub(crate) fn direct_contact_reason(state: &ShardState, peer: HostId) -> String {
  format!("{:?}", direct_contact(state, peer))
}

/// Why this node keeps, or drops, direct contact with a peer — the one rule behind
/// [`keeps_direct_contact_with`], with its reason, so a probe task's idle and resume transitions can be
/// logged by cause.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirectContact {
  /// The fleet's consensus groups are not both initialized yet: everyone is contacted.
  Forming,
  /// This node is not a member of the council configuration it holds (A-331): a fresh member whose first fetch was
  /// answered before the council admitted it, or a member retired while it was away. It is no peer's neighbour and
  /// holds nothing under that configuration, and the voters it knows are its join donor's, which may be the member it
  /// replaced alone. Judged by the member rule a fresh member kept no peer at all: every record link idled, its learner
  /// fetched over no session, and it never learned the configuration that admits it (live_chaos's fresh-member flow, 2
  /// of 48 runs at one CPU; diag-raft-chaos-ab-68b-2: council at version 8, in contact with none of its four live
  /// members, `fleet.learner.no_session` 113 in 48 s); a retired member kept only the old neighbours of the placement it
  /// last installed. Every member it holds alive is contacted, as while forming, until it holds a configuration that
  /// names it.
  Unadmitted,
  /// Believed dead, but only as a manifest **seed** this node has not yet replaced by a learned id: the seed
  /// was never a live incarnation, so its death (folded where a neighbour learned the peer's real id, and
  /// gossiped here) says only that the peer has a real incarnation this node has not reached. Contact is
  /// kept so this node dials the peer and learns that id itself; idling here stranded the peer for good,
  /// since gossip never enrolls a stranger's id
  /// (`docs/bugs/2026-09-28-a-gossiped-seed-death-stranded-an-unreached-peer.md`).
  UnlearnedSeed,
  /// Not a member of the council's committed configuration (a newcomer or an unknown id): contacted.
  NotInCouncilConfiguration,
  /// In this node's neighbourhood: its current one, or its settled one while a change is in flight, since its
  /// joint writes and its joint lease reach both.
  Neighbour,
  /// This node holds the peer's content: it is in the peer's neighbourhood, whose writes, lease probes and takeover
  /// rounds reach it. Kept from this side too, so the pair's probe plane is keyed whichever of the two is its canonical
  /// dialer: the reverse of [`DirectContact::Neighbour`], as [`DirectContact::AsVoter`] is of [`DirectContact::Voter`]
  /// (2026-10-10). A pair's plane epoch is announced only by its lower anchor, so contact kept from one side alone left
  /// the pair unkeyed whenever the side that dropped it was the lower anchor.
  Holder,
  /// A voter of the council or the root group.
  Voter,
  /// A member this node, leading the council or the root group, is catching up to join its voters (thesis
  /// §4.2.1): its rounds need a session to it wherever it sits.
  Staged,
  /// This node is a voter of the council or the root group, and every member keeps contact with the voters
  /// ([`DirectContact::Voter`]): kept from this side too, so the pair's probe plane is keyed whichever of the two is its
  /// canonical dialer. Found 2026-10-10 by the formation report's keying facts: a voter, the pair's lower anchor, had
  /// dropped a member outside its neighbourhood, never announced the pair's epoch once formation's
  /// everyone-contacts-everyone window closed, and the member's probes to it went unanswered for 4,000 periods.
  AsVoter,
  /// Believed dead, but it announced itself since over an authenticated session at the incarnation it died at: its
  /// process lives under the same id (an anchor's warm restart). Contact is kept so the pair's plane is keyed and its
  /// probes, answered with the death, let it refute (hyper-swim's refutation needs the dead member's probe to be heard).
  Returned,
  /// This node is the pair's canonical dialer (its anchor is the lower one) and serves a record session the peer dialed
  /// to it, which the peer dials and keeps only while it keeps direct contact with this node (A-337): kept, so the pair's
  /// plane is keyed whenever the peer's rule keeps it, whatever this node's own view says. Each role rule above is kept
  /// from both sides only while both sides hold the same view: a learner told stale voters (a seat change moves no
  /// configuration version, so no learner fetches on it, and a learner answered by another learner takes that one's
  /// stale voters) dropped a voter as `OutsideNeighbourhood` while the voter kept it `AsVoter`, and where the learner was
  /// the pair's lower anchor the pair was never keyed and its probe mesh never formed (fleet test
  /// `a_learners_reports_reach_the_council_once_the_voter_it_joined_from_is_gone`, g251's diag run 44262759484, ubuntu,
  /// 2026-11-18).
  Dialed,
  /// Dropped: this node believes the peer dead.
  BelievedDead,
  /// Dropped: a council member outside this node's neighbourhood and no voter.
  OutsideNeighbourhood,
}

impl DirectContact {
  fn kept(self) -> bool {
    !matches!(self, Self::BelievedDead | Self::OutsideNeighbourhood)
  }
}

/// The contact kept with `peer`, which this node holds dead: see [`DirectContact`]'s first three dead variants.
fn dead_contact(state: &ShardState, peer: HostId) -> DirectContact {
  if is_unlearned_seed(state, peer) {
    DirectContact::UnlearnedSeed
  } else if returned_since_death(state, peer) {
    DirectContact::Returned
  } else {
    DirectContact::BelievedDead
  }
}

/// Whether `peer`, held dead here, has announced itself since at the incarnation it died at (`ShardState::returned_members`).
fn returned_since_death(state: &ShardState, peer: HostId) -> bool {
  let incarnation = state
    .fleet
    .membership()
    .state(peer)
    .map(|belief| belief.incarnation);
  incarnation.is_some() && state.returned_members.get(&peer).copied() == incarnation
}

fn direct_contact(state: &ShardState, peer: HostId) -> DirectContact {
  let believed_dead = state
    .fleet
    .membership()
    .state(peer)
    .is_some_and(|belief| belief.liveness == Liveness::Dead);
  if believed_dead {
    dead_contact(state, peer)
  } else if let Some(joining) = joining_contact(state) {
    joining
  } else if !state.council.configuration().members.contains(&peer) {
    DirectContact::NotInCouncilConfiguration
  } else if state.fleet.configuration().neighbours(peer) {
    DirectContact::Neighbour
  } else if state
    .council
    .configuration()
    .holds_for(peer, state.fleet.host())
  {
    DirectContact::Holder
  } else if state.council.known_voter(peer) || state.root.known_voter(peer) {
    DirectContact::Voter
  } else if state.council.is_staged(peer) || state.root.is_staged(peer) {
    DirectContact::Staged
  } else if state.council.is_voter(state.fleet.host()) || state.root.is_voter(state.fleet.host()) {
    DirectContact::AsVoter
  } else if dialed_by_a_higher_anchor(state, peer) {
    DirectContact::Dialed
  } else {
    DirectContact::OutsideNeighbourhood
  }
}

/// Whether this node is the pair's canonical dialer with `peer` (its anchor below the peer's, the side that announces the
/// pair's plane epoch) and serves a record session the peer dialed to it ([`ShardState::dialed_in`]): the peer keeps
/// direct contact with this node (A-337). Only the canonical dialer follows a peer's session, so two nodes never keep each
/// other by their sessions alone: the higher anchor's contact never rests on the lower's dial.
fn dialed_by_a_higher_anchor(state: &ShardState, peer: HostId) -> bool {
  crate::member_task::anchor_of(state, peer).is_some_and(|anchor| {
    anchor > state.origin_anchor
      && state
        .dialed_in
        .get(&anchor)
        .is_some_and(|sessions| *sessions > 0)
  })
}

/// The contact this node keeps with every live peer while it is not yet a member under its consensus groups, whatever
/// the peer's role: [`DirectContact::Forming`] before both groups are initialized, [`DirectContact::Unadmitted`] while
/// the council configuration it holds does not name it (A-331). `None` once it is a member, where the role rule applies.
/// Both are judged of this node alone, so the member rule below them never meets a node with no neighbourhood.
fn joining_contact(state: &ShardState) -> Option<DirectContact> {
  if !state.council.initialized() || !state.root.initialized() {
    Some(DirectContact::Forming)
  } else if !state
    .council
    .configuration()
    .members
    .contains(&state.fleet.host())
  {
    Some(DirectContact::Unadmitted)
  } else {
    None
  }
}

/// Whether `peer` is a manifest seed (`member_id(anchor, 0)` of a rostered peer) whose anchor this node has
/// not learned a real id for. Bounded by the roster: one pass over the manifest's peers and the learned map.
fn is_unlearned_seed(state: &ShardState, peer: HostId) -> bool {
  let rostered = state
    .config
    .fleet
    .as_ref()
    .is_some_and(|fleet| fleet.peers.contains(&peer));
  rostered
    && !state
      .learned_members
      .keys()
      .any(|anchor| crate::deploy::member_id(*anchor, 0) == peer)
}

/// Keeps one peer's client record session up for the coordinator (§4.8) — a candidate holder's, or a
/// consensus voter's ([`keeps_direct_contact_with`]): a per-peer task that brings the session up on **one**
/// socket — one handshake attempt per period, retried until the peer's pinned `accept` completes rather than
/// a fresh-port re-dial being ignored — and installs it in the shard state ([`ShardState::record_sessions`]),
/// where the coordinator borrows it for each dispatch. If the coordinator ever loses it (a borrow that ended
/// without a return), the entry is gone and this task re-establishes on a fresh socket. Per peer — never in
/// the coordinator — because a handshake attempt to a peer that is slow to come up is bounded but long (the
/// retransmit ceiling), and in the coordinator it would stall every other peer's commits and every takeover
/// behind one slow link. Idles when the peer is retired from every set this node reaches it for — its
/// neighbourhood and its consensus groups — dropping its session (a retired peer is never a candidate or a
/// voter again under this configuration), so it is not an unbounded retry of a dead peer (banned item 8).
async fn establish_record_link(driver: PeerDriver, dial: PeerDial) {
  let identity = driver.identity;
  let mut discovery_cursor = crate::discovery::Cursor::default();
  let PeerDial {
    anchor,
    host: seed,
    name,
    address,
    certificate,
    resolver,
  } = dial;
  let mut peer_host = seed;
  // Whether this link is idling because the peer is out of direct contact, so the idle is counted once on entering it.
  let mut idling = false;
  // When the next discovery page is due, and the wait after it (see `next_discovery_wait`).
  let mut discovery_due: u64 = 0;
  let mut discovery_wait: u64 = HEARTBEAT_NS;
  let mut client: Option<Endpoint> = client_for(
    identity,
    &name,
    (&address, crate::deploy::Plane::Record),
    &certificate,
    resolver,
  )
  .await;
  loop {
    // Follow the peer's current id (task #22): a restart is a new process, so the session to its previous
    // incarnation is dead by definition — drop it, and from here keep the link under the id the peer now
    // writes as (dialed again once the council admits it, as any newly admitted member is).
    peer_host = refresh_record_identity(anchor, peer_host, &mut client, &mut discovery_cursor);
    if out_of_contact(peer_host, &mut idling) {
      // The peer is retired. Drop its record session and **idle** — this task does not end, so if the peer
      // rejoins (its probe reaches this node's serve side, which re-admits it — [`serve_peer_probes`]) this
      // loop sees it back in the neighbourhood and re-establishes. A believed-dead peer is never dialed
      // (that establish would block on a peer that will not answer, `docs/bugs/2026-09-10-*`), so idling
      // costs nothing until the peer is alive again.
      state::with_state(|s| {
        s.record_sessions.remove(&peer_host);
        sessions_changed(s, peer_host);
      });
      dial_settled(peer_host);
      crate::daemon::pace(HEARTBEAT_NS).await;
      continue;
    }
    // Absent means no session (never established, or lost); a `None` entry means the coordinator has it out
    // on a dispatch — not this task's to touch.
    let absent =
      state::with_state(|s| !s.record_sessions.contains_key(&peer_host)).unwrap_or(false);
    if absent {
      dial_call_begins(peer_host);
      let (kept, session) = establish_session(
        client,
        None,
        identity,
        &name,
        (&address, crate::deploy::Plane::Record),
        &certificate,
        resolver,
      )
      .await;
      dial_call_ended(peer_host, kept.as_ref(), session.is_some());
      client = kept;
      enroll_record_session(session, peer_host, anchor, &mut discovery_cursor, driver).await;
      dial_enrolled(peer_host);
      discovery_wait = HEARTBEAT_NS;
      discovery_due = 0;
    } else {
      dial_settled(peer_host);
    }
    let discovery_held =
      state::with_state(|s| s.injected_discovery_hold == Some(peer_host)).unwrap_or(false);
    if !absent && !discovery_held && futures::now_ns() >= discovery_due {
      let before = discovery_cursor.settled();
      refresh_discovery(peer_host, anchor, &mut discovery_cursor, driver).await;
      discovery_wait = next_discovery_wait(before, discovery_cursor.settled(), discovery_wait);
      discovery_due = futures::now_ns().saturating_add(discovery_wait);
    }
    crate::daemon::pace(HEARTBEAT_NS).await;
  }
}

/// Marks `peer`'s record dial as inside a handshake call from now ([`crate::state::RecordDial`]), the dial itself begun
/// now unless it already was. One entry a peer, bounded by the fleet's peer capacity as `pairs_delivered` is.
fn dial_call_begins(peer: HostId) {
  let now = futures::now_ns();
  state::with_state(|s| {
    let capacity = s.config.fleet_peer_capacity.max(1);
    if s.record_dials.len() < capacity || s.record_dials.contains_key(&peer) {
      let dial = s.record_dials.entry(peer).or_default();
      if dial.dialing_since_ns.is_none() {
        dial.dialing_since_ns = Some(now);
      }
      dial.call_since_ns = Some(now);
      dial.calls = dial.calls.saturating_add(1);
    }
  });
}

/// Records how `peer`'s handshake call ended: `established` (its first discovery page next), or not, with the dialer
/// `kept` for another call on its socket (its budgets spent and why the last ended) or dropped for a fresh dial.
fn dial_call_ended(peer: HostId, kept: Option<&Endpoint>, established: bool) {
  let now = futures::now_ns();
  state::with_state(|s| {
    if let Some(dial) = s.record_dials.get_mut(&peer) {
      dial.call_since_ns = None;
      dial.enroll_since_ns = established.then_some(now);
      dial.budgets_spent = kept.map_or(0, Endpoint::handshake_budgets_spent);
      dial.stall = kept.and_then(Endpoint::handshake_stall);
    }
  });
}

/// `peer`'s fresh session's first discovery page has ended, the session installed or not.
fn dial_enrolled(peer: HostId) {
  state::with_state(|s| {
    if let Some(dial) = s.record_dials.get_mut(&peer) {
      dial.enroll_since_ns = None;
    }
  });
}

/// `peer`'s record link holds its session, or idles with its peer out of contact: no dial runs.
fn dial_settled(peer: HostId) {
  state::with_state(|s| {
    if let Some(dial) = s.record_dials.get_mut(&peer) {
      dial.dialing_since_ns = None;
      dial.call_since_ns = None;
      dial.enroll_since_ns = None;
    }
  });
}

/// Whether `peer` is out of this node's direct contact, so its record link idles; entering the idle is counted
/// ([`LINK_OUT_OF_CONTACT`]), so a pair with no record session tells by design from a link that failed.
fn out_of_contact(peer: HostId, idling: &mut bool) -> bool {
  let out = state::with_state(|s| !keeps_direct_contact_with(s, peer)) == Some(true);
  if out && !*idling {
    count_refusal(LINK_OUT_OF_CONTACT);
  }
  *idling = out;
  out
}

/// Counter: a record link that went idle because its peer is out of this node's direct contact (`keeps_direct_contact_with`:
/// neither holds for the other, and neither votes nor is staged), counted on entering the idle. Two such members form
/// no record session and no probe session by design (raft-chaos's five-node flow took such a pair for a stall: the two
/// non-voters held 3 of 4 peers probed for a whole fleet wait, CI diag-live-fixes-30).
const LINK_OUT_OF_CONTACT: &str = "fleet.link.out_of_contact";

/// Counter: a discovery page sent over a record link (a settled roster's pages back off to one per horizon a peer).
/// Format: a counter name in the daemon's status report.
const DISCOVERY_PAGES: &str = "fleet.discovery.pages";

/// The wait before a record link's next discovery page. While the sweep stays complete at the same roster generation
/// the wait doubles, from one heartbeat up to the membership horizon ([`crate::lease::horizon_ns`]); anything else (a
/// sweep under way, a new generation, a new session) brings it back to one heartbeat. So a settled fleet checks each
/// peer about once a horizon rather than every heartbeat (it was one exchange a peer a heartbeat, ~2% of a core per idle
/// node on the two-network topology, 2026-10-08), and a roster change is still learned no later than a death would be.
fn next_discovery_wait(before: Option<u64>, after: Option<u64>, wait: u64) -> u64 {
  match (before, after) {
    (Some(was), Some(now)) if was == now => wait
      .saturating_mul(2)
      .clamp(HEARTBEAT_NS, crate::lease::horizon_ns()),
    _ => HEARTBEAT_NS,
  }
}

/// A learner's last council configuration fetch ([`drive_learner_fetch`]): when it asked (the runtime clock), how many
/// members it asked, and the council version it held once the replies were adopted. Read by status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LearnerFetchLog {
  /// When the last fetch asked, or `None` before any.
  pub(crate) asked_at_ns: Option<u64>,
  /// The members it asked.
  pub(crate) asked: u32,
  /// The council version held after its replies were adopted.
  pub(crate) fetched_version: u64,
}

/// An uninitialized learner's fetch pace: when its next fetch is due and the wait after that one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LearnerPace {
  /// The runtime clock's time the next fetch is due.
  due_ns: u64,
  /// The wait after the next fetch.
  wait_ns: u64,
}

impl Default for LearnerPace {
  fn default() -> Self {
    LearnerPace {
      due_ns: 0,
      wait_ns: HEARTBEAT_NS,
    }
  }
}

impl LearnerPace {
  /// Whether a fetch is due at `now_ns`.
  fn due(&self, now_ns: u64) -> bool {
    now_ns >= self.due_ns
  }

  /// Paces the next fetch after one made at `now_ns`.
  fn fetched(&mut self, now_ns: u64, still_uninitialized: bool) {
    self.wait_ns = next_learner_wait(still_uninitialized, self.wait_ns);
    self.due_ns = now_ns.saturating_add(self.wait_ns);
  }

  /// Back to fetching at once (the group is initialized, or another reason to fetch arrived).
  fn reset(&mut self) {
    *self = LearnerPace::default();
  }
}

/// The wait before an uninitialized learner's next fetch (no group has been bootstrapped yet, so its configuration is
/// empty and every live peer looks like a divergence): it doubles while the group stays uninitialized, from one
/// heartbeat up to the membership horizon ([`crate::lease::horizon_ns`]), and is one heartbeat once it is initialized.
/// A fleet left unbootstrapped then polls each voter about once a horizon rather than every heartbeat (measured
/// 2026-10-08: about 43 root and 17 council fetches a second per node before `bootstrap root`), and a bootstrap is
/// still learned within a horizon; a stale-configuration refusal or recovery fetches at once, as before.
fn next_learner_wait(still_uninitialized: bool, wait_ns: u64) -> u64 {
  if still_uninitialized {
    wait_ns
      .saturating_mul(2)
      .clamp(HEARTBEAT_NS, crate::lease::horizon_ns())
  } else {
    HEARTBEAT_NS
  }
}

/// Counter: learner fetches skipped while an uninitialized group's backoff ran (the backoff's non-vacuity counter).
const LEARNER_FETCH_PACED: &str = "fleet.learner.fetch_paced";

/// Counter: configuration fetch requests a learner sent, root and council (one per voter asked).
const LEARNER_FETCHES: &str = "fleet.learner.fetches";

/// The status count under which a learner with evidence it is behind records a period with no session to any member of
/// its region to ask: what a fetch that never happened leaves, where before it left nothing at all.
/// Format: a refusal name in the daemon's status report.
const LEARNER_FETCH_NO_SESSION: &str = "fleet.learner.no_session";

/// The member id the link should address now (task #22). A change — the peer restarted — removes the
/// previous member's record entry (the session to its previous incarnation is dead by definition), drops a
/// dial still in its handshake (it points at the old incarnation's address and keys, so driving it on is
/// not progress toward the new one; counted, as at a retirement) and restarts the discovery sweep.
fn refresh_record_identity(
  anchor: HostId,
  previous: HostId,
  client: &mut Option<Endpoint>,
  cursor: &mut crate::discovery::Cursor,
) -> HostId {
  state::with_state(|state| refresh_record_identity_in(state, anchor, previous, client, cursor))
    .unwrap_or(previous)
}

/// [`refresh_record_identity`] over the shard's state. The manifest's **seed** id (`member_id(anchor, 0)`,
/// the routing placeholder) was never a live incarnation: the first fresh id learned for the peer names
/// the same process the link is already dialing — at its manifest or discovered address, under its pinned
/// certificate — so a dial in its handshake is kept and completes, rather than being dropped and redialed
/// a period later (CI run 36408099369: `fleet.dial.stale_dropped: 3` counted while a three-process fleet
/// formed; `docs/bugs/2026-09-28-formation-dropped-a-dial-to-the-peer-it-was-reaching.md`).
fn refresh_record_identity_in(
  state: &mut ShardState,
  anchor: HostId,
  previous: HostId,
  client: &mut Option<Endpoint>,
  cursor: &mut crate::discovery::Cursor,
) -> HostId {
  let current = state
    .learned_members
    .get(&anchor)
    .map_or(previous, |learned| learned.host);
  if current != previous {
    state.record_sessions.remove(&previous);
    sessions_changed(state, previous);
    // The dial's record is the previous id's: the link's dial under the new id starts afresh.
    state.record_dials.remove(&previous);
    let previous_was_a_live_incarnation = previous != crate::deploy::member_id(anchor, 0);
    if previous_was_a_live_incarnation && client.take().is_some() {
      count_refusal_in(state, DIAL_STALE_DROPPED);
    }
    *cursor = crate::discovery::Cursor::default();
  }
  current
}

/// Enrolls a freshly established session: the first discovery page over it, then the session installed as
/// the peer's record link. Any other outcome drops the fresh session here and the next period dials afresh.
async fn enroll_record_session(
  session: Option<Endpoint>,
  peer_host: HostId,
  anchor: HostId,
  cursor: &mut crate::discovery::Cursor,
  driver: PeerDriver,
) {
  let Some(mut session) = session else {
    return;
  };
  *cursor = crate::discovery::Cursor::default();
  if exchange_discovery(&mut session, anchor, peer_host, cursor, driver).await
    == DiscoveryOutcome::Answered
  {
    state::with_state(|state| {
      state
        .record_sessions
        .insert(peer_host, crate::state::RecordLink::up(session));
      sessions_changed(state, peer_host);
    });
    crate::live_ship::holder_reached(peer_host);
  }
}

/// One discovery page over the peer's installed record link, the session borrowed for it and put back
/// **only into the entry it left** — an entry removed meanwhile (the peer replaced or retired) is never
/// re-created by a late page; any outcome but an answer releases the session and the link re-dials.
async fn refresh_discovery(
  peer_host: HostId,
  anchor: HostId,
  cursor: &mut crate::discovery::Cursor,
  driver: PeerDriver,
) {
  let session = state::with_state(|state| {
    state
      .record_sessions
      .get_mut(&peer_host)
      .and_then(|link| link.endpoint.take())
  })
  .flatten();
  if let Some(mut session) = session {
    let outcome = exchange_discovery(&mut session, anchor, peer_host, cursor, driver).await;
    state::with_state(|state| {
      if outcome == DiscoveryOutcome::Answered {
        if let Some(link) = state.record_sessions.get_mut(&peer_host) {
          link.endpoint = Some(session);
        }
        sessions_changed(state, peer_host);
      } else {
        state.record_sessions.remove(&peer_host);
        sessions_changed(state, peer_host);
        // A discovery page can be the first exchange to meet the restarted peer's reset (A-261): counted as a reset,
        // as a borrower's return counts it, not left to read as an unexplained drop.
        if session.peer_reset() {
          state.count(LINK_PEER_RESET, 1);
        }
        let dropped = crate::state::WallAndPeriods::now(state);
        state.dead_session_dropped_at.insert(peer_host, dropped);
      }
    });
  }
}

/// How a discovery exchange with a peer ended (§4.8): typed, so a deadline, an invalidation, a transport
/// fault and a protocol refusal are never one outcome — and a session is never silently kept or dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiscoveryOutcome {
  /// The peer answered a page; the session is kept.
  Answered,
  /// The deadline passed unanswered: the exchange abandoned, the session released so the link re-dials.
  Deadline,
  /// The link was invalidated while the exchange waited — the peer's member id changed, or it was retired.
  Invalidated,
  /// The transport failed terminally.
  Transport,
  /// The reply, or the request itself, was refused by the discovery protocol.
  Refused,
}

/// Why a discovery exchange ended without a reply (the race in [`exchange_discovery`]), with its counter.
#[derive(Clone, Copy)]
enum DiscoveryFault {
  Deadline,
  Invalidated,
  Transport,
  /// The exchange's deadline could not be armed (its sleep refused, off a shard): never read as the
  /// deadline passing (AUD-29-39).
  Unbounded,
}

impl DiscoveryFault {
  fn counter(self) -> &'static str {
    match self {
      DiscoveryFault::Deadline => DISCOVERY_DEADLINE,
      DiscoveryFault::Invalidated => DISCOVERY_INVALIDATED,
      DiscoveryFault::Transport => DISCOVERY_TRANSPORT,
      DiscoveryFault::Unbounded => DISCOVERY_UNBOUNDED,
    }
  }

  fn outcome(self) -> DiscoveryOutcome {
    match self {
      DiscoveryFault::Deadline => DiscoveryOutcome::Deadline,
      DiscoveryFault::Invalidated => DiscoveryOutcome::Invalidated,
      DiscoveryFault::Transport => DiscoveryOutcome::Transport,
      // A deadline that cannot be armed is a terminal runtime failure of the exchange: its session is
      // released like a transport's; its own counter tells the two apart.
      DiscoveryFault::Unbounded => DiscoveryOutcome::Transport,
    }
  }
}

/// Whether the link to `anchor` still addresses `expected`: the anchor's learned member is still `expected`
/// (or none was learned yet, the seed standing) and this node still keeps direct contact with it.
fn link_valid(anchor: HostId, expected: HostId) -> bool {
  state::with_state(|s| link_invalid_reason(s, anchor, expected).is_none()).unwrap_or(false)
}

/// Why `anchor`'s link, made to `expected`, no longer holds, as a status counter name; `None` while it holds:
/// the anchor's learned member is another incarnation, or this node no longer keeps direct contact with the
/// member, by the reason [`direct_contact`] gives. Counted beside `fleet.discovery.invalidated`, so link churn
/// says which rule dropped the link.
fn link_invalid_reason(
  state: &ShardState,
  anchor: HostId,
  expected: HostId,
) -> Option<&'static str> {
  if state
    .learned_members
    .get(&anchor)
    .is_some_and(|learned| learned.host != expected)
  {
    return Some(LINK_INVALID_MEMBER_CHANGED);
  }
  match direct_contact(state, expected) {
    DirectContact::BelievedDead => Some(LINK_INVALID_BELIEVED_DEAD),
    DirectContact::OutsideNeighbourhood => Some(LINK_INVALID_OUTSIDE_NEIGHBOURHOOD),
    _ => None,
  }
}

/// A record link dropped because its anchor's learned member became another incarnation.
const LINK_INVALID_MEMBER_CHANGED: &str = "fleet.link.invalid.member_changed";
/// A record link dropped because this node came to believe its member dead.
const LINK_INVALID_BELIEVED_DEAD: &str = "fleet.link.invalid.believed_dead";
/// A record link dropped because its member left this node's neighbourhood and holds no voter's seat.
const LINK_INVALID_OUTSIDE_NEIGHBOURHOOD: &str = "fleet.link.invalid.outside_neighbourhood";

/// Wakes the discovery exchange pending on `anchor`'s record link, if one is, so it re-checks the link's
/// validity at once — a replacement learned on contact, a retirement folded — rather than at its deadline.
fn wake_link_waiter(state: &mut ShardState, anchor: HostId) {
  if let Some(waker) = state.link_waiters.remove(&anchor) {
    waker.wake();
  }
}

/// [`wake_link_waiter`] for the anchor whose current member is `host` (a death is folded by member id).
pub(crate) fn wake_link_waiter_of(state: &mut ShardState, host: HostId) {
  let anchor = state
    .learned_members
    .iter()
    .find(|(_, learned)| learned.host == host)
    .map(|(anchor, _)| *anchor);
  if let Some(anchor) = anchor {
    wake_link_waiter(state, anchor);
  }
}

/// One discovery page over `session`, **bounded**: raced against one deadline armed once at the measured
/// control-plane round budget's full span ([`consensus_budget`] over the slowest measured path — the bound
/// every other record-plane exchange runs under), and against the link's validity, re-checked whenever the
/// exchange is woken (a reply, the timer, or [`wake_link_waiter`]). A peer that never answers — the process
/// under the session gone, its replacement unable to read the old keys, which no datagram socket reports as
/// a terminal error — therefore cannot hold the link task, and its endpoint, past the budget: before this
/// bound a survivor refreshing discovery when the old process disappeared awaited its reply for good, and
/// never returned to the loop that notices the replacement and re-dials it — the leader made 271
/// replication attempts toward a replacement voter with no session and sent no append
/// (`docs/bugs/2026-09-16-discovery-await-strands-a-replacement-raft-voter.md`). Validity is checked before
/// a ready reply is accepted, so a reply and a replacement notification that become ready together never
/// apply a page for a member the link no longer addresses. Partial packets, acknowledgements and
/// retransmissions do not renew the deadline. Every outcome is typed and counted.
async fn exchange_discovery(
  session: &mut Endpoint,
  anchor: HostId,
  expected: HostId,
  cursor: &mut crate::discovery::Cursor,
  driver: PeerDriver,
) -> DiscoveryOutcome {
  let Some(request) = state::with_state(|state| cursor.request(state)).flatten() else {
    return DiscoveryOutcome::Refused;
  };
  let span_ns = consensus_budget(slowest_path_tail_ns()).max_deadline_ns();
  // Every page counted: the non-vacuity counter for the record link's discovery backoff (`next_discovery_wait`).
  count_refusal(DISCOVERY_PAGES);
  let before = session.last_exchange();
  let exchanged = {
    let mut exchange =
      std::pin::pin!(session.request(crate::discovery::STREAM, Priority::Metadata, &request));
    let mut timer = std::pin::pin!(futures::sleep(span_ns));
    std::future::poll_fn(|cx| {
      if !link_valid(anchor, expected) {
        return std::task::Poll::Ready(Err(DiscoveryFault::Invalidated));
      }
      if let std::task::Poll::Ready(result) = std::future::Future::poll(exchange.as_mut(), cx) {
        return std::task::Poll::Ready(result.map_err(|_| DiscoveryFault::Transport));
      }
      match std::future::Future::poll(timer.as_mut(), cx) {
        std::task::Poll::Ready(Ok(())) => {
          return std::task::Poll::Ready(Err(DiscoveryFault::Deadline));
        }
        std::task::Poll::Ready(Err(_)) => {
          return std::task::Poll::Ready(Err(DiscoveryFault::Unbounded));
        }
        std::task::Poll::Pending => {}
      }
      // At most one waiter per anchor: the link task drives one exchange at a time.
      state::with_state(|s| {
        s.link_waiters.insert(anchor, cx.waker().clone());
      });
      std::task::Poll::Pending
    })
    .await
  };
  state::with_state(|s| {
    s.link_waiters.remove(&anchor);
  });
  let reply = match exchanged {
    Ok(reply) => reply,
    Err(fault) => {
      // The request future was dropped mid-exchange: abandoned, so its frames do not ride a later flush (only its own:
      // a forward the lane left open on this session is not this page's).
      session.abandon_begun_since(before);
      count_refusal(fault.counter());
      if matches!(fault, DiscoveryFault::Invalidated)
        && let Some(reason) =
          state::with_state(|s| link_invalid_reason(s, anchor, expected)).flatten()
      {
        count_refusal(reason);
      }
      return fault.outcome();
    }
  };
  match state::with_state(|state| cursor.receive(state, &reply, anchor)) {
    Some(Ok(peers)) => {
      for peer in peers {
        driver.start(peer);
      }
      DiscoveryOutcome::Answered
    }
    Some(Err(refusal)) => {
      count_refusal(refusal.counter());
      DiscoveryOutcome::Refused
    }
    None => DiscoveryOutcome::Refused,
  }
}

/// A dispatch the coordinator made whose holders may still be in flight: the [`Stragglers`] to recover
/// sessions from, the holders borrowed for it that have not yet come back (each returns through the
/// dispatch's `reusable` at its return, or through the stragglers later), and what a **late reply** means
/// to this dispatch ([`LateReplies`]). Once the stragglers are spent, a holder still outstanding never
/// returned its session: it is dropped from the shard state as lost, so its link task re-establishes it —
/// a borrow that ended without a return is a loss by definition. Every fan-out the coordinator makes is one
/// of these, settled at the top of each period: a record commit, a takeover promotion, a council or root
/// replication or election round, a learner fetch. Bounded: the dispatches alive are at most the rounds a
/// period makes times the periods a straggler may take, its request deadline
/// ([`CommitBudget::max_deadline_ns`]).
pub(crate) struct Dispatch {
  stragglers: Stragglers,
  outstanding: Vec<HostId>,
  late: LateReplies,
}

/// What a dispatch does with a reply that arrives **after** its progress-aware stop ([`Dispatch::settle`]).
/// The stop bounds how long a round *waits* (§4.8 "late work";
/// `docs/bugs/2026-09-12-broadcast-waits-out-dead-voter.md`); it must not discard the *work* that then
/// arrives, or a slow-but-live voter costs its acknowledgement every round — under sustained CPU starvation,
/// forever, which is how an elected leader with its sessions intact still never committed
/// (`docs/bugs/2026-09-13-consensus-voters-outside-record-neighbourhood.md`).
#[derive(Clone, Copy)]
pub(crate) enum LateReplies {
  /// A record commit or takeover promotion: the late reply is dropped — the round already resolved without
  /// it, and the record re-ships to that holder next period, idempotently, over its recovered session.
  Discard,
  /// A configuration-council Raft round: fold a late vote or append reply into the council.
  Council,
  /// A root-group Raft round: fold a late vote or append reply into the root group.
  Root,
  /// A config learner's fetch: adopt a late, newer regional configuration.
  ConfigFetch,
  /// A root learner's fetch: adopt a late, newer root configuration.
  RootFetch,
  /// A seal's content round: a holder's acknowledgement that arrived after the round's collection stopped
  /// at the hedge delay is **folded into the seal** on its owner shard — bound to the object, sequence and
  /// manifest it was put for, and timed into the put-latency window — never discarded: the stop bounds how
  /// long the round waits before hedging, not whether a slow holder's verified hold counts.
  Content {
    /// Which placement the round served: the seal's map on its owner shard.
    track: Track,
    /// The owner shard the seal lives on.
    shard: u16,
    /// The object whose seal the round put.
    object: ObjectId,
    /// The snapshot sealed (a superseded seal ignores the fold).
    snapshot: DbSnapshotId,
    /// The head sequence the content is placed for.
    sequence: u64,
    /// The manifest identity put.
    manifest: [u8; 32],
    /// When the round was dispatched, for the straggler's latency reading.
    dispatched_ns: u64,
  },
  /// A mirror shipment (`crate::mirror::ship`): a holder's acknowledgement that came back after the round's budget is
  /// folded into the job's acknowledged holders on its owner shard, never discarded. A holder keeps the first value at
  /// a position, so a resend after a discarded acknowledgement could be refused for good (2026-10-09).
  MirrorShip {
    /// The owner shard the mirror job lives on.
    shard: u16,
    /// The object shipped.
    object: ObjectId,
    /// The head sequence shipped.
    sequence: u64,
  },
}

impl Dispatch {
  /// Records a dispatch over the `taken` holders, of which `reusable` came back at its return, and how a
  /// reply that comes back later is treated.
  pub(crate) fn new(
    taken: Vec<HostId>,
    reusable: &[(HostId, Endpoint)],
    stragglers: Stragglers,
    late: LateReplies,
  ) -> Self {
    let outstanding = taken
      .into_iter()
      .filter(|host| !reusable.iter().any(|(returned, _)| returned == host))
      .collect();
    Self {
      stragglers,
      outstanding,
      late,
    }
  }

  /// Recovers whatever stragglers have finished into the shard state — each late reply folded as
  /// [`LateReplies`] directs, each session returned; `true` once the dispatch is spent (every straggler
  /// accounted for, any holder still outstanding marked lost) and can be dropped.
  fn settle(&mut self) -> bool {
    let late = self.late;
    let (recovered, done) = match late {
      LateReplies::Discard => self.stragglers.recover(),
      _ => {
        let (arrived, done) = self.stragglers.recover_replies();
        let mut recovered = Vec::with_capacity(arrived.len());
        let mut replies = Vec::with_capacity(arrived.len());
        for (host, reply, endpoint) in arrived {
          replies.push((host, reply));
          recovered.push((host, endpoint));
        }
        match late {
          LateReplies::Content { .. } => fold_late_content(late, &replies),
          LateReplies::MirrorShip {
            shard,
            object,
            sequence,
          } => crate::mirror::fold_late_acks(shard, object, sequence, &replies),
          _ => fold_late_replies(late, &replies),
        }
        (recovered, done)
      }
    };
    self
      .outstanding
      .retain(|host| !recovered.iter().any(|(returned, _)| returned == host));
    return_sessions(recovered);
    if done {
      let lost = std::mem::take(&mut self.outstanding);
      state::with_state(|s| {
        for host in &lost {
          s.record_sessions.remove(host);
          sessions_changed(s, *host);
        }
      });
    }
    done
  }
}

/// Folds a consensus or learner-fetch round's late replies as `late` directs (see [`LateReplies`]).
/// Folds a content round's late acknowledgements into the seal they were put for, on the seal's **owner
/// shard** (the seal lives there; this runs on the control shard, so it hops with [`run_on`]): each reply
/// that decodes as a bound acknowledgement from a candidate is merged into the placement and its latency —
/// from the round's dispatch to now — recorded into the owner shard's put-latency window. A superseded
/// seal ignores the fold. Only a `LateReplies::Content` reaches here.
fn fold_late_content(late: LateReplies, replies: &[(HostId, TimedReply)]) {
  let LateReplies::Content {
    track,
    shard,
    object,
    snapshot,
    sequence,
    manifest,
    dispatched_ns,
  } = late
  else {
    return;
  };
  let acked: Vec<HostId> = replies
    .iter()
    .filter_map(|(_, reply)| match ContentMessage::decode(&reply.bytes) {
      Ok(ContentMessage::Ack(ack)) if ack.binds(object, sequence, &manifest) => Some(ack.holder),
      _ => None,
    })
    .collect();
  if acked.is_empty() {
    return;
  }
  let latency_ns = futures::now_ns().saturating_sub(dispatched_ns);
  let origin = state::with_state(|s| s.shard).unwrap_or_default();
  crate::xshard::run_on_counted(origin, shard, move |s| {
    for holder in &acked {
      match track {
        Track::Home => s.put_latency.record(latency_ns),
        Track::Mirror => s.mirror_put_latency.record(latency_ns),
      }
      // Folded only into the job the put was made for: the same snapshot at the same put sequence. A restore's
      // straggler, bound to the head's sequence, is no acknowledgement of a replace of that snapshot that started
      // meanwhile at the next one, which the holder's door judges apart.
      let repaired = match seal_jobs(s, track).get_mut(&object) {
        Some(job)
          if job.snapshot == snapshot
            && job.put_sequence == sequence
            && job.content.candidates.contains(holder) =>
        {
          fold_content_ack(job, *holder)
        }
        _ => false,
      };
      if repaired {
        s.repairs = s.repairs.saturating_add(1);
        s.count(HEAL_REPAIRED_LATE, 1);
      }
    }
  });
}

/// Folds a consensus or learner-fetch round's late replies as `late` directs (see [`LateReplies`]). A late
/// consensus reply also samples the path to its voter — the tail of the round trip is exactly what the
/// election timing and the round budget must see.
fn fold_late_replies(late: LateReplies, replies: &[(HostId, TimedReply)]) {
  state::with_state(|s| {
    if matches!(late, LateReplies::Council | LateReplies::Root) {
      sample_voter_paths(s, replies);
    }
    for (peer, reply) in replies {
      let bytes = &reply.bytes;
      match late {
        LateReplies::Discard => {}
        LateReplies::Council => {
          if let Some(message) = late_raft_reply(s, false, bytes, *peer) {
            s.council.fold_reply(message);
          }
        }
        LateReplies::Root => {
          if let Some(message) = late_raft_reply(s, true, bytes, *peer) {
            s.root.fold_reply(message);
          }
        }
        LateReplies::ConfigFetch => {
          crate::consensus::adopt_fetch(s, false, *peer, bytes);
        }
        LateReplies::RootFetch => {
          crate::consensus::adopt_fetch(s, true, *peer, bytes);
        }
        // Folded on the owner shard by [`fold_late_content`] and `mirror::fold_late_acks`, outside this borrow.
        LateReplies::Content { .. } | LateReplies::MirrorShip { .. } => {}
      }
    }
  });
}

/// A late Raft reply worth folding: a vote or an append reply, each of which folds with no follow-on
/// message and is exactly the acknowledgement a slow voter would otherwise cost. A late **pre-vote** reply
/// is dropped: completing a pre-election here would owe follow-on vote requests a settle cannot ship, and
/// the next campaign simply re-runs its pre-vote. A dropped grant is counted
/// ([`ELECTION_LATE_PRE_VOTE_GRANT`]).
fn late_raft_reply(
  state: &mut ShardState,
  root: bool,
  bytes: &[u8],
  peer: HostId,
) -> Option<RaftMessage> {
  match crate::consensus::decode_message(state, root, peer, bytes) {
    Ok(RaftMessage::PreVoteReply(reply)) => {
      if reply.granted {
        count_refusal_in(state, ELECTION_LATE_PRE_VOTE_GRANT);
      }
      None
    }
    Err(_) => None,
    Ok(message) => Some(message),
  }
}

/// The owner authority this node's heads are written under right now: the configuration's current version
/// as the generation (it advances on every join or retirement) and this host as the owner.
fn owner_authority(local: HostId) -> Option<Authority> {
  state::with_state(|s| Authority {
    generation: s.fleet.configuration().version,
    owner: local,
  })
}

// ── The configuration council plane (§4.8, D-14) ─────────────────────────────────────────────────────
//
// The regional configuration council's Raft ([`ShardState::council`]) is driven over the **same** record
// sessions the coordinator already holds and served on the **same** socket the peer records arrive on — one
// more stream id ([`CONFIG_STREAM`]) on each. Driving it from the record-plane coordinator ([`drive_config_
// council`], called once per period) is deliberate: the council's borrow of the voter sessions is then
// sequential with the record ships in the same task, so it never contends with them; a separate task would
// have to either share the sessions (contend) or open a third socket per node, and the one coordinator needs
// neither. The serve side is one arm in [`serve_peer_records`]; the drive side is a concurrent, session-
// preserving fan-out ([`broadcast`]) of the leader's replication or a follower's election.

/// Format: the stream id the configuration council's Raft messages ride on a record session — distinct from
/// the record commit (1), the phase-one prepare (2) and the content exchanges (4/5/6) the same session
/// multiplexes, so `serve_peer_records` dispatches a council message by its stream. (The standalone
/// `raft_wire::RAFT_STREAM` is a separate single-plane context; here the council shares the record session.)
const CONFIG_STREAM: u64 = 7;

/// Format: the stream id a **learner** fetches the committed regional configuration on (§4.8, D-14). A
/// non-voter member does not vote in the council; it asks a voter for its configuration over this stream and
/// adopts a newer one — the design's config-learning path, distinct from the Raft stream (7) above.
const CONFIG_FETCH_STREAM: u64 = 8;

/// Format: the stream id the **root group's** Raft messages ride on a record session (§4.8, D-14 — the root
/// group across regions), distinct from the regional council's stream (7): the same session multiplexes both
/// consensus planes, and `serve_peer_records` dispatches a root message to the root group by this stream.
const ROOT_STREAM: u64 = 9;

/// Format: the stream id a **root learner** fetches the committed root configuration on (§4.8, D-14) — the
/// cross-region parallel of [`CONFIG_FETCH_STREAM`]. A region member that is not its region's representative
/// does not vote in the root group; it asks a root voter for the root configuration over this stream and
/// adopts a newer one.
const ROOT_FETCH_STREAM: u64 = 10;

/// Format: the stream id a **forwarded verb** rides on a record session (§4.8 "Lookup") — a request a node
/// cannot serve locally is forwarded to the node that can (today the operator's `PromoteRegion` to the root
/// leader; general volume-verb forwarding to a remotely-homed volume's owner is owed). The request is an
/// encoded `RequestBody`, the reply an encoded `ReplyBody` ([`slates_ipc::protocol::encode_body`]).
pub(crate) const FORWARD_STREAM: u64 = 11;

/// Format: the stream id a member's **report** to the council rides on a record session (§4.8 "Neighbourhood
/// changes", "Promotion and takeover"): a settlement or a takeover confirmation the member makes about itself,
/// as an encoded [`slates_cluster::config_group::ConfigCommand`], answered with a [`ReportOutcome`]. Distinct from every other stream the
/// session multiplexes (1–14), so `serve_peer_records` dispatches it by its stream.
const REPORT_STREAM: u64 = 15;
/// The stream a node's recipient public key is asked on (A-92 piece 4b): answered from the control shard, which holds
/// the recipient; the mutual TLS session binds the answer to the peer that gave it.
/// Format: one stream id per RPC kind on a connection; a fixed label, not a tunable.
const RECIPIENT_STREAM: u64 = 18;
/// The stream an owner shard delivers a pair key on (A-92 piece 4b): its partition, the key's id and the key wrapped
/// to this node's recipient; answered `[PAIR_ACCEPTED]` once recorded.
/// Format: one stream id per RPC kind on a connection; a fixed label, not a tunable.
const PAIR_KEY_STREAM: u64 = 19;
/// Format: the reply byte of an accepted pair key.
const PAIR_ACCEPTED: u8 = 1;
/// The stream a volume's successor asks a fellow candidate for the volume's lineage key on (A-295): the object, the key's
/// id and the asker's recipient public key ([`lineage_request`]), answered with the key wrapped to that recipient, or
/// empty when refused (counted where it was refused). After the work routes' kind (25, `work_route::STREAM`) and the
/// live batches' (26, `LIVE_ENTRIES_STREAM`): a kind another stream takes is served as that stream, so its request
/// reaches no lineage handler at all (the first run of the window test, every ask unanswered while the work-route arm
/// took kind 25). Held distinct by `every_request_kind_the_record_session_dispatches_is_its_own`.
/// Format: one stream id per RPC kind on a connection; a fixed label, not a tunable.
const LINEAGE_STREAM: u64 = 27;

/// Format: the refusal name counting council requests left unanswered because their sender is held dead
/// ([`serve_council`]).
const COUNCIL_PEER_HELD_DEAD: &str = "fleet.council.peer_held_dead";

/// Answers one configuration-council Raft message a peer shipped on [`CONFIG_STREAM`] (§4.8, D-14): the
/// council serves a pre-vote, a vote request, or an append — applying whatever an append newly commits to
/// the regional configuration and refreshing this node's leader contact — and returns the reply to ship
/// back. A reply-typed or malformed message is answered with nothing (the sender counts no reply). Runs
/// synchronously inside `serve_once`, no await held across it.
fn serve_council(state: &mut ShardState, peer: HostId, request: &[u8]) -> Vec<u8> {
  if !same_region(state, peer) {
    return Vec::new();
  }
  // A peer this node's failure detector has held dead for the whole confirmation window (the window the leader's
  // retirement waits out) is answered nothing: no append or snapshot renews its leadership here, and no vote elects
  // it. The detector is the authority on liveness (an Omega detector driving leader election, Chandra and Toueg
  // 1996); Raft's safety does not depend on whom a node answers, only its liveness does. Without this, a leader cut
  // off on the probe plane but still heard on the record plane kept both followers' lease at its term; it never
  // proposes its own retirement, so the council never retired it and no takeover began (2026-10-06,
  // `a_council_leader_the_failure_detector_holds_dead_is_replaced_and_retired`). Unanswered, it loses CheckQuorum and
  // steps down, and the followers' election timers run and elect one of them.
  if stable_dead_council_members(state).contains(&peer) {
    state.count(COUNCIL_PEER_HELD_DEAD, 1);
    return Vec::new();
  }
  match crate::consensus::decode_message(state, false, peer, request) {
    Ok(message) => state
      .council
      .answer(message)
      .and_then(|reply| crate::consensus::encode_message(state, false, &reply))
      .unwrap_or_default(),
    Err(_) => Vec::new(),
  }
}

/// Answers a root-group Raft request received over the transport ([`ROOT_STREAM`]) — the cross-region
/// counterpart of [`serve_council`]: runs it through this node's [`RootGroup`](slates_cluster::root_group::RootGroup),
/// returning the reply to ship back and applying whatever newly committed to the root configuration. A
/// reply-typed or malformed message is answered with nothing.
fn serve_root(state: &mut ShardState, peer: HostId, request: &[u8]) -> Vec<u8> {
  match crate::consensus::decode_message(state, true, peer, request) {
    Ok(message) => state
      .root
      .answer(message)
      .and_then(|reply| crate::consensus::encode_message(state, true, &reply))
      .unwrap_or_default(),
    Err(_) => Vec::new(),
  }
}

/// Answers a root learner's fetch (§4.8, D-14), parallel to [`serve_config_fetch`]. A fresh
/// member receives the common base and retained prefix; later fetches name the group and version.
fn serve_root_fetch(state: &ShardState, request: &[u8]) -> Vec<u8> {
  crate::consensus::serve_fetch(state, true, request).unwrap_or_default()
}

/// Answers a member's report to the council on [`REPORT_STREAM`] (§4.8 "Neighbourhood changes", "Promotion and
/// takeover"): the council takes a settlement or a takeover confirmation the reporting member makes about
/// itself — the session's authenticated peer is the only member a report may name — and a caught-up leader
/// proposes it ([`slates_cluster::config_group::RegionalCouncil::report`]). The reply is the
/// [`ReportOutcome`]; a member keeps reporting each period until its installed configuration reflects it, so a
/// follower's answer, or a proposal a lost leadership dropped, costs one more period. A refused report is
/// counted [`REPORT_REFUSED`]: an honest member never sends one.
fn serve_council_report(state: &mut ShardState, peer_host: HostId, request: &[u8]) -> Vec<u8> {
  let outcome = match slates_cluster::config_group::ConfigCommand::decode(request) {
    Some(command) => {
      let bound = moved_bound(state);
      state.council.report(peer_host, command, bound)
    }
    None => ReportOutcome::Refused,
  };
  if outcome == ReportOutcome::Refused {
    state.count(REPORT_REFUSED, 1);
  }
  outcome.encode()
}

/// Derived: the most moved-object exceptions this node, leading the council, lets the configuration hold (banned item 8;
/// §4.8 "Authority scope"). An exception names a live volume, and a member owns at most as many volumes as its registry
/// has slots (`max_slots` on each of its shards, derived from its machine profile at boot), so a region holds at most
/// that many moved volumes a member: the bound is that, counting every member at this node's size. A move past it is
/// answered `Full` and never proposed.
pub(crate) fn moved_bound(state: &ShardState) -> usize {
  state
    .council
    .configuration()
    .members
    .len()
    .saturating_mul(state.shards.len())
    .saturating_mul(state.volumes.max_slots())
}

/// The status refusal count under which a council voter records a member's report it refused: bytes that are
/// no command, a membership change sent as a report, or a report naming a host other than the session's peer.
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
const REPORT_REFUSED: &str = "fleet.report.refused";

/// The status count under which a member records a report it sent to the council (§4.8 "Neighbourhood
/// changes"): it rises once a period while the member's report is not yet in its installed configuration.
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
const REPORT_SENT: &str = "fleet.report.sent";

/// The status count under which a member records a period whose report found no session to send it on: a report that
/// was not sent, which [`REPORT_SENT`] does not count.
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
const REPORT_NO_SESSION: &str = "fleet.report.no_session";

/// This node's neighbourhood change the council has not settled yet (§4.8 "Neighbourhood changes"): the
/// council's configuration version and the version its current neighbourhood was fixed at, when it differs from
/// its settled one and this node has installed that configuration; `None` when settled, or while this node's
/// placement is behind the council's (its shards would judge against an older neighbourhood).
fn unsettled_neighbourhood(state: &ShardState, local: HostId) -> Option<(u64, u64)> {
  let regional = state.council.configuration();
  let current = regional.neighbourhoods.get(&local)?;
  let settled = regional.settled.get(&local)?;
  (settled.generation != current.generation
    && state.fleet.configuration().version == regional.version)
    .then_some((regional.version, current.generation))
}

/// Whether every record this shard owns is held by `f + 1` of its cohort in the current neighbourhood (§4.8
/// "the newest committed record is held by f+1 of the new candidates";
/// [`slates_db::register::Configuration::placed_on_current`]): each volume's newest head, by the
/// acknowledgements the record plane recorded for it, and each green's newest merge record
/// ([`crate::merge_service::greens_placed_on_current_cohort`]). A head placed only on the settled cohort is not
/// yet counted; the record plane re-ships it to the new candidates. The settled cohort is not asked: a joint
/// write already reached it, and an old cohort that lost a host could never answer. True with nothing owned.
fn records_placed_on_current_neighbourhood(state: &ShardState) -> bool {
  let config = state.fleet.configuration();
  // Every register of each volume the node owns — its head and its catalog (AUD-29-17).
  let heads = state.volumes.iter().all(|(_, slot)| {
    let object = ObjectId(slot.id.bytes);
    [object, object.catalog()].into_iter().all(|register| {
      state
        .placed_heads
        .get(&register)
        .is_some_and(|head| config.placed_on_current(register, &head.placement.acked))
    })
  });
  heads && crate::merge_service::greens_placed_on_current_cohort(state)
}

/// Whether every volume this shard owns is served from within this node's current neighbourhood (A-546): its head
/// snapshot's placement names only this node and hosts of that neighbourhood, so the settle that names it certifies that
/// no head this node serves names a holder outside its neighbourhoods, and such a holder lets its content go (A-508's
/// holders). A placement made before the neighbourhood moved is replaced by the healer while the change is in flight
/// ([`heal_for`]). A volume whose head names no content yet is served.
fn content_placed_on_current_neighbourhood(state: &ShardState) -> bool {
  let config = state.fleet.configuration();
  state.volumes.iter().all(|(_, slot)| {
    let id = DbVolumeId {
      bytes: slot.id.bytes,
    };
    let Some(record) = state.db.partition().volume(id) else {
      return true;
    };
    if record.epoch == 0 {
      return true;
    }
    state
      .db
      .partition()
      .snapshot(id, record.head)
      .is_some_and(|snapshot| match &snapshot.placed {
        PlacementState::Placed { region, .. } => {
          crate::takeover::served_from_placement(region, config.owner, &config.neighbourhood)
        }
        PlacementState::Local => true,
      })
  })
}

/// Reports this node's neighbourhood change placed, once it is (§4.8 "Neighbourhood changes"; Vertical Paxos
/// II's state transfer before the old configuration retires): while the council's current neighbourhood for
/// this node differs from its settled one, every shard is asked whether all it owns is held by `f + 1` of its
/// cohort in the current neighbourhood ([`records_placed_on_current_neighbourhood`]) under the very configuration
/// version the report names; when all are, `Settle` for that neighbourhood's version goes to the council
/// ([`send_council_report`]). Its content must be placed within the current neighbourhood too
/// ([`content_placed_on_current_neighbourhood`], A-546), so the settle certifies it. Repeated each period until the
/// installed configuration shows it settled; a report for a neighbourhood that moved on meanwhile changes nothing (the
/// fold checks the version).
async fn report_settlement(
  origin: u16,
  shards: &[u16],
  local: HostId,
  budget: CommitBudget,
  in_flight: &mut Vec<Dispatch>,
) {
  let Some((version, generation)) =
    state::with_state(|s| unsettled_neighbourhood(s, local)).flatten()
  else {
    return;
  };
  for shard in shards.iter().copied() {
    let placed = call_within(
      origin,
      shard,
      move |s| {
        s.fleet.configuration().version == version
          && records_placed_on_current_neighbourhood(s)
          && content_placed_on_current_neighbourhood(s)
      },
      HEARTBEAT_NS,
    )
    .await;
    if placed != Some(true) {
      return;
    }
  }
  send_council_report(
    slates_cluster::config_group::ConfigCommand::Settle {
      owner: local,
      generation,
    },
    local,
    budget,
    in_flight,
  )
  .await;
}

/// Sends a report this node makes about itself to the council (§4.8): proposed at once when this node leads the
/// council, else sent to every member of its region it holds a session to over [`REPORT_STREAM`], where the leader
/// proposes it and any other member answers `NotLeader`. Counted [`REPORT_SENT`], or [`REPORT_NO_SESSION`] with no one
/// to send it to. Not to the voters alone: on a learner they are its join state's, which nothing updates, so a learner
/// that joined from the bootstrapper reported to nobody once that voter left, and its settlements and takeover
/// confirmations never reached the council (CI diag-raft-chaos-learner-56 run 1: `fleet.report.sent` 662 with the node's
/// neighbourhood unsettled for a whole fleet wait). The sessions return with the replies; a late reply is dropped (the
/// caller reports again next period if the configuration does not show it).
pub(crate) async fn send_council_report(
  command: slates_cluster::config_group::ConfigCommand,
  local: HostId,
  budget: CommitBudget,
  in_flight: &mut Vec<Dispatch>,
) {
  let request = command.encode();
  let Some(targets) = state::with_state(|s| {
    if s.council.is_leader() {
      s.count(REPORT_SENT, 1);
      let bound = moved_bound(s);
      let _ = s.council.report(local, command, bound);
      return None;
    }
    Some(
      s.record_sessions
        .keys()
        .copied()
        .filter(|host| *host != local && same_region(s, *host))
        .collect::<Vec<HostId>>(),
    )
  })
  .flatten() else {
    return;
  };
  let sessions = take_sessions("fleet.lent_to.send_council_report", |host| {
    targets.contains(&host)
  });
  let _ = state::with_state_counted(|s| {
    s.count(
      if sessions.is_empty() {
        REPORT_NO_SESSION
      } else {
        REPORT_SENT
      },
      1,
    );
  });
  if sessions.is_empty() {
    return;
  }
  let requests: Vec<(HostId, Vec<u8>, Endpoint)> = sessions
    .into_iter()
    .map(|(host, endpoint)| (host, request.clone(), endpoint))
    .collect();
  let sent: Vec<HostId> = requests.iter().map(|(host, _, _)| *host).collect();
  let (replied, stragglers) = broadcast(requests, REPORT_STREAM, Priority::Metadata, budget).await;
  let recovered: Vec<(HostId, Endpoint)> = replied
    .into_iter()
    .map(|(host, _, endpoint)| (host, endpoint))
    .collect();
  in_flight.push(Dispatch::new(
    sent,
    &recovered,
    stragglers,
    LateReplies::Discard,
  ));
  return_sessions(recovered);
}

/// Answers a learner's configuration fetch on [`CONFIG_FETCH_STREAM`] (§4.8, D-14). Initial
/// admission transfers the common base and retained Raft prefix. An initialized learner names its
/// group and applied version and receives only a newer configuration. Malformed or foreign-group
/// requests receive no state. Runs synchronously inside `serve_once`.
fn serve_config_fetch(state: &ShardState, request: &[u8]) -> Vec<u8> {
  crate::consensus::serve_fetch(state, false, request).unwrap_or_default()
}

/// Drives one replication round as the council **leader**: ships each other voter the append it is owed (a
/// heartbeat, or the entries it still lacks) over its borrowed record session, concurrently, and folds each
/// reply — the leader advances its commit index as a majority acknowledge, and the regional configuration
/// applies whatever newly commits. Borrows only the voter sessions; a voter with no live session is not
/// reached this round and is retried next period.
async fn drive_council_replication(
  others: &[HostId],
  budget: CommitBudget,
  in_flight: &mut Vec<Dispatch>,
) {
  let sessions = take_sessions("fleet.lent_to.drive_council_replication", |host| {
    others.contains(&host)
  });
  if sessions.is_empty() {
    return;
  }
  // What each borrowed voter is owed — a budgeted append, a heartbeat, or the snapshot when its entries were
  // compacted away — built under a brief borrow (the endpoints stay out here).
  let appends: std::collections::BTreeMap<HostId, Vec<u8>> = state::with_state(|s| {
    let batch = crate::consensus::append_batch_bytes(s.council.voters().len());
    sessions
      .iter()
      .filter_map(|(host, _)| {
        s.council.replication_for(*host, batch).and_then(|message| {
          crate::consensus::encode_message(s, false, &message).map(|bytes| (*host, bytes))
        })
      })
      .collect()
  })
  .unwrap_or_default();
  // Pair each session with its append; a voter owed nothing keeps its session without a dispatch.
  let mut requests = Vec::new();
  let mut kept = Vec::new();
  for (host, endpoint) in sessions {
    match appends.get(&host) {
      Some(bytes) => requests.push((host, bytes.clone(), endpoint)),
      None => kept.push((host, endpoint)),
    }
  }
  let sent: Vec<HostId> = requests.iter().map(|(host, _, _)| *host).collect();
  let (replied, stragglers) = broadcast(requests, CONFIG_STREAM, Priority::Control, budget).await;
  let mut recovered = kept;
  let mut replies = Vec::with_capacity(replied.len());
  for (host, reply, endpoint) in replied {
    replies.push((host, reply));
    recovered.push((host, endpoint));
  }
  state::with_state(|s| {
    sample_voter_paths(s, &replies);
    for (peer, reply) in replies {
      if let Ok(message) = crate::consensus::decode_message(s, false, peer, &reply.bytes) {
        s.council.fold_reply(message);
      }
    }
  });
  // A voter that replies late is settled by the coordinator: its acknowledgement is folded then and its
  // session returned, so a slow follower never costs the leader its ack or its session.
  in_flight.push(Dispatch::new(
    sent,
    &recovered,
    stragglers,
    LateReplies::Council,
  ));
  return_sessions(recovered);
}

/// The consensus group a drive step addresses (§4.8 mechanism 2): the regional council or the root group.
/// Both run the one Raft dialect, each over its own stream and late-reply fold, so the steps below are
/// written once over both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Group {
  Council,
  Root,
}

impl Group {
  fn is_root(self) -> bool {
    matches!(self, Group::Root)
  }

  fn stream(self) -> u64 {
    match self {
      Group::Council => CONFIG_STREAM,
      Group::Root => ROOT_STREAM,
    }
  }

  fn late(self) -> LateReplies {
    match self {
      Group::Council => LateReplies::Council,
      Group::Root => LateReplies::Root,
    }
  }

  fn fold_reply(self, state: &mut ShardState, message: RaftMessage) -> Vec<RaftMessage> {
    match self {
      Group::Council => state.council.fold_reply(message),
      Group::Root => state.root.fold_reply(message),
    }
  }

  fn forget_leader(self, state: &mut ShardState) {
    match self {
      Group::Council => state.council.forget_leader(),
      Group::Root => state.root.forget_leader(),
    }
  }

  fn invited_campaign(self, state: &mut ShardState) -> Vec<RaftMessage> {
    match self {
      Group::Council => state.council.invited_campaign(),
      Group::Root => state.root.invited_campaign(),
    }
  }

  fn transferring_to(self, state: &ShardState) -> Option<HostId> {
    match self {
      Group::Council => state.council.transferring_to(),
      Group::Root => state.root.transferring_to(),
    }
  }

  fn take_timeout_now(self, state: &mut ShardState) -> Option<(HostId, RaftMessage)> {
    match self {
      Group::Council => state.council.take_timeout_now(),
      Group::Root => state.root.take_timeout_now(),
    }
  }
}

/// The real vote round of an election over the borrowed voter `sessions` (Raft §5.2): the vote request goes
/// to each, the replies are folded (this node leads once its majority grants), a late vote is folded when
/// the coordinator settles the round, and every session is returned. Shared by a pre-election that won its
/// pre-votes and by a leader-invited campaign (thesis §3.10), which skips the pre-votes.
async fn drive_vote_round(
  group: Group,
  vote: RaftMessage,
  sessions: Vec<(HostId, Endpoint)>,
  budget: CommitBudget,
  in_flight: &mut Vec<Dispatch>,
) {
  let vote_bytes =
    state::with_state(|s| crate::consensus::encode_message(s, group.is_root(), &vote))
      .flatten()
      .unwrap_or_default();
  let requests: Vec<(HostId, Vec<u8>, Endpoint)> = sessions
    .into_iter()
    .map(|(host, endpoint)| (host, vote_bytes.clone(), endpoint))
    .collect();
  let sent: Vec<HostId> = requests.iter().map(|(host, _, _)| *host).collect();
  let (replied, stragglers) = broadcast(requests, group.stream(), Priority::Control, budget).await;
  let mut recovered = Vec::with_capacity(replied.len());
  let mut vote_replies = Vec::with_capacity(replied.len());
  for (host, reply, endpoint) in replied {
    vote_replies.push((host, reply));
    recovered.push((host, endpoint));
  }
  state::with_state(|s| {
    sample_voter_paths(s, &vote_replies);
    for (peer, reply) in vote_replies {
      if let Ok(message) = crate::consensus::decode_message(s, group.is_root(), peer, &reply.bytes)
      {
        group.fold_reply(s, message);
      }
    }
  });
  // A late vote still counts when it arrives: the coordinator folds it as it settles the round.
  in_flight.push(Dispatch::new(sent, &recovered, stragglers, group.late()));
  return_sessions(recovered);
}

/// Borrows the sessions of the other voters a campaign's round asks ([`take_sessions`]). A voter whose session
/// is out of its link for a moment — held by its link task's discovery page for one round trip, or lent to a
/// dispatch — is waited for, paced at `budget`'s poll interval, until the round's base deadline: a page
/// returns its session within the page's round trip, which that deadline covers (it is at least the slowest
/// voter's round-trip tail, `round_budget`). Taking only what was in the links at the first look sent the
/// round to no one whenever the only live voter's session was out, and the campaign then cost a whole
/// election timeout: on KIND, 4 of 10 leader losses
/// (`docs/bugs/2026-09-29-a-campaign-asked-no-one-while-a-session-was-out.md`). A voter with no link — retired,
/// or never dialled — is not waited for. Each voter awaited and taken is counted
/// ([`ELECTION_SESSION_AWAITED`]); each still unasked at the end, by why ([`ELECTION_NO_LINK`],
/// [`ELECTION_SESSION_LENT`], [`ELECTION_SESSION_HELD`]).
async fn take_campaign_sessions(
  others: &[HostId],
  budget: CommitBudget,
) -> Vec<(HostId, Endpoint)> {
  let began = futures::now_ns();
  let (withheld, withheld_until) = campaign_hold(began);
  let available = |host: HostId| !(withheld.contains(&host) && futures::now_ns() < withheld_until);
  let mut taken = take_sessions("fleet.lent_to.take_campaign_sessions", |host| {
    others.contains(&host) && available(host)
  });
  let first_look = taken.len();
  loop {
    let out = voters_out(others, &taken);
    if out.is_empty() || futures::now_ns().saturating_sub(began) >= budget.deadline_ns {
      count_campaign_sessions(others, &taken, taken.len().saturating_sub(first_look));
      return taken;
    }
    // Waits for one of the voters' sessions to come back (or its link to change) within what is left, then takes
    // again; it slept the fleet's poll interval between looks before 2026-11-20. A test's campaign hold ends at its
    // instant, not at an event, so while one holds the wait ends there too. No wait possible (off a shard) ends the wait
    // with what was taken, never a spin.
    let now = futures::now_ns();
    let mut left = budget.deadline_ns.saturating_sub(now.saturating_sub(began));
    if out.iter().any(|voter| withheld.contains(voter)) && now < withheld_until {
      left = left.min(withheld_until.saturating_sub(now));
    }
    if session_returned(&out, left).await.is_none() {
      count_campaign_sessions(others, &taken, taken.len().saturating_sub(first_look));
      return taken;
    }
    taken.extend(take_sessions(
      "fleet.lent_to.take_campaign_sessions",
      |host| out.contains(&host) && available(host),
    ));
  }
}

/// Test support: the voters whose sessions a campaign that began at `began_ns` counts as out of their links,
/// and until when ([`ShardState::campaign_session_hold`]); none in production.
fn campaign_hold(began_ns: u64) -> (std::collections::BTreeSet<HostId>, u64) {
  state::with_state(|s| s.campaign_session_hold.clone())
    .flatten()
    .map_or_else(
      || (std::collections::BTreeSet::new(), began_ns),
      |(voters, span_ns)| (voters, began_ns.saturating_add(span_ns)),
    )
}

/// The other voters a campaign has not yet taken a session of whose record link exists — the session out
/// on a discovery page or a dispatch — the ones it waits for.
fn voters_out(others: &[HostId], taken: &[(HostId, Endpoint)]) -> Vec<HostId> {
  state::with_state(|s| {
    others
      .iter()
      .copied()
      .filter(|voter| !taken.iter().any(|(host, _)| host == voter))
      .filter(|voter| s.record_sessions.contains_key(voter))
      .collect()
  })
  .unwrap_or_default()
}

/// Counts a campaign's session take: the voters awaited ([`ELECTION_SESSION_AWAITED`]), and each voter left
/// unasked by why.
fn count_campaign_sessions(others: &[HostId], taken: &[(HostId, Endpoint)], awaited: usize) {
  let _ = state::with_state_counted(|s| {
    if awaited > 0 {
      let count = s.refusals.entry(ELECTION_SESSION_AWAITED).or_insert(0);
      *count = count.saturating_add(u64::try_from(awaited).unwrap_or(u64::MAX));
    }
    for voter in others
      .iter()
      .filter(|voter| !taken.iter().any(|(host, _)| host == *voter))
    {
      let why = match s.record_sessions.get(voter) {
        None => ELECTION_NO_LINK,
        Some(link) if link.borrowed.is_some() => ELECTION_SESSION_LENT,
        Some(_) => ELECTION_SESSION_HELD,
      };
      count_refusal_in(s, why);
    }
  });
}

/// A campaign the leader invited (thesis §3.10, leadership transfer): when the serve path has recorded an
/// invitation, starts the election it asks for — no pre-vote, the leader invited it — and runs its vote
/// round over the other voters' sessions. The borrow that starts it publishes the new term and vote before
/// any request leaves ([`crate::retention::retain`] closes every borrow). Returns whether a campaign ran, so
/// the caller skips this period's timer. An invitation this node ignores (another term, or it leads or does
/// not vote) runs nothing.
async fn drive_invited_campaign(
  group: Group,
  others: &[HostId],
  budget: CommitBudget,
  in_flight: &mut Vec<Dispatch>,
) -> bool {
  let votes = state::with_state(|s| group.invited_campaign(s)).unwrap_or_default();
  let Some(vote) = votes.into_iter().next() else {
    return false;
  };
  let sessions = take_campaign_sessions(others, budget).await;
  if !sessions.is_empty() {
    drive_vote_round(group, vote, sessions, budget, in_flight).await;
  }
  true
}

/// Runs this period's invited campaign, if the leader sent an invitation ([`drive_invited_campaign`]), and
/// re-baselines the election timer after it, so the campaign is not immediately retriggered: a won election
/// makes this node leader next period, a lost one waits out the timer again. Returns whether a campaign ran
/// (the caller then skips this period's timer). A leader ignores an invitation (none runs).
async fn campaign_if_invited(
  group: Group,
  others: &[HostId],
  budget: CommitBudget,
  timer: &mut ElectionTimer,
  in_flight: &mut Vec<Dispatch>,
) -> bool {
  if !drive_invited_campaign(group, others, budget, in_flight).await {
    return false;
  }
  let contact = state::with_state(|s| match group {
    Group::Council => s.council.leader_contact(),
    Group::Root => s.root.leader_contact(),
  });
  if let Some(contact) = contact {
    timer.rebaseline(contact);
  }
  true
}

/// The leader's half of a leadership transfer (thesis §3.10): once the target has caught up, ships it the
/// invitation over its borrowed session, once. The target answers nothing (its vote requests are the answer);
/// the session is returned. A target with no live session is not reached this period, and the invitation is
/// taken only when the session is in hand, so it is not lost.
async fn drive_transfer_invitation(
  group: Group,
  budget: CommitBudget,
  in_flight: &mut Vec<Dispatch>,
) {
  let Some(target) = state::with_state(|s| group.transferring_to(s)).flatten() else {
    return;
  };
  let sessions = take_sessions("fleet.lent_to.drive_transfer_invitation", |host| {
    host == target
  });
  if sessions.is_empty() {
    return;
  }
  let bytes = state::with_state(|s| {
    group
      .take_timeout_now(s)
      .and_then(|(_, message)| crate::consensus::encode_message(s, group.is_root(), &message))
  })
  .flatten();
  let Some(bytes) = bytes else {
    return_sessions(sessions);
    return;
  };
  let requests: Vec<(HostId, Vec<u8>, Endpoint)> = sessions
    .into_iter()
    .map(|(host, endpoint)| (host, bytes.clone(), endpoint))
    .collect();
  let sent: Vec<HostId> = requests.iter().map(|(host, _, _)| *host).collect();
  let (replied, stragglers) = broadcast(requests, group.stream(), Priority::Control, budget).await;
  let recovered: Vec<(HostId, Endpoint)> = replied
    .into_iter()
    .map(|(host, _, endpoint)| (host, endpoint))
    .collect();
  in_flight.push(Dispatch::new(sent, &recovered, stragglers, group.late()));
  return_sessions(recovered);
}

/// Drives an election as a **follower** whose leader contact has lapsed (Raft §9.6, the full pre-vote then
/// real vote over the transport): begins the pre-election and broadcasts the pre-vote to every other voter;
/// on a granted majority the real vote requests go out the same way, and folding their replies makes this
/// node leader once its own majority grants. Every borrowed voter session is returned whatever the outcome.
/// A node that already leads, or the sole voter (which `election_timeout` self-elects with no messages),
/// sends nothing.
async fn drive_council_election(
  others: &[HostId],
  budget: CommitBudget,
  in_flight: &mut Vec<Dispatch>,
) {
  // Begin the pre-election; `election_timeout` returns one (identical) pre-vote per other voter, so the
  // first is the message to broadcast. Empty means this node already leads or self-elected — nothing to do.
  let Some(Some(pre_vote)) = state::with_state(|s| s.council.election_timeout().into_iter().next())
  else {
    return;
  };
  let sessions = take_campaign_sessions(others, budget).await;
  if sessions.is_empty() {
    return;
  }
  // Phase one — the pre-vote round over the borrowed voter sessions.
  let pre_bytes = state::with_state(|s| crate::consensus::encode_message(s, false, &pre_vote))
    .flatten()
    .unwrap_or_default();
  let requests: Vec<(HostId, Vec<u8>, Endpoint)> = sessions
    .into_iter()
    .map(|(host, endpoint)| (host, pre_bytes.clone(), endpoint))
    .collect();
  let sent: Vec<HostId> = requests.iter().map(|(host, _, _)| *host).collect();
  let (replied, stragglers) = broadcast(requests, CONFIG_STREAM, Priority::Control, budget).await;
  let mut sessions = Vec::with_capacity(replied.len());
  let mut pre_replies = Vec::with_capacity(replied.len());
  for (host, reply, endpoint) in replied {
    pre_replies.push((host, reply));
    sessions.push((host, endpoint));
  }
  // A voter whose pre-vote reply is late is settled by the coordinator: its session comes back then (the
  // late pre-vote reply itself is not folded — the next campaign re-runs its pre-vote).
  in_flight.push(Dispatch::new(
    sent,
    &sessions,
    stragglers,
    LateReplies::Council,
  ));
  // Fold the pre-vote replies; a granted majority yields the real vote request to broadcast next (the
  // follow-on of a pre-vote reply is always a vote request — the term is advanced only now).
  let vote = state::with_state(|s| {
    sample_voter_paths(s, &pre_replies);
    let mut vote = None;
    for (peer, reply) in &pre_replies {
      if let Ok(message) = crate::consensus::decode_message(s, false, *peer, &reply.bytes)
        && let Some(request) = s.council.fold_reply(message).into_iter().next()
      {
        vote = Some(request);
      }
    }
    vote
  })
  .flatten();
  let Some(vote) = vote else {
    return_sessions(sessions);
    return;
  };
  // Phase two — the real vote round over the same sessions.
  drive_vote_round(Group::Council, vote, sessions, budget, in_flight).await;
}

/// Whether this node's own SWIM membership view differs from the members of the configuration it has
/// installed — the signal a learner uses to fetch the council's committed configuration reactively (§4.8 the
/// piggyback rule). It compares this node's **alive** set against the configuration's **members**, which is
/// exactly the predicate the council leader reconciles from ([`slates_cluster::config_group::RegionalCouncil::reconcile_alive`] admits an
/// alive non-member and takes over a member SWIM confirmed dead), so a divergence means the council has — or, if
/// this node's view is ahead of the leader's, soon will have — a newer configuration this node must install:
/// to admit a newcomer, retire a departed member, or take over a failed owner's objects (the quiescent
/// successor's only cue, as it receives no records for the dead owner's objects). Equal in steady state, so a
/// converged learner fetches nothing. Compared as sets; both include this node.
/// Whether a member of this node's region has announced, on its probes, a configuration newer than the one this node's
/// council holds (`AnswersGiven::announced_versions`): evidence that the council committed a change this learner has not
/// fetched. A change that leaves the membership alone (a voter change, an owner's settlement) shows no divergence in
/// the learner's own view, so before this a learner held version 12 while its region stood at 17, and reported its
/// settlement 359 times for a generation the council had moved past (CI diag-live-fixes-21). Another region's members
/// announce their own council's versions, another counter, so only this region's count.
fn region_announces_newer_configuration(state: &ShardState) -> bool {
  let own = state.council.configuration().version;
  state
    .answers_given
    .announced_versions
    .iter()
    .any(|(peer, announced)| *announced > own && same_region(state, *peer))
}

fn membership_diverges_from_config(state: &ShardState) -> bool {
  let alive: std::collections::BTreeSet<HostId> =
    state.fleet.membership().alive().into_iter().collect();
  let members: std::collections::BTreeSet<HostId> = state
    .council
    .configuration()
    .members
    .iter()
    .copied()
    .collect();
  alive != members
}

/// Reconciles the council's membership and voter set from this node's own SWIM view, as the region's
/// configuration master (§4.8, D-14): proposes any admit or retire, which the replication that follows commits
/// over the transport and applies on every voter. Only the leader proposes (`reconcile_alive`); it probes
/// every member, so a follower's own detection need not. Each alive member is proposed with the failure domain
/// its node declares, so an admission — a restarted node's new id among them (task #22) — carries the domain
/// into the configuration.
fn reconcile_council_membership(s: &mut ShardState) {
  let alive: Vec<(HostId, Option<DomainId>)> = authenticated_alive(s)
    .into_iter()
    .filter(|host| same_region(s, *host))
    .map(|host| (host, declared_domain(s, host)))
    .collect();
  // Retire only members this node's own SWIM view has confirmed **dead** AND kept dead for the
  // death-confirmation window — never one merely suspected, and never one declared dead only transiently.
  // A live voter briefly unreachable during a leader loss's re-election (a fresh joiner's just-formed
  // sessions churning most of all) can reach `Dead` for a period or two before its refutation arrives;
  // retiring it there is an irreversible consensus action on a revocable belief, and it drops a live
  // member — leaving a later loss's survivors short of a majority
  // (docs/bugs/2026-09-17-council-retires-a-suspected-voter.md). The window lets the refutation land first.
  // And only once f + 1 voters hold it dead, this leader's watch one of them (§4.8 "Promotion and takeover").
  let dead = confirmed_dead_council_members(s);
  s.council.reconcile_alive(&alive, &dead);
  // The voter set follows the committed membership (Raft §6, one joint change at a time): a voter taken
  // over leaves the consensus set — it stops counting toward every majority — and a member this node
  // holds alive is promoted to its seat, so the council keeps tolerating `f` failures; a sitting voter
  // keeps its seat while it is a member, so an admission never displaces a live one
  // (docs/bugs/2026-09-22-council-seats-follow-id-order-not-liveness.md).
  let voting: Vec<HostId> = alive.iter().map(|(host, _)| *host).collect();
  s.council.reconcile_voters(&voting);
  // Once no change is in flight, the voters in force go into the configuration, so a learner learns them by its version
  // (A-380): a seat change alone moves none.
  s.council.record_voters();
}

/// One period as the council **leader**: reconcile the membership ([`reconcile_council_membership`]),
/// replicate to every other voter, send a leadership transfer's invitation once its target has caught up
/// (thesis §3.10), and — every derived base of leader periods — judge the quorum (CheckQuorum, Raft §6.2:
/// a leader that heard from no majority since the previous check steps down rather than sitting on a term
/// it can no longer hold; the window is fed by the append replies folded above, timely or late).
async fn lead_council_period(
  others: &[HostId],
  budget: CommitBudget,
  timer: &mut ElectionTimer,
  timing: &ElectionTiming,
  in_flight: &mut Vec<Dispatch>,
) {
  state::with_state(reconcile_council_membership);
  drive_council_replication(others, budget, in_flight).await;
  drive_transfer_invitation(Group::Council, budget, in_flight).await;
  if timer.leader_period(timing) {
    state::with_state(|s| {
      s.council.check_quorum();
      // A voter that commits distinguishably faster takes over, once this leader has led a whole window
      // (§3.4); its invitation leaves with next period's replication.
      let alive = authenticated_alive(s);
      s.council.priority_transfer(&alive);
    });
  }
}

/// Reconciles the root group's region membership and voter set from this node's own alive view, as the root
/// master (§4.8, D-14): proposes admitting any newly-alive region and retiring any **mirror-less** region no
/// host is alive in (a lost region with a mirror is left for a deliberate operator promotion — split-brain
/// safety). The root voter set follows the committed regions and their live representatives (Raft §6, one
/// joint change at a time): a dead representative leaves the root consensus set, and a region whose
/// representative died is carried by its next live host.
fn reconcile_root_membership(s: &mut ShardState) {
  let alive = alive_regions(s);
  s.root.reconcile_regions(&alive, &s.region_mirrors);
  let representatives = alive_representatives(s);
  s.root.reconcile_voters(&representatives);
  // Once no change is in flight, the voters in force go into the root configuration, so a root learner learns them by
  // its version (A-406): a seat change alone moves none.
  s.root.record_voters();
}

/// One period as the **root** leader — the cross-region parallel of [`lead_council_period`]: reconcile the
/// regions ([`reconcile_root_membership`]), replicate, send a transfer's invitation, and CheckQuorum on the
/// election-timeout cadence.
async fn lead_root_period(
  others: &[HostId],
  budget: CommitBudget,
  timer: &mut ElectionTimer,
  timing: &ElectionTiming,
  in_flight: &mut Vec<Dispatch>,
) {
  state::with_state(reconcile_root_membership);
  drive_root_replication(others, budget, in_flight).await;
  drive_transfer_invitation(Group::Root, budget, in_flight).await;
  if timer.leader_period(timing) {
    state::with_state(|s| {
      s.root.check_quorum();
      let alive = authenticated_alive(s);
      s.root.priority_transfer(&alive);
    });
  }
}

/// A learner's period of its group's election timer (thesis §4.2.3). A learner never campaigns, but the lease
/// of a leader it no longer hears lapses at the minimum election timeout, as a voter's does: a member whose
/// promotion to voter the leader committed without it is a voter in a candidate's configuration, and a lease
/// it could never lose refused that candidate for good
/// (`docs/bugs/2026-09-29-a-member-that-missed-its-promotion-refused-every-election.md`). Where a voter would
/// have campaigned, the learner re-baselines the timer instead.
fn lapse_learner_lease(
  group: Group,
  timer: &mut ElectionTimer,
  contact: u64,
  timing: &ElectionTiming,
  local: HostId,
  rank: usize,
) {
  match timer.follower_period(contact, timing, local, rank) {
    FollowerStep::Follow => {}
    FollowerStep::LeaderLapsed => {
      let _ = state::with_state_counted(|s| group.forget_leader(s));
    }
    FollowerStep::Campaign => {
      let _ = state::with_state_counted(|s| group.forget_leader(s));
      timer.rebaseline(contact);
    }
  }
}

/// Drives this node's configuration council one period from the record-plane coordinator (§4.8, D-14). As
/// **leader** it replicates a heartbeat to every voter (holding the term and carrying the commit index); as
/// a **follower** it counts the periods since the leader last made contact and, once past the jittered
/// election timeout, campaigns; the **sole voter** self-elects with no messages. `idle`, `seen_contact` and
/// `attempt` persist across periods (the coordinator owns them): the follower's election timer, the last
/// leader-contact value it saw, and its jitter rotation.
async fn drive_config_council(
  local: HostId,
  budget: CommitBudget,
  timer: &mut ElectionTimer,
  in_flight: &mut Vec<Dispatch>,
) {
  let Some((is_voter, is_leader, contact, voters, targets, rank)) = state::with_state(|s| {
    if s.recovery.council.is_some() {
      return (
        false,
        false,
        s.council.leader_contact(),
        Vec::new(),
        Vec::new(),
        0,
      );
    }
    let voters = s.council.voters();
    // This period's election priority (§3.4): the quorum round trip over the measured paths to the other
    // voters; and this node's rank among the voters it holds alive, for the timer's gate.
    let priority = quorum_priority(
      voters
        .iter()
        .filter(|voter| **voter != local)
        .map(|voter| s.peer_paths.get(voter)),
      voters.len(),
    );
    s.council.set_priority(priority);
    let rank = s.council.election_rank(&authenticated_alive(s));
    (
      s.council.is_voter(local),
      s.council.is_leader(),
      s.council.leader_contact(),
      voters,
      s.council.replication_targets(),
      rank,
    )
  }) else {
    return;
  };

  // Advance the council death watch every period, on every node (leader, voter and learner alike), so the
  // count is monotonic for a genuinely dead member and survives a leadership change — a fresh leader inherits
  // the fleet-wide death history rather than restarting the window (which would strand a real retirement
  // behind a re-election, docs/bugs/2026-09-17-council-retires-a-suspected-voter.md).
  state::with_state(update_council_death_watch);

  // A learner (non-voter member): it does not drive the Raft. It fetches the committed configuration from a
  // voter and adopts the newest (§4.8, D-14) — but **reactively**, only when it has evidence its configuration
  // is behind the region (§4.8 the piggyback rule), never on a bare period, so an idle learner whose view
  // matches its configuration sends nothing. The evidence is either flagged from the data plane
  // (`config_refresh_wanted` — a `ConfigurationStale` refusal it received to a record it sent, or a record it
  // accepted naming a newer generation) or read here from its own SWIM view diverging from its installed
  // membership ([`membership_diverges_from_config`] — the signal a quiescent takeover successor needs, since
  // it learns a failed owner's retirement and its own reassignment only by installing the committed
  // configuration). The adopted configuration is installed into placement by `sync_config_from_council`,
  // exactly as a voter's committed one is.
  if !is_voter && !is_leader {
    // An uninitialized council paces its fetches (`next_learner_wait`): its empty configuration makes every live peer
    // a divergence, which is no evidence. Recovery and a stale-configuration refusal fetch at once.
    let now = futures::now_ns();
    let wanted = state::with_state(|s| {
      if s.recovery.council.is_some() || s.config_refresh_wanted {
        s.council_learner_pace.reset();
        true
      } else if !s.council.initialized() {
        let due = s.council_learner_pace.due(now);
        if !due {
          s.count(LEARNER_FETCH_PACED, 1);
        }
        due
      } else {
        s.council_learner_pace.reset();
        membership_diverges_from_config(s) || region_announces_newer_configuration(s)
      }
    })
    .unwrap_or(false);
    if wanted {
      drive_learner_fetch(budget, in_flight).await;
      let _ = state::with_state_counted(|s| {
        s.config_refresh_wanted = false;
        let still = !s.council.initialized();
        s.council_learner_pace.fetched(futures::now_ns(), still);
      });
    }
    let timing = derive_group_timing(&voters, |s, timing| s.council_timing = timing);
    lapse_learner_lease(Group::Council, timer, contact, &timing, local, rank);
    return;
  }
  let others: Vec<HostId> = voters.into_iter().filter(|voter| *voter != local).collect();
  // This period's election timing, derived from the measured paths to the other voters (§4.8 "Derived
  // constants"): ten times the slowest voter's round-trip tail, floored at ten periods — the floor on any
  // loopback, and what `Daemon::council_timing` reports. Voters only: a member being caught up takes no part
  // in an election. The window follows the same tail: a batch for each period a lost batch takes to repair
  // (`docs/wip/research/consensus-enhancements.md` §3.5) — one on a loopback.
  let timing = derive_group_timing(&others, |s, timing| {
    s.council_timing = timing;
    let batch = crate::consensus::append_batch_bytes(s.council.voters().len());
    s.council
      .set_window_budget(timing.window_budget(HEARTBEAT_NS, batch));
  });
  // A leader's invitation (thesis §3.10, leadership transfer) outranks this period's timer.
  if campaign_if_invited(Group::Council, &others, budget, timer, in_flight).await {
    return;
  }

  if is_leader {
    let replicated: Vec<HostId> = targets.into_iter().filter(|peer| *peer != local).collect();
    lead_council_period(&replicated, budget, timer, &timing, in_flight).await;
    return;
  }
  if others.is_empty() {
    // The sole voter (a one-node council, the fleet degenerate): self-elect, then it leads next period.
    let _ = state::with_state_counted(|s| s.council.election_timeout());
    timer.reset();
    return;
  }
  follow_council_period(
    &others,
    budget,
    timer,
    &timing,
    (contact, local, rank),
    in_flight,
  )
  .await;
}

/// One period as a council **follower** ([`drive_config_council`]): its election timer, and its death report to the
/// leader, which retires a member only once f + 1 voters hold it dead.
async fn follow_council_period(
  others: &[HostId],
  budget: CommitBudget,
  timer: &mut ElectionTimer,
  timing: &ElectionTiming,
  (contact, local, rank): (u64, HostId, usize),
  in_flight: &mut Vec<Dispatch>,
) {
  // A follower: the timer resets while the leader keeps making contact; otherwise it ages toward its
  // jittered timeout under this period's derived timing and campaigns there. Silent past the minimum
  // election timeout, it forgets its leader so the voter it yields to wins its pre-vote (thesis §4.2.3).
  match timer.follower_period(contact, timing, local, rank) {
    FollowerStep::Campaign => {
      drive_council_election(others, budget, in_flight).await;
      // Re-baseline the contact counter so a fresh campaign is not immediately retriggered: a won election
      // makes this node leader next period; a lost one waits out the timer again.
      if let Some(contact) = state::with_state(|s| s.council.leader_contact()) {
        timer.rebaseline(contact);
      }
    }
    FollowerStep::LeaderLapsed => {
      let _ = state::with_state_counted(|s| s.council.forget_leader());
    }
    FollowerStep::Follow => {}
  }
  // A voter's deaths reach the leader, which retires a member only once f + 1 voters hold it dead.
  send_death_report(others, budget, in_flight).await;
}

/// Each alive region's **representative** host as this node sees it (§4.8, D-14): the lowest-id host of the
/// SWIM alive set in each region ([`root_representatives`] over the alive hosts and the fleet's host→region
/// assignment). The root leader moves the root voter set to the representatives of the committed regions
/// (`RootGroup::reconcile_voters`), so a region whose representative died is carried by its next live host.
fn alive_representatives(state: &ShardState) -> std::collections::BTreeMap<RegionId, HostId> {
  root_representatives(&authenticated_alive(state), &state.node_regions)
}

/// The regions currently **alive** as this node sees them (§4.8, D-14): the distinct regions of the SWIM
/// alive set, mapped through the fleet's host→region assignment (a host absent from the map is in the sole
/// region `RegionId(0)`). The root group's leader reconciles the region membership against this — an alive
/// host's region is an alive region, and a region with no alive host is retired.
fn alive_regions(state: &ShardState) -> Vec<RegionId> {
  let mut regions: Vec<RegionId> = authenticated_alive(state)
    .into_iter()
    .map(|host| {
      state
        .node_regions
        .get(&host)
        .copied()
        .unwrap_or(RegionId(0))
    })
    .collect();
  regions.sort_unstable_by_key(|region| region.0);
  regions.dedup();
  regions
}

/// Whether this node's own alive view of the regions differs from the region membership in the root
/// configuration it holds — the signal a **root learner** uses to fetch the committed root configuration
/// reactively (§4.8, D-14), the cross-region parallel of [`membership_diverges_from_config`]. Compared as
/// sets over [`alive_regions`] and the root configuration's regions; equal in steady state, so a converged
/// learner fetches nothing. (This rests on the all-to-all mesh, where a node's SWIM sees every region's
/// hosts; a region-scoped mesh would need a version-carrying signal instead — owed with region-scoped SWIM. One
/// version-carrying signal exists: a location query bound to a newer root than this node holds, [`root_learner_fetch`],
/// A-489, since a seat change moves the version and neither comparison here.)
/// A root voter it knows that its membership no longer holds alive is the same signal for the voters (A-406): the
/// group has replaced it or will, and the configuration records who votes once it has, while a region whose
/// representative died stays in the regions and moves nothing the first comparison sees.
fn root_diverges(state: &ShardState) -> bool {
  let alive: std::collections::BTreeSet<RegionId> = alive_regions(state).into_iter().collect();
  let known: std::collections::BTreeSet<RegionId> =
    state.root.configuration().regions.iter().copied().collect();
  alive != known
    || state.root.known_voters().iter().any(|voter| {
      state
        .fleet
        .membership()
        .state(*voter)
        .is_none_or(|held| held.liveness != Liveness::Alive)
    })
}

/// Drives this node's **root group** one period from the record-plane coordinator (§4.8, D-14 — the root
/// group across regions), the cross-region counterpart of [`drive_config_council`] and structurally its
/// parallel (a third such consensus group would motivate factoring the shared Raft-drive shape). As the
/// elected root master it reconciles the region membership from the regions currently alive
/// ([`alive_regions`]) and replicates over the transport ([`ROOT_STREAM`]); the sole voter self-elects; a
/// follower ages toward an election. A non-voter fetches the root's committed configuration;
/// a retiring leader keeps replicating until its removal commits. The root election timer is
/// independent of the council's timer.
async fn drive_root_group(
  local: HostId,
  budget: CommitBudget,
  timer: &mut ElectionTimer,
  in_flight: &mut Vec<Dispatch>,
) {
  let Some((is_voter, is_leader, contact, voters, targets, rank)) = state::with_state(|s| {
    if s.recovery.root.is_some() {
      return (
        false,
        false,
        s.root.leader_contact(),
        Vec::new(),
        Vec::new(),
        0,
      );
    }
    let voters = s.root.voters();
    // This period's root election priority and rank, as the council's (§3.4) — across regions, where the
    // quorum round trips differ and the priority does its work.
    let priority = quorum_priority(
      voters
        .iter()
        .filter(|voter| **voter != local)
        .map(|voter| s.peer_paths.get(voter)),
      voters.len(),
    );
    s.root.set_priority(priority);
    let rank = s.root.election_rank(&authenticated_alive(s));
    (
      s.root.is_voter(local),
      s.root.is_leader(),
      s.root.leader_contact(),
      voters,
      s.root.replication_targets(),
      rank,
    )
  }) else {
    return;
  };
  if !is_voter && !is_leader {
    // A root learner (a region member that is not its region's representative): it does not drive the Raft.
    // It fetches the committed root configuration from a root voter and adopts the newest (§4.8, D-14) —
    // reactively, only when its own alive view of the regions, or of the root voters it knows, diverges from the
    // root configuration it holds ([`root_diverges`]), so a converged learner sends nothing. The cross-region parallel
    // of the config learner's reactive fetch. It asks the voters its configuration records (A-406), and a peer whose
    // location query showed it a newer root (A-489, [`root_learner_fetch`]).
    // As the council learner: an uninitialized root paces its fetches; recovery fetches at once.
    let now = futures::now_ns();
    let fetch = state::with_state(|s| root_learner_fetch(s, now)).unwrap_or(RootLearnerFetch::Idle);

    if let RootLearnerFetch::Due { shown_by } = fetch {
      let known = state::with_state(|s| s.root.known_voters()).unwrap_or_else(|| voters.clone());
      let targets = with_shown_peer(root_learner_targets(&known, local), shown_by, local);
      drive_root_learner_fetch(&targets, budget, in_flight).await;
      let _ = state::with_state_counted(|s| {
        let still = !s.root.initialized();
        s.root_learner_pace.fetched(futures::now_ns(), still);
      });
    }
    let timing = derive_group_timing(&voters, |s, timing| s.root_timing = timing);
    lapse_learner_lease(Group::Root, timer, contact, &timing, local, rank);
    return;
  }
  let others: Vec<HostId> = voters.into_iter().filter(|voter| *voter != local).collect();
  // This period's election timing and window over the paths to the other root voters, as the council's.
  let timing = derive_group_timing(&others, |s, timing| {
    s.root_timing = timing;
    let batch = crate::consensus::append_batch_bytes(s.root.voters().len());
    s.root
      .set_window_budget(timing.window_budget(HEARTBEAT_NS, batch));
  });
  // A leader's invitation (thesis §3.10, leadership transfer) outranks this period's timer.
  if campaign_if_invited(Group::Root, &others, budget, timer, in_flight).await {
    return;
  }

  if is_leader {
    let replicated: Vec<HostId> = targets.into_iter().filter(|peer| *peer != local).collect();
    lead_root_period(&replicated, budget, timer, &timing, in_flight).await;
    return;
  }
  if others.is_empty() {
    // The sole root voter (a single-region fleet's degenerate): self-elect, then it leads next period.
    let _ = state::with_state_counted(|s| s.root.election_timeout());
    timer.reset();
    return;
  }
  // A follower: the timer resets while the leader keeps making contact; otherwise it ages toward its
  // jittered timeout under this period's derived timing and campaigns there. Silent past the minimum
  // election timeout, it forgets its leader so the voter it yields to wins its pre-vote (thesis §4.2.3).
  match timer.follower_period(contact, &timing, local, rank) {
    FollowerStep::Campaign => {
      drive_root_election(&others, budget, in_flight).await;
      if let Some(contact) = state::with_state(|s| s.root.leader_contact()) {
        timer.rebaseline(contact);
      }
    }
    FollowerStep::LeaderLapsed => {
      let _ = state::with_state_counted(|s| s.root.forget_leader());
    }
    FollowerStep::Follow => {}
  }
}

/// The root group's replication round over the transport — the parallel of [`drive_council_replication`] on
/// [`ROOT_STREAM`], reading [`ShardState::root`]: builds the append owed each borrowed root voter under a
/// brief borrow, ships them concurrently, folds the replies, and returns every session.
async fn drive_root_replication(
  others: &[HostId],
  budget: CommitBudget,
  in_flight: &mut Vec<Dispatch>,
) {
  let sessions = take_sessions("fleet.lent_to.drive_root_replication", |host| {
    others.contains(&host)
  });
  if sessions.is_empty() {
    return;
  }
  let appends: std::collections::BTreeMap<HostId, Vec<u8>> = state::with_state(|s| {
    let batch = crate::consensus::append_batch_bytes(s.root.voters().len());
    sessions
      .iter()
      .filter_map(|(host, _)| {
        s.root.replication_for(*host, batch).and_then(|message| {
          crate::consensus::encode_message(s, true, &message).map(|bytes| (*host, bytes))
        })
      })
      .collect()
  })
  .unwrap_or_default();
  let mut requests = Vec::new();
  let mut kept = Vec::new();
  for (host, endpoint) in sessions {
    match appends.get(&host) {
      Some(bytes) => requests.push((host, bytes.clone(), endpoint)),
      None => kept.push((host, endpoint)),
    }
  }
  let sent: Vec<HostId> = requests.iter().map(|(host, _, _)| *host).collect();
  let (replied, stragglers) = broadcast(requests, ROOT_STREAM, Priority::Control, budget).await;
  let mut recovered = kept;
  let mut replies = Vec::with_capacity(replied.len());
  for (host, reply, endpoint) in replied {
    replies.push((host, reply));
    recovered.push((host, endpoint));
  }
  state::with_state(|s| {
    sample_voter_paths(s, &replies);
    for (peer, reply) in replies {
      if let Ok(message) = crate::consensus::decode_message(s, true, peer, &reply.bytes) {
        s.root.fold_reply(message);
      }
    }
  });
  // A voter that replies late is settled by the coordinator: its acknowledgement is folded then and its
  // session returned, so a slow follower never costs the leader its ack or its session.
  in_flight.push(Dispatch::new(
    sent,
    &recovered,
    stragglers,
    LateReplies::Root,
  ));
  return_sessions(recovered);
}

/// The root group's pre-vote then vote election over the transport — the parallel of
/// [`drive_council_election`] on [`ROOT_STREAM`], reading [`ShardState::root`].
async fn drive_root_election(
  others: &[HostId],
  budget: CommitBudget,
  in_flight: &mut Vec<Dispatch>,
) {
  let Some(Some(pre_vote)) = state::with_state(|s| s.root.election_timeout().into_iter().next())
  else {
    return;
  };
  let sessions = take_campaign_sessions(others, budget).await;
  if sessions.is_empty() {
    return;
  }
  // Phase one — the pre-vote round over the borrowed root-voter sessions.
  let pre_bytes = state::with_state(|s| crate::consensus::encode_message(s, true, &pre_vote))
    .flatten()
    .unwrap_or_default();
  let requests: Vec<(HostId, Vec<u8>, Endpoint)> = sessions
    .into_iter()
    .map(|(host, endpoint)| (host, pre_bytes.clone(), endpoint))
    .collect();
  let sent: Vec<HostId> = requests.iter().map(|(host, _, _)| *host).collect();
  let (replied, stragglers) = broadcast(requests, ROOT_STREAM, Priority::Control, budget).await;
  let mut sessions = Vec::with_capacity(replied.len());
  let mut pre_replies = Vec::with_capacity(replied.len());
  for (host, reply, endpoint) in replied {
    pre_replies.push((host, reply));
    sessions.push((host, endpoint));
  }
  // A voter whose pre-vote reply is late is settled by the coordinator: its session comes back then (the
  // late pre-vote reply itself is not folded — the next campaign re-runs its pre-vote).
  in_flight.push(Dispatch::new(
    sent,
    &sessions,
    stragglers,
    LateReplies::Root,
  ));
  // Fold the pre-vote replies; a granted majority yields the real vote request to broadcast next.
  let vote = state::with_state(|s| {
    sample_voter_paths(s, &pre_replies);
    let mut vote = None;
    for (peer, reply) in &pre_replies {
      if let Ok(message) = crate::consensus::decode_message(s, true, *peer, &reply.bytes)
        && let Some(request) = s.root.fold_reply(message).into_iter().next()
      {
        vote = Some(request);
      }
    }
    vote
  })
  .flatten();
  let Some(vote) = vote else {
    return_sessions(sessions);
    return;
  };
  // Phase two — the real vote round over the same sessions.
  drive_vote_round(Group::Root, vote, sessions, budget, in_flight).await;
}

/// What a root learner does this period ([`root_learner_fetch`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RootLearnerFetch {
  /// No fetch.
  Idle,
  /// A fetch from the voters it knows ([`root_learner_targets`]) and, when a peer's location query showed it a newer
  /// root (A-489), from that peer too, which holds it.
  Due {
    /// The peer that showed this node a newer root than it holds, if one did since its last fetch.
    shown_by: Option<HostId>,
  },
}

/// Whether this root learner fetches the committed root configuration this period (§4.8, D-14), on the control shard:
/// recovery fetches at once; an uninitialized root paces its fetches; an initialized one fetches when its view of the
/// regions or of the voters it knows differs from the root it holds ([`root_diverges`]), or when a peer showed it a newer
/// root than it holds ([`ShardState::root_newer_seen`], taken here). Before A-489 the last was missing: a seat change
/// moves the root's version and nothing a learner compares (A-406), so a learner left behind at formation never fetched,
/// and refused every location round a peer on the newer root asked (`RootViewDiffers`), for the lookup's whole budget.
pub(crate) fn root_learner_fetch(state: &mut ShardState, now: u64) -> RootLearnerFetch {
  let held = state.root.configuration().version;
  let shown = state
    .root_newer_seen
    .filter(|(version, _)| *version > held)
    .map(|(_, peer)| peer);
  let due = if state.recovery.root.is_some() {
    state.root_learner_pace.reset();
    true
  } else if !state.root.initialized() {
    let due = state.root_learner_pace.due(now);
    if !due {
      state.count(LEARNER_FETCH_PACED, 1);
    }
    due
  } else {
    state.root_learner_pace.reset();
    root_diverges(state) || shown.is_some()
  };
  if due || shown.is_none() {
    // Taken by this fetch, or already reached: a version this node holds now needs nothing more.
    state.root_newer_seen = None;
  }
  if due {
    RootLearnerFetch::Due { shown_by: shown }
  } else {
    RootLearnerFetch::Idle
  }
}

/// The hosts a root learner's fetch asks: `targets` ([`root_learner_targets`]) and `shown_by`, the peer that showed it a
/// newer root (A-489), since the voters it knows can be as far behind as it is. An empty `targets` asks every session
/// the learner holds ([`drive_root_learner_fetch`]), the shown peer's among them, so it stays empty; this node is never
/// asked.
fn with_shown_peer(
  mut targets: Vec<HostId>,
  shown_by: Option<HostId>,
  local: HostId,
) -> Vec<HostId> {
  if let Some(peer) = shown_by
    && peer != local
    && !targets.is_empty()
    && !targets.contains(&peer)
  {
    targets.push(peer);
  }
  targets
}

/// Whom a root learner asks for the committed root configuration: the voters it knows. The voters it last learned
/// can all be gone (a lost region's representative was the only one it knew) while the leader has moved the voter set
/// to each live region's representative; with no session to any voter it knows, it asks those representatives
/// instead, or it never hears the root's later commits (found 2026-10-07: a region-1 learner stayed on the
/// pre-promotion root for good, its only known voter the dead region-0 representative, 4,269 fetches finding no
/// session). A learner that knows no voter yet names none, and so asks every session it holds, as it always has.
fn root_learner_targets(voters: &[HostId], local: HostId) -> Vec<HostId> {
  let reachable = state::with_state(|s| {
    voters
      .iter()
      .any(|voter| s.record_sessions.contains_key(voter))
  })
  .unwrap_or(false);
  if reachable || voters.is_empty() {
    return voters.to_vec();
  }
  state::with_state(|s| {
    alive_representatives(s)
      .into_values()
      .filter(|host| *host != local)
      .collect::<Vec<HostId>>()
  })
  .unwrap_or_default()
}

/// Drives a **root learner** one period: fetches the committed root configuration from the root voters it has
/// a session to and adopts the newest (§4.8, D-14) — the cross-region parallel of [`drive_learner_fetch`].
/// The fetch carries this learner's root version, so a caught-up learner's fetch is an empty reply, not a
/// full transfer; every borrowed session is returned. `adopt` only moves forward, so folding all replies
/// leaves the learner on the highest version any reachable voter returned.
async fn drive_root_learner_fetch(
  voters: &[HostId],
  budget: CommitBudget,
  in_flight: &mut Vec<Dispatch>,
) {
  let sessions = take_sessions("fleet.lent_to.drive_root_learner_fetch", |host| {
    voters.is_empty() || voters.contains(&host)
  });
  if sessions.is_empty() {
    return;
  }
  let request = state::with_state(|s| {
    slates_wire::Wire::to_bytes(&crate::consensus::Fetch {
      group: if s.recovery.root.is_some() {
        None
      } else {
        s.root_group.as_ref().map(|identity| identity.id)
      },
      version: s.root.configuration().version,
    })
  })
  .unwrap_or_default();
  let requests: Vec<(HostId, Vec<u8>, Endpoint)> = sessions
    .into_iter()
    .map(|(host, endpoint)| (host, request.clone(), endpoint))
    .collect();
  let sent: Vec<HostId> = requests.iter().map(|(host, _, _)| *host).collect();
  let (replied, stragglers) =
    broadcast(requests, ROOT_FETCH_STREAM, Priority::Metadata, budget).await;
  let _ = state::with_state_counted(|s| {
    s.count(
      LEARNER_FETCHES,
      u64::try_from(sent.len()).unwrap_or(u64::MAX),
    );
  });
  let mut recovered = Vec::with_capacity(replied.len());
  let mut fetched = Vec::with_capacity(replied.len());
  for (host, reply, endpoint) in replied {
    fetched.push((host, reply.bytes));
    recovered.push((host, endpoint));
  }
  // A late fetch reply is adopted when it arrives (the coordinator settles the round's stragglers).
  in_flight.push(Dispatch::new(
    sent,
    &recovered,
    stragglers,
    LateReplies::RootFetch,
  ));
  return_sessions(recovered);
  state::with_state(|s| {
    for (peer, bytes) in &fetched {
      crate::consensus::adopt_fetch(s, true, *peer, bytes);
    }
  });
}

/// Drives a **learner** (a non-voter member) one period: fetches the committed regional configuration from every
/// member of its region it holds a session to, and adopts the newest (§4.8, D-14 — the council is a small elected set,
/// so a non-voter learns the configuration rather than voting on it; the adopted configuration is then installed into
/// placement by [`sync_config_from_council`] exactly as a voter's committed one is). Any member can answer: what a member
/// serves is a configuration it applied, so a committed one, the answer is empty unless it is newer than the asker's,
/// and adoption only moves forward, so a lagging member's copy never holds the learner back. Asked of voters alone, a
/// learner was stranded: its voters are those of its join state, which a fetched configuration does not update, so
/// once a learner that joined from the bootstrapper lost it, it had no one to ask (CI diag-raft-chaos-mesh-52: both
/// non-voters at version 8 against 11 and 12 for a whole fleet wait). A learner fetches only on evidence that it is
/// behind (`drive_config_council`), so a converged region sends nothing. Every borrowed session is returned.
async fn drive_learner_fetch(budget: CommitBudget, in_flight: &mut Vec<Dispatch>) {
  let regional_peers: Vec<HostId> = state::with_state(|s| {
    s.record_sessions
      .keys()
      .copied()
      .filter(|host| same_region(s, *host))
      .collect()
  })
  .unwrap_or_default();
  let sessions = take_sessions("fleet.lent_to.drive_learner_fetch", |host| {
    regional_peers.contains(&host)
  });
  if sessions.is_empty() {
    let _ = state::with_state_counted(|s| s.count(LEARNER_FETCH_NO_SESSION, 1));
    return;
  }
  // The fetch carries this learner's current configuration version, so a voter returns the configuration
  // only when it has a newer one (a caught-up learner's fetch is then an empty reply, not a full transfer).
  let request = state::with_state(|s| {
    slates_wire::Wire::to_bytes(&crate::consensus::Fetch {
      group: if s.recovery.council.is_some() {
        None
      } else {
        s.council_group.as_ref().map(|identity| identity.id)
      },
      version: s.council.configuration().version,
    })
  })
  .unwrap_or_default();
  let requests: Vec<(HostId, Vec<u8>, Endpoint)> = sessions
    .into_iter()
    .map(|(host, endpoint)| (host, request.clone(), endpoint))
    .collect();
  let sent: Vec<HostId> = requests.iter().map(|(host, _, _)| *host).collect();
  let (replied, stragglers) =
    broadcast(requests, CONFIG_FETCH_STREAM, Priority::Metadata, budget).await;
  let asked_at_ns = futures::now_ns();
  let _ = state::with_state_counted(|s| {
    s.count(
      LEARNER_FETCHES,
      u64::try_from(sent.len()).unwrap_or(u64::MAX),
    );
    s.learner_fetch.asked_at_ns = Some(asked_at_ns);
    s.learner_fetch.asked = u32::try_from(sent.len()).unwrap_or(u32::MAX);
  });
  let mut recovered = Vec::with_capacity(replied.len());
  let mut fetched = Vec::with_capacity(replied.len());
  for (host, reply, endpoint) in replied {
    fetched.push((host, reply.bytes));
    recovered.push((host, endpoint));
  }
  // A late fetch reply is adopted when it arrives (the coordinator settles the round's stragglers).
  in_flight.push(Dispatch::new(
    sent,
    &recovered,
    stragglers,
    LateReplies::ConfigFetch,
  ));
  return_sessions(recovered);
  // Adopt the newest fetched configuration (`adopt` only moves forward, so folding all replies leaves the
  // learner on the highest version any reachable voter returned).
  state::with_state(|s| {
    for (peer, bytes) in &fetched {
      crate::consensus::adopt_fetch(s, false, *peer, bytes);
    }
    s.learner_fetch.fetched_version = s.council.configuration().version;
  });
}

/// Installs the configuration the council has committed into this node's placement view and takes over any
/// object whose owner it has retired (§4.8, D-14 — the council is the authority, this node places under what
/// it agreed). Derives this node's owner view from the regional configuration (`configuration_for`) and
/// installs it ([`slates_cluster::fleet::FleetNode::install_configuration`]): the placement configuration the verbs read is updated,
/// the owner acceptor's authority is brought into step (records write under the current generation), and the
/// departed owners' objects this node now owns are returned — recorded as pending takeovers the record plane
/// drives, with every held acceptor's authority brought into step so this node can promote what it took over
/// and answer another survivor's promotion of the rest. Idempotent: an unchanged configuration takes over
/// nothing, so the coordinator calls it every period.
fn sync_config_from_council(local: HostId) {
  // The takeover drive and the held-record fences live on this control shard — the peer records arrive here —
  // so the install runs only here, and only when the council has committed something new (source-gated: the
  // version advances on every change, so an equal version is the same configuration, the near-zero rate the
  // design makes a tripwire). The committed configuration reaches the node's other shards separately, every
  // period, through [`fan_configs_to_shards`], so a failed cross-shard dispatch self-heals.
  state::with_state(|s| {
    s.consensus_ready = !s.recovery.joining()
      && s.council.initialized()
      && s.root.initialized()
      && s.council.configuration().members.contains(&local);

    if s.council.configuration().version == s.fleet.configuration().version {
      return;
    }
    let (configuration, members) = {
      let regional = s.council.configuration();
      (regional.configuration_for(local), regional.members.clone())
    };
    let Some(configuration) = configuration else {
      return;
    };
    // Measure the newly-committed configuration against the durability policy (§4.8, D-14 — the copyset count
    // check at every configuration change): a breach is counted, and the measured shortfall is what this
    // shard's writes are refused with from now on; a no-op when no operator durability policy is declared.
    s.durability_shortfall = crate::daemon::record_durability(
      &configuration,
      s.config
        .fleet
        .as_ref()
        .and_then(|membership| membership.durability),
    );
    // This node installed a newer configuration: any supersession it learned is resolved at or below it,
    // so its own lease can hold again once holders confirm it under the new version; the bounded startup
    // allowance restarts from now (a takeover under this version cannot yet have committed).
    s.lease
      .installed(configuration.version, slates_machine::clock::monotonic_ns());
    // At this host's own epoch advanced (retired while away, admitted again under its id), its catalogued volumes are
    // yielded to their successors (A-334, `crate::readmission`).
    let version = configuration.version;
    crate::readmission::install_placement(s, configuration, &members);
    // The install reaches the held register copies by a walk a slice a turn (A-398, [`InstallWalk`]): each copy's
    // fence raised to its owner's committed epoch, its object moved to the successor a kept retirement's lineage ranks
    // or let go as stale, and its authority moved to the owner its route names ([`install_on_copy`]). Until the walk
    // reaches a copy, the copy refuses a record of this version (its own generation is older), and a takeover round's
    // page installs its asker on the copies it lists itself; so the walk changes when a copy follows, never what it may
    // accept (`docs/wip/models/InstallInTurns.tla`). A newer install begins the walk again from the first copy.
    s.install_walk = Some(InstallWalk {
      version,
      after: None,
      walking: false,
    });
    s.count(INSTALL_WALKS, 1);
  });
}

/// The install of the committed configuration on this holder's register copies, a slice a turn (A-398; §4.8
/// "Configuration, by consensus"; CLAUDE.md "Bounded work everywhere"). An install once moved every held copy in the
/// turn that installed the version: each fence raised, each object a kept retirement names moved to its successor and
/// each authority reconciled, one turn of the control shard over every copy this node holds. Now the version is
/// installed in that turn (the node's placement, so a record of another generation is refused at the door,
/// `check_held_record`) and the copies follow by this walk, in key order, a step quantum of the control shard a turn
/// ([`install_slice`]), each turn a turn of its own ([`crate::xshard::Turns`]). Model-checked first
/// (`docs/wip/models/InstallInTurns.tla`, three holders, two takeovers): a copy reconciled any time after its node
/// installed the version keeps TotalOrder, Continuity, StaleNeverCommits and ReadSafety; so does one that, besides,
/// skips its own generation check, or whose fences are not raised, or that answers phase one under its old authority;
/// only with all three gone does a stale owner's record commit.
#[derive(Clone, Copy, Debug)]
pub(crate) struct InstallWalk {
  /// The configuration version the walk brings the copies to: a newer install begins a walk of its own.
  version: u64,
  /// The last copy the walk reached, in key order; `None` before the first.
  after: Option<ObjectId>,
  /// Whether a task walks it now ([`walk_install`]); the coordinator's period starts one when none does, so a task the
  /// shard refused, or one whose turn missed its deadline, is begun again where the walk stands.
  walking: bool,
}

/// Counter: an install whose walk over the held register copies began ([`InstallWalk`], A-398).
const INSTALL_WALKS: &str = "fleet.config.install_walks";
/// Counter: one turn of an install's walk ([`install_slice`]).
const INSTALL_SLICES: &str = "fleet.config.install_slices";
/// Counter: an install's walk that reached every held copy.
const INSTALL_WALKED: &str = "fleet.config.installs_walked";
/// Counter: a walk task the control shard could not spawn; the next period begins it again.
const INSTALL_UNWALKED: &str = "fleet.config.install_unwalked";

/// Begins a task walking the install owed on this (the control) shard when none walks it ([`InstallWalk`]): at the
/// period after the install, and again at a later period if the task could not be spawned or its turn was not answered.
fn drive_install_walk(local: HostId) {
  let Some((control, version)) = state::with_state(|s| {
    let walk = s.install_walk.as_mut()?;
    if walk.walking {
      return None;
    }
    walk.walking = true;
    Some((s.shard, walk.version))
  })
  .flatten() else {
    return;
  };
  let spawned = futures::spawn(walk_install(control, local, version)).and_then(futures::detach);
  if spawned.is_err() {
    let _ = state::with_state_counted(|s| {
      s.count(INSTALL_UNWALKED, 1);
      if let Some(walk) = s.install_walk.as_mut()
        && walk.version == version
      {
        walk.walking = false;
      }
    });
  }
}

/// Walks the install of `version` over this shard's held register copies a turn at a time ([`install_slice`]), each
/// turn awaited within a heartbeat and a turn of the shard of its own. Ends once every copy took the version, a newer
/// install began its own walk, or a turn was not answered within its deadline (the period begins the walk again where
/// it stands).
async fn walk_install(control: u16, local: HostId, version: u64) {
  let mut turns = crate::xshard::Turns::new(control, control);
  loop {
    match turns
      .call(move |s| install_slice(s, local, version), HEARTBEAT_NS)
      .await
    {
      Some(true) => {}
      Some(false) => return,
      None => break,
    }
  }
  let _ = state::with_state_counted(|s| {
    if let Some(walk) = s.install_walk.as_mut()
      && walk.version == version
    {
      walk.walking = false;
    }
  });
}

/// One turn of an install's walk: the held copies after the walk's last, in key order, each brought to the installed
/// version ([`install_on_copy`]), until the shard's step quantum is spent (or the one a test injected); the first copy
/// is always reached, so every turn moves the walk on. Whether another turn is owed: not once every copy took the
/// version, nor for a walk a newer install replaced, nor while the council has committed a version past it (the next
/// period installs that one and begins its walk afresh, so the copies are never brought to a configuration the node's
/// placement has not installed).
fn install_slice(s: &mut ShardState, local: HostId, version: u64) -> bool {
  let Some(walk) = s.install_walk else {
    return false;
  };
  if walk.version != version
    || s.fleet.configuration().version != version
    || s.council.configuration().version != version
  {
    return false;
  }
  s.count(INSTALL_SLICES, 1);
  let quantum = s
    .install_walk_quantum_ns
    .or_else(futures::step_budget_ns)
    .unwrap_or(s.config.runtime.step_budget_ns);
  let regional = s.council.configuration().clone();
  let retired: std::collections::BTreeSet<HostId> = regional.retired.keys().copied().collect();
  let began = futures::now_ns();
  let mut after = walk.after;
  loop {
    let next = match after {
      Some(last) => s
        .holder_records
        .as_map()
        .range((std::ops::Bound::Excluded(last), std::ops::Bound::Unbounded))
        .next()
        .map(|(object, _)| *object),
      None => s.holder_records.keys().next().copied(),
    };
    let Some(object) = next else {
      s.install_walk = None;
      s.count(INSTALL_WALKED, 1);
      return false;
    };
    install_on_copy(s, local, (&regional, &retired), version, object);
    after = Some(object);
    if futures::now_ns().saturating_sub(began) >= quantum {
      break;
    }
  }
  if let Some(walk) = s.install_walk.as_mut() {
    walk.after = after;
  }
  true
}

/// The installed configuration on one held copy, `object`'s, as the install once did it to every copy at once, in its
/// order: the copy's fence raised to its owner's committed epoch (§4.8 "every holder raises its fence for that host to
/// the new epoch"; read against the owner the copy names before it moves, so a departed owner's bumped epoch fences
/// its resumed stale self `StaleEpoch`), then the object moved to the successor its lineage ranks or let go as stale
/// (`takeover::resolve_held_object`), then its authority moved to the owner its route names at `generation`
/// ([`reconcile_held_copy`]).
pub(crate) fn install_on_copy(
  s: &mut ShardState,
  local: HostId,
  (regional, retired): (
    &slates_db::register::RegionalConfiguration,
    &std::collections::BTreeSet<HostId>,
  ),
  generation: u64,
  object: ObjectId,
) {
  if let Some(acceptor) = s.holder_records.get_mut(&object)
    && let Some(epoch) = regional.epochs.get(&acceptor.owner())
  {
    acceptor.raise_fence(*epoch);
  }
  crate::takeover::resolve_held_object(s, local, regional, object);
  reconcile_held_copy(s, object, generation, retired);
}

/// Fans the control shard's current committed configurations — the council's placement `Configuration` and the
/// root group's `RootConfiguration` — to every other shard each period (§4.8, D-7 "one owning shard per
/// volume", §4.8 "Lookup"). Every shard serves clients: the placement verbs (`place`/`region_placed`/
/// `await_placed`/`host_epoch`) and the cross-region lookup guard (`crate::verbs::home_redirect`) run on a
/// volume's owner shard, which may not be this control shard, so each shard must read what the council and the
/// root group committed or a volume owned elsewhere would report a stale placement or home after a membership
/// change, move or promotion. Only the control shard drives the council and the root group and installs their
/// commits (`sync_config_from_council`, `drive_root_group`); every other shard holds a read-only copy fanned
/// from here.
///
/// **Re-fanned every period, not only on change.** `run_on` is refused when a shard's control channel is
/// momentarily full ([`slates_rt::RtError::ControlFull`]), and a fan dropped on the one period a configuration changed
/// would otherwise leave that shard stale until the next change. Re-fanning heals it next period. This is the
/// authoritative cross-shard state: an owner shard routes and admits from the **committed configuration**
/// fanned here (`fleet.configuration()`, `object_owner`), never from the raw SWIM membership, which
/// The peer-state fold publishes to the other shards **best-effort** for D-7 uniformity but which no
/// off-control-shard path reads, so a membership fold dropped on a full channel is harmless where a dropped
/// configuration fan would not be. The receiving side is
/// version-gated (`install_configuration` installs only a newer version; `RootGroup::adopt` ignores an
/// equal-or-older one), so a re-fan of an unchanged configuration is a no-op there. Both configurations ride
/// **one** cross-shard message per shard, and both are bounded (a bounded neighbourhood; regions, moved homes
/// and promotions), so the per-period cost is bounded at every scale. A non-control shard tracks no held
/// object, so its `install_configuration` returns no reassignment to drive — takeover stays on this shard.
fn fan_configs_to_shards(origin: u16, shards: &[u16]) {
  let Some(fans) = state::with_state(|s| {
    // Prune the owner-lease ledgers to the current members each period (a restarted peer's retired id
    // leaves; §4.8 "Leases and reads", AUD-08; a former member's last answer stays until its horizon passes, since a
    // promotion of its objects waits on it, A-351), then fan this node's lease evidence to every owner shard —
    // the verb gate (`verbs::dispatch`) and the mount (`crate::nfs`) run there and must read the same
    // confirmation the control shard's probe tasks collected. `answers_given` and `departed_owners` stay on
    // the control shard (only the takeover drive reads them), so they are not fanned.
    let members = s.fleet.members();
    s.lease.retain_members(members);
    s.answers_given
      .retain_members(members, slates_machine::clock::monotonic_ns());
    s.fanned.observe_lease(&s.lease);
    // This shard's own parked lease reads meet the evidence it just pruned and observed.
    crate::lease_wait::resolve(s);
    let mut fans = Vec::new();
    for shard in shards.iter().copied().filter(|shard| *shard != origin) {
      if let Some(fan) = s.fanned.owed(s, shard) {
        fans.push((shard, fan));
      } else {
        count_refusal_in(s, FAN_UNCHANGED);
      }
    }
    fans
  }) else {
    return;
  };
  for (shard, fan) in fans {
    let delivered = fan.delivered();
    match run_on(origin, shard, move |s| {
      fan.install(s);
      // The owner shard's parked lease reads meet the evidence the fan brought (`crate::lease_wait`).
      crate::lease_wait::resolve(s);
    }) {
      Ok(()) => {
        let _ = state::with_state_counted(|s| {
          s.fanned.record(shard, delivered);
          count_refusal_in(s, FAN_SENT);
        });
      }
      Err(_) => {
        let _ = state::with_state_counted(|s| count_refusal_in(s, FAN_REFUSED));
      }
    }
  }
}

/// Counter: a period's configuration fan to a shard carried something that changed (AUD-29-29).
pub(crate) const FAN_SENT: &str = "fleet.fan.sent";
/// Counter: a period found nothing changed for a shard and sent it nothing (AUD-29-29).
pub(crate) const FAN_UNCHANGED: &str = "fleet.fan.unchanged";
/// Counter: a shard's bounded channel refused a fan; it is sent again the next period (AUD-29-29).
pub(crate) const FAN_REFUSED: &str = "fleet.fan.refused";

/// What one shard last received of the configuration fan: the placement and root versions, the readiness
/// flag, and the lease's generation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Delivered {
  placement: u64,
  root: u64,
  ready: Option<bool>,
  lease: u64,
}

/// The control shard's memory of the configuration fan (AUD-29-29): what each other shard last received, and
/// the lease as last fanned with a generation counting its changes, so a period clones and sends only what a
/// shard lacks. Before 2026-10-01 every period cloned the placement, the members, the root configuration and
/// the lease once, and again for every shard, and the receiver checked versions only after the copies.
#[derive(Debug, Default)]
pub(crate) struct Fanned {
  shards: std::collections::BTreeMap<u16, Delivered>,
  lease: crate::lease::OwnerLease,
  lease_generation: u64,
  /// The failover delay each other shard last received from the beat's own fan ([`fan_failover`], A-574). Bounded by
  /// the daemon's shards.
  failover: std::collections::BTreeMap<u16, u64>,
}

impl Fanned {
  /// Notes the control shard's lease: a change since the last fanned copy is a new generation.
  fn observe_lease(&mut self, lease: &crate::lease::OwnerLease) {
    if *lease != self.lease {
      self.lease = lease.clone();
      self.lease_generation = self.lease_generation.saturating_add(1);
    }
  }

  /// The fan `shard` is owed now — only the parts newer than what it last received — or `None` when it holds
  /// everything already.
  fn owed(&self, state: &ShardState, shard: u16) -> Option<Fan> {
    let had = self.shards.get(&shard).copied().unwrap_or_default();
    let placement = state.fleet.configuration();
    let root = state.root.configuration();
    let fan = Fan {
      placement: (placement.version > had.placement || had.ready.is_none())
        .then(|| (placement.clone(), state.fleet.members().to_vec())),
      root: (root.version > had.root).then(|| root.clone()),
      ready: (had.ready != Some(state.consensus_ready)).then_some(state.consensus_ready),
      lease: (self.lease_generation > had.lease).then(|| self.lease.clone()),
      now: Delivered {
        placement: placement.version,
        root: root.version,
        ready: Some(state.consensus_ready),
        lease: self.lease_generation,
      },
    };
    fan.carries().then_some(fan)
  }

  /// Records that `shard`'s channel accepted a fan of `delivered`.
  fn record(&mut self, shard: u16, delivered: Delivered) {
    self.shards.insert(shard, delivered);
  }

  /// The failover delay `shard` is owed: `failover_ns`, the control shard's, when it is not what the shard last received
  /// ([`fan_failover`], A-574).
  fn failover_owed(&self, failover_ns: u64, shard: u16) -> Option<u64> {
    (self.failover.get(&shard) != Some(&failover_ns)).then_some(failover_ns)
  }

  /// Records that `shard`'s channel accepted a failover delay of `failover_ns`.
  fn record_failover(&mut self, shard: u16, failover_ns: u64) {
    self.failover.insert(shard, failover_ns);
  }
}

/// One shard's configuration fan: each part present only when the shard lacks it.
struct Fan {
  placement: Option<(slates_db::register::Configuration, Vec<HostId>)>,
  root: Option<slates_db::register::RootConfiguration>,
  ready: Option<bool>,
  lease: Option<crate::lease::OwnerLease>,
  now: Delivered,
}

impl Fan {
  fn carries(&self) -> bool {
    self.placement.is_some() || self.root.is_some() || self.ready.is_some() || self.lease.is_some()
  }

  fn delivered(&self) -> Delivered {
    self.now
  }

  /// Installs the fan on the receiving shard; each install stays version-gated there.
  fn install(self, s: &mut ShardState) {
    if let Some((placement, members)) = self.placement
      && (!s.consensus_ready || placement.version > s.fleet.configuration().version)
    {
      // This shard serves its own writes, so it measures the fanned configuration against the durability
      // policy for itself (the shortfall its writes are refused with); the control shard already counted this
      // change's breach, so the measurement here moves no signal.
      s.durability_shortfall = s
        .config
        .fleet
        .as_ref()
        .and_then(|membership| membership.durability)
        .and_then(|bound| bound.shortfall(&placement));
      // This shard's own catalog is yielded at this host's epoch advanced, as the control shard's is (A-334).
      crate::readmission::install_placement(s, placement, &members);
    }
    if let Some(root) = self.root {
      s.root.adopt(root);
    }
    if let Some(ready) = self.ready {
      s.consensus_ready = ready;
    }
    // The owner lease is authoritative on the control shard; every other shard reads the fanned copy. The
    // confirmations carry absolute send times on the shared host clock, so a fan a full channel dropped only
    // shortens the lease on that shard until the next period re-fans it (never lengthens it).
    if let Some(lease) = self.lease {
      s.lease = lease;
    }
  }
}

/// Counter: a beat's fan of the failover delay to a shard whose last received delay it no longer was (A-574).
pub(crate) const FAILOVER_FAN_SENT: &str = "fleet.failover_fan.sent";
/// Counter: a shard's bounded channel refused the failover delay's fan; it is sent again at the next beat (A-574).
pub(crate) const FAILOVER_FAN_REFUSED: &str = "fleet.failover_fan.refused";

/// Fans this node's failover delay to every other shard whose last received delay it is not (A-363, A-574), from the
/// control shard's beat on every node, laptop or fleet (`daemon::heartbeat_loop`), so every shard's waits read the
/// control shard's estimate within a beat of its moving. A shard whose channel refused it is fanned again at the next
/// beat. Until A-574 the coordinator's configuration fan carried it, which a laptop does not run, so a laptop's other
/// shards read the delay their boot set, for good.
pub(crate) fn fan_failover() {
  let Some((origin, owed)) = state::with_state(|s| {
    let owed: Vec<(u16, u64)> = s
      .shards
      .iter()
      .copied()
      .filter(|shard| *shard != s.shard)
      .filter_map(|shard| {
        s.fanned
          .failover_owed(s.failover_ns, shard)
          .map(|failover_ns| (shard, failover_ns))
      })
      .collect();
    (s.shard, owed)
  }) else {
    return;
  };
  for (shard, failover_ns) in owed {
    match run_on(origin, shard, move |s| s.failover_ns = failover_ns) {
      Ok(()) => {
        let _ = state::with_state_counted(|s| {
          s.fanned.record_failover(shard, failover_ns);
          count_refusal_in(s, FAILOVER_FAN_SENT);
        });
      }
      Err(_) => {
        let _ = state::with_state_counted(|s| count_refusal_in(s, FAILOVER_FAN_REFUSED));
      }
    }
  }
}

/// Refreshes this node's failover estimate for the period (§4.8 "Derived constants", A-363): the detection term from the
/// member plane (SWIM's count of periods from a crashed peer's last answer to its condemnation, each at the detector's
/// measured period's running tail, `MemberPlane::detection`), and the confirmation from the council's election timing
/// this period derived, at the coordinator's measured period. The recovery term moves as takeovers and volumes complete
/// (`takeover::drive_host_takeover`, `fleet::materialize`). Nothing sets the estimate: it is what this node measured.
fn refresh_failover(state: &mut ShardState) {
  let detection = state
    .plane
    .plane
    .as_ref()
    .and_then(slates_cluster::member_plane::MemberPlane::detection);
  let timing = state.council_timing;
  state.failover.refresh(detection, &timing, HEARTBEAT_NS);
  publish_failover(state);
}

/// Reads this node's supervision for the beat (A-574): the widest window the watchdog judges this daemon's shards by
/// (A-455) and the lapse bound its anchor publishes (A-467, `lapse_bound_ns`, `None` where no anchor publishes one),
/// into the failover estimate, where they are the detection while no peer's detector judges the node (a laptop at
/// `f = 0`, a fleet node before its detector's first period), then publishes the delay. The control shard's beat runs it
/// on every node, laptop or fleet, and fans the delay to every shard ([`fan_failover`]).
pub(crate) fn refresh_supervision(state: &mut ShardState, lapse_bound_ns: Option<u64>) {
  let widest_window_ns = crate::watchdog::widest_window_ns(&state.shards);
  state.failover.supervise(widest_window_ns, lapse_bound_ns);
  publish_failover(state);
}

/// Publishes this node's failover estimate as the delay its waits read (A-363): the estimate in whole coordinator
/// periods, or a test's injection. Run on the control shard after each refresh of a term, the coordinator's (detection
/// and confirmation, [`refresh_failover`]) and the beat's (the anchor's supervision, [`refresh_supervision`]); the beat
/// fans it to every other shard where it moved ([`fan_failover`]).
pub(crate) fn publish_failover(state: &mut ShardState) {
  state.failover_ns = state
    .injected_failover_ns
    .unwrap_or_else(|| failover_in_periods(state.failover.estimate().total_ns()));
}

/// Derived: a failover delay of `ns` in whole coordinator periods, rounded up (A-363): the granularity every wait that
/// reads it acts at (a takeover counts its attempts a period at a time, and the forward lane and the location round
/// look a period apart), so the fan carries the estimate only when it crosses a period, not at every jitter of its
/// tails.
pub(crate) fn failover_in_periods(ns: u64) -> u64 {
  ns.div_ceil(HEARTBEAT_NS).saturating_mul(HEARTBEAT_NS)
}

/// The record-plane coordinator (§4.8 "records are sent to all candidates; committed at `f + 1`"; "Promotion
/// and takeover"): one task per node that, each period, ships every unplaced head to all its candidates in
/// one commit and drives every owed takeover over all surviving holders, borrowing the holder sessions the
/// per-peer link tasks keep in the shard state ([`ShardState::record_sessions`]). Consolidating the former
/// per-peer ship tasks into one dispatcher is what lets a single commit reach all candidates at once (the
/// design's shape, not N independent single-holder commits) and, decisively, what lets an `f > 1` takeover
/// promote over the several surviving holders one object needs — a per-peer task held only its own peer's
/// session and could reach a one-holder (`f = 1`) quorum only.
///
/// A dispatch returns at quorum; the holders still in flight hand their sessions back later
/// ([`Stragglers`]), recovered here each period ([`Dispatch::settle`]), so an early quorum costs no slow
/// holder its session. The coordinator keeps its own owner acceptor (a dispatch holds it across awaits, which
/// the brief `with_state` borrow cannot span) and re-installs the configuration's authority on it each
/// period, so it writes under the current generation after a membership change. The connection-ID demux that
/// would carry all sessions over one socket is owed and would leave this coordinator unchanged — only the
/// socket count beneath the link tasks falls from O(N) to one.
/// `progress` is this daemon's forward-progress heartbeat (§4.14): the coordinator bumps it once per period,
/// so an out-of-band observer ([`crate::daemon::Daemon::fleet_progress`], read directly off the atomic — no
/// shard round-trip) can tell a coordinator that is merely **slow under CPU load** (still cycling, fewer
/// periods per wall-second) from one that has **stalled** (no bump). A test's `poll_until` charges its budget
/// against these periods, not wall-clock, so a correct-but-starved operation is never falsely failed.
async fn run_record_plane(local: HostId) {
  let Some(authority) = owner_authority(local) else {
    return;
  };
  let mut owner_acceptor = Acceptor::new(local, authority);
  // This shard (the control shard) holds the peer sessions and coordinates; every shard owns volumes, so
  // each period reaches every owner shard for its seals and heads (`xshard`).
  let (origin, shards) = state::with_state(|s| (s.shard, s.shards.clone())).unwrap_or_default();
  // Dispatches whose holders are still in flight. Bounded: each is spent within the dispatch span
  // (`CommitBudget::max_deadline_ns`), so at most that span's worth of periods' dispatches are ever held.
  let mut in_flight: Vec<Dispatch> = Vec::new();
  // The configuration council is driven from this one coordinator (§4.8, D-14): its brief borrow of the
  // voter sessions is sequential with the record ships below, so it never contends for them. Its election
  // timer persists across periods (a follower ages toward a campaign on it; a leader ticks CheckQuorum on it;
  // it keeps the last leader contact it saw and its jitter rotation).
  let mut council_timer = ElectionTimer::new();
  // The root group's own election timer (§4.8, D-14 — the cross-region authority), distinct from the
  // council's: it is a separate Raft over the region representatives, driven from this same coordinator.
  let mut root_timer = ElectionTimer::new();
  loop {
    // Forward-progress heartbeat: one bump per coordinator period. An observer reads it to tell a fleet that
    // is slow under CPU load (still cycling) from one that has stalled (no bump) — see `progress`.
    slates_rt::registry::with_current(|ctx| ctx.beat_progress());
    // The merge record plane stamps each version with the period it first works it in, and judges its placement by
    // it (`merge_service::PLACED_WITHIN_PERIOD`).
    let _ = state::with_state(|s| {
      s.merge.coordinator_period = s.merge.coordinator_period.saturating_add(1);
      // The period's length is a sample of the coordinator's period, which the council's death watch counts in
      // (A-363): a starved coordinator's periods stretch, and its failover estimate with them.
      s.failover.period_began(slates_rt::futures::now_ns());
    });
    in_flight.retain_mut(|dispatch| !dispatch.settle());
    // This period's round budget, derived from the slowest measured peer path — every dispatch below (a
    // council or root round, a record commit, a takeover promotion, a content put, a learner fetch) gathers
    // replies from peers, so the round must outlast the farthest one's round trip ([`consensus_budget`]).
    // On one host every path is inside a heartbeat and this is the budget it always was (R8).
    let budget = consensus_budget(slowest_path_tail_ns());
    // Drive the configuration authority first — an election or a replication heartbeat over the transport —
    // then the records under the configuration it maintains.
    drive_config_council(local, budget, &mut council_timer, &mut in_flight).await;
    // The failover estimate for this period, from the detector as it stands and the council's timing just derived
    // (A-363): published in the daemon's status as it moves.
    let _ = state::with_state_counted(refresh_failover);
    // Drive the root group across regions on the same coordinator (§4.8, D-14): a brief borrow of the
    // root-voter sessions, sequential with the council's and the record ships, so it never contends.
    drive_root_group(local, budget, &mut root_timer, &mut in_flight).await;
    follow_configuration(
      origin,
      &shards,
      local,
      budget,
      &mut owner_acceptor,
      &mut in_flight,
    )
    .await;
    for shard in shards.iter().copied() {
      run_record_period(
        origin,
        shard,
        local,
        budget,
        &mut owner_acceptor,
        &mut in_flight,
      )
      .await;
    }
    // Drive each takeover this node owes a confirmation of: one batched phase-one round per retired host
    // across its recovery neighbourhoods, adopting each object once its cohorts have promised and confirming
    // the share once done (§4.8 "Promotion and takeover"; the takeover module). The holds and the rounds are
    // this node's (they arrive over this shard's sessions), so this runs here.
    adopt_and_serve(origin, local, budget, &mut in_flight).await;
    pace_running_merges(
      origin,
      &shards,
      local,
      budget,
      &mut owner_acceptor,
      &mut in_flight,
    )
    .await;
  }
}

/// Waits out the coordinator's period ([`HEARTBEAT_NS`]), running the merge record plane at once each time an owner
/// shard nudges it (§4.16 "Commit"; `merge_service::nudge_record_plane`): a version enqueued, or an input or record of
/// one newly acknowledged. A submit at `f > 0` is answered when its record commits, so its answer waits for the
/// exchange rather than the period; the council, the root, the seals and the heads keep the period. The merge plane
/// runs here, on the coordinator, because the owner's acceptor and the holders' sessions are the coordinator's.
/// Before, each version's inputs were put at one period and its record shipped at the next: one submit at a time on an
/// idle three-node fleet at `f = 1` took 208 to 647 ms (2026-11-06, the bug record of that day, "a submit at f = 1
/// waited for the coordinator's period").
async fn pace_running_merges(
  origin: u16,
  shards: &[u16],
  local: HostId,
  budget: CommitBudget,
  owner_acceptor: &mut Acceptor,
  in_flight: &mut Vec<Dispatch>,
) {
  let period_ends = slates_rt::futures::now_ns().saturating_add(HEARTBEAT_NS);
  loop {
    let remaining = period_ends.saturating_sub(slates_rt::futures::now_ns());
    if remaining == 0 {
      return;
    }
    match slates_rt::futures::within(remaining, crate::merge_service::nudged()).await {
      Ok(Some(())) => {
        in_flight.retain_mut(|dispatch| !dispatch.settle());
        for shard in shards.iter().copied() {
          crate::merge_service::run_merge_period(
            origin,
            shard,
            local,
            budget,
            owner_acceptor,
            in_flight,
          )
          .await;
        }
      }
      Ok(None) => return,
      // The timer refused: the period's own pace counts and reports it, and the loop stops there as before.
      Err(_) => {
        crate::daemon::pace(remaining).await;
        return;
      }
    }
  }
}

/// One period of following the council's configuration on the coordinator (§4.8, D-14): install what the
/// council has agreed into this node's placement view — taking over any object whose owner it retired —, fan
/// the committed configurations to every other shard, report this node's neighbourhood change placed once it
/// is, and keep the owner's hold writing under the current generation.
async fn follow_configuration(
  origin: u16,
  shards: &[u16],
  local: HostId,
  budget: CommitBudget,
  owner_acceptor: &mut Acceptor,
  in_flight: &mut Vec<Dispatch>,
) {
  // Install the configuration the council has agreed into this node's placement view, and take over any
  // object whose owner the council has now retired (§4.8, D-14 — the council is the authority; a departed
  // owner's objects that rendezvous first to this node over the new neighbourhood are owed a phase-one
  // recovery, driven below).
  sync_config_from_council(local);
  // The install reaches the held register copies by a walk in turns (A-398): begun here once the version is installed,
  // and begun again where it stands if its task could not run.
  drive_install_walk(local);
  // Fan the committed placement and root configurations out to every other shard, every period, so a client
  // reads the same placement and cross-region home wherever it lands (§4.8 "Lookup", D-7). Re-fanned every
  // period so a dispatch a full control channel refused self-heals; the receiving install/adopt are
  // version-gated, so an unchanged configuration is a no-op there (see `fan_configs_to_shards`).
  fan_configs_to_shards(origin, shards);
  // Windows homed by identity in the region (A-724 piece 6): at a new configuration, offered to their homes and let go
  // here once every home holds them, a turn a period (`vcs_store::homes::heal_region`).
  crate::vcs_store::homes::heal_region(origin, shards, budget).await;
  // Credentials as fleet state (A-764): at a new configuration, every credential this node holds offered to every other
  // member, so one that joined or came back with nothing holds what the region holds (`credentials::heal_region`).
  crate::credentials::heal_region(origin, shards, budget).await;
  // Planned moves (A-291): each move this node's shards began is reported until the switch, routed to its new owner
  // from then on, and released once adopted; a destroyed moved volume's exception is reported forgotten.
  crate::moves::drive_outgoing(origin, local, owner_acceptor, budget, in_flight).await;
  crate::moves::report_forgotten(local, budget, in_flight).await;
  // Report this node's neighbourhood change placed once every shard holds all it owns at `f + 1` of each
  // cohort of its current placement, so the council retires the old set (§4.8 "Neighbourhood changes").
  report_settlement(origin, shards, local, budget, in_flight).await;
  // Keep the owner's hold writing under the current configuration generation: the version advances on
  // every join or retirement (`FleetNode::observe` keeps the node's own acceptor in step the same way), and
  // a record under a stale generation is refused `ForeignGeneration` by the owner's own hold — so without
  // this, no head provisioned after a membership change could ever place. Never refused: the version only
  // advances, and `install_authority` accepts an equal-or-higher generation.
  if let Some(authority) = owner_authority(local) {
    let _ = owner_acceptor.install_authority(authority);
  }
}

/// Advances `shard`'s seal jobs of `track` one bounded slice each on the owner shard and runs the content rounds of
/// those whose archive is complete, keeping each round's dispatch for its stragglers. The walk over the shard's volumes
/// runs in turns of about a step quantum each ([`seal_walk_slice`]), each a turn of the shard of its own, the control
/// shard's too ([`crate::xshard::Turns`]), each turn's puts dispatched before the next turn, so the shard serves its
/// clients between them. Whether the shard holds any job of the track at the walk's end.
async fn put_track_content(
  origin: u16,
  shard: u16,
  local: HostId,
  budget: CommitBudget,
  track: Track,
  in_flight: &mut Vec<Dispatch>,
) -> bool {
  let mut turns = crate::xshard::Turns::new(origin, shard);
  let mut after = None;
  let mut opening = true;
  loop {
    let first = std::mem::take(&mut opening);
    let (work, jobs, next) = turns
      .call(
        move |s| {
          let (work, next) = seal_walk_slice(s, local, (track, first), after, budget);
          (work, !seal_jobs(s, track).is_empty(), next)
        },
        HEARTBEAT_NS,
      )
      .await
      .unwrap_or_default();
    for item in work {
      if let Some(dispatch) = put_seal_content(origin, local, item).await {
        in_flight.push(dispatch);
      }
    }
    match next {
      Some(last) => after = Some(last),
      None => return jobs,
    }
  }
}

/// One period of the record plane for the volumes `shard` owns, in the order the design's placement rule
/// requires: seals advance and their content is put (content places first), then the heads naming placed
/// content ship, then the seals whose content and head both placed are recorded durably. The seal walk,
/// the head values and every recording run **on the owner shard** (its volumes, database partition and
/// configuration copy live there); the archives and heads move to this shard for the dispatch by value.
/// A shard that does not answer within a period is skipped this period (its work is retried next).
async fn run_record_period(
  origin: u16,
  shard: u16,
  local: HostId,
  budget: CommitBudget,
  owner_acceptor: &mut Acceptor,
  in_flight: &mut Vec<Dispatch>,
) {
  // Advance the shard's seals one bounded slice each and put the completed archives' content to their
  // candidates (§4.10) — content places before the head that names it ships — then the shipments of placed heads
  // to the mirror region (§4.10 "Mirroring across regions").
  put_track_content(origin, shard, local, budget, Track::Home, in_flight).await;
  let mirroring = put_track_content(origin, shard, local, budget, Track::Mirror, in_flight).await;
  // A mirror shipment's head and catalog records, once its content is held there (`crate::mirror`); no call at all
  // while the shard ships nothing to a mirror.
  if mirroring {
    crate::mirror::ship(origin, shard, local, budget, in_flight).await;
  }
  deliver_pairs(origin, shard, local, budget).await;
  ship_shard_heads(origin, shard, local, budget, owner_acceptor, in_flight).await;
  // Live volumes' entries a holder still lacks (A-200): writes ship at their barrier; this catches up a lost message or
  // a holder that was away.
  crate::live_ship::period(origin, shard).await;
  // Record durably each seal whose content and head have both placed.
  crate::xshard::run_on_counted(origin, shard, record_placed_seals);
  crate::xshard::run_on_counted(origin, shard, record_mirrored_seals);
  retire_shard_tombstones(origin, shard, local, owner_acceptor).await;
  // The greens' merge records (§4.16 "Commit"): each green's lowest pending version — its inputs put
  // while unplaced, its record shipped in order once they are.
  crate::merge_service::run_merge_period(origin, shard, local, budget, owner_acceptor, in_flight)
    .await;
}

/// Delivers `shard`'s pair key to each host of its neighbourhood it has not yet delivered one to in this daemon's life
/// (A-92 piece 4b): the host's recipient public key asked over its authenticated session (and kept), the shard's
/// pair key made or found and wrapped to it, sent, and marked delivered on the host's acceptance. A pair is named by
/// the host's stable anchor, never its member id, which a restart of its daemon moves: so a candidate's restart keeps
/// its pair, and the records stay one per (owner shard, candidate). Once per pair per daemon life; a candidate accepts
/// a repeat of the key it holds, so a restart's redelivery changes nothing there.
async fn deliver_pairs(origin: u16, shard: u16, local: HostId, budget: CommitBudget) {
  if state::with_state(|s| s.injected_pair_withhold).unwrap_or(false) {
    return;
  }
  let Some((members, delivered)) = call_within(
    origin,
    shard,
    move |s| {
      s.seal_root.as_ref()?;
      // The home neighbourhood and the mirror neighbourhood alike: a mirror holder must be able to open a promoted
      // volume's content, so a head's sealing wraps the lineage key for it too (`docs/wip/mirroring.md` decision 4).
      let mut members: Vec<HostId> = s
        .fleet
        .configuration()
        .neighbourhood
        .iter()
        .chain(s.mirror_neighbourhood.iter())
        .copied()
        .filter(|host| *host != local)
        .collect();
      members.sort_unstable();
      members.dedup();
      Some((members, s.pairs_delivered.clone()))
    },
    HEARTBEAT_NS,
  )
  .await
  .flatten() else {
    return;
  };
  // Each neighbour by its stable anchor, from the identities this control shard has learned.
  let anchors: Vec<(HostId, u64)> = state::with_state(|s| {
    members
      .iter()
      .filter_map(|member| {
        s.learned_members
          .iter()
          .find(|(_, learned)| learned.host == *member)
          .map(|(anchor, _)| (*member, anchor.0))
      })
      .collect()
  })
  .unwrap_or_default();
  // A pair is owed to a neighbour this shard has not delivered one to, or delivered one to another incarnation of: a
  // node back at its anchor with fresh RAM holds none of the keys it was delivered, and one marked delivered by anchor
  // alone was never delivered again, so the replacement could not open this node's heads as their successor (the same
  // key again is accepted by a node that kept it, so a redelivery to a warm restart changes nothing).
  // Test support: the deliveries to one anchor made to fail (`Daemon::inject_pair_withhold_to`), as a delivery whose
  // recipient answer never comes back fails each period, while the others go on and nothing delivered is forgotten.
  let withheld_to = state::with_state(|s| s.injected_pair_withhold_to).flatten();
  let owed: Vec<(HostId, u64)> = anchors
    .iter()
    .copied()
    .filter(|(member, anchor)| delivered.get(anchor) != Some(member))
    .filter(|(_, anchor)| Some(*anchor) != withheld_to)
    .collect();
  // The owner shard learns every neighbour's anchor, so it can tell which candidates a head's sealing covers.
  let resolved: std::collections::BTreeMap<HostId, u64> = anchors.into_iter().collect();
  if call_within(
    origin,
    shard,
    move |s| s.member_anchors = resolved,
    HEARTBEAT_NS,
  )
  .await
  .is_none()
  {
    count_refusal(MEMBER_ANCHORS_UNSENT);
  }
  for (member, anchor) in owed {
    let Some(public) = peer_recipient(member, anchor, budget).await else {
      // The neighbour's recipient key did not come back this period: its pair is owed again next period, counted.
      count_refusal(PAIR_RECIPIENT_UNKNOWN);
      continue;
    };
    let delivery = call_within(
      origin,
      shard,
      move |s| {
        crate::seal_keys::pair_for_delivery(s, anchor, &public)
          .ok()
          .map(|(id, record)| (s.partition, id, record))
      },
      HEARTBEAT_NS,
    )
    .await
    .flatten();
    let Some((partition, id, record)) = delivery else {
      count_refusal(PAIR_UNDELIVERED);
      count_refusal(PAIR_UNBUILT);
      continue;
    };
    let mut request = Vec::with_capacity(
      size_of::<u16>()
        .saturating_add(id.len())
        .saturating_add(record.len()),
    );
    request.extend_from_slice(&partition.to_le_bytes());
    request.extend_from_slice(&id);
    request.extend_from_slice(&record);
    let accepted = send_pair(member, &request, budget).await;
    if accepted.as_deref() == Some(&[PAIR_ACCEPTED][..]) {
      let _ = call_within(
        origin,
        shard,
        move |s| {
          if s.pairs_delivered.len() < s.config.fleet_peer_capacity.max(1)
            || s.pairs_delivered.contains_key(&anchor)
          {
            s.pairs_delivered.insert(anchor, member);
          }
        },
        HEARTBEAT_NS,
      )
      .await;
    } else {
      count_refusal(PAIR_UNDELIVERED);
    }
  }
}

/// One pair key delivery to `member` over its record session, within the round's budget: the member's reply, or `None`
/// with the way it failed counted (no free session to the member, or no answer within the budget).
async fn send_pair(member: HostId, request: &[u8], budget: CommitBudget) -> Option<Vec<u8>> {
  let mut sessions = take_sessions("fleet.lent_to.deliver_pairs", |candidate| {
    candidate == member
  });
  let taken = sessions.pop();
  return_sessions(sessions);
  let Some((holder, endpoint)) = taken else {
    count_refusal(PAIR_NO_SESSION);
    return None;
  };
  let (reply, endpoint) = request_within(
    endpoint,
    PAIR_KEY_STREAM,
    Priority::Metadata,
    request,
    budget.max_deadline_ns(),
  )
  .await;
  return_sessions(vec![(holder, endpoint)]);
  if reply.bytes.is_empty() {
    count_refusal(PAIR_UNANSWERED);
    return None;
  }
  Some(reply.bytes)
}

/// Counter: a volume's lineage key handed to the volume's successor (A-295), on the candidate that handed it.
const LINEAGE_HANDED: &str = "fleet.seal.lineage_handed";
/// Counter: a request for a volume's key whose bytes are not an object, a key id and a recipient key
/// ([`lineage_request`]).
const LINEAGE_MALFORMED: &str = "fleet.seal.lineage_refused.malformed";
/// Counter: a request for a volume's key from a node that does not act for the volume here ([`acts_for`]).
const LINEAGE_NOT_ACTING: &str = "fleet.seal.lineage_refused.not_acting";
/// Counter: a request for a volume's key this node cannot open from its own copies: it holds no sealing of that key (no
/// live log, no head naming it), or no entry or pair of its own for one.
const LINEAGE_NOT_HELD: &str = "fleet.seal.lineage_refused.not_held";
/// Counter: on a successor, a host asked for the volume's key that handed none (it refused, or did not answer).
const LINEAGE_UNHANDED: &str = "fleet.seal.lineage_unhanded";
/// Counter: on a successor, a key handed over that did not open the volume's naming record: refused, never kept.
const LINEAGE_UNCHECKED: &str = "fleet.seal.lineage_unchecked";
/// Counter: on a successor with no recipient key of its own (sealing unavailable here), no fellow candidate was asked.
const LINEAGE_UNASKED: &str = "fleet.seal.lineage_unasked";
/// Format: a lineage key's id (hyper-seal's `KeyId`), as a head's sealing names it.
const LINEAGE_ID_BYTES: usize = 16;

/// A request on [`LINEAGE_STREAM`]: `object`, the id of the lineage key asked for, and the asker's recipient public key
/// as the recipient stream answers it (`seal_keys::node_recipient_public`), the key the answer is wrapped to.
fn lineage_request_bytes(object: ObjectId, lineage: [u8; 16], public: &[u8]) -> Vec<u8> {
  let mut request = Vec::with_capacity(
    slates_db::register::OBJECT_BYTES
      .saturating_add(LINEAGE_ID_BYTES)
      .saturating_add(public.len()),
  );
  request.extend_from_slice(&object.0);
  request.extend_from_slice(&lineage);
  request.extend_from_slice(public);
  request
}

/// A request on [`LINEAGE_STREAM`] read back: the object, the lineage key's id and the asker's recipient key, or `None`
/// when the bytes are not those three (a recipient key that does not decode among them).
fn lineage_request(
  request: &[u8],
) -> Option<(ObjectId, [u8; 16], hyper_seal::recipient::RecipientPublic)> {
  let (object, rest) = request.split_at_checked(slates_db::register::OBJECT_BYTES)?;
  let (lineage, public) = rest.split_at_checked(LINEAGE_ID_BYTES)?;
  Some((
    ObjectId(object.try_into().ok()?),
    lineage.try_into().ok()?,
    crate::seal_keys::decode_public(public)?,
  ))
}

/// Whether `asker` acts for `object` in this node's view (A-295), `owner` being the owner this node's held record names
/// (the object's creator while it holds none): `asker` is that owner and a member (a successor whose adoption has
/// reached this holder), or the owner is retired and the configuration's line of retired owners takes the object to
/// `asker` (`RegionalConfiguration::lineage`). Only such a node is handed the volume's key; as the owner's successor it is
/// a candidate the owner would have delivered a pair to, so no node outside the volume's candidates learns the key.
fn acts_for(
  regional: &slates_db::register::RegionalConfiguration,
  owner: HostId,
  asker: HostId,
  object: ObjectId,
) -> bool {
  if owner == asker {
    return regional.members.contains(&asker);
  }
  !regional.members.contains(&owner)
    && matches!(
      regional.lineage(owner, object),
      slates_db::register::Lineage::Successor { successor, .. } if successor == asker
    )
}

/// This node's own copies of the sealing of `object` that wrap the lineage key `lineage`: its held live log's whole entry
/// first, then each head it holds naming that key, newest first.
fn held_sealings_of(
  s: &ShardState,
  object: ObjectId,
  lineage: [u8; 16],
) -> Vec<crate::head::HeadSealing> {
  let mut sealings: Vec<crate::head::HeadSealing> = crate::live_ship::held_sealing(s, object)
    .into_iter()
    .collect();
  if let Some(acceptor) = s.holder_records.get(&object) {
    let mut heads: Vec<crate::head::HeadSealing> = acceptor
      .values(object)
      .filter_map(crate::head::HeadValue::from_record_bytes)
      .filter_map(|head| head.sealing)
      .collect();
    heads.reverse();
    sealings.extend(heads);
  }
  sealings.retain(|sealing| sealing.lineage == lineage);
  sealings
}

/// A successor's request for a volume's lineage key (A-295), on this node's control shard, from the member `peer_host`
/// the session's authenticated certificate resolves to: it must act for the volume here ([`acts_for`]); the key comes
/// from this node's own copies of the volume's sealing ([`held_sealings_of`]), never from the request, opened under the
/// pair the owner delivered to this node and wrapped to the recipient key the request carries. That key needs no other
/// check: only the authenticated asker sent it, and a key it does not hold the other half of opens nothing for it.
/// Carried rather than kept from a pair delivery, since this node delivers pairs only to its own neighbourhood, which
/// in a fleet wider than one need not hold the asker. Empty when refused, counted by why.
fn serve_lineage(s: &mut ShardState, peer_host: HostId, request: &[u8]) -> Vec<u8> {
  let Some((object, lineage, public)) = lineage_request(request) else {
    count_refusal_in(s, LINEAGE_MALFORMED);
    return Vec::new();
  };
  let owner = s
    .holder_records
    .get(&object)
    .map_or_else(|| object.creator(), slates_db::register::Acceptor::owner);
  if !acts_for(s.council.configuration(), owner, peer_host, object) {
    count_refusal_in(s, LINEAGE_NOT_ACTING);
    return Vec::new();
  }
  let handed = held_sealings_of(s, object, lineage)
    .iter()
    .find_map(|sealing| crate::seal_keys::hand_lineage(s, sealing, &public));
  match handed {
    Some(delivery) => {
      count_refusal_in(s, LINEAGE_HANDED);
      delivery
    }
    None => {
      count_refusal_in(s, LINEAGE_NOT_HELD);
      Vec::new()
    }
  }
}

/// Asks `hosts` in turn for `object`'s lineage key as its successor (A-295), when this node cannot open its own entry in
/// the volume's `sealing`: the first key handed that checks against the sealing (`seal_keys::accept_handed_lineage`) is
/// returned, wrapped under this node's root. `None` when none handed one this attempt; a refusal is counted where it was
/// made, and here a host that handed none or a key that did not check.
pub(crate) async fn ask_handed_lineage(
  object: ObjectId,
  sealing: &crate::head::HeadSealing,
  hosts: &[HostId],
) -> Option<([u8; 16], Vec<u8>)> {
  let public =
    state::with_state(|s| crate::seal_keys::node_recipient_public(s)).unwrap_or_default();
  if public.is_empty() {
    count_refusal(LINEAGE_UNASKED);
    return None;
  }
  let request = lineage_request_bytes(object, sealing.lineage, &public);
  for host in hosts.iter().copied() {
    let Some(delivery) = request_peer(host, LINEAGE_STREAM, &request, live_budget()).await else {
      count_refusal(LINEAGE_UNHANDED);
      continue;
    };
    match state::with_state(|s| crate::seal_keys::accept_handed_lineage(s, sealing, &delivery)) {
      Some(Ok(key)) => return Some(key),
      _ => {
        count_refusal(LINEAGE_UNCHECKED);
      }
    }
  }
  None
}

/// Counter: the owner shard did not answer this period's neighbour anchors (A-92 piece 4c); sent again next period,
/// and until then a sealed head waits for the candidates it cannot resolve.
/// Format: a counter name in the daemon's status report.
const MEMBER_ANCHORS_UNSENT: &str = "fleet.seal.member_anchors_unsent";

/// Counter: a pair key that could not be made, wrapped or delivered this period (A-92 piece 4b); tried again next.
/// Format: a counter name in the daemon's status report.
const PAIR_UNDELIVERED: &str = "fleet.seal.pair_undelivered";

/// Counters: which way a pair delivery failed (each also in [`PAIR_UNDELIVERED`]): the owner shard did not build it, no
/// session to the member was free, or the member did not answer within the budget (a refusal is the member's own
/// `fleet.seal.pair_refused`).
const PAIR_UNBUILT: &str = "fleet.seal.pair_unbuilt";
/// See [`PAIR_UNBUILT`].
const PAIR_NO_SESSION: &str = "fleet.seal.pair_no_session";
/// See [`PAIR_UNBUILT`].
const PAIR_UNANSWERED: &str = "fleet.seal.pair_unanswered";

/// Counter: a neighbour's recipient key that did not come back in a period, so its pair was not built (`deliver_pairs`).
const PAIR_RECIPIENT_UNKNOWN: &str = "fleet.seal.pair_recipient_unknown";

/// The recipient public key of the node with stable `anchor`, reached as `member` (A-92 piece 4b): kept from an earlier
/// answer, or asked over its authenticated record session now and kept by anchor (bounded by the fleet's peer
/// capacity). `None` when no session to it is reachable or it answers none (sealing is unavailable there).
async fn peer_recipient(
  member: HostId,
  anchor: u64,
  budget: CommitBudget,
) -> Option<hyper_seal::recipient::RecipientPublic> {
  if let Some(Some(known)) =
    state::with_state(|s| kept_recipient(&s.peer_recipients, anchor, member))
  {
    return Some(known);
  }
  let answer = request_peer(member, RECIPIENT_STREAM, &[], budget).await?;
  let public = crate::seal_keys::decode_public(&answer)?;
  let kept = public.clone();
  state::with_state(|s| {
    let capacity = s.config.fleet_peer_capacity.max(1);
    keep_recipient(&mut s.peer_recipients, capacity, anchor, member, kept);
  });
  Some(public)
}

/// The recipient key kept for the node at `anchor`, when it was learned from `member`: a node back at its anchor with
/// fresh RAM (the same certificate, so the same anchor) has a new member id and a recipient of its own, so a key learned
/// from its predecessor is not its key. Kept by anchor alone, the predecessor's key was used for the replacement for
/// good: every pair wrapped to it was refused (1,583 of 1,583, diag-live-durability-72), so the replacement could open
/// no key it was sent (docs/bugs/2026-11-09-a-replacements-pairs-were-wrapped-to-its-destroyed-predecessors-key.md).
fn kept_recipient(
  kept: &std::collections::BTreeMap<u64, (HostId, hyper_seal::recipient::RecipientPublic)>,
  anchor: u64,
  member: HostId,
) -> Option<hyper_seal::recipient::RecipientPublic> {
  kept
    .get(&anchor)
    .filter(|(learned_from, _)| *learned_from == member)
    .map(|(_, public)| public.clone())
}

/// Keeps `public`, learned from `member` at `anchor`: replacing what the anchor's earlier member gave, or a new anchor
/// while fewer than `capacity` are kept (the bound `pairs_delivered` keeps too).
fn keep_recipient(
  kept: &mut std::collections::BTreeMap<u64, (HostId, hyper_seal::recipient::RecipientPublic)>,
  capacity: usize,
  anchor: u64,
  member: HostId,
  public: hyper_seal::recipient::RecipientPublic,
) {
  if kept.len() < capacity || kept.contains_key(&anchor) {
    kept.insert(anchor, (member, public));
  }
}

/// One request to `host` over its record session, within the round's budget: the reply, or `None` when no session to
/// it is reachable or the exchange failed (an empty reply is a refusal).
pub(crate) async fn request_peer(
  host: HostId,
  stream: u64,
  request: &[u8],
  budget: CommitBudget,
) -> Option<Vec<u8>> {
  let mut sessions = take_sessions("fleet.lent_to.request_peer", |candidate| candidate == host);
  let taken = sessions.pop();
  return_sessions(sessions);
  let (holder, endpoint) = taken?;
  let (reply, endpoint) = request_within(
    endpoint,
    stream,
    Priority::Metadata,
    request,
    budget.max_deadline_ns(),
  )
  .await;
  return_sessions(vec![(holder, endpoint)]);
  (!reply.bytes.is_empty()).then_some(reply.bytes)
}

/// A pair key an owner's shard delivered (A-92 piece 4b): its partition, the key's id and the record wrapped to this node's
/// recipient, accepted from the node with stable anchor `peer` (the session's authenticated certificate, never a field of
/// the request) and recorded under that anchor.
fn accept_pair_delivery(s: &mut ShardState, peer: HostId, request: &[u8]) -> Vec<u8> {
  count_refusal_in(s, PAIR_SERVED);
  // A request too short for its fields is answered empty, counted: the sender sees no answer, and without the count
  // the receiver's side of a delivery that fails would not show.
  let parsed = request
    .split_at_checked(size_of::<u16>())
    .and_then(|(partition, rest)| {
      let (id, delivery) = rest.split_at_checked(PAIR_ID_BYTES)?;
      let partition = u16::from_le_bytes(partition.try_into().ok()?);
      let id: [u8; PAIR_ID_BYTES] = id.try_into().ok()?;
      Some((partition, id, delivery))
    });
  let Some((partition, id, delivery)) = parsed else {
    count_refusal_in(s, PAIR_MALFORMED);
    return Vec::new();
  };
  match crate::seal_keys::accept_pair(s, (peer.0, partition), id, delivery) {
    Ok(()) => vec![PAIR_ACCEPTED],
    Err(_) => {
      count_refusal_in(s, PAIR_REFUSED);
      Vec::new()
    }
  }
}

/// Format: a pair key's id (hyper-seal's `KeyId`).
const PAIR_ID_BYTES: usize = 16;
/// Counter: a pair key delivery this node refused (malformed, not for its recipient, or another key for a recorded pair).
/// Format: a counter name in the daemon's status report.
const PAIR_REFUSED: &str = "fleet.seal.pair_refused";
/// Counter: pair key deliveries that reached this receiver's serve path (the receiving side of `pair_unanswered`).
const PAIR_SERVED: &str = "fleet.seal.pair_served";
/// Counter: a pair key delivery too short for its partition and key id, answered empty.
const PAIR_MALFORMED: &str = "fleet.seal.pair_malformed";

/// Ships each of `shard`'s unplaced heads — catalogs, heads and tombstones — to all its candidate holders at
/// once, committed at `f + 1`.
async fn ship_shard_heads(
  origin: u16,
  shard: u16,
  local: HostId,
  budget: CommitBudget,
  owner_acceptor: &mut Acceptor,
  in_flight: &mut Vec<Dispatch>,
) {
  let work = call_within(
    origin,
    shard,
    move |s| {
      // The state logs' records the partition handed on since the last pass become parts owed (A-636 part 2b).
      crate::state_log::take_made(s);
      unplaced_heads(s, local)
    },
    HEARTBEAT_NS,
  )
  .await
  .unwrap_or_default();
  for head in work {
    if let Some(dispatch) = ship_head(origin, owner_acceptor, local, &head, budget)
      .await
      .dispatch
    {
      in_flight.push(dispatch);
    }
  }
}

/// Retires each destroyed volume's tombstone on `shard` that every candidate holds (AUD-29-43); the owner's
/// acceptor forgets the volume's and its catalog's registers with it.
async fn retire_shard_tombstones(
  origin: u16,
  shard: u16,
  local: HostId,
  owner_acceptor: &mut Acceptor,
) {
  let retired = call_within(
    origin,
    shard,
    move |s| retire_done_tombstones(s, local),
    HEARTBEAT_NS,
  )
  .await
  .unwrap_or_default();
  for object in retired {
    owner_acceptor.forget_object(object);
    owner_acceptor.forget_object(object.catalog());
    // A destroyed volume this node owned under a moved-object exception: the exception goes with it (A-291).
    let _ = state::with_state_counted(|s| crate::moves::note_destroyed(s, object, local));
    // The routing forgets the volume too, as a holder's does when it accepts the retirement (`tombstone::on_held`).
    // A successor that took a destroyed volume over tracks itself its owner (`takeover::record_adoption`); kept after
    // the retirement, it answered every verb on the volume `Overloaded` as an owned volume not yet materialized, for
    // good, while the other survivor answered `NotFound` (6 of 6 runs, 2026-11-06, `crates/cli/tests/live_chaos.rs`:
    // an unanswered destroy, then its owner's loss). On every shard, since the takeover tracked it on every shard.
    // A successor's own copies go too (A-288): the takeover kept its acceptor of each register in `holder_records` and
    // this shard's placement of an adopted catalog, and nothing drops them once every candidate has dropped its own, so
    // a retired work's route, or a destroyed taken-over volume, would stay held here for good.
    let _ = state::with_state_counted(|s| {
      for register in [object, object.catalog()] {
        crate::takeover::forget_owner_on_every_shard(s, register);
        s.departed_owners.remove(&register);
        s.holder_records.remove(&register);
        s.placed_heads.remove(&register);
      }
    });
  }
}

/// Format: nanoseconds per second, for the archive header's creation time in Unix seconds.
const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// Commits one head to the candidate holders that have not yet acknowledged it (those with a live session in
/// the shard state), through the owner's own acceptor, at `f + 1`. The holders' sessions are borrowed for the
/// commit and returned whatever the outcome (`request_within`; stragglers later). Every acknowledgement the
/// round collected is recorded **whatever the round's outcome** ([`record_acks_in`]): once a head is placed by
/// an earlier round, a later round re-shipping to a straggler is short of quorum on its own — `commit_record`
/// counts only its own round — and discarding its acknowledgements re-shipped that straggler forever at
/// `f > 1`. Returns the dispatch to settle, or `None` if no holder could be reached this period.
async fn ship_head(
  origin: u16,
  owner_acceptor: &mut Acceptor,
  local: HostId,
  head: &Head,
  budget: CommitBudget,
) -> HeadShipped {
  let holders = take_sessions("fleet.lent_to.ship_head", |host| {
    head.shape.candidates.contains(&host) && !head.acked.contains(&host)
  });
  let counters = ship_counters(head.object.class());
  if holders.is_empty() {
    count_refusal(counters.no_session);
    // No live session to a candidate that still needs the head; retry next period.
    return HeadShipped {
      dispatch: None,
      recorded: false,
    };
  }
  if head.live {
    // The candidates are asked now, so a held durable reply on the live volume may hold them to an answer a window
    // hence (A-301). Noted on the volume's shard before the round is sent: that shard runs what this one sends it in
    // order, so the note is there before the round's own acknowledgements ([`record_acks_in`], sent after the round).
    let object = head.object;
    crate::xshard::run_on_counted(origin, head.shard, move |s| {
      crate::live_ship::asked(s, object);
    });
  }
  let taken: Vec<HostId> = holders.iter().map(|(host, _)| *host).collect();
  let committed = commit_record(
    local,
    owner_acceptor,
    &head.shape,
    &head.record,
    head.quorum,
    holders,
    budget,
  )
  .await;
  // The reactive piggyback, stale-sender direction (§4.8 "a receiver with a newer configuration refuses
  // ConfigurationStale{version}; refreshes consume the operation's bounded retry budget"): a holder on a
  // newer configuration refused this record and named its version. Flag a refresh — this owner is behind —
  // so the coordinator fetches the committed configuration next period; the head stays unplaced and re-ships
  // under the refreshed generation. Gated on the version being strictly newer than what this node now holds.
  if let Some(version) = committed.stale_version {
    let _ = state::with_state_counted(|s| {
      if version > s.council.configuration().version {
        s.config_refresh_wanted = true;
      }
    });
  }
  count_refusal(ship_outcome_counter(
    counters,
    &committed.outcome,
    head.quorum,
  ));
  let dispatch = Dispatch::new(
    taken,
    &committed.reusable,
    committed.stragglers,
    LateReplies::Discard,
  );
  return_sessions(committed.reusable);
  let placement = match committed.outcome {
    Ok(placement) => placement,
    Err(ClusterError::Uncertain { placement } | ClusterError::NotPlaced { placement }) => placement,
    // A runtime refusal dispatched nothing; retry next period.
    Err(_) => {
      return HeadShipped {
        dispatch: Some(dispatch),
        recorded: false,
      };
    }
  };
  // The owner's own acceptor keeps a single-value register at its newest position only (AUD-29-43).
  if crate::catalog::is_single_value(&head.record.value) {
    owner_acceptor.compact_below(head.object, head.record.sequence);
  }
  // The placement is the owner shard's fact: record it there.
  let (object, sequence, epoch) = (head.object, head.record.sequence, head.record.epoch);
  crate::xshard::run_on_counted(origin, head.shard, move |s| {
    record_acks_in(s, object, sequence, epoch, placement);
  });
  HeadShipped {
    dispatch: Some(dispatch),
    recorded: true,
  }
}

/// The counters one register class's shipments are counted under: a volume's own register (its head or tombstone), its
/// catalog, or a repository's state log (A-636 part 2b), each its own, so a state log's part is never counted as its
/// repository's head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ShipCounters {
  /// Committed at `f + 1` by one ship.
  placed: &'static str,
  /// Acknowledged by fewer than `f + 1` candidates.
  short: &'static str,
  /// Its outcome uncertain (the dispatch's budget ran out).
  uncertain: &'static str,
  /// Refused by the runtime before anything was dispatched.
  failed: &'static str,
  /// Owed to candidates none of which had a session to ride.
  no_session: &'static str,
}

/// The counters of a volume's own register (its head, or its tombstone).
const HEAD_SHIPS: ShipCounters = ShipCounters {
  placed: SHIP_HEAD_PLACED,
  short: SHIP_HEAD_SHORT,
  uncertain: SHIP_HEAD_UNCERTAIN,
  failed: SHIP_HEAD_FAILED,
  no_session: SHIP_HEAD_NO_SESSION,
};

/// The counters of a catalog register.
const CATALOG_SHIPS: ShipCounters = ShipCounters {
  placed: SHIP_CATALOG_PLACED,
  short: SHIP_CATALOG_SHORT,
  uncertain: SHIP_CATALOG_UNCERTAIN,
  failed: SHIP_CATALOG_FAILED,
  no_session: SHIP_CATALOG_NO_SESSION,
};

/// The counters of a state log's part (A-636 part 2b).
const STATE_LOG_SHIPS: ShipCounters = ShipCounters {
  placed: "fleet.ship.state_log.placed",
  short: "fleet.ship.state_log.short",
  uncertain: "fleet.ship.state_log.uncertain",
  failed: "fleet.ship.state_log.failed",
  no_session: "fleet.ship.state_log.no_session",
};

/// The counters a register of `class` ships under, read from its id's class (A-636 part 2a), never from its value.
fn ship_counters(class: RegisterClass) -> ShipCounters {
  match class {
    RegisterClass::Own => HEAD_SHIPS,
    RegisterClass::Catalog => CATALOG_SHIPS,
    RegisterClass::State(_) => STATE_LOG_SHIPS,
  }
}

/// The counter naming one shipment's outcome among `counters`: placed at `f + 1`, acknowledged short of it, uncertain,
/// or refused before anything was dispatched.
fn ship_outcome_counter(
  counters: ShipCounters,
  outcome: &Result<Placement, ClusterError>,
  quorum: Quorum,
) -> &'static str {
  match outcome {
    Ok(placement) if placement.placed(quorum) => counters.placed,
    Ok(_) | Err(ClusterError::NotPlaced { .. }) => counters.short,
    Err(ClusterError::Uncertain { .. }) => counters.uncertain,
    Err(_) => counters.failed,
  }
}

/// Counter: a volume's head (or tombstone) committed at `f + 1` by one ship.
const SHIP_HEAD_PLACED: &str = "fleet.ship.head.placed";
/// Counter: a head ship acknowledged by fewer than `f + 1` candidates.
const SHIP_HEAD_SHORT: &str = "fleet.ship.head.short";
/// Counter: a head ship whose outcome is uncertain (the dispatch's budget ran out).
const SHIP_HEAD_UNCERTAIN: &str = "fleet.ship.head.uncertain";
/// Counter: a head ship refused by the runtime before anything was dispatched.
const SHIP_HEAD_FAILED: &str = "fleet.ship.head.failed";
/// Counter: a head owed to candidates none of which had a session to ride.
const SHIP_HEAD_NO_SESSION: &str = "fleet.ship.head.no_session";
/// Counter: a volume's catalog record committed at `f + 1` by one ship.
const SHIP_CATALOG_PLACED: &str = "fleet.ship.catalog.placed";
/// Counter: a catalog ship acknowledged by fewer than `f + 1` candidates.
const SHIP_CATALOG_SHORT: &str = "fleet.ship.catalog.short";
/// Counter: a catalog ship whose outcome is uncertain.
const SHIP_CATALOG_UNCERTAIN: &str = "fleet.ship.catalog.uncertain";
/// Counter: a catalog ship refused by the runtime before anything was dispatched.
const SHIP_CATALOG_FAILED: &str = "fleet.ship.catalog.failed";
/// Counter: a catalog owed to candidates none of which had a session to ride.
const SHIP_CATALOG_NO_SESSION: &str = "fleet.ship.catalog.no_session";

/// Merges an acknowledging set into the object's recorded placement at `sequence` — never overwriting it
/// within a sequence: the union of every round's acknowledgements is the true region placement, since each
/// holder's acceptance is durable on that holder and binds the same record, so `f + 1` distinct
/// acknowledgements place the head whichever rounds carried them. A newer sequence replaces an older one's
/// placement (the head register's newest position is the head); an older round's acknowledgements are
/// ignored. `unplaced_heads` reads the merged set to ship only to the candidates still missing the head,
/// and the verbs read it for `region_placed`.
fn record_acks_in(
  state: &mut ShardState,
  object: ObjectId,
  sequence: u64,
  epoch: HostEpoch,
  placement: Placement,
) {
  // A state log's part is a ledger position, its candidates' acknowledgements kept per log (A-636 part 2b): never the
  // one sequence a head's placement keeps, and never a live volume's.
  if let RegisterClass::State(_) = object.class() {
    crate::state_log::acknowledged(state, object, sequence, &placement);
    return;
  }
  let entry = state
    .placed_heads
    .entry(object)
    .or_insert_with(|| PlacedHead {
      sequence,
      epoch,
      placement: Placement {
        acked: Vec::new(),
        ..placement.clone()
      },
    });
  if entry.sequence > sequence {
    return; // A stale round for a superseded head.
  }
  if entry.sequence < sequence {
    entry.sequence = sequence;
    entry.placement.acked.clear();
  }
  // The round's candidates and cohorts are the owner's current placement of the head; the acknowledgements
  // of earlier rounds of the same head stay, each a durable hold of the same record.
  entry.placement.candidates = placement.candidates.clone();
  entry.placement.joint = placement.joint.clone();
  entry.epoch = entry.epoch.max(epoch);
  let mut moved = false;
  for host in placement.acked {
    if !entry.placement.acked.contains(&host) {
      entry.placement.acked.push(host);
      moved = true;
    }
  }
  // A placement that gained an acknowledgement moved a live volume's durability forward: its held replies wait on
  // (`slates_bridge_core::durable_bound`).
  if moved {
    crate::live_ship::heard(state, object);
  }
  // A live volume's writes wait for its head's placement as well as their own durability (A-200): answer those it was
  // the last thing they waited for.
  crate::live_ship::resolve(state, object.0);
  // A destroy is answered once its tombstone is placed (`live_ship::defer_destroy`).
  crate::live_ship::resolve_destroys(state, object.0);
  // A live shipper waits for its holders' catalog to be placed before it ships (A-240).
  crate::live_ship::catalog_placed(state, object);
}

/// Borrows out of the shard state the live sessions of the holders satisfying `wanted`, leaving each borrowed
/// entry `None` (out on a dispatch — the link task leaves it alone) until [`return_sessions`] puts it back. A
/// holder with no live session is skipped — the dispatch proceeds with the holders it can reach and the rest
/// are retried next period.
pub(crate) fn take_sessions(
  lent_to: &'static str,
  wanted: impl Fn(HostId) -> bool,
) -> Vec<(HostId, Endpoint)> {
  state::with_state(|s| {
    let mut taken = Vec::new();
    for (host, link) in s.record_sessions.iter_mut() {
      if wanted(*host)
        && let Some(mut endpoint) = link.endpoint.take()
      {
        // The borrow is tagged with the session's own identity, so only this session returns to the slot.
        link.borrowed = endpoint.connection_id().ok();
        link.lent_to = Some(lent_to);
        taken.push((*host, endpoint));
      }
    }
    taken
  })
  .unwrap_or_default()
}

/// Format: nanoseconds per millisecond, for the link's log lines.
const NS_PER_MS: u64 = 1_000_000;
/// The status count of a record session dropped on its return because its peer was silent past the rule
/// ([`return_sessions`]), for the link to dial again.
const LINK_SESSION_JUDGED_DEAD: &str = "fleet.link.session_judged_dead";

/// Format: a wait for a record session out on loan woken by its return or its link's change ([`session_returned`]),
/// the non-vacuity count of the woken wait.
pub(crate) const SESSION_WAIT_WOKEN: &str = "fleet.session_wait.woken";
/// Format: such a wait that met its deadline unwoken.
pub(crate) const SESSION_WAIT_ELAPSED: &str = "fleet.session_wait.elapsed";
/// Format: such waits woken by the session's own return to its link ([`return_sessions`]), the count that tells a wake
/// at the return from one by any other change of the link.
pub(crate) const SESSION_WAIT_RETURNED: &str = "fleet.session_wait.returned";

/// Waits, on the control shard, for a record session to one of `hosts` to come back from its loan, or for its link to
/// change (installed or dropped), for at most `left_ns`: `Some(true)` when such a change woke it, `Some(false)` at the
/// deadline, `None` when no wait could be made (off a shard, as a refused sleep was): the caller ends its wait then, never
/// spins. A borrower that finds a session out waits here for the event and looks again at once; before
/// 2026-11-20 each slept a pace (a timer tick, a tenth of a period, the fleet's poll interval) and looked again, so a
/// session handed back just after a look waited out the rest of the pace (main's sweep after A-697's release bench).
pub(crate) async fn session_returned(hosts: &[HostId], left_ns: u64) -> Option<bool> {
  let woken = match futures::within(
    left_ns,
    SessionChange {
      hosts: hosts.to_vec(),
      token: None,
    },
  )
  .await
  {
    Ok(Some(Some(()))) => true,
    Ok(Some(None)) | Err(_) => return None,
    Ok(None) => false,
  };
  let _ = state::with_state(|s| {
    s.count(
      if woken {
        SESSION_WAIT_WOKEN
      } else {
        SESSION_WAIT_ELAPSED
      },
      1,
    );
  });
  Some(woken)
}

/// The wait of [`session_returned`]: registered with its waker under each of `hosts` at its first poll, on the shard it
/// runs on (the control shard, where the sessions are), and ready once [`sessions_changed`] woke its token; `None` when
/// the shard's state could not be read (off a shard). Dropped, it takes its registrations away, so none outlives it.
struct SessionChange {
  hosts: Vec<HostId>,
  token: Option<u64>,
}

impl std::future::Future for SessionChange {
  type Output = Option<()>;

  fn poll(
    self: std::pin::Pin<&mut Self>,
    cx: &mut std::task::Context<'_>,
  ) -> std::task::Poll<Option<()>> {
    let this = self.get_mut();
    let polled = state::with_state(|s| match this.token {
      Some(token) if s.session_woken.remove(&token) => std::task::Poll::Ready(Some(())),
      Some(_) => std::task::Poll::Pending,
      None => {
        let token = s.next_session_wait;
        s.next_session_wait = token.wrapping_add(1);
        for host in &this.hosts {
          s.session_waiters
            .entry(*host)
            .or_default()
            .push((token, cx.waker().clone()));
        }
        this.token = Some(token);
        std::task::Poll::Pending
      }
    });
    polled.unwrap_or(std::task::Poll::Ready(None))
  }
}

impl Drop for SessionChange {
  fn drop(&mut self) {
    let Some(token) = self.token else {
      return;
    };
    let hosts = std::mem::take(&mut self.hosts);
    let _ = state::with_state(|s| {
      s.session_woken.remove(&token);
      for host in hosts {
        if let Some(waiters) = s.session_waiters.get_mut(&host) {
          waiters.retain(|(waiting, _)| *waiting != token);
          if waiters.is_empty() {
            s.session_waiters.remove(&host);
          }
        }
      }
    });
  }
}

/// Answers every wait for `host`'s record session ([`session_returned`]): its session handed back, or its link installed
/// or dropped. Called wherever `record_sessions` changes for a host. Each wait is woken once, and leaves the lists of the
/// other peers it waited for. Returns how many it woke.
pub(crate) fn sessions_changed(state: &mut ShardState, host: HostId) -> u64 {
  let Some(waiters) = state.session_waiters.remove(&host) else {
    return 0;
  };
  let mut woken = 0u64;
  for (token, waker) in waiters {
    if state.session_woken.insert(token) {
      for others in state.session_waiters.values_mut() {
        others.retain(|(waiting, _)| *waiting != token);
      }
      waker.wake();
      woken = woken.saturating_add(1);
    }
  }
  state
    .session_waiters
    .retain(|_, waiters| !waiters.is_empty());
  woken
}

/// Returns borrowed sessions to the shard state after a dispatch. A slot is refilled only by the session
/// that left it — the entry still there, its session out, and the returning session's connection id the
/// one the borrow was tagged with. Anything else is a late return — a peer retired meanwhile (its entry
/// removed by its link task), a slot the link task re-established since (holding a newer session, or
/// lending it), or a session other than the one borrowed — dropped and counted (`fleet.link.stale_return`),
/// never installed over a newer session. A session its peer reset (A-261) is never put back: its link is dropped, so the
/// link task dials the peer's new process at its next pace rather than lending a dead session to every later borrower
/// (a restarted primary's client node forwarded into one for 25 to 135 s, loadtest-scale 2026-11-08).
///
/// A session whose peer is gone without a word is dropped here too, and its entry with it: the transport's one
/// judgement of a dead session (`Endpoint::peer_silent_past`, fleet-transport.md §5), an ack-eliciting packet
/// unanswered for the session's persistent-congestion duration (A-444: its floor is the session's own probe timeout,
/// where a liveness window stood, which the duration, three initial probe timeouts at least, always passed). It covers a
/// reset lost and a peer that is silent. Judged at the one place every borrower hands the session back, and every
/// borrower's hold is bounded by a derived budget, so a silent session is judged within its window plus one hold: with
/// a deaf session under a 256-client burst the council's replication held it 120 times in 10 s while the forward lane's
/// own check never got a turn (2026-11-09). A starved node can judge a live peer dead: that costs a session replaced,
/// and the forwards on it are answered unanswered and sent again under their own ids, never run twice. A reset is
/// checked first, so it is counted as one.
pub(crate) fn return_sessions(sessions: Vec<(HostId, Endpoint)>) {
  state::with_state(|s| {
    for (host, mut endpoint) in sessions {
      let returned = endpoint.connection_id().ok();
      match s.record_sessions.get_mut(&host) {
        Some(link)
          if link.endpoint.is_none() && link.borrowed == returned && endpoint.peer_reset() =>
        {
          s.record_sessions.remove(&host);
          s.count(LINK_PEER_RESET, 1);
          let dropped = crate::state::WallAndPeriods::now(s);
          s.dead_session_dropped_at.insert(host, dropped);
        }
        // Silent past the session's own persistent-congestion duration (RFC 9002 §7.6.1), floored at its own probe
        // timeout (§6.2.1): the session plane's estimator decides, never a fixed window (A-444).
        Some(link)
          if link.endpoint.is_none()
            && link.borrowed == returned
            && endpoint.peer_silent_past(endpoint.pto()) =>
        {
          s.record_sessions.remove(&host);
          s.count(LINK_SESSION_JUDGED_DEAD, 1);
          // How far past the window the judgment came, so the margin over the duration is on record, not just a count.
          slates_machine::log_line!(
            "slates-server: record session to {host:?} dropped silent after {} ms, its window {} ms",
            endpoint.peer_silence_ns().unwrap_or(0) / NS_PER_MS,
            endpoint
              .persistent_congestion_duration()
              .max(endpoint.pto())
              / NS_PER_MS
          );
          let dropped = crate::state::WallAndPeriods::now(s);
          s.dead_session_dropped_at.insert(host, dropped);
        }
        Some(link) if link.endpoint.is_none() && link.borrowed == returned => {
          link.last_seen = Some(crate::state::RecordLink::seen(host, &endpoint));
          link.endpoint = Some(endpoint);
          link.borrowed = None;
          link.lent_to = None;
        }
        _ => s.count(LINK_STALE_RETURN, 1),
      }
      // Whatever became of it (handed back, dropped, or a stale return), the borrowers waiting for this peer look again.
      let woken = sessions_changed(s, host);
      s.count(SESSION_WAIT_RETURNED, woken);
    }
  });
}

/// Forwards a request to one peer over its record session and returns the reply bytes (§4.8 "Lookup" — a verb
/// a node cannot serve locally is sent to the node that can, over [`FORWARD_STREAM`]). Borrows the peer's
/// session the same way a dispatch does ([`take_sessions`]/[`return_sessions`], so it does not corrupt the
/// coordinator's use), returning it whatever the outcome ([`request_within`]).
///
/// A session that is not there to borrow — out on a coordinator dispatch or a discovery page, or being
/// re-established by its link task — is waited for, paced at the fleet's poll interval
/// (`HEARTBEAT_NS / POLL_PER_PERIOD`), inside the one `deadline_ns` that also bounds the request: the peer
/// is live and its session is out only for a moment. Refusing at the first miss turned that moment into a
/// client-visible `HomedElsewhere` for a write — or its retry — whose owner was serving
/// (docs/bugs/2026-09-25-a-forward-refused-while-the-owners-session-was-out.md). The first miss is counted
/// by where the session was (`fleet.forward.session_out`: the link holds none right now;
/// `fleet.forward.no_session`: no link), and a wait that outlives the deadline once more
/// (`fleet.forward.session_never_returned`). `None` then (the caller refuses); an empty `Some` when the
/// request itself timed out. Called from the control shard, where the record sessions live.
pub(crate) async fn forward_over_leader_session(
  peer: HostId,
  request: Vec<u8>,
  deadline_ns: u64,
) -> Option<Vec<u8>> {
  forward_batch_over_leader_session(
    peer,
    &[request],
    deadline_ns,
    &mut crate::forward_lane::FailoverBound::from_now(),
    &mut crate::forward_lane::Place::default(),
  )
  .await
  .map(|batch| {
    batch
      .replies
      .into_iter()
      .next()
      .flatten()
      .unwrap_or_default()
  })
}

/// [`forward_over_leader_session`] for several requests at once — a read-ahead batch of windows
/// (`RequestBody::ReadWindow`) — all sent together on the borrowed session ([`slates_cluster::requests_within`]).
/// `stall_ns` bounds the wait for the session and, once it is borrowed, any stretch with no progress: a batch that
/// keeps delivering is never cut off for its size. A stretch with no progress is given up once the lane's shard has
/// read what came, or at once past `bound`, the failover bound from the forward's first send, held to the floor of the
/// path to `peer` where that is longer ([`crate::forward_lane`]'s stall, A-315, A-352). Returns the replies in order (`None` for one not answered) with how they
/// streamed ([`slates_cluster::BatchReplies`]), or `None` when the session never came back within the budget. The
/// batch is queued at `place` in the peer's lane: a resend passes the place its first issue recorded there
/// ([`crate::forward_lane::Place`]).
pub(crate) async fn forward_batch_over_leader_session(
  peer: HostId,
  requests: &[Vec<u8>],
  stall_ns: u64,
  bound: &mut crate::forward_lane::FailoverBound,
  place: &mut crate::forward_lane::Place,
) -> Option<slates_cluster::BatchReplies> {
  crate::forward_lane::forward_batch(peer, requests, stall_ns, bound, place).await
}

#[cfg(test)]
mod spawn_label_tests {
  /// A-740's labels through the fleet's loop spawner (w-forming, 2026-11-20; found by use: every fleet loop's long poll
  /// was named by the one `fleet.rs` line inside `spawn_detached`). Do: on a shard, spawn a loop through
  /// `spawn_detached` whose future reads the task the shard's pulse says it is polling. Expect it named by the line of
  /// this test that spawned it. Before, by the line in `spawn_detached` that called `futures::spawn`.
  #[test]
  // The test thread waits off-shard for what the shard's task reports.
  #[allow(clippy::disallowed_methods)]
  fn a_fleet_loops_label_names_the_line_that_spawned_it() {
    /// Shape: how long the test thread waits for the report.
    const REPORT_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
    let daemon = crate::daemon::audit_daemon();
    let (seen, label) = std::sync::mpsc::sync_channel::<(u32, Option<u32>)>(1);
    crate::daemon::observe_first(&daemon, move |state| {
      let home = state.shard;
      let spawned_at = line!() + 1;
      super::spawn_detached(
        async move {
          let named = slates_rt::registry::with_entry(home, |entry| entry.pulse.polling().label)
            .flatten()
            .and_then(|label| match label {
              slates_rt::label::TaskLabel::Site(site) if site.file().ends_with("fleet.rs") => {
                Some(site.line())
              }
              _ => None,
            });
          let _ = seen.try_send((spawned_at, named));
        },
        "test.loop_spawn",
      );
    });
    let (spawned_at, named) = label.recv_timeout(REPORT_WAIT).unwrap();
    assert_eq!(
      named,
      Some(spawned_at),
      "the loop is named by the line that spawned it"
    );
    daemon.stop();
  }
}

#[cfg(test)]
mod lineage_tests {
  use super::{HostId, ObjectId, acts_for, lineage_request, lineage_request_bytes};
  use slates_db::register::{Quorum, RegionalConfiguration};

  /// A-295, the successor's request for a volume's key read back. Do read a request made for an object, a key id and a
  /// fresh recipient's public key, and requests of every other shape: none, one byte short, one byte long, a lone object
  /// id, an object and key id with no recipient key, and the recipient key's mode byte changed. Expect the object, key
  /// id and recipient key back from the first and nothing from the rest.
  #[test]
  fn a_lineage_request_of_any_other_shape_is_refused() {
    let object = ObjectId::new(HostId(9), 3);
    let public = hyper_seal::recipient::Recipient::generate(true)
      .unwrap()
      .public();
    let encoded = crate::seal_keys::encode_public(&public);
    let request = lineage_request_bytes(object, [7; 16], &encoded);
    assert_eq!(
      lineage_request(&request),
      Some((object, [7; 16], public.clone()))
    );
    let short = request
      .get(..request.len().saturating_sub(1))
      .unwrap_or_default();
    let mut long = request.clone();
    long.push(0);
    let unkeyed = lineage_request_bytes(object, [7; 16], &[]);
    let mut remoded = request.clone();
    let mode = request
      .len()
      .checked_sub(hyper_seal::recipient::ECDH_PUBLIC + 1)
      .unwrap();
    remoded[mode] ^= 0xff;
    for refused in [
      &[][..],
      short,
      &long[..],
      &object.0[..],
      &unkeyed[..],
      &remoded[..],
    ] {
      assert_eq!(lineage_request(refused), None, "{} bytes", refused.len());
    }
  }

  /// A-295, who is handed a volume's key. Do form three members, ask for the owner, the two others and an outsider
  /// whether each acts for the owner's object (no held record), then retire the owner and ask again. Expect, while the
  /// owner lives, only the owner; once it is retired, only the member the configuration makes its successor: never the
  /// other survivor, an outsider or the retired owner.
  #[test]
  fn only_the_volumes_acting_owner_is_handed_its_key() {
    let (owner, b, c, outsider) = (HostId(1), HostId(2), HostId(3), HostId(4));
    let object = ObjectId::new(owner, 7);
    let mut regional = RegionalConfiguration::formed(
      vec![owner, b, c],
      Quorum { f: 1 },
      Default::default(),
      3,
      true,
    );
    let acting = |regional: &RegionalConfiguration| {
      [owner, b, c, outsider]
        .into_iter()
        .filter(|asker| acts_for(regional, owner, *asker, object))
        .collect::<Vec<HostId>>()
    };
    assert_eq!(
      acting(&regional),
      vec![owner],
      "the owner lives: only the owner"
    );
    assert!(regional.retire(owner, 3), "the owner retired");
    let successor = regional.successor(owner, object);
    assert!(successor.is_some_and(|successor| successor == b || successor == c));
    assert_eq!(
      acting(&regional),
      successor.into_iter().collect::<Vec<HostId>>(),
      "once the owner is retired, only its successor"
    );
  }
}

#[cfg(test)]
mod dial_tests {
  use super::{ESTABLISH_BUDGETS_BEFORE_REDIAL, HandshakeStall, keeps_its_socket};

  /// §4.8 formation: a dialer past its confirmation redials at once; one whose peer has not answered keeps its socket
  /// for the budgets a booting or starved peer may need. Do: ask for every stall at no budgets spent and at the limit.
  /// Expect: a confirmation stall never keeps the socket; every other stall keeps it until the budgets are spent.
  #[test]
  fn a_dialer_past_its_confirmation_redials_at_once() {
    for stall in [
      HandshakeStall::ConfirmDeadline,
      HandshakeStall::ConfirmSilence,
      HandshakeStall::ConfirmGarbage,
    ] {
      assert!(!keeps_its_socket(1, Some(stall)), "{stall:?}");
    }
    for stall in [
      HandshakeStall::Silence,
      HandshakeStall::Repeats,
      HandshakeStall::Fragments,
      HandshakeStall::TurnCeiling,
    ] {
      assert!(keeps_its_socket(1, Some(stall)), "{stall:?}");
      assert!(
        !keeps_its_socket(ESTABLISH_BUDGETS_BEFORE_REDIAL, Some(stall)),
        "{stall:?}"
      );
    }
  }
}

#[cfg(test)]
mod probe_mesh_tests {
  use super::{HostId, ProbeMesh};

  /// §4.8, the mesh's predicate at its edges: do judge a laptop (no other member known), a fleet with every
  /// in-contact peer probed, one with a peer unprobed, and a fleet of several with no peer in contact (none reached, or
  /// all dead). Expect only the laptop and the fully probed fleet meshed.
  #[test]
  fn a_mesh_over_no_peer_is_meshed_only_on_a_laptop() {
    let (a, b, c) = (HostId(1), HostId(2), HostId(3));
    let mesh =
      |in_contact: &[HostId], unmeshed: &[HostId], alive: &[HostId], others_known| ProbeMesh {
        in_contact: in_contact.to_vec(),
        unmeshed: unmeshed.to_vec(),
        alive: alive.to_vec(),
        others_known,
      };
    assert!(
      mesh(&[], &[], &[a], false).meshed(),
      "a laptop is meshed (R8)"
    );
    assert!(
      mesh(&[b, c], &[], &[a, b, c], true).meshed(),
      "every peer probed"
    );
    assert!(
      !mesh(&[b, c], &[c], &[a, b, c], true).meshed(),
      "c unprobed"
    );
    assert!(
      !mesh(&[], &[], &[a], true).meshed(),
      "a fleet of several with no peer in contact"
    );
  }
}

#[cfg(test)]
mod request_kind_tests {
  /// §4.8, D-14: a record session multiplexes every fleet request by its kind, and `serve_peer_records` dispatches by
  /// kind alone, "never a guess from the bytes" — so two kinds sharing a value send one plane's requests to another's
  /// handler. Do: list every request kind a fleet connection carries (each `serve_peer_records` dispatches, and the
  /// ledger's promote). Expect no two equal. Found when live batches
  /// (`LIVE_ENTRIES_STREAM`) and A-288's work route questions both took 25 on branches merged the same day: every batch
  /// was answered by the route's handler with nothing and sent again at once, about 500 a second, while the holders
  /// took none of it (2026-11-17, g230).
  #[test]
  fn every_request_kind_the_record_session_dispatches_is_its_own() {
    let kinds: [(&str, u64); 28] = [
      ("record", slates_cluster::RECORD_STREAM),
      (
        "content offer",
        slates_cluster::content::CONTENT_OFFER_STREAM,
      ),
      (
        "content chunk",
        slates_cluster::content::CONTENT_CHUNK_STREAM,
      ),
      (
        "content fetch",
        slates_cluster::content::CONTENT_FETCH_STREAM,
      ),
      ("council", super::CONFIG_STREAM),
      ("council fetch", super::CONFIG_FETCH_STREAM),
      ("root", super::ROOT_STREAM),
      ("root fetch", super::ROOT_FETCH_STREAM),
      ("forward", super::FORWARD_STREAM),
      ("merge record", crate::merge_service::MERGE_RECORD_STREAM),
      ("discovery", crate::discovery::STREAM),
      ("owner location", crate::owner_location::STREAM),
      ("report", super::REPORT_STREAM),
      ("takeover", crate::takeover::TAKEOVER_STREAM),
      ("plane epoch", crate::member_task::PLANE_EPOCH_STREAM),
      ("recipient", super::RECIPIENT_STREAM),
      ("pair key", super::PAIR_KEY_STREAM),
      ("mirror record", crate::mirror::MIRROR_RECORD_STREAM),
      ("mirror promise", crate::mirror::MIRROR_PROMISE_STREAM),
      ("live entry", slates_cluster::live::LIVE_ENTRY_STREAM),
      ("live state", slates_cluster::live::LIVE_STATE_STREAM),
      ("death", super::DEATH_STREAM),
      ("work route", crate::work_route::STREAM),
      ("live entries", slates_cluster::live::LIVE_ENTRIES_STREAM),
      ("lineage", super::LINEAGE_STREAM),
      ("ledger promote", slates_cluster::LEDGER_PROMOTE_STREAM),
      ("takeover ledger", crate::takeover::TAKEOVER_LEDGER_STREAM),
      ("move", crate::moves::MOVE_STREAM),
    ];
    let mut shared = Vec::new();
    for (at, (name, kind)) in kinds.iter().enumerate() {
      for (other, other_kind) in kinds.iter().skip(at + 1) {
        if kind == other_kind {
          shared.push((*name, *other, *kind));
        }
      }
    }
    assert!(
      shared.is_empty(),
      "request kinds that share a value: {shared:?}"
    );
  }
}

#[cfg(test)]
mod learner_wait_tests {
  use super::{HEARTBEAT_NS, next_learner_wait};

  /// The uninitialized-learner backoff (battery, 2026-10-09). Do: step the wait while the group stays uninitialized,
  /// then once it is initialized. Expect it to double from a heartbeat and stop at the membership horizon, and to be
  /// one heartbeat again once the group is initialized.
  #[test]
  fn an_uninitialized_learner_backs_its_fetches_off_to_the_horizon_and_an_initialized_one_returns()
  {
    let horizon = crate::lease::horizon_ns();
    let mut wait = HEARTBEAT_NS;
    let mut waits = Vec::new();
    for _ in 0..8 {
      wait = next_learner_wait(true, wait);
      waits.push(wait);
    }
    assert_eq!(waits.first(), Some(&HEARTBEAT_NS.saturating_mul(2)));
    assert!(waits.windows(2).all(|pair| pair[0] <= pair[1]), "{waits:?}");
    assert_eq!(
      waits.last(),
      Some(&horizon),
      "the wait stops at the horizon: {waits:?}"
    );
    assert_eq!(next_learner_wait(false, horizon), HEARTBEAT_NS);
  }
}

#[cfg(test)]
mod discovery_wait_tests {
  use super::{HEARTBEAT_NS, next_discovery_wait};

  /// A settled roster's checks back off and a change brings them back: do a run of pages at one generation, then one
  /// at a new generation, then one mid-sweep. Expect the wait to double from a heartbeat up to the membership horizon
  /// and stay there, and to return to one heartbeat at the change and mid-sweep.
  #[test]
  fn a_settled_roster_backs_its_discovery_checks_off_to_the_horizon_and_a_change_resets_them() {
    let horizon = crate::lease::horizon_ns();
    let mut wait = HEARTBEAT_NS;
    let mut waits = Vec::new();
    for _ in 0..8 {
      wait = next_discovery_wait(Some(7), Some(7), wait);
      waits.push(wait);
    }
    assert_eq!(waits.first(), Some(&(2 * HEARTBEAT_NS)));
    assert!(waits.windows(2).all(|pair| pair[1] >= pair[0]), "{waits:?}");
    assert_eq!(
      waits.last(),
      Some(&horizon),
      "the wait stops at the horizon: {waits:?}"
    );
    assert_eq!(
      next_discovery_wait(Some(7), Some(8), horizon),
      HEARTBEAT_NS,
      "a new generation resets"
    );
    assert_eq!(
      next_discovery_wait(Some(7), None, horizon),
      HEARTBEAT_NS,
      "a sweep under way resets"
    );
    assert_eq!(
      next_discovery_wait(None, Some(7), horizon),
      HEARTBEAT_NS,
      "a first completed sweep starts at a heartbeat"
    );
  }
}

#[cfg(test)]
mod tests {
  /// A volume named `<name>-origin` made for a principal on `state`'s shard, snapshotted, and that snapshot cloned as
  /// `<name>-clone`: the clone, which has taken no snapshot of its own.
  fn fresh_clone(state: &mut crate::state::ShardState, name: &str) -> slates_db::catalog::VolumeId {
    use slates_ipc::protocol::{NamePolicy, ReplyBody, RequestBody, SizeClass};
    let owner = slates_db::catalog::Principal::Uid {
      host: slates_db::HostId(0),
      uid: 1234,
    };
    let ReplyBody::Created { id: origin } = crate::verbs::dispatch(
      state,
      1,
      &owner,
      RequestBody::Create {
        name: format!("{name}-origin"),
        size: SizeClass::Dynamic { max: 1 << 20 },
        names: NamePolicy::Exact,
        require_locked: false,
        base: None,
      },
    ) else {
      panic!("the origin was created")
    };
    let ReplyBody::Snapshotted { id: snapshot, .. } =
      crate::verbs::dispatch(state, 1, &owner, RequestBody::Snapshot { volume: origin })
    else {
      panic!("the origin was snapshotted")
    };
    let ReplyBody::Cloned { id: clone } = crate::verbs::dispatch(
      state,
      1,
      &owner,
      RequestBody::Clone {
        volume: origin,
        snapshot,
        name: format!("{name}-clone"),
      },
    ) else {
      panic!("the clone was made")
    };
    crate::verbs::to_db_volume(clone)
  }

  /// A-577. Do: on a shard, clone a volume's snapshot and ask the record plane what head it ships for the clone before the
  /// clone takes a snapshot of its own. Expect the creation head at sequence 0, naming no content, as a new volume's: the
  /// fleet knows the clone at once, so a live clone's log can answer its writers and a takeover finds it. Before, the
  /// clone's record was a copy of its origin's, naming the origin's head at the origin's sequence, and nothing shipped for
  /// the clone until its own first seal: at `f > 0` a live clone's log never answered.
  #[test]
  fn a_fresh_clone_ships_its_creation_head_at_once() {
    let shipped = crate::daemon::audit_on_shard(|state| {
      let id = fresh_clone(state, "shipped");
      let record = state.db.partition().volume(id).cloned().unwrap();
      let quorum = state.fleet.configuration().quorum;
      super::head_value_of(state, &record, super::ObjectId(id.bytes), quorum)
        .map(|(sequence, value)| (sequence, value.manifest))
    });
    assert_eq!(
      shipped,
      Some((0, None)),
      "a fresh clone ships its creation head, naming no content"
    );
  }

  /// A-565's piece (the sibling A-544 saw). Do: on a shard, clone a volume's snapshot and ask the home seal walk what the
  /// clone has to seal before it takes a snapshot of its own, then after its first, recorded unplaced as at `f > 0` (a
  /// laptop records a new snapshot placed on its owner at once, leaving nothing to seal). Expect nothing before (its head
  /// names no snapshot of its own: the creation head since A-577) and its own head after. Before A-565 the walk named
  /// its origin's head, and `start_seal` was refused for it, counted `fleet.seal`, every period until the clone's first
  /// snapshot: for every clone made.
  #[test]
  fn a_clone_has_nothing_to_seal_until_its_first_snapshot() {
    let (before, after, own) = crate::daemon::audit_on_shard(|state| {
      use slates_ipc::protocol::{ReplyBody, RequestBody};
      let owner = slates_db::catalog::Principal::Uid {
        host: slates_db::HostId(0),
        uid: 1234,
      };
      let id = fresh_clone(state, "unsealed");
      let object = super::ObjectId(id.bytes);
      let before = super::sealable_head(state, id, object);
      let clone = crate::verbs::to_wire_volume(id);
      let ReplyBody::Snapshotted { .. } =
        crate::verbs::dispatch(state, 1, &owner, RequestBody::Snapshot { volume: clone })
      else {
        panic!("the clone was snapshotted")
      };
      let own = state.db.partition().volume(id).map(|record| record.head);
      if let Some(own) = own {
        let unplaced = slates_db::Op::SnapshotPlaced {
          volume: id,
          id: own,
          placed: super::PlacementState::Local,
        };
        state.db.mutate(&mut state.segment, &unplaced, 0).unwrap();
      }
      (before, super::sealable_head(state, id, object), own)
    });
    assert_eq!(
      before, None,
      "a clone has nothing of its own to seal before its first snapshot"
    );
    assert_eq!(
      after.map(|(head, _)| head),
      own,
      "its first snapshot of its own is what it seals"
    );
  }

  /// Installs the council's committed configuration on `state`'s fleet node as the control shard's install does
  /// (`readmission::install_placement`), then walks the held copies to it a slice at a time until the walk ends
  /// ([`super::install_slice`]), as the coordinator's period drives it ([`super::drive_install_walk`]).
  pub(super) fn walk_the_install(state: &mut crate::state::ShardState, local: super::HostId) {
    let regional = state.council.configuration().clone();
    let Some(configuration) = regional.configuration_for(local) else {
      return;
    };
    let version = configuration.version;
    crate::readmission::install_placement(state, configuration, &regional.members);
    state.install_walk = Some(super::InstallWalk {
      version,
      after: None,
      walking: false,
    });
    // Every turn reaches at least one copy, so the walk ends within one turn a copy and one more.
    let turns = state.holder_records.len().saturating_add(1);
    for _ in 0..turns {
      if !super::install_slice(state, local, version) {
        return;
      }
    }
  }

  /// What a holder's copy of a moved object answers once its install's walk has reached it
  /// ([`a_holders_copy_of_a_moved_object_follows_the_move_as_the_install_walk_reaches_it`]).
  #[derive(Debug)]
  struct Followed {
    /// The member the object moved from (its creator) and the member it moved to.
    from: super::HostId,
    to: super::HostId,
    /// The owner this node's routing names for the object.
    routed: Option<super::HostId>,
    /// The owner the copy's authority names.
    authority: Option<super::HostId>,
    /// How the copy judges the next record from the move's new owner, and from the owner it left.
    from_new: Option<Result<(), slates_db::register::RegisterError>>,
    from_old: Option<Result<(), slates_db::register::RegisterError>>,
    /// The copies counted followed (`fleet.move.followed`).
    followed: u64,
  }

  /// A-487, goal 85's bug 2, closed by A-485's rule (§4.8 "Authority scope", A-291's switch through A-398's walk). Do: on
  /// a holder in a region of three members at `f = 1`, hold a copy of an object its creator owns, then commit the
  /// creator's move of it to the third member and install that configuration through the walk as the coordinator drives
  /// it ([`walk_the_install`]). Expect the copy routed to the new owner, its authority naming the new owner, its next
  /// record from the new owner accepted and one from the creator refused `Unauthorized`, and the copy counted followed.
  /// Before A-485 the walk's per-copy step followed a move only for a copy routed to a retired host, so the copy kept the
  /// creator's authority and refused every record and content put of the new owner: a volume moved back to its creator
  /// could never place its head off its owner, and the move test's answers alternated `Moving` and
  /// `MoveRefused { Unplaced }` until its deadline (the third node refusing 8,006 records and 2,663 content puts
  /// `Unauthorized`, quiet Mac Studio, main 3022c6894).
  #[test]
  fn a_holders_copy_of_a_moved_object_follows_the_move_as_the_install_walk_reaches_it() {
    let seen = crate::daemon::audit_on_shard(|state| {
      use slates_cluster::config_group::{Reconfiguration, RegionalCouncil};
      use slates_db::register::{Acceptor, Authority, FIRST_EPOCH, ObjectId, Quorum, Record};
      let local = state.fleet.host();
      let (from, to) = (super::HostId(local.0 ^ 1), super::HostId(local.0 ^ 2));
      state.council = RegionalCouncil::new(
        local,
        vec![local],
        vec![local],
        Quorum { f: 1 },
        std::collections::BTreeMap::new(),
        3,
        false,
      );
      for host in [from, to] {
        assert!(
          state
            .council
            .propose(Reconfiguration::Admit { host, domain: None })
        );
      }
      walk_the_install(state, local);
      let object = ObjectId::new(from, 7);
      let generation = state.fleet.configuration().version;
      let mut copy = Acceptor::new(
        local,
        Authority {
          generation,
          owner: from,
        },
      );
      copy
        .accept(&Record {
          owner: from,
          object,
          sequence: 0,
          epoch: FIRST_EPOCH,
          generation,
          value: b"the head before the move".to_vec(),
        })
        .unwrap();
      state.holder_records.insert(object, copy);
      state.fleet.track_object_owner(object, from);
      assert!(state.council.propose(Reconfiguration::Move {
        object,
        from,
        to,
        cohort: vec![from, local, to],
      }));
      walk_the_install(state, local);
      let generation = state.council.configuration().version;
      let fence = state
        .holder_records
        .get(&object)
        .map(|copy| copy.persisted().0);
      let next = |owner| {
        fence.map(|epoch| Record {
          owner,
          object,
          sequence: 1,
          epoch,
          generation,
          value: b"the next head".to_vec(),
        })
      };
      Followed {
        from,
        to,
        routed: state.fleet.object_owner(object),
        authority: state.holder_records.get(&object).map(Acceptor::owner),
        from_new: next(to).map(|record| super::check_held_record(state, local, to, &record)),
        from_old: next(from).map(|record| super::check_held_record(state, local, from, &record)),
        followed: state
          .refusals
          .get("fleet.move.followed")
          .copied()
          .unwrap_or(0),
      }
    });
    assert_eq!(
      seen.routed,
      Some(seen.to),
      "the copy is routed to the move's new owner: {seen:?}"
    );
    assert_eq!(
      seen.authority,
      Some(seen.to),
      "the copy's authority names the new owner: {seen:?}"
    );
    assert_eq!(
      seen.from_new,
      Some(Ok(())),
      "the new owner's next record is accepted: {seen:?}"
    );
    assert_eq!(
      seen.from_old,
      Some(Err(slates_db::register::RegisterError::Unauthorized)),
      "the old owner's ({:?}) is refused: {seen:?}",
      seen.from
    );
    assert_eq!(seen.followed, 1, "the copy is counted followed: {seen:?}");
  }

  /// CLAUDE.md §2 item 8 with §4.8's paged takeover: a taken-over object's wait for a register the still-paging round
  /// of its departed owner may yet bring reads that round's page advances, which the bound takes as its progress; every
  /// other wait, and every wait once the round stops paging, reads none and counts. Do: ask for the round of a catalog
  /// wait and of another wait for an object whose departed owner's round pages at 7 advances, of a catalog wait whose
  /// round has stopped paging, and of one for an object no round adopted. Expect 7 for the catalog wait under its
  /// paging round and none elsewhere. Before (2026-11-18) every wait counted, which gave a green up, and with the volume
  /// bound a volume, whose catalog register came pages after its head: on a host with more than the bound's worth of
  /// pages of heads in one partition (100 periods of about 55 objects at the default SLO), every one.
  #[test]
  fn a_catalog_wait_reads_its_round_only_while_the_round_pages() {
    let (departed, other) = (super::HostId(5), super::HostId(6));
    let paging = std::collections::BTreeMap::from([(departed, 7)]);
    assert_eq!(
      super::paging_round(true, Some(departed), &paging),
      Some(7),
      "a catalog wait while its round pages reads the round's advances"
    );
    assert_eq!(
      super::paging_round(true, Some(other), &paging),
      None,
      "a catalog wait once its round stopped paging: counted"
    );
    assert_eq!(
      super::paging_round(true, None, &paging),
      None,
      "a catalog wait for an object no round adopted: counted"
    );
    assert_eq!(
      super::paging_round(false, Some(departed), &paging),
      None,
      "another wait while its round pages: counted"
    );
    assert!(
      super::GreenWait::Catalog.waits_on_the_round()
        && !super::GreenWait::Chain.waits_on_the_round()
        && !super::GreenWait::Inputs.waits_on_the_round(),
      "a green waits on its round only for its catalog register"
    );
    assert!(
      super::VolumeWait::Catalog.waits_on_the_round()
        && super::VolumeWait::CatalogPlacement.waits_on_the_round()
        && !super::VolumeWait::Content.waits_on_the_round(),
      "a volume waits on its round only for its catalog register and that register's placement"
    );
  }

  /// A node back at its anchor with fresh RAM (its store destroyed, the same certificate) has a recipient of its own: the
  /// key learned from its predecessor is not used to deliver to it, and its own replaces the predecessor's even with the
  /// cache full. Kept by anchor alone, every pair the survivors sent the replacement was wrapped to the destroyed node's
  /// key and refused (1,583 of 1,583, diag-live-durability-72): the replacement could open no key it was sent.
  #[test]
  fn a_recipient_learned_from_a_predecessor_is_not_used_for_its_replacement() {
    let destroyed = hyper_seal::recipient::Recipient::generate(true)
      .unwrap()
      .public();
    let fresh = hyper_seal::recipient::Recipient::generate(true)
      .unwrap()
      .public();
    let (anchor, predecessor, replacement) = (7, HostId(1), HostId(2));
    // Shape: a cache with room for this one anchor alone.
    let capacity = 1;
    let mut kept = std::collections::BTreeMap::new();
    super::keep_recipient(&mut kept, capacity, anchor, predecessor, destroyed.clone());
    assert_eq!(
      super::kept_recipient(&kept, anchor, predecessor),
      Some(destroyed),
      "the predecessor's key, for the predecessor"
    );
    assert_eq!(
      super::kept_recipient(&kept, anchor, replacement),
      None,
      "the replacement at the same anchor is asked for its own key"
    );
    super::keep_recipient(&mut kept, capacity, anchor, replacement, fresh.clone());
    assert_eq!(
      super::kept_recipient(&kept, anchor, replacement),
      Some(fresh),
      "the replacement's key replaces the predecessor's, the cache full"
    );
  }

  /// A region of five members at `f = 1` with one neighbourhood of all of them, and an object host 1 created.
  fn content_region() -> (slates_db::register::RegionalConfiguration, ObjectId, u64) {
    // Shape: a neighbourhood of every member.
    let scatter = 5;
    let regional = slates_db::register::RegionalConfiguration::formed(
      (1..=5).map(HostId).collect(),
      slates_db::register::Quorum { f: 1 },
      std::collections::BTreeMap::new(),
      scatter,
      false,
    );
    (regional, ObjectId::new(HostId(1), 7), scatter)
  }

  /// The acting owner's candidates for `object`, as the holder computes them.
  fn content_candidates(
    regional: &slates_db::register::RegionalConfiguration,
    owner: HostId,
    object: ObjectId,
  ) -> Vec<HostId> {
    regional
      .configuration_for(owner)
      .unwrap()
      .place(object)
      .candidates
  }

  /// AUD-29-45 (§4.13 "Content identity and sharing"): do: ask who may place and read an object's content in
  /// a live region; expect the owner alone to place, and only onto its own candidates; its candidates to read;
  /// a member outside the placement and a non-member to be refused.
  #[test]
  fn content_authority_follows_the_objects_owner_and_placement() {
    use slates_cluster::content::ContentAccess::{Place, Read};
    let (regional, object, _) = content_region();
    let records = std::collections::BTreeMap::new();
    let owner = HostId(1);
    let candidates = content_candidates(&regional, owner, object);
    let holder = *candidates.iter().find(|host| **host != owner).unwrap();
    let outsider = (1..=5)
      .map(HostId)
      .find(|host| !candidates.contains(host))
      .unwrap();
    let allowed =
      |peer, access, local| content_authorized(&regional, &records, local, peer, access, object);
    assert!(allowed(owner, Place, holder));
    assert!(!allowed(owner, Place, outsider), "only onto a candidate");
    assert!(!allowed(holder, Place, holder), "only the owner places");
    assert!(allowed(holder, Read, holder), "a candidate reads");
    assert!(
      !allowed(outsider, Read, holder),
      "a member outside the placement"
    );
    assert!(!allowed(HostId(99), Read, holder), "a non-member");
  }

  /// AUD-29-45: do: take the object's owner over, and ask again; expect the successor to act as owner — to
  /// place onto its own candidates and read from the old cohort — while the departed host is refused.
  #[test]
  fn content_authority_passes_to_the_takeover_successor() {
    use slates_cluster::content::ContentAccess::{Place, Read};
    let (mut regional, object, scatter) = content_region();
    let records = std::collections::BTreeMap::new();
    let owner = HostId(1);
    let holder = *content_candidates(&regional, owner, object)
      .iter()
      .find(|host| **host != owner)
      .unwrap();
    regional.take_over(owner, scatter).unwrap();
    let successor = regional.successor(owner, object).unwrap();
    let landing = *content_candidates(&regional, successor, object)
      .iter()
      .find(|host| **host != successor)
      .unwrap();
    let allowed =
      |peer, access, local| content_authorized(&regional, &records, local, peer, access, object);
    assert!(
      allowed(successor, Place, landing),
      "the successor acts as owner"
    );
    assert!(allowed(successor, Read, holder));
    assert!(
      !allowed(owner, Read, holder),
      "the departed owner is refused"
    );
    assert!(!allowed(owner, Place, landing));
  }
  use super::*;
  use slates_cluster::DispatchWait;

  /// A dial in its handshake to `anchor`'s peer: a real client endpoint on a loopback socket, its first
  /// flight unsent — what the record link holds between periods while the peer has not answered yet.
  fn pending_dial() -> Endpoint {
    let key = rcgen::KeyPair::generate().unwrap();
    let certificate = rcgen::CertificateParams::new(vec!["fleet".to_owned()])
      .unwrap()
      .self_signed(&key)
      .unwrap();
    let der = certificate.der().clone();
    let identity = Identity::from_der(
      der.clone(),
      rustls::pki_types::PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
    );
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
    let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9);
    Endpoint::client(socket, peer, &identity, &der, "fleet", fleet_shape()).unwrap()
  }

  /// A record link's redial (A-200's rolling partition flow): do call [`establish_session`] once with no client, to a
  /// port nothing is bound to. Expect the call that builds the endpoint to run its first handshake too, a budget spent
  /// before it returns. Before, the build and the handshake were separate calls a link period apart: 100.3 ms at the
  /// tenth percentile and 100.9 to 101.9 ms at the median over 487 redials (diag-live-durability-50), on the path of every
  /// resumed peer's catch-up.
  #[test]
  fn a_redial_starts_its_handshake_in_the_call_that_builds_it() {
    let config = slates_rt::runtime::RuntimeConfig {
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
      wake_tracking: None,
    };
    let mut sim = slates_rt::sim::SimRuntime::new(&config, 1).unwrap();
    let shard = sim.shard_ids()[0];
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    sim
      .spawn_on(shard, async move {
        let key = rcgen::KeyPair::generate().unwrap();
        let certificate = rcgen::CertificateParams::new(vec!["fleet".to_owned()])
          .unwrap()
          .self_signed(&key)
          .unwrap();
        let der = certificate.der().clone();
        let identity = Identity::from_der(
          der.clone(),
          rustls::pki_types::PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
        );
        let kept = slates_rt::registry::with_current(|context| context.keep(identity))
          .unwrap()
          .unwrap();
        let address = NodeAddress::Ip(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9));
        let (client, session) = establish_session(
          None,
          None,
          kept,
          "fleet",
          (&address, crate::deploy::Plane::Record),
          &der,
          None,
        )
        .await;
        let _ = result_tx.send((
          client.map(|dial| dial.handshake_budgets_spent()),
          session.is_some(),
        ));
      })
      .unwrap();
    sim.run_until_idle();
    assert_eq!(
      result_rx.try_recv().unwrap(),
      (Some(1), false),
      "the dial is kept with its first handshake budget spent (the peer is silent), no session"
    );
  }

  /// AC-8.1 (§4.8 "Deployment", task #22; docs/bugs/2026-09-28-formation-dropped-a-dial-to-the-peer-it-was-reaching.md):
  /// when the first fresh id is learned for a peer the link knew only by its manifest seed, the dial in
  /// its handshake is kept — it is reaching that same process — and nothing is counted; when a peer that
  /// had a live incarnation restarts under a new id, the pending dial (to the old incarnation) is dropped
  /// and counted as stale. Both cases move the link to the new id and restart its discovery sweep.
  #[test]
  fn a_seed_replaced_by_the_first_fresh_id_keeps_the_dial_but_a_restart_drops_it() {
    let (seed_case, restart_case) = crate::daemon::audit_on_shard(|state| {
      let anchor = HostId(0x5eed);
      let seed = crate::deploy::member_id(anchor, 0);
      let first = crate::deploy::member_id(anchor, 11);
      let second = crate::deploy::member_id(anchor, 22);
      let stale = |state: &ShardState| state.refusals.get(DIAL_STALE_DROPPED).copied().unwrap_or(0);

      state.learned_members.insert(
        anchor,
        LearnedMember {
          boot_nonce: 11,
          host: first,
          previous: None,
        },
      );
      let mut client = Some(pending_dial());
      let mut cursor = crate::discovery::Cursor::default();
      let now = refresh_record_identity_in(state, anchor, seed, &mut client, &mut cursor);
      let seed_case = (now == first, client.is_some(), stale(state));

      state.learned_members.insert(
        anchor,
        LearnedMember {
          boot_nonce: 22,
          host: second,
          previous: None,
        },
      );
      let now = refresh_record_identity_in(state, anchor, first, &mut client, &mut cursor);
      let restart_case = (now == second, client.is_some(), stale(state));
      (seed_case, restart_case)
    });
    assert_eq!(
      seed_case,
      (true, true, 0),
      "the first fresh id moves the link and keeps its pending dial, counting nothing"
    );
    assert_eq!(
      restart_case,
      (true, false, 1),
      "a restart moves the link and drops the dial to the old incarnation, counted"
    );
  }

  /// AC-8.1 (§4.8 "Deployment", task #22; docs/bugs/2026-09-28-a-gossiped-seed-death-stranded-an-unreached-peer.md):
  /// a peer this node has never reached is known only by its manifest seed. When a neighbour that did reach
  /// it learns its real id, the seed is folded dead and that death gossips here — but it says only that the
  /// peer has a real incarnation, so this node keeps direct contact (its probe and record link keep dialing
  /// to learn that id itself). Once the real id is learned, a death of the real id drops contact as before.
  #[test]
  fn a_gossiped_seed_death_keeps_contact_until_the_real_id_is_learned() {
    let (unreached, learned_seed, learned_real) = crate::daemon::audit_on_shard(|state| {
      let anchor = HostId(0x5eed);
      let seed = crate::deploy::member_id(anchor, 0);
      let real = crate::deploy::member_id(anchor, 7);
      state.config.fleet = Some(crate::config::FleetMembership {
        quorum: Quorum { f: 1 },
        peers: vec![seed],
        host: state.fleet.host(),
        origin_anchor: state.origin_anchor,
        domains: std::collections::BTreeMap::new(),
        regions: std::collections::BTreeMap::new(),
        region_mirrors: std::collections::BTreeMap::new(),
        durability: None,
      });
      let dead = MemberState {
        liveness: Liveness::Dead,
        incarnation: 0,
      };
      // A neighbour learned the peer's real id and gossiped its seed's death here.
      apply_peer_state(&mut state.fleet, seed, Some(dead));
      let unreached = keeps_direct_contact_with(state, seed);
      // This node reaches the peer itself and learns its real id; then the real id dies.
      state.learned_members.insert(
        anchor,
        LearnedMember {
          boot_nonce: 7,
          host: real,
          previous: None,
        },
      );
      let learned_seed = keeps_direct_contact_with(state, seed);
      apply_peer_state(&mut state.fleet, real, Some(dead));
      let learned_real = keeps_direct_contact_with(state, real);
      (unreached, learned_seed, learned_real)
    });
    assert!(
      unreached,
      "a peer known only by its seed keeps contact when the seed's death is gossiped"
    );
    assert!(
      !learned_seed,
      "once the real id is learned, the seed is a retired placeholder"
    );
    assert!(!learned_real, "a learned peer's death drops contact");
  }

  /// A millisecond in nanoseconds, so the samples read as round times.
  const MS: u64 = 1_000_000;

  /// AC-8.1: authenticated announcements must derive from their anchor and nonce. A new
  /// nonce changes the member in either numeric direction; malformed claims are refused.
  #[test]
  fn an_announced_identity_is_current_restarted_or_forged() {
    let anchor = HostId(0xA11C);
    let seed = crate::deploy::member_id(anchor, 0);
    let next = crate::deploy::member_id(anchor, 1);
    let known = LearnedMember {
      boot_nonce: 0,
      host: seed,
      previous: None,
    };
    assert_eq!(
      classify_announced(Some(&known), anchor, 0, seed),
      LearnedOutcome::Current,
      "the seed at boot_nonce 0 is the ordinary contact"
    );
    assert_eq!(
      classify_announced(Some(&known), anchor, 1, next),
      LearnedOutcome::Restarted { old: seed },
      "a different boot nonce is a restart, naming the seed as the id to retire"
    );
    let restarted = LearnedMember {
      boot_nonce: 1,
      host: next,
      previous: None,
    };
    assert_eq!(
      classify_announced(Some(&restarted), anchor, 0, seed),
      LearnedOutcome::Restarted { old: next },
      "boot nonces have no numeric ordering"
    );
    assert_eq!(
      classify_announced(Some(&known), anchor, 1, seed),
      LearnedOutcome::Forged,
      "the seed id is not what boot_nonce 1 derives to"
    );
    assert_eq!(
      classify_announced(Some(&known), anchor, 0, HostId(7)),
      LearnedOutcome::Forged,
      "an id that derives from nothing is forged"
    );
    assert_eq!(
      classify_announced(None, anchor, 3, crate::deploy::member_id(anchor, 3)),
      LearnedOutcome::Restarted { old: seed },
      "first contact retires the non-voting manifest placeholder"
    );
  }

  /// The hedge delay follows the design's law, by use: one period before any reading (the first seal of a
  /// boot, the laptop); then the measured p95 of the content class's put latency — the reading nineteen in
  /// twenty acknowledgements arrive within — and the window is bounded: the oldest reading leaves as the
  /// newest arrives, so a load regime is forgotten within a window of seals.
  #[test]
  fn the_hedge_delay_is_the_measured_p95_over_a_bounded_window() {
    let mut latency = LatencyWindow::default();
    assert_eq!(
      hedge_delay_ns(&latency),
      HEARTBEAT_NS,
      "before any reading: one period"
    );
    // Twenty prompt acknowledgements and one slow one: the p95 (nearest rank, the 20th of 21 sorted) is
    // the slowest prompt reading, not the outlier — a single straggler does not become the trigger.
    for _ in 0..20 {
      latency.record(10 * MS);
    }
    latency.record(900 * MS);
    assert_eq!(
      hedge_delay_ns(&latency),
      10 * MS,
      "one slow acknowledgement in twenty-one does not move the p95"
    );
    // Fill the window with slow readings: the p95 follows the new regime, and the window stays bounded.
    for _ in 0..PUT_LATENCY_WINDOW {
      latency.record(50 * MS);
    }
    assert_eq!(latency.len(), PUT_LATENCY_WINDOW, "the window is bounded");
    assert_eq!(
      hedge_delay_ns(&latency),
      50 * MS,
      "the prompt regime was forgotten within one window"
    );
  }

  /// A content round that has gathered **no** acknowledgement when the hedge delay elapses expires there —
  /// it is not extended — so the coordinator hedges the remaining candidates at the measured p95 (§4.8
  /// "hedged to the remaining candidates after the measured p95 put latency"; Dean & Barroso). Before this
  /// the round's progress witness was born "advancing" and the extension was granted unconditionally at
  /// the delay, so a first round to a starved holder ran the full span (3.2 s measured against a 3 s
  /// hold) and the hedge never fired — one run in three, on the sub-poll timing of the deadline race.
  #[test]
  fn a_round_with_no_acknowledgement_at_the_hedge_delay_expires_rather_than_extends() {
    let mut latency = LatencyWindow::default();
    latency.record(10 * MS);
    let budget = content_budget(&latency, consensus_budget(None));
    let hedge_delay = hedge_delay_ns(&latency);
    let started = 1_000 * MS;
    let mut wait = DispatchWait::new(budget, started);
    // At the hedge delay with nothing gathered: the round must expire, not be extended to the full span.
    let judged = wait.judge(0, started + hedge_delay);
    assert!(
      !judged,
      "a round with no acknowledgement at the p95 expires so the hedge can fire; it was extended"
    );
    // The same round with one acknowledgement gathered just before the delay is still filling: extended.
    let mut filling = DispatchWait::new(budget, started);
    filling.witness_mut().observe(1, started + hedge_delay / 2);
    assert!(
      filling.judge(1, started + hedge_delay),
      "a round that gathered an acknowledgement within the delay is still filling and is extended"
    );
  }

  /// The hedge widens the round's targets on the **clock**, not on the count of rounds that placed: before the
  /// hedge delay a round goes to the first `f` remote candidates; once the first round has been outstanding
  /// for the delay, to every remaining one — even if no round has placed yet (the first round's only holder
  /// was unavailable, so it produced no placement and the count never moved; keyed on the count, the put
  /// re-aimed at that holder for the whole 3 s hold).
  #[test]
  fn the_hedge_widens_the_targets_on_the_clock_not_the_placed_round_count() {
    let remote = vec![HostId(2), HostId(3), HostId(4)];
    assert_eq!(
      hedge_targets(false, remote.clone(), 1),
      vec![HostId(2)],
      "before the delay: the first f remote candidates only"
    );
    assert_eq!(
      hedge_targets(true, remote.clone(), 1),
      remote,
      "at the delay: every remaining candidate, whatever the placed-round count"
    );
  }

  /// The healer's cadence follows the measured put-failure rate, by use: the at-rest cadence while every
  /// round places, tightening in proportion to the share of rounds that ended short, down to one period —
  /// never below it — when short rounds are as common as placed ones.
  #[test]
  fn the_healer_cadence_tightens_with_the_measured_put_failure_rate() {
    let mut outcomes = PutOutcomes::default();
    assert_eq!(
      heal_period_ns(&outcomes),
      HEARTBEAT_NS * HEAL_PERIODS_AT_REST,
      "before any round, and while none has failed: the at-rest cadence"
    );
    for _ in 0..HEAL_PERIODS_AT_REST {
      outcomes.record(true);
    }
    assert_eq!(
      heal_period_ns(&outcomes),
      HEARTBEAT_NS * HEAL_PERIODS_AT_REST,
      "a hundred placed rounds and no short one: still at rest"
    );
    outcomes.record(false);
    let one_short = heal_period_ns(&outcomes);
    assert!(
      (HEARTBEAT_NS..HEARTBEAT_NS * HEAL_PERIODS_AT_REST).contains(&one_short),
      "one short round in a hundred tightens the cadence: {one_short} ns"
    );
    for _ in 0..HEAL_PERIODS_AT_REST {
      outcomes.record(false);
    }
    assert_eq!(
      heal_period_ns(&outcomes),
      HEARTBEAT_NS,
      "short rounds as common as placed ones: one snapshot per period, the floor"
    );
  }

  /// §4.10 "the healer" (A-276), the heal's decision at `f = 0` and on a fleet. Do: decide the heal for a one-node
  /// fleet's placement (the owner alone at `f = 0`), for an `f = 1` placement whose named holders are members and
  /// candidates, one naming a departed holder, one naming fewer than `f + 1` holders, one whose named holder is a member
  /// outside the candidates, and one naming a holder twice. Expect: no heal at `f = 0`, since the owner's copy is the
  /// whole placement; a restore of exactly the named candidates with the owner; a replace for the departed and for the
  /// short, each found; no heal when no named candidate is left, the outsider found, but a replace for the same
  /// placement of an adopted volume (A-529); and the repeat counted once.
  /// Before (2026-11-17), a placement naming no remote holder was replaced, so at `f = 0` every healer step advanced the
  /// head's sequence by one, for good.
  #[test]
  fn the_heal_restores_named_candidates_and_replaces_only_a_departed_or_short_placement() {
    let (owner, b, c, d) = (HostId(1), HostId(2), HostId(3), HostId(4));
    let lone = heal_decision(
      owner,
      &[owner.0],
      &[owner],
      &[owner],
      (Quorum { f: 0 }, false),
    );
    assert_eq!(
      lone,
      (None, HealFindings::default()),
      "f = 0: the owner's own copy is the whole placement; nothing to heal"
    );
    let fleet = [owner, b, c];
    let restored = heal_decision(
      owner,
      &[owner.0, b.0],
      &fleet,
      &fleet,
      (Quorum { f: 1 }, false),
    );
    assert_eq!(
      restored,
      (Some(Heal::Restore(vec![owner, b])), HealFindings::default()),
      "every named holder a member and a candidate: restore exactly them"
    );
    let departed = heal_decision(
      owner,
      &[owner.0, d.0],
      &fleet,
      &fleet,
      (Quorum { f: 1 }, false),
    );
    assert_eq!(
      departed,
      (
        Some(Heal::Replace),
        HealFindings {
          named_gone: true,
          named_outside_candidates: true,
          ..HealFindings::default()
        }
      ),
      "a named holder that left: replace"
    );
    let short = heal_decision(owner, &[owner.0], &fleet, &fleet, (Quorum { f: 1 }, false));
    assert_eq!(
      short,
      (
        Some(Heal::Replace),
        HealFindings {
          named_short: true,
          ..HealFindings::default()
        }
      ),
      "fewer than f + 1 named: replace"
    );
    let moved = heal_decision(
      owner,
      &[owner.0, c.0],
      &fleet,
      &[owner, b],
      (Quorum { f: 1 }, false),
    );
    assert_eq!(
      moved,
      (
        None,
        HealFindings {
          named_outside_candidates: true,
          ..HealFindings::default()
        }
      ),
      "the named holder a member outside the candidates: left as its head names it, found"
    );
    let adopted = heal_decision(
      owner,
      &[owner.0, c.0],
      &fleet,
      &[owner, b],
      (Quorum { f: 1 }, true),
    );
    assert_eq!(
      adopted,
      (
        Some(Heal::Replace),
        HealFindings {
          named_outside_candidates: true,
          ..HealFindings::default()
        }
      ),
      "an adopted volume naming a member outside its candidates: replace, so its round can confirm (A-529)"
    );
    let repeated = heal_decision(
      owner,
      &[owner.0, b.0, b.0],
      &fleet,
      &fleet,
      (Quorum { f: 2 }, false),
    );
    assert_eq!(
      repeated,
      (
        Some(Heal::Replace),
        HealFindings {
          named_short: true,
          ..HealFindings::default()
        }
      ),
      "a holder named twice is one copy: two copies are short of f + 1 = 3"
    );
  }

  /// A-546: while an owner's neighbourhood change is in flight, the healer replaces a placement naming a host outside
  /// its current neighbourhood, and only then. Do: judge a placement naming a host outside the neighbourhood, in flight
  /// and settled, and one naming only the owner and its neighbours, in flight. Expect a replace for the first in flight
  /// alone: settled, the healer's own rule decides; within the neighbourhood nothing is owed.
  #[test]
  fn an_unsettled_owners_placement_outside_its_current_neighbourhood_is_replaced() {
    let (owner, b, c, outside) = (HostId(1), HostId(2), HostId(3), HostId(4));
    let neighbourhood = [owner, b, c];
    assert_eq!(
      [
        unsettled_replace(&[owner.0, b.0, outside.0], owner, &neighbourhood, true),
        unsettled_replace(&[owner.0, b.0, outside.0], owner, &neighbourhood, false),
        unsettled_replace(&[owner.0, b.0, c.0], owner, &neighbourhood, true),
      ],
      [true, false, false],
      "replaced only in flight and only when it names a host outside the current neighbourhood"
    );
  }

  /// Shape: the receiving shard of the fan tests — any shard other than the fixture's own.
  const OTHER_SHARD: u16 = 1;

  /// AUD-29-29 (§4.8, D-7). Do: on the control shard, ask what another shard is owed across periods — a
  /// first fan, a recorded delivery, a lease change, a newer placement, and a fan whose channel refused it (not
  /// recorded). Expect: the first fan carries every part the receiver can install; nothing is owed (no copy made) while nothing
  /// changed; a lease change owes the lease alone; a newer placement owes the placement alone; and an
  /// unrecorded fan is owed again, so a refused channel self-heals. Before 2026-10-01 every period cloned the
  /// placement,
  /// members, root configuration and lease once, and again for every shard.
  #[test]
  fn a_shard_is_fanned_only_what_changed_since_it_last_received_it() {
    crate::daemon::audit_on_shard(|s| {
      s.fanned.observe_lease(&s.lease);
      let first = s.fanned.owed(s, OTHER_SHARD).expect("a first fan is owed");
      assert!(first.placement.is_some());
      // A root configuration is fanned only at a version the receiver can adopt (above its initial zero).
      assert_eq!(first.root.is_some(), s.root.configuration().version > 0);
      assert!(first.ready.is_some() && first.lease.is_some());
      let delivered = first.delivered();
      s.fanned.record(OTHER_SHARD, delivered);
      assert!(
        s.fanned.owed(s, OTHER_SHARD).is_none(),
        "nothing changed, nothing owed"
      );

      s.lease.superseded = Some(s.fleet.configuration().version + 1);
      let lease = s.lease.clone();
      s.fanned.observe_lease(&lease);
      let fan = s.fanned.owed(s, OTHER_SHARD).expect("the lease changed");
      assert!(fan.lease.is_some());
      assert!(fan.placement.is_none() && fan.root.is_none() && fan.ready.is_none());
      // Refused: not recorded, so it is owed again the next period.
      assert!(
        s.fanned.owed(s, OTHER_SHARD).is_some(),
        "a refused fan self-heals"
      );
      let delivered = fan.delivered();
      s.fanned.record(OTHER_SHARD, delivered);

      let mut newer = s.fleet.configuration().clone();
      newer.version += 1;
      let members = s.fleet.members().to_vec();
      s.fleet.install_configuration(newer.clone(), &members);
      let fan = s
        .fanned
        .owed(s, OTHER_SHARD)
        .expect("the placement changed");
      assert_eq!(
        fan
          .placement
          .as_ref()
          .map(|(placement, _)| placement.version),
        Some(newer.version)
      );
      assert!(fan.lease.is_none() && fan.root.is_none() && fan.ready.is_none());
    });
  }

  /// A-363, A-574. Do: ask what a shard is owed of the failover delay before any fan, record it delivered, move the
  /// control shard's delay by a period, record that, and ask the configuration fan what the shard is owed. Expect the
  /// delay owed at first; nothing once delivered; the moved delay owed; nothing once that is delivered; and no
  /// configuration fan owed for a moved delay alone: every shard's waits read the control shard's estimate, sent only
  /// when it moves, by the beat's own fan on every node, laptop or fleet.
  #[test]
  fn a_shard_is_fanned_the_failover_delay_when_it_moves() {
    crate::daemon::audit_on_shard(|s| {
      assert_eq!(
        s.fanned.failover_owed(s.failover_ns, OTHER_SHARD),
        Some(s.failover_ns),
        "the first fan carries the delay"
      );
      s.fanned.record_failover(OTHER_SHARD, s.failover_ns);
      assert_eq!(
        s.fanned.failover_owed(s.failover_ns, OTHER_SHARD),
        None,
        "nothing moved"
      );
      s.fanned.observe_lease(&s.lease);
      let configuration = s.fanned.owed(s, OTHER_SHARD).expect("a first fan is owed");
      s.fanned.record(OTHER_SHARD, configuration.delivered());
      s.failover_ns = s.failover_ns.saturating_add(HEARTBEAT_NS);
      assert_eq!(
        s.fanned.failover_owed(s.failover_ns, OTHER_SHARD),
        Some(s.failover_ns),
        "the moved delay is owed"
      );
      assert!(
        s.fanned.owed(s, OTHER_SHARD).is_none(),
        "the configuration fan does not carry it"
      );
      s.fanned.record_failover(OTHER_SHARD, s.failover_ns);
      assert_eq!(
        s.fanned.failover_owed(s.failover_ns, OTHER_SHARD),
        None,
        "delivered"
      );
    });
  }

  /// AUD-29-29. Do: install a fan carrying only a lease on a shard whose lease is empty. Expect: the shard's
  /// lease is the fanned one and nothing else it holds changes.
  #[test]
  fn an_installed_fan_gives_the_receiver_the_fanned_lease() {
    crate::daemon::audit_on_shard(|s| {
      let fanned = crate::lease::OwnerLease {
        superseded: Some(s.fleet.configuration().version + 1),
        ..crate::lease::OwnerLease::default()
      };
      let version = s.fleet.configuration().version;
      s.lease = crate::lease::OwnerLease::default();
      Fan {
        placement: None,
        root: None,
        ready: None,
        lease: Some(fanned.clone()),
        now: Delivered::default(),
      }
      .install(s);
      assert_eq!(s.lease, fanned);
      assert_eq!(
        s.fleet.configuration().version,
        version,
        "no placement was fanned"
      );
    });
  }
}

#[cfg(test)]
mod root_learner_tests {
  use super::{HostId, with_shown_peer};

  /// A-489 (§4.8, D-14 root learners): a root learner's fetch asks the peer that showed it a newer root beside the voters
  /// it knows, since those can be as far behind as it is. Do: add a shown peer to a list of one known voter, to a list
  /// already naming it, to an empty list, and add this node itself; and add no peer. Expect: the voter and the peer; the
  /// list unchanged (never asked twice); the empty list kept empty, which asks every session the learner holds, the
  /// peer's among them; this node never asked; the list unchanged.
  #[test]
  fn a_root_learners_fetch_asks_the_peer_that_showed_it_a_newer_root() {
    let (voter, peer, local) = (HostId(1), HostId(2), HostId(3));
    assert_eq!(
      with_shown_peer(vec![voter], Some(peer), local),
      vec![voter, peer]
    );
    assert_eq!(
      with_shown_peer(vec![voter, peer], Some(peer), local),
      vec![voter, peer]
    );
    assert_eq!(
      with_shown_peer(Vec::new(), Some(peer), local),
      Vec::<HostId>::new()
    );
    assert_eq!(
      with_shown_peer(vec![voter], Some(local), local),
      vec![voter]
    );
    assert_eq!(with_shown_peer(vec![voter], None, local), vec![voter]);
  }
}

#[cfg(test)]
mod state_log_tests {
  use slates_cluster::config_group::{Reconfiguration, RegionalCouncil};
  use slates_db::catalog::{LogName, LogSpan, LoggedChange, NameChange, RepositoryId, StateLog};
  use slates_db::catalog::{StateLogRecord, VolumeId};
  use slates_db::register::{Ack, FIRST_EPOCH, Quorum, RegisterClass};
  use slates_ipc::protocol::{ObjectFormat, ReplyBody, RequestBody, VcsTool};

  use super::{HostId, ObjectId, Placement, Record};
  use crate::state::ShardState;

  /// This node and two members admitted beside it, a region of three at `f = 1`, its configuration installed as the
  /// coordinator installs it: every object's candidates are the three.
  fn region_of_three(state: &mut ShardState) -> (HostId, [HostId; 2]) {
    let local = state.fleet.host();
    let others = [HostId(local.0 ^ 1), HostId(local.0 ^ 2)];
    state.council = RegionalCouncil::new(
      local,
      vec![local],
      vec![local],
      Quorum { f: 1 },
      std::collections::BTreeMap::new(),
      3,
      false,
    );
    for host in others {
      assert!(
        state
          .council
          .propose(Reconfiguration::Admit { host, domain: None })
      );
    }
    super::tests::walk_the_install(state, local);
    (local, others)
  }

  /// The owning principal the verbs run as.
  fn owner() -> slates_db::catalog::Principal {
    slates_db::catalog::Principal::Uid {
      host: slates_db::HostId(0),
      uid: 1234,
    }
  }

  /// A repository `name` made on `state`'s shard with its default branch `main`, and a branch `topic` of it: its id.
  fn repository_and_branch(state: &mut ShardState, name: &str) -> RepositoryId {
    let created = crate::verbs::dispatch(
      state,
      1,
      &owner(),
      RequestBody::CreateRepository {
        name: name.to_owned(),
        tool: VcsTool::Git,
        format: ObjectFormat::Sha1,
        default_branch: "main".to_owned(),
        green: None,
      },
    );
    assert!(
      matches!(created, ReplyBody::GreenCreated { .. }),
      "{created:?}"
    );
    let branched = crate::verbs::dispatch(
      state,
      1,
      &owner(),
      RequestBody::CreateBranch {
        repository: name.to_owned(),
        name: "topic".to_owned(),
        from: "main".to_owned(),
        version: None,
      },
    );
    assert!(
      matches!(branched, ReplyBody::GreenCreated { .. }),
      "{branched:?}"
    );
    state
      .db
      .partition()
      .repositories()
      .repository_named(name)
      .map(|record| record.id)
      .unwrap()
  }

  /// A-636 part 2b. Do: on an owner shard in a region of three at `f = 1`, create a repository and a branch of it, ask
  /// the record plane what it owes, then record a commit round of the first part acknowledged by one remote candidate,
  /// and another by the other, as `ship_head`'s rounds record them. Expect: the names log's two parts owed in position
  /// order (the root joined and the default bound, 0 to 2; the branch joined and bound, 2 to 4), each at the names
  /// log's register, to the repository's own candidates, its value the record itself, naming one operation each; no
  /// other state log owed anything; the first part still owed to the candidate that lacks it, then let go once both
  /// hold it, the second still owed to both; and no acknowledgement of a part kept as a head's placement.
  #[test]
  fn an_owners_state_log_records_are_owed_to_the_repositorys_candidates_in_order() {
    crate::daemon::audit_on_shard(|state| {
      let (local, [b, c]) = region_of_three(state);
      let repo = repository_and_branch(state, "shipped");
      crate::state_log::take_made(state);
      let register = LogName::State {
        repository: repo,
        log: StateLog::Names,
      }
      .register();
      let config = state.fleet.configuration().clone();
      let owed = super::owed_state_log_parts(state, &config, local);
      let candidates = config.place(ObjectId(repo.bytes)).candidates;
      assert_eq!(candidates.len(), 3, "{candidates:?}");
      let parts: Vec<(ObjectId, u64, u64, Vec<HostId>)> = owed
        .iter()
        .map(|head| {
          let part = crate::state_log::record_of(&head.record.value).unwrap();
          assert_eq!(head.shape.candidates, candidates);
          assert_eq!(
            head.record.sequence, part.to,
            "a part sits at the position it leaves"
          );
          (head.object, part.from, part.to, head.acked.clone())
        })
        .collect();
      assert_eq!(
        parts,
        vec![(register, 0, 2, Vec::new()), (register, 2, 4, Vec::new())]
      );
      let placement = Placement {
        acked: vec![local, b],
        ..config.place(register)
      };
      super::record_acks_in(state, register, 2, FIRST_EPOCH, placement.clone());
      let owed = super::owed_state_log_parts(state, &config, local);
      let holding = |acked: &[HostId]| {
        let mut acked = acked.to_vec();
        acked.sort_unstable();
        acked
      };
      assert_eq!(
        owed
          .iter()
          .map(|head| (head.record.sequence, holding(&head.acked)))
          .collect::<Vec<_>>(),
        vec![(2, holding(&[local, b])), (4, Vec::new())],
        "the owner's own hold and b's"
      );
      let placement = Placement {
        acked: vec![c],
        ..placement
      };
      super::record_acks_in(state, register, 2, FIRST_EPOCH, placement);
      let owed = super::owed_state_log_parts(state, &config, local);
      assert_eq!(
        owed
          .iter()
          .map(|head| head.record.sequence)
          .collect::<Vec<_>>(),
        vec![4]
      );
      assert!(!state.placed_heads.contains_key(&register));
    });
  }

  /// A-636 part 2c-1. Do: on an owner shard in a region of three at `f = 1`, create a repository and a branch of it, ask
  /// the record plane what it owes for the repository's own register; record both remote candidates' acknowledgements as
  /// `ship_head`'s round records them and ask again; then retire the branch's name and ask again. Expect: the
  /// repository's record owed at its own id, to its family's candidates, at sequence 5 (policy version 0, the names log
  /// at 4, plus one), its value the record as the database holds it; nothing owed once both candidates hold it; after
  /// the retire, the record owed again at 6 to both.
  #[test]
  fn an_owners_repository_record_is_owed_to_its_familys_candidates_at_each_change() {
    crate::daemon::audit_on_shard(|state| {
      let (local, [b, c]) = region_of_three(state);
      let repo = repository_and_branch(state, "recorded");
      let object = ObjectId(repo.bytes);
      let config = state.fleet.configuration().clone();
      let owed_now = |state: &ShardState| {
        super::owed_repositories(state, &config, local)
          .into_iter()
          .map(|head| {
            (
              head.object,
              head.record.sequence,
              head.shape.candidates,
              head.record.value,
            )
          })
          .collect::<Vec<_>>()
      };
      let record = state
        .db
        .partition()
        .repositories()
        .repository(repo)
        .cloned()
        .unwrap();
      let candidates = config.place(object).candidates;
      assert_eq!(
        owed_now(state),
        vec![(
          object,
          5,
          candidates.clone(),
          crate::catalog::repository_value(&record)
        )]
      );
      assert_eq!(
        crate::catalog::repository_of(&crate::catalog::repository_value(&record)),
        Some(record)
      );
      let placement = Placement {
        acked: vec![local, b, c],
        ..config.place(object)
      };
      super::record_acks_in(state, object, 5, FIRST_EPOCH, placement);
      assert_eq!(owed_now(state), Vec::new());
      let retired = crate::verbs::dispatch(
        state,
        1,
        &owner(),
        RequestBody::RetireBranch {
          repository: "recorded".to_owned(),
          name: "topic".to_owned(),
        },
      );
      assert!(matches!(retired, ReplyBody::Acknowledged), "{retired:?}");
      let again: Vec<(u64, Vec<HostId>)> = owed_now(state)
        .into_iter()
        .map(|(_, sequence, candidates, _)| (sequence, candidates))
        .collect();
      assert_eq!(again, vec![(6, candidates)]);
    });
  }

  /// The names log's part of `repository` from `from` to `to`, one bind a position, as one operation's.
  fn names_part(repository: RepositoryId, (from, to): (u64, u64)) -> StateLogRecord {
    let log = LogName::State {
      repository,
      log: StateLog::Names,
    };
    StateLogRecord {
      repository,
      log: StateLog::Names,
      from,
      to,
      identity: [u8::try_from(to).unwrap(); 32],
      spans: vec![LogSpan { log, from, to }],
      changes: (from..to)
        .map(|position| LoggedChange::Named {
          change: NameChange::Bind {
            name: format!("b{position}"),
            green: VolumeId::default(),
          },
        })
        .collect(),
    }
  }

  /// A-636 part 2b. Do: on a holder in a region of three at `f = 1`, receive from a repository's owner its names log's
  /// parts as the record plane ships them: the part from 2 to 3 before the first; the first, 0 to 2; the one from 2 to 3
  /// again; then 3 to 5; and at that register a value that reads as a volume's head. Expect: the part past a gap
  /// refused with no acknowledgement, counted `discontinuous`; the others acknowledged in order, every position kept (a
  /// ledger: none compacted to the newest); the head-shaped value refused, counted malformed.
  #[test]
  fn a_holder_holds_a_repositorys_state_log_as_a_prefix_never_compacted() {
    let seen = crate::daemon::audit_on_shard(|state| {
      let (local, [owner, _]) = region_of_three(state);
      let repo = RepositoryId {
        bytes: ObjectId::new(owner, 5).0,
      };
      let register = LogName::State {
        repository: repo,
        log: StateLog::Names,
      }
      .register();
      let generation = state.fleet.configuration().version;
      let shipped = |part: &StateLogRecord| Record {
        owner,
        object: register,
        sequence: part.to,
        epoch: FIRST_EPOCH,
        generation,
        value: crate::state_log::value_of(part),
      };
      let mut acked = Vec::new();
      for span in [(2, 3), (0, 2), (2, 3), (3, 5)] {
        let reply =
          super::accept_held_record(state, local, owner, &shipped(&names_part(repo, span)));
        acked.push(Ack::decode(&reply).is_ok());
      }
      let head = crate::head::HeadValue {
        manifest: None,
        content_holders: Vec::new(),
        sealing: None,
      };
      let headlike = Record {
        sequence: 6,
        value: head.to_record_bytes(),
        ..shipped(&names_part(repo, (5, 6)))
      };
      acked.push(Ack::decode(&super::accept_held_record(state, local, owner, &headlike)).is_ok());
      let kept: Vec<u64> = [2, 3, 5]
        .into_iter()
        .filter(|position| {
          state
            .holder_records
            .get(&register)
            .is_some_and(|copy| copy.value_at(register, *position).is_some())
        })
        .collect();
      let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
      (
        acked,
        kept,
        count("fleet.record.refused.state_log.discontinuous"),
        count("fleet.record.refused.state_log.malformed"),
      )
    });
    assert_eq!(seen.0, vec![false, true, true, true, false], "{seen:?}");
    assert_eq!(seen.1, vec![2, 3, 5], "every position kept: {seen:?}");
    assert_eq!((seen.2, seen.3), (1, 1), "{seen:?}");
  }

  /// A-636 part 2c-2. Do: on a holder in a region of three at `f = 1`, hold a departed owner's repository's names log
  /// as its record plane ships it: its root bound `main` (0 to 1), then `topic` bound by a push of a version of
  /// `topic`'s green that this node's head never reached (1 to 2); adopt the repository's record; rebuild it, twice.
  /// Expect: it is ready with both parts; it stands with `main` alone, its names log at 1, the push dropped and counted
  /// once, its store made, counted rebuilt once; the second rebuild finds it standing and changes nothing.
  #[test]
  fn a_successor_rebuilds_a_repository_whole_from_the_logs_it_holds() {
    let seen = crate::daemon::audit_on_shard(|state| {
      let (local, [departed, _]) = region_of_three(state);
      let family = |number| ObjectId::family(departed, 0, 9, number).unwrap();
      let green = |number| VolumeId {
        bytes: family(number).0,
      };
      let repo = RepositoryId { bytes: family(1).0 };
      let generation = state.fleet.configuration().version;
      for held in held_names(repo, green(2), green(3)) {
        let shipped = Record {
          owner: departed,
          object: held.log_name().register(),
          sequence: held.to,
          epoch: FIRST_EPOCH,
          generation,
          value: crate::state_log::value_of(&held),
        };
        let reply = super::accept_held_record(state, local, departed, &shipped);
        assert!(Ack::decode(&reply).is_ok());
      }
      let record = slates_db::catalog::RepositoryRecord {
        index_bits: 0,
        id: repo,
        name: "linux".to_owned(),
        tool: slates_db::catalog::VcsTool::Git,
        format: slates_db::catalog::ObjectFormat::Sha1,
        default_branch: Some("main".to_owned()),
        owner: owner(),
        policy: slates_db::catalog::RepositoryPolicy::default(),
      };
      state.pending_repositories.insert(family(1), (2, record));
      let ready = crate::takeover::repositories_ready(state);
      let held: Vec<usize> = ready.iter().map(|(_, _, parts)| parts.len()).collect();
      let rebuilt: Vec<bool> = ready
        .into_iter()
        .flat_map(|(_, record, parts)| {
          let again = (record.clone(), parts.clone());
          [
            crate::takeover::rebuild_repository(state, record, parts),
            crate::takeover::rebuild_repository(state, again.0, again.1),
          ]
        })
        .collect();
      let repositories = state.db.partition().repositories();
      let names = (
        repositories.branch(repo, "main") == Some(green(2)),
        repositories.branch(repo, "topic"),
        repositories.log_position(repo, StateLog::Names),
      );
      let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
      let counted = [
        crate::takeover::TAKEOVER_REPOSITORY_REBUILT,
        crate::takeover::TAKEOVER_REPOSITORY_OPERATION_DROPPED,
        crate::takeover::TAKEOVER_REPOSITORY_UNREBUILT,
      ]
      .map(count);
      (held, rebuilt, names, state.vcs_stores.holds(repo), counted)
    });
    assert_eq!(
      seen,
      (
        vec![2],
        vec![true, true],
        (true, None, Some(1)),
        true,
        [1, 1, 0]
      )
    );
  }

  /// A departed owner's names log of `repo`: `main` bound to `root` (0 to 1), then `topic` bound to `topic` by a push
  /// of its version 1 (1 to 2).
  fn held_names(repo: RepositoryId, root: VolumeId, topic: VolumeId) -> [StateLogRecord; 2] {
    let names = LogName::State {
      repository: repo,
      log: StateLog::Names,
    };
    let part = |from: u64, (name, green): (&str, VolumeId), pushed: Option<LogSpan>| {
      let to = from.saturating_add(1);
      StateLogRecord {
        repository: repo,
        log: StateLog::Names,
        from,
        to,
        identity: [u8::try_from(to).unwrap_or(u8::MAX); 32],
        spans: std::iter::once(LogSpan {
          log: names,
          from,
          to,
        })
        .chain(pushed)
        .collect(),
        changes: vec![LoggedChange::Named {
          change: NameChange::Bind {
            name: name.to_owned(),
            green,
          },
        }],
      }
    };
    let pushed = LogSpan {
      log: LogName::Green { green: topic },
      from: 0,
      to: 1,
    };
    [
      part(0, ("main", root), None),
      part(1, ("topic", topic), Some(pushed)),
    ]
  }

  /// A-636 part 2b. Do: name the counters a volume's head, its catalog and each of a repository's state logs ship
  /// under, placed and short of placed. Expect: each class its own, a state log's never its repository's head's.
  #[test]
  fn a_state_logs_part_ships_under_its_own_counters_never_its_repositorys_heads() {
    let quorum = Quorum { f: 1 };
    let candidates = vec![HostId(1), HostId(2), HostId(3)];
    let placed = Placement {
      acked: vec![HostId(1), HostId(2)],
      ..Placement::of(&candidates)
    };
    let short = Placement {
      acked: vec![HostId(1)],
      ..Placement::of(&candidates)
    };
    let repository = ObjectId::new(HostId(1), 5);
    let outcome = |object: ObjectId, placement: &Placement| {
      super::ship_outcome_counter(
        super::ship_counters(object.class()),
        &Ok(placement.clone()),
        quorum,
      )
    };
    assert_eq!(outcome(repository, &placed), super::SHIP_HEAD_PLACED);
    assert_eq!(
      outcome(repository.catalog(), &short),
      super::SHIP_CATALOG_SHORT
    );
    for log in [
      StateLog::Names,
      StateLog::Phases,
      StateLog::Markers,
      StateLog::Locks,
    ] {
      let register = repository.in_class(RegisterClass::State(log));
      assert_eq!(
        outcome(register, &placed),
        "fleet.ship.state_log.placed",
        "{log:?}"
      );
      assert_eq!(
        outcome(register, &short),
        "fleet.ship.state_log.short",
        "{log:?}"
      );
      assert_eq!(
        super::ship_counters(register.class()).no_session,
        "fleet.ship.state_log.no_session"
      );
    }
  }
}
