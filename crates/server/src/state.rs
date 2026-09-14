//! The shard's state: its volumes, its partition of the database, its clients, and the
//! reserve; one cell per shard thread, borrowed briefly by the shard's tasks (§4.3: one
//! owning shard per volume; §4.8: one writer per partition).

use std::cell::RefCell;
use std::collections::BTreeMap;

use slates_anchor::AnchorSegment;
use slates_base::OsHost;
use slates_db::Db;
use slates_db::catalog::{Principal, VolumeId};
use slates_db::register::{Acceptor, ObjectId};
use slates_ipc::DaemonEnd;
use slates_ipc::protocol::ReplyBody;
use slates_mem::SharedObject;
use slates_mem::Slab;
use slates_merge::engine::Green;
use slates_merge::increment::VolumeOp;
use slates_vfs::volume::{Store, Volume};

use crate::config::DaemonConfig;

/// One client the shard serves.
pub struct ClientSlot {
  /// The ring ends.
  pub end: DaemonEnd,
  /// The principal established at rendezvous.
  pub principal: Principal,
  /// The client id.
  pub client_id: u32,
  /// The client's process id (the liveness probe's input where no socket closes).
  pub pid: u32,
  /// When the client last wrote a slot (any kind); a client silent past the liveness budget
  /// is asked about.
  pub last_seen_ns: u64,
  /// The control channel, where the platform has one (Linux: the socket whose close is how
  /// the daemon learns of a dead client, and whose peer end closing tells the client the
  /// daemon died; held for the client's life).
  pub control: Option<slates_ipc::rendezvous::platform::Control>,
  /// Set when the consumer this channel attested as has since been revoked by a human (§4.13): every
  /// later verb on the channel refuses `ConsumerRevoked` before any effect. A local flag — the
  /// revocation is fanned to every shard's slots when it commits — so the per-verb gate is one read,
  /// never a cross-shard call on a write path (banned item 10).
  pub revoked: bool,
}

impl std::fmt::Debug for ClientSlot {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ClientSlot")
      .field("client_id", &self.client_id)
      .finish()
  }
}

/// One volume the shard owns: the volume core's object, the host of its base when it has
/// one, and its bounded reservation.
pub struct VolumeSlot {
  /// The id.
  pub id: VolumeId,
  /// The mount name the volume was provisioned under — the friendly name it appears under in the host
  /// root, so a client mounts `/<name>` or reaches it by `cd <name>` (§4.6 "Chosen path"). Unique per
  /// host: a name is created on the one partition `verbs::owner_of_name` routes it to, which is also
  /// the partition the volume's id encodes, so a name routes to its volume with no global index (D-14).
  pub name: String,
  /// The volume.
  pub volume: Volume,
  /// The read-only host of the base directory, for an overlay.
  pub host: Option<OsHost>,
  /// The reservation, for a bounded volume.
  pub reservation: Option<slates_mem::budget::Reservation>,
  /// The inode-version reservation (§4.2 inode dimension): the volume's logical inode allowance
  /// reserved against the shard's version slab, returned on teardown so the slab is never over-offered.
  pub version_credit: Option<slates_mem::budget::VersionCredit>,
  /// The metadata reservation (§4.2 metadata dimension): the volume's records — its journal budget,
  /// its object, its snapshot slab's first segment — reserved against the shard's metadata ledger at
  /// admission, returned on teardown so the class is never over-offered.
  pub metadata_credit: Option<slates_mem::budget::MetadataCredit>,
}

impl std::fmt::Debug for VolumeSlot {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("VolumeSlot").field("id", &self.id).finish()
  }
}

/// What a fleet peer is currently known as (task #22 learn-on-contact): the daemon generation it last
/// announced and the ephemeral member id that generation derives to (`deploy::member_id(anchor,
/// generation)`). Kept per stable anchor in [`ShardState::learned_members`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LearnedMember {
  /// The daemon generation the peer announced (its anchor segment's start count).
  pub generation: u64,
  /// The member id at that generation — what membership, ownership and records name the peer by.
  pub host: slates_db::HostId,
}

