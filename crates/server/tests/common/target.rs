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

impl Drop for TargetDir {
  fn drop(&mut self) {
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
