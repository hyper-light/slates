//! The design the dialect builds, checked exhaustively at small scope
//! (`docs/wip/research/consensus-enhancements.md` §3.5, §3.7 and §4; slices 9 and 10). The slot model
//! (`tests/slot_model.rs`) proved the ballot recovery over instances with no order. The dialect keeps Raft's
//! in-order log instead, and adds only what the fast track and parallel replication need above it — so two
//! steps of this design lie outside that proof, and this model is where they are checked:
//!
//! - **The candidate's own log is kept.** A new leader recovers only the indices above its last log entry:
//!   Raft's election rule (the candidate's last entry is at least as up to date as each voter's) and log
//!   matching are what make the entries below it safe.
//! - **A slot goes only under a classic commit.** A follower accepts out-of-order entries and fast votes only
//!   from the leader it is synced to — once its log holds that leader's no-op, the entry the leader appends
//!   after its recovery. A slot is an acceptor's record of an accepted value, and a node drops it only once it
//!   knows an index at or above the slot's is committed classically — held by a majority's logs at their
//!   leader's term — since Raft's election rule then puts that entry, and every value below it, in every later
//!   leader's log. A fast commit does not: its value is in a fast quorum's windows, not a majority's logs, and
//!   a successor that lacks it recovers it from them. So a follower learns only the classic commit index; the
//!   leader alone counts its fast choices, to apply and acknowledge them. Two earlier rules fail (2026-09-29):
//!   dropping a synced node's older slots at the sync itself ([`Variant::DropAtSync`]; the explorer met it at
//!   full scale first) loses a chosen value in 18 steps — a later leader's truncation erased the log entries
//!   that were then the only record; and pruning at a commit index that counts fast commits
//!   ([`Variant::PruneAtFastCommit`]; this model met it while checking the first correction) breaks log
//!   matching in 12 steps — a follower keeps a fast-committed entry under the fast leader's term while every
//!   later leader holds it under its own — and with a fourth term loses a chosen value: a candidate whose last
//!   entry a successor re-proposed under a newer term outranks the log that holds it, and the pruned slots are
//!   gone.
//!
//! **The model.** Each node has a Raft log (the leader-approved entries, in order, each with the term of the
//! leader that placed it), the classic commit index it knows, a window of slots (a leader's entry that arrived
//! out of order, or a fast vote, each with the term it was accepted in), the term it is synced to, and — while
//! leading — which nodes hold each index, where the fast track opens and the index of its no-op. The actions
//! are atomic:
//!
//! - a timeout;
//! - a term learnt from a leader's message;
//! - an election won by a chosen majority under Raft's rule, with the recovery below;
//! - an in-order append of the leader's log through an index, with Raft's truncation of the first
//!   conflicting entry above the follower's committed prefix, which it keeps as it holds it (as the code's
//!   append skips entries at or below its commit index), and the leader's classic commit index learnt
//!   through it;
//! - an out-of-order append of one of the leader's entries into a synced follower's window;
//! - a fast vote by a synced follower at an open index;
//! - the leader's decision at its next index from a classic quorum of fast votes;
//! - a classic proposal at its next index;
//! - a classic commit, once a majority's logs hold an index (under [`Variant::CommitFromWindows`], or its
//!   windows).
//!
//! **The recovery.** The voters report their window slots above the candidate's last log entry, each with its
//! ballot: its term and whether it is a fast vote (under [`Variant::ReportLogsToo`], their log entries too, a
//! log entry's ballot being its term and "decided"). Per index, the highest ballot decides as in the slot model
//! (a decision is re-proposed; a fast ballot re-proposes the value with at least `|Q| + |F| − n` of the
//! reports, and is otherwise free). The leader appends the recovered values at its term, a no-op at each free
//! index below the last recovered one, and then its own no-op, which is the sync point; it keeps its window.
//! It then either opens the fast track after that point or proposes classically; the dialect chooses by
//! measurement, and the model searches both.
//!
//! **The checks,** after every step:
//! - agreement: one value committed per index;
//! - P2c: no leader of a term at or after a commit sends another value there;
//! - log matching: two logs holding an entry of one term at one index agree up to it (a follower's committed
//!   prefix is classic, so its entries are every later leader's, term and all: the path that keeps a
//!   committed entry under an older term than the leader's is never taken);
//! - election safety: one leader per term;
//! - leader completeness: a leader's log holds every committed value at its index, and extends to every
//!   committed index.

// Test harness: an unwrap, expect or panic here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::fmt;
use std::hash::{Hash, Hasher};

use support::exhaustive::{self, MEMORY_CEILING_BYTES, Model, Packer, Step, least_over_ties};

/// Shape: the most nodes a scope here models.
const MAX_NODES: usize = 5;
/// Shape: the most log indices a scope here models (a length packs into two bits).
const MAX_INDICES: usize = 3;
/// Shape: the highest term a scope here reaches; a term packs into three bits.
const MAX_TERMS: u8 = 4;
/// Shape: the most proposed values a scope here models, beside the no-op; a value packs into two bits.
const MAX_VALUES: u8 = 3;
/// Format: the value a no-op carries (never proposed, and never renamed).
const NOOP: u8 = 0;
/// Format: the 64-bit words a packed state takes.
const WORDS: usize = 8;

/// The bounds of one search, and the variant of the design searched.
#[derive(Clone, Copy, Debug)]
struct Scope {
  nodes: usize,
  indices: usize,
  /// Proposed values, besides the no-op (they are `1..=values`).
  values: u8,
  terms: u8,
  variant: Variant,
}

/// The design, or one of the alternatives to a piece of it that the search measured and rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Variant {
  /// The design: a voter reports its window slots.
  Design,
  /// A voter also reports its log entries above the candidate's. Safe, but a classic leader's recovery then
  /// resurrects a deposed leader's uncommitted entries, which Raft discards — and the design needs none of
  /// them: classic commits are in order, so Raft's election rule keeps them, and a fast vote stays in its
  /// window until a sync to a leader whose log holds it.
  ReportLogsToo,
  /// A follower drops a window slot once its log covers the index: Raft's truncation can then erase the
  /// only record of a fast vote.
  DropCovered,
  /// A leader counts a window's copy of its entry toward a commit (out-of-order commitment): Raft's
  /// truncation can then erase a replica of a committed entry.
  CommitFromWindows,
  /// A node drops its slots of older terms at the sync itself (and a new leader clears its window at its
  /// recovery): a later leader's truncation can then erase the log entries that were the only remaining
  /// record of a chosen value.
  DropAtSync,
  /// A node prunes its slots at a commit index that counts fast commits: a fast-committed value is in no
  /// majority's logs, so a later leader may lack it and need the pruned slot.
  PruneAtFastCommit,
}

