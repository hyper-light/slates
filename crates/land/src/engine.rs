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
//!   descriptor held on the displaced file is `fstat`ed; a fingerprint other than the witness
//!   exchanges back, removes the temporary and records `Undone(TargetInUse)`; a create whose
//!   name appeared meanwhile fails the link with `EEXIST` and records `Conflict(TargetInUse)`;
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
//! [`crate::grant`] and [`Audit`]; the stage-and-exchange alternative runs for an empty target
//! (the populated-target case needs a hard-link verb the seam gains with its measured cost,
//! GAPS §8c); reflinks are not used (the probe is recorded as `false`).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use slates_vfs::base::BaseConfig;
use slates_vfs::error::VfsError;
use slates_vfs::host::{HostDir, HostError, HostFile, HostKind, LandCapabilities, LandFs};
use slates_vfs::inode::Witness;
use slates_vfs::volume::{Store, Volume};

use crate::grant::{
  GrantId, GrantRecord, GrantRefusal, GrantScope, Grants, LandingLeaseHeld, Leases,
};
use crate::manifest::{Action, Filter, LandingEntry, Manifest, OverlayIdentity, Summary, plan};
use crate::ramp::{Ramp, StepSample};
use crate::verdict::{ConflictClass, DiskState, Verdict, verdict};

/// Format: the prefix of every hidden sibling a landing creates inside the granted target; the
/// landing id follows, then a per-landing counter, so a sweep recognizes its own names.
const HIDDEN_PREFIX: &str = ".slates-";
/// Format: the name of the staging directory's counter slot.
const STAGE_SUFFIX: &str = "stage";
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
/// Format: the mode of a staging directory before the target's own mode is applied: owner
/// only, so a half-built tree is never readable by others.
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
  /// The unverified window of the exchange fallback, when that path was taken.
  pub window_ns: Option<u64>,
}

/// A Degraded cell the landing hit (the design's failure matrix).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Degradation {
  /// The filesystem lacks an atomic exchange; entries were written by verify-then-rename with
  /// the widest window observed.
  NoExchange {
    /// The widest unverified window, nanoseconds.
    widest_window_ns: u64,
  },
  /// Media durability was not requested (macOS barriers only).
  BarriersOnly,
  /// A crash interrupted the landing; the report lists what finished.
  Crashed {
    /// The errno.
    errno: i32,
  },
}

/// Which durability the landing achieved (§4.15 step 8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Durability {
  /// Every written file's data sync ran before its link or exchange.
  pub data_synced: bool,
  /// Every touched directory was synced.
  pub dirs_synced: bool,
  /// The media barrier ran.
  pub media: bool,
  /// Directories synced.
  pub dirs: usize,
  /// The total time in the directory syncs, nanoseconds (the per-directory cost the sync
  /// strategy derives from).
  pub dir_sync_ns: u64,
}

/// Costs measured by a landing, remembered by the caller per target filesystem for the
/// stage-and-exchange break-even (§4.15 step 10) and the sync strategy.
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

impl LandingCosts {
  /// Whether staging beats in-place for a delta of `delta_entries` and `delta_bytes` into a
  /// target of `target_entries` (§4.15 step 10's formulas). Unknown costs (no samples) never
  /// choose staging.
  pub fn prefers_staging(&self, target_entries: u64, delta_entries: u64, delta_bytes: u64) -> bool {
    if self.samples == 0 {
      return false;
    }
    /// Format: bytes per KiB.
    const KIB: u64 = 1024;
    let write = delta_bytes
      .div_ceil(KIB)
      .saturating_mul(self.write_ns_per_kib);
    let staging = target_entries
      .saturating_add(delta_entries)
      .saturating_mul(self.link_ns)
      .saturating_add(write)
      .saturating_add(self.exchange_ns);
    let in_place = delta_entries
      .saturating_mul(self.exchange_ns.saturating_add(self.verify_ns))
      .saturating_add(write);
    staging < in_place
  }
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
  /// Bytes written to the disk.
  pub bytes_written: u64,
  /// The durability achieved.
  pub durability: Durability,
  /// The Degraded cells hit.
  pub degraded: Vec<Degradation>,
  /// Whether the landing was staged and exchanged.
  pub staged: bool,
  /// The depth the concurrency ramp settled on (recorded; Phase 1 runs one entry at a time).
  pub ramp_depth: u32,
  /// The costs measured.
  pub costs: LandingCosts,
  /// Hidden siblings of an earlier landing swept before this one ran.
  pub swept: usize,
  /// The target directory handle valid after the landing: the caller's, unless the landing
  /// was staged and exchanged, which gives the target a new inode (§4.15 step 10) and so a new
  /// handle; the caller then closes its old one.
  pub target: HostDir,
}

