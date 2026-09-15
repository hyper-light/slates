//! The ring protocol's bodies (§4.4's lifecycle verbs as the client sends them, §4.9's rules:
//! a schema hash in front of every body, a closed refusal taxonomy) and their framing into
//! slots: a body that fits the slot's payload travels inline; a larger one travels through the
//! client's bulk area, in the chunk that belongs to the slot's ring index, and the slot names
//! it by offset and length. Both ends share this module, so the daemon and the Rust client
//! (and, through it, the SDKs and the CLI) speak one thing.

use slates_wire::Wire;

use crate::error::IpcError;
use crate::region::ClientRegion;
use crate::slot::{PAYLOAD_BYTES, Slot, SlotKind};

/// A volume id on the wire.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq, Default, Hash, PartialOrd, Ord)]
pub struct VolumeId {
  /// The bytes.
  pub bytes: [u8; 16],
}

/// A snapshot id on the wire.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct SnapshotId {
  /// The value.
  pub value: u64,
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
  /// Folded names, as APFS.
  Fold,
}

/// A landing filter (§4.15 step 1): path prefixes to keep, and to leave out.
#[derive(Wire, Clone, Debug, PartialEq, Eq, Default)]
pub struct Filter {
  /// Path prefixes to keep (empty keeps all).
  pub include: Vec<String>,
  /// Path prefixes to leave out.
  pub exclude: Vec<String>,
}

/// A principal on the wire (§4.13), as a `share` names one: the host account, or an enrolled consumer
/// under one. The daemon maps it to the catalog's principal (`verbs::to_db_principal`); a certificate
/// or SID principal is never named by a client, so neither is on the wire.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum Principal {
  /// A Unix user.
  Uid {
    /// The uid.
    uid: u32,
  },
  /// An enrolled consumer under a host account.
  Consumer {
    /// The account.
    account: u32,
    /// The consumer id.
    consumer: u64,
  },
}

/// Rights on a volume (§4.13 "Access lists"), as a `share` grants them.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rights {
  /// Attach for reading, snapshot reads, status, read_base, versions, export.
  pub read: bool,
  /// Attach for writing, the mutating verbs, snapshot, clone, submit, rebase, pin, rewitness, land.
  pub write: bool,
  /// Resize, destroy, archive, changing the list, revoking leases.
  pub admin: bool,
}

/// A grant's scope (§4.15 step 3): one landing of the bound manifest, or every landing of the
/// volume into the target for the session.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantScope {
  /// A single landing of exactly the manifest the human saw.
  Once,
  /// Every landing of the volume into the target for the session (each still presents its
  /// manifest and still refuses on a conflict).
  Session,
}

/// The durability scope of `await placed` (§4.8 D-18): the owner's region, or the mirror.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
  /// The home region: `f + 1` of the owner's candidates (the local append at `f = 0`).
  Region,
  /// The mirror region (absent at `f = 0`, refused `Unsupported`).
  Mirror,
}

/// What a client attaches for.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intent {
  /// Reads only.
  Read,
  /// Reads and writes: takes the volume's lease.
  Write,
}

/// The complete immutable base a green starts from (§4.16, the A-9 integration requirement: "Green's
/// immutable version chain starts from scratch or a complete immutable base, never an implicitly live
/// host directory"): a snapshot of a volume. The daemon refuses `ConsistentBaseUnavailable` unless the
/// snapshot covers its whole logical tree — a scratch volume's, or an overlay's whose base entries are
/// all witnessed and pinned — so no green version ever depends on a live host directory.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct GreenBase {
  /// The volume the snapshot belongs to.
  pub volume: VolumeId,
  /// The snapshot: the green's version 0.
  pub snapshot: SnapshotId,
}

/// Which view of a volume a [`RequestBody::Read`] reads (§4.16 "Attachments and versions"; §4.12
/// `slates.fs.read`).
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadAt {
  /// The volume's head (a green's head version; a work's or a plain volume's live tree).
  Head,
  /// A green's named version.
  Version {
    /// The version.
    version: u64,
  },
  /// The version an attachment pins — a view that moves only by `advance`.
  Attachment {
    /// The attachment.
    attachment: u64,
  },
}

/// A request body.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum RequestBody {
  /// Create a volume.
  Create {
    /// The name.
    name: String,
    /// The size class.
    size: SizeClass,
    /// The name policy.
    names: NamePolicy,
    /// Whether unlocked memory refuses.
    require_locked: bool,
    /// A host directory to overlay, or nothing for a scratch volume.
    base: Option<String>,
  },
  /// Take a snapshot.
  Snapshot {
    /// The volume.
    volume: VolumeId,
  },
  /// Destroy a snapshot: its retained inode versions return to the shard's version budget; refused
  /// if a clone still pins it.
  DestroySnapshot {
    /// The volume.
    volume: VolumeId,
    /// The snapshot.
    snapshot: SnapshotId,
  },
  /// Create a green volume — a shared merge target many work volumes submit increments into (§4.16).
  /// Its chain starts from scratch, or from a complete immutable base (`base`), whose state becomes
  /// version 0; served on the base volume's owner shard when a base is named.
  CreateGreen {
    /// The name.
    name: String,
    /// Whether an increment must carry evidence to be accepted.
    require_evidence: bool,
    /// The complete immutable base, or none for a green that starts empty.
    base: Option<GreenBase>,
  },
  /// A green's version chain: its head version (and, later, the records from `from`).
  Versions {
    /// The green.
    green: VolumeId,
  },
  /// The files a green changed strictly after `version` (§4.16): what a lagging work reconciles.
  ChangedSince {
    /// The green.
    green: VolumeId,
    /// The base version to compare against.
    version: u64,
  },
  /// Create a work volume over a green (§4.16): an agent's private clone to declare operations on.
  CreateWork {
    /// The green to work over.
    green: VolumeId,
    /// The name.
    name: String,
  },
  /// Declare an edit on a work volume (§4.16): at `path`, remove `delete_len` bytes at `at` and
  /// insert `bytes` — a true splice, not a whole-file rewrite. A new path is created.
  Edit {
    /// The work volume.
    work: VolumeId,
    /// The file.
    path: String,
    /// The offset.
    at: u64,
    /// Bytes removed at `at`.
    delete_len: u64,
    /// Bytes inserted at `at`.
    bytes: Vec<u8>,
  },
  /// Submit a work volume's declared operations to its green as an increment (§4.16). The evidence
  /// references are opaque BLAKE3 identities retained with the increment; a green created with
  /// `require_evidence` refuses `EvidenceRequired` when none is given.
  Submit {
    /// The work volume.
    work: VolumeId,
    /// The evidence references (opaque to slates), possibly none.
    evidence: Vec<[u8; 32]>,
  },
  /// Declare a namespace or metadata operation on a work volume (§4.16): the counterpart to `Edit`'s
  /// content splice, for links, directories, modes and extended attributes.
  Declare {
    /// The work volume.
    work: VolumeId,
    /// The operation.
    op: WorkOp,
  },
  /// Rebase a work volume onto its green's head, the only corrective path (§4.16): map its pending
  /// operations forward, moving the work's base without committing to the green.
  Rebase {
    /// The work volume.
    work: VolumeId,
  },
  /// Clone a snapshot into a new volume.
  Clone {
    /// The volume.
    volume: VolumeId,
    /// The snapshot.
    snapshot: SnapshotId,
    /// The clone's name.
    name: String,
  },
  /// Attach (the client form: the reply names the path a bridge will publish).
  Attach {
    /// The volume.
    volume: VolumeId,
    /// A snapshot, for a read-only attachment to it.
    snapshot: Option<SnapshotId>,
    /// The intent.
    intent: Intent,
    /// The form (§4.4 `attach(…, transport, chosen_path?)`; §4.6 A-9).
    form: AttachRequest,
  },
  /// Detach.
  Detach {
    /// The attachment.
    attachment: u64,
  },
  /// Resize.
  Resize {
    /// The volume.
    volume: VolumeId,
    /// The new size class.
    size: SizeClass,
  },
  /// Destroy.
  Destroy {
    /// The volume.
    volume: VolumeId,
  },
  /// Status.
  Status {
    /// The volume.
    volume: VolumeId,
  },
  /// List the caller's volumes.
  List,
  /// Acknowledge completions up to a sequence (releases their records).
  Acknowledge {
    /// Every sequence up to and including this one.
    up_to: u32,
  },
  /// The base entry as the disk holds it now (§4.4 `read_base`).
  ReadBase {
    /// The volume.
    volume: VolumeId,
    /// The path.
    path: String,
  },
  /// Re-witness drifted entries (§4.4 `rewitness`).
  Rewitness {
    /// The volume.
    volume: VolumeId,
    /// The paths, or every drifted entry.
    paths: Option<Vec<String>>,
  },
  /// Pin base subtrees (§4.4 `pin`).
  Pin {
    /// The volume.
    volume: VolumeId,
    /// The paths, or the whole base.
    paths: Option<Vec<String>>,
  },
  /// A grant for a presented landing (§4.15 step 3, §4.13 "Grants"): carries the human surface's
  /// **proof of issuer authority** — `BLAKE3_keyed(issuer_secret, landing ‖ manifest ‖ scope ‖ term)` —
  /// which the daemon recomputes against the secret it minted into the anchor segment. Without a
  /// verifying proof the grant refuses `GrantIssuerUnverified` (AC-2.8: the kind arriving from an agent
  /// channel — the MCP server and the SDKs carry no proof by construction — is refused and counted);
  /// with one, the landing's grant is issued bound to that exact manifest.
  Grant {
    /// The presented landing the grant covers.
    landing: u64,
    /// The manifest hash the human approved — must equal the presented landing's, or the plan changed.
    manifest: [u8; 32],
    /// Once, or the session.
    scope: GrantScope,
    /// The grant's validity, nanoseconds from issue.
    term_ns: u64,
    /// The proof of issuer authority.
    proof: [u8; 32],
  },
  /// Enroll a consumer under a host account (§4.13 "Principals"): the human surface, proving issuer
  /// authority as for a grant, mints a consumer id and the secret capability the workload will bind its
  /// channel with. The secret is returned once, to the human, who delivers it to the workload through the
  /// trusted harness — never over a channel other agents share.
  Enroll {
    /// The host account (uid) the consumer runs under.
    account: u32,
    /// The proof of issuer authority: `BLAKE3_keyed(issuer_secret, "enroll" ‖ account)`.
    proof: [u8; 32],
  },
  /// Bind this channel to an enrolled consumer (§4.13: "a consumer channel is bound at rendezvous using a
  /// capability delivered and retained outside other agents' reach"): the workload's first verb. The
  /// proof is `BLAKE3_keyed(consumer_secret, client_id)`, so a proof captured from another session does
  /// not bind this one. Refused `ConsumerNotEnrolled` (no such consumer, or the proof does not verify) or
  /// `ConsumerRevoked`; after it, the channel's principal is the consumer and every right is checked
  /// against it.
  Attest {
    /// The consumer.
    consumer: u64,
    /// The proof of the capability.
    proof: [u8; 32],
  },
  /// Revoke a consumer's enrollment (§4.13): the human surface, proving issuer authority; every later
  /// effect from a channel bound to it refuses `ConsumerRevoked`.
  Revoke {
    /// The consumer.
    consumer: u64,
    /// The proof of issuer authority: `BLAKE3_keyed(issuer_secret, "revoke" ‖ consumer)`.
    proof: [u8; 32],
  },
  /// Set a principal's rights on a volume (§4.13 "Access lists": `admin` covers changing the list; the
  /// owner holds every right). Rights all false remove the entry.
  Share {
    /// The volume.
    volume: VolumeId,
    /// The principal given (or denied) rights.
    principal: Principal,
    /// The rights.
    rights: Rights,
  },
  /// The daemon's own status (§4.14 `slates.status`: every shard's counters and health
  /// signals, and the anchor's view of the daemon as the segment holds it).
  DaemonStatus,
  /// Promote a lost region's declared mirror (§4.8, D-14 — region-loss promotion at operator cadence): the
  /// operator, having judged the region truly lost, proposes `PromoteRegion` on the root group so the lost
  /// region's volumes re-home to the mirror. Issued on the root leader; a follower refuses `NotRootLeader`,
  /// a region with no declared mirror refuses `Unsupported`. Operator-initiated, never automatic, so a merely
  /// partitioned region is not failed over into a second owner.
  PromoteRegion {
    /// The lost region's id.
    region: u64,
  },
  /// Await a durability scope for a volume's head, or a snapshot (§4.8 D-18): returns when the
  /// scope is placed. At `f = 0` the region is the local append (already placed) and the
  /// mirror is refused `Unsupported`.
  AwaitPlaced {
    /// The volume.
    volume: VolumeId,
    /// A snapshot, or the head when none.
    snapshot: Option<SnapshotId>,
    /// The scope.
    scope: Scope,
  },
  /// Land a snapshot's diverged entries onto a host directory (§4.15). Without a grant the
  /// reply is `GrantRequired` with the manifest and its summary; with a grant that binds the
  /// manifest the landing runs and the reply is `Landed`. The grant itself is never created
  /// on this channel (§4.13, R10): it comes on the control channel through `slates grant`.
  Land {
    /// The volume.
    volume: VolumeId,
    /// A snapshot, or the head when none.
    snapshot: Option<SnapshotId>,
    /// The host directory to write into.
    target: String,
    /// The filter over the diverged entries.
    filter: Filter,
    /// A grant the caller already holds (from `slates grant`), or none to be presented.
    grant: Option<u64>,
  },
  /// The caller's grants (a read; creating a grant is off-ring).
  Grants,
  /// The audit log from a sequence (a read).
  Audit {
    /// Every record at or after this sequence.
    since: u64,
  },
  /// Drain one shard's telemetry ring (§4.14): the chokepoint spans it holds, up to the bound one reply
  /// carries, with the loss marker for the batch and the per-chokepoint freshness. A read that consumes
  /// (the ring is a queue); its completion is recorded like any verb, so a retry returns the same batch.
  Telemetry {
    /// The shard's partition, as `DaemonStatus` reports it.
    partition: u16,
  },
  /// The verified content digest of a clean base file (§4.15 `digest`): the BLAKE3 of the bytes
  /// the disk holds for an untouched entry, verified current at the export; an entry the volume
  /// diverged refuses `DigestNotClean`, a file changing under the hash `DigestUnverified`. A read.
  Digest {
    /// The volume.
    volume: VolumeId,
    /// The path.
    path: String,
  },
  /// Re-pin a green attachment to `version`, or to the head when none (§4.16 "Attachments and
  /// versions": "no attachment's view changes without `advance`"). The reply names the paths the
  /// move invalidates — exactly those some version in the span changed. Served on the attachment's
  /// owner shard. Appended at the end for append-only evolution.
  Advance {
    /// The attachment.
    attachment: u64,
    /// The version to pin, or the head.
    version: Option<u64>,
  },
  /// Read a file's bytes from a volume at a view (§4.12 `slates.fs.read`): a green's head or a named
  /// version, the version an attachment pins, or a work's or plain volume's live tree. A read; never
  /// recorded. Appended at the end for append-only evolution.
  Read {
    /// The volume.
    volume: VolumeId,
    /// The file.
    path: String,
    /// The view.
    at: ReadAt,
  },
  /// Explicitly create a configuration group through an authenticated local account (AUD-07).
  /// Binding the request to this start prevents a client retry from resetting a replacement.
  Bootstrap {
    /// Also create the root group, on the first node of the first region only.
    root: bool,
    /// The fresh member id read from this daemon's status before the explicit request.
    member: u64,
  },
  /// Inspect this member's retained state before an explicit quorum-loss recovery (§4.8).
  RecoveryPlan {
    /// Root or regional group.
    root: bool,
    /// Join this explicitly named replacement group instead of creating its first voter.
    target: Option<[u8; 32]>,
  },
  /// Authorize exactly the reviewed recovery plan using the anchor's human capability.
  Recover {
    /// Root or regional group.
    root: bool,
    /// The replacement group to join, or none to recover this copy as its first voter.
    target: Option<[u8; 32]>,
    /// Digest returned by `RecoveryPlan`.
    plan: [u8; 32],
    /// The human surface's capability proof over the reviewed plan.
    proof: [u8; 32],
  },
}

