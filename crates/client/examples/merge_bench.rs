//! The edit-and-merge latency harness (§4.16 "Laptop degenerate": "every arrow in the submission
//! is an in-process call, and the merge path is microseconds"; Ada's goal of 2026-11-04: a full edit
//! and merge under 1 ms, preferably under 50 µs). One sample is what an agent pays to change a file
//! and learn the green's verdict: the client's `edit` of its work volume (the declared splice, §4.16
//! "Declared operations"), then its `submit` through the server's verdict to the reply — accepted at
//! a new version, accepted identical, or a conflict with its windows. The client and the daemon are
//! one binary and one process here, over the real rendezvous and rings (§4.7), exactly as `slates`
//! runs both.
//!
//! Every sample is checked against a model of the green kept in the harness: the verdict must be
//! the one the design's rule gives for the form (`Accept`, `AcceptIdentical`, `Conflict` with a
//! window over the edited range), the head must be the model's version, and the edited file read
//! back from the green's head must equal the model's bytes. A wrong answer ends the run with exit 1,
//! so no number is printed for a path that answered wrongly.
//!
//! Forms, each over two green shapes (one file, and a repository of many small files):
//! - `accept`: one work edits a file and submits; its base is the head, so the fast path accepts
//!   (`last_changed[path] <= base`).
//! - `ranged`: a second work's base lags the head by the first work's accept on the same file; its
//!   disjoint range is mapped and swept and accepts.
//! - `identical`: the lagging work writes the very bytes the head already holds at that range; pass
//!   two's memcmp accepts it with no effect.
//! - `conflict`: the lagging work writes different bytes over the same range; the verdict is a
//!   conflict and the green does not move. The work is replaced afterwards, untimed.
//!
//! Each form runs quiet (nothing else on the machine but what the box carries) and loaded (one
//! spinning thread per core this process can run, owned and joined by the harness, so the shards
//! and the client contend for every core). Rows are `ratchet-info\tmerge.<form>_<shape>_<load>_<part>_<pct>`
//! with `<lower>\t<median>\t<upper>` in nanoseconds over the runs, then every run's value; the
//! `goal` lines compare the quiet single-client total p99 with Ada's two bounds. Nothing is gated
//! yet: the bounds are the goal, recorded beside the measurement until the path meets them.
// Bench harness code: an unwrap here is a failed run.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use slates_client::{Client, ClientError, Deadlines, ReadAt, Submitted, VolumeId};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: runs per row (the smallest count with a middle run, CLAUDE.md §5 "best of N with all N
/// shown").
const RUNS: usize = 3;
/// Shape: samples per form per run.
const SAMPLES: usize = 1_000;
/// Shape: the bytes one edit replaces: a line or two of source, what an agent's edit is.
const EDIT_BYTES: usize = 64;
/// Shape: each green file's length: a small source file, room for two disjoint edit ranges.
const FILE_BYTES: usize = 4_096;
/// Shape: the green shapes measured, as file counts: a single file, and a repository of small files
/// (the submit's cost may scale with what the green holds, which this pair exposes).
const GREEN_FILES: &[usize] = &[1, 1_024];
/// Shape: Ada's preferred bound for a full edit and merge (2026-11-04).
const GOAL_PREFERRED_NS: u64 = 50_000;
/// Shape: Ada's required bound for a full edit and merge (2026-11-04).
const GOAL_REQUIRED_NS: u64 = 1_000_000;
/// Shape: how long the client spins for a reply before it parks: past the goal, so a reply inside
/// the goal is taken without a wake (the provisioning bench's spinning form, AC-2.1).
const SPIN_NS: u64 = GOAL_REQUIRED_NS;
/// Format: the percentiles reported, in parts per thousand.
const PERCENTILES: &[(&str, u64)] = &[("p50", 500), ("p99", 990), ("p999", 999)];
/// Format: parts per thousand.
const PERMILLE: u64 = 1000;
/// Shape: how long a client retries the rendezvous while the daemon comes up.
const CONNECT_WAIT: Duration = Duration::from_secs(10);
/// Format: the offset of the second, disjoint edit range in a file: its second half.
const SECOND_RANGE: u64 = (FILE_BYTES / 2) as u64;

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
      )) if started.elapsed() < CONNECT_WAIT => std::hint::spin_loop(),
      Err(e) => panic!("{e}"),
    }
  }
}

