//! A real directory on disk for a test's landing target or overlay base (design §0.2, A-50): a landing writes
//! the host's disk and an overlay reads a base there, so tests exercise both for real — never under `/tmp`
//! or any system temporary directory, and never in a RAM directory. It lives in the build output
//! (`CARGO_TARGET_TMPDIR`, under `target/`), named with the process id and a per-process counter, and is
//! removed when dropped. The path is canonical: a landing resolves its target component by component with
//! `O_NOFOLLOW`.

use std::sync::atomic::{AtomicU64, Ordering};

/// A test's directory, removed on drop.
pub(crate) struct TargetDir {
  /// The canonical path of the directory.
  pub(crate) path: String,
}

impl TargetDir {
  /// Writes `bytes` to `name` in the directory: a file the disk held before a volume overlaid it or a
  /// landing wrote into it — the fixture's own directory in the build output, as its creation and removal
  /// are.
  #[allow(dead_code)]
  pub(crate) fn seed(&self, name: &str, bytes: &[u8]) {
    #[allow(clippy::disallowed_methods)]
    std::fs::write(format!("{}/{name}", self.path), bytes).unwrap();
  }
}

impl Drop for TargetDir {
  fn drop(&mut self) {
    // A mount the test left at the directory is unmounted first, as its user would (A-102: a daemon's stop or a
    // volume's destroy ends a FUSE mount but leaves it in place, answering `ENOTCONN`); otherwise the removal below
    // fails on it and a dead mount stays on the host. Lazily, so a mount still busy goes as soon as it is free.
    #[cfg(target_os = "linux")]
    {
      let mounted = std::fs::read_to_string("/proc/self/mountinfo").is_ok_and(|table| {
        table
          .lines()
          .any(|line| line.split(' ').nth(4) == Some(self.path.as_str()))
      });
      if mounted {
        let _ = std::process::Command::new("fusermount3")
          .args(["-u", "-z", &self.path])
          .status();
      }
    }
    // The test's own directory in the build output (CLAUDE §4: removed at the end).
    #[allow(clippy::disallowed_methods)]
    let _removed = std::fs::remove_dir_all(&self.path);
  }
}

/// A fresh, empty, canonical directory in the build output.
pub(crate) fn target_dir() -> TargetDir {
  static NEXT: AtomicU64 = AtomicU64::new(0);
  let made = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
    "slates-{}-{}",
    std::process::id(),
    NEXT.fetch_add(1, Ordering::Relaxed)
  ));
  #[allow(clippy::disallowed_methods)]
  std::fs::create_dir_all(&made).unwrap();
  #[allow(clippy::disallowed_methods)]
  let path = std::fs::canonicalize(&made).unwrap();
  TargetDir {
    path: path.to_string_lossy().into_owned(),
  }
}
