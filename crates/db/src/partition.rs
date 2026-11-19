//! The partition: the records and their indexes, and the deterministic state transition every
//! operation is (§4.8 `Partition`). Two entry points: [`Partition::check`], the guard the live
//! path runs before appending (a stale lease, a duplicate name, a missing record), and
//! [`Partition::apply`], the unconditional transition that both the live path and replay run;
//! a recorded operation was checked once and applies the same way forever after, which is what
//! makes the partition in memory equal the partition replayed from the log.
//!
//! Indexes are adaptive radix trees over the records' keys (§4.8: `volumes_by_name: Art`,
//! leases by principal, completions by client); records sit in slabs named by generational
//! handles (D-8). The lease expiry wheel is the runtime's hierarchical timing wheel keyed by
//! the volume's slot, so expiry is a pop per tick, never a scan.

use std::collections::{BTreeMap, BTreeSet};

use slates_mem::{Handle, Slab};
use slates_rt::timer::Wheel;
use slates_wire::Wire;
use slates_wire::request::{ClientWindow, Seen};

use crate::Art;
use crate::catalog::{
  AttachmentRecord, AuditRecord, CompletionRecord, Consumer, ConsumerRecord, GrantRecord,
  GrantState, IdentityRecord, LandingLeaseRecord, LandingRecord, LeaseRecord, LineageEdge,
  NameAliasRecord, NfsCallRecord, NfsClientRecord, NfsDelegationRecord, NfsLockRecord,
  NfsOpenRecord, NfsPurgeRecord, NfsSessionRecord, Principal, SEAL_KEY_RECORD_BYTES,
  SEAL_RECIPIENT_RECORD_MAX, SealKeyOwner, SealKeyRecord, SnapshotId, SnapshotRecord, Tombstone,
  VolumeId, VolumeRecord,
};
use crate::error::DbError;
use crate::op::Op;

/// Derived: the table capacities a partition is created with (the daemon's derivation from
/// the profile: memory share per shard over record size, and the admission limit for the
/// windows).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PartitionCaps {
  /// Volumes the partition may hold.
  pub volumes: usize,
  /// Snapshots the partition may hold.
  pub snapshots: usize,
  /// Attachments the partition may hold.
  pub attachments: usize,
  /// Slots per slab segment (the pre-faulted growth unit).
  pub segment_slots: usize,
  /// Lease timers the wheel may hold.
  pub timers: usize,
  /// The wheel's tick, nanoseconds.
  pub tick_ns: u64,
  /// The bytes all green merge chains together may hold in the partition (§4.16); a new increment
  /// past it is a typed capacity refusal (bounded growth until checkpointing folds old entries).
  pub green_chain_bytes: usize,
  /// The audit records the partition retains (§4.15, §4.14; A-265): past it the oldest is dropped and counted. The
  /// daemon derives it from memory (`DaemonConfig`), so the audit is bounded with nothing invented.
  pub audit_records: usize,
}

/// One client's completion state, as the snapshot carries it.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct ClientCompletions {
  /// The host id (`HostId.0`) whose client this is (this node for a local client; the authenticated origin
  /// for a forwarded one) — the high half of the globally-unique completion key.
  pub origin: u64,
  /// The client.
  pub client: u32,
  /// Every sequence up to and including this one is acknowledged, except `owed`.
  pub acknowledged_up_to: Option<u32>,
  /// The retained completions.
  pub records: Vec<CompletionRecord>,
  /// The ids at or below `acknowledged_up_to` the client still owes, kept open (§4.9).
  pub owed: Vec<u32>,
  /// The requests that ran and wait for a version to commit (A-252).
  pub awaiting: Vec<crate::catalog::AwaitingCompletion>,
}

/// One green volume's merge chain as a snapshot carries it: the green, its origin when it was
/// seeded from a complete base, and its increments in order.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct GreenChain {
  /// The green volume.
  pub green: VolumeId,
  /// The encoded increments, in commit order.
  pub increments: Vec<Vec<u8>>,
  /// The encoded origin (§4.16), or none for a green that started from scratch. Appended for
  /// append-only evolution.
  pub origin: Option<Vec<u8>>,
}

/// The whole partition state as one canonical value: what a snapshot slot holds.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct PartitionSnapshot {
  /// The sequence of the last operation folded in.
  pub seq: u64,
  /// The volumes, by id order.
  pub volumes: Vec<VolumeRecord>,
  /// The snapshots, by (volume, id) order.
  pub snapshots: Vec<SnapshotRecord>,
  /// The lineage edges, by child order.
  pub lineage: Vec<LineageEdge>,
  /// The attachments, by id order.
  pub attachments: Vec<AttachmentRecord>,
  /// The completion windows, by client order.
  pub completions: Vec<ClientCompletions>,
  /// The grants, by id order.
  pub grants: Vec<GrantRecord>,
  /// The landing leases, by target order.
  pub landing_leases: Vec<LandingLeaseRecord>,
  /// The landings, by id order.
  pub landings: Vec<LandingRecord>,
  /// The audit records retained, oldest first: the partition's bounded window (A-265).
  pub audit: Vec<AuditRecord>,
  /// Each green volume's merge chain, by green order (§4.16; appended for append-only evolution).
  pub green_chains: Vec<GreenChain>,
  /// The enrolled consumers, by id order (§4.13; appended for append-only evolution).
  pub consumers: Vec<ConsumerRecord>,
  /// Highest client id reserved before handoff (§4.7); zero before the first admission.
  pub client_id_high_water: u32,
  /// The NFSv4 opens held at this partition's files, by `other` (§4.6 A-37; appended for append-only
  /// evolution, as are the two fields after it).
  pub nfs_opens: Vec<NfsOpenRecord>,
  /// The NFSv4 lock states held at this partition's files, by `other`.
  pub nfs_locks: Vec<NfsLockRecord>,
  /// The NFSv4 clients this partition's listener holds, by client id.
  pub nfs_clients: Vec<NfsClientRecord>,
  /// The partition's NFSv4 instance: the last one a daemon life advanced to (zero before any).
  pub nfs_instance: u32,
  /// The destroyed volumes whose tombstone is still owed to their candidate holders, by volume order
  /// (AUD-29-43; appended for append-only evolution).
  pub tombstones: Vec<Tombstone>,
  /// The NFSv4 delegations held at this partition's files, by `other` (§4.6 A-78; appended).
  pub nfs_delegations: Vec<NfsDelegationRecord>,
  /// The sealing keys recorded at this partition, wrapped by their parents, by owner (A-92; appended).
  pub seal_keys: Vec<SealKeyRecord>,
  /// The NFSv4.1 sessions this partition's listener holds, by session id (A-127; appended).
  pub nfs_sessions: Vec<NfsSessionRecord>,
  /// The NFSv4.1 call records this partition's file-state changes carried, by session slot (A-127; appended).
  pub nfs_calls: Vec<NfsCallRecord>,
  /// The NFSv4 purges this partition's shard owes its owners, not yet acknowledged by all (2026-10-10; appended).
  pub nfs_purges: Vec<NfsPurgeRecord>,
  /// Each work volume's log since its last restatement (§4.16, D-18; 2026-11-04; appended).
  pub work_logs: Vec<WorkLog>,
  /// Each green's origin held as a reference to its base snapshot (A-217; 2026-11-05; appended).
  pub green_origin_refs: Vec<GreenOriginRef>,
  /// The names this partition holds for volumes other partitions hold, by name (A-236; appended).
  pub name_aliases: Vec<NameAliasRecord>,
  /// The CLI attachments whose term lapsed, oldest first (A-230; appended).
  pub expired_attachments: Vec<u64>,
  /// Each green's checkpoint (W3; §4.16; 2026-11-06; appended).
  pub green_checkpoints: Vec<GreenCheckpoint>,
  /// Each live work's hold on a version of a green of this partition (A-247; appended).
  pub work_floors: Vec<crate::catalog::WorkFloorRecord>,
  /// Each work's submit awaiting its settle (A-247; appended).
  pub pending_submits: Vec<crate::catalog::PendingSubmitRecord>,
  /// Each work's creation awaiting its admission (A-247; appended).
  pub pending_creations: Vec<crate::catalog::PendingCreationRecord>,
  /// Each green this node took over and adopted from its own holder ledger of it (A-333; 2026-11-18; appended).
  pub green_adoptions: Vec<GreenAdoption>,
  /// The sequence the next audit record takes (A-265; appended): numbered in `apply`, so a recovery numbers each
  /// record as the live partition did.
  pub audit_next_seq: u64,
  /// The audit records dropped past the bound (A-265; appended).
  pub audit_dropped: u64,
  /// The highest grant id ever issued here (A-266; appended): the table holds live grants only.
  pub grants_issued_through: u64,
  /// The audit records owed the control partition and not yet acknowledged, oldest first (A-265's crossing; appended).
  pub audit_outbox: Vec<AuditRecord>,
  /// The crossing sequence the next owed record takes (A-265; appended).
  pub audit_outbox_next: u64,
  /// On the control partition, what each owing partition has delivered (A-265; appended), by partition order.
  pub audit_received: Vec<crate::catalog::AuditReceivedMark>,
  /// The snapshots captured as complete immutable bases (§4.15 `capture_base`, A-291; 2026-11-18; appended), by
  /// (volume, snapshot) order.
  pub captured_bases: Vec<CapturedBase>,
  /// Each volume's carried capture (A-503; 2026-11-18; appended), by volume order.
  pub carried_captures: Vec<crate::catalog::CarriedCaptureRecord>,
  /// The repositories this partition holds (§4.17.1, A-515; appended), by id order.
  pub repositories: Vec<crate::catalog::RepositoryRecord>,
  /// Their namespaces' bindings (§4.17.1, A-515; appended), by (repository, name) order.
  pub repository_branches: Vec<crate::catalog::BranchRecord>,
  /// Every lineage's members (§4.17.1 item 4, A-575; appended), lineage by lineage in id order, each in the order it
  /// joined.
  pub repository_members: Vec<crate::catalog::MemberRecord>,
  /// The review comments of this partition's repositories (§4.17.1 shared piece 6, A-596; appended), by repository,
  /// request and number.
  pub review_comments: Vec<crate::catalog::ReviewCommentRecord>,
  /// Their approvals (A-596; appended), by repository, request and approver.
  pub review_approvals: Vec<crate::catalog::ReviewApprovalRecord>,
  /// Every repository's own state: its state logs' positions and what they hold (§4.17.1 shared piece 2, A-603 rebuilt
  /// by A-642; appended), by repository order.
  pub repository_registers: Vec<crate::catalog::RegisterRecord>,
  /// The atomic records kept, by key, each with the positions pointing to it still (A-642; appended).
  pub atomic_records: Vec<crate::catalog::AtomicHeld>,
  /// The principals' tool identities this partition keeps (§4.13; A-664; appended), by principal key.
  pub identities: Vec<IdentityRecord>,
  /// The version-control credentials this partition holds (§4.13, §4.17.2 "Access"; A-625 part 3; appended), by id.
  pub credentials: Vec<crate::catalog::CredentialRecord>,
  /// The least counter a volume id minted here may take: one past the highest counter of every volume created here
  /// (A-694; appended), so a recovery mints the ids the live partition would.
  pub volume_counter: u64,
}

/// A green adopted by its takeover successor from the successor's own holder ledger of it (§4.16 owner-loss recovery;
/// A-333): versions 0 to `version` are the merge records this node accepted as one of the green's holders and the inputs
/// it holds for them, kept while the green lives here; the green's chain holds the versions after `version`. A few dozen
/// bytes a green, where a re-recorded origin was the whole tree's bytes.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct GreenAdoption {
  /// The green.
  pub green: VolumeId,
  /// The adopted version: the green's head when this node took it over.
  pub version: u64,
  /// The green's head identity at `version`.
  pub identity: [u8; 32],
  /// Version 0's inputs manifest identity as the ledger names it; `None` for a green with no origin.
  pub origin: Option<[u8; 32]>,
}

/// A snapshot captured as a complete immutable base (§4.15 `capture_base`, A-291): every name, byte and attribute of its
/// tree is held by the volume, none read from the host directory any more, so a clone of it needs no base directory and
/// moves between fleet members as a plain volume does.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CapturedBase {
  /// The volume captured.
  pub volume: VolumeId,
  /// Its snapshot that holds the whole tree.
  pub snapshot: SnapshotId,
}

/// One green's checkpoint, as a snapshot carries it (W3; §4.16): the version it is the green's state at, and its
/// encoding (`slates_merge::checkpoint`). The green's chain holds only the increments after `version`.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct GreenCheckpoint {
  /// The green.
  pub green: VolumeId,
  /// The version it is the green's state at.
  pub version: u64,
  /// The encoded checkpoint.
  pub bytes: Vec<u8>,
}

/// A green's origin as a reference (A-217): the base volume and snapshot it was walked from, and the BLAKE3 of the
/// origin's canonical encoding, which a recovery's walk of the same snapshot must reproduce.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct OriginRef {
  /// The base volume.
  pub volume: VolumeId,
  /// The snapshot of it.
  pub snapshot: SnapshotId,
  /// The origin's identity.
  pub identity: [u8; 32],
}

/// One green's origin reference, as a snapshot carries it.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct GreenOriginRef {
  /// The green.
  pub green: VolumeId,
  /// Its origin.
  pub origin: OriginRef,
}

/// One work volume's log: what recovery replays onto the work, in order.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct WorkLog {
  /// The work volume.
  pub work: VolumeId,
  /// The encoded entries, oldest first: at most one restatement, first, then edits and declarations.
  pub entries: Vec<Vec<u8>>,
}

