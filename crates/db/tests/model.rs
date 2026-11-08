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
  clippy::disallowed_types,
  clippy::indexing_slicing
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
use slates_db::replay::{SnapshotPolicy, Tail, recover, snapshot_due};
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

/// AC-4.11 / T-4.13, §4.8: detach a source mount with two bindings, recover the database,
/// and expect all three gone while another mount remains. Replayed removal is the same atom.
#[test]
fn removing_a_source_mount_removes_its_bindings_and_recovery_agrees() {
  let mut segment = segment("slates-db-mount-dependency", LOG_BYTES);
  let mut db = open(&mut segment);
  let volume = volume(1, "mounted");
  db.mutate(
    &mut segment,
    &Op::VolumeCreated {
      record: volume.clone(),
    },
    0,
  )
  .unwrap();
  let parent = AttachmentRecord {
    id: 1,
    volume: volume.id,
    consumer: Consumer::Bridge,
    snapshot: None,
    form: AttachForm::Root,
    principal: principal(0),
    rights: Rights {
      read: true,
      write: true,
      admin: false,
    },
    token: [1; 16],
  };
  for record in [
    parent.clone(),
    AttachmentRecord {
      id: 2,
      ..parent.clone()
    },
    AttachmentRecord {
      id: 3,
      consumer: Consumer::Mount { attachment: 1 },
      form: AttachForm::Oci {
        source: "/mount".into(),
        destination: "/work".into(),
        read_only: false,
      },
      ..parent.clone()
    },
    AttachmentRecord {
      id: 4,
      consumer: Consumer::Mount { attachment: 1 },
      form: AttachForm::Oci {
        source: "/mount".into(),
        destination: "/read".into(),
        read_only: true,
      },
      rights: Rights {
        read: true,
        write: false,
        admin: false,
      },
      ..parent
    },
  ] {
    db.mutate(&mut segment, &Op::AttachmentAdded { record }, 0)
      .unwrap();
  }
  db.snapshot(&mut segment).unwrap();
  let restored = recover(&mut segment, 0, caps(), 0).unwrap().0;
  assert_eq!(
    restored.partition().attachments_of(volume.id),
    db.partition().attachments_of(volume.id),
    "recovery keeps the live mount and both bindings"
  );
  let child = db.partition().attachment(4).unwrap().clone();
  for invalid in [
    AttachmentRecord {
      id: 5,
      consumer: Consumer::Mount { attachment: 0 },
      ..child.clone()
    },
    AttachmentRecord {
      id: 5,
      consumer: Consumer::Mount { attachment: 3 },
      ..child.clone()
    },
    AttachmentRecord {
      id: 5,
      principal: principal(1),
      ..child.clone()
    },
    AttachmentRecord {
      id: 5,
      form: AttachForm::Root,
      ..child.clone()
    },
    AttachmentRecord {
      id: 5,
      rights: Rights {
        read: true,
        write: true,
        admin: true,
      },
      ..child
    },
  ] {
    assert_eq!(
      db.mutate(&mut segment, &Op::AttachmentAdded { record: invalid }, 0),
      Err(DbError::NotFound)
    );
    assert_eq!(
      db.partition().attachments_of(volume.id).len(),
      4,
      "a refused dependency has no effect"
    );
  }
  db.mutate(&mut segment, &Op::AttachmentRemoved { id: 1 }, 0)
    .unwrap();
  let recovered = recover(&mut segment, 0, caps(), 0).unwrap().0;
  for partition in [db.partition(), recovered.partition()] {
    let remaining: Vec<_> = partition
      .attachments_of(volume.id)
      .iter()
      .map(|record| record.id)
      .collect();
    assert_eq!(remaining, vec![2], "only the unrelated mount remains");
  }
}

/// §4.6 A-115: do record a host mount bound to uid 1000 and scoped to directory 7, then bind its mount point; expect the
/// form to keep the identity and the scope beside the mount point, live and after recovery. Before A-115 a bind
/// rewrote every unscoped host mount's form as a chosen path, which for a bound mount would drop the identity and
/// serve every caller.
#[test]
fn binding_a_bound_mount_keeps_its_identity_and_recovery_agrees() {
  let mut segment = segment("slates-db-bound-mount", LOG_BYTES);
  let mut db = open(&mut segment);
  let volume = volume(1, "bound");
  db.mutate(
    &mut segment,
    &Op::VolumeCreated {
      record: volume.clone(),
    },
    0,
  )
  .unwrap();
  let record = AttachmentRecord {
    id: 1,
    volume: volume.id,
    consumer: Consumer::Bridge,
    snapshot: None,
    form: AttachForm::BoundMount {
      uid: 1000,
      scope: Some(7),
      mount_point: None,
    },
    principal: principal(0),
    rights: Rights {
      read: true,
      write: true,
      admin: false,
    },
    token: [1; 16],
  };
  db.mutate(&mut segment, &Op::AttachmentAdded { record }, 0)
    .unwrap();
  db.mutate(
    &mut segment,
    &Op::AttachmentBound {
      id: 1,
      path: "/mnt/agent".into(),
    },
    0,
  )
  .unwrap();
  let expected = AttachForm::BoundMount {
    uid: 1000,
    scope: Some(7),
    mount_point: Some("/mnt/agent".into()),
  };
  let recovered = recover(&mut segment, 0, caps(), 0).unwrap().0;
  for partition in [db.partition(), recovered.partition()] {
    let form = &partition.attachment(1).unwrap().form;
    assert_eq!(
      form, &expected,
      "the identity and the scope survive the bind"
    );
    assert_eq!(form.bound_uid(), Some(1000));
    assert_eq!(form.mount_point(), Some("/mnt/agent"));
  }
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
    owner: principal(0),
    access: vec![],
    created_ns: 0,
    catalog_version: 0,
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
  LiveShip(usize, bool),
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
  Acknowledge(u32, u32, Vec<u32>),
  Grant(usize),
  GrantState(usize, u8),
  LandingLease(u32, u64),
  Landing(usize),
  LandingState(usize, u8),
  Audit(u8),
  GreenAdvance(usize),
  GreenOriginate(usize),
  /// A green's checkpoint (W3) at a version, which folds the increments up to it.
  GreenCheckpoint(usize, u64),
  NfsOpen(u8, u8, u32),
  NfsOpenClear(usize),
  NfsLock(usize, u64, u64, bool),
  NfsLockClear(usize),
  NfsClientPurge(u8),
  NfsClient(u8, u32),
  NfsClientClear(u8),
  NfsInstance,
  /// A delegation of a file to a client (A-78), read or write.
  NfsDelegation(u8, u8, bool),
  /// A delegation returned or revoked.
  NfsDelegationClear(usize),
  /// An NFSv4.1 session recorded (A-127): its client and which of the client's sessions.
  NfsSession(u8, u8),
  /// An NFSv4.1 session destroyed.
  NfsSessionClear(u8, u8),
  /// A call record of a session's slot recorded with a file-state change (A-127): the client, the session, the slot.
  NfsCall(u8, u8, u8),
  /// A destroyed session's call records dropped.
  NfsCallsClear(u8, u8),
  /// An NFSv4 purge owed with a drop (2026-10-10): the client, and which of its sessions (or its whole state).
  NfsPurgeOwe(u8, Option<u8>),
  /// An owed purge settled by every owner.
  NfsPurgeSettle(u8, Option<u8>),
  /// A sealing key recorded (A-92): its owner's kind (tenant, naming or lineage) and who.
  SealKey(u8, u8),
  /// A sealing key destroyed (crypto-erase).
  SealKeyDestroy(u8, u8),
  Crash,
}

