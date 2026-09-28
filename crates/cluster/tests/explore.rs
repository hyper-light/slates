//! The Raft dialect's **safety explorer** (§4.8 mechanism 2; `docs/wip/research/consensus-enhancements.md`
//! §5): a seeded, message-level exploration of the real [`RaftNode`] under an adversarial asynchronous
//! network — every message may be delivered in any order, dropped, or duplicated; any node may time out,
//! crash and restart from what it retained, or be cut off by a partition — with the invariants Raft's proof
//! rests on checked after **every** step, across the whole history rather than at its end:
//!
//! - **Election Safety**: at most one leader per term, ever (not only among the current roles).
//! - **Log Matching**: two logs holding an entry of the same term at an index agree on it and on every
//!   entry before it.
//! - **Leader Completeness**: a node that becomes leader holds every entry committed before its term.
//! - **State Machine Safety**: no two nodes ever commit different entries at one index, including a node
//!   that crashed and recovered.
//!
//! The drive copies the council's dispatch (`config_group.rs`): pre-vote first, the new leader's current-term
//! no-op (§5.4.2), CheckQuorum on the leader, replies folded by their sender. Durability is the dialect's own
//! signal: after each step a node with `retention_pending` publishes `saved()` and is marked retained before
//! the step's messages count as sent, and a crash restores from the last publication — so a dialect that
//! acknowledged an entry without asking for its retention would lose it here and fail State Machine Safety.
//!
//! What is not explored yet: snapshots and install-snapshot, and membership changes (the conformance suite
//! `tests/raft.rs` covers both by script). The model-level exploration of the Fast Raft and ParallelRaft log
//! shapes extends this driver. Test by use (R5): the real core, the council's drive, observable outcomes.

// Test harness: an unwrap, expect or panic here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};

use slates_cluster::raft::{LogEntry, RaftNode, SavedRaft};
use slates_cluster::raft_wire::RaftMessage;
use slates_db::register::HostId;

/// Shape: the seeds each cluster size is explored under, and the steps each seed runs. Together they bound
/// the run (every structure below is bounded by a step count), and the per-seed step count is long enough
/// for many elections, commits and crash recoveries in one history.
const SEEDS: u64 = 400;
/// Shape: the steps one seeded history runs.
const STEPS: usize = 4_000;
/// Shape: a history alternates adversarial and calm stretches of this many steps. In a calm stretch nothing
/// crashes, drops or partitions, so elections settle and entries commit; the adversarial stretch that
/// follows then tests the committed history against faults. Without calm stretches a five-voter history
/// committed almost nothing (108 entries over 400 seeds), which would leave the checks vacuous.
const STRETCH: usize = 250;
/// Shape: the in-flight message bag's bound. A step that would overflow it drops the oldest message — a
/// loss, which the protocol must tolerate anyway — and counts it.
const IN_FLIGHT_BOUND: usize = 256;
/// Shape: the commands a history proposes at most, so the logs stay small enough to compare pairwise after
/// every step.
const PROPOSALS_BOUND: u64 = 64;

/// A splitmix64 generator: deterministic from its seed, so every failure replays from the seed printed.
struct Rng(u64);

impl Rng {
  fn next(&mut self) -> u64 {
    self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = self.0;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
  }

  /// A uniform index below `bound` (which must be positive).
  fn below(&mut self, bound: usize) -> usize {
    usize::try_from(self.next() % u64::try_from(bound).unwrap()).unwrap()
  }
}

/// A message in flight: its sender, its recipient, and the message.
#[derive(Clone)]
struct Flight {
  from: HostId,
  to: HostId,
  message: RaftMessage,
}

/// What an exploration counted — the non-vacuity evidence: a history that never elected, never committed
/// or never recovered a crashed node proves nothing about those paths.
#[derive(Default, Debug)]
struct Counters {
  elections_won: u64,
  commits: u64,
  crashes: u64,
  dropped: u64,
  duplicated: u64,
  overflowed: u64,
  pre_votes_refused: u64,
}

/// One explored cluster: the nodes, what each last retained, the network, the partition, and the history
/// the invariants are checked against.
struct Cluster {
  nodes: Vec<RaftNode>,
  retained: Vec<SavedRaft>,
  in_flight: Vec<Flight>,
  /// Nodes cut off from the rest: a message between the two sides is lost.
  isolated: BTreeSet<HostId>,
  /// Every term's leader, over the whole history (Election Safety).
  leaders: BTreeMap<u64, HostId>,
  /// The first entry seen committed at each index (State Machine Safety).
  committed: BTreeMap<u64, LogEntry>,
  /// The (node, term) leaderships already checked for completeness — the property binds a node when it
  /// becomes leader, not a deposed leader that has not yet heard of the newer term.
  completeness_checked: BTreeSet<(HostId, u64)>,
  proposals: u64,
  counters: Counters,
}

