//! T-6.7 in its shuttle form (§4.16, D-27; Part 6 "Concurrency", nightly): sixteen simulated agents
//! submit increments with random overlaps to one green through its owner, the way agents reach a
//! green's merge task (one owner per green, D-7; a submission is a move over a bounded channel, the
//! reply a move back). shuttle draws the thread schedule and the agents' randomness, so every
//! schedule is replayable from its seed. The green's commit order must be a linearization of the
//! agents' submissions: replaying the log in that order on a fresh green reproduces every outcome;
//! every conflict the block oracle predicts is reported and no other; no accepted operation is lost
//! (the final bytes equal the oracle's); and the fast-path counter equals the oracle's count of
//! disjoint submissions — the design's rule, `last_changed[path] <= base`, stated per file.
//!
//! The oracle is the block reference `tests/engine.rs` decides its generated histories against
//! (`tests/common/mod.rs`): every edit is a length-preserving overwrite of a whole block, so the
//! verdict is per-block identity and the reference never reasons about coordinates.

#![cfg(shuttle)]
// Test harness code: an unwrap or a panic here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::atomic::{AtomicU64, Ordering};

use common::{BLOCKS, Build, base_file, block_edits_on, blocks_to_bytes};
use shuttle::rand::{Rng, thread_rng};
use shuttle::scheduler::RandomScheduler;
use shuttle::sync::mpsc::{Receiver, SyncSender, sync_channel};
use shuttle::{Config, Runner};
use slates_merge::engine::{Green, Increment, Outcome};

/// Shape: agents — sixteen, the design's words (T-6.7; Part 6 example 11).
const AGENTS: usize = 16;
/// Shape: submissions per agent — three: the first meets a fresh base, the later ones a base that
/// other agents' commits have moved past, which is what the conflict, the identical accept and the
/// fast path each need to occur.
const ROUNDS: usize = 3;
/// Shape: files — three, "a set of files small enough to force overlaps" (Part 6 example 11) among
/// sixteen agents' edits.
const FILES: usize = 3;
/// Shape: the block tags an agent may write, `1..=TAGS` (the oracle's tags; 0 leaves a block).
const TAGS: u8 = 3;
/// Derived: the schedule budget — the case count of the serial tests' generated histories
/// (proptest's default, 256, rounded to the budget `tests/clones.rs` runs), which the measured mix
/// makes ample: 48 submissions per schedule, and over 200 schedules 2,606 accepts, 584 identical
/// accepts, 6,994 conflicts and 1,934 fast paths in 0.26 s (2026-09-13, `docs/wip/concurrency.md`).
const SCHEDULES: usize = 200;
/// Shape: the fixed seed of the schedule budget, so a run is reproducible; changed only with the
/// record.
const SEED: u64 = 0x5eed_6ee4;
/// Derived: the stack each simulated thread runs on, 2 MiB — what Rust gives a spawned test thread
/// (`RUST_MIN_STACK`'s default), so the owner runs the engine on the stack it has under `cargo test`.
const STACK_BYTES: usize = 2 << 20;
/// Shape: the request channel's bound — one outstanding request per agent, since an agent waits for
/// each reply before its next request (the client's request/reply discipline).
const REQUEST_BOUND: usize = AGENTS;
/// Shape: the setup increment's identity byte, outside the agents' range.
const SETUP_ID: u8 = 0xff;

/// Accepted submissions across every schedule (non-vacuity of the budget).
static ACCEPTS: AtomicU64 = AtomicU64::new(0);
/// Accepted submissions with an identical overlap across every schedule.
static IDENTICAL: AtomicU64 = AtomicU64::new(0);
/// Conflicts across every schedule.
static CONFLICTS: AtomicU64 = AtomicU64::new(0);
/// Fast-path submissions across every schedule.
static FAST_PATHS: AtomicU64 = AtomicU64::new(0);

/// A request to the green's owner, with the channel the reply goes back on.
enum Request {
  /// The head version, for an agent choosing its base.
  Head { reply: SyncSender<u64> },
  /// A submission; `file` and `edits` are the agent's declared intent for the oracle's log.
  Submit {
    agent: usize,
    file: usize,
    edits: [u8; BLOCKS],
    increment: Increment,
    reply: SyncSender<Outcome>,
  },
  /// The agent has finished.
  Done,
}

/// One committed-or-refused submission, in the owner's order.
struct Logged {
  agent: usize,
  base: u64,
  file: usize,
  edits: [u8; BLOCKS],
  increment: Increment,
  outcome: Outcome,
}

fn file_name(file: usize) -> String {
  format!("f{file}")
}

/// A distinct identity per (agent, round).
fn identity(agent: usize, round: usize) -> [u8; 32] {
  let mut id = [0u8; 32];
  id[0] = u8::try_from(agent).unwrap();
  id[1] = u8::try_from(round).unwrap();
  id
}

