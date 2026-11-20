//! The A/B that settled how a whole-file rewrite is recovered as the edit it was (D-O17, A-776; `src/refine.rs`).
//!
//! Corpus: this crate's own source files (real Rust, embedded at build time). Workloads, each a pair of agents saving
//! whole files from one base, the second judged after the first by the real green ([`Green::submit`]):
//! - `lines`: the two change one token each on two different lines (expected: both accepted);
//! - `tokens`: the two change two different word tokens of one line (expected: both accepted);
//! - `format`: one re-indents every line (a formatter pass), the other changes one token (expected: both accepted);
//! - `clash`: the two change the same token differently (expected: a conflict; an accept here is a silent merge).
//!
//! Arms: `whole` (before: one delete and one insert of the file), `trim` (the common prefix and suffix trimmed, one
//! range), and Myers at `byte`, `token` and `line` grain, each anchored on units unique to both files (patience) when
//! Myers alone runs out of its budget (`src/refine.rs`). Line-anchored arms (patience, histogram) answer at line grain,
//! which `myers/line` measures: two edits to one line conflict. For each arm and workload: the conflict rate, the rate the
//! green's file after both differs from the oracle (both edits applied to the base, or the first alone on a conflict),
//! and the refinement's cost a save in µs per KiB (median and p99 over the saves; the fastest of `ROUNDS` interleaved
//! rounds is the median's, every round shown) and its scratch high-water.
//!
//! usage: `cargo run --release -p slates-merge --example rewrite_ab`
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::print_stdout,
  clippy::indexing_slicing,
  clippy::arithmetic_side_effects,
  clippy::cast_possible_truncation,
  clippy::cast_precision_loss
)]

use slates_merge::engine::{Green, Increment, Outcome};
use slates_merge::ops_doc::{Op, OpsDoc};
use slates_merge::origin::Origin;
use slates_merge::refine::{Grain, Hunk, Refiner, ops_of};
use std::collections::HashMap;

/// The corpus: real source files of this crate.
const CORPUS: [&str; 6] = [
  include_str!("../src/engine.rs"),
  include_str!("../src/verdict.rs"),
  include_str!("../src/derive.rs"),
  include_str!("../src/increment.rs"),
  include_str!("../src/push.rs"),
  include_str!("../src/history.rs"),
];
/// Shape: pairs a workload a corpus file.
const PAIRS: usize = 40;
/// Shape: interleaved rounds of the timing.
const ROUNDS: usize = 5;
const PATH: &str = "src/file.rs";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Arm {
  Whole,
  Trim,
  Myers(Grain),
}

const ARMS: [Arm; 5] = [
  Arm::Whole,
  Arm::Trim,
  Arm::Myers(Grain::Byte),
  Arm::Myers(Grain::Token),
  Arm::Myers(Grain::Line),
];

fn name(arm: Arm) -> &'static str {
  match arm {
    Arm::Whole => "whole",
    Arm::Trim => "trim",
    Arm::Myers(Grain::Byte) => "myers/byte",
    Arm::Myers(Grain::Token) => "myers/token",
    Arm::Myers(Grain::Line) => "myers/line",
  }
}

/// A deterministic generator (xorshift64*).
struct Rng(u64);
impl Rng {
  fn next(&mut self) -> u64 {
    self.0 ^= self.0 >> 12;
    self.0 ^= self.0 << 25;
    self.0 ^= self.0 >> 27;
    self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
  }
  fn below(&mut self, n: usize) -> usize {
    (self.next() % n.max(1) as u64) as usize
  }
}

/// The word tokens of `text`: (start, end).
fn words(text: &str, from: usize, to: usize) -> Vec<(usize, usize)> {
  let bytes = text.as_bytes();
  let mut out = Vec::new();
  let mut at = from;
  while at < to {
    if bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_' {
      let start = at;
      while at < to && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_') {
        at += 1;
      }
      out.push((start, at));
    } else {
      at += 1;
    }
  }
  out
}

fn lines(text: &str) -> Vec<(usize, usize)> {
  let mut out = Vec::new();
  let mut start = 0;
  for (at, byte) in text.bytes().enumerate() {
    if byte == b'\n' {
      out.push((start, at));
      start = at + 1;
    }
  }
  out
}

/// The bytes `[start, end)` of `text`, at character boundaries (the corpus' tokens are ASCII).
fn cut(text: &str, (start, end): (usize, usize)) -> &str {
  text.get(start..end).unwrap()
}

/// `text` with the bytes `[start, end)` replaced by `with`.
fn splice(text: &str, (start, end): (usize, usize), with: &str) -> String {
  format!(
    "{}{}{}",
    text.get(..start).unwrap(),
    with,
    text.get(end..).unwrap()
  )
}

/// One workload pair: the base, the two agents' files, and the oracle (`None`: the second must conflict).
struct Pair {
  base: String,
  first: String,
  second: String,
  oracle: Option<String>,
}

