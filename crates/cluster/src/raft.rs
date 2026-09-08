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
//! Degenerate on a laptop (`f = 0`): one voter, itself; an election reaches a majority of one at once
//! and an appended entry commits at once — the same code path as a fleet, never a mode switch (R8). Owed
//! (the rest of the dialect, each its own slice): PreVote and CheckQuorum, joint consensus for
//! membership changes, ReadIndex for linearizable reads, snapshot/log compaction, and the bug-record
//! conformance suite.
//!
//! Evidence: Ongaro & Ousterhout, *In Search of an Understandable Consensus Algorithm (Extended
//! Version)*, 2014 (tier A); the safety argument for the election restriction is §5.4.

use std::collections::{BTreeMap, BTreeSet};

use slates_db::register::HostId;

/// A node's role in its term (Raft §5.1). A follower defers to a leader; a candidate is seeking votes; a
/// leader has a majority for its term.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
  /// Passive — grants votes and (once replication lands) accepts a leader's entries.
  Follower,
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

/// One entry in the replicated log: the term the entry was created in (Raft's per-entry term, the basis
/// of the log-matching property) and the opaque command it carries (for the configuration group, an
/// encoded configuration change — the Raft core does not interpret it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
  /// The term in which the leader created this entry.
  pub term: u64,
  /// The command to apply once the entry commits (opaque to the Raft core).
  pub command: Vec<u8>,
}

/// A leader's replication message (Raft `AppendEntries`): the leader's term, the log position it is
/// appending after (`prev_log_index`/`prev_log_term`, the consistency check), the entries to append
/// (empty for a heartbeat), and the leader's commit index. A follower appends only when its log matches
/// at the previous position, so the logs converge (the log-matching property, §5.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppendEntries {
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
  /// The follower replying.
  pub follower: HostId,
  /// The follower's current term.
  pub term: u64,
  /// Whether the append succeeded.
  pub success: bool,
  /// On success, the last index the follower's log now matches the leader on.
  pub match_index: u64,
}

/// A Raft node's state: its identity, the voters it counts a majority against, the persistent term and
/// vote (Raft's `currentTerm`/`votedFor`), its role, the votes gathered this election, the replicated
/// `log` and how far it is committed, and — while leader — the per-follower `next_index`/`match_index`
/// replication progress.
pub struct RaftNode {
  id: HostId,
  voters: Vec<HostId>,
  current_term: u64,
  voted_for: Option<HostId>,
  role: Role,
  votes: BTreeSet<HostId>,
  log: Vec<LogEntry>,
  commit_index: u64,
  next_index: BTreeMap<HostId, u64>,
  match_index: BTreeMap<HostId, u64>,
}

impl RaftNode {
  /// A fresh node: a follower at term zero with an empty log, among `voters` (which includes itself).
  pub fn new(id: HostId, voters: Vec<HostId>) -> RaftNode {
    RaftNode {
      id,
      voters,
      current_term: 0,
      voted_for: None,
      role: Role::Follower,
      votes: BTreeSet::new(),
      log: Vec::new(),
      commit_index: 0,
      next_index: BTreeMap::new(),
      match_index: BTreeMap::new(),
    }
  }

