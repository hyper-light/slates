//! Establishing (and tearing down) the loopback bridge mount (§4.6): `mount_nfs localhost:PORT` at an
//! existing user-owned directory — no privilege (R10; `noresvport` uses a high source port, so no
//! root), no kernel extension, no Apple entitlement. It is the signing-free path sylk mounts over
//! FUSE-T (itself an NFS loopback under the hood); slates reaches it directly because it serves its
//! own NFS, so it needs no FUSE library at all.
//!
//! The shape is adapted from sylk's cgofuse mount (`core/purevfs`): a **pure, injectable capability
//! detection** ([`classify`]) so the selection tests on any host with no live mount, a **probe** that
//! refuses with a message naming what is missing rather than a raw `mount_nfs` failure, and the
//! **mount/unmount lifecycle** ([`establish`]/[`unmount`]). Where sylk classifies a FUSE backend
//! (macFUSE vs FUSE-T), slates classifies its one mechanism — `mount_nfs`, built into macOS and the
//! BSDs — because its NFS server replaces the FUSE library.

use std::process::Command;

use slates_client::StatusReport;

use crate::Failure;

/// Derived: the attribute-cache timeout (`actimeo`, whole seconds) the mount requests. slates's
/// loopback GETATTR is sub-millisecond, so revalidation is cheap; a short cache keeps the view fresh —
/// an overlay changes under merges and outside edits — while amortizing repeated stats over a burst of
/// identical reads. One second is the finest `mount_nfs`'s whole-second `actimeo` expresses; sylk
/// documents the same trade-off for its 100 ms FUSE attribute timeout (`core/purevfs`).
const ATTR_CACHE_SECONDS: u32 = 1;

/// The loopback mount mechanism a host offers (§4.6), the analogue of sylk's FUSE-backend selection.
/// slates serves its own NFS, so its mechanism is `mount_nfs` — built into macOS and the BSDs, needing
/// no FUSE library, kernel extension, or Apple entitlement.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MountBackend {
  /// The loopback NFS mount, via `mount_nfs`.
  NfsLoopback,
  /// No supported loopback mount on this host (`mount_nfs` is not on the `PATH`).
  Unsupported,
}

/// Classifies the loopback mount mechanism from an injectable "is this command on the `PATH`" probe.
/// Pure — no filesystem or process references — so every branch tests on any host without a live mount,
/// exactly as sylk factors its detection into a predicate-injected `classifyDarwinFUSEBackend`.
fn classify(command_exists: impl Fn(&str) -> bool) -> MountBackend {
  if command_exists("mount_nfs") {
    MountBackend::NfsLoopback
  } else {
    MountBackend::Unsupported
  }
}

/// Whether `name` resolves to an executable on the `PATH` (the analogue of sylk's `commandAvailable`
/// over `exec.LookPath`): a directory of the `PATH` holds an entry `name` the caller may execute.
fn command_on_path(name: &str) -> bool {
  let Some(paths) = std::env::var_os("PATH") else {
    return false;
  };
  std::env::split_paths(&paths)
    .any(|dir| rustix::fs::access(dir.join(name), rustix::fs::Access::EXEC_OK).is_ok())
}

/// The loopback mount mechanism this host offers, probing the real `PATH`.
pub(crate) fn backend() -> MountBackend {
  classify(command_on_path)
}

/// The `mount_nfs` arguments to mount the volume named `name`, served on the daemon's loopback `port`,
/// at `path`: the design's options (§4.6 — NFSv3 over TCP, the explicit `port`/`mountport` so no
/// portmap query is needed, `soft,intr` so a wedged mount is escapable, `locallocks`, `nosuid`,
/// `rdirplus`), plus `noresvport` for the unprivileged mount (R10) and the derived attribute-cache
/// timeout. slates serves NFS and MOUNT on one port, so `port` and `mountport` are the same. The
/// export is `localhost:/<name>`, the volume's provisioned name under the synthetic host root.
fn mount_args(port: u16, name: &str, path: &str) -> Vec<String> {
  let options = format!(
    "vers=3,tcp,port={port},mountport={port},noresvport,soft,intr,locallocks,nosuid,rdirplus,\
     actimeo={ATTR_CACHE_SECONDS}"
  );
  vec![
    "-o".to_owned(),
    options,
    format!("localhost:/{name}"),
    path.to_owned(),
  ]
}

