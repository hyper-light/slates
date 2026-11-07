//! The verification that a host path is the mount point of this volume's export (§4.6 A-9 "A
//! metadata record is insufficient evidence of a usable container path"; §4.4 "Attach with a chosen
//! path that cannot be honoured: Refused (`ChosenPathUnavailable{reason}`)"). Pure over a mount
//! table: the one platform seam is [`host_mount_kind`], which names the host mount a platform's
//! daemon serves, and [`expected_mount`] turns it into what the table must show for a volume. The
//! checks run in the order that consults the least: an absolute path first, then the table.

use crate::mount_table::{MountEntry, MountIdentity};

/// The host mount a platform establishes for a volume (§4.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostMountKind {
  /// The macOS NFS loopback mount: source `slates:/<name>` (§4.6 A-34; no capability in the table).
  NfsLoopback,
  /// The Linux FUSE mount: type `fuse.slates`, source `slates:<tag>`, the tag of the daemon that made it (a random
  /// value per daemon start that names nothing else, A-227: the table is world-readable, so it carries no attachment).
  Fuse,
}

/// The host mount kind of this build's platform, or none.
pub const fn host_mount_kind() -> Option<HostMountKind> {
  if cfg!(target_os = "macos") {
    Some(HostMountKind::NfsLoopback)
  } else if cfg!(target_os = "linux") {
    Some(HostMountKind::Fuse)
  } else {
    None
  }
}

/// What the mount table must show for a volume's host mount.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpectedMount {
  /// The filesystem type the bridge mounts as.
  pub fstype: &'static str,
  /// What the source must be.
  pub source: SourceRule,
}

/// How a host mount's source names what it serves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceRule {
  /// Exactly this source: the macOS loopback mount's `slates:/<volume name>`.
  Exactly(String),
  /// A slates FUSE source (`slates:` and a daemon's tag). The daemon then holds the mount's table id to its own
  /// mounts (A-227) and the mount point to its live record (the volume, the principal), since a name in a table is no
  /// authority by itself.
  Slates,
}

/// Format: the filesystem type the macOS NFS client records for a mount (`statfs.f_fstypename`).
const NFS_FSTYPE: &str = "nfs";
/// Format: the source `slates mount` gives the loopback mount on macOS: `slates:/<volume name>`
/// (`crates/cli/src/mount.rs`, `NFS_MATTR_MNTFROM`), which carries no capability (§4.6 A-34).
const NFS_SOURCE_PREFIX: &str = "slates:/";
/// Format: the FUSE filesystem type the kernel records for `subtype=slates` (`fuse.<subtype>`).
const FUSE_FSTYPE: &str = "fuse.slates";
/// Format: the prefix of a slates FUSE mount's source, before the tag of the daemon that made it.
const SLATES_SOURCE_PREFIX: &str = "slates:";

/// The table entry a host mount of `volume_name` shows.
pub fn expected_mount(kind: HostMountKind, volume_name: &str) -> ExpectedMount {
  match kind {
    HostMountKind::NfsLoopback => ExpectedMount {
      fstype: NFS_FSTYPE,
      source: SourceRule::Exactly(format!("{NFS_SOURCE_PREFIX}{volume_name}")),
    },
    HostMountKind::Fuse => ExpectedMount {
      fstype: FUSE_FSTYPE,
      source: SourceRule::Slates,
    },
  }
}

/// Why a host path is not this volume's mount point; the closed taxonomy the daemon maps to
/// `ChosenPathUnavailable{reason}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostPathRefusal {
  /// Not an absolute path; the table was not consulted.
  NotAbsolute,
  /// No mount at exactly this path.
  NotAMountPoint,
  /// A mount of another filesystem type.
  ForeignFilesystem {
    /// The type the table records.
    fstype: String,
  },
  /// A slates export of another volume.
  NotThisVolume {
    /// The source the table records.
    source: String,
  },
  /// Another mount beneath the source mount point (AUD-29-65).
  DescendantMount {
    /// The mount point beneath the source.
    mount_point: String,
  },
}

/// A host path the table vouches for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedHostMount {
  /// The mount point, as the table records it.
  pub mount_point: String,
  /// The filesystem type.
  pub fstype: String,
  /// The source the table records.
  pub source: String,
  /// Whether the source names what the mount serves: the volume itself (macOS), or a slates FUSE mount whose table id
  /// and mount point the daemon then holds to its own mounts and record (Linux).
  pub names_volume: bool,
  /// The table's own id of the verified mount, which the kernel reuses: what the daemon holds to its own mounts for a
  /// slates FUSE source ([`SourceRule::Slates`], A-227).
  pub table_id: u64,
  /// The kernel's identity of the verified mount instance, which the harness checks again just before its
  /// runtime binds the path ([`source_unchanged`]; AUD-29-66).
  pub identity: MountIdentity,
}

