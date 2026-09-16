//! The container bind form of `attach` (§4.6 A-9 "A host OCI runtime passes the established host
//! attachment into the container mount namespace"; §4.4 `attach(…, transport, chosen_path?)`;
//! RQ-20). The daemon's part is small and exact: for a consumer whose rights already admit the intent,
//! it verifies that the host path is the mount point of this volume's export through the kernel's
//! mount table (`slates_bridge_oci`, which never touches the mount), records the authorized binding
//! (`AttachForm::Oci`), and returns the runtime-specification `mounts` entry the harness hands its
//! runtime. It enters no namespace, creates no directory, runs no runtime and asks for no privilege
//! (R10); the runtime binds, and the container's view is the host mount's — proven by use in T-4.13.
//!
//! Refusals, all before the lease or the record: no host mount offered on this host
//! (`AttachmentUnsupported{Oci, HostMountRequired}`), a snapshot (the host mount presents the live
//! head: `SnapshotNotPresentedByHostMount`), and every `ChosenPathUnavailable{reason}` of the crate's
//! closed taxonomy (relative paths, not a mount point, a foreign filesystem, another volume, an
//! unreadable table).

use slates_ipc::protocol::{AttachTransport, OciBinding, Refusal, UnsupportedReason};
#[cfg(unix)]
use slates_ipc::protocol::{HostMountEvidence, HostPathReason};

/// The verified binding of `source` (a mount point of the volume named `volume_name`) at
/// `destination`, read-only or not.
#[cfg(unix)]
pub(crate) fn bind(
  volume_name: &str,
  source: &str,
  destination: &str,
  read_only: bool,
) -> Result<OciBinding, Refusal> {
  use slates_bridge_oci::binding::{DestinationRefusal, OciMountEntry};
  use slates_bridge_oci::mount_table::mount_table;
  use slates_bridge_oci::verify::{
    HostPathRefusal, expected_mount, host_mount_kind, verify_host_mount,
  };

  let Some(kind) = host_mount_kind() else {
    return Err(Refusal::AttachmentUnsupported {
      transport: AttachTransport::Oci,
      reason: UnsupportedReason::HostMountRequired,
    });
  };
  let chosen = |reason| Refusal::ChosenPathUnavailable { reason };
  // The pure checks first, so nothing is consulted for a request that cannot be honoured.
  if !source.starts_with('/') {
    return Err(chosen(HostPathReason::NotAbsolute));
  }
  if !destination.starts_with('/') {
    return Err(chosen(HostPathReason::DestinationNotAbsolute));
  }
  let entries = mount_table()
    .map_err(|e| chosen(HostPathReason::MountTableUnavailable { errno: e.errno() }))?;
  let verified =
    verify_host_mount(&entries, source, &expected_mount(kind, volume_name)).map_err(|refusal| {
      chosen(match refusal {
        HostPathRefusal::NotAbsolute => HostPathReason::NotAbsolute,
        HostPathRefusal::NotAMountPoint => HostPathReason::NotAMountPoint,
        HostPathRefusal::ForeignFilesystem { fstype } => {
          HostPathReason::ForeignFilesystem { fstype }
        }
        HostPathRefusal::NotThisVolume { source } => HostPathReason::NotThisVolume { source },
      })
    })?;
  let entry = OciMountEntry::new(&verified, destination, read_only).map_err(|e| match e {
    DestinationRefusal::NotAbsolute => chosen(HostPathReason::DestinationNotAbsolute),
  })?;
  Ok(OciBinding {
    source: entry.source.clone(),
    destination: entry.destination.clone(),
    read_only,
    mount_type: entry.mount_type().to_owned(),
    options: entry.options(),
    evidence: HostMountEvidence {
      fstype: verified.fstype,
      mount_source: verified.source,
      names_volume: verified.names_volume,
    },
  })
}

/// There is no host mount to bind on this platform.
#[cfg(not(unix))]
pub(crate) fn bind(
  _volume_name: &str,
  _source: &str,
  _destination: &str,
  _read_only: bool,
) -> Result<OciBinding, Refusal> {
  Err(Refusal::AttachmentUnsupported {
    transport: AttachTransport::Oci,
    reason: UnsupportedReason::HostPlatform,
  })
}
