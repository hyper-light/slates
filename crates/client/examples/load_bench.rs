//! The many-client load test (§4.16 "Submission", "Rebase, the only corrective path"; §4.7; Phase 9 "Scale, soak,
//! chaos"): N clients, each its own connection, each creating a work volume over one green whose version 0 is a real
//! source tree (the latest Linux kernel), editing files with deliberate overlaps so that conflicts happen, submitting,
//! and resolving every conflict through the design's corrective path. Every operation's latency is recorded from the
//! moment its request is sent to the moment its reply is taken, and reported per kind as p50, p99, p999 and max.
//!
//! **The workload.** Each client runs `rounds` rounds. A round picks `edits` slots — a fixed-length, slot-aligned
//! range of one file — from a small hot set of files (so clients collide) or from the whole tree (so most work is
//! disjoint), overwrites each with bytes that name the client and the round, and submits. One hot slot is written
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
//! **The oracle (offline, after the run).** Every submit is recorded with its base, its send and reply instants,
//! its slots and its outcome. The accepted versions must be exactly `1..=V`, each once. An accepted submit at `v`
//! must have no slot that an accepted version in `(base, v)` touched while the green's bytes at `v - 1` differ from
//! the slot's (the verdict's rule). A conflict's windows must equal the rule's expected set at some head the verdict
//! could have seen: between the newest version whose reply came back before the submit was sent and the newest
//! version whose submit was sent before this reply came back. And the green's head bytes of every file the run
//! touched must equal the version-ordered replay of the accepted submits over the files' version-0 bytes.
//!
//! Rows: `latency\t<n>\t<kind>\t<count>\t<p50>\t<p99>\t<p999>\t<max>` in nanoseconds, then `oracle\t<n>\t…` lines;
//! a nonzero `oracle_failures` exits 1.
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

/// One slot an edit overwrites.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Slot {
  path: String,
  at: u64,
  bytes: Vec<u8>,
}

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
}

/// What a submit came back with.
#[derive(Clone, Debug)]
enum Outcome {
  Accepted(u64),
  Conflict(Vec<(String, u64, u64)>),
}

/// The conflict classes a conflict reply named, kept beside the windows for the event dump.
type Classes = Vec<u8>;

/// One submit, as the oracle judges it.
#[derive(Clone, Debug)]
struct SubmitEvent {
  base: u64,
  sent_ns: u64,
  replied_ns: u64,
  slots: Vec<Slot>,
  outcome: Outcome,
  agent: usize,
  classes: Classes,
}

/// What a client is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
  CreateWork,
  Edit(usize),
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

/// One client and its script.
struct Agent {
  /// A daemon restart this client is riding out, if any.
  recovery: Option<Recovery>,
  /// The green's version a resolution reads its windows at, and the window bytes it read.
  resolving: (u64, Vec<u8>),
  id: usize,
  client: Client,
  green: VolumeId,
  work: Option<VolumeId>,
  base: u64,
  rounds: Vec<Vec<Slot>>,
  round: usize,
  step: Step,
  windows: Vec<MergeWindow>,
  resolutions: usize,
  inflight: Option<(u64, u64, Kind)>,
}

/// What a worker brings back.
#[derive(Default)]
struct Tally {
  latencies: BTreeMap<Kind, Vec<u64>>,
  submits: Vec<SubmitEvent>,
  refusals: BTreeMap<String, u64>,
  starved: u64,
  rebase_conflicts: u64,
  max_sweep_ns: u64,
  /// Reconnects after the daemon was found gone, requests resent under their own ids, and the longest outage a client
  /// rode out: the non-vacuity counters of the restart path (a chaos run that resent nothing tested nothing).
  reconnects: u64,
  resent: u64,
  max_outage_ns: u64,
}

/// Where a client is in riding out a daemon restart: reconnecting under its id, or resending its request in flight
/// under that request's own id once reconnected (the daemon answers from its completion record if it had served it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Recovery {
  Reconnecting { since_ns: u64 },
  Resending { since_ns: u64 },
}

/// splitmix64: a reproducible plan without a dependency.
struct Rng(u64);

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
}

fn ns_since(epoch: Instant) -> u64 {
  u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)
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

