//! The log model that parallel replication and the fast track share (`docs/wip/research/
//! consensus-enhancements.md` §3.5, §3.7 and §4), checked by an exhaustive search over every interleaving of
//! its actions at small scope, before any of it is built into the dialect.
//!
//! **What is modelled.** Each index above the committed prefix is a single-decree instance. Under parallel
//! replication a follower accepts the leader's entry at an index before the entries below it arrive; under
//! the fast track it accepts a proposer's entry straight from the proposer. Either way a new leader cannot
//! trust its own log above the committed prefix the way a Raft leader can: it must recover every such index
//! from a majority before it proposes there. The actions are atomic, as in a TLA+ specification: a term
//! bump (a timeout, or a term learnt from a leader's message), an election won with a chosen majority (and
//! the recovery its rule runs), a proposal landing at a node, a leader's decision from the votes of a chosen
//! majority, one replication, a commit. A lost or late message is an action not taken, or taken later; a
//! crashed node is one that takes no more actions.
//!
//! **The two recoveries.** *Published* is Fast Raft as its authors give it (Castiglia, Goldberg and
//! Patterson, arXiv:2004.06215 §IV): an entry is self-approved (inserted from a proposer into an empty
//! slot) or leader-approved (sent by a leader, which overwrites); the election compares only the last
//! leader-approved entry; the leader decides an index by the entries its followers hold there (every
//! follower answers a proposal with the entry it holds, and the self-approved entries a new leader's voters
//! send it are the same votes). Its decision loop can be read three ways ([`Reading`]); all three are
//! searched. *Ballots* is the rule §4 of the research record states: every slot records the ballot it was
//! accepted at — its term, and within a term a leader's decision outranks a fast vote — and per index the
//! highest ballot among the majority's reports decides. A leader's decision is re-proposed as it is. At a
//! fast ballot, the value a fast quorum could have chosen is re-proposed: the one with at least
//! `|Q| + |F| − n` of the reports (Lamport, *Fast Paxos*, 2006). Otherwise the index is free, and only a free
//! index is opened to the fast track.
//!
//! **The checks.** Every commit must commit the value committed before at its index, if any (agreement:
//! Fast Raft's Definition 2.1). No leader of the term a value was chosen in, or of a later term, may send
//! another value at its index (Paxos's P2c, which Fast Raft's Lemma 2 claims: "a follower never overwrites a
//! chosen entry"). The search also faults on two leaders in one term, and on two different decisions at one
//! classic ballot. The published rule fails under every reading, and the search prints the shortest
//! counterexample of each. The ballot rule must pass at every scope. Every path it has — fast commits,
//! classic commits, both kinds of constrained recovery, a leader's decision from its fast votes, a stale
//! value overwritten, a commit above an uncommitted index — must be taken at least once, so that a
//! silently dead path cannot pass for a proof.
//!
//! **Why one index proves every index count, for the ballot rule.** Every action touches one index, or the
//! terms and votes all indices share; a replication to a node that raises its term does to the shared state
//! exactly what the node learning that term from its leader does. So a history over several indices
//! projects, index by index, onto a valid history over one: drop the other indices' steps, turning a
//! replication there that raised a term into the [`Action::Learn`] it implies. A fault at any index would
//! appear in the one-index search. The two-index scope only shows that a commit above an uncommitted index
//! is reachable. (The published rule does not project, since its leader decides only at `commitIndex + 1`.)
//!
//! **The search.** A state is packed into four words. Nodes are interchangeable and so are values, so each
//! state is stored as the representative of its class: nodes ordered by a signature that renaming leaves
//! unchanged, ties broken by trying every order within them and keeping the least packed state, values
//! renumbered by first appearance. The published rule is searched breadth first on one core with whole keys
//! and parents, which yields the shortest counterexample. The ballot rule, expected to pass, is searched
//! breadth first level by level across every core over 128-bit fingerprints of the representatives: two
//! states share a fingerprint with probability 2⁻¹²⁸, so some pair among `n` does with probability below
//! `n²/2¹²⁹` (under 10⁻²¹ for a billion states), and only then could a state be skipped. Traversal order does
//! not change the work: a breadth-first and a depth-first search of the same scope both visited the same
//! 463,715 classes, in 1.13 s and 1.16 s, holding 94 MB and 38 MB (2026-09-28); the cost is the classes
//! times the work per step, dominated by finding each successor's representative, so the lever is cores.
//! The first search, which kept every leader's vote tally and every labelling, held 18 GB after 300 s
//! without finishing (2026-09-28); every search now has a budget derived from the measured cost of a state
//! and fails rather than exhausting the machine.

// Test harness: an unwrap, expect or panic here is a failed test.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

mod support;

use std::fmt;
use std::hash::{Hash, Hasher};

use support::exhaustive::{self, MEMORY_CEILING_BYTES, Model, Packer, Step, least_over_ties};

/// Shape: the most nodes a scope here models.
const MAX_NODES: usize = 5;
/// Shape: the most log indices a scope here models.
const MAX_INDICES: usize = 2;
/// Shape: the highest term a scope here reaches; a term packs into three bits.
const MAX_TERMS: u8 = 4;
/// Shape: the most distinct proposed values a scope here models; a value packs into two bits beside "none".
const MAX_VALUES: u8 = 3;
/// Format: the 64-bit words a packed state takes — at most 37 bits a node and 20 more for the whole state.
const WORDS: usize = 4;

/// How Fast Raft's decision loop is read. Its §IV-B loop — "while there exists a k = commitIndex + 1 for
/// which at least a classic quorum of votes has been received: insert entry e from possibleEntries[k] with
/// highest number of votes" — decides again as votes arrive; its §IV-C prose says leader-approved entries
/// "are treated the same as they are treated in classic Raft". Each reading is searched, so the failure is
/// no artifact of one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reading {
  /// As written: the loop decides whenever a classic quorum of votes is in.
  Literal,
  /// A leader decides an index at most once in its term.
  OncePerTerm,
  /// Once per term, and never by votes where the leader already holds a leader-approved entry, which it
  /// replicates as classic Raft would.
  KeepLeaderApproved,
}

/// Which recovery a newly elected leader runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rule {
  /// Fast Raft as published, under one reading of its decision loop.
  Published(Reading),
  /// The ballot rule of the research record's §4.
  Ballots,
}

/// The bounds of one search.
#[derive(Clone, Copy, Debug)]
struct Scope {
  nodes: usize,
  indices: usize,
  values: u8,
  terms: u8,
  rule: Rule,
}

impl Scope {
  fn majority(self) -> usize {
    self.nodes / 2 + 1
  }

