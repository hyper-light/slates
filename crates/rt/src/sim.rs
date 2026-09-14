//! The simulation driver and runtime: virtual time, a seeded generator, kicks as flags, and fault
//! injection, so a whole set of shards runs deterministically on one thread (§4.3, D-20).
//!
//! Every simulated shard shares one clock. `SimRuntime::run_until_idle` steps each shard in
//! turn; when all are idle it advances the clock to the earliest deadline any shard asked for,
//! and stops when no shard has a deadline or a message. Fault injection: `kill_driver` makes a
//! shard's next wait fail with `DriverLost`, which the shard answers by cancelling every task
//! with a terminal completion and exiting (T-0.7). The shared state is atomics behind leaked
//! `&'static` references, so a kick is a plain `Copy` handle and nothing here is unsafe.
//!
//! The simulated UDP fabric below carries the fleet plane at N=1 and, since 2026-09-14, models a path's
//! latency ([`SimDelay`]: a one-way delay with seeded jitter, in order per flow unless told otherwise) so
//! the consensus timing rules of §4.8 — "election timeout ≥ 10 × broadcast RTT p99" — are provable on a
//! WAN profile under this virtual clock rather than only on a loopback where every round trip sits inside
//! one heartbeat (`docs/wip/wan-timeout.md`).

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

/// The latency profile of a modelled path on the simulated fabric (§4.8 A-9's required evidence:
/// "independently delayed … and reordered messages"): a one-way delay, a symmetric jitter around it drawn
/// from the simulation's seeded generator, and whether the path may hand a later datagram of one flow to
/// its receiver before an earlier one. The fabric's default is the zero path — no delay, no jitter — on
/// which a datagram is delivered the instant it is sent, exactly as the fabric always did (the model is
/// additive: every simulation that never sets a profile runs unchanged, R8). A profile is data describing
/// the network under test, never a mode of the code that runs over it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SimDelay {
  /// The one-way delay of the path, nanoseconds.
  one_way_ns: u64,
  /// The half-width of the jitter, nanoseconds: each datagram's flight is `one_way ± jitter`, uniform.
  jitter_ns: u64,
  /// Whether a datagram may overtake an earlier one on the same directed flow. `false` — the path queues
  /// in order: an arrival is clamped to no earlier than the previous datagram's on that flow, so jitter
  /// spreads the gaps but never reorders. `true` — the jitter alone decides, so a later datagram can land
  /// first, the reordering a consumer's dedup and reorder paths are exercised against.
  reorders: bool,
}

impl SimDelay {
  /// The zero path: delivered at once (the fabric's default).
  pub const NONE: SimDelay = SimDelay {
    one_way_ns: 0,
    jitter_ns: 0,
    reorders: false,
  };

  /// A path of `one_way_ns ± jitter_ns` that keeps each flow in send order.
  pub const fn in_order(one_way_ns: u64, jitter_ns: u64) -> SimDelay {
    SimDelay {
      one_way_ns,
      jitter_ns,
      reorders: false,
    }
  }

  /// A path of `one_way_ns ± jitter_ns` whose jitter may reorder a flow.
  pub const fn reordering(one_way_ns: u64, jitter_ns: u64) -> SimDelay {
    SimDelay {
      one_way_ns,
      jitter_ns,
      reorders: true,
    }
  }

  /// The one-way delay this profile centres on.
  pub fn one_way_ns(&self) -> u64 {
    self.one_way_ns
  }

  /// The half-width of this profile's jitter.
  pub fn jitter_ns(&self) -> u64 {
    self.jitter_ns
  }

