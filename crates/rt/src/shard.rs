//! The shard: one thread, one task arena, one run queue, one timing wheel, one driver, and the
//! loop that ties them (§4.3, "Loop").
//!
//! Each iteration: drain the control channel and the inbound rings (spawns, cancels, shutdown,
//! wakes) and the driver's completions into the run queue; expire timers; run ready tasks to
//! their next await, at most a batch of them; then, if nothing is ready, spin for the configured
//! window while a client is active, and park in the driver until a kick, a completion or the
//! next deadline. Cancellation is a message that guarantees a terminal completion: the future is
//! dropped at the next poll boundary, the task's children are cancelled and joined, and whoever
//! joins it sees `Cancelled`. The watchdog counts polls that exceed the step budget derived from
//! the measured wake cost (a step longer than a peer's wake starves the shard).
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

use crate::control::Control;
use crate::driver::{Completion, Driver, DriverSeed, Kick};
use crate::error::RtError;
use crate::queue::LocalQueue;
use crate::registry::{self, Entry, MAX_SHARDS};
use crate::runtime::RuntimeConfig;
use crate::task::{BoxedFuture, NO_LINK, Outcome, SpawnRequest, State, TaskSlot};
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
  /// Polls longer than the step budget.
  pub long_steps: u64,
  /// The longest poll, in nanoseconds.
  pub longest_step_ns: u64,
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
  /// Idle spins that ended with work arriving.
  pub spin_hits: u64,
  /// Idle spins that ran out and parked.
  pub spin_misses: u64,
  /// Idle spins ended by a timer falling due.
  pub spin_deadlines: u64,
}

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
    kick: Kick,
  ) -> Result<ShardSeed, RtError> {
    let (id, control) = registry::register(config.ring_entries, config.tasks_per_shard, kick)?;
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
  entry: Option<&'static Entry>,
  inner: RefCell<ShardInner>,
}

impl std::fmt::Debug for ShardContext {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ShardContext")
      .field("id", &self.id)
      .finish()
  }
}

