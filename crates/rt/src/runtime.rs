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
  /// The base page in bytes, for sizing the task slab's segments.
  pub page_bytes: usize,
}

impl RuntimeConfig {
  /// A configuration from the profile: tick, step budget and ring size from its derived
  /// constants; the task and timer limits from the caller's measurements.
  pub fn from_profile(
    profile: &MachineProfile,
    shards: u16,
    tasks_per_shard: usize,
    timers_per_shard: usize,
  ) -> Self {
    let d = profile.derived();
    let ring_entries = usize::try_from(d.ring_entries.get()).unwrap_or(usize::MAX);
    Self {
      shards,
      tasks_per_shard,
      timers_per_shard,
      ring_entries,
      step_budget_ns: d.task_step_budget_ns.get(),
      timer_tick_ns: d.timer_tick_ns.get(),
      batch: derived!(
        ring_entries,
        "one inbound ring per phase until the per-item cost is measured (§4.3)",
        ["rt.ring_entries"]
      )
      .get(),
      pin: true,
      page_bytes: usize::try_from(profile.facts.page.base).unwrap_or(1),
    }
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
      let pin = config.pin;
      let core = u32::try_from(index).unwrap_or(0);
      let thread = std::thread::Builder::new()
        .name(format!("slates-shard-{}", ctx.id))
        .spawn(move || {
          if pin {
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
