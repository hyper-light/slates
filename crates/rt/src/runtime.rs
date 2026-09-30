//! The runtime: the configuration derived from the profile, the OS runtime that owns one thread
//! per shard, the local runtime that runs one shard on the calling thread, and the shard-pair
//! ring wiring both share with the simulation (§4.3).

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use slates_machine::placement::Placement;
use slates_machine::probes::Pinning;
use slates_machine::{Derived, MachineProfile, derived};
use slates_mem::SpscRing;

use crate::control::Control;
use crate::driver::{DriverSeed, Kick, Prepared, os_driver};
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
  /// The wake estimate each shard refines from its own kicked parks (§4.1, §4.3): the boot probe's mean
  /// as the prior, its window, and the idle window's multiple of it. While a shard tracks, its step
  /// quantum and idle spin follow the estimate rather than the fixed `step_budget_ns` and `spin_ns`;
  /// `None` is a hand-written configuration with no measured prior to track (a test harness's fixed
  /// quanta, an internal helper runtime), which keeps its fixed values.
  pub wake_tracking: Option<WakeTracking>,
}

/// What a shard needs to refine its wake estimate after boot (§4.1: the boot probe's mean converges
/// slowly under a heavy tail — a virtual machine's needs 38,000–75,000 wakes where the probe's budget buys
/// a few thousand — so the estimate a shard sizes its spin and its quantum by keeps learning from the
/// wakes it actually pays).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WakeTracking {
  /// The boot probe's mean wake, nanoseconds: the estimate's seed.
  pub prior_ns: u64,
  /// The estimate's weighting shift, about `2^shift` wakes ([`slates_machine::wake::WakeLatency::estimate_shift`]).
  pub shift: u32,
  /// The idle spin window as a multiple of the estimate (1 for the runtime's own spin-then-park; the
  /// daemon's idle window sets its ratio).
  pub idle_ratio: u64,
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
      // Fixed only to cores the process owns ([`shard_cores`]); none means the OS places the shards.
      pin: !cores.is_empty(),
      cores,
      page_bytes: usize::try_from(profile.facts.page.base).unwrap_or(1),
      spin_ns: d.spin_before_park_ns.get(),
      wake_tracking: Some(WakeTracking {
        prior_ns: d.spin_before_park_ns.get(),
        shift: profile.wake.estimate_shift(),
        idle_ratio: 1,
      }),
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

/// The shard count and the cores they are fixed to, by the machine's placement rule
/// ([`slates_machine::placement`], §4.3 "Shards = performance cores"): every core of the fastest class the
/// process may run on that its CPU budget runs at once, less one kept for control and the OS, at least one
/// (the design's worked example: six Super cores give five shards) — fixed to those cores only when the
/// process owns them, and left to the OS (no cores) when it shares them in time under a CPU quota below its
/// cpuset (docs/bugs/2026-09-29-every-daemon-under-a-cpu-quota-pinned-its-shard-to-cpu-1.md).
pub fn shard_cores(profile: &MachineProfile) -> (Derived<u16>, Vec<u32>) {
  let placement = Placement::of(&profile.facts.cores, profile.facts.cpu_budget);
  let cores = placement
    .fixed
    .map(|fixed| fixed.shards)
    .unwrap_or_default();
  (placement.shards, cores)
}

