//! A-636's tests: a repository's logs over the fenced ledger, each log replicated on its own holders, every record
//! committed whole and applied at its joins, a takeover adopting each log and each record whole or not at all, and a
//! replica reading by a vector of positions, under crashes, partitions and takeovers (A-642's robustness rules).

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};

use super::{AppendRefused, Answer, Cut, LogRecord, LogsCohort, LogsOwner, ReadRefused};
use crate::catalog::{LogName, LogSpan, RepositoryId, StateLog, VolumeId, spans_split_by};
use crate::ledger::{Reach, TakeoverError};
use crate::register::{HostId, ObjectId, Quorum, candidates_for};

/// Shape: the host that owns every test's repository.
const OWNER: HostId = HostId(1);
/// Shape: the repository whose state logs the tests keep.
const REPOSITORY: RepositoryId = RepositoryId { bytes: [0x2a; 16] };
/// Shape: the first object counter a state log's id takes, past every green's a test names.
const STATE_OBJECTS: u64 = 0x100;
/// Shape: the greens a generated history keeps, enough that its records land on both of the owner's copysets.
const GREENS: u64 = 3;
/// Shape: the greens probed for one the owner's other copyset holds; rendezvous misses it in all of them with
/// probability 2⁻⁶³.
const PROBES: u64 = 64;
/// Shape: the superseded owners a generated history keeps proposing through.
const STALE_KEPT: usize = 4;
/// Shape: the steps of one generated history.
const STEPS: usize = 64;

/// The hosts of a repository at `f`: the owner and two copysets of `2f` co-holders each, so logs placed by their own
/// object ids land apart, and never fewer than three, so some host holds no copy of a log.
fn hosts(f: u32) -> Vec<HostId> {
  // Shape: two copysets of `2f` co-holders each beside the owner; three hosts at the least.
  let count = u64::from(f).saturating_mul(4).saturating_add(1).max(3);
  (1..=count).map(HostId).collect()
}

/// The object id of green `index`.
fn green_object(index: u64) -> ObjectId {
  ObjectId::new(OWNER, index.saturating_add(1))
}

/// The log of green `index`.
fn green(index: u64) -> LogName {
  LogName::Green {
    green: VolumeId {
      bytes: green_object(index).0,
    },
  }
}

/// The repository's state log `log`.
fn state(log: StateLog) -> LogName {
  LogName::State {
    repository: REPOSITORY,
    log,
  }
}

/// The object id of state log `log`, past every green's.
fn state_object(log: StateLog) -> ObjectId {
  // Format: each state log's offset past the first state object.
  let offset = match log {
    StateLog::Names => 0,
    StateLog::Phases => 1,
    StateLog::Markers => 2,
    StateLog::Locks => 3,
  };
  ObjectId::new(OWNER, STATE_OBJECTS.saturating_add(offset))
}

/// The record numbered `number`'s identity.
fn identity(number: u64) -> [u8; 32] {
  let bytes = number.to_be_bytes();
  std::array::from_fn(|index| bytes.get(index).copied().unwrap_or(0))
}

/// A record numbered `number` over `spans`, each `(log, from, to)`.
fn record(number: u64, spans: &[(LogName, u64, u64)]) -> LogRecord {
  LogRecord {
    identity: identity(number),
    spans: spans
      .iter()
      .map(|(log, from, to)| LogSpan {
        log: *log,
        from: *from,
        to: *to,
      })
      .collect(),
  }
}

/// A reach of exactly `hosts`.
fn only(hosts: &[HostId]) -> Reach {
  Reach::Only(hosts.iter().copied().collect())
}

/// A repository at `f = 1` whose two greens keep their logs on the owner's two copysets, beside its names log.
struct Apart {
  /// The holders of every log.
  cohort: LogsCohort,
  /// The first green's log.
  a: LogName,
  /// The second green's log, on the other copyset.
  b: LogName,
  /// The names log.
  names: LogName,
}

impl Apart {
  /// The repository, its second green the first rendezvous places on the copyset the first green's is not on.
  fn new() -> Apart {
    let hosts = hosts(1);
    let place = |object| candidates_for(OWNER, &hosts, &BTreeMap::new(), object, Quorum { f: 1 });
    let first = place(green_object(0));
    let other = (1..PROBES)
      .find(|index| place(green_object(*index)) != first)
      .expect("a green on the other copyset");
    let (a, b, names) = (green(0), green(other), state(StateLog::Names));
    let logs = [
      (a, green_object(0)),
      (b, green_object(other)),
      (names, state_object(StateLog::Names)),
    ];
    let cohort = LogsCohort::new(OWNER, &hosts, &BTreeMap::new(), &logs, Quorum { f: 1 });
    Apart {
      cohort,
      a,
      b,
      names,
    }
  }

  /// The holders of `log`, owner first.
  fn holders(&self, log: &LogName) -> Vec<HostId> {
    self.cohort.candidates(log).to_vec()
  }

  /// The owner and `log`'s co-holders: every other copyset out of reach.
  fn only(&self, log: &LogName) -> Reach {
    only(&self.holders(log))
  }

  /// The holders of `log` beside the owner.
  fn co_holders(&self, log: &LogName) -> Vec<HostId> {
    self
      .holders(log)
      .into_iter()
      .filter(|host| *host != OWNER)
      .collect()
  }

  /// Every host but the owner: the owner crashed.
  fn owner_gone(&self) -> Reach {
    let mut hosts = self.co_holders(&self.a);
    hosts.extend(self.co_holders(&self.b));
    only(&hosts)
  }
}

