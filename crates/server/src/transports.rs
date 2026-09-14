//! The transport capability report (§4.6 A-9: "Capabilities differ by host, kernel, runtime and VMM
//! and must be reported by `attach` and `status`: supported transport, target-path constraints,
//! read/write policy, sharing/cache semantics, residency boundary and conformance evidence. Requesting
//! an unsupported form returns `AttachmentUnsupported{transport, reason}`"; Appendix C: "Native host
//! mount, OCI namespace handoff and guest virtio-fs support must each report their own tested
//! semantics"; RQ-20, RQ-24).
//!
//! Every fact in the report is one of three things: read from the machine when the report is made
//! (the kernel's `uname`; whether the daemon's loopback listener bound; which OCI runtime the daemon's
//! `PATH` holds), decided by the platform at one seam ([`Platform`] — the table is pure over it and its
//! unit tests run every branch on every host, the shape of vorpal's `mem/src/policy.rs`), or a
//! statement of what this tree holds for the transport (which bridge the daemon wires, which by-use
//! test exists). Nothing is guessed: a transport the daemon cannot establish is listed with its typed
//! reason, never omitted and never claimed (AC-9.7: "A skipped lane or pure simulation cannot close its
//! transport guarantee").
//!
//! Transport by transport, as of 2026-09-14: the record form under the root mount is served on every
//! host (`crates/server/tests/daemon.rs` drives attach, lease and detach over the ring); the NFS
//! loopback mount is the macOS host mount (`slates mount` runs `mount_nfs` with no privilege;
//! `crates/cli/tests/cli.rs` mounts a real kernel mount) and needs a privilege on Linux that slates
//! never asks for (the kernel refuses an NFS mount in an unprivileged user namespace: `nfs` lacks
//! `FS_USERNS_MOUNT`); the FUSE, FSKit and WinFsp bridges exist as crates the daemon does not serve
//! yet; the container bind is not established by the daemon yet. The guest transports are §4.6's
//! virtio-fs device (`crate::virtiofs`), reported in-process until wired here.

use slates_db::catalog::Rights;
use slates_ipc::protocol::{
  AttachTransport, AttachmentCapability, Conformance, KernelCache, OciRuntime, ReadWritePolicy,
  Residency, SharingSemantics, TargetPathConstraint, TransportReport, UnsupportedReason,
};

use crate::state::ShardState;

/// The host platform: the one `cfg` seam of this module. The table below is pure over it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Platform {
  /// macOS: the NFS loopback mount is the host mount; FSKit is the bridge the daemon does not wire yet.
  MacOs,
  /// Linux: the FUSE bridge is the host mount the daemon does not wire yet; NFS needs a privilege.
  Linux,
  /// Windows: WinFsp is the bridge the daemon does not wire yet.
  Windows,
  /// Anything else: no host mount.
  Other,
}

/// This build's platform.
pub(crate) const fn platform() -> Platform {
  if cfg!(target_os = "macos") {
    Platform::MacOs
  } else if cfg!(target_os = "linux") {
    Platform::Linux
  } else if cfg!(windows) {
    Platform::Windows
  } else {
    Platform::Other
  }
}

/// What a report is computed from: the platform, the daemon's live facts, and the caller's rights.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Situation {
  /// The platform.
  pub(crate) platform: Platform,
  /// Whether the daemon's NFS loopback listener bound (§4.6): whether an export exists to mount.
  pub(crate) nfs_listener_bound: bool,
  /// Whether the caller may write the volume: the policy's ceiling (a read intent narrows it).
  pub(crate) may_write: bool,
}

/// The situation of this daemon, on this host, for a caller with `rights` on a volume.
pub(crate) fn situation(_state: &ShardState, rights: &Rights) -> Situation {
  Situation {
    platform: platform(),
    nfs_listener_bound: crate::daemon::NFS_PORT.load(std::sync::atomic::Ordering::Acquire) != 0,
    may_write: rights.write,
  }
}

fn read_write(may_write: bool) -> ReadWritePolicy {
  if may_write {
    ReadWritePolicy::ReadWrite
  } else {
    ReadWritePolicy::ReadOnly
  }
}

/// Every transport shares the one owning shard's view (D-7).
const fn sharing(server_open_state: bool, cache: KernelCache) -> SharingSemantics {
  SharingSemantics {
    one_owning_shard: true,
    server_open_state,
    cache,
  }
}

/// An offered transport: supported, with its evidence.
fn offered(
  transport: AttachTransport,
  situation: &Situation,
  target_path: TargetPathConstraint,
  sharing: SharingSemantics,
  residency: Residency,
  conformance: Conformance,
) -> AttachmentCapability {
  AttachmentCapability {
    transport,
    supported: true,
    unsupported_reason: None,
    target_path,
    read_write: read_write(situation.may_write),
    sharing,
    residency,
    conformance,
  }
}

