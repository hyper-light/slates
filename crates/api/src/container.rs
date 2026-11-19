//! A volume's host mount bound into a container (§4.6 A-9 "A host OCI runtime passes the established host attachment
//! into the container mount namespace"; §4.4 `attach(…, transport, chosen_path?)`; A-520): the OCI form of `attach` as
//! a verb of the consumer contract (A-299). A host that runs its work in containers (kiln's container engine, a
//! scheduler's OCI class through [`crate::Volumes`]) mounts the volume ([`crate::mount`]), names the mount point and the
//! path the container sees it at, and gets back the runtime-specification `mounts` entry its runtime performs. The
//! daemon verifies, in the kernel's mount table and never by touching the mount, that the path is exactly this volume's
//! mount (on Linux its own mount by its record, A-227), records the binding, and answers the entry
//! (`crates/server/src/oci.rs`, `crates/bridge-oci`). Slates enters no namespace, makes no directory and runs no
//! runtime (R10): the host's runtime binds, and the container's view is the host mount's, as T-4.13 proves by use.
//!
//! **What it answers.** A [`ContainerBind`]: the binding's own attachment, which ends at [`Instance::detach`] or with its
//! source mount, and the entry ([`OciBinding`]): `type: bind` with the non-recursive `bind`, `ro` or `rw` by the intent,
//! `private`, `nosuid` and `nodev`, beside what the kernel's table said of the source. The form's six facts (§4.6 A-9:
//! its target path, policy, sharing, residency and evidence) are the status report's ([`crate::TransportReport`]).
//!
//! **Refused, typed, before any effect:** a volume that does not exist (`NotFound`); a host that offers no mount to bind
//! (`Unsupported::Attachment`: Windows' `HostPlatform`, a host without its mount bridge `HostMountRequired`); a source
//! or a destination that is not absolute, a source that is no mount point, another file system's, one with a mount
//! beneath it, or another volume's where the table's source names the volume (macOS) (`Invalid::ChosenPath`); and a
//! mount this caller did not make of this volume, or a read-only one bound for writing (`Denied::Rights`).
//!
//! **The source's spelling.** The daemon compares the source with the kernel's table, which records a mount at its real
//! path (macOS reaches `/var` through `/private/var`), so the source is spelled so first: its folder's real path with its
//! own name kept ([`real_source`]). Never the mount point resolved whole, which looks into the mount: a request to the
//! very daemon asked, and on a hard NFS mount one that waits on its server. A source with no folder to resolve (relative,
//! the root, or in a folder that does not exist) goes as given, and the daemon's verdict on it is the answer.
//!
//! **Checked again just before the runtime binds** ([`check_source`], [`ContainerBind::check_source`]; AUD-29-66): the
//! kernel's table read now, then the identity of the mount at the source read now (Linux's `STATX_MNT_ID_UNIQUE`, an id
//! the kernel never reuses; macOS's filesystem id) and compared with the one the binding's evidence recorded, never by
//! touching the mount. A mount gone or replaced between the bind and the runtime's use refuses typed ([`SourceFault`]),
//! so the runtime is never handed a replacement, or a bare folder where the mount was, which it would bind, or make.
//! The judgment is pure over the table and the identity it is given, and held on every host by its tests; a host with
//! no table to read (Windows) is told so by the table itself. Moved here from the CLI's `slates oci-check` on
//! 2026-11-19 (A-520), which now calls it.

use std::fmt;
use std::path::Path;

use slates_bridge_oci::mount_table::MountEntry;
use slates_bridge_oci::verify::SourceChange;
use slates_client::Attachment;
use slates_ipc::protocol::{AttachRequest, Established, Intent, OciBinding, VolumeId};

pub use slates_bridge_oci::mount_table::{MountIdentity, MountTableError};

use crate::error::{Error, Missing, ProtocolFault};
use crate::instance::Instance;

/// Format: the verb a daemon's answer that established no binding names, as a protocol fault.
const VERB: &str = "attach (a container bind)";

