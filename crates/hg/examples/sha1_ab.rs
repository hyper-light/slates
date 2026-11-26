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

#![allow(clippy::cast_precision_loss)]

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

/// Shape: number of hashing implementations timed in each interleaved round.
const WAY_COUNT: usize = 5;

/// Format: usage-error exit code, matching the `slates` CLI convention.
const EXIT_USAGE: u8 = 2;

/// Format: undelivered-output exit code, matching the `slates` CLI convention.
const EXIT_OUTPUT: u8 = 4;

/// A way of hashing a piece: its label and its hash function.
type Way = (&'static str, fn(&[u8]) -> [u8; 20]);

/// Shape: the five hashing implementations compared in every round.
const WAYS: [Way; WAY_COUNT] = [
  ("sha1-checked (old)", sha1dc_checked),
  ("slates-vcs sha1dc (new)", slates_sha1dc),
  ("sha1-checked, det off", sha1dc_unchecked),
  ("aws-lc hardware", hardware),
  ("hardware + sha1dc check", hardware_with_sha1dc_check),
];

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

/// Parses an optional positive integer OS argument, falling back to `default` when omitted.
fn parse_optional_positive_usize(
  label: &str,
  raw: Option<&OsStr>,
  default: usize,
) -> Result<usize, String> {
  match raw {
    Some(value) => parse_positive_usize(label, decode_utf8_arg(value)?),
    None => Ok(default),
  }
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

/// Validates that a round count fits within Rust's `isize::MAX` `Vec<f64>` allocation layout cap.
fn validate_rounds_layout(rounds: usize) -> Result<usize, String> {
  let fits = rounds
    .checked_mul(std::mem::size_of::<f64>())
    .is_some_and(|bytes| isize::try_from(bytes).is_ok());
  if !fits {
    return Err(format!("rounds `{rounds}` exceeds maximum allocation size"));
  }
  Ok(rounds)
}

/// Parses optional `[<corpus MiB> [<rounds>]]` OS arguments without panicking.
fn parse_args(arguments: &[OsString]) -> Result<Config, String> {
  if let Some(extra) = arguments.get(2) {
    return Err(format!("unexpected argument `{}`", extra.to_string_lossy()));
  }
  let mib = parse_optional_positive_usize(
    "corpus MiB",
    arguments.first().map(OsString::as_os_str),
    DEFAULT_MIB,
  )?;
  let corpus_bytes = corpus_bytes_from_mib(mib)?;
  let rounds = parse_optional_positive_usize(
    "rounds",
    arguments.get(1).map(OsString::as_os_str),
    DEFAULT_ROUNDS,
  )?;
  let rounds = validate_rounds_layout(rounds)?;
  Ok(Config {
    mib,
    corpus_bytes,
    rounds,
  })
}

/// Fallibly pre-allocates one `rounds`-sample rate buffer per hashing way before output or work starts.
fn allocate_rates(rounds: usize) -> Result<[Vec<f64>; WAY_COUNT], String> {
  let mut rates = std::array::from_fn(|_| Vec::new());
  for way_rates in &mut rates {
    way_rates
      .try_reserve_exact(rounds)
      .map_err(|err| format!("cannot allocate {rounds} rate samples: {err}"))?;
  }
  Ok(rates)
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

/// Fallibly pre-allocates the rate buffers and corpus before printing any output or starting work.
fn prepare_buffers(config: &Config) -> Result<(Vec<u8>, [Vec<f64>; WAY_COUNT]), String> {
  let rates = allocate_rates(config.rounds)?;
  let corpus = build_corpus(config.corpus_bytes)?;
  Ok((corpus, rates))
}

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

fn sha1dc_checked(piece: &[u8]) -> [u8; 20] {
  sha1dc(true, piece)
}

fn sha1dc_unchecked(piece: &[u8]) -> [u8; 20] {
  sha1dc(false, piece)
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

fn hardware_with_sha1dc_check(piece: &[u8]) -> [u8; 20] {
  let fast = hardware(piece);
  assert_eq!(fast, sha1dc(true, piece));
  fast
}

fn time_way(
  pieces: &[&[u8]],
  reference: &[[u8; 20]],
  corpus_len: usize,
  way: fn(&[u8]) -> [u8; 20],
) -> f64 {
  let started = Instant::now();
  for (piece, expected) in pieces.iter().zip(reference) {
    assert_eq!(way(piece), *expected);
  }
  corpus_len as f64 / started.elapsed().as_secs_f64() / 1e6
}

fn measure_piece_size(
  corpus: &[u8],
  piece_len: usize,
  rounds: usize,
  rates: &mut [Vec<f64>; WAY_COUNT],
) {
  for way_rates in rates.iter_mut() {
    way_rates.clear();
  }
  let pieces: Vec<&[u8]> = corpus.chunks(piece_len).collect();
  let reference: Vec<[u8; 20]> = pieces.iter().map(|piece| sha1dc(true, piece)).collect();
  for _ in 0..rounds {
    for ((_, way), way_rates) in WAYS.iter().zip(rates.iter_mut()) {
      way_rates.push(time_way(&pieces, &reference, corpus.len(), *way));
    }
  }
}

fn write_way_row(
  stdout: &mut impl std::io::Write,
  name: &str,
  way_rates: &[f64],
) -> std::io::Result<()> {
  write!(stdout, "  {name:<26}")?;
  for rate in way_rates {
    write!(stdout, " {rate:.0}")?;
  }
  writeln!(stdout)
}

fn write_piece_report(
  stdout: &mut impl std::io::Write,
  piece_len: usize,
  rates: &[Vec<f64>; WAY_COUNT],
) -> std::io::Result<()> {
  writeln!(stdout, "pieces of {} KiB:", piece_len / 1024)?;
  for ((name, _), way_rates) in WAYS.iter().zip(rates) {
    write_way_row(stdout, name, way_rates)?;
  }
  Ok(())
}

/// Direct stdout writer holding the stdout lock and writing through borrowed fd 1 on Unix so
/// `EBADF` on read-only or unwritable stdout surfaces without duplicating a descriptor.
struct DirectStdout {
  inner: std::io::StdoutLock<'static>,
}

impl DirectStdout {
  /// Locks process stdout for direct writes without allocating another file descriptor.
  fn open() -> Self {
    Self {
      inner: std::io::stdout().lock(),
    }
  }
}

impl std::io::Write for DirectStdout {
  #[cfg(unix)]
  fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
    rustix::io::write(&self.inner, buf).map_err(std::io::Error::from)
  }

  #[cfg(not(unix))]
  fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
    self.inner.write(buf)
  }

  #[cfg(unix)]
  fn flush(&mut self) -> std::io::Result<()> {
    Ok(())
  }

  #[cfg(not(unix))]
  fn flush(&mut self) -> std::io::Result<()> {
    self.inner.flush()
  }
}

fn run_benchmark(
  config: &Config,
  corpus: &[u8],
  rates: &mut [Vec<f64>; WAY_COUNT],
) -> std::io::Result<()> {
  let mut stdout = std::io::LineWriter::new(DirectStdout::open());
  writeln!(
    stdout,
    "corpus {} MiB; rounds {}; MB/s of each way, every round",
    config.mib, config.rounds
  )?;
  stdout.flush()?;
  for piece_len in PIECES {
    measure_piece_size(corpus, piece_len, config.rounds, rates);
    write_piece_report(&mut stdout, piece_len, rates)?;
    stdout.flush()?;
  }
  Ok(())
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
  let (corpus, mut rates) = match prepare_buffers(&config) {
    Ok(buffers) => buffers,
    Err(message) => {
      let _ = writeln!(std::io::stderr(), "sha1_ab: {message}");
      return ExitCode::from(EXIT_USAGE);
    }
  };
  if let Err(err) = run_benchmark(&config, &corpus, &mut rates) {
    let _ = writeln!(std::io::stderr(), "sha1_ab: cannot write output: {err}");
    return ExitCode::from(EXIT_OUTPUT);
  }
  ExitCode::SUCCESS
}
