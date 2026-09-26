//! The shard: one thread, one task arena, one run queue, one timing wheel, one driver, and the
//! loop that ties them (§4.3, "Loop").
//!
//! Each iteration: drain the control channel and the inbound rings (spawns, cancels, shutdown,
//! wakes) and the driver's completions into the run queue; expire timers; run ready tasks to
//! their next await, at most a batch of them; then, if nothing is ready, spin for the configured
//! window while a client is active, and park in the driver until a kick, a completion or the
//! next deadline. Cancellation is a message that guarantees a terminal completion: the future is
//! dropped at the next poll boundary, the task's children are cancelled and joined, and whoever
//! joins it sees `Cancelled`. The watchdog counts polls that exceed the step quantum — the expected wake
//! (a step longer than a peer's wake starves the shard), which the shard refines from the kicked parks it
//! pays (§4.3, the online wake estimate) — and attributes each to its task (past the quantum on the CPU,
//! or waiting inside a call) or to the host (runnable and off the CPU), so a poll the operating system
//! preempted is not counted as the task's bug ([`crate::attribution`]).
//!
//! Ownership: a context is built on its own thread from a [`ShardSeed`] and leaked, so every
//! reference to it is `&'static` and the thread-local the wakers route through holds a plain
//! reference; the mutable state sits in a `RefCell`, so a task's poll runs with no borrow held
//! and a waker, spawn or join from inside the poll takes its own short borrow; a nested borrow
//! is refused and counted, never undefined. There is no unsafe code in this module.

use std::cell::{Cell, RefCell};
use std::pin::Pin;
use std::sync::mpsc::Receiver;
use std::task::{Context, Poll};

use slates_mem::{Encoded, Handle, Slab, SpscRing};

use slates_machine::wake::WakeEstimate;

use crate::attribution::{self, Attribution, Tracker};
use crate::control::Control;
use crate::driver::{Completion, Driver, DriverKind, DriverSeed, Kick};
use crate::error::RtError;
use crate::parking::{Parked, Woken};
use crate::queue::LocalQueue;
use crate::registry::{self, Entry, MAX_SHARDS};
use crate::runtime::RuntimeConfig;
use crate::task::{Admission, BoxedFuture, NO_LINK, Outcome, SpawnRequest, State, TaskSlot};
use crate::timer::Wheel;
use crate::waker::waker_for;

/// A shard's process-wide id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShardId(pub u16);

/// A task's id: its packed word (shard, slot, generation).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TaskId(pub Encoded);

impl TaskId {
  /// The owning shard.
  pub fn shard(&self) -> ShardId {
    ShardId(self.0.shard())
  }
}

/// What one loop iteration did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StepOutcome {
  /// Whether any message, timer or task was processed.
  pub did_work: bool,
  /// The next timer deadline, in the driver's nanoseconds.
  pub next_deadline_ns: Option<u64>,
  /// Whether the shard finished its shutdown and left the loop.
  pub exit: bool,
}

/// The shard's counters (the tripwires of GAPS §7 read them).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
  /// Loop iterations.
  pub steps: u64,
  /// Task polls.
  pub polls: u64,
  /// Polls past the step quantum that were their task's own — the bounded-work rule's bug signal (§4.3):
  /// past it on the CPU, or waiting inside a call past it ([`Counters::blocked_steps`]).
  pub long_steps: u64,
  /// Of `long_steps`, the polls that waited inside a call: blocked in the kernel (a voluntary context
  /// switch, counted per thread on Linux) or yielded by the runtime to a full peer ring.
  pub blocked_steps: u64,
  /// Polls past the quantum by the wall clock that the host held: within it on the CPU and never waiting
  /// inside a call — preempted by the operating system, or the virtual CPU stolen by the hypervisor.
  pub preempted_steps: u64,
  /// Polls past the quantum by the wall clock that could not be attributed: no window open yet, a window
  /// whose earlier work leaves the poll's CPU undecided, off the CPU where the platform cannot tell a
  /// block from a preemption (macOS), or no per-thread clock (Windows). With `long_steps` and
  /// `preempted_steps`, every poll past the quantum by the wall clock.
  pub unattributed_steps: u64,
  /// The longest poll by the wall clock, in nanoseconds (how long the shard was held, whoever held it).
  pub longest_step_ns: u64,
  /// Kicked parks whose kick-to-running latency fed the online wake estimate (a non-vacuity count).
  pub wake_samples: u64,
  /// Kick stamps that predated their park's announcement (a sender that saw an earlier park), dropped.
  pub wake_stale: u64,
  /// Kicks that found the shard not yet asleep, so no wake was measured: stamped while its park was being
  /// set up, or (Linux, where the thread's voluntary switches are counted) answered by a wait that never
  /// slept.
  pub wake_unslept: u64,
  /// The online wake estimate, nanoseconds (the step quantum while the shard tracks one).
  pub wake_cost_ns: u64,
  /// Tasks admitted.
  pub spawns: u64,
  /// Tasks whose future returned.
  pub completed: u64,
  /// Tasks whose future was dropped.
  pub cancelled: u64,
  /// Wakes that arrived over a shard-pair ring.
  pub wakes_pair: u64,
  /// Wakes that arrived over the foreign ring.
  pub wakes_foreign: u64,
  /// Control messages received.
  pub controls: u64,
  /// Wakes whose generation no longer matched.
  pub stale_wakes: u64,
  /// Timers fired.
  pub timers_fired: u64,
  /// Driver waits.
  pub waits: u64,
  /// Driver completions delivered.
  pub completions: u64,
  /// Times the driver was lost.
  pub driver_lost: u64,
  /// Driver errors other than loss.
  pub driver_errors: u64,
  /// Refused nested borrows (a bug signal).
  pub nested_borrows: u64,
  /// Admissions refused because the arena was full.
  pub admission_refused: u64,
  /// Spawn requests drained after shutdown began and refused unadmitted (their receipts answered
  /// `Terminated`, their futures dropped): a shard shutting down admits nothing new, so its arena
  /// drains monotonically and the loop's exit is bounded by the tasks it already holds.
  pub refused_at_shutdown: u64,
  /// Idle spins that ended with work arriving.
  pub spin_hits: u64,
  /// Pollers woken because their ring had something (client command rings).
  pub poller_wakes: u64,
  /// Idle spins that ran out and parked.
  pub spin_misses: u64,
  /// Idle spins ended by a timer falling due.
  pub spin_deadlines: u64,
  /// The measured scheduler overrun, nanoseconds ([`ShardContext::scheduler_overrun_ns`]).
  pub scheduler_overrun_ns: u64,
}

/// Shape: the exponential-forgetting shift of the measured scheduler overrun — a wait's overrun that
/// is not renewed decays by `1/8` (`>> 3`) at each later wait, so a starvation spike lingers about
/// eight waits (under a second of a fleet shard's timer-paced waits on a quiet host) and then
/// recovers as the load lifts, rather than pinning detection windows open for good.
const OVERRUN_FORGET_SHIFT: u32 = 3;

/// What a shard is built from: everything is `Send`, so the runtime assembles seeds on its own
/// thread and each shard thread builds its context from one.
pub struct ShardSeed {
  /// The registered id.
  pub id: u16,
  /// Builds the driver on the shard's thread.
  pub driver: DriverSeed,
  /// The kick the driver answers to.
  pub kick: Kick,
  /// The control channel's receiving end.
  pub control: Receiver<Control>,
  /// The configuration.
  pub config: RuntimeConfig,
  outbound: Vec<Option<&'static SpscRing>>,
  inbound: Vec<&'static SpscRing>,
}

impl std::fmt::Debug for ShardSeed {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ShardSeed").field("id", &self.id).finish()
  }
}

