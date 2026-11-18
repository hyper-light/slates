//! The consumer contract's closed error taxonomy (§4.12 "The consumer contract", A-299). Every failure of every call —
//! each refusal the daemon sends (§4.4, §4.8, §4.13, §4.15, §4.16), each failure of the channel to it (§4.7) and each
//! failure in this process (a mount, a start, a path) — is exactly one [`Error`], and its variant is the class of
//! thing a consumer does about it:
//!
//! | Class | What a consumer does |
//! |---|---|
//! | [`Error::NotFound`] | treat the subject as gone; [`Missing`] names it |
//! | [`Error::AlreadyExists`] | use what exists, or choose another name |
//! | [`Error::Denied`] | ask whoever holds the right (a person, the launcher); retrying changes nothing |
//! | [`Error::Exhausted`] | free something, or wait for room; the bound is in the detail |
//! | [`Error::Busy`] | wait for the holder, or end it |
//! | [`Error::Stale`] | read the current state and ask again |
//! | [`Error::Retry`] | ask again the same way after the pause the error carries |
//! | [`Error::Unavailable`] | start the instance, or report that it is down |
//! | [`Error::Unsupported`] | take another route; this machine or daemon does not offer it |
//! | [`Error::Invalid`] | fix the request |
//! | [`Error::Unusable`] | the subject's state forbids it (destroying, not recovered, no group) |
//! | [`Error::Elsewhere`] | send it where the detail says |
//! | [`Error::Host`] | the host's file system or OS refused; the detail carries its error number |
//! | [`Error::Mount`] | a host mount could not be made or removed |
//! | [`Error::Start`] | an instance could not be started or stopped |
//! | [`Error::Protocol`] | a fault in slates itself: report it |
//!
//! **Closed, and typed all the way down.** No enum here is `#[non_exhaustive]`: a consumer may match every variant, and
//! a variant added to the contract is a change of [`crate::CONTRACT`], which a consumer pinning the contract sees at
//! its next vendoring as a compile error where it matched exhaustively (§4.12 "Versioning"). No variant carries text a
//! consumer must read to decide: where the daemon or the OS said something in words only (a malformed request's
//! reason, a host's message), the words travel as a [`Note`], which is displayed and never matched.
//!
//! **Retrying is the error's to say.** [`Error::Retry`] carries the derived pause before the first retry (the client's
//! own pacing, `Client::retry_pause_ns`, starting at the daemon's published spin window); [`crate::Instance::retry_pause_ns`]
//! gives each later one. [`Error::is_retryable`] is true for it and for [`Error::Unavailable`], whose instance may come
//! back.

use std::fmt;

use slates_client::ClientError;
use slates_ipc::error::RouteRefusal;
use slates_ipc::protocol::{
  AttachTransport, HostPathReason, MoveRefusal, OwnMountUse, Principal, RecoveryCause, Refusal,
  SnapshotId, UnsupportedReason, VolumeId,
};
use slates_ipc::{DeliveryFault, IpcError};

use crate::policy::PolicyRefusal;

/// Words for a person, carried beside a typed variant: what the daemon or the OS said in prose. A consumer decides on
/// the variant; a note is only shown, so its one accessor is [`fmt::Display`].
#[derive(Clone, PartialEq, Eq)]
pub struct Note(String);

impl Note {
  /// A note of `text`.
  pub(crate) fn new(text: impl Into<String>) -> Note {
    Note(text.into())
  }
}

impl fmt::Display for Note {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&self.0)
  }
}

impl fmt::Debug for Note {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{:?}", self.0)
  }
}

/// A typed failure of one of the contract's calls (see the module doc for what each class asks of a consumer). `Eq`
/// is not derived: [`Unusable::DurabilityUnmet`] carries measured probabilities.
#[derive(Clone, Debug, PartialEq)]
pub enum Error {
  /// What the call named does not exist (any more). Nothing changed.
  NotFound(Missing),
  /// What the call would make exists already. Nothing changed.
  AlreadyExists(Existing),
  /// The caller may not do this. Nothing changed.
  Denied(Denied),
  /// A bound is reached. Nothing changed.
  Exhausted(Exhausted),
  /// Another holder has what the call needs. Nothing changed.
  Busy(Busy),
  /// The call named a state that has moved on. Nothing changed.
  Stale(Stale),
  /// Refused for now: the same call may succeed after the pause. Nothing changed, or the daemon kept it so a retry under
  /// the same request id meets its record (§4.9).
  Retry {
    /// Why.
    cause: RetryCause,
    /// The derived pause before asking again, in nanoseconds.
    pause_ns: u64,
  },
  /// No daemon answers at the instance.
  Unavailable {
    /// The instance asked for.
    instance: String,
    /// What the client found.
    cause: Absence,
  },
  /// This machine, platform or daemon does not offer what was asked.
  Unsupported(Unsupported),
  /// The request itself is wrong.
  Invalid(Invalid),
  /// The subject's state forbids the call.
  Unusable(Unusable),
  /// The call must be sent elsewhere.
  Elsewhere(Elsewhere),
  /// The host's file system or OS refused.
  Host(HostFault),
  /// A host mount could not be made or removed (§4.6).
  Mount(MountFault),
  /// An instance could not be started, or its anchor stopped ([`crate::start`]).
  Start(StartFault),
  /// A fault in slates itself: the client and the daemon disagree about the protocol. Report it.
  Protocol(ProtocolFault),
}

/// What a call named that does not exist (any more).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Missing {
  /// The volume.
  Volume(VolumeId),
  /// The volume, or this snapshot of it.
  Snapshot {
    /// The volume.
    volume: VolumeId,
    /// The snapshot.
    snapshot: SnapshotId,
  },
  /// A presentation awaiting approval: it was replaced, landed, or its client went away (A-46). Present it again.
  Presentation {
    /// The presentation's id.
    id: u64,
  },
  /// An attachment: it ended (a detach, the volume's destroy, its holder's end).
  Attachment {
    /// The attachment.
    attachment: u64,
  },
  /// An entry of a volume.
  Entry {
    /// The volume.
    volume: VolumeId,
    /// The entry, from the volume's root.
    path: String,
  },
  /// An enrolled consumer.
  Consumer {
    /// The consumer.
    consumer: u64,
  },
  /// A shard of the daemon.
  Shard {
    /// The shard.
    shard: u16,
  },
}

/// What exists already.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Existing {
  /// A volume holds the name; its id.
  Volume {
    /// The volume holding the name.
    existing: VolumeId,
  },
  /// The instance's consensus group exists (§4.8 `bootstrap root`).
  Group,
}

