//! A replica fed its owner's feed is its owner's green at every version it holds, and reads its owner's content in
//! place (A-273 steps 2 and 3; §4.16 "Rebase, the only corrective path"). Each generated history runs one owner green
//! over a generated origin: works submit over bases behind the head (conflicts met, over every dimension), fresh files
//! are made and edited, and the owner folds its history, to a floor or as far as a budget needs. A replica is made from
//! the owner at any point. The owner's feed messages travel in order and arrive late: delivered a few at a time, still
//! in flight when the replica is rebuilt (so it must pass them over), or lost (so the next must be refused as a gap and
//! the replica rebuilt); the replica's acknowledgements reach the owner late too, and the owner frees only what they
//! say was passed. After every step the replica's head must be a version the owner committed, its identity the owner's
//! at that version, every name's bytes at its head the owner's, and no read of its refused. At the end of every
//! history, with everything delivered and acknowledged, it must equal the owner whole: the head and its identity,
//! every name's bytes and the paths changed since every version, the answer to a rebase over every base, the fold
//! floor, and its current bytes the owner's own memory, never a copy; and once it leaves, the owner keeps nothing for it.
// Test harness code: an unwrap or a panic in a helper is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

mod common;

use std::collections::{BTreeMap, VecDeque};

use common::generated::{Raw, increment_of, origin_of, path, raw};
use proptest::prelude::*;
use slates_mem::sealed::Passed;
use slates_merge::engine::{Green, Outcome};
use slates_merge::replica::{Fed, FeedBody, FeedMessage, FeedRefusal, Replica};

/// Shape: the alphabet's indices the comparisons walk: every path `common::generated::path` can name.
const PATH_INDICES: u8 = 7;
/// Shape: the generated increments each final comparison rebases over every base both greens hold.
const PROBES: usize = 3;

/// One step of a generated history.
#[derive(Clone, Debug)]
enum Step {
  /// A work submits this increment over a base this far behind the head.
  Submit(Raw, u8),
  /// A work creates a file of its own, over a base this far behind the head.
  Fresh(u8),
  /// A work edits one of those files at this offset, over a base this far behind the head.
  Edit(u8, u8, u8),
  /// The owner folds its history this many versions past its floor, at most to its head.
  Fold(u8),
  /// The owner folds as far as a budget of this many sixteen-byte units needs.
  FoldToBudget(u8),
  /// This many messages in flight reach the replica.
  Deliver(u8),
  /// This many of the replica's acknowledgements reach the owner.
  Acknowledge(u8),
  /// The oldest message in flight never arrives.
  Lose,
  /// A replica is made from the owner (a new one, if one was there, which leaves).
  Join,
}

fn step() -> impl Strategy<Value = Step> {
  (0u8..17, raw(), any::<u8>(), any::<u8>(), any::<u8>()).prop_map(
    |(kind, raw, first, second, third)| match kind {
      0..=3 => Step::Submit(raw, first % 6),
      4..=5 => Step::Fresh(first % 8),
      6..=7 => Step::Edit(first, second % 16, third % 8),
      8 => Step::Fold(first % 4),
      9 => Step::FoldToBudget(first),
      10..=12 => Step::Deliver(first % 6),
      13 => Step::Acknowledge(first % 6),
      14 => Step::Lose,
      _ => Step::Join,
    },
  )
}

fn id(n: usize) -> [u8; 32] {
  let mut id = [0xA2; 32];
  id[..8].copy_from_slice(&(n as u64).to_le_bytes());
  id
}

/// What a history did, for the non-vacuity assertions.
#[derive(Default, Debug)]
struct Ran {
  /// Messages the replica applied, versions and folds apart.
  versions_applied: u64,
  folds_applied: u64,
  /// Messages it passed over as held by what it was built from.
  passed_over: u64,
  /// Gaps it refused, each followed by a rebuild.
  gaps: u64,
  /// Rebases compared with the owner's, and the conflicts among them.
  rebases: u64,
  rebase_conflicts: u64,
  /// Frees the owner kept for the replica, read at the end: content the replica read in place that the owner released.
  frees_held: u64,
}