/// The verdict a form expects.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Form {
  Accept,
  Ranged,
  Identical,
  Conflict,
}

impl Form {
  const ALL: [Form; 4] = [Form::Accept, Form::Ranged, Form::Identical, Form::Conflict];

  fn key(self) -> &'static str {
    match self {
      Form::Accept => "accept",
      Form::Ranged => "ranged",
      Form::Identical => "identical",
      Form::Conflict => "conflict",
    }
  }
}

/// One sample's parts, in nanoseconds.
#[derive(Clone, Copy)]
struct Sample {
  edit: u64,
  submit: u64,
}

/// A green, its model and the works that edit it.
struct Fixture {
  client: Client,
  green: VolumeId,
  /// The green's head as the model has it.
  head: u64,
  /// Every file's bytes at the head.
  model: BTreeMap<String, Vec<u8>>,
  /// The work whose base is the head after each of its accepts.
  leader: VolumeId,
  /// The work whose base lags the head by the leader's last accept.
  follower: VolumeId,
  /// Works made so far (each a fresh name).
  made: u64,
  /// A counter folded into every edit's bytes, so no two edits write the same bytes by accident.
  stamp: u64,
  /// The fixture's name prefix.
  tag: String,
}

fn path_of(index: usize) -> String {
  format!("src/f{index:05}.rs")
}

/// `EDIT_BYTES` bytes no other edit of this run has written.
fn fresh_bytes(stamp: u64) -> Vec<u8> {
  let mut bytes = Vec::with_capacity(EDIT_BYTES);
  let mut word = stamp.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
  while bytes.len() < EDIT_BYTES {
    word ^= word << 13;
    word ^= word >> 7;
    word ^= word << 17;
    bytes.extend_from_slice(&word.to_le_bytes());
  }
  bytes.truncate(EDIT_BYTES);
  bytes
}

/// Fails the run: the harness never prints a number for a path that answered wrongly.
fn wrong(what: &str) -> ! {
  eprintln!("merge_bench: WRONG ANSWER: {what}");
  std::process::exit(1);
}

impl Fixture {
  fn new(instance: &str, tag: &str, files: usize) -> Fixture {
    let mut client = connect(instance);
    client.spin_for(Some(SPIN_NS));
    let green = client.create_green(&format!("{tag}-green"), false).unwrap();
    let (seed, _) = client.create_work(green, &format!("{tag}-seed")).unwrap();
    let mut model = BTreeMap::new();
    for index in 0..files {
      let mut bytes = Vec::with_capacity(FILE_BYTES);
      let mut stamp = u64::try_from(index).unwrap_or(u64::MAX) << 32;
      while bytes.len() < FILE_BYTES {
        bytes.extend_from_slice(&fresh_bytes(stamp));
        stamp = stamp.wrapping_add(1);
      }
      bytes.truncate(FILE_BYTES);
      client.edit(seed, &path_of(index), 0, 0, &bytes).unwrap();
      model.insert(path_of(index), bytes);
    }
    match client.submit(seed).unwrap() {
      Submitted::Accepted(1) => {}
      other => wrong(&format!("the seed submit answered {other:?}, not Accepted(1)")),
    }
    client.destroy(seed).unwrap();
    let (leader, _) = client.create_work(green, &format!("{tag}-w0")).unwrap();
    let (follower, _) = client.create_work(green, &format!("{tag}-w1")).unwrap();
    Fixture {
      client,
      green,
      head: 1,
      model,
      leader,
      follower,
      made: 2,
      stamp: 0,
      tag: tag.to_owned(),
    }
  }

  fn next_stamp(&mut self) -> u64 {
    self.stamp = self.stamp.wrapping_add(1);
    self.stamp
  }

  /// Overwrites `[at, at + EDIT_BYTES)` of `path` in the model.
  fn model_overwrite(&mut self, path: &str, at: u64, bytes: &[u8]) {
    let file = self.model.get_mut(path).unwrap();
    let start = usize::try_from(at).unwrap();
    file[start..start + bytes.len()].copy_from_slice(bytes);
  }

