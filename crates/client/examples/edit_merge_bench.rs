//! The client's edit and merge latency on this platform (§4.16 `edit` and `submit`; RQ-4's
//! "under 1 ms" for the agent-facing verbs; the per-platform record of `docs/wip/platforms.md`):
//! one green volume served by an in-process daemon, reached from the Rust client through the
//! real rendezvous and rings. Each sample makes a work volume off the head (untimed), declares
//! one edit (timed: `edit`), submits it (timed: `merge`, an accepted increment), then destroys
//! the work (untimed). Two forms run on fresh greens: `new_file`, where each sample creates its
//! own file, so the green grows a file a version, and `one_file`, where each sample replaces one
//! file's line, so only the version count grows. Each run also prints the median of its first
//! and last tenth of samples, which shows a cost that grows with the green and which the
//! percentiles alone would mix away. The client spins for its
//! reply as the provisioning histogram's spinning form does, so a row is the verb's round trip
//! and its service, not a wake. Rows are `ratchet-info\t<key>\t<lower>\t<median>\t<upper>` in
//! nanoseconds over the runs, and the `goal` lines hold each p99 against the millisecond.
//!
//! Command: `cargo build --release -p slates-client --example edit_merge_bench`, then
//! `target/release/examples/edit_merge_bench`.
// Bench harness code: an unwrap here is a failed run.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::time::{Duration, Instant};

use slates_client::{Client, ClientError, Deadlines, Submitted};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: runs per row.
const RUNS: usize = 3;
/// Shape: edit-and-submit samples per run.
const SAMPLES: usize = 1_000;
/// Shape: the goal each verb's p99 is held to: one millisecond (RQ-4, "end-to-end latency
/// under 1 ms").
const GOAL_NS: u64 = 1_000_000;
/// Shape: the client's spin window, the provisioning histogram's floor (R9): the reply of a
/// verb served within it lands without a wake.
const SPIN_NS: u64 = 50_000;
/// Shape: the bytes one edit inserts: a line of source.
const EDIT_BYTES: &[u8] = b"    let answer = compute(&input)?; // an agent's one-line edit\n";
/// Format: the percentiles reported, in parts per thousand.
const PERCENTILES: &[(&str, u64)] = &[("p50", 500), ("p99", 990), ("p999", 999)];
/// Format: parts per thousand.
const PERMILLE: u64 = 1000;
/// Shape: how long the client retries the rendezvous while the daemon comes up.
const CONNECT_WAIT: Duration = Duration::from_secs(10);

fn ns(elapsed: Duration) -> u64 {
  u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
}

fn deadlines() -> Deadlines {
  Deadlines::derive(
    slates_server::daemon::LIVENESS_BUDGET_NS,
    slates_db::replay::RECOVERY_BUDGET_NS,
  )
  .get()
}

fn connect(instance: &str) -> Client {
  let started = Instant::now();
  loop {
    match Client::connect(instance, deadlines()) {
      Ok(client) => return client,
      Err(ClientError::Ipc(
        slates_ipc::IpcError::DaemonUnavailable { .. } | slates_ipc::IpcError::RingFull,
      )) if started.elapsed() < CONNECT_WAIT => std::thread::yield_now(),
      Err(e) => panic!("{e}"),
    }
  }
}

/// The percentiles of one run's samples, then its max.
fn percentiles(mut samples: Vec<u64>) -> Vec<(&'static str, u64)> {
  samples.sort_unstable();
  let last = samples.len().saturating_sub(1);
  let mut out: Vec<(&'static str, u64)> = PERCENTILES
    .iter()
    .map(|(name, permille)| {
      let index = usize::try_from(u64::try_from(last).unwrap_or(u64::MAX) * permille / PERMILLE)
        .unwrap_or(last);
      (*name, samples[index.min(last)])
    })
    .collect();
  out.push(("max", samples[last]));
  out
}

/// What each sample edits, so the two forms tell a cost that follows the green's file count from
/// one that follows its version count.
#[derive(Clone, Copy)]
enum Form {
  /// Each sample creates its own file: after sample `n` the green holds `n + 1` files and
  /// `n + 1` versions.
  NewFile,
  /// Each sample replaces the one file's line: the green holds one file and `n + 1` versions.
  OneFile,
}

impl Form {
  fn key(self) -> &'static str {
    match self {
      Form::NewFile => "new_file",
      Form::OneFile => "one_file",
    }
  }
}

