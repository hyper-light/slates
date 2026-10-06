//! AUD-29-47 (§4.10a, D-15; RFC 9001 §4.1.3): the handshake's Handshake-level bytes — EncryptedExtensions,
//! both certificates, CertificateVerify, Finished — never cross the wire in plaintext. A relay on the path
//! records every datagram of a full mutual handshake and may meddle: tamper with a sealed fragment once,
//! tamper with every one, or deliver a sealed flight ahead of the ServerHello it follows. Until 2026-09-30
//! both levels crossed as one plaintext stream, and an observer read the certificates off it.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::sync::mpsc::{Sender, channel};

use rustix::net::{Ipv4Addr, SocketAddrV4};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::SimRuntime;
use slates_rt::udp::UdpSocket;
use slates_transport::connection::ConnectionShape;
use slates_transport::endpoint::{Endpoint, EndpointError};
use slates_transport::flight::{SEALED_FRAGMENT_TAG, SEALED_HEADER};
use slates_transport::handshake::Identity;

/// Format: the name the client dials (the server's certificate names it).
const NAME: &str = "slates-node";
/// Shape: how long the relay waits for a datagram before it judges the handshake over (virtual time,
/// past every retransmit budget the handshake spends).
const QUIET_NS: u64 = 30_000_000_000;
/// Shape: the largest datagram the relay reads (the path floor fits well within it).
const DATAGRAM_BYTES: usize = 65_536;

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

fn shape() -> ConnectionShape {
  let cap = slates_transport::endpoint::MAX_PACKET_PAYLOAD;
  ConnectionShape::for_frame_cap(
    cap,
    slates_transport::connection::initial_receive_window(cap),
  )
}

/// A self-signed identity naming `name`.
fn identity(name: &str) -> Identity {
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

/// What the relay does to the server's sealed fragments on their way to the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Meddling {
  /// Forwards everything as it came.
  None,
  /// Flips one ciphertext byte of the first sealed fragment; its retransmits pass intact.
  TamperFirstSealed,
  /// Flips one ciphertext byte of every sealed fragment.
  TamperEverySealed,
  /// Holds the server's first plain fragment (its ServerHello) until a sealed fragment has gone ahead of it.
  SealedBeforeHello,
}

fn is_sealed(datagram: &[u8]) -> bool {
  datagram.first() == Some(&SEALED_FRAGMENT_TAG)
}

/// Relays between `client` and `server` on `socket` until the path is quiet, meddling as asked; reports
/// every datagram it saw, as it arrived.
async fn relay(
  socket: UdpSocket,
  client: SocketAddrV4,
  server: SocketAddrV4,
  meddling: Meddling,
  seen: Sender<Vec<Vec<u8>>>,
) {
  let mut buf = vec![0u8; DATAGRAM_BYTES];
  let mut recorded = Vec::new();
  let mut tampered = 0u32;
  let mut held: Option<Vec<u8>> = None;
  let mut hello_held = false;
  loop {
    let received = slates_rt::futures::within(QUIET_NS, socket.recv_from(&mut buf)).await;
    let Ok(Some(Ok((len, from)))) = received else {
      break;
    };
    let mut datagram = buf.get(..len).unwrap().to_vec();
    recorded.push(datagram.clone());
    if from == client {
      socket.send_to(&datagram, server).unwrap();
      continue;
    }
    let tamper = is_sealed(&datagram)
      && match meddling {
        Meddling::TamperFirstSealed => tampered == 0,
        Meddling::TamperEverySealed => true,
        Meddling::None | Meddling::SealedBeforeHello => false,
      };
    if tamper {
      tampered += 1;
      let at = SEALED_HEADER + 1;
      if let Some(byte) = datagram.get_mut(at) {
        *byte ^= 0x01;
      }
    }
    if meddling == Meddling::SealedBeforeHello && !hello_held && !is_sealed(&datagram) {
      hello_held = true;
      held = Some(datagram);
      continue;
    }
    socket.send_to(&datagram, client).unwrap();
    if is_sealed(&datagram)
      && let Some(hello) = held.take()
    {
      socket.send_to(&hello, client).unwrap();
    }
  }
  let _ = seen.send(recorded);
}

