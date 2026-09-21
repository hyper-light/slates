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
  /// The macOS NFS loopback mount: `localhost:/<name>@<attachment>.<token>` (§4.13).
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
  /// The source prefix that names the volume, before its bearer capability.
  pub source: Option<String>,
}

/// Format: the filesystem type the macOS NFS client records for a mount (`statfs.f_fstypename`).
const NFS_FSTYPE: &str = "nfs";
/// Format: the export name `slates mount` passes: `localhost:/<volume name>` (`crates/cli/src/mount.rs`).
const NFS_SOURCE_PREFIX: &str = "localhost:/";
/// Format: the FUSE filesystem type the kernel records for `subtype=slates` (`fuse.<subtype>`).
const FUSE_FSTYPE: &str = "fuse.slates";
/// Format: hexadecimal encodes each byte in two digits; the mount token holds 16 bytes (§4.13).
const TOKEN_HEX_DIGITS: usize = size_of::<[u8; 16]>() * 2;
/// Format: the attachment id and token use base-16 digits in the mount source.
const HEX_RADIX: u32 = 16;

/// The exact volume name must precede a well-formed bearer suffix. This verifies the
/// kernel's source description; the NFS server separately validates authority per request.
fn source_capability(source: &str, expected: &str) -> Option<MountCapability> {
  let capability = source
    .strip_prefix(expected)
    .and_then(|tail| tail.strip_prefix('@'))?;
  let (attachment, token) = capability.split_once('.')?;
  if attachment.is_empty()
    || !attachment.bytes().all(|byte| byte.is_ascii_hexdigit())
    || token.len() != TOKEN_HEX_DIGITS
  {
    return None;
  }
  let attachment = u64::from_str_radix(attachment, HEX_RADIX).ok()?;
  let mut secret = [0u8; 16];
  for (pair, byte) in token.as_bytes().as_chunks::<2>().0.iter().zip(&mut secret) {
    let high = char::from(pair[0]).to_digit(HEX_RADIX)?;
    let low = char::from(pair[1]).to_digit(HEX_RADIX)?;
    *byte = u8::try_from(high * HEX_RADIX + low).ok()?;
  }
  Some(MountCapability {
    attachment,
    token: secret,
  })
}

/// The kernel's source capability, checked against the daemon's live attachment before an OCI
/// binding can borrow its lifetime (§4.6, §4.13). Debug output deliberately omits the bearer token.
#[derive(Clone, PartialEq, Eq)]
pub struct MountCapability {
  /// The source mount's attachment id.
  pub attachment: u64,
  /// The secret presented by the mount; never included in descriptive evidence.
  pub token: [u8; 16],
}

impl std::fmt::Debug for MountCapability {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("MountCapability")
      .field("attachment", &self.attachment)
      .finish_non_exhaustive()
  }
}

/// A bearer token has no place in descriptive evidence or a refusal.
fn source_without_capability(source: &str) -> &str {
  source.split_once('@').map_or(source, |(name, _)| name)
}

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
    /// The source the table records, with a bearer suffix removed.
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
  /// The source with its bearer capability removed.
  pub source: String,
  /// Whether the source names the volume.
  pub names_volume: bool,
  /// Authority to check against the live source mount, absent when the table cannot identify it.
  pub capability: Option<MountCapability>,
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
  let capability = expected
    .source
    .as_ref()
    .and_then(|source| source_capability(&entry.source, source));
  if expected.source.is_some() && capability.is_none() {
    return Err(HostPathRefusal::NotThisVolume {
      source: source_without_capability(&entry.source).to_owned(),
    });
  }
  Ok(VerifiedHostMount {
    mount_point: entry.mount_point.clone(),
    fstype: entry.fstype.clone(),
    source: source_without_capability(&entry.source).to_owned(),
    names_volume: expected.source.is_some(),
    capability,
  })
}
