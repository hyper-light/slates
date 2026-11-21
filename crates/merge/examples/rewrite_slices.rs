//! The A/B of a whole-file rewrite's refinement in one step against the same refinement sliced under a turn's deadline
//! (A-787; `src/refine.rs` "Sliced"): the time a save takes and the longest step it holds the shard for.
//!
//! Cases: every source file of this crate re-indented with every hundredth line's first word changed (a formatter
//! and a codemod at once, the heaviest path), and the same shape generated at 1, 4 and 12 MiB. Each case is refined
//! `ROUNDS` times, the arms interleaved; the fastest round is reported and every round shown.
//! - `whole`: `Refiner::refine`, the refinement in one call. Built at a tree before A-787 it is one step, so its time
//!   is its longest step.
//! - `sliced` (A-787's tree only): `Refiner::refine_paced` driven by a `Pace` on the real clock, a turn ending
//!   `TURN_NS` after it began. Reported: its total, its turns and its longest unit (`Pace::longest_unit_ns`), the most a
//!   step runs past its deadline.
//!
//! usage: `cargo run --release -p slates-merge --example rewrite_slices`
#![allow(
  clippy::unwrap_used,
  clippy::print_stdout,
  clippy::indexing_slicing,
  clippy::arithmetic_side_effects,
  clippy::cast_possible_truncation,
  clippy::cast_precision_loss
)]

use std::sync::OnceLock;
use std::time::Instant;

use slates_merge::refine::Refiner;

/// The corpus: real source files of this crate.
const CORPUS: [(&str, &str); 6] = [
  ("engine.rs", include_str!("../src/engine.rs")),
  ("verdict.rs", include_str!("../src/verdict.rs")),
  ("derive.rs", include_str!("../src/derive.rs")),
  ("increment.rs", include_str!("../src/increment.rs")),
  ("push.rs", include_str!("../src/push.rs")),
  ("history.rs", include_str!("../src/history.rs")),
];
/// Shape: rounds a case and arm.
const ROUNDS: usize = 5;
/// Shape: a sliced turn's budget, 250 µs: half of a 500 µs quantum, the order of the shard's step quantum.
const TURN_NS: u64 = 250_000;

/// The real clock, nanoseconds since the first read.
fn now_ns() -> u64 {
  static START: OnceLock<Instant> = OnceLock::new();
  START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// `text` re-indented (each leading blank doubled) with every hundredth line's first word suffixed.
fn reformat(text: &str) -> String {
  text
    .split_inclusive('\n')
    .enumerate()
    .map(|(n, line)| {
      let lead = line.len() - line.trim_start_matches(' ').len();
      let body = &line[lead..];
      let body = if n % 100 == 0 {
        let word = body
          .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
          .unwrap_or(0);
        format!("{}_x{}", &body[..word], &body[word..])
      } else {
        body.to_owned()
      };
      format!("{}{}", " ".repeat(lead * 2), body)
    })
    .collect()
}

fn generated(bytes: usize) -> String {
  let mut out = String::new();
  let mut n = 0usize;
  while out.len() < bytes {
    out.push_str(&format!(
      "  let value_{n} = compute({n}, other_{});\n",
      n % 97
    ));
    n += 1;
  }
  out
}

fn main() {
  let mut cases: Vec<(String, Vec<u8>, Vec<u8>)> = CORPUS
    .iter()
    .map(|(name, text)| {
      (
        (*name).to_owned(),
        text.as_bytes().to_vec(),
        reformat(text).into_bytes(),
      )
    })
    .collect();
  for mib in [1usize, 4, 12] {
    let base = generated(mib << 20);
    let post = reformat(&base);
    cases.push((
      format!("generated {mib} MiB"),
      base.into_bytes(),
      post.into_bytes(),
    ));
  }
  println!("{:<18} {:>9}  {:>12}  rounds ms", "case", "KiB", "whole ms");
  let mut sliced_rows = Vec::new();
  for (name, base, post) in &cases {
    let mut whole = Vec::new();
    let mut sliced = Vec::new();
    for _ in 0..ROUNDS {
      let mut refiner = Refiner::new();
      let began = Instant::now();
      let (hunks, _) = refiner.refine(base, post);
      std::hint::black_box(hunks.len());
      whole.push(began.elapsed().as_secs_f64() * 1e3);
      sliced.push(sliced_round(base, post));
    }
    let shown: Vec<String> = whole.iter().map(|t| format!("{t:.2}")).collect();
    let best = whole.iter().copied().fold(f64::MAX, f64::min);
    println!(
      "{name:<18} {:>9}  {best:>12.2}  {}",
      base.len().max(post.len()) / 1024,
      shown.join(" ")
    );
    sliced_rows.push((name.clone(), sliced));
  }
  print_sliced(&sliced_rows);
}

// SLICED-BEGIN (removed when built at a tree before A-787)
/// One sliced refinement: total ms, turns, longest unit µs.
fn sliced_round(base: &[u8], post: &[u8]) -> Option<(f64, u64, f64)> {
  let mut refiner = Refiner::new();
  let ticks = slates_merge::pace::Ticks;
  let pace = slates_merge::pace::Pace::new(now_ns);
  let began = Instant::now();
  let mut turns = 0u64;
  let mut work = Box::pin(refiner.refine_paced(
    (base, post),
    (slates_merge::refine::Grain::Token, None),
    &ticks,
  ));
  loop {
    turns += 1;
    pace.turn(now_ns() + TURN_NS);
    if pace.drive(work.as_mut()).is_ready() {
      break;
    }
  }
  Some((
    began.elapsed().as_secs_f64() * 1e3,
    turns,
    pace.longest_unit_ns() as f64 / 1e3,
  ))
}

fn print_sliced(rows: &[(String, Vec<Option<(f64, u64, f64)>>)]) {
  println!(
    "\n{:<18} {:>12} {:>8} {:>16}  rounds (ms / longest unit µs)",
    "case", "sliced ms", "turns", "longest unit µs"
  );
  for (name, rounds) in rows {
    let rounds: Vec<(f64, u64, f64)> = rounds.iter().flatten().copied().collect();
    let best = rounds
      .iter()
      .copied()
      .fold((f64::MAX, 0, 0.0), |b, r| if r.0 < b.0 { r } else { b });
    let worst_unit = rounds.iter().map(|r| r.2).fold(0.0, f64::max);
    let shown: Vec<String> = rounds
      .iter()
      .map(|r| format!("{:.2}/{:.1}", r.0, r.2))
      .collect();
    println!(
      "{name:<18} {:>12.2} {:>8} {:>16.1}  {}",
      best.0,
      best.1,
      worst_unit,
      shown.join(" ")
    );
  }
}
// SLICED-END
