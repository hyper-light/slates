//! Admission receipts (§4.3 "admission"): a task submitted from another thread reports whether the
//! shard admitted it, refused it, or terminated it unadmitted, so a submitter never mistakes a
//! submission for an admission; a submission pinned to a slot's holder is refused once the slot is
//! reused; and a shutdown lands although its message first meets a full control channel.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use slates_rt::control::Control;
use slates_rt::futures::{now_ns, sleep};
use slates_rt::runtime::{Runtime, RuntimeConfig, submit_to_holder};
use slates_rt::{Admission, AdmissionReceipt, RtError, ShardId, registry};

/// Shape: the arena of these tests — small, so a full arena is a handful of parked tasks.
const ARENA: usize = 4;
/// Shape: how long a receipt or a reply is waited for before the test calls it lost: far past a
/// shard's step and a held shard's spin, so a slow box does not fail a correct runtime.
const WAIT: Duration = Duration::from_secs(10);
/// Shape: a wait that must *not* be satisfied is watched this long — long enough that a task which
/// was going to run has run.
const QUIET: Duration = Duration::from_millis(200);
/// Shape: how long a held shard spins, nanoseconds: long enough for the submissions made meanwhile
/// to queue behind it, short enough for the test.
const HOLD_NS: u64 = 300_000_000;
/// Shape: a parked filler's hop between checks of its release flag, nanoseconds.
const HOP_NS: u64 = 1_000_000;

