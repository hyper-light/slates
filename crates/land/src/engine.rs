//! The landing state machine (§4.15 steps 2–11; D-26): `Planning → AwaitingGrant → Validating →
//! Writing → Syncing → Advancing → Done | Partial`, `Validating → Refused`, any state →
//! `Aborted`. One landing is one call of [`land`] over the write seam ([`LandFs`]) of the
//! volume crate; the seam is what the oracle replaces with the simulated host.
//!
//! What this module guarantees, and how:
//! - nothing is written before the grant that names the manifest's hash and the lease on the
//!   target: the grant check and the lease precede the capability probe, and the preliminary
//!   verdict pass that a `GrantRequired` refusal carries only reads;
//! - nothing is written while any entry's verdict is a conflict: validation runs over every
//!   entry first and a single conflict refuses the whole landing;
//! - every written entry is old or new, never torn: a file lands as a temporary that is
//!   written, given its mode and mtime, data-synced and only then linked at its name (a create)
//!   or exchanged with the old file (a replacement);
//! - a compare-and-swap lost to an outsider is undone and reported: after the exchange the
//!   file at the displaced name is opened and verified against the witness. An exchange's
//!   own ctime change requires a stable content hash; a mismatch exchanges back, removes
//!   the temporary and records `Undone(TargetInUse)`; a create whose
//!   name appeared meanwhile fails the link with `EEXIST` and records `Conflict(TargetInUse)`;
//! - nothing an outsider put at a name is removed in the witnessed entry's place (AUD-29-04): an entry
//!   is removed only as the object moved to one of this landing's own names (by the exchange, or by a
//!   rename that replaces nothing) and checked there; one that is not the witnessed object goes back
//!   without replacing anything, or is kept beside its name and reported (`Degradation::Kept`). The
//!   write seam offers no rename that replaces its destination;
//! - a re-run is idempotent: the plan is by hash, and an entry the disk already holds is a
//!   `Skip`, which advances without a write;
//! - the work is proportional to the delta: the only directories opened are the parents of the
//!   manifest's entries, the only files read are theirs, and the sweep of a crashed landing's
//!   siblings lists those parents only.
//!
//! Phase 1 shape (the design's Phase 2 and 4 pieces named where they replace this): entries
//! run one at a time, and the ramp records the depth the policy would have chosen; bytes are
//! read from the volume into a buffer and written through the seam (arena pages through
//! io_uring in Phase 4); grants, leases and the audit log are the in-process records of
//! [`crate::grant`] and [`Audit`]. Every temporary stays inside the granted directory; replacing
//! the target itself would write its ungranted parent (R1, §4.15 step 10). Reflinks are not used
//! (the probe is recorded as `false`).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use slates_vfs::base::{BaseConfig, Landed};
use slates_vfs::error::VfsError;
use slates_vfs::host::{HostDir, HostError, HostFile, HostKind, LandCapabilities, LandFs};
use slates_vfs::inode::{Fingerprint, Witness};
use slates_vfs::volume::{Store, Volume};

use crate::grant::{
  GrantBinding, GrantId, GrantRecord, GrantRefusal, GrantScope, Grants, LandingLease,
  LandingLeaseHeld, TargetIdentity, lease_key,
};
use crate::manifest::{Action, Filter, LandingEntry, Manifest, OverlayIdentity, Summary, plan};
use crate::ramp::{Ramp, StepSample};
pub use crate::source::Source;
use crate::verdict::{ConflictClass, DiskState, Verdict, verdict};

/// Format: the prefix of every hidden sibling a landing creates inside the granted target; the
/// landing id follows, then a per-landing counter, so a sweep recognizes its own names.
const HIDDEN_PREFIX: &str = ".slates-";
/// Format: the mark between a hidden name's landing id and the hash of the manifest path whose entry was
/// moved aside under it (AUD-29-04), so a sweep after a crash can check the entry before removing it.
const ASIDE_MARK: &str = "-aside-";
/// Format: the prefix of a name an entry moved aside is kept under when its own name was taken before it
/// could be put back: no sweep removes it (it is not this landing's hidden name), and the report names it.
const KEPT_PREFIX: &str = ".slates-kept-";
/// Format: the radix of the numbers in a hidden name (the landing id, the aside path hash), written `{:016x}`.
const HIDDEN_NAME_RADIX: u32 = 16;
/// Format: the leading bytes of a manifest path's BLAKE3 that an aside name carries: one `u64`.
const PATH_HASH_BYTES: usize = size_of::<u64>();
/// Format: `EEXIST` on Linux and macOS (the seam reports it as `Unavailable(17)`).
const ERRNO_EXIST: i32 = 17;
/// Format: `EINVAL` on Linux and macOS: a filesystem without `RENAME_EXCHANGE` reports it.
const ERRNO_INVAL: i32 = 22;
/// Format: `ENOTSUP` on macOS: a filesystem without `RENAME_SWAP` reports it.
const ERRNO_NOTSUP_MACOS: i32 = 45;
/// Format: `EOPNOTSUPP` on Linux (`ENOTSUP` is the same number there).
const ERRNO_NOTSUP_LINUX: i32 = 95;
/// Format: `ENOSPC` on Linux and macOS: the entry fails and the landing continues.
const ERRNO_NOSPC: i32 = 28;
/// Format: `EDQUOT` on Linux.
const ERRNO_DQUOT_LINUX: i32 = 122;
/// Format: `EDQUOT` on macOS.
const ERRNO_DQUOT_MACOS: i32 = 69;
/// Format: the mode of a temporary directory before the entry's own mode is applied: owner
/// only, so a half-built replacement is never readable by others.
const STAGE_MODE: u32 = 0o700;
/// Format: the read buffer for hashing a file the verdict must identify; one page's worth of
/// slots keeps the loop bounded per call (the design's chunk window is 64 KiB; hashing reads
/// are cold-path).
const HASH_READ_BYTES: usize = 65_536;

/// The landing's state (§4.15).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LandingState {
  /// Building the manifest.
  Planning,
  /// The manifest is presented; no grant covers it yet.
  AwaitingGrant,
  /// Lease taken; verdicts being computed.
  Validating,
  /// Entries being written by class.
  Writing,
  /// Directory syncs.
  Syncing,
  /// Witnesses advancing.
  Advancing,
  /// Every entry landed or was already there.
  Done,
  /// Some entries lost their compare-and-swap or failed; the rest landed.
  Partial,
  /// Nothing written: a conflict, a held lease, a missing or mismatched grant.
  Refused,
  /// Stopped by the host mid-landing; every written entry is old or new.
  Aborted,
}

/// Why an entry was skipped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
  /// The disk already holds what the overlay holds.
  AlreadyThere,
  /// The disk moved and the overlay did not change: nothing to land, drift reported.
  Drift,
  /// A previous entry's failure left its parent missing.
  ParentMissing,
  /// The grant expired or was revoked before this entry started.
  GrantEnded,
  /// The landing lease's term ended before this entry started: a holder paused past its term never writes
  /// on after another may hold the target (AUD-29-03).
  LeaseEnded,
}

/// What happened to one entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
  /// Landed.
  Written,
  /// Not written, on purpose.
  Skipped(SkipReason),
  /// The disk changed to the overlay's bytes on its own; accepted without a write.
  AcceptedIdentical,
  /// The host refused the write with this errno; the temporary was removed.
  Failed {
    /// The errno.
    errno: i32,
  },
  /// A compare-and-swap lost before any write reached the name.
  Conflict(ConflictClass),
  /// Written, then exchanged back because the displaced file was not the witnessed one.
  Undone(ConflictClass),
  /// The volume refused to give the entry's bytes.
  Volume(VfsError),
}

/// One entry of the report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryReport {
  /// The path.
  pub path: Box<str>,
  /// The action.
  pub action: Action,
  /// The verdict, once validated.
  pub verdict: Option<Verdict>,
  /// The outcome, once written.
  pub outcome: Option<Outcome>,
  /// The window the entry's name was absent in the exchange fallback (between moving the old entry aside
  /// and placing the new one), when that path was taken.
  pub window_ns: Option<u64>,
}

/// A Degraded cell the landing hit (the design's failure matrix).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Degradation {
  /// The filesystem lacks an atomic exchange; entries were written by moving the old entry aside and
  /// the new one in, never over anything, with the widest window a name was absent.
  NoExchange {
    /// The widest window, nanoseconds.
    widest_window_ns: u64,
  },
  /// Media durability was not requested (macOS barriers only).
  BarriersOnly,
  /// A crash interrupted the landing; the report lists what finished.
  Crashed {
    /// The errno.
    errno: i32,
  },
  /// A directory the landing touched could not be opened or synced: the entries named in it are on the
  /// disk but not known durable, so they stay in the volume's overlay, and a resume syncs the directory
  /// again before it advances them (§4.15 steps 8–9; AUD-29-05).
  Unsynced {
    /// The directory, by its path from the target.
    dir: Box<str>,
    /// What the host answered.
    error: HostError,
  },
  /// The media barrier the grant asked for failed: nothing this landing wrote is known to have reached the
  /// media, so no entry leaves the overlay (AUD-29-05). Barriers only, when media durability was not asked
  /// for, is [`Degradation::BarriersOnly`] instead: a supported level, not a failed one.
  MediaUnsynced {
    /// What the host answered.
    error: HostError,
  },
  /// An entry of the landing's own on the disk could not be removed — a temporary after a failed write,
  /// or a hidden sibling of an earlier attempt the sweep could not settle — and stays under its name,
  /// reported rather than hidden (AUD-29-05).
  Leftover {
    /// Its path from the target.
    path: Box<str>,
    /// What the host answered.
    error: HostError,
  },
  /// A directory the sweep could not list: hidden siblings an earlier attempt left in it, if any, stay
  /// there unseen (AUD-29-05).
  Unswept {
    /// The directory, by its path from the target.
    dir: Box<str>,
    /// What the host answered.
    error: HostError,
  },
  /// An entry the landing moved to one of its own names could not go back, because its name had been taken
  /// again meanwhile: it is kept at `kept`, never removed (AUD-29-04). It is an entry that was not the
  /// witnessed one, or the witnessed one whose replacement lost its name to an outsider inside the exchange
  /// fallback's window.
  Kept {
    /// The manifest path of the entry it was displaced by.
    path: Box<str>,
    /// The path it is kept at, beside where the landing had moved it.
    kept: Box<str>,
  },
}

/// Which durability the landing achieved (§4.15 step 8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Durability {
  /// Every written file's data sync ran before its link or exchange.
  pub data_synced: bool,
  /// Every touched directory was synced.
  pub dirs_synced: bool,
  /// The media barrier ran and held.
  pub media: bool,
  /// Whether the grant asked for media durability: without it, barriers only is the supported level
  /// (`BarriersOnly`), not a failed one (`MediaUnsynced`; AUD-29-05).
  pub media_requested: bool,
  /// Directories synced.
  pub dirs: usize,
  /// The total time in the directory syncs, nanoseconds (the per-directory cost the sync
  /// strategy derives from).
  pub dir_sync_ns: u64,
}

/// Costs measured by a landing, remembered by the caller per target filesystem for
/// throughput and sync-strategy measurements (§4.15 steps 7–8).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LandingCosts {
  /// Linking or renaming one entry into place, nanoseconds (median of the landing's samples).
  pub link_ns: u64,
  /// Writing one KiB, nanoseconds.
  pub write_ns_per_kib: u64,
  /// One exchange, nanoseconds.
  pub exchange_ns: u64,
  /// One verify (`fstat` of the displaced descriptor), nanoseconds.
  pub verify_ns: u64,
  /// One directory sync, nanoseconds.
  pub dir_sync_ns: u64,
  /// Samples behind the numbers.
  pub samples: u64,
}

