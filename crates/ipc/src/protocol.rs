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

/// A principal on the wire (§4.13), as a `share` names one: a host's account, or an enrolled consumer under one. A host
/// account means something only on its host (A-237), so each names its host by that host's certificate anchor, as
/// `slates status` shows it (`DaemonReport::anchor`). The daemon maps it to the catalog's principal
/// (`verbs::to_db_principal`); a certificate or SID principal is never named by a client, so neither is on the wire.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum Principal {
  /// A Unix user of a host.
  Uid {
    /// The host whose user it is: its certificate anchor.
    host: u64,
    /// The uid.
    uid: u32,
  },
  /// An enrolled consumer under a host's account.
  Consumer {
    /// The host whose account the consumer was enrolled under: its certificate anchor.
    host: u64,
    /// The account.
    account: u32,
    /// The consumer id.
    consumer: u64,
  },
}

/// A version-control credential, as `EnrollCredential` gives it (§4.13, §4.17.2 "Access"; A-625 part 3).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum CredentialMaterial {
  /// An SSH user key: its public key blob (RFC 4253 §6.6), the bytes a `.pub` line carries base64-encoded.
  SshKey {
    /// The blob.
    blob: Vec<u8>,
  },
  /// An HTTP token, Basic's password or a bearer token, by its SHA-256 alone: the token stays with the human the issuer
  /// showed it to, so it never crosses a channel.
  HttpToken {
    /// The token's SHA-256.
    id: [u8; 32],
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

/// A kind of write a policy governs (A-285).
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteAct {
  /// A merge (a work's submit into its green).
  Merge,
  /// A landing to the host's disk.
  Land,
}

/// How one kind of write is permitted: the mode the user gave the harness or agent (A-285).
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteMode {
  /// Written at once, with no approval asked (Claude Code's `bypassPermissions`, Codex's full access).
  Bypass,
  /// Answered with an approval request the harness presents; written once an approval binds the manifest.
  Ask,
  /// Refused (a mode that forbids writing, such as plan).
  Forbid,
}

/// A write policy: the mode for merges and the mode for landings (A-285), and whether merges must be signed (A-321).
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct WritePolicy {
  /// How a merge is permitted.
  pub merge: WriteMode,
  /// How a landing is permitted.
  pub land: WriteMode,
  /// Merges must carry a signature that verifies under a trusted signer (signed work, A-321): on a green, every merge
  /// into it; on a work, every merge from it. A requirement on what the volume accepts, never a permission: an unsigned
  /// or invalid merge is refused `SignatureRequired` even in bypass, and a verified one still follows its mode. A
  /// consumer's policy cannot set it (refused `BadRequest`): it is the volume's rule.
  pub require_signed_merges: bool,
}

/// The kind of key that signed (signed work, A-321): the ones the daemon's verifier accepts.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignerKey {
  /// `ssh-ed25519` (RFC 8709).
  Ed25519,
  /// `sk-ssh-ed25519@openssh.com`: a FIDO key, whose every signature carries the holder's touch.
  SecurityKeyEd25519,
}

/// Who signed (A-321): the key's kind and its SHA-256 fingerprint over its SSH wire form (`ssh-keygen -l -E sha256`
/// prints the base64 of these bytes after `SHA256:`).
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Signer {
  /// The key's kind.
  pub key: SignerKey,
  /// The key's fingerprint.
  pub fingerprint: [u8; 32],
}

/// Why a signature did not verify (A-321): `slates_server::signed_work::SignatureRefusal`, one to one.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignatureFault {
  /// The armour, the base64 or the blob's grammar is wrong.
  Malformed,
  /// Made under another namespace than `slates-work` (for a git commit, a file).
  Namespace,
  /// A key type, signature type or hash slates does not accept.
  Unsupported,
  /// The key is not one of the volume's trusted signers.
  UnregisteredKey,
  /// The signature does not verify over the act's statement.
  BadSignature,
  /// A security key signed without its holder's touch.
  NotTouched,
}

/// How an act's signature was judged (signed work, A-321): recorded with the merge or the landing as provenance and
/// answered, never a permission.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verification {
  /// The signature verified over the act's statement under a trusted signer.
  Verified {
    /// Who signed.
    signer: Signer,
  },
  /// The act carried no signature.
  Unsigned,
  /// The act carried a signature that did not verify.
  Invalid {
    /// Why.
    reason: SignatureFault,
  },
}

/// What a write policy is set on (A-285): an enrolled consumer (a session's agent) or a volume. The stricter of the
/// caller's consumer and the volume governs.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicySubject {
  /// An enrolled consumer.
  Consumer {
    /// The consumer id.
    consumer: u64,
  },
  /// A volume.
  Volume {
    /// The volume.
    volume: VolumeId,
  },
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

