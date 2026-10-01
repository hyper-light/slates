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
  /// An **enrolled consumer** under a host account (§4.13 "Principals": "a trusted enrollment
  /// establishes `ConsumerId` and scoped rights for a workload under that account"): the workload
  /// bound its channel at rendezvous with the capability its enrollment minted, so two agents sharing a
  /// uid are two principals here, each with only the rights an access list names for it. Appended for
  /// append-only wire evolution.
  Consumer {
    /// The host account (uid) the consumer was enrolled under.
    account: u32,
    /// The consumer's id, minted at enrollment.
    consumer: u64,
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
    /// Format: see `KIND_UID`.
    const KIND_CONSUMER: u8 = 4;
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
      Principal::Consumer { account, consumer } => {
        out.push(KIND_CONSUMER);
        out.extend_from_slice(&account.to_le_bytes());
        out.extend_from_slice(&consumer.to_le_bytes());
      }
    }
    out
  }
}

/// An enrolled consumer's durable record (§4.13): who enrolled it (the account), the secret capability
/// it attests with, and whether a human has revoked it. Replayed with the log, so the binding a session
/// resumes under is the one the human established. The capability lives in the anchor segment under the
/// same trust boundary as the daemon's grant-issuer secret: mapped only by the supervisor, the daemon and
/// the anchor's user — the workload proves *knowledge* of it over the ring (a keyed hash of its channel's
/// client id), and the capability itself never crosses a channel after the one delivery to the human.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct ConsumerRecord {
  /// The consumer id.
  pub consumer: u64,
  /// The host account it was enrolled under.
  pub account: u32,
  /// The consumer's secret capability — the key its attestation proofs are made under.
  pub secret: [u8; 32],
  /// Revoked by a human: every later effect refuses.
  pub revoked: bool,
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

/// A destroyed volume's register tombstone (§4.4 destroy "tombstone the id"; AUD-29-43): the sequence its
/// replicated registers are closed at. The owner ships it to every candidate holder as the object's final
/// register value, so a holder releases what it held for the volume and a takeover adopts the destruction
/// instead of the last live head; once every candidate holds it the owner ships the retirement at the next
/// sequence and, when every candidate holds that too, drops the tombstone (`Op::TombstoneRetired`). Until
/// then it occupies the volume's slot in the partition's volume capacity, so tombstones are bounded by the
/// same derived cap volumes are.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tombstone {
  /// The destroyed volume.
  pub volume: VolumeId,
  /// The register sequence the tombstone is written at: one past every sequence the volume's registers
  /// used (its head epoch and, for a green, its newest version).
  pub sequence: u64,
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
  /// The catalog register's sequence (§4.8 "catalog entries are registers the owner writes under that
  /// epoch"): 0 at creation, raised by every op that changes what a successor must rebuild — the policy,
  /// the access list, the base — so the owner ships each change as a newer record and a takeover adopts the
  /// newest (AUD-29-17). Raised in `apply`, so replay reproduces it.
  pub catalog_version: u64,
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
  /// An OCI binding borrowing a host mount (§4.6): it survives the issuing client and ends
  /// with explicit detach, the source mount, or the volume. The parent must be a bridge
  /// attachment of the same volume; dependencies cannot form chains or cycles.
  Mount {
    /// The owning host mount's attachment id.
    attachment: u64,
  },
}

/// The form of an attachment. Append-only.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum AttachForm {
  /// Under the root mount.
  Root,
  /// At a chosen path.
  ChosenPath {
    /// The path.
    path: String,
  },
  /// A container bind (§4.6 A-9): the verified host mount point bound by the OCI runtime at a path
  /// inside the container; the authorized binding the record carries.
  Oci {
    /// The host mount point (the bind's source).
    source: String,
    /// The container path (the bind's destination).
    destination: String,
    /// Whether the bind is read-only.
    read_only: bool,
  },
  /// A Linux FUSE mount (§4.6 "Linux"; AUD-29-64) at its mount point. Kept apart from a chosen-path host
  /// mount because its device is the daemon process's own: it cannot outlive the process, so recovery ends
  /// it, where an NFS host mount reconnects to the restarted daemon.
  FuseMount {
    /// The mount point.
    path: String,
  },
}

