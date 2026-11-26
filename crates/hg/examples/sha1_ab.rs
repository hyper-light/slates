//! The node check's hash, A/B (main's ruling of 2026-11-21 05:12 (b); updated for A-893): w-perf's profile of hg-src's
//! first push put about 26 s of the shard's 90.6 s in `node::check`, the collision-detecting SHA-1 (previously
//! `sha1-checked`, now `slates_vcs::sha1dc`, configured as Mercurial 7.2's binding configures it: detection on, safe
//! hash off) over every pushed revision's whole text. This times, interleaved over `rounds`, five ways of hashing a
//! text-like corpus in pieces of a manifest's and a file revision's typical length, one hasher a piece as the intake
//! hashes a revision: `sha1-checked` (previous production baseline, detection on); `slates_vcs::sha1dc` (current
//! `node.rs` implementation: 8-lane SIMD UBC + hardware SHA-1); `sha1-checked` with detection off (software SHA-1);
//! AWS-LC's hardware SHA-1 (the CPU's SHA instructions without collision detection); and AWS-LC's hardware hash
//! paired with `sha1-checked`'s collision check beside it. Every way's digests are held equal to `sha1-checked`'s.
//!
//! Command: `cargo run --release -p slates-hg --example sha1_ab -- [<corpus MiB> [<rounds>]]`.

#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::indexing_slicing,
  clippy::cast_possible_truncation,
  clippy::cast_precision_loss,
  clippy::print_stdout,
  clippy::print_stderr,
  clippy::disallowed_methods
)]

use std::ffi::{OsStr, OsString};
use std::io::Write as _;
use std::process::ExitCode;
use std::time::Instant;

use sha1_checked::{Digest, Sha1};

/// Shape: a manifest's typical length in hg's own history (about 4,200 entries of about 57 bytes), and a file
/// revision's: the two piece lengths a push hashes.
const PIECES: [usize; 2] = [240 * 1024, 8 * 1024];

/// Derived: IEC binary mebibyte (2^20 = 1,048,576 bytes), converting `<corpus MiB>` to bytes.
const BYTES_PER_MIB: usize = 1 << 20;

/// Shape: default corpus size in MiB when `<corpus MiB>` is omitted.
const DEFAULT_MIB: usize = 256;

/// Shape: default interleaved benchmark round count when `<rounds>` is omitted.
const DEFAULT_ROUNDS: usize = 5;

/// Format: usage-error exit code, matching the `slates` CLI convention.
const EXIT_USAGE: u8 = 2;

/// Validated benchmark run configuration parsed once at the CLI boundary.
struct Config {
  mib: usize,
  corpus_bytes: usize,
  rounds: usize,
}

/// Decodes an OS CLI argument as UTF-8 or returns a usage error.
fn decode_utf8_arg(raw: &OsStr) -> Result<&str, String> {
  raw.to_str().ok_or_else(|| {
    format!(
      "invalid argument `{}`: not valid UTF-8",
      raw.to_string_lossy()
    )
  })
}

/// Parses a positive integer CLI argument or returns a usage error naming the argument.
fn parse_positive_usize(label: &str, raw: &str) -> Result<usize, String> {
  let value: usize = raw
    .parse()
    .map_err(|_| format!("invalid {label} `{raw}`: expected a positive integer"))?;
  if value == 0 {
    return Err(format!("invalid {label} `0`: must be at least 1"));
  }
  Ok(value)
}

/// Converts a corpus size in MiB to a non-zero byte count within Rust's `isize::MAX` allocation layout cap.
fn corpus_bytes_from_mib(mib: usize) -> Result<usize, String> {
  let bytes = mib
    .checked_mul(BYTES_PER_MIB)
    .ok_or_else(|| format!("corpus MiB `{mib}` overflows address space"))?;
  if bytes == 0 || isize::try_from(bytes).is_err() {
    return Err(format!(
      "corpus MiB `{mib}` exceeds maximum allocation size"
    ));
  }
  Ok(bytes)
}

/// Parses optional `[<corpus MiB> [<rounds>]]` OS arguments without panicking.
fn parse_args(arguments: &[OsString]) -> Result<Config, String> {
  if let Some(extra) = arguments.get(2) {
    return Err(format!("unexpected argument `{}`", extra.to_string_lossy()));
  }
  let mib = match arguments.first() {
    Some(value) => parse_positive_usize("corpus MiB", decode_utf8_arg(value)?)?,
    None => DEFAULT_MIB,
  };
  let corpus_bytes = corpus_bytes_from_mib(mib)?;
  let rounds = match arguments.get(1) {
    Some(value) => parse_positive_usize("rounds", decode_utf8_arg(value)?)?,
    None => DEFAULT_ROUNDS,
  };
  Ok(Config {
    mib,
    corpus_bytes,
    rounds,
  })
}

