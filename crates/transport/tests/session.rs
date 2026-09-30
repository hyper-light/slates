//! The session plane as a *live* connection over UDP (§4.10a §8): two `Endpoint`s complete the
//! `rustls::quic` TLS 1.3 handshake over the runtime's UDP socket, then the client sends a stream in
//! packets protected by the handshake's 1-RTT keys and the server reassembles it — the whole session
//! stack (handshake + packet protection + framing + stream reassembly) exercised end to end,
//! deterministically at N=1 on the simulation UDP fabric (no OS network). The session-plane analogue
//! of `plane.rs`. Test by use (R5).

// Test harness code: an unwrap or a panic here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::future::Future;
use std::sync::mpsc::channel;

use rustix::net::{Ipv4Addr, SocketAddrV4};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::SimRuntime;
use slates_rt::udp::UdpSocket;
use slates_transport::connection::Priority;
use slates_transport::demux::{Demux, DemuxCounters, DemuxId};
use slates_transport::endpoint::{Endpoint, EndpointError};
use slates_transport::handshake::Identity;
use slates_transport::params::TransportParameters;

const NAME: &str = "slates-node";
const STREAM_ID: u64 = 1;
/// Shape: a small packet budget — sixteen bytes of stream data per frame — so a message spans several
/// packets and the reassembly, credit and loss paths all run.
const FRAME_CAP: usize = slates_transport::session::STREAM_FRAME_HEADER_BYTES + 16;
/// Shape: the receive ceiling the test sessions' windows may auto-tune to — sixty-four initial windows,
/// room for the tuning path to run without any test holding more than a few kilobytes.
const RECEIVE_CEILING_WINDOWS: u64 = 64;

/// The connection shape every test session is built with: the frame cap, a ceiling of
/// [`RECEIVE_CEILING_WINDOWS`] initial windows, and the session plane's controller.
fn shape() -> slates_transport::connection::ConnectionShape {
  slates_transport::connection::ConnectionShape::for_frame_cap(
    FRAME_CAP,
    RECEIVE_CEILING_WINDOWS * slates_transport::connection::initial_receive_window(FRAME_CAP),
  )
}

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
    slates_rt::futures::sleep(1_000).await.unwrap();
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
          shape(),
        )
        .map_err(|e| format!("{e:?}"))?;
        server.establish().await.map_err(|e| format!("{e:?}"))?;
        let (id, _kind, received) = server.next_request().await.map_err(|e| format!("{e:?}"))?;
        server.reply(id, &[]).map_err(|e| format!("{e:?}"))?;
        server.settle().await.map_err(|e| format!("{e:?}"))?;
        Ok::<_, String>(received)
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
      let mut client =
        Endpoint::client(socket, peer, &client_identity, &server_cert, NAME, shape()).unwrap();
      client.establish().await.unwrap();
      client
        .request(STREAM_ID, Priority::Bulk, &content)
        .await
        .unwrap();
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

/// §4.10a (RFC 9000 §7.4, §18.2 in slates's dialect; the base of path MTU discovery, RFC 8899): each end
/// of an established session holds the transport parameters its peer declared in the authenticated
/// handshake — the dialect accepted and the largest datagram the peer reads — and they are exactly what
/// the peer presented. Do an exchange, expect each side's view of the other to equal the other's own.
#[test]
fn each_end_holds_the_parameters_its_peer_declared() {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];
  let server_identity = self_signed(NAME);
  let client_identity = self_signed(NAME);
  let server_cert = server_identity.certificate();
  let client_cert = client_identity.certificate();
  let (server_port_tx, server_port_rx) = channel();
  let (client_port_tx, client_port_rx) = channel();
  let (server_view_tx, server_view_rx) = channel();
  let (client_view_tx, client_view_rx) = channel();
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = server_port_tx.send(socket.local_addr().unwrap().port());
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, recv_port(client_port_rx).await);
      let mut server = Endpoint::server(
        socket,
        peer,
        &server_identity,
        std::slice::from_ref(&client_cert),
        shape(),
      )
      .unwrap();
      server.establish().await.unwrap();
      let (request, _kind, _) = server.next_request().await.unwrap();
      server.reply(request, &[]).unwrap();
      server.settle().await.unwrap();
      let _ = server_view_tx.send(server.peer_parameters());
    })
    .unwrap();
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = client_port_tx.send(socket.local_addr().unwrap().port());
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, recv_port(server_port_rx).await);
      let mut client =
        Endpoint::client(socket, peer, &client_identity, &server_cert, NAME, shape()).unwrap();
      client.establish().await.unwrap();
      client
        .request(STREAM_ID, Priority::Control, b"parameters")
        .await
        .unwrap();
      let _ = client_view_tx.send(client.peer_parameters());
    })
    .unwrap();
  sim.run_until_idle();
  let declared = Some(TransportParameters::local());
  assert_eq!(
    server_view_rx.try_recv(),
    Ok(declared),
    "the server holds what the client declared"
  );
  assert_eq!(
    client_view_rx.try_recv(),
    Ok(declared),
    "the client holds what the server declared"
  );
}

/// Shape: the path MTUs the discovery test moves between — a jumbo-frame LAN, then an Ethernet path.
const JUMBO_MTU: usize = 9_000;
/// Shape: an Ethernet path's MTU, the path shrinks to mid-session.
const ETHERNET_MTU: usize = 1_500;
/// Shape: each transfer's size — many packets at either MTU, so the search and the black hole both see
/// traffic.
const TRANSFER_BYTES: usize = 400_000;

/// A digest the server replies with: the request's length and a position-weighted sum of its bytes, so a
/// reordered, duplicated or truncated delivery answers differently.
fn digest(bytes: &[u8]) -> Vec<u8> {
  let sum = bytes.iter().enumerate().fold(0u64, |sum, (at, byte)| {
    sum.wrapping_add(u64::from(*byte).wrapping_mul(at as u64 + 1))
  });
  let mut out = (bytes.len() as u64).to_le_bytes().to_vec();
  out.extend_from_slice(&sum.to_le_bytes());
  out
}

