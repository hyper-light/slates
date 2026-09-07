//! The database baselines (Phase 2 task 2; AC-2.7's shape): the adaptive radix tree's insert
//! and lookup per key at 10^4 and 10^5 keys, one mutation through the log (guard, append,
//! apply), and recovery of a partition of 10^4 volumes from 10^6 log records against the
//! recovery budget of §4.8 (a snapshot then the tail). Rows are
//! `ratchet\t<key>\t<lower>\t<median>\t<upper>` in nanoseconds, as `cargo xtask ratchet` reads
//! them.
// Bench harness code: an unwrap here is a failed run.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Instant;

use slates_anchor::{AnchorSegment, Geometry};
use slates_db::catalog::{
  BaseRecord, NamePolicy, PolicyRecord, Principal, Role, SizeClass, SnapshotId, VolumeId,
  VolumeRecord, VolumeState,
};
use slates_db::partition::PartitionCaps;
use slates_db::replay::{RECOVERY_BUDGET_NS, SnapshotPolicy, recover};
use slates_db::{Art, Op};
use slates_machine::facts::Identity;

/// Shape: runs per row.
const RUNS: usize = 5;
/// Shape: the segment page.
const PAGE: u64 = 4096;
/// Shape: the log ring of the recovery row: room for 10^6 accounting records (about 60 bytes
/// each) with headroom.
const LOG_BYTES: u64 = 128 << 20;
/// Shape: a snapshot slot: room for 10^4 volume records.
const SNAPSHOT_BYTES: u64 = 8 << 20;
/// Shape: the volumes of the recovery row (AC-2.7).
const VOLUMES: u64 = 10_000;
/// Shape: the log records of the recovery row (AC-2.7).
const RECORDS: u64 = 1_000_000;

fn identity() -> Identity {
  Identity {
    cpu: "bench".into(),
    os: "bench".into(),
    arch: "bench".into(),
    cores: 4,
    memory: 1,
    page: PAGE,
  }
}

fn caps() -> PartitionCaps {
  PartitionCaps {
    volumes: 1 << 15,
    snapshots: 1 << 15,
    attachments: 1 << 15,
    segment_slots: 1 << 10,
    timers: 1 << 15,
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
    name: format!("volume-{n}"),
    owner_shard: 0,
    policy: PolicyRecord {
      size: SizeClass::Bounded { limit: 1 << 30 },
      names: NamePolicy::Exact,
      require_locked: false,
      role: Role::Plain,
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
  }
}

fn row(key: &str, mut samples: Vec<u64>) {
  samples.sort_unstable();
  let lower = samples[0];
  let median = samples[samples.len() / 2];
  let upper = samples[samples.len() - 1];
  println!("ratchet\t{key}\t{lower}\t{median}\t{upper}");
  println!(
    "  {key}: {median} ns [{lower}, {upper}] over {} runs: {samples:?}",
    samples.len()
  );
}

fn ns(elapsed: std::time::Duration) -> u64 {
  u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
}

fn art_rows() {
  for keys in [10_000u64, 100_000] {
    let mut inserts = Vec::new();
    let mut lookups = Vec::new();
    for _ in 0..RUNS {
      let mut art: Art<u32> = Art::new();
      let started = Instant::now();
      for k in 0..keys {
        art.insert(
          &vid(k * 2_654_435_761 % (1 << 40)).bytes,
          u32::try_from(k).unwrap(),
        );
      }
      inserts.push(ns(started.elapsed()) / keys);
      let started = Instant::now();
      let mut hits = 0u64;
      for k in 0..keys {
        if art.get(&vid(k * 2_654_435_761 % (1 << 40)).bytes).is_some() {
          hits += 1;
        }
      }
      lookups.push(ns(started.elapsed()) / keys);
      assert_eq!(hits, keys);
    }
    row(&format!("db.art_insert_per_key_at_{keys}"), inserts);
    row(&format!("db.art_lookup_per_key_at_{keys}"), lookups);
  }
}

fn segment(name: &str) -> AnchorSegment {
  AnchorSegment::create(
    name,
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

fn log_rows() {
  let mut mutate = Vec::new();
  let mut recoveries = Vec::new();
  let mut per_record = Vec::new();
  for run in 0..RUNS {
    let mut seg = segment(&format!("slates-db-bench-{run}"));
    let (mut db, _) = recover(&mut seg, 0, caps(), 0).unwrap();
    // No snapshot while the history is built: the recovery row replays the whole log.
    db.set_policy(SnapshotPolicy {
      bytes_between_snapshots: u64::MAX,
    });
    for n in 0..VOLUMES {
      db.mutate(&mut seg, &Op::VolumeCreated { record: volume(n) }, 0)
        .unwrap();
    }
    let started = Instant::now();
    for n in 0..RECORDS {
      db.mutate(
        &mut seg,
        &Op::VolumeAccounted {
          id: vid(n % VOLUMES),
          referenced_bytes: n,
          unique_bytes: n / 2,
        },
        0,
      )
      .unwrap();
    }
    mutate.push(ns(started.elapsed()) / RECORDS);
    let live = db.partition().volume(vid(7)).unwrap().referenced_bytes;
    drop(db);
    let started = Instant::now();
    let (recovered, stats) = recover(&mut seg, 0, caps(), 0).unwrap();
    let elapsed = ns(started.elapsed());
    assert_eq!(
      recovered
        .partition()
        .volume(vid(7))
        .unwrap()
        .referenced_bytes,
      live
    );
    assert_eq!(stats.replayed_records, VOLUMES + RECORDS);
    recoveries.push(elapsed);
    per_record.push(elapsed / (VOLUMES + RECORDS));
    let policy = SnapshotPolicy::derive(RECOVERY_BUDGET_NS, stats.replay_bytes_per_us());
    println!(
      "  recovery {run}: {elapsed} ns for {} records ({} bytes); throughput {} bytes/us; snapshot every {} bytes",
      stats.replayed_records,
      stats.replayed_bytes,
      stats.replay_bytes_per_us(),
      policy.value.bytes_between_snapshots
    );
  }
  row("db.mutate_guard_append_apply_per_record", mutate);
  row("db.recover_10k_volumes_from_1m_records", recoveries.clone());
  row("db.replay_per_record", per_record);
  let budget_ok = recoveries.iter().all(|r| *r <= RECOVERY_BUDGET_NS);
  println!(
    "  AC-2.7: recovery of 10^4 volumes from 10^6 records within the {} ns budget: {}",
    RECOVERY_BUDGET_NS,
    if budget_ok { "yes" } else { "NO" }
  );
  assert!(budget_ok, "AC-2.7: recovery exceeded the budget");
}

fn main() {
  art_rows();
  log_rows();
}
