//! The node check's hash, A/B (main's ruling of 2026-11-21 05:12 (b)): w-perf's profile of hg-src's first push put
//! about 26 s of the shard's 90.6 s in `node::check`, the collision-detecting SHA-1 (sha1dc, `sha1-checked`, configured
//! as 7.2's binding configures it) over every pushed revision's whole text. This times, interleaved over `rounds`, each
//! way of hashing a text-like corpus in pieces of a manifest's and a file revision's typical length, one hasher a piece
//! as the intake hashes a revision: sha1dc as `node.rs` configures it; sha1dc with its detection off (software SHA-1);
//! AWS-LC's SHA-1 (the CPU's SHA instructions); and AWS-LC's then sha1dc's, the hardware hash with the collision check
//! beside it. Every way's digests are held equal to sha1dc's.
//!
//! Command: `cargo run --release -p slates-hg --example sha1_ab -- <corpus MiB> <rounds>`.

#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::indexing_slicing,
  clippy::cast_possible_truncation,
  clippy::cast_precision_loss,
  clippy::print_stdout,
  clippy::disallowed_methods
)]

use std::time::Instant;

use sha1_checked::{Digest, Sha1};

/// Shape: a manifest's typical length in hg's own history (about 4,200 entries of about 57 bytes), and a file
/// revision's: the two piece lengths a push hashes.
const PIECES: [usize; 2] = [240 * 1024, 8 * 1024];

fn sha1dc(detect: bool, piece: &[u8]) -> [u8; 20] {
  let mut state = Sha1::builder()
    .detect_collision(detect)
    .safe_hash(false)
    .build();
  Digest::update(&mut state, piece);
  let result = state.try_finalize();
  let mut out = [0; 20];
  out.copy_from_slice(result.hash());
  out
}

fn hardware(piece: &[u8]) -> [u8; 20] {
  let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY, piece);
  let mut out = [0; 20];
  out.copy_from_slice(digest.as_ref());
  out
}

fn main() {
  let arguments: Vec<String> = std::env::args().skip(1).collect();
  let mib: usize = arguments
    .first()
    .map_or(256, |value| value.parse().unwrap());
  let rounds: usize = arguments.get(1).map_or(5, |value| value.parse().unwrap());
  // Text-like bytes: manifest lines of a path, a NUL and forty hex digits, varied by a seeded generator.
  let mut corpus = Vec::with_capacity(mib << 20);
  let mut seed = 0x2026_1121_u64;
  while corpus.len() < mib << 20 {
    seed ^= seed << 13;
    seed ^= seed >> 7;
    seed ^= seed << 17;
    corpus.extend_from_slice(
      format!(
        "mercurial/dir{}/file{}.py\0{:040x}\n",
        seed % 97,
        seed % 1013,
        seed
      )
      .as_bytes(),
    );
  }
  corpus.truncate(mib << 20);
  println!(
    "corpus {} MiB; rounds {rounds}; MB/s of each way, every round",
    mib
  );
  for piece_len in PIECES {
    let pieces: Vec<&[u8]> = corpus.chunks(piece_len).collect();
    let reference: Vec<[u8; 20]> = pieces.iter().map(|piece| sha1dc(true, piece)).collect();
    let ways: [(&str, &dyn Fn(&[u8]) -> [u8; 20]); 4] = [
      ("sha1dc (node.rs)", &|piece| sha1dc(true, piece)),
      ("sha1dc, detection off", &|piece| sha1dc(false, piece)),
      ("aws-lc hardware", &hardware),
      ("hardware + sha1dc check", &|piece| {
        let fast = hardware(piece);
        assert_eq!(fast, sha1dc(true, piece));
        fast
      }),
    ];
    let mut rates: Vec<Vec<f64>> = vec![Vec::new(); ways.len()];
    for _ in 0..rounds {
      for (at, (_, way)) in ways.iter().enumerate() {
        let started = Instant::now();
        for (piece, expected) in pieces.iter().zip(&reference) {
          assert_eq!(way(piece), *expected);
        }
        rates[at].push(corpus.len() as f64 / started.elapsed().as_secs_f64() / 1e6);
      }
    }
    println!("pieces of {} KiB:", piece_len / 1024);
    for ((name, _), rates) in ways.iter().zip(&rates) {
      let shown: Vec<String> = rates.iter().map(|rate| format!("{rate:.0}")).collect();
      println!("  {name:<26} {}", shown.join(" "));
    }
  }
}