/// A cut of `positions`, each `(log, position)`.
fn cut(positions: &[(LogName, u64)]) -> Cut {
  positions.iter().copied().collect()
}

/// A-636 and A-642 (each green's log replicated on its own at `f + 1`): do append a push to green B while B's
/// co-holders are out of reach, then two pushes to green A, whose holders are on the other copyset; then offer every
/// log again with every host back. Expect B's push to wait on B's holders alone and A's two to commit at once, B's
/// pending push holding up neither (only a green's own later pushes wait); and then every push committed, the
/// owner's cut equal to the oracle's.
#[test]
fn each_log_commits_on_its_own_holders_and_a_push_waits_on_no_other_log() {
  let mut world = Apart::new();
  let (a, b) = (world.a, world.b);
  let mut owner = LogsOwner::bootstrap(&world.cohort);
  let reach = world.only(&a);
  let waits = owner
    .append(&mut world.cohort, record(1, &[(b, 0, 1)]), &reach)
    .unwrap();
  let first = owner
    .append(&mut world.cohort, record(2, &[(a, 0, 1)]), &reach)
    .unwrap();
  let second = owner
    .append(&mut world.cohort, record(3, &[(a, 1, 2)]), &reach)
    .unwrap();
  assert_eq!(
    (waits.whole, first.whole, second.whole),
    (false, true, true),
    "B's push waits on B's holders; A's commit on A's at once"
  );
  let announced = owner.announce(&mut world.cohort, &Reach::All);
  let expected = cut(&[(a, 2), (b, 1), (world.names, 0)]);
  assert_eq!(announced, expected, "every push committed once B's holders are back");
  assert_eq!(world.cohort.committed_cut(), expected, "the owner's cut is the oracle's");
}

/// A-636 and A-642 (an atomic record committed whole, applied at its join on every holder): do append a record over
/// greens A and B while B's co-holders are out of reach, then a push to A after it, offering every log again each
/// time; then offer every log again with every host back. Expect no holder to apply anything while B lacks the record
/// (A's next push waits behind the record it follows); then the record and the push after it applied by every holder
/// of A, the record by every holder of B, and both logs at once on the owner, which holds both.
#[test]
fn an_atomic_record_commits_whole_and_every_holder_applies_it_at_its_join() {
  let mut world = Apart::new();
  let (a, b) = (world.a, world.b);
  let mut owner = LogsOwner::bootstrap(&world.cohort);
  let reach = world.only(&a);
  let atomic = owner
    .append(&mut world.cohort, record(1, &[(a, 0, 1), (b, 0, 1)]), &reach)
    .unwrap();
  owner.announce(&mut world.cohort, &reach);
  let after = owner
    .append(&mut world.cohort, record(2, &[(a, 1, 2)]), &reach)
    .unwrap();
  owner.announce(&mut world.cohort, &reach);
  assert_eq!((atomic.whole, after.whole), (false, false), "B lacks the record");
  assert_nothing_applied(&world);
  owner.announce(&mut world.cohort, &Reach::All);
  assert_applied_by_every_holder(&world, a, 2);
  assert_applied_by_every_holder(&world, b, 1);
  assert_eq!(
    world.cohort.applied(OWNER),
    cut(&[(a, 2), (b, 1)]),
    "the owner, holding both logs, applied the record on both"
  );
}

/// No holder of A or B applied any position yet.
fn assert_nothing_applied(world: &Apart) {
  for host in world.holders(&world.a).into_iter().chain(world.holders(&world.b)) {
    let applied = world.cohort.applied(host);
    assert!(
      applied.values().all(|position| *position == 0),
      "{host:?} applied {applied:?} before the record was whole"
    );
  }
}

/// Every holder of `log` applied it to `position`.
fn assert_applied_by_every_holder(world: &Apart, log: LogName, position: u64) {
  for host in world.holders(&log) {
    assert_eq!(
      world.cohort.applied(host).get(&log),
      Some(&position),
      "{host:?} applied {log:?} at the record's join"
    );
  }
}

/// A repository whose record over greens A and B, then a push to A, are committed and applied everywhere.
fn applied_everywhere() -> (Apart, LogsOwner) {
  let mut world = Apart::new();
  let (a, b) = (world.a, world.b);
  let mut owner = LogsOwner::bootstrap(&world.cohort);
  for spans in [vec![(a, 0, 1), (b, 0, 1)], vec![(a, 1, 2)]] {
    let number = u64::try_from(owner.log(&a).len()).unwrap().saturating_add(1);
    let appended = owner
      .append(&mut world.cohort, record(number, &spans), &Reach::All)
      .unwrap();
    assert!(appended.whole, "committed with every host in reach");
  }
  owner.announce(&mut world.cohort, &Reach::All);
  (world, owner)
}

