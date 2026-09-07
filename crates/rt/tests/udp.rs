//! A UDP datagram received asynchronously through the runtime's driver (§4.10a): a task awaits
//! `recv_from`, which registers read-readiness with the shard's driver and yields; a second task
//! sends after a short runtime sleep, the socket becomes readable, the driver wakes the receiver, and
//! the datagram arrives. This is the fleet transport's substrate proven end to end on the
//! readiness-native driver (kqueue here on macOS; epoll on Linux) — no foreign runtime.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc::channel;
use std::time::Duration;

// The address types come through `rustix::net` (the standard types re-exported) to honour the
// host-path wall's `std::net` guard.
use rustix::net::{Ipv4Addr, SocketAddrV4};
use slates_rt::runtime::{Runtime, RuntimeConfig};
use slates_rt::udp::UdpSocket;

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
