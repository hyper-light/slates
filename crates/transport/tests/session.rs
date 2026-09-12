//! The session plane as a *live* connection over UDP (§4.10a §8): two `Endpoint`s complete the
//! `rustls::quic` TLS 1.3 handshake over the runtime's UDP socket, then the client sends a stream in
//! packets protected by the handshake's 1-RTT keys and the server reassembles it — the whole session
//! stack (handshake + packet protection + framing + stream reassembly) exercised end to end,
//! deterministically at N=1 on the simulation UDP fabric (no OS network). The session-plane analogue
//! of `plane.rs`. Test by use (R5).

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc::channel;

use rustix::net::{Ipv4Addr, SocketAddrV4};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::SimRuntime;
use slates_rt::udp::UdpSocket;
use slates_transport::demux::{Demux, DemuxCounters};
use slates_transport::endpoint::{Endpoint, EndpointError};
use slates_transport::handshake::Identity;

const NAME: &str = "slates-node";
const STREAM_ID: u64 = 1;
const FRAME_CAP: usize = 16;

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

/// A self-signed identity, minted with `ring` via `rcgen` — the test's stand-in for enrollment.
fn self_signed(name: &str) -> Identity {
  let key = rcgen::KeyPair::generate().unwrap();
  let cert = rcgen::CertificateParams::new(vec![name.to_owned()])
    .unwrap()
    .self_signed(&key)
    .unwrap();
  Identity::from_der(
    cert.der().clone(),
    PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
  )
}

/// Waits (yielding on the runtime) for a value on a just-sent-once channel. Takes the receiver by
/// value: an owned `Receiver` is `Send` (held across the await), whereas `&Receiver` is not.
async fn recv_port(rx: std::sync::mpsc::Receiver<u16>) -> u16 {
  loop {
    if let Ok(p) = rx.try_recv() {
      return p;
    }
    slates_rt::futures::sleep(1_000).await;
  }
}

/// A stream travels over a live, handshaken, packet-protected session and reassembles exactly.
#[test]
fn a_stream_flows_over_a_live_session() {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];

  let content: Vec<u8> = (0..300u16)
    .map(|i| u8::try_from(i % 251).unwrap_or(0))
    .collect();
  let server_identity = self_signed(NAME);
  let client_identity = self_signed(NAME);
  let server_cert = server_identity.certificate();
  let client_cert = client_identity.certificate();

  let (server_port_tx, server_port_rx) = channel();
  let (client_port_tx, client_port_rx) = channel();
  let (result_tx, result_rx) = channel();
  let expected = content.clone();

  // The server: bind, exchange ports, present its identity, handshake, reassemble the stream.
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = server_port_tx.send(socket.local_addr().unwrap().port());
      let client_port = recv_port(client_port_rx).await;
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, client_port);
      let outcome = async {
        let mut server = Endpoint::server(
          socket,
          peer,
          &server_identity,
          std::slice::from_ref(&client_cert),
          FRAME_CAP,
        )
        .map_err(|e| format!("{e:?}"))?;
        server.establish().await.map_err(|e| format!("{e:?}"))?;
        server
          .recv_stream(STREAM_ID)
          .await
          .map_err(|e| format!("{e:?}"))
      }
      .await;
      let _ = result_tx.send(outcome);
    })
    .unwrap();

  // The client: bind, exchange ports, pin the server's cert, handshake, send the stream.
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = client_port_tx.send(socket.local_addr().unwrap().port());
      let server_port = recv_port(server_port_rx).await;
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
      let mut client = Endpoint::client(
        socket,
        peer,
        &client_identity,
        &server_cert,
        NAME,
        FRAME_CAP,
      )
      .unwrap();
      client.establish().await.unwrap();
      client.send_stream(STREAM_ID, &content).await.unwrap();
    })
    .unwrap();

  sim.run_until_idle();

  match result_rx.try_recv() {
    Ok(Ok(received)) => assert_eq!(
      received, expected,
      "the stream reassembled over the live session"
    ),
    other => panic!("the live session did not deliver the stream: {other:?}"),
  }
}