/// Why the caller may not do this.
#[derive(Clone, Debug, PartialEq)]
pub enum Denied {
  /// The caller's rights on the volume do not include the verb (§4.13 "Access lists").
  Rights {
    /// The verb, as the daemon named it.
    verb: Note,
  },
  /// A request forwarded from another host for a volume nothing has shared with that host's account (A-237).
  NotShared {
    /// The requester as the owner judged it: the share that would let it through names exactly this.
    principal: Principal,
  },
  /// The daemon requires an enrolled consumer for the verb, and the caller is not one (§4.13 "Principals").
  NotEnrolled,
  /// The caller's consumer enrollment was revoked.
  Revoked,
  /// The client named a session its key does not prove (§4.9).
  SessionUnproven,
  /// A grant asked on a channel that cannot carry one (the ring, MCP; R10).
  GrantChannel {
    /// The channel, as the daemon named it.
    channel: Note,
  },
  /// The authority's proof did not verify (§4.13 "Grants").
  Unproven,
  /// This process holds no issuer authority, or the one it was given does not attach.
  NoAuthority(IssuerFault),
  /// The process answering at the instance is not the daemon's user (§4.13): never trusted.
  PeerNotDaemonUser {
    /// The peer's uid (a hash of its SID on Windows).
    uid: u32,
  },
  /// The consumer capability a harness delivers on an inherited descriptor could not be taken (§4.13).
  CapabilityNotDelivered(DeliveryFault),
  /// A write policy refused the act (A-299): the daemon's refusal where a call other than a landing or a merge meets
  /// it. A landing and a merge answer it as their own outcome ([`crate::LandOutcome::Refused`],
  /// [`crate::MergeOutcome::Refused`]), since in a forbidding mode it is the expected answer.
  Policy(PolicyRefusal),
}

/// Which bound is reached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Exhausted {
  /// The volume's quota.
  Quota,
  /// The shard's reserve cannot cover the reservation; the bytes it has.
  Memory {
    /// Bytes available.
    available: u64,
  },
  /// The daemon's or a shard's clients (§4.7 admission); the bound, when the channel named it.
  Clients {
    /// The bound, when known.
    limit: Option<u64>,
  },
  /// The owner shard's presentations awaiting approval: retry once one is approved or its client has gone.
  Presentations,
  /// A durable table of the partition is at its derived capacity (§4.8); its name.
  Table {
    /// The table, as the daemon named it.
    table: Note,
  },
  /// The client's share of the shard's direct maps (§4.6, A-171).
  Maps {
    /// The maps the client holds.
    held_maps: u64,
    /// The most it may hold.
    most_maps: u64,
    /// The bytes its maps hold.
    held_bytes: u64,
    /// The most bytes they may hold.
    share_bytes: u64,
  },
  /// The attachments of other hosts' volumes a node keeps for one client (§4.10, A-222).
  RemoteAttachments {
    /// The bound.
    bound: u64,
  },
  /// This client has issued its last request sequence: make a new instance (AUD-29-21).
  Sequences {
    /// The exhausted client id.
    client: u32,
  },
  /// As many operations are outstanding as the client admits: take or abandon one (AUD-29-22).
  Outstanding {
    /// The bound.
    limit: u64,
  },
}

/// Who holds what the call needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Busy {
  /// Another holder has the volume's write lease (D-16).
  WriteLease {
    /// The holder's epoch.
    epoch: u64,
  },
  /// Another landing holds the target's landing lease (§4.15, AC-2.9).
  LandingTarget {
    /// The holding landing attempt.
    holder: u64,
  },
  /// Writable direct maps made under the attachment are not yet released (§4.6, A-186).
  MapsHeld {
    /// The maps held.
    maps: u64,
  },
  /// The session's client id is held by a live client; the daemon assigned another.
  Session {
    /// The id assigned instead.
    assigned: u32,
  },
  /// A live process already holds the instance's rendezvous (A-176).
  Instance,
  /// The volume has an attachment or a lease, which a move of it waits for to end (A-291): detach, then ask again.
  Attached,
  /// The volume is already moving to another member (A-291): ask for that move, or wait for it to end.
  Moving,
}

/// What has moved on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stale {
  /// An older lease epoch: the current one.
  Lease {
    /// The current epoch.
    current: u64,
  },
  /// A merge record below the epoch a holder has seen (§4.16).
  Epoch {
    /// The epoch seen.
    current: u64,
  },
  /// A work's base version its green no longer has (§4.16): destroyed, another green's, or past the head.
  Base {
    /// The green.
    green: VolumeId,
    /// The version the work was based on.
    version: u64,
  },
  /// An attachment whose term lapsed (§4.7, A-230): it ended then. Attach again.
  Attachment {
    /// The attachment.
    attachment: u64,
  },
  /// An approval of another manifest than the one about to be written: present again.
  Approval,
  /// A grant missing, expired or revoked.
  Grant,
  /// The disk changed under these entries after the plan was made (§4.15 step 4): present again.
  Disk {
    /// The entries, from the target's root.
    paths: Vec<String>,
  },
  /// A bootstrap naming another daemon start.
  Bootstrap,
  /// The reviewed state or the daemon's authority changed before a recovery was authorized.
  Recovery,
}

/// Why a call is refused for now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetryCause {
  /// A shard's cross-shard queue is at its bound; nothing was started.
  Overloaded {
    /// The shard.
    shard: u16,
  },
  /// The verb's record could not be made durable; its effects were rolled back with it (§4.8; AUD-06).
  Unpublished(Note),
  /// The owner's authority over the latest state is not confirmed now (§4.8 "Leases and reads"; AUD-08).
  LeaseUnconfirmed {
    /// The configuration version the refusing node has installed.
    version: u64,
  },
  /// A snapshot's barrier could not close: an attachment still had a request in flight (§4.6).
  Barrier {
    /// The attachment.
    attachment: u64,
    /// The generation that could not close.
    generation: u64,
  },
  /// A granted landing's target lease ended before it started; nothing was written (AUD-29-03).
  LandingLeaseLost,
  /// The client holds as many unacknowledged completion records as its bound allows (§4.9).
  AcknowledgementOwed {
    /// The records held.
    retained: u64,
    /// The bound.
    bound: u64,
  },
  /// No owner of the volume answered a forwarded verb within the takeover bound (§4.8 "Lookup").
  OwnerUnreachable {
    /// The region searched.
    region: u64,
  },
  /// A merge's inputs are not held where its verdict must run yet (§4.16).
  ContentUnavailable,
  /// A file changed while it was read or digested: nothing stale is returned.
  ChangedWhileRead {
    /// The file, when the client named it.
    path: Option<String>,
  },
  /// A stream's holder record has not reached its shard yet (A-181).
  HolderPending,
  /// The command ring is full: nothing was sent.
  RingFull,
  /// The daemon left this channel, or a reconnected one is still being bound: nothing was sent.
  ChannelLost,
  /// The rendezvous's handshakes in progress are at their bound (A-187).
  Handshakes,
  /// A client id was assigned but its handoff failed (§4.7).
  HandoffLost,
  /// A wait ended at its deadline.
  Deadline,
  /// A move's records were not placed within the fleet's failover SLO, so it was not reported and the volume thawed
  /// (A-291): ask for the move again.
  MoveUnplaced,
}

