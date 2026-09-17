//! The runtime: the configuration derived from the profile, the OS runtime that owns one thread
//! per shard, the local runtime that runs one shard on the calling thread, and the shard-pair
//! ring wiring both share with the simulation (§4.3).

use std::future::Future;
use std::thread::JoinHandle;

use slates_machine::{Derived, MachineProfile, derived};
use slates_mem::SpscRing;

use crate::control::Control;
use crate::driver::{DriverSeed, Kick, os_driver};
use crate::error::RtError;
use crate::registry;
use crate::shard::{Counters, ShardContext, ShardId, ShardSeed, TaskId};
use crate::task::{AdmissionReceipt, SpawnRequest};

/// Submits a detached task to the shard `holder` names, from any thread and without a runtime handle
/// (a submitter that may outlive the runtime — an observation still pending while its daemon stops),
/// and returns the receipt of its admission; refused `ShardGone` when the slot is free or held by a
/// later registration, `ControlFull` when the holder's control channel is full.
pub fn submit_to_holder<F: Future<Output = ()> + Send + 'static>(
  holder: registry::SlotHolder,
  future: F,
) -> Result<AdmissionReceipt, RtError> {
  let (request, receipt) = SpawnRequest::with_receipt(Box::pin(future), None);
  registry::send_control_to_holder(holder, Control::Spawn(Box::new(request)))?;
  Ok(receipt)
}

/// Requests a task's cancellation from any thread without a runtime handle (the message
/// [`Runtime::cancel`] sends); refused when the task's shard is gone or its control channel is full.
/// A task that already ended is a stale word the shard ignores.
pub fn cancel_task(task: TaskId) -> Result<(), RtError> {
  registry::send_control(task.0.shard(), Control::Cancel(task.0))
}

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

/// Wires the single-producer rings between every ordered pair of seeds; rings are leaked for
/// the process (bounded by shards²).
pub(crate) fn connect_pairs(seeds: &mut [ShardSeed]) -> Result<(), RtError> {
  let entries = seeds.first().map_or(1, |s| s.config.ring_entries);
  let ids: Vec<u16> = seeds.iter().map(|s| s.id).collect();
  for a in 0..seeds.len() {
    for b in 0..seeds.len() {
      if a == b {
        continue;
      }
      // The ring is owned by the source shard's registry entry and lent to both contexts as
      // `&'static`: the entry outlives every borrower (its slot is given back only after every
      // thread of the runtime joined, and the entry itself is dropped only by the slot's next
      // registration), so the lifetime is the slot protocol's promise, not a leak.
      let ring: &'static SpscRing = registry::lend_pair_ring(ids[a], SpscRing::new(entries)?)
        .ok_or(RtError::ShardGone { shard: ids[a] })?;
      seeds[a].set_outbound(ids[b], ring);
      seeds[b].set_inbound(ring);
    }
  }
  Ok(())
}

/// The slot's kick for an OS driver: its descriptor, owned by the slot (Unix), or none (Windows: the
/// completion port is the driver's).
#[cfg(any(target_os = "macos", target_os = "freebsd"))]
fn register_kick(fd: Option<std::os::fd::OwnedFd>) -> registry::RegisterKick {
  match fd {
    Some(fd) => registry::RegisterKick::Descriptor(fd, Kick::Kqueue),
    None => registry::RegisterKick::Kick(Kick::None),
  }
}

#[cfg(target_os = "linux")]
fn register_kick(fd: Option<std::os::fd::OwnedFd>) -> registry::RegisterKick {
  match fd {
    Some(fd) => registry::RegisterKick::Descriptor(fd, Kick::Eventfd),
    None => registry::RegisterKick::Kick(Kick::None),
  }
}

