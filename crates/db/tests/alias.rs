//! A-236's protocol under a crash at every step (§4.8 "Lookup"; the pattern of `model.rs`: the database dropped and
//! recovered from a real anchor segment). A clone whose name hashes to another partition is made in
//! `slates_db::alias::CLONE_STEPS` order across two partitions: the alias reserved at the name's partition, the clone
//! made at its origin's, the alias confirmed. It is destroyed in `DESTROY_STEPS` order. After a crash between any two
//! steps, both partitions are recovered, the name's partition is reconciled against the clone's
//! (`slates_db::alias::reconcile`), and the name must either belong to no one or resolve to the clone that holds it:
//! never a clone no name reaches, never a name two volumes hold.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use slates_anchor::{AnchorSegment, Geometry};
use slates_db::alias::{CLONE_STEPS, DESTROY_STEPS, Reconciled, Step, reconcile, resolve};
use slates_db::catalog::{
  BaseRecord, NameAliasRecord, NamePolicy, PolicyRecord, Principal, Role, SizeClass, SnapshotId,
  VolumeId, VolumeRecord, VolumeState,
};
use slates_db::partition::PartitionCaps;
use slates_db::replay::recover;
use slates_db::{Db, DbError, Op};
use slates_machine::facts::Identity;

/// Shape: the segment page.
const PAGE: u64 = 4096;
/// Shape: the log ring of each partition (bytes).
const LOG_BYTES: u64 = 1 << 20;
/// Shape: a snapshot slot (bytes).
const SNAPSHOT_BYTES: u64 = 1 << 20;
/// Format: the partition the name hashes to.
const NAME_PARTITION: u16 = 0;
/// Format: the partition the clone lives on, beside its origin.
const CLONE_PARTITION: u16 = 1;
/// Format: the clone's name.
const NAME: &str = "kept-at-snapshot";

fn identity() -> Identity {
  Identity {
    cpu: "test".into(),
    os: "test".into(),
    arch: "test".into(),
    cores: 4,
    memory: 1,
    page: PAGE,
  }
}

fn segment(name: &str) -> AnchorSegment {
  AnchorSegment::create(
    name,
    &identity(),
    Geometry {
      partitions: 2,
      page: PAGE,
      profile_bytes: PAGE,
      log_bytes: LOG_BYTES,
      snapshot_bytes: SNAPSHOT_BYTES,
      audit_bytes: PAGE,
      landing_slots: 1,
      landing_slot_bytes: PAGE,
    },
  )
  .unwrap()
}

fn caps() -> PartitionCaps {
  PartitionCaps {
    volumes: 1 << 8,
    snapshots: 1 << 8,
    attachments: 1 << 8,
    segment_slots: 64,
    timers: 1 << 8,
    tick_ns: 1_000,
    green_chain_bytes: 1 << 16,
  }
}

fn vid(partition: u16, n: u64) -> VolumeId {
  let mut bytes = [0u8; 16];
  bytes[..2].copy_from_slice(&partition.to_be_bytes());
  bytes[8..].copy_from_slice(&n.to_be_bytes());
  VolumeId { bytes }
}

fn volume(id: VolumeId, name: &str) -> VolumeRecord {
  VolumeRecord {
    id,
    name: name.to_owned(),
    owner_shard: 0,
    policy: PolicyRecord {
      size: SizeClass::Bounded { limit: 1 << 20 },
      names: NamePolicy::Exact,
      require_locked: false,
      role: Role::Plain,
      live_shipped: false,
    },
    base: BaseRecord::Scratch,
    head: SnapshotId { value: 0 },
    epoch: 0,
    referenced_bytes: 0,
    unique_bytes: 0,
    state: VolumeState::Live,
    lease: None,
    owner: Principal::Uid { uid: 1000 },
    access: vec![],
    created_ns: 0,
    catalog_version: 0,
  }
}

/// The two partitions, recovered from `segment`.
struct Pair {
  name: Db,
  clone: Db,
}

fn recovered(segment: &mut AnchorSegment) -> Pair {
  Pair {
    name: recover(segment, NAME_PARTITION, caps(), 0).unwrap().0,
    clone: recover(segment, CLONE_PARTITION, caps(), 0).unwrap().0,
  }
}

/// Runs one protocol step for the clone `id` named `NAME`.
fn run(step: Step, pair: &mut Pair, segment: &mut AnchorSegment, id: VolumeId) {
  let op = match step {
    Step::Reserve => Op::NameAliasReserved {
      record: NameAliasRecord {
        name: NAME.to_owned(),
        volume: id,
        confirmed: false,
      },
    },
    Step::Confirm => Op::NameAliasConfirmed {
      name: NAME.to_owned(),
    },
    Step::Release => Op::NameAliasReleased {
      name: NAME.to_owned(),
    },
    Step::Create => Op::VolumeCreated {
      record: volume(id, NAME),
    },
    Step::Destroy => Op::VolumeDestroyed { id },
  };
  let db = match step {
    Step::Reserve | Step::Confirm | Step::Release => &mut pair.name,
    Step::Create | Step::Destroy => &mut pair.clone,
  };
  db.mutate(segment, &op, 0).unwrap();
}

