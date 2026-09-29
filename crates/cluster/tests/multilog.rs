//! MLRaft's explorer (`docs/wip/research/consensus-enhancements.md` §3.6; `slates_cluster::multilog`): a
//! seeded exploration of every node's `n` real Raft logs over one adversarial network, as the single-log
//! explorer (`tests/explore.rs`) explores one log — any message delivered in any order, dropped or
//! duplicated; any node timing out in any log, crashing and restarting from what it retained; a node cut off
//! by a partition; commands proposed to any log's leader, keyed and global; leaders appending barriers when
//! the adversary lets them. Checked after every step:
//!
//! - **per log**, as Raft's own: one leader per term, ever, and no two nodes committing different entries at
//!   one index — including a node that crashed and recovered;
//! - **the merge**, the part this mechanism adds: every node's application — each key's commands in order,
//!   with the epoch each saw, and the global commands in order — is a prefix of one history, the first seen,
//!   across the nodes and across a node's restarts (a restarted node applies everything again, and must
//!   reach the same).
//!
//! Test by use (R5): the real logs, the real merge, observable application.

// Test harness: an unwrap, expect or panic here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};

use slates_cluster::multilog::{Applied, MultiLog, Route};
use slates_cluster::raft::{LogEntry, SavedRaft};
use slates_cluster::raft_wire::RaftMessage;
use slates_db::register::HostId;

/// Shape: the seeds each shape is explored under at full scale, and in the workspace's debug run.
const SEEDS_FULL: u64 = 200;
const SEEDS_QUICK: u64 = 16;
/// Shape: the steps one seeded history runs.
const STEPS: usize = 3_000;
/// Shape: a history alternates adversarial and calm stretches of this many steps, as the single-log explorer's
/// do, so elections settle and commands commit between the faults.
const STRETCH: usize = 200;
/// Shape: the in-flight bound (the oldest message dropped past it, counted).
const IN_FLIGHT_BOUND: usize = 512;
/// Shape: the commands a history proposes at most, keyed and global.
const PROPOSALS_BOUND: u64 = 96;
/// Shape: the keys the commands write — few, so every key collects a history across several epochs.
const KEYS: u64 = 12;
/// Shape: one command in this many is global.
const GLOBAL_EVERY: u64 = 6;
/// Shape: the actions one step chooses among.
const ACTIONS: usize = 100;

/// A splitmix64 generator: deterministic from its seed.
struct Rng(u64);

impl Rng {
  fn next(&mut self) -> u64 {
    self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = self.0;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
  }

  fn below(&mut self, bound: usize) -> usize {
    usize::try_from(self.next() % u64::try_from(bound).unwrap()).unwrap()
  }
}

/// A message in flight in one log.
#[derive(Clone)]
struct Flight {
  log: usize,
  from: HostId,
  to: HostId,
  message: RaftMessage,
}

/// One node's application since its last restart: each key's commands with the epoch each saw (`None` for the
/// global commands).
type History = BTreeMap<Option<u64>, Vec<(Vec<u8>, u64)>>;

/// What an exploration counted — the non-vacuity evidence.
#[derive(Default, Debug)]
struct Counters {
  elections_won: u64,
  keyed_applied: u64,
  globals_applied: u64,
  barriers_appended: u64,
  crashes: u64,
  replays_matched: u64,
  dropped: u64,
  duplicated: u64,
  overflowed: u64,
  /// Proposals a node could not append: it did not lead the log the command routes to.
  proposals_refused: u64,
}

struct Cluster {
  nodes: Vec<MultiLog>,
  retained: Vec<Vec<SavedRaft>>,
  in_flight: Vec<Flight>,
  isolated: BTreeSet<HostId>,
  /// Per (log, term), the leader seen.
  leaders: BTreeMap<(usize, u64), HostId>,
  /// Per (log, index), the entry first seen committed there.
  committed: BTreeMap<(usize, u64), LogEntry>,
  /// Per node, its application since its last restart.
  applied: Vec<History>,
  /// The history every node's application must be a prefix of: per key, the longest sequence seen.
  reference: History,
  proposals: u64,
  counters: Counters,
}

