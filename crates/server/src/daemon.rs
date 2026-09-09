//! The daemon (§2.6 "Boot order" steps 2, 3 and 5): attach or create the segment, start the
//! runtime's shards, recover each shard's partition and install its state, publish the
//! profile, start the doorbell thread, and run the control shard's rendezvous; stop in
//! reverse, joining everything the daemon started.

#[cfg(unix)]
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(unix)]
use slates_anchor::ENV_NFS_LISTENER;
use slates_anchor::{AnchorSegment, RegionKind};
use slates_db::catalog::Principal;
use slates_ipc::{ClientRegion, Listener, Prepared};
use slates_machine::facts::Identity;
use slates_machine::{MachineProfile, derived};
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_mem::{Handoff, Slab};
use slates_rt::control::Control;
use slates_rt::task::SpawnRequest;
use slates_rt::tcp::{Ipv4Addr, SocketAddrV4, TcpListener};
use slates_rt::{RtError, Runtime, ShardId, futures, registry};
use slates_vfs::clock::HostClock;
use slates_vfs::volume::{Store, StoreConfig};
use slates_wire::observe::{Chokepoint, ChokepointRegistry};

use crate::config::DaemonConfig;
use crate::doorbell::{DoorbellThread, Waits};
use crate::error::ServerError;
use crate::state::{self, ClientSlot, ShardState};
use crate::verbs;

/// Shape: the heartbeat cadence the control shard beats the anchor's word at (§4.14
/// `daemon.alive`): a tenth of the anchor's liveness budget, so nine beats fit inside it.
pub const HEARTBEAT_NS: u64 = 100_000_000;
/// Shape: the anchor's liveness budget for `daemon.alive` (a second; the supervisor's
/// input until the CLI takes the operator's value).
pub const LIVENESS_BUDGET_NS: u64 = 1_000_000_000;

/// Where the segment comes from.
#[derive(Clone, Debug)]
pub enum SegmentSource {
  /// Create a fresh segment (tests, the first start without an anchor).
  Create {
    /// The object's name.
    name: String,
  },
  /// Attach the segment the anchor handed over in the environment.
  FromEnv,
  /// Attach a segment by its handoff (an anchor in this process: tests, embeddings).
  Handoff {
    /// The handoff.
    handoff: Handoff,
    /// The mapped length.
    len: usize,
    /// The content object's handoff and length, if the anchor made one, so its shard content
    /// survives the restart (§4.8). `None` recreates content empty on rebuild as before (BUG-11).
    content: Option<(Handoff, usize)>,
  },
}

/// The daemon.
pub struct Daemon {
  runtime: Option<Runtime>,
  segment: AnchorSegment,
  doorbell: Option<DoorbellThread>,
  config: DaemonConfig,
  shards: Vec<ShardId>,
  /// The loopback port the NFS transport (§4.6) listens on, when it is serving; `None` if the
  /// listener could not be bound. A client mounts `nfs://localhost:PORT`.
  nfs_port: Option<u16>,
}

impl std::fmt::Debug for Daemon {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Daemon")
      .field("shards", &self.shards)
      .field("instance", &self.config.instance)
      .finish()
  }
}

/// Set by the doorbell thread each time it kicks; the control task's poller reads it.
static DOORBELL_RANG: AtomicBool = AtomicBool::new(false);
/// Shape: pending NFS connections the kernel queues before the accept loop takes them. A mount opens a
/// small, bounded number of connections; the OS clamps the backlog to the system maximum anyway.
const NFS_BACKLOG: i32 = 16;

