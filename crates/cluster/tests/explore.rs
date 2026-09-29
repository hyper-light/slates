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
//! Leadership transfers are explored too (thesis §3.10): a leader hands off to a random voter, the invitation
//! rides a heartbeat once the target has caught up, and a delivered invitation starts the target's election.
//!
//! Compaction and snapshots are explored (Raft §7): any node compacts its committed prefix at any step and
//! to any point, and a leader whose follower needs compacted entries ships its snapshot. The explored
//! state machine is the committed history itself — a snapshot carries every entry (term and command)
//! through its index — so each check runs over the whole reconstructed log, and compaction can hide
//! nothing from them. Appends carry a budget of about two entries, so catching a follower up crosses many
//! batch boundaries, and a snapshot's state is sometimes corrupted in flight, which its recipient must
//! decline (the groups decode the state first, `crate::fold`) without the leader crediting it.
//!
//! Not explored yet: membership changes (the conformance suite `tests/raft.rs` covers them by script). The
//! model-level exploration of the Fast Raft and ParallelRaft log shapes extends this driver. Test by use
//! (R5): the real core, the council's drive, observable outcomes.

// Test harness: an unwrap, expect or panic here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};

use slates_cluster::raft::{InstallSnapshot, LogEntry, RaftNode, SavedRaft};
use slates_cluster::raft_wire::RaftMessage;
use slates_db::register::HostId;

