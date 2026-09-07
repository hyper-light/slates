//! The database's tests (Phase 2 task 2; §4.8, AC-2.3's durability half, AC-2.4's lease
//! fencing, AC-2.7's shape): generated operation histories applied through the database into a
//! real anchor segment with a crash (the database dropped, recovery from the segment) at random
//! points; the recovered partition must equal the live one after every crash. The torn tail: a
//! record corrupted in the ring is cut off and overwritten. Hostile records: a length of
//! `u32::MAX`, a foreign magic, a bad checksum, a truncated header, each refused without a
//! panic. Leases: one holder, a second refused, expiry through the wheel, epoch fencing.
//! Snapshots: the cadence, the trim, recovery from a snapshot plus its tail.
// Test harness code: an unwrap here is a failed test, which is what it should be. proptest's
// strategy types carry `Arc` (D-8's harness exception).
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::disallowed_types
)]

use std::sync::atomic::Ordering;

use proptest::prelude::*;
use slates_anchor::{AnchorSegment, Geometry, RegionKind};
use slates_db::catalog::{
  AttachForm, AttachmentRecord, AuditKind, AuditRecord, BaseRecord, CompletionRecord, Consumer,
  GrantRecord, GrantScope, GrantState, GrantSurface, LandingLeaseRecord, LandingRecord,
  LandingState, LeaseRecord, LineageEdge, NamePolicy, PlacementState, PolicyRecord, Principal,
  Rights, Role, SizeClass, SnapshotId, SnapshotRecord, VolumeId, VolumeRecord, VolumeState,
};
use slates_db::partition::PartitionCaps;
use slates_db::replay::{SnapshotPolicy, recover};
use slates_db::{Db, DbError, Op};
use slates_machine::facts::Identity;
use slates_wire::request::Seen;

/// Shape: the segment page.
const PAGE: u64 = 4096;
/// Shape: the log ring of the tests (bytes).
const LOG_BYTES: u64 = 1 << 20;
/// Shape: a snapshot slot of the tests (bytes).
const SNAPSHOT_BYTES: u64 = 1 << 20;

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

fn segment(name: &str, log_bytes: u64) -> AnchorSegment {
  AnchorSegment::create(
    name,
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
    green_chain_bytes: 1 << 20,
  }
}

fn open(segment: &mut AnchorSegment) -> Db {
  recover(segment, 0, caps(), 0).unwrap().0
}

fn vid(n: u64) -> VolumeId {
  let mut bytes = [0u8; 16];
  bytes[..8].copy_from_slice(&n.to_be_bytes());
  VolumeId { bytes }
}

fn principal(n: u32) -> Principal {
  Principal::Uid { uid: 1000 + n }
}

fn volume(n: u64, name: &str) -> VolumeRecord {
  VolumeRecord {
    id: vid(n),
    name: name.to_owned(),
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
    owner: principal(0),
    access: vec![],
    created_ns: 0,
  }
}

// ---------------------------------------------------------------- generated histories

/// One step of a generated history, chosen against the live ids at generation time by
/// indices; the interpreter maps indices onto the ids that exist when the step runs.
#[derive(Clone, Debug)]
enum Step {
  Create,
  State(usize, u8),
  Account(usize, u64, u64),
  Access(usize),
  Destroy(usize),
  Snapshot(usize),
  Place(usize, usize),
  DropSnapshot(usize, usize),
  Lineage(usize, usize),
  Lease(usize, u32, u64),
  Release(usize),
  Attach(usize, u32),
  Detach(usize),
  Complete(u32, u32),
  Acknowledge(u32, u32),
  Grant(usize),
  GrantState(usize, u8),
  LandingLease(u32, u64),
  Landing(usize),
  LandingState(usize, u8),
  Audit(u8),
  GreenAdvance(usize),
  Crash,
}