impl ShardContext {
  /// Builds the context from its seed on the calling thread (which builds the driver too) and
  /// leaks it: a shard lives for the process (one runtime in production), and the leak is what
  /// lets every reference to it be a plain `&'static` with no unsafe code.
  pub fn build(seed: ShardSeed) -> Result<&'static ShardContext, RtError> {
    let config = seed.config;
    let driver = (seed.driver)()?;
    let mut arena = Slab::new(
      config.tasks_per_shard.min(config.segment_tasks()),
      config.tasks_per_shard,
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
    Ok(Box::leak(Box::new(ShardContext {
      id: seed.id,
      local: LocalQueue::new(config.tasks_per_shard),
      outbound: seed.outbound,
      inbound: seed.inbound,
      current_task: Cell::new(None),
      exited: Cell::new(false),
      active: Cell::new(false),
      pair_full_events: Cell::new(0),
      nested_borrows: Cell::new(0),
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
      }),
    })))
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

  /// The counters.
  pub fn counters(&self) -> Counters {
    let mut c = self.with_inner(|inner| inner.counters).unwrap_or_default();
    c.nested_borrows = self.nested_borrows.get();
    c
  }

  /// The driver's clock.
  pub fn now_ns(&self) -> u64 {
    self.with_inner(|inner| inner.driver.now_ns()).unwrap_or(0)
  }

  /// Times a shard-pair ring was full and the sender spun.
  pub fn pair_full_events(&self) -> u64 {
    self.pair_full_events.get()
  }

  /// Sends a word to `target` over the pair ring, if one exists; kicks the target.
  pub fn send_to(&self, target: u16, word: u64) -> bool {
    let Some(Some(ring)) = self.outbound.get(usize::from(target)) else {
      return false;
    };
    let (mut producer, _) = ring.split();
    let mut pending = word;
    loop {
      match producer.push(pending) {
        Ok(()) => break,
        Err(back) => {
          pending = back;
          self.pair_full_events.set(self.pair_full_events.get() + 1);
          if let Some(entry) = registry::entry(target) {
            entry.kick.kick();
          }
          std::thread::yield_now();
        }
      }
    }
    if let Some(entry) = registry::entry(target) {
      entry.kick.kick();
    }
    true
  }

  // ------------------------------------------------------------------ admission

  /// Admits a spawn request (from any thread's message, or the runtime): detached.
  pub fn spawn_request(&self, request: SpawnRequest) -> Result<TaskId, RtError> {
    let SpawnRequest { future, parent } = request;
    let parent = parent.filter(|p| p.shard() == self.id).map(|p| p.slot());
    self.admit(future, parent, false)
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
    loop {
      let outcome = self.step();
      if outcome.exit {
        break;
      }
      if outcome.did_work {
        continue;
      }
      if self.active.get() && self.spin_until_work(outcome.next_deadline_ns) {
        continue;
      }
      self.park(outcome.next_deadline_ns);
    }
    registry::set_current(None);
    self.exited.set(true);
  }

  /// Spins for the configured window watching the rings and the driver; true when something
  /// arrived or a timer fell due during the spin (either is work for the next step), false when
  /// the window ran out with nothing to do.
  fn spin_until_work(&self, deadline_ns: Option<u64>) -> bool {
    let (spin_ns, now) = self
      .with_inner(|inner| (inner.config.spin_ns, inner.driver.now_ns()))
      .unwrap_or((0, 0));
    if spin_ns == 0 {
      return false;
    }
    let spin_end = now.saturating_add(spin_ns);
    loop {
      if self.has_inbound()
        || self
          .with_inner(|inner| inner.driver.has_pending())
          .unwrap_or(false)
      {
        self.with_inner(|inner| inner.counters.spin_hits += 1);
        return true;
      }
      let now = self.now_ns();
      if deadline_ns.is_some_and(|d| now >= d) {
        self.with_inner(|inner| inner.counters.spin_deadlines += 1);
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
    let mut did_work = false;
    let (drained, batch) = self
      .with_inner(|inner| {
        inner.counters.steps += 1;
        let mut work = self.drain_control(inner);
        work |= self.drain_inbound(inner);
        work |= self.expire_timers(inner);
        (work, inner.config.batch)
      })
      .unwrap_or((false, 1));
    did_work |= drained;
    let ready = self.local.take_ready();
    let batch = batch.max(1);
    for (i, slot) in ready.iter().enumerate() {
      if i < batch {
        did_work = true;
        self.poll_slot(*slot);
      } else {
        self.local.clear_pending(*slot);
        self.local.push(*slot);
      }
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
    StepOutcome {
      did_work,
      next_deadline_ns,
      exit,
    }
  }

  /// Parks in the driver until a kick, a completion or `deadline_ns`.
  pub fn park(&'static self, deadline_ns: Option<u64>) {
    let lost = self
      .with_inner(|inner| {
        inner.counters.waits += 1;
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
      .unwrap_or(false);
    if lost {
      self.fail_all();
    }
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
    let mut any = false;
    let batch = inner.config.batch.max(1);
    for _ in 0..batch {
      let Ok(message) = inner.control.try_recv() else {
        break;
      };
      any = true;
      inner.counters.controls += 1;
      self.handle_control(inner, message);
    }
    any
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
        let SpawnRequest { future, parent } = *request;
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
          }
          Err(_) => inner.counters.admission_refused += 1,
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
    let elapsed = self.now_ns().saturating_sub(start);
    self.current_task.set(None);
    let done = matches!(poll, Poll::Ready(()));
    let dropped =
      self.with_inner(|inner| after_poll(inner, &self.local, slot, future, elapsed, done));
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

/// Records the poll and either stores the future back or finishes the task; returns a future to
/// drop outside the borrow, if any.
fn after_poll(
  inner: &mut ShardInner,
  local: &LocalQueue,
  slot: u32,
  future: BoxedFuture,
  elapsed: u64,
  done: bool,
) -> Option<BoxedFuture> {
  let budget = inner.config.step_budget_ns;
  inner.counters.polls += 1;
  if elapsed > budget {
    inner.counters.long_steps += 1;
  }
  inner.counters.longest_step_ns = inner.counters.longest_step_ns.max(elapsed);
  let Some(handle) = handle_at(&inner.arena, slot) else {
    return Some(future);
  };
  let Ok(task) = inner.arena.get_mut(handle) else {
    return Some(future);
  };
  task.polls += 1;
  if elapsed > budget {
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
