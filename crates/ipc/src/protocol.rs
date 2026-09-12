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
  CreateGreen {
    /// The name.
    name: String,
    /// Whether an increment must carry evidence to be accepted.
    require_evidence: bool,
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
  /// Submit a work volume's declared operations to its green as an increment (§4.16).
  Submit {
    /// The work volume.
    work: VolumeId,
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
  /// A grant: refused on this channel by kind (§4.13, AC-2.8); the kind exists so the refusal
  /// is typed and counted.
  Grant {
    /// The request the grant would cover.
    request: u64,
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
}

/// What a health signal's absence means (§4.14, A-9): a missing sample is never silently read as
/// "healthy". A `None` value carries this so a consumer knows whether the gap is benign or a fault.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbsenceIs {
  /// The value has simply not been measured yet (a fresh daemon's replay time before any replay);
  /// its absence is not a fault, only "not known".
  Unknown,
  /// The value should be present; its absence means a producer that ought to be reporting is not,
  /// which is itself a degradation, not a healthy zero.
  Degraded,
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
}

/// A refusal kind's count.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct RefusalCount {
  /// The kind's name.
  pub kind: String,
  /// Refusals of it.
  pub count: u64,
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

/// The closed refusal taxonomy on the wire (§4.4, §4.13).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
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
}

/// A reply body.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
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
  /// The daemon's status.
  DaemonStatus {
    /// The report.
    report: DaemonReport,
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
  use super::{AbsenceIs, HealthSignal};

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
