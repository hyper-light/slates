//! The hecate Raft dialect, pure core (§4.8 mechanism 2 "Configuration, by consensus") — the regional
//! configuration group agrees membership, neighbourhoods, host epochs and takeover assignments by Raft,
//! not by the data-plane fenced register (that is mechanism 1). This slice is the **sans-io role and
//! term state machine and leader election** (Raft §5.2 leader election, §5.4.1 the election
//! restriction): a deterministic state machine driven by an externally-timed `start_election` and by
//! received vote messages, so it is oracle-tested at N=1 before any timer or datagram is involved. The
//! caller owns the election timer and ships the [`RequestVote`]/[`VoteReply`] it returns.
//!
//! Degenerate on a laptop (`f = 0`): one voter, itself; an election reaches a majority of one at once,
//! so the node is its own leader with no messages — the same code path as a fleet, never a mode switch
//! (R8). Owed (the rest of the dialect, each its own slice): log replication (`AppendEntries`), PreVote
//! and CheckQuorum, joint consensus for membership changes, ReadIndex for linearizable reads, and the
//! bug-record conformance suite. This core deliberately holds only the election-relevant log summary
//! (the last entry's index and term, for the §5.4.1 up-to-dateness check); the log itself arrives with
//! replication.
//!
//! Evidence: Ongaro & Ousterhout, *In Search of an Understandable Consensus Algorithm (Extended
//! Version)*, 2014 (tier A); the safety argument for the election restriction is §5.4.

use std::collections::BTreeSet;

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

/// A Raft node's election state: its identity, the voters it counts a majority against, the persistent
/// term and vote (Raft's `currentTerm`/`votedFor`), its role, the votes gathered this election, and the
/// last-log summary the election restriction compares. Log replication (and the log itself) is owed.
pub struct RaftNode {
  id: HostId,
  voters: Vec<HostId>,
  current_term: u64,
  voted_for: Option<HostId>,
  role: Role,
  votes: BTreeSet<HostId>,
  last_log_index: u64,
  last_log_term: u64,
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
      last_log_index: 0,
      last_log_term: 0,
    }
  }

  /// A node recovered after a restart from its persisted term, vote and last-log summary (Raft persists
  /// `currentTerm`, `votedFor` and the log before responding). It comes back a follower — a restart
  /// never resumes as leader or candidate.
  pub fn recovered(
    id: HostId,
    voters: Vec<HostId>,
    current_term: u64,
    voted_for: Option<HostId>,
    last_log_index: u64,
    last_log_term: u64,
  ) -> RaftNode {
    RaftNode {
      id,
      voters,
      current_term,
      voted_for,
      role: Role::Follower,
      votes: BTreeSet::new(),
      last_log_index,
      last_log_term,
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
      last_log_index: self.last_log_index,
      last_log_term: self.last_log_term,
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

  /// Becomes leader if the votes gathered this election are a majority of the voters.
  fn become_leader_if_majority(&mut self) {
    if self.votes.len() >= self.majority() {
      self.role = Role::Leader;
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
    candidate_term > self.last_log_term
      || (candidate_term == self.last_log_term && candidate_index >= self.last_log_index)
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
    let mut node = RaftNode::recovered(A, vec![A, B, C], 5, None, 0, 0);
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
    let mut node = RaftNode::recovered(A, vec![A, B, C], 2, None, 3, 2);

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
    let mut node = RaftNode::recovered(A, vec![A, B, C], 2, None, 3, 2);
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
}