/// A concrete quorum-loss recovery proposal (§4.8). It identifies the retained copy and the
/// old voters the operator must fence; it cannot prove that an unreachable copy holds no newer data.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct ConsensusRecoveryPlan {
  /// The member performing this operation.
  pub member: u64,
  /// The old consensus group.
  pub previous: [u8; 32],
  /// This exact proposal, bound to the daemon start and retained state.
  pub digest: [u8; 32],
  /// The last locally known committed Raft position.
  pub committed: u64,
  /// The retained log's last position, including its uncommitted tail.
  pub last_log: u64,
  /// The last locally known committed application version, including a learner's fetched view.
  pub version: u64,
  /// The old configuration's voters, including both sides of an unfinished joint change.
  pub voters: Vec<u64>,
  /// Explicitly approved destination, or none when creating the replacement group.
  pub target: Option<[u8; 32]>,
}

/// What a signal's absence means (§4.14, A-9): a missing sample is never silently read as "healthy".
/// One vocabulary for the health signals here and the chokepoint spans in `slates-wire`.
pub use slates_wire::observe::AbsenceIs;
/// Who observes a signal (§4.14): shared with the chokepoint registry.
pub use slates_wire::observe::Observer;

/// What a health signal's `freshness_ns` is measured from (§4.14 "freshness"): the registry states it,
/// so the report cannot age one signal by a rule it invented.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FreshnessBasis {
  /// Computed when the report is built: age zero by construction.
  AtReport,
  /// A boot-time fact (the last replay's duration): its age is the time since boot.
  SinceBoot,
}

impl FreshnessBasis {
  /// The basis's name in the registry table.
  pub const fn name(self) -> &'static str {
    match self {
      FreshnessBasis::AtReport => "measured at report (age 0)",
      FreshnessBasis::SinceBoot => "measured at boot (age = time since boot)",
    }
  }
}

/// A health signal (§4.14): a value, whether it is present, what its absence would mean, and how old
/// it is. The value is optional so an absent sample is never conflated with a real numeric zero (A-9:
/// "values may be absent and must remain distinguishable from numeric zero").
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct Signal {
  /// The name, dotted (`catalog.volumes`).
  pub name: String,
  /// The value, or `None` when the signal is absent (not measured / not reporting) — never conflated
  /// with a real zero (§4.14, A-9).
  pub value: Option<u64>,
  /// What a `None` value means for this signal, so absence is never read as "healthy" (§4.14, A-9).
  pub absence: AbsenceIs,
  /// How old the value is, in nanoseconds (zero for one computed now).
  pub freshness_ns: u64,
}

/// The closed registry of shard health signals (§4.14, D-23; GAP-A9-12). Every signal a shard reports
/// is a variant here, so the set cannot drift — a free-string name can be miscounted ("nine spans were
/// called seven"); an enum cannot. The report is built by mapping [`HealthSignal::ALL`], so a variant
/// added without a measurement is a compile error and a variant that is never emitted cannot exist.
/// The wire keeps [`Signal`]'s string name (`name()`), so no consumer changes; this is the producer's
/// closed vocabulary. Signals are content-free counts and ages only (D-23), never payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HealthSignal {
  /// Volumes in this shard's catalog.
  CatalogVolumes,
  /// Nanoseconds the recovered log took to replay at boot (its freshness is the time since boot).
  LogReplayNs,
  /// Leases within the failover SLO of expiring.
  LeaseExpiring,
  /// The summed depth of the client command rings on this shard.
  RingDepth,
  /// Live clients on this shard.
  ShardClients,
  /// Operations deferred on this shard.
  ShardDeferred,
}

impl HealthSignal {
  /// The closed registry: every shard health signal, in the order the report emits them. A doc-truth
  /// test pins this set and its names so a rename or an addition is caught, not silently miscounted.
  pub const ALL: [HealthSignal; 6] = [
    HealthSignal::CatalogVolumes,
    HealthSignal::LogReplayNs,
    HealthSignal::LeaseExpiring,
    HealthSignal::RingDepth,
    HealthSignal::ShardClients,
    HealthSignal::ShardDeferred,
  ];

