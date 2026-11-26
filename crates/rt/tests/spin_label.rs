//! A spinning task's label in its shard's pulse (A-740): a watchdog that finds a shard spinning reads, from its own
//! thread, which task the shard is polling and at which poll, so its report names the work rather than the shard alone.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use slates_rt::futures;
use slates_rt::label::TaskLabel;
use slates_rt::runtime::{Runtime, RuntimeConfig};

/// Shape: how long the test waits for anything a shard does; a bound on a hang, never met by a passing run.
const WAIT: Duration = Duration::from_secs(30);

fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 1,
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

/// What the shard's pulse says it is polling, read from this thread as the watchdog reads it.
fn polling(shard: u16) -> slates_rt::label::Polling {
  slates_rt::registry::with_entry(shard, |entry| entry.pulse.polling()).unwrap()
}

/// A-740. Do: spawn a task from another thread, let it yield twice, then spin without yielding: first under its spawn
/// site's label, then inside `futures::doing("spin_label::spinner")`. Expect: read from this thread while it spins, the
/// shard's pulse names the spawn site (this file) at the task's third poll, then the named work at the same poll; once
/// the guard drops the site's label is back. The file is compared with `file!()` whole: a Windows build names it
/// `crates\rt\tests\spin_label.rs`, which a `/`-separated suffix never matches.
#[test]
fn a_spinning_tasks_label_and_poll_count_are_read_from_another_thread() {
  let runtime = Runtime::start(&config()).unwrap();
  let shard = *runtime.shard_ids().first().unwrap();
  let phase: &'static AtomicBoolPair = Box::leak(Box::new(AtomicBoolPair::default()));
  runtime
    .spawn_on(shard, async move {
      futures::yield_now().await;
      futures::yield_now().await;
      phase.spinning.store(true, Ordering::Release);
      while !phase.go_on.swap(false, Ordering::AcqRel) {
        std::hint::spin_loop();
      }
      {
        let _doing = futures::doing("spin_label::spinner");
        phase.spinning.store(true, Ordering::Release);
        while !phase.go_on.swap(false, Ordering::AcqRel) {
          std::hint::spin_loop();
        }
      }
      phase.spinning.store(true, Ordering::Release);
      while !phase.go_on.swap(false, Ordering::AcqRel) {
        std::hint::spin_loop();
      }
    })
    .unwrap();
  let mut seen = Vec::new();
  for _ in 0..3 {
    let started = Instant::now();
    while !phase.spinning.swap(false, Ordering::AcqRel) {
      assert!(started.elapsed() < WAIT, "the task never spun");
      std::hint::spin_loop();
    }
    seen.push(polling(shard.0));
    phase.go_on.store(true, Ordering::Release);
  }
  let words: Vec<String> = seen.iter().map(ToString::to_string).collect();
  assert!(
    matches!(seen[0].label, Some(TaskLabel::Site(site)) if site.file() == file!()),
    "{words:?}"
  );
  assert_eq!(seen[0].polls, 3, "{words:?}");
  assert_eq!(words[1], "task spin_label::spinner, at its poll 3");
  assert_eq!(
    words[2], words[0],
    "the site's label is back once the guard drops"
  );
  runtime.shutdown().unwrap();
}

/// The two flags the spinning task and the test hand each other.
#[derive(Default)]
struct AtomicBoolPair {
  /// The task is spinning in its next phase.
  spinning: AtomicBool,
  /// The test has read it: go on to the next phase.
  go_on: AtomicBool,
}

/// Main 01a20182's order of 2026-11-23: every task submitted from another thread read `runtime.rs:38`, the submit's own
/// line, so a wedged shard named no culprit. Do: from this thread submit a task (`submit_to_holder`) that spins until
/// told, and read the shard's pulse while it spins. Expect: its label this file, `file!()` whole, at the submitting
/// line.
#[test]
fn a_task_submitted_from_another_thread_is_labelled_with_its_submitters_line() {
  let runtime = Runtime::start(&config()).unwrap();
  let shard = *runtime.shard_ids().first().unwrap();
  let holder = runtime.holder_of(shard).unwrap();
  let phase: &'static AtomicBoolPair = Box::leak(Box::new(AtomicBoolPair::default()));
  let line = line!() + 1;
  let receipt = slates_rt::runtime::submit_to_holder(holder, async move {
    phase.spinning.store(true, Ordering::Release);
    while !phase.go_on.swap(false, Ordering::AcqRel) {
      std::hint::spin_loop();
    }
  })
  .unwrap();
  let started = Instant::now();
  while !phase.spinning.swap(false, Ordering::AcqRel) {
    assert!(started.elapsed() < WAIT, "the task never ran");
    std::hint::spin_loop();
  }
  let seen = polling(shard.0);
  phase.go_on.store(true, Ordering::Release);
  let _ = receipt.wait(WAIT);
  runtime.shutdown().unwrap();
  assert!(
    matches!(seen.label, Some(TaskLabel::Site(site))
      if site.file() == file!() && site.line() == line),
    "{seen}"
  );
}

/// Shape: how long the long task's one poll holds its shard: past any short poll's length.
const LONG_POLL: Duration = Duration::from_millis(20);

/// A-920 6b (main 01a20182's ruling of 2026-11-26: a wide import's longest poll names its site). Do: on one shard, spawn
/// a task that yields, one whose one poll spins [`LONG_POLL`], and another that yields after it. Expect: the shard's
/// pulse names the long task's spawn site, this file at its spawning line, as its longest poll's task, with the
/// shorter polls before and after it leaving that name as it is.
#[test]
fn a_shards_longest_poll_names_its_task() {
  let runtime = Runtime::start(&config()).unwrap();
  let shard = *runtime.shard_ids().first().unwrap();
  let done: &'static AtomicBoolPair = Box::leak(Box::new(AtomicBoolPair::default()));
  runtime
    .spawn_on(shard, async move {
      futures::yield_now().await;
    })
    .unwrap();
  let line = line!() + 2;
  runtime
    .spawn_on(shard, async move {
      let began = Instant::now();
      while began.elapsed() < LONG_POLL {
        std::hint::spin_loop();
      }
      done.spinning.store(true, Ordering::Release);
    })
    .unwrap();
  let started = Instant::now();
  while !done.spinning.load(Ordering::Acquire) {
    assert!(started.elapsed() < WAIT, "the long task never ran");
    std::thread::yield_now();
  }
  runtime
    .spawn_on(shard, async move {
      futures::yield_now().await;
      done.go_on.store(true, Ordering::Release);
    })
    .unwrap();
  while !done.go_on.load(Ordering::Acquire) {
    assert!(started.elapsed() < WAIT, "the last task never ran");
    std::thread::yield_now();
  }
  let longest =
    slates_rt::registry::with_entry(shard.0, |entry| entry.pulse.longest_polling()).unwrap();
  assert!(
    matches!(longest.label, Some(TaskLabel::Site(site)) if site.file() == file!() && site.line() == line),
    "{longest}"
  );
  runtime.shutdown().unwrap();
}
