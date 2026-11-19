//! A-629 part 2: a repository's Git LFS lock log in the partition, driven through the partition's
//! guard and apply as the live path drives it, snapshotted and restored, and held to a model of
//! the module's rules.

use std::collections::BTreeMap;

use proptest::prelude::*;

use super::*;
use crate::catalog::{ObjectFormat, Principal, RepositoryPolicy, RepositoryRecord, VcsTool};
use crate::partition::{Partition, PartitionCaps};

/// Shape: the partition the tests' repositories live on.
const HOME: u16 = 3;

/// Shape: the locks the small budget has room for, about, so a history meets the bound.
const BUDGET_LOCKS: usize = 6;

/// Shape: a lock's time, 2026-11-19T00:00:00Z.
const LOCKED_AT: i64 = 1_795_046_400;

/// An id on `HOME` with `serial` in its low byte, as the server mints them.
fn repository_id(serial: u8) -> RepositoryId {
  let mut bytes = [0u8; 16];
  bytes[8..10].copy_from_slice(&HOME.to_be_bytes());
  bytes[15] = serial;
  RepositoryId { bytes }
}

/// The principal numbered `serial`.
fn person(serial: u8) -> Principal {
  Principal::Certificate { hash: [serial; 32] }
}

/// A partition's caps with a merge byte budget of `budget`.
fn caps_with(budget: usize) -> PartitionCaps {
  PartitionCaps {
    volumes: 1 << 4,
    snapshots: 1 << 4,
    attachments: 1 << 4,
    segment_slots: 4,
    timers: 1 << 4,
    tick_ns: 1_000,
    green_chain_bytes: budget,
    audit_records: 1 << 4,
  }
}

/// Caps whose merge byte budget holds about [`BUDGET_LOCKS`] locks.
fn caps() -> PartitionCaps {
  caps_with(BUDGET_LOCKS * size_of::<LfsLockRecord>())
}

/// Repository `serial` created, owned by person 0.
fn created(serial: u8) -> Op {
  Op::RepositoryCreated {
    record: RepositoryRecord {
      id: repository_id(serial),
      name: format!("repository-{serial}"),
      tool: VcsTool::Git,
      format: ObjectFormat::Sha1,
      default_branch: None,
      owner: person(0),
      policy: RepositoryPolicy::default(),
    },
  }
}

/// The record of a lock on `path` of repository `serial` at `place`, taken by person `owner`.
fn lock_record(serial: u8, place: u64, path: &str, owner: u8) -> LfsLockRecord {
  LfsLockRecord {
    repository: repository_id(serial),
    place,
    path: path.to_owned(),
    owner: person(owner),
    locked_at: LOCKED_AT,
  }
}

/// A lock on `path` of repository `serial` at `place`, taken by person `owner`.
fn take(serial: u8, place: u64, path: &str, owner: u8) -> Op {
  Op::LfsLockTaken {
    lock: lock_record(serial, place, path, owner),
  }
}

/// The release of the lock at `place` of repository `serial`.
fn release(serial: u8, place: u64) -> Op {
  Op::LfsLockReleased {
    repository: repository_id(serial),
    place,
  }
}

/// The refusal of `rule`.
fn lock_refusal(rule: LfsLockRefusal) -> Result<(), DbError> {
  Err(DbError::LfsLock(rule))
}

/// Runs `op` as the live path does: the guard, then the apply.
fn run(partition: &mut Partition, op: &Op) -> Result<(), DbError> {
  partition.check(op, 0)?;
  partition.apply(op)
}

/// A partition with `caps` holding repository 0.
fn partition_with(caps: PartitionCaps) -> Partition {
  let mut partition = Partition::new(caps, 0);
  run(&mut partition, &created(0)).unwrap();
  partition
}

/// A partition holding repository 0, its budget small.
fn partition() -> Partition {
  partition_with(caps())
}

/// What a partition holds of its lock logs, compared whole across a snapshot.
type Held = (Vec<LfsLockLogRecord>, Vec<LfsLockRecord>, usize);