fn step() -> impl Strategy<Value = Step> {
  prop_oneof![
    3 => Just(Step::Create),
    2 => (0..8usize, 0..8u8).prop_map(|(v, s)| Step::State(v, s)),
    2 => (0..8usize, 0..1_000_000u64, 0..1_000_000u64).prop_map(|(v, r, u)| Step::Account(v, r, u)),
    1 => (0..8usize).prop_map(Step::Access),
    1 => (0..8usize).prop_map(Step::Destroy),
    2 => (0..8usize).prop_map(Step::Snapshot),
    1 => (0..8usize, 0..4usize).prop_map(|(v, s)| Step::Place(v, s)),
    1 => (0..8usize, 0..4usize).prop_map(|(v, s)| Step::DropSnapshot(v, s)),
    1 => (0..8usize, 0..8usize).prop_map(|(c, o)| Step::Lineage(c, o)),
    2 => (0..8usize, 0..3u32, 1..1_000u64).prop_map(|(v, p, t)| Step::Lease(v, p, t)),
    1 => (0..8usize).prop_map(Step::Release),
    1 => (0..8usize, 0..3u32).prop_map(|(v, c)| Step::Attach(v, c)),
    1 => (0..8usize).prop_map(Step::Detach),
    2 => (0..3u32, 0..16u32).prop_map(|(c, s)| Step::Complete(c, s)),
    1 => (0..3u32, 0..16u32).prop_map(|(c, s)| Step::Acknowledge(c, s)),
    1 => (0..8usize).prop_map(Step::Grant),
    1 => (0..8usize, 0..4u8).prop_map(|(g, s)| Step::GrantState(g, s)),
    1 => (0..2u32, 1..100u64).prop_map(|(h, t)| Step::LandingLease(h, t)),
    1 => (0..8usize).prop_map(Step::Landing),
    1 => (0..8usize, 0..10u8).prop_map(|(l, s)| Step::LandingState(l, s)),
    1 => (0..7u8).prop_map(Step::Audit),
    2 => (0..8usize).prop_map(Step::GreenAdvance),
    1 => Just(Step::Crash),
  ]
}

/// The interpreter's counters: the ids it handed out.
#[derive(Default)]
struct Ids {
  next_volume: u64,
  live: Vec<u64>,
  snapshots: Vec<(u64, u64)>,
  next_attachment: u64,
  attachments: Vec<u64>,
  next_grant: u64,
  next_landing: u64,
  audit_seq: u64,
}

fn volume_state(n: u8) -> VolumeState {
  match n {
    0 => VolumeState::Creating,
    1 => VolumeState::Live,
    2 => VolumeState::Sealing,
    3 => VolumeState::Landing,
    4 => VolumeState::Archived,
    5 => VolumeState::Restoring,
    6 => VolumeState::Destroying,
    _ => VolumeState::Destroyed,
  }
}

fn grant_state(n: u8) -> GrantState {
  match n {
    0 => GrantState::Issued,
    1 => GrantState::Consumed,
    2 => GrantState::Expired,
    _ => GrantState::Revoked,
  }
}

fn landing_state(n: u8) -> LandingState {
  match n {
    0 => LandingState::Planning,
    1 => LandingState::AwaitingGrant,
    2 => LandingState::Validating,
    3 => LandingState::Writing,
    4 => LandingState::Syncing,
    5 => LandingState::Advancing,
    6 => LandingState::Done,
    7 => LandingState::Partial,
    8 => LandingState::Refused,
    _ => LandingState::Aborted,
  }
}

fn audit_kind(n: u8) -> AuditKind {
  match n {
    0 => AuditKind::GrantIssued,
    1 => AuditKind::GrantRevoked,
    2 => AuditKind::LandingPlanned,
    3 => AuditKind::LandingValidated,
    4 => AuditKind::EntryWritten,
    5 => AuditKind::EntryRefused,
    _ => AuditKind::LandingFinished,
  }
}

fn pick<T: Copy>(items: &[T], index: usize) -> Option<T> {
  if items.is_empty() {
    None
  } else {
    Some(items[index % items.len()])
  }
}