impl AttachForm {
  /// The host mount point a mounted form names: a chosen-path host mount's or a FUSE mount's.
  pub fn mount_point(&self) -> Option<&str> {
    match self {
      Self::ChosenPath { path } | Self::FuseMount { path } => Some(path),
      Self::Root | Self::Oci { .. } => None,
    }
  }
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
  /// The rights this attachment was granted, from the volume's access list at attach time (§4.13; AUD-01):
  /// what the attachment authorizes at a host mount, so an NFS request served through it runs under these
  /// rights — never the unconditional read/write the edge used to fabricate. The owner holds every right.
  pub rights: Rights,
  /// The attachment's **mount capability token** (§4.6, §4.13; AUD-01): a random secret minted when the
  /// attachment is created, returned to the authorized consumer by `attach`, and presented at the NFS
  /// mount so the edge authorizes the connection as this attachment's consumer with its `rights` — the
  /// bearer capability the loopback edge needs, since a supplied `AUTH_SYS` uid and loopback reachability
  /// are not consumer authority. Zero for an attachment that establishes no host mount (an SDK record form,
  /// a green pin), which the NFS edge never authorizes.
  pub token: [u8; 16],
}

/// A completion record (RIFL, §4.9). The idempotency key is **globally unique**: `origin` is the host whose
/// client issued the request — this node's own host for a local client, and the *authenticated* forwarding
/// peer for a cross-node forwarded verb (§4.8 "Lookup") — so a forwarded request from another node's client
/// can never collide with a local client that happens to share its (per-node) id.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct CompletionRecord {
  /// The host id (`HostId.0`) whose client issued the request — this node for a local client, the
  /// authenticated forwarding peer for a cross-node forwarded verb. Part of the idempotency key, so per-node
  /// client ids stay globally unique. Stored as the raw `u64` (host ids serialize as `u64`; `HostId` is an
  /// in-memory wrapper).
  pub origin: u64,
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
  /// The target directory's device when the landing was presented: with `principal` (the consumer),
  /// `volume`, `snapshot` and `target`, the grant's whole binding (§4.13 "Grants": the target identity), so
  /// a grant rebuilt after a restart binds the directory the human approved and not whatever holds its path
  /// then (AUD-29-01, AUD-29-06; appended for append-only evolution, as is the field after it).
  pub target_device: u64,
  /// The target directory's inode on that device.
  pub target_inode: u64,
}

/// The landing lease on a canonical target: one record per target, kept by the control partition (§4.15
/// "Ownership facts"; AUD-29-03).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct LandingLeaseRecord {
  /// The target: its canonical identity's key (`slates_land::grant::lease_key`, the directory's device and
  /// inode), so every alias of one directory names one record.
  pub target: String,
  /// The holder: the landing attempt that took it.
  pub holder: u64,
  /// The fencing generation: the take's log sequence on the control partition, so it only grows — across
  /// releases, which remove the record, and restarts.
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

/// An NFSv4 open held at its file's owner partition (§4.6 A-37): what `slates_bridge_nfs::v4::files`
/// keeps for it, so a restart rebuilds the owner's file state and the client's state ids stay valid.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct NfsOpenRecord {
  /// The state id's `other`.
  pub other: [u8; 12],
  /// The client holding it.
  pub clientid: u64,
  /// The open-owner.
  pub owner: Vec<u8>,
  /// The file handle it opened.
  pub fh: Vec<u8>,
  /// The share access.
  pub access: u32,
  /// The share deny.
  pub deny: u32,
  /// The state id's current seqid.
  pub seqid: u32,
}

/// One byte range of an NFSv4 lock state: `[start, end)`, `end == u64::MAX` reaching the file's end.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct NfsLockRange {
  /// The first byte.
  pub start: u64,
  /// One past the last byte, or `u64::MAX`.
  pub end: u64,
  /// A write (exclusive) lock; a read (shared) lock otherwise.
  pub write: bool,
}

/// An NFSv4 lock state held at its file's owner partition (§4.6 A-37).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct NfsLockRecord {
  /// The state id's `other`.
  pub other: [u8; 12],
  /// The client holding it.
  pub clientid: u64,
  /// The lock-owner.
  pub owner: Vec<u8>,
  /// The file handle.
  pub fh: Vec<u8>,
  /// The open it was created from.
  pub open: [u8; 12],
  /// The state id's current seqid.
  pub seqid: u32,
  /// Its ranges, ascending.
  pub ranges: Vec<NfsLockRange>,
}

/// An NFSv4 client held at the listener's partition (§4.6 A-37): its id, owner, verifier, principal
/// and next CREATE_SESSION sequence, so a restarted listener still knows it and its state ids stay
/// valid; its sessions are not kept (a client re-creates them after `NFS4ERR_BADSESSION`).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct NfsClientRecord {
  /// The client id.
  pub clientid: u64,
  /// The client owner (`co_ownerid`).
  pub owner: Vec<u8>,
  /// Format: the owner's verifier, an NFSv4 `verifier4` (8 bytes, RFC 7863).
  pub verifier: [u8; 8],
  /// The principal that established it.
  pub principal: u32,
  /// The next CREATE_SESSION sequence it must send.
  pub create_seq: u32,
}
