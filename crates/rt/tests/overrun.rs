//! The measured scheduler quantum (§4.8 "SWIM period = max(k × RTT p99, scheduler quantum)"): a shard
//! records how late its step ran after the wait before it — parked in its driver, or idle-spinning
//! for its timer — as an exponentially-forgetting maximum (`ShardContext::scheduler_overrun_ns`),
//! read by a task through `futures::scheduler_overrun_ns` and by any thread through the registry
//! pulse. Three properties pin what the number means: a wait stepped past its deadline reports the
//! lateness; a **busy** shard's late timer is not an overrun (the fleet's first window derivation
//! measured its sleeps' overrun, which is task latency, and dilated its windows at rest —
//! `docs/bugs/2026-09-16-fleet-detection-windows-use-a-fixed-scheduler-quantum.md`); and on an idle
//! shard the measurement never exceeds the lateness the shard's own clock actually shows.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use slates_rt::futures;
use slates_rt::registry;
use slates_rt::runtime::{LocalRuntime, Runtime, RuntimeConfig};

/// Shape: the sleep each task takes — tens of timer ticks, short enough that a run of them stays well
/// inside a second and long enough that a wait for it is a real driver park, not a step's tail.
const SLEEP_NS: u64 = 5_000_000;
/// Shape: how many sleeps the idle shard takes, so the measurement is folded over many waits (its
/// forgetting has run many times over) rather than judged on one.
const SLEEPS: u64 = 40;
/// Shape: how long the shard is held off its step after a wait's deadline — the descheduling the test
/// models — ten of its sleeps, far past a timer tick and a driver wake, so the reported overrun is
/// unmistakably it and not either of those.
const HELD_OFF: Duration = Duration::from_millis(50);
/// Shape: how long the multi-thread shard gets to finish its sleeps before the test fails; many times
/// the sleeps' own span, so a loaded machine does not flake it.
const DONE_DEADLINE: Duration = Duration::from_secs(10);

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
    // No idle spin: every wait below is a park, measured against a real driver wake.
    spin_ns: 0,
    wake_tracking: None,
  }
}

/// Holds the calling thread for `window` without a blocking sleep (D-9): the thread yields, so it is
/// not stepping its shard — which, for the shard, is the operating system not running it.
fn hold_for(window: Duration) {
  let began = Instant::now();
  while began.elapsed() < window {
    std::thread::yield_now();
  }
}

fn wait_until(deadline: Duration, done: impl Fn() -> bool) -> bool {
  let began = Instant::now();
  while began.elapsed() < deadline {
    if done() {
      return true;
    }
    std::thread::yield_now();
  }
  done()
}

fn nanos(window: Duration) -> u64 {
  u64::try_from(window.as_nanos()).unwrap()
}

/// A shard that waited for a deadline and first stepped `HELD_OFF` past it reports at least that much,
/// and never more than its own clock shows: the test thread is the shard, so holding it between its
/// park and its next step is exactly the operating system holding a woken shard off its core.
#[test]
#[cfg_attr(miri, ignore)] // the OS driver opens a kqueue or an eventfd, which Miri does not model
fn a_wait_stepped_past_its_deadline_reports_the_lateness() {
  let rt = LocalRuntime::new(&config()).unwrap();
  let ctx = rt.context();
  rt.spawn(async {
    futures::sleep(SLEEP_NS).await;
  })
  .unwrap();
  // The task arms its timer; the shard then has nothing to do until it fires.
  let mut outcome = ctx.step();
  while outcome.did_work {
    outcome = ctx.step();
  }
  let deadline = outcome
    .next_deadline_ns
    .expect("the sleep armed a timer the shard waits for");
  ctx.park(Some(deadline));
  // Woken at the deadline — and then not run.
  hold_for(HELD_OFF);
  ctx.step();
  let stepped_by = ctx.now_ns();
  let overrun = ctx.scheduler_overrun_ns();
  rt.run_until_idle();
  let counters = ctx.counters();
  assert!(
    counters.waits >= 1,
    "the shard parked for the deadline (waits {}) — the test's premise",
    counters.waits
  );
  assert!(
    overrun >= nanos(HELD_OFF),
    "the step {} ns past its wait's deadline reports at least the {} ns it was held off",
    overrun,
    nanos(HELD_OFF)
  );
  assert!(
    overrun <= stepped_by.saturating_sub(deadline),
    "the overrun ({overrun} ns) never exceeds the lateness the shard clock shows ({} ns)",
    stepped_by.saturating_sub(deadline)
  );
  assert_eq!(
    counters.scheduler_overrun_ns, overrun,
    "the counters carry the same measurement"
  );
}

/// How late the sleeper saw its own timer fire, on the shard clock.
static SLEEPER_LATE_NS: AtomicU64 = AtomicU64::new(0);
/// The overrun the sleeper read on waking; the sentinel until it has.
static SLEEPER_OVERRUN_NS: AtomicU64 = AtomicU64::new(u64::MAX);