/// Why no daemon answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Absence {
  /// No daemon serves the instance: its rendezvous is absent, or its claim went unanswered.
  NoDaemon(Note),
  /// The daemon went away and did not come back within the reconnect budget (§4.7 "Failure matrix").
  Gone {
    /// The budget that passed, in nanoseconds.
    after_ns: u64,
  },
  /// The client can no longer observe replies (its completion reader failed); make a new instance.
  CompletionLost(Note),
}

/// What is not offered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unsupported {
  /// A feature this daemon or platform does not offer, as the daemon named it.
  Feature(Note),
  /// An attachment form not offered here (§4.6 A-9), with what is missing.
  Attachment {
    /// The transport asked for.
    transport: AttachTransport,
    /// What is missing.
    reason: UnsupportedReason,
  },
  /// A direct map of a file (§4.6, A-171): read it by the paged read.
  DirectMap,
  /// A rendezvous form this platform does not offer yet.
  Rendezvous {
    /// The form.
    feature: &'static str,
  },
  /// A merge or a landing that carries a signature: the daemon's verifier for signed work is not in this build
  /// ([`crate::signed`]). Nothing was sent.
  SignedWork,
  /// A write-policy verb: the daemon's side of the write modes is not in this build ([`crate::policy`]). Nothing was
  /// sent.
  WritePolicies,
}

/// What is wrong with the request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Invalid {
  /// A name the daemon does not take.
  Name,
  /// A name policy the base does not allow (case-folding over a byte-exact folder, or the reverse).
  NamePolicy,
  /// A malformed request, with the daemon's words.
  Request(Note),
  /// A path that cannot be carried as it is.
  Path {
    /// The path, as text (lossily, when it was not Unicode).
    path: String,
    /// What is wrong with it.
    fault: PathFault,
  },
  /// A volume of the wrong kind for the verb (§4.16, §4.4).
  Kind(KindMismatch),
  /// The green requires evidence on every increment, and the merge carried none.
  EvidenceRequired,
  /// A green's base must be a complete immutable snapshot, and this one still serves entries from a host folder: pin the
  /// whole base and snapshot again.
  LiveBase,
  /// No clean digest exists for the entry: read and hash its bytes instead (§4.15).
  NoCleanDigest,
  /// A chosen host or container path that cannot be honoured (§4.4).
  ChosenPath(HostPathReason),
  /// An image the image format refused, with its words (§4.6, A-160).
  Image(Note),
  /// A host path that lies on a mount this daemon serves (A-227).
  OwnMount {
    /// The path.
    path: String,
    /// What it was given as.
    used_as: OwnMountUse,
  },
  /// A payload larger than the channel carries.
  TooLarge {
    /// The bytes offered.
    offered: u64,
    /// The bytes the channel carries.
    capacity: u64,
  },
  /// A process named to receive a handle does not hold the stream it must (Windows).
  NotAHolder {
    /// The process named.
    pid: u32,
  },
  /// A move's target cannot take the volume (A-291): name another member.
  MoveTarget(MoveTarget),
}

/// Why a move's target cannot take the volume ([`Invalid::MoveTarget`], A-291).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveTarget {
  /// It is not another member of the volume's region: `status` names the members.
  NotMember,
  /// It is outside the owner's neighbourhood (it holds no key to the volume's sealed content), or its neighbourhood does
  /// not cover the cohort the volume's records sit in.
  OutsideNeighbourhood,
}

/// A volume of the wrong kind for the verb.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KindMismatch {
  /// The verb would write a green, which only its merge task writes: merge through a work volume.
  ReadOnlyGreen,
  /// The verb names a green, and the volume is not one.
  NotGreen,
  /// The verb names a work volume, and the volume is not one.
  NotWork,
}

/// Why the subject cannot serve the call in its present state.
#[derive(Clone, Debug, PartialEq)]
pub enum Unusable {
  /// The volume is being destroyed.
  Destroying,
  /// The volume is archived.
  Archived,
  /// Recovery could not rebuild the volume after the daemon's restart; why (§4.8).
  NotRecovered {
    /// Why.
    cause: RecoveryCause,
  },
  /// The instance has no consensus group yet: make it once ([`crate::Instance::ensure_group`]) on an instance run alone,
  /// or wait for a fleet node to import its groups (§4.8).
  NoGroup,
  /// No complete group state is available to recover from, or its counters cannot advance safely.
  RecoveryUnavailable,
  /// The volume cannot move to another member in its present form (A-291).
  Unmovable(Unmovable),
  /// The fleet's configuration cannot hold the write to the operator's declared durability (§4.8 "Placement"); reads
  /// continue.
  DurabilityUnmet {
    /// The configuration's measured coincident-loss probability.
    coincident_loss: f64,
    /// The loss the operator's policy accepts.
    accepted_loss: f64,
    /// The hosts the policy assumes fail at once.
    coincident_failures: u64,
  },
}

/// Why a volume cannot move to another member in its present form ([`Unusable::Unmovable`], A-291).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unmovable {
  /// It is not a plain live volume: an overlay (its base stays on its source host), an image, a green or a work.
  NotPlain,
  /// It is a clone, has clones, or is a green's origin, each sharing its records on its owner.
  Lineage,
  /// It has snapshots other than its head: a move carries the head, and the others would be lost. Destroy them first.
  Snapshots,
}

/// Where to send the call instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Elsewhere {
  /// The volume is homed in another region (§4.8 "Lookup").
  Region {
    /// The region homing it.
    region: u64,
  },
  /// Only the root leader proposes this (§4.8, D-14): `status` names it.
  RootLeader,
}

