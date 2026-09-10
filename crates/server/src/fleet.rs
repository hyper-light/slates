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
//! transport's peers), but a fleet of N > 2 does not yet form its full mesh reliably: some of the N·(N−1)
//! handshakes over `accept` intermittently fail to establish, which the robust connection management the
//! transport marks owed (a fixed-port mesh, or an accept that re-learns across a dialer's flights) closes.
//! Multiplexing several peers on *one* socket (an O(N) socket count rather than the mesh's O(N²)) is the
//! connection-ID demux, also owed.
//!
//! **Tasks per peer on the control shard.** Each peer has its own probe task and serve tasks, because
//! [`serve_probe`] borrows its detector across the receive await while [`probe_once`] does not — so a single
//! shared detector cannot drive both, and each peer's detector is folded into the shared `FleetNode` by
//! [`sync_peer`], which touches only the peer it tracks (its own deaths and joins), so N detectors compose:
//! - the **probe** task owns a failure [`Detector`], dials the peer, and each protocol period probes it,
//!   folds the acknowledgement (or lets a timeout age the suspicion), then folds the detector's view into
//!   the shard's `FleetNode` and records any takeover;
//! - the **serve** tasks accept the peer on this node's per-peer sockets and answer its probes (so the peer
//!   sees this node alive) and its register record commits (so this node backs the peer as a holder);
//! - the **ship** task dials the peer's record address and replicates this node's unplaced volume heads to
//!   it each period (the cross-node commit, §4.8 "committed at `f + 1`").
//!
//! The `FleetNode` lives in the shard state (the verbs read it for placement), so it is touched only
//! through brief synchronous [`state::with_state`] — never held across an await. At `f = 0` (the laptop)
//! there is no fleet transport and this loop does not run; the placement path still runs the same
//! `FleetNode`, degenerate (R8). Phase-one recovery of a taken-over object's head and serving it under the
//! new epoch is owed; here the routing view records the reassignment `sync_peer` computes.

use rustls::pki_types::CertificateDer;
use slates_cluster::detector::{Detector, DetectorTiming};
use slates_cluster::fleet::sync_peer;
use slates_cluster::swim::{ProbeOutcome, SwimMessage, probe_once, serve_probe};
use slates_cluster::{CommitBudget, commit_record, serve_record};
use slates_db::register::{Acceptor, Authority, HostId, ObjectId, Quorum, Record};
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
    // The client sides each dial their own session — the probe task the peer's probe address, the record
    // ship task the record address at its own boot — so a slow or not-yet-listening peer never blocks
    // another peer's setup.
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
    if let Ok(task) = futures::spawn(ship_records(
      identity,
      name.clone(),
      record_address,
      certificate,
      host,
      local,
      budget,
    )) {
      let _ = futures::detach(task);
    }
  }
}

/// Dials `address` and completes the handshake on one socket. The handshake retransmits its own flights
/// ([`Endpoint::establish`]), so a single dial socket survives a peer that boots after this node first dials
/// it — there is no re-dial from a fresh port, which would race the peer's `accept` (it pins the first source
/// it hears, so a port change mid-handshake strands the session). `None` if the runtime refuses a socket or
/// endpoint, or the handshake is abandoned (a peer that never answers within the retransmit ceiling).
async fn dial(
  identity: &Identity,
  name: &str,
  address: SocketAddrV4,
  certificate: &CertificateDer<'static>,
) -> Option<Endpoint> {
  let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).ok()?;
  let mut client = Endpoint::client(
    socket,
    address,
    identity,
    certificate,
    name,
    FLEET_FRAME_CAP,
  )
  .ok()?;
  match client.establish().await {
    Ok(()) => Some(client),
    Err(_) => None,
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
/// does not track, so N detectors compose into one membership. While the session is alive a successful probe
/// reuses it (continuous packet numbers); a timeout drops it and the loop keeps ticking so the suspicion ages
/// to death (the peer is unreachable — the single session cannot be re-established), driving the takeover the
/// moment the fleet retires it.
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
  let Some(endpoint) = dial(identity, &name, address, &certificate).await else {
    return;
  };
  // The direct probe session to this peer has formed — record it, so the daemon can tell the real mesh
  // is up (`fleet_meshed`) rather than trusting the membership's optimistically seeded alive set.
  state::with_state(|s| s.formed_probe_peers.insert(peer_host));
  let timing = detector_timing(neighbourhood);
  let fanout = usize::try_from(timing.gossip_transmits).unwrap_or(1);
  let mut detector = Detector::new(local, timing);
  detector.join(peer_host);
  let mut session: Option<Endpoint> = Some(endpoint);

  loop {
    detector.tick();
    if let Some(open) = session.take() {
      let ping = SwimMessage::Ping {
        from: local,
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
        // A timeout (or a runtime refusal): the session is dropped; the loop keeps ticking to age the
        // suspicion to death, since the single accepted session cannot be re-established.
        Ok((_, ProbeOutcome::TimedOut)) | Err(_) => {}
      }
    }

    // Fold this peer's liveness into the shard's FleetNode (brief, synchronous — never held across an await).
    // Peer-scoped ([`sync_peer`], not `sync_membership`): each peer has its own detector, and a detector's
    // gossip carries other peers' states, so folding the whole view would let one peer's detector re-join a
    // peer another has retired — they would flap it until every detector converged. Scoped to this peer, the
    // retirement sticks the moment this detector ages it out.
    let retired = state::with_state(|s| {
      let takeovers = sync_peer(detector.membership(), &mut s.fleet, peer_host);
      (
        !takeovers.is_empty(),
        !s.fleet.configuration().neighbourhood.contains(&peer_host),
      )
    });
    if let Some((_took_over, gone)) = retired
      && gone
    {
      // The peer is retired and its objects reassigned in the routing view; phase-one recovery and serving
      // the taken-over head under the new epoch are owed. It is no longer part of the direct mesh.
      state::with_state(|s| s.formed_probe_peers.remove(&peer_host));
      return;
    }
    futures::sleep(HEARTBEAT_NS).await;
  }
}

