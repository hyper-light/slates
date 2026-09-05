//! The runtime: the configuration derived from the profile, the OS runtime that owns one thread
//! per shard, the local runtime that runs one shard on the calling thread, and the shard-pair
//! ring wiring both share with the simulation (§4.3).

use std::future::Future;
use std::ptr::NonNull;
use std::thread::JoinHandle;

use slates_machine::{Derived, MachineProfile, derived};
use slates_mem::SpscRing;

use crate::driver::{Driver, os_driver};
use crate::error::RtError;
use crate::msg::Msg;
use crate::registry;
use crate::shard::{Counters, ShardContext, ShardId, TaskId};
use crate::task::SpawnRequest;

/// The runtime's configuration; every number is derived or measured by the caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeConfig {
  /// Shards to run.
  pub shards: u16,
  /// The admission limit per shard (Little's law on the measured request rate and p99 service
  /// time; see [`admission_limit`]).
  pub tasks_per_shard: usize,
  /// Timers a shard may hold at once.
  pub timers_per_shard: usize,
  /// Entries in each inbound ring.
  pub ring_entries: usize,
  /// The step budget the watchdog counts against, in nanoseconds.
  pub step_budget_ns: u64,
  /// The timing wheel's tick, in nanoseconds.
  pub timer_tick_ns: u64,
  /// The bound on items processed per loop phase.
  pub batch: usize,
  /// Whether to pin shard threads to cores.
  pub pin: bool,
  /// The core ids shard threads are pinned to, in shard order (empty: the OS chooses).
  pub cores: Vec<u32>,
  /// The base page in bytes, for sizing the task slab's segments.
  pub page_bytes: usize,
  /// How long an idle shard spins checking its rings before parking, while a client is active
  /// (the 2-competitive bound: the measured wake cost).
  pub spin_ns: u64,
}

impl RuntimeConfig {
  /// A configuration from the profile: the shard count and their cores from the core classes,
  /// the tick, step budget, spin window and ring size from the derived constants, and the batch
  /// bound calibrated against `latency_budget_ns` (see [`RuntimeConfig::calibrate_batch`]). The
  /// task and timer limits come from the caller's measurements (Little's law, [`admission_limit`]).
  pub fn from_profile(
    profile: &MachineProfile,
    tasks_per_shard: usize,
    timers_per_shard: usize,
    latency_budget_ns: u64,
  ) -> Self {
    let d = profile.derived();
    let ring_entries = usize::try_from(d.ring_entries.get()).unwrap_or(usize::MAX);
    let (shards, cores) = shard_cores(profile);
    let mut config = Self {
      shards: shards.get(),
      tasks_per_shard,
      timers_per_shard,
      ring_entries,
      step_budget_ns: d.task_step_budget_ns.get(),
      timer_tick_ns: d.timer_tick_ns.get(),
      batch: ring_entries,
      pin: true,
      cores,
      page_bytes: usize::try_from(profile.facts.page.base).unwrap_or(1),
      spin_ns: d.spin_before_park_ns.get(),
    };
    config.batch = config.calibrate_batch(latency_budget_ns).get();
    config
  }

  /// The batch bound: the latency budget divided by the measured cost of one loop item (a task
  /// poll of a trivial future through the whole loop), measured here and now on a simulated
  /// shard, so the bound is never a guess (§4.3, "batch bound = latency budget / measured
  /// per-item cost").
  pub fn calibrate_batch(&self, latency_budget_ns: u64) -> Derived<usize> {
    let per_item_ns = measured_item_cost_ns(self);
    derived!(
      usize::try_from(latency_budget_ns / per_item_ns.max(1))
        .unwrap_or(usize::MAX)
        .clamp(1, self.ring_entries.max(1)),
      "latency budget / measured per-item loop cost, clamped to [1, ring entries]",
      [
        "rt.latency_budget_ns",
        "rt.item_cost_ns (measured at start)",
        "rt.ring_entries"
      ]
    )
  }

  /// Task slots per slab segment: one base page of slots.
  pub fn segment_tasks(&self) -> usize {
    derived!(
      (self.page_bytes / std::mem::size_of::<crate::task::TaskSlot>()).max(1),
      "base page / task slot size",
      ["page.base"]
    )
    .get()
  }
}

