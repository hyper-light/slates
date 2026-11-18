//! The many-client load test (§4.16 "Submission", "Rebase, the only corrective path"; §4.7; Phase 9 "Scale, soak,
//! chaos"): N clients, each its own connection, each creating a work volume over one green whose version 0 is a real
//! source tree (the latest Linux kernel), editing files with deliberate overlaps so that conflicts happen, submitting,
//! and resolving every conflict through the design's corrective path. Every operation's latency is recorded from the
//! moment its request is sent to the moment its reply is taken, and reported per kind as p50, p99, p999 and max.
//!
//! **The workload.** Each client runs `rounds` rounds. A round picks `edits` slots — a fixed-length, slot-aligned
//! range of one file — from a hot set of files (so clients collide) or from the whole tree (so most work is
//! disjoint), overwrites each with bytes that name the client and the round, and submits. The hot set is `--hot-files`
//! distinct files, each as hot as the next, and at most half the files long enough to edit, so a pick from the whole
//! tree misses it at least half the time; a larger one is refused (a `refused` row, exit 1). One hot slot is written
//! with the same bytes by every client that picks it, so the per-range identity rule (pass two's memcmp) is driven
//! too: those overlaps must be accepted, never reported as conflicts. Edits are length-preserving and slot-aligned,
//! so a conflict window, the client's range and the intervening range coincide, and the oracle below states the
//! design's rule exactly.
//!
//! **Conflict handling, as a client must do it.** On `Conflict { windows }` the client reads the green's head bytes
//! of each window's file, writes those bytes into its own work over the window (its edit there now equals the
//! green's, which pass two accepts as identical), rebases, writes its own bytes over the window again on the new
//! base, and submits again — at most `MAX_RESOLUTIONS` times a round; a client that runs out is counted as starved,
//! never hidden.
//!
//! **The oracle (offline, after the run; `support/load_oracle.rs` states it exactly).** Every submit is recorded with
//! its base, its send and reply instants, its slots and its outcome. The accepted versions must be exactly `1..=V`,
//! each once. An accepted submit at `v` must have no slot that an accepted version in `(base, v)` touched while the
//! green's bytes at `v - 1` differ from the slot's (the verdict's rule). A conflict's windows must equal the rule's
//! expected set at some head the verdict could have seen, bounded by the accepted submits whose replies came back
//! before the submit was sent and those sent before its reply came back. And the green's head bytes of every file the
//! run touched must equal the version-ordered replay of the accepted submits over the files' bytes before the level.
//!
//! **The offered load (`--think-us <mean>`).** A client waits for each reply before its next request, as an agent does,
//! and then thinks: a pause drawn afresh after every reply from the exponential distribution of the stated mean, and
//! one draw before its first request, so no two clients start or stay in step. Many independent clients' requests
//! together approach a Poisson stream whatever each one's own pauses (the Palm–Khintchine theorem), so this is the
//! stream a fleet of independent agents offers. The fixed pause it replaced sent every client's first request at the
//! same instant and kept the clients bunched after, which no fleet of independent agents does. A mean of 0 is the
//! closed loop at capacity: every client always waits on a request or is ready to send one, so its latency is the
//! clients over the throughput (Little's law), not a service time.
//!
//! **Where each client's time went.** A client not yet done is at each instant in flight (a request sent, its reply
//! not yet taken), thinking, pausing after a refusal for now, or held: due to send and not yet sent, by the harness's
//! sweep or by its connection's ring or in-flight bound. Each is summed where it happens (a held client's wait at its
//! send, from the instant it was due), and the `offered` row gives each as the time-averaged number of clients, with
//! the replies and verdicts a second. A latency is stamped at a send, so the time a client is held before a send is in
//! no `latency` row, and before an operation's first send in no `operation` row either: a level with clients held
//! measures less than its clients wait, and says so here.
//!
//! **Each instant is read on the side of its event that keeps the oracle's bounds sound.** A send is stamped before
//! its request is handed to the connection's ring, and read again once it is, so the request left between the two; a
//! reply is stamped after the ring it was taken from was read, so it arrived before. Stamped the other way round (as
//! until 2026-11-18), a worker the host preempted between handing a submit over and reading the clock recorded a send
//! after the verdict that reflected it, and one preempted between reading the clock and reading the ring recorded a
//! reply before it came: the measurer's three oracle failures under load, one of them a recorded round trip of 5 µs
//! where the run's submits took 374 to 399 µs (BENCHMARKS, 2026-11-18). The clock is the host's common monotonic one
//! (`slates_machine::clock::monotonic_ns`), the clock the daemon stamps its spans in.
//!
//! **What the harness itself may have added (the `floor` rows).** A reply not taken by a connection's drain was not
//! there when that drain began, so it arrived after the later of that instant and its request's handing over: the
//! attempt's `floor`, a lower bound on what the daemon and the ring took, whatever the harness's own sweep did. Each
//! `latency` lies between its `floor` and itself, and the gap is the harness's resolution: the longest such window of
//! the level is `max_drain_gap_ns` in the `harness` row. A tail in `latency` with none in `floor` may be the harness's;
//! a tail in `floor` is not.
//!
//! **The bound (the `bound` row; Ada's decision of 2026-11-18, A-297).** "1,000 clients under 1 ms" is stated at up to
//! 10³ verdicts a second, the floor of §1.4's fleet-session row ("Increments per green volume per second: thousands").
//! A level meets it when every kind's p99, each attempt's (`latency`) and each operation's (`operation`), is under 1 ms
//! while the level's verdicts a second are at least 10³, with no oracle failure and no client given up or starved. The
//! kinds are every request and the cycle, a whole edit and merge (§1.4's status under A-297). The round is reported in
//! the `latency` row and not judged (A-416): it holds as many edits and merges as its conflicts make, so its p99 grows
//! with the conflict rate the hot set sets, not with any one request's time. The row names the worst row and kind with
//! its p99, and the held clients beside them, whose wait no latency row holds. A cycle and a round leave out their
//! client's own think pauses, which are the client's time, not the service's: left in, a cycle at the pace that offers
//! 10³ verdicts a second from 1,000 clients holds five pauses of about 200 ms, and no paced level could meet the bound.
//! At a think mean of 0 they are the cycle's and the round's wall.
//!
//! Rows: `refused\t<n>\t<why>` for a level whose arguments cannot be run as they say (a hot set past its share, above),
//! which runs nothing and exits 1; `offered\t<n>\t…` (above); `latency\t<n>\t<kind>\t<count>\t<p50>\t<p99>\t<p999>\t<max>` in nanoseconds, each
//! attempt timed alone; then `operation\t…` in the same columns, each logical operation from its first attempt to its
//! answer, refusals for now and resends included; then `floor\t…` in the same columns, each attempt's floor (above);
//! then `oracle\t<n>\t…` lines; a nonzero `oracle_failures` exits 1;
//! then `bound\t<n>\t…` (above). Last, from the daemon's own report: `lost\t<counter>\t<count>` for each of its
//! counters of a lost reply, result, release or ask; `room\t<counter>\t<count>` for its waits for room (begun and
//! ended, summed over the shards, and the most one shard held at once, goal 238); and `counter\t<counter>\t<count>` for
//! where a work's submissions were composed.
// Bench harness code: an unwrap here is a failed run.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use slates_client::{
  Client, ClientError, CreateSpec, Deadlines, NamePolicy, ReadAt, SizeClass, VolumeId,
};
use slates_ipc::protocol::{EntryKind, GreenBase, MergeWindow, Refusal, ReplyBody, RequestBody};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

// The oracle: the design's rule over the level's submits, in its own file so its test
// (`crates/client/tests/load_oracle.rs`) names the same code.
#[path = "support/load_oracle.rs"]
mod load_oracle;
use load_oracle::{Outcome, Slot, SubmitEvent};

/// Shape: rounds of edit-and-submit each client runs, unless `--rounds` says otherwise.
const ROUNDS: usize = 3;
/// Shape: slots each round edits.
const EDITS_PER_ROUND: usize = 4;
/// Shape: the hot files every client may pick, where collisions happen, unless `--hot-files` says otherwise (a run at
/// a larger N scales the hot set with N to hold the conflict rate a client meets).
const HOT_FILES: usize = 8;
/// Shape: slots per file a client picks among (slot-aligned, so overlaps are exact).
const SLOTS_PER_FILE: u64 = 4;
/// Shape: a slot's length in bytes.
const SLOT_BYTES: u64 = 64;
/// Shape: the distance between slot starts, two slots, so no two slots are adjacent: the deriver composes adjacent
/// overwrites into one operation (one window), which this oracle's per-slot rule does not model.
const SLOT_STRIDE: u64 = 2 * SLOT_BYTES;
/// Shape: the chance, in parts per thousand, that an edit lands in the hot set.
const HOT_PERMILLE: u64 = 500;
/// Shape: the hot set's largest share of the eligible files, as one over this: half. A pick from the whole tree then
/// misses the hot set at least half the time, so the tree's picks stay mostly disjoint, the workload's other half (the
/// module's "The workload"); a larger `--hot-files` is refused ([`hot_set`]).
const HOT_SET_SHARE_DIVISOR: usize = 2;
/// Shape: how many times a round resolves a conflict and submits again before the client counts as starved.
const MAX_RESOLUTIONS: usize = 64;
/// Shape: how long a client retries the rendezvous while the daemon comes up or its claim slots clear under a
/// burst of concurrent connects.
const CONNECT_WAIT: Duration = Duration::from_secs(30);
/// Format: the percentiles reported, in parts per thousand.
const PERCENTILES: &[u64] = &[500, 990, 999];
/// Format: parts per thousand.
const PERMILLE: u64 = 1000;
/// Shape: the seed of the deterministic plan (splitmix64), so a run is reproducible.
const SEED: u64 = 0x5eed_1a7e_57c0_ffee;
/// The bound a level is judged against: every kind's p99 under 1 ms (RQ-4's end-to-end bound; the goal "1,000 clients
/// under 1 ms", stated by Ada on 2026-11-18 at the offered load below; A-297).
const BOUND_P99_NS: u64 = 1_000_000;
/// The offered load the bound is stated at: at least 10³ verdicts a second, the floor of §1.4's fleet-session row
/// ("Increments per green volume per second: tens (a few agents) | thousands (a fleet session)", hecate MERGE.md §7),
/// Ada's decision of 2026-11-18. Below it a level measures a lighter load than the bound names, so it cannot meet it.
const BOUND_VERDICTS_PER_S: u128 = 1_000;
/// Format: the percentile the bound judges, in parts per thousand: the p99.
const BOUND_PERMILLE: u64 = 990;

/// The kinds of operation timed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
  CreateWork,
  Edit,
  SubmitAccepted,
  SubmitConflict,
  ReadHead,
  Rebase,
  Destroy,
  /// A round's edit and merge as its client sees it, its own think pauses left out: from sending its first edit to its
  /// submit's first verdict, an accept or a conflict, every request between included (the goal's own measure: a whole
  /// edit and merge). A refusal's pause and a held send stay in: the service or the harness kept the client waiting.
  Cycle,
  /// A round from its first edit to its accept, conflicts and their resolution included, its own think pauses left
  /// out as the cycle's are. Reported, never judged against the bound ([`Kind::judged`]).
  Round,
}

impl Kind {
  /// Whether the bound judges this kind (A-297, A-416): every request and the cycle, a whole edit and merge. Not the
  /// round, which holds as many edits and merges as its conflicts make, so its time grows with the conflict rate the
  /// hot set sets rather than with any one request's.
  fn judged(self) -> bool {
    self != Kind::Round
  }
}

/// What a client is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
  CreateWork,
  Edit(usize),
  /// The round's record appended to the client's own file: an edit that is not idempotent, so a request applied twice
  /// (or an acknowledged one lost) shows in the bytes.
  Append,
  Submit,
  /// A conflict's resolution: the green's head version, read first so every window is read at one version.
  ResolveVersion,
  /// The window `n`'s bytes at that version (one `ReadRange` page: a window is a slot, far below a page).
  ResolveRead(usize),
  /// Window `n` written over in the work with the green's bytes, so pass two finds it identical.
  ResolveEdit(usize),
  Rebase,
  Reedit(usize),
  Destroy,
  Done,
}

/// One logical client and its script: its own work volume, its own rounds, merges and conflicts, one request in
/// flight at a time, multiplexed with others over one of its worker's connections (`--per-connection`).
struct Agent {
  /// The length of the client's own file in its work: the records of its accepted rounds.
  own_len: u64,
  /// The green's version a resolution reads its windows at, and the window bytes it read.
  resolving: (u64, Vec<u8>),
  id: usize,
  /// The worker-local index of the connection this client's requests go over.
  conn: usize,
  green: VolumeId,
  work: Option<VolumeId>,
  base: u64,
  rounds: Vec<Vec<Slot>>,
  round: usize,
  step: Step,
  windows: Vec<MergeWindow>,
  resolutions: usize,
  /// The request in flight: its word, its send instant and its kind.
  inflight: Option<(u64, u64, Kind)>,
  /// A request the daemon refused retryable under its own id (`Overloaded`, `OwnerUnreachable`, `Unpublished`), to
  /// send again under that word: its word, its first send's instant and its kind. The verb may have run (a forwarded
  /// submit waiting on the fleet commit is answered `Overloaded` while its acceptance commits), and only the same id
  /// meets its completion record; the client keeps its acknowledgement below the id until the resend.
  retry: Option<(u64, u64, Kind)>,
  /// When the round's first edit was first sent: the start of its [`Kind::Cycle`] (cleared at its first verdict) and of
  /// its [`Kind::Round`] (cleared at its accept).
  cycle_since: Option<u64>,
  round_since: Option<u64>,
  /// The think pauses the client drew while its cycle and its round were open, left out of each ([`Agent::thought`]).
  cycle_thought_ns: u64,
  round_thought_ns: u64,
  /// When the current step's first attempt was sent, kept across a refusal for now and its resend.
  op_since: Option<u64>,
  /// The pause before this client sends again after a refusal for now (`LeaseUnconfirmed`, `AcknowledgementOwed`, a
  /// refusal retryable under its id), doubling while they repeat (`Client::retry_pause_ns`), zero once a reply moves
  /// it on. Resent at once, 1,000 clients refused `LeaseUnconfirmed` after a primary's kill sent 20,000 a second.
  pause_ns: u64,
  /// When the request in flight was sent, this attempt alone (a retry under its id keeps its first send in `inflight`
  /// for its latency): the start of the client's time in flight.
  attempt_sent_ns: u64,
  /// When this attempt's request was in its ring: the clock read once the send returned, so the request left at or
  /// before it. A reply's floor is measured from it ([`Worker::drain`]).
  handed_ns: u64,
}

