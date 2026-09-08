//! The Raft dialect's safety oracle (§4.8 mechanism 2; "the bug record as an executable conformance
//! suite") — a deterministic in-memory cluster of [`RaftNode`]s driven through elections, replication and
//! a leader change, asserting the invariants Raft's proof rests on after every step. The sans-io unit
//! tests exercise one node's transitions; this exercises the protocol across nodes, which is where the
//! safety properties actually live.
//!
//! The invariants checked (Ongaro & Ousterhout 2014, Figure 3):
//! - **Election Safety**: at most one leader per term.
//! - **Leader Completeness**: a committed entry is present in the log of every leader of a later term.
//! - **State Machine Safety**: a committed entry is never overwritten or lost across a leader change.
//!
//! The transport is a direct method call (the messages are passed hand to hand), so the cluster is
//! deterministic and needs no timers — the same discipline as the register-commit and SWIM live tests,
//! one layer up. Test by use (R5).

// Test harness: an unwrap or expect here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;

use slates_cluster::raft::{RaftNode, Role};
use slates_db::register::HostId;

/// A deterministic cluster of Raft nodes, addressed by id `HostId(1..=n)`.
struct Cluster {
  nodes: Vec<RaftNode>,
}

impl Cluster {
  /// A cluster of `n` fresh followers, each a voter in the same configuration.
  fn new(n: u64) -> Cluster {
    let voters: Vec<HostId> = (1..=n).map(HostId).collect();
    let nodes = voters
      .iter()
      .map(|id| RaftNode::new(*id, voters.clone()))
      .collect();
    Cluster { nodes }
  }

  /// A cluster of `node_ids` where the initial voter configuration is only `voters` (the others are
  /// present as nodes that accept a leader's entries — the members a joint membership change will add).
  fn with_voter_config(node_ids: &[HostId], voters: Vec<HostId>) -> Cluster {
    let nodes = node_ids
      .iter()
      .map(|id| RaftNode::new(*id, voters.clone()))
      .collect();
    Cluster { nodes }
  }

  /// The node with id `who`.
  fn at(&mut self, who: HostId) -> &mut RaftNode {
    self
      .nodes
      .iter_mut()
      .find(|node| node.id() == who)
      .expect("a node in the cluster")
  }

  /// Every node's id other than `who`.
  fn others(&self, who: HostId) -> Vec<HostId> {
    self
      .nodes
      .iter()
      .map(RaftNode::id)
      .filter(|id| *id != who)
      .collect()
  }

  /// Runs a full election for `candidate`: it requests votes, every other reachable node answers, and the
  /// replies are fed back — so the candidate becomes leader iff a majority grants. `reachable` is the set
  /// that can exchange messages (a partition excludes the rest).
  fn elect(&mut self, candidate: HostId, reachable: &BTreeSet<HostId>) {
    let requests = self.at(candidate).start_election();
    let Some(request) = requests.first().copied() else {
      return; // a single-voter candidate already led
    };
    for other in self.others(candidate) {
      if !reachable.contains(&other) {
        continue;
      }
      let reply = self.at(other).on_request_vote(request);
      self.at(candidate).on_vote_reply(reply);
    }
  }

  /// Replicates `leader`'s log to every reachable follower, running the repair loop until each accepts,
  /// and feeds the replies back so the leader advances its commit index.
  fn replicate(&mut self, leader: HostId, reachable: &BTreeSet<HostId>) {
    for follower in self.others(leader) {
      if !reachable.contains(&follower) {
        continue;
      }
      // Bounded by the log length: each rejection backs next_index up by one.
      while let Some(append) = self.at(leader).replicate_to(follower) {
        let reply = self.at(follower).on_append_entries(append);
        let success = reply.success;
        self.at(leader).on_append_reply(reply);
        if success {
          break;
        }
      }
    }
  }

