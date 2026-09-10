//! The fleet membership loop the control shard runs (§4.8 "Membership"; §2.6 boot step 6). A fleet node
//! probes each of its peers over the transport, serves their probes, replicates its volume heads to them,
//! and folds the converged SWIM view into its [`FleetNode`](slates_cluster::fleet::FleetNode) — retiring a
//! dead peer and taking over the objects that rendezvous now ranks first to this node.
//!
//! **Per-peer sessions over [`Endpoint::accept`]** (`slates_transport::endpoint::Endpoint::accept`): a peer
//! dials this node from an address chosen at dial time, and the node accepts it. Because `accept` learns one
//! peer from the first datagram on its socket, this node binds **one serve socket per peer per plane** and
//! *dials* each peer's serve socket from a separate socket, so every session is cleanly one-directional (no
//! bidirectional-request deadlock). A two-node fleet is the single-peer degenerate — one serve socket pair —
//! and is proven live (`crates/server/tests/fleet.rs`). The loop is the general N-peer form (it iterates the
//! transport's peers), and a fleet of N forms its full N·(N−1) mesh reliably (the handshake retries on one
//! socket until the peer's pinned `accept` completes — [`establish_session`]; measured 25/25 full suite under
//! load, formation 60/60). Multiplexing several peers on *one* socket (an O(N) socket count rather than the
//! mesh's O(N²)) is the connection-ID demux, owed; it would leave this loop's structure unchanged.
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

use rustls::pki_types::CertificateDer;
use slates_archive::Archive;
use slates_cluster::content::{fetch_content, is_content_stream, put_content};
use slates_cluster::detector::{Detector, DetectorTiming};
use slates_cluster::fleet::{apply_peer_state, sync_peer};
use slates_cluster::swim::{ProbeOutcome, SwimMessage, probe_once, serve_probe};
use slates_cluster::{
  ClusterError, CommitBudget, PROMOTE_STREAM, RECORD_STREAM, Stragglers, commit_record,
  promote_record,
};
use slates_db::Op;
use slates_db::catalog::{
  PlacementState, SnapshotId as DbSnapshotId, VolumeId as DbVolumeId, VolumeRecord,
};
use slates_db::register::{
  Acceptor, Authority, FIRST_EPOCH, HostEpoch, HostId, ObjectId, Placement, Prepare, Quorum,
  Record, candidates_for,
};
use slates_rt::futures;
use slates_rt::tcp::{Ipv4Addr, SocketAddrV4};
use slates_rt::udp::UdpSocket;
use slates_transport::endpoint::Endpoint;
use slates_transport::handshake::Identity;
use slates_vfs::clock::Clock;
use slates_vfs::export::{Progress, SnapshotArchiver};

use crate::daemon::HEARTBEAT_NS;
use crate::head::{HeadValue, PlacedHead, SealJob};
use crate::state::{self, ShardState};
use crate::verbs;
use crate::xshard::{call_within, run_on};

/// Format: RFC 9000 §14.1 — the smallest maximum UDP payload every QUIC path is required to carry without
/// path-MTU discovery (which the transport marks owed, `endpoint.rs`). Sizing a fleet datagram within it
/// keeps it deliverable unfragmented on any conformant path.
const MIN_DATAGRAM_BYTES: usize = 1200;

