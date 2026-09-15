//! The hecate Raft dialect, pure core (§4.8 mechanism 2 "Configuration, by consensus") — the regional
//! configuration group agrees membership, neighbourhoods, host epochs and takeover assignments by Raft,
//! not by the data-plane fenced register (that is mechanism 1). Built here: the **sans-io role and term
//! state machine with leader election** (Raft §5.2, §5.4.1 the election restriction) and **log
//! replication** (§5.3 `AppendEntries` — the consistency check, conflict truncation and log repair — and
//! §5.4.2 the commit-safety rule, so an earlier-term entry is never committed by replica count alone). A
//! deterministic state machine driven by an externally-timed `start_election`/`append_command` and by
//! received messages, so it is oracle-tested at N=1 before any timer or datagram is involved. The caller
//! owns the election and heartbeat timers and ships the [`RequestVote`]/[`VoteReply`]/[`AppendEntries`]/
//! [`AppendReply`] it returns.
//!
//! Also built: **PreVote** (§9.6) — a would-be candidate first runs a non-binding pre-vote round at the
//! term it *would* seek, without incrementing its own term; only on a majority of pre-votes does it start
//! a real election. A peer refuses a pre-vote while it still believes a leader is alive, so a
//! partitioned, term-inflated node cannot force a healthy leader to step down when it rejoins. And
//! **CheckQuorum** (§6.2) — a leader that has not been in contact with a majority since its previous
//! check steps down, so a leader cut off from the cluster stops acting as one. And **ReadIndex** (§6.4) —
//! the leader serves a linearizable read at its commit index without appending a log entry, safe only
//! when it has committed in its current term and is confirmed in contact with a majority. PreVote and
//! CheckQuorum together give the stability etcd's raft ships by default. And the **joint-consensus
//! majority rule** (§6) — during a membership change the node enters a joint configuration where every
//! decision needs a majority of *both* the old and new voter sets, so no two disjoint majorities can form
//! across the change; every quorum check (election, commit, CheckQuorum, ReadIndex) honours it.
//!
//! And **snapshot/log compaction with install-snapshot** (§7): [`compact`](RaftNode::compact) folds the
//! committed prefix into a snapshot and discards it, so the log stays bounded (every index resolves
//! through a snapshot offset that is a no-op until the first compaction); and a follower that has fallen
//! below the leader's snapshot — which no append can reach — is caught up by
//! [`install_snapshot_for`](RaftNode::install_snapshot_for)/[`on_install_snapshot`](RaftNode::on_install_snapshot).
//! The multi-node **conformance suite** (`tests/raft.rs`) drives a cluster through election, replication,
//! a partition and a membership change, checking Election Safety, Log Matching, Leader Completeness and
//! State Machine Safety.
//!
//! And the **log-integrated membership change** (§6): [`begin`](RaftNode::begin_membership_change) and
//! [`complete_membership_change`](RaftNode::complete_membership_change) append `C_old,new`/`C_new`
//! configuration entries that take effect the moment they are appended (the effective configuration is
//! derived from the log, so a truncated entry reverts it) and replicate like any entry; compaction folds
//! a discarded configuration into the base, and install-snapshot carries it, so it is never lost. One
//! change is in flight at a time (a configuration entry is appended only once the previous one has
//! committed — Ongaro's thesis §4.1, the rule that keeps two changes from producing disjoint majorities);
//! a leader whose own removal commits **steps down** (§4.2.2); a node outside its effective configuration
//! never campaigns; and the leader keeps replicating to the **outgoing** voters until the entry that
//! removes them commits, so a live member demoted to learner learns its own removal
//! ([`replication_targets`](RaftNode::replication_targets)). This completes the dialect's core.
//!
//! Degenerate on a laptop (`f = 0`): one voter, itself; a pre-vote and an election each reach a majority
//! of one at once, an appended entry commits at once, and the lone voter is always its own quorum so it
//! never steps down — the same code path as a fleet, never a mode switch (R8). Owed: driving the dialect
//! live over the transport in a multi-node fleet (this core is sans-io and multi-node-tested by direct
//! message passing), and the PreVote/CheckQuorum timer cadence, which is the caller's clock.
//!
//! Evidence: Ongaro & Ousterhout, *In Search of an Understandable Consensus Algorithm (Extended
//! Version)*, 2014 (tier A); the safety argument for the election restriction is §5.4.

use std::collections::{BTreeMap, BTreeSet};

use slates_db::register::HostId;

/// A node's role in its term (Raft §5.1, plus the PreVote pre-candidacy of §9.6). A follower defers to a
/// leader; a pre-candidate is testing whether an election could win without yet inflating its term; a
/// candidate is seeking votes; a leader has a majority for its term.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
  /// Passive — grants votes and accepts a leader's entries.
  Follower,
  /// Running a pre-vote round (§9.6): gathering non-binding assurances that an election could win,
  /// without incrementing its term, so a partitioned node cannot disrupt a healthy leader.
  PreCandidate,
  /// Seeking votes for its term.
  Candidate,
  /// Won a majority for its term.
  Leader,
}

/// A request for votes (Raft `RequestVote`): the candidate's term and the summary of its log the
/// election restriction (§5.4.1) compares against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestVote {
  /// The candidate's term.
  pub term: u64,
  /// The candidate seeking the vote.
  pub candidate: HostId,
  /// The index of the candidate's last log entry (zero when its log is empty).
  pub last_log_index: u64,
  /// The term of the candidate's last log entry (zero when its log is empty).
  pub last_log_term: u64,
}

/// A pre-vote request (Raft §9.6): asked at the term the candidate *would* seek (`current + 1`) without
/// the candidate incrementing its own term. A peer answers whether it would grant a real vote — but its
/// term is left untouched, so a partitioned high-term node cannot force the cluster's term upward.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreVote {
  /// The term the candidate would seek (its current term plus one).
  pub term: u64,
  /// The pre-candidate.
  pub candidate: HostId,
  /// The index of the candidate's last log entry.
  pub last_log_index: u64,
  /// The term of the candidate's last log entry.
  pub last_log_term: u64,
}

/// A reply to a [`PreVote`]: the voter and whether it would grant a real vote. It carries no term
/// authority — a pre-vote never changes any node's term.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreVoteReply {
  /// The voter replying.
  pub voter: HostId,
  /// The term the pre-vote was for (the candidate matches replies to its pre-election).
  pub term: u64,
  /// Whether the voter would grant a real vote.
  pub granted: bool,
}

/// A reply to a [`RequestVote`]: the replying voter, its current term (so a candidate learns of a newer
/// term), and whether it granted the vote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoteReply {
  /// The voter replying.
  pub voter: HostId,
  /// The voter's current term.
  pub term: u64,
  /// Whether the vote was granted.
  pub granted: bool,
}

/// A voter configuration (Raft §6): the base voter set and, during a membership change, the incoming
/// set. A decision needs a majority of the base and — when `joint` is set — of the incoming set too.
#[derive(slates_wire::Wire, Clone, Debug, PartialEq, Eq)]
pub struct VoterConfig {
  /// The base voter set.
  pub voters: Vec<HostId>,
  /// The incoming voter set while a joint membership change is in flight.
  pub joint: Option<Vec<HostId>>,
}

/// One entry in the replicated log (Raft's per-entry term is the basis of the log-matching property).
/// A normal entry carries an opaque `command` the state machine applies (for the configuration group, an
/// encoded configuration change — the Raft core does not interpret it). A **configuration entry** instead
/// carries a [`VoterConfig`] that changes the Raft voter set; it takes effect the moment it is appended
/// (§6), so the Raft core reads it directly rather than through the state machine.
#[derive(slates_wire::Wire, Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
  /// The term in which the leader created this entry.
  pub term: u64,
  /// The command to apply once the entry commits (empty for a configuration entry).
  pub command: Vec<u8>,
  /// The voter configuration this entry installs, when it is a configuration entry (Raft §6).
  pub config: Option<VoterConfig>,
}

impl LogEntry {
  /// A normal command entry.
  pub fn command(term: u64, command: Vec<u8>) -> LogEntry {
    LogEntry {
      term,
      command,
      config: None,
    }
  }

  /// A configuration entry installing `config` (Raft §6, take-effect-on-append).
  pub fn configuration(term: u64, config: VoterConfig) -> LogEntry {
    LogEntry {
      term,
      command: Vec::new(),
      config: Some(config),
    }
  }
}

/// A leader's replication message (Raft `AppendEntries`): the leader's term, the log position it is
/// appending after (`prev_log_index`/`prev_log_term`, the consistency check), the entries to append
/// (empty for a heartbeat), and the leader's commit index. A follower appends only when its log matches
/// at the previous position, so the logs converge (the log-matching property, §5.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppendEntries {
  /// The current read-confirmation round, or zero when no read is pending. A follower echoes it
  /// only after recognizing this term's leader, so old replies cannot confirm a later read.
  pub read_context: u64,
  /// The leader's term.
  pub term: u64,
  /// The leader sending the entries.
  pub leader: HostId,
  /// The index immediately preceding the new entries (zero at the start of the log).
  pub prev_log_index: u64,
  /// The term of the entry at `prev_log_index` (zero at the start of the log).
  pub prev_log_term: u64,
  /// The entries to append (empty for a heartbeat).
  pub entries: Vec<LogEntry>,
  /// The leader's commit index, so the follower may advance its own.
  pub leader_commit: u64,
}

/// A follower's reply to [`AppendEntries`]: the follower, its current term, whether the append
/// succeeded (the consistency check held), and — on success — the highest log index it now matches the
/// leader on, so the leader advances `match_index`/`next_index` for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppendReply {
  /// The read context from the request this reply answers; zero is not a read confirmation.
  pub read_context: u64,
  /// The follower replying.
  pub follower: HostId,
  /// The follower's current term.
  pub term: u64,
  /// Whether the append succeeded.
  pub success: bool,
  /// On success, the last index the follower's log now matches the leader on.
  pub match_index: u64,
}

/// A leader's snapshot transfer (Raft `InstallSnapshot`, §7) — sent to a follower that has fallen below
/// the leader's snapshot, so `AppendEntries` cannot reach it (the entries it needs were compacted away).
/// It resets the follower's log to begin after the snapshot's last included entry and carries the
/// state-machine state at that point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallSnapshot {
  /// The leader's term.
  pub term: u64,
  /// The leader sending the snapshot.
  pub leader: HostId,
  /// The index of the last entry the snapshot includes (the follower's log resets to just after it).
  pub last_included_index: u64,
  /// The term of that last included entry (checked against any entry the follower still holds there).
  pub last_included_term: u64,
  /// The voter configuration in effect at the snapshot, so a follower that discards its log to install
  /// the snapshot does not lose it (Raft §6 configurations live in the state, hence the snapshot).
  pub config: VoterConfig,
  /// The state-machine state at the snapshot (opaque to the Raft core; the caller applies it).
  pub state: Vec<u8>,
}

/// A follower's reply to [`InstallSnapshot`]: the follower and its current term.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InstallSnapshotReply {
  /// The follower replying.
  pub follower: HostId,
  /// The follower's current term.
  pub term: u64,
}

/// One outstanding ReadIndex round (§6.4): a read's start index and voter configuration, plus
/// only the distinct voters that have answered that round. One round bounds pending read state.
struct ReadRound {
  context: u64,
  term: u64,
  index: u64,
  config: VoterConfig,
  confirmed: BTreeSet<HostId>,
}

/// The complete state retained with a voter identity (§4.8; Raft §3.8/§5). Term and vote cannot
/// be recovered independently of the log, snapshot and configuration. The commit index is retained
/// too, so the group's deterministic fold can resume without exposing a shorter committed prefix.
#[derive(slates_wire::Wire, Clone, Debug, PartialEq, Eq)]
pub struct SavedRaft {
  /// The only voter identity this publication can recover.
  pub id: HostId,
  /// The configuration at the snapshot boundary, before the remaining log entries.
  pub base: VoterConfig,
  /// The greatest observed election term.
  pub term: u64,
  /// The vote already granted in that term.
  pub voted_for: Option<HostId>,
  /// All entries above the snapshot, including the uncommitted tail and configuration entries.
  pub log: Vec<LogEntry>,
  /// The last committed position.
  pub commit_index: u64,
  /// The last position folded into the state-machine snapshot.
  pub snapshot_index: u64,
  /// The term at that position, needed for log matching.
  pub snapshot_term: u64,
  /// The state-machine snapshot at that position.
  pub snapshot_data: Vec<u8>,
}