impl Scope {
  fn majority(self) -> usize {
    self.nodes / 2 + 1
  }

  /// The fast quorum: the smallest `f` with `2f + q > 2n` (Fast Paxos's requirement; ⌈3n/4⌉).
  fn fast_quorum(self) -> usize {
    let majority = self.majority();
    (1..=self.nodes)
      .find(|fast| 2 * fast + majority > 2 * self.nodes)
      .unwrap()
  }

  fn checked(self) -> Scope {
    assert!(self.nodes <= MAX_NODES && self.indices <= MAX_INDICES);
    assert!(self.values <= MAX_VALUES && self.terms <= MAX_TERMS);
    assert_eq!(self.fast_quorum(), (3 * self.nodes).div_ceil(4));
    self
  }

  fn node_ids(self) -> std::ops::Range<usize> {
    0..self.nodes
  }

  /// The proposed values.
  fn proposals(self) -> std::ops::RangeInclusive<u8> {
    1..=self.values
  }
}

/// A leader-approved entry in a node's log: the term of the leader that placed it, and its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Entry {
  term: u8,
  value: u8,
}

/// A window slot above a node's log: a leader's entry that arrived out of order, or a fast vote.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Slot {
  term: u8,
  fast: bool,
  value: u8,
}

impl Slot {
  /// The slot's ballot: its term, and within a term a leader's decision outranks a fast vote.
  fn ballot(self) -> (u8, bool) {
    (self.term, !self.fast)
  }
}

/// A leader's volatile state for its term.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Leading {
  /// Per node, how far its log is known to match this leader's.
  matched: [u8; MAX_NODES],
  /// Per index, the nodes known to hold this leader's entry there in their windows (counted toward a commit
  /// only under [`Variant::CommitFromWindows`]).
  window_acks: [u8; MAX_INDICES],
  /// The index (from one) of this leader's no-op — the sync point — or zero when its log had no room.
  sync_index: u8,
  /// The first index open to the fast track, or zero when this leader proposes classically.
  open_from: u8,
  /// Per index, whether this leader committed there.
  committed: [bool; MAX_INDICES],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Node {
  term: u8,
  voted_for: Option<u8>,
  /// The log's length, then its entries.
  length: u8,
  log: [Entry; MAX_INDICES],
  window: [Option<Slot>; MAX_INDICES],
  /// The term of the leader whose no-op this node's log holds (zero before any).
  synced: u8,
  /// The length of the prefix this node knows committed classically (under
  /// [`Variant::PruneAtFastCommit`], committed at all).
  commit: u8,
  leading: Option<Leading>,
}

impl Node {
  fn entries(&self) -> &[Entry] {
    &self.log[..usize::from(self.length)]
  }

  /// The last log entry's term and index (from one), as Raft's election compares them.
  fn last(&self) -> (u8, u8) {
    (
      self.entries().last().map_or(0, |entry| entry.term),
      self.length,
    )
  }
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
  chosen: [Option<Chosen>; MAX_INDICES],
}

impl State {
  fn initial() -> State {
    let node = Node {
      term: 0,
      voted_for: None,
      length: 0,
      log: [Entry { term: 0, value: 0 }; MAX_INDICES],
      window: [None; MAX_INDICES],
      synced: 0,
      commit: 0,
      leading: None,
    };
    State {
      nodes: [node; MAX_NODES],
      chosen: [None; MAX_INDICES],
    }
  }
}

/// One atomic step. Indices are from zero.
#[derive(Clone, Copy, Debug)]
enum Action {
  Timeout {
    node: usize,
  },
  Learn {
    node: usize,
    term: u8,
  },
  /// A candidate wins with `quorum` and recovers; `fast` says whether it opens the fast track.
  Elect {
    node: usize,
    quorum: u8,
    fast: bool,
  },
  /// The leader's log through `through` (a length) reaches `node` in order.
  Append {
    leader: usize,
    node: usize,
    through: u8,
  },
  /// The leader's entry at `index` reaches a synced `node` out of order.
  Scatter {
    leader: usize,
    node: usize,
    index: usize,
  },
  /// A synced node casts a fast vote at an open index.
  Vote {
    node: usize,
    index: usize,
    value: u8,
  },
  /// The leader decides its next index from the fast votes of `voters`.
  Decide {
    leader: usize,
    value: u8,
    voters: u8,
  },
  /// The leader proposes at its next index, classically.
  Propose {
    leader: usize,
    value: u8,
  },
  Commit {
    leader: usize,
    index: usize,
  },
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
      Action::Elect { node, quorum, fast } => write!(
        out,
        "{} is elected by {}{}",
        NAMES[node],
        names(quorum),
        if fast { ", fast" } else { "" }
      ),
      Action::Append {
        leader,
        node,
        through,
      } => write!(
        out,
        "{} appends to {} through {through}",
        NAMES[leader], NAMES[node]
      ),
      Action::Scatter {
        leader,
        node,
        index,
      } => write!(out, "{} scatters {index} to {}", NAMES[leader], NAMES[node]),
      Action::Vote { node, index, value } => {
        write!(out, "{} fast-votes v{value} at {index}", NAMES[node])
      }
      Action::Decide {
        leader,
        value,
        voters,
      } => write!(
        out,
        "{} decides v{value} from {}",
        NAMES[leader],
        names(voters)
      ),
      Action::Propose { leader, value } => write!(out, "{} proposes v{value}", NAMES[leader]),
      Action::Commit { leader, index } => write!(out, "{} commits {index}", NAMES[leader]),
    }
  }
}

/// Why a step is a violation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
  /// Two values committed at one index.
  Disagreement { index: usize, first: u8, second: u8 },
  /// A leader of the term a value was committed in, or a later term, sent another value at its index.
  OverwroteChosen {
    index: usize,
    chosen: u8,
    sent: u8,
    term: u8,
  },
  /// Two logs hold an entry of one term at one index but differ up to it.
  LogsDiverge { index: usize },
  /// Two leaders in one term.
  TwoLeaders { term: u8 },
  /// A new leader's log lacks a committed value, or holds another value where one was committed.
  LeaderIncomplete { index: usize },
}

