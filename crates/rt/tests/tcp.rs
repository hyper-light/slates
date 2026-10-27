//! Async TCP through the runtime's driver (§4.6, R5): a server task accepts a connection, reads a
//! request and writes a framed reply, all awaiting the shard's driver; a client task connects, sends
//! the request and reads the reply back — the accept/read/write path the NFS loopback server runs on,
//! proven end to end on each readiness-native driver (kqueue on macOS, epoll on Linux, the IOCP driver's
//! AFD reactor on Windows), with no foreign runtime and both ends on the runtime's own sockets.

// Test harness code: an unwrap here is a failed test, which is what it should be; the shared helper below
// panics and indexes as a test does.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::sync::mpsc::channel;
use std::time::Duration;

// The address types come through `slates_rt::tcp` (the standard types re-exported), honouring the host-path
// wall's `std::net` guard on every platform.
use slates_rt::runtime::{Runtime, RuntimeConfig};
use slates_rt::tcp::{Ipv4Addr, SocketAddrV4};
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

/// A listener bound, reduced to its bare OS handle (a descriptor on Unix, a socket on Windows), and re-adopted
/// serves on the SAME port — the hand-off a supervisor uses to keep the loopback port across a daemon restart
/// (§4.6, "One TCP loopback listener held by the anchor"). Proven by accepting a connection on the adopted
/// listener at the original port, so the port is stable across the hand-off, not re-assigned.
#[test]
fn a_listener_handed_over_by_its_handle_serves_on_the_same_port() {
  let rt = Runtime::start(&config()).unwrap();
  let id = rt.shard_ids()[0];

  // Bind and note the port, then hand the listener over as a bare descriptor and re-adopt it — the
  // supervisor-binds / daemon-adopts hand-off. The port must not change.
  let bound = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), BACKLOG).unwrap();
  let port = bound.local_addr().unwrap().port();
  assert_ne!(port, 0, "the OS assigned a port");
  let listener = TcpListener::from_owned(bound.into_owned()).unwrap();
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

/// Shape: request/reply rounds timed after the connection settles; the median of them is judged, so a
/// scheduling hiccup on a loaded host moves one sample, not the verdict.
const ROUNDS: usize = 32;
/// Shape: a quarter of the smallest delayed-acknowledgement timer the supported kernels run (Linux
/// `TCP_DELACK_MIN` = HZ/25 = 40 ms, include/net/tcp.h; macOS `net.inet.tcp.delayed_ack` 100 ms), so a
/// round that waited for the peer's delayed ACK cannot pass, yet the bound is thousands of loopback
/// round trips.
const ROUND_WITHIN: Duration = Duration::from_millis(10);

/// §4.6 (the NFS loopback server's sockets): a server that answers one request in two writes — as it
/// does when two replies leave in separate batches — must not hold the second write until the client
/// acknowledges the first. With Nagle's algorithm on, the second small write waits for that ACK, and a
/// client with nothing to send delays its ACK (40 ms on Linux), so each round costs a delayed-ACK timer:
/// the 40 ms tails the hot-directory storm measured through a native Linux mount. Do run request/reply
/// rounds whose reply leaves in two writes; expect the median round far under the delayed-ACK timer.
#[test]
fn a_reply_in_two_writes_does_not_wait_for_the_peers_delayed_acknowledgement() {
  let rt = Runtime::start(&config()).unwrap();
  let id = rt.shard_ids()[0];
  let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), BACKLOG).unwrap();
  let addr = listener.local_addr().unwrap();
  rt.spawn_on(id, async move {
    if let Ok(stream) = listener.accept().await {
      let mut buf = [0u8; 64];
      while let Ok(n) = stream.read(&mut buf).await {
        if n == 0
          || stream.write_all(REPLY_PREFIX).await.is_err()
          || stream.write_all(&buf[..n]).await.is_err()
        {
          break;
        }
      }
    }
  })
  .unwrap();
  let (tx, rx) = channel();
  rt.spawn_on(id, async move {
    let outcome: Result<Vec<Duration>, slates_rt::RtError> = async {
      let stream = TcpStream::connect(addr).await?;
      let want = REPLY_PREFIX.len() + REQUEST.len();
      let mut rounds = Vec::with_capacity(ROUNDS);
      for _ in 0..ROUNDS {
        let started = std::time::Instant::now();
        stream.write_all(REQUEST).await?;
        let mut got = 0;
        let mut buf = [0u8; 64];
        while got < want {
          let n = stream.read(&mut buf).await?;
          if n == 0 {
            break;
          }
          got += n;
        }
        rounds.push(started.elapsed());
      }
      Ok(rounds)
    }
    .await;
    let _ = tx.send(outcome);
  })
  .unwrap();
  let mut rounds = rx
    .recv_timeout(Duration::from_secs(30))
    .expect("the rounds completed")
    .expect("the rounds succeeded");
  rounds.sort();
  let median = rounds[rounds.len() / 2];
  assert!(
    median < ROUND_WITHIN,
    "the median round took {median:?} (all: {rounds:?}): the second write waited for a delayed ACK"
  );
  rt.shutdown().unwrap();
}