impl Cluster {
  fn new(voters: u64, logs: usize) -> Cluster {
    let ids: Vec<HostId> = (1..=voters).map(HostId).collect();
    let nodes: Vec<MultiLog> = ids
      .iter()
      .map(|id| MultiLog::new(*id, ids.clone(), logs))
      .collect();
    let retained = nodes.iter().map(MultiLog::saved).collect();
    Cluster {
      applied: vec![History::new(); nodes.len()],
      nodes,
      retained,
      in_flight: Vec::new(),
      isolated: BTreeSet::new(),
      leaders: BTreeMap::new(),
      committed: BTreeMap::new(),
      reference: History::new(),
      proposals: 0,
      counters: Counters::default(),
    }
  }

  fn id(&self, at: usize) -> HostId {
    HostId(u64::try_from(at).unwrap() + 1)
  }

  fn position(id: HostId) -> usize {
    usize::try_from(id.0).unwrap() - 1
  }

  fn send(&mut self, log: usize, from: HostId, to: HostId, message: RaftMessage) {
    if self.isolated.contains(&from) != self.isolated.contains(&to) {
      self.counters.dropped += 1;
      return;
    }
    if self.in_flight.len() >= IN_FLIGHT_BOUND {
      self.in_flight.remove(0);
      self.counters.overflowed += 1;
    }
    self.in_flight.push(Flight {
      log,
      from,
      to,
      message,
    });
  }

  /// Publishes what node `at` must retain before its messages leave.
  fn retain(&mut self, at: usize) {
    if self.nodes[at].retention_pending() {
      self.retained[at] = self.nodes[at].saved();
      self.nodes[at].retained();
    }
  }

  /// The council's rule on winning an election in `log`: append a no-op (Raft §5.4.2).
  fn finish_election(&mut self, at: usize, log: usize, was_leader: bool) {
    let node = self.nodes[at].log_mut(log).unwrap();
    if !was_leader && node.is_leader() {
      node.append_command(Vec::new());
      self.counters.elections_won += 1;
    }
  }

  fn others(&self, at: usize) -> Vec<HostId> {
    let id = self.id(at);
    (0..self.nodes.len())
      .map(|other| self.id(other))
      .filter(|other| *other != id)
      .collect()
  }

  /// Node `at` times out in `log`: a pre-election, its pre-votes to every other voter.
  fn time_out(&mut self, at: usize, log: usize) {
    let node = self.nodes[at].log_mut(log).unwrap();
    let was_leader = node.is_leader();
    let pre_votes = node.on_election_timeout();
    self.finish_election(at, log, was_leader);
    self.retain(at);
    let from = self.id(at);
    for (to, pre_vote) in self.others(at).into_iter().zip(pre_votes) {
      self.send(log, from, to, RaftMessage::PreVote(pre_vote));
    }
  }

  /// `log`'s leader, if any, replicates to every other voter.
  fn heartbeat(&mut self, log: usize) {
    let Some(at) = (0..self.nodes.len()).find(|at| self.nodes[*at].log(log).unwrap().is_leader())
    else {
      return;
    };
    let from = self.id(at);
    for to in self.others(at) {
      if let Some(append) = self.nodes[at]
        .log_mut(log)
        .unwrap()
        .replicate_to(to, usize::MAX)
      {
        self.send(log, from, to, RaftMessage::AppendEntries(append));
      }
    }
  }

  /// A command — keyed to one of [`KEYS`] or, one in [`GLOBAL_EVERY`], global — proposed at a node `rng`
  /// picks: appended when it leads the log the command routes to.
  fn propose(&mut self, rng: &mut Rng) {
    if self.proposals >= PROPOSALS_BOUND {
      return;
    }
    let route = if rng.below(usize::try_from(GLOBAL_EVERY).unwrap()) == 0 {
      Route::Global
    } else {
      Route::Key(u64::try_from(rng.below(usize::try_from(KEYS).unwrap())).unwrap())
    };
    let at = rng.below(self.nodes.len());
    self.proposals += 1;
    let command = self.proposals.to_le_bytes().to_vec();
    if self.nodes[at].propose(route, command) {
      self.retain(at);
    } else {
      self.counters.proposals_refused += 1;
    }
  }

  /// Node `at` appends the barriers its leaderships owe.
  fn barriers(&mut self, at: usize) {
    let appended = self.nodes[at].append_barriers();
    self.counters.barriers_appended += u64::try_from(appended).unwrap();
    self.retain(at);
  }

