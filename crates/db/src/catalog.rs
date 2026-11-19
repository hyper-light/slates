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

/// A principal, established at rendezvous (§4.13). A host account means something only on its host (A-237): a uid, a
/// SID and a consumer enrolled under an account each carry the host whose account it is, by that host's certificate
/// anchor (`slates_server::deploy`'s blake3 of the node certificate, stable across the host's restarts, never the
/// per-boot member id). So one uid on two hosts is two principals, a forwarded request is scoped by the authenticated
/// session's anchor ([`Principal::rehosted`]), and an owner or access entry names (host, account). A certificate names
/// itself on every host.
#[derive(Wire, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Principal {
  /// A Unix user of a host.
  Uid {
    /// The host whose user it is, by its certificate anchor.
    host: crate::HostId,
    /// The uid.
    uid: u32,
  },
  /// A Windows SID of a host.
  Sid {
    /// The host whose account it is, by its certificate anchor.
    host: crate::HostId,
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
  /// uid are two principals here, each with only the rights an access list names for it.
  Consumer {
    /// The host whose account the consumer was enrolled under, by its certificate anchor.
    host: crate::HostId,
    /// The host account (uid) the consumer was enrolled under.
    account: u32,
    /// The consumer's id, minted at enrollment.
    consumer: u64,
  },
}

/// Format: the bit that marks a tenant id hashed from a principal. Since A-237 every tenant is hashed (a host account
/// with its host), and the bit stays so no hashed tenant is ever a bare uid.
const HASHED_TENANT: u64 = 1 << 63;

impl Principal {
  /// The tenant a volume owned by this principal seals under (A-92; A-9: the host account): a uid, or a consumer's host
  /// account; a SID or a certificate by a namespaced hash of its identity, marked with the top bit so it never names the
  /// same tenant as a uid. Here, beside the records, so a volume's destroy can tell when its tenant has no volume left.
  pub fn tenant(&self) -> u64 {
    let hashed = |kind: &[u8], identity: &[&[u8]]| {
      let mut hasher = blake3::Hasher::new();
      hasher.update(b"slates tenant v2");
      hasher.update(kind);
      for part in identity {
        hasher.update(part);
      }
      let mut word = [0u8; size_of::<u64>()];
      if let Some(head) = hasher.finalize().as_bytes().get(..size_of::<u64>()) {
        word.copy_from_slice(head);
      }
      u64::from_le_bytes(word) | HASHED_TENANT
    };
    // A host account seals under its host as well (A-237): one uid on two hosts is two tenants.
    match self {
      Principal::Uid { host, uid } => hashed(b"uid", &[&host.0.to_le_bytes(), &uid.to_le_bytes()]),
      Principal::Consumer { host, account, .. } => {
        hashed(b"uid", &[&host.0.to_le_bytes(), &account.to_le_bytes()])
      }
      Principal::Sid { host, sid } => hashed(b"sid", &[&host.0.to_le_bytes(), sid.as_bytes()]),
      Principal::Certificate { hash } => hashed(b"certificate", &[hash]),
    }
  }

  /// The host whose account this principal is (A-237); `None` for a certificate, which names itself on every host.
  pub fn host(&self) -> Option<crate::HostId> {
    match self {
      Principal::Uid { host, .. }
      | Principal::Sid { host, .. }
      | Principal::Consumer { host, .. } => Some(*host),
      Principal::Certificate { .. } => None,
    }
  }

  /// This principal as the host `host` authenticated it (A-237): a host account's host replaced by `host`, a
  /// certificate as it is. The owner of a forwarded request scopes its principal by the session's authenticated
  /// anchor this way, never by the host the origin's request names.
  #[must_use]
  pub fn rehosted(&self, host: crate::HostId) -> Principal {
    match self {
      Principal::Uid { uid, .. } => Principal::Uid { host, uid: *uid },
      Principal::Sid { sid, .. } => Principal::Sid {
        host,
        sid: sid.clone(),
      },
      Principal::Consumer {
        account, consumer, ..
      } => Principal::Consumer {
        host,
        account: *account,
        consumer: *consumer,
      },
      Principal::Certificate { hash } => Principal::Certificate { hash: *hash },
    }
  }

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
      Principal::Uid { host, uid } => {
        out.push(KIND_UID);
        out.extend_from_slice(&host.0.to_le_bytes());
        out.extend_from_slice(&uid.to_le_bytes());
      }
      Principal::Sid { host, sid } => {
        out.push(KIND_SID);
        out.extend_from_slice(&host.0.to_le_bytes());
        out.extend_from_slice(sid.as_bytes());
      }
      Principal::Certificate { hash } => {
        out.push(KIND_CERTIFICATE);
        out.extend_from_slice(hash);
      }
      Principal::Consumer {
        host,
        account,
        consumer,
      } => {
        out.push(KIND_CONSUMER);
        out.extend_from_slice(&host.0.to_le_bytes());
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
  /// The consumer's own write policy (A-285): the mode its launcher gave the agent's session, set through the
  /// launcher's authority; the stricter of it and a volume's governs the consumer's acts. `None` until one is set.
  /// Appended.
  pub write: Option<WritePolicy>,
  /// How many times the consumer's write policy was set (A-285), as a volume's `write_version`. Appended.
  pub write_version: u64,
}

/// git's author and committer of the commits the server makes for a principal (§4.17.1 "A push is a work" item 4;
/// A-664): the name and the email exactly as git writes them. The server holds them to git's own rules before it records
/// them (`slates_git::derived::Identity`: no byte git would strip from either end or drop from inside, a name not
/// empty), so every commit made of them carries these bytes.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct GitIdentity {
  /// The name.
  pub name: String,
  /// The email: may be empty, which git writes `<>`.
  pub email: String,
}

/// A principal's identities in the tools whose objects the server makes for it (§4.13 "a principal's record"; §4.17.1
/// "A push is a work" item 4; §4.17.3 decision 7; A-664): git's author and committer, and Mercurial's user. Set whole by
/// the principal's owner on the control channel, never on the principal's own channel, as the version after the one
/// its setter read, so a setting made from a stale read changes nothing (`Op::IdentitiesSet`). Kept on the partition
/// that holds the principal's record, keyed by the principal's key.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct IdentityRecord {
  /// The principal.
  pub principal: Principal,
  /// git's author and committer; `None` when the principal has none, so a submit of its to a git repository's green is
  /// refused `IdentityRequired`.
  pub git: Option<GitIdentity>,
  /// Mercurial's user, `Name <address>` (§4.17.3 decision 7); `None` when it has none.
  pub mercurial: Option<String>,
  /// How many times they were set: the first setting's is 1.
  pub version: u64,
}

/// What a version-control credential is (§4.13, §4.17.2 "Access"; A-625 part 3): an SSH user key or an HTTP token.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum CredentialKind {
  /// An SSH user key (RFC 4252 §7): its public key blob as a client sends it, and its algorithm's name, the blob's
  /// first string (`ssh-ed25519`, `ssh-rsa`, ...), so a later algorithm joins with no change of record.
  SshKey {
    /// The key's algorithm, as its blob names it.
    algorithm: String,
    /// The public key blob (RFC 4253 §6.6).
    blob: Vec<u8>,
  },
  /// An HTTP token, Basic's password or a bearer token (RFC 7617, RFC 6750): held as its SHA-256 alone, the record's
  /// id, so the token itself is never kept.
  HttpToken,
}