/// Shape: three records whose whole run (8 MiB) is past any default socket send buffer (Linux `tcp_wmem` tops out at
/// 4 MiB, macOS's autotuned buffer below that), so one `writev` cannot take it and the run is completed in parts.
const LARGE_RECORD_BYTES: usize = 8 * 1024 * 1024 / 3;

/// A run of records sent by one vectored write arrives as their concatenation (2026-10-09: the NFS server sends a
/// turn's replies this way, never copying them into one buffer). Do: on one connection, send a run of three small
/// records, then a run of three records too large for the send buffer, each with `send_records_vectored`; read
/// everything on the other end. Expect: the small run was sent whole, the large one completed after a short send on
/// Unix (Windows took it in one send), and the bytes read are both runs' records in order, byte for byte.
#[test]
fn a_vectored_run_of_records_arrives_whole_and_in_order() {
  let rt = Runtime::start(&config()).unwrap();
  let id = rt.shard_ids()[0];
  let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), BACKLOG).unwrap();
  let addr = listener.local_addr().unwrap();
  let small: Vec<Vec<u8>> = (0..3u8).map(|n| vec![n; 100 + usize::from(n)]).collect();
  let large: Vec<Vec<u8>> = (0..3u8)
    .map(|n| {
      (0..LARGE_RECORD_BYTES)
        .map(|at| u8::try_from(at % 251).unwrap() ^ n)
        .collect()
    })
    .collect();
  let mut expected: Vec<u8> = small.concat();
  expected.extend(large.concat());
  let want = expected.len();

  let (sent_tx, sent_rx) = channel();
  let (small_run, large_run) = (small.clone(), large.clone());
  rt.spawn_on(id, async move {
    let outcome: Result<_, slates_rt::RtError> = async {
      let stream = listener.accept().await?;
      let small_slices: Vec<&[u8]> = small_run.iter().map(Vec::as_slice).collect();
      let large_slices: Vec<&[u8]> = large_run.iter().map(Vec::as_slice).collect();
      let first = stream.send_records_vectored(&small_slices).await?;
      let second = stream.send_records_vectored(&large_slices).await?;
      Ok((first, second))
    }
    .await;
    let _ = sent_tx.send(outcome);
  })
  .unwrap();

  let (read_tx, read_rx) = channel();
  rt.spawn_on(id, async move {
    let outcome: Result<Vec<u8>, slates_rt::RtError> = async {
      let stream = TcpStream::connect(addr).await?;
      let mut got = Vec::with_capacity(want);
      let mut buf = vec![0u8; 64 * 1024];
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
    let _ = read_tx.send(outcome);
  })
  .unwrap();

  let got = read_rx
    .recv_timeout(Duration::from_secs(30))
    .unwrap()
    .unwrap();
  let (first, second) = sent_rx
    .recv_timeout(Duration::from_secs(30))
    .unwrap()
    .unwrap();
  rt.shutdown().unwrap();
  assert!(
    matches!(first, slates_rt::tcp::Sent::Whole),
    "the small run went in one send: {first:?}"
  );
  // How much of a run one send takes is the kernel's. A Unix kernel bounds the send buffer below the large run
  // (Linux `tcp_wmem` at 4 MiB, macOS's autotuned buffer below that), so there the run is completed after a short
  // send, which runs the completion path. Windows' AFD took the whole 8 MiB run in one `WSASend` on loopback
  // (jupiter, 2026-10-26: `Whole`), so there the bytes read are the check; the completion path is the same code.
  #[cfg(unix)]
  assert!(
    matches!(second, slates_rt::tcp::Sent::Completed),
    "the large run was completed after a short send: {second:?}"
  );
  #[cfg(windows)]
  let _ = second;
  assert_eq!(got.len(), want, "every byte arrived");
  assert!(
    got == expected,
    "both runs' records arrived in order, byte for byte"
  );
}

/// Shape: how long a test waits for its task's answer. Windows retries a connect refused on loopback before it
/// reports the refusal (about two seconds), so the wait leaves room for that.
const WAIT: Duration = Duration::from_secs(10);

/// A loopback address at `port`.
fn local(port: u16) -> SocketAddrV4 {
  SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)
}