/// Which view of a volume a [`RequestBody::ReadRange`] reads (§4.16 "Attachments and versions"; §4.12
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
  /// One page of the paths a green changed strictly after `version` (§4.16, A-264): what a lagging work reconciles.
  /// Read from the green's change index in version order from `from` (`None` begins), each page ending where one reply
  /// chunk is full (§4.3) or where the shard's slice of its turn is spent, whichever comes first, so a page costs what
  /// it passes and never the green's tree. A page may hold no path and still carry a cursor; the client walks to the
  /// end. A path changed after `version` when the walk began is answered once, and once more if it changes again
  /// during the walk. A version past the head is refused `UnknownBase`.
  ChangedSince {
    /// The green.
    green: VolumeId,
    /// The base version to compare against.
    version: u64,
    /// Where the previous page stopped, or `None` for the first page.
    from: Option<ChangedCursor>,
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
  /// One page of the volumes the caller may read (§4.4 `list`), in volume-id order: `after` `None` begins, and each
  /// page's `next` continues it. A page holds what one reply chunk holds (§4.3), so the client walks a listing of any
  /// length; a volume that exists throughout the walk is listed exactly once. A read; never recorded.
  List {
    /// The last volume of the previous page, or `None` for the first page.
    after: Option<VolumeId>,
  },
  /// Acknowledge completions up to a sequence except the ids still owed (releases the others' records, §4.9).
  Acknowledge {
    /// Every sequence up to and including this one, except `owed`.
    up_to: u32,
    /// The ids at or below `up_to` the client still owes (awaited, or held for a resend under the same id), whose
    /// records stay: at most the ring's slots and the client's hold.
    owed: Vec<u32>,
  },
  /// One page of the base entry as the disk holds it now (§4.4 `read_base`, A-267): its bytes from `offset`, as many as
  /// one reply holds, with the file's size and its fingerprint's stamp; a client walks the pages to the size and
  /// refuses a file whose stamp moved between them. Neither a reply nor the daemon holds more than a page.
  ReadBase {
    /// The volume.
    volume: VolumeId,
    /// The path.
    path: String,
    /// Where the page starts: 0, then the bytes read so far.
    offset: u64,
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
    /// A signature over the landing's statement (signed work, A-321; `slates_server::signed_work::landing_statement`,
    /// signed under the namespace `slates-work`): judged under the volume's trusted signers against the manifest the
    /// landing writes (or presents), recorded with the landing's audit records and answered. It permits nothing: the
    /// landing follows its write mode.
    signature: Option<Vec<u8>>,
  },
  /// One page of the caller's grants (a read; creating a grant is off-ring), in grant-id order past `after`: `None`
  /// begins, and each page's `next` continues it. A page holds what one reply chunk holds (§4.3), so a principal's
  /// grants of any number are walked; a grant that exists throughout the walk is answered exactly once.
  Grants {
    /// The last grant of the previous page, or `None` for the first page.
    after: Option<u64>,
  },
  /// One page of the audit (§4.15, A-265): the records from sequence `since` on, in sequence order, as many as one
  /// reply chunk holds. One partition holds the audit, the control shard's, so the sequence numbers the daemon's
  /// records and a walk continues from one past the last record a page held. A read.
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
  /// Retired (A-267): a whole file in one reply, which past a reply chunk's bytes was refused "reply too large for the
  /// bulk area" after the daemon had read the whole file into its memory. A file is read a page at a time with
  /// `ReadRange`, which the client walks; this verb is refused `Unsupported` and kept only so the tags after it hold
  /// (D-15, `tests/wire_order.rs`).
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
  /// Continue this client's immutable daemon report (§4.14); offsets are encoded byte offsets.
  DaemonStatusNext {
    /// The request identity that captured the report.
    snapshot: u64,
    /// The first byte still needed.
    offset: u64,
  },
  /// Binds a host mount's attachment to the path the mount was established at (§4.4 `Binding →
  /// Bound`; GAP-A9-4): the requesting process mounted under the capability `attach` returned and
  /// reports where, so the record's form becomes the chosen path and `status` reports it. Refused for
  /// an attachment that is not a host mount's, or another principal's. Appended.
  BindMount {
    /// The attachment (a host mount's).
    attachment: u64,
    /// The mount point, as established.
    path: String,
  },
  /// Reserve a staging buffer of `len` bytes on a work's owner for an edit too large for one request (§4.12;
  /// a request rides one bulk chunk): the token `StagePut` fills and `EditStaged` applies. Charged to the owner's
  /// metadata budget until applied or expired; never recorded. Appended.
  StageBegin {
    /// The work volume the staged bytes are for.
    work: VolumeId,
    /// The bytes to be staged.
    len: u64,
  },
  /// Write the next `bytes` of a staging buffer at `offset` (which must be where the previous put ended). Never
  /// recorded. Appended.
  StagePut {
    /// The work volume the buffer is for.
    work: VolumeId,
    /// The buffer, as `StageBegin` named it.
    token: u64,
    /// Where these bytes go in the buffer.
    offset: u64,
    /// The bytes.
    bytes: Vec<u8>,
  },
  /// `Edit` with a full staging buffer as its inserted bytes: one splice, recorded as one edit, the buffer then
  /// released (§4.16). Appended.
  EditStaged {
    /// The work volume.
    work: VolumeId,
    /// The file.
    path: String,
    /// The offset.
    at: u64,
    /// Bytes removed at `at`.
    delete_len: u64,
    /// The buffer holding the bytes inserted at `at`.
    token: u64,
  },
  /// Read up to `max` bytes of a file from `offset` at a view (§4.12 `slates.fs.read`, paged: a reply rides one
  /// bulk chunk). A read; never recorded. Appended.
  ReadRange {
    /// The volume.
    volume: VolumeId,
    /// The file.
    path: String,
    /// The view.
    at: ReadAt,
    /// The first byte wanted.
    offset: u64,
    /// The most bytes wanted (the daemon answers fewer when its reply chunk holds fewer).
    max: u64,
  },
  /// List a directory's direct entries at a view (§4.12 `slates.fs.list`), one reply chunk at a time: `cursor` 0
  /// begins, and each page names the cursor that continues it. A read; never recorded. Appended.
  ReadDir {
    /// The volume.
    volume: VolumeId,
    /// The directory (`""` or `/` for the root).
    path: String,
    /// The view.
    at: ReadAt,
    /// Where to continue: 0 to begin, else the `next` of the previous page.
    cursor: u64,
  },
  /// Replace a plain volume's file at `path` with `bytes` (§4.12 `slates.fs.write`), under the caller's own write
  /// attachment of the volume: the file is created with `mode` when absent (its directory must exist), else truncated
  /// and rewritten, as one recorded verb. Appended.
  FsWrite {
    /// The volume (routes the verb to its owner).
    volume: VolumeId,
    /// The caller's write attachment of the volume.
    attachment: u64,
    /// The file.
    path: String,
    /// The file's whole new content.
    bytes: Vec<u8>,
    /// The permission bits a created file takes.
    mode: u32,
  },
  /// `FsWrite` whose content is a full staging buffer (`StageBegin`, `StagePut`), for a file larger than one
  /// request: one verb, so the file never holds half of it. Appended.
  FsWriteStaged {
    /// The volume.
    volume: VolumeId,
    /// The caller's write attachment of the volume.
    attachment: u64,
    /// The file.
    path: String,
    /// The staging buffer.
    token: u64,
    /// The permission bits a created file takes.
    mode: u32,
  },
  /// Remove the file, symbolic link or empty directory at `path` (§4.12 `slates.fs.remove`). Appended.
  FsRemove {
    /// The volume.
    volume: VolumeId,
    /// The caller's write attachment of the volume.
    attachment: u64,
    /// The name removed.
    path: String,
  },
  /// Rename `from` to `to`, replacing what `to` names as `rename(2)` does (§4.12 `slates.fs.move`). Appended.
  FsRename {
    /// The volume.
    volume: VolumeId,
    /// The caller's write attachment of the volume.
    attachment: u64,
    /// The source.
    from: String,
    /// The destination.
    to: String,
  },
  /// Make a directory (§4.12 `slates.fs.mkdir`). Appended.
  FsMkdir {
    /// The volume.
    volume: VolumeId,
    /// The caller's write attachment of the volume.
    attachment: u64,
    /// The directory.
    path: String,
    /// Its permission bits.
    mode: u32,
  },
  /// A read-ahead window of a file (§4.8 "Lookup" forwards; `ReadRange` across nodes): up to `max` bytes from
  /// `offset` at a view, answered as one `ReadPage`. Sent node to node only — the origin of a forwarded `ReadRange`
  /// asks its owner for a window and answers the client's following pages from it — and capped at the owner by a
  /// bounded window; a client asking it directly over its ring is answered as a `ReadRange`. A read; never recorded.
  /// Appended.
  ReadWindow {
    /// The volume.
    volume: VolumeId,
    /// The file.
    path: String,
    /// The view.
    at: ReadAt,
    /// The first byte wanted.
    offset: u64,
    /// The most bytes wanted.
    max: u64,
  },
  /// Map a file's bytes read-only into a same-kernel client's address space, with no copy per read (§4.6, A-171): the
  /// daemon fills a copy of the file sealed against every writer but itself, keeps it equal to the file, and hands its
  /// descriptor over the control socket before the reply; the client `mmap`s it whole. Offered only to a same-uid peer
  /// the platform can pass a descriptor to; any other peer reads through `ReadRange` instead. The reply waits for the
  /// copy's fill, which runs in slices on the owner shard. Appended.
  MapRead {
    /// The volume.
    volume: VolumeId,
    /// The file.
    path: String,
    /// The view.
    at: ReadAt,
  },
  /// Build a snapshot's disk image (§4.6, A-160): a new volume `name` holding one file, [`IMAGE_FILE`], an ext2 file
  /// system of `capacity` bytes holding the snapshot's tree, for a VMM that shares no folder with its guest
  /// (Firecracker) to attach as a block device. The image volume pins the snapshot, as a clone does, until it is
  /// destroyed. The reply waits for the build, which runs in slices on the volume's owner shard. Appended.
  ImageBuild {
    /// The volume.
    volume: VolumeId,
    /// The snapshot imaged.
    snapshot: SnapshotId,
    /// The image volume's name.
    name: String,
    /// The image's size in bytes: the file system's, with the guest's room to write.
    capacity: u64,
  },
  /// Merge an image volume's image into a new clone `name` of the snapshot it was built from (§4.6, A-160): the
  /// clone holds the tree the image holds now, and each node the build wrote keeps its inode. The reply waits for
  /// the merge, which runs in slices on the image volume's owner shard. Appended.
  ImageMerge {
    /// The image volume.
    image: VolumeId,
    /// The clone's name.
    name: String,
  },
  /// Let go of a direct map this client holds (§4.6, A-171): the daemon stops keeping the copy equal to its file and
  /// gives its bytes back to the fill budget. The client unmaps its own view first. `NotFound` for a map this client
  /// does not hold. Appended.
  ReleaseMap {
    /// The volume the map was of (routes the verb to its owner shard, where the copy is held).
    volume: VolumeId,
    /// The mapping id the `Mapped` reply named.
    mapping_id: u64,
  },
  /// Tells the daemon that the process `pid` holds the Shards stream of `attachment` as the handle `handle` there
  /// (§4.6, A-181; Windows): what `slates shards` sends once the command it started holds the end, before it closes its
  /// own copy, so the command is a known holder that a VMM's DAX claim can be judged against (A-180). The daemon checks
  /// the claim first, by object against a holder it knows (`slates_ipc::handle_route::claim`), so the request grants
  /// nothing a true claim did not already have. Refused `NotFound` for no such stream of `volume`, `Forbidden` where the
  /// handle is not the stream's, `HolderPending` while the attach's own holder record is still on its way to the
  /// stream's shard, `Unsupported` on Unix, where the end is a descriptor the daemon never needs to find again.
  /// Appended.
  ShardsHolder {
    /// The volume the stream serves (routes the verb to its owner shard, where the stream is served).
    volume: VolumeId,
    /// The attachment the stream serves.
    attachment: u64,
    /// The process that holds the stream.
    pid: u32,
    /// The value of its handle of the stream, in that process.
    handle: u64,
  },
  /// Map a file's bytes read and write into a same-kernel client's address space (§4.6, A-186), under the caller's own
  /// write attachment of the volume, as a file verb rides one: the daemon fills a copy of the file that the client may
  /// change the bytes of and nothing else, keeps it equal to the file as `MapRead`'s copy is, and hands it as `MapRead`
  /// does (the reply is `Mapped`, `MapOffered` or `MapHanded`). The client's stores land in the copy and reach the file
  /// at its `MapSync`, its `ReleaseMap`, or its end, each written back through the attachment. Refused
  /// `MapUnavailable` for every shape `MapRead` refuses, with no paged fallback, since a copied map could not keep
  /// another writer's bytes; `Forbidden` for an attachment the caller may not write through; `BadRequest` for a
  /// granule that is not a power of two of at least the page. Appended.
  MapWrite {
    /// The volume (routes the verb to its owner shard).
    volume: VolumeId,
    /// The caller's write attachment of the volume, which the map's stores are written back under.
    attachment: u64,
    /// The file.
    path: String,
    /// What the copy's length is rounded up to, from the file's: a power of two of at least the page. The page for a
    /// map a process reads and writes itself; 2 MiB for a pmem VMM's backing, which Firecracker pads with pages it
    /// never syncs and Cloud Hypervisor refuses unless whole (`slates pmem`). Stores past the file's end within the
    /// copy are not the file's and are never written.
    granule: u64,
  },
  /// Write the stores a writable direct map holds into its file (§4.6, A-186): the daemon compares the copy with the
  /// file up to the file's end and writes each run that differs through the map's attachment, then publishes before it
  /// replies, as a file verb does. Stores past the file's end within the copy's last page are not the file's and are
  /// not written. The reply waits for the comparison, which runs in slices on the owner shard. `NotFound` for a map
  /// this client does not hold or that is read-only, or whose attachment or file is gone. Appended.
  MapSync {
    /// The volume the map is of (routes the verb to its owner shard, where the copy is held).
    volume: VolumeId,
    /// The mapping id the map's hand-off named.
    mapping_id: u64,
  },
  /// Check a green's head out into the volume it was created from (§4.16, A-201): `base` names that volume and the
  /// snapshot the green's origin was taken from, refused unless the snapshot's origin is the green's own and the volume
  /// is unchanged since it. The head's difference from the origin is written through `attachment`, the caller's write
  /// attachment of the volume, as the file verbs write. The volume then lands under a grant like any other, so the
  /// landing's verdict judges the merge's files against the disk. The reply waits for the checkout, which runs in
  /// slices on the volume's owner shard. Appended.
  Checkout {
    /// The green.
    green: VolumeId,
    /// The volume and snapshot the green was created from.
    base: GreenBase,
    /// The caller's write attachment of `base.volume`.
    attachment: u64,
  },
  /// Turn a volume's live shipping on or off (§4.10, A-200): while on, each publication of the volume is shipped to its
  /// candidates and a barrier answers only once the owner and `f` of them hold it. `admin` on the volume is required.
  /// Appended.
  LiveShip {
    /// The volume.
    volume: VolumeId,
    /// Whether it is live-shipped from now on.
    live_shipped: bool,
  },
  /// Detach an attachment of `volume` (§4.10, A-222): `Detach` routed by the volume, so an attachment another host's
  /// owner holds is ended there (an attachment id names its owner partition, not its host). Appended.
  DetachOf {
    /// The volume the attachment is of.
    volume: VolumeId,
    /// The attachment.
    attachment: u64,
  },
  /// End every attachment the sending host's dead `client` holds on `volume` (§4.10, A-222): sent by a node's reap of
  /// a dead client to the owner of each volume that client attached from it. The owner trusts only the authenticated
  /// peer as the host, so a node ends only its own clients' attachments. Never sent by a client. Appended.
  RemoteReap {
    /// The volume the dead client attached.
    volume: VolumeId,
    /// The dead client's id on the sending host.
    client: u32,
  },
  /// Edit a work volume and submit it, in one request (§4.16, A-225): exactly `Edit` (or `EditStaged`'s edit) and then,
  /// when the edit is accepted, `Submit`, in one durable step under one completion record. Answered with the edit's
  /// refusal, or with the submit's reply. Appended.
  EditSubmit {
    /// The work volume.
    work: VolumeId,
    /// The file.
    path: String,
    /// The offset.
    at: u64,
    /// Bytes removed at `at`.
    delete_len: u64,
    /// The bytes inserted at `at`.
    insert: EditBytes,
    /// The evidence references (opaque to slates), possibly none.
    evidence: Vec<[u8; 32]>,
  },
  /// A presented landing's entries, a page at a time (§4.15 step 2: the confirmation surface shows "the full entry list
  /// on demand"): the landing a `GrantRequired` reply named, its entries' encodings from `from` (the start, then each
  /// page's `next`). Served where the landing was presented (its id names its owner partition), to the principal it was
  /// presented to, while it awaits its grant; one consumed, replaced or abandoned is `NotFound`. A read. Appended.
  LandingEntries {
    /// The presented landing.
    landing: u64,
    /// Where the page starts.
    from: EntriesCursor,
  },
  /// Sets a write policy (A-285): the launcher's or the harness's act, proven by the keyed hash under the issuer
  /// secret (`slates_server::landing::policy_proof`). Refused `Forbidden` on a consumer-bound channel whatever its
  /// proof, so an agent can never set, and so never raise, its own policy; `GrantIssuerUnverified` without the proof.
  /// A volume's policy is served on the volume's owner shard. Appended.
  SetPolicy {
    /// What the policy is set on.
    subject: PolicySubject,
    /// The policy.
    policy: WritePolicy,
    /// The policy version the setter read (`Policy`'s `version`): refused `PolicyChanged` once the policy has moved.
    version: u64,
    /// The keyed hash under the launcher's secret, over the subject, the policy and the version.
    proof: [u8; 32],
  },
  /// Reads a write policy (A-285): `None` when none is set, which is ask. Appended.
  Policy {
    /// What the policy is set on.
    subject: PolicySubject,
  },
  /// Presents a landing and never lands it, whatever the volume's write policy (A-285): the answer is `GrantRequired`
  /// with the plan, its entries kept for `LandingEntries`, exactly as an asked `Land` answers, and nothing is written.
  /// A reviewer reads the changes a bypass volume would land at once, and a forbidden volume's too, since reading them
  /// writes nothing. Served on the volume's owner shard, as `Land` is. Appended.
  Present {
    /// The volume.
    volume: VolumeId,
    /// The snapshot, or the head.
    snapshot: Option<SnapshotId>,
    /// The target directory's path.
    target: String,
    /// The filter.
    filter: Filter,
    /// A signature over the presented landing's statement, judged as `Land`'s is (A-321): what a held landing of an
    /// asking session carries to its approval request.
    signature: Option<Vec<u8>>,
  },
  /// A `Submit` whose caller's own policy asks (A-285): served as a submit under an ask mode whatever the volumes'
  /// modes, so the merge is answered `MergeApprovalRequired` (or refused, if a volume forbids it). What an agent's
  /// channel sends in place of `Submit` (or `SubmitSigned`) when the consumer's merge mode asks, and what a signer sends
  /// to learn the increment's identity its merge statement names (signed work, A-321): nothing is merged either way.
  /// Appended.
  SubmitHeld {
    /// The work volume.
    work: VolumeId,
    /// The evidence references (opaque to slates), possibly none.
    evidence: Vec<[u8; 32]>,
    /// The signature a `SubmitSigned` carried, kept through the hold and judged for the approval request (A-321).
    signature: Option<Vec<u8>>,
  },
  /// An `EditSubmit` whose caller's own policy asks (A-285): the edit applies, and the submit is held as
  /// `SubmitHeld`'s is. Appended.
  EditSubmitHeld {
    /// The work volume.
    work: VolumeId,
    /// The file.
    path: String,
    /// The offset.
    at: u64,
    /// Bytes removed at `at`.
    delete_len: u64,
    /// The bytes inserted at `at`.
    insert: EditBytes,
    /// The evidence references (opaque to slates), possibly none.
    evidence: Vec<[u8; 32]>,
  },
  /// Submits a work whose merge an ask policy held for approval (A-285): the launcher's or the harness's approval of
  /// exactly the increment `identity` names (the identity a `MergeApprovalRequired` reply carried), proven by the keyed
  /// hash under the issuer secret (`slates_server::landing::merge_approval_proof`). The increment is built again and
  /// merged only if it is still that one; one that changed since is refused `GrantMismatch`. Refused `Forbidden` on a
  /// consumer-bound channel and `GrantIssuerUnverified` without the proof. Appended.
  SubmitApproved {
    /// The work volume.
    work: VolumeId,
    /// The evidence references (opaque to slates), possibly none.
    evidence: Vec<[u8; 32]>,
    /// The approved increment's identity.
    identity: [u8; 32],
    /// The keyed hash under the issuer secret.
    proof: [u8; 32],
    /// The signature over the merge's statement, when the merge is signed work (A-321): the approval carries it again,
    /// since the held submit kept nothing. Its reply is then `SubmittedSigned`.
    signature: Option<Vec<u8>>,
  },
  /// A `Submit` carrying a signature over its merge's statement (signed work, A-321; `slates_server::signed_work::
  /// merge_statement`, signed under the namespace `slates-work`): judged under the work's and the green's trusted
  /// signers, recorded with the version as provenance and answered `SubmittedSigned`. The signature never permits
  /// anything: the merge follows its write modes as a `Submit` does, and a volume that requires signed merges refuses
  /// one whose signature does not verify when it would merge (`SignatureRequired`). Appended.
  SubmitSigned {
    /// The work volume.
    work: VolumeId,
    /// The evidence references (opaque to slates), possibly none.
    evidence: Vec<[u8; 32]>,
    /// The SSH signature blob (the armour's base64, decoded).
    signature: Vec<u8>,
  },
  /// Sets a volume's trusted signers (signed work, A-321): the public keys, in SSH wire form, whose signatures over the
  /// volume's work verify; the whole set, so a key is removed by setting the set without it. The launcher's act, proven
  /// as a write policy is (`slates_server::landing::signers_proof`, over the volume, the keys and the policy version it
  /// read) and refused alike on a consumer-bound channel; a key slates does not verify is refused `BadRequest`. The set
  /// moves the volume's policy version. Served on the volume's owner shard. Appended.
  SetSigners {
    /// The volume.
    volume: VolumeId,
    /// The public keys, in SSH wire form.
    keys: Vec<Vec<u8>>,
    /// The policy version the setter read (`Policy`'s `version`): refused `PolicyChanged` once the policy has moved.
    version: u64,
    /// The keyed hash under the launcher's secret.
    proof: [u8; 32],
  },
  /// A green version's merge record as a consumer reads it back (§4.16, A-321): how the signature of the merge that made
  /// the version was judged, its provenance, kept with the green's accepted record (the version's chain entry, then the
  /// checkpoint that folds it). Served on the green's owner shard; version 0 is the origin, which no merge made; a
  /// version not yet committed is `NotFound`. A read. Appended.
  MergeRecord {
    /// The green.
    green: VolumeId,
    /// The version.
    version: u64,
  },
  /// One page of an overlay volume's drift (§4.4 `status`, A-267): the entries a status lists — every witnessed entry
  /// whose disk no longer matches its witness, and every one whose re-check the host refuses now — in inode order past
  /// `after`, re-checked now as a status re-checks them, as many as one reply chunk holds and its slice of the shard's
  /// step allows. A walk of the pages re-checks each witnessed entry once, and an entry drifted for the whole walk is
  /// answered once. A status carries the first that fit; this reads them all. A read; never recorded. Appended.
  Drifted {
    /// The overlay volume.
    volume: VolumeId,
    /// The inode the previous page ended at, or `None` for the first page.
    after: Option<u64>,
  },
  /// One page of a work's last conflicting verdict (§4.16, A-267): the windows of the submit's or the rebase's verdict
  /// `verdict` named, from position `from`, as many as one reply chunk holds. The work keeps the verdict until its next
  /// submit or rebase; a verdict no longer kept is refused `VerdictReplaced`. Served on the work's owner. A read; never
  /// recorded. Appended.
  Conflicts {
    /// The work volume.
    work: VolumeId,
    /// The verdict, as its `Submitted` or `Rebased` named it.
    verdict: u64,
    /// The position to read from: the windows the verdict's reply carried, then each page's `next`.
    from: u32,
  },
  /// Renews a CLI attachment's term from whatever holds it (A-230, A-384): what `slates attach --hold` and the client
  /// library's and the SDKs' attachment objects send while they live, so an attachment its holder keeps never lapses
  /// for want of another verb naming it. Does nothing else. Served on the attachment's owner shard and answered
  /// `Renewed` with the term now in force and when to renew again; refused as any use of the attachment is
  /// (`AttachmentExpired` once its term lapsed), and `BadRequest` for an attachment with no term (a mount's renews
  /// through its own activity). Appended.
  Renew {
    /// The attachment.
    attachment: u64,
  },
  /// Move `volume` to the fleet member `to` (§4.10 "Ownership follows the writer": "operators keep an explicit move";
  /// A-291): its owner freezes it, places its records and reports the move, and the member `to` adopts it, serves it
  /// and seals it under its own placement before the owner lets its copy go. Answered `Moving` until the configuration
  /// names `to`, then `Moved`; the caller asks again until `Moved`. A volume that cannot move is refused
  /// [`Refusal::MoveRefused`]. `admin` on the volume is required. Appended.
  Move {
    /// The volume.
    volume: VolumeId,
    /// The member it moves to, by its host id.
    to: u64,
  },
  /// Capture an overlay's whole base into a complete immutable snapshot (§4.15 `capture_base`, A-291): every file's
  /// bytes and every link's target become the volume's own, then a snapshot is taken and recorded captured, so a clone
  /// of it needs no base directory and moves between fleet members as a plain volume does. The caller asserts the source
  /// is quiesced (nothing writes it while the capture runs); one that changed under it is refused
  /// [`Refusal::ConsistentBaseUnavailable`]. Answered `BaseCaptured` once the walk ends. `write` on the volume is
  /// required. Appended.
  CaptureBase {
    /// The overlay.
    volume: VolumeId,
  },
  /// Export a snapshot as an archive (§4.11 "Archive", D-17; Mantle's CP-T10.4, A-441): a new volume `name` holding one
  /// file, [`ARCHIVE_FILE`], the snapshot's content-addressed archive, which a caller reads a page at a time
  /// (`ReadRange`) and keeps wherever it keeps bytes. The reply waits for the export, which walks the snapshot in
  /// slices on the volume's owner shard, the snapshot pinned until its archive is written. `read` on the volume is
  /// required. Appended.
  ArchiveExport {
    /// The volume.
    volume: VolumeId,
    /// The snapshot exported.
    snapshot: SnapshotId,
    /// The archive volume's name.
    name: String,
  },
  /// Make a volume from an archive (§4.11 "Archive", D-17; Mantle's CP-T10.4, A-453): a new volume `name` of `size`
  /// holding the tree of the archive the caller's volume `archive` holds as [`ARCHIVE_FILE`] in its root, as an export
  /// leaves it (a caller writes the bytes it kept there). The archive file's length is admitted against `archive`'s
  /// own size class and the shard's room before a byte is read; the file is read, decoded, verified and restored in
  /// slices on `archive`'s owner shard, and the new volume made there with the archive's name policy. The reply waits
  /// for the import. A stream that is no archive, or one not whole, is refused [`Refusal::ArchiveRefused`] by its
  /// kind, with nothing made. `read` on `archive` is required. Appended.
  ArchiveImport {
    /// The volume whose root holds the archive file.
    archive: VolumeId,
    /// The new volume's name.
    name: String,
    /// The new volume's size class.
    size: SizeClass,
    /// Whether unlocked memory refuses, as a create's.
    require_locked: bool,
  },
  /// Create repository `name` (§4.17.1 "The repository and its greens"; A-575) on the partition its name hashes to: its
  /// record and its lineage's root green, bound to `default_branch`. The root is `green`, an unbound green of that
  /// partition that joins as it stands, or a new empty green. Answered `GreenCreated` with the root. Appended.
  CreateRepository {
    /// The repository's name, unique everywhere.
    name: String,
    /// The tool it serves.
    tool: VcsTool,
    /// How it names its objects.
    format: ObjectFormat,
    /// The branch its root is bound to, and the one a client gets when it names none.
    default_branch: String,
    /// The green that becomes its root, when not a new empty one.
    green: Option<VolumeId>,
  },
  /// Make branch `name` of repository `repository` at `version` of branch `from` (its head when `None`), sharing every
  /// node of `from`'s state, so what it costs is what changed after `version` (§4.17.1 item 4, A-575). Answered
  /// `GreenCreated` with the new green. Appended.
  CreateBranch {
    /// The repository.
    repository: String,
    /// The new branch's name.
    name: String,
    /// The branch it is made from.
    from: String,
    /// The version of `from` it is made at; its head when `None`.
    version: Option<u64>,
  },
  /// Bind branch `name` of repository `repository` to `green`, a member of the repository's lineage no name binds:
  /// what restores a retired branch (§4.17.1 item 5; A-575). Answered `Acknowledged`. Appended.
  BindBranch {
    /// The repository.
    repository: String,
    /// The branch's name.
    name: String,
    /// The green.
    green: VolumeId,
  },
  /// Retire branch `name` of repository `repository`: the name leaves the namespace and its green stays a member of
  /// the lineage, so binding the name to it again restores the branch (§4.17.1 item 5; A-575). Answered
  /// `Acknowledged`. Appended.
  RetireBranch {
    /// The repository.
    repository: String,
    /// The branch's name.
    name: String,
  },
  /// Set repository `repository`'s policy whole (§4.17.1 "Policy"; A-622): its rules by name pattern and its hooks,
  /// replacing what it held, made from `read`, the policy version its setter read (0 for a repository never given one).
  /// Its owner's verb. Answered `Acknowledged`, the policy then at version `read + 1`; refused `PolicyChanged` with the
  /// version now when the policy has moved since `read`, and `Database` naming the first rule or hook its repository
  /// cannot hold. Appended.
  SetRepositoryPolicy {
    /// The repository.
    repository: String,
    /// The policy version the setter read.
    read: u64,
    /// The rules, in order: git's precedence and Subversion's at one depth read the order.
    rules: Vec<PolicyRule>,
    /// The hooks, in order: one point's hooks run in that order.
    hooks: Vec<HookConfig>,
    /// A Subversion repository's hooks-env, its sections in order (A-648): each hook's environment is its own section,
    /// else the `default` one, else nothing. Empty for git and Mercurial. Appended.
    hooks_env: Vec<HooksEnvSection>,
  },
  /// Enroll a version-control credential for `principal` (§4.13: the human surface, proving issuer authority as an
  /// enrollment does; §4.17.2 "Access"; A-625 part 3): an SSH user key by its blob, or an HTTP token by its SHA-256, so
  /// the token itself never crosses a channel. It proves `principal` to the HTTP and SSH edges until `expires_ns` on the
  /// wall clock (nanoseconds since the Unix epoch) when one is given. Served on the partition the credential's id routes
  /// to. Answered `CredentialEnrolled` with the id; refused `Forbidden` on a consumer's channel, `GrantIssuerUnverified`
  /// when the proof does not verify, `AlreadyExists` for an id held, `BadRequest` for a key blob that names no
  /// algorithm, and `TableAtCapacity` past the partition's table bytes. Appended.
  EnrollCredential {
    /// The principal it proves.
    principal: Principal,
    /// The credential.
    material: CredentialMaterial,
    /// When it stops proving `principal` (wall clock, nanoseconds since the Unix epoch); none for never.
    expires_ns: Option<u64>,
    /// The proof of issuer authority: `BLAKE3_keyed(issuer_secret, "slates.credential" ‖ the three fields' wire
    /// encodings)`.
    proof: [u8; 32],
  },
  /// Revoke the version-control credential named `id` (§4.13; A-625 part 3): the human surface, proving issuer
  /// authority. Served on the partition the id routes to. Answered `CredentialRevoked`; refused `Forbidden` on a
  /// consumer's channel, `GrantIssuerUnverified` when the proof does not verify, and `NotFound` for an id not held.
  /// Appended.
  RevokeCredential {
    /// The credential's id: the SHA-256 of a key's blob or of a token.
    id: [u8; 32],
    /// The proof of issuer authority: `BLAKE3_keyed(issuer_secret, "slates.credential-revoke" ‖ id)`.
    proof: [u8; 32],
  },
}

/// What a policy rule's pattern names (§4.17.1 "Policy"; A-622), as `SetRepositoryPolicy` gives it: the names its
/// repository's tool has.
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

/// A right a policy rule decides (§4.17.1 "Policy"; A-622).
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

/// Whether a policy rule allows its right or denies it (A-622).
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyEffect {
  /// It allows the right.
  Allow,
  /// It denies the right.
  Deny,
}

/// One entry of the principals a policy rule speaks to (A-622).
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

/// The principals a policy rule speaks to (A-622).
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

/// One policy rule (§4.17.1 "Policy"; A-622): on the names `pattern` covers, `effect` for `right` to the principals
/// `who` names.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct PolicyRule {
  /// What the pattern names.
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

/// A hook point (§4.17.1; A-597's points), each its tool's (A-622).
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
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

/// One configured hook (§4.17.1: "a program the repository's owner configured"; A-622).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct HookConfig {
  /// The point it runs at.
  pub point: HookPointName,
  /// Its name: at a Mercurial point, the point's name alone or with a `.suffix`, one hook a name; git and Subversion
  /// keep one hook a point, named for its point.
  pub name: String,
  /// The program it runs: an absolute path on the host that runs the repository's hooks.
  pub program: String,
}

/// A variable of a Subversion repository's hooks-env (A-648), as `SetRepositoryPolicy` gives it: one `NAME = value`
/// line of a section of the file `svnserve.conf`'s `hooks-env` names.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct HooksEnvVariable {
  /// Its name: not empty, holding no `=` and no NUL.
  pub name: String,
  /// Its value, holding no NUL.
  pub value: Vec<u8>,
}

/// A section of a Subversion repository's hooks-env (A-648), as `SetRepositoryPolicy` gives it: the variables the hook
/// named as the section runs with, or, in the section named `default`, those of every hook with no section of its own.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct HooksEnvSection {
  /// Its name, a hook's or `default`.
  pub name: String,
  /// Its variables, in order.
  pub variables: Vec<HooksEnvVariable>,
}

/// The tool a repository serves (§4.17; A-575), as `CreateRepository` names it.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum VcsTool {
  /// git.
  Git,
  /// Mercurial.
  Mercurial,
  /// Subversion.
  Subversion,
}

/// How a repository names its objects (§4.17.1 item 1; A-575), as `CreateRepository` names it: fixed at creation.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectFormat {
  /// SHA-1 names: git's default and Mercurial's node ids.
  Sha1,
  /// SHA-256 names: git's `object-format=sha256`.
  Sha256,
  /// Revision numbers: Subversion's.
  Revisions,
}

/// The bytes an [`RequestBody::EditSubmit`] inserts (A-225): carried in the request, or, for an edit larger than one
/// request, staged on the work's owner first (`StageBegin`, `StagePut`) and named by its buffer, as `EditStaged` does.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum EditBytes {
  /// The bytes themselves.
  Inline {
    /// The bytes.
    bytes: Vec<u8>,
  },
  /// A staged buffer on the work's owner.
  Staged {
    /// The buffer's token.
    token: u64,
  },
}

/// Format: the name of the image file in an image volume's root (§4.6, A-160): what a VMM is pointed at.
pub const IMAGE_FILE: &str = "image.ext2";

/// Format: the name of the archive file in an archive volume's root (`ArchiveExport`, A-441): the snapshot's archive in
/// the format of `slates_archive` (§4.11), its own magic and version inside.
pub const ARCHIVE_FILE: &str = "snapshot.slatesarchive";

/// What an image merge did (§4.6, A-160), counted.
#[derive(Wire, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImageMergeReport {
  /// Image nodes that are the build's nodes, the root among them.
  pub kept: u64,
  /// Nodes made that the snapshot never had.
  pub created: u64,
  /// Names given to a build's node by renaming one of its old names.
  pub moved: u64,
  /// Names given as links.
  pub linked: u64,
  /// The snapshot's nodes the image no longer holds, removed.
  pub removed: u64,
  /// Old names removed.
  pub names_removed: u64,
  /// Entries moved aside to free a name the image gives to another node.
  pub staged: u64,
  /// File bytes compared with the image.
  pub bytes_compared: u64,
  /// File bytes written: the chunks that differed.
  pub bytes_written: u64,
  /// Nodes whose permissions, owner or times were set.
  pub attributes_set: u64,
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
  /// The median time this shard took to serve an NFS call whose volume it owns, since boot (§4.6, §4.14).
  NfsLocalP50Ns,
  /// The 99th percentile of the same.
  NfsLocalP99Ns,
  /// The 99th percentile of the part of a local serve the operating system held the shard's thread off a core: what
  /// a busy machine adds to the serve itself.
  NfsLocalOffCpuP99Ns,
  /// The 99th percentile of a call this shard read but forwarded to its volume's owner shard and back.
  NfsForwardedP99Ns,
  /// The window the daemon's watchdog judges this shard by (A-455, §4.14 `daemon.alive`): the liveness window, raised to
  /// the scheduler overrun the shard measured where that is longer, never past the observation budget. A shard with no
  /// step for this long is stuck unless it waited for a CPU or for memory, and one that ran this long of CPU held to
  /// the build without a step is spinning.
  WatchWindowNs,
}