impl Cluster {
  fn new(size: u64) -> Cluster {
    let voters: Vec<HostId> = (1..=size).map(HostId).collect();
    let nodes: Vec<RaftNode> = voters
      .iter()
      .map(|id| RaftNode::new(*id, voters.clone()))
      .collect();
    let retained = nodes.iter().map(RaftNode::saved).collect();
    Cluster {
      nodes,
      retained,
      in_flight: Vec::new(),
      isolated: BTreeSet::new(),
      leaders: BTreeMap::new(),
      committed: BTreeMap::new(),
      completeness_checked: BTreeSet::new(),
      proposals: 0,
      counters: Counters::default(),
    }
  }

  fn position(&self, id: HostId) -> usize {
    self.nodes.iter().position(|node| node.id() == id).unwrap()
  }

  /// Queues `message` from `from` to `to`, unless a partition separates them; bounded (the oldest message is
  /// dropped on overflow, counted).
  fn send(&mut self, from: HostId, to: HostId, message: RaftMessage) {
    if self.isolated.contains(&from) != self.isolated.contains(&to) {
      self.counters.dropped += 1;
      return;
    }
    if self.in_flight.len() >= IN_FLIGHT_BOUND {
      self.in_flight.remove(0);
      self.counters.overflowed += 1;
    }
    self.in_flight.push(Flight { from, to, message });
  }

  /// Publishes what node `at` must retain before its messages leave (the dialect's own signal).
  fn retain(&mut self, at: usize) {
    let node = &mut self.nodes[at];
    if node.retention_pending() {
      self.retained[at] = node.saved();
      node.retained();
    }
  }

  /// The council's rule on winning an election: append a current-term no-op (§5.4.2).
  fn finish_election(&mut self, at: usize, was_leader: bool) {
    let node = &mut self.nodes[at];
    if !was_leader && node.is_leader() {
      node.append_command(Vec::new());
      self.counters.elections_won += 1;
    }
  }

  /// An election timeout at node `at`: a pre-election, its pre-votes sent to every other voter.
  fn time_out(&mut self, at: usize) {
    let was_leader = self.nodes[at].is_leader();
    let pre_votes = self.nodes[at].on_election_timeout();
    self.finish_election(at, was_leader);
    self.retain(at);
    let from = self.nodes[at].id();
    let targets: Vec<HostId> = self.nodes[at]
      .all_voters()
      .into_iter()
      .filter(|voter| *voter != from)
      .collect();
    for (to, pre_vote) in targets.into_iter().zip(pre_votes) {
      self.send(from, to, RaftMessage::PreVote(pre_vote));
    }
  }

  /// The leader at `at` replicates to every peer it replicates to (entries or a heartbeat).
  fn heartbeat(&mut self, at: usize) {
    let from = self.nodes[at].id();
    let targets = self.nodes[at].replication_targets();
    for to in targets {
      if let Some(append) = self.nodes[at].replicate_to(to) {
        self.send(from, to, RaftMessage::AppendEntries(append));
      }
    }
  }

  /// Delivers the in-flight message at `index`: a request is answered, a reply folded by its sender.
  fn deliver(&mut self, index: usize) {
    let flight = self.in_flight.remove(index);
    let at = self.position(flight.to);
    let was_leader = self.nodes[at].is_leader();
    let mut outgoing: Vec<RaftMessage> = Vec::new();
    match flight.message {
      RaftMessage::PreVote(pre) => {
        let reply = self.nodes[at].on_pre_vote(pre);
        if !reply.granted {
          self.counters.pre_votes_refused += 1;
        }
        outgoing.push(RaftMessage::PreVoteReply(reply));
      }
      RaftMessage::RequestVote(vote) => {
        outgoing.push(RaftMessage::VoteReply(self.nodes[at].on_request_vote(vote)));
      }
      RaftMessage::AppendEntries(append) => {
        outgoing.push(RaftMessage::AppendReply(
          self.nodes[at].on_append_entries(append),
        ));
      }
      RaftMessage::PreVoteReply(reply) => {
        if let Some(votes) = self.nodes[at].on_pre_vote_reply(reply) {
          outgoing.extend(votes.into_iter().map(RaftMessage::RequestVote));
        }
      }
      RaftMessage::VoteReply(reply) => self.nodes[at].on_vote_reply(reply),
      RaftMessage::AppendReply(reply) => self.nodes[at].on_append_reply(reply),
    }
    self.finish_election(at, was_leader);
    self.retain(at);
    let from = flight.to;
    // A request's reply goes back to its sender; the vote requests a granted pre-election yields go to
    // every other voter, one each.
    let voters: Vec<HostId> = self.nodes[at]
      .all_voters()
      .into_iter()
      .filter(|voter| *voter != from)
      .collect();
    let mut vote_targets = voters.into_iter();
    for message in outgoing {
      let to = match message {
        RaftMessage::RequestVote(_) => match vote_targets.next() {
          Some(to) => to,
          None => continue,
        },
        _ => flight.from,
      };
      self.send(from, to, message);
    }
  }

