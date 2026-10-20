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
    wake_tracking: None,
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
  // A client's earlier request opened the shard's idle window, as the server notes one it served.
  rt.spawn_on(shard, async {
    registry::with_current(|ctx| ctx.note_activity());
  })
  .unwrap();
  rt.spawn_on(shard, doorbell_loop()).unwrap();
  assert!(
    wait_until(WAKE_DEADLINE, || SERVED.load(Ordering::Acquire) >= 1),
    "the doorbell task ran once and went idle"
  );
  // Let the step that polled the task end and the shard, inside its window with nothing to do, settle
  // into its spin; then ring.
  let _ = wait_until(SETTLE, || false);
  RUNG.store(true, Ordering::Release);
  let _ = registry::with_entry(shard.0, |entry| entry.kick.kick());
  let woken = wait_until(WAKE_DEADLINE, || SERVED.load(Ordering::Acquire) >= 2);
  let counters = rt.shutdown().unwrap();
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

/// How many times the still-living task's poller was asked — the liveness control below.
static LIVE_ASKED: AtomicU64 = AtomicU64::new(0);
/// How many times the ended task's poller was asked. Must stop climbing once its task is gone.
static GONE_ASKED: AtomicU64 = AtomicU64::new(0);

/// Registers a poller and stays alive, never woken: the control that proves the shard is still
/// sweeping pollers while the assertion below watches the other counter stand still.
async fn live_poller_task() {
  if let Some(task) = futures::current_task() {
    let _ = registry::with_current(|ctx| {
      ctx.register_poller(
        task,
        Box::new(|| {
          LIVE_ASKED.fetch_add(1, Ordering::AcqRel);
          false
        }),
      )
    });
  }
  loop {
    futures::idle().await;
  }
}

/// Shape: how many steps the ending task lives for before it returns. It has to outlive at least one
/// sweep or its poller is reaped before it is ever asked and the test cannot tell "forgotten" from
/// "never registered" — measured on the first run, where returning immediately left the counter at 0
/// while the control poller was asked three million times. A yield re-queues the task, and every step
/// sweeps, so a handful is ample; 64 leaves margin on a loaded machine without lengthening the test.
const GONE_TASK_STEPS: u32 = 64;

/// Registers a poller, lives a few steps so the sweep actually asks it, and then **ends**. The poller
/// must not outlive the task that owns it.
async fn gone_poller_task() {
  if let Some(task) = futures::current_task() {
    let _ = registry::with_current(|ctx| {
      ctx.register_poller(
        task,
        Box::new(|| {
          GONE_ASKED.fetch_add(1, Ordering::AcqRel);
          false
        }),
      )
    });
  }
  for _ in 0..GONE_TASK_STEPS {
    futures::yield_now().await;
  }
}

/// Keeps the shard stepping, so `wake_ready_pollers` keeps running for the duration of the test. Without
/// it the shard parks and *every* poller stops being asked, which would make the assertion below pass for
/// the wrong reason.
async fn busy_task() {
  loop {
    futures::yield_now().await;
  }
}

/// §4.3, CLAUDE.md #8 (every structure bounded) — a poller is forgotten when its task ends. Do: on a shard
/// kept stepping, register one poller from a task that stays alive and one from a task that returns
/// immediately; once both have been asked, let the shard sweep and compare. Expect: the living task's
/// poller keeps being asked while the ended task's is never asked again.
///
/// This is what bounds `ShardInner::pollers`, which `register_poller` pushes to with no bound of its own:
/// the sweep in `wake_ready_pollers` retains only pollers whose slot still holds the generation they
/// registered with, so the list can never outgrow the shard's task arena, which *is* bounded. Nothing
/// tested it until 2026-10-20 — the file's only other test covers the wake path — so deleting that
/// `retain` left every test green while a shard accumulated pollers for dead tasks and called their
/// closures forever. It is also why `unregister_poller` has no caller: the sweep already does its job.
#[test]
fn a_poller_is_forgotten_when_its_task_ends() {
  let rt = Runtime::start(&config()).unwrap();
  let shard = rt.shard_ids()[0];
  rt.spawn_on(shard, busy_task()).unwrap();
  rt.spawn_on(shard, live_poller_task()).unwrap();
  rt.spawn_on(shard, gone_poller_task()).unwrap();

  assert!(
    wait_until(WAKE_DEADLINE, || LIVE_ASKED.load(Ordering::Acquire) >= 1
      && GONE_ASKED.load(Ordering::Acquire) >= 1),
    "both pollers were registered and asked at least once: live {}, gone {}",
    LIVE_ASKED.load(Ordering::Acquire),
    GONE_ASKED.load(Ordering::Acquire)
  );

  // The ending task has returned by now. Let the shard sweep, then take the two counters together.
  let _ = wait_until(SETTLE, || false);
  let live_at_rest = LIVE_ASKED.load(Ordering::Acquire);
  let gone_at_rest = GONE_ASKED.load(Ordering::Acquire);
  let _ = wait_until(SETTLE, || false);
  let live_after = LIVE_ASKED.load(Ordering::Acquire);
  let gone_after = GONE_ASKED.load(Ordering::Acquire);
  let _ = rt.shutdown();

  assert!(
    live_after > live_at_rest,
    "non-vacuity: the shard was still sweeping pollers across the window ({live_at_rest} then \
     {live_after}), so a frozen counter means forgotten and not parked"
  );
  assert_eq!(
    gone_after, gone_at_rest,
    "the ended task's poller is never asked again ({gone_at_rest} then {gone_after})"
  );
}