/// The paths a step took, as bits (the non-vacuity counters' keys).
const FAST_COMMIT: u64 = 1;
const CLASSIC_COMMIT: u64 = 2;
const OUT_OF_ORDER_COMMIT: u64 = 4;
const RECOVERED_FROM_A_LOG: u64 = 8;
const RECOVERED_FROM_A_WINDOW_DECISION: u64 = 16;
const RECOVERED_A_FAST_CHOICE: u64 = 32;
const FILLED_A_HOLE: u64 = 64;
const SYNC_DROPPED_A_SLOT: u64 = 128;
const TRUNCATED_A_LOG: u64 = 256;
const SCATTERED: u64 = 512;
const KEPT_A_COMMITTED_TERM: u64 = 1024;
const PRUNED_AT_COMMIT: u64 = 2048;
/// Format: each path's name, in bit order.
const PATHS: [&str; 12] = [
  "fast commits",
  "classic commits",
  "commits above an uncommitted index",
  "recoveries from a log above the candidate's",
  "recoveries from a window decision",
  "recoveries of a possible fast choice",
  "holes filled with no-ops",
  "slots dropped at a sync",
  "logs truncated",
  "entries scattered",
  "committed entries kept under an older term",
  "slots pruned at a commit",
];

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

fn majorities(scope: Scope) -> impl Iterator<Item = u8> {
  let everyone = u8::try_from((1_usize << scope.nodes) - 1).unwrap();
  (1..=everyone).filter(move |mask| size(*mask) >= scope.majority())
}

fn leader_of(scope: Scope, state: &State, term: u8) -> Option<usize> {
  scope
    .node_ids()
    .find(|node| state.nodes[*node].term == term && state.nodes[*node].leading.is_some())
}

fn candidates(scope: Scope, state: &State, out: &mut Vec<Action>) {
  for node in scope.node_ids() {
    out.push(Action::Timeout { node });
    out.extend((1..=scope.terms).map(|term| Action::Learn { node, term }));
    for quorum in majorities(scope).filter(|quorum| quorum & bit(node) != 0) {
      out.push(Action::Elect {
        node,
        quorum,
        fast: false,
      });
      out.push(Action::Elect {
        node,
        quorum,
        fast: true,
      });
    }
    for index in 0..scope.indices {
      out.extend(
        scope
          .proposals()
          .map(|value| Action::Vote { node, index, value }),
      );
    }
    if state.nodes[node].leading.is_some() {
      leader_candidates(scope, node, out);
    }
  }
}

fn leader_candidates(scope: Scope, leader: usize, out: &mut Vec<Action>) {
  for value in scope.proposals() {
    out.push(Action::Propose { leader, value });
    out.extend(majorities(scope).map(|voters| Action::Decide {
      leader,
      value,
      voters,
    }));
  }
  for index in 0..scope.indices {
    out.push(Action::Commit { leader, index });
  }
  for node in scope.node_ids().filter(|node| *node != leader) {
    for through in 1..=scope.indices {
      out.push(Action::Append {
        leader,
        node,
        through: u8::try_from(through).unwrap(),
      });
    }
    for index in 0..scope.indices {
      out.push(Action::Scatter {
        leader,
        node,
        index,
      });
    }
  }
}

