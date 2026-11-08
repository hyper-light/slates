//! The grants table as the database keeps it (§4.15, A-266): live grants only, a grant leaving the table at its terminal
//! step, and the highest id ever issued kept across a recovery so no id is issued twice.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use slates_anchor::{AnchorSegment, Geometry};
use slates_db::Op;
use slates_db::catalog::{
  GrantRecord, GrantScope, GrantState, GrantSurface, Principal, SnapshotId, VolumeId,
};
use slates_db::partition::PartitionCaps;
use slates_db::replay::{SnapshotPolicy, recover};
use slates_machine::facts::Identity;

/// Shape: the segment page.
const PAGE: u64 = 4096;
/// Shape: the log ring: room for every record the history appends.
const LOG_BYTES: u64 = 1 << 20;
/// Shape: a snapshot slot.
const SNAPSHOT_BYTES: u64 = 1 << 20;
/// Shape: the clock the history runs at (nanoseconds); leases play no part here.
const NOW_NS: u64 = 1_000;
/// Shape: the grants issued.
const GRANTS: u64 = 40;

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
    &format!("{name}-{}", std::process::id()),
    &identity(),
    Geometry {
      partitions: 1,
      page: PAGE,
      profile_bytes: PAGE,
      log_bytes: LOG_BYTES,
      snapshot_bytes: SNAPSHOT_BYTES,
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
    green_chain_bytes: 1 << 20,
    audit_records: 1 << 12,
  }
}

fn issued(id: u64) -> Op {
  Op::GrantIssued {
    record: GrantRecord {
      id,
      principal: Principal::Uid {
        host: slates_db::HostId(0),
        uid: 1000,
      },
      surface: GrantSurface::Cli,
      volume: VolumeId { bytes: [1; 16] },
      snapshot: SnapshotId { value: 1 },
      target: "/t".into(),
      manifest: [7; 32],
      scope: GrantScope::Once,
      issued_ns: NOW_NS,
      expires_ns: NOW_NS + 1_000,
      state: GrantState::Issued,
      target_device: 0,
      target_inode: 0,
    },
  }
}

/// §4.15 (A-266). Do: issue 40 grants and end each at a terminal step (consumed, expired or revoked in turn), snapshot,
/// and recover. Expect: the table holds none of them, before and after the recovery, while the highest id issued
/// stays 40, so the next grant after a restart takes an id past every ended one. Before 2026-11-08 an ended grant
/// stayed in the table for the partition's life, and the table was the only record of the highest id.
#[test]
fn an_ended_grant_leaves_the_table_and_its_id_is_never_issued_again() {
  let mut seg = segment("slates-db-grants-ended");
  let (mut db, _) = recover(&mut seg, 0, caps(), NOW_NS).unwrap();
  db.set_policy(SnapshotPolicy {
    bytes_between_snapshots: u64::MAX,
  });
  let ends = [
    GrantState::Consumed,
    GrantState::Expired,
    GrantState::Revoked,
  ];
  for id in 1..=GRANTS {
    db.mutate(&mut seg, &issued(id), NOW_NS).unwrap();
    let ended = ends[usize::try_from(id).unwrap() % ends.len()];
    db.mutate(
      &mut seg,
      &Op::GrantStateChanged { id, state: ended },
      NOW_NS,
    )
    .unwrap();
  }
  assert_eq!(db.partition().grants().count(), 0, "no ended grant is held");
  assert_eq!(db.partition().grants_issued_through(), GRANTS);
  db.snapshot(&mut seg).unwrap();
  drop(db);
  let (recovered, _) = recover(&mut seg, 0, caps(), NOW_NS).unwrap();
  assert_eq!(
    recovered.partition().grants().count(),
    0,
    "nor after a recovery"
  );
  assert_eq!(
    recovered.partition().grants_issued_through(),
    GRANTS,
    "the highest id issued survives the recovery"
  );
}