/// The operation a step means against the ids that exist now, or `None` when it names
/// nothing (the history simply skips it).
fn op_for(step: &Step, ids: &mut Ids, now_ns: u64) -> Option<Op> {
  Some(match step {
    Step::Create => {
      let n = ids.next_volume;
      ids.next_volume += 1;
      Op::VolumeCreated {
        record: volume(n, &format!("v{n}")),
      }
    }
    Step::State(v, s) => Op::VolumeStateChanged {
      id: vid(pick(&ids.live, *v)?),
      state: volume_state(*s),
    },
    Step::Account(v, r, u) => Op::VolumeAccounted {
      id: vid(pick(&ids.live, *v)?),
      referenced_bytes: *r,
      unique_bytes: *u,
    },
    Step::Access(v) => Op::AccessChanged {
      id: vid(pick(&ids.live, *v)?),
      access: vec![slates_db::catalog::AccessEntry {
        principal: principal(2),
        rights: Rights {
          read: true,
          write: false,
          admin: false,
        },
      }],
    },
    Step::Destroy(v) => Op::VolumeDestroyed {
      id: vid(pick(&ids.live, *v)?),
    },
    Step::Snapshot(v) => {
      let volume = pick(&ids.live, *v)?;
      let id = ids
        .snapshots
        .iter()
        .filter(|(vol, _)| *vol == volume)
        .map(|(_, s)| *s)
        .max()
        .map_or(1, |s| s + 1);
      Op::SnapshotTaken {
        record: SnapshotRecord {
          id: SnapshotId { value: id },
          volume: vid(volume),
          epoch: id,
          identity: None,
          placed: PlacementState::Local,
          taken_ns: now_ns,
        },
      }
    }
    Step::Place(v, s) => {
      let (volume, id) = pick(&ids.snapshots, *v + *s)?;
      Op::SnapshotPlaced {
        volume: vid(volume),
        id: SnapshotId { value: id },
        placed: PlacementState::Placed {
          region: vec![1, 2],
          mirror: None,
        },
      }
    }
    Step::DropSnapshot(v, s) => {
      let (volume, id) = pick(&ids.snapshots, *v + *s)?;
      Op::SnapshotDestroyed {
        volume: vid(volume),
        id: SnapshotId { value: id },
      }
    }
    Step::Lineage(c, o) => Op::LineageAdded {
      edge: LineageEdge {
        child: vid(pick(&ids.live, *c)?),
        origin_volume: vid(pick(&ids.live, *o)?),
        origin_snapshot: SnapshotId { value: 1 },
      },
    },
    Step::Lease(v, p, term) => Op::LeaseTaken {
      volume: vid(pick(&ids.live, *v)?),
      lease: LeaseRecord {
        holder: principal(*p),
        epoch: 1,
        expires_ns: now_ns + term,
      },
    },
    Step::Release(v) => Op::LeaseReleased {
      volume: vid(pick(&ids.live, *v)?),
    },
    Step::Attach(v, c) => {
      let id = ids.next_attachment;
      ids.next_attachment += 1;
      Op::AttachmentAdded {
        record: AttachmentRecord {
          id,
          volume: vid(pick(&ids.live, *v)?),
          consumer: Consumer::Sdk { client: *c },
          snapshot: None,
          form: AttachForm::Root,
          principal: principal(*c),
        },
      }
    }
    Step::Detach(a) => Op::AttachmentRemoved {
      id: pick(&ids.attachments, *a)?,
    },
    Step::Complete(c, s) => Op::CompletionRecorded {
      record: CompletionRecord {
        client: *c,
        sequence: *s,
        result: vec![u8::try_from(*s).unwrap_or(0); 3],
      },
    },
    Step::Acknowledge(c, s) => Op::CompletionsAcknowledged {
      client: *c,
      up_to: *s,
    },
    step @ (Step::Grant(..)
    | Step::GrantState(..)
    | Step::LandingLease(..)
    | Step::Landing(..)
    | Step::LandingState(..)
    | Step::Audit(..)
    | Step::GreenAdvance(..)) => return op_for_service(step, ids, now_ns),
    Step::Crash => return None,
  })
}

