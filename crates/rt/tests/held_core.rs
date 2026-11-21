//! A busy shard asks its driver no more often than asking costs, and an idle moment watches the rings in user space no
//! longer than blocking costs (§4.3 "Loop"; A-218; 85a item 3, main's ruling (c), 2026-11-21; `slates_rt::held_core`).
//!
//! On macOS an empty zero-timeout `kevent` sleeps about 13 µs with an involuntary switch, and a busy shard asked its
//! driver once a quantum (a few microseconds), so every busy step after the first slept: a live write's exchange paid
//! two such sleeps on its control shard (about 30 µs, g228). This test drives a real shard through an idle window (where
//! it learns what a harvest costs) and then through busy steps, and holds both rules to their counts on whatever host
//! runs it: harvests deferred exactly when the measured harvest outlasts the quantum, and every idle moment's watch
//! counted.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use slates_rt::futures::yield_now;
use slates_rt::registry;
use slates_rt::runtime::{NO_IDLE_FLOOR_NS, Runtime, RuntimeConfig, WakeTracking};
use slates_rt::shard::{Counters, ShardId};

/// Shape: the wake estimate the shard is seeded with, a kicked park's on Apple silicon (A-387's row): its quantum and
/// the bound of its idle watch.
const PRIOR_WAKE_NS: u64 = 5_000;
/// Shape: the estimate's weighting, sixteen wakes.
const SHIFT: u32 = 4;
/// Shape: the idle window, thousands of harvests long on any host, so the shard learns its harvest's cost in it.
const WINDOW_NS: u64 = 20_000_000;
/// Shape: the busy steps the test runs, each a yield of one task: thousands of quanta.
const BUSY_STEPS: u32 = 20_000;
/// Shape: how long a shard gets to answer a question (a step is microseconds).
const ANSWER: Duration = Duration::from_secs(5);

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
    spin_ns: WINDOW_NS,
    wake_tracking: Some(WakeTracking {
      prior_ns: PRIOR_WAKE_NS,
      shift: SHIFT,
      idle_ratio: WINDOW_NS / PRIOR_WAKE_NS,
      idle_floor_ns: NO_IDLE_FLOOR_NS,
      idle_ceiling_ns: WINDOW_NS,
    }),
  }
}

/// The shard's counters, asked of the shard by a task.
fn counters_of(rt: &Runtime, shard: ShardId) -> Counters {
  let (tx, rx) = channel();
  rt.spawn_on(shard, async move {
    let _ = tx.send(registry::with_current(|ctx| ctx.counters()).unwrap_or_default());
  })
  .unwrap();
  rx.recv_timeout(ANSWER).unwrap()
}

/// Waits `span` on the test thread (a channel nobody sends on is the wait, as the runtime's disallowed `sleep` is for
/// shipped code).
fn wait(span: Duration) {
  let (_hold, rx) = channel::<()>();
  let started = Instant::now();
  while started.elapsed() < span {
    let _ = rx.recv_timeout(span.saturating_sub(started.elapsed()));
  }
}

/// 85a item 3 by use. Do: note one client activity and wait out its idle window (the shard's idle moments watch, then
/// spin, learning a harvest's cost), then run a task that yields twenty thousand times. Expect: idle moments' watches
/// counted with time spent; and harvests deferred if and only if the harvest the shard measured outlasts its quantum
/// (a macOS `kevent` that sleeps, not a Linux or Windows poll): the longer interval runs exactly where it should.
#[test]
fn a_busy_shard_asks_its_driver_no_more_often_than_asking_costs_and_an_idle_one_watches_its_rings_first()
 {
  let rt = Runtime::start(&config()).unwrap();
  let shard = rt.shard_ids()[0];
  let before = counters_of(&rt, shard);
  rt.spawn_on(shard, async {
    registry::with_current(|ctx| ctx.note_activity());
  })
  .unwrap();
  wait(Duration::from_nanos(WINDOW_NS).saturating_mul(2));
  let idled = counters_of(&rt, shard);
  let watches = (idled.watches_caught - before.watches_caught)
    + (idled.watches_ran_out - before.watches_ran_out);
  assert!(watches > 0, "no idle moment watched its rings: {idled:?}");
  assert!(
    idled.watch_ns > before.watch_ns,
    "no time was counted watching: {idled:?}"
  );
  let (done_tx, done_rx) = channel();
  rt.spawn_on(shard, async move {
    for _ in 0..BUSY_STEPS {
      yield_now().await;
    }
    let _ = done_tx.send(());
  })
  .unwrap();
  done_rx.recv_timeout(ANSWER).unwrap();
  let busy = counters_of(&rt, shard);
  let deferred = busy.harvests_deferred - idled.harvests_deferred;
  let harvest_outlasts_quantum = idled.held_turn_ns > idled.wake_cost_ns;
  eprintln!(
    "held_core: harvest {} ns against a quantum of {} ns; deferred {deferred}; watches {watches}, {} ns",
    idled.held_turn_ns,
    idled.wake_cost_ns,
    idled.watch_ns - before.watch_ns
  );
  assert_eq!(
    deferred > 0,
    harvest_outlasts_quantum,
    "busy harvests deferred {deferred} times with a harvest of {} ns against a quantum of {} ns: {busy:?}",
    idled.held_turn_ns,
    idled.wake_cost_ns
  );
  rt.shutdown().unwrap();
}