impl ShardSeed {
  /// Registers a shard over `driver` in the process registry and returns its seed.
  pub fn register(
    config: &RuntimeConfig,
    driver: DriverSeed,
    kick: registry::RegisterKick,
  ) -> Result<ShardSeed, RtError> {
    let (id, control) = registry::register(config.ring_entries, config.tasks_per_shard, kick)?;
    let kick = registry::with_entry(id, |entry| entry.kick).unwrap_or(Kick::None);
    Ok(ShardSeed {
      id,
      driver,
      kick,
      control,
      config: config.clone(),
      outbound: vec![None; MAX_SHARDS],
      inbound: Vec::new(),
    })
  }

  /// Connects the single-producer ring this shard sends on to `target`.
  pub(crate) fn set_outbound(&mut self, target: u16, ring: &'static SpscRing) {
    if let Some(slot) = self.outbound.get_mut(usize::from(target)) {
      *slot = Some(ring);
    }
  }

  /// Connects the single-producer ring this shard receives on.
  pub(crate) fn set_inbound(&mut self, ring: &'static SpscRing) {
    self.inbound.push(ring);
  }
}

/// A poller: a task that owns an inbound ring the loop cannot see (a client's command ring
/// in shared memory, §4.3 "drain inbound rings (client command rings, ...)"). The loop asks
/// `ready` each step and during the idle spin, and wakes the task when it says so; the task
/// yields with [`crate::futures::idle`] and is polled again only when woken. `ready` may consume
/// its signal when asked (the daemon's doorbell flag does), so every asker wakes the task on a
/// yes — the step and the spin both go through `wake_ready_pollers`, never a bare `ready()`.
pub struct Poller {
  slot: u32,
  generation: u32,
  ready: Box<dyn Fn() -> bool>,
}

impl std::fmt::Debug for Poller {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Poller").field("slot", &self.slot).finish()
  }
}

/// The mutable state behind the borrow.
pub struct ShardInner {
  arena: Slab<TaskSlot>,
  timers: Wheel,
  driver: Box<dyn Driver>,
  control: Receiver<Control>,
  config: RuntimeConfig,
  counters: Counters,
  shutting_down: bool,
  fired: Vec<u64>,
  completions: Vec<Completion>,
  pollers: Vec<Poller>,
}

impl ShardInner {
  /// Entries per inbound ring.
  pub fn ring_entries(&self) -> usize {
    self.config.ring_entries
  }
}

/// The shard's context: what its thread, its wakers and its tasks see.
pub struct ShardContext {
  /// The shard id.
  pub id: u16,
  /// The local run queue.
  pub local: LocalQueue,
  outbound: Vec<Option<&'static SpscRing>>,
  inbound: Vec<&'static SpscRing>,
  current_task: Cell<Option<u32>>,
  exited: Cell<bool>,
  active: Cell<bool>,
  pair_full_events: Cell<u64>,
  nested_borrows: Cell<u64>,
  /// The absolute deadline the shard last waited for — parked in its driver, or idle-spinning — and
  /// has not stepped since; the next [`step`](Self::step) takes it and measures how late it runs
  /// against it ([`Self::scheduler_overrun_ns`]). `None` while running, or after an unbounded wait.
  waited_for_ns: Cell<Option<u64>>,
  /// The exponentially-forgetting maximum of how late a step ran after the wait before it, nanoseconds
  /// (the measurement [`Self::scheduler_overrun_ns`] describes).
  scheduler_overrun_ns: Cell<u64>,
  /// The online wake estimate (§4.1, §4.3): seeded with the boot probe's mean and fed each kicked park's
  /// kick-to-running latency ([`Self::note_wake`]); the step quantum and the idle spin follow it. `None`
  /// when the configuration carries no measured prior to track ([`crate::runtime::WakeTracking`]).
  wake: Cell<Option<WakeEstimate>>,
  /// The idle spin window as a multiple of the wake estimate, while tracking.
  idle_ratio: u64,
  /// The configured step budget and idle spin, the quantum and the spin when not tracking.
  fixed_quantum_ns: u64,
  fixed_spin_ns: u64,
  /// Whether the driver's clock is real time: the simulation's is not the kicker's clock nor the thread's
  /// CPU clock, so a simulated shard neither measures its wakes nor judges polls by CPU time (it stays
  /// deterministic).
  real_time: bool,
  /// Who held each long poll: the attribution windows and when the shard reads the thread's account
  /// ([`crate::attribution::Tracker`]). Unused on a simulated shard, whose polls are the task's alone.
  attribution: Cell<Tracker>,
  entry: Option<&'static Entry>,
  inner: RefCell<ShardInner>,
  /// Declared after `inner`, so it drops after the tasks: a task may hold a kept value.
  kept: Kept,
}

/// Values a shard owns for its life and drops with its context, after its tasks (§4.3: a per-shard
/// singleton — a socket's demultiplexer, a fleet identity — is `&'static` to the shard's tasks, and
/// this is what makes that `'static` a promise the context's end keeps rather than a leak, as
/// `Box::leak` was before 2026-09-14). Shape: filled at boot only (one entry per socket or identity a
/// shard serves), never per operation, so it needs no bound of its own; dropped last-in-first-out so
/// a value kept later (a demultiplexer over an identity) goes before what it borrows.
#[derive(Default)]
struct Kept(RefCell<Vec<Box<dyn std::any::Any>>>);

impl Drop for Kept {
  fn drop(&mut self) {
    let mut values = self.0.borrow_mut();
    while let Some(value) = values.pop() {
      drop(value);
    }
  }
}

impl std::fmt::Debug for ShardContext {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ShardContext")
      .field("id", &self.id)
      .finish()
  }
}