/// A recovery's reconciliation of every alias the name's partition holds: what the daemon runs at its start.
fn reconcile_all(pair: &mut Pair, segment: &mut AnchorSegment) {
  let aliases: Vec<NameAliasRecord> = pair.name.partition().name_aliases().cloned().collect();
  for alias in aliases {
    let verdict = reconcile(&alias, pair.clone.partition().volume(alias.volume));
    let op = match verdict {
      Reconciled::Keep => continue,
      Reconciled::Confirm => Op::NameAliasConfirmed { name: alias.name },
      Reconciled::Release => Op::NameAliasReleased { name: alias.name },
    };
    pair.name.mutate(segment, &op, 0).unwrap();
  }
}

/// What `NAME` reaches: the volume a lookup at the name's partition resolves to.
fn resolved(pair: &Pair) -> Option<VolumeId> {
  if let Some(local) = pair.name.partition().volume_by_name(NAME) {
    return Some(local.id);
  }
  pair.name.partition().name_alias(NAME).and_then(resolve)
}

/// Whether a create of `NAME` at the name's partition (the only place a create of it goes) is refused as held.
fn held(pair: &Pair) -> bool {
  let other = Op::VolumeCreated {
    record: volume(vid(NAME_PARTITION, 99), NAME),
  };
  matches!(
    pair.name.partition().check(&other, 0),
    Err(DbError::AlreadyExists { .. })
  )
}

/// The invariant after a reconciled recovery: the name belongs to no one and no clone holds it, or it resolves to the
/// clone that holds it and a second create of it is refused.
fn assert_consistent(pair: &Pair, id: VolumeId, after: &str) {
  let clone = pair
    .clone
    .partition()
    .volume(id)
    .filter(|volume| volume.name == NAME);
  match (resolved(pair), clone) {
    (None, None) => assert!(!held(pair), "{after}: a name no one holds is still refused"),
    (Some(reached), Some(clone)) => {
      assert_eq!(
        reached, clone.id,
        "{after}: the name reaches another volume"
      );
      assert!(
        held(pair),
        "{after}: a second create of a resolving name was allowed"
      );
    }
    (None, Some(_)) => panic!("{after}: the clone exists and no name reaches it"),
    (Some(reached), None) => {
      panic!("{after}: the name resolves to {reached:?}, which no partition holds")
    }
  }
}

/// A-236. Do: for each point from before the first step to after the last, run the clone's steps up to it, crash
/// (both partitions dropped and recovered from the segment), check the unreconciled name, reconcile, and check again.
/// Expect: before the reconcile a reserved alias holds the name and resolves nothing; after it, the name belongs to no
/// one or resolves to the clone, at every crash point.
#[test]
fn a_clones_name_is_consistent_after_a_crash_at_every_step() {
  for crash_after in 0..=CLONE_STEPS.len() {
    let tag = format!("slates-db-alias-clone-{crash_after}-{}", std::process::id());
    let mut segment = segment(&tag);
    let mut pair = recovered(&mut segment);
    let id = vid(CLONE_PARTITION, 1);
    for step in CLONE_STEPS.iter().take(crash_after) {
      run(*step, &mut pair, &mut segment, id);
    }
    drop(pair);
    let mut pair = recovered(&mut segment);
    if let Some(alias) = pair.name.partition().name_alias(NAME)
      && !alias.confirmed
    {
      assert_eq!(resolve(alias), None, "a reserved alias resolves nothing");
      assert!(
        held(&pair),
        "a reserved alias holds the name against a create"
      );
    }
    reconcile_all(&mut pair, &mut segment);
    let after = format!("a crash after {crash_after} of the clone's steps");
    assert_consistent(&pair, id, &after);
    drop(pair);
    let pair = recovered(&mut segment);
    assert_consistent(
      &pair,
      id,
      &format!("{after}, reconciled and recovered again"),
    );
    let expected = if crash_after >= 2 { Some(id) } else { None };
    assert_eq!(resolved(&pair), expected, "{after}: what the name reaches");
  }
}

