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
  /// The client gave up with the submit in flight (the daemon gone past its reconnect budget, a reply lost): the
  /// green may or may not have taken it, which the oracle settles from the green itself ([`resolve_unknown`]).
  Unknown,
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
  round: usize,
  classes: Classes,
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
  /// The pause before this client sends again after a refusal for now (`LeaseUnconfirmed`, `AcknowledgementOwed`, a
  /// refusal retryable under its id), doubling while they repeat (`Client::retry_pause_ns`), zero once a reply moves
  /// it on. Resent at once, 1,000 clients refused `LeaseUnconfirmed` after a primary's kill sent 20,000 a second.
  pause_ns: u64,
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
  /// Requests the daemon refused retryable under their own id and the client sent again under it.
  retried: u64,
  /// Requests the daemon refused `LeaseUnconfirmed` (not run) and the client sent again as new.
  unconfirmed: u64,
  /// Requests the daemon refused `AcknowledgementOwed` and the client sent again: the multiplexed connection's
  /// backpressure while an older request of the connection is still awaited (its acknowledgement cannot pass it).
  backpressure: u64,
  /// The longest wait between two drains of one connection's completion ring: the resolution of every latency here,
  /// since a reply's arrival is stamped when its ring is drained.
  max_drain_gap_ns: u64,
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

