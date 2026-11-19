//! The operations: every local mutation of a partition, as the log records it (§4.8 `OpLog`
//! "every local mutation"). An operation carries what `apply` needs and nothing it decides:
//! the guard (`Partition::check`) runs before the append, so a recorded operation applies
//! unconditionally and replay is deterministic.

use slates_wire::Wire;

use crate::catalog::{
  AccessEntry, AttachmentRecord, AuditRecord, BaseRecord, BranchRecord, CompletionRecord,
  ConsumerRecord, GrantRecord, GrantState, LandingLeaseRecord, LandingRecord, LandingState,
  LeaseRecord, LineageEdge, NameAliasRecord, NfsClientRecord, NfsDelegationRecord, NfsLockRecord,
  NfsOpenRecord, PlacementState, Principal, RepositoryId, RepositoryRecord, ReviewApprovalRecord,
  ReviewCommentRecord, SizeClass, SnapshotId, SnapshotRecord, Tombstone, VolumeId, VolumeRecord,
  VolumeState,
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
  /// `Binding → Bound`; GAP-A9-4): its form becomes `ChosenPath { path }`, a scoped mount's keeps its scope and a
  /// bound mount's its identity and scope (A-115). Appended.
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
    /// Every sequence up to and including this one, except `owed`.
    up_to: u32,
    /// The ids at or below `up_to` the client still owes (awaited, or held for a resend under the same id): kept open
    /// rather than released (§4.9). An open id this does not name is closed; a closed one is never reopened.
    owed: Vec<u32>,
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
  /// A sealing key was made and recorded wrapped by its parent (A-92). Appended.
  SealKeySet {
    /// The record.
    record: crate::catalog::SealKeyRecord,
  },
  /// A sealing key was destroyed: its one wrapped copy is gone, so everything sealed under it is erased (seal.md
  /// §3.1, cryptographic erase). Appended.
  SealKeyDestroyed {
    /// Whose key.
    owner: crate::catalog::SealKeyOwner,
  },
  /// An NFSv4.1 session was recorded or changed at its client's listener (A-127). Appended.
  NfsSessionSet {
    /// The record.
    record: crate::catalog::NfsSessionRecord,
  },
  /// An NFSv4.1 session was destroyed at its client's listener (A-127). Appended.
  NfsSessionCleared {
    /// Format: the session id (16 bytes).
    sessionid: [u8; 16],
  },
  /// An NFSv4.1 call record was recorded or replaced with the file-state change one of its calls made (A-127).
  /// Appended.
  NfsCallSet {
    /// The record.
    record: crate::catalog::NfsCallRecord,
  },
  /// Every call record of a destroyed NFSv4.1 session was dropped (A-127). Appended.
  NfsCallsCleared {
    /// Format: the session id (16 bytes).
    sessionid: [u8; 16],
  },
  /// An NFSv4 purge became owed to every owner, with the drop that made it (2026-10-10). Appended.
  NfsPurgeOwed {
    /// The purge.
    record: crate::catalog::NfsPurgeRecord,
  },
  /// An owed NFSv4 purge was acknowledged by every owner (2026-10-10). Appended.
  NfsPurgeSettled {
    /// The purge.
    record: crate::catalog::NfsPurgeRecord,
  },
  /// A volume's live shipping was turned on or off (§4.10, A-200; 2026-11-04). Appended.
  LiveShippingChanged {
    /// The volume.
    id: VolumeId,
    /// Whether it is live-shipped now.
    live_shipped: bool,
  },
  /// A work volume's change since its last restatement, as the verb applied it (A-207; §4.16; D-18: a local
  /// acknowledgement promises daemon-restart survival only when it is recoverable from anchor-owned RAM). The entry
  /// is the server's encoded work-log entry (an edit or a declaration); the database stores it opaquely, appended to
  /// the work's log, and the server replays the log onto the recovered work. Counted against the merge byte budget the
  /// green chains share. Appended (2026-11-04).
  WorkLogged {
    /// The work volume.
    work: VolumeId,
    /// The encoded entry.
    entry: Vec<u8>,
  },
  /// A green volume's origin, recorded as a reference to the complete immutable snapshot it starts from (A-217;
  /// §4.16; the A-9 requirement that a chain starts "from scratch or a complete immutable base"): the base volume,
  /// the snapshot, and the identity of the origin walked from it, so recovery walks the same snapshot again and
  /// refuses one that no longer gives the same origin. The green holds the snapshot while it lives, so nothing frees
  /// it under the reference. Recorded once, before any increment. Appended (2026-11-05).
  GreenOriginHeld {
    /// The green volume.
    green: VolumeId,
    /// The snapshot it names.
    origin: crate::partition::OriginRef,
  },
  /// A work volume restated whole (§4.16): its base, the files it holds of its own and its journal, after an accepted
  /// submit or a rebase. Replaces the work's log with this one entry. Appended (2026-11-04).
  WorkRestated {
    /// The work volume.
    work: VolumeId,
    /// The encoded restatement.
    entry: Vec<u8>,
  },
  /// A name was reserved at its name's partition for a clone another partition is about to make (A-236): the name is
  /// held from here, and resolves nothing until confirmed. Appended.
  NameAliasReserved {
    /// The alias, unconfirmed.
    record: NameAliasRecord,
  },
  /// A reserved name's clone was made: the alias resolves from here (A-236). Appended.
  NameAliasConfirmed {
    /// The name.
    name: String,
  },
  /// A name's alias was released: its clone was destroyed, never made, or no longer holds the name (A-236). Appended.
  NameAliasReleased {
    /// The name.
    name: String,
  },
  /// A CLI attachment's term was renewed by a verb naming it (A-230). Appended.
  AttachmentRenewed {
    /// The attachment.
    id: u64,
    /// When its renewed term lapses.
    expires_ns: u64,
  },
  /// A CLI attachment's term lapsed: it is removed, and its id kept among the expired so a later use is refused by kind
  /// (A-230). Appended.
  AttachmentExpired {
    /// The attachment.
    id: u64,
  },
  /// A green's checkpoint (W3; §4.16 "Deltas older than the derived retention fold into checkpoint deltas"): the
  /// green's state at `version` beyond its origin, encoded (`slates_merge::checkpoint`), held in place of every
  /// increment up to `version`, which applying it drops. Refused at or below the checkpoint held and past the head.
  /// Appended (2026-11-06).
  GreenCheckpointed {
    /// The green volume.
    green: VolumeId,
    /// The version the checkpoint is the green's state at.
    version: u64,
    /// The encoded checkpoint.
    checkpoint: Vec<u8>,
  },
  /// A live-shipped volume's shipper committed its holder set (A-240): carried by the catalog register to `f + 1`
  /// before the shipper's first entry. Appended (2026-11-06).
  LiveHoldersCommitted {
    /// The volume.
    id: VolumeId,
    /// The shipper's epoch and holders.
    holders: crate::catalog::LiveHolders,
  },
  /// A request ran and waits for a version of a green to commit before it completes (§4.16 "Commit", A-252): logged with
  /// the version's append, in one transaction, so a resend under its id after a restart meets the wait and never runs
  /// again. Its completion, recorded at the commit, replaces it. Appended (2026-11-08).
  CompletionAwaiting {
    /// The completion key's origin: this node for a local client, the authenticated forwarding peer for a forward.
    origin: u64,
    /// The client.
    client: u32,
    /// The sequence.
    sequence: u32,
    /// The green.
    green: VolumeId,
    /// The version it waits for.
    version: u64,
  },
  /// A volume's write policy was set by the launcher or the harness, through the issuer (A-285). Appended (2026-11-18).
  VolumeWritePolicySet {
    /// The volume.
    id: VolumeId,
    /// The policy.
    policy: crate::catalog::WritePolicy,
  },
  /// An enrolled consumer's write policy was set by its launcher (A-285). Appended (2026-11-18).
  ConsumerWritePolicySet {
    /// The consumer.
    consumer: u64,
    /// The policy.
    policy: crate::catalog::WritePolicy,
  },
  /// A work's base is held for it on its green's partition, wherever the work lives (A-247): made at the work's
  /// creation, moved by its accept and rebase. Appended (2026-11-18), with the five after it: each variant's index is
  /// its tag on the log, so A-247's records follow every variant main had when they landed.
  WorkFloorSet {
    /// The green.
    green: VolumeId,
    /// The work.
    work: VolumeId,
    /// The version the work is based on.
    base: u64,
  },
  /// A work's creation waits for its green's owner to admit it, holding its name and its id (A-247). Appended.
  WorkCreationPending {
    /// The creation.
    record: crate::catalog::PendingCreationRecord,
  },
  /// A work's pending creation ended: made, refused, or dropped at a start (A-247). Appended.
  WorkCreationSettled {
    /// The name.
    name: String,
  },
  /// A work's submit went to its green's owner and waits to be settled on the work (A-247). Appended.
  WorkSubmitPending {
    /// The submit.
    record: crate::catalog::PendingSubmitRecord,
  },
  /// A work's pending submit was settled (A-247). Appended.
  WorkSubmitSettled {
    /// The work.
    work: VolumeId,
  },
  /// A work's floor is released: it was destroyed, or found gone at recovery (A-247). Appended.
  WorkFloorDropped {
    /// The green.
    green: VolumeId,
    /// The work.
    work: VolumeId,
  },
  /// A volume's trusted signers were set by its launcher (signed work, A-321): the whole set, replacing the last.
  /// Appended (2026-11-17).
  VolumeSignersSet {
    /// The volume.
    id: VolumeId,
    /// The public keys, in SSH wire form, in the order set.
    keys: Vec<Vec<u8>>,
  },
  /// A volume this partition lost to a takeover is taken over here again (A-362): its stale record goes, with its
  /// snapshots and lineage, so the state the takeover adopted is recorded in its place by the `VolumeCreated` that
  /// follows. Unlike `VolumeDestroyed` it leaves no tombstone (the volume lives on, its registers adopted here) and erases
  /// no key (the adopted lineage is the volume's own). Its attachments are ended before it. Appended (2026-11-18).
  VolumeRelinquished {
    /// The volume.
    id: VolumeId,
  },
  /// A green this node took over from its departed owner, recorded here by adopting this node's own holder ledger of it
  /// (§4.16 owner-loss recovery; A-333): the green's catalog record, and versions 0 to `version` as the merge records
  /// this node accepted as one of the green's holders and the inputs it holds for them, which it keeps while the green
  /// lives here. So the partition records neither the green's origin nor those versions' increments, and its chain holds
  /// only the versions after `version`. A kernel-sized origin re-recorded as bytes was refused at the partition's merge
  /// budget, which lost the green with its owner. One operation, so the green is never recorded without its adoption:
  /// checked as a volume's creation is. Appended (2026-11-18).
  GreenAdopted {
    /// The green's catalog record, under its original id and name.
    record: crate::catalog::VolumeRecord,
    /// The adopted version: the green's head when this node took it over.
    version: u64,
    /// The green's head identity at `version`, which a replica rebuilt from the ledger must reproduce.
    identity: [u8; 32],
    /// Version 0's inputs manifest identity, as the ledger's version 0 record names it (the origin's canonical encoding,
    /// archived); `None` for a green with no origin.
    origin: Option<[u8; 32]>,
  },
  /// An audit record this partition owes the control partition (A-265's crossing, made durable 2026-11-18): kept in the
  /// partition's outbox, numbered by its own crossing sequence in `apply`, until the control partition acknowledges it,
  /// so it commits with the atom that made it and a crash before its batch arrives loses nothing. Appended.
  AuditOwed {
    /// The record; its `seq` is set in `apply`.
    record: AuditRecord,
  },
  /// The control partition appended this partition's owed records before crossing sequence `through` (A-265): they
  /// leave the outbox. Appended.
  AuditDelivered {
    /// The first crossing sequence the control partition has not appended.
    through: u64,
  },
  /// On the control partition: a batch of the records partition `origin` owes, the first at crossing sequence `first`
  /// (A-265). `apply` appends only those past what `origin` already delivered, and marks `origin` delivered through the
  /// batch's end, so a batch sent again, after a crash or a lost acknowledgement, appends nothing twice. Appended.
  AuditBatchAppended {
    /// The partition that owed the records.
    origin: u16,
    /// The crossing sequence of the batch's first record.
    first: u64,
    /// The records, in crossing order.
    records: Vec<AuditRecord>,
  },
  /// A volume moved to another fleet member left this node (§4.10 "Ownership follows the writer", A-291): its record,
  /// snapshots, lineage and this node's copies of its keys go, as a destroy's do, but no tombstone stays. Its registers
  /// belong to the member the configuration's moved-object exception names, which adopted them, so nothing here may
  /// close them. Appended (2026-11-18).
  VolumeReleased {
    /// The volume.
    id: VolumeId,
  },
  /// A snapshot was captured as a complete immutable base (§4.15 `capture_base`, A-291): its tree holds every name,
  /// byte and attribute, none read from the host directory, so a clone of it is made with no base directory. Kept while
  /// the snapshot and its volume are. Appended (2026-11-18).
  BaseCaptured {
    /// The volume.
    volume: VolumeId,
    /// The snapshot.
    snapshot: crate::catalog::SnapshotId,
  },
  /// A captured clone's move carried its capture (A-503): what the volume's catalog ships from now on, and the snapshot
  /// here holding the head the move hands over (its mark, should the move thaw). Raises the catalog register's sequence,
  /// so the catalog ships it before the move is reported. Replaces any carried capture the volume held. Appended
  /// (2026-11-18).
  CaptureCarried {
    /// The volume.
    volume: VolumeId,
    /// What the move carries.
    carried: crate::catalog::CarriedRecord,
    /// The snapshot holding the head it is relative to.
    mark: Option<crate::catalog::SnapshotId>,
  },
  /// A volume a move brought here was materialized with the carried capture its adopted catalog holds (A-503), its mark
  /// the snapshot it was materialized as when that is the head the capture was carried against, else none. The catalog
  /// is the adopted one, unchanged, so its sequence stays. Appended (2026-11-18).
  CaptureAdopted {
    /// The volume.
    volume: VolumeId,
    /// What its adopted catalog carries.
    carried: crate::catalog::CarriedRecord,
    /// Its snapshot holding that head, when it holds one.
    mark: Option<crate::catalog::SnapshotId>,
  },
  /// A repository was created, its namespace empty (§4.17.1, A-515). Appended.
  RepositoryCreated {
    /// The record.
    record: RepositoryRecord,
  },
  /// A branch name of a repository was bound to a green of its partition (§4.17.1, A-515). Appended.
  RepositoryBranchAdded {
    /// The binding.
    record: BranchRecord,
  },
  /// A branch name left its repository's namespace; its green is untouched, so binding the name to it again restores
  /// the branch (§4.17.1 "Deleting a branch", A-515). Appended.
  RepositoryBranchRetired {
    /// The repository.
    repository: RepositoryId,
    /// The name.
    name: String,
  },
  /// A repository's default branch was set, or cleared with `None` (§4.17.1, A-515). Appended.
  RepositoryDefaultSet {
    /// The repository.
    repository: RepositoryId,
    /// The branch name, bound or not yet (an unborn default).
    branch: Option<String>,
  },
  /// A repository left, with every name of its namespace; its greens are untouched, each destroyed by its own verb
  /// (§4.17.1, A-515). Appended.
  RepositoryDestroyed {
    /// The repository.
    repository: RepositoryId,
  },
  /// A host mount's attachment was seen mounted (A-540): the kernel's id of its mount, kept in the attachment's record
  /// so a successor's start can judge whether the kernel let that mount go while no daemon ran. Appended.
  AttachmentMountSeen {
    /// The attachment.
    id: u64,
    /// The kernel's id of its mount, as its mount table names it.
    mount: u64,
  },
  /// A green joined its repository's lineage (§4.17.1 item 4, A-575): the repository's first green as its root, or a
  /// green made at a version of another member. Appended.
  RepositoryGreenJoined {
    /// The membership.
    record: crate::catalog::MemberRecord,
  },
  /// A review comment was written on a request (§4.17.1 shared piece 6, A-596): anchored to a path, the whole lines a
  /// byte range covers, and the version of the request's work it stands on. Appended.
  ReviewCommented {
    /// The comment.
    record: ReviewCommentRecord,
  },
  /// A review comment moved with its request's work to a later version, its lines unchanged: now `len` bytes at `start`
  /// of `version` (A-596). Appended.
  ReviewCommentMoved {
    /// The repository.
    repository: RepositoryId,
    /// The request.
    request: VolumeId,
    /// The comment's number.
    id: u64,
    /// Its lines' first byte at `version`.
    start: u64,
    /// The bytes they cover there.
    len: u64,
    /// The version it now stands on.
    version: u64,
  },
  /// A change at `version` met a review comment's lines (A-596): it is outdated, where it last stood, never moved again.
  /// Appended.
  ReviewCommentOutdated {
    /// The repository.
    repository: RepositoryId,
    /// The request.
    request: VolumeId,
    /// The comment's number.
    id: u64,
    /// The version whose change met its lines.
    version: u64,
  },
  /// A request was approved at the version its approver saw; the approver's earlier approval, if any, is replaced
  /// (A-596). Appended.
  ReviewApproved {
    /// The approval.
    record: ReviewApprovalRecord,
  },
  /// A change at `version` met what the repository's policy says an approval covers: the approval is stale (A-596).
  /// Appended.
  ReviewApprovalStaled {
    /// The repository.
    repository: RepositoryId,
    /// The request.
    request: VolumeId,
    /// Who approved.
    approver: Principal,
    /// The version whose change made it stale.
    version: u64,
  },
  /// A request's review records left with it, its review merged or closed (A-596). Appended.
  ReviewClosed {
    /// The repository.
    repository: RepositoryId,
    /// The request.
    request: VolumeId,
  },
  /// An entry of a repository's register (§4.17.1 item 2, shared piece 2; A-603): one durable step committing several of
  /// its greens' versions, greens joining its lineage, its namespace's changes and its own state's (Mercurial's phase
  /// roots and obsolescence markers), its checks read at its position first; all of it or none. Every other operation
  /// that changes a repository is an entry of one change and takes a position too. Appended.
  RepositoryAdvanced {
    /// The entry.
    entry: crate::catalog::RepositoryEntry,
  },
  /// A repository's policy was set whole by its owner (§4.17.1 "Policy", A-622): its rules and hooks replaced, its
  /// version the one after the version its setter read. An entry of the repository's register of this one change, so
  /// it takes the register's next position (A-603). Appended.
  RepositoryPolicySet {
    /// The repository.
    repository: RepositoryId,
    /// The policy, its version the next.
    policy: crate::catalog::RepositoryPolicy,
  },
}

