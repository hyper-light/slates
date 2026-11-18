//! `slates-api`: the library an embedding host drives slates through, and its typed consumer contract (§4.12
//! "Embedding", A-278; "The consumer contract", A-299). A host such as kiln vendors slates and reaches it only through
//! this crate, never through the `slates` command: it starts or attaches to an instance, provisions and manages
//! volumes, serves a volume as a host mount or as a descriptor a VMM takes, reads an overlay's drift, lands a volume
//! and merges a work volume under the write permission its user granted, and observes the daemon. Every answer is a
//! typed outcome and every failure a typed [`Error`]: closed enums a consumer matches and acts on, never text to parse.
//!
//! **Why a crate of its own.** The Rust client (`slates-client`, §2.4) is the ring protocol's one implementation and
//! the base of every surface: the CLI, the MCP server and both SDKs. What an embedding host needs past it was the CLI's
//! alone until 2026-11-18 — the host mount, the issuer authority, the anchor and daemon roles — so kiln ran the
//! `slates` binary for every verb and parsed its JSON. Those pieces live here now and the CLI calls them, so there is
//! one implementation, not a copy per front end (CLAUDE.md §2 item 7).
//!
//! **The contract (A-299).**
//! - *Volumes* ([`volume`]): one typed create for every source ([`VolumeSpec`], [`Source`]: an overlay over a host
//!   folder, empty, a clone of a snapshot, a green, a work volume), snapshot, destroy, list, a typed status, and an
//!   overlay's drift as typed entries.
//! - *Serving* ([`mount`], [`Instance::serve_shards`], [`Instance::serve_vhost_user`]): a host mount on bare metal, or
//!   a descriptor a VMM takes.
//! - *Write permission* ([`policy`]): a landing and a merge follow the mode the user gave the harness or agent asking to
//!   write — bypass, ask, forbid ([`WriteMode`]) — set by the launcher per consumer or volume, never raised by an
//!   agent. A landing answers [`LandOutcome`], a merge [`MergeOutcome`], a rebase [`RebaseOutcome`] ([`outcome`]).
//! - *Conflicts* are verdicts, typed per path ([`LandingConflict`], [`MergeConflict`]); nothing is ever inferred (D-27).
//! - *Signed work* ([`signed`]): a merge or a landing may carry a signature, and its outcome says how it was judged
//!   ([`Verification`]); a volume's policy may require signed merges. Signatures never gate permission.
//! - *Errors* ([`error`]): one closed taxonomy whose variant is what a consumer does about it.
//!
//! **Starting an instance with no slates binary** ([`start`], [`role`]). A host starts its instance from its own
//! executable: [`Instance::start`] runs `HOST_EXE PREFIX... --instance NAME anchor`, and the host's `main` hands the
//! arguments after its `PREFIX` to [`role::main`], which serves the anchor and daemon roles the `slates` binary serves.
//! The anchor starts its daemon from the same executable and prefix.
//!
//! **What a host holds.** An [`Instance`]: a client of one running instance (§4.7) and every verb of the contract. Its
//! calls are the client's synchronous round trips, which spin and then park on the daemon's wake word (§4.7 "Wake
//! strategy"); a provisioning verb answers in microseconds (R9), and a long one — a landing, a FUSE mount — waits while
//! the daemon lives and fails [`Error::Unavailable`] once it is gone (A-232). The launcher's authority is an [`Issuer`]
//! ([`grant`]): what approves a landing an agent's mode asks about.
//!
//! **Versioning.** Two numbers. [`VERSION`] is the workspace's one hand-edited version (§4.12 "One version").
//! [`CONTRACT`] is the contract's: raised by one with every change to a type or a verb of the contract. No contract enum
//! is `#[non_exhaustive]`, so a consumer that pins slates at a commit asserts the contract version it was written
//! against and matches exhaustively; at its next vendoring a moved contract shows as that assertion failing beside the
//! compile errors of the matches it must extend — never as a variant falling silently through a wildcard. The wire
//! types re-exported here grow append-only (D-15) and count as the contract's where it returns them.
//!
//! **What is owed** (GAPS, the A-278 and A-299 rows): the starter handed the launcher's authority on every platform;
//! the write modes, approvals of merges and signed work verified on the daemon's side (A-285); typed fields on the wire
//! for drift, a presentation's conflicts and a merge's versions, replacing what this crate reads from text; a paged
//! listing; an async form over a host's reactor (R6); a seam trait with a simulator and a conformance suite.

// The no-panic law (CLAUDE.md, banned item 6): shipped code never overflows or divides by zero. Test builds are exempt.
// The panic rule: a print macro panics on a refused write, and a library prints nothing: every outcome is returned.
#![deny(clippy::print_stdout, clippy::print_stderr)]
#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

