//! AUD-29-39 (§4.3, R3/R6, bans 8/9): a sleep completes no earlier than its deadline or is refused typed —
//! never a false elapsed time. A wheel with no free timer makes the sleep wait for one (counted, bounded by
//! the task arena), not complete at once; a cancelled sleep gives its timer to the next waiter; a sleep polled
//! off a shard is refused; a deadline past the clock's range never fires and owns no timer once dropped.
//! Until 2026-09-30, in a one-timer shard, the second of two `sleep(10_000)` completed after 0 ns. These run
//! on the simulated clock, so Miri runs them too.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc::channel;
use std::task::{Context, Poll, Waker};

use slates_rt::error::RtError;
use slates_rt::futures::{cancel, now_ns, sleep, spawn, within};
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::SimRuntime;

/// Shape: the span every waiting sleep asks for, in the audit's words.
const SPAN_NS: u64 = 10_000;
/// Shape: a sleep long enough that nothing else in a test outlasts it.
const LONG_NS: u64 = 1_000_000_000;

/// One shard whose wheel holds exactly one timer.
fn one_timer() -> RuntimeConfig {
  RuntimeConfig {
    shards: 1,
    tasks_per_shard: 16,
    timers_per_shard: 1,
    ring_entries: 16,
    step_budget_ns: 1_000_000_000,
    timer_tick_ns: 1_000,
    batch: 16,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
    wake_tracking: None,
  }
}

/// AUD-29-39: do: in a one-timer shard, two tasks each `sleep(10_000)`; expect both to complete no earlier
/// than 10,000 ns after they began — the second having waited for the first's timer (counted in
/// `timer_waits`) — and neither refused.
#[test]
fn a_sleep_facing_a_full_wheel_waits_rather_than_completing_early() {
  let mut sim = SimRuntime::new(&one_timer(), 1).unwrap();
  let shard = sim.shard_ids()[0];
  let (tx, rx) = channel();
  for _ in 0..2 {
    let tx = tx.clone();
    sim
      .spawn_on(shard, async move {
        let began = now_ns();
        let slept = sleep(SPAN_NS).await;
        let _ = tx.send((slept, now_ns().saturating_sub(began)));
      })
      .unwrap();
  }
  sim.run_until_idle();
  for _ in 0..2 {
    let (slept, elapsed) = rx.recv().unwrap();
    assert_eq!(slept, Ok(()));
    assert!(elapsed >= SPAN_NS, "slept {elapsed} ns of {SPAN_NS}");
  }
  let counters = sim.context(shard).unwrap().counters();
  assert!(
    counters.timer_waits >= 1,
    "the second sleep waited for a timer: {counters:?}"
  );
}

/// AUD-29-39: do: in a one-timer shard, one task sleeps a second and holds the timer; a second task asks for
/// 10,000 ns and waits; cancel the first; expect the second to complete no earlier than its deadline and
/// long before the first's, on the timer the cancellation freed.
#[test]
fn a_cancelled_sleep_hands_its_timer_to_the_waiter() {
  let mut sim = SimRuntime::new(&one_timer(), 2).unwrap();
  let shard = sim.shard_ids()[0];
  let (tx, rx) = channel();
  sim
    .spawn_on(shard, async move {
      let holder = spawn(async {
        let _ = sleep(LONG_NS).await;
      })
      .unwrap();
      let waiter = spawn(async move {
        let began = now_ns();
        let slept = sleep(SPAN_NS).await;
        let _ = tx.send((slept, now_ns().saturating_sub(began)));
      })
      .unwrap();
      // Both have been polled once: the holder armed the only timer, the waiter queued for it.
      slates_rt::futures::yield_now().await;
      slates_rt::futures::yield_now().await;
      cancel(holder).unwrap();
      let _ = slates_rt::futures::join(waiter).await;
    })
    .unwrap();
  sim.run_until_idle();
  let (slept, elapsed) = rx.recv().unwrap();
  assert_eq!(slept, Ok(()));
  assert!(elapsed >= SPAN_NS, "slept {elapsed} ns of {SPAN_NS}");
  assert!(
    elapsed < LONG_NS,
    "woken by the freed timer, not the holder's end"
  );
}

/// A waker that is not a task's.
fn foreign_context() -> Context<'static> {
  Context::from_waker(Waker::noop())
}