/// What a partition holds of its lock logs.
fn held(partition: &Partition) -> Held {
  let locks = partition.lfs_locks();
  (
    locks.log_records().collect(),
    locks.lock_records().cloned().collect(),
    locks.bytes(),
  )
}

/// Each standing lock of repository 0 from `place` on, by place and path.
fn listed(partition: &Partition, place: u64) -> Vec<(u64, String)> {
  let locks = partition.lfs_locks().from_place(repository_id(0), place);
  locks.map(|lock| (lock.place, lock.path.clone())).collect()
}

/// A-629 part 2. Do: take locks on `a` and `b` at places 0 and 1, release the first, take `a`
/// again at place 2, then take `c` at place 1, a place the log has moved past. Expect: each lock
/// at the place its taker read; the released place not taken again; the stale take refused
/// `Moved`, naming place 3; the log listed from place 1 as the locks at 1 and 2.
#[test]
fn a_lock_takes_the_place_its_taker_read_and_no_place_is_taken_twice() {
  let mut partition = partition();
  for op in [
    take(0, 0, "a", 1),
    take(0, 1, "b", 1),
    release(0, 0),
    take(0, 2, "a", 2),
  ] {
    run(&mut partition, &op).unwrap();
  }
  let stale = run(&mut partition, &take(0, 1, "c", 1));
  assert_eq!(stale, lock_refusal(LfsLockRefusal::Moved { next: 3 }));
  let expected = vec![(1, "b".to_owned()), (2, "a".to_owned())];
  let next = partition.lfs_locks().next(repository_id(0));
  assert_eq!((listed(&partition, 1), next), (expected, 3));
}

/// A-629 part 2. Do: take a lock on a path held, on an empty path, on one holding a NUL byte and
/// in a repository the partition does not hold, and release a place where no lock stands. Expect
/// each refused by its kind (`Held` naming the holder, `Path` twice, `NotFound`, `NotHeld`) and
/// nothing recorded: the log still stands at place 1.
#[test]
fn the_guard_refuses_each_rule_by_kind() {
  let mut partition = partition();
  run(&mut partition, &take(0, 0, "a", 1)).unwrap();
  let refusals = [
    run(&mut partition, &take(0, 1, "a", 2)),
    run(&mut partition, &take(0, 1, "", 2)),
    run(&mut partition, &take(0, 1, "a\0b", 2)),
    run(&mut partition, &take(9, 0, "a", 2)),
    run(&mut partition, &release(0, 7)),
  ];
  let expected = [
    lock_refusal(LfsLockRefusal::Held { place: 0 }),
    lock_refusal(LfsLockRefusal::Path),
    lock_refusal(LfsLockRefusal::Path),
    Err(DbError::NotFound),
    lock_refusal(LfsLockRefusal::NotHeld),
  ];
  assert_eq!(refusals, expected);
  assert_eq!(partition.lfs_locks().next(repository_id(0)), 1);
}

/// A-629 part 2. Do: take three locks in two repositories and release one, snapshot the partition
/// and restore it, then take the next lock in the restored one. Expect the same logs, locks and
/// bytes after the restore, and the next lock taken at the place the log stood at before it.
#[test]
fn a_snapshot_carries_every_lock_log_whole() {
  let mut partition = partition();
  run(&mut partition, &created(1)).unwrap();
  for op in [
    take(0, 0, "a", 1),
    take(1, 0, "a", 1),
    take(0, 1, "b", 2),
    release(0, 0),
  ] {
    run(&mut partition, &op).unwrap();
  }
  let mut rebuilt = Partition::from_snapshot(&partition.to_snapshot(0), caps(), 0).unwrap();
  assert_eq!(held(&rebuilt), held(&partition));
  run(&mut rebuilt, &take(0, 2, "c", 1)).unwrap();
  assert_eq!(
    listed(&rebuilt, 0),
    [(1, "b".to_owned()), (2, "c".to_owned())]
  );
}

