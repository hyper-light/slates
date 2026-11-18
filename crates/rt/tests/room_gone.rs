//! A sender may wait for room only in a shard that can still make it (§4.3 "admission limit"; `slates_rt::room`). A
//! shard that has left its loop, or an id no shard holds, will never drain again, so an ask for room there is refused
//! `ShardGone` at once and nothing waits on it: the daemon then refuses its kept forwards to that owner typed rather
//! than idle on them (`slates_server::verbs::ask_room_for_kept_forwards`). In a binary of its own because the test stops
//! a shard, and a runtime started beside it in the same process could otherwise take its registry slot.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use slates_rt::control::Control;
use slates_rt::registry;
use slates_rt::runtime::{Runtime, RuntimeConfig};
use slates_rt::{RtError, ShardId, futures};

/// Shape: how long the test waits for each step it expects (the stopped shard leaving its loop, the asks' answer) before
/// calling it lost: far past a shard's exit and one task's poll, on any machine.
const WAIT: Duration = Duration::from_secs(10);

fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 2,
    tasks_per_shard: 16,
    timers_per_shard: 64,
    ring_entries: 64,
    step_budget_ns: 1_000_000_000,
    timer_tick_ns: 100_000,
    batch: 4,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
    wake_tracking: None,
  }
}

/// Stops `shard` alone (its own `Shutdown`, the others running on) and waits until a spawn to it is refused as gone:
/// its loop has returned and its channel's receiving end is dropped.
fn stop_alone(rt: &Runtime, shard: ShardId) {
  registry::send_control(shard.0, Control::Shutdown).unwrap();
  let started = Instant::now();
  while !matches!(rt.spawn_on(shard, async {}), Err(RtError::ShardGone { .. })) {
    assert!(
      started.elapsed() < WAIT,
      "the stopped shard left its loop within {WAIT:?}"
    );
    std::thread::yield_now();
  }
}

/// What asks for room in each of `targets`, made from a task on `asker`, were answered.
fn asks_from(rt: &Runtime, asker: ShardId, targets: [u16; 2]) -> [Result<(), RtError>; 2] {
  let (answer, answered) = std::sync::mpsc::channel();
  rt.spawn_on(asker, async move {
    let _ = answer.send(targets.map(futures::want_room));
  })
  .unwrap();
  answered.recv_timeout(WAIT).expect("the asks were answered")
}

/// §4.3, the room wake. Do: on a two-shard runtime, stop the second shard alone and wait until a spawn to it is refused
/// as gone; then, from a task on the first, ask for room in the stopped shard and in an id no shard holds. Expect: both
/// asks refused `ShardGone` naming the shard asked, and neither counted among the first shard's asks that wait.
#[test]
fn an_ask_for_room_in_a_shard_that_will_never_drain_is_refused_at_once() {
  let rt = Runtime::start(&config()).unwrap();
  let (asker, stopped) = (rt.shard_ids()[0], rt.shard_ids()[1]);
  stop_alone(&rt, stopped);
  // No registry slot has the largest id: the registry holds far fewer.
  let unheld = u16::MAX;
  let [gone, never_held] = asks_from(&rt, asker, [stopped.0, unheld]);
  assert!(
    matches!(gone, Err(RtError::ShardGone { shard }) if shard == stopped.0),
    "an ask for room in a stopped shard: {gone:?}"
  );
  assert!(
    matches!(never_held, Err(RtError::ShardGone { shard }) if shard == unheld),
    "an ask for room in an id no shard holds: {never_held:?}"
  );
  let counters = rt.shutdown().unwrap();
  assert_eq!(
    counters.first().map(|first| first.room_waits),
    Some(0),
    "a refused ask is not counted as one that waits"
  );
}
