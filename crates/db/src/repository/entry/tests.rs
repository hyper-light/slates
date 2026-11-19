//! A-603's tests (§4.17.1 shared piece 2): an entry of a repository's register, driven through the partition's guard
//! and apply, and through a database over a real anchor segment. The model test runs generated histories of entries,
//! destroys, snapshots and crashes against the design's rules kept over plain maps, the database dropped and recovered
//! from its segment at random points.

// proptest's strategy types carry `Arc` (D-8's harness exception).
#![allow(clippy::disallowed_types)]

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use slates_anchor::{AnchorSegment, Geometry};
use slates_machine::facts::Identity;

use crate::catalog::{
  BaseRecord, GreenVersion, MemberOrigin, MemberRecord, NameBinding, NameChange, NamePolicy,
  ObjectFormat, PolicyRecord, Principal, RepositoryCheck, RepositoryEntry, RepositoryId,
  RepositoryRecord, Role, SizeClass, SnapshotId, StateChange, VcsTool, VolumeId, VolumeRecord,
  VolumeState,
};
use crate::error::DbError;
use crate::op::Op;
use crate::partition::{Partition, PartitionCaps};
use crate::replay::{SnapshotPolicy, recover};

/// Shape: the partition the tests' repositories live on.
const HOME: u16 = 3;
/// Shape: another partition, whose green no entry of a repository on `HOME` may take.
const ELSEWHERE: u16 = 4;
/// Shape: a merge budget no test here reaches, but the one that measures the budget.
const ROOMY: usize = 1 << 20;

