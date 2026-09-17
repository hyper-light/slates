//! The workload suite (Part 6 "Real workloads"; AC-3.2, AC-4.2): every roster tool present on
//! this host runs its script twice under one fixed environment — in a host directory and in a
//! directory inside the mount — and the two runs must be byte-identical: same exit code, same
//! output with the directory roots normalized, same tree manifest under the roster's reviewed
//! exclusions. The volume is created with the host directory's name-folding policy so git's
//! `core.ignorecase` probe sees the same filesystem on both sides (`EQUIVALENCE.md` §4).

use std::path::Path;
use std::process::{Command, Stdio};

use slates_conformance::Suite;
use slates_conformance::capability::HostOs;
use slates_conformance::record::{Counts, WorkloadResult, WorkloadStatus};
use slates_conformance::workload::{
  ENV_SQLITE_BUSY_MS, ENV_WATCH_SECONDS, Entry, EntryKind, Manifest, ROSTER, Workload, compare,
};

use super::slates::{Session, folds_names};
use super::{Run, SQLITE_BUSY_MS, SuiteResult, WATCH_SECONDS, create_dir, tool_on_path};
use crate::Failure;

/// Format: the fixed identity and date every git commit is made with, so object hashes match
/// between the host run and the mounted run.
const GIT_IDENTITY: &[(&str, &str)] = &[
  ("GIT_AUTHOR_NAME", "slates"),
  ("GIT_AUTHOR_EMAIL", "slates@example.invalid"),
  ("GIT_COMMITTER_NAME", "slates"),
  ("GIT_COMMITTER_EMAIL", "slates@example.invalid"),
  ("GIT_AUTHOR_DATE", "2026-09-14T00:00:00Z"),
  ("GIT_COMMITTER_DATE", "2026-09-14T00:00:00Z"),
  ("GIT_CONFIG_GLOBAL", "/dev/null"),
  ("GIT_CONFIG_NOSYSTEM", "1"),
  ("TZ", "UTC"),
  ("LC_ALL", "C"),
  ("npm_config_update_notifier", "false"),
  ("npm_config_fund", "false"),
  ("npm_config_audit", "false"),
  // No npm log files: their path (in the cache) would be the one line that differs between runs.
  ("npm_config_logs_max", "0"),
];

/// Format: a file's permission bits in `st_mode`.
const PERMISSION_MASK: u32 = 0o7777;

/// Whether a roster entry is the watcher for another operating system.
fn foreign_watcher(workload: &Workload, os: HostOs) -> bool {
  workload.name == "watcher" && (workload.tool == "fswatch") != (os == HostOs::Macos)
}

/// The BLAKE3 of a file's bytes, hexadecimal.
fn digest_of(path: &Path) -> Result<String, Failure> {
  let bytes = std::fs::read(path).map_err(|e| Failure(format!("{}: {e}", path.display())))?;
  Ok(blake3::hash(&bytes).to_hex().to_string())
}

/// One manifest entry for a path under `root`.
fn entry_of(root: &Path, path: &Path) -> Result<Entry, Failure> {
  use std::os::unix::fs::PermissionsExt;
  let metadata =
    std::fs::symlink_metadata(path).map_err(|e| Failure(format!("{}: {e}", path.display())))?;
  let relative = path
    .strip_prefix(root)
    .unwrap_or(path)
    .to_string_lossy()
    .replace('\\', "/");
  let file_type = metadata.file_type();
  let (kind, size, digest) = if file_type.is_symlink() {
    let target = std::fs::read_link(path)
      .map(|t| t.to_string_lossy().into_owned())
      .unwrap_or_default();
    (EntryKind::Symlink { target }, 0, String::new())
  } else if file_type.is_dir() {
    (EntryKind::Directory, 0, String::new())
  } else if file_type.is_file() {
    (EntryKind::File, metadata.len(), digest_of(path)?)
  } else {
    (EntryKind::Other, 0, String::new())
  };
  // A symlink's permission bits are not meaningful (POSIX ignores them — access is through the target)
  // and are not portable: Linux always reports 0o777, macOS reports the creation mode masked by the
  // umask (so 0o755 for a default umask), and the NFS-loopback mount reports 0o777. Normalize a
  // symlink's mode to the 0o777 convention so the comparison judges the tree, not a non-behavioural,
  // fs-specific value; a regular file's or directory's mode is compared as measured.
  let mode = if file_type.is_symlink() {
    0o777
  } else {
    metadata.permissions().mode() & PERMISSION_MASK
  };
  Ok(Entry {
    path: relative,
    kind,
    mode,
    size,
    digest,
  })
}

/// The manifest of a tree, sorted by path.
fn manifest_of(root: &Path) -> Result<Manifest, Failure> {
  let mut entries = Vec::new();
  let mut stack = vec![root.to_path_buf()];
  while let Some(dir) = stack.pop() {
    for entry in std::fs::read_dir(&dir).map_err(|e| Failure(format!("{}: {e}", dir.display())))? {
      let path = entry?.path();
      let item = entry_of(root, &path)?;
      if matches!(item.kind, EntryKind::Directory) {
        stack.push(path);
      }
      entries.push(item);
    }
  }
  entries.sort_by(|a, b| a.path.cmp(&b.path));
  Ok(Manifest { entries })
}