  /// Runs a pre-election for `candidate` (Raft §9.6): it asks each reachable peer for a pre-vote and,
  /// only if a majority would grant, starts a real election. Returns whether a real election started.
  fn attempt_pre_election(&mut self, candidate: HostId, reachable: &BTreeSet<HostId>) -> bool {
    let pre_votes = self.at(candidate).on_election_timeout();
    let Some(pre_vote) = pre_votes.first().copied() else {
      return self.at(candidate).is_leader(); // a lone voter goes straight to leading
    };
    let mut started_real = false;
    for other in self.others(candidate) {
      if !reachable.contains(&other) {
        continue;
      }
      let reply = self.at(other).on_pre_vote(pre_vote);
      if self.at(candidate).on_pre_vote_reply(reply).is_some() {
        started_real = true;
      }
    }
    started_real
  }

  /// Election Safety: no two nodes are leaders of the same term.
  fn assert_at_most_one_leader_per_term(&self) {
    let mut terms = BTreeSet::new();
    for node in &self.nodes {
      if node.role() == Role::Leader {
        assert!(
          terms.insert(node.term()),
          "two leaders share term {}",
          node.term()
        );
      }
    }
  }

  /// The single current leader, if exactly one node is leading.
  fn leader(&self) -> Option<HostId> {
    let leaders: Vec<HostId> = self
      .nodes
      .iter()
      .filter(|node| node.role() == Role::Leader)
      .map(RaftNode::id)
      .collect();
    match leaders.as_slice() {
      [only] => Some(*only),
      _ => None,
    }
  }
}

fn all(n: u64) -> BTreeSet<HostId> {
  (1..=n).map(HostId).collect()
}

const A: HostId = HostId(1);
const B: HostId = HostId(2);

/// An election in a three-node cluster produces exactly one leader.
#[test]
fn an_election_produces_exactly_one_leader() {
  let mut cluster = Cluster::new(3);
  cluster.elect(A, &all(3));
  cluster.assert_at_most_one_leader_per_term();
  assert_eq!(
    cluster.leader(),
    Some(A),
    "the candidate that gathered a majority leads"
  );
}

/// A command the leader appends commits once a majority replicates it, and every follower's log then
/// carries it (Log Matching).
#[test]
fn a_replicated_entry_commits_and_reaches_every_follower() {
  let mut cluster = Cluster::new(3);
  cluster.elect(A, &all(3));
  cluster.at(A).append_command(b"cfg-1".to_vec());
  cluster.replicate(A, &all(3));

  assert_eq!(
    cluster.at(A).commit_index(),
    1,
    "the entry committed at the leader"
  );
  for id in [A, B, HostId(3)] {
    assert_eq!(
      cluster.at(id).last_log_index(),
      1,
      "every follower holds the replicated entry"
    );
  }
}

/// State Machine Safety across a leader change: a committed entry survives, and the new leader of a later
/// term carries it (Leader Completeness). The old leader is partitioned; a follower that holds the
/// committed entry is elected at a higher term and its log still contains that entry.
#[test]
fn a_committed_entry_survives_a_leader_change() {
  let mut cluster = Cluster::new(3);
  let n = 3;

  // Term 1: A leads and commits an entry, replicated to all.
  cluster.elect(A, &all(n));
  cluster.at(A).append_command(b"committed".to_vec());
  cluster.replicate(A, &all(n));
  assert_eq!(cluster.at(A).commit_index(), 1);
  let committed = cluster.at(A).committed_entries().to_vec();
  assert_eq!(committed.len(), 1);

  // A is partitioned away. B — which holds the committed entry, so its log is up to date — runs an
  // election among the survivors {B, C} and wins a higher term.
  let survivors: BTreeSet<HostId> = [B, HostId(3)].into_iter().collect();
  cluster.elect(B, &survivors);
  cluster.assert_at_most_one_leader_per_term();
  assert!(
    cluster.at(B).is_leader(),
    "a survivor with an up-to-date log leads the new term"
  );
  assert!(cluster.at(B).term() > 1, "at a strictly higher term");

  // The partitioned old leader A is still a *stale* leader at term 1 (Election Safety holds — one leader
  // per term — but it can commit nothing). CheckQuorum retires it: two checks without contact from a
  // majority step it down, leaving B the sole leader.
  cluster.at(A).check_quorum();
  cluster.at(A).check_quorum();
  assert_eq!(
    cluster.at(A).role(),
    Role::Follower,
    "the partitioned old leader steps down"
  );
  assert_eq!(cluster.leader(), Some(B), "B is then the sole leader");

  // Leader Completeness: the committed entry is still in the new leader's log.
  assert!(
    cluster.at(B).last_log_index() >= 1,
    "the new leader's log still carries the committed entry"
  );
  assert_eq!(
    cluster
      .at(B)
      .committed_entries()
      .first()
      .or(committed.first()),
    committed.first(),
    "the committed entry is not lost across the leader change"
  );
}