/// A publication that cannot describe a legal recovered Raft state (§4.8). The caller refuses
/// recovery; none of these errors permits constructing a fresh voter under the saved identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RaftRecoveryError {
  /// Joining again would discard a term, vote or prefix this member has already observed.
  AlreadyInitialized,
  /// A join attempted to reuse the state donor's identity instead of a fresh member identity.
  ReusedIdentity,
  /// A voter set is empty or repeats an identity.
  InvalidVoters,
  /// The snapshot boundary disagrees with its term or carries state at index zero.
  InvalidSnapshot,
  /// A log entry's term decreases or exceeds the saved current term.
  InvalidLogTerm,
  /// The committed position is outside the retained snapshot and log.
  InvalidCommitIndex,
  /// A retained position cannot be represented by the protocol's index width.
  IndexOverflow,
}

impl SavedRaft {
  /// Validates the state before any recovered node is exposed. Framing and checksum verification
  /// belong to the transport or publication reader, before this decoded value reaches the core.
  fn validate(&self) -> Result<(), RaftRecoveryError> {
    let valid_set = |voters: &[HostId]| {
      !voters.is_empty() && voters.iter().copied().collect::<BTreeSet<_>>().len() == voters.len()
    };
    let valid_config = |config: &VoterConfig| {
      valid_set(&config.voters) && config.joint.as_deref().is_none_or(valid_set)
    };
    if !valid_config(&self.base) {
      return Err(RaftRecoveryError::InvalidVoters);
    }
    if (self.snapshot_index == 0 && (self.snapshot_term != 0 || !self.snapshot_data.is_empty()))
      || (self.snapshot_index > 0 && self.snapshot_term == 0)
      || self.snapshot_term > self.term
    {
      return Err(RaftRecoveryError::InvalidSnapshot);
    }
    let last_index = self
      .snapshot_index
      .checked_add(u64::try_from(self.log.len()).map_err(|_| RaftRecoveryError::IndexOverflow)?)
      .ok_or(RaftRecoveryError::IndexOverflow)?;
    if self.commit_index < self.snapshot_index || self.commit_index > last_index {
      return Err(RaftRecoveryError::InvalidCommitIndex);
    }
    let mut previous_term = self.snapshot_term;
    for entry in &self.log {
      if entry.term == 0 || entry.term < previous_term || entry.term > self.term {
        return Err(RaftRecoveryError::InvalidLogTerm);
      }
      if entry
        .config
        .as_ref()
        .is_some_and(|config| !valid_config(config))
      {
        return Err(RaftRecoveryError::InvalidVoters);
      }
      previous_term = entry.term;
    }
    Ok(())
  }
}

/// A Raft node's state: its identity, the voters it counts a majority against, the persistent term and
/// vote (Raft's `currentTerm`/`votedFor`), its role, the votes gathered this election, the replicated
/// `log` and how far it is committed, and — while leader — the per-follower `next_index`/`match_index`
/// replication progress.
pub struct RaftNode {
  /// Changed persistent state, cleared only after its owner publishes it (§4.8).
  retention_pending: bool,
  id: HostId,
  voters: Vec<HostId>,
  joint: Option<Vec<HostId>>,
  current_term: u64,
  voted_for: Option<HostId>,
  role: Role,
  votes: BTreeSet<HostId>,
  pre_votes: BTreeSet<HostId>,
  has_leader: bool,
  /// The leader this node currently believes in — a follower learns it from an accepted append; a leader is
  /// itself (via [`leader`](RaftNode::leader)). A **redirection hint** only (for an operator command that must
  /// reach the leader), never consulted for safety. `None` when campaigning, stepped down, or a leader that
  /// lost its quorum.
  leader_hint: Option<HostId>,
  contacts: BTreeSet<HostId>,
  read_context: u64,
  read_round: Option<ReadRound>,
  log: Vec<LogEntry>,
  commit_index: u64,
  next_index: BTreeMap<HostId, u64>,
  match_index: BTreeMap<HostId, u64>,
  snapshot_index: u64,
  snapshot_term: u64,
  snapshot_data: Vec<u8>,
}

impl RaftNode {
  /// A fresh node at term zero with an empty log. It is a non-voting learner when `voters` does
  /// not contain its id; only a replicated membership change may then admit it (§4.8, AUD-07).
  /// The caller must never use this constructor to recreate a state-losing voter under its old id.
  pub fn new(id: HostId, voters: Vec<HostId>) -> RaftNode {
    RaftNode {
      retention_pending: true,
      id,
      voters,
      joint: None,
      current_term: 0,
      voted_for: None,
      role: Role::Follower,
      votes: BTreeSet::new(),
      pre_votes: BTreeSet::new(),
      has_leader: false,
      leader_hint: None,
      contacts: BTreeSet::new(),
      read_context: 0,
      read_round: None,
      log: Vec::new(),
      commit_index: 0,
      next_index: BTreeMap::new(),
      match_index: BTreeMap::new(),
      snapshot_index: 0,
      snapshot_term: 0,
      snapshot_data: Vec::new(),
    }
  }

  /// The state to publish before acknowledging a changed term, vote, log or snapshot (§4.8).
  /// Volatile leadership, read rounds, contact evidence and replication progress are excluded.
  pub fn saved(&self) -> SavedRaft {
    SavedRaft {
      id: self.id,
      base: VoterConfig {
        voters: self.voters.clone(),
        joint: self.joint.clone(),
      },
      term: self.current_term,
      voted_for: self.voted_for,
      log: self.log.clone(),
      commit_index: self.commit_index,
      snapshot_index: self.snapshot_index,
      snapshot_term: self.snapshot_term,
      snapshot_data: self.snapshot_data.clone(),
    }
  }

  /// Restores one validated publication as a follower, preserving its vote and committed prefix.
  /// Unlike reconstructing from a term and a log alone, this also restores compacted membership.
  pub fn restore(saved: SavedRaft) -> Result<RaftNode, RaftRecoveryError> {
    saved.validate()?;
    let mut node = RaftNode::new(saved.id, saved.base.voters);
    node.joint = saved.base.joint;
    node.current_term = saved.term;
    node.voted_for = saved.voted_for;
    node.log = saved.log;
    node.commit_index = saved.commit_index;
    node.snapshot_index = saved.snapshot_index;
    node.snapshot_term = saved.snapshot_term;
    node.snapshot_data = saved.snapshot_data;
    Ok(node)
  }

  /// Whether a reply would depend on state not yet published by the owner (§4.8).
  pub fn retention_pending(&self) -> bool {
    self.retention_pending
  }

  /// Marks the complete state published. Call only after the anchor publication succeeds.
  pub fn retained(&mut self) {
    self.retention_pending = false;
  }

  /// Drops volatile authority while an operator-reviewed replacement is being fetched (§4.8).
  /// The term, vote and log remain recoverable; the caller also stops elections and input RPCs.
  pub fn suspend(&mut self) {
    self.role = Role::Follower;
    self.has_leader = false;
    self.leader_hint = None;
    self.read_round = None;
    self.contacts.clear();
  }

  /// This node's id.
  pub fn id(&self) -> HostId {
    self.id
  }

  /// This node's role.
  pub fn role(&self) -> Role {
    self.role
  }

  /// This node's current term.
  pub fn term(&self) -> u64 {
    self.current_term
  }

  /// Whether this node is the leader for its term.
  pub fn is_leader(&self) -> bool {
    self.role == Role::Leader
  }

  /// The leader this node currently knows — itself when it leads, else the last leader it accepted an append
  /// from (`None` when campaigning, stepped down, or a leader that lost its quorum). A **redirection hint**
  /// only: an operator command that must reach the leader is forwarded here, and a stale hint costs a retry,
  /// never a safety violation (the target refuses if it is not in fact the leader). Never consulted on a
  /// safety path.
  pub fn leader(&self) -> Option<HostId> {
    if self.role == Role::Leader {
      Some(self.id)
    } else {
      self.leader_hint
    }
  }

  /// Who this node voted for in its current term, if anyone.
  pub fn voted_for(&self) -> Option<HostId> {
    self.voted_for
  }

  /// The caller's election timer fired with no leader contact: begin a **pre-election** (Raft §9.6).
  /// The node becomes a pre-candidate and forgets its belief in a leader, but does **not** increment its
  /// term; it returns the [`PreVote`] to send each other voter, asking whether a real election could
  /// win. A single voter's pre-vote already carries a majority, so it proceeds straight to a real
  /// election and leads (the `f = 0` degenerate, no messages). Preferring this over a direct
  /// `start_election` is what keeps a partitioned, term-inflated node from disrupting a healthy leader.
  /// A node that is **not a voter** of its effective configuration — removed by a committed membership
  /// change, or a learner that never was one — does not campaign at all (Ongaro's thesis §4.2.3: a
  /// removed server that kept campaigning would disrupt the cluster it no longer belongs to).
  pub fn on_election_timeout(&mut self) -> Vec<PreVote> {
    if !self.is_voter(self.id) {
      return Vec::new();
    }
    self.has_leader = false;
    self.leader_hint = None;
    self.role = Role::PreCandidate;
    self.pre_votes = BTreeSet::from([self.id]);
    if self.is_majority(&self.pre_votes) {
      self.start_election();
      return Vec::new();
    }
    let request = PreVote {
      term: self.current_term.saturating_add(1),
      candidate: self.id,
      last_log_index: self.last_log_index(),
      last_log_term: self.last_log_term(),
    };
    self
      .all_voters()
      .into_iter()
      .filter(|voter| *voter != self.id)
      .map(|_| request)
      .collect()
  }

  /// Answers a received [`PreVote`] (Raft §9.6) **without changing this node's term, vote or role** — a
  /// pre-vote is non-binding. The node would grant a real vote only if it does not currently believe a
  /// leader is alive (it has not heard from one since its own election timeout), it is not itself the
  /// leader, the pre-vote's term is ahead of its own, and the candidate's log is at least as up-to-date.
  /// Because the term is never touched, a partitioned node's inflated term cannot force a step-down here.
  pub fn on_pre_vote(&self, request: PreVote) -> PreVoteReply {
    let granted = self.is_voter(self.id)
      && !self.has_leader
      && self.role != Role::Leader
      && request.term > self.current_term
      && self.candidate_log_is_current(request.last_log_index, request.last_log_term);
    PreVoteReply {
      voter: self.id,
      term: request.term,
      granted,
    }
  }

  /// Handles a received [`PreVoteReply`]. While this node is a pre-candidate for this pre-term, a granted
  /// reply is counted; once a majority would grant, the node starts the **real** election (incrementing
  /// its term now, having confirmed it can win) and returns the [`RequestVote`] to send. Otherwise
  /// `None`.
  pub fn on_pre_vote_reply(&mut self, reply: PreVoteReply) -> Option<Vec<RequestVote>> {
    if self.role != Role::PreCandidate
      || reply.term != self.current_term.saturating_add(1)
      || !reply.granted
    {
      return None;
    }
    self.pre_votes.insert(reply.voter);
    if self.is_majority(&self.pre_votes) {
      return Some(self.start_election());
    }
    None
  }

  /// The caller's election timer fired: begin an election (Raft §5.2). Advance to the next term, become
  /// a candidate, vote for self, and return the [`RequestVote`] to send each *other* voter. A single
  /// voter reaches its own majority here and becomes leader with no messages (the `f = 0` degenerate).
  /// Prefer [`on_election_timeout`](RaftNode::on_election_timeout), which runs the pre-vote round first.
  pub fn start_election(&mut self) -> Vec<RequestVote> {
    if !self.is_voter(self.id) {
      return Vec::new();
    }
    self.retention_pending = true;
    self.current_term = self.current_term.saturating_add(1);
    self.read_round = None;
    self.role = Role::Candidate;
    self.voted_for = Some(self.id);
    self.votes = BTreeSet::from([self.id]);
    self.become_leader_if_majority();

    let request = RequestVote {
      term: self.current_term,
      candidate: self.id,
      last_log_index: self.last_log_index(),
      last_log_term: self.last_log_term(),
    };
    self
      .all_voters()
      .into_iter()
      .filter(|voter| *voter != self.id)
      .map(|_| request)
      .collect()
  }

