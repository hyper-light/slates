//! A-603's and A-635's tests (§4.17.1 shared piece 2, rebuilt by A-642): an entry of a repository's logs, driven
//! through the partition's guard and apply, and through a database over a real anchor segment. Each green keeps its own
//! log and the repository's own state its logs of its own; an entry appends to each green only while it is at the
//! position the push read; an entry moving two logs or more is an atomic record, which a holder applies at its join and
//! a cut holds whole or not at all. The model test runs generated histories of entries, single-green appends,
//! checkpoints, destroys, snapshots and crashes against the design's rules kept over plain maps, the database dropped
//! and recovered from its segment at random points, holding NoLoss, Agreement and StaleNeverCommits.

// proptest's strategy types carry `Arc` (D-8's harness exception).
#![allow(clippy::disallowed_types)]

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use slates_anchor::{AnchorSegment, Geometry};
use slates_machine::facts::Identity;

use crate::catalog::{
  AtomicRecord, BaseRecord, GreenVersion, Join, LogName, LogSpan, MemberOrigin, MemberRecord,
  NameBinding, NameChange, NamePolicy, ObjectFormat, PolicyRecord, Principal, RepositoryCheck,
  RepositoryEntry, RepositoryId, RepositoryRecord, Role, SizeClass, SnapshotId, StateChange,
  StateLog, VcsTool, VolumeId, VolumeRecord, VolumeState,
};
use crate::error::DbError;
use crate::op::Op;
use crate::partition::{Partition, PartitionCaps};
use crate::replay::{SnapshotPolicy, recover};

mod replica;

/// Shape: the partition the tests' repositories live on.
const HOME: u16 = 3;
/// Shape: another partition, whose green no entry of a repository on `HOME` may take.
const ELSEWHERE: u16 = 4;
/// Shape: a merge budget no test here reaches, but the one that measures the budget.
const ROOMY: usize = 1 << 20;

/// An id on `partition` with `serial` its number, in the one family every test id shares (tag 0), as the server mints
/// a repository's and its greens' (A-636 part 2b-2), so a green of the partition may join any of its repositories.
fn id_bytes(partition: u16, serial: u8) -> [u8; 16] {
  crate::register::ObjectId::family(crate::register::HostId(0), partition, 0, u32::from(serial))
    .map_or([0u8; 16], |id| id.0)
}

fn repository_id(serial: u8) -> RepositoryId {
  RepositoryId {
    bytes: id_bytes(HOME, serial),
  }
}

fn volume_id(partition: u16, serial: u8) -> VolumeId {
  VolumeId {
    bytes: id_bytes(partition, serial),
  }
}

/// A green on `HOME`.
fn green(serial: u8) -> VolumeId {
  volume_id(HOME, serial)
}

fn caps(green_chain_bytes: usize) -> PartitionCaps {
  PartitionCaps {
    volumes: 1 << 4,
    snapshots: 1 << 4,
    attachments: 1 << 4,
    segment_slots: 4,
    timers: 1 << 4,
    tick_ns: 1_000,
    green_chain_bytes,
    audit_records: 1 << 4,
  }
}

fn owner() -> Principal {
  Principal::Certificate { hash: [7; 32] }
}

/// A volume record on its id's partition: a green, or a plain volume.
fn volume(id: VolumeId, is_green: bool) -> VolumeRecord {
  VolumeRecord {
    id,
    name: format!("volume-{}-{}", id.partition(), id.bytes[15]),
    owner_shard: id.partition(),
    policy: PolicyRecord {
      size: SizeClass::Dynamic { max: 0 },
      names: NamePolicy::Exact,
      require_locked: false,
      role: if is_green {
        Role::Green {
          require_evidence: false,
          head_version: 0,
        }
      } else {
        Role::Plain
      },
      live_shipped: false,
      live_holders: None,
      write: None,
      write_version: 0,
      signers: Vec::new(),
    },
    base: BaseRecord::Scratch,
    head: SnapshotId::default(),
    epoch: 0,
    referenced_bytes: 0,
    unique_bytes: 0,
    state: VolumeState::Live,
    lease: None,
    owner: owner(),
    access: Vec::new(),
    created_ns: 0,
    catalog_version: 0,
    taken_over: None,
  }
}

fn repository(serial: u8, name: &str) -> RepositoryRecord {
  RepositoryRecord {
    index_bits: 0,
    id: repository_id(serial),
    name: name.to_owned(),
    tool: VcsTool::Mercurial,
    format: ObjectFormat::Sha1,
    default_branch: None,
    owner: owner(),
    policy: crate::catalog::RepositoryPolicy::default(),
  }
}

/// Runs `op` as the live path does: the guard, then the apply.
fn run(partition: &mut Partition, op: &Op) -> Result<(), DbError> {
  partition.check(op, 0)?;
  partition.apply(op)
}

/// Runs `entry`.
fn advance(partition: &mut Partition, entry: RepositoryEntry) -> Result<(), DbError> {
  run(partition, &Op::RepositoryAdvanced { entry })
}

/// An entry of `repository`'s logs, changing nothing yet.
fn at(repository: RepositoryId) -> RepositoryEntry {
  RepositoryEntry {
    repository,
    ..RepositoryEntry::default()
  }
}

fn version(green: VolumeId, version: u64, entry: &[u8]) -> GreenVersion {
  GreenVersion {
    green,
    version,
    entry: entry.to_vec(),
  }
}

fn member(repository: RepositoryId, green: VolumeId, origin: MemberOrigin) -> MemberRecord {
  MemberRecord {
    repository,
    green,
    origin,
  }
}

fn bind(name: &str, green: VolumeId) -> NameChange {
  NameChange::Bind {
    name: name.to_owned(),
    green,
  }
}

fn retire(name: &str) -> NameChange {
  NameChange::Retire {
    name: name.to_owned(),
  }
}

fn bound(pairs: &[(&str, VolumeId)]) -> Vec<NameBinding> {
  pairs
    .iter()
    .map(|(name, green)| NameBinding {
      name: (*name).to_owned(),
      green: *green,
    })
    .collect()
}

fn head(green: VolumeId, version: u64) -> RepositoryCheck {
  RepositoryCheck::Head { green, version }
}

fn names(prefix: &str, pairs: &[(&str, VolumeId)]) -> RepositoryCheck {
  RepositoryCheck::Names {
    prefix: prefix.to_owned(),
    bound: bound(pairs),
  }
}

fn root_added(phase: u8, node: u8) -> StateChange {
  StateChange::PhaseRootAdded {
    phase,
    node: vec![node; 20],
  }
}

fn root_removed(phase: u8, node: u8) -> StateChange {
  StateChange::PhaseRootRemoved {
    phase,
    node: vec![node; 20],
  }
}

fn marker(bytes: &[u8]) -> StateChange {
  StateChange::MarkerAdded {
    marker: bytes.to_vec(),
  }
}

fn locked(path: &str, lock: &[u8]) -> StateChange {
  StateChange::LockTaken {
    path: path.to_owned(),
    lock: lock.to_vec(),
  }
}

fn unlocked(path: &str, lock: &[u8]) -> StateChange {
  StateChange::LockReleased {
    path: path.to_owned(),
    lock: lock.to_vec(),
  }
}

/// The refusal of a green that moved from the position the push read.
fn moved(green: VolumeId, read: u64, at: u64) -> DbError {
  DbError::Moved {
    green: green.bytes,
    read,
    at,
  }
}

fn chain(partition: &Partition, green: VolumeId) -> Vec<Vec<u8>> {
  partition.green_chain(green).to_vec()
}

fn log_position(partition: &Partition, repository: RepositoryId, log: StateLog) -> Option<u64> {
  partition.repositories().log_position(repository, log)
}

fn green_log(green: VolumeId) -> LogName {
  LogName::Green { green }
}

fn state_log(repository: RepositoryId, log: StateLog) -> LogName {
  LogName::State { repository, log }
}

fn span(log: LogName, from: u64, to: u64) -> LogSpan {
  LogSpan { log, from, to }
}

/// A partition rebuilt from `partition`'s snapshot answers as `partition` does, whole.
fn assert_snapshot_rebuilds(partition: &Partition, caps: PartitionCaps) {
  let snapshot = partition.to_snapshot(0);
  let rebuilt = Partition::from_snapshot(&snapshot, caps, 0).unwrap();
  assert_eq!(rebuilt.to_snapshot(0), snapshot);
}

/// A repository on `HOME` whose lineage holds a root `a` and branches `b` and `c` made from it, bound to `main`, `topic`
/// and `other` by its first entry, and beside them a plain volume and a green of another partition: the partition, the
/// repository, and `[a, b, c, plain, foreign]`.
fn three_branches(caps: PartitionCaps) -> (Partition, RepositoryId, [VolumeId; 5]) {
  let mut partition = Partition::new(caps, 0);
  let ids = [
    green(1),
    green(2),
    green(3),
    volume_id(HOME, 4),
    volume_id(ELSEWHERE, 5),
  ];
  for (id, is_green) in ids.into_iter().zip([true, true, true, false, true]) {
    run(
      &mut partition,
      &Op::VolumeCreated {
        record: volume(id, is_green),
      },
    )
    .unwrap();
  }
  let record = repository(9, "linux");
  let repo = record.id;
  run(&mut partition, &Op::RepositoryCreated { record }).unwrap();
  let [a, b, c, ..] = ids;
  let made_from_a = MemberOrigin::Branch {
    from: a,
    version: 0,
  };
  let mut first = at(repo);
  first.joined = vec![
    member(repo, a, MemberOrigin::Root),
    member(repo, b, made_from_a),
    member(repo, c, made_from_a),
  ];
  first.names = vec![bind("main", a), bind("topic", b), bind("other", c)];
  advance(&mut partition, first).unwrap();
  (partition, repo, ids)
}