#[cfg(not(unix))]
fn register_kick(_fd: Option<()>) -> registry::RegisterKick {
  registry::RegisterKick::Kick(Kick::None)
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
    let mut seeds = Vec::new();
    let mut notes = Vec::new();
    for _ in 0..config.shards {
      let prepared = os_driver(u32::try_from(config.ring_entries).unwrap_or(u32::MAX))?;
      notes.extend(prepared.notes);
      seeds.push(ShardSeed::register(
        config,
        prepared.seed,
        register_kick(prepared.kick_fd),
      )?);
    }
    connect_pairs(&mut seeds)?;
    let ids: Vec<ShardId> = seeds.iter().map(|s| ShardId(s.id)).collect();
    let mut threads = Vec::new();
    for (index, seed) in seeds.into_iter().enumerate() {
      let core = if config.pin {
        config.cores.get(index).copied()
      } else {
        None
      };
      let thread = std::thread::Builder::new()
        .name(format!("slates-shard-{}", seed.id))
        .spawn(move || {
          if let Some(core) = core {
            let _ = slates_machine::probes::pin_current_thread(core);
          }
          let Ok(ctx) = ShardContext::build(seed) else {
            return Counters::default();
          };
          let id = ctx.id;
          ctx.run();
          let counters = ctx.counters();
          // The loop has returned on this thread: the current-context cell is cleared, the arena is
          // empty, and nothing foreign dereferences a context — so this thread, which built it,
          // frees it. The slot's entry stays (retired by `shutdown`'s `unregister` after the join).
          registry::reclaim_context(id);
          counters
        })
        .map_err(|e| RtError::DriverRefused {
          call: "thread spawn",
          code: e.raw_os_error(),
        })?;
      threads.push(thread);
    }
    Ok(Runtime {
      threads,
      ids,
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

  /// Spawns a detached task on a shard from any thread; refused when the shard's control
  /// channel is full (its admission limit) or the shard is gone. `Ok` is a **submission**: the request
  /// is in the shard's control channel, and whether the shard admits it is answered only to a receipt
  /// ([`Self::spawn_on_with_receipt`]).
  pub fn spawn_on<F: Future<Output = ()> + Send + 'static>(
    &self,
    shard: ShardId,
    future: F,
  ) -> Result<(), RtError> {
    let request = Box::new(SpawnRequest::new(Box::pin(future), None));
    registry::send_control(shard.0, Control::Spawn(request))
  }

  /// Spawns a detached task on a shard from any thread and returns the receipt of its admission
  /// ([`AdmissionReceipt`]): refused here when the shard's control channel is full or the shard is gone;
  /// otherwise the receipt reports, once the shard drains the request, whether the task was admitted
  /// (and as which task), refused (the arena full), or terminated unadmitted (the shard shutting down).
  pub fn spawn_on_with_receipt<F: Future<Output = ()> + Send + 'static>(
    &self,
    shard: ShardId,
    future: F,
  ) -> Result<AdmissionReceipt, RtError> {
    let (request, receipt) = SpawnRequest::with_receipt(Box::pin(future), None);
    registry::send_control(shard.0, Control::Spawn(Box::new(request)))?;
    Ok(receipt)
  }

  /// The registration holding `shard`'s slot, for a submitter that must reach this runtime's shard and
  /// never a later holder of its slot ([`submit_to_holder`]); `None` when the shard is gone.
  pub fn holder_of(&self, shard: ShardId) -> Option<registry::SlotHolder> {
    registry::holder_of(shard.0)
  }

  /// Tells a shard whether a client is active, which enables the idle spin before parking.
  pub fn set_active(&self, shard: ShardId, active: bool) -> Result<(), RtError> {
    registry::send_control(shard.0, Control::Active(active))
  }

  /// Requests a task's cancellation from any thread.
  pub fn cancel(&self, task: TaskId) -> Result<(), RtError> {
    registry::send_control(task.0.shard(), Control::Cancel(task.0))
  }

  /// Shuts every shard down (cancelling what runs) and joins the threads; returns the counters.
  pub fn shutdown(self) -> Vec<Counters> {
    for id in &self.ids {
      // A full control channel refuses the message; the shard drains its channel as it runs, so the
      // send is retried until it lands or the shard is gone. Before 2026-09-17 the refusal was
      // dropped, and the join below then waited for a shutdown the shard never received: a runtime
      // shut down while a shard's channel was full (a burst of submissions behind a long poll) hung
      // for good (`tests/admission.rs`). The wait is bounded by the shard's next drain — a shard that
      // never drains again would hang the join just the same.
      while let Err(RtError::ControlFull { .. }) = registry::send_control(id.0, Control::Shutdown) {
        std::thread::yield_now();
      }
    }
    let counters: Vec<Counters> = self
      .threads
      .into_iter()
      .map(|t| t.join().unwrap_or_default())
      .collect();
    // Every thread has ended: give every slot back (the kick descriptors close, the slots are
    // reusable). After every join, never before — a shard's pair rings are lent to its peers.
    for id in &self.ids {
      registry::unregister(id.0);
    }
    counters
  }
}

/// One shard on the calling thread with the OS driver (tests, benches, the CLI's own work).
pub struct LocalRuntime {
  ctx: &'static ShardContext,
  notes: Vec<String>,
}

impl std::fmt::Debug for LocalRuntime {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("LocalRuntime")
      .field("shard", &self.ctx.id)
      .finish()
  }
}

impl Drop for LocalRuntime {
  /// The shard ran on this thread and its loop has returned by the time the value drops (every
  /// `run_until_*` returns before), so its context is freed and its slot given back here: the kick
  /// descriptor closes and the slot is reusable by the next runtime.
  fn drop(&mut self) {
    let id = self.ctx.id;
    registry::note_arena_generation(id, self.ctx.arena_generation_high());
    // The context was built and run on this thread (the handle is `!Send`), and every `run_until_*`
    // returned before this drop: free it here, then give the slot back.
    registry::reclaim_context(id);
    registry::unregister(id);
  }
}

impl LocalRuntime {
  /// Builds the shard.
  pub fn new(config: &RuntimeConfig) -> Result<LocalRuntime, RtError> {
    let prepared = os_driver(u32::try_from(config.ring_entries).unwrap_or(u32::MAX))?;
    Ok(LocalRuntime {
      ctx: ShardContext::build(ShardSeed::register(
        config,
        prepared.seed,
        register_kick(prepared.kick_fd),
      )?)?,
      notes: prepared.notes,
    })
  }

  /// Builds the shard over a given driver (the simulation, or a test double).
  pub fn with_driver(
    config: &RuntimeConfig,
    driver: DriverSeed,
    kick: Kick,
  ) -> Result<LocalRuntime, RtError> {
    Ok(LocalRuntime {
      ctx: ShardContext::build(ShardSeed::register(
        config,
        driver,
        registry::RegisterKick::Kick(kick),
      )?)?,
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
  pub fn context(&self) -> &'static ShardContext {
    self.ctx
  }
}
