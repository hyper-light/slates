//! A-92 piece 5's measurement (seal.md §8, "at rest is idle RAM"): what opening one 4 KiB block of idle sealed RAM
//! costs, the number the decision to seal a volume's idle plaintext waits on (a 4 KiB open at most about 1 µs p99 under
//! load). Three cases, each timed one open at a time: a warm open under a volume's version key (the rule for data
//! overwritten in place), a warm open of a file opener already made, and a cold open that unwraps the data key and
//! checks its commitment first. `cargo run --release -p slates-cluster --example seal_idle_bench`; prints p50, p99, p999
//! and the maximum of each, with the timer's own cost measured the same way.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::print_stdout,
  clippy::disallowed_methods,
  clippy::indexing_slicing
)]

use std::time::Instant;

use hyper_seal::keys::WrappingKey;
use hyper_seal::stream::{FileOpener, FileSealer, VersionKey};

/// Shape: the block size the decision is about (a base page).
const BLOCK: usize = 4096;
/// Shape: timed opens per case: enough that the p999 is a hundred readings deep.
const SAMPLES: usize = 100_000;

fn report(label: &str, mut samples: Vec<u64>) {
  samples.sort_unstable();
  // Nearest rank, in thousandths.
  let at = |permille: usize| samples[(samples.len() - 1) * permille / 1000];
  println!(
    "{label}: p50 {} ns, p99 {} ns, p999 {} ns, max {} ns ({} samples)",
    at(500),
    at(990),
    at(999),
    samples[samples.len() - 1],
    samples.len()
  );
}

fn timed(mut work: impl FnMut()) -> Vec<u64> {
  (0..SAMPLES)
    .map(|_| {
      let start = Instant::now();
      work();
      u64::try_from(start.elapsed().as_nanos()).unwrap()
    })
    .collect()
}

fn main() {
  hyper_seal::lock_keys(16).unwrap();
  let lineage = WrappingKey::generate(1).unwrap();
  report("timer alone", timed(|| {}));

  // A version-keyed block: the volume's key, the object's id, the block's version.
  let (secret, _) = lineage.make_child().unwrap();
  let version_key = VersionKey::new(&secret, [7; 16]).unwrap();
  let plain = vec![0x5a_u8; BLOCK];
  let mut sealed = plain.clone();
  let tag = version_key.seal(42, 0, true, &mut sealed).unwrap();
  let mut scratch = sealed.clone();
  report(
    "warm 4 KiB open, version key",
    timed(|| {
      scratch.copy_from_slice(&sealed);
      version_key.open(42, 0, true, &mut scratch, &tag).unwrap();
    }),
  );

  // A file written once: its header's data key wrapped by the lineage key.
  let (mut sealer, header) = FileSealer::new(&lineage, u32::try_from(BLOCK).unwrap()).unwrap();
  let mut file = plain.clone();
  let file_tag = sealer.seal(&mut file, true).unwrap();
  let opener = FileOpener::new(&lineage, &header).unwrap();
  report(
    "warm 4 KiB open, file opener made",
    timed(|| {
      scratch.copy_from_slice(&file);
      opener.open(0, true, &mut scratch, &file_tag).unwrap();
    }),
  );
  report(
    "cold 4 KiB open (unwrap and commitment, then open)",
    timed(|| {
      scratch.copy_from_slice(&file);
      FileOpener::new(&lineage, &header)
        .unwrap()
        .open(0, true, &mut scratch, &file_tag)
        .unwrap();
    }),
  );
  report(
    "4 KiB copy alone (what a plaintext read does)",
    timed(|| {
      scratch.copy_from_slice(&file);
      std::hint::black_box(&scratch);
    }),
  );
  assert_eq!(scratch.len(), BLOCK);
}