impl HealthSignal {
  /// The closed registry: every shard health signal, in the order the report emits them. A doc-truth
  /// test pins this set and its names so a rename or an addition is caught, not silently miscounted.
  pub const ALL: [HealthSignal; 11] = [
    HealthSignal::CatalogVolumes,
    HealthSignal::LogReplayNs,
    HealthSignal::LeaseExpiring,
    HealthSignal::RingDepth,
    HealthSignal::ShardClients,
    HealthSignal::ShardDeferred,
    HealthSignal::NfsLocalP50Ns,
    HealthSignal::NfsLocalP99Ns,
    HealthSignal::NfsLocalOffCpuP99Ns,
    HealthSignal::NfsForwardedP99Ns,
    HealthSignal::WatchWindowNs,
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
      HealthSignal::NfsLocalP50Ns => "nfs.local_p50_ns",
      HealthSignal::NfsLocalP99Ns => "nfs.local_p99_ns",
      HealthSignal::NfsLocalOffCpuP99Ns => "nfs.local_off_cpu_p99_ns",
      HealthSignal::NfsForwardedP99Ns => "nfs.forwarded_p99_ns",
      HealthSignal::WatchWindowNs => "shard.watch_window_ns",
    }
  }

  /// What this signal's absence means (§4.14, A-9). `log.replay_ns` is `Unknown` until a replay
  /// happens (a fresh daemon that replayed nothing has no replay time — distinct from a 0 ns replay).
  /// The rest are always-computable counts on a live shard, so an absent one is a `Degraded` producer,
  /// never a healthy zero.
  pub const fn absence(self) -> AbsenceIs {
    match self {
      HealthSignal::LogReplayNs
      | HealthSignal::NfsLocalP50Ns
      | HealthSignal::NfsLocalP99Ns
      | HealthSignal::NfsLocalOffCpuP99Ns
      | HealthSignal::NfsForwardedP99Ns => AbsenceIs::Unknown,
      HealthSignal::CatalogVolumes
      | HealthSignal::LeaseExpiring
      | HealthSignal::RingDepth
      | HealthSignal::ShardClients
      | HealthSignal::ShardDeferred
      | HealthSignal::WatchWindowNs => AbsenceIs::Degraded,
    }
  }

  /// What this signal's `freshness_ns` is measured from (§4.14): the report ages each signal by the
  /// registry's rule, not one of its own.
  pub const fn freshness(self) -> FreshnessBasis {
    match self {
      HealthSignal::LogReplayNs
      | HealthSignal::NfsLocalP50Ns
      | HealthSignal::NfsLocalP99Ns
      | HealthSignal::NfsLocalOffCpuP99Ns
      | HealthSignal::NfsForwardedP99Ns => FreshnessBasis::SinceBoot,
      HealthSignal::CatalogVolumes
      | HealthSignal::LeaseExpiring
      | HealthSignal::RingDepth
      | HealthSignal::ShardClients
      | HealthSignal::ShardDeferred
      | HealthSignal::WatchWindowNs => FreshnessBasis::AtReport,
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
      HealthSignal::NfsLocalP50Ns | HealthSignal::NfsLocalP99Ns => {
        "the shard's NFS service times for calls served here"
      }
      HealthSignal::NfsLocalOffCpuP99Ns => {
        "the shard's NFS service times less its thread's CPU time"
      }
      HealthSignal::NfsForwardedP99Ns => {
        "the shard's NFS service times for calls forwarded to an owner"
      }
      HealthSignal::WatchWindowNs => {
        "the shard's measured scheduler overrun, by the watchdog's window rule"
      }
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
      | HealthSignal::ShardDeferred
      | HealthSignal::NfsLocalP50Ns
      | HealthSignal::NfsLocalP99Ns
      | HealthSignal::NfsLocalOffCpuP99Ns
      | HealthSignal::NfsForwardedP99Ns
      | HealthSignal::WatchWindowNs => Observer::OwnerShard,
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
/// would default to — and its election state: term, priority, rank, lease and the campaigns it began, so an
/// observer can tell why a group elected whom it did (`docs/wip/kind-lane.md`). The regional council and
/// the root group each report one.
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
  /// This node's current term in the group.
  pub term: u64,
  /// This node's election priority as it last measured it (§3.4): its quorum round trip, nanoseconds; zero
  /// while a needed path is unmeasured.
  pub priority_ns: u64,
  /// The spread of the path that sets the priority, nanoseconds.
  pub priority_spread_ns: u64,
  /// How many voters it holds alive outrank it: the timeouts its timer yields before it campaigns (§3.4).
  pub rank: u32,
  /// Whether it holds a leader's lease — it believes a leader alive, and so refuses pre-votes (thesis
  /// §4.2.3).
  pub leader_lease: bool,
  /// The pre-elections (pre-vote rounds) it has begun since it started.
  pub pre_elections: u64,
  /// The elections it has begun since it started: after a won pre-election, or on a leader's invitation.
  pub elections: u64,
  /// Replies to its own pre-elections that granted.
  pub pre_votes_granted: u64,
  /// Replies to its own pre-elections that refused.
  pub pre_votes_refused: u64,
  /// Pre-votes it refused as a voter (thesis §9.6) because it does not vote or it leads.
  pub refused_role: u64,
  /// Pre-votes it refused because it held a leader's lease.
  pub refused_leased: u64,
  /// Pre-votes it refused because their term was not ahead of its own.
  pub refused_term: u64,
  /// Pre-votes it refused because the candidate's log was behind its own.
  pub refused_log: u64,
  /// The voters seated in the group's configuration in force: its fault tolerance is what this many voters
  /// give, whatever the membership (thesis §4.3). A member being caught up and promoted is not seated until
  /// its joint change commits.
  pub voters: u64,
  /// Whether a joint change of the voter set is in flight (Raft §6).
  pub joint: bool,
  /// The member ids of the voters in force (both sets while a joint change is in flight), ascending: which members a
  /// loss costs the group a vote. Appended (2026-11-07).
  pub voter_ids: Vec<u64>,
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
  /// The bytes of the shard's content arena locked in RAM (§4.2 D-12, BUG-1): the whole arena once a
  /// strict (`--locked`) volume lives on the shard, so none of its content can be paged to swap; zero
  /// otherwise. Each region is locked as it joins, or growth stops.
  pub locked_bytes: u64,
  /// Whether this is the control shard — the one that runs the membership loop, drives the consensus
  /// groups and holds their live timing; the other shards hold inert copies.
  pub control: bool,
  /// The regional configuration council as this shard holds it (live on the control shard).
  pub council: GroupReport,
  /// The root group across regions as this shard holds it (live on the control shard).
  pub root: GroupReport,
  /// The objects this shard holds a record copy of as a candidate holder (§4.8 "records are sent to all
  /// candidates"); live on the control shard, zero on the others.
  pub held_records: u64,
  /// The objects whose takeover this shard owes and has not finished (§4.8 "Promotion": a retired owner's
  /// objects the council reassigned here); live on the control shard, zero on the others.
  pub takeovers_pending: u64,
  /// The version of the regional configuration this shard has installed.
  pub configuration_version: u64,
  /// The takeover state as this shard's council holds it (live on the control shard).
  pub takeover: TakeoverReport,
  /// This shard's failure detector's report of each peer (live on the control shard, empty elsewhere).
  pub detector: Vec<DetectorPeerReport>,
  /// Task admissions this shard's runtime arena refused since boot (§4.3 "a task exceeding the budget
  /// is a counted bug signal"; §4.14): the shard's derived task budget covers every task the daemon
  /// spawns on it — clients' cross-shard work, its own loops, the fleet's share — so a count here is a
  /// sizing defect surfacing, never expected load. A refused client admission is what left a fleet
  /// node unable to seat any client on 2026-09-14.
  pub tasks_refused: u64,
  /// Landings presented here and awaiting a grant (§4.15 step 3; AUD-29-07): each is consumed when its
  /// granted landing finishes, replaced when its client presents the same volume and target again, and
  /// dropped when its client is retired, so it returns to zero; `landings_awaiting_bound` bounds it.
  pub landings_awaiting: u64,
  /// The bound on `landings_awaiting`: the daemon's client seats (one pending presentation per seat).
  pub landings_awaiting_bound: u64,
  /// Granted landings running here as owned tasks (§4.15 "Ownership facts"; AUD-29-03): each ends — landed,
  /// refused or failed — with its reply, so it returns to zero; the shard's task arena bounds it.
  pub landings_in_flight: u64,
  /// Target landing leases recorded on this shard (§4.15 step 4; AUD-29-03): live on the control shard, the
  /// one owner of the host's target leases, zero on the others. A landing releases its lease before its
  /// reply, and a take releases every lease whose term has ended, so it returns to zero.
  pub target_leases: u64,
  /// The part of `committed_bytes` held for other owners as their content's candidate holder (§4.2; AUD-29-43):
  /// replicated chunks and manifests at their arena block length, charged from unpromised capacity only.
  /// Appended.
  pub replicated_bytes: u64,
  /// The node's ML-KEM-1024 recipient id (A-92 piece 4a): live on the control shard, whose partition holds it; empty
  /// on the others and while sealing is unavailable. Appended.
  pub seal_recipient_id: Vec<u8>,
  /// This shard's record session to each peer, as last seen (live on the control shard, empty elsewhere). Appended.
  pub sessions: Vec<SessionReport>,
  /// NFS calls this shard served, by NFSv3 procedure number (0 `NULL` to 21 `COMMIT`, RFC 1813 §3; an NFSv4
  /// compound counts each operation's v3 call): the round trips each client operation cost. Appended.
  pub nfs_calls: Vec<u64>,
  /// This shard's detector granularity (its measured wake lateness), nanoseconds: live on the control shard, zero
  /// elsewhere. Appended.
  pub detector_granularity_ns: u64,
  /// The sessions peers dialed to this shard, as its serve loops last saw them (live on the control shard). Appended.
  pub served_sessions: Vec<SessionReport>,
  /// This shard's detector's pooled verdict: live on the control shard, empty elsewhere. Appended.
  pub detector_pool: DetectorPoolReport,
  /// This shard's detector's relay-stage timer: live on the control shard, empty elsewhere. Appended.
  pub detector_relays: DetectorRelayReport,
  /// The attachments this shard's registry holds: admitted and not yet drained, by every transport. One that outlives
  /// its transport is a slot no later attach can take. Appended.
  pub registry_attachments: u64,
  /// Live shipping on this shard (A-200): each live volume it ships, and on the control shard each it holds as a
  /// candidate. Appended.
  pub live: Vec<LiveReport>,
  /// This shard's partition snapshots against the log they absorb, and the cadence they follow (A-243). Appended.
  pub snapshots: DbSnapshotReport,
  /// The members this node keeps direct contact with (§4.8: its holders, the consensus voters and staged members, and
  /// every member while it is a voter itself), this node left out: the peers its probe mesh and record links reach.
  /// Counted on the control shard only; zero elsewhere and on a laptop. Appended.
  pub peers_in_contact: u32,
  /// Of `peers_in_contact`, those with no formed probe session yet: zero once this node's mesh is up
  /// (`Daemon::fleet_meshed`). Appended.
  pub peers_unmeshed: u32,
  /// This node's council configuration fetches as a learner (control shard only; empty elsewhere). Appended.
  pub learner: LearnerReport,
  /// This node's failover estimate (A-363): live on the control shard, which runs the coordinator; elsewhere the
  /// recovery prior alone. Appended.
  pub failover: FailoverReport,
}

/// A learner's configuration fetches (§4.8, D-14): the council version it holds now, the version it held after its last
/// fetch, how many members that fetch asked, and how long ago it asked. What tells a learner that stopped learning
/// from one that learns (a node held at version 8 while its council stood at 12 or 13, CI diag-raft-chaos-60).
#[derive(Wire, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LearnerReport {
  /// The council configuration version this node holds.
  pub council_version: u64,
  /// The version it held once its last fetch's replies were adopted; zero before any fetch.
  pub fetched_version: u64,
  /// The members its last fetch asked; zero before any fetch.
  pub asked: u32,
  /// How long ago, in nanoseconds of the daemon's clock, its last fetch asked; `None` before any.
  pub last_asked_ago_ns: Option<u64>,
}

/// A partition's snapshots and their cadence, as `slates status` shows them (A-243).
#[derive(Wire, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DbSnapshotReport {
  /// Snapshots taken since the daemon recovered.
  pub snapshots: u64,
  /// The bytes they wrote.
  pub bytes_written: u64,
  /// The longest one took, encode and publish, nanoseconds (it runs on this shard's turn).
  pub longest_ns: u64,
  /// The log bytes appended since the daemon recovered.
  pub log_bytes_appended: u64,
  /// The log since the last snapshot, bytes.
  pub tail_bytes: u64,
  /// The replay work its commits measured (each record's encode, decode, checksum and copy, and its apply; A-420),
  /// nanoseconds: the next snapshot comes once it reaches `budget_ns` and `tail_bytes` reaches `floor_bytes`.
  pub tail_replay_ns: u64,
  /// The recovery budget the tail's work is held to, nanoseconds.
  pub budget_ns: u64,
  /// The last snapshot's size, bytes: the doubling rule's floor for the tail.
  pub floor_bytes: u64,
}

/// One live-shipped volume's log as a shard sees it (A-200): as its owner (`holding` false: the head it appended and
/// the position the owner and `f` candidates hold) or as a candidate (`holding` true: the head it holds; `durable` is
/// then unknown, `None`).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct LiveReport {
  /// The volume.
  pub volume: VolumeId,
  /// Whether this is a candidate's copy, not the owner's log.
  pub holding: bool,
  /// The head position (`None`: nothing yet).
  pub head: Option<u64>,
  /// The durable position, on the owner.
  pub durable: Option<u64>,
  /// On the owner: publications are owed to the log (a burst outran the credit, or one could not be shipped), so its
  /// head is behind the volume until the whole record it owes is appended.
  pub owed: bool,
  /// The head's epoch: the owner's epoch that made the entry at the head (`None`: nothing yet). Appended.
  pub head_epoch: Option<u64>,
  /// The head's chain identity: the BLAKE3 of the entry at the head, which binds every entry before it (`None`: nothing
  /// yet). Two copies at one epoch and position hold the same log exactly when these agree (Agreement). Appended.
  pub head_identity: Option<[u8; 32]>,
  /// On a candidate, the fence its copy has promised: no message made under a lower epoch is served. On the owner, the
  /// epoch it ships under. Appended.
  pub fence: u64,
  /// On the owner, the member ids of the holders its durability counts (during a transition, the set it is moving
  /// from), ascending; empty on a candidate. Appended (2026-11-07).
  pub holders: Vec<u64>,
  /// On the owner, the member ids of the set a holder transition is moving to, ascending; empty when none is in
  /// flight, and on a candidate. Appended (2026-11-07).
  pub moving_to: Vec<u64>,
  /// On the owner, the member ids of the holder set its volume's catalog records (A-240: what a takeover's phase one
  /// counts), ascending; it can name a transition's target before the shipper activates it. On a candidate, the same
  /// when the volume is served here (a successor before its first publication); otherwise empty, as when nothing is
  /// recorded. Appended (2026-11-07).
  pub recorded: Vec<u64>,
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
  /// Peers with a formed probe session (the direct mesh), summed over the shards: the mesh is up when this reaches
  /// [`FleetReport::peers_in_contact`], not the member count less one, since two members that neither hold for each
  /// other nor vote keep no direct contact (§4.8).
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
  /// The objects this node holds a record copy of as a candidate holder (the control shard's count).
  pub held_records: u64,
  /// The objects whose takeover this node owes and has not finished (the control shard's count): a takeover
  /// that stalls shows here.
  pub takeovers_pending: u64,
  /// The version of the regional configuration the control shard has installed: a retirement or an
  /// admission advances it.
  pub configuration_version: u64,
  /// The takeover state as the control shard's council holds it: this node's neighbourhood settlement and
  /// every retirement kept, so a takeover that stalls shows why in any node's status.
  pub takeover: TakeoverReport,
  /// What the control shard's failure detector found of each peer by its own probes (§4.8 membership), with
  /// the theory's allowance beside each count: a count far above its allowance is a detector misjudging a
  /// live peer, one within it is the network the detector was told about. Empty on a laptop.
  pub detector: Vec<DetectorPeerReport>,
  /// The control shard's record session to each peer (§4.8, the session forwards and reads ride): its congestion
  /// state as last seen, so a slow cross-region transfer shows whether the window, the round trip or loss bound it.
  /// Empty on a laptop. Appended.
  pub sessions: Vec<SessionReport>,
  /// The member's own wake lateness as its detector measured it (its granularity `G`, which floors every margin),
  /// nanoseconds; zero before measured. A loaded host stalling the condemning node shows here. Appended.
  pub detector_granularity_ns: u64,
  /// The sessions peers dialed to this node, by the peer's anchor, as its serve loops last saw them after a request:
  /// the connections a peer's forwards and reads ride, whose sender here is this node (`sessions` are the ones this
  /// node dialed). Empty on a laptop. Appended.
  pub served_sessions: Vec<SessionReport>,
  /// The control shard's failure detector's pooled verdict: whether it configured, on how many round trips, and why it
  /// last refused. Empty on a laptop. Appended.
  pub detector_pool: DetectorPoolReport,
  /// The control shard's failure detector's relay-stage timer: its timeout, and how many relayed answers came late.
  /// Empty on a laptop. Appended.
  pub detector_relays: DetectorRelayReport,
  /// The members this node keeps direct contact with, this node left out: what [`FleetReport::peers_probed`] reaches
  /// once the mesh is up. Zero on a laptop. Appended.
  pub peers_in_contact: u32,
  /// Of `peers_in_contact`, those with no formed probe session yet: the mesh is up at zero. Appended.
  pub peers_unmeshed: u32,
  /// This node's council configuration fetches as a learner, from its control shard. Appended.
  pub learner: LearnerReport,
  /// The failover delay as this node estimates it now, from its control shard (A-363): what an owner's loss costs its
  /// objects' service, learned from the detector, the council and the recoveries this node timed. Nothing sets it.
  /// Appended.
  pub failover: FailoverReport,
}

/// The failover delay as a node estimates it now (§4.8 "Derived constants", A-363): from an owner's loss to its objects
/// served again, detection plus confirmation plus recovery, each term the running estimate of the path it names, with
/// the measurements behind it, so a reader tells a measured term from a prior. Nothing sets it; it moves with what the
/// node measures. A laptop reports the recovery alone: no peer to detect and no council to confirm (R8).
#[derive(Wire, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FailoverReport {
  /// The failover delay, nanoseconds: the three terms summed.
  pub total_ns: u64,
  /// Detection: SWIM's count of the failure detector's periods from a crashed peer's last answer to its condemnation,
  /// each at the running tail of the detector's measured period (Lifeguard's local health and each pair's NFD-E margin
  /// stretch them), nanoseconds; zero where there is no peer to detect.
  pub detection_ns: u64,
  /// The detector periods timed behind it.
  pub detection_samples: u64,
  /// Confirmation: the council's death-confirmation window at the coordinator's measured period, with the retirement's
  /// commit round and its install, nanoseconds; zero where there is no council.
  pub confirmation_ns: u64,
  /// The coordinator periods timed behind it.
  pub confirmation_samples: u64,
  /// Recovery: the larger tail of the takeovers and the volumes this node timed, nanoseconds; before any, the ratified
  /// recovery budget, a prior.
  pub recovery_ns: u64,
  /// The takeovers this node timed, round begun to share confirmed.
  pub takeovers_timed: u64,
  /// The taken-over volumes this node timed, adopted to served.
  pub volumes_timed: u64,
}

