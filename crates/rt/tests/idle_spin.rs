//! The idle spin is opened by client activity and bounded by its window (§4.7 "Shards poll rings while
//! any client has activity within the measured idle window"; §4.3 "Loop"; A-42).
//!
//! A shard with nothing to run spins before it parks only inside the window its last client activity
//! opened ([`slates_rt::shard::ShardContext::note_activity`]), and only until that window's end: timers that
//! fire after the client has gone quiet do not open a fresh window each. Until 2026-09-29 the daemon set a
//! shard "active" at its first client's handoff and nothing cleared it, so every timer of an idle daemon
//! ended in a full spin: an idle solo daemon spent 1.6–1.8 % of a CPU after one client had come and gone
//! (0.07 % before any), and an idle fleet pod's shard 15–32 % of a core
//! (docs/bugs/2026-09-29-an-idle-shard-spun-for-good-once-a-client-had-connected.md).

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use slates_rt::futures::sleep;
use slates_rt::registry;
use slates_rt::runtime::{NO_IDLE_FLOOR_NS, Runtime, RuntimeConfig, WakeTracking};
use slates_rt::shard::{Counters, ShardId};

/// Shape: the idle window — long against the timer period, so the window holds dozens of idle moments,
/// and short enough that the test spans a few of them in well under a second.
const WINDOW_NS: u64 = 200_000_000;
/// Shape: the period of the task whose timers make the shard go idle and wake again, 40 per window.
const TICK_NS: u64 = 5_000_000;
/// Shape: a stretch with no client activity, as long as a window: 40 idle moments, each of which would
/// spin if an idle moment opened a window of its own.
const QUIET: Duration = Duration::from_nanos(WINDOW_NS);
/// Shape: how long a shard gets to answer a counters question (a step is microseconds).
const ANSWER: Duration = Duration::from_secs(5);
/// Shape: the held test's own window, a millisecond: a fifth of the timer period, so the window an activity opens
/// covers at most the idle moment it began in.
const OWN_WINDOW_NS: u64 = 1_000_000;
/// Shape: the hold the held test asks for, as long as the first test's window: forty timer periods, two hundred times
/// the shard's own window.
const HOLD_NS: u64 = WINDOW_NS;
/// Shape: the spins a hold of [`HOLD_NS`] must show at least: half its timer periods, far above the one idle moment the
/// shard's own window covers, so a run whose timers came late still tells a held window from an unheld one.
const LEAST_HELD_SPINS: u64 = HOLD_NS / TICK_NS / 2;

/// Shape: the capped test's idle ceiling: ten timer periods, so a spin under it still covers several idle moments.
const CEILING_NS: u64 = 10 * TICK_NS;
/// Shape: the wake estimate the capped test seeds its shard with, a hundred quiet stretches: a window priced on it, or a
/// hold as long, would spin through the whole test.
const INFLATED_WAKE_NS: u64 = 100 * WINDOW_NS;
/// Shape: the capped test's estimate weighting, sixteen wakes (the shard is never kicked, so the estimate stays seeded).
const CAPPED_SHIFT: u32 = 4;

fn config() -> RuntimeConfig {
  config_with_window(WINDOW_NS)
}

/// A one-shard configuration that tracks a wake estimate seeded at [`INFLATED_WAKE_NS`] and prices its idle window on
/// it, as the runtime's own spin-then-park does (a ratio of one, no floor), under a ceiling of [`CEILING_NS`] (A-361).
fn capped_config() -> RuntimeConfig {
  RuntimeConfig {
    wake_tracking: Some(WakeTracking {
      prior_ns: INFLATED_WAKE_NS,
      shift: CAPPED_SHIFT,
      idle_ratio: 1,
      idle_floor_ns: NO_IDLE_FLOOR_NS,
      idle_ceiling_ns: CEILING_NS,
    }),
    ..config_with_window(CEILING_NS)
  }
}

/// A one-shard configuration whose idle window is `window_ns` and tracks no wake estimate.
fn config_with_window(window_ns: u64) -> RuntimeConfig {
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
    spin_ns: window_ns,
    wake_tracking: None,
  }
}

/// The idle spins the shard has entered: every spin ends in a hit, a miss or a timer's deadline.
fn spins(counters: &Counters) -> u64 {
  counters.spin_hits + counters.spin_misses + counters.spin_deadlines
}

/// The shard's counters, asked of the shard by a task (which is no client activity).
fn counters_of(rt: &Runtime, shard: ShardId) -> Counters {
  let (tx, rx) = channel();
  rt.spawn_on(shard, async move {
    let _ = tx.send(registry::with_current(|ctx| ctx.counters()).unwrap_or_default());
  })
  .unwrap();
  rx.recv_timeout(ANSWER).unwrap()
}

/// Waits `span` on the test thread (the stretch is the measurement; the shard's own timers run through
/// it). A channel nobody sends on is the wait, as the runtime's disallowed `sleep` is for shipped code.
fn wait(span: Duration) {
  let (_hold, rx) = channel::<()>();
  let started = Instant::now();
  while started.elapsed() < span {
    let _ = rx.recv_timeout(span.saturating_sub(started.elapsed()));
  }
}