/// A request/reply exchange completes over a live session, the server producing its reply
/// **asynchronously** (`serve_once_async`, the shape a forwarded verb needs — its reply comes from another
/// shard or await; here the handler yields before replying, exercising the pending-await path): the client
/// sends a request, the server transforms it and replies, the client receives exactly the reply — the RPC
/// seam register/placement and cross-region forwarding will ride (§4.8 lookups route to the owner). Do X,
/// expect Y.
#[test]
fn a_request_gets_a_reply_over_a_live_session() {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];

  let request: Vec<u8> = (0..250u16)
    .map(|i| u8::try_from(i % 251).unwrap_or(0))
    .collect();
  // The reply the server computes: the request with every byte incremented — a transform, so a reply
  // echoed by mistake or a crossed stream would show.
  let expected_reply: Vec<u8> = request.iter().map(|b| b.wrapping_add(1)).collect();
  let server_identity = self_signed(NAME);
  let client_identity = self_signed(NAME);
  let server_cert = server_identity.certificate();
  let client_cert = client_identity.certificate();

  let (server_port_tx, server_port_rx) = channel();
  let (client_port_tx, client_port_rx) = channel();
  let (result_tx, result_rx) = channel();

  // The server: handshake, serve one request, reply with the transform.
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = server_port_tx.send(socket.local_addr().unwrap().port());
      let client_port = recv_port(client_port_rx).await;
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, client_port);
      let mut server = Endpoint::server(
        socket,
        peer,
        &server_identity,
        std::slice::from_ref(&client_cert),
        FRAME_CAP,
      )
      .unwrap();
      server.establish().await.unwrap();
      server
        .serve_once_async(|_, req| async move {
          // Yield before producing the reply, so the pending-await path of `serve_once_async` is exercised
          // (a forwarded verb's reply comes from an `xshard` await, not synchronously).
          slates_rt::futures::sleep(1_000).await;
          req.iter().map(|b| b.wrapping_add(1)).collect()
        })
        .await
        .unwrap();
    })
    .unwrap();

  // The client: handshake, send the request, receive the reply.
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = client_port_tx.send(socket.local_addr().unwrap().port());
      let server_port = recv_port(server_port_rx).await;
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
      let outcome = async {
        let mut client = Endpoint::client(
          socket,
          peer,
          &client_identity,
          &server_cert,
          NAME,
          FRAME_CAP,
        )
        .map_err(|e| format!("{e:?}"))?;
        client.establish().await.map_err(|e| format!("{e:?}"))?;
        let reply = client
          .request(STREAM_ID, &request)
          .await
          .map_err(|e| format!("{e:?}"))?;
        // The server acknowledged the request (its acknowledgement rode the reply's data packets), so
        // the client has folded in an RTT sample over the real loopback round trip (RFC 9002 §5.3).
        Ok::<_, String>((reply, client.smoothed_rtt()))
      }
      .await;
      let _ = result_tx.send(outcome);
    })
    .unwrap();

  sim.run_until_idle();

  match result_rx.try_recv() {
    Ok(Ok((reply, smoothed_rtt))) => {
      assert_eq!(
        reply, expected_reply,
        "the reply arrived over the live session"
      );
      assert!(
        smoothed_rtt > 0,
        "the client estimated the round-trip time from the request's acknowledgement"
      );
    }
    other => panic!("the request/reply did not complete: {other:?}"),
  }
}