/// §4.10a (RFC 8899 as RFC 9000 §14.3 applies it; `crate::pmtud`): a session finds how large a datagram its
/// path carries and sends at that size, and when the path shrinks under it — a black hole — falls back to
/// the floor, finds the new size, and loses no data. Do X (a transfer over a 9,000-byte path; shrink the
/// path to 1,500 bytes; another transfer), expect Y (after the first the confirmed size is within one
/// packet overhead of 9,000; after the second it is at most 1,500 and within that granularity of it; a
/// black hole was detected; both transfers arrive byte-exact). Non-vacuous: probes were lost as well as
/// acknowledged (the path really bounded the search).
#[test]
fn a_session_finds_its_paths_mtu_and_falls_back_when_the_path_shrinks() {
  let mut sim = SimRuntime::new(&config(), 7).unwrap();
  let id = sim.shard_ids()[0];
  slates_rt::sim::sim_udp_set_path(
    slates_rt::sim::SimPath::in_order(1_000_000, 0).with_mtu(JUMBO_MTU),
  );
  let server_identity = self_signed(NAME);
  let client_identity = self_signed(NAME);
  let server_cert = server_identity.certificate();
  let client_cert = client_identity.certificate();
  let (server_port_tx, server_port_rx) = channel();
  let (client_port_tx, client_port_rx) = channel();
  let (result_tx, result_rx) = channel();
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = server_port_tx.send(socket.local_addr().unwrap().port());
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, recv_port(client_port_rx).await);
      let mut server = Endpoint::server(
        socket,
        peer,
        &server_identity,
        std::slice::from_ref(&client_cert),
        shape(),
      )
      .unwrap();
      server.establish().await.unwrap();
      for _ in 0..2 {
        let (request, _kind, received) = server.next_request().await.unwrap();
        server.reply(request, &digest(&received)).unwrap();
      }
      server.settle().await.unwrap();
    })
    .unwrap();
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = client_port_tx.send(socket.local_addr().unwrap().port());
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, recv_port(server_port_rx).await);
      let mut client =
        Endpoint::client(socket, peer, &client_identity, &server_cert, NAME, shape()).unwrap();
      client.establish().await.unwrap();
      let first: Vec<u8> = (0..TRANSFER_BYTES)
        .map(|at| u8::try_from(at % 251).unwrap_or(0))
        .collect();
      let first_reply = client
        .request(STREAM_ID, Priority::Bulk, &first)
        .await
        .unwrap();
      let after_first = client.path_mtu();
      slates_rt::sim::sim_udp_set_path(
        slates_rt::sim::SimPath::in_order(1_000_000, 0).with_mtu(ETHERNET_MTU),
      );
      let second: Vec<u8> = (0..TRANSFER_BYTES)
        .map(|at| u8::try_from(at % 241).unwrap_or(0))
        .collect();
      let second_reply = client
        .request(STREAM_ID, Priority::Bulk, &second)
        .await
        .unwrap();
      let _ = result_tx.send((
        first_reply == digest(&first),
        second_reply == digest(&second),
        after_first,
        client.path_mtu(),
        client.path_mtu_stats(),
      ));
    })
    .unwrap();
  sim.run_until_idle();
  let (first_exact, second_exact, after_first, after_second, stats) = result_rx.try_recv().unwrap();
  assert!(
    first_exact && second_exact,
    "both transfers arrived byte-exact"
  );
  let after_first = after_first.unwrap();
  assert!(
    after_first <= JUMBO_MTU
      && JUMBO_MTU - after_first <= slates_transport::pmtud::SEARCH_GRANULARITY,
    "the jumbo path was found: {after_first}"
  );
  let after_second = after_second.unwrap();
  let stats = stats.unwrap();
  assert!(
    stats.black_holes >= 1,
    "the shrink was detected as a black hole: {stats:?}"
  );
  assert!(
    after_second <= ETHERNET_MTU
      && ETHERNET_MTU - after_second <= slates_transport::pmtud::SEARCH_GRANULARITY,
    "the shrunken path was found: {after_second} ({stats:?})"
  );
  assert!(stats.probes_lost > 0 && stats.probes_acked > 0, "{stats:?}");
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
  // A modelled half-millisecond each way, so the round trip the estimator must report is a known one
  // millisecond on the virtual clock rather than "some positive number" — an oracle, not a smoke test.
  slates_rt::sim::sim_udp_set_path(slates_rt::sim::SimPath::in_order(HALF_MS_NS, 0));
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
        shape(),
      )
      .unwrap();
      server.establish().await.unwrap();
      server
        .serve_once_async(|_, req| async move {
          // Yield before producing the reply, so the pending-await path of `serve_once_async` is exercised
          // (a forwarded verb's reply comes from an `xshard` await, not synchronously).
          slates_rt::futures::sleep(1_000).await.unwrap();
          req.iter().map(|b| b.wrapping_add(1)).collect()
        })
        .await
        .unwrap();
      // The reply is in flight when `serve_once_async` returns; a server that stops settles it first.
      server.settle().await.unwrap();
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
          Endpoint::client(socket, peer, &client_identity, &server_cert, NAME, shape())
            .map_err(|e| format!("{e:?}"))?;
        client.establish().await.map_err(|e| format!("{e:?}"))?;
        let reply = client
          .request(STREAM_ID, Priority::Control, &request)
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
      // The path is one millisecond round trip on the virtual clock; the server's handler yields a
      // microsecond before replying, which the acknowledgement riding the reply carries into the sample.
      assert!(
        (2 * HALF_MS_NS..=2 * HALF_MS_NS + HANDLER_YIELD_NS * 2).contains(&smoothed_rtt),
        "the client estimated the path's round trip on the runtime clock: {smoothed_rtt} ns"
      );
    }
    other => panic!("the request/reply did not complete: {other:?}"),
  }
}

