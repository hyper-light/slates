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

/// Shape: rings the idle-spin test sends, each into a window the shard has just opened: more than one, so a ring that
/// a loaded machine delays past the window, into the park the kick ends, does not leave the spin untested.
const RINGS: u64 = 8;

/// How many of the window-opening tasks have run.
static NOTED: AtomicU64 = AtomicU64::new(0);

/// A ring that lands while the shard is idle-spinning wakes the consuming poller. Do: [`RINGS`] times, have the shard
/// run a task that notes activity, which opens its idle window; once it has run, ring as the daemon's doorbell thread
/// does (the flag set, then the shard kicked, which wakes only a parked shard); wait until the doorbell task is served.
/// Expect: every ring served, by the poller (`poller_wakes`), and the spin found work (`spin_hits`). A spin that asks a
/// consuming question and drops the answer loses a ring that lands in it, and the kick does not save it (the shard is
/// not parked): that ring is never served. Each wait is on what it waits for, with [`BACKSTOP`] only for a shard that
/// stopped: under load the old fixed 50 ms settle and 2 s deadline were the test's, not the shard's.
#[test]
fn a_ring_during_the_idle_spin_wakes_a_consuming_poller() {
  let rt = Runtime::start(&config()).unwrap();
  let shard = rt.shard_ids()[0];
  rt.spawn_on(shard, doorbell_loop()).unwrap();
  assert!(
    wait_until(BACKSTOP, || SERVED.load(Ordering::Acquire) >= 1),
    "the doorbell task ran once and went idle"
  );
  let mut served_all = true;
  for ring in 1..=RINGS {
    let noted = NOTED.load(Ordering::Acquire);
    rt.spawn_on(shard, async {
      registry::with_current(|ctx| ctx.note_activity());
      NOTED.fetch_add(1, Ordering::AcqRel);
    })
    .unwrap();
    assert!(
      wait_until(BACKSTOP, || NOTED.load(Ordering::Acquire) > noted),
      "ring {ring}: the shard opened its idle window"
    );
    RUNG.store(true, Ordering::Release);
    let _ = registry::with_entry(shard.0, |entry| entry.kick.kick());
    if !wait_until(BACKSTOP, || SERVED.load(Ordering::Acquire) > ring) {
      served_all = false;
      break;
    }
  }
  let counters = rt.shutdown().unwrap();
  let shard_counters = &counters[0];
  assert!(
    served_all,
    "every ring woke the poller: served {} times for {RINGS} rings, poller wakes {}",
    SERVED.load(Ordering::Acquire) - 1,
    shard_counters.poller_wakes
  );
  assert!(
    shard_counters.spin_hits >= 1,
    "the spin found work (spin hits {}): some ring landed in it",
    shard_counters.spin_hits
  );
  assert!(
    shard_counters.poller_wakes >= RINGS,
    "non-vacuity: the loop woke the poller for every ring ({} wakes), not a foreign wake",
    shard_counters.poller_wakes
  );
}

/// How many times the still-living task's poller was asked — the liveness control below.
static LIVE_ASKED: AtomicU64 = AtomicU64::new(0);
/// How many times the ended task's poller was asked. Must stop climbing once its task is gone.
static GONE_ASKED: AtomicU64 = AtomicU64::new(0);
/// Set by the ending task in its last poll, the step after which its slot is free: what the sweeps are counted from.
static GONE_ENDED: AtomicBool = AtomicBool::new(false);

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
  GONE_ENDED.store(true, Ordering::Release);
}

/// Shape: sweeps the shard must make after the ending task's last step before the ended poller's count is taken. A
/// sweep is one pass of `wake_ready_pollers`, which asks the living poller once, so the living count measures them. The
/// step that set [`GONE_ENDED`] may still be finishing when it is seen, and a sweep may be under way, so the count
/// starts one sweep later than the first that could have begun after it.
const SWEEPS_TO_FORGET: u64 = 2;
/// Shape: sweeps watched after that, across which the ended poller's count must not move: far more than the one sweep
/// that would ask a remembered poller, so a poller forgotten only by luck is not missed.
const SWEEPS_WATCHED: u64 = 1_000;
/// Shape: the wall-clock backstop for each wait, which only ends a test whose shard stopped altogether. The waits are
/// on sweeps, which a shard starved of its CPU still makes, late (the old 50 ms windows saw no sweep at all 1 run in 30
/// at load 40 on an 18-core Mac, 4 of 20 at load 82); a minute is past any starvation a test machine has shown.
const BACKSTOP: Duration = Duration::from_secs(60);

/// Waits until the living poller has been asked `more` times past `from`: that many sweeps. Its count, when the wait
/// ended, and whether the sweeps happened before [`BACKSTOP`].
fn sweeps_past(from: u64, more: u64) -> (u64, bool) {
  let target = from.saturating_add(more);
  let swept = wait_until(BACKSTOP, || LIVE_ASKED.load(Ordering::Acquire) >= target);
  (LIVE_ASKED.load(Ordering::Acquire), swept)
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
/// after a few steps; once that task has ended and the shard has swept past it, watch [`SWEEPS_WATCHED`] more sweeps,
/// counted by the living poller, not by the wall clock. Expect: the ended task's poller was asked while its task lived
/// and is never asked again.
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

  let ended = wait_until(BACKSTOP, || GONE_ENDED.load(Ordering::Acquire));
  let gone_while_alive = GONE_ASKED.load(Ordering::Acquire);
  let (live_at_rest, forgotten) = sweeps_past(LIVE_ASKED.load(Ordering::Acquire), SWEEPS_TO_FORGET);
  let gone_at_rest = GONE_ASKED.load(Ordering::Acquire);
  let (live_after, watched) = sweeps_past(live_at_rest, SWEEPS_WATCHED);
  let gone_after = GONE_ASKED.load(Ordering::Acquire);
  let _ = rt.shutdown();

  assert!(
    ended && forgotten && watched,
    "the shard swept within {BACKSTOP:?} of each wait: the ending task ended {ended}, then {SWEEPS_TO_FORGET} sweeps \
     {forgotten} and {SWEEPS_WATCHED} more {watched} (the living poller asked {live_at_rest} then {live_after} times)"
  );
  assert!(
    gone_while_alive >= 1,
    "non-vacuity: the ending task's poller was asked while its task lived ({gone_while_alive} times), so a count that \
     stands still means forgotten and not never registered"
  );
  assert_eq!(
    gone_after,
    gone_at_rest,
    "the ended task's poller is never asked again across {} sweeps ({gone_at_rest} then {gone_after})",
    live_after - live_at_rest
  );
}