/// Repeated request/reply exchanges on one endpoint keep the connection's packet numbers strictly
/// increasing — a number is never reused under the 1-RTT keys (RFC 9000 §12.3, RFC 9001 §5.3). Before
/// the persistent-connection fix each RPC restarted the counter at zero, reusing packet numbers (and
/// so AEAD nonces) under the fixed keys. Each exchange also returns its own reply correctly.
#[test]
fn repeated_exchanges_never_reuse_packet_numbers() {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];
  const EXCHANGES: u64 = 4;

  let server_identity = self_signed(NAME);
  let client_identity = self_signed(NAME);
  let server_cert = server_identity.certificate();
  let client_cert = client_identity.certificate();
  let (server_port_tx, server_port_rx) = channel();
  let (client_port_tx, client_port_rx) = channel();
  let (result_tx, result_rx) = channel();

  // The server: serve `EXCHANGES` requests in turn, each echoed with a byte-transform.
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = server_port_tx.send(socket.local_addr().unwrap().port());
      let client_port = recv_port(client_port_rx).await;
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, client_port);
      let mut server = Endpoint::server(
        socket,
        peer,
        &server_identity,
        std::slice::from_ref(&client_cert),
        FRAME_CAP,
      )
      .unwrap();
      server.establish().await.unwrap();
      for _ in 0..EXCHANGES {
        server
          .serve_once(|_, req| req.iter().map(|b| b.wrapping_add(7)).collect())
          .await
          .unwrap();
      }
    })
    .unwrap();

  // The client: make `EXCHANGES` requests on one endpoint, recording the packet-number cursor before
  // each; assert every reply is right and the cursor is strictly increasing (never reset, never reused).
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = client_port_tx.send(socket.local_addr().unwrap().port());
      let server_port = recv_port(server_port_rx).await;
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
      let outcome = async {
        let mut client = Endpoint::client(
          socket,
          peer,
          &client_identity,
          &server_cert,
          NAME,
          FRAME_CAP,
        )
        .map_err(|e| format!("{e:?}"))?;
        client.establish().await.map_err(|e| format!("{e:?}"))?;
        let mut cursors = vec![client.tx_packet_number()];
        for exchange in 0..EXCHANGES {
          let request: Vec<u8> = (0..40u8)
            .map(|b| b.wrapping_add(u8::try_from(exchange).unwrap_or(0)))
            .collect();
          let reply = client
            .request(exchange + 1, &request)
            .await
            .map_err(|e| format!("{e:?}"))?;
          let expected: Vec<u8> = request.iter().map(|b| b.wrapping_add(7)).collect();
          if reply != expected {
            return Err(format!("exchange {exchange}: wrong reply"));
          }
          cursors.push(client.tx_packet_number());
        }
        // Strictly increasing: every exchange consumed fresh packet numbers.
        for pair in cursors.windows(2) {
          if pair[1] <= pair[0] {
            return Err(format!(
              "packet numbers not strictly increasing: {cursors:?}"
            ));
          }
        }
        Ok(())
      }
      .await;
      let _ = result_tx.send(outcome);
    })
    .unwrap();

  sim.run_until_idle();

  match result_rx.try_recv() {
    Ok(Ok(())) => {}
    other => panic!("repeated exchanges did not keep packet numbers monotonic: {other:?}"),
  }
}

/// Two identities built from one certificate and key: what a node that re-dials after losing its
/// session presents — the same enrolled identity, a fresh socket.
fn same_identity_twice(name: &str) -> (Identity, Identity) {
  let key = rcgen::KeyPair::generate().unwrap();
  let cert = rcgen::CertificateParams::new(vec![name.to_owned()])
    .unwrap()
    .self_signed(&key)
    .unwrap();
  let der = PrivateKeyDer::try_from(key.serialize_der()).unwrap();
  (
    Identity::from_der(cert.der().clone(), der.clone_key()),
    Identity::from_der(cert.der().clone(), der),
  )
}

/// Waits (yielding on the runtime) until `count` values have arrived on a channel. Takes the receiver by
/// value (an owned `Receiver` is `Send`; a borrow of one is not).
async fn recv_count<T>(rx: std::sync::mpsc::Receiver<T>, count: usize) -> Vec<T> {
  let mut out = Vec::new();
  while out.len() < count {
    match rx.try_recv() {
      Ok(value) => out.push(value),
      Err(_) => slates_rt::futures::sleep(1_000).await,
    }
  }
  out
}

/// What a serve task reports when its session ends: how many requests it served, and how.
#[derive(Debug)]
struct Served {
  requests: u32,
  ended: String,
}