/// A-603, A-642 (§4.17.1 item 2: git's atomic push, one record for several greens). Do: commit versions of two of a
/// repository's three greens in one entry; then send an entry whose second version skips one, one naming a version
/// made from a position its green moved past, and entries naming a green outside the lineage; then land two versions of
/// one green in one entry (a stack landed whole). Expect: the first entry moves both chains, and is one atomic record
/// that each green's log points to at its new position; each refused entry is refused by its kind (`Moved`, naming the
/// green that moved, the position read and the one it is at; `NotFound` for a green the lineage does not hold) with
/// every chain as it was; the stack moves one green only, so it is no atomic record; a partition rebuilt from its
/// snapshot answers the same.
#[test]
fn an_entry_commits_several_greens_versions_together_or_none_of_them() {
  let (mut partition, repo, [a, b, c, plain, foreign]) = three_branches(caps(ROOMY));
  let mut push = at(repo);
  push.versions = vec![version(a, 1, b"a1"), version(b, 1, b"b1")];
  advance(&mut partition, push).unwrap();
  assert_eq!(chain(&partition, a), vec![b"a1".to_vec()]);
  assert_eq!(chain(&partition, b), vec![b"b1".to_vec()]);
  assert!(chain(&partition, c).is_empty());
  let record = AtomicRecord {
    spans: vec![span(green_log(a), 0, 1), span(green_log(b), 0, 1)],
  };
  assert_eq!(partition.repositories().atomic_at(a, 1), Some(&record));
  assert_eq!(partition.repositories().atomic_at(b, 1), Some(&record));
  assert_stale_and_outside_entries_are_refused(&mut partition, repo, [a, b, c, plain, foreign]);
  let mut stack = at(repo);
  stack.versions = vec![version(a, 2, b"a2"), version(a, 3, b"a3")];
  advance(&mut partition, stack).unwrap();
  assert_eq!(
    chain(&partition, a),
    vec![b"a1".to_vec(), b"a2".to_vec(), b"a3".to_vec()]
  );
  assert_eq!(partition.green_head(a), 3);
  assert_eq!(partition.repositories().atomic_at(a, 3), None);
  assert_snapshot_rebuilds(&partition, caps(ROOMY));
}

/// The refused entries of [`an_entry_commits_several_greens_versions_together_or_none_of_them`]: nothing of them
/// applied.
fn assert_stale_and_outside_entries_are_refused(
  partition: &mut Partition,
  repo: RepositoryId,
  [a, b, c, plain, foreign]: [VolumeId; 5],
) {
  let before = partition.to_snapshot(0);
  let mut skipping = at(repo);
  skipping.versions = vec![version(a, 2, b"a2"), version(b, 3, b"b3")];
  assert_eq!(advance(partition, skipping), Err(moved(b, 2, 1)));
  let mut stale = at(repo);
  stale.versions = vec![version(c, 1, b"c1"), version(a, 1, b"a-again")];
  assert_eq!(advance(partition, stale), Err(moved(a, 0, 1)));
  for outsider in [plain, foreign, green(99)] {
    let mut outside = at(repo);
    outside.versions = vec![version(c, 1, b"c1"), version(outsider, 1, b"x")];
    assert_eq!(
      advance(partition, outside),
      Err(DbError::NotFound),
      "{outsider:?}"
    );
  }
  assert_eq!(
    partition.to_snapshot(0),
    before,
    "a refused entry applied something"
  );
}

/// A-603, A-642 (§4.17.3 decision 6: `check:heads` and `unbundle`'s heads, every head unchanged, each green's position
/// compared). Do: commit an entry checking every name and an unmoved green's head; move that green and send the first
/// entry's checks again; retire a name and check names under prefixes; check a green's head at the version the entry
/// itself makes and at the one before it; check heads outside the repository; check a state log's position. Expect: the
/// checks hold at first; a moved head refuses the entry `Moved`, naming the green, nothing of it applied; a names check
/// refuses `CheckFailed` naming its index once a name under its prefix changed, and holds while names elsewhere change;
/// a check reads before the entry's own changes; a head outside the repository never holds; a state log's check holds
/// at its position and refuses past it.
#[test]
fn checks_read_each_logs_position_and_refuse_the_whole_entry() {
  let (mut partition, repo, [a, b, c, plain, _]) = three_branches(caps(ROOMY));
  let every = || names("", &[("main", a), ("other", c), ("topic", b)]);
  let mut push = at(repo);
  push.checks = vec![every(), head(c, 0)];
  push.versions = vec![version(a, 1, b"a1")];
  advance(&mut partition, push).unwrap();
  let mut other = at(repo);
  other.versions = vec![version(c, 1, b"c1")];
  advance(&mut partition, other).unwrap();
  let before = partition.to_snapshot(0);
  let mut late = at(repo);
  late.checks = vec![every(), head(c, 0)];
  late.versions = vec![version(a, 2, b"a2")];
  late.names = vec![retire("topic")];
  assert_eq!(advance(&mut partition, late), Err(moved(c, 0, 1)));
  assert_eq!(partition.to_snapshot(0), before);
  assert_names_checks_follow_their_prefixes(&mut partition, repo, [a, b, c]);
  assert_checks_read_before_the_entrys_own_changes(&mut partition, repo, a);
  let elsewhere = second_repository_with_root(&mut partition, green(6));
  for outsider in [plain, elsewhere, green(99)] {
    let mut outside = at(repo);
    outside.checks = vec![head(outsider, 0)];
    assert_eq!(
      advance(&mut partition, outside),
      Err(DbError::CheckFailed { index: 0 }),
      "{outsider:?}"
    );
  }
  assert_a_state_logs_check_holds_at_its_position(&mut partition, repo);
}

fn assert_names_checks_follow_their_prefixes(
  partition: &mut Partition,
  repo: RepositoryId,
  [a, b, c]: [VolumeId; 3],
) {
  let mut retiring = at(repo);
  retiring.names = vec![retire("topic")];
  advance(partition, retiring).unwrap();
  let mut named = at(repo);
  named.checks = vec![names("t", &[("topic", b)])];
  assert_eq!(
    advance(partition, named),
    Err(DbError::CheckFailed { index: 0 })
  );
  let mut narrow = at(repo);
  narrow.checks = vec![names("o", &[("other", c)]), head(c, 1)];
  narrow.versions = vec![version(a, 2, b"a2")];
  advance(partition, narrow).unwrap();
  let mut adding = at(repo);
  adding.names = vec![bind("topic", b)];
  adding.checks = vec![names("", &[("main", a), ("other", c)])];
  advance(partition, adding).unwrap();
  let mut stale = at(repo);
  stale.checks = vec![names("", &[("main", a), ("other", c)])];
  assert_eq!(
    advance(partition, stale),
    Err(DbError::CheckFailed { index: 0 })
  );
}

fn assert_checks_read_before_the_entrys_own_changes(
  partition: &mut Partition,
  repo: RepositoryId,
  a: VolumeId,
) {
  let mut own = at(repo);
  own.checks = vec![head(a, 3)];
  own.versions = vec![version(a, 3, b"a3")];
  assert_eq!(advance(partition, own.clone()), Err(moved(a, 3, 2)));
  own.checks = vec![head(a, 2)];
  advance(partition, own).unwrap();
}

fn assert_a_state_logs_check_holds_at_its_position(partition: &mut Partition, repo: RepositoryId) {
  let phases = log_position(partition, repo, StateLog::Phases).unwrap();
  let mut moving = at(repo);
  moving.checks = vec![RepositoryCheck::Log {
    log: StateLog::Phases,
    position: phases,
  }];
  moving.state = vec![root_added(1, 9)];
  advance(partition, moving.clone()).unwrap();
  moving.state = vec![root_added(1, 8)];
  assert_eq!(
    advance(partition, moving),
    Err(DbError::CheckFailed { index: 0 })
  );
}

/// A second repository on `HOME`, `other`, whose root is `root`, made a green here first. Its root.
fn second_repository_with_root(partition: &mut Partition, root: VolumeId) -> VolumeId {
  run(
    partition,
    &Op::VolumeCreated {
      record: volume(root, true),
    },
  )
  .unwrap();
  let record = repository(10, "other");
  let second = record.id;
  run(partition, &Op::RepositoryCreated { record }).unwrap();
  let mut first = at(second);
  first.joined = vec![member(second, root, MemberOrigin::Root)];
  advance(partition, first).unwrap();
  root
}

/// A-603, A-642 (§4.17.3 decisions 3 and 4; the repository's own state in logs of its own). Do: commit a version with a
/// phase root and two markers; move phases alone; take and release LFS locks alone; send entries, each with a version,
/// whose state change was made against another state (a root that is one, one that is not, a held marker, a marker
/// twice, a lock on a locked path, a release of another lock). Expect: the roots, markers and locks read back in their
/// orders; each state log at a position of its own, moved only by its own changes; the version with state is one
/// atomic record over the green's log and the state logs it moved, and a state move alone is none; each wrong change
/// refuses its entry `StateChanged` naming its index, nothing applied; a partition rebuilt from its snapshot answers
/// the same.
#[test]
fn the_repositorys_state_moves_in_logs_of_its_own_with_its_push_or_alone() {
  let (mut partition, repo, [a, ..]) = three_branches(caps(ROOMY));
  let mut push = at(repo);
  push.versions = vec![version(a, 1, b"a1")];
  push.state = vec![root_added(1, 1), marker(b"m1"), marker(b"m2")];
  advance(&mut partition, push).unwrap();
  let record = AtomicRecord {
    spans: vec![
      span(green_log(a), 0, 1),
      span(state_log(repo, StateLog::Phases), 0, 1),
      span(state_log(repo, StateLog::Markers), 0, 2),
    ],
  };
  assert_eq!(partition.repositories().atomic_at(a, 1), Some(&record));
  let mut alone = at(repo);
  alone.state = vec![
    root_removed(1, 1),
    root_added(1, 2),
    locked("x.bin", b"lock-1"),
  ];
  advance(&mut partition, alone).unwrap();
  assert_state_logs_read_back(&partition, repo);
  assert_wrong_state_changes_are_refused(&mut partition, repo, a);
  assert_snapshot_rebuilds(&partition, caps(ROOMY));
}