/// A-636 and A-642 (replica reads by a vector of positions): do commit and apply a record over greens A and B and a
/// push to A after it; read at the owner by vectors over both logs and past what it applied; read at one of A's
/// co-holders, which holds no copy of B. Expect a vector holding the record on both logs answered with it on both; one
/// holding it on A and not on B refused `Split`, naming it; one past what was applied refused `NotApplied`; B refused
/// `NotHeld` at A's co-holder, while A alone is answered there.
#[test]
fn a_replica_reads_a_vector_of_positions_and_refuses_one_that_splits_a_record() {
  let (world, _) = applied_everywhere();
  let (a, b) = (world.a, world.b);
  let both = world.cohort.read(OWNER, &cut(&[(a, 1), (b, 1)]));
  let expected: Answer = [(a, vec![identity(1)]), (b, vec![identity(1)])].into();
  assert_eq!(both, Ok(expected), "the record read whole on both logs");
  assert_eq!(
    world.cohort.read(OWNER, &cut(&[(a, 1), (b, 0)])),
    Err(ReadRefused::Split {
      identity: identity(1)
    }),
    "a vector holding the record on A and not on B"
  );
  assert_eq!(
    world.cohort.read(OWNER, &cut(&[(a, 3)])),
    Err(ReadRefused::NotApplied {
      log: a,
      applied: 2,
      asked: 3
    }),
    "past what the owner applied"
  );
  assert_reads_at_a_holder_of_one_log(&world);
}

/// One of A's co-holders, no candidate of B's, answers A alone and refuses B.
fn assert_reads_at_a_holder_of_one_log(world: &Apart) {
  let (a, b) = (world.a, world.b);
  let host = world.co_holders(&a).first().copied().unwrap();
  assert!(!world.holders(&b).contains(&host), "the copysets differ");
  assert_eq!(
    world.cohort.read(host, &cut(&[(b, 1)])),
    Err(ReadRefused::NotHeld { log: b }),
    "B at a host holding no copy of it"
  );
  let answer = world.cohort.read(host, &cut(&[(a, 2)])).unwrap();
  assert_eq!(
    answer.get(&a),
    Some(&vec![identity(1), identity(2)]),
    "A alone at its co-holder"
  );
}

/// A-636 and A-642 (a takeover adopts each log, and an atomic record whole or not at all): do commit a push to green A;
/// append a record over A and B while B's co-holders are out of reach, so only the owner's own copy of B holds it,
/// then a push to A after it; take the repository over from A's co-holder with the owner crashed; append and offer
/// again as the new owner. Expect the record dropped whole, with the push after it on A, since B's read never saw it;
/// the committed push kept (NoLoss); the new owner's push to A taking the dropped record's position; and the dropped
/// record in no committed log and no holder's answer.
#[test]
fn a_takeover_drops_a_record_no_read_holds_whole_and_what_follows_it() {
  let mut world = Apart::new();
  let (a, b) = (world.a, world.b);
  let mut first = LogsOwner::bootstrap(&world.cohort);
  first
    .append(&mut world.cohort, record(1, &[(a, 0, 1)]), &Reach::All)
    .unwrap();
  let reach = world.only(&a);
  first
    .append(&mut world.cohort, record(2, &[(a, 1, 2), (b, 0, 1)]), &reach)
    .unwrap();
  first
    .append(&mut world.cohort, record(3, &[(a, 2, 3)]), &reach)
    .unwrap();
  let successor = world.co_holders(&a).first().copied().unwrap();
  let gone = world.owner_gone();
  let mut taken = LogsOwner::take_over(&mut world.cohort, successor, &gone).unwrap();
  assert_eq!(taken.dropped, vec![identity(2)], "dropped whole, once");
  assert_eq!(
    (taken.owner.log(&a), taken.owner.log(&b)),
    (&[identity(1)][..], &[][..]),
    "the committed push kept, the record and what followed it gone"
  );
  let next = taken
    .owner
    .append(&mut world.cohort, record(4, &[(a, 1, 2)]), &gone)
    .unwrap();
  taken.owner.announce(&mut world.cohort, &gone);
  assert!(next.whole, "the new owner's push takes the dropped record's position");
  let history = world.cohort.committed_history();
  assert_eq!(history.get(&a), Some(&vec![identity(1), identity(4)]), "NoLoss on A");
  assert_eq!(history.get(&b), Some(&vec![]), "nothing of the record on B");
}

/// A-636 and A-642 (a takeover keeps a record one read holds whole, and finishes it): do append a record over greens A
/// and B while B's co-holders are out of reach, so A commits it and B holds it on the owner alone; take the repository
/// over from B's co-holder with every host in reach; offer every log again as the new owner. Expect the record kept,
/// since every log's read holds it; uncommitted before the takeover and committed whole after, on both logs; and the
/// superseded owner's next record refused by the fences on every log (StaleNeverCommits).
#[test]
fn a_takeover_keeps_a_record_every_read_holds_and_finishes_it() {
  let mut world = Apart::new();
  let (a, b) = (world.a, world.b);
  let mut first = LogsOwner::bootstrap(&world.cohort);
  let reach = world.only(&a);
  first
    .append(&mut world.cohort, record(1, &[(a, 0, 1), (b, 0, 1)]), &reach)
    .unwrap();
  assert_eq!(world.cohort.committed_cut().get(&b), Some(&0), "B has not committed it");
  let successor = world.co_holders(&b).first().copied().unwrap();
  let mut taken = LogsOwner::take_over(&mut world.cohort, successor, &Reach::All).unwrap();
  assert_eq!(taken.dropped, Vec::<[u8; 32]>::new(), "kept whole");
  taken.owner.announce(&mut world.cohort, &Reach::All);
  assert_eq!(
    world.cohort.committed_cut(),
    cut(&[(a, 1), (b, 1), (world.names, 0)]),
    "committed whole, on both logs"
  );
  let stale = first
    .append(&mut world.cohort, record(2, &[(a, 1, 2), (b, 1, 2)]), &Reach::All)
    .unwrap();
  assert!(!stale.whole, "the superseded owner commits nothing");
}