  /// One datagram's flight time: `one_way − jitter + U[0, 2·jitter]` from `rng` (uniform over the
  /// symmetric span; saturating at zero), or exactly the one-way delay when there is no jitter — so a
  /// jitter-free profile draws nothing and leaves the generator untouched.
  fn draw(&self, rng: &mut Xorshift) -> u64 {
    if self.jitter_ns == 0 {
      return self.one_way_ns;
    }
    let span = self.jitter_ns.saturating_mul(2).saturating_add(1);
    let offset = u64::try_from(rng.below(usize::try_from(span).unwrap_or(usize::MAX))).unwrap_or(0);
    self
      .one_way_ns
      .saturating_sub(self.jitter_ns)
      .saturating_add(offset)
  }
}

/// A datagram the fabric holds until its arrival time.
#[derive(Debug)]
struct InFlight {
  dest: u16,
  bytes: Vec<u8>,
  from: u16,
}

/// The simulated UDP fabric (§4.10a): a deterministic, in-memory datagram switch so the fleet plane
/// is testable at N=1 without the OS network — the "sim arm first" the design's phasing calls for.
/// It is a thread-local because the simulation runs on one thread (so no `Send`/`Sync`, no lock), and
/// wakes a waiting receiver through the registry, the same path a real driver completion takes.
///
/// Latency (§4.8 A-9): every send is timed against the simulation clock the runtime installs
/// ([`sim_fabric_reset`]) and the path's [`SimDelay`] — the directed pair's override, else the fabric-wide
/// profile. A datagram whose arrival is now (the zero path) goes straight to its mailbox; one whose arrival
/// is later waits in flight, ordered by arrival, and the simulation loop hands it over — and wakes its
/// receiver — once the clock reaches it ([`SimRuntime::run_until_idle`] also treats the earliest arrival
/// as a deadline the clock may advance to, so a fleet whose only pending event is a datagram in flight
/// proceeds). With no clock installed (a fabric used without a `SimRuntime`) "now" is zero and only the
/// zero path delivers — the latency model needs the clock that drives it.
#[derive(Debug)]
pub struct SimFabric {
  next_port: u16,
  mailboxes: BTreeMap<u16, VecDeque<(Vec<u8>, u16)>>,
  interests: BTreeMap<u16, u64>,
  /// The simulation clock in-flight datagrams are timed against; `None` until a runtime installs one.
  clock: Option<&'static SimShared>,
  /// The seeded generator the jitter is drawn from — the fabric's own stream, so a profile's draws never
  /// perturb the shards' generators and a run replays exactly from its seed.
  rng: Xorshift,
  /// The profile of every path without a directed override.
  default_delay: SimDelay,
  /// Directed overrides, keyed `(from, dest)`.
  pair_delays: BTreeMap<(u16, u16), SimDelay>,
  /// Datagrams not yet arrived, keyed by arrival time then send sequence (so two arrivals at one instant
  /// keep send order and never collide).
  in_flight: BTreeMap<(u64, u64), InFlight>,
  next_sequence: u64,
  /// The latest arrival scheduled on each directed flow, the floor an in-order path clamps the next to.
  last_arrival: BTreeMap<(u16, u16), u64>,
}

/// Format: the salt that separates the fabric's generator stream from the shards' (both are seeded from
/// the runtime seed; `xorshift64*` seeded identically would draw identical words), a fixed odd word.
const FABRIC_SEED_SALT: u64 = 0xD1B5_4A32_D192_ED03;

impl SimFabric {
  fn new(clock: Option<&'static SimShared>, seed: u64) -> SimFabric {
    SimFabric {
      // Ports start at 1 so 0 stays the "unspecified" address, as in the OS.
      next_port: 1,
      mailboxes: BTreeMap::new(),
      interests: BTreeMap::new(),
      clock,
      rng: Xorshift::new(seed ^ FABRIC_SEED_SALT),
      default_delay: SimDelay::NONE,
      pair_delays: BTreeMap::new(),
      in_flight: BTreeMap::new(),
      next_sequence: 0,
      last_arrival: BTreeMap::new(),
    }
  }

  fn bind(&mut self) -> u16 {
    let port = self.next_port;
    self.next_port = self.next_port.saturating_add(1);
    self.mailboxes.entry(port).or_default();
    port
  }