fn step() -> impl Strategy<Value = Step> {
  prop_oneof![
    3 => Just(Step::Create),
    2 => (0..8usize, 0..8u8).prop_map(|(v, s)| Step::State(v, s)),
    2 => (0..8usize, 0..1_000_000u64, 0..1_000_000u64).prop_map(|(v, r, u)| Step::Account(v, r, u)),
    1 => (0..8usize).prop_map(Step::Access),
    1 => (0..8usize, proptest::bool::ANY).prop_map(|(v, on)| Step::LiveShip(v, on)),
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
    1 => (0..3u32, 0..16u32, proptest::collection::vec(0..16u32, 0..4)).prop_map(|(c, s, owed)| Step::Acknowledge(c, s, owed)),
    1 => (0..8usize).prop_map(Step::Grant),
    1 => (0..8usize, 0..4u8).prop_map(|(g, s)| Step::GrantState(g, s)),
    1 => (0..2u32, 1..100u64).prop_map(|(h, t)| Step::LandingLease(h, t)),
    1 => (0..8usize).prop_map(Step::Landing),
    1 => (0..8usize, 0..10u8).prop_map(|(l, s)| Step::LandingState(l, s)),
    1 => (0..7u8).prop_map(Step::Audit),
    2 => (0..8usize).prop_map(Step::GreenAdvance),
    1 => (0..8usize).prop_map(Step::GreenOriginate),
    1 => (0..8usize, 0..6u64).prop_map(|(v, at)| Step::GreenCheckpoint(v, at)),
    2 => (0..4u8, 0..8u8, 1..4u32).prop_map(|(c, f, a)| Step::NfsOpen(c, f, a)),
    1 => (0..8usize).prop_map(Step::NfsOpenClear),
    2 => (0..8usize, 0..1_000u64, 1..1_000u64, any::<bool>()).prop_map(|(o, a, l, w)| Step::NfsLock(o, a, l, w)),
    1 => (0..8usize).prop_map(Step::NfsLockClear),
    1 => (0..4u8).prop_map(Step::NfsClientPurge),
    1 => (0..4u8, 1..8u32).prop_map(|(c, q)| Step::NfsClient(c, q)),
    1 => (0..4u8).prop_map(Step::NfsClientClear),
    1 => Just(Step::NfsInstance),
    1 => (0..4u8, 0..8u8, any::<bool>()).prop_map(|(c, f, w)| Step::NfsDelegation(c, f, w)),
    1 => (0..8usize).prop_map(Step::NfsDelegationClear),
    1 => (0..4u8, 0..2u8).prop_map(|(c, n)| Step::NfsSession(c, n)),
    1 => (0..4u8, 0..2u8).prop_map(|(c, n)| Step::NfsSessionClear(c, n)),
    2 => (0..4u8, 0..2u8, 0..4u8).prop_map(|(c, n, l)| Step::NfsCall(c, n, l)),
    1 => (0..4u8, 0..2u8).prop_map(|(c, n)| Step::NfsCallsClear(c, n)),
    1 => (0..4u8, proptest::option::of(0..2u8)).prop_map(|(c, n)| Step::NfsPurgeOwe(c, n)),
    1 => (0..4u8, proptest::option::of(0..2u8)).prop_map(|(c, n)| Step::NfsPurgeSettle(c, n)),
    1 => (0..3u8, 0..4u8).prop_map(|(k, w)| Step::SealKey(k, w)),
    1 => (0..3u8, 0..4u8).prop_map(|(k, w)| Step::SealKeyDestroy(k, w)),
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
  next_nfs_state: u64,
  nfs_opens: Vec<[u8; 12]>,
  nfs_delegations: Vec<[u8; 12]>,
  nfs_locks: Vec<[u8; 12]>,
  nfs_instance: u32,
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
    // A-200: live shipping is part of the policy the recovered partition must equal.
    Step::LiveShip(v, on) => Op::LiveShippingChanged {
      id: vid(pick(&ids.live, *v)?),
      live_shipped: *on,
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
          rights: slates_db::catalog::Rights {
            read: true,
            write: true,
            admin: false,
          },
          token: [0u8; 16],
        },
      }
    }
    Step::Detach(a) => Op::AttachmentRemoved {
      id: pick(&ids.attachments, *a)?,
    },
    Step::Complete(c, s) => Op::CompletionRecorded {
      record: CompletionRecord {
        origin: 0,
        client: *c,
        sequence: *s,
        result: vec![u8::try_from(*s).unwrap_or(0); 3],
      },
    },
    Step::Acknowledge(c, s, owed) => Op::CompletionsAcknowledged {
      origin: 0,
      client: *c,
      up_to: *s,
      owed: owed.clone(),
    },
    step @ (Step::Grant(..)
    | Step::GrantState(..)
    | Step::LandingLease(..)
    | Step::Landing(..)
    | Step::LandingState(..)
    | Step::Audit(..)
    | Step::GreenAdvance(..)
    | Step::GreenOriginate(..)
    | Step::GreenCheckpoint(..)) => return op_for_service(step, ids, now_ns),
    step @ (Step::NfsOpen(..)
    | Step::NfsOpenClear(..)
    | Step::NfsLock(..)
    | Step::NfsLockClear(..)
    | Step::NfsClientPurge(..)
    | Step::NfsClient(..)
    | Step::NfsClientClear(..)
    | Step::NfsInstance
    | Step::NfsDelegation(..)
    | Step::NfsDelegationClear(..)
    | Step::SealKey(..)
    | Step::SealKeyDestroy(..)) => return op_for_nfs(step, ids),
    step @ (Step::NfsSession(..)
    | Step::NfsSessionClear(..)
    | Step::NfsCall(..)
    | Step::NfsCallsClear(..)
    | Step::NfsPurgeOwe(..)
    | Step::NfsPurgeSettle(..)) => return op_for_nfs_session(step),
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
          target_device: 0,
          target_inode: 0,
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
    // An origin is recorded once and only before any increment (the partition's guard refuses a
    // second or a late one; a refused step is never recorded, which is what the model checks).
    Step::GreenOriginate(v) => Op::GreenOriginated {
      green: vid(pick(&ids.live, *v)?),
      origin: (*v as u64).to_be_bytes().to_vec(),
    },
    // A checkpoint at a version the green's head may or may not reach: the partition refuses one at or below the
    // checkpoint held or past the head, and the model checks a refused one was never recorded.
    Step::GreenCheckpoint(v, at) => Op::GreenCheckpointed {
      green: vid(pick(&ids.live, *v)?),
      version: *at,
      checkpoint: at.to_be_bytes().to_vec(),
    },
    _ => return None,
  })
}

