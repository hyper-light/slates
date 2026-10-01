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

/// A verified bind recipe and the source mount it borrows, kept inside the daemon. Only the recipe
/// crosses the reply boundary.
pub(crate) struct Binding {
  /// The runtime's mount entry and descriptive evidence.
  pub entry: OciBinding,
  /// The source mount point the kernel's table vouches for (a `slates:/<name>` mount exactly there).
  pub mount_point: String,
  /// The attachment the source mount's table entry names (a Linux FUSE mount's `slates:<attachment>`), which
  /// the parent record must be; `None` where the table names the volume instead (macOS).
  pub attachment: Option<u64>,
}

impl Binding {
  /// The source mount's attachment, found in the daemon's own records: the host mount of this volume,
  /// for this principal, bound (`BindMount`) to exactly the mount point the kernel's table vouches for
  /// (§4.6 A-34: the table carries no capability, so the binding is proven by the record, not by a
  /// secret read back from the table) — and, where the table names an attachment (a Linux FUSE mount's
  /// `slates:<attachment>`; AUD-29-64), that very attachment, live: a stale, foreign or replaced source names
  /// an attachment the record does not hold at this point for this volume and principal. A stale or foreign source cannot create an orphan binding, and a
  /// bind cannot promise rights the source does not carry.
  pub(crate) fn consumer(
    &self,
    partition: &slates_db::partition::Partition,
    volume: slates_db::catalog::VolumeId,
    principal: &slates_db::catalog::Principal,
  ) -> Result<slates_db::catalog::Consumer, Refusal> {
    use slates_db::catalog::Consumer;
    let parent = partition
      .attachments_of(volume)
      .into_iter()
      .find(|parent| {
        matches!(parent.consumer, Consumer::Bridge)
          && self.attachment.is_none_or(|named| parent.id == named)
          && parent.principal == *principal
          && matches!(&parent.form, slates_db::catalog::AttachForm::ChosenPath { path } if *path == self.mount_point)
          && parent.rights.read
          && (self.entry.read_only || parent.rights.write)
      })
      .ok_or(Refusal::Forbidden {
        verb: "OCI source mount".to_owned(),
      })?;
    Ok(Consumer::Mount {
      attachment: parent.id,
    })
  }
}

