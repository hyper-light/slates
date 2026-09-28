//! The session plane's path-MTU benchmark (§4.10a; RFC 8899 as RFC 9000 §14.3 applies it; the constrained-link
//! design, `docs/wip/research/nfs-transport-constrained-links.md` slice 3d): real endpoints on the real runtime
//! and real loopback sockets — TLS 1.3 handshake, packet protection, the clocked connection with Copa — move
//! one bulk transfer from a client shard to a server shard, and the wall-clock goodput is recorded with the
//! path MTU discovery settled on and what its search cost (probes sent, acknowledged, lost, refused).
//!
//! What it answers: how much a session gains by sending the largest datagram its path carries instead of the
//! 1,200-byte floor. On loopback the gain is the per-packet cost — a system call, header protection and an
//! AEAD seal per packet — divided over fewer, larger packets. The same transfer on the commit before discovery
//! (`ed613fe`, every packet at the floor) is the baseline; `docs/wip/BENCHMARKS.md` records both.
//!
//! `cargo run --release -p slates-transport --example path_mtu_bench` prints one CSV row per run, then the best.
//! **Failures** (the process exits non-zero): a run whose reply is not the request's digest, or that fails.

// A benchmark harness: an unwrap here is a failed run, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::mpsc::{Receiver, channel};
use std::time::Instant;

use rustls::pki_types::PrivateKeyDer;
use slates_rt::runtime::{Runtime, RuntimeConfig};
use slates_rt::udp::{Ipv4Addr, SocketAddrV4, UdpSocket};
use slates_transport::connection::{ConnectionShape, Priority};
use slates_transport::endpoint::{Endpoint, MAX_PACKET_PAYLOAD};
use slates_transport::handshake::Identity;

/// Shape: the bulk transfer each run moves — large enough that the handshake and the search's early probes
/// are a small part of the run (a 256 MiB transfer is thousands of round trips at loopback rates).
const TRANSFER_BYTES: usize = 256 << 20;
/// Shape: the runs recorded; the best is reported (the BENCHMARKS.md best-of-N discipline, all N shown).
const RUNS: usize = 5;
/// Shape: the session's receive ceiling — the flow-control window may auto-tune to it; large enough that the
/// window never limits a loopback transfer (the controller does).
const RECEIVE_CEILING: u64 = 64 << 20;
/// Format: the TLS name both ends present.
const NAME: &str = "slates-bench";
/// Format: the request kind the transfer rides.
const BULK_KIND: u64 = 3;

fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 2,
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

fn self_signed() -> Identity {
  let key = rcgen::KeyPair::generate().unwrap();
  let cert = rcgen::CertificateParams::new(vec![NAME.to_owned()])
    .unwrap()
    .self_signed(&key)
    .unwrap();
  Identity::from_der(
    cert.der().clone(),
    PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
  )
}

fn shape() -> ConnectionShape {
  ConnectionShape::for_frame_cap(MAX_PACKET_PAYLOAD, RECEIVE_CEILING)
}

/// A digest of the transfer: its length and a position-weighted sum, so a truncated or reordered delivery
/// answers differently.
fn digest(bytes: &[u8]) -> Vec<u8> {
  let sum = bytes.iter().enumerate().fold(0u64, |sum, (at, byte)| {
    sum.wrapping_add(u64::from(*byte).wrapping_mul(u64::try_from(at).unwrap_or(0) + 1))
  });
  let mut out = u64::try_from(bytes.len())
    .unwrap_or(0)
    .to_le_bytes()
    .to_vec();
  out.extend_from_slice(&sum.to_le_bytes());
  out
}

/// What one run measured.
struct Run {
  seconds: f64,
  path_mtu: Option<usize>,
  probes: String,
}

/// Waits (yielding to the shard) for a value on `rx`.
async fn recv<T>(rx: Receiver<T>) -> T {
  loop {
    if let Ok(value) = rx.try_recv() {
      return value;
    }
    slates_rt::futures::yield_now().await;
  }
}

fn one_run(payload: &'static [u8]) -> Run {
  let rt = Runtime::start(&config()).unwrap();
  let shards = rt.shard_ids();
  let server_identity = self_signed();
  let client_identity = self_signed();
  let server_cert = server_identity.certificate();
  let client_cert = client_identity.certificate();
  let server_socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
  let client_socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
  let server_addr = server_socket.local_addr().unwrap();
  let client_addr = client_socket.local_addr().unwrap();
  let (done_tx, done_rx) = channel();
  let (result_tx, result_rx) = channel();
  rt.spawn_on(shards[0], async move {
    let mut server = Endpoint::server(
      server_socket,
      client_addr,
      &server_identity,
      std::slice::from_ref(&client_cert),
      shape(),
    )
    .unwrap();
    server.establish().await.unwrap();
    let (request, _kind, received) = server.next_request().await.unwrap();
    server.reply(request, &digest(&received)).unwrap();
    server.settle().await.unwrap();
    let _ = done_tx.send(());
  })
  .unwrap();
  rt.spawn_on(shards[1], async move {
    let mut client = Endpoint::client(
      client_socket,
      server_addr,
      &client_identity,
      &server_cert,
      NAME,
      shape(),
    )
    .unwrap();
    client.establish().await.unwrap();
    let started = Instant::now();
    let reply = client
      .request(BULK_KIND, Priority::Bulk, payload)
      .await
      .unwrap();
    let seconds = started.elapsed().as_secs_f64();
    assert_eq!(reply, digest(payload), "the transfer arrived byte-exact");
    let _ = result_tx.send(Run {
      seconds,
      path_mtu: client.path_mtu(),
      probes: format!("{:?}", client.path_mtu_stats()),
    });
    recv(done_rx).await;
  })
  .unwrap();
  let run = result_rx.recv().unwrap();
  let _ = rt.shutdown();
  run
}

fn main() {
  let payload: &'static [u8] = Box::leak(
    (0..TRANSFER_BYTES)
      .map(|at| u8::try_from(at % 251).unwrap_or(0))
      .collect::<Vec<u8>>()
      .into_boxed_slice(),
  );
  println!("run,bytes,seconds,goodput_mbps,path_mtu,probes");
  let mut best: f64 = 0.0;
  for run in 0..RUNS {
    let measured = one_run(payload);
    #[allow(clippy::cast_precision_loss)]
    let mbps = (TRANSFER_BYTES as f64 * 8.0) / measured.seconds / 1e6;
    best = best.max(mbps);
    println!(
      "{run},{TRANSFER_BYTES},{:.3},{mbps:.1},{:?},\"{}\"",
      measured.seconds, measured.path_mtu, measured.probes
    );
  }
  println!("best,{TRANSFER_BYTES},,{best:.1},,");
}