  /// Crashes node `at` and restarts it from what it last retained.
  fn crash(&mut self, at: usize) {
    self.nodes[at] =
      RaftNode::restore(self.retained[at].clone()).expect("a retained state restores");
    self.counters.crashes += 1;
  }

  /// One step chosen by `rng`: any action in an adversarial stretch; in a calm one, no crash, drop, duplicate
  /// or partition (a calm stretch heals any partition first).
  fn step(&mut self, rng: &mut Rng, calm: bool) {
    if calm {
      self.isolated.clear();
    }
    let roll = rng.below(100);
    if calm && matches!(roll, 50..=57 | 92..=97) {
      return;
    }
    match roll {
      0..=49 => self.network_tick(rng),
      50..=54 => self.drop_one(rng),
      55..=57 => self.duplicate_one(rng),
      58..=65 => {
        let at = rng.below(self.nodes.len());
        self.time_out(at);
      }
      66..=79 => {
        if let Some(at) = self.nodes.iter().position(RaftNode::is_leader) {
          self.heartbeat(at);
        }
      }
      80..=87 => self.propose(),
      88..=91 => self.check_quorum_everywhere(),
      92..=94 => {
        let at = rng.below(self.nodes.len());
        self.crash(at);
      }
      95..=97 => self.isolate_or_heal(rng),
      _ => {}
    }
  }

  /// A network tick: deliver up to one message per node, each chosen uniformly from the bag (so any order is
  /// possible). One per node matches delivery to the fan-out a broadcast generates; delivering one message per
  /// step let a five-voter bag saturate and silently drop 14,324 messages at its bound.
  fn network_tick(&mut self, rng: &mut Rng) {
    for _ in 0..self.nodes.len() {
      if self.in_flight.is_empty() {
        break;
      }
      let index = rng.below(self.in_flight.len());
      self.deliver(index);
    }
  }

  /// Loses one message in flight.
  fn drop_one(&mut self, rng: &mut Rng) {
    if self.in_flight.is_empty() {
      return;
    }
    let index = rng.below(self.in_flight.len());
    self.in_flight.remove(index);
    self.counters.dropped += 1;
  }

  /// Delivers one message in flight twice (a copy is queued beside it).
  fn duplicate_one(&mut self, rng: &mut Rng) {
    if self.in_flight.is_empty() {
      return;
    }
    let index = rng.below(self.in_flight.len());
    let copy = self.in_flight[index].clone();
    self.send(copy.from, copy.to, copy.message);
    self.counters.duplicated += 1;
  }

  /// The leader, if any, proposes the next command (bounded by [`PROPOSALS_BOUND`]).
  fn propose(&mut self) {
    if self.proposals >= PROPOSALS_BOUND {
      return;
    }
    let Some(at) = self.nodes.iter().position(RaftNode::is_leader) else {
      return;
    };
    self.proposals += 1;
    let command = self.proposals.to_le_bytes().to_vec();
    self.nodes[at].append_command(command);
    self.retain(at);
  }

  /// Every node that believes it leads runs its CheckQuorum tick.
  fn check_quorum_everywhere(&mut self) {
    for node in &mut self.nodes {
      if node.is_leader() {
        node.check_quorum();
      }
    }
  }

  /// Isolates one node (a minority) when none is isolated; otherwise heals.
  fn isolate_or_heal(&mut self, rng: &mut Rng) {
    if self.isolated.is_empty() {
      let at = rng.below(self.nodes.len());
      self.isolated.insert(self.nodes[at].id());
    } else {
      self.isolated.clear();
    }
  }

  /// Checks every invariant against the whole history so far; panics naming the one violated.
  fn check(&mut self, seed: u64, step: usize) {
    let logs: Vec<(HostId, SavedRaft)> = self
      .nodes
      .iter()
      .map(|node| (node.id(), node.saved()))
      .collect();
    let at = format!("seed {seed} step {step}");
    self.check_election_safety(&at);
    self.check_state_machine_safety(&logs, &at);
    self.check_leader_completeness(&logs, &at);
    check_log_matching(&logs, &at);
  }