  /// Handles a received [`RequestVote`] (Raft §5.2, §5.4.1). A request under a newer term first steps
  /// this node down to a follower at that term (clearing its vote). The vote is granted only when the
  /// request is for our current term, we have not already voted for someone else this term, and the
  /// candidate's log is at least as up-to-date as ours (the election restriction that keeps a leader's
  /// log a superset of every committed entry). Returns the reply to send back.
  pub fn on_request_vote(&mut self, request: RequestVote) -> VoteReply {
    if request.term > self.current_term {
      self.step_down(request.term);
    }
    let not_yet_voted_elsewhere =
      self.voted_for.is_none() || self.voted_for == Some(request.candidate);
    let granted = self.is_voter(self.id)
      && request.term == self.current_term
      && not_yet_voted_elsewhere
      && self.candidate_log_is_current(request.last_log_index, request.last_log_term);
    if granted {
      self.retention_pending |= self.voted_for != Some(request.candidate);
      self.voted_for = Some(request.candidate);
    }
    VoteReply {
      voter: self.id,
      term: self.current_term,
      granted,
    }
  }

  /// Handles a received [`VoteReply`]. A reply carrying a newer term steps us down. Otherwise, while we
  /// are still the candidate for this term, a granted vote is counted, and reaching a majority makes us
  /// leader. A stale reply (an older term, or after we have moved on) is ignored.
  pub fn on_vote_reply(&mut self, reply: VoteReply) {
    if reply.term > self.current_term {
      self.step_down(reply.term);
      return;
    }
    if self.role == Role::Candidate && reply.term == self.current_term && reply.granted {
      self.votes.insert(reply.voter);
      self.become_leader_if_majority();
    }
  }

  /// Steps this node down to a follower at `term` on observing it from any message (Raft §5.1 "a node
  /// that sees a higher term becomes a follower"). A no-op if `term` is not newer.
  pub fn observe_term(&mut self, term: u64) {
    if term > self.current_term {
      self.step_down(term);
    }
  }

  /// Adopts `term` as the current term, reverting to a follower and forgetting this term's vote and any
  /// gathered votes.
  fn step_down(&mut self, term: u64) {
    self.retention_pending |= self.current_term != term || self.voted_for.is_some();
    self.current_term = term;
    self.voted_for = None;
    self.read_round = None;
    self.role = Role::Follower;
    self.leader_hint = None;
    self.votes.clear();
  }

  /// Becomes leader if the votes gathered this election are a majority of the voters, initialising the
  /// replication progress for each follower — `next_index` at the end of the leader's log (Raft's
  /// optimistic guess) and `match_index` at nothing known replicated (§5.3).
  fn become_leader_if_majority(&mut self) {
    if self.role != Role::Candidate || !self.is_majority(&self.votes) {
      return;
    }
    self.role = Role::Leader;
    // Start the CheckQuorum window already in contact with the voters that just elected it, so the first
    // check does not spuriously step a freshly-won leader down before its heartbeats have replied.
    self.contacts = self.votes.clone();
    let next = self.last_log_index().saturating_add(1);
    self.next_index.clear();
    self.match_index.clear();
    for peer in &self.all_voters() {
      if *peer != self.id {
        self.next_index.insert(*peer, next);
        self.match_index.insert(*peer, 0);
      }
    }
  }

  /// Every voter that participates now — the base set, plus the incoming set while a joint membership
  /// change is in flight (Raft §6). The leader sends votes and entries to all of them; a message's
  /// recipient is the connection, so duplicates in the union are harmless, but they are deduped here.
  pub fn all_voters(&self) -> Vec<HostId> {
    let config = self.effective_config();
    let mut set: BTreeSet<HostId> = config.voters.into_iter().collect();
    if let Some(new) = config.joint {
      set.extend(new);
    }
    set.into_iter().collect()
  }

  /// Whether `node` votes under the configuration in effect now — the base set, or either set of a joint
  /// change in flight. A removed voter stops being one the moment the entry that removes it is appended.
  pub fn is_voter(&self, node: HostId) -> bool {
    self.all_voters().contains(&node)
  }

  /// The peers the leader replicates to: every current voter ([`all_voters`](RaftNode::all_voters)),
  /// plus — while the latest configuration entry is still uncommitted — the **outgoing** voters of the
  /// configuration it replaces. An outgoing voter that is alive (a member demoted to learner because a
  /// lower-id member joined the council) thereby receives the entry that removes it in the same rounds
  /// that commit it, learns it is no longer a voter, and stops campaigning; a dead one costs nothing
  /// (there is no session to it). Bounded: the extra targets drop out the moment the change commits.
  pub fn replication_targets(&self) -> Vec<HostId> {
    let mut set: BTreeSet<HostId> = self.all_voters().into_iter().collect();
    if self.latest_config_index() > self.commit_index
      && let Some(outgoing) = self.config_before_latest()
    {
      set.extend(outgoing.voters);
      if let Some(joint) = outgoing.joint {
        set.extend(joint);
      }
    }
    set.into_iter().collect()
  }

  /// The one-based log index of the most recent configuration entry, or zero when the log holds none
  /// (the effective configuration is then the base).
  fn latest_config_index(&self) -> u64 {
    self
      .log
      .iter()
      .rposition(|entry| entry.config.is_some())
      .and_then(|position| u64::try_from(position).ok())
      .map_or(0, |position| {
        self
          .snapshot_index
          .saturating_add(position)
          .saturating_add(1)
      })
  }

  /// The configuration the latest configuration entry replaced: the previous configuration entry in the
  /// log, or the base when it is the only one. `None` when the log holds no configuration entry.
  fn config_before_latest(&self) -> Option<VoterConfig> {
    let latest = self.log.iter().rposition(|entry| entry.config.is_some())?;
    let previous = self.log[..latest]
      .iter()
      .rev()
      .find_map(|entry| entry.config.clone());
    Some(previous.unwrap_or(VoterConfig {
      voters: self.voters.clone(),
      joint: self.joint.clone(),
    }))
  }

  /// The configuration in force **at the commit index**: the most recent configuration entry at or below
  /// it, or the base when none is committed — what a leader consults to learn that its own removal has
  /// committed.
  fn committed_config(&self) -> VoterConfig {
    let committed = usize::try_from(self.commit_index.saturating_sub(self.snapshot_index))
      .unwrap_or(usize::MAX)
      .min(self.log.len());
    for entry in self.log[..committed].iter().rev() {
      if let Some(config) = &entry.config {
        return config.clone();
      }
    }
    VoterConfig {
      voters: self.voters.clone(),
      joint: self.joint.clone(),
    }
  }

  /// A leader whose own removal has **committed** steps down (Ongaro's thesis §4.2.2): once the sole
  /// configuration in force at the commit index no longer names it, it stops leading — it managed the
  /// cluster through the change and now hands over to the new voters, who elect among themselves. While
  /// the change is still joint it keeps leading (the old configuration still names it).
  fn step_down_if_removed(&mut self) {
    if self.role != Role::Leader {
      return;
    }
    let committed = self.committed_config();
    if committed.joint.is_none() && !committed.voters.contains(&self.id) {
      self.read_round = None;
      self.role = Role::Follower;
      self.has_leader = false;
      self.leader_hint = None;
      self.votes.clear();
    }
  }

  /// The voter configuration in effect now: the most recent configuration entry in the log (a
  /// configuration takes effect the moment it is appended, before it commits — Raft §6), or the base
  /// configuration when the log holds none. A truncated configuration entry reverts the effective
  /// configuration automatically, because it is derived from the log rather than stored.
  fn effective_config(&self) -> VoterConfig {
    for entry in self.log.iter().rev() {
      if let Some(config) = &entry.config {
        return config.clone();
      }
    }
    VoterConfig {
      voters: self.voters.clone(),
      joint: self.joint.clone(),
    }
  }

  /// Whether `granters` form a majority under the current configuration (the quorum intersection Raft's
  /// safety rests on): more than half of the base voters, **and** — while a joint change is in flight —
  /// more than half of the incoming voters too, so no two disjoint majorities can form across the change.
  fn is_majority(&self, granters: &BTreeSet<HostId>) -> bool {
    let carries =
      |set: &[HostId]| set.iter().filter(|voter| granters.contains(voter)).count() > set.len() / 2;
    let config = self.effective_config();
    carries(&config.voters) && config.joint.as_ref().is_none_or(|new| carries(new))
  }

  /// Whether this node is in a joint configuration (a membership change is in flight).
  pub fn in_joint_configuration(&self) -> bool {
    self.effective_config().joint.is_some()
  }

  /// Whether a candidate's last-log summary is at least as up-to-date as ours (Raft §5.4.1): a later
  /// last term wins; at an equal last term the longer (or equal) log wins.
  fn candidate_log_is_current(&self, candidate_index: u64, candidate_term: u64) -> bool {
    candidate_term > self.last_log_term()
      || (candidate_term == self.last_log_term() && candidate_index >= self.last_log_index())
  }

  /// The vector position of the one-based log `index`, or `None` when it is not in the in-memory log
  /// (it is at or before the snapshot, or beyond the end). With no snapshot (`snapshot_index == 0`) this
  /// is `index - 1` — the un-compacted layout.
  fn position(&self, index: u64) -> Option<usize> {
    if index <= self.snapshot_index {
      return None;
    }
    usize::try_from(index - self.snapshot_index - 1).ok()
  }

  /// The index of the last log entry (the snapshot index for an empty log; zero when neither exists).
  /// Raft indexes entries from one.
  pub fn last_log_index(&self) -> u64 {
    self
      .snapshot_index
      .saturating_add(u64::try_from(self.log.len()).unwrap_or(u64::MAX))
  }

  /// The term of the last log entry (the snapshot term for an empty log; zero when neither exists).
  pub fn last_log_term(&self) -> u64 {
    self
      .log
      .last()
      .map_or(self.snapshot_term, |entry| entry.term)
  }

  /// The term of the entry at the one-based `index`, or `None` if it is not individually known: index
  /// zero (the empty-log sentinel), an index below the snapshot (folded into it), or beyond the log's
  /// end. The snapshot's own index returns the snapshot term. The consistency check treats the sentinel
  /// specially.
  fn entry_term(&self, index: u64) -> Option<u64> {
    if index == 0 {
      return None;
    }
    if index == self.snapshot_index {
      return Some(self.snapshot_term);
    }
    let position = self.position(index)?;
    self.log.get(position).map(|entry| entry.term)
  }

  /// The highest index known committed (a majority holds it).
  pub fn commit_index(&self) -> u64 {
    self.commit_index
  }

  /// The committed log entries not yet folded into the snapshot, in order (the entries the caller applies
  /// after the snapshotted prefix). With no snapshot this is the whole committed prefix.
  pub fn committed_entries(&self) -> &[LogEntry] {
    let committed_above_snapshot = self.commit_index.saturating_sub(self.snapshot_index);
    let count = usize::try_from(committed_above_snapshot).unwrap_or(usize::MAX);
    &self.log[..count.min(self.log.len())]
  }

  /// The index up to which the log has been compacted into a snapshot (zero when nothing is compacted).
  pub fn snapshot_index(&self) -> u64 {
    self.snapshot_index
  }