/// The setup increment: every file created with the base bytes, version 1.
fn setup() -> Increment {
  let mut build = Build::new();
  for file in 0..FILES {
    build.create(&file_name(file), &base_file());
  }
  build.at(SETUP_ID, 0)
}

/// An agent's rounds: read the head, edit random blocks of a random file against it, submit, and
/// check the reply's shape (an accepted version is past the base; the next head is at or past it).
fn agent(agent: usize, requests: SyncSender<Request>) {
  let mut rng = thread_rng();
  let (reply_head, head_replies) = sync_channel::<u64>(1);
  let (reply_outcome, outcome_replies) = sync_channel::<Outcome>(1);
  let mut floor = 0u64;
  for round in 0..ROUNDS {
    requests
      .send(Request::Head {
        reply: reply_head.clone(),
      })
      .unwrap();
    let base = head_replies.recv().unwrap();
    assert!(base >= floor, "the head never moves backwards");
    let file = rng.gen_range(0..FILES);
    let mut edits = [0u8; BLOCKS];
    for tag in edits.iter_mut() {
      *tag = rng.gen_range(0..=TAGS);
    }
    if edits.iter().all(|&tag| tag == 0) {
      edits[0] = 1;
    }
    let increment = block_edits_on(&file_name(file), &edits).with_id(identity(agent, round), base);
    requests
      .send(Request::Submit {
        agent,
        file,
        edits,
        increment,
        reply: reply_outcome.clone(),
      })
      .unwrap();
    match outcome_replies.recv().unwrap() {
      Outcome::Accepted { version } => {
        assert!(version > base, "an accepted version is past its base");
        floor = version;
      }
      Outcome::Conflict { windows } => {
        assert!(!windows.is_empty(), "a conflict names its windows");
      }
    }
  }
  requests.send(Request::Done).unwrap();
}

/// The owner: serves requests in arrival order until every agent is done, logging each submission.
fn owner(green: &mut Green, requests: &Receiver<Request>) -> Vec<Logged> {
  let mut log = Vec::new();
  let mut done = 0;
  while done < AGENTS {
    match requests.recv().unwrap() {
      Request::Head { reply } => reply.send(green.head()).unwrap(),
      Request::Submit {
        agent,
        file,
        edits,
        increment,
        reply,
      } => {
        let outcome = green.submit(&increment);
        log.push(Logged {
          agent,
          base: increment.base,
          file,
          edits,
          increment,
          outcome: outcome.clone(),
        });
        reply.send(outcome).unwrap();
      }
      Request::Done => done += 1,
    }
  }
  log
}

/// The block oracle's state for one file.
#[derive(Clone)]
struct FileModel {
  tags: [u8; BLOCKS],
  block_version: [u64; BLOCKS],
  last_changed: u64,
}

/// The design's conflict rule per block: a submission conflicts iff some block it writes was
/// written after its base with different bytes.
fn conflicts(model: &FileModel, entry: &Logged) -> bool {
  (0..BLOCKS).any(|b| {
    entry.edits[b] != 0 && model.block_version[b] > entry.base && model.tags[b] != entry.edits[b]
  })
}

/// Applies an accepted submission's blocks at `version`: a block written after the base with the
/// same bytes is an identical overlap and is left as it is; every other written block takes the
/// submission's bytes. Returns whether any block was applied and whether any was identical.
fn apply_blocks(model: &mut FileModel, entry: &Logged, version: u64) -> (bool, bool) {
  let mut applied = false;
  let mut identical = false;
  for b in (0..BLOCKS).filter(|&b| entry.edits[b] != 0) {
    if model.block_version[b] > entry.base && model.tags[b] == entry.edits[b] {
      identical = true;
      continue;
    }
    model.tags[b] = entry.edits[b];
    model.block_version[b] = version;
    applied = true;
  }
  (applied, identical)
}

/// Decides the log by the block oracle, in the owner's order: a conflicting submission is refused;
/// any other is accepted at the next version, its non-identical blocks applied, and it took the
/// fast path iff the file was last changed at or before its base. Returns the oracle's fast-path
/// count, the per-file bytes, and the head.
fn decide_by_the_oracle(log: &[Logged], head_after_setup: u64) -> (u64, Vec<Vec<u8>>, u64) {
  let mut files = vec![
    FileModel {
      tags: [0; BLOCKS],
      block_version: [0; BLOCKS],
      last_changed: head_after_setup,
    };
    FILES
  ];
  let mut head = head_after_setup;
  let mut fast_paths = 0u64;
  for entry in log {
    let model = &mut files[entry.file];
    if conflicts(model, entry) {
      assert!(
        matches!(entry.outcome, Outcome::Conflict { .. }),
        "agent {} based on {} on file {}: the oracle conflicts, the engine said {:?}",
        entry.agent,
        entry.base,
        entry.file,
        entry.outcome
      );
      CONFLICTS.fetch_add(1, Ordering::Relaxed);
      continue;
    }
    head += 1;
    assert_eq!(
      entry.outcome,
      Outcome::Accepted { version: head },
      "agent {} based on {} on file {}: the oracle accepts at {head}",
      entry.agent,
      entry.base,
      entry.file
    );
    ACCEPTS.fetch_add(1, Ordering::Relaxed);
    if model.last_changed <= entry.base {
      fast_paths += 1;
      FAST_PATHS.fetch_add(1, Ordering::Relaxed);
    }
    let (applied, identical) = apply_blocks(model, entry, head);
    if identical {
      IDENTICAL.fetch_add(1, Ordering::Relaxed);
    }
    if applied {
      model.last_changed = head;
    }
  }
  let bytes = files
    .iter()
    .map(|model| blocks_to_bytes(&model.tags))
    .collect();
  (fast_paths, bytes, head)
}

