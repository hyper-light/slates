//! The landing audit as the database keeps it (§4.15, §4.14): every record numbered in the order it was appended, the
//! same numbers after a recovery, from a snapshot or from the log.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use slates_anchor::{AnchorSegment, Geometry};
use slates_db::Op;
use slates_db::catalog::{AuditKind, AuditRecord, Principal};
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
/// Shape: the records appended on each side of the snapshot.
const RECORDS: u64 = 40;

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

/// An audit record as the server makes one: its sequence is the database's to assign, so it is made zero.
fn planned(landing: u64) -> Op {
  Op::AuditAppended {
    record: AuditRecord {
      seq: 0,
      at_ns: NOW_NS,
      kind: AuditKind::LandingPlanned,
      principal: Principal::Uid {
        host: slates_db::HostId(0),
        uid: 1000,
      },
      grant: None,
      landing: Some(landing),
      manifest: Some([7; 32]),
      outcome: None,
    },
  }
}

fn seqs(db: &slates_db::Db) -> Vec<u64> {
  db.partition().audit().map(|record| record.seq).collect()
}

/// §4.15, §4.14 (A-265). Do: append 40 audit records, snapshot, append 40 more, then recover. Expect: the records are
/// numbered 0 to 79 in the order they were appended, and the recovered partition (the snapshot's 40, then the log's
/// 40 replayed) holds the same numbers, so a reader's `since` means the same record before and after a restart. Before
/// 2026-11-08 every record was numbered 0, so `audit --since N` filtered nothing.
#[test]
fn audit_records_are_numbered_in_order_and_a_recovery_numbers_them_the_same() {
  let mut seg = segment("slates-db-audit-seq");
  let (mut db, _) = recover(&mut seg, 0, caps(), NOW_NS).unwrap();
  db.set_policy(SnapshotPolicy {
    bytes_between_snapshots: u64::MAX,
  });
  for landing in 0..RECORDS {
    db.mutate(&mut seg, &planned(landing), NOW_NS).unwrap();
  }
  db.snapshot(&mut seg).unwrap();
  for landing in RECORDS..2 * RECORDS {
    db.mutate(&mut seg, &planned(landing), NOW_NS).unwrap();
  }
  let live = seqs(&db);
  assert_eq!(
    live,
    (0..2 * RECORDS).collect::<Vec<_>>(),
    "numbered in order"
  );
  drop(db);
  let (recovered, stats) = recover(&mut seg, 0, caps(), NOW_NS).unwrap();
  assert_eq!(
    stats.replayed_records, RECORDS,
    "the snapshot holds the first half"
  );
  assert_eq!(seqs(&recovered), live, "a recovery numbers them the same");
}

/// Shape: the retained window of the retention test: smaller than what it appends, so the bound is reached.
const WINDOW: usize = 16;

/// §4.15, §4.14 (A-265). Do: with the partition's audit bounded at 16 records, append 40, then recover from the log,
/// and again from a snapshot. Expect: the 16 newest are retained (24 to 39), the 24 oldest dropped and counted, and
/// each recovery holds the same window and count: growth stops at the bound, oldest first. Before 2026-11-08 the
/// audit grew for the partition's life and every snapshot carried all of it.
#[test]
fn the_audit_keeps_its_bounded_window_newest_and_counts_what_it_dropped() {
  let bounded = PartitionCaps {
    audit_records: WINDOW,
    ..caps()
  };
  let mut seg = segment("slates-db-audit-window");
  let (mut db, _) = recover(&mut seg, 0, bounded, NOW_NS).unwrap();
  db.set_policy(SnapshotPolicy {
    bytes_between_snapshots: u64::MAX,
  });
  for landing in 0..RECORDS {
    db.mutate(&mut seg, &planned(landing), NOW_NS).unwrap();
  }
  let window: Vec<u64> = (RECORDS - WINDOW as u64..RECORDS).collect();
  assert_eq!(seqs(&db), window, "the newest 16, oldest first");
  assert_eq!(db.partition().audit_dropped(), RECORDS - WINDOW as u64);
  drop(db);
  let (recovered, _) = recover(&mut seg, 0, bounded, NOW_NS).unwrap();
  assert_eq!(seqs(&recovered), window, "a replay keeps the same window");
  assert_eq!(
    recovered.partition().audit_dropped(),
    RECORDS - WINDOW as u64
  );
  let mut recovered = recovered;
  recovered.snapshot(&mut seg).unwrap();
  drop(recovered);
  let (again, _) = recover(&mut seg, 0, bounded, NOW_NS).unwrap();
  assert_eq!(seqs(&again), window, "and so does a snapshot");
  assert_eq!(again.partition().audit_dropped(), RECORDS - WINDOW as u64);
}
