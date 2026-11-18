//! `slates-api`: the library an embedding host drives slates through (§4.12 "Embedding", A-278). A host such as kiln
//! vendors slates and reaches it only through this crate, never through the `slates` command: it attaches to an
//! instance, creates, clones, snapshots and destroys volumes, serves a volume as a host mount or as a descriptor a VMM
//! takes, reads what drifted under an overlay, plans a landing, lands it under a grant its own confirmation surface
//! issues, and observes the daemon. Every failure is a typed refusal ([`Error`]).
//!
//! **Why a crate of its own.** The Rust client (`slates-client`, §2.4) is the ring protocol's one implementation and
//! the base of every surface: the CLI, the MCP server and both SDKs. What an embedding host needs past it was the CLI's
//! alone until 2026-11-18 — the host mount (`crates/cli/src/mount.rs`), the issuer authority a grant is proven under,
//! the grant's term — so kiln ran the `slates` binary for every verb and parsed its JSON (kiln's
//! `crates/kiln/src/workspace/slates.rs`: one process per verb, a plan held only while the command that made it ran,
//! one `--exclude` a landing). Those pieces live here now and the CLI calls them, so there is one mount and one issuer,
//! not a copy per front end (CLAUDE.md §2 item 7).
//!
//! **Starting an instance with no slates binary** ([`start`], [`role`]). A host starts its instance from its own
//! executable: [`Instance::start`] runs `HOST_EXE PREFIX... --instance NAME anchor`, and the host's `main` hands the
//! arguments after its `PREFIX` to [`role::main`], which serves the anchor and daemon roles the `slates` binary serves
//! (they live here, and the binary calls them too). The anchor starts its daemon from the same executable and prefix.
//!
//! **What a host holds.**
//! - An [`Instance`]: a client of one running instance (the rendezvous, §4.7), and every verb this crate offers. Its
//!   calls are the client's synchronous round trips, which spin and then park on the daemon's wake word (§4.7 "Wake
//!   strategy"); a provisioning verb answers in microseconds (R9), and a long one — a granted landing, a FUSE mount —
//!   waits while the daemon lives and fails `Unavailable` once it is gone (A-232). [`Instance::client`] lends the
//!   whole typed client for the verbs this crate does not wrap (the merge verbs of §4.16, the file verbs).
//! - A grant's proof ([`grant`]): [`Instance::grant`] takes a [`GrantClaim`] and any [`GrantProof`] the daemon
//!   accepts, so the call stays as the proof's forms grow (Ada, 2026-11-18: a signature by a key only the human holds).
//!   An [`Issuer`] is today's [`GrantAuthority`]: the anchor segment's issuer secret, which only a process holding the
//!   segment can use, as `slates grant` does. The SDKs and the MCP server do not depend on this crate, so no grant
//!   verb reaches them, which is the structural exclusion §4.12 asks for.
//!
//! **Versioning.** The crate's version is the workspace's (`[workspace.package] version`, the one hand-edited
//! version, §4.12 "One version"). The report types it re-exports are the wire's, which evolve append-only (D-15): a
//! field or a variant is added, never changed or removed, so a host reads them and matches their enums with a
//! wildcard arm.
//!
//! **What is owed** (GAPS, the A-278 row): an async form over a host's reactor (R6); a seam trait with a simulator and
//! a conformance suite for hosts whose rules ask for one (Hex's §9, kiln's `kiln-seams`).

// The no-panic law (CLAUDE.md, banned item 6): shipped code never overflows or divides by zero. Test builds are exempt.
// The panic rule: a print macro panics on a refused write, and a library prints nothing: every outcome is returned.
#![deny(clippy::print_stdout, clippy::print_stderr)]
#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

pub mod error;
pub mod grant;
pub mod instance;
pub mod issuer;
pub mod mount;
pub mod role;
pub mod start;

pub use error::{Error, IssuerFault, MountFault, PathFault, StartFault};
pub use grant::{GrantAuthority, GrantClaim, GrantProof};
pub use instance::{Approved, Instance, LandingPlan, LandingRequest, Planned};
pub use issuer::{GRANT_TERM_NS, Issuer};
pub use mount::{MountOptions, Mounted};
pub use start::{Anchor, StartSpec, Started};

/// The crate's version: the workspace's one hand-edited version (§4.12 "One version"), which a host reports beside its
/// own.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The typed client and its own types, for the verbs [`Instance::client`] lends.
pub use slates_client::{
  Attachment, Client, ClientError, CreateSpec, Deadlines, ShardsStream, StreamEnd, VhostUserDevice,
};
/// The wire's types this crate's verbs take and return (§4.9; append-only, D-15).
pub use slates_ipc::protocol::{
  AuditEntry, ConflictClass, DaemonReport, Filter, GrantScope, GrantSummary, Intent,
  LandingOutcome, LandingSummary, NamePolicy, PlannedAction, PlannedContent, PlannedEntry, Refusal,
  SizeClass, SnapshotId, StatusReport, TelemetryReport, VolumeId, VolumeSummary,
};

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

/// A volume laid over the host directory `base` (§4.15, D-25): it reads every entry it has not changed from the
/// directory as the directory is now and keeps its own changes in memory, growing as it needs up to `max_bytes`
/// (§4.2's dynamic size class). Names are byte-exact (`NamePolicy::Exact`); a host that overlays a case-folding
/// directory sets `names` itself. Refused [`PathFault::NotAbsolute`] for a relative `base`, which the daemon would
/// resolve against its own working directory, and [`PathFault::NotUnicode`] for one the wire cannot carry.
pub fn overlay(name: &str, base: &std::path::Path, max_bytes: u64) -> Result<CreateSpec, Error> {
  let base = absolute_text(base)?;
  Ok(CreateSpec {
    name: name.to_owned(),
    size: SizeClass::Dynamic { max: max_bytes },
    names: NamePolicy::Exact,
    require_locked: false,
    base: Some(base),
  })
}

/// A scratch volume: no base, bounded at `limit_bytes` reserved at creation (§4.2's bounded size class).
pub fn scratch(name: &str, limit_bytes: u64) -> CreateSpec {
  CreateSpec {
    name: name.to_owned(),
    size: SizeClass::Bounded { limit: limit_bytes },
    names: NamePolicy::Exact,
    require_locked: false,
    base: None,
  }
}

/// A volume id as text: its sixteen bytes in hexadecimal, as `slates volume list` prints it and parses it back.
pub fn volume_id_text(id: VolumeId) -> String {
  id.bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `path` as the wire carries it: absolute, and Unicode.
pub(crate) fn absolute_text(path: &std::path::Path) -> Result<String, Error> {
  if !path.is_absolute() {
    return Err(Error::Path {
      path: path.to_string_lossy().into_owned(),
      fault: PathFault::NotAbsolute,
    });
  }
  path.to_str().map(str::to_owned).ok_or_else(|| Error::Path {
    path: path.to_string_lossy().into_owned(),
    fault: PathFault::NotUnicode,
  })
}