pub mod error;
pub mod grant;
pub mod instance;
pub mod issuer;
pub mod mount;
pub mod observe;
pub mod outcome;
pub mod policy;
pub mod role;
pub mod signed;
pub mod start;
pub mod volume;

pub use error::{
  Absence, Busy, Denied, Elsewhere, Error, Exhausted, Existing, HostFault, Invalid, IssuerFault,
  KindMismatch, Missing, MountFault, MountMissing, Note, PathFault, ProtocolFault, RetryCause,
  Stale, StartFault, Unreported, Unsupported, Unusable,
};
pub use grant::{GrantAuthority, GrantClaim, GrantProof};
pub use instance::{Instance, LandingRequest};
pub use issuer::{GRANT_TERM_NS, Issuer};
pub use mount::{MountKind, MountOptions, Mounted};
pub use observe::{AuditKind, AuditRecord, GrantRecord, GrantState, LandingStage};
pub use outcome::{
  ActionTally, Approval, Awaiting, LandOutcome, LandingConflict, LandingReport, LandingState,
  MergeConflict, MergeConflictClass, MergeConflicts, MergeOutcome, RebaseOutcome, Summary, Work,
};
pub use policy::{PolicyReason, PolicyRefusal, PolicySubject, WriteAct, WriteMode, WritePolicy};
pub use signed::{SignatureFault, Signer, SignerKey, Verification, WorkSignature};
pub use start::{Anchor, StartSpec, Started};
pub use volume::{
  BaseState, Drift, DriftKind, DriftReport, Provisioned, Source, VolumeSpec, VolumeStatus, Watcher,
};

/// The crate's version: the workspace's one hand-edited version (§4.12 "One version"), which a host reports beside its
/// own.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Format: the consumer contract's version (§4.12 "Versioning", A-299), raised by one with every change to a type or a
/// verb of the contract; a consumer asserts the one it was written against. 1: the contract as A-299 first published it
/// (2026-11-18). 2: `Denied::Policy`, the class a write policy's refusal takes outside a landing or a merge (the
/// same day). 3: `Instance::set_write_policy` and `Instance::write_policy`, over the daemon's policy verbs (A-285), and
/// a landing or a merge a policy forbids answered as its `Refused` outcome (the same day). 4: the audit log and the
/// grant listing typed (`AuditRecord`, `GrantRecord`; the same day).
pub const CONTRACT: u32 = 4;

/// The typed client and its own types, for the verbs [`Instance::client`] lends outside the contract.
pub use slates_client::{Attachment, Client, Deadlines, ShardsStream, StreamEnd, VhostUserDevice};
/// The wire's types the contract takes and returns (§4.9; append-only, D-15).
pub use slates_ipc::protocol::{
  AttachTransport, ConflictClass, DaemonReport, Filter, GrantScope, GreenBase, HostAnswer,
  HostPathReason, Intent, JournalReport, LandingDegradation, LandingDurability, MountReport,
  NamePolicy, OwnMountUse, PlacedState, PlannedAction, PlannedContent, PlannedEntry, Principal,
  ReadAt, RecoveryCause, SnapshotId, TelemetryReport, TransportReport, UnsupportedReason, VolumeId,
  VolumeSummary,
};
/// The rendezvous's and the channel's own typed refusals the contract carries.
pub use slates_ipc::{DeliveryFault, error::RouteRefusal};

/// The client's deadlines as the product derives them (§4.7 "Failure matrix"; `Deadlines::derive`): a reply is waited
/// for as long as the anchor waits for the daemon's heartbeat before it questions the daemon's liveness, and a
/// reconnect is tried for the recovery budget a restart is bounded by. Every client this crate makes uses them, as the
/// CLI's do.
pub fn deadlines() -> Deadlines {
  Deadlines::derive(
    slates_server::daemon::LIVENESS_BUDGET_NS,
    slates_db::replay::RECOVERY_BUDGET_NS,
  )
  .get()
}

/// A volume id as text: its sixteen bytes in hexadecimal, as `slates volume list` prints it and parses it back.
pub fn volume_id_text(id: VolumeId) -> String {
  id.bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `path` as the wire carries it: absolute, and Unicode.
pub(crate) fn absolute_text(path: &std::path::Path) -> Result<String, Error> {
  if !path.is_absolute() {
    return Err(Error::Invalid(Invalid::Path {
      path: path.to_string_lossy().into_owned(),
      fault: PathFault::NotAbsolute,
    }));
  }
  path.to_str().map(str::to_owned).ok_or_else(|| {
    Error::Invalid(Invalid::Path {
      path: path.to_string_lossy().into_owned(),
      fault: PathFault::NotUnicode,
    })
  })
}
