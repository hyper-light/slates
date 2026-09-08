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
use rustls::pki_types::PrivateKeyDer;
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::SimRuntime;
use slates_rt::udp::UdpSocket;
use slates_transport::endpoint::Endpoint;
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
  let identity = self_signed(NAME);
  let pinned = identity.certificate();

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
        let mut server =
          Endpoint::server(socket, peer, &identity, FRAME_CAP).map_err(|e| format!("{e:?}"))?;
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
      let mut client = Endpoint::client(socket, peer, &pinned, NAME, FRAME_CAP).unwrap();
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

/// A request/reply exchange completes over a live session: the client sends a request, the server
/// transforms it and replies, the client receives exactly the reply — the RPC seam register/placement
/// will ride (§4.8 lookups route to the owner). Do X, expect Y.
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
  let identity = self_signed(NAME);
  let pinned = identity.certificate();

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
      let mut server = Endpoint::server(socket, peer, &identity, FRAME_CAP).unwrap();
      server.establish().await.unwrap();
      server
        .serve_once(|req| req.iter().map(|b| b.wrapping_add(1)).collect())
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
        let mut client =
          Endpoint::client(socket, peer, &pinned, NAME, FRAME_CAP).map_err(|e| format!("{e:?}"))?;
        client.establish().await.map_err(|e| format!("{e:?}"))?;
        client
          .request(STREAM_ID, &request)
          .await
          .map_err(|e| format!("{e:?}"))
      }
      .await;
      let _ = result_tx.send(outcome);
    })
    .unwrap();

  sim.run_until_idle();

  match result_rx.try_recv() {
    Ok(Ok(reply)) => assert_eq!(
      reply, expected_reply,
      "the reply arrived over the live session"
    ),
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

  let identity = self_signed(NAME);
  let pinned = identity.certificate();
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
      let mut server = Endpoint::server(socket, peer, &identity, FRAME_CAP).unwrap();
      server.establish().await.unwrap();
      for _ in 0..EXCHANGES {
        server
          .serve_once(|req| req.iter().map(|b| b.wrapping_add(7)).collect())
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
        let mut client =
          Endpoint::client(socket, peer, &pinned, NAME, FRAME_CAP).map_err(|e| format!("{e:?}"))?;
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