/// The partition.
pub struct Partition {
  caps: PartitionCaps,
  volumes: Slab<VolumeRecord>,
  by_id: Art<Handle<VolumeRecord>>,
  by_name: Art<Handle<VolumeRecord>>,
  snapshots: Slab<SnapshotRecord>,
  snapshot_index: Art<Handle<SnapshotRecord>>,
  lineage: Art<LineageEdge>,
  leases_by_holder: Art<VolumeId>,
  lease_expiry: Wheel,
  lease_timers: BTreeMap<VolumeId, slates_rt::timer::TimerId>,
  /// The earliest lease expiry entered in the wheel since the shard's deadline keeper last asked
  /// ([`Partition::take_noted_lease`]): what tells the keeper a lease was set to lapse before the deadline it sleeps for
  /// (A-536), whichever path recorded it (a take, a renewal, a replay).
  lease_noted: Option<u64>,
  attachments: Slab<AttachmentRecord>,
  attachment_index: Art<Handle<AttachmentRecord>>,
  /// The termed attachments (a CLI's, A-230) by when their term lapses, each `(expires_ns, id)`: derived from the
  /// records, made again at a restore, one entry a termed attachment, so the lapsed are found without a walk of the
  /// table (A-536).
  termed_by_expiry: BTreeSet<(u64, u64)>,
  /// The earliest term entered in [`Partition::termed_by_expiry`] since the shard's deadline keeper last asked
  /// ([`Partition::take_noted_term`]), as `lease_noted` is for leases (A-536).
  term_noted: Option<u64>,
  completions: BTreeMap<(u64, u32), ClientWindow<Vec<u8>>>,
  /// The completions a green's successor adopted from the green's merge records (A-255), each with the work whose submit
  /// it completes (A-288), by key `(origin, client, sequence)`: held while the client's window retains the completion,
  /// and dropped when the client acknowledges it. The work's route (its catalog record) is kept while one is held here
  /// ([`Partition::completions_routed_by`]). Bounded by the windows: one entry per retained completion at most.
  routed_completions: BTreeMap<(u64, u32, u32), VolumeId>,
  /// How many of `routed_completions` each work has, so the route's question is one lookup. Derived from
  /// `routed_completions` and rebuilt with it from a snapshot, which carries only each record's work.
  routed_by_work: BTreeMap<VolumeId, usize>,
  /// A scalar reservation, retained even when a client ran no recorded verb.
  client_id_high_water: u32,
  grants: BTreeMap<u64, GrantRecord>,
  /// The enrolled consumers, by id (§4.13; bounded by the enrollments a human makes, one durable record
  /// each — never grown by a workload).
  consumers: BTreeMap<u64, ConsumerRecord>,
  /// The principals' tool identities (§4.13; A-664), by principal key: one record a principal, each set whole by its
  /// owner, so bounded by the principals its owners name, never grown by a workload.
  identities: BTreeMap<Vec<u8>, IdentityRecord>,
  landing_leases: Art<LandingLeaseRecord>,
  landings: BTreeMap<u64, LandingRecord>,
  /// The audit records retained, oldest first: at most `caps.audit_records` (A-265).
  audit: std::collections::VecDeque<AuditRecord>,
  /// The audit records dropped oldest first past the bound (A-265), snapshotted with the window.
  audit_dropped: u64,
  /// The sequence the next audit record takes (A-265): a counter of the partition's own, snapshotted, so the numbers
  /// never depend on which records are retained.
  audit_next_seq: u64,
  /// The audit records this partition owes the control partition and has not had acknowledged, oldest first, each
  /// carrying its crossing sequence in `seq` (A-265's crossing, made durable 2026-11-18): kept from the atom that made
  /// it until the control partition has appended it, so a crash between the two loses none. At most
  /// `caps.audit_records`; past it the oldest leaves, counted in `audit_dropped`, as the control partition's window
  /// would drop it.
  audit_outbox: std::collections::VecDeque<AuditRecord>,
  /// The crossing sequence the next owed record takes (A-265), snapshotted: the records before it were owed already.
  audit_outbox_next: u64,
  /// On the control partition: for each partition that owes it records, the crossing sequence of the first it has not
  /// appended (A-265), snapshotted, so a batch sent again appends only what is new. One entry a partition.
  audit_received: BTreeMap<u16, u64>,
  /// The highest grant id ever issued here (A-266): the table holds live grants only, so this mark, snapshotted, is
  /// what keeps an ended grant's id from being issued again after a restart.
  grants_issued_through: u64,
  /// The least counter a volume id minted here may take (A-694): one past the highest counter of every volume created
  /// here, advanced by each creation's apply and snapshotted, so the ids an append mints for several new volumes before
  /// any is recorded never repeat, and a recovery mints what the live partition would (`crate::Db::volume_counter_at`).
  volume_counter: u64,
  /// Each green volume's merge chain (§4.16): its accepted increments after its checkpoint (all of them while it holds
  /// none), encoded, in commit order, so recovery replays them to rebuild the green. Bounded by `green_chain_bytes`
  /// against the cap.
  green_chains: BTreeMap<VolumeId, Vec<Vec<u8>>>,
  /// Each checkpointed green's checkpoint (W3; §4.16): its version and its encoding, held in place of every increment
  /// up to that version. Counted in `green_chain_bytes` against the same cap. Removed with the green.
  green_checkpoints: BTreeMap<VolumeId, (u64, Vec<u8>)>,
  /// The snapshots captured as complete immutable bases (§4.15 `capture_base`, A-291): a clone of one is made with no
  /// base directory. Bounded by the snapshots (one entry a snapshot at most); an entry leaves with its snapshot or its
  /// volume.
  captured_bases: std::collections::BTreeSet<CapturedBase>,
  /// Each captured clone's carried capture (A-503): what its move carried of its capture, shipped in its catalog, and
  /// the snapshot here it is relative to (its mark). One entry a volume at most; an entry leaves with its volume, and its
  /// mark with its snapshot.
  carried: BTreeMap<VolumeId, (crate::catalog::CarriedRecord, Option<SnapshotId>)>,
  /// The repositories and their namespaces (§4.17.1, A-515): repositories bounded by `caps.volumes`, bindings by the
  /// greens each binds.
  repositories: crate::repository::Repositories,
  /// The review records of those repositories (§4.17.1 shared piece 6, A-596): their bytes counted in
  /// `green_chain_bytes` against the merge byte budget; they leave with their review's close and their repository.
  reviews: crate::review::Reviews,
  /// The version-control credentials whose ids route here (§4.13, §4.17.2 "Access"; A-625 part 3): their bytes counted
  /// in `green_chain_bytes` against the table byte budget; each leaves with its revocation.
  credentials: crate::credentials::Credentials,
  /// Each base-seeded green's origin (§4.16): its version-0 state, encoded, replayed before its
  /// chain on recovery. Counted in `green_chain_bytes` against the same cap.
  green_origins: BTreeMap<VolumeId, Vec<u8>>,
  /// The total bytes held across all green chains, origins and checkpoints, so a new increment can be refused at the
  /// cap before it is appended (bounded growth; a checkpoint folds the increments up to it, W3). The work logs count
  /// here too: one merge byte budget per partition.
  green_chain_bytes: usize,
  /// Each work volume's log since its last restatement (§4.16, D-18): what recovery replays onto the work so an
  /// acknowledged edit survives a daemon restart. Counted in `green_chain_bytes` against the same cap; a work's
  /// restatement (an accepted submit, a rebase) replaces its log, and its destroy reclaims it.
  work_logs: BTreeMap<VolumeId, Vec<Vec<u8>>>,
  /// Each green's origin held as a reference to its base snapshot (A-217): a few dozen bytes a green, whatever its
  /// tree, where `green_origins` held the whole tree's bytes. Removed with the green.
  green_origin_refs: BTreeMap<VolumeId, OriginRef>,
  /// Each green adopted from this node's own holder ledger by its takeover (A-333): the versions up to the adoption are
  /// the ledger's, the chain holds the ones after it. A few dozen bytes a green. Removed with the green.
  green_adoptions: BTreeMap<VolumeId, GreenAdoption>,
  /// The NFSv4 file state and clients (§4.6 A-37), each bounded before any operation is issued: the
  /// owner's file state bounds its opens and locks, the listener's session table its clients, and a
  /// replay reproduces only what they admitted.
  nfs_opens: BTreeMap<[u8; 12], NfsOpenRecord>,
  nfs_delegations: BTreeMap<[u8; 12], NfsDelegationRecord>,
  seal_keys: BTreeMap<SealKeyOwner, SealKeyRecord>,
  nfs_locks: BTreeMap<[u8; 12], NfsLockRecord>,
  nfs_clients: BTreeMap<u64, NfsClientRecord>,
  /// The listener's sessions (A-127), bounded by its session table: each client's sessions under its bound.
  nfs_sessions: BTreeMap<[u8; 16], NfsSessionRecord>,
  /// The call records file-state changes carried (A-127), one per session slot, bounded by the shard's records.
  nfs_calls: BTreeMap<[u8; 20], NfsCallRecord>,
  /// The purges this shard owes its owners (2026-10-10): one per client dropped or session destroyed here and not yet
  /// acknowledged by every owner, bounded by the drops a round could not settle.
  nfs_purges: BTreeSet<NfsPurgeRecord>,
  /// The names this partition holds for volumes other partitions hold (A-236): at most one a name, and at most
  /// `caps.volumes` of them, since each stands for a volume another partition holds under the same cap.
  name_aliases: BTreeMap<String, NameAliasRecord>,
  /// Each live work's base, by green and work (A-247), on the green's partition.
  work_floors: BTreeMap<(VolumeId, VolumeId), u64>,
  /// Each work's submit awaiting its settle, at most one a work (A-247).
  pending_submits: BTreeMap<VolumeId, crate::catalog::PendingSubmitRecord>,
  /// Each work's creation awaiting its admission, by name (A-247).
  pending_creations: BTreeMap<String, crate::catalog::PendingCreationRecord>,
  /// How many works hold each version of each green, so a green's reachable floor is one look-up per verdict rather
  /// than a walk of its works (A-247: a green with 10,000 works is judged once per accept).
  work_floor_counts: BTreeMap<(VolumeId, u64), u64>,
  /// The CLI attachments whose term lapsed, oldest first (A-230), so a later use is refused as expired rather than as
  /// unknown. At most `caps.attachments` of them: the oldest leaves when a lapse would pass it.
  expired_attachments: std::collections::VecDeque<u64>,
  nfs_instance: u32,
  /// The destroyed volumes whose tombstone this owner still owes its candidate holders (AUD-29-43), by volume,
  /// each with the register sequence it is written at. A tombstone keeps its volume's slot of the volume
  /// capacity until it is retired, so the table is bounded by the same derived cap.
  tombstones: BTreeMap<VolumeId, u64>,
}

impl std::fmt::Debug for Partition {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Partition")
      .field("volumes", &self.by_id.len())
      .field("snapshots", &self.snapshot_index.len())
      .field("attachments", &self.attachment_index.len())
      .field("grants", &self.grants.len())
      .field("landings", &self.landings.len())
      .finish()
  }
}

fn snapshot_key(volume: VolumeId, id: SnapshotId) -> Vec<u8> {
  let mut key = volume.bytes.to_vec();
  key.extend_from_slice(&id.value.to_be_bytes());
  key
}

impl Partition {
  /// An empty partition with `caps`, its wheel starting at `now_ns`.
  pub fn new(caps: PartitionCaps, now_ns: u64) -> Partition {
    Partition {
      caps,
      volumes: Slab::new(caps.segment_slots, caps.volumes),
      by_id: Art::new(),
      by_name: Art::new(),
      snapshots: Slab::new(caps.segment_slots, caps.snapshots),
      snapshot_index: Art::new(),
      lineage: Art::new(),
      leases_by_holder: Art::new(),
      lease_expiry: Wheel::new(caps.tick_ns, caps.timers, now_ns),
      lease_timers: BTreeMap::new(),
      lease_noted: None,
      termed_by_expiry: BTreeSet::new(),
      term_noted: None,
      attachments: Slab::new(caps.segment_slots, caps.attachments),
      attachment_index: Art::new(),
      completions: BTreeMap::new(),
      routed_completions: BTreeMap::new(),
      routed_by_work: BTreeMap::new(),
      client_id_high_water: 0,
      grants: BTreeMap::new(),
      consumers: BTreeMap::new(),
      identities: BTreeMap::new(),
      nfs_opens: BTreeMap::new(),
      nfs_delegations: BTreeMap::new(),
      seal_keys: BTreeMap::new(),
      nfs_locks: BTreeMap::new(),
      nfs_clients: BTreeMap::new(),
      nfs_sessions: BTreeMap::new(),
      nfs_calls: BTreeMap::new(),
      nfs_purges: BTreeSet::new(),
      name_aliases: BTreeMap::new(),
      work_floors: BTreeMap::new(),
      pending_submits: BTreeMap::new(),
      pending_creations: BTreeMap::new(),
      work_floor_counts: BTreeMap::new(),
      expired_attachments: std::collections::VecDeque::new(),
      nfs_instance: 0,
      landing_leases: Art::new(),
      landings: BTreeMap::new(),
      audit: std::collections::VecDeque::new(),
      audit_dropped: 0,
      green_chains: BTreeMap::new(),
      green_origins: BTreeMap::new(),
      green_chain_bytes: 0,
      work_logs: BTreeMap::new(),
      green_origin_refs: BTreeMap::new(),
      green_adoptions: BTreeMap::new(),
      green_checkpoints: BTreeMap::new(),
      audit_next_seq: 0,
      audit_outbox: std::collections::VecDeque::new(),
      audit_outbox_next: 0,
      audit_received: BTreeMap::new(),
      grants_issued_through: 0,
      volume_counter: 0,
      captured_bases: std::collections::BTreeSet::new(),
      carried: BTreeMap::new(),
      repositories: crate::repository::Repositories::new(),
      reviews: crate::review::Reviews::new(),
      credentials: crate::credentials::Credentials::new(),
      tombstones: BTreeMap::new(),
    }
  }

  /// The version-control credentials whose ids route to this partition (§4.13, §4.17.2 "Access"; A-625 part 3): what
  /// the HTTP edge's authenticator and the SSH edge's key holders look a presented credential up in.
  pub fn credentials(&self) -> &crate::credentials::Credentials {
    &self.credentials
  }

  /// The repositories this partition holds and their namespaces (§4.17.1, A-515).
  pub fn repositories(&self) -> &crate::repository::Repositories {
    &self.repositories
  }

  /// The review records of this partition's repositories (§4.17.1 shared piece 6, A-596).
  pub fn reviews(&self) -> &crate::review::Reviews {
    &self.reviews
  }

  /// What the catalog holds under `id`, as a repository's namespace asks it (§4.17.1: a namespace binds greens).
  fn held(&self, id: VolumeId) -> crate::repository::Held {
    match self.volume(id) {
      None => crate::repository::Held::Absent,
      Some(record) if matches!(record.policy.role, crate::catalog::Role::Green { .. }) => {
        crate::repository::Held::Green
      }
      Some(_) => crate::repository::Held::NotGreen,
    }
  }

  /// The register sequence a destroy of `id` would close its registers at (AUD-29-43): one past its head
  /// epoch and, for a green, past its newest version (version 0 is the origin, version `n` the `n`-th chain
  /// entry), so the tombstone is newer than every value the volume's registers ever held. `None` when the
  /// volume does not exist or the sequence is spent.
  pub fn tombstone_sequence(&self, id: VolumeId) -> Option<u64> {
    let record = self.volume(id)?;
    // The green's head, not its chain's length: the chain holds only the versions after a checkpoint (A-238) or an
    // adoption (A-333), and a tombstone at or below a version its holders accepted would not be newer than it.
    record.epoch.max(self.green_head(id)).checked_add(1)
  }

  /// The tombstone this owner still owes for the destroyed volume `id`, if any.
  pub fn tombstone(&self, id: VolumeId) -> Option<Tombstone> {
    self.tombstones.get(&id).map(|sequence| Tombstone {
      volume: id,
      sequence: *sequence,
    })
  }

