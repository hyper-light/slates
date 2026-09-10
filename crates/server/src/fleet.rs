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
//! - the one **record-plane coordinator** ([`run_record_plane`]) owns *every* peer's client record session,
//!   so each period it ships each unplaced head to **all** its candidate holders in one commit (§4.8 "records
//!   are sent to all candidates; committed at `f + 1`") and **drives any owed takeover** over **all** the
//!   object's surviving holders — the `f + 1` promise quorum a takeover needs, several holders at `f > 1`.
//!
//! **Takeover phase-one recovery** (§4.8 "Promotion and takeover"): when a peer dies, the probe loop records
//! the objects `sync_peer` reassigns to this node ([`ShardState::pending_takeovers`]) and brings every held
//! acceptor's authority into step with the routing view ([`reconcile_held_authority`] — the successor is
//! installed, so a holder answers the new owner's prepare and accepts its re-commit); the coordinator then
//! promotes the object over all surviving candidate holders and re-commits the adopted head under the new
//! epoch, recording the placement so the verbs read the head owned and region-placed. The serve loop answers
//! a `Prepare` from the same durable hold ([`serve_held_promotion`]). This is the general `f`-tolerant form
//! (proven at `f = 1` over three nodes and `f = 2` over five, the promotion spanning a multi-holder quorum);
//! serving the taken-over head's **content** (§4.10) — a holder has the head record, not the chunk bytes — is
//! owed.
//!
//! The `FleetNode` lives in the shard state (the verbs read it for placement), so it is touched only
//! through brief synchronous [`state::with_state`] — never held across an await. At `f = 0` (the laptop)
//! there is no fleet transport and this loop does not run; the placement path still runs the same
//! `FleetNode`, degenerate (R8).

use rustls::pki_types::CertificateDer;
use slates_cluster::detector::{Detector, DetectorTiming};
use slates_cluster::fleet::sync_peer;
use slates_cluster::swim::{ProbeOutcome, SwimMessage, probe_once, serve_probe};
use slates_cluster::{CommitBudget, commit_record, promote_record};
use slates_db::register::{
  Acceptor, Authority, FIRST_EPOCH, HostEpoch, HostId, ObjectId, Prepare, Quorum, Record,
  candidates_for,
};
use slates_rt::futures;
use slates_rt::tcp::{Ipv4Addr, SocketAddrV4};
use slates_rt::udp::UdpSocket;
use slates_transport::endpoint::Endpoint;
use slates_transport::handshake::Identity;