impl ShardContext {
  /// Builds the context from its seed on the calling thread (which builds the driver too). The
  /// context is handed out as `&'static` — every task, waker-side path and the thread's current-context
  /// cell name it that way — but it is not leaked: its slot keeps the box's raw pointer and the same
  /// thread frees it once its loop has returned (`registry::reclaim_context`; the multi-thread
  /// worker after `run`, `LocalRuntime` and `SimRuntime` in `Drop`). No other thread ever
  /// dereferences a context (a wake routes by id through the registry entry), so the `'static` is a
  /// promise the loop's exit keeps, not a leak. Before 2026-09-14 the box was leaked outright.
  pub fn build(seed: ShardSeed) -> Result<&'static ShardContext, RtError> {
    let config = seed.config;
    let driver = (seed.driver)(seed.kick)?;
    // The arena's generations continue from where the slot's previous holder left them, so a wake
    // word minted for that shard can never name a task of this one (registry slot reuse, §4.3).
    let generation_base = registry::entry(seed.id).map_or(0, |entry| entry.generation_base);
    let mut arena = Slab::with_generation_base(
      config.tasks_per_shard.min(config.segment_tasks()),
      config.tasks_per_shard,
      generation_base,
    );
    arena.reserve_segments(
      config
        .tasks_per_shard
        .div_ceil(config.segment_tasks().max(1)),
    );
    let timers = Wheel::new(
      config.timer_tick_ns,
      config.timers_per_shard,
      driver.now_ns(),
    );
    // Only a shard that tracks an estimate on a real clock times its wakes; the simulation's parks and
    // kicks read no host clock (D-20), so its instruction counts stay free of OS calls.
    let times_wakes = config.wake_tracking.is_some() && driver.kind() != DriverKind::Simulation;
    let context = Box::into_raw(Box::new(ShardContext {
      id: seed.id,
      local: LocalQueue::new(config.tasks_per_shard),
      outbound: seed.outbound,
      inbound: seed.inbound,
      current_task: Cell::new(None),
      exited: Cell::new(false),
      active: Cell::new(false),
      pair_full_events: Cell::new(0),
      nested_borrows: Cell::new(0),
      waited_for_ns: Cell::new(None),
      scheduler_overrun_ns: Cell::new(0),
      wake: Cell::new(
        config
          .wake_tracking
          .map(|tracking| WakeEstimate::new(tracking.prior_ns, tracking.shift)),
      ),
      idle_ratio: config
        .wake_tracking
        .map_or(1, |tracking| tracking.idle_ratio),
      fixed_quantum_ns: config.step_budget_ns,
      fixed_spin_ns: config.spin_ns,
      real_time: driver.kind() != DriverKind::Simulation,
      attribution: Cell::new(Tracker::default()),
      entry: registry::entry(seed.id),
      inner: RefCell::new(ShardInner {
        arena,
        timers,
        driver,
        control: seed.control,
        fired: Vec::with_capacity(config.timers_per_shard),
        completions: Vec::with_capacity(config.ring_entries),
        config,
        counters: Counters::default(),
        shutting_down: false,
        pollers: Vec::new(),
      }),
      kept: Kept::default(),
    }));
    let id = seed.id;
    registry::attach_context(id, context);
    if times_wakes && let Some(entry) = registry::entry(id) {
      entry.parking.time_wakes();
    }
    // SAFETY: `context` came from `Box::into_raw` just above — valid, aligned, uniquely owned — and
    // is freed only by `registry::reclaim_context`, which this thread calls after its loop returned
    // and every reference derived here is gone.
    Ok(unsafe { &*context })
  }

  /// Gives the shard `value` to own for its life and hands back a `'static` reference to it: the
  /// form a per-shard singleton takes (a socket's demultiplexer, the fleet identity its sessions
  /// present) so every task of the shard can borrow it plainly. Dropped with the context — on the
  /// owning thread after its loop returned and every task is gone — last kept first. Call it at
  /// boot, once per singleton (see [`Kept`]); it is not for per-operation values.
  pub fn keep<T: 'static>(&self, value: T) -> &'static T {
    let boxed: Box<T> = Box::new(value);
    let reference: &T = &boxed;
    // SAFETY: extends the borrow to `'static`. The value lives in a heap box whose allocation never
    // moves (only the `Box` handle does, into `kept`) and is dropped only with this context —
    // `registry::reclaim_context`, on the owning thread after its loop returned and its task arena
    // emptied — so no task, the only holder of such a reference, outlives it.
    let reference: &'static T = unsafe { &*std::ptr::from_ref(reference) };
    self.kept.0.borrow_mut().push(boxed);
    reference
  }

  /// Runs `f` with the mutable state, or refuses a nested borrow (counted).
  pub fn with_inner<R>(&self, f: impl FnOnce(&mut ShardInner) -> R) -> Option<R> {
    match self.inner.try_borrow_mut() {
      Ok(mut inner) => Some(f(&mut inner)),
      Err(_) => {
        self.nested_borrows.set(self.nested_borrows.get() + 1);
        None
      }
    }
  }

  /// Marks one period of forward progress of an application loop on this shard, for an observer on
  /// any thread (`registry::Pulse::progress`).
  pub fn beat_progress(&self) {
    if let Some(entry) = self.entry {
      entry.pulse.beat();
    }
  }

  /// Whether the shard left its loop.
  pub fn exited(&self) -> bool {
    self.exited.get()
  }

  /// Whether a client is active (the idle spin is enabled).
  pub fn active(&self) -> bool {
    self.active.get()
  }

  /// Sets the active flag on this shard's thread (the runtime sends a message from elsewhere).
  pub fn set_active(&self, active: bool) {
    self.active.set(active);
  }

  /// Whether any inbound ring holds a word (a check without a syscall).
  pub fn has_inbound(&self) -> bool {
    if self.entry.is_some_and(|e| {
      !e.inbound.is_empty() || e.control_pending.load(std::sync::atomic::Ordering::Acquire)
    }) {
      return true;
    }
    self.inbound.iter().any(|ring| !ring.is_empty())
  }

  /// The task being polled on this shard right now.
  pub fn current_task(&self) -> Option<TaskId> {
    let slot = self.current_task.get()?;
    let generation = self
      .with_inner(|inner| inner.arena.generation_at(slot))
      .flatten()?;
    Encoded::pack(self.id, slot, generation).map(TaskId)
  }

  /// One past the highest task-arena generation this shard has issued: what the registry slot's
  /// next holder starts from, so a wake word minted for this shard never names a task of the next
  /// (slot reuse, §4.3).
  pub fn arena_generation_high(&self) -> u32 {
    self
      .with_inner(|inner| inner.arena.generation_high())
      .unwrap_or(0)
  }

  /// The counters.
  pub fn counters(&self) -> Counters {
    let mut c = self.with_inner(|inner| inner.counters).unwrap_or_default();
    c.nested_borrows = self.nested_borrows.get();
    c.scheduler_overrun_ns = self.scheduler_overrun_ns.get();
    c.wake_cost_ns = self.wake_cost_ns();
    c
  }

  /// This shard's **measured scheduler quantum** (§4.8 "SWIM period = max(k × RTT p99, scheduler
  /// quantum)"), in nanoseconds: the exponentially-forgetting maximum of how far past a wait's
  /// deadline the shard's next step ran. Only an idle shard waits — parked in its driver or spinning
  /// for its timer ([`run`](Self::run)); a busy one never reaches either, and its late timers fire
  /// from the step's expiry without passing here — so a step that runs late after a wait is time the
  /// operating system left the shard off a core with nothing else to do: the descheduling the shard
  /// suffers, not the latency of serving its own tasks. Roughly zero on a quiet host (a park wakes
  /// within a tick), seconds on an oversubscribed one. Forgotten by [`OVERRUN_FORGET_SHIFT`] at each
  /// later wait so a spike recovers once the load lifts. The fleet's failure detector reads it through
  /// [`crate::futures::scheduler_overrun_ns`] to floor its windows at the starvation this node itself
  /// observes; an observer on another thread reads the mirror in the registry pulse.
  pub fn scheduler_overrun_ns(&self) -> u64 {
    self.scheduler_overrun_ns.get()
  }

  /// Folds one finished wait into the measured scheduler overrun ([`Self::scheduler_overrun_ns`]):
  /// the shard waited for the absolute `deadline_ns` and this step is the first to run after it.
  fn note_wait_overrun(&self, inner: &ShardInner, deadline_ns: u64) {
    let overrun = inner.driver.now_ns().saturating_sub(deadline_ns);
    let held = self.scheduler_overrun_ns.get();
    let forgotten = held.saturating_sub(held >> OVERRUN_FORGET_SHIFT);
    let measured = forgotten.max(overrun);
    self.scheduler_overrun_ns.set(measured);
    if let Some(entry) = self.entry {
      entry.pulse.record_scheduler_overrun(measured);
    }
  }

  /// The driver's clock.
  pub fn now_ns(&self) -> u64 {
    self.with_inner(|inner| inner.driver.now_ns()).unwrap_or(0)
  }

  /// Times a shard-pair ring was full and the sender spun.
  pub fn pair_full_events(&self) -> u64 {
    self.pair_full_events.get()
  }

  /// Sends a word to `target` over the pair ring, if one exists; kicks the target. A full ring is
  /// retried only while the target is live and has not left its loop: a target that exited never
  /// drains the ring again, so the wake is counted stale and the send reports `false` rather than
  /// spinning for good (the same fault `registry::send_foreign` had: a shutdown in which one shard
  /// left before a peer's last wakes to it would have hung the peer, and the join).
  pub fn send_to(&self, target: u16, word: u64) -> registry::PairSend {
    let Some(Some(ring)) = self.outbound.get(usize::from(target)) else {
      return registry::PairSend::NoRing;
    };
    let (mut producer, _) = ring.split();
    let mut pending = word;
    loop {
      match producer.push(pending) {
        Ok(()) => break,
        Err(back) => {
          pending = back;
          self.pair_full_events.set(self.pair_full_events.get() + 1);
          let live = registry::with_entry(target, |entry| {
            entry.kick.kick();
            !entry.exited.load(std::sync::atomic::Ordering::Acquire)
          });
          if live != Some(true) {
            registry::count_stale(target);
            return registry::PairSend::Gone;
          }
          self.drain_while_waiting();
        }
      }
    }
    let _ = registry::with_entry(target, |entry| entry.kick.kick());
    registry::PairSend::Sent
  }

  /// The multi-producer path from a shard thread to a shard it keeps no pair ring to (another
  /// runtime's): the same turns as a foreign thread's send, but draining this shard's own rings
  /// between them, so it never blocks its peers while it waits ([`Self::drain_while_waiting`]).
  pub fn send_foreign_draining(&self, target: u16, word: u64) {
    let mut pending = word;
    loop {
      match registry::try_send_foreign(target, pending) {
        registry::TrySend::Landed | registry::TrySend::Gone => return,
        registry::TrySend::Full(back) => {
          pending = back;
          self.drain_while_waiting();
        }
      }
    }
  }

  /// One waiting turn of a shard whose send found a full ring: drain what its own peers sent it (so
  /// a peer spinning on a full ring *to this shard* is released — two shards saturating each other's
  /// rings would otherwise each wait for the other to drain, for good), then yield. Skipped when the
  /// state is borrowed (a wake from inside a borrow): the next turn drains.
  fn drain_while_waiting(&self) {
    let _ = self.with_inner(|inner| self.drain_inbound(inner));
    // The poll waits for its peer here, not on the CPU: the window marks it (`attribution`).
    self.update_attribution(Tracker::yielded_in_poll);
    std::thread::yield_now();
  }

  // ------------------------------------------------------------------ admission

  /// Admits a spawn request (from any thread's message, or the runtime): detached. The request's
  /// receipt, if it carries one, is answered with the outcome.
  pub fn spawn_request(&self, request: SpawnRequest) -> Result<TaskId, RtError> {
    let SpawnRequest {
      future,
      parent,
      receipt,
    } = request;
    let parent = parent.filter(|p| p.shard() == self.id).map(|p| p.slot());
    let admitted = self.admit(future, parent, false);
    receipt.answer(match &admitted {
      Ok(task) => Admission::Admitted(*task),
      Err(refusal) => Admission::Refused(refusal.clone()),
    });
    admitted
  }

  /// Admits a local future under `parent`, joinable.
  pub fn spawn_local(&self, future: BoxedFuture, parent: Option<u32>) -> Result<TaskId, RtError> {
    self.admit(future, parent, true)
  }

  fn admit(
    &self,
    future: BoxedFuture,
    parent: Option<u32>,
    joinable: bool,
  ) -> Result<TaskId, RtError> {
    let id = self
      .with_inner(|inner| -> Result<TaskId, RtError> {
        let handle = match inner.arena.insert(TaskSlot::new(future, parent, joinable)) {
          Ok(h) => h,
          Err(slates_mem::MemError::SlabFull { capacity }) => {
            inner.counters.admission_refused += 1;
            return Err(RtError::TooManyTasks { capacity });
          }
          Err(e) => return Err(RtError::Mem(e)),
        };
        let slot = handle.index();
        if let Some(p) = parent {
          link_child(&mut inner.arena, p, slot);
        }
        if let Ok(task) = inner.arena.get_mut(handle) {
          task.state = State::Queued;
        }
        inner.counters.spawns += 1;
        Encoded::pack(self.id, slot, handle.generation())
          .map(TaskId)
          .ok_or(RtError::TooManyTasks {
            capacity: inner.config.tasks_per_shard,
          })
      })
      .ok_or(RtError::NotOnShardThread)??;
    self.local.push(id.0.slot());
    Ok(id)
  }

  /// Requests cancellation; the task terminates at its next poll boundary.
  pub fn cancel(&self, id: TaskId) -> Result<(), RtError> {
    let handle = handle_of(id);
    self
      .with_inner(|inner| -> Result<(), RtError> {
        let task = inner.arena.get_mut(handle).map_err(|_| stale(id))?;
        task.cancel_requested = true;
        Ok(())
      })
      .ok_or(RtError::NotOnShardThread)??;
    self.local.push(id.0.slot());
    Ok(())
  }

  /// Marks a joinable task detached: its slot is reaped at termination (or now, if terminal).
  pub fn detach(&self, id: TaskId) -> Result<(), RtError> {
    let handle = handle_of(id);
    self
      .with_inner(|inner| -> Result<(), RtError> {
        let task = inner.arena.get_mut(handle).map_err(|_| stale(id))?;
        task.joinable = false;
        if task.is_done() {
          let parent = task.parent;
          if let Some(p) = parent {
            unlink_child(&mut inner.arena, p, id.0.slot());
          }
          let _ = inner.arena.remove(handle);
        }
        Ok(())
      })
      .ok_or(RtError::NotOnShardThread)?
  }

  /// Polls a join: `Ready(outcome)` once the task is terminal (its slot is reaped then), else
  /// `Pending` with `waker` recorded.
  pub fn poll_join(&self, id: TaskId, waker: &std::task::Waker) -> Poll<Result<Outcome, RtError>> {
    let handle = handle_of(id);
    self
      .with_inner(|inner| match inner.arena.get_mut(handle) {
        Err(_) => Poll::Ready(Err(stale(id))),
        Ok(task) if task.is_done() => {
          let outcome = task.outcome.unwrap_or(Outcome::Cancelled);
          let parent = task.parent;
          if let Some(p) = parent {
            unlink_child(&mut inner.arena, p, id.0.slot());
          }
          let _ = inner.arena.remove(handle);
          Poll::Ready(Ok(outcome))
        }
        Ok(task) => {
          task.join_waker = Some(waker.clone());
          Poll::Pending
        }
      })
      .unwrap_or(Poll::Ready(Err(RtError::NotOnShardThread)))
  }

  /// Arms a timer for `word` at `deadline_ns`.
  pub fn arm_timer(&self, deadline_ns: u64, word: u64) -> Result<crate::timer::TimerId, RtError> {
    self
      .with_inner(|inner| inner.timers.insert(deadline_ns, word))
      .ok_or(RtError::NotOnShardThread)?
  }

  /// Registers one-shot interest in a socket's readability with the shard's driver (§4.10a): when it
  /// next becomes readable, the driver wakes the task whose `word` this is. The path a UDP recv future
  /// takes; mirrors [`arm_timer`], but on the driver rather than the wheel.
  pub fn register_readable(&self, raw: i32, word: u64) -> Result<(), RtError> {
    self
      .with_inner(|inner| inner.driver.register_readable(raw, word))
      .ok_or(RtError::NotOnShardThread)?
  }

  /// Registers one-shot interest in a socket's writability with the shard's driver (§4.6): when it
  /// next has send-buffer space, the driver wakes the task whose `word` this is. The path a TCP
  /// `write_all` takes when the send buffer filled; mirrors [`ShardContext::register_readable`].
  pub fn register_writable(&self, raw: i32, word: u64) -> Result<(), RtError> {
    self
      .with_inner(|inner| inner.driver.register_writable(raw, word))
      .ok_or(RtError::NotOnShardThread)?
  }

  /// Whether this shard runs the simulation driver (so a `UdpSocket` uses the in-memory fabric).
  pub fn driver_is_sim(&self) -> bool {
    self
      .with_inner(|inner| inner.driver.is_sim())
      .unwrap_or(false)
  }

  /// Disarms a timer.
  pub fn disarm_timer(&self, id: crate::timer::TimerId) -> Result<(), RtError> {
    self
      .with_inner(|inner| inner.timers.cancel(id))
      .ok_or(RtError::NotOnShardThread)?
  }

  /// Live tasks in the arena.
  pub fn live_tasks(&self) -> usize {
    self.with_inner(|inner| inner.arena.len()).unwrap_or(0)
  }

  // ------------------------------------------------------------------ the loop

  /// Runs the loop on the calling thread until shutdown completes. When idle and a client is
  /// active, the shard spins for the configured window checking its rings before it parks: a
  /// wake that lands during the spin costs a cache-line transfer instead of a kernel wake.
  pub fn run(&'static self) {
    registry::set_current(Some(self));
    // Driver I/O completions are harvested only when the shard waits ([`park`]). Under continuous task
    // readiness the loop below never reaches `park` — a shard whose client never lets `serve_round` go
    // idle re-queues its serve task every step — so without this an I/O-bound task (a fleet node's
    // datagram demux, whose socket readiness only `wait` delivers) would starve indefinitely behind the
    // CPU-bound one, and its peers' packets would sit unread in the kernel while the shard spins. So a
    // run that stays busy without ever waiting harvests the driver without blocking once it has gone a
    // step budget (`step_budget_ns`, "a step longer than a peer's wake starves the shard" — §4.3) since
    // its last wait: I/O then keeps pace with tasks under any load, and an idle shard (which reaches
    // `park` every loop) pays nothing for it.
    let mut last_wait_ns = self.now_ns();
    loop {
      let outcome = self.step();
      if outcome.exit {
        // The slot's next holder starts its task generations past this shard's (slot reuse), and a
        // sender still spinning on this shard's full ring learns nothing will drain it.
        registry::note_arena_generation(self.id, self.arena_generation_high());
        registry::note_exited(self.id);
        break;
      }
      if outcome.did_work {
        let now = self.now_ns();
        if now.saturating_sub(last_wait_ns) >= self.quantum_ns() {
          self.harvest_io();
          last_wait_ns = now;
        }
        continue;
      }
      if self.active.get() && self.spin_until_work(outcome.next_deadline_ns) {
        continue;
      }
      self.park(outcome.next_deadline_ns);
      last_wait_ns = self.now_ns();
    }
    registry::set_current(None);
    self.exited.set(true);
  }

  /// Harvests the driver's ready I/O completions **without blocking** (a zero timeout) and queues their
  /// tasks, for a continuously busy [`run`] that would otherwise never reach [`park`] where I/O is
  /// harvested. Unlike `park` it does not set the parked flag: the shard is not waiting, so a concurrent
  /// kick must not believe it is. A driver error other than loss is left for the next real wait to
  /// surface; loss is likewise deferred (this is a best-effort poll, not the loop's liveness point).
  fn harvest_io(&self) {
    self.with_inner(|inner| {
      let mut completions = std::mem::take(&mut inner.completions);
      let result = inner.driver.wait(Some(0), &mut completions);
      for c in completions.drain(..) {
        inner.counters.completions += 1;
        self.local.push(Encoded::from_word(c.user_data).slot());
      }
      inner.completions = completions;
      if matches!(result, Err(RtError::DriverLost)) {
        inner.counters.driver_lost += 1;
      }
    });
  }

  /// Spins for the configured window watching the rings and the driver; true when something
  /// arrived or a timer fell due during the spin (either is work for the next step), false when
  /// the window ran out with nothing to do.
  fn spin_until_work(&self, deadline_ns: Option<u64>) -> bool {
    let spin_ns = self.spin_window_ns();
    let now = self.now_ns();
    if spin_ns == 0 {
      return false;
    }
    self.update_attribution(Tracker::wait_began);
    let found = self.spin_for_work(now.saturating_add(spin_ns), deadline_ns);
    if found {
      self.wait_ended();
    }
    found
  }

  /// The spin itself: true when work arrived or `deadline_ns` fell due before `spin_end`.
  fn spin_for_work(&self, spin_end: u64, deadline_ns: Option<u64>) -> bool {
    loop {
      // A poller's question may consume its signal (the daemon's doorbell flag is swapped to false
      // when asked), so the spin wakes the pollers it finds ready rather than only reporting them:
      // asked here and dropped, a ring would be lost until the next one (`tests/pollers.rs`).
      if self.has_inbound()
        || self
          .with_inner(|inner| inner.driver.has_pending() || self.wake_ready_pollers(inner))
          .unwrap_or(false)
      {
        self.with_inner(|inner| inner.counters.spin_hits += 1);
        return true;
      }
      let now = self.now_ns();
      if let Some(deadline) = deadline_ns
        && now >= deadline
      {
        self.with_inner(|inner| inner.counters.spin_deadlines += 1);
        // The spin waited for this deadline as a park would have; the next step measures its lateness.
        self.waited_for_ns.set(Some(deadline));
        return true;
      }
      if now >= spin_end {
        self.with_inner(|inner| inner.counters.spin_misses += 1);
        return false;
      }
      std::hint::spin_loop();
    }
  }

  /// Steps until no task, timer or message is pending, parking for timers as needed; returns
  /// when the shard is idle or has exited. The driver is polled only when it may hold something
  /// (a zero-timeout poll costs a syscall the idle path must not pay for nothing).
  pub fn run_until_idle(&'static self) {
    registry::set_current(Some(self));
    loop {
      let outcome = self.step();
      if outcome.exit {
        self.exited.set(true);
        break;
      }
      if outcome.did_work {
        continue;
      }
      match outcome.next_deadline_ns {
        Some(deadline) => self.park(Some(deadline)),
        None => {
          let pending = self
            .with_inner(|inner| inner.driver.has_pending())
            .unwrap_or(false);
          if !pending {
            break;
          }
          self.park(Some(self.now_ns()));
          if !self.step().did_work {
            break;
          }
        }
      }
    }
    registry::set_current(None);
  }

  /// One loop iteration without blocking.
  pub fn step(&'static self) -> StepOutcome {
    if self.exited.get() {
      return StepOutcome {
        did_work: false,
        next_deadline_ns: None,
        exit: true,
      };
    }
    registry::set_current(Some(self));
    // While a long poll has gone unattributed, each step opens a window at its start, so the next long
    // poll is judged by the step's own CPU alone; an unarmed shard reads neither clock.
    if self.real_time {
      self.update_attribution(|tracker| tracker.step_began(|| self.account_now()));
    }
    let mut did_work = false;
    let (drained, batch) = self
      .with_inner(|inner| {
        inner.counters.steps += 1;
        // The pulse an observer on another thread reads (`registry::Pulse`): a handful of plain stores on a
        // line this core owns, so a stall diagnosis sees the arena saturating (`admission_refused`) or a
        // long poll (`longest_step_ns`) without a shard round-trip.
        if let Some(entry) = self.entry {
          entry.pulse.record(
            inner.counters.steps,
            inner.counters.spawns,
            inner.counters.completed,
            inner.counters.admission_refused,
            inner.counters.longest_step_ns,
          );
        }
        // The first step after a wait: how late it runs against the deadline the wait was for is the
        // shard's measured scheduler overrun (`scheduler_overrun_ns`).
        if let Some(deadline) = self.waited_for_ns.take() {
          self.note_wait_overrun(inner, deadline);
        }
        let mut work = self.drain_control(inner);
        work |= self.drain_inbound(inner);
        work |= self.expire_timers(inner);
        work |= self.wake_ready_pollers(inner);
        (work, inner.config.batch)
      })
      .unwrap_or((false, 1));
    did_work |= drained;
    // At most one batch, oldest first: a step costs its batch, not the ready set (`LocalQueue`).
    let ready = self.local.take_ready(batch.max(1));
    for slot in &ready {
      did_work = true;
      self.poll_slot(*slot);
    }
    self.local.finish_drain(ready);
    let (exit, next_deadline_ns) = self
      .with_inner(|inner| {
        (
          inner.shutting_down && inner.arena.is_empty(),
          inner.timers.next_deadline_ns(),
        )
      })
      .unwrap_or((false, None));
    if exit {
      self.exited.set(true);
    }
    self.update_attribution(|tracker| tracker.step_ended(did_work));
    StepOutcome {
      did_work,
      next_deadline_ns,
      exit,
    }
  }

  /// Parks in the driver until a kick, a completion or `deadline_ns`. The parking announcement
  /// comes first and the inbox re-check second, so a message that landed between the loop's last
  /// look and here is seen now, and one that lands after sees the announcement and kicks (the
  /// protocol and its loom model: [`crate::parking`]).
  pub fn park(&'static self, deadline_ns: Option<u64>) {
    // What this wait is for; the next step measures how late it runs against it (`scheduler_overrun_ns`).
    self.waited_for_ns.set(deadline_ns);
    self.update_attribution(Tracker::wait_began);
    let mut lost = false;
    match self.entry {
      Some(entry) => {
        // A tracking shard counts its voluntary switches as the wait begins, so a kicked park can tell a
        // wait that slept from one that found the kick pending (`note_wake`); the shard is idle here, so
        // the read delays nothing.
        let learns = self.real_time && self.wake.get().is_some();
        let mut switches_before_wait = None;
        let parked = entry.parking.park_unless_pending(
          || self.has_inbound(),
          || {
            if learns {
              switches_before_wait = attribution::voluntary_switches_now();
            }
            lost = self.wait_in_driver(deadline_ns);
          },
        );
        if let Parked::Waited(Some(woken)) = parked {
          self.note_wake(entry, woken, switches_before_wait);
        }
      }
      None => lost = self.wait_in_driver(deadline_ns),
    }
    self.wait_ended();
    if lost {
      self.fail_all();
    }
  }

  /// The step quantum now (§4.3, "a step longer than a peer's wake starves the shard"): the online wake
  /// estimate while the shard tracks one, else the configured step budget. What the I/O harvest cadence,
  /// the long-step count and the daemon's cooperative slices ([`crate::futures::step_budget_ns`]) read.
  pub fn quantum_ns(&self) -> u64 {
    self
      .wake
      .get()
      .map_or(self.fixed_quantum_ns, |estimate| estimate.mean_ns())
      .max(1)
  }

  /// The idle spin window now: the online wake estimate times the configured idle ratio while the shard
  /// tracks one (the 2-competitive spin, §4.3), else the configured spin.
  fn spin_window_ns(&self) -> u64 {
    self.wake.get().map_or(self.fixed_spin_ns, |estimate| {
      estimate.mean_ns().saturating_mul(self.idle_ratio)
    })
  }

  /// The online wake estimate now, nanoseconds (the configured step budget when not tracking).
  pub fn wake_cost_ns(&self) -> u64 {
    self.quantum_ns()
  }

  /// Folds a park's measured wake into the online estimate (§4.3): the kick-to-running latency of a park
  /// a kick ended asleep ([`Woken::latency_ns`]) — the event the boot probe times, a sleeper woken. A
  /// stamp from before the park's announcement is counted stale; one from the park's setup, or (Linux) a
  /// wait across which the thread never switched out (`switches_before_wait` against now), found the
  /// shard awake and is counted unslept; both are dropped. A park a timer or a completion ended teaches
  /// nothing. Only a tracking shard on a real clock learns: the simulation's clock is not the kicker's.
  fn note_wake(&self, entry: &Entry, woken: Woken, switches_before_wait: Option<u64>) {
    let Some(mut estimate) = self.wake.get() else {
      return;
    };
    if !self.real_time {
      return;
    }
    if woken.stale() {
      self.with_inner(|inner| inner.counters.wake_stale += 1);
      return;
    }
    let Some(latency) = woken.latency_ns() else {
      if woken.early() {
        self.with_inner(|inner| inner.counters.wake_unslept += 1);
      }
      return;
    };
    // Where the thread's voluntary switches are counted, a wait that never switched never slept: the
    // kick was pending when the wait began (or landed before the kernel put the thread to sleep).
    if let (Some(before), Some(after)) =
      (switches_before_wait, attribution::voluntary_switches_now())
      && after == before
    {
      self.with_inner(|inner| inner.counters.wake_unslept += 1);
      return;
    }
    estimate.record(latency);
    self.wake.set(Some(estimate));
    let mean = estimate.mean_ns();
    self.with_inner(|inner| {
      inner.counters.wake_samples += 1;
      inner.counters.wake_cost_ns = mean;
    });
    entry.pulse.record_wake_cost(mean);
  }

  /// Attributes a poll against the step quantum (§4.3's long-step count): within it by the wall clock it
  /// is within; past it, the attribution windows decide whose it was ([`crate::attribution`]). A
  /// simulated shard's clock is not the thread's, and nothing preempts a simulation, so its long polls
  /// are the task's.
  fn attribute_poll(&self, poll_started_ns: u64, ended_ns: u64) -> Option<Attribution> {
    let quantum = self.quantum_ns();
    if ended_ns.saturating_sub(poll_started_ns) <= quantum {
      return None;
    }
    if !self.real_time {
      return Some(Attribution::Long);
    }
    let end = attribution::thread_account();
    let mut tracker = self.attribution.get();
    let attributed = tracker.long_poll(poll_started_ns, ended_ns, end, quantum);
    self.attribution.set(tracker);
    Some(attributed)
  }

  /// A wait ended: while a long poll has gone unattributed, a window opens now.
  fn wait_ended(&self) {
    if self.real_time {
      self.update_attribution(|tracker| tracker.wait_ended(|| self.account_now()));
    }
  }

  /// The thread's account and the shard clock now: an attribution window's start.
  fn account_now(&self) -> Option<(attribution::ThreadAccount, u64)> {
    attribution::thread_account().map(|account| (account, self.now_ns()))
  }

  /// Applies `change` to the attribution tracker.
  fn update_attribution(&self, change: impl FnOnce(&mut Tracker)) {
    let mut tracker = self.attribution.get();
    change(&mut tracker);
    self.attribution.set(tracker);
  }

  /// The driver's blocking wait until a kick, a completion or `deadline_ns`, its completions
  /// queued; true when the driver was lost.
  fn wait_in_driver(&'static self, deadline_ns: Option<u64>) -> bool {
    self
      .with_inner(|inner| {
        inner.counters.waits += 1;
        if let Some(entry) = self.entry {
          entry.pulse.record_waits(inner.counters.waits);
        }
        let timeout = deadline_ns.map(|d| d.saturating_sub(inner.driver.now_ns()));
        let mut completions = std::mem::take(&mut inner.completions);
        let result = inner.driver.wait(timeout, &mut completions);
        for c in completions.drain(..) {
          inner.counters.completions += 1;
          self.local.push(Encoded::from_word(c.user_data).slot());
        }
        inner.completions = completions;
        match result {
          Ok(()) => false,
          Err(RtError::DriverLost) => {
            inner.counters.driver_lost += 1;
            true
          }
          Err(_) => {
            inner.counters.driver_errors += 1;
            false
          }
        }
      })
      .unwrap_or(false)
  }

  /// Cancels every task with a terminal completion and exits: the driver is gone.
  fn fail_all(&'static self) {
    self.with_inner(|inner| {
      inner.shutting_down = true;
      cancel_all(&mut inner.arena, &self.local);
    });
    let mut bound = self.live_tasks().saturating_mul(2).saturating_add(1);
    while !self.exited.get() && bound > 0 {
      let outcome = self.step();
      bound -= 1;
      if outcome.exit {
        break;
      }
    }
    self.exited.set(true);
  }

  fn drain_control(&self, inner: &mut ShardInner) -> bool {
    let Some(entry) = self.entry else {
      return false;
    };
    if !entry
      .control_pending
      .load(std::sync::atomic::Ordering::Acquire)
    {
      return false;
    }
    // Clear before draining: a send that lands during the drain sets the flag again and is
    // seen on the next step at the latest.
    entry
      .control_pending
      .store(false, std::sync::atomic::Ordering::Release);
    let batch = inner.config.batch.max(1);
    let mut drained = 0;
    while drained < batch {
      let Ok(message) = inner.control.try_recv() else {
        break;
      };
      drained += 1;
      inner.counters.controls += 1;
      self.handle_control(inner, message);
    }
    if drained == batch {
      // A whole batch drained may have left messages behind it: re-arm the flag, so the next step
      // drains again. Before 2026-09-17 nothing did, and a burst larger than one batch — a flood of
      // submissions behind a long poll — sat undrained until some later send happened to set the flag:
      // a spawn queued behind it was never admitted, and a shutdown refused by the still-full channel
      // was retried against a shard that had parked for good (`tests/burst.rs`,
      // `docs/bugs/2026-09-17-control-drain-forgets-a-burst-past-one-batch.md`).
      entry
        .control_pending
        .store(true, std::sync::atomic::Ordering::Release);
    }
    drained > 0
  }

  fn drain_inbound(&self, inner: &mut ShardInner) -> bool {
    let mut any = false;
    let batch = inner.config.batch.max(1);
    if let Some(entry) = self.entry {
      let mut consumer = entry.inbound.consumer();
      for _ in 0..batch {
        let Some(word) = consumer.pop() else { break };
        any = true;
        inner.counters.wakes_foreign += 1;
        self.handle_wake(inner, Encoded::from_word(word));
      }
    }
    for ring in &self.inbound {
      let (_, mut consumer) = ring.split();
      for _ in 0..batch {
        let Some(word) = consumer.pop() else { break };
        any = true;
        inner.counters.wakes_pair += 1;
        self.handle_wake(inner, Encoded::from_word(word));
      }
    }
    any
  }

  /// Registers `task` as a poller with `ready`; the loop wakes it whenever `ready` says so.
  /// Refused when the task is not live on this shard.
  pub fn register_poller(&self, task: TaskId, ready: Box<dyn Fn() -> bool>) -> Result<(), RtError> {
    let slot = task.0.slot();
    let generation = task.0.generation();
    self
      .with_inner(|inner| {
        if !inner
          .arena
          .generation_at(slot)
          .is_some_and(|g| g & GENERATION_MASK == generation)
        {
          return Err(RtError::StaleTask { slot, generation });
        }
        inner.pollers.push(Poller {
          slot,
          generation,
          ready,
        });
        Ok(())
      })
      .unwrap_or(Err(RtError::NotOnShardThread))
  }

  /// Forgets a poller.
  pub fn unregister_poller(&self, task: TaskId) {
    let slot = task.0.slot();
    self.with_inner(|inner| inner.pollers.retain(|p| p.slot != slot));
  }

  /// Wakes every poller whose ring is ready; a poller whose task ended is dropped.
  fn wake_ready_pollers(&self, inner: &mut ShardInner) -> bool {
    let mut any = false;
    inner.pollers.retain(|p| {
      inner
        .arena
        .generation_at(p.slot)
        .is_some_and(|g| g & GENERATION_MASK == p.generation)
    });
    for p in &inner.pollers {
      if (p.ready)() {
        any = true;
        inner.counters.poller_wakes += 1;
        self.local.push(p.slot);
      }
    }
    any
  }

  fn handle_wake(&self, inner: &mut ShardInner, word: Encoded) {
    if inner
      .arena
      .generation_at(word.slot())
      .is_some_and(|g| g & GENERATION_MASK == word.generation())
    {
      self.local.push(word.slot());
    } else {
      inner.counters.stale_wakes += 1;
    }
  }

  fn handle_control(&self, inner: &mut ShardInner, message: Control) {
    match message {
      Control::Spawn(request) => {
        let SpawnRequest {
          future,
          parent,
          receipt,
        } = *request;
        if inner.shutting_down {
          // A shard shutting down admits nothing new — its arena drains to empty and the loop exits
          // — so the request is refused unadmitted: its receipt answered, its future dropped here.
          inner.counters.refused_at_shutdown += 1;
          receipt.answer(Admission::Terminated);
          return;
        }
        let parent = parent.filter(|p| p.shard() == self.id).map(|p| p.slot());
        match inner.arena.insert(TaskSlot::new(future, parent, false)) {
          Ok(handle) => {
            if let Some(p) = parent {
              link_child(&mut inner.arena, p, handle.index());
            }
            if let Ok(task) = inner.arena.get_mut(handle) {
              task.state = State::Queued;
            }
            inner.counters.spawns += 1;
            self.local.push(handle.index());
            receipt.answer(
              match Encoded::pack(self.id, handle.index(), handle.generation()) {
                Some(word) => Admission::Admitted(TaskId(word)),
                None => Admission::Refused(RtError::TooManyTasks {
                  capacity: inner.config.tasks_per_shard,
                }),
              },
            );
          }
          Err(slates_mem::MemError::SlabFull { capacity }) => {
            inner.counters.admission_refused += 1;
            receipt.answer(Admission::Refused(RtError::TooManyTasks { capacity }));
          }
          Err(e) => {
            inner.counters.admission_refused += 1;
            receipt.answer(Admission::Refused(RtError::Mem(e)));
          }
        }
      }
      Control::Cancel(word) => {
        if let Ok(task) = inner
          .arena
          .get_mut(Handle::from_raw(word.slot(), word.generation()))
        {
          task.cancel_requested = true;
          self.local.push(word.slot());
        }
      }
      Control::Shutdown => {
        inner.shutting_down = true;
        cancel_all(&mut inner.arena, &self.local);
      }
      Control::Active(active) => self.active.set(active),
    }
  }

  fn expire_timers(&self, inner: &mut ShardInner) -> bool {
    let now = inner.driver.now_ns();
    let mut fired = std::mem::take(&mut inner.fired);
    inner.timers.advance(now, &mut fired);
    let any = !fired.is_empty();
    for word in fired.drain(..) {
      inner.counters.timers_fired += 1;
      self.local.push(Encoded::from_word(word).slot());
    }
    inner.fired = fired;
    any
  }

  fn poll_slot(&self, slot: u32) {
    self.local.clear_pending(slot);
    let taken = self.with_inner(|inner| take_future(inner, slot)).flatten();
    let Some((mut future, generation, cancelled)) = taken else {
      return;
    };
    if cancelled {
      // The future is dropped outside any borrow: destructors may wake other tasks.
      drop(future);
      self.with_inner(|inner| finish(inner, &self.local, slot, Outcome::Cancelled));
      return;
    }
    let word = Encoded::pack(self.id, slot, generation).unwrap_or(Encoded::from_word(0));
    let waker = waker_for(word);
    let mut cx = Context::from_waker(&waker);
    self.current_task.set(Some(slot));
    let start = self.now_ns();
    let poll = future.as_mut().poll(&mut cx);
    let ended = self.now_ns();
    self.current_task.set(None);
    let done = matches!(poll, Poll::Ready(()));
    let attributed = self.attribute_poll(start, ended);
    let dropped = self.with_inner(|inner| {
      after_poll(
        inner,
        &self.local,
        PollDone {
          slot,
          future,
          elapsed: ended.saturating_sub(start),
          done,
          attributed,
        },
      )
    });
    drop(dropped);
  }
}