/// One run of a form: `SAMPLES` edit-and-submit cycles on a fresh green; the edit and the submit
/// samples, in order.
fn run(client: &mut Client, form: Form, run: usize) -> (Vec<u64>, Vec<u64>) {
  let green = client
    .create_green(&format!("bench-green-{}-{run}", form.key()), false)
    .unwrap();
  let mut edits = Vec::with_capacity(SAMPLES);
  let mut merges = Vec::with_capacity(SAMPLES);
  for n in 0..SAMPLES {
    let (work, _) = client.create_work(green, &format!("w-{run}-{n}")).unwrap();
    let (path, replaced) = match form {
      Form::NewFile => (format!("src/file-{n}.rs"), 0),
      // The first sample creates the file; every later one replaces the line the last put there.
      Form::OneFile => (
        "src/one.rs".to_owned(),
        if n == 0 { 0 } else { EDIT_BYTES.len() as u64 },
      ),
    };
    let started = Instant::now();
    client.edit(work, &path, 0, replaced, EDIT_BYTES).unwrap();
    edits.push(ns(started.elapsed()));
    let started = Instant::now();
    let verdict = client.submit(work).unwrap();
    merges.push(ns(started.elapsed()));
    // Each work is made off the head and submitted before the next is made, so a conflict is a
    // defect of the merge, not noise.
    assert!(
      matches!(verdict, Submitted::Accepted(_)),
      "{} sample {n}: {verdict:?}",
      form.key()
    );
    client.destroy(work).unwrap();
  }
  (edits, merges)
}

/// Shape: the samples compared at each end of a run: a tenth of them.
const GROWTH_SHARE: usize = 10;

/// The median of a run's first and last tenth: a verb whose cost follows the green's history
/// (each sample adds one accepted version) shows it here, where the percentiles mix it away.
fn print_growth(run: usize, key: &str, samples: &[u64]) {
  let tenth = (samples.len() / GROWTH_SHARE).max(1);
  let median = |slice: &[u64]| {
    let mut sorted = slice.to_vec();
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
  };
  let first = median(&samples[..tenth]);
  let last = median(&samples[samples.len() - tenth..]);
  println!(
    "  run {run}: {key} median of the first {tenth} samples {first} ns, of the last {tenth} {last} ns"
  );
}

fn print_rows(key: &str, per_run: &[Vec<(&'static str, u64)>]) -> u64 {
  let mut p99 = 0;
  for (index, (name, _)) in per_run[0].iter().enumerate() {
    let mut values: Vec<u64> = per_run.iter().map(|r| r[index].1).collect();
    let shown = values.clone();
    values.sort_unstable();
    let (lower, median, upper) = (
      values[0],
      values[values.len() / 2],
      values[values.len() - 1],
    );
    if *name == "p99" {
      p99 = median;
    }
    println!("ratchet-info\tclient.{key}_{name}\t{lower}\t{median}\t{upper}");
    println!("  client.{key}_{name}: {median} ns [{lower}, {upper}] over {RUNS} runs: {shown:?}");
  }
  p99
}

fn main() {
  let profile = MachineProfile::measure_patiently(&ProfileOptions {
    budget_per_probe: ProfileOptions::default().budget_per_probe,
    codecs: false,
    core_matrix: false,
  })
  .expect("the machine profile measures");
  let instance = format!("edit-merge-bench-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, None);
  let shards = config.runtime.shards;
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-edit-merge-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  println!(
    "edit and merge latency: {} {}, {shards} shards, {} cores, client spin {SPIN_NS} ns, {SAMPLES} samples x {RUNS} runs",
    std::env::consts::OS,
    std::env::consts::ARCH,
    profile.facts.cpus_at_once()
  );
  let mut client = connect(&instance);
  client.spin_for(Some(SPIN_NS));
  let mut failed = false;
  for form in [Form::NewFile, Form::OneFile] {
    let mut edit_runs = Vec::new();
    let mut merge_runs = Vec::new();
    for index in 0..RUNS {
      let (edits, merges) = run(&mut client, form, index);
      print_growth(index, &format!("{}.edit", form.key()), &edits);
      print_growth(index, &format!("{}.merge", form.key()), &merges);
      edit_runs.push(percentiles(edits));
      merge_runs.push(percentiles(merges));
    }
    for (verb, runs) in [("edit", &edit_runs), ("merge", &merge_runs)] {
      let key = format!("{}.{verb}", form.key());
      let p99 = print_rows(&key, runs);
      let ok = p99 <= GOAL_NS;
      failed |= !ok;
      println!(
        "goal: {key} p99 {p99} ns vs {GOAL_NS} ns: {}",
        if ok { "ok" } else { "FAIL" }
      );
    }
  }
  drop(client);
  daemon.stop();
  if failed {
    std::process::exit(1);
  }
}