/// A refused transport: its reason, the constraints it would have, and no evidence claimed.
fn refused(
  transport: AttachTransport,
  situation: &Situation,
  reason: UnsupportedReason,
  target_path: TargetPathConstraint,
  sharing: SharingSemantics,
  residency: Residency,
) -> AttachmentCapability {
  AttachmentCapability {
    transport,
    supported: false,
    unsupported_reason: Some(reason),
    target_path,
    read_write: read_write(situation.may_write),
    sharing,
    residency,
    conformance: Conformance::None,
  }
}

/// The record form under the root mount (§4.4 `AttachForm::Root`): always establishable; no kernel
/// client of its own, so nothing is cached and the bytes stay in the daemon's RAM.
pub(crate) fn root(situation: &Situation) -> AttachmentCapability {
  offered(
    AttachTransport::Root,
    situation,
    TargetPathConstraint::RootMount,
    sharing(false, KernelCache::NotEstablished),
    Residency::DaemonRam,
    Conformance::VerbLifecycleTest,
  )
}

/// The NFS loopback mount (§4.6 "macOS fallback"): the daemon's export, mounted by `slates mount`
/// with `mount_nfs` at a user-owned directory and no privilege on macOS, exactly when the listener
/// bound. Refused elsewhere: the Linux kernel refuses `nfs` in an unprivileged user namespace and
/// slates never asks for the privilege (R10); Windows has no `mount_nfs`.
fn nfs_loopback(situation: &Situation) -> AttachmentCapability {
  let sharing = sharing(false, KernelCache::ClientTimeouts);
  let residency = Residency::DaemonRamAndKernelCache;
  let target = TargetPathConstraint::UserOwnedExistingDirectory;
  match situation.platform {
    Platform::MacOs if situation.nfs_listener_bound => offered(
      AttachTransport::NfsLoopback,
      situation,
      target,
      sharing,
      residency,
      Conformance::LiveKernelMountTest,
    ),
    Platform::MacOs => refused(
      AttachTransport::NfsLoopback,
      situation,
      UnsupportedReason::ListenerNotBound,
      target,
      sharing,
      residency,
    ),
    Platform::Linux => refused(
      AttachTransport::NfsLoopback,
      situation,
      UnsupportedReason::MountNeedsPrivilege,
      target,
      sharing,
      residency,
    ),
    Platform::Windows | Platform::Other => refused(
      AttachTransport::NfsLoopback,
      situation,
      UnsupportedReason::HostPlatform,
      target,
      sharing,
      residency,
    ),
  }
}

/// A bridge that exists as a crate on its platform but is not served by the daemon: refused
/// `BridgeNotWired` there, `HostPlatform` everywhere else.
fn unwired_bridge(
  transport: AttachTransport,
  situation: &Situation,
  home: Platform,
  target_path: TargetPathConstraint,
  sharing: SharingSemantics,
) -> AttachmentCapability {
  let reason = if situation.platform == home {
    UnsupportedReason::BridgeNotWired
  } else {
    UnsupportedReason::HostPlatform
  };
  refused(
    transport,
    situation,
    reason,
    target_path,
    sharing,
    Residency::DaemonRamAndKernelCache,
  )
}

/// The Linux `/dev/fuse` mount (§4.6 "Linux"): the codec, dispatch and `fusermount3` launcher exist
/// (`crates/bridge-fuse`); the daemon does not serve a mount over them yet.
fn fuse(situation: &Situation) -> AttachmentCapability {
  unwired_bridge(
    AttachTransport::Fuse,
    situation,
    Platform::Linux,
    TargetPathConstraint::UserOwnedExistingDirectory,
    sharing(true, KernelCache::NotEstablished),
  )
}

/// The macOS FSKit module (§4.6 "macOS 26+"): the handler and codec exist (`crates/bridge-fskit`);
/// the app-group ring and the mount session are not wired to the daemon.
fn fskit(situation: &Situation) -> AttachmentCapability {
  unwired_bridge(
    AttachTransport::Fskit,
    situation,
    Platform::MacOs,
    TargetPathConstraint::UserOwnedExistingDirectory,
    sharing(true, KernelCache::NotEstablished),
  )
}

/// The Windows WinFsp volume (§4.6 "Windows"): the binding exists (`crates/bridge-winfsp`); the
/// daemon does not serve it.
fn winfsp(situation: &Situation) -> AttachmentCapability {
  unwired_bridge(
    AttachTransport::WinFsp,
    situation,
    Platform::Windows,
    TargetPathConstraint::DriveLetter,
    sharing(true, KernelCache::NotEstablished),
  )
}