  /// Compacts the log by folding the committed prefix up to `up_to` into a snapshot and discarding those
  /// entries, so the log stays bounded (§7). Only committed entries are compacted — `up_to` must be at or
  /// below the commit index and beyond the current snapshot — and the snapshot term is recorded so the
  /// consistency check at the boundary still holds. Returns whether it compacted. The caller must have
  /// captured the state machine's state at `up_to` first; a follower far enough behind to need a
  /// discarded entry is served an install-snapshot (owed).
  pub fn compact(&mut self, up_to: u64, state: Vec<u8>) -> bool {
    if up_to <= self.snapshot_index || up_to > self.commit_index {
      return false;
    }
    let Some(term) = self.entry_term(up_to) else {
      return false;
    };
    let discard = usize::try_from(up_to - self.snapshot_index).unwrap_or(usize::MAX);
    let discard = discard.min(self.log.len());
    // A configuration entry in the discarded prefix would take its voter set with it — fold the most
    // recent one into the base configuration so the effective configuration is preserved. (A later
    // configuration entry that survives the compaction still dominates it, being derived from the log.)
    if let Some(config) = self.log[..discard]
      .iter()
      .rev()
      .find_map(|entry| entry.config.clone())
    {
      self.voters = config.voters;
      self.joint = config.joint;
    }
    self.retention_pending = true;
    self.log.drain(0..discard);
    self.snapshot_index = up_to;
    self.snapshot_term = term;
    self.snapshot_data = state;
    true
  }

  /// The [`InstallSnapshot`] to send `follower` when it has fallen below the leader's snapshot (the
  /// entries it needs were compacted away), else `None`. The leader calls this when
  /// [`replicate_to`](RaftNode::replicate_to) returns `None`.
  pub fn install_snapshot_for(&self, follower: HostId) -> Option<InstallSnapshot> {
    if self.role != Role::Leader || self.snapshot_index == 0 {
      return None;
    }
    let next = self.next_index.get(&follower).copied().unwrap_or(1).max(1);
    if next > self.snapshot_index {
      return None; // an append can still reach it
    }
    Some(InstallSnapshot {
      term: self.current_term,
      leader: self.id,
      last_included_index: self.snapshot_index,
      last_included_term: self.snapshot_term,
      // The configuration at the snapshot is the base — `compact` folded any discarded configuration
      // entry into it, and no surviving log entry precedes the snapshot.
      config: VoterConfig {
        voters: self.voters.clone(),
        joint: self.joint.clone(),
      },
      state: self.snapshot_data.clone(),
    })
  }

  /// Handles a received [`InstallSnapshot`] as a follower (Raft §7). A stale-term snapshot is rejected; a
  /// current-or-newer one is installed: if the follower holds an entry at the snapshot's last included
  /// index and term it keeps the following entries, otherwise it discards its whole log; then it adopts
  /// the snapshot index, term and state and advances its commit index to at least the snapshot. The
  /// caller applies the state to its state machine. Returns the reply.
  pub fn on_install_snapshot(&mut self, request: InstallSnapshot) -> InstallSnapshotReply {
    if request.term < self.current_term {
      return InstallSnapshotReply {
        follower: self.id,
        term: self.current_term,
      };
    }
    if request.term > self.current_term {
      self.step_down(request.term);
    }
    self.read_round = None;
    self.role = Role::Follower;
    self.has_leader = true;
    self.leader_hint = Some(request.leader);

    if request.last_included_index > self.snapshot_index {
      self.retention_pending = true;
      let keeps_suffix =
        self.entry_term(request.last_included_index) == Some(request.last_included_term);
      if keeps_suffix {
        let discard = usize::try_from(request.last_included_index - self.snapshot_index)
          .unwrap_or(usize::MAX)
          .min(self.log.len());
        self.log.drain(0..discard);
      } else {
        self.log.clear();
      }
      self.snapshot_index = request.last_included_index;
      self.snapshot_term = request.last_included_term;
      self.snapshot_data = request.state;
      // Adopt the configuration at the snapshot as the base, so the effective configuration is preserved
      // now that the log entries that carried it are gone.
      self.voters = request.config.voters;
      self.joint = request.config.joint;
      self.commit_index = self.commit_index.max(request.last_included_index);
    }
    InstallSnapshotReply {
      follower: self.id,
      term: self.current_term,
    }
  }

  /// Handles a follower's [`InstallSnapshotReply`] as the leader: a newer term steps us down; otherwise
  /// the follower now holds up to the snapshot, so its `match_index`/`next_index` advance past it and the
  /// commit index may advance.
  pub fn on_install_snapshot_reply(&mut self, reply: InstallSnapshotReply) {
    if reply.term > self.current_term {
      self.step_down(reply.term);
      return;
    }
    if self.role != Role::Leader || reply.term != self.current_term {
      return;
    }
    self.contacts.insert(reply.follower);
    self.match_index.insert(reply.follower, self.snapshot_index);
    self
      .next_index
      .insert(reply.follower, self.snapshot_index.saturating_add(1));
    self.advance_leader_commit();
  }

  /// Appends `command` to the leader's own log at the current term and updates its self-match, so a
  /// single-voter leader commits it at once (Raft §5.3, leader append). A non-leader ignores the append
  /// and reports `false` — only the leader proposes.
  pub fn append_command(&mut self, command: Vec<u8>) -> bool {
    if self.role != Role::Leader {
      return false;
    }
    self.retention_pending = true;
    self.log.push(LogEntry::command(self.current_term, command));
    self.advance_leader_commit();
    true
  }

  /// Builds the [`AppendEntries`] to send `follower`, from the leader's `next_index` for it: the entries
  /// after that point and the previous position for the consistency check. Empty entries make it a
  /// heartbeat. Returns `None` if this node is not the leader.
  pub fn replicate_to(&self, follower: HostId) -> Option<AppendEntries> {
    if self.role != Role::Leader {
      return None;
    }
    let next = self.next_index.get(&follower).copied().unwrap_or(1).max(1);
    let prev_log_index = next.saturating_sub(1);
    // The entry before the new ones has been compacted away — the follower needs an install-snapshot
    // ([`install_snapshot_for`](RaftNode::install_snapshot_for)), not an append.
    if prev_log_index < self.snapshot_index {
      return None;
    }
    let prev_log_term = if prev_log_index == 0 {
      0
    } else {
      self.entry_term(prev_log_index).unwrap_or(0)
    };
    let from = usize::try_from(next.saturating_sub(self.snapshot_index).saturating_sub(1))
      .unwrap_or(usize::MAX);
    let entries = self.log.get(from..).unwrap_or(&[]).to_vec();
    Some(AppendEntries {
      read_context: self.read_round.as_ref().map_or(0, |read| read.context),
      term: self.current_term,
      leader: self.id,
      prev_log_index,
      prev_log_term,
      entries,
      leader_commit: self.commit_index,
    })
  }

  /// Handles a received [`AppendEntries`] as a follower (Raft §5.3). A stale-term append is rejected. A
  /// current-or-newer term is recognised (this node becomes a follower for it). The append succeeds only
  /// when the log matches at `prev_log_index`/`prev_log_term`; then any conflicting suffix is truncated,
  /// the new entries appended, and the commit index advanced toward the leader's. Returns the reply,
  /// carrying on success the last index now matched.
  pub fn on_append_entries(&mut self, request: AppendEntries) -> AppendReply {
    if request.term < self.current_term {
      return self.append_reply(false, 0, 0);
    }
    if request.term > self.current_term {
      self.step_down(request.term);
    }
    // A current-term append means a leader exists for our term — defer to it (a candidate steps down)
    // and note the contact, so we refuse pre-votes that would disrupt this leader (§9.6).
    self.read_round = None;
    self.role = Role::Follower;
    self.has_leader = true;
    self.leader_hint = Some(request.leader);

    // Consistency check: our log must contain the previous entry with the leader's term.
    if request.prev_log_index > 0
      && self.entry_term(request.prev_log_index) != Some(request.prev_log_term)
    {
      return self.append_reply(false, 0, request.read_context);
    }

    // Append, truncating the first conflicting entry and everything after it.
    let mut index = request.prev_log_index;
    for entry in request.entries {
      index = index.saturating_add(1);
      match self.entry_term(index) {
        Some(term) if term == entry.term => {} // already present and matching — keep it
        Some(_) => {
          self.truncate_from(index);
          self.log.push(entry);
        }
        None => {
          self.retention_pending = true;
          self.log.push(entry);
        }
      }
    }

    // Advance the commit index to the leader's, but no further than the entries we now hold.
    if request.leader_commit > self.commit_index {
      let committed = request.leader_commit.min(index);
      self.retention_pending |= self.commit_index != committed;
      self.commit_index = committed;
    }
    self.append_reply(true, index, request.read_context)
  }

  /// Handles a follower's [`AppendReply`] as the leader (Raft §5.3). A newer term steps us down. On
  /// success the follower's `match_index`/`next_index` advance and the commit index may advance; on the
  /// consistency-check failure `next_index` backs up one so the next append tries an earlier position.
  pub fn on_append_reply(&mut self, reply: AppendReply) {
    if reply.term > self.current_term {
      self.step_down(reply.term);
      return;
    }
    if self.role != Role::Leader || reply.term != self.current_term {
      return;
    }
    if self.all_voters().contains(&reply.follower)
      && let Some(read) = self.read_round.as_mut()
      && reply.read_context == read.context
      && reply.term == read.term
    {
      read.confirmed.insert(reply.follower);
    }
    // Any same-term reply proves the follower is reachable this CheckQuorum window.
    self.contacts.insert(reply.follower);
    if reply.success {
      self.match_index.insert(reply.follower, reply.match_index);
      self
        .next_index
        .insert(reply.follower, reply.match_index.saturating_add(1));
      self.advance_leader_commit();
    } else if let Some(next) = self.next_index.get_mut(&reply.follower) {
      *next = (*next).saturating_sub(1).max(1);
    }
  }

  /// The leader's CheckQuorum tick (Raft §6.2): if the leader has not been in contact with a majority of
  /// voters since the previous check, it steps down to a follower — so a leader cut off from the cluster
  /// stops acting as leader (it will not keep serving reads or block a fresh election on the majority
  /// side). Then the contact window resets. A non-leader is unaffected, and a lone voter is always its
  /// own majority, so it never steps down (the `f = 0` degenerate).
  pub fn check_quorum(&mut self) {
    if self.role != Role::Leader {
      return;
    }
    let mut reachable = self.contacts.clone();
    reachable.insert(self.id);
    if !self.is_majority(&reachable) {
      self.read_round = None;
      self.role = Role::Follower;
      self.has_leader = false;
      self.leader_hint = None;
    }
    self.contacts.clear();
  }

  /// Starts one ReadIndex round after a current-term commit (Raft §6.4; AUD-09). The next
  /// replication/heartbeat to each voter carries this context. A second concurrent read is refused;
  /// callers may share a pending round only for reads that started before that round was sent.
  pub fn begin_read(&mut self) -> Option<u64> {
    if self.role != Role::Leader
      || self.entry_term(self.commit_index) != Some(self.current_term)
      || self.read_round.is_some()
    {
      return None;
    }
    self.read_context = self.read_context.checked_add(1)?;
    self.read_round = Some(ReadRound {
      context: self.read_context,
      term: self.current_term,
      index: self.commit_index,
      config: self.effective_config(),
      confirmed: BTreeSet::from([self.id]),
    });
    Some(self.read_context)
  }

  /// Completes `context` only after a majority has answered that read's round in the same term and
  /// voter configuration. The caller applies through the returned index before answering the read.
  /// The result is consumed: neither this context nor its old replies can authorize a later read.
  pub fn read_index(&mut self, context: u64) -> Option<u64> {
    let read = self.read_round.as_ref()?;
    if read.context != context
      || self.role != Role::Leader
      || read.term != self.current_term
      || read.config != self.effective_config()
      || !self.is_majority(&read.confirmed)
    {
      return None;
    }
    let index = read.index;
    self.read_round = None;
    Some(index)
  }

  /// Releases the single pending read after its caller times out or is cancelled (§4.3). A stale
  /// cancellation cannot remove another caller's newer round.
  pub fn cancel_read(&mut self, context: u64) {
    if self
      .read_round
      .as_ref()
      .is_some_and(|read| read.context == context)
    {
      self.read_round = None;
    }
  }