/// One history in progress: the owner, its replica if one has joined, the messages in flight to it, and what the owner
/// committed.
struct History {
  origin_rows: Vec<Raw>,
  owner: Green,
  replica: Option<Replica>,
  inflight: VecDeque<FeedMessage>,
  /// The replica's acknowledgements on their way to the owner.
  acks: VecDeque<Passed>,
  /// The owner's identity at every version it committed, and at the origin.
  identities: BTreeMap<u64, [u8; 32]>,
  fresh: Vec<String>,
  ran: Ran,
  next_id: usize,
}

impl History {
  fn new(origin_rows: &[Raw]) -> History {
    let mut owner = Green::with_origin(origin_of(origin_rows));
    let identities = BTreeMap::from([(0, owner.head_identity())]);
    History {
      origin_rows: origin_rows.to_vec(),
      owner,
      replica: None,
      inflight: VecDeque::new(),
      acks: VecDeque::new(),
      identities,
      fresh: Vec::new(),
      ran: Ran::default(),
      next_id: 0,
    }
  }

  /// The base a work this far behind the head has: never below the owner's fold floor, which no live work is behind.
  fn base(&self, behind: u8) -> u64 {
    self
      .owner
      .head()
      .saturating_sub(u64::from(behind))
      .max(self.owner.folded_below())
  }

  /// Submits `build` over `base` to the owner; an accepted version's feed message goes in flight to the replica.
  fn submit(&mut self, build: &common::Build, base: u64) -> Outcome {
    self.next_id += 1;
    let increment = build.with_id(id(self.next_id), base);
    let (outcome, applied) = self.owner.submit_resolved(&increment);
    if let (Outcome::Accepted { version }, Some(applied)) = (&outcome, applied) {
      self.identities.insert(*version, self.owner.head_identity());
      let message = self
        .owner
        .feed_version(*version, increment, applied, None)
        .unwrap();
      self.send(message);
    }
    outcome
  }

  /// A message in flight to the replica; with no replica there is no one to send it to.
  fn send(&mut self, message: FeedMessage) {
    if self.replica.is_some() {
      self.inflight.push_back(message);
    }
  }

  /// The replica rebuilt from the owner now, after it refused a message; its old registration given back.
  fn rebuild(&mut self) {
    let fresh = self.owner.replica().unwrap();
    let left = self.replica.as_mut().unwrap().rebuild(fresh).unwrap();
    self.owner.leave_replica(left);
  }

  /// Up to `count` of the replica's acknowledgements reach the owner, oldest first.
  fn acknowledge(&mut self, count: usize) {
    for _ in 0..count {
      let Some(passed) = self.acks.pop_front() else {
        return;
      };
      self.owner.acknowledge_replica(passed);
    }
  }

  /// Up to `count` messages in flight reach the replica, in order; a refusal is followed by a rebuild.
  fn deliver(&mut self, count: usize) {
    for _ in 0..count {
      let (Some(replica), Some(message)) = (self.replica.as_mut(), self.inflight.pop_front())
      else {
        return;
      };
      match replica.apply(&message) {
        Ok(Fed::Applied { passed }) => {
          self.acks.push_back(passed);
          match message.body {
            FeedBody::Version { .. } => self.ran.versions_applied += 1,
            FeedBody::Fold { .. } => self.ran.folds_applied += 1,
          }
        }
        Ok(Fed::PassedOver) => self.ran.passed_over += 1,
        Err(FeedRefusal::Gap { expected, sent }) => {
          assert!(sent > expected, "a gap is a message past the next one");
          self.ran.gaps += 1;
          self.rebuild();
        }
        Err(refusal) => panic!("the owner's own feed diverged: {refusal:?}"),
      }
    }
  }