/// Shape: half a millisecond one way — a modelled path whose round trip is a known one millisecond.
const HALF_MS_NS: u64 = 500_000;
/// Shape: the microsecond the served handler yields before replying, the processing delay the sample
/// legitimately carries.
const HANDLER_YIELD_NS: u64 = 1_000;
/// Shape: the one-way delay of a modelled inter-region path, 80 ms (see `docs/wip/wan-timeout.md`).
const WAN_ONE_WAY_NS: u64 = 80_000_000;
/// Shape: that path's jitter, ± 20 ms.
const WAN_JITTER_NS: u64 = 20_000_000;
/// Shape: the exchanges the client makes over the far path — enough for the estimate to settle past its
/// seeded first sample, few enough to keep the run short.
const WAN_EXCHANGES: usize = 6;

/// AC (§4.10a §8; RFC 9002 §5.1): an endpoint's round-trip estimate measures the **path** on the clock its
/// timers run on — the runtime's, which the simulation makes virtual — so over a modelled 80 ms ± 20 ms path
/// the smoothed RTT settles inside the path's 120–200 ms round-trip band and the probe timeout it arms sits
/// above it. Before this the samples came from the wall clock while the timeouts ran on the runtime clock:
/// under simulation a 160 ms path measured as microseconds, arming a millisecond probe timeout that
/// retransmitted every exchange several times before its real reply could arrive
/// (`docs/bugs/2026-09-14-transport-rtt-sampled-on-the-wall-clock.md`).
#[test]
fn an_endpoint_measures_the_paths_round_trip_on_the_runtime_clock() {
  let mut sim = SimRuntime::new(&config(), 3).unwrap();
  slates_rt::sim::sim_udp_set_path(slates_rt::sim::SimPath::in_order(
    WAN_ONE_WAY_NS,
    WAN_JITTER_NS,
  ));
  let id = sim.shard_ids()[0];
  let server_identity = self_signed(NAME);
  let client_identity = self_signed(NAME);
  let server_cert = server_identity.certificate();
  let client_cert = client_identity.certificate();
  let (server_port_tx, server_port_rx) = channel();
  let (client_port_tx, client_port_rx) = channel();
  let (result_tx, result_rx) = channel();

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
        shape(),
      )
      .unwrap();
      server.establish().await.unwrap();
      for _ in 0..WAN_EXCHANGES {
        server.serve_once(|_, req| req).await.unwrap();
      }
      server.settle().await.unwrap();
    })
    .unwrap();

  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = client_port_tx.send(socket.local_addr().unwrap().port());
      let server_port = recv_port(server_port_rx).await;
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
      let outcome = async {
        let mut client =
          Endpoint::client(socket, peer, &client_identity, &server_cert, NAME, shape())
            .map_err(|e| format!("{e:?}"))?;
        client.establish().await.map_err(|e| format!("{e:?}"))?;
        let mut exchange_times = Vec::with_capacity(WAN_EXCHANGES);
        for _ in 0..WAN_EXCHANGES {
          let started = slates_rt::futures::now_ns();
          client
            .request(STREAM_ID, Priority::Control, b"far")
            .await
            .map_err(|e| format!("{e:?}"))?;
          exchange_times.push(slates_rt::futures::now_ns() - started);
        }
        Ok::<_, String>((client.smoothed_rtt(), client.pto(), exchange_times))
      }
      .await;
      let _ = result_tx.send(outcome);
    })
    .unwrap();

  sim.run_until_idle();

  let band = 2 * (WAN_ONE_WAY_NS - WAN_JITTER_NS)..=2 * (WAN_ONE_WAY_NS + WAN_JITTER_NS);
  match result_rx.try_recv() {
    Ok(Ok((smoothed_rtt, pto, exchange_times))) => {
      assert!(
        exchange_times.iter().all(|elapsed| band.contains(elapsed)),
        "every exchange took its round trip on the virtual clock: {exchange_times:?} ns"
      );
      assert!(
        band.contains(&smoothed_rtt),
        "the smoothed RTT settled inside the path's round-trip band: {smoothed_rtt} ns"
      );
      assert!(
        pto > smoothed_rtt,
        "the probe timeout sits above the estimate: pto {pto} ns, smoothed {smoothed_rtt} ns"
      );
    }
    other => panic!("the exchanges over the far path did not complete: {other:?}"),
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
        shape(),
      )
      .unwrap();
      server.establish().await.unwrap();
      for _ in 0..EXCHANGES {
        server
          .serve_once(|_, req| req.iter().map(|b| b.wrapping_add(7)).collect())
          .await
          .unwrap();
      }
      server.settle().await.unwrap();
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
          Endpoint::client(socket, peer, &client_identity, &server_cert, NAME, shape())
            .map_err(|e| format!("{e:?}"))?;
        client.establish().await.map_err(|e| format!("{e:?}"))?;
        let mut cursors = vec![client.tx_packet_number()];
        for exchange in 0..EXCHANGES {
          let request: Vec<u8> = (0..40u8)
            .map(|b| b.wrapping_add(u8::try_from(exchange).unwrap_or(0)))
            .collect();
          let reply = client
            .request(exchange + 1, Priority::Control, &request)
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

/// Shape: how long (simulated nanoseconds) the abandoning client waits before its next request — long enough
/// that the server has received the abandoned request and put its reply on the wire (a few timer ticks on
/// the loopback fabric), short next to the initial probe timeout so the server's reply is still in flight,
/// unacknowledged, when the next request arrives.
const SETTLE_NS: u64 = 2_000_000;
/// Shape: the bound on each end's wait for an exchange that a defect could otherwise leave waiting forever
/// (the simulation advances time to every re-drive timer, so an unbounded wait never goes idle). Several
/// times the initial probe timeout, so a served exchange completes and a dropped one fails the test.
const EXCHANGE_BOUND_NS: u64 = 4_000_000_000;

/// Runs `future` to completion or until `within_ns` of simulated time pass — `None` on the deadline. The
/// bound a test needs on an exchange that a defect could leave waiting forever.
async fn within<F: Future>(within_ns: u64, future: F) -> Option<F::Output> {
  let mut future = std::pin::pin!(future);
  let mut deadline = std::pin::pin!(slates_rt::futures::sleep(within_ns));
  std::future::poll_fn(|cx| {
    if let std::task::Poll::Ready(output) = future.as_mut().poll(cx) {
      return std::task::Poll::Ready(Some(output));
    }
    if deadline.as_mut().poll(cx).map(Result::unwrap).is_ready() {
      return std::task::Poll::Ready(None);
    }
    std::task::Poll::Pending
  })
  .await
}

/// Polls `future` exactly once, under the running task's own context, and drops it — the shape of a caller
/// whose deadline fires after its request was flushed but before it read the reply (the fleet probe racing
/// `request` against a deadline). Returns whether the single poll already completed it. Polled with the
/// task's real waker (never a detached one): a readiness interest the poll registers must stay wakeable, or
/// the runtime holds the registration as pending work and the simulation never goes idle.
async fn poll_once_then_abandon<F: Future>(future: F) -> bool {
  let mut future = std::pin::pin!(future);
  std::future::poll_fn(|cx| std::task::Poll::Ready(future.as_mut().poll(cx).is_ready())).await
}

/// AC (RFC 9000 §2.1 — a stream id is never reused within a connection; §4.10a §8): a request that reaches
/// the peer while the peer still holds the **abandoned** previous exchange's reply unacknowledged is
/// **served**, and the late reply to the abandoned exchange is never read as the new one's. The client
/// flushes request A (one poll), gives it up, lets the server receive A and put its reply on the wire, then
/// sends request B and must receive B's own transform within a bound. On a stream id reused per request
/// kind, B rode A's id: at the server B's offset-0 frame was a duplicate of A's completed stream (deduped,
/// never served) while A's reply awaited an acknowledgement that never came, and at the client A's late
/// reply could be read as B's — the collision that starved a live peer's probes
/// (`docs/bugs/2026-09-13-reused-stream-id-collides-behind-an-unacked-reply.md`).
#[test]
fn a_request_behind_an_abandoned_exchanges_unacknowledged_reply_is_served() {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];
  let server_identity = self_signed(NAME);
  let client_identity = self_signed(NAME);
  let server_cert = server_identity.certificate();
  let client_cert = client_identity.certificate();
  let (server_port_tx, server_port_rx) = channel();
  let (client_port_tx, client_port_rx) = channel();
  let (served_tx, served_rx) = channel();
  let (result_tx, result_rx) = channel();

  // The server: serve two requests in turn, each bounded, reporting what it served — so a request the
  // transport drops is visible as "never served" rather than as a hang.
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
        shape(),
      )
      .unwrap();
      server.establish().await.unwrap();
      // Three serves: the warm-up exchange (which also takes the server past its handshake confirmation,
      // where a client's first 1-RTT datagram is dropped by design and left to the client's retransmit),
      // then A, then B.
      for _ in 0..3 {
        let served_tx = served_tx.clone();
        let outcome = within(
          EXCHANGE_BOUND_NS,
          server.serve_once(move |_, req| {
            let _ = served_tx.send(req.clone());
            req.iter().map(|b| b.wrapping_add(7)).collect()
          }),
        )
        .await;
        if outcome.is_none() {
          break;
        }
      }
      let _ = within(EXCHANGE_BOUND_NS, server.settle()).await;
    })
    .unwrap();

  // The client: a completed warm-up exchange, then A flushed and abandoned, a settle, then B awaited
  // within the bound.
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = client_port_tx.send(socket.local_addr().unwrap().port());
      let server_port = recv_port(server_port_rx).await;
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
      let outcome = async {
        let mut client =
          Endpoint::client(socket, peer, &client_identity, &server_cert, NAME, shape())
            .map_err(|e| format!("{e:?}"))?;
        client.establish().await.map_err(|e| format!("{e:?}"))?;
        let warm = within(
          EXCHANGE_BOUND_NS,
          client.request(STREAM_ID, Priority::Control, b"warm-up"),
        )
        .await
        .ok_or_else(|| "the warm-up exchange never completed".to_owned())?
        .map_err(|e| format!("{e:?}"))?;
        if warm
          != b"warm-up"
            .iter()
            .map(|b| b.wrapping_add(7))
            .collect::<Vec<u8>>()
        {
          return Err("the warm-up reply was wrong".to_owned());
        }
        let completed_early =
          poll_once_then_abandon(client.request(STREAM_ID, Priority::Control, b"request A")).await;
        if let Some(abandoned) = client.last_exchange() {
          client.abandon(abandoned);
        }
        slates_rt::futures::sleep(SETTLE_NS).await.unwrap();
        let reply_b = within(
          EXCHANGE_BOUND_NS,
          client.request(STREAM_ID, Priority::Control, b"request B"),
        )
        .await
        .ok_or_else(|| "request B was never answered within the bound".to_owned())?
        .map_err(|e| format!("{e:?}"))?;
        Ok::<_, String>((completed_early, reply_b))
      }
      .await;
      let _ = result_tx.send(outcome);
    })
    .unwrap();

  sim.run_until_idle();

  let served: Vec<Vec<u8>> = served_rx.try_iter().collect();
  match result_rx.try_recv() {
    Ok(Ok((completed_early, reply_b))) => {
      assert!(
        !completed_early,
        "one poll flushes the request but cannot complete it — the reply needs a round trip"
      );
      assert!(
        served.iter().any(|r| r == b"request A"),
        "the server received the abandoned request A (the collision is set up): served {served:?}"
      );
      assert!(
        served.iter().any(|r| r == b"request B"),
        "the server served request B behind A's unacknowledged reply: served {served:?}"
      );
      assert_eq!(
        reply_b,
        b"request B"
          .iter()
          .map(|b| b.wrapping_add(7))
          .collect::<Vec<u8>>(),
        "B's reply is B's own transform, not A's late reply"
      );
    }
    other => panic!("the exchange behind an abandoned reply failed: {other:?}; served {served:?}"),
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
      Err(_) => slates_rt::futures::sleep(1_000).await.unwrap(),
    }
  }
  out
}

