//! The fleet membership loop the control shard runs (§4.8 "Membership"; §2.6 boot step 6). A fleet node
//! probes each of its peers over the transport, serves their probes, replicates its volume heads to them,
//! and folds the converged SWIM view into its [`FleetNode`](slates_cluster::fleet::FleetNode) — retiring a
//! dead peer and taking over the objects that rendezvous now ranks first to this node.
//!
//! **One serve socket per plane, every peer on it** (`slates_transport::demux::Demux`): a peer dials this
//! node's advertised probe or record socket from an address chosen at dial time; the demultiplexer routes
//! each datagram to that peer's session by the connection id in its header (a raw handshake datagram by
//! its source, opening a session for a dialer it has not heard from), hands each new session to the
//! plane's accept loop, and **replaces** a peer's old session when the peer re-dials after losing it —
//! so a mid-run session loss recovers without anyone being told. This node *dials* each peer's serve
//! sockets from separate sockets, so every session is cleanly one-directional (no bidirectional-request
//! deadlock). A two-node fleet is the single-peer degenerate and is proven live
//! (`crates/server/tests/fleet.rs`); the loop is the general N-peer form (it iterates the transport's
//! peers), and a fleet of N forms its full N·(N−1) session mesh over 2N serve sockets (the handshake
//! retries on one socket until the peer's accept completes — [`establish_session`]).
//!
//! **Tasks on the control shard.** The **probe** plane is per peer, because [`serve_probe`] borrows its
//! detector across the receive await while [`probe_once`] does not — so a single shared detector cannot drive
//! both — and each peer's detector is folded into the shared `FleetNode` by [`sync_peer`], which touches only
//! the peer it tracks (its own deaths and joins), so N detectors compose. The **record** plane is one
//! coordinator for all peers, because a commit and a takeover promotion span *several* holders at once:
//! - the per-peer **probe** task owns a failure [`Detector`], dials the peer, and each protocol period probes
//!   it, folds the acknowledgement (or lets a timeout age the suspicion), then folds the detector's view into
//!   the shard's `FleetNode` and records any takeover;
//! - the per-peer **serve** tasks accept the peer on this node's per-peer sockets and answer its probes (so
//!   the peer sees this node alive) and its register record commits ([`serve_peer_records`] accepts each into
//!   this node's **durable per-object hold** in the shard state, so it backs the peer as a candidate holder
//!   and the record survives for a takeover to read);
//! - the per-peer **record link** task ([`establish_record_link`]) keeps this node's client record session
//!   to the peer up in the shard state ([`ShardState::record_sessions`]), retrying its handshake on one
//!   socket each period and re-establishing a lost one — per peer, so one slow link never stalls the rest;
//! - the one **record-plane coordinator** ([`run_record_plane`]) borrows those sessions for each dispatch,
//!   so each period it ships each unplaced head to **all** its candidate holders in one commit (§4.8 "records
//!   are sent to all candidates; committed at `f + 1`") and **drives any owed takeover** over **all** the
//!   object's surviving holders — the `f + 1` promise quorum a takeover needs, several holders at `f > 1` —
//!   and recovers the sessions of holders still in flight past an early quorum ([`Stragglers`]).
//!
//! **Takeover phase-one recovery** (§4.8 "Promotion and takeover"): when a peer dies, the probe loop records
//! the objects `sync_peer` reassigns to this node ([`ShardState::pending_takeovers`]) and brings every held
//! acceptor's authority into step with the routing view ([`reconcile_held_authority`] — the successor is
//! installed, so a holder answers the new owner's prepare and accepts its re-commit); the coordinator then
//! promotes the object over all surviving candidate holders and re-commits the adopted head under the new
//! epoch, recording the placement so the verbs read the head owned and region-placed. The serve loop answers
//! a `Prepare` from the same durable hold ([`serve_held_promotion`]). This is the general `f`-tolerant form
//! (proven at `f = 1` over three nodes and `f = 2` over five, the promotion spanning a multi-holder quorum).
//!
//! **Content replication and serve** (§4.10 "Content replication"; §4.8 mechanism 1): each owned volume's
//! newest snapshot is archived in bounded slices ([`advance_seals`], the vfs `SnapshotArchiver`), its
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
//! dispatch, and the acknowledgements and durable placements are recorded back there. Owed:
//! content-defined chunking and the compress-or-not cost model (D-17), the hedge trigger from a measured
//! p95, anti-entropy and the healer.
//!
//! The `FleetNode` lives in the shard state (the verbs read it for placement), so it is touched only
//! through brief synchronous [`state::with_state`] — never held across an await. At `f = 0` (the laptop)
//! there is no fleet transport and this loop does not run; the placement path still runs the same
//! `FleetNode`, degenerate (R8).

use std::sync::mpsc::{TryRecvError, channel};

use rustls::pki_types::CertificateDer;
use slates_archive::Archive;
use slates_cluster::content::{fetch_content, is_content_stream, put_content};
use slates_cluster::detector::{Detector, DetectorTiming};
use slates_cluster::fleet::{apply_peer_state, sync_peer};
use slates_cluster::membership::Liveness;
use slates_cluster::raft_wire::{
  RaftMessage, decode_regional_configuration, decode_root_configuration,
  encode_regional_configuration, encode_root_configuration,
};
use slates_cluster::swim::{ProbeOutcome, SwimMessage, probe_once};
use slates_cluster::{
  ClusterError, CommitBudget, PROMOTE_STREAM, RECORD_STREAM, Stragglers, commit_record,
  promote_record, request_within,
};
use slates_db::Op;
use slates_db::catalog::{
  PlacementState, SnapshotId as DbSnapshotId, VolumeId as DbVolumeId, VolumeRecord,
};
use slates_db::register::{
  Acceptor, Authority, FIRST_EPOCH, HostEpoch, HostId, ObjectId, Placement, Prepare, Quorum,
  Record, RegionId, candidates_for, encode_refusal,
};
use slates_rt::futures;
use slates_rt::tcp::{Ipv4Addr, SocketAddrV4};
use slates_rt::udp::UdpSocket;
use slates_transport::demux::Demux;
use slates_transport::endpoint::{Endpoint, MIN_DATAGRAM_BYTES};
use slates_transport::handshake::Identity;
use slates_vfs::clock::Clock;
use slates_vfs::export::{Progress, SnapshotArchiver};

use crate::daemon::HEARTBEAT_NS;
use crate::head::{HeadValue, PlacedHead, SealJob};
use crate::state::{self, ShardState};
use crate::verbs;
use crate::xshard::{call_within, run_on};

/// An upper bound on one packet's non-payload bytes for the frame-cap derivation — the RFC 9000 §17.3
/// short header (first byte + the eight-byte connection id + a packet number ≤ 4 bytes → 13), the RFC
/// 9001 §5.3 AEAD tag (16), and one RFC 9000 §19.8 STREAM-frame header (type + stream-id + offset +
/// length varints + fin, ≤ 43): 72, rounded up so a full-cap frame's packet never crosses
/// [`MIN_DATAGRAM_BYTES`].
/// Format: 13 + 16 + 43 = 72, rounded up to 80.
const FLEET_PACKET_OVERHEAD: usize = 80;

/// Derived: the largest stream-frame payload whose packet still fits within [`MIN_DATAGRAM_BYTES`] (§4.9
/// "Frame caps per class from measured MTU and class budgets"; the frame cap is `Endpoint`'s
/// `max_frame_len`, which also floors the initial receive window at `(REORDER_THRESHOLD + 1) × cap`). At
/// this cap a whole fleet message — a SWIM probe, a register record, or its acknowledgement, each at most a
/// few hundred bytes — rides a single frame inside one receive window, so a commit completes in one round
/// trip. At the previous 16-byte cap a 68-byte record fragmented into five frames across a 64-byte window
/// and needed ten credit-gated round trips, which lost the commit's deadline under core contention.
/// Anchored to [`MIN_DATAGRAM_BYTES`] less [`FLEET_PACKET_OVERHEAD`].
const FLEET_FRAME_CAP: usize = MIN_DATAGRAM_BYTES - FLEET_PACKET_OVERHEAD;

/// Derived: SWIM's infection factor rounded to a per-bit integer weight for `λ·ln(n+1)` (§4.8; SWIM §4.1).
/// `λ·ln(x) = λ·ln(2)·log2(x)`, and the bit-length of `x` is `⌊log2(x)⌋+1`, so with SWIM's high-probability
/// `λ ≈ 3` the coefficient `λ·ln(2) ≈ 2.08` rounds to `2` per bit — a small integer (determinism-clean, no
/// float on any decision) that tracks `λ·ln(n+1)` within a rebroadcast across sizes (n=1 → 2, n=1000 → 20).
const GOSSIP_PER_BIT: u32 = 2;

/// Derived: the base suspicion window in protocol periods, at full local health (§4.8 "SWIM with
/// Lifeguard"; SWIM §4.2 uses a small multiple of the period so a lost acknowledgement is retried by the
/// next period before a member is suspected). Two periods: one to miss, one to confirm the miss, before the
/// aging declares death; the Lifeguard multiplier dilates it when this node itself looks unhealthy.
const SUSPICION_PERIODS: u32 = 2;

/// Derived: the Lifeguard local-health multiplier cap minus one — a 3× cap (§4.8 "bounded local-health
/// multiplier"; the raw `(LHM+1)` reaches 9× at the paper's saturation, which pushes timers off a cliff, so
/// hyperscale softened it to a 3× cap). A small integer keeps the dilation determinism-clean.
const LOCAL_HEALTH_CAP: u32 = 2;

/// Derived: how many times the collection loop polls for a reply within one protocol period — ten, so the
/// loop wakes within a tenth of a period of the acknowledgement (10 ms at the default cadence) without
/// spinning. A finer value measured from the RTT is the owed refinement.
const POLL_PER_PERIOD: u64 = 10;

/// A fleet peer this node probes and is probed by (§4.8): its host id, the two addresses this node dials to
/// reach it (its probe and record sockets), and the operator-provisioned certificate the mutual-TLS session
/// pins (§4.8 "TLS 1.3 via rustls with certificates provisioned by the operator") — which is also how the
/// serve side tells which peer dialed it (`serve_peer_records`).
pub struct FleetPeer {
  /// The peer's host id.
  pub host: HostId,
  /// The peer's advertised probe address — where it accepts this node's SWIM probes, and where this node
  /// dials it.
  pub address: SocketAddrV4,
  /// The peer's advertised record address — where it accepts this node's register record commits (a separate
  /// socket from the probe one: the SWIM and register wire formats are not distinguished by content on a
  /// shared stream).
  pub record_address: SocketAddrV4,
  /// The peer's operator-provisioned certificate, pinned for the mutual-TLS session.
  pub certificate: CertificateDer<'static>,
}

/// The fleet transport material the membership loop drives (§4.8, boot step 6). It is kept out of the
/// Clone-able [`DaemonConfig`](crate::config::DaemonConfig) because [`Identity`] is not `Clone` (it holds a
/// private key): the membership *policy* (quorum + peers) lives in the config and builds the `FleetNode`;
/// this *transport* material is handed to [`Daemon::start`](crate::Daemon) and moved to the control shard.
pub struct FleetTransport {
  /// This node's fleet TLS identity (operator-provisioned).
  pub identity: Identity,
  /// The TLS server name this node presents and its peers pin.
  pub name: String,
  /// This node's probe-serve address: the one socket every peer's SWIM probes arrive on.
  pub probe_bind: SocketAddrV4,
  /// This node's record-serve address: the one socket every peer's record commits, prepares and content
  /// exchanges arrive on.
  pub record_bind: SocketAddrV4,
  /// The peers this node probes and is probed by, each with its dial addresses.
  pub peers: Vec<FleetPeer>,
}

/// What this node dials to reach one peer on one plane: the peer's host, the address on that plane, and the
/// certificate to pin; `name` is the fleet's TLS name the session is verified under.
struct PeerDial {
  host: HostId,
  name: String,
  address: SocketAddrV4,
  certificate: CertificateDer<'static>,
}

/// Derived: the sessions a serve socket's demultiplexer holds per peer — the live one and the one a re-dial
/// establishes to replace it (the old is closed once the new binds, so two suffice; a third dialer from
/// the same peer is refused typed until one releases).
const SESSIONS_PER_PEER: usize = 2;

/// The SWIM/Lifeguard timing for a neighbourhood of `neighbourhood` members (this node plus its peers),
/// derived from the design's stated formulas (§4.8 "Derived constants"): the base suspicion window is
/// [`SUSPICION_PERIODS`]; gossip disseminates `λ·ln(n+1)` times ([`GOSSIP_PER_BIT`] × the bit-length of
/// `n+1`); the local-health multiplier is capped at [`LOCAL_HEALTH_CAP`]; the confirmation curve is off
/// (`suspicion_min = suspicion_periods`) and one corroboration suffices, because a small fleet has no
/// indirect proxies to gather more, so the window is not held open waiting for confirmations that cannot
/// arrive.
fn detector_timing(neighbourhood: usize) -> DetectorTiming {
  // The bit-length of `n+1` (`⌊log2(n+1)⌋+1`): the word width less its leading zeros. `neighbourhood` is a
  // `usize`, so its width is `usize::BITS`; `saturating_sub` keeps the degenerate `n+1 = 1` at one bit.
  let bits = usize::BITS.saturating_sub(neighbourhood.saturating_add(1).leading_zeros());
  DetectorTiming {
    suspicion_periods: SUSPICION_PERIODS,
    gossip_transmits: bits.saturating_mul(GOSSIP_PER_BIT).max(1),
    health_max: LOCAL_HEALTH_CAP,
    suspicion_min: SUSPICION_PERIODS,
    confirmations_expected: 1,
  }
}

