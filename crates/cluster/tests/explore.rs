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
//! not, and completes a joint change once its entry commits.
//!
//! The fast track and the window are explored (research record §3.5, §3.7 and §4; the design the prefix
//! model verifies): every node holds a window as large as one append, so a leader pipelines to each follower
//! whose place it has confirmed — each heartbeat move sends the next batch before the last is acknowledged,
//! as far as that window holds — and a follower buffers what arrives ahead of a hole; a leader may open its
//! term's fast track at any step; a client's command then reaches any node, which proposes it to every
//! voter, and each voter's vote goes to its own leader; and a leader may fill the index its fast track
//! stalled at, when lost votes left it short of a quorum. A ghost of every vote cast — the Paxos acceptors' state, counted from
//! the moment a vote is cast, whatever becomes of its message — marks each index a fast quorum chose, and
//! the checks take the dialect's form around it:
//!
//! - **Fast agreement**: no two commands are ever chosen at one index.
//! - **State Machine Safety** and **Leader Completeness** compare commands and configurations, and terms too
//!   except at a chosen index: a leader applies its fast commit under its own term, and a successor that
//!   lacks it re-proposes it under the successor's. A new leader also holds every chosen command.
//! - **Log Matching** stays Raft's own, terms and all: a follower's committed prefix is classic.
//! - Every log's terms never decrease, and every window stays within its budget and above its node's commit
//!   index.
//!
//! Test by use (R5): the real core, the council's drive, observable outcomes.

// Test harness: an unwrap, expect or panic here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};

use slates_cluster::raft::{
  CatchUp, ElectionPriority, FastVote, InstallSnapshot, LogEntry, RaftNode, SavedRaft,
  WindowCounters, fast_quorum,
};
use slates_cluster::raft_wire::RaftMessage;
use slates_db::register::HostId;

