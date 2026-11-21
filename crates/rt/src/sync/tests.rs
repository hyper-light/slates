//! The channel between every pair of waiters: thread and thread, a task receiving from a thread, a task sending into
//! a full channel a thread drains, and each end's going (A-767 piece C2).

use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::*;
use crate::runtime::{LocalRuntime, RuntimeConfig};

/// Shape: a runtime for a few tasks on one shard.
fn runtime() -> LocalRuntime {
  LocalRuntime::new(&RuntimeConfig {
    shards: 1,
    tasks_per_shard: 8,
    timers_per_shard: 8,
    ring_entries: 64,
    step_budget_ns: 1_000_000,
    timer_tick_ns: 100_000,
    batch: 8,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
    wake_tracking: None,
  })
  .unwrap()
}

/// Shape: how long a test drives its shard before it gives up, far past any wake.
const BOUND: Duration = Duration::from_secs(20);

/// Steps the shard until the task reports on `report` or [`BOUND`] passes; the report, if one came. A wake from
/// another thread reaches the shard's inbox, which a step drains.
fn drive<R>(runtime: &LocalRuntime, report: &mpsc::Receiver<R>) -> Option<R> {
  let deadline = Instant::now() + BOUND;
  while Instant::now() < deadline {
    if let Ok(reported) = report.try_recv() {
      return Some(reported);
    }
    let _ = runtime.context().step();
    std::thread::yield_now();
  }
  report.try_recv().ok()
}

/// Do: send 1,000 values through a channel of two from one plain thread to another. Expect: every value arrives,
/// in order, and the receiver sees `Closed` once the sender is gone.
#[test]
fn two_threads_pass_every_value_in_order() {
  let (sender, mut receiver) = channel::<u32>(2).unwrap();
  let sending = std::thread::spawn(move || {
    for value in 0..1_000 {
      sender.blocking_send(value).unwrap();
    }
  });
  for expected in 0..1_000 {
    assert_eq!(receiver.blocking_recv().unwrap(), expected);
  }
  sending.join().unwrap();
  assert_eq!(receiver.blocking_recv(), Err(SyncError::Closed(())));
}

/// Do: a task awaits 500 values a plain thread sends into a channel of four. Expect: the task is woken for each and
/// takes all 500 in order, then `Closed` once the thread's sender is gone.
#[test]
fn a_task_receives_what_a_thread_sends() {
  let runtime = runtime();
  let (sender, mut receiver) = channel::<u32>(4).unwrap();
  let (report, reported) = mpsc::channel();
  runtime
    .spawn(async move {
      let mut taken = 0u32;
      loop {
        match receiver.recv().await {
          Ok(value) if value == taken => taken += 1,
          other => {
            let _ = report.send((taken, other.map(|_| ())));
            return;
          }
        }
      }
    })
    .unwrap();
  let sending = std::thread::spawn(move || {
    for value in 0..500 {
      sender.blocking_send(value).unwrap();
    }
  });
  let answer = drive(&runtime, &reported);
  sending.join().unwrap();
  assert_eq!(
    answer,
    Some((500, Err(SyncError::Closed(())))),
    "every value in order, then the close"
  );
}

/// Do: a task sends 300 values into a channel of one that a plain thread drains slowly. Expect: the task waits for
/// room in a place, is granted it by each take, and every value arrives in order.
#[test]
fn a_task_waits_for_room_a_thread_makes() {
  let runtime = runtime();
  let (sender, mut receiver) = channel::<u32>(1).unwrap();
  let (report, reported) = mpsc::channel();
  runtime
    .spawn(async move {
      let mut outcome = Ok(());
      for value in 0..300 {
        outcome = sender.send(value).await;
        if outcome.is_err() {
          break;
        }
      }
      let _ = report.send(outcome);
    })
    .unwrap();
  let draining = std::thread::spawn(move || {
    for expected in 0..300 {
      assert_eq!(receiver.blocking_recv().unwrap(), expected);
      if expected % 50 == 0 {
        std::thread::sleep(Duration::from_millis(2));
      }
    }
    receiver
  });
  assert_eq!(
    drive(&runtime, &reported),
    Some(Ok(())),
    "the task sent all 300"
  );
  let mut receiver = draining.join().unwrap();
  assert_eq!(
    receiver.try_recv(),
    Err(SyncError::Closed(())),
    "the task's sender is gone with the task"
  );
}

/// Do: fill a channel of one, try another send, take, drop the receiver, send again. Expect: `Full` with the value
/// handed back, then room, then `Closed` with the value handed back.
#[test]
fn full_and_closed_hand_the_value_back() {
  let (sender, mut receiver) = channel::<&str>(1).unwrap();
  assert_eq!(receiver.try_recv(), Ok(None));
  sender.try_send("first").unwrap();
  assert_eq!(sender.try_send("second"), Err(SyncError::Full("second")));
  assert_eq!(receiver.try_recv(), Ok(Some("first")));
  drop(receiver);
  assert_eq!(sender.try_send("third"), Err(SyncError::Closed("third")));
}

/// Do: a task waits to send into a full channel, then the receiver goes. Expect: the waiting task is woken and told
/// `Closed`, its value handed back.
#[test]
fn a_waiting_sender_learns_the_receiver_went() {
  let runtime = runtime();
  let (sender, receiver) = channel::<u32>(1).unwrap();
  sender.try_send(0).unwrap();
  let (report, reported) = mpsc::channel();
  runtime
    .spawn(async move {
      let _ = report.send(sender.send(1).await);
    })
    .unwrap();
  let _ = runtime.context().step();
  assert!(
    reported.try_recv().is_err(),
    "the send waits while the channel is full"
  );
  let dropping = std::thread::spawn(move || drop(receiver));
  dropping.join().unwrap();
  assert_eq!(drive(&runtime, &reported), Some(Err(SyncError::Closed(1))));
}

/// Do: make a channel, clone its sender, drop every end. Expect: the channel's cells are back in the table.
#[test]
fn a_channel_gives_its_cells_back() {
  let (sender, receiver) = channel_with::<u8>(2, 3).unwrap();
  let cell = sender.cell;
  let more = sender.clone();
  drop(sender);
  drop(receiver);
  assert!(cell.cell().is_some(), "a sender still holds the channel");
  drop(more);
  assert!(cell.cell().is_none(), "the last end gave the cell back");
}

/// Do: refuse a zero capacity, and a blocking call on a shard. Expect: `BadConfig`, and `NotOnShardThread` with the
/// value handed back (a shard never blocks).
#[test]
fn a_bad_channel_and_a_blocking_call_on_a_shard_are_refused() {
  assert!(matches!(
    channel::<u8>(0),
    Err(SyncRefusal::BadConfig { .. })
  ));
  assert!(matches!(
    channel_with::<u8>(1, 0),
    Err(SyncRefusal::BadConfig { .. })
  ));
  let runtime = runtime();
  let (sender, _receiver) = channel::<u8>(1).unwrap();
  let (report, reported) = mpsc::channel();
  runtime
    .spawn(async move {
      let _ = report.send(sender.blocking_send(7));
    })
    .unwrap();
  assert_eq!(
    drive(&runtime, &reported),
    Some(Err(SyncError::NotOnShardThread(7)))
  );
}