fn apply(scope: Scope, state: &State, action: Action) -> Option<Next> {
  let next = match action {
    Action::Timeout { node } => timeout(scope, state, node),
    Action::Learn { node, term } => learn(scope, state, node, term),
    Action::Elect { node, quorum, fast } => elect(scope, state, node, quorum, fast),
    Action::Append {
      leader,
      node,
      through,
    } => append(scope, state, leader, node, through),
    Action::Scatter {
      leader,
      node,
      index,
    } => scatter(scope, state, leader, node, index),
    Action::Vote { node, index, value } => vote(scope, state, node, index, value),
    Action::Decide {
      leader,
      value,
      voters,
    } => decide(scope, state, leader, value, voters),
    Action::Propose { leader, value } => propose(scope, state, leader, value),
    Action::Commit { leader, index } => commit(scope, state, leader, index),
  }?;
  let mut next = next;
  if next.fault.is_none() {
    next.fault = diverging_logs(scope, &next.state);
  }
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

/// Raft's vote: a later term, or this term with no vote elsewhere, and a candidate whose last log entry is at
/// least as up to date.
fn grants(state: &State, voter: usize, candidate: usize) -> bool {
  let (elector, running) = (state.nodes[voter], state.nodes[candidate]);
  let term_allows = elector.term < running.term
    || (elector.term == running.term
      && elector
        .voted_for
        .is_none_or(|choice| usize::from(choice) == candidate));
  term_allows && running.last() >= elector.last()
}

/// A voter's report at `index`: its window slot, which it keeps until it syncs to a newer term — so Raft's
/// truncation of its log never erases the vote a slot records (under [`Variant::ReportLogsToo`], the higher
/// ballot of that and its log entry there).
fn report_at(scope: Scope, node: &Node, index: usize) -> Option<Slot> {
  if scope.variant != Variant::ReportLogsToo {
    return node.window[index];
  }
  let logged = (index < usize::from(node.length)).then(|| Slot {
    term: node.log[index].term,
    fast: false,
    value: node.log[index].value,
  });
  match (logged, node.window[index]) {
    (Some(entry), Some(slot)) => Some(if slot.ballot() > entry.ballot() {
      slot
    } else {
      entry
    }),
    (entry, slot) => entry.or(slot),
  }
}

/// What the recovery decides at `index` from the reports of `quorum`: a value it must re-propose with the
/// path that constrained it, or `None` when the index is free.
fn recovered_at(scope: Scope, state: &State, quorum: u8, index: usize) -> Option<(u8, u64)> {
  let reports: Vec<(Slot, bool)> = members(quorum, scope.nodes)
    .filter_map(|voter| {
      let node = &state.nodes[voter];
      report_at(scope, node, index).map(|slot| (slot, node.window[index] != Some(slot)))
    })
    .collect();
  let highest = reports.iter().map(|(slot, _)| slot.ballot()).max()?;
  let at_highest: Vec<&(Slot, bool)> = reports
    .iter()
    .filter(|(slot, _)| slot.ballot() == highest)
    .collect();
  let (_, decided) = highest;
  if decided {
    let (slot, in_log) = at_highest[0];
    let path = if *in_log {
      RECOVERED_FROM_A_LOG
    } else {
      RECOVERED_FROM_A_WINDOW_DECISION
    };
    return Some((slot.value, path));
  }
  let threshold = size(quorum) + scope.fast_quorum() - scope.nodes;
  scope
    .proposals()
    .find(|value| {
      at_highest
        .iter()
        .filter(|(slot, _)| slot.value == *value)
        .count()
        >= threshold
    })
    .map(|value| (value, RECOVERED_A_FAST_CHOICE))
}

fn elect(scope: Scope, state: &State, node: usize, quorum: u8, fast: bool) -> Option<Next> {
  let running = state.nodes[node];
  if quorum & bit(node) == 0 || running.leading.is_some() || running.voted_for != Some(id(node)) {
    return None;
  }
  if !members(quorum, scope.nodes)
    .filter(|voter| *voter != node)
    .all(|voter| grants(state, voter, node))
  {
    return None;
  }
  let term = running.term;
  let mut next = Step::plain(*state);
  for voter in members(quorum, scope.nodes) {
    let elector = &mut next.state.nodes[voter];
    elector.term = term;
    elector.voted_for = Some(id(node));
    elector.leading = None;
  }
  if leader_of(scope, state, term).is_some() {
    next.fault = Some(Fault::TwoLeaders { term });
    return Some(next);
  }
  recover(scope, state, node, quorum, fast, &mut next);
  Some(next)
}

/// The recovery above the candidate's last log entry, then its no-op (the module doc's rule).
fn recover(scope: Scope, state: &State, node: usize, quorum: u8, fast: bool, next: &mut Next) {
  let term = state.nodes[node].term;
  let kept = usize::from(state.nodes[node].length);
  let recovered: Vec<Option<(u8, u64)>> = (kept..scope.indices)
    .map(|index| recovered_at(scope, state, quorum, index))
    .collect();
  let last_recovered = recovered
    .iter()
    .rposition(Option::is_some)
    .map(|at| kept + at + 1);
  let recovered_end = last_recovered.unwrap_or(kept);
  let leader = &mut next.state.nodes[node];
  for (index, found) in (kept..recovered_end).zip(&recovered) {
    let value = match found {
      Some((value, path)) => {
        next.paths |= path;
        *value
      }
      None => {
        next.paths |= FILLED_A_HOLE;
        NOOP
      }
    };
    leader.log[index] = Entry { term, value };
  }
  let mut length = recovered_end;
  let sync_index = if length < scope.indices {
    leader.log[length] = Entry { term, value: NOOP };
    length += 1;
    u8::try_from(length).unwrap()
  } else {
    0
  };
  leader.length = u8::try_from(length).unwrap();
  if scope.variant == Variant::DropAtSync {
    leader.window = [None; MAX_INDICES];
  }
  leader.synced = term;
  leader.leading = Some(Leading {
    matched: [0; MAX_NODES],
    window_acks: [0; MAX_INDICES],
    sync_index,
    open_from: if fast && sync_index > 0 {
      sync_index + 1
    } else {
      0
    },
    committed: [false; MAX_INDICES],
  });
  next.paths |= settle_window(scope, &mut next.state.nodes[node]);
  check_new_leader(scope, next, node);
  let entries: Vec<Entry> = next.state.nodes[node].entries().to_vec();
  for (index, entry) in entries.iter().enumerate().skip(kept) {
    guard(next, index, entry.value, term);
  }
}

/// Faults a new leader whose log lacks a committed value or extends short of a committed index.
fn check_new_leader(scope: Scope, next: &mut Next, node: usize) {
  let leader = next.state.nodes[node];
  for index in 0..scope.indices {
    let Some(chosen) = next.state.chosen[index] else {
      continue;
    };
    let holds = index < usize::from(leader.length) && leader.log[index].value == chosen.value;
    if !holds && next.fault.is_none() {
      next.fault = Some(Fault::LeaderIncomplete { index });
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

/// The leader's log through `through` reaches `node` in order: Raft's append, from where the two logs
/// agree, truncating the follower's first conflicting entry and all after it — above the follower's committed
/// prefix, which it keeps as it holds it (the dialect's consistency check). The follower learns the leader's
/// commit index as far as the append reaches, and a follower whose log thereby reaches the leader's no-op is
/// synced; its window then settles ([`settle_window`]).
fn append(scope: Scope, state: &State, leader: usize, node: usize, through: u8) -> Option<Next> {
  let source = state.nodes[leader];
  let leading = source.leading?;
  let target = state.nodes[node];
  if node == leader || target.term > source.term || through > source.length {
    return None;
  }
  let kept = usize::from(target.commit).min(usize::from(target.length));
  let agreed = kept
    + source.entries()[kept.min(source.entries().len())..]
      .iter()
      .zip(&target.entries()[kept..])
      .take_while(|(ours, theirs)| ours == theirs)
      .count();
  let mut next = Step::plain(*state);
  let follower = &mut next.state.nodes[node];
  if follower.term < source.term {
    follower.term = source.term;
    follower.voted_for = None;
    follower.leading = None;
  }
  let through = usize::from(through);
  if through > agreed {
    if usize::from(target.length) > agreed {
      next.paths |= TRUNCATED_A_LOG;
    }
    follower.log[kept..through].copy_from_slice(&source.log[kept..through]);
    follower.length = u8::try_from(through).unwrap();
  }
  if (0..kept).any(|index| follower.log[index].term != source.log[index].term) {
    next.paths |= KEPT_A_COMMITTED_TERM;
  }
  let learnt = source.commit.min(u8::try_from(through).unwrap());
  follower.commit = follower.commit.max(learnt);
  next.paths |= sync_and_prune(scope, follower, &source, leading.sync_index);
  let matched = u8::try_from(through.min(usize::from(next.state.nodes[node].length))).unwrap();
  let acks = &mut next.state.nodes[leader].leading.as_mut()?.matched[node];
  *acks = (*acks).max(matched);
  for (index, entry) in source.entries().iter().enumerate().take(through) {
    guard(&mut next, index, entry.value, source.term);
  }
  Some(next)
}

/// After an append from `source`: the follower is synced once its log holds the leader's no-op — its log
/// agreeing with the leader's through it, the committed prefix it keeps counting as agreeing, not merely as
/// long, since a stale log may be. Its window then settles. Returns the paths taken.
fn sync_and_prune(scope: Scope, follower: &mut Node, source: &Node, sync_index: u8) -> u64 {
  let kept = usize::from(follower.commit).min(usize::from(follower.length));
  let agreeing = kept
    + source.entries()[kept.min(source.entries().len())..]
      .iter()
      .zip(&follower.entries()[kept..])
      .take_while(|(ours, theirs)| ours == theirs)
      .count();
  if sync_index > 0 && agreeing >= usize::from(sync_index) && follower.synced < source.term {
    follower.synced = source.term;
  }
  settle_window(scope, follower)
}

/// A node's window after its commit index or sync moved: a slot at a classically committed index goes (every
/// later leader's log holds that index). Under [`Variant::DropAtSync`] a synced node's slots of older terms go
/// too, and under [`Variant::DropCovered`] a slot its log covers. Returns the paths taken.
fn settle_window(scope: Scope, node: &mut Node) -> u64 {
  let (length, commit, synced) = (
    usize::from(node.length),
    usize::from(node.commit),
    node.synced,
  );
  let mut paths = 0;
  for (index, slot) in node.window.iter_mut().enumerate() {
    let Some(held) = *slot else {
      continue;
    };
    let covered = scope.variant == Variant::DropCovered && index < length;
    let older = scope.variant == Variant::DropAtSync && held.term < synced;
    if index < commit {
      paths |= PRUNED_AT_COMMIT;
      *slot = None;
    } else if covered || older {
      paths |= SYNC_DROPPED_A_SLOT;
      *slot = None;
    }
  }
  paths
}

/// A leader's known commit after it committed through `through` (a length) classically, or — under
/// [`Variant::PruneAtFastCommit`] — its committed prefix extended over each index committed in its term, fast
/// ones included.
fn leader_commit(scope: Scope, node: &mut Node, through: usize) {
  let mut commit = usize::from(node.commit).max(through);
  if scope.variant == Variant::PruneAtFastCommit
    && let Some(leading) = node.leading
  {
    while leading.committed.get(commit).copied().unwrap_or(false) {
      commit += 1;
    }
  }
  node.commit = u8::try_from(commit).unwrap();
}

/// The leader's entry at `index` — one of its own term — reaches a follower synced to it, above that
/// follower's log, out of order.
fn scatter(scope: Scope, state: &State, leader: usize, node: usize, index: usize) -> Option<Next> {
  let source = state.nodes[leader];
  source.leading?;
  let target = state.nodes[node];
  let entry = *source.entries().get(index)?;
  if node == leader
    || entry.term != source.term
    || target.term != source.term
    || target.synced != source.term
    || index < usize::from(target.length)
  {
    return None;
  }
  let slot = Slot {
    term: entry.term,
    fast: false,
    value: entry.value,
  };
  if target.window[index].is_some_and(|held| held.ballot() >= slot.ballot()) {
    return None;
  }
  let mut next = Step::plain(*state);
  next.paths |= SCATTERED;
  next.state.nodes[node].window[index] = Some(slot);
  if scope.variant == Variant::CommitFromWindows {
    next.state.nodes[leader].leading.as_mut()?.window_acks[index] |= bit(node);
  }
  guard(&mut next, index, entry.value, source.term);
  Some(next)
}

/// A node synced to its term's leader casts a fast vote at an index that leader opened, above its own log.
fn vote(scope: Scope, state: &State, node: usize, index: usize, value: u8) -> Option<Next> {
  let voter = state.nodes[node];
  let leader = leader_of(scope, state, voter.term)?;
  let leading = state.nodes[leader].leading?;
  let position = u8::try_from(index + 1).unwrap();
  if voter.synced != voter.term
    || leading.open_from == 0
    || position < leading.open_from
    || index < usize::from(voter.length)
    || voter.window[index].is_some_and(|held| held.ballot() >= (voter.term, false))
  {
    return None;
  }
  let mut next = *state;
  next.nodes[node].window[index] = Some(Slot {
    term: voter.term,
    fast: true,
    value,
  });
  Some(Step::plain(next))
}

/// The leader decides its next index from the fast votes of `voters` (each holding a fast vote of this
/// term there): the value a fast quorum could have chosen, when one could, else any; committed at once when
/// a fast quorum voted it.
fn decide(scope: Scope, state: &State, leader: usize, value: u8, voters: u8) -> Option<Next> {
  let deciding = state.nodes[leader];
  let leading = deciding.leading?;
  let index = usize::from(deciding.length);
  let position = u8::try_from(index + 1).unwrap();
  if leading.open_from == 0 || position < leading.open_from || index >= scope.indices {
    return None;
  }
  let votes: Vec<u8> = members(voters, scope.nodes)
    .map(|voter| {
      let node = state.nodes[voter];
      node.window[index]
        .filter(|slot| slot.fast && slot.term == deciding.term && node.term == deciding.term)
        .map(|slot| slot.value)
    })
    .collect::<Option<_>>()?;
  let count = |candidate: u8| votes.iter().filter(|vote| **vote == candidate).count();
  let threshold = votes.len() + scope.fast_quorum() - scope.nodes;
  let forced = scope.proposals().find(|other| count(*other) >= threshold);
  if forced.is_some_and(|forced| forced != value) {
    return None;
  }
  let mut next = Step::plain(*state);
  guard(&mut next, index, value, deciding.term);
  let decider = &mut next.state.nodes[leader];
  decider.log[index] = Entry {
    term: deciding.term,
    value,
  };
  decider.length += 1;
  if count(value) >= scope.fast_quorum() {
    decider.leading.as_mut()?.committed[index] = true;
    leader_commit(scope, decider, 0);
    next.paths |= settle_window(scope, decider);
    record_commit(&mut next, index, value, deciding.term, FAST_COMMIT);
  }
  Some(next)
}

/// The leader proposes at its next index, classically (a leader that did not open the fast track).
fn propose(scope: Scope, state: &State, leader: usize, value: u8) -> Option<Next> {
  let proposer = state.nodes[leader];
  let leading = proposer.leading?;
  let index = usize::from(proposer.length);
  if leading.open_from != 0 || index >= scope.indices {
    return None;
  }
  let mut next = Step::plain(*state);
  guard(&mut next, index, value, proposer.term);
  let placed = &mut next.state.nodes[leader];
  placed.log[index] = Entry {
    term: proposer.term,
    value,
  };
  placed.length += 1;
  Some(next)
}

/// The leader commits its entry of this term at `index` once a majority's logs hold it (matched through it),
/// and with it every index below: those logs agree up to it. A window's copy is not counted — out-of-order
/// commitment let Raft's truncation erase a replica of a committed entry (the first search's 12-step history,
/// 2026-09-28).
fn commit(scope: Scope, state: &State, leader: usize, index: usize) -> Option<Next> {
  let source = state.nodes[leader];
  let leading = source.leading?;
  let entry = *source.entries().get(index)?;
  if entry.term != source.term || leading.committed[index] {
    return None;
  }
  let position = u8::try_from(index + 1).unwrap();
  let holders = scope
    .node_ids()
    .filter(|node| {
      *node == leader
        || leading.matched[*node] >= position
        || leading.window_acks[index] & bit(*node) != 0
    })
    .count();
  if holders < scope.majority() {
    return None;
  }
  let mut next = Step::plain(*state);
  // A majority's logs agree up to the index, so every index below commits with it; a count that includes
  // windows (the rejected variant) commits the index alone.
  let from = if scope.variant == Variant::CommitFromWindows {
    index
  } else {
    0
  };
  let leading = next.state.nodes[leader].leading.as_mut()?;
  for below in from..=index {
    leading.committed[below] = true;
  }
  let committer = &mut next.state.nodes[leader];
  let classic = if scope.variant == Variant::CommitFromWindows {
    0
  } else {
    index + 1
  };
  leader_commit(scope, committer, classic);
  next.paths |= settle_window(scope, committer);
  for (below, held) in source
    .entries()
    .iter()
    .enumerate()
    .take(index + 1)
    .skip(from)
  {
    record_commit(&mut next, below, held.value, source.term, CLASSIC_COMMIT);
  }
  Some(next)
}

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

/// Log matching: any two logs holding an entry of one term at one index agree up to it.
fn diverging_logs(scope: Scope, state: &State) -> Option<Fault> {
  for (first, second) in scope
    .node_ids()
    .flat_map(|first| (first + 1..scope.nodes).map(move |second| (first, second)))
  {
    let (left, right) = (state.nodes[first].entries(), state.nodes[second].entries());
    for index in 0..left.len().min(right.len()) {
      if left[index].term == right[index].term && left[..=index] != right[..=index] {
        return Some(Fault::LogsDiverge { index });
      }
    }
  }
  None
}

/// The shape of a leader's state, without the nodes its acknowledgements name.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct LeadingShape {
  sync_index: u8,
  open_from: u8,
  committed: [bool; MAX_INDICES],
  /// How far the nodes' logs are known to match, sorted.
  matched: [u8; MAX_NODES],
  acks: [u32; MAX_INDICES],
}

/// What the representative orders nodes by: a node's fields that name neither another node nor a proposed
/// value, and the counts of references to it — all unchanged by renaming nodes or proposed values.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Signature {
  term: u8,
  candidate: Option<bool>,
  votes_received: usize,
  length: u8,
  log_terms: [u8; MAX_INDICES],
  log_noops: [bool; MAX_INDICES],
  window: [Option<(u8, bool, bool)>; MAX_INDICES],
  synced: u8,
  commit: u8,
  leading: Option<LeadingShape>,
  matched_by: [u8; MAX_NODES],
  held_by_leaders: [usize; MAX_INDICES],
}

fn signature(scope: Scope, state: &State, me: usize) -> Signature {
  let node = &state.nodes[me];
  let mut matched_by: Vec<u8> = scope
    .node_ids()
    .filter_map(|leader| {
      state.nodes[leader]
        .leading
        .map(|leading| leading.matched[me])
    })
    .collect();
  matched_by.sort_unstable();
  let mut matches = [0; MAX_NODES];
  for (slot, value) in matches.iter_mut().zip(matched_by) {
    *slot = value;
  }
  Signature {
    term: node.term,
    candidate: node.voted_for.map(|choice| usize::from(choice) == me),
    votes_received: scope
      .node_ids()
      .filter(|other| state.nodes[*other].voted_for == Some(id(me)))
      .count(),
    length: node.length,
    log_terms: node.log.map(|entry| entry.term),
    log_noops: node.log.map(|entry| entry.value == NOOP),
    window: node
      .window
      .map(|slot| slot.map(|slot| (slot.term, slot.fast, slot.value == NOOP))),
    synced: node.synced,
    commit: node.commit,
    leading: node.leading.map(|leading| {
      let mut matched = leading.matched;
      matched.sort_unstable();
      LeadingShape {
        sync_index: leading.sync_index,
        open_from: leading.open_from,
        committed: leading.committed,
        matched,
        acks: leading.window_acks.map(u8::count_ones),
      }
    }),
    matched_by: matches,
    held_by_leaders: std::array::from_fn(|index| {
      scope
        .node_ids()
        .filter(|leader| {
          state.nodes[*leader]
            .leading
            .is_some_and(|leading| leading.window_acks[index] & bit(me) != 0)
        })
        .count()
    }),
  }
}

fn remap(mask: u8, renumber: &[usize; MAX_NODES], nodes: usize) -> u8 {
  members(mask, nodes).fold(0, |out, node| out | bit(renumber[node]))
}

/// `state` with its nodes placed in `order`, its proposed values renumbered by first appearance, packed.
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
      let mut matched = [0; MAX_NODES];
      for (from, value) in leading.matched.iter().enumerate().take(scope.nodes) {
        matched[renumber[from]] = *value;
      }
      leading.matched = matched;
      leading.window_acks = leading
        .window_acks
        .map(|acks| remap(acks, &renumber, scope.nodes));
    }
    representative.nodes[new] = node;
  }
  renumber_values(scope, &mut representative);
  pack(&representative)
}