/// What a worker brings back.
#[derive(Default)]
struct Tally {
  /// Replies taken: a worker's sign of motion, for its stall dump.
  answered: u64,
  latencies: BTreeMap<Kind, Vec<u64>>,
  /// Each logical operation's latency from its first attempt to its answer: a request refused for now
  /// (`AcknowledgementOwed`, `LeaseUnconfirmed`) and sent again as new, or retried under its id, is one operation whose
  /// wait includes the refusals, the pauses and the resends. `latencies` times each attempt alone.
  operations: BTreeMap<Kind, Vec<u64>>,
  /// Each answered attempt's floor, by the kind its latency is filed under: how long after its request was in the ring
  /// its reply was still not there, which no sweep of the harness can have added (the module's "floor").
  floors: BTreeMap<Kind, Vec<u64>>,
  submits: Vec<SubmitEvent>,
  /// The clients that gave up, by why ([`Agent::give_up`]): a refusal the harness does not ride out, a lost connection,
  /// a retry past its patience.
  refusals: BTreeMap<String, u64>,
  starved: u64,
  rebase_conflicts: u64,
  max_sweep_ns: u64,
  /// Reconnects after the daemon was found gone, requests resent under their own ids, and the longest outage a client
  /// rode out: the non-vacuity counters of the restart path (a chaos run that resent nothing tested nothing).
  reconnects: u64,
  resent: u64,
  max_outage_ns: u64,
  /// Requests the daemon refused retryable under their own id and the client sent again under it.
  retried: u64,
  /// Requests the daemon refused `LeaseUnconfirmed` (not run) and the client sent again as new.
  unconfirmed: u64,
  /// Requests the daemon refused `AcknowledgementOwed` and the client sent again: the multiplexed connection's
  /// backpressure while an older request of the connection is still awaited (its acknowledgement cannot pass it).
  backpressure: u64,
  /// The longest window a reply's arrival could not be placed in: from the later of its request's handing over and the
  /// start of the drain before the one that took it, to the stamp after the one that took it. The resolution of every
  /// latency here (each lies within this of its floor). A connection with nothing in flight is not drained, and its
  /// idle time is in no window (it was, before 2026-11-18, so a gap of seconds was a connection whose clients thought).
  max_drain_gap_ns: u64,
  /// Where the clients' time went, summed over clients in client-nanoseconds: in flight, thinking (the pauses drawn),
  /// pausing after a refusal for now, and held (due to send and not yet sent, [`Worker::send_ready`]). A request given
  /// up while in flight is counted in none.
  in_flight_ns: u64,
  thinking_ns: u64,
  paused_ns: u64,
  held_ns: u64,
}

impl Tally {
  /// Records one logical operation of `kind`, from its first send to its final answer, resends included.
  fn operation_took(&mut self, kind: Kind, ns: u64) {
    self.operations.entry(kind).or_default().push(ns);
  }

  /// Adds a worker's tally into this one.
  fn absorb(&mut self, mut tally: Tally) {
    for (kind, mut samples) in std::mem::take(&mut tally.latencies) {
      self.latencies.entry(kind).or_default().append(&mut samples);
    }
    for (kind, mut samples) in std::mem::take(&mut tally.operations) {
      self
        .operations
        .entry(kind)
        .or_default()
        .append(&mut samples);
    }
    for (kind, mut samples) in std::mem::take(&mut tally.floors) {
      self.floors.entry(kind).or_default().append(&mut samples);
    }
    self.submits.append(&mut tally.submits);
    for (why, count) in tally.refusals {
      *self.refusals.entry(why).or_default() += count;
    }
    self.starved += tally.starved;
    self.rebase_conflicts += tally.rebase_conflicts;
    self.max_sweep_ns = self.max_sweep_ns.max(tally.max_sweep_ns);
    self.reconnects += tally.reconnects;
    self.resent += tally.resent;
    self.max_outage_ns = self.max_outage_ns.max(tally.max_outage_ns);
    self.backpressure += tally.backpressure;
    self.retried += tally.retried;
    self.unconfirmed += tally.unconfirmed;
    self.max_drain_gap_ns = self.max_drain_gap_ns.max(tally.max_drain_gap_ns);
    self.answered = self.answered.saturating_add(tally.answered);
    self.in_flight_ns = self.in_flight_ns.saturating_add(tally.in_flight_ns);
    self.thinking_ns = self.thinking_ns.saturating_add(tally.thinking_ns);
    self.paused_ns = self.paused_ns.saturating_add(tally.paused_ns);
    self.held_ns = self.held_ns.saturating_add(tally.held_ns);
  }
}

/// splitmix64: a reproducible plan without a dependency.
struct Rng(u64);

/// Format: the uniform draws behind a think: a draw's high 32 bits, offset by half a step and divided by 2³², is
/// uniform on (0, 1) and never 0, so its logarithm is finite.
const UNIFORM_STEPS: f64 = 4_294_967_296.0;
/// Format: half of one uniform step (above).
const HALF_STEP: f64 = 0.5;
/// Format: the bits a 64-bit draw is shifted down by to keep its high 32.
const HIGH_HALF: u32 = 32;

impl Rng {
  fn next(&mut self) -> u64 {
    self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = self.0;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
  }
  fn below(&mut self, n: u64) -> u64 {
    self.next() % n.max(1)
  }

  /// A draw from the exponential distribution of mean `mean_ns`, in nanoseconds, by inversion (`-mean · ln U`, `U`
  /// uniform on (0, 1)); 0 for a mean of 0. The largest draw is `mean · ln 2³³`, about 23 means, past which the
  /// distribution holds e⁻²³ of its mass, about 10⁻¹⁰.
  fn exponential_ns(&mut self, mean_ns: u64) -> u64 {
    if mean_ns == 0 {
      return 0;
    }
    let high = u32::try_from(self.next() >> HIGH_HALF).unwrap_or(u32::MAX);
    let uniform = (f64::from(high) + HALF_STEP) / UNIFORM_STEPS;
    let seconds = Duration::from_nanos(mean_ns).as_secs_f64() * -uniform.ln();
    Duration::try_from_secs_f64(seconds).map_or(u64::MAX, |pause| {
      u64::try_from(pause.as_nanos()).unwrap_or(u64::MAX)
    })
  }
}

/// The harness's clock: a reading of the host's common monotonic clock, in nanoseconds (the module's "Each instant").
type ClockRead = Box<dyn FnMut() -> u64 + Send>;

/// A worker's clock: nanoseconds since the level's start, read through `read`, which is the host's monotonic clock in a
/// run and a test's own in a test (a host that preempts the worker between two of its steps, in particular).
struct Clock {
  read: ClockRead,
  /// The level's start, on `read`'s scale.
  epoch: u64,
}

impl Clock {
  /// The host's common monotonic clock, counted from `epoch`, one of its own readings.
  fn host(epoch: u64) -> Clock {
    Clock {
      read: Box::new(slates_machine::clock::monotonic_ns),
      epoch,
    }
  }

  /// Nanoseconds since the level's start.
  fn now(&mut self) -> u64 {
    (self.read)().saturating_sub(self.epoch)
  }
}

fn deadlines() -> Deadlines {
  Deadlines::derive(
    slates_server::daemon::LIVENESS_BUDGET_NS,
    slates_db::replay::RECOVERY_BUDGET_NS,
  )
  .get()
}

/// Connects, retrying the rendezvous while the daemon is coming up or its claim slots are contended.
fn connect(instance: &str) -> Result<Client, ClientError> {
  let started = Instant::now();
  loop {
    match Client::connect(instance, deadlines()) {
      Err(ClientError::Ipc(
        slates_ipc::IpcError::DaemonUnavailable { .. } | slates_ipc::IpcError::RingFull,
      )) if started.elapsed() < CONNECT_WAIT => std::thread::yield_now(),
      other => return other,
    }
  }
}

/// The first of a comma-separated `--instance` list: the node the harness's own setup and oracle reads go through.
fn first_instance(instances: &str) -> &str {
  instances.split(',').next().unwrap_or(instances)
}

/// Every file of the green's version 0, with its length, walked through the daemon (`ReadDir`).
fn walk(client: &mut Client, green: VolumeId) -> Vec<(String, u64)> {
  let mut files = Vec::new();
  let mut dirs = vec![String::new()];
  while let Some(dir) = dirs.pop() {
    // Patient: a kill rung's node may go down while the setup walks (fixtures' 1,000-client primary kill answered a
    // listing `OwnerUnreachable` 23 s in, 2026-11-07).
    let entries = patiently(client, &format!("list {dir}"), |client| {
      client.list_dir(green, &dir, ReadAt::Version { version: 0 })
    });
    for entry in entries {
      let path = if dir.is_empty() {
        entry.name.clone()
      } else {
        format!("{dir}/{}", entry.name)
      };
      match entry.kind {
        EntryKind::Dir => dirs.push(path),
        EntryKind::File => files.push((path, entry.size)),
        _ => {}
      }
    }
  }
  files.sort();
  files
}

/// The file only client `agent` appends to.
fn own_path(agent: usize) -> String {
  format!("loadtest-own-{agent}")
}

/// The record client `agent` appends in `round`: unique, so a duplicate or a loss shows.
fn own_record(agent: usize, round: usize) -> Vec<u8> {
  format!("{agent}:{round}\n").into_bytes()
}

/// Each client's own file as the green's head holds it must be its accepted rounds' records, each once, in round
/// order. Returns the failures, each printed.
fn own_files_oracle(
  n: usize,
  client: &mut Client,
  green: VolumeId,
  submits: &[SubmitEvent],
) -> u64 {
  let mut rounds: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
  for event in submits
    .iter()
    .filter(|event| matches!(event.outcome, Outcome::Accepted(_)))
  {
    rounds.entry(event.agent).or_default().push(event.round);
  }
  let mut failures = 0u64;
  for (agent, mut accepted) in rounds {
    accepted.sort_unstable();
    let expected: Vec<u8> = accepted
      .iter()
      .flat_map(|round| own_record(agent, *round))
      .collect();
    let held = read_patiently(client, green, &own_path(agent), ReadAt::Head);
    if held != expected {
      failures += 1;
      if failures <= 20 {
        println!(
          "oracle_failure\t{n}\tclient {agent}'s own file holds {:?}, its accepted rounds {accepted:?} append {:?}",
          String::from_utf8_lossy(&held),
          String::from_utf8_lossy(&expected)
        );
      }
    }
  }
  println!("oracle\t{n}\town_files_failures\t{failures}");
  failures
}

/// The slot bytes client `agent` writes in `round` at `(file, slot)`: unique to the three, except the shared slot.
fn slot_bytes(agent: usize, round: usize, shared: bool) -> Vec<u8> {
  let tag = if shared {
    "shared-identical-bytes".to_owned()
  } else {
    format!("c{agent:06}r{round:03}")
  };
  let mut bytes = tag.into_bytes();
  bytes.resize(usize::try_from(SLOT_BYTES).unwrap(), b'.');
  bytes
}

/// The level's hot set (the module's "The workload"): `hot_files` distinct indices into the `eligible` files, drawn
/// without replacement by the plan's own draws (the first `hot_files` places of a Fisher–Yates shuffle), or why there
/// can be none. A hot pick is uniform over the set's entries, so a file drawn twice was twice as hot as the rest: 800
/// draws with replacement over `kernel/`'s 700-odd eligible files named every file, some three to five times, and 19 to
/// 28 of 1,000 clients starved (the Mac Studio, 2026-11-18). And a set of more than one `HOT_SET_SHARE_DIVISOR`th of
/// the eligible files leaves the whole tree's picks no longer mostly outside it, so it is refused, never shrunk: a run
/// is what its arguments say or does not run.
fn hot_set(rng: &mut Rng, eligible: usize, hot_files: usize) -> Result<Vec<usize>, String> {
  if hot_files > eligible / HOT_SET_SHARE_DIVISOR {
    return Err(format!(
      "--hot-files {hot_files} is more than 1/{HOT_SET_SHARE_DIVISOR} of the {eligible} files long enough to edit: a hot set is at most {} of them",
      eligible / HOT_SET_SHARE_DIVISOR
    ));
  }
  let mut order: Vec<usize> = (0..eligible).collect();
  for place in 0..hot_files {
    let rest = u64::try_from(eligible - place).map_err(|e| e.to_string())?;
    let drawn = place + usize::try_from(rng.below(rest)).map_err(|e| e.to_string())?;
    order.swap(place, drawn);
  }
  order.truncate(hot_files);
  Ok(order)
}

/// One round's slots for one client: `EDITS_PER_ROUND` distinct `(file, slot)` pairs. With no hot set every pick is
/// from the whole tree.
fn plan_round(
  rng: &mut Rng,
  files: &[(String, u64)],
  hot: &[usize],
  agent: usize,
  round: usize,
) -> Vec<Slot> {
  let mut picked: BTreeSet<(usize, u64)> = BTreeSet::new();
  while picked.len() < EDITS_PER_ROUND {
    let file = if !hot.is_empty() && rng.below(PERMILLE) < HOT_PERMILLE {
      hot[usize::try_from(rng.below(hot.len() as u64)).unwrap()]
    } else {
      usize::try_from(rng.below(files.len() as u64)).unwrap()
    };
    picked.insert((file, rng.below(SLOTS_PER_FILE)));
  }
  picked
    .into_iter()
    .map(|(file, slot)| Slot {
      path: files[file].0.clone(),
      at: slot * SLOT_STRIDE,
      bytes: slot_bytes(agent, round, Some(&file) == hot.first() && slot == 0),
    })
    .collect()
}

impl Agent {
  fn slots(&self) -> &[Slot] {
    &self.rounds[self.round]
  }

  /// The request for the current step, or none when done.
  fn request(&self) -> Option<(RequestBody, Kind)> {
    let work = self.work;
    Some(match self.step {
      Step::CreateWork => (
        RequestBody::CreateWork {
          green: self.green,
          name: format!("load-{}", self.id),
        },
        Kind::CreateWork,
      ),
      Step::Append => (
        RequestBody::Edit {
          work: work?,
          path: own_path(self.id),
          at: self.own_len,
          delete_len: 0,
          bytes: own_record(self.id, self.round),
        },
        Kind::Edit,
      ),
      Step::Edit(n) | Step::Reedit(n) => {
        let slot = &self.slot_for_step(n);
        (
          RequestBody::Edit {
            work: work?,
            path: slot.path.clone(),
            at: slot.at,
            delete_len: SLOT_BYTES,
            bytes: slot.bytes.clone(),
          },
          Kind::Edit,
        )
      }
      Step::Submit => (
        RequestBody::Submit {
          work: work?,
          evidence: Vec::new(),
        },
        Kind::SubmitAccepted,
      ),
      Step::ResolveVersion => (RequestBody::Versions { green: self.green }, Kind::ReadHead),
      Step::ResolveRead(n) => {
        let window = &self.windows[n];
        (
          RequestBody::ReadRange {
            volume: self.green,
            path: window.path.clone(),
            at: ReadAt::Version {
              version: self.resolving.0,
            },
            offset: window.at,
            max: window.len,
          },
          Kind::ReadHead,
        )
      }
      Step::ResolveEdit(n) => {
        let window = &self.windows[n];
        (
          RequestBody::Edit {
            work: work?,
            path: window.path.clone(),
            at: window.at,
            delete_len: window.len,
            bytes: self.resolving.1.clone(),
          },
          Kind::Edit,
        )
      }
      Step::Rebase => (RequestBody::Rebase { work: work? }, Kind::Rebase),
      Step::Destroy => (RequestBody::Destroy { volume: work? }, Kind::Destroy),
      Step::Done => return None,
    })
  }

  /// The slot an edit step writes: a round's own slot. A re-edit after a rebase writes every own slot of the round
  /// again (the resolution wrote the green's bytes over each window, and a window may span several slots).
  fn slot_for_step(&self, n: usize) -> Slot {
    self.slots()[n].clone()
  }