/// Logs, once per process, a shard whose fixed core the OS refused where the OS pins (Linux, Windows): the
/// placement names only cores the process may run on, so a refusal means its mask changed after the facts
/// were read — a fault to show, not to swallow; the shard then runs where the scheduler places it and its
/// counters say so ([`Counters::pin_refused`]). macOS takes affinity as a hint that Apple silicon refuses by
/// design, so there a refusal is counted, not logged.
fn log_pin_refused(shard: u16, core: u32) {
  static LOGGED: AtomicBool = AtomicBool::new(false);
  if cfg!(any(target_os = "linux", windows)) && !LOGGED.swap(true, Ordering::AcqRel) {
    eprintln!(
      "slates-rt: shard {shard} could not be fixed to CPU {core} (the OS refused); it runs where the \
       scheduler places it (first occurrence; each shard counts its own)"
    );
  }
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

/// The slot's kick for an OS driver: its descriptor (Unix) or its completion port (Windows), owned by
/// the slot and closed when the slot retires the registration.
#[cfg(any(target_os = "macos", target_os = "freebsd"))]
pub(crate) fn register_kick(fd: Option<std::os::fd::OwnedFd>) -> registry::RegisterKick {
  match fd {
    Some(fd) => registry::RegisterKick::Descriptor(fd, Kick::Kqueue),
    None => registry::RegisterKick::Kick(Kick::None),
  }
}

#[cfg(target_os = "linux")]
pub(crate) fn register_kick(fd: Option<std::os::fd::OwnedFd>) -> registry::RegisterKick {
  match fd {
    Some(fd) => registry::RegisterKick::Descriptor(fd, Kick::Eventfd),
    None => registry::RegisterKick::Kick(Kick::None),
  }
}

#[cfg(windows)]
pub(crate) fn register_kick(port: Option<crate::iocp::Port>) -> registry::RegisterKick {
  match port {
    Some(port) => registry::RegisterKick::Port(port),
    None => registry::RegisterKick::Kick(Kick::None),
  }
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn register_kick(_fd: Option<()>) -> registry::RegisterKick {
  registry::RegisterKick::Kick(Kick::None)
}

/// One shard's worker: its id and the thread running it, whose result is the shard's counters or why it
/// could not run.
struct Worker {
  id: u16,
  thread: JoinHandle<Result<Counters, RtError>>,
}

/// The OS runtime: one thread per shard. It owns its workers from start to a terminal state (AUD-29-12):
/// `start` is transactional — every shard's context is built and acknowledged before it returns, and any
/// failure stops and joins the workers that did start and gives every registry slot back before the typed
/// refusal returns — and a runtime dropped without [`Runtime::shutdown`] stops and joins its workers in
/// `Drop`. Until 2026-09-30 a worker whose context failed to build returned default counters under a
/// successful start, a failure part-way through `start` detached the threads already spawned and kept
/// their slots, and dropping the value detached every worker.
pub struct Runtime {
  workers: Vec<Worker>,
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

impl Drop for Runtime {
  /// A runtime dropped without [`Runtime::shutdown`] stops, cancels and joins its workers and gives their
  /// slots back, exactly as `shutdown` would; a worker's failure is reported on the error stream, the one
  /// place a drop can put it.
  fn drop(&mut self) {
    if self.workers.is_empty() {
      return;
    }
    if let Err(error) = stop(std::mem::take(&mut self.workers)) {
      eprintln!("slates-rt: a runtime dropped without shutdown: {error}");
    }
  }
}

/// A shard's worker body: builds the context on this thread (pinned first, so the arena and rings are
/// first touched on the shard's own core), acknowledges the build — or its refusal — on `ready`, then runs
/// the loop and frees the context. The acknowledgement's sender is dropped with it, so a worker that ends
/// before acknowledging is seen as gone rather than awaited.
fn run_worker(
  seed: ShardSeed,
  core: Option<u32>,
  ready: std::sync::mpsc::SyncSender<(u16, Result<(), RtError>)>,
) -> Result<Counters, RtError> {
  let id = seed.id;
  let pinned = core.map(|core| (core, slates_machine::probes::pin_current_thread(core)));
  let ctx = match ShardContext::build(seed) {
    Ok(ctx) => ctx,
    Err(error) => {
      let _ = ready.send((id, Err(error.clone())));
      return Err(error);
    }
  };
  let _ = ready.send((id, Ok(())));
  drop(ready);
  if let Some((core, Pinning::Refused)) = pinned {
    ctx.note_pin_refused();
    log_pin_refused(ctx.id, core);
  }
  ctx.run();
  let counters = ctx.counters();
  // The loop has returned on this thread: the current-context cell is cleared, the arena is
  // empty, and nothing foreign dereferences a context — so this thread, which built it,
  // frees it. The slot's entry stays (retired by `stop`'s `unregister` after the join).
  registry::reclaim_context(id);
  Ok(counters)
}

/// Stops every worker (a shutdown message, retried while the shard's control channel is full), joins them
/// all, and then gives every slot back — after every join, never before, since a shard's pair rings are lent
/// to its peers. The counters in shard order, or the first worker's failure (every worker is still joined
/// and every slot still given back).
fn stop(workers: Vec<Worker>) -> Result<Vec<Counters>, RtError> {
  for worker in &workers {
    // A full control channel refuses the message; the shard drains its channel as it runs, so the send is
    // retried until it lands or the shard is gone. Before 2026-09-17 the refusal was dropped, and the join
    // then waited for a shutdown the shard never received (`tests/admission.rs`).
    while let Err(RtError::ControlFull { .. }) =
      registry::send_control(worker.id, Control::Shutdown)
    {
      std::thread::yield_now();
    }
  }
  let ids: Vec<u16> = workers.iter().map(|worker| worker.id).collect();
  let mut counters = Vec::with_capacity(workers.len());
  let mut failure = None;
  for worker in workers {
    let id = worker.id;
    match worker.thread.join() {
      Ok(Ok(shard)) => counters.push(shard),
      Ok(Err(error)) => {
        failure.get_or_insert(error);
      }
      Err(_) => {
        failure.get_or_insert(RtError::WorkerFailed { shard: id });
      }
    }
  }
  for id in ids {
    registry::unregister(id);
  }
  match failure {
    Some(error) => Err(error),
    None => Ok(counters),
  }
}

/// Gives back the slots of seeds that never became workers.
fn release_seeds(seeds: Vec<ShardSeed>) {
  let ids: Vec<u16> = seeds.iter().map(|seed| seed.id).collect();
  drop(seeds);
  for id in ids {
    registry::unregister(id);
  }
}

/// Registers `config.shards` seeds over drivers `prepare` makes, pairing their rings; on any refusal every
/// slot claimed so far is given back.
fn register_seeds(
  config: &RuntimeConfig,
  prepare: &mut dyn FnMut() -> Result<Prepared, RtError>,
  notes: &mut Vec<String>,
) -> Result<Vec<ShardSeed>, RtError> {
  let mut seeds = Vec::new();
  for _ in 0..config.shards {
    let registered = prepare().and_then(|prepared| {
      notes.extend(prepared.notes);
      ShardSeed::register(config, prepared.seed, register_kick(prepared.kick_fd))
    });
    match registered {
      Ok(seed) => seeds.push(seed),
      Err(error) => {
        release_seeds(seeds);
        return Err(error);
      }
    }
  }
  if let Err(error) = connect_pairs(&mut seeds) {
    release_seeds(seeds);
    return Err(error);
  }
  Ok(seeds)
}

impl Runtime {
  /// Starts `config.shards` shard threads on the OS drivers.
  pub fn start(config: &RuntimeConfig) -> Result<Runtime, RtError> {
    let entries = u32::try_from(config.ring_entries).unwrap_or(u32::MAX);
    Self::start_with(config, &mut || os_driver(entries))
  }

  /// Starts `config.shards` shard threads over the drivers `prepare` makes, one call per shard: the OS
  /// drivers ([`Runtime::start`]) or a test double, as [`LocalRuntime::with_driver`] takes one. Returns once
  /// every shard's context is built on its own thread; any refusal — a driver `prepare` refuses, a
  /// registration, a thread the OS will not spawn, a context that fails to build — stops and joins the
  /// workers already started, gives every slot back, and returns that refusal.
  pub fn start_with(
    config: &RuntimeConfig,
    prepare: &mut dyn FnMut() -> Result<Prepared, RtError>,
  ) -> Result<Runtime, RtError> {
    let mut notes = Vec::new();
    let seeds = register_seeds(config, prepare, &mut notes)?;
    let ids: Vec<ShardId> = seeds.iter().map(|s| ShardId(s.id)).collect();
    let (ready, acknowledged) = std::sync::mpsc::sync_channel(seeds.len().max(1));
    let mut workers = Vec::with_capacity(seeds.len());
    let mut seeds = seeds.into_iter().enumerate();
    while let Some((index, seed)) = seeds.next() {
      let core = if config.pin {
        config.cores.get(index).copied()
      } else {
        None
      };
      let id = seed.id;
      let ready = ready.clone();
      let spawned = std::thread::Builder::new()
        .name(format!("slates-shard-{id}"))
        .spawn(move || run_worker(seed, core, ready));
      match spawned {
        Ok(thread) => workers.push(Worker { id, thread }),
        Err(error) => {
          // The seed moved into the refused closure and is dropped with it; its slot, and the rest, go back.
          registry::unregister(id);
          release_seeds(seeds.map(|(_, seed)| seed).collect());
          let refusal = RtError::DriverRefused {
            call: "thread spawn",
            code: error.raw_os_error(),
          };
          roll_back(workers, &refusal);
          return Err(refusal);
        }
      }
    }
    drop(ready);
    if let Err(error) = await_readiness(&acknowledged, &workers) {
      roll_back(workers, &error);
      return Err(error);
    }
    Ok(Runtime {
      workers,
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

  /// Requests a task's cancellation from any thread.
  pub fn cancel(&self, task: TaskId) -> Result<(), RtError> {
    registry::send_control(task.0.shard(), Control::Cancel(task.0))
  }

  /// Shuts every shard down (cancelling what runs), joins every worker and gives every slot back; the
  /// counters in shard order, or the first worker's failure (typed; every worker is still joined and every
  /// slot still given back).
  pub fn shutdown(mut self) -> Result<Vec<Counters>, RtError> {
    stop(std::mem::take(&mut self.workers))
  }
}

/// Stops and joins the workers of a start that failed with `refusal`, giving every slot back. A failure of
/// the rollback other than the refusal itself (a sibling that panicked meanwhile) is reported on the error
/// stream, since the start returns the refusal that caused it.
fn roll_back(workers: Vec<Worker>, refusal: &RtError) {
  if let Err(error) = stop(workers)
    && error != *refusal
  {
    eprintln!("slates-rt: rolling back a start refused ({refusal}) also found: {error}");
  }
}

/// Waits for every worker to acknowledge its build: the first refusal, or `WorkerFailed` for a worker that
/// ended without acknowledging (its sender dropped with it), so no failed shard is ever advertised.
fn await_readiness(
  acknowledged: &std::sync::mpsc::Receiver<(u16, Result<(), RtError>)>,
  workers: &[Worker],
) -> Result<(), RtError> {
  for _ in workers {
    match acknowledged.recv() {
      Ok((_, Ok(()))) => {}
      Ok((_, Err(error))) => return Err(error),
      Err(_) => {
        let silent = workers
          .iter()
          .find(|worker| worker.thread.is_finished())
          .map_or(u16::MAX, |worker| worker.id);
        return Err(RtError::WorkerFailed { shard: silent });
      }
    }
  }
  Ok(())
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
