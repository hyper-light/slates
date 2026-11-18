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
//! - *The chosen-path launcher* ([`exec`], A-370): a volume shown at a path one command names, in a namespace of its
//!   own, served by the host's own executable; where the host's AppArmor restricts unprivileged user namespaces, it uses
//!   slates' AppArmor profile only when its user opts in ([`AppArmor`]).
//! - *Write permission* ([`policy`]): a landing and a merge follow the mode the user gave the harness or agent asking to
//!   write — bypass, ask, forbid ([`WriteMode`]) — set by the launcher per consumer or volume, never raised by an
//!   agent. A landing answers [`LandOutcome`], a merge [`MergeOutcome`], a rebase [`RebaseOutcome`] ([`outcome`]).
//! - *Conflicts* are verdicts, typed per path ([`LandingConflict`], [`MergeConflict`]); nothing is ever inferred (D-27).
//! - *Signed work* ([`signed`], A-321): a merge or a landing may carry a signature over its statement, its outcome says
//!   how it was judged ([`Verification`]) and its record keeps it (a merge's [`MergeRecord`], a landing's
//!   [`AuditRecord`]); a volume's trusted signers are the launcher's to set, and its policy may require signed merges.
//!   Signatures never gate permission.
//! - *Errors* ([`error`]): one closed taxonomy whose variant is what a consumer does about it.
//! - *The seam* ([`seam`], [`sim`]): the volume verbs as a trait, for hosts that need a simulator (A-310).
//! - *The async form* ([`asynchronous`]): verbs a host awaits on its own loop through its [`Reactor`], never a thread
//!   blocked on a round trip (R6).
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
//! **What is owed** (GAPS, the A-278 and A-299 rows): a typed field on the wire for a merge's versions (the work's
//! base beside the green's head), which this crate reads in a second call after the verdict; and the async form for
//! the verbs it does not serve yet (the paged reads, landings, merges, policies and observation).
//!
//! **The seam (A-310).** A host whose rules reach other products only through a trait with a simulator and a
//! conformance suite (Hex's `CLAUDE.md` §9) holds [`Volumes`]: the contract's volume verbs, implemented by [`Instance`]
//! and by [`SimVolumes`], an in-memory stand-in that answers the same typed outcomes and errors. One conformance suite
//! runs over both (`the_seam_conforms` in `tests/api.rs`).

// The no-panic law (CLAUDE.md, banned item 6): shipped code never overflows or divides by zero. Test builds are exempt.
// The panic rule: a print macro panics on a refused write, and a library prints nothing: every outcome is returned.
#![deny(clippy::print_stdout, clippy::print_stderr)]
#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

pub mod asynchronous;
pub mod error;
pub mod exec;
pub mod grant;
pub mod instance;
pub mod issuer;
pub mod launcher;
pub mod mount;
pub mod observe;
pub mod outcome;
pub mod page;
pub mod policy;
pub mod role;
pub mod seam;
pub mod signed;
pub mod sim;
pub mod start;
pub mod volume;

pub use asynchronous::{AsyncInstance, Completion, Reactor, Waiting, Woke};
pub use error::{
  Absence, Busy, Denied, Elsewhere, Error, Exhausted, Existing, HostFault, Invalid, IssuerFault,
  KindMismatch, Missing, MountFault, MountMissing, MoveTarget, Note, PathFault, ProtocolFault,
  RetryCause, Stale, StartFault, Unmovable, Unreported, Unsupported, Unusable,
};
pub use exec::{AppArmor, Confinement, ExecFault, ExecSpec, Readiness};
pub use grant::{GrantAuthority, GrantClaim, GrantProof};
pub use instance::{Instance, LandingRequest};
pub use issuer::{GRANT_TERM_NS, Issuer};
pub use launcher::{LauncherFault, LauncherSecret};
pub use mount::{LetGo, MountKind, MountOptions, Mounted, Unmounted};
pub use observe::{AuditKind, AuditRecord, GrantRecord, GrantState, LandingStage};
pub use outcome::{
  ActionTally, LandOutcome, LandingApproval, LandingConflict, LandingReport, LandingState,
  MergeApproval, MergeConflict, MergeConflictClass, MergeConflicts, MergeOutcome, MergeRecord,
  MoveProgress, RebaseOutcome, Summary, Work,
};
pub use page::Page;
pub use policy::{PolicyReason, PolicyRefusal, PolicySubject, WriteAct, WriteMode, WritePolicy};
pub use seam::Volumes;
pub use signed::{
  MergeStatement, SignatureFault, Signer, SignerKey, TrustedSigner, Verification, WorkSignature,
};
pub use sim::SimVolumes;
pub use start::{Anchor, StartSpec, Started};
pub use volume::{
  BaseState, Drift, DriftKind, DriftPage, DriftReport, Provisioned, Source, VolumeSpec,
  VolumeStatus, Watcher,
};