/// A version-control credential a human enrolled for a principal (§4.13: through the CLI's control verbs, never MCP or
/// an SDK; A-625 part 3). Named by the SHA-256 of what a client presents (an SSH key's blob, OpenSSH's fingerprint; an
/// HTTP token's bytes), so an edge finds it by one lookup on the partition its id routes to, and a token's bytes are
/// never held. A record never changes once made: enrolled whole, revoked whole.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct CredentialRecord {
  /// The SHA-256 of what a client presents.
  pub id: [u8; 32],
  /// The principal it proves.
  pub principal: Principal,
  /// What it is.
  pub kind: CredentialKind,
  /// When it stops proving its principal, on the wall clock in nanoseconds since the Unix epoch; none when it never
  /// does.
  pub expires_ns: Option<u64>,
}

impl CredentialRecord {
  /// Whether it still proves its principal at wall-clock instant `now_ns`: it has no expiry, or its expiry is later.
  pub fn live_at(&self, now_ns: u64) -> bool {
    self.expires_ns.is_none_or(|expires_ns| now_ns < expires_ns)
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
  /// Whether the volume is live-shipped (§4.10, A-200): each of its publications is shipped to its candidates and a
  /// barrier is answered only once the owner and `f` of them hold it. False at creation; set by `LiveShip`.
  /// Appended.
  pub live_shipped: bool,
  /// The holder set the volume's live shipper ships to, under its epoch (A-240): committed at `f + 1` through the
  /// catalog register before the shipper's first entry, so a takeover counts phase one's reports from exactly the hosts
  /// a durable entry is held by. `None` until a shipper commits one. Appended.
  pub live_holders: Option<LiveHolders>,
  /// The volume's write policy (A-285): how a merge and a landing of it are permitted, set by the launcher or the
  /// harness through the issuer, never by an agent. `None` until one is set: merges at once, lands under approval.
  /// Appended.
  pub write: Option<WritePolicy>,
  /// How many times the write policy or the trusted signers were set (A-285, A-321): a setter names the version it
  /// read, so a captured proof sets nothing once the policy has moved. Appended.
  pub write_version: u64,
  /// The keys trusted to sign the volume's work (signed work, A-321), in SSH wire form, in the order the launcher set
  /// them: a merge's signature verifies under its work's and its green's, a landing's under its volume's. Appended.
  pub signers: Vec<Vec<u8>>,
}

/// How one kind of write is permitted (A-285): the mode the user gave the harness or agent.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteMode {
  /// Written at once, with no approval asked.
  Bypass,
  /// Answered with an approval request the harness presents; written once the approval binds the manifest.
  Ask,
  /// Refused.
  Forbid,
}

/// A volume's write policy (A-285): the mode for its merges and the mode for its landings, and whether its merges must
/// be signed (A-321).
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct WritePolicy {
  /// How a merge into the volume is permitted.
  pub merge: WriteMode,
  /// How a landing of the volume is permitted.
  pub land: WriteMode,
  /// Merges into the volume (a green) or from it (a work) must carry a signature that verifies under a trusted signer.
  pub require_signed_merges: bool,
}

/// A live shipper's holder set and the epoch it ships under (A-240): the `2f` candidates it ships to, as their host
/// ids, fixed for the shipper's life.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct LiveHolders {
  /// The shipper's epoch.
  pub epoch: u64,
  /// The hosts it ships to.
  pub holders: Vec<u64>,
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
  /// Nothing beneath: an image volume, holding the disk image of `volume`'s `snapshot` (§4.6, A-160). It pins the
  /// snapshot, as a clone does, so the merge of its image can clone the snapshot it was built from; it shares no
  /// record with it. Appended.
  Image {
    /// The volume imaged.
    volume: VolumeId,
    /// The snapshot imaged.
    snapshot: SnapshotId,
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
  /// The takeover that made this node the volume's owner (AC-8.3, A-325), recorded with the volume when the successor
  /// materializes it, so its loss report survives the successor's restart over its anchor; `None` for a volume made
  /// here. Appended.
  pub taken_over: Option<TakeoverRecord>,
}

/// The takeover that made a node a volume's owner (AC-8.3, A-325; the loss report in `slates status`): the epoch the head
/// was placed under, the head sequence the volume was served from, and whether it was rebuilt from the volume's live log
/// (no acknowledged write lost) or from the seal the head names (the lost owner's edits after it are the loss window).
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct TakeoverRecord {
  /// The epoch the adopted head was placed under.
  pub epoch: u64,
  /// The head sequence the volume was served from.
  pub head_sequence: u64,
  /// Whether the volume was rebuilt from its live log rather than from the seal the head names.
  pub from_live_log: bool,
  /// Set when the takeover recovered the volume past its tolerance (A-309, A-332): more of the hosts holding it were lost
  /// than its placement tolerates, so it was rebuilt from the newest copy the hosts that remained held, and a write
  /// acknowledged only to the lost hosts may be gone. `None` within tolerance. Appended.
  pub past_tolerance: Option<TakeoverPastTolerance>,
}

/// How a takeover went past a volume's tolerance (A-309, A-332): which of its parts were recovered from fewer surviving
/// holders than recovery counts on, and by how much. At least one part is set.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct TakeoverPastTolerance {
  /// The volume's head register: its first cohort short of the recovery quorum when the takeover round adopted the newest
  /// record every surviving member of the volume's cohorts held (`slates_db::takeover::Readiness::ReadyPastTolerance`).
  /// `None` when the register was recovered within tolerance.
  pub registers: Option<RecoveryShortfall>,
  /// The volume's live log (A-200): its recorded holders short of `f + 1` members when phase one adopted the greatest head
  /// every surviving recorded holder reported (`slates_db::live::past_tolerance`). `None` when the log was recovered
  /// within tolerance, or the volume has none.
  pub live_log: Option<RecoveryShortfall>,
}

/// A recovery count short of its quorum (A-332): the hosts recovery counts on, and how many of them were still members
/// when it recovered past tolerance.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryShortfall {
  /// The hosts recovery needed: a cohort's recovery quorum, or `f + 1` of a live log's recorded holders.
  pub needed: u64,
  /// How many of the hosts that count were members then (fewer than `needed`).
  pub remaining: u64,
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

/// What a captured clone's move carried of its capture (§4.15 `capture_base`, A-291's move; A-503, stage 2 of a captured
/// clone's landing): the identity of the head the move handed over (its archive manifest's, the same wherever the tree
/// is exported), and the captured state of every path where the capture and that head differ, in
/// `slates_vfs::captured::CarriedCapture`'s canonical encoding (opaque here: this crate keeps it, the volume core reads
/// it). Shipped in the volume's catalog register, so the member a move hands the volume to judges its landings against
/// the capture as the member that made the clone did.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct CarriedRecord {
  /// The head the carried paths are relative to, by its archive manifest's identity.
  pub head: [u8; 32],
  /// The carried paths, encoded by the volume core.
  pub entries: Vec<u8>,
}

impl CarriedRecord {
  /// Format: the head a captured clone's record names before any head of it was carried (A-544): all zeros, which no
  /// archive's BLAKE3 identity is in practice, so no head a successor adopts matches it and its landing is refused by kind.
  pub const NO_HEAD: [u8; 32] = [0; 32];
}

