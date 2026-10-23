//! A readiness wait names its own descriptor (§4.3 "Drivers"; `crates/rt/src/readiness.rs`): a task that awaits
//! several things at once — two descriptors, or a descriptor and a deadline — learns which of them happened. The
//! FUSE serve loop races its device against its change signal; every timed wait races its work against a deadline
//! (`futures::within`). Until 2026-10-22 a readiness future, once registered, reported ready on *any* wake of its
//! task, so a wake from the other source was taken for its own: a daemon serving two FUSE mounts of one volume spun
//! a core after one write (the change signal's wake read as the device's, the signal never drained), and a deadline
//! on a readiness wait could never be reported
//! (`docs/bugs/2026-10-22-a-readiness-wait-took-any-wake-of-its-task-for-its-own.md`).
//!
//! Real sockets on the platform's own driver (kqueue on macOS, epoll on Linux, IOCP's AFD reactor on Windows), so
//! every driver's completions are attributed the same way.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::indexing_slicing,
  clippy::unwrap_in_result
)]

use std::future::Future;
use std::sync::mpsc::channel;
use std::task::Poll;
use std::time::Duration;

use slates_rt::runtime::{Runtime, RuntimeConfig};
use slates_rt::udp::{Ipv4Addr, SocketAddrV4, UdpSocket};

/// Shape: how long the test waits for a task's answer before it fails rather than hanging the binary. The work
/// is one loopback datagram or one short deadline: milliseconds.
const ANSWER_WAIT: Duration = Duration::from_secs(10);
/// Shape: the deadline the second test races a readiness wait against. Long enough for the task to register its
/// interest and the shard to park, short enough that the test is quick; nothing is ever sent, so any length
/// shows the same thing.
const DEADLINE_NS: u64 = 20_000_000;

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

fn loopback() -> UdpSocket {
  UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap()
}

/// Which of the two raced waits finished.
#[derive(Debug, PartialEq, Eq)]
enum Finished {
  /// The socket nothing is sent to (polled first, as the FUSE serve loop polls its device first).
  Quiet,
  /// The socket a datagram is sent to.
  Signalled,
}

/// §4.3; the bug record above. Do: in one task, race the readiness of two sockets — the quiet one polled first —
/// and once both are registered send a datagram to the other. Expect: the race names the socket that was sent to,
/// and the quiet one has nothing to read. Before the fix the quiet one answered first, on the other's wake.
#[test]
#[cfg_attr(miri, ignore)] // real sockets on the OS driver (a kqueue, an epoll, IOCP's AFD), which Miri does not model
fn a_task_racing_two_descriptors_learns_which_one_became_readable() {
  let rt = Runtime::start(&config()).unwrap();
  let id = rt.shard_ids()[0];
  let quiet = loopback();
  let signalled = loopback();
  let target = signalled.local_addr().unwrap();
  let (registered_tx, registered_rx) = channel();
  let (answer_tx, answer_rx) = channel();
  rt.spawn_on(id, async move {
    let mut quiet_ready = std::pin::pin!(quiet.readable());
    let mut signalled_ready = std::pin::pin!(signalled.readable());
    let mut told = false;
    let finished = std::future::poll_fn(|cx| {
      if let Poll::Ready(ready) = quiet_ready.as_mut().poll(cx) {
        return Poll::Ready(ready.map(|()| Finished::Quiet));
      }
      if let Poll::Ready(ready) = signalled_ready.as_mut().poll(cx) {
        return Poll::Ready(ready.map(|()| Finished::Signalled));
      }
      if !told {
        told = true;
        let _ = registered_tx.send(());
      }
      Poll::Pending
    })
    .await;
    let mut buf = [0u8; 16];
    let quiet_had = quiet.try_recv_from(&mut buf).map(|got| got.is_some());
    let _ = answer_tx.send((finished, quiet_had));
  })
  .unwrap();
  registered_rx
    .recv_timeout(ANSWER_WAIT)
    .expect("the task registered both waits");
  loopback().send_to(b"ping", target).unwrap();
  let (finished, quiet_had) = answer_rx
    .recv_timeout(ANSWER_WAIT)
    .expect("the race finished");
  assert_eq!(
    finished.unwrap(),
    Finished::Signalled,
    "the wait that finished is the descriptor that became readable"
  );
  assert!(!quiet_had.unwrap(), "the quiet socket had nothing to read");
  rt.shutdown().unwrap();
}

