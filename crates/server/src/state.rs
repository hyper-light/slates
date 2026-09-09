//! The shard's state: its volumes, its partition of the database, its clients, and the
//! reserve; one cell per shard thread, borrowed briefly by the shard's tasks (§4.3: one
//! owning shard per volume; §4.8: one writer per partition).

use std::cell::RefCell;
use std::collections::BTreeMap;

use slates_anchor::AnchorSegment;
use slates_base::OsHost;
use slates_db::Db;
use slates_db::catalog::{Principal, VolumeId};
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
}

impl std::fmt::Debug for VolumeSlot {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("VolumeSlot").field("id", &self.id).finish()
  }
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
  /// The partition.
  pub db: Db,
  /// The register configuration (§4.8): the owner host, its epoch, its neighbourhood and
  /// the quorum; `Configuration::solo` at `f = 0`, the same type a fleet uses.
  pub config_register: slates_db::Configuration,
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
  /// Replies waiting for the client's ring (full, or the reply came from another shard), by
  /// client slot; `recorded` says the completion record already exists (at the owner
  /// partition of a forwarded verb), so this shard must not record it again.
  pub deferred: Vec<Deferred>,
  /// The shard's server task, woken when a client is added while it idles.
  pub server_task: Option<slates_rt::TaskId>,
  /// Listings in flight: by request word, the client slot, the shards still to answer, and
  /// the summaries so far (a scatter-gather over owners, §4.8 "Lookup").
  pub scatters: BTreeMap<u64, (u32, usize, Vec<slates_ipc::protocol::VolumeSummary>)>,
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
  /// This shard's bounded telemetry sink (§4.14): the chokepoint spans emitted on the shard, held
  /// shed-first (the newest kept, the oldest dropped and counted). Per-shard and thread-local, so it
  /// needs no lock (R2) — the design's "per-shard rings"; a control-shard aggregation is owed.
  pub telemetry: slates_wire::observe::SpanSink,
  /// The next span id this shard assigns (§4.14, the three-id law): a per-shard monotonic counter, so
  /// every span this shard emits has a distinct id within the shard (and distinct across the daemon
  /// with the partition in view). It never resets except by a restart.
  pub next_span_id: u64,
  /// The request the shard is serving right now (§4.14): set at the start of `run_recorded`, so a
  /// fine-grained chokepoint deeper in a verb (a `merge.verdict`, a `land.entry`) can stamp its span
  /// with the real request identity without threading it through every handler. The shard is
  /// single-threaded and runs one verb at a time with no awaits inside (§4.7), so this is unambiguous.
  pub current_request: slates_wire::request::RequestId,
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
  /// The monotonic time the request was read from the ring, for the `ring.request` span (§4.14),
  /// emitted when this deferred reply is finally written. Zero means "unknown" — a reply that arrived
  /// from another shard (a forward's `deliver`), whose `ring.request` span at the origin is owed and
  /// so is not emitted here rather than emitted with a wrong duration.
  pub read_ns: u64,
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
    s.deferred.push(Deferred {
      client_index,
      request,
      reply,
      recorded,
      // A cross-shard reply (a forward's return or a scatter's part): the origin's `ring.request`
      // span start was not threaded across the shard boundary, so it is not emitted (owed), not
      // emitted with a wrong duration.
      read_ns: 0,
    });
    s.server_task
  })
  .flatten();
  if let Some(task) = server {
    slates_rt::registry::wake(task.0);
  }
}