  /// Every tombstone this owner still owes, by volume order.
  pub fn tombstones(&self) -> impl Iterator<Item = Tombstone> + '_ {
    self.tombstones.iter().map(|(volume, sequence)| Tombstone {
      volume: *volume,
      sequence: *sequence,
    })
  }

  /// Volumes and owed tombstones together: what the volume capacity bounds.
  fn volume_slots_used(&self) -> usize {
    self.by_id.len().saturating_add(self.tombstones.len())
  }

  /// The caps.
  pub fn caps(&self) -> PartitionCaps {
    self.caps
  }

  /// A green volume's merge chain (§4.16): its accepted increments after its checkpoint
  /// ([`Partition::green_checkpoint`]; all of them while it holds none), encoded, in commit order, so the chain's
  /// entry `n` is version `checkpoint + n + 1`. Empty for a green with no version past its checkpoint and for any
  /// non-green volume.
  pub fn green_chain(&self, green: VolumeId) -> &[Vec<u8>] {
    self.green_chains.get(&green).map_or(&[], Vec::as_slice)
  }

  /// A green's checkpoint (W3; §4.16): the version it is the green's state at and its encoding, or `None` while the
  /// green holds none (and for any non-green volume).
  pub fn green_checkpoint(&self, green: VolumeId) -> Option<(u64, &[u8])> {
    self
      .green_checkpoints
      .get(&green)
      .map(|(version, bytes)| (*version, bytes.as_slice()))
  }

  /// A green's head as its durable records hold it: its floor ([`Partition::green_floor`]) and the increments after it.
  pub fn green_head(&self, green: VolumeId) -> u64 {
    let chained = u64::try_from(self.green_chain(green).len()).unwrap_or(u64::MAX);
    self.green_floor(green).saturating_add(chained)
  }

  /// The version a green's chain begins after: its checkpoint's version (W3) once it holds one, else the version it was
  /// adopted at by its takeover (A-333), else 0. The chain's entry `n` is version `floor + n + 1`. A checkpoint of an
  /// adopted green is always past its adoption ([`Partition::green_checkpoint`]'s check), so the newer of the two is the
  /// checkpoint's whenever both stand.
  pub fn green_floor(&self, green: VolumeId) -> u64 {
    match (
      self.green_checkpoints.get(&green),
      self.green_adoptions.get(&green),
    ) {
      (Some((version, _)), _) => *version,
      (None, Some(adoption)) => adoption.version,
      (None, None) => 0,
    }
  }

  /// A green's adoption by its takeover successor (A-333), if it was adopted here from this node's holder ledger.
  pub fn green_adoption(&self, green: VolumeId) -> Option<&GreenAdoption> {
    self.green_adoptions.get(&green)
  }

  /// What a checkpoint at `version` folds of `green`'s records: the increments it drops and the bytes it releases (the
  /// increments and the checkpoint it replaces), or the refusal when `version` is not past the checkpoint held or is
  /// past the head (W3).
  fn checkpoint_folds(&self, green: VolumeId, version: u64) -> Result<(usize, usize), DbError> {
    // An adopted green's chain begins after its adoption (A-333), which no checkpoint may fold below.
    let held = self.green_floor(green);
    let held_bytes = self
      .green_checkpoints
      .get(&green)
      .map_or(0, |(_, bytes)| bytes.len());
    let chain = self.green_chain(green);
    let out_of_range = DbError::CheckpointOutOfRange {
      version,
      held,
      head: self.green_head(green),
    };
    let folded = version
      .checked_sub(held)
      .filter(|folded| *folded > 0)
      .and_then(|folded| usize::try_from(folded).ok())
      .filter(|folded| *folded <= chain.len())
      .ok_or(out_of_range)?;
    let dropped: usize = chain.iter().take(folded).map(Vec::len).sum();
    Ok((folded, dropped.saturating_add(held_bytes)))
  }

  /// The check of a green's checkpoint (W3): the green exists, the version is in range, and the bytes fit the merge
  /// budget. A checkpoint no larger than what it releases is admitted at any fill, so a partition at its cap can
  /// always fold back under it.
  fn check_green_checkpoint(
    &self,
    green: VolumeId,
    version: u64,
    bytes: usize,
  ) -> Result<(), DbError> {
    self.volume(green).ok_or(DbError::NotFound)?;
    let (_, released) = self.checkpoint_folds(green, version)?;
    if bytes <= released {
      return Ok(());
    }
    self.merge_bytes_fit(released, bytes)
  }

  /// Applies a green's checkpoint (W3): drops the increments up to its version and the checkpoint it replaces, and
  /// holds it. Unconditional, as every apply is: a version the check would refuse folds nothing it does not hold.
  fn apply_green_checkpoint(&mut self, green: VolumeId, version: u64, bytes: &[u8]) {
    let (folded, released) = self.checkpoint_folds(green, version).unwrap_or((0, 0));
    if let Some(chain) = self.green_chains.get_mut(&green) {
      chain.drain(..folded.min(chain.len()));
      if chain.is_empty() {
        self.green_chains.remove(&green);
      }
    }
    self.green_chain_bytes = self
      .green_chain_bytes
      .saturating_sub(released)
      .saturating_add(bytes.len());
    self
      .green_checkpoints
      .insert(green, (version, bytes.to_vec()));
    // The green's log below the checkpoint is folded: its pointers to atomic records there go with it (A-642).
    self.repositories.fold_atomic(green, version);
  }

  /// A green volume's encoded origin (§4.16), or `None` for a green that started from scratch
  /// (and for any non-green volume).
  pub fn green_origin(&self, green: VolumeId) -> Option<&[u8]> {
    self.green_origins.get(&green).map(Vec::as_slice)
  }

  // ------------------------------------------------------------------ reads

  /// A volume by id.
  pub fn volume(&self, id: VolumeId) -> Option<&VolumeRecord> {
    let h = *self.by_id.get(&id.bytes)?;
    self.volumes.get(h).ok()
  }

  /// Whether attachment `id` was a CLI attachment whose term lapsed (A-230), among the most recent the partition keeps.
  pub fn attachment_expired(&self, id: u64) -> bool {
    self.expired_attachments.contains(&id)
  }

  /// The oldest version any live work of `green` is based on (A-247), or `None` when it has none.
  pub fn oldest_work_floor(&self, green: VolumeId) -> Option<u64> {
    self
      .work_floor_counts
      .range((green, 0)..=(green, u64::MAX))
      .next()
      .map(|((_, base), _)| *base)
  }

  /// The work creation pending under `name`, when there is one (A-247).
  pub fn pending_creation(&self, name: &str) -> Option<&crate::catalog::PendingCreationRecord> {
    self.pending_creations.get(name)
  }

  /// Every work creation pending on this partition (A-247).
  pub fn pending_creations(&self) -> impl Iterator<Item = &crate::catalog::PendingCreationRecord> {
    self.pending_creations.values()
  }

  /// The work's submit awaiting its settle, when it has one (A-247).
  pub fn pending_submit(&self, work: VolumeId) -> Option<&crate::catalog::PendingSubmitRecord> {
    self.pending_submits.get(&work)
  }

  /// Every work's hold on a green of this partition (A-247), for the orphan check at recovery.
  pub fn work_floors(&self) -> impl Iterator<Item = crate::catalog::WorkFloorRecord> + '_ {
    self
      .work_floors
      .iter()
      .map(|((green, work), base)| crate::catalog::WorkFloorRecord {
        green: *green,
        work: *work,
        base: *base,
      })
  }

  /// The work's floor on its green, when it holds one (A-247).
  pub fn work_floor(&self, green: VolumeId, work: VolumeId) -> Option<u64> {
    self.work_floors.get(&(green, work)).copied()
  }

  /// Records `work`'s hold of version `base` of `green`, in place of any it had (A-247), keeping the counts over them.
  fn set_work_floor(&mut self, green: VolumeId, work: VolumeId, base: u64) {
    if let Some(previous) = self.work_floors.insert((green, work), base) {
      self.release_floor_count(green, previous);
    }
    let held = self.work_floor_counts.entry((green, base)).or_insert(0);
    *held = held.saturating_add(1);
  }

  /// The guard of a work's records on a partition (A-247). A floor is one per live work, so it is bounded as the volumes
  /// are; a drop of none is a no-op (a destroy's message may meet the recovery's orphan check). A pending submit is one a
  /// work: a second while one waits is refused, so the first is never lost.
  fn check_work_record(&self, op: &Op) -> Result<(), DbError> {
    match op {
      Op::WorkFloorSet { green, work, .. } => {
        if self.work_floors.contains_key(&(*green, *work))
          || self.work_floors.len() < self.caps.volumes
        {
          Ok(())
        } else {
          Err(DbError::Capacity {
            table: "work floors",
          })
        }
      }
      // A name is held once on its partition: by a volume, an alias, or one pending creation.
      Op::WorkCreationPending { record } => {
        let held = self
          .volume_by_name(&record.name)
          .map(|volume| volume.id)
          .or_else(|| self.name_alias(&record.name).map(|alias| alias.volume))
          .or_else(|| {
            self
              .pending_creations
              .get(&record.name)
              .filter(|pending| *pending != record)
              .map(|pending| pending.work)
          });
        match held {
          Some(existing) => Err(DbError::AlreadyExists {
            existing: existing.bytes,
          }),
          None if self.pending_creations.len() >= self.caps.volumes => Err(DbError::Capacity {
            table: "pending work creations",
          }),
          None => Ok(()),
        }
      }
      Op::WorkSubmitPending { record } => match self.pending_submits.get(&record.work) {
        Some(pending) if pending != record => Err(DbError::AlreadyExists {
          existing: record.work.bytes,
        }),
        _ => Ok(()),
      },
      _ => Ok(()),
    }
  }

  /// Applies a work's floor or pending-submit record (A-247).
  /// Erases volume `id`'s keys with its record (A-92; seal.md §3.1, cryptographic erase): its lineage and naming keys,
  /// whose one wrapped copy was here, so every chunk sealed under them, on any holder, opens for no one after this; and
  /// its tenant's key when `tenant` has no other volume on this partition (a tenant key wraps only its volumes' lineage
  /// keys). The scan is bounded by the partition's volume cap, once per destroy or release.
  fn erase_volume_keys(&mut self, id: VolumeId, tenant: Option<u64>) {
    self.seal_keys.remove(&SealKeyOwner::Lineage { volume: id });
    self.seal_keys.remove(&SealKeyOwner::Naming { volume: id });
    if let Some(account) = tenant
      && !self
        .volumes()
        .iter()
        .any(|volume| volume.owner.tenant() == account)
    {
      self.seal_keys.remove(&SealKeyOwner::Tenant { account });
    }
  }

  /// A moved volume's release (A-291): as a destroy's apply, but no tombstone, since the volume's registers are the
  /// member's it moved to; this node's copies of its keys go with its record, its new owner holding its own.
  fn release_volume(&mut self, id: VolumeId) -> Result<(), DbError> {
    let tenant = self.volume(id).map(|volume| volume.owner.tenant());
    self.remove_volume(id)?;
    self.erase_volume_keys(id, tenant);
    Ok(())
  }

  fn apply_work_floor(&mut self, op: &Op) {
    match op {
      Op::WorkFloorSet { green, work, base } => self.set_work_floor(*green, *work, *base),
      Op::WorkFloorDropped { green, work } => {
        if let Some(base) = self.work_floors.remove(&(*green, *work)) {
          self.release_floor_count(*green, base);
        }
      }
      Op::WorkSubmitPending { record } => {
        self.pending_submits.insert(record.work, record.clone());
      }
      Op::WorkSubmitSettled { work } => {
        self.pending_submits.remove(work);
      }
      Op::WorkCreationPending { record } => {
        self
          .pending_creations
          .insert(record.name.clone(), record.clone());
      }
      Op::WorkCreationSettled { name } => {
        self.pending_creations.remove(name);
      }
      _ => {}
    }
  }

  /// Releases one work's hold of `base`.
  fn release_floor_count(&mut self, green: VolumeId, base: u64) {
    if let Some(held) = self.work_floor_counts.get_mut(&(green, base)) {
      *held = held.saturating_sub(1);
      if *held == 0 {
        self.work_floor_counts.remove(&(green, base));
      }
    }
  }

  /// The alias this partition holds for `name`, if any (A-236): a name another partition's volume holds.
  pub fn name_alias(&self, name: &str) -> Option<&NameAliasRecord> {
    self.name_aliases.get(name)
  }

  /// Every alias this partition holds, by name (A-236): what a recovery reconciles.
  pub fn name_aliases(&self) -> impl Iterator<Item = &NameAliasRecord> {
    self.name_aliases.values()
  }

  /// A volume by name.
  pub fn volume_by_name(&self, name: &str) -> Option<&VolumeRecord> {
    let h = *self.by_name.get(name.as_bytes())?;
    self.volumes.get(h).ok()
  }

  /// The first `limit` volumes past `after` (from the first when `None`) in volume-id order (§4.4 `list`, A-267): a
  /// bounded ordered read of the id index ([`crate::art::Art::after`]), so a page of the listing costs the volumes it
  /// passes, never the table.
  pub fn volumes_after(&self, after: Option<VolumeId>, limit: usize) -> Vec<&VolumeRecord> {
    self
      .by_id
      .after(after.as_ref().map(|id| id.bytes.as_slice()), limit)
      .into_iter()
      .filter_map(|(_, h)| self.volumes.get(*h).ok())
      .collect()
  }

  /// Every volume, by id order.
  pub fn volumes(&self) -> Vec<&VolumeRecord> {
    self
      .by_id
      .iter()
      .filter_map(|(_, h)| self.volumes.get(*h).ok())
      .collect()
  }

  /// Volumes held.
  pub fn volume_count(&self) -> usize {
    self.by_id.len()
  }

  /// A snapshot.
  pub fn snapshot(&self, volume: VolumeId, id: SnapshotId) -> Option<&SnapshotRecord> {
    let h = *self.snapshot_index.get(&snapshot_key(volume, id))?;
    self.snapshots.get(h).ok()
  }

  /// A volume's snapshots, by id order.
  pub fn snapshots_of(&self, volume: VolumeId) -> Vec<&SnapshotRecord> {
    self
      .snapshot_index
      .scan_prefix(&volume.bytes)
      .into_iter()
      .filter_map(|(_, h)| self.snapshots.get(*h).ok())
      .collect()
  }

  /// Whether `volume`'s `snapshot` was captured as a complete immutable base (§4.15 `capture_base`, A-291), so a clone
  /// of it is made with no base directory.
  pub fn is_captured(&self, volume: VolumeId, snapshot: SnapshotId) -> bool {
    self
      .captured_bases
      .contains(&CapturedBase { volume, snapshot })
  }

  /// What a captured clone's move carried of its capture (A-503), with the snapshot here it is relative to (its mark),
  /// when this node holds one.
  pub fn carried(
    &self,
    volume: VolumeId,
  ) -> Option<(&crate::catalog::CarriedRecord, Option<SnapshotId>)> {
    self
      .carried
      .get(&volume)
      .map(|(carried, mark)| (carried, *mark))
  }

  /// A clone's lineage edge.
  pub fn lineage(&self, child: VolumeId) -> Option<&LineageEdge> {
    self.lineage.get(&child.bytes)
  }

  /// Whether a volume of this partition is a clone of `origin` (its lineage edge names it): a clone shares its origin's
  /// records here, so the origin cannot leave this node while one stands (A-291). A scan bounded by the partition's
  /// volume cap.
  pub fn has_clones(&self, origin: VolumeId) -> bool {
    self
      .lineage
      .iter()
      .any(|(_, edge)| edge.origin_volume == origin)
  }

  /// The volume a principal holds a lease on.
  pub fn lease_of(&self, holder: &Principal) -> Option<VolumeId> {
    self.leases_by_holder.get(&holder.key()).copied()
  }

  /// The attachment ids held by one SDK client (a walk of the table, bounded by its cap; the
  /// reclaim of a dead client asks, never a hot path).
  pub fn attachments_of_client(&self, client: u32) -> Vec<u64> {
    self
      .attachments
      .iter()
      .filter(|(_, a)| matches!(a.consumer, Consumer::Sdk { client: c } if c == client))
      .map(|(_, a)| a.id)
      .collect()
  }

  /// The attachments an enrolled consumer holds on this partition, whatever their volume or form (§4.13: a
  /// consumer's revocation ends every one of them, AUD-29-84). A walk of the table, bounded by its cap; a
  /// revocation asks, never a hot path.
  pub fn attachments_held_by_consumer(&self, consumer: u64) -> Vec<&AttachmentRecord> {
    self
      .attachments
      .iter()
      .filter(
        |(_, a)| matches!(a.principal, Principal::Consumer { consumer: c, .. } if c == consumer),
      )
      .map(|(_, a)| a)
      .collect()
  }

  /// Every attachment this partition holds, in slab order: what a sweep over all of them walks (A-230's lapse).
  pub fn attachments(&self) -> impl Iterator<Item = &AttachmentRecord> {
    self.attachments.iter().map(|(_, record)| record)
  }

  /// The attachments other hosts' clients hold here (§4.10, A-222: those of a host that left the configuration end).
  /// A walk of the table, bounded by its cap; a configuration's install asks, never a hot path.
  pub fn remote_attachments(&self) -> Vec<&AttachmentRecord> {
    self
      .attachments
      .iter()
      .filter(|(_, a)| matches!(a.consumer, Consumer::Remote { .. }))
      .map(|(_, a)| a)
      .collect()
  }

  /// The attachments of a volume (a walk of the table, bounded by its cap; a detach and a
  /// recovery ask, never a hot path).
  pub fn attachments_of(&self, volume: VolumeId) -> Vec<&AttachmentRecord> {
    self
      .attachments
      .iter()
      .filter(|(_, a)| a.volume == volume)
      .map(|(_, a)| a)
      .collect()
  }

  /// A binding borrows one live bridge mount of the same volume and principal, within its
  /// rights (§4.6). This guard also prevents dependency chains and cycles before the append.
  fn check_mount_dependency(&self, record: &AttachmentRecord) -> Result<(), DbError> {
    let Consumer::Mount { attachment } = record.consumer else {
      return Ok(());
    };
    let parent = self.attachment(attachment).ok_or(DbError::NotFound)?;
    if !matches!(parent.consumer, Consumer::Bridge)
      || parent.volume != record.volume
      || parent.principal != record.principal
      || !matches!(record.form, crate::catalog::AttachForm::Oci { .. })
      || record.snapshot.is_some()
      || (record.rights.read && !parent.rights.read)
      || (record.rights.write && !parent.rights.write)
      || record.rights.admin
    {
      return Err(DbError::NotFound);
    }
    Ok(())
  }

  /// Removes one already-checked attachment while applying its recorded transition.
  fn remove_attachment(&mut self, id: u64) -> Result<(), DbError> {
    let handle = self
      .attachment_index
      .remove(&id.to_be_bytes())
      .ok_or(DbError::NotFound)?;
    let record = self.attachments.remove(handle)?;
    self.unindex_term(&record);
    Ok(())
  }

  /// Enters a termed attachment's lapse in [`Partition::termed_by_expiry`] and notes it for the keeper (A-536); any
  /// other kind holds no term.
  fn index_term(&mut self, record: &AttachmentRecord) {
    if let Consumer::Termed { expires_ns } = record.consumer {
      self.termed_by_expiry.insert((expires_ns, record.id));
      self.term_noted = Some(
        self
          .term_noted
          .map_or(expires_ns, |noted| noted.min(expires_ns)),
      );
    }
  }

  /// Takes a termed attachment's lapse out of [`Partition::termed_by_expiry`].
  fn unindex_term(&mut self, record: &AttachmentRecord) {
    if let Consumer::Termed { expires_ns } = record.consumer {
      self.termed_by_expiry.remove(&(expires_ns, record.id));
    }
  }

  /// The termed attachments whose term lapsed by `now_ns`, read from their index alone (A-536): the caller ends each.
  pub fn lapsed_terms(&self, now_ns: u64) -> Vec<u64> {
    self
      .termed_by_expiry
      .iter()
      .take_while(|(expires_ns, _)| *expires_ns <= now_ns)
      .map(|(_, id)| *id)
      .collect()
  }

  /// When the next termed attachment lapses, the deadline the shard's keeper sleeps until for them (A-536).
  pub fn next_term_lapse(&self) -> Option<u64> {
    self
      .termed_by_expiry
      .first()
      .map(|(expires_ns, _)| *expires_ns)
  }

  /// The earliest term entered since the last call, taken: the keeper wakes for it when it sleeps for a later deadline
  /// (A-536).
  pub fn take_noted_term(&mut self) -> Option<u64> {
    self.term_noted.take()
  }

  /// The highest attachment id the partition holds, or `None` when it holds none (a walk of the table,
  /// bounded by its cap; asked once at boot, so a fresh counter never re-mints a recovered id).
  pub fn highest_attachment(&self) -> Option<u64> {
    self.attachments.iter().map(|(_, a)| a.id).max()
  }

  /// An attachment.
  pub fn attachment(&self, id: u64) -> Option<&AttachmentRecord> {
    let h = *self.attachment_index.get(&id.to_be_bytes())?;
    self.attachments.get(h).ok()
  }

  /// What a client's window knows about a sequence. The key is `(origin, client)`: `origin` is this node's
  /// own host for a local client and the authenticated forwarding peer for a cross-node forwarded verb, so a
  /// forwarded request never collides with a local client sharing its per-node id (§4.8 "Lookup").
  pub fn completion(&self, origin: u64, client: u32, sequence: u32) -> Seen<Vec<u8>> {
    self
      .completions
      .get(&(origin, client))
      .map_or(Seen::New, |w| w.lookup(sequence))
  }

  /// How many completion records a client's window retains unacknowledged here (zero for a client with none).
  pub fn retained_completions(&self, origin: u64, client: u32) -> usize {
    self
      .completions
      .get(&(origin, client))
      .map_or(0, ClientWindow::retained)
  }

  /// The ids at or below a client's watermark it still owes (its window's open ids), in order: what an origin relays
  /// beside the watermark with a forwarded write, so the owner keeps them open too (§4.9).
  pub fn owed_completions(&self, origin: u64, client: u32) -> Vec<u32> {
    self
      .completions
      .get(&(origin, client))
      .map_or_else(Vec::new, |w| w.owed().collect())
  }

  /// How many completions this partition retains that a green's successor adopted for `work`'s submits and a resend can
  /// reach only through the work's route (A-288): while any is retained, a resend of one is answered here, so the work's
  /// route (its catalog record) is kept; at zero nothing the route leads to can answer, and its adopter retires it. Zero
  /// for every work that has none here.
  pub fn completions_routed_by(&self, work: VolumeId) -> usize {
    self.routed_by_work.get(&work).copied().unwrap_or(0)
  }

  /// The highest sequence a client has acknowledged (its completion window's watermark), or `None` if it has
  /// acknowledged nothing. The origin reads its local client's watermark (`origin` = its own host) to relay
  /// it with a forwarded write, so the owner prunes the forwarded client's completions the same way a local
  /// client's are pruned — by acknowledgement, not left to grow (§4.9 RIFL; banned item 8).
  pub fn acknowledged_up_to(&self, origin: u64, client: u32) -> Option<u32> {
    self
      .completions
      .get(&(origin, client))
      .and_then(ClientWindow::acknowledged_up_to)
  }

  /// The allocation floor after recovery: issued ids and this host's retained completion
  /// identities. Foreign hosts' client numbers never advance the local allocator.
  pub fn client_id_high_water(&self, origin: u64) -> u32 {
    self
      .completions
      .range((origin, 0)..=(origin, u32::MAX))
      .next_back()
      .map_or(self.client_id_high_water, |((_, client), _)| {
        self.client_id_high_water.max(*client)
      })
  }

  /// `principal`'s grants past grant id `after` (`None`: from the first), in grant-id order (§4.13, A-266, A-267): a
  /// range from the cursor over the id-ordered table, so a page of a principal's grants costs the grants it passes, never
  /// the whole table; collecting every grant of the principal and then skipping to the cursor cost the table each page.
  pub fn grants_of_after<'a>(
    &'a self,
    principal: &'a Principal,
    after: Option<u64>,
  ) -> impl Iterator<Item = &'a GrantRecord> + 'a {
    let from = match after {
      Some(after) => std::ops::Bound::Excluded(after),
      None => std::ops::Bound::Unbounded,
    };
    self
      .grants
      .range((from, std::ops::Bound::Unbounded))
      .map(|(_, grant)| grant)
      .filter(move |grant| &grant.principal == principal)
  }

  /// A grant.
  pub fn grant(&self, id: u64) -> Option<&GrantRecord> {
    self.grants.get(&id)
  }

  /// Every grant, by id order: what a restarted shard rebuilds its runtime grants from (AUD-29-06).
  pub fn grants(&self) -> impl Iterator<Item = &GrantRecord> {
    self.grants.values()
  }

  /// A grant's state change: at a terminal step (consumed, expired, revoked) it leaves the table (A-266), which holds
  /// live grants; its history is the audit log.
  fn change_grant_state(&mut self, id: u64, state: GrantState) -> Result<(), DbError> {
    if matches!(
      state,
      GrantState::Consumed | GrantState::Expired | GrantState::Revoked
    ) {
      self.grants.remove(&id).ok_or(DbError::NotFound)?;
    } else {
      self.grants.get_mut(&id).ok_or(DbError::NotFound)?.state = state;
    }
    Ok(())
  }

  /// The highest grant id ever issued here (A-266), whatever grants have since ended and left the table.
  pub fn grants_issued_through(&self) -> u64 {
    self.grants_issued_through
  }

  /// The least counter a volume id minted here may take (A-694): one past the highest counter of every volume created
  /// here.
  pub fn volume_counter(&self) -> u64 {
    self.volume_counter
  }

  /// An enrolled consumer's record (§4.13), or `None` if no such consumer was ever enrolled.
  pub fn consumer(&self, id: u64) -> Option<&ConsumerRecord> {
    self.consumers.get(&id)
  }

  /// A principal's tool identities (§4.13; A-664), or `None` when its owner never set them here.
  pub fn identities(&self, principal: &Principal) -> Option<&IdentityRecord> {
    self.identities.get(&principal.key())
  }

  /// A principal's identities set as the version after its current one (A-664), the first setting's 1: a setting made
  /// from a stale read is refused `PolicyChanged` with the version now, and changes nothing.
  fn check_identities(&self, record: &IdentityRecord) -> Result<(), DbError> {
    let current = self
      .identities(&record.principal)
      .map_or(0, |held| held.version);
    if current.checked_add(1) == Some(record.version) {
      Ok(())
    } else {
      Err(DbError::PolicyChanged { current })
    }
  }

  /// The NFSv4 opens held at this partition's files (§4.6 A-37).
  pub fn nfs_opens(&self) -> impl Iterator<Item = &NfsOpenRecord> {
    self.nfs_opens.values()
  }

  /// The NFSv4 delegations recorded at this partition's files (§4.6 A-78).
  pub fn nfs_delegations(&self) -> impl Iterator<Item = &NfsDelegationRecord> {
    self.nfs_delegations.values()
  }

  /// The NFSv4 lock states held at this partition's files.
  pub fn nfs_locks(&self) -> impl Iterator<Item = &NfsLockRecord> {
    self.nfs_locks.values()
  }

  /// The NFSv4 clients this partition's listener holds.
  pub fn nfs_clients(&self) -> impl Iterator<Item = &NfsClientRecord> {
    self.nfs_clients.values()
  }

  /// The NFSv4.1 sessions this partition's listener holds (A-127).
  pub fn nfs_sessions(&self) -> impl Iterator<Item = &NfsSessionRecord> {
    self.nfs_sessions.values()
  }

  /// The NFSv4.1 call records this partition's file-state changes carried (A-127).
  pub fn nfs_calls(&self) -> impl Iterator<Item = &NfsCallRecord> {
    self.nfs_calls.values()
  }

  /// The NFSv4 purges this partition's shard owes its owners (2026-10-10).
  pub fn nfs_purges(&self) -> impl Iterator<Item = &NfsPurgeRecord> {
    self.nfs_purges.iter()
  }

  /// The partition's NFSv4 instance: the last one a daemon life advanced to.
  pub fn nfs_instance(&self) -> u32 {
    self.nfs_instance
  }

  /// The sealing key recorded for `owner` at this partition, wrapped by its parent (A-92).
  pub fn seal_key(&self, owner: &SealKeyOwner) -> Option<&SealKeyRecord> {
    self.seal_keys.get(owner)
  }

  /// Every sealing key recorded at this partition, by owner.
  pub fn seal_keys(&self) -> impl Iterator<Item = &SealKeyRecord> {
    self.seal_keys.values()
  }

  /// Applies one NFSv4 record operation (A-37): an upsert, a clear, or a client's purge.
  fn apply_nfs(&mut self, op: &Op) {
    match op {
      Op::NfsOpenSet { record } => {
        self.nfs_opens.insert(record.other, record.clone());
      }
      Op::NfsOpenCleared { other } => {
        self.nfs_opens.remove(other);
      }
      Op::NfsDelegationSet { record } => {
        self.nfs_delegations.insert(record.other, record.clone());
      }
      Op::NfsDelegationCleared { other } => {
        self.nfs_delegations.remove(other);
      }
      Op::NfsLockSet { record } => {
        self.nfs_locks.insert(record.other, record.clone());
      }
      Op::NfsLockCleared { other } => {
        self.nfs_locks.remove(other);
      }
      Op::NfsClientStateCleared { clientid } => {
        self
          .nfs_opens
          .retain(|_, record| record.clientid != *clientid);
        self
          .nfs_locks
          .retain(|_, record| record.clientid != *clientid);
        self
          .nfs_delegations
          .retain(|_, record| record.clientid != *clientid);
        // Its call records go with its state (A-127).
        self
          .nfs_calls
          .retain(|_, record| record.clientid != *clientid);
      }
      Op::NfsClientSet { record } => {
        self.nfs_clients.insert(record.clientid, record.clone());
      }
      Op::NfsClientCleared { clientid } => {
        self.nfs_clients.remove(clientid);
        // A dropped client's sessions go with it (A-127).
        self
          .nfs_sessions
          .retain(|_, record| record.clientid != *clientid);
      }
      Op::NfsSessionSet { record } => {
        self.nfs_sessions.insert(record.sessionid, record.clone());
      }
      Op::NfsSessionCleared { sessionid } => {
        self.nfs_sessions.remove(sessionid);
      }
      Op::NfsCallSet { record } => {
        self.nfs_calls.insert(record.key, record.clone());
      }
      Op::NfsCallsCleared { sessionid } => {
        self
          .nfs_calls
          .retain(|key, _| key.get(..sessionid.len()) != Some(&sessionid[..]));
      }
      Op::NfsPurgeOwed { record } => {
        self.nfs_purges.insert(*record);
      }
      Op::NfsPurgeSettled { record } => {
        self.nfs_purges.remove(record);
      }
      Op::NfsInstanceAdvanced { instance } => {
        self.nfs_instance = self.nfs_instance.max(*instance);
      }
      _ => {}
    }
  }
}