/// A-636 (StaleNeverCommits across logs): do commit a record over greens A and B; take the repository over with every
/// host in reach; have the superseded owner append a record over both and a push to the names log, every host in
/// reach; then have the new owner append at the same positions. Expect none of the superseded owner's records whole and
/// none in any committed log, and the new owner's committed at those positions.
#[test]
fn a_superseded_owner_commits_nothing_on_any_log() {
  let mut world = Apart::new();
  let (a, b, names) = (world.a, world.b, world.names);
  let mut first = LogsOwner::bootstrap(&world.cohort);
  first
    .append(&mut world.cohort, record(1, &[(a, 0, 1), (b, 0, 1)]), &Reach::All)
    .unwrap();
  let successor = world.co_holders(&a).first().copied().unwrap();
  let taken = LogsOwner::take_over(&mut world.cohort, successor, &Reach::All).unwrap();
  let stale = [
    record(2, &[(a, 1, 2), (b, 1, 2)]),
    record(3, &[(names, 0, 2)]),
  ]
  .map(|stale| first.append(&mut world.cohort, stale, &Reach::All).unwrap());
  assert!(stale.iter().all(|appended| !appended.whole), "nothing whole");
  let mut owner = taken.owner;
  let fresh = owner
    .append(&mut world.cohort, record(4, &[(a, 1, 2), (b, 1, 2)]), &Reach::All)
    .unwrap();
  assert!(fresh.whole, "the new owner commits at the same positions");
  let history = world.cohort.committed_history();
  let committed: BTreeSet<[u8; 32]> = history.values().flatten().copied().collect();
  assert_eq!(committed, [identity(1), identity(4)].into(), "no superseded record committed");
}

/// A-636 (a takeover that cannot fence every log fences none): do commit a push to green A; take the repository over
/// with the owner and B's co-holders out of reach, so A's round could run and B's could not; then append to A as the
/// first owner. Expect the takeover refused, naming B and its quorum, and the first owner's push to A committed:
/// nothing was fenced.
#[test]
fn a_takeover_that_cannot_fence_every_log_fences_none() {
  let mut world = Apart::new();
  let (a, b) = (world.a, world.b);
  let mut first = LogsOwner::bootstrap(&world.cohort);
  first
    .append(&mut world.cohort, record(1, &[(a, 0, 1)]), &Reach::All)
    .unwrap();
  let successor = world.co_holders(&a).first().copied().unwrap();
  let refused = LogsOwner::take_over(&mut world.cohort, successor, &only(&world.co_holders(&a)));
  let refusal = refused.map(|_| ()).unwrap_err();
  assert_eq!(
    (refusal.log, refusal.error),
    (
      b,
      TakeoverError::NoQuorum {
        reachable: 0,
        needed: 2
      }
    ),
    "refused at B, whose holders are out of reach"
  );
  let next = first
    .append(&mut world.cohort, record(2, &[(a, 1, 2)]), &Reach::All)
    .unwrap();
  assert!(next.whole, "A was not fenced");
}

/// A-636 and A-642 (an append reads each log's position, or is refused whole): do commit a push to green A; append a
/// record that read A at its old position, one that moves nothing, one over a log the repository does not keep, one
/// whose span moves no position, one naming A twice, and one reusing a committed record's identity. Expect each
/// refused, typed (`Moved` naming A, the position read and the one it is at), and nothing proposed: A's position and
/// the committed cut unchanged.
#[test]
fn an_append_reads_each_logs_position_or_is_refused_whole() {
  let mut world = Apart::new();
  let a = world.a;
  let mut owner = LogsOwner::bootstrap(&world.cohort);
  owner
    .append(&mut world.cohort, record(1, &[(a, 0, 1)]), &Reach::All)
    .unwrap();
  let before = world.cohort.committed_cut();
  let refused = refusals(a).map(|(refused, _)| {
    owner
      .append(&mut world.cohort, refused, &Reach::All)
      .unwrap_err()
  });
  let expected = refusals(a).map(|(_, refusal)| refusal);
  assert_eq!(refused, expected, "each refused, typed");
  assert_eq!(owner.position(&a), Some(1), "nothing proposed on A");
  assert_eq!(world.cohort.committed_cut(), before, "nothing committed");
}

/// The refused records of [`an_append_reads_each_logs_position_or_is_refused_whole`], each with its refusal.
fn refusals(a: LogName) -> [(LogRecord, AppendRefused); 6] {
  let unknown = green(PROBES.saturating_add(1));
  [
    (
      record(2, &[(a, 0, 1)]),
      AppendRefused::Moved {
        log: a,
        read: 0,
        at: 1,
      },
    ),
    (record(3, &[]), AppendRefused::Empty),
    (
      record(4, &[(unknown, 0, 1)]),
      AppendRefused::UnknownLog { log: unknown },
    ),
    (record(5, &[(a, 1, 1)]), AppendRefused::Malformed { log: a }),
    (
      record(6, &[(a, 1, 2), (a, 2, 3)]),
      AppendRefused::Malformed { log: a },
    ),
    (record(1, &[(a, 1, 2)]), AppendRefused::IdentityTaken),
  ]
}