/// A volume's host mount bound into a container ([`Instance::bind_into_container`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerBind {
  /// The binding's own attachment: [`Instance::detach`] ends it, and it ends with its source mount.
  pub attachment: u64,
  /// The runtime's `mounts` entry, word for word, and what the kernel's table said of its source.
  pub binding: OciBinding,
}

impl Instance {
  /// Binds `volume`'s host mount at `source` into a container at `destination` (§4.6 A-9; A-520), read-only for
  /// [`Intent::Read`]: the daemon verifies `source` is exactly this volume's mount in the kernel's table, records the
  /// binding, and answers the entry the host hands its runtime (see the module's notes). The binding presents the head
  /// the mount presents. Refused typed before any effect (the module's notes).
  pub fn bind_into_container(
    &mut self,
    volume: VolumeId,
    source: &str,
    destination: &str,
    intent: Intent,
  ) -> Result<ContainerBind, Error> {
    let form = AttachRequest::Oci {
      source: real_source(source),
      destination: destination.to_owned(),
    };
    let attachment = self
      .client
      .attach_with(volume, None, intent, form)
      .map_err(|e| self.fail(e, Some(Missing::Volume(volume))))?;
    bound(attachment)
  }
}

/// Why a bind's source is no longer the mount its binding verified ([`check_source`]; AUD-29-66), typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceFault {
  /// No mount is at the source any more: a runtime handed it would bind whatever folder is left there, or make one.
  Missing {
    /// The source.
    source: String,
  },
  /// Another mount is at the source: the mount was unmounted and mounted again, or something was mounted over it.
  Replaced {
    /// The source.
    source: String,
    /// The mount the binding verified.
    verified: MountIdentity,
    /// The mount at the source now.
    found: MountIdentity,
  },
  /// This kernel names no mount by an id it never reuses, so a replacement at the source cannot be told from the mount
  /// verified (Linux added `STATX_MNT_ID_UNIQUE` in 6.8).
  Uncheckable {
    /// The source.
    source: String,
  },
  /// The kernel's mount table could not be read (on a host with none to read, `Unsupported`).
  MountTable(MountTableError),
  /// The identity of the mount at the source could not be read.
  Identity(MountTableError),
}

impl fmt::Display for SourceFault {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Missing { source } => write!(f, "SourceMissing: no mount is at {source} any more"),
      Self::Replaced { source, found, .. } => write!(
        f,
        "SourceReplaced: {source} is now mount {} on device {}, not the mount verified",
        found.mount, found.device
      ),
      Self::Uncheckable { source } => write!(
        f,
        "SourceUncheckable: this kernel does not report a mount id it never reuses, so a replacement at {source} cannot \
         be told from the mount verified (Linux 6.8 added it)"
      ),
      Self::MountTable(e) => write!(f, "reading the mount table: {e}"),
      Self::Identity(e) => write!(f, "reading the mount identity: {e}"),
    }
  }
}

impl ContainerBind {
  /// Whether this binding's source is still the mount it verified, by the identity its evidence recorded
  /// ([`check_source`]): the check a host runs immediately before its runtime binds the source.
  pub fn check_source(&self) -> Result<(), SourceFault> {
    check_source(
      &self.binding.source,
      MountIdentity {
        mount: self.binding.evidence.mount_id,
        device: self.binding.evidence.mount_device,
      },
    )
  }
}

/// Whether `source` is still the mount instance verified as `verified` (AUD-29-66; see the module's notes): the kernel's
/// table read now, and the identity of the mount at `source` read now and compared, never by touching the mount. The
/// table first: a source whose mount is gone has nothing to identify, and is `Missing`, never a failed identity read.
pub fn check_source(source: &str, verified: MountIdentity) -> Result<(), SourceFault> {
  judged(
    source,
    verified,
    slates_bridge_oci::mount_table::mount_table(),
    || slates_bridge_oci::mount_table::mount_identity_at(source),
  )
}

