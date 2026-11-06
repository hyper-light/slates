//! The edit-and-merge latency harness (§4.16 "Laptop degenerate": "every arrow in the submission is an in-process
//! call, and the merge path is microseconds"; RQ-4's "under 1 ms" for the agent-facing verbs; Ada's goal of
//! 2026-11-04: a full edit and merge under 1 ms, preferably under 50 µs; the per-platform client record of
//! `docs/wip/platforms.md`). One sample is what an agent pays to change a file and learn the green's verdict: the
//! client's `edit` of its work volume (the declared splice, §4.16 "Declared operations"), then its `submit` through
//! the server's verdict to the reply — accepted at a new version, accepted identical, or a conflict with its windows.
//! The client and the daemon are one binary and one process here, over the real rendezvous and rings (§4.7), exactly
//! as `slates` runs both. The client spins up to the required bound for each reply, so a reply inside the goal is a
//! round trip and its service, not a wake (until 2026-11-05 the growth forms below spun 50 µs).
//!
//! Two families of forms:
//! - **Verdict forms**, each over green shapes of 10 to 100,000 files of 4 KiB: `accept` (one work edits a file and
//!   submits; its base is the head, so the fast path accepts), `ranged` (a second work's base lags the head by the
//!   first's accept on the same file; its disjoint range is mapped and swept), `identical` (the lagging work writes
//!   the bytes the head already holds there; pass two's memcmp accepts it with no effect) and `conflict` (it writes
//!   different bytes there; the green does not move, and the work is replaced afterwards, untimed) and `rebase` (the
//!   lagging work's disjoint edit is rebased onto the head in the submit's place, then submitted untimed). Every sample is
//!   checked against a model of the green: the verdict the design's rule gives, the head, and the edited file read
//!   back from the head. A wrong answer ends the run with exit 1, so no number is printed for a path that answered
//!   wrongly.
//! - **Growth forms**, on fresh greens: each sample makes a work off the head (untimed), declares one 64-byte edit,
//!   submits it (always accepted) and destroys the work (untimed). `new_file`: each sample creates its own file, so
//!   the green gains a file a version; `one_file`: each sample replaces one file's line, so only the version count
//!   grows. Each run also prints the median of its first and last tenth, which shows a cost that grows with the green
//!   and which the percentiles alone would mix away.
//!
//! Each runs quiet (nothing else on the machine but what the box carries) and, for the verdict forms, loaded (one
//! spinning thread per core this process can run, owned and joined here). Rows are
//! `ratchet-info\t<key>\t<lower>\t<median>\t<upper>` in nanoseconds over the runs, then every run's value; the
//! `goal` lines compare each p99 with Ada's two bounds. Nothing is gated: the bounds are the goal, recorded beside the
//! measurement.
//!
//! Command: `cargo build --release -p slates-client --example edit_merge_bench`, then
//! `target/release/examples/edit_merge_bench [--quiet-only] [--files N].. [--form F]..` (`--form new_file` or
//! `one_file` selects a growth form; with no `--form`, every form runs).
// Bench harness code: an unwrap here is a failed run.
#![allow(
  clippy::unwrap_used,
  clippy::unwrap_in_result,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use slates_client::{Client, ClientError, Deadlines, ReadAt, Rebased, Submitted, VolumeId};
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
/// Shape: the green shapes measured, as file counts, a decade apart from a handful of files to a large
/// repository (§4.16's worked example is a 300k-file repository): a submit's cost must not follow them.
const GREEN_FILES: &[usize] = &[10, 1_000, 10_000, 100_000];
/// Shape: the wall time one run of one form may take before it stops sampling (at least [`MIN_SAMPLES`]),
/// so a shape whose merge is slow still finishes and prints its numbers with their sample count.
const RUN_BUDGET: Duration = Duration::from_secs(20);
/// Shape: the fewest samples a run takes whatever its budget: enough that its p99 is a sample, not the max.
const MIN_SAMPLES: usize = 200;
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
  Rebase,
}