use crate::daemon::HEARTBEAT_NS;
use crate::state::{self, ShardState};

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

  // The record plane is one coordinator task owning every peer's client record session (below), so a commit
  // reaches all candidates at once and a takeover promotes over all surviving holders; the probe plane stays
  // per-peer (each SWIM detector tracks its own peer). Collect each peer's record link as the loop sets the
  // peers up, then spawn the one coordinator.
  let mut record_links: Vec<(HostId, SocketAddrV4, CertificateDer<'static>)> = Vec::new();

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
      continue;
    };
    if let Ok(task) = futures::spawn(serve_peer_probes(probe_serve, local, neighbourhood)) {
      let _ = futures::detach(task);
    }
    if let Ok(task) = futures::spawn(serve_peer_records(record_serve, local, host)) {
      let _ = futures::detach(task);
    }
    // The probe client dials its own session (the peer's probe address) so a slow or not-yet-listening peer
    // never blocks another peer's setup; the record client link is handed to the coordinator spawned below.
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
    record_links.push((host, record_address, certificate));
  }

  // One record-plane coordinator for all peers (§4.8 "records are sent to all candidates"): it owns every
  // holder session, so it ships each head to all candidates in one commit and drives each takeover over all
  // surviving holders (the `f > 1` promotion a per-peer ship task could not reach).
  if let Ok(task) = futures::spawn(run_record_plane(
    identity,
    name,
    local,
    budget,
    record_links,
  )) {
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
  loop {
    let served = endpoint
      .serve_once(|request| {
        if let Ok(prepare) = Prepare::decode(&request) {
          state::with_state(|s| serve_held_promotion(s, &prepare)).unwrap_or_default()
        } else if let Ok(record) = Record::decode(&request) {
          state::with_state(|s| accept_held_record(s, local, peer_host, &record))
            .unwrap_or_default()
        } else {
          Vec::new()
        }
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
  object: ObjectId,
  record: Record,
  candidates: Vec<HostId>,
  acked: Vec<HostId>,
  quorum: Quorum,
}

/// The volume heads this node owns that some candidate has **not yet acknowledged** (§4.8 "records are sent
/// to **all** candidates; committed at `f + 1`"): for each volume whose placement still has a remote
/// candidate holder missing the head, the record to commit, the full candidate set, the candidates that
/// already hold it, and the quorum the configuration computes. Read under `with_state`.
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
    let object = ObjectId(slot.id.bytes);
    let placement = config.place(object);
    let acked = state
      .placed_heads
      .get(&object)
      .map(|placed| placed.acked.clone())
      .unwrap_or_default();
    // Every remote candidate that has not acked still needs the head; when none remain the head is done.
    let outstanding = placement
      .candidates
      .iter()
      .any(|candidate| *candidate != local && !acked.contains(candidate));
    if !outstanding {
      continue;
    }
    heads.push(Head {
      object,
      record: Record {
        owner: local,
        object,
        sequence: 0,
        epoch: config.host_epoch,
        generation: config.version,
        value: slot.id.bytes.to_vec(),
      },
      candidates: placement.candidates,
      acked,
      quorum: config.quorum,
    });
  }
  heads
}

/// One candidate holder's client-side record link the coordinator owns: the peer's host and record address,
/// its certificate, and the persistent handshake sockets — an un-established `client` retried each period on
/// its own socket and, once up, the live `session`. Holding one per peer is what lets a single driver commit
/// a head to **all** candidates at once and promote a taken-over object over **all** surviving holders.
struct HolderLink {
  host: HostId,
  address: SocketAddrV4,
  certificate: CertificateDer<'static>,
  client: Option<Endpoint>,
  session: Option<Endpoint>,
}

impl HolderLink {
  /// Advances this link's handshake one attempt on its own socket (an already-established link is unchanged),
  /// so the peer's pinned `accept` completes rather than a fresh-port re-dial being ignored.
  async fn establish(&mut self, identity: &'static Identity, name: &str) {
    let (client, session) = establish_session(
      self.client.take(),
      self.session.take(),
      identity,
      name,
      self.address,
      &self.certificate,
    )
    .await;
    self.client = client;
    self.session = session;
  }
}

/// The record-plane coordinator (§4.8 "records are sent to all candidates; committed at `f + 1`"; "Promotion
/// and takeover"): one task per node owning **every** candidate holder's client session, so each period it
/// ships every unplaced head to all its candidates in one commit and drives every owed takeover over all
/// surviving holders. Consolidating the former per-peer ship tasks into one is what lets a single commit
/// reach all candidates at once (the design's shape, not N independent single-holder commits) and,
/// decisively, what lets an `f > 1` takeover promote over the several surviving holders one object needs — a
/// per-peer task held only its own peer's session and could reach a one-holder (`f = 1`) quorum only.
///
/// The sessions are brought up on their own sockets during formation — well before an idle successor (a node
/// with no volumes of its own) first uses one to drive a takeover — and reused across commits, the dispatch
/// keeping one across a timeout (`request_within`) so a load-timed-out commit or promotion retries over the
/// same warm session. The connection-ID demux that would carry all of them over one socket is owed and would
/// leave this coordinator's logic unchanged — only the socket count beneath it falls from O(N) to one.
async fn run_record_plane(
  identity: &'static Identity,
  name: String,
  local: HostId,
  budget: CommitBudget,
  peers: Vec<(HostId, SocketAddrV4, CertificateDer<'static>)>,
) {
  let Some(authority) = state::with_state(|s| Authority {
    generation: s.fleet.configuration().version,
    owner: local,
  }) else {
    return;
  };
  let mut owner_acceptor = Acceptor::new(local, authority);
  let mut links: Vec<HolderLink> = peers
    .into_iter()
    .map(|(host, address, certificate)| HolderLink {
      host,
      client: client_for(identity, &name, address, &certificate),
      session: None,
      address,
      certificate,
    })
    .collect();
  loop {
    // Bring every holder link up (one handshake attempt each on its own socket, retried until it completes).
    for link in &mut links {
      link.establish(identity, &name).await;
    }
    // Ship each unplaced head to all its candidate holders at once, committed at `f + 1`.
    let work = state::with_state(|s| unplaced_heads(s, local)).unwrap_or_default();
    for head in work {
      ship_head(&mut links, &mut owner_acceptor, local, &head, budget).await;
    }
    // Drive each owed takeover over all this object's surviving candidate holders (phase-one recovery). A
    // drive that does not place leaves the object pending, so the next period retries.
    let owed = state::with_state(|s| takeovers(s, local)).unwrap_or_default();
    for object in owed {
      drive_takeover(&mut links, object, local, budget).await;
    }
    futures::sleep(HEARTBEAT_NS).await;
  }
}

/// Commits one head to the candidate holders that have not yet acknowledged it (those the coordinator has a
/// live session to), through the owner's own acceptor, at `f + 1`. The holders' sessions are borrowed out of
/// the links for the commit and returned whatever the outcome (`request_within`), and the acknowledging set
/// is **merged** into the object's placement — never overwritten, or a commit that placed one holder would
/// forget another and the head would re-ship forever (`unplaced_heads` reads the merged set).
async fn ship_head(
  links: &mut [HolderLink],
  owner_acceptor: &mut Acceptor,
  local: HostId,
  head: &Head,
  budget: CommitBudget,
) {
  let holders = take_sessions(links, |host| {
    head.candidates.contains(&host) && !head.acked.contains(&host)
  });
  if holders.is_empty() {
    return; // No live session to a candidate that still needs the head; retry next period.
  }
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
  return_sessions(links, committed.reusable);
  if let Ok(placement) = committed.outcome {
    state::with_state(|s| {
      let entry = s
        .placed_heads
        .entry(head.object)
        .or_insert_with(|| placement.clone());
      for host in placement.acked {
        if !entry.acked.contains(&host) {
          entry.acked.push(host);
        }
      }
    });
  }
}

/// Borrows out the live sessions of the links whose host satisfies `wanted`, leaving those links session-less
/// for the dispatch (returned by [`return_sessions`]). A link with no live session is skipped — the dispatch
/// proceeds with the holders it can reach and the rest are retried next period.
fn take_sessions(
  links: &mut [HolderLink],
  wanted: impl Fn(HostId) -> bool,
) -> Vec<(HostId, Endpoint)> {
  let mut taken = Vec::new();
  for link in links.iter_mut() {
    if wanted(link.host)
      && let Some(endpoint) = link.session.take()
    {
      taken.push((link.host, endpoint));
    }
  }
  taken
}

/// Returns borrowed sessions to their links after a dispatch (a holder the dispatch dropped is not in
/// `reusable`, so its link stays session-less and re-establishes next period).
fn return_sessions(links: &mut [HolderLink], reusable: Vec<(HostId, Endpoint)>) {
  for (host, endpoint) in reusable {
    if let Some(link) = links.iter_mut().find(|link| link.host == host) {
      link.session = Some(endpoint);
    }
  }
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
async fn drive_takeover(
  links: &mut [HolderLink],
  object: ObjectId,
  local: HostId,
  budget: CommitBudget,
) {
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
    return;
  };

  let prepare = Prepare {
    owner: local,
    object,
    epoch,
    generation,
  };
  // Phase one: promise locally through this node's hold and over every surviving candidate holder; adopt the
  // newest record across the quorum of promises.
  let holders = take_sessions(links, |host| candidates.contains(&host));
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
  return_sessions(links, promoted.reusable);
  // Safe adoption: re-commit the adopted head under the new epoch over the holders, reaching the quorum.
  let mut placed = None;
  if let Ok(promotion) = promoted.outcome
    && let Some(adoption) = promotion.adoption_record(&prepare)
  {
    let holders = take_sessions(links, |host| candidates.contains(&host));
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
    return_sessions(links, committed.reusable);
    if let Ok(placement) = committed.outcome
      && placement.placed(quorum)
    {
      placed = Some(placement);
    }
  }
  // Re-insert the hold (now under this node's authority) and, when the adoption placed, record the placement
  // and clear the pending takeover so the head is served as owned; otherwise the object stays pending.
  state::with_state(|s| {
    s.holder_records.insert(object, acceptor);
    if let Some(placement) = placed {
      s.placed_heads.insert(object, placement);
      s.pending_takeovers.remove(&object);
    }
  });
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
