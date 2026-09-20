//! The runtime registry reclaims a shard's slot when its runtime shuts down (§4.3; banned item 8:
//! every structure has a derived bound *and* reclamation). Before this, every shard leaked its
//! registry entry, its kick descriptor and its context for the process lifetime, so a process that
//! started runtimes repeatedly (the fleet test suite: ~35 tests × 2–5 daemons × 1–5 shards) filled
//! the 1024-slot table and leaked a descriptor per shard — the accumulated suite state that made
//! late tests fail under oversubscription (`docs/wip/fleet-under-load.md`).

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use slates_rt::registry::MAX_SHARDS;
use slates_rt::runtime::{Runtime, RuntimeConfig};

/// The tests of this binary measure process-global state — the registry's slots, the descriptor table —
/// so they run one at a time (a test harness lock, D-8's stated exception): under parallel threads another
/// test's runtime takes the freed slot the stale-waker test expects to see reused, or moves the
/// descriptor count the leak test compares (3 of 10 parallel runs failed that way on 2026-09-14).
#[allow(clippy::disallowed_types)] // a test harness lock (D-8's stated exception), poison recovered
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
  SERIAL
    .lock()
    .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn config(shards: u16) -> RuntimeConfig {
  RuntimeConfig {
    shards,
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
  }
}

/// The process's open descriptor count (Unix), through the descriptor table's own listing — a
/// read of `/dev/fd` (macOS, Linux via the symlink), never a write.
#[cfg(unix)]
fn open_descriptors() -> usize {
  std::fs::read_dir("/dev/fd").map_or(0, |dir| dir.count())
}

/// A local or CI run can require the backend it intends to cover. Docker's default policy
/// selects epoll, which cannot establish that io_uring releases its pending file references.
#[cfg(target_os = "linux")]
fn verify_linux_driver(notes: &[String]) {
  let selection = notes
    .iter()
    .find(|note| note.starts_with("io_uring = "))
    .expect("the runtime reports its Linux driver probe");
  let driver = if selection.ends_with("using epoll") {
    "epoll"
  } else {
    "io_uring"
  };
  eprintln!("listener retirement driver: {driver}; {notes:?}");
  if let Some(expected) = std::env::var_os("SLATES_TEST_DRIVER") {
    assert_eq!(
      expected, driver,
      "the requested Linux backend must be exercised"
    );
  }
}

#[cfg(target_os = "linux")]
fn bind_abstract_listener(address: &rustix::net::SocketAddrUnix) -> std::os::fd::OwnedFd {
  use rustix::net::{AddressFamily, SocketFlags, SocketType};

  let socket = rustix::net::socket_with(
    AddressFamily::UNIX,
    SocketType::STREAM,
    SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
    None,
  )
  .unwrap();
  rustix::net::bind(&socket, address).unwrap();
  // The fixture admits no connections; a one-connection backlog suffices for listening.
  rustix::net::listen(&socket, 1).unwrap();
  socket
}

/// Do: start and shut down one more runtime than the registry has slots, one shard each. Expect:
/// every start succeeds — a shut-down runtime's slot is reclaimed, so the bound is on *live*
/// shards, not on shards ever created. Before the fix the 1025th start refused `TooManyShards`.
#[test]
fn a_shut_down_runtimes_slot_is_reclaimed_so_more_runtimes_than_slots_may_run_in_turn() {
  let _serial = serial();
  for round in 0..=MAX_SHARDS {
    let runtime = Runtime::start(&config(1)).unwrap_or_else(|e| {
      panic!("runtime {round} refused after {round} shut-down runtimes: {e:?}")
    });
    assert_eq!(runtime.shard_ids().len(), 1);
    let _ = runtime.shutdown();
  }
}

/// Do: measure the open descriptors, run 64 start/shutdown cycles of a two-shard runtime, measure
/// again. Expect: the count returns to its baseline (the kick and driver descriptors are closed
/// with the shard). Before the fix each shard leaked its kqueue/eventfd: +2 per cycle.
#[cfg(unix)]
#[test]
fn a_shut_down_runtime_closes_every_descriptor_it_opened() {
  let _serial = serial();
  // One warm-up cycle so lazily-opened process-wide descriptors (the thread-local storage of the
  // first shard thread, the allocator's) are in the baseline.
  let _ = Runtime::start(&config(2)).unwrap().shutdown();
  let baseline = open_descriptors();
  for _ in 0..64 {
    let _ = Runtime::start(&config(2)).unwrap().shutdown();
  }
  let after = open_descriptors();
  assert!(
    after <= baseline,
    "descriptors leaked across 64 two-shard cycles: {baseline} before, {after} after"
  );
}

