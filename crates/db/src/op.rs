//! The operations: every local mutation of a partition, as the log records it (§4.8 `OpLog`
//! "every local mutation"). An operation carries what `apply` needs and nothing it decides:
//! the guard (`Partition::check`) runs before the append, so a recorded operation applies
//! unconditionally and replay is deterministic.

use slates_wire::Wire;

use crate::catalog::{
  AccessEntry, AttachmentRecord, AuditRecord, BaseRecord, CompletionRecord, ConsumerRecord,
  GrantRecord, GrantState, LandingLeaseRecord, LandingRecord, LandingState, LeaseRecord,
  LineageEdge, NfsClientRecord, NfsDelegationRecord, NfsLockRecord, NfsOpenRecord, PlacementState,
  SizeClass, SnapshotId, SnapshotRecord, Tombstone, VolumeId, VolumeRecord, VolumeState,
};

/// One mutation.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum Op {
  /// A volume was created.
  VolumeCreated {
    /// The record.
    record: VolumeRecord,
  },
  /// A volume changed state.
  VolumeStateChanged {
    /// The volume.
    id: VolumeId,
    /// The state.
    state: VolumeState,
  },
  /// A volume was resized.
  VolumeResized {
    /// The volume.
    id: VolumeId,
    /// The size class.
    size: SizeClass,
  },
  /// A volume's accounting was recorded.
  VolumeAccounted {
    /// The volume.
    id: VolumeId,
    /// Bytes referenced.
    referenced_bytes: u64,
    /// Bytes unique.
    unique_bytes: u64,
  },
  /// A volume's head advanced (a snapshot was taken).
  VolumeHeadAdvanced {
    /// The volume.
    id: VolumeId,
    /// The head snapshot.
    head: SnapshotId,
    /// The head epoch.
    epoch: u64,
  },
  /// A volume's access list changed.
  AccessChanged {
    /// The volume.
    id: VolumeId,
    /// The list.
    access: Vec<AccessEntry>,
  },
  /// A volume was destroyed (its record becomes a tombstone).
  VolumeDestroyed {
    /// The volume.
    id: VolumeId,
  },
  /// A snapshot was recorded.
  SnapshotTaken {
    /// The record.
    record: SnapshotRecord,
  },
  /// A snapshot's placement changed.
  SnapshotPlaced {
    /// The volume.
    volume: VolumeId,
    /// The snapshot.
    id: SnapshotId,
    /// The placement.
    placed: PlacementState,
  },
  /// A snapshot was destroyed.
  SnapshotDestroyed {
    /// The volume.
    volume: VolumeId,
    /// The snapshot.
    id: SnapshotId,
  },
  /// A lineage edge was recorded.
  LineageAdded {
    /// The edge.
    edge: LineageEdge,
  },
  /// A lease was taken or renewed.
  LeaseTaken {
    /// The volume.
    volume: VolumeId,
    /// The lease.
    lease: LeaseRecord,
  },
  /// A lease was released or expired.
  LeaseReleased {
    /// The volume.
    volume: VolumeId,
  },
  /// An attachment was added.
  AttachmentAdded {
    /// The record.
    record: AttachmentRecord,
  },
  /// An attachment was removed.
  AttachmentRemoved {
    /// The attachment.
    id: u64,
  },
  /// A host mount's attachment was bound to the path the mount was established at (§4.4
  /// `Binding → Bound`; GAP-A9-4): its form becomes `ChosenPath { path }`. Appended.
  AttachmentBound {
    /// The attachment.
    id: u64,
    /// The mount point, as the mounting process established it.
    path: String,
  },
  /// A completion was recorded.
  CompletionRecorded {
    /// The record.
    record: CompletionRecord,
  },
  /// A client acknowledged completions.
  CompletionsAcknowledged {
    /// The host id (`HostId.0`) whose client this is (this node for a local client; the authenticated origin
    /// for a forwarded one) — the high half of the globally-unique completion key.
    origin: u64,
    /// The client.
    client: u32,
    /// Every sequence up to and including this one.
    up_to: u32,
  },
  /// A grant was issued.
  GrantIssued {
    /// The record.
    record: GrantRecord,
  },
  /// A grant changed state.
  GrantStateChanged {
    /// The grant.
    id: u64,
    /// The state.
    state: GrantState,
  },
  /// A landing lease was taken.
  LandingLeaseTaken {
    /// The record.
    record: LandingLeaseRecord,
  },
  /// A landing lease was released.
  LandingLeaseReleased {
    /// The target.
    target: String,
  },
  /// A landing was recorded.
  LandingRecorded {
    /// The record.
    record: LandingRecord,
  },
  /// A landing changed state.
  LandingStateChanged {
    /// The landing.
    id: u64,
    /// The state.
    state: LandingState,
    /// Entries written.
    written: u32,
    /// Entries in conflict.
    conflicts: u32,
  },
  /// An audit record was appended.
  AuditAppended {
    /// The record.
    record: AuditRecord,
  },
  /// A green volume's merge chain advanced by one accepted increment (§4.16 "the merge record is a
  /// partition log append"; §4.8). The bytes are the merge crate's encoded increment; the database
  /// stores them opaquely (it never parses a merge structure) and the server replays them on recovery
  /// to rebuild the green. Appended at the end of the operation set for append-only evolution.
  GreenAdvanced {
    /// The green volume.
    green: VolumeId,
    /// The encoded increment (identity, base, ops document and post-state).
    increment: Vec<u8>,
  },
  /// A snapshot's content identity was computed (§4.10, D-17): the BLAKE3 root of its archive's
  /// manifest — the value the fleet's holders verify and a reader fetches the content by. A snapshot
  /// is taken O(1) with no identity (`SnapshotTaken` records `None`); the identity is recorded once the
  /// content plane has archived it. Appended at the end of the operation set for append-only evolution.
  SnapshotIdentified {
    /// The volume.
    volume: VolumeId,
    /// The snapshot.
    id: SnapshotId,
    /// The manifest identity.
    identity: [u8; 32],
  },
  /// A consumer was enrolled under an account by the human surface (§4.13). Appended at the end for
  /// append-only evolution.
  ConsumerEnrolled {
    /// The record.
    record: ConsumerRecord,
  },
  /// A consumer's enrollment was revoked by the human surface (§4.13).
  ConsumerRevoked {
    /// The consumer.
    consumer: u64,
  },
  /// A green volume's origin — its version-0 state, captured from a complete immutable base — was
  /// recorded (§4.16 "the origin version from a snapshot"; the A-9 requirement that a chain starts
  /// "from scratch or a complete immutable base"). The bytes are the merge crate's encoded origin;
  /// the database stores them opaquely (it never parses a merge structure) and the server replays
  /// them before the chain on recovery, so a green seeded from a base recovers to the same state as
  /// one still running. Recorded once, before any increment; counted against the same byte budget
  /// as the chain. Appended at the end of the operation set for append-only evolution.
  GreenOriginated {
    /// The green volume.
    green: VolumeId,
    /// The encoded origin.
    origin: Vec<u8>,
  },
  /// Reserve a client identity before handoff (§4.7, §4.9). The control partition retains
  /// the highest issued id so a restarted listener cannot reuse a completion identity.
  ClientIdReserved {
    /// The highest client id issued so far.
    client: u32,
  },
  /// An NFSv4 open was recorded or changed at its file's owner (§4.6 A-37). Appended at the end for
  /// append-only evolution, as are the NFSv4 operations after it.
  NfsOpenSet {
    /// The record.
    record: NfsOpenRecord,
  },
  /// An NFSv4 open was closed or freed.
  NfsOpenCleared {
    /// The state id's `other`.
    other: [u8; 12],
  },
  /// An NFSv4 lock state was recorded or changed at its file's owner.
  NfsLockSet {
    /// The record.
    record: NfsLockRecord,
  },
  /// An NFSv4 lock state was freed, or went with its open.
  NfsLockCleared {
    /// The state id's `other`.
    other: [u8; 12],
  },
  /// Every open and lock state of an NFSv4 client at this owner was dropped (its lease lapsed, it
  /// rebooted, or it was destroyed).
  NfsClientStateCleared {
    /// The client id.
    clientid: u64,
  },
  /// An NFSv4 client was recorded or changed at the listener (§4.6 A-37).
  NfsClientSet {
    /// The record.
    record: NfsClientRecord,
  },
  /// An NFSv4 client was dropped at the listener.
  NfsClientCleared {
    /// The client id.
    clientid: u64,
  },
  /// This partition's NFSv4 instance advanced (§4.6 A-37): once per daemon life, before its first
  /// NFSv4 id is minted, so every client, session and state id the partition mints names an instance
  /// no earlier life used, and no id is ever minted twice.
  NfsInstanceAdvanced {
    /// The new instance.
    instance: u32,
  },
  /// A volume took a base: a landing made a scratch volume an overlay over the directory it landed on
  /// (§4.15 step 9), so a recovery reacquires that directory for it. Appended.
  VolumeRebased {
    /// The volume.
    id: VolumeId,
    /// What it now sits on.
    base: BaseRecord,
  },
  /// A takeover successor adopted a destroyed volume's tombstone (AUD-29-43): it owes the tombstone and its
  /// retirement to the volume's candidate holders exactly as the destroying owner did. Appended.
  TombstoneAdopted {
    /// The tombstone.
    tombstone: Tombstone,
  },
  /// Every candidate holder holds a destroyed volume's retirement (AUD-29-43): nothing is owed any more and
  /// the tombstone gives its slot back. Appended.
  TombstoneRetired {
    /// The destroyed volume.
    id: VolumeId,
  },
  /// A snapshot mount was re-pinned (§4.4 `Bound → Advancing → Bound`; AUD-29-76): the attachment, and every
  /// container bind borrowing it, now presents `snapshot`. Appended.
  AttachmentRepinned {
    /// The attachment.
    id: u64,
    /// The snapshot it now presents.
    snapshot: crate::catalog::SnapshotId,
  },
  /// An NFSv4 delegation was granted or changed at its file's owner (§4.6 A-78). Appended.
  NfsDelegationSet {
    /// The record.
    record: NfsDelegationRecord,
  },
  /// An NFSv4 delegation was returned or revoked. Appended.
  NfsDelegationCleared {
    /// The state id's `other`.
    other: [u8; 12],
  },
}