impl RequestBody {
  /// The volume a request is about, when it is about one. A green over a base is born on the base volume's owner
  /// shard, where the snapshot is walked, so `CreateGreen` names its base's volume. One rule for the daemon's routing
  /// and the client's waiting ([`Self::may_be_forwarded`]).
  pub fn volume(&self) -> Option<VolumeId> {
    match self {
      RequestBody::Snapshot { volume }
      | RequestBody::DestroySnapshot { volume, .. }
      | RequestBody::Versions { green: volume }
      | RequestBody::ChangedSince { green: volume, .. }
      | RequestBody::CreateWork { green: volume, .. }
      | RequestBody::Edit { work: volume, .. }
      | RequestBody::Declare { work: volume, .. }
      | RequestBody::Submit { work: volume, .. }
      | RequestBody::SubmitApproved { work: volume, .. }
      | RequestBody::Rebase { work: volume }
      | RequestBody::Clone { volume, .. }
      | RequestBody::Attach { volume, .. }
      | RequestBody::Resize { volume, .. }
      | RequestBody::Destroy { volume }
      | RequestBody::Status { volume }
      | RequestBody::Drifted { volume, .. }
      | RequestBody::Conflicts { work: volume, .. }
      | RequestBody::ReadBase { volume, .. }
      | RequestBody::Digest { volume, .. }
      | RequestBody::Rewitness { volume, .. }
      | RequestBody::Pin { volume, .. }
      | RequestBody::AwaitPlaced { volume, .. }
      | RequestBody::MapRead { volume, .. }
      | RequestBody::MapWrite { volume, .. }
      | RequestBody::MapSync { volume, .. }
      | RequestBody::ReleaseMap { volume, .. }
      | RequestBody::ShardsHolder { volume, .. }
      | RequestBody::ReadRange { volume, .. }
      | RequestBody::ReadWindow { volume, .. }
      | RequestBody::ReadDir { volume, .. }
      | RequestBody::StageBegin { work: volume, .. }
      | RequestBody::StagePut { work: volume, .. }
      | RequestBody::EditStaged { work: volume, .. }
      | RequestBody::EditSubmit { work: volume, .. }
      | RequestBody::EditSubmitHeld { work: volume, .. }
      | RequestBody::SubmitHeld { work: volume, .. }
      | RequestBody::SubmitSigned { work: volume, .. }
      | RequestBody::FsWrite { volume, .. }
      | RequestBody::FsWriteStaged { volume, .. }
      | RequestBody::FsRemove { volume, .. }
      | RequestBody::FsRename { volume, .. }
      | RequestBody::FsMkdir { volume, .. }
      | RequestBody::Land { volume, .. }
      | RequestBody::Present { volume, .. }
      | RequestBody::ImageBuild { volume, .. }
      | RequestBody::ImageMerge { image: volume, .. } => Some(*volume),
      // A volume's policy lives in its record, on its owner shard.
      RequestBody::SetPolicy {
        subject: PolicySubject::Volume { volume },
        ..
      }
      | RequestBody::Policy {
        subject: PolicySubject::Volume { volume },
      } => Some(*volume),
      RequestBody::SetPolicy { .. } | RequestBody::Policy { .. } => None,
      // A volume's signers live in its record too (A-321); a green's version records on the green's owner shard.
      RequestBody::SetSigners { volume, .. } | RequestBody::MergeRecord { green: volume, .. } => {
        Some(*volume)
      }
      // A checkout writes its green's base volume, on the owner shard of both (`green --base` runs there).
      RequestBody::Checkout { base, .. } => Some(base.volume),
      // A green over a base is born on the base volume's owner shard, where the snapshot is walked.
      RequestBody::CreateGreen { base, .. } => base.map(|b| b.volume),
      RequestBody::Create { .. }
      | RequestBody::Advance { .. }
      | RequestBody::Detach { .. }
      | RequestBody::BindMount { .. }
      | RequestBody::List { .. }
      | RequestBody::DaemonStatus
      | RequestBody::DaemonStatusNext { .. }
      | RequestBody::Bootstrap { .. }
      | RequestBody::RecoveryPlan { .. }
      | RequestBody::Recover { .. }
      | RequestBody::PromoteRegion { .. }
      | RequestBody::Grants { .. }
      | RequestBody::Audit { .. }
      | RequestBody::Acknowledge { .. }
      | RequestBody::Telemetry { .. }
      | RequestBody::Grant { .. }
      | RequestBody::LandingEntries { .. }
      | RequestBody::Enroll { .. }
      | RequestBody::Attest { .. }
      | RequestBody::Revoke { .. }
      | RequestBody::Renew { .. } => None,
      // A credential is served on the partition its id routes to, never a volume's (A-625 part 3).
      RequestBody::EnrollCredential { .. } | RequestBody::RevokeCredential { .. } => None,
      // Retired (A-267): refused where it arrives, so routed nowhere.
      RequestBody::Read { .. } => None,
      RequestBody::Share { volume, .. }
      | RequestBody::LiveShip { volume, .. }
      | RequestBody::DetachOf { volume, .. }
      | RequestBody::RemoteReap { volume, .. }
      | RequestBody::Move { volume, .. }
      | RequestBody::CaptureBase { volume }
      | RequestBody::ArchiveExport { volume, .. } => Some(*volume),
      RequestBody::ArchiveImport { archive, .. } => Some(*archive),
      // A repository's verbs are served on the partition its name hashes to, as a create by name is (A-575).
      RequestBody::CreateRepository { .. }
      | RequestBody::CreateBranch { .. }
      | RequestBody::BindBranch { .. }
      | RequestBody::RetireBranch { .. }
      | RequestBody::SetRepositoryPolicy { .. } => None,
    }
  }

  /// Whether a verb changes the set of volumes or a volume's roots, so the owner shard must republish its recovery
  /// image (§4.8). Data-plane content writes go through the mount transport, not here.
  pub fn mutates_shard_image(&self) -> bool {
    matches!(
      self,
      RequestBody::Create { .. }
        | RequestBody::CreateGreen { .. }
        | RequestBody::CreateRepository { .. }
        | RequestBody::CreateBranch { .. }
        | RequestBody::CreateWork { .. }
        | RequestBody::Edit { .. }
        | RequestBody::EditStaged { .. }
        | RequestBody::EditSubmit { .. }
        | RequestBody::EditSubmitHeld { .. }
        | RequestBody::SubmitHeld { .. }
        | RequestBody::SubmitSigned { .. }
        | RequestBody::Declare { .. }
        | RequestBody::Submit { .. }
        | RequestBody::SubmitApproved { .. }
        | RequestBody::Rebase { .. }
        | RequestBody::Clone { .. }
        | RequestBody::Resize { .. }
        | RequestBody::Destroy { .. }
        | RequestBody::Snapshot { .. }
        | RequestBody::DestroySnapshot { .. }
        | RequestBody::Share { .. }
        | RequestBody::LiveShip { .. }
        | RequestBody::Enroll { .. }
        | RequestBody::Revoke { .. }
        | RequestBody::FsWrite { .. }
        | RequestBody::FsWriteStaged { .. }
        | RequestBody::FsRemove { .. }
        | RequestBody::FsRename { .. }
        | RequestBody::FsMkdir { .. }
        | RequestBody::Checkout { .. }
        | RequestBody::SetPolicy { .. }
        | RequestBody::SetSigners { .. }
        | RequestBody::Move { .. }
        | RequestBody::CaptureBase { .. }
    )
  }

  /// Whether a verb is a **read** a node forwards to a volume's owner without a completion record: a volume-scoped
  /// query that mutates nothing, so re-serving a retried forward is idempotent (§4.8 "Lookup"). A staging verb
  /// forwards the same way: a retried put of bytes already written is answered unchanged, and a retried begin only
  /// strands a charged buffer that expires within a lease.
  pub fn forwards_as_read(&self) -> bool {
    matches!(
      self,
      RequestBody::Status { .. }
        | RequestBody::Drifted { .. }
        | RequestBody::Conflicts { .. }
        | RequestBody::Versions { .. }
        | RequestBody::MergeRecord { .. }
        | RequestBody::ChangedSince { .. }
        | RequestBody::ReadRange { .. }
        | RequestBody::ReadWindow { .. }
        | RequestBody::ReadDir { .. }
        | RequestBody::StageBegin { .. }
        | RequestBody::StagePut { .. }
    )
  }

  /// Whether a verb is a **write** to an existing volume a node forwards to that volume's owner under a completion
  /// record (§4.8 "Lookup"): a volume-scoped mutation ([`Self::volume`] names the volume, [`Self::mutates_shard_image`]
  /// changes it).
  pub fn forwards_as_write(&self) -> bool {
    self.volume().is_some()
      && (self.mutates_shard_image()
        // An attachment in the record form, and its detach by volume, are recorded on the volume's owner whatever
        // host the client is on (§4.10, A-222). Every other form hands the client something of the owner's host (a
        // mount, a device, a stream), so it is not forwarded and is refused from another host by its kind.
        || matches!(
          self,
          RequestBody::Attach {
            form: AttachRequest::Root,
            ..
          } | RequestBody::DetachOf { .. }
        ))
  }

  /// Whether a node may forward this verb to another node, so its reply can take a location round and a forward
  /// across the fleet, each bounded by the daemon (§4.8 "Lookup"): longer than a client's reply deadline, while the
  /// daemon always answers. The client waits for such a verb while its daemon lives (`slates_client::defers_reply`).
  pub fn may_be_forwarded(&self) -> bool {
    self.forwards_as_read() || self.forwards_as_write()
  }
}

/// One record session to a peer, as this node last saw it (§4.8; condition 7's evidence): the sender's congestion
/// state on the session forwards and reads ride. A session out on loan cannot be read, so the report is its state when
/// it was last returned (`lent` says it is out now); zero everywhere when it has not been seen yet.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct SessionReport {
  /// The peer's member id.
  pub peer: u64,
  /// Whether the session is out on loan now, so these numbers are from its last return.
  pub lent: bool,
  /// The congestion window, bytes (RFC 9002 §7; the controller slates runs, Copa).
  pub congestion_window: u64,
  /// The smoothed round trip, nanoseconds (RFC 9002 §5.3).
  pub smoothed_rtt_ns: u64,
  /// The probe timeout, nanoseconds (RFC 9002 §6.2.1).
  pub pto_ns: u64,
  /// Losses declared and then found spurious (a reordering mistaken for loss).
  pub spurious_losses: u64,
  /// Persistent-congestion collapses of the window (RFC 9002 §7.6).
  pub persistent_collapses: u64,
  /// Stream bytes the session has consumed from its peer.
  pub bytes_consumed: u64,
  /// The path MTU discovered, bytes; zero before discovery.
  pub path_mtu: u64,
}

/// One peer as this node's failure detector judges it (hyper-swim's `PeerReport`): the suspicions and
/// condemnations its own probes made, each beside the expected number were the peer alive throughout
/// (Chen, Toueg and Aguilera 2002, Theorem 7, summed over every judged probe), in thousandths.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct DetectorPeerReport {
  /// The peer's member id.
  pub peer: u64,
  /// Whether the pair's own estimator judges its probes (rather than the member's pooled one, or none yet).
  pub configured: bool,
  /// Suspicions this node's own probes started.
  pub suspicions: u64,
  /// Their allowance in thousandths: the expected suspicions of a peer alive throughout.
  pub suspicion_allowance_milli: u64,
  /// Times this node condemned the peer by its own probes.
  pub condemnations: u64,
  /// Their allowance in thousandths: over every judged probe, the bound on it and the probe before both
  /// going unanswered.
  pub condemnation_allowance_milli: u64,
  /// The verdict that judges the pair's probes now: `own` (its estimator configured), `pool` (the member's pooled
  /// verdict), `provisional` (a pair the pool does not fit, or a first contact: hyper-swim claims no Theorem 7 bound
  /// for it), or `measuring` (no verdict yet). What tells a false condemnation under a bounded verdict, a defect, from
  /// one under a provisional verdict, a design limit. Appended.
  pub judged_by: String,
  /// That verdict's expected arrival of an answer, nanoseconds (zero while measuring). Appended.
  pub expected_ns: u64,
  /// That verdict's margin past the expected arrival before a probe counts as unanswered, nanoseconds. Appended.
  pub margin_ns: u64,
  /// The round trips the pair's own estimator has taken (hyper-swim's `round_trips_taken`). Appended.
  pub samples: u64,
  /// The judging verdict's mistake bound in thousandths: 1000 for a provisional verdict (no Theorem 7 bound), the
  /// configured β otherwise; zero while measuring. Appended.
  pub mistake_milli: u64,
  /// Whether the member's pooled verdict does not fit the pair (hyper-swim's `misfits_pool`): its round trips do not feed
  /// the pool, and before its own estimator configures it is judged provisionally by its own round trip (while the pool
  /// has no verdict, only once it has been silent past RFC 6298's timeout). Appended.
  pub pool_misfit: bool,
  /// Why the pair's own estimator last refused to configure (hyper-timing's `Refusal`), empty once it configured or
  /// before it tried. Appended.
  pub refused: String,
}

/// The relay stage as the member's failure detector bounds it (hyper-swim's `RelayReport`): each relay judges its own
/// leg to the target by its own deadline and nacks when that passes unanswered, and the stage ends once every relay
/// asked has nacked, or when the slowest has its response due, by the member's span for it and the bound it stated.
/// A relay-reachable peer condemned under load says here how long its relay stages could wait.
#[derive(Wire, Clone, Debug, Default, PartialEq, Eq)]
pub struct DetectorRelayReport {
  /// The latest any relay the member could ask has its response due, from the request, nanoseconds (zero while no
  /// alive peer has stated a relay bound, when an unanswered probe asks no relay).
  pub due_ns: u64,
  /// The peers that have stated a relay bound.
  pub stated: u64,
}

/// The member's pooled verdict as its failure detector holds it (hyper-swim's `PoolReport`): what judges every pair that
/// has no verdict of its own and fits the pool, so a peer shown `measuring` says whether the pool never configured,
/// and why.
#[derive(Wire, Clone, Debug, Default, PartialEq, Eq)]
pub struct DetectorPoolReport {
  /// Whether the pool has a verdict (it keeps one once configured).
  pub configured: bool,
  /// The round trips the pool took.
  pub samples: u64,
  /// Why the pool's latest configuration refused (hyper-timing's `Refusal`), empty after one succeeded or before any
  /// was tried.
  pub refused: String,
  /// The pool's verdict's expected arrival of an answer, nanoseconds (zero before configured).
  pub expected_ns: u64,
  /// The pool's verdict's margin, nanoseconds (zero before configured).
  pub margin_ns: u64,
}

/// The takeover state a council holds (§4.8 "Neighbourhood changes", "Promotion and takeover"): this node's
/// settled and current neighbourhood versions, and every retirement the configuration keeps.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct TakeoverReport {
  /// The version this node's settled neighbourhood was fixed at — the set its records are all placed on.
  pub settled_generation: u64,
  /// The version its current neighbourhood was fixed at: equal to the settled one outside a change in
  /// flight.
  pub neighbourhood_generation: u64,
  /// The retirements the configuration keeps, oldest first.
  pub retirements: Vec<RetirementReport>,
  /// How many members the regional configuration holds: the council's committed membership, where the fleet
  /// block's `members` is the ones this node's detector holds alive.
  pub members: u64,
}

/// One retirement the configuration keeps (§4.8 "Promotion and takeover").
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct RetirementReport {
  /// The retired host.
  pub host: u64,
  /// The configuration version that retired it.
  pub version: u64,
  /// The members its objects' successors are ranked among, fixed at the retirement.
  pub survivors: Vec<u64>,
  /// The survivors that confirmed their share of its takeover.
  pub confirmed: Vec<u64>,
  /// The retired hosts whose takeover it had not confirmed its share of when it retired.
  pub unconfirmed: Vec<u64>,
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
  /// The node's sealing at rest (A-92). Appended.
  pub seal: SealReport,
  /// Every message the anchor's hold channel refused over its life, as the anchor last wrote it to the segment
  /// (A-213); the anchor's log says each distinct refusal once, so this is where the rest show. Appended.
  pub anchor_hold_refusals: u64,
  /// Log lines the daemon could not write over its life (its standard error closed, or a pipe whose reader went away),
  /// each counted and dropped (A-213). Appended.
  pub log_lines_refused: u64,
  /// Log lines the anchor could not write over its life, as it last wrote the count to the segment (A-213). Appended.
  pub anchor_log_lines_refused: u64,
  /// Windows Error Reporting is set to write a full-memory dump of the daemon when it crashes (A-239: `LocalDumps`, for
  /// every program or for the daemon's own image), which carries every byte the daemon holds, tenant content and keys
  /// included. An administrator's setting the daemon cannot override (R10); it reports it and serves. Always `false` on
  /// Unix, where the process is excluded from core dumps whole. Appended.
  pub dumps_local_full: bool,
  /// This node's certificate anchor (A-237): the host a principal of this node names, as a share to one of its
  /// accounts must name it. Stable across the node's restarts. Appended.
  pub anchor: u64,
  /// The anchor has judged this daemon recovered (A-311): it has served as long as its start took, so its exit from
  /// here on is no failed start and is restarted at once, where an exit before would count toward a crash loop. Whoever
  /// ends a daemon on purpose (a test's kill, an operator's) waits for it first. `false` with no anchor, which judges
  /// nothing. Appended.
  pub recovered: bool,
}

/// The node's sealing at rest (A-92): how its root came (`unavailable`, `minted`, `minted-daemon-only` or
/// `adopted`), the root's id (never its key; empty while sealing is unavailable), and hyper-seal's locked key
/// region: its slots and the keys held in it now.
#[derive(Wire, Clone, Debug, PartialEq, Eq, Default)]
pub struct SealReport {
  /// How the root came.
  pub state: String,
  /// The root's key id, 16 bytes, or empty.
  pub root_id: Vec<u8>,
  /// The key region's slots.
  pub key_slots: u64,
  /// The keys held in it now.
  pub keys_held: u64,
  /// The node's ML-KEM-1024 recipient id (A-92 piece 4a), or empty while sealing is unavailable. Appended.
  pub recipient_id: Vec<u8>,
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
  /// Whether it is live-shipped (A-200). Appended.
  pub live_shipped: bool,
  /// Why recovery could not rebuild it after the daemon's restart, when it could not (§4.8): every verb on it but its
  /// destroy answers [`Refusal::NotRecovered`] with this cause. Appended.
  pub not_recovered: Option<RecoveryCause>,
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
  /// Create a namespace inode with no content, with explicit metadata and no endpoint state: a FIFO or socket name
  /// (A-26), or a device node and its number (A-294).
  Mknod {
    /// The new path.
    path: String,
    /// The metadata recorded by the declaring volume.
    node: IpcNode,
  },
}

/// A declared kind of name with no content: a FIFO or socket name (A-26), or a device node with its number, metadata
/// nothing opens as a device (A-294). The device kinds are appended, so the two earlier tags are unchanged.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpcNodeKind {
  /// A named pipe without queued bytes.
  Fifo,
  /// A socket name without a listener or connection.
  Socket,
  /// A block device node (A-294).
  BlockDevice {
    /// The major number.
    major: u32,
    /// The minor number.
    minor: u32,
  },
  /// A character device node (A-294); a whiteout is one numbered 0/0.
  CharDevice {
    /// The major number.
    major: u32,
    /// The minor number.
    minor: u32,
  },
}

/// Metadata supplied by a work's declared creation of a name with no content (§4.16, A-26, A-294). These are volume
/// metadata, not host credentials, endpoint authority or device access. Replay never reads a local clock.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct IpcNode {
  /// The kind, with a device node's number.
  pub kind: IpcNodeKind,
  /// Permission bits.
  pub mode: u32,
  /// Volume owner id.
  pub uid: u32,
  /// Volume group id.
  pub gid: u32,
  /// Last access time, in volume-clock nanoseconds.
  pub atime: i64,
  /// Last modification time.
  pub mtime: i64,
  /// Last metadata change time.
  pub ctime: i64,
  /// Creation time.
  pub btime: i64,
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
  /// The host mounts bound to a path (§4.4 `Bound`; GAP-A9-4), as many as the report's mount budget
  /// carries; `mounts_elided` counts the rest. Appended.
  pub mounts: Vec<MountReport>,
  /// Bound mounts the budget did not carry. Appended.
  pub mounts_elided: u32,
  /// The volume's journal: its budget, and what its consuming rounds found waiting (§4.5). Appended.
  pub journal: JournalReport,
  /// Unlinked files kept for a mount that survived a daemon's takeover (U10): its kernel may hold a handle the dead
  /// daemon issued after its last publication, which no image counted, so such a file is kept, charged to the volume's
  /// quota, until that mount ends. Zero outside a takeover. Appended.
  pub takeover_kept: u32,
  /// An overlay's base as this status read it, typed (§4.15 "Drift"; A-299): the facts `drifted` and `watcher` carry as
  /// text for people, for a consumer to match. `None` for a volume with no base folder (a scratch volume, a clone of
  /// one, a green, a work volume). Appended.
  pub base: Option<BaseReport>,
  /// What an owner loss would lose of the volume now, and what the takeover that made this node its owner lost (§4.10
  /// "Auto-seal", AC-8.3, D-18). Appended.
  pub loss: LossReport,
  /// Drifted or unverified entries the report did not carry (A-267; appended): `drifted` and `base`'s lists hold the
  /// first that fit beside the report's other fields, the same entries in both, and `Drifted` reads every one.
  pub drifted_elided: u32,
}