/// Every file of the green's version 0, with its length, walked through the daemon (`ReadDir`).
fn walk(client: &mut Client, green: VolumeId) -> Vec<(String, u64)> {
  let mut files = Vec::new();
  let mut dirs = vec![String::new()];
  while let Some(dir) = dirs.pop() {
    for entry in client
      .list_dir(green, &dir, ReadAt::Version { version: 0 })
      .unwrap()
    {
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

/// One round's slots for one client: `EDITS_PER_ROUND` distinct `(file, slot)` pairs.
fn plan_round(
  rng: &mut Rng,
  files: &[(String, u64)],
  hot: &[usize],
  agent: usize,
  round: usize,
) -> Vec<Slot> {
  let mut picked: BTreeSet<(usize, u64)> = BTreeSet::new();
  while picked.len() < EDITS_PER_ROUND {
    let file = if rng.below(PERMILLE) < HOT_PERMILLE {
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

  /// Sends the current step's request.
  /// No reply yet to `word`: past the client's own derived reply deadline, a gone daemon starts the restart path; a
  /// reply the daemon defers past one deadline while it lives (`Client::defers`) is waited for, as the synchronous
  /// call waits; anything else past `reply_patience` deadlines is counted lost and named, never waited on for ever.
  fn overdue(
    &mut self,
    word: u64,
    (sent_ns, kind): (u64, Kind),
    epoch: Instant,
    tally: &mut Tally,
  ) {
    let waited = ns_since(epoch).saturating_sub(sent_ns);
    let deadline = self.client.deadlines().reply_ns;
    if waited <= deadline {
      return;
    }
    if self.client.daemon_gone() {
      return self.lost_daemon(epoch);
    }
    if !self.client.defers(word) && waited > deadline.saturating_mul(reply_patience()) {
      self.give_up(
        tally,
        &format!("no reply to {kind:?} within the reply deadline (daemon gone: false)"),
      );
    }
  }

  fn lost_daemon(&mut self, epoch: Instant) {
    self.recovery = Some(Recovery::Reconnecting {
      since_ns: ns_since(epoch),
    });
  }

  /// One step of riding out a daemon restart, never blocking the sweep: a reconnect step under the client's id, then
  /// the request in flight resent under its own id. Bounded by the client's own derived reconnect budget.
  fn recover(&mut self, recovery: Recovery, epoch: Instant, tally: &mut Tally) {
    let (Recovery::Reconnecting { since_ns } | Recovery::Resending { since_ns }) = recovery;
    let outage = ns_since(epoch).saturating_sub(since_ns);
    if outage > self.client.deadlines().reconnect_ns {
      return self.give_up(
        tally,
        "the daemon did not come back within the reconnect budget",
      );
    }
    match recovery {
      Recovery::Reconnecting { .. } => match self.client.try_reconnect() {
        Ok(true) => {
          tally.reconnects += 1;
          self.recovery = Some(Recovery::Resending { since_ns });
        }
        Ok(false) => {}
        Err(e) => self.give_up(tally, &format!("reconnect: {e}")),
      },
      Recovery::Resending { .. } => {
        let Some((word, _, _)) = self.inflight else {
          self.recovery = None;
          return;
        };
        match self.client.resend_awaited(word) {
          Ok(true) => {
            tally.resent += 1;
            tally.max_outage_ns = tally.max_outage_ns.max(outage);
            self.recovery = None;
          }
          Ok(false) => {}
          Err(_) if self.client.daemon_gone() => {
            self.recovery = Some(Recovery::Reconnecting { since_ns })
          }
          Err(e) => self.give_up(tally, &format!("resend: {e}")),
        }
      }
    }
  }

  fn send(&mut self, epoch: Instant, tally: &mut Tally) {
    let Some((body, kind)) = self.request() else {
      return;
    };
    match self.client.begin(&body) {
      Ok(id) => self.inflight = Some((id.word(), ns_since(epoch), kind)),
      Err(
        ClientError::Ipc(slates_ipc::IpcError::RingFull) | ClientError::TooManyOutstanding { .. },
      ) => {}
      Err(_) if self.client.daemon_gone() => self.lost_daemon(epoch),
      Err(e) => self.give_up(tally, &format!("send {kind:?}: {e}")),
    }
  }

  fn give_up(&mut self, tally: &mut Tally, why: &str) {
    *tally.refusals.entry(why.to_owned()).or_default() += 1;
    self.step = Step::Done;
    self.inflight = None;
  }

  /// Takes a reply if one is ready, records it, and moves the script on.
  fn poll(&mut self, epoch: Instant, tally: &mut Tally) {
    if let Some(recovery) = self.recovery {
      return self.recover(recovery, epoch, tally);
    }
    let Some((word, sent_ns, kind)) = self.inflight else {
      self.send(epoch, tally);
      return;
    };
    let reply = match self.client.poll_reply_word(word) {
      Ok(None) => return self.overdue(word, (sent_ns, kind), epoch, tally),
      Ok(Some(reply)) => reply,
      Err(_) if self.client.daemon_gone() => return self.lost_daemon(epoch),
      Err(e) => return self.give_up(tally, &format!("poll {kind:?}: {e}")),
    };
    let replied_ns = ns_since(epoch);
    self.inflight = None;
    // The daemon keeps every reply's completion record until the client acknowledges it, and refuses a client that
    // owes too many (`AcknowledgementOwed`): an async caller acknowledges as it goes, unawaited.
    if let Err(e) = self.client.begin_ack_if_due() {
      return self.give_up(tally, &format!("acknowledge: {e}"));
    }
    self.advance(reply, (sent_ns, replied_ns, kind), tally);
  }

  fn advance(
    &mut self,
    reply: ReplyBody,
    (sent_ns, replied_ns, kind): (u64, u64, Kind),
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
        self.step = if n + 1 < self.slots().len() {
          Step::Edit(n + 1)
        } else {
          Step::Submit
        };
        record(tally, kind, elapsed);
      }
      (Step::Submit, ReplyBody::Submitted { version, conflicts }) => {
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
      (Step::Rebase, ReplyBody::Rebased { version, conflicts }) => {
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
    let name = format!("{kind:?}: {refusal:?}");
    let short = name
      .split([' ', '{', '('])
      .take(2)
      .collect::<Vec<_>>()
      .join(" ");
    self.give_up(tally, &short);
  }

  fn submitted(
    &mut self,
    version: Option<u64>,
    conflicts: Vec<MergeWindow>,
    at: (u64, u64),
    tally: &mut Tally,
  ) {
    let elapsed = at.1.saturating_sub(at.0);
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
      classes: conflicts.iter().map(|w| w.class).collect(),
    });
    match version {
      Some(v) => {
        record(tally, Kind::SubmitAccepted, elapsed);
        self.base = v;
        self.resolutions = 0;
        self.round += 1;
        self.step = if self.round < self.rounds.len() {
          Step::Edit(0)
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

/// How many of the client's reply deadlines a request may wait past its send before the harness counts its reply lost:
/// one, unless `--reply-patience <n>` asks more (to tell a late reply from one never sent).
fn reply_patience() -> u64 {
  arg("--reply-patience").map_or(1, |n| n.parse().expect("--reply-patience n"))
}

/// Shape: the unfinished clients a progress report names, each with its step and the request it waits on.
const PROGRESS_NAMED: usize = 4;
/// Shape: how often a worker prints its progress, so a long or stuck level shows where it is.
const PROGRESS_EVERY: Duration = Duration::from_secs(5);

/// One worker: its agents, each with one request in flight, swept until every one is done. Every `PROGRESS_EVERY` it
/// prints how many of its agents are done and the submits it has seen.
fn drive(mut agents: Vec<Agent>, epoch: Instant, worker: usize) -> Tally {
  let mut tally = Tally::default();
  let mut reported = Instant::now();
  while agents.iter().any(|agent| agent.step != Step::Done) {
    let sweep = Instant::now();
    for agent in agents.iter_mut().filter(|agent| agent.step != Step::Done) {
      agent.poll(epoch, &mut tally);
    }
    tally.max_sweep_ns = tally
      .max_sweep_ns
      .max(u64::try_from(sweep.elapsed().as_nanos()).unwrap_or(u64::MAX));
    if reported.elapsed() >= PROGRESS_EVERY {
      reported = Instant::now();
      let done = agents
        .iter()
        .filter(|agent| agent.step == Step::Done)
        .count();
      for agent in agents
        .iter()
        .filter(|agent| agent.step != Step::Done)
        .take(PROGRESS_NAMED)
      {
        let age = agent
          .inflight
          .map(|(_, sent, kind)| (kind, ns_since(epoch).saturating_sub(sent)));
        println!(
          "pending\t{worker}\tagent\t{}\tstep\t{:?}\tround\t{}\tinflight\t{age:?}\toutstanding\t{}",
          agent.id,
          agent.step,
          agent.round,
          agent.client.outstanding().len()
        );
      }
      println!(
        "progress\t{worker}\t{}\tdone\t{done}\tof\t{}\tsubmits\t{}",
        epoch.elapsed().as_secs(),
        agents.len(),
        tally.submits.len()
      );
    }
  }
  tally
}

/// The kernel tree as a green: an overlay over the host directory, pinned whole, snapshotted, and the green
/// created over that snapshot (the A-9 complete immutable base). Each step's time is printed.
fn seed_green(client: &mut Client, tree: &str, tag: &str) -> VolumeId {
  let step = Instant::now();
  let overlay = client
    .create(&CreateSpec {
      name: format!("{tag}-tree"),
      size: SizeClass::Dynamic { max: u64::MAX >> 1 },
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

fn report(n: usize, tally: &mut Tally) {
  for (kind, samples) in &mut tally.latencies {
    samples.sort_unstable();
    let cuts: Vec<String> = PERCENTILES
      .iter()
      .map(|p| percentile(samples, *p).to_string())
      .collect();
    println!(
      "latency\t{n}\t{kind:?}\t{}\t{}\t{}",
      samples.len(),
      cuts.join("\t"),
      samples.last().copied().unwrap_or(0)
    );
  }
  for (why, count) in &tally.refusals {
    println!("refusal\t{n}\t{count}\t{why}");
  }
  println!(
    "harness\t{n}\tstarved\t{}\trebase_conflicts\t{}\tmax_sweep_ns\t{}\treconnects\t{}\tresent\t{}\tmax_outage_ns\t{}",
    tally.starved,
    tally.rebase_conflicts,
    tally.max_sweep_ns,
    tally.reconnects,
    tally.resent,
    tally.max_outage_ns
  );
}

/// The green's bytes of each touched file at `version`, from the version-0 bytes and the accepted submits in order.
fn replay(
  base: &BTreeMap<String, Vec<u8>>,
  accepted: &[&SubmitEvent],
  through: u64,
) -> BTreeMap<String, Vec<u8>> {
  let mut state = base.clone();
  for event in accepted.iter().take(usize::try_from(through).unwrap()) {
    for slot in &event.slots {
      let file = state.get_mut(&slot.path).unwrap();
      let at = usize::try_from(slot.at).unwrap();
      file[at..at + slot.bytes.len()].copy_from_slice(&slot.bytes);
    }
  }
  state
}

/// The windows the verdict's rule expects for `event` judged against head `head`: each slot that a version in
/// `(base, head]` touched, where the green's bytes at `head` differ from the slot's.
fn expected_windows(
  event: &SubmitEvent,
  accepted: &[&SubmitEvent],
  states: &[BTreeMap<String, Vec<u8>>],
  head: u64,
) -> BTreeSet<(String, u64, u64)> {
  let touched: BTreeSet<(&str, u64)> = accepted
    .iter()
    .enumerate()
    .filter(|(index, _)| (*index as u64) + 1 > event.base && (*index as u64) < head)
    .flat_map(|(_, other)| other.slots.iter().map(|slot| (slot.path.as_str(), slot.at)))
    .collect();
  let at_head = &states[usize::try_from(head).unwrap()];
  event
    .slots
    .iter()
    .filter(|slot| touched.contains(&(slot.path.as_str(), slot.at)))
    .filter(|slot| {
      let at = usize::try_from(slot.at).unwrap();
      at_head[&slot.path][at..at + slot.bytes.len()] != slot.bytes[..]
    })
    .map(|slot| (slot.path.clone(), slot.at, SLOT_BYTES))
    .collect()
}

/// Why the oracle did not expect a window at `(path, at)`: the versions after the base that touched it, and the head's
/// bytes there against the submit's.
fn explain(
  event: &SubmitEvent,
  accepted: &[&SubmitEvent],
  at_head: &BTreeMap<String, Vec<u8>>,
  path: &str,
  at: u64,
) -> String {
  let touched: Vec<u64> = accepted
    .iter()
    .enumerate()
    .filter(|(_, other)| {
      other
        .slots
        .iter()
        .any(|slot| slot.path == path && slot.at == at)
    })
    .map(|(index, _)| index as u64 + 1)
    .collect();
  let mine = event
    .slots
    .iter()
    .find(|slot| slot.path == path && slot.at == at);
  let start = usize::try_from(at).unwrap();
  let head = at_head
    .get(path)
    .and_then(|bytes| bytes.get(start..start + 16));
  format!(
    "{path}@{at}: touched by versions {touched:?}; head {:?}; mine {:?}",
    head.map(String::from_utf8_lossy),
    mine.map(|slot| String::from_utf8_lossy(&slot.bytes[..16]).into_owned())
  )
}

/// Judges every submit and the final bytes; returns the failures, each printed.
fn oracle(
  n: usize,
  submits: &[SubmitEvent],
  base: &BTreeMap<String, Vec<u8>>,
  head: &BTreeMap<String, Vec<u8>>,
) -> u64 {
  let mut failures = 0u64;
  let mut fail = |what: String| {
    failures += 1;
    if failures <= 20 {
      println!("oracle_failure\t{n}\t{what}");
    }
  };
  let mut accepted: Vec<&SubmitEvent> = submits
    .iter()
    .filter(|e| matches!(e.outcome, Outcome::Accepted(_)))
    .collect();
  accepted.sort_by_key(|e| match e.outcome {
    Outcome::Accepted(v) => v,
    Outcome::Conflict(_) => 0,
  });
  for (index, event) in accepted.iter().enumerate() {
    if let Outcome::Accepted(v) = event.outcome
      && v != index as u64 + 1
    {
      fail(format!(
        "accepted versions are not 1..=V: position {index} holds {v}"
      ));
    }
  }
  let states: Vec<BTreeMap<String, Vec<u8>>> = (0..=accepted.len() as u64)
    .map(|v| replay(base, &accepted, v))
    .collect();
  for event in submits {
    match &event.outcome {
      Outcome::Accepted(v) => {
        let expected = expected_windows(event, &accepted, &states, v - 1);
        if !expected.is_empty() {
          fail(format!(
            "version {v} (base {}) was accepted over conflicts {expected:?}",
            event.base
          ));
        }
      }
      Outcome::Conflict(windows) => {
        let reported: BTreeSet<(String, u64, u64)> = windows.iter().cloned().collect();
        let low = accepted
          .iter()
          .filter(|e| e.replied_ns < event.sent_ns)
          .count() as u64;
        let high = accepted
          .iter()
          .filter(|e| e.sent_ns < event.replied_ns)
          .count() as u64;
        let fits = (low.max(event.base)..=high)
          .any(|h| expected_windows(event, &accepted, &states, h) == reported);
        if !fits {
          let top = expected_windows(event, &accepted, &states, high);
          let why: Vec<String> = reported
            .difference(&top)
            .map(|(path, at, _)| {
              explain(
                event,
                &accepted,
                &states[usize::try_from(high).unwrap()],
                path,
                *at,
              )
            })
            .collect();
          fail(format!(
            "conflict (agent {}, base {}) reported {reported:?}; no head in {low}..={high} expects that; at {high} \
             expected {top:?}; unexpected: {why:?}",
            event.agent, event.base
          ));
        }
      }
    }
  }
  let replayed = &states[accepted.len()];
  for (path, bytes) in replayed {
    if head.get(path) != Some(bytes) {
      fail(format!(
        "the green's head of {path} differs from the replay of the accepted submits"
      ));
    }
  }
  println!(
    "oracle\t{n}\taccepted\t{}\tconflicts\t{}\tfiles_checked\t{}\toracle_failures\t{failures}",
    accepted.len(),
    submits.len() - accepted.len(),
    replayed.len()
  );
  failures
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

/// A file read that a busy daemon may stall past one reply deadline: tried again up to `reply_patience` times (the
/// oracle's reads come after every client is done, so a stall here is the daemon draining, never a lost answer).
fn read_patiently(client: &mut Client, green: VolumeId, path: &str, at: ReadAt) -> Vec<u8> {
  let mut tries = 0;
  loop {
    match client.read(green, path, at) {
      Err(ClientError::Stalled { .. }) if tries < reply_patience() => tries += 1,
      other => return other.unwrap(),
    }
  }
}

/// One load level: `n` clients over the green, `workers` threads sweeping them. Returns the oracle's failures.
fn level(
  instance: &str,
  green: VolumeId,
  files: &[(String, u64)],
  (n, rounds, hot_files): (usize, usize, usize),
  workers: usize,
) -> u64 {
  let eligible: Vec<(String, u64)> = files
    .iter()
    .filter(|(_, len)| *len >= SLOTS_PER_FILE * SLOT_STRIDE)
    .cloned()
    .collect();
  let mut rng = Rng(SEED ^ n as u64);
  let hot: Vec<usize> = (0..hot_files)
    .map(|_| usize::try_from(rng.below(eligible.len() as u64)).unwrap())
    .collect();
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
  let mut reader = connect(instance).unwrap();
  let head_before = reader.versions(green).unwrap();
  let base = read_files(&mut reader, green, &paths, ReadAt::Head);
  let connect_started = Instant::now();
  let mut agents: Vec<Agent> = plans
    .into_iter()
    .enumerate()
    .map(|(id, rounds)| Agent {
      id: id + n * 1_000_000,
      client: connect(instance).unwrap(),
      green,
      work: None,
      base: 0,
      rounds,
      round: 0,
      step: Step::CreateWork,
      windows: Vec::new(),
      resolving: (0, Vec::new()),
      recovery: None,
      resolutions: 0,
      inflight: None,
    })
    .collect();
  println!(
    "level\t{n}\tconnect_all_ns\t{}",
    connect_started.elapsed().as_nanos()
  );
  let epoch = Instant::now();
  let mut groups: Vec<Vec<Agent>> = (0..workers).map(|_| Vec::new()).collect();
  for (index, agent) in agents.drain(..).enumerate() {
    groups[index % workers].push(agent);
  }
  let tallies: Vec<Tally> = std::thread::scope(|scope| {
    let handles: Vec<_> = groups
      .into_iter()
      .enumerate()
      .map(|(worker, group)| scope.spawn(move || drive(group, epoch, worker)))
      .collect();
    handles
      .into_iter()
      .map(|handle| handle.join().unwrap())
      .collect()
  });
  let wall = epoch.elapsed();
  let mut total = Tally::default();
  for mut tally in tallies {
    for (kind, mut samples) in std::mem::take(&mut tally.latencies) {
      total
        .latencies
        .entry(kind)
        .or_default()
        .append(&mut samples);
    }
    total.submits.append(&mut tally.submits);
    for (why, count) in tally.refusals {
      *total.refusals.entry(why).or_default() += count;
    }
    total.starved += tally.starved;
    total.rebase_conflicts += tally.rebase_conflicts;
    total.max_sweep_ns = total.max_sweep_ns.max(tally.max_sweep_ns);
    total.reconnects += tally.reconnects;
    total.resent += tally.resent;
    total.max_outage_ns = total.max_outage_ns.max(tally.max_outage_ns);
  }
  println!(
    "level\t{n}\twall_ns\t{}\tworkers\t{workers}",
    wall.as_nanos()
  );
  report(n, &mut total);
  // Version numbers in the events are absolute; rebase them to this level's first version for the oracle.
  for event in &mut total.submits {
    event.base = event.base.saturating_sub(head_before);
    if let Outcome::Accepted(v) = &mut event.outcome {
      *v -= head_before;
    }
  }
  let head = read_files(&mut reader, green, &paths, ReadAt::Head);
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
  oracle(n, &total.submits, &base, &head)
}

fn arg(name: &str) -> Option<String> {
  let mut args = std::env::args();
  args.find(|a| a == name).and_then(|_| args.next())
}

fn main() {
  let tree = arg("--tree").expect("--tree <the source tree the green is seeded from>");
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
  // `--instance <name>` drives a daemon already running under its own anchor (`slates --instance <name> anchor`), so a
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
  let mut setup = connect(&instance).unwrap();
  let green = seed_green(&mut setup, &tree, "load");
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
  drop(setup);
  if let Some(daemon) = daemon {
    daemon.stop();
  }
  if failures > 0 {
    std::process::exit(1);
  }
}