fn canonical(scope: Scope, state: &State) -> Key {
  let signatures: Vec<Signature> = scope
    .node_ids()
    .map(|node| signature(scope, state, node))
    .collect();
  least_over_ties(&signatures, &mut |order| arranged(scope, state, order))
}

/// Renumbers the proposed values of `state` by first appearance (logs, windows, then the history); the no-op
/// keeps its number.
fn renumber_values(scope: Scope, state: &mut State) {
  let mut appearing: Vec<u8> = Vec::new();
  for node in scope.node_ids() {
    let held = &state.nodes[node];
    appearing.extend(held.entries().iter().map(|entry| entry.value));
    appearing.extend(held.window.iter().flatten().map(|slot| slot.value));
  }
  appearing.extend(state.chosen.iter().flatten().map(|chosen| chosen.value));
  appearing.extend(scope.proposals());
  let mut seen: Vec<u8> = Vec::new();
  for value in appearing.into_iter().filter(|value| *value != NOOP) {
    if !seen.contains(&value) {
      seen.push(value);
    }
  }
  let label = |value: u8| {
    if value == NOOP {
      NOOP
    } else {
      id(seen.iter().position(|old| *old == value).unwrap() + 1)
    }
  };
  for node in scope.node_ids() {
    let held = &mut state.nodes[node];
    let length = usize::from(held.length);
    for entry in &mut held.log[..length] {
      entry.value = label(entry.value);
    }
    for slot in held.window.iter_mut().flatten() {
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

/// Format: the widths of a packed field — a term, a node number plus one, a value, a value plus one (an
/// optional value: up to [`MAX_VALUES`] plus one), a log length, and an index plus one.
const TERM_BITS: usize = 3;
const NODE_BITS: usize = 3;
const VALUE_BITS: usize = 2;
const OPTIONAL_VALUE_BITS: usize = 3;
const LENGTH_BITS: usize = 2;
const INDEX_BITS: usize = 3;

fn pack_node(packer: &mut Packer<WORDS>, node: &Node) {
  packer.put(TERM_BITS, u64::from(node.term));
  packer.put_option(NODE_BITS, node.voted_for);
  packer.put(LENGTH_BITS, u64::from(node.length));
  for entry in node.entries() {
    packer.put(TERM_BITS, u64::from(entry.term));
    packer.put(VALUE_BITS, u64::from(entry.value));
  }
  for slot in node.window {
    packer.put(1, u64::from(slot.is_some()));
    if let Some(slot) = slot {
      packer.put(TERM_BITS, u64::from(slot.term));
      packer.put(1, u64::from(slot.fast));
      packer.put(VALUE_BITS, u64::from(slot.value));
    }
  }
  packer.put(TERM_BITS, u64::from(node.synced));
  packer.put(LENGTH_BITS, u64::from(node.commit));
  packer.put(1, u64::from(node.leading.is_some()));
  if let Some(leading) = node.leading {
    for matched in leading.matched {
      packer.put(LENGTH_BITS, u64::from(matched));
    }
    for acks in leading.window_acks {
      packer.put(MAX_NODES, u64::from(acks));
    }
    packer.put(INDEX_BITS, u64::from(leading.sync_index));
    packer.put(INDEX_BITS, u64::from(leading.open_from));
    for committed in leading.committed {
      packer.put(1, u64::from(committed));
    }
  }
}

fn take_node(packer: &mut Packer<WORDS>) -> Node {
  let mut node = State::initial().nodes[0];
  node.term = packer.small(TERM_BITS);
  node.voted_for = packer.take_option(NODE_BITS);
  node.length = packer.small(LENGTH_BITS);
  for entry in &mut node.log[..usize::from(node.length)] {
    entry.term = packer.small(TERM_BITS);
    entry.value = packer.small(VALUE_BITS);
  }
  for slot in &mut node.window {
    *slot = (packer.take(1) == 1).then(|| Slot {
      term: packer.small(TERM_BITS),
      fast: packer.take(1) == 1,
      value: packer.small(VALUE_BITS),
    });
  }
  node.synced = packer.small(TERM_BITS);
  node.commit = packer.small(LENGTH_BITS);
  if packer.take(1) == 1 {
    let mut leading = Leading {
      matched: [0; MAX_NODES],
      window_acks: [0; MAX_INDICES],
      sync_index: 0,
      open_from: 0,
      committed: [false; MAX_INDICES],
    };
    for matched in &mut leading.matched {
      *matched = packer.small(LENGTH_BITS);
    }
    for acks in &mut leading.window_acks {
      *acks = packer.small(MAX_NODES);
    }
    leading.sync_index = packer.small(INDEX_BITS);
    leading.open_from = packer.small(INDEX_BITS);
    for committed in &mut leading.committed {
      *committed = packer.take(1) == 1;
    }
    node.leading = Some(leading);
  }
  node
}

fn pack(state: &State) -> Key {
  let mut packer = Packer::<WORDS>::new();
  for node in &state.nodes {
    pack_node(&mut packer, node);
  }
  for chosen in state.chosen {
    packer.put_option(OPTIONAL_VALUE_BITS, chosen.map(|chosen| chosen.value));
    packer.put(TERM_BITS, chosen.map_or(0, |chosen| u64::from(chosen.term)));
  }
  Key(packer.words())
}

fn unpack(key: Key) -> State {
  let mut packer = Packer::<WORDS>::over(key.0);
  let mut state = State::initial();
  for node in &mut state.nodes {
    *node = take_node(&mut packer);
  }
  for chosen in &mut state.chosen {
    let value = packer.take_option(OPTIONAL_VALUE_BITS);
    let term = packer.small(TERM_BITS);
    *chosen = value.map(|value| Chosen { value, term });
  }
  state
}

/// The prefix model at one scope.
struct PrefixModel {
  scope: Scope,
}

impl PrefixModel {
  fn at(scope: Scope) -> PrefixModel {
    PrefixModel {
      scope: scope.checked(),
    }
  }

  fn label(&self) -> String {
    let scope = self.scope;
    format!(
      "prefix model ({:?}), {} nodes, {} indices, {} values, {} terms",
      scope.variant, scope.nodes, scope.indices, scope.values, scope.terms
    )
  }
}

impl Model for PrefixModel {
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
        let log: Vec<String> = held
          .entries()
          .iter()
          .map(|entry| format!("v{}@{}", entry.value, entry.term))
          .collect();
        let window: Vec<String> = held
          .window
          .iter()
          .enumerate()
          .filter_map(|(index, slot)| {
            slot.map(|slot| {
              format!(
                "{index}:v{}@{}{}",
                slot.value,
                slot.term,
                if slot.fast { 'f' } else { 'd' }
              )
            })
          })
          .collect();
        format!(
          "{}:t{}s{}c{} [{}] {{{}}}",
          NAMES[node],
          held.term,
          held.synced,
          held.commit,
          log.join(","),
          window.join(",")
        )
      })
      .collect::<Vec<_>>()
      .join("  ")
  }
}