impl Form {
  const ALL: [Form; 5] = [
    Form::Accept,
    Form::Ranged,
    Form::Identical,
    Form::Conflict,
    Form::Rebase,
  ];

  fn key(self) -> &'static str {
    match self {
      Form::Accept => "accept",
      Form::Ranged => "ranged",
      Form::Identical => "identical",
      Form::Conflict => "conflict",
      Form::Rebase => "rebase",
    }
  }
}

/// One sample's parts, in nanoseconds.
#[derive(Clone, Copy)]
struct Sample {
  edit: u64,
  submit: u64,
  /// The edit's request sequence (`Client::last_request`), to find its spans.
  edit_request: u32,
  /// The submit's (or rebase's) request sequence.
  submit_request: u32,
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
  eprintln!("edit_merge_bench: WRONG ANSWER: {what}");
  std::process::exit(1);
}

impl Fixture {
  /// A green of `files` files and its two works, or the daemon's typed refusal of a green that size (its partition's
  /// chain or the shard's budget cannot hold the seed): a measured answer, printed in the shape's place.
  fn new(instance: &str, tag: &str, files: usize) -> Result<Fixture, String> {
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
      if let Err(refused) = client.edit(seed, &path_of(index), 0, 0, &bytes) {
        client.destroy(seed).unwrap();
        client.destroy(green).unwrap();
        return Err(format!("the seed's edit {index} of {files}: {refused}"));
      }
      model.insert(path_of(index), bytes);
    }
    match client.submit(seed) {
      Ok(Submitted::Accepted(1)) => {}
      Err(ClientError::Refused(refusal)) => {
        client.destroy(seed).unwrap();
        client.destroy(green).unwrap();
        return Err(format!("the seed's submit of {files} files: {refusal:?}"));
      }
      other => wrong(&format!(
        "the seed submit answered {other:?}, not Accepted(1)"
      )),
    }
    client.destroy(seed).unwrap();
    let (leader, _) = client.create_work(green, &format!("{tag}-w0")).unwrap();
    let (follower, _) = client.create_work(green, &format!("{tag}-w1")).unwrap();
    Ok(Fixture {
      client,
      green,
      head: 1,
      model,
      leader,
      follower,
      made: 2,
      stamp: 0,
      tag: tag.to_owned(),
    })
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
    self
      .client
      .edit(leader, path, 0, EDIT_BYTES as u64, bytes)
      .unwrap();
    let answer = self.client.submit(leader).unwrap();
    self.expect_accept(answer, "the leader's edit");
    self.model_overwrite(path, 0, bytes);
  }

  fn expect_accept(&mut self, answer: Submitted, what: &str) {
    let wanted = self.head + 1;
    match answer {
      Submitted::Accepted(version) if version == wanted => self.head = version,
      other => wrong(&format!(
        "{what} answered {other:?}, not Accepted({wanted})"
      )),
    }
  }

  /// The edited file read back from the green's head must equal the model's bytes, and the head
  /// must be the model's.
  fn check(&mut self, path: &str) {
    let head = self.client.versions(self.green).unwrap();
    if head != self.head {
      wrong(&format!(
        "the green's head is {head}, the model's {}",
        self.head
      ));
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
      wrong(&format!(
        "a new work is based on {base}, the head is {}",
        self.head
      ));
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
        self
          .client
          .edit(leader, &path, 0, EDIT_BYTES as u64, &bytes)
          .unwrap();
        let edited = Instant::now();
        let edit_request = self.client.last_request().sequence;
        let answer = self.client.submit(leader).unwrap();
        let done = Instant::now();
        let submit_request = self.client.last_request().sequence;
        self.expect_accept(answer, "an edit based on the head");
        self.model_overwrite(&path, 0, &bytes);
        self.check(&path);
        Sample {
          edit: ns(edited - started),
          submit: ns(done - edited),
          edit_request,
          submit_request,
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
        let edit_request = self.client.last_request().sequence;
        let answer = self.client.submit(follower).unwrap();
        let done = Instant::now();
        let submit_request = self.client.last_request().sequence;
        self.expect_accept(answer, "a disjoint edit behind the head");
        self.model_overwrite(&path, SECOND_RANGE, &bytes);
        self.check(&path);
        Sample {
          edit: ns(edited - started),
          submit: ns(done - edited),
          edit_request,
          submit_request,
        }
      }
      Form::Identical => {
        let lead = fresh_bytes(self.next_stamp());
        self.lead(&path, &lead);
        let follower = self.follower;
        let started = Instant::now();
        self
          .client
          .edit(follower, &path, 0, EDIT_BYTES as u64, &lead)
          .unwrap();
        let edited = Instant::now();
        let edit_request = self.client.last_request().sequence;
        let answer = self.client.submit(follower).unwrap();
        let done = Instant::now();
        let submit_request = self.client.last_request().sequence;
        // An identical edit accepts at a new version with no effect: the model's bytes stay.
        self.expect_accept(answer, "an identical edit behind the head");
        self.check(&path);
        Sample {
          edit: ns(edited - started),
          submit: ns(done - edited),
          edit_request,
          submit_request,
        }
      }
      Form::Conflict => {
        let lead = fresh_bytes(self.next_stamp());
        self.lead(&path, &lead);
        let bytes = fresh_bytes(self.next_stamp());
        let follower = self.follower;
        let started = Instant::now();
        self
          .client
          .edit(follower, &path, 0, EDIT_BYTES as u64, &bytes)
          .unwrap();
        let edited = Instant::now();
        let edit_request = self.client.last_request().sequence;
        let answer = self.client.submit(follower).unwrap();
        let done = Instant::now();
        let submit_request = self.client.last_request().sequence;
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
          edit_request,
          submit_request,
        }
      }
      Form::Rebase => {
        // The lagging work's disjoint edit is rebased onto the head (timed in the submit's place), then
        // submitted (untimed): the rebase must move it onto the head with its edit kept, so the submit
        // accepts at the next version by the fast path and the file reads back with both edits.
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
        let edit_request = self.client.last_request().sequence;
        let answer = self.client.rebase(follower).unwrap();
        let done = Instant::now();
        let submit_request = self.client.last_request().sequence;
        if answer != Rebased::Rebased(self.head) {
          wrong(&format!(
            "a disjoint edit behind the head rebased as {answer:?}, not onto {}",
            self.head
          ));
        }
        let read = self.client.read(follower, &path, ReadAt::Head).unwrap();
        let mut expected = self.model.get(&path).cloned().unwrap_or_default();
        let at = usize::try_from(SECOND_RANGE).unwrap();
        expected[at..at + bytes.len()].copy_from_slice(&bytes);
        if read != expected {
          wrong(&format!(
            "{path} in the rebased work is not the head with its edit"
          ));
        }
        let answer = self.client.submit(follower).unwrap();
        self.expect_accept(answer, "a rebased edit");
        self.model_overwrite(&path, SECOND_RANGE, &bytes);
        self.check(&path);
        Sample {
          edit: ns(edited - started),
          submit: ns(done - edited),
          edit_request,
          submit_request,
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
/// One request's server-side time, from its spans (§4.14): the ring span on the shard that read its slot (read to
/// reply written), the shard ops of its verb on the shards that ran it, and the merge verdict inside a submit.
#[derive(Clone, Copy, Default)]
struct Served {
  ring: u64,
  op: u64,
  verdict: u64,
}

/// Every span of this client's requests the shards hold, drained (each drain is bounded, so a shard is drained until
/// it holds no more), keyed by request sequence; and the spans the shards shed before a drain.
fn drain(client: &mut Client, shards: u16, into: &mut BTreeMap<u32, Served>) -> u64 {
  let me = client.client_id();
  let mut shed = 0u64;
  for partition in 0..shards {
    loop {
      let report = client.telemetry(partition).unwrap();
      shed = shed.saturating_add(report.shed_before);
      let name = |point: u32| {
        report
          .chokepoints
          .get(usize::try_from(point).unwrap_or(usize::MAX))
          .map(|c| c.name.clone())
          .unwrap_or_default()
      };
      for span in &report.spans {
        if span.request_client != me {
          continue;
        }
        let took = span.end_ns.saturating_sub(span.start_ns);
        let served = into.entry(span.request_sequence).or_default();
        match name(span.point).as_str() {
          "ring.request" => served.ring = served.ring.saturating_add(took),
          "shard.op" => served.op = served.op.saturating_add(took),
          "merge.verdict" => served.verdict = served.verdict.saturating_add(took),
          _ => {}
        }
      }
      if report.remaining == 0 {
        break;
      }
    }
  }
  shed
}

/// The median of `values` (0 for none).
fn median(mut values: Vec<u64>) -> u64 {
  values.sort_unstable();
  values.get(values.len() / 2).copied().unwrap_or(0)
}

/// Where a form's time went (§4.14 spans): for its median samples and for its samples at or above p99 of the total,
/// the median of the client's total, of the ring spans (the server's time from reading the slot to writing the
/// reply), of the shard ops and of the verdicts. The total less the ring is the client's own time and the wake: the
/// doorbell, the shard's wake from its park and the client's wake.
fn attribute(key: &str, samples: &[(Sample, Served, Served)], shed: u64) {
  let total = |s: &(Sample, Served, Served)| s.0.edit.saturating_add(s.0.submit);
  let mut totals: Vec<u64> = samples.iter().map(total).collect();
  totals.sort_unstable();
  let at = |permille: usize| {
    totals
      .get(totals.len().saturating_sub(1) * permille / 1000)
      .copied()
      .unwrap_or(0)
  };
  let (p49, p51, p99) = (at(490), at(510), at(990));
  let unmatched = samples
    .iter()
    .filter(|s| s.1.ring == 0 || s.2.ring == 0)
    .count();
  for (band, keep) in [
    (
      "median",
      Box::new(|t: u64| t >= p49 && t <= p51) as Box<dyn Fn(u64) -> bool>,
    ),
    ("p99_tail", Box::new(|t: u64| t >= p99)),
  ] {
    let chosen: Vec<&(Sample, Served, Served)> =
      samples.iter().filter(|s| keep(total(s))).collect();
    let pick =
      |f: &dyn Fn(&(Sample, Served, Served)) -> u64| median(chosen.iter().map(|s| f(s)).collect());
    let client = pick(&|s| total(s));
    let ring = pick(&|s| s.1.ring.saturating_add(s.2.ring));
    let op = pick(&|s| s.1.op.saturating_add(s.2.op));
    let verdict = pick(&|s| s.2.verdict);
    let outside = pick(&|s| total(s).saturating_sub(s.1.ring.saturating_add(s.2.ring)));
    println!(
      "attrib\tmerge.{key}_{band}: {} samples; client {client} ns, ring {ring} ns, shard ops {op} ns, verdict {verdict} ns, outside the ring {outside} ns (median of each); spans shed {shed}, samples unmatched {unmatched}",
      chosen.len()
    );
  }
}

/// One form on one green shape: per part, per percentile, the per-run values; the attribution lines are printed.
fn measure(
  instance: &str,
  (form, files, load): (Form, usize, &str),
  shards: u16,
) -> Result<BTreeMap<String, Vec<u64>>, String> {
  let mut rows: BTreeMap<String, Vec<u64>> = BTreeMap::new();
  let mut attributed: Vec<(Sample, Served, Served)> = Vec::new();
  let mut shed = 0u64;
  for run in 0..RUNS {
    let tag = format!("{}-{files}-{load}-{run}", form.key());
    let mut fixture = Fixture::new(instance, &tag, files)?;
    let mut edits = Vec::with_capacity(SAMPLES);
    let mut submits = Vec::with_capacity(SAMPLES);
    let mut totals = Vec::with_capacity(SAMPLES);
    let mut run_samples: Vec<Sample> = Vec::with_capacity(SAMPLES);
    let mut served: BTreeMap<u32, Served> = BTreeMap::new();
    // What the shards held before this run is not this run's.
    drain(&mut fixture.client, shards, &mut BTreeMap::new());
    let started = Instant::now();
    for n in 0..SAMPLES {
      if n >= MIN_SAMPLES && started.elapsed() > RUN_BUDGET {
        break;
      }
      let sample = fixture.sample(form, n % files);
      edits.push(sample.edit);
      submits.push(sample.submit);
      totals.push(sample.edit.saturating_add(sample.submit));
      run_samples.push(sample);
      // Drained after every sample (untimed): a shard's span ring holds as many spans as a client region has slots
      // (`crates/server/src/daemon.rs`, `telemetry_capacity`), fewer than a few samples emit; the shed count printed
      // with each form says whether that was enough.
      shed = shed.saturating_add(drain(&mut fixture.client, shards, &mut served));
    }
    shed = shed.saturating_add(drain(&mut fixture.client, shards, &mut served));
    for sample in run_samples {
      let edit = served
        .get(&sample.edit_request)
        .copied()
        .unwrap_or_default();
      let submit = served
        .get(&sample.submit_request)
        .copied()
        .unwrap_or_default();
      attributed.push((sample, edit, submit));
    }
    rows
      .entry("samples".to_owned())
      .or_default()
      .push(u64::try_from(totals.len()).unwrap_or(u64::MAX));
    for (part, samples) in [("edit", edits), ("submit", submits), ("total", totals)] {
      for (pct, value) in percentiles(samples) {
        rows.entry(format!("{part}_{pct}")).or_default().push(value);
      }
    }
    fixture.client.destroy(fixture.leader).unwrap();
    fixture.client.destroy(fixture.follower).unwrap();
    fixture.client.destroy(fixture.green).unwrap();
  }
  attribute(
    &format!("{}_f{files}_{load}", form.key()),
    &attributed,
    shed,
  );
  Ok(rows)
}

fn print_rows(key: &str, rows: &BTreeMap<String, Vec<u64>>) {
  for (name, values) in rows {
    let mut sorted = values.clone();
    sorted.sort_unstable();
    let (lower, median, upper) = (
      sorted[0],
      sorted[sorted.len() / 2],
      sorted[sorted.len() - 1],
    );
    println!("ratchet-info\tmerge.{key}_{name}\t{lower}\t{median}\t{upper}\t{values:?}");
  }
}

fn median_of(rows: &BTreeMap<String, Vec<u64>>, name: &str) -> u64 {
  let mut sorted = rows[name].clone();
  sorted.sort_unstable();
  sorted[sorted.len() / 2]
}

/// A growth form: what each fresh-work sample edits, so a cost that follows the green's file count is told from one
/// that follows its version count.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Growth {
  /// Each sample creates its own file: after sample `n` the green holds `n + 1` files and `n + 1` versions.
  NewFile,
  /// Each sample replaces the one file's line: the green holds one file and `n + 1` versions.
  OneFile,
}

impl Growth {
  const ALL: [Growth; 2] = [Growth::NewFile, Growth::OneFile];

  fn key(self) -> &'static str {
    match self {
      Growth::NewFile => "new_file",
      Growth::OneFile => "one_file",
    }
  }
}

/// Shape: the line a growth sample writes: one line of source.
const LINE: &[u8] = b"    let answer = compute(&input)?; // an agent's one-line edit\n";
/// Shape: the samples compared at each end of a growth run: a tenth of them.
const GROWTH_SHARE: usize = 10;

/// One run of a growth form: `SAMPLES` edit-and-submit cycles on a fresh green, each on a fresh work off the head;
/// the edit and the submit samples, in order.
fn growth_run(client: &mut Client, form: Growth, run: usize) -> (Vec<u64>, Vec<u64>) {
  let green = client
    .create_green(&format!("growth-{}-{run}", form.key()), false)
    .unwrap();
  let mut edits = Vec::with_capacity(SAMPLES);
  let mut merges = Vec::with_capacity(SAMPLES);
  for n in 0..SAMPLES {
    let (work, _) = client.create_work(green, &format!("w-{run}-{n}")).unwrap();
    let (path, replaced) = match form {
      Growth::NewFile => (format!("src/file-{n}.rs"), 0),
      // The first sample creates the file; every later one replaces the line the last put there.
      Growth::OneFile => (
        "src/one.rs".to_owned(),
        if n == 0 { 0 } else { LINE.len() as u64 },
      ),
    };
    let started = Instant::now();
    client.edit(work, &path, 0, replaced, LINE).unwrap();
    edits.push(ns(started.elapsed()));
    let started = Instant::now();
    let verdict = client.submit(work).unwrap();
    merges.push(ns(started.elapsed()));
    // Each work is made off the head and submitted before the next is made, so anything but the next version is
    // a defect of the merge, not noise.
    let wanted = u64::try_from(n).unwrap_or(u64::MAX).saturating_add(1);
    if verdict != Submitted::Accepted(wanted) {
      wrong(&format!(
        "{} sample {n} answered {verdict:?}, not Accepted({wanted})",
        form.key()
      ));
    }
    client.destroy(work).unwrap();
  }
  client.destroy(green).unwrap();
  (edits, merges)
}

/// The median of a run's first and last tenth: a verb whose cost follows the green's history (each sample adds one
/// accepted version) shows it here, where the percentiles mix it away.
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

/// Every selected growth form, its runs, its rows and its goal lines.
fn growth_sweep(instance: &str, selection: &Selection) {
  let mut client = connect(instance);
  client.spin_for(Some(SPIN_NS));
  for form in selection.growth.iter().copied() {
    let mut rows: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for index in 0..RUNS {
      let (edits, merges) = growth_run(&mut client, form, index);
      print_growth(index, &format!("{}.edit", form.key()), &edits);
      print_growth(index, &format!("{}.merge", form.key()), &merges);
      for (verb, samples) in [("edit", edits), ("merge", merges)] {
        for (pct, value) in percentiles(samples) {
          rows.entry(format!("{verb}_{pct}")).or_default().push(value);
        }
      }
    }
    for (name, values) in &rows {
      let mut sorted = values.clone();
      sorted.sort_unstable();
      let (lower, median, upper) = (
        sorted[0],
        sorted[sorted.len() / 2],
        sorted[sorted.len() - 1],
      );
      let (verb, pct) = name.split_once('_').unwrap_or((name, ""));
      println!(
        "ratchet-info\tclient.{}.{verb}_{pct}\t{lower}\t{median}\t{upper}\t{values:?}",
        form.key()
      );
    }
    for verb in ["edit", "merge"] {
      let p99 = median_of(&rows, &format!("{verb}_p99"));
      println!(
        "goal\tclient.{}.{verb}: p50 {} ns, p99 {p99} ns: {}",
        form.key(),
        median_of(&rows, &format!("{verb}_p50")),
        against_goal(p99)
      );
    }
  }
}

/// Where a p99 stands against Ada's two bounds.
fn against_goal(p99: u64) -> &'static str {
  if p99 <= GOAL_PREFERRED_NS {
    "within the preferred 50 us"
  } else if p99 <= GOAL_REQUIRED_NS {
    "within the required 1 ms"
  } else {
    "OVER the required 1 ms"
  }
}