  /// Node `at` crashes and restarts from what it retained: its logs restore, its merge starts over, and its
  /// application is rebuilt from nothing.
  fn crash(&mut self, at: usize) {
    self.nodes[at] =
      MultiLog::restore(self.retained[at].clone()).expect("a retained state restores");
    self.applied[at] = History::new();
    self.counters.crashes += 1;
  }

  /// Delivers the message at `index` of the bag: a request answered, a reply folded, a won pre-election's vote
  /// requests sent.
  fn deliver(&mut self, index: usize) {
    let flight = self.in_flight.remove(index);
    let at = Self::position(flight.to);
    let log = flight.log;
    let node = self.nodes[at].log_mut(log).unwrap();
    let was_leader = node.is_leader();
    let mut reply = None;
    let mut votes = Vec::new();
    match flight.message {
      RaftMessage::PreVote(pre) => reply = Some(RaftMessage::PreVoteReply(node.on_pre_vote(pre))),
      RaftMessage::RequestVote(vote) => {
        reply = Some(RaftMessage::VoteReply(node.on_request_vote(vote)))
      }
      RaftMessage::AppendEntries(append) => {
        reply = Some(RaftMessage::AppendReply(node.on_append_entries(append)));
      }
      RaftMessage::PreVoteReply(answer) => {
        votes = node.on_pre_vote_reply(answer).unwrap_or_default()
      }
      RaftMessage::VoteReply(answer) => node.on_vote_reply(answer),
      RaftMessage::AppendReply(answer) => node.on_append_reply(answer),
      _ => {}
    }
    self.finish_election(at, log, was_leader);
    self.retain(at);
    let from = flight.to;
    if let Some(reply) = reply {
      self.send(log, from, flight.from, reply);
    }
    for (to, vote) in self.others(at).into_iter().zip(votes) {
      self.send(log, from, to, RaftMessage::RequestVote(vote));
    }
  }

  fn network_tick(&mut self, rng: &mut Rng) {
    for _ in 0..self.nodes.len() {
      if self.in_flight.is_empty() {
        break;
      }
      let index = rng.below(self.in_flight.len());
      self.deliver(index);
    }
  }

  fn step(&mut self, rng: &mut Rng, calm: bool) {
    if calm {
      self.isolated.clear();
    }
    let roll = rng.below(ACTIONS);
    if calm && matches!(roll, 60..=65 | 90..=94) {
      return;
    }
    let logs = self.nodes[0].count();
    match roll {
      0..=39 => self.network_tick(rng),
      40..=47 => {
        let log = rng.below(logs);
        self.heartbeat(log);
      }
      48..=55 => self.propose(rng),
      56..=59 => {
        let at = rng.below(self.nodes.len());
        self.barriers(at);
      }
      60..=62 => {
        if !self.in_flight.is_empty() {
          let index = rng.below(self.in_flight.len());
          self.in_flight.remove(index);
          self.counters.dropped += 1;
        }
      }
      63..=65 => {
        if !self.in_flight.is_empty() {
          let index = rng.below(self.in_flight.len());
          let copy = self.in_flight[index].clone();
          self.send(copy.log, copy.from, copy.to, copy.message);
          self.counters.duplicated += 1;
        }
      }
      66..=79 => {
        let (at, log) = (rng.below(self.nodes.len()), rng.below(logs));
        if calm && self.nodes[at].log(log).unwrap().leader().is_some() {
          return;
        }
        self.time_out(at, log);
      }
      80..=89 => {
        for log in 0..logs {
          self.heartbeat(log);
        }
      }
      90..=92 => {
        let at = rng.below(self.nodes.len());
        self.crash(at);
      }
      93..=94 => {
        if self.isolated.is_empty() {
          let at = rng.below(self.nodes.len());
          self.isolated.insert(self.id(at));
        } else {
          self.isolated.clear();
        }
      }
      _ => {
        let at = rng.below(self.nodes.len());
        self.barriers(at);
      }
    }
  }

  /// Every node applies what its logs now allow, and the checks run.
  fn check(&mut self, seed: u64, step: usize) {
    let at = format!("seed {seed} step {step}");
    for node in 0..self.nodes.len() {
      let fresh = self.nodes[node].apply_ready();
      self.record(node, fresh, &at);
    }
    self.check_logs(&at);
  }