/// The service operations (grants, landings, audit, green chains), split from [`op_for`] to
/// keep each dispatch under the cognitive-complexity bound. Called only for those steps.
fn op_for_service(step: &Step, ids: &mut Ids, now_ns: u64) -> Option<Op> {
  Some(match step {
    Step::Grant(v) => {
      let id = ids.next_grant;
      ids.next_grant += 1;
      Op::GrantIssued {
        record: GrantRecord {
          id,
          principal: principal(0),
          surface: GrantSurface::Cli,
          volume: vid(pick(&ids.live, *v)?),
          snapshot: SnapshotId { value: 1 },
          target: "/t".into(),
          manifest: [7; 32],
          scope: GrantScope::Once,
          issued_ns: now_ns,
          expires_ns: now_ns + 1_000,
          state: GrantState::Issued,
        },
      }
    }
    Step::GrantState(g, s) => Op::GrantStateChanged {
      id: (*g as u64).checked_rem(ids.next_grant.max(1))?,
      state: grant_state(*s),
    },
    Step::LandingLease(h, term) => Op::LandingLeaseTaken {
      record: LandingLeaseRecord {
        target: "/t".into(),
        holder: u64::from(*h),
        generation: now_ns,
        expires_ns: now_ns + term,
      },
    },
    Step::Landing(v) => {
      let id = ids.next_landing;
      ids.next_landing += 1;
      Op::LandingRecorded {
        record: LandingRecord {
          id,
          volume: vid(pick(&ids.live, *v)?),
          snapshot: SnapshotId { value: 1 },
          target: "/t".into(),
          manifest: [7; 32],
          grant: None,
          state: LandingState::Planning,
          written: 0,
          conflicts: 0,
        },
      }
    }
    Step::LandingState(l, s) => Op::LandingStateChanged {
      id: (*l as u64).checked_rem(ids.next_landing.max(1))?,
      state: landing_state(*s),
      written: u32::from(*s),
      conflicts: 0,
    },
    Step::Audit(k) => {
      ids.audit_seq += 1;
      Op::AuditAppended {
        record: AuditRecord {
          seq: ids.audit_seq,
          at_ns: now_ns,
          kind: audit_kind(*k),
          principal: principal(0),
          grant: None,
          landing: None,
          manifest: None,
          outcome: None,
        },
      }
    }
    Step::GreenAdvance(v) => Op::GreenAdvanced {
      green: vid(pick(&ids.live, *v)?),
      // The bytes are opaque to the database; a small deterministic value keeps the chain well under
      // its byte budget over a run so recovery, not the cap, is what the model exercises.
      increment: (*v as u64).to_le_bytes().to_vec(),
    },
    _ => return None,
  })
}

/// Keeps the interpreter's view of live ids in step with what the partition accepted.
fn note_applied(op: &Op, ids: &mut Ids) {
  match op {
    Op::VolumeCreated { record } => ids
      .live
      .push(u64::from_be_bytes(record.id.bytes[..8].try_into().unwrap())),
    Op::VolumeDestroyed { id } => {
      let n = u64::from_be_bytes(id.bytes[..8].try_into().unwrap());
      ids.live.retain(|v| *v != n);
      ids.snapshots.retain(|(v, _)| *v != n);
    }
    Op::SnapshotTaken { record } => ids.snapshots.push((
      u64::from_be_bytes(record.volume.bytes[..8].try_into().unwrap()),
      record.id.value,
    )),
    Op::SnapshotDestroyed { volume, id } => {
      let n = u64::from_be_bytes(volume.bytes[..8].try_into().unwrap());
      ids.snapshots.retain(|(v, s)| !(*v == n && *s == id.value));
    }
    Op::AttachmentAdded { record } => ids.attachments.push(record.id),
    Op::AttachmentRemoved { id } => ids.attachments.retain(|a| a != id),
    _ => {}
  }
}