/// The check's judgment of `source` over the kernel's `table`, read already, and the identity at `source`, read by `live`
/// only once the table shows a mount there ([`check_source`]): pure, so its order and each answer are tested on every
/// host.
fn judged(
  source: &str,
  verified: MountIdentity,
  table: Result<Vec<MountEntry>, MountTableError>,
  live: impl FnOnce() -> Result<MountIdentity, MountTableError>,
) -> Result<(), SourceFault> {
  let table = table.map_err(SourceFault::MountTable)?;
  let missing = || SourceFault::Missing {
    source: source.to_owned(),
  };
  if !slates_bridge_oci::verify::mounted_at(&table, source) {
    return Err(missing());
  }
  let live = match live() {
    Ok(live) => live,
    Err(MountTableError::NoUniqueMountId) => {
      return Err(SourceFault::Uncheckable {
        source: source.to_owned(),
      });
    }
    Err(e) => return Err(SourceFault::Identity(e)),
  };
  slates_bridge_oci::verify::source_unchanged(&table, source, verified, live).map_err(|change| {
    match change {
      SourceChange::Missing => missing(),
      SourceChange::Replaced { verified, found } => SourceFault::Replaced {
        source: source.to_owned(),
        verified,
        found,
      },
    }
  })
}

/// The binding a daemon's answer to a container bind established, or the protocol fault of an answer that established
/// none: a binding is never made up from a record.
fn bound(attachment: Attachment) -> Result<ContainerBind, Error> {
  match attachment.established {
    Established::OciBind { binding } => Ok(ContainerBind {
      attachment: attachment.attachment,
      binding: *binding,
    }),
    Established::Record => Err(Error::Protocol(ProtocolFault::UnexpectedReply {
      verb: VERB,
    })),
  }
}