/// A volume's carried capture as a partition keeps it (A-503): what its catalog ships ([`CarriedRecord`]) and, when this
/// node holds that head's tree as one of the volume's snapshots, that snapshot, the **mark** its landings are judged from.
/// The mark is this node's own snapshot id, so it is never shipped.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct CarriedCaptureRecord {
  /// The volume.
  pub volume: VolumeId,
  /// What its catalog ships.
  pub carried: CarriedRecord,
  /// Its snapshot holding the head `carried` is relative to, when this node holds one.
  pub mark: Option<SnapshotId>,
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
  /// A guest device (§4.6 virtio-fs; AUD-29-68): the harness's VMM, admitted for the authenticated consumer
  /// the record's principal names. It ends with the device: the guest's hangup, a revocation, a detach, the
  /// volume's destroy, and the daemon's process (recovery ends it, as it ends a FUSE mount). Appended.
  Guest,
  /// A client of another host, its attachment recorded on this owner (§4.10, A-222): `host` is the forwarding peer's
  /// authenticated member id, never a claim of the request's. It ends with a detach routed by its volume, its host's
  /// reap of `client` (`RemoteReap`), the host's retirement, and the volume's destroy; the volume's write lease stays
  /// its holder's while it lives and lasts one term after it ends (D-16, A-414). Appended.
  Remote {
    /// The member id of the client's host.
    host: u64,
    /// The client's id on that host.
    client: u32,
  },
  /// A CLI attachment (§4.7, A-230): one a process makes and exits, for later processes to name, so it outlives the
  /// client that made it. It holds a term, its holder's (A-384), renewed by every verb that names it and by its holder's
  /// `Renew`, and ends at the term's lapse; the volume's write lease then stays its principal's only while another of
  /// its write attachments of the volume lives (D-16, A-414). Appended.
  Termed {
    /// When its term lapses, on the shard's monotonic clock.
    expires_ns: u64,
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
  /// mount because its device is the daemon process's own: recovery keeps it only when the anchor held the
  /// device across the restart and the kernel can resend (A-61), and ends it otherwise, where an NFS host mount
  /// reconnects to the restarted daemon.
  FuseMount {
    /// The mount point.
    path: String,
  },
  /// A host mount presenting only one directory of the volume (§4.6 scoped exports; AUD-29-76): the
  /// directory's inode, and the mount point once the kernel mount is bound.
  ScopedMount {
    /// The directory's inode number: the export's root.
    scope: u64,
    /// The mount point, once bound.
    mount_point: Option<String>,
  },
  /// A Linux FUSE mount presenting only one directory of the volume (§4.6 scoped exports; AUD-29-76): ended by
  /// recovery as a `FuseMount` is, held to its directory as a `ScopedMount` is.
  ScopedFuseMount {
    /// The mount point.
    path: String,
    /// The directory's inode number: the mount's root.
    scope: u64,
  },
  /// A guest device's tag (`mount -t virtiofs <tag>` in the guest), with the directory it presents when it
  /// presents one (AUD-29-76). Appended.
  GuestTag {
    /// The tag.
    tag: String,
    /// The directory's inode number, when the device presents one directory.
    scope: Option<u64>,
  },
  /// A Linux FUSE mount shared with other local users' processes (`allow_other`): what a container bind on
  /// Linux borrows (§4.6 A-9). Ended by recovery as a `FuseMount` is. Appended.
  SharedFuseMount {
    /// The mount point.
    path: String,
    /// The directory's inode number, when the mount presents one directory.
    scope: Option<u64>,
  },
  /// A host mount bound to one Unix identity (§4.6 A-115): it serves requests whose caller is `uid`, and to every
  /// other caller, the superuser's included, only attributes and an `ACCESS` that grants nothing, so no kernel's
  /// cache of what one identity read through it serves another. Optionally one directory. Appended.
  BoundMount {
    /// The identity served.
    uid: u32,
    /// The directory's inode number, when the mount presents one directory.
    scope: Option<u64>,
    /// The mount point, once bound.
    mount_point: Option<String>,
  },
  /// A Shards microVM's fs device served over an anonymous stream (§4.6, the Shards route): the daemon keeps one end
  /// of a socket pair and serves the attachment on it, and the other end went to whoever attached. The stream is the
  /// daemon process's own, so recovery ends the record as it ends a guest device's. Optionally one directory.
  /// Appended.
  ShardsFs {
    /// The directory's inode number, when the stream presents one directory.
    scope: Option<u64>,
  },
  /// A Windows WinFsp volume at a drive letter (§4.6 "Windows"). The WinFsp file system is the daemon process's own,
  /// so recovery ends the record as it ends a FUSE mount the anchor did not hold. Appended.
  WinFspMount {
    /// The drive letter and its colon (`S:`).
    path: String,
  },
}

impl AttachForm {
  /// The host mount point a mounted form names: a chosen-path host mount's or a FUSE mount's.
  pub fn mount_point(&self) -> Option<&str> {
    match self {
      Self::ChosenPath { path }
      | Self::FuseMount { path }
      | Self::ScopedFuseMount { path, .. }
      | Self::SharedFuseMount { path, .. }
      | Self::WinFspMount { path } => Some(path),
      Self::ScopedMount { mount_point, .. } | Self::BoundMount { mount_point, .. } => {
        mount_point.as_deref()
      }
      Self::Root | Self::Oci { .. } | Self::GuestTag { .. } | Self::ShardsFs { .. } => None,
    }
  }

  /// The directory a scoped mount presents, if this form is one.
  pub fn scope(&self) -> Option<u64> {
    match self {
      Self::ScopedMount { scope, .. } | Self::ScopedFuseMount { scope, .. } => Some(*scope),
      Self::GuestTag { scope, .. }
      | Self::SharedFuseMount { scope, .. }
      | Self::BoundMount { scope, .. }
      | Self::ShardsFs { scope } => *scope,
      _ => None,
    }
  }

  /// The one Unix identity a bound mount serves (§4.6 A-115); `None` for a form that serves every caller.
  pub fn bound_uid(&self) -> Option<u32> {
    match self {
      Self::BoundMount { uid, .. } => Some(*uid),
      _ => None,
    }
  }

  /// The mount point of a FUSE form (whole, scoped or shared): a mount the daemon's own process holds, so it
  /// ends with it.
  pub fn fuse_mount_point(&self) -> Option<&str> {
    match self {
      Self::FuseMount { path }
      | Self::ScopedFuseMount { path, .. }
      | Self::SharedFuseMount { path, .. } => Some(path),
      _ => None,
    }
  }

  /// The mount point of a mount the daemon itself established (a FUSE form or a WinFsp volume), which a later bind
  /// leaves as it is: the daemon named it, so no client's report of where it is replaces it.
  pub fn daemon_mount_point(&self) -> Option<&str> {
    match self {
      Self::WinFspMount { path } => Some(path),
      _ => self.fuse_mount_point(),
    }
  }

  /// Whether a container runtime's processes may reach this mount (`allow_other`).
  pub fn shared_with_other_users(&self) -> bool {
    matches!(self, Self::SharedFuseMount { .. })
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
  /// The kernel's id of the host mount this attachment was seen bound to (A-540): the filesystem id the kernel's mount
  /// table names it by, recorded once the mount is seen (at its bind, or at a successor's start), so a restarted
  /// daemon can tell a mount the kernel let go while no daemon ran from one it never saw. `None` until seen, and for
  /// every form that is no host mount. Appended.
  pub mount: Option<u64>,
}

/// A request that ran and waits for a version of a green to commit (§4.16 "Commit", A-252), as a snapshot carries it.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct AwaitingCompletion {
  /// The sequence.
  pub sequence: u32,
  /// The green.
  pub green: VolumeId,
  /// The version it waits for.
  pub version: u64,
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
  /// For a completion a green's successor adopted from the green's merge records (A-255) whose resend can reach it only
  /// through its work's route, the work whose submit it completes (A-288): the work's catalog record, that route, is kept
  /// while the green's owner retains one such completion (`Partition::completions_routed_by`). `None` for every completion
  /// a verb ran here, and for one of a client on the work's own owner node, whose resend that node's records answer.
  /// Appended.
  pub work: Option<VolumeId>,
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
  /// The volume's write policy (A-285): the daemon issued the grant itself under a bypass policy. Appended.
  Policy,
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
  /// A grant reached its term unused and ended (A-266): recorded with the grant's end, in one atom, under the principal
  /// the grant was made for, since no one acted. Appended last, so no published tag moves (D-15).
  GrantExpired,
}

