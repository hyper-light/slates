//! AUD-29-49 (§4.10a; RFC 9000 §8.1): a server sends an unvalidated address at most three times the bytes it
//! received from it. A relay on the path counts each direction's bytes. The server's certificate is wide, so
//! its flight is larger than three padded client datagrams: with a client that falls silent after its first
//! datagram — what a spoofed source is — the server stops at its allowance and holds; with a client that
//! keeps retransmitting, each datagram grows the allowance, the server resumes where it stopped, and the
//! handshake completes. Until 2026-09-30 a server sent its whole flight to any source that sent a hello.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::mpsc::{Sender, channel};

use rustix::net::{Ipv4Addr, SocketAddrV4};
use rustls::pki_types::PrivateKeyDer;
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::SimRuntime;
use slates_rt::udp::UdpSocket;
use slates_transport::connection::ConnectionShape;
use slates_transport::endpoint::{Endpoint, EndpointError};
use slates_transport::handshake::Identity;

/// Format: the name the client dials.
const NAME: &str = "slates-node";
/// Shape: names on the server's certificate — enough to make its flight span many path-floor datagrams.
const WIDE_NAMES: usize = 120;
/// Shape: how long the relay waits for a datagram before it judges the handshake over (virtual time).
const QUIET_NS: u64 = 30_000_000_000;
/// Shape: the largest datagram the relay reads.
const DATAGRAM_BYTES: usize = 65_536;
/// Format: RFC 9000 §8.1's factor, restated so the test states the rule it checks.
const FACTOR: u64 = 3;

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

fn identity(names: &[String]) -> Identity {
  let key = rcgen::KeyPair::generate().unwrap();
  let cert = rcgen::CertificateParams::new(names.to_vec())
    .unwrap()
    .self_signed(&key)
    .unwrap();
  Identity::from_der(
    cert.der().clone(),
    PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
  )
}

/// The bytes each direction carried through the relay.
#[derive(Debug, Default)]
struct Carried {
  client_to_server: u64,
  server_to_client: u64,
}

/// Relays until the path is quiet; with `silence_client`, forwards only the client's first datagram.
async fn relay(
  socket: UdpSocket,
  client: SocketAddrV4,
  server: SocketAddrV4,
  silence_client: bool,
  report: Sender<Carried>,
) {
  let mut buf = vec![0u8; DATAGRAM_BYTES];
  let mut carried = Carried::default();
  let mut client_datagrams = 0u32;
  loop {
    let received = slates_rt::futures::within(QUIET_NS, socket.recv_from(&mut buf)).await;
    let Ok(Some(Ok((len, from)))) = received else {
      break;
    };
    let datagram = buf.get(..len).unwrap();
    let bytes = u64::try_from(len).unwrap();
    if from == client {
      client_datagrams += 1;
      if silence_client && client_datagrams > 1 {
        continue;
      }
      carried.client_to_server += bytes;
      socket.send_to(datagram, server).unwrap();
    } else {
      carried.server_to_client += bytes;
      socket.send_to(datagram, client).unwrap();
    }
  }
  let _ = report.send(carried);
}

struct Run {
  client: Result<(), EndpointError>,
  holds: u64,
  carried: Carried,
}

fn handshake(silence_client: bool) -> Run {
  let mut sim = SimRuntime::new(&config(), 5).unwrap();
  let shard = sim.shard_ids()[0];
  let names: Vec<String> = (0..WIDE_NAMES)
    .map(|at| format!("{NAME}-{at}.example"))
    .chain(std::iter::once(NAME.to_owned()))
    .collect();
  let server_identity = identity(&names);
  let client_identity = identity(&["slates-client".to_owned()]);
  let (client_cert, server_cert) = (client_identity.certificate(), server_identity.certificate());
  let (client_tx, client_rx) = channel();
  let (server_tx, server_rx) = channel();
  let (carried_tx, carried_rx) = channel();
  sim
    .spawn_on(shard, async move {
      let any = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
      let (client_socket, server_socket, relay_socket) = (
        UdpSocket::bind(any).unwrap(),
        UdpSocket::bind(any).unwrap(),
        UdpSocket::bind(any).unwrap(),
      );
      let (client_at, server_at, relay_at) = (
        client_socket.local_addr().unwrap(),
        server_socket.local_addr().unwrap(),
        relay_socket.local_addr().unwrap(),
      );
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
      let relaying = slates_rt::futures::spawn(relay(
        relay_socket,
        client_at,
        server_at,
        silence_client,
        carried_tx,
      ))
      .unwrap();
      let serving = slates_rt::futures::spawn(async move {
        let _ = server.establish().await;
        let _ = server_tx.send(server.amplification_holds());
      })
      .unwrap();
      let _ = client_tx.send(client.establish().await);
      let _ = slates_rt::futures::join(serving).await;
      let _ = slates_rt::futures::join(relaying).await;
    })
    .unwrap();
  sim.run_until_idle();
  Run {
    client: client_rx.recv().unwrap(),
    holds: server_rx.recv().unwrap(),
    carried: carried_rx.recv().unwrap(),
  }
}

/// AUD-29-49: do: a server with a wide certificate and a client that falls silent after its first datagram
/// (a spoofed source's shape); expect the server to have sent at most three times what it received, to
/// have held at least once (non-vacuity: its flight was larger than its allowance), and the client not to
/// establish.
#[test]
fn a_silent_source_draws_at_most_three_times_what_it_sent() {
  let run = handshake(true);
  eprintln!("{:?} holds {}", run.carried, run.holds);
  assert!(
    run.carried.server_to_client <= FACTOR * run.carried.client_to_server,
    "amplified past the allowance: {:?}",
    run.carried
  );
  assert!(run.holds >= 1, "the server's flight exceeded its allowance");
  assert!(run.client.is_err());
}

/// AUD-29-49: do: the same wide server and a client that keeps retransmitting; expect the handshake to
/// complete — each client datagram grows the allowance and the server resumes its flight where it stopped —
/// with the server having held on the way.
#[test]
fn a_talking_client_completes_a_flight_larger_than_its_allowance() {
  let run = handshake(false);
  eprintln!("{:?} holds {}", run.carried, run.holds);
  assert!(run.client.is_ok(), "{:?}", run.client);
  assert!(run.holds >= 1, "the server's flight exceeded its allowance");
}