  /// The dotted name this signal reports under (the stable wire vocabulary a consumer keys on).
  pub const fn name(self) -> &'static str {
    match self {
      HealthSignal::CatalogVolumes => "catalog.volumes",
      HealthSignal::LogReplayNs => "log.replay_ns",
      HealthSignal::LeaseExpiring => "lease.expiring",
      HealthSignal::RingDepth => "ring.depth",
      HealthSignal::ShardClients => "shard.clients",
      HealthSignal::ShardDeferred => "shard.deferred",
    }
  }

  /// What this signal's absence means (§4.14, A-9). `log.replay_ns` is `Unknown` until a replay
  /// happens (a fresh daemon that replayed nothing has no replay time — distinct from a 0 ns replay).
  /// The rest are always-computable counts on a live shard, so an absent one is a `Degraded` producer,
  /// never a healthy zero.
  pub const fn absence(self) -> AbsenceIs {
    match self {
      HealthSignal::LogReplayNs => AbsenceIs::Unknown,
      HealthSignal::CatalogVolumes
      | HealthSignal::LeaseExpiring
      | HealthSignal::RingDepth
      | HealthSignal::ShardClients
      | HealthSignal::ShardDeferred => AbsenceIs::Degraded,
    }
  }

  /// What this signal's `freshness_ns` is measured from (§4.14): the report ages each signal by the
  /// registry's rule, not one of its own.
  pub const fn freshness(self) -> FreshnessBasis {
    match self {
      HealthSignal::LogReplayNs => FreshnessBasis::SinceBoot,
      HealthSignal::CatalogVolumes
      | HealthSignal::LeaseExpiring
      | HealthSignal::RingDepth
      | HealthSignal::ShardClients
      | HealthSignal::ShardDeferred => FreshnessBasis::AtReport,
    }
  }

  /// What produces this signal's value (§4.14 "expected producer"): the shard structure it is read
  /// from. Every one is the owning shard's own accounting, so a live shard always has it.
  pub const fn producer(self) -> &'static str {
    match self {
      HealthSignal::CatalogVolumes => "the shard's volume catalog",
      HealthSignal::LogReplayNs => "the last recovery replay",
      HealthSignal::LeaseExpiring => "the shard's lease table against the failover SLO",
      HealthSignal::RingDepth => "the clients' command rings, summed",
      HealthSignal::ShardClients => "the shard's client slots",
      HealthSignal::ShardDeferred => "the shard's deferred-reply queue",
    }
  }

  /// Who observes this signal (§4.14 "observer"): every shard signal is self-observed by the shard
  /// that reports it. The host-observed signals of the catalog (the anchor's view of the daemon) are the
  /// `DaemonReport`'s own words, not registry entries yet.
  pub const fn observer(self) -> Observer {
    match self {
      HealthSignal::CatalogVolumes
      | HealthSignal::LogReplayNs
      | HealthSignal::LeaseExpiring
      | HealthSignal::RingDepth
      | HealthSignal::ShardClients
      | HealthSignal::ShardDeferred => Observer::OwnerShard,
    }
  }

  /// The registry as the Markdown table `docs/wip/observability.md` carries — one row per signal in
  /// report order, every column a registry method — so the document is generated from the code and the
  /// doc-truth test fails the moment either drifts.
  pub fn registry_table() -> String {
    let mut table = String::from(
      "| Signal | Absence means | Freshness | Producer | Observer |\n|---|---|---|---|---|\n",
    );
    for signal in HealthSignal::ALL {
      table.push_str(&format!(
        "| `{}` | {} | {} | {} | {} |\n",
        signal.name(),
        signal.absence().name(),
        signal.freshness().name(),
        signal.producer(),
        signal.observer().name(),
      ));
    }
    table
  }
}

/// A refusal kind's count.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct RefusalCount {
  /// The kind's name.
  pub kind: String,
  /// Refusals of it.
  pub count: u64,
}

/// One consensus group's state on a node, as `status` reports it (§4.8 "Configuration, by consensus";
/// "Derived constants": *"election timeout for the configuration group ≥ 10 × broadcast RTT p99 with the
/// randomization span from RTT variance"*): whether this node currently leads it, and the election timing
/// it last derived — the base and span in coordinator periods, the measured round-trip tail and spread
/// they came from, and the samples behind them, so an observer tells a measured timing from the floor it
/// would default to. The regional council and the root group each report one.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct GroupReport {
  /// Whether this node believes itself the group's elected leader.
  pub leads: bool,
  /// The base election timeout in coordinator periods.
  pub base_periods: u32,
  /// The randomization span in coordinator periods.
  pub span_periods: u32,
  /// The broadcast round-trip tail the base was derived from (nanoseconds); zero before any sample.
  pub rtt_tail_ns: u64,
  /// The round-trip variation the span was derived from (nanoseconds); zero before any sample.
  pub rtt_spread_ns: u64,
  /// The round trips measured across the voter paths that fed the derivation.
  pub samples: u64,
}

/// One shard's part of the daemon's status.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct ShardReport {
  /// The partition.
  pub partition: u16,
  /// Live clients.
  pub clients: u32,
  /// Volumes owned.
  pub volumes: u64,
  /// Requests served (completions recorded).
  pub served: u64,
  /// Refusals by kind.
  pub refusals: Vec<RefusalCount>,
  /// Records replayed at the last start.
  pub replayed_records: u64,
  /// The last replay's duration.
  pub replay_ns: u64,
  /// Whether the last replay cut a torn tail.
  pub torn_tail: bool,
  /// The shard's reserve.
  pub reserve_bytes: u64,
  /// Bytes committed to bounded volumes.
  pub committed_bytes: u64,
  /// The shard's inode-version slab capacity (§4.2 inode dimension).
  pub version_slots: u64,
  /// Version slots committed to volumes' reserved inode allowances.
  pub committed_versions: u64,
  /// The health signals.
  pub signals: Vec<Signal>,
  /// Chokepoint spans this shard currently holds in its bounded telemetry sink (§4.14): the most
  /// recent, at most the sink's capacity — the shed-first ring keeps the newest.
  pub spans_held: u64,
  /// Chokepoint spans this shard has shed since boot because its bounded sink was full (§4.14
  /// "bounded rings report dropped spans"): the explicit telemetry-loss signal, never a silent gap.
  pub spans_dropped: u64,
  /// Fleet peers this shard has a formed probe session to (§4.8 "Membership"). The membership loop runs
  /// on the control shard, so only its part counts; the other shards form none. Zero on a laptop.
  pub peers_probed: u32,
  /// The part of `committed_bytes` that is snapshot-retained content (§4.2 retention, the byte
  /// dimension): arena blocks the heads have let go of that their snapshots still pin, charged from
  /// unpromised capacity. Distinguishes bytes held for snapshots from promised entitlement.
  pub retained_bytes: u64,
  /// The part of `committed_versions` that is snapshot-retained inode versions (§4.2 retention,
  /// the inode dimension).
  pub retained_versions: u64,
  /// The shard's metadata ledger (§4.2 metadata dimension): the metadata class less its slabs'
  /// maximum footprint — what volume records (journal budgets, volume objects) may take.
  pub metadata_bytes: u64,
  /// Metadata bytes reserved for the records of the volumes the shard owns.
  pub committed_metadata: u64,
  /// The bytes the shard's content arena maps — its address space — of which `reserve_bytes` is
  /// the usable (buddy-allocatable) part the budget admits against (§4.2 "segment, slab and buddy
  /// geometry report usable capacity, not mapping length": both, so the difference is visible).
  pub mapped_bytes: u64,
  /// Whether this is the control shard — the one that runs the membership loop, drives the consensus
  /// groups and holds their live timing; the other shards hold inert copies.
  pub control: bool,
  /// The regional configuration council as this shard holds it (live on the control shard).
  pub council: GroupReport,
  /// The root group across regions as this shard holds it (live on the control shard).
  pub root: GroupReport,
  /// Task admissions this shard's runtime arena refused since boot (§4.3 "a task exceeding the budget
  /// is a counted bug signal"; §4.14): the shard's derived task budget covers every task the daemon
  /// spawns on it — clients' cross-shard work, its own loops, the fleet's share — so a count here is a
  /// sizing defect surfacing, never expected load. A refused client admission is what left a fleet
  /// node unable to seat any client on 2026-09-14.
  pub tasks_refused: u64,
}

/// The daemon's place in its fleet (§4.8; §2.6 boot step 6), as the verbs' placement authority sees it.
/// A laptop reports itself: `f = 0`, one member, no peers probed — the same fields, the degenerate values
/// (R8).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct FleetReport {
  /// This node's member id — what its peers, a recorded holder set and its volume ids name it by.
  pub host: u64,
  /// The fault tolerance the node is configured for: a write commits at `f + 1` acknowledgements.
  pub f: u32,
  /// This node's current host epoch (its authority; 1 on a laptop, higher after a takeover promotion).
  pub host_epoch: u64,
  /// The members the membership view holds alive, this node among them.
  pub members: Vec<u64>,
  /// Peers with a formed probe session (the direct mesh), summed over the shards: the mesh is up when
  /// this reaches the member count less one.
  pub peers_probed: u32,
  /// Packets the node's serve sockets dropped because their connection id named no live session (a
  /// stale packet, a stray, or a session already closed), summed over the planes.
  pub unknown_id: u64,
  /// Datagrams the serve sockets dropped because a session's inbox was full, summed over the planes.
  pub inbox_full: u64,
  /// Handshakes from new dialers refused because every session slot was taken, summed over the planes.
  pub sessions_refused: u64,
  /// Sessions closed because their peer established a new one (a re-dial after a loss), summed over the
  /// planes.
  pub replaced: u64,
  /// The regional configuration council (§4.8, D-14) as the control shard drives it: whether this node
  /// leads it, and the election timing it derived from the measured voter paths.
  pub council: GroupReport,
  /// The root group across regions, likewise; the degenerate self-leading group in a single-region fleet.
  pub root: GroupReport,
}

/// The daemon's status.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct DaemonReport {
  /// The daemon's process id.
  pub pid: u32,
  /// The segment's generation (daemon starts over it).
  pub generation: u64,
  /// Restarts the anchor made.
  pub restarts: u64,
  /// The age of the daemon's last heartbeat in the segment.
  pub heartbeat_age_ns: u64,
  /// Clients found dead and reclaimed.
  pub clients_reaped: u64,
  /// Connects refused at the client bound.
  pub clients_refused: u64,
  /// Every shard's part, by partition.
  pub shards: Vec<ShardReport>,
  /// The daemon's place in its fleet.
  pub fleet: FleetReport,
}

