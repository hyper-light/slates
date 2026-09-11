//! A registered poller is woken by the idle spin as it is by a step (§4.3 "drain inbound rings";
//! §4.7 "a parked shard is woken by the driver kick"): the daemon's control loop registers a poller
//! whose readiness check **consumes** its signal (the doorbell flag is swapped to false when asked),
//! so whoever asks must wake the task — a spin that asks and drops the answer loses the ring, and a
//! client's claim then waits until another client rings or its own claim wait runs out.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use slates_rt::futures;
use slates_rt::registry;
use slates_rt::runtime::{Runtime, RuntimeConfig};

/// Shape: the idle spin window — long enough that a ring sent the instant the poller task goes idle
/// lands inside the spin, not after the park, even on a loaded machine (the timing the test asserts
/// with `spin_hits`).
const SPIN_NS: u64 = 2_000_000_000;
/// Shape: how long the ring gets to wake the poller before the test fails — far past a step, far
/// below the client's one-second claim wait the lost ring would otherwise be paid in.
const WAKE_DEADLINE: Duration = Duration::from_secs(2);
/// Shape: how long the shard gets to finish the step that polled the task and settle into its idle
/// spin before the ring is sent (a step is microseconds; a generous margin on a loaded machine), far
/// inside the spin window — so the ring lands in the spin, not in the step before it.
const SETTLE: Duration = Duration::from_millis(50);

fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 1,
    tasks_per_shard: 64,
    timers_per_shard: 64,
    ring_entries: 64,
    step_budget_ns: 1_000_000_000,
    timer_tick_ns: 100_000,
    batch: 64,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: SPIN_NS,
  }
}

/// The doorbell: rung by a client (set), consumed by the poller's question (swapped to false).
static RUNG: AtomicBool = AtomicBool::new(false);
/// How many times the doorbell task was polled awake: once at its start, once per served ring.
static SERVED: AtomicU64 = AtomicU64::new(0);

/// The daemon's control loop in miniature: register as a poller with a consuming question, then
/// idle until woken, counting each wake.
async fn doorbell_loop() {
  if let Some(task) = futures::current_task() {
    registry::with_current(|ctx| {
      ctx.register_poller(task, Box::new(|| RUNG.swap(false, Ordering::AcqRel)))
    })
    .unwrap()
    .unwrap();
  }
  loop {
    SERVED.fetch_add(1, Ordering::AcqRel);
    futures::idle().await;
  }
}

fn wait_until(deadline: Duration, done: impl Fn() -> bool) -> bool {
  let started = Instant::now();
  while started.elapsed() < deadline {
    if done() {
      return true;
    }
    std::thread::yield_now();
  }
  done()
}

/// A ring that lands while the shard is idle-spinning wakes the consuming poller: the spin saw it
/// (`spin_hits` moved — the ring landed in the spin, not after the park) and the task was woken
/// (`poller_wakes` moved, `SERVED` advanced), within the wake deadline. The ring is sent as the
/// daemon's doorbell thread sends it: the flag set, then the shard kicked (a spinning shard needs no
/// kick; a parked one does).
#[test]
fn a_ring_during_the_idle_spin_wakes_a_consuming_poller() {
  let rt = Runtime::start(&config()).unwrap();
  let shard = rt.shard_ids()[0];
  rt.set_active(shard, true).unwrap();
  rt.spawn_on(shard, doorbell_loop()).unwrap();
  assert!(
    wait_until(WAKE_DEADLINE, || SERVED.load(Ordering::Acquire) >= 1),
    "the doorbell task ran once and went idle"
  );
  // Let the step that polled the task end and the shard, active with nothing to do, settle into its
  // spin; then ring.
  let _ = wait_until(SETTLE, || false);
  RUNG.store(true, Ordering::Release);
  if let Some(entry) = registry::entry(shard.0) {
    entry.kick.kick();
  }
  let woken = wait_until(WAKE_DEADLINE, || SERVED.load(Ordering::Acquire) >= 2);
  let counters = rt.shutdown();
  let shard_counters = &counters[0];
  assert!(
    shard_counters.spin_hits >= 1,
    "the ring landed inside the idle spin (spin hits {}); the test's timing premise",
    shard_counters.spin_hits
  );
  assert!(
    woken,
    "the ring during the spin woke the poller: served {} times, poller wakes {}",
    SERVED.load(Ordering::Acquire),
    shard_counters.poller_wakes
  );
  assert!(
    shard_counters.poller_wakes >= 1,
    "non-vacuity: the loop woke the poller, not a foreign wake"
  );
}