/// The NFSv4 record operations (§4.6 A-37), split from [`op_for`]: opens and lock states named by a
/// counter, clears and purges of what exists, client records, and instance advances.
fn op_for_nfs(step: &Step, ids: &mut Ids) -> Option<Op> {
  use slates_db::catalog::{
    NfsClientRecord, NfsDelegationRecord, NfsLockRange, NfsLockRecord, NfsOpenRecord,
  };
  let mint = |ids: &mut Ids| {
    ids.next_nfs_state += 1;
    let mut other = [0u8; 12];
    other[4..].copy_from_slice(&ids.next_nfs_state.to_be_bytes());
    other
  };
  Some(match step {
    Step::NfsOpen(client, file, access) => Op::NfsOpenSet {
      record: NfsOpenRecord {
        other: mint(ids),
        clientid: u64::from(*client),
        owner: vec![*client, *file],
        // Distinct uids across the generated history, so the field makes the recovery round trip under
        // test rather than riding along as one constant.
        uid: u32::from(*client),
        fh: vec![*file; 16],
        access: *access,
        deny: 0,
        seqid: 1,
      },
    },
    Step::NfsOpenClear(o) => Op::NfsOpenCleared {
      other: pick(&ids.nfs_opens, *o)?,
    },
    Step::NfsLock(o, start, length, write) => {
      let open = pick(&ids.nfs_opens, *o)?;
      Op::NfsLockSet {
        record: NfsLockRecord {
          other: mint(ids),
          clientid: u64::from(open[11] % 4),
          owner: open.to_vec(),
          fh: vec![open[11]; 16],
          open,
          seqid: 1,
          ranges: vec![NfsLockRange {
            start: *start,
            end: start + length,
            write: *write,
          }],
        },
      }
    }
    Step::NfsLockClear(l) => Op::NfsLockCleared {
      other: pick(&ids.nfs_locks, *l)?,
    },
    Step::NfsClientPurge(client) => Op::NfsClientStateCleared {
      clientid: u64::from(*client),
    },
    Step::NfsClient(client, create_seq) => Op::NfsClientSet {
      record: NfsClientRecord {
        clientid: u64::from(*client),
        owner: vec![*client],
        verifier: [*client; 8],
        principal: u32::from(*client),
        create_seq: *create_seq,
      },
    },
    Step::NfsClientClear(client) => Op::NfsClientCleared {
      clientid: u64::from(*client),
    },
    Step::NfsInstance => Op::NfsInstanceAdvanced {
      instance: ids.nfs_instance + 1,
    },
    Step::NfsDelegation(client, file, write) => Op::NfsDelegationSet {
      record: NfsDelegationRecord {
        other: mint(ids),
        clientid: u64::from(*client),
        fh: vec![*file; 16],
        write: *write,
        seqid: 1,
      },
    },
    Step::NfsDelegationClear(d) => Op::NfsDelegationCleared {
      other: pick(&ids.nfs_delegations, *d)?,
    },
    Step::SealKey(kind, who) => Op::SealKeySet {
      record: slates_db::catalog::SealKeyRecord {
        owner: seal_owner(*kind, *who),
        id: [*who; 16],
        record: vec![*kind; slates_db::catalog::SEAL_KEY_RECORD_BYTES],
      },
    },
    Step::SealKeyDestroy(kind, who) => Op::SealKeyDestroyed {
      owner: seal_owner(*kind, *who),
    },
    _ => return None,
  })
}

/// The session id the model's `session` of `client` is (A-127): the client in its first bytes, as a minted one is.
fn model_sessionid(client: u8, session: u8) -> [u8; 16] {
  let mut id = [0u8; 16];
  id[7] = client;
  id[15] = session;
  id
}

/// The NFSv4.1 session and call-record operations (A-127), split from [`op_for`]: sessions and slot records named by
/// small numbers, so sets, clears, a client's purge and a client's drop meet each other.
fn op_for_nfs_session(step: &Step) -> Option<Op> {
  use slates_db::catalog::{NfsCallRecord, NfsCallbackRecord, NfsChannelRecord, NfsSessionRecord};
  let channel = NfsChannelRecord {
    header_pad: 0,
    max_request: 1 << 20,
    max_response: 1 << 20,
    max_response_cached: 4096,
    max_operations: 16,
    max_requests: 4,
  };
  Some(match step {
    Step::NfsSession(client, session) => Op::NfsSessionSet {
      record: NfsSessionRecord {
        sessionid: model_sessionid(*client, *session),
        clientid: u64::from(*client),
        sequence: u32::from(*session) + 1,
        fore: channel,
        back: channel,
        callback: (*session == 0).then(|| NfsCallbackRecord {
          program: 0x4000_0000,
          credential: vec![0; 8],
          minor: 2,
        }),
      },
    },
    Step::NfsSessionClear(client, session) => Op::NfsSessionCleared {
      sessionid: model_sessionid(*client, *session),
    },
    Step::NfsCall(client, session, slot) => {
      let mut key = [0u8; 20];
      key[..16].copy_from_slice(&model_sessionid(*client, *session));
      key[16..].copy_from_slice(&u32::from(*slot).to_be_bytes());
      Op::NfsCallSet {
        record: NfsCallRecord {
          key,
          clientid: u64::from(*client),
          record: vec![*client, *session, *slot],
        },
      }
    }
    Step::NfsCallsClear(client, session) => Op::NfsCallsCleared {
      sessionid: model_sessionid(*client, *session),
    },
    Step::NfsPurgeOwe(client, session) => Op::NfsPurgeOwed {
      record: model_purge(*client, *session),
    },
    Step::NfsPurgeSettle(client, session) => Op::NfsPurgeSettled {
      record: model_purge(*client, *session),
    },
    _ => return None,
  })
}

