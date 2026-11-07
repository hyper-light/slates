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
/// Shape: a wide log ring: room for the whole run's tail, so the full-ring path never forces a snapshot and the cadence
/// alone decides.
const WIDE_LOG_BYTES: u64 = 64 << 20;
/// Shape: a log ring about the partition's size (raft's case): the full-ring path forces a snapshot about once a ring,
/// which the bound must survive too.
const NARROW_LOG_BYTES: u64 = 8 << 20;
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

fn segment(log_bytes: u64) -> AnchorSegment {
  AnchorSegment::create(
    &format!("slates-db-cadence-{log_bytes}-{}", std::process::id()),
    &identity(),
    Geometry {
      partitions: 1,
      page: PAGE,
      profile_bytes: PAGE,
      log_bytes,
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

/// What one run measured: the partition's encoded bytes, the log appended after it was built, and what the snapshots
/// wrote meanwhile.
struct Absorbed {
  partition: u64,
  appended: u64,
  written: u64,
}

/// Recovers a fresh partition over a log ring of `log_bytes` (nothing to replay, so no measured throughput), builds it
/// to about 8 MiB with 32 increments of 256 KiB, then appends 16 MiB of log in small records that change one volume's
/// accounting and do not grow it.
fn absorb(log_bytes: u64) -> Absorbed {
  let mut seg = segment(log_bytes);
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
  let before = db.stats();
  let mut n = 0u64;
  while db.stats().log_bytes_appended - before.log_bytes_appended < TAIL_BYTES {
    n += 1;
    let accounted = Op::VolumeAccounted {
      id,
      referenced_bytes: n,
      unique_bytes: n,
    };
    db.mutate(&mut seg, &accounted, 0).unwrap();
  }
  let after = db.stats();
  let absorbed = Absorbed {
    partition,
    appended: after.log_bytes_appended - before.log_bytes_appended,
    written: after.snapshot_bytes_written - before.snapshot_bytes_written,
  };
  println!(
    "ring {log_bytes}: partition {} bytes; {} bytes of log in {n} records; snapshots wrote {} bytes; cadence {} \
     bytes, apply {} bytes/us, longest snapshot {} ns",
    absorbed.partition,
    absorbed.appended,
    absorbed.written,
    after.bytes_between_snapshots,
    after.apply_bytes_per_us,
    after.snapshot_ns_max
  );
  absorbed
}

/// The bound: the snapshots wrote at most twice the log they absorbed plus one partition.
fn assert_bounded(run: &Absorbed) {
  assert!(
    run.partition > INCREMENTS as u64 * INCREMENT_BYTES as u64,
    "the partition holds the increments"
  );
  assert!(
    run.written <= 2 * run.appended + run.partition,
    "snapshots wrote {} bytes while the log absorbed {} (partition {})",
    run.written,
    run.appended,
    run.partition
  );
}

/// A-243, §4.8. Do: over a 64 MiB log ring, recover a fresh partition, build it to about 8 MiB, then append 16 MiB of
/// log in small records. Expect: the snapshots taken meanwhile wrote at most twice the log plus one partition:
/// snapshot work proportional to the log, not to the log times the partition. Before, a snapshot every 1 MB of log
/// re-encoded the 8 MiB partition, about 128 MiB written for 16 MiB of log (red on CI run 42385009677).
#[test]
fn a_partitions_snapshots_cost_what_its_log_absorbs_not_its_size_per_megabyte() {
  assert_bounded(&absorb(WIDE_LOG_BYTES));
}

/// A-243, §4.8 (raft's case). Do: the same over a log ring about the partition's size, so a full ring forces a
/// snapshot about once a ring whatever the cadence. Expect: the same bound.
#[test]
fn a_ring_about_the_partitions_size_keeps_the_same_bound() {
  assert_bounded(&absorb(NARROW_LOG_BYTES));
}