/// What a `GrantRequired` refusal carries for the confirmation surface (§4.15 step 2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Presented {
  /// The manifest.
  pub manifest: Manifest,
  /// The preliminary verdict pass (no writes).
  pub preliminary: Vec<EntryReport>,
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
    self.next_seq += 1;
    if self.records.len() < self.retain {
      self.records.push(record);
    } else {
      self.records[self.head] = record;
      self.head = (self.head + 1) % self.retain;
    }
  }

  /// The records, oldest first.
  pub fn records(&self) -> Vec<&AuditRecord> {
    let (later, earlier) = self.records.split_at(self.head);
    earlier.iter().chain(later.iter()).collect()
  }
}

/// The target directory, opened by the caller with containment (§4.15 step 4): its handle,
/// its canonical key (the lease's name), and its parent with the name inside it when the
/// caller could open that (needed only for stage-and-exchange).
#[derive(Clone, Debug)]
pub struct LandingTarget {
  /// The target directory.
  pub dir: HostDir,
  /// The canonical target the lease names.
  pub key: Box<str>,
  /// The parent and the target's name in it.
  pub parent: Option<(HostDir, Box<str>)>,
}

/// One landing's request.
#[derive(Clone, Debug)]
pub struct LandingRequest {
  /// The landing's id (hidden siblings carry it).
  pub landing_id: u64,
  /// The session holding the lease.
  pub holder: u64,
  /// The grant, when the human issued one.
  pub grant: Option<GrantId>,
  /// The filter.
  pub filter: Filter,
  /// Now, monotonic ns.
  pub now_ns: u64,
  /// Derived: the lease term, the caller's landing-duration p99 × k (§4.15).
  pub lease_term_ns: u64,
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
  /// Costs remembered from earlier landings into this filesystem, for the break-even.
  pub costs: Option<LandingCosts>,
  /// The target's entry count when the caller knows it (from its base listings), for the
  /// break-even; unknown means in-place unless the target is empty.
  pub target_entries: Option<u64>,
}

/// What the caller may run before each entry's write: the oracle injects outsider edits here
/// (T-1.14). The default does nothing.
pub trait Observer<H: LandFs> {
  /// Called with the host before `entry` is written.
  fn before_write(&mut self, host: &mut H, entry: &LandingEntry);
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
  samples[samples.len() / 2]
}

impl CostSamples {
  fn fold(mut self) -> LandingCosts {
    let samples = u64::try_from(
      self.link_ns.len()
        + self.write_ns_per_kib.len()
        + self.exchange_ns.len()
        + self.verify_ns.len(),
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
  /// The root the entries' paths are relative to: the target, or the staging directory.
  root: HostDir,
  /// The target's parent and its name there, when the caller could open it (staging needs it).
  stage_parent: Option<(HostDir, Box<str>)>,
  /// Directories opened beneath the root, by volume path (`"/"` is the root itself).
  dirs: BTreeMap<Box<str>, HostDir>,
  /// Directories a write touched (synced at the end).
  touched: BTreeSet<Box<str>>,
  caps: LandCapabilities,
  /// Whether the exchange path is still believed to work (flipped by the first `EINVAL`).
  exchange: bool,
  hidden_counter: u64,
  bytes_written: u64,
  widest_window_ns: u64,
  degraded: Vec<Degradation>,
  costs: CostSamples,
  ramp: Ramp,
  /// The host errno that ended the landing, when one did.
  crashed: Option<i32>,
}

/// What the writer needs besides the host: the volume, the grant, the observer, the audit log.
struct WriteContext<'x, H: LandFs> {
  vol: &'x mut Volume,
  store: &'x mut Store,
  grant: Option<&'x GrantRecord>,
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
    self.hidden_counter += 1;
    format!("{HIDDEN_PREFIX}{:016x}-{n}", self.request.landing_id).into()
  }