/// An overlay's base as a status read it (§4.15 "Drift"; A-299). Append-only.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum BaseReport {
  /// The base was read.
  Read {
    /// Each entry the disk changed after the volume witnessed it: landing it would conflict.
    drifted: Vec<DriftEntry>,
    /// Each entry whose re-check the host refused now, by path: not known to match its witness.
    unverified: Vec<String>,
    /// The base's watcher.
    watcher: WatcherState,
  },
  /// The base could not be read now: its drift is unknown.
  Unreadable,
}

/// One drifted entry of an overlay (§4.15 "Drift").
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct DriftEntry {
  /// The entry, from the volume's root.
  pub path: String,
  /// What the disk did to it.
  pub kind: DriftKind,
}

/// What changed on the disk beneath a witnessed entry (§4.15 "Drift"). Append-only.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriftKind {
  /// The same file, with other bytes or attributes.
  Modified,
  /// The entry is gone.
  Deleted,
  /// Another file sits at the path.
  Replaced,
  /// A file became a directory, or the reverse.
  TypeChanged,
}

/// An overlay's watcher (§4.15): hints that make drift reports prompt, never the truth. Append-only.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatcherState {
  /// Delivering hints.
  Live,
  /// It lost events; a full re-check is done or scheduled.
  Overflowed,
  /// None on this host or folder: fingerprints alone.
  Unavailable,
}

/// A volume's loss window (§4.10 "Auto-seal", AC-8.3, D-18): what of it lives on its owner alone, and what the takeover
/// that made this node its owner lost. Every time is this host's own clock's, so no two hosts' clocks are compared; a
/// seal is named by its head sequence, which the owner and every successor share.
#[derive(Wire, Clone, Debug, Default, PartialEq, Eq)]
pub struct LossReport {
  /// The window an owner loss would lose now, nanoseconds (the design's `last_placed_snapshot_age`, made exact): zero
  /// when every change is on another host too (the head sealed and placed with nothing changed since, or a live-shipped
  /// log in a fleet, whose acknowledged writes are durable on its holders); otherwise the age of the newest seal placed
  /// on `f + 1` hosts, or of the volume itself when none is (on a laptop, `f = 0`, nothing is on another host). `None`
  /// when the time is unknown (taken under another boot of this host's clock) or not measured: never reported as zero.
  pub at_risk_ns: Option<u64>,
  /// The head sequence of the newest seal placed on `f + 1` hosts: the one a takeover would serve from now, so the
  /// edits after it are the window. `None` while no seal is placed on another host.
  pub placed_sequence: Option<u64>,
  /// Whether the volume ships its live log to holders in a fleet (A-200), so an owner loss loses no write acknowledged
  /// at durability.
  pub live_shipped: bool,
  /// Set on a node that took the volume over from a lost owner: what it serves from, and so what the loss took, and
  /// whether the takeover went past the volume's tolerance (A-332). Recorded with the volume (A-325), so it survives this
  /// node's restart.
  pub taken_over: Option<VolumeTakeover>,
}

/// The takeover that made this node a volume's owner (AC-8.3): the loss window an owner loss left.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct VolumeTakeover {
  /// The epoch the volume has been served under since.
  pub epoch: u64,
  /// Served from the volume's live log: every write the lost owner acknowledged at durability survived.
  pub from_live_log: bool,
  /// The head sequence served from: when the volume was not served from its live log, the lost owner's edits after the
  /// seal it names (its `placed_sequence` when it was lost) are the loss window.
  pub head_sequence: u64,
  /// Set when the takeover recovered the volume past its tolerance (A-309, A-332): more of the hosts holding it were lost
  /// than its placement tolerates, so it was rebuilt from the newest copy the hosts that remained held, and a write
  /// acknowledged only to the lost hosts may be gone. `None` within tolerance, where every write the lost owner
  /// acknowledged at durability survived. Appended.
  pub past_tolerance: Option<PastTolerance>,
}

/// How a takeover went past a volume's tolerance (A-309, A-332): which of its parts were recovered from fewer surviving
/// holders than recovery counts on, and by how much. At least one part is set.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PastTolerance {
  /// The volume's head register: its first cohort short of the recovery quorum when the takeover adopted the newest
  /// record every surviving member of the volume's cohorts held. `None` when it was recovered within tolerance.
  pub registers: Option<Shortfall>,
  /// The volume's live log (A-200): its recorded holders short of `f + 1` members when the takeover adopted the greatest
  /// head every surviving recorded holder reported. `None` when the log was recovered within tolerance, or the volume
  /// has none.
  pub live_log: Option<Shortfall>,
}

/// A recovery count short of its quorum (A-332): the hosts recovery counts on, and how many of them were still members
/// when the takeover recovered past tolerance.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shortfall {
  /// The hosts recovery needed: a cohort's recovery quorum, or `f + 1` of a live log's recorded holders.
  pub needed: u64,
  /// How many of the hosts that count were members then (fewer than `needed`).
  pub remaining: u64,
}

/// A volume's journal as its consuming rounds measured it (§4.5 "Journal"). The budget's formula is the mutation rate
/// times the longest subscriber lag times the record size, and a round's backlog is that product measured directly:
/// the records that waited for it. Maxima since the journal was made.
#[derive(Wire, Clone, Debug, Default, PartialEq, Eq)]
pub struct JournalReport {
  /// The bytes of records the journal may retain.
  pub budget_bytes: u64,
  /// The bytes the retained records are charged.
  pub retained_bytes: u64,
  /// Records retained.
  pub retained_records: u64,
  /// The mean charge of a retained record, in bytes (a record's fixed size when none is retained).
  pub mean_record_bytes: u64,
  /// Records appended since the journal was made.
  pub appended: u64,
  /// Records retention has dropped.
  pub dropped: u64,
  /// Consuming rounds measured.
  pub rounds: u64,
  /// Rounds that found their cursor behind retention (A-141's overruns).
  pub overruns: u64,
  /// The largest backlog a round found, in records.
  pub largest_records: u64,
  /// The largest backlog a round found, in bytes: the formula's product, measured.
  pub largest_bytes: u64,
  /// How long the oldest record of that largest backlog had waited, in nanoseconds.
  pub largest_lag_ns: u64,
  /// The longest any round's oldest record had waited, in nanoseconds.
  pub longest_lag_ns: u64,
  /// The readers open: the volume's FUSE mounts that consume the journal. Appended.
  pub readers: u64,
  /// How many records the slowest open reader may still read: the head less its point. Zero with no reader open.
  /// Appended.
  pub slowest_reader_behind: u64,
  /// The bytes the shard's metadata ledger granted the budget beyond its one-page floor, as a live reader needed them
  /// (A-157). Appended.
  pub grown_bytes: u64,
  /// Growths the ledger refused, each overrunning the reader that still owed the record dropped. Zero is the healthy
  /// reading. Appended.
  pub growth_refused: u64,
}

/// One host mount of a volume, bound to its mount point (§4.4 `Bound`).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct MountReport {
  /// The attachment.
  pub attachment: u64,
  /// The mount point the mounting process established and reported (`BindMount`).
  pub path: String,
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
  /// A Shards microVM's fs device over an anonymous stream the daemon hands the attaching client (§4.6, the Shards
  /// route; A-161). Appended.
  ShardsFs,
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
  /// The bind source a host mount exports, verified against the kernel's mount table at attach and checked
  /// again by the harness just before its runtime binds it (`slates oci-check`). Whether a runtime consumes
  /// the source as tested is not this row's to say: the harness's runtime handshake (`slates oci-runtime`)
  /// names the runtime's profile and the container workload that ran through it (T-4.13), or refuses an
  /// untested profile typed (AUD-29-67).
  VerifiedSourceExport,
  /// The simulated guest driver's differential oracle against direct FUSE dispatch
  /// (`crates/bridge-virtiofs`); no live guest has run (AC-9.7).
  SimulatedGuestDriver,
  /// A live Linux guest under a real VMM mounted the tag and ran the conformance roster's workloads, each
  /// identical on the volume and on the guest's RAM (AC-9.7; `crates/server/tests/virtiofs.rs`). Appended.
  LiveGuestWorkloads,
  /// A test plays Shards' fs device over the stream end against a real daemon, each change checked through
  /// another transport (`crates/server/tests/shards.rs`, `crates/client/tests/shards.rs`); no Shards microVM has
  /// run against the daemon (AC-9.7). Appended.
  SimulatedShardsDevice,
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
  /// The host offers no unprivileged FUSE mount: `/dev/fuse` or the OS's `fusermount3` is missing (R10:
  /// slates never mounts with a privilege of its own).
  FuseUnavailable,
  /// The bind's source authority is verified (the mount's attachment, held to the daemon's record), but no
  /// container workload has run through this platform's bind (T-4.13), so it is not offered until one does
  /// (AUD-29-64: a recipe is not evidence that a runtime consumes it as intended).
  ContainerWorkloadUnproven,
  /// A mount another user's processes (a container runtime's) can reach needs `allow_other`, which the host's
  /// operator grants with `user_allow_other` in `/etc/fuse.conf`; this host's mount helper refused it.
  /// Appended.
  AllowOtherNotGranted,
  /// A container bind on Linux binds a mount other users' processes can reach; the source is a FUSE mount the
  /// daemon made for its user alone. Mount it with `slates mount --shared`. Appended.
  MountNotShared,
  /// The form hands the client a descriptor, and this platform's route for one is not built. Every platform slates
  /// ships has its route now (Linux the control socket, macOS the socket route, Windows the handle route, A-161 and
  /// A-179), so no daemon answers it; it stays for the wire's append-only order and for an older daemon. Appended.
  DescriptorRouteNotBuilt,
  /// WinFsp's user-mode library (`winfsp-x64.dll`) could not be loaded: WinFsp is not installed on this host. slates
  /// installs nothing (R10); WinFsp's own installer is the one privileged piece (§4.6). Appended.
  WinFspNotInstalled,
  /// The volume's owner is another host, and this form hands the client something of the owner's host (a mount, a
  /// device, a stream, a container bind); only the record form is served across hosts (§4.10, A-222). Appended.
  OwnedByAnotherHost,
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

/// The host's transport report (§4.6 A-9): the facts that qualify it, then every transport. The consuming
/// container runtime is not among the facts: the daemon does not run it and a name on its `PATH` certifies
/// nothing (AUD-29-67); the harness that holds the runtime asks it for its profile (`slates oci-runtime`).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct TransportReport {
  /// The operating system as the kernel names itself (`uname` sysname; `windows` there).
  pub os: String,
  /// The kernel release (`uname` release); `None` where the OS states none.
  pub kernel: Option<String>,
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
  /// A host kernel mount the daemon's OS filesystem bridge serves (§4.6, §4.13; the `slates mount`
  /// flow): the attachment is the **mount's**, recorded under the bridge consumer, so it outlives the
  /// process that requested it (the CLI exits after `mount_nfs`) and the daemon (the anchor keeps the
  /// listener across a restart), and ends only when the kernel unmounts the mount (seen through the kernel's own
  /// unmount event, A-511; a `UMNT` ends nothing), with an explicit `detach`, or with the volume's destroy. The
  /// reply's `token` is the capability the mount presents.
  HostMount,
  /// A Linux FUSE mount of the volume at `mount_point` — an existing directory the caller owns — that the
  /// daemon establishes through the OS's `fusermount3` and serves on the volume's owner shard (§4.6
  /// "Linux"; AUD-29-64). The attachment is the mount's, as a `HostMount`'s is, ending with the kernel's
  /// unmount, a `detach`, or the volume's destroy; the reply comes once the mount is established.
  FuseMount {
    /// The mount point, canonical.
    mount_point: String,
  },
  /// A host mount presenting only one directory of the volume (§4.6 scoped exports; AUD-29-76): the export's
  /// root is that directory, `..` there is itself, and no handle reaches outside it — enforced by the export
  /// on every request, so a symbolic link swapped in after the attach changes nothing. Recorded by the
  /// directory's inode, so a later rename of it does not change what the mount presents.
  ScopedHostMount {
    /// The directory, as a path from the volume's root.
    subtree: String,
  },
  /// A Linux FUSE mount at `mount_point` presenting only one directory of the volume (§4.6 scoped exports;
  /// AUD-29-76): a `FuseMount` whose root is that directory, held to it as a `ScopedHostMount` is.
  ScopedFuseMount {
    /// The mount point, canonical.
    mount_point: String,
    /// The directory, as a path from the volume's root.
    subtree: String,
  },
  /// A Linux FUSE mount other local users' processes can reach — a container runtime binding it into a
  /// container (§4.6 A-9) — mounted `allow_other`, the kernel checking each caller's permission bits
  /// (`default_permissions`). Optionally one directory of the volume. Refused `AllowOtherNotGranted` where the
  /// host's operator has not granted `user_allow_other`.
  SharedFuseMount {
    /// The mount point, canonical.
    mount_point: String,
    /// The directory presented, as a path from the volume's root; the whole volume when absent.
    subtree: Option<String>,
  },
  /// A host mount bound to one Unix identity (§4.6 A-115): requests whose `AUTH_SYS` caller is `uid` are served;
  /// every other caller, the superuser's included, is answered attributes and an `ACCESS` granting nothing, so a
  /// kernel's per-user access cache stops its walk at the export's root and no link target one identity read is
  /// served to another from the client's cache. Optionally one directory. Appended.
  BoundHostMount {
    /// The identity served.
    uid: u32,
    /// The directory presented, as a path from the volume's root; the whole volume when absent.
    subtree: Option<String>,
  },
  /// A Shards microVM's fs device served over an anonymous stream (§4.6, the Shards route; A-161): the daemon makes a
  /// socket pair, serves the attachment on its end, and hands the other end to the client, which passes it to the
  /// VMM. The end reaches the client over its control socket on Linux; the reply names the attachment. Optionally
  /// one directory. Appended.
  ShardsFs {
    /// The directory presented, as a path from the volume's root; the whole volume when absent.
    subtree: Option<String>,
  },
  /// slates' own virtio-fs device served to an out-of-process VMM over vhost-user (§4.6; goal G-vhost): the daemon makes
  /// a socket pair, serves the device's back end on its end, and hands the other end to the client, which passes it to
  /// the VMM as its front end's socket (crosvm takes `socket=/proc/self/fd/N`). No socket file exists anywhere (R1). The
  /// end reaches the client over its control socket, sent before the reply; Linux only. The attachment ends when the
  /// VMM's end closes, as well as by `detach`, a revocation or the volume's destroy. Appended.
  VhostUserFs {
    /// The tag the guest mounts (`mount -t virtiofs <tag>`): at most 36 bytes of UTF-8, no NUL.
    tag: String,
    /// The directory presented, as a path from the volume's root; the whole volume when absent.
    subtree: Option<String>,
    /// The DAX window offered to the front end, in bytes: a power of two of at least one 2 MiB range. None offers no
    /// window, and the guest reads and writes through the queue alone.
    dax_window: Option<u64>,
  },
  /// A Windows WinFsp volume of the whole volume at `mount_point`, a free drive letter (`S:`), that the daemon
  /// establishes and serves on the volume's owner shard (§4.6 "Windows"). The attachment is the mount's, as a
  /// `FuseMount`'s is: it ends with a `detach`, the volume's destroy, or the daemon's process, which holds the WinFsp
  /// file system. A directory mount point is refused (an NTFS reparse point, §4.6). Appended.
  WinFspMount {
    /// The drive letter and its colon (`S:`).
    mount_point: String,
  },
  /// A CLI attachment (§4.7, A-230): the record form, made by a process that exits for later processes to name. It
  /// outlives its client for a term, the failover SLO as the write lease's, renewed by every verb that names it, and
  /// ends at the term's lapse; a verb naming it after is refused `AttachmentExpired`. Appended.
  Termed,
}

impl AttachRequest {
  /// The mount point of a FUSE form (whole or scoped), which the daemon itself mounts.
  pub fn fuse_mount_point(&self) -> Option<&str> {
    match self {
      Self::FuseMount { mount_point }
      | Self::ScopedFuseMount { mount_point, .. }
      | Self::SharedFuseMount { mount_point, .. } => Some(mount_point),
      _ => None,
    }
  }

  /// The directory a scoped form presents, as a path from the volume's root.
  pub fn subtree(&self) -> Option<&str> {
    match self {
      Self::ScopedHostMount { subtree } | Self::ScopedFuseMount { subtree, .. } => Some(subtree),
      Self::SharedFuseMount { subtree, .. }
      | Self::BoundHostMount { subtree, .. }
      | Self::ShardsFs { subtree }
      | Self::VhostUserFs { subtree, .. } => subtree.as_deref(),
      _ => None,
    }
  }
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
  /// The kernel's identity of the verified mount instance — its mount id and its device (AUD-29-66) — which
  /// a harness checks again just before its runtime binds the path (`slates oci-check`), so it never binds a
  /// replacement or a directory left where the mount was. Appended for append-only evolution.
  pub mount_id: u64,
  /// See [`HostMountEvidence::mount_id`].
  pub mount_device: u64,
  /// Whether the source was mounted **shared with other users** (`allow_other`), which decides whether a
  /// **rootful** runtime's container processes can read it: they reach the mount as ids other than the
  /// mounting user's, and a FUSE mount without `allow_other` answers them `EACCES`. A **rootless** runtime
  /// maps container-root to the mounting uid and reads it either way — measured 2026-10-19, a rootless
  /// podman container read and wrote a slates volume through an unshared FUSE mount.
  ///
  /// Reported rather than enforced, because slates cannot see which runtime the harness will drive and
  /// §4.6 A-9 puts that work on the harness ("slates does the namespace work of neither the runtime nor the
  /// launcher"; "a metadata record is insufficient evidence of a usable container path"). A harness driving
  /// a rootful engine checks this and refuses; one driving a rootless engine binds. Until 2026-10-20 the
  /// attach *refused* an unshared source outright, which blocked the unprivileged case R10 prefers and
  /// effectively demanded a root edit of `/etc/fuse.conf` for this form. Appended for append-only evolution.
  pub shared_with_other_users: bool,
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
  /// Another mount sits beneath the source mount point (AUD-29-65): the bind is of the source mount alone
  /// (non-recursive), so the container would not see what the host shows there — refused rather than bind
  /// a view the host contradicts or expose a filesystem no slates attachment authorizes.
  DescendantMount {
    /// The mount point beneath the source, as the table records it.
    mount_point: String,
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
  /// Entries per action name, for people (a consumer reads [`LandingSummary::actions`]).
  pub by_action: Vec<ActionCount>,
  /// Bytes the landing writes.
  pub bytes: u64,
  /// Entries the filter left out.
  pub filtered_out: u64,
  /// Entries per action, typed (A-299, 2026-11-18: until then a consumer read `by_action`'s names). Appended.
  pub actions: ActionTally,
}

/// A landing manifest's entries per action (§4.15 "Plan"), each [`PlannedAction`]'s count. Append-only.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ActionTally {
  /// Files the disk did not have, written.
  pub create: u64,
  /// Files the disk had, replaced.
  pub replace: u64,
  /// Files the disk had, removed.
  pub delete: u64,
  /// Directories moved whole from their origins.
  pub rename: u64,
  /// Directories made.
  pub mkdir: u64,
  /// Directories removed with everything beneath them.
  pub rmdir: u64,
  /// Symlinks made or replaced.
  pub symlink: u64,
  /// Directories emptied and kept.
  pub clear: u64,
  /// Files whose mode or Windows attribute bits alone are set, their bytes kept (A-458). Appended.
  pub chmod: u64,
}

/// One entry of a presented landing's manifest (§4.15 step 2, "the full entry list on demand"): what the landing would
/// do at one path, as the manifest its grant binds names it. `LandingEntries` pages them, in the manifest's order (its
/// landing order: directories made first, removals last and deepest first).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct PlannedEntry {
  /// The path, from the target's root (`/src/a.rs`).
  pub path: String,
  /// What the landing does there.
  pub action: PlannedAction,
  /// A file's planned bytes, for a create or a replace; none for anything else.
  pub content: Option<PlannedContent>,
  /// The mode a file or a directory lands with (the volume's, as `stat` reports it); none for a symlink and for a
  /// removal or a rename, which land no mode of the volume's.
  pub mode: Option<u32>,
  /// Whether the entry replaces or removes what the volume witnessed on the disk; false for what it creates.
  pub witnessed: bool,
  /// The conflict the presentation's preliminary verdict found at this path, or none: a landing with any is refused.
  pub conflict: Option<ConflictClass>,
}