/// AC-0.6 / T-2.14, §4.3: shut down with a listener awaiting readiness, then bind its
/// abstract address immediately. Joining the shard must release the kernel's references too;
/// counting open descriptors alone cannot detect a pending poll retaining the old listener.
#[cfg(target_os = "linux")]
#[test]
fn shutdown_releases_a_listener_with_an_armed_readiness_wait() {
  use std::future::{Future, poll_fn};
  use std::os::fd::AsRawFd;
  use std::task::Poll;

  use rustix::net::SocketAddrUnix;

  let _serial = serial();
  let name = format!("slates-shutdown-readiness-{}", std::process::id());
  let address = SocketAddrUnix::new_abstract_name(name.as_bytes()).unwrap();
  let listener = bind_abstract_listener(&address);
  let runtime = Runtime::start(&config(1)).unwrap();
  verify_linux_driver(runtime.notes());
  let (armed, received) = std::sync::mpsc::sync_channel(1);
  runtime
    .spawn_on(runtime.shard_ids()[0], async move {
      let mut readiness = std::pin::pin!(slates_rt::readiness::readable(listener.as_raw_fd()));
      let mut armed = Some(armed);
      poll_fn(|context| {
        let result = readiness.as_mut().poll(context);
        if result.is_pending()
          && let Some(armed) = armed.take()
        {
          armed.send(()).unwrap();
        }
        assert!(matches!(result, Poll::Pending), "no client connects");
        result
      })
      .await
      .unwrap();
    })
    .unwrap();
  let observed = received.recv_timeout(std::time::Duration::from_secs(5));
  let counters = runtime.shutdown();
  observed.expect("the listener's readiness wait was armed before shutdown");
  assert_eq!(counters[0].cancelled, 1, "shutdown cancelled the listener");
  drop(bind_abstract_listener(&address));
}

/// AC-0.6 / T-2.14, §4.3: retire a local runtime with more pending listener polls
/// than fit in one completion batch. No shard-thread exit may hide asynchronous
/// cleanup: every address must rebind on this thread without a delay or retry.
#[cfg(target_os = "linux")]
#[test]
fn dropping_a_local_runtime_releases_a_polled_listener_before_returning() {
  use rustix::net::SocketAddrUnix;
  use std::os::fd::AsRawFd;

  let _serial = serial();
  let config = config(1);
  // io_uring's default CQ holds twice its SQ entries. One more listener, plus the kick
  // and drain, forces retirement to consume completions across the CQ overflow boundary.
  let addresses: Vec<_> = (0..config.ring_entries * 2 + 1)
    .map(|listener| {
      let name = format!("slates-local-readiness-{}-{listener}", std::process::id());
      SocketAddrUnix::new_abstract_name(name.as_bytes()).unwrap()
    })
    .collect();
  let listeners: Vec<_> = addresses.iter().map(bind_abstract_listener).collect();
  let runtime = slates_rt::runtime::LocalRuntime::new(&config).unwrap();
  verify_linux_driver(runtime.notes());
  for listener in &listeners {
    runtime
      .context()
      .register_readable(listener.as_raw_fd(), 0xABCD)
      .unwrap();
  }
  drop(listeners);
  drop(runtime);
  for address in addresses {
    drop(bind_abstract_listener(&address));
  }
}

/// Do: spawn a task on a runtime and keep its wake word; shut the runtime down; start a new runtime
/// that reuses the same registry slot; fire the stale wake. Expect: the new runtime's shard is not
/// disturbed — the stale word names a task generation the new arena has not issued (its generations
/// continue from the old shard's high-water mark), so the wake is refused by the arena — and the
/// new runtime still runs its own task to completion. Non-vacuous: the new runtime provably reused
/// the same shard id, and the stale wake was delivered to it (the registry's stale-wake counter did
/// not move: the slot was live), so the arena's generation check is what refused it.
#[test]
fn a_wake_minted_for_a_dead_shard_is_refused_by_the_slots_new_holder() {
  let _serial = serial();
  use std::sync::atomic::{AtomicU64, Ordering};
  use std::task::{Context, Poll};
  static POLLS: AtomicU64 = AtomicU64::new(0);

  // A future that records every poll and captures its waker on the first.
  struct Capture(std::sync::mpsc::Sender<std::task::Waker>);
  impl std::future::Future for Capture {
    type Output = ();
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
      let _ = self.0.send(cx.waker().clone());
      Poll::Ready(())
    }
  }

  let (tx, rx) = std::sync::mpsc::channel();
  let first = Runtime::start(&config(1)).unwrap();
  let id = first.shard_ids()[0];
  first.spawn_on(id, Capture(tx)).unwrap();
  let waker = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
  let _ = first.shutdown();

  let second = Runtime::start(&config(1)).unwrap();
  assert_eq!(
    second.shard_ids()[0],
    id,
    "the second runtime reused the freed slot"
  );
  // A live task on the new holder, polled once by its own spawn.
  struct Counted(std::sync::mpsc::Sender<()>);
  impl std::future::Future for Counted {
    type Output = ();
    fn poll(self: std::pin::Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
      POLLS.fetch_add(1, Ordering::Relaxed);
      let _ = self.0.send(());
      Poll::Ready(())
    }
  }
  let (done_tx, done_rx) = std::sync::mpsc::channel();
  second.spawn_on(id, Counted(done_tx)).unwrap();
  done_rx
    .recv_timeout(std::time::Duration::from_secs(5))
    .expect("the new holder polled its own task");
  let before = slates_rt::registry::stale_wakes(id.0);
  waker.wake_by_ref();
  let counters = second.shutdown();
  let after = slates_rt::registry::stale_wakes(id.0);
  assert_eq!(
    after, before,
    "the stale wake reached a live slot (it was the arena that refused it)"
  );
  assert_eq!(
    counters[0].completed, 1,
    "the new holder ran exactly its own task; the stale wake polled nothing extra"
  );
}
