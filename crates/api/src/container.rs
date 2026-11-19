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

use std::path::Path;

use slates_client::Attachment;
use slates_ipc::protocol::{AttachRequest, Established, Intent, OciBinding, VolumeId};

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
}