/// The purge record of client `client`'s session `session`, or of its whole state.
fn model_purge(client: u8, session: Option<u8>) -> slates_db::catalog::NfsPurgeRecord {
  slates_db::catalog::NfsPurgeRecord {
    clientid: u64::from(client),
    session: session.map(|session| model_sessionid(client, session)),
  }
}

/// A sealing key's owner: a tenant, its naming key, or a volume's lineage, by `kind`.
fn seal_owner(kind: u8, who: u8) -> slates_db::catalog::SealKeyOwner {
  match kind {
    0 => slates_db::catalog::SealKeyOwner::Tenant {
      account: u64::from(who),
    },
    1 => slates_db::catalog::SealKeyOwner::Naming {
      volume: VolumeId { bytes: [who; 16] },
    },
    _ => slates_db::catalog::SealKeyOwner::Lineage {
      volume: VolumeId { bytes: [who; 16] },
    },
  }
}

/// Keeps the interpreter's view of live ids in step with what the partition accepted.
fn note_applied(op: &Op, ids: &mut Ids) {
  match op {
    Op::NfsOpenSet { record } => ids.nfs_opens.push(record.other),
    Op::NfsOpenCleared { other } => ids.nfs_opens.retain(|o| o != other),
    Op::NfsDelegationSet { record } => ids.nfs_delegations.push(record.other),
    Op::NfsDelegationCleared { other } => ids.nfs_delegations.retain(|o| o != other),
    Op::NfsLockSet { record } => ids.nfs_locks.push(record.other),
    Op::NfsLockCleared { other } => ids.nfs_locks.retain(|o| o != other),
    Op::NfsInstanceAdvanced { instance } => ids.nfs_instance = *instance,
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

/// When a generated history's database snapshots: a pinned byte cadence, or the measured trigger (A-243) with a
/// recovery budget, whose snapshot points follow the clock and so differ between two runs of one history.
#[derive(Clone, Copy, Debug)]
enum Cadence {
  Pinned(u64),
  Measured(u64),
}

/// Sets `cadence` on a database just opened or recovered.
fn hold_to(db: &mut Db, cadence: Cadence) {
  match cadence {
    Cadence::Pinned(bytes) => db.set_policy(SnapshotPolicy {
      bytes_between_snapshots: bytes,
    }),
    Cadence::Measured(budget_ns) => db.set_recovery_budget(budget_ns),
  }
}

/// A refusal where the history expects none, as the generated case's failure.
fn failed(error: impl std::fmt::Debug) -> TestCaseError {
  TestCaseError::fail(format!("{error:?}"))
}

/// Runs a generated history under `cadence`, crashing where it says: after every crash, and at the end, the recovered
/// partition equals the live one whole; refused operations were never recorded.
fn every_recovery_equals_the_live_partition_under(
  steps: &[Step],
  cadence: Cadence,
) -> Result<(), TestCaseError> {
  let mut seg = segment("slates-db-model", LOG_BYTES);
  let mut db = open(&mut seg);
  hold_to(&mut db, cadence);
  let mut ids = Ids::default();
  let mut now_ns = 0u64;
  let mut applied = 0usize;
  for s in steps {
    now_ns += 100;
    if matches!(s, Step::Crash) {
      let live = db.partition().to_snapshot(0);
      let seq = db.next_seq();
      drop(db);
      let (recovered, stats) = recover(&mut seg, 0, caps(), now_ns).map_err(failed)?;
      prop_assert_eq!(
        recovered.partition().to_snapshot(0),
        live,
        "recovery differs from the live partition"
      );
      prop_assert_eq!(recovered.next_seq(), seq);
      prop_assert!(!stats.torn, "a clean crash has no torn tail");
      db = recovered;
      hold_to(&mut db, cadence);
      continue;
    }
    let Some(op) = op_for(s, &mut ids, now_ns) else {
      continue;
    };
    match db.mutate(&mut seg, &op, now_ns) {
      Ok(_) => {
        note_applied(&op, &mut ids);
        applied += 1;
      }
      Err(
        DbError::NotFound
        | DbError::AlreadyExists { .. }
        | DbError::LeaseHeld { .. }
        | DbError::StaleLease { .. }
        | DbError::StaleCompletion { .. }
        | DbError::Capacity { .. }
        | DbError::CheckpointOutOfRange { .. },
      ) => {}
      Err(other) => prop_assert!(false, "unexpected refusal {other:?}"),
    }
    for expired in db.partition_mut().expired_leases(now_ns) {
      db.mutate(&mut seg, &Op::LeaseReleased { volume: expired }, now_ns)
        .map_err(failed)?;
    }
  }
  let live = db.partition().to_snapshot(0);
  let seq = db.next_seq();
  drop(db);
  let (recovered, _) = recover(&mut seg, 0, caps(), now_ns).map_err(failed)?;
  prop_assert_eq!(recovered.partition().to_snapshot(0), live);
  prop_assert_eq!(recovered.next_seq(), seq);
  prop_assert!(usize::try_from(seq).map_err(failed)? >= applied);
  Ok(())
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig { cases: slates_test_seeds::cases(60), ..ProptestConfig::default() }))]

  /// AC-2.3's durability half and AC-2.4's fencing under generated histories: after every
  /// crash the recovered partition equals the live one; refused operations were never
  /// recorded; the snapshot cadence and the log trim never lose a record.
  #[test]
  fn every_recovery_equals_the_live_partition(steps in proptest::collection::vec(step(), 1..250), snapshot_every in 1u64..20_000) {
    every_recovery_equals_the_live_partition_under(&steps, Cadence::Pinned(snapshot_every))?;
  }

  /// AC-2.3, A-243 (raft): the same under the measured trigger, a recovery budget of 0 to 200 µs, so snapshots fall
  /// where the clock puts them (with a budget of 0, at every snapshot's size of log). Expect: every recovery equals the
  /// live partition whole, wherever the snapshots fell.
  #[test]
  fn every_recovery_equals_the_live_partition_wherever_the_measured_trigger_snapshots(steps in proptest::collection::vec(step(), 1..250), budget_ns in 0u64..200_000) {
    every_recovery_equals_the_live_partition_under(&steps, Cadence::Measured(budget_ns))?;
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
  let at = slates_anchor::layout::RING_BYTES + usize::try_from(tail_before).unwrap() + 40;
  let mut byte = [0u8; 1];
  seg.region_read(RegionKind::Log(0), at, &mut byte).unwrap();
  byte[0] ^= 0xff;
  seg.region_write(RegionKind::Log(0), at, &byte).unwrap();
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
      seg
        .region_write(RegionKind::Log(0), at, &header[..32.min(len)])
        .unwrap();
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
        origin: 0,
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
        origin: 0,
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
    db.partition().completion(0, 7, 3),
    Seen::Completed(vec![1, 2, 3])
  );
  assert_eq!(db.partition().completion(0, 7, 5), Seen::New);
  db.mutate(
    &mut seg,
    &Op::CompletionsAcknowledged {
      origin: 0,
      client: 7,
      up_to: 3,
      owed: Vec::new(),
    },
    0,
  )
  .unwrap();
  assert_eq!(db.partition().completion(0, 7, 3), Seen::Acknowledged);
  assert_eq!(db.partition().completion(0, 7, 4), Seen::Completed(vec![4]));
}