/// Generates a deterministic `corpus_bytes`-byte text-like corpus of manifest lines (path, NUL, 40 hex digits).
fn build_corpus(corpus_bytes: usize) -> Result<Vec<u8>, String> {
  let mut corpus = Vec::new();
  corpus
    .try_reserve_exact(corpus_bytes)
    .map_err(|err| format!("cannot allocate {corpus_bytes}-byte corpus: {err}"))?;
  let mut seed = 0x2026_1121_u64;
  while corpus.len() < corpus_bytes {
    seed ^= seed << 13;
    seed ^= seed >> 7;
    seed ^= seed << 17;
    let line = format!(
      "mercurial/dir{}/file{}.py\0{:040x}\n",
      seed % 97,
      seed % 1013,
      seed
    );
    let remaining = corpus_bytes.saturating_sub(corpus.len());
    let bytes = line.as_bytes();
    corpus.extend_from_slice(bytes.get(..remaining).unwrap_or(bytes));
  }
  Ok(corpus)
}

/// A way of hashing a piece: its name and its hasher.
type Way<'a> = (&'a str, &'a dyn Fn(&[u8]) -> [u8; 20]);

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

fn slates_sha1dc(piece: &[u8]) -> [u8; 20] {
  match slates_vcs::sha1dc::hash(piece) {
    Ok(digest) => digest,
    Err(attack) => attack.sha1,
  }
}

fn hardware(piece: &[u8]) -> [u8; 20] {
  let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY, piece);
  let mut out = [0; 20];
  out.copy_from_slice(digest.as_ref());
  out
}

fn run_benchmark(config: &Config, corpus: &[u8]) {
  println!(
    "corpus {} MiB; rounds {}; MB/s of each way, every round",
    config.mib, config.rounds
  );
  for piece_len in PIECES {
    let pieces: Vec<&[u8]> = corpus.chunks(piece_len).collect();
    let reference: Vec<[u8; 20]> = pieces.iter().map(|piece| sha1dc(true, piece)).collect();
    let ways: [Way<'_>; 5] = [
      ("sha1-checked (old)", &|piece| sha1dc(true, piece)),
      ("slates-vcs sha1dc (new)", &slates_sha1dc),
      ("sha1-checked, det off", &|piece| sha1dc(false, piece)),
      ("aws-lc hardware", &hardware),
      ("hardware + sha1dc check", &|piece| {
        let fast = hardware(piece);
        assert_eq!(fast, sha1dc(true, piece));
        fast
      }),
    ];
    let mut rates: Vec<Vec<f64>> = vec![Vec::new(); ways.len()];
    for _ in 0..config.rounds {
      for ((_, way), way_rates) in ways.iter().zip(&mut rates) {
        let started = Instant::now();
        for (piece, expected) in pieces.iter().zip(&reference) {
          assert_eq!(way(piece), *expected);
        }
        way_rates.push(corpus.len() as f64 / started.elapsed().as_secs_f64() / 1e6);
      }
    }
    println!("pieces of {} KiB:", piece_len / 1024);
    for ((name, _), rates) in ways.iter().zip(&rates) {
      let shown: Vec<String> = rates.iter().map(|rate| format!("{rate:.0}")).collect();
      println!("  {name:<26} {}", shown.join(" "));
    }
  }
}

fn main() -> ExitCode {
  let arguments: Vec<OsString> = std::env::args_os().skip(1).collect();
  let config = match parse_args(&arguments) {
    Ok(parsed) => parsed,
    Err(message) => {
      let mut stderr = std::io::stderr();
      let _ = writeln!(stderr, "sha1_ab: {message}");
      let _ = writeln!(stderr, "usage: sha1_ab [<corpus MiB> [<rounds>]]");
      return ExitCode::from(EXIT_USAGE);
    }
  };
  let corpus = match build_corpus(config.corpus_bytes) {
    Ok(corpus) => corpus,
    Err(message) => {
      let _ = writeln!(std::io::stderr(), "sha1_ab: {message}");
      return ExitCode::from(EXIT_USAGE);
    }
  };
  run_benchmark(&config, &corpus);
  ExitCode::SUCCESS
}