/// What the host refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostFault {
  /// An overlay's base folder is unavailable.
  Base {
    /// The folder.
    path: String,
    /// The OS error number.
    errno: i32,
  },
  /// An overlay's base could not be read now: its drift is unknown (§4.15 "Drift").
  BaseUnreadable,
  /// A landing's target cannot be opened, or escapes containment (§4.15 step 4); the daemon's words.
  Target(Note),
  /// An OS call of the channel failed.
  Os {
    /// The call.
    call: &'static str,
    /// The OS error number, when the OS gave one.
    errno: Option<i32>,
  },
  /// The channel's shared memory refused, in its words.
  Memory(Note),
  /// The daemon's socket route refused a hand-off (§4.6; A-161, A-173).
  Route(RouteRefusal),
  /// A direct map's hand-off failed and its copy could not be given back (A-174): it is held until this client ends.
  HandOff(Note),
}

/// What slates got wrong: the client and the daemon disagree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProtocolFault {
  /// The daemon answered with a body the verb does not expect.
  UnexpectedReply {
    /// The verb.
    verb: &'static str,
  },
  /// A refusal the verb cannot meet (a `NotFound` for a call that names nothing that could be missing), as the daemon
  /// sent it.
  UnexpectedRefusal(Note),
  /// A stale retry of an acknowledged request.
  DuplicateRequest,
  /// A name the daemon sent that the contract does not know (a state, an action, a class): the client and the daemon
  /// are of different builds.
  UnknownName {
    /// What was named.
    field: &'static str,
  },
  /// The channel's own framing or sealing refused, in its words.
  Channel(Note),
}

impl Error {
  /// Whether asking again may succeed: a [`Error::Retry`] after its pause, or [`Error::Unavailable`] once the instance
  /// is back.
  pub fn is_retryable(&self) -> bool {
    matches!(self, Self::Retry { .. } | Self::Unavailable { .. })
  }

  /// Whether no daemon answers at the instance.
  pub fn is_unavailable(&self) -> bool {
    matches!(self, Self::Unavailable { .. })
  }

  /// The pause before the first retry, for a [`Error::Retry`].
  pub fn retry_pause_ns(&self) -> Option<u64> {
    match self {
      Self::Retry { pause_ns, .. } => Some(*pause_ns),
      _ => None,
    }
  }

  /// A client failure of a call to `instance` that named `subject`, typed; `pause_ns` is the pause a retryable one
  /// carries.
  pub(crate) fn of_client(
    instance: &str,
    error: ClientError,
    subject: Option<Missing>,
    pause_ns: u64,
  ) -> Error {
    match error {
      ClientError::Refused(refusal) => Error::of_refusal(refusal, subject, pause_ns),
      ClientError::Ipc(ipc) => Error::of_ipc(instance, ipc, pause_ns),
      ClientError::UnexpectedReply { verb } => {
        Error::Protocol(ProtocolFault::UnexpectedReply { verb })
      }
      ClientError::DaemonGone { after_ns } => Error::Unavailable {
        instance: instance.to_owned(),
        cause: Absence::Gone { after_ns },
      },
      ClientError::SessionTaken { assigned } => Error::Busy(Busy::Session { assigned }),
      ClientError::SequencesExhausted { client } => {
        Error::Exhausted(Exhausted::Sequences { client })
      }
      ClientError::TooManyOutstanding { limit } => Error::Exhausted(Exhausted::Outstanding {
        limit: u64::try_from(limit).unwrap_or(u64::MAX),
      }),
      ClientError::RingFull => retry(RetryCause::RingFull, pause_ns),
      ClientError::ChannelLost | ClientError::Rebinding => retry(RetryCause::ChannelLost, pause_ns),
      ClientError::CompletionLost { reason } => Error::Unavailable {
        instance: instance.to_owned(),
        cause: Absence::CompletionLost(Note::new(reason)),
      },
      ClientError::ChangedWhileRead { path } => {
        retry(RetryCause::ChangedWhileRead { path: Some(path) }, pause_ns)
      }
      error @ ClientError::HandOffUnreleased { .. } => {
        Error::Host(HostFault::HandOff(Note::new(error.to_string())))
      }
    }
  }

  /// A channel failure, typed.
  fn of_ipc(instance: &str, error: IpcError, pause_ns: u64) -> Error {
    match error {
      error @ IpcError::DaemonUnavailable { .. } => Error::Unavailable {
        instance: instance.to_owned(),
        cause: Absence::NoDaemon(Note::new(error.to_string())),
      },
      IpcError::PeerRefused { uid } => Error::Denied(Denied::PeerNotDaemonUser { uid }),
      IpcError::RingFull => retry(RetryCause::RingFull, pause_ns),
      IpcError::PayloadTooLarge { offered, capacity } => Error::Invalid(Invalid::TooLarge {
        offered: u64::try_from(offered).unwrap_or(u64::MAX),
        capacity: u64::try_from(capacity).unwrap_or(u64::MAX),
      }),
      IpcError::OsRefused { call, code } => Error::Host(HostFault::Os { call, errno: code }),
      IpcError::Memory(fault) => Error::Host(HostFault::Memory(Note::new(fault.to_string()))),
      IpcError::Unsupported { feature } => Error::Unsupported(Unsupported::Rendezvous { feature }),
      IpcError::DeadlineExceeded => retry(RetryCause::Deadline, pause_ns),
      IpcError::HandshakesFull { .. } => retry(RetryCause::Handshakes, pause_ns),
      IpcError::HandoffLost { .. } => retry(RetryCause::HandoffLost, pause_ns),
      IpcError::TooManyClients { limit } => Error::Exhausted(Exhausted::Clients {
        limit: Some(u64::try_from(limit).unwrap_or(u64::MAX)),
      }),
      IpcError::CapabilityNotDelivered { fault } => {
        Error::Denied(Denied::CapabilityNotDelivered(fault))
      }
      IpcError::RouteRefused { refusal } => Error::Host(HostFault::Route(refusal)),
      IpcError::InstanceHeld { .. } => Error::Busy(Busy::Instance),
      IpcError::NotAHolder { pid } => Error::Invalid(Invalid::NotAHolder { pid }),
      error @ (IpcError::BadSlot { .. }
      | IpcError::Layout { .. }
      | IpcError::UnconfinedDescriptor { .. }
      | IpcError::SealRefused { .. }) => {
        Error::Protocol(ProtocolFault::Channel(Note::new(error.to_string())))
      }
    }
  }