/// Serves `session` for up to `max_requests` requests (or until it fails), replying to each with every
/// byte + `add`; reports how it ended. Spawned per accepted session, as a fleet node spawns its serve
/// tasks — bounded here so the simulation, which runs until nothing is pending, can go idle (a serve
/// loop waiting on a live session re-arms its probe timer forever).
async fn serve_up_to(
  mut session: Endpoint,
  add: u8,
  report: std::sync::mpsc::Sender<Served>,
  max_requests: u32,
) {
  let mut requests = 0u32;
  let ended = match session.establish().await {
    Ok(()) => loop {
      if requests >= max_requests {
        break "done".to_owned();
      }
      match session
        .serve_once(|_, req| req.iter().map(|b| b.wrapping_add(add)).collect())
        .await
      {
        Ok(()) => requests += 1,
        Err(EndpointError::Closed) => break "closed".to_owned(),
        Err(e) => break format!("{e:?}"),
      }
    },
    Err(e) => format!("handshake: {e:?}"),
  };
  let _ = report.send(Served { requests, ended });
}

/// Dials the server at the port that arrives on `port_rx`, handshakes, and returns the reply to each
/// request in `requests`, in order — or the first error.
async fn dial_and_request(
  identity: Identity,
  server_cert: CertificateDer<'static>,
  port_rx: std::sync::mpsc::Receiver<u16>,
  requests: Vec<Vec<u8>>,
) -> Result<Vec<Vec<u8>>, String> {
  let socket =
    UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).map_err(|e| format!("{e:?}"))?;
  let server_port = recv_port(port_rx).await;
  let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
  let mut client = Endpoint::client(socket, peer, &identity, &server_cert, NAME, FRAME_CAP)
    .map_err(|e| format!("{e:?}"))?;
  client.establish().await.map_err(|e| format!("{e:?}"))?;
  let mut replies = Vec::new();
  for request in &requests {
    replies.push(
      client
        .request(STREAM_ID, request)
        .await
        .map_err(|e| format!("{e:?}"))?,
    );
  }
  Ok(replies)
}

/// What the server of these tests is told: its identity, the peers it admits, how many sessions it may
/// hold, how many requests each accepted session serves (in accept order, each replying with every byte
/// plus `add`), where to publish its port, where to report each session's end, how many clients to wait
/// for before reporting the demultiplexer's counters, and where.
struct ServerPlan {
  identity: Identity,
  allowed: Vec<CertificateDer<'static>>,
  max_sessions: usize,
  sessions: Vec<u32>,
  add: u8,
  port_txs: Vec<std::sync::mpsc::Sender<u16>>,
  served_tx: std::sync::mpsc::Sender<Served>,
  done_rx: std::sync::mpsc::Receiver<()>,
  clients: usize,
  counters_tx: std::sync::mpsc::Sender<(DemuxCounters, usize)>,
}

/// The server of these tests: one demultiplexer on one socket, its receive loop as its own task, a serve
/// task per accepted session, then — once the clients have reported done — the demultiplexer's counters
/// and live-session count, reported out. The body runs as a task local to the shard (a demultiplexer is
/// the shard's own, not `Send`); this wrapper is what `spawn_on` takes.
async fn serve_shared_socket(plan: ServerPlan) {
  let task = slates_rt::futures::spawn(serve_shared_socket_on_shard(plan)).unwrap();
  let _ = slates_rt::futures::detach(task);
}

async fn serve_shared_socket_on_shard(plan: ServerPlan) {
  let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
  let identity: &'static Identity = Box::leak(Box::new(plan.identity));
  let demux = Demux::start(socket, identity, plan.allowed, FRAME_CAP, plan.max_sessions);
  let port = demux.local_addr().unwrap().port();
  for tx in plan.port_txs {
    let _ = tx.send(port);
  }
  let run = slates_rt::futures::spawn(async move {
    let _ = demux.run().await;
  })
  .unwrap();
  let _ = slates_rt::futures::detach(run);
  for max_requests in plan.sessions {
    let session = demux.accept().await;
    let serve = slates_rt::futures::spawn(serve_up_to(
      session,
      plan.add,
      plan.served_tx.clone(),
      max_requests,
    ))
    .unwrap();
    let _ = slates_rt::futures::detach(serve);
  }
  let _ = recv_count(plan.done_rx, plan.clients).await;
  let _ = plan.counters_tx.send((demux.counters(), demux.sessions()));
}