/// Runs a workload's script in a fresh directory and captures everything.
fn execute(
  workload: &Workload,
  dir: &Path,
  scratch: &Path,
) -> Result<slates_conformance::workload::Run, Failure> {
  create_dir(dir)?;
  let script = format!("exec 2>&1\nset -e\n{}", workload.script);
  let output = Command::new("sh")
    .arg("-c")
    .arg(&script)
    .current_dir(dir)
    .envs(GIT_IDENTITY.iter().copied())
    .env("npm_config_cache", scratch.join("npm-cache"))
    .env(ENV_SQLITE_BUSY_MS, SQLITE_BUSY_MS.to_string())
    .env(ENV_WATCH_SECONDS, WATCH_SECONDS.to_string())
    .stdin(Stdio::null())
    .output()
    .map_err(|e| Failure(format!("running the {} workload: {e}", workload.name)))?;
  Ok(slates_conformance::workload::Run {
    directory: dir.display().to_string(),
    exit_code: output.status.code().unwrap_or(-1),
    output: String::from_utf8_lossy(&output.stdout).into_owned(),
    manifest: manifest_of(dir)?,
  })
}

/// The workload suite over the mount.
pub(crate) fn run_workloads(run: &Run<'_>) -> Result<SuiteResult, Failure> {
  let host_root = run.scratch.subdir("workloads-host")?;
  let fold = folds_names(&host_root);
  let session = Session::open(run, "workloads", fold, None)?;
  let mount_root = session.workdir("workloads")?;
  let mut tools = Vec::new();
  let mut notes = vec![format!(
    "the volume was created with --fold={fold} to match the host scratch's name policy; git identity and dates fixed; \
     sqlite busy timeout {SQLITE_BUSY_MS} ms; watcher wait {WATCH_SECONDS} s"
  )];
  for workload in ROSTER {
    if foreign_watcher(workload, run.os) {
      continue;
    }
    if !tool_on_path(workload.tool) {
      tools.push(WorkloadResult {
        name: workload.name.to_owned(),
        status: WorkloadStatus::Skipped {
          tool: workload.tool.to_owned(),
        },
      });
      continue;
    }
    let host = execute(workload, &host_root.join(workload.name), run.scratch.path())?;
    let mount = execute(
      workload,
      &mount_root.join(workload.name),
      run.scratch.path(),
    )?;
    let status = compare(workload, &host, &mount);
    if let WorkloadStatus::Differs { detail } = &status {
      notes.push(format!("{}: {detail}", workload.name));
      println!("workloads: {} differs: {detail}", workload.name);
    } else {
      println!("workloads: {}: {status:?}", workload.name);
    }
    tools.push(WorkloadResult {
      name: workload.name.to_owned(),
      status,
    });
  }
  notes.extend(session.size_note.clone());
  drop(session);
  let ok = !tools
    .iter()
    .any(|t| matches!(t.status, WorkloadStatus::Differs { .. }));
  let run_count = tools
    .iter()
    .filter(|t| !matches!(t.status, WorkloadStatus::Skipped { .. }))
    .count();
  let counts = Counts::Workloads { tools };
  Ok(SuiteResult {
    privilege: super::suites::this_user().privilege(),
    outcome: match run.adapter(Suite::Workloads) {
      Some((adapter, not_covered)) => slates_conformance::Outcome::Limited {
        adapter,
        not_covered,
        counts,
      },
      None => slates_conformance::Outcome::Ran { counts },
    },
    command: "sh -c '<roster script>' in <host scratch>/<tool> and <mount>/<tool>, per tool of crates/conformance/src/workload.rs ROSTER; trees compared by manifest".to_owned(),
    bound: format!("{run_count} tools run, one script each"),
    expected_failure_list: None,
    notes,
    ok,
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::path::PathBuf;

  /// AC-3.2 / T-3.3: run the real editor save workload in ordinary and TMPDIR-matching paths.
  /// Both must retain the original bytes in the backup and write the edited bytes to the new file.
  /// The caller supplies RAM-backed scratch; no mount, installation or privilege is performed here.
  #[test]
  fn an_editor_save_preserves_its_backup_even_inside_tmpdir() {
    let Some(ram) = std::env::var_os("SLATES_TEST_RAMDIR") else {
      eprintln!("skipping real Vim save history: set SLATES_TEST_RAMDIR to a RAM-backed directory");
      return;
    };
    if !tool_on_path("vim") {
      eprintln!("skipping real Vim save history: vim is absent");
      return;
    }
    let path = PathBuf::from(ram).join(format!("slates-editor-{}", std::process::id()));
    // Development-tool fixture, created only beneath the caller's explicitly supplied RAM directory.
    #[allow(clippy::disallowed_methods)]
    std::fs::create_dir(&path).unwrap();
    let scratch = super::super::Scratch { path, keep: false };
    let ordinary = scratch.path().join("ordinary");
    let temporary = scratch.path().join("temporary");
    let workload = ROSTER
      .iter()
      .find(|workload| workload.name == "editor")
      .unwrap();
    for directory in [&ordinary, &temporary] {
      create_dir(directory).unwrap();
      let output = Command::new("sh")
        .args(["-ec", workload.script])
        .current_dir(directory)
        .env("TMPDIR", &temporary)
        .env_remove("TMP")
        .env_remove("TEMP")
        .stdin(Stdio::null())
        .output()
        .unwrap();
      assert!(
        output.status.success(),
        "editor failed: {}",
        String::from_utf8_lossy(&output.stderr)
      );
      let edited = std::fs::read(directory.join("note.txt")).unwrap();
      let backup = std::fs::read(directory.join("note.txt~"));
      eprintln!(
        "editor save in {}: output={:?}, backup={backup:?}",
        directory.display(),
        String::from_utf8_lossy(&output.stdout)
      );
      assert_eq!(edited, b"hello draft\n");
      assert_eq!(
        backup.unwrap(),
        b"draft\n",
        "the rename-save preserves the previous bytes"
      );
    }
  }
}