  /// Election Safety, over the whole history: every term has at most one leader, ever.
  fn check_election_safety(&mut self, at: &str) {
    for node in &self.nodes {
      if node.is_leader() {
        let previous = *self.leaders.entry(node.term()).or_insert(node.id());
        assert_eq!(
          previous,
          node.id(),
          "{at}: two leaders of term {}",
          node.term()
        );
      }
    }
  }

  /// State Machine Safety: every committed entry agrees with the first seen committed at its index.
  fn check_state_machine_safety(&mut self, logs: &[(HostId, SavedRaft)], at: &str) {
    for (id, saved) in logs {
      let base = saved.snapshot_index;
      for index in (base + 1)..=saved.commit_index {
        let entry = &saved.log[usize::try_from(index - base - 1).unwrap()];
        let first = self.committed.entry(index).or_insert_with(|| entry.clone());
        assert_eq!(
          first, entry,
          "{at}: {id:?} committed a different entry at index {index}"
        );
      }
    }
    self.counters.commits = u64::try_from(self.committed.len()).unwrap();
  }

  /// Leader Completeness: a node becoming leader holds every entry committed so far (checked once per
  /// leadership — the property binds a node when it becomes leader, not a deposed one yet to hear of it).
  fn check_leader_completeness(&mut self, logs: &[(HostId, SavedRaft)], at: &str) {
    for (id, saved) in logs {
      let node = &self.nodes[self.position(*id)];
      if !node.is_leader() || !self.completeness_checked.insert((*id, node.term())) {
        continue;
      }
      for (index, entry) in &self.committed {
        let held = usize::try_from(index - saved.snapshot_index - 1)
          .ok()
          .and_then(|position| saved.log.get(position));
        assert_eq!(
          held,
          Some(entry),
          "{at}: leader {id:?} of term {} lacks committed index {index}",
          node.term()
        );
      }
    }
  }
}

/// Log Matching, pairwise: two logs holding an entry of the same term at an index agree on it and on every
/// entry before it.
fn check_log_matching(logs: &[(HostId, SavedRaft)], at: &str) {
  for (left_id, left) in logs {
    for (right_id, right) in logs {
      if left_id >= right_id {
        continue;
      }
      let shared = left.log.len().min(right.log.len());
      let matched = (0..shared)
        .rev()
        .find(|position| left.log[*position].term == right.log[*position].term);
      if let Some(position) = matched {
        assert_eq!(
          left.log[..=position],
          right.log[..=position],
          "{at}: {left_id:?} and {right_id:?} share a term at index {} but differ before it",
          position + 1
        );
      }
    }
  }
}

/// Explores `SEEDS` seeded histories of a cluster of `size` voters and returns what they counted.
fn explore(size: u64) -> Counters {
  let mut total = Counters::default();
  for seed in 0..SEEDS {
    let mut rng = Rng(seed ^ (size << 32));
    let mut cluster = Cluster::new(size);
    for step in 0..STEPS {
      let calm = (step / STRETCH) % 2 == 1;
      cluster.step(&mut rng, calm);
      cluster.check(seed, step);
    }
    let c = &cluster.counters;
    total.elections_won += c.elections_won;
    total.commits += c.commits;
    total.crashes += c.crashes;
    total.dropped += c.dropped;
    total.duplicated += c.duplicated;
    total.overflowed += c.overflowed;
    total.pre_votes_refused += c.pre_votes_refused;
  }
  total
}

/// T-8.13 (§4.8 mechanism 2; `docs/wip/research/consensus-enhancements.md` §5): the dialect keeps Election
/// Safety, Log Matching, Leader Completeness and State Machine Safety in every explored history of three
/// and five voters under loss, duplication, reordering, partitions and crash-restarts — and the histories
/// did exercise elections, commits, crashes and pre-vote refusals (non-vacuity).
#[test]
fn the_dialect_keeps_raft_safety_under_an_adversarial_network() {
  for size in [3, 5] {
    let counted = explore(size);
    eprintln!("explored {size} voters x {SEEDS} seeds x {STEPS} steps: {counted:?}");
    assert!(counted.elections_won > SEEDS, "elections were won");
    assert!(counted.commits > SEEDS, "entries committed");
    assert!(counted.crashes > SEEDS, "nodes crashed and recovered");
    assert!(counted.pre_votes_refused > 0, "pre-votes were refused");
  }
}
