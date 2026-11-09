//! Diagnosing a FUSE mount the host's mount-helper policy refused (§4.6; GAPS, decided 2026-11-09).
//!
//! Ubuntu 26.04 confines `fusermount3` with an AppArmor profile that allows FUSE mounts only under some directories;
//! elsewhere the kernel refuses the helper's `mount(2)`, and the helper prints `mount failed: Permission denied`, which
//! alone does not say why. The mount is always attempted, and the kernel decides. Only after such a refusal does the
//! handshake read the helper's own AppArmor label, as the kernel reports it in `/proc/<pid>/attr/apparmor/current`
//! (`attr/current` on kernels without the per-module directory), while the exited helper is held unreaped
//! (`waitid` with `WNOWAIT`, `crate::mount`): a process keeps its credentials, and so its label, until it is reaped.
//! Measured on GitHub's ubuntu-26.04 runner image 20261101.186.1 (diag-slabs-57, 2026-11-09): `fusermount3 (enforce)`
//! read on the host as an unprivileged user, as root, and in a container as root.
//!
//! Nothing predicts which directories a profile allows: re-implementing AppArmor's globs here would be a second
//! authority, drift from the operator's local rules, other distributions and the next profile version, and refuse
//! mounts the kernel would allow. The profile's file and the local-override file are named as where to look, by
//! Ubuntu's convention, never read. A label that cannot be read, or that names no profile and mode, leaves the refusal
//! the helper's own. Reads only, in a `bridge-*` crate the lint wall lets read host paths (R1).

use std::io::Read;

/// Format: the kernel's per-process AppArmor label files, under `/proc/<pid>/`, the per-module one first.
const LABEL_FILES: [&str; 2] = ["attr/apparmor/current", "attr/current"];
/// Format: the label of a process no profile confines.
const UNCONFINED: &str = "unconfined";
/// Format: where Ubuntu keeps a profile's file, named after the profile.
const PROFILE_DIRECTORY: &str = "/etc/apparmor.d";
/// Format: where Ubuntu keeps an operator's local rules for a profile, which the profile includes by its own name.
const LOCAL_DIRECTORY: &str = "/etc/apparmor.d/local";
/// Format: what `fusermount3` prints when the kernel refused its mount: `fusermount3: mount failed: Permission denied`.
pub const PERMISSION_DENIED: &str = "Permission denied";
/// Shape: the most of a label file read: one page, far past any profile name and mode the kernel writes there.
const LABEL_BYTES: u64 = 4096;

/// The policy that refused a mount, as the kernel labels the helper: the helper, the AppArmor profile and its mode,
/// and the files an operator would look at, named by convention.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelperPolicy {
  /// The helper, as the mount spawned it.
  pub helper: String,
  /// The AppArmor profile confining it.
  pub profile: String,
  /// The profile's mode (`enforce`, `complain`, …), as the kernel reports it.
  pub mode: String,
  /// Where the profile's file is kept, by convention (not read).
  pub policy_file: String,
  /// Where an operator's local rules for it are kept, by convention (not read).
  pub local_override: String,
}

/// The AppArmor label of process `pid`, read from `/proc`; `None` when no label file can be read. Called only while
/// the process is held unreaped, so `pid` cannot name another process.
pub fn label_of(pid: u32) -> Option<String> {
  LABEL_FILES.iter().find_map(|file| {
    let mut label = String::new();
    std::fs::File::open(format!("/proc/{pid}/{file}"))
      .ok()?
      .take(LABEL_BYTES)
      .read_to_string(&mut label)
      .ok()?;
    Some(label.trim().to_owned())
  })
}

/// The policy a label names for `helper`: `PROFILE (MODE)`. `None` for an unconfined process, an empty label, or one
/// that names no mode.
pub fn policy_of(helper: &str, label: &str) -> Option<HelperPolicy> {
  let (profile, mode) = label.trim().rsplit_once(" (")?;
  let mode = mode.strip_suffix(')')?;
  if profile.is_empty() || mode.is_empty() || profile == UNCONFINED {
    return None;
  }
  Some(HelperPolicy {
    helper: helper.to_owned(),
    profile: profile.to_owned(),
    mode: mode.to_owned(),
    policy_file: format!("{PROFILE_DIRECTORY}/{profile}"),
    local_override: format!("{LOCAL_DIRECTORY}/{profile}"),
  })
}

#[cfg(test)]
mod tests {
  use super::policy_of;

  /// Ubuntu 26.04's label for its helper (diag-slabs-57).
  #[test]
  fn an_enforced_profile_is_named_with_its_mode_and_files() {
    let policy = policy_of("fusermount3", "fusermount3 (enforce)\n").unwrap();
    assert_eq!(
      (policy.profile.as_str(), policy.mode.as_str()),
      ("fusermount3", "enforce")
    );
    assert_eq!(policy.policy_file, "/etc/apparmor.d/fusermount3");
    assert_eq!(policy.local_override, "/etc/apparmor.d/local/fusermount3");
  }

  /// An unconfined helper, or a label naming no mode, attributes nothing.
  #[test]
  fn an_unconfined_or_modeless_label_attributes_nothing() {
    assert_eq!(policy_of("fusermount3", "unconfined"), None);
    assert_eq!(policy_of("fusermount3", "fusermount3"), None);
    assert_eq!(policy_of("fusermount3", ""), None);
  }
}