/// The shard count and the cores they pin to: every core of the fastest class the OS reports,
/// less one kept for the control shard and the OS (§4.3 "Shards = performance cores"; the design's
/// worked example: 6 Super cores give 5 shards); at least one shard.
pub fn shard_cores(profile: &MachineProfile) -> (Derived<u16>, Vec<u32>) {
  let best_level = profile
    .facts
    .cores
    .iter()
    .map(|c| c.level)
    .min()
    .unwrap_or(0);
  let mut cores: Vec<u32> = profile
    .facts
    .cores
    .iter()
    .filter(|c| c.level == best_level)
    .map(|c| c.id)
    .collect();
  if cores.len() > 1 {
    cores.remove(0);
  }
  let count = u16::try_from(cores.len()).unwrap_or(u16::MAX).max(1);
  (
    derived!(
      count,
      "cores of the fastest class minus one for control, at least one",
      ["cores.class", "cores.level"]
    ),
    cores,
  )
}

/// Measures the cost of one loop item on a simulated shard: spawn a trivial task, run it to
/// completion, reap it. The simulation driver has no OS resources, so this costs microseconds.
fn measured_item_cost_ns(config: &RuntimeConfig) -> u64 {
  let probe = RuntimeConfig {
    shards: 1,
    ..config.clone()
  };
  let Ok(mut sim) = crate::sim::SimRuntime::new(&probe, 0) else {
    return 1;
  };
  let shard = sim.shard_ids().first().copied();
  let Some(shard) = shard else { return 1 };
  let started = std::time::Instant::now();
  /// Shape: enough items to amortize the clock reads (two per batch) below one percent.
  const ITEMS: u64 = 4096;
  for _ in 0..ITEMS {
    let _ = sim.spawn_on(shard, async {});
    sim.run_until_idle();
  }
  u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX) / ITEMS
}

/// Little's law: the tasks in flight at a measured request rate and p99 service time.
pub fn admission_limit(requests_per_second: u64, p99_service_ns: u64) -> Derived<usize> {
  /// Format: nanoseconds per second.
  const NANOS_PER_SECOND: u128 = 1_000_000_000;
  let l = u128::from(requests_per_second) * u128::from(p99_service_ns) / NANOS_PER_SECOND;
  derived!(
    usize::try_from(l).unwrap_or(usize::MAX).max(1),
    "request rate × p99 service time (Little's law)",
    ["rt.request_rate", "rt.service_p99_ns"]
  )
}

/// Wires the single-producer rings between every ordered pair of shards; rings are leaked for
/// the process (bounded by shards²).
pub(crate) fn connect_pairs(shards: &mut [Box<ShardContext>], ids: &[u16]) -> Result<(), RtError> {
  let entries = shards
    .first()
    .map_or(1, |s| s.with_inner(|i| i.ring_entries()).unwrap_or(1));
  for a in 0..shards.len() {
    for b in 0..shards.len() {
      if a == b {
        continue;
      }
      let ring: NonNull<SpscRing<u64>> =
        NonNull::from(Box::leak(Box::new(SpscRing::new(entries)?)));
      shards[a].set_outbound(ids[b], ring);
      shards[b].set_inbound(ids[a], ring);
    }
  }
  Ok(())
}

/// The OS runtime: one thread per shard.
pub struct Runtime {
  threads: Vec<JoinHandle<Counters>>,
  ids: Vec<ShardId>,
  notes: Vec<String>,
}

impl std::fmt::Debug for Runtime {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Runtime")
      .field("shards", &self.ids)
      .finish()
  }
}