/// The idempotency key is globally unique: two **distinct origin hosts** with the *same* (client, sequence)
/// keep independent completions, so a cross-node forwarded write (keyed by its authenticated origin) never
/// returns — or is deduplicated against — a local client that happens to share its per-node id (§4.8, task
/// #29). Non-vacuous: the two records carry different results, and each origin reads back its own; a shared
/// (client, sequence) key would have overwritten one with the other.
#[test]
fn completions_from_distinct_origins_do_not_collide() {
  let mut seg = segment("slates-db-completion-origins", LOG_BYTES);
  let mut db = open(&mut seg);
  for (origin, result) in [(1u64, vec![10u8]), (2u64, vec![20u8])] {
    db.mutate(
      &mut seg,
      &Op::CompletionRecorded {
        record: CompletionRecord {
          origin,
          client: 7,
          sequence: 3,
          result,
        },
      },
      0,
    )
    .unwrap();
  }
  assert_eq!(
    db.partition().completion(1, 7, 3),
    Seen::Completed(vec![10])
  );
  assert_eq!(
    db.partition().completion(2, 7, 3),
    Seen::Completed(vec![20])
  );
  // A third origin has seen nothing — the key distinguishes the origin, not just the client.
  assert_eq!(db.partition().completion(3, 7, 3), Seen::New);
  // An acknowledgement from one origin does not release the other's completion.
  db.mutate(
    &mut seg,
    &Op::CompletionsAcknowledged {
      origin: 1,
      client: 7,
      up_to: 3,
      owed: Vec::new(),
    },
    0,
  )
  .unwrap();
  assert_eq!(db.partition().completion(1, 7, 3), Seen::Acknowledged);
  assert_eq!(
    db.partition().completion(2, 7, 3),
    Seen::Completed(vec![20])
  );
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

/// Shape: the bytes of each increment the checkpoint tests advance a green by.
const INCREMENT_BYTES: usize = 100;
/// Shape: the bytes of each checkpoint they take: less than what it folds, so the fold returns budget.
const CHECKPOINT_BYTES: usize = 40;
/// Shape: the increments the checkpoint tests' merge budget holds.
const BUDGET_INCREMENTS: usize = 5;

/// The checkpoint tests' green.
const CHECKPOINT_GREEN: u64 = 1;

/// The checkpoint tests' caps: a merge budget of [`BUDGET_INCREMENTS`] increments.
fn checkpoint_caps() -> PartitionCaps {
  PartitionCaps {
    green_chain_bytes: BUDGET_INCREMENTS * INCREMENT_BYTES,
    ..caps()
  }
}

/// The green's increment `n`.
fn advance(n: u8) -> Op {
  Op::GreenAdvanced {
    green: vid(CHECKPOINT_GREEN),
    increment: vec![n; INCREMENT_BYTES],
  }
}

/// The green's checkpoint at `version`.
fn checkpoint_at(version: u64) -> Op {
  Op::GreenCheckpointed {
    green: vid(CHECKPOINT_GREEN),
    version,
    checkpoint: vec![0xC0; CHECKPOINT_BYTES],
  }
}

/// W3, §4.16. Do: under a merge budget of five 100-byte increments, advance a green five versions and try a sixth;
/// checkpoint it at version 3 with 40 bytes, then advance it until refused. Expect: the sixth is refused at the cap;
/// after the checkpoint the chain holds only versions 4 and 5 and the head stays 5; exactly two more increments fit
/// (the budget fell by the 300 bytes folded less the checkpoint's 40). Returns the database at head 7 and its segment.
fn a_checkpoint_returns_what_it_folds() -> (AnchorSegment, Db) {
  let green = vid(CHECKPOINT_GREEN);
  let mut seg = segment("slates-db-checkpoint", LOG_BYTES);
  let mut db = recover(&mut seg, 0, checkpoint_caps(), 0).unwrap().0;
  let record = volume(CHECKPOINT_GREEN, "green");
  db.mutate(&mut seg, &Op::VolumeCreated { record }, 0)
    .unwrap();
  for n in 0..5u8 {
    db.mutate(&mut seg, &advance(n), 0).unwrap();
  }
  let sixth = db.mutate(&mut seg, &advance(5), 0);
  assert!(matches!(sixth, Err(DbError::Capacity { .. })), "{sixth:?}");
  db.mutate(&mut seg, &checkpoint_at(3), 0).unwrap();
  let partition = db.partition();
  assert_eq!(
    partition.green_chain(green),
    &[vec![3; INCREMENT_BYTES], vec![4; INCREMENT_BYTES]]
  );
  assert_eq!(
    partition.green_checkpoint(green),
    Some((3, &[0xC0; CHECKPOINT_BYTES][..]))
  );
  assert_eq!(partition.green_head(green), 5);
  db.mutate(&mut seg, &advance(5), 0).unwrap();
  db.mutate(&mut seg, &advance(6), 0).unwrap();
  let eighth = db.mutate(&mut seg, &advance(7), 0);
  assert!(
    matches!(eighth, Err(DbError::Capacity { .. })),
    "{eighth:?}"
  );
  (seg, db)
}

/// Asks for checkpoints at the one held (3), below it, and past the head (7): each refused `CheckpointOutOfRange`,
/// none recorded.
fn checkpoints_out_of_range_are_refused(seg: &mut AnchorSegment, db: &mut Db) {
  let seq = db.next_seq();
  for version in [3, 2, 8] {
    let refused = db.mutate(seg, &checkpoint_at(version), 0);
    let expected = DbError::CheckpointOutOfRange {
      version,
      held: 3,
      head: 7,
    };
    assert_eq!(
      refused.err(),
      Some(expected),
      "a checkpoint at {version} is refused"
    );
  }
  assert_eq!(db.next_seq(), seq, "no refused checkpoint was recorded");
}

/// W3, §4.16. Do: the fold of [`a_checkpoint_returns_what_it_folds`]; then ask for checkpoints at 3, at 2 and past the
/// head 7; recover; checkpoint at the head; snapshot and recover again. Expect: the three are refused
/// `CheckpointOutOfRange`, never recorded; recovery from the log and from a snapshot holds the same checkpoint and
/// chain; the checkpoint at the head leaves an empty chain, and a late origin is still refused.
#[test]
fn a_green_checkpoint_folds_its_chain_and_recovery_holds_it() {
  let green = vid(CHECKPOINT_GREEN);
  let (mut seg, mut db) = a_checkpoint_returns_what_it_folds();
  checkpoints_out_of_range_are_refused(&mut seg, &mut db);
  let live = db.partition().to_snapshot(0);
  drop(db);
  let (mut db, _) = recover(&mut seg, 0, checkpoint_caps(), 0).unwrap();
  assert_eq!(
    db.partition().to_snapshot(0),
    live,
    "recovery holds the checkpoint and the chain after it"
  );
  db.mutate(&mut seg, &checkpoint_at(7), 0).unwrap();
  assert!(db.partition().green_chain(green).is_empty());
  assert_eq!(db.partition().green_head(green), 7);
  let late = db.mutate(
    &mut seg,
    &Op::GreenOriginated {
      green,
      origin: vec![1],
    },
    0,
  );
  assert!(
    matches!(late, Err(DbError::AlreadyExists { .. })),
    "{late:?}"
  );
  db.snapshot(&mut seg).unwrap();
  let live = db.partition().to_snapshot(0);
  drop(db);
  let (recovered, stats) = recover(&mut seg, 0, checkpoint_caps(), 0).unwrap();
  assert!(stats.snapshot.is_some());
  assert_eq!(
    recovered.partition().to_snapshot(0),
    live,
    "a snapshot carries the checkpoint"
  );
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

/// A-243: the measured trigger needs both the budget's replay work and a snapshot's size of log. Work alone would
/// re-encode a large partition for every budget's worth of small records, past the doubling bound; bytes alone is the
/// extrapolation that let a tail of small records run past the budget.
#[test]
fn a_snapshot_is_due_only_once_the_tail_took_the_budget_and_reached_the_floor() {
  let tail = |bytes, replay_ns| Tail { bytes, replay_ns };
  assert!(snapshot_due(tail(4_096, 1_000), 4_096, 1_000));
  assert!(
    !snapshot_due(tail(4_095, 1_000_000), 4_096, 1_000),
    "work without the floor's bytes"
  );
  assert!(
    !snapshot_due(tail(1 << 30, 999), 4_096, 1_000),
    "bytes without the budget's work"
  );
  assert!(
    snapshot_due(tail(0, 0), 0, 0),
    "a zero budget and floor snapshot at every commit"
  );
}

/// Shape: the snapshot interval of the rewind test — small, so the history crosses hundreds of snapshots.
const REWIND_INTERVAL: u64 = 16 << 10;
/// Shape: the log ring of the rewind test: far past what one interval needs, as a daemon's ring is (2.86 GB per
/// partition against a 1 MB interval on a fresh daemon).
const REWIND_LOG_BYTES: u64 = 4 << 20;
/// Shape: how many interval-sized spans of the ring a writer may touch (the interval, the record that crosses it,
/// and slack for the record header).
const REWIND_SPANS: u64 = 4;

/// §4.8 (A-71): do renew one lease until the log has carried several times its ring's capacity, snapshotting every
/// [`REWIND_INTERVAL`] bytes; expect the ring never to write past the first few intervals of its region — a
/// snapshot's trim empties the ring, and the next record starts again at its first byte, so the pages a long run
/// keeps backed are one interval's, not the whole ring's (before: a daemon's segment grew 1.8 MB per Docker round
/// and kept every page until the 2.86 GB ring wrapped). Then recover; expect the same partition. Then put the ring
/// in the state a crash between the rewind's two stores leaves (the head at the first byte, the tail not yet);
/// expect recovery to cut the stale record there as torn and come back to the snapshot.
#[test]
fn a_trimmed_ring_rewinds_so_a_long_run_touches_one_interval() {
  let mut seg = segment("slates-db-ring-rewind", REWIND_LOG_BYTES);
  let mut db = open(&mut seg);
  db.set_policy(SnapshotPolicy {
    bytes_between_snapshots: REWIND_INTERVAL,
  });
  db.mutate(
    &mut seg,
    &Op::VolumeCreated {
      record: volume(1, "v1"),
    },
    0,
  )
  .unwrap();
  let mut now = 10u64;
  let mut carried = 0u64;
  while carried < 5 * REWIND_LOG_BYTES {
    let before = seg.ring_words(RegionKind::Log(0)).unwrap()[1].load(Ordering::Acquire);
    db.mutate(&mut seg, &lease(1, 1, now + 1_000), now).unwrap();
    let after = seg.ring_words(RegionKind::Log(0)).unwrap()[1].load(Ordering::Acquire);
    carried += after.saturating_sub(before).max(1);
    now += 1;
  }
  assert!(
    db.snapshots_taken() > 100,
    "{} snapshots",
    db.snapshots_taken()
  );
  let untouched_from =
    slates_anchor::layout::RING_BYTES + usize::try_from(REWIND_SPANS * REWIND_INTERVAL).unwrap();
  let ring_end = usize::try_from(REWIND_LOG_BYTES).unwrap();
  let mut chunk = vec![0u8; 64 << 10];
  let mut at = untouched_from;
  while at < ring_end {
    let len = chunk.len().min(ring_end - at);
    seg
      .region_read(RegionKind::Log(0), at, &mut chunk[..len])
      .unwrap();
    assert!(
      chunk[..len].iter().all(|byte| *byte == 0),
      "the ring wrote at byte {} or after, past {REWIND_SPANS} intervals",
      at
    );
    at += len;
  }
  db.snapshot(&mut seg).unwrap();
  let before = db.partition().to_snapshot(0);
  drop(db);
  let (recovered, _) = recover(&mut seg, 0, caps(), now).unwrap();
  assert_eq!(
    recovered.partition().to_snapshot(0),
    before,
    "a rewound ring recovers"
  );
  drop(recovered);
  let words = seg.ring_words(RegionKind::Log(0)).unwrap();
  let head = words[0].load(Ordering::Acquire);
  words[1].store(head + REWIND_INTERVAL, Ordering::Release);
  let (recovered, stats) = recover(&mut seg, 0, caps(), now).unwrap();
  assert!(
    stats.torn,
    "the stale record at the rewound head is cut as torn"
  );
  assert_eq!(
    recovered.partition().to_snapshot(0),
    before,
    "a crash mid-rewind recovers the snapshot"
  );
}

/// A tenant's key record as the partition holds it, if any.
fn tenant_key(db: &Db, account: u64) -> bool {
  db.partition()
    .seal_key(&slates_db::catalog::SealKeyOwner::Tenant { account })
    .is_some()
}

/// A-92 (seal.md §3.1, cryptographic erase): do give one tenant two volumes and another tenant one, record both
/// tenants' keys, then destroy the first tenant's volumes one at a time, and recover the database. Expect the first
/// tenant's key to stay while it has a volume on the partition and to be gone in the same record as its last volume's
/// destroy, the other tenant's key untouched, and the recovered database to agree. A tenant key wraps only its
/// volumes' lineage keys, so with its last volume it guards nothing, and slates has no account of its own to remove:
/// the key's life is its volumes'.
#[test]
fn a_tenants_key_is_erased_with_its_last_volume_on_the_partition() {
  let mut segment = segment("slates-db-tenant-erase", LOG_BYTES);
  let mut db = open(&mut segment);
  let (first, other) = (principal(0), principal(1));
  let (first_account, other_account) = (1000u64, 1001u64);
  for (n, owner) in [(1, &first), (2, &first), (3, &other)] {
    let record = VolumeRecord {
      owner: owner.clone(),
      ..volume(n, &format!("v{n}"))
    };
    db.mutate(&mut segment, &Op::VolumeCreated { record }, 0)
      .unwrap();
  }
  for account in [first_account, other_account] {
    let record = slates_db::catalog::SealKeyRecord {
      owner: slates_db::catalog::SealKeyOwner::Tenant { account },
      id: [1; 16],
      record: vec![2; slates_db::catalog::SEAL_KEY_RECORD_BYTES],
    };
    db.mutate(&mut segment, &Op::SealKeySet { record }, 0)
      .unwrap();
  }
  db.mutate(&mut segment, &Op::VolumeDestroyed { id: vid(1) }, 0)
    .unwrap();
  assert!(
    tenant_key(&db, first_account),
    "the tenant still has a volume here"
  );
  db.mutate(&mut segment, &Op::VolumeDestroyed { id: vid(2) }, 0)
    .unwrap();
  assert!(
    !tenant_key(&db, first_account),
    "erased with the tenant's last volume"
  );
  assert!(
    tenant_key(&db, other_account),
    "another tenant's key is untouched"
  );
  drop(db);
  let recovered = open(&mut segment);
  assert!(!tenant_key(&recovered, first_account) && tenant_key(&recovered, other_account));
}

// ---------------------------------------------------------------- acknowledgements that name owed ids

/// One step of a client whose acknowledgements name the ids it still owes (§4.9), and of their delivery to the
/// database, in any order, more than once, late.
#[derive(Clone, Debug)]
enum OwedStep {
  /// The client issues its next id, owed until answered.
  Issue,
  /// The server records the completion of the client's `n`-th owed id, and the client takes the answer (no longer
  /// owed).
  Answer(usize),
  /// The server records the completion of the `n`-th owed id, and the client holds it for a resend under it (still
  /// owed: an `Overloaded` answer to a verb that ran).
  Hold(usize),
  /// The client releases its `n`-th owed id unanswered (gives the resend up).
  Release(usize),
  /// The server runs the client's `n`-th owed id and it waits for a version to commit (A-252): it stays owed, durable as
  /// waiting until its completion replaces that.
  Await(usize),
  /// The client sends an acknowledgement of where it stands: every id issued, the owed ones named.
  Acknowledge,
  /// The `n`-th acknowledgement in flight is delivered, and stays in flight too when `again` (a duplicate).
  Deliver(usize, bool),
  /// The database is dropped and recovered.
  Crash,
}

fn owed_step() -> impl Strategy<Value = OwedStep> {
  prop_oneof![
    3 => Just(OwedStep::Issue),
    2 => (0..8usize).prop_map(OwedStep::Answer),
    1 => (0..8usize).prop_map(OwedStep::Hold),
    1 => (0..8usize).prop_map(OwedStep::Release),
    1 => (0..8usize).prop_map(OwedStep::Await),
    2 => Just(OwedStep::Acknowledge),
    3 => (0..8usize, any::<bool>()).prop_map(|(n, again)| OwedStep::Deliver(n, again)),
    1 => Just(OwedStep::Crash),
  ]
}

/// Shape: the client of the owed-acknowledgement model.
const OWING_CLIENT: u32 = 9;
/// Shape: the green its waiting requests wait on.
const OWED_GREEN: VolumeId = VolumeId { bytes: [5; 16] };

/// The model client: its last issued id, the ids it owes, and the acknowledgements it sent not yet delivered.
#[derive(Default)]
struct OwingClient {
  issued: u32,
  owed: std::collections::BTreeSet<u32>,
  recorded: std::collections::BTreeSet<u32>,
  /// The ids waiting for a version, by that version (A-252).
  awaiting: std::collections::BTreeMap<u32, u64>,
  in_flight: Vec<(u32, Vec<u32>)>,
}

impl OwingClient {
  /// The `n`-th owed id, wrapping.
  fn nth_owed(&self, n: usize) -> Option<u32> {
    let count = self.owed.len();
    (count > 0)
      .then(|| self.owed.iter().nth(n % count).copied())
      .flatten()
  }
}

/// Applies one step of the owed-acknowledgement model.
fn owed_step_applied(
  step: &OwedStep,
  client: &mut OwingClient,
  db: &mut Db,
  seg: &mut AnchorSegment,
) -> Result<(), TestCaseError> {
  let record = |sequence: u32| Op::CompletionRecorded {
    record: CompletionRecord {
      origin: 0,
      client: OWING_CLIENT,
      sequence,
      result: sequence.to_le_bytes().to_vec(),
    },
  };
  match step {
    OwedStep::Issue => {
      client.issued += 1;
      client.owed.insert(client.issued);
    }
    OwedStep::Answer(n) | OwedStep::Hold(n) => {
      if let Some(sequence) = client.nth_owed(*n) {
        if !client.recorded.contains(&sequence) {
          db.mutate(seg, &record(sequence), 0).map_err(failed)?;
          client.recorded.insert(sequence);
        }
        if matches!(step, OwedStep::Answer(_)) {
          client.owed.remove(&sequence);
        }
      }
    }
    OwedStep::Await(n) => {
      if let Some(sequence) = client.nth_owed(*n)
        && !client.recorded.contains(&sequence)
      {
        let version = u64::from(sequence) + 100;
        db.mutate(
          seg,
          &Op::CompletionAwaiting {
            origin: 0,
            client: OWING_CLIENT,
            sequence,
            green: OWED_GREEN,
            version,
          },
          0,
        )
        .map_err(failed)?;
        client.awaiting.insert(sequence, version);
      }
    }
    OwedStep::Release(n) => {
      if let Some(sequence) = client.nth_owed(*n) {
        client.owed.remove(&sequence);
      }
    }
    OwedStep::Acknowledge => client
      .in_flight
      .push((client.issued, client.owed.iter().copied().collect())),
    OwedStep::Deliver(n, again) => {
      let count = client.in_flight.len();
      if count > 0 {
        let (up_to, owed) = if *again {
          client.in_flight[n % count].clone()
        } else {
          client.in_flight.remove(n % count)
        };
        let acknowledged = Op::CompletionsAcknowledged {
          origin: 0,
          client: OWING_CLIENT,
          up_to,
          owed,
        };
        db.mutate(seg, &acknowledged, 0).map_err(failed)?;
      }
    }
    OwedStep::Crash => {}
  }
  Ok(())
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig { cases: slates_test_seeds::cases(120), ..ProptestConfig::default() }))]

  /// §4.9, raft's check of the owed-id rule: a client issues ids, has some answered, holds some for a resend (the
  /// server recorded them), releases some, and sends acknowledgements of where it stands, which reach the database in
  /// any order, duplicated and late, with crashes between, under a small pinned snapshot cadence. Expect after every
  /// step: no id the client still owes is released (`Seen::Acknowledged`), an owed id the server recorded is answered
  /// from its record, and every recovery equals the live partition whole.
  #[test]
  fn no_owed_id_is_released_whatever_order_its_acknowledgements_arrive_in(steps in proptest::collection::vec(owed_step(), 1..200), snapshot_every in 64u64..4_096) {
    let mut seg = segment("slates-db-owed", LOG_BYTES);
    let mut db = open(&mut seg);
    db.set_policy(SnapshotPolicy { bytes_between_snapshots: snapshot_every });
    let mut client = OwingClient::default();
    for step in &steps {
      owed_step_applied(step, &mut client, &mut db, &mut seg)?;
      if matches!(step, OwedStep::Crash) {
        let live = db.partition().to_snapshot(0);
        drop(db);
        db = recover(&mut seg, 0, caps(), 0).map_err(failed)?.0;
        db.set_policy(SnapshotPolicy { bytes_between_snapshots: snapshot_every });
        prop_assert_eq!(db.partition().to_snapshot(0), live, "recovery differs from the live partition");
      }
      for owed in &client.owed {
        let seen = db.partition().completion(0, OWING_CLIENT, *owed);
        prop_assert!(seen != Seen::Acknowledged, "owed id {} released after {:?}", owed, step);
        if client.recorded.contains(owed) {
          prop_assert_eq!(seen, Seen::Completed(owed.to_le_bytes().to_vec()), "owed id {} lost its record", owed);
        } else if let Some(version) = client.awaiting.get(owed) {
          // Never `New`: a request that ran and waits is never run again, across any recovery (A-252).
          prop_assert_eq!(
            seen,
            Seen::Awaiting { object: OWED_GREEN.bytes, version: *version },
            "owed id {} lost its wait", owed
          );
        }
      }
    }
  }
}

