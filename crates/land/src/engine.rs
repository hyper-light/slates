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
//!   written, given its mode and mtime and written out, and only after the barrier of its batch
//!   (A-130) linked at its name (a create) or exchanged with the old file (a replacement);
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
//! run one at a time; files are staged in batches of at most one copy window of pages and placed
//! after one durability barrier each (A-130: macOS 3–5× faster than a barrier per file; on Linux,
//! where no barrier covers a batch without flushing other writers' data, each write-out is the
//! file's `fdatasync` and the batch barrier adds nothing); bytes are
//! read from the volume into a buffer and written through the seam (arena pages through
//! io_uring in Phase 4); grants, leases and the audit log are the in-process records of
//! [`crate::grant`] and [`Audit`]. Every temporary stays inside the granted directory; replacing
//! the target itself would write its ungranted parent (R1, §4.15 step 10). Reflinks are not used
//! (the probe is recorded as `false`).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Instant;

use slates_vfs::base::{BaseConfig, Landed};
use slates_vfs::error::VfsError;
use slates_vfs::host::{
  Barrier, HostDir, HostError, HostFile, HostKind, LandCapabilities, LandFs, OsCode,
};
use slates_vfs::inode::{Fingerprint, WindowsAttributes, Witness};
use slates_vfs::volume::{Store, Volume};

