//! Volumes as the consumer contract provisions and reports them (§4.12 "The consumer contract", A-299; §4.4, §4.15,
//! §4.16): one typed create for every source a volume can have, a typed status, and an overlay's drift as typed
//! entries.
//!
//! **Provisioning.** [`VolumeSpec`] names a volume and its [`Source`]: an overlay over a host folder (§4.15, D-25), an
//! empty volume (§4.2's bounded class), a clone of a snapshot (§4.4, D-5), a green (§4.16), or a work volume over a
//! green. [`crate::Instance::provision`] makes it and answers what it made ([`Provisioned`]): every source answers the
//! volume, and a work volume also its green and the green version it starts from.
//!
//! **Drift (§4.15 "Drift").** An overlay reports each entry the disk changed after the volume witnessed it, typed by
//! what happened ([`DriftKind`]), and each entry whose re-check the host refused now (not known to match, so never
//! reported as if it did). The daemon's status reply carries these as text today (`StatusReport::drifted`, `watcher`);
//! this module reads them in one place ([`base_of`]), by the grammar the daemon writes them in, until the reply carries
//! them typed (GAPS, A-299's owed list), when the reading is replaced.

use std::path::PathBuf;

use slates_ipc::protocol::{
  GreenBase, JournalReport, MountReport, NamePolicy, PlacedState, SnapshotId, StatusReport,
  TransportReport, VolumeId,
};

use crate::error::{Error, ProtocolFault};
use crate::outcome::Work;

/// A volume to make: its name, and what it starts from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeSpec {
  /// The name, unique among the caller's volumes.
  pub name: String,
  /// What the volume starts from.
  pub source: Source,
}

impl VolumeSpec {
  /// An overlay named `name` over the host folder `folder`, growing up to `max_bytes`, its names byte-exact (a host
  /// that overlays a case-folding folder names [`NamePolicy::Fold`] itself).
  pub fn overlay(name: &str, folder: impl Into<PathBuf>, max_bytes: u64) -> VolumeSpec {
    VolumeSpec {
      name: name.to_owned(),
      source: Source::Overlay {
        folder: folder.into(),
        max_bytes,
        names: NamePolicy::Exact,
      },
    }
  }

  /// An empty volume named `name` with `limit_bytes` reserved, its names byte-exact.
  pub fn empty(name: &str, limit_bytes: u64) -> VolumeSpec {
    VolumeSpec {
      name: name.to_owned(),
      source: Source::Empty {
        limit_bytes,
        names: NamePolicy::Exact,
      },
    }
  }

  /// A clone named `name` of `snapshot` of `volume`.
  pub fn clone_of(name: &str, volume: VolumeId, snapshot: SnapshotId) -> VolumeSpec {
    VolumeSpec {
      name: name.to_owned(),
      source: Source::Clone { volume, snapshot },
    }
  }

  /// A work volume named `name` over `green`, at its head.
  pub fn work(name: &str, green: VolumeId) -> VolumeSpec {
    VolumeSpec {
      name: name.to_owned(),
      source: Source::Work { green },
    }
  }
}

/// What a volume starts from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
  /// Laid over a host folder (§4.15, D-25): it reads every entry it has not changed from the folder as the folder is
  /// now and keeps its own changes in memory, growing as it needs up to `max_bytes` (§4.2's dynamic class). The folder
  /// is absolute; a relative one is refused [`crate::PathFault::NotAbsolute`] before the daemon is asked.
  Overlay {
    /// The folder.
    folder: PathBuf,
    /// The most the volume's own changes may hold, in bytes.
    max_bytes: u64,
    /// How names compare: byte-exact, or folded as a case-insensitive folder's are.
    names: NamePolicy,
  },
  /// Empty, with no base: `limit_bytes` reserved at creation (§4.2's bounded class).
  Empty {
    /// The quota, in bytes.
    limit_bytes: u64,
    /// How names compare.
    names: NamePolicy,
  },
  /// A clone of a snapshot (§4.4, D-5): it shares every chunk with its origin until either writes.
  Clone {
    /// The origin volume.
    volume: VolumeId,
    /// The snapshot cloned.
    snapshot: SnapshotId,
  },
  /// A green (§4.16): the line merges commit to, starting empty, or from a complete immutable snapshot (never one
  /// still served live from a host folder: refused [`crate::Invalid::LiveBase`]).
  Green {
    /// The snapshot that is the green's version 0, or none for an empty green.
    base: Option<GreenBase>,
    /// Every merge must carry evidence (§4.16).
    require_evidence: bool,
  },
  /// A work volume over a green, at the green's head (§4.16): where an agent works before it merges.
  Work {
    /// The green.
    green: VolumeId,
  },
}