/// §4.16 "Commit", A-252: a request's wait and the effect it waits on are one durable step. Do: in one transaction, a
/// wait and a second operation; drop the database before the commit, recover; then the same and commit, recover. Expect:
/// before the commit neither is there, the request `New` (it never ran as far as the log knows); after it the wait is
/// there, and a completion recorded later replaces it through a further recovery.
#[test]
fn a_wait_and_its_transaction_are_one_durable_step() {
  let mut seg = segment("slates-db-await-atomic", LOG_BYTES);
  let mut db = open(&mut seg);
  let wait = Op::CompletionAwaiting {
    origin: 0,
    client: OWING_CLIENT,
    sequence: 3,
    green: OWED_GREEN,
    version: 7,
  };
  let other = Op::CompletionRecorded {
    record: CompletionRecord {
      origin: 0,
      client: OWING_CLIENT,
      sequence: 1,
      result: b"other".to_vec(),
    },
  };
  db.begin();
  db.mutate(&mut seg, &wait, 0).unwrap();
  db.mutate(&mut seg, &other, 0).unwrap();
  drop(db);
  let (mut db, ..) = recover(&mut seg, 0, caps(), 0).unwrap();
  assert_eq!(db.partition().completion(0, OWING_CLIENT, 3), Seen::New);
  assert_eq!(db.partition().completion(0, OWING_CLIENT, 1), Seen::New);
  db.begin();
  db.mutate(&mut seg, &wait, 0).unwrap();
  db.mutate(&mut seg, &other, 0).unwrap();
  db.commit(&mut seg).unwrap();
  drop(db);
  let (mut db, ..) = recover(&mut seg, 0, caps(), 0).unwrap();
  assert_eq!(
    db.partition().completion(0, OWING_CLIENT, 3),
    Seen::Awaiting {
      object: OWED_GREEN.bytes,
      version: 7
    }
  );
  db.mutate(
    &mut seg,
    &Op::CompletionRecorded {
      record: CompletionRecord {
        origin: 0,
        client: OWING_CLIENT,
        sequence: 3,
        result: b"seven".to_vec(),
      },
    },
    0,
  )
  .unwrap();
  drop(db);
  let (db, ..) = recover(&mut seg, 0, caps(), 0).unwrap();
  assert_eq!(
    db.partition().completion(0, OWING_CLIENT, 3),
    Seen::Completed(b"seven".to_vec())
  );
}