/// The crate's version: the workspace's one hand-edited version (§4.12 "One version"), which a host reports beside its
/// own.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The consumer contract's version (§4.12 "Versioning", A-299), raised by one with every change to a type or a verb
/// of the contract; a consumer asserts the one it was written against.
///
/// 1: the contract as A-299 first published it (2026-11-18). 2: `Denied::Policy`, the class a write policy's refusal
/// takes outside a landing or a merge (the same day). 3: `Instance::set_write_policy` and `Instance::write_policy`,
/// over the daemon's policy verbs (A-285), and a landing or a merge a policy forbids answered as its `Refused` outcome
/// (the same day). 4: the audit log and the
/// grant listing typed (`AuditRecord`, `GrantRecord`; the same day). 5: `WritePolicy::UNSET` merges at once and asks
/// for landings, as the daemon applies an unset policy (the same day). 6: a merge an ask mode holds is a
/// `MergeApproval` (the increment's identity and paths) approved by `approve_merge`, and a landing's approval a
/// `LandingApproval` approved by `approve_landing` (`Approval` and `approve` split by what they approve); `merge` takes
/// evidence; `plan_landing` presents through the daemon's present-only verb whatever the mode (the same day). 7: the
/// launcher's own secret: `LauncherSecret`, `StartSpec::launcher` and `with_launcher`, `Issuer::from_launcher_secret`,
/// `StartSpec::anchor_command` fallible (`StartFault::Launcher`), `IssuerFault::LauncherCannotRecover`,
/// `Unsupported::SecureRandom`; and, from A-285's owner the same hour, `Stale::Policy`, a policy setting that raced
/// another (`set_write_policy` names the version it read) (the same day). 8: an overlay's drift read from the status
/// reply's typed field, never its text: `DriftReport::watcher` a `Watcher`, and a green's or a work volume's status
/// read with no base (it failed as a protocol fault before) (the same day). 9: the [`Volumes`]
/// seam over the contract's volume verbs, implemented by [`Instance`] and by the simulator [`SimVolumes`] (A-310) (the
/// same day). 10: a volume's loss window, `VolumeStatus::loss` (`LossReport`, `VolumeTakeover`; AC-8.3): what an owner
/// loss would lose now, the newest seal placed on another host, and on a successor the takeover that made it the owner
/// (the same day). 11: `DaemonReport::recovered`, the anchor's verdict that the daemon has served as long as its start
/// took, so its exit is no failed start (A-311) (the same day). 12: `ShardsStream::resync`, the device's end of a Shards
/// stream's resync channel, which `serve_shards` hands beside the stream's where the daemon makes one (Linux; macOS too
/// since A-339, and Windows since A-427, the type unchanged), none from the simulator (A-316; the same day). 13: signed merges
/// (A-321): `merge` sends its signature and answers how the daemon judged it, `merge_statement` (a
/// [`MergeStatement`]), `approve_merge` carries the signature again (`MergeApproval::signature`), `merge_record` (a
/// [`MergeRecord`], `Missing::Version`), `set_signers` and `signers` ([`TrustedSigner`]); a volume's policy may require
/// signed merges, refused
/// [`PolicyReason::SignatureRequired`], and a consumer's that does `Invalid::SignedMergesNotAVolume`;
/// `SignatureFault::Expired` gone, since no statement expires; a signed landing still `Unsupported(SignedWork)` (the same
/// day). 14: `StartSpec::anchor_command_under`, the anchor's command under a wrapper that execs it (`systemd-run
/// --scope`), the launcher's standard input kept (A-299; the same day). 15: the volume core's refusals typed
/// (`VolumeRefusal`, each in the class a consumer acts on as `Busy`, `Stale`, `Denied`, `Exhausted`, `Unusable` or
/// `Invalid` `::Volume`): until then each reached a consumer as an invalid request carrying a word, a pinned snapshot's
/// destroy among them (A-299; the same day). 16: `Instance::unmount` and the seam's `unmount` answer what was removed ([`Unmounted`]): on
/// Windows each volume the scan could not ask is named there when another volume reported the drive (it was named only
/// when none did), and a host refusal renders in words, the daemon's `TargetUnavailable` by that name (A-299; the same
/// day). 17: the daemon's database refusals typed (`DatabaseRefusal`, unusable as `Unusable::Database`): until then a
/// corrupt record, a regressed NFSv4 instance, a checkpoint out of range, a malformed sealing key and a refused encoding
/// reached a consumer as an invalid request carrying words (A-299; the same day). 18: a snapshot's retention refused is
/// typed apart from the quota (`VolumeRefusal::RetentionExceeded`, exhausted as `Exhausted::Volume`): until then the
/// volume core answered it `NoSpace`, which a consumer read as its own quota (kiln's review, A-342's record; the same
/// day). 19: a target's refusal is typed (`HostFault::Target(TargetRefusal)`, the wire's appended `Refusal::Target`):
/// until then a landing target's, a mount point's and a mount's refusals carried the daemon's words; the superseded
/// `TargetUnavailable` is a protocol fault, since no daemon of this build sends it (A-299; the same day). 20: signed
/// landings (A-321): `land` sends its signature and the daemon judges it, so `Unsupported::SignedWork` is gone; a
/// [`LandingApproval`] carries the landing's `statement` (what a signer signs) and the `signature` it carried, which
/// `approve_landing` presents and lands with again; a [`LandingReport`]'s `verification` is the verdict on what was
/// written, no longer always unsigned; an [`AuditRecord`] of a landing as a whole carries its `verification` (the same
/// day). 21: a takeover past the volume's tolerance shows in its loss report (`VolumeTakeover::past_tolerance`, a
/// [`PastTolerance`] naming the [`Shortfall`] of its registers' cohort and of its live log's recorded holders): until
/// then a consumer could not tell a takeover that may have lost writes acknowledged only to the lost hosts from one that
/// kept every one (A-332; the same day). 22: a reply held for durability past the failover SLO is a typed
/// retry (`RetryCause::DurabilityPending`, the wire's appended `Refusal::DurabilityPending`): until then a write whose
/// log or tombstone could not be held where its reply waits for it was never answered (A-354; the same day). 23: a
/// start a host waits on needs no wall-clock deadline of its own: [`StartSpec::until_served`] asks the anchor to stop when its daemons crash-loop before any of them has served, and
/// [`Instance::wait_for`] with that anchor's child then ends [`StartFault::NeverServed`]; until then an anchor looped on
/// such an instance for good (A-119), so a host bounded its wait by a second it picked, and a slow start under load
/// outlasted it (kiln's 30 s at load 28, 2026-11-18; A-366). 24: the daemon's status carries the failover delay as the
/// node estimates it now (`DaemonReport::fleet`'s `failover`, a [`FailoverReport`]): detection, confirmation and
/// recovery, each with the measurements behind it, summed; nothing sets it, it moves with what the node measures (A-363;
/// the same day). 25: the chosen-path launcher ([`exec`]): an [`ExecSpec`] whose [`AppArmor`] option, off unless the
/// user opts in, lets the launcher make its namespace under slates' AppArmor profile where the host restricts
/// unprivileged user namespaces; [`exec::command`] builds the command that runs it from a host's own executable, which
/// [`role::main`] serves; [`exec::check`] answers the host's [`Readiness`]; its refusals are [`ExecFault`] (A-370; the
/// same day).
/// 26: lists a page at a time (A-263 to A-267): `Page`, `Instance::{list_page, grants_page, audit_page,
/// changed_since, changed_since_page, drift_page}`, `DriftPage`, `DriftReport::elided`, `ChangedCursor`;
/// `Instance::drift` reads every entry; a landing target longer than a grant's listing holds is refused
/// `TargetRefusal::TooLong` (A-266) (the same day). 27: a list a verb's own reply carries counts what one reply could
/// not hold (A-267): `Instance::rewitness` answers `Rewitnessed { paths, paths_elided }`, `Stale::Disk` carries
/// `elided`, `LandingReport` carries `degraded_elided`, and a presentation is clean only when it carries and counts no
/// conflict (the same day). 28: `Stale::Verdict`, a merge's or a rebase's conflict windows read after the work's verdict
/// was replaced; `merge` and `rebase` answer every window of a verdict however many, a signed merge's too (A-267) (the
/// same day). 29: `drift_page` reads past an inode (`after: Option<u64>`), in inode order, re-checking the entries it
/// passes; `drift` answers its lists in path order (A-267) (the same day). 30: a granted landing that is refused ends
/// its audit trail: the records of the steps its attempt took, then an [`AuditKind::LandingFinished`] whose `outcome` is
/// [`LandingStage::Refused`], or [`LandingStage::Aborted`] once it wrote an entry; such an end that names no manifest
/// (refused before any plan, under a grant that names none) carries no `verification`. Until then a refused landing's
/// trail ended at its grant (found by g90; the same day). 31: the async form ([`AsyncInstance`] over a host's
/// [`Reactor`]: provisioning from every source, a snapshot, a status and a destroy, awaited on the host's own loop
/// through the client's driver; [`AsyncInstance::into_instance`] stops the completion signal, so the synchronous form's
/// waits are woken; R6) (A-299; the same day).
/// 32: `AuditKind::GrantExpired`, the record a grant's end at its term leaves in the audit (A-266) (the same day).
/// 33: a volume's move to another fleet member (A-291, the same day): `Instance::move_volume` and `Instance::member`,
/// `MoveProgress`, and its refusals' details (`Busy::Attached`, `Busy::Moving`, `Unusable::Unmovable`,
/// `Invalid::MoveTarget`, `RetryCause::MoveUnplaced`). 34: `Instance::capture_base`, an overlay's base captured whole so
/// its clones move (A-291, the same day). 35: only a granted landing ends its audit trail when refused: one whose grant
/// the daemon held as it began, so every such [`AuditKind::LandingFinished`] names its manifest and its verdict. A
/// landing under a grant the daemon does not hold is presented once planned, and refused before its plan it is a
/// refused request with no trail; under 30 it wrote an end naming no manifest (the same day). 36: a busy mount is let go
/// of on every Unix host: [`mount::unmount_busy`] answers how ([`LetGo::Detached`] on Linux, [`LetGo::Forced`] on macOS
/// and the BSDs), where Linux's `unmount_lazy` was refused elsewhere; `MountMissing::LazyUnmount` is gone (found by
/// g076a; the same day). 37: [`mount::available`] on Windows answers [`MountMissing::MountManager`] where the machine
/// does not let WinFsp's driver make a mount's drive (`MountUseMountmgrFromFSD`, which the driver reads at every mount,
/// A-369); until then such a machine was told it could mount, and every mount there was refused afterwards (A-417; the
/// same day). 38: [`Instance::export_archive`], a snapshot exported as its archive into a volume of the caller's
/// ([`ArchiveExported`], [`ARCHIVE_FILE`]), read back a page at a time with [`Instance::read`] (Mantle's CP-T10.4,
/// A-441; the same day).
/// 39: [`role::check_fleet`] reads a fleet manifest and plans its node exactly as the node's anchor and
/// daemon do at their start, answering a [`role::FleetCheck`] or the start's own refusal, for a starter that writes the
/// manifest itself (kiln's runners as a fleet, kiln goal 95101ef3; the same day).
/// 40: [`Instance::import_archive`], a volume made from an archive a volume of the caller's holds as [`ARCHIVE_FILE`]
/// ([`ArchiveImported`]), and its refusal by kind, [`Invalid::Archive`] naming an [`ArchiveFault`] (Mantle's
/// CP-T10.4, A-453; the same day).
/// 41: a landing carries a file whose mode or Windows attribute bits alone changed (A-458): [`PlannedAction::Chmod`]
/// names such an entry with the mode and bits it had, and [`ActionTally::chmod`] counts them; until then such a change
/// was planned as a replacement the verdict passed by, and it never landed (the same day).
///
/// Format: one whole number, the last of the versions above.
pub const CONTRACT: u32 = 41;

