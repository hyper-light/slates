//! A partition's log ring and snapshot slots keep high-water marks in the ring's header, raised before the writes that
//! pass them and lowered after the give-back of what no read reaches again (A-926 part 2; A-746's owed give-back).
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::print_stdout
)]

use std::sync::atomic::Ordering;

use slates_anchor::layout::{PAYLOAD_BYTES, PAYLOAD_LEN, RING_BYTES, RegionKind};
use slates_anchor::{AnchorSegment, Geometry, RegionMarks};
use slates_db::Op;
use slates_db::catalog::{
  BaseRecord, NamePolicy, PolicyRecord, Principal, Role, SizeClass, SnapshotId, VolumeId,
  VolumeRecord, VolumeState,
};
use slates_db::partition::PartitionCaps;
use slates_db::replay::{Db, SnapshotPolicy, recover};
use slates_machine::facts::Identity;

/// Shape: the segment page.
const PAGE: u64 = 4096;
/// Shape: the log ring: many pages.
const LOG_BYTES: u64 = 4 << 20;
/// Shape: a snapshot slot.
const SNAPSHOT_BYTES: u64 = 1 << 20;
/// Shape: the clock the histories run at (nanoseconds).
const NOW_NS: u64 = 1_000;
/// Shape: the records a history logs past its first snapshot: past several of the ring's pages.
const RECORDS: u64 = 400;

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
      write: None,
      write_version: 0,
      signers: Vec::new(),
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
    taken_over: None,
  }
}

/// A history over `seg` that leaves pages no read reaches again in every region with a mark: `volumes` volumes made and
/// a snapshot taken (the first slot), [`RECORDS`] records logged past it and a second snapshot (the second slot, the
/// ring released to its first byte, A-71), all but one volume removed and a third, shorter snapshot (the first slot
/// again, past its payload the first's longer one): the database, the ring's pages past its tail, the first slot's past
/// its payload.
fn history(seg: &mut AnchorSegment, volumes: u64) -> Db {
  let (mut db, _) = recover(seg, 0, caps(), NOW_NS).unwrap();
  db.set_policy(SnapshotPolicy {
    bytes_between_snapshots: u64::MAX,
  });
  for n in 0..volumes {
    db.mutate(seg, &Op::VolumeCreated { record: volume(n) }, NOW_NS)
      .unwrap();
  }
  db.snapshot(seg).unwrap();
  for n in 0..RECORDS {
    db.mutate(
      seg,
      &Op::VolumeAccounted {
        id: vid(n % volumes),
        referenced_bytes: n,
        unique_bytes: n / 2,
      },
      NOW_NS,
    )
    .unwrap();
  }
  db.snapshot(seg).unwrap();
  for n in 1..volumes {
    db.mutate(seg, &Op::VolumeDestroyed { id: vid(n) }, NOW_NS)
      .unwrap();
  }
  db.snapshot(seg).unwrap();
  db
}

/// The payload length a snapshot slot holds now.
fn payload(seg: &AnchorSegment, slot: u8) -> usize {
  let mut len = [0u8; 8];
  seg
    .region_read_racy(RegionKind::Snapshot(0, slot), PAYLOAD_LEN, &mut len)
    .unwrap();
  usize::try_from(u64::from_le_bytes(len)).unwrap()
}

/// The marks a give-back carried to its end leaves: the ring at its tail's byte, each slot at its payload's end.
fn settled(seg: &AnchorSegment) -> RegionMarks {
  let tail = seg.ring_words(RegionKind::Log(0)).unwrap()[1].load(Ordering::Acquire);
  RegionMarks {
    ring: RING_BYTES + usize::try_from(tail).unwrap(),
    snapshots: [0, 1].map(|slot| PAYLOAD_BYTES + payload(seg, slot)),
  }
}

/// Gives back every dead page of partition 0, a `budget` a step: the steps it took.
fn give_back_whole(seg: &mut AnchorSegment, budget: usize) -> usize {
  let mut steps = 0;
  loop {
    steps += 1;
    if !seg.give_back_dead(0, (budget, None)).unwrap().more {
      return steps;
    }
  }
}

/// A-926 part 2, the migration (main's ruling of 2026-11-25). Do: run the history on a segment, then put its marks back
/// to none, as an anchor before them leaves a segment, and attach it as a daemon does; read the marks; take a snapshot
/// (a release) and give back what is dead a page a step. Expect, as attached, every region whole; after the give-back
/// each mark at its region's live end (the ring at its tail, each slot at its payload), the database the history's.
#[test]
fn an_old_segment_is_adopted_whole_then_its_give_back_makes_its_marks_exact() {
  let mut seg = segment("slates-db-marks-adopted");
  let db = history(&mut seg, 32);
  let live = db.partition().to_snapshot(0);
  drop(db);
  seg.unmark(0).unwrap();
  let (handoff, len) = seg.handoff().unwrap();
  let mut adopted = AnchorSegment::attach(&handoff, len, &identity()).unwrap();
  let whole = RegionMarks {
    ring: adopted.region_len(RegionKind::Log(0)).unwrap(),
    snapshots: [0, 1].map(|slot| adopted.region_len(RegionKind::Snapshot(0, slot)).unwrap()),
  };
  assert_eq!(adopted.region_marks(0).unwrap(), whole, "adopted whole");
  let (mut db, _) = recover(&mut adopted, 0, caps(), NOW_NS).unwrap();
  db.snapshot(&mut adopted).unwrap();
  let steps = give_back_whole(&mut adopted, PAGE as usize);
  assert!(steps > 1, "given back a page a step");
  assert_eq!(
    adopted.region_marks(0).unwrap(),
    settled(&adopted),
    "exact after the first give-back"
  );
  drop(db);
  let (recovered, _) = recover(&mut adopted, 0, caps(), NOW_NS).unwrap();
  assert_eq!(
    recovered.partition().to_snapshot(0),
    live,
    "the history's database"
  );
}