/// Where a container's bytes can reside: on macOS every OCI runtime hosts its containers in a Linux
/// VM whose page cache sits over the runtime's share of the host path; on Linux the bind shares the
/// host kernel's cache; elsewhere there is no bind.
const fn container_residency(platform: Platform) -> Residency {
  match platform {
    Platform::MacOs => Residency::DaemonRamKernelCacheAndRuntimeVm,
    Platform::Linux => Residency::DaemonRamAndKernelCache,
    Platform::Windows | Platform::Other => Residency::DaemonRam,
  }
}

/// The container bind (§4.6 A-9): a bind of an established host mount into a container namespace by
/// the host's OCI runtime. Not established by the daemon yet: refused `BridgeNotWired` with the
/// constraints it will have — the container's view is the host mount's.
fn oci(situation: &Situation) -> AttachmentCapability {
  refused(
    AttachTransport::Oci,
    situation,
    UnsupportedReason::BridgeNotWired,
    TargetPathConstraint::ContainerDestination,
    sharing(false, KernelCache::InheritedFromHostMount),
    container_residency(situation.platform),
  )
}

/// Every transport, in a fixed order.
pub(crate) fn capabilities(situation: &Situation) -> Vec<AttachmentCapability> {
  vec![
    root(situation),
    nfs_loopback(situation),
    fuse(situation),
    fskit(situation),
    winfsp(situation),
    oci(situation),
  ]
}

/// Format: the OCI runtimes and the CLIs that drive one, in the order the report prefers them.
const OCI_RUNTIMES: [&str; 6] = ["runc", "crun", "youki", "docker", "podman", "nerdctl"];

/// The first OCI runtime `command_exists` finds, in the preferred order. Pure over the injected
/// predicate (the shape of `crates/cli/src/mount.rs::classify`), so every branch tests on every host.
pub(crate) fn classify_oci_runtime(command_exists: impl Fn(&str) -> bool) -> OciRuntime {
  OCI_RUNTIMES
    .iter()
    .find(|command| command_exists(command))
    .map_or(OciRuntime::NoneOnPath, |command| OciRuntime::Found {
      name: (*command).to_owned(),
    })
}

/// Whether `name` resolves to an executable on the daemon's `PATH`: a directory of the `PATH` holds an
/// entry `name` this process may execute (a query, never a write).
#[cfg(unix)]
fn command_on_path(name: &str) -> bool {
  let Some(paths) = std::env::var_os("PATH") else {
    return false;
  };
  std::env::split_paths(&paths)
    .any(|dir| rustix::fs::access(dir.join(name), rustix::fs::Access::EXEC_OK).is_ok())
}

/// The OCI runtime on this daemon's `PATH`.
#[cfg(unix)]
fn probe_oci_runtime() -> OciRuntime {
  classify_oci_runtime(command_on_path)
}

/// The Windows `PATH` probe (`PATHEXT` resolution) is not built; the report says so rather than
/// reporting "none found".
#[cfg(not(unix))]
fn probe_oci_runtime() -> OciRuntime {
  OciRuntime::NotProbed
}

/// The host facts as the kernel states them: `uname`'s system name and release.
#[cfg(unix)]
fn host_facts() -> (String, Option<String>) {
  let uts = rustix::system::uname();
  (
    uts.sysname().to_string_lossy().into_owned(),
    Some(uts.release().to_string_lossy().into_owned()),
  )
}

/// Windows has no `uname`: the standard library's OS name, and no kernel release claimed.
#[cfg(not(unix))]
fn host_facts() -> (String, Option<String>) {
  (std::env::consts::OS.to_owned(), None)
}

