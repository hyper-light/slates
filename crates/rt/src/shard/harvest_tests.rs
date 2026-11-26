//! A-783's harvest period (`ShardContext::harvest_period_ns`, main 01a20182's ruling of 2026-11-25): a paced step's asks
//! harvest I/O no more often than the shard's measured scheduler overrun, or a harvest's cost where that is longer.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::futures;
use crate::registry;
use crate::runtime::{LocalRuntime, RuntimeConfig};

/// A one-shard runtime for a test's own task.
fn runtime() -> LocalRuntime {
  LocalRuntime::new(&RuntimeConfig {
    shards: 1,
    tasks_per_shard: 8,
    timers_per_shard: 8,
    ring_entries: 8,
    step_budget_ns: 1_000_000,
    timer_tick_ns: 100_000,
    batch: 8,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
    wake_tracking: None,
  })
  .unwrap()
}

/// The shard's asks and the harvests among them, so far.
fn asks_and_harvests() -> (u64, u64) {
  registry::with_current(|ctx| {
    let counters = ctx.counters();
    (counters.waiting_asked, counters.paced_harvests)
  })
  .unwrap()
}

/// Shape: the scheduler overrun the loaded-host test holds: a loaded host's, far past what a harvest costs here.
const HELD_OVERRUN_NS: u64 = 200_000;
/// Shape: how many held overruns the loaded-host test asks through: enough that a harvest every ask would be many times
/// the harvests its period allows.
const OVERRUNS_ASKED: u64 = 20;

/// Main 01a20182's sample of 2026-11-25 (the encoding behind git/git's push spent 39 % of its time in the `kevent` of
/// every ask's harvest). Do: on a shard alone, hold its scheduler overrun at [`HELD_OVERRUN_NS`] and ask whether anyone
/// waits as fast as a loop can for [`OVERRUNS_ASKED`] overruns, keeping the least harvest period met. Expect: no one
/// waiting; the asks many more than the harvests, and the harvests no more than the time asked over the least period,
/// and one. Red before: every ask harvested.
#[test]
fn a_loaded_shards_asks_harvest_io_once_its_overrun() {
  let runtime = runtime();
  let (sent, got) = std::sync::mpsc::channel();
  runtime
    .spawn(async move {
      registry::with_current(|ctx| ctx.hold_scheduler_overrun_ns(HELD_OVERRUN_NS)).unwrap();
      let (asks_before, harvests_before) = asks_and_harvests();
      let began = futures::now_ns();
      let mut least = u64::MAX;
      let mut seen = false;
      while futures::now_ns().saturating_sub(began) < OVERRUNS_ASKED * HELD_OVERRUN_NS {
        least = least.min(futures::harvest_period_ns().unwrap());
        seen |= futures::others_waiting();
      }
      let elapsed = futures::now_ns().saturating_sub(began);
      let (asks, harvests) = asks_and_harvests();
      let _ = sent.send((
        seen,
        asks - asks_before,
        harvests - harvests_before,
        elapsed,
        least,
      ));
    })
    .unwrap();
  runtime.run_until_idle();
  let (seen, asks, harvests, elapsed, least) = got.recv().unwrap();
  assert!(!seen, "no one waited on a shard alone");
  assert!(
    harvests <= elapsed / HELD_OVERRUN_NS + 1,
    "{harvests} harvests of {asks} asks over {elapsed} ns at an overrun of {HELD_OVERRUN_NS} ns"
  );
  assert!(asks > harvests, "{asks} asks, {harvests} harvests");
  assert!(
    least >= HELD_OVERRUN_NS && harvests <= elapsed / least + 1,
    "the period held to the overrun: {least} ns"
  );
}

/// Shape: how many asks the quiet-host test makes.
const QUIET_ASKS: u64 = 64;

/// A-783 (the quiet end: a shard whose scheduler lets it run on time harvests I/O at every ask a harvest's cost apart,
/// as before the period). Do: on a shard alone, hold its scheduler overrun at zero and ask [`QUIET_ASKS`] times, each a
/// harvest period after the last ask returned. Expect: every ask harvested.
#[test]
fn a_quiet_shards_asks_a_harvests_cost_apart_each_harvest() {
  let runtime = runtime();
  let (sent, got) = std::sync::mpsc::channel();
  runtime
    .spawn(async move {
      registry::with_current(|ctx| ctx.hold_scheduler_overrun_ns(0)).unwrap();
      let (asks_before, harvests_before) = asks_and_harvests();
      let mut returned = futures::now_ns();
      for _ in 0..QUIET_ASKS {
        let period = futures::harvest_period_ns().unwrap();
        while futures::now_ns().saturating_sub(returned) < period {
          std::hint::spin_loop();
        }
        let _ = futures::others_waiting();
        returned = futures::now_ns();
      }
      let (asks, harvests) = asks_and_harvests();
      let _ = sent.send((asks - asks_before, harvests - harvests_before));
    })
    .unwrap();
  runtime.run_until_idle();
  let (asks, harvests) = got.recv().unwrap();
  assert_eq!((asks, harvests), (QUIET_ASKS, QUIET_ASKS));
}
