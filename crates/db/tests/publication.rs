//! Transaction publication and its two failure classes (§4.8 transactions, AC-2.3's durability half;
//! AUD-06): a transaction whose record cannot be made durable — nothing appended, no snapshot
//! published — is **rolled back** to the segment's durable state, so its effects and its completion
//! record are gone together and a retry re-executes rather than reading a success from memory; a
//! maintenance snapshot that fails **after** a durable append never fails the commit — the record
//! stands, the snapshot is deferred to the next commit and counted. Each is driven deterministically
//! through the database's one-shot publication fault and checked against a fresh recovery from the
//! same segment, the oracle a restart would consult — the same predicate is asked of the live
//! partition and of the recovered one, so the two can never disagree unnoticed.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_anchor::{AnchorSegment, Geometry};
use slates_db::catalog::{
  BaseRecord, CompletionRecord, NamePolicy, PolicyRecord, Principal, Role, SizeClass, SnapshotId,
  VolumeId, VolumeRecord, VolumeState,
};
use slates_db::partition::{Partition, PartitionCaps};
use slates_db::replay::{PublicationFault, Recovered, SnapshotPolicy, recover};
use slates_db::{Db, DbError, Op};
use slates_machine::facts::Identity;
use slates_wire::request::Seen;

/// Shape: the segment page.
const PAGE: u64 = 4096;
/// Shape: the log ring of the tests (bytes) — room for every record the histories below append.
const LOG_BYTES: u64 = 1 << 20;
/// Shape: a snapshot slot of the tests (bytes).
const SNAPSHOT_BYTES: u64 = 1 << 20;
/// Shape: the clock the histories run at (nanoseconds); leases play no part here.
const NOW_NS: u64 = 1_000;
/// Shape: the completion origin and client the recorded verbs are keyed by; each verb takes its own
/// sequence (its volume number), as one client's requests do.
const ORIGIN: u64 = 7;
const CLIENT: u32 = 3;

/// The completion sequence the recorded create of volume `n` is keyed by.
fn sequence_of(n: u64) -> u32 {
  u32::try_from(n).unwrap()
}

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

fn open(segment: &mut AnchorSegment) -> Db {
  recover(segment, 0, caps(), NOW_NS).unwrap().0
}

/// A fresh recovery from the segment — the state a restart would build — and what it replayed.
fn recovered(segment: &mut AnchorSegment) -> (Db, Recovered) {
  recover(segment, 0, caps(), NOW_NS).unwrap()
}

fn vid(n: u64) -> VolumeId {
  let mut bytes = [0u8; 16];
  bytes[..8].copy_from_slice(&n.to_be_bytes());
  VolumeId { bytes }
}