/// The report for `situation`: the host facts read now, then every transport.
pub(crate) fn report(situation: &Situation) -> TransportReport {
  let (os, kernel) = host_facts();
  TransportReport {
    os,
    kernel,
    oci_runtime: probe_oci_runtime(),
    capabilities: capabilities(situation),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn on(platform: Platform, nfs_listener_bound: bool) -> Situation {
    Situation {
      platform,
      nfs_listener_bound,
      may_write: true,
    }
  }

  fn entry(situation: &Situation, transport: AttachTransport) -> AttachmentCapability {
    capabilities(situation)
      .into_iter()
      .find(|c| c.transport == transport)
      .unwrap_or_else(|| panic!("{transport:?} is reported"))
  }

  /// Every entry on every platform is supported exactly when it carries no reason, claims evidence only
  /// when supported, and shares the one owning shard's view (D-7).
  #[test]
  fn every_entry_is_supported_exactly_when_it_carries_no_reason() {
    for platform in [
      Platform::MacOs,
      Platform::Linux,
      Platform::Windows,
      Platform::Other,
    ] {
      for bound in [false, true] {
        for capability in capabilities(&on(platform, bound)) {
          assert_eq!(
            capability.supported,
            capability.unsupported_reason.is_none(),
            "{platform:?} bound={bound}: {capability:?}"
          );
          assert!(
            capability.supported || capability.conformance == Conformance::None,
            "no evidence claimed for a refused transport: {capability:?}"
          );
          assert!(capability.sharing.one_owning_shard);
        }
      }
    }
  }

  /// macOS: the NFS loopback mount is offered exactly when the listener bound, with its live-mount
  /// evidence; the FSKit bridge is not wired; FUSE and WinFsp are other platforms'.
  #[test]
  fn macos_offers_the_nfs_loopback_mount_when_the_listener_bound() {
    let bound = on(Platform::MacOs, true);
    let nfs = entry(&bound, AttachTransport::NfsLoopback);
    assert!(nfs.supported);
    assert_eq!(nfs.conformance, Conformance::LiveKernelMountTest);
    assert_eq!(nfs.sharing.cache, KernelCache::ClientTimeouts);
    assert_eq!(
      entry(&on(Platform::MacOs, false), AttachTransport::NfsLoopback).unsupported_reason,
      Some(UnsupportedReason::ListenerNotBound)
    );
    assert_eq!(
      entry(&bound, AttachTransport::Fskit).unsupported_reason,
      Some(UnsupportedReason::BridgeNotWired)
    );
    assert_eq!(
      entry(&bound, AttachTransport::Fuse).unsupported_reason,
      Some(UnsupportedReason::HostPlatform)
    );
    assert_eq!(
      entry(&bound, AttachTransport::WinFsp).unsupported_reason,
      Some(UnsupportedReason::HostPlatform)
    );
    assert_eq!(
      entry(&bound, AttachTransport::Oci).residency,
      Residency::DaemonRamKernelCacheAndRuntimeVm
    );
  }

  /// Linux: an NFS mount needs a privilege slates never asks for (R10), whether or not the listener
  /// bound; the FUSE bridge is not wired; a container shares the host kernel's cache.
  #[test]
  fn linux_refuses_nfs_for_the_privilege_and_names_fuse_as_unwired() {
    for bound in [false, true] {
      let situation = on(Platform::Linux, bound);
      assert_eq!(
        entry(&situation, AttachTransport::NfsLoopback).unsupported_reason,
        Some(UnsupportedReason::MountNeedsPrivilege)
      );
      assert_eq!(
        entry(&situation, AttachTransport::Fuse).unsupported_reason,
        Some(UnsupportedReason::BridgeNotWired)
      );
      assert_eq!(
        entry(&situation, AttachTransport::Oci).residency,
        Residency::DaemonRamAndKernelCache
      );
    }
  }

  /// Windows: WinFsp is the unwired bridge on a drive letter; the Unix mounts do not exist there.
  #[test]
  fn windows_names_winfsp_as_unwired_and_the_unix_mounts_as_other_platforms() {
    let situation = on(Platform::Windows, true);
    let winfsp = entry(&situation, AttachTransport::WinFsp);
    assert_eq!(
      winfsp.unsupported_reason,
      Some(UnsupportedReason::BridgeNotWired)
    );
    assert_eq!(winfsp.target_path, TargetPathConstraint::DriveLetter);
    for other in [
      AttachTransport::NfsLoopback,
      AttachTransport::Fuse,
      AttachTransport::Fskit,
    ] {
      assert_eq!(
        entry(&situation, other).unsupported_reason,
        Some(UnsupportedReason::HostPlatform),
        "{other:?}"
      );
    }
  }

  /// The read/write policy is the caller's ceiling: a reader sees read-only everywhere.
  #[test]
  fn a_reader_is_reported_read_only_on_every_transport() {
    let reader = Situation {
      may_write: false,
      ..on(Platform::MacOs, true)
    };
    for capability in capabilities(&reader) {
      assert_eq!(capability.read_write, ReadWritePolicy::ReadOnly);
    }
    for capability in capabilities(&on(Platform::MacOs, true)) {
      assert_eq!(capability.read_write, ReadWritePolicy::ReadWrite);
    }
  }

  /// The runtime probe prefers a bare runtime over a CLI that drives one, and reports an empty
  /// `PATH` as none found — tested with an injected predicate, no `PATH` consulted.
  #[test]
  fn the_runtime_probe_prefers_runc_and_reports_none_typed() {
    assert_eq!(
      classify_oci_runtime(|command| command == "docker" || command == "runc"),
      OciRuntime::Found {
        name: "runc".to_owned()
      }
    );
    assert_eq!(
      classify_oci_runtime(|command| command == "docker"),
      OciRuntime::Found {
        name: "docker".to_owned()
      }
    );
    assert_eq!(classify_oci_runtime(|_| false), OciRuntime::NoneOnPath);
    assert_eq!(
      classify_oci_runtime(|command| command == "mount_nfs"),
      OciRuntime::NoneOnPath
    );
  }
}