impl Runtime {
  /// Starts `config.shards` shard threads on the OS drivers.
  pub fn start(config: &RuntimeConfig) -> Result<Runtime, RtError> {
    let mut contexts = Vec::new();
    let mut ids = Vec::new();
    let mut notes = Vec::new();
    for _ in 0..config.shards {
      let (driver, driver_notes) =
        os_driver(u32::try_from(config.ring_entries).unwrap_or(u32::MAX))?;
      notes.extend(driver_notes);
      let ctx = ShardContext::new(config, driver)?;
      ids.push(ctx.id);
      contexts.push(ctx);
    }
    connect_pairs(&mut contexts, &ids)?;
    let mut threads = Vec::new();
    for (index, ctx) in contexts.into_iter().enumerate() {
      let core = if config.pin {
        config.cores.get(index).copied()
      } else {
        None
      };
      let thread = std::thread::Builder::new()
        .name(format!("slates-shard-{}", ctx.id))
        .spawn(move || {
          if let Some(core) = core {
            let _ = slates_machine::probes::pin_current_thread(core);
          }
          ctx.run();
          ctx.counters()
        })
        .map_err(|e| RtError::DriverRefused {
          call: "thread spawn",
          code: e.raw_os_error(),
        })?;
      threads.push(thread);
    }
    Ok(Runtime {
      threads,
      ids: ids.into_iter().map(ShardId).collect(),
      notes,
    })
  }

  /// The shard ids, in order.
  pub fn shard_ids(&self) -> &[ShardId] {
    &self.ids
  }

  /// The driver notes (which driver, which flags, which fallbacks).
  pub fn notes(&self) -> &[String] {
    &self.notes
  }

  /// Spawns a detached task on a shard from any thread.
  pub fn spawn_on<F: Future<Output = ()> + Send + 'static>(&self, shard: ShardId, future: F) {
    let request = Box::new(SpawnRequest::new(Box::pin(future), None));
    registry::send_foreign(shard.0, Msg::Spawn(request).into_word());
  }

  /// Tells a shard whether a client is active, which enables the idle spin before parking.
  pub fn set_active(&self, shard: ShardId, active: bool) {
    registry::send_foreign(shard.0, Msg::Active(active).into_word());
  }

  /// Requests a task's cancellation from any thread.
  pub fn cancel(&self, task: TaskId) {
    registry::send_foreign(task.0.shard(), Msg::Cancel(task.0).into_word());
  }

  /// Shuts every shard down (cancelling what runs) and joins the threads; returns the counters.
  pub fn shutdown(self) -> Vec<Counters> {
    for id in &self.ids {
      registry::send_foreign(id.0, Msg::Shutdown.into_word());
    }
    self
      .threads
      .into_iter()
      .map(|t| t.join().unwrap_or_default())
      .collect()
  }
}

/// One shard on the calling thread with the OS driver (tests, benches, the CLI's own work).
pub struct LocalRuntime {
  ctx: Box<ShardContext>,
  notes: Vec<String>,
}

impl std::fmt::Debug for LocalRuntime {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("LocalRuntime")
      .field("shard", &self.ctx.id)
      .finish()
  }
}

impl LocalRuntime {
  /// Builds the shard.
  pub fn new(config: &RuntimeConfig) -> Result<LocalRuntime, RtError> {
    let (driver, notes) = os_driver(u32::try_from(config.ring_entries).unwrap_or(u32::MAX))?;
    Ok(LocalRuntime {
      ctx: ShardContext::new(config, driver)?,
      notes,
    })
  }

  /// Builds the shard over a given driver (the simulation, or a test double).
  pub fn with_driver(
    config: &RuntimeConfig,
    driver: Box<dyn Driver>,
  ) -> Result<LocalRuntime, RtError> {
    Ok(LocalRuntime {
      ctx: ShardContext::new(config, driver)?,
      notes: Vec::new(),
    })
  }

  /// The shard.
  pub fn shard_id(&self) -> ShardId {
    ShardId(self.ctx.id)
  }

  /// The driver notes.
  pub fn notes(&self) -> &[String] {
    &self.notes
  }

  /// Spawns a joinable task.
  pub fn spawn<F: Future<Output = ()> + 'static>(&self, future: F) -> Result<TaskId, RtError> {
    self.ctx.spawn_local(crate::shard::boxed(future), None)
  }

  /// Runs until no task, timer or message is pending.
  pub fn run_until_idle(&self) {
    self.ctx.run_until_idle();
  }

  /// The shard's context, for counters and joins.
  pub fn context(&self) -> &ShardContext {
    &self.ctx
  }
}
