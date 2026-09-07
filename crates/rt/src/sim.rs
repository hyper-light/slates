//! The simulation driver and runtime: virtual time, a seeded generator, kicks as flags, and fault
//! injection, so a whole set of shards runs deterministically on one thread (§4.3, D-20).
//!
//! Every simulated shard shares one clock. `SimRuntime::run_until_idle` steps each shard in
//! turn; when all are idle it advances the clock to the earliest deadline any shard asked for,
//! and stops when no shard has a deadline or a message. Fault injection: `kill_driver` makes a
//! shard's next wait fail with `DriverLost`, which the shard answers by cancelling every task
//! with a terminal completion and exiting (T-0.7). The shared state is atomics behind leaked
//! `&'static` references, so a kick is a plain `Copy` handle and nothing here is unsafe.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use slates_machine::stats::Xorshift;

use crate::driver::{Completion, Driver, DriverKind, DriverSeed, Kick};
use crate::error::RtError;
use crate::runtime::RuntimeConfig;
use crate::shard::{ShardContext, ShardId, ShardSeed, StepOutcome, TaskId};
use crate::task::SpawnRequest;

/// Format: the "no deadline requested" sentinel.
const NO_DEADLINE: u64 = u64::MAX;

/// The state a simulated driver shares with its kick and the simulation clock.
#[derive(Debug)]
pub struct SimShared {
  now_ns: AtomicU64,
  kicked: AtomicBool,
  requested_deadline: AtomicU64,
  rng_state: AtomicU64,
  kill_at_wait: AtomicU64,
  waits: AtomicU64,
}

impl SimShared {
  fn new(seed: u64) -> Self {
    Self {
      now_ns: AtomicU64::new(0),
      kicked: AtomicBool::new(false),
      requested_deadline: AtomicU64::new(NO_DEADLINE),
      rng_state: AtomicU64::new(Xorshift::new(seed).next_u64()),
      kill_at_wait: AtomicU64::new(NO_DEADLINE),
      waits: AtomicU64::new(0),
    }
  }

  /// Marks the driver kicked.
  pub fn set_kicked(&self) {
    self.kicked.store(true, Ordering::Release);
  }

  /// Virtual now.
  pub fn now_ns(&self) -> u64 {
    self.now_ns.load(Ordering::Acquire)
  }

  /// The next pseudo-random word from the simulation's seeded generator.
  pub fn next_random(&self) -> u64 {
    let mut rng = Xorshift::new(self.rng_state.load(Ordering::Acquire));
    let value = rng.next_u64();
    self.rng_state.store(value, Ordering::Release);
    value
  }

  fn requested_deadline(&self) -> Option<u64> {
    match self.requested_deadline.load(Ordering::Acquire) {
      NO_DEADLINE => None,
      d => Some(d),
    }
  }
}

/// The simulation driver of one shard.
#[derive(Debug)]
pub struct SimDriver {
  shared: &'static SimShared,
  clock: &'static SimShared,
  nops: Vec<u64>,
}

impl Driver for SimDriver {
  fn kind(&self) -> DriverKind {
    DriverKind::Simulation
  }

  fn kick_handle(&self) -> Kick {
    Kick::Sim(self.shared)
  }

  fn now_ns(&self) -> u64 {
    self.clock.now_ns()
  }

  fn wait(&mut self, timeout_ns: Option<u64>, out: &mut Vec<Completion>) -> Result<(), RtError> {
    let waits = self.shared.waits.fetch_add(1, Ordering::AcqRel) + 1;
    if waits >= self.shared.kill_at_wait.load(Ordering::Acquire) {
      return Err(RtError::DriverLost);
    }
    out.extend(self.nops.drain(..).map(|user_data| Completion {
      user_data,
      result: 0,
    }));
    if self.shared.kicked.swap(false, Ordering::AcqRel) || !out.is_empty() {
      self
        .shared
        .requested_deadline
        .store(NO_DEADLINE, Ordering::Release);
      return Ok(());
    }
    // Nothing to deliver: record the deadline for the simulation clock and return without
    // blocking; the runtime advances time when every shard is idle.
    let deadline = timeout_ns.map_or(NO_DEADLINE, |t| self.now_ns().saturating_add(t));
    self
      .shared
      .requested_deadline
      .store(deadline, Ordering::Release);
    Ok(())
  }