/// A consumer id in the 16-byte `existing` shape `AlreadyExists` carries (the id in the low eight
/// bytes, little-endian), so a duplicate enrollment names the consumer it collided with.
fn consumer_id_bytes(consumer: u64) -> [u8; 16] {
  let mut out = [0u8; 16];
  out[..8].copy_from_slice(&consumer.to_le_bytes());
  out
}

impl Partition {
  /// The landing lease on a target.
  pub fn landing_lease(&self, target: &str) -> Option<&LandingLeaseRecord> {
    self.landing_leases.get(target.as_bytes())
  }

  /// Every landing lease record (the control partition's, §4.15; AUD-29-03).
  pub fn landing_leases(&self) -> impl Iterator<Item = &LandingLeaseRecord> {
    self.landing_leases.iter().map(|(_, lease)| lease)
  }

  /// A landing.
  pub fn landing(&self, id: u64) -> Option<&LandingRecord> {
    self.landings.get(&id)
  }

  /// The highest landing id the partition holds, or `None` when it holds none — asked once at boot, so
  /// a fresh counter never re-mints a recovered landing's id (the records are durable and the guard
  /// refuses a duplicate).
  pub fn highest_landing(&self) -> Option<u64> {
    self.landings.keys().next_back().copied()
  }

  /// The audit records retained, oldest first (A-265): the newest `caps.audit_records` of them. Their history is the
  /// log and the snapshots that carry this window; what fell past the bound is counted ([`Partition::audit_dropped`]).
  pub fn audit(&self) -> impl Iterator<Item = &AuditRecord> {
    self.audit.iter()
  }

  /// The audit records from sequence `since` on, oldest first (A-265, A-267): a seek, never a scan of the window. The
  /// window is in sequence order (each record is numbered as it is appended, and the oldest leave first), so a page of
  /// the audit costs the records it carries and a binary search, not the window's length; filtering the window from its
  /// start made a walk of it quadratic in the window.
  pub fn audit_from(&self, since: u64) -> impl Iterator<Item = &AuditRecord> {
    let start = self.audit.partition_point(|record| record.seq < since);
    self.audit.range(start..)
  }

  /// The audit records dropped oldest first past the bound since the partition began (A-265).
  pub fn audit_dropped(&self) -> u64 {
    self.audit_dropped
  }

  /// Appends an audit record, numbered here in log order whatever it carried (A-265): the guard-then-apply rule, so a
  /// replay numbers each record as the live partition did. Bounded: the oldest leaves, counted, once the window is
  /// full, from the caps alone, so a replay keeps the same window.
  fn append_audit(&mut self, record: &AuditRecord) {
    let mut record = record.clone();
    record.seq = self.audit_next_seq;
    self.audit_next_seq = self.audit_next_seq.saturating_add(1);
    self.audit.push_back(record);
    while self.audit.len() > self.caps.audit_records {
      self.audit.pop_front();
      self.audit_dropped = self.audit_dropped.saturating_add(1);
    }
  }

  /// The audit records this partition owes the control partition, oldest first, each carrying its crossing sequence in
  /// `seq` (A-265): what a batch sends, from the oldest, so a batch that was lost is in the next one.
  pub fn audit_outbox(&self) -> impl Iterator<Item = &AuditRecord> {
    self.audit_outbox.iter()
  }

  /// On the control partition: the crossing sequence of the first record partition `origin` owes that is not appended
  /// here (A-265); 0 for a partition that delivered none.
  pub fn audit_received(&self, origin: u16) -> u64 {
    self.audit_received.get(&origin).copied().unwrap_or(0)
  }

  /// The audit crossing's three records (A-265), unconditional so a replay repeats them:
  /// - `AuditOwed`: the record enters the outbox under the next crossing sequence; past the bound the oldest leaves,
  ///   counted, from the caps alone, as `append_audit`'s window does.
  /// - `AuditDelivered`: the records before `through` leave the outbox; an older acknowledgement than one already
  ///   applied removes nothing.
  /// - `AuditBatchAppended`: the records at or past `origin`'s mark are appended, in order, and the mark moves to the
  ///   batch's end; a record before the mark was appended by an earlier batch and is passed over. A batch whose first
  ///   record is past the mark appends from its first: the records between were dropped from the outbox's bound, and
  ///   counted there.
  fn apply_audit_crossing(&mut self, op: &Op) {
    match op {
      Op::AuditOwed { record } => {
        let mut record = record.clone();
        record.seq = self.audit_outbox_next;
        self.audit_outbox_next = self.audit_outbox_next.saturating_add(1);
        self.audit_outbox.push_back(record);
        while self.audit_outbox.len() > self.caps.audit_records {
          self.audit_outbox.pop_front();
          self.audit_dropped = self.audit_dropped.saturating_add(1);
        }
      }
      Op::AuditDelivered { through } => {
        while self
          .audit_outbox
          .front()
          .is_some_and(|record| record.seq < *through)
        {
          self.audit_outbox.pop_front();
        }
      }
      Op::AuditBatchAppended {
        origin,
        first,
        records,
      } => {
        let from = self.audit_received(*origin);
        for (crossing, record) in (*first..).zip(records) {
          if crossing >= from {
            self.append_audit(record);
          }
        }
        let count = u64::try_from(records.len()).unwrap_or(u64::MAX);
        let end = first.saturating_add(count);
        self.audit_received.insert(*origin, from.max(end));
      }
      _ => {}
    }
  }