fn assert_state_logs_read_back(partition: &Partition, repo: RepositoryId) {
  let repositories = partition.repositories();
  let roots: Vec<(u8, Vec<u8>)> = repositories
    .phase_roots(repo)
    .map(|(phase, node)| (phase, node.to_vec()))
    .collect();
  assert_eq!(roots, vec![(1, vec![2; 20])]);
  let markers: Vec<&[u8]> = repositories.markers(repo).collect();
  assert_eq!(markers, vec![&b"m1"[..], &b"m2"[..]]);
  let locks: Vec<(&str, &[u8])> = repositories.locks(repo).collect();
  assert_eq!(locks, vec![("x.bin", &b"lock-1"[..])]);
  let positions: Vec<Option<u64>> = [
    StateLog::Names,
    StateLog::Phases,
    StateLog::Markers,
    StateLog::Locks,
  ]
  .into_iter()
  .map(|log| log_position(partition, repo, log))
  .collect();
  assert_eq!(positions, vec![Some(6), Some(3), Some(2), Some(1)]);
}

fn assert_wrong_state_changes_are_refused(
  partition: &mut Partition,
  repo: RepositoryId,
  a: VolumeId,
) {
  let before = partition.to_snapshot(0);
  for (state, index) in [
    (vec![root_added(1, 2)], 0),
    (vec![marker(b"m3"), root_removed(1, 1)], 1),
    (vec![marker(b"m1")], 0),
    (vec![marker(b"m3"), marker(b"m3")], 1),
    (vec![locked("y.bin", b"l"), locked("x.bin", b"l")], 1),
    (vec![unlocked("x.bin", b"another")], 0),
  ] {
    let mut wrong = at(repo);
    wrong.versions = vec![version(a, 2, b"a2")];
    wrong.state = state;
    assert_eq!(
      advance(partition, wrong),
      Err(DbError::StateChanged { index })
    );
  }
  assert_eq!(
    partition.to_snapshot(0),
    before,
    "a refused entry applied something"
  );
  let mut release = at(repo);
  release.state = vec![unlocked("x.bin", b"lock-1")];
  advance(partition, release).unwrap();
  assert_eq!(partition.repositories().locks(repo).count(), 0);
}

/// A-603, A-642 (bounded growth). Do: on a partition whose merge budget holds 256 bytes, add a marker of 100 bytes, then
/// one of 200; send two versions of 100 bytes each, which fit one at a time and not together; take a lock of 200; destroy
/// the repository; then add the larger marker to a new repository. Expect: the larger marker, the two versions and the
/// lock are each refused `Capacity` while the first marker is held, nothing applied; the destroy gives the first's bytes
/// back, so the larger fits after it; a partition rebuilt from its snapshot charges the same.
#[test]
fn the_repositorys_state_is_charged_to_the_merge_budget_and_given_back_with_it() {
  let budget = caps(256);
  let (mut partition, repo, [a, b, ..]) = three_branches(budget);
  let mut small = at(repo);
  small.state = vec![marker(&[1; 100])];
  advance(&mut partition, small).unwrap();
  let before = partition.to_snapshot(0);
  let full = Err(DbError::Capacity {
    table: "green_chains",
  });
  let mut large = at(repo);
  large.state = vec![marker(&[2; 200])];
  assert_eq!(advance(&mut partition, large.clone()), full);
  let mut together = at(repo);
  together.versions = vec![version(a, 1, &[3; 100]), version(b, 1, &[4; 100])];
  assert_eq!(advance(&mut partition, together), full);
  let mut lock = at(repo);
  lock.state = vec![locked("big", &[5; 200])];
  assert_eq!(advance(&mut partition, lock), full);
  assert_eq!(partition.to_snapshot(0), before);
  let mut rebuilt = Partition::from_snapshot(&partition.to_snapshot(0), budget, 0).unwrap();
  assert_eq!(
    advance(&mut rebuilt, large),
    full,
    "a rebuilt partition charges the state it carries"
  );
  run(
    &mut partition,
    &Op::RepositoryDestroyed { repository: repo },
  )
  .unwrap();
  assert_eq!(log_position(&partition, repo, StateLog::Names), None);
  second_repository_with_root(&mut partition, green(6));
  let mut again = at(repository_id(10));
  again.state = vec![marker(&[2; 200])];
  advance(&mut partition, again).unwrap();
}

/// A-662 (§4.17.1 "Durability and residency": a push's objects held as a version holds its files' contents). Do: append
/// two pushes, each a version of the root and its objects; then one whose objects alone pass the merge budget; rebuild
/// the partition from its snapshot; destroy the repository. Expect: the objects read back in the order the register
/// received them, and their bytes counted (`Repositories::objects_bytes`); a push of one green and its objects one
/// atomic record of the green and the names log, since the objects ride the names log to the repository's candidates
/// (A-724 piece 8c), so a takeover keeps or drops the push whole; the one past the merge budget is kept, since objects are the
/// repository's content, charged by its owner shard to its memory budget, never the merge budget the partition's
/// chains share (found by use: a Subversion import of 5,000 revisions refused `green_chains` at 1,685); the rebuilt
/// partition reads the same objects; the destroy drops them.
#[test]
fn a_pushs_objects_are_kept_in_order_carried_by_a_snapshot_and_never_charged_to_the_merge_budget() {
  let budget = caps(256);
  let (mut partition, repo, [a, ..]) = three_branches(budget);
  for (at_version, byte) in [(1, 7u8), (2, 8)] {
    let mut pushed = at(repo);
    pushed.versions = vec![version(a, at_version, &[byte; 10])];
    pushed.objects = vec![byte; 50];
    advance(&mut partition, pushed).unwrap();
  }
  let atomic = partition.repositories().atomic_at(a, 1).expect("the push is one atomic record");
  assert!(
    atomic.spans.iter().any(|span| matches!(span.log, LogName::State { log: StateLog::Names, .. })),
    "{atomic:?}"
  );
  let kept: Vec<Vec<u8>> = partition
    .repositories()
    .objects(repo)
    .map(<[u8]>::to_vec)
    .collect();
  assert_eq!(kept, vec![vec![7u8; 50], vec![8u8; 50]]);
  assert_eq!(
    partition.repositories().object_record(repo, 1),
    Some(&[8u8; 50][..])
  );
  let mut large = at(repo);
  large.versions = vec![version(a, 3, &[9; 10])];
  large.objects = vec![9; 300];
  advance(&mut partition, large).unwrap();
  assert_eq!(partition.repositories().objects_bytes(repo), 400);
  let rebuilt = Partition::from_snapshot(&partition.to_snapshot(0), budget, 0).unwrap();
  assert!(
    rebuilt
      .repositories()
      .objects(repo)
      .eq(partition.repositories().objects(repo))
  );
  run(
    &mut partition,
    &Op::RepositoryDestroyed { repository: repo },
  )
  .unwrap();
  assert_eq!(partition.repositories().objects(repo).count(), 0);
}

/// A-642 (no global reference: each green and each state log at its own position). Do: append to one green alone, to
/// two at once, and to a third alone after an entry was made against the first's position before the second moved;
/// change names alone; record a member's version alone (`GreenAdvanced`); check a log after another log moved. Expect:
/// each green's position moves only by its own versions and each state log's only by its own changes; an entry made
/// against one green's position commits whatever other greens did meanwhile; a lone version is no atomic record; the
/// A-515 and A-575 operations move the names log alone.
#[test]
fn no_change_waits_on_another_logs_position() {
  let (mut partition, repo, [a, b, c, ..]) = three_branches(caps(ROOMY));
  let names_at = log_position(&partition, repo, StateLog::Names);
  let mut made_against_a = at(repo);
  made_against_a.versions = vec![version(a, 1, b"a1")];
  made_against_a.checks = vec![head(a, 0)];
  let mut both = at(repo);
  both.versions = vec![version(b, 1, b"b1"), version(c, 1, b"c1")];
  advance(&mut partition, both).unwrap();
  advance(&mut partition, made_against_a).unwrap();
  assert_eq!(
    [a, b, c].map(|green| partition.green_head(green)),
    [1, 1, 1]
  );
  assert_eq!(partition.repositories().atomic_at(a, 1), None);
  assert_eq!(log_position(&partition, repo, StateLog::Names), names_at);
  run(
    &mut partition,
    &Op::GreenAdvanced {
      green: a,
      increment: b"a2".to_vec(),
    },
  )
  .unwrap();
  assert_eq!(partition.green_head(a), 2);
  assert_eq!(partition.green_head(b), 1);
  run(
    &mut partition,
    &Op::RepositoryBranchRetired {
      repository: repo,
      name: "other".to_owned(),
    },
  )
  .unwrap();
  assert_eq!(
    log_position(&partition, repo, StateLog::Names),
    names_at.map(|position| position + 1)
  );
  assert_eq!(log_position(&partition, repo, StateLog::Phases), Some(0));
}

/// A-642 requirements 1 and 2 (a holder applies an atomic record only at its join; a replica refuses a cut that splits
/// one). Do: commit one entry moving two greens and a state log; read where holders at various positions stand against
/// it; ask the partition about cuts of the logs. Expect: every log at its predecessor is `Ready`; one behind is
/// `Waiting`; all past it `Applied`; one past and one at its predecessor `Diverged`; a cut holding one green's new
/// position beside another's old one is refused naming the record, while cuts holding all of it, none of it, or naming
/// one of its logs alone are consistent.
#[test]
fn a_holder_applies_an_atomic_record_at_its_join_and_a_cut_holds_all_of_it_or_none() {
  let (mut partition, repo, [a, b, ..]) = three_branches(caps(ROOMY));
  let mut push = at(repo);
  push.versions = vec![version(a, 1, b"a1"), version(b, 1, b"b1")];
  push.state = vec![marker(b"m")];
  advance(&mut partition, push).unwrap();
  let record = partition.repositories().atomic_at(b, 1).cloned().unwrap();
  let markers = state_log(repo, StateLog::Markers);
  let holder = |at_a: u64, at_b: u64, at_markers: u64| {
    move |log: &LogName| match log {
      LogName::Green { green } if *green == a => at_a,
      LogName::Green { .. } => at_b,
      LogName::State { .. } => at_markers,
    }
  };
  assert_eq!(record.join(holder(0, 0, 0)), Join::Ready);
  assert_eq!(record.join(holder(1, 1, 1)), Join::Applied);
  assert_eq!(record.join(holder(1, 0, 0)), Join::Diverged);
  let lagging = AtomicRecord {
    spans: vec![span(green_log(a), 3, 4), span(green_log(b), 1, 2)],
  };
  assert_eq!(lagging.join(holder(2, 1, 0)), Join::Waiting);
  let split = [(green_log(a), 1), (green_log(b), 0)];
  assert_eq!(partition.repositories().split_by(&split), Some(&record));
  for consistent in [
    vec![(green_log(a), 1), (green_log(b), 1), (markers, 1)],
    vec![(green_log(a), 0), (green_log(b), 0)],
    vec![(green_log(a), 1)],
  ] {
    assert_eq!(partition.repositories().split_by(&consistent), None);
  }
}