/// What a serve task reports when its session ends: how many requests it served, how, and how many
/// handshake fragments it sent (a flight larger than a datagram sends more than one).
#[derive(Debug)]
struct Served {
  requests: u32,
  ended: String,
  fragments_sent: u64,
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
        // The last reply is in flight when `serve_once` returns; settle it before the session drops.
        break match session.settle().await {
          Ok(()) => "done".to_owned(),
          Err(e) => format!("settle: {e:?}"),
        };
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
  let _ = report.send(Served {
    requests,
    ended,
    fragments_sent: session.fragments_sent(),
  });
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
  let mut client = Endpoint::client(socket, peer, &identity, &server_cert, NAME, shape())
    .map_err(|e| format!("{e:?}"))?;
  client.establish().await.map_err(|e| format!("{e:?}"))?;
  let mut replies = Vec::new();
  for request in &requests {
    replies.push(
      client
        .request(STREAM_ID, Priority::Control, request)
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
  peer_capacity: usize,
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
  let identity = slates_rt::registry::with_current(|ctx| ctx.keep(plan.identity))
    .unwrap()
    .unwrap();
  let demux = Demux::start(socket, identity, plan.allowed, shape(), plan.peer_capacity).unwrap();
  let port = demux.with(Demux::local_addr).unwrap().unwrap().port();
  for tx in plan.port_txs {
    let _ = tx.send(port);
  }
  let run = slates_rt::futures::spawn(async move {
    let _ = demux.run().await;
  })
  .unwrap();
  let _ = slates_rt::futures::detach(run);
  for max_requests in plan.sessions {
    let session = demux.accept().await.unwrap();
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
  let _ = plan.counters_tx.send(
    demux
      .with(|demux| (demux.counters(), demux.sessions()))
      .unwrap(),
  );
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
        peer_capacity: 2,
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

/// Shape: a roster large enough that the certificate-authority hints rustls would list made the server's
/// flight overflow the receiver's datagram (64 admitted peers: 3,024 bytes against 2,048, measured
/// 2026-09-14); a fleet of this size and larger must handshake like a fleet of two.
const LARGE_ROSTER: usize = 64;

/// §4.8 (every node holds the whole roster) with §4.9 (a fleet message rides one datagram), by use: a
/// server admitting a **large roster** still completes a dialer's handshake and serves its request. Do:
/// a server that admits 64 peers; one of them dials, handshakes and asks once. Expect: the reply, one
/// session opened, none refused. Non-vacuous: before the roster verifier dropped the hints, this dial
/// faulted at the dialer with `Tls(InvalidMessage(HandshakePayloadTooLarge))` — the server's first flight
/// truncated at the receiver — which is how the KIND lane's 38-peer node could never be dialed.
#[test]
fn a_server_admitting_a_large_roster_still_completes_a_dialers_handshake() {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];
  let server_identity = self_signed(NAME);
  let server_cert = server_identity.certificate();
  let dialer = self_signed(NAME);
  let mut allowed: Vec<_> = (1..LARGE_ROSTER)
    .map(|_| self_signed(NAME).certificate())
    .collect();
  allowed.push(dialer.certificate());
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
        peer_capacity: 1,
        sessions: vec![1],
        add: 5,
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
      let outcome = dial_and_request(dialer, server_cert, port_rx, vec![b"roster".to_vec()]).await;
      let _ = result_tx.send(outcome);
      let _ = done_tx.send(());
    })
    .unwrap();
  sim.run_until_idle();

  let outcome = result_rx.try_recv().expect("the dialer finished");
  let expected: Vec<u8> = b"roster".iter().map(|b| b.wrapping_add(5)).collect();
  assert_eq!(
    outcome.as_deref(),
    Ok(&[expected][..]),
    "the dialer handshook with a server admitting {LARGE_ROSTER} peers and was served"
  );
  let (counters, _live) = counters_rx.try_recv().expect("the server reported");
  assert_eq!(counters.opened, 1, "one session: {counters:?}");
  assert_eq!(counters.sessions_refused, 0, "{counters:?}");
}

/// Shape: how many subject-alternative names the wide certificate carries — enough that the server's
/// Certificate message, and so its flight, spans more than one path-floor datagram (a name is a
/// dozen-odd bytes; 120 of them add about two kilobytes to a 694-byte flight).
const WIDE_CERTIFICATE_NAMES: usize = 120;

/// A self-signed identity whose certificate is wide (many subject-alternative names), so a server
/// presenting it sends a handshake flight larger than one datagram — what a real deployment's chain of a
/// leaf and an intermediate looks like on the wire.
fn wide_identity(name: &str) -> Identity {
  let key = rcgen::KeyPair::generate().unwrap();
  let names: Vec<String> = (0..WIDE_CERTIFICATE_NAMES)
    .map(|i| format!("{name}-{i}.example"))
    .chain(std::iter::once(name.to_owned()))
    .collect();
  let cert = rcgen::CertificateParams::new(names)
    .unwrap()
    .self_signed(&key)
    .unwrap();
  Identity::from_der(
    cert.der().clone(),
    PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
  )
}

/// §4.10a (RFC 9000 §19.6 CRYPTO frames), by use: a server whose handshake flight is **larger than one
/// datagram** — a wide certificate here, a chain in a deployment — still completes a dialer's handshake:
/// the flight crosses as fragments the dialer reassembles. Do: a server with a wide certificate; one
/// client dials, handshakes and asks once. Expect: the reply; the server sent more than one fragment
/// (non-vacuity — the flight really spanned datagrams); one session opened, none refused. Before
/// fragmentation such a flight was refused typed at the sender (`FlightTooLarge`, 2026-09-14) and,
/// before that, truncated at the receiver.
#[test]
fn a_server_flight_larger_than_a_datagram_crosses_as_fragments() {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];
  let server_identity = wide_identity(NAME);
  let server_cert = server_identity.certificate();
  let dialer = self_signed(NAME);
  let allowed = vec![dialer.certificate()];
  let (port_tx, port_rx) = channel();
  let (served_tx, served_rx) = channel();
  let (done_tx, done_rx) = channel();
  let (counters_tx, counters_rx) = channel();
  let (result_tx, result_rx) = channel();
  sim
    .spawn_on(
      id,
      serve_shared_socket(ServerPlan {
        identity: server_identity,
        allowed,
        peer_capacity: 1,
        sessions: vec![1],
        add: 2,
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
      let outcome = dial_and_request(dialer, server_cert, port_rx, vec![b"wide".to_vec()]).await;
      let _ = result_tx.send(outcome);
      let _ = done_tx.send(());
    })
    .unwrap();
  sim.run_until_idle();

  let outcome = result_rx.try_recv().expect("the dialer finished");
  let expected: Vec<u8> = b"wide".iter().map(|b| b.wrapping_add(2)).collect();
  assert_eq!(
    outcome.as_deref(),
    Ok(&[expected][..]),
    "the dialer handshook with a server whose flight spans datagrams and was served"
  );
  let served: Vec<Served> = served_rx.try_iter().collect();
  let fragments: u64 = served.iter().map(|s| s.fragments_sent).sum();
  eprintln!("the wide server's handshake crossed as {fragments} fragments");
  assert!(
    fragments >= 2,
    "the server's flight crossed as more than one fragment (non-vacuity): {served:?}"
  );
  let (counters, _live) = counters_rx.try_recv().expect("the server reported");
  assert_eq!(counters.opened, 1, "one session: {counters:?}");
  assert_eq!(counters.sessions_refused, 0, "{counters:?}");
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
        peer_capacity: 1,
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
        let mut endpoint = Endpoint::client(socket, peer, &client, &server_cert, NAME, shape())
          .map_err(|e| format!("{e:?}"))?;
        endpoint.establish().await.map_err(|e| format!("{e:?}"))?;
        let before = endpoint
          .request(STREAM_ID, Priority::Control, b"before the stray")
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
        slates_rt::futures::sleep(1_000_000).await.unwrap();
        let after = endpoint
          .request(STREAM_ID, Priority::Control, b"after the stray")
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
        peer_capacity: 2,
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

/// A stage of a pool history the server reports as it passes it: the demultiplexer's counters and how many
/// sessions it holds at that moment.
#[derive(Debug)]
struct PoolStage {
  name: &'static str,
  counters: DemuxCounters,
  held: usize,
  last_setup_refusal: Option<String>,
}

/// Reports the pool's state at `name`.
fn report_stage(demux: &Demux, name: &'static str, stages: &std::sync::mpsc::Sender<PoolStage>) {
  let _ = stages.send(PoolStage {
    name,
    counters: demux.counters(),
    held: demux.sessions(),
    last_setup_refusal: demux.last_setup_refusal(),
  });
}

/// Starts a demultiplexer of `capacity` peer reservations on a fresh socket, its receive loop as its own task, and
/// publishes its port to every dialer that will dial it.
fn start_pool(
  identity: Identity,
  allowed: Vec<CertificateDer<'static>>,
  capacity: usize,
  port_txs: Vec<std::sync::mpsc::Sender<u16>>,
) -> DemuxId {
  let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
  let identity = slates_rt::registry::with_current(|ctx| ctx.keep(identity))
    .unwrap()
    .unwrap();
  let demux = Demux::start(socket, identity, allowed, shape(), capacity).unwrap();
  let port = demux.with(Demux::local_addr).unwrap().unwrap().port();
  for tx in port_txs {
    let _ = tx.send(port);
  }
  let run = slates_rt::futures::spawn(async move {
    let _ = demux.run().await;
  })
  .unwrap();
  let _ = slates_rt::futures::detach(run);
  demux
}

/// Shape: two peer reservations for the setup-refusal history; no endpoint should be allocated.
const SMALL_POOL: usize = 2;
/// Shape: peer capacity exceeds the burst's three distinct identities.
const ROOMY_POOL: usize = 8;
/// Shape: three distinct dialers share one accepting socket.
const BURST_DIALS: usize = 3;

/// AC-7.7/§4.10a: distinct identities below the peer/handshake bounds all establish and are served.
/// Counters prove that no capacity or setup refusal occurred and every endpoint was reclaimed.
#[test]
fn a_burst_within_the_pool_is_admitted_with_no_capacity_refusal() {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];
  let server_identity = self_signed(NAME);
  let server_cert = server_identity.certificate();
  let dialers: Vec<Identity> = (0..BURST_DIALS).map(|_| self_signed(NAME)).collect();
  let allowed: Vec<CertificateDer<'static>> = dialers.iter().map(Identity::certificate).collect();
  let mut port_rxs = Vec::new();
  let mut port_txs = Vec::new();
  for _ in 0..BURST_DIALS {
    let (tx, rx) = channel();
    port_txs.push(tx);
    port_rxs.push(rx);
  }
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
        peer_capacity: ROOMY_POOL,
        sessions: vec![1; BURST_DIALS],
        add: 7,
        port_txs,
        served_tx,
        done_rx,
        clients: BURST_DIALS,
        counters_tx,
      }),
    )
    .unwrap();
  for (index, (identity, port_rx)) in dialers.into_iter().zip(port_rxs).enumerate() {
    let (result_tx, done_tx, server_cert) =
      (result_tx.clone(), done_tx.clone(), server_cert.clone());
    sim
      .spawn_on(id, async move {
        let request = format!("dial {index}").into_bytes();
        let outcome = dial_and_request(identity, server_cert, port_rx, vec![request.clone()]).await;
        let _ = result_tx.send((request, outcome));
        let _ = done_tx.send(());
      })
      .unwrap();
  }
  sim.run_until_idle();

  for _ in 0..BURST_DIALS {
    let (request, outcome) = result_rx.try_recv().expect("each dial finished");
    let expected: Vec<u8> = request.iter().map(|b| b.wrapping_add(7)).collect();
    assert_eq!(
      outcome.as_deref(),
      Ok(&[expected][..]),
      "each dial of the burst was admitted and served"
    );
  }
  let (counters, live) = counters_rx.try_recv().expect("the server reported");
  assert_eq!(
    counters.sessions_refused, 0,
    "a pool with room refuses nothing for capacity: {counters:?}"
  );
  assert_eq!(counters.setup_refused, 0, "{counters:?}");
  assert_eq!(
    counters.opened,
    u64::try_from(BURST_DIALS).unwrap(),
    "{counters:?}"
  );
  assert_eq!(
    counters.high_water,
    u64::try_from(BURST_DIALS).unwrap(),
    "the burst's size is the pool's high-water mark: {counters:?}"
  );
  assert_eq!(live, 0, "every slot returned as its serve task ended");
}