  fn step(&mut self, step: &Step) {
    match step {
      Step::Submit(raw, behind) => {
        let base = self.base(*behind);
        self.submit(&increment_of(raw), base);
      }
      Step::Fresh(behind) => {
        let name = format!("fresh-{}", self.fresh.len());
        let mut build = common::Build::new();
        build.create(&name, &[0xF0; 32]);
        let base = self.base(*behind);
        if matches!(self.submit(&build, base), Outcome::Accepted { .. }) {
          self.fresh.push(name);
        }
      }
      Step::Edit(file, at, behind) => {
        if !self.fresh.is_empty() {
          let name = self.fresh[usize::from(*file) % self.fresh.len()].clone();
          let mut build = common::Build::new();
          build.overwrite(&name, u64::from(*at), &[*at; 2]);
          let base = self.base(*behind);
          self.submit(&build, base);
        }
      }
      Step::Fold(past) => {
        let floor = (self.owner.folded_below() + 1 + u64::from(*past)).min(self.owner.head());
        if let (_, Some(message)) = self.owner.fold_and_feed(floor) {
          self.send(message);
        }
      }
      Step::FoldToBudget(units) => {
        let head = self.owner.head();
        let budget = usize::from(*units) * 16;
        if let (_, Some(message)) = self.owner.fold_to_budget_and_feed(head, budget) {
          self.send(message);
        }
      }
      Step::Deliver(count) => self.deliver(usize::from(*count)),
      Step::Acknowledge(count) => self.acknowledge(usize::from(*count)),
      Step::Lose => {
        self.inflight.pop_front();
      }
      Step::Join => {
        if let Some(old) = self.replica.take() {
          self.owner.leave_replica(old.leave().unwrap());
          self.acks.clear();
        }
        self.replica = Replica::new(self.owner.replica().unwrap());
      }
    }
    self.check_at_the_replicas_head();
  }

  /// Every name the history can touch: the path alphabet and the fresh files.
  fn names(&self) -> Vec<String> {
    (0..PATH_INDICES)
      .map(|index| path(index).to_owned())
      .chain(self.fresh.iter().cloned())
      .collect()
  }

  /// The replica, wherever it is in the feed, holds a version the owner committed and is the owner's green there: its
  /// identity (every dimension at once) and every name's bytes, at its head.
  fn check_at_the_replicas_head(&mut self) {
    let names = self.names();
    let Some(replica) = self.replica.as_mut() else {
      return;
    };
    let head = replica.green().head();
    assert!(head <= self.owner.head(), "a replica ahead of its owner");
    assert_eq!(
      replica.green().content_reads_refused(),
      0,
      "a read the replica could not answer in place"
    );
    assert_eq!(
      Some(replica.green_mut().head_identity()),
      self.identities.get(&head).copied(),
      "the replica's identity at version {head} against the owner's there"
    );
    if head >= self.owner.folded_below() {
      for name in &names {
        assert_eq!(
          replica.green().content_at(name, head),
          self.owner.content_at(name, head),
          "{name} at the replica's head {head}"
        );
      }
    }
  }

  /// Everything in flight delivered, the replica equals the owner whole (module docs). Returns what the history did.
  fn finish(mut self, probes: &[Raw]) -> Ran {
    self.deliver(self.inflight.len());
    self.acknowledge(self.acks.len());
    // A lost last message is followed by none the replica could refuse as a gap, so it stays behind, as a lagging
    // replica does: the owner's next message would be refused and the replica rebuilt. The history ends before one is
    // sent, so that rebuild is made here.
    if self
      .replica
      .as_ref()
      .is_some_and(|replica| replica.position() < self.owner.feed_position())
    {
      self.rebuild();
    }
    let names = self.names();
    let Some(mut replica) = self.replica.take() else {
      return self.ran;
    };
    self.acknowledge(self.acks.len());
    let owner = &mut self.owner;
    let green = replica.green_mut();
    assert_eq!(green.head(), owner.head(), "the replica caught up");
    assert_eq!(green.head_identity(), owner.head_identity());
    assert_eq!(
      green.head_identity(),
      green.head_identity_from_scratch(),
      "the replica's identity is its state's"
    );
    assert_eq!(
      green.folded_below(),
      owner.folded_below(),
      "the replica keeps the history its owner keeps, and no more"
    );
    for name in &names {
      if let (Some(mine), Some(theirs)) = (green.content(name), owner.content(name)) {
        assert!(
          std::ptr::eq(mine.as_ptr(), theirs.as_ptr()) && mine.len() == theirs.len(),
          "{name}'s current bytes are the owner's own, read in place"
        );
      }
    }
    assert_eq!(green.content_reads_refused(), 0);
    let floor = owner.folded_below();
    for version in floor..=owner.head() {
      for name in &names {
        assert_eq!(
          green.content_at(name, version),
          owner.content_at(name, version),
          "{name} at version {version}"
        );
      }
      assert_eq!(
        green.changed_since(version),
        owner.changed_since(version),
        "the paths changed since version {version}"
      );
    }
    for (at, probe) in probes.iter().enumerate() {
      for base in floor..=owner.head() {
        let increment = increment_of(probe).with_id(id(usize::MAX - at), base);
        let answer = green.rebase(&increment);
        assert_eq!(
          answer,
          owner.rebase(&increment),
          "a rebase over base {base}"
        );
        self.ran.rebases += 1;
        if matches!(answer, slates_merge::engine::Rebased::Conflict { .. }) {
          self.ran.rebase_conflicts += 1;
        }
      }
    }
    self.ran.frees_held = owner.content_frees_held();
    owner.leave_replica(replica.leave().unwrap());
    assert_eq!(
      owner.content_held_bytes(),
      0,
      "the owner keeps nothing for a replica that left"
    );
    assert_eq!(
      owner.retained_bytes().history,
      owner.history_bytes_recounted()
    );
    self.ran
  }
}