/// The first of a comma-separated `--instance` list: the node the harness's own setup and oracle reads go through.
fn first_instance(instances: &str) -> &str {
  instances.split(',').next().unwrap_or(instances)
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
          Step::Append
        };
        record(tally, kind, elapsed);
      }
      (Step::Append, ReplyBody::Edited) => {
        self.step = Step::Submit;
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

/// How many of the client's reply deadlines a request may wait past its send before the harness counts its reply lost:
/// one, unless `--reply-patience <n>` asks more (to tell a late reply from one never sent).
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

/// The pause each client takes after a reply before its next request (`--think-us <µs>`, default none: a closed loop
/// with no think time, every client always waiting on a request). An agent between its requests thinks, reads its
/// tree or runs a build: the offered load the latency bound is stated for. A latency never includes the pause.
fn think_ns() -> u64 {
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
  /// When this connection's ring was last drained.
  drained_ns: u64,
}

impl Connection {
  fn new(client: Client, epoch: Instant) -> Connection {
    Connection {
      client,
      inflight: BTreeMap::new(),
      reconnecting: None,
      resend: Vec::new(),
      resend_since: 0,
      drained_ns: ns_since(epoch),
    }
  }

  fn lost(&mut self, epoch: Instant) {
    if self.reconnecting.is_none() {
      let now = ns_since(epoch);
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
  /// The pause a client takes after each reply before its next request (`--think-us`), nanoseconds.
  think_ns: u64,
  tally: Tally,
  epoch: Instant,
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
    if let Some((word, first_sent_ns, retried_kind)) = agent.retry {
      return match conn.client.begin_again(word, &body) {
        Ok(true) => {
          agent.retry = None;
          agent.inflight = Some((word, first_sent_ns, retried_kind));
          conn.inflight.insert(word, a);
          self.tally.retried += 1;
          true
        }
        Ok(false) | Err(ClientError::TooManyOutstanding { .. } | ClientError::Rebinding) => false,
        Err(_) if conn.client.daemon_gone() => {
          conn.lost(self.epoch);
          false
        }
        Err(e) => {
          agent.inflight = agent.retry.take();
          agent.give_up(&mut self.tally, &format!("retry {kind:?}: {e}"));
          true
        }
      };
    }
    match conn.client.begin(&body) {
      Ok(id) => {
        agent.inflight = Some((id.word(), ns_since(self.epoch), kind));
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
        conn.lost(self.epoch);
        false
      }
      Err(e) => {
        agent.give_up(&mut self.tally, &format!("send {kind:?}: {e}"));
        true
      }
    }
  }

  /// Sends for every ready client, in order, until one cannot go (each connection's ring and bound are its own, so
  /// a client that cannot go is kept and the rest are tried).
  fn send_ready(&mut self) {
    let now = ns_since(self.epoch);
    let mut kept = Vec::new();
    while let Some(std::cmp::Reverse((due, a))) = self.ready.peek().copied() {
      if due > now {
        break;
      }
      self.ready.pop();
      if self.agents[a].step == Step::Done || self.agents[a].inflight.is_some() {
        continue;
      }
      if !self.send(a) {
        kept.push(std::cmp::Reverse((now, a)));
      }
    }
    self.ready.extend(kept);
  }

  /// Drains connection `c`'s ring, stamping each reply's arrival, and moves each reply's client on.
  fn drain(&mut self, c: usize) {
    let now = ns_since(self.epoch);
    let conn = &mut self.conns[c];
    self.tally.max_drain_gap_ns = self
      .tally
      .max_drain_gap_ns
      .max(now.saturating_sub(conn.drained_ns));
    conn.drained_ns = now;
    let words = match conn.client.take_ready() {
      Ok(words) => words,
      Err(_) if conn.client.daemon_gone() => return conn.lost(self.epoch),
      Err(e) => return self.fail_connection(c, &format!("take_ready: {e}")),
    };
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
      let agent = &mut self.agents[a];
      let Some((_, sent_ns, kind)) = agent.inflight.take() else {
        continue;
      };
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
          continue;
        }
        agent.retry = Some((word, sent_ns, kind));
        agent.pause_ns = self.conns[agent.conn].client.retry_pause_ns(agent.pause_ns);
        self
          .ready
          .push(std::cmp::Reverse((now.saturating_add(agent.pause_ns), a)));
        continue;
      }
      let for_now = matches!(
        &reply,
        ReplyBody::Refused {
          refusal: Refusal::LeaseUnconfirmed { .. } | Refusal::AcknowledgementOwed { .. }
        }
      );
      agent.advance(reply, (sent_ns, now, kind), &mut self.tally);
      if agent.step != Step::Done {
        let wait = if for_now {
          agent.pause_ns = self.conns[agent.conn].client.retry_pause_ns(agent.pause_ns);
          agent.pause_ns
        } else {
          agent.pause_ns = 0;
          self.think_ns
        };
        self
          .ready
          .push(std::cmp::Reverse((now.saturating_add(wait), a)));
      }
    }
    // The daemon keeps every reply's completion record until the client acknowledges it, and refuses a client that
    // owes too many (`AcknowledgementOwed`): an async caller acknowledges as it goes, unawaited, never past a request
    // it still awaits.
    if answered && let Err(e) = self.conns[c].client.begin_ack_if_due() {
      self.fail_connection(c, &format!("acknowledge: {e}"));
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
    let now = ns_since(self.epoch);
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
          return conn.lost(self.epoch);
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
  /// path; a reply the daemon defers past one deadline while it lives (`Client::defers`) is waited for, as the
  /// synchronous call waits; anything else past `reply_patience` deadlines is counted lost and named, never waited on
  /// for ever.
  fn overdue(&mut self) {
    let now = ns_since(self.epoch);
    for c in 0..self.conns.len() {
      let conn = &mut self.conns[c];
      if conn.reconnecting.is_some() || !conn.resend.is_empty() {
        continue;
      }
      let deadline = conn.client.deadlines().reply_ns;
      let mut lost = Vec::new();
      let mut gone = false;
      for (word, a) in &conn.inflight {
        let Some((_, sent_ns, kind)) = self.agents[*a].inflight else {
          continue;
        };
        let waited = now.saturating_sub(sent_ns);
        if waited <= deadline {
          continue;
        }
        if conn.client.daemon_gone() {
          gone = true;
          break;
        }
        if !conn.client.defers(*word) && waited > deadline.saturating_mul(reply_patience()) {
          lost.push((*word, *a, kind));
        }
      }
      if gone {
        conn.lost(self.epoch);
        continue;
      }
      for (word, a, kind) in lost {
        self.conns[c].inflight.remove(&word);
        self.conns[c].client.abandon(word);
        self.agents[a].give_up(
          &mut self.tally,
          &format!("no reply to {kind:?} within the reply deadline (daemon gone: false)"),
        );
      }
    }
  }

  fn progress(&self, worker: usize) {
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
        .map(|(_, sent, kind)| (kind, ns_since(self.epoch).saturating_sub(sent)));
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
      self.epoch.elapsed().as_secs(),
      self.agents.len(),
      self.tally.submits.len()
    );
  }
}