/// The shard's state.
pub struct ShardState {
  /// The shard: the runtime's id, what messages are addressed to (process-local).
  pub shard: u16,
  /// The partition: this shard's index among the daemon's, what volume ids and client ids
  /// route by (persistent: a restarted daemon's partition holds the same records).
  pub partition: u16,
  /// The configuration.
  pub config: DaemonConfig,
  /// The segment (this shard's mapping).
  pub segment: AnchorSegment,
  /// The anchor-owned content object that holds this shard's recovery image (§4.8), and the byte
  /// range within it this shard owns (shards share one object, partitioned by index). `None` when
  /// the anchor provides no content object (a build or config without anchor-backed recovery); then
  /// a restart recreates content empty as before (BUG-11).
  pub content: Option<SharedObject>,
  /// The half-open byte range `[start, end)` of `content` this shard publishes into and recovers
  /// from; `0..0` when there is no content object.
  pub content_range: (usize, usize),
  /// The NFS write verifier this shard's exports answer WRITE and COMMIT with (RFC 1813
  /// `writeverf3`, §4.6): the shard's boot instant, so it is unique to this daemon instance and a
  /// client that holds unstable writes from before a restart sees it change and re-sends them.
  /// Format: the boot instant's monotonic nanoseconds, big-endian — a monotonic clock never repeats
  /// within a host's uptime, and a host reboot discards every client's unstable state with the
  /// anchor's RAM, so no older instance is ever confused with a newer one.
  pub write_verifier: [u8; size_of::<u64>()],
  /// The partition.
  pub db: Db,
  /// The owner runtime this node takes part in a region as (§4.8, boot step 6): the SWIM membership
  /// view, the configuration group, and the owner's register acceptor, composed by `slates-cluster`'s
  /// `FleetNode`. `FleetNode::solo` at `f = 0` (the laptop) — the same code path a fleet runs (R8), a
  /// membership event folding into the configuration through [`slates_cluster::FleetNode::observe`]. The
  /// verbs read `fleet.configuration()` for the candidate placement of an object and `placed_heads` for
  /// what the fleet has actually committed; the live probe/gossip loop that drives `observe` and the
  /// cross-node commit run in `crate::fleet` on the control shard.
  pub fleet: slates_cluster::fleet::FleetNode,
  /// The durability the installed configuration **cannot** hold to the operator's declared policy (§4.8
  /// "Placement" — the policy "that gates a refusal"): the measured shortfall, or `None` when the
  /// configuration is within the accepted loss or no policy is declared (the default, and every laptop —
  /// a single copy has no coincident loss, so `f = 0` is never short, R8). Measured at every configuration
  /// install — boot, a council commit, the cross-shard fan — by `DurabilityBound::shortfall`, and read per
  /// write by the verbs as a field (`verbs::dispatch` refuses a write that would commit a new head or seal
  /// `DurabilityUnmet` with these numbers), never recomputed on the write path.
  pub durability_shortfall: Option<crate::config::DurabilityShortfall>,
  /// This node's **stable cert-anchor** — `deploy::host_id_of_certificate` of its own certificate, the id that
  /// does **not** change across a restart (§4.8; task #22 two-id model). Distinct from `fleet.host()`, which
  /// is the **ephemeral** member id (per boot, so a restart is a new member). The anchor keys a client's
  /// **completion record** (the RIFL origin's high half), so a retry meets its record across a daemon restart —
  /// exactly-once survives the ephemeral id changing. Ownership, rendezvous and `ObjectId::creator` use the
  /// ephemeral `fleet.host()`; only the completion origin uses this. On a laptop it is the identity's own
  /// cert anchor, stable per machine (R8).
  pub origin_anchor: slates_db::HostId,
  /// The regional configuration council (§4.8, D-14 — the "configuration master"): the multi-voter Raft the
  /// control shard's config plane drives over the transport to agree on the region's configuration
  /// (`crate::fleet::run_config_council`). A laptop runs a solo council that self-leads (R8). Only the
  /// control shard drives and serves it; other shards hold an inert copy of the same boot state.
  pub council: slates_cluster::config_group::RegionalCouncil,
  /// The **root configuration group** across regions (§4.8, D-14 — "a root group across regions holds region
  /// membership and cross-region promotions"): the multi-voter Raft the control shard's config plane drives
  /// over the transport (among the region representatives) to agree on the `RootConfiguration` — which regions
  /// exist, moved volume homes, and region promotions. A single-region fleet (the default) runs a solo root
  /// group that self-leads (R8). Only the control shard drives and serves it; other shards hold an inert copy.
  pub root: slates_cluster::root_group::RootGroup,
  /// Each fleet member's region (§4.8, D-14), the host→region map the root group's leader reconciles the
  /// region membership from (an alive host's region is an alive region). A host absent is in the sole region
  /// `RegionId(0)`. Set at boot from the fleet membership; empty on a laptop.
  pub node_regions: std::collections::BTreeMap<slates_db::HostId, slates_db::register::RegionId>,
  /// Each region's designated mirror (§4.8 — region-loss promotion): the reconcile leaves a lost mirrored
  /// region for a deliberate operator promotion (not auto-retire), and `Daemon::promote_region` looks up the
  /// mirror to promote here. Set at boot from the fleet membership; empty on a laptop or when none declared.
  pub region_mirrors:
    std::collections::BTreeMap<slates_db::register::RegionId, slates_db::register::RegionId>,
  /// The landing runtime: grants, leases and the audit log (§4.15), mirrored to the
  /// database's durable records.
  pub landing: crate::landing::LandingState,
  /// The store.
  pub store: Store,
  /// The volumes.
  pub volumes: Slab<VolumeSlot>,
  /// Volumes by id.
  pub by_id: BTreeMap<VolumeId, slates_mem::Handle<VolumeSlot>>,
  /// The clients.
  pub clients: Slab<ClientSlot>,
  /// The next inode prefix a volume takes (unique per volume on the host: the shard in the
  /// high bits and a counter below).
  pub next_prefix: u16,
  /// The next attachment id.
  pub next_attachment: u64,
  /// The clock.
  pub clock: slates_vfs::clock::HostClock,
  /// Requests served.
  pub served: u64,
  /// Refusals by kind name.
  pub refusals: BTreeMap<&'static str, u64>,
  /// The daemon's grant-issuer secret for this start (§4.13), read from the anchor's supervision block
  /// at shard init — the key a `Grant` request's proof of authority is verified under
  /// (`landing::grant_proof`). Every shard reads the same secret, so a grant verifies on whichever shard
  /// serves the client; it is never sent on any channel.
  pub issuer_secret: [u8; slates_anchor::layout::ISSUER_SECRET_BYTES],
  /// Replies waiting for the client's ring (full, or the reply came from another shard), by
  /// client slot; `recorded` says the completion record already exists (at the owner
  /// partition of a forwarded verb), so this shard must not record it again.
  pub deferred: Vec<Deferred>,
  /// The shard's server task, woken when a client is added while it idles.
  pub server_task: Option<slates_rt::TaskId>,
  /// Listings in flight: by request word, the client slot, the shards still to answer, and
  /// the summaries so far (a scatter-gather over owners, §4.8 "Lookup").
  pub scatters: BTreeMap<u64, (u32, usize, Vec<slates_ipc::protocol::VolumeSummary>)>,
  /// `grants` reads in flight: the caller's grants live on the shards that presented their landings (a
  /// grant record is written where its landing was), so the read is a scatter-gather like a listing — by
  /// request word, the client slot, the shards still to answer, and the summaries so far.
  pub grant_scatters: BTreeMap<u64, (u32, usize, Vec<slates_ipc::protocol::GrantSummary>)>,
  /// Every shard of the daemon, for the scatter.
  pub shards: Vec<u16>,
  /// What the last start's recovery found (the status reports it).
  pub recovered: slates_db::replay::Recovered,
  /// When this shard's state was installed (the freshness of what recovery measured).
  pub booted_ns: u64,
  /// Daemon status requests in flight: by request word, the client slot, the shards still
  /// to answer, and the parts so far.
  pub status_scatters: BTreeMap<u64, (u32, usize, Vec<slates_ipc::protocol::ShardReport>)>,
  /// Acknowledgements in flight: by request word, the client slot, the shards still to
  /// answer, and the reply so far.
  pub ack_scatters: BTreeMap<u64, (u32, usize, ReplyBody)>,
  /// When this shard last did work for a client (served, forwarded, or ran a forwarded verb);
  /// the server loop polls for the idle window past it before parking (§4.7).
  pub last_work_ns: u64,
  /// Forwards refused by a full control channel, kept to retry (backpressure, never a drop);
  /// bounded by the clients' credit, refused typed beyond it.
  pub pending_forwards: std::collections::VecDeque<PendingForward>,
  /// Green volumes' merge engines (§4.16): the in-memory chain and per-path state a green owns,
  /// keyed by its id. A green is not a store-backed VFS tree; its merged content lives here.
  pub greens: BTreeMap<VolumeId, Green>,
  /// Work volumes' declared operations (§4.16): each work over a green accumulates the operations an
  /// agent declares (through `edit`) and the bytes they name, composed into an increment on submit.
  pub works: BTreeMap<VolumeId, WorkState>,
  /// This shard's bounded telemetry ring (§4.14): the chokepoint spans emitted on the shard, held
  /// shed-first (the newest kept, the oldest dropped and counted), drained by the `Telemetry` verb
  /// (`crate::telemetry`) in batches bounded to one reply. Per-shard and thread-local, so it needs no
  /// lock (R2) — the design's "per-shard rings"; the status scatter is their aggregation.
  pub telemetry: slates_wire::observe::SpanSink,
  /// This shard's opener of spans (§4.14, the three-id law): it mints the shard's trace and span ids
  /// (the node and partition folded in, so they are distinct daemon- and fleet-wide) and is the only
  /// way to open a span — a root from the request it serves, a child from the context that caused it.
  pub tracer: slates_wire::observe::Tracer,
  /// The innermost open span's context while the shard serves (§4.14): the `ring.request` span from
  /// the slot read, then the `shard.op` span while its verb runs — so a chokepoint deeper in a verb (a
  /// `log.append`, a `merge.verdict`, a `land.entry`) opens its span *within* the one that caused it,
  /// sharing its request and trace and naming it, without threading the context through every handler.
  /// The shard is single-threaded and runs one verb at a time with no awaits inside (§4.7), so this is
  /// unambiguous; `None` between requests, and for work whose cause crossed a boundary that carried
  /// none (the span then declares its cause missing).
  pub current_span: Option<slates_wire::observe::SpanContext>,
  /// The `ring.request` spans of requests this shard forwarded to another shard, or scattered, by request
  /// word (§4.14): opened at the slot read, ended when the reply comes back through `deliver` and is
  /// written, so a forwarded reply's ring span is timed from read to reply like a local one. Bounded by
  /// the forwards in flight — the clients' credit (`clients_per_shard × slots`, the same bound
  /// `pending_forwards` keeps); past it a span is shed and counted rather than held.
  pub forwarded_rings: BTreeMap<u64, slates_wire::observe::OpenSpan>,
  /// Derived: how many spans one `Telemetry` reply carries (`crate::telemetry::spans_per_reply`): what
  /// one bulk chunk holds past the report's fixed part.
  pub telemetry_quota: usize,
  /// When this shard's ring was last drained (boot at first), so a batch reports the window it covers.
  pub last_drain_ns: u64,
  /// The region placement the fleet has committed for each object this node owns (§4.8): the acknowledging
  /// set the control-shard membership loop recorded when it replicated the object's head record to its
  /// candidate holders and reached the quorum. The placement authority the verbs read (`region_placed`,
  /// `await_placed`) consults this — a head with a stored quorum placement is region-placed; without one it
  /// is the local append (`f = 0`, or not yet replicated). Empty on a laptop (no fleet loop runs).
  pub placed_heads: BTreeMap<ObjectId, crate::head::PlacedHead>,
  /// The fleet peers this node has established a live probe session with (§4.8): a peer is inserted the
  /// moment its probe session's handshake completes, so this is the set of peers the direct mesh has
  /// actually formed to — distinct from the membership's optimistically **seeded** alive set, which holds
  /// every configured peer from boot before any is contacted. The daemon reads it (`fleet_meshed`) to
  /// tell whether the fleet's direct mesh is up, which a formation observer must wait for rather than the
  /// seeded view. Empty on a laptop (no fleet loop runs).
  pub formed_probe_peers: std::collections::BTreeSet<slates_db::HostId>,
  /// This node's own daemon **generation** (§4.8 "Recovery"; task #22): the anchor segment's start count
  /// its runtime member id folds in (`member_id(origin_anchor, generation)`), announced on every SWIM ping
  /// and acknowledgement so its peers validate the id it answers under. Zero on a laptop, an anchorless
  /// daemon, or a fresh test segment (the manifest's precomputable generation-0 seed).
  pub member_generation: u64,
  /// The **current member id of every rostered fleet peer**, by the peer's stable anchor, with the daemon
  /// generation that id was announced at (task #22 learn-on-contact; §4.8 "a restarted host rejoins as a
  /// new member and holds nothing until its generation ... [is] validated"). Seeded by the membership loop
  /// at boot with each peer's generation-0 id — the seed the manifest precomputes, a placeholder until the
  /// peer's first contact — and replaced when a peer announces a **higher** generation (a restart: the old
  /// id is folded dead and taken over, the new admitted); never moved backwards (a lower generation is a
  /// stale or replayed boot, refused and counted). One entry per rostered peer, so it is bounded by the
  /// roster. Only the control shard (which probes and serves the peers) consults it; empty on a laptop.
  pub learned_members: BTreeMap<slates_db::HostId, LearnedMember>,
  /// The serve-socket demultiplexers the membership loop runs on this shard (the control shard's two
  /// planes; empty elsewhere and on a laptop), for the status report to read their counters.
  pub demuxes: Vec<&'static slates_transport::demux::Demux>,
  /// The register records this node holds as a **candidate holder** for other owners' objects (§4.8
  /// "records are sent to all candidates; committed at `f + 1`"): one durable [`Acceptor`] per object
  /// this node backs, keyed by the object. A peer's record commit — served on the per-peer record socket
  /// by [`crate::fleet::serve_peer_records`] — is accepted into the object's acceptor and **stored here**,
  /// so the record survives the serve task (the task-local acceptor it replaced held nothing a survivor
  /// could read). It is exactly the state phase-one recovery reads on a takeover: when the owner dies, the
  /// survivor that rendezvous ranks first for the object promotes over the holders, each answering from
  /// this hold, and adopts the newest committed record (§4.8 "the new owner runs phase one … adopts the
  /// newest reported record").
  ///
  /// **One acceptor per object** (not per owner): each object has a single owner at a time, so its
  /// acceptor carries one [`slates_db::register::Authority`] — the object's current owner under the
  /// configuration generation — which a takeover re-installs to the successor with
  /// [`slates_db::register::Acceptor::install_authority`]. This keeps every acceptor within the register's
  /// "one authorized owner per generation" model (the *per-object authority* a single acceptor would need
  /// to serve several owners at once is the owed refinement noted on [`slates_db::register::Authority`]),
  /// and lets a promotion for an object route to its hold by object id regardless of which peer socket
  /// carried the message. The acceptor's authority owner is the socket's TLS-authenticated peer, so a
  /// record whose owner field is not that peer is refused. Empty on a laptop (no fleet loop runs).
  pub holder_records: BTreeMap<ObjectId, Acceptor>,
  /// The objects this node must **take over** — an owner died and rendezvous ranked this node first among
  /// the survivors for the object (§4.8 "Promotion and takeover"), recorded here by the probe loop
  /// ([`crate::fleet::probe_peer`] via `sync_peer`) for the record-ship task to drive: it promotes the
  /// object's head over the surviving candidate holders (phase one, adopting the newest committed record),
  /// re-commits the adopted head under the new epoch, and records the placement — then removes the object.
  /// An object stays until its takeover places (the drive retries each period, self-healing across the
  /// window while every survivor brings its holds' authority into step). Empty on a laptop (no fleet loop
  /// runs) and whenever no takeover is outstanding.
  pub pending_takeovers: std::collections::BTreeSet<ObjectId>,
  /// A learner has evidence its configuration is behind the region and should refresh from a voter next
  /// period (§4.8 the reactive piggyback). Set when this node **received** a record naming a newer
  /// configuration generation than its own (the holder-behind case, [`crate::fleet::accept_held_record`]),
  /// or when a holder **refused** one of this node's records as `ConfigurationStale` naming a newer version
  /// (the stale-sender case, surfaced by `commit_record` and read in [`crate::fleet::ship_head`]). The
  /// record-plane coordinator fetches once when it is set (or when this node's own SWIM view diverges from
  /// its installed membership) and clears it — so an idle learner whose view matches its configuration sends
  /// nothing, replacing the per-period conditional poll. Only meaningful on the control shard (which drives
  /// and serves the council); inert elsewhere and on a laptop.
  pub config_refresh_wanted: bool,
  /// This node's client record sessions to its candidate holder peers (§4.8), keyed by peer host: each
  /// per-peer link task ([`crate::fleet::establish_record_link`]) brings its session up on one socket and
  /// installs it here as `Some`; the record-plane coordinator borrows a session for each dispatch — leaving
  /// the entry `None` while it is out — and returns it after, straggler sessions being recovered later. An
  /// absent entry means no session (never established, or lost — a borrow that ended without a return),
  /// which the link task re-establishes; a retired peer's entry is removed with it. Empty on a laptop (no
  /// fleet loop runs).
  pub record_sessions: BTreeMap<slates_db::HostId, Option<slates_transport::endpoint::Endpoint>>,
  /// What this node holds as a **content candidate** for other owners' snapshots (§4.10 "Content
  /// replication"): the distinct chunks by identity and each manifest held whole, verified before
  /// anything is stored — served on the record session's content stream by the fleet loop. It is what
  /// a takeover successor materializes a taken-over volume from, and what a reader fetches by identity.
  /// Empty on a laptop (no fleet loop runs).
  pub held_content: slates_cluster::content::ContentHold,
  /// The seals in progress for volumes this node owns (§4.10), by object: each walks the volume's newest
  /// snapshot into its archive in bounded slices, puts the archive to the content candidates until
  /// `f + 1` hold it, and is dropped once the head naming it places and the snapshot is recorded placed.
  /// Empty on a laptop and whenever every owned snapshot is placed.
  pub seals: BTreeMap<ObjectId, crate::head::SealJob>,
  /// The measured put latency of this node's **content class** (§4.8 "Derived constants": "hedge delay =
  /// measured p95 put latency per class"): one reading per binding content acknowledgement this owner
  /// shard has collected — the time from the round's dispatch to that holder's verified acknowledgement.
  /// Its p95 is the hedge trigger: how long the first content round to `f + 1` candidates is given before
  /// the remaining candidates are hedged. Bounded to a window of the newest readings; empty until the
  /// first content acknowledgement, and on a laptop, where no content round runs — the trigger is then
  /// one period (R8, the same code with an empty window).
  pub put_latency: crate::fleet::PutLatency,
  /// The measured **put-failure rate** of this owner shard's content class (§4.8 "Derived constants":
  /// "healer cadence from the measured put-failure rate"): how many content rounds ended placed and how
  /// many ended short (uncertain at the deadline, or every holder answering short of quorum), over the
  /// shard's lifetime. The healer's cadence is derived from their ratio ([`crate::fleet::heal_period_ns`]):
  /// a shard whose puts fail often walks its placed content sooner. Both zero on a laptop.
  pub put_outcomes: crate::fleet::PutOutcomes,
  /// The healer's position (§4.10 "anti-entropy … the healer"): which owned volume's placed snapshot it
  /// re-offers next, in id order, and when it last did — one snapshot per healer period, a bounded slice
  /// of the walk over everything this node has placed. Idle on a laptop (nothing is placed remotely).
  pub healer: crate::fleet::HealerCursor,
  /// How many placed snapshots the healer has **repaired** — re-put to a recorded holder that answered an
  /// offer with chunks it lacked — the non-vacuity counter a test reads (`fleet_repairs`): a healer that
  /// walks but never repairs anything cannot masquerade as working.
  pub repairs: u64,
  /// Taken-over objects whose head this node adopted and placed but whose **content** it has not yet
  /// materialized into a served volume (§4.10 "Promotion and takeover" → serve): the adopted head
  /// value, kept until the manifest's archive is held (already, as a content candidate, or fetched
  /// from a recorded holder) and the volume is created under its original id. Empty once every
  /// takeover serves.
  pub pending_materializations: BTreeMap<ObjectId, crate::head::HeadValue>,
}