/// The record serve side (§4.8 "records are sent to all candidates"): complete the accepted session's
/// handshake and loop accepting the peer's register record commits into a holder acceptor for the peer's
/// objects — this node backs the peer as a candidate holder. The acceptor's authority names the peer as the
/// owner under the configuration's generation, so a record from the peer is authorized and one from a stale
/// writer is refused. A serve failure (the peer's connection dropped) ends the loop.
async fn serve_peer_records(mut endpoint: Endpoint, local: HostId, peer_host: HostId) {
  if endpoint.establish().await.is_err() {
    return;
  }
  let Some(authority) = state::with_state(|s| Authority {
    generation: s.fleet.configuration().version,
    owner: peer_host,
  }) else {
    return;
  };
  let mut acceptor = Acceptor::new(local, authority);
  while serve_record(&mut endpoint, &mut acceptor).await.is_ok() {}
}

/// A volume head this node owns that is not yet region-placed, with everything the register commit needs.
struct Head {
  object: ObjectId,
  record: Record,
  candidates: Vec<HostId>,
  quorum: Quorum,
}

/// The volume heads this node owns that are not yet region-placed (§4.8): for each volume on the shard whose
/// object has no stored quorum placement, the record to commit (under the configuration's epoch and
/// generation) and the candidates and quorum the configuration computes for it. Read under `with_state`.
fn unplaced_heads(state: &ShardState, local: HostId) -> Vec<Head> {
  let config = state.fleet.configuration();
  let mut heads = Vec::new();
  for (_, slot) in state.volumes.iter() {
    let object = ObjectId(slot.id.bytes);
    if state.placed_heads.contains_key(&object) {
      continue;
    }
    let placement = config.place(object);
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
      quorum: config.quorum,
    });
  }
  heads
}

/// The record ship side (§4.8 "records are sent to all candidates; committed at `f + 1`"): each protocol
/// period this node replicates its unplaced volume heads to the peer holder, recording the acknowledging
/// [`Placement`] in `placed_heads` so the placement authority the verbs read reports the head region-placed.
/// A head already at a quorum placement is skipped (idempotent), so a placed head costs one map lookup.
///
/// The peer session is dialed **lazily** — only once there is a head to place — and then reused across
/// commits (its packet-number space stays continuous, RFC 9000 §12.3). Dialing at first use, rather than at
/// boot, is what keeps the session warm from its handshake through its first commit: a session dialed at
/// boot sits idle across the whole formation window (seconds of probing before the first volume is
/// provisioned), and an idle-then-reused session's first request stalls — the reused socket's first receive
/// after the long gap does not deliver the reply. A commit that loses its session (a straggler timeout drops
/// it) clears it, so the next period with an unplaced head redials. Reconnecting after a mid-run loss also
/// needs the peer's serve side to re-accept the new session, which — like the transport's other reconnection
/// work — is owed; so a lost session is retried, but a peer that has torn its accept side down is reached
/// only once it rebuilds.
async fn ship_records(
  identity: &'static Identity,
  name: String,
  record_address: SocketAddrV4,
  certificate: CertificateDer<'static>,
  peer_host: HostId,
  local: HostId,
  budget: CommitBudget,
) {
  let Some(authority) = state::with_state(|s| Authority {
    generation: s.fleet.configuration().version,
    owner: local,
  }) else {
    return;
  };
  let mut owner_acceptor = Acceptor::new(local, authority);
  // Dial the peer's record session at boot, alongside the probe session, so the peer's serve side completes
  // its handshake now rather than waiting idle for a first use seconds later (an idle accept socket does not
  // wake promptly on a datagram that arrives long after its handshake — the same reuse stall the commit
  // itself would hit). The session then stays open, reused across commits; a commit that loses it redials.
  let mut session = dial(identity, &name, record_address, &certificate).await;
  loop {
    let work = state::with_state(|s| unplaced_heads(s, local)).unwrap_or_default();
    // A session lost to a straggler timeout is redialed the next period there is a head to place.
    if !work.is_empty() && session.is_none() {
      session = dial(identity, &name, record_address, &certificate).await;
    }
    for head in work {
      let Some(endpoint) = session.take() else {
        // No live session (dial failed, or a prior head this period lost it): leave the rest for a retry.
        break;
      };
      let committed = commit_record(
        local,
        &mut owner_acceptor,
        &head.candidates,
        &head.record,
        head.quorum,
        vec![(peer_host, endpoint)],
        budget,
      )
      .await;
      // Keep the holder's connection for the next commit when it replied; a lost one clears the session so
      // the next period redials.
      session = committed
        .reusable
        .into_iter()
        .find(|(host, _)| *host == peer_host)
        .map(|(_, endpoint)| endpoint);
      if let Ok(placement) = committed.outcome {
        state::with_state(|s| {
          s.placed_heads.insert(head.object, placement);
        });
      }
    }
    futures::sleep(HEARTBEAT_NS).await;
  }
}