  /// The fast quorum: the smallest `f` with `2f + q > 2n` for the classic quorum `q` — the size at which a
  /// value a fast quorum accepted has the most votes in every classic quorum (Fast Paxos's requirement, which
  /// Fast Raft states as ⌈3n/4⌉; [`Scope::checked`] asserts the two agree).
  fn fast_quorum(self) -> usize {
    let majority = self.majority();
    (1..=self.nodes)
      .find(|fast| 2 * fast + majority > 2 * self.nodes)
      .unwrap()
  }

  /// This scope, after checking it fits the packed state and that the derived fast quorum is Fast Raft's.
  fn checked(self) -> Scope {
    assert!(self.nodes <= MAX_NODES && self.indices <= MAX_INDICES);
    assert!(self.values <= MAX_VALUES && self.terms <= MAX_TERMS);
    assert_eq!(self.fast_quorum(), (3 * self.nodes).div_ceil(4));
    self
  }

  fn node_ids(self) -> std::ops::Range<usize> {
    0..self.nodes
  }

  fn published(self) -> bool {
    matches!(self.rule, Rule::Published(_))
  }
}

/// A node's slot at one index: the value, the term it was accepted at, and whether a leader decided it (a
/// leader-approved entry, or under the ballot rule a classic ballot) rather than it arriving straight from a
/// proposer (a self-approved entry, or a fast vote).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Slot {
  term: u8,
  classic: bool,
  value: u8,
}

impl Slot {
  /// The slot's ballot under the ballot rule: its term, and within a term a decision outranks a fast vote.
  fn ballot(self) -> (u8, bool) {
    (self.term, self.classic)
  }
}

/// What a leader may still do at an index in its term.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Phase {
  /// Nothing decided there yet (under the ballot rule, recovery found nothing that could have been chosen).
  Free,
  /// The fast track is open there (the ballot rule).
  Opened,
  /// The leader decided there.
  Decided,
}

/// A leader's volatile state for its term. It keeps no tally of votes: a decision reads the votes of the
/// majority it is taken from at once (see [`Action::Decide`]), which is the tally a leader holds when every
/// vote it counted is still the voter's entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Leading {
  /// Per index, the nodes that acknowledged the leader's current entry there.
  acks: [u8; MAX_INDICES],
  /// Per index, what the leader may still do there.
  phase: [Phase; MAX_INDICES],
  /// Per index, whether this leader committed there.
  committed: [bool; MAX_INDICES],
}

