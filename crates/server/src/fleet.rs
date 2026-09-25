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
//! dispatch, and the acknowledgements and durable placements are recorded back there. The hedge to the
//! remaining candidates fires on the **measured p95** of the content class's put latency ([`PutLatency`],
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
use slates_cluster::content::{
  CONTENT_PUT_STREAM, ContentMessage, fetch_content, is_content_stream, put_content,
};
use slates_cluster::coordinates::{CoordinateEngine, NetworkCoordinate};
use slates_cluster::detector::{Detector, DetectorTiming};
use slates_cluster::fleet::{apply_peer_state, sync_peer};
use slates_cluster::membership::{Liveness, MemberState};
use slates_cluster::raft_wire::RaftMessage;
use slates_cluster::root_group::root_representatives;
use slates_cluster::swim::{Delivery, ProbeOutcome, SwimMessage, deliver_once, probe_once};
use slates_cluster::timing::{ElectionTimer, ElectionTiming, PathRtt, RoundAnchors, round_budget};
use slates_cluster::{
  ClusterError, CommitBudget, PROMOTE_STREAM, RECORD_STREAM, Stragglers, TimedReply, broadcast,
  commit_record, promote_record, request_within,
};
use slates_db::Op;
use slates_db::catalog::{
  PlacementState, SnapshotId as DbSnapshotId, VolumeId as DbVolumeId, VolumeRecord,
};
use slates_db::register::{
  Acceptor, Authority, DomainId, FIRST_EPOCH, HostEpoch, HostId, ObjectId, Placement, Prepare,
  Quorum, Record, RegionId, RegisterError, candidates_for, encode_refusal,
};
use slates_rt::futures;
use slates_rt::udp::UdpSocket;
use slates_rt::udp::{Ipv4Addr, SocketAddrV4};
use slates_transport::demux::Demux;
use slates_transport::endpoint::{Endpoint, EndpointError, MIN_DATAGRAM_BYTES};
use slates_transport::handshake::Identity;
use slates_transport::rtt::RttEstimator;
use slates_vfs::clock::Clock;
use slates_vfs::export::{Progress, SnapshotArchiver};

use crate::daemon::{HEARTBEAT_NS, LIVENESS_BUDGET_NS};
use crate::deploy::NodeAddress;
use crate::dns::{self, Resolver};
use crate::head::{HeadValue, PlacedHead, SealJob};
use crate::state::{self, LearnedMember, ShardState};
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
pub const FLEET_FRAME_CAP: usize = MIN_DATAGRAM_BYTES - FLEET_PACKET_OVERHEAD;