/// One bounded drain of a shard's telemetry ring (§4.14): the operator-facing export of the chokepoint
/// spans. The ring is a queue — this batch is gone from the shard once reported — bounded to what one
/// reply carries (the shard's derived quota), so `remaining` says whether to drain again. Loss is never
/// silent: `shed_before` marks the spans the bounded ring shed between the previous drain and this one,
/// `dropped_total` the loss since boot, and `missing_links` the spans whose cause was not carried across
/// a boundary. `chokepoints` is the registry in roster order with each chokepoint's freshness judged
/// against `horizon_ns`, and `spans` names its chokepoint by roster index into it.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct TelemetryReport {
  /// The shard drained.
  pub partition: u16,
  /// The monotonic time of the drain on the shard's clock (the spans' `end_ns` are on the same clock).
  pub now_ns: u64,
  /// The time this batch covers: since the previous drain, or since boot for the first.
  pub window_ns: u64,
  /// The freshness horizon applied to every chokepoint (the failover SLO, §4.14): a newest span older
  /// than this is not reported as a live value.
  pub horizon_ns: u64,
  /// Spans shed by the bounded ring between the previous drain and this one — the typed loss marker for
  /// the batch, lost before its first span.
  pub shed_before: u64,
  /// Spans shed since boot.
  pub dropped_total: u64,
  /// Spans still held after this bounded drain; drain again to read them.
  pub remaining: u64,
  /// Spans in this batch whose cause existed but was not carried to the shard (`CauseRecord::Missing`).
  pub missing_links: u64,
  /// The chokepoint registry, in roster order, with this batch's per-chokepoint freshness.
  pub chokepoints: Vec<ChokepointReport>,
  /// The spans, oldest first.
  pub spans: Vec<SpanRecord>,
}

/// One chokepoint's entry in a [`TelemetryReport`] (§4.14): its registry definition and whether this
/// batch shows it live. `latest_age_ns` is the newest span's age when the batch holds one; `fresh` says
/// whether that age is within the horizon — when it is not, the value is typed absent (`absence` says
/// what that means) rather than reported as current, and `expected` says whether a producer of this
/// chokepoint runs on this host at all.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct ChokepointReport {
  /// The registry name (`shard.op`).
  pub name: String,
  /// The dimension name the span's `label` codes (`verb`), empty for none.
  pub dimension: String,
  /// Spans of this chokepoint in this batch.
  pub spans: u64,
  /// The newest span's age at the drain (`now_ns − end_ns`), when the batch holds one.
  pub latest_age_ns: Option<u64>,
  /// Whether the newest span is within the horizon — the value is current. False with a
  /// `latest_age_ns` is a stale last sighting, not a live value.
  pub fresh: bool,
  /// What "not fresh" means for this chokepoint (the registry's word).
  pub absence: AbsenceIs,
  /// The expected producer's name (the registry's word).
  pub producer: String,
  /// Whether that producer runs on this host (a laptop runs no replication; no host runs the archive
  /// codec yet), so an operator can tell "nothing here can produce it" from "idle".
  pub expected: bool,
}

/// One chokepoint span as exported (§4.14, the three-id law on the wire): the roster index of its
/// chokepoint, its content-free dimension code, the request identity (for replay), the trace (128 bits
/// as two words) and span identities, its cause, and its monotonic start and end.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct SpanRecord {
  /// The chokepoint's roster index (into [`TelemetryReport::chokepoints`]).
  pub point: u32,
  /// The content-free dimension code.
  pub label: u32,
  /// The request's client id.
  pub request_client: u32,
  /// The request's sequence within its client.
  pub request_sequence: u32,
  /// The trace id's high 64 bits.
  pub trace_high: u64,
  /// The trace id's low 64 bits.
  pub trace_low: u64,
  /// The span id.
  pub span: u64,
  /// What caused the span.
  pub cause: CauseRecord,
  /// The monotonic start (ns).
  pub start_ns: u64,
  /// The monotonic end (ns).
  pub end_ns: u64,
}

/// A span's cause on the wire (§4.14 "`caused_by`"): the form of `slates_wire::observe::Cause`.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum CauseRecord {
  /// The span opened its trace at an entry point.
  Root,
  /// Caused by this span of the same trace.
  Span {
    /// The causing span's id.
    id: u64,
  },
  /// A cause existed but was not carried across the boundary — the explicit missing link.
  Missing,
}

/// A volume's summary in a listing.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct VolumeSummary {
  /// The id.
  pub id: VolumeId,
  /// The name.
  pub name: String,
  /// Bytes referenced.
  pub referenced_bytes: u64,
  /// Bytes unique.
  pub unique_bytes: u64,
  /// Whether it is an overlay.
  pub overlay: bool,
}

/// A merge conflict window (§4.16): the file, the range in base coordinates that met an intervening
/// change, and the class of the conflict (the `MergeConflictClass` discriminant).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct MergeWindow {
  /// The file.
  pub path: String,
  /// The conflicting range's offset (base coordinates).
  pub at: u64,
  /// The conflicting range's length.
  pub len: u64,
  /// The conflict class, as its `MergeConflictClass` discriminant.
  pub class: u8,
}

/// A namespace or metadata operation a work volume declares (§4.16), the counterpart to the content
/// splice `Edit` carries. A mounted work would journal these from its filesystem operations; without
/// a mount, `Declare` records them directly so every dimension the merge engine composes — links,
/// directories, modes and extended attributes — can be exercised. Content edits stay with `Edit`.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum WorkOp {
  /// Remove the name at `path` (a file, symlink or hard link).
  Unlink {
    /// The path removed.
    path: String,
  },
  /// Rename `from` to `to`.
  Rename {
    /// The source path.
    from: String,
    /// The destination path.
    to: String,
  },
  /// Create an empty directory at `path`.
  Mkdir {
    /// The directory path.
    path: String,
  },
  /// Remove the empty directory at `path`.
  Rmdir {
    /// The directory path.
    path: String,
  },
  /// Set the mode of the file or directory at `path`.
  SetMode {
    /// The path.
    path: String,
    /// The new mode.
    mode: u32,
  },
  /// Create or retarget a symbolic link at `path` pointing at `target`.
  Symlink {
    /// The link's path.
    path: String,
    /// The link's target.
    target: String,
  },
  /// Create a hard link at `path` to the existing file `target`.
  Link {
    /// The new name.
    path: String,
    /// The existing file it links to.
    target: String,
  },
  /// Set the extended attribute `name` on `path` to `value`.
  SetXattr {
    /// The path.
    path: String,
    /// The attribute name.
    name: String,
    /// The attribute value.
    value: Vec<u8>,
  },
  /// Remove the extended attribute `name` from `path`.
  RemoveXattr {
    /// The path.
    path: String,
    /// The attribute name.
    name: String,
  },
}

/// A volume's placement (§4.8, D-18): every reply carries it from the first version, so the
/// fleet parts of Phase 8 change no interface. At `f = 0` a volume's head is placed the moment
/// the owner holds it and the mirror does not exist.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlacedState {
  /// Whether the head is placed in the region (`f + 1` regional acknowledgements).
  pub region: bool,
  /// The age of the newest record the mirror acknowledged, or nothing where no mirror exists.
  pub mirror_age_ns: Option<u64>,
  /// The owner's host epoch (its authority; 1 on a laptop).
  pub host_epoch: u64,
}

/// A volume's status (§4.4 `status`).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct StatusReport {
  /// The id.
  pub id: VolumeId,
  /// The name.
  pub name: String,
  /// Bytes referenced.
  pub referenced_bytes: u64,
  /// Bytes unique.
  pub unique_bytes: u64,
  /// The lease epoch, when held.
  pub lease_epoch: Option<u64>,
  /// Attachments.
  pub attachments: u32,
  /// The head snapshot.
  pub head: SnapshotId,
  /// Drifted entries, for an overlay.
  pub drifted: Vec<String>,
  /// The watcher state name, for an overlay.
  pub watcher: String,
  /// Snapshots held.
  pub snapshots: u32,
  /// The placement of the head (§4.8, D-18).
  pub placed: PlacedState,
  /// The daemon's NFS loopback port (§4.6), so a client can mount the volume with `mount_nfs
  /// localhost:PORT`; `None` when the daemon is not serving NFS (the listener could not bind).
  pub nfs_port: Option<u16>,
  /// Every transport this host offers or refuses for the volume, with the six facts of §4.6 A-9, as
  /// the caller's rights allow. Boxed: a cold report that would otherwise make every reply's move
  /// larger (the provisioning reply stays small, R9); the wire bytes are the report's own.
  pub transports: Box<TransportReport>,
}

/// The transports an attachment can take (§4.6 A-9 "supported transport"; RQ-20: host processes, OCI
/// containers and Linux guests consume the same VFS). `status` reports each with the six facts of
/// [`AttachmentCapability`], offered or refused with a typed reason; a request names its form through
/// [`AttachRequest`]. Append-only.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachTransport {
  /// The record form under the daemon's root mount (§4.4 `AttachForm::Root`): the SDK's attachment
  /// with its lease; the daemon establishes no path of its own for it.
  Root,
  /// The macOS host mount: the daemon's NFSv3 loopback export, mounted by the client with `mount_nfs`
  /// at an existing user-owned directory (`slates mount`) and no privilege (§4.6 "macOS fallback").
  NfsLoopback,
  /// The Linux host mount over `/dev/fuse` through `fusermount3` (§4.6 "Linux").
  Fuse,
  /// The macOS FSKit module (§4.6 "macOS 26+").
  Fskit,
  /// The Windows WinFsp volume on a drive letter (§4.6 "Windows").
  WinFsp,
  /// A bind of an established host mount into a container's mount namespace, performed by the host's
  /// OCI runtime (§4.6 A-9 "A host OCI runtime passes the established host attachment into the
  /// container mount namespace"); slates records the authorized binding and reports the `mounts` entry.
  Oci,
  /// The virtio-fs guest device over the in-process VMM seam (§4.6 A-9; `crates/bridge-virtiofs`):
  /// the harness runs the VM in the daemon's process and hands the seam in, the guest mounts a tag.
  VirtioFsInProcess,
  /// The virtio-fs guest device over inherited descriptors (vhost-user): the seam models it; the
  /// binding is not built.
  VirtioFsInheritedDescriptor,
}