/// One worker: its connections and their logical clients, each with one request in flight, swept until every client
/// is done. A sweep drains every connection's ring first (stamping each reply's arrival, so a latency here is the
/// daemon's and the ring's, not the time the harness took to reach the client), moves each answered client on and
/// sends its next request at once, then sends for clients still waiting for room. Every `PROGRESS_EVERY` it prints how
/// many of its clients are done and the submits it has seen.
fn drive(conns: Vec<Connection>, agents: Vec<Agent>, epoch: Instant, worker: usize) -> Tally {
  let ready = (0..agents.len())
    .map(|a| std::cmp::Reverse((0, a)))
    .collect();
  let check_every = conns.first().map_or(1, |conn| {
    (conn.client.deadlines().reply_ns / OVERDUE_CHECKS_PER_DEADLINE).max(1)
  });
  let mut state = Worker {
    conns,
    agents,
    ready,
    think_ns: think_ns(),
    tally: Tally::default(),
    epoch,
  };
  let mut reported = Instant::now();
  let mut checked = ns_since(epoch);
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
    if ns_since(epoch).saturating_sub(checked) >= check_every {
      checked = ns_since(epoch);
      state.overdue();
    }
    if reported.elapsed() >= PROGRESS_EVERY {
      reported = Instant::now();
      state.progress(worker);
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

/// Each slot's writes by the accepted versions, in version order: what the green held at a slot at any version is the
/// last write at or before it, else the file's version-0 bytes. Built once, linear in the accepted slots, so judging a
/// verdict at a head is a lookup per slot. (The oracle had replayed a whole copy of every touched file for every
/// version: 1,600 versions × 677 files at 1,000 clients, minutes of the harness's own CPU after the clients were done,
/// which read as a hung daemon.)
type SlotWrites<'a> = BTreeMap<(&'a str, u64), Vec<(u64, &'a [u8])>>;

fn slot_writes<'a>(accepted: &[&'a SubmitEvent]) -> SlotWrites<'a> {
  let mut writes: SlotWrites<'a> = BTreeMap::new();
  for (index, event) in accepted.iter().enumerate() {
    let version = index as u64 + 1;
    for slot in &event.slots {
      writes
        .entry((slot.path.as_str(), slot.at))
        .or_default()
        .push((version, slot.bytes.as_slice()));
    }
  }
  writes
}

/// The green's bytes at slot `(path, at)` of `len` bytes as of `version`.
fn slot_at<'a>(
  writes: &SlotWrites<'a>,
  base: &'a BTreeMap<String, Vec<u8>>,
  (path, at, len): (&str, u64, usize),
  version: u64,
) -> &'a [u8] {
  let written = writes.get(&(path, at)).and_then(|history| {
    history
      .iter()
      .rev()
      .find(|(written, _)| *written <= version)
      .map(|(_, bytes)| *bytes)
  });
  written.unwrap_or_else(|| {
    let start = usize::try_from(at).unwrap();
    &base[path][start..start + len]
  })
}

/// The green's bytes of every touched file after the last accepted version.
fn replay(base: &BTreeMap<String, Vec<u8>>, writes: &SlotWrites<'_>) -> BTreeMap<String, Vec<u8>> {
  let mut state = base.clone();
  for ((path, at), history) in writes {
    if let Some((_, bytes)) = history.last() {
      let start = usize::try_from(*at).unwrap();
      state.get_mut(*path).unwrap()[start..start + bytes.len()].copy_from_slice(bytes);
    }
  }
  state
}

