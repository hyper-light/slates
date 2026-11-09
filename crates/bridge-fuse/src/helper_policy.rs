//! Diagnosing a FUSE mount the host's mount-helper policy refused (§4.6; GAPS, decided 2026-11-09).
//!
//! Ubuntu 26.04 confines `fusermount3` with an AppArmor profile (`/etc/apparmor.d/fusermount3`) that allows FUSE mounts
//! only under some directories; elsewhere the kernel refuses the helper's `mount(2)` and the helper prints `mount
//! failed: Permission denied`. That message alone does not say why. The mount is always attempted, and the kernel
//! decides: this module runs only after such a refusal, and looks for what an unprivileged user can read that names the
//! cause, AppArmor enabled (`/sys/module/apparmor/parameters/enabled`) and a profile file attached to the helper's path.
//! It never predicts which directories a profile allows: re-implementing AppArmor's globs here would be a second
//! authority, drift from the operator's local additions, other distributions and the next profile version, and refuse
//! mounts the kernel would allow. The directories Ubuntu 26.04's profile allows are listed in `docs/cli.md`, with the
//! image and the date they were measured.
//!
//! Reads only: this crate is one of the `bridge-*` crates the lint wall lets read host paths (R1), and nothing here
//! writes or creates anything. Every read is bounded ([`PROFILE_FILE_BYTES`]).

use std::io::Read;
use std::path::{Path, PathBuf};

/// Format: the kernel's switch for AppArmor, readable by every user: `Y` when it is enabled.
const APPARMOR_ENABLED: &str = "/sys/module/apparmor/parameters/enabled";
/// Format: where AppArmor's profile files are kept; a profile for a helper is named after it (Ubuntu's convention).
const APPARMOR_PROFILES: &str = "/etc/apparmor.d";
/// Format: the directory of an operator's local additions, which a profile includes by its own file name
/// (`include if exists <local/NAME>`).
const APPARMOR_LOCAL: &str = "local";
/// Format: what `fusermount3` prints when the kernel refused its mount: `fusermount3: mount failed: Permission denied`.
pub const PERMISSION_DENIED: &str = "Permission denied";
/// Shape: the most of a profile file read: 64 KiB, far past the 3 KiB of Ubuntu 26.04's `fusermount3` profile, so a
/// profile file is read whole while no file can make the diagnosis read without bound.
const PROFILE_FILE_BYTES: u64 = 64 << 10;
/// Format: the keyword that opens a named profile's declaration in an AppArmor profile file.
const PROFILE_KEYWORD: &str = "profile";

/// The policy that refused a mount: the helper, the AppArmor profile attached to it, the file that declares it, and the
/// file an operator adds local rules to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelperPolicy {
  /// The helper's path, as the kernel attaches a profile to it (symbolic links resolved).
  pub helper: String,
  /// The profile's name.
  pub profile: String,
  /// The file that declares the profile.
  pub policy_file: String,
  /// The file an operator adds local rules to, which the profile includes.
  pub local_override: String,
}

/// The AppArmor profile that confines `helper_name` (a program on the path, as the mount spawned it), when AppArmor is
/// enabled and a profile file attached to the helper's path is readable; `None` otherwise, and the refusal stays the
/// helper's own. Called only after the helper failed with [`PERMISSION_DENIED`].
pub fn diagnose(helper_name: &str) -> Option<HelperPolicy> {
  if !read_bounded(Path::new(APPARMOR_ENABLED))?
    .trim()
    .eq_ignore_ascii_case("Y")
  {
    return None;
  }
  let helper = resolve(helper_name)?;
  let file_name = helper.file_name()?.to_str()?.to_owned();
  let policy_file = Path::new(APPARMOR_PROFILES).join(&file_name);
  let helper = helper.to_str()?.to_owned();
  let profile = attached_profile(&read_bounded(&policy_file)?, &helper)?;
  Some(HelperPolicy {
    helper,
    profile,
    policy_file: policy_file.to_str()?.to_owned(),
    local_override: Path::new(APPARMOR_PROFILES)
      .join(APPARMOR_LOCAL)
      .join(&file_name)
      .to_str()?
      .to_owned(),
  })
}

/// The name of the profile in `profile_text` attached to the program at `helper`: a `profile NAME PATH` declaration
/// whose attachment is `helper`, or a profile named by the path itself. `None` when none is.
pub fn attached_profile(profile_text: &str, helper: &str) -> Option<String> {
  profile_text.lines().find_map(|line| {
    let mut words = line.split_whitespace();
    match words.next()? {
      PROFILE_KEYWORD => {
        let name = words.next()?;
        (words.next()? == helper).then(|| name.to_owned())
      }
      first if first == helper => Some(helper.to_owned()),
      _ => None,
    }
  })
}

/// The absolute path of the program `name` as a spawn by name finds it: the first match on `PATH`, symbolic links
/// resolved, since the kernel attaches a profile to the program's real path.
fn resolve(name: &str) -> Option<PathBuf> {
  let path = std::env::var_os("PATH")?;
  std::env::split_paths(&path)
    .map(|dir| dir.join(name))
    .find(|candidate| candidate.is_file())
    .and_then(|found| std::fs::canonicalize(found).ok())
}

/// The text of `path`, at most [`PROFILE_FILE_BYTES`] of it; `None` when it cannot be read.
fn read_bounded(path: &Path) -> Option<String> {
  let mut text = String::new();
  std::fs::File::open(path)
    .ok()?
    .take(PROFILE_FILE_BYTES)
    .read_to_string(&mut text)
    .ok()?;
  Some(text)
}

#[cfg(test)]
mod tests {
  use super::attached_profile;

  /// Ubuntu 26.04's own declaration (diag-slabs-51): a named profile attached to the helper's path.
  #[test]
  fn a_named_profile_attached_to_the_helper_is_found() {
    let text = "abi <abi/5.0>,\ninclude <tunables/global>\nprofile fusermount3 /usr/bin/fusermount3 {\n  /dev/fuse rw,\n}\n";
    assert_eq!(
      attached_profile(text, "/usr/bin/fusermount3"),
      Some("fusermount3".to_owned())
    );
  }

  /// A profile named by the path it confines.
  #[test]
  fn a_profile_named_by_the_helpers_path_is_found() {
    assert_eq!(
      attached_profile("/usr/bin/fusermount3 {\n}\n", "/usr/bin/fusermount3"),
      Some("/usr/bin/fusermount3".to_owned())
    );
  }

  /// A profile attached to another program, or none at all, attributes nothing.
  #[test]
  fn a_profile_for_another_path_attributes_nothing() {
    assert_eq!(
      attached_profile(
        "profile fusermount3 /bin/fusermount3 {\n}\n",
        "/usr/bin/fusermount3"
      ),
      None
    );
    assert_eq!(
      attached_profile(
        "# a comment naming /usr/bin/fusermount3\n",
        "/usr/bin/fusermount3"
      ),
      None
    );
  }
}
