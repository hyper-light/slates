//! Establishing the loopback NFS mount (§4.6): `mount_nfs localhost:PORT` at an existing user-owned
//! directory — no privilege (R10; `noresvport` uses a high source port, so no root) and no kernel
//! extension, the signing-free macOS bridge that needs no Apple entitlement. It is the same mechanism
//! sylk mounts over FUSE-T (which is itself an NFS loopback under the hood), reached here directly
//! because slates serves its own NFS. The daemon serves NFS + MOUNT + portmap on one loopback port
//! ([`slates_client::StatusReport::nfs_port`]); a volume's export is its provisioned name, which the
//! synthetic root resolves (`bridge-nfs`).

use std::process::Command;

use slates_client::StatusReport;

use crate::Failure;

/// Derived: the attribute-cache timeout (`actimeo`, whole seconds) the mount requests. slates's
/// loopback GETATTR is sub-millisecond, so revalidation is cheap; a short cache keeps the view fresh —
/// an overlay changes under merges and outside edits — while amortizing repeated stats over a burst of
/// identical reads. One second is the finest `mount_nfs`'s whole-second `actimeo` expresses; sylk
/// documents the same trade-off for its 100 ms FUSE attribute timeout (`core/purevfs`).
const ATTR_CACHE_SECONDS: u32 = 1;

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
/// mounted path. A typed failure if the daemon is not serving NFS, `mount_nfs` cannot be run (not a
/// macOS/BSD host), or it refuses (a missing mount point, a busy path).
pub(crate) fn establish(report: &StatusReport, path: &str) -> Result<String, Failure> {
  let port = report.nfs_port.ok_or_else(|| {
    Failure::Failed(
      "the daemon is not serving NFS (its loopback listener did not bind); cannot mount".to_owned(),
    )
  })?;
  let args = mount_args(port, &report.name, path);
  let status = Command::new("mount_nfs")
    .args(&args)
    .status()
    .map_err(|e| {
      Failure::Failed(format!(
        "running mount_nfs (is this a macOS/BSD host?): {e}"
      ))
    })?;
  if !status.success() {
    return Err(Failure::Failed(format!(
      "mount_nfs localhost:/{} {path} failed ({status})",
      report.name
    )));
  }
  Ok(path.to_owned())
}

#[cfg(test)]
mod tests {
  use super::mount_args;

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
}
