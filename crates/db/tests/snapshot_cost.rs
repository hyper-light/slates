//! What a maintenance snapshot costs against the log it covers (§4.3 "Bounded work everywhere", §4.8): a snapshot is
//! taken inside the commit that crosses the policy's byte interval, on the shard's thread.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::sync::atomic::Ordering;
use std::time::Instant;

use slates_anchor::layout::{RING_BYTES, RegionKind};
use slates_anchor::{AnchorSegment, Geometry};
use slates_db::Op;
use slates_db::catalog::{
  BaseRecord, NamePolicy, PolicyRecord, Principal, Role, SizeClass, SnapshotId, VolumeId,
  VolumeRecord, VolumeState,
};
use slates_db::partition::PartitionCaps;
use slates_db::record::{LogRing, RingWord};
use slates_db::replay::{SnapshotPolicy, recover};
use slates_machine::facts::Identity;

/// Shape: the segment page.
const PAGE: u64 = 4096;
/// Shape: the log ring: room for the longest row's accounting records (about 60 bytes each) with headroom.
const LOG_BYTES: u64 = 128 << 20;
/// Shape: a snapshot slot: room for the rows' few volumes.
const SNAPSHOT_BYTES: u64 = 1 << 20;
/// Shape: the volumes every row holds, so the snapshot's own encoding is the same in each and only the log differs.
const VOLUMES: u64 = 8;
/// Shape: the clock the histories run at (nanoseconds); leases play no part here.
const NOW_NS: u64 = 1_000;

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
    green_chain_bytes: 1 << 20,
  }
}

fn vid(n: u64) -> VolumeId {
  let mut bytes = [0u8; 16];
  bytes[..8].copy_from_slice(&n.to_be_bytes());
  VolumeId { bytes }
}

fn volume(n: u64) -> VolumeRecord {
  VolumeRecord {
    id: vid(n),
    name: format!("v{n}"),
    owner_shard: 0,
    policy: PolicyRecord {
      size: SizeClass::Bounded { limit: 1 << 20 },
      names: NamePolicy::Exact,
      require_locked: false,
      role: Role::Plain,
      live_shipped: false,
      live_holders: None,
    },
    base: BaseRecord::Scratch,
    head: SnapshotId { value: 0 },
    epoch: 0,
    referenced_bytes: 0,
    unique_bytes: 0,
    state: VolumeState::Live,
    lease: None,
    owner: Principal::Uid {
      host: slates_db::HostId(0),
      uid: 1000,
    },
    access: Vec::new(),
    created_ns: NOW_NS,
    catalog_version: 0,
  }
}

/// §4.3 "Bounded work everywhere", §4.8. A snapshot is written from the partition, so the log it covers is dead the
/// moment it publishes: releasing it reads no record. Do: log sixteen records past a snapshot, flip a byte in the body
/// of the first, and take another snapshot. Expect: it succeeds, the ring is empty, and a recovery holds every record's
/// effect with nothing replayed. Before 2026-11-08 the release copied, checksummed and decoded every record to learn
/// its length (one poll over 1,000,000 records: 654 ms in release, 14.9 s in debug), so a damaged dead record refused
/// the snapshot and the log was never released.
#[test]
fn a_snapshot_releases_the_log_it_covers_without_reading_a_record() {
  /// Shape: the records past the snapshot, more than one so the walk would meet the damage first and still have more.
  const RECORDS: u64 = 16;
  /// Format: an offset inside the first record's body, past its header.
  const INTO_THE_BODY: usize = 40;
  let mut seg = segment("slates-db-snapshot-reads-nothing");
  let (mut db, _) = recover(&mut seg, 0, caps(), NOW_NS).unwrap();
  db.set_policy(SnapshotPolicy {
    bytes_between_snapshots: u64::MAX,
  });
  for n in 0..VOLUMES {
    db.mutate(&mut seg, &Op::VolumeCreated { record: volume(n) }, NOW_NS)
      .unwrap();
  }
  db.snapshot(&mut seg).unwrap();
  for n in 0..RECORDS {
    db.mutate(
      &mut seg,
      &Op::VolumeAccounted {
        id: vid(n % VOLUMES),
        referenced_bytes: n,
        unique_bytes: n / 2,
      },
      NOW_NS,
    )
    .unwrap();
  }
  let at = RING_BYTES + INTO_THE_BODY;
  let mut byte = [0u8; 1];
  seg.region_read(RegionKind::Log(0), at, &mut byte).unwrap();
  byte[0] ^= 0xff;
  seg.region_write(RegionKind::Log(0), at, &byte).unwrap();
  let before = db.partition().to_snapshot(0);
  db.snapshot(&mut seg)
    .expect("a snapshot reads no record of the log it covers");
  let (used, _, _) = db.log_usage(&seg).unwrap();
  assert_eq!(used, 0, "the covered log is released");
  drop(db);
  let (recovered, stats) = recover(&mut seg, 0, caps(), NOW_NS).unwrap();
  assert_eq!(stats.replayed_records, 0, "the snapshot holds every record");
  assert!(!stats.torn, "the released record is not read");
  assert_eq!(recovered.partition().to_snapshot(0), before);
}