/// Shape: the seeds each cluster size is explored under at full scale — the `--ignored` run CI makes in
/// release ("T-8.13 Raft safety explorer at full scale"): 400 seeds × 4,000 steps × 2 sizes is 14 s there
/// and 167 s in a debug build (measured 2026-09-28, Apple M5 Max), so the workspace's debug run explores
/// [`SEEDS_QUICK`] instead.
const SEEDS_FULL: u64 = 400;
/// Shape: the seeds the workspace's debug run explores — about ten seconds of a debug build at 167 s per 400
/// seeds, and still enough histories for every non-vacuity floor below (each is at least one event per seed;
/// full scale counted 18 invited elections per seed at three voters).
const SEEDS_QUICK: u64 = 24;
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
/// Shape: the entry bytes one append carries — about two of the explorer's command entries (21 wire bytes
/// each: term, length, an eight-byte command, the configuration flag) — so catching a follower up takes
/// several batches and every batch boundary is explored.
const APPEND_BUDGET: usize = 48;
/// Shape: the actions one step chooses among — the hundred of the original mix and four more: three for a
/// compaction, one for corrupting a snapshot in flight.
const ACTIONS: usize = 104;

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
  /// Leadership transfers started (thesis §3.10).
  transfers_started: u64,
  /// Invitations sent (the target had caught up).
  invitations_sent: u64,
  /// Invitations that started an election at their target.
  invited_elections: u64,
  /// Compactions taken (Raft §7).
  compactions: u64,
  /// Snapshots a leader shipped to a follower whose entries were compacted away.
  snapshots_sent: u64,
  /// Snapshots a follower installed (its reply credited the leader).
  snapshots_installed: u64,
  /// Snapshots whose state was corrupted in flight and declined by their recipient.
  snapshots_declined: u64,
  /// Appends that left entries owed because the budget bounded them.
  bounded_batches: u64,
  /// Consistency-check refusals that carried a conflict hint (Raft §5.3).
  conflict_hints: u64,
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
  /// The next snapshot a leader ships is corrupted in flight (set by the adversary's corruption move).
  corrupt_next_snapshot: bool,
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
      corrupt_next_snapshot: false,
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
    // `replication_targets` lists the voter set, the leader included; the council's drive replicates to the
    // others only (`drive_config_council`'s `others`). Sending the leader its own current-term append
    // demoted it to a follower at the same term (`on_append_entries` defers to a current-term leader), so
    // until this filter the explorer's leaders deposed themselves on most heartbeats.
    let targets: Vec<HostId> = self.nodes[at]
      .replication_targets()
      .into_iter()
      .filter(|to| *to != from)
      .collect();
    for to in targets {
      let node = &self.nodes[at];
      if let Some(append) = node.replicate_to(to, APPEND_BUDGET) {
        let reaches = append.prev_log_index + u64::try_from(append.entries.len()).unwrap();
        if reaches < node.last_log_index() {
          self.counters.bounded_batches += 1;
        }
        self.send(from, to, RaftMessage::AppendEntries(append));
      } else if let Some(mut snapshot) = node.install_snapshot_for(to) {
        self.counters.snapshots_sent += 1;
        if std::mem::take(&mut self.corrupt_next_snapshot) {
          snapshot.state.pop();
        }
        self.send(from, to, RaftMessage::InstallSnapshot(snapshot));
      }
    }
    // A transfer whose target has caught up: its invitation rides with the replication (thesis §3.10).
    if let Some((to, invitation)) = self.nodes[at].take_timeout_now() {
      self.counters.invitations_sent += 1;
      self.send(from, to, RaftMessage::TimeoutNow(invitation));
    }
  }

  /// The leader, if any, starts a leadership transfer to a voter `rng` picks (refusals — itself, a transfer
  /// already in flight — are part of the exploration).
  fn transfer(&mut self, rng: &mut Rng) {
    let Some(at) = self.nodes.iter().position(RaftNode::is_leader) else {
      return;
    };
    let voters = self.nodes[at].all_voters();
    if voters.is_empty() {
      return;
    }
    let target = voters[rng.below(voters.len())];
    if self.nodes[at].transfer_leadership(target).is_ok() {
      self.counters.transfers_started += 1;
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
      RaftMessage::AppendReply(reply) => {
        if reply.conflict_index > 0 {
          self.counters.conflict_hints += 1;
        }
        self.nodes[at].on_append_reply(reply);
      }
      RaftMessage::InstallSnapshot(snapshot) => {
        outgoing.push(RaftMessage::InstallSnapshotReply(
          self.install(at, snapshot),
        ));
      }
      RaftMessage::InstallSnapshotReply(reply) => {
        if reply.match_index > 0 {
          self.counters.snapshots_installed += 1;
        }
        self.nodes[at].on_install_snapshot_reply(reply);
      }
      RaftMessage::TimeoutNow(invitation) => {
        let votes = self.nodes[at].on_timeout_now(invitation);
        if !votes.is_empty() {
          self.counters.invited_elections += 1;
        }
        outgoing.extend(votes.into_iter().map(RaftMessage::RequestVote));
      }
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

  /// Node `at` handles a leader's snapshot as the groups do (`crate::fold`): the state is decoded first — here
  /// the committed history through the snapshot's index — and a state that does not decode, or does not
  /// reach that index, is declined.
  fn install(
    &mut self,
    at: usize,
    snapshot: InstallSnapshot,
  ) -> slates_cluster::raft::InstallSnapshotReply {
    let whole = decode_history(&snapshot.state)
      .is_some_and(|history| u64::try_from(history.len()).unwrap() == snapshot.last_included_index);
    if whole {
      self.nodes[at].on_install_snapshot(snapshot)
    } else {
      self.counters.snapshots_declined += 1;
      self.nodes[at].decline_snapshot(&snapshot)
    }
  }

  /// Node `at` compacts its committed prefix to a point `rng` picks above its snapshot — any legal point, at
  /// any step, is the adversary's choice — with the history through it as the snapshot's state.
  fn compact(&mut self, rng: &mut Rng, at: usize) {
    let node = &self.nodes[at];
    let (snapshot, commit) = (node.snapshot_index(), node.commit_index());
    if commit <= snapshot {
      return;
    }
    let up_to =
      snapshot + 1 + u64::try_from(rng.below(usize::try_from(commit - snapshot).unwrap())).unwrap();
    let history = full_log(&node.saved());
    let state = encode_history(&history[..usize::try_from(up_to).unwrap()]);
    assert!(
      self.nodes[at].compact(up_to, state),
      "a committed prefix compacts"
    );
    self.retain(at);
    self.counters.compactions += 1;
  }

  /// Arms the corruption of the next snapshot a leader ships: its state loses its last byte in flight, so it
  /// no longer decodes whole. (Corrupting one already in flight found one in about one seed in five —
  /// snapshots are rare in the bag at any moment — which left the decline path nearly unexplored: 0 and 5
  /// declines over 24 seeds, measured 2026-09-28.)
  fn corrupt_snapshot(&mut self) {
    self.corrupt_next_snapshot = true;
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
    let roll = rng.below(ACTIONS);
    if calm && matches!(roll, 50..=57 | 92..=97 | 103) {
      return;
    }
    match roll {
      0..=49 => self.network_tick(rng),
      50..=54 => self.drop_one(rng),
      55..=57 => self.duplicate_one(rng),
      58..=65 => {
        let at = rng.below(self.nodes.len());
        // In a calm stretch a node's election timer fires only when it has lost its leader, as a healthy
        // timer does; an adversarial stretch fires it anywhere (a paused process, a skewed clock). Measured
        // 2026-09-28 against firing anywhere in both stretches (three / five voters): transfer invitations
        // 7,714 / 9,170 against 2,247 / 4,805, pre-vote refusals still 78,722 / 190,047 from the adversarial
        // stretches.
        if calm && self.nodes[at].leader().is_some() {
          return;
        }
        self.time_out(at);
      }
      // Heartbeats against CheckQuorum ticks at 8:1 (16 : 2 of 100), near a deployment's ten heartbeats per
      // election timeout; 3.5:1 (14 : 4) was the first setting. The measurement in the calm-timer comment
      // above covers the two changes together.
      66..=79 | 90..=91 => {
        if let Some(at) = self.nodes.iter().position(RaftNode::is_leader) {
          self.heartbeat(at);
        }
      }
      80..=87 => self.propose(),
      88..=89 => self.check_quorum_everywhere(),
      92..=94 => {
        let at = rng.below(self.nodes.len());
        self.crash(at);
      }
      95..=97 => self.isolate_or_heal(rng),
      98..=99 => self.transfer(rng),
      100..=102 => {
        let at = rng.below(self.nodes.len());
        self.compact(rng, at);
      }
      103 => self.corrupt_snapshot(),
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
    let logs: Vec<(HostId, Whole)> = self
      .nodes
      .iter()
      .map(|node| {
        let saved = node.saved();
        (
          node.id(),
          Whole {
            commit_index: saved.commit_index,
            log: full_log(&saved),
          },
        )
      })
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

  /// State Machine Safety: every committed entry agrees with the first seen committed at its index — the
  /// entries a snapshot holds included, since a snapshot is committed history.
  fn check_state_machine_safety(&mut self, logs: &[(HostId, Whole)], at: &str) {
    for (id, whole) in logs {
      for index in 1..=whole.commit_index {
        let entry = &whole.log[usize::try_from(index - 1).unwrap()];
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
  fn check_leader_completeness(&mut self, logs: &[(HostId, Whole)], at: &str) {
    for (id, whole) in logs {
      let node = &self.nodes[self.position(*id)];
      if !node.is_leader() || !self.completeness_checked.insert((*id, node.term())) {
        continue;
      }
      for (index, entry) in &self.committed {
        let held = usize::try_from(index - 1)
          .ok()
          .and_then(|position| whole.log.get(position));
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

/// A node's whole log for the checks: its snapshot's history followed by the entries above it, and its commit
/// index.
struct Whole {
  commit_index: u64,
  log: Vec<LogEntry>,
}

/// The whole log `saved` describes: the history its snapshot carries (every entry through the snapshot's
/// index) followed by the entries above it.
fn full_log(saved: &SavedRaft) -> Vec<LogEntry> {
  let mut log = if saved.snapshot_index == 0 {
    Vec::new()
  } else {
    decode_history(&saved.snapshot_data).expect("a retained snapshot decodes")
  };
  assert_eq!(
    u64::try_from(log.len()).unwrap(),
    saved.snapshot_index,
    "a snapshot carries every entry through its index"
  );
  log.extend(saved.log.iter().cloned());
  log
}

/// The explorer's snapshot state: every entry's term, command length and command, little-endian.
fn encode_history(entries: &[LogEntry]) -> Vec<u8> {
  let mut out = Vec::new();
  for entry in entries {
    out.extend_from_slice(&entry.term.to_le_bytes());
    out.extend_from_slice(&u32::try_from(entry.command.len()).unwrap().to_le_bytes());
    out.extend_from_slice(&entry.command);
  }
  out
}

/// The history a snapshot state carries, or `None` when the bytes do not decode whole.
fn decode_history(bytes: &[u8]) -> Option<Vec<LogEntry>> {
  let mut entries = Vec::new();
  let mut rest = bytes;
  while !rest.is_empty() {
    let (term, tail) = rest.split_at_checked(8)?;
    let (length, tail) = tail.split_at_checked(4)?;
    let length = usize::try_from(u32::from_le_bytes(length.try_into().ok()?)).ok()?;
    let (command, tail) = tail.split_at_checked(length)?;
    entries.push(LogEntry::command(
      u64::from_le_bytes(term.try_into().ok()?),
      command.to_vec(),
    ));
    rest = tail;
  }
  Some(entries)
}

/// Log Matching, pairwise: two logs holding an entry of the same term at an index agree on it and on every
/// entry before it.
fn check_log_matching(logs: &[(HostId, Whole)], at: &str) {
  for (left_id, left) in logs {
    for (right_id, right) in logs {
      if left_id >= right_id {
        continue;
      }
      let (left, right) = (&left.log, &right.log);
      let shared = left.len().min(right.len());
      let matched = (0..shared)
        .rev()
        .find(|position| left[*position].term == right[*position].term);
      if let Some(position) = matched {
        assert_eq!(
          left[..=position],
          right[..=position],
          "{at}: {left_id:?} and {right_id:?} share a term at index {} but differ before it",
          position + 1
        );
      }
    }
  }
}

/// Explores `seeds` seeded histories of a cluster of `size` voters and returns what they counted.
fn explore(size: u64, seeds: u64) -> Counters {
  let mut total = Counters::default();
  for seed in 0..seeds {
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
    total.transfers_started += c.transfers_started;
    total.invitations_sent += c.invitations_sent;
    total.invited_elections += c.invited_elections;
    total.compactions += c.compactions;
    total.snapshots_sent += c.snapshots_sent;
    total.snapshots_installed += c.snapshots_installed;
    total.snapshots_declined += c.snapshots_declined;
    total.bounded_batches += c.bounded_batches;
    total.conflict_hints += c.conflict_hints;
  }
  total
}

/// Explores three and five voters over `seeds` histories each and holds every non-vacuity floor: each path
/// the exploration claims to cover must have been reached at least once per explored seed.
fn explore_and_check_coverage(seeds: u64) {
  for size in [3, 5] {
    let counted = explore(size, seeds);
    eprintln!("explored {size} voters x {seeds} seeds x {STEPS} steps: {counted:?}");
    let floors = [
      (counted.elections_won, "elections were won"),
      (counted.commits, "entries committed"),
      (counted.crashes, "nodes crashed and recovered"),
      (counted.pre_votes_refused, "pre-votes were refused"),
      (counted.transfers_started, "leadership transfers started"),
      (counted.invitations_sent, "transfer invitations went out"),
      (counted.invited_elections, "invitations started elections"),
      (counted.compactions, "logs were compacted"),
      (
        counted.snapshots_sent,
        "snapshots went to followers whose entries were compacted away",
      ),
      (counted.snapshots_installed, "snapshots were installed"),
      (
        counted.snapshots_declined,
        "corrupted snapshots were declined",
      ),
      (counted.bounded_batches, "the budget bounded appends"),
      (counted.conflict_hints, "refusals carried conflict hints"),
    ];
    for (count, path) in floors {
      assert!(
        count > seeds,
        "{size} voters: {path} ({count} over {seeds} seeds)"
      );
    }
  }
}

/// T-8.13 (§4.8 mechanism 2; `docs/wip/research/consensus-enhancements.md` §5): the dialect keeps Election
/// Safety, Log Matching, Leader Completeness and State Machine Safety in every explored history of three
/// and five voters under loss, duplication, reordering, partitions, crash-restarts and leadership transfers
/// (thesis §3.10) — and the histories did exercise elections, commits, crashes, pre-vote refusals, and
/// transfers started, invited and elected (non-vacuity). The workspace's scale ([`SEEDS_QUICK`]).
#[test]
fn the_dialect_keeps_raft_safety_under_an_adversarial_network() {
  explore_and_check_coverage(SEEDS_QUICK);
}

/// T-8.13 at full scale ([`SEEDS_FULL`]), run by CI in release with `--ignored`.
#[test]
#[ignore = "full scale: CI runs it in release (`cargo test -p slates-cluster --release --test explore -- --ignored`)"]
fn the_dialect_keeps_raft_safety_at_full_scale() {
  explore_and_check_coverage(SEEDS_FULL);
}