/// T-6.7: under every schedule shuttle draws, the green's commit order is a linearization of the
/// sixteen agents' submissions, every conflict the oracle predicts is reported and no other, no
/// accepted operation is lost, and the fast-path counter equals the oracle's disjoint count.
/// Linearizability: the commit order replayed serially on a fresh green reproduces every outcome,
/// the same head, the same bytes and the same fast-path count.
fn check_linearizable(setup: &Increment, head_after_setup: u64, log: &[Logged], green: &Green) {
  let mut replay = Green::new();
  assert_eq!(
    replay.submit(setup),
    Outcome::Accepted {
      version: head_after_setup
    }
  );
  for entry in log {
    assert_eq!(
      replay.submit(&entry.increment),
      entry.outcome,
      "agent {}'s submission replays to the same outcome",
      entry.agent
    );
  }
  assert_eq!(replay.head(), green.head());
  assert_eq!(replay.fast_path_hits(), green.fast_path_hits());
  for file in 0..FILES {
    assert_eq!(
      replay.content(&file_name(file)),
      green.content(&file_name(file))
    );
  }
}

/// The block oracle: every conflict reported and no other, no accepted operation lost, and the
/// fast-path counter equal to the oracle's disjoint count.
fn check_by_the_oracle(head_after_setup: u64, log: &[Logged], green: &Green) {
  let (fast_paths, bytes, head) = decide_by_the_oracle(log, head_after_setup);
  assert_eq!(green.head(), head, "one version per accepted submission");
  assert_eq!(
    green.fast_path_hits(),
    fast_paths,
    "the fast-path counter equals the oracle's disjoint count"
  );
  for (file, expected) in bytes.iter().enumerate() {
    assert_eq!(
      green.content(&file_name(file)).unwrap(),
      expected.as_slice(),
      "file {file}: no accepted operation lost, none invented"
    );
  }
}

/// One schedule: the green set up, sixteen agents run against its owner, then both checks.
fn one_schedule() {
  let mut green = Green::new();
  let setup = setup();
  let head_after_setup = match green.submit(&setup) {
    Outcome::Accepted { version } => version,
    other => panic!("the setup increment accepts: {other:?}"),
  };
  let (requests, inbox) = sync_channel::<Request>(REQUEST_BOUND);
  let agents: Vec<_> = (0..AGENTS)
    .map(|index| {
      let requests = requests.clone();
      shuttle::thread::spawn(move || agent(index, requests))
    })
    .collect();
  drop(requests);
  let log = owner(&mut green, &inbox);
  for handle in agents {
    handle.join().unwrap();
  }
  assert_eq!(log.len(), AGENTS * ROUNDS, "every submission was logged");
  check_linearizable(&setup, head_after_setup, &log, &green);
  check_by_the_oracle(head_after_setup, &log, &green);
}

#[test]
fn sixteen_agents_submitting_with_random_overlaps_are_linearized_and_decided_as_the_oracle_says() {
  let mut config = Config::new();
  config.stack_size = STACK_BYTES;
  let schedules =
    Runner::new(RandomScheduler::new_from_seed(SEED, SCHEDULES), config).run(one_schedule);
  eprintln!(
    "shuttle: T-6.7: {schedules} schedules (seed {SEED:#x}): accepts {}, identical accepts {}, conflicts {}, fast paths {}",
    ACCEPTS.load(Ordering::Relaxed),
    IDENTICAL.load(Ordering::Relaxed),
    CONFLICTS.load(Ordering::Relaxed),
    FAST_PATHS.load(Ordering::Relaxed)
  );
  assert_eq!(schedules, SCHEDULES, "the whole budget ran");
  assert!(
    ACCEPTS.load(Ordering::Relaxed) > 0,
    "some schedule accepted"
  );
  assert!(
    IDENTICAL.load(Ordering::Relaxed) > 0,
    "some schedule accepted an identical overlap"
  );
  assert!(
    CONFLICTS.load(Ordering::Relaxed) > 0,
    "some schedule conflicted"
  );
  assert!(
    FAST_PATHS.load(Ordering::Relaxed) > 0,
    "some schedule took the fast path"
  );
}
