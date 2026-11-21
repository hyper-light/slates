//! A shard's idle work and its driver (§4.2's idle work; §4.3, "a step longer than a peer's wake starves the shard";
//! g228): while a shard has idle work to do, the I/O its sockets become ready for is still taken between the slices.
//! The idle loop went from one slice straight back to a step, and only a park, the idle spin or a busy stretch's
//! quantum harvest the driver, so a socket's readiness waited for the idle work to run out. The daemon's memory sweep,
//! moved onto idle time, ran for seconds: live writes waited 50-120 ms on the 32-core Studio, 8 to 25 a 2,000-write
//! run (BENCHMARKS, g228).

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::channel;
use std::time::Duration;

use slates_rt::registry;
use slates_rt::runtime::{Runtime, RuntimeConfig};
use slates_rt::tcp::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};

/// Shape: how long the test waits for the connection to be accepted; the idle work never runs out, so without the
/// harvest it is never accepted at all.
const SETTLE: Duration = Duration::from_secs(5);
/// Shape: the test makes exactly one connection.
const BACKLOG: i32 = 1;

/// Whether the idle work is still to run: always, until the test ends.
static WORKING: AtomicBool = AtomicBool::new(true);
/// Slices the idle work has done.
static SLICES: AtomicU64 = AtomicU64::new(0);

fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 2,
    tasks_per_shard: 64,
    timers_per_shard: 64,
    ring_entries: 64,
    step_budget_ns: 1_000_000,
    timer_tick_ns: 100_000,
    batch: 64,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
    wake_tracking: None,
  }
}

/// Idle work that has a slice to do every time it is asked, until the test ends.
fn endless() -> bool {
  if !WORKING.load(Ordering::Acquire) {
    return false;
  }
  SLICES.fetch_add(1, Ordering::AcqRel);
  true
}

/// g228. Do: on shard 0, register idle work that never runs out and a task that accepts one TCP connection; from
/// shard 1, connect. Expect: the connection accepted while the idle work is still doing slices (it did slices before
/// and after the accept): the driver is harvested between idle slices, so a socket's readiness never waits for idle
/// work to end.
#[test]
fn a_shard_busy_with_idle_work_still_takes_its_sockets_readiness() {
  let rt = Runtime::start(&config()).unwrap();
  let ids = rt.shard_ids();
  let (server, client) = (ids[0], ids[1]);
  let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), BACKLOG).unwrap();
  let addr = listener.local_addr().unwrap();
  let (registered, idle_on) = channel();
  rt.spawn_on(server, async move {
    let _ = registered.send(
      registry::with_current(|ctx| ctx.set_idle_work(Box::new(endless)))
        .unwrap()
        .is_ok(),
    );
  })
  .unwrap();
  assert!(
    idle_on.recv_timeout(SETTLE).unwrap(),
    "the idle work was registered"
  );
  // The shard is in its idle work now: wait until it has done slices before the accept is even asked for.
  let (_hold, pause) = channel::<()>();
  while SLICES.load(Ordering::Acquire) == 0 {
    let _ = pause.recv_timeout(Duration::from_millis(1));
  }
  let (accepted, on_accept) = channel();
  rt.spawn_on(server, async move {
    let _ = accepted.send((
      listener.accept().await.is_ok(),
      SLICES.load(Ordering::Acquire),
    ));
  })
  .unwrap();
  rt.spawn_on(client, async move {
    if let Ok(stream) = TcpStream::connect(addr).await {
      // Held until the accept is seen, so the connection is never refused for want of its other end.
      let mut buf = [0u8; 1];
      let _ = stream.read(&mut buf).await;
    }
  })
  .unwrap();
  let outcome = on_accept.recv_timeout(SETTLE);
  let slices_after = SLICES.load(Ordering::Acquire);
  WORKING.store(false, Ordering::Release);
  rt.shutdown().unwrap();
  let (ok, slices_at_accept) =
    outcome.expect("the connection was accepted while the shard had idle work");
  assert!(ok, "the accept succeeded");
  assert!(
    slices_at_accept > 0 && slices_after >= slices_at_accept,
    "the idle work was running around the accept: {slices_at_accept} slices at it, {slices_after} after"
  );
}