fn pairs(workload: &str, base: &str, rng: &mut Rng) -> Vec<Pair> {
  let all_lines = lines(base);
  let worded: Vec<(usize, usize)> = all_lines
    .iter()
    .copied()
    .filter(|(s, e)| words(base, *s, *e).len() >= 2)
    .collect();
  let mut out = Vec::new();
  while out.len() < PAIRS {
    let pair = match workload {
      "lines" => {
        let a = worded[rng.below(worded.len())];
        let b = worded[rng.below(worded.len())];
        if a == b {
          continue;
        }
        let ta = words(base, a.0, a.1)[0];
        let tb = words(base, b.0, b.1)[0];
        let (lo, hi) = if ta.0 < tb.0 { (ta, tb) } else { (tb, ta) };
        let first = splice(base, ta, &format!("{}_a", cut(base, ta)));
        let second = splice(base, tb, &format!("{}_b", cut(base, tb)));
        let lo_new = if lo == ta { "_a" } else { "_b" };
        let hi_new = if hi == ta { "_a" } else { "_b" };
        let both = splice(
          &splice(base, hi, &format!("{}{hi_new}", cut(base, hi))),
          lo,
          &format!("{}{lo_new}", cut(base, lo)),
        );
        Pair {
          base: base.into(),
          first,
          second,
          oracle: Some(both),
        }
      }
      "tokens" => {
        let line = worded[rng.below(worded.len())];
        let ws = words(base, line.0, line.1);
        let i = rng.below(ws.len());
        let mut j = rng.below(ws.len());
        if i == j {
          j = (i + 1) % ws.len();
        }
        let (lo, hi) = if ws[i].0 < ws[j].0 {
          (ws[i], ws[j])
        } else {
          (ws[j], ws[i])
        };
        let first = splice(base, lo, "alpha");
        let second = splice(base, hi, "beta");
        let both = splice(&splice(base, hi, "beta"), lo, "alpha");
        Pair {
          base: base.into(),
          first,
          second,
          oracle: Some(both),
        }
      }
      "format" => {
        // The formatter re-indents every line from two-space to four-space indentation.
        let reindent = |text: &str| -> String {
          text
            .split_inclusive('\n')
            .map(|l| {
              let lead = l.len() - l.trim_start_matches(' ').len();
              format!("{}{}", " ".repeat(lead * 2), l.get(lead..).unwrap())
            })
            .collect()
        };
        let line = worded[rng.below(worded.len())];
        let ws = words(base, line.0, line.1);
        let t = ws[rng.below(ws.len())];
        let second = splice(base, t, "renamed_token");
        let first = reindent(base);
        let both = reindent(&second);
        Pair {
          base: base.into(),
          first,
          second,
          oracle: Some(both),
        }
      }
      _ => {
        let line = worded[rng.below(worded.len())];
        let ws = words(base, line.0, line.1);
        let t = ws[rng.below(ws.len())];
        let first = splice(base, t, &format!("{}_a", cut(base, t)));
        let second = splice(base, t, &format!("x{}", cut(base, t)));
        Pair {
          base: base.into(),
          first,
          second,
          oracle: None,
        }
      }
    };
    out.push(pair);
  }
  out
}

/// The hunks `arm` recovers for one save.
fn hunks_of(arm: Arm, base: &[u8], post: &[u8], refiner: &mut Refiner) -> Vec<Hunk> {
  match arm {
    Arm::Whole => vec![Hunk {
      base_at: 0,
      base_len: base.len() as u64,
      post_at: 0,
      post_len: post.len() as u64,
    }],
    Arm::Trim => refiner
      .refine_bounded(base, post, Grain::Token, Some(0))
      .0
      .to_vec(),
    Arm::Myers(grain) => refiner.refine_grain(base, post, grain).0.to_vec(),
  }
}

fn increment(base: &[u8], post: &[u8], hunks: &[Hunk], id: u8) -> Increment {
  let mut doc = OpsDoc::default();
  let path = doc.paths.intern(PATH);
  let mut ops: Vec<Op> = Vec::new();
  ops_of(
    hunks,
    (base.len() as u64, post.len() as u64),
    path,
    &mut ops,
  );
  doc.ops = ops;
  doc.canonicalize();
  Increment {
    id: [id; 32],
    base: 0,
    doc,
    post_state: post.to_vec(),
    evidence: Vec::new(),
    named: Default::default(),
  }
}

#[derive(Default)]
struct Tally {
  pairs: usize,
  conflicts: usize,
  wrong: usize,
  hunks: usize,
}