impl Leading {
  fn new() -> Leading {
    Leading {
      acks: [0; MAX_INDICES],
      phase: [Phase::Free; MAX_INDICES],
      committed: [false; MAX_INDICES],
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Node {
  term: u8,
  voted_for: Option<u8>,
  slots: [Option<Slot>; MAX_INDICES],
  leading: Option<Leading>,
}

/// A value committed at an index, and the term it was committed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Chosen {
  value: u8,
  term: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct State {
  nodes: [Node; MAX_NODES],
  /// Under the ballot rule, whether the leader of each term opened the fast track at each index.
  opened: [[bool; MAX_INDICES]; MAX_TERMS as usize + 1],
  /// What was committed at each index first, the history the checks read.
  chosen: [Option<Chosen>; MAX_INDICES],
}

impl State {
  fn initial() -> State {
    let node = Node {
      term: 0,
      voted_for: None,
      slots: [None; MAX_INDICES],
      leading: None,
    };
    State {
      nodes: [node; MAX_NODES],
      opened: [[false; MAX_INDICES]; MAX_TERMS as usize + 1],
      chosen: [None; MAX_INDICES],
    }
  }
}

/// One atomic step of the model.
#[derive(Clone, Copy, Debug)]
enum Action {
  /// A node times out: it enters the next term and votes for itself.
  Timeout { node: usize },
  /// A node learns the term of a leader from its append or heartbeat, without voting. (A candidate's vote
  /// request needs no step of its own: a node it reaches either votes, which [`Action::Elect`] covers, or
  /// takes no action that matters, which not stepping covers.)
  Learn { node: usize, term: u8 },
  /// A candidate wins its term with the votes of `quorum` (a bit per node), and runs its rule's recovery.
  Elect { node: usize, quorum: u8 },
  /// Published: a proposal lands in a node's empty slot, self-approved.
  Insert {
    node: usize,
    index: usize,
    value: u8,
  },
  /// Ballots: a node accepts a proposal at an index its term's leader opened, as a fast vote.
  Accept {
    node: usize,
    index: usize,
    value: u8,
  },
  /// Ballots: a leader opens a free index to the fast track.
  Open { leader: usize, index: usize },
  /// Ballots: a leader proposes its own value at a free index (parallel replication's new entry).
  Propose {
    leader: usize,
    index: usize,
    value: u8,
  },
  /// A leader decides an index from the votes of `voters` (a bit per node): each voter's entry there.
  Decide {
    leader: usize,
    index: usize,
    value: u8,
    voters: u8,
  },
  /// A leader's decided entry reaches one node, which acknowledges it.
  Replicate {
    leader: usize,
    node: usize,
    index: usize,
  },
  /// A leader commits its entry at an index once a majority acknowledged it.
  Commit { leader: usize, index: usize },
}

/// Format: the letters nodes print as.
const NAMES: [char; MAX_NODES] = ['A', 'B', 'C', 'D', 'E'];

fn names(mask: u8) -> String {
  (0..MAX_NODES)
    .filter(|node| mask & bit(*node) != 0)
    .map(|node| NAMES[node])
    .collect()
}

impl fmt::Display for Action {
  fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
    match *self {
      Action::Timeout { node } => write!(out, "{} times out", NAMES[node]),
      Action::Learn { node, term } => write!(out, "{} learns term {term}", NAMES[node]),
      Action::Elect { node, quorum } => {
        write!(out, "{} is elected by {}", NAMES[node], names(quorum))
      }
      Action::Insert { node, index, value } => {
        write!(out, "{} inserts v{value} at {index}", NAMES[node])
      }
      Action::Accept { node, index, value } => {
        write!(out, "{} fast-accepts v{value} at {index}", NAMES[node])
      }
      Action::Open { leader, index } => write!(out, "{} opens {index}", NAMES[leader]),
      Action::Propose {
        leader,
        index,
        value,
      } => write!(out, "{} proposes v{value} at {index}", NAMES[leader]),
      Action::Decide {
        leader,
        index,
        value,
        voters,
      } => write!(
        out,
        "{} decides v{value} at {index} from {}",
        NAMES[leader],
        names(voters)
      ),
      Action::Replicate {
        leader,
        node,
        index,
      } => write!(
        out,
        "{} replicates {index} to {}",
        NAMES[leader], NAMES[node]
      ),
      Action::Commit { leader, index } => write!(out, "{} commits {index}", NAMES[leader]),
    }
  }
}

/// Why a search stopped, or what it recorded on the way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
  /// Two values committed at one index.
  Disagreement { index: usize, first: u8, second: u8 },
  /// A leader of the term a value was committed in, or of a later term, sent another value at its index.
  OverwroteChosen {
    index: usize,
    chosen: u8,
    sent: u8,
    term: u8,
  },
  /// Two leaders in one term.
  TwoLeaders { term: u8 },
  /// Two different decisions at one classic ballot.
  TwoDecisions { index: usize, term: u8 },
}

/// The paths a step took, as bits of a mask (the non-vacuity counters' keys).
const FAST_COMMIT: u64 = 1;
const CLASSIC_COMMIT: u64 = 2;
const RECOVERED_DECISION: u64 = 4;
const RECOVERED_FAST_CHOICE: u64 = 8;
const DECIDED_FROM_VOTES: u64 = 16;
const OVERWROTE_STALE: u64 = 32;
const OUT_OF_ORDER_COMMIT: u64 = 64;
/// Format: each path's name, in bit order.
const PATHS: [&str; 7] = [
  "fast commits",
  "classic commits",
  "recoveries a decision constrained",
  "recoveries a possible fast choice constrained",
  "decisions from fast votes",
  "stale values overwritten",
  "commits above an uncommitted index",
];
/// Format: the position of [`OUT_OF_ORDER_COMMIT`] in [`PATHS`], a path only a scope of two indices has.
const OUT_OF_ORDER_PATH: usize = 6;

/// The result of one step.
type Next = Step<State, Fault>;

fn bit(node: usize) -> u8 {
  1 << node
}

fn id(node: usize) -> u8 {
  u8::try_from(node).unwrap()
}

fn members(mask: u8, nodes: usize) -> impl Iterator<Item = usize> {
  (0..nodes).filter(move |node| mask & bit(*node) != 0)
}

fn size(mask: u8) -> usize {
  usize::try_from(mask.count_ones()).unwrap()
}

/// Every set of at least a majority of the nodes, as masks.
fn majorities(scope: Scope) -> impl Iterator<Item = u8> {
  let everyone = u8::try_from((1_usize << scope.nodes) - 1).unwrap();
  (1..=everyone).filter(move |mask| size(*mask) >= scope.majority())
}

/// The node leading `term`, if one does.
fn leader_of(scope: Scope, state: &State, term: u8) -> Option<usize> {
  scope
    .node_ids()
    .find(|node| state.nodes[*node].term == term && state.nodes[*node].leading.is_some())
}

/// Every action worth trying from `state` (each is checked for being enabled when applied).
fn candidates(scope: Scope, state: &State, out: &mut Vec<Action>) {
  for node in scope.node_ids() {
    out.push(Action::Timeout { node });
    out.extend((1..=scope.terms).map(|term| Action::Learn { node, term }));
    out.extend(
      majorities(scope)
        .filter(|quorum| quorum & bit(node) != 0)
        .map(|quorum| Action::Elect { node, quorum }),
    );
    for index in 0..scope.indices {
      out.extend((0..scope.values).map(|value| match scope.rule {
        Rule::Published(_) => Action::Insert { node, index, value },
        Rule::Ballots => Action::Accept { node, index, value },
      }));
    }
    if state.nodes[node].leading.is_some() {
      leader_candidates(scope, node, out);
    }
  }
}

fn leader_candidates(scope: Scope, leader: usize, out: &mut Vec<Action>) {
  for index in 0..scope.indices {
    out.push(Action::Commit { leader, index });
    if scope.rule == Rule::Ballots {
      out.push(Action::Open { leader, index });
      out.extend((0..scope.values).map(|value| Action::Propose {
        leader,
        index,
        value,
      }));
    }
    for value in 0..scope.values {
      out.extend(majorities(scope).map(|voters| Action::Decide {
        leader,
        index,
        value,
        voters,
      }));
    }
    out.extend(
      scope
        .node_ids()
        .filter(|node| *node != leader)
        .map(|node| Action::Replicate {
          leader,
          node,
          index,
        }),
    );
  }
}

/// `action` applied to `state`, or `None` when it is not enabled there or changes nothing.
fn apply(scope: Scope, state: &State, action: Action) -> Option<Next> {
  let next = match action {
    Action::Timeout { node } => timeout(scope, state, node),
    Action::Learn { node, term } => learn(scope, state, node, term),
    Action::Elect { node, quorum } => elect(scope, state, node, quorum),
    Action::Insert { node, index, value } => insert(state, node, index, value),
    Action::Accept { node, index, value } => accept(state, node, index, value),
    Action::Open { leader, index } => open(state, leader, index),
    Action::Propose {
      leader,
      index,
      value,
    } => propose(state, leader, index, value),
    Action::Decide {
      leader,
      index,
      value,
      voters,
    } => decide(scope, state, leader, index, value, voters),
    Action::Replicate {
      leader,
      node,
      index,
    } => replicate(scope, state, leader, node, index),
    Action::Commit { leader, index } => commit(scope, state, leader, index),
  }?;
  (next.state != *state || next.fault.is_some()).then_some(next)
}

fn timeout(scope: Scope, state: &State, node: usize) -> Option<Next> {
  if state.nodes[node].term >= scope.terms {
    return None;
  }
  let mut next = *state;
  let candidate = &mut next.nodes[node];
  candidate.term += 1;
  candidate.voted_for = Some(id(node));
  candidate.leading = None;
  Some(Step::plain(next))
}

fn learn(scope: Scope, state: &State, node: usize, term: u8) -> Option<Next> {
  if term <= state.nodes[node].term || leader_of(scope, state, term).is_none() {
    return None;
  }
  let mut next = *state;
  let learner = &mut next.nodes[node];
  learner.term = term;
  learner.voted_for = None;
  learner.leading = None;
  Some(Step::plain(next))
}

/// The last leader-approved entry of `node`, as Fast Raft's election compares it: its index counted from one
/// (zero when there is none) and its term.
fn last_leader_approved(scope: Scope, node: &Node) -> (usize, u8) {
  (0..scope.indices)
    .rev()
    .find_map(|index| {
      node.slots[index]
        .filter(|slot| slot.classic)
        .map(|slot| (index + 1, slot.term))
    })
    .unwrap_or((0, 0))
}

/// Whether `voter` grants `candidate` its vote for the candidate's term. The ballot rule's recovery needs no
/// log comparison for safety, so it is searched with none — every majority may elect any candidate, which
/// covers any comparison an implementation adds.
fn grants(scope: Scope, state: &State, voter: usize, candidate: usize) -> bool {
  let (elector, running) = (state.nodes[voter], state.nodes[candidate]);
  let term_allows = elector.term < running.term
    || (elector.term == running.term
      && elector
        .voted_for
        .is_none_or(|choice| usize::from(choice) == candidate));
  if !term_allows || !scope.published() {
    return term_allows;
  }
  // Fast Raft §IV-C: "candLastLogIndex ≥ lastLeaderIndex and candLastLogTerm ≥ log[lastLeaderIndex].term, or
  // candLastLogTerm > lastLeaderIndex.term".
  let (candidate_index, candidate_term) = last_leader_approved(scope, &running);
  let (voter_index, voter_term) = last_leader_approved(scope, &elector);
  (candidate_index >= voter_index && candidate_term >= voter_term) || candidate_term > voter_term
}

fn elect(scope: Scope, state: &State, node: usize, quorum: u8) -> Option<Next> {
  let running = state.nodes[node];
  if quorum & bit(node) == 0 || running.leading.is_some() || running.voted_for != Some(id(node)) {
    return None;
  }
  if !members(quorum, scope.nodes)
    .filter(|voter| *voter != node)
    .all(|voter| grants(scope, state, voter, node))
  {
    return None;
  }
  let mut next = Step::plain(*state);
  for voter in members(quorum, scope.nodes) {
    let elector = &mut next.state.nodes[voter];
    elector.term = running.term;
    elector.voted_for = Some(id(node));
    elector.leading = None;
  }
  if leader_of(scope, state, running.term).is_some() {
    next.fault = Some(Fault::TwoLeaders { term: running.term });
    return Some(next);
  }
  next.state.nodes[node].leading = Some(Leading::new());
  if scope.rule == Rule::Ballots {
    ballot_recovery(scope, state, node, quorum, &mut next);
  }
  Some(next)
}

/// The value the ballot rule constrains a new leader to at `index`, given the reports of `quorum`, with the
/// path that constrained it — or `Ok(None)` when nothing could have been chosen there.
fn constrained(
  scope: Scope,
  state: &State,
  quorum: u8,
  index: usize,
) -> Result<Option<(u8, u64)>, Fault> {
  let reports: Vec<Slot> = members(quorum, scope.nodes)
    .filter_map(|voter| state.nodes[voter].slots[index])
    .collect();
  let Some(highest) = reports.iter().map(|slot| slot.ballot()).max() else {
    return Ok(None);
  };
  let at_highest: Vec<u8> = reports
    .iter()
    .filter(|slot| slot.ballot() == highest)
    .map(|slot| slot.value)
    .collect();
  let (term, classic) = highest;
  if classic {
    let value = at_highest[0];
    if at_highest.iter().any(|other| *other != value) {
      return Err(Fault::TwoDecisions { index, term });
    }
    return Ok(Some((value, RECOVERED_DECISION)));
  }
  let threshold = size(quorum) + scope.fast_quorum() - scope.nodes;
  Ok(
    (0..scope.values)
      .find(|value| at_highest.iter().filter(|vote| **vote == *value).count() >= threshold)
      .map(|value| (value, RECOVERED_FAST_CHOICE)),
  )
}

/// The ballot rule's recovery: every index the reports constrain is re-proposed at the new term's classic
/// ballot; every other index is free.
fn ballot_recovery(scope: Scope, state: &State, node: usize, quorum: u8, next: &mut Next) {
  let term = state.nodes[node].term;
  for index in 0..scope.indices {
    match constrained(scope, state, quorum, index) {
      Err(fault) => next.fault = Some(fault),
      Ok(None) => {}
      Ok(Some((value, path))) => {
        guard(next, index, value, term);
        let leader = &mut next.state.nodes[node];
        leader.slots[index] = Some(Slot {
          term,
          classic: true,
          value,
        });
        if let Some(leading) = leader.leading.as_mut() {
          leading.phase[index] = Phase::Decided;
          leading.acks[index] = bit(node);
        }
        next.paths |= path;
      }
    }
  }
}

/// Faults a leader of `term` sending `value` at `index` when another value was committed there in `term` or
/// before — the step Paxos's P2c forbids.
fn guard(next: &mut Next, index: usize, value: u8, term: u8) {
  if let Some(chosen) = next.state.chosen[index]
    && term >= chosen.term
    && value != chosen.value
    && next.fault.is_none()
  {
    next.fault = Some(Fault::OverwroteChosen {
      index,
      chosen: chosen.value,
      sent: value,
      term,
    });
  }
}

fn insert(state: &State, node: usize, index: usize, value: u8) -> Option<Next> {
  if state.nodes[node].slots[index].is_some() {
    return None;
  }
  let mut next = *state;
  next.nodes[node].slots[index] = Some(Slot {
    term: state.nodes[node].term,
    classic: false,
    value,
  });
  Some(Step::plain(next))
}

fn accept(state: &State, node: usize, index: usize, value: u8) -> Option<Next> {
  let acceptor = state.nodes[node];
  let term = acceptor.term;
  if !state.opened[usize::from(term)][index] {
    return None;
  }
  let held = acceptor.slots[index];
  if held.is_some_and(|slot| slot.ballot() >= (term, false)) {
    return None;
  }
  let mut next = Step::plain(*state);
  next.state.nodes[node].slots[index] = Some(Slot {
    term,
    classic: false,
    value,
  });
  if held.is_some_and(|slot| slot.value != value) {
    next.paths |= OVERWROTE_STALE;
  }
  Some(next)
}

fn open(state: &State, leader: usize, index: usize) -> Option<Next> {
  let mut next = *state;
  let leading = next.nodes[leader].leading.as_mut()?;
  if leading.phase[index] != Phase::Free {
    return None;
  }
  leading.phase[index] = Phase::Opened;
  next.opened[usize::from(state.nodes[leader].term)][index] = true;
  Some(Step::plain(next))
}

fn propose(state: &State, leader: usize, index: usize, value: u8) -> Option<Next> {
  let term = state.nodes[leader].term;
  if state.nodes[leader].leading?.phase[index] != Phase::Free {
    return None;
  }
  let mut next = Step::plain(*state);
  guard(&mut next, index, value, term);
  let proposer = &mut next.state.nodes[leader];
  proposer.slots[index] = Some(Slot {
    term,
    classic: true,
    value,
  });
  let leading = proposer.leading.as_mut()?;
  leading.phase[index] = Phase::Decided;
  leading.acks[index] = bit(leader);
  Some(next)
}

/// The votes of `voters` at `index` as a leader of `term` hears them — each voter's entry there — or `None`
/// when one of them cannot vote: it is not in the term, holds nothing there, or (under the ballot rule)
/// holds something other than a fast vote of the term.
fn votes_of(scope: Scope, state: &State, term: u8, index: usize, voters: u8) -> Option<Vec<u8>> {
  members(voters, scope.nodes)
    .map(|voter| {
      let elector = state.nodes[voter];
      let slot = elector.slots[index].filter(|_| elector.term == term)?;
      let fast_vote_of_term = slot.term == term && !slot.classic;
      (scope.published() || fast_vote_of_term).then_some(slot.value)
    })
    .collect()
}

fn decide(
  scope: Scope,
  state: &State,
  leader: usize,
  index: usize,
  value: u8,
  voters: u8,
) -> Option<Next> {
  let deciding = state.nodes[leader];
  let leading = deciding.leading?;
  let votes = votes_of(scope, state, deciding.term, index, voters)?;
  let count = |candidate: u8| votes.iter().filter(|vote| **vote == candidate).count();
  let allowed = match scope.rule {
    Rule::Published(reading) => {
      published_allows(scope, reading, &deciding, index, count(value), &count)
    }
    Rule::Ballots => ballot_allows(scope, &leading, index, value, votes.len(), &count),
  };
  if !allowed {
    return None;
  }
  let mut next = Step::plain(*state);
  guard(&mut next, index, value, deciding.term);
  let decided = Slot {
    term: deciding.term,
    classic: true,
    value,
  };
  let changed = deciding.slots[index] != Some(decided);
  next.state.nodes[leader].slots[index] = Some(decided);
  let leading = next.state.nodes[leader].leading.as_mut()?;
  if changed {
    leading.acks[index] = bit(leader);
  }
  leading.phase[index] = Phase::Decided;
  if scope.rule == Rule::Ballots {
    next.paths |= DECIDED_FROM_VOTES;
  }
  if count(value) >= scope.fast_quorum() && !leading.committed[index] {
    leading.committed[index] = true;
    record_commit(&mut next, index, value, deciding.term, FAST_COMMIT);
  }
  Some(next)
}

/// Fast Raft §IV-B: only at `commitIndex + 1`, the entry with the most votes, ties broken arbitrarily (so
/// each tied value is its own branch) — restricted as `reading` reads the loop.
fn published_allows(
  scope: Scope,
  reading: Reading,
  deciding: &Node,
  index: usize,
  for_value: usize,
  count: &dyn Fn(u8) -> usize,
) -> bool {
  let Some(leading) = deciding.leading else {
    return false;
  };
  let next_to_commit = (0..scope.indices).find(|earlier| !leading.committed[*earlier]);
  let most = (0..scope.values).map(count).max().unwrap_or(0);
  let decided = leading.phase[index] == Phase::Decided;
  let holds_leader_approved = deciding.slots[index].is_some_and(|slot| slot.classic);
  let reading_allows = match reading {
    Reading::Literal => true,
    Reading::OncePerTerm => !decided,
    Reading::KeepLeaderApproved => !decided && !holds_leader_approved,
  };
  next_to_commit == Some(index) && for_value == most && reading_allows
}

/// The ballot rule within a term: an index opened to the fast track is decided once, and when a value could
/// have been chosen by a fast quorum — at least `heard + |F| − n` of the heard votes — it is that value.
fn ballot_allows(
  scope: Scope,
  leading: &Leading,
  index: usize,
  value: u8,
  heard: usize,
  count: &dyn Fn(u8) -> usize,
) -> bool {
  if leading.phase[index] != Phase::Opened {
    return false;
  }
  let threshold = heard + scope.fast_quorum() - scope.nodes;
  let forced = (0..scope.values).find(|other| count(*other) >= threshold);
  forced.is_none_or(|forced| forced == value)
}

fn replicate(
  scope: Scope,
  state: &State,
  leader: usize,
  node: usize,
  index: usize,
) -> Option<Next> {
  let source = state.nodes[leader];
  source.leading?;
  let slot = source.slots[index].filter(|slot| slot.classic)?;
  // The ballot rule replicates only this term's decisions (recovered ones are re-proposed at this term);
  // Fast Raft's leader sends every leader-approved entry from `nextIndex` on.
  if scope.rule == Rule::Ballots && slot.term != source.term {
    return None;
  }
  let target = state.nodes[node];
  if node == leader || target.term > source.term {
    return None;
  }
  let mut next = Step::plain(*state);
  guard(&mut next, index, slot.value, source.term);
  let follower = &mut next.state.nodes[node];
  if follower.term < source.term {
    follower.term = source.term;
    follower.voted_for = None;
    follower.leading = None;
  }
  follower.slots[index] = Some(slot);
  next.state.nodes[leader].leading.as_mut()?.acks[index] |= bit(node);
  if target.slots[index].is_some_and(|held| held.value != slot.value) {
    next.paths |= OVERWROTE_STALE;
  }
  Some(next)
}

fn commit(scope: Scope, state: &State, leader: usize, index: usize) -> Option<Next> {
  let source = state.nodes[leader];
  let leading = source.leading?;
  let slot = source.slots[index].filter(|slot| slot.classic && slot.term == source.term)?;
  if leading.committed[index] || size(leading.acks[index] | bit(leader)) < scope.majority() {
    return None;
  }
  let mut next = Step::plain(*state);
  next.state.nodes[leader].leading.as_mut()?.committed[index] = true;
  record_commit(&mut next, index, slot.value, source.term, CLASSIC_COMMIT);
  Some(next)
}

/// Records `value` committed at `index` in `term` in the history, or the disagreement when another value
/// was.
fn record_commit(next: &mut Next, index: usize, value: u8, term: u8, path: u64) {
  next.paths |= path;
  if index > 0 && next.state.chosen[index - 1].is_none() {
    next.paths |= OUT_OF_ORDER_COMMIT;
  }
  match next.state.chosen[index] {
    None => next.state.chosen[index] = Some(Chosen { value, term }),
    Some(first) if first.value != value => {
      next.fault = Some(Fault::Disagreement {
        index,
        first: first.value,
        second: value,
      });
    }
    Some(_) => {}
  }
}

/// The shape of a leader's state, without the nodes its acknowledgements name.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct LeadingShape {
  phase: [Phase; MAX_INDICES],
  committed: [bool; MAX_INDICES],
  acks: [u32; MAX_INDICES],
}