/// Shape: a few zero bytes — not DER at all — so the roster's trust store refuses the entry the moment a
/// session's verifier is built from it.
const NOT_A_CERTIFICATE_BYTES: usize = 4;

/// The refusal counter's other category, by use: a roster the demultiplexer cannot build a TLS server
/// state from (one entry that is not a certificate) refuses every dial as a **setup** fault, never as
/// capacity — the slot it took is given back (the pool stays empty, nothing opened), `setup_refused`
/// moves and `sessions_refused` does not. Non-vacuous: counted as one, a saturation proof would pass on a
/// broken roster without exhausting anything (the sibling defect the re-dial record named).
#[test]
fn a_roster_the_server_cannot_build_from_is_a_setup_refusal_not_capacity() {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];
  let server_identity = self_signed(NAME);
  let server_cert = server_identity.certificate();
  let dialer = self_signed(NAME);
  let broken: Vec<CertificateDer<'static>> =
    vec![CertificateDer::from(vec![0u8; NOT_A_CERTIFICATE_BYTES])];
  let (stages_tx, stages_rx) = channel();
  let (result_tx, result_rx) = channel();
  let (dialer_done_tx, dialer_done_rx) = channel::<()>();
  let (port_tx, port_rx) = channel();
  sim
    .spawn_on(id, async move {
      let task = slates_rt::futures::spawn(async move {
        let demux = start_pool(server_identity, broken, SMALL_POOL, vec![port_tx]);
        recv_signal(dialer_done_rx).await;
        demux
          .with(|demux| report_stage(demux, "final", &stages_tx))
          .unwrap();
      })
      .unwrap();
      let _ = slates_rt::futures::detach(task);
    })
    .unwrap();
  sim
    .spawn_on(id, async move {
      let outcome = dial_and_request(dialer, server_cert, port_rx, vec![b"never".to_vec()]).await;
      let _ = result_tx.send(outcome);
      let _ = dialer_done_tx.send(());
    })
    .unwrap();
  sim.run_until_idle();

  let stages: Vec<PoolStage> = stages_rx.try_iter().collect();
  let final_stage = stages
    .iter()
    .find(|s| s.name == "final")
    .unwrap_or_else(|| panic!("the server reported: {stages:?}"));
  assert!(
    final_stage.counters.setup_refused >= 1,
    "every dial was refused as a setup fault: {final_stage:?}"
  );
  assert_eq!(
    final_stage.counters.sessions_refused, 0,
    "none was refused for capacity — the pool had room: {final_stage:?}"
  );
  assert_eq!(
    final_stage.counters.opened, 0,
    "nothing opened: {final_stage:?}"
  );
  assert_eq!(
    final_stage.held, 0,
    "the slot each attempt took was given back: {final_stage:?}"
  );
  assert_eq!(final_stage.counters.high_water, 0, "{final_stage:?}");
  assert!(
    final_stage
      .last_setup_refusal
      .as_deref()
      .is_some_and(|error| error.contains("Setup")),
    "the refusal's error is kept for diagnosis and names the roster's setup, not capacity: \
     {final_stage:?}"
  );
  let outcome = result_rx.try_recv().expect("the dialer finished");
  assert_eq!(
    outcome.as_deref(),
    Err(&"NotReady".to_owned()),
    "the dial never established against a server that could not build its session"
  );
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
        peer_capacity: 1,
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