  /// Folds node `node`'s newly applied commands into its history and holds it to the reference.
  fn record(&mut self, node: usize, fresh: Vec<Applied>, at: &str) {
    for applied in fresh {
      if applied.key.is_some() {
        self.counters.keyed_applied += 1;
      } else {
        self.counters.globals_applied += 1;
      }
      let history = self.applied[node].entry(applied.key).or_default();
      history.push((applied.command, applied.epoch));
      let reference = self.reference.entry(applied.key).or_default();
      let position = history.len() - 1;
      match reference.get(position) {
        Some(seen) => assert_eq!(
          seen, &history[position],
          "{at}: node {node} applied a different command or epoch for key {:?} at its {position}th",
          applied.key
        ),
        None => reference.push(history[position].clone()),
      }
      if self.counters.crashes > 0 && position + 1 < reference.len() {
        self.counters.replays_matched += 1;
      }
    }
  }

  /// Raft's own invariants, per log: one leader per term, ever, and one entry committed per index.
  fn check_logs(&mut self, at: &str) {
    for node in &self.nodes {
      for log in 0..node.count() {
        let raft = node.log(log).unwrap();
        if raft.is_leader() {
          let previous = *self.leaders.entry((log, raft.term())).or_insert(raft.id());
          assert_eq!(
            previous,
            raft.id(),
            "{at}: two leaders of term {} in log {log}",
            raft.term()
          );
        }
        let base = raft.snapshot_index();
        for (offset, entry) in raft.committed_entries().iter().enumerate() {
          let index = base + u64::try_from(offset).unwrap() + 1;
          let first = self
            .committed
            .entry((log, index))
            .or_insert_with(|| entry.clone());
          assert_eq!(
            first, entry,
            "{at}: log {log} committed two entries at {index}"
          );
        }
      }
    }
  }
}

/// Explores `seeds` histories of `voters` voters holding `logs` logs each, and returns what they counted.
fn explore(voters: u64, logs: usize, seeds: u64) -> Counters {
  let mut total = Counters::default();
  for seed in 0..seeds {
    let mut rng = Rng(seed ^ (voters << 32) ^ (u64::try_from(logs).unwrap() << 40));
    let mut cluster = Cluster::new(voters, logs);
    for step in 0..STEPS {
      let calm = (step / STRETCH) % 2 == 1;
      cluster.step(&mut rng, calm);
      cluster.check(seed, step);
    }
    let c = &cluster.counters;
    total.elections_won += c.elections_won;
    total.keyed_applied += c.keyed_applied;
    total.globals_applied += c.globals_applied;
    total.barriers_appended += c.barriers_appended;
    total.crashes += c.crashes;
    total.replays_matched += c.replays_matched;
    total.dropped += c.dropped;
    total.duplicated += c.duplicated;
    total.overflowed += c.overflowed;
    total.proposals_refused += c.proposals_refused;
  }
  total
}

/// Explores three voters with three logs and five with two, and holds every non-vacuity floor.
fn explore_and_check_coverage(seeds: u64) {
  for (voters, logs) in [(3, 3), (5, 2)] {
    let counted = explore(voters, logs, seeds);
    eprintln!(
      "explored {voters} voters x {logs} logs x {seeds} seeds x {STEPS} steps: {counted:?}"
    );
    let floors = [
      (counted.elections_won, "elections were won"),
      (counted.keyed_applied, "keyed commands were applied"),
      (counted.globals_applied, "global commands were applied"),
      (counted.barriers_appended, "barriers were appended"),
      (counted.crashes, "nodes crashed and recovered"),
      (
        counted.replays_matched,
        "restarted nodes replayed their application",
      ),
    ];
    for (count, path) in floors {
      assert!(
        count > seeds,
        "{voters} voters, {logs} logs: {path} ({count} over {seeds} seeds)"
      );
    }
  }
}

/// §3.6, T-8.13's counterpart for MLRaft: the merge applies every key's commands in one order and each in one
/// epoch, and the global commands in one order, on every node and across every restart, under loss,
/// duplication, reordering, partitions and crash-restarts; each log keeps Raft's own safety. The workspace's
/// scale ([`SEEDS_QUICK`]).
#[test]
fn the_multi_log_merges_alike_under_an_adversarial_network() {
  explore_and_check_coverage(SEEDS_QUICK);
}

/// The same at full scale ([`SEEDS_FULL`]), run by CI in release with `--ignored`.
#[test]
#[ignore = "full scale: CI runs it in release"]
fn the_multi_log_merges_alike_at_full_scale() {
  explore_and_check_coverage(SEEDS_FULL);
}