/// One arm's correctness on one workload's pairs: each pair's two saves judged by the real green, the second after the
/// first; conflicts, wrong files and the ranges the arm recovered, counted.
fn tally_of(arm: Arm, set: &[Pair]) -> Tally {
  let mut tally = Tally::default();
  let mut refiner = Refiner::new();
  for pair in set {
    let (b, f, s) = (
      pair.base.as_bytes(),
      pair.first.as_bytes(),
      pair.second.as_bytes(),
    );
    let h1 = hunks_of(arm, b, f, &mut refiner);
    let h2 = hunks_of(arm, b, s, &mut refiner);
    tally.hunks += h1.len() + h2.len();
    let mut green = Green::with_origin(Origin {
      files: vec![(PATH.into(), b.to_vec())],
      ..Origin::default()
    });
    let one = green.submit(&increment(b, f, &h1, 1));
    assert!(
      matches!(one, Outcome::Accepted { .. }),
      "{arm:?} first: {one:?}"
    );
    let two = green.submit(&increment(b, s, &h2, 2));
    let conflicted = matches!(two, Outcome::Conflict { .. });
    tally.pairs += 1;
    tally.conflicts += usize::from(conflicted);
    let content = green.content(PATH).unwrap_or_default();
    let expect: &[u8] = match (&pair.oracle, conflicted) {
      (Some(both), false) => both.as_bytes(),
      _ => f,
    };
    // On `clash` an accept is wrong whatever the file holds; on the rest a conflict is counted, not wrong.
    let wrong = if pair.oracle.is_none() {
      !conflicted
    } else {
      content != expect
    };
    tally.wrong += usize::from(wrong);
  }
  tally
}

fn main() {
  let only: Option<String> = std::env::args().nth(1);
  let arms: Vec<Arm> = ARMS
    .iter()
    .copied()
    .filter(|a| only.as_deref().is_none_or(|o| o == name(*a)))
    .collect();
  let workloads = ["lines", "tokens", "format", "clash"];
  let mut sets: Vec<(&str, Vec<Pair>)> = Vec::new();
  for workload in workloads {
    let mut rng = Rng(0x5eed_0000 ^ workload.len() as u64);
    let mut all = Vec::new();
    for file in CORPUS {
      all.extend(pairs(workload, file, &mut rng));
    }
    sets.push((workload, all));
  }
  println!(
    "corpus: {} files, {} KiB; {} pairs a workload",
    CORPUS.len(),
    CORPUS.iter().map(|f| f.len()).sum::<usize>() / 1024,
    PAIRS * CORPUS.len()
  );
  // Correctness and conflict rates.
  println!(
    "\n{:<15} {:>8} {:>16} {:>16} {:>8}",
    "arm", "workload", "conflicts", "wrong file", "ranges"
  );
  for arm in arms.iter().copied() {
    for (workload, set) in &sets {
      let tally = tally_of(arm, set);
      println!(
        "{:<15} {:>8} {:>9}/{:<6} {:>9}/{:<6} {:>8}",
        name(arm),
        workload,
        tally.conflicts,
        tally.pairs,
        tally.wrong,
        tally.pairs,
        tally.hunks
      );
    }
  }
  // Cost a save: every save of every workload, per arm, interleaved by round.
  let saves: Vec<(&[u8], &[u8])> = sets
    .iter()
    .flat_map(|(_, set)| {
      set.iter().flat_map(|p| {
        [
          (p.base.as_bytes(), p.first.as_bytes()),
          (p.base.as_bytes(), p.second.as_bytes()),
        ]
      })
    })
    .collect();
  println!(
    "\n{:<15} {:>12} {:>12} {:>12}  rounds (median µs/KiB)",
    "arm", "median µs/KiB", "p99 µs/KiB", "scratch KiB"
  );
  let mut rounds: HashMap<&str, Vec<(f64, f64)>> = HashMap::new();
  let mut scratch: HashMap<&str, usize> = HashMap::new();
  for _ in 0..ROUNDS {
    for arm in arms.iter().copied() {
      let mut refiner = Refiner::new();
      let mut per_kib: Vec<f64> = Vec::with_capacity(saves.len());
      for (base, post) in &saves {
        let started = std::time::Instant::now();
        let hunks = hunks_of(arm, base, post, &mut refiner);
        let elapsed = started.elapsed().as_nanos() as f64 / 1_000.0;
        std::hint::black_box(hunks);
        per_kib.push(elapsed / (base.len().max(post.len()) as f64 / 1024.0));
      }
      per_kib.sort_by(|a, b| a.partial_cmp(b).unwrap());
      let median = per_kib[per_kib.len() / 2];
      let p99 = per_kib[per_kib.len() * 99 / 100];
      rounds.entry(name(arm)).or_default().push((median, p99));
      let high = scratch.entry(name(arm)).or_default();
      *high = (*high).max(refiner.scratch_bytes());
    }
  }
  for arm in arms.iter().copied() {
    let all = &rounds[name(arm)];
    let best = all
      .iter()
      .copied()
      .fold((f64::MAX, f64::MAX), |b, r| if r.0 < b.0 { r } else { b });
    let shown: Vec<String> = all.iter().map(|r| format!("{:.3}", r.0)).collect();
    println!(
      "{:<15} {:>12.3} {:>12.3} {:>12.1}  {}",
      name(arm),
      best.0,
      best.1,
      scratch[name(arm)] as f64 / 1024.0,
      shown.join(" ")
    );
  }
}