/// A-642 requirement 3's retention (an atomic record lives while a green's log points to it). Do: commit a record over
/// two greens; checkpoint one past it; then destroy the other; and rebuild from a snapshot between. Expect: the record
/// stays while the second green's log points to it, and the snapshot carries it; it goes with the second's destroy.
#[test]
fn an_atomic_record_lives_while_a_greens_log_points_to_it() {
  let (mut partition, repo, [a, b, c, ..]) = three_branches(caps(ROOMY));
  let mut push = at(repo);
  push.versions = vec![version(b, 1, b"b1"), version(c, 1, b"c1")];
  advance(&mut partition, push).unwrap();
  let record = partition.repositories().atomic_at(b, 1).cloned().unwrap();
  run(
    &mut partition,
    &Op::GreenCheckpointed {
      green: b,
      version: 1,
      checkpoint: b"checkpoint".to_vec(),
    },
  )
  .unwrap();
  assert_eq!(partition.repositories().atomic_at(b, 1), None);
  assert_eq!(partition.repositories().atomic_at(c, 1), Some(&record));
  assert_snapshot_rebuilds(&partition, caps(ROOMY));
  run(&mut partition, &Op::VolumeDestroyed { id: c }).unwrap();
  assert_eq!(partition.repositories().atomic_records().count(), 0);
  assert!(partition.volume(a).is_some());
}

// The model test.

/// Shape: the greens a generated history works over, few so histories collide on each rule often.
const GREENS: u8 = 4;
/// Shape: the volume index of the plain volume.
const PLAIN: u8 = GREENS;
/// Shape: the volume index of the green on another partition.
const FOREIGN: u8 = GREENS + 1;
/// Shape: the volume index of an id nothing holds.
const UNKNOWN: u8 = GREENS + 2;
/// Shape: every volume index a step may name.
const VOLUMES: u8 = GREENS + 3;
/// Shape: the names a history binds; one index past them is the empty name.
const NAMES: u8 = 3;
/// Shape: the prefixes a history's name checks read.
const PREFIXES: [&str; 3] = ["", "n", "n1"];
/// Shape: the nodes a history's phase roots name, and the phases.
const NODES: u8 = 3;
/// Shape: see [`NODES`].
const PHASES: u8 = 2;
/// Shape: the markers a history adds, and the paths its locks name.
const MARKERS: u8 = 3;
/// Shape: the database's log ring, and each snapshot slot, in bytes: a history's records and its partition fit.
const SEGMENT_BYTES: u64 = 1 << 16;
/// Shape: the log bytes between the snapshots the database takes on its own, a few entries' worth, so a history's
/// recoveries start from a snapshot and replay a tail.
const SNAPSHOT_EVERY: u64 = 1 << 11;
/// Shape: the segment's page.
const PAGE: u64 = 4096;
/// Shape: the state logs, in log order.
const STATE_LOGS: [StateLog; 4] = [
  StateLog::Names,
  StateLog::Phases,
  StateLog::Markers,
  StateLog::Locks,
];

/// The volume a history's index names: the greens and the plain volume on `HOME`, the foreign green on `ELSEWHERE`, and
/// an id on `HOME` that nothing holds. Each index is its id's last byte.
fn volume_of(index: u8) -> VolumeId {
  if index == FOREIGN {
    volume_id(ELSEWHERE, index)
  } else {
    volume_id(HOME, index)
  }
}

/// The index of a volume a history named.
fn index_of(volume: VolumeId) -> u8 {
  volume.bytes[15]
}

fn name_of(index: u8) -> String {
  if index < NAMES {
    format!("n{index}")
  } else {
    String::new()
  }
}

/// How a step's value departs from the one the model holds right: mostly it does not.
#[derive(Clone, Copy, Debug)]
enum Skew {
  Exact,
  Below,
  Above,
}

impl Skew {
  fn of(choice: u8) -> Skew {
    match choice % 10 {
      0 => Skew::Below,
      1 => Skew::Above,
      _ => Skew::Exact,
    }
  }

  fn apply(self, value: u64) -> u64 {
    match self {
      Skew::Exact => value,
      Skew::Below => value.saturating_sub(1),
      Skew::Above => value.saturating_add(1),
    }
  }
}

/// A green a step names: mostly a member of the lineage at the step (the model's `n`-th, wrapping), else any index.
#[derive(Clone, Copy, Debug)]
enum Pick {
  Member(u8),
  Volume(u8),
}

impl Pick {
  fn of((choice, n): (u8, u8)) -> Pick {
    if choice % 5 == 0 {
      Pick::Volume(n % VOLUMES)
    } else {
      Pick::Member(n)
    }
  }
}

#[derive(Clone, Debug)]
enum CheckStep {
  Head { green: Pick, skew: Skew },
  Names { prefix: u8, skew: Skew },
  Log { log: u8, skew: Skew },
}

#[derive(Clone, Debug)]
struct JoinStep {
  volume: u8,
  from: Option<(Pick, u8)>,
}

#[derive(Clone, Debug)]
enum NameStep {
  Bind { name: u8, green: Pick },
  Retire { name: u8 },
  Default { name: Option<u8> },
}

#[derive(Clone, Debug)]
enum StateStep {
  Add { phase: u8, node: u8 },
  Remove { phase: u8, node: u8 },
  Marker { marker: u8 },
  Lock { path: u8, lock: u8 },
  Unlock { path: u8, lock: u8 },
}

#[derive(Clone, Debug)]
struct EntryStep {
  /// Whether the entry keeps only its items that hold, judged in order on the model, at the right versions: an entry the
  /// rules accept whole, so histories reach records of several greens and state moves often, not refusals alone.
  clean: bool,
  checks: Vec<CheckStep>,
  joins: Vec<JoinStep>,
  versions: Vec<(Pick, Skew)>,
  names: Vec<NameStep>,
  state: Vec<StateStep>,
}

#[derive(Clone, Debug)]
enum Step {
  Advance(EntryStep),
  /// One member's version alone (`Op::GreenAdvanced`), a single-green push between the records.
  Single {
    green: u8,
  },
  /// A member's checkpoint `back` versions below its head.
  Checkpoint {
    green: u8,
    back: u8,
  },
  Destroy {
    volume: u8,
  },
  Snapshot,
  Crash,
}

fn pick() -> impl Strategy<Value = Pick> {
  (any::<u8>(), any::<u8>()).prop_map(Pick::of)
}

fn skew() -> impl Strategy<Value = Skew> {
  any::<u8>().prop_map(Skew::of)
}

fn check_step() -> impl Strategy<Value = CheckStep> {
  (0..3u8, pick(), 0..4u8, skew()).prop_map(|(kind, green, n, skew)| match kind {
    0 => CheckStep::Head { green, skew },
    1 => CheckStep::Names { prefix: n, skew },
    _ => CheckStep::Log { log: n, skew },
  })
}

fn name_step() -> impl Strategy<Value = NameStep> {
  (0..6u8, 0..=NAMES, pick(), any::<bool>()).prop_map(|(kind, name, green, some)| match kind {
    0..=2 => NameStep::Bind { name, green },
    3 | 4 => NameStep::Retire { name: name % NAMES },
    _ => NameStep::Default {
      name: some.then_some(name),
    },
  })
}

fn state_step() -> impl Strategy<Value = StateStep> {
  (0..5u8, 0..PHASES, 0..NODES, 0..MARKERS).prop_map(|(kind, phase, node, n)| match kind {
    0 => StateStep::Add { phase, node },
    1 => StateStep::Remove { phase, node },
    2 => StateStep::Marker { marker: n },
    3 => StateStep::Lock {
      path: n,
      lock: node,
    },
    _ => StateStep::Unlock {
      path: n,
      lock: node,
    },
  })
}

fn entry_step() -> impl Strategy<Value = EntryStep> {
  let join =
    (0..VOLUMES, any::<u8>(), pick(), 0..4u8).prop_map(|(volume, root, from, version)| JoinStep {
      volume,
      from: (root % 10 != 0).then_some((from, version)),
    });
  (
    proptest::bool::weighted(0.6),
    proptest::collection::vec(check_step(), 0..3),
    proptest::collection::vec(join, 0..2),
    proptest::collection::vec((pick(), skew()), 0..5),
    proptest::collection::vec(name_step(), 0..3),
    proptest::collection::vec(state_step(), 0..4),
  )
    .prop_map(|(clean, checks, joins, versions, names, state)| EntryStep {
      clean,
      checks,
      joins,
      versions,
      names,
      state,
    })
}

fn step() -> impl Strategy<Value = Step> {
  (0..20u8, entry_step(), 0..VOLUMES, any::<u8>()).prop_map(|(kind, entry, volume, n)| match kind {
    0..=10 => Step::Advance(entry),
    11..=13 => Step::Single { green: n },
    14 => Step::Checkpoint {
      green: n,
      back: volume,
    },
    15 => Step::Destroy { volume },
    16 => Step::Snapshot,
    _ => Step::Crash,
  })
}

/// What an operation was answered, by kind, with what an entry's refusal names.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Answer {
  Ok,
  NotFound,
  AlreadyExists,
  NotGreen,
  InvalidName,
  BranchedFrom,
  Moved { green: u8, read: u64, at: u64 },
  CheckFailed(u32),
  StateChanged(u32),
  Unexpected(String),
}

