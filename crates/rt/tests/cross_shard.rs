//! Two shards on OS threads wake each other through the pair rings and the kick: the cross-shard
//! wake of §4.3's worked example, and shutdown with tasks still running.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::channel;
use std::task::{Context, Poll};

use slates_rt::futures::sleep;
use slates_rt::runtime::{Runtime, RuntimeConfig};

fn config(shards: u16) -> RuntimeConfig {
  RuntimeConfig {
    shards,
    tasks_per_shard: 64,
    timers_per_shard: 64,
    ring_entries: 64,
    step_budget_ns: 1_000_000_000,
    timer_tick_ns: 100_000,
    batch: 64,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
  }
}

/// A one-shot signal between tasks on different shards: the waiter stores its waker word in a
/// static cell; the signaller wakes it through the registry.
// The waiter's packed word can be zero (shard 0, slot 0, generation 0), so registration is a
// separate flag rather than a non-zero sentinel.
static WAITER: AtomicU64 = AtomicU64::new(0);
static REGISTERED: AtomicU64 = AtomicU64::new(0);
static FLAG: AtomicU64 = AtomicU64::new(0);

struct WaitForFlag;

impl std::future::Future for WaitForFlag {
  type Output = ();
  fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
    if FLAG.load(Ordering::Acquire) == 1 {
      return Poll::Ready(());
    }
    let word = slates_rt::waker::word_of(cx.waker()).unwrap();
    WAITER.store(word.word(), Ordering::Release);
    REGISTERED.store(1, Ordering::Release);
    if FLAG.load(Ordering::Acquire) == 1 {
      Poll::Ready(())
    } else {
      Poll::Pending
    }
  }
}

#[test]
fn a_task_on_one_shard_is_woken_by_a_task_on_another() {
  let rt = Runtime::start(&config(2)).unwrap();
  let ids = rt.shard_ids().to_vec();
  let (tx, rx) = channel();
  let tx_waiter = tx.clone();
  rt.spawn_on(ids[0], async move {
    WaitForFlag.await;
    let _ = tx_waiter.send("waiter woke on shard 0");
  })
  .unwrap();
  rt.spawn_on(ids[1], async move {
    // Give the waiter time to register its waker, then flip the flag and wake it across shards.
    while REGISTERED.load(Ordering::Acquire) == 0 {
      sleep(100_000).await;
    }
    FLAG.store(1, Ordering::Release);
    let word = slates_mem::Encoded::from_word(WAITER.load(Ordering::Acquire));
    slates_rt::registry::wake(word);
    let _ = tx.send("signaller done on shard 1");
  })
  .unwrap();
  let mut received = Vec::new();
  for _ in 0..2 {
    match rx.recv_timeout(std::time::Duration::from_secs(5)) {
      Ok(m) => received.push(m),
      Err(e) => {
        let counters = rt.shutdown();
        panic!(
          "timed out ({e}) after {received:?}; waiter word {}; flag {}; counters {counters:#?}",
          WAITER.load(Ordering::Acquire),
          FLAG.load(Ordering::Acquire)
        );
      }
    }
  }
  let first = received.remove(0);
  let second = received.remove(0);
  let mut got = vec![first, second];
  got.sort();
  assert_eq!(
    got,
    vec!["signaller done on shard 1", "waiter woke on shard 0"]
  );
  let counters = rt.shutdown();
  assert_eq!(counters.len(), 2);
  assert!(
    counters.iter().all(|c| c.nested_borrows == 0),
    "{counters:?}"
  );
  assert!(
    counters[0].wakes_pair + counters[0].wakes_foreign >= 1,
    "{counters:?}"
  );
}

#[test]
fn shutdown_cancels_running_tasks_and_joins_the_threads() {
  let rt = Runtime::start(&config(3)).unwrap();
  for id in rt.shard_ids().to_vec() {
    rt.spawn_on(id, async {
      sleep(60_000_000_000).await;
    })
    .unwrap();
  }
  // Let the spawns land before the shutdown message.
  std::thread::yield_now();
  let counters = rt.shutdown();
  assert_eq!(counters.len(), 3);
  let cancelled: u64 = counters.iter().map(|c| c.cancelled).sum();
  let spawned: u64 = counters.iter().map(|c| c.spawns).sum();
  assert_eq!(cancelled, spawned, "{counters:?}");
}