fn volume(n: u64, name: &str) -> VolumeRecord {
  VolumeRecord {
    id: vid(n),
    name: name.to_owned(),
    owner_shard: 0,
    policy: PolicyRecord {
      size: SizeClass::Bounded { limit: 1 << 20 },
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
    access: Vec::new(),
    created_ns: NOW_NS,
  }
}

/// The recorded verb the database serves: the effect (a volume created) and its completion record,
/// applied inside one transaction exactly as the daemon's `run_recorded` applies them.
fn recorded_create(db: &mut Db, segment: &mut AnchorSegment, n: u64, name: &str) {
  db.begin();
  db.mutate(
    segment,
    &Op::VolumeCreated {
      record: volume(n, name),
    },
    NOW_NS,
  )
  .unwrap();
  db.mutate(
    segment,
    &Op::CompletionRecorded {
      record: CompletionRecord {
        origin: ORIGIN,
        client: CLIENT,
        sequence: sequence_of(n),
        result: name.as_bytes().to_vec(),
      },
    },
    NOW_NS,
  )
  .unwrap();
}

/// Whether a partition holds the recorded create of volume `n` — its effect **and** its completion
/// record, answering `name` — or neither: the pair the transaction makes one durable step of.
fn holds_recorded(partition: &Partition, n: u64, name: &str) -> bool {
  let completion = partition.completion(ORIGIN, CLIENT, sequence_of(n));
  partition.volume(vid(n)).is_some()
    && matches!(completion, Seen::Completed(bytes) if bytes == name.as_bytes())
}

/// Whether a partition holds nothing of the recorded create of volume `n`: no effect, and its
/// completion `New` — a retry would re-execute, never be answered from memory.
fn holds_nothing_of(partition: &Partition, n: u64) -> bool {
  partition.volume(vid(n)).is_none()
    && partition.completion(ORIGIN, CLIENT, sequence_of(n)) == Seen::New
}

/// AC-2.3 (AUD-06): a transaction whose record cannot be appended — nothing written — is rolled back
/// to the segment's durable state: its effect and its completion record are both gone from the live
/// partition (a retry finds the request `New`, never a success in memory), the durable transaction
/// before it is untouched, the sequence has not moved, a fresh recovery from the segment agrees, and
/// the same transaction run again without the fault is durable everywhere. Non-vacuous: the rollback
/// is counted, and the refusal names the sequence it would have taken.
#[test]
fn a_publication_refused_before_the_append_rolls_the_transaction_back_and_a_retry_re_executes() {
  let mut seg = segment("slates-db-unpublished");
  let mut db = open(&mut seg);
  recorded_create(&mut db, &mut seg, 1, "durable");
  db.commit(&mut seg).unwrap();
  let seq_before = db.next_seq();

  db.inject_publication_fault(Some(PublicationFault::BeforeAppend));
  recorded_create(&mut db, &mut seg, 2, "unpublished");
  assert!(
    holds_recorded(db.partition(), 2, "unpublished"),
    "inside the transaction the effect and its completion are applied (later operations see them)"
  );
  let refused = db.commit(&mut seg).unwrap_err();
  assert!(
    matches!(refused, DbError::Unpublished { seq, .. } if seq == seq_before),
    "the commit is refused as unpublished at the sequence it would have taken: {refused}"
  );

  // The live partition is back at its durable state, and the segment agrees.
  let rolled_back =
    holds_nothing_of(db.partition(), 2) && holds_recorded(db.partition(), 1, "durable");
  let (fresh, _) = recovered(&mut seg);
  let segment_agrees =
    holds_nothing_of(fresh.partition(), 2) && holds_recorded(fresh.partition(), 1, "durable");
  assert!(
    rolled_back && segment_agrees,
    "the unpublished effect and its completion are gone together (live: {rolled_back}, recovered: \
     {segment_agrees}); the durable transaction before them is untouched"
  );
  assert_eq!(
    (db.next_seq(), db.rollbacks(), db.in_transaction()),
    (seq_before, 1, false),
    "nothing was appended (the sequence did not move), the rollback is counted, the transaction is \
     closed"
  );

  // The retry: the same transaction without the fault is durable, in memory and in the segment.
  recorded_create(&mut db, &mut seg, 2, "unpublished");
  db.commit(&mut seg).unwrap();
  let (fresh, _) = recovered(&mut seg);
  assert!(
    holds_recorded(db.partition(), 2, "unpublished")
      && holds_recorded(fresh.partition(), 2, "unpublished"),
    "the retried effect and its completion record are durable"
  );
  assert_eq!(
    (db.next_seq(), db.rollbacks()),
    (seq_before + 1, 1),
    "the retry took the sequence the first attempt would have; no further rollback"
  );
}

/// AC-2.3 (AUD-06): a maintenance snapshot that fails **after** the record was appended is a
/// different failure — the record is durable, so the commit stands and the snapshot is deferred, not
/// refused: the live partition keeps the effect and the completion, a fresh recovery replays the
/// record from the log, the failure is counted, and the next commit publishes the deferred snapshot.
#[test]
fn a_maintenance_snapshot_that_fails_after_the_append_is_deferred_and_the_commit_stands() {
  let mut seg = segment("slates-db-maintenance");
  let mut db = open(&mut seg);
  // Snapshot on every commit, so the maintenance step runs after each append.
  db.set_policy(SnapshotPolicy {
    bytes_between_snapshots: 1,
  });
  let snapshots_before = db.snapshots_taken();

  db.inject_publication_fault(Some(PublicationFault::AfterAppend));
  recorded_create(&mut db, &mut seg, 1, "durable");
  let seq = db
    .commit(&mut seg)
    .expect("the append is durable, so the commit stands")
    .expect("a record was written");
  let (fresh, stats) = recovered(&mut seg);
  assert!(
    holds_recorded(db.partition(), 1, "durable") && holds_recorded(fresh.partition(), 1, "durable"),
    "the effect and its completion stand, live and in the segment: a retry is served from the record"
  );
  assert_eq!(
    (
      db.maintenance_failures(),
      db.rollbacks(),
      db.snapshots_taken()
    ),
    (1, 0, snapshots_before),
    "the deferred snapshot is counted; nothing was rolled back; no snapshot was published"
  );
  assert_eq!(
    (stats.replayed_records, stats.next_seq > seq),
    (1, true),
    "recovery replayed the record from the log (no snapshot carries it) past the durable sequence"
  );

  // The next commit publishes the deferred snapshot.
  db.begin();
  db.mutate(
    &mut seg,
    &Op::VolumeCreated {
      record: volume(2, "next"),
    },
    NOW_NS,
  )
  .unwrap();
  db.commit(&mut seg).unwrap();
  let (fresh, stats) = recovered(&mut seg);
  assert_eq!(
    (
      db.snapshots_taken(),
      db.maintenance_failures(),
      stats.replayed_records
    ),
    (snapshots_before + 1, 1, 0),
    "the deferred snapshot was taken by the next commit, no further failure, and recovery replays \
     nothing: the snapshot carries everything and the log was trimmed"
  );
  assert!(
    holds_recorded(fresh.partition(), 1, "durable") && fresh.partition().volume(vid(2)).is_some(),
    "both records are in the snapshot"
  );
}