/// What `task` answered on a fresh one-shard runtime within [`WAIT`]; the test fails with the runtime's counters when
/// it never answered.
fn answered<T: Send + 'static>(task: impl Future<Output = T> + Send + 'static) -> T {
  let rt = Runtime::start(&config()).unwrap();
  let id = rt.shard_ids()[0];
  let (tx, rx) = channel();
  rt.spawn_on(id, async move {
    let _ = tx.send(task.await);
  })
  .unwrap();
  match rx.recv_timeout(WAIT) {
    Ok(answer) => {
      rt.shutdown().unwrap();
      answer
    }
    Err(e) => {
      let counters = rt.shutdown().unwrap();
      panic!("timed out ({e}); counters {counters:#?}");
    }
  }
}

/// Do: connect to a loopback port nothing listens on (a listener bound, then dropped). Expect a typed refusal naming
/// `connect`, neither a stream nor a hang: the failed handshake reaches the writable edge (on Windows the AFD
/// reactor's `AFD_POLL_CONNECT_FAIL`) and `SO_ERROR` names it.
#[test]
fn a_connection_nothing_accepts_is_refused_typed() {
  let addr = TcpListener::bind(local(0), BACKLOG)
    .unwrap()
    .local_addr()
    .unwrap();
  let answer = answered(async move { TcpStream::connect(addr).await.map(|_| ()) });
  assert!(
    matches!(
      answer,
      Err(slates_rt::RtError::DriverRefused {
        call: "connect",
        code: Some(_)
      })
    ),
    "{answer:?}"
  );
}

/// Do: connect a pair and ask each side whether its peer closed; then shut the client's stream down and ask the server
/// again once readability says something changed. Expect false while both stand and true after: the kernel's own
/// record of the connection (`TCP_INFO`, `TCP_CONNECTION_INFO`, `SIO_TCP_INFO`) left the established state (A-113).
#[test]
fn a_peers_close_is_seen_in_the_kernels_record_of_the_connection() {
  let answer = answered(async move {
    let listener = TcpListener::bind(local(0), BACKLOG)?;
    let client = TcpStream::connect(listener.local_addr()?).await?;
    let server = listener.accept().await?;
    let standing = (server.peer_closed()?, client.peer_closed()?);
    client.shutdown()?;
    server.wait_readable().await?;
    Ok::<_, slates_rt::RtError>((standing, server.peer_closed()?))
  });
  assert_eq!(answer.unwrap(), ((false, false), true));
}

/// Do: send twelve bytes, peek them, discard the first eight, then read. Expect the peek to see all twelve and leave
/// them queued, and the read to return the last four alone: a peeked request stays in the kernel until it is
/// consumed (A-113), on every platform (`MSG_PEEK`; `MSG_TRUNC` on Linux, a copy elsewhere).
#[test]
fn a_peeked_request_stays_queued_until_it_is_discarded() {
  const SENT: &[u8; 12] = b"headbodytail";
  let answer = answered(async move {
    let listener = TcpListener::bind(local(0), BACKLOG)?;
    let client = TcpStream::connect(listener.local_addr()?).await?;
    let server = listener.accept().await?;
    client.write_all(SENT).await?;
    let mut peeked = [0u8; 12];
    let mut seen = 0;
    while seen < SENT.len() {
      server.wait_readable().await?;
      seen = server.peek_now(&mut peeked)?.unwrap_or(0);
    }
    let mut scratch = [0u8; 8];
    server.discard(8, &mut scratch)?;
    let mut rest = [0u8; 4];
    let read = server.read(&mut rest).await?;
    Ok::<_, slates_rt::RtError>((peeked, read, rest))
  });
  let (peeked, read, rest) = answer.unwrap();
  assert_eq!(&peeked, SENT, "the peek saw every byte");
  assert_eq!(
    (read, &rest),
    (4, b"tail"),
    "the discard took the first eight alone"
  );
}

/// Do: ask a stream for a receive low-water mark of 4 KiB. Expect it kept on Unix; on Windows, a typed refusal
/// naming `setsockopt(SO_RCVLOWAT)`, since Windows' TCP provider keeps no such mark (Microsoft Learn, SOL_SOCKET
/// options), and the answer must say so rather than pretend.
#[test]
fn a_receive_low_water_mark_is_kept_where_the_kernel_keeps_one() {
  let answer = answered(async move {
    let listener = TcpListener::bind(local(0), BACKLOG)?;
    let client = TcpStream::connect(listener.local_addr()?).await?;
    let _server = listener.accept().await?;
    Ok::<_, slates_rt::RtError>(client.want_bytes(4096))
  })
  .unwrap();
  #[cfg(unix)]
  assert!(answer.is_ok(), "{answer:?}");
  #[cfg(windows)]
  assert!(
    matches!(
      answer,
      Err(slates_rt::RtError::DriverRefused {
        call: "setsockopt(SO_RCVLOWAT)",
        ..
      })
    ),
    "{answer:?}"
  );
}