/// Format: the generation bits an inbound wake carries (the low 24 of the slot's 32).
const GENERATION_MASK: u32 = (1 << 24) - 1;

fn handle_of(id: TaskId) -> Handle<TaskSlot> {
  Handle::from_raw(id.0.slot(), id.0.generation())
}

fn stale(id: TaskId) -> RtError {
  RtError::StaleTask {
    slot: id.0.slot(),
    generation: id.0.generation(),
  }
}

fn handle_at(arena: &Slab<TaskSlot>, slot: u32) -> Option<Handle<TaskSlot>> {
  arena.generation_at(slot).map(|g| Handle::from_raw(slot, g))
}

/// Takes the future out of a live slot: `(future, generation, cancelled)`.
fn take_future(inner: &mut ShardInner, slot: u32) -> Option<(BoxedFuture, u32, bool)> {
  let handle = handle_at(&inner.arena, slot)?;
  let task = inner.arena.get_mut(handle).ok()?;
  if matches!(task.state, State::Finishing | State::Done | State::Running) {
    return None;
  }
  let future = task.future.take()?;
  if task.cancel_requested {
    task.state = State::Finishing;
    return Some((future, handle.generation(), true));
  }
  task.state = State::Running;
  Some((future, handle.generation(), false))
}

/// One finished poll, as `after_poll` records it.
struct PollDone {
  slot: u32,
  future: BoxedFuture,
  elapsed: u64,
  done: bool,
  /// Who held it, when it ran past the step quantum by the wall clock ([`ShardContext::attribute_poll`]).
  attributed: Option<Attribution>,
}