proptest! {
  #![proptest_config(ProptestConfig { cases: 60, failure_persistence: None, ..ProptestConfig::default() })]

  /// AC-2.3's durability half and AC-2.4's fencing under generated histories: after every
  /// crash the recovered partition equals the live one; refused operations were never
  /// recorded; the snapshot cadence and the log trim never lose a record.
  #[test]
  fn every_recovery_equals_the_live_partition(steps in proptest::collection::vec(step(), 1..250), snapshot_every in 1u64..20_000) {
    let mut seg = segment("slates-db-model", LOG_BYTES);
    let mut db = open(&mut seg);
    db.set_policy(SnapshotPolicy { bytes_between_snapshots: snapshot_every });
    let mut ids = Ids::default();
    let mut now_ns = 0u64;
    let mut applied = 0usize;
    for s in &steps {
      now_ns += 100;
      if matches!(s, Step::Crash) {
        let live = db.partition().to_snapshot(0);
        let seq = db.next_seq();
        drop(db);
        let (recovered, stats) = recover(&mut seg, 0, caps(), now_ns).unwrap();
        prop_assert_eq!(recovered.partition().to_snapshot(0), live, "recovery differs from the live partition");
        prop_assert_eq!(recovered.next_seq(), seq);
        prop_assert!(!stats.torn, "a clean crash has no torn tail");
        db = recovered;
        db.set_policy(SnapshotPolicy { bytes_between_snapshots: snapshot_every });
        continue;
      }
      let Some(op) = op_for(s, &mut ids, now_ns) else { continue };
      match db.mutate(&mut seg, &op, now_ns) {
        Ok(_) => {
          note_applied(&op, &mut ids);
          applied += 1;
        }
        Err(DbError::NotFound | DbError::AlreadyExists { .. } | DbError::LeaseHeld { .. } | DbError::StaleLease { .. } | DbError::StaleCompletion { .. } | DbError::Capacity { .. }) => {}
        Err(other) => prop_assert!(false, "unexpected refusal {other:?}"),
      }
      for expired in db.partition_mut().expired_leases(now_ns) {
        db.mutate(&mut seg, &Op::LeaseReleased { volume: expired }, now_ns).unwrap();
      }
    }
    let live = db.partition().to_snapshot(0);
    let seq = db.next_seq();
    drop(db);
    let (recovered, _) = recover(&mut seg, 0, caps(), now_ns).unwrap();
    prop_assert_eq!(recovered.partition().to_snapshot(0), live);
    prop_assert_eq!(recovered.next_seq(), seq);
    prop_assert!(usize::try_from(seq).unwrap() >= applied);
  }
}

// ---------------------------------------------------------------- the torn tail and hostile bytes

/// Five volumes, a sixth record appended, then a byte flipped inside its body: the segment,
/// the partition before the sixth, and the tail offsets before and after it.
fn segment_with_a_torn_sixth_record() -> (
  AnchorSegment,
  slates_db::partition::PartitionSnapshot,
  u64,
  u64,
) {
  let mut seg = segment("slates-db-torn", LOG_BYTES);
  let mut db = open(&mut seg);
  for n in 0..5 {
    db.mutate(
      &mut seg,
      &Op::VolumeCreated {
        record: volume(n, &format!("v{n}")),
      },
      0,
    )
    .unwrap();
  }
  let before = db.partition().to_snapshot(0);
  let tail_before = seg.ring_words(RegionKind::Log(0)).unwrap()[1].load(Ordering::Acquire);
  db.mutate(
    &mut seg,
    &Op::VolumeCreated {
      record: volume(5, "v5"),
    },
    0,
  )
  .unwrap();
  let tail_after = seg.ring_words(RegionKind::Log(0)).unwrap()[1].load(Ordering::Acquire);
  drop(db);
  let ring = seg.region_bytes_mut(RegionKind::Log(0)).unwrap();
  let at = slates_anchor::layout::RING_BYTES + usize::try_from(tail_before).unwrap() + 40;
  ring[at] ^= 0xff;
  (seg, before, tail_before, tail_after)
}

