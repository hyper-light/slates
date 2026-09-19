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

/// Derived: the attribute-cache timeout (`actimeo`, whole seconds — `man mount_nfs`:
/// `actimeo=⟨seconds⟩`) the mount requests. §4.6 owed this value "from the measured loopback RTT"; the
/// RTT resolves it, and floors it. slates's loopback GETATTR RTT is sub-millisecond — roughly 10,000×
/// finer than the whole-second `actimeo` knob — so an RTT-derived timeout rounds to the knob's minimum.
/// The two competing goals both land on that minimum: `actimeo=0` (`noac`) sends every `getattr` to the
/// server and defeats `rdirplus`'s attribute batching, while the macOS default (5–60 s, scaled by file
/// age) is far too stale for an overlay that changes under merges and outside edits. So the mount asks
/// for the finest nonzero cache the knob expresses, one second — the same freshness/amortization
/// trade-off sylk documents for its 100 ms FUSE attribute timeout (`core/purevfs`).
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
/// at `path`, under the mount `capability` (§4.13; AUD-01 — the attachment id and token `attach`
/// returned, the bearer authority the daemon validates every request's file handle against; a mount
/// path with no capability reaches nothing): the design's options (§4.6 — NFSv3 over TCP, the explicit
/// `port`/`mountport` so no portmap query is needed, `soft,intr` so a wedged mount is escapable,
/// `locallocks`, `nosuid`, `rdirplus`), plus `noresvport` for the unprivileged mount (R10) and the
/// derived attribute-cache timeout, and `rdonly` for a read-only mount (the kernel refuses writes at the
/// mount, as the daemon does under the read-only capability). slates serves NFS and MOUNT on one port,
/// so `port` and `mountport` are the same. The export is
/// `localhost:/<name>@<attachment_hex>.<token_hex>`, the volume's provisioned name under the synthetic
/// host root with its capability.
fn mount_args(
  port: u16,
  name: &str,
  capability: MountCapability,
  path: &str,
  read_only: bool,
) -> Vec<String> {
  let access = if read_only { ",rdonly" } else { "" };
  let options = format!(
    "vers=3,tcp,port={port},mountport={port},noresvport,soft,intr,locallocks,nosuid,rdirplus,\
     actimeo={ATTR_CACHE_SECONDS}{access}"
  );
  vec![
    "-o".to_owned(),
    options,
    format!("localhost:{}", export_path(name, capability)),
    path.to_owned(),
  ]
}

/// Format: a mount capability as `attach` returns it — the attachment id and its 16-byte secret token
/// (§4.13; AUD-01), the bearer authority the daemon validates every request's file handle against.
pub(crate) type MountCapability = (u64, [u8; 16]);

/// The export path a capability mount presents: `/<name>@<attachment_hex>.<token_hex>` (§4.13; AUD-01).
pub(crate) fn export_path(name: &str, capability: MountCapability) -> String {
  let (attachment, token) = capability;
  let token_hex: String = token.iter().map(|b| format!("{b:02x}")).collect();
  format!("/{name}@{attachment:x}.{token_hex}")
}

/// Runs `mount_nfs` to mount the volume `report` names at the existing user-owned `path` under the mount
/// `capability` its attachment returned (`read_only` for a read-only mount), returning the mounted path.
/// Refuses first (like sylk's `strictExecutionProbe`, naming what is missing) if the host has no loopback
/// mount mechanism or the daemon is not serving NFS; then a typed failure if `mount_nfs` refuses (a
/// missing mount point, a busy path).
pub(crate) fn establish(
  report: &StatusReport,
  capability: MountCapability,
  path: &str,
  read_only: bool,
) -> Result<String, Failure> {
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
  let args = mount_args(port, &report.name, capability, path, read_only);
  let status = Command::new("mount_nfs")
    .args(&args)
    .status()
    .map_err(|e| Failure::Failed(format!("running mount_nfs: {e}")))?;
  if !status.success() {
    // The failure names the volume, never its capability token (a secret).
    return Err(Failure::Failed(format!(
      "mount_nfs localhost:/{}@… {path} failed ({status})",
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

  /// A read-only mount asks the kernel for `rdonly` (with the daemon refusing writes under the
  /// read-only capability too); a writable mount does not.
  #[test]
  fn a_read_only_mount_asks_the_kernel_for_rdonly() {
    let writable = mount_args(
      54321,
      "myproject",
      (0x1f, [0xab; 16]),
      "/Users/me/mnt",
      false,
    );
    assert!(
      !writable[1].contains("rdonly"),
      "a writable mount is not read-only: {}",
      writable[1]
    );
    let read_only = mount_args(
      54321,
      "myproject",
      (0x1f, [0xab; 16]),
      "/Users/me/mnt",
      true,
    );
    assert!(
      read_only[1].ends_with(",rdonly"),
      "a read-only mount asks the kernel for rdonly: {}",
      read_only[1]
    );
  }

  /// The mount arguments carry the daemon's loopback port for both NFS and MOUNT, the unprivileged
  /// `noresvport`, NFSv3, the derived attribute-cache timeout, and the export named by the volume's
  /// provisioned name at the chosen path — the command `slates mount` runs.
  #[test]
  fn the_mount_arguments_target_the_daemon_port_and_the_named_export() {
    let args = mount_args(
      54321,
      "myproject",
      (0x1f, [0xab; 16]),
      "/Users/me/mnt",
      false,
    );
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
      args[2],
      format!("localhost:/myproject@1f.{}", "ab".repeat(16)),
      "the export is the volume's provisioned name with its mount capability (AUD-01)"
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
