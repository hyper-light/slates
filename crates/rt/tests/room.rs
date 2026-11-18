//! A send a full control channel refuses is sent again once the channel has room, told by the drain that makes the room
//! (§4.3 "admission limit": a refused spawn is backpressure, kept and sent again, never dropped; `slates_rt::room`).
//! Until 2026-11-17 nothing told the sender: the daemon's serve loop tried a kept forward again once per idle window, a
//! window that grows with the machine's load, so the forward waited for a timer while its owner idled
//! (`docs/bugs/2026-11-17-a-kept-forward-polled-for-room-and-its-test-timed-the-owners-queue.md`). The sender here has no timer at all: only
//! the drain's word can move it.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use slates_rt::control::Control;
use slates_rt::registry::{self, RefusedControl};
use slates_rt::runtime::{Runtime, RuntimeConfig};
use slates_rt::task::SpawnRequest;
use slates_rt::{Admission, RtError, futures};

/// Shape: each shard's task arena, and so its control channel's bound: a channel the test fills in a moment.
const TASKS: usize = 16;
/// Shape: the drain batch, a quarter of the channel, so the held shard's channel empties over several drains.
const BATCH: usize = 4;
/// Shape: how long the test waits for each step it expects (the hold to begin, the ask, the send) before calling it
/// lost: far past a drain of sixteen messages and a cross-shard wake, on any machine.
const WAIT: Duration = Duration::from_secs(10);

/// The flood's tasks that ran on the held shard.
static FLOOD_RAN: AtomicU64 = AtomicU64::new(0);
/// The sender's asks for room whose second try was refused too.
static ASKED: AtomicU64 = AtomicU64::new(0);
/// The sender's message ran on the held shard.
static LANDED: AtomicBool = AtomicBool::new(false);

fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 2,
    tasks_per_shard: TASKS,
    timers_per_shard: 64,
    ring_entries: 64,
    step_budget_ns: 1_000_000_000,
    timer_tick_ns: 100_000,
    batch: BATCH,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
    wake_tracking: None,
  }
}

/// Sends `message` to `target`, asking for room whenever the channel is full: the send, an ask, a second send, and only
/// then a wait with no deadline and no timer, which only a wake ends.
async fn send_when_room(target: u16, mut message: Control) {
  loop {
    message = match registry::send_control_or_return(target, message) {
      Ok(()) => return,
      Err(RefusedControl {
        error: RtError::ControlFull { .. },
        message: Some(unsent),
      }) => unsent,
      Err(refused) => panic!("the send was refused: {}", refused.error),
    };
    futures::want_room(target).unwrap();
    message = match registry::send_control_or_return(target, message) {
      Ok(()) => return,
      Err(RefusedControl {
        error: RtError::ControlFull { .. },
        message: Some(unsent),
      }) => unsent,
      Err(refused) => panic!("the second try was refused: {}", refused.error),
    };
    // Counted only once the second try is refused too, so the test releases the held shard only when nothing but the
    // room wake can move this task.
    ASKED.fetch_add(1, Ordering::SeqCst);
    futures::idle().await;
  }
}

/// Waits up to [`WAIT`] for `done`.
fn wait_for(what: &str, done: impl Fn() -> bool) {
  let started = Instant::now();
  while !done() {
    assert!(started.elapsed() < WAIT, "{what} within {WAIT:?}");
    std::thread::yield_now();
  }
}

/// §4.3, the room wake. Do: on a two-shard runtime, hold the second shard and fill its control channel; from a task on
/// the first, send one more message, ask for room when it is refused, and wait with no timer; then release the held
/// shard. Expect: the message lands once the held shard drains, the flood before it runs whole, and the first shard
/// counted the room wake that moved its task. Before 2026-11-17 there was no ask: such a task waited for good.
#[test]
fn a_send_refused_for_a_full_channel_is_told_when_the_channel_drains() {
  let rt = Runtime::start(&config()).unwrap();
  let (sender, held) = (rt.shard_ids()[0], rt.shard_ids()[1]);
  // The hold spins until the test releases it, so the flood and the send queue behind it on any machine.
  let (release, released) = std::sync::mpsc::channel::<()>();
  let (running, started) = std::sync::mpsc::channel::<()>();
  let hold = rt
    .spawn_on_with_receipt(held, async move {
      let _ = running.send(());
      while matches!(
        released.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
      ) {
        std::hint::spin_loop();
      }
    })
    .unwrap();
  assert!(matches!(hold.wait(WAIT), Some(Admission::Admitted(_))));
  started.recv_timeout(WAIT).expect("the hold is running");
  let mut flooded = 0u64;
  loop {
    match rt.spawn_on(held, async {
      FLOOD_RAN.fetch_add(1, Ordering::SeqCst);
    }) {
      Ok(()) => flooded += 1,
      Err(RtError::ControlFull { .. }) => break,
      Err(e) => panic!("{e}"),
    }
  }
  assert!(flooded > 0, "the held shard's channel took the flood");
  let target = held.0;
  rt.spawn_on(sender, async move {
    let message = Control::Spawn(Box::new(SpawnRequest::new(
      Box::pin(async {
        LANDED.store(true, Ordering::SeqCst);
      }),
      None,
    )));
    send_when_room(target, message).await;
  })
  .unwrap();
  wait_for("the sender asked for room", || {
    ASKED.load(Ordering::SeqCst) > 0
  });
  release.send(()).unwrap();
  wait_for("the sender's message landed", || {
    LANDED.load(Ordering::SeqCst)
  });
  assert_eq!(
    FLOOD_RAN.load(Ordering::SeqCst),
    flooded,
    "the flood before the message ran whole"
  );
  let counters = rt.shutdown().unwrap();
  let first = counters.first().expect("the sender's shard's counters");
  assert!(
    first.room_waits >= 1 && first.room_wakes >= 1,
    "the sender's shard asked for room and was woken by it: {} asks, {} wakes",
    first.room_waits,
    first.room_wakes
  );
}