/// A-236. Do: make the clone in full, then for each point of its destroy's steps, crash there, recover, reconcile.
/// Expect: the name resolves to the clone until the clone is destroyed, and to no one from then on, at every crash
/// point; and a create of the name is allowed once no one holds it.
#[test]
fn a_destroyed_clones_name_is_consistent_after_a_crash_at_every_step() {
  for crash_after in 0..=DESTROY_STEPS.len() {
    let tag = format!(
      "slates-db-alias-destroy-{crash_after}-{}",
      std::process::id()
    );
    let mut segment = segment(&tag);
    let mut pair = recovered(&mut segment);
    let id = vid(CLONE_PARTITION, 1);
    for step in CLONE_STEPS {
      run(step, &mut pair, &mut segment, id);
    }
    for step in DESTROY_STEPS.iter().take(crash_after) {
      run(*step, &mut pair, &mut segment, id);
    }
    drop(pair);
    let mut pair = recovered(&mut segment);
    reconcile_all(&mut pair, &mut segment);
    let after = format!("a crash after {crash_after} of the destroy's steps");
    assert_consistent(&pair, id, &after);
    let expected = if crash_after == 0 { Some(id) } else { None };
    assert_eq!(resolved(&pair), expected, "{after}: what the name reaches");
  }
}

/// A-236 (a clone that cannot be made). Do: reserve the alias, then release it as a refused clone does, crash, and
/// recover. Expect: the name belongs to no one, and a create of it is allowed.
#[test]
fn a_clone_that_could_not_be_made_gives_its_name_back() {
  let mut segment = segment(&format!("slates-db-alias-refused-{}", std::process::id()));
  let mut pair = recovered(&mut segment);
  let id = vid(CLONE_PARTITION, 1);
  run(Step::Reserve, &mut pair, &mut segment, id);
  run(Step::Release, &mut pair, &mut segment, id);
  drop(pair);
  let pair = recovered(&mut segment);
  assert_consistent(&pair, id, "a refused clone");
  assert!(!held(&pair), "the name is free again");
}

/// A-236 (uniqueness at the name's partition). Do: hold the name as a volume of the name's partition, then reserve an
/// alias for it; and the other way round. Expect: each second holder refused `AlreadyExists`, whichever came first.
#[test]
fn a_name_is_held_once_by_a_volume_or_an_alias() {
  let mut segment = segment(&format!("slates-db-alias-unique-{}", std::process::id()));
  let mut pair = recovered(&mut segment);
  pair
    .name
    .mutate(
      &mut segment,
      &Op::VolumeCreated {
        record: volume(vid(NAME_PARTITION, 1), NAME),
      },
      0,
    )
    .unwrap();
  let reserve = Op::NameAliasReserved {
    record: NameAliasRecord {
      name: NAME.to_owned(),
      volume: vid(CLONE_PARTITION, 1),
      confirmed: false,
    },
  };
  assert!(matches!(
    pair.name.mutate(&mut segment, &reserve, 0),
    Err(DbError::AlreadyExists { .. })
  ));
  let other = Op::NameAliasReserved {
    record: NameAliasRecord {
      name: "another".to_owned(),
      volume: vid(CLONE_PARTITION, 2),
      confirmed: false,
    },
  };
  pair.name.mutate(&mut segment, &other, 0).unwrap();
  assert!(matches!(
    pair.name.partition().check(
      &Op::VolumeCreated {
        record: volume(vid(NAME_PARTITION, 2), "another"),
      },
      0
    ),
    Err(DbError::AlreadyExists { .. })
  ));
  assert!(matches!(
    pair.name.mutate(&mut segment, &other, 0),
    Err(DbError::AlreadyExists { .. })
  ));
}

/// A-236 (the snapshot's appended field). Do: hold a confirmed and a reserved alias, take the partition's snapshot,
/// encode and decode it, and rebuild a partition from it. Expect: both aliases back as they were, the reserved one still
/// resolving nothing and still holding its name.
#[test]
fn a_partitions_aliases_survive_its_snapshot() {
  use slates_db::partition::{Partition, PartitionSnapshot};
  use slates_wire::Wire;
  let mut segment = segment(&format!("slates-db-alias-snapshot-{}", std::process::id()));
  let mut pair = recovered(&mut segment);
  let id = vid(CLONE_PARTITION, 1);
  for step in CLONE_STEPS {
    run(step, &mut pair, &mut segment, id);
  }
  let reserved = NameAliasRecord {
    name: "still-reserved".to_owned(),
    volume: vid(CLONE_PARTITION, 2),
    confirmed: false,
  };
  pair
    .name
    .mutate(
      &mut segment,
      &Op::NameAliasReserved {
        record: reserved.clone(),
      },
      0,
    )
    .unwrap();
  let snapshot = pair.name.partition().to_snapshot(0);
  let mut bytes = Vec::new();
  snapshot.encode(&mut bytes);
  let decoded = PartitionSnapshot::decode(&mut bytes.as_slice()).unwrap();
  let rebuilt = Partition::from_snapshot(&decoded, caps(), 0).unwrap();
  assert_eq!(rebuilt.name_alias(NAME).and_then(resolve), Some(id));
  assert_eq!(rebuilt.name_alias("still-reserved"), Some(&reserved));
  assert!(matches!(
    rebuilt.check(
      &Op::VolumeCreated {
        record: volume(vid(NAME_PARTITION, 3), "still-reserved"),
      },
      0
    ),
    Err(DbError::AlreadyExists { .. })
  ));
}