fn run(origin_rows: &[Raw], steps: &[Step], probes: &[Raw]) -> Ran {
  let mut history = History::new(origin_rows);
  for step in steps {
    history.step(step);
  }
  history.finish(probes)
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::with_cases(256)))]

  /// A-273. Do: a generated history of submits, fresh files, edits and folds on an owner over a generated origin, with
  /// a replica joining from the owner's seed and its feed delivered in order, late, or with a message lost. Expect:
  /// after every step the replica is the owner's green at its head, and at the end, everything delivered, it equals
  /// the owner whole and answers every rebase the owner answers over a base it holds.
  #[test]
  fn a_replica_fed_by_its_owner_is_its_owners_green_at_every_version(
    origin_rows in prop::collection::vec(raw(), 0..8),
    steps in prop::collection::vec(step(), 1..64),
    probes in prop::collection::vec(raw(), PROBES..=PROBES),
  ) {
    run(&origin_rows, &steps, &probes);
  }
}

/// Non-vacuity, A-273: one fixed history reaches every path of the feed. Do: a replica is made, versions and a fold
/// are delivered, a version is made while a message is in flight and the replica is made anew (so it passes the message
/// over), a message is lost (so the next is refused as a gap and the replica rebuilt), and the final comparison
/// rebases over every base. Expect: versions and folds applied, a message passed over, a gap refused, rebases compared,
/// a conflict among them, and frees the owner kept for the replica's in-place reads.
#[test]
fn a_fixed_history_reaches_every_path_of_the_feed() {
  // A generated step's shape: kind, path index, a second path, the offset (0, so every overwrite fits) and the byte.
  let make = |kind: u8, here: u8, byte: u8| -> Raw { (kind, here, 1, 0, byte) };
  let (create, overwrite, remove, mkdir, setmode) = (0, 1, 5, 6, 8);
  let steps = [
    Step::Submit(make(create, 0, 1), 0),
    Step::Submit(make(overwrite, 0, 2), 0),
    Step::Join,
    Step::Submit(make(overwrite, 0, 3), 0),
    Step::Submit(make(setmode, 0, 4), 0),
    Step::Deliver(2),
    Step::Fold(0),
    Step::Deliver(1),
    Step::Submit(make(create, 1, 5), 0),
    Step::Join,
    Step::Deliver(1),
    Step::Submit(make(overwrite, 1, 6), 0),
    Step::Lose,
    Step::Submit(make(overwrite, 1, 7), 0),
    Step::Deliver(1),
    Step::Submit(make(mkdir, 2, 8), 0),
    Step::Submit(make(overwrite, 0, 9), 1),
  ];
  let probes = [
    make(overwrite, 0, 10),
    make(overwrite, 1, 11),
    make(remove, 2, 12),
  ];
  let ran = run(&[], &steps, &probes);
  assert!(ran.versions_applied >= 2, "{ran:?}");
  assert!(ran.folds_applied >= 1, "{ran:?}");
  assert!(ran.passed_over >= 1, "{ran:?}");
  assert!(ran.gaps >= 1, "{ran:?}");
  assert!(ran.rebases >= 1, "{ran:?}");
  assert!(ran.rebase_conflicts >= 1, "{ran:?}");
  assert!(ran.frees_held >= 1, "{ran:?}");
}