impl Op {
  /// The operation's name, for counters and the audit.
  pub fn name(&self) -> &'static str {
    match self {
      Op::VolumeCreated { .. } => "volume_created",
      Op::WorkLogged { .. } => "work_logged",
      Op::GreenOriginHeld { .. } => "green_origin_held",
      Op::WorkRestated { .. } => "work_restated",
      Op::GreenCheckpointed { .. } => "green_checkpointed",
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
      Op::AttachmentMountSeen { .. } => "attachment_mount_seen",
      Op::RepositoryGreenJoined { .. } => "repository_green_joined",
      Op::ReviewCommented { .. } => "review_commented",
      Op::ReviewCommentMoved { .. } => "review_comment_moved",
      Op::ReviewCommentOutdated { .. } => "review_comment_outdated",
      Op::ReviewApproved { .. } => "review_approved",
      Op::ReviewApprovalStaled { .. } => "review_approval_staled",
      Op::ReviewClosed { .. } => "review_closed",
      Op::RepositoryAdvanced { .. } => "repository_advanced",
      Op::RepositoryPolicySet { .. } => "repository_policy_set",
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
      Op::GreenAdopted { .. } => "green_adopted",
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
      Op::SealKeySet { .. } => "seal_key_set",
      Op::SealKeyDestroyed { .. } => "seal_key_destroyed",
      Op::NfsSessionSet { .. } => "nfs_session_set",
      Op::NfsSessionCleared { .. } => "nfs_session_cleared",
      Op::NfsCallSet { .. } => "nfs_call_set",
      Op::NfsCallsCleared { .. } => "nfs_calls_cleared",
      Op::NfsPurgeOwed { .. } => "nfs_purge_owed",
      Op::NfsPurgeSettled { .. } => "nfs_purge_settled",
      Op::NameAliasReserved { .. } => "name_alias_reserved",
      Op::NameAliasConfirmed { .. } => "name_alias_confirmed",
      Op::NameAliasReleased { .. } => "name_alias_released",
      Op::WorkFloorSet { .. } => "work_floor_set",
      Op::WorkFloorDropped { .. } => "work_floor_dropped",
      Op::WorkCreationPending { .. } => "work_creation_pending",
      Op::WorkCreationSettled { .. } => "work_creation_settled",
      Op::WorkSubmitPending { .. } => "work_submit_pending",
      Op::WorkSubmitSettled { .. } => "work_submit_settled",
      Op::AttachmentRenewed { .. } => "attachment_renewed",
      Op::AttachmentExpired { .. } => "attachment_expired",
      Op::LiveShippingChanged { .. } => "live_shipping_changed",
      Op::LiveHoldersCommitted { .. } => "live_holders_committed",
      Op::CompletionAwaiting { .. } => "completion_awaiting",
      Op::VolumeWritePolicySet { .. } => "volume_write_policy_set",
      Op::ConsumerWritePolicySet { .. } => "consumer_write_policy_set",
      Op::VolumeSignersSet { .. } => "volume_signers_set",
      Op::VolumeRelinquished { .. } => "volume_relinquished",
      Op::AuditOwed { .. } => "audit_owed",
      Op::AuditDelivered { .. } => "audit_delivered",
      Op::AuditBatchAppended { .. } => "audit_batch_appended",
      Op::VolumeReleased { .. } => "volume_released",
      Op::BaseCaptured { .. } => "base_captured",
      Op::CaptureCarried { .. } => "capture_carried",
      Op::CaptureAdopted { .. } => "capture_adopted",
      Op::RepositoryCreated { .. } => "repository_created",
      Op::RepositoryBranchAdded { .. } => "repository_branch_added",
      Op::RepositoryBranchRetired { .. } => "repository_branch_retired",
      Op::RepositoryDefaultSet { .. } => "repository_default_set",
      Op::RepositoryDestroyed { .. } => "repository_destroyed",
    }
  }
}