/// AUD-29-39: do: poll a sleep outside any shard, with a waker that is not a task's; expect the refusal
/// `NotOnShardThread`, never an immediate `Ok` read as elapsed time.
#[test]
fn a_sleep_polled_off_a_shard_is_refused() {
  let mut sleeping = std::pin::pin!(sleep(SPAN_NS));
  assert_eq!(
    std::future::Future::poll(sleeping.as_mut(), &mut foreign_context()),
    Poll::Ready(Err(RtError::NotOnShardThread))
  );
}

/// AUD-29-39: do: in a two-timer shard (the saturated sleep's and the race's deadline), a task races
/// `sleep(u64::MAX)` against 10,000 ns, drops it, then sleeps 10,000 ns twice at once; expect the saturated
/// sleep not to complete, and both later sleeps to arm without waiting — on the timers the dropped ones gave
/// back (no orphaned timer) — and complete on time.
#[test]
fn a_deadline_past_the_clock_never_fires_and_its_timer_returns_on_drop() {
  let mut config = one_timer();
  config.timers_per_shard = 2;
  let mut sim = SimRuntime::new(&config, 3).unwrap();
  let shard = sim.shard_ids()[0];
  let (tx, rx) = channel();
  sim
    .spawn_on(shard, async move {
      let forever = within(SPAN_NS, sleep(u64::MAX)).await;
      let began = now_ns();
      let (first, second) = (sleep(SPAN_NS), sleep(SPAN_NS));
      let slept = within(LONG_NS, async { (first.await, second.await) }).await;
      let _ = tx.send((forever, slept, now_ns().saturating_sub(began)));
    })
    .unwrap();
  sim.run_until_idle();
  let (forever, slept, elapsed) = rx.recv().unwrap();
  assert_eq!(forever, Ok(None), "the saturated sleep did not complete");
  assert_eq!(slept, Ok(Some((Ok(()), Ok(())))));
  assert!(elapsed >= SPAN_NS);
  assert_eq!(sim.context(shard).unwrap().counters().timer_waits, 0);
}

/// AUD-29-39: do: in a one-timer shard, one task holds the timer for a second and another waits for it; drop
/// the runtime; expect the drop to return (both tasks cancelled, the waiter's queue entry and the held timer
/// released) — no hang and no leaked task.
#[test]
fn shutdown_ends_a_sleep_waiting_for_a_timer() {
  let mut sim = SimRuntime::new(&one_timer(), 4).unwrap();
  let shard = sim.shard_ids()[0];
  for _ in 0..2 {
    sim
      .spawn_on(shard, async {
        let _ = sleep(LONG_NS).await;
      })
      .unwrap();
  }
  for _ in 0..4 {
    let _ = sim.context(shard).unwrap().step();
  }
  let counters = sim.context(shard).unwrap().counters();
  assert_eq!(counters.timer_waits, 1, "one sleep waits: {counters:?}");
  drop(sim);
}

/// AUD-29-39: do: race work against a deadline with `within` — work that finishes first, work that never
/// does, and the race polled off a shard; expect `Ok(Some(output))`, `Ok(None)` no earlier than the deadline,
/// and the refusal — the three answers kept apart.
#[test]
fn within_tells_the_work_the_deadline_and_a_refusal_apart() {
  let mut sim = SimRuntime::new(&one_timer(), 5).unwrap();
  let shard = sim.shard_ids()[0];
  let (tx, rx) = channel();
  sim
    .spawn_on(shard, async move {
      let finished = within(SPAN_NS, async { 7 }).await;
      let began = now_ns();
      let expired = within(SPAN_NS, std::future::pending::<u8>()).await;
      let _ = tx.send((finished, expired, now_ns().saturating_sub(began)));
    })
    .unwrap();
  sim.run_until_idle();
  let (finished, expired, elapsed) = rx.recv().unwrap();
  assert_eq!(finished, Ok(Some(7)));
  assert_eq!(expired, Ok(None));
  assert!(elapsed >= SPAN_NS);
  let mut off_shard = std::pin::pin!(within(SPAN_NS, std::future::pending::<u8>()));
  assert_eq!(
    std::future::Future::poll(off_shard.as_mut(), &mut foreign_context()),
    Poll::Ready(Err(RtError::NotOnShardThread))
  );
}