/// The report of a landing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandingReport {
  /// The terminal state.
  pub state: LandingState,
  /// The manifest's hash.
  pub manifest_hash: [u8; 32],
  /// The manifest's summary.
  pub summary: Summary,
  /// Every entry.
  pub entries: Vec<EntryReport>,
  /// Written entries.
  pub written: usize,
  /// Entries skipped or accepted without a write.
  pub skipped: usize,
  /// Entries that lost their compare-and-swap.
  pub conflicts: usize,
  /// Entries the host refused.
  pub failed: usize,
  /// Entries on the disk that stay in the overlay because they did not reach the durability boundary: their
  /// directory, or the media barrier the grant asked for, did not sync (`degraded` says which). The resume
  /// syncs and advances them (AUD-29-05).
  pub held: usize,
  /// Bytes written to the disk.
  pub bytes_written: u64,
  /// The durability achieved.
  pub durability: Durability,
  /// The Degraded cells hit.
  pub degraded: Vec<Degradation>,
  /// The depth the concurrency ramp settled on (recorded; Phase 1 runs one entry at a time).
  pub ramp_depth: u32,
  /// The costs measured.
  pub costs: LandingCosts,
  /// Hidden siblings of an earlier landing swept before this one ran.
  pub swept: usize,
  /// The caller's target directory handle, whose identity the landing preserves.
  pub target: HostDir,
}

/// What a `GrantRequired` refusal carries for the confirmation surface (§4.15 step 2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Presented {
  /// The manifest.
  pub manifest: Manifest,
  /// The preliminary verdict pass (no writes).
  pub preliminary: Vec<EntryReport>,
  /// Who lands what where: the binding a grant for this presentation must carry (§4.13 "Grants").
  pub binding: GrantBinding,
}

/// Why a landing was refused before any write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LandingRefusal {
  /// No grant covers the manifest; the surface presents it (with the preliminary verdicts).
  GrantRequired(Box<Presented>),
  /// A grant exists but does not cover this manifest.
  Grant(GrantRefusal),
  /// Another session holds the landing lease on the target.
  LeaseHeld(LandingLeaseHeld),
  /// A granted landing came without a live landing lease on this target's canonical identity (the caller
  /// takes it from the target's one host-local owner before the engine runs; AUD-29-03): nothing was
  /// written.
  LeaseRequired,
  /// At least one entry's verdict is a conflict; nothing was written.
  Conflict(Vec<EntryReport>),
  /// The target could not be read.
  Target(HostError),
  /// The volume refused.
  Volume(VfsError),
}

/// What the audit log records (§4.15's `AuditKind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditKind {
  /// A manifest was planned.
  LandingPlanned,
  /// Verdicts were computed.
  LandingValidated,
  /// One entry landed.
  EntryWritten,
  /// One entry was refused or undone.
  EntryRefused,
  /// The landing reached a terminal state.
  LandingFinished,
}

/// One audit record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditRecord {
  /// Sequence number.
  pub seq: u64,
  /// Monotonic ns.
  pub at_ns: u64,
  /// The kind.
  pub kind: AuditKind,
  /// The grant, when one is bound.
  pub grant: Option<GrantId>,
  /// The landing.
  pub landing: u64,
  /// The manifest.
  pub manifest: [u8; 32],
  /// The terminal state, for `LandingFinished`.
  pub outcome: Option<LandingState>,
}

/// The in-process audit log: a bounded ring (database records from Phase 2).
#[derive(Debug)]
pub struct Audit {
  records: Vec<AuditRecord>,
  /// Derived: the retention the caller derives (landing rate × audit horizon, §4.15).
  retain: usize,
  next_seq: u64,
  head: usize,
}

impl Audit {
  /// An audit log keeping the latest `retain` records.
  pub fn new(retain: usize) -> Self {
    Self {
      records: Vec::new(),
      retain: retain.max(1),
      next_seq: 0,
      head: 0,
    }
  }

  fn push(&mut self, mut record: AuditRecord) {
    record.seq = self.next_seq;
    self.next_seq = self.next_seq.saturating_add(1);
    if self.records.len() < self.retain {
      self.records.push(record);
    } else {
      if let Some(slot) = self.records.get_mut(self.head) {
        *slot = record;
      }
      self.head = self
        .head
        .saturating_add(1)
        .checked_rem(self.retain)
        .unwrap_or(0);
    }
  }

  /// The records, oldest first.
  pub fn records(&self) -> Vec<&AuditRecord> {
    let (later, earlier) = self.records.split_at(self.head);
    earlier.iter().chain(later.iter()).collect()
  }
}

/// The target directory, opened by the caller with containment (§4.15 step 4): its handle,
/// and its canonical key (the lease's name). The grant conveys no authority over its parent.
#[derive(Clone, Debug)]
pub struct LandingTarget {
  /// The target directory.
  pub dir: HostDir,
  /// The canonical target the lease names.
  pub key: Box<str>,
}

/// One landing's request.
#[derive(Clone, Debug)]
pub struct LandingRequest {
  /// The landing's id (hidden siblings carry it).
  pub landing_id: u64,
  /// The consumer landing: its exact principal identity (the server's principal key), which a grant binds.
  pub consumer: Box<[u8]>,
  /// The volume landed, by its id.
  pub volume: [u8; 16],
  /// The snapshot landed, as the grant binds it (the catalog's number).
  pub snapshot: u64,
  /// What is landed (A-49): the head, or a snapshot of the volume exactly as it froze. The plan, the
  /// presentation, the verdicts' witnessed bases and the bytes written all come from it.
  pub source: Source,
  /// The grant, when the human issued one.
  pub grant: Option<GrantId>,
  /// The filter.
  pub filter: Filter,
  /// Now, monotonic ns.
  pub now_ns: u64,
  /// Whether the grant asked for media durability.
  pub media_durability: bool,
  /// Derived: the large-class boundary a scratch volume's new base uses (`BaseConfig`).
  pub large_class_bytes: u64,
  /// Measured: the landing pool's cores (the ramp's start).
  pub cores: u32,
  /// Derived: the most in-flight entries the pool can carry.
  pub max_depth: u32,
  /// Measured: the sample variance the ramp treats as noise, parts per thousand.
  pub variance_permille: u64,
}

/// What the caller may run before each entry's write: the oracle injects outsider edits here
/// (T-1.14). The default does nothing.
pub trait Observer<H: LandFs> {
  /// Called with the host before `entry` is written.
  fn before_write(&mut self, host: &mut H, entry: &LandingEntry);
  /// Called after each entry is processed (whatever its verdict), with the entry's monotonic start and
  /// end in the landing's clock (`now_ns` + elapsed). A telemetry observer records one `land.entry`
  /// span per entry from it (§4.14); the default does nothing, so a caller that does not observe (the
  /// oracle, the tests) is unaffected and stays wire-free.
  fn after_entry(&mut self, _start_ns: u64, _end_ns: u64) {}
}

/// The observer that does nothing.
#[derive(Clone, Copy, Debug, Default)]
pub struct Unobserved;

impl<H: LandFs> Observer<H> for Unobserved {
  fn before_write(&mut self, _host: &mut H, _entry: &LandingEntry) {}
}

/// A per-entry timing sample by kind, for the costs.
#[derive(Debug, Default)]
struct CostSamples {
  link_ns: Vec<u64>,
  write_ns_per_kib: Vec<u64>,
  exchange_ns: Vec<u64>,
  verify_ns: Vec<u64>,
  dir_sync_ns: Vec<u64>,
}

fn median(samples: &mut [u64]) -> u64 {
  if samples.is_empty() {
    return 0;
  }
  samples.sort_unstable();
  samples.get(samples.len() / 2).copied().unwrap_or(0)
}

impl CostSamples {
  fn fold(mut self) -> LandingCosts {
    let samples = u64::try_from(
      self
        .link_ns
        .len()
        .saturating_add(self.write_ns_per_kib.len())
        .saturating_add(self.exchange_ns.len())
        .saturating_add(self.verify_ns.len()),
    )
    .unwrap_or(u64::MAX);
    LandingCosts {
      link_ns: median(&mut self.link_ns),
      write_ns_per_kib: median(&mut self.write_ns_per_kib),
      exchange_ns: median(&mut self.exchange_ns),
      verify_ns: median(&mut self.verify_ns),
      dir_sync_ns: median(&mut self.dir_sync_ns),
      samples,
    }
  }
}

/// The landing in flight.
struct Landing<'a, H: LandFs> {
  host: &'a mut H,
  request: &'a LandingRequest,
  /// The granted target that every entry's path is relative to.
  root: HostDir,
  /// Directories opened beneath the root, by volume path (`"/"` is the root itself).
  dirs: BTreeMap<Box<str>, HostDir>,
  /// Directories a write touched (synced at the end).
  touched: BTreeSet<Box<str>>,
  /// Whether the exchange path is still believed to work (flipped by the first `EINVAL`).
  exchange: bool,
  /// Whether the filesystem gives unnamed temporaries (`O_TMPFILE`); where it does not, a temporary is
  /// named from its creation.
  unnamed_temporaries: bool,
  hidden_counter: u64,
  bytes_written: u64,
  widest_window_ns: u64,
  degraded: Vec<Degradation>,
  costs: CostSamples,
  ramp: Ramp,
  /// The host errno that ended the landing, when one did.
  crashed: Option<i32>,
  /// Touched directories that could not be opened or synced: entries in them are not advanced.
  unsynced: BTreeSet<Box<str>>,
  /// Whether the media barrier the grant asked for failed: then no entry is advanced.
  media_failed: bool,
  /// Each file this landing placed, by path, with the fingerprint its placement left: taken through the
  /// file's own descriptor after the last rename, so an outsider's later change to the name cannot be taken
  /// for it (A-49's rebase).
  placed: BTreeMap<Box<str>, Fingerprint>,
  /// Each replaced file this landing's sweep put back at its name after a crash, by path, with the
  /// fingerprint the put-back left. The rename moves the file's ctime and nothing else; the sweep had
  /// proved it the witnessed file just before (`aside_is_witnessed`), so that ctime is this landing's own
  /// and the entry's witness is judged with it ([`Landing::witness_of`]). Any change after the put-back
  /// moves the fingerprint again and is still a conflict. Until 2026-09-30 the resume took its own
  /// put-back for an outsider's edit and reported `ModifyModify` on every entry a no-exchange crash had
  /// set aside — hidden by a simulated clock that never moved.
  restored: BTreeMap<Box<str>, Fingerprint>,
}

/// What the writer needs besides the host: the volume, the grant, the observer, the audit log.
struct WriteContext<'x, H: LandFs> {
  vol: &'x mut Volume,
  store: &'x mut Store,
  grant: Option<&'x GrantRecord>,
  /// The landing lease every entry is fenced by.
  lease: &'x LandingLease,
  observer: &'x mut dyn Observer<H>,
  audit: &'x mut Audit,
}

/// The result of writing one entry.
struct Written {
  outcome: Outcome,
  window_ns: Option<u64>,
}