/// The paths every scope of the design takes (recovery from a log is a rejected variant's, a slot dropped
/// at a sync another's, and a hole needs three indices).
const DESIGN_PATHS: [&str; 8] = [
  "fast commits",
  "classic commits",
  "commits above an uncommitted index",
  "recoveries from a window decision",
  "recoveries of a possible fast choice",
  "logs truncated",
  "entries scattered",
  "slots pruned at a commit",
];

fn scope(nodes: usize, indices: usize, values: u8, terms: u8, variant: Variant) -> Scope {
  Scope {
    nodes,
    indices,
    values,
    terms,
    variant,
  }
}

/// Searches `scope` on every core, printing the report, and returns it with any fault met.
fn run(
  scope: Scope,
) -> (
  exhaustive::Report<PrefixModel>,
  Option<exhaustive::Met<PrefixModel>>,
) {
  let model = PrefixModel::at(scope);
  let (report, met) = exhaustive::explore(&model, MEMORY_CEILING_BYTES, false);
  exhaustive::print_report(&model, &model.label(), State::initial(), &report);
  (report, met)
}

/// Runs the design at `scope`: no fault, and every path it has taken (a hole only with three indices).
fn design_holds(scope: Scope) {
  let model = PrefixModel::at(scope);
  let (report, met) = run(scope);
  if let Some((from, action, fault)) = met {
    eprintln!("{fault:?} met by {action} from {}", model.describe(&from));
    let shortest = exhaustive::search(&model, State::initial(), &|_| true);
    exhaustive::print_report(&model, &model.label(), State::initial(), &shortest);
    panic!("the design failed at {scope:?}: {fault:?}");
  }
  for name in DESIGN_PATHS {
    assert!(
      report.taken(&model, name) > 0,
      "{name} never ran at {scope:?}"
    );
  }
  let holes = report.taken(&model, "holes filled with no-ops");
  assert!(
    scope.indices < 3 || holes > 0,
    "no hole was filled at {scope:?}"
  );
  // A follower's committed prefix is classic, so it holds every later leader's entries, terms and all.
  assert_eq!(
    report.taken(&model, "committed entries kept under an older term"),
    0,
    "a committed entry was kept under an older term at {scope:?}"
  );
}

