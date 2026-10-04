//! The control-plane seal benchmark (§4.10a §7): what sealing and opening one control datagram costs
//! ([`ControlDatagram::encode_sealed`] then [`ControlDatagram::decode_sealed`], AES-256-GCM under a counter nonce),
//! at a small body and at a large one, and how long a fresh process waits for its first random bytes
//! (`rustls`' provider draws them for every handshake; AWS-LC built with CPU jitter entropy pays its gathering
//! there).
//!
//! What it answers: the seal's cost on the cryptographic library slates builds on, against the one before it (the
//! RustCrypto crates measured with this same file), and the start-up cost the jitter-entropy switch removes.
//!
//! `cargo run --release -p slates-transport --example seal_bench` prints the first-random time, one CSV row per
//! round, then the best of each column. **Failures** (the process exits non-zero): a datagram that does not open
//! back to what was sealed.

// A benchmark harness: an unwrap here is a failed run, which is what it should be. rustls's process default provider
// is held in an `Arc` by its signature (D-8 exception 3, a harness).
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::disallowed_types
)]

use std::time::Instant;

use slates_transport::seal::{KEY_BYTES, Opener, Sealer};
use slates_transport::{ControlDatagram, Envelope};

/// Shape: the rounds recorded; the best is reported (the BENCHMARKS.md best-of-N discipline, all N shown).
const ROUNDS: usize = 7;
/// Shape: datagrams sealed and opened per round, far above the clock's resolution and far below the key's
/// confidentiality limit (2^23).
const PER_ROUND: usize = 100_000;
/// Shape: the body sizes measured — a typical control message, and one near a datagram's payload floor.
const BODIES: [usize; 2] = [64, 1024];

/// A control datagram with a body of `len` bytes.
fn datagram(len: usize) -> ControlDatagram {
  ControlDatagram {
    sender: 0x0102_0304_0506_0708,
    key_epoch: 7,
    envelope: Envelope {
      kind: 3,
      class: 1,
      flags: 0,
      epoch: 42,
      hlc: 0xDEAD_BEEF,
      request_id: 99,
    },
    body: vec![0x5a; len],
  }
}

/// Nanoseconds per seal-and-open pair over one round at body `len`.
fn round(len: usize) -> f64 {
  let key = [0x11u8; KEY_BYTES];
  let mut sealer = Sealer::from_key(&key, 1);
  let mut opener = Opener::from_key(&key, 1);
  let sample = datagram(len);
  let started = Instant::now();
  for _ in 0..PER_ROUND {
    let wire = sample.encode_sealed(&mut sealer).unwrap();
    let opened = ControlDatagram::decode_sealed(&wire, &mut opener).unwrap();
    assert_eq!(
      opened.body.len(),
      len,
      "the datagram opened to what was sealed"
    );
  }
  started.elapsed().as_nanos() as f64 / PER_ROUND as f64
}

fn main() {
  // The first random bytes of this process, through the provider every handshake uses.
  let provider = rustls::crypto::CryptoProvider::get_default()
    .cloned()
    .unwrap_or_else(|| std::sync::Arc::new(provider()));
  let mut first = [0u8; 32];
  let started = Instant::now();
  provider.secure_random.fill(&mut first).unwrap();
  println!(
    "first random bytes: {:.1} µs",
    started.elapsed().as_nanos() as f64 / 1_000.0
  );
  println!("round,seal_open_64_ns,seal_open_1024_ns");
  let mut best = [f64::MAX; BODIES.len()];
  for index in 0..ROUNDS {
    let row: Vec<f64> = BODIES.iter().map(|len| round(*len)).collect();
    for (slot, value) in best.iter_mut().zip(&row) {
      *slot = slot.min(*value);
    }
    println!("{},{:.1},{:.1}", index + 1, row[0], row[1]);
  }
  println!("best,{:.1},{:.1}", best[0], best[1]);
}

/// The provider the transport's handshake builds on.
fn provider() -> rustls::crypto::CryptoProvider {
  rustls::crypto::aws_lc_rs::default_provider()
}
