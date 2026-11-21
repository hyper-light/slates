//! A shard keeps its core when it harvests its driver while a core is free, and an idle spin keeps it no longer than
//! blocking costs (§4.3 "Loop"; A-218; 85a item 3, main's ruling (c), 2026-11-21; `slates_rt::held_core`).
//!
//! On macOS a zero-timeout `kevent` sleeps about 15 µs with an involuntary switch (A-218), and a busy shard harvested its
//! driver with one between steps, so a live write's exchange paid two such sleeps on its control shard (about 30 µs,
//! g228). The kept path (`Driver::poll_now`, `KEVENT_FLAG_IMMEDIATE` there) is taken only while the kernel counts a free
//! processor. This test drives a real shard through busy steps and an idle spin and holds the kept path to its counts:
//! a non-vacuity check that the path runs, and that every harvest is counted on exactly one path.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use slates_rt::futures::yield_now;
use slates_rt::registry;
use slates_rt::runtime::{NO_IDLE_FLOOR_NS, Runtime, RuntimeConfig, WakeTracking};
use slates_rt::shard::{Counters, ShardId};

/// Shape: the wake estimate the shard is seeded with, a kicked park's on Apple silicon (A-387's row): the quantum its
/// busy steps harvest at and the bound its idle spin keeps the core within.
const PRIOR_WAKE_NS: u64 = 5_000;
/// Shape: the estimate's weighting, sixteen wakes.
const SHIFT: u32 = 4;
/// Shape: the idle window, many times the bound, so a spin outlives its kept part and sleeps the rest.
const WINDOW_NS: u64 = 20_000_000;
/// Shape: the busy steps the test runs, each a yield of one task: thousands of quanta, so harvests are certain.
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

/// 85a item 3 by use. Do: run a task that yields twenty thousand times (busy steps, each harvest a quantum apart), then
/// note one client activity and wait out its idle window. Expect: harvests counted; and, while the kernel counts a free
/// processor, harvests that kept the core (the kept path is live), idle spins that kept it and ended, by catching work or
/// by spinning out the cost of blocking, and time spent keeping it. With no processor free the kept path must not run,
/// so the test then holds that instead and says so.
#[test]
fn a_shard_keeps_its_core_between_steps_while_a_core_is_free_and_a_spin_no_longer_than_blocking_costs()
 {
  let free = slates_machine::facts::free_processors();
  let rt = Runtime::start(&config()).unwrap();
  let shard = rt.shard_ids()[0];
  let before = counters_of(&rt, shard);
  let (done_tx, done_rx) = channel();
  rt.spawn_on(shard, async move {
    for _ in 0..BUSY_STEPS {
      yield_now().await;
    }
    let _ = done_tx.send(());
  })
  .unwrap();
  done_rx.recv_timeout(ANSWER).unwrap();
  rt.spawn_on(shard, async {
    registry::with_current(|ctx| ctx.note_activity());
  })
  .unwrap();
  wait(Duration::from_nanos(WINDOW_NS).saturating_mul(2));
  let after = counters_of(&rt, shard);
  let kept = after.harvests_kept - before.harvests_kept;
  let slept = after.harvests_slept - before.harvests_slept;
  assert!(
    kept + slept > 0,
    "no harvest was counted on either path: {after:?}"
  );
  if free == Some(0) {
    eprintln!(
      "held_core: SKIPPED the kept path's checks: the kernel counts no free processor (free {free:?})"
    );
    assert_eq!(
      kept, 0,
      "the core was kept with no processor free: {after:?}"
    );
    rt.shutdown().unwrap();
    return;
  }
  let spins_ended = (after.kept_spins_caught - before.kept_spins_caught)
    + (after.kept_spins_ran_out - before.kept_spins_ran_out);
  eprintln!(
    "held_core: free {free:?}; harvests kept {kept}, slept {slept}; kept spins ended {spins_ended}, kept {} ns",
    after.kept_spin_ns - before.kept_spin_ns
  );
  assert!(
    kept > 0,
    "a core was free and no harvest kept it: {after:?}"
  );
  assert!(
    spins_ended > 0,
    "the idle spin never kept its core: {after:?}"
  );
  assert!(
    after.kept_spin_ns > before.kept_spin_ns,
    "no time was counted keeping the core: {after:?}"
  );
  rt.shutdown().unwrap();
}
