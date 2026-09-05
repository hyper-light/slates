//! The records of a partition (§4.8's data model, §4.4's `Volume` catalog part, §4.13's
//! principals and access lists, §4.15's grant, landing-lease and landing records, §4.14's
//! audit record). Every record has one canonical encoding through `Wire`, which is what the
//! log, the snapshots and (from Phase 8) the register puts carry; the schema hash of each
//! travels in front of it so a changed definition is refused, never misread.

use slates_wire::Wire;

/// A volume id: 128 random bits whose high half names the creator host (§4.8 "Lookup").
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct VolumeId {
  /// The bytes.
  pub bytes: [u8; 16],
}

/// A snapshot id, unique within its volume.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SnapshotId {
  /// The value.
  pub value: u64,
}

/// A principal, established at rendezvous (§4.13).
#[derive(Wire, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Principal {
  /// A Unix user.
  Uid {
    /// The uid.
    uid: u32,
  },
  /// A Windows SID.
  Sid {
    /// The SID text.
    sid: String,
  },
  /// A TLS leaf certificate, by the BLAKE3 of its bytes.
  Certificate {
    /// The hash.
    hash: [u8; 32],
  },
}

impl Principal {
  /// The index key: a kind byte then the identity bytes.
  pub fn key(&self) -> Vec<u8> {
    /// Format: the kind bytes of the principal key.
    const KIND_UID: u8 = 1;
    /// Format: see `KIND_UID`.
    const KIND_SID: u8 = 2;
    /// Format: see `KIND_UID`.
    const KIND_CERTIFICATE: u8 = 3;
    let mut out = Vec::new();
    match self {
      Principal::Uid { uid } => {
        out.push(KIND_UID);
        out.extend_from_slice(&uid.to_le_bytes());
      }
      Principal::Sid { sid } => {
        out.push(KIND_SID);
        out.extend_from_slice(sid.as_bytes());
      }
      Principal::Certificate { hash } => {
        out.push(KIND_CERTIFICATE);
        out.extend_from_slice(hash);
      }
    }
    out
  }
}

/// Rights on a volume (§4.13).
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rights {
  /// Attach for reading, snapshot reads, status, read_base, versions, export.
  pub read: bool,
  /// Attach for writing, the mutating verbs, snapshot, clone, submit, rebase, pin, rewitness,
  /// materialize.
  pub write: bool,
  /// Resize, destroy, archive, changing the list, revoking leases.
  pub admin: bool,
}

/// One access-list entry.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct AccessEntry {
  /// The principal.
  pub principal: Principal,
  /// Its rights.
  pub rights: Rights,
}

/// Bounded or dynamic (§4.2).
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizeClass {
  /// Reserved at creation.
  Bounded {
    /// The limit in bytes.
    limit: u64,
  },
  /// Grown from measured rates up to a maximum.
  Dynamic {
    /// The maximum in bytes.
    max: u64,
  },
}

/// The name-equivalence policy.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamePolicy {
  /// Byte-exact names.
  Exact,
  /// Folded (normalization and case), as APFS.
  Fold,
}

/// The volume's role in the merge plane (§4.16).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum Role {
  /// An ordinary volume.
  Plain,
  /// A work volume over a green.
  Work {
    /// The green.
    green: VolumeId,
    /// The base version.
    base_version: u64,
    /// Whether increments stream.
    stream: bool,
  },
  /// A green volume.
  Green {
    /// Whether increments need evidence.
    require_evidence: bool,
    /// The head version.
    head_version: u64,
  },
}

/// The policy a volume was created with.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct PolicyRecord {
  /// The size class.
  pub size: SizeClass,
  /// The name policy.
  pub names: NamePolicy,
  /// Whether unlocked memory refuses.
  pub require_locked: bool,
  /// The role.
  pub role: Role,
}

/// What the volume sits on (§4.15).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum BaseRecord {
  /// Nothing beneath.
  Scratch,
  /// A host directory, re-opened at recovery from this path (never a string on a hot path).
  Path {
    /// The path.
    path: String,
  },
}

/// The volume state machine (§4.4).
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolumeState {
  /// Being created.
  Creating,
  /// Serving.
  Live,
  /// A snapshot in progress.
  Sealing,
  /// A granted landing writing the target.
  Landing,
  /// The live tree dropped, a compressed snapshot kept.
  Archived,
  /// Coming back from an archive.
  Restoring,
  /// Being torn down.
  Destroying,
  /// Gone (tombstone).
  Destroyed,
}

/// A volume's lease (§4.4 "Leases and fencing", D-16).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct LeaseRecord {
  /// The holder.
  pub holder: Principal,
  /// The fencing epoch.
  pub epoch: u64,
  /// Expires at, monotonic ns.
  pub expires_ns: u64,
}

/// The catalog record of a volume.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct VolumeRecord {
  /// The id.
  pub id: VolumeId,
  /// The name, scoped to the host and user.
  pub name: String,
  /// The owning shard.
  pub owner_shard: u16,
  /// The policy.
  pub policy: PolicyRecord,
  /// The base.
  pub base: BaseRecord,
  /// The head snapshot.
  pub head: SnapshotId,
  /// The head epoch.
  pub epoch: u64,
  /// Bytes referenced.
  pub referenced_bytes: u64,
  /// Bytes unique.
  pub unique_bytes: u64,
  /// The state.
  pub state: VolumeState,
  /// The lease, when held.
  pub lease: Option<LeaseRecord>,
  /// The owner principal.
  pub owner: Principal,
  /// The access list.
  pub access: Vec<AccessEntry>,
  /// Created at, monotonic ns.
  pub created_ns: u64,
}