/// What a rewitness did (§4.15 `rewitness`): the paths one reply holds and the count of the rest (A-267).
pub use slates_client::Rewitnessed;
/// A snapshot exported as its archive ([`Instance::export_archive`], A-441), and a volume made from one
/// ([`Instance::import_archive`], A-453).
pub use slates_client::{ArchiveExported, ArchiveImported};
/// The typed client and its own types, for the verbs [`Instance::client`] lends outside the contract.
pub use slates_client::{Attachment, Client, Deadlines, ShardsStream, StreamEnd, VhostUserDevice};
/// The wire's types the contract takes and returns (§4.9; append-only, D-15).
pub use slates_ipc::protocol::{
  ARCHIVE_FILE, ArchiveFault, AttachTransport, ChangedCursor, ConflictClass, DaemonReport,
  DatabaseRefusal, FailoverReport, Filter, GrantScope, GreenBase, HostAnswer, HostPathReason,
  Intent, JournalReport, LandingDegradation, LandingDurability, LossReport, MountReport,
  NamePolicy, OwnMountUse, PastTolerance, PlacedState, PlannedAction, PlannedContent, PlannedEntry,
  Principal, ReadAt, RecoveryCause, Shortfall, SnapshotId, TargetRefusal, TelemetryReport,
  TransportReport, UnsupportedReason, VolumeId, VolumeRefusal, VolumeSummary, VolumeTakeover,
};
/// The rendezvous's and the channel's own typed refusals the contract carries.
pub use slates_ipc::{DeliveryFault, error::RouteRefusal};

