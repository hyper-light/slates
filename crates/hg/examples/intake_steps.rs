//! How long a push's intake steps take over a real history, by the kind of unit each takes (goal 93: Mercurial's own
//! push held thousands of steps past 100 ms, before and after the index seal's reland; main's ruling of 2026-11-23
//! 16:29: attribute them by kind). A bundle's changegroup is taken into an empty index through the intake as the daemon
//! takes it, cut into segments of `segment bytes`, each segment's record written and timed apart from the step that
//! cut it, its room the push's admission. Each step is timed under the kind of unit it takes (`Intake::unit`). Printed:
//! the wall time; for each kind its steps, its time, its steps past 100 ms and past 1 s and its longest; the segment
//! records' alike; the slowest steps with their kinds; and the fork's rows, seals, most own runs, tiered merges and
//! longest merge in steps, and its own bytes in windows, for the size-tiered rule's bound in bytes.
//!
//! Command: `cargo run --release -p slates-hg --example intake_steps -- <bundle (hg bundle --all -t none-v2)> <window
//! bytes> <room bytes> <segment bytes>`.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::print_stdout,
  clippy::print_stderr,
  clippy::disallowed_methods,
  clippy::cast_precision_loss
)]

use std::collections::BTreeMap;
use std::time::Instant;

use slates_hg::Admission;
use slates_hg::bundle2::Bundle2;
use slates_hg::index::HgIndex;
use slates_hg::push::intake::{Intake, PushSegment, Stepped};
use slates_hg::push::read;
use slates_vcs::ContentStore;

/// Shape: the room the bundle's decoding and the store are given (the whole history fits).
const DECODE_ROOM: u64 = 1 << 36;
/// Shape: the slowest steps printed.
const SLOWEST: usize = 15;
/// Shape: a long step, and a very long one, in microseconds.
const LONG_US: u128 = 100_000;
/// Shape: see [`LONG_US`].
const VERY_LONG_US: u128 = 1_000_000;

/// One kind's figures: its steps, their time, how many were long and very long, and the longest, in microseconds.
#[derive(Default)]
struct Kind {
  steps: u64,
  total_us: u128,
  long: u64,
  very_long: u64,
  longest_us: u128,
}

impl Kind {
  /// Notes one step of `us`.
  fn note(&mut self, us: u128) {
    self.steps += 1;
    self.total_us += us;
    self.long += u64::from(us > LONG_US);
    self.very_long += u64::from(us > VERY_LONG_US);
    self.longest_us = self.longest_us.max(us);
  }
}

fn main() {
  let args: Vec<String> = std::env::args().collect();
  let [_, bundle, window, room, segment] = args.as_slice() else {
    panic!("usage: intake_steps <bundle> <window bytes> <room bytes> <segment bytes>");
  };
  let (window, room, segment): (usize, u64, u64) = (
    window.parse().unwrap(),
    room.parse().unwrap(),
    segment.parse().unwrap(),
  );
  let bytes = std::fs::read(bundle).unwrap();
  let decoded = Bundle2::decode(&bytes, &mut Admission::new(DECODE_ROOM)).unwrap();
  let unbundle = read(&decoded, &mut Admission::new(DECODE_ROOM)).unwrap();
  let (_, part) = unbundle.changegroup().expect("a changegroup part");
  let payload = part.payload.clone();
  let mut store = ContentStore::new(DECODE_ROOM, window).unwrap();
  let mut index = HgIndex::new();
  let mut admission = Admission::new(room);
  let began = Instant::now();
  let mut intake = Intake::begin(
    (part.version, &payload),
    &mut index,
    &mut store,
    &mut admission,
  )
  .unwrap()
  .cutting(segment);
  println!("intake_steps: began in {:?}", began.elapsed());
  let mut kinds: BTreeMap<&'static str, Kind> = BTreeMap::new();
  let mut records = Kind::default();
  let mut slowest: Vec<(u128, u64, &'static str)> = Vec::new();
  let (mut merge_run, mut longest_merge) = (0_u64, 0_u64);
  let mut at = 0_u64;
  let whole = Instant::now();
  loop {
    let unit = intake.unit();
    let start = Instant::now();
    let stepped = intake.step(&payload, &mut store, &mut admission).unwrap();
    let took = start.elapsed().as_micros();
    kinds.entry(unit).or_default().note(took);
    slowest.push((took, at, unit));
    if slowest.len() > SLOWEST * 4 {
      slowest.sort_unstable_by(|left, right| right.0.cmp(&left.0));
      slowest.truncate(SLOWEST);
    }
    merge_run = if unit == "merge" { merge_run + 1 } else { 0 };
    longest_merge = longest_merge.max(merge_run);
    if matches!(stepped, Stepped::Done) {
      intake.cut_rest();
    }
    while let Some(taken) = intake.take_segment() {
      let start = Instant::now();
      let _record = whole_record(&intake, &taken, u64::MAX, &store);
      records.note(start.elapsed().as_micros());
    }
    at += 1;
    if at.is_multiple_of(100_000) {
      eprintln!(
        "intake_steps: {at} steps, {:.1} s",
        whole.elapsed().as_secs_f64()
      );
    }
    match stepped {
      Stepped::More => {}
      Stepped::Done => break,
      other => panic!("a whole changegroup in memory stepped {other:?}"),
    }
  }
  println!(
    "intake_steps: wall {:.2} s, {at} steps",
    whole.elapsed().as_secs_f64()
  );
  let line = |name: &str, kind: &Kind| {
    println!(
      "intake_steps:   {name:>18}: {:>7} steps, {:>8.2} s, past 100 ms {:>5}, past 1 s {:>3}, longest {:>8.1} ms",
      kind.steps,
      kind.total_us as f64 / 1e6,
      kind.long,
      kind.very_long,
      kind.longest_us as f64 / 1e3
    );
  };
  for (name, kind) in &kinds {
    line(name, kind);
  }
  line("segment records", &records);
  slowest.sort_unstable_by(|left, right| right.0.cmp(&left.0));
  for (us, step, unit) in slowest.iter().take(SLOWEST) {
    println!(
      "intake_steps:   step {step} ({unit}): {:.1} ms",
      *us as f64 / 1e3
    );
  }
  let Ok((mut fork, intaken)) = intake.finish() else {
    panic!("the intake finished");
  };
  let (rows, seals) = (fork.rows().rows(), fork.rows().counters().seals);
  let own_bytes = fork.rows().own_bytes();
  let windows = own_bytes / u64::try_from(window).unwrap().max(1);
  println!(
    "intake_steps: the fork's rows {rows}, seals {seals} ({} rows a seal on average); most own runs {}, tiered merges \
     {}, the longest merge {longest_merge} steps",
    rows / seals.max(1),
    intaken.most_own_runs,
    intaken.tiered_merges
  );
  println!(
    "intake_steps: the fork's own bytes {own_bytes} ({windows} windows of {window}); the bytes bound at the end \
     floor(log3 windows) + 1 = {}",
    windows.max(1).ilog(3) + 1
  );
}

/// `segment`'s record, written by `intake` a step at a time to its end (as the daemon writes it, a turn's steps at a time).
fn whole_record(
  intake: &Intake,
  segment: &PushSegment,
  room: u64,
  store: &ContentStore,
) -> Vec<u8> {
  let mut writer = intake.record_writer(segment, room).unwrap();
  while intake.record_step(&mut writer, store).unwrap() {}
  writer.finish().unwrap()
}