  /// Begins a membership change to `new_voters` (Raft §6 joint consensus): the node enters a **joint
  /// configuration** where every decision — election, commit, CheckQuorum — needs a majority of both the
  /// old and the new voter sets, so no two disjoint majorities can form across the change. Only the
  /// leader begins one, not while another is in flight, and not while the previous configuration entry
  /// is still uncommitted (one change at a time — Ongaro's thesis §4.1: two configuration entries in
  /// flight could let disjoint majorities form). An empty target is refused (a group cannot vote itself
  /// out of existence). Returns whether it started.
  pub fn begin_membership_change(&mut self, new_voters: Vec<HostId>) -> bool {
    let current = self.effective_config();
    if self.role != Role::Leader
      || current.joint.is_some()
      || new_voters.is_empty()
      || self.latest_config_index() > self.commit_index
    {
      return false;
    }
    self.read_round = None;
    // Append the joint configuration `C_old,new` as a log entry — it takes effect on append (§6), so the
    // very next quorum check needs a majority of both sets. It replicates like any entry.
    self.retention_pending = true;
    self.log.push(LogEntry::configuration(
      self.current_term,
      VoterConfig {
        voters: current.voters,
        joint: Some(new_voters),
      },
    ));
    self.advance_leader_commit();
    true
  }

  /// Completes a membership change: leaves the joint configuration, adopting the new voter set as the
  /// sole configuration (Raft §6, the transition to `C_new`). Only valid while a change is in flight, and
  /// only once the joint configuration entry has itself **committed** — enforced here, so a caller cannot
  /// leave the joint phase early. Returns whether it completed. A leader not named by `C_new` keeps
  /// leading until that entry commits, then steps down ([`advance_leader_commit`] applies §4.2.2).
  ///
  /// [`advance_leader_commit`]: RaftNode::advance_leader_commit
  pub fn complete_membership_change(&mut self) -> bool {
    let current = self.effective_config();
    let Some(new_voters) = current.joint else {
      return false;
    };
    if self.role != Role::Leader || self.latest_config_index() > self.commit_index {
      return false;
    }
    self.read_round = None;
    // Append the final configuration `C_new` (§6): the change is done once this commits.
    self.retention_pending = true;
    self.log.push(LogEntry::configuration(
      self.current_term,
      VoterConfig {
        voters: new_voters,
        joint: None,
      },
    ));
    self.advance_leader_commit();
    true
  }

  /// Truncates the log from the one-based `index` onward (removing that entry and every later one).
  fn truncate_from(&mut self, index: u64) {
    let keep = usize::try_from(index.saturating_sub(self.snapshot_index).saturating_sub(1))
      .unwrap_or(usize::MAX);
    self.retention_pending |= keep < self.log.len();
    self.log.truncate(keep);
  }

  /// A follower's reply with this node's current term.
  fn append_reply(&self, success: bool, match_index: u64, read_context: u64) -> AppendReply {
    AppendReply {
      read_context,
      follower: self.id,
      term: self.current_term,
      success,
      match_index,
    }
  }

  /// Advances the leader's commit index (Raft §5.4.2): the highest index a majority of voters hold whose
  /// entry is from the **current term**. Earlier-term entries are not committed by replica count alone —
  /// they commit only once a current-term entry above them does — which is the safety subtlety Raft's
  /// figure 8 exposes. A commit that carries the leader's own removal into force steps it down
  /// ([`step_down_if_removed`](RaftNode::step_down_if_removed)).
  fn advance_leader_commit(&mut self) {
    if self.role != Role::Leader {
      return;
    }
    let mut candidate = self.last_log_index();
    while candidate > self.commit_index {
      if self.entry_term(candidate) == Some(self.current_term) {
        let holders: BTreeSet<HostId> = self
          .all_voters()
          .into_iter()
          .filter(|voter| self.match_of(*voter) >= candidate)
          .collect();
        if self.is_majority(&holders) {
          self.retention_pending = true;
          self.commit_index = candidate;
          self.step_down_if_removed();
          return;
        }
      }
      candidate = candidate.saturating_sub(1);
    }
  }