fn answer_of(result: Result<u64, DbError>) -> Answer {
  match result {
    Ok(_) => Answer::Ok,
    Err(DbError::NotFound) => Answer::NotFound,
    Err(DbError::AlreadyExists { .. }) => Answer::AlreadyExists,
    Err(DbError::NotGreen) => Answer::NotGreen,
    Err(DbError::InvalidName) => Answer::InvalidName,
    Err(DbError::BranchedFrom { .. }) => Answer::BranchedFrom,
    Err(DbError::Moved { green, read, at }) => Answer::Moved {
      green: green[15],
      read,
      at,
    },
    Err(DbError::CheckFailed { index }) => Answer::CheckFailed(index),
    Err(DbError::StateChanged { index }) => Answer::StateChanged(index),
    Err(other) => Answer::Unexpected(format!("{other:?}")),
  }
}

/// One green's log in the model: the version its checkpoint folded to, and the entries after it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct GreenLog {
  floor: u64,
  entries: Vec<Vec<u8>>,
}

impl GreenLog {
  fn head(&self) -> u64 {
    self.floor + self.entries.len() as u64
  }
}

/// The design's rules for one repository (§4.17.1 items 2 to 5, shared piece 2; A-515, A-575, A-603, A-642), kept over
/// plain maps by volume index: the serial reference the partition must equal after every step. An entry is judged on a
/// copy of the model, item by item, and the copy kept only when every item held: all or none, by construction.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Model {
  /// The volumes the partition holds, by index.
  alive: BTreeSet<u8>,
  /// The lineage in the order it joined: each member and, for a branch, the member and version it was made at.
  members: Vec<(u8, Option<(u8, u64)>)>,
  /// The namespace: each name and the green it binds.
  names: BTreeMap<String, u8>,
  /// The default branch.
  default: Option<String>,
  /// Each green's log, where it has a version or a checkpoint.
  logs: BTreeMap<u8, GreenLog>,
  /// The state logs' positions, the markers' being their count.
  names_position: u64,
  phases_position: u64,
  locks_position: u64,
  /// The phase roots.
  roots: BTreeSet<(u8, Vec<u8>)>,
  /// The markers, in the order received.
  markers: Vec<Vec<u8>>,
  /// The locks, by path.
  locks: BTreeMap<String, Vec<u8>>,
  /// The atomic records kept, by key, each with the positions of greens' logs that point to it.
  records: BTreeMap<(LogName, u64), KeptRecord>,
}

/// An atomic record the model keeps, with the positions `(green, position)` of greens' logs that point to it.
type KeptRecord = (AtomicRecord, BTreeSet<(u8, u64)>);

impl Model {
  fn is_member(&self, volume: u8) -> bool {
    self.members.iter().any(|(member, _)| *member == volume)
  }

  fn head(&self, volume: u8) -> u64 {
    self.logs.get(&volume).map_or(0, GreenLog::head)
  }

  fn state_position(&self, log: StateLog) -> u64 {
    match log {
      StateLog::Names => self.names_position,
      StateLog::Phases => self.phases_position,
      StateLog::Markers => self.markers.len() as u64,
      StateLog::Locks => self.locks_position,
    }
  }

  /// The green `pick` names at this state.
  fn picked(&self, pick: Pick) -> u8 {
    match pick {
      Pick::Volume(volume) => volume,
      Pick::Member(n) => {
        let count = self.members.len().max(1);
        self
          .members
          .get(usize::from(n) % count)
          .map_or(0, |(member, _)| *member)
      }
    }
  }

  /// Bindings under `prefix`, in name order.
  fn under(&self, prefix: &str) -> Vec<NameBinding> {
    self
      .names
      .iter()
      .filter(|(name, _)| name.starts_with(prefix))
      .map(|(name, green)| NameBinding {
        name: name.clone(),
        green: volume_of(*green),
      })
      .collect()
  }

  /// A volume the guard holds as a green of this repository's partition: the refusal for one it does not, in the
  /// guard's order (absent, not a green, another partition's).
  fn green_here(&self, volume: u8) -> Result<(), Answer> {
    if !self.alive.contains(&volume) {
      return Err(Answer::NotFound);
    }
    if volume == PLAIN {
      return Err(Answer::NotGreen);
    }
    if volume == FOREIGN {
      return Err(Answer::NotFound);
    }
    Ok(())
  }

  /// Whether `check` holds at this state, the refusal when it does not.
  fn holds(&self, index: usize, check: &RepositoryCheck) -> Result<(), Answer> {
    let failed = Answer::CheckFailed(u32::try_from(index).unwrap_or(u32::MAX));
    match check {
      RepositoryCheck::Head { green, version } => {
        let green = index_of(*green);
        if !self.is_member(green) {
          return Err(failed);
        }
        let at = self.head(green);
        if at != *version {
          return Err(Answer::Moved {
            green,
            read: *version,
            at,
          });
        }
        Ok(())
      }
      RepositoryCheck::Names { prefix, bound } if self.under(prefix) != *bound => Err(failed),
      RepositoryCheck::Log { log, position } if self.state_position(*log) != *position => {
        Err(failed)
      }
      _ => Ok(()),
    }
  }

  /// Judges and, when it is accepted, applies `entry`: the rules in the design's order.
  fn advance(&mut self, repository: RepositoryId, entry: &RepositoryEntry) -> Answer {
    for (index, check) in entry.checks.iter().enumerate() {
      if let Err(answer) = self.holds(index, check) {
        return answer;
      }
    }
    let mut next = self.clone();
    if let Err(answer) = next.apply(entry) {
      return answer;
    }
    next.keep_record(repository, self, entry);
    *self = next;
    Answer::Ok
  }

  /// Applies `entry`'s items in order, each against what the items before it left.
  fn apply(&mut self, entry: &RepositoryEntry) -> Result<(), Answer> {
    for joined in &entry.joined {
      self.join(joined)?;
    }
    for version in &entry.versions {
      self.version(version)?;
    }
    for change in &entry.names {
      self.name(change)?;
    }
    self.names_position += (entry.joined.len() + entry.names.len()) as u64;
    for (index, change) in entry.state.iter().enumerate() {
      if !self.state(change) {
        return Err(Answer::StateChanged(
          u32::try_from(index).unwrap_or(u32::MAX),
        ));
      }
    }
    Ok(())
  }

  /// Keeps the atomic record an accepted `entry` makes, judged from `before`, the state it was applied to: its greens'
  /// spans from their heads before and the state logs it moved; kept when it moved two logs or more, a green among them.
  fn keep_record(&mut self, repository: RepositoryId, before: &Model, entry: &RepositoryEntry) {
    let mut spans: BTreeMap<LogName, LogSpan> = BTreeMap::new();
    for version in &entry.versions {
      let log = green_log(version.green);
      spans
        .entry(log)
        .and_modify(|span| span.to = span.to.max(version.version))
        .or_insert(span(
          log,
          before.head(index_of(version.green)),
          version.version,
        ));
    }
    for log in STATE_LOGS {
      let (from, to) = (before.state_position(log), self.state_position(log));
      if to > from {
        let name = state_log(repository, log);
        spans.insert(name, span(name, from, to));
      }
    }
    let spans: Vec<LogSpan> = spans.into_values().collect();
    let moves_a_green = spans
      .iter()
      .any(|span| matches!(span.log, LogName::Green { .. }));
    let Some(first) = spans.first().copied() else {
      return;
    };
    if spans.len() < 2 || !moves_a_green {
      return;
    }
    let mut pointers = BTreeSet::new();
    for span in &spans {
      if let LogName::Green { green } = span.log {
        for position in span.from + 1..=span.to {
          pointers.insert((index_of(green), position));
        }
      }
    }
    self
      .records
      .insert((first.log, first.to), (AtomicRecord { spans }, pointers));
  }

  /// `green`'s log folded through `floor`: its pointers there go, and each record left with none.
  fn fold_records(&mut self, green: u8, floor: u64) {
    for (_, pointers) in self.records.values_mut() {
      pointers.retain(|(at, position)| *at != green || *position > floor);
    }
    self.records.retain(|_, (_, pointers)| !pointers.is_empty());
  }

  fn join(&mut self, joined: &MemberRecord) -> Result<(), Answer> {
    let volume = index_of(joined.green);
    self.green_here(volume)?;
    if self.is_member(volume) {
      return Err(Answer::AlreadyExists);
    }
    let origin = match joined.origin {
      MemberOrigin::Root if !self.members.is_empty() => return Err(Answer::AlreadyExists),
      MemberOrigin::Root => None,
      MemberOrigin::Branch { from, version } => {
        if !self.is_member(index_of(from)) {
          return Err(Answer::NotFound);
        }
        Some((index_of(from), version))
      }
    };
    self.members.push((volume, origin));
    Ok(())
  }

  fn version(&mut self, version: &GreenVersion) -> Result<(), Answer> {
    let green = index_of(version.green);
    if !self.is_member(green) {
      return Err(Answer::NotFound);
    }
    let at = self.head(green);
    if version.version != at + 1 {
      return Err(Answer::Moved {
        green,
        read: version.version.saturating_sub(1),
        at,
      });
    }
    self
      .logs
      .entry(green)
      .or_default()
      .entries
      .push(version.entry.clone());
    Ok(())
  }

  fn name(&mut self, change: &NameChange) -> Result<(), Answer> {
    let valid = |name: &str| !name.is_empty() && !name.contains('\0');
    match change {
      NameChange::Bind { name, green } => {
        if !valid(name) {
          return Err(Answer::InvalidName);
        }
        if self.names.contains_key(name) {
          return Err(Answer::AlreadyExists);
        }
        let green = index_of(*green);
        self.green_here(green)?;
        if !self.is_member(green) {
          return Err(Answer::NotFound);
        }
        if self.names.values().any(|bound| *bound == green) {
          return Err(Answer::AlreadyExists);
        }
        self.names.insert(name.clone(), green);
        Ok(())
      }
      NameChange::Retire { name } => self.names.remove(name).map(|_| ()).ok_or(Answer::NotFound),
      NameChange::Default { branch } => {
        if branch.as_deref().is_some_and(|branch| !valid(branch)) {
          return Err(Answer::InvalidName);
        }
        self.default.clone_from(branch);
        Ok(())
      }
    }
  }

