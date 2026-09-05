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
use slates_mem::Slab;
use slates_mem::budget::ShardBudget;
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
  /// The volume.
  pub volume: Volume,
  /// The read-only host of the base directory, for an overlay.
  pub host: Option<OsHost>,
  /// The reservation, for a bounded volume.
  pub reservation: Option<slates_mem::budget::Reservation>,
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
  /// The partition.
  pub db: Db,
  /// The store.
  pub store: Store,
  /// The volumes.
  pub volumes: Slab<VolumeSlot>,
  /// Volumes by id.
  pub by_id: BTreeMap<VolumeId, slates_mem::Handle<VolumeSlot>>,
  /// The clients.
  pub clients: Slab<ClientSlot>,
  /// The reserve.
  pub budget: ShardBudget,
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
  /// Replies waiting because a client's completion ring was full, by client slot.
  pub deferred: Vec<(u32, u64, ReplyBody)>,
  /// The shard's server task, woken when a client is added while it idles.
  pub server_task: Option<slates_rt::TaskId>,
  /// Listings in flight: by request word, the client slot, the shards still to answer, and
  /// the summaries so far (a scatter-gather over owners, §4.8 "Lookup").
  pub scatters: BTreeMap<u64, (u32, usize, Vec<slates_ipc::protocol::VolumeSummary>)>,
  /// Every shard of the daemon, for the scatter.
  pub shards: Vec<u16>,
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
pub fn deliver(client_index: u32, request: u64, reply: ReplyBody) {
  let server = with_state(|s| {
    s.deferred.push((client_index, request, reply));
    s.server_task
  })
  .flatten();
  if let Some(task) = server {
    slates_rt::registry::wake(task.0);
  }
}