fn config(tasks_per_shard: usize) -> RuntimeConfig {
  RuntimeConfig {
    shards: 1,
    tasks_per_shard,
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

/// Holds `shard` inside one poll for [`HOLD_NS`] — a task that spins on the shard clock — and
/// returns once the shard has admitted it, so everything submitted after this queues behind the hold.
fn hold(rt: &Runtime, shard: ShardId) {
  let receipt = rt
    .spawn_on_with_receipt(shard, async {
      let end = now_ns().saturating_add(HOLD_NS);
      while now_ns() < end {
        std::hint::spin_loop();
      }
    })
    .unwrap();
  assert!(
    matches!(receipt.wait(WAIT), Some(Admission::Admitted(_))),
    "the hold is admitted"
  );
}

/// Submits a task that reports on a channel when it runs; the receipt and the report's receiver.
fn submit_reporter(rt: &Runtime, shard: ShardId) -> (AdmissionReceipt, Receiver<()>) {
  let (tx, rx) = channel();
  let receipt = rt
    .spawn_on_with_receipt(shard, async move {
      let _ = tx.send(());
    })
    .unwrap();
  (receipt, rx)
}

/// A receipt names the admitted task on the shard it was submitted to, and that task runs.
#[test]
fn a_receipt_names_the_admitted_task_and_the_task_runs() {
  let rt = Runtime::start(&config(ARENA)).unwrap();
  let shard = rt.shard_ids()[0];
  let (receipt, ran) = submit_reporter(&rt, shard);
  let admission = receipt.wait(WAIT).expect("the shard drains the request");
  assert!(
    matches!(admission, Admission::Admitted(task) if task.0.shard() == shard.0),
    "admitted on the shard submitted to: {admission:?}"
  );
  ran
    .recv_timeout(WAIT)
    .expect("the admitted task ran and reported");
  rt.shutdown();
}

/// Whether the fillers may end (the arena test's release flag).
static RELEASE: AtomicBool = AtomicBool::new(false);

/// Fills `shard`'s arena with tasks parked on [`RELEASE`]: submits until a receipt is refused, and
/// returns how many were admitted and the capacity the refusal named.
fn fill_arena(rt: &Runtime, shard: ShardId) -> (usize, usize) {
  let mut admitted = 0;
  for _ in 0..ARENA * 2 {
    let receipt = rt
      .spawn_on_with_receipt(shard, async {
        while !RELEASE.load(Ordering::Acquire) {
          sleep(HOP_NS).await;
        }
      })
      .unwrap();
    match receipt.wait(WAIT) {
      Some(Admission::Admitted(_)) => admitted += 1,
      Some(Admission::Refused(RtError::TooManyTasks { capacity })) => return (admitted, capacity),
      other => panic!("a filler met {other:?}"),
    }
  }
  panic!(
    "{} fillers were admitted into an arena of {ARENA}",
    ARENA * 2
  );
}

/// A receptive control channel and a full arena: the request is submitted, and the receipt reports
/// the admission refused with the arena's capacity; the refused task never runs. Once a filler ends,
/// the same submission is admitted and runs.
#[test]
fn a_full_arena_refuses_on_the_receipt_and_admits_once_a_task_ends() {
  let rt = Runtime::start(&config(ARENA)).unwrap();
  let shard = rt.shard_ids()[0];
  let (admitted, capacity) = fill_arena(&rt, shard);
  assert_eq!(admitted, capacity, "every slot of the arena was filled");
  let (receipt, ran) = submit_reporter(&rt, shard);
  assert_eq!(
    receipt.wait(WAIT),
    Some(Admission::Refused(RtError::TooManyTasks { capacity })),
    "submitted (the channel is receptive), refused at admission (the arena is full)"
  );
  assert!(
    ran.recv_timeout(QUIET).is_err(),
    "a refused task never runs"
  );
  RELEASE.store(true, Ordering::Release);
  let began = Instant::now();
  loop {
    let (receipt, ran) = submit_reporter(&rt, shard);
    match receipt.wait(WAIT) {
      Some(Admission::Admitted(_)) => {
        ran.recv_timeout(WAIT).expect("the admitted task ran");
        break;
      }
      Some(Admission::Refused(RtError::TooManyTasks { .. })) if began.elapsed() < WAIT => {
        std::thread::yield_now();
      }
      other => panic!("after the release the submission met {other:?}"),
    }
  }
  rt.shutdown();
}

/// A request the shard drains after its shutdown began is terminated unadmitted: the receipt says
/// so, and the task never runs.
#[test]
fn a_request_drained_during_shutdown_is_terminated_on_its_receipt() {
  let rt = Runtime::start(&config(ARENA)).unwrap();
  let shard = rt.shard_ids()[0];
  hold(&rt, shard);
  // Queued behind the hold, in this order: the shutdown, then the request.
  registry::send_control(shard.0, Control::Shutdown).unwrap();
  let (receipt, ran) = submit_reporter(&rt, shard);
  let counters = rt.shutdown();
  assert_eq!(receipt.wait(WAIT), Some(Admission::Terminated));
  assert!(
    ran.recv_timeout(QUIET).is_err(),
    "a terminated task never runs"
  );
  assert_eq!(
    counters[0].refused_at_shutdown, 1,
    "the refusal is counted on the shard"
  );
}

/// A submission pinned to a slot's holder is refused as gone once that runtime has shut down, and
/// stays refused when a later runtime holds the slot — while a submission to the new holder is
/// admitted. When the slot was in fact reused, a message addressed by id alone reaches the stranger,
/// which is what the pin prevents.
#[test]
fn a_submission_pinned_to_a_holder_is_refused_once_its_slot_is_reused() {
  let first = Runtime::start(&config(ARENA)).unwrap();
  let shard = first.shard_ids()[0];
  let holder = first.holder_of(shard).unwrap();
  first.shutdown();
  assert!(
    matches!(
      submit_to_holder(holder, async {}),
      Err(RtError::ShardGone { .. })
    ),
    "a free slot refuses its old holder's submission"
  );
  let second = Runtime::start(&config(ARENA)).unwrap();
  let taken = second.shard_ids()[0];
  assert!(
    matches!(
      submit_to_holder(holder, async {}),
      Err(RtError::ShardGone { .. })
    ),
    "a slot held by a later registration refuses the old holder's submission"
  );
  let fresh = second.holder_of(taken).unwrap();
  assert_ne!(fresh, holder, "a later registration is a different holder");
  let (tx, rx) = channel();
  let receipt = submit_to_holder(fresh, async move {
    let _ = tx.send(());
  })
  .unwrap();
  assert!(matches!(receipt.wait(WAIT), Some(Admission::Admitted(_))));
  rx.recv_timeout(WAIT).expect("the new holder ran the task");
  if taken == shard {
    let (tx, rx) = channel();
    second
      .spawn_on(shard, async move {
        let _ = tx.send(());
      })
      .unwrap();
    rx.recv_timeout(WAIT)
      .expect("by id alone, the reused slot's new holder ran a task meant for the old one");
  }
  second.shutdown();
}

/// A shutdown whose message first meets a full control channel still lands and completes: the send
/// is retried as the shard drains. Before 2026-09-17 the refusal was dropped and the join never
/// returned.
#[test]
fn a_shutdown_lands_against_a_full_control_channel() {
  let rt = Runtime::start(&config(ARENA)).unwrap();
  let shard = rt.shard_ids()[0];
  hold(&rt, shard);
  let mut queued = 0;
  loop {
    match rt.spawn_on(shard, async {}) {
      Ok(()) => queued += 1,
      Err(RtError::ControlFull { .. }) => break,
      Err(e) => panic!("{e}"),
    }
  }
  assert!(queued >= 1, "the channel filled behind the hold");
  let (done_tx, done_rx) = channel();
  std::thread::spawn(move || {
    rt.shutdown();
    let _ = done_tx.send(());
  });
  done_rx
    .recv_timeout(WAIT)
    .expect("the shutdown completed although its message first met a full control channel");
}