/// AC (§4.10a §8 "connection IDs"; §4.8 one serve socket per plane): **two clients dial one accepting
/// socket** and each gets its own reply — the demultiplexer opens a session per dialer, routes each
/// client's 1-RTT packets to its session by the connection id in the header, and neither exchange
/// crosses into the other. Non-vacuous: before this, an accepting socket pinned the first source it
/// heard, so the second client could never have been answered on the same socket; and the counters
/// show two sessions opened, none refused, no packet routed to an unknown id.
#[test]
fn two_clients_share_one_accepting_socket_and_each_gets_its_own_reply() {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];
  let server_identity = self_signed(NAME);
  let server_cert = server_identity.certificate();
  let first = self_signed(NAME);
  let second = self_signed(NAME);
  let allowed = vec![first.certificate(), second.certificate()];
  let (port_tx_1, port_rx_1) = channel();
  let (port_tx_2, port_rx_2) = channel();
  let (served_tx, _served_rx) = channel();
  let (done_tx, done_rx) = channel();
  let (counters_tx, counters_rx) = channel();
  let (result_tx, result_rx) = channel();

  sim
    .spawn_on(
      id,
      serve_shared_socket(ServerPlan {
        identity: server_identity,
        allowed,
        max_sessions: 2,
        sessions: vec![1, 1],
        add: 9,
        port_txs: vec![port_tx_1, port_tx_2],
        served_tx,
        done_rx,
        clients: 2,
        counters_tx,
      }),
    )
    .unwrap();
  for (identity, port_rx, request) in [
    (first, port_rx_1, b"first client".to_vec()),
    (second, port_rx_2, b"second client, longer".to_vec()),
  ] {
    let (result_tx, done_tx, server_cert) =
      (result_tx.clone(), done_tx.clone(), server_cert.clone());
    sim
      .spawn_on(id, async move {
        let outcome = dial_and_request(identity, server_cert, port_rx, vec![request.clone()]).await;
        let _ = result_tx.send((request, outcome));
        let _ = done_tx.send(());
      })
      .unwrap();
  }
  sim.run_until_idle();

  for _ in 0..2 {
    let (request, outcome) = result_rx.try_recv().expect("each client finished");
    let expected: Vec<u8> = request.iter().map(|b| b.wrapping_add(9)).collect();
    assert_eq!(
      outcome.as_deref(),
      Ok(&[expected][..]),
      "each client's own request came back transformed over its own session"
    );
  }
  let (counters, live) = counters_rx.try_recv().expect("the server reported");
  assert_eq!(counters.opened, 2, "one session per dialer: {counters:?}");
  assert_eq!(counters.sessions_refused, 0, "{counters:?}");
  assert_eq!(
    counters.unknown_id, 0,
    "every packet found its session: {counters:?}"
  );
  assert_eq!(
    live, 0,
    "each session released its slot when its serve task ended"
  );
}

