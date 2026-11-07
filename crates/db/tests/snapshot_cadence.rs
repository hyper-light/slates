//! A partition's snapshots cost at most a constant times the log they absorb, whatever the partition's size (§4.8
//! "snapshots of partition state into the segment on a cadence derived from measured replay throughput and the
//! recovery budget"; A-243). A snapshot re-encodes the whole partition. Its cadence was the bytes the replay throughput
//! measured at boot covers in the recovery budget, and a daemon booted fresh measured nothing and took one byte a
//! microsecond, a snapshot every 1 MB of log for the rest of its life. So each byte of log cost the partition's size
//! divided by 1 MB in snapshot writes: at 10,000 works the green's owner shard spent about 1,800 of 2,241 samples
//! re-encoding its partition (2026-11-07, this Mac).
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_anchor::{AnchorSegment, Geometry};
use slates_db::Op;
use slates_db::catalog::{
  BaseRecord, NamePolicy, PolicyRecord, Principal, Role, SizeClass, SnapshotId, VolumeId,
  VolumeRecord, VolumeState,
};
use slates_db::partition::PartitionCaps;
use slates_db::replay::recover;
use slates_machine::facts::Identity;

/// Shape: the segment page.
const PAGE: u64 = 4096;
/// Shape: the log ring: room for the whole run's tail, so the full-ring path never forces a snapshot and the cadence
/// alone decides.
const LOG_BYTES: u64 = 64 << 20;
/// Shape: a snapshot slot: room for the partition the test builds.
const SNAPSHOT_BYTES: u64 = 32 << 20;
/// Shape: the bytes of each increment that builds the partition.
const INCREMENT_BYTES: usize = 256 << 10;
/// Shape: the increments: a partition of about 8 MiB, far more than the 1 MB cadence a fresh boot took.
const INCREMENTS: usize = 32;
/// Shape: the log the test appends after the partition is built, in small records that do not grow it.
const TAIL_BYTES: u64 = 16 << 20;

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

fn segment() -> AnchorSegment {
  AnchorSegment::create(
    &format!("slates-db-cadence-{}", std::process::id()),
    &identity(),
    Geometry {
      partitions: 1,
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
    volumes: 1 << 12,
    snapshots: 1 << 12,
    attachments: 1 << 12,
    segment_slots: 64,
    timers: 1 << 12,
    tick_ns: 1_000,
    green_chain_bytes: 64 << 20,
  }
}

fn green() -> VolumeRecord {
  let mut bytes = [0u8; 16];
  bytes[0] = 1;
  VolumeRecord {
    id: VolumeId { bytes },
    name: "green".to_owned(),
    owner_shard: 0,
    policy: PolicyRecord {
      size: SizeClass::Bounded { limit: 1 << 30 },
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

/// A-243, §4.8. Do: recover a fresh partition (nothing to replay, so no measured throughput); build it to about 8 MiB
/// with 32 increments of 256 KiB; then append 16 MiB of log in small records that change one volume's accounting and
/// do not grow the partition. Expect: the snapshots taken while the 16 MiB were appended wrote at most twice that plus
/// one partition: snapshot work proportional to the log, not to the log times the partition. Before, a snapshot every
/// 1 MB of log re-encoded the 8 MiB partition: about 128 MiB written for 16 MiB of log.
#[test]
fn a_partitions_snapshots_cost_what_its_log_absorbs_not_its_size_per_megabyte() {
  let mut seg = segment();
  let mut db = recover(&mut seg, 0, caps(), 0).unwrap().0;
  let record = green();
  let id = record.id;
  db.mutate(&mut seg, &Op::VolumeCreated { record }, 0)
    .unwrap();
  for n in 0..INCREMENTS {
    let increment = vec![u8::try_from(n % 251).unwrap(); INCREMENT_BYTES];
    db.mutate(
      &mut seg,
      &Op::GreenAdvanced {
        green: id,
        increment,
      },
      0,
    )
    .unwrap();
  }
  let partition = slates_wire::Wire::to_bytes(&db.partition().to_snapshot(0)).len() as u64;
  let (written_before, appended_before) = (db.snapshot_bytes_written(), db.log_bytes_appended());
  let mut n = 0u64;
  while db.log_bytes_appended() - appended_before < TAIL_BYTES {
    n += 1;
    let accounted = Op::VolumeAccounted {
      id,
      referenced_bytes: n,
      unique_bytes: n,
    };
    db.mutate(&mut seg, &accounted, 0).unwrap();
  }
  let written = db.snapshot_bytes_written() - written_before;
  let appended = db.log_bytes_appended() - appended_before;
  println!(
    "partition {partition} bytes; {appended} bytes of log appended in {n} records; snapshots wrote {written} bytes"
  );
  assert!(
    partition > INCREMENTS as u64 * INCREMENT_BYTES as u64,
    "the partition holds the increments"
  );
  assert!(
    written <= 2 * appended + partition,
    "snapshots wrote {written} bytes while the log absorbed {appended} (partition {partition})"
  );
}