  /// A move's refusal (A-291), typed by what a consumer does about it: detach or wait ([`Busy`]), change the volume
  /// ([`Unusable::Unmovable`]), name another target ([`Invalid::MoveTarget`]), or ask again ([`RetryCause::MoveUnplaced`]).
  fn of_move_refusal(reason: MoveRefusal, pause_ns: u64) -> Error {
    match reason {
      MoveRefusal::Attached => Error::Busy(Busy::Attached),
      MoveRefusal::Pending => Error::Busy(Busy::Moving),
      MoveRefusal::NotPlain => Error::Unusable(Unusable::Unmovable(Unmovable::NotPlain)),
      MoveRefusal::Lineage => Error::Unusable(Unusable::Unmovable(Unmovable::Lineage)),
      MoveRefusal::Snapshots => Error::Unusable(Unusable::Unmovable(Unmovable::Snapshots)),
      MoveRefusal::Target => Error::Invalid(Invalid::MoveTarget(MoveTarget::NotMember)),
      MoveRefusal::Neighbourhood => {
        Error::Invalid(Invalid::MoveTarget(MoveTarget::OutsideNeighbourhood))
      }
      MoveRefusal::Unplaced => retry(RetryCause::MoveUnplaced, pause_ns),
    }
  }

  /// A daemon refusal of a call that named `subject`, typed: one exhaustive table over §4.4's taxonomy as the wire
  /// carries it, so a refusal added to the wire does not compile until it is given its class here.
  pub(crate) fn of_refusal(refusal: Refusal, subject: Option<Missing>, pause_ns: u64) -> Error {
    match refusal {
      Refusal::NotFound => subject.map_or_else(
        || Error::Protocol(ProtocolFault::UnexpectedRefusal(Note::new("NotFound"))),
        Error::NotFound,
      ),
      Refusal::AlreadyExists { existing } => Error::AlreadyExists(Existing::Volume { existing }),
      Refusal::ConsensusAlreadyInitialized => Error::AlreadyExists(Existing::Group),
      Refusal::StaleLease { current } => Error::Stale(Stale::Lease { current }),
      Refusal::StaleEpoch { current } => Error::Stale(Stale::Epoch { current }),
      Refusal::UnknownBase { green, version } => Error::Stale(Stale::Base { green, version }),
      Refusal::AttachmentExpired { attachment } => Error::Stale(Stale::Attachment { attachment }),
      Refusal::GrantMismatch => Error::Stale(Stale::Approval),
      Refusal::GrantInvalid => Error::Stale(Stale::Grant),
      Refusal::LandingConflict { entries } => Error::Stale(Stale::Disk { paths: entries }),
      Refusal::ConsensusBootstrapStale => Error::Stale(Stale::Bootstrap),
      Refusal::ConsensusRecoveryStale => Error::Stale(Stale::Recovery),
      Refusal::LeaseHeld { epoch } => Error::Busy(Busy::WriteLease { epoch }),
      Refusal::LandingLeaseHeld { holder } => Error::Busy(Busy::LandingTarget { holder }),
      Refusal::MapsHeld { maps } => Error::Busy(Busy::MapsHeld { maps }),
      Refusal::NoSpace => Error::Exhausted(Exhausted::Quota),
      Refusal::BudgetExceeded { available } => Error::Exhausted(Exhausted::Memory { available }),
      Refusal::TooManyClients => Error::Exhausted(Exhausted::Clients { limit: None }),
      Refusal::LandingsAwaitingFull => Error::Exhausted(Exhausted::Presentations),
      Refusal::TableAtCapacity { table } => Error::Exhausted(Exhausted::Table {
        table: Note::new(table),
      }),
      Refusal::MapShareExhausted {
        held_maps,
        most_maps,
        held_bytes,
        share_bytes,
      } => Error::Exhausted(Exhausted::Maps {
        held_maps,
        most_maps,
        held_bytes,
        share_bytes,
      }),
      Refusal::RemoteAttachmentsFull { bound } => {
        Error::Exhausted(Exhausted::RemoteAttachments { bound })
      }
      Refusal::Forbidden { verb } => Error::Denied(Denied::Rights {
        verb: Note::new(verb),
      }),
      Refusal::NotShared { principal, .. } => Error::Denied(Denied::NotShared { principal }),
      Refusal::MoveRefused { reason } => Error::of_move_refusal(reason, pause_ns),
      Refusal::PolicyForbids { act, by } => {
        Error::Denied(Denied::Policy(crate::policy::PolicyRefusal {
          act: match act {
            slates_ipc::protocol::WriteAct::Merge => crate::policy::WriteAct::Merge,
            slates_ipc::protocol::WriteAct::Land => crate::policy::WriteAct::Land,
          },
          by: match by {
            slates_ipc::protocol::PolicySubject::Consumer { consumer } => {
              crate::policy::PolicySubject::Consumer(consumer)
            }
            slates_ipc::protocol::PolicySubject::Volume { volume } => {
              crate::policy::PolicySubject::Volume(volume)
            }
          },
          reason: crate::policy::PolicyReason::Forbidden,
        }))
      }
      Refusal::ConsumerNotEnrolled => Error::Denied(Denied::NotEnrolled),
      Refusal::ConsumerRevoked => Error::Denied(Denied::Revoked),
      Refusal::SessionUnproven => Error::Denied(Denied::SessionUnproven),
      Refusal::GrantChannelRefused { channel } => Error::Denied(Denied::GrantChannel {
        channel: Note::new(channel),
      }),
      Refusal::GrantIssuerUnverified => Error::Denied(Denied::Unproven),
      Refusal::Overloaded { shard } => retry(RetryCause::Overloaded { shard }, pause_ns),
      Refusal::Unpublished { reason } => {
        retry(RetryCause::Unpublished(Note::new(reason)), pause_ns)
      }
      Refusal::LeaseUnconfirmed { version } => {
        retry(RetryCause::LeaseUnconfirmed { version }, pause_ns)
      }
      Refusal::BarrierIncomplete {
        attachment,
        generation,
      } => retry(
        RetryCause::Barrier {
          attachment,
          generation,
        },
        pause_ns,
      ),
      Refusal::LandingLeaseLost => retry(RetryCause::LandingLeaseLost, pause_ns),
      Refusal::AcknowledgementOwed { retained, bound } => retry(
        RetryCause::AcknowledgementOwed { retained, bound },
        pause_ns,
      ),
      Refusal::OwnerUnreachable { region } => {
        retry(RetryCause::OwnerUnreachable { region }, pause_ns)
      }
      Refusal::ContentUnavailable => retry(RetryCause::ContentUnavailable, pause_ns),
      Refusal::DigestUnverified => retry(RetryCause::ChangedWhileRead { path: None }, pause_ns),
      Refusal::HolderPending => retry(RetryCause::HolderPending, pause_ns),
      Refusal::Unsupported { feature } => {
        Error::Unsupported(Unsupported::Feature(Note::new(feature)))
      }
      Refusal::AttachmentUnsupported { transport, reason } => {
        Error::Unsupported(Unsupported::Attachment { transport, reason })
      }
      Refusal::MapUnavailable => Error::Unsupported(Unsupported::DirectMap),
      Refusal::InvalidName => Error::Invalid(Invalid::Name),
      Refusal::PolicyMismatch => Error::Invalid(Invalid::NamePolicy),
      Refusal::BadRequest { reason } => Error::Invalid(Invalid::Request(Note::new(reason))),
      Refusal::ReadOnlyVolume => Error::Invalid(Invalid::Kind(KindMismatch::ReadOnlyGreen)),
      Refusal::NotGreen => Error::Invalid(Invalid::Kind(KindMismatch::NotGreen)),
      Refusal::NotWork => Error::Invalid(Invalid::Kind(KindMismatch::NotWork)),
      Refusal::EvidenceRequired => Error::Invalid(Invalid::EvidenceRequired),
      Refusal::ConsistentBaseUnavailable => Error::Invalid(Invalid::LiveBase),
      Refusal::DigestNotClean => Error::Invalid(Invalid::NoCleanDigest),
      Refusal::ChosenPathUnavailable { reason } => Error::Invalid(Invalid::ChosenPath(reason)),
      Refusal::ImageRefused { reason } => Error::Invalid(Invalid::Image(Note::new(reason))),
      Refusal::OwnMount { path, used_as } => Error::Invalid(Invalid::OwnMount { path, used_as }),
      Refusal::Destroying => Error::Unusable(Unusable::Destroying),
      Refusal::Archived => Error::Unusable(Unusable::Archived),
      Refusal::NotRecovered { cause } => Error::Unusable(Unusable::NotRecovered { cause }),
      Refusal::ConsensusNotInitialized => Error::Unusable(Unusable::NoGroup),
      Refusal::ConsensusRecoveryUnavailable => Error::Unusable(Unusable::RecoveryUnavailable),
      Refusal::DurabilityUnmet {
        coincident_loss,
        accepted_loss,
        coincident_failures,
      } => Error::Unusable(Unusable::DurabilityUnmet {
        coincident_loss,
        accepted_loss,
        coincident_failures,
      }),
      Refusal::HomedElsewhere { region } => Error::Elsewhere(Elsewhere::Region { region }),
      Refusal::NotRootLeader => Error::Elsewhere(Elsewhere::RootLeader),
      Refusal::BaseUnavailable { path, errno } => Error::Host(HostFault::Base { path, errno }),
      Refusal::TargetUnavailable { reason } => Error::Host(HostFault::Target(Note::new(reason))),
      Refusal::DuplicateRequest => Error::Protocol(ProtocolFault::DuplicateRequest),
    }
  }
}