/// One handshake through a meddling relay: both ends' outcomes, the client's discarded count, every
/// datagram on the path, and both certificates.
struct Run {
  client: Result<(), EndpointError>,
  server: Result<(), EndpointError>,
  client_discarded: u64,
  wire: Vec<Vec<u8>>,
  certificates: [CertificateDer<'static>; 2],
}

fn handshake_through(meddling: Meddling) -> Run {
  let mut sim = SimRuntime::new(&config(), 7).unwrap();
  let shard = sim.shard_ids()[0];
  let client_identity = identity("slates-client");
  let server_identity = identity(NAME);
  let certificates = [client_identity.certificate(), server_identity.certificate()];
  let (client_cert, server_cert) = (certificates[0].clone(), certificates[1].clone());
  let (client_tx, client_rx) = channel();
  let (server_tx, server_rx) = channel();
  let (wire_tx, wire_rx) = channel();
  sim
    .spawn_on(shard, async move {
      let any = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
      let (client_socket, server_socket, relay_socket) = (
        UdpSocket::bind(any).unwrap(),
        UdpSocket::bind(any).unwrap(),
        UdpSocket::bind(any).unwrap(),
      );
      let client_at = client_socket.local_addr().unwrap();
      let server_at = server_socket.local_addr().unwrap();
      let relay_at = relay_socket.local_addr().unwrap();
      let mut client = Endpoint::client(
        client_socket,
        relay_at,
        &client_identity,
        &server_cert,
        NAME,
        shape(),
      )
      .unwrap();
      let mut server = Endpoint::server(
        server_socket,
        relay_at,
        &server_identity,
        &[client_cert],
        shape(),
      )
      .unwrap();
      let relaying =
        slates_rt::futures::spawn(relay(relay_socket, client_at, server_at, meddling, wire_tx))
          .unwrap();
      let serving = slates_rt::futures::spawn(async move {
        let _ = server_tx.send(server.establish().await);
      })
      .unwrap();
      let outcome = client.establish().await;
      let _ = client_tx.send((outcome, client.discarded()));
      let _ = slates_rt::futures::join(serving).await;
      let _ = slates_rt::futures::join(relaying).await;
    })
    .unwrap();
  sim.run_until_idle();
  let (client, client_discarded) = client_rx.recv().unwrap();
  Run {
    client,
    server: server_rx.recv().unwrap(),
    client_discarded,
    wire: wire_rx.recv().unwrap(),
    certificates,
  }
}

/// Whether `needle` appears anywhere in `datagram`.
fn carries(datagram: &[u8], needle: &[u8]) -> bool {
  datagram
    .windows(needle.len())
    .any(|window| window == needle)
}

/// AUD-29-47: do: a full mutual handshake through a recording relay; expect it to complete both ways, no
/// datagram on the path to carry either certificate, and the Handshake level to have crossed sealed — the
/// sealed fragments carry at least a certificate's worth of bytes (non-vacuity).
#[test]
fn no_certificate_crosses_the_wire_in_plaintext() {
  let run = handshake_through(Meddling::None);
  assert!(run.client.is_ok(), "{:?}", run.client);
  assert!(run.server.is_ok(), "{:?}", run.server);
  for certificate in &run.certificates {
    assert!(
      !run
        .wire
        .iter()
        .any(|datagram| carries(datagram, certificate.as_ref())),
      "a certificate crossed in plaintext"
    );
  }
  let sealed: usize = run
    .wire
    .iter()
    .filter(|datagram| is_sealed(datagram))
    .map(Vec::len)
    .sum();
  assert!(
    sealed >= run.certificates[1].as_ref().len(),
    "the Handshake level crossed sealed: {sealed} sealed bytes"
  );
}

/// AUD-29-47: do: tamper with the first sealed fragment the server sends; expect the client to refuse it
/// (counted as discarded, never fed to TLS) and the handshake to complete on the intact retransmit.
#[test]
fn a_tampered_sealed_fragment_is_refused_and_its_retransmit_completes() {
  let run = handshake_through(Meddling::TamperFirstSealed);
  assert!(run.client.is_ok(), "{:?}", run.client);
  assert!(run.server.is_ok(), "{:?}", run.server);
  assert!(
    run.client_discarded >= 1,
    "the tampered fragment was discarded"
  );
}

/// AUD-29-47: do: tamper with every sealed fragment the server sends; expect the client never to establish
/// (typed `NotReady` once its budget is spent) — no tampered Handshake-level byte is accepted.
#[test]
fn a_path_that_tampers_every_sealed_fragment_never_establishes() {
  let run = handshake_through(Meddling::TamperEverySealed);
  assert!(
    matches!(run.client, Err(EndpointError::NotReady)),
    "{:?}",
    run.client
  );
  assert!(run.client_discarded >= 1);
}

/// AUD-29-47: do: deliver the server's sealed flight ahead of the ServerHello that yields its keys; expect
/// the client to hold the sealed fragments until the keys arrive and complete the handshake.
#[test]
fn a_sealed_flight_ahead_of_its_hello_completes() {
  let run = handshake_through(Meddling::SealedBeforeHello);
  assert!(run.client.is_ok(), "{:?}", run.client);
  assert!(run.server.is_ok(), "{:?}", run.server);
}