impl<H: LandFs> Landing<'_, H> {
  fn hidden_name(&mut self) -> Box<str> {
    let n = self.hidden_counter;
    // Saturating, not wrapping: a hidden name is created exclusively, so a repeat is refused, never reused.
    self.hidden_counter = self.hidden_counter.saturating_add(1);
    format!("{HIDDEN_PREFIX}{:016x}-{n}", self.request.landing_id).into()
  }

  fn is_own_hidden(&self, name: &str) -> bool {
    name.starts_with(&format!("{HIDDEN_PREFIX}{:016x}-", self.request.landing_id))
  }

  /// The name the entry at `path` is moved aside to while it is checked (AUD-29-04): this landing's hidden
  /// prefix, the aside mark and the path's hash. It is also the replacement temporary's name, since the
  /// exchange leaves the displaced entry under it.
  fn aside_name(&self, path: &str) -> Box<str> {
    format!(
      "{HIDDEN_PREFIX}{:016x}{ASIDE_MARK}{:016x}",
      self.request.landing_id,
      path_hash(path)
    )
    .into()
  }

  /// The path hash an aside name of this landing carries.
  fn aside_hash(&self, name: &str) -> Option<u64> {
    let prefix = format!(
      "{HIDDEN_PREFIX}{:016x}{ASIDE_MARK}",
      self.request.landing_id
    );
    u64::from_str_radix(name.strip_prefix(prefix.as_str())?, HIDDEN_NAME_RADIX).ok()
  }

  /// A name no sweep removes, for an entry moved aside that could not go back.
  fn kept_name(&mut self) -> Box<str> {
    let n = self.hidden_counter;
    self.hidden_counter = self.hidden_counter.saturating_add(1);
    format!("{KEPT_PREFIX}{:016x}-{n}", self.request.landing_id).into()
  }

  /// Puts the entry moved aside at `aside` under `aside_dir` (the directory at `aside_dir_path`) back at
  /// `name` under `dir`, never replacing whatever holds that name now; if the name was taken meanwhile, keeps
  /// the entry under a name no sweep removes and reports it (`Degradation::Kept`) for the entry at `path`.
  fn put_back(
    &mut self,
    aside_dir: HostDir,
    aside_dir_path: &str,
    aside: &str,
    dir: HostDir,
    name: &str,
    path: &str,
  ) -> Result<(), HostError> {
    match self.host.rename_noreplace(aside_dir, aside, dir, name) {
      Ok(()) => Ok(()),
      Err(HostError::Unavailable(ERRNO_EXIST)) => self.keep(aside_dir, aside_dir_path, aside, path),
      Err(e) => Err(e),
    }
  }

  /// Whether the entry moved aside at `aside` is the witnessed object: the witness's fingerprint in all but
  /// the ctime the move itself changes (the entry's full fingerprint was checked just before the move).
  /// `None` when nothing is there any more.
  fn aside_matches(
    &mut self,
    dir: HostDir,
    aside: &str,
    witness: &Witness,
  ) -> Result<Option<bool>, HostError> {
    match self.host.entry_fingerprint(dir, aside) {
      Ok(moved) => Ok(Some(
        Fingerprint {
          ctime_ns: witness.fingerprint.ctime_ns,
          ..moved
        } == witness.fingerprint,
      )),
      Err(HostError::NotFound) => Ok(None),
      Err(e) => Err(e),
    }
  }

  /// Opens (or finds) the directory at `path`, relative to the root; every opened handle is
  /// cached by path and closed at the terminal step.
  fn open_dir_path(&mut self, path: &str) -> Result<HostDir, HostError> {
    if path == "/" || path.is_empty() {
      return Ok(self.root);
    }
    if let Some(d) = self.dirs.get(path) {
      return Ok(*d);
    }
    let (parent, name) = split(path);
    let parent = self.open_dir_path(parent)?;
    let opened = self.host.open_dir(parent, name)?;
    self.dirs.insert(path.into(), opened);
    Ok(opened)
  }

  /// Forgets a cached directory beneath `path` (after it was removed or renamed).
  fn forget_dirs_under(&mut self, path: &str) {
    let stale: Vec<Box<str>> = self
      .dirs
      .keys()
      .filter(|k| k.as_ref() == path || k.starts_with(&format!("{path}/")))
      .cloned()
      .collect();
    for k in stale {
      if let Some(d) = self.dirs.remove(&k) {
        self.host.close_dir(d);
      }
    }
  }

  // ------------------------------------------------------------- validation

  /// What the disk holds at `path`, hashing the bytes only when `hash` asks for it, and for a
  /// directory checking against `ours` (the paths this manifest creates) when given.
  fn disk_state(
    &mut self,
    path: &str,
    hash: bool,
    ours: Option<&BTreeSet<String>>,
  ) -> Result<DiskState, HostError> {
    let (dir_path, name) = split(path);
    let dir = match self.open_dir_path(dir_path) {
      Ok(d) => d,
      Err(HostError::NotFound | HostError::NotDirectory | HostError::NotFile) => {
        return Ok(DiskState::Absent);
      }
      Err(e) => return Err(e),
    };
    match self.host.open_file(dir, name) {
      Ok(file) => {
        let fingerprint = self.host.fstat(file)?;
        let identity = if hash {
          Some(self.hash_file(file, fingerprint.size)?)
        } else {
          None
        };
        self.host.close_file(file);
        Ok(DiskState::File {
          fingerprint,
          identity,
        })
      }
      Err(HostError::NotFound) => Ok(DiskState::Absent),
      Err(HostError::NotFile) => self.disk_state_not_file(dir, name, path, ours),
      Err(e) => Err(e),
    }
  }

  fn disk_state_not_file(
    &mut self,
    dir: HostDir,
    name: &str,
    path: &str,
    ours: Option<&BTreeSet<String>>,
  ) -> Result<DiskState, HostError> {
    match self.host.open_dir(dir, name) {
      Ok(d) => {
        let fingerprint = self.host.fingerprint_dir(d);
        let holds_only_ours = match ours {
          Some(ours) => self
            .host
            .list(d)
            .map(|entries| entries.iter().all(|e| ours.contains(&join(path, &e.name)))),
          None => Ok(false),
        };
        self.host.close_dir(d);
        Ok(DiskState::Dir {
          fingerprint: fingerprint?,
          holds_only_ours: holds_only_ours?,
        })
      }
      Err(HostError::NotDirectory | HostError::NotFile) => Ok(DiskState::Symlink),
      Err(HostError::NotFound) => Ok(DiskState::Absent),
      Err(e) => Err(e),
    }
  }

  /// The disk for a directory rename: the origin's state, or, when the origin is gone, the
  /// destination's as `AtDestination` (a resumed landing meets its finished move).
  fn rename_state(&mut self, from: &str, to: &str) -> Result<DiskState, HostError> {
    match self.disk_state(from, false, None)? {
      DiskState::Absent => Ok(match self.disk_state(to, false, None)? {
        DiskState::Dir { fingerprint, .. } => DiskState::AtDestination(fingerprint),
        other => other,
      }),
      origin => Ok(origin),
    }
  }

  /// Hash at most the captured length, so an outsider growing the file cannot extend the work.
  fn hash_file(&mut self, file: HostFile, length: u64) -> Result<[u8; 32], HostError> {
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; HASH_READ_BYTES];
    let mut off = 0u64;
    while off < length {
      let take = usize::try_from(length.saturating_sub(off))
        .unwrap_or(buf.len())
        .min(buf.len());
      let n = self
        .host
        .read_at(file, off, buf.get_mut(..take).unwrap_or_default())?;
      if n == 0 {
        break;
      }
      hasher.update(buf.get(..n).unwrap_or_default());
      off = off.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
    }
    Ok(*hasher.finalize().as_bytes())
  }

  /// Whether the verdict for `entry` needs the disk's bytes hashed: a create over an existing
  /// file always (same bytes or a conflict), a replacement only when the fingerprint moved
  /// from the witness or the witness was racy.
  /// The witness an entry is judged against: its own, or — for a replaced file this landing's sweep put
  /// back — the same witness with the ctime the put-back left, when that is the only difference
  /// ([`Landing::restored`]).
  fn witness_of(&self, entry: &LandingEntry) -> Option<Witness> {
    let witness = entry.witnessed?;
    let restored = self
      .restored
      .get(&entry.path)
      .filter(|after| {
        Fingerprint {
          ctime_ns: witness.fingerprint.ctime_ns,
          ..**after
        } == witness.fingerprint
      })
      .copied();
    Some(match restored {
      Some(after) => Witness {
        fingerprint: after,
        ..witness
      },
      None => witness,
    })
  }

  fn needs_hash(&mut self, entry: &LandingEntry) -> Result<bool, HostError> {
    let witness = self.witness_of(entry);
    match (&entry.action, witness.as_ref()) {
      (Action::Create, _) | (Action::Replace, None) => Ok(true),
      (Action::Replace, Some(w)) => {
        if w.racy {
          return Ok(true);
        }
        let (dir_path, name) = split(&entry.path);
        let Ok(dir) = self.open_dir_path(dir_path) else {
          return Ok(false);
        };
        match self.host.open_file(dir, name) {
          Ok(f) => {
            let fp = self.host.fstat(f);
            self.host.close_file(f);
            Ok(fp? != w.fingerprint)
          }
          Err(_) => Ok(false),
        }
      }
      _ => Ok(false),
    }
  }

  /// The verdict pass over every entry (no writes).
  fn validate(&mut self, manifest: &Manifest) -> Result<Vec<EntryReport>, HostError> {
    let mut out = Vec::with_capacity(manifest.entries.len());
    // The paths this manifest creates, for a resumed clear to recognize its own directory.
    let ours: BTreeSet<String> = manifest
      .entries
      .iter()
      .filter(|e| {
        matches!(
          e.action,
          Action::Create | Action::Mkdir | Action::Symlink { .. } | Action::Clear
        )
      })
      .map(|e| e.path.to_string())
      .collect();
    for entry in &manifest.entries {
      let hash = self.needs_hash(entry)?;
      // Every entry's directory is synced at the end, written or not, so a resumed landing
      // makes the previous attempt's entries durable too.
      self.touched.insert(split(&entry.path).0.into());
      let check_ours = matches!(entry.action, Action::Clear).then_some(&ours);
      let disk = match &entry.action {
        Action::Rename { from } => {
          self.touched.insert(split(from).0.into());
          self.rename_state(from, &entry.path)?
        }
        _ => self.disk_state(&entry.path, hash, check_ours)?,
      };
      let witness = self.witness_of(entry);
      let v = verdict(
        &entry.action,
        witness.as_ref(),
        disk,
        entry.overlay.as_ref(),
      );
      out.push(EntryReport {
        path: entry.path.clone(),
        action: entry.action.clone(),
        verdict: Some(v),
        outcome: None,
        window_ns: None,
      });
    }
    Ok(out)
  }

  // ------------------------------------------------------------- writing

  /// Writes every `Apply` entry in manifest order (already by class), timing each class as one
  /// ramp step. Stops at a host crash; skips entries after the grant ended.
  fn write_all(
    &mut self,
    manifest: &Manifest,
    reports: &mut [EntryReport],
    cx: &mut WriteContext<'_, H>,
  ) {
    let WriteContext {
      vol,
      store,
      grant,
      lease,
      observer,
      audit,
    } = cx;
    let grant = *grant;
    let lease = *lease;
    let started = Instant::now();
    let mut class = None;
    let mut step_started = Instant::now();
    let mut step_entries = 0u64;
    let mut step_max_ns = 0u64;
    for (entry, report) in manifest.entries.iter().zip(reports.iter_mut()) {
      if self.crashed.is_some() {
        break;
      }
      if class != Some(entry.class()) {
        if class.is_some() {
          self.ramp.observe(StepSample {
            entries: step_entries,
            wall_ns: elapsed_ns(step_started),
            p99_ns: step_max_ns,
          });
        }
        class = Some(entry.class());
        step_started = Instant::now();
        step_entries = 0;
        step_max_ns = 0;
      }
      // The `land.entry` chokepoint span (§4.14) brackets one entry's processing, whatever its verdict.
      let entry_start_ns = self.request.now_ns.saturating_add(elapsed_ns(started));
      let outcome = match report.verdict {
        Some(Verdict::Apply) => {
          let now_ns = self.request.now_ns.saturating_add(elapsed_ns(started));
          if grant_ended(grant, now_ns) {
            Written {
              outcome: Outcome::Skipped(SkipReason::GrantEnded),
              window_ns: None,
            }
          } else if lease.expires_ns <= now_ns {
            Written {
              outcome: Outcome::Skipped(SkipReason::LeaseEnded),
              window_ns: None,
            }
          } else {
            observer.before_write(self.host, entry);
            let entry_started = Instant::now();
            let w = self.write_entry(entry, vol, store);
            step_entries = step_entries.saturating_add(1);
            step_max_ns = step_max_ns.max(elapsed_ns(entry_started));
            w
          }
        }
        Some(Verdict::Skip) => Written {
          outcome: Outcome::Skipped(skip_reason(entry)),
          window_ns: None,
        },
        Some(Verdict::AcceptIdentical) => Written {
          outcome: Outcome::AcceptedIdentical,
          window_ns: None,
        },
        Some(Verdict::Conflict(c)) => Written {
          outcome: Outcome::Conflict(c),
          window_ns: None,
        },
        None => continue,
      };
      observer.after_entry(
        entry_start_ns,
        self.request.now_ns.saturating_add(elapsed_ns(started)),
      );
      audit.push(AuditRecord {
        seq: 0,
        at_ns: self.request.now_ns.saturating_add(elapsed_ns(started)),
        kind: if outcome.outcome == Outcome::Written {
          AuditKind::EntryWritten
        } else {
          AuditKind::EntryRefused
        },
        grant: grant.map(|g| g.id),
        landing: self.request.landing_id,
        manifest: manifest.hash,
        outcome: None,
      });
      report.outcome = Some(outcome.outcome);
      report.window_ns = outcome.window_ns;
    }
    if class.is_some() {
      self.ramp.observe(StepSample {
        entries: step_entries,
        wall_ns: elapsed_ns(step_started),
        p99_ns: step_max_ns,
      });
    }
  }

  /// Writes one entry, mapping a host refusal to its outcome: a full disk fails the entry, any
  /// other unavailability ends the landing (the target is gone or the disk is).
  fn write_entry(&mut self, entry: &LandingEntry, vol: &mut Volume, store: &mut Store) -> Written {
    match self.try_write_entry(entry, vol, store) {
      Ok(w) => w,
      Err(WriteFailure::Host(HostError::Unavailable(errno))) if disk_full(errno) => Written {
        outcome: Outcome::Failed { errno },
        window_ns: None,
      },
      Err(WriteFailure::Host(HostError::Unavailable(errno))) => {
        self.crashed = Some(errno);
        Written {
          outcome: Outcome::Failed { errno },
          window_ns: None,
        }
      }
      Err(WriteFailure::Host(HostError::NotFound | HostError::NotDirectory)) => Written {
        outcome: Outcome::Skipped(SkipReason::ParentMissing),
        window_ns: None,
      },
      Err(WriteFailure::Host(HostError::NotFile | HostError::StaleHandle)) => Written {
        outcome: Outcome::Conflict(ConflictClass::TypeChanged),
        window_ns: None,
      },
      Err(WriteFailure::Volume(e)) => Written {
        outcome: Outcome::Volume(e),
        window_ns: None,
      },
    }
  }

  fn try_write_entry(
    &mut self,
    entry: &LandingEntry,
    vol: &mut Volume,
    store: &mut Store,
  ) -> Result<Written, WriteFailure> {
    let (dir_path, name) = split(&entry.path);
    let outcome = match &entry.action {
      Action::Mkdir => {
        let dir = self.open_dir_path(dir_path)?;
        let mode = entry.overlay.map_or(STAGE_MODE, |o| o.mode);
        self.touched.insert(dir_path.into());
        match self.host.mkdir(dir, name, mode) {
          Ok(()) => Outcome::Written,
          Err(HostError::Unavailable(ERRNO_EXIST)) => self.existing_directory(dir, name)?,
          Err(e) => return Err(e.into()),
        }
      }
      Action::Symlink { target } => {
        let dir = self.open_dir_path(dir_path)?;
        self.touched.insert(dir_path.into());
        match self.host.symlink(dir, name, target) {
          Ok(()) => Outcome::Written,
          Err(HostError::Unavailable(ERRNO_EXIST)) => Outcome::Conflict(ConflictClass::TargetInUse),
          Err(e) => return Err(e.into()),
        }
      }
      Action::Create => return self.write_file(entry, vol, store, None),
      Action::Replace => {
        let witness = self.witness_of(entry);
        return self.write_file(entry, vol, store, witness.as_ref());
      }
      Action::Delete => self.delete(dir_path, name, entry.witnessed.as_ref())?,
      Action::Rmdir => self.remove_tree_entry(dir_path, name, entry.witnessed.as_ref())?,
      Action::Clear => return self.clear_entry(dir_path, name, entry),
      Action::Rename { from } => self.rename_dir(from, dir_path, name, entry.witnessed.as_ref())?,
    };
    Ok(Written {
      outcome,
      window_ns: None,
    })
  }

  /// A mkdir that found its name taken: already there only when what holds the name is a directory. One
  /// that met a file an outsider made is a type conflict, so it is never advanced over that file with the
  /// private work beneath it (AUD-29-05; before 2026-09-29 any `EEXIST` counted as already there).
  fn existing_directory(&mut self, dir: HostDir, name: &str) -> Result<Outcome, WriteFailure> {
    match self.host.entry_fingerprint(dir, name) {
      Ok(there) if crate::manifest::kind_is_dir(there.mode) => {
        Ok(Outcome::Skipped(SkipReason::AlreadyThere))
      }
      Ok(_) => Ok(Outcome::Conflict(ConflictClass::TypeChanged)),
      Err(HostError::NotFound) => Ok(Outcome::Conflict(ConflictClass::TargetInUse)),
      Err(e) => Err(e.into()),
    }
  }

  /// Writes a file: temporary, bytes, mode, mtime, data sync, then link at the name (a create)
  /// or exchange with the old file and verify the displaced one (a replacement).
  fn write_file(
    &mut self,
    entry: &LandingEntry,
    vol: &mut Volume,
    store: &mut Store,
    witnessed: Option<&Witness>,
  ) -> Result<Written, WriteFailure> {
    let (dir_path, name) = split(&entry.path);
    let overlay = entry
      .overlay
      .ok_or(WriteFailure::Volume(VfsError::Invalid))?;
    let bytes = read_overlay_bytes(vol, store, self.host, self.request.source, &entry.path)?;
    let dir = self.open_dir_path(dir_path)?;
    self.touched.insert(dir_path.into());
    // Every temporary is created at a plain hidden name, the landing's own by its name: a crash while it is
    // being written leaves it there, and the sweep removes it. A replacement's complete temporary then takes
    // the entry's aside name, where the exchange leaves the displaced entry and a sweep after a crash checks
    // it before removing it (AUD-29-04). Before 2026-09-30 a replacement's temporary was created at its
    // aside name, where a crash mid-write left bytes that matched neither the witness nor the overlay, and
    // the resume kept them (a named-temporary filesystem: APFS).
    let created = self.hidden_name();
    let temp = self.fill_temp(dir, dir_path, &created, &bytes, overlay)?;
    let result = match witnessed {
      None => self.link_create(temp, dir, &created, name),
      Some(witness) => {
        let aside = self.aside_name(&entry.path);
        self.swap_replace(Swap {
          temp,
          dir,
          created: &created,
          hidden: &aside,
          name,
          path: &entry.path,
          witness,
        })
      }
    };
    // What the placement left at the name, read through the file's own descriptor before it closes.
    if let Ok(w) = &result
      && w.outcome == Outcome::Written
      && let Ok(placed) = self.host.fstat(temp)
    {
      self.placed.insert(entry.path.clone(), placed);
    }
    self.host.close_file(temp);
    match result {
      Ok(w) => {
        if w.outcome == Outcome::Written {
          self.bytes_written = self.bytes_written.saturating_add(overlay.size);
        }
        Ok(w)
      }
      // After a failed exchange/undo, the hidden name may hold the displaced file. Leave
      // it for recovery; unlinking it here could discard the original or an outsider's data.
      Err(e) => Err(e),
    }
  }

  /// The temporary with its bytes, mode, mtime and data sync, at its hidden name.
  fn fill_temp(
    &mut self,
    dir: HostDir,
    dir_path: &str,
    hidden: &str,
    bytes: &[u8],
    overlay: OverlayIdentity,
  ) -> Result<HostFile, WriteFailure> {
    let temp = self.host.create_temp(dir, hidden)?;
    let started = Instant::now();
    let filled = self
      .host
      .write_at(temp, 0, bytes)
      .and_then(|()| self.host.set_mode(temp, overlay.mode))
      .and_then(|()| self.host.set_mtime(temp, overlay.mtime_ns))
      .and_then(|()| self.host.sync_file(temp));
    if let Err(e) = filled {
      self.host.close_file(temp);
      // An unnamed temporary has no name to remove (`NotFound`); a named one that stays is reported, never
      // dropped (AUD-29-05).
      match self.host.unlink(dir, hidden) {
        Ok(()) | Err(HostError::NotFound) => {}
        Err(error) => self.degraded.push(Degradation::Leftover {
          path: join(dir_path, hidden).into(),
          error,
        }),
      }
      return Err(e.into());
    }
    /// Format: bytes per KiB.
    const KIB: u64 = 1024;
    let kib = u64::try_from(bytes.len())
      .unwrap_or(u64::MAX)
      .div_ceil(KIB)
      .max(1);
    self
      .costs
      .write_ns_per_kib
      .push(elapsed_ns(started).checked_div(kib).unwrap_or(0));
    Ok(temp)
  }

  /// A create: link the temporary at the name; a name that appeared meanwhile is a conflict.
  fn link_create(
    &mut self,
    temp: HostFile,
    dir: HostDir,
    hidden: &str,
    name: &str,
  ) -> Result<Written, WriteFailure> {
    let started = Instant::now();
    let placed = self.host.place(temp, dir, name);
    self.costs.link_ns.push(elapsed_ns(started));
    let outcome = match placed {
      Ok(()) => Outcome::Written,
      Err(HostError::Unavailable(ERRNO_EXIST)) => Outcome::Conflict(ConflictClass::TargetInUse),
      Err(e) => return Err(e.into()),
    };
    // A named temporary still hangs at its hidden name; an unnamed one never had it (a host
    // that fell back to a name for this one file is covered by the same unlink).
    match self.host.unlink(dir, hidden) {
      Ok(()) | Err(HostError::NotFound) => {}
      Err(e) => return Err(e.into()),
    }
    Ok(Written {
      outcome,
      window_ns: None,
    })
  }

  /// A replacement: place the temporary at its hidden name, hold the old file, exchange, and
  /// verify the displaced file is the witnessed one; else exchange back.
  fn swap_replace(&mut self, swap: Swap<'_>) -> Result<Written, WriteFailure> {
    // The complete temporary takes its aside name (a link), then leaves its creation name if it had one.
    self.host.place(swap.temp, swap.dir, swap.hidden)?;
    if !self.unnamed_temporaries {
      match self.host.unlink(swap.dir, swap.created) {
        Ok(()) | Err(HostError::NotFound) => {}
        Err(e) => return Err(e.into()),
      }
    }
    let old = match self.host.open_file(swap.dir, swap.name) {
      Ok(f) => f,
      Err(HostError::NotFound | HostError::NotFile) => {
        self.host.unlink(swap.dir, swap.hidden)?;
        return Ok(Written {
          outcome: Outcome::Conflict(ConflictClass::TargetInUse),
          window_ns: None,
        });
      }
      Err(e) => return Err(e.into()),
    };
    let result = if self.exchange {
      self.exchange_and_verify(old, swap)
    } else {
      self.replace_by_renames(old, swap)
    };
    self.host.close_file(old);
    result
  }

  fn exchange_and_verify(
    &mut self,
    old: HostFile,
    swap: Swap<'_>,
  ) -> Result<Written, WriteFailure> {
    let Swap {
      dir,
      hidden,
      name,
      witness: w,
      ..
    } = swap;
    // Only our exchange may explain a changed ctime. An earlier metadata change is still
    // a witness conflict, even when the outsider left the same mode and content behind.
    let before = self.host.fstat(old)?;
    let started = Instant::now();
    match self.host.exchange(dir, hidden, name) {
      Ok(()) => {}
      Err(HostError::Unavailable(errno)) if no_exchange(errno) => {
        self.exchange = false;
        return self.replace_by_renames(old, swap);
      }
      Err(e) => return Err(e.into()),
    }
    self.costs.exchange_ns.push(elapsed_ns(started));
    let verify_started = Instant::now();
    let verified = if before == w.fingerprint {
      self.verify_displaced(dir, hidden, w)
    } else {
      Ok(false)
    };
    self.costs.verify_ns.push(elapsed_ns(verify_started));
    if matches!(verified, Ok(true)) {
      self.host.unlink(dir, hidden)?;
      return Ok(Written {
        outcome: Outcome::Written,
        window_ns: None,
      });
    }
    // Lost to an outsider: the old file goes back, and the temporary goes once it is checked to be ours — a
    // second outsider edit between the two exchanges leaves its own file there instead (AUD-29-04).
    self.host.exchange(dir, hidden, name)?;
    self.remove_own_temp(swap)?;
    verified?;
    Ok(Written {
      outcome: Outcome::Undone(ConflictClass::TargetInUse),
      window_ns: None,
    })
  }

  /// Removes the replacement's temporary from its hidden name once the entry there is checked to be it: the
  /// open descriptor holds the temporary's inode, so the identity is exact. Another entry there is kept under
  /// a name no sweep removes and reported (`Degradation::Kept`), never removed.
  fn remove_own_temp(&mut self, swap: Swap<'_>) -> Result<(), WriteFailure> {
    let ours = self.host.fstat(swap.temp)?;
    match self.host.entry_fingerprint(swap.dir, swap.hidden) {
      Ok(there) if there.dev == ours.dev && there.ino == ours.ino => {
        self.host.unlink(swap.dir, swap.hidden)?;
      }
      Ok(_) => self.keep(swap.dir, split(swap.path).0, swap.hidden, swap.path)?,
      Err(HostError::NotFound) => {}
      Err(e) => return Err(e.into()),
    }
    Ok(())
  }

  /// Keeps the entry at `hidden` under `dir` (the directory at `dir_path`) under a name no sweep removes,
  /// reported as `Degradation::Kept` for the entry at `path`.
  fn keep(
    &mut self,
    dir: HostDir,
    dir_path: &str,
    hidden: &str,
    path: &str,
  ) -> Result<(), HostError> {
    let kept = self.kept_name();
    self.host.rename_noreplace(dir, hidden, dir, &kept)?;
    self.degraded.push(Degradation::Kept {
      path: path.into(),
      kept: join(dir_path, &kept).into(),
    });
    Ok(())
  }

  /// Open what the exchange actually displaced: a pre-exchange descriptor can still point
  /// at an inode an outsider already replaced. Always close this new descriptor on refusal.
  fn verify_displaced(
    &mut self,
    dir: HostDir,
    hidden: &str,
    witness: &Witness,
  ) -> Result<bool, HostError> {
    let file = match self.host.open_file(dir, hidden) {
      Ok(file) => file,
      Err(HostError::NotFound | HostError::NotFile) => return Ok(false),
      Err(error) => return Err(error),
    };
    let result = self.displaced_matches(file, witness);
    self.host.close_file(file);
    result
  }

  /// An exchange can change ctime itself. All other fields must match; a changed ctime
  /// or racy witness additionally requires the witnessed bytes and a stable read (§4.15).
  fn displaced_matches(&mut self, file: HostFile, witness: &Witness) -> Result<bool, HostError> {
    let before = self.host.fstat(file)?;
    let preserved = Fingerprint {
      ctime_ns: witness.fingerprint.ctime_ns,
      ..before
    };
    if preserved != witness.fingerprint {
      return Ok(false);
    }
    if before == witness.fingerprint && !witness.racy {
      return Ok(true);
    }
    let identity = self.hash_file(file, before.size)?;
    let after = self.host.fstat(file)?;
    Ok(identity == witness.identity && after == before)
  }

  /// The exchange fallback (Degraded; AUD-29-04). The old file must match the witness through the descriptor
  /// held on it; it is then moved aside without replacing anything and checked where it went (as the
  /// exchange's displaced file is), the temporary takes the name without replacing anything, and only then
  /// is the checked file removed. The name is absent between the two renames: that window is measured and
  /// reported, and a crash inside it leaves the old file aside for the resume's sweep to put back. Before
  /// 2026-09-29 the temporary was renamed over the name after the check, which removed whatever an outsider
  /// had put there in between.
  fn replace_by_renames(&mut self, old: HostFile, swap: Swap<'_>) -> Result<Written, WriteFailure> {
    let Swap {
      dir,
      hidden,
      name,
      path,
      witness: w,
      ..
    } = swap;
    let verify_started = Instant::now();
    let current = self.host.fstat(old)?;
    self.costs.verify_ns.push(elapsed_ns(verify_started));
    if current != w.fingerprint {
      self.host.unlink(dir, hidden)?;
      return Ok(Written {
        outcome: Outcome::Conflict(ConflictClass::TargetInUse),
        window_ns: None,
      });
    }
    // The temporary takes a plain hidden name, freeing the aside name for the old file.
    let staged = self.hidden_name();
    self.host.rename_noreplace(dir, hidden, dir, &staged)?;
    let aside = self.aside_name(path);
    let window_started = Instant::now();
    if let Some(outcome) = self.set_file_aside(dir, name, &aside, path, w)? {
      self.host.unlink(dir, &staged)?;
      return Ok(Written {
        outcome,
        window_ns: None,
      });
    }
    if let Err(e) = self.host.rename_noreplace(dir, &staged, dir, name) {
      // The name was made again inside the window (`EEXIST`), or the host refused: the old file goes back,
      // kept beside the name when an outsider holds it, and the temporary goes.
      self.put_back(dir, split(path).0, &aside, dir, name, path)?;
      self.host.unlink(dir, &staged)?;
      return match e {
        HostError::Unavailable(ERRNO_EXIST) => Ok(Written {
          outcome: Outcome::Undone(ConflictClass::TargetInUse),
          window_ns: None,
        }),
        e => Err(e.into()),
      };
    }
    let window_ns = elapsed_ns(window_started);
    self.costs.link_ns.push(window_ns);
    self.widest_window_ns = self.widest_window_ns.max(window_ns);
    self.host.unlink(dir, &aside)?;
    Ok(Written {
      outcome: Outcome::Written,
      window_ns: Some(window_ns),
    })
  }

  /// The fallback's old file, moved aside to `aside` and checked there: `None` when it is the witnessed file,
  /// else the outcome the replacement ends with — nothing was there, or another entry, which went back.
  fn set_file_aside(
    &mut self,
    dir: HostDir,
    name: &str,
    aside: &str,
    path: &str,
    witness: &Witness,
  ) -> Result<Option<Outcome>, WriteFailure> {
    match self.host.rename_noreplace(dir, name, dir, aside) {
      Ok(()) => {}
      Err(HostError::NotFound) => return Ok(Some(Outcome::Conflict(ConflictClass::TargetInUse))),
      Err(e) => return Err(e.into()),
    }
    if self.verify_displaced(dir, aside, witness)? {
      return Ok(None);
    }
    self.put_back(dir, split(path).0, aside, dir, name, path)?;
    Ok(Some(Outcome::Undone(ConflictClass::TargetInUse)))
  }

  /// A delete of a file, symlink or other entry (AUD-29-04): the entry must be the witnessed one as it
  /// stands, and it is removed only as the object moved aside in one step and checked there, so an
  /// outsider's replacement at any moment is never removed in its place. One that is not the witnessed
  /// object goes back where it was, never over a newer entry. Before 2026-09-29 the file was checked through
  /// a descriptor that was then closed and the name unlinked, so a replacement in between was removed, and
  /// a symlink or other entry was unlinked unchecked.
  fn delete(
    &mut self,
    dir_path: &str,
    name: &str,
    witnessed: Option<&Witness>,
  ) -> Result<Outcome, WriteFailure> {
    let dir = self.open_dir_path(dir_path)?;
    self.touched.insert(dir_path.into());
    let current = match self.host.entry_fingerprint(dir, name) {
      Ok(current) => current,
      Err(HostError::NotFound) => return Ok(Outcome::Skipped(SkipReason::AlreadyThere)),
      Err(e) => return Err(e.into()),
    };
    let Some(witness) = witnessed.filter(|w| w.fingerprint == current) else {
      return Ok(Outcome::Conflict(ConflictClass::TargetInUse));
    };
    let path = join(dir_path, name);
    let aside = self.aside_name(&path);
    match self.host.rename_noreplace(dir, name, dir, &aside) {
      Ok(()) => {}
      Err(HostError::NotFound) => return Ok(Outcome::Skipped(SkipReason::AlreadyThere)),
      Err(e) => return Err(e.into()),
    }
    match self.aside_matches(dir, &aside, witness)? {
      Some(true) => {
        self.host.unlink(dir, &aside)?;
        Ok(Outcome::Written)
      }
      Some(false) => {
        self.put_back(dir, split(&path).0, &aside, dir, name, &path)?;
        Ok(Outcome::Undone(ConflictClass::TargetInUse))
      }
      None => Ok(Outcome::Conflict(ConflictClass::TargetInUse)),
    }
  }

  /// A recursive removal: the directory's inode must be the witnessed one; then the directory is renamed to
  /// this landing's aside name in one step (the name is old or gone, never half-removed), checked there, and
  /// the hidden tree is removed bottom-up; a crash leaves it for the sweep.
  fn remove_tree_entry(
    &mut self,
    dir_path: &str,
    name: &str,
    witnessed: Option<&Witness>,
  ) -> Result<Outcome, WriteFailure> {
    let dir = self.open_dir_path(dir_path)?;
    self.touched.insert(dir_path.into());
    match self.set_aside(dir, dir_path, name, witnessed)? {
      Ok(aside) => {
        self.remove_named_tree(dir, &aside)?;
        Ok(Outcome::Written)
      }
      Err(outcome) => Ok(outcome),
    }
  }

  /// A clear: a fresh directory (the overlay's mode) is made beside the old one and exchanged
  /// with it in one step, so the name always holds a directory, old or new; the displaced
  /// tree is verified to be the witnessed one (else exchanged back) and then removed. Without
  /// an exchange, two renames with the window measured (the Degraded cell).
  fn clear_entry(
    &mut self,
    dir_path: &str,
    name: &str,
    entry: &LandingEntry,
  ) -> Result<Written, WriteFailure> {
    let dir = self.open_dir_path(dir_path)?;
    self.touched.insert(dir_path.into());
    let mode = entry.overlay.map_or(STAGE_MODE, |o| o.mode);
    match self.verify_dir(dir, name, entry.witnessed.as_ref())? {
      None => {
        let outcome = match self.host.mkdir(dir, name, mode) {
          Ok(()) => Outcome::Written,
          Err(HostError::Unavailable(ERRNO_EXIST)) => Outcome::Conflict(ConflictClass::TargetInUse),
          Err(e) => return Err(e.into()),
        };
        return Ok(Written {
          outcome,
          window_ns: None,
        });
      }
      Some(false) => {
        return Ok(Written {
          outcome: Outcome::Conflict(ConflictClass::TargetInUse),
          window_ns: None,
        });
      }
      Some(true) => {}
    }
    let path = join(dir_path, name);
    self.forget_dirs_under(&path);
    // The fresh directory takes the entry's aside name: the exchange leaves the displaced directory under
    // it, and a sweep after a crash checks it before removing it (AUD-29-04).
    let fresh = self.aside_name(&path);
    self.host.mkdir(dir, &fresh, mode)?;
    // The fresh directory's identity, taken while only this landing's hidden name holds it.
    let made = self.host.entry_fingerprint(dir, &fresh)?;
    if !self.exchange_names(dir, &fresh, name)? {
      return self.clear_by_renames(dir, &path, &fresh, name, entry.witnessed.as_ref());
    }
    // The displaced directory must be the witnessed one; else the old one goes back.
    if self.verify_dir(dir, &fresh, entry.witnessed.as_ref())? != Some(true) {
      return self.undo_clear(dir, &path, &fresh, name, made);
    }
    self.remove_named_tree(dir, &fresh)?;
    Ok(Written {
      outcome: Outcome::Written,
      window_ns: None,
    })
  }

  /// A clear that displaced another entry than the witnessed directory: the exchange back returns it to its
  /// name, and the fresh directory is removed only once the entry the exchange back left at `fresh` is
  /// checked to be it (`made`, its identity when only this landing's hidden name held it). A second outsider
  /// edit between the two exchanges leaves its own entry there instead, and that is kept, never removed; so
  /// is the fresh directory when `rmdir` refuses it, because an outsider wrote into it while it held the name
  /// (AUD-29-04).
  fn undo_clear(
    &mut self,
    dir: HostDir,
    path: &str,
    fresh: &str,
    name: &str,
    made: Fingerprint,
  ) -> Result<Written, WriteFailure> {
    self.host.exchange(dir, fresh, name)?;
    match self.host.entry_fingerprint(dir, fresh) {
      Ok(there) if there.dev == made.dev && there.ino == made.ino => {
        match self.host.rmdir(dir, fresh) {
          Ok(()) => {}
          Err(HostError::Unavailable(_)) => self.keep(dir, split(path).0, fresh, path)?,
          Err(e) => return Err(e.into()),
        }
      }
      Ok(_) => self.keep(dir, split(path).0, fresh, path)?,
      Err(HostError::NotFound) => {}
      Err(e) => return Err(e.into()),
    }
    Ok(Written {
      outcome: Outcome::Undone(ConflictClass::TargetInUse),
      window_ns: None,
    })
  }

  /// The clear without an exchange: the old directory steps aside, the fresh one takes the
  /// name; the name is absent between the two renames, and that window is reported.
  fn clear_by_renames(
    &mut self,
    dir: HostDir,
    path: &str,
    fresh: &str,
    name: &str,
    witnessed: Option<&Witness>,
  ) -> Result<Written, WriteFailure> {
    // The fresh directory takes a plain hidden name, freeing the aside name for the original.
    let plain = self.hidden_name();
    self.host.rename_noreplace(dir, fresh, dir, &plain)?;
    let aside = self.aside_name(path);
    let started = Instant::now();
    if let Some(outcome) = self.set_original_aside(dir, path, name, &aside, witnessed)? {
      self.host.rmdir(dir, &plain)?;
      return Ok(Written {
        outcome,
        window_ns: None,
      });
    }
    if let Err(e) = self.host.rename_noreplace(dir, &plain, dir, name) {
      // The name was made again meanwhile (`EEXIST`), or the host refused: the witnessed directory goes back
      // — kept beside the name when an outsider holds it — rather than removed, and ours goes.
      self.put_back(dir, split(path).0, &aside, dir, name, path)?;
      self.host.rmdir(dir, &plain)?;
      return match e {
        HostError::Unavailable(ERRNO_EXIST) => Ok(Written {
          outcome: Outcome::Undone(ConflictClass::TargetInUse),
          window_ns: None,
        }),
        e => Err(e.into()),
      };
    }
    let window_ns = elapsed_ns(started);
    self.widest_window_ns = self.widest_window_ns.max(window_ns);
    self.remove_named_tree(dir, &aside)?;
    Ok(Written {
      outcome: Outcome::Written,
      window_ns: Some(window_ns),
    })
  }

  /// A clear's original without an exchange, moved aside to `aside` and checked there (AUD-29-04; before
  /// 2026-09-29 it was removed unchecked, after a check made before the move): `None` when it is the
  /// witnessed directory, else the outcome the clear ends with — nothing was there, or another directory,
  /// which went back.
  fn set_original_aside(
    &mut self,
    dir: HostDir,
    path: &str,
    name: &str,
    aside: &str,
    witnessed: Option<&Witness>,
  ) -> Result<Option<Outcome>, WriteFailure> {
    match self.host.rename_noreplace(dir, name, dir, aside) {
      Ok(()) => {}
      Err(HostError::NotFound) => return Ok(Some(Outcome::Conflict(ConflictClass::TargetInUse))),
      Err(e) => return Err(e.into()),
    }
    if self.verify_dir(dir, aside, witnessed)? == Some(true) {
      return Ok(None);
    }
    self.put_back(dir, split(path).0, aside, dir, name, path)?;
    Ok(Some(Outcome::Undone(ConflictClass::TargetInUse)))
  }

  /// Exchanges two names in `dir`; `false` when the filesystem has no exchange (the landing
  /// then takes the fallback for every later entry too).
  fn exchange_names(&mut self, dir: HostDir, a: &str, b: &str) -> Result<bool, WriteFailure> {
    if !self.exchange {
      return Ok(false);
    }
    match self.host.exchange(dir, a, b) {
      Ok(()) => Ok(true),
      Err(HostError::Unavailable(errno)) if no_exchange(errno) => {
        self.exchange = false;
        Ok(false)
      }
      Err(e) => Err(e.into()),
    }
  }

  /// Whether the directory `name` in `dir` is the witnessed one: `None` when absent,
  /// `Some(false)` when there but another (or not a directory).
  fn verify_dir(
    &mut self,
    dir: HostDir,
    name: &str,
    witnessed: Option<&Witness>,
  ) -> Result<Option<bool>, HostError> {
    let victim = match self.host.open_dir(dir, name) {
      Ok(d) => d,
      Err(HostError::NotFound) => return Ok(None),
      Err(HostError::NotDirectory) => return Ok(Some(false)),
      Err(e) => return Err(e),
    };
    let current = self.host.fingerprint_dir(victim);
    self.host.close_dir(victim);
    Ok(Some(witnessed.is_some_and(|w| {
      current.as_ref().is_ok_and(|c| c.ino == w.fingerprint.ino)
    })))
  }

  /// Moves the directory `name` to this landing's aside name after its inode matched the witness, and checks
  /// it there: `Ok` with the aside name when it is the witnessed directory, else the outcome the removal ends
  /// with — nothing there, another entry there (nothing moved), or another entry moved and put back.
  fn set_aside(
    &mut self,
    dir: HostDir,
    dir_path: &str,
    name: &str,
    witnessed: Option<&Witness>,
  ) -> Result<Result<Box<str>, Outcome>, WriteFailure> {
    match self.verify_dir(dir, name, witnessed)? {
      None => return Ok(Err(Outcome::Skipped(SkipReason::AlreadyThere))),
      Some(false) => return Ok(Err(Outcome::Conflict(ConflictClass::TargetInUse))),
      Some(true) => {}
    }
    let path = join(dir_path, name);
    self.forget_dirs_under(&path);
    let aside = self.aside_name(&path);
    match self.host.rename_noreplace(dir, name, dir, &aside) {
      Ok(()) => {}
      Err(HostError::NotFound) => return Ok(Err(Outcome::Skipped(SkipReason::AlreadyThere))),
      Err(e) => return Err(e.into()),
    }
    // The directory moved is the one removed: it must still be the witnessed one, else it goes back
    // (AUD-29-04; before 2026-09-29 the check came before the move, so a directory an outsider put in
    // between was removed with everything beneath it).
    if self.verify_dir(dir, &aside, witnessed)? == Some(true) {
      return Ok(Ok(aside));
    }
    self.put_back(dir, dir_path, &aside, dir, name, &path)?;
    Ok(Err(Outcome::Undone(ConflictClass::TargetInUse)))
  }

  /// Removes everything inside `dir`.
  fn remove_children(&mut self, dir: HostDir) -> Result<(), HostError> {
    for entry in self.host.list(dir)? {
      if entry.kind == HostKind::Dir {
        let child = self.host.open_dir(dir, &entry.name)?;
        let emptied = self.remove_children(child);
        self.host.close_dir(child);
        emptied?;
        self.host.rmdir(dir, &entry.name)?;
      } else {
        self.host.unlink(dir, &entry.name)?;
      }
    }
    Ok(())
  }

  /// A directory rename (AUD-29-04): the origin must be the witnessed directory as it stands; it is then moved
  /// to this landing's aside name beside it without replacing anything and checked there, and only then moved
  /// to its new name, again without replacing anything. One that is not the witnessed directory goes back,
  /// and so does the witnessed one when the new name is taken meanwhile. Between the two renames the
  /// directory is at neither name, so each name stays old or new; a crash there leaves it aside for the
  /// resume's sweep to put back. Before 2026-09-29 the origin was checked and then renamed straight to its
  /// new name, where no check could say which directory had moved.
  fn rename_dir(
    &mut self,
    from: &str,
    to_dir_path: &str,
    to_name: &str,
    witnessed: Option<&Witness>,
  ) -> Result<Outcome, WriteFailure> {
    let (from_dir_path, from_name) = split(from);
    let from_dir = self.open_dir_path(from_dir_path)?;
    let to_dir = self.open_dir_path(to_dir_path)?;
    self.touched.insert(from_dir_path.into());
    self.touched.insert(to_dir_path.into());
    if let Some(conflict) = self.origin_conflict(from_dir, from_name, witnessed)? {
      return Ok(conflict);
    }
    self.forget_dirs_under(from);
    let aside = self.aside_name(from);
    match self
      .host
      .rename_noreplace(from_dir, from_name, from_dir, &aside)
    {
      Ok(()) => {}
      Err(HostError::NotFound) => return Ok(Outcome::Conflict(ConflictClass::RenameRename)),
      Err(e) => return Err(e.into()),
    }
    if self.verify_dir(from_dir, &aside, witnessed)? != Some(true) {
      self.put_back(from_dir, from_dir_path, &aside, from_dir, from_name, from)?;
      return Ok(Outcome::Undone(ConflictClass::TargetInUse));
    }
    let started = Instant::now();
    if let Err(e) = self
      .host
      .rename_noreplace(from_dir, &aside, to_dir, to_name)
    {
      // The new name was taken meanwhile (`EEXIST`), or the host refused: the directory goes back.
      self.put_back(from_dir, from_dir_path, &aside, from_dir, from_name, from)?;
      return match e {
        HostError::Unavailable(ERRNO_EXIST) => Ok(Outcome::Undone(ConflictClass::TargetInUse)),
        e => Err(e.into()),
      };
    }
    self.costs.link_ns.push(elapsed_ns(started));
    Ok(Outcome::Written)
  }

  /// A rename's origin as it stands: `None` when it is the witnessed directory (by inode), else the
  /// conflict the rename ends with — gone, no longer a directory, or another directory.
  fn origin_conflict(
    &mut self,
    from_dir: HostDir,
    from_name: &str,
    witnessed: Option<&Witness>,
  ) -> Result<Option<Outcome>, WriteFailure> {
    let origin = match self.host.open_dir(from_dir, from_name) {
      Ok(d) => d,
      Err(HostError::NotFound) => return Ok(Some(Outcome::Conflict(ConflictClass::RenameRename))),
      Err(HostError::NotDirectory) => {
        return Ok(Some(Outcome::Conflict(ConflictClass::TypeChanged)));
      }
      Err(e) => return Err(e.into()),
    };
    let current = self.host.fingerprint_dir(origin);
    self.host.close_dir(origin);
    let matches =
      witnessed.is_some_and(|w| current.as_ref().is_ok_and(|c| c.ino == w.fingerprint.ino));
    Ok((!matches).then_some(Outcome::Conflict(ConflictClass::TargetInUse)))
  }

  // ------------------------------------------------------------- syncing

  /// One sync per touched directory, then the media barrier when asked. Every failure is recorded
  /// (AUD-29-05): a directory that cannot be opened or synced is `Unsynced` and its entries are held in the
  /// overlay; a failed media barrier the grant asked for is `MediaUnsynced` and holds every entry. Before
  /// 2026-09-29 a directory that failed to open was skipped with the landing still reporting its
  /// directories synced, and a sync failure other than a crash-like errno still let every entry advance.
  fn sync_all(&mut self) -> Durability {
    let touched: Vec<Box<str>> = self.touched.iter().cloned().collect();
    let mut dirs = 0usize;
    let mut total_ns = 0u64;
    for path in touched {
      let started = Instant::now();
      let synced = self
        .open_dir_path(&path)
        .and_then(|dir| self.host.sync_dir(dir));
      let ns = elapsed_ns(started);
      total_ns = total_ns.saturating_add(ns);
      self.costs.dir_sync_ns.push(ns);
      match synced {
        Ok(()) => dirs = dirs.saturating_add(1),
        Err(error) => {
          self.note_crash(error);
          self.degraded.push(Degradation::Unsynced {
            dir: path.clone(),
            error,
          });
          self.unsynced.insert(path);
        }
      }
    }
    let media = self.request.media_durability && self.media_barrier();
    if !self.request.media_durability {
      self.degraded.push(Degradation::BarriersOnly);
    }
    Durability {
      data_synced: self.crashed.is_none(),
      dirs_synced: self.unsynced.is_empty(),
      media,
      media_requested: self.request.media_durability,
      dirs,
      dir_sync_ns: total_ns,
    }
  }

  /// The media barrier the grant asked for: whether it held. A failure is `MediaUnsynced`, and holds every
  /// entry in the overlay.
  fn media_barrier(&mut self) -> bool {
    match self.host.sync_media(self.root) {
      Ok(()) => true,
      Err(error) => {
        self.note_crash(error);
        self.degraded.push(Degradation::MediaUnsynced { error });
        self.media_failed = true;
        false
      }
    }
  }

  /// A host refusal that ends the landing: an errno other than a full disk (a host that stopped answering
  /// — the simulated crash, an `EIO`) aborts the landing so nothing is advanced; a full disk and the typed
  /// refusals (not found, not a directory, a stale handle) are recorded by the caller and the landing goes on.
  fn note_crash(&mut self, error: HostError) {
    if let HostError::Unavailable(errno) = error
      && !disk_full(errno)
    {
      self.crashed = Some(errno);
    }
  }

  /// Whether an entry reached the durability boundary this landing promises (§4.15 steps 8–9): its directory
  /// synced — for a rename, both directories — and the media barrier held when the grant asked for it. Only
  /// such an entry leaves the overlay.
  fn durable(&self, report: &EntryReport) -> bool {
    if self.media_failed {
      return false;
    }
    let (dir, _) = split(&report.path);
    let origin_synced = match &report.action {
      Action::Rename { from } => !self.unsynced.contains(split(from).0),
      _ => true,
    };
    origin_synced && !self.unsynced.contains(dir)
  }

  // ------------------------------------------------------------- sweep

  /// Settles the hidden siblings carrying this landing id in the manifest's directories (a crashed earlier
  /// attempt of the same landing), proportional to the delta's directories: how many it removed. A sibling
  /// it cannot settle is reported `Leftover`, and a directory it cannot list `Unswept` (AUD-29-05; before
  /// 2026-09-29 both were dropped, and a failed sweep counted zero).
  fn sweep(&mut self, manifest: &Manifest) -> usize {
    let mut parents: BTreeSet<Box<str>> = BTreeSet::new();
    for entry in &manifest.entries {
      parents.insert(split(&entry.path).0.into());
      if let Action::Rename { from } = &entry.action {
        parents.insert(split(from).0.into());
      }
    }
    let mut removed = 0usize;
    for parent in parents {
      let listed = self
        .open_dir_path(&parent)
        .and_then(|dir| self.host.list(dir).map(|entries| (dir, entries)));
      let (dir, entries) = match listed {
        Ok(found) => found,
        // A directory the plan names that is not there (one this landing creates) holds no sibling.
        Err(HostError::NotFound | HostError::NotDirectory) => continue,
        Err(error) => {
          self
            .degraded
            .push(Degradation::Unswept { dir: parent, error });
          continue;
        }
      };
      for e in entries {
        if !self.is_own_hidden(&e.name) {
          continue;
        }
        let settled = match self.aside_hash(&e.name) {
          // An entry moved aside is removed only once checked (AUD-29-04).
          Some(hash) => self.resolve_aside(dir, &parent, &e.name, e.kind, hash, manifest),
          None if e.kind == HostKind::Dir => self.remove_named_tree(dir, &e.name).map(|()| true),
          None => self.host.unlink(dir, &e.name).map(|()| true),
        };
        match settled {
          Ok(true) => removed = removed.saturating_add(1),
          Ok(false) => {}
          Err(error) => self.degraded.push(Degradation::Leftover {
            path: join(&parent, &e.name).into(),
            error,
          }),
        }
      }
    }
    removed
  }

  /// Settles an entry a crashed attempt left moved aside at `aside` under `parent` (AUD-29-04). The witnessed
  /// entry its manifest entry displaced is removed; for a replacement or a clear, only once the name holds
  /// the new entry — inside the exchange fallback's window the name is absent, and the old entry goes back so
  /// the path is old again and the resume lands it anew. This landing's own leftover (a replacement's
  /// temporary, a clear's fresh directory) is removed. Any other entry goes back to its name, or is kept
  /// beside it when the name is taken, as is one no manifest entry names. Whether it was removed.
  fn resolve_aside(
    &mut self,
    dir: HostDir,
    parent: &str,
    aside: &str,
    kind: HostKind,
    hash: u64,
    manifest: &Manifest,
  ) -> Result<bool, HostError> {
    let Some(entry) = manifest.entries.iter().find(|m| {
      let displaced = displaced_path(m);
      path_hash(displaced) == hash && split(displaced).0 == parent
    }) else {
      self.keep(dir, parent, aside, &join(parent, aside))?;
      return Ok(false);
    };
    let displaced = displaced_path(entry);
    let (_, name) = split(displaced);
    if self.aside_is_witnessed(dir, aside, entry)? {
      // A rename's directory is never removed: it goes back to its origin (below), and the resume renames it.
      if matches!(entry.action, Action::Rename { .. }) {
        self.put_back(dir, parent, aside, dir, name, displaced)?;
        return Ok(false);
      }
      if matches!(entry.action, Action::Replace | Action::Clear) {
        match self.host.rename_noreplace(dir, aside, dir, name) {
          Ok(()) => {
            if matches!(entry.action, Action::Replace)
              && let Ok(after) = self.host.entry_fingerprint(dir, name)
            {
              self.restored.insert(displaced.into(), after);
            }
            return Ok(false);
          }
          Err(HostError::Unavailable(ERRNO_EXIST)) => {}
          Err(e) => return Err(e),
        }
      }
      if kind == HostKind::Dir {
        self.remove_named_tree(dir, aside)?;
      } else {
        self.host.unlink(dir, aside)?;
      }
      return Ok(true);
    }
    if self.remove_if_own(dir, aside, kind, entry)? {
      return Ok(true);
    }
    self.put_back(dir, parent, aside, dir, name, displaced)?;
    Ok(false)
  }

  /// Whether the entry at `aside` is the witnessed entry `entry` displaces: a directory by its inode, a
  /// replaced file as the exchange's displaced file is checked (content when its ctime moved), anything else
  /// by its own fingerprint.
  fn aside_is_witnessed(
    &mut self,
    dir: HostDir,
    aside: &str,
    entry: &LandingEntry,
  ) -> Result<bool, HostError> {
    let Some(witness) = entry.witnessed.as_ref() else {
      return Ok(false);
    };
    Ok(match entry.action {
      Action::Rmdir | Action::Clear | Action::Rename { .. } => {
        self.verify_dir(dir, aside, Some(witness))? == Some(true)
      }
      Action::Replace => self.verify_displaced(dir, aside, witness)?,
      _ => self.aside_matches(dir, aside, witness)? == Some(true),
    })
  }

  /// Removes the entry at `aside` when it is this landing's own leftover: a replacement's temporary (a file
  /// holding the overlay's bytes) or a clear's fresh directory (removed only while empty, by `rmdir`). The one
  /// other empty directory that can sit at an aside name is an outsider's that an exchange displaced just
  /// before a crash; its removal loses no bytes, only that empty directory's own metadata. Whether it was
  /// removed.
  fn remove_if_own(
    &mut self,
    dir: HostDir,
    aside: &str,
    kind: HostKind,
    entry: &LandingEntry,
  ) -> Result<bool, HostError> {
    match kind {
      HostKind::File => {
        let ours = match entry.overlay {
          Some(overlay) => self.file_hashes_to(dir, aside, &overlay.hash)?,
          None => false,
        };
        if ours {
          self.host.unlink(dir, aside)?;
        }
        Ok(ours)
      }
      HostKind::Dir => match self.host.rmdir(dir, aside) {
        Ok(()) => Ok(true),
        // Not empty: never this landing's.
        Err(HostError::Unavailable(_)) => Ok(false),
        Err(e) => Err(e),
      },
      _ => Ok(false),
    }
  }

  /// Whether the file `name` under `dir` holds bytes whose BLAKE3 is `hash` (the manifest's overlay identity).
  fn file_hashes_to(
    &mut self,
    dir: HostDir,
    name: &str,
    hash: &[u8; 32],
  ) -> Result<bool, HostError> {
    let file = match self.host.open_file(dir, name) {
      Ok(file) => file,
      Err(HostError::NotFound | HostError::NotFile) => return Ok(false),
      Err(e) => return Err(e),
    };
    let result = self
      .host
      .fstat(file)
      .and_then(|current| self.hash_file(file, current.size));
    self.host.close_file(file);
    Ok(result? == *hash)
  }

  fn remove_named_tree(&mut self, dir: HostDir, name: &str) -> Result<(), HostError> {
    let d = self.host.open_dir(dir, name)?;
    let emptied = self.remove_children(d);
    self.host.close_dir(d);
    emptied?;
    self.host.rmdir(dir, name)?;
    Ok(())
  }

  /// Closes every directory opened beneath the root (never the root: the caller's).
  fn close_dirs(&mut self) {
    let opened: Vec<HostDir> = self.dirs.values().copied().collect();
    self.dirs.clear();
    for d in opened {
      self.host.close_dir(d);
    }
  }
}