/// What the representative orders nodes by: a node's fields that name neither another node nor a value,
/// and the counts of references to it — all unchanged by renaming nodes or values.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Signature {
  term: u8,
  candidate: Option<bool>,
  votes_received: usize,
  slots: [Option<(u8, bool)>; MAX_INDICES],
  leading: Option<LeadingShape>,
  acked_by: [usize; MAX_INDICES],
}

fn signature(scope: Scope, state: &State, me: usize) -> Signature {
  let node = &state.nodes[me];
  let acked_by = std::array::from_fn(|index| {
    scope
      .node_ids()
      .filter(|leader| {
        state.nodes[*leader]
          .leading
          .is_some_and(|leading| leading.acks[index] & bit(me) != 0)
      })
      .count()
  });
  Signature {
    term: node.term,
    candidate: node.voted_for.map(|choice| usize::from(choice) == me),
    votes_received: scope
      .node_ids()
      .filter(|other| state.nodes[*other].voted_for == Some(id(me)))
      .count(),
    slots: node
      .slots
      .map(|slot| slot.map(|slot| (slot.term, slot.classic))),
    leading: node.leading.map(|leading| LeadingShape {
      phase: leading.phase,
      committed: leading.committed,
      acks: leading.acks.map(u8::count_ones),
    }),
    acked_by,
  }
}