  fn is_own_hidden(&self, name: &str) -> bool {
    name.starts_with(&format!("{HIDDEN_PREFIX}{:016x}-", self.request.landing_id))
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
          Some(self.hash_file(file)?)
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

  fn hash_file(&mut self, file: HostFile) -> Result<[u8; 32], HostError> {
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; HASH_READ_BYTES];
    let mut off = 0u64;
    loop {
      let n = self.host.read_at(file, off, &mut buf)?;
      if n == 0 {
        break;
      }
      hasher.update(&buf[..n]);
      off = off.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
    }
    Ok(*hasher.finalize().as_bytes())
  }

  /// Whether the verdict for `entry` needs the disk's bytes hashed: a create over an existing
  /// file always (same bytes or a conflict), a replacement only when the fingerprint moved
  /// from the witness or the witness was racy.
  fn needs_hash(&mut self, entry: &LandingEntry) -> Result<bool, HostError> {
    match (&entry.action, entry.witnessed.as_ref()) {
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
      let v = verdict(
        &entry.action,
        entry.witnessed.as_ref(),
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
      observer,
      audit,
    } = cx;
    let grant = *grant;
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
      let outcome = match report.verdict {
        Some(Verdict::Apply) => {
          if grant_ended(
            grant,
            self.request.now_ns.saturating_add(elapsed_ns(started)),
          ) {
            Written {
              outcome: Outcome::Skipped(SkipReason::GrantEnded),
              window_ns: None,
            }
          } else {
            observer.before_write(self.host, entry);
            let entry_started = Instant::now();
            let w = self.write_entry(entry, vol, store);
            step_entries += 1;
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
          Err(HostError::Unavailable(ERRNO_EXIST)) => Outcome::Skipped(SkipReason::AlreadyThere),
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
      Action::Replace => return self.write_file(entry, vol, store, entry.witnessed.as_ref()),
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
    let bytes = read_overlay_bytes(vol, store, self.host, &entry.path)?;
    let dir = self.open_dir_path(dir_path)?;
    self.touched.insert(dir_path.into());
    let hidden = self.hidden_name();
    let temp = self.fill_temp(dir, &hidden, &bytes, overlay)?;
    let result = match witnessed {
      None => self.link_create(temp, dir, &hidden, name),
      Some(w) => self.swap_replace(temp, dir, &hidden, name, w),
    };
    self.host.close_file(temp);
    match result {
      Ok(w) => {
        if w.outcome == Outcome::Written {
          self.bytes_written = self.bytes_written.saturating_add(overlay.size);
        }
        Ok(w)
      }
      Err(e) => {
        // The temporary never reached the name: remove it if it has one.
        let _ = self.host.unlink(dir, &hidden);
        Err(e)
      }
    }
  }

  /// The temporary with its bytes, mode, mtime and data sync, at its hidden name.
  fn fill_temp(
    &mut self,
    dir: HostDir,
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
      let _ = self.host.unlink(dir, hidden);
      return Err(e.into());
    }
    /// Format: bytes per KiB.
    const KIB: u64 = 1024;
    let kib = u64::try_from(bytes.len())
      .unwrap_or(u64::MAX)
      .div_ceil(KIB)
      .max(1);
    self.costs.write_ns_per_kib.push(elapsed_ns(started) / kib);
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
  fn swap_replace(
    &mut self,
    temp: HostFile,
    dir: HostDir,
    hidden: &str,
    name: &str,
    w: &Witness,
  ) -> Result<Written, WriteFailure> {
    self.host.place(temp, dir, hidden)?;
    let old = match self.host.open_file(dir, name) {
      Ok(f) => f,
      Err(HostError::NotFound | HostError::NotFile) => {
        self.host.unlink(dir, hidden)?;
        return Ok(Written {
          outcome: Outcome::Conflict(ConflictClass::TargetInUse),
          window_ns: None,
        });
      }
      Err(e) => return Err(e.into()),
    };
    let result = if self.exchange {
      self.exchange_and_verify(old, dir, hidden, name, w)
    } else {
      self.verify_then_rename(old, dir, hidden, name, w)
    };
    self.host.close_file(old);
    result
  }

  fn exchange_and_verify(
    &mut self,
    old: HostFile,
    dir: HostDir,
    hidden: &str,
    name: &str,
    w: &Witness,
  ) -> Result<Written, WriteFailure> {
    let started = Instant::now();
    match self.host.exchange(dir, hidden, name) {
      Ok(()) => {}
      Err(HostError::Unavailable(errno)) if no_exchange(errno) => {
        self.exchange = false;
        return self.verify_then_rename(old, dir, hidden, name, w);
      }
      Err(e) => return Err(e.into()),
    }
    self.costs.exchange_ns.push(elapsed_ns(started));
    let verify_started = Instant::now();
    let displaced = self.host.fstat(old)?;
    self.costs.verify_ns.push(elapsed_ns(verify_started));
    if displaced == w.fingerprint {
      self.host.unlink(dir, hidden)?;
      return Ok(Written {
        outcome: Outcome::Written,
        window_ns: None,
      });
    }
    // Lost to an outsider: the old file goes back, the temporary goes away.
    self.host.exchange(dir, hidden, name)?;
    self.host.unlink(dir, hidden)?;
    Ok(Written {
      outcome: Outcome::Undone(ConflictClass::TargetInUse),
      window_ns: None,
    })
  }

  /// The exchange fallback (Degraded): verify the old file through its descriptor, then rename
  /// the temporary over it; the window between the two is measured and reported.
  fn verify_then_rename(
    &mut self,
    old: HostFile,
    dir: HostDir,
    hidden: &str,
    name: &str,
    w: &Witness,
  ) -> Result<Written, WriteFailure> {
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
    let window_started = Instant::now();
    self.host.rename(dir, hidden, dir, name)?;
    let window_ns = elapsed_ns(window_started);
    self.costs.link_ns.push(window_ns);
    self.widest_window_ns = self.widest_window_ns.max(window_ns);
    Ok(Written {
      outcome: Outcome::Written,
      window_ns: Some(window_ns),
    })
  }

  /// A delete: the file's fingerprint must still be the witnessed one through a descriptor;
  /// a symlink (no descriptor) is unlinked as is.
  fn delete(
    &mut self,
    dir_path: &str,
    name: &str,
    witnessed: Option<&Witness>,
  ) -> Result<Outcome, WriteFailure> {
    let dir = self.open_dir_path(dir_path)?;
    self.touched.insert(dir_path.into());
    let file = match self.host.open_file(dir, name) {
      Ok(f) => f,
      Err(HostError::NotFound) => return Ok(Outcome::Skipped(SkipReason::AlreadyThere)),
      Err(HostError::NotFile) => {
        self.host.unlink(dir, name)?;
        return Ok(Outcome::Written);
      }
      Err(e) => return Err(e.into()),
    };
    let current = self.host.fstat(file);
    self.host.close_file(file);
    let matches = witnessed.is_some_and(|w| current.as_ref().is_ok_and(|c| *c == w.fingerprint));
    if !matches {
      return Ok(Outcome::Conflict(ConflictClass::TargetInUse));
    }
    self.host.unlink(dir, name)?;
    Ok(Outcome::Written)
  }

  /// A recursive removal: the directory's inode must be the witnessed one; then the directory
  /// is renamed to a hidden sibling in one step (the name is old or gone, never half-removed)
  /// and the hidden tree is removed bottom-up; a crash leaves it for the sweep.
  fn remove_tree_entry(
    &mut self,
    dir_path: &str,
    name: &str,
    witnessed: Option<&Witness>,
  ) -> Result<Outcome, WriteFailure> {
    let dir = self.open_dir_path(dir_path)?;
    self.touched.insert(dir_path.into());
    let Some(hidden) = self.set_aside(dir, dir_path, name, witnessed)? else {
      return Ok(Outcome::Skipped(SkipReason::AlreadyThere));
    };
    let Some(hidden) = hidden else {
      return Ok(Outcome::Conflict(ConflictClass::TargetInUse));
    };
    self.remove_named_tree(dir, &hidden)?;
    Ok(Outcome::Written)
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
    self.forget_dirs_under(&join(dir_path, name));
    let fresh = self.hidden_name();
    self.host.mkdir(dir, &fresh, mode)?;
    if !self.exchange_names(dir, &fresh, name)? {
      return self.clear_by_renames(dir, &fresh, name);
    }
    // The displaced directory must be the witnessed one; else the old one goes back.
    if self.verify_dir(dir, &fresh, entry.witnessed.as_ref())? != Some(true) {
      self.host.exchange(dir, &fresh, name)?;
      self.host.rmdir(dir, &fresh)?;
      return Ok(Written {
        outcome: Outcome::Undone(ConflictClass::TargetInUse),
        window_ns: None,
      });
    }
    self.remove_named_tree(dir, &fresh)?;
    Ok(Written {
      outcome: Outcome::Written,
      window_ns: None,
    })
  }

  /// The clear without an exchange: the old directory steps aside, the fresh one takes the
  /// name; the name is absent between the two renames, and that window is reported.
  fn clear_by_renames(
    &mut self,
    dir: HostDir,
    fresh: &str,
    name: &str,
  ) -> Result<Written, WriteFailure> {
    let aside = self.hidden_name();
    let started = Instant::now();
    self.host.rename(dir, name, dir, &aside)?;
    self.host.rename(dir, fresh, dir, name)?;
    let window_ns = elapsed_ns(started);
    self.widest_window_ns = self.widest_window_ns.max(window_ns);
    self.remove_named_tree(dir, &aside)?;
    Ok(Written {
      outcome: Outcome::Written,
      window_ns: Some(window_ns),
    })
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
  ) -> Result<Option<bool>, WriteFailure> {
    let victim = match self.host.open_dir(dir, name) {
      Ok(d) => d,
      Err(HostError::NotFound) => return Ok(None),
      Err(HostError::NotDirectory) => return Ok(Some(false)),
      Err(e) => return Err(e.into()),
    };
    let current = self.host.fingerprint_dir(victim);
    self.host.close_dir(victim);
    Ok(Some(witnessed.is_some_and(|w| {
      current.as_ref().is_ok_and(|c| c.ino == w.fingerprint.ino)
    })))
  }

  /// Renames the directory `name` to a hidden sibling after its inode matched the witness.
  /// `None`: nothing there; `Some(None)`: there but not the witnessed one; `Some(Some(h))`: set
  /// aside at `h`.
  fn set_aside(
    &mut self,
    dir: HostDir,
    dir_path: &str,
    name: &str,
    witnessed: Option<&Witness>,
  ) -> Result<Option<Option<Box<str>>>, WriteFailure> {
    match self.verify_dir(dir, name, witnessed)? {
      None => Ok(None),
      Some(false) => Ok(Some(None)),
      Some(true) => {
        self.forget_dirs_under(&join(dir_path, name));
        let hidden = self.hidden_name();
        self.host.rename(dir, name, dir, &hidden)?;
        Ok(Some(Some(hidden)))
      }
    }
  }

  /// Removes everything inside `dir`.
  fn remove_children(&mut self, dir: HostDir) -> Result<(), WriteFailure> {
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

  /// A directory rename: the origin's inode must be the witnessed one.
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
    let origin = match self.host.open_dir(from_dir, from_name) {
      Ok(d) => d,
      Err(HostError::NotFound) => return Ok(Outcome::Conflict(ConflictClass::RenameRename)),
      Err(HostError::NotDirectory) => return Ok(Outcome::Conflict(ConflictClass::TypeChanged)),
      Err(e) => return Err(e.into()),
    };
    let current = self.host.fingerprint_dir(origin);
    self.host.close_dir(origin);
    let matches =
      witnessed.is_some_and(|w| current.as_ref().is_ok_and(|c| c.ino == w.fingerprint.ino));
    if !matches {
      return Ok(Outcome::Conflict(ConflictClass::TargetInUse));
    }
    self.forget_dirs_under(from);
    let started = Instant::now();
    self.host.rename(from_dir, from_name, to_dir, to_name)?;
    self.costs.link_ns.push(elapsed_ns(started));
    Ok(Outcome::Written)
  }

  // ------------------------------------------------------------- syncing

  /// One sync per touched directory, then the media barrier when asked.
  fn sync_all(&mut self) -> Durability {
    let touched: Vec<Box<str>> = self.touched.iter().cloned().collect();
    let mut dirs_synced = true;
    let mut dirs = 0usize;
    let mut total_ns = 0u64;
    for path in touched {
      let Ok(dir) = self.open_dir_path(&path) else {
        continue;
      };
      let started = Instant::now();
      match self.host.sync_dir(dir) {
        Ok(()) => {}
        Err(HostError::Unavailable(errno)) if !disk_full(errno) => {
          dirs_synced = false;
          self.crashed = Some(errno);
        }
        Err(_) => dirs_synced = false,
      }
      let ns = elapsed_ns(started);
      total_ns = total_ns.saturating_add(ns);
      self.costs.dir_sync_ns.push(ns);
      dirs += 1;
    }
    let media = if self.request.media_durability {
      match self.host.sync_media(self.root) {
        Ok(()) => true,
        Err(HostError::Unavailable(errno)) if !disk_full(errno) => {
          self.crashed = Some(errno);
          false
        }
        Err(_) => false,
      }
    } else {
      false
    };
    if !self.request.media_durability {
      self.degraded.push(Degradation::BarriersOnly);
    }
    Durability {
      data_synced: self.crashed.is_none(),
      dirs_synced,
      media,
      dirs,
      dir_sync_ns: total_ns,
    }
  }

  // ------------------------------------------------------------- sweep

  /// Removes hidden siblings carrying this landing id from the manifest's directories (a
  /// crashed earlier attempt of the same landing), proportional to the delta's directories.
  fn sweep(&mut self, manifest: &Manifest) -> Result<usize, HostError> {
    let mut parents: BTreeSet<Box<str>> = BTreeSet::new();
    for entry in &manifest.entries {
      parents.insert(split(&entry.path).0.into());
      if let Action::Rename { from } = &entry.action {
        parents.insert(split(from).0.into());
      }
    }
    let mut removed = 0usize;
    for parent in parents {
      let Ok(dir) = self.open_dir_path(&parent) else {
        continue;
      };
      for e in self.host.list(dir)? {
        if !self.is_own_hidden(&e.name) {
          continue;
        }
        let gone = if e.kind == HostKind::Dir {
          self.remove_named_tree(dir, &e.name).is_ok()
        } else {
          self.host.unlink(dir, &e.name).is_ok()
        };
        if gone {
          removed += 1;
        }
      }
    }
    Ok(removed)
  }

  fn remove_named_tree(&mut self, dir: HostDir, name: &str) -> Result<(), WriteFailure> {
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
  match path.rfind('/') {
    Some(0) => ("/", &path[1..]),
    Some(i) => (&path[..i], &path[i + 1..]),
    None => ("/", path),
  }
}

fn join(dir: &str, name: &str) -> String {
  if dir == "/" {
    format!("/{name}")
  } else {
    format!("{dir}/{name}")
  }
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
  path: &str,
) -> Result<Vec<u8>, WriteFailure> {
  let located = vol.resolve(store, path)?;
  let attrs = vol.stat(store, located.inode)?;
  let mut bytes = vec![0u8; usize::try_from(attrs.size).map_err(|_| VfsError::FileTooLarge)?];
  let n = vol
    .with_host(host)
    .read(store, located.inode, 0, &mut bytes)?;
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
  leases: &mut Leases,
  audit: &mut Audit,
  request: &LandingRequest,
  observer: &mut dyn Observer<H>,
) -> Result<LandingReport, LandingRefusal> {
  // Plan.
  let manifest = plan(vol, store, host, &request.filter).map_err(LandingRefusal::Volume)?;
  audit.push(AuditRecord {
    seq: 0,
    at_ns: request.now_ns,
    kind: AuditKind::LandingPlanned,
    grant: request.grant,
    landing: request.landing_id,
    manifest: manifest.hash,
    outcome: None,
  });
  // Grant.
  let grant = match grants.check(request.grant, manifest.hash, request.now_ns) {
    Ok(g) => g,
    Err(GrantRefusal::GrantRequired) => {
      let preliminary = preliminary_verdicts(host, target, request, &manifest)?;
      return Err(LandingRefusal::GrantRequired(Box::new(Presented {
        manifest,
        preliminary,
      })));
    }
    Err(e) => return Err(LandingRefusal::Grant(e)),
  };
  // Lease.
  let lease = leases
    .take(
      &target.key,
      request.holder,
      request.now_ns,
      request.lease_term_ns,
    )
    .map_err(LandingRefusal::LeaseHeld)?;
  let result = land_under_lease(
    host, target, vol, store, audit, request, &manifest, &grant, observer,
  );
  leases.release(&lease);
  if let Ok(report) = &result
    && matches!(report.state, LandingState::Done | LandingState::Partial)
    && grant.scope == GrantScope::Once
  {
    grants.consume(grant.id);
  }
  result
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
      stage_parent: target.parent.clone(),
      dirs: BTreeMap::new(),
      touched: BTreeSet::new(),
      exchange: caps.exchange,
      caps,
      hidden_counter: 0,
      bytes_written: 0,
      widest_window_ns: 0,
      degraded: Vec::new(),
      costs: CostSamples::default(),
      ramp: Ramp::new(request.cores, request.max_depth, request.variance_permille),
      crashed: None,
    }
  }
}

/// Everything after the lease: validate, sweep, write (in place or staged), sync, advance.
#[allow(clippy::too_many_arguments)]
fn land_under_lease<H: LandFs>(
  host: &mut H,
  target: &LandingTarget,
  vol: &mut Volume,
  store: &mut Store,
  audit: &mut Audit,
  request: &LandingRequest,
  manifest: &Manifest,
  grant: &GrantRecord,
  observer: &mut dyn Observer<H>,
) -> Result<LandingReport, LandingRefusal> {
  let caps = host
    .capabilities(target.dir)
    .map_err(LandingRefusal::Target)?;
  let mut landing = Landing::start(host, target, request, caps);
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
  let swept = landing.sweep(manifest).unwrap_or(0);
  let staged = landing.stage_if_better(manifest, &reports);
  landing.write_all(
    manifest,
    &mut reports,
    &mut WriteContext {
      vol,
      store,
      grant: Some(grant),
      observer,
      audit,
    },
  );
  // Directory syncs run where the entries were written (the stage's directories when staged),
  // before any exchange makes them visible at the target.
  let mut durability = landing.sync_all();
  let (staged, target_dir) = match staged {
    Some(stage) => landing.finish_stage(target, stage, &mut durability),
    None => (false, target.dir),
  };
  if let Some(errno) = landing.crashed {
    landing.degraded.push(Degradation::Crashed { errno });
  }
  if !landing.exchange {
    landing.degraded.push(Degradation::NoExchange {
      widest_window_ns: landing.widest_window_ns,
    });
  }
  landing.close_dirs();
  let state = terminal_state(&reports, landing.crashed.is_some());
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
  let mut landed: Vec<String> = Vec::with_capacity(reports.len());
  let completed = state != LandingState::Aborted;
  for r in reports
    .iter()
    .filter(|r| completed && advances(r.outcome.as_ref()))
  {
    landed.push(r.path.to_string());
    if let Action::Rename { from } = &r.action {
      landed.push(from.to_string());
    }
  }
  let base = facts.map(|facts| BaseConfig {
    root: target_dir,
    facts,
    large_class_bytes: request.large_class_bytes,
  });
  vol
    .with_host(landing.host)
    .land_advance(store, &landed, base)
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
    entries: reports,
    bytes_written: landing.bytes_written,
    durability,
    degraded: landing.degraded,
    staged,
    ramp_depth: landing.ramp.depth,
    costs,
    swept,
    target: target_dir,
  })
}

/// Whether an outcome takes its entry out of the overlay: the disk holds the overlay's state.
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

fn terminal_state(reports: &[EntryReport], crashed: bool) -> LandingState {
  if crashed {
    return LandingState::Aborted;
  }
  let clean = reports.iter().all(|r| {
    matches!(
      r.outcome,
      Some(Outcome::Written | Outcome::Skipped(_) | Outcome::AcceptedIdentical)
    )
  });
  if clean {
    LandingState::Done
  } else {
    LandingState::Partial
  }
}

/// A staging directory in flight: the hidden sibling beside the target.
struct Stage {
  parent: HostDir,
  name: Box<str>,
  target_name: Box<str>,
}

impl<H: LandFs> Landing<'_, H> {
  /// Stage-and-exchange when the target is empty (no entry to link) or the remembered costs
  /// say staging wins (§4.15 step 10); returns the stage the entries are then written into.
  fn stage_if_better(&mut self, manifest: &Manifest, reports: &[EntryReport]) -> Option<Stage> {
    let (parent, target_name) = self.stage_parent.clone()?;
    if !self.caps.exchange || !self.exchange {
      return None;
    }
    let empty = self
      .host
      .list(self.root)
      .map(|l| l.is_empty())
      .unwrap_or(false);
    let all_apply = reports.iter().all(|r| r.verdict == Some(Verdict::Apply));
    let by_costs = match (self.request.costs, self.request.target_entries) {
      (Some(costs), Some(total)) => costs.prefers_staging(
        total,
        u64::try_from(manifest.entries.len()).unwrap_or(u64::MAX),
        manifest.summary.bytes,
      ),
      _ => false,
    };
    // The populated-target case needs every existing entry linked into the stage, a seam verb
    // that waits on its measured cost (GAPS §8c); until then only an empty target stages.
    if !(empty && all_apply) || by_costs && !empty {
      return None;
    }
    let name: Box<str> = format!(
      "{HIDDEN_PREFIX}{:016x}-{STAGE_SUFFIX}",
      self.request.landing_id
    )
    .into();
    self.host.mkdir(parent, &name, STAGE_MODE).ok()?;
    let stage = self.host.open_dir(parent, &name).ok()?;
    self.root = stage;
    Some(Stage {
      parent,
      name,
      target_name,
    })
  }

