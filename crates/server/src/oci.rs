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
  /// bind cannot promise rights the source does not carry. The parent must present the version the bind asks
  /// for — the head (`None`) or exactly `snapshot` (AUD-29-76) — so a recorded bind never claims one while the
  /// container sees the other.
  pub(crate) fn consumer(
    &self,
    partition: &slates_db::partition::Partition,
    volume: slates_db::catalog::VolumeId,
    principal: &slates_db::catalog::Principal,
    snapshot: Option<slates_db::catalog::SnapshotId>,
  ) -> Result<(slates_db::catalog::Consumer, bool), Refusal> {
    use slates_db::catalog::Consumer;
    let parent = partition
      .attachments_of(volume)
      .into_iter()
      .find(|parent| {
        matches!(parent.consumer, Consumer::Bridge)
          && self.attachment.is_none_or(|named| parent.id == named)
          && parent.principal == *principal
          && parent.snapshot == snapshot
          && parent.form.mount_point() == Some(self.mount_point.as_str())
          && parent.rights.read
          && (self.entry.read_only || parent.rights.write)
      })
      .ok_or(Refusal::Forbidden {
        verb: "OCI source mount".to_owned(),
      })?;
    // **Reported, not refused.** A rootful engine's container processes reach the source as ids other than
    // the mounting user's, so a FUSE mount without `allow_other` answers them `EACCES` — standard FUSE
    // behaviour. A **rootless** engine maps container-root to the mounting uid and reads it either way,
    // measured 2026-10-19: a rootless podman container read and wrote a slates volume through an unshared
    // FUSE mount, was refused `EROFS` on a read-only bind, and wrote through a writable one
    // (`a_rootless_container_reads_and_writes_the_volume_through_its_mount`).
    //
    // slates cannot see which runtime the harness will drive, and §4.6 A-9 puts that work on the harness —
    // "slates does the namespace work of neither the runtime nor the launcher", and "a metadata record is
    // insufficient evidence of a usable container path", which is why `oci-check` exists for the harness to
    // run immediately before its runtime binds. So the fact travels in the evidence
    // (`HostMountEvidence::shared_with_other_users`) and the harness decides.
    //
    // Until 2026-10-20 this **refused** an unshared source outright, on reasoning that is true of a rootful
    // engine and false of a rootless one. That blocked the unprivileged case R10 prefers and effectively
    // demanded a root edit of `/etc/fuse.conf` for this form — a privilege requirement in a product whose
    // rule is that none is ever needed. Refusing a configuration that demonstrably works is not the safe
    // direction once you know it works.
    let shared = parent.form.shared_with_other_users();
    Ok((
      Consumer::Mount {
        attachment: parent.id,
      },
      shared,
    ))
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
  use slates_bridge_oci::mount_table::MountIdentity;
  use slates_bridge_oci::mount_table::{mount_identity_at, mount_table};
  use slates_bridge_oci::verify::{
    HostPathRefusal, VerifiedHostMount, expected_mount, host_mount_kind, verify_host_mount,
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
  let verified = verify_host_mount(
    &entries,
    source,
    &expected_mount(kind, volume_name),
    MountIdentity::default(),
  )
  .map_err(|refusal| {
    chosen(match refusal {
      HostPathRefusal::NotAbsolute => HostPathReason::NotAbsolute,
      HostPathRefusal::NotAMountPoint => HostPathReason::NotAMountPoint,
      HostPathRefusal::ForeignFilesystem { fstype } => HostPathReason::ForeignFilesystem { fstype },
      HostPathRefusal::NotThisVolume { source } => HostPathReason::NotThisVolume { source },
      HostPathRefusal::DescendantMount { mount_point } => {
        HostPathReason::DescendantMount { mount_point }
      }
    })
  })?;
  // Only now, for a path the table has already vouched for: the identity recorded as evidence is the one the
  // kernel never reuses, read of this path alone. The table's own mount id is reused, so a mount swapped in at
  // the same path reproduces it and the later check passes a replacement
  // (`docs/bugs/2026-10-17-oci-check-accepts-a-replaced-mount.md`). Asking after the pure checks keeps each
  // refusal the precise one — a path that is no mount point is `NotAMountPoint`, not a failed identity read —
  // and a kernel that cannot name a mount uniquely refuses rather than record evidence no check can use.
  let identity = mount_identity_at(source)
    .map_err(|e| chosen(HostPathReason::MountTableUnavailable { errno: e.errno() }))?;
  let verified = VerifiedHostMount {
    identity,
    ..verified
  };
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
        // Stamped by the caller once `consumer` has resolved the parent attachment, which is what records
        // whether the mount was made `--shared`; the mount table alone does not say.
        shared_with_other_users: false,
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
  use proptest::prelude::*;
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

  /// A partition holding the volume and its FUSE mount's attachment (the bridge's, at [`POINT`]), shared with
  /// other users as a mount a container binds must be (`slates mount --shared`; AUD-29-67).
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
          form: AttachForm::SharedFuseMount {
            path: POINT.to_owned(),
            scope: None,
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
          shared_with_other_users: false,
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
      binding(POINT, Some(MOUNTED)).consumer(&partition, volume, &OWNER, None),
      Ok((
        Consumer::Mount {
          attachment: MOUNTED
        },
        true
      ))
    );
    for (bad, principal) in [
      (binding(POINT, Some(UNKNOWN)), &OWNER),
      (binding(ELSEWHERE, Some(MOUNTED)), &OWNER),
      (binding(POINT, Some(MOUNTED)), &STRANGER),
    ] {
      assert!(
        matches!(
          bad.consumer(&partition, volume, principal, None),
          Err(Refusal::Forbidden { .. })
        ),
        "{:?} at {} for {principal:?}",
        bad.attachment,
        bad.mount_point
      );
    }
    assert_eq!(
      binding(POINT, None).consumer(&partition, volume, &OWNER, None),
      Ok((
        Consumer::Mount {
          attachment: MOUNTED
        },
        true
      ))
    );
  }

  /// AUD-29-67 (a container reaches a Linux source as its own ids), as it stands since 2026-10-20. Do: record
  /// the FUSE mount at [`POINT`] as one the daemon made for its own user (not shared), and bind it by its
  /// attachment. Expect: the bind is **allowed and reports `shared_with_other_users: false`** — a rootful
  /// runtime's processes would meet `EACCES` there and a rootless one would not, and slates cannot see which
  /// the harness will drive, so the fact travels in the evidence for the harness's own pre-bind check
  /// (§4.6 A-9; `oci-check`).
  ///
  /// This **replaces** a refusal. Until 2026-10-20 an unshared source was refused `MountNotShared`, which
  /// blocked the unprivileged case R10 prefers — proven by a rootless podman container reading and writing a
  /// slates volume through exactly such a mount — and effectively demanded a root edit of `/etc/fuse.conf`.
  #[test]
  fn a_bind_of_a_mount_not_shared_with_other_users_reports_it_rather_than_refusing() {
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
    assert_eq!(
      binding(POINT, Some(MOUNTED)).consumer(&partition, volume.id, &OWNER, None),
      Ok((
        Consumer::Mount {
          attachment: MOUNTED
        },
        false
      )),
      "an unshared source binds, and says it is unshared, instead of being refused"
    );
  }

  /// AUD-29-76 (a bind of a snapshot mount). Do: beside the head's mount, record a mount of snapshot 7 at
  /// another point; bind each point asking for the head and for snapshot 7. Expect: a bind binds only to a
  /// mount presenting what it asks for — the head's point for the head, the snapshot's point for snapshot 7 —
  /// and is refused `Forbidden` across them, so a recorded bind never claims the head while the container
  /// sees a snapshot, or the reverse.
  #[test]
  fn a_bind_binds_only_to_a_mount_presenting_the_version_it_asks_for() {
    let mut partition = partition();
    let volume = volume().id;
    let pinned = SnapshotId { value: 7 };
    partition
      .apply(&Op::AttachmentAdded {
        record: AttachmentRecord {
          id: UNKNOWN,
          volume,
          consumer: Consumer::Bridge,
          snapshot: Some(pinned),
          form: AttachForm::ChosenPath {
            path: ELSEWHERE.to_owned(),
          },
          principal: OWNER,
          rights: Rights {
            read: true,
            write: false,
            admin: false,
          },
          token: [2; 16],
        },
      })
      .unwrap();
    let mut read_only = binding(ELSEWHERE, None);
    read_only.entry.read_only = true;
    assert_eq!(
      read_only.consumer(&partition, volume, &OWNER, Some(pinned)),
      Ok((
        Consumer::Mount {
          attachment: UNKNOWN
        },
        false
      ))
    );
    assert!(matches!(
      read_only.consumer(&partition, volume, &OWNER, None),
      Err(Refusal::Forbidden { .. })
    ));
    assert!(matches!(
      binding(POINT, None).consumer(&partition, volume, &OWNER, Some(pinned)),
      Err(Refusal::Forbidden { .. })
    ));
  }
  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig {
      cases: slates_test_seeds::cases(if cfg!(miri) { 4 } else { 200 }),
      ..ProptestConfig::default()
    }))]

    /// §4.6 A-9 under chaos, over the reporting behaviour that replaced the `MountNotShared` refusal on
    /// 2026-10-20. The hand-written attacks beside this drive four shapes; this drives the whole space of
    /// them at once — a mount that is shared or not, under this principal or another, at this path or
    /// elsewhere, named by the right attachment or a wrong one, for the head or a snapshot.
    ///
    /// Expect, for every generated case:
    ///
    /// - **the reported sharing is never invented.** When the bind is allowed, the `bool` it returns equals
    ///   the parent attachment record's own `form.shared_with_other_users()`. This is the property the
    ///   removed refusal used to stand in for, and the one a rootful harness now relies on: a wrong answer
    ///   here sends a container at a mount it cannot read, or worse, says "shared" of one that is not.
    /// - **removing the refusal widened nothing else.** A bind is allowed only when the parent is the
    ///   bridge's attachment of *this* volume, for *this* principal, at *this* mount point, presenting the
    ///   version asked for, carrying the rights the bind needs — the oracle below re-derives that from the
    ///   record and must agree with the code on every case, allow or refuse.
    #[test]
    fn the_reported_sharing_always_matches_the_record_and_nothing_else_widened(
      shared in any::<bool>(),
      other_principal in any::<bool>(),
      elsewhere in any::<bool>(),
      name_wrong_attachment in any::<bool>(),
      read_only in any::<bool>(),
      write_rights in any::<bool>(),
      ask_snapshot in any::<bool>(),
      record_snapshot in any::<bool>(),
    ) {
      let mut partition = Partition::new(CAPS, 0);
      let volume = volume();
      partition
        .apply(&Op::VolumeCreated { record: volume.clone() })
        .unwrap();
      let form = if shared {
        AttachForm::SharedFuseMount { path: POINT.to_owned(), scope: None }
      } else {
        AttachForm::FuseMount { path: POINT.to_owned() }
      };
      let principal = if other_principal { Principal::Uid { uid: 1001 } } else { OWNER };
      let snapshot = record_snapshot.then_some(SnapshotId { value: 1 });
      partition
        .apply(&Op::AttachmentAdded {
          record: AttachmentRecord {
            id: MOUNTED,
            volume: volume.id,
            consumer: Consumer::Bridge,
            snapshot,
            form,
            principal,
            rights: Rights { read: true, write: write_rights, admin: false },
            token: [1; 16],
          },
        })
        .unwrap();

      let at = if elsewhere { ELSEWHERE } else { POINT };
      let named = if name_wrong_attachment { Some(UNKNOWN) } else { Some(MOUNTED) };
      let asked = ask_snapshot.then_some(SnapshotId { value: 1 });
      let mut binding = binding(at, named);
      binding.entry.read_only = read_only;
      let got = binding.consumer(&partition, volume.id, &OWNER, asked);

      // The oracle: what the record says the answer must be, derived here rather than copied from the code.
      let allowed = !elsewhere
        && !name_wrong_attachment
        && !other_principal
        && asked == snapshot
        && (read_only || write_rights);
      if allowed {
        prop_assert_eq!(
          got,
          Ok((Consumer::Mount { attachment: MOUNTED }, shared)),
          "an allowed bind reports exactly the record's own sharing"
        );
      } else {
        prop_assert!(
          got.is_err(),
          "a bind the record does not cover must refuse, however the sharing reads: {got:?}"
        );
      }
    }
  }
}
