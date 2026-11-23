//! A-809's leftovers measured (main's order of 2026-11-21 04:11): what a path's layered anchor costs as its history grows.
//!
//! - (a) A read of an earlier value against the layers over it: a 16 KiB file's same 64 bytes overwritten `m` times,
//!   then version 1 read whole (`content_ref_at`), which resolves the anchor through every layer over that range.
//! - (a') The same read after `m` changes alternating overwrites and inserts: each insert a shift the read carries the
//!   range through.
//! - (b) A change's cost against its path's history, for three streams over a 16 KiB file: overwrites alone (the layered
//!   path), overwrites alternating with inserts (each insert resolves the layers and re-anchors in full), and an SDK
//!   edit's same-length replacement, a delete and an insert at one offset.
//!
//! Prints the fastest of the rounds for each point. Nothing is folded, so every version is history, as when a reader
//! pins the floor.
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
/// Shape: rounds a point; the fastest is reported.
const ROUNDS: usize = 5;
/// Shape: the changes a stream's point times, after its history reaches the point's length.
const TIMED: u64 = 200;

/// The kinds of change a stream makes.
#[derive(Clone, Copy)]
enum Change {
  Overwrite,
  Insert,
  Replace,
}

/// An increment based at `base`, identity `n`, making `change` at `at`.
fn increment(base: u64, n: u64, change: Change, at: u64) -> Increment {
  let mut doc = OpsDoc::default();
  let path = doc.paths.intern(PATH);
  let op = |kind, src| Op {
    kind,
    flags: 0,
    path,
    at,
    len: SLOT,
    src,
  };
  doc.ops = match change {
    Change::Overwrite => vec![op(OpKind::Overwrite, 0)],
    Change::Insert => vec![op(OpKind::Insert, 0)],
    Change::Replace => vec![op(OpKind::Delete, 0), op(OpKind::Insert, 0)],
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

/// A green holding the file alone, at version 0.
fn green() -> Green {
  Green::with_origin(Origin {
    files: vec![(PATH.to_owned(), (0..LEN).map(|n| (n % 241) as u8).collect())],
    ..Origin::default()
  })
}

/// Submits `change` at `at` as the next version, expecting it accepted.
fn submit(green: &mut Green, change: Change, at: u64) {
  let head = green.head();
  let outcome = green.submit(&increment(head, head + 1, change, at));
  assert_eq!(outcome, Outcome::Accepted { version: head + 1 });
}

/// (a): the fastest read of version 1 whole after `m` overwrites of one range.
fn read_after(m: u64) -> (u128, usize) {
  let mut best = u128::MAX;
  let mut len = 0;
  for _ in 0..ROUNDS {
    let mut green = green();
    for _ in 0..m {
      submit(&mut green, Change::Overwrite, 4096);
    }
    let started = Instant::now();
    let bytes = green.content_ref_at_bytes(PATH, 1).unwrap();
    best = best.min(started.elapsed().as_nanos());
    len = bytes.len();
  }
  (best, len)
}

/// (a'): the fastest read of version 1 whole after `m` changes alternating an overwrite and an insert, each insert a
/// shift a read carries the range through (A-803's leftover (b)).
fn read_after_shifts(m: u64) -> u128 {
  let mut best = u128::MAX;
  for _ in 0..ROUNDS {
    let mut green = green();
    for n in 0..m {
      let change = if n % 2 == 0 {
        Change::Overwrite
      } else {
        Change::Insert
      };
      submit(&mut green, change, (n * 97) % 8192);
    }
    let started = Instant::now();
    let bytes = green.content_ref_at_bytes(PATH, 1).unwrap();
    best = best.min(started.elapsed().as_nanos());
    assert!(!bytes.is_empty());
  }
  best
}

/// (b): the fastest mean submit, in ns, of `TIMED` changes made once the path's history holds `history` versions, the
/// stream `stream` naming each version's change; and the values those changes walked a change.
fn change_cost(history: u64, stream: fn(u64) -> Change) -> (u128, f64) {
  let mut best = u128::MAX;
  let mut walked = 0.0;
  for _ in 0..ROUNDS {
    let mut green = green();
    for n in 0..history {
      submit(&mut green, stream(n), (n * 97) % 8192);
    }
    let before = green.history_reanchored_values();
    let started = Instant::now();
    for n in history..history + TIMED {
      submit(&mut green, stream(n), (n * 97) % 8192);
    }
    let mean = started.elapsed().as_nanos() / u128::from(TIMED);
    if mean < best {
      best = mean;
      walked = (green.history_reanchored_values() - before) as f64 / TIMED as f64;
    }
  }
  (best, walked)
}

/// A stream of changes: its name, and the change it makes at each step.
type Stream = (&'static str, fn(u64) -> Change);

fn main() {
  for m in [10u64, 100, 1_000, 10_000] {
    let (ns, len) = read_after(m);
    println!("a\tread_version_1_after_overwrites\t{m}\tns\t{ns}\tbytes\t{len}");
  }
  for m in [10u64, 100, 1_000, 10_000] {
    println!(
      "a\tread_version_1_after_alternating\t{m}\tns\t{}",
      read_after_shifts(m)
    );
  }
  let streams: [Stream; 3] = [
    ("overwrites", |_| Change::Overwrite),
    ("overwrite_insert_alternating", |n| {
      if n % 2 == 0 {
        Change::Overwrite
      } else {
        Change::Insert
      }
    }),
    ("sdk_replace", |_| Change::Replace),
  ];
  for (name, stream) in streams {
    for history in [100u64, 1_000, 5_000] {
      let (ns, walked) = change_cost(history, stream);
      println!(
        "b\t{name}\thistory\t{history}\tns_per_change\t{ns}\tvalues_walked_per_change\t{walked:.1}"
      );
    }
  }
}