/// AC (§4.10a §8, hostile): a datagram shaped like a 1-RTT packet but naming **no session** is dropped
/// and counted — never a panic, never delivered to a live session — and the live session keeps serving
/// afterwards (the demultiplexer's routing, not the session's crypto, refused it). Non-vacuous: the
/// unknown-id counter moves exactly once, and the request sent after the stray is answered.
#[test]
fn a_packet_naming_no_session_is_dropped_and_counted_while_the_live_session_serves_on() {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];
  let server_identity = self_signed(NAME);
  let server_cert = server_identity.certificate();
  let client = self_signed(NAME);
  let allowed = vec![client.certificate()];
  let (port_tx, port_rx) = channel();
  let (stray_port_tx, stray_port_rx) = channel();
  let (served_tx, _served_rx) = channel();
  let (done_tx, done_rx) = channel();
  let (counters_tx, counters_rx) = channel();
  let (result_tx, result_rx) = channel();

  sim
    .spawn_on(
      id,
      serve_shared_socket(ServerPlan {
        identity: server_identity,
        allowed,
        max_sessions: 1,
        sessions: vec![2],
        add: 1,
        port_txs: vec![port_tx, stray_port_tx],
        served_tx,
        done_rx,
        clients: 1,
        counters_tx,
      }),
    )
    .unwrap();
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let server_port = recv_port(port_rx).await;
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
      let outcome = async {
        let mut endpoint = Endpoint::client(socket, peer, &client, &server_cert, NAME, FRAME_CAP)
          .map_err(|e| format!("{e:?}"))?;
        endpoint.establish().await.map_err(|e| format!("{e:?}"))?;
        let before = endpoint
          .request(STREAM_ID, b"before the stray")
          .await
          .map_err(|e| format!("{e:?}"))?;
        // The stray: a short header (fixed bit set) naming an id no session has, from a third socket.
        let stray_socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
          .map_err(|e| format!("{e:?}"))?;
        let stray_port = recv_port(stray_port_rx).await;
        let mut stray = vec![0x40u8];
        stray.extend_from_slice(&[0xEE; 8]);
        stray.extend_from_slice(&[0; 32]);
        stray_socket
          .send_to(&stray, SocketAddrV4::new(Ipv4Addr::LOCALHOST, stray_port))
          .map_err(|e| format!("{e:?}"))?;
        // Let the demultiplexer see it before the next request.
        slates_rt::futures::sleep(1_000_000).await;
        let after = endpoint
          .request(STREAM_ID, b"after the stray")
          .await
          .map_err(|e| format!("{e:?}"))?;
        Ok::<_, String>((before, after))
      }
      .await;
      let _ = result_tx.send(outcome);
      let _ = done_tx.send(());
    })
    .unwrap();
  sim.run_until_idle();

  match result_rx.try_recv() {
    Ok(Ok((before, after))) => {
      assert_eq!(
        before,
        b"before the stray"
          .iter()
          .map(|b| b + 1)
          .collect::<Vec<u8>>()
      );
      assert_eq!(
        after,
        b"after the stray"
          .iter()
          .map(|b| b + 1)
          .collect::<Vec<u8>>()
      );
    }
    other => panic!("the session did not serve around the stray: {other:?}"),
  }
  let (counters, live) = counters_rx.try_recv().expect("the server reported");
  assert_eq!(
    counters.unknown_id, 1,
    "the stray was counted, once: {counters:?}"
  );
  assert_eq!(
    counters.opened, 1,
    "the stray opened no session: {counters:?}"
  );
  assert_eq!(
    live, 0,
    "the session released its slot when its serve task ended"
  );
}