/// The ring header's slot of `word` (`LogRing`'s words: head, tail, capacity, sequence base).
fn slot(word: RingWord) -> usize {
  match word {
    RingWord::Head => 0,
    RingWord::Tail => 1,
    RingWord::SequenceBase => 3,
  }
}

/// §4.8, D-18. Releasing the log is three stores with a crash possible between any two, after the snapshot covering it
/// has published. Do: log records past a snapshot and take another, then put the ring's words back as a crash after each
/// prefix of `LogRing::release_stores` leaves them (none, the sequence base, and the head, and the tail), and recover.
/// Each runs on two rings: the head at the first byte, as every release since A-71 leaves it, and the head past the
/// first record, as an older segment's may be. Expect, at every crash point: the recovered partition equals the live
/// one, no record is replayed (each was in the snapshot, so one applied again would count twice), and a mutation after
/// the recovery survives a second recovery. On the second ring the head must be stored before the tail: a tail at zero
/// under a head past it reads as an empty ring whose next records sit behind the head, and are lost.
#[test]
fn a_crash_at_every_store_of_a_release_recovers_the_partition_the_snapshot_holds() {
  for head_past_first in [false, true] {
    for crash_after in 0..=LogRing::release_stores(0).len() {
      crash_a_release_after(head_past_first, crash_after);
    }
  }
}

/// One crash point of the test above.
fn crash_a_release_after(head_past_first: bool, crash_after: usize) {
  /// Shape: the records past the first snapshot.
  const RECORDS: u64 = 16;
  let shape = format!("head past the first record: {head_past_first}, after {crash_after} stores");
  let mut seg = segment(&format!(
    "slates-db-release-crash-{}-{crash_after}",
    u8::from(head_past_first)
  ));
  let (mut db, _) = recover(&mut seg, 0, caps(), NOW_NS).unwrap();
  db.set_policy(SnapshotPolicy {
    bytes_between_snapshots: u64::MAX,
  });
  for n in 0..VOLUMES {
    db.mutate(&mut seg, &Op::VolumeCreated { record: volume(n) }, NOW_NS)
      .unwrap();
  }
  db.snapshot(&mut seg).unwrap();
  let ring = RegionKind::Log(0);
  let read = |seg: &AnchorSegment, word: RingWord| {
    seg.ring_words(ring).unwrap()[slot(word)].load(Ordering::Acquire)
  };
  let mut first_end = 0;
  for n in 0..RECORDS {
    db.mutate(
      &mut seg,
      &Op::VolumeAccounted {
        id: vid(n % VOLUMES),
        referenced_bytes: n,
        unique_bytes: n / 2,
      },
      NOW_NS,
    )
    .unwrap();
    if n == 0 {
      first_end = read(&seg, RingWord::Tail);
    }
  }
  if head_past_first {
    let words = seg.ring_words(ring).unwrap();
    let base = read(&seg, RingWord::SequenceBase);
    words[slot(RingWord::Head)].store(first_end, Ordering::Release);
    words[slot(RingWord::SequenceBase)].store(base + 1, Ordering::Release);
  }
  let live = db.partition().to_snapshot(0);
  let before =
    [RingWord::Head, RingWord::Tail, RingWord::SequenceBase].map(|word| (word, read(&seg, word)));
  db.snapshot(&mut seg).unwrap();
  let next_seq = read(&seg, RingWord::SequenceBase);
  drop(db);
  let words = seg.ring_words(ring).unwrap();
  for (word, value) in before {
    words[slot(word)].store(value, Ordering::Release);
  }
  for (word, value) in LogRing::release_stores(next_seq)
    .into_iter()
    .take(crash_after)
  {
    words[slot(word)].store(value, Ordering::Release);
  }
  let (mut recovered, stats) = recover(&mut seg, 0, caps(), NOW_NS).unwrap();
  assert_eq!(
    recovered.partition().to_snapshot(0),
    live,
    "{shape}: recovers the snapshot's partition"
  );
  assert_eq!(
    stats.replayed_records, 0,
    "{shape}: replays no record the snapshot holds"
  );
  recovered
    .mutate(
      &mut seg,
      &Op::VolumeAccounted {
        id: vid(0),
        referenced_bytes: 1 << 20,
        unique_bytes: 1 << 19,
      },
      NOW_NS,
    )
    .unwrap();
  let after = recovered.partition().to_snapshot(0);
  drop(recovered);
  let (again, _) = recover(&mut seg, 0, caps(), NOW_NS).unwrap();
  assert_eq!(
    again.partition().to_snapshot(0),
    after,
    "{shape}: leaves a log the next record recovers from"
  );
}