/// What [`crate::Instance::provision`] made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provisioned {
  /// A volume of any source but a work's.
  Volume(VolumeId),
  /// A work volume: the work and its green, and the green version it starts from.
  Work {
    /// The work and its green.
    work: Work,
    /// The green version the work is based on.
    base: u64,
  },
}

impl Provisioned {
  /// The volume made.
  pub fn volume(&self) -> VolumeId {
    match self {
      Self::Volume(volume) => *volume,
      Self::Work { work, .. } => work.volume,
    }
  }
}

/// A volume's status (§4.4 `status`, §4.14), typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeStatus {
  /// The volume.
  pub id: VolumeId,
  /// Its name.
  pub name: String,
  /// Bytes referenced.
  pub referenced_bytes: u64,
  /// Bytes only it holds.
  pub unique_bytes: u64,
  /// The write lease's epoch, when one is held (D-16).
  pub lease_epoch: Option<u64>,
  /// Its attachments.
  pub attachments: u32,
  /// The head snapshot.
  pub head: SnapshotId,
  /// Snapshots held.
  pub snapshots: u32,
  /// Where the head is placed (§4.8, D-18).
  pub placed: PlacedState,
  /// The daemon's NFS loopback port, when it serves NFS (§4.6).
  pub nfs_port: Option<u16>,
  /// Every transport this host offers or refuses for the volume, with why (§4.6 A-9).
  pub transports: TransportReport,
  /// The host mounts bound to a path, as many as the report carries.
  pub mounts: Vec<MountReport>,
  /// Bound mounts the report did not carry.
  pub mounts_elided: u32,
  /// The volume's journal (§4.5).
  pub journal: JournalReport,
  /// Unlinked files kept for a mount that survived a takeover (U10).
  pub takeover_kept: u32,
  /// An overlay's base as the status read it; none for a volume with no base.
  pub base: Option<BaseState>,
}

/// An overlay's base as a status found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BaseState {
  /// The base was read: its drift.
  Read(DriftReport),
  /// The base could not be read now: its drift is unknown, and the rest of the status stands.
  Unreadable,
}

/// An overlay's base as a status read it (§4.15 "Drift").
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct DriftReport {
  /// Each entry the disk changed after the volume witnessed it: landing it would conflict.
  pub drifted: Vec<Drift>,
  /// Each entry whose re-check the host refused now: not known to match its witness.
  pub unverified: Vec<String>,
  /// The watcher's state, when the base has one.
  pub watcher: Option<Watcher>,
}

/// One drifted entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Drift {
  /// The entry, from the volume's root.
  pub path: String,
  /// What the disk did to it.
  pub kind: DriftKind,
}

/// What changed on the disk beneath a witnessed entry (§4.15 "Drift").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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

impl DriftKind {
  /// The kind by the name the daemon writes.
  fn of_name(name: &str) -> Option<DriftKind> {
    match name {
      "Modified" => Some(Self::Modified),
      "Deleted" => Some(Self::Deleted),
      "Replaced" => Some(Self::Replaced),
      "TypeChanged" => Some(Self::TypeChanged),
      _ => None,
    }
  }
}

/// An overlay's watcher (§4.15): hints that make drift reports prompt, never the truth.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Watcher {
  /// Delivering hints.
  Live,
  /// It lost events; a full re-check is done or scheduled.
  Overflowed,
  /// None on this host or folder: fingerprints alone.
  Unavailable,
}

/// The watcher state a status reply names for a volume with no base.
const NO_BASE: &str = "scratch";
/// The watcher state a status reply names when the base could not be read; its one drift entry then says why.
const BASE_REFUSED: &str = "unavailable";
/// What the daemon writes after an entry's path when the host refused its re-check.
const UNVERIFIED: &str = " (unverified: ";

/// A volume's status reply, typed.
pub(crate) fn status_of(report: StatusReport) -> Result<VolumeStatus, Error> {
  let base = base_of(&report.drifted, &report.watcher)?;
  Ok(VolumeStatus {
    id: report.id,
    name: report.name,
    referenced_bytes: report.referenced_bytes,
    unique_bytes: report.unique_bytes,
    lease_epoch: report.lease_epoch,
    attachments: report.attachments,
    head: report.head,
    snapshots: report.snapshots,
    placed: report.placed,
    nfs_port: report.nfs_port,
    transports: *report.transports,
    mounts: report.mounts,
    mounts_elided: report.mounts_elided,
    journal: report.journal,
    takeover_kept: report.takeover_kept,
    base,
  })
}