/// Format: this test's name, which the re-invoked binary runs as the child.
const INHERITANCE_TEST: &str = "a_child_started_while_a_listener_is_open_keeps_no_copy_of_it";
/// Format: the argument making the re-invoked binary the child, which libtest takes as a name filter that matches no
/// test: the child holds whatever it inherited until its input closes.
const HOLDER: &str = "role=holder";

/// Do: bind a listener, start a child process while it is open, close the listener, then bind its port again. The
/// child is this binary re-invoked through the standard library, which on Windows starts every child inheriting each
/// inheritable handle of this process, and it holds what it inherited until its input closes. Expect the port free at
/// once: no child keeps a copy of the listener (Windows: `WSA_FLAG_NO_HANDLE_INHERIT`; Unix: close-on-exec), so a
/// process the daemon starts can neither accept its connections nor keep its port after the daemon closes it.
#[test]
fn a_child_started_while_a_listener_is_open_keeps_no_copy_of_it() {
  if std::env::args().any(|argument| argument == HOLDER) {
    let _ = std::io::Read::read_to_end(&mut std::io::stdin(), &mut Vec::new());
    std::process::exit(0);
  }
  let listener = TcpListener::bind(local(0), BACKLOG).unwrap();
  let port = listener.local_addr().unwrap().port();
  let mut child = std::process::Command::new(std::env::current_exe().unwrap())
    .args(["--exact", INHERITANCE_TEST, "--nocapture", HOLDER])
    .stdin(std::process::Stdio::piped())
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .spawn()
    .unwrap();
  drop(listener);
  let rebound = std::net::TcpListener::bind(("127.0.0.1", port));
  drop(child.stdin.take());
  let ended = child.wait().unwrap();
  assert!(
    rebound.is_ok(),
    "the port is free once the listener closes, so the child holds no copy: {rebound:?}"
  );
  assert!(
    ended.success(),
    "the child ended when its input closed: {ended}"
  );
}

/// Do (Windows): bind a listener to the wildcard address, then bind another socket, with no option at all, to the same
/// port on the loopback address, as a local process taking its connections would. Expect the second bind refused
/// `WSAEACCES`: the port is the listener's on every address (`SO_EXCLUSIVEADDRUSE`). Without it Windows binds the second
/// socket, the more specific address, and hands it the connections made to that address (Microsoft Learn, "Using
/// SO_REUSEADDR and SO_EXCLUSIVEADDRUSE"). Measured 2026-10-26 on jupiter (Windows 10.0.26200): beside a default listener
/// on `0.0.0.0` the plain loopback bind succeeds. A second bind of the very address the listener holds is refused either
/// way (`WSAEADDRINUSE` plain, `WSAEACCES` with `SO_REUSEADDR`), which is why this test until then, binding both to
/// loopback, passed without the option too.
#[cfg(windows)]
#[test]
fn no_other_socket_can_bind_a_windows_listeners_port() {
  use windows_sys::Win32::Networking::WinSock::{
    AF_INET, IN_ADDR, IN_ADDR_0, INVALID_SOCKET, IPPROTO_TCP, SOCK_STREAM, SOCKADDR, SOCKADDR_IN,
    SOCKET_ERROR, WSAEACCES, WSAGetLastError, bind, closesocket, socket,
  };
  // The runtime's bind starts Winsock in this process.
  let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0), BACKLOG).unwrap();
  let port = listener.local_addr().unwrap().port();
  // SAFETY: a plain socket creation, checked against INVALID_SOCKET and closed below.
  let other = unsafe { socket(i32::from(AF_INET), SOCK_STREAM, IPPROTO_TCP) };
  assert_ne!(other, INVALID_SOCKET);
  let address = SOCKADDR_IN {
    sin_family: AF_INET,
    sin_port: port.to_be(),
    sin_addr: IN_ADDR {
      S_un: IN_ADDR_0 {
        S_addr: u32::from_ne_bytes(Ipv4Addr::LOCALHOST.octets()),
      },
    },
    sin_zero: [0; 8],
  };
  // SAFETY: a live SOCKADDR_IN passed with its own length.
  let bound = unsafe {
    bind(
      other,
      (&raw const address).cast::<SOCKADDR>(),
      i32::try_from(size_of::<SOCKADDR_IN>()).unwrap(),
    )
  };
  // SAFETY: reads this thread's last Winsock error, before anything else can set it.
  let error = unsafe { WSAGetLastError() };
  // SAFETY: the socket made above, closed once.
  unsafe { closesocket(other) };
  assert_eq!(
    (bound, error),
    (SOCKET_ERROR, WSAEACCES),
    "the port is the listener's alone, on every address"
  );
  drop(listener);
}