/// A-926 part 2 (main's ruling of 2026-11-25: one raise a page, the append's cost unchanged). Do: on a fresh segment,
/// log records past a snapshot, reading the ring's mark after each; time the appends against the same on a segment
/// adopted from before the marks, whose marks are whole and so never raised (the compare alone), best of five rounds
/// interleaved. Expect the mark to move once a page the ring's tail crosses, never past the page holding it, and
/// prints each segment's time an append.
#[test]
fn a_growing_log_raises_its_mark_once_a_page() {
  /// Shape: the rounds of each arm.
  const ROUNDS: usize = 5;
  let append_ns = |seg: &mut AnchorSegment| -> (u128, usize, u64, usize) {
    let (mut db, _) = recover(seg, 0, caps(), NOW_NS).unwrap();
    db.set_policy(SnapshotPolicy {
      bytes_between_snapshots: u64::MAX,
    });
    db.mutate(seg, &Op::VolumeCreated { record: volume(0) }, NOW_NS)
      .unwrap();
    db.snapshot(seg).unwrap();
    let mut raises = 0;
    let mut mark = seg.region_marks(0).unwrap().ring;
    let began_at = mark;
    let began = std::time::Instant::now();
    for n in 0..RECORDS {
      db.mutate(
        seg,
        &Op::VolumeAccounted {
          id: vid(0),
          referenced_bytes: n,
          unique_bytes: n,
        },
        NOW_NS,
      )
      .unwrap();
      let now = seg.region_marks(0).unwrap().ring;
      raises += usize::from(now != mark);
      mark = now;
    }
    let ns = began.elapsed().as_nanos() / u128::from(RECORDS);
    let tail = seg.ring_words(RegionKind::Log(0)).unwrap()[1].load(Ordering::Acquire);
    (ns, raises, tail, began_at)
  };
  let (mut marked_ns, mut whole_ns) = (u128::MAX, u128::MAX);
  for round in 0..ROUNDS {
    let mut fresh = segment(&format!("slates-db-marks-raises-{round}"));
    let (ns, raises, tail, began_at) = append_ns(&mut fresh);
    marked_ns = marked_ns.min(ns);
    let reached = (RING_BYTES as u64 + tail).div_ceil(PAGE) as usize;
    assert_eq!(
      raises,
      reached - began_at / PAGE as usize,
      "one raise a page the tail crossed past the mark it began at"
    );
    assert_eq!(
      fresh.region_marks(0).unwrap().ring as u64,
      reached as u64 * PAGE,
      "the mark at the page holding the tail"
    );
    let old = segment(&format!("slates-db-marks-whole-{round}"));
    old.unmark(0).unwrap();
    let (handoff, len) = old.handoff().unwrap();
    let mut adopted = AnchorSegment::attach(&handoff, len, &identity()).unwrap();
    let (ns, raises, _, _) = append_ns(&mut adopted);
    whole_ns = whole_ns.min(ns);
    assert_eq!(raises, 0, "whole marks raised never");
  }
  println!(
    "segment_marks: {RECORDS} appends, best of {ROUNDS}: {marked_ns} ns an append marked, {whole_ns} ns with the marks whole"
  );
}

/// A-926 part 2's crash oracle (main's ruling of 2026-11-25, as `crates/land/tests/oracle.rs` crashes at every write): the
/// give-back is a discard then a mark's fall a region, so a crash may come after any of those stores. Do: run the history
/// and take a reference give-back to its end on a segment of its own; then, for each count of stores, run the history
/// afresh, give back stopped after that many stores, recover the database, then resume the give-back to its end. Expect at
/// every crash point the recovered database the reference's, and the resumed give-back's marks the reference's.
#[test]
fn a_crash_at_every_store_of_a_give_back_recovers_the_database_and_resumes_to_the_reference() {
  let mut reference = segment("slates-db-marks-reference");
  let db = history(&mut reference, 32);
  let live = db.partition().to_snapshot(0);
  drop(db);
  give_back_whole(&mut reference, usize::MAX);
  let marks = reference.region_marks(0).unwrap();
  assert_eq!(marks, settled(&reference), "the reference settled");
  println!(
    "segment_marks: the reference's payloads {:?}, its marks {marks:?}",
    [0, 1].map(|slot| payload(&reference, slot))
  );
  // Three regions, a discard and a fall each, and one store past them: a give-back that ran whole.
  for stores in 0..=7 {
    let mut seg = segment(&format!("slates-db-marks-crash-{stores}"));
    drop(history(&mut seg, 32));
    let _ = seg.give_back_dead(0, (usize::MAX, Some(stores))).unwrap();
    let (recovered, _) = recover(&mut seg, 0, caps(), NOW_NS).unwrap();
    assert_eq!(
      recovered.partition().to_snapshot(0),
      live,
      "crashed after {stores} stores: the database recovered"
    );
    drop(recovered);
    give_back_whole(&mut seg, usize::MAX);
    assert_eq!(
      seg.region_marks(0).unwrap(),
      marks,
      "crashed after {stores} stores: the resumed give-back reaches the reference"
    );
  }
}