  fn give_up(&mut self, tally: &mut Tally, why: &str) {
    *tally.refusals.entry(why.to_owned()).or_default() += 1;
    // A submit in flight when the client gives up has an outcome the client never learned: kept, so the oracle can
    // settle it from the green rather than take it for never accepted.
    if let Some((_, sent_ns, Kind::SubmitAccepted)) = self.inflight {
      tally.submits.push(SubmitEvent {
        base: self.base,
        sent_ns,
        replied_ns: sent_ns,
        slots: self.slots().to_vec(),
        outcome: Outcome::Unknown,
        agent: self.id,
        round: self.round,
        classes: Vec::new(),
      });
    }
    self.step = Step::Done;
    self.inflight = None;
  }

  /// Moves the client on with `reply`, the answer to its attempt sent at `sent_ns` and taken at `replied_ns`.
  /// `first_ns` is when the operation's first attempt was sent: a round's cycle starts there, so a refusal for now of
  /// its first edit, and the pause after it, are in the cycle.
  fn advance(
    &mut self,
    reply: ReplyBody,
    (sent_ns, replied_ns, kind): (u64, u64, Kind),
    first_ns: u64,
    tally: &mut Tally,
  ) {
    let elapsed = replied_ns.saturating_sub(sent_ns);
    match (self.step, reply) {
      (_, ReplyBody::Refused { refusal }) => self.refused(&refusal, kind, tally),
      (Step::CreateWork, ReplyBody::WorkCreated { id, base }) => {
        self.work = Some(id);
        self.base = base;
        self.step = Step::Edit(0);
        record(tally, kind, elapsed);
      }
      (Step::Edit(n), ReplyBody::Edited) => {
        if n == 0 && self.round_since.is_none() {
          self.cycle_since = Some(first_ns);
          self.round_since = Some(first_ns);
        }
        self.step = if n + 1 < self.slots().len() {
          Step::Edit(n + 1)
        } else {
          Step::Append
        };
        record(tally, kind, elapsed);
      }
      (Step::Append, ReplyBody::Edited) => {
        self.step = Step::Submit;
        record(tally, kind, elapsed);
      }
      // An agent's work conflicts in its few slots, far under what one reply carries, so no window is ever counted past
      // the reply (A-267's `conflicts_elided`).
      (
        Step::Submit,
        ReplyBody::Submitted {
          version, conflicts, ..
        },
      ) => {
        self.submitted(version, conflicts, (sent_ns, replied_ns), tally);
      }
      (Step::ResolveVersion, ReplyBody::Versions { head }) => {
        record(tally, kind, elapsed);
        self.resolving.0 = head;
        self.step = Step::ResolveRead(0);
      }
      (Step::ResolveRead(n), ReplyBody::ReadPage { bytes, .. }) => {
        record(tally, kind, elapsed);
        if bytes.len() != usize::try_from(self.windows[n].len).unwrap() {
          return self.give_up(tally, "a window's read came back short of the window");
        }
        self.resolving.1 = bytes;
        self.step = Step::ResolveEdit(n);
      }
      (Step::ResolveEdit(n), ReplyBody::Edited) => {
        record(tally, kind, elapsed);
        self.step = if n + 1 < self.windows.len() {
          Step::ResolveRead(n + 1)
        } else {
          Step::Rebase
        };
      }
      (
        Step::Rebase,
        ReplyBody::Rebased {
          version, conflicts, ..
        },
      ) => {
        record(tally, kind, elapsed);
        self.rebased(version, conflicts, tally);
      }
      (Step::Reedit(n), ReplyBody::Edited) => {
        record(tally, kind, elapsed);
        self.step = if n + 1 < self.slots().len() {
          Step::Reedit(n + 1)
        } else {
          Step::Submit
        };
      }
      (Step::Destroy, ReplyBody::Destroyed) => {
        record(tally, kind, elapsed);
        self.step = Step::Done;
      }
      (step, other) => self.give_up(tally, &format!("unexpected reply at {step:?}: {other:?}")),
    }
  }

  fn refused(&mut self, refusal: &Refusal, kind: Kind, tally: &mut Tally) {
    // A request refused `AcknowledgementOwed` was never run or recorded (`verbs::acknowledgement_owed`): the
    // connection's other clients completed past an older request still awaited, which its acknowledgement may not
    // pass. The step is sent again, as new, once the worker next finds this client idle.
    if matches!(refusal, Refusal::AcknowledgementOwed { .. }) {
      tally.backpressure += 1;
      return;
    }
    // `LeaseUnconfirmed` refuses before the verb runs, a write too (§4.8 "Leases and reads"): the owner's lease
    // was not confirmed within its bound, and the protocol says to retry after it. Sent again as new, never under
    // its own id, whose recorded refusal would only answer the same.
    if matches!(refusal, Refusal::LeaseUnconfirmed { .. }) {
      tally.unconfirmed += 1;
      return;
    }
    // A request the daemon could not read is named in full: its reason says which part of the slot was wrong.
    if matches!(refusal, Refusal::BadRequest { .. }) {
      return self.give_up(tally, &format!("{kind:?}: {refusal:?}"));
    }
    let name = format!("{kind:?}: {refusal:?}");
    let short = name
      .split([' ', '{', '('])
      .take(2)
      .collect::<Vec<_>>()
      .join(" ");
    self.give_up(tally, &short);
  }

  /// Counts a think pause the client drew after a reply, so a cycle and a round open now leave it out: the pause is the
  /// client's own time, not the service's.
  fn thought(&mut self, pause_ns: u64) {
    if self.cycle_since.is_some() {
      self.cycle_thought_ns = self.cycle_thought_ns.saturating_add(pause_ns);
    }
    if self.round_since.is_some() {
      self.round_thought_ns = self.round_thought_ns.saturating_add(pause_ns);
    }
  }

  fn submitted(
    &mut self,
    version: Option<u64>,
    conflicts: Vec<MergeWindow>,
    at: (u64, u64),
    tally: &mut Tally,
  ) {
    let elapsed = at.1.saturating_sub(at.0);
    if let Some(start) = self.cycle_since.take() {
      let thought = std::mem::take(&mut self.cycle_thought_ns);
      record(
        tally,
        Kind::Cycle,
        at.1.saturating_sub(start).saturating_sub(thought),
      );
    }
    if version.is_some()
      && let Some(start) = self.round_since.take()
    {
      let thought = std::mem::take(&mut self.round_thought_ns);
      record(
        tally,
        Kind::Round,
        at.1.saturating_sub(start).saturating_sub(thought),
      );
    }
    let outcome = match version {
      Some(v) => Outcome::Accepted(v),
      None => Outcome::Conflict(
        conflicts
          .iter()
          .map(|w| (w.path.clone(), w.at, w.len))
          .collect(),
      ),
    };
    tally.submits.push(SubmitEvent {
      base: self.base,
      sent_ns: at.0,
      replied_ns: at.1,
      slots: self.slots().to_vec(),
      outcome,
      agent: self.id,
      round: self.round,
      classes: conflicts.iter().map(|w| w.class).collect(),
    });
    match version {
      Some(v) => {
        record(tally, Kind::SubmitAccepted, elapsed);
        self.base = v;
        self.own_len += u64::try_from(own_record(self.id, self.round).len()).unwrap();
        self.resolutions = 0;
        self.round += 1;
        self.step = if self.round < self.rounds.len() {
          Step::Edit(0)
        } else if keep_works() {
          Step::Done
        } else {
          Step::Destroy
        };
      }
      None => {
        record(tally, Kind::SubmitConflict, elapsed);
        self.resolutions += 1;
        if self.resolutions > MAX_RESOLUTIONS || conflicts.is_empty() {
          tally.starved += 1;
          self.step = Step::Destroy;
          return;
        }
        self.windows = conflicts;
        self.resolve();
      }
    }
  }

  /// Resolves every window (the design's corrective path), one request at a time like every other step: the green's
  /// head version, then each window's bytes at that version (a version never changes, so no submit landing meanwhile
  /// can tear the read), each written over its window in the work; then the rebase.
  fn resolve(&mut self) {
    self.step = Step::ResolveVersion;
  }

  fn rebased(&mut self, version: Option<u64>, conflicts: Vec<MergeWindow>, tally: &mut Tally) {
    match version {
      Some(v) => {
        self.base = v;
        self.step = Step::Reedit(0);
      }
      None => {
        // The head moved again between the read and the rebase: resolve the new windows, within the same bound as a
        // submit's conflicts, so a client under endless contention is counted starved rather than looping.
        tally.rebase_conflicts += 1;
        if tally.rebase_conflicts <= REBASE_CONFLICTS_NAMED {
          let named: Vec<String> = conflicts
            .iter()
            .map(|w| format!("{}@{}+{} class {}", w.path, w.at, w.len, w.class))
            .collect();
          println!(
            "rebase_conflict\tagent\t{}\tround\t{}\tbase\t{}\twindows\t{named:?}\tmine\t{:?}",
            self.id,
            self.round,
            self.base,
            self
              .slots()
              .iter()
              .map(|slot| format!("{}@{}", slot.path, slot.at))
              .collect::<Vec<_>>()
          );
        }
        self.resolutions += 1;
        if self.resolutions > MAX_RESOLUTIONS || conflicts.is_empty() {
          tally.starved += 1;
          self.step = Step::Destroy;
          return;
        }
        self.windows = conflicts;
        self.resolve();
      }
    }
  }
}

fn record(tally: &mut Tally, kind: Kind, elapsed: u64) {
  tally.latencies.entry(kind).or_default().push(elapsed);
}

/// How many reconnect budgets the harness keeps asking again past a retryable refusal before it gives the request up:
/// one, unless `--reply-patience <n>` asks more. A request with no reply yet is waited for while its daemon lives, the
/// client's rule (A-232), so this bounds no reply wait.
fn reply_patience() -> u64 {
  arg("--reply-patience").map_or(1, |n| n.parse().expect("--reply-patience n"))
}

/// Whether each client keeps its work after its last round (`--keep-works`) instead of destroying it, so the daemon's
/// memory with every work in place can be read after the level (each client a volume it keeps, as the goal's clients
/// do).
fn keep_works() -> bool {
  std::env::args().any(|a| a == "--keep-works")
}

/// Format: nanoseconds a microsecond.
const NS_PER_US: u64 = 1_000;

/// Whether `refusal` is one the protocol marks retryable under the request's own id: the verb was not run, or ran and
/// its completion record answers the same id once it is durable (§4.8, §4.9). Not `ContentUnavailable`: the verb
/// recorded it as its completion (a fenced green, unreadable content), so a resend under the id only meets it again;
/// 12596890 resent it until its budget ran out.
fn retryable_under_its_id(refusal: &Refusal) -> bool {
  matches!(
    refusal,
    Refusal::Overloaded { .. } | Refusal::OwnerUnreachable { .. } | Refusal::Unpublished { .. }
  )
}

/// Connection `c`'s reconnect budget: how long a retryable refusal is retried before the outcome is left unknown.
fn conn_reconnect_ns(conns: &[Connection], c: usize) -> u64 {
  conns
    .get(c)
    .map_or(0, |conn| conn.client.deadlines().reconnect_ns)
}

/// The mean of the pause each client takes after a reply before its next request (`--think-us <µs>`, default none: a
/// closed loop with no think time, every client always waiting on a request). Each pause is drawn from the exponential
/// distribution of this mean ([`Rng::exponential_ns`]), and so is a pause before each client's first request. An agent
/// between its requests thinks, reads its tree or runs a build: the offered load the latency bound is stated for. A
/// latency never includes the pause.
fn think_mean_ns() -> u64 {
  arg("--think-us").map_or(0, |us| {
    us.parse::<u64>()
      .expect("--think-us µs")
      .saturating_mul(NS_PER_US)
  })
}

/// How many logical clients share one connection (`--per-connection <k>`, default one): each keeps its own work
/// volume, rounds, merges and conflicts and one request in flight, so a connection carries up to `k` at once, within
/// the client's own in-flight bound (its ring's slots), which refuses a `k` past it at the first send.
fn per_connection() -> usize {
  arg("--per-connection").map_or(1, |k| {
    k.parse::<usize>().expect("--per-connection k").max(1)
  })
}

/// Shape: the rebase conflicts a worker names, each with its windows and the client's own slots, so a run shows what a
/// rebase reported without printing every one.
const REBASE_CONFLICTS_NAMED: u64 = 5;

/// Shape: the unfinished clients a progress report names, each with its step and the request it waits on.
const PROGRESS_NAMED: usize = 4;
/// Shape: how often a worker prints its progress, so a long or stuck level shows where it is.
const PROGRESS_EVERY: Duration = Duration::from_secs(5);
/// Shape: how long a worker goes with no reply before it dumps where every connection's replies stand
/// ([`Worker::stall_dump`]), once per stall. Twelve progress reports: a stall, not a slow commit.
const STALL_DUMP_AFTER: Duration = Duration::from_secs(60);
/// Shape: the stuck requests a stall dump names per connection.
const STALL_NAMED: usize = 4;
/// Shape: how many times per reply deadline a worker looks for overdue requests: often enough that a lost reply is
/// named within a few percent of the deadline past it, rarely enough that the scan costs nothing next to the sweeps.
const OVERDUE_CHECKS_PER_DEADLINE: u64 = 16;

/// One connection a worker holds (one ring, one client id, one completion window at the daemon) and the requests in
/// flight over it, each its logical client's own. The connection, not the logical client, reconnects after the daemon
/// is found gone; each client's request is then resent under its own id, so the daemon answers it from its completion
/// record if it served it.
struct Connection {
  client: Client,
  /// The worker-local agent awaiting each request word in flight here.
  inflight: BTreeMap<u64, usize>,
  /// Since when this connection has been reconnecting after its daemon was found gone.
  reconnecting: Option<u64>,
  /// Words in flight when the channel was lost, still to be resent once reconnected, and since when.
  resend: Vec<u64>,
  resend_since: u64,
  /// When this connection's last drain began, the clock read before its ring was read: a reply that drain did not take
  /// arrived after it (the module's "floor").
  read_began_ns: u64,
}

impl Connection {
  fn new(client: Client, now: u64) -> Connection {
    Connection {
      client,
      inflight: BTreeMap::new(),
      reconnecting: None,
      resend: Vec::new(),
      resend_since: 0,
      read_began_ns: now,
    }
  }

  fn lost(&mut self, now: u64) {
    if self.reconnecting.is_none() {
      self.reconnecting = Some(now);
      self.resend = self.inflight.keys().copied().collect();
      self.resend_since = now;
    }
  }
}

/// One worker's state: its connections, its logical clients, the clients ready to send, and what it measured.
struct Worker {
  conns: Vec<Connection>,
  agents: Vec<Agent>,
  /// Clients with nothing in flight and a step to send, by when each may send: its last reply plus the think time
  /// (`--think-us`), earliest first.
  ready: std::collections::BinaryHeap<std::cmp::Reverse<(u64, usize)>>,
  /// The mean pause a client takes after each reply before its next request (`--think-us`), nanoseconds, and the
  /// worker's own draws of it.
  think: Think,
  tally: Tally,
  clock: Clock,
}

/// The clients' pauses between requests: each drawn from the exponential distribution of the stated mean, and each
/// counted in the tally's thinking time.
struct Think {
  mean_ns: u64,
  draws: Rng,
}

impl Think {
  /// The pause before a client's next request, counted.
  fn draw(&mut self, tally: &mut Tally) -> u64 {
    let pause = self.draws.exponential_ns(self.mean_ns);
    tally.thinking_ns = tally.thinking_ns.saturating_add(pause);
    pause
  }
}