/// Shape: the seeds each cluster size is explored under at full scale — the `--ignored` run CI makes in
/// release ("T-8.13 Raft safety explorer at full scale"): 400 seeds × 4,000 steps × 2 sizes took 26.1 s with
/// the fast track, its filled holes, pipelining and windows of three sizes explored (measured 2026-09-29, Apple
/// M5 Max, release; 27.7 s on 2026-09-28 with compaction and membership changes, 14 s before them), so the
/// workspace's debug run explores [`SEEDS_QUICK`] instead.
const SEEDS_FULL: u64 = 400;
/// Shape: the seeds the workspace's debug run explores — 18.6 s of a debug build (2026-09-29), and still
/// enough histories for every non-vacuity floor below (each is at least one event per seed but the three
/// rare paths', floored once per exploration).
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
/// Shape: the actions one step chooses among — the hundred of the original mix and ten more: three for a
/// compaction, one for corrupting a snapshot in flight, four for a reconfiguration period, one for the leader
/// opening its fast track, one for the leader filling the index its fast track stalled at.
const ACTIONS: usize = 110;
/// Shape: the windows a node may hold, in wire bytes: one, one and a half and two of the append budget — the
/// groups set a window from the measured paths, so windows differ across a group — each a span of 3, 5 or 7
/// indices above a node's log and room for two, three or four of the explorer's command entries (21 wire bytes
/// each), so the bound is met often and a leader's recovery meets voters whose windows reach further than its
/// own. A half-append window (one entry) was tried first: a node holding it could rarely vote, and three
/// voters' fast commits fell below one per seed (21 over 24, 2026-09-29).
const WINDOW_BUDGETS: [usize; 3] = [APPEND_BUDGET, APPEND_BUDGET * 3 / 2, APPEND_BUDGET * 2];

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
  /// Fast tracks a leader opened (research record §3.7).
  fast_tracks_opened: u64,
  /// Commands proposed on a fast track, each to every voter.
  fast_proposals: u64,
  /// Fast votes cast.
  fast_votes_cast: u64,
  /// Fast votes whose voter knew no leader to send them to.
  fast_votes_unrouted: u64,
  /// Indices a leader's fast track stalled at, filled by the leader (`RaftNode::fill_hole`).
  stalled_fills: u64,
  /// Indices a fast quorum chose (the ghost the checks read).
  fast_choices: u64,
  /// Indices committed under two terms — a fast choice committed by its leader and re-proposed by a successor
  /// (§4's recovery), which the dialect's State Machine Safety allows at a chosen index only.
  re_proposed_commits: u64,
  /// What the nodes' windows did, summed over every node's life (a crash restarts a node's count, so each
  /// node's is added before it crashes).
  window: WindowCounters,
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
  /// Each node's window for the history, set again after a crash-restart.
  windows: BTreeMap<HostId, usize>,
  /// The voters of each term whose leader opened its fast track, as that leader counted them when it did.
  fast_terms: BTreeMap<u64, usize>,
  /// Every fast vote cast, by term and index: each voter's command (the ghost of the Paxos acceptors' state,
  /// which the checks read — a vote counts from the moment it is cast, whether or not its message arrives).
  votes_cast: BTreeMap<(u64, u64), BTreeMap<HostId, Vec<u8>>>,
  /// The command a fast quorum chose at each index, over the whole history.
  chosen: BTreeMap<u64, Vec<u8>>,
  /// The indices committed under two terms (see [`Counters::re_proposed_commits`]).
  re_proposed: BTreeSet<u64>,
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
      windows: BTreeMap::new(),
      fast_terms: BTreeMap::new(),
      votes_cast: BTreeMap::new(),
      chosen: BTreeMap::new(),
      re_proposed: BTreeSet::new(),
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
    let pre_votes = self.nodes[at].on_election_timeout().unwrap();
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
      let node = &mut self.nodes[at];
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
      // The fast track (research record §3.7): a proposal is answered with the voter's vote, cast the moment
      // it is returned; a vote the leader receives is tallied.
      RaftMessage::FastPropose(proposal) => {
        if let Some(vote) = self.nodes[at].on_fast_propose(proposal) {
          self.cast(&vote);
          outgoing.push(RaftMessage::FastVote(vote));
        }
      }
      RaftMessage::FastVote(vote) => self.nodes[at].on_fast_vote(vote),
    }
    self.finish_election(at, was_leader);
    self.retain(at);
    self.route(at, flight.from, outgoing);
  }

  /// Sends what node `at` answered a message from `sender` with: a request's reply goes back to its sender;
  /// the vote requests a granted pre-election yields go to every other voter, one each; a fast vote goes to
  /// the voter's leader, whoever proposed (research record §3.7).
  fn route(&mut self, at: usize, sender: HostId, outgoing: Vec<RaftMessage>) {
    let from = self.nodes[at].id();
    let voters: Vec<HostId> = self.nodes[at]
      .all_voters()
      .into_iter()
      .filter(|voter| *voter != from)
      .collect();
    let mut vote_targets = voters.into_iter();
    for message in outgoing {
      let to = match message {
        RaftMessage::RequestVote(_) => vote_targets.next(),
        RaftMessage::FastVote(_) => {
          let leader = self.nodes[at].leader();
          if leader.is_none() {
            self.counters.fast_votes_unrouted += 1;
          }
          leader
        }
        _ => Some(sender),
      };
      if let Some(to) = to {
        self.send(from, to, message);
      }
    }
  }

  /// Records a fast vote cast (the ghost [`Cluster::votes_cast`]).
  fn cast(&mut self, vote: &FastVote) {
    self.counters.fast_votes_cast += 1;
    self
      .votes_cast
      .entry((vote.term, vote.index))
      .or_default()
      .insert(vote.voter, vote.command.clone());
  }

  /// The leader, if any, fills the index its fast track stalled at, if it has one: a no-op proposed there to
  /// every voter, itself included (the drive does this once a stall has lasted a repair's round trips; here
  /// the adversary chooses when).
  fn fill_a_stalled_hole(&mut self) {
    let Some(at) = self.nodes.iter().position(RaftNode::is_leader) else {
      return;
    };
    let Some(index) = self.nodes[at].stalled_index() else {
      return;
    };
    let Some(fill) = self.nodes[at].fill_hole(index) else {
      return;
    };
    self.counters.stalled_fills += 1;
    let from = self.nodes[at].id();
    for to in self.nodes[at].all_voters() {
      self.send(from, to, RaftMessage::FastPropose(fill.clone()));
    }
  }

  /// The leader, if any, opens its term's fast track (refusals — no sync point yet, a configuration not yet
  /// committed, a transfer in flight, already open — are part of the exploration), and the explorer records
  /// the voters its votes are counted against.
  fn open_fast_track(&mut self) {
    let Some(at) = self.nodes.iter().position(RaftNode::is_leader) else {
      return;
    };
    if self.nodes[at].open_fast_track() {
      let node = &self.nodes[at];
      self.fast_terms.insert(node.term(), node.all_voters().len());
      self.counters.fast_tracks_opened += 1;
      let (id, term) = (node.id(), node.term());
      self.note(|| format!("open fast track at {id:?} t{term}"));
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
    add_window(&mut self.counters.window, self.nodes[at].window_counters());
    self.nodes[at] =
      RaftNode::restore(self.retained[at].clone()).expect("a retained state restores");
    // A priority is measured, not retained: the restarted node measures the same paths again. Its window is
    // the caller's setting from the same paths, not retained either.
    if let Some(priority) = self.priorities.get(&id) {
      self.nodes[at].set_priority(*priority);
    }
    if let Some(window) = self.windows.get(&id) {
      self.nodes[at].set_window_budget(*window);
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

  /// Gives every node a window drawn from [`WINDOW_BUDGETS`] for the history.
  fn assign_windows(&mut self, rng: &mut Rng) {
    for node in &mut self.nodes {
      let window = WINDOW_BUDGETS[rng.below(WINDOW_BUDGETS.len())];
      node.set_window_budget(window);
      self.windows.insert(node.id(), window);
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
      80..=87 => self.propose(rng),
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
      108 => self.open_fast_track(),
      109 => self.fill_a_stalled_hole(),
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

  /// A client's next command (bounded by [`PROPOSALS_BOUND`]) reaches a node `rng` picks: one to which the
  /// fast track is open proposes it to every voter, itself included; otherwise it goes to the leader, if any,
  /// which proposes it on its fast track when that is open and appends it when not (refused while a transfer
  /// is in flight).
  fn propose(&mut self, rng: &mut Rng) {
    if self.proposals >= PROPOSALS_BOUND {
      return;
    }
    let command = (self.proposals + 1).to_le_bytes().to_vec();
    let reached = rng.below(self.nodes.len());
    let leader = self.nodes.iter().position(RaftNode::is_leader);
    for at in std::iter::once(reached).chain(leader) {
      if let Some(proposal) = self.nodes[at].propose_fast(command.clone()) {
        self.proposals += 1;
        self.counters.fast_proposals += 1;
        let from = self.nodes[at].id();
        for to in self.nodes[at].all_voters() {
          self.send(from, to, RaftMessage::FastPropose(proposal.clone()));
        }
        return;
      }
    }
    let Some(at) = leader else {
      return;
    };
    self.proposals += 1;
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
            committed_through: node.committed_through(),
            log: full_log(&saved),
          },
        )
      })
      .collect();
    let at = format!("seed {seed} step {step}");
    self.check_election_safety(&at);
    self.check_fast_agreement(&at);
    self.check_state_machine_safety(&logs, &at);
    self.check_leader_completeness(&logs, &at);
    check_log_matching(&logs, &at);
    self.check_terms_and_windows(&logs, &at);
  }

  /// Fast agreement (Paxos's P2 at the acceptors, from the ghost [`Cluster::votes_cast`]): a command becomes
  /// chosen at its index once a fast quorum of its term's voters has voted it, and no other command is ever
  /// chosen there, in any term.
  fn check_fast_agreement(&mut self, at: &str) {
    for ((term, index), votes) in &self.votes_cast {
      let voters = *self.fast_terms.get(term).unwrap_or_else(|| {
        panic!("{at}: a fast vote in term {term}, whose leader opened no fast track")
      });
      let mut counts: BTreeMap<&Vec<u8>, usize> = BTreeMap::new();
      for command in votes.values() {
        *counts.entry(command).or_default() += 1;
      }
      for (command, count) in counts {
        if count < fast_quorum(voters) {
          continue;
        }
        let first = self.chosen.entry(*index).or_insert_with(|| command.clone());
        assert_eq!(
          first, command,
          "{at}: two commands chosen at index {index} (the second in term {term})"
        );
      }
    }
    self.counters.fast_choices = u64::try_from(self.chosen.len()).unwrap();
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
  /// entries a snapshot holds included, since a snapshot is committed history, and a leader's fast commits,
  /// which it applies and acknowledges. In the dialect's form: the command and configuration always, the term
  /// too except at an index a fast quorum chose, where a successor that re-proposed the choice commits it
  /// under its own term (§4's recovery) and the fast-committing leader under its.
  fn check_state_machine_safety(&mut self, logs: &[(HostId, Whole)], at: &str) {
    for (id, whole) in logs {
      for index in 1..=whole.committed_through {
        let entry = &whole.log[usize::try_from(index - 1).unwrap()];
        let first = self.committed.entry(index).or_insert_with(|| entry.clone());
        assert!(
          same_command(first, entry),
          "{at}: {id:?} committed a different command at index {index}: {entry:?} where {first:?} was"
        );
        if first.term != entry.term {
          assert!(
            self.chosen.contains_key(&index),
            "{at}: {id:?} committed index {index} under term {} where term {} committed it, and no fast \
             quorum chose it",
            entry.term,
            first.term
          );
          self.re_proposed.insert(index);
        }
      }
    }
    self.counters.commits = u64::try_from(self.committed.len()).unwrap();
    self.counters.re_proposed_commits = u64::try_from(self.re_proposed.len()).unwrap();
  }

  /// Leader Completeness: a leader holds every entry committed so far — in the dialect's form, as State
  /// Machine Safety compares them — and every command a fast quorum chose, at its index (the prefix model's
  /// `LeaderIncomplete`), once its recovery is done; checked once per leadership, since the property binds a
  /// node when it takes up leading, not a deposed one yet to hear of it. A leader materializes a recovery that
  /// reaches far above its log a slice at a time (AUD-29-37) and holds the recovered commands only at its
  /// end; meanwhile it takes no proposal, membership change or read, and the checks run every step — State
  /// Machine Safety and fast agreement — hold it to committing nothing that contradicts them.
  fn check_leader_completeness(&mut self, logs: &[(HostId, Whole)], at: &str) {
    for (id, whole) in logs {
      let node = &self.nodes[self.position(*id)];
      if !node.is_leader()
        || node.recovering()
        || !self.completeness_checked.insert((*id, node.term()))
      {
        continue;
      }
      let held_at = |index: u64| {
        usize::try_from(index - 1)
          .ok()
          .and_then(|position| whole.log.get(position))
      };
      for (index, entry) in &self.committed {
        let held = held_at(*index);
        assert!(
          held.is_some_and(|held| same_command(held, entry)
            && (held.term == entry.term || self.chosen.contains_key(index))),
          "{at}: leader {id:?} of term {} lacks committed index {index}: holds {held:?}, committed {entry:?}",
          node.term()
        );
      }
      for (index, command) in &self.chosen {
        let held = held_at(*index);
        assert!(
          held.is_some_and(|held| held.command == *command && held.config.is_none()),
          "{at}: leader {id:?} of term {} lacks the command a fast quorum chose at index {index}: holds {held:?}",
          node.term()
        );
      }
    }
  }

  /// Two structural invariants of every node: its log's terms never decrease (its recovery refuses a log
  /// whose do), and its window holds slots only above its commit index and within its budget (no unbounded
  /// growth).
  fn check_terms_and_windows(&self, logs: &[(HostId, Whole)], at: &str) {
    for (id, whole) in logs {
      let terms: Vec<u64> = whole.log.iter().map(|entry| entry.term).collect();
      assert!(
        terms.windows(2).all(|pair| pair[0] <= pair[1]),
        "{at}: {id:?}'s log terms decrease: {terms:?}"
      );
      let window = self.nodes[self.position(*id)].window();
      let held: usize = window
        .iter()
        .map(|report| report.slot.entry.encoded_len())
        .sum();
      let budget = self.windows.get(id).copied().unwrap_or(0);
      assert!(
        held <= budget,
        "{at}: {id:?}'s window holds {held} bytes, past its budget {budget}"
      );
      assert!(
        window
          .iter()
          .all(|report| report.index > whole.commit_index),
        "{at}: {id:?} keeps a window slot at or below its commit index {}",
        whole.commit_index
      );
    }
  }
}

/// Whether two entries carry the same command and configuration — the state machine's input, whatever the
/// term they were appended under.
fn same_command(left: &LogEntry, right: &LogEntry) -> bool {
  left.command == right.command && left.config == right.config
}

/// Adds `more` to `total`, path by path.
fn add_window(total: &mut WindowCounters, more: WindowCounters) {
  total.recovered += more.recovered;
  total.recovered_fast_choices += more.recovered_fast_choices;
  total.recovered_beyond_reach += more.recovered_beyond_reach;
  total.holes_filled += more.holes_filled;
  total.pruned += more.pruned;
  total.buffered += more.buffered;
  total.absorbed += more.absorbed;
  total.decided_from_votes += more.decided_from_votes;
  total.sent_ahead += more.sent_ahead;
  total.fast_commits += more.fast_commits;
  total.recovery_slices += more.recovery_slices;
  total.recovery_refused += more.recovery_refused;
  total.recoveries_sliced += more.recoveries_sliced;
}

/// A node's whole log for the checks: its snapshot's history followed by the entries above it; its commit
/// index, which is classic; and the index through which it knows the log committed, a leader's fast commits
/// included.
struct Whole {
  commit_index: u64,
  committed_through: u64,
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
/// entry before it — in Raft's own form, terms and all, since a follower's committed prefix is classic and so
/// is every later leader's.
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
    cluster.assign_windows(&mut rng);
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
    total.fast_tracks_opened += c.fast_tracks_opened;
    total.fast_proposals += c.fast_proposals;
    total.fast_votes_cast += c.fast_votes_cast;
    total.fast_votes_unrouted += c.fast_votes_unrouted;
    total.stalled_fills += c.stalled_fills;
    total.fast_choices += c.fast_choices;
    total.re_proposed_commits += c.re_proposed_commits;
    add_window(&mut total.window, c.window);
    for node in &cluster.nodes {
      add_window(&mut total.window, node.window_counters());
    }
  }
  total
}