/// A-629 part 2. Do: take two locks, then destroy their repository. Expect both standing before,
/// and after it the locks and the log gone with it and their bytes given back: nothing listed, the
/// place back to 0, no bytes held.
#[test]
fn a_destroyed_repository_s_locks_leave_with_it() {
  let mut partition = partition();
  for op in [take(0, 0, "a", 1), take(0, 1, "b", 1)] {
    run(&mut partition, &op).unwrap();
  }
  let standing = listed(&partition, 0).len();
  let destroyed = Op::RepositoryDestroyed {
    repository: repository_id(0),
  };
  run(&mut partition, &destroyed).unwrap();
  let locks = partition.lfs_locks();
  let left = (
    listed(&partition, 0),
    locks.next(repository_id(0)),
    locks.bytes(),
  );
  assert_eq!((standing, left), (2, (Vec::new(), 0, 0)));
}

/// A-629 part 2, bounds. Do: take locks on new paths until the merge byte budget, room for about
/// six lock records, refuses one. Expect a refusal within twelve takes, typed as the merge plane's
/// `Capacity`, every lock before it standing, and at least one taken.
#[test]
fn locks_past_the_merge_budget_are_refused_as_capacity() {
  let mut partition = partition();
  let mut taken = 0u64;
  let mut refusal = None;
  for _ in 0..BUDGET_LOCKS * 2 {
    match run(&mut partition, &take(0, taken, &format!("file-{taken}"), 1)) {
      Ok(()) => taken += 1,
      Err(refused) => {
        refusal = Some(refused);
        break;
      }
    }
  }
  let standing = u64::try_from(listed(&partition, 0).len()).unwrap();
  let capacity = DbError::Capacity {
    table: "green_chains",
  };
  assert_eq!(
    (refusal, standing, taken > 0),
    (Some(capacity), taken, true)
  );
}

/// A-629 part 2, hostile input. Do: restore lock logs from snapshots that name a repository the
/// partition does not hold, give one repository two logs, hold a lock with no log, one at a place
/// its log has not reached, and two locks on one path; then take a lock on a log restored at the
/// last place. Expect each snapshot refused as corrupt, and the take refused `Exhausted`.
#[test]
fn a_snapshot_that_cannot_be_restored_whole_is_refused() {
  let log = |next| LfsLockLogRecord {
    repository: repository_id(0),
    next,
  };
  let lock = |place, path: &str| lock_record(0, place, path, 1);
  let held_here = |id| id == repository_id(0);
  let cases: [(Vec<LfsLockLogRecord>, Vec<LfsLockRecord>); 5] = [
    (vec![log(1)], vec![]),
    (vec![log(1), log(2)], vec![]),
    (vec![], vec![lock(0, "a")]),
    (vec![log(1)], vec![lock(1, "a")]),
    (vec![log(2)], vec![lock(0, "a"), lock(1, "a")]),
  ];
  for (index, (logs, locks)) in cases.iter().enumerate() {
    let holds = |id| index != 0 && held_here(id);
    let restored = LfsLocks::restore(logs, locks, holds);
    assert!(
      matches!(restored, Err(DbError::Corrupt { .. })),
      "case {index}"
    );
  }
  let last = LfsLocks::restore(&[log(u64::MAX)], &[], held_here).unwrap();
  let refused = last.check(&take(0, u64::MAX, "a", 1), held_here);
  assert_eq!(refused, lock_refusal(LfsLockRefusal::Exhausted));
}

/// One step of a generated history: take the lock on path number `path` at the place the log
/// stands at less `behind`, as person `owner`; or release the lock at `place`.
#[derive(Clone, Copy, Debug)]
enum Step {
  /// A take.
  Take {
    /// The path's number: 0 is empty and 1 holds a NUL byte, so both are refused.
    path: u8,
    /// How far behind the log the taker read.
    behind: u8,
    /// Who takes it.
    owner: u8,
  },
  /// A release.
  Release {
    /// The place.
    place: u8,
  },
}

/// A generated step, a take as often as a release.
fn step() -> impl Strategy<Value = Step> {
  (any::<bool>(), 0u8..6, 0u8..3, 0u8..3, 0u8..12).prop_map(
    |(taking, path, behind, owner, place)| {
      if taking {
        Step::Take {
          path,
          behind,
          owner,
        }
      } else {
        Step::Release { place }
      }
    },
  )
}