/// What a landing does at one path (§4.15 "Plan"; the manifest's actions).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum PlannedAction {
  /// A file the disk did not have, written.
  Create,
  /// A file the disk had, replaced.
  Replace,
  /// A file the disk had, removed.
  Delete,
  /// A directory the disk had, moved here whole from its origin, as one rename.
  Rename {
    /// The origin, from the target's root.
    from: String,
  },
  /// A directory made.
  Mkdir,
  /// A directory the disk had, removed with everything beneath it.
  Rmdir,
  /// A symlink made, or one the disk had replaced.
  Symlink {
    /// Its target, as the volume holds it.
    target: String,
  },
  /// A directory the disk had, emptied of everything beneath it and kept (the volume removed it and made it again).
  Clear,
  /// A file the disk had whose bytes the landing keeps and whose mode or Windows attribute bits it sets (A-458): the
  /// entry's `mode` is the mode it lands with, beside what it had. Appended.
  Chmod {
    /// The permission bits it had (`0o7777` of its mode).
    permissions_was: u32,
    /// The Windows attribute bits it had (A-262, at Windows' values; none on Linux and macOS).
    windows_was: u16,
    /// The Windows attribute bits it lands with.
    windows: u16,
  },
}

/// A conflict class of §4.15 step 4's verdict table: why a landing would refuse an entry.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictClass {
  /// The volume and the disk both changed the entry, differently.
  ModifyModify,
  /// The volume changed it; the disk deleted it.
  ModifyDelete,
  /// The volume deleted it; the disk changed it.
  DeleteModify,
  /// The volume renamed the directory; the disk moved its origin.
  RenameRename,
  /// The volume and the disk both created the entry, with different bytes.
  CreateCreate,
  /// A file became a directory, or the reverse.
  TypeChanged,
  /// Someone else replaced the file between the check and the write.
  TargetInUse,
}

/// A planned file's bytes, as the manifest binds them: their BLAKE3 and their length. A reader of the volume's file
/// checks what it read against them, so what it read is what the grant would land.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlannedContent {
  /// BLAKE3 of the bytes.
  pub hash: [u8; 32],
  /// Their length.
  pub size: u64,
}

/// Where a page of a presented landing's entries starts (`LandingEntries`): an entry, by its place in the manifest's
/// order, and a byte within its encoding. The list starts at the zero cursor; each page names the next.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq, Default, PartialOrd, Ord)]
pub struct EntriesCursor {
  /// The entry.
  pub entry: u64,
  /// The byte within the entry's encoding.
  pub byte: u64,
}

/// A finished landing's outcome (§4.15): the counts, and what the landing achieved and met — its
/// durability, every Degraded cell, the entries it kept private, and the barriers it took (AUD-29-05:
/// until 2026-09-29 the reply carried the counts only, and a landing that did not sync read as done).
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
  /// Entries on the disk that stay in the volume's overlay because they did not reach the durability
  /// boundary (their directory, or the media barrier asked for, did not sync); a resume advances them.
  pub held: u64,
  /// The durability the landing achieved (§4.15 step 8).
  pub durability: LandingDurability,
  /// Every Degraded cell the landing met, in the order met, as many as one reply holds (A-267).
  pub degraded: Vec<LandingDegradation>,
  /// The batch barriers the landing took (§4.15 step 8, A-130): each made a batch of written-out files durable
  /// before any of them was placed, so fewer than the files written whenever a batch held more than one.
  pub barriers: u64,
  /// The terminal state, typed (A-299, 2026-11-18: until then a consumer read `state`'s name). Appended.
  pub stage: LandingStage,
  /// How the landing's signature was judged against the manifest it wrote (signed work, A-321), as its finished
  /// record keeps it: `Unsigned` when the landing carried none. Appended.
  pub verification: Verification,
  /// Degraded cells past what one reply holds (A-267), never sent: a host that refuses every directory's sync meets
  /// one a directory, so `degraded` carries the first that fit beside the rest of the outcome. Appended.
  pub degraded_elided: u32,
}

/// A landing's stage (§4.15 "State machines"): the four terminal states a finished landing reports and the stages
/// before them. Append-only.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum LandingStage {
  /// Its manifest is being planned.
  Planning,
  /// Presented, awaiting an approval.
  AwaitingGrant,
  /// Its entries are being validated against the disk.
  Validating,
  /// Its entries are being written.
  Writing,
  /// Its written entries are being made durable.
  Syncing,
  /// The volume is advancing past what landed.
  Advancing,
  /// Every entry landed.
  Done,
  /// Some entries were undone; the rest landed.
  Partial,
  /// Validation refused it; nothing was written.
  Refused,
  /// Cancelled or crashed; a resume continues it.
  Aborted,
}

/// The durability a landing achieved (§4.15 step 8).
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct LandingDurability {
  /// Every written file was written out, and behind its batch's barrier, before its link or exchange.
  pub data_synced: bool,
  /// Every directory the landing touched was synced.
  pub dirs_synced: bool,
  /// The media barrier ran and held.
  pub media: bool,
  /// Whether the grant asked for media durability: without it, barriers only is the supported level.
  pub media_requested: bool,
  /// Directories synced.
  pub dirs: u64,
}

/// A host's answer as a landing reports it: its typed refusals, or the errno it gave.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostAnswer {
  /// The entry does not exist (any more).
  NotFound,
  /// Not a directory where one was needed.
  NotDirectory,
  /// Not a file where one was needed.
  NotFile,
  /// A handle the host no longer holds.
  StaleHandle,
  /// The host's errno (Linux, macOS), never a Win32 code (A-539: `Win32`).
  Errno {
    /// The errno.
    errno: i32,
  },
  /// The host's Win32 error code (Windows), in Win32's own numbering: never an errno, whatever its number (A-539).
  /// `Errno` carries an errno alone. Appended.
  Win32 {
    /// The Win32 error code.
    code: u32,
  },
}

/// A Degraded cell a landing met (§4.15 failure matrix), as the reply carries it.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum LandingDegradation {
  /// The filesystem has no atomic exchange; the fallback's widest window, nanoseconds, a name was absent.
  NoExchange {
    /// The widest window, nanoseconds.
    widest_window_ns: u64,
  },
  /// Media durability was not asked for: barriers only, the supported level.
  BarriersOnly,
  /// The host stopped answering (a crash-like errno; a Win32 code is `CrashedWin32`, A-539); the landing aborted and
  /// advanced nothing.
  Crashed {
    /// The errno.
    errno: i32,
  },
  /// A touched directory could not be opened or synced; the entries in it stay in the overlay.
  Unsynced {
    /// The directory, by its path from the target.
    dir: String,
    /// What the host answered.
    answer: HostAnswer,
  },
  /// The media barrier asked for failed; every entry stays in the overlay.
  MediaUnsynced {
    /// What the host answered.
    answer: HostAnswer,
  },
  /// An entry of the landing's own could not be removed and stays under its name.
  Leftover {
    /// Its path from the target.
    path: String,
    /// What the host answered.
    answer: HostAnswer,
  },
  /// A directory the sweep could not list; an earlier attempt's siblings in it, if any, stay.
  Unswept {
    /// The directory, by its path from the target.
    dir: String,
    /// What the host answered.
    answer: HostAnswer,
  },
  /// An entry moved aside that could not go back is kept beside its name, never removed.
  Kept {
    /// The manifest path of the entry it was displaced by.
    path: String,
    /// The path it is kept at.
    kept: String,
  },
  /// The host stopped answering with a Win32 error code (Windows), in Win32's own numbering (A-539); the landing
  /// aborted and advanced nothing. `Crashed` carries an errno alone. Appended.
  CrashedWin32 {
    /// The Win32 error code.
    code: u32,
  },
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
  /// The state name (`issued`, `consumed`, `expired`, `revoked`), for people.
  pub state: String,
  /// The state, typed (A-299, 2026-11-18: until then a consumer read `state`'s name). Appended.
  pub grant_state: GrantState,
}

/// Where a grant stands (§4.15 "State machines"). Append-only.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantState {
  /// Issued and not yet used up.
  Issued,
  /// A single-use grant's landing finished.
  Consumed,
  /// Its term passed, or its session ended.
  Expired,
  /// Revoked.
  Revoked,
}

/// Where a `changed_since` walk stands (§4.16, A-264): the green version it is in and how many of that version's
/// changed paths it has passed. A committed version's paths never change, so the cursor stays valid as the green moves.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ChangedCursor {
  /// The version.
  pub version: u64,
  /// The entries of `version` passed.
  pub passed: u32,
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
  /// The terminal state, for a finished landing, for people.
  pub outcome: Option<String>,
  /// The record's kind, typed (A-299, 2026-11-18: until then a consumer read `kind`'s name). Appended.
  pub audit_kind: AuditKind,
  /// The terminal state, typed, for a finished landing. Appended with `audit_kind`.
  pub stage: Option<LandingStage>,
  /// How the landing's signature was judged (signed work, A-321), on a record of the landing as a whole: its
  /// presentation's and a granted run's `LandingPlanned`, and its `LandingFinished` (`Unsigned` when it carried
  /// none). `None` on every other record. Appended.
  pub verification: Option<Verification>,
}

/// What an audit record records (§4.15). Append-only.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditKind {
  /// A grant was issued (by the launcher's approval, or by a bypass policy).
  GrantIssued,
  /// A grant was revoked.
  GrantRevoked,
  /// A landing was presented.
  LandingPlanned,
  /// A landing's entries were validated against the disk.
  LandingValidated,
  /// An entry was written.
  EntryWritten,
  /// An entry was refused.
  EntryRefused,
  /// A landing finished.
  LandingFinished,
  /// A grant reached its term unused and ended (A-266), recorded under the principal it was made for. Appended.
  GrantExpired,
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
  /// The base directory is unavailable: the host's errno, 0 where it named none (a Win32 code is
  /// `BaseUnavailableWin32`, A-539).
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
  /// retried (the client's credit bounds what it can have in flight). A listing, the grants or a status, gathered from
  /// every shard, answers it too when a shard's part could not be sent there or came home unserved, that shard's state
  /// refusing to serve it (fenced, or never started): the answer is never given without that shard's part (A-557).
  Overloaded {
    /// The shard whose queue is full, or whose part a gathered answer lacks.
    shard: u16,
  },
  /// A malformed request.
  BadRequest {
    /// What was wrong.
    reason: String,
  },
  /// The verb ran but its record could not be made durable in anchor-owned RAM, so its effects and
  /// its completion record were rolled back together — nothing of it exists, in memory or in the
  /// segment (§4.8 transactions, AC-2.3; AUD-06). The request was not completed and may be retried
  /// under the same id (it re-executes; it is never answered from memory). `reason` names what
  /// stopped the publication (the log full and its snapshot unpublishable, the segment refusing).
  Unpublished {
    /// What stopped the publication.
    reason: String,
  },
  /// The landing target directory cannot be opened, or escapes containment (§4.15 step 4). Superseded by
  /// [`Refusal::Target`], which types the reason (A-299, 2026-11-18): no daemon of this build sends it; kept for the
  /// wire's append-only rule.
  TargetUnavailable {
    /// What was wrong.
    reason: String,
  },
  /// The landing hit conflicts; nothing was written (§4.15 step 4).
  LandingConflict {
    /// The conflicting entries: as many as one reply holds beside the count (§4.3; A-267).
    entries: Vec<String>,
    /// Conflicting entries past what one reply holds (A-267), never sent: presenting the landing again lists every
    /// one (`GrantRequired`, then `LandingEntries`).
    entries_elided: u32,
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
  /// The owner's authority over the object's **latest state** is not confirmed right now (§4.8 "Leases
  /// and reads"; AUD-08): within the lease bound, too few of the object's candidate holders answered this
  /// node's probes reporting it alive under the configuration version it has installed — the node is cut
  /// off, was paused past the bound, or has learned of a newer configuration than it holds (it may have
  /// been retired and its objects taken over). A read at the head, the head version, a status, an
  /// enumeration and every write of live state refuse; an explicitly pinned immutable read (a green's
  /// named version, an attachment's pinned view) is served. Retry after the bound, or through the id's
  /// current owner. Appended for append-only evolution.
  LeaseUnconfirmed {
    /// The configuration version the refusing node has installed.
    version: u64,
  },
  /// The snapshot's barrier could not close: an attachment of the volume still had a request in
  /// flight — a consumer lost mid-request (§4.4 A-9, §4.6 "A failed participant gives a typed
  /// incomplete barrier, not a clean snapshot"). Retry once the owner's failed-consumer cleanup has
  /// drained it. Appended for append-only evolution.
  BarrierIncomplete {
    /// The attachment that could not close (the catalog id of a mount; a device's registry key).
    attachment: u64,
    /// The generation that could not close.
    generation: u64,
  },
  /// The owner shard's landings awaiting a grant are at their bound: the landing was not presented — no id
  /// was allocated and nothing was recorded. Retry once one is granted and landed or its client has gone
  /// (AUD-29-07). Appended for append-only evolution.
  LandingsAwaitingFull,
  /// A granted landing's target lease ended before the landing could start — its term passed while it
  /// waited — so nothing was written; retry (AUD-29-03). Appended for append-only evolution.
  LandingLeaseLost,
  /// The client holds as many unacknowledged completion records on this shard as its bound allows (§4.9
  /// "Exactly-once": a record is kept until the client acknowledges it): the request was not run and nothing was
  /// recorded. Acknowledge what the client has received, retry any id it still owes a retry, then send it again.
  /// A retry of an id already completed and an `Acknowledge` are always served. Appended for append-only
  /// evolution.
  AcknowledgementOwed {
    /// The completion records the client holds unacknowledged on the refusing shard.
    retained: u64,
    /// The bound they reached.
    bound: u64,
  },
  /// No owner of a volume homed in this region's fleet answered a forwarded verb within the takeover bound (the
  /// operator's failover SLO): the forward was sent again while the owner was not confirmed dead, and none came back
  /// (§4.8 "Lookup"). Retryable with the same request id: a successor serves once a takeover installs it. Until
  /// 2026-10-09 this case was answered `HomedElsewhere`, which names another region and sends the caller there.
  OwnerUnreachable {
    /// The region whose fleet the volume's owner was sought in.
    region: u64,
  },
  /// A `MapRead` (§4.6, A-171) could not be served as a direct map — a different-uid, non-plain or non-head request,
  /// an overlay, a client served by another shard, a platform with no route yet, or a copy the daemon's budget or the
  /// OS refused — so the client reads the file by its normal paged read instead. The bytes are identical; only the
  /// copy per read is paid, and every reply is one page (`ReadPage`). Appended.
  MapUnavailable,
  /// An image could not be built or merged (§4.6, A-160): the image format refused the tree or the image (a name or a
  /// time it cannot hold, a capacity too small, a corrupt or hostile image), or the image holds what a volume cannot
  /// (a name or a symlink target not UTF-8, a device). The reason is the refusal in words, since the protocol carries
  /// no dependency on the image format's types. Appended.
  ImageRefused {
    /// The reason, in words.
    reason: String,
  },
  /// A `MapRead` would take this client past its share of the shard's direct maps (§4.6, A-171): its count, derived
  /// from the process's descriptor and mapping limits, or its bytes, derived from the shard's fill budget. Nothing was
  /// made. The client releases a map (`ReleaseMap`) or reads by the paged path. Appended.
  MapShareExhausted {
    /// The maps the client holds.
    held_maps: u64,
    /// The most it may hold.
    most_maps: u64,
    /// The bytes its maps hold.
    held_bytes: u64,
    /// The most bytes they may hold.
    share_bytes: u64,
  },
  /// A `ShardsHolder` met a stream whose attach has not yet recorded its own holder on the stream's shard (A-181): the
  /// attach queues that record there as it replies, so a request sent at once can overtake it. Nothing was recorded,
  /// and the request may be sent again; the client does so within its reconnect budget. Appended.
  HolderPending,
  /// A `Detach` met writable direct maps made under the attachment and not yet released (§4.6, A-186): their stores
  /// reach the file only through the attachment, at a `MapSync`, a `ReleaseMap` or their client's end, so the
  /// attachment stays until each is released, as a file system with files open refuses to unmount. Nothing changed.
  /// Appended.
  MapsHeld {
    /// The writable maps held under the attachment.
    maps: u64,
  },
  /// A client already holds as many attachments of other hosts' volumes as its node keeps for it (§4.10, A-222):
  /// the node remembers each so its reap can end them at their owners, and that memory is bounded. Nothing was
  /// attached. Appended.
  RemoteAttachmentsFull {
    /// The bound.
    bound: u64,
  },
  /// A host path the daemon was given lies on a mount it serves itself (§4.6: no daemon thread opens a path on a volume
  /// the daemon serves; A-227): opening it would send the daemon a request only it can answer. Nothing was opened.
  /// Appended.
  OwnMount {
    /// The path as given.
    path: String,
    /// What the daemon was asked to use it as.
    used_as: OwnMountUse,
  },
  /// A verb named a CLI attachment whose term lapsed (§4.7, A-230): it ended then, its write lease with it, and is
  /// named here rather than as unknown, so a user who paused past the term learns why. Attach again. Appended.
  AttachmentExpired {
    /// The attachment.
    attachment: u64,
  },
  /// A client named an id its session key does not prove (§4.9; `slates_server::session_key`): the seat and the
  /// completion records under that id belong to the client it was issued to, so nothing is served on this channel,
  /// which the daemon then ends. A client resumes only the session it was handed (`Session`, with its key). Appended.
  SessionUnproven,
  /// A durable table of the partition the verb records into is at its derived capacity (§4.8; CLAUDE.md banned item
  /// 8): `green_chains`, the merge byte budget a partition's green chains, origins, checkpoints and work logs share,
  /// when what live readers still hold below their greens' checkpoints fills it. Nothing was recorded. Until 2026-11-06
  /// (A-238) it reached the client as `BadRequest { reason: "<table> at capacity" }`, as if the request were
  /// malformed. Appended.
  TableAtCapacity {
    /// The table.
    table: String,
  },
  /// The volume is in this node's catalog, and listed, but recovery could not rebuild it after the daemon's restart
  /// (§4.8): every verb on it answers this, naming why, rather than `NotFound`. Appended.
  NotRecovered {
    /// Why recovery could not rebuild it.
    cause: RecoveryCause,
  },
  /// A request forwarded from another host for a volume nothing has shared with that host's account (A-237: a uid
  /// means something only on its host). `principal` is the account as the owner judged it, its host included: the
  /// share that would let it through names exactly this (`slates share ID uid:N@ANCHOR`).
  NotShared {
    /// The verb.
    verb: String,
    /// The requester, by its host's certificate anchor and its account.
    principal: Principal,
  },
  /// The write policy forbids this act (A-285): the mode the user gave the agent forbids writing. `by` names the
  /// subject whose policy decided. Appended.
  PolicyForbids {
    /// The act refused.
    act: WriteAct,
    /// The subject whose policy forbids it.
    by: PolicySubject,
  },
  /// A `SetPolicy` named a policy version the policy has moved past (A-285), or a `SetRepositoryPolicy` a repository
  /// policy version it has moved past (A-622): a raced or replayed setting changes nothing. Read the policy again and set
  /// it from there. Appended.
  PolicyChanged {
    /// The policy's version now.
    current: u64,
  },
  /// The merge's work or green requires signed merges (A-321), and the merge's signature was absent or did not verify:
  /// nothing was merged. A requirement on what the volume accepts, met in every write mode when the merge would happen
  /// (a bypass merge at once, an asked one at its approval). Appended.
  SignatureRequired {
    /// The volume whose policy requires signed merges.
    by: PolicySubject,
    /// How the merge's signature was judged.
    verification: Verification,
  },
  /// The volume core refused the verb for a reason the variants before this one do not name (A-299): until
  /// 2026-11-18 each reached a client as `BadRequest` carrying the reason's name, which a consumer could only read as
  /// words. Appended.
  Volume {
    /// Why, typed.
    refusal: VolumeRefusal,
  },
  /// The daemon's own records could not serve the verb, for a reason the variants before this one do not name (A-299):
  /// until 2026-11-18 each reached a client as `BadRequest` carrying the database error's words. Appended.
  Database {
    /// Why, typed.
    refusal: DatabaseRefusal,
  },
  /// A target the verb needs cannot be used: a landing's directory, a mount point, or the mount the daemon made there
  /// (§4.15 step 4, §4.6). Until 2026-11-18 its reason was words (`TargetUnavailable`). Appended.
  Target {
    /// Why, typed.
    refusal: TargetRefusal,
  },
  /// The verb ran and its effects are committed on its owner, but they were not held where its reply waits for them
  /// (a live-shipped volume's log on the owner and `f` candidates, A-200; a destroy's tombstone placed at `f + 1`,
  /// AUD-29-43) within the failover SLO from the hold: the total bound of the rule every transport judges a held reply
  /// by (`slates_bridge_core::durable_bound`), the operator's bound on how long an object may go unserved before a
  /// takeover serves it, past which a write not yet held is past what the fleet promises (A-354). Through a shorter
  /// silence the reply still waits, never a false success. The verb's effects stay committed on the owner and nothing
  /// claims them durable; this is the request's recorded outcome (§4.9), so a retry under the same id meets it, and the
  /// verb runs again only under a new id. Until 2026-11-18 such a reply was held for good when what it waited for could
  /// not happen. A consensus operation (a bootstrap, a recovery) answers it too when it ran on the control shard but
  /// a shard of the daemon did not apply its publication within that shard's own time (A-566). Appended.
  DurabilityPending {
    /// How long the reply was held before it was told this, nanoseconds.
    held_ns: u64,
  },
  /// A `Conflicts` read named a verdict the work no longer keeps (A-267): a later submit or rebase of the work replaced
  /// it, or the daemon that judged it restarted. Submit or rebase again for a new verdict. Appended.
  VerdictReplaced {
    /// The work volume.
    work: VolumeId,
  },
  /// A planned move of a volume to another fleet member (`Move`, §4.10 "Ownership follows the writer", A-291) was
  /// refused, for a reason a caller can act on by its kind. Nothing moved, and the volume is served where it was.
  /// Appended.
  MoveRefused {
    /// Why.
    reason: MoveRefusal,
  },
  /// The bytes an `ArchiveImport` read are not an archive this daemon can make a volume from (A-453), refused by kind
  /// before the volume is published: nothing is made, its name stays free and its room goes back. Appended.
  ArchiveRefused {
    /// Why.
    fault: ArchiveFault,
  },
  /// The base directory is unavailable: the host's Win32 error code (Windows), in Win32's own numbering (A-539).
  /// `BaseUnavailable` carries an errno alone. Appended.
  BaseUnavailableWin32 {
    /// The path.
    path: String,
    /// The Win32 error code.
    code: u32,
  },
}