/// The last record's bytes are corrupted in the ring (a crash inside the copy): recovery keeps
/// everything before it, cuts it off, and the next mutation overwrites it.
#[test]
fn a_torn_tail_is_cut_off_and_overwritten() {
  let (mut seg, before, tail_before, tail_after) = segment_with_a_torn_sixth_record();
  let (recovered, stats) = recover(&mut seg, 0, caps(), 0).unwrap();
  assert!(stats.torn, "the corrupted record is the torn tail");
  assert_eq!(
    recovered.partition().to_snapshot(0),
    before,
    "everything acknowledged before it is present"
  );
  assert_eq!(
    recovered.next_seq(),
    5,
    "the torn record's sequence is reused"
  );
  assert_eq!(stats.replayed_records, 5);
  let tail_cut = seg.ring_words(RegionKind::Log(0)).unwrap()[1].load(Ordering::Acquire);
  assert_eq!(
    tail_cut, tail_before,
    "the tail returned to the last verified byte"
  );
  assert!(tail_after > tail_before);
  assert_next_mutation_overwrites_the_torn_record(&mut seg, recovered);
}

/// The next mutation lands over the torn record and recovers cleanly.
fn assert_next_mutation_overwrites_the_torn_record(seg: &mut AnchorSegment, mut db: Db) {
  db.mutate(
    seg,
    &Op::VolumeCreated {
      record: volume(9, "v9"),
    },
    0,
  )
  .unwrap();
  let live = db.partition().to_snapshot(0);
  drop(db);
  let (again, stats) = recover(seg, 0, caps(), 0).unwrap();
  assert!(!stats.torn);
  assert_eq!(again.partition().to_snapshot(0), live);
  assert!(again.partition().volume_by_name("v9").is_some());
  assert!(again.partition().volume_by_name("v5").is_none());
}

/// Hostile bytes at the tail: a length of `u32::MAX`, a foreign magic, a bad checksum and a
/// truncated header each stop the replay before them with everything earlier intact.
#[test]
fn hostile_records_are_refused_without_a_panic() {
  for (name, poison) in [
    ("len", 1u8),
    ("magic", 2u8),
    ("crc", 3u8),
    ("truncated", 4u8),
  ] {
    let mut seg = segment(&format!("slates-db-hostile-{name}"), LOG_BYTES);
    let mut db = open(&mut seg);
    for n in 0..3 {
      db.mutate(
        &mut seg,
        &Op::VolumeCreated {
          record: volume(n, &format!("v{n}")),
        },
        0,
      )
      .unwrap();
    }
    let before = db.partition().to_snapshot(0);
    drop(db);
    let tail = seg.ring_words(RegionKind::Log(0)).unwrap()[1].load(Ordering::Acquire);
    let at = slates_anchor::layout::RING_BYTES + usize::try_from(tail).unwrap();
    {
      let ring = seg.region_bytes_mut(RegionKind::Log(0)).unwrap();
      let mut header = [0u8; 32];
      header[0..4].copy_from_slice(&slates_db::record::RECORD_MAGIC.to_le_bytes());
      header[4..8].copy_from_slice(&64u32.to_le_bytes());
      header[8..16].copy_from_slice(&3u64.to_le_bytes());
      match poison {
        1 => header[4..8].copy_from_slice(&u32::MAX.to_le_bytes()),
        2 => header[0..4].copy_from_slice(b"EVIL"),
        3 => header[16..20].copy_from_slice(&0xdead_beefu32.to_le_bytes()),
        _ => {}
      }
      let len = if poison == 4 { 12 } else { 32 + 64 };
      ring[at..at + 32.min(len)].copy_from_slice(&header[..32.min(len)]);
    }
    let words = seg.ring_words(RegionKind::Log(0)).unwrap();
    let bump = if poison == 4 { 12 } else { 32 + 64 };
    words[1].store(tail + bump, Ordering::Release);
    let (recovered, stats) = recover(&mut seg, 0, caps(), 0).unwrap();
    assert!(stats.torn, "{name}");
    assert_eq!(recovered.partition().to_snapshot(0), before, "{name}");
    assert_eq!(stats.replayed_records, 3, "{name}");
  }
}