/// What a generated history met, by kind.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Census {
  /// Records of one log that entered the committed history.
  single_committed: u32,
  /// Records of several logs that entered the committed history whole.
  atomic_committed: u32,
  /// Records of several logs an append left not whole: some log's holders out of reach.
  atomic_waited: u32,
  /// Records of one log committed at once while a record over other logs waited.
  interleaved: u32,
  /// Appends refused `Moved`.
  moved: u32,
  /// Records a superseded owner appended, none committed.
  stale: u32,
  /// Takeovers that ran.
  takeovers: u32,
  /// Takeovers refused, nothing fenced.
  takeovers_refused: u32,
  /// Records a takeover dropped whole.
  dropped_whole: u32,
  /// Records a takeover kept whole that had not committed: finished by its offers.
  kept_unfinished: u32,
  /// Records of several logs a holder applied at its join.
  applied_at_join: u32,
  /// Replica reads answered.
  reads_served: u32,
  /// Replica reads refused `Split`.
  reads_split: u32,
  /// Replica reads refused `NotApplied`.
  reads_not_applied: u32,
  /// Replica reads refused `NotHeld`.
  reads_not_held: u32,
}

impl Census {
  /// Every count, in field order.
  fn met(&self) -> [u32; 15] {
    [
      self.single_committed,
      self.atomic_committed,
      self.atomic_waited,
      self.interleaved,
      self.moved,
      self.stale,
      self.takeovers,
      self.takeovers_refused,
      self.dropped_whole,
      self.kept_unfinished,
      self.applied_at_join,
      self.reads_served,
      self.reads_split,
      self.reads_not_applied,
      self.reads_not_held,
    ]
  }

  /// Adds `other`'s counts to these.
  fn add(&mut self, other: &Census) {
    let sum = |mine: &mut u32, theirs: u32| *mine = mine.saturating_add(theirs);
    sum(&mut self.single_committed, other.single_committed);
    sum(&mut self.atomic_committed, other.atomic_committed);
    sum(&mut self.atomic_waited, other.atomic_waited);
    sum(&mut self.interleaved, other.interleaved);
    sum(&mut self.moved, other.moved);
    sum(&mut self.stale, other.stale);
    sum(&mut self.takeovers, other.takeovers);
    sum(&mut self.takeovers_refused, other.takeovers_refused);
    sum(&mut self.dropped_whole, other.dropped_whole);
    sum(&mut self.kept_unfinished, other.kept_unfinished);
    sum(&mut self.applied_at_join, other.applied_at_join);
    sum(&mut self.reads_served, other.reads_served);
    sum(&mut self.reads_split, other.reads_split);
    sum(&mut self.reads_not_applied, other.reads_not_applied);
    sum(&mut self.reads_not_held, other.reads_not_held);
  }
}

/// One step of a generated history.
#[derive(Clone, Debug)]
enum Step {
  /// The owner appends a record over the logs `logs` picks ([`chosen`]), a state log's span two positions where
  /// `long`'s bit for it is set, read at the owner's positions, or one past on its first log when `misread`.
  Append {
    /// The logs.
    logs: u8,
    /// The spans' lengths.
    long: u8,
    /// Whether the push read its first log one past.
    misread: bool,
  },
  /// The superseded owner `which` picks appends over the logs `logs` picks, at its own positions.
  Stale {
    /// The owner, counted modulo those kept.
    which: u8,
    /// The logs.
    logs: u8,
  },
  /// Only the hosts `mask`'s bits name are in reach.
  Partition {
    /// The hosts, bit `n` for the host `n + 1`.
    mask: u16,
  },
  /// Every host is in reach.
  Heal,
  /// The owner offers every log again; the holders apply what is committed.
  Announce,
  /// The host `host` picks takes the repository over.
  TakeOver {
    /// The host, counted modulo the hosts.
    host: u8,
  },
  /// The host `host` picks reads a vector over the logs `logs` picks: each at what it applied, one before where
  /// `back`'s bit for it is set, or one past every one when `ahead`.
  Read {
    /// The host, counted modulo the hosts.
    host: u8,
    /// The logs.
    logs: u8,
    /// The logs read one before what was applied.
    back: u8,
    /// Whether the read asks one past what was applied.
    ahead: bool,
  },
}

/// A step, weighted towards appends, offers and reads.
fn step() -> impl Strategy<Value = Step> {
  // Shape: twenty-four kinds, weighted by the arms below.
  let kinds = 0..24u8;
  (kinds, any::<u8>(), any::<u8>(), any::<u8>(), any::<u16>(), any::<bool>()).prop_map(
    |(kind, first, second, third, mask, flag)| match kind {
      // Shape: appends, eight in twenty-four, one in eight misread.
      0..=7 => Step::Append {
        logs: first,
        long: second,
        misread: flag && second.is_multiple_of(4),
      },
      // Shape: a superseded owner's append, then partitions, a heal.
      8 => Step::Stale {
        which: second,
        logs: first,
      },
      9..=11 => Step::Partition { mask },
      // Shape: a heal, offers, takeovers; reads take the rest, one in sixteen past what was applied.
      12 => Step::Heal,
      13..=16 => Step::Announce,
      17..=18 => Step::TakeOver { host: second },
      _ => Step::Read {
        host: second,
        logs: first,
        back: third,
        ahead: flag && third.is_multiple_of(8),
      },
    },
  )
}