/// A refusal for now, with its pause.
fn retry(cause: RetryCause, pause_ns: u64) -> Error {
  Error::Retry { cause, pause_ns }
}

impl fmt::Display for Error {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::NotFound(missing) => write!(f, "not found: {missing}"),
      Self::AlreadyExists(Existing::Volume { existing }) => write!(
        f,
        "a volume holds that name already: {}",
        crate::volume_id_text(*existing)
      ),
      Self::AlreadyExists(Existing::Group) => f.write_str("this instance's consensus group exists already"),
      Self::Denied(denied) => write!(f, "denied: {denied:?}"),
      Self::Exhausted(exhausted) => write!(f, "a bound is reached: {exhausted:?}"),
      Self::Busy(busy) => write!(f, "held by another: {busy:?}"),
      Self::Stale(stale) => write!(f, "stale: {stale:?}; read the current state and ask again"),
      Self::Retry { cause, pause_ns } => write!(f, "refused for now ({cause:?}); ask again after {pause_ns} ns"),
      Self::Unavailable { instance, cause } => write!(f, "no daemon at instance {instance}: {cause}"),
      Self::Unsupported(unsupported) => write!(f, "not offered here: {unsupported:?}"),
      Self::Invalid(invalid) => write!(f, "invalid request: {invalid:?}"),
      Self::Unusable(Unusable::NoGroup) => f.write_str(
        "this node has no consensus group yet; create it once (`bootstrap`), or, on a fleet node, wait for it to \
         import the groups",
      ),
      Self::Unusable(unusable) => write!(f, "unusable now: {unusable:?}"),
      Self::Elsewhere(elsewhere) => write!(f, "ask elsewhere: {elsewhere:?}"),
      Self::Host(fault) => write!(f, "the host refused: {fault:?}"),
      Self::Mount(fault) => write!(f, "{fault}"),
      Self::Start(fault) => write!(f, "{fault}"),
      Self::Protocol(fault) => write!(f, "a fault in slates (report it): {fault:?}"),
    }
  }
}

impl fmt::Display for Missing {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Volume(volume) => write!(f, "volume {}", crate::volume_id_text(*volume)),
      Self::Snapshot { volume, snapshot } => write!(
        f,
        "volume {} or its snapshot {}",
        crate::volume_id_text(*volume),
        snapshot.value
      ),
      Self::Presentation { id } => write!(
        f,
        "presentation {id} (replaced, landed, or its client went away: present it again)"
      ),
      Self::Attachment { attachment } => write!(f, "attachment {attachment}"),
      Self::Entry { volume, path } => {
        write!(f, "{path} in volume {}", crate::volume_id_text(*volume))
      }
      Self::Consumer { consumer } => write!(f, "consumer {consumer}"),
      Self::Shard { shard } => write!(f, "shard {shard}"),
    }
  }
}

impl fmt::Display for Absence {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::NoDaemon(note) => write!(f, "{note}"),
      Self::Gone { after_ns } => write!(
        f,
        "the daemon went away and did not come back within {after_ns} ns"
      ),
      Self::CompletionLost(note) => write!(f, "the client can no longer observe replies: {note}"),
    }
  }
}

impl std::error::Error for Error {}

/// What is wrong with a path a verb takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathFault {
  /// It is relative: the daemon would resolve it against its own working directory, not the caller's.
  NotAbsolute,
  /// It is not Unicode, and the wire carries paths as UTF-8.
  NotUnicode,
}

impl fmt::Display for PathFault {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::NotAbsolute => f.write_str(
        "a relative path; the daemon resolves paths against its own working directory, so pass an absolute one",
      ),
      Self::NotUnicode => f.write_str("not Unicode; the wire carries paths as UTF-8"),
    }
  }
}