/// The NFS loopback listener to serve: on Unix, the one a supervising anchor holds and hands over in
/// the environment ([`slates_anchor::ENV_NFS_LISTENER`]), so its port survives a daemon restart (§4.6)
/// — adopted here; failing that (a standalone start, tests, or Windows, where NFS is not the bridge) a
/// fresh ephemeral bind. Adopting the anchor's descriptor is the only path that keeps the port stable
/// across restarts.
fn nfs_listener() -> Result<TcpListener, RtError> {
  #[cfg(unix)]
  if let Some(raw) = std::env::var(ENV_NFS_LISTENER)
    .ok()
    .and_then(|value| value.parse::<RawFd>().ok())
  {
    // SAFETY: the anchor bound this listening socket and handed its descriptor to us across the spawn,
    // inherited at this number; ownership is ours now (the anchor keeps its own copy), so wrapping it
    // in an `OwnedFd` gives it a single owner that closes it on drop.
    let owned = unsafe { OwnedFd::from_raw_fd(raw) };
    return TcpListener::from_fd(owned);
  }
  TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), NFS_BACKLOG)
}
/// Shards whose initialization refused (a health signal; the daemon serves the rest).
pub static INIT_FAILURES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Clients handed to a shard that had no state to take them (a health signal).
pub static HANDOFF_LOST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Volumes the recovered catalog holds that a shard could not rebuild (a health signal).
pub static RECOVERY_SKIPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Volumes skipped from a shard publish because they could not be imaged (an overlay with base-backed
/// inodes, whose base recovery is its own gate); the rest of the shard still publishes (a health
/// signal, §4.8).
pub static PUBLISH_SKIPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Connects refused at the daemon's derived client bound (a health signal, AC-2.6).
pub static CLIENTS_REFUSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Clients found dead and reclaimed (a health signal; T-2.3).
pub static CLIENTS_REAPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// The loopback port the NFS transport serves on (§4.6), 0 until the listener is bound. Published as
/// a process-global word (like the health signals) so a verb handler on any shard can report it to a
/// client — `slates mount` reads it to run `mount_nfs localhost:PORT`.
pub static NFS_PORT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

impl Daemon {
  /// Starts the daemon from a profile.
  pub fn start(
    profile: &MachineProfile,
    config: DaemonConfig,
    source: SegmentSource,
  ) -> Result<Daemon, ServerError> {
    // The observability gate (§2.6, §4.14): the health plane refuses to serve until every chokepoint
    // span has registered its emitter, so a daemon never serves with a silently missing span source.
    // Fail-closed and checked before any resource is acquired — an incomplete roster stops the boot
    // here, naming what is missing, rather than serving blind.
    let chokepoints = registered_chokepoints();
    if !chokepoints.is_ready() {
      return Err(ServerError::ChokepointsUnregistered {
        missing: chokepoints
          .missing()
          .into_iter()
          .map(Chokepoint::name)
          .collect(),
      });
    }
    let identity = profile.facts.identity.clone();
    let mut segment = match source {
      SegmentSource::Create { name } => {
        let content_name = content_name_of(&name);
        AnchorSegment::create(&name, &identity, config.geometry)?
          .with_content(&content_name, content_bytes(&config))?
      }
      SegmentSource::FromEnv => AnchorSegment::attach_from_env(&identity)?,
      SegmentSource::Handoff {
        handoff,
        len,
        content,
      } => {
        let mut segment = AnchorSegment::attach(&handoff, len, &identity)?;
        if let Some((content_handoff, content_len)) = content {
          let object = slates_mem::SharedObject::open(&content_handoff, content_len)
            .map_err(slates_anchor::AnchorError::from)?;
          segment.adopt_content(object);
        }
        segment
      }
    };
    if let Ok(json) = profile.to_json() {
      segment.publish(RegionKind::Profile, json.as_bytes())?;
    }
    if let Some((was, now)) = limits::raise_descriptor_limit() {
      eprintln!("slates-server: descriptor limit raised from {was} to {now}");
    }
    let runtime = Runtime::start(&config.runtime)?;
    let shards: Vec<ShardId> = runtime.shard_ids().to_vec();
    for (index, shard) in shards.iter().enumerate() {
      let env = segment.handoff_env()?;
      let config = config.clone();
      let identity = identity.clone();
      let partition = u16::try_from(index).unwrap_or(u16::MAX);
      let all: Vec<u16> = shards.iter().map(|s| s.0).collect();
      runtime.spawn_on(*shard, async move {
        if let Err(e) = init_shard(&config, &env, &identity, partition, &all) {
          INIT_FAILURES.fetch_add(1, Ordering::AcqRel);
          eprintln!("slates-server: shard {partition} failed to initialize: {e}");
        }
      })?;
    }
    // The rendezvous and the doorbell.
    let listener = Listener::open(&config.instance)?;
    let kicks: Vec<slates_rt::driver::Kick> = shards
      .iter()
      .filter_map(|s| registry::entry(s.0).map(|e| e.kick))
      .collect();
    let waits = match listener.doorbell_waiter()? {
      Some((object, offset)) => Waits::Word { object, offset },
      None => Waits::Socket(listener.raw_fd().unwrap_or(-1)),
    };
    let doorbell = DoorbellThread::start(waits, kicks, &DOORBELL_RANG);
    let control_config = config.clone();
    let control_env = segment.handoff_env()?;
    let control_identity = identity.clone();
    let control = shards.first().copied().ok_or(ServerError::NotOnShard)?;
    let shard_ids = shards.clone();
    runtime.spawn_on(control, async move {
      control_loop(
        listener,
        control_config,
        control_env,
        control_identity,
        shard_ids,
      )
      .await;
    })?;
    // The NFS transport (§4.6): one loopback listener served on the control shard. A supervising
    // anchor holds the listener and hands its descriptor over in the environment, so its port survives
    // a daemon restart (Unix); the daemon adopts that when present, or binds a fresh ephemeral one when
    // it runs standalone (tests). Either way the port is known here, before the serve task moves the
    // listener onto the shard, and the cross-shard bridge queue reaches volumes on other shards.
    let nfs_port = match nfs_listener() {
      Ok(nfs_listener) => {
        let port = nfs_listener.local_addr().ok().map(|addr| addr.port());
        if let Some(port) = port {
          // Publish the port for a verb handler to report to a client (`slates mount`).
          NFS_PORT.store(u32::from(port), Ordering::Release);
          runtime.spawn_on(control, async move {
            if let Ok(task) = futures::spawn(crate::nfs::serve(nfs_listener, port)) {
              let _ = futures::detach(task);
            }
          })?;
        }
        port
      }
      Err(_) => None,
    };
    Ok(Daemon {
      runtime: Some(runtime),
      segment,
      doorbell: Some(doorbell),
      config,
      shards,
      nfs_port,
    })
  }

  /// The loopback port the NFS transport (§4.6) is serving on, if the listener bound; a client mounts
  /// `nfs://localhost:PORT` to reach this daemon's volumes (those on its accepting shard, R8).
  pub fn nfs_port(&self) -> Option<u16> {
    self.nfs_port
  }

  /// The configuration.
  pub fn config(&self) -> &DaemonConfig {
    &self.config
  }

  /// The shards.
  pub fn shards(&self) -> &[ShardId] {
    &self.shards
  }

  /// The segment (the daemon's own mapping).
  pub fn segment(&self) -> &AnchorSegment {
    &self.segment
  }

  /// Stops the daemon: the doorbell thread, then every shard, joined.
  pub fn stop(mut self) {
    if let Some(mut doorbell) = self.doorbell.take() {
      doorbell.stop();
    }
    if let Some(runtime) = self.runtime.take() {
      runtime.shutdown();
    }
  }
}

impl Drop for Daemon {
  fn drop(&mut self) {
    if let Some(mut doorbell) = self.doorbell.take() {
      doorbell.stop();
    }
    if let Some(runtime) = self.runtime.take() {
      runtime.shutdown();
    }
  }
}

/// The chokepoint span emitters this daemon declares at boot (§4.14 "Span roster", §2.6): the health
/// plane refuses to serve until every one has registered. Registration proves the emitter *exists*,
/// not that it is *live* (a registered emitter may be idle — a laptop runs no consensus or replication
/// yet still declares those chokepoints, R8). Each line names the subsystem that owns the emitter, so
/// a future refactor moves each `register` to that subsystem's own initialization and this function
/// becomes the point that confirms they all reported in. A missing line here shuts the gate.
fn registered_chokepoints() -> ChokepointRegistry {
  let mut registry = ChokepointRegistry::new();
  registry.register(Chokepoint::BridgeRequest); // the NFS/FSKit bridge request (§4.6, `crate::nfs`)
  registry.register(Chokepoint::RingRequest); // a client ring slot (the control loop, below)
  registry.register(Chokepoint::ShardOp); // one verb on its owner shard (`crate::verbs`)
  registry.register(Chokepoint::LogAppend); // an op-log record appended (`slates_db::replay`)
  registry.register(Chokepoint::ShipRecord); // a record shipped to its candidates (§4.8 register)
  registry.register(Chokepoint::ConsensusStep); // a configuration commit (§4.8 config group)
  registry.register(Chokepoint::ArchiveChunk); // a chunk compressed or expanded (§4.10 archive)
  registry.register(Chokepoint::LandEntry); // one landing entry (§4.15, `crate::landing`)
  registry.register(Chokepoint::MergeVerdict); // one increment judged (§4.16 merge)
  registry
}

/// The node's host id: the first eight bytes of the machine identity's hash.
fn host_id_of(identity: &Identity) -> u64 {
  let hash = identity.hash();
  u64::from_le_bytes([
    hash[0], hash[1], hash[2], hash[3], hash[4], hash[5], hash[6], hash[7],
  ])
}

/// The content object's name for a segment named `seg_name`: the segment name with its `seg`
/// marker replaced by `con`, so it stays the same length (within the platform's shared-object name
/// limit the segment already meets) and is distinct from the segment's own name.
fn content_name_of(seg_name: &str) -> String {
  match seg_name.strip_prefix("slates-seg-") {
    Some(rest) => format!("slates-con-{rest}"),
    None => format!("{seg_name}-c"),
  }
}

/// Slots per shard in the content object: the recovery image is published as a double buffer (§4.8),
/// so each shard's slice holds two reserve-sized slots — the last committed image and the one being
/// published. An interrupted publish lands in the non-committed slot, so the committed one always
/// survives. Two is the minimum for that guarantee (a single slot cannot survive a torn write of
/// itself); more slots would only add unused space.
const PUBLISH_SLOTS: usize = 2;

/// The content object's total size: two reserve-sized slots per shard (a double buffer) times the
/// partitions, so each shard owns space for its committed recovery image and the one it is writing
/// (§4.8). The object is lazily backed, so the unused tail costs address space, not RAM, until an
/// image is published into it.
fn content_bytes(config: &DaemonConfig) -> usize {
  let per_shard = usize::try_from(config.reserve_per_shard).unwrap_or(usize::MAX);
  per_shard
    .saturating_mul(PUBLISH_SLOTS)
    .saturating_mul(usize::from(config.geometry.partitions.max(1)))
}

fn handoff_of(env: &[(String, String)]) -> Result<(Handoff, usize), ServerError> {
  let handoff = env
    .iter()
    .find(|(k, _)| k == slates_anchor::segment::ENV_HANDOFF)
    .map(|(_, v)| v.clone())
    .ok_or(ServerError::NotOnShard)?;
  let len: usize = env
    .iter()
    .find(|(k, _)| k == slates_anchor::segment::ENV_LEN)
    .and_then(|(_, v)| v.parse().ok())
    .ok_or(ServerError::NotOnShard)?;
  let handoff = match handoff.parse::<i32>() {
    Ok(fd) if cfg!(target_os = "linux") => Handoff::Descriptor(fd),
    _ => Handoff::Name(handoff),
  };
  Ok((handoff, len))
}

/// Runs on the shard: attaches the segment, recovers the partition, builds the store and
/// installs the state, then spawns the server loop as a poller.
fn init_shard(
  config: &DaemonConfig,
  env: &[(String, String)],
  identity: &Identity,
  partition: u16,
  config_shards: &[u16],
) -> Result<(), ServerError> {
  let (handoff, len) = handoff_of(env)?;
  let mut segment = AnchorSegment::attach(&handoff, len, identity)?;
  let mut clock = HostClock::new();
  let now = slates_vfs::clock::Clock::monotonic_ns(&mut clock);
  let (db, recovered) = slates_db::replay::recover(&mut segment, partition, config.caps, now)?;
  let mut arena = ChunkArena::new(config.page);
  let region_len = usize::try_from(config.reserve_per_shard).unwrap_or(usize::MAX);
  arena.add_region(Region::map(
    region_len.max(config.page),
    config.page,
    config.huge_pages,
  )?)?;
  // The operation headroom (§4.2): the bounded temporary coexistence of in-flight operations, kept
  // free of every admission (reservation and dynamic growth alike). A write into a sealed chunk
  // copies it into a new open extent — copy-on-write at chunk granularity — so the source chunk and
  // its destination coexist (two chunk windows) until the seal, and at most one such copy-up is in
  // flight per client the shard serves. This is structural — the chunk window times the client count
  // — not the measured burst the earlier placeholder stood in for.
  let chunk_window =
    u64::try_from(slates_vfs::content::chunk_bytes(config.page).get()).unwrap_or(u64::MAX);
  let concurrent_writers = u64::try_from(config.clients_per_shard).unwrap_or(1).max(1);
  let headroom = derived!(
    chunk_window
      .saturating_mul(2)
      .saturating_mul(concurrent_writers),
    "2 × chunk_bytes × clients_per_shard (a copy-up's source and destination chunk per concurrent writer)",
    ["vfs.chunk_bytes", "clients_per_shard"]
  );
  // The store owns the shard budget (§4.2): it is over what the arena can actually hand out (its
  // buddy-allocatable capacity), not the region's mapping length, so admission never promises quota
  // the arena cannot back (BUG-2), and it keeps the derived operation headroom free of every
  // admission. Living with the store, the write path reaches it without a lock.
  let store = Store::new(
    &StoreConfig {
      page: config.page,
      cache_line: config.cache_line,
      max_dirs: config.store.max_dirs,
      max_inodes: config.store.max_inodes,
      max_chunks: config.store.max_chunks,
      max_dir_blocks: config.store.max_dir_blocks,
      dir_cutover: config.store.dir_cutover,
    },
    arena,
    headroom.get(),
  );
  let shard = registry::current_shard().unwrap_or(partition);
  // The node's host id: the machine identity's hash, stable across restarts, distinct per
  // machine, so a recorded holder set and a volume id's creator-host bits mean the same
  // thing when Phase 8 adds peers. One host, `f = 0`, on a laptop.
  let host = slates_db::HostId(host_id_of(identity));
  let config_register = slates_db::Configuration::solo(host);
  // The anchor-owned content object that survives a restart (§4.8), if the anchor provides one.
  // Shards share the one object, partitioned by index: this shard owns the slice `[start, end)`.
  let content = match AnchorSegment::open_content(env) {
    Some(result) => Some(result?),
    None => None,
  };
  let content_range = match &content {
    Some(object) => {
      let partitions = config_shards.len().max(1);
      let per_shard = object.len() / partitions;
      let start = usize::from(partition).saturating_mul(per_shard);
      (start, start.saturating_add(per_shard))
    }
    None => (0, 0),
  };
  // The telemetry sink keeps the most recent spans up to one client ring's depth (§4.14): a shard
  // processes at most a ring of in-flight requests, so a ring's depth of recent spans covers the
  // current activity window; older spans are shed (and counted), telemetry being the shed-first class.
  let telemetry_capacity = usize::try_from(config.region.slots).unwrap_or(1).max(1);
  let mut state = ShardState {
    shard,
    partition,
    config: config.clone(),
    segment,
    content,
    content_range,
    db,
    config_register,
    landing: crate::landing::LandingState::default(),
    store,
    volumes: Slab::new(config.caps.segment_slots, config.caps.volumes),
    by_id: std::collections::BTreeMap::new(),
    clients: Slab::new(config.caps.segment_slots, config.clients_per_shard),
    next_prefix: partition
      .saturating_mul(u16::try_from(config.caps.segment_slots).unwrap_or(u16::MAX))
      .max(1),
    next_attachment: 1,
    clock,
    served: 0,
    refusals: std::collections::BTreeMap::new(),
    deferred: Vec::new(),
    server_task: None,
    scatters: std::collections::BTreeMap::new(),
    shards: config_shards.to_vec(),
    recovered,
    booted_ns: now,
    status_scatters: std::collections::BTreeMap::new(),
    last_work_ns: now,
    pending_forwards: std::collections::VecDeque::new(),
    greens: std::collections::BTreeMap::new(),
    works: std::collections::BTreeMap::new(),
    ack_scatters: std::collections::BTreeMap::new(),
    telemetry: slates_wire::observe::SpanSink::with_capacity(telemetry_capacity),
    next_span_id: 1,
    current_request: slates_wire::request::RequestId::default(),
  };
  let rebuilt = verbs::rebuild_recovered(&mut state);
  if rebuilt.skipped > 0 {
    RECOVERY_SKIPPED.fetch_add(
      u64::try_from(rebuilt.skipped).unwrap_or(u64::MAX),
      Ordering::AcqRel,
    );
  }
  if rebuilt != verbs::Rebuilt::default() {
    // Say what recovery did, not more: identities were rebuilt (content is recreated empty until it
    // is anchor-backed, BUG-11), and the content-less local snapshots and attachments were dropped.
    eprintln!(
      "slates-server: shard {shard}: rebuilt {} volume identities (content empty; {} skipped), {} merge volumes (green chains replayed, works reset), dropped {} local snapshots and {} attachments",
      rebuilt.volumes,
      rebuilt.skipped,
      rebuilt.merge_volumes,
      rebuilt.snapshots_dropped,
      rebuilt.attachments_dropped
    );
  }
  state::install(state);
  // Detached: the loop lives as long as the shard; nothing joins it (a joinable task stays in
  // the arena after it ends, which would hold the shard's shutdown).
  if let Ok(task) = futures::spawn(serve_loop()) {
    let _ = futures::detach(task);
  }
  if let Ok(task) = futures::spawn(reap_loop()) {
    let _ = futures::detach(task);
  }
  Ok(())
}

/// The shard's sweep for dead clients and expired leases, at the liveness cadence: the same
/// budget the anchor allows the daemon's own heartbeat, so a peer silent that long is asked
/// about (§4.7 "Failure matrix"). Derived: the cadence is the budget; a client is asked about
/// once silent for the budget, so a death is seen within two budgets at most.
async fn reap_loop() {
  let cadence = derived!(
    LIVENESS_BUDGET_NS,
    "the liveness budget (the anchor's question of the daemon, asked of the daemon's clients)",
    ["LIVENESS_BUDGET_NS"]
  )
  .get();
  loop {
    futures::sleep(cadence).await;
    let reaped = state::with_state(|s| {
      let _ = verbs::expire_leases(s);
      verbs::reap_dead_clients(s, cadence)
    })
    .unwrap_or_default();
    if reaped.clients > 0 {
      CLIENTS_REAPED.fetch_add(
        u64::try_from(reaped.clients).unwrap_or(u64::MAX),
        Ordering::AcqRel,
      );
    }
  }
}

/// The shard's server loop: a poller of its clients' rings; serves while there is work, keeps
/// polling for the idle window after its last work (§4.7 "Wake strategy": a shard polls while
/// any client has activity within the window), and idles past it.
async fn serve_loop() {
  if let Some(task) = futures::current_task() {
    let _ =
      registry::with_current(|ctx| ctx.register_poller(task, Box::new(state::any_ring_ready)));
    state::with_state(|s| s.server_task = Some(task));
  }
  let idle_window_ns = state::with_state(|s| {
    derived!(
      u64::from(s.config.region.spin_ns).saturating_mul(crate::config::IDLE_WINDOW_RATIO),
      "spin_ns × IDLE_WINDOW_RATIO",
      ["wake.p99_ns", "IDLE_WINDOW_RATIO"]
    )
    .get()
  })
  .unwrap_or(0);
  loop {
    let (did, within_window) = state::with_state(|s| {
      let did = verbs::serve_round(s);
      let now = slates_vfs::clock::Clock::monotonic_ns(&mut s.clock);
      if did {
        s.last_work_ns = now;
      }
      (did, now.saturating_sub(s.last_work_ns) < idle_window_ns)
    })
    .unwrap_or((false, false));
    if did || within_window {
      futures::yield_now().await;
    } else {
      state::with_state(|s| verbs::mark_parked(s, true));
      futures::idle().await;
      state::with_state(|s| verbs::mark_parked(s, false));
    }
  }
}

/// The control shard's loop: the rendezvous, the heartbeat, and a client handed to its shard
/// as a spawned task (sharing by move).
async fn control_loop(
  mut listener: Listener,
  config: DaemonConfig,
  env: Vec<(String, String)>,
  identity: Identity,
  shards: Vec<ShardId>,
) {
  let Ok((handoff, len)) = handoff_of(&env) else {
    return;
  };
  let Ok(segment) = AnchorSegment::attach(&handoff, len, &identity) else {
    return;
  };
  if let Some(task) = futures::current_task() {
    let _ = registry::with_current(|ctx| {
      ctx.register_poller(
        task,
        Box::new(|| DOORBELL_RANG.swap(false, Ordering::AcqRel)),
      )
    });
  }
  if let Ok(task) = futures::spawn(heartbeat_loop(segment)) {
    let _ = futures::detach(task);
  }
  // Clients this daemon handed out, so a wanted id that is live is not given twice; bounded
  // by the daemon's client capacity, refused typed beyond it (AC-2.6). A client's shard is
  // its id's residue, so a client reconnecting under its old id after a restart lands on the
  // shard that holds its completion records (§4.9), with fresh ids still round-robin.
  let bound = derived!(
    config.clients_per_shard.saturating_mul(shards.len()),
    "clients_per_shard × shards",
    ["clients_per_shard", "shards"]
  )
  .get();
  loop {
    let accepted = listener.accept_pending(
      &|id| state::with_handed(|h| h.contains(&id)),
      &mut |client_id| {
        if state::with_handed(|h| h.len()) >= bound {
          CLIENTS_REFUSED.fetch_add(1, Ordering::AcqRel);
          return Err(slates_ipc::IpcError::TooManyClients { limit: bound });
        }
        let shard = shards[usize::try_from(client_id).unwrap_or(0) % shards.len().max(1)];
        let region = ClientRegion::create(
          &format!("slates-cr-{}-{client_id}", config.instance),
          client_id,
          shard.0,
          config.region,
        )?;
        Ok(Prepared {
          region,
          kick_fd: kick_fd_of(shard),
        })
      },
    );
    if let Ok(accepted) = accepted {
      for a in accepted {
        let shard = ShardId(a.region.shard());
        let principal = Principal::Uid { uid: a.uid };
        let client_id = a.client_id;
        state::with_handed(|h| h.insert(client_id));
        let control = a.control;
        let pid = a.pid;
        let end = slates_ipc::DaemonEnd::new(a.region);
        let request = Box::new(SpawnRequest::new(
          Box::pin(async move {
            // The server task may be idle with its parked flags set on the clients it knew;
            // a wake makes it mark the new client too before it idles again.
            let server = state::with_state(|s| {
              let last_seen_ns = slates_vfs::clock::Clock::monotonic_ns(&mut s.clock);
              if let Err(e) = s.clients.insert(ClientSlot {
                end,
                principal,
                client_id,
                pid,
                last_seen_ns,
                control,
              }) {
                eprintln!("slates-server: client {client_id} refused by the shard's table: {e}");
              }
              s.server_task
            });
            match server {
              Some(Some(task)) => {
                registry::wake(task.0);
              }
              Some(None) => {}
              None => {
                HANDOFF_LOST.fetch_add(1, Ordering::AcqRel);
                eprintln!("slates-server: client {client_id} handed to a shard without state");
              }
            }
          }),
          None,
        ));
        if let Err(e) = registry::send_control(shard.0, Control::Spawn(request)) {
          HANDOFF_LOST.fetch_add(1, Ordering::AcqRel);
          eprintln!(
            "slates-server: client {client_id} could not be handed to shard {}: {e}",
            shard.0
          );
        }
        let _ = registry::send_control(shard.0, Control::Active(true));
      }
    }
    futures::idle().await;
  }
}

/// The heartbeat: the anchor's `daemon.alive` input, beaten at a cadence inside its budget.
async fn heartbeat_loop(segment: AnchorSegment) {
  let mut clock = HostClock::new();
  loop {
    let now = slates_vfs::clock::Clock::monotonic_ns(&mut clock);
    if let Ok(sup) = segment.supervision() {
      sup.beat(now);
    }
    futures::sleep(HEARTBEAT_NS).await;
  }
}

#[cfg(target_os = "linux")]
fn kick_fd_of(shard: ShardId) -> Option<i32> {
  use std::os::fd::AsRawFd;
  match registry::entry(shard.0).map(|e| e.kick) {
    Some(slates_rt::driver::Kick::Eventfd(fd)) => Some(fd.as_raw_fd()),
    _ => None,
  }
}

#[cfg(not(target_os = "linux"))]
fn kick_fd_of(_shard: ShardId) -> Option<i32> {
  None
}

mod limits {
  //! The process's descriptor limit: every client costs descriptors (its region, its control
  //! channel, its completion signal), and the derived client bound (AC-2.6) is far above the
  //! soft limit a shell hands out; the daemon raises its own soft limit to the hard one, which
  //! needs no privilege. Paired `#[cfg]` functions, one signature.

  /// Raises the soft limit to the hard one; `(before, after)` when it changed.
  #[cfg(unix)]
  pub(super) fn raise_descriptor_limit() -> Option<(u64, u64)> {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    let limit = getrlimit(Resource::Nofile);
    let current = limit.current?;
    let maximum = limit.maximum.unwrap_or(u64::MAX);
    let wanted = platform_cap(maximum);
    if wanted <= current {
      return None;
    }
    setrlimit(
      Resource::Nofile,
      Rlimit {
        current: Some(wanted),
        maximum: limit.maximum,
      },
    )
    .ok()?;
    Some((current, wanted))
  }

  /// Format: macOS `OPEN_MAX` (`<sys/syslimits.h>`): `setrlimit` refuses a soft descriptor
  /// limit above it even when the hard limit is unlimited.
  #[cfg(target_os = "macos")]
  const MACOS_OPEN_MAX: u64 = 10_240;

  /// macOS caps the soft limit at `OPEN_MAX`.
  #[cfg(target_os = "macos")]
  fn platform_cap(maximum: u64) -> u64 {
    maximum.min(MACOS_OPEN_MAX)
  }

  #[cfg(all(unix, not(target_os = "macos")))]
  fn platform_cap(maximum: u64) -> u64 {
    maximum
  }

  /// Windows has no per-process handle limit to raise.
  #[cfg(not(unix))]
  pub(super) fn raise_descriptor_limit() -> Option<(u64, u64)> {
    None
  }
}

#[cfg(test)]
mod tests {
  use super::registered_chokepoints;
  use slates_wire::observe::Chokepoint;

  /// The daemon declares every chokepoint span, so the observability gate opens and it serves (§2.6,
  /// §4.14). This is the daemon side of the roster doc-truth: if a `register` line were dropped from
  /// [`registered_chokepoints`], the gate would name the missing chokepoint and `Daemon::start` would
  /// refuse — this test catches that omission without spawning a daemon. The live daemon tests
  /// (`crates/server/tests`) are the by-use proof that an open gate actually serves.
  #[test]
  fn the_daemon_declares_every_chokepoint_so_the_gate_opens() {
    let registry = registered_chokepoints();
    assert!(
      registry.is_ready(),
      "the daemon must register every chokepoint to serve; missing: {:?}",
      registry
        .missing()
        .into_iter()
        .map(Chokepoint::name)
        .collect::<Vec<_>>()
    );
  }
}