/// Where a transport puts the volume (§4.6 A-9 "target-path constraints").
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetPathConstraint {
  /// Under the daemon's root mount, `<root>/<volume>`; the caller chooses nothing.
  RootMount,
  /// An existing directory the caller owns and names; never created by slates (AC-3.5).
  UserOwnedExistingDirectory,
  /// A destination inside the container's root filesystem, created there by the OCI runtime and bound
  /// from the host mount point; no host directory is created and no image is built (§4.6 A-9).
  ContainerDestination,
  /// A drive letter (an object-namespace junction), never a directory (§4.6 "Windows").
  DriveLetter,
  /// A tag the guest mounts (`mount -t virtiofs <tag>`), assigned when the device is attached; no
  /// host path exists, no directory is created, no socket is placed on disk (§4.6 A-9).
  GuestTag,
}

/// What the attachment's rights let a consumer do through the transport (§4.6 A-9 "read/write policy").
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadWritePolicy {
  /// Reads only: every mutation is refused (`EROFS` at a mount, `ro` on a bind).
  ReadOnly,
  /// Reads and writes, under the volume's lease.
  ReadWrite,
}

/// How a transport's kernel client caches names, attributes and data (§4.6 A-9 "sharing/cache
/// semantics").
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum KernelCache {
  /// Not established: no kernel client exists for the form (the record form), or none has negotiated.
  NotEstablished,
  /// The kernel client's own timeouts, chosen at mount time by the mounting command (the NFS loopback
  /// mount asks for `actimeo=1`, `crates/cli/src/mount.rs`); the server is stateless and keeps no open
  /// state, so coherence is the client's timeout, not an invalidation.
  ClientTimeouts,
  /// Negotiated with the kernel or guest at `FUSE_INIT`.
  Negotiated {
    /// Whether writeback caching was negotiated (dirty pages held by the client until written back).
    writeback: bool,
    /// Whether explicit data invalidation was negotiated.
    explicit_invalidation: bool,
  },
  /// The container's view is the host mount's: the bind adds no cache of its own; a runtime that hosts
  /// containers in a VM adds its guest's page cache over the share it makes of the host path.
  InheritedFromHostMount,
}

/// What a delete of a file that some process still holds open does at the transport (§4.6 A-9
/// "sharing/cache semantics"; Appendix C "macOS NFS fallback: `.nfs` temp files on
/// delete-while-open"). Measured 2026-09-14 over the container bind on macOS: the runtime's share of
/// the host path holds every file a container touched open beyond the container's lifetime, so a
/// delete inside the container leaves a `.nfs.*` entry in the volume (not released within 150 s),
/// which blocks `rmdir` of its directory and the unmount until the share lets go.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteWhileOpen {
  /// No kernel client holds files open through this form (the record form).
  NoKernelClient,
  /// The name goes at once; the open file's inode lives until its last close (FUSE, a guest device).
  Unlinked,
  /// The kernel client renames the file to `.nfs.<id>` until its last close, then removes it: the
  /// entry is visible in every view meanwhile and its directory cannot be removed.
  SillyRenamed,
}

/// The sharing and cache semantics of a transport (§4.6 A-9).
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct SharingSemantics {
  /// One owning shard serves the volume in order (D-7): every transport shares one view.
  pub one_owning_shard: bool,
  /// Whether the server keeps per-consumer open state (FUSE and a guest device do; NFS does not).
  pub server_open_state: bool,
  /// The kernel-side cache.
  pub cache: KernelCache,
  /// What a delete of an open file does; a container bind inherits its host mount's.
  pub delete_while_open: DeleteWhileOpen,
}

/// Where the bytes can reside (§4.6 A-9 "residency boundary"; R1: never on a disk).
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Residency {
  /// Only the daemon's RAM.
  DaemonRam,
  /// The daemon's RAM and the mounting kernel's page cache on the same host.
  DaemonRamAndKernelCache,
  /// The daemon's RAM, the host kernel's cache, and the VM's page cache over its share of the host path
  /// — every OCI runtime on macOS hosts its containers in a Linux VM.
  DaemonRamKernelCacheAndRuntimeVm,
  /// The daemon's RAM and the guest kernel's page cache, written back through the device (R1); and
  /// whether host memory is mapped into the guest (DAX) — never, until its gate is met (AC-4.12).
  DaemonRamAndGuestPageCache {
    /// Whether DAX mappings are advertised to the guest.
    dax_mapped: bool,
  },
}

/// The evidence behind a transport's report (§4.6 A-9 "conformance evidence"; AC-9.7 "A skipped lane
/// or pure simulation cannot close its transport guarantee"): the kind of by-use test this tree holds
/// for the transport on this platform — never a claim that it ran here.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conformance {
  /// No by-use evidence for this transport on this platform.
  None,
  /// The lifecycle verbs driven over the ring: attach, lease, detach (`crates/server/tests/daemon.rs`).
  VerbLifecycleTest,
  /// A real kernel mount driven by use (`crates/cli/tests/cli.rs`, the live mount flow).
  LiveKernelMountTest,
  /// The same filesystem workload inside a real container and on the host (T-4.13).
  ContainerWorkloadTest,
  /// The simulated guest driver's differential oracle against direct FUSE dispatch
  /// (`crates/bridge-virtiofs`); no live guest has run (AC-9.7).
  SimulatedGuestDriver,
}

/// Why a transport is not offered here: the `reason` of [`Refusal::AttachmentUnsupported`] and of a
/// refused entry in the report. Append-only.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsupportedReason {
  /// The transport does not exist on this operating system.
  HostPlatform,
  /// The daemon's loopback listener did not bind, so there is no export to mount.
  ListenerNotBound,
  /// Mounting the transport needs a privilege slates never asks for (R10): the Linux kernel refuses an
  /// NFS mount in an unprivileged user namespace.
  MountNeedsPrivilege,
  /// The bridge exists as a crate but the daemon does not establish or serve it yet.
  BridgeNotWired,
  /// A container bind needs an established host mount, and no host mount transport is offered here.
  HostMountRequired,
  /// The host mount presents the volume's live head, so a snapshot cannot be bound through it.
  SnapshotNotPresentedByHostMount,
  /// A guest device needs its VMM seam, which the harness hands to the daemon in-process
  /// (`Daemon::attach_guest_device`); no seam accompanies a ring request.
  SeamNotOnWire,
  /// The inherited-descriptor (vhost-user / libkrun) VMM binding is not built; the in-process seam
  /// is the served form (the device's own reason, `crates/bridge-virtiofs`).
  BindingNotBuilt,
  /// DAX was requested; the baseline contract does not require it and it cannot be advertised until
  /// mapping isolation, pinning and teardown are established for the VMM (§4.6 A-9, AC-4.12).
  DaxNotEstablished,
  /// A notification queue was requested; `VIRTIO_FS_F_NOTIFICATION` is not offered.
  NotificationQueueNotOffered,
}

/// One transport's report (§4.6 A-9: "supported transport, target-path constraints, read/write policy,
/// sharing/cache semantics, residency boundary and conformance evidence").
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttachmentCapability {
  /// The transport.
  pub transport: AttachTransport,
  /// Whether this daemon establishes it on this host, now.
  pub supported: bool,
  /// Why not, when it does not; `None` exactly when `supported`.
  pub unsupported_reason: Option<UnsupportedReason>,
  /// The target-path constraint.
  pub target_path: TargetPathConstraint,
  /// The read/write policy as the caller's rights (and, on an `attach`, its intent) allow.
  pub read_write: ReadWritePolicy,
  /// The sharing and cache semantics.
  pub sharing: SharingSemantics,
  /// The residency boundary.
  pub residency: Residency,
  /// The conformance evidence.
  pub conformance: Conformance,
}

/// The OCI runtime found on the daemon's `PATH` (§4.6 A-9 "Capabilities differ by host, kernel,
/// runtime"): a fact about this host, not a requirement — the harness may hold its own runtime.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum OciRuntime {
  /// The first of `runc`, `crun`, `youki`, `docker`, `podman`, `nerdctl` found, in that order.
  Found {
    /// The command's name.
    name: String,
  },
  /// The `PATH` was probed and holds none of them.
  NoneOnPath,
  /// The `PATH` was not probed on this platform.
  NotProbed,
}

/// The host's transport report (§4.6 A-9): the facts that qualify it, then every transport.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct TransportReport {
  /// The operating system as the kernel names itself (`uname` sysname; `windows` there).
  pub os: String,
  /// The kernel release (`uname` release); `None` where the OS states none.
  pub kernel: Option<String>,
  /// The OCI runtime on the daemon's `PATH`.
  pub oci_runtime: OciRuntime,
  /// Every transport, in a fixed order, offered or refused with its reason.
  pub capabilities: Vec<AttachmentCapability>,
}

/// The form an attach asks for (§4.4 `attach(volume|snapshot, consumer, transport, chosen_path?)`).
/// Append-only.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum AttachRequest {
  /// The record form under the root mount.
  Root,
  /// A bind of the established host mount at `source` — a mount point of this volume on this host,
  /// by its real path — to `destination` inside a container, for the host's OCI runtime to perform.
  Oci {
    /// The host mount point (the bind's `source`).
    source: String,
    /// The path inside the container (the bind's `destination`).
    destination: String,
  },
  /// A guest device over a guest transport (`VirtioFsInProcess`, `VirtioFsInheritedDescriptor`).
  /// Refused over the ring: the VMM seam is handed to the daemon in-process by the harness.
  Guest {
    /// The guest transport.
    transport: AttachTransport,
  },
}

/// What an attach established (§4.4 "establish the path or device, then publish `Bound`").
/// Append-only.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum Established {
  /// The record under the root mount; no path of its own.
  Record,
  /// An authorized container binding of a verified host mount (§4.6 A-9). Boxed: the binding's
  /// strings would otherwise grow every reply's move; the wire bytes are the binding's own.
  OciBind {
    /// The binding.
    binding: Box<OciBinding>,
  },
}

/// What the kernel's mount table established about a host path — read as a query of the table, never
/// by touching the mount (§4.6 A-9 "A metadata record is insufficient evidence of a usable container
/// path": the table is the kernel's word that the path is a mount point of the named export; the
/// container's view is then exactly the host mount's, which the live mount tests prove by use).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct HostMountEvidence {
  /// The filesystem type at the mount point (`nfs` for the loopback mount; `fuse.slates` for FUSE).
  pub fstype: String,
  /// The mount's source as the table records it (`localhost:/<name>` for the loopback mount).
  pub mount_source: String,
  /// Whether the source names this volume (the NFS export does; the FUSE source is `slates` for every
  /// volume, so there the evidence is the slates filesystem type alone).
  pub names_volume: bool,
}

