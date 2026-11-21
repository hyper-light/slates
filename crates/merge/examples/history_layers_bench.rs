//! A path's history as it grows (A-809's leftovers, main's order of 2026-11-21 04:11; A-818's A/B, main's conditions of
//! 09:40 and 10:35: "write per change, old-version read and memory per version at m = 10/100/1k/10k ... plus many
//! paths changed once each ... against main and against (i) on the generic leaf, all N shown").
//!
//! - (a) A read of version 1 whole after `m` overwrites of one 64-byte range of a 16 KiB file.
//! - (a') The same read after `m` changes alternating overwrites and inserts.
//! - (b) A change's cost against its path's history of `m` versions, for three streams over a 16 KiB file: overwrites
//!   alone, overwrites alternating with inserts, and an SDK edit's same-length replacement (a delete and an insert at
//!   one offset); with the history's bytes a version.
//! - (c) Many paths changed once each: `m` files of 16 KiB, each overwritten in one 64-byte slot; the history's bytes a
//!   version.
//! - Option (i), runs naming immutable chunks, is the `leaf_chunks_bench` example's (it needs A-818's library).
//!
//! It uses only what main's library had before A-818 (`submit`, `content_ref_at`, `retained_bytes`), so the same file
//! runs on both sides of the A/B.
//!
//! Prints every round of each point (all N), fastest first is not assumed: the reader takes the minimum or the median.
//! Nothing is folded, so every version is history, as when a reader pins the floor.
//!
//! usage: `cargo run --release -p slates-merge --example history_layers_bench`
// A benchmark: an unexpected outcome is a failed run, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::print_stdout,
  clippy::cast_possible_truncation
)]

use std::time::Instant;

use slates_merge::engine::{Green, Increment, Outcome};
use slates_merge::ops_doc::{Op, OpKind, OpsDoc};
use slates_merge::origin::Origin;

/// The measured file's path.
const PATH: &str = "f";
/// Shape: the file's bytes: the load rung's mean file is about 21 KiB.
const LEN: usize = 16 << 10;
/// Shape: the bytes an edit writes, a harness slot's width times four.
const SLOT: u64 = 64;
/// Shape: rounds a point, every one printed.
const ROUNDS: usize = 5;
/// Shape: the changes a stream's point times, after its history reaches the point's length.
const TIMED: u64 = 200;
/// Shape: the history lengths and path counts measured (main's 09:40 condition).
const POINTS: [u64; 4] = [10, 100, 1_000, 10_000];

/// The kinds of change a stream makes.
const OVERWRITE: u8 = 0;
/// See [`OVERWRITE`].
const INSERT: u8 = 1;
/// See [`OVERWRITE`].
const REPLACE: u8 = 2;

/// An increment based at `base`, identity `n`, on `path`, making change `kind` of `SLOT` bytes at `at`.
fn increment(base: u64, n: u64, path: &str, (kind, at): (u8, u64)) -> Increment {
  let mut doc = OpsDoc::default();
  let interned = doc.paths.intern(path);
  let op = |kind| Op {
    kind,
    flags: 0,
    path: interned,
    at,
    len: SLOT,
    src: 0,
  };
  doc.ops = match kind {
    OVERWRITE => vec![op(OpKind::Overwrite)],
    INSERT => vec![op(OpKind::Insert)],
    _ => vec![op(OpKind::Delete), op(OpKind::Insert)],
  };
  let mut id = [0u8; 32];
  id[..8].copy_from_slice(&n.to_le_bytes());
  Increment {
    id,
    base,
    doc,
    post_state: vec![(n % 251) as u8; SLOT as usize],
    evidence: Vec::new(),
    named: Default::default(),
  }
}

/// The first file's bytes.
fn first_file() -> Vec<u8> {
  (0..LEN).map(|n| (n % 241) as u8).collect()
}

/// A green holding `paths` files of the first file's bytes, at version 0.
fn green(paths: u64) -> Green {
  let files = (0..paths)
    .map(|n| (path_of(n, paths), first_file()))
    .collect();
  Green::with_origin(Origin {
    files,
    ..Origin::default()
  })
}

