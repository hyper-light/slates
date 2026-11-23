//! The watchdog's beat allocates nothing once it runs (§4.14 `daemon.alive`, A-228; goal 93's "no allocation by
//! design"). The watchdog is an owned thread that looks at every shard each beat, a tenth of a second, for the daemon's
//! whole life, so whatever one beat allocates the allocator churns all day. Before 2026-11-23 a beat made 4 allocations
//! (82 over 20 beats idle, 81 under 71,073 requests), about 1 KB a beat by their types (estimated, not measured): the
//! lateness p99's sorted copy and the looks, windows and verdicts. A Mercurial push's first heap read had put 24% of its
//! bytes there, about 10-15 MB a second; that was its sampler charging a thread's first symbol load to its next
//! allocation (`tests/hg_push_heap.rs`, fixed in f14ea81bb), not the watchdog.
//!
//! This binary's global allocator counts the allocations made on the watchdog's own thread, found by its name
//! (`slates-watchdog`), so the shards, the client and the test count nothing. A daemon runs idle and then under a
//! client's requests, and each phase's beats after the first are held to no allocation.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::panic_in_result_fn
)]
// The watchdog's thread is found by its name through `pthread_getname_np`.
#![cfg(unix)]

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use slates_server::daemon::HEARTBEAT_NS;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: the name the daemon gives its watchdog's thread (`Watchdog::start`), as `pthread_getname_np` returns it.
const WATCHDOG_THREAD: &[u8] = b"slates-watchdog";
/// Shape: the bytes `pthread_getname_np` writes at most, its terminator included (Linux's `TASK_COMM_LEN`; macOS keeps
/// more, and a longer name is no match).
const NAME_BYTES: usize = 64;
/// Shape: the beats each phase waits before it counts, so the thread's first look and the phase's own start are behind
/// it.
const WARM_BEATS: u64 = 5;
/// Shape: the beats each phase counts over: two seconds of the watchdog's tenth-of-a-second beat.
const COUNTED_BEATS: u64 = 20;

/// Allocations the watchdog's thread made: `alloc`, `alloc_zeroed` and `realloc` calls.
static WATCHDOG_ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

thread_local! {
  /// Whether this thread was found to be the watchdog's. Only a positive finding is kept: std names a spawned thread
  /// after it starts, so a thread asked before its name was set is asked again.
  static IS_WATCHDOG: Cell<bool> = const { Cell::new(false) };
}

/// Whether the calling thread is the watchdog's, by its name; allocation-free, since it runs inside the allocator.
fn on_watchdog_thread() -> bool {
  if IS_WATCHDOG.with(Cell::get) {
    return true;
  }
  let mut name = [0u8; NAME_BYTES];
  // SAFETY: the calling thread's own handle, and a buffer of `NAME_BYTES` bytes on this frame that the call writes at
  // most that many bytes into, a terminator included.
  let rc =
    unsafe { libc::pthread_getname_np(libc::pthread_self(), name.as_mut_ptr().cast(), NAME_BYTES) };
  let named =
    rc == 0 && name.starts_with(WATCHDOG_THREAD) && name.get(WATCHDOG_THREAD.len()) == Some(&0);
  if named {
    IS_WATCHDOG.with(|found| found.set(true));
  }
  named
}

/// The system allocator, counting the watchdog thread's allocations.
struct CountingWatchdog;

impl CountingWatchdog {
  fn count() {
    if on_watchdog_thread() {
      WATCHDOG_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    }
  }
}

// SAFETY: every call is forwarded unchanged to the system allocator; counting touches only an atomic, a thread-local
// flag and a stack buffer, and allocates nothing.
unsafe impl GlobalAlloc for CountingWatchdog {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    Self::count();
    // SAFETY: forwarded unchanged.
    unsafe { System.alloc(layout) }
  }
  unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
    Self::count();
    // SAFETY: forwarded unchanged.
    unsafe { System.alloc_zeroed(layout) }
  }
  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    // SAFETY: forwarded unchanged.
    unsafe { System.dealloc(ptr, layout) }
  }
  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    Self::count();
    // SAFETY: forwarded unchanged.
    unsafe { System.realloc(ptr, layout, new_size) }
  }
}

#[global_allocator]
static GLOBAL: CountingWatchdog = CountingWatchdog;

/// The watchdog's looks so far.
fn looks(daemon: &Daemon) -> u64 {
  daemon
    .watchdog_turns()
    .expect("the daemon runs a watchdog")
    .0
}

/// Waits until the watchdog has looked `beats` more times than `from`, within twice their time and a second.
fn wait_beats(daemon: &Daemon, from: u64, beats: u64) -> u64 {
  let bound = Duration::from_nanos(HEARTBEAT_NS.saturating_mul(beats).saturating_mul(2))
    + Duration::from_secs(1);
  let began = Instant::now();
  loop {
    let now = looks(daemon);
    if now >= from + beats {
      return now;
    }
    assert!(
      began.elapsed() < bound,
      "the watchdog looked {} times in {bound:?}, not {beats}",
      now - from
    );
    std::thread::sleep(Duration::from_nanos(HEARTBEAT_NS / 4));
  }
}

/// The watchdog's allocations over [`COUNTED_BEATS`] beats after [`WARM_BEATS`], and the beats it took.
fn counted_beats(daemon: &Daemon) -> (u64, u64) {
  let warm = wait_beats(daemon, looks(daemon), WARM_BEATS);
  let before = WATCHDOG_ALLOCATIONS.load(Ordering::Relaxed);
  let after_beats = wait_beats(daemon, warm, COUNTED_BEATS);
  let allocations = WATCHDOG_ALLOCATIONS.load(Ordering::Relaxed) - before;
  (allocations, after_beats - warm)
}

/// §4.14 `daemon.alive`, goal 93. Do: start a two-shard daemon; count the allocations its watchdog's thread makes over
/// twenty beats once five have passed, first with the daemon idle, then while a client sends requests. Expect: the
/// thread allocated at its start (so the count sees it), each phase ran its twenty beats, and neither allocated.
#[test]
fn the_watchdogs_beat_allocates_nothing_once_it_runs() {
  let profile = common::machine_profile();
  let instance = format!("wd-alloc-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(2));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: common::fixture_object("slates-seg", "wd-alloc"),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  wait_beats(&daemon, 0, 1);
  // Non-vacuity: the thread's own start allocates (its per-shard state), so a count that never moved is a blind one.
  assert!(
    WATCHDOG_ALLOCATIONS.load(Ordering::Relaxed) > 0,
    "the count never saw the watchdog's thread"
  );

  let (idle, idle_beats) = counted_beats(&daemon);
  println!("watchdog allocations: idle {idle} over {idle_beats} beats");

  let busy = AtomicBool::new(true);
  let (served, (loaded, loaded_beats)) = std::thread::scope(|scope| {
    let requests = scope.spawn(|| {
      let mut client = common::client::connect(&instance);
      let volume = client.create(&common::client::scratch("wd-alloc")).unwrap();
      let mut served = 0_u64;
      while busy.load(Ordering::Relaxed) {
        client.status(volume).unwrap();
        served += 1;
      }
      served
    });
    let counted = counted_beats(&daemon);
    busy.store(false, Ordering::Relaxed);
    (requests.join().unwrap(), counted)
  });
  println!(
    "watchdog allocations: {loaded} over {loaded_beats} beats while {served} requests were served"
  );
  assert!(served > 0, "the loaded phase served requests");
  assert_eq!(
    (idle, loaded),
    (0, 0),
    "the watchdog's beat allocated (idle, under requests)"
  );
  drop(daemon);
}