/// Why a host mount could not be made or removed (§4.6). An OS refusal carries its error number, never a message
/// alone, so a host decides on it.
#[derive(Debug, Clone, PartialEq)]
pub enum MountFault {
  /// The mount point could not be resolved to the real path the kernel records (it does not exist, or is not
  /// reachable); the error number.
  MountPoint {
    /// The mount point as given.
    path: String,
    /// The OS error number, when the OS gave one.
    errno: Option<i32>,
  },
  /// This machine cannot mount the form asked: what is missing.
  Unsupported {
    /// What is missing.
    missing: MountMissing,
  },
  /// The daemon serves no NFS loopback listener (its bind failed), so no loopback mount can reach it.
  NoNfsListener,
  /// The daemon issued no mount capability for the mount's attachment (§4.13; AUD-01), so nothing could present one.
  NoCapability,
  /// The daemon's MOUNT service did not hand over the export's root handle (macOS, §4.6 A-34).
  RootHandle {
    /// The OS error number of the loopback exchange, when the socket failed.
    errno: Option<i32>,
    /// The MOUNT status the daemon refused with (`mountstat3`), when it answered one.
    status: Option<u32>,
  },
  /// The kernel refused the mount; the error number.
  Refused {
    /// The mount point.
    path: String,
    /// The OS error number, when the OS gave one.
    errno: Option<i32>,
  },
  /// The mount was made but its record could not name it (§4.4 `Binding → Bound`), and taking it down again failed
  /// too: the mount stays until its user unmounts it.
  Unbound {
    /// The daemon's refusal of the bind.
    bind: Box<Error>,
    /// Why taking the mount down again failed.
    unmount: Box<MountFault>,
  },
  /// The OS program that removes a mount (`umount`, `fusermount3 -u`) could not run, or refused: the path is still
  /// in use, or nothing is mounted there.
  Unmount {
    /// The mount point.
    path: String,
    /// The program's exit code, when it ran and exited.
    status: Option<i32>,
    /// The OS error number of starting it, when it could not run.
    errno: Option<i32>,
  },
  /// No volume that reported its mounts is mounted at the path (Windows, where a drive is the daemon's own and is
  /// removed by detaching its attachment): the volumes that could not report theirs.
  NotMounted {
    /// The path asked.
    path: String,
    /// Each volume whose status was refused, and the refusal.
    unreported: Vec<Unreported>,
  },
}

/// A volume that could not report its mounts ([`MountFault::NotMounted`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Unreported {
  /// The volume.
  pub volume: VolumeId,
  /// Its name.
  pub name: String,
  /// Why its status was refused.
  pub refusal: Box<Error>,
}

/// What this machine lacks to mount the form asked (§4.6). Each says what to install or change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MountMissing {
  /// `mount_nfs` is not on the `PATH` (a BSD): the loopback mount needs it.
  MountNfs,
  /// `fusermount3` is not on the `PATH` (Linux): the daemon mounts FUSE through it, unprivileged (R10).
  Fusermount3,
  /// `/dev/fuse` is missing, or this user cannot read and write it (Linux).
  FuseDevice,
  /// WinFsp's library did not load (Windows).
  WinFsp,
  /// A shared mount is the Linux FUSE mount's `allow_other`; other hosts' mounts are the user's alone.
  SharedMount,
  /// A subtree or shared mount is the Unix mounts'; a WinFsp mount presents the whole volume at a drive.
  WindowsSubtreeOrShared,
  /// A lazy unmount is Linux's (`fusermount3 -u -z`); elsewhere unmount the path once its users have closed it.
  LazyUnmount,
}

impl fmt::Display for MountMissing {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(match self {
      Self::MountNfs => {
        "`mount_nfs` is not on the PATH — slates's loopback mount needs it (it is built into macOS and the BSDs); \
         no kernel extension, FUSE library, or Apple entitlement is required"
      }
      Self::Fusermount3 => {
        "`fusermount3` is not on the PATH — the daemon mounts FUSE through it, unprivileged (R10); the OS's fuse3 \
         package ships it"
      }
      Self::FuseDevice => {
        "/dev/fuse is missing, or this user cannot read and write it — load the fuse module, or pass the device into \
         the container"
      }
      Self::WinFsp => {
        "WinFsp is not installed: its library (winfsp-x64.dll, or winfsp-a64.dll on arm64) did not load; install \
         WinFsp from winfsp.dev"
      }
      Self::SharedMount => {
        "a shared mount is the Linux FUSE mount's allow_other; a macOS host mount reaches a container through the \
         container's own runtime"
      }
      Self::WindowsSubtreeOrShared => {
        "a subtree or shared mount is the Unix mounts'; a WinFsp mount presents the whole volume at a drive letter"
      }
      Self::LazyUnmount => {
        "a lazy unmount is Linux's (`fusermount3 -u -z`); here unmount the path once its users have closed it"
      }
    })
  }
}

impl fmt::Display for MountFault {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::MountPoint { path, errno } => {
        write!(f, "the mount point {path}: {}", errno_text(*errno))
      }
      Self::Unsupported { missing } => write!(f, "cannot mount here: {missing}"),
      Self::NoNfsListener => f.write_str(
        "the daemon is not serving NFS (its loopback listener did not bind); cannot mount",
      ),
      Self::NoCapability => f.write_str(
        "the daemon issued no mount capability for this attachment; the volume cannot be mounted",
      ),
      Self::RootHandle { errno, status } => match (errno, status) {
        (_, Some(status)) => write!(
          f,
          "the daemon's MOUNT service refused the export (mountstat3 {status})"
        ),
        (errno, None) => write!(f, "the daemon's MOUNT service: {}", errno_text(*errno)),
      },
      Self::Refused { path, errno } => {
        write!(f, "mounting at {path}: {}", errno_text(*errno))
      }
      Self::Unbound { bind, unmount } => write!(
        f,
        "the mount's record could not name it ({bind}), and unmounting it again failed ({unmount})"
      ),
      Self::Unmount {
        path,
        status,
        errno,
      } => match (status, errno) {
        (Some(status), _) => write!(
          f,
          "unmounting {path} failed (exit {status}); is the path still in use?"
        ),
        (None, errno) => write!(
          f,
          "unmounting {path}: the unmount program could not run: {}",
          errno_text(*errno)
        ),
      },
      Self::NotMounted { path, unreported } if unreported.is_empty() => {
        write!(f, "no slates volume is mounted at {path}")
      }
      Self::NotMounted { path, unreported } => {
        let named: Vec<String> = unreported
          .iter()
          .map(|volume| {
            format!(
              "volume {} ({}) refused: {}",
              volume.name,
              crate::volume_id_text(volume.volume),
              volume.refusal
            )
          })
          .collect();
        write!(
          f,
          "no slates volume that reported its mounts is mounted at {path}; {} did not report: {}",
          unreported.len(),
          named.join("; ")
        )
      }
    }
  }
}

