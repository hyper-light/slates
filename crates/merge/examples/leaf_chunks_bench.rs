//! Option (i) of A-818 (main's condition of 2026-11-21 10:35: the A/B "against (i) on the generic leaf"): a path's
//! versions as persistent piece trees whose runs name immutable chunks (`slates_merge::leaf_chunks_bench`), the rows of
//! `history_layers_bench` that (i) has: the alternating stream's change cost and read of version 1 at m =
//! 10/100/1k/10k, the history's bytes a version, and many paths changed once each. Every round printed.
//!
//! usage: `cargo run --release -p slates-merge --example leaf_chunks_bench`
// A benchmark: an unexpected outcome is a failed run, which is what it should be.
#![allow(clippy::print_stdout)]

use slates_merge::leaf_chunks_bench::{Cost, many_paths_once, one_path};

/// Shape: the file's bytes, as `history_layers_bench`'s.
const LEN: usize = 16 << 10;
/// Shape: the bytes an edit writes, as `history_layers_bench`'s.
const SLOT: u64 = 64;
/// Shape: rounds a point, every one printed.
const ROUNDS: usize = 5;
/// Shape: the changes a point times, as `history_layers_bench`'s.
const TIMED: u64 = 200;
/// Shape: the history lengths and path counts measured.
const POINTS: [u64; 4] = [10, 100, 1_000, 10_000];

/// The alternating stream, as `history_layers_bench`'s: an overwrite, then an insert.
fn alternating(n: u64) -> (u8, u64) {
  (u8::from(n % 2 == 1), (n * 97) % 8192)
}

fn first_file() -> Vec<u8> {
  (0..LEN)
    .map(|n| u8::try_from(n % 241).unwrap_or(0))
    .collect()
}

/// A row's rounds, tab-separated.
fn all(rounds: impl Iterator<Item = u128>) -> String {
  rounds
    .map(|ns| ns.to_string())
    .collect::<Vec<_>>()
    .join("\t")
}

fn main() {
  for m in POINTS {
    let costs: Vec<Cost> = (0..ROUNDS)
      .map(|_| one_path(first_file(), (m, TIMED, SLOT), alternating))
      .collect();
    println!(
      "i\toverwrite_insert_alternating\thistory\t{m}\tns_per_change\t{}",
      all(costs.iter().map(|cost| cost.ns_per_change))
    );
    println!(
      "i\tread_version_1_after_alternating\tm\t{m}\tns\t{}",
      all(costs.iter().map(|cost| cost.read_first_ns))
    );
    let versions = usize::try_from(m + TIMED).unwrap_or(usize::MAX);
    let bytes = costs.last().map_or(0, |cost| cost.history_bytes);
    println!(
      "i\thistory_bytes_per_version\thistory\t{m}\t{}",
      bytes / versions
    );
    let paths = usize::try_from(m).unwrap_or(usize::MAX);
    println!(
      "i\tmany_paths_once_history_bytes_per_version\tpaths\t{m}\t{}",
      many_paths_once(&first_file(), m, (4096, SLOT)) / paths
    );
  }
}
