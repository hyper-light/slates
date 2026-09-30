//! AUD-29-48 (§4.10a; RFC 9001 §6 key update, §6.6 usage limits): a long-lived session moves through key
//! generations as its packets reach each generation's limit, over live exchanges — requests, replies, their
//! acknowledgements and whatever probes the path needs — and every exchange is answered correctly across the
//! updates. The limit is capped small ([`Endpoint::cap_key_usage`]), so crossing it takes tens of packets,
//! not billions. Until 2026-09-30 the first generation protected a session for its whole life.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::mpsc::channel;

use rustix::net::{Ipv4Addr, SocketAddrV4};
use rustls::pki_types::PrivateKeyDer;
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::SimRuntime;
use slates_rt::udp::UdpSocket;
use slates_transport::connection::{ConnectionShape, Priority};
use slates_transport::endpoint::{Endpoint, EndpointError};
use slates_transport::handshake::Identity;

/// Format: the name the client dials.
const NAME: &str = "slates-node";
/// Shape: the stream kind the exchanges use.
const KIND: u64 = 1;
/// Shape: packets one key generation may seal in this test — small, so the exchanges cross it many times.
const PACKETS_PER_GENERATION: u64 = 8;
/// Shape: request/reply exchanges, enough to cross several generations at the cap above.
const EXCHANGES: u8 = 40;

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

/// AUD-29-48: do: a client and server whose key generations each seal at most eight packets; forty request
/// / reply exchanges; expect every reply correct, both ends to have moved through several generations, and
/// no packet refused as a forgery (every update recognized by its key phase, stragglers opened by the kept
/// previous key).
#[test]
fn a_long_session_updates_its_keys_and_every_exchange_is_answered() {
  let mut sim = SimRuntime::new(&config(), 11).unwrap();
  let shard = sim.shard_ids()[0];
  let client_identity = identity("slates-client");
  let server_identity = identity(NAME);
  let (client_cert, server_cert) = (client_identity.certificate(), server_identity.certificate());
  let (client_tx, client_rx) = channel();
  let (server_tx, server_rx) = channel();
  sim
    .spawn_on(shard, async move {
      let any = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
      let (client_socket, server_socket) =
        (UdpSocket::bind(any).unwrap(), UdpSocket::bind(any).unwrap());
      let (client_at, server_at) = (
        client_socket.local_addr().unwrap(),
        server_socket.local_addr().unwrap(),
      );
      let mut client = Endpoint::client(
        client_socket,
        server_at,
        &client_identity,
        &server_cert,
        NAME,
        shape(),
      )
      .unwrap();
      let mut server = Endpoint::server(
        server_socket,
        client_at,
        &server_identity,
        &[client_cert],
        shape(),
      )
      .unwrap();
      client.cap_key_usage(PACKETS_PER_GENERATION, u64::MAX);
      server.cap_key_usage(PACKETS_PER_GENERATION, u64::MAX);
      let serving = slates_rt::futures::spawn(async move {
        let outcome: Result<(), EndpointError> = async {
          server.establish().await?;
          for _ in 0..EXCHANGES {
            server
              .serve_once(|_, request| request.iter().map(|b| b.wrapping_add(1)).collect())
              .await?;
          }
          Ok(())
        }
        .await;
        let _ = server_tx.send((outcome, server.key_usage()));
      })
      .unwrap();
      let outcome: Result<Vec<Vec<u8>>, EndpointError> = async {
        client.establish().await?;
        let mut replies = Vec::new();
        for round in 0..EXCHANGES {
          replies.push(
            client
              .request(KIND, Priority::Control, &[round, round])
              .await?,
          );
        }
        Ok(replies)
      }
      .await;
      let _ = client_tx.send((outcome, client.key_usage()));
      let _ = slates_rt::futures::join(serving).await;
    })
    .unwrap();
  sim.run_until_idle();
  let (replies, (client_updates, client_failures)) = client_rx.recv().unwrap();
  let (served, (server_updates, server_failures)) = server_rx.recv().unwrap();
  let replies = replies.unwrap();
  served.unwrap();
  for (round, reply) in (0..EXCHANGES).zip(&replies) {
    assert_eq!(reply, &vec![round.wrapping_add(1), round.wrapping_add(1)]);
  }
  eprintln!("key generations moved past: client {client_updates}, server {server_updates}");
  assert!(
    client_updates >= 2,
    "the client moved through generations: {client_updates}"
  );
  assert!(
    server_updates >= 2,
    "the server moved through generations: {server_updates}"
  );
  assert_eq!(
    (client_failures, server_failures),
    (0, 0),
    "no packet was refused as a forgery"
  );
}