/// Path number `number`'s text: 0 empty, 1 with a NUL byte, the rest plain.
fn path_of(number: u8) -> String {
  match number {
    0 => String::new(),
    1 => "a\0b".to_owned(),
    _ => format!("path-{number}"),
  }
}

/// The module's rules as its doc states them, over plain maps: the place the next lock takes, and
/// the standing locks by place, each its path and its owner.
#[derive(Debug, Default)]
struct Model {
  /// The place the next lock takes.
  next: u64,
  /// The standing locks.
  held: BTreeMap<u64, (String, u8)>,
}

impl Model {
  /// A take of `path` at `place` by `owner`, refused by the first rule it breaks.
  fn take(&mut self, path: &str, place: u64, owner: u8) -> Result<(), DbError> {
    if path.is_empty() || path.contains('\0') {
      return lock_refusal(LfsLockRefusal::Path);
    }
    if place != self.next {
      return lock_refusal(LfsLockRefusal::Moved { next: self.next });
    }
    let holder = self.held.iter().find(|(_, (held, _))| held == path);
    if let Some((holder, _)) = holder {
      return lock_refusal(LfsLockRefusal::Held { place: *holder });
    }
    self.held.insert(place, (path.to_owned(), owner));
    self.next += 1;
    Ok(())
  }

  /// A release of the lock at `place`.
  fn release(&mut self, place: u64) -> Result<(), DbError> {
    match self.held.remove(&place) {
      Some(_) => Ok(()),
      None => lock_refusal(LfsLockRefusal::NotHeld),
    }
  }

  /// The bytes the design charges: each lock's record, path and owner's key, and the log's place
  /// once the log is made.
  fn bytes(&self) -> usize {
    let log = if self.next > 0 {
      size_of::<LfsLockLogRecord>()
    } else {
      0
    };
    let locks = self
      .held
      .values()
      .map(|(path, owner)| size_of::<LfsLockRecord>() + path.len() + person(*owner).key().len());
    log + locks.sum::<usize>()
  }

  /// The standing locks, by place and path.
  fn listed(&self) -> Vec<(u64, String)> {
    let held = self.held.iter();
    held
      .map(|(place, (path, _))| (*place, path.clone()))
      .collect()
  }
}

/// Runs `step` on the partition and on the model: both answers.
fn both(partition: &mut Partition, model: &mut Model, step: Step) -> [Result<(), DbError>; 2] {
  match step {
    Step::Take {
      path,
      behind,
      owner,
    } => {
      let place = model.next.saturating_sub(u64::from(behind));
      let path = path_of(path);
      let ours = run(partition, &take(0, place, &path, owner));
      [ours, model.take(&path, place, owner)]
    }
    Step::Release { place } => {
      let ours = run(partition, &release(0, u64::from(place)));
      [ours, model.release(u64::from(place))]
    }
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig {
    cases: slates_test_seeds::cases(400),
    ..ProptestConfig::default()
  }))]

  /// A-629 part 2, model: do run a generated history of takes and releases, some of empty or
  /// NUL-holding paths, some read behind the log, some of held paths or absent places, on a
  /// partition with room for all; then snapshot and restore it. Expect every answer, the standing
  /// locks, the log's place and the bytes charged to equal the model's at every step, and the
  /// restored partition to hold the same.
  #[test]
  fn a_lock_log_follows_its_rules(steps in proptest::collection::vec(step(), 1..48)) {
    let mut partition = partition_with(caps_with(1 << 20));
    let mut model = Model::default();
    for step in steps {
      let [ours, modelled] = both(&mut partition, &mut model, step);
      prop_assert_eq!(ours, modelled, "{:?}", step);
    }
    let locks = partition.lfs_locks();
    let found = (listed(&partition, 0), locks.next(repository_id(0)), locks.bytes());
    prop_assert_eq!(found, (model.listed(), model.next, model.bytes()));
    let rebuilt = Partition::from_snapshot(&partition.to_snapshot(0), caps_with(1 << 20), 0);
    prop_assert_eq!(rebuilt.map(|rebuilt| held(&rebuilt)), Ok(held(&partition)));
  }
}
