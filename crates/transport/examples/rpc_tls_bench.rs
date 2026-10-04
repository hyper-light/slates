//! The RPC-with-TLS connection-build benchmark (RFC 9289; §4.6 "Kubernetes publication without privilege",
//! AUD-29-75): what building a server connection per accepted TCP connection costs
//! ([`slates_transport::handshake::rpc_tls_connection`], one owner for rustls's signature-mandated `Arc`), set
//! against what sharing one config across connections would cost (an `Arc` clone, the shape R2 rejects when an
//! alternative exists), and against the mutual TLS 1.3 handshake every connection pays anyway.
//!
//! What it answers: whether the per-connection build is a material part of accepting a connection. A kernel
//! NFS client keeps one long-lived TCP connection per server, so the build is paid once per mount and per
//! reconnect; the question is its size against the handshake, not its rate.
//!
//! `cargo run --release -p slates-transport --example rpc_tls_bench` prints one CSV row per round, then the
//! best of each column. **Failures** (the process exits non-zero): a handshake that does not complete or does
//! not agree on `sunrpc`.

// A benchmark harness: an unwrap here is a failed run, which is what it should be. rustls's client and the
// shared-config comparison take `Arc` by signature (D-8 exception 3, a harness).
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::disallowed_types
)]

use std::sync::Arc;
use std::time::Instant;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection};
use slates_transport::handshake::{Identity, rpc_tls_connection};

/// Shape: the rounds recorded; the best is reported (the BENCHMARKS.md best-of-N discipline, all N shown).
const ROUNDS: usize = 7;
/// Shape: connections built per round, so one round's time is far above the clock's resolution.
const BUILDS_PER_ROUND: usize = 2_000;
/// Shape: handshakes per round (each is milliseconds' worth of public-key work, so fewer suffice).
const HANDSHAKES_PER_ROUND: usize = 200;
/// Format: the ALPN identifier RFC 9289 §7.2 assigns SunRPC.
const ALPN: &[u8] = b"sunrpc";
/// Format: the server's DNS name, in its certificate and in the client's expectation.
const SERVER_NAME: &str = "slates-0.slates.default.svc.cluster.local";

/// A certificate authority and two leaves it issued (the server's and a node's client), DER.
struct Pki {
  authority: CertificateDer<'static>,
  server: (CertificateDer<'static>, PrivateKeyDer<'static>),
  client: (CertificateDer<'static>, PrivateKeyDer<'static>),
}

fn pki() -> Pki {
  let authority_key = rcgen::KeyPair::generate().unwrap();
  let mut authority_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
  authority_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
  let authority = authority_params.self_signed(&authority_key).unwrap();
  let leaf = |name: &str| {
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec![name.to_owned()])
      .unwrap()
      .signed_by(&key, &authority, &authority_key)
      .unwrap();
    (
      cert.der().clone(),
      PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
    )
  };
  Pki {
    authority: authority.der().clone(),
    server: leaf(SERVER_NAME),
    client: leaf("node-a"),
  }
}

fn server_identity(pki: &Pki) -> Identity {
  Identity::from_der(pki.server.0.clone(), pki.server.1.clone_key())
    .with_authorities(vec![pki.authority.clone()])
}

/// The shape R2 rejects, built for comparison only: one config shared by every connection.
fn shared_config(pki: &Pki) -> Arc<ServerConfig> {
  let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
  let mut roots = RootCertStore::empty();
  roots.add(pki.authority.clone()).unwrap();
  let verifier =
    rustls::server::WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
      .build()
      .unwrap();
  let mut config = ServerConfig::builder_with_provider(provider)
    .with_protocol_versions(&[&rustls::version::TLS13])
    .unwrap()
    .with_client_cert_verifier(verifier)
    .with_single_cert(vec![pki.server.0.clone()], pki.server.1.clone_key())
    .unwrap();
  config.alpn_protocols = vec![ALPN.to_vec()];
  config.send_tls13_tickets = 0;
  Arc::new(config)
}

fn client_config(pki: &Pki) -> Arc<ClientConfig> {
  let mut roots = RootCertStore::empty();
  roots.add(pki.authority.clone()).unwrap();
  let mut config =
    ClientConfig::builder_with_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
      .with_protocol_versions(&[&rustls::version::TLS13])
      .unwrap()
      .with_root_certificates(roots)
      .with_client_auth_cert(vec![pki.client.0.clone()], pki.client.1.clone_key())
      .unwrap();
  config.alpn_protocols = vec![ALPN.to_vec()];
  Arc::new(config)
}

/// Runs one handshake to completion in memory; panics if it fails or does not agree on `sunrpc`.
fn handshake(mut server: ServerConnection, client_config: &Arc<ClientConfig>) {
  let name = ServerName::try_from(SERVER_NAME).unwrap();
  let mut client = ClientConnection::new(client_config.clone(), name).unwrap();
  let mut wire = Vec::new();
  while client.is_handshaking() || server.is_handshaking() {
    wire.clear();
    while client.wants_write() {
      client.write_tls(&mut wire).unwrap();
    }
    let mut bytes = wire.as_slice();
    while !bytes.is_empty() {
      server.read_tls(&mut bytes).unwrap();
      server.process_new_packets().unwrap();
    }
    wire.clear();
    while server.wants_write() {
      server.write_tls(&mut wire).unwrap();
    }
    let mut bytes = wire.as_slice();
    while !bytes.is_empty() {
      client.read_tls(&mut bytes).unwrap();
      client.process_new_packets().unwrap();
    }
  }
  assert_eq!(
    server.alpn_protocol(),
    Some(ALPN),
    "the session agreed on sunrpc"
  );
  assert!(
    server.peer_certificates().is_some(),
    "the client was authenticated"
  );
}

/// Nanoseconds per operation over `count` runs of `operation`.
fn per_op(count: usize, mut operation: impl FnMut()) -> f64 {
  let start = Instant::now();
  for _ in 0..count {
    operation();
  }
  start.elapsed().as_nanos() as f64 / count as f64
}

fn main() {
  let pki = pki();
  let identity = server_identity(&pki);
  let shared = shared_config(&pki);
  let client = client_config(&pki);
  println!(
    "round,build_per_connection_us,shared_config_clone_us,mutual_handshake_us,build_share_of_accept"
  );
  let mut best = [f64::MAX; 3];
  for round in 1..=ROUNDS {
    let build = per_op(BUILDS_PER_ROUND, || {
      std::hint::black_box(rpc_tls_connection(&identity, ALPN).unwrap());
    });
    let clone = per_op(BUILDS_PER_ROUND, || {
      std::hint::black_box(ServerConnection::new(shared.clone()).unwrap());
    });
    let handshake_ns = per_op(HANDSHAKES_PER_ROUND, || {
      handshake(ServerConnection::new(shared.clone()).unwrap(), &client);
    }) - clone;
    let share = build / (build + handshake_ns);
    println!(
      "{round},{:.2},{:.2},{:.2},{:.4}",
      build / 1e3,
      clone / 1e3,
      handshake_ns / 1e3,
      share
    );
    best[0] = best[0].min(build);
    best[1] = best[1].min(clone);
    best[2] = best[2].min(handshake_ns);
  }
  println!(
    "best,{:.2},{:.2},{:.2},{:.4}",
    best[0] / 1e3,
    best[1] / 1e3,
    best[2] / 1e3,
    best[0] / (best[0] + best[2])
  );
}
