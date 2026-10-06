//! A burst of control messages larger than one drain batch (§4.3 "the control channel"): every message
//! is drained without a further send re-arming the shard, and a shutdown queued behind the burst lands.
//! Before 2026-09-17 the drain cleared its pending flag before draining one bounded batch, so a burst
//! past that batch sat undrained until the next successful send — a spawn queued behind a burst was
//! never admitted, and a shutdown refused by the full channel was retried against a shard that had
//! parked for good (`docs/bugs/2026-09-17-control-drain-forgets-a-burst-past-one-batch.md`).

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use slates_rt::runtime::{Runtime, RuntimeConfig};
use slates_rt::{Admission, RtError};

/// Shape: the drain batch of this test's shard — small, so a burst several times its size is a few
/// dozen messages.
const BATCH: usize = 8;
/// Shape: the burst, four drain batches: three of them would be forgotten by a drain that re-arms only
/// on a later send.
const BURST: usize = BATCH * 4;
/// Shape: how long the burst is given to run before the test calls it lost — far past the hold and the
/// steps that drain four batches.
const WAIT: Duration = Duration::from_secs(10);

/// How many of the burst's tasks have run.
static RAN: AtomicU64 = AtomicU64::new(0);

fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 1,
    // The control channel is bounded at the admission limit: room for the whole burst.
    tasks_per_shard: BURST,
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

/// A burst queued behind a held shard is drained whole once the shard runs — batch after batch, with
/// no further send — and the shutdown queued after it lands.
#[test]
fn a_burst_past_one_batch_is_drained_whole_and_the_shutdown_behind_it_lands() {
  let rt = Runtime::start(&config()).unwrap();
  let shard = rt.shard_ids()[0];
  // The hold spins until the test releases it (or drops the release), so the whole burst queues behind it on any
  // machine; a timed spin could end first on a slow one and the burst would drain as sent, never testing a drain
  // past one batch.
  let (release, released) = std::sync::mpsc::channel::<()>();
  let hold = rt
    .spawn_on_with_receipt(shard, async move {
      while matches!(
        released.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
      ) {
        std::hint::spin_loop();
      }
    })
    .unwrap();
  assert!(matches!(hold.wait(WAIT), Some(Admission::Admitted(_))));
  // The burst: every send lands (the channel has room for it all), all behind the hold.
  for _ in 0..BURST {
    match rt.spawn_on(shard, async {
      RAN.fetch_add(1, Ordering::Relaxed);
    }) {
      Ok(()) => {}
      Err(RtError::ControlFull { .. }) => break,
      Err(e) => panic!("{e}"),
    }
  }
  assert_eq!(
    RAN.load(Ordering::Relaxed),
    0,
    "nothing of the burst ran while the hold held the shard"
  );
  release.send(()).unwrap();
  let began = Instant::now();
  while RAN.load(Ordering::Relaxed) < BURST as u64 && began.elapsed() < WAIT {
    std::thread::yield_now();
  }
  let ran = RAN.load(Ordering::Relaxed);
  assert_eq!(
    ran, BURST as u64,
    "every task of the burst ran ({ran} of {BURST}; a drain that re-arms only on a later send runs \
     one batch of {BATCH})"
  );
  let (done_tx, done_rx) = std::sync::mpsc::channel();
  std::thread::spawn(move || {
    rt.shutdown().unwrap();
    let _ = done_tx.send(());
  });
  done_rx
    .recv_timeout(WAIT)
    .expect("the shutdown queued behind the burst landed and completed");
}
