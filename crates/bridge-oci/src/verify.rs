//! The verification that a host path is the mount point of this volume's export (§4.6 A-9 "A
//! metadata record is insufficient evidence of a usable container path"; §4.4 "Attach with a chosen
//! path that cannot be honoured: Refused (`ChosenPathUnavailable{reason}`)"). Pure over a mount
//! table: the one platform seam is [`host_mount_kind`], which names the host mount a platform's
//! daemon serves, and [`expected_mount`] turns it into what the table must show for a volume. The
//! checks run in the order that consults the least: an absolute path first, then the table.

use crate::mount_table::MountEntry;

/// The host mount a platform establishes for a volume (§4.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostMountKind {
  /// The macOS NFS loopback mount: type `nfs`, source `localhost:/<name>` (`crates/cli/src/mount.rs`).
  NfsLoopback,
  /// The Linux FUSE mount: type `fuse.slates`, source `slates` for every volume
  /// (`crates/bridge-fuse/src/mount.rs`, `fsname=slates,subtype=slates`).
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
  /// The source that names the volume, where the bridge's source does.
  pub source: Option<String>,
}

/// Format: the filesystem type the macOS NFS client records for a mount (`statfs.f_fstypename`).
const NFS_FSTYPE: &str = "nfs";
/// Format: the export name `slates mount` passes: `localhost:/<volume name>` (`crates/cli/src/mount.rs`).
const NFS_SOURCE_PREFIX: &str = "localhost:/";
/// Format: the FUSE filesystem type the kernel records for `subtype=slates` (`fuse.<subtype>`).
const FUSE_FSTYPE: &str = "fuse.slates";

/// The table entry a host mount of `volume_name` shows.
pub fn expected_mount(kind: HostMountKind, volume_name: &str) -> ExpectedMount {
  match kind {
    HostMountKind::NfsLoopback => ExpectedMount {
      fstype: NFS_FSTYPE,
      source: Some(format!("{NFS_SOURCE_PREFIX}{volume_name}")),
    },
    HostMountKind::Fuse => ExpectedMount {
      fstype: FUSE_FSTYPE,
      source: None,
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
}

/// A host path the table vouches for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedHostMount {
  /// The mount point, as the table records it.
  pub mount_point: String,
  /// The filesystem type.
  pub fstype: String,
  /// The source.
  pub source: String,
  /// Whether the source names the volume.
  pub names_volume: bool,
}

/// The path with trailing separators removed, the root kept as `/`.
fn normalized(path: &str) -> &str {
  let trimmed = path.trim_end_matches('/');
  if trimmed.is_empty() { "/" } else { trimmed }
}

/// Verifies `host_path` against `entries`: absolute, exactly a mount point (the last mount at that
/// path is the visible one), of the expected type, and — where the source names volumes — of this
/// volume.
pub fn verify_host_mount(
  entries: &[MountEntry],
  host_path: &str,
  expected: &ExpectedMount,
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
  if let Some(source) = &expected.source
    && &entry.source != source
  {
    return Err(HostPathRefusal::NotThisVolume {
      source: entry.source.clone(),
    });
  }
  Ok(VerifiedHostMount {
    mount_point: entry.mount_point.clone(),
    fstype: entry.fstype.clone(),
    source: entry.source.clone(),
    names_volume: expected.source.is_some(),
  })
}
