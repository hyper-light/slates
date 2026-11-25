//! A kick that an idle spin's harvest takes is not lost (A-798; §4.7 "a parked shard is woken by the driver kick").
//!
//! A client publishes its request, then rings; the daemon's doorbell thread kicks the shard the ring names, and the kick
//! is sticky (an `EVFILT_USER` trigger, an eventfd count) until a wait takes it. The idle spin takes kicks too: each turn
//! harvests the driver without blocking, and a harvest reports the completions it found, never the kick it took. Until
//! A-798 a turn looked at the rings and the pollers first and harvested after, so a request published between the look
//! and the harvest lost its kick to the harvest; on the spin's last turn the shard then parked with the request unseen and
//! no kick left to wake it. A bench's client waited for good with every shard parked (provision_bench, 2026-11-20; w-engine's
//! load_bench end-of-run read, twice on the big Studio).

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use slates_rt::futures;
use slates_rt::registry;
use slates_rt::runtime::{Runtime, RuntimeConfig};

/// Shape: the idle spin window: long enough that a loaded machine's shard (a 4-core runner under ThreadSanitizer, or a Mac
/// building beside it) reaches its spin and asks within it after the window task's step, which at 1 ms it did not (3 of
/// 20 native runs at load, 2026-11-24: the shard parked after two asks, or asked only once the window had closed). The
/// injecting ask holds its turn to the window's end, so this costs the test once.
const SPIN_NS: u64 = 100_000_000;

/// Shape: how long the test waits for the ring to be served. A served ring takes microseconds; a lost one is never served.
/// Generous for a loaded machine, and the hang a lost wake causes is told apart from slowness by the shard's own state
/// (parked, the ring unserved).
const BACKSTOP: Duration = Duration::from_secs(20);

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

/// The request's publication, as a client's ring would show it to the shard's poller.
static RUNG: AtomicBool = AtomicBool::new(false);
/// Every ask of the poller's question.
static ASKED: AtomicU64 = AtomicU64::new(0);
/// The ask at which the poller injects the ring (0: none yet).
static INJECT_AT: AtomicU64 = AtomicU64::new(0);
/// Set once the ring has been injected.
static INJECTED: AtomicBool = AtomicBool::new(false);
/// Whether the injecting ask came while the idle window was still open: the spin's ask, so the A-798 path is the one
/// the test drives (the test's non-vacuity).
static INJECTED_IN_WINDOW: AtomicBool = AtomicBool::new(false);
/// How many times the serving task was woken: once at its start, once per served ring.
static SERVED: AtomicU64 = AtomicU64::new(0);
/// How many window-opening tasks have run.
static NOTED: AtomicU64 = AtomicU64::new(0);
/// When the idle window the test opened ends, on the host's monotonic clock: read right after the activity was noted, so
/// never before the end the spin compares against (the driver's clock runs at the host's rate).
static WINDOW_END_NS: AtomicU64 = AtomicU64::new(u64::MAX);

/// The host's monotonic clock, nanoseconds.
fn monotonic_ns() -> u64 {
  slates_machine::clock::monotonic_ns()
}

/// The poller's question: whether a ring is published (taking it), and, on the ask the test names, a client's ring
/// landing just after this look: published, then the shard kicked (from another thread, as the doorbell thread does), then
/// the turn held until the window it spins in has ended, so the spin ends on this turn. The hold is that condition on
/// the clock the window is measured by, never a sleep (D-9), under the test's backstop.
fn ask() -> bool {
  let n = ASKED.fetch_add(1, Ordering::AcqRel) + 1;
  let rung = RUNG.swap(false, Ordering::AcqRel);
  if n == INJECT_AT.load(Ordering::Acquire) {
    INJECTED_IN_WINDOW.store(
      monotonic_ns() <= WINDOW_END_NS.load(Ordering::Acquire),
      Ordering::Release,
    );
    let shard = registry::current_shard().unwrap();
    std::thread::spawn(move || {
      RUNG.store(true, Ordering::Release);
      registry::with_entry(shard, |entry| entry.kick.kick()).unwrap();
    })
    .join()
    .unwrap();
    INJECTED.store(true, Ordering::Release);
    let held = Instant::now();
    while monotonic_ns() <= WINDOW_END_NS.load(Ordering::Acquire) && held.elapsed() < BACKSTOP {
      std::hint::spin_loop();
    }
  }
  rung
}

async fn serving_loop() {
  if let Some(task) = futures::current_task() {
    registry::with_current(|ctx| ctx.register_poller(task, Box::new(ask)))
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

/// A-798. Do: let the serving task go idle; open the shard's idle window with a task that notes activity; once the spin has
/// asked the poller, have the poller's next ask inject a ring right after its own look (published, then the shard
/// kicked from another thread) and hold the turn past the window, so the spin ends on it. Expect: the ring is served.
/// Looking before harvesting, the turn's harvest took the kick and reported nothing, the spin ended, and the shard parked
/// with the ring published and no kick left: never served (this test hangs to its backstop and fails).
#[test]
fn a_kick_the_spins_harvest_takes_still_gets_its_ring_served() {
  let rt = Runtime::start(&config()).unwrap();
  let shard = rt.shard_ids()[0];
  rt.spawn_on(shard, serving_loop()).unwrap();
  assert!(
    wait_until(BACKSTOP, || SERVED.load(Ordering::Acquire) >= 1),
    "the serving task ran once and went idle"
  );
  rt.spawn_on(shard, async {
    registry::with_current(|ctx| {
      ctx.note_activity();
      WINDOW_END_NS.store(
        monotonic_ns().saturating_add(ctx.idle_window_ns()),
        Ordering::Release,
      );
    });
    // The injection lands in the spin: the step that runs this task and the one after it each ask once; the asks after
    // those are the spin's, which asks every turn. So name an ask two past this step's, counted here on the shard's own
    // thread as it opens the window: named from the test's thread, it raced the window, and a thread descheduled past
    // the window's 1 ms (TSan on a loaded runner, main's diag 45517552633) named an ask the parked shard never made.
    INJECT_AT.store(ASKED.load(Ordering::Acquire) + 3, Ordering::Release);
    NOTED.fetch_add(1, Ordering::AcqRel);
  })
  .unwrap();
  assert!(
    wait_until(BACKSTOP, || NOTED.load(Ordering::Acquire) >= 1),
    "the shard opened its idle window"
  );
  assert!(
    wait_until(BACKSTOP, || INJECTED.load(Ordering::Acquire)),
    "the poller injected the ring (asked {} times)",
    ASKED.load(Ordering::Acquire)
  );
  assert!(
    INJECTED_IN_WINDOW.load(Ordering::Acquire),
    "the ring was injected by the spin's ask, within the idle window"
  );
  let served = wait_until(BACKSTOP, || SERVED.load(Ordering::Acquire) >= 2);
  let parked = registry::with_entry(shard.0, |entry| entry.parking.parked()).unwrap_or(false);
  let counters = rt.shutdown().unwrap();
  assert!(
    served,
    "the ring published right after the spin's look was never served: the shard parked ({parked}) with its kick taken \
     by the spin's harvest (spin hits {}, poller wakes {})",
    counters[0].spin_hits, counters[0].poller_wakes
  );
}