// ---------------------------------------------------------------- leases

fn lease(holder: u32, epoch: u64, expires_ns: u64) -> Op {
  Op::LeaseTaken {
    volume: vid(1),
    lease: LeaseRecord {
      holder: principal(holder),
      epoch,
      expires_ns,
    },
  }
}

/// AC-2.4's core: one holder at a time; a stale epoch is refused and never applied; the
/// holder renews with its epoch.
#[test]
fn leases_fence_by_epoch() {
  let mut seg = segment("slates-db-leases-fence", LOG_BYTES);
  let mut db = open(&mut seg);
  db.mutate(
    &mut seg,
    &Op::VolumeCreated {
      record: volume(1, "v1"),
    },
    0,
  )
  .unwrap();
  db.mutate(&mut seg, &lease(1, 1, 1_000), 10).unwrap();
  assert_eq!(db.partition().lease_of(&principal(1)), Some(vid(1)));
  assert!(matches!(
    db.mutate(&mut seg, &lease(2, 2, 2_000), 20),
    Err(DbError::LeaseHeld { epoch: 1 })
  ));
  assert!(matches!(
    db.mutate(&mut seg, &lease(1, 0, 2_000), 20),
    Err(DbError::StaleLease { current: 1 })
  ));
  db.mutate(&mut seg, &lease(1, 1, 3_000), 30).unwrap();
  assert_eq!(
    db.partition()
      .volume(vid(1))
      .unwrap()
      .lease
      .as_ref()
      .unwrap()
      .expires_ns,
    3_000
  );
  let seq = db.next_seq();
  drop(db);
  let (recovered, _) = recover(&mut seg, 0, caps(), 30).unwrap();
  assert_eq!(
    recovered.next_seq(),
    seq,
    "refused leases were never recorded"
  );
}

/// Expiry through the wheel: the next holder takes epoch + 1; recovery rebuilds the wheel.
#[test]
fn leases_expire_through_the_wheel_and_the_wheel_survives_recovery() {
  let mut seg = segment("slates-db-leases-expire", LOG_BYTES);
  let mut db = open(&mut seg);
  db.mutate(
    &mut seg,
    &Op::VolumeCreated {
      record: volume(1, "v1"),
    },
    0,
  )
  .unwrap();
  db.mutate(&mut seg, &lease(1, 1, 3_000), 30).unwrap();
  assert!(db.partition_mut().expired_leases(2_999).is_empty());
  assert_eq!(db.partition_mut().expired_leases(3_000), vec![vid(1)]);
  assert!(
    matches!(
      db.mutate(&mut seg, &lease(2, 1, 5_000), 3_000),
      Err(DbError::StaleLease { current: 1 })
    ),
    "the new holder needs epoch + 1"
  );
  db.mutate(&mut seg, &lease(2, 2, 5_000), 3_000).unwrap();
  assert_eq!(db.partition().lease_of(&principal(2)), Some(vid(1)));
  assert_eq!(db.partition().lease_of(&principal(1)), None);
  db.mutate(&mut seg, &Op::LeaseReleased { volume: vid(1) }, 3_100)
    .unwrap();
  assert!(db.partition().volume(vid(1)).unwrap().lease.is_none());
  db.mutate(&mut seg, &lease(3, 3, 9_000), 4_000).unwrap();
  drop(db);
  let (mut recovered, _) = recover(&mut seg, 0, caps(), 4_000).unwrap();
  assert!(recovered.partition_mut().expired_leases(8_999).is_empty());
  assert_eq!(
    recovered.partition_mut().expired_leases(9_000),
    vec![vid(1)]
  );
}

// ---------------------------------------------------------------- completions and snapshots