/// The `mounts[]` entry the harness hands its OCI runtime (the OCI runtime specification's bind mount:
/// `destination`, `type`, `source`, `options`), with what slates verified about the source. slates
/// performs no namespace work: the runtime binds; the record carries the authorization.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct OciBinding {
  /// The host mount point (the bind's `source`), as the mount table records it.
  pub source: String,
  /// The path inside the container (the bind's `destination`), created there by the runtime.
  pub destination: String,
  /// Whether the bind is read-only: a read attachment.
  pub read_only: bool,
  /// The entry's `type` (`bind`).
  pub mount_type: String,
  /// The entry's `options`: the recursive bind, then `ro` or `rw`.
  pub options: Vec<String>,
  /// What the kernel's mount table said about the source.
  pub evidence: HostMountEvidence,
}

/// Why a chosen host path cannot be honoured (§4.4 "Attach with a chosen path that cannot be
/// honoured: Refused (`ChosenPathUnavailable{reason}`)"). Append-only.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum HostPathReason {
  /// The source is not an absolute path; nothing was consulted.
  NotAbsolute,
  /// The destination is not an absolute path (the runtime specification requires one).
  DestinationNotAbsolute,
  /// The kernel's mount table lists no mount at exactly this path (a directory inside a mount is not
  /// the attachment; give the mount point's real path).
  NotAMountPoint,
  /// The mount at this path is another filesystem, not a slates export.
  ForeignFilesystem {
    /// The filesystem type the table records.
    fstype: String,
  },
  /// The mount at this path is a slates export of another volume.
  NotThisVolume {
    /// The source the table records.
    source: String,
  },
  /// The kernel's mount table could not be read (the errno), so nothing is claimed.
  MountTableUnavailable {
    /// The errno of the query; 0 when the table is malformed rather than refused.
    errno: i32,
  },
}

/// An action name and how many entries take it.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct ActionCount {
  /// The action name.
  pub action: String,
  /// The count.
  pub count: u64,
}

/// A landing manifest's summary (§4.15 step 2).
#[derive(Wire, Clone, Debug, PartialEq, Eq, Default)]
pub struct LandingSummary {
  /// Entries per action name.
  pub by_action: Vec<ActionCount>,
  /// Bytes the landing writes.
  pub bytes: u64,
  /// Entries the filter left out.
  pub filtered_out: u64,
}

/// A finished landing's outcome (§4.15).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct LandingOutcome {
  /// The landing id.
  pub landing: u64,
  /// The terminal state name (`done`, `partial`, `refused`, `aborted`).
  pub state: String,
  /// Entries written.
  pub written: u64,
  /// Entries skipped or accepted without a write.
  pub skipped: u64,
  /// Entries that lost their compare-and-swap.
  pub conflicts: u64,
  /// Entries the host refused.
  pub failed: u64,
  /// Bytes written to the disk.
  pub bytes_written: u64,
}

/// A grant in a listing (§4.13, §4.15).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct GrantSummary {
  /// The id.
  pub id: u64,
  /// The volume.
  pub volume: VolumeId,
  /// The target.
  pub target: String,
  /// The manifest hash it binds.
  pub manifest: [u8; 32],
  /// The scope.
  pub scope: GrantScope,
  /// The state name (`issued`, `consumed`, `expired`, `revoked`).
  pub state: String,
}

/// An audit record on the wire (§4.15).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct AuditEntry {
  /// The sequence.
  pub seq: u64,
  /// Monotonic ns.
  pub at_ns: u64,
  /// The kind name.
  pub kind: String,
  /// The grant, when bound.
  pub grant: Option<u64>,
  /// The landing, when bound.
  pub landing: Option<u64>,
  /// The manifest, when bound.
  pub manifest: Option<[u8; 32]>,
  /// The terminal state, for a finished landing.
  pub outcome: Option<String>,
}

/// The closed refusal taxonomy on the wire (§4.4, §4.13). (`Eq` is not derived because the durability
/// refusal carries the measured loss probabilities, which are floating point.)
#[derive(Wire, Clone, Debug, PartialEq)]
pub enum Refusal {
  /// Not found.
  NotFound,
  /// The name exists; the original's id.
  AlreadyExists {
    /// The existing volume.
    existing: VolumeId,
  },
  /// A stale lease epoch.
  StaleLease {
    /// The current epoch.
    current: u64,
  },
  /// Another holder has the lease.
  LeaseHeld {
    /// The holder's epoch.
    epoch: u64,
  },
  /// No space in the quota.
  NoSpace,
  /// The shard's reserve cannot cover the reservation.
  BudgetExceeded {
    /// Bytes available.
    available: u64,
  },
  /// The principal lacks the right.
  Forbidden {
    /// The verb.
    verb: String,
  },
  /// A grant on a channel that cannot carry one.
  GrantChannelRefused {
    /// The channel.
    channel: String,
  },
  /// The volume is being destroyed.
  Destroying,
  /// The volume is archived.
  Archived,
  /// An invalid name.
  InvalidName,
  /// A policy the base does not allow.
  PolicyMismatch,
  /// The base directory is unavailable.
  BaseUnavailable {
    /// The path.
    path: String,
    /// The errno.
    errno: i32,
  },
  /// A stale retry of an acknowledged request.
  DuplicateRequest,
  /// Not supported here.
  Unsupported {
    /// The feature.
    feature: String,
  },
  /// The shard's client or task capacity is reached.
  TooManyClients,
  /// A shard's cross-shard queue is at its bound; the request was not started and may be
  /// retried (the client's credit bounds what it can have in flight).
  Overloaded {
    /// The shard whose queue is full.
    shard: u16,
  },
  /// A malformed request.
  BadRequest {
    /// What was wrong.
    reason: String,
  },
  /// The landing target directory cannot be opened, or escapes containment (§4.15 step 4).
  TargetUnavailable {
    /// What was wrong.
    reason: String,
  },
  /// The landing hit conflicts; nothing was written (§4.15 step 4).
  LandingConflict {
    /// The conflicting entries.
    entries: Vec<String>,
  },
  /// Another session holds the target's landing lease (§4.15, AC-2.9).
  LandingLeaseHeld {
    /// The holder.
    holder: u64,
  },
  /// The grant does not bind the manifest about to be written (§4.15 step 3, AC-2.8).
  GrantMismatch,
  /// The grant is missing, expired or revoked.
  GrantInvalid,
  /// The volume is homed in another region (§4.8 "Lookup" — a home move or region-loss promotion is a
  /// configuration exception): this node's region does not serve it, and the answer must come from the named
  /// home region. Names the region so the caller re-routes there (the cross-region redirect, the data-plane
  /// counterpart of the configuration-version piggyback).
  HomedElsewhere {
    /// The region id that now homes the volume.
    region: u64,
  },
  /// A region-loss promotion (`PromoteRegion`) was issued on a node that is not the root leader, so it cannot
  /// propose the change (§4.8, D-14). The operator re-issues it on the root leader (`status` names it).
  NotRootLeader,
  /// The write would commit a new head or seal that the fleet's committed configuration cannot hold to the
  /// operator's declared durability (§4.8 "Placement" — "the operator's accepted ε and the coincident-failure
  /// size are the durability policy that gates a refusal"; D-14, D-18): under a coincident failure of
  /// `coincident_failures` hosts the configuration's copysets lose data with probability `coincident_loss`,
  /// above the accepted `accepted_loss` (the operator's ε). Measured at the configuration change that
  /// installed the configuration, never per write; the resolution is the operator's — more copies, more
  /// re-replication bandwidth, tighter failure domains, or a policy that accepts the loss — never a silent
  /// degrade. Reads, destroys, resizes and status continue.
  DurabilityUnmet {
    /// The configuration's measured coincident-loss probability under the policy's failure count.
    coincident_loss: f64,
    /// The loss probability the operator's policy accepts (its ε).
    accepted_loss: f64,
    /// The number of hosts the policy assumes fail at once.
    coincident_failures: u64,
  },
  /// A grant whose proof of issuer authority did not verify (§4.13 "Grants": the daemon verifies the
  /// authority and the exact manifest, target, consumer, scope and validity before accepting a grant; a
  /// forged, replayed, retargeted or modified-plan approval refuses before writing). The proof is a keyed
  /// hash over the landing under the issuer secret only the anchor-mapped human surface holds, so a
  /// workload that invokes the CLI binary, or claims a channel, cannot mint it.
  GrantIssuerUnverified,
  /// The caller is not an enrolled consumer under its account, and this daemon requires enrollment for
  /// the verb (§4.13 "Principals": per-request identity strings and the peer uid alone cannot establish
  /// consumer identity; unsupported secure enrollment refuses instead of issuing an ambient channel).
  ConsumerNotEnrolled,
  /// The caller's consumer enrollment was revoked by a human; every later effect refuses (§4.13
  /// "Refusals added"; revocation reaches a live session before its next protected verb).
  ConsumerRevoked,
  /// No clean digest exists for the entry (§4.15): it is not an untouched regular base file (the
  /// volume created, copied up, pinned or lost it, or it is a symlink), so a digest would name
  /// bytes that are not the disk's. Read and hash the bytes instead.
  DigestNotClean,
  /// The file changed on the disk while it was being digested (§4.15 "verified current"): nothing
  /// stale is exported; retry.
  DigestUnverified,
  /// The verb would write a green volume, which nothing but its merge task writes (§4.16, D-27; §4.4
  /// "an SDK write is `ReadOnlyVolume`"): an edit or declaration on it, a write attachment, a snapshot
  /// or resize of it. Merge through a work volume instead. Appended for append-only evolution.
  ReadOnlyVolume,
  /// The verb names a green (a work's creation, the chain reads, an advance) but the volume is not one
  /// (§4.4 merge refusals).
  NotGreen,
  /// The verb names a work volume (an edit, a declaration, a submit, a rebase) but the volume is not one
  /// — a plain volume, which declares nothing to merge (§4.4 merge refusals).
  NotWork,
  /// The increment's base names a version its green does not have — the green was destroyed, or the
  /// version is another green's or past the head (§4.16 failure matrix).
  UnknownBase {
    /// The green.
    green: VolumeId,
    /// The version the work was based on.
    version: u64,
  },
  /// The green requires evidence on every increment and the submit carried none (§4.16).
  EvidenceRequired,
  /// The base a green was asked to start from is not a complete immutable snapshot: an overlay's with
  /// entries still served live from the host directory (A-9: never an implicitly live host directory).
  /// Pin the whole base and snapshot again.
  ConsistentBaseUnavailable,
  /// An increment's inputs — its ops document and post-state — are not held where the verdict must
  /// run (a holder asked to recompute a version whose inputs have not reached it); retryable (§4.16).
  ContentUnavailable,
  /// A merge record below the epoch a holder has already seen for the green's owner; the stale owner
  /// drops the role (§4.16 failure matrix).
  StaleEpoch {
    /// The epoch the holder has seen.
    current: u64,
  },
  /// The requested attachment form is not offered here (§4.6 A-9 "Requesting an unsupported form
  /// returns `AttachmentUnsupported{transport, reason}`"); refused before any effect, and `status`
  /// reports every transport with its reason.
  AttachmentUnsupported {
    /// The transport requested.
    transport: AttachTransport,
    /// What is missing.
    reason: UnsupportedReason,
  },
  /// The chosen host or container path cannot be honoured (§4.4); refused before any effect.
  ChosenPathUnavailable {
    /// Why.
    reason: HostPathReason,
  },
  /// This member has not joined an existing configuration group or been explicitly bootstrapped.
  ConsensusNotInitialized,
  /// Bootstrap would replace an existing group's consensus state.
  ConsensusAlreadyInitialized,
  /// This explicit bootstrap request names a different daemon start.
  ConsensusBootstrapStale,
  /// The reviewed state or daemon authority changed before recovery was authorized.
  ConsensusRecoveryStale,
  /// No complete group state is available, or its counters cannot advance safely.
  ConsensusRecoveryUnavailable,
}

