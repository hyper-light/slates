//! A shard that joins another thread of the process is read as that thread is when that thread is runnable (A-228's
//! transitive wait, §4.14): `slates_rt::registry::spawn_waited` and `WaitedThread::join` mark the join on the shard's
//! registry entry, and `shard_run_readings` reads a shard blocked in the join as runnable while the joined thread runs
//! or waits for a CPU, and as blocked while the joined thread is blocked too. The daemon's watchdog and its observations
//! judge a shard by that reading, so a shard joining a starved thread is starved, not stuck, and one joining a blocked
//! thread is still a deadlock.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use slates_rt::registry;
use slates_rt::runtime::{Runtime, RuntimeConfig};
use slates_rt::{RunState, SlotHolder};

/// Shape: how long the test waits for the shard to reach each phase before failing: far past any step.
const PHASE_WAIT: Duration = Duration::from_secs(30);
/// Shape: how often the test looks.
const LOOK: Duration = Duration::from_millis(5);

fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 1,
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
    wake_tracking: None,
  }
}

/// Waits `span` on a channel nothing sends on (the lint wall bans `std::thread::sleep`).
fn wait(span: Duration) {
  let (_hold, rx) = std::sync::mpsc::channel::<()>();
  let _ = rx.recv_timeout(span);
}

/// Format: the joined thread's phases: spinning, then blocked, then released to end.
const SPIN: u8 = 0;
/// Format: see [`SPIN`].
const BLOCK: u8 = 1;
/// Format: see [`SPIN`].
const END: u8 = 2;
/// The joined thread's phase.
static PHASE: AtomicU8 = AtomicU8::new(SPIN);
/// Set by the shard's task once it has started the join.
static JOINING: AtomicBool = AtomicBool::new(false);
/// Set once the join returned.
static JOINED: AtomicBool = AtomicBool::new(false);

/// The shard's reading, asked as the watchdog asks.
fn reading(holder: SlotHolder) -> Option<RunState> {
  registry::shard_run_readings(&[holder])
    .pop()
    .flatten()
    .map(|reading| reading.state)
}

/// Whether the shard's entry is marked as waiting on a thread.
fn marked(holder: SlotHolder) -> bool {
  registry::with_entry(holder.shard(), |entry| {
    entry.waits_on.load(Ordering::Acquire) != 0
  })
  .unwrap_or(false)
}

/// Waits until `done` holds, up to [`PHASE_WAIT`]: whether it did. A failed wait is returned rather than raised, so the
/// test ends the joined thread first: the shard cannot stop while its join waits.
fn until(what: &str, done: impl Fn() -> bool) -> Result<(), String> {
  let started = Instant::now();
  while !done() {
    if started.elapsed() >= PHASE_WAIT {
      return Err(format!("{what} within {PHASE_WAIT:?}"));
    }
    wait(LOOK);
  }
  Ok(())
}

/// The phases the test watches the shard through, while the joined thread spins and then blocks.
fn observe_the_join(holder: SlotHolder, readable: bool) -> Result<(), String> {
  until("the shard joins", || {
    JOINING.load(Ordering::Acquire) && marked(holder)
  })?;
  if readable {
    until(
      "the shard, joining a running thread, reads runnable",
      || reading(holder) == Some(RunState::Runnable),
    )?;
  }
  PHASE.store(BLOCK, Ordering::Release);
  if readable {
    until("the shard, joining a blocked thread, reads blocked", || {
      reading(holder) == Some(RunState::Blocked)
    })?;
  }
  if marked(holder) {
    Ok(())
  } else {
    Err("still marked while the join waits".to_owned())
  }
}

/// A-228's transitive wait. Do: on a shard, join a thread started by `spawn_waited` that spins, then blocks on a channel,
/// then ends. Expect: the shard's entry marked while it joins; the shard read runnable while the joined thread spins
/// (the shard itself is blocked in the join), and blocked once the joined thread blocks; the mark gone after the join.
/// Linux, macOS and Windows read thread states; elsewhere the reader answers nothing and the test says so.
#[test]
fn a_shard_joining_a_thread_reads_as_the_thread_runs() {
  let rt = Runtime::start(&config()).unwrap();
  let shard = rt.shard_ids()[0];
  let holder = rt.holder_of(shard).unwrap();
  let (release, released) = std::sync::mpsc::sync_channel::<()>(1);
  rt.spawn_on(shard, async move {
    let joined = registry::spawn_waited(
      std::thread::Builder::new().name("waited".to_owned()),
      move || {
        while PHASE.load(Ordering::Acquire) == SPIN {
          std::hint::spin_loop();
        }
        let _ = released.recv();
      },
    )
    .unwrap();
    JOINING.store(true, Ordering::Release);
    joined.join().unwrap();
    JOINED.store(true, Ordering::Release);
  })
  .unwrap();
  let readable = cfg!(any(target_os = "linux", target_os = "macos", windows));
  let observed = observe_the_join(holder, readable);
  let last = reading(holder);
  let was_marked = marked(holder);
  PHASE.store(END, Ordering::Release);
  let _ = release.send(());
  let ended = until("the join returns", || JOINED.load(Ordering::Acquire))
    .and_then(|()| until("the mark is cleared", || !marked(holder)));
  let _ = rt.shutdown();
  if !readable {
    eprintln!("SKIP the run-state half: this platform reads no thread state");
  }
  assert!(
    observed.is_ok(),
    "{observed:?} (the shard's last reading {last:?}, marked {was_marked})"
  );
  assert!(ended.is_ok(), "{ended:?}");
}