  /// Exchanges the built stage with the (empty) target and removes the displaced directory;
  /// on any failure the stage is removed and the target left as it was.
  fn finish_stage(
    &mut self,
    target: &LandingTarget,
    stage: Stage,
    durability: &mut Durability,
  ) -> (bool, HostDir) {
    self.close_dirs();
    let ok = self.crashed.is_none()
      && self
        .host
        .exchange(stage.parent, &stage.name, &stage.target_name)
        .is_ok();
    // Whatever now sits at the hidden name goes: the built tree on failure, the displaced
    // empty target on success. The parent then syncs so the exchange is durable.
    let _ = self.remove_named_tree(stage.parent, &stage.name);
    self.host.close_dir(self.root);
    // The exchanged target is a new inode: a fresh handle names it (the caller's names the
    // displaced one on a real host).
    let target_dir = if ok {
      self
        .host
        .open_dir(stage.parent, &stage.target_name)
        .unwrap_or(target.dir)
    } else {
      target.dir
    };
    self.root = target_dir;
    self.touched.clear();
    let started = Instant::now();
    if self.host.sync_dir(stage.parent).is_err() {
      durability.dirs_synced = false;
    }
    durability.dirs += 1;
    durability.dir_sync_ns = durability.dir_sync_ns.saturating_add(elapsed_ns(started));
    (ok, target_dir)
  }
}
