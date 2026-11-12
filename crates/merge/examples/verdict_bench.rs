//! The verdict's CPU per submit, by the size of the file an edit lands in (§4.16 "The verdict"), for the three paths a
//! slot replacement takes: an edit that overlaps an intervening change with other bytes (a conflict, found by the
//! whole-file identity check), one that overlaps it with the same bytes (converges, found by the same check), and a
//! disjoint edit at the head (accepted: `apply` writes the new file and the history splices it).
//!
//! Each case submits a fresh increment (a new identity, so no cache answers it) to a green holding the measured file
//! and as many small files as the load rung's `kernel/` green holds, so the maps a verdict searches are the rung's size.
//! Every submit's outcome is checked to be the case's, so the path timed is the one named. Prints one line per case
//! and size: the fastest of the rounds' mean nanoseconds a submit.
//!
//! usage: `cargo run --release -p slates-merge --example verdict_bench`
// A benchmark: an unexpected outcome is a failed run, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::print_stdout,
  clippy::cast_possible_truncation
)]

use slates_merge::engine::{Green, Increment, Outcome};
use slates_merge::ops_doc::{Op, OpKind, OpsDoc};
use slates_merge::origin::Origin;

/// Shape: the files of the green, the measured one among them: the load rung's (`kernel/` of linux 7.4-rc1: 754
/// files), ten times it, and a hundred times it (a whole kernel tree is about 90,000), so a cost that follows the
/// green's paths shows apart from one that follows the file or the change.
const FILES: [usize; 3] = [754, 7_540, 75_400];
/// Shape: each other file's bytes, small, so the green's maps hold the rung's paths without its bytes.
const SMALL: usize = 16;
/// Shape: the bytes a slot replacement writes, a harness slot's width.
const SLOT: usize = 16;
/// Shape: the measured file's sizes: a page, the rung's mean file (16,171 KiB over 754 files, about 21 KiB), 1 MiB.
const SIZES: [usize; 3] = [4 << 10, 21 << 10, 1 << 20];
/// Shape: submits a round.
const SUBMITS: u64 = 2_000;
/// Shape: rounds a case; the fastest round's mean is reported, the noise of a busy host taken out.
const ROUNDS: usize = 7;

/// The measured file's path.
const PATH: &str = "kernel/measured.c";

/// An increment based at `base` that replaces the `SLOT` bytes at `at` of the measured file with `bytes` (a delete and
/// an insert at one offset, as an SDK edit composes), its identity `n`.
fn replace(base: u64, at: u64, bytes: &[u8], n: u64) -> Increment {
  let mut doc = OpsDoc::default();
  let path = doc.paths.intern(PATH);
  let len = bytes.len() as u64;
  doc.ops = vec![
    Op {
      kind: OpKind::Delete,
      flags: 0,
      path,
      at,
      len,
      src: 0,
    },
    Op {
      kind: OpKind::Insert,
      flags: 0,
      path,
      at,
      len,
      src: 0,
    },
  ];
  let mut id = [0u8; 32];
  id[..8].copy_from_slice(&n.to_le_bytes());
  Increment {
    id,
    base,
    doc,
    post_state: bytes.to_vec(),
    evidence: Vec::new(),
  }
}

/// A green holding the measured file of `size` bytes and `paths - 1` small ones, its middle slot then replaced at
/// version 1 with `b'B'` bytes: what every overlapping case is judged against.
fn green(size: usize, paths: usize) -> Green {
  let mut files: Vec<(String, Vec<u8>)> = (1..paths)
    .map(|n| (format!("kernel/file{n:04}.c"), vec![b'.'; SMALL]))
    .collect();
  files.push((
    PATH.to_owned(),
    (0..size).map(|n| (n % 251) as u8).collect(),
  ));
  let mut green = Green::with_origin(Origin {
    files,
    ..Origin::default()
  });
  let outcome = green.submit(&replace(0, middle(size), &[b'B'; SLOT], u64::MAX));
  assert!(
    matches!(outcome, Outcome::Accepted { version: 1 }),
    "{outcome:?}"
  );
  green
}

/// The offset of the measured file's middle slot.
fn middle(size: usize) -> u64 {
  (size / 2) as u64
}

/// The mean nanoseconds of `SUBMITS` submits `make` builds (from the submit's number), each checked by `expect`, over
/// a fresh green of the measured file's `size` among `paths` files: the fastest of `ROUNDS` rounds.
fn time(
  (size, paths): (usize, usize),
  make: impl Fn(&Green, u64) -> Increment,
  expect: impl Fn(&Outcome) -> bool,
) -> u128 {
  let mut best = u128::MAX;
  for _ in 0..ROUNDS {
    let mut green = green(size, paths);
    let increments: Vec<Increment> = (0..SUBMITS).map(|n| make(&green, n)).collect();
    let mut elapsed = 0u128;
    for increment in &increments {
      let started = std::time::Instant::now();
      let outcome = green.submit(increment);
      elapsed += started.elapsed().as_nanos();
      assert!(expect(&outcome), "{outcome:?}");
    }
    best = best.min(elapsed / u128::from(SUBMITS));
  }
  best
}

fn main() {
  for paths in FILES {
    for size in SIZES {
      let shape = (size, paths);
      let conflict = time(
        shape,
        |_, n| replace(0, middle(size), &[b'C'; SLOT], n),
        |outcome| matches!(outcome, Outcome::Conflict { .. }),
      );
      let converge = time(
        shape,
        |_, n| replace(0, middle(size), &[b'B'; SLOT], n),
        |outcome| matches!(outcome, Outcome::Accepted { .. }),
      );
      // Disjoint and at the head the green has when it is judged: a quarter in, each one's base the version before it.
      let accept = time(
        shape,
        |green, n| {
          replace(
            green.head() + n,
            (size / 4) as u64,
            &[b'A' + (n % 26) as u8; SLOT],
            n,
          )
        },
        |outcome| matches!(outcome, Outcome::Accepted { .. }),
      );
      println!(
        "verdict\tpaths\t{paths}\tfile_bytes\t{size}\tconflict_ns\t{conflict}\tconverge_ns\t{converge}\taccept_ns\t{accept}"
      );
    }
  }
}