/// Shape: the seed of a worker's think draws, apart from the plan's (`SEED`), so the plan is the same whatever the
/// pacing: worker `w` at level `n` draws from `THINK_SEED ^ n ^ (w << 32)`.
const THINK_SEED: u64 = 0x7a1c_0ff5_e7ba_c0de;

/// The pause after a refusal for now (or a refusal retryable under its id): the client's own doubling pause, counted in
/// the tally's paused time.
fn refusal_pause(client: &Client, last_pause_ns: u64, tally: &mut Tally) -> u64 {
  let pause = client.retry_pause_ns(last_pause_ns);
  tally.paused_ns = tally.paused_ns.saturating_add(pause);
  pause
}

impl Worker {
  /// Sends agent `a`'s current step on its connection; a full ring or the connection's own in-flight bound puts it
  /// back in line, a lost channel starts the connection's reconnect.
  fn send(&mut self, a: usize) -> bool {
    let agent = &mut self.agents[a];
    let Some((body, kind)) = agent.request() else {
      return true;
    };
    let conn = &mut self.conns[agent.conn];
    if conn.reconnecting.is_some() {
      return false;
    }
    // Read before the request is handed to its ring, so the send's stamp is never later than its departure, and once
    // the send returns, so the departure is bracketed (the module's "Each instant").
    let sent = self.clock.now();
    if let Some((word, first_sent_ns, retried_kind)) = agent.retry {
      let resent = conn.client.begin_again(word, &body);
      let handed = self.clock.now();
      return match resent {
        Ok(true) => {
          agent.retry = None;
          agent.inflight = Some((word, first_sent_ns, retried_kind));
          agent.attempt_sent_ns = sent;
          agent.handed_ns = handed;
          conn.inflight.insert(word, a);
          self.tally.retried += 1;
          true
        }
        Ok(false) | Err(ClientError::TooManyOutstanding { .. } | ClientError::Rebinding) => false,
        Err(_) if conn.client.daemon_gone() => {
          conn.lost(handed);
          false
        }
        Err(e) => {
          agent.inflight = agent.retry.take();
          agent.give_up(&mut self.tally, &format!("retry {kind:?}: {e}"));
          true
        }
      };
    }
    let begun = conn.client.begin(&body);
    let handed = self.clock.now();
    match begun {
      Ok(id) => {
        agent.op_since.get_or_insert(sent);
        agent.inflight = Some((id.word(), sent, kind));
        agent.attempt_sent_ns = sent;
        agent.handed_ns = handed;
        conn.inflight.insert(id.word(), a);
        true
      }
      Err(
        ClientError::Ipc(slates_ipc::IpcError::RingFull)
        | ClientError::RingFull
        | ClientError::TooManyOutstanding { .. }
        | ClientError::Rebinding,
      ) => false,
      Err(_) if conn.client.daemon_gone() => {
        conn.lost(handed);
        false
      }
      Err(e) => {
        agent.give_up(&mut self.tally, &format!("send {kind:?}: {e}"));
        true
      }
    }
  }

  /// Sends for every ready client, in order, until one cannot go (each connection's ring and bound are its own, so
  /// a client that cannot go is kept and the rest are tried). Each due client's wait past the instant it was due (or
  /// was last kept) is counted held: the harness's sweep or its connection's bound kept it back.
  fn send_ready(&mut self) {
    let now = self.clock.now();
    let mut kept = Vec::new();
    while let Some(std::cmp::Reverse((due, a))) = self.ready.peek().copied() {
      if due > now {
        break;
      }
      self.ready.pop();
      if self.agents[a].step == Step::Done || self.agents[a].inflight.is_some() {
        continue;
      }
      self.tally.held_ns = self.tally.held_ns.saturating_add(now.saturating_sub(due));
      if !self.send(a) {
        kept.push(std::cmp::Reverse((now, a)));
      }
    }
    self.ready.extend(kept);
  }

  /// Drains connection `c`'s ring and moves each reply's client on. Each reply is stamped after the ring was read, so
  /// it arrived before its stamp; and it was not there when the connection's drain before began, so it arrived after
  /// that and after its request was handed over: its floor (the module's "floor").
  fn drain(&mut self, c: usize) {
    let began = self.clock.now();
    let conn = &mut self.conns[c];
    let taken = conn.client.take_ready();
    let now = self.clock.now();
    let words = match taken {
      Ok(words) => words,
      Err(_) if conn.client.daemon_gone() => return conn.lost(now),
      Err(e) => return self.fail_connection(c, &format!("take_ready: {e}")),
    };
    let not_before = std::mem::replace(&mut conn.read_began_ns, began);
    let mut answered = false;
    for word in words {
      let conn = &mut self.conns[c];
      let Some(a) = conn.inflight.remove(&word) else {
        continue;
      };
      let reply = match conn.client.poll_reply_word(word) {
        Ok(Some(reply)) => reply,
        Ok(None) => continue,
        Err(ClientError::Refused(refusal)) => ReplyBody::Refused { refusal },
        Err(e) => {
          self.agents[a].give_up(&mut self.tally, &format!("poll: {e}"));
          continue;
        }
      };
      answered = true;
      self.tally.answered += 1;
      let handed = self.agents[a].handed_ns;
      self.tally.max_drain_gap_ns = self
        .tally
        .max_drain_gap_ns
        .max(now.saturating_sub(not_before.max(handed)));
      self.take_answer(a, word, reply, (now, not_before.saturating_sub(handed)));
    }
    // The daemon keeps every reply's completion record until the client acknowledges it, and refuses a client that
    // owes too many (`AcknowledgementOwed`): an async caller acknowledges as it goes, unawaited, never past a request
    // it still awaits.
    if answered && let Err(e) = self.conns[c].client.begin_ack_if_due() {
      self.fail_connection(c, &format!("acknowledge: {e}"));
    }
  }

  /// Moves client `a` on with `reply`, the answer to its request `word`, taken by `now` and absent `floor` past its
  /// request's handing over: a retryable refusal is resent under its id (or given up past the patience); any other
  /// answer advances the client and, once it is final, records the logical operation from its first send, and the
  /// attempt's floor under the kind its latency is filed under.
  fn take_answer(&mut self, a: usize, word: u64, reply: ReplyBody, (now, floor): (u64, u64)) {
    let agent = &mut self.agents[a];
    let Some((_, sent_ns, kind)) = agent.inflight.take() else {
      return;
    };
    self.tally.in_flight_ns = self
      .tally
      .in_flight_ns
      .saturating_add(now.saturating_sub(agent.attempt_sent_ns));
    if let ReplyBody::Refused { refusal } = &reply
      && retryable_under_its_id(refusal)
    {
      // Bounded: past the client's reconnect budget times the patience, the outcome is left unknown for the oracle.
      let waited = now.saturating_sub(sent_ns);
      let budget = conn_reconnect_ns(&self.conns, agent.conn).saturating_mul(reply_patience());
      if waited > budget {
        // Given up: the client need no longer keep its acknowledgement below the id.
        self.conns[agent.conn].client.release_retryable(word);
        agent.inflight = Some((word, sent_ns, kind));
        agent.give_up(
          &mut self.tally,
          &format!("{kind:?}: still refused retryable after {waited} ns"),
        );
        return;
      }
      agent.retry = Some((word, sent_ns, kind));
      agent.pause_ns = refusal_pause(
        &self.conns[agent.conn].client,
        agent.pause_ns,
        &mut self.tally,
      );
      self
        .ready
        .push(std::cmp::Reverse((now.saturating_add(agent.pause_ns), a)));
      return;
    }
    let for_now = matches!(
      &reply,
      ReplyBody::Refused {
        refusal: Refusal::LeaseUnconfirmed { .. } | Refusal::AcknowledgementOwed { .. }
      }
    );
    let refused = matches!(&reply, ReplyBody::Refused { .. });
    // A submit's operation is filed under its outcome, as its attempt's latency is (`Agent::submitted`).
    let outcome = match &reply {
      ReplyBody::Submitted { version: None, .. } => Kind::SubmitConflict,
      _ => kind,
    };
    let first = if for_now { None } else { agent.op_since.take() };
    agent.advance(
      reply,
      (sent_ns, now, kind),
      first.unwrap_or(sent_ns),
      &mut self.tally,
    );
    if !refused {
      self.tally.floors.entry(outcome).or_default().push(floor);
      if let Some(first) = first {
        self
          .tally
          .operation_took(outcome, now.saturating_sub(first));
      }
    }
    if agent.step != Step::Done {
      let wait = if for_now {
        agent.pause_ns = refusal_pause(
          &self.conns[agent.conn].client,
          agent.pause_ns,
          &mut self.tally,
        );
        agent.pause_ns
      } else {
        agent.pause_ns = 0;
        let pause = self.think.draw(&mut self.tally);
        agent.thought(pause);
        pause
      };
      self
        .ready
        .push(std::cmp::Reverse((now.saturating_add(wait), a)));
    }
  }

  /// Every client of connection `c` gives up with `why`.
  fn fail_connection(&mut self, c: usize, why: &str) {
    for agent in self.agents.iter_mut().filter(|agent| agent.conn == c) {
      if agent.step != Step::Done {
        agent.give_up(&mut self.tally, why);
      }
    }
    self.conns[c].inflight.clear();
  }

  /// One step of riding out a daemon restart on connection `c`, never blocking the sweep: a reconnect step under the
  /// connection's id, then each request in flight resent under its own id. Bounded by the client's own derived
  /// reconnect budget.
  fn recover(&mut self, c: usize) {
    let now = self.clock.now();
    let conn = &mut self.conns[c];
    let since = conn.reconnecting.unwrap_or(conn.resend_since);
    if now.saturating_sub(since) > conn.client.deadlines().reconnect_ns {
      return self.fail_connection(
        c,
        "the daemon did not come back within the reconnect budget",
      );
    }
    if conn.reconnecting.is_some() {
      match conn.client.try_reconnect() {
        Ok(true) => {
          self.tally.reconnects += 1;
          conn.reconnecting = None;
        }
        Ok(false) => return,
        Err(e) => return self.fail_connection(c, &format!("reconnect: {e}")),
      }
    }
    let mut left = Vec::new();
    for word in std::mem::take(&mut conn.resend) {
      if !conn.inflight.contains_key(&word) {
        continue;
      }
      match conn.client.resend_awaited(word) {
        Ok(true) => {
          self.tally.resent += 1;
          self.tally.max_outage_ns = self
            .tally
            .max_outage_ns
            .max(now.saturating_sub(conn.resend_since));
        }
        Ok(false) | Err(ClientError::Rebinding) => left.push(word),
        Err(_) if conn.client.daemon_gone() => {
          conn.resend = left;
          conn.reconnecting = None;
          return conn.lost(now);
        }
        Err(e) => {
          let why = format!("resend: {e}");
          return self.fail_connection(c, &why);
        }
      }
    }
    conn.resend = left;
  }

  /// No reply yet to some request: past the client's own derived reply deadline, a gone daemon starts the restart
  /// path, and a live one's request is waited for, the client's own rule (A-232). A worker whose replies stop coming
  /// is named by its stall dump ([`Self::stall_dump`]). Until 2026-11-13 a request the daemon would not defer
  /// (`Client::defers`, now gone) was counted lost past `reply_patience` deadlines while its daemon lived.
  fn overdue(&mut self) {
    let now = self.clock.now();
    for c in 0..self.conns.len() {
      let conn = &mut self.conns[c];
      if conn.reconnecting.is_some() || !conn.resend.is_empty() {
        continue;
      }
      let deadline = conn.client.deadlines().reply_ns;
      let overdue = conn.inflight.values().any(|a| {
        self.agents[*a]
          .inflight
          .is_some_and(|(_, sent_ns, _)| now.saturating_sub(sent_ns) > deadline)
      });
      if overdue && conn.client.daemon_gone() {
        conn.lost(now);
      }
    }
  }

  /// Where every connection with a request in flight stands, printed once per stall, so a stalled level names its hop
  /// without a debugger (fixtures' f = 1 stalls of 2026-11-07, where ptrace was not allowed): `ring_unread` or `held`
  /// above zero is a reply that came and was not collected (the harness's side); both zero with requests awaited is a
  /// reply that never came (the daemon's side, placed by its `fleet.forward.*` hop counters). `now` is the worker's
  /// clock.
  fn stall_dump(&self, worker: usize, quiet: Duration, now: u64) {
    for (c, conn) in self.conns.iter().enumerate() {
      if conn.inflight.is_empty() {
        continue;
      }
      let (unread, held, awaited) = conn.client.reply_state();
      let named: Vec<String> = conn
        .inflight
        .iter()
        .take(STALL_NAMED)
        .map(|(word, a)| {
          let agent = &self.agents[*a];
          let age = agent
            .inflight
            .map_or(0, |(_, sent, _)| now.saturating_sub(sent) / 1_000_000);
          let kind = agent.inflight.map(|(_, _, kind)| kind);
          format!("{word:#x}:{kind:?}:{age}ms")
        })
        .collect();
      println!(
        "stall\t{worker}\tquiet_ms\t{}\tconn\t{c}\tinflight\t{}\toutstanding\t{}\tawaited\t{awaited}\tring_unread\t{unread:?}\theld\t{held}\tunawaited_dropped\t{}\treconnecting\t{}\tresend\t{}\tdaemon_gone\t{}\tstuck\t{}",
        quiet.as_millis(),
        conn.inflight.len(),
        conn.client.outstanding().len(),
        conn.client.unawaited_dropped(),
        conn.reconnecting.is_some(),
        conn.resend.len(),
        conn.client.daemon_gone(),
        named.join(",")
      );
    }
  }

  /// Where the worker's clients stand at `now`, its clock.
  fn progress(&self, worker: usize, now: u64) {
    let done = self
      .agents
      .iter()
      .filter(|agent| agent.step == Step::Done)
      .count();
    for agent in self
      .agents
      .iter()
      .filter(|agent| agent.step != Step::Done)
      .take(PROGRESS_NAMED)
    {
      let age = agent
        .inflight
        .map(|(_, sent, kind)| (kind, now.saturating_sub(sent)));
      println!(
        "pending\t{worker}\tagent\t{}\tstep\t{:?}\tround\t{}\tinflight\t{age:?}\toutstanding\t{}",
        agent.id,
        agent.step,
        agent.round,
        self.conns[agent.conn].client.outstanding().len()
      );
    }
    println!(
      "progress\t{worker}\t{}\tdone\t{done}\tof\t{}\tsubmits\t{}",
      Duration::from_nanos(now).as_secs(),
      self.agents.len(),
      self.tally.submits.len()
    );
  }
}