/// A work volume's accumulated declared operations (§4.16), composed into an increment on submit.
pub struct WorkState {
  /// The green this work is over.
  pub green: VolumeId,
  /// The green version the work is based on.
  pub base_version: u64,
  /// The declared operations, in order.
  pub journal: Vec<VolumeOp>,
  /// The work's current content per file path — the post-state the increment's content ops name by
  /// range (a mounted work would keep this in its VFS tree; here `edit` maintains it directly).
  pub content: BTreeMap<String, Vec<u8>>,
}

/// A reply waiting to be written into a client's ring.
#[derive(Clone, Debug)]
pub struct Deferred {
  /// The client's slot index.
  pub client_index: u32,
  /// The request word.
  pub request: u64,
  /// The reply.
  pub reply: ReplyBody,
  /// Whether its completion record exists already (the owner partition recorded it).
  pub recorded: bool,
  /// The request's open `ring.request` span (§4.14), opened at the slot read and ended when this
  /// deferred reply is finally written — a reply deferred by a full ring, or one that came back from
  /// another shard, is timed from read to reply like a synchronous one. `None` only for a reply whose
  /// ring span was shed at the forwarding bound (counted as loss) or never opened (a forward that
  /// never reached its owner after the bound).
  pub ring: Option<slates_wire::observe::OpenSpan>,
}

