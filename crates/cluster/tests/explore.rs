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
//! Priority is explored (`docs/wip/research/consensus-enhancements.md` §3.4): every node holds a distinct
//! random election priority for its history, and a leader's CheckQuorum tick hands leadership to a node that
//! outranks it once it has led a whole window — so priority transfers interleave with every fault above.
//!
//! Membership changes are explored (Raft §6, thesis §4.2.1): every history has one spare node beyond its
//! voters, and the leader's reconfiguration move — the groups' `reconcile_voters` rule — brings a non-voter
//! in through staging (caught up in rounds before the joint change may begin) while there is room for one
//! more voter, takes a random voter out (the leader included, which then steps down, §4.2.2) when there is
//! not, and completes a joint change once its entry commits. The model-level exploration of the Fast Raft
//! and ParallelRaft log shapes extends this driver. Test by use (R5): the real core, the council's drive,
//! observable outcomes.

// Test harness: an unwrap, expect or panic here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};

use slates_cluster::raft::{
  CatchUp, ElectionPriority, InstallSnapshot, LogEntry, RaftNode, SavedRaft,
};
use slates_cluster::raft_wire::RaftMessage;
use slates_db::register::HostId;

/// Shape: the seeds each cluster size is explored under at full scale — the `--ignored` run CI makes in
/// release ("T-8.13 Raft safety explorer at full scale"): 400 seeds × 4,000 steps × 2 sizes took 27.7 s there
/// with compaction and membership changes explored (measured 2026-09-28, Apple M5 Max; 14 s before them),
/// so the workspace's debug run explores [`SEEDS_QUICK`] instead.
const SEEDS_FULL: u64 = 400;
/// Shape: the seeds the workspace's debug run explores — 20.5 s of a debug build (2026-09-28), and still
/// enough histories for every non-vacuity floor below (each is at least one event per seed but the rare
/// staging abort's).
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
/// Shape: the range of the explorer's random quorum round trips, and their spread — wide enough that most
/// pairs of nodes outrank one another and a few tie.
const PRIORITY_FLOOR_MS: u64 = 50;
const PRIORITY_CEILING_MS: u64 = 250;
const PRIORITY_SPREAD_MS: u64 = 5;
/// Shape: the events a replayed history keeps for its diagnosis — the last few hundred steps' worth.
const TRACE_BOUND: usize = 2_000;
/// Shape: the fewest voters a reconfiguration leaves — three, the smallest group that tolerates a failure.
const MIN_VOTERS: usize = 3;
/// Shape: the entry bytes one append carries — about two of the explorer's command entries (21 wire bytes
/// each: term, length, an eight-byte command, the configuration flag) — so catching a follower up takes
/// several batches and every batch boundary is explored.
const APPEND_BUDGET: usize = 48;
/// Shape: the actions one step chooses among — the hundred of the original mix and eight more: three for a
/// compaction, one for corrupting a snapshot in flight, four for a reconfiguration period.
const ACTIONS: usize = 108;

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
  /// Joint membership changes begun (Raft §6).
  changes_begun: u64,
  /// Joint membership changes completed (`C_new` appended).
  changes_completed: u64,
  /// Stagings that caught a member up before its change began (thesis §4.2.1).
  members_caught_up: u64,
  /// Stagings aborted: the member's lag did not shrink for a whole window.
  stagings_aborted: u64,
  /// Leadership transfers started by priority (§3.4).
  priority_transfers: u64,
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
  /// The last events of a replayed history, for diagnosing a violation ([`replay_to_the_first_violation`]);
  /// `None` in an ordinary exploration, which formats nothing.
  trace: Option<std::collections::VecDeque<String>>,
  /// Each node's election priority for the history (§3.4), set again after a crash-restart.
  priorities: BTreeMap<HostId, ElectionPriority>,
}