  // ------------------------------------------------------------------ the guard

  /// [`Partition::check`] for a name alias (A-236) and a CLI attachment's term (A-230).
  fn check_alias_or_term(&self, op: &Op) -> Result<(), DbError> {
    match op {
      // A name is held once on its partition: by a volume there, or by one alias for another partition's volume.
      Op::NameAliasReserved { record } => {
        if let Some(existing) = self.volume_by_name(&record.name) {
          return Err(DbError::AlreadyExists {
            existing: existing.id.bytes,
          });
        }
        if let Some(alias) = self.name_alias(&record.name) {
          return Err(DbError::AlreadyExists {
            existing: alias.volume.bytes,
          });
        }
        // A work being admitted here holds its name meanwhile (A-247), against a work made on another shard as much as
        // against one made here (A-400).
        if let Some(pending) = self.pending_creations.get(&record.name) {
          return Err(DbError::AlreadyExists {
            existing: pending.work.bytes,
          });
        }
        if self.name_aliases.len() >= self.caps.volumes {
          return Err(DbError::Capacity {
            table: "name aliases",
          });
        }
        Ok(())
      }
      Op::NameAliasConfirmed { name } | Op::NameAliasReleased { name } => {
        self.name_alias(name).map(|_| ()).ok_or(DbError::NotFound)
      }
      // Only a CLI attachment holds a term.
      Op::AttachmentRenewed { id, .. } | Op::AttachmentExpired { id } => match self.attachment(*id)
      {
        Some(record) if matches!(record.consumer, Consumer::Termed { .. }) => Ok(()),
        _ => Err(DbError::NotFound),
      },
      _ => Ok(()),
    }
  }

  /// The guard the live path runs before appending: refuses what the log must never record.
  pub fn check(&self, op: &Op, now_ns: u64) -> Result<(), DbError> {
    match op {
      Op::VolumeCreated { record } => self.check_volume_created(record),
      Op::VolumeDestroyed { id } => {
        self.volume(*id).ok_or(DbError::NotFound)?;
        // A green live members of its lineage were made from keeps the version each was made at (A-575).
        let members = self.repositories.branched_from(*id);
        if members > 0 {
          return Err(DbError::BranchedFrom { members });
        }
        self
          .tombstone_sequence(*id)
          .map(|_| ())
          .ok_or(DbError::Capacity {
            table: "tombstone sequence",
          })
      }
      // A moved volume leaves only as a whole record (A-291): no attachment of it may be left behind to name it.
      Op::VolumeReleased { id } => {
        self.volume(*id).ok_or(DbError::NotFound)?;
        let attachments = self.attachments_of(*id).len();
        if attachments > 0 {
          return Err(DbError::Attached {
            attachments: u32::try_from(attachments).unwrap_or(u32::MAX),
          });
        }
        Ok(())
      }
      // A captured base names a snapshot this partition holds (A-291).
      Op::BaseCaptured { volume, snapshot } => {
        self.snapshot(*volume, *snapshot).ok_or(DbError::NotFound)?;
        Ok(())
      }
      // A carried capture names a volume here, and its mark one of that volume's snapshots (A-503).
      Op::CaptureCarried { volume, mark, .. } | Op::CaptureAdopted { volume, mark, .. } => {
        self.volume(*volume).ok_or(DbError::NotFound)?;
        if let Some(mark) = mark {
          self.snapshot(*volume, *mark).ok_or(DbError::NotFound)?;
        }
        Ok(())
      }
      // A repository's record and namespace keep §4.17.1's rules (A-515): `crate::repository`.
      Op::RepositoryCreated { .. }
      | Op::RepositoryBranchAdded { .. }
      | Op::RepositoryBranchRetired { .. }
      | Op::RepositoryDefaultSet { .. }
      | Op::RepositoryDestroyed { .. }
      | Op::RepositoryGreenJoined { .. } => self.repositories.check(
        op,
        self.caps.volumes,
        |id| self.held(id),
        |green| self.green_head(green),
      ),
      // An entry of a repository's register (A-603): its rules, then its versions' chain entries and the state it adds
      // within the merge budget, less the state it removes.
      Op::RepositoryAdvanced { entry } => self.check_entry(entry, &[]),
      // A repository's policy, set whole (A-622): `crate::repository`; its rules and hooks fit the merge byte budget,
      // the policy it replaces given back.
      Op::RepositoryPolicySet { repository, policy } => {
        self.repositories.check(
          op,
          self.caps.volumes,
          |id| self.held(id),
          |green| self.green_head(green),
        )?;
        let bytes = self.repositories.policy_bytes(*repository, policy);
        self.merge_bytes_fit(bytes.released, bytes.added)
      }
      // A review record keeps §4.17.1's rules (A-596): `crate::review`; its bytes fit the merge byte budget.
      Op::ReviewCommented { .. }
      | Op::ReviewCommentMoved { .. }
      | Op::ReviewCommentOutdated { .. }
      | Op::ReviewApproved { .. }
      | Op::ReviewApprovalStaled { .. }
      | Op::ReviewClosed { .. } => {
        self
          .reviews
          .check(op, |id| self.repositories.repository(id).is_some())?;
        let (released, added) = self.reviews.charge(op);
        self.merge_bytes_fit(released, added)
      }
      // A version-control credential (A-625 part 3): `crate::credentials`; its bytes fit the table byte budget, and the
      // refusal names the table the human was adding to.
      Op::CredentialEnrolled { .. } | Op::CredentialRevoked { .. } => {
        self.credentials.check(op)?;
        let (released, added) = self.credentials.charge(op);
        self
          .merge_bytes_fit(released, added)
          .map_err(|_| DbError::Capacity {
            table: "credentials",
          })
      }
      Op::TombstoneAdopted { tombstone } => {
        if self.volume(tombstone.volume).is_some()
          || self.tombstones.contains_key(&tombstone.volume)
        {
          return Err(DbError::AlreadyExists {
            existing: tombstone.volume.bytes,
          });
        }
        if self.volume_slots_used() >= self.caps.volumes {
          return Err(DbError::Capacity { table: "volumes" });
        }
        Ok(())
      }
      Op::TombstoneRetired { id } => self.tombstones.get(id).map(|_| ()).ok_or(DbError::NotFound),
      Op::VolumeStateChanged { id, .. }
      | Op::VolumeResized { id, .. }
      | Op::VolumeAccounted { id, .. }
      | Op::VolumeHeadAdvanced { id, .. }
      | Op::AccessChanged { id, .. }
      | Op::LiveShippingChanged { id, .. }
      | Op::LiveHoldersCommitted { id, .. }
      | Op::VolumeWritePolicySet { id, .. }
      | Op::VolumeSignersSet { id, .. }
      | Op::VolumeRelinquished { id }
      | Op::VolumeRebased { id, .. } => self.volume(*id).map(|_| ()).ok_or(DbError::NotFound),
      Op::SnapshotTaken { record } => {
        self.volume(record.volume).ok_or(DbError::NotFound)?;
        if self.snapshot(record.volume, record.id).is_some() {
          return Err(DbError::AlreadyExists {
            existing: record.volume.bytes,
          });
        }
        if self.snapshot_index.len() >= self.caps.snapshots {
          return Err(DbError::Capacity { table: "snapshots" });
        }
        Ok(())
      }
      Op::SnapshotPlaced { volume, id, .. }
      | Op::SnapshotIdentified { volume, id, .. }
      | Op::SnapshotDestroyed { volume, id } => self
        .snapshot(*volume, *id)
        .map(|_| ())
        .ok_or(DbError::NotFound),
      Op::LineageAdded { edge } => {
        self.volume(edge.child).ok_or(DbError::NotFound)?;
        Ok(())
      }
      Op::LeaseTaken { volume, lease } => self.check_lease(*volume, lease, now_ns),
      Op::LeaseReleased { volume } => self.volume(*volume).map(|_| ()).ok_or(DbError::NotFound),
      Op::AttachmentAdded { record } => {
        self.volume(record.volume).ok_or(DbError::NotFound)?;
        self.check_mount_dependency(record)?;
        if self.attachment(record.id).is_some() {
          return Err(DbError::AlreadyExists {
            existing: record.volume.bytes,
          });
        }
        if self.attachment_index.len() >= self.caps.attachments {
          return Err(DbError::Capacity {
            table: "attachments",
          });
        }
        Ok(())
      }
      Op::AttachmentRemoved { id } => self.attachment(*id).map(|_| ()).ok_or(DbError::NotFound),
      Op::AttachmentBound { id, .. } => self.attachment(*id).map(|_| ()).ok_or(DbError::NotFound),
      Op::AttachmentMountSeen { id, .. } => {
        self.attachment(*id).map(|_| ()).ok_or(DbError::NotFound)
      }
      // Only an attachment presenting a snapshot is re-pinned; the head is not a version to move from.
      Op::AttachmentRepinned { id, .. } => self
        .attachment(*id)
        .filter(|record| record.snapshot.is_some())
        .map(|_| ())
        .ok_or(DbError::NotFound),
      Op::CompletionRecorded { record } => {
        match self.completion(record.origin, record.client, record.sequence) {
          Seen::Acknowledged => Err(DbError::StaleCompletion {
            acknowledged_up_to: self
              .completions
              .get(&(record.origin, record.client))
              .and_then(ClientWindow::acknowledged_up_to)
              .unwrap_or(0),
          }),
          Seen::New | Seen::Completed(_) | Seen::Awaiting { .. } => Ok(()),
        }
      }
      Op::CompletionAwaiting {
        origin,
        client,
        sequence,
        ..
      } => match self.completion(*origin, *client, *sequence) {
        Seen::Acknowledged => Err(DbError::StaleCompletion {
          acknowledged_up_to: self
            .completions
            .get(&(*origin, *client))
            .and_then(ClientWindow::acknowledged_up_to)
            .unwrap_or(0),
        }),
        Seen::New | Seen::Completed(_) | Seen::Awaiting { .. } => Ok(()),
      },
      Op::CompletionsAcknowledged { .. } | Op::ClientIdReserved { .. } => Ok(()),
      // The NFSv4 records mirror state their owner already bounded and admitted (A-37).
      Op::NfsOpenSet { .. }
      | Op::NfsOpenCleared { .. }
      | Op::NfsDelegationSet { .. }
      | Op::NfsDelegationCleared { .. }
      | Op::NfsLockSet { .. }
      | Op::NfsLockCleared { .. }
      | Op::NfsClientStateCleared { .. }
      | Op::NfsClientSet { .. }
      | Op::NfsClientCleared { .. }
      | Op::NfsSessionSet { .. }
      | Op::NfsSessionCleared { .. }
      | Op::NfsCallSet { .. }
      | Op::NfsCallsCleared { .. }
      | Op::NfsPurgeOwed { .. }
      | Op::NfsPurgeSettled { .. } => Ok(()),
      Op::WorkFloorSet { .. }
      | Op::WorkFloorDropped { .. }
      | Op::WorkSubmitPending { .. }
      | Op::WorkSubmitSettled { .. }
      | Op::WorkCreationPending { .. }
      | Op::WorkCreationSettled { .. } => self.check_work_record(op),
      Op::NameAliasReserved { .. }
      | Op::NameAliasConfirmed { .. }
      | Op::NameAliasReleased { .. }
      | Op::AttachmentRenewed { .. }
      | Op::AttachmentExpired { .. } => self.check_alias_or_term(op),
      // A key is made once: a second record for its owner would leave two keys sealing one owner's content. A record
      // is hyper-seal's fixed length, so a malformed one never enters the log.
      Op::SealKeySet { record } => {
        let fits = match record.owner {
          SealKeyOwner::Recipient => (1..=SEAL_RECIPIENT_RECORD_MAX).contains(&record.record.len()),
          _ => record.record.len() == SEAL_KEY_RECORD_BYTES,
        };
        if !fits {
          Err(DbError::SealKeyMalformed {
            len: record.record.len(),
          })
        } else if self.seal_keys.contains_key(&record.owner) {
          Err(DbError::AlreadyExists {
            existing: record.id,
          })
        } else {
          Ok(())
        }
      }
      Op::SealKeyDestroyed { owner } => {
        if self.seal_keys.contains_key(owner) {
          Ok(())
        } else {
          Err(DbError::NotFound)
        }
      }
      // An instance only moves forward: an earlier one would let a new id repeat an old one.
      Op::NfsInstanceAdvanced { instance } => {
        if *instance > self.nfs_instance {
          Ok(())
        } else {
          Err(DbError::NfsInstanceRegressed {
            current: self.nfs_instance,
          })
        }
      }
      Op::GrantIssued { record } => {
        if self.grants.contains_key(&record.id) {
          return Err(DbError::AlreadyExists {
            existing: record.volume.bytes,
          });
        }
        Ok(())
      }
      Op::GrantStateChanged { id, .. } => self.grant(*id).map(|_| ()).ok_or(DbError::NotFound),
      Op::ConsumerEnrolled { record } => {
        if self.consumers.contains_key(&record.consumer) {
          return Err(DbError::AlreadyExists {
            existing: consumer_id_bytes(record.consumer),
          });
        }
        Ok(())
      }
      Op::ConsumerRevoked { consumer } | Op::ConsumerWritePolicySet { consumer, .. } => self
        .consumers
        .get(consumer)
        .map(|_| ())
        .ok_or(DbError::NotFound),
      Op::IdentitiesSet { record } => self.check_identities(record),
      Op::LandingLeaseTaken { record } => self.check_landing_lease(record, now_ns),
      Op::LandingLeaseReleased { target } => self
        .landing_lease(target)
        .map(|_| ())
        .ok_or(DbError::NotFound),
      Op::LandingRecorded { record } => {
        if self.landings.contains_key(&record.id) {
          return Err(DbError::AlreadyExists {
            existing: record.volume.bytes,
          });
        }
        Ok(())
      }
      Op::LandingStateChanged { id, .. } => self.landing(*id).map(|_| ()).ok_or(DbError::NotFound),
      // The audit is never refused (A-265): its records are the history of what happened, kept or counted as dropped.
      Op::AuditAppended { .. }
      | Op::AuditOwed { .. }
      | Op::AuditDelivered { .. }
      | Op::AuditBatchAppended { .. } => Ok(()),
      Op::GreenAdvanced { green, increment } => {
        self.volume(*green).ok_or(DbError::NotFound)?;
        // Bounded growth (§4.16; CLAUDE.md): the chain grows per accepted version, so a new increment
        // is refused once the partition's green chains would exceed their byte budget — the typed
        // refusal at the bound; a checkpoint (W3) folds the increments below the reachable floor.
        if self.green_chain_bytes.saturating_add(increment.len()) > self.caps.green_chain_bytes {
          return Err(DbError::Capacity {
            table: "green_chains",
          });
        }
        Ok(())
      }
      Op::WorkLogged { work, entry } => {
        self.volume(*work).ok_or(DbError::NotFound)?;
        self.merge_bytes_fit(0, entry.len())
      }
      Op::GreenOriginHeld { green, origin } => {
        self.volume(*green).ok_or(DbError::NotFound)?;
        self
          .snapshot(origin.volume, origin.snapshot)
          .ok_or(DbError::NotFound)?;
        self.check_version_zero_unrecorded(*green)
      }
      Op::WorkRestated { work, entry } => {
        self.volume(*work).ok_or(DbError::NotFound)?;
        self.merge_bytes_fit(self.work_log_bytes(*work), entry.len())
      }
      Op::GreenCheckpointed {
        green,
        version,
        checkpoint,
      } => self.check_green_checkpoint(*green, *version, checkpoint.len()),
      Op::GreenOriginated { green, origin } => {
        self.volume(*green).ok_or(DbError::NotFound)?;
        self.check_version_zero_unrecorded(*green)?;
        if self.green_chain_bytes.saturating_add(origin.len()) > self.caps.green_chain_bytes {
          return Err(DbError::Capacity {
            table: "green_chains",
          });
        }
        Ok(())
      }
      // A green adopted by its successor is recorded by its adoption (A-333): a new volume, checked as a creation is.
      Op::GreenAdopted { record, .. } => self.check_volume_created(record),
    }
  }