/// Derived: SWIM's infection factor rounded to a per-bit integer weight for `λ·ln(n+1)` (§4.8; SWIM §4.1).
/// `λ·ln(x) = λ·ln(2)·log2(x)`, and the bit-length of `x` is `⌊log2(x)⌋+1`, so with SWIM's high-probability
/// `λ ≈ 3` the coefficient `λ·ln(2) ≈ 2.08` rounds to `2` per bit — a small integer (determinism-clean, no
/// float on any decision) that tracks `λ·ln(n+1)` within a rebroadcast across sizes (n=1 → 2, n=1000 → 20).
const GOSSIP_PER_BIT: u32 = 2;

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
pub struct FleetTransport {
  /// This node's fleet TLS identity (operator-provisioned).
  pub identity: Identity,
  /// The address this node publishes to authenticated peers.
  pub advertise: NodeAddress,
  /// Operator trust anchors for certificates of nodes absent from the seed manifest.
  pub enrollment_roots: Vec<CertificateDer<'static>>,
  /// The TLS server name this node presents and its peers pin.
  pub name: String,
  /// This node's probe-serve address: the one socket every peer's SWIM probes arrive on.
  pub probe_bind: SocketAddrV4,
  /// This node's record-serve address: the one socket every peer's record commits, prepares and content
  /// exchanges arrive on.
  pub record_bind: SocketAddrV4,
  /// The peers this node probes and is probed by, each with its dial addresses.
  pub peers: Vec<FleetPeer>,
  /// The host's resolver configuration, when any peer is addressed by a DNS name (the deployment plan
  /// refuses a named peer without one); `None` for a fleet of literal addresses.
  pub resolver: Option<Resolver>,
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
  resolver: Option<&'static Resolver>,
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

/// The peer a probe task tracks: its stable anchor (fixed for the task's life) and its **current** member id,
/// which follows what the peer announces on contact (task #22) — a restart moves it to the new id.
struct ProbedPeer {
  anchor: HostId,
  host: HostId,
}

/// Refused: a peer announced a member id that is not `member_id(anchor, boot_nonce)` for the certificate it
/// presented — an id it could not have derived (a forgery, or a corrupted announcement). Counted in the
/// status report's refusals, never folded.
const MEMBER_ID_FORGED: &str = "fleet.member_id_forged";
/// What learning an announced identity on contact concluded ([`classify_announced`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LearnedOutcome {
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
fn learn_member(
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
      *state.refusals.entry(MEMBER_ID_FORGED).or_insert(0) += 1;
    }
    LearnedOutcome::Current => {
      state.authenticated_members.insert(announced);
      state
        .learned_members
        .entry(anchor)
        .or_insert(LearnedMember {
          boot_nonce,
          host: announced,
        });
    }
    LearnedOutcome::Restarted { old } => {
      state.authenticated_members.remove(&old);
      state.authenticated_members.insert(announced);
      state.learned_members.insert(
        anchor,
        LearnedMember {
          boot_nonce,
          host: announced,
        },
      );
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
/// Manifest seeds locate certificates and addresses; optimistic SWIM seeding is not admission.
fn authenticated_alive(state: &ShardState) -> Vec<HostId> {
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
  let members: Vec<HostId> = state.council.configuration().members.clone();
  state
    .council_death_watch
    .retain(|host, _| members.contains(host));
  for member in members {
    let dead = state
      .fleet
      .membership()
      .state(member)
      .is_some_and(|belief| belief.liveness == Liveness::Dead);
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

/// The member id currently learned for `anchor` (task #22): the control shard's learned map, `None` off it
/// or for an anchor it has never seeded.
fn current_member(anchor: HostId) -> Option<HostId> {
  state::with_state(|s| s.learned_members.get(&anchor).map(|learned| learned.host)).flatten()
}

/// The SWIM/Lifeguard timing for a neighbourhood of `neighbourhood` members (this node plus its peers),
/// derived from the design's stated formulas (§4.8 "Derived constants"): the base suspicion window is
/// [`SUSPICION_PERIODS`]; gossip disseminates `λ·ln(n+1)` times ([`GOSSIP_PER_BIT`] × the bit-length of
/// `n+1`); the local-health multiplier is capped at [`LOCAL_HEALTH_CAP`]; the confirmation curve is off
/// (`suspicion_min = suspicion_periods`) and one corroboration suffices, because a small fleet has no
/// indirect proxies to gather more, so the window is not held open waiting for confirmations that cannot
/// arrive.
fn detector_timing(neighbourhood: usize) -> DetectorTiming {
  DetectorTiming {
    suspicion_periods: SUSPICION_PERIODS,
    gossip_transmits: neighbourhood_bits(neighbourhood)
      .saturating_mul(GOSSIP_PER_BIT)
      .max(1),
    health_max: LOCAL_HEALTH_CAP,
    suspicion_min: SUSPICION_PERIODS,
    // The `K` of the Lifeguard confirmation curve is the number of relays asked (§4.8); with `suspicion_min
    // == suspicion_periods` the curve is switched off, so this sets no timing today.
    confirmations_expected: u32::try_from(indirect_fanout(neighbourhood)).unwrap_or(u32::MAX),
  }
}

/// The bit-length of `n+1` (`⌊log2(n+1)⌋+1`) for a neighbourhood of `n` peers: the word width less its
/// leading zeros; `saturating_sub` keeps the degenerate `n+1 = 1` at one bit. The size term every
/// `O(log n)` SWIM budget is derived from — the gossip rebroadcasts ([`GOSSIP_PER_BIT`]) and the indirect
/// fan-out ([`indirect_fanout`]).
fn neighbourhood_bits(neighbourhood: usize) -> u32 {
  usize::BITS.saturating_sub(neighbourhood.saturating_add(1).leading_zeros())
}

/// Derived: `k`, how many relays a prober asks to reach a target its direct probe could not (§4.8 "direct
/// probe → k indirect proxies → SUSPECT"; SWIM §4.1's `k` ping-requests) — the bit-length of `n+1`, the same
/// size term the gossip budget uses, so the number of independent relay paths tried grows with the log of
/// the neighbourhood as SWIM's constants do (n = 2 → 2 relays, n = 1000 → 10) and never exceeds the peers
/// that exist: the caller takes at most the alive relays it holds sessions to. At least one, so a
/// two-peer neighbourhood still tries its one relay.
fn indirect_fanout(neighbourhood: usize) -> usize {
  usize::try_from(neighbourhood_bits(neighbourhood))
    .unwrap_or(usize::MAX)
    .max(1)
}

/// The probe's timing law for one peer (§4.8 "Derived constants": "detection timeout for membership from
/// RTT p99 × k; SWIM period = max(k × RTT p99, scheduler quantum)") — the measured round trip of the path to
/// this peer and how many probes in a row it has missed, from which each probe's deadline is derived
/// (nothing here is a hidden constant):
///
/// - **From the measured round trip**: the RFC 9002 §6.2.1 probe timeout, `smoothed_rtt + max(4 · rttvar,
///   granularity)` — Jacobson's mean-deviation bound on the round-trip tail (SIGCOMM 1988; the RTO the
///   Internet runs on), the running form of "RTT p99 × k" with `k` in the four deviations — over the
///   **shared path estimate** (`ShardState::peer_paths`, [`PathRtt`]), fed by every acknowledged probe (a
///   timed-out probe yields no sample: Karn's rule) and, since 2026-09-14, by every consensus round's reply
///   to this peer — one estimate per path, the same one the election timeout is derived from. A peer whose
///   acknowledgements have grown slow (its shard starved on a loaded box) is waited for accordingly, and
///   the estimate follows it back down.
/// - **Floored at the scheduler quantum**: [`HEARTBEAT_NS`], the daemon's beat — the finest cadence anything
///   on the control shard is scheduled at, so no deadline is set finer than the scheduler resolves (on a quiet
///   loopback the estimate is ~50 ms, below it: the floor keeps the quiet behaviour exactly what it was).
/// - **Backed off on each consecutive miss**: doubled per miss (RFC 9002 §6.2.4's exponential backoff — the
///   miss produced no sample, so the wait must grow without one), so silence is probed at 1×, 2×, 4× … the
///   estimate, not at a fixed beat six times over.
/// - **Capped at the larger of the liveness budget and measured quantum**: [`LIVENESS_BUDGET_NS`]
///   bounds the backoff at rest; a measured scheduling delay above it raises the cap so the floor still
///   holds. A dead peer is declared after a bounded number of misses (six misses ≈ 4 s at rest).
///
/// The probe **period** the design names, `max(k × RTT p99, scheduler quantum)`, holds by construction: the
/// probe task awaits each probe's outcome — its acknowledgement, or this deadline — before sleeping the
/// quantum ([`probe_period_ns`]), so probes never overlap and the cadence is at least the path's round trip
/// plus a beat.
///
/// The suspicion window ([`SUSPICION_PERIODS`]) still counts *probes*, so it dilates with the deadline: the
/// misses that kill a peer are misses of an adaptive, backed-off wait — a live peer starved for seconds is no
/// longer six 100 ms deadlines late; it is a slow peer the estimator and the backoff wait for
/// (`docs/bugs/2026-09-13-swim-fixed-probe-deadline-kills-a-starved-live-peer.md`).
struct ProbeTiming {
  consecutive_misses: u32,
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

/// The indirect-probe stage's traffic between this node's per-peer probe tasks and its probe serve side
/// (§4.8 "direct probe → k indirect proxies → SUSPECT"; SWIM §4.1; AUD-15). One detector runs per peer
/// and each probe task owns the session to its peer, so the stage is a hand-off between tasks over these
/// bounded queues rather than one detector's method calls:
///
/// - a **requester** whose direct probe of `target` timed out posts a ping-request for each chosen relay
///   under `outgoing[relay]` and wakes the relay's probe task, which sends it on its session
///   ([`carry_indirect_traffic`]);
/// - the **relay**'s serve side, receiving a ping-request from an authenticated requester, posts the ask
///   under `asks[target]` and wakes the target's probe task, which probes the target at once; an
///   acknowledgement moves the ask to `results[requester]` and wakes the requester's probe task, which
///   carries the answer back as an [`SwimMessage::IndirectAck`];
/// - the **requester**'s serve side records a relayed answer under `acks[target]` and wakes the target's
///   probe task, which credits it to its detector before the next tick ([`Detector::on_indirect_ack`]) —
///   so the target is not suspected on a lost direct packet.
///
/// Every key is an authenticated member this node keeps direct contact with (a request naming another is
/// refused and counted), so each map is bounded by the neighbourhood and the whole by its square; a newer
/// request for the same pair replaces the older, so no queue grows with time. `coordinates` holds the
/// coordinate each peer last announced, so relays are ranked nearest the target (the design's Vivaldi
/// selection). `wakers` parks each probe task between probes ([`sleep_or_wake`]) so traffic is carried
/// within a round trip rather than a period.
#[derive(Default)]
pub(crate) struct IndirectProbes {
  /// Ping-requests this node has posted, by relay: the target and the requester's probe nonce.
  pub outgoing: std::collections::BTreeMap<HostId, std::collections::BTreeMap<HostId, u64>>,
  /// Asks this node received as a relay, by target: the requester and its probe nonce.
  pub asks: std::collections::BTreeMap<HostId, std::collections::BTreeMap<HostId, u64>>,
  /// Answers this node owes as a relay, by requester: the target reached and the requester's nonce.
  pub results: std::collections::BTreeMap<HostId, std::collections::BTreeMap<HostId, u64>>,
  /// Relayed acknowledgements of this node's own probes, by target: the latest nonce a relay reached it for.
  pub acks: std::collections::BTreeMap<HostId, u64>,
  /// The coordinate each peer last announced on an acknowledgement.
  pub coordinates: std::collections::BTreeMap<HostId, NetworkCoordinate>,
  /// The waker of each probe task parked between probes.
  pub wakers: std::collections::BTreeMap<HostId, std::task::Waker>,
}

impl IndirectProbes {
  /// Whether traffic awaits the probe task of `peer`: a ping-request to send it, an answer to carry to it,
  /// an ask to probe it for, or a relayed acknowledgement to credit.
  fn traffic_pending_for(&self, peer: HostId) -> bool {
    self
      .outgoing
      .get(&peer)
      .is_some_and(|queue| !queue.is_empty())
      || self
        .results
        .get(&peer)
        .is_some_and(|queue| !queue.is_empty())
      || self.asks.get(&peer).is_some_and(|queue| !queue.is_empty())
      || self.acks.contains_key(&peer)
  }

  /// Forgets everything about `peer` — its queues, its coordinate, its parked waker — when its probe task
  /// releases it (retired): a request for a retired peer is refused thereafter, never queued.
  fn forget(&mut self, peer: HostId) {
    self.outgoing.remove(&peer);
    self.asks.remove(&peer);
    self.results.remove(&peer);
    self.acks.remove(&peer);
    self.coordinates.remove(&peer);
    self.wakers.remove(&peer);
  }
}

/// Wakes the probe task of `peer` if it is parked between probes ([`sleep_or_wake`]), so it carries the
/// traffic just posted for it at once.
fn wake_probe_task(state: &mut ShardState, peer: HostId) {
  if let Some(waker) = state.indirect.wakers.remove(&peer) {
    waker.wake();
  }
}

/// The probe task's wait between probes of `peer`: the derived period, cut short the moment indirect-probe
/// traffic is posted for this task ([`wake_probe_task`]), so a relay probes its target, and a requester
/// credits a relayed answer, within a round trip of the request rather than up to a period later — the
/// latency that keeps the indirect stage inside the suspicion window. Re-checks the queues before parking
/// (a post that landed between the check and the park is not missed) and drops its waker on the way out.
async fn sleep_or_wake(period_ns: u64, peer: HostId) {
  let mut timer = std::pin::pin!(futures::sleep(period_ns));
  std::future::poll_fn(|cx| {
    if std::future::Future::poll(timer.as_mut(), cx).is_ready() {
      return std::task::Poll::Ready(());
    }
    let pending = state::with_state(|s| {
      if s.indirect.traffic_pending_for(peer) {
        return true;
      }
      s.indirect.wakers.insert(peer, cx.waker().clone());
      false
    })
    .unwrap_or(true);
    if pending {
      return std::task::Poll::Ready(());
    }
    std::task::Poll::Pending
  })
  .await;
  state::with_state(|s| {
    s.indirect.wakers.remove(&peer);
  });
}

/// The relays a requester asks to reach `target` (up to `fanout`, [`indirect_fanout`]): the alive peers it
/// holds a formed probe session to, other than the target, ranked **nearest the target** in coordinate
/// space when both coordinates are known (the design's Vivaldi selection — a near proxy is the likeliest
/// to reach it, so a slow far peer is not mistaken for a failed near one; the same ordering
/// [`Detector::request_indirect`] uses inside one detector), an unknown distance sorting last, ties in id
/// order for determinism. Mirrors that pure method over the **shared** view, since each per-peer detector
/// knows only its own peer.
fn indirect_relays(state: &ShardState, target: HostId, fanout: usize) -> Vec<HostId> {
  let alive = state.fleet.membership().alive();
  let mut relays: Vec<HostId> = state
    .formed_probe_peers
    .iter()
    .copied()
    .filter(|peer| *peer != target && alive.contains(peer))
    .collect();
  let distance = |relay: HostId| -> Option<f64> {
    let from = state.indirect.coordinates.get(&relay)?;
    let to = state.indirect.coordinates.get(&target)?;
    Some(CoordinateEngine::estimate_rtt(from, to))
  };
  relays.sort_by(|a, b| match (distance(*a), distance(*b)) {
    (Some(x), Some(y)) => x.total_cmp(&y).then(a.0.cmp(&b.0)),
    (Some(_), None) => std::cmp::Ordering::Less,
    (None, Some(_)) => std::cmp::Ordering::Greater,
    (None, None) => a.0.cmp(&b.0),
  });
  relays.truncate(fanout);
  relays
}

/// Derived: how many of a requester's own probes a relayed acknowledgement may lag and still be credited —
/// the suspicion window ([`SUSPICION_PERIODS`]): a relay's answer about a probe older than the window is
/// no longer evidence against the suspicion the window would have declared, so it is dropped rather than
/// credited to a later probe.
const INDIRECT_ACK_LAG_PROBES: u64 = SUSPICION_PERIODS as u64;

/// The control shard's scheduler quantum (§4.8 "SWIM period = max(k × RTT p99, scheduler quantum)"): the
/// design's [`HEARTBEAT_NS`] floor raised to the shard's **measured** descheduling — how late its steps
/// have run after the waits before them, reported by the runtime ([`futures::scheduler_overrun_ns`]).
/// `HEARTBEAT_NS` alone is the design's *assumed* quantum, the finest cadence a control-shard task is
/// scheduled at on a quiet host; but on an oversubscribed one (a CI runner running the whole suite on a
/// few cores; a box at several times its core count) an idle shard is left off-CPU for far longer, and
/// a fixed 100 ms quantum cannot account for that delay in its failure-detection windows
/// (`docs/bugs/2026-09-16-fleet-detection-windows-use-a-fixed-scheduler-quantum.md`). It is measured off
/// the shard's **waits** (only an idle shard parks or spins for its timer — a busy one never reaches
/// either — so it is the OS descheduling, not the latency of serving this shard's own tasks), sits at
/// `HEARTBEAT_NS` on a quiet host (a wait wakes within a tick, overrun ~0) — so every window that floors
/// at it is unchanged there — and rises on an oversubscribed one. The failure detector's windows — the
/// probe period, the probe deadline and its cap, and thereby the suspicion window — floor at it, so a
/// node that is itself starved is slow to declare an equally-starved peer dead, in proportion to the
/// starvation it observes. Only those: the node's own liveness signal — the record plane's council
/// heartbeats and record ships, the re-dial cadences — keeps the heartbeat, since a starved node must
/// announce itself as often as it can, not less often.
fn scheduler_quantum_ns() -> u64 {
  HEARTBEAT_NS.max(futures::scheduler_overrun_ns())
}

impl ProbeTiming {
  /// A fresh law: no misses. Before the path has a sample the deadline is the RFC 9002 §6.2.2 initial probe
  /// timeout (twice the initial RTT), conservative until the first acknowledgement seeds the estimate.
  fn new() -> ProbeTiming {
    ProbeTiming {
      consecutive_misses: 0,
    }
  }

  /// This probe's deadline over the path's measured tail (`None` before any sample): `max(tail or the
  /// initial probe timeout, scheduler quantum) × 2^misses`, capped at the larger of
  /// `LIVENESS_BUDGET_NS` and that quantum (the derivation on the type). The peer acknowledges inline,
  /// so no acknowledgement delay is added.
  fn deadline_ns(&self, path_tail_ns: Option<u64>) -> u64 {
    self.deadline_at_quantum(path_tail_ns, scheduler_quantum_ns())
  }

  /// The same deadline law at a supplied quantum, also used to measure whether the live scheduler
  /// floor changed a probe's budget. This comparison never selects a second execution path.
  fn deadline_at_quantum(&self, path_tail_ns: Option<u64>, quantum: u64) -> u64 {
    // Floored at the **measured** scheduler quantum, not the fixed heartbeat: on an oversubscribed host
    // a live peer answers late by the shard's own descheduling, and the cap rises with it so the wait
    // never expires inside the starvation this node itself observes.
    let base = path_tail_ns
      .unwrap_or_else(|| RttEstimator::new().initial_pto())
      .max(quantum);
    // A shift by the word width or more is already past the cap: saturate rather than overflow.
    let backoff = 1u64
      .checked_shl(self.consecutive_misses)
      .unwrap_or(u64::MAX);
    base
      .saturating_mul(backoff)
      .min(LIVENESS_BUDGET_NS.max(quantum))
  }

  /// The budget for this probe: its derived deadline, polled at the collection-loop cadence (a tenth of a
  /// period, [`POLL_PER_PERIOD`]).
  fn budget(&self, path_tail_ns: Option<u64>) -> CommitBudget {
    let deadline = self.deadline_ns(path_tail_ns);
    if deadline > self.deadline_at_quantum(path_tail_ns, HEARTBEAT_NS) {
      let quantum = scheduler_quantum_ns();
      let _ = state::with_state(|s| {
        s.probe_windows.deadlines_dilated = s.probe_windows.deadlines_dilated.saturating_add(1);
        s.probe_windows.largest_quantum_ns = s.probe_windows.largest_quantum_ns.max(quantum);
      });
    }
    CommitBudget::hard(deadline, (HEARTBEAT_NS / POLL_PER_PERIOD).max(1))
  }

  /// The probe was acknowledged: the backoff resets (the round trip itself is the path estimate's sample,
  /// folded by the caller into the shared path).
  fn acknowledged(&mut self) {
    self.consecutive_misses = 0;
  }

  /// The probe timed out: no sample (Karn's rule), one more consecutive miss to back off on.
  fn missed(&mut self) {
    self.consecutive_misses = self.consecutive_misses.saturating_add(1);
  }
}

/// The measured tail of the path to `peer` (`None` before its first round trip), read off the shared path
/// estimate on this shard.
fn path_tail_ns(peer: HostId) -> Option<u64> {
  state::with_state(|s| s.peer_paths.get(&peer).and_then(PathRtt::tail_ns)).flatten()
}

/// The slowest measured peer path's tail (`None` with no path measured) — what bounds a round to any set of
/// peers this node dispatches to, so the coordinator's period budget is derived from it.
fn slowest_path_tail_ns() -> Option<u64> {
  state::with_state(|s| s.peer_paths.values().filter_map(PathRtt::tail_ns).max()).flatten()
}

/// Folds one completed round trip to `peer` into its shared path estimate.
fn sample_path(state: &mut ShardState, peer: HostId, round_trip_ns: u64) {
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
/// SWIM probe ([`ProbeTiming::budget`], a hard deadline — one missed probe is absorbed by the suspicion
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
fn consensus_budget(slowest_tail_ns: Option<u64>) -> CommitBudget {
  round_budget(&ROUND_ANCHORS, slowest_tail_ns)
}

/// Runs the fleet membership loop for `transport` on the control shard (§4.8, boot step 6). It binds this
/// node's two serve sockets (one per plane, every peer on each through a demultiplexer) and spawns their
/// receive and accept loops; for each peer it dials the peer's advertised addresses and spawns the probe and
/// record-link tasks; then the one record-plane coordinator. A two-node fleet is the single-peer degenerate.
/// Detached tasks: they live as long as the shard and are cancelled by the runtime's shutdown.
pub async fn run_membership(transport: FleetTransport) {
  let FleetTransport {
    advertise,
    enrollment_roots,
    identity,
    name,
    probe_bind,
    record_bind,
    peers,
    resolver,
  } = transport;
  if peers.is_empty() && enrollment_roots.is_empty() {
    // No peers — nothing to probe; the placement path still runs the `FleetNode`, degenerate (R8).
    return;
  }
  // The identity is shared by every serve session and client dial of this shard — it is not `Clone` (it
  // holds a private key), so the control shard owns the one copy for its life (`ShardContext::keep`) and
  // each task borrows it as `&'static`; it is dropped with the shard's context, after every task. Before
  // 2026-09-14 it was leaked once per daemon boot. The resolver every dial task shares is kept the same way.
  let Some(identity) = slates_rt::registry::with_current(|ctx| {
    ctx.keep(identity.with_authorities(enrollment_roots.clone()))
  }) else {
    count_refusal(BIND_REFUSED);
    return;
  };
  let resolver: Option<&'static Resolver> =
    resolver.and_then(|resolver| slates_rt::registry::with_current(|ctx| ctx.keep(resolver)));
  let neighbourhood =
    state::with_state(|state| state.config.fleet_peer_capacity.saturating_add(1)).unwrap_or(1);
  let local = state::with_state(|s| s.fleet.host()).unwrap_or(HostId(0));
  let initialized = state::with_state(|state| {
    state.discovery = Some(crate::discovery::Discovery::new(
      state,
      name.clone(),
      enrollment_roots.clone(),
      identity.certificate(),
      advertise,
      record_bind.port(),
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
  let Some(driver) = slates_rt::registry::with_current(|context| {
    context.keep(PeerDriver {
      identity,
      name: name.clone(),
      resolver,
      local,
      neighbourhood,
    })
  }) else {
    count_refusal(LOOP_SPAWN_REFUSED);
    return;
  };
  // The boot line of the fleet's derived timing (R3: every derived value logged with its inputs). Nothing is
  // measured yet, so every value is at its floor; each period re-derives it from the measured paths, and
  // `Daemon::council_timing` / `slates status` read the live values.
  let floor = ElectionTiming::floor();
  let round = consensus_budget(None);
  eprintln!(
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
  let (Ok(probe_demux), Ok(record_demux)) = (
    Demux::start(
      probe_socket,
      identity,
      allowed.clone(),
      FLEET_FRAME_CAP,
      peer_capacity,
    ),
    Demux::start(
      record_socket,
      identity,
      allowed,
      FLEET_FRAME_CAP,
      peer_capacity,
    ),
  ) else {
    count_refusal(BIND_REFUSED);
    return;
  };
  state::with_state(|s| s.demuxes = vec![probe_demux, record_demux]);
  for demux in [probe_demux, record_demux] {
    spawn_detached(run_demux(demux), LOOP_SPAWN_REFUSED);
  }
  spawn_detached(
    accept_probes(probe_demux, local, neighbourhood, roster.clone()),
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
}

/// Shared immutable dial inputs, owned by the control shard; each candidate owns its two tasks.
struct PeerDriver {
  identity: &'static Identity,
  name: String,
  resolver: Option<&'static Resolver>,
  local: HostId,
  neighbourhood: usize,
}

impl PeerDriver {
  fn start(&'static self, peer: FleetPeer) {
    let probe = PeerDial {
      anchor: peer.anchor,
      host: peer.host,
      name: self.name.clone(),
      address: peer.address,
      certificate: peer.certificate.clone(),
      resolver: self.resolver,
    };
    let record = PeerDial {
      anchor: peer.anchor,
      host: peer.host,
      name: self.name.clone(),
      address: peer.record_address,
      certificate: peer.certificate,
      resolver: self.resolver,
    };
    spawn_detached(
      probe_peer(self.identity, probe, self.local, self.neighbourhood),
      LOOP_SPAWN_REFUSED,
    );
    spawn_detached(establish_record_link(self, record), LOOP_SPAWN_REFUSED);
  }
}

/// A serve socket's receive loop as a task, for the daemon's life: it routes every datagram to its session.
/// The socket refusing ends it, counted (`fleet.serve`), so an operator sees a node that stopped accepting
/// rather than a mesh that silently never re-forms.
async fn run_demux(demux: &'static Demux) {
  if let Err(e) = demux.run().await {
    // Counted for `status`, and said once in the log with its reason: a node that silently stopped
    // accepting sessions on a plane is a mesh that never forms with nothing to read but a count.
    count_refusal(SERVE_REFUSED);
    eprintln!("slates-server: fleet: a serve socket's receive loop ended: {e:?}");
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
  roster: Vec<Rostered>,
) {
  loop {
    let session = demux.accept().await;
    // A full task arena drops the accepted session here, explicitly — its slot goes back to the
    // demultiplexer and the peer's next re-dial takes a fresh one — and counts it, never lost in
    // silence (banned item 9). The fleet's share of the arena is sized for every session the
    // demultiplexer can hold (`config::with_fleet`), so the count is a tripwire.
    spawn_detached(
      serve_peer_probes(session, local, neighbourhood, roster.clone()),
      SERVE_SPAWN_REFUSED,
    );
  }
}

/// Accepts every record session a peer dials on the record socket and serves it over this node's durable
/// holds (§4.8): one serve task per session, which resolves the peer it authenticated through `roster`.
/// Bounded as [`accept_probes`] is.
async fn accept_records(
  demux: &'static Demux,
  local: HostId,
  roster: Vec<Rostered>,
  driver: &'static PeerDriver,
) {
  loop {
    let session = demux.accept().await;
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
async fn client_for(
  identity: &Identity,
  name: &str,
  address: (&NodeAddress, crate::deploy::Plane),
  certificate: &CertificateDer<'static>,
  resolver: Option<&'static Resolver>,
) -> Option<Endpoint> {
  let (address, plane) = address;
  let discovered = state::with_state(|state| {
    state
      .discovery
      .as_ref()
      .and_then(|discovery| discovery.address(certificate, plane))
  })
  .flatten();
  let address = discovered.as_ref().unwrap_or(address);
  let peer = match address {
    NodeAddress::Ip(address) => *address,
    NodeAddress::Name { host, port } => {
      let Some(resolver) = resolver else {
        count_refusal(RESOLVE_NO_RESOLVER);
        return None;
      };
      match dns::lookup(resolver, host).await {
        Ok(ip) => SocketAddrV4::new(ip, *port),
        Err(e) => {
          // The first refusal of each kind is logged once, so an operator's first look at the daemon's log
          // names the cause; the counts in `status` carry the rest.
          if count_refusal(resolve_refusal(&e)) == 1 {
            eprintln!("slates-server: fleet: resolving `{host}`: {e}");
          }
          return None;
        }
      }
    }
  };
  let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).ok()?;
  Endpoint::client(socket, peer, identity, certificate, name, FLEET_FRAME_CAP).ok()
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
/// client that was consumed but whose session was then lost is rebuilt. Returns the updated `(client, session)`.
async fn establish_session(
  client: Option<Endpoint>,
  session: Option<Endpoint>,
  identity: &Identity,
  name: &str,
  address: (&NodeAddress, crate::deploy::Plane),
  certificate: &CertificateDer<'static>,
  resolver: Option<&'static Resolver>,
) -> (Option<Endpoint>, Option<Endpoint>) {
  let (address, plane) = address;
  if session.is_some() {
    return (client, session);
  }
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
        if endpoint.handshake_budgets_spent() < ESTABLISH_BUDGETS_BEFORE_REDIAL =>
      {
        (Some(endpoint), None)
      }
      Err(EndpointError::NotReady) => {
        // Counted, and said once: a dialer that keeps dialing afresh names the peer it cannot reach.
        if count_refusal(DIAL_REDIAL) == 1 {
          eprintln!(
            "slates-server: fleet: no reply from {address} through {ESTABLISH_BUDGETS_BEFORE_REDIAL} handshake budgets; dialing afresh from a new port"
          );
        }
        (None, None)
      }
      Err(e) => {
        if count_refusal(DIAL_FAULT) == 1 {
          eprintln!("slates-server: fleet: the handshake to {address} faulted: {e:?}");
        }
        (None, None)
      }
    },
    None => (
      client_for(identity, name, (address, plane), certificate, resolver).await,
      None,
    ),
  }
}

/// Shape: the handshake budgets a dialer spends on one socket before it dials afresh from a new port. The
/// first budget covers a peer not yet listening when first dialed (a fleet forms as its nodes boot one
/// after another); the second resends the pending flight through a whole budget once more, for a peer that
/// was merely starved. A flight resent for a full budget with no reply means the peer's half-open state for
/// this source is gone or the peer is down, and a fresh dial — a new source the demultiplexer opens a fresh
/// session for — is the recovery.
pub const ESTABLISH_BUDGETS_BEFORE_REDIAL: u32 = 2;

/// Answers authenticated probes and shares adopted membership changes (§4.8). The prober's
/// anchor and boot nonce are validated before any report is folded. A returning peer hears
/// our death belief in the acknowledgement and refutes it from its shared local incarnation;
/// its next alive report re-admits it. Reports about known third members disseminate onward,
/// while strangers and superseded identities cannot enroll through gossip.
async fn serve_peer_probes(
  mut endpoint: Endpoint,
  local: HostId,
  neighbourhood: usize,
  roster: Vec<Rostered>,
) {
  if let Err(e) = endpoint.establish().await {
    count_accept_failure(&e, "probe");
    return;
  }
  // Only a **rostered** certificate may move membership (auth): the anchor it stands for is what the prober's
  // announced id and boot_nonce are validated against ([`learn_member`], task #22 learn-on-contact) — a
  // restart announces a **different** boot_nonce (a random per-start value; nonces cannot establish numeric
  // age order, so a different one, not a higher one, is what marks a restart), and its id is admitted as a
  // **new member** while its old id is retired in that one fold; a stale or forged announcement is refused,
  // counted, and not answered. An **unrostered or unauthenticated** prober receives no acknowledgement at all
  // (the serve handler answers only after `learn_member` validates the presented certificate's anchor and
  // boot_nonce), so authentication gates the reply, not only the fold. This node's own boot_nonce rides every
  // acknowledgement, so the prober validates this node the same way.
  let rostered_anchor = endpoint.peer_certificate().and_then(|presented| {
    roster
      .iter()
      .find(|peer| peer.certificate == presented)
      .map(|peer| peer.anchor)
      .or_else(|| {
        state::with_state(|state| {
          state
            .discovery
            .as_ref()
            .and_then(|discovery| discovery.recognizes(&presented))
        })
        .flatten()
      })
  });
  if rostered_anchor.is_none() {
    count_refusal(ACCEPT_REFUSED);
    return;
  }
  let local_boot_nonce = state::with_state(|s| s.member_boot_nonce).unwrap_or(0);
  let timing = detector_timing(neighbourhood);
  let fanout = usize::try_from(timing.gossip_transmits).unwrap_or(1);
  let mut detector = Detector::new(local, timing);
  loop {
    let served = endpoint
      .serve_once(|_stream, request| match SwimMessage::decode(&request) {
        // A ping-request announces no boot_nonce (it asks about a third party), so it is validated by the
        // authenticated session it arrived on instead: its sender must be the member the session's anchor
        // currently announces, and its target a member this node keeps direct contact with. Accepted, the
        // ask is posted for the target's probe task — woken to probe at once — and the exchange is answered
        // empty; the answer itself travels back on this node's own probe session to the requester.
        Ok(SwimMessage::PingReq {
          from,
          target,
          nonce,
          gossip,
        }) => {
          state::with_state(|state| {
            receive_ping_request(
              state,
              &mut detector,
              rostered_anchor,
              from,
              target,
              nonce,
              &gossip,
            );
          });
          Vec::new()
        }
        Ok(message) => {
          let Some((gossip, configuration_version)) = state::with_state(|state| {
            let anchor = rostered_anchor?;
            let boot_nonce = message.boot_nonce()?;
            let peer = message.from();
            if learn_member(state, anchor, boot_nonce, peer) == LearnedOutcome::Forged {
              return None;
            }
            // Test support: a peer this node is deaf to gets no acknowledgement of its **direct** probe —
            // the asymmetric path loss the indirect-probe regression imposes. Its gossip is not folded
            // either (the packet is treated as lost), and the exchange is answered empty, which the prober
            // reads as a timed-out probe.
            if matches!(message, SwimMessage::Ping { .. }) && state.probe_deaf_to.contains(&peer) {
              return None;
            }
            receive_probe_gossip(state, &mut detector, peer, message.gossip());
            if let Some(coordinate) = message.coordinate() {
              detector.learn_coordinate(peer, coordinate.clone());
            }
            // A relay's answer for one of this node's own probes: recorded for the target's probe task,
            // which is woken to credit it before its next tick.
            if let SwimMessage::IndirectAck { target, nonce, .. } = &message {
              receive_indirect_ack(state, *target, *nonce);
            }
            // The holder side of the owner lease (§4.8 "Leases and reads"; AUD-08): this node is about to
            // answer `peer`'s direct probe, a lease-confirming acknowledgement that feeds `peer`'s lease
            // over its own objects (this node is one of its candidate holders). Record when, and the
            // configuration version `peer` announces — the two facts that gate whether this node may later
            // answer a successor's promotion of `peer`'s objects: it must stay silent for the membership
            // horizon (so any lease it fed has expired) unless `peer` has announced it saw its retirement.
            if let SwimMessage::Ping {
              configuration_version,
              ..
            } = &message
            {
              let now = slates_machine::clock::monotonic_ns();
              state.answers_given.answered_alive(peer, now);
              state.answers_given.announced(peer, *configuration_version);
            }
            let belief = state.fleet.membership().state(peer);
            Some((
              outgoing_probe_gossip(state, peer, belief, fanout),
              state
                .lease
                .known_version(state.fleet.configuration().version),
            ))
          })
          .flatten() else {
            // A forged identity may neither mutate membership nor receive credit.
            return Vec::new();
          };
          SwimMessage::Ack {
            from: local,
            nonce: message.nonce().unwrap_or(0),
            boot_nonce: local_boot_nonce,
            configuration_version,
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

/// A ping-request received on an authenticated probe session (this node is the relay): accepted when its
/// sender `from` is the member the session's `anchor` currently announces (the requester's identity is the
/// session's, since the request carries no boot_nonce) and its `target` is a member this node keeps direct
/// contact with — the bound on the ask queue, so no authenticated peer can name arbitrary targets into it.
/// Accepted, the requester's gossip is folded (an authenticated sender), the ask is posted under the
/// target and the target's probe task is woken to probe it now; an acknowledgement answers every ask for
/// that target ([`answer_relay_asks`]). Refused and counted otherwise.
fn receive_ping_request(
  state: &mut ShardState,
  detector: &mut Detector,
  anchor: Option<HostId>,
  from: HostId,
  target: HostId,
  nonce: u64,
  gossip: &[(HostId, MemberState)],
) {
  let requester = anchor.and_then(|anchor| {
    state
      .learned_members
      .get(&anchor)
      .map(|learned| learned.host)
  });
  if requester != Some(from) || !keeps_direct_contact_with(state, target) || target == from {
    count_refusal_in(state, PROBE_INDIRECT_REFUSED);
    return;
  }
  receive_probe_gossip(state, detector, from, gossip);
  state
    .indirect
    .asks
    .entry(target)
    .or_default()
    .insert(from, nonce);
  wake_probe_task(state, target);
}

/// A relay's answer received on its authenticated probe session (this node is the requester, its sender
/// already validated by [`learn_member`]): recorded under the `target` it reached — a peer this node
/// probes, else refused and counted — with the requester's own probe `nonce` echoed, and that target's
/// probe task woken to credit it before its next tick ([`probe_and_apply`]).
fn receive_indirect_ack(state: &mut ShardState, target: HostId, nonce: u64) {
  if !keeps_direct_contact_with(state, target) {
    count_refusal_in(state, PROBE_INDIRECT_REFUSED);
    return;
  }
  let latest = state.indirect.acks.entry(target).or_insert(nonce);
  *latest = (*latest).max(nonce);
  wake_probe_task(state, target);
}

/// At the top of a probe cycle, decides whether to probe this peer or idle. Returns `false` when the peer is
/// **retired** — not one this node keeps direct contact with ([`keeps_direct_contact_with`]: its record
/// neighbourhood, its council's voters, the root group's voters) — and the caller idles the task (it does not
/// end: a believed-dead peer is never dialed, since that establish would block on a peer that will not
/// answer, and no probe session is held; when the peer rejoins — its own probe reaching this node's serve
/// side re-admits it at a higher incarnation, [`serve_peer_probes`] — the mesh regains it and probing
/// resumes). On the resume from idle it realigns `detector` to the fleet's re-admitted belief, so the
/// detector tracks the peer as alive and can detect a *future* death rather than carrying its stale death
/// forever. Returns `true` to probe.
fn resume_if_in_mesh(detector: &mut Detector, peer_host: HostId, was_idle: &mut bool) -> bool {
  let in_mesh = state::with_state(|s| keeps_direct_contact_with(s, peer_host)).unwrap_or(false);
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
/// now **retired** (out of the configuration's neighbourhood). Each detector times only its
/// actual peer; authenticated third-member reports separately enter the shared gossip view.
/// Incarnation ordering rejects stale alive reports. This only advances the
/// **failure view**; the configuration is the regional council's (D-14), so a death drives no takeover here
/// — the council leader reconciles the retirement from the folded view and, when it commits, the record
/// plane installs the new configuration and takes over what fell to this node ([`sync_config_from_council`]).
/// The folded state is handed to every other shard's membership copy **best-effort**, so all advance
/// identically (D-7): the cross-shard `run_on` is idempotent (incarnation-gated) but not retried, since no
/// off-control-shard path reads the raw SWIM membership — an owner shard routes and admits from the committed
/// configuration, which `fan_configs_to_shards` re-fans every period — so a fold dropped on a momentarily
/// full control channel is harmless (it is not, unlike the configuration, load-bearing on another shard).
/// "Retired"
/// means no longer a peer this node keeps direct contact with ([`keeps_direct_contact_with`] — read from the
/// committed configuration and the consensus voter sets, not from one detector's suspicion); the caller then
/// closes the peer's sessions on both planes, so this must be the **same** predicate the link task and the
/// probe's resume use, or a consensus voter outside the copyset is probed, judged retired, and has its
/// freshly dialed record session torn down every period.
fn fold_peer_state(detector: &Detector, peer_host: HostId, origin: u16, shards: &[u16]) -> bool {
  let retired = state::with_state(|s| {
    sync_peer(detector.membership(), &mut s.fleet, peer_host);
    let retired = !keeps_direct_contact_with(s, peer_host);
    if retired {
      // A discovery exchange pending on this peer's link re-checks it now, not at its deadline.
      wake_link_waiter_of(s, peer_host);
    }
    retired
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

/// Sends one probe over `session` (when established) and folds its outcome into `detector` and `timing`,
/// returning the session to carry forward. An acknowledgement clears the suspicion, learns the peer's gossip
/// and coordinate, and feeds its round trip to the deadline law ([`ProbeTiming`]); a timeout is a
/// **transient miss**, not a verdict — a lost packet, or a peer too starved to answer within the deadline —
/// so the session is kept and re-probed next period at a backed-off deadline (only silence across the
/// suspicion window ages the peer to death), while every ping to a suspected peer carries the suspicion for
/// a still-live peer to refute ([`Detector::ping_gossip`]); a runtime refusal (never expected from the inline
/// probe) drops it. The detector ticks only when a probe actually goes out, so an unestablished session never
/// resolves as a miss.
#[allow(clippy::too_many_arguments)]
async fn probe_and_apply(
  detector: &mut Detector,
  session: Option<Endpoint>,
  local: HostId,
  local_boot_nonce: u64,
  peer: &ProbedPeer,
  fanout: usize,
  nonce: u64,
  timing: &mut ProbeTiming,
) -> Option<Endpoint> {
  let open = session?;
  if let Some(belief) =
    state::with_state(|state| state.fleet.membership().state(peer.host)).flatten()
  {
    detector.apply(peer.host, belief);
  }
  credit_relayed_answer(detector, peer.host, nonce);
  detector.tick();
  // The newest configuration version this node knows (installed, or a supersession a peer announced): so a
  // holder receiving this probe learns at once when a retired owner has seen its retirement (§4.8 "Leases
  // and reads"; AUD-08). Sent with the probe below; the sent time is captured before the send.
  let (ping_gossip, announced_version) = state::with_state(|state| {
    (
      // The buddy system: a ping to a peer this node suspects always carries that suspicion, so the peer
      // refutes it from this very probe rather than after the gossip budget is spent.
      outgoing_probe_gossip(
        state,
        peer.host,
        detector.membership().state(peer.host),
        fanout,
      ),
      state
        .lease
        .known_version(state.fleet.configuration().version),
    )
  })?;
  let sent_ns = slates_machine::clock::monotonic_ns();
  let ping = SwimMessage::Ping {
    from: local,
    nonce,
    boot_nonce: local_boot_nonce,
    configuration_version: announced_version,
    gossip: ping_gossip,
  };
  match probe_once(open, &ping, timing.budget(path_tail_ns(peer.host))).await {
    Ok((
      returned,
      ProbeOutcome::Acked {
        from,
        boot_nonce,
        configuration_version,
        gossip,
        rtt_ns,
        coordinate,
      },
    )) => {
      // Validate the announced identity before accepting either liveness or relayed gossip.
      let admitted = state::with_state(|state| {
        learn_member(state, peer.anchor, boot_nonce, from) != LearnedOutcome::Forged
      })
      .unwrap_or(false);
      if !admitted {
        return None;
      }
      if from == peer.host {
        detector.on_ack(peer.host);
        timing.acknowledged();
        // The acknowledged round trip samples the shared path to this peer — the estimate the next probe's
        // deadline, the election timing and the round budget are all derived from.
        let _ = state::with_state(|s| {
          sample_path(s, peer.host, rtt_ns);
          s.probe_windows.acknowledged = s.probe_windows.acknowledged.saturating_add(1);
          // The peer's announced coordinate, shared so relays can be ranked nearest a target.
          s.indirect.coordinates.insert(peer.host, coordinate.clone());
          // This node, as a relay: every requester that asked it to reach this peer is answered — the ask
          // moves to the requester's queue and the requester's probe task is woken to carry it back.
          answer_relay_asks(s, peer.host);
          // The owner lease (§4.8 "Leases and reads"; AUD-08): this peer, a candidate holder of this node's
          // objects, has answered a probe reporting this node alive. Credit it toward the lease at the
          // version the peer announced — measured from the probe's send time, before the peer formed its
          // answer. A version newer than this node's installed one means this node's authority is
          // superseded (a retirement or takeover it has not applied): every object's lease voids until it
          // installs that version.
          s.lease.confirm(peer.host, sent_ns, configuration_version);
          if configuration_version > s.fleet.configuration().version {
            s.lease.supersede(configuration_version);
          }
        });
        #[allow(clippy::cast_precision_loss)]
        detector.observe_rtt(peer.host, rtt_ns as f64);
        detector.learn_coordinate(peer.host, coordinate);
      }
      state::with_state(|state| receive_probe_gossip(state, detector, from, &gossip));
      returned
    }
    Ok((returned, ProbeOutcome::TimedOut)) => {
      timing.missed();
      begin_indirect_stage(peer.host, nonce);
      returned
    }
    Ok((_, ProbeOutcome::Broken)) => {
      // The session cannot carry another exchange (closed, or the socket refused): a miss for the
      // detector, and the session released — `None` here makes the next period's `establish_session`
      // dial afresh through `client_for`, re-resolving the peer's address — rather than re-probing a
      // dead session every period until the suspicion window retires the peer (rejoin design item 2).
      count_refusal(PROBE_BROKEN);
      timing.missed();
      None
    }
    Err(_) => None,
  }
}

/// One probe cycle over an established session: the indirect stage's traffic for this peer first — the
/// ping-requests this node asks it to relay, the answers this node owes it ([`carry_indirect_traffic`]) —
/// then this node's own probe of it ([`probe_and_apply`]). Returns the session to carry forward, `None`
/// when either released it.
#[allow(clippy::too_many_arguments)]
async fn probe_cycle(
  detector: &mut Detector,
  session: Option<Endpoint>,
  local: HostId,
  local_boot_nonce: u64,
  peer: &ProbedPeer,
  fanout: usize,
  nonce: u64,
  timing: &mut ProbeTiming,
) -> Option<Endpoint> {
  let session =
    carry_indirect_traffic(session, local, local_boot_nonce, peer, fanout, timing).await;
  probe_and_apply(
    detector,
    session,
    local,
    local_boot_nonce,
    peer,
    fanout,
    nonce,
    timing,
  )
  .await
}

/// Credits a relay's answer about this node's probe of `peer_host` that last went unanswered, **before**
/// the tick resolves that probe (§4.8 "direct probe → k indirect proxies → SUSPECT"): the tick then counts
/// it answered, so the peer is not suspected on a lost direct packet. `nonce` is the probe about to be
/// sent; an answer about a probe older than the suspicion window ([`INDIRECT_ACK_LAG_PROBES`]) is stale
/// and dropped. Counted when credited (`fleet.probe.indirect.acked`).
fn credit_relayed_answer(detector: &mut Detector, peer_host: HostId, nonce: u64) {
  let relayed = state::with_state(|state| state.indirect.acks.remove(&peer_host)).flatten();
  if relayed.is_some_and(|relayed| nonce.saturating_sub(relayed) <= INDIRECT_ACK_LAG_PROBES) {
    detector.on_indirect_ack(peer_host);
    count_refusal(PROBE_INDIRECT_ACKED);
  }
}

/// The direct probe of `peer_host` (nonce `nonce`) went unanswered: begins the indirect stage — asks up to
/// `k` relays nearest the peer ([`indirect_relays`], [`indirect_fanout`]) to reach it on this node's
/// behalf before the next tick would suspect it. The ping-requests are posted for the relays' probe tasks
/// (each owns the session to its relay) and those tasks are woken to send them now. Counted per relay
/// asked (`fleet.probe.indirect.requested`).
fn begin_indirect_stage(peer_host: HostId, nonce: u64) {
  let _ = state::with_state(|state| {
    let fanout = indirect_fanout(state.fleet.members().len());
    for relay in indirect_relays(state, peer_host, fanout) {
      state
        .indirect
        .outgoing
        .entry(relay)
        .or_default()
        .insert(peer_host, nonce);
      count_refusal_in(state, PROBE_INDIRECT_REQUESTED);
      wake_probe_task(state, relay);
    }
  });
}

/// This node, as a relay, has just heard `target` acknowledge its probe: every requester that asked it to
/// reach `target` gets its answer posted (`results[requester]`, the requester's own probe nonce echoed) and
/// its probe task woken to carry it back. Counted per answer (`fleet.probe.indirect.relayed`).
fn answer_relay_asks(state: &mut ShardState, target: HostId) {
  let Some(asks) = state.indirect.asks.remove(&target) else {
    return;
  };
  for (requester, nonce) in asks {
    state
      .indirect
      .results
      .entry(requester)
      .or_default()
      .insert(target, nonce);
    count_refusal_in(state, PROBE_INDIRECT_RELAYED);
    wake_probe_task(state, requester);
  }
}

/// Counts one refusal or event of `kind` on a shard state already borrowed (the in-closure form of
/// [`count_refusal`], which borrows the state itself).
fn count_refusal_in(state: &mut ShardState, kind: &'static str) {
  let count = state.refusals.entry(kind).or_insert(0);
  *count = count.saturating_add(1);
}

/// Carries this node's pending indirect-probe traffic for `peer` over the probe session its task owns,
/// before that task's own probe: the ping-requests this node posted for `peer` to relay (this node is the
/// requester, `peer` the relay), and the answers this node owes `peer` as a relay (`peer` is the
/// requester). Each is one bounded exchange under the probe budget ([`deliver_once`]); an undelivered one
/// is counted and dropped — the requester's next direct miss asks again — and a terminal fault releases the
/// session exactly as a probe's would. Returns the session for the probe that follows.
async fn carry_indirect_traffic(
  session: Option<Endpoint>,
  local: HostId,
  local_boot_nonce: u64,
  peer: &ProbedPeer,
  fanout: usize,
  timing: &ProbeTiming,
) -> Option<Endpoint> {
  let mut open = session?;
  let (requests, answers) = state::with_state(|s| {
    (
      s.indirect.outgoing.remove(&peer.host).unwrap_or_default(),
      s.indirect.results.remove(&peer.host).unwrap_or_default(),
    )
  })
  .unwrap_or_default();
  if requests.is_empty() && answers.is_empty() {
    return Some(open);
  }
  let budget = timing.budget(path_tail_ns(peer.host));
  let gossip = || {
    state::with_state(|s| {
      let belief = s.fleet.membership().state(peer.host);
      outgoing_probe_gossip(s, peer.host, belief, fanout)
    })
    .unwrap_or_default()
  };
  let mut messages: Vec<SwimMessage> = Vec::with_capacity(requests.len() + answers.len());
  for (target, nonce) in requests {
    messages.push(SwimMessage::PingReq {
      from: local,
      target,
      nonce,
      gossip: gossip(),
    });
  }
  for (target, nonce) in answers {
    messages.push(SwimMessage::IndirectAck {
      from: local,
      target,
      nonce,
      boot_nonce: local_boot_nonce,
      gossip: gossip(),
    });
  }
  for message in &messages {
    let (returned, delivery) = deliver_once(open, message, budget).await;
    match delivery {
      Delivery::Delivered => {}
      Delivery::Undelivered => {
        count_refusal(PROBE_INDIRECT_UNDELIVERED);
      }
      Delivery::Broken => {
        count_refusal(PROBE_BROKEN);
        return None;
      }
    }
    open = returned?;
  }
  Some(open)
}

/// Applies authenticated gossip to the shared failure view (§4.8). Third-member reports
/// never enter this session's probe rotation. Unknown identities require enrollment; a
/// superseded identity cannot regain membership through a relayed alive report.
fn receive_probe_gossip(
  state: &mut ShardState,
  detector: &mut Detector,
  sender: HostId,
  gossip: &[(HostId, MemberState)],
) {
  let local = state.fleet.host();
  for &(subject, update) in gossip {
    if subject == local {
      state.fleet.observe(local, update);
      detector.apply(local, update);
    } else if state.fleet.membership().state(subject).is_some() || subject == sender {
      if update.liveness == Liveness::Alive && !state.authenticated_members.contains(&subject) {
        continue;
      }
      if subject == sender {
        detector.apply_gossip_from(sender, &[(subject, update)]);
      }
      if apply_peer_state(&mut state.fleet, subject, Some(update)) {
        wake_link_waiter_of(state, subject);
      }
    }
  }
}

/// One shared dissemination queue feeds every live session. Reserve payload entries for
/// our own current incarnation and the target's buddy suspicion/death; the rest carries
/// adopted changes. The existing derived fanout bounds the whole packet.
fn outgoing_probe_gossip(
  state: &mut ShardState,
  peer: HostId,
  belief: Option<MemberState>,
  fanout: usize,
) -> Vec<(HostId, MemberState)> {
  let mut mandatory = Vec::new();
  let local = state.fleet.host();
  if let Some(own) = state.fleet.membership().state(local) {
    mandatory.push((local, own));
  }
  if let Some(belief) = belief
    && belief.liveness != Liveness::Alive
  {
    mandatory.push((peer, belief));
  }
  let reports = state.fleet.gossip(
    fanout.saturating_sub(mandatory.len()),
    u32::try_from(fanout).unwrap_or(u32::MAX),
  );
  for report in reports {
    if !mandatory.iter().any(|(subject, _)| *subject == report.0) {
      mandatory.push(report);
    }
  }
  mandatory
}

/// Switches a probe task to the id its peer now holds (task #22): whichever side learned a restart since the
/// last period — the task itself from an acknowledgement, or the serve side from the peer's own probe — the
/// peer's previous id is folded **dead** in this task's detector (never probed again, its death handed to
/// the other shards as any fold is) and the new id joins fresh; the mesh record moves with it, and the
/// round-trip law starts over for what is a new member on the same wire. A no-op while the peer's id is
/// unchanged, which is every period but the one after a restart.
fn follow_current_id(
  detector: &mut Detector,
  peer: &mut ProbedPeer,
  timing: &mut ProbeTiming,
  origin: u16,
  shards: &[u16],
) {
  let Some(current) = current_member(peer.anchor) else {
    return;
  };
  if current == peer.host {
    return;
  }
  let old = peer.host;
  let death = MemberState {
    liveness: Liveness::Dead,
    incarnation: detector
      .membership()
      .state(old)
      .map_or(0, |belief| belief.incarnation),
  };
  detector.apply(old, death);
  detector.join(current);
  state::with_state(|s| {
    if s.formed_probe_peers.remove(&old) {
      s.formed_probe_peers.insert(current);
    }
    wake_link_waiter_of(s, old);
  });
  for shard in shards.iter().copied().filter(|shard| *shard != origin) {
    let _ = run_on(origin, shard, move |s| {
      apply_peer_state(&mut s.fleet, old, Some(death));
    });
  }
  *timing = ProbeTiming::new();
  // The old id's path estimate goes with it: a restarted node is a new member on the same wire, and its
  // path is measured afresh under the new id (bounded: one estimate per live rostered id).
  let _ = state::with_state(|s| s.peer_paths.remove(&old));
  peer.host = current;
}

/// The probe side: dial the peer's probe address (so a slow or not-yet-listening peer never blocks the other
/// peers' loops — each probe task dials its own), then each protocol period probe the peer, fold the outcome,
/// and fold this detector's converged view into the shard's `FleetNode` (§4.8). Each peer has its own probe
/// task and its own detector; `sync_membership` folds each detector's view without disturbing the peers it
/// does not track, so N detectors compose into one membership. The session is **reused whatever a probe's
/// outcome** — an acknowledgement and a timeout both hand it back ([`probe_once`]) — so a single missed
/// probe (a lost packet, a peer too starved to answer in time) does not drop it: the next period re-probes
/// at a backed-off deadline ([`ProbeTiming`]), a still-live peer refutes the suspicion every ping to it
/// carries, and only a peer silent across the suspicion window ages to death and is retired (driving the
/// takeover). Dropping the session on one miss would retire a live peer on any transient glitch
/// (`docs/bugs/2026-09-10-swim-stale-ack.md`); a re-dial now replaces a lost session at the peer, but a
/// probe verdict still rests on the suspicion window, not on one miss.
async fn probe_peer(
  identity: &'static Identity,
  dial: PeerDial,
  local: HostId,
  neighbourhood: usize,
) {
  let PeerDial {
    anchor,
    host: seed,
    name,
    address,
    certificate,
    resolver,
  } = dial;
  // The peer's current member id, its seed until learned otherwise (task #22): the loop below follows it.
  let mut peer = ProbedPeer { anchor, host: seed };
  let local_boot_nonce = state::with_state(|s| s.member_boot_nonce).unwrap_or(0);
  let mut probe_timing = ProbeTiming::new();
  let timing = detector_timing(neighbourhood);
  let fanout = usize::try_from(timing.gossip_transmits).unwrap_or(1);
  let mut detector = Detector::new(local, timing);
  detector.join(peer.host);
  // This shard (the control shard) probes; every other shard is an owner with its own copy of the
  // configuration (D-7), so each state this detector folds is handed to the rest as well.
  let (origin, shards) = state::with_state(|s| (s.shard, s.shards.clone())).unwrap_or_default();
  // Bring the probe session up on one socket, retrying the handshake there each period until it completes —
  // so a formation-race handshake that partially reached the peer's pinned `accept` finishes rather than
  // stranding the session (which would leave this peer unprobed and the N·(N−1) mesh un-formed). Once up it
  // is reused whatever a probe's outcome (see [`probe_once`]). The detector ticks only when a probe is
  // actually sent, so an as-yet-unestablished session never resolves as a missed probe and falsely ages the
  // peer.
  let mut client: Option<Endpoint> = client_for(
    identity,
    &name,
    (&address, crate::deploy::Plane::Probe),
    &certificate,
    resolver,
  )
  .await;
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
    follow_current_id(&mut detector, &mut peer, &mut probe_timing, origin, &shards);
    // Idle while this peer is retired; on the resume, the detector is realigned to the re-admitted belief.
    // A retirement this task learns here — the shard's membership already says so, from another task's
    // fold or an injected death — releases the probe session and any pending dial exactly as one its
    // own fold finds below does; before, a task that went idle this way kept both, so the resume re-used
    // a session to a process that was gone and drove a dial at an address the peer had left.
    if !resume_if_in_mesh(&mut detector, peer.host, &mut was_idle) {
      release_probe_session(peer.host, &mut session, &mut client, &mut recorded_mesh);
      futures::sleep(HEARTBEAT_NS).await;
      continue;
    }
    (client, session) = establish_session(
      client,
      session,
      identity,
      &name,
      (&address, crate::deploy::Plane::Probe),
      &certificate,
      resolver,
    )
    .await;
    if session.is_some() && !recorded_mesh {
      // The direct probe session to this peer has formed — record it, so the daemon can tell the real mesh
      // is up (`fleet_meshed`) rather than trusting the membership's optimistically seeded alive set.
      state::with_state(|s| s.formed_probe_peers.insert(peer.host));
      recorded_mesh = true;
    }
    if session.is_some() {
      probe_nonce += 1;
      session = probe_cycle(
        &mut detector,
        session.take(),
        local,
        local_boot_nonce,
        &peer,
        fanout,
        probe_nonce,
        &mut probe_timing,
      )
      .await;
    }

    let retired = fold_peer_state(&detector, peer.host, origin, &shards);
    if retired {
      // The peer is retired and gone from the direct mesh. Its objects' phase-one recovery is now driven by
      // the record-ship task (over the surviving candidate holders); this node's **outgoing** probe session
      // to the peer is dropped (below). The task does **not** end — the top of the loop idles it until the
      // peer rejoins, so a false retirement (or a restart) heals without a supervisor re-spawning anything.
      //
      // The peer's **incoming** sessions to this node are deliberately left alone. They are owned by their
      // serve tasks and reclaimed by the demultiplexer's authenticated-replacement mechanism: when the peer
      // re-dials (a restart, or a refutation after a false death), its fresh handshake replaces its own prior
      // session under its certificate (`Endpoint::connection_id` → `Demux::bind`), ending the stale one.
      // Force-closing them here **by certificate** (`Demux::close_peer`) was the KIND whole-pod-restart bug
      // (2026-09-14): a restart presents the same operator certificate, so a same-id replacement that had
      // already re-dialed and bound its serve session was the session `close_peer` tore down — the very
      // session carrying this node's death belief back for the peer to self-refute. The survivor and the
      // restart then aged each other out into a circular wait, both `fleet_meshed` vacuously
      // (`docs/bugs/2026-09-14-retirement-closes-the-same-id-restarts-serve-session.md`). A peer that never
      // returns holds one idle serve slot per plane, bounded by the roster (banned item 8 holds).
      release_probe_session(peer.host, &mut session, &mut client, &mut recorded_mesh);
    }
    // The next probe waits one beat at full health, more as this node's own probes fail (Lifeguard) — cut
    // short the moment indirect-probe traffic is posted for this task, so a relay probes its target, and a
    // requester credits a relayed answer, within a round trip rather than a period.
    sleep_or_wake(probe_period_ns(detector.health_multiplier()), peer.host).await;
  }
}

/// What a probe task lets go of when its peer is retired, whichever path told it — its own fold at the
/// bottom of a cycle or the shard's membership at the top: the direct mesh record, this node's
/// **outgoing** probe session (the peer's incoming ones are the serve tasks' and the demultiplexer's,
/// see [`probe_peer`]), and a dial still in its handshake (rejoin design item 3): that dial's socket
/// points at the address the peer had, and driving its pending flight through the remaining handshake
/// budgets after the peer returns would spend them on a stale address (a rescheduled pod's old IP)
/// before `client_for` re-resolves; dropped now, the resume dials afresh — the discovered or resolved
/// address — on its first period. The drop of a pending dial is counted (`fleet.dial.stale_dropped`),
/// so the test that retires a peer mid-dial can see the pending dial was there.
fn release_probe_session(
  peer_host: HostId,
  session: &mut Option<Endpoint>,
  client: &mut Option<Endpoint>,
  recorded_mesh: &mut bool,
) {
  state::with_state(|s| {
    s.formed_probe_peers.remove(&peer_host);
    // A retired peer relays nothing and is asked about nothing: its indirect-probe queues are dropped, so a
    // request naming it is refused thereafter rather than queued for a task that idles.
    s.indirect.forget(peer_host);
  });
  *session = None;
  *recorded_mesh = false;
  if client.take().is_some() {
    count_refusal(DIAL_STALE_DROPPED);
  }
}

/// The probe **cadence** — how long the probe task waits before its next probe of a peer — dilated by the
/// detector's Lifeguard local-health multiplier (`health + 1`, capped at [`LOCAL_HEALTH_CAP`] + 1): a node
/// whose own probes are failing probes less aggressively (Lifeguard §3.1's local-health-aware probe;
/// memberlist scales its probe interval by its awareness score), so a degraded prober neither floods a
/// struggling peer nor counts misses faster than its own health warrants —
/// [`Detector::health_multiplier`] is defined as the caller's multiplier on its probe-period timer.
/// Derived: one heartbeat period ([`HEARTBEAT_NS`], the beat every fleet loop runs at) × the multiplier,
/// so full health is exactly the beat. Measured 2026-09-13: first wired, it was **wrongly** rejected on a
/// 495 s retirement hang that was a holder acceptor born stale (`accept_held_record`, fixed in
/// `docs/bugs/2026-09-13-holder-acceptor-born-stale-never-placed.md`); re-measured on the fixed tree the
/// same test passes 3/3 at 11.3 s with the dilation and the starvation test at 8.8 s.
fn probe_period_ns(health_multiplier: u32) -> u64 {
  // The Lifeguard dilation (heartbeat × the health multiplier) OR the measured scheduler quantum,
  // whichever is longer — so a shard that is itself descheduled paces its probes no finer than it is
  // actually scheduled, and the suspicion window (this cadence × the probes it counts) dilates with the
  // observed starvation rather than a fixed 100 ms beat.
  let health_period = HEARTBEAT_NS.saturating_mul(u64::from(health_multiplier));
  let quantum = scheduler_quantum_ns();
  if quantum > health_period {
    let _ = state::with_state(|s| {
      s.probe_windows.periods_dilated = s.probe_windows.periods_dilated.saturating_add(1);
      s.probe_windows.largest_quantum_ns = s.probe_windows.largest_quantum_ns.max(quantum);
    });
  }
  health_period.max(quantum)
}

/// The record serve side (§4.8 "records are sent to all candidates"; "Promotion and takeover"): complete
/// the accepted session's handshake and loop answering the peer over this node's per-object holds — a
/// **record** commit is accepted into the object's durable acceptor (this node backs the peer as a
/// candidate holder), and a **prepare** (a new owner's phase-one message when it takes over an object) is
/// answered from that same hold with a binding promise. Both are served here because the session plane
/// carries both and `serve_once` hands the handler the raw request either way; the two never collide, a
/// [`Prepare`] being a fixed 40 bytes and a [`Record`] always longer (its prefix alone exceeds that), so
/// the length disambiguates. The acceptor authorizes the record's or prepare's owner and configuration generation against
/// the authority the takeover installed, fences the epoch, and refuses a stale writer. The peer is whoever
/// the handshake authenticated — its certificate names it in `roster` (mutual TLS admits only roster
/// certificates; one not found is counted, never served). A serve failure (the peer's connection dropped
/// when it died, or its session replaced by a re-dial) ends the loop.
async fn serve_peer_records(
  mut endpoint: Endpoint,
  local: HostId,
  roster: Vec<Rostered>,
  driver: &'static PeerDriver,
) {
  if let Err(e) = endpoint.establish().await {
    count_accept_failure(&e, "record");
    return;
  }
  // Two ids for the authenticated peer (task #22 two-id model). `peer_anchor` is its **stable** anchor — the
  // one the roster holds for the certificate it presented, the same anchor its member ids derive from and its
  // announcements are validated against, so every plane names the node by one id. The RIFL completion origin
  // keys on it, so a forwarded write stays exactly-once across the forwarding node's restart. Its **ephemeral**
  // member id — what records and ownership key on — is whatever is currently learned for that anchor (its
  // seed until it announces itself; a restart moves it), resolved per request so a node that restarted
  // mid-session is served under the id it now writes as.
  let Some(certificate) = endpoint.peer_certificate() else {
    count_refusal(ACCEPT_REFUSED);
    return;
  };
  let peer_anchor = roster
    .iter()
    .find(|peer| peer.certificate == certificate)
    .map_or_else(
      || crate::deploy::host_id_of_certificate(&certificate),
      |peer| peer.anchor,
    );
  let seed = crate::deploy::member_id(peer_anchor, 0);
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
  loop {
    if !enrolled {
      enrolled = state::with_state(|state| {
        state
          .discovery
          .as_ref()
          .and_then(|discovery| discovery.recognizes(&certificate))
      })
      .flatten()
      .is_some();
    }
    let served = endpoint
      .serve_once_async(|stream, request| {
        let certificate = certificate.clone();
        async move {
          if stream == crate::discovery::STREAM {
            // Test support: a fault holds this reply after the request arrived (never reachable from the
            // wire), so a peer's exchange is pending — its endpoint borrowed — while this node is then
            // stopped: the interrupted-discovery restart the replacement-voter regression forces.
            while state::with_state(|state| state.discovery_withhold_replies).unwrap_or(false) {
              futures::sleep(HEARTBEAT_NS / POLL_PER_PERIOD).await;
            }
            let outcome =
              state::with_state(|state| crate::discovery::serve(state, &certificate, &request));
            return match outcome {
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
            };
          }
          if !enrolled {
            count_refusal(ACCEPT_REFUSED);
            return Vec::new();
          }
          match stream {
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
            PROMOTE_STREAM => Prepare::decode(&request)
              .ok()
              .and_then(|prepare| {
                state::with_state(|s| {
                  let peer_host = s
                    .learned_members
                    .get(&peer_anchor)
                    .map_or(seed, |learned| learned.host);
                  serve_held_promotion(s, peer_host, &prepare)
                })
              })
              .unwrap_or_default(),
            stream if is_content_stream(stream) => state::with_state(|s| {
              // A test's injected placement refusal (§4.16 placed-before-reference): a holder that
              // refuses every content put, counted, so an owner's record is shown to wait on it.
              if s.merge.fault.refuse_content_puts && stream == CONTENT_PUT_STREAM {
                *s.refusals
                  .entry(crate::merge_service::CONTENT_PUT_REFUSED)
                  .or_insert(0) += 1;
                return Vec::new();
              }
              s.held_content.serve(local, &request)
            })
            .unwrap_or_default(),
            // A green's merge record (§4.16 "Apply on holders"): recomputed before it is accepted.
            crate::merge_service::MERGE_RECORD_STREAM => Record::decode(&request)
              .ok()
              .and_then(|record| {
                state::with_state(|s| {
                  let peer_host = s
                    .learned_members
                    .get(&peer_anchor)
                    .map_or(seed, |learned| learned.host);
                  crate::merge_service::accept_merge_record(s, local, peer_host, &record)
                })
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
            FORWARD_STREAM => verbs::serve_forward(control, peer_anchor, &request).await,
            crate::owner_location::STREAM => {
              state::with_state(
                |state| match crate::owner_location::serve(state, &request) {
                  Ok(reply) => reply,
                  Err(error) => {
                    *state.refusals.entry(error.counter()).or_insert(0) += 1;
                    Vec::new()
                  }
                },
              )
              .unwrap_or_default()
            }
            _ => Vec::new(),
          }
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
pub(crate) fn serve_held_promotion(
  state: &mut ShardState,
  peer_host: HostId,
  prepare: &Prepare,
) -> Vec<u8> {
  if prepare.owner != peer_host {
    return encode_refusal(&RegisterError::Unauthorized);
  }
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
pub(crate) fn accept_held_record(
  state: &mut ShardState,
  local: HostId,
  peer_host: HostId,
  record: &Record,
) -> Vec<u8> {
  if let Err(error) = check_held_record(state, local, peer_host, record) {
    return encode_refusal(&error);
  }
  let generation = state.fleet.configuration().version;
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
      let mut acceptor = Acceptor::new(
        local,
        Authority {
          generation,
          owner: peer_host,
        },
      );
      let result = acceptor.accept(record);
      if result.is_ok() {
        state.holder_records.insert(record.object, acceptor);
      }
      result
    }
  };
  match accepted {
    Ok(ack) => {
      match state
        .fleet
        .track_object(record.object, peer_host, state.council.configuration())
      {
        Ok(()) => ack.encode(),
        Err(error) => encode_refusal(&error),
      }
    }
    // A refusal the sender acts on rides back on the wire (a `ConfigurationStale` naming this node's newer
    // version, so a stale sender refreshes and retries); every other refusal is an empty reply the sender
    // counts as no acknowledgement ([`encode_refusal`]).
    Err(error) => encode_refusal(&error),
  }
}

/// Validates a record against both the transport principal and installed authority before any effect
/// (§4.8, §4.16; AUD-10/AUD-12). The first record and all later records have the same peer binding.
/// A merge holder calls this before recomputing, then accepts in the same synchronous shard turn.
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
  round: CommitBudget,
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
      && !start_seal(
        state,
        local,
        object,
        handle,
        head,
        sequence,
        created_unix,
        false,
      )
    {
      continue;
    }
    if !advance_seal(state, object, handle, slice_bytes) {
      continue;
    }
    if let Some(item) = content_work(state, object, quorum, round) {
      work.push(item);
    }
  }
  heal_one_placed_snapshot(state, local, created_unix);
  work
}

/// The healer's step (§4.10 "anti-entropy walks Merkle manifests between recorded holders and repairs only
/// differing subtrees; the healer replays puts …"; §4.8 "Derived constants": "healer cadence from the
/// measured put-failure rate"): once per healer period ([`heal_period_ns`]) it re-opens a seal job for the
/// **next** owned volume whose head snapshot is recorded placed — a `healing` job, seeded with the owner as
/// the only acknowledged holder — so the ordinary content rounds re-**offer** the archive to every candidate:
/// a holder that lost nothing answers with an empty missing set and is put zero bytes (the Merkle identity
/// diff finds no differing subtree), a holder that lost content is put exactly what it lacks and counts as
/// a repair, and the placement is re-recorded. One snapshot per period, in volume-id order, wrapping: a
/// bounded slice of the walk over everything this node has placed (§4.3 bounded work). Nothing to do while
/// a seal for the volume is already in progress, on a laptop (nothing is placed remotely), or between steps.
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
      if record.epoch == 0 || state.seals.contains_key(&object) {
        return None;
      }
      let snapshot = state.db.partition().snapshot(*id, record.head)?;
      matches!(snapshot.placed, PlacementState::Placed { .. }).then_some((
        *id,
        *handle,
        object,
        record.head,
        record.epoch,
      ))
    });
  state.healer.last_step_ns = Some(now);
  let Some((id, handle, object, head, sequence)) = candidate else {
    state.healer.after = None;
    return;
  };
  state.healer.after = Some(id);
  start_seal(
    state,
    local,
    object,
    handle,
    head,
    sequence,
    created_unix,
    true,
  );
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
/// counted, if the snapshot cannot be walked. A `healing` seal is the healer's re-offer of an
/// already-placed snapshot ([`heal_one_placed_snapshot`]).
#[allow(clippy::too_many_arguments)]
fn start_seal(
  state: &mut ShardState,
  local: HostId,
  object: ObjectId,
  handle: slates_mem::Handle<crate::state::VolumeSlot>,
  head: DbSnapshotId,
  sequence: u64,
  created_unix: u64,
  healing: bool,
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
      first_round_at_ns: None,
      healing,
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
      job.manifest = Some(archive.manifest_identity());
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

/// The measured put latency of the content class on one owner shard (§4.8 "Derived constants": "hedge
/// delay = measured p95 put latency per class"): the newest [`PUT_LATENCY_WINDOW`] readings in arrival
/// order, each the time from a put round's dispatch to one holder's verified acknowledgement. The p95 over
/// them — the reading nineteen in twenty acknowledgements arrive within — is the hedge trigger
/// ([`hedge_delay_ns`]). Bounded: the oldest reading leaves as the newest arrives (banned item 8).
#[derive(Debug, Default)]
pub struct PutLatency {
  readings_ns: std::collections::VecDeque<u64>,
}

impl PutLatency {
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
fn hedge_delay_ns(latency: &PutLatency) -> u64 {
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
fn content_budget(latency: &PutLatency, round: CommitBudget) -> CommitBudget {
  let hedge_delay = hedge_delay_ns(latency);
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

/// Folds one holder's bound content acknowledgement into `job` — the same merge whether it arrived in the
/// round that dispatched it or later, as a straggler ([`LateReplies::Content`]); the placement is the
/// distinct acknowledging candidates, so a repeat is inert.
fn fold_content_ack(job: &mut SealJob, holder: HostId) {
  if !job.content.acked.contains(&holder) {
    job.content.acked.push(holder);
  }
}

/// The content put `object`'s seal calls for this period — its archive moved out for the dispatch — or
/// `None` once the content is placed (the head naming it then ships through `unplaced_heads`), while the
/// archive is out on a put, or while the **first round is still within the hedge delay**: the hedge round
/// to the remaining candidates goes out only once the first round has been outstanding for longer than the
/// measured p95 put latency ([`hedge_delay_ns`]) — the design's trigger, replacing the period the rounds
/// happen to be driven at (§4.8 "hedged to the remaining candidates after the measured p95 put latency").
fn content_work(
  state: &mut ShardState,
  object: ObjectId,
  quorum: Quorum,
  round: CommitBudget,
) -> Option<ContentWork> {
  let hedge_delay = hedge_delay_ns(&state.put_latency);
  let budget = content_budget(&state.put_latency, round);
  let now = state.clock.monotonic_ns();
  let job = state.seals.get_mut(&object)?;
  if job.content.placed(quorum) {
    return None;
  }
  let hedged = match job.first_round_at_ns {
    Some(first_round_at) if now.saturating_sub(first_round_at) < hedge_delay => return None,
    Some(_) => true,
    None => false,
  };
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
  let hedge = usize::try_from(work.quorum.f).unwrap_or(0);
  let remote: Vec<HostId> = work
    .candidates
    .iter()
    .copied()
    .filter(|host| *host != local && !work.acked.contains(host))
    .collect();
  let targets = hedge_targets(work.hedged, remote, hedge);
  let holders = take_sessions(|host| targets.contains(&host));
  let dispatched_ns = futures::now_ns();
  let manifest = work.archive.manifest_identity();
  let (placement, latencies_ns, refilled, dispatch) = if holders.is_empty() {
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
        shard: work.shard,
        object: work.object,
        snapshot: work.snapshot,
        sequence: work.sequence,
        manifest,
        dispatched_ns,
      },
    );
    return_sessions(placed.reusable);
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
    shard,
    object,
    snapshot,
    archive,
    round,
    quorum,
    ..
  } = work;
  let _ = run_on(origin, shard, move |s| {
    for (_, latency_ns) in &latencies_ns {
      s.put_latency.record(*latency_ns);
    }
    let Some(job) = s.seals.get_mut(&object) else {
      return; // The seal was superseded meanwhile; its archive is dropped with it.
    };
    if job.snapshot != snapshot {
      return;
    }
    job.archive = Some(archive);
    if round == 0 && job.first_round_at_ns.is_none() {
      job.first_round_at_ns = Some(dispatched_ns);
    }
    if let Some(placement) = placement {
      job.rounds = job.rounds.saturating_add(1);
      // A healing round that shipped bytes to a holder repaired it: that holder had lost content the
      // placement recorded it as holding (§4.10 "repairs only differing subtrees").
      if job.healing {
        let repaired = refilled
          .iter()
          .filter(|host| placement.acked.contains(host))
          .count();
        s.repairs = s
          .repairs
          .saturating_add(u64::try_from(repaired).unwrap_or(u64::MAX));
      }
      for host in placement.acked {
        fold_content_ack(job, host);
      }
      s.put_outcomes.record(job.content.placed(quorum));
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

/// One period's materialization of everything this node took over: the plain volumes whose heads it
/// adopted ([`materialize_pending`]), then the greens whose newest merge records it adopted
/// ([`materialize_pending_greens`]).
async fn materialize_adopted_objects(origin: u16, budget: CommitBudget) {
  materialize_pending(origin, budget).await;
  materialize_pending_greens(origin).await;
}

/// Materializes each taken-over **green** whose newest merge record this node adopted (§4.16
/// owner-loss recovery; AUD-14) into an owned green **on the shard its id routes to**: the chain is
/// gathered on this control shard from the node's own accepted merge records and the inputs it holds
/// (`merge_service::recover_green_inputs`), moved by value to the owner shard, and there re-recorded
/// durably and replayed into a fresh engine whose head identity must equal the adopted record's
/// (`verbs::materialize_taken_over_green`). A green whose inputs are not all held, or whose adopted
/// version lies beyond this node's accepted prefix (the general ledger-prefix transfer, GAP-A9-7, is
/// still owed), is counted and stays pending; a mismatch is fatal-and-loud for the green here.
async fn materialize_pending_greens(origin: u16) {
  let pending: Vec<(ObjectId, crate::merge_service::MergeRecordValue)> = state::with_state(|s| {
    s.pending_green_materializations
      .iter()
      .map(|(object, adopted)| (*object, adopted.clone()))
      .collect()
  })
  .unwrap_or_default();
  for (object, adopted) in pending {
    let recovery =
      state::with_state(|s| crate::merge_service::recover_green_inputs(s, object, &adopted))
        .flatten();
    let Some(recovery) = recovery else {
      count_refusal(GREEN_TAKEOVER_INCOMPLETE);
      continue;
    };
    let id = DbVolumeId { bytes: object.0 };
    let partition = verbs::owner_of(slates_ipc::protocol::VolumeId { bytes: object.0 });
    // The takeover's placement of the adopted record (its promotion epoch and acknowledging holders),
    // recorded where the promotion ran; it moves to the owner shard with the green, as a head's does,
    // so the successor's record plane writes the green's next versions at the promotion epoch.
    let Some((target, placed)) = state::with_state(|s| {
      let target = s.shards.get(usize::from(partition)).copied()?;
      let placed = s.placed_heads.get(&object).cloned()?;
      Some((target, placed))
    })
    .flatten() else {
      continue;
    };
    let served = call_within(
      origin,
      target,
      move |s| {
        let served = verbs::materialize_taken_over_green(s, id, recovery).is_ok();
        if served {
          s.placed_heads.insert(object, placed);
        }
        served
      },
      HEARTBEAT_NS,
    )
    .await;
    state::with_state(|s| {
      if served == Some(true) {
        s.pending_green_materializations.remove(&object);
      } else {
        *s.refusals.entry(GREEN_TAKEOVER_REFUSED).or_insert(0) += 1;
      }
    });
  }
}

/// A taken-over green whose chain could not be gathered this period (an input not held, or the adopted
/// version beyond this node's accepted prefix); it stays pending and is retried.
const GREEN_TAKEOVER_INCOMPLETE: &str = "merge.takeover_incomplete";
/// A taken-over green the owner shard refused to materialize (a recomputed identity that does not
/// match the adopted record's, or a catalog refusal); it stays pending, counted.
const GREEN_TAKEOVER_REFUSED: &str = "merge.takeover_refused";

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

/// The status refusal counts under which a dial task records a peer's DNS name that did not resolve, by
/// cause — the name itself invalid (`fleet.resolve`), no resolver configured, every attempt timed out,
/// `NXDOMAIN` (a pod not yet created, or a name the cluster's DNS does not serve), an answer without an
/// `A` record, a malformed reply, the runtime refusing the socket: the dial is skipped this period and made
/// again the next, so a count that keeps rising names a peer the fleet cannot reach by name and says why.
/// Format: refusal names in the daemon's status report, alongside the verbs' refusal kinds.
/// A probe session released on a terminal transport fault (`ProbeOutcome::Broken`): the next period
/// dials afresh.
const PROBE_BROKEN: &str = "fleet.probe.broken";
/// A ping-request posted for a relay after a direct probe timed out (one per relay asked) — the indirect
/// stage began (§4.8; AUD-15).
const PROBE_INDIRECT_REQUESTED: &str = "fleet.probe.indirect.requested";
/// This node, as a relay, reached a target on a requester's behalf and posted the answer — the relay path
/// carried a real acknowledgement (the non-vacuity the indirect-probe regression asserts on the relay).
const PROBE_INDIRECT_RELAYED: &str = "fleet.probe.indirect.relayed";
/// A relayed acknowledgement was credited to this node's own probe of the target before the suspicion
/// verdict — a lost direct packet did not suspect a live peer (the non-vacuity asserted on the requester).
const PROBE_INDIRECT_ACKED: &str = "fleet.probe.indirect.acked";
/// An indirect-probe message refused at the serve side: a ping-request whose sender is not the session's
/// learned member, or one naming a target this node keeps no direct contact with (the bound on the
/// queues), or a relayed acknowledgement for a peer this node does not probe.
const PROBE_INDIRECT_REFUSED: &str = "fleet.probe.indirect.refused";
/// An indirect-probe message that did not reach its peer within the probe budget (the session kept).
const PROBE_INDIRECT_UNDELIVERED: &str = "fleet.probe.indirect.undelivered";
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
/// A record session returned to a slot that no longer expects it — a retired peer's, a slot re-established
/// since, or a session other than the one borrowed — dropped rather than installed over a newer one.
const LINK_STALE_RETURN: &str = "fleet.link.stale_return";
const RESOLVE_REFUSED: &str = "fleet.resolve";
const RESOLVE_NO_RESOLVER: &str = "fleet.resolve.no-resolver";
const RESOLVE_TIMEOUT: &str = "fleet.resolve.timeout";
const RESOLVE_NXDOMAIN: &str = "fleet.resolve.refused";
const RESOLVE_NO_ADDRESS: &str = "fleet.resolve.no-address";
const RESOLVE_MALFORMED: &str = "fleet.resolve.malformed";
const RESOLVE_IO: &str = "fleet.resolve.io";

/// The status refusal counts under which a dial task records a handshake that ran out its budgets with
/// the peer silent (`fleet.dial.redial`: the socket is dropped and the next period dials afresh) and one
/// the transport faulted (`fleet.dial.fault`: a peer whose half-open state for this source is gone, a
/// certificate the pin refuses, a malformed flight). A count that keeps rising names a peer this node
/// cannot establish a session with, and the log's first line of each says why.
/// Format: refusal names in the daemon's status report, alongside the verbs' refusal kinds.
const DIAL_REDIAL: &str = "fleet.dial.redial";
const DIAL_FAULT: &str = "fleet.dial.fault";

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
fn count_accept_failure(error: &EndpointError, plane: &str) {
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
    eprintln!("slates-server: fleet: a dialer's {plane}-plane handshake ended: {error:?}");
  }
}

/// Counts a fleet-loop refusal in the shard's status refusal counts, so a peer the loop could not set up is
/// visible to an operator (the mesh will not form to it) rather than a swallowed error (banned item 9).
pub(crate) fn count_refusal(kind: &'static str) -> u64 {
  state::with_state(|s| {
    let count = s.refusals.entry(kind).or_insert(0);
    *count += 1;
    *count
  })
  .unwrap_or(0)
}

/// Whether this node keeps **direct contact** with `peer` — a record session dialed and kept up for the
/// coordinator ([`establish_record_link`]) and a SWIM probe of its own ([`probe_peer`]): the peers the
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
/// time. A retired peer that comes back is re-admitted alive by its own probes ([`serve_peer_probes`]) and
/// regains contact then — which is what reaching the surviving voters makes possible.
pub(crate) fn keeps_direct_contact_with(state: &ShardState, peer: HostId) -> bool {
  let believed_dead = state
    .fleet
    .membership()
    .state(peer)
    .is_some_and(|belief| belief.liveness == Liveness::Dead);
  !believed_dead
    && (!state.council.initialized()
      || !state.root.initialized()
      || !state.council.configuration().members.contains(&peer)
      || state.fleet.configuration().neighbourhood.contains(&peer)
      || state.council.is_voter(peer)
      || state.root.is_voter(peer))
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
async fn establish_record_link(driver: &'static PeerDriver, dial: PeerDial) {
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
    let retired = state::with_state(|s| !keeps_direct_contact_with(s, peer_host));
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
      client = kept;
      enroll_record_session(session, peer_host, anchor, &mut discovery_cursor, driver).await;
    }
    if !absent {
      refresh_discovery(peer_host, anchor, &mut discovery_cursor, driver).await;
    }
    futures::sleep(HEARTBEAT_NS).await;
  }
}

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
  let current = current_member(anchor).unwrap_or(previous);
  if current != previous {
    state::with_state(|state| state.record_sessions.remove(&previous));
    if client.take().is_some() {
      count_refusal(DIAL_STALE_DROPPED);
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
  driver: &'static PeerDriver,
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
    });
  }
}

/// One discovery page over the peer's installed record link, the session borrowed for it and put back
/// **only into the entry it left** — an entry removed meanwhile (the peer replaced or retired) is never
/// re-created by a late page; any outcome but an answer releases the session and the link re-dials.
async fn refresh_discovery(
  peer_host: HostId,
  anchor: HostId,
  cursor: &mut crate::discovery::Cursor,
  driver: &'static PeerDriver,
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
      } else {
        state.record_sessions.remove(&peer_host);
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
}

impl DiscoveryFault {
  fn counter(self) -> &'static str {
    match self {
      DiscoveryFault::Deadline => DISCOVERY_DEADLINE,
      DiscoveryFault::Invalidated => DISCOVERY_INVALIDATED,
      DiscoveryFault::Transport => DISCOVERY_TRANSPORT,
    }
  }

  fn outcome(self) -> DiscoveryOutcome {
    match self {
      DiscoveryFault::Deadline => DiscoveryOutcome::Deadline,
      DiscoveryFault::Invalidated => DiscoveryOutcome::Invalidated,
      DiscoveryFault::Transport => DiscoveryOutcome::Transport,
    }
  }
}

/// Whether the link to `anchor` still addresses `expected`: the anchor's learned member is still `expected`
/// (or none was learned yet, the seed standing) and this node still keeps direct contact with it.
fn link_valid(anchor: HostId, expected: HostId) -> bool {
  state::with_state(|s| {
    s.learned_members
      .get(&anchor)
      .is_none_or(|learned| learned.host == expected)
      && keeps_direct_contact_with(s, expected)
  })
  .unwrap_or(false)
}

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
  driver: &'static PeerDriver,
) -> DiscoveryOutcome {
  let Some(request) = state::with_state(|state| cursor.request(state)).flatten() else {
    return DiscoveryOutcome::Refused;
  };
  let span_ns = consensus_budget(slowest_path_tail_ns()).max_deadline_ns();
  let exchanged = {
    let mut exchange = std::pin::pin!(session.request(crate::discovery::STREAM, &request));
    let mut timer = std::pin::pin!(futures::sleep(span_ns));
    std::future::poll_fn(|cx| {
      if !link_valid(anchor, expected) {
        return std::task::Poll::Ready(Err(DiscoveryFault::Invalidated));
      }
      if let std::task::Poll::Ready(result) = std::future::Future::poll(exchange.as_mut(), cx) {
        return std::task::Poll::Ready(result.map_err(|_| DiscoveryFault::Transport));
      }
      if std::future::Future::poll(timer.as_mut(), cx).is_ready() {
        return std::task::Poll::Ready(Err(DiscoveryFault::Deadline));
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
      // The request future was dropped mid-exchange: abandoned, so its frames do not ride a later flush.
      session.abandon_exchange();
      count_refusal(fault.counter());
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
        if let LateReplies::Content { .. } = late {
          fold_late_content(late, &replies);
        } else {
          fold_late_replies(late, &replies);
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
  let _ = run_on(origin, shard, move |s| {
    for holder in &acked {
      s.put_latency.record(latency_ns);
      if let Some(job) = s.seals.get_mut(&object)
        && job.snapshot == snapshot
        && job.content.candidates.contains(holder)
      {
        fold_content_ack(job, *holder);
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
          if !crate::consensus::adopt_fetch(s, false, *peer, bytes) {
            *s.refusals.entry("consensus_join_refused").or_insert(0) += 1;
          }
        }
        LateReplies::RootFetch => {
          if !crate::consensus::adopt_fetch(s, true, *peer, bytes) {
            *s.refusals.entry("consensus_join_refused").or_insert(0) += 1;
          }
        }
        // Folded on the seal's owner shard by [`fold_late_content`], outside this borrow.
        LateReplies::Content { .. } => {}
      }
    }
  });
}

/// A late Raft reply worth folding: a vote or an append reply, each of which folds with no follow-on
/// message and is exactly the acknowledgement a slow voter would otherwise cost. A late **pre-vote** reply
/// is dropped: completing a pre-election here would owe follow-on vote requests a settle cannot ship, and
/// the next campaign simply re-runs its pre-vote.
fn late_raft_reply(
  state: &ShardState,
  root: bool,
  bytes: &[u8],
  peer: HostId,
) -> Option<RaftMessage> {
  match crate::consensus::decode_message(state, root, peer, bytes) {
    Ok(RaftMessage::PreVoteReply(_)) | Err(_) => None,
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
const FORWARD_STREAM: u64 = 11;

/// Answers one configuration-council Raft message a peer shipped on [`CONFIG_STREAM`] (§4.8, D-14): the
/// council serves a pre-vote, a vote request, or an append — applying whatever an append newly commits to
/// the regional configuration and refreshing this node's leader contact — and returns the reply to ship
/// back. A reply-typed or malformed message is answered with nothing (the sender counts no reply). Runs
/// synchronously inside `serve_once`, no await held across it.
fn serve_council(state: &mut ShardState, peer: HostId, request: &[u8]) -> Vec<u8> {
  if !same_region(state, peer) {
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
  let sessions = take_sessions(|host| others.contains(&host));
  if sessions.is_empty() {
    return;
  }
  // The append owed each borrowed voter, built under a brief borrow (the endpoints stay out here).
  let appends: std::collections::BTreeMap<HostId, Vec<u8>> = state::with_state(|s| {
    sessions
      .iter()
      .filter_map(|(host, _)| {
        s.council.replication_for(*host).and_then(|append| {
          crate::consensus::encode_message(s, false, &RaftMessage::AppendEntries(append))
            .map(|bytes| (*host, bytes))
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
  let (replied, stragglers) = broadcast(requests, CONFIG_STREAM, budget).await;
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
  let sessions = take_sessions(|host| others.contains(&host));
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
  let (replied, stragglers) = broadcast(requests, CONFIG_STREAM, budget).await;
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
  let vote_bytes = state::with_state(|s| crate::consensus::encode_message(s, false, &vote))
    .flatten()
    .unwrap_or_default();
  let requests: Vec<(HostId, Vec<u8>, Endpoint)> = sessions
    .into_iter()
    .map(|(host, endpoint)| (host, vote_bytes.clone(), endpoint))
    .collect();
  let sent: Vec<HostId> = requests.iter().map(|(host, _, _)| *host).collect();
  let (replied, stragglers) = broadcast(requests, CONFIG_STREAM, budget).await;
  let mut recovered = Vec::with_capacity(replied.len());
  let mut vote_replies = Vec::with_capacity(replied.len());
  for (host, reply, endpoint) in replied {
    vote_replies.push((host, reply));
    recovered.push((host, endpoint));
  }
  state::with_state(|s| {
    sample_voter_paths(s, &vote_replies);
    for (peer, reply) in vote_replies {
      if let Ok(message) = crate::consensus::decode_message(s, false, peer, &reply.bytes) {
        s.council.fold_reply(message);
      }
    }
  });
  // A late vote still counts when it arrives: the coordinator folds it as it settles the round.
  in_flight.push(Dispatch::new(
    sent,
    &recovered,
    stragglers,
    LateReplies::Council,
  ));
  return_sessions(recovered);
}

/// Whether this node's own SWIM membership view differs from the members of the configuration it has
/// installed — the signal a learner uses to fetch the council's committed configuration reactively (§4.8 the
/// piggyback rule). It compares this node's **alive** set against the configuration's **members**, which is
/// exactly the predicate the council leader reconciles from ([`RegionalCouncil::reconcile_alive`] admits an
/// alive non-member and takes over a member SWIM confirmed dead), so a divergence means the council has — or, if
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
  timer: &mut ElectionTimer,
  in_flight: &mut Vec<Dispatch>,
) {
  let Some((is_voter, is_leader, contact, voters)) = state::with_state(|s| {
    if s.recovery.council.is_some() {
      return (false, false, s.council.leader_contact(), Vec::new());
    }
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
    let wanted = state::with_state(|s| {
      s.recovery.council.is_some()
        || !s.council.initialized()
        || s.config_refresh_wanted
        || membership_diverges_from_config(s)
    })
    .unwrap_or(false);
    if wanted {
      drive_learner_fetch(&voters, budget, in_flight).await;
      let _ = state::with_state(|s| s.config_refresh_wanted = false);
    }
    return;
  }
  let others: Vec<HostId> = voters.into_iter().filter(|voter| *voter != local).collect();
  // This period's election timing, derived from the measured paths to the other voters (§4.8 "Derived
  // constants"): ten times the slowest voter's round-trip tail, floored at ten periods — the floor on any
  // loopback, and what `Daemon::council_timing` reports.
  let timing = derive_group_timing(&others, |s, timing| s.council_timing = timing);

  if is_leader {
    // As the region's configuration master, track its membership from this node's own SWIM view: propose
    // any admit or retire, which the replication below commits over the transport and applies on every
    // voter. Only the leader proposes (`reconcile_alive`); it probes every member, so a follower's own
    // detection need not. Each alive member is proposed with the failure domain its node declares, so an
    // admission — a restarted node's new id among them (task #22) — carries the domain into the configuration.
    state::with_state(|s| {
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
      let dead = stable_dead_council_members(s);
      s.council.reconcile_alive(&alive, &dead);
      // The voter set follows the committed membership (Raft §6, one joint change at a time): a voter taken
      // over leaves the consensus set — it stops counting toward every majority — and a member this node
      // holds alive is promoted to its seat, so the council keeps tolerating `f` failures; a sitting voter
      // keeps its seat while it is a member, so an admission never displaces a live one
      // (docs/bugs/2026-09-22-council-seats-follow-id-order-not-liveness.md).
      let voting: Vec<HostId> = alive.iter().map(|(host, _)| *host).collect();
      s.council.reconcile_voters(&voting)
    });
    drive_council_replication(&others, budget, in_flight).await;
    // CheckQuorum (Raft §6.2) on the election-timeout cadence: every derived base of leader periods the
    // council judges whether a majority was heard from (the append replies folded above, timely or late)
    // and steps this node down if not — so a leader cut off from its followers yields rather than sitting
    // on a term it can no longer hold.
    if timer.leader_period(&timing) {
      state::with_state(|s| s.council.check_quorum());
    }
    return;
  }
  if others.is_empty() {
    // The sole voter (a one-node council, the fleet degenerate): self-elect, then it leads next period.
    let _ = state::with_state(|s| s.council.election_timeout());
    timer.reset();
    return;
  }
  // A follower: the timer resets while the leader keeps making contact; otherwise it ages toward its
  // jittered timeout under this period's derived timing and campaigns there.
  if timer.follower_period(contact, &timing, local) {
    drive_council_election(&others, budget, in_flight).await;
    // Re-baseline the contact counter so a fresh campaign is not immediately retriggered: a won election
    // makes this node leader next period; a lost one waits out the timer again.
    if let Some(contact) = state::with_state(|s| s.council.leader_contact()) {
      timer.rebaseline(contact);
    }
  }
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
/// follower ages toward an election. A non-voter fetches the root's committed configuration;
/// a retiring leader keeps replicating until its removal commits. The root election timer is
/// independent of the council's timer.
async fn drive_root_group(
  local: HostId,
  budget: CommitBudget,
  timer: &mut ElectionTimer,
  in_flight: &mut Vec<Dispatch>,
) {
  let Some((is_voter, is_leader, contact, voters)) = state::with_state(|s| {
    if s.recovery.root.is_some() {
      return (false, false, s.root.leader_contact(), Vec::new());
    }
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
  if !is_voter && !is_leader {
    // A root learner (a region member that is not its region's representative): it does not drive the Raft.
    // It fetches the committed root configuration from a root voter and adopts the newest (§4.8, D-14) —
    // reactively, only when its own alive view of the regions diverges from the root configuration it holds
    // ([`root_diverges`]), so a converged learner sends nothing. The cross-region parallel of the config
    // learner's reactive fetch.
    let wanted =
      state::with_state(|s| s.recovery.root.is_some() || !s.root.initialized() || root_diverges(s))
        .unwrap_or(false);
    if wanted {
      drive_root_learner_fetch(&voters, budget, in_flight).await;
    }
    return;
  }
  let others: Vec<HostId> = voters.into_iter().filter(|voter| *voter != local).collect();
  // This period's election timing over the paths to the other root voters, as the council's.
  let timing = derive_group_timing(&others, |s, timing| s.root_timing = timing);

  if is_leader {
    // As the root master, reconcile the region membership from this node's own alive view: propose admitting
    // any newly-alive region and retiring any **mirror-less** region no host is alive in (a lost region with a
    // mirror is left for a deliberate operator promotion — §4.8, split-brain safety), committed over the
    // transport by the replication below and applied on every root voter.
    state::with_state(|s| {
      let alive = alive_regions(s);
      s.root.reconcile_regions(&alive, &s.region_mirrors);
      // The root voter set follows the committed regions and their live representatives (Raft §6, one joint
      // change at a time): a dead representative leaves the root consensus set, and a region whose
      // representative died is carried by its next live host.
      let representatives = alive_representatives(s);
      s.root.reconcile_voters(&representatives)
    });
    drive_root_replication(&others, budget, in_flight).await;
    // CheckQuorum (Raft §6.2) on the election-timeout cadence, as the council's above: every derived base
    // of leader periods the root group judges whether a majority of its voters was heard from and steps
    // this node down if not.
    if timer.leader_period(&timing) {
      state::with_state(|s| s.root.check_quorum());
    }
    return;
  }
  if others.is_empty() {
    // The sole root voter (a single-region fleet's degenerate): self-elect, then it leads next period.
    let _ = state::with_state(|s| s.root.election_timeout());
    timer.reset();
    return;
  }
  // A follower: the timer resets while the leader keeps making contact; otherwise it ages toward its
  // jittered timeout under this period's derived timing and campaigns there.
  if timer.follower_period(contact, &timing, local) {
    drive_root_election(&others, budget, in_flight).await;
    if let Some(contact) = state::with_state(|s| s.root.leader_contact()) {
      timer.rebaseline(contact);
    }
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
  let sessions = take_sessions(|host| others.contains(&host));
  if sessions.is_empty() {
    return;
  }
  let appends: std::collections::BTreeMap<HostId, Vec<u8>> = state::with_state(|s| {
    sessions
      .iter()
      .filter_map(|(host, _)| {
        s.root.replication_for(*host).and_then(|append| {
          crate::consensus::encode_message(s, true, &RaftMessage::AppendEntries(append))
            .map(|bytes| (*host, bytes))
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
  let (replied, stragglers) = broadcast(requests, ROOT_STREAM, budget).await;
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
  let sessions = take_sessions(|host| others.contains(&host));
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
  let (replied, stragglers) = broadcast(requests, ROOT_STREAM, budget).await;
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
  let vote_bytes = state::with_state(|s| crate::consensus::encode_message(s, true, &vote))
    .flatten()
    .unwrap_or_default();
  let requests: Vec<(HostId, Vec<u8>, Endpoint)> = sessions
    .into_iter()
    .map(|(host, endpoint)| (host, vote_bytes.clone(), endpoint))
    .collect();
  let sent: Vec<HostId> = requests.iter().map(|(host, _, _)| *host).collect();
  let (replied, stragglers) = broadcast(requests, ROOT_STREAM, budget).await;
  let mut recovered = Vec::with_capacity(replied.len());
  let mut vote_replies = Vec::with_capacity(replied.len());
  for (host, reply, endpoint) in replied {
    vote_replies.push((host, reply));
    recovered.push((host, endpoint));
  }
  state::with_state(|s| {
    sample_voter_paths(s, &vote_replies);
    for (peer, reply) in vote_replies {
      if let Ok(message) = crate::consensus::decode_message(s, true, peer, &reply.bytes) {
        s.root.fold_reply(message);
      }
    }
  });
  // A late vote still counts when it arrives: the coordinator folds it as it settles the round.
  in_flight.push(Dispatch::new(
    sent,
    &recovered,
    stragglers,
    LateReplies::Root,
  ));
  return_sessions(recovered);
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
  let sessions = take_sessions(|host| voters.is_empty() || voters.contains(&host));
  if sessions.is_empty() {
    return;
  }
  let request = state::with_state(|s| {
    slates_wire::Wire::to_bytes(&crate::consensus::Fetch {
      group: if s.recovery.root.is_some() {
        None
      } else {
        s.root_group
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
  let (replied, stragglers) = broadcast(requests, ROOT_FETCH_STREAM, budget).await;
  let mut recovered = Vec::with_capacity(replied.len());
  let mut fetched = Vec::new();
  for (host, reply, endpoint) in replied {
    if !reply.bytes.is_empty() {
      fetched.push((host, reply.bytes));
    }
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
      if !crate::consensus::adopt_fetch(s, true, *peer, bytes) {
        *s.refusals.entry("consensus_join_refused").or_insert(0) += 1;
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
async fn drive_learner_fetch(
  voters: &[HostId],
  budget: CommitBudget,
  in_flight: &mut Vec<Dispatch>,
) {
  let regional_peers: Vec<HostId> = state::with_state(|s| {
    s.record_sessions
      .keys()
      .copied()
      .filter(|host| same_region(s, *host))
      .collect()
  })
  .unwrap_or_default();
  let sessions = take_sessions(|host| {
    regional_peers.contains(&host) && (voters.is_empty() || voters.contains(&host))
  });
  if sessions.is_empty() {
    return;
  }
  // The fetch carries this learner's current configuration version, so a voter returns the configuration
  // only when it has a newer one (a caught-up learner's fetch is then an empty reply, not a full transfer).
  let request = state::with_state(|s| {
    slates_wire::Wire::to_bytes(&crate::consensus::Fetch {
      group: if s.recovery.council.is_some() {
        None
      } else {
        s.council_group
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
  let (replied, stragglers) = broadcast(requests, CONFIG_FETCH_STREAM, budget).await;
  let mut recovered = Vec::with_capacity(replied.len());
  let mut fetched = Vec::new();
  for (host, reply, endpoint) in replied {
    if !reply.bytes.is_empty() {
      fetched.push((host, reply.bytes));
    }
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
      if !crate::consensus::adopt_fetch(s, false, *peer, bytes) {
        *s.refusals.entry("consensus_join_refused").or_insert(0) += 1;
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
    s.consensus_ready = !s.recovery.joining()
      && s.council.initialized()
      && s.root.initialized()
      && s.council.configuration().members.contains(&local);

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
    // The owner each held object had *before* this install, and the version that is retiring some of them,
    // so a takeover this install produces records who departed and when — the holder-side promotion gate
    // (§4.8 "Leases and reads", AUD-08) reads it to keep a successor from promoting an object before the
    // departed owner's lease can have expired.
    let since_version = configuration.version;
    let pre_owners: std::collections::BTreeMap<ObjectId, HostId> = s
      .holder_records
      .keys()
      .filter_map(|object| s.fleet.object_owner(*object).map(|owner| (*object, owner)))
      .collect();
    // This node installed a newer configuration: any supersession it learned is resolved at or below it,
    // so its own lease can hold again once holders confirm it under the new version; the bounded startup
    // allowance restarts from now (a takeover under this version cannot yet have committed).
    s.lease
      .installed(configuration.version, slates_machine::clock::monotonic_ns());
    for reassignment in s.fleet.install_configuration(configuration, &members) {
      s.pending_takeovers.insert(reassignment.object);
      if let Some(&owner) = pre_owners.get(&reassignment.object) {
        s.departed_owners.insert(
          reassignment.object,
          crate::lease::DepartedOwner {
            owner,
            since_version,
          },
        );
      }
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
/// **Re-fanned every period, not only on change.** `run_on` is refused when a shard's control channel is
/// momentarily full ([`RtError::ControlFull`]), and a fan dropped on the one period a configuration changed
/// would otherwise leave that shard stale until the next change. Re-fanning heals it next period. This is the
/// authoritative cross-shard state: an owner shard routes and admits from the **committed configuration**
/// fanned here (`fleet.configuration()`, `object_owner`), never from the raw SWIM membership, which
/// [`fold_peer_state`] publishes to the other shards **best-effort** for D-7 uniformity but which no
/// off-control-shard path reads, so a membership fold dropped on a full channel is harmless where a dropped
/// configuration fan would not be. The receiving side is
/// version-gated (`install_configuration` installs only a newer version; `RootGroup::adopt` ignores an
/// equal-or-older one), so a re-fan of an unchanged configuration is a no-op there. Both configurations ride
/// **one** cross-shard message per shard, and both are bounded (a bounded neighbourhood; regions, moved homes
/// and promotions), so the per-period cost is bounded at every scale. A non-control shard tracks no held
/// object, so its `install_configuration` returns no reassignment to drive — takeover stays on this shard.
fn fan_configs_to_shards(origin: u16, shards: &[u16]) {
  let Some((placement, members, root, ready, lease)) = state::with_state(|s| {
    // Prune the owner-lease ledgers to the current members each period (a restarted peer's retired id
    // leaves; §4.8 "Leases and reads", AUD-08), then fan this node's lease evidence to every owner shard —
    // the verb gate (`verbs::dispatch`) and the mount (`crate::nfs`) run there and must read the same
    // confirmation the control shard's probe tasks collected. `answers_given` and `departed_owners` stay on
    // the control shard (only the takeover drive reads them), so they are not fanned.
    s.lease.retain_members(&members_snapshot(s));
    s.answers_given.retain_members(&members_snapshot(s));
    (
      s.fleet.configuration().clone(),
      s.fleet.members().to_vec(),
      s.root.configuration().clone(),
      s.consensus_ready,
      s.lease.clone(),
    )
  }) else {
    return;
  };
  for shard in shards.iter().copied().filter(|shard| *shard != origin) {
    let placement = placement.clone();
    let members = members.clone();
    let root = root.clone();
    let lease = lease.clone();
    let _ = run_on(origin, shard, move |s| {
      if !s.consensus_ready || placement.version > s.fleet.configuration().version {
        // This shard serves its own writes, so it measures the fanned configuration against the durability
        // policy for itself (the shortfall its writes are refused with); the control shard already counted
        // this change's breach, so the measurement here moves no signal.
        s.durability_shortfall = s
          .config
          .fleet
          .as_ref()
          .and_then(|membership| membership.durability)
          .and_then(|bound| bound.shortfall(&placement));
        let _ = s.fleet.install_configuration(placement, &members);
      }
      s.root.adopt(root);
      s.consensus_ready = ready;
      // The owner lease is authoritative on the control shard; every other shard reads the fanned copy.
      // The confirmations carry absolute send times on the shared host clock, so a fan a full channel
      // dropped only shortens the lease on that shard until the next period re-fans it (never lengthens it).
      s.lease = lease;
    });
  }
}

/// The current fleet members as a plain vector — the argument the lease pruners take. A brief read used
/// twice in one borrow, factored so neither call clones the membership through a longer path.
fn members_snapshot(state: &ShardState) -> Vec<HostId> {
  state.fleet.members().to_vec()
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
    in_flight.retain_mut(|dispatch| !dispatch.settle());
    // This period's round budget, derived from the slowest measured peer path — every dispatch below (a
    // council or root round, a record commit, a takeover promotion, a content put, a learner fetch) gathers
    // replies from peers, so the round must outlast the farthest one's round trip ([`consensus_budget`]).
    // On one host every path is inside a heartbeat and this is the budget it always was (R8).
    let budget = consensus_budget(slowest_path_tail_ns());
    // Drive the configuration authority first — an election or a replication heartbeat over the transport —
    // then the records under the configuration it maintains.
    drive_config_council(local, budget, &mut council_timer, &mut in_flight).await;
    // Drive the root group across regions on the same coordinator (§4.8, D-14): a brief borrow of the
    // root-voter sessions, sequential with the council's and the record ships, so it never contends.
    drive_root_group(local, budget, &mut root_timer, &mut in_flight).await;
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
    materialize_adopted_objects(origin, budget).await;
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
      let slice_bytes = s.config.archive_slice_bytes();
      let created_unix = u64::try_from(s.clock.wall_ns()).unwrap_or(0) / NANOS_PER_SECOND;
      advance_seals(s, local, slice_bytes, created_unix, budget)
    },
    HEARTBEAT_NS,
  )
  .await
  .unwrap_or_default();
  for work in seals {
    if let Some(dispatch) = put_seal_content(origin, local, work).await {
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
  // The greens' merge records (§4.16 "Commit"): each green's lowest pending version — its inputs put
  // while unplaced, its record shipped in order once they are.
  crate::merge_service::run_merge_period(origin, shard, local, budget, owner_acceptor, in_flight)
    .await;
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
pub(crate) fn take_sessions(wanted: impl Fn(HostId) -> bool) -> Vec<(HostId, Endpoint)> {
  state::with_state(|s| {
    let mut taken = Vec::new();
    for (host, link) in s.record_sessions.iter_mut() {
      if wanted(*host)
        && let Some(mut endpoint) = link.endpoint.take()
      {
        // The borrow is tagged with the session's own identity, so only this session returns to the slot.
        link.borrowed = endpoint.connection_id().ok();
        taken.push((*host, endpoint));
      }
    }
    taken
  })
  .unwrap_or_default()
}

/// Returns borrowed sessions to the shard state after a dispatch. A slot is refilled only by the session
/// that left it — the entry still there, its session out, and the returning session's connection id the
/// one the borrow was tagged with. Anything else is a late return — a peer retired meanwhile (its entry
/// removed by its link task), a slot the link task re-established since (holding a newer session, or
/// lending it), or a session other than the one borrowed — dropped and counted (`fleet.link.stale_return`),
/// never installed over a newer session.
pub(crate) fn return_sessions(sessions: Vec<(HostId, Endpoint)>) {
  state::with_state(|s| {
    for (host, mut endpoint) in sessions {
      let returned = endpoint.connection_id().ok();
      match s.record_sessions.get_mut(&host) {
        Some(link) if link.endpoint.is_none() && link.borrowed == returned => {
          link.endpoint = Some(endpoint);
          link.borrowed = None;
        }
        _ => *s.refusals.entry(LINK_STALE_RETURN).or_insert(0) += 1,
      }
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
  let began = futures::now_ns();
  let mut missed = false;
  loop {
    let waited = futures::now_ns().saturating_sub(began);
    if let Some((_, endpoint)) = take_sessions(|host| host == peer).pop() {
      let remaining = deadline_ns.saturating_sub(waited);
      let (reply, endpoint) = request_within(endpoint, FORWARD_STREAM, &request, remaining).await;
      return_sessions(vec![(peer, endpoint)]);
      return Some(reply.bytes);
    }
    let first_miss = !missed;
    missed = true;
    let expired = waited >= deadline_ns;
    state::with_state(|s| {
      if first_miss {
        let missing = if s.record_sessions.contains_key(&peer) {
          "fleet.forward.session_out"
        } else {
          "fleet.forward.no_session"
        };
        *s.refusals.entry(missing).or_insert(0) += 1;
      }
      if expired {
        *s.refusals
          .entry("fleet.forward.session_never_returned")
          .or_insert(0) += 1;
      }
    });
    if expired {
      return None;
    }
    futures::sleep(HEARTBEAT_NS / POLL_PER_PERIOD).await;
  }
}

/// The pending takeovers this node should drive: the objects it owes a takeover for
/// ([`ShardState::pending_takeovers`]) whose surviving candidate set — computed the same way every node
/// computes placement ([`candidates_for`] over the current neighbourhood) — contains this node, the new owner
/// `sync_peer` reassigned them to, **and** whose departed owner's lease can no longer hold (§4.8 "Leases and
/// reads", AUD-08): the holder-side promotion gate ([`AnswersGiven::promotion_open`]) — this node has not
/// answered the departed owner's probe for the membership horizon (so any lease it fed has expired) or the
/// owner has announced it saw the retiring configuration (so it refuses its own clients now). Without a
/// recorded departed owner the gate is open (a re-driven takeover whose record has been cleared; the council's
/// own death-confirmation window already exceeds the horizon). The coordinator drives each object over **all**
/// its surviving candidate holders, so the `f > 1` promotion quorum (this node plus `f` holders) is reached
/// over the several sessions it owns. Read under `with_state`.
fn takeovers(state: &ShardState, local: HostId) -> Vec<ObjectId> {
  let config = state.fleet.configuration();
  let now = slates_machine::clock::monotonic_ns();
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
    .filter(|object| {
      state.departed_owners.get(object).is_none_or(|departed| {
        state
          .answers_given
          .promotion_open(departed.owner, departed.since_version, now)
      })
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
    let recovery = s.fleet.recovery_cohort(object)?.clone();
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
    Some((acceptor, recovery, candidates, quorum, generation, epoch))
  })
  .flatten();
  let Some((mut acceptor, recovery, candidates, quorum, generation, epoch)) = prepared else {
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
  let holders = take_sessions(|host| recovery.candidates.contains(&host));
  let taken: Vec<HostId> = holders.iter().map(|(host, _)| *host).collect();
  let promoted = promote_record(
    local,
    &mut acceptor,
    &recovery.candidates,
    &prepare,
    recovery.quorum,
    holders,
    budget,
  )
  .await;
  dispatches.push(Dispatch::new(
    taken,
    &promoted.reusable,
    promoted.stragglers,
    LateReplies::Discard,
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
      LateReplies::Discard,
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
      s.fleet.record_adopted_placement(
        object,
        local,
        slates_cluster::routing::RecoveryCohort {
          generation,
          candidates,
          quorum,
        },
      );
      s.placed_heads.insert(
        object,
        PlacedHead {
          sequence,
          epoch: prepare.epoch,
          placement,
        },
      );
      s.pending_takeovers.remove(&object);
      // The object is owned here now; its departed-owner promotion gate has served its purpose.
      s.departed_owners.remove(&object);
      if let Some(head) = HeadValue::from_record_bytes(&value) {
        s.pending_materializations.insert(object, head);
      } else if let Some(merge) = crate::merge_service::MergeRecordValue::from_record_bytes(&value)
      {
        // A green: its newest merge record was adopted; the owned green is rebuilt from this node's
        // accepted chain and held inputs on the shard its id routes to (AUD-14).
        s.pending_green_materializations.insert(object, merge);
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

#[cfg(test)]
mod tests {
  use super::*;
  use slates_cluster::DispatchWait;

  /// AC-8.1 / T-8.12: an authenticated neighbor reports the death of a known member this node
  /// does not probe directly. The council's failure view must receive that death; old alive gossip
  /// cannot resurrect it. Gossip about unknown identities must not enroll them.
  #[test]
  fn a_neighbors_gossip_retires_a_known_third_member_without_enrolling_strangers() {
    let (lost_is_alive, stranger_is_known) = crate::daemon::audit_on_shard(|state| {
      let local = state.fleet.host();
      let neighbor = HostId(local.0.wrapping_add(1));
      let lost = HostId(local.0.wrapping_add(2));
      let stranger = HostId(local.0.wrapping_add(3));
      let alive = MemberState {
        liveness: Liveness::Alive,
        incarnation: 0,
      };
      state.fleet.observe(neighbor, alive);
      state.fleet.observe(lost, alive);
      state.authenticated_members.insert(lost);
      let mut detector = Detector::new(local, detector_timing(3));
      detector.join(neighbor);
      let message = SwimMessage::Ping {
        from: neighbor,
        nonce: 1,
        boot_nonce: 0,
        configuration_version: 0,
        gossip: vec![
          (
            lost,
            MemberState {
              liveness: Liveness::Dead,
              incarnation: 1,
            },
          ),
          (stranger, alive),
        ],
      };
      let message = SwimMessage::decode(&message.encode()).unwrap();
      receive_probe_gossip(state, &mut detector, neighbor, message.gossip());
      receive_probe_gossip(state, &mut detector, neighbor, &[(lost, alive)]);
      let outgoing = outgoing_probe_gossip(state, neighbor, Some(alive), 4);
      let relayed = SwimMessage::Ping {
        from: local,
        nonce: 2,
        boot_nonce: 0,
        configuration_version: 0,
        gossip: outgoing,
      };
      let relayed = SwimMessage::decode(&relayed.encode()).unwrap();
      assert!(
        relayed
          .gossip()
          .iter()
          .any(|(subject, report)| { *subject == lost && report.liveness == Liveness::Dead }),
        "a third member's death must travel to another live peer"
      );
      // Foreign alive reports must not add a second target to this session's detector.
      let refuted = MemberState {
        liveness: Liveness::Alive,
        incarnation: 2,
      };
      receive_probe_gossip(state, &mut detector, neighbor, &[(lost, refuted)]);
      assert!(state.fleet.membership().alive().contains(&lost));
      for _ in 0..4 {
        assert_eq!(detector.tick().map(|ping| ping.to), Some(neighbor));
        detector.on_ack(neighbor);
      }
      receive_probe_gossip(
        state,
        &mut detector,
        neighbor,
        &[(
          lost,
          MemberState {
            liveness: Liveness::Dead,
            incarnation: 2,
          },
        )],
      );
      (
        state.fleet.membership().alive().contains(&lost),
        state.fleet.membership().state(stranger).is_some(),
      )
    });
    assert!(
      !lost_is_alive,
      "a third member's death must reach the council's failure view"
    );
    assert!(!stranger_is_known, "gossip does not authorize enrollment");
  }

  /// AC-8.1 / T-8.12: shared self-refutation is visible on a different peer session, while
  /// an authenticated restart prevents a neighbor's high-incarnation report reviving the old id.
  #[test]
  fn probe_gossip_shares_refutation_and_cannot_revive_a_replaced_identity() {
    crate::daemon::audit_on_shard(|state| {
      let local = state.fleet.host();
      let anchor = HostId(local.0.wrapping_add(1));
      let old = crate::deploy::member_id(anchor, 1);
      let current = crate::deploy::member_id(anchor, 2);
      learn_member(state, anchor, 1, old);
      learn_member(state, anchor, 2, current);
      let mut detector = Detector::new(local, detector_timing(3));
      detector.join(current);
      receive_probe_gossip(
        state,
        &mut detector,
        current,
        &[
          (
            old,
            MemberState {
              liveness: Liveness::Alive,
              incarnation: 10,
            },
          ),
          (
            local,
            MemberState {
              liveness: Liveness::Dead,
              incarnation: 10,
            },
          ),
        ],
      );
      let outgoing = outgoing_probe_gossip(state, current, None, 4);
      assert!(!state.fleet.membership().alive().contains(&old));
      assert!(outgoing.contains(&(
        local,
        MemberState {
          liveness: Liveness::Alive,
          incarnation: 11
        }
      )));
      let mut other_session = Detector::new(local, detector_timing(3));
      receive_probe_gossip(
        state,
        &mut other_session,
        current,
        &[(
          local,
          MemberState {
            liveness: Liveness::Dead,
            incarnation: 9,
          },
        )],
      );
      let outgoing = outgoing_probe_gossip(state, current, None, 4);
      assert!(outgoing.contains(&(
        local,
        MemberState {
          liveness: Liveness::Alive,
          incarnation: 11
        }
      )));
    });
  }

  /// A millisecond in nanoseconds, so the samples read as round times.
  const MS: u64 = 1_000_000;

  /// The probe deadline follows the design's law, by use: before any sample it is the conservative initial
  /// probe timeout; a quiet-loopback round trip floors it at the beat; each consecutive miss doubles it; the
  /// liveness budget caps it however many misses; an acknowledgement resets the backoff; and a slow peer's
  /// measured round trip raises it above the floor, still within the cap.
  #[test]
  fn the_probe_deadline_is_derived_from_the_round_trip_backed_off_and_capped() {
    let mut timing = ProbeTiming::new();
    assert_eq!(
      timing.deadline_ns(None),
      RttEstimator::new().initial_pto(),
      "before any sample: the initial probe timeout"
    );
    // The measured quiet-loopback probe round trip (p99 17 ms): its probe timeout sits below the beat.
    let mut path = PathRtt::new();
    path.on_sample(17 * MS);
    timing.acknowledged();
    assert_eq!(
      timing.deadline_ns(path.tail_ns()),
      HEARTBEAT_NS,
      "a quiet round trip floors the deadline at the beat"
    );
    timing.missed();
    assert_eq!(
      timing.deadline_ns(path.tail_ns()),
      2 * HEARTBEAT_NS,
      "one miss doubles it"
    );
    timing.missed();
    assert_eq!(
      timing.deadline_ns(path.tail_ns()),
      4 * HEARTBEAT_NS,
      "two misses quadruple it"
    );
    for _ in 0..8 {
      timing.missed();
    }
    assert_eq!(
      timing.deadline_ns(path.tail_ns()),
      LIVENESS_BUDGET_NS,
      "however many misses, the liveness budget caps it"
    );
    timing.acknowledged();
    assert_eq!(
      timing.deadline_ns(path.tail_ns()),
      HEARTBEAT_NS,
      "an acknowledgement resets the backoff"
    );

    // A slow peer — its acknowledgements take 300 ms — is waited for above the floor, within the cap.
    let mut slow_path = PathRtt::new();
    slow_path.on_sample(300 * MS);
    let slow = ProbeTiming::new();
    let deadline = slow.deadline_ns(slow_path.tail_ns());
    assert!(
      deadline > HEARTBEAT_NS && deadline <= LIVENESS_BUDGET_NS,
      "a slow peer's deadline follows its round trip: {deadline} ns"
    );
    assert_eq!(
      Some(deadline),
      slow_path.tail_ns(),
      "above the floor the deadline is the path's measured tail itself"
    );
  }

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

  /// The probe cadence follows the Lifeguard local health: exactly one beat at full health, `health + 1`
  /// beats as the node's own probes fail, never past the cap — a degraded prober probes less aggressively.
  #[test]
  fn the_probe_cadence_is_one_beat_dilated_by_the_local_health() {
    assert_eq!(
      probe_period_ns(1),
      HEARTBEAT_NS,
      "full health: exactly the beat"
    );
    assert_eq!(
      probe_period_ns(2),
      2 * HEARTBEAT_NS,
      "one step of ill health: two beats"
    );
    let capped = LOCAL_HEALTH_CAP + 1;
    assert_eq!(
      probe_period_ns(capped),
      u64::from(capped) * HEARTBEAT_NS,
      "at the cap: three beats, never more"
    );
  }

  /// The hedge delay follows the design's law, by use: one period before any reading (the first seal of a
  /// boot, the laptop); then the measured p95 of the content class's put latency — the reading nineteen in
  /// twenty acknowledgements arrive within — and the window is bounded: the oldest reading leaves as the
  /// newest arrives, so a load regime is forgotten within a window of seals.
  #[test]
  fn the_hedge_delay_is_the_measured_p95_over_a_bounded_window() {
    let mut latency = PutLatency::default();
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
    let mut latency = PutLatency::default();
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
}