/// Format: an upper bound on one packet's non-payload bytes for the frame-cap derivation — the RFC 9000
/// §17.3 short header (first byte + a packet number ≤ 4 bytes → 5), the RFC 9001 §5.3 AEAD tag (16), and
/// one RFC 9000 §19.8 STREAM-frame header (type + stream-id + offset + length varints + fin, ≤ 43). Rounded
/// up to 64, a safe margin so a full-cap frame's packet never crosses [`MIN_DATAGRAM_BYTES`].
const FLEET_PACKET_OVERHEAD: usize = 64;

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
/// reach it, the two local addresses this node binds to *serve* it, and the operator-provisioned certificate
/// the mutual-TLS session pins (§4.8 "TLS 1.3 via rustls with certificates provisioned by the operator").
///
/// The serve binds are **per peer**: [`Endpoint::accept`] learns one peer from the first datagram on its
/// socket, so a node backing N peers needs one serve socket per peer per plane until the connection-ID demux
/// that would multiplex several peers on one socket lands (owed, `endpoint.rs`). A two-node fleet is the
/// single-peer degenerate — one peer, so one serve socket pair.
pub struct FleetPeer {
  /// The peer's host id.
  pub host: HostId,
  /// The peer's advertised probe address — where it accepts this node's SWIM probes, and where this node
  /// dials it.
  pub address: SocketAddrV4,
  /// The peer's advertised record address — where it accepts this node's register record commits (a separate
  /// socket from the probe one, because the SWIM and register wire formats are not distinguished by content
  /// on a shared stream; the connection-ID demux that would multiplex them on one socket is owed).
  pub record_address: SocketAddrV4,
  /// This node's local probe-serve address for this peer — where it accepts *this peer's* SWIM probes.
  pub probe_bind: SocketAddrV4,
  /// This node's local record-serve address for this peer — where it accepts *this peer's* record commits.
  pub record_bind: SocketAddrV4,
  /// The peer's operator-provisioned certificate, pinned for the mutual-TLS session.
  pub certificate: CertificateDer<'static>,
}

/// The fleet transport material the membership loop drives (§4.8, boot step 6). It is kept out of the
/// Clone-able [`DaemonConfig`](crate::config::DaemonConfig) because [`Identity`] is not `Clone` (it holds a
/// private key): the membership *policy* (quorum + peers) lives in the config and builds the `FleetNode`;
/// this *transport* material is handed to [`Daemon::start`](crate::Daemon) and moved to the control shard.
/// Each peer carries its own serve binds (the per-peer socket mesh), so the node-level addresses live on the
/// peers, not here.
pub struct FleetTransport {
  /// This node's fleet TLS identity (operator-provisioned).
  pub identity: Identity,
  /// The TLS server name this node presents and its peers pin.
  pub name: String,
  /// The peers this node probes and is probed by, each with its own dial and serve addresses.
  pub peers: Vec<FleetPeer>,
}

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