/// §4, the design, at the scope the default suite affords: three nodes, three indices, one value, two terms
/// — fast and classic commits, both recoveries, holes filled, slots pruned at a commit, logs truncated,
/// entries scattered.
#[test]
fn the_design_keeps_agreement() {
  design_holds(scope(3, 3, 1, 2, Variant::Design));
}

/// §4, the design at full scope as CI's runners hold it (4 GiB): three terms with three nodes and three
/// indices, two values with four nodes, and four terms with two indices. The scopes past that ceiling —
/// three indices with two values (188,172,261 classes) and with four terms (152,906,020, where both of
/// 2026-09-29's rejected rules show) — are searched by hand, the command and measurements in
/// `docs/wip/BENCHMARKS.md`.
#[test]
#[ignore = "exhaustive; CI's full-scale step runs it in release"]
fn the_design_keeps_agreement_at_full_scope() {
  for (nodes, indices, values, terms) in [(3, 3, 1, 3), (4, 2, 2, 3), (3, 2, 2, 4)] {
    design_holds(scope(nodes, indices, values, terms, Variant::Design));
  }
}

/// §4, every alternative the search rejected, each at the smallest scope where it shows, by its shortest
/// history: counting a window's copy toward a commit loses a committed entry, as does dropping a window slot
/// its log covers or dropping a synced node's older slots at the sync, and pruning at a commit index that
/// counts fast commits keeps a committed entry under a term the leader's log does not hold there. Reporting
/// log entries too is safe, but its recovery resurrects a deposed leader's uncommitted entries, which the
/// design never does.
#[test]
#[ignore = "exhaustive; CI's full-scale step runs it in release"]
fn each_rejected_alternative_loses_a_committed_entry_or_resurrects_a_stale_one() {
  for failing in [
    scope(3, 3, 1, 3, Variant::CommitFromWindows),
    scope(3, 3, 1, 3, Variant::DropCovered),
    scope(3, 3, 1, 3, Variant::PruneAtFastCommit),
    scope(3, 3, 1, 4, Variant::DropAtSync),
  ] {
    let model = PrefixModel::at(failing);
    let shortest = exhaustive::search(&model, State::initial(), &|_| true);
    exhaustive::print_report(&model, &model.label(), State::initial(), &shortest);
    let (_, fault) = shortest
      .stopped
      .expect("the alternative loses a committed entry");
    assert!(
      matches!(
        fault,
        Fault::LeaderIncomplete { .. } | Fault::Disagreement { .. } | Fault::LogsDiverge { .. }
      ),
      "{fault:?}"
    );
  }
  let (report, met) = run(scope(3, 3, 1, 3, Variant::ReportLogsToo));
  assert!(met.is_none(), "{met:?}");
  let model = PrefixModel::at(scope(3, 3, 1, 3, Variant::ReportLogsToo));
  assert!(report.taken(&model, "recoveries from a log above the candidate's") > 0);
}