/// Why the bytes an `ArchiveImport` read make no volume ([`Refusal::ArchiveRefused`], A-453): each the class of the
/// archive reader's refusals (`slates_archive::ArchiveError`) a caller acts on alike. Append-only.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveFault {
  /// The stream does not begin with an archive's magic: another format's file, or no file at all.
  ForeignMagic,
  /// An archive of a format this daemon does not read: a newer major, a required flag it does not know, or a name
  /// policy it does not hold. A newer daemon reads it.
  Unsupported,
  /// The stream ends before its own frame does: cut off where it was kept or copied. Copy it again whole.
  Truncated,
  /// The bytes are not those the archive's checksum or identities name: altered where it was kept or copied.
  Corrupt,
  /// Whole and matching its checksum, the archive breaks the format's rules: a declared length past its cap, a chunk
  /// out of canonical form, a total, extent, name or path that does not hold, or a tree that does not restore. A
  /// faulty or hostile writer made it.
  Malformed,
}

/// Why a target cannot be used ([`Refusal::Target`], A-299): each the class of a landing target's, a mount point's or a
/// mount's refusal. The path is the caller's own, so none carries it. Append-only.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetRefusal {
  /// The path is not absolute.
  NotAbsolute,
  /// A component escapes the landing's target directory.
  Escapes,
  /// The entry does not exist.
  Missing,
  /// Not a directory where one is needed.
  NotDirectory,
  /// Not a file where one is needed.
  NotFile,
  /// A handle the host no longer holds.
  Stale,
  /// Owned by another user than the one the verb runs for: the owner's uid, where the host named it.
  OwnedByAnother {
    /// The owner's uid.
    owner: Option<u32>,
  },
  /// The host refused the path: its errno (a Win32 code is `HostWin32`, A-539).
  Host {
    /// The errno.
    errno: i32,
  },
  /// The kernel's mount table names no mount of this daemon's there.
  NotMounted,
  /// This daemon already records as many of its own mounts as it can: the bound.
  RecordsFull {
    /// The most mounts the daemon records.
    capacity: u64,
  },
  /// This daemon has no tag to name its own mounts by, so it cannot record one (A-227).
  NoOwnMountTag,
  /// The OS's mount helper or kernel refused the mount: the OS error number, when one was given (the helper's own
  /// words, and how it exited, are in the daemon's log).
  MountRefused {
    /// The OS error number.
    errno: Option<i32>,
  },
  /// The mount point is not a drive letter (`S:`).
  NotADriveLetter,
  /// The anchor's hold on the drive did not answer within its bound (A-206): the mount, or its release, waits on it.
  AnchorHold,
  /// A detach of this drive already waits for the anchor's release.
  DetachPending,
  /// This machine does not let WinFsp's driver make a Mount Manager drive (A-369), which a WinFsp drive must be for
  /// Windows to resolve its files' final paths: an Administrator allows it once, with `MountUseMountmgrFromFSD` set to
  /// 1 under WinFsp's key, as WinFsp itself is installed once.
  MountManagerNotAllowed,
  /// The drive letter names another volume: the Mount Manager gave it to another, or a definition local to the
  /// daemon's session hides the machine's (A-369).
  DriveLetterTaken,
  /// The path is longer than the daemon takes where it must name it (A-266): a landing target past what a grant's
  /// summary holds on one grants page, refused before anything is presented. Appended.
  TooLong {
    /// The longest path the daemon takes there, in bytes.
    longest: u64,
  },
  /// The host refused the path: its Win32 error code (Windows), in Win32's own numbering (A-539). `Host` carries an
  /// errno alone. Appended.
  HostWin32 {
    /// The Win32 error code.
    code: u32,
  },
}

/// What the daemon's database refused, where the refusals before [`Refusal::Database`] name nothing closer (A-299):
/// each the class of a database error (`slates_db::DbError`) by the same name. Append-only.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatabaseRefusal {
  /// A record in the log did not verify (a torn tail, or a hostile byte): the sequence expected there.
  Corrupt {
    /// The sequence expected.
    seq: u64,
  },
  /// An NFSv4 instance advance named an instance at or below the current one (§4.6 A-37).
  NfsInstanceRegressed {
    /// The partition's current instance.
    current: u32,
  },
  /// A green's checkpoint at a version it cannot stand for (W3; §4.16): not past the checkpoint the green holds, or
  /// past its head.
  CheckpointOutOfRange {
    /// The version the checkpoint named.
    version: u64,
    /// The version of the checkpoint the green holds (0: none).
    held: u64,
    /// The green's head.
    head: u64,
  },
  /// A sealing key record of other than its fixed length (A-92).
  SealKeyMalformed {
    /// The record's length.
    len: u64,
  },
  /// A record's encoding was refused.
  Encoding,
  /// An entry of a repository's register made for a position other than the register's next, or naming a version of a
  /// green other than the one after its head (§4.17.1 shared piece 2, A-603). Superseded by [`DatabaseRefusal::Moved`]
  /// with A-603's repository-wide position (A-642): kept on the wire, never sent. Appended.
  OutOfOrder {
    /// The position or version the entry named.
    named: u64,
    /// The one the register holds next.
    expected: u64,
  },
  /// An entry's check does not hold (A-603, A-642): what it checks changed since it was read (a name under a prefix, a
  /// state log's position), as Mercurial's `check:heads` fails when a head was added. Appended.
  CheckFailed {
    /// The check's index among the entry's checks.
    index: u32,
  },
  /// An entry's change of its repository's own state was made against another state (A-603): a phase root it adds is
  /// one already, one it removes is not, or a marker it adds is held. Appended.
  StateChanged {
    /// The change's index among the entry's state changes.
    index: u32,
  },
  /// A repository policy's rule its repository cannot hold (A-622): its pattern names what the repository's tool does
  /// not have, is empty, holds a NUL, or is a Subversion path not from the root. Appended.
  PolicyRuleMalformed {
    /// The rule's index among the policy's rules.
    index: u32,
  },
  /// A repository policy's hook its repository cannot hold (A-622): at another tool's point, named other than its point
  /// allows or named twice there, or running a program that is not an absolute path. Appended.
  PolicyHookMalformed {
    /// The hook's index among the policy's hooks.
    index: u32,
  },
  /// A green an entry moves or checks is not at the position the push read (§4.17.1 shared piece 2, A-642): it moved
  /// since, so the entry is refused whole, naming the green. Appended.
  Moved {
    /// The green.
    green: VolumeId,
    /// The position the push read.
    read: u64,
    /// The position the green is at.
    at: u64,
  },
  /// A repository policy's hooks-env section its repository cannot hold (A-648): in a repository that is not
  /// Subversion's, named by nothing or holding a NUL, or with a variable no hook's environment can carry (a name empty
  /// or holding `=` or a NUL, a value holding a NUL). Appended.
  PolicyHooksEnvMalformed {
    /// The section's index among the policy's hooks-env sections.
    index: u32,
  },
}

/// What the volume core refused, where the refusals before [`Refusal::Volume`] name nothing closer (A-299): each the
/// class of a volume-core error (`slates_vfs::VfsError`) by the same name. Append-only.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolumeRefusal {
  /// `ENOTDIR`: not a directory where one was needed.
  NotDirectory,
  /// `EISDIR`: a directory where a file was needed.
  IsDirectory,
  /// `ENOTEMPTY`: a directory that must be empty is not.
  NotEmpty,
  /// `EINVAL`: an argument the operation does not take.
  Invalid,
  /// `ENODATA`: no such extended attribute.
  NoAttribute,
  /// `EPERM`: the volume's rules forbid the operation.
  NotPermitted,
  /// A link protected against the operation (a sticky directory's, a hard link's).
  LinkProtected,
  /// An operation a special file (a FIFO, a socket name) does not support.
  SpecialFileOperation,
  /// `EMLINK`: a file at its link bound.
  TooManyLinks,
  /// `EFBIG`: past a file's largest size.
  FileTooLarge,
  /// `EXDEV`: a move across volumes.
  CrossVolumeMove,
  /// A handle or number that no longer names anything (a freed inode, a destroyed snapshot).
  StaleHandle,
  /// The snapshot is pinned by a live clone, or the volume is a green's origin: destroy the clone first.
  Pinned,
  /// A guest maps the file in a way the change cannot keep its map equal to, so the change waits for the map's end.
  DaxMapped,
  /// A witnessed base entry changed on the disk beneath an unpinned range: the read refuses rather than mix states.
  BaseDrift,
  /// The verb names an overlay's base, and the volume has none.
  NotOverlay,
  /// The digest cache is at its bound.
  DigestCacheFull,
  /// A memory refusal the variants before [`Refusal::Volume`] do not name.
  Memory,
  /// A journal reader asked for records retention has dropped.
  JournalIncomplete {
    /// The record it needed.
    needed: u64,
    /// The oldest record retained.
    oldest: u64,
  },
  /// A recovery image of another format version.
  RecoveryVersion {
    /// The version found.
    found: u16,
    /// The version this build reads.
    expected: u16,
  },
  /// What a snapshot pins could not be kept (§4.2 `RetentionBudgetExceeded`): no unpromised capacity is left for a
  /// retained version or bytes, or the volume is at its retention allowance. Not the volume's quota: destroying a
  /// snapshot frees it. Appended (2026-11-18; until then it reached a client as `NoSpace`).
  RetentionExceeded,
}

/// Why a planned move was refused ([`Refusal::MoveRefused`], A-291): each names what the caller can change. Append-only.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveRefusal {
  /// The volume is not a plain live volume: an overlay (its base stays on its source host, §4.10 "Overlay volumes in a
  /// fleet"), an image, a green or a work, or one being destroyed.
  NotPlain,
  /// The volume has an attachment or a lease: a writer detaches first.
  Attached,
  /// The volume has clones, or is a green's origin: each depends on it on its owner. (A clone itself moves: its tree is
  /// archived whole and served standalone by its new owner, A-291.)
  Lineage,
  /// The volume has snapshots other than its head: a move carries the head, and the others would be lost with the old
  /// owner's copy. Destroy them first.
  Snapshots,
  /// The target is not another member of the volume's region.
  Target,
  /// The target is outside the owner's neighbourhood (it holds no key to the volume's sealed content), or its own
  /// neighbourhood does not cover the cohort the volume's records sit in (its takeover could not recover the volume).
  Neighbourhood,
  /// The volume's records, or its head's content on hosts other than its owner (the move's handoff), were not placed
  /// within the fleet's failover SLO, so the move was not reported; the volume thawed. Ask again.
  Unplaced,
  /// The volume is already moving to another member.
  Pending,
}

/// Why recovery could not rebuild a volume its catalog holds ([`Refusal::NotRecovered`], §4.8). Append-only.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum RecoveryCause {
  /// The content image the anchor kept does not hold the volume.
  NoImage,
  /// The image's blocks could not be claimed in this daemon's arena.
  BlocksUnclaimed,
  /// The overlay's base directory is gone.
  BaseMissing {
    /// The base, as recorded.
    path: String,
  },
  /// The overlay's base lies on a mount this daemon serves (§4.6, A-227), so it was not opened.
  BaseOnOwnMount {
    /// The base, as recorded.
    path: String,
  },
  /// The host refused the overlay's base directory.
  BaseUnavailable {
    /// The base, as recorded.
    path: String,
    /// The host's refusal, as the base plane names it.
    reason: String,
  },
  /// A clone's origin volume was not recovered.
  OriginNotRecovered,
  /// The volume core refused the image as it was rebuilt.
  ImageRefused {
    /// The volume core's refusal.
    reason: String,
  },
  /// The volume's acknowledged resources do not fit this process: a byte budget, the lock capacity, the version slab,
  /// the metadata ledger or the volume table.
  ResourcesUnavailable {
    /// Which, and the refusal.
    reason: String,
  },
  /// A volume or green this node took over from a departed owner (§4.8, §4.16 owner-loss recovery) could not be adopted
  /// or materialized within the takeover's bound: the failover SLO's worth of the fleet's periods without progress, each
  /// one attempt. Every verb on it answers this rather than waiting without end. Appended.
  TakeoverIncomplete {
    /// What the last attempt waited on: `promises` (a recovery cohort never gave its quorum of promises in the takeover
    /// round, A-304), `catalog` (its catalog register or that register's placement never adopted), `catalog_placement`,
    /// `content` (a volume's content not held here, and not served whole by a recorded holder), `live` (a live-shipped
    /// volume's log), `shard` (no shard here for its partition), `chain` (a version's record not known here: the adopted
    /// version beyond this node's accepted records), `inputs` (an input its ledger names not held here, and not served
    /// whole by the survivors that reported the adopted version), `placement` (no placement of the adopted record, or no
    /// shard for its partition), or `refused` (the owner shard refused the rebuilt volume or green).
    waited_on: String,
  },
}

impl RecoveryCause {
  /// The cause's kind, its variant's name, as every listing names it (`volume list`, its JSON, the MCP surface and
  /// the SDKs, one definition): the whole cause, with its path or reason, is in the volume's status.
  #[must_use]
  pub fn kind(&self) -> &'static str {
    match self {
      RecoveryCause::NoImage => "NoImage",
      RecoveryCause::BlocksUnclaimed => "BlocksUnclaimed",
      RecoveryCause::BaseMissing { .. } => "BaseMissing",
      RecoveryCause::BaseOnOwnMount { .. } => "BaseOnOwnMount",
      RecoveryCause::BaseUnavailable { .. } => "BaseUnavailable",
      RecoveryCause::OriginNotRecovered => "OriginNotRecovered",
      RecoveryCause::ImageRefused { .. } => "ImageRefused",
      RecoveryCause::ResourcesUnavailable { .. } => "ResourcesUnavailable",
      RecoveryCause::TakeoverIncomplete { .. } => "TakeoverIncomplete",
    }
  }
}

/// What a path refused [`Refusal::OwnMount`] was given as (A-227). Append-only.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnMountUse {
  /// A volume's base directory, or an entry below it.
  Base,
  /// A granted landing's target.
  LandingTarget,
  /// A FUSE mount point.
  MountPoint,
}

/// The writes a snapshot is known to include (§4.6 "Writeback and snapshot barrier"): every write the
/// owner had **recorded** when the barrier closed the volume's attachments — the ring's, and a mount
/// transport's that reached the daemon. What the reply cannot claim is said: a kernel client that
/// buffers writes (the NFS client's unstable writes before its `COMMIT`, a guest's page cache) may hold
/// some its process was told succeeded, so a snapshot over such a mount reports the server-visible
/// boundary rather than a complete one. Application buffers a process never submitted are outside
/// either.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotBoundary {
  /// Every write any consumer was told succeeded is in the snapshot: no attachment of the volume can
  /// hold a write the daemon has not recorded (ring clients, write-through transports).
  Complete,
  /// The writes the daemon had recorded: at least one attachment rides a transport whose kernel may
  /// buffer acknowledged writes it has not sent yet.
  ServerVisible,
}