/// AC (§4.8 "reconnection after a mid-run session loss"; §4.10a §8): a peer that **re-dials with the
/// same identity** from a fresh socket gets a new session that **replaces** its old one — the old
/// session's serve loop ends with `Closed`, the new session serves — so a node whose session was lost
/// mid-run reconnects without the server being told. Non-vacuous: the replaced counter moves exactly
/// once; the first serve task reports ending `closed` after one request; the second request is answered.
#[test]
fn a_peer_that_redials_replaces_its_old_session() {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];
  let server_identity = self_signed(NAME);
  let server_cert = server_identity.certificate();
  let (first, again) = same_identity_twice(NAME);
  let allowed = vec![first.certificate()];
  let (port_tx_1, port_rx_1) = channel();
  let (port_tx_2, port_rx_2) = channel();
  let (served_tx, served_rx) = channel();
  let (done_tx, done_rx) = channel();
  let (counters_tx, counters_rx) = channel();
  let (result_tx, result_rx) = channel();
  let (first_done_tx, first_done_rx) = channel();

  sim
    .spawn_on(
      id,
      serve_shared_socket(ServerPlan {
        identity: server_identity,
        allowed,
        max_sessions: 2,
        sessions: vec![2, 1],
        add: 3,
        port_txs: vec![port_tx_1, port_tx_2],
        served_tx,
        done_rx,
        clients: 2,
        counters_tx,
      }),
    )
    .unwrap();
  // The first dial: one request, then the peer "loses" its session (drops its endpoint) and re-dials.
  let (result_tx_1, done_tx_1, cert_1) = (result_tx.clone(), done_tx.clone(), server_cert.clone());
  sim
    .spawn_on(id, async move {
      let outcome = dial_and_request(first, cert_1, port_rx_1, vec![b"first dial".to_vec()]).await;
      let _ = result_tx_1.send(("first", outcome));
      let _ = first_done_tx.send(());
      let _ = done_tx_1.send(());
    })
    .unwrap();
  sim
    .spawn_on(id, async move {
      let _ = recv_count(first_done_rx, 1).await;
      let outcome =
        dial_and_request(again, server_cert, port_rx_2, vec![b"re-dial".to_vec()]).await;
      let _ = result_tx.send(("again", outcome));
      let _ = done_tx.send(());
    })
    .unwrap();
  sim.run_until_idle();

  for _ in 0..2 {
    let (which, outcome) = result_rx.try_recv().expect("both dials finished");
    let expected: Vec<u8> = match which {
      "first" => b"first dial".iter().map(|b| b + 3).collect(),
      _ => b"re-dial".iter().map(|b| b + 3).collect(),
    };
    assert_eq!(outcome.as_deref(), Ok(&[expected][..]), "{which}: served");
  }
  let (counters, _live) = counters_rx.try_recv().expect("the server reported");
  assert_eq!(
    counters.replaced, 1,
    "the re-dial replaced the old session: {counters:?}"
  );
  let served: Vec<Served> = served_rx.try_iter().collect();
  let closed: Vec<&Served> = served.iter().filter(|s| s.ended == "closed").collect();
  assert_eq!(
    closed.len(),
    1,
    "the old session's serve loop ended with Closed after its one request: {served:?}"
  );
  assert_eq!(closed[0].requests, 1, "{served:?}");
}

/// The single-session degenerate of the shared socket (R8): a server that is **not** told the client's
/// address in advance (only the client is told the server's published address, as a fleet node
/// advertises its socket) accepts the dialer, completes the handshake and replies. Non-vacuous: the
/// server never received the client's port; the reply arriving proves the session was opened for the
/// source the demultiplexer heard and routed by the connection id it derived.
#[test]
fn an_accepted_server_learns_its_peer_and_replies() {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];
  let request: Vec<u8> = (0..300u16)
    .map(|i| u8::try_from(i % 251).unwrap_or(0))
    .collect();
  let expected_reply: Vec<u8> = request.iter().map(|b| b.wrapping_add(9)).collect();
  let server_identity = self_signed(NAME);
  let server_cert = server_identity.certificate();
  let client_identity = self_signed(NAME);
  let allowed = vec![client_identity.certificate()];
  let (port_tx, port_rx) = channel();
  let (served_tx, _served_rx) = channel();
  let (done_tx, done_rx) = channel();
  let (counters_tx, counters_rx) = channel();
  let (result_tx, result_rx) = channel();

  sim
    .spawn_on(
      id,
      serve_shared_socket(ServerPlan {
        identity: server_identity,
        allowed,
        max_sessions: 1,
        sessions: vec![1],
        add: 9,
        port_txs: vec![port_tx],
        served_tx,
        done_rx,
        clients: 1,
        counters_tx,
      }),
    )
    .unwrap();
  sim
    .spawn_on(id, async move {
      let outcome = dial_and_request(client_identity, server_cert, port_rx, vec![request]).await;
      let _ = result_tx.send(outcome);
      let _ = done_tx.send(());
    })
    .unwrap();
  sim.run_until_idle();

  match result_rx.try_recv() {
    Ok(Ok(replies)) => assert_eq!(
      replies,
      vec![expected_reply],
      "the accepted server learned its peer from the first datagram and replied over that session"
    ),
    other => panic!("the accept-from-unknown exchange did not complete: {other:?}"),
  }
  let (counters, _) = counters_rx.try_recv().expect("the server reported");
  assert_eq!(counters.opened, 1, "{counters:?}");
}
