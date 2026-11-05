//! A spinning shard keeps its core (§4.3 "Loop", §4.7 "Shards poll rings while any client has activity within
//! the measured idle window"). Inside the window a client's activity opened, an idle shard spins: each turn asks its
//! rings, its pollers and its driver, the driver by a poll that must not block, so that a message landing during the
//! spin costs a cache-line transfer instead of a kernel wake. A poll that gives the core up on each turn makes every
//! turn a trip through the scheduler, and a message waits that trip out: the spin then saves nothing.
//!
//! On macOS the driver polled with a zero-valued timeout, which kevent(2) names as one way to poll. On macOS 26.4.1
//! (Apple M5 Max, 2026-11-05) that call took 14.6–15.5 µs and cost one involuntary context switch a call, where
//! `KEVENT_FLAG_IMMEDIATE` took 180–186 ns and none. A cross-shard round trip between two spinning shards then cost
//! what one between two parked shards did (7.8–8.5 µs against 6.5–8.9 µs, `rt_bench`, three rounds)
//! (`docs/bugs/2026-11-05-a-spinning-shard-gave-its-core-up-on-every-turn.md`).
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use slates_rt::futures::yield_now;
use slates_rt::registry;
use slates_rt::runtime::{Runtime, RuntimeConfig};

/// Shape: the idle window after each turn, long against a round trip (microseconds) so that each reply lands inside
/// the spin, and short enough that the shards park soon after the test.
const WINDOW_NS: u64 = 50_000_000;
/// Shape: round trips per form, enough for a median that one scheduling hiccup cannot move; a few tens of
/// milliseconds at a parked round trip's cost.
const ROUNDS: usize = 2_000;
/// Shape: how much cheaper the spinning round trip must be. A round trip between parked shards pays two kernel wakes;
/// between spinning shards two cache-line transfers. At half the parked cost, the spin has saved less than one of
/// the two wakes it exists to save.
const SAVING: u64 = 2;
/// Shape: how long the test waits for the ping-pong to report (a round trip is microseconds).
const ANSWER: Duration = Duration::from_secs(30);

/// Whose turn it is: 0 or 1, and `u64::MAX` once the test is over.
static TURN: AtomicU64 = AtomicU64::new(0);
/// Each side's waker word, for the other side to wake it.
static WAKERS: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
/// One bit per side, set once the side's waker word is known.
static REGISTERED: AtomicU64 = AtomicU64::new(0);

fn config(spin_ns: u64) -> RuntimeConfig {
  RuntimeConfig {
    shards: 2,
    tasks_per_shard: 64,
    timers_per_shard: 64,
    ring_entries: 64,
    step_budget_ns: 1_000_000_000,
    timer_tick_ns: 100_000,
    batch: 64,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns,
    wake_tracking: None,
  }
}

/// Waits for `TURN` to equal `side`, publishing this task's waker word for the other side.
struct AwaitTurn {
  side: usize,
}

impl std::future::Future for AwaitTurn {
  type Output = ();
  fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<()> {
    if let Some(word) = slates_rt::waker::word_of(cx.waker()) {
      WAKERS
        .get(self.side)
        .unwrap()
        .store(word.word(), Ordering::Release);
      REGISTERED.fetch_or(1 << self.side, Ordering::AcqRel);
    }
    let side = u64::try_from(self.side).unwrap();
    let turn = TURN.load(Ordering::Acquire);
    if turn == side || turn == u64::MAX {
      std::task::Poll::Ready(())
    } else {
      std::task::Poll::Pending
    }
  }
}

fn wake_side(side: usize) {
  let word = WAKERS.get(side).unwrap().load(Ordering::Acquire);
  registry::wake(slates_mem::Encoded::from_word(word));
}

/// Hands the turn to `side` and wakes it; a spinning run notes the hand-off as client activity first, so this shard
/// spins out the window while the other side answers.
fn hand_turn_to(side: usize, spinning: bool) {
  if spinning {
    registry::with_current(|ctx| ctx.note_activity());
  }
  TURN.store(u64::try_from(side).unwrap(), Ordering::Release);
  wake_side(side);
}

/// The median of `ROUNDS` round trips between a task on each of two shards, spinning between turns or parking.
fn median_round_trip_ns(spinning: bool) -> u64 {
  TURN.store(0, Ordering::Release);
  REGISTERED.store(0, Ordering::Release);
  let rt = Runtime::start(&config(if spinning { WINDOW_NS } else { 0 })).unwrap();
  let (first, second) = (
    *rt.shard_ids().first().unwrap(),
    *rt.shard_ids().get(1).unwrap(),
  );
  rt.spawn_on(second, async move {
    loop {
      AwaitTurn { side: 1 }.await;
      if TURN.load(Ordering::Acquire) == u64::MAX {
        break;
      }
      hand_turn_to(0, spinning);
    }
  })
  .unwrap();
  let (tx, rx) = channel::<Vec<u64>>();
  rt.spawn_on(first, async move {
    AwaitTurn { side: 0 }.await;
    while REGISTERED.load(Ordering::Acquire) & 2 == 0 {
      yield_now().await;
    }
    let mut times = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
      let started = Instant::now();
      hand_turn_to(1, spinning);
      AwaitTurn { side: 0 }.await;
      times.push(u64::try_from(started.elapsed().as_nanos()).unwrap());
    }
    let _ = tx.send(times);
  })
  .unwrap();
  let mut times = rx.recv_timeout(ANSWER).unwrap();
  TURN.store(u64::MAX, Ordering::Release);
  wake_side(1);
  rt.shutdown().unwrap();
  times.sort_unstable();
  *times.get(times.len() / 2).unwrap()
}

/// §4.3, §4.7, R9. Do: time round trips between a task on each of two shards, first with both shards parking between
/// turns, then with both spinning out a window after each. Expect: the spinning round trip's median is under half
/// the parked one's. Before the fix, a macOS shard's poll gave its core up on every spin turn, and the two cost the
/// same.
#[test]
fn a_spinning_shard_answers_sooner_than_a_parked_one() {
  let parked = median_round_trip_ns(false);
  let spinning = median_round_trip_ns(true);
  println!("median cross-shard round trip: parked {parked} ns, spinning {spinning} ns");
  assert!(
    spinning.saturating_mul(SAVING) < parked,
    "a round trip between spinning shards ({spinning} ns) saved less than half of one between parked shards \
     ({parked} ns)"
  );
}