  /// How far `voter`'s log matches the leader's: the leader's own last index for itself, else the
  /// follower's tracked `match_index` (nothing for a follower not yet replicated to).
  fn match_of(&self, voter: HostId) -> u64 {
    if voter == self.id {
      self.last_log_index()
    } else {
      self.match_index.get(&voter).copied().unwrap_or(0)
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const A: HostId = HostId(1);
  const B: HostId = HostId(2);
  const C: HostId = HostId(3);
  const D: HostId = HostId(4);
  const E: HostId = HostId(5);

  /// A fixture with an explicitly uncommitted, uncompacted prefix. Production recovery takes
  /// the complete SavedRaft value, including snapshot and commit position.
  fn node_with_uncommitted_log(
    id: HostId,
    voters: Vec<HostId>,
    term: u64,
    voted_for: Option<HostId>,
    log: Vec<LogEntry>,
  ) -> RaftNode {
    RaftNode::restore(SavedRaft {
      id,
      base: VoterConfig {
        voters,
        joint: None,
      },
      term,
      voted_for,
      log,
      commit_index: 0,
      snapshot_index: 0,
      snapshot_term: 0,
      snapshot_data: Vec::new(),
    })
    .expect("a valid fixture prefix")
  }

  fn request_from(candidate: HostId, term: u64) -> RequestVote {
    RequestVote {
      term,
      candidate,
      last_log_index: 0,
      last_log_term: 0,
    }
  }

  /// A log with one entry per given term (a distinct command each, so entries are unequal).
  fn log_of(terms: &[u64]) -> Vec<LogEntry> {
    terms
      .iter()
      .enumerate()
      .map(|(index, &term)| LogEntry::command(term, vec![u8::try_from(index).unwrap_or(u8::MAX)]))
      .collect()
  }

  /// Drives a candidate to leadership among `voters` by granting it every other voter's vote.
  fn elected_leader(id: HostId, voters: Vec<HostId>) -> RaftNode {
    let mut node = RaftNode::new(id, voters.clone());
    node.start_election();
    for voter in voters {
      if voter != id {
        node.on_vote_reply(VoteReply {
          voter,
          term: node.term(),
          granted: true,
        });
      }
    }
    node
  }

  /// Drives replication from `leader` to `follower` (id `who`), applying each reply, until an append can
  /// no longer be built (the follower needs a snapshot). Returns whether it became stuck; bounded.
  fn replicate_until_stuck(leader: &mut RaftNode, follower: &mut RaftNode, who: HostId) -> bool {
    for _ in 0..8 {
      let Some(append) = leader.replicate_to(who) else {
        return true;
      };
      let reply = follower.on_append_entries(append);
      leader.on_append_reply(reply);
    }
    false
  }

  /// AC-8.1, §4.8, AUD-07: a damaged retained prefix cannot become a voter; a complete
  /// wire round-trip preserves the vote and rejects a competing candidate in the same term.
  #[test]
  fn damaged_voter_publications_are_refused_before_voting() {
    use slates_wire::Wire;
    let mut voter = RaftNode::new(A, vec![A, B, C]);
    assert!(
      voter
        .on_request_vote(RequestVote {
          term: 7,
          candidate: B,
          last_log_index: 0,
          last_log_term: 0,
        })
        .granted
    );
    let saved = voter.saved();
    let bytes = saved.to_bytes();
    for end in 0..bytes.len() {
      assert!(SavedRaft::from_bytes(&bytes[..end]).is_err());
    }
    let mut recovered = RaftNode::restore(SavedRaft::from_bytes(&bytes).unwrap()).unwrap();
    assert!(
      !recovered
        .on_request_vote(RequestVote {
          term: 7,
          candidate: C,
          last_log_index: 0,
          last_log_term: 0,
        })
        .granted
    );
    let mut damaged = saved.clone();
    damaged.commit_index = 1;
    assert_eq!(
      RaftNode::restore(damaged).err(),
      Some(RaftRecoveryError::InvalidCommitIndex)
    );
    let mut damaged = saved.clone();
    damaged.base.voters.push(A);
    assert_eq!(
      RaftNode::restore(damaged).err(),
      Some(RaftRecoveryError::InvalidVoters)
    );
    let mut damaged = saved.clone();
    damaged.snapshot_index = 1;
    assert_eq!(
      RaftNode::restore(damaged).err(),
      Some(RaftRecoveryError::InvalidSnapshot)
    );
    let mut damaged = saved;
    damaged.log.push(LogEntry::command(8, vec![1]));
    assert_eq!(
      RaftNode::restore(damaged).err(),
      Some(RaftRecoveryError::InvalidLogTerm)
    );
  }

  /// AC-8.1, §4.8, AUD-07: retain a vote, compact a committed command and change membership.
  /// Restore the prefix, snapshot and effective voters together, and refuse a competing vote.
  #[test]
  fn recovery_preserves_the_vote_snapshot_and_membership_with_the_log() {
    let mut node = RaftNode::new(A, vec![A]);
    node.start_election();
    assert!(node.append_command(b"committed before restart".to_vec()));
    assert!(node.compact(1, b"state at index one".to_vec()));
    assert!(node.begin_membership_change(vec![A, B]));
    let mut follower = RaftNode::new(B, vec![A]);
    // Catch up from the compacted prefix before accepting the joint configuration.
    let snapshot = node.install_snapshot_for(B).unwrap();
    node.on_install_snapshot_reply(follower.on_install_snapshot(snapshot));
    let append = node.replicate_to(B).unwrap();
    node.on_append_reply(follower.on_append_entries(append));
    let saved = node.saved();
    let restored = RaftNode::restore(saved.clone()).unwrap();
    assert_eq!(restored.saved(), saved);
    assert_eq!(restored.role(), Role::Follower);
    assert!(!restored.is_leader());
    let mut restored = restored;
    assert!(
      !restored
        .on_request_vote(RequestVote {
          term: 1,
          candidate: B,
          last_log_index: 2,
          last_log_term: 1,
        })
        .granted,
      "the recovered self-vote must still exclude another candidate"
    );
    assert_eq!(restored.all_voters(), vec![A, B]);
  }

  /// AC-8.1, §4.8 restart-as-join; AUD-07: a fresh replacement is outside the existing voter
  /// configuration. It may receive replication, but neither a vote request nor direct campaign
  /// entrypoint may let it supply a vote before a membership entry admits it.
  #[test]
  fn a_learner_neither_grants_votes_nor_campaigns_before_admission() {
    let mut learner = RaftNode::new(D, vec![A, B, C]);
    let request = RequestVote {
      term: 7,
      candidate: A,
      last_log_index: 0,
      last_log_term: 0,
    };
    assert!(!learner.on_request_vote(request).granted);
    assert!(
      !learner
        .on_pre_vote(PreVote {
          term: 8,
          candidate: A,
          last_log_index: 0,
          last_log_term: 0,
        })
        .granted
    );
    assert!(learner.start_election().is_empty());
    assert_eq!(learner.role(), Role::Follower);
  }

  /// A single-voter group elects itself: its vote reaches a majority of one, so it becomes
  /// leader for term 1 without sending messages (the `f = 0` degenerate).
  #[test]
  fn a_single_voter_elects_itself_leader() {
    let mut node = RaftNode::new(A, vec![A]);
    let requests = node.start_election();
    assert!(requests.is_empty(), "a lone voter sends no requests");
    assert!(node.is_leader(), "and is immediately leader");
    assert_eq!(node.term(), 1);
  }

  /// A candidate that gathers a majority of votes becomes leader; among three voters, its own vote plus
  /// one granted reply is the majority.
  #[test]
  fn a_candidate_becomes_leader_at_a_majority() {
    let mut node = RaftNode::new(A, vec![A, B, C]);
    let requests = node.start_election();
    assert_eq!(requests.len(), 2, "a request to each other voter");
    assert_eq!(node.role(), Role::Candidate);

    node.on_vote_reply(VoteReply {
      voter: B,
      term: 1,
      granted: true,
    });
    assert!(node.is_leader(), "self plus one of three is a majority");
  }

  /// A candidate short of a majority stays a candidate: among five voters, its own vote plus one is not
  /// enough.
  #[test]
  fn a_split_vote_stays_a_candidate() {
    let mut node = RaftNode::new(A, vec![A, B, C, D, E]);
    node.start_election();
    node.on_vote_reply(VoteReply {
      voter: B,
      term: 1,
      granted: true,
    });
    assert_eq!(
      node.role(),
      Role::Candidate,
      "two of five is not a majority"
    );
    node.on_vote_reply(VoteReply {
      voter: C,
      term: 1,
      granted: true,
    });
    assert!(node.is_leader(), "three of five is");
  }

  /// A voter grants its vote once per term: having voted for one candidate, it denies another in the
  /// same term, but grants the one it already voted for (idempotent retry).
  #[test]
  fn a_vote_is_granted_once_per_term() {
    let mut node = RaftNode::new(A, vec![A, B, C]);
    assert!(
      node.on_request_vote(request_from(B, 1)).granted,
      "first candidate wins the vote"
    );
    assert!(
      !node.on_request_vote(request_from(C, 1)).granted,
      "a second candidate is denied"
    );
    assert!(
      node.on_request_vote(request_from(B, 1)).granted,
      "the same candidate is granted again (a lost reply retried)"
    );
  }

  /// A request under a newer term steps a candidate down to a follower and can win its vote (its own
  /// candidacy is abandoned for the newer term).
  #[test]
  fn a_newer_term_steps_a_candidate_down_and_can_win_its_vote() {
    let mut node = RaftNode::new(A, vec![A, B, C]);
    node.start_election(); // A is a candidate at term 1
    assert_eq!(node.role(), Role::Candidate);

    let reply = node.on_request_vote(request_from(B, 2));
    assert!(reply.granted, "the newer-term candidate wins the vote");
    assert_eq!(node.role(), Role::Follower, "and A is now a follower");
    assert_eq!(node.term(), 2, "at the newer term");
  }

  /// A vote request under a stale term is denied, and the reply carries our current term so the stale
  /// candidate learns it is behind.
  #[test]
  fn a_stale_term_request_is_denied() {
    let mut node = node_with_uncommitted_log(A, vec![A, B, C], 5, None, Vec::new());
    let reply = node.on_request_vote(request_from(B, 3));
    assert!(!reply.granted, "a term-3 request is stale at term 5");
    assert_eq!(reply.term, 5, "the reply reports the current term");
  }

  /// The election restriction (Raft §5.4.1): a candidate whose log is less up-to-date than ours is
  /// denied, while one at least as up-to-date is granted — so a leader's log never omits a committed
  /// entry.
  #[test]
  fn a_less_up_to_date_candidate_is_denied() {
    // Our last log entry is at index 3, term 2.
    let mut node = node_with_uncommitted_log(A, vec![A, B, C], 2, None, log_of(&[1, 1, 2]));

    // A candidate at the same term but a shorter log is denied.
    let behind = RequestVote {
      term: 3,
      candidate: B,
      last_log_index: 2,
      last_log_term: 2,
    };
    assert!(
      !node.on_request_vote(behind).granted,
      "a shorter log at the same term is not current"
    );

    // A fresh node at the same state grants a candidate with a later last-log term despite a shorter log.
    let mut node = node_with_uncommitted_log(A, vec![A, B, C], 2, None, log_of(&[1, 1, 2]));
    let ahead = RequestVote {
      term: 3,
      candidate: C,
      last_log_index: 1,
      last_log_term: 3,
    };
    assert!(
      node.on_request_vote(ahead).granted,
      "a later last-log term is more up-to-date"
    );
  }

  /// A single-voter leader commits its own appends at once — a majority of one (the `f = 0` degenerate
  /// of log replication).
  #[test]
  fn a_lone_leader_commits_its_own_appends() {
    let mut node = elected_leader(A, vec![A]);
    assert!(node.is_leader());
    assert!(node.append_command(b"cfg-1".to_vec()));
    assert_eq!(
      node.commit_index(),
      1,
      "a lone voter's append is immediately committed"
    );
    assert!(node.append_command(b"cfg-2".to_vec()));
    assert_eq!(node.commit_index(), 2);
    assert_eq!(
      node.committed_entries().len(),
      2,
      "both appends are in the committed prefix"
    );
  }

  /// A leader replicates an append to a follower; once a majority (leader plus one of three) holds the
  /// entry, it commits.
  #[test]
  fn an_append_commits_once_a_majority_replicates_it() {
    let mut leader = elected_leader(A, vec![A, B, C]);
    leader.append_command(b"cfg-1".to_vec());
    assert_eq!(
      leader.commit_index(),
      0,
      "not committed until a majority holds it"
    );

    let to_b = leader.replicate_to(B).expect("leader replicates");
    assert_eq!(to_b.entries.len(), 1);
    let mut follower = RaftNode::new(B, vec![A, B, C]);
    let reply = follower.on_append_entries(to_b);
    assert!(reply.success, "the follower accepts the append");
    leader.on_append_reply(reply);

    assert_eq!(
      leader.commit_index(),
      1,
      "leader plus one follower is a majority of three"
    );
    assert_eq!(follower.last_log_index(), 1, "the follower holds the entry");
  }

  /// A follower whose log does not match at the previous position rejects the append; the leader backs up
  /// `next_index` and retries from earlier until the follower's log is repaired to agree (the log-repair
  /// loop, §5.3).
  #[test]
  fn a_mismatched_follower_is_repaired_by_backing_up() {
    // A leader for a fresh term over a two-entry log, plus one new current-term entry.
    let mut leader = node_with_uncommitted_log(A, vec![A, B, C], 3, None, log_of(&[3, 3]));
    leader.start_election(); // term 4
    leader.on_vote_reply(VoteReply {
      voter: B,
      term: leader.term(),
      granted: true,
    });
    assert!(leader.is_leader());
    leader.append_command(b"cfg-new".to_vec()); // index 3, term 4

    // A follower with a single conflicting entry (term 1) at index 1.
    let mut follower = node_with_uncommitted_log(B, vec![A, B, C], 1, None, log_of(&[1]));

    let first = leader.replicate_to(B).expect("append");
    let reply = follower.on_append_entries(first);
    assert!(!reply.success, "a mismatched previous entry is rejected");
    leader.on_append_reply(reply);

    for _ in 0..5 {
      let append = leader.replicate_to(B).expect("append");
      let reply = follower.on_append_entries(append);
      leader.on_append_reply(reply);
      if reply.success {
        break;
      }
    }
    assert_eq!(
      follower.last_log_index(),
      leader.last_log_index(),
      "the follower's log is repaired to match the leader's"
    );
    assert_eq!(
      follower.last_log_term(),
      4,
      "including the leader's newest entry"
    );
  }

  /// The commit-safety rule (Raft §5.4.2): a leader does not commit an entry from an earlier term by
  /// replica count alone — committing a current-term entry is what carries the earlier ones with it.
  #[test]
  fn an_earlier_term_entry_is_not_committed_by_count_alone() {
    // A leader for term 5 holding one entry left over from term 2 (index 1).
    let mut leader = node_with_uncommitted_log(A, vec![A, B, C], 4, None, log_of(&[2]));
    leader.start_election(); // term 5
    leader.on_vote_reply(VoteReply {
      voter: B,
      term: leader.term(),
      granted: true,
    });
    assert!(leader.is_leader());

    // A majority replicates the old (term-2) entry — it must NOT be committed by count alone.
    let mut follower = node_with_uncommitted_log(B, vec![A, B, C], 5, None, Vec::new());
    let append = leader.replicate_to(B).expect("append");
    let reply = follower.on_append_entries(append);
    leader.on_append_reply(reply);
    assert_eq!(
      leader.commit_index(),
      0,
      "an earlier-term entry is not committed by replica count"
    );

    // Appending and replicating a current-term entry commits both together.
    leader.append_command(b"cfg-5".to_vec()); // index 2, term 5
    let append = leader.replicate_to(B).expect("append");
    let reply = follower.on_append_entries(append);
    leader.on_append_reply(reply);
    assert_eq!(
      leader.commit_index(),
      2,
      "committing the current-term entry carries the earlier one"
    );
  }

  /// A lone voter's pre-vote round carries at once and proceeds straight to a real election, so it leads
  /// (the `f = 0` degenerate of PreVote).
  #[test]
  fn a_solo_node_pre_elects_and_leads() {
    let mut node = RaftNode::new(A, vec![A]);
    let pre_votes = node.on_election_timeout();
    assert!(pre_votes.is_empty(), "a lone voter sends no pre-votes");
    assert!(node.is_leader(), "and proceeds straight to leadership");
    assert_eq!(node.term(), 1);
  }

  /// The anti-disruption property (Raft §9.6): a node that has heard from a leader refuses a pre-vote —
  /// even one at a far higher term — and, crucially, its own term is left untouched, so a partitioned,
  /// term-inflated node that rejoins cannot force the healthy leader to step down.
  #[test]
  fn a_partitioned_node_cannot_disrupt_a_node_with_a_leader() {
    let mut node = RaftNode::new(B, vec![A, B, C]);
    // B hears a heartbeat from leader A at term 1.
    node.on_append_entries(AppendEntries {
      read_context: 0,
      term: 1,
      leader: A,
      prev_log_index: 0,
      prev_log_term: 0,
      entries: Vec::new(),
      leader_commit: 0,
    });
    assert_eq!(node.term(), 1);

    // A partitioned node with an inflated term asks for a pre-vote.
    let reply = node.on_pre_vote(PreVote {
      term: 10,
      candidate: C,
      last_log_index: 0,
      last_log_term: 0,
    });
    assert!(
      !reply.granted,
      "a node with a live leader refuses the pre-vote"
    );
    assert_eq!(
      node.term(),
      1,
      "and its term is not inflated by the pre-vote"
    );
  }

  /// A pre-vote majority starts the real election: a pre-candidate that gathers a majority of pre-votes
  /// increments its term and issues real vote requests.
  #[test]
  fn pre_votes_from_a_majority_start_a_real_election() {
    let mut node = RaftNode::new(A, vec![A, B, C]);
    let pre_votes = node.on_election_timeout();
    assert_eq!(pre_votes.len(), 2, "a pre-vote to each other voter");
    assert_eq!(node.role(), Role::PreCandidate);
    assert_eq!(
      node.term(),
      0,
      "the term is not inflated during the pre-vote round"
    );

    let requests = node.on_pre_vote_reply(PreVoteReply {
      voter: B,
      term: 1,
      granted: true,
    });
    let requests = requests.expect("a pre-vote majority starts the real election");
    assert_eq!(requests.len(), 2, "real vote requests are issued");
    assert_eq!(node.role(), Role::Candidate);
    assert_eq!(node.term(), 1, "now the term advances");
  }

  /// A candidate whose term is behind is refused a pre-vote even by a leaderless peer — its pre-vote term
  /// does not exceed the peer's, so it could not win a real election either.
  #[test]
  fn a_behind_candidate_is_refused_a_pre_vote() {
    // A leaderless peer at term 5.
    let node = node_with_uncommitted_log(A, vec![A, B, C], 5, None, Vec::new());
    let reply = node.on_pre_vote(PreVote {
      term: 3,
      candidate: B,
      last_log_index: 0,
      last_log_term: 0,
    });
    assert!(
      !reply.granted,
      "a pre-vote term not ahead of ours is refused"
    );
  }

  /// CheckQuorum (Raft §6.2): a leader that goes a whole window without contact from a majority steps
  /// down. The first check after election still counts the electing votes; a second, with no replies
  /// since, finds only itself and relinquishes leadership.
  #[test]
  fn a_leader_without_a_quorum_steps_down() {
    let mut leader = elected_leader(A, vec![A, B, C]);
    assert!(leader.is_leader());

    leader.check_quorum(); // window one: still counts the electing majority
    assert!(
      leader.is_leader(),
      "the freshly-won leader is not stepped down"
    );

    leader.check_quorum(); // window two: no contact since — steps down
    assert_eq!(
      leader.role(),
      Role::Follower,
      "a leader cut off from a majority steps down"
    );
  }

  /// The leader-redirection hint ([`RaftNode::leader`]): a leader reports itself, a follower learns the leader
  /// from an accepted append, and campaigning forgets it. A hint for redirecting an operator command to the
  /// leader — never a safety input.
  #[test]
  fn the_leader_hint_tracks_the_current_leader() {
    // A leader reports itself.
    let leader = elected_leader(A, vec![A, B, C]);
    assert_eq!(leader.leader(), Some(A));

    // A fresh follower knows no leader until it accepts an append, then reports its sender.
    let mut node = RaftNode::new(B, vec![A, B, C]);
    assert_eq!(node.leader(), None);
    node.on_append_entries(AppendEntries {
      read_context: 0,
      term: 1,
      leader: A,
      prev_log_index: 0,
      prev_log_term: 0,
      entries: Vec::new(),
      leader_commit: 0,
    });
    assert_eq!(
      node.leader(),
      Some(A),
      "a follower learns the leader from its append"
    );

    // Campaigning forgets the hint, so a candidate never redirects to a stale leader.
    node.on_election_timeout();
    assert_eq!(node.leader(), None, "a campaigning node forgets its leader");
  }

  /// A leader that keeps hearing from a follower stays leader across checks — the contact refreshes the
  /// window.
  #[test]
  fn a_leader_with_a_quorum_stays() {
    let mut leader = elected_leader(A, vec![A, B, C]);
    leader.check_quorum(); // resets the window

    // A follower replies within the new window, so the leader is in contact with a majority.
    leader.on_append_reply(AppendReply {
      read_context: 0,
      follower: B,
      term: leader.term(),
      success: true,
      match_index: 0,
    });
    leader.check_quorum();
    assert!(
      leader.is_leader(),
      "a leader in contact with a majority stays"
    );
  }

  /// A lone leader never steps down under CheckQuorum — it is always its own majority (the `f = 0`
  /// degenerate).
  #[test]
  fn a_solo_leader_never_steps_down() {
    let mut leader = elected_leader(A, vec![A]);
    for _ in 0..3 {
      leader.check_quorum();
    }
    assert!(
      leader.is_leader(),
      "a single voter is always its own quorum"
    );
  }

  /// ReadIndex (Raft §6.4): a leader serves a read only after committing in its current term. A lone
  /// leader has no read index until it commits an entry; then the read index is its commit index.
  #[test]
  fn a_leader_serves_a_read_index_after_committing_in_its_term() {
    let mut leader = elected_leader(A, vec![A]);
    assert_eq!(
      leader.begin_read(),
      None,
      "no read before a current-term commit"
    );

    leader.append_command(b"cfg-1".to_vec()); // commits at once (f = 0)
    let read = leader
      .begin_read()
      .expect("current-term commit starts a read");
    assert_eq!(
      leader.read_index(read),
      Some(1),
      "the read index is the current commit index"
    );
  }

  /// A non-leader never provides a read index — only the leader may serve a linearizable read.
  #[test]
  fn a_non_leader_has_no_read_index() {
    let mut follower = RaftNode::new(B, vec![A, B, C]);
    assert_eq!(follower.begin_read(), None);
  }

  /// AC-2.5, §4.8 ReadSafety; AUD-09: after A's commit, isolate A and elect B, then commit a
  /// later value through B and C. A has not run CheckQuorum yet. Its old contacts must not authorize
  /// a new read of the superseded value.
  #[test]
  fn historical_contacts_cannot_confirm_a_read_after_a_new_leader_commits() {
    let mut old = elected_leader(A, vec![A, B, C]);
    let mut successor = RaftNode::new(B, vec![A, B, C]);
    let mut third = RaftNode::new(C, vec![A, B, C]);
    old.append_command(b"old".to_vec());
    old.on_append_reply(successor.on_append_entries(old.replicate_to(B).unwrap()));
    let election = successor.start_election();
    successor.on_vote_reply(third.on_request_vote(election[0]));
    assert!(successor.is_leader());
    successor.append_command(b"new".to_vec());
    successor.on_append_reply(third.on_append_entries(successor.replicate_to(C).unwrap()));
    successor.on_append_reply(third.on_append_entries(successor.replicate_to(C).unwrap()));
    assert_eq!(successor.commit_index(), 2);
    let read = old
      .begin_read()
      .expect("old leader has not stepped down yet");
    assert_eq!(
      old.read_index(read),
      None,
      "pre-read contacts cannot prove current read authority"
    );
  }

  /// AC-2.5, §4.8 ReadSafety; AUD-09: old heartbeats and completed read rounds cannot confirm a
  /// new read. The follower's reply must echo the new context; a result is consumed exactly once.
  #[test]
  fn each_read_needs_its_own_confirmation_round() {
    let mut leader = elected_leader(A, vec![A, B, C]);
    let mut follower = RaftNode::new(B, vec![A, B, C]);
    leader.append_command(b"value".to_vec());
    let old = follower.on_append_entries(leader.replicate_to(B).unwrap());
    leader.on_append_reply(old);
    let first = leader.begin_read().unwrap();
    assert_eq!(
      leader.begin_read(),
      None,
      "one pending round is the admission bound"
    );
    leader.on_append_reply(old);
    assert_eq!(leader.read_index(first), None);
    let confirmed = follower.on_append_entries(leader.replicate_to(B).unwrap());
    leader.on_append_reply(confirmed);
    assert_eq!(leader.read_index(first), Some(1));
    assert_eq!(
      leader.read_index(first),
      None,
      "a proof cannot authorize another read"
    );
    let second = leader.begin_read().unwrap();
    leader.on_append_reply(confirmed);
    leader.cancel_read(first);
    assert_eq!(
      leader.read_index(second),
      None,
      "old replies and cancellation cannot complete the new round"
    );
    leader.on_append_reply(follower.on_append_entries(leader.replicate_to(B).unwrap()));
    assert_eq!(leader.read_index(second), Some(1));
  }

  /// AC-2.5, §4.8 ReadSafety; AUD-09: two voters of five are a minority even with duplicate or
  /// foreign replies. Changing the voter configuration cancels the pending proof.
  #[test]
  fn read_confirmation_counts_distinct_current_voters_and_cancels_on_reconfiguration() {
    let voters = vec![A, B, C, D, E];
    let mut leader = elected_leader(A, voters.clone());
    leader.append_command(b"value".to_vec());
    let mut second = RaftNode::new(B, voters.clone());
    let mut third = RaftNode::new(C, voters);
    leader.on_append_reply(second.on_append_entries(leader.replicate_to(B).unwrap()));
    leader.on_append_reply(third.on_append_entries(leader.replicate_to(C).unwrap()));
    let read = leader.begin_read().unwrap();
    let reply = second.on_append_entries(leader.replicate_to(B).unwrap());
    leader.on_append_reply(reply);
    leader.on_append_reply(reply);
    leader.on_append_reply(AppendReply {
      follower: HostId(999),
      ..reply
    });
    assert_eq!(leader.read_index(read), None);
    assert!(leader.begin_membership_change(vec![A, B, C]));
    leader.on_append_reply(third.on_append_entries(leader.replicate_to(C).unwrap()));
    assert_eq!(
      leader.read_index(read),
      None,
      "the old configuration's proof is cancelled"
    );
    let replacement = leader.begin_read().unwrap();
    leader.cancel_read(replacement);
    assert!(
      leader.begin_read().is_some(),
      "a cancelled caller releases the round"
    );
  }

  /// The §6.4 safety: a leader that has only an inherited (earlier-term) commit index cannot serve a
  /// linearizable read until it commits an entry in its own term — so it never serves a read at a commit
  /// index it has not confirmed under its own leadership.
  #[test]
  fn a_leader_without_a_current_term_commit_has_no_read_index() {
    // Elected at a fresh term over an old-term log; recovered resets the commit index to zero.
    let mut leader = node_with_uncommitted_log(A, vec![A, B, C], 3, None, log_of(&[3]));
    leader.start_election(); // term 4
    leader.on_vote_reply(VoteReply {
      voter: B,
      term: leader.term(),
      granted: true,
    });
    assert!(leader.is_leader());
    assert_eq!(
      leader.begin_read(),
      None,
      "no read until a term-4 entry commits"
    );

    // Commit a current-term entry with a majority (a follower that already holds the term-3 prefix).
    leader.append_command(b"cfg-4".to_vec());
    let mut follower = node_with_uncommitted_log(B, vec![A, B, C], 4, None, log_of(&[3]));
    let append = leader.replicate_to(B).expect("append");
    let reply = follower.on_append_entries(append);
    assert!(
      reply.success,
      "the follower with the matching prefix accepts the append"
    );
    leader.on_append_reply(reply);
    let read = leader
      .begin_read()
      .expect("current-term commit starts a read");
    assert_eq!(leader.read_index(read), None);
    leader.on_append_reply(follower.on_append_entries(leader.replicate_to(B).unwrap()));
    assert_eq!(
      leader.read_index(read),
      Some(2),
      "a term-4 commit enables the read at index 2"
    );
  }

  /// Joint consensus (Raft §6): during a membership change a commit needs a majority of BOTH the old and
  /// the new voter sets. A majority of the old configuration alone does not commit; only when both
  /// configurations hold the entry does it commit — so no two disjoint majorities can form across a
  /// change.
  #[test]
  fn a_joint_change_needs_a_majority_of_both_configurations() {
    let mut leader = elected_leader(A, vec![A, B, C]);
    assert!(
      leader.begin_membership_change(vec![C, D, E]),
      "the leader enters the joint configuration"
    );
    assert!(leader.in_joint_configuration());

    leader.append_command(b"x".to_vec());
    let term = leader.term();
    let reply = |follower| AppendReply {
      read_context: 0,
      follower,
      term,
      success: true,
      match_index: 1,
    };

    // B is a majority of the old set {A,B,C} together with A, but holds no majority of the new set.
    leader.on_append_reply(reply(B));
    assert_eq!(
      leader.commit_index(),
      0,
      "a majority of the old configuration alone does not commit during a joint change"
    );

    // C and D bring a majority of the new set {C,D,E} too (with A and B, still a majority of the old).
    leader.on_append_reply(reply(C));
    leader.on_append_reply(reply(D));
    assert_eq!(
      leader.commit_index(),
      1,
      "a majority of both configurations commits"
    );
  }

  /// Completing a change leaves the joint configuration for the new voter set alone, after which a
  /// majority is measured against the new set only. It completes only once the joint entry has committed
  /// under both sets (the core enforces the gate).
  #[test]
  fn completing_a_change_adopts_the_new_configuration() {
    let mut leader = elected_leader(A, vec![A, B, C]);
    leader.begin_membership_change(vec![C, D, E]);
    assert!(leader.in_joint_configuration());

    // Commit the joint entry: B with A carries the old set {A,B,C}; C and D carry the new set {C,D,E}.
    let term = leader.term();
    for follower in [B, C, D] {
      leader.on_append_reply(AppendReply {
        read_context: 0,
        follower,
        term,
        success: true,
        match_index: 1,
      });
    }
    assert_eq!(leader.commit_index(), 1, "the joint entry committed");
    assert!(leader.complete_membership_change(), "the change completes");
    assert!(!leader.in_joint_configuration(), "no longer joint");
    assert_eq!(
      leader.all_voters(),
      vec![C, D, E],
      "the new configuration is the sole one"
    );
  }

  /// Only a leader may begin a membership change — a follower cannot.
  #[test]
  fn only_a_leader_begins_a_change() {
    let mut follower = RaftNode::new(A, vec![A, B, C]);
    assert!(!follower.begin_membership_change(vec![C, D, E]));
    assert!(!follower.in_joint_configuration());
  }

  /// A leader whose own removal commits **steps down** (Ongaro's thesis §4.2.2) — and only then: while the
  /// change is joint the old configuration still names it and it keeps leading. Both configurations commit
  /// the joint entry; `C_new` commits under the new majority alone; the leader is then a follower, no
  /// longer a voter, and never campaigns again.
  #[test]
  fn a_removed_leader_steps_down_once_its_removal_commits() {
    let mut leader = elected_leader(A, vec![A, B, C]);
    assert!(leader.begin_membership_change(vec![B, C]));
    let term = leader.term();
    let reply = |follower, match_index| AppendReply {
      read_context: 0,
      follower,
      term,
      success: true,
      match_index,
    };
    // The joint entry (index 1): B with A carries the old set but not the new one, so the change cannot
    // complete yet; C's acknowledgement commits it, and the joint configuration still names the leader.
    leader.on_append_reply(reply(B, 1));
    let completed_early = leader.complete_membership_change();
    leader.on_append_reply(reply(C, 1));
    let joint = (completed_early, leader.commit_index(), leader.is_leader());
    assert_eq!(
      joint,
      (false, 1, true),
      "no completion before the joint entry commits; committed by C; still leading while joint"
    );
    let completed = leader.complete_membership_change();
    assert_eq!(
      (completed, leader.all_voters()),
      (true, vec![B, C]),
      "C_new is appended once the joint entry committed"
    );
    // C_new (index 2) commits under the new majority alone (B and C): the leader steps down, is no longer
    // a voter, and never campaigns again.
    leader.on_append_reply(reply(B, 2));
    leader.on_append_reply(reply(C, 2));
    let campaign = leader.on_election_timeout();
    assert_eq!(
      (
        leader.commit_index(),
        leader.role(),
        leader.is_voter(A),
        campaign.is_empty()
      ),
      (2, Role::Follower, false, true),
      "committed under the new majority; the removed leader stepped down and does not campaign"
    );
  }

  /// One change at a time (thesis §4.1): a change cannot begin while the previous configuration entry is
  /// uncommitted, a joint change cannot complete before its joint entry commits, and an empty voter set is
  /// never a target.
  #[test]
  fn a_change_waits_for_the_previous_configuration_entry_to_commit() {
    let mut leader = elected_leader(A, vec![A, B, C]);
    assert!(
      !leader.begin_membership_change(Vec::new()),
      "an empty voter set is refused"
    );
    assert!(leader.begin_membership_change(vec![A, B]));
    assert!(
      !leader.complete_membership_change(),
      "the joint entry is not yet committed"
    );
    let term = leader.term();
    let reply = |follower, match_index| AppendReply {
      read_context: 0,
      follower,
      term,
      success: true,
      match_index,
    };
    leader.on_append_reply(reply(B, 1));
    assert_eq!(
      leader.commit_index(),
      1,
      "A and B carry both the old and the new set"
    );
    assert!(leader.complete_membership_change());
    assert!(
      !leader.begin_membership_change(vec![A]),
      "C_new is uncommitted: no further change may begin"
    );
    leader.on_append_reply(reply(B, 2));
    assert_eq!(leader.commit_index(), 2);
    assert!(
      leader.begin_membership_change(vec![A]),
      "once it commits the next change may begin"
    );
  }

  /// Outgoing voters stay replication targets until the entry that removes them commits — so a live
  /// member demoted to learner receives it — and drop out the moment it does.
  #[test]
  fn outgoing_voters_are_replicated_to_until_their_removal_commits() {
    let mut leader = elected_leader(A, vec![A, B, C]);
    assert!(leader.begin_membership_change(vec![A, B]));
    let term = leader.term();
    let reply = |follower, match_index| AppendReply {
      read_context: 0,
      follower,
      term,
      success: true,
      match_index,
    };
    leader.on_append_reply(reply(B, 1));
    assert!(leader.complete_membership_change());
    assert_eq!(
      leader.all_voters(),
      vec![A, B],
      "C_new is in effect on append"
    );
    assert_eq!(
      leader.replication_targets(),
      vec![A, B, C],
      "C, outgoing, is still replicated to while C_new is uncommitted"
    );
    leader.on_append_reply(reply(B, 2));
    assert_eq!(leader.commit_index(), 2);
    assert_eq!(
      leader.replication_targets(),
      vec![A, B],
      "committed: the outgoing voter drops out"
    );
  }

  /// Compaction (Raft §7) folds the committed prefix into a snapshot and discards it, bounding the log,
  /// while every index still resolves — the last index is unchanged and appends continue past the
  /// snapshot. Compacting backward or beyond the commit index is refused.
  #[test]
  fn compaction_bounds_the_log_while_indices_stay_correct() {
    // A lone leader commits every append at once, so five appends give a five-entry committed log.
    let mut leader = elected_leader(A, vec![A]);
    for value in 0..5u8 {
      leader.append_command(vec![value]);
    }
    assert_eq!(leader.commit_index(), 5);
    assert_eq!(leader.last_log_index(), 5);

    // Compact up to index 3: the prefix is discarded, but the last index and the committed remainder are
    // still correct.
    assert!(
      leader.compact(3, Vec::new()),
      "committed entries up to 3 compact"
    );
    assert_eq!(leader.snapshot_index(), 3);
    assert_eq!(
      leader.last_log_index(),
      5,
      "the last index is unchanged by compaction"
    );
    assert_eq!(
      leader.committed_entries().len(),
      2,
      "only the entries above the snapshot (indices 4 and 5) remain to apply"
    );

    // Appends continue past the snapshot boundary and still commit.
    leader.append_command(b"after-snapshot".to_vec());
    assert_eq!(leader.last_log_index(), 6);
    assert_eq!(
      leader.commit_index(),
      6,
      "the entry after the snapshot commits"
    );
  }

  /// Compaction is refused backward (at or below the current snapshot) and ahead of the commit index —
  /// only the committed, not-yet-snapshotted prefix may be discarded.
  #[test]
  fn compaction_refuses_backward_or_uncommitted() {
    let mut leader = elected_leader(A, vec![A]);
    for value in 0..3u8 {
      leader.append_command(vec![value]);
    }
    assert!(
      leader.compact(2, Vec::new()),
      "committed entries up to 2 compact"
    );
    assert!(
      !leader.compact(2, Vec::new()),
      "cannot compact at or below the current snapshot"
    );
    assert!(!leader.compact(1, Vec::new()), "nor backward");
    assert!(
      !leader.compact(100, Vec::new()),
      "nor beyond the commit index"
    );
  }

  /// After compaction the leader still replicates correctly to a follower: the append it builds anchors
  /// at the snapshot boundary (using the snapshot term), and a follower that already holds that prefix
  /// accepts the entries beyond it.
  #[test]
  fn replication_works_across_a_snapshot_boundary() {
    // A three-node leader with a committed three-entry log (recovered so the log exists), elected fresh.
    let mut leader = node_with_uncommitted_log(A, vec![A, B, C], 2, None, log_of(&[1, 1, 2]));
    leader.start_election(); // term 3
    leader.on_vote_reply(VoteReply {
      voter: B,
      term: leader.term(),
      granted: true,
    });
    assert!(leader.is_leader());
    leader.append_command(b"t3".to_vec()); // index 4, term 3

    // Commit index 4 by replicating to a follower that holds the term-1/term-2 prefix.
    let mut follower = node_with_uncommitted_log(B, vec![A, B, C], 3, None, log_of(&[1, 1, 2]));
    let append = leader.replicate_to(B).expect("append");
    let reply = follower.on_append_entries(append);
    assert!(reply.success);
    leader.on_append_reply(reply);
    assert_eq!(leader.commit_index(), 4);

    // Compact the leader up to index 2; its log now begins after the snapshot.
    assert!(leader.compact(2, Vec::new()));
    assert_eq!(leader.snapshot_index(), 2);

    // Append and replicate again: the follower (already caught up) accepts across the boundary.
    leader.append_command(b"t3-more".to_vec()); // index 5
    let append = leader.replicate_to(B).expect("append after compaction");
    assert!(
      append.prev_log_index >= leader.snapshot_index(),
      "the append anchors at or after the snapshot"
    );
    let reply = follower.on_append_entries(append);
    assert!(
      reply.success,
      "the caught-up follower accepts the post-compaction append"
    );
    assert_eq!(follower.last_log_index(), 5);
  }

  /// A follower fallen below the leader's snapshot is caught up by an install-snapshot (Raft §7):
  /// replication first backs its next index down to the snapshot boundary and then cannot proceed (the
  /// entries are compacted away), so the leader ships the snapshot; the follower installs it and then
  /// accepts the entries beyond it.
  #[test]
  fn a_follower_below_the_snapshot_is_caught_up_by_install_snapshot() {
    // A term-3 leader with a four-entry committed log, compacted up to index 3 with some snapshot state.
    let mut leader = node_with_uncommitted_log(A, vec![A, B, C], 2, None, log_of(&[1, 1, 2]));
    leader.start_election(); // term 3
    leader.on_vote_reply(VoteReply {
      voter: B,
      term: leader.term(),
      granted: true,
    });
    leader.append_command(b"t3".to_vec()); // index 4, term 3
    let mut follower_b = node_with_uncommitted_log(B, vec![A, B, C], 3, None, log_of(&[1, 1, 2]));
    let append = leader.replicate_to(B).expect("append");
    let reply = follower_b.on_append_entries(append);
    leader.on_append_reply(reply);
    assert_eq!(leader.commit_index(), 4);
    assert!(leader.compact(3, b"snapshot-state".to_vec()));

    // A fresh, empty follower C is far below the snapshot. Replication backs its next index down until an
    // append can no longer be built (the previous entry is compacted away).
    let mut follower_c = RaftNode::new(C, vec![A, B, C]);
    let needs_snapshot = replicate_until_stuck(&mut leader, &mut follower_c, C);
    assert!(
      needs_snapshot,
      "replication cannot reach a follower below the snapshot"
    );

    // The leader ships the snapshot; the follower installs it and reports back.
    let snapshot = leader.install_snapshot_for(C).expect("C needs a snapshot");
    assert_eq!(snapshot.last_included_index, 3);
    assert_eq!(snapshot.state, b"snapshot-state");
    let reply = follower_c.on_install_snapshot(snapshot);
    leader.on_install_snapshot_reply(reply);
    assert_eq!(
      follower_c.snapshot_index(),
      3,
      "the follower adopted the snapshot"
    );

    // Now a normal append carries the entries beyond the snapshot, and C is caught up.
    let append = leader.replicate_to(C).expect("append after the snapshot");
    let reply = follower_c.on_append_entries(append);
    assert!(reply.success, "C accepts the post-snapshot entries");
    assert_eq!(
      follower_c.last_log_index(),
      4,
      "C is caught up to the leader"
    );
  }

  /// The log-integrated membership change (Raft §6): a configuration entry takes effect the moment it is
  /// appended — the node is joint before the entry commits — and reverts when the entry is truncated,
  /// because the effective configuration is derived from the log, not stored.
  #[test]
  fn a_configuration_takes_effect_on_append_and_reverts_on_truncation() {
    let mut leader = elected_leader(A, vec![A, B, C]);
    assert!(!leader.in_joint_configuration());
    leader.begin_membership_change(vec![C, D, E]);
    assert!(
      leader.in_joint_configuration(),
      "the joint configuration takes effect on append, before it commits"
    );

    // A follower adopts the joint configuration when it receives the entry.
    let mut follower = RaftNode::new(B, vec![A, B, C]);
    let append = leader
      .replicate_to(B)
      .expect("append carrying the configuration entry");
    follower.on_append_entries(append);
    assert!(
      follower.in_joint_configuration(),
      "the follower adopts it on append"
    );

    // A conflicting entry at index 1 from a newer term truncates the configuration entry, reverting the
    // configuration to the base.
    let conflicting = AppendEntries {
      read_context: 0,
      term: follower.term() + 1,
      leader: A,
      prev_log_index: 0,
      prev_log_term: 0,
      entries: vec![LogEntry::command(follower.term() + 1, b"other".to_vec())],
      leader_commit: 0,
    };
    let reply = follower.on_append_entries(conflicting);
    assert!(reply.success);
    assert!(
      !follower.in_joint_configuration(),
      "truncating the configuration entry reverts the configuration"
    );
  }

  /// Compaction preserves the effective configuration: a configuration entry folded into the snapshot is
  /// carried into the base, so the node stays in the joint configuration after its log is compacted.
  #[test]
  fn compaction_preserves_the_configuration() {
    let mut leader = elected_leader(A, vec![A, B, C]);
    leader.begin_membership_change(vec![B, C, D]); // joint {A,B,C} ∪ {B,C,D} at index 1
    assert!(leader.in_joint_configuration());

    // Replicate the joint entry to B and C — a majority of both configurations — so it commits.
    for id in [B, C] {
      let mut follower = RaftNode::new(id, vec![A, B, C]);
      let append = leader.replicate_to(id).expect("append");
      let reply = follower.on_append_entries(append);
      leader.on_append_reply(reply);
    }
    assert_eq!(
      leader.commit_index(),
      1,
      "the joint configuration entry commits"
    );

    // Compact past it; the joint configuration survives in the base.
    assert!(leader.compact(1, b"state".to_vec()));
    assert!(
      leader.in_joint_configuration(),
      "the configuration folded into the snapshot is preserved"
    );
  }
}