  /// A node recovered after a restart from its persisted term, vote and `log` (Raft persists
  /// `currentTerm`, `votedFor` and the log before responding). It comes back a follower — a restart
  /// never resumes as leader or candidate — with nothing yet known committed.
  pub fn recovered(
    id: HostId,
    voters: Vec<HostId>,
    current_term: u64,
    voted_for: Option<HostId>,
    log: Vec<LogEntry>,
  ) -> RaftNode {
    RaftNode {
      id,
      voters,
      current_term,
      voted_for,
      role: Role::Follower,
      votes: BTreeSet::new(),
      log,
      commit_index: 0,
      next_index: BTreeMap::new(),
      match_index: BTreeMap::new(),
    }
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

  /// Who this node voted for in its current term, if anyone.
  pub fn voted_for(&self) -> Option<HostId> {
    self.voted_for
  }

  /// The caller's election timer fired: begin an election (Raft §5.2). Advance to the next term, become
  /// a candidate, vote for self, and return the [`RequestVote`] to send each *other* voter. A single
  /// voter reaches its own majority here and becomes leader with no messages (the `f = 0` degenerate).
  pub fn start_election(&mut self) -> Vec<RequestVote> {
    self.current_term = self.current_term.saturating_add(1);
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
      .voters
      .iter()
      .copied()
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
    let granted = request.term == self.current_term
      && not_yet_voted_elsewhere
      && self.candidate_log_is_current(request.last_log_index, request.last_log_term);
    if granted {
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
    self.current_term = term;
    self.voted_for = None;
    self.role = Role::Follower;
    self.votes.clear();
  }

  /// Becomes leader if the votes gathered this election are a majority of the voters, initialising the
  /// replication progress for each follower — `next_index` at the end of the leader's log (Raft's
  /// optimistic guess) and `match_index` at nothing known replicated (§5.3).
  fn become_leader_if_majority(&mut self) {
    if self.role != Role::Candidate || self.votes.len() < self.majority() {
      return;
    }
    self.role = Role::Leader;
    let next = self.last_log_index().saturating_add(1);
    self.next_index.clear();
    self.match_index.clear();
    for peer in &self.voters {
      if *peer != self.id {
        self.next_index.insert(*peer, next);
        self.match_index.insert(*peer, 0);
      }
    }
  }

  /// A majority of the voters: more than half, so any two majorities intersect (the quorum intersection
  /// Raft's safety rests on).
  fn majority(&self) -> usize {
    self.voters.len() / 2 + 1
  }

  /// Whether a candidate's last-log summary is at least as up-to-date as ours (Raft §5.4.1): a later
  /// last term wins; at an equal last term the longer (or equal) log wins.
  fn candidate_log_is_current(&self, candidate_index: u64, candidate_term: u64) -> bool {
    candidate_term > self.last_log_term()
      || (candidate_term == self.last_log_term() && candidate_index >= self.last_log_index())
  }

  /// The index of the last log entry (zero for an empty log). Raft indexes entries from one.
  pub fn last_log_index(&self) -> u64 {
    u64::try_from(self.log.len()).unwrap_or(u64::MAX)
  }

  /// The term of the last log entry (zero for an empty log).
  pub fn last_log_term(&self) -> u64 {
    self.log.last().map_or(0, |entry| entry.term)
  }

  /// The term of the entry at the one-based `index`, or `None` if the log has no such entry (index zero,
  /// the position before the log, is the empty-log sentinel and has no term here — the consistency check
  /// treats it specially).
  fn entry_term(&self, index: u64) -> Option<u64> {
    let one_based = usize::try_from(index).unwrap_or(usize::MAX);
    let zero_based = one_based.checked_sub(1)?;
    self.log.get(zero_based).map(|entry| entry.term)
  }

  /// The highest index known committed (a majority holds it).
  pub fn commit_index(&self) -> u64 {
    self.commit_index
  }

  /// The committed log entries in order (the prefix the caller may apply).
  pub fn committed_entries(&self) -> &[LogEntry] {
    let committed = usize::try_from(self.commit_index).unwrap_or(usize::MAX);
    &self.log[..committed.min(self.log.len())]
  }

  /// Appends `command` to the leader's own log at the current term and updates its self-match, so a
  /// single-voter leader commits it at once (Raft §5.3, leader append). A non-leader ignores the append
  /// and reports `false` — only the leader proposes.
  pub fn append_command(&mut self, command: Vec<u8>) -> bool {
    if self.role != Role::Leader {
      return false;
    }
    self.log.push(LogEntry {
      term: self.current_term,
      command,
    });
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
    let prev_log_term = if prev_log_index == 0 {
      0
    } else {
      self.entry_term(prev_log_index).unwrap_or(0)
    };
    let from = usize::try_from(prev_log_index).unwrap_or(usize::MAX);
    let entries = self.log.get(from..).unwrap_or(&[]).to_vec();
    Some(AppendEntries {
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
      return self.append_reply(false, 0);
    }
    if request.term > self.current_term {
      self.step_down(request.term);
    }
    // A current-term append means a leader exists for our term — defer to it (a candidate steps down).
    self.role = Role::Follower;

    // Consistency check: our log must contain the previous entry with the leader's term.
    if request.prev_log_index > 0
      && self.entry_term(request.prev_log_index) != Some(request.prev_log_term)
    {
      return self.append_reply(false, 0);
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
        None => self.log.push(entry),
      }
    }

    // Advance the commit index to the leader's, but no further than the entries we now hold.
    if request.leader_commit > self.commit_index {
      self.commit_index = request.leader_commit.min(index);
    }
    self.append_reply(true, index)
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

  /// Truncates the log from the one-based `index` onward (removing that entry and every later one).
  fn truncate_from(&mut self, index: u64) {
    let keep = usize::try_from(index.saturating_sub(1)).unwrap_or(usize::MAX);
    self.log.truncate(keep);
  }

  /// A follower's reply with this node's current term.
  fn append_reply(&self, success: bool, match_index: u64) -> AppendReply {
    AppendReply {
      follower: self.id,
      term: self.current_term,
      success,
      match_index,
    }
  }

  /// Advances the leader's commit index (Raft §5.4.2): the highest index a majority of voters hold whose
  /// entry is from the **current term**. Earlier-term entries are not committed by replica count alone —
  /// they commit only once a current-term entry above them does — which is the safety subtlety Raft's
  /// figure 8 exposes.
  fn advance_leader_commit(&mut self) {
    if self.role != Role::Leader {
      return;
    }
    let mut candidate = self.last_log_index();
    while candidate > self.commit_index {
      if self.entry_term(candidate) == Some(self.current_term) {
        let holders = self
          .voters
          .iter()
          .filter(|voter| self.match_of(**voter) >= candidate)
          .count();
        if holders >= self.majority() {
          self.commit_index = candidate;
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
      .map(|(index, &term)| LogEntry {
        term,
        command: vec![u8::try_from(index).unwrap_or(u8::MAX)],
      })
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

  /// A single-voter group elects itself: an election reaches the majority of one at once, so the node is
  /// leader for term 1 with no messages to send (the `f = 0` degenerate).
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
    let mut node = RaftNode::recovered(A, vec![A, B, C], 5, None, Vec::new());
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
    let mut node = RaftNode::recovered(A, vec![A, B, C], 2, None, log_of(&[1, 1, 2]));

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
    let mut node = RaftNode::recovered(A, vec![A, B, C], 2, None, log_of(&[1, 1, 2]));
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
    let mut leader = RaftNode::recovered(A, vec![A, B, C], 3, None, log_of(&[3, 3]));
    leader.start_election(); // term 4
    leader.on_vote_reply(VoteReply {
      voter: B,
      term: leader.term(),
      granted: true,
    });
    assert!(leader.is_leader());
    leader.append_command(b"cfg-new".to_vec()); // index 3, term 4

    // A follower with a single conflicting entry (term 1) at index 1.
    let mut follower = RaftNode::recovered(B, vec![A, B, C], 1, None, log_of(&[1]));

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
    let mut leader = RaftNode::recovered(A, vec![A, B, C], 4, None, log_of(&[2]));
    leader.start_election(); // term 5
    leader.on_vote_reply(VoteReply {
      voter: B,
      term: leader.term(),
      granted: true,
    });
    assert!(leader.is_leader());

    // A majority replicates the old (term-2) entry — it must NOT be committed by count alone.
    let mut follower = RaftNode::recovered(B, vec![A, B, C], 5, None, Vec::new());
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
}