impl Op {
  /// The operation's name, for counters and the audit.
  pub fn name(&self) -> &'static str {
    match self {
      Op::VolumeCreated { .. } => "volume_created",
      Op::VolumeStateChanged { .. } => "volume_state_changed",
      Op::VolumeResized { .. } => "volume_resized",
      Op::VolumeAccounted { .. } => "volume_accounted",
      Op::VolumeHeadAdvanced { .. } => "volume_head_advanced",
      Op::AccessChanged { .. } => "access_changed",
      Op::VolumeDestroyed { .. } => "volume_destroyed",
      Op::TombstoneAdopted { .. } => "tombstone_adopted",
      Op::TombstoneRetired { .. } => "tombstone_retired",
      Op::SnapshotTaken { .. } => "snapshot_taken",
      Op::SnapshotPlaced { .. } => "snapshot_placed",
      Op::SnapshotIdentified { .. } => "snapshot_identified",
      Op::ConsumerEnrolled { .. } => "consumer_enrolled",
      Op::ConsumerRevoked { .. } => "consumer_revoked",
      Op::SnapshotDestroyed { .. } => "snapshot_destroyed",
      Op::LineageAdded { .. } => "lineage_added",
      Op::LeaseTaken { .. } => "lease_taken",
      Op::LeaseReleased { .. } => "lease_released",
      Op::AttachmentAdded { .. } => "attachment_added",
      Op::AttachmentRemoved { .. } => "attachment_removed",
      Op::AttachmentBound { .. } => "attachment_bound",
      Op::CompletionRecorded { .. } => "completion_recorded",
      Op::CompletionsAcknowledged { .. } => "completions_acknowledged",
      Op::GrantIssued { .. } => "grant_issued",
      Op::GrantStateChanged { .. } => "grant_state_changed",
      Op::LandingLeaseTaken { .. } => "landing_lease_taken",
      Op::LandingLeaseReleased { .. } => "landing_lease_released",
      Op::LandingRecorded { .. } => "landing_recorded",
      Op::LandingStateChanged { .. } => "landing_state_changed",
      Op::AuditAppended { .. } => "audit_appended",
      Op::GreenAdvanced { .. } => "green_advanced",
      Op::GreenOriginated { .. } => "green_originated",
      Op::ClientIdReserved { .. } => "client_id_reserved",
      Op::NfsOpenSet { .. } => "nfs_open_set",
      Op::NfsOpenCleared { .. } => "nfs_open_cleared",
      Op::NfsLockSet { .. } => "nfs_lock_set",
      Op::NfsLockCleared { .. } => "nfs_lock_cleared",
      Op::NfsClientStateCleared { .. } => "nfs_client_state_cleared",
      Op::NfsClientSet { .. } => "nfs_client_set",
      Op::NfsClientCleared { .. } => "nfs_client_cleared",
      Op::NfsInstanceAdvanced { .. } => "nfs_instance_advanced",
      Op::VolumeRebased { .. } => "volume_rebased",
      Op::AttachmentRepinned { .. } => "attachment_repinned",
      Op::NfsDelegationSet { .. } => "nfs_delegation_set",
      Op::NfsDelegationCleared { .. } => "nfs_delegation_cleared",
    }
  }
}