impl Cluster {
  /// `size` voters and one spare node beyond them, a non-voter until a reconfiguration brings it in.
  fn new(size: u64) -> Cluster {
    let voters: Vec<HostId> = (1..=size).map(HostId).collect();
    let nodes: Vec<RaftNode> = (1..=size + 1)
      .map(|id| RaftNode::new(HostId(id), voters.clone()))
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
      trace: None,
      priorities: BTreeMap::new(),
    }
  }

  /// Records an event of a replayed history (bounded to [`TRACE_BOUND`], the oldest dropped).
  fn note(&mut self, event: impl FnOnce() -> String) {
    if let Some(trace) = &mut self.trace {
      if trace.len() >= TRACE_BOUND {
        trace.pop_front();
      }
      trace.push_back(event());
    }
  }

  /// A one-line summary of every node: role, term, commit and snapshot, voters, joint, last log entries.
  fn summary(&self) -> String {
    self
      .nodes
      .iter()
      .map(|node| {
        let saved = node.saved();
        let tail: Vec<String> = full_log(&saved)
          .iter()
          .enumerate()
          .skip(full_log(&saved).len().saturating_sub(6))
          .map(|(i, entry)| {
            format!(
              "{}:t{}{}",
              i + 1,
              entry.term,
              entry
                .config
                .as_ref()
                .map_or(String::new(), |config| format!(
                  "/cfg{:?}+{:?}",
                  config.voters, config.joint
                ))
            )
          })
          .collect();
        format!(
          "{:?} {:?} t{} commit {} snap {} voters {:?} joint {} staged {:?} tail [{}]",
          node.id(),
          node.role(),
          node.term(),
          node.commit_index(),
          node.snapshot_index(),
          node.all_voters(),
          node.in_joint_configuration(),
          node.staged(),
          tail.join(" ")
        )
      })
      .collect::<Vec<_>>()
      .join("\n")
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
    let id = self.nodes[at].id();
    self.note(|| format!("time out {id:?}"));
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
    self.note(|| {
      format!(
        "deliver {:?} -> {:?}: {:?}",
        flight.from, flight.to, flight.message
      )
    });
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

  /// One reconfiguration period of the leader, if there is one — the groups' `reconcile_voters` rule: complete
  /// a joint change once its entry has committed; otherwise, with the log committed, bring a random non-voter
  /// in through staging while the voters number fewer than the nodes, or take a random voter out (never
  /// below three) when every node votes.
  fn reconfigure(&mut self, rng: &mut Rng) {
    let Some(at) = self.nodes.iter().position(RaftNode::is_leader) else {
      return;
    };
    let node = &mut self.nodes[at];
    if node.in_joint_configuration() {
      let completed = node.complete_membership_change();
      if completed {
        self.counters.changes_completed += 1;
      }
      let leader_id = node.id();
      self.note(|| format!("complete at {leader_id:?}: {completed}"));
      self.retain(at);
      return;
    }
    if node.last_log_index() != node.commit_index() {
      return;
    }
    let voters = node.all_voters();
    let voters_before = voters.len();
    let outside: Vec<HostId> = self
      .nodes
      .iter()
      .map(RaftNode::id)
      .filter(|id| !voters.contains(id))
      .collect();
    let target: Vec<HostId> = if !outside.is_empty() {
      let mut target = voters;
      target.push(outside[rng.below(outside.len())]);
      target.sort_unstable_by_key(|id| id.0);
      target
    } else if voters.len() > MIN_VOTERS {
      let out = voters[rng.below(voters.len())];
      voters.into_iter().filter(|id| *id != out).collect()
    } else {
      return;
    };
    let node = &mut self.nodes[at];
    let outcome = node.catch_up(&target);
    let leader_id = node.id();
    self.note(|| format!("reconfigure at {leader_id:?} toward {target:?}: {outcome:?}"));
    let node = &mut self.nodes[at];
    match outcome {
      CatchUp::Ready => {
        if node.begin_membership_change(target.clone()) {
          self.counters.changes_begun += 1;
          if target.len() > voters_before {
            self.counters.members_caught_up += 1;
          }
        }
      }
      CatchUp::Aborted { .. } => self.counters.stagings_aborted += 1,
      CatchUp::Pending | CatchUp::NotLeader => {}
    }
    self.retain(at);
  }

  /// Crashes node `at` and restarts it from what it last retained.
  fn crash(&mut self, at: usize) {
    let id = self.nodes[at].id();
    self.note(|| format!("crash {id:?}"));
    self.nodes[at] =
      RaftNode::restore(self.retained[at].clone()).expect("a retained state restores");
    // A priority is measured, not retained: the restarted node measures the same paths again.
    if let Some(priority) = self.priorities.get(&id) {
      self.nodes[at].set_priority(*priority);
    }
    self.counters.crashes += 1;
  }

  /// Gives every node a distinct random election priority for the history (§3.4): quorum round trips
  /// between [`PRIORITY_FLOOR_MS`] and [`PRIORITY_CEILING_MS`], each with a spread of
  /// [`PRIORITY_SPREAD_MS`] — some pairs outrank, some tie.
  fn assign_priorities(&mut self, rng: &mut Rng) {
    for node in &mut self.nodes {
      let span = usize::try_from(PRIORITY_CEILING_MS - PRIORITY_FLOOR_MS).unwrap();
      let quorum_ms = PRIORITY_FLOOR_MS + u64::try_from(rng.below(span)).unwrap();
      let priority = ElectionPriority {
        quorum_ns: quorum_ms * 1_000_000,
        spread_ns: PRIORITY_SPREAD_MS * 1_000_000,
      };
      node.set_priority(priority);
      self.priorities.insert(node.id(), priority);
    }
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
      104..=107 => self.reconfigure(rng),
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
    let all: Vec<HostId> = self.nodes.iter().map(RaftNode::id).collect();
    for node in &mut self.nodes {
      if node.is_leader() {
        node.check_quorum();
        if node.priority_transfer(&all).is_some() {
          self.counters.priority_transfers += 1;
        }
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

/// The explorer's snapshot state: every entry whole — its term, its command (length-prefixed) and its
/// configuration (a presence byte, then the voter set and the joint set's presence byte and set), little-
/// endian. A configuration entry must survive the round trip: the first form kept only terms and commands,
/// so a compacted joint-configuration entry came back a plain empty command and the State Machine Safety
/// check flagged the compacting node (seed 0, step 3,018, 2026-09-28 — the model's fault, not Raft's).
fn encode_history(entries: &[LogEntry]) -> Vec<u8> {
  let hosts = |out: &mut Vec<u8>, set: &[HostId]| {
    out.extend_from_slice(&u32::try_from(set.len()).unwrap().to_le_bytes());
    for host in set {
      out.extend_from_slice(&host.0.to_le_bytes());
    }
  };
  let mut out = Vec::new();
  for entry in entries {
    out.extend_from_slice(&entry.term.to_le_bytes());
    out.extend_from_slice(&u32::try_from(entry.command.len()).unwrap().to_le_bytes());
    out.extend_from_slice(&entry.command);
    match &entry.config {
      None => out.push(0),
      Some(config) => {
        out.push(1);
        hosts(&mut out, &config.voters);
        match &config.joint {
          None => out.push(0),
          Some(joint) => {
            out.push(1);
            hosts(&mut out, joint);
          }
        }
      }
    }
  }
  out
}

/// A host set from the front of `bytes`: its count, then each id.
fn take_hosts(bytes: &[u8]) -> Option<(Vec<HostId>, &[u8])> {
  let (count, mut rest) = bytes.split_at_checked(4)?;
  let count = usize::try_from(u32::from_le_bytes(count.try_into().ok()?)).ok()?;
  let mut hosts = Vec::new();
  for _ in 0..count {
    let (id, tail) = rest.split_at_checked(8)?;
    hosts.push(HostId(u64::from_le_bytes(id.try_into().ok()?)));
    rest = tail;
  }
  Some((hosts, rest))
}

/// A presence byte from the front of `bytes`, or `None` when it is neither zero nor one.
fn take_flag(bytes: &[u8]) -> Option<(bool, &[u8])> {
  let (&flag, rest) = bytes.split_first()?;
  match flag {
    0 => Some((false, rest)),
    1 => Some((true, rest)),
    _ => None,
  }
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
    let (has_config, tail) = take_flag(tail)?;
    let (config, tail) = if has_config {
      let (voters, tail) = take_hosts(tail)?;
      let (has_joint, tail) = take_flag(tail)?;
      let (joint, tail) = if has_joint {
        let (joint, tail) = take_hosts(tail)?;
        (Some(joint), tail)
      } else {
        (None, tail)
      };
      (
        Some(slates_cluster::raft::VoterConfig { voters, joint }),
        tail,
      )
    } else {
      (None, tail)
    };
    entries.push(LogEntry {
      term: u64::from_le_bytes(term.try_into().ok()?),
      command: command.to_vec(),
      config,
    });
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
    cluster.assign_priorities(&mut rng);
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
    total.changes_begun += c.changes_begun;
    total.changes_completed += c.changes_completed;
    total.members_caught_up += c.members_caught_up;
    total.stagings_aborted += c.stagings_aborted;
    total.priority_transfers += c.priority_transfers;
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
      (counted.changes_begun, "membership changes began"),
      (counted.changes_completed, "membership changes completed"),
      (
        counted.members_caught_up,
        "added members were caught up before their change",
      ),
      (counted.priority_transfers, "leaders handed off by priority"),
    ];
    for (count, path) in floors {
      assert!(
        count > seeds,
        "{size} voters: {path} ({count} over {seeds} seeds)"
      );
    }
    // A staging aborts only when its member is cut off for a whole CheckQuorum window — 12 of 24 seeds at
    // three voters, 4 of 24 at five (2026-09-28) — so this path is floored at once per exploration.
    assert!(
      counted.stagings_aborted > 0,
      "{size} voters: a staging aborted"
    );
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

/// Diagnosis: replays one seeded history of `SLATES_EXPLORE_SIZE` voters (seed `SLATES_EXPLORE_SEED`) with
/// its trace on, and at the first step whose check fails prints the trace — the last events and every
/// node's state after each step — to standard error before failing (R1: a test writes no host path; the
/// caller redirects it). Run by hand: `SLATES_EXPLORE_SIZE=3 SLATES_EXPLORE_SEED=0 cargo test -p
/// slates-cluster --test explore replay_to_the_first_violation -- --ignored --nocapture 2> <file>`. Without
/// its environment it skips loudly and passes, so an `--ignored` run of the whole file never fails on it
/// (CI run 36511884967 did, `3316fc0`).
#[test]
#[ignore = "a diagnosis tool, run by hand with its environment set"]
fn replay_to_the_first_violation() {
  let (Some(size), Some(seed)) = (
    std::env::var("SLATES_EXPLORE_SIZE")
      .ok()
      .and_then(|size| size.parse::<u64>().ok()),
    std::env::var("SLATES_EXPLORE_SEED")
      .ok()
      .and_then(|seed| seed.parse::<u64>().ok()),
  ) else {
    eprintln!(
      "skipping the replay: set SLATES_EXPLORE_SIZE and SLATES_EXPLORE_SEED to replay one history"
    );
    return;
  };
  let mut rng = Rng(seed ^ (size << 32));
  let mut cluster = Cluster::new(size);
  cluster.assign_priorities(&mut rng);
  cluster.trace = Some(std::collections::VecDeque::new());
  for step in 0..STEPS {
    let calm = (step / STRETCH) % 2 == 1;
    cluster.note(|| format!("== step {step} (calm {calm})"));
    cluster.step(&mut rng, calm);
    let summary = cluster.summary();
    cluster.note(|| summary);
    let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      cluster.check(seed, step);
    }));
    if checked.is_err() {
      for line in cluster.trace.take().unwrap() {
        eprintln!("{line}");
      }
      panic!("violation at step {step}; the trace precedes this line");
    }
  }
}