  /// Virtual now on the installed clock, or zero without one.
  fn now_ns(&self) -> u64 {
    self.clock.map_or(0, SimShared::now_ns)
  }

  /// Sends a datagram from `from` to `dest`: delivered now on the zero path (returning a waker word to
  /// wake if a receiver was waiting), or scheduled for its drawn arrival on a delayed one.
  fn send(&mut self, dest: u16, bytes: &[u8], from: u16) -> Option<u64> {
    let now = self.now_ns();
    let profile = self
      .pair_delays
      .get(&(from, dest))
      .copied()
      .unwrap_or(self.default_delay);
    let mut arrival = now.saturating_add(profile.draw(&mut self.rng));
    if !profile.reorders
      && let Some(previous) = self.last_arrival.get(&(from, dest))
    {
      arrival = arrival.max(*previous);
    }
    self.last_arrival.insert((from, dest), arrival);
    if arrival <= now {
      return self.deliver(dest, bytes.to_vec(), from);
    }
    let sequence = self.next_sequence;
    self.next_sequence = self.next_sequence.saturating_add(1);
    self.in_flight.insert(
      (arrival, sequence),
      InFlight {
        dest,
        bytes: bytes.to_vec(),
        from,
      },
    );
    None
  }

  /// Puts a datagram in `dest`'s mailbox; returns the waker word of a receiver waiting on it.
  fn deliver(&mut self, dest: u16, bytes: Vec<u8>, from: u16) -> Option<u64> {
    self
      .mailboxes
      .entry(dest)
      .or_default()
      .push_back((bytes, from));
    self.interests.remove(&dest)
  }

  /// Hands over every in-flight datagram whose arrival is at or before `now`, in arrival order, and
  /// returns the waker words of the receivers waiting on them.
  fn deliver_due(&mut self, now: u64) -> Vec<u64> {
    let mut wakes = Vec::new();
    while let Some(entry) = self.in_flight.first_entry() {
      if entry.key().0 > now {
        break;
      }
      let InFlight { dest, bytes, from } = entry.remove();
      if let Some(word) = self.deliver(dest, bytes, from) {
        wakes.push(word);
      }
    }
    wakes
  }