/// One worker: its connections and their logical clients, each with one request in flight, swept until every client
/// is done. A sweep drains every connection's ring first (stamping each reply's arrival, so a latency here is the
/// daemon's and the ring's, not the time the harness took to reach the client), moves each answered client on and
/// sends its next request at once, then sends for clients still waiting for room. Every `PROGRESS_EVERY` it prints how
/// many of its clients are done and the submits it has seen. Each client first waits one pause drawn from `think`, as
/// it does after every reply, so the clients start apart as independent agents do.
fn drive(
  conns: Vec<Connection>,
  agents: Vec<Agent>,
  clock: Clock,
  worker: usize,
  mut think: Think,
) -> Tally {
  let mut tally = Tally::default();
  let ready = (0..agents.len())
    .map(|a| std::cmp::Reverse((think.draw(&mut tally), a)))
    .collect();
  let check_every = conns.first().map_or(1, |conn| {
    (conn.client.deadlines().reply_ns / OVERDUE_CHECKS_PER_DEADLINE).max(1)
  });
  let mut state = Worker {
    conns,
    agents,
    ready,
    think,
    tally,
    clock,
  };
  let mut reported = Instant::now();
  let mut checked = state.clock.now();
  // The last time any reply moved a client on, and whether this stall was dumped.
  let mut moved = Instant::now();
  let mut answers = 0u64;
  let mut dumped = false;
  while state.agents.iter().any(|agent| agent.step != Step::Done) {
    let sweep = Instant::now();
    for c in 0..state.conns.len() {
      if state.conns[c].reconnecting.is_some() || !state.conns[c].resend.is_empty() {
        state.recover(c);
      } else if !state.conns[c].inflight.is_empty() {
        state.drain(c);
      }
    }
    state.send_ready();
    state.tally.max_sweep_ns = state
      .tally
      .max_sweep_ns
      .max(u64::try_from(sweep.elapsed().as_nanos()).unwrap_or(u64::MAX));
    let now = state.clock.now();
    if now.saturating_sub(checked) >= check_every {
      checked = now;
      state.overdue();
    }
    if reported.elapsed() >= PROGRESS_EVERY {
      reported = Instant::now();
      state.progress(worker, now);
    }
    let answered = state.tally.answered;
    if answered != answers {
      answers = answered;
      moved = Instant::now();
      dumped = false;
    } else if !dumped && moved.elapsed() >= STALL_DUMP_AFTER {
      dumped = true;
      state.stall_dump(worker, moved.elapsed(), now);
    }
  }
  state.tally
}

/// Shape: the overlay's bound over its tree's bytes. Twice the tree leaves room for the pin's copy and the edits a
/// green never writes back (the overlay is only the green's base); a dynamic maximum past the shard instead makes the
/// overlay's inode allowance the whole version slab, which a restart cannot recover (bug record of 2026-11-05), and
/// with the green's origin held as a reference to the overlay's snapshot (A-217) the green would be fenced with it.
const OVERLAY_BOUND_FACTOR: u64 = 2;

/// The bytes of `tree` as `du -sb` reports them (a subprocess, as the daemon tests spawn `mkdir`; the harness reads no
/// disk itself). It had been listed through a probe overlay destroyed before the real one was pinned, and 5 of 7 runs
/// over the kernel's `fs/`, `Documentation/` and `arch/` then grew past 8 to 25 GB inside the pin (GAPS, open).
fn tree_bytes(tree: &str) -> u64 {
  // GNU `du -sb` is apparent bytes; BSD `du` (macOS) has no `-b`, and `-A -k` is the apparent size in KiB.
  let (flags, unit) = if cfg!(target_os = "macos") {
    ("-sAk", 1024)
  } else {
    ("-sb", 1)
  };
  let du = std::process::Command::new("du")
    .args([flags, tree])
    .output()
    .expect("du <tree>");
  let text = String::from_utf8_lossy(&du.stdout);
  text
    .split_whitespace()
    .next()
    .and_then(|bytes| bytes.parse::<u64>().ok())
    .expect("du prints the size first")
    .saturating_mul(unit)
    .max(1)
}

/// The kernel tree as a green: an overlay over the host directory, pinned whole, snapshotted, and the green
/// created over that snapshot (the A-9 complete immutable base). Each step's time is printed.
fn seed_green(client: &mut Client, tree: &str, tag: &str) -> VolumeId {
  let step = Instant::now();
  let bound = tree_bytes(tree).saturating_mul(OVERLAY_BOUND_FACTOR);
  println!(
    "setup\tprobe_tree_ns\t{}\tbound\t{bound}",
    step.elapsed().as_nanos()
  );
  let step = Instant::now();
  let overlay = client
    .create(&CreateSpec {
      name: format!("{tag}-tree"),
      size: SizeClass::Bounded { limit: bound },
      names: NamePolicy::Exact,
      require_locked: false,
      base: Some(tree.to_owned()),
    })
    .unwrap();
  println!("setup\tcreate_overlay_ns\t{}", step.elapsed().as_nanos());
  let step = Instant::now();
  let pinned = client.pin(overlay, None).unwrap();
  println!(
    "setup\tpin_whole_ns\t{}\tentries\t{pinned}",
    step.elapsed().as_nanos()
  );
  let step = Instant::now();
  let snapshot = client.snapshot(overlay).unwrap();
  println!("setup\tsnapshot_ns\t{}", step.elapsed().as_nanos());
  let step = Instant::now();
  let green = client
    .create_green_over(
      &format!("{tag}-green"),
      false,
      GreenBase {
        volume: overlay,
        snapshot,
      },
    )
    .unwrap();
  println!("setup\tcreate_green_ns\t{}", step.elapsed().as_nanos());
  green
}

fn percentile(sorted: &[u64], permille: u64) -> u64 {
  let last = sorted.len().saturating_sub(1);
  let index = usize::try_from(u64::try_from(last).unwrap() * permille / PERMILLE).unwrap();
  sorted.get(index.min(last)).copied().unwrap_or(0)
}

/// Prints one row a kind of `samples`, sorted in place: `<row>\t<n>\t<kind>\t<count>\t<p50>\t<p99>\t<p999>\t<max>`.
fn quantile_rows(row: &str, n: usize, samples: &mut BTreeMap<Kind, Vec<u64>>) {
  for (kind, samples) in samples {
    samples.sort_unstable();
    let cuts: Vec<String> = PERCENTILES
      .iter()
      .map(|p| percentile(samples, *p).to_string())
      .collect();
    println!(
      "{row}\t{n}\t{kind:?}\t{}\t{}\t{}",
      samples.len(),
      cuts.join("\t"),
      samples.last().copied().unwrap_or(0)
    );
  }
}

fn report(n: usize, tally: &mut Tally) {
  quantile_rows("latency", n, &mut tally.latencies);
  quantile_rows("operation", n, &mut tally.operations);
  quantile_rows("floor", n, &mut tally.floors);
  for (why, count) in &tally.refusals {
    println!("refusal\t{n}\t{count}\t{why}");
  }
  println!(
    "harness\t{n}\tstarved\t{}\trebase_conflicts\t{}\tmax_sweep_ns\t{}\tmax_drain_gap_ns\t{}\treconnects\t{}\tresent\t{}\tmax_outage_ns\t{}\tbackpressure\t{}\tretried\t{}\tunconfirmed\t{}",
    tally.starved,
    tally.rebase_conflicts,
    tally.max_sweep_ns,
    tally.max_drain_gap_ns,
    tally.reconnects,
    tally.resent,
    tally.max_outage_ns,
    tally.backpressure,
    tally.retried,
    tally.unconfirmed
  );
}

/// Format: nanoseconds a second.
const NS_PER_S: u128 = 1_000_000_000;
/// Format: tenths in a unit, the `offered` row's one decimal place.
const TENTHS: u128 = 10;

/// `part` over `whole` to one decimal place: a time-averaged number of clients from their summed time and the wall.
fn tenths(part: u64, whole: u64) -> String {
  let scaled = (u128::from(part) * TENTHS)
    .checked_div(u128::from(whole))
    .unwrap_or(0);
  format!("{}.{}", scaled / TENTHS, scaled % TENTHS)
}

/// `count` events over `wall_ns` as a rate a second.
fn per_second(count: u64, wall_ns: u64) -> u128 {
  u128::from(count)
    .saturating_mul(NS_PER_S)
    .checked_div(u128::from(wall_ns))
    .unwrap_or(0)
}

/// The level's offered load and where its clients' time went: the think's stated mean; the replies taken, the verdicts
/// (submits answered, accepted or in conflict) and the rebases a second; and the time-averaged number of clients in
/// flight, thinking, pausing after a refusal for now, and held, with their sum, the clients not yet done. In the closed
/// loop that sum is the throughput times each operation's cycle, request and pause together (Little's law), so `held`
/// is the share of the clients the harness or a connection's bound kept back: their wait is in no latency row.
fn offered_row(n: usize, think_mean_ns: u64, wall_ns: u64, tally: &Tally) -> String {
  let alive = tally
    .in_flight_ns
    .saturating_add(tally.thinking_ns)
    .saturating_add(tally.paused_ns)
    .saturating_add(tally.held_ns);
  format!(
    "offered\t{n}\tthink_mean_ns\t{think_mean_ns}\treplies_per_s\t{}\tverdicts_per_s\t{}\trebases_per_s\t{}\talive\t{}\tin_flight\t{}\tthinking\t{}\tpaused\t{}\theld\t{}",
    per_second(tally.answered, wall_ns),
    per_second(verdicts(tally), wall_ns),
    per_second(attempts(tally, Kind::Rebase), wall_ns),
    tenths(alive, wall_ns),
    tenths(tally.in_flight_ns, wall_ns),
    tenths(tally.thinking_ns, wall_ns),
    tenths(tally.paused_ns, wall_ns),
    tenths(tally.held_ns, wall_ns),
  )
}

/// The attempts of `kind` the level timed.
fn attempts(tally: &Tally, kind: Kind) -> u64 {
  tally.latencies.get(&kind).map_or(0, |samples| {
    u64::try_from(samples.len()).unwrap_or(u64::MAX)
  })
}

/// The verdicts the level took: its submits answered, accepted or in conflict.
fn verdicts(tally: &Tally) -> u64 {
  attempts(tally, Kind::SubmitAccepted).saturating_add(attempts(tally, Kind::SubmitConflict))
}

/// The level judged against the bound (the module's "The bound"; Ada's decision of 2026-11-18): the bound and the
/// verdicts a second it is stated at; the level's own verdicts a second; the worst p99 of every judged kind
/// ([`Kind::judged`]: every request and the cycle, not the round), each attempt's (`latency`) and each operation's
/// (`operation`), with its row and kind; the clients held, time-averaged as in the `offered` row, whose wait no latency
/// row holds; the clients given up or starved; the oracle's failures; and `met\tyes` when the level met the bound,
/// else `met\tno`. A level that timed nothing meets nothing.
fn bound_row(n: usize, wall_ns: u64, tally: &Tally, oracle_failures: u64) -> String {
  let verdicts_per_s = per_second(verdicts(tally), wall_ns);
  let worst = [
    ("latency", &tally.latencies),
    ("operation", &tally.operations),
  ]
  .into_iter()
  .flat_map(|(row, kinds)| {
    kinds
      .iter()
      .filter(|(kind, _)| kind.judged())
      .map(move |(kind, samples)| {
        let mut sorted = samples.clone();
        sorted.sort_unstable();
        (percentile(&sorted, BOUND_PERMILLE), row, *kind)
      })
  })
  .max();
  let gave_up = tally
    .refusals
    .values()
    .fold(tally.starved, |sum, count| sum.saturating_add(*count));
  let met = worst.is_some_and(|(p99, _, _)| p99 < BOUND_P99_NS)
    && verdicts_per_s >= BOUND_VERDICTS_PER_S
    && gave_up == 0
    && oracle_failures == 0;
  let (worst_p99, worst_row, worst_kind) = worst
    .map_or((0, "none", "none".to_owned()), |(p99, row, kind)| {
      (p99, row, format!("{kind:?}"))
    });
  format!(
    "bound\t{n}\tp99_bound_ns\t{BOUND_P99_NS}\tverdicts_floor_per_s\t{BOUND_VERDICTS_PER_S}\tverdicts_per_s\t{verdicts_per_s}\tworst\t{worst_row}\t{worst_kind}\tworst_p99_ns\t{worst_p99}\theld\t{}\tgave_up\t{gave_up}\toracle_failures\t{oracle_failures}\tmet\t{}",
    tenths(tally.held_ns, wall_ns),
    if met { "yes" } else { "no" },
  )
}

/// Judges the level (the rule is `support/load_oracle.rs`'s): every submit over `base`, the touched files' bytes before
/// the level, then the green's head of each file as `read_head` reads it, one file at a time. Prints the first
/// failures, the summary row and the oracle's own time (`oracle_ns`: the submits' judgment, then the heads' with
/// their reads), so a level's wall is told from the harness's own work after it; returns the failures.
fn oracle(
  n: usize,
  submits: &[SubmitEvent],
  base: BTreeMap<String, Vec<u8>>,
  read_head: impl FnMut(&str) -> Vec<u8>,
) -> u64 {
  let judging = Instant::now();
  let (judged, writes) = load_oracle::judge_submits(submits, &base);
  let judge_ns = judging.elapsed().as_nanos();
  let reading = Instant::now();
  let mut failures = judged.failures;
  let files_checked = load_oracle::judge_heads(base, &writes, read_head, &mut failures);
  let heads_ns = reading.elapsed().as_nanos();
  for what in &failures.named {
    println!("oracle_failure\t{n}\t{what}");
  }
  println!(
    "oracle\t{n}\taccepted\t{}\tconflicts\t{}\tunknown\t{}\tfiles_checked\t{files_checked}\toracle_failures\t{}",
    judged.accepted, judged.conflicts, judged.unknown, failures.count
  );
  println!("oracle_ns\t{n}\tsubmits\t{judge_ns}\theads\t{heads_ns}");
  failures.count
}

/// Settles the submits whose outcome their clients never learned, from the green itself: every version of this level
/// the green holds that no known acceptance names is matched to the unknown submit whose slots hold that client's own
/// bytes at it and did not all hold them at the version before (each client's bytes are unique to it and its round).
/// A matched submit becomes accepted at that version; an unmatched one was never taken. Returns the versions no submit
/// explains, each a failure: a version nobody of this level submitted. Version numbers here are this level's (rebased
/// to `head_before`), as the oracle's are.
fn resolve_unknown(
  reader: &mut Client,
  green: VolumeId,
  head_before: u64,
  submits: &mut [SubmitEvent],
) -> Vec<u64> {
  let head = versions_patiently(reader, green).saturating_sub(head_before);
  let known: BTreeSet<u64> = submits
    .iter()
    .filter_map(|e| match e.outcome {
      Outcome::Accepted(v) => Some(v),
      _ => None,
    })
    .collect();
  let mut unexplained = Vec::new();
  for version in (1..=head).filter(|v| !known.contains(v)) {
    let matched = submits.iter().position(|event| {
      matches!(event.outcome, Outcome::Unknown)
        && holds_its_bytes(reader, green, event, head_before + version)
        && !holds_its_bytes(reader, green, event, head_before + version - 1)
    });
    match matched {
      Some(index) => submits[index].outcome = Outcome::Accepted(version),
      None => unexplained.push(version),
    }
  }
  unexplained
}

/// Whether every slot of `event` holds the event's own bytes at the green's `version`.
fn holds_its_bytes(
  reader: &mut Client,
  green: VolumeId,
  event: &SubmitEvent,
  version: u64,
) -> bool {
  event.slots.iter().all(|slot| {
    let reply = reader.call(&RequestBody::ReadRange {
      volume: green,
      path: slot.path.clone(),
      at: ReadAt::Version { version },
      offset: slot.at,
      max: SLOT_BYTES,
    });
    matches!(reply, Ok(ReplyBody::ReadPage { bytes, .. }) if bytes == slot.bytes)
  })
}