  /// The leader's untimed accept that the lagging forms run behind: it overwrites the first range
  /// of `path`, and the follower's base now lags the head by it.
  fn lead(&mut self, path: &str, bytes: &[u8]) {
    let leader = self.leader;
    self.client.edit(leader, path, 0, EDIT_BYTES as u64, bytes).unwrap();
    let answer = self.client.submit(leader).unwrap();
    self.expect_accept(answer, "the leader's edit");
    self.model_overwrite(path, 0, bytes);
  }

  fn expect_accept(&mut self, answer: Submitted, what: &str) {
    let wanted = self.head + 1;
    match answer {
      Submitted::Accepted(version) if version == wanted => self.head = version,
      other => wrong(&format!("{what} answered {other:?}, not Accepted({wanted})")),
    }
  }

  /// The edited file read back from the green's head must equal the model's bytes, and the head
  /// must be the model's.
  fn check(&mut self, path: &str) {
    let head = self.client.versions(self.green).unwrap();
    if head != self.head {
      wrong(&format!("the green's head is {head}, the model's {}", self.head));
    }
    let read = self.client.read(self.green, path, ReadAt::Head).unwrap();
    if Some(&read) != self.model.get(path) {
      wrong(&format!("{path} at the head differs from the model"));
    }
  }

  /// Replaces the follower (after a conflict its journal holds the refused edit): untimed.
  fn replace_follower(&mut self) {
    self.client.destroy(self.follower).unwrap();
    self.made += 1;
    let name = format!("{}-w{}", self.tag, self.made);
    let (follower, base) = self.client.create_work(self.green, &name).unwrap();
    if base != self.head {
      wrong(&format!("a new work is based on {base}, the head is {}", self.head));
    }
    self.follower = follower;
  }

  /// One timed sample of `form` on file `index`.
  fn sample(&mut self, form: Form, index: usize) -> Sample {
    let path = path_of(index);
    match form {
      Form::Accept => {
        let bytes = fresh_bytes(self.next_stamp());
        let leader = self.leader;
        let started = Instant::now();
        self.client.edit(leader, &path, 0, EDIT_BYTES as u64, &bytes).unwrap();
        let edited = Instant::now();
        let answer = self.client.submit(leader).unwrap();
        let done = Instant::now();
        self.expect_accept(answer, "an edit based on the head");
        self.model_overwrite(&path, 0, &bytes);
        self.check(&path);
        Sample {
          edit: ns(edited - started),
          submit: ns(done - edited),
        }
      }
      Form::Ranged => {
        let lead = fresh_bytes(self.next_stamp());
        self.lead(&path, &lead);
        let bytes = fresh_bytes(self.next_stamp());
        let follower = self.follower;
        let started = Instant::now();
        self
          .client
          .edit(follower, &path, SECOND_RANGE, EDIT_BYTES as u64, &bytes)
          .unwrap();
        let edited = Instant::now();
        let answer = self.client.submit(follower).unwrap();
        let done = Instant::now();
        self.expect_accept(answer, "a disjoint edit behind the head");
        self.model_overwrite(&path, SECOND_RANGE, &bytes);
        self.check(&path);
        Sample {
          edit: ns(edited - started),
          submit: ns(done - edited),
        }
      }
      Form::Identical => {
        let lead = fresh_bytes(self.next_stamp());
        self.lead(&path, &lead);
        let follower = self.follower;
        let started = Instant::now();
        self.client.edit(follower, &path, 0, EDIT_BYTES as u64, &lead).unwrap();
        let edited = Instant::now();
        let answer = self.client.submit(follower).unwrap();
        let done = Instant::now();
        // An identical edit accepts at a new version with no effect: the model's bytes stay.
        self.expect_accept(answer, "an identical edit behind the head");
        self.check(&path);
        Sample {
          edit: ns(edited - started),
          submit: ns(done - edited),
        }
      }
      Form::Conflict => {
        let lead = fresh_bytes(self.next_stamp());
        self.lead(&path, &lead);
        let bytes = fresh_bytes(self.next_stamp());
        let follower = self.follower;
        let started = Instant::now();
        self.client.edit(follower, &path, 0, EDIT_BYTES as u64, &bytes).unwrap();
        let edited = Instant::now();
        let answer = self.client.submit(follower).unwrap();
        let done = Instant::now();
        match answer {
          Submitted::Conflict(windows)
            if windows
              .iter()
              .any(|w| w.path == path && w.at < EDIT_BYTES as u64 && w.len > 0) => {}
          other => wrong(&format!(
            "a differing edit over the leader's range answered {other:?}, not a conflict on {path}"
          )),
        }
        self.check(&path);
        self.replace_follower();
        Sample {
          edit: ns(edited - started),
          submit: ns(done - edited),
        }
      }
    }
  }
}