/// The windows the verdict's rule expects for `event` judged against head `head`: each slot that a version in
/// `(base, head]` touched, where the green's bytes at `head` differ from the slot's.
fn expected_windows(
  event: &SubmitEvent,
  (writes, base): (&SlotWrites<'_>, &BTreeMap<String, Vec<u8>>),
  head: u64,
) -> BTreeSet<(String, u64, u64)> {
  event
    .slots
    .iter()
    .filter(|slot| {
      writes
        .get(&(slot.path.as_str(), slot.at))
        .is_some_and(|history| history.iter().any(|(v, _)| *v > event.base && *v <= head))
    })
    .filter(|slot| {
      slot_at(writes, base, (&slot.path, slot.at, slot.bytes.len()), head) != slot.bytes.as_slice()
    })
    .map(|slot| (slot.path.clone(), slot.at, SLOT_BYTES))
    .collect()
}

/// Why the oracle did not expect a window at `(path, at)`: the versions that wrote it, and the head's bytes there
/// against the submit's.
fn explain(
  event: &SubmitEvent,
  (writes, base): (&SlotWrites<'_>, &BTreeMap<String, Vec<u8>>),
  (path, at): (&str, u64),
  head: u64,
) -> String {
  let touched: Vec<u64> = writes
    .get(&(path, at))
    .map(|history| history.iter().map(|(v, _)| *v).collect())
    .unwrap_or_default();
  let mine = event
    .slots
    .iter()
    .find(|slot| slot.path == path && slot.at == at);
  let held = slot_at(writes, base, (path, at, 16), head);
  format!(
    "{path}@{at}: touched by versions {touched:?}; head {:?}; mine {:?}",
    String::from_utf8_lossy(held),
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
    Outcome::Conflict(_) | Outcome::Unknown => 0,
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
  let writes = slot_writes(&accepted);
  let judged = (&writes, base);
  for event in submits {
    match &event.outcome {
      Outcome::Accepted(v) => {
        let expected = expected_windows(event, judged, v - 1);
        if !expected.is_empty() {
          fail(format!(
            "version {v} (base {}) was accepted over conflicts {expected:?}",
            event.base
          ));
        }
      }
      // An outcome still unknown after `resolve_unknown` was not taken by the green: it judges nothing.
      Outcome::Unknown => {}
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
        let fits =
          (low.max(event.base)..=high).any(|h| expected_windows(event, judged, h) == reported);
        if !fits {
          let top = expected_windows(event, judged, high);
          let why: Vec<String> = reported
            .difference(&top)
            .map(|(path, at, _)| explain(event, judged, (path, *at), high))
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
  let replayed = &replay(base, &writes);
  for (path, bytes) in replayed {
    if head.get(path) != Some(bytes) {
      fail(format!(
        "the green's head of {path} differs from the replay of the accepted submits"
      ));
    }
  }
  println!(
    "oracle\t{n}\taccepted\t{}\tconflicts\t{}\tunknown\t{}\tfiles_checked\t{}\toracle_failures\t{failures}",
    accepted.len(),
    submits
      .iter()
      .filter(|e| matches!(e.outcome, Outcome::Conflict(_)))
      .count(),
    submits
      .iter()
      .filter(|e| matches!(e.outcome, Outcome::Unknown))
      .count(),
    replayed.len()
  );
  failures
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

/// A file read that a busy daemon may stall past one reply deadline, or refuse for now: tried again (`patiently`).
fn read_patiently(client: &mut Client, green: VolumeId, path: &str, at: ReadAt) -> Vec<u8> {
  patiently(client, &format!("read {path}"), |client| {
    client.read(green, path, at)
  })
}

/// The green's version count, asked patiently.
fn versions_patiently(client: &mut Client, green: VolumeId) -> u64 {
  patiently(client, "versions", |client| client.versions(green))
}

/// An oracle call tried again while its answer is only late or refused for now: a stall past one reply deadline (the
/// daemon draining after every client is done), `LeaseUnconfirmed` (an owner confirming its lease after a node's
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
        ClientError::Stalled { .. }
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
      Err(e) => panic!("oracle {what}: {e} after {:?}", started.elapsed()),
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
      pause_ns: 0,
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
  let epoch = Instant::now();
  let tallies: Vec<Tally> = std::thread::scope(|scope| {
    let handles: Vec<_> = groups
      .into_iter()
      .enumerate()
      .map(|(worker, (clients, agents))| {
        scope.spawn(move || {
          let conns = clients
            .into_iter()
            .map(|client| Connection::new(client, epoch))
            .collect();
          drive(conns, agents, epoch, worker)
        })
      })
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
    total.backpressure += tally.backpressure;
    total.retried += tally.retried;
    total.unconfirmed += tally.unconfirmed;
    total.max_drain_gap_ns = total.max_drain_gap_ns.max(tally.max_drain_gap_ns);
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
  // The oracle reads through a connection made now: the one made before the load may belong to a daemon a kill rung
  // has since restarted (its `versions` answered `DaemonGone` after the primary's kill, 2026-11-07).
  let mut reader = connect(first_instance(instance)).unwrap();
  let unexplained = resolve_unknown(&mut reader, green, head_before, &mut total.submits);
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
  for version in &unexplained {
    println!(
      "oracle_failure\t{n}\tversion {version} is on the green but no submit of this level, known or unknown, explains it"
    );
  }
  u64::try_from(unexplained.len()).unwrap_or(u64::MAX)
    + oracle(n, &total.submits, &base, &head)
    + own_files_oracle(n, &mut reader, green, &total.submits)
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
  drop(setup);
  if let Some(daemon) = daemon {
    daemon.stop();
  }
  if failures > 0 {
    std::process::exit(1);
  }
}