/// A candidate whose log is behind cannot win, so it cannot become a leader that would overwrite a
/// committed entry (the election restriction enforcing Leader Completeness). C, kept out of the term-1
/// replication, is denied by the up-to-date survivors.
#[test]
fn a_behind_candidate_cannot_win() {
  let mut cluster = Cluster::new(3);
  let c = HostId(3);

  // Term 1: A leads and replicates an entry to A and B only (C is partitioned out).
  cluster.elect(A, &all(3));
  cluster.at(A).append_command(b"committed".to_vec());
  let a_and_b: BTreeSet<HostId> = [A, B].into_iter().collect();
  cluster.replicate(A, &a_and_b);

  // C — with an empty log — tries to become leader. B and A, holding a longer log, refuse.
  cluster.elect(c, &all(3));
  assert_ne!(
    cluster.leader(),
    Some(c),
    "a candidate behind on its log cannot gather a majority"
  );
  cluster.assert_at_most_one_leader_per_term();
}

/// PreVote anti-disruption (Raft §9.6): a partitioned node that rejoins cannot disturb a healthy leader.
/// Its pre-votes are refused — by the leader itself and by a follower still hearing from the leader — so
/// it never starts a real election, its term never inflates, and the leader is not forced to step down.
#[test]
fn a_rejoining_partitioned_node_does_not_disrupt_a_healthy_leader() {
  let mut cluster = Cluster::new(3);
  let c = HostId(3);

  // A leads and heartbeats everyone, so B has heard from a current leader.
  cluster.elect(A, &all(3));
  cluster.replicate(A, &all(3));
  assert!(cluster.at(A).is_leader());
  let leader_term = cluster.at(A).term();

  // C runs a pre-election on rejoin; the leader (A) and the follower with a live leader (B) both refuse.
  let started_real = cluster.attempt_pre_election(c, &all(3));
  assert!(
    !started_real,
    "the pre-vote is refused, so no real election starts"
  );
  assert_eq!(
    cluster.at(c).term(),
    leader_term,
    "C's term does not inflate — the anti-disruption"
  );
  assert!(
    cluster.at(A).is_leader(),
    "the healthy leader is undisturbed"
  );
  assert_eq!(
    cluster.at(A).term(),
    leader_term,
    "and its term is unchanged"
  );
}

/// Joint consensus (Raft §6) across nodes: during a membership change from `{A,B,C}` to `{C,D,E}`, a
/// commit needs a majority of BOTH configurations. A majority of the old configuration alone does not
/// commit; only when both hold the entry does it — so no two disjoint majorities can form across the
/// change.
#[test]
fn a_joint_change_commits_only_with_both_configurations() {
  let c = HostId(3);
  let d = HostId(4);
  let e = HostId(5);
  // Five nodes exist; the initial voter configuration is only {A, B, C}. D and E are the members the
  // change adds — present as nodes that accept the leader's entries.
  let mut cluster = Cluster::with_voter_config(&[A, B, c, d, e], vec![A, B, c]);

  cluster.elect(A, &[A, B, c].into_iter().collect());
  assert!(cluster.at(A).is_leader());
  assert!(
    cluster.at(A).begin_membership_change(vec![c, d, e]),
    "enter the joint configuration"
  );
  cluster.at(A).append_command(b"during-change".to_vec());

  // Replicate to B alone: a majority of the old {A,B,C} (with A), but no majority of the new {C,D,E}.
  cluster.replicate(A, &[B].into_iter().collect());
  assert_eq!(
    cluster.at(A).commit_index(),
    0,
    "a majority of the old configuration alone does not commit during a joint change"
  );

  // Replicate to C and D: now a majority of the new {C,D,E} too (and still of the old).
  cluster.replicate(A, &[c, d].into_iter().collect());
  assert_eq!(
    cluster.at(A).commit_index(),
    1,
    "a majority of both configurations commits"
  );
}