/// Where a snapshot's records and content are held (§4.8).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum PlacementState {
  /// The owner only.
  Local,
  /// Acknowledged by f+1 candidates in the region, and in the mirror when listed.
  Placed {
    /// The acknowledging hosts in the region.
    region: Vec<u64>,
    /// The acknowledging hosts in the mirror.
    mirror: Option<Vec<u64>>,
  },
}

/// A snapshot record.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct SnapshotRecord {
  /// The id.
  pub id: SnapshotId,
  /// The volume.
  pub volume: VolumeId,
  /// The epoch it sealed.
  pub epoch: u64,
  /// The identity once hashed.
  pub identity: Option<[u8; 32]>,
  /// The placement.
  pub placed: PlacementState,
  /// Taken at, monotonic ns.
  pub taken_ns: u64,
}

/// A lineage edge: a clone and its origin.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct LineageEdge {
  /// The clone.
  pub child: VolumeId,
  /// The origin volume.
  pub origin_volume: VolumeId,
  /// The origin snapshot.
  pub origin_snapshot: SnapshotId,
}

/// Who attached.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum Consumer {
  /// An SDK client.
  Sdk {
    /// The client id.
    client: u32,
  },
  /// A bridge mount.
  Bridge,
  /// The launcher.
  Launcher,
}

/// The form of an attachment.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum AttachForm {
  /// Under the root mount.
  Root,
  /// At a chosen path.
  ChosenPath {
    /// The path.
    path: String,
  },
}

/// An attachment record.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct AttachmentRecord {
  /// The id.
  pub id: u64,
  /// The volume.
  pub volume: VolumeId,
  /// The consumer.
  pub consumer: Consumer,
  /// The snapshot, for a read-only attachment.
  pub snapshot: Option<SnapshotId>,
  /// The form.
  pub form: AttachForm,
  /// The principal.
  pub principal: Principal,
}

/// A completion record (RIFL, §4.9).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct CompletionRecord {
  /// The client.
  pub client: u32,
  /// The sequence.
  pub sequence: u32,
  /// The reply bytes.
  pub result: Vec<u8>,
}

/// The surface a grant came from (§4.15).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum GrantSurface {
  /// The command line.
  Cli,
  /// A confirmation surface of a harness.
  Confirmation {
    /// The harness.
    harness: String,
  },
}

/// A grant's scope.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum GrantScope {
  /// One landing of the manifest.
  Once,
  /// Every landing of the volume into the target for the session.
  Session {
    /// The session.
    session: u64,
  },
}

/// A grant's state.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantState {
  /// Usable.
  Issued,
  /// Used by its landing.
  Consumed,
  /// Past its term or session.
  Expired,
  /// Revoked by the human.
  Revoked,
}

/// A grant record.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct GrantRecord {
  /// The id.
  pub id: u64,
  /// The principal it was made for.
  pub principal: Principal,
  /// The surface.
  pub surface: GrantSurface,
  /// The volume.
  pub volume: VolumeId,
  /// The snapshot.
  pub snapshot: SnapshotId,
  /// The canonical target.
  pub target: String,
  /// The manifest hash it binds.
  pub manifest: [u8; 32],
  /// The scope.
  pub scope: GrantScope,
  /// Issued at, monotonic ns.
  pub issued_ns: u64,
  /// Expires at, monotonic ns.
  pub expires_ns: u64,
  /// The state.
  pub state: GrantState,
}

/// The landing lease on a canonical target.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct LandingLeaseRecord {
  /// The target.
  pub target: String,
  /// The holder (a session).
  pub holder: u64,
  /// The fencing generation.
  pub generation: u64,
  /// Expires at, monotonic ns.
  pub expires_ns: u64,
}

/// The landing state machine (§4.15).
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum LandingState {
  /// Building the manifest.
  Planning,
  /// Presented; no grant yet.
  AwaitingGrant,
  /// Verdicts being computed.
  Validating,
  /// Writing.
  Writing,
  /// Syncing.
  Syncing,
  /// Advancing witnesses.
  Advancing,
  /// Done.
  Done,
  /// Some entries failed; the rest landed.
  Partial,
  /// Refused before any write.
  Refused,
  /// Stopped mid-way; every entry old or new.
  Aborted,
}

/// A landing record.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct LandingRecord {
  /// The id.
  pub id: u64,
  /// The volume.
  pub volume: VolumeId,
  /// The snapshot.
  pub snapshot: SnapshotId,
  /// The target.
  pub target: String,
  /// The manifest hash.
  pub manifest: [u8; 32],
  /// The grant, once bound.
  pub grant: Option<u64>,
  /// The state.
  pub state: LandingState,
  /// Entries written.
  pub written: u32,
  /// Entries in conflict.
  pub conflicts: u32,
}

/// What the audit log records (§4.14, §4.15).
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditKind {
  /// A grant was issued.
  GrantIssued,
  /// A grant was revoked.
  GrantRevoked,
  /// A manifest was planned.
  LandingPlanned,
  /// Verdicts were computed.
  LandingValidated,
  /// One entry landed.
  EntryWritten,
  /// One entry was refused.
  EntryRefused,
  /// The landing reached a terminal state.
  LandingFinished,
}

/// One audit record; content-free except the manifest hash.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct AuditRecord {
  /// Sequence number.
  pub seq: u64,
  /// Monotonic ns.
  pub at_ns: u64,
  /// The kind.
  pub kind: AuditKind,
  /// The principal.
  pub principal: Principal,
  /// The grant, when one is bound.
  pub grant: Option<u64>,
  /// The landing.
  pub landing: Option<u64>,
  /// The manifest.
  pub manifest: Option<[u8; 32]>,
  /// The terminal state, for `LandingFinished`.
  pub outcome: Option<LandingState>,
}
