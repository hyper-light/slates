//! A waker fired from a thread outside the runtime reaches the task it was made for, on every pointer width
//! (§4.3; `docs/bugs/2026-10-01-a-32-bit-waker-lost-its-task.md`). A 32-bit target's waker carries the shard
//! and slot only (the 64-bit word does not fit its data pointer), so its wake is a slot wake; before that, a
//! word past 32 bits — any task on a shard other than 0 — became 0, naming shard 0, slot 0, generation 0.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::pin::Pin;
use std::sync::mpsc::{Sender, channel};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use slates_rt::runtime::{Runtime, RuntimeConfig};

/// Shape: two shards (the tasks park on the second, whose word does not fit 32 bits), room for the two
/// parked tasks, the smallest ring and timer table; the step budget and
/// tick are the runtime tests' usual values (`tests/reclaim.rs`).
fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 2,
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
    wake_tracking: None,
  }
}

/// Shape: how long the test waits for a task to report — far past a wake's microseconds, short enough that
/// a lost wake fails the test rather than hanging it.
const REPORT_WAIT: Duration = Duration::from_secs(5);

/// A task that hands its waker out on its first poll and stays pending, then finishes (reporting its name)
/// on the poll a wake brings.
struct Parked {
  name: &'static str,
  wakers: Sender<(&'static str, Waker)>,
  finished: Sender<&'static str>,
  handed_out: bool,
}

impl Future for Parked {
  type Output = ();
  fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
    if self.handed_out {
      let _ = self.finished.send(self.name);
      return Poll::Ready(());
    }
    self.handed_out = true;
    let _ = self.wakers.send((self.name, cx.waker().clone()));
    Poll::Pending
  }
}

/// T-0.4, AUD-29-32. Do: park two tasks on the runtime's second shard, then wake only the second task from a
/// thread outside the runtime, then the first. Expect: the second finishes and the first does not until its
/// own waker fires — a waker reaches the task it was made for, not whatever task a lost word names.
#[test]
#[cfg_attr(miri, ignore)] // the OS driver opens a kqueue or an eventfd, which Miri does not model
fn a_waker_fired_from_another_thread_wakes_the_task_it_was_made_for() {
  let runtime = Runtime::start(&config()).unwrap();
  let shard = runtime.shard_ids()[1];
  let (wakers_tx, wakers) = channel();
  let (finished_tx, finished) = channel();
  for name in ["first", "second"] {
    runtime
      .spawn_on(
        shard,
        Parked {
          name,
          wakers: wakers_tx.clone(),
          finished: finished_tx.clone(),
          handed_out: false,
        },
      )
      .unwrap();
  }
  let mut first = None;
  let mut second = None;
  for _ in 0..2 {
    match wakers.recv_timeout(REPORT_WAIT).unwrap() {
      ("first", waker) => first = Some(waker),
      (_, waker) => second = Some(waker),
    }
  }
  let (first, second) = (first.unwrap(), second.unwrap());

  std::thread::spawn(move || second.wake()).join().unwrap();
  assert_eq!(
    finished.recv_timeout(REPORT_WAIT),
    Ok("second"),
    "the second task's waker woke the second task"
  );
  assert!(
    finished.try_recv().is_err(),
    "the first task was not woken by the second's waker"
  );

  std::thread::spawn(move || first.wake()).join().unwrap();
  assert_eq!(finished.recv_timeout(REPORT_WAIT), Ok("first"));
  let counters = runtime.shutdown().unwrap();
  assert_eq!(counters[1].completed, 2);
}