/// What the control partition has appended of one partition's owed audit records (A-265's crossing): every record that
/// partition owed before crossing sequence `through` is in the audit, so a batch sent again appends only what is new.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuditReceivedMark {
  /// The partition that owed the records.
  pub origin: u16,
  /// The crossing sequence of the first record from `origin` not yet appended.
  pub through: u64,
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
  /// How the landing's signature was judged (signed work, A-321), on a `LandingPlanned` and a `LandingFinished`
  /// record: the landing's provenance. `None` on every other record, and on a landing that carried no signature.
  /// Appended.
  pub signature: Option<SignatureVerdict>,
}

/// How an act's signature was judged (signed work, A-321), as a record keeps it: the server's verdict
/// (`slates_server::signed_work`), opaque here, so a record read back reports it as it was judged.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct SignatureVerdict {
  /// The verdict's kind (verified or invalid), as the server numbers it.
  pub kind: u8,
  /// The verdict's detail (the signer's key type, or why it did not verify), as the server numbers it.
  pub detail: u8,
  /// The signer's key fingerprint (zero for a signature that did not verify).
  pub fingerprint: [u8; 32],
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
  /// The uid that made the open. Part of the open's identity: an open-owner is opaque data the client
  /// chooses, so it carries no authority between users, and a restored open must know whose it was.
  pub uid: u32,
  /// The file handle it opened.
  pub fh: Vec<u8>,
  /// The share access.
  pub access: u32,
  /// The share deny.
  pub deny: u32,
  /// The state id's current seqid.
  pub seqid: u32,
}

/// An NFSv4 delegation held at its file's owner (RFC 8881 §10.4; §4.6 A-78): durable, as an open is (A-37), because
/// the anchor keeps a mount across a daemon restart and a client keeps trusting its cache while it holds one.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct NfsDelegationRecord {
  /// The delegation state id's `other`.
  pub other: [u8; 12],
  /// The client holding it.
  pub clientid: u64,
  /// The file handle it delegates.
  pub fh: Vec<u8>,
  /// Whether it is a write delegation (`OPEN_DELEGATE_WRITE`); a read delegation otherwise.
  pub write: bool,
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
/// valid; its sessions are records of their own ([`NfsSessionRecord`], A-127).
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

/// An NFSv4.1 channel's attributes as negotiated (RFC 8881 §18.36, `channel_attrs4`), as a session record carries them
/// (A-127).
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct NfsChannelRecord {
  /// `ca_headerpadsize`.
  pub header_pad: u32,
  /// `ca_maxrequestsize`.
  pub max_request: u32,
  /// `ca_maxresponsesize`.
  pub max_response: u32,
  /// `ca_maxresponsesize_cached`.
  pub max_response_cached: u32,
  /// `ca_maxoperations`.
  pub max_operations: u32,
  /// `ca_maxrequests`: the slot count.
  pub max_requests: u32,
}

/// A session's back channel as its record carries it (A-127): the client's callback program, the RPC credential its
/// callbacks carry (an encoded `opaque_auth` the client listed), and the minor version they name.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct NfsCallbackRecord {
  /// The callback program number.
  pub program: u32,
  /// The encoded credential.
  pub credential: Vec<u8>,
  /// The minor version callbacks name.
  pub minor: u32,
}

/// An NFSv4.1 session held at its client's listener partition (A-127): enough for a restarted listener to serve it
/// again, so a client retries on its own slots after a daemon restart and carries on with no CREATE_SESSION. Its
/// slots' sequence ids are not kept: a restored slot takes its next request as it comes.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct NfsSessionRecord {
  /// Format: the session id, an NFSv4.1 `sessionid4` (16 bytes, RFC 8881 §2.10.3).
  pub sessionid: [u8; 16],
  /// The client it belongs to.
  pub clientid: u64,
  /// The CREATE_SESSION sequence id that created it.
  pub sequence: u32,
  /// The fore channel.
  pub fore: NfsChannelRecord,
  /// The back channel.
  pub back: NfsChannelRecord,
  /// The back channel's callback, when the client asked for one.
  pub callback: Option<NfsCallbackRecord>,
}

/// An NFSv4.1 call record held at the partition of the shard that served its calls (A-127): the session slot it
/// answers for, the client it belongs to, and the record's bytes (`slates_bridge_nfs::v4::replay`, opaque here). It
/// rides the file-state transaction a call of it changed, so a compound retried after a restart is answered with what
/// it did, never run again.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct NfsCallRecord {
  /// Format: the session slot: an NFSv4.1 session id (16 bytes) then the slot as a big-endian word.
  pub key: [u8; 20],
  /// The client the session belongs to.
  pub clientid: u64,
  /// The record's bytes.
  pub record: Vec<u8>,
}

/// An NFSv4 purge a shard owes every owner shard (2026-10-10): a dropped client's file state and call records, or a
/// destroyed session's call records. Recorded in the same transaction as the drop that makes it owed, and settled once
/// every owner acknowledged it, so a death between the two cannot leave a dropped client's locks at an owner with no
/// lease left to expire them.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NfsPurgeRecord {
  /// The client.
  pub clientid: u64,
  /// Format: the destroyed session's id (16 bytes), or `None` for the dropped client's whole state.
  pub session: Option<[u8; 16]>,
}

/// A volume name held at its name's partition for a volume another partition holds (§4.8 "Lookup", A-236): a clone
/// lives beside its origin, for copy-on-write, while its name hashes elsewhere. The name's partition is where the name
/// is looked up and where its uniqueness is decided, so it holds this alias, which points at the clone. An alias is
/// reserved before the clone is made and confirmed after: a reserved one holds the name (a create of it is refused) but
/// resolves nothing until confirmed, and a daemon's recovery confirms or releases every reserved one by asking the
/// clone's partition whether the clone exists under this name.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct NameAliasRecord {
  /// The name.
  pub name: String,
  /// The volume it names, on the partition its id encodes.
  pub volume: VolumeId,
  /// Whether the clone was made and the alias confirmed; until then the alias holds the name and resolves nothing.
  pub confirmed: bool,
}

/// A work being created on its name's partition while its green's owner admits it (A-247): the id it was given, which
/// this record's own log entry consumes so no other volume is given it, and the name it holds meanwhile.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct PendingCreationRecord {
  /// The name.
  pub name: String,
  /// The work's id.
  pub work: VolumeId,
  /// The completion key's origin.
  pub origin: u64,
  /// The completion key's client.
  pub client: u32,
  /// The completion key's sequence.
  pub sequence: u32,
}

/// A work's submit that went to its green's owner and whose answer the work has not yet settled (A-247): kept on the
/// work's partition from the ask until the settle, so no other verb of the work runs between the owner's verdict and
/// the work's reset, across a lost answer or a restart. The request is kept whole, to be run again under its own key
/// once the owner answers it.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct PendingSubmitRecord {
  /// The work.
  pub work: VolumeId,
  /// The completion key's origin.
  pub origin: u64,
  /// The completion key's client.
  pub client: u32,
  /// The completion key's sequence.
  pub sequence: u32,
  /// The client the request was served for.
  pub client_id: u32,
  /// The principal it was asked as.
  pub principal: Principal,
  /// The request, in the wire's encoding.
  pub body: Vec<u8>,
}