/// What the command line selects: the shapes and forms to run, and whether to run the loaded sweep.
struct Selection {
  /// The daemon's shards, whose spans the attribution drains.
  shards: u16,
  shapes: Vec<usize>,
  forms: Vec<Form>,
  growth: Vec<Growth>,
  loaded: bool,
  /// Whether the quiet sweep runs (`--loaded-only` skips it).
  quiet: bool,
  /// Spinning threads per core in the loaded form (`--spinners-per-core N`).
  spinners_per_core: usize,
}

impl Selection {
  /// `--quiet-only`, `--loaded-only`, `--spinners-per-core N` (the loaded form's spinners, one by default),
  /// `--files N` (repeatable; the shapes instead of [`GREEN_FILES`]) and `--form NAME`
  /// (repeatable; `accept`, `ranged`, `identical`, `conflict`, `rebase`, `new_file`, `one_file`): a profile run selects one
  /// form on one shape. With no `--form`, every form runs.
  fn parse() -> Selection {
    let mut selection = Selection {
      shards: 0,
      shapes: Vec::new(),
      forms: Vec::new(),
      growth: Vec::new(),
      loaded: true,
      quiet: true,
      spinners_per_core: 1,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
      match arg.as_str() {
        "--quiet-only" => selection.loaded = false,
        "--loaded-only" => selection.quiet = false,
        "--spinners-per-core" => {
          selection.spinners_per_core = args
            .next()
            .and_then(|n| n.parse().ok())
            .expect("--spinners-per-core takes a count");
        }
        "--files" => selection.shapes.push(
          args
            .next()
            .and_then(|n| n.parse().ok())
            .expect("--files takes a count"),
        ),
        "--form" => {
          let name = args.next().expect("--form takes a name");
          if let Some(form) = Form::ALL.into_iter().find(|form| form.key() == name) {
            selection.forms.push(form);
          } else if let Some(form) = Growth::ALL.into_iter().find(|form| form.key() == name) {
            selection.growth.push(form);
          } else {
            panic!("--form is accept, ranged, identical, conflict, rebase, new_file or one_file");
          }
        }
        other => panic!("unknown argument {other}"),
      }
    }
    if selection.shapes.is_empty() {
      selection.shapes = GREEN_FILES.to_vec();
    }
    if selection.forms.is_empty() && selection.growth.is_empty() {
      selection.forms = Form::ALL.to_vec();
      selection.growth = Growth::ALL.to_vec();
    }
    selection
  }
}