/// The logs a step picks from `logs`: one log when its top bit is clear (a push to one green or one state log), else
/// every log its low bits name and at least two (an atomic push).
fn chosen(logs: u8, all: &[LogName]) -> Vec<LogName> {
  let count = all.len().max(1);
  let first = usize::from(logs) % count;
  // Format: the top bit picks several logs, the low seven name them, the high nibble a second.
  let single = logs & 0x80 == 0;
  let named =
    |index: &usize| u32::try_from(*index).is_ok_and(|bit| bit < 7 && logs & (1u8 << bit) != 0);
  let second = (first + 1 + usize::from(logs >> 4) % (count - 1).max(1)) % count;
  let mut picked: Vec<usize> = (0..count).filter(|index| !single && named(index)).collect();
  picked.push(first);
  if !single {
    picked.push(second);
  }
  picked.sort_unstable();
  picked.dedup();
  picked.into_iter().filter_map(|index| all.get(index).copied()).collect()
}

/// One generated history's world: the repository's holders, its owner and the owners it superseded, the reach, and
/// what the checks saw so far.
struct World {
  /// The hosts.
  hosts: Vec<HostId>,
  /// Every log, greens first.
  logs: Vec<LogName>,
  /// The holders of every log.
  cohort: LogsCohort,
  /// The owner.
  owner: LogsOwner,
  /// The owners superseded, newest last.
  stale: Vec<LogsOwner>,
  /// Which hosts a message reaches.
  reach: Reach,
  /// The next record's number.
  next: u64,
  /// Each appended record's spans.
  records: BTreeMap<[u8; 32], Vec<LogSpan>>,
  /// The committed history the checks saw last.
  history: Answer,
  /// What each holder had applied when the checks last looked.
  applied: BTreeMap<HostId, Cut>,
  /// Each holder's records of several logs applied.
  joined: BTreeSet<(HostId, [u8; 32])>,
  /// What the history met.
  census: Census,
}

impl World {
  /// A repository at `f` over its hosts, with three greens and the four state logs.
  fn new(f: u32) -> World {
    let hosts = hosts(f);
    let states = [
      StateLog::Names,
      StateLog::Phases,
      StateLog::Markers,
      StateLog::Locks,
    ];
    let mut placed: Vec<(LogName, ObjectId)> = (0..GREENS)
      .map(|index| (green(index), green_object(index)))
      .collect();
    placed.extend(states.map(|log| (state(log), state_object(log))));
    let cohort = LogsCohort::new(OWNER, &hosts, &BTreeMap::new(), &placed, Quorum { f });
    World {
      logs: placed.iter().map(|(log, _)| *log).collect(),
      owner: LogsOwner::bootstrap(&cohort),
      hosts,
      cohort,
      stale: Vec::new(),
      reach: Reach::All,
      next: 1,
      records: BTreeMap::new(),
      history: Answer::new(),
      applied: BTreeMap::new(),
      joined: BTreeSet::new(),
      census: Census::default(),
    }
  }

  /// Runs `step`, then the checks.
  fn step(&mut self, step: &Step) -> Result<(), TestCaseError> {
    match step {
      Step::Append {
        logs,
        long,
        misread,
      } => self.append(*logs, *long, *misread)?,
      Step::Stale { which, logs } => self.stale(*which, *logs)?,
      Step::Partition { mask } => self.partition(*mask),
      Step::Heal => self.reach = Reach::All,
      Step::Announce => {
        self.owner.announce(&mut self.cohort, &self.reach);
      }
      Step::TakeOver { host } => self.take_over(*host)?,
      Step::Read {
        host,
        logs,
        back,
        ahead,
      } => self.read(*host, *logs, *back, *ahead)?,
    }
    self.check()
  }

  /// A fresh record over `logs` at `owner`'s positions, a state log's span two positions where `long`'s bit is set.
  fn record(&mut self, owner: &LogsOwner, logs: &[LogName], long: u8) -> LogRecord {
    let spans = logs
      .iter()
      .enumerate()
      .map(|(index, log)| {
        let from = owner.position(log).unwrap_or(0);
        // Shape: a state log's span is one position or two; a green's, one.
        let of_state = matches!(log, LogName::State { .. });
        let two = of_state && u32::try_from(index).is_ok_and(|bit| long & (1u8 << (bit % 8)) != 0);
        LogSpan {
          log: *log,
          from,
          to: from.saturating_add(if two { 2 } else { 1 }),
        }
      })
      .collect();
    let number = self.next;
    self.next = self.next.saturating_add(1);
    LogRecord {
      identity: identity(number),
      spans,
    }
  }

  /// The owner appends a record over the logs `logs` picks.
  fn append(&mut self, logs: u8, long: u8, misread: bool) -> Result<(), TestCaseError> {
    let logs = chosen(logs, &self.logs);
    let owner = self.owner.clone();
    let mut record = self.record(&owner, &logs, long);
    if misread && let Some(first) = record.spans.first_mut() {
      first.from = first.from.saturating_add(1);
      first.to = first.to.saturating_add(1);
    }
    let pending_elsewhere = self.waiting_beside(&logs);
    let spans = record.spans.clone();
    match self.owner.append(&mut self.cohort, record.clone(), &self.reach) {
      Ok(appended) => {
        self.records.insert(record.identity, spans);
        self.count_append(logs.len(), appended.whole, pending_elsewhere);
        Ok(())
      }
      Err(AppendRefused::Moved { .. }) if misread => {
        self.census.moved = self.census.moved.saturating_add(1);
        Ok(())
      }
      Err(refusal) => Err(TestCaseError::fail(format!("refused {refusal:?}"))),
    }
  }