use crate::grant::{
  GrantBinding, GrantId, GrantRecord, GrantRefusal, GrantScope, Grants, LandingLease,
  LandingLeaseHeld, TargetIdentity, lease_key,
};
use crate::manifest::{
  Action, FileMeta, Filter, Judged, LandingEntry, Manifest, OverlayIdentity, Summary,
};
pub use crate::source::Source;
use crate::verdict::{
  ConflictClass, DiskState, MetaVerdict, Verdict, content_verdict, landed_meta, meta_verdict,
  verdict, with_modified,
};

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
  /// The host refused the write with this code, typed by its numbering (A-539); the temporary was removed.
  Failed {
    /// The code the host named; none where it named none (a fetch under way, A-524).
    code: Option<OsCode>,
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
    /// The code the host named, typed by its numbering (A-539).
    code: OsCode,
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
  /// One batch barrier (A-130), nanoseconds.
  pub barrier_ns: u64,
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
  /// The batch barriers the landing took (A-130): each made a batch of written-out files durable before any of them
  /// was placed, so fewer than the files written whenever a batch held more than one.
  pub barriers: usize,
  /// The files whose bytes were shared with an identical file their batch had already staged rather than written
  /// (§4.15 step 6, A-445): the clone path's own count, so a landing that never clones where it could shows it.
  pub cloned: usize,
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
  /// The run was stepped after it had ended or been abandoned: it has nothing more to do.
  Ended,
  /// The run was stepped while its host was away ([`LandingRun::take_host`]).
  HostAway,
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

  /// The sequence the next record takes, one past the last record's: a reader that notes it before a run knows the
  /// run's records as those from it on (the server mirrors a granted run's records once, §4.15 "Audit").
  pub fn next_seq(&self) -> u64 {
    self.next_seq
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
  barrier_ns: Vec<u64>,
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
        .saturating_add(self.verify_ns.len())
        .saturating_add(self.barrier_ns.len()),
    )
    .unwrap_or(u64::MAX);
    LandingCosts {
      link_ns: median(&mut self.link_ns),
      write_ns_per_kib: median(&mut self.write_ns_per_kib),
      exchange_ns: median(&mut self.exchange_ns),
      verify_ns: median(&mut self.verify_ns),
      dir_sync_ns: median(&mut self.dir_sync_ns),
      barrier_ns: median(&mut self.barrier_ns),
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
  /// Whether the filesystem shares one file's bytes with another (`LandCapabilities::reflink`, A-445): where it does,
  /// a file whose bytes its batch already staged is cloned from that temporary ([`clone_source`]).
  reflink: bool,
  /// The files cloned so far ([`LandingReport::cloned`]).
  cloned: usize,
  hidden_counter: u64,
  bytes_written: u64,
  widest_window_ns: u64,
  degraded: Vec<Degradation>,
  costs: CostSamples,
  /// The batch barriers taken (A-130).
  barriers: usize,
  /// The host's code that ended the landing, when one did (A-539: typed by its numbering).
  crashed: Option<OsCode>,
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
  /// Each entry judged by content (a captured clone's, A-291) whose validation found the disk still holding what the
  /// capture held, by path, with the fingerprint of the object found there: its write is bound to that object, as an
  /// overlay's is to its witness ([`Landing::witness_of`]), so an outsider's change between the validation and the
  /// write is a conflict, never overwritten or removed.
  observed: BTreeMap<Box<str>, Fingerprint>,
  /// Each replacement judged by content whose validation decided its metadata (A-466), by path: what the file lands
  /// with, each field by A-458's rule, and whether its bytes are written or only that metadata set.
  replaced: BTreeMap<Box<str>, ReplacedMeta>,
}

/// A captured clone's replacement as its validation judged its metadata (A-466): the mode and Windows bits the file lands
/// with (`crate::verdict::landed_meta`: the volume's where it changed a field and the disk still held the capture's, the
/// disk's otherwise), and whether its bytes are written. They are not where the disk already held the clone's bytes:
/// then only the metadata the file lacks is set, through its own handle ([`Landing::chmod`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReplacedMeta {
  /// What the file lands with.
  lands: FileMeta,
  /// Whether its bytes are written (staged and placed) or the disk's kept.
  bytes: bool,
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

/// The write phase's state across the entries it writes, kept between slices: the class being written, the file being
/// copied, and the batch staged for its barrier (A-130).
struct WriteProgress {
  /// The instant the landing's request clock was read (`LandingRun::began`): its elapsed clock for the grant and lease
  /// fences and the entries' spans, so the time the plan, the sweep and the validation took counts. It was the write
  /// phase's own start until 2026-11-08, and a landing paused before its writes wrote past its lease's term.
  started: Instant,
  /// The class of the entry last begun (§4.15 step 5): a batch takes its barrier before an entry of another class.
  class: Option<u8>,
  /// When the entry being written began, on the landing's clock (its `land.entry` span's start).
  entry_start_ns: u64,
  /// The file being copied, a window per unit, when the entry is a file past one window.
  copy: Option<FileCopy>,
  /// The files written out and waiting for their batch's barrier, in entry order; once the barrier has returned
  /// (`placing`), the ones still to place. At most [`batch_pages`] of them.
  staged: VecDeque<Staged>,
  /// The metadata entries set in place and waiting for the same barrier to sync their files (A-472), in entry order;
  /// once it has returned, the ones still to report. Each counts one page against [`batch_pages`].
  metas: VecDeque<StagedMeta>,
  /// The pages the staged files count for ([`file_pages`]): at most [`batch_pages`], unless one file alone is larger.
  staged_pages: u64,
  /// Whether the staged batch's barrier has returned: its files are being placed, one per unit.
  placing: bool,
  /// The staged batch's barrier while it is away from the host (A-132).
  barrier: BarrierState,
}

impl WriteProgress {
  fn start(began: Instant) -> Self {
    Self {
      started: began,
      class: None,
      entry_start_ns: 0,
      copy: None,
      staged: VecDeque::new(),
      metas: VecDeque::new(),
      staged_pages: 0,
      placing: false,
      barrier: BarrierState::Idle,
    }
  }
}

/// Where the staged batch's barrier is (A-132): taken from the host, it runs wherever the run's driver puts it — a
/// thread of its own under the server, in place under the one-call landing — and the run steps nothing until its
/// outcome is back, so nothing of the batch is placed before its files are durable.
enum BarrierState {
  /// No barrier is out.
  Idle,
  /// Taken ([`LandFs::begin_barrier`]), waiting for the driver ([`LandingRun::take_barrier`]).
  Taken {
    barrier: Box<dyn Barrier>,
    began: Instant,
  },
  /// With the driver, running.
  Running { began: Instant },
  /// Run, its outcome waiting for the next unit to end it ([`LandFs::end_barrier`]).
  Ran {
    ran: Result<(), HostError>,
    began: Instant,
  },
}

impl BarrierState {
  /// Whether the run waits on the barrier: taken or running, its outcome not yet back.
  fn awaited(&self) -> bool {
    matches!(
      self,
      BarrierState::Taken { .. } | BarrierState::Running { .. }
    )
  }
}

/// The files the staged batch's barrier syncs: its temporaries, then the files its metadata entries set in place
/// (A-472), each in entry order.
fn batch_files(progress: &WriteProgress) -> Vec<HostFile> {
  progress
    .staged
    .iter()
    .map(|staged| staged.copy.temp)
    .chain(progress.metas.iter().map(|meta| meta.file))
    .collect()
}

/// A metadata entry staged for its batch's barrier (A-472): its fields already set in place through its own handle,
/// which stays open for the barrier to sync whole; reported once the barrier returned. A mode or bits set is atomic,
/// so the file holds the old value or the new one whatever happens before then; what waits on the barrier is the
/// report, and with it the volume's advance, which must not outrun what a power loss could take back.
struct StagedMeta {
  /// Its entry's place in the manifest, and in the reports.
  index: usize,
  /// The file, open through the handle that set it.
  file: HostFile,
  /// When its entry began, on the landing's clock (its `land.entry` span's start).
  entry_start_ns: u64,
}

/// What setting a metadata entry's fields came to (A-472): set and staged for the barrier with the file still open, or
/// an outcome now (identical, a conflict, a refusal of that file alone).
enum MetaSet {
  /// Set: the file stays open for its batch's barrier.
  Staged(HostFile),
  /// Nothing set, and this outcome to record.
  Done(Outcome),
}

/// Where one unit left its entry.
enum EntryStep {
  /// Its file is still being copied, a window per unit.
  Copying,
  /// Done with for now without an outcome: its file is staged for its batch's barrier (the outcome lands when it is
  /// placed), or it had no verdict to act on.
  Staged,
  /// Done, with this outcome to record.
  Done(Written),
}

/// A file staged for its batch's barrier (A-130): written out and open at its hidden name, placed only after the
/// barrier returned.
struct Staged {
  /// Its entry's place in the manifest, and in the reports.
  index: usize,
  copy: FileCopy,
  /// The witness a replacement's exchange checks, taken when the file was staged.
  witness: Option<Witness>,
  /// When its entry began, on the landing's clock (its `land.entry` span's start).
  entry_start_ns: u64,
}

/// A file being copied into its temporary, a window per unit (AUD-29-25), so a landing's memory and each
/// unit's work are bounded by the window however large the file.
struct FileCopy {
  temp: HostFile,
  dir: HostDir,
  created: Box<str>,
  inode: slates_vfs::ids::InodeNo,
  /// The next byte to copy.
  next: u64,
  overlay: OverlayIdentity,
  started: Instant,
}

/// The sync phase's tally across the directories it syncs, kept between slices.
#[derive(Default)]
struct SyncProgress {
  dirs: usize,
  total_ns: u64,
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
      Err(HostError::Exists(_)) => self.keep(aside_dir, aside_dir_path, aside, path),
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
      Ok(moved) => Ok(Some(same_object_after_a_move(&witness.fingerprint, &moved))),
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
    if entry.judged == Judged::Content {
      // Judged by content: the write is bound to the object the validation found holding the captured bytes; with
      // none found the witness's fingerprint names nothing on this disk, and the write is refused.
      return Some(Witness {
        fingerprint: self
          .observed
          .get(&entry.path)
          .copied()
          .unwrap_or(witness.fingerprint),
        ..witness
      });
    }
    let restored = self
      .restored
      .get(&entry.path)
      .filter(|after| same_object_after_a_move(&witness.fingerprint, after))
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
    // An entry judged by content compares the disk's bytes with what the capture held, whatever its fingerprint.
    if entry.judged == Judged::Content && matches!(entry.action, Action::Replace | Action::Delete) {
      return Ok(true);
    }
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

  /// One entry's verdict against the disk now (the unit a sliced landing validates per step).
  fn validate_entry(
    &mut self,
    entry: &LandingEntry,
    ours: &Ours,
  ) -> Result<EntryReport, HostError> {
    {
      let hash = self.needs_hash(entry)?;
      // Every entry's directory is synced at the end, written or not, so a resumed landing
      // makes the previous attempt's entries durable too.
      self.touched.insert(split(&entry.path).0.into());
      // A clear's own fresh directory holds only what the manifest makes beneath it; a directory a content manifest
      // removes may hold only what the manifest removes beneath it.
      let check_ours = match (&entry.action, entry.judged) {
        (Action::Clear, _) => Some(&ours.created),
        (Action::Rmdir, Judged::Content) => Some(&ours.removed),
        _ => None,
      };
      let disk = match &entry.action {
        Action::Rename { from } => {
          self.touched.insert(split(from).0.into());
          self.rename_state(from, &entry.path)?
        }
        _ => self.disk_state(&entry.path, hash, check_ours)?,
      };
      let v = match entry.judged {
        Judged::Witness => {
          let witness = self.witness_of(entry);
          verdict(
            &entry.action,
            witness.as_ref(),
            disk,
            entry.overlay.as_ref(),
          )
        }
        Judged::Content => self.judge_by_content(entry, disk)?,
      };
      Ok(EntryReport {
        path: entry.path.clone(),
        action: entry.action.clone(),
        verdict: Some(v),
        outcome: None,
        window_ns: None,
      })
    }
  }

  /// A captured clone's replacement, its bytes judged (`content`), with its metadata judged beside them field by field
  /// (A-466, [`landed_meta`]): a field the clone changed that the disk holds a third way is a conflict
  /// (`ModifyModify`); bytes to write are staged with the metadata decided; bytes the disk already holds are kept,
  /// and where the file lacks a field the clone set, that field alone is set (the entry applies, without its bytes).
  /// Anything else keeps its verdict.
  fn judge_replaced_meta(
    &mut self,
    entry: &LandingEntry,
    content: Verdict,
    disk: DiskState,
  ) -> Verdict {
    let (Action::Replace, Some(witness), Some(overlay), DiskState::File { fingerprint, .. }) = (
      &entry.action,
      entry.witnessed.as_ref(),
      entry.overlay.as_ref(),
      disk,
    ) else {
      return content;
    };
    if !matches!(content, Verdict::Apply | Verdict::AcceptIdentical) {
      return content;
    }
    let found = FileMeta::of_fingerprint(&fingerprint);
    let Some(lands) = landed_meta(
      FileMeta::of_fingerprint(&witness.fingerprint),
      found,
      FileMeta::of_volume(overlay.mode, entry.windows),
    ) else {
      return Verdict::Conflict(ConflictClass::ModifyModify);
    };
    let bytes = content == Verdict::Apply;
    if !bytes && lands == found {
      return Verdict::AcceptIdentical;
    }
    self
      .replaced
      .insert(entry.path.clone(), ReplacedMeta { lands, bytes });
    Verdict::Apply
  }

  /// An entry judged by content (A-291): its verdict, and, where it is written over or removes what the capture held,
  /// the object found there now, which binds the write ([`Landing::observed`]).
  fn judge_by_content(
    &mut self,
    entry: &LandingEntry,
    disk: DiskState,
  ) -> Result<Verdict, HostError> {
    let v = content_verdict(
      &entry.action,
      entry.witnessed.as_ref(),
      disk,
      entry.overlay.as_ref(),
    );
    self.observed.remove(&entry.path);
    self.replaced.remove(&entry.path);
    let v = self.judge_replaced_meta(entry, v, disk);
    if v == Verdict::Apply && matches!(entry.action, Action::Replace | Action::Delete) {
      let found = match (&entry.action, disk) {
        (Action::Replace, DiskState::File { fingerprint, .. }) => Some(fingerprint),
        (Action::Delete, _) => {
          let (dir_path, name) = split(&entry.path);
          let dir = self.open_dir_path(dir_path)?;
          match self.host.entry_fingerprint(dir, name) {
            Ok(found) => Some(found),
            Err(HostError::NotFound) => None,
            Err(e) => return Err(e),
          }
        }
        _ => None,
      };
      if let Some(found) = found {
        self.observed.insert(entry.path.clone(), found);
      }
    }
    Ok(v)
  }

  // ------------------------------------------------------------- writing

  /// One unit of the write phase (§4.15 steps 5–8, A-130) under its `progress`, `next` the cursor over the manifest:
  /// the end of a barrier whose outcome is back (A-132), a placement from the batch whose barrier has returned, the
  /// barrier of a batch that closes before the next entry, or a unit of the next entry (a window of the file being
  /// copied, or the entry itself). Nothing while a barrier is away. Returns whether the phase is done: every entry
  /// taken and no batch left.
  fn write_unit(
    &mut self,
    manifest: &Manifest,
    reports: &mut [EntryReport],
    next: &mut usize,
    cx: &mut WriteContext<'_, H>,
    progress: &mut WriteProgress,
  ) -> bool {
    if progress.barrier.awaited() {
      return false;
    }
    if matches!(progress.barrier, BarrierState::Ran { .. }) {
      self.end_barrier_unit(manifest, reports, cx, progress);
      return false;
    }
    if progress.placing {
      if self.crashed.is_some() {
        self.abandon_batch(manifest, progress);
      } else if let Some(staged) = progress.staged.pop_front() {
        self.place_staged(manifest, reports, staged, cx, progress);
        if progress.staged.is_empty() && progress.metas.is_empty() {
          progress.placing = false;
          progress.staged_pages = 0;
        }
        return false;
      } else if let Some(meta) = progress.metas.pop_front() {
        self.report_meta(manifest, reports, meta, cx, progress);
        if progress.metas.is_empty() {
          progress.placing = false;
          progress.staged_pages = 0;
        }
        return false;
      }
      progress.placing = false;
      progress.staged_pages = 0;
    }
    let entry = manifest.entries.get(*next);
    if progress.copy.is_none()
      && !(progress.staged.is_empty() && progress.metas.is_empty())
      && self.batch_closes(entry, reports.get(*next), progress, cx.store)
    {
      self.barrier_unit(manifest, reports, cx, progress);
      return false;
    }
    let (Some(entry), Some(report)) = (entry, reports.get_mut(*next)) else {
      return true;
    };
    if self.crashed.is_some() {
      return true;
    }
    if self.write_one(manifest, entry, report, *next, cx, progress) {
      *next = next.saturating_add(1);
    }
    false
  }

  /// One unit of the entry at `index`: its verdict decides whether it is written, skipped or refused. A file is copied
  /// into its temporary — a window per unit past one window, so no unit's work grows with a file's size (AUD-29-25) —
  /// and staged for its batch's barrier (A-130); its outcome lands in its report when it is placed. Anything else is
  /// written, or not, here, and its outcome lands now. Returns whether the entry is done with: `false` while its file
  /// is still being copied (`copy` holds it).
  fn write_one(
    &mut self,
    manifest: &Manifest,
    entry: &LandingEntry,
    report: &mut EntryReport,
    index: usize,
    cx: &mut WriteContext<'_, H>,
    progress: &mut WriteProgress,
  ) -> bool {
    let step = match progress.copy.take() {
      Some(copy) => self.copy_unit(entry, index, copy, cx, progress),
      None => self.begin_entry(entry, report, index, cx, progress),
    };
    match step {
      EntryStep::Copying => false,
      EntryStep::Staged => true,
      EntryStep::Done(outcome) => {
        let entry_start_ns = progress.entry_start_ns;
        self.record_entry(
          manifest,
          report,
          outcome,
          cx,
          progress.started,
          entry_start_ns,
        );
        true
      }
    }
  }

  /// The first unit of the entry at `index`, by its verdict: a write it applies (a file's begun, and staged if it fits
  /// one window), or the outcome a skip, an identity or a conflict records.
  fn begin_entry(
    &mut self,
    entry: &LandingEntry,
    report: &EntryReport,
    index: usize,
    cx: &mut WriteContext<'_, H>,
    progress: &mut WriteProgress,
  ) -> EntryStep {
    progress.class = Some(entry.class());
    // The `land.entry` chokepoint span (§4.14) brackets one entry's processing, whatever its verdict: a file's, from
    // here to its placement.
    progress.entry_start_ns = self
      .request
      .now_ns
      .saturating_add(elapsed_ns(progress.started));
    let outcome = match report.verdict {
      Some(Verdict::Apply) => match self.fence(cx, progress.started) {
        Some(skipped) => skipped,
        None => {
          cx.observer.before_write(self.host, entry);
          if self.stages_meta(entry, report) {
            return self.stage_meta(entry, index, cx.store, progress);
          }
          if matches!(entry.action, Action::Create | Action::Replace) {
            return self.begin_file(entry, index, cx, progress);
          }
          return EntryStep::Done(self.write_entry(entry));
        }
      },
      Some(Verdict::Skip) => Outcome::Skipped(skip_reason(entry)),
      Some(Verdict::AcceptIdentical) => Outcome::AcceptedIdentical,
      Some(Verdict::Conflict(c)) => Outcome::Conflict(c),
      // No verdict: nothing to record (the entry was never validated).
      None => return EntryStep::Staged,
    };
    EntryStep::Done(Written {
      outcome,
      window_ns: None,
    })
  }

  /// A file's first unit: its temporary created and, for a file within one window, its bytes copied and the file
  /// staged; a larger one is copied a window per unit from the next.
  fn begin_file(
    &mut self,
    entry: &LandingEntry,
    index: usize,
    cx: &mut WriteContext<'_, H>,
    progress: &mut WriteProgress,
  ) -> EntryStep {
    let window = cx.store.content.chunk_bytes().max(1);
    match self.begin_clone(entry, cx.vol, cx.store, progress) {
      Ok(Some(copy)) => return self.stage_step(entry, index, copy, cx.store, progress),
      Ok(None) => {}
      Err(failure) => return EntryStep::Done(self.written_of(Err(failure))),
    }
    let mut copy = match self.begin_copy(entry, cx.vol, cx.store) {
      Ok(copy) => copy,
      Err(failure) => return EntryStep::Done(self.written_of(Err(failure))),
    };
    if copy.overlay.size > u64::try_from(window).unwrap_or(u64::MAX) {
      progress.copy = Some(copy);
      return EntryStep::Copying;
    }
    if let Err(failure) = self.copy_rest(&mut copy, cx.vol, cx.store, window) {
      self.drop_copy(&entry.path, copy);
      return EntryStep::Done(self.written_of(Err(failure)));
    }
    self.stage_step(entry, index, copy, cx.store, progress)
  }

  /// One window of a file past one window; once it is all copied, the file staged — only while the grant and lease
  /// still hold, since a long copy can outlive them.
  fn copy_unit(
    &mut self,
    entry: &LandingEntry,
    index: usize,
    mut copy: FileCopy,
    cx: &mut WriteContext<'_, H>,
    progress: &mut WriteProgress,
  ) -> EntryStep {
    let window = cx.store.content.chunk_bytes().max(1);
    match self.copy_chunk(&mut copy, cx.vol, cx.store, window) {
      Ok(true) => {
        progress.copy = Some(copy);
        EntryStep::Copying
      }
      Ok(false) => match self.fence(cx, progress.started) {
        Some(skipped) => {
          self.drop_copy(&entry.path, copy);
          EntryStep::Done(Written {
            outcome: skipped,
            window_ns: None,
          })
        }
        None => self.stage_step(entry, index, copy, cx.store, progress),
      },
      Err(failure) => {
        self.drop_copy(&entry.path, copy);
        EntryStep::Done(self.written_of(Err(failure)))
      }
    }
  }

  /// [`Landing::stage_file`] as an entry's step: staged, or its refusal as the entry's outcome.
  fn stage_step(
    &mut self,
    entry: &LandingEntry,
    index: usize,
    copy: FileCopy,
    store: &Store,
    progress: &mut WriteProgress,
  ) -> EntryStep {
    match self.stage_file(entry, index, copy, store, progress) {
      Ok(()) => EntryStep::Staged,
      Err(failure) => EntryStep::Done(self.written_of(Err(failure))),
    }
  }

  /// The skip an ended grant or lease imposes on an entry's next write-capable step, on the landing's elapsed clock
  /// since `started`; `None` while both hold.
  fn fence(&self, cx: &WriteContext<'_, H>, started: Instant) -> Option<Outcome> {
    let now_ns = self.request.now_ns.saturating_add(elapsed_ns(started));
    if grant_ended(cx.grant, now_ns) {
      Some(Outcome::Skipped(SkipReason::GrantEnded))
    } else if cx.lease.expires_ns <= now_ns {
      Some(Outcome::Skipped(SkipReason::LeaseEnded))
    } else {
      None
    }
  }

  /// Whether the staged batch takes its barrier before `entry` (none: the entries are done) (A-130): the landing
  /// crashed (its batch is then abandoned), the entries are done, the entry opens another class (a class starts only
  /// when the one before it is placed, §4.15 step 5), or it is a file the batch has no room for. A batch holds at most
  /// a copy window of pages, each file counted as at least one: its barrier's work is bounded by the window, as every
  /// other unit's is (AUD-29-25), and so are the temporaries it holds open.
  fn batch_closes(
    &self,
    entry: Option<&LandingEntry>,
    report: Option<&EntryReport>,
    progress: &WriteProgress,
    store: &Store,
  ) -> bool {
    if self.crashed.is_some() {
      return true;
    }
    let (Some(entry), Some(report)) = (entry, report) else {
      return true;
    };
    if progress.class != Some(entry.class()) {
      return true;
    }
    let Some(pages) = self.batch_pages_of(entry, report, store) else {
      return false;
    };
    progress.staged_pages.saturating_add(pages) > batch_pages(store)
  }

  /// The pages `entry` would take in a batch: a staged file's ([`file_pages`]), one for a metadata entry, whose file
  /// the batch holds open (A-472), and none for an entry the batch does not hold.
  fn batch_pages_of(
    &self,
    entry: &LandingEntry,
    report: &EntryReport,
    store: &Store,
  ) -> Option<u64> {
    if self.stages_meta(entry, report) {
      return Some(file_pages(0, store));
    }
    stages_a_file(entry, report)
      .then(|| file_pages(entry.overlay.map_or(0, |overlay| overlay.size), store))
  }

  /// Whether `entry` is a metadata entry its verdict applies (A-472): a file's mode, bits or time changed alone
  /// (`Chmod`), a base directory's mode or bits (`ChmodDir`, A-501), or a captured clone's replacement whose bytes the
  /// disk already holds (A-466), set in place and synced by its batch's barrier.
  fn stages_meta(&self, entry: &LandingEntry, report: &EntryReport) -> bool {
    matches!(report.verdict, Some(Verdict::Apply))
      && (matches!(entry.action, Action::Chmod { .. } | Action::ChmodDir { .. })
        || self.sets_meta_alone(entry))
  }

  /// The barrier of the staged batch taken (§4.15 step 8, A-130, A-132): every staged file is to be durable before any
  /// of them is placed, so the run waits while the barrier runs away from it ([`LandingRun::take_barrier`]), and a
  /// later unit ends it ([`Landing::end_barrier_unit`]); the placements follow, one per unit. A crashed landing abandons
  /// its batch instead (its entries stay unwritten, and the resume lands them); a grant or lease that ended skips the
  /// batch's entries; a barrier the host refuses fails every entry of the batch, as a refused sync failed its one file
  /// before batches.
  fn barrier_unit(
    &mut self,
    manifest: &Manifest,
    reports: &mut [EntryReport],
    cx: &mut WriteContext<'_, H>,
    progress: &mut WriteProgress,
  ) {
    if self.crashed.is_some() {
      self.abandon_batch(manifest, progress);
      return;
    }
    let refused = match self.fence(cx, progress.started) {
      Some(skipped) => Ok(skipped),
      None => {
        let began = Instant::now();
        match self.host.begin_barrier(&batch_files(progress)) {
          Ok(barrier) => {
            progress.barrier = BarrierState::Taken { barrier, began };
            return;
          }
          Err(error) => Err(error),
        }
      }
    };
    self.settle_batch(manifest, reports, cx, progress, &refused);
  }

  /// The staged batch's barrier ended once its outcome is back (A-132): placed from the next unit on when every file is
  /// durable, its entries failed when the barrier was refused.
  fn end_barrier_unit(
    &mut self,
    manifest: &Manifest,
    reports: &mut [EntryReport],
    cx: &mut WriteContext<'_, H>,
    progress: &mut WriteProgress,
  ) {
    let BarrierState::Ran { ran, began } =
      std::mem::replace(&mut progress.barrier, BarrierState::Idle)
    else {
      return;
    };
    match self.host.end_barrier(&batch_files(progress), ran) {
      Ok(()) => {
        self.costs.barrier_ns.push(elapsed_ns(began));
        self.barriers = self.barriers.saturating_add(1);
        progress.placing = true;
      }
      Err(error) => self.settle_batch(manifest, reports, cx, progress, &Err(error)),
    }
  }

  /// A staged batch that will not be placed: each temporary closed and removed, each metadata entry's file closed (A-472),
  /// each entry given `refused` — skipped (its grant or lease ended) or failed (its barrier refused).
  fn settle_batch(
    &mut self,
    manifest: &Manifest,
    reports: &mut [EntryReport],
    cx: &mut WriteContext<'_, H>,
    progress: &mut WriteProgress,
    refused: &Result<Outcome, HostError>,
  ) {
    progress.staged_pages = 0;
    while let Some(staged) = progress.staged.pop_front() {
      let path = manifest
        .entries
        .get(staged.index)
        .map_or("/", |entry| entry.path.as_ref());
      self.drop_copy(path, staged.copy);
      let written = match refused {
        Ok(skipped) => Written {
          outcome: skipped.clone(),
          window_ns: None,
        },
        Err(error) => self.written_of(Err((*error).into())),
      };
      if let Some(report) = reports.get_mut(staged.index) {
        self.record_entry(
          manifest,
          report,
          written,
          cx,
          progress.started,
          staged.entry_start_ns,
        );
      }
    }
    // A metadata entry's fields are already set: unsynced, it is recorded as its batch is (skipped or failed), which
    // never advances it, so the volume keeps it and the next landing judges the file again (A-472).
    while let Some(meta) = progress.metas.pop_front() {
      self.host.close_file(meta.file);
      let written = match refused {
        Ok(skipped) => Written {
          outcome: skipped.clone(),
          window_ns: None,
        },
        Err(error) => self.written_of(Err((*error).into())),
      };
      if let Some(report) = reports.get_mut(meta.index) {
        self.record_entry(
          manifest,
          report,
          written,
          cx,
          progress.started,
          meta.entry_start_ns,
        );
      }
    }
  }

  /// A crashed landing's batch abandoned: each staged temporary closed and removed (a removal the host refuses is
  /// reported, as any temporary left behind is); its entries stay unwritten, and the resume lands them.
  fn abandon_batch(&mut self, manifest: &Manifest, progress: &mut WriteProgress) {
    progress.placing = false;
    progress.staged_pages = 0;
    while let Some(staged) = progress.staged.pop_front() {
      let path = manifest
        .entries
        .get(staged.index)
        .map_or("/", |entry| entry.path.as_ref());
      self.drop_copy(path, staged.copy);
    }
    while let Some(meta) = progress.metas.pop_front() {
      self.host.close_file(meta.file);
    }
  }

  /// One staged file placed after its batch's barrier, its outcome landing in its report: linked at its name, or
  /// exchanged with the witnessed file it replaces — unless the grant or lease ended since, when it is removed
  /// unplaced.
  fn place_staged(
    &mut self,
    manifest: &Manifest,
    reports: &mut [EntryReport],
    staged: Staged,
    cx: &mut WriteContext<'_, H>,
    progress: &WriteProgress,
  ) {
    let Staged {
      index,
      copy,
      witness,
      entry_start_ns,
    } = staged;
    let Some(entry) = manifest.entries.get(index) else {
      self.drop_copy("/", copy);
      return;
    };
    let written = match self.fence(cx, progress.started) {
      Some(skipped) => {
        self.drop_copy(&entry.path, copy);
        Written {
          outcome: skipped,
          window_ns: None,
        }
      }
      None => {
        let placed = self.place_file(entry, copy, witness.as_ref());
        self.written_of(placed)
      }
    };
    if let Some(report) = reports.get_mut(index) {
      self.record_entry(
        manifest,
        report,
        written,
        cx,
        progress.started,
        entry_start_ns,
      );
    }
  }

  /// The write phase's open temporaries closed when its run is abandoned: the file being copied and every staged one,
  /// none placed. A named one stays at its hidden name for the resume's sweep, the state a crash leaves; before
  /// 2026-10-10 an abandoned run left the copy's descriptor open in the host for as long as the host lived.
  fn release_write(&mut self, progress: &mut WriteProgress) {
    // A barrier still with the run goes unrun: its own descriptors close with it.
    progress.barrier = BarrierState::Idle;
    if let Some(copy) = progress.copy.take() {
      self.host.close_file(copy.temp);
    }
    while let Some(staged) = progress.staged.pop_front() {
      self.host.close_file(staged.copy.temp);
    }
    while let Some(meta) = progress.metas.pop_front() {
      self.host.close_file(meta.file);
    }
    progress.placing = false;
    progress.staged_pages = 0;
  }

  /// An entry's outcome recorded: its `land.entry` span (from `entry_start_ns` to now, on the clock started at
  /// `started`), its audit record, its report.
  fn record_entry(
    &mut self,
    manifest: &Manifest,
    report: &mut EntryReport,
    outcome: Written,
    cx: &mut WriteContext<'_, H>,
    started: Instant,
    entry_start_ns: u64,
  ) {
    let now_ns = self.request.now_ns.saturating_add(elapsed_ns(started));
    cx.observer.after_entry(entry_start_ns, now_ns);
    cx.audit.push(AuditRecord {
      seq: 0,
      at_ns: now_ns,
      kind: if outcome.outcome == Outcome::Written {
        AuditKind::EntryWritten
      } else {
        AuditKind::EntryRefused
      },
      grant: cx.grant.map(|g| g.id),
      landing: self.request.landing_id,
      manifest: manifest.hash,
      outcome: None,
    });
    report.outcome = Some(outcome.outcome);
    report.window_ns = outcome.window_ns;
  }

  /// Writes one entry other than a file, mapping a host refusal to its outcome: a full disk fails the entry, any
  /// other unavailability ends the landing (the target is gone or the disk is).
  fn write_entry(&mut self, entry: &LandingEntry) -> Written {
    let attempted = self.try_write_entry(entry);
    self.written_of(attempted)
  }

  /// A write's result as its entry's outcome (see [`Landing::write_entry`]).
  fn written_of(&mut self, attempted: Result<Written, WriteFailure>) -> Written {
    match attempted {
      Ok(w) => w,
      // No room fails its entry alone, and the landing goes on (A-539: the seam names it, whatever the platform numbers).
      Err(WriteFailure::Host(HostError::NoSpace(code))) => Written {
        outcome: Outcome::Failed { code: Some(code) },
        window_ns: None,
      },
      // Any other refusal the host names a code for — a name in use or a verb missing where the landing expected
      // neither, or anything it could not serve — ends the landing, whatever the code's number.
      Err(WriteFailure::Host(
        HostError::Exists(code) | HostError::Unsupported(code) | HostError::Unavailable(code),
      )) => {
        self.crashed = Some(code);
        Written {
          outcome: Outcome::Failed { code: Some(code) },
          window_ns: None,
        }
      }
      // An entry below the target on a mount this daemon serves (A-227): refused before any open, that entry alone.
      Err(WriteFailure::Host(HostError::OwnMount(code))) => Written {
        outcome: Outcome::Failed { code: Some(code) },
        window_ns: None,
      },
      Err(WriteFailure::Host(HostError::NotFound | HostError::NotDirectory)) => Written {
        outcome: Outcome::Skipped(SkipReason::ParentMissing),
        window_ns: None,
      },
      Err(WriteFailure::Host(HostError::NotFile | HostError::StaleHandle)) => Written {
        outcome: Outcome::Conflict(ConflictClass::TypeChanged),
        window_ns: None,
      },
      // A landing's target is a directory host, which never answers a pending fetch (an S3 base's, A-524); were one to,
      // that entry fails alone, with no code to name.
      Err(WriteFailure::Host(HostError::Pending)) => Written {
        outcome: Outcome::Failed { code: None },
        window_ns: None,
      },
      Err(WriteFailure::Volume(e)) => Written {
        outcome: Outcome::Volume(e),
        window_ns: None,
      },
    }
  }

  fn try_write_entry(&mut self, entry: &LandingEntry) -> Result<Written, WriteFailure> {
    let (dir_path, name) = split(&entry.path);
    let outcome = match &entry.action {
      Action::Mkdir => {
        let dir = self.open_dir_path(dir_path)?;
        let mode = entry.overlay.map_or(STAGE_MODE, |o| o.mode);
        self.touched.insert(dir_path.into());
        match self.host.mkdir(dir, name, mode, entry.windows.bits()) {
          Ok(()) => Outcome::Written,
          Err(HostError::Exists(_)) => self.existing_directory(dir, name)?,
          Err(e) => return Err(e.into()),
        }
      }
      Action::Symlink { target } => {
        let dir = self.open_dir_path(dir_path)?;
        self.touched.insert(dir_path.into());
        // Where a writer that makes a link in two steps stages it (Windows, A-438): this landing's own hidden name,
        // which its sweep removes after a crash between the steps.
        let staged = self.hidden_name();
        match self
          .host
          .symlink(dir, name, target, &staged, entry.windows.bits())
        {
          Ok(()) => Outcome::Written,
          Err(HostError::Exists(_)) => Outcome::Conflict(ConflictClass::TargetInUse),
          Err(e) => return Err(e.into()),
        }
      }
      // A file is staged for its batch's barrier by `write_one` (A-130), and a metadata entry too (A-472, A-501): never
      // written here.
      Action::Create | Action::Replace | Action::Chmod { .. } | Action::ChmodDir { .. } => {
        return Err(WriteFailure::Volume(VfsError::Invalid));
      }
      Action::Delete => {
        // An entry judged by content is removed as the object its validation found (A-291).
        let witness = match entry.judged {
          Judged::Witness => entry.witnessed,
          Judged::Content => self.witness_of(entry),
        };
        self.delete(dir_path, name, witness.as_ref())?
      }
      Action::Rmdir if entry.judged == Judged::Content => {
        self.remove_emptied_dir(dir_path, name)?
      }
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

  /// The rest of a file no larger than one window copied into its temporary (one window; a short read from the volume
  /// takes another).
  fn copy_rest(
    &mut self,
    copy: &mut FileCopy,
    vol: &mut Volume,
    store: &mut Store,
    window: usize,
  ) -> Result<(), WriteFailure> {
    while self.copy_chunk(copy, vol, store, window)? {}
    Ok(())
  }

  /// A file's copy begun: its temporary created at a plain hidden name in its directory. Every temporary is
  /// created at a plain hidden name, the landing's own by its name: a crash while it is being written leaves it
  /// there, and the sweep removes it. A replacement's complete temporary then takes the entry's aside name,
  /// where the exchange leaves the displaced entry and a sweep after a crash checks it before removing it
  /// (AUD-29-04). Before 2026-09-30 a replacement's temporary was created at its aside name, where a crash
  /// mid-write left bytes that matched neither the witness nor the overlay, and the resume kept them (a
  /// named-temporary filesystem: APFS).
  /// A file's bytes shared with an identical file its batch has already staged (§4.15 step 6, A-445: "reflink instead
  /// of writev when the target filesystem supports it and an identical file already exists in the target tree, found
  /// through the landing's own hash index of the manifest, never by scanning the disk"): the staged temporary with the
  /// same BLAKE3 and length ([`clone_source`]), cloned into a new temporary that is then complete, as a copy is once its
  /// last window is written. `None` where the filesystem has no reflinks, the batch holds no such file, or the host
  /// refused to share this pair (`EOPNOTSUPP`, nothing made): the bytes are then written as they always were.
  fn begin_clone(
    &mut self,
    entry: &LandingEntry,
    vol: &mut Volume,
    store: &mut Store,
    progress: &WriteProgress,
  ) -> Result<Option<FileCopy>, WriteFailure> {
    if !self.reflink {
      return Ok(None);
    }
    let overlay = entry
      .overlay
      .ok_or(WriteFailure::Volume(VfsError::Invalid))?;
    let Some(from) = clone_source(&progress.staged, &overlay) else {
      return Ok(None);
    };
    let (dir_path, _) = split(&entry.path);
    let located = self.request.source.resolve(vol, store, &entry.path)?;
    let dir = self.open_dir_path(dir_path)?;
    self.touched.insert(dir_path.into());
    let created = self.hidden_name();
    let started = Instant::now();
    let temp = match self.host.clone_temp(dir, &created, from, overlay.size) {
      Ok(temp) => temp,
      // The filesystem shares no extents (A-445): the bytes are written instead.
      Err(HostError::Unsupported(_)) => return Ok(None),
      Err(e) => return Err(e.into()),
    };
    self.cloned = self.cloned.saturating_add(1);
    Ok(Some(FileCopy {
      temp,
      dir,
      created,
      inode: located.inode,
      next: overlay.size,
      overlay,
      started,
    }))
  }

  fn begin_copy(
    &mut self,
    entry: &LandingEntry,
    vol: &mut Volume,
    store: &mut Store,
  ) -> Result<FileCopy, WriteFailure> {
    let (dir_path, _) = split(&entry.path);
    let overlay = entry
      .overlay
      .ok_or(WriteFailure::Volume(VfsError::Invalid))?;
    let located = self.request.source.resolve(vol, store, &entry.path)?;
    let dir = self.open_dir_path(dir_path)?;
    self.touched.insert(dir_path.into());
    let created = self.hidden_name();
    let temp = self.host.create_temp(dir, &created)?;
    Ok(FileCopy {
      temp,
      dir,
      created,
      inode: located.inode,
      next: 0,
      overlay,
      started: Instant::now(),
    })
  }

  /// One window of the file read from the volume (which may read a large-class file's unwritten windows from
  /// the base) and written into the temporary: whether more remain. A file the volume holds shorter than its
  /// manifest's size is refused, never padded.
  fn copy_chunk(
    &mut self,
    copy: &mut FileCopy,
    vol: &mut Volume,
    store: &mut Store,
    window: usize,
  ) -> Result<bool, WriteFailure> {
    let remaining = copy.overlay.size.saturating_sub(copy.next);
    if remaining == 0 {
      return Ok(false);
    }
    let len = usize::try_from(remaining).unwrap_or(usize::MAX).min(window);
    let mut bytes = vec![0u8; len];
    let read = self
      .request
      .source
      .read_at(vol, store, self.host, copy.inode, copy.next, &mut bytes)?;
    let Some(chunk) = bytes.get(..read).filter(|chunk| !chunk.is_empty()) else {
      return Err(WriteFailure::Volume(VfsError::Invalid));
    };
    self.host.write_at(copy.temp, copy.next, chunk)?;
    copy.next = copy
      .next
      .saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
    Ok(copy.next < copy.overlay.size)
  }

  /// A copied file staged for its batch's barrier (A-130): its mode and mtime set and its write-out done, then held open
  /// at its hidden name with the witness its placement will check. A refusal removes the temporary and is the entry's.
  fn stage_file(
    &mut self,
    entry: &LandingEntry,
    index: usize,
    copy: FileCopy,
    store: &Store,
    progress: &mut WriteProgress,
  ) -> Result<(), WriteFailure> {
    let (dir_path, _) = split(&entry.path);
    // An overlay's file lands with the volume's mode and bits, its verdict having found the disk at its witness; a
    // captured clone's replacement with what its validation decided field by field (A-466).
    let (mode, windows) = match self.replaced.get(&entry.path) {
      Some(replaced) => (
        (copy.overlay.mode & !FileMeta::PERMISSIONS) | replaced.lands.permissions,
        replaced.lands.windows,
      ),
      None => (copy.overlay.mode, entry.windows.bits()),
    };
    let filled = self
      .host
      .set_mode(copy.temp, mode)
      .and_then(|()| self.host.set_windows(copy.temp, windows))
      .and_then(|()| self.host.set_mtime(copy.temp, copy.overlay.mtime_ns))
      .and_then(|()| self.host.write_out(copy.temp));
    if let Err(e) = filled {
      self.give_up_temp(copy.dir, dir_path, &copy.created, copy.temp);
      return Err(e.into());
    }
    /// Format: bytes per KiB.
    const KIB: u64 = 1024;
    let kib = copy.overlay.size.div_ceil(KIB).max(1);
    self
      .costs
      .write_ns_per_kib
      .push(elapsed_ns(copy.started).checked_div(kib).unwrap_or(0));
    let witness = match entry.action {
      Action::Replace => self.witness_of(entry),
      _ => None,
    };
    progress.staged_pages = progress
      .staged_pages
      .saturating_add(file_pages(copy.overlay.size, store));
    progress.staged.push_back(Staged {
      index,
      copy,
      witness,
      entry_start_ns: progress.entry_start_ns,
    });
    Ok(())
  }

  /// A staged file placed after its batch's barrier: linked at its name, or exchanged with the witnessed file it
  /// replaces; then closed, the fingerprint its placement left read through its own descriptor first.
  fn place_file(
    &mut self,
    entry: &LandingEntry,
    copy: FileCopy,
    witnessed: Option<&Witness>,
  ) -> Result<Written, WriteFailure> {
    let (_, name) = split(&entry.path);
    let FileCopy {
      temp,
      dir,
      created,
      overlay,
      ..
    } = copy;
    let result = match witnessed {
      None => self.link_create(temp, dir, &created, name),
      Some(witness) => {
        let aside = self.aside_name(&entry.path);
        self.swap_replace(Swap {
          temp,
          dir,
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

  /// A copy that will not be placed (a failure, or its grant or lease ended while it ran): its temporary
  /// closed and removed.
  fn drop_copy(&mut self, path: &str, copy: FileCopy) {
    let (dir_path, _) = split(path);
    self.give_up_temp(copy.dir, dir_path, &copy.created, copy.temp);
  }

  /// Closes a temporary and removes it at its hidden name. An unnamed temporary has no name to remove
  /// (`NotFound`); a named one that stays is reported, never dropped (AUD-29-05).
  fn give_up_temp(&mut self, dir: HostDir, dir_path: &str, hidden: &str, temp: HostFile) {
    self.host.close_file(temp);
    match self.host.unlink(dir, hidden) {
      Ok(()) | Err(HostError::NotFound) => {}
      Err(error) => self.degraded.push(Degradation::Leftover {
        path: join(dir_path, hidden).into(),
        error,
      }),
    }
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
    // The placement leaves the temporary at its name alone (a named one moved there from its hidden name); a name that
    // appeared meanwhile refused it, and it stays at its hidden name for the give-up below.
    let outcome = match placed {
      Ok(()) => Outcome::Written,
      Err(HostError::Exists(_)) => {
        match self.host.unlink(dir, hidden) {
          Ok(()) | Err(HostError::NotFound) => {}
          Err(e) => return Err(e.into()),
        }
        Outcome::Conflict(ConflictClass::TargetInUse)
      }
      Err(e) => return Err(e.into()),
    };
    Ok(Written {
      outcome,
      window_ns: None,
    })
  }

  /// A replacement: place the temporary at its hidden name, hold the old file, exchange, and
  /// verify the displaced file is the witnessed one; else exchange back.
  fn swap_replace(&mut self, swap: Swap<'_>) -> Result<Written, WriteFailure> {
    // The complete temporary takes its aside name, its one name after (a named one moved there from its creation
    // name).
    self.host.place(swap.temp, swap.dir, swap.hidden)?;
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
      Err(HostError::Unsupported(_)) => {
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
    if !same_object_after_a_move(&witness.fingerprint, &before) {
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
        HostError::Exists(_) => Ok(Written {
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

  /// A metadata entry staged for its batch's barrier (A-458, A-472): its fields set in place through its own handle
  /// ([`Landing::set_meta`]), the handle kept open in the batch so the barrier syncs the file whole, and its outcome
  /// recorded once the barrier returned ([`Landing::report_meta`]); an entry with nothing to set, or refused, is
  /// recorded now.
  fn stage_meta(
    &mut self,
    entry: &LandingEntry,
    index: usize,
    store: &Store,
    progress: &mut WriteProgress,
  ) -> EntryStep {
    let (dir_path, name) = split(&entry.path);
    let set = metadata_of(entry).and_then(|change| self.set_meta(dir_path, name, change));
    match set {
      Ok(MetaSet::Staged(file)) => {
        progress.staged_pages = progress.staged_pages.saturating_add(file_pages(0, store));
        progress.metas.push_back(StagedMeta {
          index,
          file,
          entry_start_ns: progress.entry_start_ns,
        });
        EntryStep::Staged
      }
      Ok(MetaSet::Done(outcome)) => EntryStep::Done(Written {
        outcome,
        window_ns: None,
      }),
      Err(failure) => EntryStep::Done(self.written_of(Err(failure))),
    }
  }

  /// A metadata entry's fields set (A-458): the file opened through its own handle, never through a link
  /// ([`LandFs::open_to_chmod`]; a directory's, [`LandFs::open_dir_to_chmod`], A-501), and judged again by that handle's
  /// `fstat`, field by field ([`meta_verdict`], a file's time beside its mode, [`with_modified`]), so what is set lands
  /// on the object judged and a field an outsider changed since the validation is never overwritten (`TargetInUse`).
  /// Then each field to set is set through the handle (`set_mtime`, `set_windows`, `set_mode`); no byte is written, and
  /// the handle stays open for the batch's barrier. A check and a set are two calls on one handle: an outsider's change
  /// to the same field between them is the one it cannot see, as POSIX and Windows offer no compare-and-set of a mode.
  /// A file the landing may not open so (its owner may not read it; on Windows it is read-only, and its flush needs a
  /// handle that may write, A-472) fails its entry alone; a directory's name that holds another kind now is a type
  /// change.
  fn set_meta(
    &mut self,
    dir_path: &str,
    name: &str,
    change: MetaChange,
  ) -> Result<MetaSet, WriteFailure> {
    let dir = self.open_dir_path(dir_path)?;
    self.touched.insert(dir_path.into());
    let opened = if change.directory {
      self.host.open_dir_to_chmod(dir, name)
    } else {
      self.host.open_to_chmod(dir, name)
    };
    let file = match opened {
      Ok(file) => file,
      Err(HostError::NotFound) => {
        return Ok(MetaSet::Done(Outcome::Conflict(ConflictClass::TargetInUse)));
      }
      Err(HostError::NotDirectory | HostError::NotFile) if change.directory => {
        return Ok(MetaSet::Done(Outcome::Conflict(ConflictClass::TypeChanged)));
      }
      // Refused by any kind the host names a code for, this entry fails alone (A-519, A-527): the landing tells none of
      // them apart here (A-539).
      Err(e) => {
        return match named_code(e) {
          Some(code) => Ok(MetaSet::Done(Outcome::Failed { code: Some(code) })),
          None => Err(e.into()),
        };
      }
    };
    let set = self.set_meta_through(file, change);
    if !matches!(set, Ok(MetaSet::Staged(_))) {
      self.host.close_file(file);
    }
    set
  }

  /// [`Landing::set_meta`] through its open file. The time is set first, so a mode set after it never stands between
  /// the handle and its time.
  fn set_meta_through(
    &mut self,
    file: HostFile,
    change: MetaChange,
  ) -> Result<MetaSet, WriteFailure> {
    let found = self.host.fstat(file)?;
    let disk = if change.directory {
      FileMeta::of_dir_fingerprint(&found)
    } else {
      FileMeta::of_fingerprint(&found)
    };
    let mut meta = meta_verdict(change.from, disk, change.to);
    if let Some((witnessed, volume)) = change.modified {
      meta = with_modified(meta, witnessed, found.mtime_ns, volume);
    }
    let (permissions, windows, modified) = match meta {
      MetaVerdict::Conflict => {
        return Ok(MetaSet::Done(Outcome::Conflict(ConflictClass::TargetInUse)));
      }
      MetaVerdict::Held => return Ok(MetaSet::Done(Outcome::AcceptedIdentical)),
      MetaVerdict::Set {
        permissions,
        windows,
        modified,
      } => (permissions, windows, modified),
    };
    if let Some(mtime_ns) = modified {
      self.host.set_mtime(file, mtime_ns)?;
    }
    if let Some(windows) = windows {
      self.host.set_windows(file, windows)?;
    }
    if let Some(permissions) = permissions {
      // The object's own type bits, beside the volume's permission bits.
      let mode = (found.mode & !FileMeta::PERMISSIONS) | permissions;
      self.host.set_mode(file, mode)?;
    }
    Ok(MetaSet::Staged(file))
  }

  /// A staged metadata entry reported after its batch's barrier synced its file (A-472): the fingerprint the set left
  /// recorded for the advance, which a landing of a snapshot rebases the head's file on ([`advance_step`]), the file
  /// closed, the entry written. Its fields were set inside the lease, before the barrier, so a grant or lease that ended
  /// since leaves it written: nothing is written here.
  fn report_meta(
    &mut self,
    manifest: &Manifest,
    reports: &mut [EntryReport],
    meta: StagedMeta,
    cx: &mut WriteContext<'_, H>,
    progress: &WriteProgress,
  ) {
    let StagedMeta {
      index,
      file,
      entry_start_ns,
    } = meta;
    let left = self.host.fstat(file);
    self.host.close_file(file);
    let written = match (left, manifest.entries.get(index)) {
      (Ok(placed), Some(entry)) => {
        self.placed.insert(entry.path.clone(), placed);
        Written {
          outcome: Outcome::Written,
          window_ns: None,
        }
      }
      (Ok(_), None) => return,
      (Err(error), _) => self.written_of(Err(error.into())),
    };
    if let Some(report) = reports.get_mut(index) {
      self.record_entry(
        manifest,
        report,
        written,
        cx,
        progress.started,
        entry_start_ns,
      );
    }
  }

  /// Whether `entry` is a captured clone's replacement whose bytes the disk already holds, so its validation decided to
  /// set its metadata alone (A-466).
  fn sets_meta_alone(&self, entry: &LandingEntry) -> bool {
    self
      .replaced
      .get(&entry.path)
      .is_some_and(|replaced| !replaced.bytes)
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

  /// A directory a content manifest removes (A-291): by then the landing removed what it held, so it goes only while
  /// empty, and one an outsider wrote into since its validation stays with what it holds (`TargetInUse`). Never a
  /// recursive removal: what the capture never held is not this landing's to remove.
  fn remove_emptied_dir(&mut self, dir_path: &str, name: &str) -> Result<Outcome, WriteFailure> {
    let dir = self.open_dir_path(dir_path)?;
    self.touched.insert(dir_path.into());
    // The handles this landing holds beneath it are closed first: a host may refuse to remove a directory held open.
    self.forget_dirs_under(&join(dir_path, name));
    match self.host.rmdir(dir, name) {
      Ok(()) => Ok(Outcome::Written),
      Err(HostError::NotFound) => Ok(Outcome::Skipped(SkipReason::AlreadyThere)),
      // Not empty (`ENOTEMPTY`, or POSIX's other answer `EEXIST`), in use, or refused by any other kind the host names
      // a code for (A-539): the outsider's, kept.
      Err(e) if named_code(e).is_some() => Ok(Outcome::Conflict(ConflictClass::TargetInUse)),
      Err(e) => Err(e.into()),
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
        let outcome = match self.host.mkdir(dir, name, mode, entry.windows.bits()) {
          Ok(()) => Outcome::Written,
          Err(HostError::Exists(_)) => Outcome::Conflict(ConflictClass::TargetInUse),
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
    self.host.mkdir(dir, &fresh, mode, entry.windows.bits())?;
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
          Err(e) if named_code(e).is_some() => self.keep(dir, split(path).0, fresh, path)?,
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
        HostError::Exists(_) => Ok(Written {
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
      Err(HostError::Unsupported(_)) => {
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
        HostError::Exists(_) => Ok(Outcome::Undone(ConflictClass::TargetInUse)),
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
  /// Syncs one touched directory (the unit a sliced landing syncs per step).
  fn sync_one(&mut self, path: Box<str>, progress: &mut SyncProgress) {
    let started = Instant::now();
    let synced = self
      .open_dir_path(&path)
      .and_then(|dir| self.host.sync_dir(dir));
    let ns = elapsed_ns(started);
    progress.total_ns = progress.total_ns.saturating_add(ns);
    self.costs.dir_sync_ns.push(ns);
    match synced {
      Ok(()) => progress.dirs = progress.dirs.saturating_add(1),
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

  /// Ends the sync phase: the media barrier the grant asked for, and the durability reached.
  fn finish_sync(&mut self, progress: &SyncProgress) -> Durability {
    let (dirs, total_ns) = (progress.dirs, progress.total_ns);
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

  /// A host refusal that ends the landing: one the host names a code for other than no room (a host that stopped
  /// answering — the simulated crash, an `EIO`, a Win32 code) aborts the landing so nothing is advanced; no room and the
  /// typed refusals (not found, not a directory, a stale handle) are recorded by the caller and the landing goes on.
  fn note_crash(&mut self, error: HostError) {
    if let HostError::Exists(code) | HostError::Unsupported(code) | HostError::Unavailable(code) =
      error
    {
      self.crashed = Some(code);
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

  /// Settles the hidden siblings carrying this landing id in one of the manifest's directories (a crashed
  /// earlier attempt of the same landing) — the unit a run sweeps per step, over [`sweep_parents`], so the
  /// sweep is proportional to the delta's directories: how many it removed. A sibling it cannot settle is
  /// reported `Leftover`, and a directory it cannot list `Unswept` (AUD-29-05; before 2026-09-29 both were
  /// dropped, and a failed sweep counted zero).
  fn sweep_parent(&mut self, parent: Box<str>, manifest: &Manifest) -> usize {
    let mut removed = 0usize;
    {
      let listed = self
        .open_dir_path(&parent)
        .and_then(|dir| self.host.list(dir).map(|entries| (dir, entries)));
      let (dir, entries) = match listed {
        Ok(found) => found,
        // A directory the plan names that is not there (one this landing creates) holds no sibling.
        Err(HostError::NotFound | HostError::NotDirectory) => return 0,
        Err(error) => {
          self
            .degraded
            .push(Degradation::Unswept { dir: parent, error });
          return 0;
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
          Err(HostError::Exists(_)) => {}
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
        // Not empty, or refused by any kind the host names a code for (A-539): never this landing's.
        Err(e) if named_code(e).is_some() => Ok(false),
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

/// The directories a manifest's entries live in, renames' origins included: where a crashed attempt's hidden
/// siblings can be.
fn sweep_parents(manifest: &Manifest) -> Vec<Box<str>> {
  let mut parents: BTreeSet<Box<str>> = BTreeSet::new();
  for entry in &manifest.entries {
    parents.insert(split(&entry.path).0.into());
    if let Action::Rename { from } = &entry.action {
      parents.insert(split(from).0.into());
    }
  }
  parents.into_iter().collect()
}

/// The paths a manifest makes and removes: what a resumed clear recognizes as its own directory's, and what a directory
/// a content manifest removes may still hold at its validation (A-291).
struct Ours {
  /// The paths the manifest creates.
  created: BTreeSet<String>,
  /// The paths the manifest removes.
  removed: BTreeSet<String>,
}

/// The paths a manifest makes and removes.
fn owned_paths(manifest: &Manifest) -> Ours {
  let paths = |made: bool| -> BTreeSet<String> {
    manifest
      .entries
      .iter()
      .filter(|e| match e.action {
        Action::Create | Action::Mkdir | Action::Symlink { .. } | Action::Clear => made,
        Action::Delete | Action::Rmdir => !made,
        Action::Replace
        | Action::Rename { .. }
        | Action::Chmod { .. }
        | Action::ChmodDir { .. } => false,
      })
      .map(|e| e.path.to_string())
      .collect()
  };
  Ours {
    created: paths(true),
    removed: paths(false),
  }
}

/// The touched directories the sync phase syncs: every one but those a removal of this landing took away or found gone,
/// whose removal the sync of its parent (touched too) makes durable. A content manifest (A-424) deletes a directory's
/// files and then the directory, so the directory was touched by its files' entries and is gone by the sync: syncing it
/// could only fail, and would hold every entry in it as unsynced.
fn directories_to_sync(
  touched: &BTreeSet<Box<str>>,
  manifest: &Manifest,
  reports: &[EntryReport],
) -> Vec<Box<str>> {
  let gone: Vec<&str> = manifest
    .entries
    .iter()
    .zip(reports)
    .filter(|(entry, report)| {
      entry.action == Action::Rmdir
        && matches!(
          report.outcome,
          Some(Outcome::Written | Outcome::Skipped(SkipReason::AlreadyThere))
        )
    })
    .map(|(entry, _)| entry.path.as_ref())
    .collect();
  touched
    .iter()
    .filter(|dir| {
      !gone.iter().any(|removed| {
        dir.as_ref() == *removed
          || dir
            .strip_prefix(*removed)
            .is_some_and(|rest| rest.starts_with('/'))
      })
    })
    .cloned()
    .collect()
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

/// What a metadata entry changes (A-458, A-466, A-501): its fields as the base held them and as the volume holds them,
/// whether it is a directory's, and a file's modification time where its entry judges one.
#[derive(Clone, Copy, Debug)]
struct MetaChange {
  /// The fields as the base held them.
  from: FileMeta,
  /// The fields as the volume holds them.
  to: FileMeta,
  /// A base directory's entry (`ChmodDir`): opened as a directory and judged by a directory's fields.
  directory: bool,
  /// A file's modification time as its witness found it and as the volume holds it (an overlay's `Chmod`, A-501); none
  /// for an entry judged by content, whose capture read another folder, and for a directory, whose time moves with every
  /// entry made or removed in it.
  modified: Option<(i64, i64)>,
}

/// What a metadata entry sets (A-458, A-466, A-501): a `Chmod`'s base and target (and an overlay's file time), a
/// `ChmodDir`'s, or a captured clone's replacement whose bytes the disk already holds, its metadata's
/// ([`replaced_meta`]); anything else is no metadata entry, a malformed one.
fn metadata_of(entry: &LandingEntry) -> Result<MetaChange, WriteFailure> {
  match &entry.action {
    Action::Chmod { from, to } => Ok(MetaChange {
      from: *from,
      to: *to,
      directory: false,
      modified: match entry.judged {
        Judged::Witness => entry
          .witnessed
          .zip(entry.overlay)
          .map(|(witness, overlay)| (witness.fingerprint.mtime_ns, overlay.mtime_ns)),
        Judged::Content => None,
      },
    }),
    Action::ChmodDir { from, to } => Ok(MetaChange {
      from: *from,
      to: *to,
      directory: true,
      modified: None,
    }),
    Action::Replace => replaced_meta(entry).map(|(from, to)| MetaChange {
      from,
      to,
      directory: false,
      modified: None,
    }),
    _ => Err(WriteFailure::Volume(VfsError::Invalid)),
  }
}

/// A replacement's metadata as a metadata entry's (A-466): what the capture held (its content witness's) and what the
/// volume holds. A replacement with no witness or no overlay is no captured clone's, and refused as a malformed entry.
fn replaced_meta(entry: &LandingEntry) -> Result<(FileMeta, FileMeta), WriteFailure> {
  match (entry.witnessed.as_ref(), entry.overlay.as_ref()) {
    (Some(witness), Some(overlay)) => Ok((
      FileMeta::of_fingerprint(&witness.fingerprint),
      FileMeta::of_volume(overlay.mode, entry.windows),
    )),
    _ => Err(WriteFailure::Volume(VfsError::Invalid)),
  }
}

/// Whether an entry's write is a file staged for a batch's barrier (A-130): a create or a replacement it applies.
fn stages_a_file(entry: &LandingEntry, report: &EntryReport) -> bool {
  matches!(report.verdict, Some(Verdict::Apply))
    && matches!(entry.action, Action::Create | Action::Replace)
}

/// The pages a staged file of `size` bytes counts for in its batch (A-130): its size in the content store's pages, at
/// least one — the least a write-out hands the device.
fn file_pages(size: u64, store: &Store) -> u64 {
  let page = u64::try_from(store.content.page())
    .unwrap_or(u64::MAX)
    .max(1);
  size.div_ceil(page).max(1)
}

/// The pages one batch holds at most (A-130): the copy window's (`chunk_bytes`, the bound on every unit's work,
/// AUD-29-25), so at most that many files.
fn batch_pages(store: &Store) -> u64 {
  let pages = store
    .content
    .chunk_bytes()
    .checked_div(store.content.page().max(1))
    .unwrap_or(0);
  u64::try_from(pages).unwrap_or(u64::MAX).max(1)
}

/// The code of a refusal the host named a code for, of any kind (A-539): a name that exists, no room, a verb the
/// filesystem lacks, or anything else it could not serve. A metadata entry's open and a refused `rmdir` read these alike,
/// as they read every errno before the seam named the kinds; a path on this daemon's own mount (A-227), a fetch under
/// way (A-524) and the typed refusals are read on their own.
fn named_code(error: HostError) -> Option<OsCode> {
  match error {
    HostError::Exists(code)
    | HostError::NoSpace(code)
    | HostError::Unsupported(code)
    | HostError::Unavailable(code) => Some(code),
    HostError::NotFound
    | HostError::NotDirectory
    | HostError::NotFile
    | HostError::StaleHandle
    | HostError::OwnMount(_)
    | HostError::Pending => None,
  }
}

/// The staged temporary a file with `overlay`'s bytes may be cloned from (A-445): the first of the batch's staged files
/// with the same BLAKE3 and length, each complete and written out. The batch is the index: bounded by its pages
/// ([`batch_pages`]), and its temporaries are held open until it is placed, so the one found is still this landing's.
fn clone_source(staged: &VecDeque<Staged>, overlay: &OverlayIdentity) -> Option<HostFile> {
  staged
    .iter()
    .find(|staged| {
      staged.copy.overlay.hash == overlay.hash && staged.copy.overlay.size == overlay.size
    })
    .map(|staged| staged.copy.temp)
}

fn elapsed_ns(since: Instant) -> u64 {
  u64::try_from(since.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// The nanoseconds from `from` to `to` (0 if `to` is earlier).
fn nanos_between(from: Instant, to: Instant) -> u64 {
  u64::try_from(to.saturating_duration_since(from).as_nanos()).unwrap_or(u64::MAX)
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

/// Runs one landing: plan, present, grant, lease and validate, write by class, sync, advance,
/// report (§4.15). The caller opened `target` with containment and closes its handle after. The one-call
/// form of [`begin_landing`] and its [`LandingRun`], stepped through without a budget.
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
  let mut run = begin_landing(
    HostSlot::Lent(host),
    target,
    vol,
    store,
    grants,
    lease,
    audit,
    request,
  )
  .map_err(|(_, refusal)| refusal)?;
  let result = run.run_through(vol, store, audit, observer);
  settle_grant(grants, run.grant(), &result);
  result
}

/// Consumes a once-grant whose landing reached `Done` or `Partial` (§4.15; AUD-29-06): a landing that ended
/// otherwise keeps its grant for the resume.
pub fn settle_grant(
  grants: &mut Grants,
  grant: &GrantRecord,
  result: &Result<LandingReport, LandingRefusal>,
) {
  if let Ok(report) = result
    && matches!(report.state, LandingState::Done | LandingState::Partial)
    && grant.scope == GrantScope::Once
  {
    grants.consume(grant.id);
  }
}

/// A landing up to its grant and lease (§4.15 steps 1–4): plans the manifest, checks the landing's binding
/// against its grant before any write-capable step, and checks the lease; then the [`LandingRun`] that does
/// the rest, which the caller steps in slices and ends with [`settle_grant`]. Refused with the host given
/// back: `GrantRequired` (the presentation, with the preliminary verdicts), a grant or lease refusal, or the
/// volume's or the target's. [`Beginning`] run to its end in one call, then [`begin_planned`].
#[allow(clippy::too_many_arguments)]
pub fn begin_landing<'h, H: LandFs>(
  host: HostSlot<'h, H>,
  target: &LandingTarget,
  vol: &mut Volume,
  store: &mut Store,
  grants: &mut Grants,
  lease: Option<&LandingLease>,
  audit: &mut Audit,
  request: &LandingRequest,
) -> Result<LandingRun<'h, H>, (HostSlot<'h, H>, LandingRefusal)> {
  let beginning = match Beginning::new(vol, target, request) {
    Ok(beginning) => beginning,
    Err(refusal) => return Err((host, refusal)),
  };
  begin_landing_from(beginning, host, vol, store, grants, lease, audit)
}

/// [`begin_landing`] from a [`Beginning`] its caller made: a captured clone's ([`Beginning::against_capture`]).
pub fn begin_landing_from<'h, H: LandFs>(
  mut beginning: Beginning,
  mut host: HostSlot<'h, H>,
  vol: &mut Volume,
  store: &mut Store,
  grants: &mut Grants,
  lease: Option<&LandingLease>,
  audit: &mut Audit,
) -> Result<LandingRun<'h, H>, (HostSlot<'h, H>, LandingRefusal)> {
  let planned = loop {
    let stepped = match host.get() {
      Some(lent) => beginning.unit(lent, vol, store, grants, audit),
      None => Err(LandingRefusal::HostAway),
    };
    match stepped {
      Ok(Some(planned)) => break planned,
      Ok(None) => {}
      Err(refusal) => return Err((host, refusal)),
    }
  };
  match planned {
    Planned::Presented(presented) => Err((host, LandingRefusal::GrantRequired(presented))),
    Planned::Granted(granted) => begin_planned(host, lease, *granted),
  }
}

/// A landing being begun a unit at a time (§4.15 steps 1–3; §4.3 "Bounded work everywhere"): its plan
/// ([`crate::manifest::PlanWalk`]), then its grant checked against the binding, then, for a landing with no grant yet,
/// its preliminary verdicts one entry a unit. A presentation of 1,000 files of 16 KiB had held its shard for one poll
/// of 6.77 s, 6.20 s of it the plan (2026-11-08). It holds no host between units: its caller lends the host to each
/// [`Beginning::unit`], so an overlay's base host can serve its volume between them. What it plans must not move
/// under it, so a sliced beginning plans a snapshot.
pub struct Beginning {
  target: LandingTarget,
  request: LandingRequest,
  began: Instant,
  stage: BeginStage,
}

enum BeginStage {
  /// The manifest being planned.
  Plan(crate::manifest::PlanWalk),
  /// A landing with no grant: its entries checked against the target one a unit. Boxed: the landing's saved state is
  /// far past the plan's.
  Verdicts(Box<Verdicting>),
  /// Ended: what it planned was given out.
  Ended,
}

/// A landing with no grant between the units of its preliminary verdicts.
struct Verdicting {
  manifest: Manifest,
  binding: GrantBinding,
  saved: Saved,
  ours: Ours,
  reports: Vec<EntryReport>,
}

/// What a [`Beginning`] ends with.
#[derive(Debug)]
pub enum Planned {
  /// No grant yet: the presentation, its manifest and preliminary verdicts, for a human to grant.
  Presented(Box<Presented>),
  /// Granted: what [`begin_planned`] begins the run with.
  Granted(Box<GrantedPlan>),
}

/// A granted landing's plan, ready for its lease and its run ([`begin_planned`]).
#[derive(Debug)]
pub struct GrantedPlan {
  target: LandingTarget,
  request: LandingRequest,
  manifest: Manifest,
  binding: GrantBinding,
  grant: GrantRecord,
  began: Instant,
}

impl Beginning {
  /// A landing of `request` into `target` from its start. The instant `request.now_ns` stands for is taken here: its
  /// caller reads the clock as it calls this, and every fence counts the landing's time from it, the plan included.
  pub fn new(
    vol: &Volume,
    target: &LandingTarget,
    request: &LandingRequest,
  ) -> Result<Beginning, LandingRefusal> {
    let began = Instant::now();
    let walk =
      crate::manifest::PlanWalk::new(vol, request.source).map_err(LandingRefusal::Volume)?;
    Ok(Beginning {
      target: target.clone(),
      request: request.clone(),
      began,
      stage: BeginStage::Plan(walk),
    })
  }

  /// [`Beginning::new`] for a clone of a captured base (§4.15 `capture_base`, A-291's landing): its manifest is the
  /// change of `request.source` since the capture `base` (a snapshot's tree in the same store, pinned while the clone
  /// lives; or, for a clone a move brought here, what the move carried of the capture and the clone's mark, A-503),
  /// every entry judged by content ([`crate::manifest::PlanWalk::against_capture`]), and its finish leaves the volume
  /// as it was.
  pub fn against_capture(
    vol: &Volume,
    target: &LandingTarget,
    request: &LandingRequest,
    base: impl Into<slates_vfs::captured::CaptureBase>,
  ) -> Result<Beginning, LandingRefusal> {
    let began = Instant::now();
    let walk = crate::manifest::PlanWalk::against_capture(vol, request.source, base)
      .map_err(LandingRefusal::Volume)?;
    Ok(Beginning {
      target: target.clone(),
      request: request.clone(),
      began,
      stage: BeginStage::Plan(walk),
    })
  }

  /// One unit with `host` lent for it: a unit of the plan, the grant's check, or one entry's preliminary verdict; what
  /// was planned once the last is done. A refusal ends the beginning (the volume's, the target's, the grant's).
  pub fn unit<H: LandFs>(
    &mut self,
    host: &mut H,
    vol: &mut Volume,
    store: &mut Store,
    grants: &mut Grants,
    audit: &mut Audit,
  ) -> Result<Option<Planned>, LandingRefusal> {
    match std::mem::replace(&mut self.stage, BeginStage::Ended) {
      BeginStage::Plan(mut walk) => {
        let Some(manifest) = walk
          .unit(vol, store, host, &self.request.filter)
          .map_err(LandingRefusal::Volume)?
        else {
          self.stage = BeginStage::Plan(walk);
          return Ok(None);
        };
        self.planned(host, manifest, grants, audit)
      }
      BeginStage::Verdicts(verdicting) => {
        let Verdicting {
          manifest,
          binding,
          saved,
          ours,
          mut reports,
        } = *verdicting;
        let mut landing = Landing::resume(host, &self.request, saved);
        let Some(entry) = manifest.entries.get(reports.len()) else {
          landing.close_dirs();
          return Ok(Some(Planned::Presented(Box::new(Presented {
            manifest,
            preliminary: reports,
            binding,
          }))));
        };
        match landing.validate_entry(entry, &ours) {
          Ok(report) => reports.push(report),
          Err(error) => {
            landing.close_dirs();
            return Err(LandingRefusal::Target(error));
          }
        }
        let saved = landing.suspend();
        self.stage = BeginStage::Verdicts(Box::new(Verdicting {
          manifest,
          binding,
          saved,
          ours,
          reports,
        }));
        Ok(None)
      }
      BeginStage::Ended => Err(LandingRefusal::HostAway),
    }
  }

  /// The plan is whole: recorded, and its binding checked against the grant (§4.13 "Grants") before any write-capable
  /// step. A granted landing is ready for its lease; one with no grant goes on to its preliminary verdicts.
  fn planned<H: LandFs>(
    &mut self,
    host: &mut H,
    manifest: Manifest,
    grants: &mut Grants,
    audit: &mut Audit,
  ) -> Result<Option<Planned>, LandingRefusal> {
    let request = &self.request;
    audit.push(AuditRecord {
      seq: 0,
      at_ns: request.now_ns,
      kind: AuditKind::LandingPlanned,
      grant: request.grant,
      landing: request.landing_id,
      manifest: manifest.hash,
      outcome: None,
    });
    // The landing's own binding — the consumer, the volume and snapshot, and the target as opened now — must be the
    // one the grant was approved for.
    let binding = binding_of(host, &self.target, request)?;
    match grants.check(request.grant, &binding, manifest.hash, request.now_ns) {
      Ok(grant) => Ok(Some(Planned::Granted(Box::new(GrantedPlan {
        target: self.target.clone(),
        request: self.request.clone(),
        manifest,
        binding,
        grant,
        began: self.began,
      })))),
      Err(GrantRefusal::GrantRequired) => {
        // The preliminary verdict pass reads only, with no capability a write would use.
        let read_only = LandCapabilities {
          exchange: false,
          reflink: false,
          unnamed_temporaries: false,
        };
        self.stage = BeginStage::Verdicts(Box::new(Verdicting {
          ours: owned_paths(&manifest),
          reports: Vec::with_capacity(manifest.entries.len()),
          saved: Saved::start(&self.target, read_only),
          manifest,
          binding,
        }));
        Ok(None)
      }
      Err(e) => Err(LandingRefusal::Grant(e)),
    }
  }
}

/// Begins a granted landing's run from its plan: its lease checked (AUD-29-03: the caller took the landing lease on
/// the target's canonical identity from the target's one host-local owner; it must name this target and still be
/// live, and every entry is fenced by its term — the engine neither takes nor releases it), then the
/// [`LandingRun`]. Refused `LeaseRequired` with the host given back.
pub fn begin_planned<'h, H: LandFs>(
  host: HostSlot<'h, H>,
  lease: Option<&LandingLease>,
  plan: GrantedPlan,
) -> Result<LandingRun<'h, H>, (HostSlot<'h, H>, LandingRefusal)> {
  let GrantedPlan {
    target,
    request,
    manifest,
    binding,
    grant,
    began,
  } = plan;
  let lease = match lease {
    Some(held)
      if held.target.as_ref() == lease_key(&binding.target) && held.expires_ns > request.now_ns =>
    {
      held.clone()
    }
    _ => return Err((host, LandingRefusal::LeaseRequired)),
  };
  LandingRun::begin(host, target, request, manifest, grant, lease, began)
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

/// A landing's state between slices: everything a [`Landing`] holds but its host and request, which the run
/// lends it again for each slice.
struct Saved {
  root: HostDir,
  dirs: BTreeMap<Box<str>, HostDir>,
  touched: BTreeSet<Box<str>>,
  exchange: bool,
  unnamed_temporaries: bool,
  reflink: bool,
  cloned: usize,
  hidden_counter: u64,
  bytes_written: u64,
  widest_window_ns: u64,
  degraded: Vec<Degradation>,
  costs: CostSamples,
  barriers: usize,
  crashed: Option<OsCode>,
  unsynced: BTreeSet<Box<str>>,
  media_failed: bool,
  placed: BTreeMap<Box<str>, Fingerprint>,
  restored: BTreeMap<Box<str>, Fingerprint>,
  observed: BTreeMap<Box<str>, Fingerprint>,
  replaced: BTreeMap<Box<str>, ReplacedMeta>,
}

impl Saved {
  /// A fresh landing's state over `target`, with what its filesystem supports.
  fn start(target: &LandingTarget, caps: LandCapabilities) -> Self {
    Self {
      root: target.dir,
      dirs: BTreeMap::new(),
      touched: BTreeSet::new(),
      exchange: caps.exchange,
      unnamed_temporaries: caps.unnamed_temporaries,
      reflink: caps.reflink,
      cloned: 0,
      hidden_counter: 0,
      bytes_written: 0,
      widest_window_ns: 0,
      degraded: Vec::new(),
      costs: CostSamples::default(),
      barriers: 0,
      crashed: None,
      unsynced: BTreeSet::new(),
      media_failed: false,
      placed: BTreeMap::new(),
      restored: BTreeMap::new(),
      observed: BTreeMap::new(),
      replaced: BTreeMap::new(),
    }
  }
}

impl<'a, H: LandFs> Landing<'a, H> {
  /// The landing over its saved state, with its host and request lent for one slice.
  fn resume(host: &'a mut H, request: &'a LandingRequest, saved: Saved) -> Self {
    Self {
      host,
      request,
      root: saved.root,
      dirs: saved.dirs,
      touched: saved.touched,
      exchange: saved.exchange,
      unnamed_temporaries: saved.unnamed_temporaries,
      reflink: saved.reflink,
      cloned: saved.cloned,
      hidden_counter: saved.hidden_counter,
      bytes_written: saved.bytes_written,
      widest_window_ns: saved.widest_window_ns,
      degraded: saved.degraded,
      costs: saved.costs,
      barriers: saved.barriers,
      crashed: saved.crashed,
      unsynced: saved.unsynced,
      media_failed: saved.media_failed,
      placed: saved.placed,
      restored: saved.restored,
      observed: saved.observed,
      replaced: saved.replaced,
    }
  }

  /// The landing's state at the end of a slice; its host and request go back to the run.
  fn suspend(self) -> Saved {
    Saved {
      root: self.root,
      dirs: self.dirs,
      touched: self.touched,
      exchange: self.exchange,
      unnamed_temporaries: self.unnamed_temporaries,
      reflink: self.reflink,
      cloned: self.cloned,
      hidden_counter: self.hidden_counter,
      bytes_written: self.bytes_written,
      widest_window_ns: self.widest_window_ns,
      degraded: self.degraded,
      costs: self.costs,
      barriers: self.barriers,
      crashed: self.crashed,
      unsynced: self.unsynced,
      media_failed: self.media_failed,
      placed: self.placed,
      restored: self.restored,
      observed: self.observed,
      replaced: self.replaced,
    }
  }
}

/// Where a run's host lives: owned by the run (a daemon's writer, given back by [`LandingRun::into_host`]
/// when the run ends), lent by a caller that runs the landing through in one call, or away between slices
/// ([`LandingRun::take_host`]) while its owner serves with it.
pub enum HostSlot<'h, H> {
  /// The run owns the host.
  Owned(H),
  /// The run borrows the host.
  Lent(&'h mut H),
  /// The host is away until [`LandingRun::put_host`] gives it back.
  Away,
}

impl<H> HostSlot<'_, H> {
  fn get(&mut self) -> Option<&mut H> {
    match self {
      HostSlot::Owned(host) => Some(host),
      HostSlot::Lent(host) => Some(host),
      HostSlot::Away => None,
    }
  }
}

/// Where a run is: one phase of §4.15's sequence after the lease, with its cursor.
enum Phase {
  /// Settling a crashed earlier attempt's hidden siblings, one directory per unit.
  Sweep { parents: Vec<Box<str>>, next: usize },
  /// Taking each entry's verdict against the disk, one entry per unit.
  Validate { ours: Ours, next: usize },
  /// Writing: per unit, one entry, one window of a large file, one batch's barrier or one staged file placed (A-130);
  /// its progress boxed to keep the enum small, as `Advance`'s is.
  Write {
    next: usize,
    progress: Box<WriteProgress>,
  },
  /// Syncing the touched directories, one per unit.
  Sync {
    touched: Vec<Box<str>>,
    next: usize,
    progress: SyncProgress,
  },
  /// Advancing the overlay and reporting.
  Finish { durability: Durability },
  /// Advancing the overlay past what reached the disk durably, one entry a unit (A-133): the finish sliced, so a
  /// landing of many entries does not hold the shard for its whole O(entries) advance (boxed to keep the enum small).
  Advance(Box<AdvanceState>),
  /// The run has returned its outcome.
  Ended,
}

/// The finish's sliced advance (A-133): the report's counts, taken once in a single pass at the start, and the cursor.
/// `begun` gates the one-time base-plane setup; `next` is the cursor over the reports, each advanced where it landed
/// (the per-entry work that was `advanced` + `land_advance`, now folded into the slice so nothing O(entries) runs in one
/// unit). `state`, `held`, the four counts and `durability` are carried to the report.
struct AdvanceState {
  durability: Durability,
  /// The landing crashed mid-write; its state is `Aborted` and nothing advances.
  crashed: bool,
  base: Option<BaseConfig>,
  next: usize,
  begun: bool,
  // The report's figures, accumulated over the advance loop so the finish does no O(entries) work in one unit:
  written: usize,
  skipped: usize,
  conflicts: usize,
  failed: usize,
  held: usize,
  /// Whether every report so far took its entry out of the overlay (the landing is `Done` only if all did).
  all_advances: bool,
}

/// A granted landing as an owned, resumable operation (AUD-29-25; §4.15, §4.3): everything after its grant
/// and lease were checked — sweep, validate, write, sync, advance — taken one unit at a time (a directory
/// swept, an entry validated or written, a directory synced) in slices the caller bounds, so the shard that
/// owns the volume runs other work between them. Nothing about the landing depends on where its slices
/// end: each unit is the same step the one-call [`land`] takes, in the same order, under the same grant and
/// lease fences (checked per entry against the landing's own elapsed clock). Dropped before it ends, it has
/// closed nothing: the caller ends it with [`LandingRun::abandon`], which closes the directories it opened.
pub struct LandingRun<'h, H: LandFs> {
  host: HostSlot<'h, H>,
  request: LandingRequest,
  target: LandingTarget,
  manifest: Manifest,
  grant: GrantRecord,
  lease: LandingLease,
  saved: Option<Saved>,
  reports: Vec<EntryReport>,
  phase: Phase,
  swept: usize,
  /// How long the last unit stepped took, in nanoseconds, measured from the previous unit's end (so a step's
  /// time is exactly the sum of its units'): a step overruns its budget by at most this (AUD-29-25).
  last_unit_ns: u64,
  /// How long the last [`LandingRun::step`] ran, in nanoseconds, to the end of its last unit.
  last_step_ns: u64,
  /// The instant the run's request clock (`request.now_ns`) stands for: taken as [`begin_landing`] starts, which its
  /// caller calls as it reads the clock. The fences count the run's time from here.
  began: Instant,
}

impl<'h, H: LandFs> LandingRun<'h, H> {
  /// A run of the granted landing of `manifest` into `target`, its grant and lease already checked.
  pub fn begin(
    mut host: HostSlot<'h, H>,
    target: LandingTarget,
    request: LandingRequest,
    manifest: Manifest,
    grant: GrantRecord,
    lease: LandingLease,
    began: Instant,
  ) -> Result<Self, (HostSlot<'h, H>, LandingRefusal)> {
    let caps = match host.get().map(|host| host.capabilities(target.dir)) {
      Some(Ok(caps)) => caps,
      Some(Err(error)) => return Err((host, LandingRefusal::Target(error))),
      None => return Err((host, LandingRefusal::HostAway)),
    };
    let saved = Saved::start(&target, caps);
    let parents = sweep_parents(&manifest);
    Ok(Self {
      host,
      request,
      target,
      manifest,
      grant,
      lease,
      saved: Some(saved),
      reports: Vec::new(),
      phase: Phase::Sweep { parents, next: 0 },
      swept: 0,
      last_unit_ns: 0,
      last_step_ns: 0,
      began,
    })
  }

  /// Runs units until the landing ends early, reaches its finish, waits on a barrier (A-132), or `budget_ns` of this
  /// slice has passed (checked between units, so a slice ends within one unit of its budget): `Some` with the outcome
  /// when it ended early (a refusal: a target that cannot be read, a conflict), `None` otherwise. A run that waits on
  /// a barrier gives it to its caller ([`LandingRun::take_barrier`]) and steps nothing until the outcome is back. It
  /// never runs the finish, which advances the volume: once [`LandingRun::ready_to_finish`], the caller runs
  /// [`LandingRun::finish`] where its records commit as one atom with the landing's completion.
  pub fn step(
    &mut self,
    vol: &mut Volume,
    store: &mut Store,
    audit: &mut Audit,
    observer: &mut dyn Observer<H>,
    budget_ns: u64,
  ) -> Option<Result<LandingReport, LandingRefusal>> {
    let began = Instant::now();
    let mut mark = began;
    self.last_step_ns = 0;
    loop {
      if self.ready_to_finish() || self.advancing() || self.awaiting_barrier() {
        return None;
      }
      let ended = self.unit(vol, store, audit, observer);
      let now = Instant::now();
      self.last_unit_ns = nanos_between(mark, now);
      self.last_step_ns = nanos_between(began, now);
      mark = now;
      if let Some(ended) = ended {
        self.phase = Phase::Ended;
        return Some(ended);
      }
      if self.last_step_ns >= budget_ns {
        return None;
      }
    }
  }

  /// How long the last unit [`LandingRun::step`] ran took, in nanoseconds: a step returns once its budget
  /// has passed, so it overruns the budget by at most this.
  pub fn last_unit_ns(&self) -> u64 {
    self.last_unit_ns
  }

  /// How long the last [`LandingRun::step`] ran, to the end of its last unit, in nanoseconds. A step that
  /// returned for its budget ran less than the budget before its last unit, so this is at most the budget
  /// plus [`LandingRun::last_unit_ns`].
  pub fn last_step_ns(&self) -> u64 {
    self.last_step_ns
  }

  /// Whether every host step is done and only the finish (the volume's advance and the report) remains.
  pub fn ready_to_finish(&self) -> bool {
    matches!(self.phase, Phase::Finish { .. })
  }

  /// Whether the run is in its sliced advance (A-133): the finish has begun and more entries remain. `finish_step`
  /// drives it; `step` never does.
  pub fn advancing(&self) -> bool {
    matches!(self.phase, Phase::Advance(_))
  }

  /// Whether the run waits on a batch's barrier away from it (A-132): taken and not yet given out, or given out and its
  /// outcome not yet back. It steps nothing meanwhile.
  pub fn awaiting_barrier(&self) -> bool {
    matches!(&self.phase, Phase::Write { progress, .. } if progress.barrier.awaited())
  }

  /// The batch barrier the run has taken (A-132), for its driver to run where waiting costs nothing — a thread of its
  /// own, never the shard that owns the landing — and to hand the outcome back with [`LandingRun::barrier_ran`].
  /// `None` when the run has no barrier to give (none taken, or this one already given out).
  pub fn take_barrier(&mut self) -> Option<Box<dyn Barrier>> {
    let Phase::Write { progress, .. } = &mut self.phase else {
      return None;
    };
    let BarrierState::Taken { began, .. } = progress.barrier else {
      return None;
    };
    match std::mem::replace(&mut progress.barrier, BarrierState::Running { began }) {
      BarrierState::Taken { barrier, .. } => Some(barrier),
      _ => None,
    }
  }

  /// The outcome of the barrier [`LandingRun::take_barrier`] gave out: the run's next step ends it. Ignored when no
  /// barrier is out.
  pub fn barrier_ran(&mut self, ran: Result<(), HostError>) {
    if let Phase::Write { progress, .. } = &mut self.phase
      && let BarrierState::Running { began } = progress.barrier
    {
      progress.barrier = BarrierState::Ran { ran, began };
    }
  }

  /// The finish: the overlay advanced past what reached the disk durably, the audit record, the report.
  /// `LandingRefusal::Ended` before [`LandingRun::ready_to_finish`] or after the run ended.
  pub fn finish(
    &mut self,
    vol: &mut Volume,
    store: &mut Store,
    audit: &mut Audit,
    observer: &mut dyn Observer<H>,
  ) -> Result<LandingReport, LandingRefusal> {
    // The one-call finish: step the sliced advance with no budget, so it runs to the report in one call.
    self
      .finish_step(vol, store, audit, observer, u64::MAX)
      .unwrap_or(Err(LandingRefusal::Ended))
  }

  /// One budgeted step of the sliced finish (A-133): the overlay advanced past what reached the disk, one entry a unit,
  /// for `budget_ns` (checked between units, so a step overruns by at most one entry). `Some` with the outcome once the
  /// finish is done or has failed; `None` when the advance has more to do and the caller should yield and step again.
  /// The caller runs it only once [`LandingRun::ready_to_finish`], after any delegation recall, so it never races the
  /// delegations a finished landing changes; its completion record commits as one atom when it returns (A-127).
  pub fn finish_step(
    &mut self,
    vol: &mut Volume,
    store: &mut Store,
    audit: &mut Audit,
    observer: &mut dyn Observer<H>,
    budget_ns: u64,
  ) -> Option<Result<LandingReport, LandingRefusal>> {
    if !self.ready_to_finish() && !self.advancing() {
      return Some(Err(LandingRefusal::Ended));
    }
    let began = Instant::now();
    let mut mark = began;
    self.last_step_ns = 0;
    loop {
      let ended = self.unit(vol, store, audit, observer);
      let now = Instant::now();
      self.last_unit_ns = nanos_between(mark, now);
      self.last_step_ns = nanos_between(began, now);
      mark = now;
      if let Some(ended) = ended {
        self.phase = Phase::Ended;
        return Some(ended);
      }
      if self.last_step_ns >= budget_ns {
        return None;
      }
    }
  }

  /// Steps the run through to its outcome, finish included, without a budget: the one-call landing.
  pub fn run_through(
    &mut self,
    vol: &mut Volume,
    store: &mut Store,
    audit: &mut Audit,
    observer: &mut dyn Observer<H>,
  ) -> Result<LandingReport, LandingRefusal> {
    loop {
      if let Some(ended) = self.step(vol, store, audit, observer, u64::MAX) {
        return ended;
      }
      // The one-call driver has nothing else to do while a barrier waits: it runs it in place.
      match self.take_barrier() {
        Some(barrier) => self.barrier_ran(barrier.run()),
        None => return self.finish(vol, store, audit, observer),
      }
    }
  }

  /// The grant the landing runs under.
  pub fn grant(&self) -> &GrantRecord {
    &self.grant
  }

  /// The lease the landing runs under.
  pub fn lease(&self) -> &LandingLease {
    &self.lease
  }

  /// Renews the landing's lease with `renewed`, its holder's own re-take of the same target (the keepalive
  /// between slices, AUD-29-25): its entries are fenced by the renewed term from here on. Refused — the lease
  /// kept as it was — for a lease on another target or of another holder; whether it was taken.
  pub fn renew_lease(&mut self, renewed: LandingLease) -> bool {
    if renewed.target != self.lease.target || renewed.holder != self.lease.holder {
      return false;
    }
    self.lease = renewed;
    true
  }

  /// Whether the run has returned its outcome.
  pub fn ended(&self) -> bool {
    matches!(self.phase, Phase::Ended)
  }

  /// The host, once the run has ended or been abandoned: the daemon's writer, given back.
  pub fn into_host(self) -> Option<H> {
    match self.host {
      HostSlot::Owned(host) => Some(host),
      HostSlot::Lent(_) | HostSlot::Away => None,
    }
  }

  /// Takes the run's own host away between slices, for its owner to serve with; `None` when the run does not
  /// own it. The run steps nothing until [`LandingRun::put_host`] gives it back.
  pub fn take_host(&mut self) -> Option<H> {
    match std::mem::replace(&mut self.host, HostSlot::Away) {
      HostSlot::Owned(host) => Some(host),
      other => {
        self.host = other;
        None
      }
    }
  }

  /// Gives the run its host back for its next slice.
  pub fn put_host(&mut self, host: H) {
    self.host = HostSlot::Owned(host);
  }

  /// Ends a run that will not be stepped again (its client went, the shard is stopping): the directories it
  /// opened are closed and nothing is advanced, so its entries stay in the overlay and a later landing
  /// resumes them, sweeping whatever this attempt left — the same state a crash leaves.
  pub fn abandon(&mut self) {
    if let (Some(host), Some(saved)) = (self.host.get(), self.saved.take()) {
      let mut landing = Landing::resume(host, &self.request, saved);
      if let Phase::Write { progress, .. } = &mut self.phase {
        landing.release_write(progress);
      }
      landing.close_dirs();
      self.saved = Some(landing.suspend());
    }
    self.phase = Phase::Ended;
  }

  /// One unit of the current phase; `Some` when the landing has ended with that outcome.
  fn unit(
    &mut self,
    vol: &mut Volume,
    store: &mut Store,
    audit: &mut Audit,
    observer: &mut dyn Observer<H>,
  ) -> Option<Result<LandingReport, LandingRefusal>> {
    let LandingRun {
      host,
      request,
      target,
      manifest,
      grant,
      lease,
      saved,
      reports,
      phase,
      swept,
      last_unit_ns: _,
      last_step_ns: _,
      began,
    } = self;
    let Some(host) = host.get() else {
      return Some(Err(LandingRefusal::HostAway));
    };
    let state = saved.take()?;
    let mut landing = Landing::resume(host, request, state);
    let mut ended = None;
    let mut next_phase = None;
    match phase {
      Phase::Sweep { parents, next } => match parents.get(*next) {
        Some(parent) => {
          *swept = swept.saturating_add(landing.sweep_parent(parent.clone(), manifest));
          *next = next.saturating_add(1);
        }
        None => {
          reports.reserve(manifest.entries.len());
          next_phase = Some(Phase::Validate {
            ours: owned_paths(manifest),
            next: 0,
          });
        }
      },
      Phase::Validate { ours, next } => match manifest.entries.get(*next) {
        Some(entry) => match landing.validate_entry(entry, ours) {
          Ok(report) => {
            reports.push(report);
            *next = next.saturating_add(1);
          }
          Err(error) => {
            landing.close_dirs();
            ended = Some(Err(LandingRefusal::Target(error)));
          }
        },
        None => {
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
            ended = Some(Err(LandingRefusal::Conflict(std::mem::take(reports))));
          } else {
            next_phase = Some(Phase::Write {
              next: 0,
              progress: Box::new(WriteProgress::start(*began)),
            });
          }
        }
      },
      Phase::Write { next, progress } => {
        let done = landing.write_unit(
          manifest,
          reports,
          next,
          &mut WriteContext {
            vol,
            store,
            grant: Some(grant),
            lease,
            observer,
            audit,
          },
          progress,
        );
        if done {
          next_phase = Some(Phase::Sync {
            touched: directories_to_sync(&landing.touched, manifest, reports),
            next: 0,
            progress: SyncProgress::default(),
          });
        }
      }
      Phase::Sync {
        touched,
        next,
        progress,
      } => match touched.get(*next) {
        Some(path) => {
          landing.sync_one(path.clone(), progress);
          *next = next.saturating_add(1);
        }
        None => {
          next_phase = Some(Phase::Finish {
            durability: landing.finish_sync(progress),
          });
        }
      },
      Phase::Finish { durability } => {
        match begin_advance(&mut landing, vol, target, request, *durability) {
          Ok(advance) => next_phase = Some(Phase::Advance(Box::new(advance))),
          Err(error) => ended = Some(Err(error)),
        }
      }
      Phase::Advance(advance) => {
        match advance_step(
          &mut landing,
          advance,
          reports.as_slice(),
          manifest,
          vol,
          store,
          request,
        ) {
          None => {}
          Some(Err(error)) => ended = Some(Err(error)),
          Some(Ok(())) => {
            let durability = advance.durability;
            return Some(Ok(finish_report(
              landing,
              Finishing {
                target,
                audit,
                request,
                manifest,
                grant,
                reports: std::mem::take(reports),
                durability,
                swept: *swept,
              },
              advance,
            )));
          }
        }
      }
      Phase::Ended => {
        *saved = Some(landing.suspend());
        return Some(Err(LandingRefusal::Ended));
      }
    }
    *saved = Some(landing.suspend());
    if let Some(next) = next_phase {
      *phase = next;
    }
    ended
  }
}

/// What finishing a landing needs besides the landing itself.
struct Finishing<'x> {
  target: &'x LandingTarget,
  audit: &'x mut Audit,
  request: &'x LandingRequest,
  manifest: &'x Manifest,
  grant: &'x GrantRecord,
  reports: Vec<EntryReport>,
  durability: Durability,
  swept: usize,
}

/// The start of a landing's sliced finish (A-133): its degradations recorded, its directories closed, and a fresh
/// (non-overlay) volume's base config read — all O(1). The report's figures are accumulated over the advance loop
/// ([`advance_step`]), not swept here, so a large landing's finish does no O(entries) work in any one unit; the advance
/// and the report ([`finish_report`]) follow.
fn begin_advance<H: LandFs>(
  landing: &mut Landing<'_, H>,
  vol: &Volume,
  target: &LandingTarget,
  request: &LandingRequest,
  durability: Durability,
) -> Result<AdvanceState, LandingRefusal> {
  if let Some(code) = landing.crashed {
    landing.degraded.push(Degradation::Crashed { code });
  }
  if !landing.exchange {
    landing.degraded.push(Degradation::NoExchange {
      widest_window_ns: landing.widest_window_ns,
    });
  }
  landing.close_dirs();
  // A fresh volume gains a base; an overlay already has one.
  let base = if vol.is_overlay() {
    None
  } else {
    let facts = landing
      .host
      .facts(target.dir)
      .map_err(LandingRefusal::Target)?;
    Some(BaseConfig {
      root: target.dir,
      facts,
      large_class_bytes: request.large_class_bytes,
    })
  };
  Ok(AdvanceState {
    durability,
    crashed: landing.crashed.is_some(),
    base,
    next: 0,
    begun: false,
    written: 0,
    skipped: 0,
    conflicts: 0,
    failed: 0,
    held: 0,
    all_advances: true,
  })
}

/// One unit of the sliced finish's advance (A-133): the one-time base setup, then one report — advanced where it landed
/// (the same rule as the former `advanced` + `land_advance`, folded per entry so no unit is O(entries)) — then the end
/// (every base listing marked stale). `None` while more remains, `Some(Ok(()))` once the advance is complete (the report
/// follows), `Some(Err)` on a host refusal that fails the finish. A report is advanced only on a completed landing where
/// its outcome takes it out of the overlay and its write reached the durability boundary; a rename also advances the
/// whiteout at its origin.
fn advance_step<H: LandFs>(
  landing: &mut Landing<'_, H>,
  advance: &mut AdvanceState,
  reports: &[EntryReport],
  manifest: &Manifest,
  vol: &mut Volume,
  store: &mut Store,
  request: &LandingRequest,
) -> Option<Result<(), LandingRefusal>> {
  // A captured clone's landing (judged by content, A-291) takes no base from the target and forgets nothing: the clone
  // stays the movable scratch volume it was, and its next landing is judged against its capture again.
  let keeps_volume = manifest.basis == Judged::Content;
  if !advance.begun && keeps_volume {
    advance.begun = true;
    return None;
  }
  if !advance.begun {
    return match vol
      .with_host(landing.host)
      .land_advance_begin(store, advance.base.take())
    {
      Ok(()) => {
        advance.begun = true;
        None
      }
      Err(error) => Some(Err(LandingRefusal::Volume(error))),
    };
  }
  let Some(report) = reports.get(advance.next) else {
    if !keeps_volume {
      vol.with_host(landing.host).land_advance_end();
    }
    return Some(Ok(()));
  };
  advance.next = advance.next.saturating_add(1);
  // Accumulate the report's figures as the loop passes each entry, so the finish never sweeps them in one O(entries)
  // unit (the counts and the held count were a pass each at the start/end before).
  match report.outcome {
    Some(Outcome::Written) => advance.written = advance.written.saturating_add(1),
    Some(Outcome::Skipped(_) | Outcome::AcceptedIdentical) => {
      advance.skipped = advance.skipped.saturating_add(1);
    }
    Some(Outcome::Conflict(_) | Outcome::Undone(_)) => {
      advance.conflicts = advance.conflicts.saturating_add(1);
    }
    Some(Outcome::Failed { .. }) => advance.failed = advance.failed.saturating_add(1),
    _ => {}
  }
  let takes_out = advances(report.outcome.as_ref());
  advance.all_advances = advance.all_advances && takes_out;
  let durable = landing.durable(report);
  // An entry that landed but did not reach the durability boundary (its directory, or the media barrier the grant
  // asked for, did not sync) stays in the overlay: the resume syncs and advances it then (AUD-29-05).
  if takes_out && !durable {
    advance.held = advance.held.saturating_add(1);
  }
  // Only a completed landing advances (`Aborted` iff it crashed, §4.15), and only an entry that left the overlay and
  // reached the durability boundary; a captured clone's entries leave nothing (above).
  if advance.crashed || !takes_out || !durable || keeps_volume {
    return None;
  }
  // The landing's own write left a fresh ctime, so its witness is racy by the design's rule (A-49): a later verdict
  // re-hashes the file rather than trust its fingerprint. The identity is the manifest entry's content hash (the
  // reports and the manifest entries are built in lock-step, so they share an index).
  let written = landing
    .placed
    .get(report.path.as_ref())
    .copied()
    .zip(
      manifest
        .entries
        .get(advance.next.saturating_sub(1))
        .and_then(|entry| entry.overlay.as_ref())
        .map(|overlay| overlay.hash),
    )
    .map(|(fingerprint, identity)| Witness {
      fingerprint,
      identity,
      witnessed_at: request.now_ns,
      racy: true,
    });
  let source = request.source.snapshot();
  let dest = Landed {
    path: report.path.to_string(),
    written,
  };
  let rename_from = match &report.action {
    Action::Rename { from } => Some(from.to_string()),
    _ => None,
  };
  if let Err(error) = vol
    .with_host(landing.host)
    .land_advance_one(store, &dest, source)
  {
    return Some(Err(LandingRefusal::Volume(error)));
  }
  if let Some(from) = rename_from {
    let origin = Landed {
      path: from,
      written: None,
    };
    if let Err(error) = vol
      .with_host(landing.host)
      .land_advance_one(store, &origin, source)
    {
      return Some(Err(LandingRefusal::Volume(error)));
    }
  }
  None
}

/// The landing's report, once its advance is done (A-133): the audit record, the counts and the overlay's state.
/// The advance already ran ([`advance_step`]); this touches no disk.
fn finish_report<H: LandFs>(
  mut landing: Landing<'_, H>,
  finishing: Finishing<'_>,
  advance: &AdvanceState,
) -> LandingReport {
  let Finishing {
    target,
    audit,
    request,
    manifest,
    grant,
    reports,
    durability,
    swept,
  } = finishing;
  // The terminal state from the accumulated figures: `Aborted` if the landing crashed, `Done` only if every entry
  // advanced and none was held, else `Partial` (AUD-29-05).
  let state = if advance.crashed {
    LandingState::Aborted
  } else if advance.held == 0 && advance.all_advances {
    LandingState::Done
  } else {
    LandingState::Partial
  };
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
  LandingReport {
    state,
    manifest_hash: manifest.hash,
    summary: manifest.summary.clone(),
    // The counts were taken in one pass at the advance's start ([`begin_advance`]); the reports do not change during it.
    written: advance.written,
    skipped: advance.skipped,
    conflicts: advance.conflicts,
    failed: advance.failed,
    held: advance.held,
    entries: reports,
    bytes_written: landing.bytes_written,
    durability,
    degraded: landing.degraded,
    barriers: landing.barriers,
    cloned: landing.cloned,
    costs,
    swept,
    target: target.dir,
  }
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

/// Whether `after` is the witnessed object `witness` after a move: every field equal but those a move or a
/// metadata-only change may set without touching the bytes — the ctime, the owner a `chown` moves with it, the birth
/// time, and Windows' `ARCHIVE` bit (A-185). NTFS gives a file renamed onto a name another file left moments before that
/// file's creation time ("tunneling"), and the landing's fallback does exactly that (the old file onto the hidden name
/// the temporary just left): measured on jupiter (Windows 10.0.26200, 2026-10-26), every replacement was undone
/// `TargetInUse` with the birth time the only other difference. A rename may set `ARCHIVE` on the file it moves, and a
/// backup clears it from every file it copied, so a witnessed file without it could never be moved and found the same
/// (`a_file_whose_archive_bit_a_backup_cleared_is_replaced_by_a_landing`, `tests/os.rs`). The device, the file index,
/// the size, the mtime, the mode and the other attribute bits still name the object, and a moved file that differs from
/// its witness in any field still has its bytes compared (`displaced_matches`).
fn same_object_after_a_move(witness: &Fingerprint, after: &Fingerprint) -> bool {
  /// Format: the attribute bits a move may set without touching the bytes, set aside.
  const MOVE_SETS: u16 = WindowsAttributes::ARCHIVE;
  Fingerprint {
    ctime_ns: witness.ctime_ns,
    uid: witness.uid,
    gid: witness.gid,
    birth_ns: witness.birth_ns,
    windows: (after.windows & !MOVE_SETS) | (witness.windows & MOVE_SETS),
    ..*after
  } == *witness
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A staged file of `size` bytes whose content hashes to `hash`, held as temporary `temp`.
  fn staged(temp: u64, hash: u8, size: u64) -> Staged {
    Staged {
      index: 0,
      copy: FileCopy {
        temp: HostFile(temp),
        dir: HostDir(1),
        created: ".slates-0000000000000001-0".into(),
        inode: slates_vfs::ids::InodeNo(temp),
        next: size,
        overlay: OverlayIdentity {
          hash: [hash; 32],
          size,
          mode: 0o644,
          mtime_ns: 1,
        },
        started: Instant::now(),
      },
      witness: None,
      entry_start_ns: 0,
    }
  }

  /// §4.15 step 6, A-445 (the landing's own index, never a scan of the disk). Do: look up a file's bytes in a staged batch
  /// holding two files with its hash, one of another length, and one of another hash. Expect the first staged file with
  /// both the same BLAKE3 and the same length; none for a length or a hash the batch does not hold, or an empty batch.
  #[test]
  fn a_file_is_cloned_only_from_a_staged_file_with_its_hash_and_its_length() {
    let batch: VecDeque<Staged> = [
      staged(7, 0xAA, 10),
      staged(8, 0xBB, 4096),
      staged(9, 0xBB, 4096),
      staged(10, 0xBB, 99),
    ]
    .into();
    let wanted = |hash: u8, size: u64| OverlayIdentity {
      hash: [hash; 32],
      size,
      mode: 0o600,
      mtime_ns: 2,
    };
    assert_eq!(
      clone_source(&batch, &wanted(0xBB, 4096)),
      Some(HostFile(8)),
      "the first such file"
    );
    assert_eq!(clone_source(&batch, &wanted(0xBB, 99)), Some(HostFile(10)));
    assert_eq!(
      clone_source(&batch, &wanted(0xBB, 100)),
      None,
      "a length the batch does not hold"
    );
    assert_eq!(
      clone_source(&batch, &wanted(0xCC, 4096)),
      None,
      "a hash the batch does not hold"
    );
    assert_eq!(clone_source(&VecDeque::new(), &wanted(0xAA, 10)), None);
  }
}