  /// The guard for recording `made`, new volumes, and then `entry` (A-635): judged whole before the first is recorded,
  /// since what a transaction records before a refusal commits with it. Each made volume is checked as its creation is,
  /// with the ids and names of those made before it and the volumes table's room for all of them; then the entry, each
  /// made green counted one the partition holds, at no version yet. A push that makes new branches records them so.
  pub fn check_creating(
    &self,
    made: &[crate::catalog::VolumeRecord],
    entry: &crate::catalog::RepositoryEntry,
  ) -> Result<(), DbError> {
    for (index, record) in made.iter().enumerate() {
      self.check_volume_created(record)?;
      if made
        .iter()
        .take(index)
        .any(|earlier| earlier.id == record.id || earlier.name == record.name)
      {
        return Err(DbError::AlreadyExists {
          existing: record.id.bytes,
        });
      }
    }
    if self.volume_slots_used().saturating_add(made.len()) > self.caps.volumes {
      return Err(DbError::Capacity { table: "volumes" });
    }
    self.check_entry(entry, made)
  }

  /// The guard for an entry of a repository's register (A-603): its rules, then its versions' chain entries and the
  /// state it adds within the merge budget, less the state it removes. `made` are volumes recorded before it in its
  /// transaction (A-635), each held as its record says, a green at no version yet.
  fn check_entry(
    &self,
    entry: &crate::catalog::RepositoryEntry,
    made: &[crate::catalog::VolumeRecord],
  ) -> Result<(), DbError> {
    let held = |id: VolumeId| match made.iter().find(|record| record.id == id) {
      Some(record) if matches!(record.policy.role, crate::catalog::Role::Green { .. }) => {
        crate::repository::Held::Green
      }
      Some(_) => crate::repository::Held::NotGreen,
      None => self.held(id),
    };
    let head = |green: VolumeId| {
      if made.iter().any(|record| record.id == green) {
        0
      } else {
        self.green_head(green)
      }
    };
    self.repositories.check_entry(entry, held, head)?;
    let bytes = crate::repository::Repositories::entry_bytes(entry);
    self.merge_bytes_fit(bytes.released, bytes.added)
  }

  /// The check of a volume's creation, and of a green's by its adoption (A-333): the id and the name free here (a name
  /// this partition holds for another partition's volume is that volume's, A-236, confirmed or not), no tombstone owed
  /// under the id, and room in the volumes table.
  fn check_volume_created(&self, record: &crate::catalog::VolumeRecord) -> Result<(), DbError> {
    if let Some(existing) = self.volume(record.id) {
      return Err(DbError::AlreadyExists {
        existing: existing.id.bytes,
      });
    }
    if let Some(existing) = self.volume_by_name(&record.name) {
      return Err(DbError::AlreadyExists {
        existing: existing.id.bytes,
      });
    }
    if let Some(alias) = self.name_alias(&record.name) {
      return Err(DbError::AlreadyExists {
        existing: alias.volume.bytes,
      });
    }
    if self.tombstones.contains_key(&record.id) {
      return Err(DbError::AlreadyExists {
        existing: record.id.bytes,
      });
    }
    if self.volume_slots_used() >= self.caps.volumes {
      return Err(DbError::Capacity { table: "volumes" });
    }
    Ok(())
  }

  /// Version 0 of a green is recorded once and only before anything after it, whatever form it takes — the origin's
  /// bytes, a reference to its base snapshot (A-217) or an adoption from this node's holder ledger (A-333, which records
  /// the green itself, so nothing precedes it): a chain that has advanced was based on whatever origin it had, so a later
  /// one would rewrite history. `AlreadyExists` otherwise.
  fn check_version_zero_unrecorded(&self, green: VolumeId) -> Result<(), DbError> {
    if self.green_origins.contains_key(&green)
      || self.green_origin_refs.contains_key(&green)
      || self.green_adoptions.contains_key(&green)
      || self.green_checkpoints.contains_key(&green)
      || !self.green_chain(green).is_empty()
    {
      return Err(DbError::AlreadyExists {
        existing: green.bytes,
      });
    }
    Ok(())
  }

  /// A lease is taken by a new holder only after the current one expired or was released,
  /// with the epoch one above the current; the holder renews with the same epoch (D-16).
  fn check_lease(&self, volume: VolumeId, lease: &LeaseRecord, now_ns: u64) -> Result<(), DbError> {
    let record = self.volume(volume).ok_or(DbError::NotFound)?;
    match &record.lease {
      None => Ok(()),
      Some(current) if current.holder == lease.holder => {
        if lease.epoch < current.epoch {
          Err(DbError::StaleLease {
            current: current.epoch,
          })
        } else {
          Ok(())
        }
      }
      Some(current) if current.expires_ns <= now_ns => {
        if lease.epoch <= current.epoch {
          Err(DbError::StaleLease {
            current: current.epoch,
          })
        } else {
          Ok(())
        }
      }
      Some(current) => Err(DbError::LeaseHeld {
        epoch: current.epoch,
      }),
    }
  }

  /// One holder per target (§4.15 step 4; AUD-29-03): a take by another attempt is refused while the current
  /// lease is unexpired, and a lease's fencing generation never goes backwards. The holder is one landing attempt,
  /// so its own take of a live lease is that attempt's renewal (the keepalive, AUD-29-25). Before 2026-09-29 the
  /// holder was the caller's session, so two landings of one principal took each other's lease; from then until
  /// 2026-11-07 the attempt's own renewal was refused while its lease held, so it only renewed once lapsed.
  fn check_landing_lease(&self, record: &LandingLeaseRecord, now_ns: u64) -> Result<(), DbError> {
    match self.landing_lease(&record.target) {
      // A live lease is its holder's: another attempt is refused, and the holder itself takes it again to renew it
      // (§4.15 "The lease keepalive", AUD-29-25) under the newer generation the next arm requires. Before 2026-11-07
      // the holder was refused too, so a running landing's renewal only succeeded once its lease had lapsed.
      Some(held) if held.expires_ns > now_ns && held.holder != record.holder => {
        Err(DbError::LeaseHeld {
          epoch: held.generation,
        })
      }
      Some(held) if record.generation <= held.generation => Err(DbError::StaleLease {
        current: held.generation,
      }),
      _ => Ok(()),
    }
  }

  // ------------------------------------------------------------------ the transition

  /// [`Op::VolumeDestroyed`]'s effect: the tombstone takes the volume's slot, its sequence read before the record goes;
  /// and the volume's lineage key goes with it, in the same record (A-92; seal.md §3.1, cryptographic erase): its one
  /// wrapped copy was here, so every chunk sealed under it, on any holder, opens for no one after this.
  fn destroy_volume(&mut self, id: VolumeId) -> Result<(), DbError> {
    let sequence = self.tombstone_sequence(id);
    let tenant = self.volume(id).map(|volume| volume.owner.tenant());
    self.remove_volume(id)?;
    if let Some(sequence) = sequence {
      self.tombstones.insert(id, sequence);
    }
    self.erase_volume_keys(id, tenant);
    Ok(())
  }

  /// [`Op::SnapshotDestroyed`]'s effect: the record and its index entry go, with any capture base it was; and a carried
  /// capture's mark goes with its snapshot (A-503), so its landings are refused from then on.
  fn destroy_snapshot(&mut self, volume: VolumeId, id: SnapshotId) -> Result<(), DbError> {
    let h = self
      .snapshot_index
      .remove(&snapshot_key(volume, id))
      .ok_or(DbError::NotFound)?;
    self.snapshots.remove(h)?;
    self.captured_bases.remove(&CapturedBase {
      volume,
      snapshot: id,
    });
    if let Some((_, mark)) = self.carried.get_mut(&volume)
      && *mark == Some(id)
    {
      *mark = None;
    }
    Ok(())
  }

  /// Applies a recorded operation; unconditional, so replay is deterministic. A record the
  /// operation names that does not exist (a corrupted history) is a typed refusal.
  pub fn apply(&mut self, op: &Op) -> Result<(), DbError> {
    match op {
      Op::VolumeCreated { record } => {
        // Advanced whatever the insert meets, as a replay must (A-694).
        self.volume_counter = self
          .volume_counter
          .max(record.id.counter().saturating_add(1));
        self.insert_volume(record.clone())
      }
      Op::VolumeStateChanged { id, state } => self.update_volume(*id, |v| v.state = *state),
      Op::VolumeResized { id, size } => self.update_volume(*id, |v| {
        v.policy.size = *size;
        v.catalog_version = v.catalog_version.saturating_add(1);
      }),
      Op::VolumeRebased { id, base } => self.update_volume(*id, |v| {
        v.base = base.clone();
        v.catalog_version = v.catalog_version.saturating_add(1);
      }),
      Op::VolumeAccounted {
        id,
        referenced_bytes,
        unique_bytes,
      } => self.update_volume(*id, |v| {
        v.referenced_bytes = *referenced_bytes;
        v.unique_bytes = *unique_bytes;
      }),
      Op::VolumeHeadAdvanced { id, head, epoch } => self.update_volume(*id, |v| {
        v.head = *head;
        v.epoch = *epoch;
      }),
      Op::AccessChanged { id, access } => self.update_volume(*id, |v| {
        v.access = access.clone();
        v.catalog_version = v.catalog_version.saturating_add(1);
      }),
      // The catalog register carries the flag (a successor keeps shipping), so its version moves as an access change's.
      Op::LiveShippingChanged { id, live_shipped } => self.update_volume(*id, |v| {
        v.policy.live_shipped = *live_shipped;
        v.catalog_version = v.catalog_version.saturating_add(1);
      }),
      // The catalog register carries the holder set (A-240), so its version moves as the flag's does.
      Op::LiveHoldersCommitted { id, holders } => self.update_volume(*id, |v| {
        v.policy.live_holders = Some(holders.clone());
        v.catalog_version = v.catalog_version.saturating_add(1);
      }),
      // The catalog register carries the policy (A-285), so its version moves as the live-shipping flag's does.
      Op::VolumeWritePolicySet { id, policy } => self.update_volume(*id, |v| {
        v.policy.write = Some(*policy);
        v.policy.write_version = v.policy.write_version.saturating_add(1);
        v.catalog_version = v.catalog_version.saturating_add(1);
      }),
      // The signers are part of the policy (A-321): its version moves with them, so a captured setting of either names
      // a version the other has moved past.
      Op::VolumeSignersSet { id, keys } => self.update_volume(*id, |v| {
        v.policy.signers.clone_from(keys);
        v.policy.write_version = v.policy.write_version.saturating_add(1);
        v.catalog_version = v.catalog_version.saturating_add(1);
      }),
      Op::VolumeDestroyed { id } => self.destroy_volume(*id),
      Op::VolumeReleased { id } => self.release_volume(*id),
      Op::BaseCaptured { volume, snapshot } => {
        self.captured_bases.insert(CapturedBase {
          volume: *volume,
          snapshot: *snapshot,
        });
        Ok(())
      }
      // The catalog register carries it (A-503), so its version moves as the write policy's does.
      Op::CaptureCarried {
        volume,
        carried,
        mark,
      } => {
        self.update_volume(*volume, |v| {
          v.catalog_version = v.catalog_version.saturating_add(1);
        })?;
        self.carried.insert(*volume, (carried.clone(), *mark));
        Ok(())
      }
      // Adopted with the volume's catalog, which already carries it: its version stays (A-503).
      Op::CaptureAdopted {
        volume,
        carried,
        mark,
      } => {
        self.volume(*volume).ok_or(DbError::NotFound)?;
        self.carried.insert(*volume, (carried.clone(), *mark));
        Ok(())
      }
      Op::RepositoryCreated { .. }
      | Op::RepositoryBranchAdded { .. }
      | Op::RepositoryBranchRetired { .. }
      | Op::RepositoryDefaultSet { .. }
      | Op::RepositoryGreenJoined { .. }
      | Op::RepositoryAdvanced { .. }
      | Op::RepositoryPolicySet { .. } => self.apply_repository(op),
      // A destroyed repository's review records leave with it, their bytes back to the merge budget (A-596), as its
      // register's state does (A-603).
      Op::RepositoryDestroyed { repository } => {
        self.apply_repository(op)?;
        let released = self.reviews.repository_removed(*repository);
        self.green_chain_bytes = self.green_chain_bytes.saturating_sub(released);
        Ok(())
      }
      Op::ReviewCommented { .. }
      | Op::ReviewCommentMoved { .. }
      | Op::ReviewCommentOutdated { .. }
      | Op::ReviewApproved { .. }
      | Op::ReviewApprovalStaled { .. }
      | Op::ReviewClosed { .. } => {
        let (released, added) = self.reviews.charge(op);
        self.reviews.apply(op)?;
        self.green_chain_bytes = self
          .green_chain_bytes
          .saturating_sub(released)
          .saturating_add(added);
        Ok(())
      }
      Op::CredentialEnrolled { .. } | Op::CredentialRevoked { .. } => {
        let (released, added) = self.credentials.charge(op);
        self.credentials.apply(op)?;
        self.green_chain_bytes = self
          .green_chain_bytes
          .saturating_sub(released)
          .saturating_add(added);
        Ok(())
      }
      // Re-adopted here (A-362): the stale record goes with what `remove_volume` takes, and nothing of a destroy's.
      Op::VolumeRelinquished { id } => self.remove_volume(*id),
      Op::TombstoneAdopted { tombstone } => {
        self.tombstones.insert(tombstone.volume, tombstone.sequence);
        Ok(())
      }
      Op::TombstoneRetired { id } => {
        self.tombstones.remove(id);
        Ok(())
      }
      Op::SnapshotTaken { record } => self.insert_snapshot(record.clone()),
      Op::SnapshotPlaced { volume, id, placed } => {
        let h = *self
          .snapshot_index
          .get(&snapshot_key(*volume, *id))
          .ok_or(DbError::NotFound)?;
        self.snapshots.get_mut(h)?.placed = placed.clone();
        Ok(())
      }
      Op::SnapshotIdentified {
        volume,
        id,
        identity,
      } => {
        let h = *self
          .snapshot_index
          .get(&snapshot_key(*volume, *id))
          .ok_or(DbError::NotFound)?;
        self.snapshots.get_mut(h)?.identity = Some(*identity);
        Ok(())
      }
      Op::SnapshotDestroyed { volume, id } => self.destroy_snapshot(*volume, *id),
      Op::LineageAdded { edge } => {
        self.lineage.insert(&edge.child.bytes, edge.clone());
        Ok(())
      }
      Op::LeaseTaken { volume, lease } => self.set_lease(*volume, Some(lease.clone())),
      Op::LeaseReleased { volume } => self.set_lease(*volume, None),
      Op::AttachmentAdded { record } => {
        let h = self.attachments.insert(record.clone())?;
        self.attachment_index.insert(&record.id.to_be_bytes(), h);
        self.index_term(record);
        Ok(())
      }
      Op::ClientIdReserved { client } => {
        self.client_id_high_water = self.client_id_high_water.max(*client);
        Ok(())
      }
      Op::NfsOpenSet { .. }
      | Op::NfsOpenCleared { .. }
      | Op::NfsDelegationSet { .. }
      | Op::NfsDelegationCleared { .. }
      | Op::NfsLockSet { .. }
      | Op::NfsLockCleared { .. }
      | Op::NfsClientStateCleared { .. }
      | Op::NfsClientSet { .. }
      | Op::NfsClientCleared { .. }
      | Op::NfsSessionSet { .. }
      | Op::NfsSessionCleared { .. }
      | Op::NfsCallSet { .. }
      | Op::NfsCallsCleared { .. }
      | Op::NfsPurgeOwed { .. }
      | Op::NfsPurgeSettled { .. }
      | Op::NfsInstanceAdvanced { .. } => {
        self.apply_nfs(op);
        Ok(())
      }
      Op::AttachmentRenewed { id, expires_ns } => {
        let h = *self
          .attachment_index
          .get(&id.to_be_bytes())
          .ok_or(DbError::NotFound)?;
        let record = self.attachments.get_mut(h).map_err(|_| DbError::NotFound)?;
        let lapsed_at = std::mem::replace(
          &mut record.consumer,
          Consumer::Termed {
            expires_ns: *expires_ns,
          },
        );
        let renewed = record.clone();
        if let Consumer::Termed { expires_ns: before } = lapsed_at {
          self.termed_by_expiry.remove(&(before, *id));
        }
        self.index_term(&renewed);
        Ok(())
      }
      Op::AttachmentExpired { id } => {
        self.remove_attachment(*id)?;
        if self.expired_attachments.len() >= self.caps.attachments.max(1) {
          self.expired_attachments.pop_front();
        }
        self.expired_attachments.push_back(*id);
        Ok(())
      }
      Op::WorkFloorSet { .. }
      | Op::WorkFloorDropped { .. }
      | Op::WorkSubmitPending { .. }
      | Op::WorkSubmitSettled { .. }
      | Op::WorkCreationPending { .. }
      | Op::WorkCreationSettled { .. } => {
        self.apply_work_floor(op);
        Ok(())
      }
      Op::NameAliasReserved { record } => {
        self
          .name_aliases
          .insert(record.name.clone(), record.clone());
        Ok(())
      }
      Op::NameAliasConfirmed { name } => {
        let alias = self.name_aliases.get_mut(name).ok_or(DbError::NotFound)?;
        alias.confirmed = true;
        Ok(())
      }
      Op::NameAliasReleased { name } => self
        .name_aliases
        .remove(name)
        .map(|_| ())
        .ok_or(DbError::NotFound),
      Op::SealKeySet { record } => {
        self.seal_keys.insert(record.owner, record.clone());
        Ok(())
      }
      Op::SealKeyDestroyed { owner } => {
        self.seal_keys.remove(owner);
        Ok(())
      }
      Op::AttachmentBound { id, path } => {
        let h = *self
          .attachment_index
          .get(&id.to_be_bytes())
          .ok_or(DbError::NotFound)?;
        let record = self.attachments.get_mut(h).map_err(|_| DbError::NotFound)?;
        // A FUSE form or a WinFsp volume already names its mount point (the daemon mounted it); a bound mount keeps
        // its identity and scope, and a scoped mount its scope, when it is bound; any other host mount is bound at a
        // chosen path.
        if record.form.daemon_mount_point().is_none() {
          record.form = match (record.form.bound_uid(), record.form.scope()) {
            (Some(uid), scope) => crate::catalog::AttachForm::BoundMount {
              uid,
              scope,
              mount_point: Some(path.clone()),
            },
            (None, Some(scope)) => crate::catalog::AttachForm::ScopedMount {
              scope,
              mount_point: Some(path.clone()),
            },
            (None, None) => crate::catalog::AttachForm::ChosenPath { path: path.clone() },
          };
        }
        Ok(())
      }
      Op::AttachmentMountSeen { id, mount } => {
        let h = *self
          .attachment_index
          .get(&id.to_be_bytes())
          .ok_or(DbError::NotFound)?;
        let record = self.attachments.get_mut(h).map_err(|_| DbError::NotFound)?;
        record.mount = Some(*mount);
        Ok(())
      }
      Op::AttachmentRepinned { id, snapshot } => {
        // The mount and its borrowers move together, so a bind never claims a version its mount no longer
        // presents (`oci::Binding::consumer` matches them).
        let moved: Vec<u64> = self
          .attachments
          .iter()
          .filter(|(_, record)| {
            record.id == *id
              || matches!(record.consumer, crate::catalog::Consumer::Mount { attachment } if attachment == *id)
          })
          .map(|(_, record)| record.id)
          .collect();
        for moved in moved {
          let h = *self
            .attachment_index
            .get(&moved.to_be_bytes())
            .ok_or(DbError::NotFound)?;
          let record = self.attachments.get_mut(h).map_err(|_| DbError::NotFound)?;
          record.snapshot = Some(*snapshot);
        }
        Ok(())
      }
      Op::AttachmentRemoved { id } => {
        // One recorded transition removes the mount and its borrowers. A dependency is
        // one level deep by admission, so no recursive walk or intermediate orphan exists.
        let dependent: Vec<u64> = if self
          .attachment(*id)
          .is_some_and(|record| matches!(record.consumer, Consumer::Bridge))
        {
          self.attachments.iter()
            .filter(|(_, record)| matches!(record.consumer, Consumer::Mount { attachment } if attachment == *id))
            .map(|(_, record)| record.id)
            .collect()
        } else {
          Vec::new()
        };
        for dependent in dependent {
          self.remove_attachment(dependent)?;
        }
        self.remove_attachment(*id)?;
        Ok(())
      }
      Op::CompletionRecorded { record } => {
        let window = self
          .completions
          .entry((record.origin, record.client))
          .or_default();
        window.record(record.sequence, record.result.clone());
        // Routed only while the window retains it: a completion the client already acknowledged is not kept (A-288).
        let routed = record
          .work
          .filter(|_| matches!(window.lookup(record.sequence), Seen::Completed(_)));
        self.route_completion((record.origin, record.client, record.sequence), routed);
        Ok(())
      }
      Op::CompletionAwaiting {
        origin,
        client,
        sequence,
        green,
        version,
      } => {
        self
          .completions
          .entry((*origin, *client))
          .or_default()
          .await_version(*sequence, green.bytes, *version);
        Ok(())
      }
      Op::CompletionsAcknowledged {
        origin,
        client,
        up_to,
        owed,
      } => {
        self
          .completions
          .entry((*origin, *client))
          .or_default()
          .acknowledge(*up_to, owed);
        self.prune_routes(*origin, *client);
        Ok(())
      }
      Op::GrantIssued { record } => {
        self.grants_issued_through = self.grants_issued_through.max(record.id);
        self.grants.insert(record.id, record.clone());
        Ok(())
      }
      Op::GrantStateChanged { id, state } => self.change_grant_state(*id, *state),
      Op::ConsumerEnrolled { record } => {
        self.consumers.insert(record.consumer, record.clone());
        Ok(())
      }
      Op::ConsumerRevoked { consumer } => {
        self
          .consumers
          .get_mut(consumer)
          .ok_or(DbError::NotFound)?
          .revoked = true;
        Ok(())
      }
      Op::ConsumerWritePolicySet { consumer, policy } => {
        let record = self.consumers.get_mut(consumer).ok_or(DbError::NotFound)?;
        record.write = Some(*policy);
        record.write_version = record.write_version.saturating_add(1);
        Ok(())
      }
      Op::IdentitiesSet { record } => {
        self
          .identities
          .insert(record.principal.key(), record.clone());
        Ok(())
      }
      Op::LandingLeaseTaken { record } => {
        self
          .landing_leases
          .insert(record.target.as_bytes(), record.clone());
        Ok(())
      }
      Op::LandingLeaseReleased { target } => {
        self
          .landing_leases
          .remove(target.as_bytes())
          .ok_or(DbError::NotFound)?;
        Ok(())
      }
      Op::LandingRecorded { record } => {
        self.landings.insert(record.id, record.clone());
        Ok(())
      }
      Op::LandingStateChanged {
        id,
        state,
        written,
        conflicts,
      } => {
        let landing = self.landings.get_mut(id).ok_or(DbError::NotFound)?;
        landing.state = *state;
        landing.written = *written;
        landing.conflicts = *conflicts;
        Ok(())
      }
      Op::AuditAppended { record } => {
        self.append_audit(record);
        Ok(())
      }
      Op::AuditOwed { .. } | Op::AuditDelivered { .. } | Op::AuditBatchAppended { .. } => {
        self.apply_audit_crossing(op);
        Ok(())
      }
      Op::GreenAdvanced { green, increment } => {
        self.green_chain_bytes = self.green_chain_bytes.saturating_add(increment.len());
        self
          .green_chains
          .entry(*green)
          .or_default()
          .push(increment.clone());
        Ok(())
      }
      Op::GreenOriginated { green, origin } => {
        self.green_chain_bytes = self.green_chain_bytes.saturating_add(origin.len());
        self.green_origins.insert(*green, origin.clone());
        Ok(())
      }
      Op::GreenOriginHeld { green, origin } => {
        self.green_origin_refs.insert(*green, *origin);
        Ok(())
      }
      Op::GreenAdopted {
        record,
        version,
        identity,
        origin,
      } => {
        self.insert_volume(record.clone())?;
        self.green_adoptions.insert(
          record.id,
          GreenAdoption {
            green: record.id,
            version: *version,
            identity: *identity,
            origin: *origin,
          },
        );
        Ok(())
      }
      Op::WorkLogged { work, entry } => {
        self.green_chain_bytes = self.green_chain_bytes.saturating_add(entry.len());
        self.work_logs.entry(*work).or_default().push(entry.clone());
        Ok(())
      }
      Op::WorkRestated { work, entry } => {
        let released = self.work_log_bytes(*work);
        self.green_chain_bytes = self
          .green_chain_bytes
          .saturating_sub(released)
          .saturating_add(entry.len());
        self.work_logs.insert(*work, vec![entry.clone()]);
        Ok(())
      }
      Op::GreenCheckpointed {
        green,
        version,
        checkpoint,
      } => {
        self.apply_green_checkpoint(*green, *version, checkpoint);
        Ok(())
      }
    }
  }