  fn submit_nop(&mut self, user_data: u64) -> Result<(), RtError> {
    self.nops.push(user_data);
    Ok(())
  }

  fn register_readable(&mut self, _raw: i32, _user_data: u64) -> Result<(), RtError> {
    // Owed (§4.10a): this completion-native driver does not carry socket readiness yet; a
    // typed refusal, never a silent drop. The readiness-native drivers (kqueue, epoll) do.
    Err(RtError::DriverRefused {
      call: "register_readable",
      code: None,
    })
  }

  fn has_pending(&self) -> bool {
    !self.nops.is_empty() || self.shared.kicked.load(Ordering::Acquire)
  }
}

/// A set of simulated shards on the calling thread.
pub struct SimRuntime {
  clock: &'static SimShared,
  shards: Vec<&'static ShardContext>,
  shared: Vec<&'static SimShared>,
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
    let clock: &'static SimShared = Box::leak(Box::new(SimShared::new(seed)));
    let mut seeds = Vec::new();
    let mut shared = Vec::new();
    for _ in 0..config.shards {
      let s: &'static SimShared = Box::leak(Box::new(SimShared::new(seed)));
      let seed: DriverSeed = Box::new(move || {
        Ok(Box::new(SimDriver {
          shared: s,
          clock,
          nops: Vec::new(),
        }) as Box<dyn Driver>)
      });
      seeds.push(ShardSeed::register(config, seed, Kick::Sim(s))?);
      shared.push(s);
    }
    crate::runtime::connect_pairs(&mut seeds)?;
    let shards = seeds
      .into_iter()
      .map(ShardContext::build)
      .collect::<Result<Vec<_>, _>>()?;
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
    self.clock.now_ns()
  }

  /// Spawns a detached task on a shard.
  pub fn spawn_on(
    &mut self,
    shard: ShardId,
    future: impl std::future::Future<Output = ()> + Send + 'static,
  ) -> Result<TaskId, RtError> {
    let ctx = self.context(shard)?;
    ctx.spawn_request(SpawnRequest::new(Box::pin(future), None))
  }

  /// Makes a shard's driver fail at its `nth` wait from now (1 = the very next one).
  pub fn kill_driver(&mut self, shard: ShardId, nth: u64) -> Result<(), RtError> {
    let index = self.index_of(shard)?;
    let shared = self.shared[index];
    shared.kill_at_wait.store(
      shared.waits.load(Ordering::Acquire) + nth,
      Ordering::Release,
    );
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
        let ctx = self.shards[index];
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
        let shared = self.shared[index];
        if let Some(deadline) = shared.requested_deadline() {
          earliest = Some(earliest.map_or(deadline, |e| e.min(deadline)));
        }
        if shared.kicked.load(Ordering::Acquire) {
          any_work = true;
        }
      }
      if any_work {
        continue;
      }
      match earliest {
        Some(deadline) => {
          if deadline > self.clock.now_ns() {
            self.clock.now_ns.store(deadline, Ordering::Release);
          }
        }
        None => break,
      }
    }
    steps
  }

  /// Advances the clock by `ns` without running anything (a pause in the story).
  pub fn advance(&mut self, ns: u64) {
    let now = self.clock.now_ns();
    self
      .clock
      .now_ns
      .store(now.saturating_add(ns), Ordering::Release);
  }

  /// A shard's context, for counters and joins in tests.
  pub fn context(&self, shard: ShardId) -> Result<&'static ShardContext, RtError> {
    let index = self.index_of(shard)?;
    self
      .shards
      .get(index)
      .copied()
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