  fn state(&mut self, change: &StateChange) -> bool {
    let applied = match change {
      StateChange::PhaseRootAdded { phase, node } => self.roots.insert((*phase, node.clone())),
      StateChange::PhaseRootRemoved { phase, node } => self.roots.remove(&(*phase, node.clone())),
      StateChange::MarkerAdded { marker } => {
        if self.markers.contains(marker) {
          return false;
        }
        self.markers.push(marker.clone());
        return true;
      }
      StateChange::LockTaken { path, lock } => {
        if self.locks.contains_key(path) {
          return false;
        }
        self.locks.insert(path.clone(), lock.clone());
        self.locks_position += 1;
        return true;
      }
      StateChange::LockReleased { path, lock } => {
        if self.locks.get(path) != Some(lock) {
          return false;
        }
        self.locks.remove(path);
        self.locks_position += 1;
        return true;
      }
    };
    if applied {
      self.phases_position += 1;
    }
    applied
  }

  /// A member's version alone (`Op::GreenAdvanced`): its log's next entry, no record.
  fn single(&mut self, green: u8, entry: Vec<u8>) {
    self.logs.entry(green).or_default().entries.push(entry);
  }

  /// A checkpoint of `green` at `version`, inside its log: its entries through it fold, and so do its pointers.
  fn checkpoint(&mut self, green: u8, version: u64) {
    if let Some(log) = self.logs.get_mut(&green) {
      let folded = usize::try_from(version - log.floor).unwrap();
      log.entries.drain(..folded);
      log.floor = version;
    }
    self.fold_records(green, version);
  }

  /// A volume's destroy: refused while a member was made from it; its name, membership, log and pointers leave with it,
  /// and the names log moves when it was bound or a member.
  fn destroy(&mut self, volume: u8) -> Answer {
    if !self.alive.contains(&volume) {
      return Answer::NotFound;
    }
    if self
      .members
      .iter()
      .any(|(_, origin)| origin.is_some_and(|(from, _)| from == volume))
    {
      return Answer::BranchedFrom;
    }
    let changed = self.is_member(volume) || self.names.values().any(|bound| *bound == volume);
    self.alive.remove(&volume);
    self.members.retain(|(member, _)| *member != volume);
    self.names.retain(|_, bound| *bound != volume);
    self.logs.remove(&volume);
    self.fold_records(volume, u64::MAX);
    if changed {
      self.names_position += 1;
    }
    Answer::Ok
  }

  /// The entry a step makes, its values read from this state and departing from it as the step's skews say, or, for a
  /// clean step, only its items that hold.
  fn build(&self, repository: RepositoryId, step: &EntryStep) -> RepositoryEntry {
    let made = self.made(repository, step);
    if step.clean {
      return self.cleaned(made);
    }
    made
  }

  /// `entry` with only its items that hold, judged in order on a copy of this state, each version the one after its
  /// green's head: an entry the rules accept whole.
  fn cleaned(&self, entry: RepositoryEntry) -> RepositoryEntry {
    let mut scratch = self.clone();
    let mut kept = at(entry.repository);
    kept.checks = entry
      .checks
      .into_iter()
      .enumerate()
      .filter(|(index, check)| self.holds(*index, check).is_ok())
      .map(|(_, check)| check)
      .collect();
    for joined in entry.joined {
      if scratch.join(&joined).is_ok() {
        kept.joined.push(joined);
      }
    }
    for made in entry.versions {
      let next = version(
        made.green,
        scratch.head(index_of(made.green)) + 1,
        &made.entry,
      );
      if scratch.version(&next).is_ok() {
        kept.versions.push(next);
      }
    }
    for change in entry.names {
      if scratch.name(&change).is_ok() {
        kept.names.push(change);
      }
    }
    for change in entry.state {
      if scratch.state(&change) {
        kept.state.push(change);
      }
    }
    kept
  }

  /// The entry a step makes as it stands.
  fn made(&self, repository: RepositoryId, step: &EntryStep) -> RepositoryEntry {
    let mut heads: BTreeMap<u8, u64> = BTreeMap::new();
    let mut entry = at(repository);
    entry.checks = step
      .checks
      .iter()
      .map(|check| self.check_of(check))
      .collect();
    entry.joined = step
      .joins
      .iter()
      .map(|join| MemberRecord {
        repository,
        green: volume_of(join.volume),
        origin: match join.from {
          None => MemberOrigin::Root,
          Some((from, version)) => MemberOrigin::Branch {
            from: volume_of(self.picked(from)),
            version: u64::from(version),
          },
        },
      })
      .collect();
    for (green, skew) in &step.versions {
      let green = self.picked(*green);
      let next = heads
        .get(&green)
        .copied()
        .unwrap_or_else(|| self.head(green))
        + 1;
      let version_made = skew.apply(next);
      heads.insert(green, version_made);
      let bytes = format!("{green}@{version_made}").into_bytes();
      entry
        .versions
        .push(version(volume_of(green), version_made, &bytes));
    }
    entry.names = step
      .names
      .iter()
      .map(|change| self.name_of_step(change))
      .collect();
    entry.state = step.state.iter().map(state_of_step).collect();
    entry
  }

  fn check_of(&self, check: &CheckStep) -> RepositoryCheck {
    match check {
      CheckStep::Head { green, skew } => {
        let green = self.picked(*green);
        head(volume_of(green), skew.apply(self.head(green)))
      }
      CheckStep::Names { prefix, skew } => {
        let prefix = PREFIXES[usize::from(*prefix) % PREFIXES.len()];
        let mut bound = self.under(prefix);
        match skew {
          Skew::Exact => {}
          Skew::Below => {
            bound.pop();
          }
          Skew::Above => bound.push(NameBinding {
            name: format!("{prefix}~"),
            green: volume_of(0),
          }),
        }
        RepositoryCheck::Names {
          prefix: prefix.to_owned(),
          bound,
        }
      }
      CheckStep::Log { log, skew } => {
        let log = STATE_LOGS[usize::from(*log) % STATE_LOGS.len()];
        RepositoryCheck::Log {
          log,
          position: skew.apply(self.state_position(log)),
        }
      }
    }
  }

  fn name_of_step(&self, change: &NameStep) -> NameChange {
    match change {
      NameStep::Bind { name, green } => bind(&name_of(*name), volume_of(self.picked(*green))),
      NameStep::Retire { name } => retire(&name_of(*name)),
      NameStep::Default { name } => NameChange::Default {
        branch: name.map(name_of),
      },
    }
  }
}

fn state_of_step(change: &StateStep) -> StateChange {
  match change {
    StateStep::Add { phase, node } => root_added(phase + 1, *node),
    StateStep::Remove { phase, node } => root_removed(phase + 1, *node),
    StateStep::Marker { marker: n } => marker(format!("marker-{n}").as_bytes()),
    StateStep::Lock { path, lock } => locked(&format!("p{path}"), &[*lock]),
    StateStep::Unlock { path, lock } => unlocked(&format!("p{path}"), &[*lock]),
  }
}

/// What the partition holds of the repository, in the model's terms.
fn observed(partition: &Partition, repository: RepositoryId) -> Model {
  let repositories = partition.repositories();
  let position = |log| {
    repositories
      .log_position(repository, log)
      .unwrap_or(u64::MAX)
  };
  let records = repositories
    .atomic_records()
    .map(|record| {
      let first = record.spans[0];
      let pointers = record
        .spans
        .iter()
        .filter_map(|span| match span.log {
          LogName::Green { green } => Some((green, span)),
          LogName::State { .. } => None,
        })
        .flat_map(|(green, span)| (span.from + 1..=span.to).map(move |at| (green, at)))
        .filter(|(green, at)| repositories.atomic_at(*green, *at) == Some(record))
        .map(|(green, at)| (index_of(green), at))
        .collect();
      ((first.log, first.to), (record.clone(), pointers))
    })
    .collect();
  Model {
    alive: (0..VOLUMES)
      .filter(|index| partition.volume(volume_of(*index)).is_some())
      .collect(),
    members: repositories
      .members_of(repository)
      .map(|member| {
        let origin = match member.origin {
          MemberOrigin::Root => None,
          MemberOrigin::Branch { from, version } => Some((index_of(from), version)),
        };
        (index_of(member.green), origin)
      })
      .collect(),
    names: repositories
      .branches(repository, "")
      .map(|(name, green)| (name.to_owned(), index_of(green)))
      .collect(),
    default: repositories
      .repository(repository)
      .and_then(|record| record.default_branch.clone()),
    logs: (0..GREENS)
      .map(|index| {
        let green = volume_of(index);
        let log = GreenLog {
          floor: partition.green_floor(green),
          entries: partition.green_chain(green).to_vec(),
        };
        (index, log)
      })
      .filter(|(_, log)| log.head() > 0)
      .collect(),
    names_position: position(StateLog::Names),
    phases_position: position(StateLog::Phases),
    locks_position: position(StateLog::Locks),
    roots: repositories
      .phase_roots(repository)
      .map(|(phase, node)| (phase, node.to_vec()))
      .collect(),
    markers: repositories
      .markers(repository)
      .map(<[u8]>::to_vec)
      .collect(),
    locks: repositories
      .locks(repository)
      .map(|(path, lock)| (path.to_owned(), lock.to_vec()))
      .collect(),
    records,
  }
}

fn segment(name: &str) -> AnchorSegment {
  let identity = Identity {
    cpu: "test".into(),
    os: "test".into(),
    arch: "test".into(),
    cores: 4,
    memory: 1,
    page: PAGE,
  };
  AnchorSegment::create(
    name,
    &identity,
    Geometry {
      partitions: 1,
      page: PAGE,
      profile_bytes: PAGE,
      log_bytes: SEGMENT_BYTES,
      snapshot_bytes: SEGMENT_BYTES,
      landing_slots: 1,
      landing_slot_bytes: PAGE,
    },
  )
  .unwrap()
}

/// A refusal where the history expects none, as the case's failure.
fn failed(error: impl std::fmt::Debug) -> TestCaseError {
  TestCaseError::fail(format!("{error:?}"))
}