/// The percentiles and the max of `samples`.
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

/// One form on one green shape: per part, per percentile, the per-run values.
fn measure(instance: &str, form: Form, files: usize, load: &str) -> BTreeMap<String, Vec<u64>> {
  let mut rows: BTreeMap<String, Vec<u64>> = BTreeMap::new();
  for run in 0..RUNS {
    let tag = format!("{}-{files}-{load}-{run}", form.key());
    let mut fixture = Fixture::new(instance, &tag, files);
    let mut edits = Vec::with_capacity(SAMPLES);
    let mut submits = Vec::with_capacity(SAMPLES);
    let mut totals = Vec::with_capacity(SAMPLES);
    for n in 0..SAMPLES {
      let sample = fixture.sample(form, n % files);
      edits.push(sample.edit);
      submits.push(sample.submit);
      totals.push(sample.edit.saturating_add(sample.submit));
    }
    for (part, samples) in [("edit", edits), ("submit", submits), ("total", totals)] {
      for (pct, value) in percentiles(samples) {
        rows.entry(format!("{part}_{pct}")).or_default().push(value);
      }
    }
    fixture.client.destroy(fixture.leader).unwrap();
    fixture.client.destroy(fixture.follower).unwrap();
    fixture.client.destroy(fixture.green).unwrap();
  }
  rows
}

fn print_rows(key: &str, rows: &BTreeMap<String, Vec<u64>>) {
  for (name, values) in rows {
    let mut sorted = values.clone();
    sorted.sort_unstable();
    let (lower, median, upper) = (sorted[0], sorted[sorted.len() / 2], sorted[sorted.len() - 1]);
    println!("ratchet-info\tmerge.{key}_{name}\t{lower}\t{median}\t{upper}\t{values:?}");
  }
}

fn median_of(rows: &BTreeMap<String, Vec<u64>>, name: &str) -> u64 {
  let mut sorted = rows[name].clone();
  sorted.sort_unstable();
  sorted[sorted.len() / 2]
}

/// Runs every form over every shape, under `load`.
fn sweep(instance: &str, load: &str) {
  for files in GREEN_FILES {
    for form in Form::ALL {
      let rows = measure(instance, form, *files, load);
      let key = format!("{}_f{files}_{load}", form.key());
      print_rows(&key, &rows);
      let p99 = median_of(&rows, "total_p99");
      let verdict = if p99 <= GOAL_PREFERRED_NS {
        "within the preferred 50 us"
      } else if p99 <= GOAL_REQUIRED_NS {
        "within the required 1 ms"
      } else {
        "OVER the required 1 ms"
      };
      println!(
        "goal\tmerge.{key}: total p50 {} ns, p99 {p99} ns, p999 {} ns, max {} ns: {verdict}",
        median_of(&rows, "total_p50"),
        median_of(&rows, "total_p999"),
        median_of(&rows, "total_max"),
      );
    }
  }
}

fn main() {
  let profile = MachineProfile::measure_patiently(&ProfileOptions {
    budget_per_probe: ProfileOptions::default().budget_per_probe,
    codecs: false,
    core_matrix: false,
  })
  .expect("the machine profile measures");
  let instance = format!("merge-bench-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, None);
  let shards = config.runtime.shards;
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-merge-bench-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let cores = profile.facts.cpus_at_once();
  println!(
    "merge bench: {shards} shards, {cores} cores, {SAMPLES} samples x {RUNS} runs, edits of {EDIT_BYTES} B in files of {FILE_BYTES} B"
  );
  let only_quiet = std::env::args().any(|arg| arg == "--quiet-only");
  sweep(&instance, "quiet");
  if !only_quiet {
    // The loaded form: one spinning thread per core this process can run, owned and joined here.
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
      for _ in 0..cores {
        scope.spawn(|| {
          while !stop.load(Ordering::Relaxed) {
            std::hint::spin_loop();
          }
        });
      }
      sweep(&instance, "loaded");
      stop.store(true, Ordering::Relaxed);
    });
  }
  daemon.stop();
}