  /// The bytes a work's log holds now.
  fn work_log_bytes(&self, work: VolumeId) -> usize {
    self
      .work_logs
      .get(&work)
      .map_or(0, |log| log.iter().map(Vec::len).sum())
  }

  /// Whether replacing `released` bytes of the merge budget with `added` keeps it within the partition's cap: the
  /// typed refusal at the bound otherwise (bounded growth, CLAUDE.md banned item 8).
  /// Applies a repository operation (A-515, A-575, A-603): the repositories' own change, then an entry's versions as
  /// the greens' chain entries, each charged to the merge budget with what the repositories' state gained, less what it
  /// released.
  fn apply_repository(&mut self, op: &Op) -> Result<(), DbError> {
    let bytes = match op {
      // Each green an entry moves goes from its head before it (A-642): what its atomic record holds of its log.
      Op::RepositoryAdvanced { entry } => {
        let greens =
          crate::repository::Repositories::green_spans(entry, |green| self.green_head(green));
        self.repositories.apply_advanced(entry, greens)?
      }
      _ => self.repositories.apply(op)?,
    };
    if let Op::RepositoryAdvanced { entry } = op {
      for version in &entry.versions {
        self.green_chain_bytes = self.green_chain_bytes.saturating_add(version.entry.len());
        self
          .green_chains
          .entry(version.green)
          .or_default()
          .push(version.entry.clone());
      }
    }
    self.green_chain_bytes = self
      .green_chain_bytes
      .saturating_sub(bytes.released)
      .saturating_add(bytes.added);
    Ok(())
  }

  fn merge_bytes_fit(&self, released: usize, added: usize) -> Result<(), DbError> {
    if self
      .green_chain_bytes
      .saturating_sub(released)
      .saturating_add(added)
      > self.caps.green_chain_bytes
    {
      return Err(DbError::Capacity {
        table: "green_chains",
      });
    }
    Ok(())
  }

  /// A green's origin reference (A-217), if its origin is held as one.
  pub fn green_origin_ref(&self, green: VolumeId) -> Option<&OriginRef> {
    self.green_origin_refs.get(&green)
  }

  /// Every green origin reference, by green (A-217): what recovery counts as pins, and what a volume's destroy checks.
  pub fn green_origin_refs(&self) -> impl Iterator<Item = (&VolumeId, &OriginRef)> {
    self.green_origin_refs.iter()
  }

  /// A work volume's log since its last restatement, oldest first (§4.16, D-18): what recovery replays onto it.
  pub fn work_log(&self, work: VolumeId) -> &[Vec<u8>] {
    self.work_logs.get(&work).map_or(&[], Vec::as_slice)
  }

  fn insert_volume(&mut self, record: VolumeRecord) -> Result<(), DbError> {
    let id = record.id;
    let name = record.name.clone();
    let lease = record.lease.clone();
    let h = self.volumes.insert(record)?;
    self.by_id.insert(&id.bytes, h);
    self.by_name.insert(name.as_bytes(), h);
    if let Some(lease) = lease {
      self.index_lease(id, &lease)?;
    }
    Ok(())
  }

  fn update_volume(
    &mut self,
    id: VolumeId,
    f: impl FnOnce(&mut VolumeRecord),
  ) -> Result<(), DbError> {
    let h = *self.by_id.get(&id.bytes).ok_or(DbError::NotFound)?;
    f(self.volumes.get_mut(h)?);
    Ok(())
  }

  fn remove_volume(&mut self, id: VolumeId) -> Result<(), DbError> {
    let h = self.by_id.remove(&id.bytes).ok_or(DbError::NotFound)?;
    let record = self.volumes.remove(h)?;
    self.by_name.remove(record.name.as_bytes());
    if let Some(lease) = &record.lease {
      self.leases_by_holder.remove(&lease.holder.key());
    }
    if let Some(t) = self.lease_timers.remove(&id) {
      let _ = self.lease_expiry.cancel(t);
    }
    let keys: Vec<Vec<u8>> = self
      .snapshot_index
      .scan_prefix(&id.bytes)
      .into_iter()
      .map(|(key, _)| key)
      .collect();
    for key in keys {
      if let Some(h) = self.snapshot_index.remove(&key) {
        let _ = self.snapshots.remove(h);
      }
    }
    self.lineage.remove(&id.bytes);
    self.captured_bases.retain(|captured| captured.volume != id);
    self.carried.remove(&id);
    // A destroyed green leaves the namespace that bound it (§4.17.1, A-515).
    self.repositories.green_removed(id);
    // A destroyed green's merge chain is reclaimed with it (its increments can never be replayed for
    // a volume that no longer exists), returning its bytes to the chain budget.
    if let Some(chain) = self.green_chains.remove(&id) {
      let bytes: usize = chain.iter().map(Vec::len).sum();
      self.green_chain_bytes = self.green_chain_bytes.saturating_sub(bytes);
    }
    if let Some(origin) = self.green_origins.remove(&id) {
      self.green_chain_bytes = self.green_chain_bytes.saturating_sub(origin.len());
    }
    self.green_origin_refs.remove(&id);
    self.green_adoptions.remove(&id);
    if let Some((_, checkpoint)) = self.green_checkpoints.remove(&id) {
      self.green_chain_bytes = self.green_chain_bytes.saturating_sub(checkpoint.len());
    }
    // A destroyed work's log can never be replayed either.
    let log = self.work_log_bytes(id);
    if self.work_logs.remove(&id).is_some() {
      self.green_chain_bytes = self.green_chain_bytes.saturating_sub(log);
    }
    Ok(())
  }

  fn insert_snapshot(&mut self, record: SnapshotRecord) -> Result<(), DbError> {
    let key = snapshot_key(record.volume, record.id);
    let h = self.snapshots.insert(record)?;
    self.snapshot_index.insert(&key, h);
    Ok(())
  }

  fn set_lease(&mut self, volume: VolumeId, lease: Option<LeaseRecord>) -> Result<(), DbError> {
    let h = *self.by_id.get(&volume.bytes).ok_or(DbError::NotFound)?;
    if let Some(old) = self.volumes.get(h)?.lease.clone() {
      self.leases_by_holder.remove(&old.holder.key());
    }
    if let Some(t) = self.lease_timers.remove(&volume) {
      let _ = self.lease_expiry.cancel(t);
    }
    self.volumes.get_mut(h)?.lease = lease.clone();
    if let Some(lease) = lease {
      self.index_lease(volume, &lease)?;
    }
    Ok(())
  }

  fn index_lease(&mut self, volume: VolumeId, lease: &LeaseRecord) -> Result<(), DbError> {
    self.leases_by_holder.insert(&lease.holder.key(), volume);
    let h = *self.by_id.get(&volume.bytes).ok_or(DbError::NotFound)?;
    let word = u64::from(h.index());
    let timer = self
      .lease_expiry
      .insert(lease.expires_ns, word)
      .map_err(|_| DbError::Capacity {
        table: "lease timers",
      })?;
    self.lease_timers.insert(volume, timer);
    // Noted as the wheel will report it, its tick at or after the expiry (`next_lease_expiry`), so the keeper's own arming
    // for it reads as no earlier deadline.
    let tick = self.lease_expiry.tick_ns().max(1);
    let due = lease.expires_ns.div_ceil(tick).saturating_mul(tick);
    self.lease_noted = Some(self.lease_noted.map_or(due, |noted| noted.min(due)));
    Ok(())
  }

  /// The earliest lease expiry entered since the last call, taken: the shard's keeper wakes for it when it sleeps for a
  /// later deadline (A-536).
  pub fn take_noted_lease(&mut self) -> Option<u64> {
    self.lease_noted.take()
  }

  /// When the next lease lapses on the wheel, the deadline the shard's keeper sleeps until for leases (A-536): the
  /// wheel's tick at or after the earliest expiry, so a pass at that moment finds the lease due.
  pub fn next_lease_expiry(&mut self) -> Option<u64> {
    self.lease_expiry.next_deadline_ns()
  }

