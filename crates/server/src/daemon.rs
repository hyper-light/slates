//! The daemon (§2.6 "Boot order" steps 2, 3 and 5): attach or create the segment, start the
//! runtime's shards, recover each shard's partition and install its state, publish the
//! profile, start the doorbell thread, and run the control shard's rendezvous; stop in
//! reverse, joining everything the daemon started.

use std::sync::atomic::{AtomicBool, Ordering};

use slates_anchor::{AnchorSegment, RegionKind};
use slates_db::catalog::Principal;
use slates_ipc::{ClientRegion, Listener, Prepared};
use slates_machine::facts::Identity;
use slates_machine::{MachineProfile, derived};
use slates_mem::arena::ChunkArena;
use slates_mem::budget::ShardBudget;
use slates_mem::region::Region;
use slates_mem::{Handoff, Slab};
use slates_rt::control::Control;
use slates_rt::task::SpawnRequest;
use slates_rt::{Runtime, ShardId, futures, registry};
use slates_vfs::clock::HostClock;
use slates_vfs::volume::{Store, StoreConfig};

use crate::config::DaemonConfig;
use crate::doorbell::{DoorbellThread, Waits};
use crate::error::ServerError;
use crate::state::{self, ClientSlot, ShardState};
use crate::verbs;

/// Shape: the heartbeat cadence the control shard beats the anchor's word at (§4.14
/// `daemon.alive`): a tenth of the anchor's liveness budget, so nine beats fit inside it.
const HEARTBEAT_NS: u64 = 100_000_000;
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
}

/// The daemon.
pub struct Daemon {
  runtime: Option<Runtime>,
  segment: AnchorSegment,
  doorbell: Option<DoorbellThread>,
  config: DaemonConfig,
  shards: Vec<ShardId>,
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
/// Shards whose initialization refused (a health signal; the daemon serves the rest).
pub static INIT_FAILURES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Clients handed to a shard that had no state to take them (a health signal).
pub static HANDOFF_LOST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl Daemon {
  /// Starts the daemon from a profile.
  pub fn start(
    profile: &MachineProfile,
    config: DaemonConfig,
    source: SegmentSource,
  ) -> Result<Daemon, ServerError> {
    let identity = profile.facts.identity.clone();
    let mut segment = match source {
      SegmentSource::Create { name } => AnchorSegment::create(&name, &identity, config.geometry)?,
      SegmentSource::FromEnv => AnchorSegment::attach_from_env(&identity)?,
    };
    if let Ok(json) = profile.to_json() {
      segment.publish(RegionKind::Profile, json.as_bytes())?;
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
    Ok(Daemon {
      runtime: Some(runtime),
      segment,
      doorbell: Some(doorbell),
      config,
      shards,
    })
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
  let (db, _recovered) = slates_db::replay::recover(&mut segment, partition, config.caps, now)?;
  let mut arena = ChunkArena::new(config.page);
  let region_len = usize::try_from(config.reserve_per_shard).unwrap_or(usize::MAX);
  arena.add_region(Region::map(
    region_len.max(config.page),
    config.page,
    config.huge_pages,
  )?)?;
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
  );
  let peak_burst = derived!(
    config.reserve_per_shard
      / u64::from(u32::try_from(config.clients_per_shard).unwrap_or(1).max(1)),
    "reserve_per_shard / clients_per_shard until the burst is measured (§4.2)",
    ["reserve_per_shard", "clients_per_shard"]
  );
  let shard = registry::current_shard().unwrap_or(partition);
  let state = ShardState {
    shard,
    config: config.clone(),
    segment,
    db,
    store,
    volumes: Slab::new(config.caps.segment_slots, config.caps.volumes),
    by_id: std::collections::BTreeMap::new(),
    clients: Slab::new(config.caps.segment_slots, config.clients_per_shard),
    budget: ShardBudget::new(config.reserve_per_shard, peak_burst.get()),
    next_prefix: shard
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
  };
  state::install(state);
  // Detached: the loop lives as long as the shard; nothing joins it (a joinable task stays in
  // the arena after it ends, which would hold the shard's shutdown).
  if let Ok(task) = futures::spawn(serve_loop()) {
    let _ = futures::detach(task);
  }
  Ok(())
}

/// The shard's server loop: a poller of its clients' rings; serves while there is work,
/// idles otherwise.
async fn serve_loop() {
  if let Some(task) = futures::current_task() {
    let _ =
      registry::with_current(|ctx| ctx.register_poller(task, Box::new(state::any_ring_ready)));
    state::with_state(|s| s.server_task = Some(task));
  }
  loop {
    let did = state::with_state(verbs::serve_round).unwrap_or(false);
    if did {
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
  let mut next_shard = 0usize;
  loop {
    let accepted = listener.accept_pending(&mut |client_id| {
      let shard = shards[next_shard % shards.len().max(1)];
      next_shard += 1;
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
    });
    if let Ok(accepted) = accepted {
      for a in accepted {
        let shard = ShardId(a.region.shard());
        let principal = Principal::Uid { uid: a.uid };
        let client_id = a.client_id;
        let end = slates_ipc::DaemonEnd::new(a.region);
        let request = Box::new(SpawnRequest::new(
          Box::pin(async move {
            // The server task may be idle with its parked flags set on the clients it knew;
            // a wake makes it mark the new client too before it idles again.
            let server = state::with_state(|s| {
              if let Err(e) = s.clients.insert(ClientSlot {
                end,
                principal,
                client_id,
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