/// Records the poll and either stores the future back or finishes the task; returns a future to
/// drop outside the borrow, if any.
fn after_poll(inner: &mut ShardInner, local: &LocalQueue, poll: PollDone) -> Option<BoxedFuture> {
  let PollDone {
    slot,
    future,
    elapsed,
    done,
    attributed,
  } = poll;
  let long = attributed.is_some_and(Attribution::is_tasks);
  inner.counters.polls += 1;
  if let Some(attributed) = attributed {
    count_long_poll(&mut inner.counters, attributed);
  }
  inner.counters.longest_step_ns = inner.counters.longest_step_ns.max(elapsed);
  let Some(handle) = handle_at(&inner.arena, slot) else {
    return Some(future);
  };
  let Ok(task) = inner.arena.get_mut(handle) else {
    return Some(future);
  };
  task.polls += 1;
  if long {
    task.long_steps += 1;
  }
  task.longest_step_ns = task.longest_step_ns.max(elapsed);
  if done {
    finish(inner, local, slot, Outcome::Completed);
    return Some(future);
  }
  if task.cancel_requested {
    finish(inner, local, slot, Outcome::Cancelled);
    return Some(future);
  }
  task.future = Some(future);
  task.state = State::Idle;
  None
}

/// Counts a poll past the step quantum by the wall clock under whoever held it.
fn count_long_poll(counters: &mut Counters, attributed: Attribution) {
  if attributed.is_tasks() {
    counters.long_steps += 1;
  }
  if attributed == Attribution::Blocked {
    counters.blocked_steps += 1;
  }
  if attributed == Attribution::Preempted {
    counters.preempted_steps += 1;
  }
  if attributed.is_unattributed() {
    counters.unattributed_steps += 1;
  }
}

