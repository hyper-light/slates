//! Async TCP through the runtime's driver (§4.6, R5): a server task accepts a connection, reads a
//! request and writes a framed reply, all awaiting the shard's driver; a client task connects, sends
//! the request and reads the reply back — the accept/read/write path the NFS loopback server runs on,
//! proven end to end on the readiness-native driver (kqueue here on macOS; epoll on Linux), with no
//! foreign runtime and both ends on the runtime's own sockets.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc::channel;
use std::time::Duration;

// The address types come through `rustix::net` (the standard types re-exported) to honour the
// host-path wall's `std::net` guard.
use rustix::net::{Ipv4Addr, SocketAddrV4};
use slates_rt::runtime::{Runtime, RuntimeConfig};
use slates_rt::tcp::{TcpListener, TcpStream};

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

/// Shape: the test makes exactly one connection, so one queued pending connection suffices.
const BACKLOG: i32 = 1;

/// The bytes the client sends and the marker the server frames its echo with, so the reply proves the
/// whole accept -> read -> write -> read round trip travelled the socket.
const REQUEST: &[u8] = b"ping";
const REPLY_PREFIX: &[u8] = b"reply:";

/// A server task accepts one connection and echoes the request back with a marker; a client task
/// connects, sends the request, and reads the framed reply — every step awaiting the driver.
#[test]
fn a_tcp_request_and_reply_travel_through_the_driver() {
  let rt = Runtime::start(&config()).unwrap();
  let id = rt.shard_ids()[0];

  // The listener is bound (and listening) before the tasks spawn, so the client can connect at once.
  let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), BACKLOG).unwrap();
  let addr = listener.local_addr().unwrap();
  assert_ne!(addr.port(), 0, "the OS assigned a port");

  rt.spawn_on(id, async move {
    if let Ok(stream) = listener.accept().await {
      let mut buf = [0u8; 64];
      if let Ok(n) = stream.read(&mut buf).await {
        let _ = stream.write_all(REPLY_PREFIX).await;
        let _ = stream.write_all(&buf[..n]).await;
      }
    }
  })
  .unwrap();

  let (tx, rx) = channel();
  rt.spawn_on(id, async move {
    let outcome: Result<Vec<u8>, slates_rt::RtError> = async {
      let stream = TcpStream::connect(addr).await?;
      stream.write_all(REQUEST).await?;
      // Read until the whole framed reply has arrived (loopback may split the two writes).
      let want = REPLY_PREFIX.len() + REQUEST.len();
      let mut got = Vec::new();
      let mut buf = [0u8; 64];
      while got.len() < want {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
          break;
        }
        got.extend_from_slice(&buf[..n]);
      }
      Ok(got)
    }
    .await;
    let _ = tx.send(outcome);
  })
  .unwrap();

  match rx.recv_timeout(Duration::from_secs(5)) {
    Ok(Ok(bytes)) => {
      let mut expected = REPLY_PREFIX.to_vec();
      expected.extend_from_slice(REQUEST);
      assert_eq!(
        bytes, expected,
        "the client read back the server's framed echo"
      );
    }
    Ok(Err(e)) => panic!("the round trip failed: {e:?}"),
    Err(e) => {
      let counters = rt.shutdown();
      panic!("timed out ({e}); counters {counters:#?}");
    }
  }
  rt.shutdown();
}
