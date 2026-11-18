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
//! reported as if it did). The status reply carries them typed (`StatusReport::base`, A-299); its `drifted` and
//! `watcher` text is for people, and this crate never reads it.

use std::path::PathBuf;

use slates_ipc::protocol::{
  self as wire, GreenBase, JournalReport, LossReport, MountReport, NamePolicy, PlacedState,
  SnapshotId, StatusReport, TransportReport, VolumeId,
};

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
  /// What an owner loss would lose of the volume now, and what the takeover that made this node its owner lost
  /// (§4.10 "Auto-seal", AC-8.3).
  pub loss: LossReport,
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriftReport {
  /// Each entry the disk changed after the volume witnessed it: landing it would conflict.
  pub drifted: Vec<Drift>,
  /// Each entry whose re-check the host refused now: not known to match its witness.
  pub unverified: Vec<String>,
  /// The base's watcher.
  pub watcher: Watcher,
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

/// A volume's status reply, typed.
pub(crate) fn status_of(report: StatusReport) -> VolumeStatus {
  VolumeStatus {
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
    base: report.base.map(base_state_of),
    loss: report.loss,
  }
}

/// An overlay's base as the reply carries it, typed (A-299).
fn base_state_of(base: wire::BaseReport) -> BaseState {
  match base {
    wire::BaseReport::Read {
      drifted,
      unverified,
      watcher,
    } => BaseState::Read(DriftReport {
      drifted: drifted
        .into_iter()
        .map(|entry| Drift {
          path: entry.path,
          kind: match entry.kind {
            wire::DriftKind::Modified => DriftKind::Modified,
            wire::DriftKind::Deleted => DriftKind::Deleted,
            wire::DriftKind::Replaced => DriftKind::Replaced,
            wire::DriftKind::TypeChanged => DriftKind::TypeChanged,
          },
        })
        .collect(),
      unverified,
      watcher: match watcher {
        wire::WatcherState::Live => Watcher::Live,
        wire::WatcherState::Overflowed => Watcher::Overflowed,
        wire::WatcherState::Unavailable => Watcher::Unavailable,
      },
    }),
    wire::BaseReport::Unreadable => BaseState::Unreadable,
  }
}

#[cfg(test)]
mod tests {
  use slates_ipc::protocol::{self as wire, DriftEntry};

  use super::{BaseState, Drift, DriftKind, DriftReport, Watcher, base_state_of};

  /// A-299, §4.15 "Drift": do type a base the status read, with an entry of each kind and one whose re-check the host
  /// refused, and one it could not read; expect each entry typed at its path, the refused one apart, the watcher, and
  /// an unreadable base kept as one rather than failing the status.
  #[test]
  fn a_base_report_is_typed_entry_by_entry() {
    let kinds = [
      (wire::DriftKind::Modified, DriftKind::Modified),
      (wire::DriftKind::Deleted, DriftKind::Deleted),
      (wire::DriftKind::Replaced, DriftKind::Replaced),
      (wire::DriftKind::TypeChanged, DriftKind::TypeChanged),
    ];
    let read = base_state_of(wire::BaseReport::Read {
      drifted: kinds
        .iter()
        .map(|(kind, _)| DriftEntry {
          path: format!("/{kind:?} (Modified)"),
          kind: *kind,
        })
        .collect(),
      unverified: vec!["/u".to_owned()],
      watcher: wire::WatcherState::Overflowed,
    });
    assert_eq!(
      read,
      BaseState::Read(DriftReport {
        drifted: kinds
          .iter()
          .map(|(wire_kind, kind)| Drift {
            path: format!("/{wire_kind:?} (Modified)"),
            kind: *kind,
          })
          .collect(),
        unverified: vec!["/u".to_owned()],
        watcher: Watcher::Overflowed,
      })
    );
    assert_eq!(
      base_state_of(wire::BaseReport::Unreadable),
      BaseState::Unreadable
    );
  }
}