/// Waits (yielding on the runtime) for a unit signal on a just-sent-once channel.
async fn recv_signal(rx: std::sync::mpsc::Receiver<()>) {
  loop {
    if rx.try_recv().is_ok() {
      return;
    }
    slates_rt::futures::sleep(1_000).await.unwrap();
  }
}

/// Discards every datagram queued on `socket` — the test's stand-in for a flight lost on the wire (the
/// fabric never drops, so the loss is played by the receiver throwing the flight away before it listens).
/// Returns how many it discarded, the non-vacuity count: the dialer's whole first budget must have
/// arrived here and been thrown away for the scenario to be the one it claims.
async fn discard_queued(socket: &UdpSocket) -> usize {
  let mut buf = [0u8; 2048];
  let mut discarded = 0;
  while within(1_000, socket.recv_from(&mut buf)).await.is_some() {
    discarded += 1;
  }
  discarded
}

/// AC (§4.8 formation; RFC 9002 §6.2 the probe timeout retransmits the pending flight): a dialer whose
/// first `establish` ran out its retransmit budget against a peer that **lost every flight** (not listening
/// yet: the fleet's boot-ordering shape, played here by the peer discarding everything queued before it
/// starts its handshake) completes the handshake on a later `establish` call on the SAME socket once the
/// peer listens — the pending flight is retransmitted across the caller's periods, not only within one
/// call. Do: dial, spend the budget (typed `NotReady`), have the peer discard the queued flights and begin
/// its handshake, call `establish` again. Expect: the discard count is at least one (the first budget's
/// flights really were lost), the second call establishes, and a stream flows. Before the fix the second
/// call had nothing to send — the flight was a local of the first call — so a peer starved past one budget
/// was never reachable on that socket again, and the N-node mesh's formation stalled forever under load
/// (`docs/bugs/2026-09-14-handshake-retry-forgets-its-flight.md`).
#[test]
fn a_dialer_that_outwaited_an_absent_peer_completes_the_handshake_once_the_peer_listens() {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];

  let content: Vec<u8> = (0..200u16)
    .map(|i| u8::try_from(i % 241).unwrap_or(0))
    .collect();
  let expected = content.clone();
  let server_identity = self_signed(NAME);
  let client_identity = self_signed(NAME);
  let server_cert = server_identity.certificate();
  let client_cert = client_identity.certificate();

  let (server_port_tx, server_port_rx) = channel();
  let (client_port_tx, client_port_rx) = channel();
  let (outwaited_tx, outwaited_rx) = channel();
  let (first_tx, first_rx) = channel();
  let (discarded_tx, discarded_rx) = channel();
  let (result_tx, result_rx) = channel();
  let result_tx_server = result_tx.clone();

  // The dialer: spend a whole budget against a peer that is not listening, report the typed outcome,
  // then — the peer now listening — establish again on the same socket and send the stream.
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = client_port_tx.send(socket.local_addr().unwrap().port());
      let server_port = recv_port(server_port_rx).await;
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
      let mut client =
        Endpoint::client(socket, peer, &client_identity, &server_cert, NAME, shape()).unwrap();
      let first = client.establish().await;
      let _ = first_tx.send(matches!(first, Err(EndpointError::NotReady)));
      let _ = outwaited_tx.send(());
      let outcome = async {
        client.establish().await.map_err(|e| format!("{e:?}"))?;
        client
          .request(STREAM_ID, Priority::Bulk, &content)
          .await
          .map(|_| ())
          .map_err(|e| format!("{e:?}"))
      }
      .await;
      if let Err(e) = outcome {
        let _ = result_tx.send(Err(e));
      }
    })
    .unwrap();

  // The peer: bound from the start (so the dialer has an address) but not listening — it discards
  // everything that arrived during the dialer's first budget, then handshakes and receives the stream.
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = server_port_tx.send(socket.local_addr().unwrap().port());
      let client_port = recv_port(client_port_rx).await;
      recv_signal(outwaited_rx).await;
      let _ = discarded_tx.send(discard_queued(&socket).await);
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, client_port);
      let outcome = async {
        let mut server = Endpoint::server(
          socket,
          peer,
          &server_identity,
          std::slice::from_ref(&client_cert),
          shape(),
        )
        .map_err(|e| format!("{e:?}"))?;
        server.establish().await.map_err(|e| format!("{e:?}"))?;
        let (id, _kind, received) = server.next_request().await.map_err(|e| format!("{e:?}"))?;
        server.reply(id, &[]).map_err(|e| format!("{e:?}"))?;
        server.settle().await.map_err(|e| format!("{e:?}"))?;
        Ok::<_, String>(received)
      }
      .await;
      let _ = result_tx_server.send(outcome);
    })
    .unwrap();

  sim.run_until_idle();

  assert_eq!(
    first_rx.try_recv(),
    Ok(true),
    "the first establish against a peer that is not listening ends typed NotReady after its budget"
  );
  let discarded = discarded_rx.try_recv().unwrap_or(0);
  assert!(
    discarded >= 1,
    "the first budget's flights reached the peer and were thrown away ({discarded} discarded)"
  );
  match result_rx.try_recv() {
    Ok(Ok(received)) => assert_eq!(
      received, expected,
      "the stream flowed over the session the second establish completed"
    ),
    other => panic!(
      "the second establish on the same socket did not complete (peer discarded {discarded} flights): {other:?}"
    ),
  }
}