/// Runs the fleet membership loop for `transport` on the control shard (§4.8, boot step 6). For each peer it
/// sets up the two sessions — accepting the peer on this node's per-peer serve sockets and dialing the peer's
/// advertised addresses — and spawns the serve, probe and record-ship tasks. A two-node fleet is the
/// single-peer degenerate; N peers use N serve socket pairs (one per peer, since [`Endpoint::accept`] pins
/// one peer per socket) until the connection-ID demux that would multiplex peers on one socket lands (owed).
/// Detached tasks: they live as long as the shard and are cancelled by the runtime's shutdown.
pub async fn run_membership(transport: FleetTransport) {
  let FleetTransport {
    identity,
    name,
    peers,
  } = transport;
  if peers.is_empty() {
    // No peers — nothing to probe; the placement path still runs the `FleetNode`, degenerate (R8).
    return;
  }
  // The identity is process-lifetime and shared by every peer's serve accepts and client dials — it is not
  // `Clone` (it holds a private key), so it is leaked to `&'static` and each peer's tasks borrow the one
  // copy. One leak per daemon boot.
  let identity: &'static Identity = Box::leak(Box::new(identity));
  let neighbourhood = peers.len().saturating_add(1); // this node and its peers.
  let local = state::with_state(|s| s.fleet.host()).unwrap_or(HostId(0));
  let budget = probe_budget();

  for peer in peers {
    let FleetPeer {
      host,
      address,
      record_address,
      probe_bind,
      record_bind,
      certificate,
    } = peer;
    // The two serve sides accept this peer on this node's two per-peer sockets (bound here on the control
    // shard) and answer, one SWIM probes and one register record commits. They come up before the clients
    // dial, so a peer's dial finds a listener.
    let (Ok(probe_accept), Ok(record_accept)) =
      (UdpSocket::bind(probe_bind), UdpSocket::bind(record_bind))
    else {
      // The peer's serve sockets could not be bound (the address is in use, or refused): this peer cannot be
      // served, so it is skipped — counted, never silent, since the mesh will not form to it and an operator
      // reading the status refusal counts must be able to see why (banned item 9).
      count_refusal(BIND_REFUSED);
      continue;
    };
    let (Ok(probe_serve), Ok(record_serve)) = (
      Endpoint::accept(
        probe_accept,
        identity,
        std::slice::from_ref(&certificate),
        FLEET_FRAME_CAP,
      ),
      Endpoint::accept(
        record_accept,
        identity,
        std::slice::from_ref(&certificate),
        FLEET_FRAME_CAP,
      ),
    ) else {
      count_refusal(ACCEPT_REFUSED);
      continue;
    };
    if let Ok(task) = futures::spawn(serve_peer_probes(probe_serve, local, neighbourhood)) {
      let _ = futures::detach(task);
    }
    if let Ok(task) = futures::spawn(serve_peer_records(record_serve, local, host)) {
      let _ = futures::detach(task);
    }
    // The client sides each keep their own session up — the probe task the peer's probe address, the record
    // link task the record address — so a slow or not-yet-listening peer never blocks another peer's setup.
    if let Ok(task) = futures::spawn(probe_peer(
      identity,
      name.clone(),
      address,
      certificate.clone(),
      host,
      local,
      neighbourhood,
    )) {
      let _ = futures::detach(task);
    }
    if let Ok(task) = futures::spawn(establish_record_link(
      identity,
      name.clone(),
      host,
      record_address,
      certificate,
    )) {
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

/// The serve side: complete the accepted session's handshake and loop answering the peer's probes (§4.8).
/// Its own detector builds the acknowledgement gossip; a serve failure (the peer's connection dropped when
/// it died) ends the loop, which is correct — a dead peer sends no more probes to answer.
async fn serve_peer_probes(mut endpoint: Endpoint, local: HostId, neighbourhood: usize) {
  if endpoint.establish().await.is_err() {
    return;
  }
  let timing = detector_timing(neighbourhood);
  let fanout = usize::try_from(timing.gossip_transmits).unwrap_or(1);
  let mut detector = Detector::new(local, timing);
  while serve_probe(&mut endpoint, &mut detector, local, fanout)
    .await
    .is_ok()
  {}
}

/// The probe side: dial the peer's probe address (so a slow or not-yet-listening peer never blocks the other
/// peers' loops — each probe task dials its own), then each protocol period probe the peer, fold the outcome,
/// and fold this detector's converged view into the shard's `FleetNode` (§4.8). Each peer has its own probe
/// task and its own detector; `sync_membership` folds each detector's view without disturbing the peers it
/// does not track, so N detectors compose into one membership. The session is **reused whatever a probe's
/// outcome** — an acknowledgement and a timeout both hand it back ([`probe_once`]) — so a single missed
/// probe (a lost packet, scheduling jitter, a nonce-rejected stale reply) does not drop it: the next period
/// re-probes, a still-live peer refutes the suspicion the ping carried, and only a peer silent across the
/// suspicion window ages to death and is retired (driving the takeover). Dropping the session on one miss —
/// which cannot be re-established, since `Endpoint::accept` pins one source — would retire a live peer on
/// any transient glitch (`docs/bugs/2026-09-10-swim-stale-ack.md`).
async fn probe_peer(
  identity: &'static Identity,
  name: String,
  address: SocketAddrV4,
  certificate: CertificateDer<'static>,
  peer_host: HostId,
  local: HostId,
  neighbourhood: usize,
) {
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

  loop {
    (client, session) =
      establish_session(client, session, identity, &name, address, &certificate).await;
    if session.is_some() && !recorded_mesh {
      // The direct probe session to this peer has formed — record it, so the daemon can tell the real mesh
      // is up (`fleet_meshed`) rather than trusting the membership's optimistically seeded alive set.
      state::with_state(|s| s.formed_probe_peers.insert(peer_host));
      recorded_mesh = true;
    }
    if let Some(open) = session.take() {
      detector.tick();
      probe_nonce += 1;
      let ping = SwimMessage::Ping {
        from: local,
        nonce: probe_nonce,
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
          session = returned;
        }
        // A timeout is a **transient miss**, not a verdict — a lost packet, a moment's scheduling jitter, or
        // a nonce-rejected stale reply. The session is **kept and re-probed** next period: the ping carries
        // this node's suspicion, a still-live peer refutes it (its acknowledgement's gossip clears the
        // suspicion), and only a peer that stays silent across the detector's suspicion window ages to death.
        // Dropping it on one miss would retire a live peer on any glitch (the formation flake). A runtime
        // refusal (never expected from the inline probe) drops the session.
        Ok((returned, ProbeOutcome::TimedOut)) => {
          session = returned;
        }
        Err(_) => {}
      }
    }

    // Fold this peer's liveness into the shard's FleetNode (brief, synchronous — never held across an await).
    // Peer-scoped ([`sync_peer`], not `sync_membership`): each peer has its own detector, and a detector's
    // gossip carries other peers' states, so folding the whole view would let one peer's detector re-join a
    // peer another has retired — they would flap it until every detector converged. Scoped to this peer, the
    // retirement sticks the moment this detector ages it out.
    let retired = state::with_state(|s| {
      let takeovers = sync_peer(detector.membership(), &mut s.fleet, peer_host);
      // A death may hand this node objects to take over (it is the rendezvous-first survivor) and changes
      // the routing owner of others it merely holds a copy of. Record the ones to drive, and bring every
      // held acceptor's authority into step with the routing view — so this node can promote the objects it
      // takes over and can answer *another* survivor's promotion of the rest (the configuration group's
      // taken-over authority, applied locally with no coordination — every survivor computes the same
      // rendezvous winner, §4.8 "Promotion and takeover").
      for reassignment in &takeovers {
        s.pending_takeovers.insert(reassignment.object);
      }
      reconcile_held_authority(s);
      !s.fleet.configuration().neighbourhood.contains(&peer_host)
    });
    // Hand the peer's state to every other shard's `FleetNode`, so all copies of the configuration advance
    // identically (the version a head is written under, the neighbourhood its candidates are drawn from).
    // A spawn refused at a shard's admission bound is retried next period (the fold is idempotent).
    let peer_state = detector.membership().state(peer_host);
    for shard in shards.iter().copied().filter(|shard| *shard != origin) {
      let _ = run_on(origin, shard, move |s| {
        apply_peer_state(&mut s.fleet, peer_host, peer_state);
      });
    }
    if retired == Some(true) {
      // The peer is retired and gone from the direct mesh. Its objects' phase-one recovery is now driven by
      // the record-ship task (over the surviving candidate holders); this probe task for the dead peer ends.
      state::with_state(|s| s.formed_probe_peers.remove(&peer_host));
      return;
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
/// the authority the takeover installed, fences the epoch, and refuses a stale writer. A serve failure (the
/// peer's connection dropped when it died) ends the loop.
async fn serve_peer_records(mut endpoint: Endpoint, local: HostId, peer_host: HostId) {
  if endpoint.establish().await.is_err() {
    return;
  }
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
/// hands [`sync_peer`]'s takeover computation this object. Returns the binding acknowledgement's bytes,
/// or an empty reply on any refusal (the owner then counts nothing toward its quorum).
fn accept_held_record(
  state: &mut ShardState,
  local: HostId,
  peer_host: HostId,
  record: &Record,
) -> Vec<u8> {
  let generation = state.fleet.configuration().version;
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
    Err(_) => Vec::new(),
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

/// The status refusal count under which the fleet loop records a peer whose serve sockets could not be
/// bound at boot (§4.14: a refusal is counted, never silent).
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
const BIND_REFUSED: &str = "fleet.bind";

/// The status refusal count under which the fleet loop records a peer whose accept endpoints could not be
/// built at boot (the runtime refused the socket or the TLS server state).
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
const ACCEPT_REFUSED: &str = "fleet.accept";

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
async fn establish_record_link(
  identity: &'static Identity,
  name: String,
  peer_host: HostId,
  address: SocketAddrV4,
  certificate: CertificateDer<'static>,
) {
  let mut client: Option<Endpoint> = client_for(identity, &name, address, &certificate);
  loop {
    let retired =
      state::with_state(|s| !s.fleet.configuration().neighbourhood.contains(&peer_host));
    if retired == Some(true) {
      state::with_state(|s| s.record_sessions.remove(&peer_host));
      return;
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
  loop {
    in_flight.retain_mut(|dispatch| !dispatch.settle());
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
      candidates_for(local, &config.neighbourhood, *object, config.quorum).contains(&local)
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
    let candidates = candidates_for(local, &config.neighbourhood, object, config.quorum);
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