/// `source` spelled as the kernel's table records a mount: its folder's real path with its own name kept, or `source` as
/// given where it has no folder to resolve (see the module's notes). The resolution is a read of the folder alone;
/// where the folder cannot be read there is no other spelling to send, and the daemon judges the path as given.
pub(crate) fn real_source(source: &str) -> String {
  let path = Path::new(source);
  if !path.is_absolute() {
    return source.to_owned();
  }
  match (path.parent(), path.file_name()) {
    (Some(folder), Some(name)) => std::fs::canonicalize(folder).map_or_else(
      |_unresolved| source.to_owned(),
      |real| real.join(name).to_string_lossy().into_owned(),
    ),
    _ => source.to_owned(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use slates_ipc::protocol::{
    AttachTransport, AttachmentCapability, Conformance, DeleteWhileOpen, HostMountEvidence,
    KernelCache, ReadWritePolicy, Residency, SharingSemantics, TargetPathConstraint,
  };

  /// Shape: an attachment id a daemon issued.
  const ATTACHMENT: u64 = 7;

  /// A daemon's answer to an attach, established as `established`.
  fn answered(established: Established) -> Attachment {
    Attachment {
      attachment: ATTACHMENT,
      lease_epoch: None,
      path: None,
      version: None,
      established,
      capability: AttachmentCapability {
        transport: AttachTransport::Oci,
        supported: true,
        unsupported_reason: None,
        target_path: TargetPathConstraint::ContainerDestination,
        read_write: ReadWritePolicy::ReadWrite,
        sharing: SharingSemantics {
          one_owning_shard: true,
          server_open_state: false,
          cache: KernelCache::InheritedFromHostMount,
          delete_while_open: DeleteWhileOpen::Unlinked,
        },
        residency: Residency::DaemonRam,
        conformance: Conformance::None,
      },
      token: None,
    }
  }

  /// A binding as a daemon answers one: the mount at `/mnt/work` bound read-write at `/work`.
  fn binding() -> OciBinding {
    OciBinding {
      source: "/mnt/work".to_owned(),
      destination: "/work".to_owned(),
      read_only: false,
      mount_type: "bind".to_owned(),
      options: ["bind", "rw", "private", "nosuid", "nodev"]
        .map(str::to_owned)
        .to_vec(),
      evidence: HostMountEvidence {
        fstype: "fuse.slates".to_owned(),
        mount_source: "slates:00000000000000ab".to_owned(),
        names_volume: true,
        mount_id: 1,
        mount_device: 2,
        shared_with_other_users: true,
      },
    }
  }

  /// A-520. Do: take a daemon's answer to a container bind that established the binding, and one that established only
  /// a record. Expect the first's binding, word for word, with the attachment it names, and the second a protocol
  /// fault naming the verb: a binding is never made up from a record.
  #[test]
  fn a_bind_answers_the_binding_the_daemon_established_and_nothing_else() {
    let established = answered(Established::OciBind {
      binding: Box::new(binding()),
    });
    assert_eq!(
      bound(established),
      Ok(ContainerBind {
        attachment: ATTACHMENT,
        binding: binding(),
      })
    );
    assert_eq!(
      bound(answered(Established::Record)),
      Err(Error::Protocol(ProtocolFault::UnexpectedReply {
        verb: VERB
      }))
    );
  }

  /// Shape: a name nothing makes, in a folder that exists.
  const NOTHING_MADE: &str = "bind-source-nothing-made";

  /// A-520 (the source's spelling). Do: spell a source named in this crate's own folder, and on macOS one in `/var` (a
  /// link to `/private/var` there), each a name nothing made; then a relative source, the root, and a source in a folder
  /// that does not exist. Expect the first two spelled by their folder's real path with their own name kept (nothing is
  /// made or looked into at the name: a read of the folder alone), and each of the others as given, for the daemon to
  /// judge.
  #[test]
  fn a_source_is_spelled_by_its_folders_real_path_and_its_own_name() {
    let folder = Path::new(env!("CARGO_MANIFEST_DIR"));
    assert_eq!(
      real_source(&folder.join(NOTHING_MADE).to_string_lossy()),
      std::fs::canonicalize(folder)
        .unwrap()
        .join(NOTHING_MADE)
        .to_string_lossy()
    );
    #[cfg(target_os = "macos")]
    assert_eq!(
      real_source(&format!("/var/{NOTHING_MADE}")),
      format!("/private/var/{NOTHING_MADE}")
    );
    assert_eq!(real_source("relative/mount"), "relative/mount");
    assert_eq!(real_source("/"), "/");
    let unresolvable = folder.join("folder-never-made").join("mount");
    let unresolvable = unresolvable.to_string_lossy();
    assert_eq!(real_source(&unresolvable), unresolvable);
  }

  /// Shape: the source the judgment's tables mount at, and two identities, the one verified and another.
  const SOURCE: &str = "/mnt/work";
  /// Shape: the identity a binding's evidence recorded.
  const VERIFIED: MountIdentity = MountIdentity {
    mount: 7,
    device: 9,
  };
  /// Shape: the identity of a mount that replaced it.
  const REPLACEMENT: MountIdentity = MountIdentity {
    mount: 8,
    device: 9,
  };

  /// A kernel table holding one slates mount at `at`, of `identity`.
  fn table_with(at: &str, identity: MountIdentity) -> Vec<MountEntry> {
    vec![MountEntry {
      identity,
      mount_point: at.to_owned(),
      fstype: "fuse.slates".to_owned(),
      source: "slates:00000000000000ab".to_owned(),
    }]
  }

  /// AUD-29-66, A-520 (moved from the CLI's `oci-check`). Do: judge a source over a table that could not be read, a
  /// table with no mount at the source, a mount whose identity this kernel cannot read uniquely, one whose identity read
  /// is refused otherwise, a mount replaced at the source, and the mount verified (named with a trailing separator),
  /// counting the identity reads. Expect `MountTable` and `Missing` with no identity read, then `Uncheckable`,
  /// `Identity`, `Replaced` naming both mounts, and the mount verified passed: never a replacement or a bare folder
  /// judged unchanged.
  #[test]
  fn a_source_is_judged_by_the_table_first_then_by_the_identity_the_kernel_never_reuses() {
    let reads = std::cell::Cell::new(0_u32);
    let identity = |found: MountIdentity| {
      let reads = &reads;
      move || -> Result<MountIdentity, MountTableError> {
        reads.set(reads.get() + 1);
        Ok(found)
      }
    };
    let refused = MountTableError::Query { errno: 13 };
    assert_eq!(
      judged(SOURCE, VERIFIED, Err(refused.clone()), identity(VERIFIED)),
      Err(SourceFault::MountTable(refused))
    );
    assert_eq!(
      judged(
        SOURCE,
        VERIFIED,
        Ok(table_with("/mnt/other", VERIFIED)),
        identity(VERIFIED)
      ),
      Err(SourceFault::Missing {
        source: SOURCE.to_owned()
      })
    );
    assert_eq!(
      reads.get(),
      0,
      "no identity is read until the table shows a mount"
    );
    assert_eq!(
      judged(SOURCE, VERIFIED, Ok(table_with(SOURCE, VERIFIED)), || Err(
        MountTableError::NoUniqueMountId
      )),
      Err(SourceFault::Uncheckable {
        source: SOURCE.to_owned()
      })
    );
    let unreadable = MountTableError::Query { errno: 2 };
    assert_eq!(
      judged(SOURCE, VERIFIED, Ok(table_with(SOURCE, VERIFIED)), || Err(
        unreadable.clone()
      )),
      Err(SourceFault::Identity(unreadable))
    );
    assert_eq!(
      judged(
        SOURCE,
        VERIFIED,
        Ok(table_with(SOURCE, REPLACEMENT)),
        identity(REPLACEMENT)
      ),
      Err(SourceFault::Replaced {
        source: SOURCE.to_owned(),
        verified: VERIFIED,
        found: REPLACEMENT
      })
    );
    assert_eq!(
      judged(
        &format!("{SOURCE}/"),
        VERIFIED,
        Ok(table_with(SOURCE, VERIFIED)),
        identity(VERIFIED)
      ),
      Ok(())
    );
    assert_eq!(reads.get(), 2, "the identity is read once a mount is there");
  }

  /// AUD-29-66, A-520 (the host's own table, read and nothing touched). Do: check the root, which every Unix host mounts,
  /// against its identity read now and against another; check a path no mount is at; on a host with no table, check its
  /// drive. Expect the root unchanged, the other identity `Replaced` naming both, the path `Missing` (on a Linux kernel
  /// that names no mount uniquely, the root `Uncheckable`), and a host with no table refused by the table,
  /// `MountTable(Unsupported)`: never a source judged unchanged where nothing was checked.
  #[test]
  fn a_source_is_checked_against_the_hosts_own_table() {
    #[cfg(unix)]
    {
      let nowhere = format!("/{NOTHING_MADE}");
      assert_eq!(
        check_source(&nowhere, MountIdentity::default()),
        Err(SourceFault::Missing {
          source: nowhere.clone()
        })
      );
      match slates_bridge_oci::mount_table::mount_identity_at("/") {
        Err(MountTableError::NoUniqueMountId) => assert_eq!(
          check_source("/", MountIdentity::default()),
          Err(SourceFault::Uncheckable {
            source: "/".to_owned()
          })
        ),
        Ok(root) => {
          assert_eq!(check_source("/", root), Ok(()));
          let other = MountIdentity {
            mount: root.mount ^ 1,
            ..root
          };
          assert_eq!(
            check_source("/", other),
            Err(SourceFault::Replaced {
              source: "/".to_owned(),
              verified: other,
              found: root
            })
          );
        }
        Err(e) => panic!("the root's identity: {e}"),
      }
    }
    #[cfg(not(unix))]
    assert!(matches!(
      check_source("C:\\", MountIdentity::default()),
      Err(SourceFault::MountTable(MountTableError::Unsupported { .. }))
    ));
  }
}