/// Completion records survive recovery and acknowledgement releases them (RIFL).
#[test]
fn completions_are_exactly_once_across_recovery() {
  let mut seg = segment("slates-db-completions", LOG_BYTES);
  let mut db = open(&mut seg);
  db.mutate(
    &mut seg,
    &Op::CompletionRecorded {
      record: CompletionRecord {
        client: 7,
        sequence: 3,
        result: vec![1, 2, 3],
      },
    },
    0,
  )
  .unwrap();
  db.mutate(
    &mut seg,
    &Op::CompletionRecorded {
      record: CompletionRecord {
        client: 7,
        sequence: 4,
        result: vec![4],
      },
    },
    0,
  )
  .unwrap();
  drop(db);
  let (mut db, _) = recover(&mut seg, 0, caps(), 0).unwrap();
  assert_eq!(
    db.partition().completion(7, 3),
    Seen::Completed(vec![1, 2, 3])
  );
  assert_eq!(db.partition().completion(7, 5), Seen::New);
  db.mutate(
    &mut seg,
    &Op::CompletionsAcknowledged {
      client: 7,
      up_to: 3,
    },
    0,
  )
  .unwrap();
  assert_eq!(db.partition().completion(7, 3), Seen::Acknowledged);
  assert_eq!(db.partition().completion(7, 4), Seen::Completed(vec![4]));
}

/// The snapshot cadence: a small policy snapshots often, the log is trimmed behind each, and
/// recovery restores the snapshot plus its tail; a full ring snapshots and retries.
#[test]
fn snapshots_trim_the_log_and_recovery_restores_them_with_the_tail() {
  let mut seg = segment("slates-db-snapshots", LOG_BYTES);
  let mut db = open(&mut seg);
  db.set_policy(SnapshotPolicy {
    bytes_between_snapshots: 2_000,
  });
  for n in 0..200 {
    db.mutate(
      &mut seg,
      &Op::VolumeCreated {
        record: volume(n, &format!("v{n}")),
      },
      0,
    )
    .unwrap();
  }
  assert!(db.snapshots_taken() > 5, "{}", db.snapshots_taken());
  let (used, _, _) = db.log_usage(&seg).unwrap();
  assert!(
    used < 4_000,
    "the log holds only the tail after the last snapshot: {used}"
  );
  let live = db.partition().to_snapshot(0);
  drop(db);
  let (recovered, stats) = recover(&mut seg, 0, caps(), 0).unwrap();
  assert!(stats.snapshot.is_some());
  assert_eq!(recovered.partition().to_snapshot(0), live);
  assert_eq!(recovered.partition().volume_count(), 200);
  assert_eq!(recovered.next_seq(), 200);
}

/// A ring too small for many records: the append snapshots and retries, never refuses.
#[test]
fn a_full_log_snapshots_and_retries() {
  let mut small = segment("slates-db-small-log", PAGE * 2);
  let mut db = open(&mut small);
  db.set_policy(SnapshotPolicy {
    bytes_between_snapshots: u64::MAX,
  });
  for n in 0..100 {
    db.mutate(
      &mut small,
      &Op::VolumeCreated {
        record: volume(n, &format!("v{n}")),
      },
      0,
    )
    .unwrap();
  }
  assert!(db.snapshots_taken() >= 1);
  let live = db.partition().to_snapshot(0);
  drop(db);
  let (recovered, _) = recover(&mut small, 0, caps(), 0).unwrap();
  assert_eq!(recovered.partition().to_snapshot(0), live);
}

/// The snapshot policy derives from the recovery budget and the measured replay throughput.
#[test]
fn the_snapshot_policy_derives_from_the_budget_and_the_measured_replay() {
  let p = SnapshotPolicy::derive(1_000_000_000, 700);
  assert_eq!(p.value.bytes_between_snapshots, 700_000_000);
  assert_eq!(
    SnapshotPolicy::derive(1_000_000_000, 0)
      .value
      .bytes_between_snapshots,
    1_000_000
  );
  assert!(p.anchors.contains(&"replay_bytes_per_us"));
}