/// Runs `mount_nfs` to mount the volume `report` names at the existing user-owned `path`, returning the
/// mounted path. Refuses first (like sylk's `strictExecutionProbe`, naming what is missing) if the host
/// has no loopback mount mechanism or the daemon is not serving NFS; then a typed failure if `mount_nfs`
/// refuses (a missing mount point, a busy path).
pub(crate) fn establish(report: &StatusReport, path: &str) -> Result<String, Failure> {
  if backend() == MountBackend::Unsupported {
    return Err(Failure::Failed(
      "cannot mount here: `mount_nfs` is not on the PATH — slates's loopback mount needs it (it is \
       built into macOS and the BSDs); no kernel extension, FUSE library, or Apple entitlement is \
       required"
        .to_owned(),
    ));
  }
  let port = report.nfs_port.ok_or_else(|| {
    Failure::Failed(
      "the daemon is not serving NFS (its loopback listener did not bind); cannot mount".to_owned(),
    )
  })?;
  let args = mount_args(port, &report.name, path);
  let status = Command::new("mount_nfs")
    .args(&args)
    .status()
    .map_err(|e| Failure::Failed(format!("running mount_nfs: {e}")))?;
  if !status.success() {
    return Err(Failure::Failed(format!(
      "mount_nfs localhost:/{} {path} failed ({status})",
      report.name
    )));
  }
  Ok(path.to_owned())
}

/// Unmounts the loopback bridge at `path` with `umount`, the lifecycle counterpart of [`establish`]
/// (sylk's cgofuse `Close`). No privilege: a user unmounts a mount they made.
pub(crate) fn unmount(path: &str) -> Result<(), Failure> {
  let status = Command::new("umount")
    .arg(path)
    .status()
    .map_err(|e| Failure::Failed(format!("running umount: {e}")))?;
  if status.success() {
    Ok(())
  } else {
    Err(Failure::Failed(format!(
      "umount {path} failed ({status}); is the path still in use?"
    )))
  }
}

#[cfg(test)]
mod tests {
  use super::{MountBackend, classify, mount_args};

  /// The mount arguments carry the daemon's loopback port for both NFS and MOUNT, the unprivileged
  /// `noresvport`, NFSv3, the derived attribute-cache timeout, and the export named by the volume's
  /// provisioned name at the chosen path — the command `slates mount` runs.
  #[test]
  fn the_mount_arguments_target_the_daemon_port_and_the_named_export() {
    let args = mount_args(54321, "myproject", "/Users/me/mnt");
    assert_eq!(args[0], "-o");
    let options = &args[1];
    assert!(options.contains("port=54321"), "the NFS port: {options}");
    assert!(
      options.contains("mountport=54321"),
      "the MOUNT port, same server: {options}"
    );
    assert!(
      options.contains("noresvport"),
      "the unprivileged mount, R10: {options}"
    );
    assert!(options.contains("vers=3"), "NFSv3: {options}");
    assert!(
      options.contains("actimeo=1"),
      "the derived attribute-cache timeout: {options}"
    );
    assert_eq!(
      args[2], "localhost:/myproject",
      "the export is the volume's provisioned name"
    );
    assert_eq!(args[3], "/Users/me/mnt", "the chosen mount point");
  }

  /// The capability detection is pure: `mount_nfs` present is the NFS loopback backend, its absence is
  /// unsupported — tested with an injected predicate, so it runs on any host with no live mount (the
  /// property sylk's predicate-injected FUSE-backend detection has).
  #[test]
  fn classify_finds_the_nfs_backend_only_when_mount_nfs_is_present() {
    assert_eq!(
      classify(|cmd| cmd == "mount_nfs"),
      MountBackend::NfsLoopback,
      "mount_nfs on the PATH is the NFS loopback backend"
    );
    assert_eq!(
      classify(|_| false),
      MountBackend::Unsupported,
      "no mount_nfs is unsupported"
    );
    assert_eq!(
      classify(|cmd| cmd == "mount"),
      MountBackend::Unsupported,
      "another mount tool is not mount_nfs"
    );
  }
}