/// Reads every file the plan names at the green's `at` view.
fn read_files(
  client: &mut Client,
  green: VolumeId,
  paths: &BTreeSet<String>,
  at: ReadAt,
) -> BTreeMap<String, Vec<u8>> {
  paths
    .iter()
    .map(|path| (path.clone(), read_patiently(client, green, path, at)))
    .collect()
}

/// A file read that a busy daemon may refuse for now: tried again (`patiently`). A late reply is waited for while the
/// daemon lives (A-232).
fn read_patiently(client: &mut Client, green: VolumeId, path: &str, at: ReadAt) -> Vec<u8> {
  patiently(client, &format!("read {path}"), |client| {
    client.read(green, path, at)
  })
}

/// The green's version count, asked patiently.
fn versions_patiently(client: &mut Client, green: VolumeId) -> u64 {
  patiently(client, "versions", |client| client.versions(green))
}

/// An oracle or setup call tried again while its answer is refused for now: its daemon gone for longer than one
/// reconnect budget, `LeaseUnconfirmed` (an owner confirming its lease after a node's
/// kill: fixtures' 1,000-client replica-kill rung aborted the harness on one, 2026-11-07), `Overloaded` or
/// `OwnerUnreachable`. Paced by the client's own retry pause and bounded by `reply_patience` reconnect budgets, past
/// which the harness stops with the last answer named.
fn patiently<T>(
  client: &mut Client,
  what: &str,
  mut call: impl FnMut(&mut Client) -> Result<T, ClientError>,
) -> T {
  let started = Instant::now();
  let bound = Duration::from_nanos(
    client
      .deadlines()
      .reconnect_ns
      .saturating_mul(reply_patience()),
  );
  let mut pause_ns = 0;
  loop {
    match call(client) {
      Ok(answer) => return answer,
      Err(
        // A kill rung's restarted node may take longer than one reconnect budget to come back; each call reconnects
        // again (fixtures' 1,000-client primary kill: `versions` met `DaemonGone` 7.52 s in, 2026-11-07).
        ClientError::DaemonGone { .. }
        | ClientError::Refused(
          Refusal::LeaseUnconfirmed { .. }
          | Refusal::Overloaded { .. }
          | Refusal::OwnerUnreachable { .. },
        ),
      ) if started.elapsed() < bound => {
        pause_ns = client.retry_pause_ns(pause_ns);
        let resume = Instant::now() + Duration::from_nanos(pause_ns);
        while Instant::now() < resume {
          std::thread::yield_now();
        }
      }
      Err(e) => panic!("harness {what}: {e} after {:?}", started.elapsed()),
    }
  }
}

/// Each worker's connections and logical clients: connection `k` goes to worker `k % workers`, and logical client `a`
/// over connection `a / per_connection`, so every connection's clients are its worker's.
fn spread(
  instance: &str,
  green: VolumeId,
  plans: Vec<Vec<Vec<Slot>>>,
  (n, per_connection, workers): (usize, usize, usize),
) -> Vec<(Vec<Client>, Vec<Agent>)> {
  let mut groups: Vec<(Vec<Client>, Vec<Agent>)> =
    (0..workers).map(|_| (Vec::new(), Vec::new())).collect();
  // `--instance a,b,…` names several client nodes: connection `k` goes to node `k` mod their count, so one harness, and
  // one oracle over every submit, drives them all.
  let instances: Vec<&str> = instance.split(',').collect();
  for k in 0..n.div_ceil(per_connection) {
    groups[k % workers]
      .0
      .push(connect(instances[k % instances.len()]).unwrap());
  }
  for (index, rounds) in plans.into_iter().enumerate() {
    let k = index / per_connection;
    groups[k % workers].1.push(Agent {
      id: index + n * 1_000_000,
      conn: k / workers,
      green,
      work: None,
      base: 0,
      rounds,
      round: 0,
      step: Step::CreateWork,
      windows: Vec::new(),
      resolving: (0, Vec::new()),
      own_len: 0,
      resolutions: 0,
      inflight: None,
      retry: None,
      cycle_since: None,
      round_since: None,
      cycle_thought_ns: 0,
      round_thought_ns: 0,
      op_since: None,
      pause_ns: 0,
      attempt_sent_ns: 0,
      handed_ns: 0,
    });
  }
  groups
}

/// The files long enough to hold a client's slots, or `None` (said, as a failure) when the green holds none: a green
/// over a scratch volume or a tree of tiny files gives the clients nothing to edit, never an index out of an empty list.
fn eligible_files(files: &[(String, u64)], n: usize) -> Option<Vec<(String, u64)>> {
  let eligible: Vec<(String, u64)> = files
    .iter()
    .filter(|(_, len)| *len >= SLOTS_PER_FILE * SLOT_STRIDE)
    .cloned()
    .collect();
  if eligible.is_empty() {
    println!(
      "oracle_failure\t{n}\tthe green holds no file of {} bytes or more to edit (of {} files)",
      SLOTS_PER_FILE * SLOT_STRIDE,
      files.len()
    );
    return None;
  }
  Some(eligible)
}

/// One load level: `n` clients over the green, `workers` threads sweeping them. Returns the oracle's failures.
fn level(
  instance: &str,
  green: VolumeId,
  files: &[(String, u64)],
  (n, rounds, hot_files): (usize, usize, usize),
  workers: usize,
) -> u64 {
  let Some(eligible) = eligible_files(files, n) else {
    return 1;
  };
  let mut rng = Rng(SEED ^ n as u64);
  let hot = match hot_set(&mut rng, eligible.len(), hot_files) {
    Ok(hot) => hot,
    Err(why) => {
      println!("refused\t{n}\t{why}");
      return 1;
    }
  };
  let plans: Vec<Vec<Vec<Slot>>> = (0..n)
    .map(|agent| {
      (0..rounds)
        .map(|round| plan_round(&mut rng, &eligible, &hot, agent, round))
        .collect()
    })
    .collect();
  let paths: BTreeSet<String> = plans
    .iter()
    .flatten()
    .flatten()
    .map(|slot| slot.path.clone())
    .collect();
  let mut reader = connect(first_instance(instance)).unwrap();
  let head_before = versions_patiently(&mut reader, green);
  let base = read_files(&mut reader, green, &paths, ReadAt::Head);
  let connect_started = Instant::now();
  let per_connection = per_connection();
  let connections = n.div_ceil(per_connection);
  let groups = spread(instance, green, plans, (n, per_connection, workers));
  let limit = groups
    .iter()
    .flat_map(|(clients, _)| clients.first())
    .map(Client::outstanding_limit)
    .next()
    .unwrap_or(0);
  println!(
    "level\t{n}\tconnect_all_ns\t{}\tconnections\t{connections}\tper_connection\t{per_connection}\toutstanding_limit\t{limit}",
    connect_started.elapsed().as_nanos()
  );
  // Past the connection's in-flight bound a client waits in the harness for its turn, which a latency stamped at the
  // send does not see but the client does: the run is not what it says it measures.
  assert!(
    per_connection <= limit,
    "--per-connection {per_connection} exceeds the connection's in-flight bound {limit}"
  );
  let think_mean = think_mean_ns();
  // The level's start on the host's monotonic clock, which every worker counts from (the module's "Each instant").
  let epoch = slates_machine::clock::monotonic_ns();
  let tallies: Vec<Tally> = std::thread::scope(|scope| {
    let handles: Vec<_> = groups
      .into_iter()
      .enumerate()
      .map(|(worker, (clients, agents))| {
        scope.spawn(move || {
          let mut clock = Clock::host(epoch);
          let started = clock.now();
          let conns = clients
            .into_iter()
            .map(|client| Connection::new(client, started))
            .collect();
          let think = Think {
            mean_ns: think_mean,
            draws: Rng(THINK_SEED ^ (n as u64) ^ ((worker as u64) << HIGH_HALF)),
          };
          drive(conns, agents, clock, worker, think)
        })
      })
      .collect();
    handles
      .into_iter()
      .map(|handle| handle.join().unwrap())
      .collect()
  });
  let wall_ns = slates_machine::clock::monotonic_ns().saturating_sub(epoch);
  let mut total = Tally::default();
  for tally in tallies {
    total.absorb(tally);
  }
  println!("level\t{n}\twall_ns\t{wall_ns}\tworkers\t{workers}");
  println!("{}", offered_row(n, think_mean, wall_ns, &total));
  report(n, &mut total);
  // Version numbers in the events are absolute; rebase them to this level's first version for the oracle.
  for event in &mut total.submits {
    event.base = event.base.saturating_sub(head_before);
    if let Outcome::Accepted(v) = &mut event.outcome {
      *v -= head_before;
    }
  }
  // The oracle reads through a connection made now: the one made before the load may belong to a daemon a kill rung
  // has since restarted (its `versions` answered `DaemonGone` after the primary's kill, 2026-11-07).
  let mut reader = connect(first_instance(instance)).unwrap();
  let unexplained = resolve_unknown(&mut reader, green, head_before, &mut total.submits);
  if std::env::args().any(|a| a == "--events") {
    for event in &total.submits {
      let slots: Vec<String> = event
        .slots
        .iter()
        .map(|s| format!("{}@{}", s.path, s.at))
        .collect();
      println!(
        "event\t{n}\t{}\t{}\t{}\t{}\t{:?}\t{:?}\t{}",
        event.agent,
        event.base,
        event.sent_ns,
        event.replied_ns,
        event.outcome,
        event.classes,
        slots.join(",")
      );
    }
  }
  for version in &unexplained {
    println!(
      "oracle_failure\t{n}\tversion {version} is on the green but no submit of this level, known or unknown, explains it"
    );
  }
  let judged = oracle(n, &total.submits, base, |path| {
    read_patiently(&mut reader, green, path, ReadAt::Head)
  });
  let failures = u64::try_from(unexplained.len())
    .unwrap_or(u64::MAX)
    .saturating_add(judged)
    .saturating_add(own_files_oracle(n, &mut reader, green, &total.submits));
  println!("{}", bound_row(n, wall_ns, &total, failures));
  failures
}

/// Format: the hexadecimal digits of a volume id, as the CLI prints it.
const VOLUME_ID_HEX: usize = 32;

/// A volume id from its 32 hexadecimal characters, as `slates` prints it.
fn parse_volume_id(text: &str) -> VolumeId {
  let text = text.trim();
  assert_eq!(
    text.len(),
    VOLUME_ID_HEX,
    "--green takes {VOLUME_ID_HEX} hexadecimal characters"
  );
  let mut bytes = [0u8; 16];
  for (byte, pair) in bytes.iter_mut().zip(text.as_bytes().chunks(2)) {
    *byte =
      u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).expect("--green is hexadecimal");
  }
  VolumeId { bytes }
}

fn arg(name: &str) -> Option<String> {
  let mut args = std::env::args();
  args.find(|a| a == name).and_then(|_| args.next())
}

/// Format: the daemon's counters of a reply, a result, a release or an ask that was lost or refused for want of room, read
/// at the end of a run: `lost\t<counter>\t<count>`, all shards summed, a zero printed too. Since A-232 a client waits
/// for a lost reply while its daemon lives, so a run's latencies alone do not show one (goal 238: about 4,200
/// `xshard.reply_dropped` a run of the 3,000-client rung on main before the carrier).
const LOSS_COUNTERS: [&str; 15] = [
  "xshard.reply_dropped",
  "xshard.keep_refused",
  "xshard.unstarted_unanswered",
  "ack.release_refused",
  "fuse.reply_undelivered",
  "image.reply_undelivered",
  "landing.reply_undelivered",
  "lease.wait.undelivered",
  "fleet.live.undelivered",
  "merge.acceptance_undelivered",
  "winfsp.reply_undelivered",
  "merge.work.ask_unanswered",
  "merge.work.park_full",
  "merge.work.pending_stuck",
  "merge.work.floor_not_sent",
];

/// Prints [`LOSS_COUNTERS`], the daemon's waits for room (`room\t<counter>\t<count>`: begun and ended summed over the
/// shards, and the most any one shard held at once) and where a work's submissions were composed
/// (`counter\t<counter>\t<count>`), from the daemon's own report.
fn report_losses(client: &mut Client) {
  let report = client.daemon_status().unwrap();
  let per_shard = |name: &'static str| {
    report.shards.iter().map(move |shard| {
      shard
        .refusals
        .iter()
        .filter(|refusal| refusal.kind == name)
        .map(|refusal| refusal.count)
        .sum::<u64>()
    })
  };
  for name in LOSS_COUNTERS {
    println!("lost\t{name}\t{}", per_shard(name).sum::<u64>());
  }
  for name in ["xshard.room_waits_started", "xshard.room_waits_ended"] {
    println!("room\t{name}\t{}", per_shard(name).sum::<u64>());
  }
  let peak = per_shard("xshard.room_waits_peak").max().unwrap_or(0);
  println!("room\txshard.room_waits_peak\t{peak}");
  for name in [
    "merge.work.composed_on_work",
    "merge.work.composed_by_owner",
  ] {
    println!("counter\t{name}\t{}", per_shard(name).sum::<u64>());
  }
}