/// Moves a task to `Finishing`, joins its children (cancelling the live ones, reaping the done
/// ones), and completes it when none is live.
fn finish(inner: &mut ShardInner, local: &LocalQueue, slot: u32, outcome: Outcome) {
  let Some(handle) = handle_at(&inner.arena, slot) else {
    return;
  };
  let (children, first_child) = match inner.arena.get_mut(handle) {
    Ok(task) => {
      task.state = State::Finishing;
      task.outcome = Some(outcome);
      task.future = None;
      (task.children, task.first_child)
    }
    Err(_) => return,
  };
  match outcome {
    Outcome::Completed => inner.counters.completed += 1,
    Outcome::Cancelled => inner.counters.cancelled += 1,
  }
  let mut child = first_child;
  while child != NO_LINK {
    let Some(child_handle) = handle_at(&inner.arena, child) else {
      break;
    };
    let (next, done) = match inner.arena.get_mut(child_handle) {
      Ok(task) => {
        task.joinable = false;
        task.cancel_requested = true;
        (task.next_sibling, task.is_done())
      }
      Err(_) => (NO_LINK, false),
    };
    if done {
      unlink_child(&mut inner.arena, slot, child);
      let _ = inner.arena.remove(child_handle);
    } else {
      local.push(child);
    }
    child = next;
  }
  if children == 0 {
    complete(inner, slot);
  }
}