/// Path `n` of `paths`: the one path when there is one.
fn path_of(n: u64, paths: u64) -> String {
  if paths == 1 {
    PATH.to_owned()
  } else {
    format!("d/{n:06}")
  }
}

/// Submits change `change` on `path` as the next version, expecting it accepted.
fn submit(green: &mut Green, path: &str, change: (u8, u64)) {
  let head = green.head();
  let outcome = green.submit(&increment(head, head + 1, path, change));
  assert_eq!(outcome, Outcome::Accepted { version: head + 1 });
}

/// The overwrite stream: one range.
fn same_range(_: u64) -> (u8, u64) {
  (OVERWRITE, 4096)
}

/// The overwrites stream.
fn overwrites(n: u64) -> (u8, u64) {
  (OVERWRITE, (n * 97) % 8192)
}

/// The alternating stream: an overwrite, then an insert.
fn alternating(n: u64) -> (u8, u64) {
  (if n % 2 == 0 { OVERWRITE } else { INSERT }, (n * 97) % 8192)
}

/// The SDK replacement stream.
fn replacing(n: u64) -> (u8, u64) {
  (REPLACE, (n * 97) % 8192)
}

/// Each round's read of version 1 whole after `m` changes of `stream`, in ns.
fn read_after(m: u64, stream: fn(u64) -> (u8, u64)) -> Vec<u128> {
  (0..ROUNDS)
    .map(|_| {
      let mut green = green(1);
      for n in 0..m {
        submit(&mut green, PATH, stream(n));
      }
      let started = Instant::now();
      let bytes = green.content_ref_at(PATH, 1).unwrap();
      let ns = started.elapsed().as_nanos();
      assert_eq!(bytes.len(), LEN);
      ns
    })
    .collect()
}

/// Each round's mean submit, in ns, of `TIMED` changes made once the path's history holds `history` versions; with
/// the last round's history bytes a version.
fn change_cost(history: u64, stream: fn(u64) -> (u8, u64)) -> (Vec<u128>, usize) {
  let mut rounds = Vec::new();
  let mut per_version = 0;
  for _ in 0..ROUNDS {
    let mut green = green(1);
    for n in 0..history {
      submit(&mut green, PATH, stream(n));
    }
    let started = Instant::now();
    for n in history..history + TIMED {
      submit(&mut green, PATH, stream(n));
    }
    rounds.push(started.elapsed().as_nanos() / u128::from(TIMED));
    per_version = green.retained_bytes().history / usize::try_from(history + TIMED).unwrap();
  }
  (rounds, per_version)
}

/// The history bytes a version when `paths` paths are each changed once.
fn many_paths_once(paths: u64) -> usize {
  let mut green = green(paths);
  for n in 0..paths {
    submit(&mut green, &path_of(n, paths), (OVERWRITE, 4096));
  }
  green.retained_bytes().history / usize::try_from(paths).unwrap()
}

/// A row's rounds, tab-separated.
fn all(rounds: &[u128]) -> String {
  rounds
    .iter()
    .map(u128::to_string)
    .collect::<Vec<_>>()
    .join("\t")
}

fn main() {
  for m in POINTS {
    println!(
      "a\tread_version_1_after_overwrites\tm\t{m}\tns\t{}",
      all(&read_after(m, same_range))
    );
    println!(
      "a\tread_version_1_after_alternating\tm\t{m}\tns\t{}",
      all(&read_after(m, alternating))
    );
  }
  let streams: [(&str, fn(u64) -> (u8, u64)); 3] = [
    ("overwrites", overwrites),
    ("overwrite_insert_alternating", alternating),
    ("sdk_replace", replacing),
  ];
  for (name, stream) in streams {
    for m in POINTS {
      let (rounds, per_version) = change_cost(m, stream);
      println!(
        "b\t{name}\thistory\t{m}\tns_per_change\t{}\thistory_bytes_per_version\t{per_version}",
        all(&rounds)
      );
    }
  }
  for m in POINTS {
    println!(
      "c\tmany_paths_once_history_bytes_per_version\tpaths\t{m}\t{}",
      many_paths_once(m)
    );
  }
}