/// The claim the fleet's windows rest on: a timer that fires late because the shard was **busy** — a
/// poll held it past the deadline — is not an overrun, since the shard never waited. The blocker is
/// polled after the sleeper in the same step and holds the shard past the sleeper's deadline; the timer
/// then fires from the next step's expiry, the sleeper sees it late on the shard clock, and the
/// measurement stays at zero with no wait ever entered.
#[test]
#[cfg_attr(miri, ignore)] // the OS driver opens a kqueue or an eventfd, which Miri does not model
fn a_busy_shards_late_timer_is_not_a_scheduler_overrun() {
  let rt = LocalRuntime::new(&config()).unwrap();
  rt.spawn(async {
    let due = futures::now_ns().saturating_add(SLEEP_NS);
    futures::sleep(SLEEP_NS).await;
    SLEEPER_LATE_NS.store(futures::now_ns().saturating_sub(due), Ordering::Release);
    SLEEPER_OVERRUN_NS.store(futures::scheduler_overrun_ns(), Ordering::Release);
  })
  .unwrap();
  rt.spawn(async {
    hold_for(HELD_OFF);
  })
  .unwrap();
  rt.run_until_idle();
  let counters = rt.context().counters();
  let late = SLEEPER_LATE_NS.load(Ordering::Acquire);
  assert!(
    late >= nanos(HELD_OFF).saturating_sub(SLEEP_NS),
    "the blocker held the shard past the sleeper's deadline: the timer fired {late} ns late (the \
     test's premise — the sleeper polls before the blocker)"
  );
  assert_eq!(
    counters.timers_fired, 1,
    "the sleeper's timer fired from a step's expiry, {counters:?}"
  );
  assert_eq!(
    counters.waits, 0,
    "the shard never waited: it was busy the whole time, {counters:?}"
  );
  assert_eq!(
    SLEEPER_OVERRUN_NS.load(Ordering::Acquire),
    0,
    "a busy shard's late timer is task latency, not a scheduler overrun"
  );
}

/// The span the sleeps took on the shard clock, from before the first to after the last.
static SLEEPS_SPAN_NS: AtomicU64 = AtomicU64::new(0);
/// The overrun the idle shard's task read after its sleeps; the sentinel until it has.
static IDLE_OVERRUN_NS: AtomicU64 = AtomicU64::new(u64::MAX);

async fn sleep_repeatedly() {
  let began = futures::now_ns();
  for _ in 0..SLEEPS {
    futures::sleep(SLEEP_NS).await;
  }
  SLEEPS_SPAN_NS.store(futures::now_ns().saturating_sub(began), Ordering::Release);
  IDLE_OVERRUN_NS.store(futures::scheduler_overrun_ns(), Ordering::Release);
}

/// On a shard thread of its own, idle between sleeps, the measurement is bounded by the lateness the
/// sleeps actually accrued on the shard's own clock: each sleep's span is its length plus how late the
/// step after its wait ran, so the span beyond the sleeps' sum bounds every overrun folded in — and so
/// the forgetting maximum. No fixed allowance: a loaded machine widens the bound exactly as much as it
/// delays the shard, and only a measurement reading something other than the wait's lateness (a wrong
/// clock, a stale deadline) can exceed it. The registry pulse mirrors the value for another thread.
#[test]
#[cfg_attr(miri, ignore)] // the OS driver opens a kqueue or an eventfd, which Miri does not model
fn an_idle_shards_measured_overrun_never_exceeds_the_lateness_it_observed() {
  let rt = Runtime::start(&config()).unwrap();
  let shard = rt.shard_ids()[0];
  rt.spawn_on(shard, sleep_repeatedly()).unwrap();
  assert!(
    wait_until(DONE_DEADLINE, || IDLE_OVERRUN_NS.load(Ordering::Acquire)
      != u64::MAX),
    "the sleeps finished inside the deadline"
  );
  let mirrored = registry::entry(shard.0).map(|entry| entry.pulse.scheduler_overrun_ns());
  let counters = rt.shutdown();
  let overrun = IDLE_OVERRUN_NS.load(Ordering::Acquire);
  let lateness = SLEEPS_SPAN_NS
    .load(Ordering::Acquire)
    .saturating_sub(SLEEPS.saturating_mul(SLEEP_NS));
  assert!(
    counters[0].waits >= SLEEPS,
    "the shard parked for each sleep (waits {}) — the test's premise",
    counters[0].waits
  );
  assert!(
    overrun <= lateness,
    "the measured overrun ({overrun} ns) is bounded by the lateness the {SLEEPS} sleeps of \
     {SLEEP_NS} ns accrued ({lateness} ns)"
  );
  assert_eq!(
    mirrored,
    Some(overrun),
    "the registry pulse mirrors the shard's measurement for another thread"
  );
}