/// The reason an entry with verdict `Skip` was skipped.
fn skip_reason(entry: &LandingEntry) -> SkipReason {
  match (
    &entry.action,
    entry.witnessed.as_ref(),
    entry.overlay.as_ref(),
  ) {
    (Action::Replace, Some(w), Some(o)) if o.hash == w.identity => SkipReason::Drift,
    _ => SkipReason::AlreadyThere,
  }
}

fn grant_ended(grant: Option<&GrantRecord>, now_ns: u64) -> bool {
  grant.is_some_and(|g| g.expires_ns <= now_ns)
}

fn no_exchange(errno: i32) -> bool {
  errno == ERRNO_INVAL || errno == ERRNO_NOTSUP_MACOS || errno == ERRNO_NOTSUP_LINUX
}

fn disk_full(errno: i32) -> bool {
  errno == ERRNO_NOSPC || errno == ERRNO_DQUOT_LINUX || errno == ERRNO_DQUOT_MACOS
}

fn elapsed_ns(since: Instant) -> u64 {
  u64::try_from(since.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// The directory and name of a volume path.
fn split(path: &str) -> (&str, &str) {
  match path.rsplit_once('/') {
    Some(("", name)) => ("/", name),
    Some((dir, name)) => (dir, name),
    None => ("/", path),
  }
}

/// The path of the base entry a manifest entry moves aside: a rename's origin, else the entry's own path.
fn displaced_path(entry: &LandingEntry) -> &str {
  match &entry.action {
    Action::Rename { from } => from,
    _ => &entry.path,
  }
}

/// A manifest path's aside hash: the first eight bytes of its BLAKE3, little-endian (AUD-29-04). A collision
/// between two paths of one landing's parent directory only makes the second move aside refuse `EEXIST`.
fn path_hash(path: &str) -> u64 {
  blake3::hash(path.as_bytes())
    .as_bytes()
    .first_chunk::<PATH_HASH_BYTES>()
    .copied()
    .map_or(0, u64::from_le_bytes)
}

fn join(dir: &str, name: &str) -> String {
  if dir == "/" {
    format!("/{name}")
  } else {
    format!("{dir}/{name}")
  }
}

/// One replacement in flight: the temporary holding the new bytes, where it sits, and what it replaces.
#[derive(Clone, Copy)]
struct Swap<'a> {
  /// The temporary, open: its descriptor holds the inode, so the temporary is known exactly.
  temp: HostFile,
  /// The entry's directory.
  dir: HostDir,
  /// The plain hidden name the temporary was created at (a named temporary still holds it).
  created: &'a str,
  /// The temporary's hidden name: the entry's aside name, where the exchange leaves the displaced file.
  hidden: &'a str,
  /// The entry's name.
  name: &'a str,
  /// The entry's manifest path.
  path: &'a str,
  /// The witness the displaced file must match.
  witness: &'a Witness,
}

/// A write refusal: the host's or the volume's.
#[derive(Debug)]
enum WriteFailure {
  Host(HostError),
  Volume(VfsError),
}

impl From<HostError> for WriteFailure {
  fn from(e: HostError) -> Self {
    Self::Host(e)
  }
}

impl From<VfsError> for WriteFailure {
  fn from(e: VfsError) -> Self {
    Self::Volume(e)
  }
}

/// The overlay's bytes for a file (through the volume, which may read a large-class file's
/// unwritten windows from the base).
fn read_overlay_bytes<H: LandFs>(
  vol: &mut Volume,
  store: &mut Store,
  host: &mut H,
  source: Source,
  path: &str,
) -> Result<Vec<u8>, WriteFailure> {
  let located = source.resolve(vol, store, path)?;
  let attrs = source.stat(vol, store, located.inode)?;
  let mut bytes = vec![0u8; usize::try_from(attrs.size).map_err(|_| VfsError::FileTooLarge)?];
  let n = source.read(vol, store, host, located.inode, &mut bytes)?;
  bytes.truncate(n);
  Ok(bytes)
}

/// Runs one landing: plan, present, grant, lease and validate, write by class, sync, advance,
/// report (§4.15). The caller opened `target` with containment and closes its handle after.
#[allow(clippy::too_many_arguments)]
pub fn land<H: LandFs>(
  host: &mut H,
  target: &LandingTarget,
  vol: &mut Volume,
  store: &mut Store,
  grants: &mut Grants,
  lease: Option<&LandingLease>,
  audit: &mut Audit,
  request: &LandingRequest,
  observer: &mut dyn Observer<H>,
) -> Result<LandingReport, LandingRefusal> {
  // Plan.
  let manifest =
    plan(vol, store, host, &request.filter, request.source).map_err(LandingRefusal::Volume)?;
  audit.push(AuditRecord {
    seq: 0,
    at_ns: request.now_ns,
    kind: AuditKind::LandingPlanned,
    grant: request.grant,
    landing: request.landing_id,
    manifest: manifest.hash,
    outcome: None,
  });
  // Grant: the landing's own binding — the consumer, the volume and snapshot, and the target as opened now —
  // must be the one the grant was approved for (§4.13 "Grants"), before any write-capable step.
  let binding = binding_of(host, target, request)?;
  let grant = match grants.check(request.grant, &binding, manifest.hash, request.now_ns) {
    Ok(g) => g,
    Err(GrantRefusal::GrantRequired) => {
      let preliminary = preliminary_verdicts(host, target, request, &manifest)?;
      return Err(LandingRefusal::GrantRequired(Box::new(Presented {
        manifest,
        preliminary,
        binding,
      })));
    }
    Err(e) => return Err(LandingRefusal::Grant(e)),
  };
  // Lease (AUD-29-03): the caller took the landing lease on the target's canonical identity from the
  // target's one host-local owner; it must name this target and still be live, and every entry is fenced by
  // its term. The engine neither takes nor releases it.
  let lease = match lease {
    Some(held)
      if held.target.as_ref() == lease_key(&binding.target) && held.expires_ns > request.now_ns =>
    {
      held
    }
    _ => return Err(LandingRefusal::LeaseRequired),
  };
  let result = land_under_lease(
    host, target, vol, store, audit, request, &manifest, &grant, lease, observer,
  );
  if let Ok(report) = &result
    && matches!(report.state, LandingState::Done | LandingState::Partial)
    && grant.scope == GrantScope::Once
  {
    grants.consume(grant.id);
  }
  result
}

/// The binding a landing presents: its consumer, volume and snapshot, and its target identified by the
/// directory the landing opened (the host's `fingerprint_dir`: device and inode), so a directory replaced
/// at the target's path is another target.
fn binding_of<H: LandFs>(
  host: &mut H,
  target: &LandingTarget,
  request: &LandingRequest,
) -> Result<GrantBinding, LandingRefusal> {
  let identity = host
    .fingerprint_dir(target.dir)
    .map_err(LandingRefusal::Target)?;
  Ok(GrantBinding {
    consumer: request.consumer.clone(),
    volume: request.volume,
    snapshot: request.snapshot,
    target: TargetIdentity {
      key: target.key.clone(),
      device: identity.dev,
      inode: identity.ino,
    },
  })
}

/// The preliminary verdict pass a `GrantRequired` reply carries (reads only).
fn preliminary_verdicts<H: LandFs>(
  host: &mut H,
  target: &LandingTarget,
  request: &LandingRequest,
  manifest: &Manifest,
) -> Result<Vec<EntryReport>, LandingRefusal> {
  let read_only = LandCapabilities {
    exchange: false,
    reflink: false,
    unnamed_temporaries: false,
  };
  let mut landing = Landing::start(host, target, request, read_only);
  let verdicts = landing.validate(manifest);
  landing.close_dirs();
  verdicts.map_err(LandingRefusal::Target)
}

impl<'a, H: LandFs> Landing<'a, H> {
  fn start(
    host: &'a mut H,
    target: &LandingTarget,
    request: &'a LandingRequest,
    caps: LandCapabilities,
  ) -> Self {
    Self {
      host,
      request,
      root: target.dir,
      dirs: BTreeMap::new(),
      touched: BTreeSet::new(),
      exchange: caps.exchange,
      unnamed_temporaries: caps.unnamed_temporaries,
      hidden_counter: 0,
      bytes_written: 0,
      widest_window_ns: 0,
      degraded: Vec::new(),
      costs: CostSamples::default(),
      ramp: Ramp::new(request.cores, request.max_depth, request.variance_permille),
      crashed: None,
      unsynced: BTreeSet::new(),
      media_failed: false,
      placed: BTreeMap::new(),
      restored: BTreeMap::new(),
    }
  }
}

/// Everything after the lease: validate, sweep, write inside the target, sync, advance.
#[allow(clippy::too_many_arguments)] // land's own inputs, the manifest and grant it checked, and its lease
fn land_under_lease<H: LandFs>(
  host: &mut H,
  target: &LandingTarget,
  vol: &mut Volume,
  store: &mut Store,
  audit: &mut Audit,
  request: &LandingRequest,
  manifest: &Manifest,
  grant: &GrantRecord,
  lease: &LandingLease,
  observer: &mut dyn Observer<H>,
) -> Result<LandingReport, LandingRefusal> {
  let caps = host
    .capabilities(target.dir)
    .map_err(LandingRefusal::Target)?;
  let mut landing = Landing::start(host, target, request, caps);
  // An earlier attempt's leftovers are settled before the verdicts are taken: an entry it left moved aside
  // (inside an exchange fallback's or a rename's window) goes back first, so validation sees each path old or
  // new, never missing (AUD-29-04).
  let swept = landing.sweep(manifest);
  let validated = landing.validate(manifest);
  let mut reports = match validated {
    Ok(r) => r,
    Err(e) => {
      landing.close_dirs();
      return Err(LandingRefusal::Target(e));
    }
  };
  audit.push(AuditRecord {
    seq: 0,
    at_ns: request.now_ns,
    kind: AuditKind::LandingValidated,
    grant: Some(grant.id),
    landing: request.landing_id,
    manifest: manifest.hash,
    outcome: None,
  });
  if reports
    .iter()
    .any(|r| matches!(r.verdict, Some(Verdict::Conflict(_))))
  {
    landing.close_dirs();
    return Err(LandingRefusal::Conflict(reports));
  }
  landing.write_all(
    manifest,
    &mut reports,
    &mut WriteContext {
      vol,
      store,
      grant: Some(grant),
      lease,
      observer,
      audit,
    },
  );
  let durability = landing.sync_all();
  let target_dir = target.dir;
  if let Some(errno) = landing.crashed {
    landing.degraded.push(Degradation::Crashed { errno });
  }
  if !landing.exchange {
    landing.degraded.push(Degradation::NoExchange {
      widest_window_ns: landing.widest_window_ns,
    });
  }
  landing.close_dirs();
  // An entry that landed but did not reach the durability boundary (its directory, or the media barrier the
  // grant asked for, did not sync) stays in the overlay: the resume syncs it and advances it then
  // (AUD-29-05; before 2026-09-29 such entries were advanced, and a lost sync lost the private work).
  let held = reports
    .iter()
    .filter(|r| advances(r.outcome.as_ref()) && !landing.durable(r))
    .count();
  let state = terminal_state(&reports, landing.crashed.is_some(), held);
  let facts = if vol.is_overlay() {
    None
  } else {
    Some(
      landing
        .host
        .facts(target_dir)
        .map_err(LandingRefusal::Target)?,
    )
  };
  // Advance.
  // A landed rename leaves the overlay with the whiteout at its origin. An aborted landing
  // advances nothing: its entries stay in the overlay so the resume re-plans them all, finds
  // the landed ones already there, and syncs their directories again.
  let landed = advanced(&reports, state, manifest, request, &landing);
  let base = facts.map(|facts| BaseConfig {
    root: target_dir,
    facts,
    large_class_bytes: request.large_class_bytes,
  });
  vol
    .with_host(landing.host)
    .land_advance(store, &landed, base, request.source.snapshot())
    .map_err(LandingRefusal::Volume)?;
  audit.push(AuditRecord {
    seq: 0,
    at_ns: request.now_ns,
    kind: AuditKind::LandingFinished,
    grant: Some(grant.id),
    landing: request.landing_id,
    manifest: manifest.hash,
    outcome: Some(state),
  });
  let costs = std::mem::take(&mut landing.costs).fold();
  Ok(LandingReport {
    state,
    manifest_hash: manifest.hash,
    summary: manifest.summary.clone(),
    written: reports
      .iter()
      .filter(|r| r.outcome == Some(Outcome::Written))
      .count(),
    skipped: reports
      .iter()
      .filter(|r| {
        matches!(
          r.outcome,
          Some(Outcome::Skipped(_) | Outcome::AcceptedIdentical)
        )
      })
      .count(),
    conflicts: reports
      .iter()
      .filter(|r| matches!(r.outcome, Some(Outcome::Conflict(_) | Outcome::Undone(_))))
      .count(),
    failed: reports
      .iter()
      .filter(|r| matches!(r.outcome, Some(Outcome::Failed { .. })))
      .count(),
    held,
    entries: reports,
    bytes_written: landing.bytes_written,
    durability,
    degraded: landing.degraded,
    ramp_depth: landing.ramp.depth,
    costs,
    swept,
    target: target_dir,
  })
}

/// Whether an outcome takes its entry out of the overlay: the disk holds the overlay's state.
/// The paths a landing advances, each written file with the witness of what it wrote: the fingerprint its
/// placement left and the identity of the source's bytes (A-49). A landed rename leaves the overlay with
/// the whiteout at its origin. An aborted landing advances nothing: its entries stay in the overlay so the
/// resume re-plans them all, finds the landed ones already there, and syncs their directories again.
fn advanced<H: LandFs>(
  reports: &[EntryReport],
  state: LandingState,
  manifest: &Manifest,
  request: &LandingRequest,
  landing: &Landing<'_, H>,
) -> Vec<Landed> {
  let identities: BTreeMap<&str, [u8; 32]> = manifest
    .entries
    .iter()
    .filter_map(|entry| Some((entry.path.as_ref(), entry.overlay?.hash)))
    .collect();
  let mut landed = Vec::with_capacity(reports.len());
  let completed = state != LandingState::Aborted;
  for r in reports
    .iter()
    .filter(|r| completed && advances(r.outcome.as_ref()) && landing.durable(r))
  {
    // The landing's own write left a fresh ctime, so its witness is racy by the design's rule: a later
    // verdict re-hashes the file rather than trust its fingerprint.
    let written = landing
      .placed
      .get(&r.path)
      .zip(identities.get(r.path.as_ref()))
      .map(|(fingerprint, identity)| Witness {
        fingerprint: *fingerprint,
        identity: *identity,
        witnessed_at: request.now_ns,
        racy: true,
      });
    landed.push(Landed {
      path: r.path.to_string(),
      written,
    });
    if let Action::Rename { from } = &r.action {
      landed.push(Landed {
        path: from.to_string(),
        written: None,
      });
    }
  }
  landed
}

fn advances(outcome: Option<&Outcome>) -> bool {
  matches!(
    outcome,
    Some(
      Outcome::Written
        | Outcome::AcceptedIdentical
        | Outcome::Skipped(SkipReason::AlreadyThere | SkipReason::Drift)
    )
  )
}

/// The landing's terminal state: `Aborted` after a crash; `Done` only when every entry landed or was
/// already there (so it leaves the overlay) and every one reached the durability boundary (`held` counts
/// those that did not); `Partial` otherwise.
fn terminal_state(reports: &[EntryReport], crashed: bool, held: usize) -> LandingState {
  if crashed {
    return LandingState::Aborted;
  }
  // Done only when every entry reached its landed state: a skip that left an entry private (its parent
  // missing, the grant ended) is Partial, as the failure matrix states (AUD-29-05; before 2026-09-29 every
  // skip counted as done).
  if held == 0 && reports.iter().all(|r| advances(r.outcome.as_ref())) {
    LandingState::Done
  } else {
    LandingState::Partial
  }
}
