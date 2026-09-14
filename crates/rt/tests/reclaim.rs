//! The runtime registry reclaims a shard's slot when its runtime shuts down (§4.3; banned item 8:
//! every structure has a derived bound *and* reclamation). Before this, every shard leaked its
//! registry entry, its kick descriptor and its context for the process lifetime, so a process that
//! started runtimes repeatedly (the fleet test suite: ~35 tests × 2–5 daemons × 1–5 shards) filled
//! the 1024-slot table and leaked a descriptor per shard — the accumulated suite state that made
//! late tests fail under oversubscription (`docs/wip/fleet-under-load.md`).

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use slates_rt::registry::MAX_SHARDS;
use slates_rt::runtime::{Runtime, RuntimeConfig};

fn config(shards: u16) -> RuntimeConfig {
  RuntimeConfig {
    shards,
    tasks_per_shard: 16,
    timers_per_shard: 16,
    ring_entries: 16,
    step_budget_ns: 1_000_000,
    timer_tick_ns: 100_000,
    batch: 16,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
  }
}

/// The process's open descriptor count (Unix), through the descriptor table's own listing — a
/// read of `/dev/fd` (macOS, Linux via the symlink), never a write.
#[cfg(unix)]
fn open_descriptors() -> usize {
  std::fs::read_dir("/dev/fd").map_or(0, |dir| dir.count())
}

/// Do: start and shut down one more runtime than the registry has slots, one shard each. Expect:
/// every start succeeds — a shut-down runtime's slot is reclaimed, so the bound is on *live*
/// shards, not on shards ever created. Before the fix the 1025th start refused `TooManyShards`.
#[test]
fn a_shut_down_runtimes_slot_is_reclaimed_so_more_runtimes_than_slots_may_run_in_turn() {
  for round in 0..=MAX_SHARDS {
    let runtime = Runtime::start(&config(1)).unwrap_or_else(|e| {
      panic!("runtime {round} refused after {round} shut-down runtimes: {e:?}")
    });
    assert_eq!(runtime.shard_ids().len(), 1);
    let _ = runtime.shutdown();
  }
}

/// Do: measure the open descriptors, run 64 start/shutdown cycles of a two-shard runtime, measure
/// again. Expect: the count returns to its baseline (the kick and driver descriptors are closed
/// with the shard). Before the fix each shard leaked its kqueue/eventfd: +2 per cycle.
#[cfg(unix)]
#[test]
fn a_shut_down_runtime_closes_every_descriptor_it_opened() {
  // One warm-up cycle so lazily-opened process-wide descriptors (the thread-local storage of the
  // first shard thread, the allocator's) are in the baseline.
  let _ = Runtime::start(&config(2)).unwrap().shutdown();
  let baseline = open_descriptors();
  for _ in 0..64 {
    let _ = Runtime::start(&config(2)).unwrap().shutdown();
  }
  let after = open_descriptors();
  assert!(
    after <= baseline,
    "descriptors leaked across 64 two-shard cycles: {baseline} before, {after} after"
  );
}

/// Do: spawn a task on a runtime and keep its wake word; shut the runtime down; start a new runtime
/// that reuses the same registry slot; fire the stale wake. Expect: the new runtime's shard is not
/// disturbed — the stale word names a task generation the new arena has not issued (its generations
/// continue from the old shard's high-water mark), so the wake is refused by the arena — and the
/// new runtime still runs its own task to completion. Non-vacuous: the new runtime provably reused
/// the same shard id, and the stale wake was delivered to it (the registry's stale-wake counter did
/// not move: the slot was live), so the arena's generation check is what refused it.
#[test]
fn a_wake_minted_for_a_dead_shard_is_refused_by_the_slots_new_holder() {
  use std::sync::atomic::{AtomicU64, Ordering};
  use std::task::{Context, Poll};
  static POLLS: AtomicU64 = AtomicU64::new(0);

  // A future that records every poll and captures its waker on the first.
  struct Capture(std::sync::mpsc::Sender<std::task::Waker>);
  impl std::future::Future for Capture {
    type Output = ();
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
      let _ = self.0.send(cx.waker().clone());
      Poll::Ready(())
    }
  }

  let (tx, rx) = std::sync::mpsc::channel();
  let first = Runtime::start(&config(1)).unwrap();
  let id = first.shard_ids()[0];
  first.spawn_on(id, Capture(tx)).unwrap();
  let waker = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
  let _ = first.shutdown();

  let second = Runtime::start(&config(1)).unwrap();
  assert_eq!(
    second.shard_ids()[0],
    id,
    "the second runtime reused the freed slot"
  );
  // A live task on the new holder, polled once by its own spawn.
  struct Counted(std::sync::mpsc::Sender<()>);
  impl std::future::Future for Counted {
    type Output = ();
    fn poll(self: std::pin::Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
      POLLS.fetch_add(1, Ordering::Relaxed);
      let _ = self.0.send(());
      Poll::Ready(())
    }
  }
  let (done_tx, done_rx) = std::sync::mpsc::channel();
  second.spawn_on(id, Counted(done_tx)).unwrap();
  done_rx
    .recv_timeout(std::time::Duration::from_secs(5))
    .expect("the new holder polled its own task");
  let before = slates_rt::registry::stale_wakes(id.0);
  waker.wake_by_ref();
  let counters = second.shutdown();
  let after = slates_rt::registry::stale_wakes(id.0);
  assert_eq!(
    after, before,
    "the stale wake reached a live slot (it was the arena that refused it)"
  );
  assert_eq!(
    counters[0].completed, 1,
    "the new holder ran exactly its own task; the stale wake polled nothing extra"
  );
}