/// Explores three and five voters over `seeds` histories each and holds every non-vacuity floor: each path
/// the exploration claims to cover must have been reached at least once per explored seed.
fn explore_and_check_coverage(seeds: u64) {
  let (mut re_proposed, mut beyond_reach) = (0, 0);
  for size in [3, 5] {
    let counted = explore(size, seeds);
    re_proposed += counted.re_proposed_commits;
    beyond_reach += counted.window.recovered_beyond_reach;
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
      (counted.fast_tracks_opened, "leaders opened the fast track"),
      (
        counted.fast_proposals,
        "commands were proposed on the fast track",
      ),
      (counted.fast_votes_cast, "fast votes were cast"),
      (counted.fast_choices, "fast quorums chose commands"),
      (
        counted.window.decided_from_votes,
        "leaders decided indices from votes",
      ),
      (
        counted.window.fast_commits,
        "fast quorums committed in one round",
      ),
      (
        counted.window.recovered_fast_choices,
        "recoveries re-proposed a fast choice",
      ),
      (
        counted.window.pruned,
        "window slots were pruned under a classic commit",
      ),
      (
        counted.window.sent_ahead,
        "leaders sent batches ahead of acknowledgements",
      ),
    ];
    for (count, path) in floors {
      assert!(
        count > seeds,
        "{size} voters: {path} ({count} over {seeds} seeds)"
      );
    }
    // Five paths are rarer than one event per seed, so each is floored at once per exploration. A staging
    // aborts only when its member is cut off for a whole CheckQuorum window. A leader sends a batch ahead only
    // when its backlog to a follower is more than one resend carries, so a follower buffers ahead of a hole
    // only then. A recovery fills a hole only when a value it re-proposes sits above an index its reports leave
    // free. And a leader fills an index its fast track stalled at only when lost votes left it short of a
    // quorum. Measured 2026-09-29 over 24 seeds, three / five voters: 4 / 11 aborts, 31 / 70 buffered and
    // 27 / 63 absorbed, 19 / 13 holes, 7 / 10 stalled indices filled.
    let rare = [
      (counted.stagings_aborted, "a staging aborted"),
      (
        counted.window.buffered,
        "a follower buffered pipelined entries ahead of a hole",
      ),
      (
        counted.window.absorbed,
        "buffered entries joined the log when the hole filled",
      ),
      (counted.window.holes_filled, "a recovery filled a hole"),
      (counted.stalled_fills, "a leader filled a stalled index"),
      (
        counted.window.recoveries_sliced,
        "a recovery took more than one slice (AUD-29-37)",
      ),
    ];
    for (count, path) in rare {
      assert!(count > 0, "{size} voters: {path}");
    }
  }
  // Two paths are rarer still, so each is floored over both sizes together. A command commits under two terms
  // only when a leader fast-commits it and a successor that lacks it re-proposes it: 3 / 0 over 24 seeds, three
  // / five voters. A recovery takes a value past its own window's reach only when a voter whose window reaches
  // further reported it and the new leader's is the smaller; with three voters only a buffered decision can,
  // since a fast choice there needs two reports and one voter reports: 0 / 16 over 400 seeds, and none in the
  // quick run, so it is floored at full scale — the unit test `a_recovery_reads_every_report_beyond_its_own_reach`
  // holds it on every run. Measured 2026-09-29.
  assert!(
    re_proposed > 0,
    "a fast commit was re-proposed by a successor"
  );
  if seeds >= SEEDS_FULL {
    assert!(
      beyond_reach > 0,
      "a recovery took a value past its own window's reach"
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
  cluster.assign_windows(&mut rng);
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
