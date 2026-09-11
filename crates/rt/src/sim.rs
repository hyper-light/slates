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

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};

use slates_mem::Encoded;

/// The simulated UDP fabric (§4.10a): a deterministic, in-memory datagram switch so the fleet plane
/// is testable at N=1 without the OS network — the "sim arm first" the design's phasing calls for.
/// It is a thread-local because the simulation runs on one thread (so no `Send`/`Sync`, no lock), and
/// wakes a waiting receiver through the registry, the same path a real driver completion takes.
#[derive(Debug, Default)]
pub struct SimFabric {
  next_port: u16,
  mailboxes: BTreeMap<u16, VecDeque<(Vec<u8>, u16)>>,
  interests: BTreeMap<u16, u64>,
}

impl SimFabric {
  fn new() -> SimFabric {
    SimFabric {
      // Ports start at 1 so 0 stays the "unspecified" address, as in the OS.
      next_port: 1,
      mailboxes: BTreeMap::new(),
      interests: BTreeMap::new(),
    }
  }

  fn bind(&mut self) -> u16 {
    let port = self.next_port;
    self.next_port = self.next_port.saturating_add(1);
    self.mailboxes.entry(port).or_default();
    port
  }

  /// Delivers a datagram to `dest`; returns a waker word to wake if a receiver was waiting on it.
  fn send(&mut self, dest: u16, bytes: &[u8], from: u16) -> Option<u64> {
    self
      .mailboxes
      .entry(dest)
      .or_default()
      .push_back((bytes.to_vec(), from));
    self.interests.remove(&dest)
  }

  fn recv(&mut self, port: u16) -> Option<(Vec<u8>, u16)> {
    self.mailboxes.get_mut(&port)?.pop_front()
  }

  /// Records one-shot read interest; returns a waker word to wake now if a datagram already waits.
  fn register(&mut self, port: u16, word: u64) -> Option<u64> {
    if self.mailboxes.get(&port).is_some_and(|q| !q.is_empty()) {
      return Some(word);
    }
    self.interests.insert(port, word);
    None
  }
}

thread_local! {
  static SIM_FABRIC: RefCell<SimFabric> = RefCell::new(SimFabric::new());
}

/// Resets the thread's simulated UDP fabric (a fresh simulation starts with an empty network).
pub(crate) fn sim_fabric_reset() {
  SIM_FABRIC.with(|f| *f.borrow_mut() = SimFabric::new());
}

/// Shape: the receive buffer a simulated datagram socket reports (`UdpSocket::recv_buffer_bytes`) — the
/// smaller of the default kernel datagram buffers on the machines this runs on (Linux 208 KiB, macOS
/// 768 KiB), so a consumer sized from it in simulation is sized as it would be on the stricter host.
pub const SIM_RECV_BUFFER_BYTES: usize = 208 * 1024;

/// Binds a simulated UDP port on this thread's fabric.
pub fn sim_udp_bind() -> u16 {
  SIM_FABRIC.with(|f| f.borrow_mut().bind())
}

/// Sends a simulated datagram, waking a waiting receiver through the registry.
pub fn sim_udp_send(dest: u16, bytes: &[u8], from: u16) {
  let wake = SIM_FABRIC.with(|f| f.borrow_mut().send(dest, bytes, from));
  if let Some(word) = wake {
    crate::registry::wake(Encoded::from_word(word));
  }
}

/// Receives one simulated datagram, or `None` when the mailbox is empty.
pub fn sim_udp_recv(port: u16) -> Option<(Vec<u8>, u16)> {
  SIM_FABRIC.with(|f| f.borrow_mut().recv(port))
}

/// Registers one-shot read interest, waking now (through the registry) if a datagram already waits.
pub fn sim_udp_register(port: u16, word: u64) {
  let wake = SIM_FABRIC.with(|f| f.borrow_mut().register(port, word));
  if let Some(word) = wake {
    crate::registry::wake(Encoded::from_word(word));
  }
}

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

  fn register_readable(&mut self, raw: i32, user_data: u64) -> Result<(), RtError> {
    // The simulated UDP fabric (§4.10a): the raw handle is a sim port; interest is recorded there and
    // woken through the registry when a datagram arrives — the same wake path a real completion takes.
    let port = u16::try_from(raw).map_err(|_| RtError::DriverRefused {
      call: "register_readable",
      code: None,
    })?;
    sim_udp_register(port, user_data);
    Ok(())
  }

  fn register_writable(&mut self, _raw: i32, _user_data: u64) -> Result<(), RtError> {
    // The simulated fabric is UDP-only and its sends never block (an in-memory push), so there is no
    // write-readiness to await under simulation; TCP is a host-local bridge (§4.6), not the fleet
    // plane (§4.10a). A typed refusal keeps the seam honest rather than pretending readiness.
    Err(RtError::DriverRefused {
      call: "register_writable",
      code: None,
    })
  }

  fn has_pending(&self) -> bool {
    !self.nops.is_empty() || self.shared.kicked.load(Ordering::Acquire)
  }

  fn is_sim(&self) -> bool {
    true
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
    // A fresh simulation starts with an empty UDP fabric on this thread.
    sim_fabric_reset();
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