/// Why this process cannot issue a grant (§4.13 "Grants"): the authority is the anchor's segment, which only the
/// anchor's own tree and, on macOS and Windows, a process given its name hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssuerFault {
  /// No anchor handoff is in this process's environment (or in the pairs given): the process was not started from
  /// the anchor's session, and the MCP server and the SDKs never are.
  NoHandoff,
  /// The handoff is incomplete or malformed: `SLATES_ANCHOR_LEN` missing, or not a number.
  Malformed {
    /// What is wrong.
    reason: &'static str,
  },
  /// The segment the handoff names could not be attached or read (gone, another machine's, another layout).
  Attach(slates_anchor::AnchorError),
  /// The daemon has not published an issuer secret yet (an all-zero secret is no authority): it is still starting.
  NotPublished,
}

impl fmt::Display for IssuerFault {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::NoHandoff => f.write_str(
        "no anchor in this environment: grants, enrollments, revocations and recoveries are the anchor user's \
         surface; run them from the session that started the daemon, with the variables the anchor printed exported \
         (the MCP server and the SDKs cannot issue them)",
      ),
      Self::Malformed { reason } => write!(f, "the anchor's handoff is malformed: {reason}"),
      Self::Attach(error) => write!(f, "attaching the anchor: {error}"),
      Self::NotPublished => f.write_str("the daemon has published no issuer secret yet"),
    }
  }
}

/// An OS error number as a person reads it.
fn errno_text(errno: Option<i32>) -> String {
  match errno {
    Some(errno) => std::io::Error::from_raw_os_error(errno).to_string(),
    None => "the OS gave no error number".to_owned(),
  }
}

/// Why an instance did not start, or its anchor could not be stopped ([`crate::start`]). An OS refusal carries its error
/// number, never a message alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartFault {
  /// The instance already answers: another anchor runs it. Attach to it ([`crate::Instance::attach`]) instead.
  AlreadyRunning {
    /// The instance.
    instance: String,
  },
  /// The anchor's program could not be started (missing, or not executable).
  Spawn {
    /// The program.
    program: String,
    /// The OS error number, when the OS gave one.
    errno: Option<i32>,
  },
  /// The anchor ended before the instance answered. Its log lines on standard error say why: a second anchor of a
  /// live instance is refused, as is a profile the machine could not measure, or a host whose `main` did not hand its
  /// prefix's arguments to [`crate::role::main`].
  AnchorExited {
    /// Its exit code (`None` when a signal ended it).
    code: Option<i32>,
  },
  /// Asking whether the anchor still runs, or waiting for it to exit, failed.
  Wait {
    /// The OS error number, when the OS gave one.
    errno: Option<i32>,
  },
  /// The anchor could not be asked to stop.
  Stop {
    /// The OS error number, when the OS gave one.
    errno: Option<i32>,
  },
}

impl fmt::Display for StartFault {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::AlreadyRunning { instance } => {
        write!(f, "the instance {instance} already runs; attach to it")
      }
      Self::Spawn { program, errno } => {
        write!(f, "starting the anchor {program}: {}", errno_text(*errno))
      }
      Self::AnchorExited { code: Some(code) } => write!(
        f,
        "the anchor exited (code {code}) before the instance answered; its log lines say why"
      ),
      Self::AnchorExited { code: None } => f.write_str(
        "the anchor was ended by a signal before the instance answered; its log lines say why",
      ),
      Self::Wait { errno } => write!(f, "waiting on the anchor: {}", errno_text(*errno)),
      Self::Stop { errno } => write!(f, "stopping the anchor: {}", errno_text(*errno)),
    }
  }
}

#[cfg(test)]
mod tests {
  use slates_client::ClientError;
  use slates_ipc::protocol::{Refusal, VolumeId};

  use super::{
    Busy, Error, Exhausted, Existing, Missing, ProtocolFault, RetryCause, Stale, Unusable,
  };

  /// Shape: the pause a test's retryable refusal carries.
  const PAUSE_NS: u64 = 7;

  /// A-299, the closed taxonomy: do type a refusal of each kind a consumer acts on differently, as a call naming a
  /// volume meets it; expect the class a consumer acts on with the refusal's own data, a `NotFound` naming the
  /// volume the call named, and a retryable refusal carrying the pause it was given.
  #[test]
  fn each_refusal_is_typed_into_the_class_a_consumer_acts_on() {
    let volume = VolumeId { bytes: [7; 16] };
    let typed = |refusal: Refusal| {
      Error::of_client(
        "i",
        ClientError::Refused(refusal),
        Some(Missing::Volume(volume)),
        PAUSE_NS,
      )
    };
    assert_eq!(
      typed(Refusal::NotFound),
      Error::NotFound(Missing::Volume(volume))
    );
    assert_eq!(
      typed(Refusal::AlreadyExists { existing: volume }),
      Error::AlreadyExists(Existing::Volume { existing: volume })
    );
    assert_eq!(
      typed(Refusal::LeaseHeld { epoch: 3 }),
      Error::Busy(Busy::WriteLease { epoch: 3 })
    );
    assert_eq!(
      typed(Refusal::BudgetExceeded { available: 9 }),
      Error::Exhausted(Exhausted::Memory { available: 9 })
    );
    assert_eq!(
      typed(Refusal::StaleLease { current: 4 }),
      Error::Stale(Stale::Lease { current: 4 })
    );
    let overloaded = typed(Refusal::Overloaded { shard: 1 });
    assert_eq!(
      overloaded,
      Error::Retry {
        cause: RetryCause::Overloaded { shard: 1 },
        pause_ns: PAUSE_NS
      }
    );
    assert!(overloaded.is_retryable());
    assert_eq!(overloaded.retry_pause_ns(), Some(PAUSE_NS));
    assert_eq!(
      typed(Refusal::ConsensusNotInitialized),
      Error::Unusable(Unusable::NoGroup)
    );
    assert!(!typed(Refusal::ConsumerRevoked).is_retryable());
  }

  /// A-299: do type a `NotFound` for a call that names nothing that could be missing, and a daemon gone past the
  /// reconnect budget; expect a protocol fault (never a guess at the subject) and `Unavailable` naming the instance.
  #[test]
  fn a_refusal_the_call_cannot_meet_is_a_protocol_fault_and_a_gone_daemon_is_unavailable() {
    assert!(matches!(
      Error::of_client("i", ClientError::Refused(Refusal::NotFound), None, PAUSE_NS),
      Error::Protocol(ProtocolFault::UnexpectedRefusal(_))
    ));
    let gone = Error::of_client("i", ClientError::DaemonGone { after_ns: 5 }, None, PAUSE_NS);
    assert!(gone.is_unavailable() && gone.is_retryable(), "{gone:?}");
  }
}