/// `mask` with each node renumbered by `renumber`.
fn remap(mask: u8, renumber: &[usize; MAX_NODES], nodes: usize) -> u8 {
  members(mask, nodes).fold(0, |out, node| out | bit(renumber[node]))
}

/// `state` with its nodes placed in `order` (the node at `order[k]` becomes node `k`), its values renumbered
/// by first appearance, packed.
fn arranged(scope: Scope, state: &State, order: &[usize]) -> Key {
  let mut renumber = [0; MAX_NODES];
  for (new, old) in order.iter().enumerate() {
    renumber[*old] = new;
  }
  let mut representative = *state;
  for (new, old) in order.iter().enumerate() {
    let mut node = state.nodes[*old];
    node.voted_for = node
      .voted_for
      .map(|choice| id(renumber[usize::from(choice)]));
    if let Some(leading) = node.leading.as_mut() {
      leading.acks = leading.acks.map(|acks| remap(acks, &renumber, scope.nodes));
    }
    representative.nodes[new] = node;
  }
  renumber_values(scope, &mut representative);
  pack(&representative)
}

/// The representative of `state`'s class, the same for every renaming of its nodes and values: nodes sorted
/// by [`Signature`], and among the orders that permute only nodes of equal signature, the one whose packed
/// state is least.
fn canonical(scope: Scope, state: &State) -> Key {
  let signatures: Vec<Signature> = scope
    .node_ids()
    .map(|node| signature(scope, state, node))
    .collect();
  least_over_ties(&signatures, &mut |order| arranged(scope, state, order))
}