fn main() {
  let tree = arg("--tree").unwrap_or_default();
  let levels: Vec<usize> = arg("--clients")
    .unwrap_or_else(|| "10".to_owned())
    .split(',')
    .map(|n| n.parse().expect("--clients n[,n…]"))
    .collect();
  let profile = MachineProfile::measure_patiently(&ProfileOptions {
    budget_per_probe: ProfileOptions::default().budget_per_probe,
    codecs: false,
    core_matrix: false,
  })
  .expect("the machine profile measures");
  let cpus = profile.facts.cpus_at_once();
  let workers: usize =
    arg("--workers").map_or((cpus / 4).max(1), |w| w.parse().expect("--workers n"));
  let rounds: usize = arg("--rounds").map_or(ROUNDS, |r| r.parse().expect("--rounds n"));
  let hot_files: usize =
    arg("--hot-files").map_or(HOT_FILES, |h| h.parse().expect("--hot-files n"));
  // `--instance <name>[,<name>…]` drives daemons already running under their own anchors (`slates --instance <name> anchor`), so a
  // chaos script can kill the daemon process mid-run and the anchor restart it; without it the daemon is in-process.
  let external = arg("--instance");
  let instance = external
    .clone()
    .unwrap_or_else(|| format!("load-bench-{}", std::process::id()));
  let daemon = if external.is_some() {
    println!("daemon\texternal\t{instance}\tcpus\t{cpus}");
    None
  } else {
    let config = DaemonConfig::derive(&profile, &instance, None);
    println!(
      "daemon\tshards\t{}\tclients_per_shard\t{}\tcpus\t{cpus}",
      config.runtime.shards, config.clients_per_shard
    );
    let daemon = Daemon::start(
      &profile,
      config,
      SegmentSource::Create {
        name: format!("slates-seg-{instance}"),
      },
    )
    .unwrap();
    daemon
      .bootstrap(true)
      .expect("the fixture explicitly creates its local consensus group");
    Some(daemon)
  };
  let mut setup = connect(first_instance(&instance)).unwrap();
  // `--green <32 hex>` drives a green that already exists, on this node or another node of its fleet (the client
  // node forwards every verb to the green's owner, A-201's filesystem server); without it the harness seeds its own
  // over `--tree`.
  let green = match arg("--green") {
    Some(text) => parse_volume_id(&text),
    None => seed_green(&mut setup, &tree, "load"),
  };
  let step = Instant::now();
  let files = walk(&mut setup, green);
  let bytes: u64 = files.iter().map(|(_, len)| len).sum();
  println!(
    "setup\twalk_ns\t{}\tfiles\t{}\tbytes\t{bytes}",
    step.elapsed().as_nanos(),
    files.len()
  );
  let mut failures = 0;
  for n in levels {
    failures += level(
      &instance,
      green,
      &files,
      (n, rounds, hot_files),
      workers.min(n),
    );
  }
  report_losses(&mut setup);
  drop(setup);
  if let Some(daemon) = daemon {
    daemon.stop();
  }
  if failures > 0 {
    std::process::exit(1);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Shape: the draws each distribution test takes: enough that five standard errors are about 1% of the mean.
  const DRAWS: u64 = 200_000;
  /// Shape: the stated mean the tests draw at, a millisecond.
  const MEAN_NS: u64 = 1_000_000;
  /// Shape: the standard errors a sample statistic may stray before a test fails (about one false failure in 1.7
  /// million runs for a normal statistic).
  const STANDARD_ERRORS: u64 = 5;

  /// AC-9.1 (the load test's stated offered load): a level at a mean of 0 draws no pause, so it stays the closed loop
  /// at capacity, every client always waiting on a request.
  #[test]
  fn a_mean_of_zero_draws_no_pause() {
    let mut draws = Rng(THINK_SEED);
    assert!((0..DRAWS).all(|_| draws.exponential_ns(0) == 0));
  }

  /// AC-9.1: the pauses follow the exponential distribution of the stated mean: draw 200,000 at a mean of 1 ms, and
  /// their mean is the stated mean, the share of them past the mean is e⁻¹ and the share past twice the mean e⁻²
  /// (the memoryless distribution's own shares; a fixed pause would put all or none past), each within five standard
  /// errors.
  #[test]
  fn pauses_follow_the_exponential_distribution_of_the_stated_mean() {
    let mut draws = Rng(THINK_SEED);
    let pauses: Vec<u64> = (0..DRAWS).map(|_| draws.exponential_ns(MEAN_NS)).collect();
    let sum: u128 = pauses.iter().map(|pause| u128::from(*pause)).sum();
    let mean = u64::try_from(sum / u128::from(DRAWS)).unwrap();
    // The exponential's standard deviation is its mean, so the sample mean's standard error is the mean over √n.
    let tolerance = STANDARD_ERRORS * MEAN_NS / DRAWS.isqrt();
    assert!(
      mean.abs_diff(MEAN_NS) <= tolerance,
      "the draws' mean is {mean} ns against a stated {MEAN_NS} ± {tolerance}"
    );
    for multiple in [1u64, 2] {
      let past = pauses
        .iter()
        .filter(|pause| **pause > multiple * MEAN_NS)
        .count();
      let share =
        f64::from(u32::try_from(past).unwrap()) / f64::from(u32::try_from(DRAWS).unwrap());
      let expected = (-f64::from(u32::try_from(multiple).unwrap())).exp();
      let error = (expected * (1.0 - expected) / f64::from(u32::try_from(DRAWS).unwrap())).sqrt();
      assert!(
        (share - expected).abs() <= f64::from(u32::try_from(STANDARD_ERRORS).unwrap()) * error,
        "{share} of the pauses run past {multiple} means against e^-{multiple} = {expected}"
      );
    }
  }

  /// AC-9.1: two workers draw their own pauses, so clients on different workers do not keep step: of the first 1,000
  /// draws of workers 0 and 1 at one level, fewer than 1% fall on the same nanosecond at the same position.
  #[test]
  fn two_workers_draw_their_own_pauses() {
    let draws = |worker: u64| {
      let mut rng = Rng(THINK_SEED ^ 1_000 ^ (worker << HIGH_HALF));
      (0..1_000)
        .map(|_| rng.exponential_ns(MEAN_NS))
        .collect::<Vec<u64>>()
    };
    let same = draws(0)
      .iter()
      .zip(draws(1).iter())
      .filter(|(first, second)| first == second)
      .count();
    assert!(
      same < 10,
      "{same} of 1,000 draws fell on the same nanosecond"
    );
  }

  /// AC-9.1: a level's clients' time is reported whole: over a 2 s wall, clients on average 600 in flight, 300
  /// thinking, 50 pausing and 50 held are 1,000 not yet done, beside 20,000 replies a second, so a level whose harness
  /// kept clients back from sending shows it next to latencies that cannot.
  #[test]
  fn the_offered_row_reports_each_state_of_the_clients_and_their_sum() {
    const WALL_NS: u64 = 2_000_000_000;
    let tally = Tally {
      answered: 40_000,
      in_flight_ns: 600 * WALL_NS,
      thinking_ns: 300 * WALL_NS,
      paused_ns: 50 * WALL_NS,
      held_ns: 50 * WALL_NS,
      ..Tally::default()
    };
    let row = offered_row(1_000, MEAN_NS, WALL_NS, &tally);
    for field in [
      "\treplies_per_s\t20000\t",
      "\talive\t1000.0\t",
      "\tin_flight\t600.0\t",
      "\tthinking\t300.0\t",
      "\tpaused\t50.0\t",
    ] {
      assert!(row.contains(field), "{row:?} lacks {field:?}");
    }
    assert!(row.ends_with("\theld\t50.0"), "{row:?}");
  }

  /// AC-9.1: a level that ended before any time passed reports rates and clients of 0, never a division by zero.
  #[test]
  fn a_level_of_no_wall_time_reports_zeroes() {
    let tally = Tally {
      answered: 7,
      held_ns: 3,
      ..Tally::default()
    };
    let row = offered_row(1, 0, 0, &tally);
    assert!(row.contains("\treplies_per_s\t0\t"), "{row:?}");
    assert!(row.ends_with("\theld\t0.0"), "{row:?}");
  }

  /// Shape: a second, the wall of the bound row's tests.
  const SECOND_NS: u64 = 1_000_000_000;

  /// A client whose work exists, at its first round's first edit; the round edits two slots of one file.
  fn agent_at_its_first_edit() -> Agent {
    let slot = |at: u64| Slot {
      path: "/f".to_owned(),
      at,
      bytes: vec![b'x'; usize::try_from(SLOT_BYTES).unwrap()],
    };
    Agent {
      own_len: 0,
      resolving: (0, Vec::new()),
      id: 0,
      conn: 0,
      green: VolumeId { bytes: [1; 16] },
      work: Some(VolumeId { bytes: [2; 16] }),
      base: 0,
      rounds: vec![vec![slot(0), slot(SLOT_STRIDE)]],
      round: 0,
      step: Step::Edit(0),
      windows: Vec::new(),
      resolutions: 0,
      inflight: None,
      retry: None,
      cycle_since: None,
      round_since: None,
      cycle_thought_ns: 0,
      round_thought_ns: 0,
      op_since: None,
      pause_ns: 0,
      attempt_sent_ns: 0,
      handed_ns: 0,
    }
  }

  /// Runs `replies` through `agent` in turn as a worker does: each sent `pause_ns` after the last was taken and
  /// answered `latency_ns` after it was sent, the pause drawn after each reply counted as the client's thinking.
  /// Returns when the last was taken.
  fn exchange(
    agent: &mut Agent,
    tally: &mut Tally,
    mut now: u64,
    (latency_ns, pause_ns): (u64, u64),
    replies: Vec<(ReplyBody, Kind)>,
  ) -> u64 {
    for (reply, kind) in replies {
      let sent = now;
      now = sent + latency_ns;
      agent.advance(reply, (sent, now, kind), sent, tally);
      agent.thought(pause_ns);
      now += pause_ns;
    }
    now - pause_ns
  }

  /// A submit's reply: accepted as `version`, or in conflict over `conflicts`.
  fn verdict(version: Option<u64>, conflicts: Vec<MergeWindow>) -> (ReplyBody, Kind) {
    (
      ReplyBody::Submitted {
        version,
        conflicts,
        conflicts_elided: 0,
        verdict: 0,
      },
      Kind::SubmitAccepted,
    )
  }

  fn only(tally: &Tally, kind: Kind) -> Vec<u64> {
    tally.latencies.get(&kind).cloned().unwrap_or_default()
  }

  /// AC-9.1 (the bound's own measure, a whole edit and merge): a client edits two slots, appends and submits, thinking
  /// 50 ms after each reply, each request answered in 100 µs; its cycle and its round are its four requests, 400 µs,
  /// not the 150 ms it thought between them, and the pause after its accept is in neither.
  #[test]
  fn a_cycle_and_a_round_leave_out_their_clients_think_pauses() {
    const LATENCY_NS: u64 = 100_000;
    const PAUSE_NS: u64 = 50_000_000;
    let mut agent = agent_at_its_first_edit();
    let mut tally = Tally::default();
    exchange(
      &mut agent,
      &mut tally,
      SECOND_NS,
      (LATENCY_NS, PAUSE_NS),
      vec![
        (ReplyBody::Edited, Kind::Edit),
        (ReplyBody::Edited, Kind::Edit),
        (ReplyBody::Edited, Kind::Edit),
        verdict(Some(1), Vec::new()),
      ],
    );
    assert_eq!(only(&tally, Kind::Cycle), vec![4 * LATENCY_NS]);
    assert_eq!(only(&tally, Kind::Round), vec![4 * LATENCY_NS]);
    assert_eq!(agent.step, Step::Destroy, "one round, then the work goes");
  }

  /// AC-9.1: a round that meets a conflict and resolves it is every request of it, the resolution's too, and none of
  /// the pauses: the cycle ends at the first verdict (four requests), the round at the accept (eleven).
  #[test]
  fn a_round_through_a_conflict_counts_its_resolution_and_none_of_its_pauses() {
    const LATENCY_NS: u64 = 100_000;
    const PAUSE_NS: u64 = 30_000_000;
    let mut agent = agent_at_its_first_edit();
    let mut tally = Tally::default();
    let window = MergeWindow {
      path: "/f".to_owned(),
      at: 0,
      len: SLOT_BYTES,
      class: 0,
    };
    let now = exchange(
      &mut agent,
      &mut tally,
      SECOND_NS,
      (LATENCY_NS, PAUSE_NS),
      vec![
        (ReplyBody::Edited, Kind::Edit),
        (ReplyBody::Edited, Kind::Edit),
        (ReplyBody::Edited, Kind::Edit),
        verdict(None, vec![window]),
      ],
    );
    assert_eq!(only(&tally, Kind::Cycle), vec![4 * LATENCY_NS]);
    assert!(only(&tally, Kind::Round).is_empty(), "the round is open");
    let window_bytes = vec![b'g'; usize::try_from(SLOT_BYTES).unwrap()];
    exchange(
      &mut agent,
      &mut tally,
      now + PAUSE_NS,
      (LATENCY_NS, PAUSE_NS),
      vec![
        (ReplyBody::Versions { head: 1 }, Kind::ReadHead),
        (
          ReplyBody::ReadPage {
            bytes: window_bytes,
            total: SLOT_BYTES,
            stamp: 0,
          },
          Kind::ReadHead,
        ),
        (ReplyBody::Edited, Kind::Edit),
        (
          ReplyBody::Rebased {
            version: Some(1),
            conflicts: Vec::new(),
            conflicts_elided: 0,
            verdict: 0,
          },
          Kind::Rebase,
        ),
        (ReplyBody::Edited, Kind::Edit),
        (ReplyBody::Edited, Kind::Edit),
        verdict(Some(2), Vec::new()),
      ],
    );
    assert_eq!(only(&tally, Kind::Cycle), vec![4 * LATENCY_NS], "one cycle");
    assert_eq!(only(&tally, Kind::Round), vec![11 * LATENCY_NS]);
  }

  /// AC-9.1: the cycle starts at its first edit's first send, so a refusal for now of that edit and the client's
  /// pause after it are in the cycle: the edit first sent at 1 s is refused and answered on its resend at 1.003 s.
  #[test]
  fn a_cycle_starts_at_its_first_edits_first_send() {
    let mut agent = agent_at_its_first_edit();
    let mut tally = Tally::default();
    let first = SECOND_NS;
    let resent = first + 2_000_000;
    agent.advance(
      ReplyBody::Edited,
      (resent, resent + 1_000_000, Kind::Edit),
      first,
      &mut tally,
    );
    exchange(
      &mut agent,
      &mut tally,
      resent + 1_000_000,
      (0, 0),
      vec![
        (ReplyBody::Edited, Kind::Edit),
        (ReplyBody::Edited, Kind::Edit),
        verdict(Some(1), Vec::new()),
      ],
    );
    assert_eq!(only(&tally, Kind::Cycle), vec![3_000_000]);
  }

  /// A tally of `verdicts` accepted submits each answered in `submit_ns`, and as many edits in `edit_ns`, each one
  /// operation.
  fn tally_of(verdicts: usize, submit_ns: u64, edit_ns: u64) -> Tally {
    let mut tally = Tally::default();
    for _ in 0..verdicts {
      record(&mut tally, Kind::SubmitAccepted, submit_ns);
      record(&mut tally, Kind::Edit, edit_ns);
      tally.operation_took(Kind::SubmitAccepted, submit_ns);
      tally.operation_took(Kind::Edit, edit_ns);
    }
    tally
  }

  /// AC-9.1 (the bound, Ada's decision of 2026-11-18): 1,000 verdicts in a second, every kind's p99 under 1 ms, no
  /// failure and nobody given up meets it, naming its worst kind; a level at the bound's own rate is enough.
  #[test]
  fn a_level_with_every_kind_under_the_bound_at_its_rate_meets_it() {
    let tally = tally_of(1_000, 400_000, 90_000);
    let row = bound_row(1_000, SECOND_NS, &tally, 0);
    for field in [
      "bound\t1000\tp99_bound_ns\t1000000\tverdicts_floor_per_s\t1000\t",
      "\tverdicts_per_s\t1000\t",
      "\tworst\toperation\tSubmitAccepted\tworst_p99_ns\t400000\t",
      "\tgave_up\t0\toracle_failures\t0\t",
    ] {
      assert!(row.contains(field), "{row:?} lacks {field:?}");
    }
    assert!(row.ends_with("\tmet\tyes"), "{row:?}");
  }

  /// AC-9.1: one kind's p99 over the bound fails the level and is named, whatever the others: two of 100 rebases at
  /// 1.5 ms put the rebases' p99 there; a p99 of exactly 1 ms is not under it.
  #[test]
  fn one_kind_over_the_bound_fails_the_level_and_is_named() {
    let mut tally = tally_of(1_000, 400_000, 90_000);
    for at in 0..100 {
      tally.operation_took(Kind::Rebase, if at < 2 { 1_500_000 } else { 200_000 });
    }
    let row = bound_row(1_000, SECOND_NS, &tally, 0);
    assert!(
      row.contains("\tworst\toperation\tRebase\tworst_p99_ns\t1500000\t"),
      "{row:?}"
    );
    assert!(row.ends_with("\tmet\tno"), "{row:?}");
    let at_the_bound = tally_of(1_000, BOUND_P99_NS, 90_000);
    let row = bound_row(1_000, SECOND_NS, &at_the_bound, 0);
    assert!(row.ends_with("\tmet\tno"), "{row:?}");
  }

  /// AC-9.1: a level below the bound's rate, with an oracle failure, with a client given up or starved, or with nothing
  /// timed does not meet it, however fast its replies.
  #[test]
  fn a_level_below_the_rate_or_failed_or_incomplete_does_not_meet_the_bound() {
    let fast = || tally_of(1_000, 10_000, 10_000);
    let below = bound_row(1_000, 2 * SECOND_NS, &fast(), 0);
    assert!(below.contains("\tverdicts_per_s\t500\t"), "{below:?}");
    assert!(below.ends_with("\tmet\tno"), "{below:?}");
    let failed = bound_row(1_000, SECOND_NS, &fast(), 1);
    assert!(
      failed.ends_with("\toracle_failures\t1\tmet\tno"),
      "{failed:?}"
    );
    let mut gave_up = fast();
    gave_up.refusals.insert("Submit: NotFound".to_owned(), 1);
    let row = bound_row(1_000, SECOND_NS, &gave_up, 0);
    assert!(
      row.contains("\tgave_up\t1\t") && row.ends_with("\tmet\tno"),
      "{row:?}"
    );
    let mut starved = fast();
    starved.starved = 1;
    let row = bound_row(1_000, SECOND_NS, &starved, 0);
    assert!(
      row.contains("\tgave_up\t1\t") && row.ends_with("\tmet\tno"),
      "{row:?}"
    );
    let empty = bound_row(1_000, SECOND_NS, &Tally::default(), 0);
    assert!(
      empty.contains("\tworst\tnone\tnone\tworst_p99_ns\t0\t"),
      "{empty:?}"
    );
    assert!(empty.ends_with("\tmet\tno"), "{empty:?}");
  }

  /// AC-9.1 (A-416): a round, which holds as many edits and merges as its conflicts make, is reported and never judged:
  /// a level whose rounds' p99 is 191 ms (the Mac Studio's at 1,000 clients, 2026-11-18) with every request and cycle
  /// under the bound meets it, naming a judged kind as its worst; a cycle over the bound, a whole edit and merge, fails
  /// it and is named.
  #[test]
  fn a_round_is_reported_and_not_judged_and_a_cycle_is_judged() {
    let mut tally = tally_of(1_000, 400_000, 90_000);
    for _ in 0..100 {
      record(&mut tally, Kind::Round, 191_000_000);
      record(&mut tally, Kind::Cycle, 900_000);
    }
    let row = bound_row(1_000, SECOND_NS, &tally, 0);
    assert!(
      row.contains("\tworst\tlatency\tCycle\tworst_p99_ns\t900000\t"),
      "{row:?}"
    );
    assert!(row.ends_with("\tmet\tyes"), "{row:?}");
    for _ in 0..100 {
      record(&mut tally, Kind::Cycle, 1_200_000);
    }
    let row = bound_row(1_000, SECOND_NS, &tally, 0);
    assert!(
      row.contains("\tworst\tlatency\tCycle\tworst_p99_ns\t1200000\t"),
      "{row:?}"
    );
    assert!(row.ends_with("\tmet\tno"), "{row:?}");
  }

  /// Shape: the eligible files of the hot-set tests, about as many as `fs/` of linux v7.4-rc1 holds (2,365 files).
  const ELIGIBLE: usize = 2_000;
  /// Shape: the hot set the 1,000-client rung asks for (`--hot-files 800`).
  const RUNG_HOT_FILES: usize = 800;

  /// The module's "The workload": a hot set names each file once, inside the eligible files, and the same seed draws
  /// the same set. Drawn with replacement, 800 draws over 2,000 files name only about 660 of them, so about 140 draws
  /// repeat a file, each such file twice as hot as the rest of the set.
  #[test]
  fn a_hot_set_names_each_file_once() {
    let hot = hot_set(&mut Rng(SEED), ELIGIBLE, RUNG_HOT_FILES).unwrap();
    let distinct: BTreeSet<usize> = hot.iter().copied().collect();
    assert_eq!(hot.len(), RUNG_HOT_FILES);
    assert_eq!(distinct.len(), RUNG_HOT_FILES, "a file named twice");
    assert!(hot.iter().all(|file| *file < ELIGIBLE));
    assert_eq!(
      hot_set(&mut Rng(SEED), ELIGIBLE, RUNG_HOT_FILES).unwrap(),
      hot
    );
    let all = hot_set(&mut Rng(SEED), ELIGIBLE, ELIGIBLE / HOT_SET_SHARE_DIVISOR).unwrap();
    let distinct: BTreeSet<usize> = all.iter().copied().collect();
    assert_eq!(distinct.len(), ELIGIBLE / HOT_SET_SHARE_DIVISOR);
  }

  /// The module's "The workload": a hot set past half the files long enough to edit is refused, never shrunk to fit
  /// (`kernel/`'s 700-odd files and the rung's 800 named every file hot); exactly half is a hot set.
  #[test]
  fn a_hot_set_past_half_the_eligible_files_is_refused() {
    let why = hot_set(&mut Rng(SEED), 701, RUNG_HOT_FILES).unwrap_err();
    assert!(
      why.contains("--hot-files 800") && why.contains("701"),
      "{why}"
    );
    assert!(hot_set(&mut Rng(SEED), 701, 351).is_err());
    assert_eq!(hot_set(&mut Rng(SEED), 701, 350).unwrap().len(), 350);
    assert_eq!(hot_set(&mut Rng(SEED), 1, 0).unwrap(), Vec::<usize>::new());
  }

  /// The module's "The workload": with no hot set (`--hot-files 0`) every pick is from the whole tree, and no shared
  /// slot is written; it indexed an empty set before.
  #[test]
  fn with_no_hot_set_every_pick_is_from_the_whole_tree() {
    let files: Vec<(String, u64)> = (0..ELIGIBLE)
      .map(|file| (format!("/f{file}"), SLOTS_PER_FILE * SLOT_STRIDE))
      .collect();
    let mut rng = Rng(SEED);
    let shared = slot_bytes(0, 0, true);
    for agent in 0..RUNG_HOT_FILES {
      let slots = plan_round(&mut rng, &files, &[], agent, 0);
      assert_eq!(slots.len(), EDITS_PER_ROUND);
      for slot in slots {
        assert_ne!(slot.bytes, shared, "a shared slot with no hot set");
      }
    }
  }

  /// Shape: shards of the stamp tests' daemon: one, so a request is served on the shard that reads its slot, and holding
  /// that shard holds the request.
  const STAMP_SHARDS: u16 = 1;
  /// Shape: each probe's wall budget for the stamp tests' machine profile (milliseconds).
  const PROBE_MS: u64 = 5;
  /// Shape: how long the stamp tests' client retries the rendezvous while its daemon starts.
  const START_WAIT: Duration = Duration::from_secs(10);
  /// Shape: how long the stamp tests' host keeps the worker off its CPU: far past what one quiet shard takes to answer a
  /// work's creation (microseconds), so the daemon answers while the worker is away, and well inside the watchdog's
  /// one-second window, which the hold before it spends no more than this of.
  const PREEMPTION: Duration = Duration::from_millis(100);
  /// Shape: the budget of the observation that holds the daemon's shard, the daemon's own liveness budget.
  const HOLD_BUDGET_NS: u64 = slates_server::daemon::LIVENESS_BUDGET_NS;

  /// A daemon's one shard held from another thread, released by a test's clock.
  struct Hold {
    /// The holder is on the shard.
    held: std::sync::atomic::AtomicBool,
    /// The holder stays while this is set.
    holding: std::sync::atomic::AtomicBool,
  }

  impl Hold {
    const fn new() -> Hold {
      Hold {
        held: std::sync::atomic::AtomicBool::new(false),
        holding: std::sync::atomic::AtomicBool::new(false),
      }
    }

    fn release(&self) {
      self
        .holding
        .store(false, std::sync::atomic::Ordering::SeqCst);
    }
  }

  /// The send test's hold, apart from the reply test's, since the two run at once.
  static SEND_HOLD: Hold = Hold::new();
  /// The reply test's hold.
  static REPLY_HOLD: Hold = Hold::new();

  /// Holds `daemon`'s control shard, its only one, until `hold` is released: an observation spinning on the shard,
  /// waited for on its own thread. Returns once the shard is held.
  fn hold_the_shard(daemon: &Daemon, hold: &'static Hold) -> std::thread::JoinHandle<()> {
    use std::sync::atomic::Ordering::SeqCst;
    hold.holding.store(true, SeqCst);
    let observation = daemon
      .observation_on_control(
        HOLD_BUDGET_NS,
        move |_: &mut slates_server::state::ShardState| {
          hold.held.store(true, SeqCst);
          while hold.holding.load(SeqCst) {
            std::hint::spin_loop();
          }
        },
      )
      .unwrap();
    let holder = std::thread::spawn(move || observation.wait().unwrap());
    while !hold.held.load(SeqCst) {
      std::thread::yield_now();
    }
    holder
  }

  /// A one-shard daemon with a green whose file `f` holds a client's slots, an observer client, and the worker's client.
  fn stamp_daemon(tag: &str) -> (Daemon, Client, Client, VolumeId) {
    let profile = MachineProfile::measure_patiently(&ProfileOptions {
      budget_per_probe: Duration::from_millis(PROBE_MS),
      codecs: false,
      core_matrix: false,
    })
    .unwrap();
    let instance = format!("lb-{tag}-{}", std::process::id());
    let config = DaemonConfig::derive(&profile, &instance, Some(STAMP_SHARDS));
    let daemon = Daemon::start(
      &profile,
      config,
      SegmentSource::Create {
        name: format!("slates-seg-lb-{tag}-{}", std::process::id()),
      },
    )
    .unwrap();
    daemon.bootstrap(true).unwrap();
    let started = Instant::now();
    let connect = || loop {
      match Client::connect(&instance, deadlines()) {
        Err(ClientError::Ipc(slates_ipc::IpcError::DaemonUnavailable { .. }))
          if started.elapsed() < START_WAIT =>
        {
          std::thread::yield_now();
        }
        other => return other.unwrap(),
      }
    };
    let mut observer = connect();
    let green = observer.create_green("g", false).unwrap();
    let (seed, _) = observer.create_work(green, "seed").unwrap();
    let bytes = vec![b'.'; usize::try_from(SLOTS_PER_FILE * SLOT_STRIDE).unwrap()];
    observer.edit(seed, "f", 0, 0, &bytes).unwrap();
    observer.submit(seed).unwrap();
    let worker_client = connect();
    (daemon, observer, worker_client, green)
  }

  /// A worker of one client about to create its work over `green`, on `client`, reading `read` for its clock.
  fn worker_creating_a_work(client: Client, green: VolumeId, read: ClockRead) -> Worker {
    let mut agent = agent_at_its_first_edit();
    agent.green = green;
    agent.work = None;
    agent.step = Step::CreateWork;
    Worker {
      conns: vec![Connection::new(client, 0)],
      agents: vec![agent],
      ready: std::collections::BinaryHeap::new(),
      think: Think {
        mean_ns: 0,
        draws: Rng(THINK_SEED),
      },
      tally: Tally::default(),
      // Epoch 0: the worker's instants are the host's monotonic clock itself, the daemon's spans' clock.
      clock: Clock { read, epoch: 0 },
    }
  }

  /// Every span the daemon's one shard holds, drained.
  fn spans(observer: &mut Client) -> (Vec<String>, Vec<slates_ipc::protocol::SpanRecord>) {
    let mut names = Vec::new();
    let mut spans = Vec::new();
    loop {
      let report = observer.telemetry(0).unwrap();
      names = report
        .chokepoints
        .iter()
        .map(|point| point.name.clone())
        .collect();
      spans.extend(report.spans);
      if report.remaining == 0 {
        return (names, spans);
      }
    }
  }

  /// The daemon's `ring.request` span of the request `word`: its slot read to its reply written, on the shard's clock.
  fn ring_request(observer: &mut Client, word: u64) -> slates_ipc::protocol::SpanRecord {
    let id = slates_client::RequestId::from_word(word);
    let (names, spans) = spans(observer);
    let point = names
      .iter()
      .position(|name| name == "ring.request")
      .unwrap();
    spans
      .into_iter()
      .find(|span| {
        usize::try_from(span.point).unwrap() == point
          && span.request_client == id.client
          && span.request_sequence == id.sequence
      })
      .unwrap()
  }

  /// AC-9.1 (the oracle's bounds rest on the harness's instants, `support/load_oracle.rs`): a worker the host keeps off
  /// its CPU before every clock read, long enough for the daemon to answer what it handed its ring, still stamps that
  /// request sent before the daemon wrote its reply. Stamped after the send returned, it recorded a send 100 ms after
  /// the answer, as the measurer's run recorded a submit sent after the verdict that reflected it (2026-11-18).
  #[test]
  fn a_send_is_stamped_before_the_daemon_can_have_answered_it() {
    let (daemon, mut observer, client, green) = stamp_daemon("send");
    let _ = spans(&mut observer);
    let holder = hold_the_shard(&daemon, &SEND_HOLD);
    // The host takes the worker off its CPU before each of its clock reads, and the shard, let go at the first, answers
    // whatever the worker handed its ring while the worker is away.
    let mut first = true;
    let read: ClockRead = Box::new(move || {
      if std::mem::take(&mut first) {
        SEND_HOLD.release();
      }
      std::thread::sleep(PREEMPTION);
      slates_machine::clock::monotonic_ns()
    });
    let mut worker = worker_creating_a_work(client, green, read);
    assert!(worker.send(0), "the creation went");
    let (word, sent_ns, _) = worker.agents[0].inflight.unwrap();
    holder.join().unwrap();
    while worker.agents[0].inflight.is_some() {
      worker.drain(0);
    }
    assert_eq!(worker.agents[0].step, Step::Edit(0), "the work was made");
    let ring = ring_request(&mut observer, word);
    assert!(
      sent_ns < ring.end_ns,
      "the send is stamped {} ns after the daemon wrote its reply",
      sent_ns.saturating_sub(ring.end_ns)
    );
    daemon.stop();
  }

  /// AC-9.1: a worker the host keeps off its CPU while it drains a ring, long enough for the reply it waits for to come,
  /// stamps that reply no earlier than the daemon's reading of its request. Stamped before the ring was read, the
  /// reply it took then was recorded before its request was even read.
  #[test]
  fn a_reply_is_stamped_after_the_ring_it_was_taken_from_was_read() {
    let (daemon, mut observer, client, green) = stamp_daemon("reply");
    let _ = spans(&mut observer);
    let holder = hold_the_shard(&daemon, &REPLY_HOLD);
    let mut worker =
      worker_creating_a_work(client, green, Box::new(slates_machine::clock::monotonic_ns));
    assert!(worker.send(0), "the creation went");
    let (word, sent_ns, _) = worker.agents[0].inflight.unwrap();
    // The host takes the worker off its CPU right after its next clock read, the drain's: the shard, let go now,
    // answers while it is away.
    let mut preempted = true;
    worker.clock.read = Box::new(move || {
      let now = slates_machine::clock::monotonic_ns();
      if std::mem::take(&mut preempted) {
        REPLY_HOLD.release();
        std::thread::sleep(PREEMPTION);
      }
      now
    });
    while worker.agents[0].inflight.is_some() {
      worker.drain(0);
    }
    holder.join().unwrap();
    assert_eq!(worker.agents[0].step, Step::Edit(0), "the work was made");
    let replied_ns = sent_ns + only(&worker.tally, Kind::CreateWork)[0];
    let ring = ring_request(&mut observer, word);
    assert!(
      replied_ns > ring.start_ns,
      "the reply is stamped {} ns before the daemon read its request",
      ring.start_ns.saturating_sub(replied_ns)
    );
    daemon.stop();
  }
}