/// Runs one scope named by the environment — the command behind the measurements in the research record:
///
/// `SLATES_PREFIX_SCOPE=nodes,indices,values,terms [SLATES_PREFIX_CEILING_GB=n] [SLATES_PREFIX_LEVELS=1]
/// cargo test --release -p slates-cluster --test prefix_model -- --ignored --exact
/// one_scope_from_the_environment --nocapture`
///
/// Skips, saying so, without `SLATES_PREFIX_SCOPE`.
#[test]
#[ignore = "a measurement tool; runs only with SLATES_PREFIX_SCOPE set"]
fn one_scope_from_the_environment() {
  let Ok(named) = std::env::var("SLATES_PREFIX_SCOPE") else {
    eprintln!("skipping: set SLATES_PREFIX_SCOPE=nodes,indices,values,terms to run one scope");
    return;
  };
  let numbers: Vec<usize> = named.split(',').map(|part| part.parse().unwrap()).collect();
  let [nodes, indices, values, terms] = numbers[..] else {
    panic!("SLATES_PREFIX_SCOPE is nodes,indices,values,terms; got {named}");
  };
  let variant = match std::env::var("SLATES_PREFIX_VARIANT").as_deref() {
    Ok("report-logs-too") => Variant::ReportLogsToo,
    Ok("drop-covered") => Variant::DropCovered,
    Ok("commit-from-windows") => Variant::CommitFromWindows,
    Ok("drop-at-sync") => Variant::DropAtSync,
    Ok("prune-at-fast-commit") => Variant::PruneAtFastCommit,
    _ => Variant::Design,
  };
  let scope = Scope {
    nodes,
    indices,
    values: u8::try_from(values).unwrap(),
    terms: u8::try_from(terms).unwrap(),
    variant,
  };
  let ceiling = std::env::var("SLATES_PREFIX_CEILING_GB")
    .map_or(MEMORY_CEILING_BYTES, |gigabytes| {
      gigabytes.parse::<usize>().unwrap() << 30
    });
  let levels = std::env::var("SLATES_PREFIX_LEVELS").is_ok();
  let model = PrefixModel::at(scope);
  let (report, met) = exhaustive::explore(&model, ceiling, levels);
  exhaustive::print_report(&model, &model.label(), State::initial(), &report);
  if let Some((from, action, fault)) = met {
    eprintln!("{fault:?} met by {action} from {}", model.describe(&from));
    if std::env::var("SLATES_PREFIX_TRACE").is_ok() {
      let shortest = exhaustive::search(&model, State::initial(), &|_| true);
      exhaustive::print_report(&model, &model.label(), State::initial(), &shortest);
    }
    panic!("{fault:?}");
  }
}
