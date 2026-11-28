//! The radix tree's own cost (`slates_merge::ptrie`, which every `Dim` of a green wraps): get, insert, remove and a fork's
//! first change at 9,000 and 100,000 keys, the A arm of main's A/B of the node arena (A-920, main's ruling of
//! 2026-11-27 23:08: a node's label and child list kept as runs in its arena instead of allocations of their own).
//!
//! - insert: a fresh map takes every key, in a shuffled order.
//! - get: every key read, in another order.
//! - fork + insert: a fork of the full map takes a new value at every key, each change copying the path another holder
//!   still names (path copying).
//! - remove: every key taken out of the full map.
//!
//! Keys are paths of a source tree: a few hundred directories, files under each, so edges split and share prefixes as
//! a repository's do. Every round of every point is printed, then the fastest, in nanoseconds a key.
//!
//! usage: `cargo run --release -p slates-merge --example ptrie_bench`
// A benchmark: an unexpected outcome is a failed run, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::print_stdout,
  clippy::cast_possible_truncation,
  clippy::cast_precision_loss
)]

use std::time::Instant;

use slates_merge::ptrie::{Nodes, Plain, Ptrie};

/// Shape: the map sizes timed: a wide import's revision (A-920's 9,000 files) and a large repository's tree.
const SIZES: [usize; 2] = [9_000, 100_000];
/// Shape: rounds a point; the fastest is reported, every round printed.
const ROUNDS: usize = 7;
/// Shape: the files a directory holds, so a key's directory prefix is shared by that many keys.
const FILES_A_DIR: usize = 30;

/// The `n` keys: `trunk/src/dir{d}/sub{s}/file{f}.c`, spread over directories `FILES_A_DIR` files each.
fn keys(n: usize) -> Vec<Vec<u8>> {
  (0..n)
    .map(|at| {
      let dir = at / FILES_A_DIR;
      format!(
        "trunk/src/dir{:04}/sub{}/file{:03}.c",
        dir / 7,
        dir % 7,
        at % FILES_A_DIR
      )
      .into_bytes()
    })
    .collect()
}

/// `keys` in an order of its own: a fixed multiplicative stride over the indices, so every run sees the same order.
fn shuffled(keys: &[Vec<u8>], stride: usize) -> Vec<Vec<u8>> {
  let n = keys.len();
  (0..n)
    .map(|at| keys[(at * stride + 1) % n].clone())
    .collect()
}

/// A full map of `keys`, each key's value its index.
fn full(nodes: &mut Nodes<u64>, keys: &[Vec<u8>]) -> Ptrie<u64> {
  let mut map = Ptrie::new();
  for (at, key) in keys.iter().enumerate() {
    map.insert(nodes, key, at as u64, &mut Plain);
  }
  map
}

/// Nanoseconds a key of `run`, timed once.
fn timed(n: usize, run: impl FnOnce()) -> f64 {
  let started = Instant::now();
  run();
  started.elapsed().as_nanos() as f64 / n as f64
}

/// Every round of `point` printed, then its fastest.
fn report(n: usize, point: &str, rounds: &[f64]) {
  let listed: Vec<String> = rounds.iter().map(|ns| format!("{ns:.1}")).collect();
  let best = rounds.iter().copied().fold(f64::INFINITY, f64::min);
  println!(
    "ptrie_bench n={n} {point}: best {best:.1} ns a key; rounds {}",
    listed.join(" ")
  );
}

/// One size's four points, `ROUNDS` rounds each, each round on a fresh arena.
fn measured(n: usize) {
  let keys = keys(n);
  let inserted = shuffled(&keys, 7_919);
  let read = shuffled(&keys, 104_729);
  let mut rounds: [Vec<f64>; 4] = Default::default();
  for _ in 0..ROUNDS {
    let mut nodes = Nodes::default();
    let mut map = Ptrie::new();
    rounds[0].push(timed(n, || {
      for (at, key) in inserted.iter().enumerate() {
        map.insert(&mut nodes, key, at as u64, &mut Plain);
      }
    }));
    let mut found = 0u64;
    rounds[1].push(timed(n, || {
      for key in &read {
        found += map.get(&nodes, key).copied().unwrap_or(0);
      }
    }));
    assert!(found > 0, "the reads found nothing");
    let mut fork = map.fork(&mut nodes);
    rounds[2].push(timed(n, || {
      for key in &read {
        fork.insert(&mut nodes, key, u64::MAX, &mut Plain);
      }
    }));
    fork.release(&mut nodes, &mut Plain);
    let mut whole = full(&mut nodes, &keys);
    rounds[3].push(timed(n, || {
      for key in &read {
        whole.remove(&mut nodes, key, &mut Plain);
      }
    }));
    assert!(whole.is_empty(&nodes), "every key removed");
    map.release(&mut nodes, &mut Plain);
    whole.release(&mut nodes, &mut Plain);
  }
  for (point, timed) in ["insert", "get", "fork + insert", "remove"]
    .iter()
    .zip(&rounds)
  {
    report(n, point, timed);
  }
}

fn main() {
  for n in SIZES {
    measured(n);
  }
}
