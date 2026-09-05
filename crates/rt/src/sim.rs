//! The simulation driver and runtime: virtual time, a seeded generator, kicks as flags, and fault
//! injection, so a whole set of shards runs deterministically on one thread (§4.3, D-20).
//!
//! Every simulated shard shares one clock. `SimRuntime::run_until_idle` steps each shard in
//! turn; when all are idle it advances the clock to the earliest deadline any shard asked for,
//! and stops when no shard has a deadline or a message. Fault injection: `kill_driver` makes a
//! shard's next wait fail with `DriverLost`, which the shard answers by cancelling every task
//! with a terminal completion and exiting (T-0.7).

use std::cell::Cell;
use std::ptr::NonNull;

use slates_machine::stats::Xorshift;

use crate::driver::{Completion, Driver, DriverKind, Kick};
use crate::error::RtError;
use crate::runtime::RuntimeConfig;
use crate::shard::{ShardContext, ShardId, StepOutcome, TaskId};
use crate::task::SpawnRequest;

/// The state a simulated driver shares with its kick and the simulation clock.
pub struct SimShared {
  now_ns: Cell<u64>,
  kicked: Cell<bool>,
  requested_deadline: Cell<Option<u64>>,
  nops: Cell<Vec<u64>>,
  rng_state: Cell<u64>,
  kill_at_wait: Cell<Option<u64>>,
  waits: Cell<u64>,
}

impl std::fmt::Debug for SimShared {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("SimShared")
      .field("now_ns", &self.now_ns.get())
      .field("kicked", &self.kicked.get())
      .field("waits", &self.waits.get())
      .finish()
  }
}

impl SimShared {
  fn new(seed: u64) -> Self {
    Self {
      now_ns: Cell::new(0),
      kicked: Cell::new(false),
      requested_deadline: Cell::new(None),
      nops: Cell::new(Vec::new()),
      rng_state: Cell::new(Xorshift::new(seed).next_u64()),
      kill_at_wait: Cell::new(None),
      waits: Cell::new(0),
    }
  }

  /// Marks the driver kicked.
  pub fn set_kicked(&self) {
    self.kicked.set(true);
  }

  /// Virtual now.
  pub fn now_ns(&self) -> u64 {
    self.now_ns.get()
  }

  /// The next pseudo-random word from the simulation's seeded generator.
  pub fn next_random(&self) -> u64 {
    let mut rng = Xorshift::new(self.rng_state.get());
    let value = rng.next_u64();
    self.rng_state.set(value);
    value
  }
}

/// The simulation driver of one shard.
#[derive(Debug)]
pub struct SimDriver {
  shared: NonNull<SimShared>,
  clock: NonNull<SimShared>,
}

impl SimDriver {
  fn shared(&self) -> &SimShared {
    // SAFETY: leaked for the process; the simulation is single-threaded.
    unsafe { self.shared.as_ref() }
  }

  fn clock(&self) -> &SimShared {
    // SAFETY: as above.
    unsafe { self.clock.as_ref() }
  }
}

impl Driver for SimDriver {
  fn kind(&self) -> DriverKind {
    DriverKind::Simulation
  }

  fn kick_handle(&self) -> Kick {
    Kick::Sim(self.shared)
  }

  fn now_ns(&self) -> u64 {
    self.clock().now_ns()
  }

  fn wait(&mut self, timeout_ns: Option<u64>, out: &mut Vec<Completion>) -> Result<(), RtError> {
    let shared = self.shared();
    let waits = shared.waits.get() + 1;
    shared.waits.set(waits);
    if shared.kill_at_wait.get().is_some_and(|at| waits >= at) {
      return Err(RtError::DriverLost);
    }
    let nops = shared.nops.take();
    for user_data in nops {
      out.push(Completion {
        user_data,
        result: 0,
      });
    }
    if shared.kicked.replace(false) || !out.is_empty() {
      shared.requested_deadline.set(None);
      return Ok(());
    }
    // Nothing to deliver: record the deadline for the simulation clock and return without
    // blocking; the runtime advances time when every shard is idle.
    shared
      .requested_deadline
      .set(timeout_ns.map(|t| self.now_ns().saturating_add(t)));
    Ok(())
  }

  fn submit_nop(&mut self, user_data: u64) -> Result<(), RtError> {
    let shared = self.shared();
    let mut nops = shared.nops.take();
    nops.push(user_data);
    shared.nops.set(nops);
    Ok(())
  }

  fn has_pending(&self) -> bool {
    let shared = self.shared();
    let nops = shared.nops.take();
    let pending = !nops.is_empty();
    shared.nops.set(nops);
    pending || shared.kicked.get()
  }
}

/// A set of simulated shards on the calling thread.
pub struct SimRuntime {
  clock: NonNull<SimShared>,
  // Boxed on purpose: the registry's thread-local points at a context during a step, so each
  // context's address must never move.
  #[allow(clippy::vec_box)]
  shards: Vec<Box<ShardContext>>,
  shared: Vec<NonNull<SimShared>>,
}

