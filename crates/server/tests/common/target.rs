//! A landing target for a test: a fresh directory under the process's temporary directory, named
//! with the process id and removed when dropped (CLAUDE.md §4: a test's scratch is named with the pid
//! and removed at the end). A landing plans against a real directory (§4.15), so the target must exist
//! and be canonical — the landing resolves it component by component with `O_NOFOLLOW`, and macOS's
//! `/var` is a symlink to `/private/var`.

/// A test's landing target, removed on drop.
pub(crate) struct TargetDir {
  /// The canonical path of the directory.
  pub(crate) path: String,
}

impl Drop for TargetDir {
  fn drop(&mut self) {
    let _ = std::process::Command::new("rm")
      .args(["-rf", &self.path])
      .output();
  }
}

/// A fresh, empty, canonical landing target under the temporary directory.
pub(crate) fn target_dir() -> TargetDir {
  let out = std::process::Command::new("mktemp")
    .args([
      "-d",
      "-t",
      &format!("slates-land-{}.XXXXXX", std::process::id()),
    ])
    .output()
    .unwrap();
  assert!(
    out.status.success(),
    "mktemp -d: {}",
    String::from_utf8_lossy(&out.stderr)
  );
  let made = String::from_utf8_lossy(&out.stdout).trim().to_owned();
  // A read of the path, never a write: the canonical form the landing's `O_NOFOLLOW` walk accepts.
  let path = std::fs::canonicalize(&made)
    .map(|p| p.to_string_lossy().into_owned())
    .unwrap_or(made);
  TargetDir { path }
}