/// The client's deadlines as the product derives them (§4.7 "Failure matrix"; `Deadlines::derive`): a wait that cannot
/// see the daemon's departure asks after the daemon's liveness as often as the anchor waits for the daemon's heartbeat
/// (a synchronous call's reply is waited for until it comes or the daemon's process ends, A-456), and a request refused
/// for now during a fleet takeover is asked again for the recovery budget and a liveness window. A reconnect itself
/// waits while the instance's anchor lives, or fails at once with none (A-468). Every client this crate makes uses
/// them, as the CLI's do.
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

/// The user guide, `docs/library.md`, as rustdoc collects doctests: each of its Rust examples compiles against this
/// crate (`cargo test -p slates-api --doc`), so the guide cannot drift from the API it shows (a doc-truth test,
/// CLAUDE.md §4; the library audit's gap d, 2026-11-18). Nothing else ever sees it: `cfg(doctest)` holds only while
/// rustdoc collects doctests, so no build compiles it, no rendered page shows it, and a vendored copy without `docs/`
/// builds as before.
#[cfg(doctest)]
#[doc = include_str!("../../../docs/library.md")]
pub struct Guide;

#[cfg(test)]
mod contract_history {
  /// §4.12 "Versioning" (A-299; a doc-truth test, CLAUDE.md §4): the contract's version list names every version up to
  /// [`super::CONTRACT`] in order, so a raised number without its line, or a line past the number, fails here. Do: read
  /// [`super::CONTRACT`]'s own doc from this file and find each version's number as the list writes it (`N: `). Expect
  /// every number from 1 to `CONTRACT` named, in increasing order, and none past it.
  #[test]
  fn the_contract_history_names_every_version_in_order() {
    let source = include_str!("lib.rs");
    let declared = source
      .find("pub const CONTRACT: u32")
      .expect("the contract's constant");
    let doc_start = source
      .get(..declared)
      .and_then(|before| before.rfind("/// The consumer contract's version"))
      .expect("the contract's doc");
    let doc: String = source
      .get(doc_start..declared)
      .expect("the doc's text")
      .lines()
      .map(|line| line.trim_start_matches("///"))
      .collect::<Vec<_>>()
      .join(" ");
    let mut from = 0;
    for version in 1..=super::CONTRACT {
      let line = format!(" {version}: ");
      let at = doc
        .get(from..)
        .and_then(|rest| rest.find(&line))
        .unwrap_or_else(|| panic!("version {version} has no line after the one before it"));
      from = from + at + line.len();
    }
    let past = format!(" {}: ", super::CONTRACT + 1);
    assert!(
      !doc.contains(&past),
      "a line past the contract's number: {past}"
    );
  }
}