/// An id on `partition` with `serial` in its low byte, as the server mints them.
fn id_bytes(partition: u16, serial: u8) -> [u8; 16] {
  let mut bytes = [0u8; 16];
  bytes[8..10].copy_from_slice(&partition.to_be_bytes());
  bytes[15] = serial;
  bytes
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
    id: repository_id(serial),
    name: name.to_owned(),
    tool: VcsTool::Mercurial,
    format: ObjectFormat::Sha1,
    default_branch: None,
    owner: owner(),
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

/// An entry of `repository`'s register at `position`, changing nothing yet.
fn at(repository: RepositoryId, position: u64) -> RepositoryEntry {
  RepositoryEntry {
    repository,
    position,
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

fn chain(partition: &Partition, green: VolumeId) -> Vec<Vec<u8>> {
  partition.green_chain(green).to_vec()
}

fn position(partition: &Partition, repository: RepositoryId) -> Option<u64> {
  partition.repositories().position(repository)
}

/// A partition rebuilt from `partition`'s snapshot answers as `partition` does, whole.
fn assert_snapshot_rebuilds(partition: &Partition, caps: PartitionCaps) {
  let snapshot = partition.to_snapshot(0);
  let rebuilt = Partition::from_snapshot(&snapshot, caps, 0).unwrap();
  assert_eq!(rebuilt.to_snapshot(0), snapshot);
}

/// A repository on `HOME` whose lineage holds a root `a` and branches `b` and `c` made from it, bound to `main`, `topic`
/// and `other` by its register's first entry, and beside them a plain volume and a green of another partition: the
/// partition, the repository, and `[a, b, c, plain, foreign]`.
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
  let mut first = at(repo, 0);
  first.joined = vec![
    member(repo, a, MemberOrigin::Root),
    member(repo, b, made_from_a),
    member(repo, c, made_from_a),
  ];
  first.names = vec![bind("main", a), bind("topic", b), bind("other", c)];
  advance(&mut partition, first).unwrap();
  (partition, repo, ids)
}

/// A-603 (§4.17.1 item 2: git's atomic push, "Either all refs are updated or none", as one register append). Do: commit
/// versions of two of a repository's three greens in one entry; then send an entry whose second version skips one, one
/// made for a stale position, and entries naming a green outside the lineage; then land two versions of one green in one
/// entry (a stack landed whole). Expect: the first entry moves both chains and the register one position, the third
/// green untouched; each refused entry is refused by its kind (`OutOfOrder` naming what it named and what was next,
/// `NotFound` for a green the lineage does not hold) with every chain and the position as they were; the stack lands
/// both versions in order; a partition rebuilt from its snapshot answers the same.
#[test]
fn an_entry_commits_several_greens_versions_together_or_none_of_them() {
  let (mut partition, repo, [a, b, c, plain, foreign]) = three_branches(caps(ROOMY));
  assert_eq!(position(&partition, repo), Some(1));
  let mut push = at(repo, 1);
  push.versions = vec![version(a, 1, b"a1"), version(b, 1, b"b1")];
  advance(&mut partition, push).unwrap();
  assert_eq!(chain(&partition, a), vec![b"a1".to_vec()]);
  assert_eq!(chain(&partition, b), vec![b"b1".to_vec()]);
  assert!(chain(&partition, c).is_empty());
  assert_eq!(position(&partition, repo), Some(2));
  let before = partition.to_snapshot(0);
  let mut skipping = at(repo, 2);
  skipping.versions = vec![version(a, 2, b"a2"), version(b, 3, b"b3")];
  assert_eq!(
    advance(&mut partition, skipping),
    Err(DbError::OutOfOrder {
      named: 3,
      expected: 2
    })
  );
  let mut stale = at(repo, 1);
  stale.versions = vec![version(c, 1, b"c1")];
  assert_eq!(
    advance(&mut partition, stale),
    Err(DbError::OutOfOrder {
      named: 1,
      expected: 2
    })
  );
  for outsider in [plain, foreign, green(99)] {
    let mut outside = at(repo, 2);
    outside.versions = vec![version(c, 1, b"c1"), version(outsider, 1, b"x")];
    assert_eq!(
      advance(&mut partition, outside),
      Err(DbError::NotFound),
      "{outsider:?}"
    );
  }
  assert_eq!(
    partition.to_snapshot(0),
    before,
    "a refused entry applied something"
  );
  let mut stack = at(repo, 2);
  stack.versions = vec![version(a, 2, b"a2"), version(a, 3, b"a3")];
  advance(&mut partition, stack).unwrap();
  assert_eq!(
    chain(&partition, a),
    vec![b"a1".to_vec(), b"a2".to_vec(), b"a3".to_vec()]
  );
  assert_eq!(partition.green_head(a), 3);
  assert_eq!(position(&partition, repo), Some(3));
  assert_snapshot_rebuilds(&partition, caps(ROOMY));
}

/// A-603 (§4.17.3 decision 6: `check:heads` and `unbundle`'s heads, every head unchanged, checked in the push's own
/// register step). Do: commit an entry checking every name of the repository and an unmoved green's head; move that green
/// in another entry and send the first entry's checks again; retire a name and check the names under prefixes; check a
/// green's head at the version the entry itself makes and at the one before it; check heads of greens outside the
/// repository; bind a name under a checked prefix. Expect: the checks hold at first; after the move the entry is
/// refused `CheckFailed` naming the check's index, nothing of it applied, its retire included; a prefix's check fails
/// once a name under it is retired or added and holds while names elsewhere change; a check reads the position before
/// the entry's own changes; a head outside the repository never holds.
#[test]
fn checks_read_the_register_at_the_entrys_position_and_refuse_the_whole_entry() {
  let (mut partition, repo, [a, b, c, plain, _]) = three_branches(caps(ROOMY));
  let every = || names("", &[("main", a), ("other", c), ("topic", b)]);
  let mut push = at(repo, 1);
  push.checks = vec![every(), head(c, 0)];
  push.versions = vec![version(a, 1, b"a1")];
  advance(&mut partition, push).unwrap();
  let mut other = at(repo, 2);
  other.versions = vec![version(c, 1, b"c1")];
  advance(&mut partition, other).unwrap();
  let before = partition.to_snapshot(0);
  let mut late = at(repo, 3);
  late.checks = vec![every(), head(c, 0)];
  late.versions = vec![version(a, 2, b"a2")];
  late.names = vec![retire("topic")];
  assert_eq!(
    advance(&mut partition, late),
    Err(DbError::CheckFailed { index: 1 })
  );
  assert_eq!(partition.to_snapshot(0), before);
  let mut retiring = at(repo, 3);
  retiring.names = vec![retire("topic")];
  advance(&mut partition, retiring).unwrap();
  let mut named = at(repo, 4);
  named.checks = vec![names("t", &[("topic", b)])];
  assert_eq!(
    advance(&mut partition, named),
    Err(DbError::CheckFailed { index: 0 })
  );
  let mut narrow = at(repo, 4);
  narrow.checks = vec![names("o", &[("other", c)]), head(c, 1)];
  narrow.versions = vec![version(a, 2, b"a2")];
  advance(&mut partition, narrow).unwrap();
  let mut own = at(repo, 5);
  own.checks = vec![head(a, 3)];
  own.versions = vec![version(a, 3, b"a3")];
  assert_eq!(
    advance(&mut partition, own.clone()),
    Err(DbError::CheckFailed { index: 0 })
  );
  own.checks = vec![head(a, 2)];
  advance(&mut partition, own).unwrap();
  let elsewhere = second_repository_with_root(&mut partition, green(6));
  for outsider in [plain, elsewhere, green(99)] {
    let mut outside = at(repo, 6);
    outside.checks = vec![head(outsider, 0)];
    assert_eq!(
      advance(&mut partition, outside),
      Err(DbError::CheckFailed { index: 0 }),
      "{outsider:?}"
    );
  }
  let mut adding = at(repo, 6);
  adding.names = vec![bind("topic", b)];
  adding.checks = vec![names("", &[("main", a), ("other", c)])];
  advance(&mut partition, adding).unwrap();
  let mut stale = at(repo, 7);
  stale.checks = vec![names("o", &[("other", c)]), every()];
  advance(&mut partition, stale.clone()).unwrap();
  stale.position = 8;
  stale.checks = vec![names("", &[("main", a), ("other", c)])];
  assert_eq!(
    advance(&mut partition, stale),
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
  let mut first = at(second, 0);
  first.joined = vec![member(second, root, MemberOrigin::Root)];
  advance(partition, first).unwrap();
  root
}

/// A-603 (§4.17.3 decisions 3 and 4: phase roots and obsolescence markers commit in the step of the push that moves
/// them, or alone). Do: commit a version with a phase root and two markers; move phases alone (a root removed, two
/// added); send entries, each with a version, whose state change was made against another state: a root added that is
/// one, a root removed that is not, a held marker, a marker twice, a root added, removed and removed again; then add a
/// marker. Expect: the roots read back in (phase, node) order and the markers in the order received; each wrong change
/// refuses its entry `StateChanged` naming its index, nothing applied, the version included; a partition rebuilt from
/// its snapshot answers the same.
#[test]
fn the_repositorys_state_moves_with_its_push_or_alone_and_only_from_the_state_it_was_made_against()
{
  let (mut partition, repo, [a, ..]) = three_branches(caps(ROOMY));
  let roots = |partition: &Partition| -> Vec<(u8, Vec<u8>)> {
    partition
      .repositories()
      .phase_roots(repo)
      .map(|(phase, node)| (phase, node.to_vec()))
      .collect()
  };
  let markers = |partition: &Partition| -> Vec<Vec<u8>> {
    partition
      .repositories()
      .markers(repo)
      .map(<[u8]>::to_vec)
      .collect()
  };
  let mut push = at(repo, 1);
  push.versions = vec![version(a, 1, b"a1")];
  push.state = vec![root_added(1, 1), marker(b"m1"), marker(b"m2")];
  advance(&mut partition, push).unwrap();
  assert_eq!(roots(&partition), vec![(1, vec![1; 20])]);
  assert_eq!(markers(&partition), vec![b"m1".to_vec(), b"m2".to_vec()]);
  let mut phases = at(repo, 2);
  phases.state = vec![root_removed(1, 1), root_added(1, 2), root_added(2, 1)];
  advance(&mut partition, phases).unwrap();
  assert_eq!(roots(&partition), vec![(1, vec![2; 20]), (2, vec![1; 20])]);
  assert_eq!(
    chain(&partition, a).len(),
    1,
    "a phase move alone moved a green"
  );
  assert_eq!(position(&partition, repo), Some(3));
  let before = partition.to_snapshot(0);
  for (state, index) in [
    (vec![root_added(1, 2)], 0),
    (vec![marker(b"m3"), root_removed(1, 1)], 1),
    (vec![marker(b"m1")], 0),
    (vec![marker(b"m3"), marker(b"m3")], 1),
    (
      vec![root_added(1, 3), root_removed(1, 3), root_removed(1, 3)],
      2,
    ),
  ] {
    let mut wrong = at(repo, 3);
    wrong.versions = vec![version(a, 2, b"a2")];
    wrong.state = state;
    assert_eq!(
      advance(&mut partition, wrong),
      Err(DbError::StateChanged { index }),
    );
  }
  assert_eq!(
    partition.to_snapshot(0),
    before,
    "a refused entry applied something"
  );
  let mut later = at(repo, 3);
  later.state = vec![marker(b"m0")];
  advance(&mut partition, later).unwrap();
  assert_eq!(
    markers(&partition),
    vec![b"m1".to_vec(), b"m2".to_vec(), b"m0".to_vec()]
  );
  assert_snapshot_rebuilds(&partition, caps(ROOMY));
}

/// A-603 (bounded growth: an entry's versions and the repository's state are charged to the partition's merge budget).
/// Do: on a partition whose merge budget holds 256 bytes, add a marker of 100 bytes, then one of 200; send an entry of two
/// versions of 100 bytes each, which fit one at a time and not together; destroy the repository; then add the larger
/// marker to a new repository. Expect: the larger marker and the two versions are each refused `Capacity` while the
/// first marker is held, nothing applied; the destroy gives the first's bytes back, so the larger fits after it; a
/// partition rebuilt from its snapshot charges the same.
#[test]
fn the_repositorys_state_is_charged_to_the_merge_budget_and_given_back_with_it() {
  let budget = caps(256);
  let (mut partition, repo, [a, b, ..]) = three_branches(budget);
  let mut small = at(repo, 1);
  small.state = vec![marker(&[1; 100])];
  advance(&mut partition, small).unwrap();
  let before = partition.to_snapshot(0);
  let mut large = at(repo, 2);
  large.state = vec![marker(&[2; 200])];
  assert_eq!(
    advance(&mut partition, large.clone()),
    Err(DbError::Capacity {
      table: "green_chains"
    })
  );
  let mut together = at(repo, 2);
  together.versions = vec![version(a, 1, &[3; 100]), version(b, 1, &[4; 100])];
  assert_eq!(
    advance(&mut partition, together),
    Err(DbError::Capacity {
      table: "green_chains"
    })
  );
  assert_eq!(partition.to_snapshot(0), before);
  assert_snapshot_rebuilds(&partition, budget);
  let mut rebuilt = Partition::from_snapshot(&partition.to_snapshot(0), budget, 0).unwrap();
  assert_eq!(
    advance(&mut rebuilt, large),
    Err(DbError::Capacity {
      table: "green_chains"
    }),
    "a rebuilt partition charges the state it carries"
  );
  run(
    &mut partition,
    &Op::RepositoryDestroyed { repository: repo },
  )
  .unwrap();
  assert_eq!(position(&partition, repo), None);
  let root = second_repository_with_root(&mut partition, green(6));
  let second = repository_id(10);
  let mut again = at(second, 1);
  again.state = vec![marker(&[2; 200])];
  advance(&mut partition, again).unwrap();
  assert_eq!(partition.repositories().markers(second).count(), 1);
  assert!(partition.volume(root).is_some());
}

/// A-603 (one position names one state of a repository). Do: create a repository, then change it by every operation that
/// can (A-515's and A-575's single operations, a member's version recorded alone, an entry, a member's destroy), and make
/// operations that do not change it (a version of a green outside its lineage, a refused retire, a destroy of a green
/// it does not hold); then destroy it. Expect: each change takes the next position and nothing else moves it; the
/// destroyed repository has no register.
#[test]
fn every_change_of_a_repository_takes_one_position() {
  let mut partition = Partition::new(caps(ROOMY), 0);
  let [a, b, outsider] = [green(1), green(2), green(3)];
  for id in [a, b, outsider] {
    run(
      &mut partition,
      &Op::VolumeCreated {
        record: volume(id, true),
      },
    )
    .unwrap();
  }
  let record = repository(9, "linux");
  let repo = record.id;
  run(&mut partition, &Op::RepositoryCreated { record }).unwrap();
  assert_eq!(position(&partition, repo), Some(0));
  let steps: Vec<(Op, Option<u64>)> = vec![
    (
      Op::RepositoryGreenJoined {
        record: member(repo, a, MemberOrigin::Root),
      },
      Some(1),
    ),
    (
      Op::RepositoryBranchAdded {
        record: crate::catalog::BranchRecord {
          repository: repo,
          name: "main".to_owned(),
          green: a,
        },
      },
      Some(2),
    ),
    (
      Op::RepositoryDefaultSet {
        repository: repo,
        branch: Some("main".to_owned()),
      },
      Some(3),
    ),
    (
      Op::GreenAdvanced {
        green: a,
        increment: b"a1".to_vec(),
      },
      Some(4),
    ),
    (
      Op::GreenAdvanced {
        green: outsider,
        increment: b"x".to_vec(),
      },
      Some(4),
    ),
    (
      Op::RepositoryGreenJoined {
        record: member(
          repo,
          b,
          MemberOrigin::Branch {
            from: a,
            version: 1,
          },
        ),
      },
      Some(5),
    ),
    (
      Op::RepositoryBranchRetired {
        repository: repo,
        name: "main".to_owned(),
      },
      Some(6),
    ),
    (
      Op::RepositoryAdvanced {
        entry: RepositoryEntry {
          names: vec![bind("main", a)],
          ..at(repo, 6)
        },
      },
      Some(7),
    ),
    (Op::VolumeDestroyed { id: b }, Some(8)),
    (Op::VolumeDestroyed { id: outsider }, Some(8)),
  ];
  for (op, expected) in steps {
    run(&mut partition, &op).unwrap();
    assert_eq!(position(&partition, repo), expected, "after {}", op.name());
  }
  assert_eq!(
    run(
      &mut partition,
      &Op::RepositoryBranchRetired {
        repository: repo,
        name: "gone".to_owned(),
      },
    ),
    Err(DbError::NotFound)
  );
  assert_eq!(
    run(&mut partition, &Op::VolumeDestroyed { id: green(99) }),
    Err(DbError::NotFound)
  );
  assert_eq!(position(&partition, repo), Some(8));
  assert_snapshot_rebuilds(&partition, caps(ROOMY));
  run(
    &mut partition,
    &Op::RepositoryDestroyed { repository: repo },
  )
  .unwrap();
  assert_eq!(position(&partition, repo), None);
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
/// Shape: the markers a history adds.
const MARKERS: u8 = 3;
/// Shape: the database's log ring, and each snapshot slot, in bytes: a history's records and its partition fit.
const SEGMENT_BYTES: u64 = 1 << 16;
/// Shape: the log bytes between the snapshots the database takes on its own, a few entries' worth, so a history's
/// recoveries start from a snapshot and replay a tail.
const SNAPSHOT_EVERY: u64 = 1 << 11;
/// Shape: the segment's page.
const PAGE: u64 = 4096;

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

/// How a step's value departs from the one the model holds right: mostly it does not, so histories are mostly accepted.
#[derive(Clone, Copy, Debug)]
enum Skew {
  Exact,
  Below,
  Above,
}

impl Skew {
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

#[derive(Clone, Debug)]
enum CheckStep {
  Head { green: Pick, skew: Skew },
  Names { prefix: u8, skew: Skew },
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
}

#[derive(Clone, Debug)]
struct EntryStep {
  /// Whether the entry keeps only its items that hold, judged in order on the model, at the right position and
  /// versions: an entry the rules accept whole, so histories reach entries of several greens, stacks and state moves
  /// often, and not refusals alone.
  clean: bool,
  skew: Skew,
  checks: Vec<CheckStep>,
  joins: Vec<JoinStep>,
  versions: Vec<(Pick, Skew)>,
  names: Vec<NameStep>,
  state: Vec<StateStep>,
}

#[derive(Clone, Debug)]
enum Step {
  Advance(EntryStep),
  Destroy { volume: u8 },
  Snapshot,
  Crash,
}

fn skew() -> impl Strategy<Value = Skew> {
  prop_oneof![
    8 => Just(Skew::Exact),
    1 => Just(Skew::Below),
    1 => Just(Skew::Above),
  ]
}

fn pick() -> impl Strategy<Value = Pick> {
  prop_oneof![
    4 => any::<u8>().prop_map(Pick::Member),
    1 => (0..VOLUMES).prop_map(Pick::Volume),
  ]
}

fn entry_step() -> impl Strategy<Value = EntryStep> {
  let check = prop_oneof![
    (pick(), skew()).prop_map(|(green, skew)| CheckStep::Head { green, skew }),
    (0..3u8, skew()).prop_map(|(prefix, skew)| CheckStep::Names { prefix, skew }),
  ];
  let join = (
    0..VOLUMES,
    proptest::option::weighted(0.9, (pick(), 0..4u8)),
  )
    .prop_map(|(volume, from)| JoinStep { volume, from });
  let name = prop_oneof![
    3 => (0..=NAMES, pick()).prop_map(|(name, green)| NameStep::Bind { name, green }),
    2 => (0..NAMES).prop_map(|name| NameStep::Retire { name }),
    1 => proptest::option::of(0..=NAMES).prop_map(|name| NameStep::Default { name }),
  ];
  let state = prop_oneof![
    (0..PHASES, 0..NODES).prop_map(|(phase, node)| StateStep::Add { phase, node }),
    (0..PHASES, 0..NODES).prop_map(|(phase, node)| StateStep::Remove { phase, node }),
    (0..MARKERS).prop_map(|marker| StateStep::Marker { marker }),
  ];
  (
    proptest::bool::weighted(0.6),
    skew(),
    proptest::collection::vec(check, 0..3),
    proptest::collection::vec(join, 0..2),
    proptest::collection::vec((pick(), skew()), 0..5),
    proptest::collection::vec(name, 0..3),
    proptest::collection::vec(state, 0..4),
  )
    .prop_map(
      |(clean, skew, checks, joins, versions, names, state)| EntryStep {
        clean,
        skew,
        checks,
        joins,
        versions,
        names,
        state,
      },
    )
}

fn step() -> impl Strategy<Value = Step> {
  prop_oneof![
    12 => entry_step().prop_map(Step::Advance),
    1 => (0..VOLUMES).prop_map(|volume| Step::Destroy { volume }),
    1 => Just(Step::Snapshot),
    2 => Just(Step::Crash),
  ]
}

/// What an operation was answered, by kind, with what a register refusal names.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Answer {
  Ok,
  NotFound,
  AlreadyExists,
  NotGreen,
  InvalidName,
  BranchedFrom,
  OutOfOrder { named: u64, expected: u64 },
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
    Err(DbError::OutOfOrder { named, expected }) => Answer::OutOfOrder { named, expected },
    Err(DbError::CheckFailed { index }) => Answer::CheckFailed(index),
    Err(DbError::StateChanged { index }) => Answer::StateChanged(index),
    Err(other) => Answer::Unexpected(format!("{other:?}")),
  }
}

/// The design's rules for one repository (§4.17.1 items 2 to 5, shared piece 2; A-515, A-575, A-603), kept over plain
/// maps by volume index: the serial reference the partition must equal after every step. An entry is judged on a copy
/// of the model, item by item, and the copy kept only when every item held: all or none, by construction.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Model {
  /// The register's next position.
  position: u64,
  /// The volumes the partition holds, by index.
  alive: BTreeSet<u8>,
  /// The lineage in the order it joined: each member and, for a branch, the member and version it was made at.
  members: Vec<(u8, Option<(u8, u64)>)>,
  /// The namespace: each name and the green it binds.
  names: BTreeMap<String, u8>,
  /// The default branch.
  default: Option<String>,
  /// Each green's head, where it has a version.
  heads: BTreeMap<u8, u64>,
  /// Each green's chain entries, where it has any.
  chains: BTreeMap<u8, Vec<Vec<u8>>>,
  /// The phase roots.
  roots: BTreeSet<(u8, Vec<u8>)>,
  /// The markers, in the order received.
  markers: Vec<Vec<u8>>,
}

impl Model {
  fn is_member(&self, volume: u8) -> bool {
    self.members.iter().any(|(member, _)| *member == volume)
  }

  fn head(&self, volume: u8) -> u64 {
    self.heads.get(&volume).copied().unwrap_or(0)
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

  /// Judges and, when it is accepted, applies `entry`: the rules in the design's order.
  fn advance(&mut self, entry: &RepositoryEntry) -> Answer {
    if entry.position != self.position {
      return Answer::OutOfOrder {
        named: entry.position,
        expected: self.position,
      };
    }
    for (index, check) in entry.checks.iter().enumerate() {
      if !self.holds(check) {
        return Answer::CheckFailed(u32::try_from(index).unwrap());
      }
    }
    let mut next = self.clone();
    for joined in &entry.joined {
      if let Err(answer) = next.join(joined) {
        return answer;
      }
    }
    for version in &entry.versions {
      if let Err(answer) = next.version(version) {
        return answer;
      }
    }
    for change in &entry.names {
      if let Err(answer) = next.name(change) {
        return answer;
      }
    }
    for (index, change) in entry.state.iter().enumerate() {
      if !next.state(change) {
        return Answer::StateChanged(u32::try_from(index).unwrap());
      }
    }
    next.position += 1;
    *self = next;
    Answer::Ok
  }

  /// Whether `check` holds at this state.
  fn holds(&self, check: &RepositoryCheck) -> bool {
    match check {
      RepositoryCheck::Head { green, version } => {
        self.is_member(index_of(*green)) && self.head(index_of(*green)) == *version
      }
      RepositoryCheck::Names { prefix, bound } => self.under(prefix) == *bound,
    }
  }

  /// `entry` with only its items that hold, judged in order on a copy of this state, at this position and with each
  /// version the one after its green's head: an entry the rules accept whole.
  fn cleaned(&self, entry: RepositoryEntry) -> RepositoryEntry {
    let mut scratch = self.clone();
    let mut kept = at(entry.repository, self.position);
    kept.checks = entry
      .checks
      .into_iter()
      .filter(|check| self.holds(check))
      .collect();
    for joined in entry.joined {
      if scratch.join(&joined).is_ok() {
        kept.joined.push(joined);
      }
    }
    for made in entry.versions {
      let green = index_of(made.green);
      let next = version(made.green, scratch.head(green) + 1, &made.entry);
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
    let expected = self.head(green) + 1;
    if version.version != expected {
      return Err(Answer::OutOfOrder {
        named: version.version,
        expected,
      });
    }
    self.heads.insert(green, version.version);
    self
      .chains
      .entry(green)
      .or_default()
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
    match change {
      StateChange::PhaseRootAdded { phase, node } => self.roots.insert((*phase, node.clone())),
      StateChange::PhaseRootRemoved { phase, node } => self.roots.remove(&(*phase, node.clone())),
      StateChange::MarkerAdded { marker } => {
        if self.markers.contains(marker) {
          return false;
        }
        self.markers.push(marker.clone());
        true
      }
    }
  }

  /// A volume's destroy: refused while a member was made from it; its name and membership leave with it, and a
  /// repository it leaves takes a position.
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
    self.heads.remove(&volume);
    self.chains.remove(&volume);
    if changed {
      self.position += 1;
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

  /// The entry a step makes as it stands.
  fn made(&self, repository: RepositoryId, step: &EntryStep) -> RepositoryEntry {
    let mut heads = self.heads.clone();
    RepositoryEntry {
      repository,
      position: step.skew.apply(self.position),
      checks: step
        .checks
        .iter()
        .map(|check| match check {
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
        })
        .collect(),
      joined: step
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
        .collect(),
      versions: step
        .versions
        .iter()
        .map(|(green, skew)| {
          let green = self.picked(*green);
          let version = skew.apply(heads.get(&green).copied().unwrap_or(0) + 1);
          heads.insert(green, version);
          GreenVersion {
            green: volume_of(green),
            version,
            entry: format!("{green}@{version}").into_bytes(),
          }
        })
        .collect(),
      names: step
        .names
        .iter()
        .map(|change| match change {
          NameStep::Bind { name, green } => bind(&name_of(*name), volume_of(self.picked(*green))),
          NameStep::Retire { name } => retire(&name_of(*name)),
          NameStep::Default { name } => NameChange::Default {
            branch: name.map(name_of),
          },
        })
        .collect(),
      state: step
        .state
        .iter()
        .map(|change| match change {
          StateStep::Add { phase, node } => root_added(phase + 1, *node),
          StateStep::Remove { phase, node } => root_removed(phase + 1, *node),
          StateStep::Marker { marker } => self::marker(format!("marker-{marker}").as_bytes()),
        })
        .collect(),
    }
  }
}

/// What the partition holds of the repository, in the model's terms.
fn observed(partition: &Partition, repository: RepositoryId) -> Model {
  let repositories = partition.repositories();
  Model {
    position: repositories.position(repository).unwrap_or(u64::MAX),
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
    heads: (0..GREENS)
      .map(|index| (index, partition.green_head(volume_of(index))))
      .filter(|(_, head)| *head > 0)
      .collect(),
    chains: (0..GREENS)
      .map(|index| (index, partition.green_chain(volume_of(index)).to_vec()))
      .filter(|(_, chain)| !chain.is_empty())
      .collect(),
    roots: repositories
      .phase_roots(repository)
      .map(|(phase, node)| (phase, node.to_vec()))
      .collect(),
    markers: repositories
      .markers(repository)
      .map(<[u8]>::to_vec)
      .collect(),
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
  /// Accepted entries committing versions of two greens or more.
  several_greens: u64,
  /// Accepted entries committing two versions or more of one green (a stack landed whole).
  stacked: u64,
  /// Accepted entries moving the repository's state with versions.
  state_with_versions: u64,
  /// Accepted entries moving the repository's state alone.
  state_alone: u64,
  /// Accepted entries whose checks held.
  checks_held: u64,
  /// Entries refused by a check, by a state change, out of order, and by a rule of the lineage or the namespace.
  check_failed: u64,
  /// See [`Census::check_failed`].
  state_changed: u64,
  /// See [`Census::check_failed`].
  out_of_order: u64,
  /// See [`Census::check_failed`].
  refused_by_rule: u64,
  /// Destroys of a member, which take the register a position.
  member_destroyed: u64,
  /// Recoveries from a snapshot and the log after it.
  recovered_from_snapshot: u64,
  /// Recoveries from the log alone.
  recovered_from_log: u64,
}

impl Census {
  /// Counts an entry and what it was answered.
  fn entry(&mut self, entry: &RepositoryEntry, answer: &Answer) {
    let greens: BTreeSet<VolumeId> = entry.versions.iter().map(|version| version.green).collect();
    match answer {
      Answer::Ok => {
        self.several_greens += u64::from(greens.len() > 1);
        self.stacked += u64::from(greens.len() < entry.versions.len());
        let moves_state = !entry.state.is_empty();
        self.state_with_versions += u64::from(moves_state && !entry.versions.is_empty());
        self.state_alone += u64::from(moves_state && entry.versions.is_empty());
        self.checks_held += u64::from(!entry.checks.is_empty());
      }
      Answer::CheckFailed(_) => self.check_failed += 1,
      Answer::StateChanged(_) => self.state_changed += 1,
      Answer::OutOfOrder { .. } => self.out_of_order += 1,
      _ => self.refused_by_rule += 1,
    }
  }
}

/// Runs a generated history through a database over its own segment: every answer and the whole repository equal the
/// model's after every step; at every crash the database is dropped and recovered from the segment, and the recovered
/// partition equals the live one whole.
fn every_history_equals_the_rules_and_survives_every_crash(
  steps: &[Step],
  census: &mut Census,
) -> Result<(), TestCaseError> {
  let mut seg = segment(&format!("slates-db-a603-{}", std::process::id()));
  let policy = SnapshotPolicy {
    bytes_between_snapshots: SNAPSHOT_EVERY,
  };
  let (mut db, _) = recover(&mut seg, 0, caps(ROOMY), 0).map_err(failed)?;
  db.set_policy(policy);
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
  let mut first = at(repo, 0);
  first.joined = vec![member(repo, volume_of(0), MemberOrigin::Root)];
  first.names = vec![bind("n0", volume_of(0))];
  prop_assert_eq!(model.advance(&first), Answer::Ok);
  db.mutate(&mut seg, &Op::RepositoryAdvanced { entry: first }, 0)
    .map_err(failed)?;
  let mut now_ns = 0;
  for step in steps {
    now_ns += 1;
    match step {
      Step::Advance(made) => {
        let entry = model.build(repo, made);
        let expected = model.advance(&entry);
        census.entry(&entry, &expected);
        let real = answer_of(db.mutate(&mut seg, &Op::RepositoryAdvanced { entry }, now_ns));
        prop_assert_eq!(real, expected, "step {:?}", step);
      }
      Step::Destroy { volume } => {
        let member = model.is_member(*volume);
        let expected = model.destroy(*volume);
        census.member_destroyed += u64::from(member && expected == Answer::Ok);
        let real = answer_of(db.mutate(
          &mut seg,
          &Op::VolumeDestroyed {
            id: volume_of(*volume),
          },
          now_ns,
        ));
        prop_assert_eq!(real, expected, "step {:?}", step);
      }
      Step::Snapshot => db.snapshot(&mut seg).map_err(failed)?,
      Step::Crash => {
        let live = db.partition().to_snapshot(0);
        let seq = db.next_seq();
        drop(db);
        let (recovered, stats) = recover(&mut seg, 0, caps(ROOMY), now_ns).map_err(failed)?;
        prop_assert_eq!(
          recovered.partition().to_snapshot(0),
          live,
          "recovery differs from the live partition"
        );
        prop_assert_eq!(recovered.next_seq(), seq);
        prop_assert!(!stats.torn, "a clean crash has no torn tail");
        if stats.snapshot.is_some() {
          census.recovered_from_snapshot += 1;
        } else {
          census.recovered_from_log += 1;
        }
        db = recovered;
        db.set_policy(policy);
      }
    }
    prop_assert_eq!(
      observed(db.partition(), repo),
      model.clone(),
      "after {:?}",
      step
    );
  }
  Ok(())
}

/// A-603 (§4.17.1 shared piece 2; the pattern of `crates/db/src/partition.rs` and `crates/db/tests/model.rs`). Do: run
/// generated histories over one repository through a database over a real anchor segment: entries joining members,
/// committing versions of one or several greens, changing names and the default, moving phase roots and adding markers,
/// with checks of heads and of names under prefixes, each item mostly right and sometimes made against another state;
/// destroys of any volume; snapshots; and crashes, at each of which the database is dropped and recovered from its
/// segment. Expect: every answer (accepted, or which refusal and what it names) and the whole repository after every
/// step (its position, members in the order they joined, names, default, every green's head and chain, phase roots,
/// markers in the order received) equal the design's rules kept over plain maps, where an entry is all or none by
/// construction; every recovered partition equals the live one whole; and the census shows every kind of entry,
/// refusal, destroy and recovery was met.
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
  let met = [
    census.several_greens,
    census.stacked,
    census.state_with_versions,
    census.state_alone,
    census.checks_held,
    census.check_failed,
    census.state_changed,
    census.out_of_order,
    census.refused_by_rule,
    census.member_destroyed,
    census.recovered_from_snapshot,
    census.recovered_from_log,
  ];
  assert!(
    met.iter().all(|&count| count > 0),
    "a kind of step never met: {census:?}"
  );
}