/// A forward waiting for room on the owner shard's control channel.
pub struct PendingForward {
  /// The client's slot index.
  pub client_index: u32,
  /// The request word.
  pub request: u64,
  /// The client id.
  pub client_id: u32,
  /// The principal.
  pub principal: Principal,
  /// The body.
  pub body: slates_ipc::protocol::RequestBody,
  /// The owner shard (the runtime's id).
  pub owner: u16,
  /// The origin's `ring.request` span context (§4.14), carried to the owner so the verb's `shard.op`
  /// opens within it — one trace across the shard boundary, its cause named.
  pub cause: Option<slates_wire::observe::SpanContext>,
}

impl std::fmt::Debug for PendingForward {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("PendingForward")
      .field("client_id", &self.client_id)
      .field("owner", &self.owner)
      .finish()
  }
}

impl std::fmt::Debug for ShardState {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ShardState")
      .field("shard", &self.shard)
      .field("volumes", &self.by_id.len())
      .field("clients", &self.clients.iter().count())
      .finish()
  }
}

thread_local! {
  static STATE: RefCell<Option<ShardState>> = const { RefCell::new(None) };
  /// The control shard's set of live client ids (handed out and not yet reclaimed), so a
  /// wanted id that is live is not given twice; bounded by the daemon's client capacity.
  static HANDED: RefCell<std::collections::BTreeSet<u32>> =
    const { RefCell::new(std::collections::BTreeSet::new()) };
}