/// AC-8.18 / §4.13: two certificates under one trusted issuer remain distinct identities.
/// A server presenting the other valid certificate cannot complete the client's pinned session.
#[test]
fn an_issued_certificate_cannot_impersonate_another_leaf_under_the_same_authority() {
  let issuer_key = rcgen::KeyPair::generate().unwrap();
  let mut params = rcgen::CertificateParams::new(vec![NAME.to_owned()]).unwrap();
  params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
  let issuer = params.self_signed(&issuer_key).unwrap();
  let mint = || {
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec![NAME.to_owned()])
      .unwrap()
      .signed_by(&key, &issuer, &issuer_key)
      .unwrap();
    Identity::from_der(
      cert.der().clone(),
      PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
    )
  };
  let expected = mint().certificate();
  let server_identity = mint();
  let client_identity = mint().with_authorities(vec![issuer.der().clone()]);
  let authority = issuer.der().clone();
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let shard = sim.shard_ids()[0];
  let (server_port_tx, server_port_rx) = channel();
  let (client_port_tx, client_port_rx) = channel();
  let (result_tx, result_rx) = channel();
  sim
    .spawn_on(shard, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      server_port_tx
        .send(socket.local_addr().unwrap().port())
        .unwrap();
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, recv_port(client_port_rx).await);
      let mut endpoint =
        Endpoint::server(socket, peer, &server_identity, &[authority], shape()).unwrap();
      let _ = endpoint.establish().await;
    })
    .unwrap();
  sim
    .spawn_on(shard, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      client_port_tx
        .send(socket.local_addr().unwrap().port())
        .unwrap();
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, recv_port(server_port_rx).await);
      let mut endpoint =
        Endpoint::client(socket, peer, &client_identity, &expected, NAME, shape()).unwrap();
      result_tx.send(endpoint.establish().await).unwrap();
    })
    .unwrap();
  sim.run_until_idle();
  assert!(matches!(
    result_rx.try_recv().unwrap(),
    Err(EndpointError::Handshake(
      slates_transport::handshake::HandshakeError::Tls(rustls::Error::InvalidCertificate(
        rustls::CertificateError::ApplicationVerificationFailure
      ))
    ))
  ));
}
