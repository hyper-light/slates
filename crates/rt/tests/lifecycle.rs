//! AUD-29-12 (§4.3; banned item 9): a runtime owns its workers from start to a terminal state. A start
//! that fails part-way leaves no worker running and no registry slot claimed; a runtime dropped without
//! `shutdown` stops, cancels and joins its workers; a worker that fails is reported typed, never as
//! default counters; and a shard that failed is never advertised as started.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::future::Future;
use std::pin::Pin;
use std::sync::mpsc::{Sender, channel};
use std::task::{Context, Poll};
use std::time::Duration;

use slates_rt::runtime::{Runtime, RuntimeConfig};

/// The tests of this binary read process-global state (which registry slots are free), so they run one at
/// a time (a test harness lock, D-8's stated exception).
#[allow(clippy::disallowed_types)] // a test harness lock (D-8's stated exception), poison recovered
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
  SERIAL
    .lock()
    .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Shape: how long a shard may take to stop and drop its tasks.
const STOP_WAIT: Duration = Duration::from_secs(10);
/// Shape: shards per runtime: more than one, so a part-way failure has work to undo.
const SHARDS: u16 = 3;

fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: SHARDS,
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

/// A task that never completes and reports when it is dropped (its shard stopped and cancelled it).
struct Parked(Sender<()>);
impl Future for Parked {
  type Output = ();
  fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
    Poll::Pending
  }
}
impl Drop for Parked {
  fn drop(&mut self) {
    let _ = self.0.send(());
  }
}

/// AUD-29-12. Do: start a runtime, park a task on every shard, drop the runtime without `shutdown`.
/// Expect: every shard stops and drops its task (its worker was stopped and joined, not detached), and a
/// new runtime then gets the same registry slots (they were given back).
#[test]
fn a_runtime_dropped_without_shutdown_stops_and_joins_its_workers() {
  let _serial = serial();
  let runtime = Runtime::start(&config()).unwrap();
  let ids = runtime.shard_ids().to_vec();
  let (dropped, told) = channel();
  for id in &ids {
    runtime.spawn_on(*id, Parked(dropped.clone())).unwrap();
  }
  drop(dropped);
  drop(runtime);
  for _ in &ids {
    told
      .recv_timeout(STOP_WAIT)
      .expect("a dropped runtime stopped its shard and dropped the parked task");
  }
  let again = Runtime::start(&config()).unwrap();
  assert_eq!(again.shard_ids(), &ids[..], "the slots were given back");
  again.shutdown().unwrap();
}

/// The slots a runtime of this configuration gets when nothing else holds any: the baseline a failed start
/// must leave intact.
fn baseline_slots() -> Vec<slates_rt::shard::ShardId> {
  let runtime = Runtime::start(&config()).unwrap();
  let ids = runtime.shard_ids().to_vec();
  runtime.shutdown().unwrap();
  ids
}

/// A start over `prepare` must be refused with `expected`, and a runtime started after it must get exactly
/// the baseline's slots: nothing the refused start claimed was kept.
fn refused_without_residue(
  prepare: &mut dyn FnMut() -> Result<slates_rt::driver::Prepared, slates_rt::error::RtError>,
  expected: &slates_rt::error::RtError,
) {
  let baseline = baseline_slots();
  let refused = Runtime::start_with(&config(), prepare);
  assert_eq!(
    refused.as_ref().err(),
    Some(expected),
    "the start was refused with its cause"
  );
  let after = Runtime::start(&config()).unwrap();
  assert_eq!(
    after.shard_ids(),
    &baseline[..],
    "the refused start kept no slot"
  );
  after.shutdown().unwrap();
}

/// Format: the refusal the test's driver double answers with.
fn injected() -> slates_rt::error::RtError {
  slates_rt::error::RtError::DriverRefused {
    call: "the test's injected refusal",
    code: None,
  }
}

/// AUD-29-12. Do: start a runtime whose third shard's driver cannot be prepared. Expect: the start is
/// refused with that refusal and the two shards registered before it are given back.
#[test]
fn a_start_refused_while_preparing_a_driver_gives_every_slot_back() {
  let _serial = serial();
  let entries = u32::try_from(config().ring_entries).unwrap();
  let mut prepared = 0;
  let mut prepare = || {
    prepared += 1;
    if prepared == 3 {
      return Err(injected());
    }
    slates_rt::driver::os_driver(entries)
  };
  refused_without_residue(&mut prepare, &injected());
}

/// AUD-29-12. Do: start a runtime whose last shard's context fails to build on its own thread (its driver
/// seed refuses there) while the first shards build and run. Expect: the start is refused with that
/// refusal — the failed shard is never advertised — and every worker is stopped and joined and every slot
/// given back.
#[test]
fn a_shard_whose_context_fails_to_build_is_never_advertised_and_its_siblings_stop() {
  let _serial = serial();
  let entries = u32::try_from(config().ring_entries).unwrap();
  let mut prepared = 0;
  let mut prepare = || {
    prepared += 1;
    let mut driver = slates_rt::driver::os_driver(entries)?;
    if prepared == usize::from(SHARDS) {
      driver.seed = Box::new(|_kick| Err(injected()));
    }
    Ok(driver)
  };
  refused_without_residue(&mut prepare, &injected());
}

/// A task that reports it was polled, then panics.
struct Panics(Sender<()>);
impl Future for Panics {
  type Output = ();
  fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
    let _ = self.0.send(());
    panic!("the test's injected worker failure");
  }
}

/// AUD-29-12. Do: make one shard's worker panic, then shut the runtime down. Expect: `WorkerFailed`
/// naming that shard — never default counters — and every slot given back all the same.
#[test]
fn a_worker_that_panics_is_reported_typed_and_its_slot_is_given_back() {
  let _serial = serial();
  let baseline = baseline_slots();
  let runtime = Runtime::start(&config()).unwrap();
  let failing = runtime.shard_ids()[1];
  let (polled, told) = channel();
  runtime.spawn_on(failing, Panics(polled)).unwrap();
  // The shutdown must not reach the shard before the task is polled, or it is cancelled unpanicked.
  told
    .recv_timeout(STOP_WAIT)
    .expect("the panicking task was polled");
  assert_eq!(
    runtime.shutdown(),
    Err(slates_rt::error::RtError::WorkerFailed { shard: failing.0 })
  );
  let after = Runtime::start(&config()).unwrap();
  assert_eq!(
    after.shard_ids(),
    &baseline[..],
    "every slot was given back"
  );
  after.shutdown().unwrap();
}