/// A live work's hold on a version of its green (A-247), kept on the green's partition whichever partition the work
/// lives on: the green's reachable floor is the minimum over these and its pins, so a checkpoint never folds past a
/// base a work still reads and composes against.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkFloorRecord {
  /// The green.
  pub green: VolumeId,
  /// The work, on the partition its id encodes.
  pub work: VolumeId,
  /// The version the work is based on.
  pub base: u64,
}

/// Format: the bytes of hyper-seal's wrapped key record (version, parent id, parent generation, AES-256-KW of the key;
/// seal.md §3.3).
pub const SEAL_KEY_RECORD_BYTES: usize = 61;

/// Whose sealing key a record holds (A-92): a tenant's key (the host account, A-9), its naming key, or a volume's
/// lineage key.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SealKeyOwner {
  /// A tenant's key, wrapped by the node's root.
  Tenant {
    /// The tenant: a host account, or the namespaced hash of a principal with no numeric account.
    account: u64,
  },
  /// A volume's naming key (seal.md §7: keyed names for its chunks), wrapped by the volume's lineage key, so it travels
  /// with the volume to a successor (A-92 piece 4c).
  Naming {
    /// The volume.
    volume: VolumeId,
  },
  /// A volume's lineage key, wrapped by its tenant's key.
  Lineage {
    /// The volume.
    volume: VolumeId,
  },
  /// The node's ML-KEM-1024 recipient key pair (seal.md §6), sealed whole under the node's root: what a successor
  /// unwraps a taken-over volume's lineage key with. Recorded in partition 0 only. Appended.
  Recipient,
  /// A pair key between an owner shard and a candidate node (A-92 piece 4b), wrapped by this node's root. On the owner
  /// shard `host` is the candidate and `partition` the owner's own; on the candidate (partition 0) `host` is the
  /// owner and `partition` the owner's shard. Appended.
  Pair {
    /// The other node.
    host: u64,
    /// The owner's partition.
    partition: u16,
  },
  /// A pair key another node's owner shard delivered to this node (A-92 piece 4b), on its control shard (partition 0),
  /// wrapped by this node's root: `host` the owner, `partition` the owner's shard. Kept apart from [`Self::Pair`], this
  /// node's own key with that node: when this node's owner shard is partition 0 as well, the two shared one owner
  /// until 2026-10-09, and whichever was recorded first made the other "another key for a recorded pair", refused for
  /// good (an owner's heads then never covered that holder, and its mirror shipments were never owed). Appended.
  PairReceived {
    /// The owner node.
    host: u64,
    /// The owner's partition.
    partition: u16,
  },
  /// A pair key an earlier incarnation of `host` delivered for `partition` (A-92 piece 4b), kept on this node's control
  /// shard while a head or live log this node holds is still sealed under it, so the volumes of a node that came back
  /// with fresh RAM at the same anchor (its keys gone, a new pair made) can still be taken over: `key` is its id, which
  /// a head's key entry names as its wrapping key. Appended.
  PairRetired {
    /// The owner node.
    host: u64,
    /// The owner's partition.
    partition: u16,
    /// The retired key's id.
    key: [u8; 16],
  },
}

/// Format: the most bytes a sealed recipient record holds: hyper-seal's `Recipient::seal` of a hybrid recipient is one
/// STREAM file of 5,027 bytes (header 114, id 16, ML-KEM-1024 decapsulation key 3,168 and encapsulation key 1,568,
/// P-384 scalar 48 and public point 97, tag 16), bounded at the next power of two.
pub const SEAL_RECIPIENT_RECORD_MAX: usize = 8192;

/// A sealing key, wrapped by its parent (A-92; seal.md §3.3): its owner, its own id (what its children's records name
/// as their parent), and hyper-seal's 61-byte wrapped record. The key's bytes never appear here.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct SealKeyRecord {
  /// Whose key.
  pub owner: SealKeyOwner,
  /// The key's own id.
  pub id: [u8; 16],
  /// The wrapped record: [`SEAL_KEY_RECORD_BYTES`] long, or for the node's recipient its sealed form, at most
  /// [`SEAL_RECIPIENT_RECORD_MAX`].
  pub record: Vec<u8>,
}

/// The version-control tool a repository serves (§4.17, D-28): the protocols its clients speak and the rules its pushes
/// keep (§4.17.2 git, §4.17.3 Mercurial, §4.17.4 Subversion).
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum VcsTool {
  /// git.
  Git,
  /// Mercurial.
  Mercurial,
  /// Subversion.
  Subversion,
}

/// How a repository names its objects (§4.17.1 "The repository and its greens", item 1): fixed at creation, since every
/// object id, index row and wire answer of the repository is in it.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectFormat {
  /// SHA-1 names: git's default object format and Mercurial's node ids.
  Sha1,
  /// SHA-256 names: git's `object-format=sha256`.
  Sha256,
  /// Revision numbers: Subversion, which names a node by its path and revision, not by a hash.
  Revisions,
}

/// A repository id (§4.17.1; A-515): 128 bits minted as a volume id is (`slates_server::verbs::fresh_volume_id`): the
/// creator host in the high 8 bytes, then the owner partition (bytes 8-9), then a per-host counter. The partition is
/// where the repository's record and every green of its namespace live.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct RepositoryId {
  /// The bytes.
  pub bytes: [u8; 16],
}

/// Format: the bytes of a 128-bit id that name its owner partition (8 and 9, big-endian), as a volume id's do
/// (`slates_server::verbs::owner_of`).
const ID_PARTITION_BYTES: std::ops::Range<usize> = 8..10;

/// The owner partition an id's bytes 8-9 name.
fn id_partition(bytes: &[u8; 16]) -> u16 {
  match bytes.get(ID_PARTITION_BYTES) {
    Some(&[high, low]) => u16::from_be_bytes([high, low]),
    _ => 0,
  }
}

impl RepositoryId {
  /// The partition that owns the repository and every green of its namespace.
  pub fn partition(&self) -> u16 {
    id_partition(&self.bytes)
  }
}

impl VolumeId {
  /// The partition the id names (bytes 8-9): where the volume was created and is owned within its node.
  pub fn partition(&self) -> u16 {
    id_partition(&self.bytes)
  }
}

/// A repository (§4.17.1 "The repository and its greens"; A-515): a name a tool's clients reach, its tool and object
/// format, its default branch, and its owner. Its namespace, each branch name bound to the green whose versions are the
/// branch's values, is held beside it as [`BranchRecord`]s, so binding one branch never rewrites the others.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct RepositoryRecord {
  /// The id; its partition bytes name the partition that holds it.
  pub id: RepositoryId,
  /// The name clients reach it by: unique on its partition, which a stable hash of the name chooses
  /// (`slates_server::verbs::owner_of_name`), so unique everywhere.
  pub name: String,
  /// The tool it serves.
  pub tool: VcsTool,
  /// How it names its objects.
  pub format: ObjectFormat,
  /// The branch a client gets when it names none (git's `HEAD`): a name that may be bound to no green yet, as git's
  /// `HEAD` may name an unborn branch. `None` until set.
  pub default_branch: Option<String>,
  /// Who created it, and holds every right on it until a policy says otherwise (§4.13).
  pub owner: Principal,
  /// Its policy (A-622): empty at creation, then set whole by `Op::RepositoryPolicySet`. Appended.
  pub policy: RepositoryPolicy,
}