/// An overlay's base from the status reply's text (`crates/server/src/verbs.rs`, the status verb): `watcher` names
/// [`NO_BASE`] for a volume with no base, [`BASE_REFUSED`] when the base could not be read, and otherwise the watcher's
/// state; each drift entry is `PATH (KIND)` for an entry the disk changed, or `PATH (unverified: REFUSAL)` for one
/// whose re-check the host refused. The kind is read from the entry's end, so a path holding parentheses of its own is
/// read whole. Refused as a protocol fault for a form the daemon does not write.
pub(crate) fn base_of(drifted: &[String], watcher: &str) -> Result<Option<BaseState>, Error> {
  let watcher = match watcher {
    NO_BASE => return Ok(None),
    BASE_REFUSED => return Ok(Some(BaseState::Unreadable)),
    "Live" => Watcher::Live,
    "Overflowed" => Watcher::Overflowed,
    "Unavailable" => Watcher::Unavailable,
    _ => return Err(unknown("watcher state")),
  };
  let mut report = DriftReport {
    watcher: Some(watcher),
    ..DriftReport::default()
  };
  for entry in drifted {
    match drift_line(entry)? {
      Line::Drifted(drift) => report.drifted.push(drift),
      Line::Unverified(path) => report.unverified.push(path),
    }
  }
  Ok(Some(BaseState::Read(report)))
}

/// One drift entry, read.
enum Line {
  /// The disk changed it.
  Drifted(Drift),
  /// The host refused its re-check.
  Unverified(String),
}

/// One drift entry from its text (see [`base_of`]).
fn drift_line(entry: &str) -> Result<Line, Error> {
  let inner = entry
    .strip_suffix(')')
    .ok_or_else(|| unknown("drift entry"))?;
  if let Some((path, kind)) = inner.rsplit_once(" (")
    && let Some(kind) = DriftKind::of_name(kind)
  {
    return Ok(Line::Drifted(Drift {
      path: path.to_owned(),
      kind,
    }));
  }
  inner
    .rfind(UNVERIFIED)
    .and_then(|at| inner.get(..at))
    .map(|path| Line::Unverified(path.to_owned()))
    .ok_or_else(|| unknown("drift entry"))
}

/// A form the daemon does not write.
fn unknown(field: &'static str) -> Error {
  Error::Protocol(ProtocolFault::UnknownName { field })
}

#[cfg(test)]
mod tests {
  use super::{BaseState, Drift, DriftKind, DriftReport, Watcher, base_of};
  use crate::error::{Error, ProtocolFault};

  /// A-299, §4.15 "Drift": do read the drift list a status reply carries, with each kind, a path holding parentheses
  /// of its own (one that ends like an unverified entry, one like a kind), and an unverified entry whose refusal
  /// carries parentheses; expect each typed by what happened at its whole path, the unverified one apart, and the
  /// watcher's state.
  #[test]
  fn each_drift_entry_is_typed_at_its_whole_path() {
    let entries = [
      "/a.txt (Modified)",
      "/gone (Deleted)",
      "/b (Replaced)",
      "/d (TypeChanged)",
      "/x (unverified: y (Modified)",
      "/odd (Deleted) (Modified)",
      "/u (unverified: ENOMEM (the arena is full))",
    ]
    .map(str::to_owned);
    let Some(BaseState::Read(report)) = base_of(&entries, "Overflowed").unwrap() else {
      panic!("a read base reports its drift");
    };
    let drift = |path: &str, kind| Drift {
      path: path.to_owned(),
      kind,
    };
    assert_eq!(
      report,
      DriftReport {
        drifted: vec![
          drift("/a.txt", DriftKind::Modified),
          drift("/gone", DriftKind::Deleted),
          drift("/b", DriftKind::Replaced),
          drift("/d", DriftKind::TypeChanged),
          drift("/x (unverified: y", DriftKind::Modified),
          drift("/odd (Deleted)", DriftKind::Modified),
        ],
        unverified: vec!["/u".to_owned()],
        watcher: Some(Watcher::Overflowed),
      }
    );
  }

  /// A-299: do read a volume with no base, one whose base could not be read, a watcher state and an entry the daemon
  /// does not write; expect no base, an unreadable one (the rest of the status stands), and two protocol faults —
  /// never a guess.
  #[test]
  fn a_missing_or_unreadable_base_and_a_stranger_are_typed() {
    assert_eq!(base_of(&[], "scratch"), Ok(None));
    assert_eq!(
      base_of(&["status refused: Io".to_owned()], "unavailable"),
      Ok(Some(BaseState::Unreadable))
    );
    assert!(matches!(
      base_of(&[], "Sleeping"),
      Err(Error::Protocol(ProtocolFault::UnknownName { .. }))
    ));
    assert!(matches!(
      base_of(&["/a (Renamed)".to_owned()], "Live"),
      Err(Error::Protocol(ProtocolFault::UnknownName { .. }))
    ));
  }
}