/// Renumbers the values of `state` by first appearance (slots node by node, then the history).
fn renumber_values(scope: Scope, state: &mut State) {
  let mut appearing: Vec<u8> = Vec::new();
  for node in scope.node_ids() {
    appearing.extend(
      state.nodes[node].slots[..scope.indices]
        .iter()
        .flatten()
        .map(|slot| slot.value),
    );
  }
  appearing.extend(state.chosen.iter().flatten().map(|chosen| chosen.value));
  appearing.extend(0..scope.values);
  let mut seen: Vec<u8> = Vec::new();
  for value in appearing {
    if !seen.contains(&value) {
      seen.push(value);
    }
  }
  let label = |value: u8| id(seen.iter().position(|old| *old == value).unwrap());
  for node in scope.node_ids() {
    for slot in state.nodes[node].slots.iter_mut().flatten() {
      slot.value = label(slot.value);
    }
  }
  for chosen in state.chosen.iter_mut().flatten() {
    chosen.value = label(chosen.value);
  }
}

/// A state packed into [`WORDS`] words.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Key([u64; WORDS]);

impl Hash for Key {
  fn hash<H: Hasher>(&self, hasher: &mut H) {
    for word in self.0 {
      hasher.write_u64(word);
    }
  }
}

/// Format: the widths of a packed field — a term, a node number plus one, a value plus one, a phase.
const TERM_BITS: usize = 3;
const NODE_BITS: usize = 3;
const VALUE_BITS: usize = 2;
const PHASE_BITS: usize = 2;

fn pack_slot(packer: &mut Packer<WORDS>, slot: Option<Slot>) {
  packer.put(1, u64::from(slot.is_some()));
  if let Some(slot) = slot {
    packer.put(VALUE_BITS, u64::from(slot.value));
    packer.put(TERM_BITS, u64::from(slot.term));
    packer.put(1, u64::from(slot.classic));
  }
}

fn take_slot(packer: &mut Packer<WORDS>) -> Option<Slot> {
  (packer.take(1) == 1).then(|| Slot {
    value: packer.small(VALUE_BITS),
    term: packer.small(TERM_BITS),
    classic: packer.take(1) == 1,
  })
}

fn pack_leading(packer: &mut Packer<WORDS>, leading: &Leading) {
  for index in 0..MAX_INDICES {
    packer.put(MAX_NODES, u64::from(leading.acks[index]));
    let phase = match leading.phase[index] {
      Phase::Free => 0,
      Phase::Opened => 1,
      Phase::Decided => 2,
    };
    packer.put(PHASE_BITS, phase);
    packer.put(1, u64::from(leading.committed[index]));
  }
}

fn take_leading(packer: &mut Packer<WORDS>) -> Leading {
  let mut leading = Leading::new();
  for index in 0..MAX_INDICES {
    leading.acks[index] = packer.small(MAX_NODES);
    leading.phase[index] = match packer.take(PHASE_BITS) {
      0 => Phase::Free,
      1 => Phase::Opened,
      _ => Phase::Decided,
    };
    leading.committed[index] = packer.take(1) == 1;
  }
  leading
}

fn pack(state: &State) -> Key {
  let mut packer = Packer::<WORDS>::new();
  for node in &state.nodes {
    packer.put(TERM_BITS, u64::from(node.term));
    packer.put_option(NODE_BITS, node.voted_for);
    for slot in node.slots {
      pack_slot(&mut packer, slot);
    }
    packer.put(1, u64::from(node.leading.is_some()));
    if let Some(leading) = &node.leading {
      pack_leading(&mut packer, leading);
    }
  }
  for opened in state.opened.iter().flatten() {
    packer.put(1, u64::from(*opened));
  }
  for chosen in state.chosen {
    packer.put_option(VALUE_BITS, chosen.map(|chosen| chosen.value));
    packer.put(TERM_BITS, chosen.map_or(0, |chosen| u64::from(chosen.term)));
  }
  Key(packer.words())
}

fn unpack(key: Key) -> State {
  let mut packer = Packer::<WORDS>::over(key.0);
  let mut state = State::initial();
  for node in &mut state.nodes {
    node.term = packer.small(TERM_BITS);
    node.voted_for = packer.take_option(NODE_BITS);
    for slot in &mut node.slots {
      *slot = take_slot(&mut packer);
    }
    node.leading = (packer.take(1) == 1).then(|| take_leading(&mut packer));
  }
  for opened in state.opened.iter_mut().flatten() {
    *opened = packer.take(1) == 1;
  }
  for chosen in &mut state.chosen {
    let value = packer.take_option(VALUE_BITS);
    let term = packer.small(TERM_BITS);
    *chosen = value.map(|value| Chosen { value, term });
  }
  state
}

/// A node's slots, as a history prints them: `v1@3L` is value 1 at term 3, leader-approved (`S` when
/// self-approved), and `-` an empty slot.
fn slots_of(scope: Scope, node: &Node) -> String {
  (0..scope.indices)
    .map(|index| match node.slots[index] {
      None => "-".to_owned(),
      Some(slot) => format!(
        "v{}@{}{}",
        slot.value,
        slot.term,
        if slot.classic { 'L' } else { 'S' }
      ),
    })
    .collect::<Vec<_>>()
    .join(",")
}