/// Marks a task terminal, wakes its joiner, unlinks it from its parent when detached, reaps it if
/// detached, and completes the parent if it was waiting on this child.
fn complete(inner: &mut ShardInner, slot: u32) {
  let mut current = Some(slot);
  while let Some(slot) = current {
    current = None;
    let Some(handle) = handle_at(&inner.arena, slot) else {
      break;
    };
    let (parent, joinable, joiner) = match inner.arena.get_mut(handle) {
      Ok(task) => {
        task.state = State::Done;
        (task.parent, task.joinable, task.join_waker.take())
      }
      Err(_) => break,
    };
    if let Some(waker) = joiner {
      waker.wake();
    }
    if let Some(p) = parent {
      // A joinable task keeps its parent link until it is joined or its parent finishes, so the
      // parent can reap it; a detached one leaves the list now.
      if !joinable {
        unlink_child(&mut inner.arena, p, slot);
      }
      if let Some(parent_task) =
        handle_at(&inner.arena, p).and_then(|h| inner.arena.get_mut(h).ok())
      {
        parent_task.children = parent_task.children.saturating_sub(1);
        if parent_task.state == State::Finishing && parent_task.children == 0 {
          current = Some(p);
        }
      }
    }
    if !joinable {
      let _ = inner.arena.remove(handle);
    }
  }
}