  /// The volumes whose leases expired by `now_ns` (the caller records `LeaseReleased` for
  /// each; the wheel pops, it never scans).
  pub fn expired_leases(&mut self, now_ns: u64) -> Vec<VolumeId> {
    let mut fired = Vec::new();
    self.lease_expiry.advance(now_ns, &mut fired);
    let mut out = Vec::new();
    for word in fired {
      let index = u32::try_from(word).unwrap_or(u32::MAX);
      let Some(generation) = self.volumes.generation_at(index) else {
        continue;
      };
      let h = Handle::from_raw(index, generation);
      if let Ok(v) = self.volumes.get(h)
        && v.lease.as_ref().is_some_and(|l| l.expires_ns <= now_ns)
      {
        out.push(v.id);
      }
    }
    out
  }

  // ------------------------------------------------------------------ snapshots

  /// The whole state as one value, labelled with `seq`.
  pub fn to_snapshot(&self, seq: u64) -> PartitionSnapshot {
    PartitionSnapshot {
      client_id_high_water: self.client_id_high_water,
      seq,
      volumes: self.volumes().into_iter().cloned().collect(),
      snapshots: self
        .snapshot_index
        .iter()
        .filter_map(|(_, h)| self.snapshots.get(*h).ok().cloned())
        .collect(),
      lineage: self.lineage.iter().map(|(_, e)| e.clone()).collect(),
      attachments: self
        .attachment_index
        .iter()
        .filter_map(|(_, h)| self.attachments.get(*h).ok().cloned())
        .collect(),
      completions: self
        .completions
        .iter()
        .map(|((origin, client), w)| ClientCompletions {
          origin: *origin,
          client: *client,
          acknowledged_up_to: w.acknowledged_up_to(),
          owed: w.owed().collect(),
          records: w
            .retained_entries()
            .map(|(sequence, result)| CompletionRecord {
              origin: *origin,
              client: *client,
              sequence,
              result: result.clone(),
              work: self
                .routed_completions
                .get(&(*origin, *client, sequence))
                .copied(),
            })
            .collect(),
          awaiting: w
            .awaiting_entries()
            .map(
              |(sequence, object, version)| crate::catalog::AwaitingCompletion {
                sequence,
                green: VolumeId { bytes: object },
                version,
              },
            )
            .collect(),
        })
        .collect(),
      grants: self.grants.values().cloned().collect(),
      landing_leases: self.landing_leases.iter().map(|(_, l)| l.clone()).collect(),
      landings: self.landings.values().cloned().collect(),
      audit: self.audit.iter().cloned().collect(),
      consumers: self.consumers.values().cloned().collect(),
      nfs_opens: self.nfs_opens.values().cloned().collect(),
      nfs_delegations: self.nfs_delegations.values().cloned().collect(),
      seal_keys: self.seal_keys.values().cloned().collect(),
      nfs_locks: self.nfs_locks.values().cloned().collect(),
      nfs_clients: self.nfs_clients.values().cloned().collect(),
      nfs_sessions: self.nfs_sessions.values().cloned().collect(),
      nfs_calls: self.nfs_calls.values().cloned().collect(),
      nfs_purges: self.nfs_purges.iter().copied().collect(),
      green_origin_refs: self
        .green_origin_refs
        .iter()
        .map(|(green, origin)| GreenOriginRef {
          green: *green,
          origin: *origin,
        })
        .collect(),
      green_checkpoints: self
        .green_checkpoints
        .iter()
        .map(|(green, (version, bytes))| GreenCheckpoint {
          green: *green,
          version: *version,
          bytes: bytes.clone(),
        })
        .collect(),
      green_adoptions: self.green_adoptions.values().copied().collect(),
      captured_bases: self.captured_bases.iter().copied().collect(),
      carried_captures: self
        .carried
        .iter()
        .map(
          |(volume, (carried, mark))| crate::catalog::CarriedCaptureRecord {
            volume: *volume,
            carried: carried.clone(),
            mark: *mark,
          },
        )
        .collect(),
      repositories: self.repositories.repositories().cloned().collect(),
      repository_branches: self.repositories.bindings().collect(),
      repository_members: self.repositories.memberships().collect(),
      review_comments: self.reviews.comment_records().cloned().collect(),
      review_approvals: self.reviews.approval_records().cloned().collect(),
      repository_registers: self.repositories.registers().collect(),
      atomic_records: self.repositories.atomic_held(),
      identities: self.identities.values().cloned().collect(),
      credentials: self.credentials.records().cloned().collect(),
      work_logs: self
        .work_logs
        .iter()
        .map(|(work, entries)| WorkLog {
          work: *work,
          entries: entries.clone(),
        })
        .collect(),
      name_aliases: self.name_aliases.values().cloned().collect(),
      work_floors: self.work_floors().collect(),
      pending_submits: self.pending_submits.values().cloned().collect(),
      pending_creations: self.pending_creations.values().cloned().collect(),
      expired_attachments: self.expired_attachments.iter().copied().collect(),
      audit_next_seq: self.audit_next_seq,
      audit_dropped: self.audit_dropped,
      grants_issued_through: self.grants_issued_through,
      volume_counter: self.volume_counter,
      audit_outbox: self.audit_outbox.iter().cloned().collect(),
      audit_outbox_next: self.audit_outbox_next,
      audit_received: self
        .audit_received
        .iter()
        .map(|(origin, through)| crate::catalog::AuditReceivedMark {
          origin: *origin,
          through: *through,
        })
        .collect(),
      nfs_instance: self.nfs_instance,
      tombstones: self.tombstones().collect(),
      green_chains: {
        // Every green with a chain or an origin, in id order, so a base-seeded green with no
        // increment yet is carried too.
        let greens: std::collections::BTreeSet<VolumeId> = self
          .green_chains
          .keys()
          .chain(self.green_origins.keys())
          .copied()
          .collect();
        greens
          .into_iter()
          .map(|green| GreenChain {
            green,
            increments: self.green_chains.get(&green).cloned().unwrap_or_default(),
            origin: self.green_origins.get(&green).cloned(),
          })
          .collect()
      },
    }
  }

  /// Restores the enrolled consumers and the principals' tool identities (§4.13, A-664) and the owed tombstones
  /// (AUD-29-43) a snapshot carries.
  fn restore_consumers_and_tombstones(&mut self, snapshot: &PartitionSnapshot) {
    for c in &snapshot.consumers {
      self.consumers.insert(c.consumer, c.clone());
    }
    for record in &snapshot.identities {
      self
        .identities
        .insert(record.principal.key(), record.clone());
    }
    for tombstone in &snapshot.tombstones {
      self.tombstones.insert(tombstone.volume, tombstone.sequence);
    }
  }

  /// Restores the expired CLI attachments a snapshot carries (A-230).
  fn restore_expired_attachments(&mut self, snapshot: &PartitionSnapshot) {
    self.expired_attachments = snapshot.expired_attachments.iter().copied().collect();
  }

  /// Restores the works' records a snapshot carries (A-247): their floors, pending submits and pending creations.
  fn restore_work_records(&mut self, snapshot: &PartitionSnapshot) {
    for pending in &snapshot.pending_creations {
      self
        .pending_creations
        .insert(pending.name.clone(), pending.clone());
    }
    for pending in &snapshot.pending_submits {
      self.pending_submits.insert(pending.work, pending.clone());
    }
    for floor in &snapshot.work_floors {
      self.set_work_floor(floor.green, floor.work, floor.base);
    }
  }

  /// Restores the name aliases a snapshot carries (A-236).
  fn restore_name_aliases(&mut self, snapshot: &PartitionSnapshot) {
    for record in &snapshot.name_aliases {
      self
        .name_aliases
        .insert(record.name.clone(), record.clone());
    }
  }

  /// Restores the NFSv4 records and instance a snapshot carries (§4.6 A-37).
  fn restore_nfs(&mut self, snapshot: &PartitionSnapshot) {
    for record in &snapshot.nfs_opens {
      self.nfs_opens.insert(record.other, record.clone());
    }
    for record in &snapshot.nfs_delegations {
      self.nfs_delegations.insert(record.other, record.clone());
    }
    for record in &snapshot.nfs_locks {
      self.nfs_locks.insert(record.other, record.clone());
    }
    for record in &snapshot.nfs_clients {
      self.nfs_clients.insert(record.clientid, record.clone());
    }
    for record in &snapshot.nfs_sessions {
      self.nfs_sessions.insert(record.sessionid, record.clone());
    }
    for record in &snapshot.nfs_calls {
      self.nfs_calls.insert(record.key, record.clone());
    }
    for record in &snapshot.nfs_purges {
      self.nfs_purges.insert(*record);
    }
    self.nfs_instance = snapshot.nfs_instance;
    for record in &snapshot.seal_keys {
      self.seal_keys.insert(record.owner, record.clone());
    }
  }

  fn restore_completions(&mut self, completions: &[ClientCompletions]) {
    for c in completions {
      let w = self.completions.entry((c.origin, c.client)).or_default();
      for r in &c.records {
        w.record(r.sequence, r.result.clone());
      }
      for a in &c.awaiting {
        w.await_version(a.sequence, a.green.bytes, a.version);
      }
      if let Some(up_to) = c.acknowledged_up_to {
        // A fresh window: the snapshot's open ids are restored exactly (each at or below its watermark, as every
        // acknowledgement applied had them).
        w.acknowledge(up_to, &c.owed);
      }
      // The routes the records carry (A-288), once the window is whole: a snapshot holds only retained records.
      for r in &c.records {
        if r.work.is_some() {
          self.route_completion((c.origin, c.client, r.sequence), r.work);
        }
      }
      self.prune_routes(c.origin, c.client);
    }
  }

  /// Sets the work the completion `key` routes to (A-288), or clears it with `None`, keeping each work's count.
  fn route_completion(&mut self, key: (u64, u32, u32), work: Option<VolumeId>) {
    let previous = match work {
      Some(work) => self.routed_completions.insert(key, work),
      None => self.routed_completions.remove(&key),
    };
    if let Some(previous) = previous
      && let Some(count) = self.routed_by_work.get_mut(&previous)
    {
      *count = count.saturating_sub(1);
      if *count == 0 {
        self.routed_by_work.remove(&previous);
      }
    }
    if let Some(work) = work {
      let count = self.routed_by_work.entry(work).or_insert(0);
      *count = count.saturating_add(1);
    }
  }

  /// Drops the routes of `(origin, client)`'s completions its window no longer retains (A-288): an acknowledgement
  /// released them, so no resend of one is answered here any more. One range lookup for a client with no routes.
  fn prune_routes(&mut self, origin: u64, client: u32) {
    let keys = (origin, client, u32::MIN)..=(origin, client, u32::MAX);
    if self.routed_completions.range(keys.clone()).next().is_none() {
      return;
    }
    let retained: BTreeSet<u32> = self
      .completions
      .get(&(origin, client))
      .map(|window| {
        window
          .retained_entries()
          .map(|(sequence, _)| sequence)
          .collect()
      })
      .unwrap_or_default();
    let released: Vec<(u64, u32, u32)> = self
      .routed_completions
      .range(keys)
      .map(|(key, _)| *key)
      .filter(|(_, _, sequence)| !retained.contains(sequence))
      .collect();
    for key in released {
      self.route_completion(key, None);
    }
  }

  /// A partition rebuilt from a snapshot: every index and the wheel rebuilt.
  pub fn from_snapshot(
    snapshot: &PartitionSnapshot,
    caps: PartitionCaps,
    now_ns: u64,
  ) -> Result<Partition, DbError> {
    let mut p = Partition::new(caps, now_ns);
    p.client_id_high_water = snapshot.client_id_high_water;
    for v in &snapshot.volumes {
      p.insert_volume(v.clone())?;
    }
    for s in &snapshot.snapshots {
      p.insert_snapshot(s.clone())?;
    }
    for e in &snapshot.lineage {
      p.lineage.insert(&e.child.bytes, e.clone());
    }
    for a in &snapshot.attachments {
      let h = p.attachments.insert(a.clone())?;
      p.attachment_index.insert(&a.id.to_be_bytes(), h);
      p.index_term(a);
    }
    p.restore_completions(&snapshot.completions);
    for g in &snapshot.grants {
      p.grants.insert(g.id, g.clone());
    }
    for l in &snapshot.landing_leases {
      p.landing_leases.insert(l.target.as_bytes(), l.clone());
    }
    for l in &snapshot.landings {
      p.landings.insert(l.id, l.clone());
    }
    p.audit = snapshot.audit.iter().cloned().collect();
    p.audit_dropped = snapshot.audit_dropped;
    p.audit_next_seq = snapshot.audit_next_seq;
    p.grants_issued_through = snapshot.grants_issued_through;
    p.volume_counter = snapshot.volume_counter;
    p.audit_outbox = snapshot.audit_outbox.iter().cloned().collect();
    p.audit_outbox_next = snapshot.audit_outbox_next;
    p.audit_received = snapshot
      .audit_received
      .iter()
      .map(|mark| (mark.origin, mark.through))
      .collect();
    p.restore_merge(snapshot);
    p.restore_nfs(snapshot);
    p.restore_name_aliases(snapshot);
    p.restore_work_records(snapshot);
    p.restore_expired_attachments(snapshot);
    p.restore_consumers_and_tombstones(snapshot);
    p.captured_bases = snapshot.captured_bases.iter().copied().collect();
    p.carried = snapshot
      .carried_captures
      .iter()
      .map(|record| (record.volume, (record.carried.clone(), record.mark)))
      .collect();
    p.repositories = crate::repository::Repositories::restore(
      &snapshot.repositories,
      &snapshot.repository_branches,
      &snapshot.repository_members,
      &snapshot.repository_registers,
      &snapshot.atomic_records,
    )?;
    // The repositories' own state is charged to the merge budget as it was when recorded (A-603).
    p.green_chain_bytes = p
      .green_chain_bytes
      .saturating_add(p.repositories.state_bytes());
    // The review records, charged to the merge byte budget as when they were recorded (A-596).
    p.reviews = crate::review::Reviews::restore(
      &snapshot.review_comments,
      &snapshot.review_approvals,
      |id| p.repositories.repository(id).is_some(),
    )?;
    p.green_chain_bytes = p.green_chain_bytes.saturating_add(p.reviews.bytes());
    // The version-control credentials, charged to the table byte budget as when each was enrolled (A-625 part 3).
    p.credentials = crate::credentials::Credentials::restore(&snapshot.credentials)?;
    p.green_chain_bytes = p.green_chain_bytes.saturating_add(p.credentials.bytes());
    Ok(p)
  }

  /// Restores the merge plane a snapshot carries (§4.16): every green's chain and origin, every held origin reference
  /// (A-217) and every work's log (A-207), each charged to the merge byte budget as when it was recorded.
  fn restore_merge(&mut self, snapshot: &PartitionSnapshot) {
    for chain in &snapshot.green_chains {
      self.restore_green_chain(chain);
    }
    for held in &snapshot.green_origin_refs {
      self.green_origin_refs.insert(held.green, held.origin);
    }
    for adoption in &snapshot.green_adoptions {
      self.green_adoptions.insert(adoption.green, *adoption);
    }
    for checkpoint in &snapshot.green_checkpoints {
      self.green_chain_bytes = self
        .green_chain_bytes
        .saturating_add(checkpoint.bytes.len());
      self.green_checkpoints.insert(
        checkpoint.green,
        (checkpoint.version, checkpoint.bytes.clone()),
      );
    }
    for log in &snapshot.work_logs {
      self.green_chain_bytes = self
        .green_chain_bytes
        .saturating_add(log.entries.iter().map(Vec::len).sum());
      self.work_logs.insert(log.work, log.entries.clone());
    }
  }

  /// Restores one green's chain and origin from a snapshot, charging both to the chain budget; a
  /// green carried with no increment (a seeded green before its first merge) gets no empty chain
  /// entry, so the restored tables equal the live ones.
  fn restore_green_chain(&mut self, chain: &GreenChain) {
    let bytes: usize = chain.increments.iter().map(Vec::len).sum();
    self.green_chain_bytes = self.green_chain_bytes.saturating_add(bytes);
    if !chain.increments.is_empty() {
      self
        .green_chains
        .insert(chain.green, chain.increments.clone());
    }
    if let Some(origin) = &chain.origin {
      self.green_chain_bytes = self.green_chain_bytes.saturating_add(origin.len());
      self.green_origins.insert(chain.green, origin.clone());
    }
  }
}

#[cfg(test)]
#[path = "tests/identities.rs"]
mod identities_tests;