/// The slot model at one scope, as the exhaustive searches run it.
struct SlotModel {
  scope: Scope,
}

impl SlotModel {
  fn at(scope: Scope) -> SlotModel {
    SlotModel {
      scope: scope.checked(),
    }
  }

  /// The scope as a report's label.
  fn label(&self) -> String {
    let scope = self.scope;
    format!(
      "{:?}, {} nodes, {} indices, {} values, {} terms",
      scope.rule, scope.nodes, scope.indices, scope.values, scope.terms
    )
  }
}

impl Model for SlotModel {
  type State = State;
  type Action = Action;
  type Fault = Fault;
  type Key = Key;

  fn paths(&self) -> &'static [&'static str] {
    &PATHS
  }

  fn initial(&self) -> State {
    State::initial()
  }

  fn candidates(&self, state: &State, out: &mut Vec<Action>) {
    candidates(self.scope, state, out);
  }

  fn apply(&self, state: &State, action: Action) -> Option<Next> {
    apply(self.scope, state, action)
  }

  fn canonical(&self, state: &State) -> Key {
    canonical(self.scope, state)
  }

  fn unpack(&self, key: Key) -> State {
    unpack(key)
  }

  fn describe(&self, state: &State) -> String {
    self
      .scope
      .node_ids()
      .map(|node| {
        let held = &state.nodes[node];
        format!(
          "{}:t{} {}",
          NAMES[node],
          held.term,
          slots_of(self.scope, held)
        )
      })
      .collect::<Vec<_>>()
      .join("  ")
  }
}

/// Format: the nodes of the scripted history, and its two values.
const A: usize = 0;
const B: usize = 1;
const C: usize = 2;
const D: usize = 3;
const E: usize = 4;
const W: u8 = 0;
const V: u8 = 1;

/// The mask of `nodes`.
fn of(nodes: &[usize]) -> u8 {
  nodes.iter().fold(0, |mask, node| mask | bit(*node))
}

/// §3.7, with five nodes, where a fast quorum (four) is larger than a classic one (three): the
/// counterexample the research record predicted, derived by hand and replayed step by step under Fast
/// Raft's published rules (read so that a leader decides an index once in its term). A leads term 1 and
/// decides w from a three-way vote; B leads term 2 and decides v, which reaches C; C leads term 3, and B, C,
/// D and E answer with v — a fast quorum, so v commits. A, whose last leader-approved entry is w from term 1,
/// wins term 4 with D and E, who hold only self-approved v: their two votes are short of a classic quorum,
/// so A's own w stands, overwrites theirs, and commits.
#[test]
fn five_nodes_lose_a_fast_committed_entry_under_the_published_recovery() {
  let scope = Scope {
    nodes: 5,
    indices: 1,
    values: 2,
    terms: 4,
    rule: Rule::Published(Reading::OncePerTerm),
  };
  let history = [
    Action::Timeout { node: A },
    Action::Elect {
      node: A,
      quorum: of(&[A, B, C]),
    },
    Action::Insert {
      node: A,
      index: 0,
      value: W,
    },
    Action::Insert {
      node: B,
      index: 0,
      value: W,
    },
    Action::Insert {
      node: C,
      index: 0,
      value: V,
    },
    Action::Decide {
      leader: A,
      index: 0,
      value: W,
      voters: of(&[A, B, C]),
    },
    Action::Timeout { node: B },
    Action::Elect {
      node: B,
      quorum: of(&[B, C, D]),
    },
    Action::Insert {
      node: D,
      index: 0,
      value: V,
    },
    Action::Learn { node: E, term: 2 },
    Action::Insert {
      node: E,
      index: 0,
      value: V,
    },
    Action::Decide {
      leader: B,
      index: 0,
      value: V,
      voters: of(&[B, C, D, E]),
    },
    Action::Replicate {
      leader: B,
      node: C,
      index: 0,
    },
    Action::Timeout { node: C },
    Action::Elect {
      node: C,
      quorum: of(&[C, D, E]),
    },
    Action::Learn { node: B, term: 3 },
    // A fast quorum: v commits.
    Action::Decide {
      leader: C,
      index: 0,
      value: V,
      voters: of(&[B, C, D, E]),
    },
    Action::Timeout { node: A },
    Action::Timeout { node: A },
    Action::Timeout { node: A },
    Action::Elect {
      node: A,
      quorum: of(&[A, D, E]),
    },
  ];
  // A replicates its stale w over D's and E's v: the step Fast Raft's Lemma 2 says never happens.
  let overwrite = [Action::Replicate {
    leader: A,
    node: D,
    index: 0,
  }];
  let commit_w = [
    Action::Replicate {
      leader: A,
      node: E,
      index: 0,
    },
    Action::Decide {
      leader: A,
      index: 0,
      value: W,
      voters: of(&[A, D, E]),
    },
    Action::Replicate {
      leader: A,
      node: D,
      index: 0,
    },
    Action::Replicate {
      leader: A,
      node: E,
      index: 0,
    },
    Action::Commit {
      leader: A,
      index: 0,
    },
  ];
  let model = SlotModel::at(scope);
  let start = State::initial();
  let everything = [&history[..], &overwrite, &commit_w].concat();
  exhaustive::print_trace(&model, start, &everything);
  assert_eq!(exhaustive::run_script(&model, start, &history).1, None);
  assert_eq!(
    exhaustive::run_script(&model, start, &[&history[..], &overwrite].concat()).1,
    Some(Fault::OverwroteChosen {
      index: 0,
      chosen: V,
      sent: W,
      term: 4
    })
  );
  assert_eq!(
    exhaustive::run_script(&model, start, &everything).1,
    Some(Fault::Disagreement {
      index: 0,
      first: V,
      second: W
    })
  );
}

/// §3.7: Fast Raft's published recovery commits two values at one index under the two readings that let a
/// leader decide by votes over an entry it holds — as written, and once per term — and the search prints
/// the shortest history of each (the research record carries them). Under the third reading, which keeps a
/// leader's leader-approved entries as classic Raft does, the scope holds no fault; that reading fails
/// liveness instead ([`keeping_leader_approved_entries_stalls_the_log_after_one_crash`]).
#[test]
#[ignore = "exhaustive; CI's full-scale step runs it in release"]
fn the_published_recovery_loses_agreement_when_its_leader_decides_by_votes() {
  for reading in [
    Reading::Literal,
    Reading::OncePerTerm,
    Reading::KeepLeaderApproved,
  ] {
    let scope = Scope {
      nodes: 4,
      indices: 1,
      values: 2,
      terms: 4,
      rule: Rule::Published(reading),
    };
    let model = SlotModel::at(scope);
    let report = exhaustive::search(&model, State::initial(), &|fault| {
      matches!(fault, Fault::Disagreement { .. })
    });
    exhaustive::print_report(&model, &model.label(), State::initial(), &report);
    assert!(
      report.taken(&model, PATHS[0]) > 0,
      "{reading:?}: no fast commit was reached"
    );
    let lost = report.stopped.is_some();
    assert_eq!(lost, reading != Reading::KeepLeaderApproved, "{reading:?}");
  }
}