/// The probe budget: how long a probe waits for its acknowledgement before it is a failure, and how often
/// the wait is polled. Derived: the deadline is one protocol period ([`HEARTBEAT_NS`], the daemon's beat
/// cadence — "SWIM period = max(k × RTT p99, scheduler quantum)", §4.8), so a probe completes within its
/// period (it returns early on the acknowledgement); the poll interval is a tenth of it, so the loop wakes
/// promptly on the reply without spinning. A measured RTT budget is the owed refinement.
fn probe_budget() -> CommitBudget {
  CommitBudget::hard(HEARTBEAT_NS, (HEARTBEAT_NS / POLL_PER_PERIOD).max(1))
}

/// Runs the fleet membership loop for `transport` on the control shard (§4.8, boot step 6). It binds this
/// node's two serve sockets (one per plane, every peer on each through a demultiplexer) and spawns their
/// receive and accept loops; for each peer it dials the peer's advertised addresses and spawns the probe and
/// record-link tasks; then the one record-plane coordinator. A two-node fleet is the single-peer degenerate.
/// Detached tasks: they live as long as the shard and are cancelled by the runtime's shutdown.
pub async fn run_membership(transport: FleetTransport) {
  let FleetTransport {
    identity,
    name,
    probe_bind,
    record_bind,
    peers,
  } = transport;
  if peers.is_empty() {
    // No peers — nothing to probe; the placement path still runs the `FleetNode`, degenerate (R8).
    return;
  }
  // The identity is process-lifetime and shared by every serve session and client dial — it is not `Clone`
  // (it holds a private key), so it is leaked to `&'static` and each task borrows the one copy. One leak per
  // daemon boot.
  let identity: &'static Identity = Box::leak(Box::new(identity));
  let neighbourhood = peers.len().saturating_add(1); // this node and its peers.
  let local = state::with_state(|s| s.fleet.host()).unwrap_or(HostId(0));
  let budget = probe_budget();

  // One serve socket per plane, shared by every peer through a demultiplexer (bound here on the control
  // shard, before the dials, so a peer's dial finds a listener). A serve socket that cannot be bound (the
  // address in use, or refused) ends the membership loop — this node cannot be probed, so it cannot take
  // part — counted, never silent, so an operator reading the status refusal counts sees why (banned item 9).
  let (Ok(probe_socket), Ok(record_socket)) =
    (UdpSocket::bind(probe_bind), UdpSocket::bind(record_bind))
  else {
    count_refusal(BIND_REFUSED);
    return;
  };
  // The roster: which peer a certificate names — mutual TLS admits only these, and the record serve side
  // resolves the peer it authenticated through it.
  let roster: Vec<(CertificateDer<'static>, HostId)> = peers
    .iter()
    .map(|peer| (peer.certificate.clone(), peer.host))
    .collect();
  let allowed: Vec<CertificateDer<'static>> = roster.iter().map(|(cert, _)| cert.clone()).collect();
  let max_sessions = peers.len().saturating_mul(SESSIONS_PER_PEER);
  let probe_demux = Demux::start(
    probe_socket,
    identity,
    allowed.clone(),
    FLEET_FRAME_CAP,
    max_sessions,
  );
  let record_demux = Demux::start(
    record_socket,
    identity,
    allowed,
    FLEET_FRAME_CAP,
    max_sessions,
  );
  state::with_state(|s| s.demuxes = vec![probe_demux, record_demux]);
  for demux in [probe_demux, record_demux] {
    if let Ok(task) = futures::spawn(run_demux(demux)) {
      let _ = futures::detach(task);
    }
  }
  if let Ok(task) = futures::spawn(accept_probes(
    probe_demux,
    local,
    neighbourhood,
    roster.clone(),
  )) {
    let _ = futures::detach(task);
  }
  if let Ok(task) = futures::spawn(accept_records(record_demux, local, roster)) {
    let _ = futures::detach(task);
  }

  for peer in peers {
    let FleetPeer {
      host,
      address,
      record_address,
      certificate,
    } = peer;
    // The client sides each keep their own session up — the probe task the peer's probe address, the record
    // link task the record address — so a slow or not-yet-listening peer never blocks another peer's setup.
    let probe_dial = PeerDial {
      host,
      name: name.clone(),
      address,
      certificate: certificate.clone(),
    };
    let record_dial = PeerDial {
      host,
      name: name.clone(),
      address: record_address,
      certificate,
    };
    if let Ok(task) = futures::spawn(probe_peer(
      identity,
      probe_dial,
      local,
      neighbourhood,
      (probe_demux, record_demux),
    )) {
      let _ = futures::detach(task);
    }
    if let Ok(task) = futures::spawn(establish_record_link(identity, record_dial)) {
      let _ = futures::detach(task);
    }
  }

  // One record-plane coordinator for all peers (§4.8 "records are sent to all candidates"): it borrows every
  // holder session the link tasks keep up, so it ships each head to all candidates in one commit and drives
  // each takeover over all surviving holders (the `f > 1` promotion a per-peer ship task could not reach).
  if let Ok(task) = futures::spawn(run_record_plane(local, budget)) {
    let _ = futures::detach(task);
  }
}

/// A serve socket's receive loop as a task, for the daemon's life: it routes every datagram to its session.
/// The socket refusing ends it, counted (`fleet.serve`), so an operator sees a node that stopped accepting
/// rather than a mesh that silently never re-forms.
async fn run_demux(demux: &'static Demux) {
  if demux.run().await.is_err() {
    count_refusal(SERVE_REFUSED);
  }
}

/// Accepts every probe session a peer dials on the probe socket and serves it (§4.8): one serve task per
/// session, ended by the session's failure or its replacement by the peer's re-dial. Bounded by the
/// demultiplexer's session slots (`SESSIONS_PER_PEER` per peer): a task ends and releases its slot before
/// another session for the same peer can be opened past that.
async fn accept_probes(
  demux: &'static Demux,
  local: HostId,
  neighbourhood: usize,
  roster: Vec<(CertificateDer<'static>, HostId)>,
) {
  loop {
    let session = demux.accept().await;
    if let Ok(task) = futures::spawn(serve_peer_probes(
      session,
      local,
      neighbourhood,
      roster.clone(),
    )) {
      let _ = futures::detach(task);
    }
  }
}

/// Accepts every record session a peer dials on the record socket and serves it over this node's durable
/// holds (§4.8): one serve task per session, which resolves the peer it authenticated through `roster`.
/// Bounded as [`accept_probes`] is.
async fn accept_records(
  demux: &'static Demux,
  local: HostId,
  roster: Vec<(CertificateDer<'static>, HostId)>,
) {
  loop {
    let session = demux.accept().await;
    if let Ok(task) = futures::spawn(serve_peer_records(session, local, roster.clone())) {
      let _ = futures::detach(task);
    }
  }
}

/// Binds a fresh socket and builds an **un-established** client endpoint for `address` — the handshake is
/// driven separately ([`Endpoint::establish`]) so the caller can **retry it on this same socket** until the
/// peer's `accept` completes, rather than re-dialing from a fresh port. Retrying on one socket is what lets
/// a handshake finish under contention: `accept` pins the first source it hears, so its half-open state
/// waits for *this* source's next flight; a fresh-port re-dial is a new source it ignores, stranding the
/// session (the record-plane cause of the takeover flaking under load). `None` if the runtime refuses the
/// socket or endpoint.
fn client_for(
  identity: &Identity,
  name: &str,
  address: SocketAddrV4,
  certificate: &CertificateDer<'static>,
) -> Option<Endpoint> {
  let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).ok()?;
  Endpoint::client(
    socket,
    address,
    identity,
    certificate,
    name,
    FLEET_FRAME_CAP,
  )
  .ok()
}

/// Advances a session toward established, one handshake attempt per call (the probe and ship loops call it
/// each period): if a `session` is already up it is returned unchanged; otherwise the held un-established
/// `client` is driven one `establish` step **on its same socket** (kept for a retry on failure, so the
/// peer's pinned `accept` completes rather than a fresh-port re-dial being ignored — the establishment
/// fragility that flaked both the probe mesh's formation and the record plane's takeover under load), and a
/// client that was consumed but whose session was then lost is rebuilt. Returns the updated `(client, session)`.
async fn establish_session(
  client: Option<Endpoint>,
  session: Option<Endpoint>,
  identity: &Identity,
  name: &str,
  address: SocketAddrV4,
  certificate: &CertificateDer<'static>,
) -> (Option<Endpoint>, Option<Endpoint>) {
  if session.is_some() {
    return (client, session);
  }
  match client {
    Some(mut endpoint) => match endpoint.establish().await {
      Ok(()) => (None, Some(endpoint)),
      Err(_) => (Some(endpoint), None),
    },
    None => (client_for(identity, name, address, certificate), None),
  }
}

/// The serve side: complete the accepted session's handshake and loop answering the peer's probes (§4.8),
/// **re-admitting a peer that has come back**. A serve session's own detector builds the acknowledgement
/// gossip as before; on top of that, the authenticated prober's own liveness is folded into the shared
/// `FleetNode` and this node's belief *about the prober* is echoed in the reply — the SWIM rejoin path
/// (hyperscale's `reset_peer_for_rejoin`, realized here without a separate death tracker or incarnation
/// bump because [`Membership::refute`] already bumps past the death it hears). A serve failure ends this
/// loop, but the peer's re-dial opens a fresh session the accept loop serves, so a returning peer is always
/// answered.
///
/// **How a return heals.** This node retired peer B, so [`probe_peer`] idled and closed B's sessions. B is
/// alive (a false suspicion) or restarted, and its own probe reaches this node here. This node believes B
/// `Dead@N`, so the reply echoes `(B, Dead@N)`; B applies it to *itself*, refutes to `alive@N+1`
/// ([`Membership::refute`], which uses the death incarnation it heard, so even a restarted B at incarnation
/// zero jumps past `N`), and gossips that alive. B's next probe carries `(B, alive@N+1)`, which this fold
/// adopts (a higher incarnation overrides the death), re-admitting B — after which the idled [`probe_peer`]
/// sees B back in the neighbourhood and resumes. The fold is **scoped to the authenticated prober** (its own
/// state only), so it can never re-admit or flap a *third* peer another detector retired — the [`sync_peer`]
/// discipline. An unauthenticated prober (its certificate not in `roster`) is answered but never folded: a
/// ping's `from` is unauthenticated, so only the certificate the handshake proved may move membership.
async fn serve_peer_probes(
  mut endpoint: Endpoint,
  local: HostId,
  neighbourhood: usize,
  roster: Vec<(CertificateDer<'static>, HostId)>,
) {
  if endpoint.establish().await.is_err() {
    return;
  }
  let prober = endpoint.peer_certificate().and_then(|presented| {
    roster
      .iter()
      .find(|(certificate, _)| *certificate == presented)
      .map(|(_, host)| *host)
  });
  let timing = detector_timing(neighbourhood);
  let fanout = usize::try_from(timing.gossip_transmits).unwrap_or(1);
  let mut detector = Detector::new(local, timing);
  loop {
    let served = endpoint
      .serve_once(|_stream, request| match SwimMessage::decode(&request) {
        Ok(message) => {
          detector.apply_gossip_from(message.from(), message.gossip());
          if let Some(coordinate) = message.coordinate() {
            detector.learn_coordinate(message.from(), coordinate.clone());
          }
          let mut gossip = detector.gossip(fanout);
          if let Some(peer) = prober {
            state::with_state(|s| {
              // Fold the authenticated prober's own asserted liveness (scoped to it) — a higher incarnation
              // re-admits it, a stale one is ignored (`FleetNode::observe`'s incarnation-gated merge).
              if let Some(asserted) = message
                .gossip()
                .iter()
                .find(|(host, _)| *host == peer)
                .map(|(_, state)| *state)
              {
                apply_peer_state(&mut s.fleet, peer, Some(asserted));
              }
              // Echo this node's belief about the prober so a peer this node believes dead learns of it and
              // self-refutes. Only when not already carried and not `Alive` (an alive belief needs no echo).
              if let Some(belief) = s.fleet.membership().state(peer)
                && belief.liveness != Liveness::Alive
                && !gossip.iter().any(|(host, _)| *host == peer)
              {
                gossip.push((peer, belief));
              }
            });
          }
          SwimMessage::Ack {
            from: local,
            nonce: message.nonce().unwrap_or(0),
            gossip,
            coordinate: detector.coordinate(),
          }
          .encode()
        }
        Err(_) => Vec::new(),
      })
      .await;
    if served.is_err() {
      return;
    }
  }
}

/// At the top of a probe cycle, decides whether to probe this peer or idle. Returns `false` when the peer is
/// **retired** (not in the neighbourhood) — the caller idles the task (it does not end: a believed-dead peer
/// is never dialed, since that establish would block on a peer that will not answer, and no probe session is
/// held; when the peer rejoins — its own probe reaching this node's serve side re-admits it at a higher
/// incarnation, [`serve_peer_probes`] — the neighbourhood regains it and probing resumes). On the resume
/// from idle it realigns `detector` to the fleet's re-admitted belief, so the detector tracks the peer as
/// alive and can detect a *future* death rather than carrying its stale death forever. Returns `true` to
/// probe.
fn resume_if_in_mesh(detector: &mut Detector, peer_host: HostId, was_idle: &mut bool) -> bool {
  let in_mesh = state::with_state(|s| s.fleet.configuration().neighbourhood.contains(&peer_host))
    .unwrap_or(false);
  if !in_mesh {
    *was_idle = true;
    return false;
  }
  if *was_idle {
    if let Some(state) = state::with_state(|s| s.fleet.membership().state(peer_host)).flatten() {
      detector.apply(peer_host, state);
    }
    *was_idle = false;
  }
  true
}

/// Folds this peer's detector view into the shard's `FleetNode` membership and returns whether the peer is
/// now **retired** (out of the configuration's neighbourhood). Peer-scoped ([`sync_peer`], not
/// `sync_membership`): each peer has its own detector whose gossip carries other peers' states, so folding
/// the whole view would let one detector re-join a peer another has retired and flap it — scoped to this
/// peer, this node's belief about it sticks the moment its detector ages it out. This only advances the
/// **failure view**; the configuration is the regional council's (D-14), so a death drives no takeover here
/// — the council leader reconciles the retirement from the folded view and, when it commits, the record
/// plane installs the new configuration and takes over what fell to this node ([`sync_config_from_council`]).
/// The folded state is handed to every other shard's membership copy so all advance identically (D-7); a
/// spawn refused at a shard's admission bound is retried next period (the fold is idempotent). "Retired" is
/// read from the configuration, so it reflects the council's committed retirement, not one detector's
/// suspicion.
fn fold_peer_state(detector: &Detector, peer_host: HostId, origin: u16, shards: &[u16]) -> bool {
  let retired = state::with_state(|s| {
    sync_peer(detector.membership(), &mut s.fleet, peer_host);
    !s.fleet.configuration().neighbourhood.contains(&peer_host)
  })
  .unwrap_or(false);
  let peer_state = detector.membership().state(peer_host);
  for shard in shards.iter().copied().filter(|shard| *shard != origin) {
    let _ = run_on(origin, shard, move |s| {
      apply_peer_state(&mut s.fleet, peer_host, peer_state);
    });
  }
  retired
}

/// Sends one probe over `session` (when established) and folds its outcome into `detector`, returning the
/// session to carry forward. An acknowledgement clears the suspicion and learns the peer's gossip, RTT and
/// coordinate; a timeout is a **transient miss**, not a verdict — a lost packet, scheduling jitter, or a
/// nonce-rejected stale reply — so the session is kept and re-probed next period (only silence across the
/// suspicion window ages the peer to death), while the ping the next period carries this node's suspicion
/// for a still-live peer to refute; a runtime refusal (never expected from the inline probe) drops it. The
/// detector ticks only when a probe actually goes out, so an unestablished session never resolves as a miss.
#[allow(clippy::too_many_arguments)]
async fn probe_and_apply(
  detector: &mut Detector,
  session: Option<Endpoint>,
  local: HostId,
  peer_host: HostId,
  fanout: usize,
  nonce: u64,
  budget: CommitBudget,
) -> Option<Endpoint> {
  let open = session?;
  detector.tick();
  let ping = SwimMessage::Ping {
    from: local,
    nonce,
    gossip: detector.gossip(fanout),
  };
  match probe_once(open, &ping, budget).await {
    Ok((
      returned,
      ProbeOutcome::Acked {
        gossip,
        rtt_ns,
        coordinate,
      },
    )) => {
      detector.on_ack(peer_host);
      detector.apply_gossip(&gossip);
      #[allow(clippy::cast_precision_loss)]
      detector.observe_rtt(peer_host, rtt_ns as f64);
      detector.learn_coordinate(peer_host, coordinate);
      returned
    }
    Ok((returned, ProbeOutcome::TimedOut)) => returned,
    Err(_) => None,
  }
}

/// The probe side: dial the peer's probe address (so a slow or not-yet-listening peer never blocks the other
/// peers' loops — each probe task dials its own), then each protocol period probe the peer, fold the outcome,
/// and fold this detector's converged view into the shard's `FleetNode` (§4.8). Each peer has its own probe
/// task and its own detector; `sync_membership` folds each detector's view without disturbing the peers it
/// does not track, so N detectors compose into one membership. The session is **reused whatever a probe's
/// outcome** — an acknowledgement and a timeout both hand it back ([`probe_once`]) — so a single missed
/// probe (a lost packet, scheduling jitter, a nonce-rejected stale reply) does not drop it: the next period
/// re-probes, a still-live peer refutes the suspicion the ping carried, and only a peer silent across the
/// suspicion window ages to death and is retired (driving the takeover). Dropping the session on one miss
/// would retire a live peer on any transient glitch (`docs/bugs/2026-09-10-swim-stale-ack.md`); a re-dial
/// now replaces a lost session at the peer, but a probe verdict still rests on the suspicion window, not
/// on one miss.
async fn probe_peer(
  identity: &'static Identity,
  dial: PeerDial,
  local: HostId,
  neighbourhood: usize,
  demuxes: (&'static Demux, &'static Demux),
) {
  let PeerDial {
    host: peer_host,
    name,
    address,
    certificate,
  } = dial;
  let budget = probe_budget();
  let timing = detector_timing(neighbourhood);
  let fanout = usize::try_from(timing.gossip_transmits).unwrap_or(1);
  let mut detector = Detector::new(local, timing);
  detector.join(peer_host);
  // This shard (the control shard) probes; every other shard is an owner with its own copy of the
  // configuration (D-7), so each state this detector folds is handed to the rest as well.
  let (origin, shards) = state::with_state(|s| (s.shard, s.shards.clone())).unwrap_or_default();
  // Bring the probe session up on one socket, retrying the handshake there each period until it completes —
  // so a formation-race handshake that partially reached the peer's pinned `accept` finishes rather than
  // stranding the session (which would leave this peer unprobed and the N·(N−1) mesh un-formed). Once up it
  // is reused whatever a probe's outcome (see [`probe_once`]). The detector ticks only when a probe is
  // actually sent, so an as-yet-unestablished session never resolves as a missed probe and falsely ages the
  // peer.
  let mut client: Option<Endpoint> = client_for(identity, &name, address, &certificate);
  let mut session: Option<Endpoint> = None;
  let mut recorded_mesh = false;
  // A per-probe nonce the acknowledgement must echo: monotonic over this session, so every probe's nonce
  // is higher than any earlier one on it, and a *stale* acknowledgement the transport redelivered (from an
  // earlier probe, carrying a lower nonce) is rejected rather than counted as this probe's reply — the
  // fix for a survivor accepting a dead peer's buffered acknowledgements (`docs/bugs/2026-09-10-swim-stale-ack.md`).
  let mut probe_nonce: u64 = 0;
  // Set while this peer is retired, so the resume that follows realigns the detector to the re-admitted
  // belief before it probes again.
  let mut was_idle = false;

  loop {
    // Idle while this peer is retired; on the resume, the detector is realigned to the re-admitted belief.
    if !resume_if_in_mesh(&mut detector, peer_host, &mut was_idle) {
      futures::sleep(HEARTBEAT_NS).await;
      continue;
    }
    (client, session) =
      establish_session(client, session, identity, &name, address, &certificate).await;
    if session.is_some() && !recorded_mesh {
      // The direct probe session to this peer has formed — record it, so the daemon can tell the real mesh
      // is up (`fleet_meshed`) rather than trusting the membership's optimistically seeded alive set.
      state::with_state(|s| s.formed_probe_peers.insert(peer_host));
      recorded_mesh = true;
    }
    if session.is_some() {
      probe_nonce += 1;
      session = probe_and_apply(
        &mut detector,
        session.take(),
        local,
        peer_host,
        fanout,
        probe_nonce,
        budget,
      )
      .await;
    }

    let retired = fold_peer_state(&detector, peer_host, origin, &shards);
    if retired {
      // The peer is retired and gone from the direct mesh. Its objects' phase-one recovery is now driven by
      // the record-ship task (over the surviving candidate holders); the probe session is dropped and the
      // sessions it dialed into this node are closed so their serve tasks end and free their slots. The task
      // does **not** end — the top of the loop idles it until the peer rejoins, so a false retirement (or a
      // restart) heals without a supervisor re-spawning anything.
      state::with_state(|s| s.formed_probe_peers.remove(&peer_host));
      demuxes.0.close_peer(&certificate);
      demuxes.1.close_peer(&certificate);
      session = None;
      recorded_mesh = false;
    }
    futures::sleep(HEARTBEAT_NS).await;
  }
}

/// The record serve side (§4.8 "records are sent to all candidates"; "Promotion and takeover"): complete
/// the accepted session's handshake and loop answering the peer over this node's per-object holds — a
/// **record** commit is accepted into the object's durable acceptor (this node backs the peer as a
/// candidate holder), and a **prepare** (a new owner's phase-one message when it takes over an object) is
/// answered from that same hold with a binding promise. Both are served here because the session plane
/// carries both and `serve_once` hands the handler the raw request either way; the two never collide, a
/// [`Prepare`] being a fixed 40 bytes and a [`Record`] always longer (its prefix alone exceeds that), so
/// the length disambiguates. The acceptor authorizes the record's or prepare's owner and generation against
/// the authority the takeover installed, fences the epoch, and refuses a stale writer. The peer is whoever
/// the handshake authenticated — its certificate names it in `roster` (mutual TLS admits only roster
/// certificates; one not found is counted, never served). A serve failure (the peer's connection dropped
/// when it died, or its session replaced by a re-dial) ends the loop.
async fn serve_peer_records(
  mut endpoint: Endpoint,
  local: HostId,
  roster: Vec<(CertificateDer<'static>, HostId)>,
) {
  if endpoint.establish().await.is_err() {
    return;
  }
  let Some(peer_host) = endpoint.peer_certificate().and_then(|presented| {
    roster
      .iter()
      .find(|(certificate, _)| *certificate == presented)
      .map(|(_, host)| *host)
  }) else {
    count_refusal(ACCEPT_REFUSED);
    return;
  };
  // Serve the peer's commits and prepares against this node's **durable** per-object holds in the shard
  // state, so an accepted record survives past this task — the state a survivor's phase-one recovery reads
  // on a takeover — and a prepare is answered from it. The handler runs synchronously inside `serve_once` (a
  // brief `with_state` borrow, no await held across it). A serve failure ends the loop.
  // Each request kind rides its own stream id, so the dispatch is by kind: a record commit, a phase-one
  // prepare, or a content exchange (an offer, a put, a fetch — §4.10) — never a guess from the bytes.
  loop {
    let served = endpoint
      .serve_once(|stream, request| match stream {
        RECORD_STREAM => Record::decode(&request)
          .ok()
          .and_then(|record| {
            state::with_state(|s| accept_held_record(s, local, peer_host, &record))
          })
          .unwrap_or_default(),
        PROMOTE_STREAM => Prepare::decode(&request)
          .ok()
          .and_then(|prepare| state::with_state(|s| serve_held_promotion(s, &prepare)))
          .unwrap_or_default(),
        stream if is_content_stream(stream) => {
          state::with_state(|s| s.held_content.serve(local, &request)).unwrap_or_default()
        }
        CONFIG_STREAM => state::with_state(|s| serve_council(s, &request)).unwrap_or_default(),
        CONFIG_FETCH_STREAM => {
          state::with_state(|s| serve_config_fetch(s, &request)).unwrap_or_default()
        }
        ROOT_STREAM => state::with_state(|s| serve_root(s, &request)).unwrap_or_default(),
        ROOT_FETCH_STREAM => {
          state::with_state(|s| serve_root_fetch(s, &request)).unwrap_or_default()
        }
        FORWARD_STREAM => {
          state::with_state(|s| verbs::serve_forward(s, &request)).unwrap_or_default()
        }
        _ => Vec::new(),
      })
      .await;
    if served.is_err() {
      return;
    }
  }
}

/// Answers a new owner's phase-one [`Prepare`] from this node's durable hold of the object (§4.8 "every
/// holder raises its fence for that host to the new epoch and reports the highest record it holds"): runs
/// the prepare through the object's acceptor — which authorizes the new owner and generation the takeover
/// installed ([`reconcile_held_authority`]), raises the fence to the prepare's epoch, and reports the
/// highest record this node holds for the object — and replies with the binding [`Promise`], or an empty
/// reply if this node holds nothing for the object or the acceptor refuses (a foreign generation, an
/// unauthorized owner, or an epoch below the fence), so the new owner counts nothing.
fn serve_held_promotion(state: &mut ShardState, prepare: &Prepare) -> Vec<u8> {
  match state.holder_records.get_mut(&prepare.object) {
    Some(acceptor) => match acceptor.prepare(prepare) {
      Ok(promise) => promise.encode(),
      Err(_) => Vec::new(),
    },
    None => Vec::new(),
  }
}

/// Brings every held acceptor's authority into step with the routing view (§4.8 "distributing the
/// taken-over authority to the holders"): for each object this node holds a copy of, installs the object's
/// current owner (as the routing view records it) under the configuration generation. On a takeover the
/// routing owner moved to the survivor rendezvous ranks first, so this installs the successor as the
/// authorized owner — the object's new owner can then commit through this hold and its prepare authorizes.
/// Every survivor computes the same routing winner, so this needs no coordination. Idempotent: an install
/// of the same owner under an equal-or-higher generation is accepted, a lower one refused (ignored), so the
/// authority only advances.
fn reconcile_held_authority(state: &mut ShardState) {
  let generation = state.fleet.configuration().version;
  let held: Vec<ObjectId> = state.holder_records.keys().copied().collect();
  for object in held {
    if let Some(owner) = state.fleet.object_owner(object)
      && let Some(acceptor) = state.holder_records.get_mut(&object)
    {
      let _ = acceptor.install_authority(Authority { generation, owner });
    }
  }
}

/// Accepts one register record this node holds as a **candidate holder** for the peer that owns it, into
/// the object's durable acceptor in the shard state (§4.8 "records are sent to all candidates"). The
/// acceptor is created on the object's first record under the authority `{owner: peer, generation: the
/// configuration version}` — `peer` is the socket's TLS-authenticated identity, so a record whose owner
/// field is not this socket's peer is refused [`Unauthorized`](slates_db::register::RegisterError) by
/// [`Acceptor::accept`], and one under a foreign generation or a stale epoch is refused likewise. On
/// acceptance the object is tracked in the routing view as backed for that owner, so the owner's death
/// hands [`sync_peer`]'s takeover computation this object. Returns the binding acknowledgement's bytes; a
/// sender on an *older* configuration is refused `ConfigurationStale` carrying this node's newer version so
/// it refreshes and retries (§4.8 the piggyback rule), and every other refusal is an empty reply the owner
/// counts as no acknowledgement. A record naming a *newer* configuration than this node's flags a reactive
/// refresh (this node is the one behind) before it is refused.
fn accept_held_record(
  state: &mut ShardState,
  local: HostId,
  peer_host: HostId,
  record: &Record,
) -> Vec<u8> {
  let generation = state.fleet.configuration().version;
  // The reactive piggyback, holder-behind direction (§4.8 "every fleet request carries the sender's
  // configuration version"): a record naming a newer generation than this node's own is evidence this node's
  // configuration is stale. Flag a refresh so the coordinator fetches the committed configuration from a
  // voter next period; the record itself is still refused below (this holder cannot validate authority under
  // a generation it has not installed), and the owner re-ships it once this node has caught up.
  if record.generation > state.council.configuration().version {
    state.config_refresh_wanted = true;
  }
  let accepted = {
    let acceptor = state
      .holder_records
      .entry(record.object)
      .or_insert_with(|| {
        Acceptor::new(
          local,
          Authority {
            generation,
            owner: peer_host,
          },
        )
      });
    acceptor.accept(record)
  };
  match accepted {
    Ok(ack) => {
      state.fleet.track_object(record.object, peer_host);
      ack.encode()
    }
    // A refusal the sender acts on rides back on the wire (a `ConfigurationStale` naming this node's newer
    // version, so a stale sender refreshes and retries); every other refusal is an empty reply the sender
    // counts as no acknowledgement ([`encode_refusal`]).
    Err(error) => encode_refusal(&error),
  }
}

/// A volume head this node owns that is not yet held by every candidate, with everything the register commit
/// needs and the candidates that already acknowledged it (so the coordinator ships only to the rest).
struct Head {
  /// The shard that owns the volume — where its placement is recorded.
  shard: u16,
  object: ObjectId,
  record: Record,
  candidates: Vec<HostId>,
  acked: Vec<HostId>,
  quorum: Quorum,
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
    let object = ObjectId(slot.id.bytes);
    let Some((sequence, value)) = head_value_of(state, record, object, config.quorum) else {
      continue; // The newest seal's content is not yet placed: the head waits for it.
    };
    let placement = config.place(object);
    let recorded = state.placed_heads.get(&object);
    let acked = recorded
      .filter(|head| head.sequence == sequence)
      .map(|head| head.placement.acked.clone())
      .unwrap_or_default();
    // The head is written at the greater of the host's epoch and the epoch the object was last written or
    // adopted under: a taken-over object's holders fenced it at its promotion epoch (see `PlacedHead`).
    let epoch = recorded
      .map_or(config.host_epoch, |head| head.epoch)
      .max(config.host_epoch);
    // Every remote candidate that has not acked still needs the head; when none remain the head is done.
    let outstanding = placement
      .candidates
      .iter()
      .any(|candidate| *candidate != local && !acked.contains(candidate));
    if !outstanding {
      continue;
    }
    heads.push(Head {
      shard: state.shard,
      object,
      record: Record {
        owner: local,
        object,
        sequence,
        epoch,
        generation: config.version,
        value: value.to_record_bytes(),
      },
      candidates: placement.candidates,
      acked,
      quorum: config.quorum,
    });
  }
  heads
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
  let value = |manifest: Option<[u8; 32]>, content_holders: Vec<u64>| HeadValue {
    manifest,
    content_holders,
    name: record.name.clone(),
    size: record.policy.size,
    names: record.policy.names,
    owner: record.owner.clone(),
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

/// A seal whose archive is complete and whose content is not yet placed: the put to run this period.
struct ContentWork {
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
}

/// The status refusal count under which the fleet loop records a snapshot it could not archive (a
/// base-backed entry whose bytes are on disk, not in RAM; a torn read): the seal stays `Local`, reported.
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
const SEAL_REFUSED: &str = "fleet.seal";

/// One period of this node's seals (§4.10 "auto-seal" → content replication; §4.3 bounded slices): for each
/// owned volume whose newest snapshot is not yet placed, a seal job is started (or replaced, when a newer
/// snapshot superseded the one being sealed), its archive walk is advanced one slice of `slice_bytes`, and —
/// once the archive is complete and its content not yet placed — the content put to run this period is
/// returned, its archive moved out for the dispatch and put back after ([`put_seal_content`]). A seal whose
/// snapshot the database already records placed is dropped (a restart resumes nothing it need not).
fn advance_seals(
  state: &mut ShardState,
  local: HostId,
  slice_bytes: u64,
  created_unix: u64,
) -> Vec<ContentWork> {
  let quorum = state.fleet.configuration().quorum;
  let mut work = Vec::new();
  let volumes: Vec<(DbVolumeId, slates_mem::Handle<crate::state::VolumeSlot>)> =
    state.by_id.iter().map(|(id, h)| (*id, *h)).collect();
  for (id, handle) in volumes {
    let object = ObjectId(id.bytes);
    let Some((head, sequence)) = sealable_head(state, id, object) else {
      continue;
    };
    if !state.seals.contains_key(&object)
      && !start_seal(state, local, object, handle, head, sequence, created_unix)
    {
      continue;
    }
    if !advance_seal(state, object, handle, slice_bytes) {
      continue;
    }
    if let Some(item) = content_work(state, object, quorum) {
      work.push(item);
    }
  }
  work
}

/// The head snapshot of `id` that still needs sealing — its id and the head sequence — or `None` when the
/// volume has no snapshot yet, or its head is already recorded placed (a finished seal is dropped). A seal in
/// progress for an older snapshot is dropped too: the newer snapshot supersedes it.
fn sealable_head(
  state: &mut ShardState,
  id: DbVolumeId,
  object: ObjectId,
) -> Option<(DbSnapshotId, u64)> {
  let record = state.db.partition().volume(id)?;
  if record.epoch == 0 {
    return None; // Nothing sealed yet: the creation head names no content.
  }
  let (head, sequence) = (record.head, record.epoch);
  let placed = state
    .db
    .partition()
    .snapshot(id, head)
    .is_some_and(|snapshot| matches!(snapshot.placed, PlacementState::Placed { .. }));
  if placed {
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
/// counted, if the snapshot cannot be walked.
fn start_seal(
  state: &mut ShardState,
  local: HostId,
  object: ObjectId,
  handle: slates_mem::Handle<crate::state::VolumeSlot>,
  head: DbSnapshotId,
  sequence: u64,
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
  );
  let Ok(archiver) = archiver else {
    *state.refusals.entry(SEAL_REFUSED).or_insert(0) += 1;
    return false;
  };
  let mut content = state.fleet.configuration().place(object);
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
      archiver: Some(archiver),
      archive: None,
      manifest: None,
      content,
      rounds: 0,
    },
  );
  true
}

/// Advances `object`'s seal one slice of `slice_bytes`; `true` once its archive is complete (this slice or
/// an earlier one). A snapshot that cannot be archived whole (an overlay's base bytes are not in RAM) stays
/// `Local`, counted so an operator sees why it never places (§4.4 coverage is never upgraded).
fn advance_seal(
  state: &mut ShardState,
  object: ObjectId,
  handle: slates_mem::Handle<crate::state::VolumeSlot>,
  slice_bytes: u64,
) -> bool {
  let Some(job) = state.seals.get_mut(&object) else {
    return false;
  };
  let Some(archiver) = job.archiver.as_mut() else {
    return true;
  };
  let Ok(slot) = state.volumes.get(handle) else {
    return false;
  };
  match archiver.advance(&slot.volume, &state.store, slice_bytes) {
    Ok(Progress::More) => false,
    Ok(Progress::Done(archive)) => {
      job.manifest = Some(archive.manifest.identity());
      job.archive = Some(archive);
      job.archiver = None;
      true
    }
    Err(_) => {
      job.archiver = None;
      *state.refusals.entry(SEAL_REFUSED).or_insert(0) += 1;
      false
    }
  }
}

/// The content put `object`'s seal calls for this period — its archive moved out for the dispatch — or
/// `None` once the content is placed (the head naming it then ships through `unplaced_heads`) or while
/// the archive is out on a put.
fn content_work(state: &mut ShardState, object: ObjectId, quorum: Quorum) -> Option<ContentWork> {
  let job = state.seals.get_mut(&object)?;
  if job.content.placed(quorum) {
    return None;
  }
  let archive = job.archive.take()?;
  Some(ContentWork {
    shard: state.shard,
    object,
    snapshot: job.snapshot,
    sequence: job.sequence,
    archive,
    candidates: job.content.candidates.clone(),
    acked: job.content.acked.clone(),
    quorum,
    round: job.rounds,
  })
}

/// Runs one content round for a seal (§4.8 mechanism 1: "content is sent to `f + 1` candidates first,
/// hedged to the remaining candidates after the measured p95 put latency"): the first round goes to the
/// first `f` remote candidates in rendezvous order (the owner is the `f + 1`-th copy), every later round
/// hedges to all remaining candidates — the round's own deadline is the hedge trigger until the p95 is
/// measured (owed). The holders' sessions are borrowed for the put and returned; the acknowledging set is
/// merged into the seal whatever the round's outcome (each acknowledgement is a distinct holder's verified,
/// durable hold), and the archive is put back for the next round. Returns the dispatch to settle.
async fn put_seal_content(
  origin: u16,
  local: HostId,
  work: ContentWork,
  budget: CommitBudget,
) -> Option<Dispatch> {
  let hedge = usize::try_from(work.quorum.f).unwrap_or(0);
  let remote: Vec<HostId> = work
    .candidates
    .iter()
    .copied()
    .filter(|host| *host != local && !work.acked.contains(host))
    .collect();
  let targets: Vec<HostId> = if work.round == 0 {
    remote.into_iter().take(hedge).collect()
  } else {
    remote
  };
  let holders = take_sessions(|host| targets.contains(&host));
  let (placement, dispatch) = if holders.is_empty() {
    (None, None)
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
    let dispatch = Dispatch::new(taken, &placed.reusable, placed.stragglers);
    return_sessions(placed.reusable);
    let placement = match placed.outcome {
      Ok(placement) => Some(placement),
      Err(ClusterError::Uncertain { placement } | ClusterError::NotPlaced { placement }) => {
        Some(placement)
      }
      Err(_) => None,
    };
    (placement, Some(dispatch))
  };
  // The seal lives on the owner shard: put the archive back and merge the round's acknowledgements there.
  let ContentWork {
    shard,
    object,
    snapshot,
    archive,
    ..
  } = work;
  let _ = run_on(origin, shard, move |s| {
    let Some(job) = s.seals.get_mut(&object) else {
      return; // The seal was superseded meanwhile; its archive is dropped with it.
    };
    if job.snapshot != snapshot {
      return;
    }
    job.archive = Some(archive);
    if let Some(placement) = placement {
      job.rounds = job.rounds.saturating_add(1);
      for host in placement.acked {
        if !job.content.acked.contains(&host) {
          job.content.acked.push(host);
        }
      }
    }
  });
  dispatch
}

/// Records durably each seal whose content **and** head have both placed (§4.10; AC-8.2 "no head record names
/// content that is not placed"): the snapshot's manifest identity (`SnapshotIdentified`) and its region
/// placement (`SnapshotPlaced`, the content holders), the facts `status` and `await placed(region)` answer
/// from and a restart resumes from. The seal is then dropped — its archive with it; the volume holds the
/// bytes, and its holders serve them by identity. Runs on the owner shard, where the seal and the volume's
/// database partition live.
fn record_placed_seals(state: &mut ShardState) {
  let quorum = state.fleet.configuration().quorum;
  let now = state.clock.monotonic_ns();
  let done: Vec<(ObjectId, DbSnapshotId, [u8; 32], Vec<u64>)> = state
    .seals
    .iter()
    .filter_map(|(object, job)| {
      let identity = job.manifest?;
      if !job.content.placed(quorum) {
        return None;
      }
      let head = state.placed_heads.get(object)?;
      if head.sequence != job.sequence || !head.placement.placed(quorum) {
        return None;
      }
      let region = job.content.acked.iter().map(|host| host.0).collect();
      Some((*object, job.snapshot, identity, region))
    })
    .collect();
  for (object, snapshot, identity, region) in done {
    let volume = DbVolumeId { bytes: object.0 };
    let ops = [
      Op::SnapshotIdentified {
        volume,
        id: snapshot,
        identity,
      },
      Op::SnapshotPlaced {
        volume,
        id: snapshot,
        placed: PlacementState::Placed {
          region,
          mirror: None,
        },
      },
    ];
    for op in &ops {
      if state.db.mutate(&mut state.segment, op, now).is_err() {
        break; // The snapshot is gone (destroyed meanwhile): nothing to record; the seal is dropped.
      }
    }
    state.seals.remove(&object);
  }
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
async fn materialize_pending(origin: u16, budget: CommitBudget) {
  let pending: Vec<(ObjectId, HeadValue)> = state::with_state(|s| {
    s.pending_materializations
      .iter()
      .map(|(object, head)| (*object, head.clone()))
      .collect()
  })
  .unwrap_or_default();
  for (object, head) in pending {
    let Some(manifest) = head.manifest else {
      // A head naming no content (the volume was never sealed): nothing to serve beyond the head.
      state::with_state(|s| s.pending_materializations.remove(&object));
      continue;
    };
    let held = state::with_state(|s| s.held_content.holds_manifest(&manifest)).unwrap_or(false);
    if !held {
      let holders = head.holders();
      let mut sessions = take_sessions(|host| holders.contains(&host));
      let Some((host, endpoint)) = sessions.pop() else {
        return_sessions(sessions);
        continue; // No recorded holder reachable this period.
      };
      return_sessions(sessions);
      let (archive, endpoint) = fetch_content(endpoint, manifest, budget.max_deadline_ns()).await;
      return_sessions(vec![(host, endpoint)]);
      let Some(archive) = archive else {
        continue;
      };
      let stored = state::with_state(|s| s.held_content.hold(archive).is_ok()).unwrap_or(false);
      if !stored {
        count_refusal(MATERIALIZE_REFUSED);
        continue;
      }
    }
    materialize(origin, object, head).await;
  }
}

/// Materializes one taken-over volume from the content this node holds for its head (see
/// [`materialize_pending`]) **on the shard the volume's id routes to** — the partition its id names is
/// where every verb for it will run (`verbs::owner_of`), so that is where it must live. The archive and
/// the head move there by value; on success the object is no longer pending, on a refusal (or a shard
/// that does not answer this period) it is counted and stays pending.
async fn materialize(origin: u16, object: ObjectId, head: HeadValue) {
  let Some(manifest) = head.manifest else {
    return;
  };
  let id = DbVolumeId { bytes: object.0 };
  let partition = verbs::owner_of(slates_ipc::protocol::VolumeId { bytes: object.0 });
  let taken = state::with_state(|s| {
    let archive = s.held_content.archive_of(&manifest)?;
    // The takeover's placement of the head (its sequence, promotion epoch and acknowledging holders),
    // recorded here where the promotion ran; it moves to the owner shard with the volume.
    let placed = s.placed_heads.get(&object).cloned()?;
    // The partition the id names, as this daemon's runtime shard (the shard list is in partition order).
    let target = s.shards.get(usize::from(partition)).copied()?;
    Some((archive, placed, target))
  })
  .flatten();
  let Some((archive, placed, target)) = taken else {
    return;
  };
  let region = head.content_holders.clone();
  let served = call_within(
    origin,
    target,
    move |s| {
      let sequence = placed.sequence;
      let served = verbs::materialize_taken_over(s, id, &head, sequence, region, &archive).is_ok();
      if served {
        // The owner shard now owns the head's placement — its record plane writes the object's next
        // heads at the promotion epoch and ships only to holders still missing this one.
        s.placed_heads.insert(object, placed);
      }
      served
    },
    HEARTBEAT_NS,
  )
  .await;
  state::with_state(|s| {
    if served == Some(true) {
      s.pending_materializations.remove(&object);
    } else {
      *s.refusals.entry(MATERIALIZE_REFUSED).or_insert(0) += 1;
    }
  });
}

/// The status refusal count under which the fleet loop records that its serve sockets could not be bound at
/// boot (§4.14: a refusal is counted, never silent) — the node then takes no part in the fleet.
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
const BIND_REFUSED: &str = "fleet.bind";

/// The status refusal count under which the fleet loop records an accepted session whose authenticated
/// peer is not in the roster (never served).
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
const ACCEPT_REFUSED: &str = "fleet.accept";

/// The status refusal count under which the fleet loop records a serve socket whose receive loop ended
/// because the socket refused — the node no longer accepts sessions on that plane.
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
const SERVE_REFUSED: &str = "fleet.serve";

/// Counts a fleet-loop refusal in the shard's status refusal counts, so a peer the loop could not set up is
/// visible to an operator (the mesh will not form to it) rather than a swallowed error (banned item 9).
fn count_refusal(kind: &'static str) {
  state::with_state(|s| *s.refusals.entry(kind).or_insert(0) += 1);
}

/// Keeps one candidate holder's client record session up for the coordinator (§4.8): a per-peer task that
/// brings the session up on **one** socket — one handshake attempt per period, retried until the peer's
/// pinned `accept` completes rather than a fresh-port re-dial being ignored — and installs it in the shard
/// state ([`ShardState::record_sessions`]), where the coordinator borrows it for each dispatch. If the
/// coordinator ever loses it (a borrow that ended without a return), the entry is gone and this task
/// re-establishes on a fresh socket. Per peer — never in the coordinator — because a handshake attempt to a
/// peer that is slow to come up is bounded but long (the retransmit ceiling), and in the coordinator it would
/// stall every other peer's commits and every takeover behind one slow link. Ends when the peer is retired
/// from the neighbourhood, dropping its session (a retired peer is never a candidate again under this
/// configuration), so it is not an unbounded retry of a dead peer (banned item 8).
async fn establish_record_link(identity: &'static Identity, dial: PeerDial) {
  let PeerDial {
    host: peer_host,
    name,
    address,
    certificate,
  } = dial;
  let mut client: Option<Endpoint> = client_for(identity, &name, address, &certificate);
  loop {
    let retired =
      state::with_state(|s| !s.fleet.configuration().neighbourhood.contains(&peer_host));
    if retired == Some(true) {
      // The peer is retired. Drop its record session and **idle** — this task does not end, so if the peer
      // rejoins (its probe reaches this node's serve side, which re-admits it — [`serve_peer_probes`]) this
      // loop sees it back in the neighbourhood and re-establishes. A believed-dead peer is never dialed
      // (that establish would block on a peer that will not answer, `docs/bugs/2026-09-10-*`), so idling
      // costs nothing until the peer is alive again.
      state::with_state(|s| s.record_sessions.remove(&peer_host));
      futures::sleep(HEARTBEAT_NS).await;
      continue;
    }
    // Absent means no session (never established, or lost); a `None` entry means the coordinator has it out
    // on a dispatch — not this task's to touch.
    let absent =
      state::with_state(|s| !s.record_sessions.contains_key(&peer_host)).unwrap_or(false);
    if absent {
      let (kept, session) =
        establish_session(client, None, identity, &name, address, &certificate).await;
      client = kept;
      if let Some(session) = session {
        state::with_state(|s| s.record_sessions.insert(peer_host, Some(session)));
      }
    }
    futures::sleep(HEARTBEAT_NS).await;
  }
}

/// A dispatch the coordinator made whose holders may still be in flight: the [`Stragglers`] to recover
/// sessions from, and the holders borrowed for it that have not yet come back (each returns through the
/// dispatch's `reusable` at its return, or through the stragglers later). Once the stragglers are spent, a
/// holder still outstanding never returned its session: it is dropped from the shard state as lost, so its
/// link task re-establishes it — a borrow that ended without a return is a loss by definition.
struct Dispatch {
  stragglers: Stragglers,
  outstanding: Vec<HostId>,
}

impl Dispatch {
  /// Records a dispatch over the `taken` holders, of which `reusable` came back at its return.
  fn new(taken: Vec<HostId>, reusable: &[(HostId, Endpoint)], stragglers: Stragglers) -> Self {
    let outstanding = taken
      .into_iter()
      .filter(|host| !reusable.iter().any(|(returned, _)| returned == host))
      .collect();
    Self {
      stragglers,
      outstanding,
    }
  }

  /// Recovers whatever stragglers have finished into the shard state; `true` once the dispatch is spent
  /// (every straggler accounted for, any holder still outstanding marked lost) and can be dropped.
  fn settle(&mut self) -> bool {
    let (recovered, done) = self.stragglers.recover();
    self
      .outstanding
      .retain(|host| !recovered.iter().any(|(returned, _)| returned == host));
    return_sessions(recovered);
    if done {
      let lost = std::mem::take(&mut self.outstanding);
      state::with_state(|s| {
        for host in &lost {
          s.record_sessions.remove(host);
        }
      });
    }
    done
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
const FORWARD_STREAM: u64 = 11;

/// The configuration council's election timeout, in record-plane heartbeat periods: a follower that goes
/// this many periods without a leader's append presumes the leader gone and campaigns. The per-node spread
/// ([`election_jitter`]) adds a further `[0, this)`, making the effective timeout uniform in `[this, 2·this)`
/// — Raft's randomized-election-timeout range (§9.3), which keeps co-timed followers from splitting the vote.
/// Derived: ten is Raft's order-of-magnitude ratio of election timeout to heartbeat interval (Ongaro §9), so
/// a live leader's per-period heartbeat refreshes contact well inside the window while a real leader loss is
/// still detected within a bounded few periods.
const ELECTION_HEARTBEATS: u32 = 10;

/// A deterministic per-node offset in `[0, ELECTION_HEARTBEATS)` added to the election timeout so two
/// followers that lose the leader in the same period do not campaign in lockstep and split the vote — the
/// determinism-clean analogue of Raft's randomized election timeout (§9.3), reproducible in the simulator.
/// It rotates each `attempt`, so a persistent split (two ids that collide modulo the window) breaks within a
/// few attempts rather than by luck.
fn election_jitter(local: HostId, attempt: u32) -> u32 {
  let window = u64::from(ELECTION_HEARTBEATS);
  u32::try_from(local.0.wrapping_add(u64::from(attempt)) % window).unwrap_or(0)
}

/// Answers one configuration-council Raft message a peer shipped on [`CONFIG_STREAM`] (§4.8, D-14): the
/// council serves a pre-vote, a vote request, or an append — applying whatever an append newly commits to
/// the regional configuration and refreshing this node's leader contact — and returns the reply to ship
/// back. A reply-typed or malformed message is answered with nothing (the sender counts no reply). Runs
/// synchronously inside `serve_once`, no await held across it.
fn serve_council(state: &mut ShardState, request: &[u8]) -> Vec<u8> {
  match RaftMessage::decode(request) {
    Ok(message) => state
      .council
      .answer(message)
      .map(|reply| reply.encode())
      .unwrap_or_default(),
    Err(_) => Vec::new(),
  }
}

/// Answers a root-group Raft request received over the transport ([`ROOT_STREAM`]) — the cross-region
/// counterpart of [`serve_council`]: runs it through this node's [`RootGroup`](slates_cluster::root_group::RootGroup),
/// returning the reply to ship back and applying whatever newly committed to the root configuration. A
/// reply-typed or malformed message is answered with nothing.
fn serve_root(state: &mut ShardState, request: &[u8]) -> Vec<u8> {
  match RaftMessage::decode(request) {
    Ok(message) => state
      .root
      .answer(message)
      .map(|reply| reply.encode())
      .unwrap_or_default(),
    Err(_) => Vec::new(),
  }
}

/// Answers a **root learner**'s fetch (§4.8, D-14) — the cross-region parallel of [`serve_config_fetch`]: the
/// request carries the learner's root configuration version, and this root voter returns its committed root
/// configuration only when it holds a newer one (a caught-up learner's fetch is then an empty reply, not a
/// full transfer).
fn serve_root_fetch(state: &ShardState, request: &[u8]) -> Vec<u8> {
  let learner_version = request
    .get(..std::mem::size_of::<u64>())
    .and_then(|bytes| <[u8; std::mem::size_of::<u64>()]>::try_from(bytes).ok())
    .map(u64::from_le_bytes)
    .unwrap_or(0);
  let config = state.root.configuration();
  if config.version > learner_version {
    encode_root_configuration(config)
  } else {
    Vec::new()
  }
}

/// Answers a **learner**'s configuration fetch on [`CONFIG_FETCH_STREAM`] (§4.8, D-14): a non-voter member
/// asks this node for the committed regional configuration, sending its own current version; this returns
/// the configuration encoded **only if this node's is newer**, else an empty reply. So a caught-up learner's
/// periodic fetch costs an eight-byte request and an empty reply rather than a full-configuration transfer —
/// most fetches are no-ops at the near-zero config-commit rate. A malformed or short request is treated as
/// version zero (the full configuration is returned). Runs synchronously inside `serve_once`.
fn serve_config_fetch(state: &ShardState, request: &[u8]) -> Vec<u8> {
  let learner_version = request
    .get(..std::mem::size_of::<u64>())
    .and_then(|bytes| <[u8; std::mem::size_of::<u64>()]>::try_from(bytes).ok())
    .map(u64::from_le_bytes)
    .unwrap_or(0);
  let config = state.council.configuration();
  if config.version > learner_version {
    encode_regional_configuration(config)
  } else {
    Vec::new()
  }
}

/// Ships each `(host, request)` to that host on [`CONFIG_STREAM`] over its borrowed record session,
/// concurrently — one child task per session, each bounded by the dispatch deadline and handing its endpoint
/// back whatever the outcome ([`request_within`]) — and returns every reply with its endpoint, so a slow or
/// dead voter never serializes the reachable ones and no session is dropped. The council is small and
/// near-silent, but a heartbeat to a dead follower must not delay the live ones, so the fan-out is the same
/// concurrent, session-preserving shape as a record commit ([`commit_record`]). Each child is joined once
/// terminal, so a per-period round never accumulates task slots (banned item 8).
async fn broadcast(
  requests: Vec<(HostId, Vec<u8>, Endpoint)>,
  stream: u64,
  budget: CommitBudget,
) -> Vec<(HostId, Vec<u8>, Endpoint)> {
  if requests.is_empty() {
    return Vec::new();
  }
  let deadline_ns = budget.max_deadline_ns();
  let (tx, rx) = channel::<(HostId, Vec<u8>, Endpoint)>();
  let mut tasks = Vec::new();
  for (host, request, endpoint) in requests {
    let tx = tx.clone();
    if let Ok(task) = futures::spawn_child(async move {
      let (reply, endpoint) = request_within(endpoint, stream, &request, deadline_ns).await;
      let _ = tx.send((host, reply, endpoint));
    }) {
      tasks.push(task);
    }
    // A spawn failure drops the cloned `tx` and the moved endpoint: that voter yields no reply this round
    // (its link task re-establishes the session), and the channel still disconnects once the rest end.
  }
  drop(tx); // so the channel disconnects when the last child has reported
  let mut replies = Vec::with_capacity(tasks.len());
  let poll_ns = (deadline_ns / POLL_PER_PERIOD).max(1);
  loop {
    match rx.try_recv() {
      Ok(triple) => replies.push(triple),
      // The children always report within the deadline; park a poll interval between wake-ups.
      Err(TryRecvError::Empty) => futures::sleep(poll_ns).await,
      // Every child has reported and dropped its sender: the round is complete.
      Err(TryRecvError::Disconnected) => break,
    }
  }
  // Reap the now-terminal children (all senders are gone, so each has finished): a joinable child of this
  // perpetual coordinator would otherwise linger in the arena. The joins are immediate.
  for task in tasks {
    let _ = futures::join(task).await;
  }
  replies
}

/// Drives one replication round as the council **leader**: ships each other voter the append it is owed (a
/// heartbeat, or the entries it still lacks) over its borrowed record session, concurrently, and folds each
/// reply — the leader advances its commit index as a majority acknowledge, and the regional configuration
/// applies whatever newly commits. Borrows only the voter sessions; a voter with no live session is not
/// reached this round and is retried next period.
async fn drive_council_replication(others: &[HostId], budget: CommitBudget) {
  let sessions = take_sessions(|host| others.contains(&host));
  if sessions.is_empty() {
    return;
  }
  // The append owed each borrowed voter, built under a brief borrow (the endpoints stay out here).
  let appends: std::collections::BTreeMap<HostId, Vec<u8>> = state::with_state(|s| {
    sessions
      .iter()
      .filter_map(|(host, _)| {
        s.council
          .replication_for(*host)
          .map(|append| (*host, RaftMessage::AppendEntries(append).encode()))
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
  let replied = broadcast(requests, CONFIG_STREAM, budget).await;
  let mut recovered = kept;
  let mut replies = Vec::with_capacity(replied.len());
  for (host, reply, endpoint) in replied {
    replies.push(reply);
    recovered.push((host, endpoint));
  }
  state::with_state(|s| {
    for reply in replies {
      if let Ok(message) = RaftMessage::decode(&reply) {
        s.council.fold_reply(message);
      }
    }
  });
  return_sessions(recovered);
}

/// Drives an election as a **follower** whose leader contact has lapsed (Raft §9.6, the full pre-vote then
/// real vote over the transport): begins the pre-election and broadcasts the pre-vote to every other voter;
/// on a granted majority the real vote requests go out the same way, and folding their replies makes this
/// node leader once its own majority grants. Every borrowed voter session is returned whatever the outcome.
/// A node that already leads, or the sole voter (which `election_timeout` self-elects with no messages),
/// sends nothing.
async fn drive_council_election(others: &[HostId], budget: CommitBudget) {
  // Begin the pre-election; `election_timeout` returns one (identical) pre-vote per other voter, so the
  // first is the message to broadcast. Empty means this node already leads or self-elected — nothing to do.
  let Some(Some(pre_vote)) = state::with_state(|s| s.council.election_timeout().into_iter().next())
  else {
    return;
  };
  let sessions = take_sessions(|host| others.contains(&host));
  if sessions.is_empty() {
    return;
  }
  // Phase one — the pre-vote round over the borrowed voter sessions.
  let pre_bytes = pre_vote.encode();
  let requests: Vec<(HostId, Vec<u8>, Endpoint)> = sessions
    .into_iter()
    .map(|(host, endpoint)| (host, pre_bytes.clone(), endpoint))
    .collect();
  let replied = broadcast(requests, CONFIG_STREAM, budget).await;
  let mut sessions = Vec::with_capacity(replied.len());
  let mut pre_replies = Vec::with_capacity(replied.len());
  for (host, reply, endpoint) in replied {
    pre_replies.push(reply);
    sessions.push((host, endpoint));
  }
  // Fold the pre-vote replies; a granted majority yields the real vote request to broadcast next (the
  // follow-on of a pre-vote reply is always a vote request — the term is advanced only now).
  let vote = state::with_state(|s| {
    let mut vote = None;
    for reply in &pre_replies {
      if let Ok(message) = RaftMessage::decode(reply)
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
  let vote_bytes = vote.encode();
  let requests: Vec<(HostId, Vec<u8>, Endpoint)> = sessions
    .into_iter()
    .map(|(host, endpoint)| (host, vote_bytes.clone(), endpoint))
    .collect();
  let replied = broadcast(requests, CONFIG_STREAM, budget).await;
  let mut recovered = Vec::with_capacity(replied.len());
  let mut vote_replies = Vec::with_capacity(replied.len());
  for (host, reply, endpoint) in replied {
    vote_replies.push(reply);
    recovered.push((host, endpoint));
  }
  state::with_state(|s| {
    for reply in vote_replies {
      if let Ok(message) = RaftMessage::decode(&reply) {
        s.council.fold_reply(message);
      }
    }
  });
  return_sessions(recovered);
}

/// Whether this node's own SWIM membership view differs from the members of the configuration it has
/// installed — the signal a learner uses to fetch the council's committed configuration reactively (§4.8 the
/// piggyback rule). It compares this node's **alive** set against the configuration's **members**, which is
/// exactly the predicate the council leader reconciles from ([`RegionalCouncil::reconcile_alive`] admits an
/// alive non-member and takes over a member no longer alive), so a divergence means the council has — or, if
/// this node's view is ahead of the leader's, soon will have — a newer configuration this node must install:
/// to admit a newcomer, retire a departed member, or take over a failed owner's objects (the quiescent
/// successor's only cue, as it receives no records for the dead owner's objects). Equal in steady state, so a
/// converged learner fetches nothing. Compared as sets; both include this node.
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

/// Drives this node's configuration council one period from the record-plane coordinator (§4.8, D-14). As
/// **leader** it replicates a heartbeat to every voter (holding the term and carrying the commit index); as
/// a **follower** it counts the periods since the leader last made contact and, once past the jittered
/// election timeout, campaigns; the **sole voter** self-elects with no messages. `idle`, `seen_contact` and
/// `attempt` persist across periods (the coordinator owns them): the follower's election timer, the last
/// leader-contact value it saw, and its jitter rotation.
async fn drive_config_council(
  local: HostId,
  budget: CommitBudget,
  idle: &mut u32,
  seen_contact: &mut u64,
  attempt: &mut u32,
) {
  let Some((is_voter, is_leader, contact, voters)) = state::with_state(|s| {
    let voters = s.council.voters();
    (
      s.council.is_voter(local),
      s.council.is_leader(),
      s.council.leader_contact(),
      voters,
    )
  }) else {
    return;
  };

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
  if !is_voter {
    let wanted =
      state::with_state(|s| s.config_refresh_wanted || membership_diverges_from_config(s))
        .unwrap_or(false);
    if wanted {
      drive_learner_fetch(&voters, budget).await;
      let _ = state::with_state(|s| s.config_refresh_wanted = false);
    }
    return;
  }
  let others: Vec<HostId> = voters.into_iter().filter(|voter| *voter != local).collect();

  if is_leader {
    // As the region's configuration master, track its membership from this node's own SWIM view: propose
    // any admit or retire, which the replication below commits over the transport and applies on every
    // voter. Only the leader proposes (`reconcile_alive`); it probes every member, so a follower's own
    // detection need not.
    state::with_state(|s| {
      let alive = s.fleet.membership().alive();
      s.council.reconcile_alive(&alive)
    });
    drive_council_replication(&others, budget).await;
    *idle = 0;
    return;
  }
  if others.is_empty() {
    // The sole voter (a one-node council, the fleet degenerate): self-elect, then it leads next period.
    let _ = state::with_state(|s| s.council.election_timeout());
    *idle = 0;
    return;
  }
  // A follower: reset the timer while the leader keeps making contact; otherwise age toward an election.
  if contact != *seen_contact {
    *seen_contact = contact;
    *idle = 0;
    return;
  }
  *idle = idle.saturating_add(1);
  if *idle >= ELECTION_HEARTBEATS.saturating_add(election_jitter(local, *attempt)) {
    *attempt = attempt.saturating_add(1);
    *idle = 0;
    drive_council_election(&others, budget).await;
    // Re-baseline the contact counter so a fresh campaign is not immediately retriggered: a won election
    // makes this node leader next period; a lost one waits out the timer again.
    *seen_contact = state::with_state(|s| s.council.leader_contact()).unwrap_or(*seen_contact);
  }
}

/// The regions currently **alive** as this node sees them (§4.8, D-14): the distinct regions of the SWIM
/// alive set, mapped through the fleet's host→region assignment (a host absent from the map is in the sole
/// region `RegionId(0)`). The root group's leader reconciles the region membership against this — an alive
/// host's region is an alive region, and a region with no alive host is retired.
fn alive_regions(state: &ShardState) -> Vec<RegionId> {
  let mut regions: Vec<RegionId> = state
    .fleet
    .membership()
    .alive()
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
/// hosts; a region-scoped mesh would need a version-carrying signal instead — owed with region-scoped SWIM.)
fn root_diverges(state: &ShardState) -> bool {
  let alive: std::collections::BTreeSet<RegionId> = alive_regions(state).into_iter().collect();
  let known: std::collections::BTreeSet<RegionId> =
    state.root.configuration().regions.iter().copied().collect();
  alive != known
}

/// Drives this node's **root group** one period from the record-plane coordinator (§4.8, D-14 — the root
/// group across regions), the cross-region counterpart of [`drive_config_council`] and structurally its
/// parallel (a third such consensus group would motivate factoring the shared Raft-drive shape). As the
/// elected root master it reconciles the region membership from the regions currently alive
/// ([`alive_regions`]) and replicates over the transport ([`ROOT_STREAM`]); the sole voter self-elects; a
/// follower ages toward an election. A node that is **not** a root voter (not a region representative) does
/// nothing here — the root **learner** that fetches the committed root configuration is the owed follow-on,
/// so a non-representative host does not yet track the root configuration (nor does the verb path read it
/// yet). `idle`, `seen_contact` and `attempt` are the root group's own persistent election-timer state,
/// distinct from the council's.
async fn drive_root_group(
  local: HostId,
  budget: CommitBudget,
  idle: &mut u32,
  seen_contact: &mut u64,
  attempt: &mut u32,
) {
  let Some((is_voter, is_leader, contact, voters)) = state::with_state(|s| {
    let voters = s.root.voters();
    (
      s.root.is_voter(local),
      s.root.is_leader(),
      s.root.leader_contact(),
      voters,
    )
  }) else {
    return;
  };
  if !is_voter {
    // A root learner (a region member that is not its region's representative): it does not drive the Raft.
    // It fetches the committed root configuration from a root voter and adopts the newest (§4.8, D-14) —
    // reactively, only when its own alive view of the regions diverges from the root configuration it holds
    // ([`root_diverges`]), so a converged learner sends nothing. The cross-region parallel of the config
    // learner's reactive fetch.
    let wanted = state::with_state(|s| root_diverges(s)).unwrap_or(false);
    if wanted {
      drive_root_learner_fetch(&voters, budget).await;
    }
    return;
  }
  let others: Vec<HostId> = voters.into_iter().filter(|voter| *voter != local).collect();

  if is_leader {
    // As the root master, reconcile the region membership from this node's own alive view: propose admitting
    // any newly-alive region and retiring any **mirror-less** region no host is alive in (a lost region with a
    // mirror is left for a deliberate operator promotion — §4.8, split-brain safety), committed over the
    // transport by the replication below and applied on every root voter.
    state::with_state(|s| {
      let alive = alive_regions(s);
      s.root.reconcile_regions(&alive, &s.region_mirrors)
    });
    drive_root_replication(&others, budget).await;
    *idle = 0;
    return;
  }
  if others.is_empty() {
    // The sole root voter (a single-region fleet's degenerate): self-elect, then it leads next period.
    let _ = state::with_state(|s| s.root.election_timeout());
    *idle = 0;
    return;
  }
  // A follower: reset the timer while the leader keeps making contact; otherwise age toward an election.
  if contact != *seen_contact {
    *seen_contact = contact;
    *idle = 0;
    return;
  }
  *idle = idle.saturating_add(1);
  if *idle >= ELECTION_HEARTBEATS.saturating_add(election_jitter(local, *attempt)) {
    *attempt = attempt.saturating_add(1);
    *idle = 0;
    drive_root_election(&others, budget).await;
    *seen_contact = state::with_state(|s| s.root.leader_contact()).unwrap_or(*seen_contact);
  }
}

/// The root group's replication round over the transport — the parallel of [`drive_council_replication`] on
/// [`ROOT_STREAM`], reading [`ShardState::root`]: builds the append owed each borrowed root voter under a
/// brief borrow, ships them concurrently, folds the replies, and returns every session.
async fn drive_root_replication(others: &[HostId], budget: CommitBudget) {
  let sessions = take_sessions(|host| others.contains(&host));
  if sessions.is_empty() {
    return;
  }
  let appends: std::collections::BTreeMap<HostId, Vec<u8>> = state::with_state(|s| {
    sessions
      .iter()
      .filter_map(|(host, _)| {
        s.root
          .replication_for(*host)
          .map(|append| (*host, RaftMessage::AppendEntries(append).encode()))
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
  let replied = broadcast(requests, ROOT_STREAM, budget).await;
  let mut recovered = kept;
  let mut replies = Vec::with_capacity(replied.len());
  for (host, reply, endpoint) in replied {
    replies.push(reply);
    recovered.push((host, endpoint));
  }
  state::with_state(|s| {
    for reply in replies {
      if let Ok(message) = RaftMessage::decode(&reply) {
        s.root.fold_reply(message);
      }
    }
  });
  return_sessions(recovered);
}

/// The root group's pre-vote then vote election over the transport — the parallel of
/// [`drive_council_election`] on [`ROOT_STREAM`], reading [`ShardState::root`].
async fn drive_root_election(others: &[HostId], budget: CommitBudget) {
  let Some(Some(pre_vote)) = state::with_state(|s| s.root.election_timeout().into_iter().next())
  else {
    return;
  };
  let sessions = take_sessions(|host| others.contains(&host));
  if sessions.is_empty() {
    return;
  }
  // Phase one — the pre-vote round over the borrowed root-voter sessions.
  let pre_bytes = pre_vote.encode();
  let requests: Vec<(HostId, Vec<u8>, Endpoint)> = sessions
    .into_iter()
    .map(|(host, endpoint)| (host, pre_bytes.clone(), endpoint))
    .collect();
  let replied = broadcast(requests, ROOT_STREAM, budget).await;
  let mut sessions = Vec::with_capacity(replied.len());
  let mut pre_replies = Vec::with_capacity(replied.len());
  for (host, reply, endpoint) in replied {
    pre_replies.push(reply);
    sessions.push((host, endpoint));
  }
  // Fold the pre-vote replies; a granted majority yields the real vote request to broadcast next.
  let vote = state::with_state(|s| {
    let mut vote = None;
    for reply in &pre_replies {
      if let Ok(message) = RaftMessage::decode(reply)
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
  let vote_bytes = vote.encode();
  let requests: Vec<(HostId, Vec<u8>, Endpoint)> = sessions
    .into_iter()
    .map(|(host, endpoint)| (host, vote_bytes.clone(), endpoint))
    .collect();
  let replied = broadcast(requests, ROOT_STREAM, budget).await;
  let mut recovered = Vec::with_capacity(replied.len());
  let mut vote_replies = Vec::with_capacity(replied.len());
  for (host, reply, endpoint) in replied {
    vote_replies.push(reply);
    recovered.push((host, endpoint));
  }
  state::with_state(|s| {
    for reply in vote_replies {
      if let Ok(message) = RaftMessage::decode(&reply) {
        s.root.fold_reply(message);
      }
    }
  });
  return_sessions(recovered);
}

/// Drives a **root learner** one period: fetches the committed root configuration from the root voters it has
/// a session to and adopts the newest (§4.8, D-14) — the cross-region parallel of [`drive_learner_fetch`].
/// The fetch carries this learner's root version, so a caught-up learner's fetch is an empty reply, not a
/// full transfer; every borrowed session is returned. `adopt` only moves forward, so folding all replies
/// leaves the learner on the highest version any reachable voter returned.
async fn drive_root_learner_fetch(voters: &[HostId], budget: CommitBudget) {
  let sessions = take_sessions(|host| voters.contains(&host));
  if sessions.is_empty() {
    return;
  }
  let request = state::with_state(|s| s.root.configuration().version)
    .unwrap_or(0)
    .to_le_bytes()
    .to_vec();
  let requests: Vec<(HostId, Vec<u8>, Endpoint)> = sessions
    .into_iter()
    .map(|(host, endpoint)| (host, request.clone(), endpoint))
    .collect();
  let replied = broadcast(requests, ROOT_FETCH_STREAM, budget).await;
  let mut recovered = Vec::with_capacity(replied.len());
  let mut fetched: Vec<Vec<u8>> = Vec::new();
  for (host, reply, endpoint) in replied {
    if !reply.is_empty() {
      fetched.push(reply);
    }
    recovered.push((host, endpoint));
  }
  return_sessions(recovered);
  state::with_state(|s| {
    for bytes in &fetched {
      if let Ok(configuration) = decode_root_configuration(bytes) {
        s.root.adopt(configuration);
      }
    }
  });
}

/// Drives a **learner** (a non-voter member) one period: fetches the committed regional configuration from
/// the council voters it has a session to and adopts the newest (§4.8, D-14 — the council is a small elected
/// set, so a non-voter learns the configuration rather than voting on it; the adopted configuration is then
/// installed into placement by [`sync_config_from_council`] exactly as a voter's committed one is). The
/// bare fetch is broadcast to every voter session and the highest-version reply adopted, so a lagging
/// voter's stale copy never holds the learner back; every borrowed session is returned. A design refinement
/// (owed) is the piggyback rule — fetching only when a stale-configuration refusal names a newer version
/// (§4.8), rather than polling each period — but polling is correct and the config-commit rate is near zero.
async fn drive_learner_fetch(voters: &[HostId], budget: CommitBudget) {
  let sessions = take_sessions(|host| voters.contains(&host));
  if sessions.is_empty() {
    return;
  }
  // The fetch carries this learner's current configuration version, so a voter returns the configuration
  // only when it has a newer one (a caught-up learner's fetch is then an empty reply, not a full transfer).
  let request = state::with_state(|s| s.council.configuration().version)
    .unwrap_or(0)
    .to_le_bytes()
    .to_vec();
  let requests: Vec<(HostId, Vec<u8>, Endpoint)> = sessions
    .into_iter()
    .map(|(host, endpoint)| (host, request.clone(), endpoint))
    .collect();
  let replied = broadcast(requests, CONFIG_FETCH_STREAM, budget).await;
  let mut recovered = Vec::with_capacity(replied.len());
  let mut fetched: Vec<Vec<u8>> = Vec::new();
  for (host, reply, endpoint) in replied {
    if !reply.is_empty() {
      fetched.push(reply);
    }
    recovered.push((host, endpoint));
  }
  return_sessions(recovered);
  // Adopt the newest fetched configuration (`adopt` only moves forward, so folding all replies leaves the
  // learner on the highest version any reachable voter returned).
  state::with_state(|s| {
    for bytes in &fetched {
      if let Ok(configuration) = decode_regional_configuration(bytes) {
        s.council.adopt(configuration);
      }
    }
  });
}

/// Installs the configuration the council has committed into this node's placement view and takes over any
/// object whose owner it has retired (§4.8, D-14 — the council is the authority, this node places under what
/// it agreed). Derives this node's owner view from the regional configuration (`configuration_for`) and
/// installs it ([`FleetNode::install_configuration`]): the placement configuration the verbs read is updated,
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
    if s.council.configuration().version == s.fleet.configuration().version {
      return;
    }
    let (configuration, members, epochs) = {
      let regional = s.council.configuration();
      (
        regional.configuration_for(local),
        regional.members.clone(),
        regional.epochs.clone(),
      )
    };
    let Some(configuration) = configuration else {
      return;
    };
    // Surface a durability breach in the newly-committed configuration (§4.8, D-14 — the copyset count check
    // at every configuration change); a no-op when no operator durability policy is declared.
    crate::daemon::record_durability(
      &configuration,
      s.config
        .fleet
        .as_ref()
        .and_then(|membership| membership.durability),
    );
    for reassignment in s.fleet.install_configuration(configuration, &members) {
      s.pending_takeovers.insert(reassignment.object);
    }
    // Raise the fence for every held object to its owner's committed fencing epoch (§4.8 "every holder
    // raises its fence for that host to the new epoch"). A failed owner's epoch was bumped by the council's
    // takeover, so this fences a resumed stale owner `StaleEpoch` across all its objects **at once** — done
    // before `reconcile_held_authority` re-owns those objects to the successor, so the fence is read against
    // the *departed* owner. Monotonic (a live owner's unchanged epoch is a no-op), additive on top of the
    // configuration-generation fence. (The FencedRegister per-host model this realizes, A-9, still owes its
    // TLA+ revalidation before the modeled StaleNeverCommits result formally applies; design §4.8.)
    for acceptor in s.holder_records.values_mut() {
      if let Some(epoch) = epochs.get(&acceptor.owner()) {
        acceptor.raise_fence(*epoch);
      }
    }
    reconcile_held_authority(s);
  });
}

/// Fans the control shard's current committed configurations — the council's placement `Configuration` and the
/// root group's `RootConfiguration` — to every other shard each period (§4.8, D-7 "one owning shard per
/// volume", §4.8 "Lookup"). Every shard serves clients: the placement verbs (`place`/`region_placed`/
/// `await_placed`/`host_epoch`) and the cross-region lookup guard ([`crate::verbs::home_redirect`]) run on a
/// volume's owner shard, which may not be this control shard, so each shard must read what the council and the
/// root group committed or a volume owned elsewhere would report a stale placement or home after a membership
/// change, move or promotion. Only the control shard drives the council and the root group and installs their
/// commits (`sync_config_from_council`, `drive_root_group`); every other shard holds a read-only copy fanned
/// from here.
///
/// **Re-fanned every period, not only on change** — the same idempotent-retry discipline [`fold_peer_state`]
/// uses for the SWIM view: `run_on` is refused when a shard's control channel is momentarily full
/// ([`RtError::ControlFull`]), and a fan dropped on the one period a configuration changed would otherwise
/// leave that shard stale until the next change. Re-fanning heals it next period, and the receiving side is
/// version-gated (`install_configuration` installs only a newer version; `RootGroup::adopt` ignores an
/// equal-or-older one), so a re-fan of an unchanged configuration is a no-op there. Both configurations ride
/// **one** cross-shard message per shard, and both are bounded (a bounded neighbourhood; regions, moved homes
/// and promotions), so the per-period cost is bounded at every scale. A non-control shard tracks no held
/// object, so its `install_configuration` returns no reassignment to drive — takeover stays on this shard.
fn fan_configs_to_shards(origin: u16, shards: &[u16]) {
  let Some((placement, members, root)) = state::with_state(|s| {
    (
      s.fleet.configuration().clone(),
      s.fleet.members().to_vec(),
      s.root.configuration().clone(),
    )
  }) else {
    return;
  };
  for shard in shards.iter().copied().filter(|shard| *shard != origin) {
    let placement = placement.clone();
    let members = members.clone();
    let root = root.clone();
    let _ = run_on(origin, shard, move |s| {
      if placement.version > s.fleet.configuration().version {
        let _ = s.fleet.install_configuration(placement, &members);
      }
      s.root.adopt(root);
    });
  }
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
async fn run_record_plane(local: HostId, budget: CommitBudget) {
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
  // voter sessions is sequential with the record ships below, so it never contends for them. These persist
  // across periods — the follower's election timer, the last leader contact it saw, and its jitter rotation.
  let mut council_idle: u32 = 0;
  let mut council_seen_contact: u64 = 0;
  let mut council_attempt: u32 = 0;
  // The root group's own election-timer state (§4.8, D-14 — the cross-region authority), distinct from the
  // council's: it is a separate Raft over the region representatives, driven from this same coordinator.
  let mut root_idle: u32 = 0;
  let mut root_seen_contact: u64 = 0;
  let mut root_attempt: u32 = 0;
  loop {
    in_flight.retain_mut(|dispatch| !dispatch.settle());
    // Drive the configuration authority first — an election or a replication heartbeat over the transport —
    // then the records under the configuration it maintains.
    drive_config_council(
      local,
      budget,
      &mut council_idle,
      &mut council_seen_contact,
      &mut council_attempt,
    )
    .await;
    // Drive the root group across regions on the same coordinator (§4.8, D-14): a brief borrow of the
    // root-voter sessions, sequential with the council's and the record ships, so it never contends.
    drive_root_group(
      local,
      budget,
      &mut root_idle,
      &mut root_seen_contact,
      &mut root_attempt,
    )
    .await;
    // Install the configuration the council has agreed into this node's placement view, and take over any
    // object whose owner the council has now retired (§4.8, D-14 — the council is the authority; a departed
    // owner's objects that rendezvous first to this node over the new neighbourhood are owed a phase-one
    // recovery, driven below).
    sync_config_from_council(local);
    // Fan the committed placement and root configurations out to every other shard, every period, so a client
    // reads the same placement and cross-region home wherever it lands (§4.8 "Lookup", D-7). Re-fanned every
    // period so a dispatch a full control channel refused self-heals; the receiving install/adopt are
    // version-gated, so an unchanged configuration is a no-op there (see `fan_configs_to_shards`).
    fan_configs_to_shards(origin, &shards);
    // Keep the owner's hold writing under the current configuration generation: the version advances on
    // every join or retirement (`FleetNode::observe` keeps the node's own acceptor in step the same way), and
    // a record under a stale generation is refused `ForeignGeneration` by the owner's own hold — so without
    // this, no head provisioned after a membership change could ever place. Never refused: the version only
    // advances, and `install_authority` accepts an equal-or-higher generation.
    if let Some(authority) = owner_authority(local) {
      let _ = owner_acceptor.install_authority(authority);
    }
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
    // Drive each owed takeover over all this object's surviving candidate holders (phase-one recovery). A
    // drive that does not place leaves the object pending, so the next period retries. The holds and the
    // takeovers are this node's (they arrive over this shard's sessions), so this runs here.
    let owed = state::with_state(|s| takeovers(s, local)).unwrap_or_default();
    for object in owed {
      in_flight.extend(drive_takeover(object, local, budget).await);
    }
    // Serve the content of each adopted head, from what this node holds or a recorded holder, on the
    // shard the taken-over id routes to.
    materialize_pending(origin, budget).await;
    futures::sleep(HEARTBEAT_NS).await;
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
  // candidates (§4.10) — content places before the head that names it ships.
  let seals = call_within(
    origin,
    shard,
    move |s| {
      let slice_bytes = s.config.archive_slice_bytes;
      let created_unix = u64::try_from(s.clock.wall_ns()).unwrap_or(0) / NANOS_PER_SECOND;
      advance_seals(s, local, slice_bytes, created_unix)
    },
    HEARTBEAT_NS,
  )
  .await
  .unwrap_or_default();
  for work in seals {
    if let Some(dispatch) = put_seal_content(origin, local, work, budget).await {
      in_flight.push(dispatch);
    }
  }
  // Ship each unplaced head to all its candidate holders at once, committed at `f + 1`.
  let work = call_within(
    origin,
    shard,
    move |s| unplaced_heads(s, local),
    HEARTBEAT_NS,
  )
  .await
  .unwrap_or_default();
  for head in work {
    if let Some(dispatch) = ship_head(origin, owner_acceptor, local, &head, budget).await {
      in_flight.push(dispatch);
    }
  }
  // Record durably each seal whose content and head have both placed.
  let _ = run_on(origin, shard, record_placed_seals);
}

/// Format: nanoseconds per second, for the archive header's creation time in Unix seconds.
const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// Commits one head to the candidate holders that have not yet acknowledged it (those with a live session in
/// the shard state), through the owner's own acceptor, at `f + 1`. The holders' sessions are borrowed for the
/// commit and returned whatever the outcome (`request_within`; stragglers later). Every acknowledgement the
/// round collected is recorded **whatever the round's outcome** ([`record_acks`]): once a head is placed by
/// an earlier round, a later round re-shipping to a straggler is short of quorum on its own — `commit_record`
/// counts only its own round — and discarding its acknowledgements re-shipped that straggler forever at
/// `f > 1`. Returns the dispatch to settle, or `None` if no holder could be reached this period.
async fn ship_head(
  origin: u16,
  owner_acceptor: &mut Acceptor,
  local: HostId,
  head: &Head,
  budget: CommitBudget,
) -> Option<Dispatch> {
  let holders =
    take_sessions(|host| head.candidates.contains(&host) && !head.acked.contains(&host));
  if holders.is_empty() {
    return None; // No live session to a candidate that still needs the head; retry next period.
  }
  let taken: Vec<HostId> = holders.iter().map(|(host, _)| *host).collect();
  let committed = commit_record(
    local,
    owner_acceptor,
    &head.candidates,
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
    let _ = state::with_state(|s| {
      if version > s.council.configuration().version {
        s.config_refresh_wanted = true;
      }
    });
  }
  let dispatch = Dispatch::new(taken, &committed.reusable, committed.stragglers);
  return_sessions(committed.reusable);
  let placement = match committed.outcome {
    Ok(placement) => placement,
    Err(ClusterError::Uncertain { placement } | ClusterError::NotPlaced { placement }) => placement,
    Err(_) => return Some(dispatch), // A runtime refusal dispatched nothing; retry next period.
  };
  // The placement is the owner shard's fact: record it there.
  let (object, sequence, epoch) = (head.object, head.record.sequence, head.record.epoch);
  let _ = run_on(origin, head.shard, move |s| {
    record_acks_in(s, object, sequence, epoch, placement);
  });
  Some(dispatch)
}

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
  let entry = state
    .placed_heads
    .entry(object)
    .or_insert_with(|| PlacedHead {
      sequence,
      epoch,
      placement: Placement {
        candidates: placement.candidates.clone(),
        acked: Vec::new(),
        mirror_acked: None,
      },
    });
  if entry.sequence > sequence {
    return; // A stale round for a superseded head.
  }
  if entry.sequence < sequence {
    entry.sequence = sequence;
    entry.placement.candidates = placement.candidates.clone();
    entry.placement.acked.clear();
  }
  entry.epoch = entry.epoch.max(epoch);
  for host in placement.acked {
    if !entry.placement.acked.contains(&host) {
      entry.placement.acked.push(host);
    }
  }
}

/// Borrows out of the shard state the live sessions of the holders satisfying `wanted`, leaving each borrowed
/// entry `None` (out on a dispatch — the link task leaves it alone) until [`return_sessions`] puts it back. A
/// holder with no live session is skipped — the dispatch proceeds with the holders it can reach and the rest
/// are retried next period.
fn take_sessions(wanted: impl Fn(HostId) -> bool) -> Vec<(HostId, Endpoint)> {
  state::with_state(|s| {
    let mut taken = Vec::new();
    for (host, slot) in s.record_sessions.iter_mut() {
      if wanted(*host)
        && let Some(endpoint) = slot.take()
      {
        taken.push((*host, endpoint));
      }
    }
    taken
  })
  .unwrap_or_default()
}

/// Returns borrowed sessions to the shard state after a dispatch. Only an existing (borrowed) entry is
/// refilled: a peer retired meanwhile has had its entry removed by its link task, and its session is dropped.
fn return_sessions(sessions: Vec<(HostId, Endpoint)>) {
  state::with_state(|s| {
    for (host, endpoint) in sessions {
      if let Some(slot) = s.record_sessions.get_mut(&host) {
        *slot = Some(endpoint);
      }
    }
  });
}

/// Forwards a request to one peer over its record session and returns the reply bytes (§4.8 "Lookup" — a verb
/// a node cannot serve locally is sent to the node that can, over [`FORWARD_STREAM`]). Borrows the peer's
/// session the same way a dispatch does ([`take_sessions`]/[`return_sessions`], so it does not corrupt the
/// coordinator's use — whichever misses the session retries), returning it whatever the outcome
/// ([`request_within`]). `None` when the peer has no live session here (the caller retries); an empty `Some`
/// when the request timed out. Called from the control shard, where the record sessions live.
pub(crate) async fn forward_over_leader_session(
  peer: HostId,
  request: Vec<u8>,
  deadline_ns: u64,
) -> Option<Vec<u8>> {
  let mut sessions = take_sessions(|host| host == peer);
  let (_, endpoint) = sessions.pop()?;
  let (reply, endpoint) = request_within(endpoint, FORWARD_STREAM, &request, deadline_ns).await;
  return_sessions(vec![(peer, endpoint)]);
  Some(reply)
}

/// The pending takeovers this node should drive: the objects it owes a takeover for
/// ([`ShardState::pending_takeovers`]) whose surviving candidate set — computed the same way every node
/// computes placement ([`candidates_for`] over the current neighbourhood) — contains this node, the new owner
/// `sync_peer` reassigned them to. The coordinator drives each over **all** of the object's surviving
/// candidate holders, so the `f > 1` promotion quorum (this node plus `f` holders) is reached over the
/// several sessions it owns. Read under `with_state`.
fn takeovers(state: &ShardState, local: HostId) -> Vec<ObjectId> {
  let config = state.fleet.configuration();
  state
    .pending_takeovers
    .iter()
    .copied()
    .filter(|object| {
      candidates_for(
        local,
        &config.neighbourhood,
        &config.domains,
        *object,
        config.quorum,
      )
      .contains(&local)
    })
    .collect()
}

/// Drives one object's takeover over **all** its surviving candidate holders (§4.8 "Promotion and takeover":
/// one batched phase-one round, then safe adoption). This node is the survivor rendezvous ranked first for
/// `object`, so it promotes the object's head over its own hold plus every surviving candidate holder the
/// coordinator has a session to and, on a quorum of promises, re-commits the adopted head under the new epoch,
/// records the placement, and clears the pending takeover so the verbs read the head as owned and
/// region-placed. At `f = 1` the quorum is this node plus one holder; at `f > 1` it is this node plus `f`
/// holders, reached over the several sessions the coordinator owns — the generalization a per-peer ship task
/// could not make. The object's hold is removed for the promotion and re-commit (nothing else touches a dead
/// owner's object's hold here) and re-inserted after, now under this node's authority. Short of quorum, or on
/// lost sessions, the object stays pending and the next period retries — self-healing across the window while
/// every survivor brings its holds' authority into step.
async fn drive_takeover(object: ObjectId, local: HostId, budget: CommitBudget) -> Vec<Dispatch> {
  let mut dispatches = Vec::new();
  // Read the takeover parameters and take exclusive hold of the object's acceptor. Nothing to drive if this
  // node does not hold the object or is not a candidate for it.
  let prepared = state::with_state(|s| {
    let config = s.fleet.configuration();
    let candidates = candidates_for(
      local,
      &config.neighbourhood,
      &config.domains,
      object,
      config.quorum,
    );
    if !candidates.contains(&local) {
      return None;
    }
    let quorum = config.quorum;
    let generation = config.version;
    let mut acceptor = s.holder_records.remove(&object)?;
    // The new epoch: one above the highest this node holds for the object, so the promotion raises the
    // holders' fence above the epoch the dead owner committed under (§4.8 "serves under the bumped epoch").
    let epoch = HostEpoch(highest_held_epoch(&acceptor, object).0.saturating_add(1));
    // Install this node as the object's owner under the current generation, so its own promise and the
    // adoption re-commit are authorized (the configuration group's taken-over authority, applied locally).
    let _ = acceptor.install_authority(Authority {
      generation,
      owner: local,
    });
    Some((acceptor, candidates, quorum, generation, epoch))
  })
  .flatten();
  let Some((mut acceptor, candidates, quorum, generation, epoch)) = prepared else {
    return dispatches;
  };

  let prepare = Prepare {
    owner: local,
    object,
    epoch,
    generation,
  };
  // Phase one: promise locally through this node's hold and over every surviving candidate holder; adopt the
  // newest record across the quorum of promises.
  let holders = take_sessions(|host| candidates.contains(&host));
  let taken: Vec<HostId> = holders.iter().map(|(host, _)| *host).collect();
  let promoted = promote_record(
    local,
    &mut acceptor,
    &candidates,
    &prepare,
    quorum,
    holders,
    budget,
  )
  .await;
  dispatches.push(Dispatch::new(
    taken,
    &promoted.reusable,
    promoted.stragglers,
  ));
  return_sessions(promoted.reusable);
  // Safe adoption: re-commit the adopted head under the new epoch over the holders, reaching the quorum.
  let mut placed = None;
  if let Ok(promotion) = promoted.outcome
    && let Some(adoption) = promotion.adoption_record(&prepare)
  {
    let holders = take_sessions(|host| candidates.contains(&host));
    let taken: Vec<HostId> = holders.iter().map(|(host, _)| *host).collect();
    let committed = commit_record(
      local,
      &mut acceptor,
      &candidates,
      &adoption,
      quorum,
      holders,
      budget,
    )
    .await;
    dispatches.push(Dispatch::new(
      taken,
      &committed.reusable,
      committed.stragglers,
    ));
    return_sessions(committed.reusable);
    if let Ok(placement) = committed.outcome
      && placement.placed(quorum)
    {
      placed = Some((adoption.sequence, adoption.value.clone(), placement));
    }
  }
  // Re-insert the hold (now under this node's authority) and, when the adoption placed, record the placement
  // and clear the pending takeover so the head is served as owned — and queue the head's content to be
  // materialized and served (§4.10); otherwise the object stays pending.
  state::with_state(|s| {
    s.holder_records.insert(object, acceptor);
    if let Some((sequence, value, placement)) = placed {
      s.placed_heads.insert(
        object,
        PlacedHead {
          sequence,
          epoch: prepare.epoch,
          placement,
        },
      );
      s.pending_takeovers.remove(&object);
      if let Some(head) = HeadValue::from_record_bytes(&value) {
        s.pending_materializations.insert(object, head);
      }
    }
  });
  dispatches
}

/// The highest epoch this node holds for `object` in `acceptor`, or the first epoch if it holds nothing —
/// the anchor the takeover's new epoch is bumped one above.
fn highest_held_epoch(acceptor: &Acceptor, object: ObjectId) -> HostEpoch {
  let (_, positions) = acceptor.persisted();
  positions
    .into_iter()
    .filter(|(held, _, _, _)| *held == object)
    .map(|(_, _, epoch, _)| epoch)
    .max_by_key(|epoch| epoch.0)
    .unwrap_or(FIRST_EPOCH)
}