/// A repository's policy (§4.17.1 "Policy: protected branches, access and hooks"; A-597's matcher, A-622's record): its
/// rules by name pattern and its hooks, set whole by its owner (`Op::RepositoryPolicySet`) and carried by its record,
/// so a partition's snapshot carries it and a replay sets it again. Empty at creation: no rule and no hook, so the
/// repository's own rule decides everything (its owner holds every right, A-575) and no hook runs. Bounded by the
/// partition's merge byte budget, which its rules' and hooks' encoded bytes are charged to, and by one log record, since
/// it is set whole.
#[derive(Wire, Clone, Debug, Default, PartialEq, Eq)]
pub struct RepositoryPolicy {
  /// How many times it has been set: a setting names the version after the one its setter read, so one made from a
  /// stale read is refused and changes nothing.
  pub version: u64,
  /// The rules, in the order given, which git's precedence (the last covering rule) and Subversion's at one depth (the
  /// rule defined last) read.
  pub rules: Vec<PolicyRule>,
  /// The hooks, in the order given: the hooks of one point run in that order.
  pub hooks: Vec<HookConfig>,
  /// A Subversion repository's hooks-env (A-648), its sections in the order given: what each of its hooks' environment
  /// is. Empty for git and Mercurial, whose hooks inherit the server's environment. Appended.
  pub hooks_env: Vec<HooksEnvSection>,
}

/// What a policy rule's pattern names (§4.17.1; A-597): the names its repository's tool has, each in its tool's form.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyNames {
  /// git refs, by `git for-each-ref`'s pattern: a ref, a prefix ending at a slash, or a glob.
  Refs,
  /// Mercurial named branches, by name or `*`.
  Branches,
  /// Mercurial bookmarks, by name or `*`.
  Bookmarks,
  /// Mercurial topics, by name or `*`.
  Topics,
  /// Subversion paths: `/path`, or `:glob:/path` with wildcards.
  Paths,
}

impl PolicyNames {
  /// Whether a repository serving `tool` has these names.
  pub fn of(self, tool: VcsTool) -> bool {
    matches!(
      (tool, self),
      (VcsTool::Git, Self::Refs)
        | (
          VcsTool::Mercurial,
          Self::Branches | Self::Bookmarks | Self::Topics
        )
        | (VcsTool::Subversion, Self::Paths)
    )
  }
}

/// A right a policy rule decides (§4.17.1 "Policy").
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyRight {
  /// Read the branch.
  Read,
  /// Submit a work to it.
  Submit,
  /// Move it to a head that does not descend from its current one.
  Rewrite,
  /// Delete it.
  Delete,
}

/// Whether a policy rule allows its right or denies it.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyEffect {
  /// It allows the right.
  Allow,
  /// It denies the right.
  Deny,
}

/// One entry of the principals a policy rule speaks to.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum PolicyEntry {
  /// This principal.
  Is {
    /// The principal.
    principal: Principal,
  },
  /// Every principal but this one (acl's `!user`, authz's `~user`).
  IsNot {
    /// The principal excepted.
    principal: Principal,
  },
}

/// The principals a policy rule speaks to.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum PolicyWho {
  /// Every principal.
  Anyone,
  /// Each principal one of the entries names.
  AnyOf {
    /// The entries.
    entries: Vec<PolicyEntry>,
  },
}

/// One policy rule (§4.17.1 "Policy"; A-597): on the names `pattern` covers, `effect` for `right` to the principals
/// `who` names.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct PolicyRule {
  /// What the pattern names: the repository's tool's names.
  pub names: PolicyNames,
  /// The pattern, in its tool's form: not empty, holding no NUL, and for Subversion a path from the root.
  pub pattern: String,
  /// The right it decides.
  pub right: PolicyRight,
  /// Whether it allows or denies.
  pub effect: PolicyEffect,
  /// Whom it speaks to.
  pub who: PolicyWho,
}

/// A hook point (§4.17.1; A-597's points), each its tool's.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum HookPointName {
  /// git's `pre-receive`.
  GitPreReceive,
  /// git's `update`.
  GitUpdate,
  /// git's `post-receive`.
  GitPostReceive,
  /// Mercurial's `pretxnchangegroup`.
  HgPretxnchangegroup,
  /// Mercurial's `changegroup`.
  HgChangegroup,
  /// Subversion's `pre-commit`.
  SvnPreCommit,
  /// Subversion's `pre-revprop-change`.
  SvnPreRevpropChange,
  /// Subversion's `post-commit`.
  SvnPostCommit,
  /// Subversion's `start-commit` (A-648). Appended.
  SvnStartCommit,
  /// Subversion's `post-revprop-change` (A-648). Appended.
  SvnPostRevpropChange,
  /// Subversion's `pre-lock` (A-648). Appended.
  SvnPreLock,
  /// Subversion's `post-lock` (A-648). Appended.
  SvnPostLock,
  /// Subversion's `pre-unlock` (A-648). Appended.
  SvnPreUnlock,
  /// Subversion's `post-unlock` (A-648). Appended.
  SvnPostUnlock,
}

impl HookPointName {
  /// The tool whose point it is.
  pub fn tool(self) -> VcsTool {
    match self {
      Self::GitPreReceive | Self::GitUpdate | Self::GitPostReceive => VcsTool::Git,
      Self::HgPretxnchangegroup | Self::HgChangegroup => VcsTool::Mercurial,
      Self::SvnPreCommit
      | Self::SvnPreRevpropChange
      | Self::SvnPostCommit
      | Self::SvnStartCommit
      | Self::SvnPostRevpropChange
      | Self::SvnPreLock
      | Self::SvnPostLock
      | Self::SvnPreUnlock
      | Self::SvnPostUnlock => VcsTool::Subversion,
    }
  }

  /// Its name in its tool.
  pub fn name(self) -> &'static str {
    match self {
      Self::GitPreReceive => "pre-receive",
      Self::GitUpdate => "update",
      Self::GitPostReceive => "post-receive",
      Self::HgPretxnchangegroup => "pretxnchangegroup",
      Self::HgChangegroup => "changegroup",
      Self::SvnPreCommit => "pre-commit",
      Self::SvnPreRevpropChange => "pre-revprop-change",
      Self::SvnPostCommit => "post-commit",
      Self::SvnStartCommit => "start-commit",
      Self::SvnPostRevpropChange => "post-revprop-change",
      Self::SvnPreLock => "pre-lock",
      Self::SvnPostLock => "post-lock",
      Self::SvnPreUnlock => "pre-unlock",
      Self::SvnPostUnlock => "post-unlock",
    }
  }
}

/// One configured hook (§4.17.1: "a program the repository's owner configured").
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct HookConfig {
  /// The point it runs at.
  pub point: HookPointName,
  /// Its name: at a Mercurial point, the point's name alone or with a `.suffix` (`pretxnchangegroup.acl`), one hook a
  /// name; git and Subversion keep one hook a point, named for its point.
  pub name: String,
  /// The program it runs: an absolute path on the host that runs the repository's hooks, holding no NUL.
  pub program: String,
}

/// A variable of a Subversion repository's hooks-env (A-648): one `NAME = value` line of a section of the file
/// `svnserve.conf`'s `hooks-env` names.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct HooksEnvVariable {
  /// Its name: not empty, holding no `=` and no NUL, so that it is one variable of a hook's environment.
  pub name: String,
  /// Its value, holding no NUL, since a hook's environment is C strings.
  pub value: Vec<u8>,
}

