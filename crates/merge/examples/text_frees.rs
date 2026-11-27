//! Measurement (not a gate): what letting a wide revision's named texts go costs one shard step, a buffer a text against
//! one buffer for them all (A-920 3n, main 01a20182's ruling of 2026-11-26 on the commit's 1 ms unit). A revision of
//! 9,000 files of 1 KiB read its texts into a buffer each, and its commit freed one a unit: the free that emptied the
//! system allocator's region spent about 1 ms in the kernel (debug, macOS, getrusage: 947 and 1,226 µs of system time,
//! one minor fault). The texts held in one buffer instead are one free, a large one (macOS: `vm_deallocate`; glibc: an
//! `munmap` past its mmap threshold), whose kernel share differs by platform. Prints both, over rounds, each round's
//! buffers made and touched first so the frees meet resident pages; every round printed, since glibc's mmap threshold
//! is dynamic and rises after the first free of a large chunk, so the first round and the rest can differ. Run in
//! release: `cargo run --release -p slates-merge --example text_frees`.
// Measurement code: an unwrap here is a failed run.
#![allow(
  clippy::unwrap_used,
  clippy::disallowed_methods,
  clippy::cast_precision_loss,
  clippy::cast_possible_truncation
)]

use std::time::{Duration, Instant};

/// Shape: the files of the wide revision, as A-920's probes import them (`t_c2c`'s `WIDE_REVISION_FILES`).
const TEXTS: usize = 9000;
/// Shape: each file's bytes (`t_c2c`'s `WIDE_TEXT_BYTES`).
const TEXT_BYTES: usize = 1024;
/// Shape: the rounds each way, so a platform's spread shows.
const ROUNDS: usize = 20;

/// The longest single free of `TEXTS` buffers of `TEXT_BYTES` each, freed one at a time in the order they were made.
fn longest_of_many_frees() -> Duration {
  let mut texts: Vec<Vec<u8>> = (0..TEXTS)
    .map(|at| vec![u8::try_from(at % 251).unwrap(); TEXT_BYTES])
    .collect();
  let mut longest = Duration::ZERO;
  while let Some(text) = texts.pop() {
    let began = Instant::now();
    drop(std::hint::black_box(text));
    longest = longest.max(began.elapsed());
  }
  longest
}

/// The one free of a buffer holding `TEXTS` texts of `TEXT_BYTES`.
fn one_large_free() -> Duration {
  let buffer = vec![1u8; TEXTS * TEXT_BYTES];
  let buffer = std::hint::black_box(buffer);
  let began = Instant::now();
  drop(buffer);
  began.elapsed()
}

/// Rounds of each, printed: microseconds in round order, with the median and the most.
fn main() {
  let many: Vec<Duration> = (0..ROUNDS).map(|_| longest_of_many_frees()).collect();
  let one: Vec<Duration> = (0..ROUNDS).map(|_| one_large_free()).collect();
  let median = |rounds: &[Duration]| {
    let mut sorted = rounds.to_vec();
    sorted.sort_unstable();
    sorted.get(ROUNDS / 2).copied().unwrap_or_default()
  };
  let most = |rounds: &[Duration]| rounds.iter().max().copied().unwrap_or_default();
  let micros = |rounds: &[Duration]| rounds.iter().map(|d| d.as_micros()).collect::<Vec<_>>();
  println!(
    "text_frees on {} {}: {TEXTS} texts of {TEXT_BYTES} bytes, {ROUNDS} rounds",
    std::env::consts::OS,
    std::env::consts::ARCH
  );
  println!(
    "  a buffer each, the longest single free a round (us): median {}, most {}, by round {:?}",
    median(&many).as_micros(),
    most(&many).as_micros(),
    micros(&many)
  );
  println!(
    "  one buffer, its one free (us): median {}, most {}, by round {:?}",
    median(&one).as_micros(),
    most(&one).as_micros(),
    micros(&one)
  );
}