/// The barrier's account of a snapshot (§4.6 "Writeback and snapshot barrier"; GAP-A9-4).
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotCoverage {
  /// The boundary the snapshot can claim.
  pub boundary: SnapshotBoundary,
  /// The live attachments of the volume whose generation the barrier closed — every request admitted
  /// before it belongs to the snapshot's generation, every request after to the next.
  pub attachments_closed: u32,
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
    /// What the snapshot covers (§4.6 "Writeback and snapshot barrier"; GAP-A9-4). Appended.
    coverage: SnapshotCoverage,
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
  /// One page of the paths changed since a version (`ChangedSince`).
  ChangedSince {
    /// The changed paths this page answers.
    paths: Vec<String>,
    /// Where the next page continues, or `None` when the walk is done. A version and a count, never a path, so a page
    /// holding one long path need not hold it twice.
    next: Option<ChangedCursor>,
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
    /// The conflict windows to rebase against, when not accepted: as many as one reply holds (§4.3; A-267).
    conflicts: Vec<MergeWindow>,
    /// Conflict windows past what one reply holds (A-267): the work keeps its verdict, and `Conflicts` reads every
    /// window of it a page at a time.
    conflicts_elided: u32,
    /// The verdict the windows are of, kept with the work for `Conflicts`; 0 when there is none.
    verdict: u64,
  },
  /// A work volume was rebased onto its green's head (§4.16). The green is unchanged.
  Rebased {
    /// The head the work is now based on, when every operation mapped cleanly; `None` on conflict.
    version: Option<u64>,
    /// The conflict windows to resolve, when the rebase did not complete: as many as one reply holds (A-267).
    conflicts: Vec<MergeWindow>,
    /// Conflict windows past what one reply holds (A-267), read with `Conflicts`.
    conflicts_elided: u32,
    /// The verdict the windows are of, kept with the work for `Conflicts`; 0 when there is none.
    verdict: u64,
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
    /// The attachment's **mount capability token** (§4.6, §4.13; AUD-01): a secret returned to the
    /// authorized consumer for an attachment that establishes a host mount, presented at the NFS mount so
    /// the loopback edge authorizes the connection as this consumer with the attachment's rights. `None`
    /// for an attachment that establishes no host mount (an SDK record form, a green pin). Appended for
    /// append-only evolution.
    token: Option<[u8; 16]>,
  },
  /// Detached.
  Detached,
  /// Resized.
  Resized,
  /// Destroyed.
  Destroyed,
  /// The status.
  Status {
    /// The report. Boxed: a cold, wide report that would otherwise size every reply's move.
    report: Box<StatusReport>,
  },
  /// One page of the listing (`List`), in volume-id order.
  Listed {
    /// The volumes of this page.
    volumes: Vec<VolumeSummary>,
    /// The cursor that continues the listing (the last volume of this page), or `None` when this page ends it.
    next: Option<VolumeId>,
  },
  /// Acknowledged.
  Acknowledged,
  /// One page of a base entry's bytes (`ReadBase`, A-267).
  BaseBytes {
    /// The bytes from the request's offset: as many as one reply holds, fewer at the file's end.
    bytes: Vec<u8>,
    /// The file's size when this page was read.
    total: u64,
    /// The file's fingerprint when this page was read, folded to a word: a file changed or replaced between two pages
    /// stamps them differently, and a reader joining pages refuses the mix.
    stamp: u64,
  },
  /// Re-witnessed.
  Rewitnessed {
    /// The paths re-witnessed: as many as one reply holds beside the count (§4.3; A-267).
    paths: Vec<String>,
    /// Paths re-witnessed past what one reply holds (A-267), never sent: the rewitness took them all.
    paths_elided: u32,
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
    /// Conflicts found before any write (empty when the landing is clean), each `PATH (CLASS)`: as many as one reply
    /// holds beside the rest of it (§4.3; A-267).
    conflicts: Vec<String>,
    /// The exact statement a signature over this landing signs (signed work, A-321;
    /// `slates_server::signed_work::landing_statement`: the volume, the target as the request named it and this
    /// manifest, one field a line): sign it under the namespace `slates-work` and land with the signature.
    statement: Vec<u8>,
    /// How the request's signature was judged against this manifest (A-321): `Unsigned` when it carried none.
    verification: Verification,
    /// Conflicts past what one reply holds (A-267), never sent here: every entry of the presentation, its conflict
    /// among it, is read a page at a time (`LandingEntries`).
    conflicts_elided: u32,
  },
  /// A landing finished (or partially).
  Landed {
    /// The outcome.
    outcome: LandingOutcome,
  },
  /// One page of the caller's grants (`Grants`), in grant-id order.
  Grants {
    /// The grants of this page.
    grants: Vec<GrantSummary>,
    /// The cursor that continues the walk (the last grant of this page), or `None` when this page ends it.
    next: Option<u64>,
  },
  /// One page of the audit (`Audit`, A-265): records in sequence order, as many as one reply chunk holds (§4.3).
  Audit {
    /// The records of this page.
    records: Vec<AuditEntry>,
    /// Whether more follow: the next page is asked from one past the last record's sequence.
    more: bool,
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
    /// The paths the move invalidated — those some version in the span changed, sorted: as many as one reply holds
    /// beside the rest of it (§4.3; A-267).
    invalidated: Vec<String>,
    /// Invalidated paths past what one reply holds (A-267), never sent: more changed than the reply carries, so a
    /// holder of the view's reads treats every path as invalidated (a superset is always safe), or reads a green's
    /// changes a page at a time (`ChangedSince`).
    invalidated_elided: u32,
  },
  /// Retired with `RequestBody::Read` (A-267): never sent; kept only so the tags after it hold (D-15,
  /// `tests/wire_order.rs`).
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
  /// One bounded page of an immutable daemon status report (§4.14).
  DaemonStatusPage {
    /// The request identity that captured the report on this client channel.
    snapshot: u64,
    /// The first encoded byte in this page.
    offset: u64,
    /// The complete encoded length, bounded by the client's reply credit.
    total: u64,
    /// Schema-checked report bytes, never more than one reply slot holds.
    bytes: Vec<u8>,
  },
  /// A host mount's attachment was bound to its mount point (`BindMount`). Appended.
  MountBound,
  /// A staging buffer was reserved (`StageBegin`). Appended.
  Staged {
    /// The buffer's token, unique on its owner shard.
    token: u64,
  },
  /// Bytes were written into a staging buffer (`StagePut`). Appended.
  StagePutDone {
    /// How many of the buffer's bytes are now written.
    filled: u64,
  },
  /// One page of a file (`ReadRange`). Appended.
  ReadPage {
    /// The bytes from the requested offset.
    bytes: Vec<u8>,
    /// The file's whole length at this view.
    total: u64,
    /// A stamp that changes whenever the file's bytes at this view may have changed: a reader that sees it move
    /// between pages starts again.
    stamp: u64,
  },
  /// One page of a directory's entries (`ReadDir`). Appended.
  DirPage {
    /// The entries, in the view's order.
    entries: Vec<DirEntry>,
    /// The cursor that continues the listing, or `None` when this page ends it.
    next: Option<u64>,
  },
  /// A file verb (`FsWrite`, `FsWriteStaged`, `FsRemove`, `FsRename`, `FsMkdir`) changed the volume. Appended.
  FsDone {
    /// The changed object's size after the verb (a written file's length; 0 for a removal or a directory).
    size: u64,
  },
  /// A same-kernel client's direct map was handed (§4.6, A-171): its metadata. The descriptor of the daemon's copy of
  /// the file arrives over the control socket before this reply; the client `mmap`s all `block_len` bytes of it from
  /// `region_off` (zero) and reads the file's bytes there, kept equal to the file within that length. Appended.
  Mapped {
    /// The daemon-minted mapping id, which the delivery carries too.
    mapping_id: u64,
    /// Where the file starts in the object: zero, since the object is the file's copy.
    region_off: u64,
    /// The file's length when it was handed.
    file_len: u64,
    /// The object's whole length: the file's page span when it was mapped.
    block_len: u64,
  },
  /// A snapshot's image was built (`ImageBuild`). Appended.
  ImageBuilt {
    /// The image volume.
    id: VolumeId,
    /// The image file's length in bytes.
    image_bytes: u64,
    /// The file system's blocks in use.
    blocks_used: u64,
    /// Its inodes in use.
    inodes_used: u64,
  },
  /// An image was merged into a new clone of its snapshot (`ImageMerge`). Appended.
  ImageMerged {
    /// The clone.
    id: VolumeId,
    /// What the merge did.
    report: ImageMergeReport,
  },
  /// A Shards stream was attached and its end is offered on the daemon's socket route (§4.6, A-161; macOS, where no
  /// control socket carries a descriptor): the client claims it once, with `secret`, within the offer's life. The
  /// secret authorizes that one claim and nothing else. Appended.
  ShardsOffered {
    /// The attachment the daemon serves on the stream.
    attachment: u64,
    /// The lease epoch, for a write intent.
    lease_epoch: Option<u64>,
    /// The transport's report for this attachment.
    capability: AttachmentCapability,
    /// The single-use secret the claim's exchange is keyed from (256 bits).
    secret: [u8; 32],
  },
  /// A direct map was let go (`ReleaseMap`). Appended.
  MapReleased,
  /// A same-kernel client's direct map is offered on the daemon's socket route (§4.6, A-173; macOS, where no control
  /// socket carries a descriptor): the client claims the read-only descriptor of the daemon's copy once, with `secret`,
  /// within the offer's life, then maps all `block_len` bytes of it. The secret authorizes that one claim and nothing
  /// else. Appended.
  MapOffered {
    /// The daemon-minted mapping id: the offer's id on the route, and what `ReleaseMap` names.
    mapping_id: u64,
    /// The file's length when it was handed.
    file_len: u64,
    /// The object's whole length: the file's page span when it was mapped.
    block_len: u64,
    /// The single-use secret the claim's exchange is keyed from (256 bits).
    secret: [u8; 32],
  },
  /// A same-kernel client's direct map was handed by handle (§4.6, A-175; Windows, where no socket carries one): the
  /// daemon duplicated a read-only handle of its copy into the client's process, after judging that it authorizes
  /// reading the copy alone; `handle` is its value there, which the client adopts once and maps all `block_len` bytes
  /// of. Recorded with `handle` zero, since the client's process outlives the daemon that duplicated it: a reply
  /// replayed from the record after a restart names no handle. Appended.
  MapHanded {
    /// The daemon-minted mapping id, what `ReleaseMap` names.
    mapping_id: u64,
    /// The file's length when it was handed.
    file_len: u64,
    /// The object's whole length: the file's page span when it was mapped.
    block_len: u64,
    /// The handle's value in the client's process; zero in the record.
    handle: u64,
  },
  /// A Shards stream was attached and its end handed by handle (§4.6, A-161, A-179; Windows, where no socket carries
  /// one): the daemon duplicated the client end of the attachment's pipe into the client's process, and `handle` is its
  /// value there, which the client adopts once. Zero names no handle: the attach is recorded with zero, filled in only
  /// as the reply goes to the client, so a reply replayed from the record after a restart names none; and a hand-off
  /// refused leaves it zero, its end closed, so the attachment ends. Appended.
  ShardsHanded {
    /// The attachment the daemon serves on the stream.
    attachment: u64,
    /// The lease epoch, for a write intent.
    lease_epoch: Option<u64>,
    /// The transport's report for this attachment.
    capability: AttachmentCapability,
    /// The handle's value in the client's process; zero in the record, or where the hand-off was refused.
    handle: u64,
    /// The value in the client's process of the device's end of the stream's resync channel (A-427), a message-mode
    /// pipe's: zero where the daemon made none, in the record, or where its hand-off was refused. Appended.
    resync: u64,
  },
  /// The process a `ShardsHolder` named was judged to hold the stream and is now a known holder of it. Appended.
  ShardsHolderAdded,
  /// A writable direct map's stores were written into its file and published (`MapSync`, §4.6 A-186). Appended.
  MapSynced {
    /// The bytes written: those of the runs where the copy differed from the file.
    written: u64,
    /// The file's length when the comparison began: the copy's bytes past it within its last page are not the file's.
    file_len: u64,
  },
  /// A green's head was checked out into the volume it was created from (`Checkout`, §4.16 A-201). Appended.
  CheckedOut {
    /// The green version the volume now holds: the head when the checkout began.
    version: u64,
    /// What the checkout changed in the clone of the base, by kind.
    report: CheckoutReport,
  },
  /// A volume's live shipping was set (`LiveShip`, A-200). Appended.
  LiveShippingSet {
    /// Whether it is live-shipped now.
    live_shipped: bool,
  },
  /// One page of a presented landing's entries (`LandingEntries`): the bytes of their encodings, one [`PlannedEntry`]
  /// after another in the manifest's order, from the request's cursor to as many as one reply holds, so an entry may
  /// begin on one page and end on a later one. Appended.
  LandingEntries {
    /// The bytes.
    bytes: Vec<u8>,
    /// Where the next page starts, or `None` when this page ends the list.
    next: Option<EntriesCursor>,
    /// The entries the manifest holds.
    total: u64,
    /// The manifest's hash, the one its `GrantRequired` named, so a reader joining pages knows each is of one manifest.
    manifest: [u8; 32],
  },
  /// A write policy was set (A-285). Appended.
  PolicySet,
  /// A write policy (A-285): `None` when none is set, which is ask. Appended.
  Policy {
    /// The policy.
    policy: Option<WritePolicy>,
    /// The policy's version: how many times it or the subject's signers were set, which a `SetPolicy` and a
    /// `SetSigners` name.
    version: u64,
    /// The subject's trusted signers (A-321), in SSH wire form, in the order set; none for a consumer, whose policy
    /// keeps none.
    signers: Vec<Vec<u8>>,
  },
  /// A merge an ask policy holds for approval (A-285): nothing was merged. `identity` names the increment exactly
  /// (`blake3` of the work, its base, its post-state and its ops document, §4.16), so an approval
  /// (`SubmitApproved`) binds it; `paths` counts the paths it changes. Appended.
  MergeApprovalRequired {
    /// The work volume.
    work: VolumeId,
    /// The increment's identity.
    identity: [u8; 32],
    /// The paths the increment changes.
    paths: u64,
    /// The exact statement a signature over this merge signs (signed work, A-321; `slates_server::signed_work::
    /// merge_statement`: the work, its green, the base and the identity, one field a line): sign it under the namespace
    /// `slates-work` and submit with the signature (`SubmitSigned`, or `SubmitApproved` carrying it).
    statement: Vec<u8>,
    /// How the held submit's signature was judged (A-321): `Unsigned` when it carried none.
    verification: Verification,
  },
  /// A signed submit's verdict (A-321): the committed green version, or the windows of a conflict, as `Submitted`
  /// answers them, and how the submit's signature was judged — recorded with the version when it was accepted.
  /// Appended.
  SubmittedSigned {
    /// The committed green version, when the increment was accepted; `None` on conflict.
    version: Option<u64>,
    /// The conflict windows to rebase against, when not accepted: the first one reply holds, as `Submitted` carries
    /// them (A-267).
    conflicts: Vec<MergeWindow>,
    /// How the signature was judged.
    verification: Verification,
    /// The verdict's windows the reply did not carry, which `Conflicts` reads under `verdict` (A-267).
    conflicts_elided: u32,
    /// The conflicting verdict the work keeps, by which `Conflicts` reads it; 0 when none is kept (A-267).
    verdict: u64,
  },
  /// A volume's trusted signers were set (A-321). Appended.
  SignersSet,
  /// A green version's merge record (A-321): how the signature of the merge that made it was judged (`Unsigned` for
  /// version 0, the origin, and for a merge that carried none). Appended.
  MergeRecord {
    /// The version.
    version: u64,
    /// How the merge's signature was judged.
    verification: Verification,
  },
  /// One page of a volume's drifted entries (`Drifted`), typed as a status's `base` types them. Appended.
  Drifted {
    /// This page's entries the disk changed after the volume witnessed them, in inode order.
    drifted: Vec<DriftEntry>,
    /// This page's entries whose re-check the host refused now, by path, in inode order.
    unverified: Vec<String>,
    /// The inode the next page starts after, or `None` when this page ends the walk. A page may hold no entry and
    /// still carry one: it spent its slice of the shard's step re-checking entries that had not drifted.
    next: Option<u64>,
    /// Entries in this page's range too long for any page alone (a host path near Linux's `PATH_MAX`), passed over
    /// and counted, never sent as a reply no chunk holds.
    oversized: u32,
  },
  /// One page of a work's kept verdict (`Conflicts`, A-267): its windows in the verdict's order. Appended.
  Conflicts {
    /// The windows of this page.
    windows: Vec<MergeWindow>,
    /// The position the next page reads from, or `None` when this page ends the verdict.
    next: Option<u32>,
  },
  /// A CLI attachment's term was renewed (A-384): the term now in force from this renewal, and how soon its holder renews
  /// again. Appended.
  Renewed {
    /// The term, nanoseconds: the attachment lapses this long after its last use.
    term_ns: u64,
    /// How soon the holder renews again, nanoseconds: a third of the shorter of the term and the daemon's liveness
    /// window, so two renewals lost or late are survived, and a cadence that does not follow the term it shapes (one a
    /// third of the term fed its own gaps back into the term, which grew to the failover delay: 1.0 s to 4.0 s in ten
    /// renewals on the Mac Studio, 2026-11-18).
    renew_every_ns: u64,
  },
  /// A volume's move is under way (`Move`, A-291): it is frozen, and the move waits for its records to be placed or for
  /// the configuration to name its new owner. Ask again. Appended.
  Moving {
    /// The member it moves to.
    to: u64,
  },
  /// The configuration names a volume's new owner (`Move`, A-291): verbs on it go there now. Appended.
  Moved {
    /// The member it moved to.
    to: u64,
  },
  /// An overlay's base was captured (`CaptureBase`, A-291): `snapshot` holds its whole tree, recorded captured, so a
  /// clone of it needs no base directory. Appended.
  BaseCaptured {
    /// The snapshot.
    snapshot: SnapshotId,
  },
  /// A snapshot was exported as an archive (`ArchiveExport`, A-441). Appended.
  ArchiveExported {
    /// The archive volume, whose root holds [`ARCHIVE_FILE`].
    id: VolumeId,
    /// The archive file's length in bytes.
    archive_bytes: u64,
    /// The archive's manifest identity: the BLAKE3 Merkle root of the snapshot's tree (D-17), the same for the same
    /// tree wherever it is exported.
    manifest: [u8; 32],
  },
  /// A volume was made from an archive (`ArchiveImport`, A-453). Appended.
  ArchiveImported {
    /// The new volume.
    id: VolumeId,
    /// The manifest identity of the archive it was made from, verified against the tree: the exported snapshot's.
    manifest: [u8; 32],
  },
  /// A version-control credential was enrolled (`EnrollCredential`, A-625 part 3). Appended.
  CredentialEnrolled {
    /// Its id: the SHA-256 of a key's blob or of a token.
    id: [u8; 32],
  },
  /// A version-control credential was revoked (`RevokeCredential`, A-625 part 3). Appended.
  CredentialRevoked,
}

/// What a checkout changed in its green's base volume (§4.16 A-201): the head's difference from the origin, by
/// kind. A path counts once, under the first kind that applied to it.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct CheckoutReport {
  /// Files written whole: new, or holding other bytes than the origin's.
  pub written: u64,
  /// Directories made.
  pub made: u64,
  /// Paths removed: files and directories the origin held and the head does not.
  pub removed: u64,
  /// Paths whose mode alone changed.
  pub moded: u64,
}

/// One entry of a directory listing (`ReadDir`).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
  /// The entry's name within its directory.
  pub name: String,
  /// What it is.
  pub kind: EntryKind,
  /// A file's length in bytes; zero for anything else.
  pub size: u64,
}

/// What a directory entry is.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
  /// A regular file.
  File,
  /// A directory.
  Dir,
  /// A symbolic link.
  Symlink,
  /// A FIFO, a socket or a device node.
  Other,
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
  let half = region.bulk_len().checked_div(2).unwrap_or(0);
  let chunk = half.checked_div(slots).unwrap_or(0);
  let position = index
    .checked_rem(u64::try_from(slots).unwrap_or(1))
    .and_then(|position| usize::try_from(position).ok())
    .unwrap_or(0);
  let base = match direction {
    Direction::Request => 0,
    Direction::Reply => half,
  };
  (base.saturating_add(position.saturating_mul(chunk)), chunk)
}

/// The bytes one framed message may take in either direction: one bulk chunk of `region` (every slot's chunk is
/// the same size). A verb whose message would not fit is paged by its caller (`ReadRange`, `StagePut`).
pub fn chunk_capacity(region: &ClientRegion) -> usize {
  let (_, capacity) = chunk(region, Direction::Request, 0);
  capacity
}

/// Whether `message`, framed as [`pack`] frames it, fits one message of `region`.
pub fn fits<M: Wire>(region: &ClientRegion, message: &M) -> bool {
  let body = frame(message);
  body.len() <= PAYLOAD_BYTES || body.len() <= chunk_capacity(region)
}

/// The framed size of `message`, as [`pack`] would write it.
pub fn framed_len<M: Wire>(message: &M) -> usize {
  frame(message).len()
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
  region.write_bulk(offset, &body)?;
  let mut payload = [0u8; BULK_REF_BYTES];
  payload[..8].copy_from_slice(&u64::try_from(offset).unwrap_or(u64::MAX).to_le_bytes());
  payload[8..].copy_from_slice(&u64::try_from(body.len()).unwrap_or(u64::MAX).to_le_bytes());
  Ok(Slot {
    kind: SlotKind::Bulk,
    request,
    payload: payload.to_vec(),
    stamp: 0,
  })
}

/// The framed body a bulk slot's `payload` names in `region`'s bulk area, copied out; a reference of the wrong size or
/// outside the area is a typed refusal.
fn bulk_body(region: &ClientRegion, payload: &[u8]) -> Result<Vec<u8>, IpcError> {
  if payload.len() != BULK_REF_BYTES {
    return Err(IpcError::BadSlot {
      reason: "bulk reference of the wrong size",
    });
  }
  let (offset, len) = payload
    .split_at_checked(size_of::<u64>())
    .unwrap_or_default();
  let word = |bytes: &[u8]| {
    usize::try_from(u64::from_le_bytes(bytes.try_into().unwrap_or([0; 8]))).unwrap_or(usize::MAX)
  };
  let (offset, len) = (word(offset), word(len));
  if offset
    .checked_add(len)
    .is_none_or(|end| end > region.bulk_len())
  {
    return Err(IpcError::BadSlot {
      reason: "bulk reference outside the area",
    });
  }
  let mut body = vec![0u8; len];
  region.read_bulk(offset, &mut body)?;
  Ok(body)
}

/// A slot taken from a ring, its body owned: a bulk slot's body copied out of the bulk area and carried inline, as
/// [`unpack`] reads an inline one, so the ring may release the slot (and with it the chunk) at once. Any other slot is
/// returned as it is. Called by each end's take before it releases the slot (`Ring::peek`, `Ring::release`): the chunk
/// is the next message's at that ring position, which the producer writes as soon as the slot is released, and a body
/// read after that was the next message's (`a_request_taken_from_the_ring_keeps_its_bulk_body_when_the_client_sends_on`).
pub fn owned(region: &ClientRegion, slot: Slot) -> Result<Slot, IpcError> {
  if slot.kind != SlotKind::Bulk {
    return Ok(slot);
  }
  Ok(Slot {
    kind: SlotKind::Inline,
    request: slot.request,
    payload: bulk_body(region, &slot.payload)?,
    stamp: slot.stamp,
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
    SlotKind::Bulk => unframe(&bulk_body(region, payload)?),
    SlotKind::Cancel | SlotKind::Heartbeat => Err(IpcError::BadSlot {
      reason: "not a message",
    }),
  }
}

#[cfg(test)]
#[allow(clippy::string_slice)] // Test code may panic (CLAUDE.md §2 item 6 applies to shipped code).
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

  /// The signals measured since boot whose absence is unknown: the replay time and the NFS service times.
  fn since_boot_or_unknown(signal: HealthSignal) -> bool {
    matches!(
      signal,
      HealthSignal::LogReplayNs
        | HealthSignal::NfsLocalP50Ns
        | HealthSignal::NfsLocalP99Ns
        | HealthSignal::NfsLocalOffCpuP99Ns
        | HealthSignal::NfsForwardedP99Ns
    )
  }

  /// Every health signal states its freshness basis (§4.14): `log.replay_ns` is a boot-time fact aged
  /// since boot, and the NFS service times accumulate since boot; every other signal is computed at the
  /// report. Pinned so a new signal must decide how it ages rather than inherit age zero.
  #[test]
  fn every_signal_states_its_freshness_basis() {
    for signal in HealthSignal::ALL {
      let expected = if since_boot_or_unknown(signal) {
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
      "nfs.local_p50_ns",
      "nfs.local_p99_ns",
      "nfs.local_off_cpu_p99_ns",
      "nfs.forwarded_p99_ns",
      "shard.watch_window_ns",
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
  /// 0 ns replay), as is an NFS service time before any call was served; every other signal is an
  /// always-computable count whose absence would be a `Degraded` producer. Pinned so a new signal must decide its
  /// absence meaning, not inherit a look-alike zero.
  #[test]
  fn every_signal_types_its_absence() {
    for signal in HealthSignal::ALL {
      let expected = if since_boot_or_unknown(signal) {
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