  /// Counts an append over `logs` logs that left its record `whole` or not, with a record over other logs waiting.
  fn count_append(&mut self, logs: usize, whole: bool, waiting_beside: bool) {
    let census = &mut self.census;
    if logs > 1 && !whole {
      census.atomic_waited = census.atomic_waited.saturating_add(1);
    }
    if logs == 1 && whole && waiting_beside {
      census.interleaved = census.interleaved.saturating_add(1);
    }
  }

  /// Whether a record over several logs, none of them `logs`, was appended and has not committed.
  fn waiting_beside(&self, logs: &[LogName]) -> bool {
    let committed: BTreeSet<&[u8; 32]> = self.history.values().flatten().collect();
    self.records.iter().any(|(identity, spans)| {
      spans.len() > 1
        && !committed.contains(identity)
        && spans.iter().all(|span| !logs.contains(&span.log))
        && self.owner.log(&spans[0].log).contains(identity)
    })
  }

  /// A superseded owner appends over the logs `logs` picks, every host in this step's reach: never whole, never
  /// committed (StaleNeverCommits).
  fn stale(&mut self, which: u8, logs: u8) -> Result<(), TestCaseError> {
    let Some(index) = usize::from(which).checked_rem(self.stale.len()) else {
      return Ok(());
    };
    let logs = chosen(logs, &self.logs);
    let owner = self.stale[index].clone();
    let record = self.record(&owner, &logs, 0);
    let identity = record.identity;
    self.records.insert(identity, record.spans.clone());
    let appended = self.stale[index]
      .append(&mut self.cohort, record, &self.reach)
      .map_err(|refusal| TestCaseError::fail(format!("stale refused {refusal:?}")))?;
    prop_assert!(!appended.whole, "a superseded owner's record was whole");
    let committed = self.cohort.committed_history();
    prop_assert!(
      committed.values().flatten().all(|held| *held != identity),
      "a superseded owner's record committed"
    );
    self.census.stale = self.census.stale.saturating_add(1);
    Ok(())
  }

  /// Only the hosts `mask` names are in reach.
  fn partition(&mut self, mask: u16) {
    let hosts = self.hosts.iter().enumerate().filter(|(index, _)| {
      u32::try_from(*index).is_ok_and(|bit| bit < u16::BITS && mask & (1u16 << bit) != 0)
    });
    self.reach = Reach::Only(hosts.map(|(_, host)| *host).collect());
  }

  /// The host `host` picks takes the repository over in this step's reach.
  fn take_over(&mut self, host: u8) -> Result<(), TestCaseError> {
    let new_id = self.hosts[usize::from(host) % self.hosts.len()];
    let committed: BTreeSet<[u8; 32]> = self.history.values().flatten().copied().collect();
    let Ok(taken) = LogsOwner::take_over(&mut self.cohort, new_id, &self.reach) else {
      self.census.takeovers_refused = self.census.takeovers_refused.saturating_add(1);
      return Ok(());
    };
    for dropped in &taken.dropped {
      prop_assert!(!committed.contains(dropped), "a committed record dropped");
    }
    let kept: BTreeSet<[u8; 32]> = self
      .logs
      .iter()
      .flat_map(|log| taken.owner.log(log).iter().copied())
      .filter(|identity| !committed.contains(identity))
      .collect();
    let census = &mut self.census;
    census.takeovers = census.takeovers.saturating_add(1);
    census.dropped_whole = census.dropped_whole.saturating_add(count(taken.dropped.len()));
    census.kept_unfinished = census.kept_unfinished.saturating_add(count(kept.len()));
    let superseded = std::mem::replace(&mut self.owner, taken.owner);
    self.stale.push(superseded);
    if self.stale.len() > STALE_KEPT {
      self.stale.remove(0);
    }
    Ok(())
  }

  /// The host `host` picks reads a vector over the logs `logs` picks.
  fn read(&mut self, host: u8, logs: u8, back: u8, ahead: bool) -> Result<(), TestCaseError> {
    let host = self.hosts[usize::from(host) % self.hosts.len()];
    let applied = self.cohort.applied(host);
    let vector: Cut = chosen(logs, &self.logs)
      .into_iter()
      .enumerate()
      .map(|(index, log)| {
        let at = applied.get(&log).copied().unwrap_or(0);
        let one_back = u32::try_from(index).is_ok_and(|bit| back & (1u8 << (bit % 8)) != 0);
        let position = match (ahead, one_back) {
          (true, _) => at.saturating_add(1),
          (false, true) => at.saturating_sub(1),
          (false, false) => at,
        };
        (log, position)
      })
      .collect();
    let answer = self.cohort.read(host, &vector);
    self.judge_read(host, &vector, &applied, answer)
  }

  /// Holds a replica's answer to the rules: an answer is the committed history and splits no record; each refusal
  /// names what is so.
  fn judge_read(
    &mut self,
    host: HostId,
    vector: &Cut,
    applied: &Cut,
    answer: Result<Answer, ReadRefused>,
  ) -> Result<(), TestCaseError> {
    let census = &mut self.census;
    match answer {
      Ok(answer) => {
        census.reads_served = census.reads_served.saturating_add(1);
        self.judge_answer(vector, &answer)
      }
      Err(ReadRefused::Split { identity }) => {
        census.reads_split = census.reads_split.saturating_add(1);
        let spans = self.records.get(&identity).cloned().unwrap_or_default();
        prop_assert!(spans_split_by(&spans, |log| vector.get(log).copied()), "split, wrongly");
        Ok(())
      }
      Err(ReadRefused::NotApplied { log, asked, .. }) => {
        census.reads_not_applied = census.reads_not_applied.saturating_add(1);
        prop_assert!(asked > applied.get(&log).copied().unwrap_or(0), "not applied, wrongly");
        Ok(())
      }
      Err(ReadRefused::NotHeld { log }) => {
        census.reads_not_held = census.reads_not_held.saturating_add(1);
        prop_assert!(!self.cohort.candidates(&log).contains(&host), "not held, wrongly");
        Ok(())
      }
    }
  }