/// What the generated histories met, across every case: the model test's non-vacuity counters, so a generator that
/// stopped making the steps that matter fails rather than passes on trivial histories.
#[derive(Debug, Default)]
struct Census {
  /// Accepted entries committing versions of two greens or more: atomic records over greens.
  several_greens: u64,
  /// Single-green appends accepted after an atomic record of the same history: pushes interleaved with records.
  interleaved_singles: u64,
  /// Accepted entries moving the repository's state with versions, and alone.
  state_with_versions: u64,
  /// See [`Census::state_with_versions`].
  state_alone: u64,
  /// Entries refused `Moved`, naming the green that moved: StaleNeverCommits met.
  stale_refused: u64,
  /// Entries refused by a check, by a state change, and by a rule of the lineage or the namespace.
  check_failed: u64,
  /// See [`Census::check_failed`].
  state_changed: u64,
  /// See [`Census::check_failed`].
  refused_by_rule: u64,
  /// Checkpoints and destroys that let an atomic record go, and destroys of members.
  records_folded: u64,
  /// See [`Census::records_folded`].
  member_destroyed: u64,
  /// Recoveries from a snapshot and the log after it, and from the log alone, with an atomic record held.
  recovered_from_snapshot: u64,
  /// See [`Census::recovered_from_snapshot`].
  recovered_from_log: u64,
  /// State-log records the database handed on, each taken by the replica (A-636 part 2b).
  state_log_records: u64,
  /// Operations whose state-log records name two logs or more: records a takeover keeps whole or drops.
  several_logs: u64,
  /// Members that left their repository, destroyed, as the names log's records carry them.
  left: u64,
}

impl Census {
  /// Counts an entry and what it was answered.
  fn entry(&mut self, entry: &RepositoryEntry, answer: &Answer) {
    let greens: BTreeSet<VolumeId> = entry.versions.iter().map(|version| version.green).collect();
    match answer {
      Answer::Ok => {
        self.several_greens += u64::from(greens.len() > 1);
        let moves_state = !entry.state.is_empty();
        self.state_with_versions += u64::from(moves_state && !entry.versions.is_empty());
        self.state_alone += u64::from(moves_state && entry.versions.is_empty());
      }
      Answer::Moved { .. } => self.stale_refused += 1,
      Answer::CheckFailed(_) => self.check_failed += 1,
      Answer::StateChanged(_) => self.state_changed += 1,
      _ => self.refused_by_rule += 1,
    }
  }

  /// Counts what one take of state-log records met.
  fn took(&mut self, taken: replica::Taken) {
    self.state_log_records += taken.records;
    self.several_logs += taken.several_logs;
    self.left += taken.left;
  }

  fn met(&self) -> [u64; 15] {
    [
      self.state_log_records,
      self.several_logs,
      self.left,
      self.several_greens,
      self.interleaved_singles,
      self.state_with_versions,
      self.state_alone,
      self.stale_refused,
      self.check_failed,
      self.state_changed,
      self.refused_by_rule,
      self.records_folded,
      self.member_destroyed,
      self.recovered_from_snapshot,
      self.recovered_from_log,
    ]
  }
}

/// One history's database, model and clock.
struct History {
  seg: AnchorSegment,
  db: crate::replay::Db,
  model: Model,
  repo: RepositoryId,
  now_ns: u64,
  /// A holder's copy of the state logs, built from the records the database handed on (A-636 part 2b).
  replica: replica::Replica,
}

/// The policy a history's database snapshots by.
const POLICY: SnapshotPolicy = SnapshotPolicy {
  bytes_between_snapshots: SNAPSHOT_EVERY,
};

impl History {
  /// A database over its own segment holding the volumes and the repository, its root `n0` joined and bound.
  fn start() -> Result<History, TestCaseError> {
    let mut seg = segment(&format!("slates-db-a635-{}", std::process::id()));
    let (mut db, _) = recover(&mut seg, 0, caps(ROOMY), 0).map_err(failed)?;
    db.set_policy(POLICY);
    let repo = repository_id(9);
    let mut model = Model::default();
    for index in 0..UNKNOWN {
      let record = volume(volume_of(index), index != PLAIN);
      db.mutate(&mut seg, &Op::VolumeCreated { record }, 0)
        .map_err(failed)?;
      model.alive.insert(index);
    }
    db.mutate(
      &mut seg,
      &Op::RepositoryCreated {
        record: repository(9, "linux"),
      },
      0,
    )
    .map_err(failed)?;
    let mut first = at(repo);
    first.joined = vec![member(repo, volume_of(0), MemberOrigin::Root)];
    first.names = vec![bind("n0", volume_of(0))];
    prop_assert_eq!(model.advance(repo, &first), Answer::Ok);
    db.mutate(&mut seg, &Op::RepositoryAdvanced { entry: first }, 0)
      .map_err(failed)?;
    let mut replica = replica::Replica::default();
    replica.take(db.take_state_log_records())?;
    Ok(History {
      seg,
      db,
      model,
      repo,
      now_ns: 0,
      replica,
    })
  }

  /// The records the database handed on since the last step, taken by the replica, which must then hold every state
  /// log as the model does (A-636 part 2b): what a successor rebuilds from the holders' copies.
  fn replicate(&mut self, census: &mut Census) -> Result<(), TestCaseError> {
    let taken = self.replica.take(self.db.take_state_log_records())?;
    census.took(taken);
    prop_assert_eq!(
      self.replica.view(),
      replica::StateView::of(&self.model),
      "the state logs' records rebuild the logs"
    );
    Ok(())
  }

  fn mutate(&mut self, op: &Op) -> Answer {
    answer_of(self.db.mutate(&mut self.seg, op, self.now_ns))
  }

  /// Runs one step against the database and the model; the answers must agree.
  fn step(&mut self, step: &Step, census: &mut Census) -> Result<(), TestCaseError> {
    self.now_ns += 1;
    match step {
      Step::Advance(made) => {
        let entry = self.model.build(self.repo, made);
        let expected = self.model.advance(self.repo, &entry);
        census.entry(&entry, &expected);
        let real = self.mutate(&Op::RepositoryAdvanced { entry });
        prop_assert_eq!(real, expected, "step {:?}", step);
      }
      Step::Single { green } => self.single(*green, census)?,
      Step::Checkpoint { green, back } => self.checkpoint(*green, *back, census)?,
      Step::Destroy { volume } => {
        let member = self.model.is_member(*volume);
        let records = self.model.records.len();
        let expected = self.model.destroy(*volume);
        census.member_destroyed += u64::from(member && expected == Answer::Ok);
        census.records_folded += u64::from(self.model.records.len() < records);
        let real = self.mutate(&Op::VolumeDestroyed {
          id: volume_of(*volume),
        });
        prop_assert_eq!(real, expected, "step {:?}", step);
      }
      Step::Snapshot => self.db.snapshot(&mut self.seg).map_err(failed)?,
      Step::Crash => self.crash(census)?,
    }
    Ok(())
  }

  /// A member's version alone.
  fn single(&mut self, green: u8, census: &mut Census) -> Result<(), TestCaseError> {
    let green = self.model.picked(Pick::Member(green));
    if !self.model.is_member(green) {
      return Ok(());
    }
    let bytes = format!("single-{green}@{}", self.model.head(green) + 1).into_bytes();
    self.model.single(green, bytes.clone());
    census.interleaved_singles += u64::from(!self.model.records.is_empty());
    let real = self.mutate(&Op::GreenAdvanced {
      green: volume_of(green),
      increment: bytes,
    });
    prop_assert_eq!(real, Answer::Ok);
    Ok(())
  }

  /// A member's checkpoint inside its log, when it has one to fold.
  fn checkpoint(&mut self, green: u8, back: u8, census: &mut Census) -> Result<(), TestCaseError> {
    let green = self.model.picked(Pick::Member(green));
    let Some(log) = self.model.logs.get(&green).cloned() else {
      return Ok(());
    };
    if log.entries.is_empty() {
      return Ok(());
    }
    let version = log
      .head()
      .saturating_sub(u64::from(back) % log.entries.len() as u64);
    let records = self.model.records.len();
    self.model.checkpoint(green, version);
    census.records_folded += u64::from(self.model.records.len() < records);
    let real = self.mutate(&Op::GreenCheckpointed {
      green: volume_of(green),
      version,
      checkpoint: format!("checkpoint-{green}@{version}").into_bytes(),
    });
    prop_assert_eq!(real, Answer::Ok);
    Ok(())
  }

  /// The database dropped and recovered from its segment: NoLoss, the recovered partition equal to the live one whole.
  fn crash(&mut self, census: &mut Census) -> Result<(), TestCaseError> {
    let live = self.db.partition().to_snapshot(0);
    let seq = self.db.next_seq();
    // The live database's memory is lost: only the segment is read back.
    let (recovered, stats) = recover(&mut self.seg, 0, caps(ROOMY), self.now_ns).map_err(failed)?;
    drop(std::mem::replace(&mut self.db, recovered));
    prop_assert_eq!(
      self.db.partition().to_snapshot(0),
      live,
      "recovery differs from the live partition"
    );
    prop_assert_eq!(self.db.next_seq(), seq);
    prop_assert!(!stats.torn, "a clean crash has no torn tail");
    let held = !self.model.records.is_empty();
    if stats.snapshot.is_some() {
      census.recovered_from_snapshot += u64::from(held);
    } else {
      census.recovered_from_log += u64::from(held);
    }
    self.db.set_policy(POLICY);
    Ok(())
  }
}

/// Runs a generated history through a database over its own segment: every answer and the whole repository equal the
/// model's after every step (Agreement, StaleNeverCommits); at every crash the database is dropped and recovered from
/// the segment, and the recovered partition equals the live one whole (NoLoss).
fn every_history_equals_the_rules_and_survives_every_crash(
  steps: &[Step],
  census: &mut Census,
) -> Result<(), TestCaseError> {
  let mut history = History::start()?;
  for step in steps {
    history.step(step, census)?;
    prop_assert_eq!(
      observed(history.db.partition(), history.repo),
      history.model.clone(),
      "after {:?}",
      step
    );
    history.replicate(census)?;
  }
  Ok(())
}