/// The verified binding of `source` (a mount point of the volume named `volume_name`) at
/// `destination`, read-only or not.
#[cfg(unix)]
pub(crate) fn bind(
  volume_name: &str,
  source: &str,
  destination: &str,
  read_only: bool,
) -> Result<Binding, Refusal> {
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
        HostPathRefusal::DescendantMount { mount_point } => {
          HostPathReason::DescendantMount { mount_point }
        }
      })
    })?;
  let entry = OciMountEntry::new(&verified, destination, read_only).map_err(|e| match e {
    DestinationRefusal::NotAbsolute => chosen(HostPathReason::DestinationNotAbsolute),
  })?;
  if !verified.names_volume {
    return Err(Refusal::AttachmentUnsupported {
      transport: AttachTransport::Oci,
      reason: UnsupportedReason::HostMountRequired,
    });
  }
  Ok(Binding {
    mount_point: verified.mount_point.clone(),
    attachment: verified.attachment,
    entry: OciBinding {
      source: entry.source.clone(),
      destination: entry.destination.clone(),
      read_only,
      mount_type: entry.mount_type().to_owned(),
      options: entry.options(),
      evidence: HostMountEvidence {
        fstype: verified.fstype,
        mount_source: verified.source,
        names_volume: verified.names_volume,
        mount_id: verified.identity.mount,
        mount_device: verified.identity.device,
      },
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
) -> Result<Binding, Refusal> {
  Err(Refusal::AttachmentUnsupported {
    transport: AttachTransport::Oci,
    reason: UnsupportedReason::HostPlatform,
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use slates_db::Op;
  use slates_db::catalog::{
    AttachForm, AttachmentRecord, BaseRecord, Consumer, NamePolicy, PolicyRecord, Principal,
    Rights, Role, SizeClass, SnapshotId, VolumeId, VolumeRecord, VolumeState,
  };
  use slates_db::partition::{Partition, PartitionCaps};

  /// Shape: room for the few records these cases hold.
  const CAPS: PartitionCaps = PartitionCaps {
    volumes: 4,
    snapshots: 4,
    attachments: 4,
    segment_slots: 4,
    timers: 4,
    tick_ns: 1,
    green_chain_bytes: 1,
  };
  /// Shape: the mounting user and another.
  const OWNER: Principal = Principal::Uid { uid: 1000 };
  const STRANGER: Principal = Principal::Uid { uid: 1001 };
  /// Shape: the FUSE mount's attachment id, and an id no record holds.
  const MOUNTED: u64 = 7;
  const UNKNOWN: u64 = 9;
  /// Shape: where the mount is, and another path.
  const POINT: &str = "/home/u/mnt";
  const ELSEWHERE: &str = "/home/u/other";

  fn volume() -> VolumeRecord {
    VolumeRecord {
      id: VolumeId { bytes: [3; 16] },
      name: "work".to_owned(),
      owner_shard: 0,
      policy: PolicyRecord {
        size: SizeClass::Bounded { limit: 1 << 20 },
        names: NamePolicy::Exact,
        require_locked: false,
        role: Role::Plain,
      },
      base: BaseRecord::Scratch,
      head: SnapshotId { value: 0 },
      epoch: 0,
      referenced_bytes: 0,
      unique_bytes: 0,
      state: VolumeState::Live,
      lease: None,
      owner: OWNER,
      access: vec![],
      created_ns: 0,
      catalog_version: 0,
    }
  }

  /// A partition holding the volume and its FUSE mount's attachment (the bridge's, at [`POINT`]).
  fn partition() -> Partition {
    let mut partition = Partition::new(CAPS, 0);
    let volume = volume();
    partition
      .apply(&Op::VolumeCreated {
        record: volume.clone(),
      })
      .unwrap();
    partition
      .apply(&Op::AttachmentAdded {
        record: AttachmentRecord {
          id: MOUNTED,
          volume: volume.id,
          consumer: Consumer::Bridge,
          snapshot: None,
          form: AttachForm::ChosenPath {
            path: POINT.to_owned(),
          },
          principal: OWNER,
          rights: Rights {
            read: true,
            write: true,
            admin: false,
          },
          token: [1; 16],
        },
      })
      .unwrap();
    partition
  }

  fn binding(mount_point: &str, attachment: Option<u64>) -> Binding {
    Binding {
      entry: OciBinding {
        source: mount_point.to_owned(),
        destination: "/work".to_owned(),
        read_only: false,
        mount_type: "bind".to_owned(),
        options: vec![],
        evidence: slates_ipc::protocol::HostMountEvidence {
          fstype: "fuse.slates".to_owned(),
          mount_source: format!("slates:{:016x}", attachment.unwrap_or(0)),
          names_volume: true,
          mount_id: 0,
          mount_device: 0,
        },
      },
      mount_point: mount_point.to_owned(),
      attachment,
    }
  }

  /// AUD-29-64 (the bind's source authority). Do: hold bind recipes to the daemon's records — the mount's
  /// own attachment at its point for its principal; an attachment the table names but no record holds (stale
  /// or foreign); the right attachment at another point; the right one for another principal; and a source
  /// naming no attachment (macOS's loopback form) at the mount's point. Expect: only the live record at its
  /// own point for its own principal binds (as the mount's consumer); every other is refused `Forbidden`; the
  /// form with no attachment named still finds the record by its point.
  #[test]
  fn a_bind_source_binds_only_as_the_live_attachment_its_table_entry_names() {
    let partition = partition();
    let volume = volume().id;
    assert_eq!(
      binding(POINT, Some(MOUNTED)).consumer(&partition, volume, &OWNER),
      Ok(Consumer::Mount {
        attachment: MOUNTED
      })
    );
    for (bad, principal) in [
      (binding(POINT, Some(UNKNOWN)), &OWNER),
      (binding(ELSEWHERE, Some(MOUNTED)), &OWNER),
      (binding(POINT, Some(MOUNTED)), &STRANGER),
    ] {
      assert!(
        matches!(
          bad.consumer(&partition, volume, principal),
          Err(Refusal::Forbidden { .. })
        ),
        "{:?} at {} for {principal:?}",
        bad.attachment,
        bad.mount_point
      );
    }
    assert_eq!(
      binding(POINT, None).consumer(&partition, volume, &OWNER),
      Ok(Consumer::Mount {
        attachment: MOUNTED
      })
    );
  }
}
