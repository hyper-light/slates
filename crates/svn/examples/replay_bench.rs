//! svnsync's replay, measured (main's order of 2026-11-20: w-interop's matrix r10 replayed git-2000.dump's 2,000
//! revisions in ~18 min, 0.54 s a revision, against stock svnserve's small fraction). A dump is imported into the
//! records, then every revision is replayed with deltas as `svnsync synchronize` asks it (`replay` of REV, low water
//! mark 0, `send-deltas` true), a step at a time as the server drives it (`Replay::next_within`, one step a call).
//! Printed per run: the import's wall time, the replay's total, each revision's mean, the slowest revisions with their
//! delta bytes, and the longest single step. Peak memory is the process's, read by the caller (`/usr/bin/time -l`).
//!
//! Run (release): `cargo run --release -p slates-svn --example replay_bench -- DUMP [FIRST LAST]`. `DUMP` is a dump
//! file, as `crates/cli/bench/git_to_svn_dump.py GIT_DIR master 2000` makes git-2000.dump.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::print_stdout,
  clippy::disallowed_methods,
  clippy::disallowed_types
)]

use std::time::{Duration, Instant};

use slates_svn::dump::{Admission, Uuid};
use slates_svn::migration::{Importer, Importing, Repository, Room};
use slates_svn::report::drive::Call;
use slates_svn::report::drive::replay::{Replay, Stepped};

/// Shape: a room past git-2000.dump's index and bytes.
const ROOM: Room = Room {
  entries: 1 << 24,
  bytes: 1 << 36,
};

/// Shape: an admission past svndiff's largest window and the longest property section.
const ADMISSION: Admission = Admission {
  buffer_bytes: 1 << 22,
};

/// Shape: the bytes the import is handed at a time, as a connection would.
const PIECE: usize = 1 << 20;

/// Imports `dump` into fresh records.
fn imported(dump: &[u8]) -> Repository {
  let uuid = Uuid::parse(b"00000000-0000-4000-8000-0000000072b1").unwrap();
  let mut repository = Repository::new(uuid, b"2026-11-20T00:00:00.000000Z", ROOM).unwrap();
  let mut importer = Importer::new(ADMISSION);
  let mut held: Vec<u8> = Vec::new();
  let (mut next, mut needed) = (0usize, 1usize);
  loop {
    while held.len() < needed && next < dump.len() {
      let end = dump.len().min(next + PIECE);
      held.extend_from_slice(&dump[next..end]);
      next = end;
    }
    match importer
      .import(&mut repository, &held, next == dump.len())
      .unwrap()
    {
      Importing::Done => return repository,
      Importing::NeedMore {
        consumed,
        needed: more,
      } => {
        held.drain(..consumed);
        needed = more.max(held.len() + 1);
      }
      Importing::Commit { consumed, record } => {
        held.drain(..consumed);
        repository.commit_record(*record).unwrap();
      }
    }
  }
}

fn main() {
  let mut args = std::env::args().skip(1);
  let path = args.next().expect("a dump file");
  let dump = std::fs::read(&path).unwrap();
  let importing = Instant::now();
  let repository = imported(&dump);
  drop(dump);
  let import = importing.elapsed();
  let youngest = repository.youngest();
  let first: u64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(1);
  let last: u64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(youngest);
  let mut per_revision: Vec<(Duration, u64, u64)> = Vec::new();
  let mut longest_step = Duration::ZERO;
  let mut steps_total = 0u64;
  let replaying = Instant::now();
  for revision in first..=last {
    let began = Instant::now();
    let mut replay = Replay::new(revision, "/", 0, true);
    let mut delta_bytes = 0u64;
    loop {
      let step = Instant::now();
      let next = replay
        .next_within((repository.index(), &repository), None, 1)
        .unwrap();
      longest_step = longest_step.max(step.elapsed());
      steps_total += 1;
      match next {
        Stepped::Ended => break,
        Stepped::Call(Call::DeltaPiece { bytes, .. }) => delta_bytes += bytes.len() as u64,
        _ => {}
      }
    }
    per_revision.push((began.elapsed(), revision, delta_bytes));
  }
  let total = replaying.elapsed();
  let count = per_revision.len().max(1) as u32;
  per_revision.sort_by(|a, b| b.0.cmp(&a.0));
  println!(
    "dump {path} youngest {youngest} import_s {:.1} replay r{first}..r{last} total_s {:.2} mean_ms {:.2} \
     longest_step_us {} steps {steps_total} texts_made_whole {} windows_made {}",
    import.as_secs_f64(),
    total.as_secs_f64(),
    (total / count).as_secs_f64() * 1e3,
    longest_step.as_micros(),
    repository.texts_made_whole(),
    repository.windows_made()
  );
  for (took, revision, delta_bytes) in per_revision.iter().take(10) {
    println!(
      "  r{revision} {:.1} ms, {delta_bytes} delta bytes",
      took.as_secs_f64() * 1e3
    );
  }
}
