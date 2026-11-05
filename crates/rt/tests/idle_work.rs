//! A shard's idle work (§4.2, "the shard schedules a pre-fault task on its idle time"; A-214): the work a shard
//! registers runs a slice at a time when the shard has nothing else to do, and the shard parks once the work says it
//! has none left, so an idle shard does not keep calling it.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use slates_rt::registry;
use slates_rt::runtime::{Runtime, RuntimeConfig};
use slates_rt::shard::ShardId;

/// Shape: how long the test waits for the shard to work through what it owes (a slice is microseconds).
const SETTLE: Duration = Duration::from_secs(5);
/// Shape: a quiet stretch, long against a slice, in which a parked shard must not call its idle work.
const QUIET: Duration = Duration::from_millis(300);
/// Shape: the slices owed at first, and later.
const FIRST: u64 = 5;
/// Shape: see [`FIRST`].
const LATER: u64 = 3;

/// Slices the idle work still owes; it does one a call while any are owed.
static OWED: AtomicU64 = AtomicU64::new(0);
/// Slices done.
static DONE: AtomicU64 = AtomicU64::new(0);
/// Calls that found nothing owed.
static EMPTY: AtomicU64 = AtomicU64::new(0);

fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 1,
    tasks_per_shard: 64,
    timers_per_shard: 64,
    ring_entries: 64,
    step_budget_ns: 1_000_000,
    timer_tick_ns: 100_000,
    batch: 64,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
    wake_tracking: None,
  }
}

/// One slice of the test's idle work: a slice owed is done, else the call is counted empty.
fn slice() -> bool {
  let owed = OWED.load(Ordering::Acquire);
  if owed == 0 {
    EMPTY.fetch_add(1, Ordering::AcqRel);
    return false;
  }
  OWED.store(owed - 1, Ordering::Release);
  DONE.fetch_add(1, Ordering::AcqRel);
  true
}

/// Waits until `done` slices are done or [`SETTLE`] passes.
fn settle(done: u64) {
  let (_hold, rx) = channel::<()>();
  let started = Instant::now();
  while DONE.load(Ordering::Acquire) < done && started.elapsed() < SETTLE {
    let _ = rx.recv_timeout(Duration::from_millis(1));
  }
}

/// The shard's driver waits so far, asked of the shard by a task.
fn waits_of(rt: &Runtime, shard: ShardId) -> u64 {
  let (tx, rx) = channel();
  rt.spawn_on(shard, async move {
    let _ = tx.send(registry::with_current(|ctx| ctx.counters().waits).unwrap_or(0));
  })
  .unwrap();
  rx.recv_timeout(SETTLE).unwrap()
}

/// Waits `span` on the test thread (a channel nobody sends on, as the runtime's disallowed `sleep` is for shipped
/// code).
fn wait(span: Duration) {
  let (_hold, rx) = channel::<()>();
  let started = Instant::now();
  while started.elapsed() < span {
    let _ = rx.recv_timeout(span.saturating_sub(started.elapsed()));
  }
}

/// A-214 by use. Do: owe the idle work five slices, register it on an idle shard; then wait a quiet stretch; then owe
/// three more and wake the shard with a task. Expect: the five slices done with no other work to prompt them; during
/// the quiet stretch, at most one call for each time the shard waited (it parks once the work says it has none left,
/// and asks again only when something wakes it, never in a loop); and the three later
/// slices done once the shard woke.
#[test]
fn idle_work_runs_a_slice_at_a_time_until_done_and_the_shard_then_parks() {
  let rt = Runtime::start(&config()).unwrap();
  let shard = rt.shard_ids()[0];
  OWED.store(FIRST, Ordering::Release);
  rt.spawn_on(shard, async {
    registry::with_current(|ctx| ctx.set_idle_work(Box::new(slice)))
      .unwrap()
      .unwrap();
  })
  .unwrap();
  settle(FIRST);
  assert_eq!(
    DONE.load(Ordering::Acquire),
    FIRST,
    "the idle shard did every owed slice"
  );
  // The shard asks once more after the last slice, finds nothing, and parks.
  let (_hold, rx) = channel::<()>();
  let started = Instant::now();
  while EMPTY.load(Ordering::Acquire) == 0 && started.elapsed() < SETTLE {
    let _ = rx.recv_timeout(Duration::from_millis(1));
  }
  // Asking the shard for its counters spawns a task on it, which wakes it, so each idle moment that wake opens asks
  // once more: what the quiet stretch must hold to is one empty call a wait at most, never a loop of them.
  let before = waits_of(&rt, shard);
  let empty = EMPTY.load(Ordering::Acquire);
  wait(QUIET);
  let after = waits_of(&rt, shard);
  let calls = EMPTY.load(Ordering::Acquire).saturating_sub(empty);
  assert!(
    calls <= after.saturating_sub(before).saturating_add(1),
    "a parked shard kept calling idle work that had none: {calls} calls across {} waits",
    after.saturating_sub(before)
  );
  OWED.store(LATER, Ordering::Release);
  rt.spawn_on(shard, async {}).unwrap();
  settle(FIRST + LATER);
  assert_eq!(
    DONE.load(Ordering::Acquire),
    FIRST + LATER,
    "the woken shard did the slices owed since"
  );
  rt.shutdown().unwrap();
}