  /// The earliest arrival still in flight, if any — a deadline the simulation clock may advance to.
  fn earliest_arrival(&self) -> Option<u64> {
    self.in_flight.keys().next().map(|(arrival, _)| *arrival)
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
  static SIM_FABRIC: RefCell<SimFabric> = RefCell::new(SimFabric::new(None, 0));
}

/// Resets the thread's simulated UDP fabric for a fresh simulation: an empty network on the zero path,
/// timed against `clock` and drawing its jitter from `seed`.
pub(crate) fn sim_fabric_reset(clock: &'static SimShared, seed: u64) {
  SIM_FABRIC.with(|f| *f.borrow_mut() = SimFabric::new(Some(clock), seed));
}

/// Sets the latency profile of every path on this thread's fabric that has no directed override — the
/// whole modelled network at one profile (every node in its own region, the worst case for a council).
/// Takes effect for datagrams sent from now on; call it after `SimRuntime::new` (which resets the fabric)
/// and before the tasks that send.
pub fn sim_udp_set_delay(delay: SimDelay) {
  SIM_FABRIC.with(|f| f.borrow_mut().default_delay = delay);
}

/// Sets the latency profile of the directed path from port `from` to port `dest`, overriding the fabric's
/// default for that pair only — a near pair inside a far fleet, or an asymmetric route.
pub fn sim_udp_set_pair_delay(from: u16, dest: u16, delay: SimDelay) {
  SIM_FABRIC.with(|f| {
    f.borrow_mut().pair_delays.insert((from, dest), delay);
  });
}

/// Hands over every in-flight datagram due by `now` and wakes the receivers waiting on them.
fn sim_fabric_deliver_due(now: u64) {
  let wakes = SIM_FABRIC.with(|f| f.borrow_mut().deliver_due(now));
  for word in wakes {
    crate::registry::wake(Encoded::from_word(word));
  }
}

/// The earliest arrival still in flight on this thread's fabric.
fn sim_fabric_earliest_arrival() -> Option<u64> {
  SIM_FABRIC.with(|f| f.borrow().earliest_arrival())
}

/// Shape: the receive buffer a simulated datagram socket reports (`UdpSocket::recv_buffer_bytes`) — the
/// smaller of the default kernel datagram buffers on the machines this runs on (Linux 208 KiB, macOS
/// 768 KiB), so a consumer sized from it in simulation is sized as it would be on the stricter host.
pub const SIM_RECV_BUFFER_BYTES: usize = 208 * 1024;

/// Binds a simulated UDP port on this thread's fabric.
pub fn sim_udp_bind() -> u16 {
  SIM_FABRIC.with(|f| f.borrow_mut().bind())
}

/// Sends a simulated datagram: on the zero path it is delivered at once and a waiting receiver is woken
/// through the registry; on a delayed path it is scheduled for its drawn arrival and handed over by the
/// simulation loop when the clock reaches it.
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

impl Drop for SimRuntime {
  /// The simulation's shards were built and stepped on this thread; every `run_until_*` returned
  /// before this drop. Each context is freed (`reclaim_context`, which also clears the thread's
  /// current-context cell a bare step left pointing at it) and its slot given back — before this, a
  /// simulation reclaimed nothing (its slots were never unregistered, so a test binary spent one of
  /// the registry's slots per simulation for good). The shared clock and per-shard flags are still
  /// leaked: a retired entry's `Kick::Sim` points at the flag and may be kicked by a stale waker
  /// until the slot's next registration, so they must outlive the entry — the same treatment as the
  /// kick descriptor is owed to them (recorded, small: a few atomics each).
  fn drop(&mut self) {
    for ctx in &self.shards {
      let id = ctx.id;
      crate::registry::note_arena_generation(id, ctx.arena_generation_high());
      crate::registry::reclaim_context(id);
      crate::registry::unregister(id);
    }
  }
}

impl SimRuntime {
  /// Builds `config.shards` simulated shards sharing one clock seeded by `seed`.
  pub fn new(config: &RuntimeConfig, seed: u64) -> Result<SimRuntime, RtError> {
    let clock: &'static SimShared = Box::leak(Box::new(SimShared::new(seed)));
    // A fresh simulation starts with an empty UDP fabric on this thread, timed against this clock.
    sim_fabric_reset(clock, seed);
    let mut seeds = Vec::new();
    let mut shared = Vec::new();
    for _ in 0..config.shards {
      let s: &'static SimShared = Box::leak(Box::new(SimShared::new(seed)));
      let seed: DriverSeed = Box::new(move |_kick| {
        Ok(Box::new(SimDriver {
          shared: s,
          clock,
          nops: Vec::new(),
        }) as Box<dyn Driver>)
      });
      seeds.push(ShardSeed::register(
        config,
        seed,
        crate::registry::RegisterKick::Kick(Kick::Sim(s)),
      )?);
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

  /// Steps every shard until none has work, advancing virtual time to the earliest deadline —
  /// a shard's timer or a datagram's arrival on the fabric — whenever all are idle. Datagrams due by
  /// the current time are handed over (and their receivers woken) before each pass, so a delayed
  /// datagram is received exactly at its arrival time. Returns the number of steps taken.
  pub fn run_until_idle(&mut self) -> u64 {
    let mut steps = 0u64;
    loop {
      sim_fabric_deliver_due(self.clock.now_ns());
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
      // A datagram in flight is a pending event too: the clock may advance to its arrival.
      if let Some(arrival) = sim_fabric_earliest_arrival() {
        earliest = Some(earliest.map_or(arrival, |e| e.min(arrival)));
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