/// Runs every selected form over every selected shape, under `load`.
fn sweep(instance: &str, load: &str, selection: &Selection) {
  for files in &selection.shapes {
    for form in selection.forms.iter().copied() {
      let key = format!("{}_f{files}_{load}", form.key());
      let rows = match measure(instance, (form, *files, load), selection.shards) {
        Ok(rows) => rows,
        Err(refused) => {
          println!("goal\tmerge.{key}: refused at capacity, not measured: {refused}");
          continue;
        }
      };
      print_rows(&key, &rows);
      let p99 = median_of(&rows, "total_p99");
      let verdict = against_goal(p99);
      println!(
        "goal\tmerge.{key}: {:?} samples, total p50 {} ns, p99 {p99} ns, p999 {} ns, max {} ns: {verdict}",
        rows["samples"],
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
  let cores = profile.facts.cpus_at_once();
  println!(
    "edit and merge latency: {} {}, {shards} shards, {cores} cores, client spin {SPIN_NS} ns, {SAMPLES} samples x {RUNS} runs, edits of {EDIT_BYTES} B in files of {FILE_BYTES} B",
    std::env::consts::OS,
    std::env::consts::ARCH
  );
  let mut selection = Selection::parse();
  selection.shards = shards;
  if selection.quiet {
    sweep(&instance, "quiet", &selection);
    growth_sweep(&instance, &selection);
  }
  if selection.loaded {
    // The loaded form: `spinners_per_core` spinning threads per core this process can run (one by default; Ada's
    // controlled load is two), owned and joined here.
    let stop = AtomicBool::new(false);
    let spinners = cores.saturating_mul(selection.spinners_per_core);
    println!("loaded: {spinners} spinning threads on {cores} cores");
    std::thread::scope(|scope| {
      for _ in 0..spinners {
        scope.spawn(|| {
          while !stop.load(Ordering::Relaxed) {
            std::hint::spin_loop();
          }
        });
      }
      sweep(&instance, "loaded", &selection);
      stop.store(true, Ordering::Relaxed);
    });
  }
  daemon.stop();
}
