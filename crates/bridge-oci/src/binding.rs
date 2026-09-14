//! The `mounts[]` entry the harness hands its OCI runtime (§4.6 A-9: "A host OCI runtime passes the
//! established host attachment into the container mount namespace"): a bind mount of the verified
//! host mount point at a destination inside the container, read-only for a read attachment. The
//! vocabulary is the OCI runtime specification's (`config.md`, "Mounts", the Linux bind form:
//! `{"destination", "type": "bind", "source", "options": ["rbind", "ro"|"rw"]}`); the field names
//! and option words are quoted from memory of the specification and flagged for verification in
//! `docs/wip/oci-handoff.md`. Docker's equivalent is `-v <source>:<destination>:<ro|rw>`.

use crate::verify::VerifiedHostMount;

/// Format: the entry's `type` for a bind mount (OCI runtime specification, Linux mounts).
pub const MOUNT_TYPE: &str = "bind";
/// Format: the recursive bind option (the runtime applies the bind to every mount beneath the source).
pub const OPTION_RBIND: &str = "rbind";
/// Format: the read-only option; the runtime remounts the bind read-only and a write gets `EROFS`.
pub const OPTION_RO: &str = "ro";
/// Format: the read-write option.
pub const OPTION_RW: &str = "rw";

/// Why a destination cannot be honoured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DestinationRefusal {
  /// The specification requires an absolute destination path.
  NotAbsolute,
}

/// One bind-mount entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OciMountEntry {
  /// The container path.
  pub destination: String,
  /// The host mount point, as verified.
  pub source: String,
  /// Whether the bind is read-only.
  pub read_only: bool,
}

impl OciMountEntry {
  /// The entry binding `source` at `destination`; the destination must be absolute.
  pub fn new(
    source: &VerifiedHostMount,
    destination: &str,
    read_only: bool,
  ) -> Result<OciMountEntry, DestinationRefusal> {
    if !destination.starts_with('/') {
      return Err(DestinationRefusal::NotAbsolute);
    }
    Ok(OciMountEntry {
      destination: destination.to_owned(),
      source: source.mount_point.clone(),
      read_only,
    })
  }

  /// The entry's `type`.
  pub const fn mount_type(&self) -> &'static str {
    MOUNT_TYPE
  }

  /// The entry's `options`: the recursive bind, then `ro` or `rw`.
  pub fn options(&self) -> Vec<String> {
    let access = if self.read_only { OPTION_RO } else { OPTION_RW };
    vec![OPTION_RBIND.to_owned(), access.to_owned()]
  }
}
