//! Async TCP through the runtime's driver (§4.6, R5): a server task accepts a connection, reads a
//! request and writes a framed reply, all awaiting the shard's driver; a client task connects, sends
//! the request and reads the reply back — the accept/read/write path the NFS loopback server runs on,
//! proven end to end on the readiness-native driver (kqueue here on macOS; epoll on Linux), with no
//! foreign runtime and both ends on the runtime's own sockets.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]
// Async TCP is the NFS loopback mount server's alone (macOS/Linux); Windows mounts through WinFsp, so
// the `tcp` module and this test are gated off it (the fleet transport is QUIC over UDP — see udp.rs).
#![cfg(not(windows))]

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
    wake_tracking: None,
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
      let counters = rt.shutdown().unwrap();
      panic!("timed out ({e}); counters {counters:#?}");
    }
  }
  rt.shutdown().unwrap();
}

/// A listener bound, reduced to its bare descriptor, and re-adopted serves on the SAME port — the
/// descriptor handoff a supervisor uses to keep the loopback port across a daemon restart (§4.6,
/// "One TCP loopback listener held by the anchor"). Proven by accepting a connection on the adopted
/// listener at the original port, so the port is stable across the hand-off, not re-assigned.
#[test]
fn a_listener_handed_over_by_descriptor_serves_on_the_same_port() {
  let rt = Runtime::start(&config()).unwrap();
  let id = rt.shard_ids()[0];

  // Bind and note the port, then hand the listener over as a bare descriptor and re-adopt it — the
  // supervisor-binds / daemon-adopts hand-off. The port must not change.
  let bound = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), BACKLOG).unwrap();
  let port = bound.local_addr().unwrap().port();
  assert_ne!(port, 0, "the OS assigned a port");
  let listener = TcpListener::from_fd(bound.into_fd()).unwrap();
  assert_eq!(
    listener.local_addr().unwrap().port(),
    port,
    "the adopted listener keeps the original port"
  );
  let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);

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
        "the client round-tripped through the re-adopted listener"
      );
    }
    Ok(Err(e)) => panic!("the round trip failed: {e:?}"),
    Err(e) => {
      let counters = rt.shutdown().unwrap();
      panic!("timed out ({e}); counters {counters:#?}");
    }
  }
  rt.shutdown().unwrap();
}

/// Shape: an idle spin window far longer than any round trip, so a reply that waited for the window to
/// end is unmistakable (§4.3's 2-competitive spin runs for the idle window before a park).
const LONG_SPIN_NS: u64 = 10_000_000_000;
/// Shape: the reply's bound: a tenth of the spin window, so it cannot have waited the window out yet
/// is thousands of loopback round trips on any host.
const REPLY_WITHIN: Duration = Duration::from_millis(1_000);

/// §4.3: a shard spinning in its idle window sees its driver's readiness, not only its rings: a request
/// arriving on a socket while the shard spins is answered at once, not after the window ends and the
/// shard parks. (A shard spins within the window its clients' activity opened, so each socket request had
/// waited out up to one window — 1.3 ms in a container — per hop.)
#[test]
fn a_spinning_shard_answers_a_socket_request_without_waiting_out_its_window() {
  let config = RuntimeConfig {
    spin_ns: LONG_SPIN_NS,
    ..config()
  };
  let rt = Runtime::start(&config).unwrap();
  let id = rt.shard_ids()[0];
  // A client's earlier request opened the shard's idle window, as the server notes one it served.
  rt.spawn_on(id, async {
    slates_rt::registry::with_current(|ctx| ctx.note_activity());
  })
  .unwrap();
  let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), BACKLOG).unwrap();
  let addr = listener.local_addr().unwrap();
  rt.spawn_on(id, async move {
    if let Ok(stream) = listener.accept().await {
      let mut buf = [0u8; 64];
      while let Ok(n) = stream.read(&mut buf).await {
        if n == 0 || stream.write_all(&buf[..n]).await.is_err() {
          break;
        }
      }
    }
  })
  .unwrap();
  let (tx, rx) = channel();
  rt.spawn_on(id, async move {
    let outcome: Result<Duration, slates_rt::RtError> = async {
      let stream = TcpStream::connect(addr).await?;
      // The first exchange settles the connection; the timed one finds the server shard spinning.
      let mut buf = [0u8; 64];
      stream.write_all(REQUEST).await?;
      stream.read(&mut buf).await?;
      let started = std::time::Instant::now();
      stream.write_all(REQUEST).await?;
      stream.read(&mut buf).await?;
      Ok(started.elapsed())
    }
    .await;
    let _ = tx.send(outcome);
  })
  .unwrap();
  let waited = rx
    .recv_timeout(Duration::from_nanos(LONG_SPIN_NS * 3))
    .expect("the exchange completed")
    .expect("the exchange succeeded");
  assert!(
    waited < REPLY_WITHIN,
    "the reply waited {waited:?}: a spinning shard must see socket readiness"
  );
  rt.shutdown().unwrap();
}