  /// A replica's answer is the committed history at the vector, and the vector splits no record it holds.
  fn judge_answer(&self, vector: &Cut, answer: &Answer) -> Result<(), TestCaseError> {
    for (log, identities) in answer {
      let history = self.history.get(log).map_or(&[][..], Vec::as_slice);
      prop_assert!(history.starts_with(identities), "a replica answered past the history");
    }
    let at = |log: &LogName| vector.get(log).copied();
    for identity in answer.values().flatten() {
      let spans = self.records.get(identity).cloned().unwrap_or_default();
      prop_assert!(!spans_split_by(&spans, at), "a replica answered a split cut");
    }
    Ok(())
  }

  /// The checks after every step: Agreement, NoLoss, and every holder's applied cut whole, committed and monotone.
  fn check(&mut self) -> Result<(), TestCaseError> {
    prop_assert_eq!(self.cohort.diverges(), None, "two values held by quorums at a position");
    let now = self.cohort.committed_history();
    for (log, before) in &self.history {
      let after = now.get(log).map_or(&[][..], Vec::as_slice);
      prop_assert!(after.starts_with(before), "NoLoss: {:?} lost a committed record", log);
    }
    self.count_committed(&now);
    self.history = now;
    for host in self.hosts.clone() {
      self.check_holder(host)?;
    }
    Ok(())
  }

  /// Counts the records `now` holds that the history did not.
  fn count_committed(&mut self, now: &Answer) {
    let before: BTreeSet<&[u8; 32]> = self.history.values().flatten().collect();
    let fresh: BTreeSet<&[u8; 32]> = now
      .values()
      .flatten()
      .filter(|identity| !before.contains(identity))
      .collect();
    for identity in fresh {
      let logs = self.records.get(identity).map_or(0, Vec::len);
      let census = &mut self.census;
      if logs > 1 {
        census.atomic_committed = census.atomic_committed.saturating_add(1);
      } else {
        census.single_committed = census.single_committed.saturating_add(1);
      }
    }
  }

  /// A holder's applied cut: answered whole at itself (no record split, the committed history), never behind what it
  /// applied before; its records of several logs counted as applied at their joins.
  fn check_holder(&mut self, host: HostId) -> Result<(), TestCaseError> {
    let applied = self.cohort.applied(host);
    let before = self.applied.get(&host).cloned().unwrap_or_default();
    for (log, position) in &before {
      let now = applied.get(log).copied().unwrap_or(0);
      prop_assert!(now >= *position, "{:?} unapplied {:?}", host, log);
    }
    let answer = self
      .cohort
      .read(host, &applied)
      .map_err(|refusal| TestCaseError::fail(format!("{host:?} applied {refusal:?}")))?;
    self.judge_answer(&applied, &answer)?;
    for identity in answer.values().flatten() {
      let several = self.records.get(identity).is_some_and(|spans| spans.len() > 1);
      if several && self.joined.insert((host, *identity)) {
        self.census.applied_at_join = self.census.applied_at_join.saturating_add(1);
      }
    }
    self.applied.insert(host, applied);
    Ok(())
  }
}

/// A count as a census count.
fn count(len: usize) -> u32 {
  u32::try_from(len).unwrap_or(u32::MAX)
}

/// Runs `steps` at `f` and returns the committed history and what it met.
fn run(f: u32, steps: &[Step]) -> Result<(Answer, Census), TestCaseError> {
  let mut world = World::new(f);
  for step in steps {
    world.step(step)?;
  }
  Ok((world.history, world.census))
}

/// A-636 and A-642's robustness rules, by generated histories: do run histories of appends (one log and several, some
/// misread), a superseded owner's appends, partitions, heals, offers, takeovers from any host and replica reads at any
/// host by any vector, at `f` from 0 to 2, over three greens and the four state logs on the owner's two copysets; run
/// each twice. Expect after every step no position held by two quorums (Agreement), every committed record kept at its
/// position (NoLoss), no superseded owner's record whole or committed (StaleNeverCommits), no committed record dropped
/// by a takeover, every holder's applied cut the committed history with no record split and never behind itself, and
/// every replica answer the committed history at an unsplit vector, every refusal true; the two runs alike
/// (determinism); and every kind of step met across the histories.
#[test]
fn every_history_of_crashes_partitions_and_takeovers_keeps_each_record_whole() {
  let census = RefCell::new(Census::default());
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(Config {
    cases: slates_test_seeds::cases(128),
    ..Config::default()
  }));
  let histories = (0..3u32, proptest::collection::vec(step(), 0..STEPS));
  let result = runner.run(&histories, |(f, steps)| {
    let first = run(f, &steps)?;
    let second = run(f, &steps)?;
    prop_assert_eq!(&first, &second, "the same history ran differently");
    census.borrow_mut().add(&first.1);
    Ok(())
  });
  if let Err(failure) = result {
    panic!("{failure}");
  }
  let census = census.into_inner();
  eprintln!("census {census:?}");
  assert!(
    census.met().iter().all(|&count| count > 0),
    "a kind of step never met: {census:?}"
  );
}