/// A reply body. (`Eq` is not derived: a [`Refusal`] may carry measured probabilities.)
#[derive(Wire, Clone, Debug, PartialEq)]
pub enum ReplyBody {
  /// Created.
  Created {
    /// The id.
    id: VolumeId,
  },
  /// Snapshotted.
  Snapshotted {
    /// The id.
    id: SnapshotId,
  },
  /// A snapshot was destroyed.
  SnapshotDestroyed,
  /// A green volume was created.
  GreenCreated {
    /// The id.
    id: VolumeId,
  },
  /// A green's version chain state.
  Versions {
    /// The head version.
    head: u64,
  },
  /// The files changed since a version.
  ChangedSince {
    /// The changed file paths.
    paths: Vec<String>,
  },
  /// A work volume was created over a green.
  WorkCreated {
    /// The id.
    id: VolumeId,
    /// The green version it is based on.
    base: u64,
  },
  /// An edit was recorded on a work volume.
  Edited,
  /// A namespace or metadata operation was declared on a work volume.
  Declared,
  /// A work volume's increment was submitted (§4.16).
  Submitted {
    /// The committed green version, when the increment was accepted; `None` on conflict.
    version: Option<u64>,
    /// The conflict windows to rebase against, when not accepted.
    conflicts: Vec<MergeWindow>,
  },
  /// A work volume was rebased onto its green's head (§4.16). The green is unchanged.
  Rebased {
    /// The head the work is now based on, when every operation mapped cleanly; `None` on conflict.
    version: Option<u64>,
    /// The conflict windows to resolve, when the rebase did not complete.
    conflicts: Vec<MergeWindow>,
  },
  /// Cloned.
  Cloned {
    /// The clone's id.
    id: VolumeId,
  },
  /// Attached.
  Attached {
    /// The attachment.
    attachment: u64,
    /// The lease epoch, for a write attachment.
    lease_epoch: Option<u64>,
    /// The path a bridge publishes (none until a bridge exists).
    path: Option<String>,
    /// The green version this attachment pins (§4.16), or none for a plain or work volume.
    version: Option<u64>,
    /// What was established for the requested form.
    established: Established,
    /// The transport's report for this attachment (§4.6 A-9 "must be reported by `attach`").
    capability: AttachmentCapability,
  },
  /// Detached.
  Detached,
  /// Resized.
  Resized,
  /// Destroyed.
  Destroyed,
  /// The status.
  Status {
    /// The report.
    report: StatusReport,
  },
  /// The listing.
  Listed {
    /// The volumes.
    volumes: Vec<VolumeSummary>,
  },
  /// Acknowledged.
  Acknowledged,
  /// The base bytes.
  BaseBytes {
    /// The bytes.
    bytes: Vec<u8>,
  },
  /// Re-witnessed.
  Rewitnessed {
    /// The paths re-witnessed.
    paths: Vec<String>,
  },
  /// Pinned.
  Pinned {
    /// Entries pinned.
    entries: u64,
  },
  /// Refused.
  Refused {
    /// The refusal.
    refusal: Refusal,
  },
  /// The daemon's status. Boxed: the report is the one cold, wide reply (every shard's part and the
  /// fleet's two consensus groups), and boxing it keeps every hot reply's enum at its narrow size — the
  /// codec encodes a box as its content.
  DaemonStatus {
    /// The report.
    report: Box<DaemonReport>,
  },
  /// The awaited scope's placement.
  Placed {
    /// Whether the scope is placed.
    placed: bool,
    /// The mirror's lag, for a mirror scope.
    mirror_age_ns: Option<u64>,
  },
  /// A landing needs a grant: the manifest the human must see, its hash, and the conflicts a
  /// preliminary verdict pass found (§4.15 step 2).
  GrantRequired {
    /// The landing id (what `slates grant` names).
    landing: u64,
    /// The manifest hash the grant must bind.
    manifest: [u8; 32],
    /// The summary of what would be written.
    summary: LandingSummary,
    /// Conflicts found before any write (empty when the landing is clean).
    conflicts: Vec<String>,
  },
  /// A landing finished (or partially).
  Landed {
    /// The outcome.
    outcome: LandingOutcome,
  },
  /// The caller's grants.
  Grants {
    /// The grants.
    grants: Vec<GrantSummary>,
  },
  /// The audit log.
  Audit {
    /// The records.
    records: Vec<AuditEntry>,
  },
  /// A grant was issued for a presented landing (the reply to a verified `Grant`).
  Granted {
    /// The grant id the landing now carries (`land ... --grant N`).
    grant: u64,
  },
  /// A consumer was enrolled: its id and — once, to the human surface — the secret capability it
  /// attests with.
  Enrolled {
    /// The consumer id.
    consumer: u64,
    /// The capability, shown once.
    secret: [u8; 32],
  },
  /// The channel is bound to the consumer it attested.
  Attested,
  /// The consumer's enrollment is revoked.
  Revoked,
  /// The principal's rights on the volume are set.
  Shared,
  /// One shard's telemetry drain (§4.14).
  Telemetry {
    /// The batch.
    report: TelemetryReport,
  },
  /// A clean file's verified content digest (§4.15).
  Digest {
    /// BLAKE3 of the file's bytes as the disk holds them.
    identity: [u8; 32],
    /// The length digested, in bytes.
    size: u64,
  },
  /// A green attachment was re-pinned (§4.16 `advance`). Appended for append-only evolution.
  Advanced {
    /// The version the attachment now pins.
    version: u64,
    /// The paths the move invalidated — those some version in the span changed, sorted.
    invalidated: Vec<String>,
  },
  /// A file's bytes at the requested view (§4.12 `read`). Appended for append-only evolution.
  ReadBytes {
    /// The bytes.
    bytes: Vec<u8>,
  },
  /// The local copy and exact action proposed for operator-reviewed quorum-loss recovery.
  RecoveryPlan {
    /// The proposal.
    plan: ConsensusRecoveryPlan,
  },
  /// The reviewed replacement was created or its bounded join was authorized.
  RecoveryStarted {
    /// The replacement consensus identity.
    group: [u8; 32],
    /// Whether this node still has to fetch and join that group.
    joining: bool,
  },
}

/// A message body on the ring: the schema hash then the canonical encoding.
fn frame<M: Wire>(message: &M) -> Vec<u8> {
  let mut out = Vec::with_capacity(size_of::<u64>());
  M::SCHEMA_HASH.encode(&mut out);
  message.encode(&mut out);
  out
}

/// Decodes a framed body, refusing another schema.
fn unframe<M: Wire>(bytes: &[u8]) -> Result<M, IpcError> {
  let mut input = bytes;
  let schema = u64::decode(&mut input).map_err(|_| IpcError::BadSlot {
    reason: "no schema word",
  })?;
  if schema != M::SCHEMA_HASH {
    return Err(IpcError::BadSlot {
      reason: "schema mismatch",
    });
  }
  M::from_bytes(input).map_err(|_| IpcError::BadSlot {
    reason: "body does not decode",
  })
}

/// Serialises a request or reply body to the schema-checked bytes the message wire uses (the framing
/// [`pack`] applies before slotting), for a body that travels **outside** the client ring — a verb forwarded
/// to another node over the fleet transport so a request reaches the volume's owner or the operator's command
/// reaches the leader (§4.8 "Lookup"). [`decode_body`] is its inverse, refusing another schema.
pub fn encode_body<M: Wire>(message: &M) -> Vec<u8> {
  frame(message)
}

/// Decodes a body [`encode_body`] produced, refusing a body of another schema or a truncated one.
pub fn decode_body<M: Wire>(bytes: &[u8]) -> Result<M, IpcError> {
  unframe(bytes)
}

/// Format: a bulk reference in a slot's payload: offset (8), length (8).
const BULK_REF_BYTES: usize = 16;

/// Which half of the bulk area a message uses: requests (the client writes), replies (the
/// daemon writes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
  /// Client to daemon.
  Request,
  /// Daemon to client.
  Reply,
}

/// The bulk chunk for ring index `index` in `direction`: the area is split in two halves,
/// each divided evenly among the ring's slots, so a message's chunk is fixed by its slot and
/// no allocator sits on the path.
fn chunk(region: &ClientRegion, direction: Direction, index: u64) -> (usize, usize) {
  let slots = region.cmd().slots().max(1);
  let half = region.bulk().len() / 2;
  let chunk = half / slots;
  let position = usize::try_from(index % u64::try_from(slots).unwrap_or(1)).unwrap_or(0);
  let base = match direction {
    Direction::Request => 0,
    Direction::Reply => half,
  };
  (base + position * chunk, chunk)
}