/// Why a verified source is no longer the mount that was verified (AUD-29-66).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceChange {
  /// No mount is at the path any more: a runtime would bind whatever directory is there, or create one.
  Missing,
  /// Another mount instance is at the path: unmounted and mounted again, or something mounted over it.
  Replaced {
    /// The identity verified.
    verified: MountIdentity,
    /// The identity there now.
    found: MountIdentity,
  },
}

/// Whether `source` is still the mount instance verified as `identity`: the check a harness runs immediately
/// before it hands the bind to its runtime, so it never binds a replacement or a bare directory left where the
/// mount was. `entries` (the kernel's table now) says whether anything is mounted there at all; `live` is that
/// path's identity read now through [`crate::mount_table::mount_identity_at`], and it is what decides, because
/// the table's own mount id is reused by the kernel and a replacement reproduces it. Pure, so it is tested on
/// every host.
pub fn source_unchanged(
  entries: &[MountEntry],
  source: &str,
  identity: MountIdentity,
  live: MountIdentity,
) -> Result<(), SourceChange> {
  let path = normalized(source);
  entries
    .iter()
    .rev()
    .find(|entry| normalized(&entry.mount_point) == path)
    .ok_or(SourceChange::Missing)?;
  if live != identity {
    return Err(SourceChange::Replaced {
      verified: identity,
      found: live,
    });
  }
  Ok(())
}

/// Whether the kernel's table shows a mount at exactly `source`. The caller asks this before reading the
/// path's identity, so a source whose mount is simply gone is `Missing` and not a failed identity read: on a
/// path with no mount there is nothing to identify, and the two answers mean different things to a harness.
pub fn mounted_at(entries: &[MountEntry], source: &str) -> bool {
  let path = normalized(source);
  entries
    .iter()
    .any(|entry| normalized(&entry.mount_point) == path)
}

/// The path with trailing separators removed, the root kept as `/`.
fn normalized(path: &str) -> &str {
  let trimmed = path.trim_end_matches('/');
  if trimmed.is_empty() { "/" } else { trimmed }
}

/// Verifies `host_path` against `entries`: absolute, exactly a mount point (the last mount at that
/// path is the visible one), of the expected type, and — where the source names volumes — of this
/// volume.
/// `identity` is the mount's identity read now through [`crate::mount_table::mount_identity_at`], which is what
/// the harness will compare against later; the table's own id is reused by the kernel and cannot serve.
pub fn verify_host_mount(
  entries: &[MountEntry],
  host_path: &str,
  expected: &ExpectedMount,
  identity: MountIdentity,
) -> Result<VerifiedHostMount, HostPathRefusal> {
  if !host_path.starts_with('/') {
    return Err(HostPathRefusal::NotAbsolute);
  }
  let path = normalized(host_path);
  let entry = entries
    .iter()
    .rev()
    .find(|entry| normalized(&entry.mount_point) == path)
    .ok_or(HostPathRefusal::NotAMountPoint)?;
  if entry.fstype != expected.fstype {
    return Err(HostPathRefusal::ForeignFilesystem {
      fstype: entry.fstype.clone(),
    });
  }
  // The topology is the source mount alone: a mount beneath it would be outside the non-recursive bind,
  // and its filesystem is no slates attachment's.
  if let Some(beneath) = entries.iter().find(|other| {
    let point = normalized(&other.mount_point);
    point != path
      && point
        .strip_prefix(path)
        .is_some_and(|rest| rest.starts_with('/') || path == "/")
  }) {
    return Err(HostPathRefusal::DescendantMount {
      mount_point: beneath.mount_point.clone(),
    });
  }
  let not_this = || HostPathRefusal::NotThisVolume {
    source: entry.source.clone(),
  };
  match &expected.source {
    SourceRule::Exactly(source) if entry.source == *source => {}
    SourceRule::Slates if entry.source.starts_with(SLATES_SOURCE_PREFIX) => {}
    SourceRule::Exactly(_) | SourceRule::Slates => return Err(not_this()),
  }
  Ok(VerifiedHostMount {
    mount_point: entry.mount_point.clone(),
    fstype: entry.fstype.clone(),
    source: entry.source.clone(),
    names_volume: true,
    table_id: entry.identity.mount,
    identity,
  })
}