/// §3.7: under the reading that keeps a leader's leader-approved entries, one leader crash between a
/// decision and its commit stalls the log for good. A decides w and crashes with only B holding it; B leads
/// term 2 and replicates w to everyone, but may not decide over it by votes, and may not commit it, since
/// it is not of B's term. Every future of that state is searched: nothing ever commits again.
#[test]
fn keeping_leader_approved_entries_stalls_the_log_after_one_crash() {
  let model = SlotModel::at(Scope {
    nodes: 4,
    indices: 1,
    values: 2,
    terms: 4,
    rule: Rule::Published(Reading::KeepLeaderApproved),
  });
  let history = [
    Action::Timeout { node: A },
    Action::Elect {
      node: A,
      quorum: of(&[A, B, C]),
    },
    Action::Insert {
      node: A,
      index: 0,
      value: W,
    },
    Action::Insert {
      node: B,
      index: 0,
      value: W,
    },
    Action::Insert {
      node: C,
      index: 0,
      value: V,
    },
    Action::Decide {
      leader: A,
      index: 0,
      value: W,
      voters: of(&[A, B, C]),
    },
    // Only B acknowledges before A crashes: short of a majority.
    Action::Replicate {
      leader: A,
      node: B,
      index: 0,
    },
    Action::Timeout { node: B },
    Action::Elect {
      node: B,
      quorum: of(&[B, C, D]),
    },
    Action::Replicate {
      leader: B,
      node: A,
      index: 0,
    },
    Action::Replicate {
      leader: B,
      node: C,
      index: 0,
    },
    Action::Replicate {
      leader: B,
      node: D,
      index: 0,
    },
  ];
  exhaustive::print_trace(&model, State::initial(), &history);
  let (stuck, fault) = exhaustive::run_script(&model, State::initial(), &history);
  assert_eq!(fault, None);
  let futures = exhaustive::search(&model, stuck, &|_| true);
  exhaustive::print_report(&model, &model.label(), stuck, &futures);
  assert!(
    futures.states > 1,
    "the stalled state has futures to search"
  );
  assert_eq!(
    futures.taken(&model, PATHS[0]) + futures.taken(&model, PATHS[1]),
    0,
    "some future of the stalled state commits"
  );
}

/// Runs the ballot rule at `scope`: no fault, and every path taken. A fault is printed with the step that met
/// it, and with the whole shortest history when the serial search affords it.
fn ballots_hold(scope: Scope) {
  let model = SlotModel::at(scope);
  let (report, met) = exhaustive::explore(&model, MEMORY_CEILING_BYTES, false);
  exhaustive::print_report(&model, &model.label(), State::initial(), &report);
  if let Some((from, action, fault)) = met {
    eprintln!("{fault:?} met by {action} from {from:?}");
    let shortest = exhaustive::search(&model, State::initial(), &|_| true);
    exhaustive::print_report(&model, &model.label(), State::initial(), &shortest);
    panic!("the ballot rule failed at {scope:?}: {fault:?}");
  }
  for (path, (name, count)) in PATHS.iter().zip(&report.paths).enumerate() {
    let reachable = scope.indices > 1 || path != OUT_OF_ORDER_PATH;
    assert!(!reachable || *count > 0, "{name} never ran at {scope:?}");
  }
}

/// §4: the ballot rule keeps agreement and P2c with four nodes, one index, two values and three terms — the
/// scope the default suite affords; the full scopes run in release.
#[test]
fn the_ballot_recovery_keeps_agreement() {
  ballots_hold(Scope {
    nodes: 4,
    indices: 1,
    values: 2,
    terms: 3,
    rule: Rule::Ballots,
  });
}

/// §4 at full scope: where the published rule fails (four nodes, four terms), with five nodes and four
/// terms (a fast quorum larger than a classic one), with three values (three-way splits of a fast round),
/// and with two indices (a commit above an uncommitted index).
#[test]
#[ignore = "exhaustive; CI's full-scale step runs it in release"]
fn the_ballot_recovery_keeps_agreement_at_full_scope() {
  for (nodes, indices, values, terms) in [(4, 1, 2, 4), (5, 1, 2, 4), (4, 1, 3, 4), (3, 2, 2, 2)] {
    ballots_hold(Scope {
      nodes,
      indices,
      values,
      terms,
      rule: Rule::Ballots,
    });
  }
}

/// Runs one scope of the ballot rule named by the environment — the command behind the measurements in the
/// research record and `docs/wip/BENCHMARKS.md`:
///
/// `SLATES_SLOT_SCOPE=nodes,indices,values,terms [SLATES_SLOT_CEILING_GB=n] [SLATES_SLOT_LEVELS=1]
/// cargo test --release -p slates-cluster --test slot_model -- --ignored --exact one_scope_from_the_environment
/// --nocapture`
///
/// Skips, saying so, without `SLATES_SLOT_SCOPE`.
#[test]
#[ignore = "a measurement tool; runs only with SLATES_SLOT_SCOPE set"]
fn one_scope_from_the_environment() {
  let Ok(named) = std::env::var("SLATES_SLOT_SCOPE") else {
    eprintln!("skipping: set SLATES_SLOT_SCOPE=nodes,indices,values,terms to run one scope");
    return;
  };
  let numbers: Vec<usize> = named.split(',').map(|part| part.parse().unwrap()).collect();
  let [nodes, indices, values, terms] = numbers[..] else {
    panic!("SLATES_SLOT_SCOPE is nodes,indices,values,terms; got {named}");
  };
  let scope = Scope {
    nodes,
    indices,
    values: u8::try_from(values).unwrap(),
    terms: u8::try_from(terms).unwrap(),
    rule: Rule::Ballots,
  };
  let ceiling = std::env::var("SLATES_SLOT_CEILING_GB").map_or(MEMORY_CEILING_BYTES, |gigabytes| {
    gigabytes.parse::<usize>().unwrap() << 30
  });
  let levels = std::env::var("SLATES_SLOT_LEVELS").is_ok();
  let model = SlotModel::at(scope);
  let (report, met) = exhaustive::explore(&model, ceiling, levels);
  exhaustive::print_report(&model, &model.label(), State::initial(), &report);
  assert!(met.is_none(), "{met:?}");
}