/// Borrows the control shard's set of live client ids (on the calling thread).
pub fn with_handed<R>(f: impl FnOnce(&mut std::collections::BTreeSet<u32>) -> R) -> R {
  HANDED.with(|cell| f(&mut cell.borrow_mut()))
}

/// Installs the state on the calling shard thread.
pub fn install(state: ShardState) {
  STATE.with(|cell| *cell.borrow_mut() = Some(state));
}

/// Takes the state off the calling shard thread (shutdown).
pub fn take() -> Option<ShardState> {
  STATE.with(|cell| cell.borrow_mut().take())
}

/// Borrows the state; `None` when the calling thread is not a shard with state, or the
/// state is already borrowed (a nested borrow is a bug, counted by the caller).
pub fn with_state<R>(f: impl FnOnce(&mut ShardState) -> R) -> Option<R> {
  STATE.with(|cell| {
    let mut guard = cell.try_borrow_mut().ok()?;
    guard.as_mut().map(f)
  })
}

/// Whether any client ring on this thread holds a request (the poller's question).
pub fn any_ring_ready() -> bool {
  with_state(|s| {
    s.clients.iter().any(|(_, c)| {
      c.end
        .region()
        .cmd()
        .depth(c.end.region().object())
        .is_ok_and(|d| d > 0)
    }) || !s.deferred.is_empty()
  })
  .unwrap_or(false)
}

/// A reply that came back from another shard (or a scatter's part): queued for the client's
/// ring and the server task woken.
pub fn deliver(client_index: u32, request: u64, reply: ReplyBody, recorded: bool) {
  let server = with_state(|s| {
    // The origin's `ring.request` span (§4.14), opened at the slot read and kept while the request was
    // away on another shard; it ends when this reply is written into the client's ring.
    let ring = s.forwarded_rings.remove(&request);
    s.deferred.push(Deferred {
      client_index,
      request,
      reply,
      recorded,
      ring,
    });
    s.server_task
  })
  .flatten();
  if let Some(task) = server {
    slates_rt::registry::wake(task.0);
  }
}