/// Frames `message` into the slot for ring index `index`, inline or through the bulk chunk.
pub fn pack<M: Wire>(
  region: &mut ClientRegion,
  direction: Direction,
  index: u64,
  request: u64,
  message: &M,
) -> Result<Slot, IpcError> {
  let body = frame(message);
  if body.len() <= PAYLOAD_BYTES {
    return Slot::inline(request, &body);
  }
  let (offset, capacity) = chunk(region, direction, index);
  if body.len() > capacity {
    return Err(IpcError::PayloadTooLarge {
      offered: body.len(),
      capacity,
    });
  }
  region.bulk_mut()[offset..offset + body.len()].copy_from_slice(&body);
  let mut payload = [0u8; BULK_REF_BYTES];
  payload[..8].copy_from_slice(&u64::try_from(offset).unwrap_or(u64::MAX).to_le_bytes());
  payload[8..].copy_from_slice(&u64::try_from(body.len()).unwrap_or(u64::MAX).to_le_bytes());
  Ok(Slot {
    kind: SlotKind::Bulk,
    request,
    payload: payload.to_vec(),
  })
}

/// Decodes the message a slot carries, inline or from the bulk chunk it names; a reference
/// outside the bulk area is a typed refusal.
pub fn unpack<M: Wire>(
  region: &ClientRegion,
  kind: SlotKind,
  payload: &[u8],
) -> Result<M, IpcError> {
  match kind {
    SlotKind::Inline => unframe(payload),
    SlotKind::Bulk => {
      if payload.len() != BULK_REF_BYTES {
        return Err(IpcError::BadSlot {
          reason: "bulk reference of the wrong size",
        });
      }
      let offset = usize::try_from(u64::from_le_bytes(
        payload[..8].try_into().unwrap_or([0; 8]),
      ))
      .unwrap_or(usize::MAX);
      let len = usize::try_from(u64::from_le_bytes(
        payload[8..].try_into().unwrap_or([0; 8]),
      ))
      .unwrap_or(usize::MAX);
      let bulk = region.bulk();
      let end = offset.checked_add(len).ok_or(IpcError::BadSlot {
        reason: "bulk reference overflows",
      })?;
      if end > bulk.len() {
        return Err(IpcError::BadSlot {
          reason: "bulk reference outside the area",
        });
      }
      unframe(&bulk[offset..end])
    }
    SlotKind::Cancel | SlotKind::Heartbeat => Err(IpcError::BadSlot {
      reason: "not a message",
    }),
  }
}

#[cfg(test)]
mod health_signal_registry {
  use super::{AbsenceIs, FreshnessBasis, HealthSignal};

  /// The design document, whose "*Health signal catalog.*" paragraph is the catalog of record.
  const DESIGN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/wip/SLATES_DESIGN.md"
  );
  /// The living observability record whose registry table is generated from this module.
  const RECORD: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/wip/observability.md"
  );
  /// The markers around the generated health-signal table in the record.
  const TABLE_BEGIN: &str = "<!-- health-signals:begin -->\n";
  const TABLE_END: &str = "<!-- health-signals:end -->";
  /// Registry signals the design's catalog does not list yet — a reviewed drift list that only
  /// shrinks (CLAUDE §4, expected-failure lists): the shard emits these two as counts of its own client
  /// slots and deferred-reply queue, and the design's §4.14 catalog owes them (or the registry a
  /// rename). Adding a name here is a design change; removing one is what fixing the catalog does.
  const DESIGN_CATALOG_DRIFT: &[&str] = &["shard.clients", "shard.deferred"];

  /// The signal names the design's "*Health signal catalog.*" sentence lists (`name{label}` tokens in
  /// backticks, labels stripped).
  fn design_catalog() -> Vec<String> {
    let design = std::fs::read_to_string(DESIGN).expect("the design document is readable");
    let sentence = design
      .lines()
      .find_map(|line| line.strip_prefix("*Health signal catalog.* "))
      .expect("the design has a `*Health signal catalog.*` paragraph");
    let mut names = Vec::new();
    let mut rest = sentence;
    while let Some(start) = rest.find('`') {
      let after = &rest[start + 1..];
      let Some(end) = after.find('`') else { break };
      let token = &after[..end];
      // A catalog token is `name{label}` or `name` (dotted or underscored: `mirror_age{volume}`);
      // `(value, freshness_ns)` and the like carry a space or a comma and are skipped.
      if !token.contains(' ') && !token.contains(',') && !token.contains('(') {
        let name = token.split_once('{').map_or(token, |(name, _)| name);
        names.push(name.to_owned());
      }
      rest = &after[end + 1..];
    }
    names
  }

  /// Every registry signal is one the design's §4.14 catalog names, except the reviewed drift list —
  /// and the drift list is exactly the registry's names the catalog lacks, so it can only shrink as the
  /// design catches up (doc-truth, GAP-A9-12). Do: read the design's catalog sentence. Expect: each
  /// registry name is in it or on the list, and every listed name is genuinely absent from it.
  #[test]
  fn every_registry_signal_is_in_the_designs_catalog_or_on_the_reviewed_drift_list() {
    let catalog = design_catalog();
    assert!(
      catalog.len() > HealthSignal::ALL.len(),
      "the design's catalog is the wider set: {catalog:?}"
    );
    for signal in HealthSignal::ALL {
      let name = signal.name();
      let in_catalog = catalog.iter().any(|listed| listed == name);
      let on_list = DESIGN_CATALOG_DRIFT.contains(&name);
      assert!(
        in_catalog || on_list,
        "{name} is emitted by the registry but neither in the design's §4.14 catalog nor on the reviewed drift list"
      );
      assert!(
        !(in_catalog && on_list),
        "{name} is now in the design's catalog: remove it from DESIGN_CATALOG_DRIFT (the list only shrinks)"
      );
    }
  }

  /// The generated block of the record between its markers.
  fn recorded_table() -> String {
    let record = std::fs::read_to_string(RECORD).expect("docs/wip/observability.md is readable");
    let start = record
      .find(TABLE_BEGIN)
      .expect("the record has the health-signals:begin marker")
      + TABLE_BEGIN.len();
    let end = record[start..]
      .find(TABLE_END)
      .expect("the record has the health-signals:end marker")
      + start;
    record[start..end].to_owned()
  }

  /// The health-signal table in `docs/wip/observability.md` is generated from the registry (doc-truth):
  /// the block between the markers equals `HealthSignal::registry_table()` byte for byte. Do: read the
  /// record. Expect: equality; `--ignored regenerate_the_health_signal_table` rewrites it deliberately.
  #[test]
  fn the_recorded_health_signal_table_is_the_registry() {
    assert_eq!(
      recorded_table(),
      HealthSignal::registry_table(),
      "docs/wip/observability.md's health-signal table drifted from the registry; run `cargo test -p slates-ipc --lib -- --ignored regenerate_the_health_signal_table`"
    );
  }

  /// The deliberate writer: rewrites the generated block of the record from the registry. Ignored, so a
  /// normal test run never mutates the tree; run with `--ignored` after a registry change.
  #[test]
  #[ignore = "rewrites docs/wip/observability.md from the registry; run deliberately with --ignored"]
  fn regenerate_the_health_signal_table() {
    let record = std::fs::read_to_string(RECORD).expect("docs/wip/observability.md is readable");
    let start = record
      .find(TABLE_BEGIN)
      .expect("the record has the health-signals:begin marker")
      + TABLE_BEGIN.len();
    let end = record[start..]
      .find(TABLE_END)
      .expect("the record has the health-signals:end marker")
      + start;
    let rewritten = format!(
      "{}{}{}",
      &record[..start],
      HealthSignal::registry_table(),
      &record[end..]
    );
    // The design's `--ignored regenerate` writer rewriting a tracked document in the repository (CLAUDE
    // §4 "Doc-truth tests"); shipped code never reaches this call.
    #[allow(clippy::disallowed_methods)]
    std::fs::write(RECORD, rewritten).expect("the record is writable");
  }

  /// Every health signal states its freshness basis (§4.14): `log.replay_ns` is a boot-time fact aged
  /// since boot; every other signal is computed at the report. Pinned so a new signal must decide how
  /// it ages rather than inherit age zero.
  #[test]
  fn every_signal_states_its_freshness_basis() {
    for signal in HealthSignal::ALL {
      let expected = if matches!(signal, HealthSignal::LogReplayNs) {
        FreshnessBasis::SinceBoot
      } else {
        FreshnessBasis::AtReport
      };
      assert_eq!(
        signal.freshness(),
        expected,
        "{} ages by its basis",
        signal.name()
      );
      assert!(!signal.producer().is_empty());
      assert!(!signal.observer().name().is_empty());
    }
  }

  /// The health-signal registry is closed (§4.14, GAP-A9-12): `ALL` emits the canonical set in order,
  /// its names are unique and dotted, and the set is pinned here as the doc-truth — so an addition, a
  /// rename or a duplicate is caught at the test, never miscounted through a free string (the "nine
  /// spans were called seven" drift the gap names).
  #[test]
  fn the_registry_is_closed_and_its_names_are_unique() {
    // The canonical set the shard report emits, pinned here; a change to the registry must update this
    // list, which is the point — a silent drift becomes a failing assertion.
    let expected = [
      "catalog.volumes",
      "log.replay_ns",
      "lease.expiring",
      "ring.depth",
      "shard.clients",
      "shard.deferred",
    ];
    let names: Vec<&str> = HealthSignal::ALL
      .iter()
      .map(|signal| signal.name())
      .collect();
    assert_eq!(
      names.as_slice(),
      expected,
      "ALL emits the canonical registry in order"
    );
    let mut unique = names.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), names.len(), "signal names are unique");
    for name in &names {
      assert!(
        !name.is_empty() && name.contains('.'),
        "a dotted, non-empty name: {name}"
      );
    }
  }

  /// Every health signal types what its absence means (§4.14, A-9): a missing sample is never a silent
  /// healthy zero. `log.replay_ns` is `Unknown` (a fresh daemon has no replay time yet, distinct from a
  /// 0 ns replay); every other signal is an always-computable count whose absence would be a `Degraded`
  /// producer. Pinned so a new signal must decide its absence meaning, not inherit a look-alike zero.
  #[test]
  fn every_signal_types_its_absence() {
    for signal in HealthSignal::ALL {
      let expected = if matches!(signal, HealthSignal::LogReplayNs) {
        AbsenceIs::Unknown
      } else {
        AbsenceIs::Degraded
      };
      assert_eq!(
        signal.absence(),
        expected,
        "{} types its absence",
        signal.name()
      );
    }
  }
}