impl std::fmt::Debug for SimRuntime {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("SimRuntime")
      .field("shards", &self.shards.len())
      .finish()
  }
}

impl SimRuntime {
  /// Builds `config.shards` simulated shards sharing one clock seeded by `seed`.
  pub fn new(config: &RuntimeConfig, seed: u64) -> Result<SimRuntime, RtError> {
    let clock = NonNull::from(Box::leak(Box::new(SimShared::new(seed))));
    let mut shards = Vec::new();
    let mut shared = Vec::new();
    let mut ids = Vec::new();
    for _ in 0..config.shards {
      let s = NonNull::from(Box::leak(Box::new(SimShared::new(seed))));
      let driver = SimDriver { shared: s, clock };
      let ctx = ShardContext::new(config, Box::new(driver))?;
      ids.push(ctx.id);
      shards.push(ctx);
      shared.push(s);
    }
    crate::runtime::connect_pairs(&mut shards, &ids)?;
    Ok(SimRuntime {
      clock,
      shards,
      shared,
    })
  }

  /// The shard ids, in order.
  pub fn shard_ids(&self) -> Vec<ShardId> {
    self.shards.iter().map(|s| ShardId(s.id)).collect()
  }

  /// Virtual now.
  pub fn now_ns(&self) -> u64 {
    // SAFETY: leaked for the process; single-threaded.
    unsafe { self.clock.as_ref() }.now_ns()
  }

  /// Spawns a detached task on a shard.
  pub fn spawn_on(
    &mut self,
    shard: ShardId,
    future: impl std::future::Future<Output = ()> + Send + 'static,
  ) -> Result<TaskId, RtError> {
    let ctx = self.context_mut(shard)?;
    ctx.spawn_request(SpawnRequest::new(Box::pin(future), None))
  }

  /// Makes a shard's driver fail at its `nth` wait from now (1 = the very next one).
  pub fn kill_driver(&mut self, shard: ShardId, nth: u64) -> Result<(), RtError> {
    let index = self.index_of(shard)?;
    // SAFETY: leaked for the process; single-threaded.
    let shared = unsafe { self.shared[index].as_ref() };
    shared.kill_at_wait.set(Some(shared.waits.get() + nth));
    Ok(())
  }

  /// Steps every shard until none has work, advancing virtual time to the earliest deadline
  /// whenever all are idle. Returns the number of steps taken.
  pub fn run_until_idle(&mut self) -> u64 {
    let mut steps = 0u64;
    loop {
      let mut any_work = false;
      let mut earliest: Option<u64> = None;
      for index in 0..self.shards.len() {
        let ctx = &self.shards[index];
        if ctx.exited() {
          continue;
        }
        let outcome: StepOutcome = ctx.step();
        steps += 1;
        if outcome.did_work {
          any_work = true;
          continue;
        }
        ctx.park(outcome.next_deadline_ns);
        // SAFETY: leaked for the process; single-threaded.
        let shared = unsafe { self.shared[index].as_ref() };
        if let Some(deadline) = shared.requested_deadline.get() {
          earliest = Some(earliest.map_or(deadline, |e| e.min(deadline)));
        }
        if shared.kicked.get() {
          any_work = true;
        }
      }
      if any_work {
        continue;
      }
      match earliest {
        Some(deadline) => {
          // SAFETY: leaked for the process; single-threaded.
          let clock = unsafe { self.clock.as_ref() };
          if deadline > clock.now_ns() {
            clock.now_ns.set(deadline);
          }
        }
        None => break,
      }
    }
    steps
  }

  /// Advances the clock by `ns` without running anything (a pause in the story).
  pub fn advance(&mut self, ns: u64) {
    // SAFETY: leaked for the process; single-threaded.
    let clock = unsafe { self.clock.as_ref() };
    clock.now_ns.set(clock.now_ns().saturating_add(ns));
  }

  /// A shard's context, for counters and joins in tests.
  pub fn context(&self, shard: ShardId) -> Result<&ShardContext, RtError> {
    let index = self.index_of(shard)?;
    self
      .shards
      .get(index)
      .map(|b| &**b)
      .ok_or(RtError::NotOnShardThread)
  }

  fn context_mut(&mut self, shard: ShardId) -> Result<&mut ShardContext, RtError> {
    let index = self.index_of(shard)?;
    self
      .shards
      .get_mut(index)
      .map(|b| &mut **b)
      .ok_or(RtError::NotOnShardThread)
  }

  fn index_of(&self, shard: ShardId) -> Result<usize, RtError> {
    self
      .shards
      .iter()
      .position(|c| c.id == shard.0)
      .ok_or(RtError::NotOnShardThread)
  }
}