/// A-603, A-635 (A-642's requirement 4; the pattern of `crates/db/src/partition.rs` and `crates/db/tests/model.rs`).
/// Do: run generated histories over one repository through a database over a real anchor segment: entries joining
/// members, committing versions of one or several greens (some made against positions their greens moved past),
/// changing names and the default, moving phase roots, markers and locks, with checks of heads, of names under
/// prefixes and of state logs' positions; single members' versions between them; checkpoints; destroys of any volume;
/// snapshots; and crashes, at each of which the database is dropped and recovered from its segment. Expect: every
/// answer and the whole repository after every step (each green's log, its floor and entries; each state log's position
/// and what it holds; the namespace and the lineage; every atomic record and the positions pointing to it) equal the
/// design's rules kept over plain maps, where an entry is all or none by construction (Agreement), an entry made
/// against a position its green moved past is refused naming it and nothing of it lands (StaleNeverCommits), and every
/// recovered partition equals the live one whole (NoLoss); a holder's copy of the state logs built from the records the
/// database handed on, each continuing its log and naming its operation alike on every log, equals the model's state
/// logs after every step (A-636 part 2b: what a successor rebuilds from); the census shows each kind of record,
/// single-green appends interleaved with records, refusals, folds, destroys and recoveries, state-log records, records
/// of several logs and members leaving were met.
#[test]
fn every_history_of_entries_equals_the_rules_and_every_crash_recovers_it() {
  let census = RefCell::new(Census::default());
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(Config {
    cases: slates_test_seeds::cases(128),
    ..Config::default()
  }));
  let result = runner.run(&proptest::collection::vec(step(), 0..48), |steps| {
    every_history_equals_the_rules_and_survives_every_crash(&steps, &mut census.borrow_mut())
  });
  if let Err(failure) = result {
    panic!("{failure}");
  }
  let census = census.into_inner();
  println!("entry model census {census:?}");
  assert!(
    census.met().iter().all(|&count| count > 0),
    "a kind of step never met: {census:?}"
  );
}

/// The record of `log` among `records`, if one is.
fn record_of(
  records: &[crate::catalog::StateLogRecord],
  log: StateLog,
) -> Option<&crate::catalog::StateLogRecord> {
  records.iter().find(|record| record.log == log)
}

/// `record`'s log, span and changes, as the test names them.
fn spanned(record: &crate::catalog::StateLogRecord) -> (StateLog, u64, u64, usize) {
  (record.log, record.from, record.to, record.changes.len())
}

/// A-636 part 2b (§4.17.1 shared piece 2, the fleet half). Do: on a repository whose first entry joined three greens and
/// bound three names, take the records each operation made: that entry's; a push of two greens' versions with a name
/// retired, a phase root, a marker and a lock; a name bound alone; the default moved alone; a member's version alone; a
/// member destroyed; and a lock released alone. Expect: the first entry one record of the names log, 0 to 6, its joins
/// then its binds; the push one record of each state log it moved, in log order, each continuing its log, all naming
/// one identity and every span the push moved, its two greens' included; each change alone one record of its log naming
/// that operation alone; the version alone none; the destroy one record of the names log, the member leaving; and no
/// two operations naming one identity.
#[test]
fn each_operation_records_what_it_moved_of_each_state_log_named_alike() {
  let (mut partition, repo, [a, b, c, ..]) = three_branches(caps(ROOMY));
  let first = partition.take_state_log_records();
  assert_the_create_records_its_joins_then_its_binds(&first, repo);
  let mut push = at(repo);
  push.versions = vec![version(a, 1, b"a1"), version(b, 1, b"b1")];
  push.names = vec![retire("other")];
  push.state = vec![root_added(1, 7), marker(b"m"), locked("p", b"l")];
  advance(&mut partition, push).unwrap();
  let pushed = partition.take_state_log_records();
  assert_a_push_names_itself_on_every_log_it_moved(&pushed, repo, [a, b]);
  let mut identities: BTreeSet<[u8; 32]> = [first[0].identity, pushed[0].identity].into();
  let bound = Op::RepositoryBranchAdded {
    record: crate::catalog::BranchRecord {
      repository: repo,
      name: "other".to_owned(),
      green: c,
    },
  };
  let default = Op::RepositoryDefaultSet {
    repository: repo,
    branch: Some("topic".to_owned()),
  };
  for (op, expected) in [
    (bound, (StateLog::Names, 7, 8, 1)),
    (default, (StateLog::Names, 8, 9, 1)),
    (Op::VolumeDestroyed { id: c }, (StateLog::Names, 9, 10, 1)),
  ] {
    run(&mut partition, &op).unwrap();
    let records = partition.take_state_log_records();
    assert_eq!(
      records.iter().map(spanned).collect::<Vec<_>>(),
      vec![expected]
    );
    assert!(identities.insert(records[0].identity), "{op:?}");
  }
  run(
    &mut partition,
    &Op::GreenAdvanced {
      green: a,
      increment: b"a2".to_vec(),
    },
  )
  .unwrap();
  assert!(
    partition.take_state_log_records().is_empty(),
    "a version alone moves no state log"
  );
  let mut release = at(repo);
  release.state = vec![unlocked("p", b"l")];
  advance(&mut partition, release).unwrap();
  let released = partition.take_state_log_records();
  assert_eq!(
    released.iter().map(spanned).collect::<Vec<_>>(),
    vec![(StateLog::Locks, 1, 2, 1)]
  );
  assert!(identities.insert(released[0].identity));
}

/// The create's records: one of the names log, positions 0 to 6, its three joins then its three binds.
fn assert_the_create_records_its_joins_then_its_binds(
  first: &[crate::catalog::StateLogRecord],
  repo: RepositoryId,
) {
  use crate::catalog::LoggedChange;
  assert_eq!(
    first.iter().map(spanned).collect::<Vec<_>>(),
    vec![(StateLog::Names, 0, 6, 6)]
  );
  assert_eq!(
    first[0].spans,
    vec![span(state_log(repo, StateLog::Names), 0, 6)]
  );
  assert!(matches!(
    first[0].changes.as_slice(),
    [
      LoggedChange::Joined { .. },
      _,
      _,
      LoggedChange::Named { .. },
      _,
      _
    ]
  ));
}

/// The push of [`each_operation_records_what_it_moved_of_each_state_log_named_alike`]: one record of each state log it
/// moved, in log order, each its log's changes alone, all naming the push's identity and its every span.
fn assert_a_push_names_itself_on_every_log_it_moved(
  pushed: &[crate::catalog::StateLogRecord],
  repo: RepositoryId,
  [a, b]: [VolumeId; 2],
) {
  assert_eq!(
    pushed.iter().map(spanned).collect::<Vec<_>>(),
    vec![
      (StateLog::Names, 6, 7, 1),
      (StateLog::Phases, 0, 1, 1),
      (StateLog::Markers, 0, 1, 1),
      (StateLog::Locks, 0, 1, 1),
    ]
  );
  let spans = vec![
    span(green_log(a), 0, 1),
    span(green_log(b), 0, 1),
    span(state_log(repo, StateLog::Names), 6, 7),
    span(state_log(repo, StateLog::Phases), 0, 1),
    span(state_log(repo, StateLog::Markers), 0, 1),
    span(state_log(repo, StateLog::Locks), 0, 1),
  ];
  for record in pushed {
    assert_eq!(record.spans, spans, "{record:?}");
    assert_eq!(record.identity, pushed[0].identity, "{record:?}");
  }
  assert_eq!(
    record_of(pushed, StateLog::Phases).map(|record| record.changes.clone()),
    Some(vec![crate::catalog::LoggedChange::Changed {
      change: root_added(1, 7)
    }])
  );
}

/// A-636 part 2b with AC-2.3's two failure classes. Do: through a database over a real segment, record a name bound
/// alone; then in a transaction a name retired whose record the segment refuses (it rolls back); then the same in a
/// transaction that commits. Expect: the database hands on the lone change's record once; nothing of the rolled-back
/// transaction, ever; and the committed transaction's record once, after its commit, none before it.
#[test]
fn a_transactions_state_log_records_go_on_only_once_it_is_durable() {
  let mut seg = segment(&format!("slates-db-a636-{}", std::process::id()));
  let (mut db, _) = recover(&mut seg, 0, caps(ROOMY), 0).unwrap();
  let [a, b] = [volume_of(0), volume_of(1)];
  for id in [a, b] {
    let record = volume(id, true);
    db.mutate(&mut seg, &Op::VolumeCreated { record }, 0)
      .unwrap();
  }
  let record = repository(9, "linux");
  let repo = record.id;
  db.mutate(&mut seg, &Op::RepositoryCreated { record }, 0)
    .unwrap();
  let mut first = at(repo);
  first.joined = vec![
    member(repo, a, MemberOrigin::Root),
    member(
      repo,
      b,
      MemberOrigin::Branch {
        from: a,
        version: 0,
      },
    ),
  ];
  first.names = vec![bind("main", a), bind("topic", b)];
  db.mutate(&mut seg, &Op::RepositoryAdvanced { entry: first }, 0)
    .unwrap();
  let alone = db.take_state_log_records();
  assert_eq!(
    alone.iter().map(spanned).collect::<Vec<_>>(),
    vec![(StateLog::Names, 0, 4, 4)]
  );
  assert!(db.take_state_log_records().is_empty(), "handed on once");
  let retired = Op::RepositoryBranchRetired {
    repository: repo,
    name: "topic".to_owned(),
  };
  db.begin();
  db.mutate(&mut seg, &retired, 0).unwrap();
  db.inject_publication_fault(Some(crate::replay::PublicationFault::BeforeAppend));
  assert!(
    db.commit(&mut seg).is_err(),
    "the segment refused the record"
  );
  assert!(
    db.take_state_log_records().is_empty(),
    "nothing of a rolled-back transaction"
  );
  db.begin();
  db.mutate(&mut seg, &retired, 0).unwrap();
  assert!(
    db.take_state_log_records().is_empty(),
    "nothing before the commit"
  );
  db.commit(&mut seg).unwrap();
  let committed = db.take_state_log_records();
  assert_eq!(
    committed.iter().map(spanned).collect::<Vec<_>>(),
    vec![(StateLog::Names, 4, 5, 1)]
  );
  assert!(db.take_state_log_records().is_empty(), "handed on once");
}
