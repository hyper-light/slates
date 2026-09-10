//! A UDP datagram received asynchronously through the runtime's driver (§4.10a): a task awaits
//! `recv_from`, which registers read-readiness with the shard's driver and yields; a second task
//! sends after a short runtime sleep, the socket becomes readable, the driver wakes the receiver, and
//! the datagram arrives. This is the fleet transport's substrate proven end to end on the
//! readiness-native driver (kqueue here on macOS; epoll on Linux) — no foreign runtime.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc::channel;
use std::time::Duration;

// The address types come from the runtime's own re-export (`core::net`'s, the same ones `rustix::net`
// re-exports) so the test builds on every platform — including Windows, where a real datagram round
// trip here exercises the IOCP driver's AFD readiness reactor (`crate::afd`).
use slates_rt::runtime::{Runtime, RuntimeConfig};
use slates_rt::udp::{Ipv4Addr, SocketAddrV4, UdpSocket};

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
  }
}

/// A task's `recv_from` awaits through the driver until a datagram arrives; a second task sends it.
#[test]
fn a_udp_datagram_is_received_through_the_driver() {
  let rt = Runtime::start(&config()).unwrap();
  let id = rt.shard_ids()[0];

  // The receiver is bound before the tasks so the sender knows where to send.
  let receiver = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
  let target = receiver.local_addr().unwrap();
  assert_ne!(target.port(), 0, "the OS assigned a port");

  let (tx, rx) = channel();
  rt.spawn_on(id, async move {
    let mut buf = [0u8; 64];
    let outcome = receiver
      .recv_from(&mut buf)
      .await
      .map(|(n, from)| (buf[..n].to_vec(), from));
    let _ = tx.send(outcome);
  })
  .unwrap();

  // The sender waits a runtime tick (letting the receiver register read-readiness on the driver),
  // then sends from outside the runtime's socket set: the receiver's socket becomes readable, the
  // driver wakes it, and recv_from returns.
  rt.spawn_on(id, async move {
    slates_rt::futures::sleep(5_000_000).await;
    let sender = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
    let _ = sender.send_to(b"ping", target);
  })
  .unwrap();

  match rx.recv_timeout(Duration::from_secs(5)) {
    Ok(Ok((bytes, from))) => {
      assert_eq!(bytes, b"ping", "the datagram's bytes arrived");
      assert_eq!(from.ip(), &Ipv4Addr::LOCALHOST, "from a loopback sender");
    }
    Ok(Err(e)) => panic!("recv_from failed: {e:?}"),
    Err(e) => {
      let counters = rt.shutdown();
      panic!("timed out ({e}); counters {counters:#?}");
    }
  }
  rt.shutdown();
}

/// The simulated UDP fabric delivers deterministically at N=1 (§4.10a "sim arm first"): a receiver
/// awaits recv_from (registering fabric interest), a sender delivers, and the datagram arrives — the
/// whole plane with no OS network, driven to idle. Uses `slates_rt::sim::SimRuntime`.
#[test]
fn a_simulated_udp_datagram_is_received() {
  use slates_rt::sim::SimRuntime;

  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];
  let (port_tx, port_rx) = channel();
  let (result_tx, result_rx) = channel();

  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      // The receiver's bound port, told to the sender before awaiting.
      let _ = port_tx.send(socket.local_addr().unwrap().port());
      let mut buf = [0u8; 64];
      let outcome = socket
        .recv_from(&mut buf)
        .await
        .map(|(n, from)| (buf[..n].to_vec(), from));
      let _ = result_tx.send(outcome);
    })
    .unwrap();

  sim
    .spawn_on(id, async move {
      // The receiver runs first (spawn order) and sends its port before awaiting, so it is ready.
      let port = loop {
        if let Ok(p) = port_rx.try_recv() {
          break p;
        }
        slates_rt::futures::sleep(1_000).await;
      };
      let sender = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = sender.send_to(b"simping", SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
    })
    .unwrap();

  sim.run_until_idle();

  match result_rx.try_recv() {
    Ok(Ok((bytes, from))) => {
      assert_eq!(bytes, b"simping", "the simulated datagram arrived");
      assert_ne!(from.port(), 0, "with the sender's fabric port");
    }
    other => panic!("the simulated recv did not complete: {other:?}"),
  }
}