fn cancel_all(arena: &mut Slab<TaskSlot>, local: &LocalQueue) {
  let slots: Vec<u32> = arena.iter().map(|(h, _)| h.index()).collect();
  for slot in slots {
    if let Some(task) = handle_at(arena, slot).and_then(|h| arena.get_mut(h).ok()) {
      task.cancel_requested = true;
    }
    local.push(slot);
  }
}

fn link_child(arena: &mut Slab<TaskSlot>, parent: u32, child: u32) {
  let old_first = match handle_at(arena, parent).and_then(|h| arena.get_mut(h).ok()) {
    Some(p) => {
      let old = p.first_child;
      p.first_child = child;
      p.children = p.children.saturating_add(1);
      old
    }
    None => return,
  };
  if let Some(c) = handle_at(arena, child).and_then(|h| arena.get_mut(h).ok()) {
    c.next_sibling = old_first;
    c.prev_sibling = NO_LINK;
  }
  if old_first != NO_LINK
    && let Some(next) = handle_at(arena, old_first).and_then(|h| arena.get_mut(h).ok())
  {
    next.prev_sibling = child;
  }
}

fn unlink_child(arena: &mut Slab<TaskSlot>, parent: u32, child: u32) {
  let (prev, next) = match handle_at(arena, child).and_then(|h| arena.get_mut(h).ok()) {
    Some(c) => (c.prev_sibling, c.next_sibling),
    None => return,
  };
  if prev == NO_LINK {
    if let Some(p) = handle_at(arena, parent).and_then(|h| arena.get_mut(h).ok()) {
      p.first_child = next;
    }
  } else if let Some(pv) = handle_at(arena, prev).and_then(|h| arena.get_mut(h).ok()) {
    pv.next_sibling = next;
  }
  if next != NO_LINK
    && let Some(nx) = handle_at(arena, next).and_then(|h| arena.get_mut(h).ok())
  {
    nx.prev_sibling = prev;
  }
}

/// Pins a boxed future for admission.
pub fn boxed<F: std::future::Future<Output = ()> + 'static>(future: F) -> BoxedFuture {
  Box::pin(future) as Pin<Box<dyn std::future::Future<Output = ()>>>
}