/// A-42 by use. Do: run a periodic timer task on an idle shard; wait a stretch with no client activity;
/// note one client activity; wait out its window; wait another quiet stretch. Expect: no spin in the
/// first stretch (no activity, no window), spins inside the window the activity opened (the fast path is
/// live: the non-vacuity count), and none in the stretch after the window ended — the timers after the
/// client went quiet open no window of their own.
#[test]
fn a_shard_spins_only_inside_the_window_its_last_client_activity_opened() {
  let rt = Runtime::start(&config()).unwrap();
  let shard = rt.shard_ids()[0];
  rt.spawn_on(shard, async {
    loop {
      sleep(TICK_NS).await.unwrap();
    }
  })
  .unwrap();
  let start = counters_of(&rt, shard);
  wait(QUIET);
  let quiet = counters_of(&rt, shard);
  assert_eq!(
    spins(&quiet),
    spins(&start),
    "an idle shard with no client activity spun: {quiet:?}"
  );
  rt.spawn_on(shard, async {
    registry::with_current(|ctx| ctx.note_activity());
  })
  .unwrap();
  wait(Duration::from_nanos(WINDOW_NS).saturating_mul(2));
  let after_window = counters_of(&rt, shard);
  assert!(
    spins(&after_window) > spins(&quiet),
    "the window the activity opened never spun: {after_window:?}"
  );
  wait(QUIET);
  let later = counters_of(&rt, shard);
  assert_eq!(
    spins(&later),
    spins(&after_window),
    "the shard spun after its client's window had ended: {later:?}"
  );
  rt.shutdown().unwrap();
}

/// A-326 by use (§4.7). Do: run a periodic timer task on an idle shard whose own window is a millisecond; from a task,
/// hold the shard's window open for two hundred milliseconds (the daemon's hold for a client in R9's closed loop); wait
/// out the hold; then wait a quiet stretch. Expect: the shard spins at the idle moments all through the hold, two hundred
/// times its own window (at least half its timer periods: the non-vacuity count), the hold counted; and no spin once
/// the hold has ended, so a hold never leaves the shard spinning past it.
#[test]
fn a_held_window_keeps_the_shard_polling_past_its_own_window_until_the_hold_ends() {
  let rt = Runtime::start(&config_with_window(OWN_WINDOW_NS)).unwrap();
  let shard = rt.shard_ids()[0];
  rt.spawn_on(shard, async {
    loop {
      sleep(TICK_NS).await.unwrap();
    }
  })
  .unwrap();
  let start = counters_of(&rt, shard);
  rt.spawn_on(shard, async {
    registry::with_current(|ctx| ctx.hold_idle_window(HOLD_NS));
  })
  .unwrap();
  wait(Duration::from_nanos(HOLD_NS).saturating_mul(2));
  let held = counters_of(&rt, shard);
  assert_eq!(
    held.idle_holds,
    start.idle_holds + 1,
    "the hold counted: {held:?}"
  );
  assert!(
    spins(&held) >= spins(&start) + LEAST_HELD_SPINS,
    "the shard parked inside its hold: {} spins in a hold of {HOLD_NS} ns against a timer period of {TICK_NS} ns: \
     {held:?}",
    spins(&held) - spins(&start)
  );
  wait(QUIET);
  let later = counters_of(&rt, shard);
  assert_eq!(
    spins(&later),
    spins(&held),
    "the shard spun after its hold had ended: {later:?}"
  );
  rt.shutdown().unwrap();
}

/// Opens the shard's idle window from a task, by `open` (a client's activity or a hold), and counts the shard's spins:
/// those two ceilings after, then those a quiet stretch after that.
fn spins_after(rt: &Runtime, shard: ShardId, open: fn()) -> (Counters, Counters) {
  rt.spawn_on(shard, async move { open() }).unwrap();
  wait(Duration::from_nanos(CEILING_NS).saturating_mul(2));
  let within = counters_of(rt, shard);
  wait(QUIET);
  (within, counters_of(rt, shard))
}

/// A-361 by use (§4.3, §4.7: an idle shard polling for work is not a wedged one). Do: on a one-shard runtime that prices
/// its idle window on a wake estimate seeded a hundred quiet stretches long, under a ceiling of ten timer periods, run a
/// periodic timer task; note one client activity, wait two ceilings, then a quiet stretch; then hold the window for the
/// inflated length and do the same. Expect: the window the shard reports is the ceiling; it spins at the idle moments
/// inside the ceiling after each (the non-vacuity count) and not once in the quiet stretch after it. No idle spin outlives
/// its ceiling, however far an estimate or a hold would take it, which is what keeps a daemon's idle spin under its
/// watchdog's window: without the ceiling a Windows daemon's window, a hundred times an estimate that counted loaded
/// wakes, spun past it and the anchor killed the live daemon (g258's now-x86 lane, 3 of 3 runs, 2026-11-18).
#[test]
fn an_inflated_estimate_or_hold_spins_no_longer_than_the_ceiling() {
  let rt = Runtime::start(&capped_config()).unwrap();
  let shard = rt.shard_ids()[0];
  rt.spawn_on(shard, async {
    loop {
      sleep(TICK_NS).await.unwrap();
    }
  })
  .unwrap();
  let start = counters_of(&rt, shard);
  assert_eq!(
    start.idle_window_ns, CEILING_NS,
    "the window priced on the inflated estimate is held to the ceiling: {start:?}"
  );
  let opens: [(&str, fn()); 2] = [
    ("a client's activity", || {
      registry::with_current(|ctx| ctx.note_activity());
    }),
    ("a hold as long as the inflated estimate", || {
      registry::with_current(|ctx| ctx.hold_idle_window(INFLATED_WAKE_NS));
    }),
  ];
  let mut before = start;
  for (opened_by, open) in opens {
    let (within, after) = spins_after(&rt, shard, open);
    assert!(
      spins(&within) > spins(&before),
      "{opened_by} opened no spin: {within:?}"
    );
    assert_eq!(
      spins(&after),
      spins(&within),
      "{opened_by}: the shard spun past its ceiling: {after:?}"
    );
    before = after;
  }
  rt.shutdown().unwrap();
}