/// A section of a Subversion repository's hooks-env (A-648; `run_hook_cmd`, `hooks.c`): the variables the hook named as
/// the section runs with, or, in the section named `default`, those of every hook with no section of its own. A hook
/// gets its section alone and never the server's environment; a section named twice is one, and one with no variables
/// is none, as Subversion's configuration reads the file.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct HooksEnvSection {
  /// Its name, a hook's or `default`: not empty, holding no NUL.
  pub name: String,
  /// Its variables, in the order given; a variable set twice keeps its last value.
  pub variables: Vec<HooksEnvVariable>,
}

/// How a green joined its repository's lineage (§4.17.1 item 4, A-575): a repository's greens share the nodes of their
/// maps in one lineage on the repository's partition, and a recovery rebuilds each the way it joined, in the order they
/// joined.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberOrigin {
  /// The lineage's first green, rebuilt from its own origin and chain.
  Root,
  /// A green made at version `version` of member `from` without a walk (`slates_merge::engine::Lineage::branch`):
  /// rebuilt by branching `from` at `version` again, then from its own checkpoint and chain.
  Branch {
    /// The member it was made from.
    from: VolumeId,
    /// The version of `from` it was made at, its own version 0.
    version: u64,
  },
}

/// A green of a repository's lineage (§4.17.1 item 4, A-575): its repository, and how it joined. A member stays one
/// while its green lives, the repository destroyed or not (A-515 leaves a destroyed repository's greens), since its
/// rebuild needs the member it was made from.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct MemberRecord {
  /// The repository whose lineage it joined.
  pub repository: RepositoryId,
  /// The green.
  pub green: VolumeId,
  /// How it joined.
  pub origin: MemberOrigin,
}

/// One name of a repository's namespace (§4.17.1; A-515): a branch, bound to the green whose versions are its values.
/// A green is bound to at most one name, and lives on its repository's partition.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct BranchRecord {
  /// The repository.
  pub repository: RepositoryId,
  /// The branch's name.
  pub name: String,
  /// The green.
  pub green: VolumeId,
}

/// A review comment on a request (§4.17.1 "Works over works", shared piece 6; A-596): anchored to a path, the whole lines
/// a byte range covers, and the version of the request it stands on (the version of the green whose versions are the
/// request's updates). It moves forward with the request by the engine's position mapping
/// (`slates_merge::engine::Green::reanchor`), its lines unchanged; once a change meets them it is outdated where it last
/// stood, never moved again (`crate::review`).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct ReviewCommentRecord {
  /// The repository whose request it is on.
  pub repository: RepositoryId,
  /// The request: the volume whose versions are the request's updates.
  pub request: VolumeId,
  /// Its number among the request's comments, from one.
  pub id: u64,
  /// Who wrote it.
  pub author: Principal,
  /// The path it is on, as the request names it.
  pub path: String,
  /// The first byte of its lines in the version it stands on.
  pub start: u64,
  /// The bytes its lines cover there, through the last one's newline (or the text's end).
  pub len: u64,
  /// The version it stands on: the one it was written on, or the one it was last moved to.
  pub version: u64,
  /// The version whose change outdated it, once one has: it stands where it last stood (`start` and `len` at
  /// `version`) and never moves again.
  pub outdated_at: Option<u64>,
  /// What it says.
  pub body: String,
}

/// An approval of a request (§4.17.1, shared piece 6; A-596): bound to the version of the request its approver saw, and
/// stale once a later change meets what the repository's policy says an approval covers (`crate::review`). A standing
/// approval is the evidence a policy counts.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct ReviewApprovalRecord {
  /// The repository whose request it is on.
  pub repository: RepositoryId,
  /// The request: the volume whose versions are the request's updates.
  pub request: VolumeId,
  /// Who approved.
  pub approver: Principal,
  /// The version of the request it saw.
  pub version: u64,
  /// The version whose change made it stale, once one has.
  pub stale_at: Option<u64>,
}

/// A check an entry makes (§4.17.1 shared piece 2; A-603, A-642): read before any of the entry's own changes and in the
/// step that commits it, against each log's own position, so it holds at exactly the positions the entry appends at. A
/// check that does not hold refuses the whole entry.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum RepositoryCheck {
  /// A green is at `version`: a head the client saw that the entry does not move is unchanged (Mercurial's
  /// `check:heads` and `unbundle`'s `heads`, §4.17.3 decision 6).
  Head {
    /// The green.
    green: VolumeId,
    /// The version it must be at.
    version: u64,
  },
  /// The names starting with `prefix` bind exactly `bound`, in name order: no name under it was added, retired or bound
  /// to another green. Where a range of names holds the heads, this with a `Head` check for each green it lists says
  /// every head is unchanged.
  Names {
    /// The prefix; empty for every name of the repository.
    prefix: String,
    /// Every name under it and the green it binds, in name order.
    bound: Vec<NameBinding>,
  },
  /// One of the repository's own state logs is at `position` (A-642): nothing changed it since the push read it.
  Log {
    /// The log.
    log: StateLog,
    /// The position it must be at.
    position: u64,
  },
}

/// The repository's own state's logs (A-642): each moved by its own changes, at its own positions, never by a counter
/// every push passes. A green's log is its chain of versions.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum StateLog {
  /// The namespace: names bound and retired, the default, and greens joining the lineage.
  Names,
  /// Mercurial's phase roots.
  Phases,
  /// Mercurial's obsolescence markers.
  Markers,
  /// Git LFS's locks.
  Locks,
}

/// One of a repository's logs (A-642): a green's chain, or one of the repository's own state's.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogName {
  /// A green's log: its chain of versions, positions its versions.
  Green {
    /// The green.
    green: VolumeId,
  },
  /// One of a repository's state logs.
  State {
    /// The repository.
    repository: RepositoryId,
    /// The log.
    log: StateLog,
  },
}

/// What one record moved of one log (A-642): from the position it found the log at to the one it left it at.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogSpan {
  /// The log.
  pub log: LogName,
  /// The position the record found it at: its predecessor.
  pub from: u64,
  /// The position it left it at.
  pub to: u64,
}

/// An atomic record (A-642): one entry that moved two logs or more, a green's among them, committed whole or not at all.
/// Each position it took on each log points to it, and a holder applies it only once every log it moves is at the
/// position it moved it from (its join); a cut of positions holds all of it or none of it.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct AtomicRecord {
  /// What it moved of each log, in log order.
  pub spans: Vec<LogSpan>,
}

/// A position of a green's log (A-642): a version of the green.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct GreenPosition {
  /// The green.
  pub green: VolumeId,
  /// The version.
  pub version: u64,
}

/// An atomic record as a partition snapshot carries it (A-642): the record and the positions of greens' logs that point
/// to it still, those its greens' checkpoints and destroys have not folded.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct AtomicHeld {
  /// The record.
  pub record: AtomicRecord,
  /// The positions pointing to it.
  pub pointers: Vec<GreenPosition>,
}

/// Where a holder's logs stand against an atomic record (A-642).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Join {
  /// Some log it moves has not reached its predecessor yet: the holder waits.
  Waiting,
  /// Every log it moves is at its predecessor: the holder applies it now.
  Ready,
  /// Every log it moves holds it already.
  Applied,
  /// Its logs disagree, some holding it and some not: a holder never comes to this, so it is refused loudly.
  Diverged,
}

impl AtomicRecord {
  /// Where logs at the positions `at` gives stand against the record (A-642's join).
  pub fn join(&self, at: impl Fn(&LogName) -> u64) -> Join {
    let (mut before, mut ready, mut held) = (false, false, false);
    for span in &self.spans {
      let position = at(&span.log);
      if position >= span.to {
        held = true;
      } else if position == span.from {
        ready = true;
      } else if position < span.from {
        before = true;
      } else {
        // Inside a span of several positions: part of the record held on this log.
        return Join::Diverged;
      }
    }
    match (before, ready, held) {
      (false, false, true) => Join::Applied,
      (_, _, true) => Join::Diverged,
      (true, _, false) => Join::Waiting,
      (false, _, false) => Join::Ready,
    }
  }