/// §4.3 on the simulation driver, whose fabric wakes a registration through the registry rather than as a
/// driver completion: the same attribution must reach the task by that path too, deterministically (and under
/// Miri). Do: in one task, race two fabric ports' readiness, the quiet one polled first; once both are registered a
/// second task sends to the other. Expect: the race names the port that was sent to, and the quiet port has
/// nothing to read.
#[test]
fn a_simulated_task_racing_two_ports_learns_which_one_became_readable() {
  use slates_rt::sim::SimRuntime;

  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];
  let (port_tx, port_rx) = channel();
  let (answer_tx, answer_rx) = channel();
  sim
    .spawn_on(id, async move {
      let quiet = loopback();
      let signalled = loopback();
      let mut quiet_ready = std::pin::pin!(quiet.readable());
      let mut signalled_ready = std::pin::pin!(signalled.readable());
      let mut told = false;
      let finished = std::future::poll_fn(|cx| {
        if let Poll::Ready(ready) = quiet_ready.as_mut().poll(cx) {
          return Poll::Ready(ready.map(|()| Finished::Quiet));
        }
        if let Poll::Ready(ready) = signalled_ready.as_mut().poll(cx) {
          return Poll::Ready(ready.map(|()| Finished::Signalled));
        }
        if !told {
          told = true;
          let _ = port_tx.send(signalled.local_addr().unwrap().port());
        }
        Poll::Pending
      })
      .await;
      let mut buf = [0u8; 16];
      let quiet_had = quiet.try_recv_from(&mut buf).map(|got| got.is_some());
      let _ = answer_tx.send((finished, quiet_had));
    })
    .unwrap();
  sim
    .spawn_on(id, async move {
      // The racing task runs first (spawn order) and names its port once both waits are registered.
      let port = loop {
        if let Ok(port) = port_rx.try_recv() {
          break port;
        }
        slates_rt::futures::sleep(1_000).await.unwrap();
      };
      let _ = loopback().send_to(b"ping", SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
    })
    .unwrap();
  sim.run_until_idle();
  let (finished, quiet_had) = answer_rx.try_recv().expect("the race finished");
  assert_eq!(
    finished.unwrap(),
    Finished::Signalled,
    "the wait that finished is the port a datagram was sent to"
  );
  assert!(!quiet_had.unwrap(), "the quiet port had nothing to read");
}

/// §4.3 (`futures::within`: `Ok(None)` once the deadline has passed); the bug record above. Do: race a readiness
/// wait on a socket nothing is ever sent to against a short deadline. Expect: the deadline, `Ok(None)`. Before the
/// fix the deadline's own wake was taken for readiness, so the race answered `Ok(Some(..))` and no timed readiness
/// wait could ever time out.
#[test]
#[cfg_attr(miri, ignore)] // real sockets on the OS driver (a kqueue, an epoll, IOCP's AFD), which Miri does not model
fn a_deadline_on_a_readiness_wait_that_never_comes_is_reported_as_the_deadline() {
  let rt = Runtime::start(&config()).unwrap();
  let id = rt.shard_ids()[0];
  let quiet = loopback();
  let (answer_tx, answer_rx) = channel();
  rt.spawn_on(id, async move {
    let outcome = slates_rt::futures::within(DEADLINE_NS, quiet.readable()).await;
    let _ = answer_tx.send(outcome.map(|finished| finished.is_some()));
  })
  .unwrap();
  let readiness_reported = answer_rx
    .recv_timeout(ANSWER_WAIT)
    .expect("the race finished")
    .unwrap();
  assert!(
    !readiness_reported,
    "the deadline passed with nothing to read: the race reports the deadline, not readiness"
  );
  rt.shutdown().unwrap();
}