/// §4.8. A release with no crash leaves a ring the next records go on from. Do: log records past a snapshot, take
/// another, log more, and recover. Expect: the ring holds only the later records, recovery replays exactly those, and
/// the partition equals the live one.
#[test]
fn the_records_after_a_release_are_what_a_recovery_replays() {
  /// Shape: the records logged on each side of the release.
  const RECORDS: u64 = 16;
  let account = |db: &mut slates_db::Db, seg: &mut AnchorSegment, from: u64| {
    for n in from..from + RECORDS {
      db.mutate(
        seg,
        &Op::VolumeAccounted {
          id: vid(n % VOLUMES),
          referenced_bytes: n,
          unique_bytes: n / 2,
        },
        NOW_NS,
      )
      .unwrap();
    }
  };
  let mut seg = segment("slates-db-after-a-release");
  let (mut db, _) = recover(&mut seg, 0, caps(), NOW_NS).unwrap();
  db.set_policy(SnapshotPolicy {
    bytes_between_snapshots: u64::MAX,
  });
  for n in 0..VOLUMES {
    db.mutate(&mut seg, &Op::VolumeCreated { record: volume(n) }, NOW_NS)
      .unwrap();
  }
  account(&mut db, &mut seg, 0);
  db.snapshot(&mut seg).unwrap();
  account(&mut db, &mut seg, RECORDS);
  let live = db.partition().to_snapshot(0);
  drop(db);
  let (recovered, stats) = recover(&mut seg, 0, caps(), NOW_NS).unwrap();
  assert!(!stats.torn, "nothing of the released log is met");
  assert_eq!(stats.replayed_records, RECORDS, "exactly the later records");
  assert_eq!(recovered.partition().to_snapshot(0), live);
}

/// Measurement (not a gate): `Db::snapshot` against the records logged since the last one, the partition held the same.
/// Run: `cargo test --release -p slates-db --test snapshot_cost -- --ignored --nocapture`.
#[test]
#[ignore = "measurement, not a gate; prints a snapshot's cost vs the log it covers"]
fn a_snapshots_cost_against_the_log_it_covers() {
  for records in [1_000u64, 10_000, 100_000, 1_000_000] {
    let mut seg = segment(&format!("slates-db-snapcost-{records}"));
    let (mut db, _) = recover(&mut seg, 0, caps(), NOW_NS).unwrap();
    db.set_policy(SnapshotPolicy {
      bytes_between_snapshots: u64::MAX,
    });
    for n in 0..VOLUMES {
      db.mutate(&mut seg, &Op::VolumeCreated { record: volume(n) }, NOW_NS)
        .unwrap();
    }
    db.snapshot(&mut seg).unwrap();
    let t0 = Instant::now();
    db.snapshot(&mut seg).unwrap();
    let empty_log = t0.elapsed();
    for n in 0..records {
      db.mutate(
        &mut seg,
        &Op::VolumeAccounted {
          id: vid(n % VOLUMES),
          referenced_bytes: n,
          unique_bytes: n / 2,
        },
        NOW_NS,
      )
      .unwrap();
    }
    let (used, _, _) = db.log_usage(&seg).unwrap();
    let t0 = Instant::now();
    db.snapshot(&mut seg).unwrap();
    let covering = t0.elapsed();
    eprintln!(
      "snapshot: {records} records ({used} log bytes) | over an empty log {empty_log:?}, over this log {covering:?} ({} ns/record)",
      covering.saturating_sub(empty_log).as_nanos() / u128::from(records)
    );
  }
}