  /// Whether a cut, the logs it names at the positions `at` gives (`None` for a log it does not name), holds part of the
  /// record and not the rest: a log inside a span, or one log past its span beside another before its own.
  pub fn split_by(&self, at: impl Fn(&LogName) -> Option<u64>) -> bool {
    spans_split_by(&self.spans, at)
  }
}

/// Whether a cut, the logs it names at the positions `at` gives (`None` for a log it does not name), holds part of a
/// record's spans and not the rest (A-642): [`AtomicRecord::split_by`]'s rule, which the fleet's replicas judge every
/// record's spans by (A-636), a record of one log's included.
pub fn spans_split_by(spans: &[LogSpan], at: impl Fn(&LogName) -> Option<u64>) -> bool {
  let mut side = None;
  for span in spans {
    let Some(position) = at(&span.log) else {
      continue;
    };
    let held = if position >= span.to {
      true
    } else if position <= span.from {
      false
    } else {
      return true;
    };
    match side {
      None => side = Some(held),
      Some(seen) if seen != held => return true,
      Some(_) => {}
    }
  }
  false
}

/// A Git LFS lock as a repository's lock log holds it (A-642): the locked path and the lock's record, its bytes the LFS
/// server's (id, owner, time).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct LockRecord {
  /// The locked path.
  pub path: String,
  /// The lock.
  pub lock: Vec<u8>,
}

/// A name and the green it binds, as a [`RepositoryCheck::Names`] lists them (A-603).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct NameBinding {
  /// The name.
  pub name: String,
  /// The green it binds.
  pub green: VolumeId,
}

/// A green's next version as an entry of its repository's register commits it (A-603): the bytes are the green's chain
/// entry, the merge crate's encoding, held opaquely as `Op::GreenAdvanced` holds them, so a recovery replays them into
/// the green as it replays every other version.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct GreenVersion {
  /// The green: a member of the entry's repository.
  pub green: VolumeId,
  /// The version: the one after the green's head, or after its version earlier in the same entry.
  pub version: u64,
  /// The chain entry.
  pub entry: Vec<u8>,
}

/// A change of a repository's namespace as an entry of its register carries it (§4.17.1 item 3, A-603): creating a
/// branch, deleting one and moving the default are entries of the register, so two clients creating one name race on
/// one append and one of them is refused.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum NameChange {
  /// `name` binds `green`, a member of the repository that no name binds, under a name the repository does not hold.
  Bind {
    /// The name.
    name: String,
    /// The green.
    green: VolumeId,
  },
  /// `name` leaves the namespace; its green stays a member, so binding the name to it again restores the branch.
  Retire {
    /// The name.
    name: String,
  },
  /// The default branch becomes `branch`, bound or not yet (an unborn default), or none.
  Default {
    /// The branch's name.
    branch: Option<String>,
  },
}

/// A change of a repository's own state, carried beside its greens' versions and namespace changes (§4.17.1 shared
/// piece 2; §4.17.3 decisions 3 and 4; A-603): moved in the same step as the push that moves it, or alone. Each is made
/// against the state at the entry's position and refused if that state is not the one it was made against, so a replay
/// applies exactly what the owner judged.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum StateChange {
  /// `node` becomes a root of phase `phase` (Mercurial's phase roots, kept as Mercurial keeps them): refused when it is
  /// one already.
  PhaseRootAdded {
    /// The phase, as the tool numbers it.
    phase: u8,
    /// The node.
    node: Vec<u8>,
  },
  /// `node` stops being a root of phase `phase`: refused when it is not one.
  PhaseRootRemoved {
    /// The phase.
    phase: u8,
    /// The node.
    node: Vec<u8>,
  },
  /// An obsolescence marker, in its encoded form, kept for good in the order the register received it (Mercurial keeps
  /// every marker it receives): refused when the repository holds it already.
  MarkerAdded {
    /// The marker's bytes.
    marker: Vec<u8>,
  },
  /// `path` is locked by `lock` (Git LFS's locks, A-642): refused when the path is locked already.
  LockTaken {
    /// The path.
    path: String,
    /// The lock's record.
    lock: Vec<u8>,
  },
  /// `path`'s lock `lock` is released: refused unless the path is locked by exactly that lock.
  LockReleased {
    /// The path.
    path: String,
    /// The lock released.
    lock: Vec<u8>,
  },
}

/// One entry of a repository's logs (§4.17.1 item 2, shared piece 2; A-603 rebuilt by A-642): what one step commits of
/// a repository, all of it or none. Each green it moves keeps its own log, and the entry appends to it only while the
/// green is at the position the push read; the repository's own state moves in logs of its own; there is no position
/// of the whole repository. Its checks are read first; then its members join, its greens' versions are committed, its
/// names change and its state moves, each item judged against what the items before it left, so git's atomic push
/// ("Either all refs are updated or none") and a Mercurial push that adds changesets, moves bookmarks, phases and
/// markers are each one durable step. An entry that moves two logs or more, a green's among them, is an atomic record.
#[derive(Wire, Clone, Debug, Default, PartialEq, Eq)]
pub struct RepositoryEntry {
  /// The repository.
  pub repository: RepositoryId,
  /// What must hold before the entry's own changes, each against its log's position.
  pub checks: Vec<RepositoryCheck>,
  /// Greens joining the repository's lineage, in order.
  pub joined: Vec<MemberRecord>,
  /// The greens' versions the entry commits, in order: each appended only while its green is at the position before
  /// it, the position the push read.
  pub versions: Vec<GreenVersion>,
  /// The namespace's changes, in order.
  pub names: Vec<NameChange>,
  /// The repository's own state's changes, in order.
  pub state: Vec<StateChange>,
  /// The objects the push brought to the repository's tool store (§4.17.1 "Durability and residency"; A-662): the
  /// content identities of the objects its quarantine sealed and the repository did not hold, in the order they were
  /// admitted, with their bytes and the moves the store committed them with, encoded by the repository's tool and
  /// opaque here. The register holds them as a version's chain entry holds its work's post-state, for as long as the
  /// repository lives, so a recovery replays them through the store's own admission and commit. Empty when the entry
  /// brought none.
  pub objects: Vec<u8>,
}

/// A phase root of a repository (§4.17.3 decision 3, A-603): `node` is a root of phase `phase`.
#[derive(Wire, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PhaseRoot {
  /// The phase, as the tool numbers it.
  pub phase: u8,
  /// The node.
  pub node: Vec<u8>,
}

/// A repository's own state as a partition snapshot carries it (A-603, A-642): each state log's position and what it
/// holds. The markers log's position is the markers it holds.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct RegisterRecord {
  /// The repository.
  pub repository: RepositoryId,
  /// The names log's position.
  pub names_position: u64,
  /// The phases log's position.
  pub phases_position: u64,
  /// The locks log's position.
  pub locks_position: u64,
  /// The phase roots, in (phase, node) order.
  pub phase_roots: Vec<PhaseRoot>,
  /// The obsolescence markers, in the order the log received them.
  pub markers: Vec<Vec<u8>>,
  /// The locks, in path order.
  pub locks: Vec<LockRecord>,
  /// Each entry's objects (`RepositoryEntry::objects`, A-662), in the order the register received them.
  pub objects: Vec<Vec<u8>>,
}
