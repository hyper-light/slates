//! AC-1.2: the differential suite against the host filesystem. The same generated histories
//! the model suite runs (`common::steps`) are applied to a volume and to a directory on a
//! RAM-backed host filesystem, and the abstract state is compared after every step under the
//! reviewed equivalence policy (`docs/wip/EQUIVALENCE.md`). The host is the oracle for POSIX;
//! the model suite is the oracle for the design's own rules (accounting, snapshots).
//!
//! Gated: `SLATES_TEST_RAMDIR` must name a RAM-backed directory (CI Linux: `/dev/shm`); without
//! it the suite prints that it skipped and passes, so a machine without a RAM disk never fails
//! and never writes disk (CLAUDE.md §4). Everything written goes under one subdirectory named
//! with the process id, removed when the harness drops it, panics included.

// Test harness code: an unwrap here is a failed test, which is what it should be. proptest's
// strategy types carry `Arc` (D-8's harness exception). The host filesystem is this test's
// oracle, so it uses `std::fs` writes; they land only under the RAM-backed directory the
// environment names, and the directory is removed at the end.
#![cfg(unix)]
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::disallowed_types,
  clippy::disallowed_methods
)]

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};

use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use slates_vfs::error::VfsError;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, Volume};

mod common;
use common::drive::{apply_volume, head_state, pick_file};
use common::steps::{Step, step};
use common::{store, volume_with};

/// Format: the environment variable naming the RAM-backed directory.
const RAMDIR_VAR: &str = "SLATES_TEST_RAMDIR";

/// The host directory of one run, removed on drop.
struct HostRoot {
  path: PathBuf,
}

impl HostRoot {
  fn create(base: &Path) -> io::Result<Self> {
    let path = base.join(format!("slates-differential-{}", std::process::id()));
    if path.exists() {
      fs::remove_dir_all(&path)?;
    }
    fs::create_dir(&path)?;
    Ok(Self { path })
  }

  fn fresh_case(&self, case: u64) -> io::Result<PathBuf> {
    let dir = self.path.join(format!("case-{case}"));
    fs::create_dir(&dir)?;
    // The anchor every symlink points at: a regular file, so a path through a symlink is
    // "not a directory" on both sides (see EQUIVALENCE.md).
    fs::write(dir.join(".anchor"), b"anchor")?;
    Ok(dir)
  }
}

impl Drop for HostRoot {
  fn drop(&mut self) {
    let _ = fs::remove_dir_all(&self.path);
  }
}

/// The host's name policy, probed once: a folding filesystem refuses the case variant.
fn probe_policy(root: &Path) -> NameEquivalence {
  fs::write(root.join("probe-a"), b"").unwrap();
  let folded = matches!(
    fs::File::create_new(root.join("PROBE-A")),
    Err(e) if e.kind() == io::ErrorKind::AlreadyExists
  );
  let _ = fs::remove_file(root.join("probe-a"));
  let _ = fs::remove_file(root.join("PROBE-A"));
  if folded {
    NameEquivalence::Fold
  } else {
    NameEquivalence::Exact
  }
}

/// A refusal as its errno name, or `OK`.
fn host_outcome(r: io::Result<()>) -> String {
  match r {
    Ok(()) => "OK".to_owned(),
    Err(e) => errno_name(e.raw_os_error().unwrap_or(0)).to_owned(),
  }
}

fn volume_outcome(r: Result<(), VfsError>) -> String {
  match r {
    Ok(()) => "OK".to_owned(),
    Err(e) => e.errno_name().to_owned(),
  }
}

/// The errno names the harness compares, from the raw numbers of the host.
fn errno_name(raw: i32) -> &'static str {
  match raw {
    libc::ENOENT => "ENOENT",
    libc::EEXIST => "EEXIST",
    libc::ENOTDIR => "ENOTDIR",
    libc::EISDIR => "EISDIR",
    libc::ENOTEMPTY => "ENOTEMPTY",
    libc::EINVAL => "EINVAL",
    libc::EPERM => "EPERM",
    libc::EMLINK => "EMLINK",
    libc::ENOSPC => "ENOSPC",
    libc::ENAMETOOLONG => "ENAMETOOLONG",
    libc::EXDEV => "EXDEV",
    libc::EACCES => "EACCES",
    _ => "OTHER",
  }
}

/// The equivalence policy's allowed sets: an outcome the host may report in place of the
/// volume's, per platform (EQUIVALENCE.md §3).
fn equivalent(volume: &str, host: &str) -> bool {
  if volume == host {
    return true;
  }
  let allowed: &[(&str, &[&str])] = &[
    // Unlinking a directory: Linux says EISDIR, macOS says EPERM.
    ("EISDIR", &["EPERM"]),
    // Renaming over a non-empty directory: ENOTEMPTY, or EEXIST on some filesystems.
    ("ENOTEMPTY", &["EEXIST"]),
  ];
  allowed
    .iter()
    .any(|(v, hosts)| *v == volume && hosts.contains(&host))
}

// ------------------------------------------------------------------ the host side

fn host_path(root: &Path, dir: &[String], name: &str) -> PathBuf {
  let mut p = root.to_path_buf();
  for c in dir {
    p.push(c);
  }
  p.push(name);
  p
}

fn apply_host(step: &Step, root: &Path, files: &[String]) -> Option<io::Result<()>> {
  Some(match step {
    Step::Create(p, n) => fs::File::create_new(host_path(root, p, n)).map(drop),
    Step::Mkdir(p, n) => fs::create_dir(host_path(root, p, n)),
    Step::Symlink(p, n) => std::os::unix::fs::symlink(root.join(".anchor"), host_path(root, p, n)),
    Step::Unlink(p, n) => fs::remove_file(host_path(root, p, n)),
    Step::Rmdir(p, n) => fs::remove_dir(host_path(root, p, n)),
    Step::Rename(fp, fnm, tp, tn) => fs::rename(host_path(root, fp, fnm), host_path(root, tp, tn)),
    Step::Link(p, n, pick) => {
      let target = pick_file(files, *pick)?;
      fs::hard_link(root.join(target), host_path(root, p, n))
    }
    Step::Write(pick, off, bytes) => {
      let target = pick_file(files, *pick)?;
      fs::OpenOptions::new()
        .write(true)
        .open(root.join(target))
        .and_then(|f| f.write_all_at(bytes, u64::from(*off)))
    }
    Step::Truncate(pick, len) => {
      let target = pick_file(files, *pick)?;
      fs::OpenOptions::new()
        .write(true)
        .open(root.join(target))
        .and_then(|f| f.set_len(u64::from(*len)))
    }
    Step::Edit(pick, at, del, bytes) => {
      let target = pick_file(files, *pick)?;
      let path = root.join(target);
      fs::read(&path).and_then(|mut content| {
        let at = usize::from(*at) % (content.len() + 1);
        let end = (at + usize::from(*del)).min(content.len());
        content.splice(at..end, bytes.iter().copied());
        fs::write(&path, content)
      })
    }
    Step::Snapshot => Ok(()),
  })
}

/// The host's abstract state: per directory the sorted names with their kinds; per regular
/// file its bytes and link count, by path.
type HostState = (
  Vec<(String, Vec<(String, char)>)>,
  BTreeMap<String, (Vec<u8>, u64)>,
);

fn host_state(root: &Path) -> HostState {
  let mut dirs = Vec::new();
  let mut files = BTreeMap::new();
  let mut stack = vec![(String::new(), root.to_path_buf())];
  while let Some((prefix, dir)) = stack.pop() {
    let mut names = Vec::new();
    for entry in fs::read_dir(&dir).unwrap() {
      let entry = entry.unwrap();
      let name = entry.file_name().to_string_lossy().to_string();
      if name == ".anchor" {
        continue;
      }
      let meta = entry.metadata().unwrap();
      let kind = if meta.is_dir() {
        'd'
      } else if meta.file_type().is_symlink() {
        'l'
      } else {
        'f'
      };
      names.push((name.clone(), kind));
      let path = format!("{prefix}/{name}");
      if kind == 'd' {
        stack.push((path, entry.path()));
      } else if kind == 'f' {
        files.insert(path, (fs::read(entry.path()).unwrap(), meta.nlink()));
      }
    }
    names.sort();
    dirs.push((prefix, names));
  }
  dirs.sort();
  (dirs, files)
}

// ------------------------------------------------------------------ the volume side

/// The volume's abstract state in the host's shape.
fn volume_state(vol: &Volume, store: &Store) -> HostState {
  let s = head_state(vol, store);
  (s.dirs, s.files)
}

// ------------------------------------------------------------------ the run

/// One history against both sides; a divergence is a test failure with the step named.
fn run_case(steps: &[Step], case_root: &Path, policy: NameEquivalence) {
  let mut store = store();
  let mut vol = volume_with(&mut store, Quota::Bounded { limit: 1 << 40 }, policy);
  for step in steps {
    let files: Vec<String> = volume_state(&vol, &store).1.into_keys().collect();
    let (Some(host), Some(real)) = (
      apply_host(step, case_root, &files),
      apply_volume(step, &mut vol, &mut store, &files),
    ) else {
      continue;
    };
    let (host, real) = (host_outcome(host), volume_outcome(real));
    assert!(
      equivalent(&real, &host),
      "outcome after {step:?}: volume {real}, host {host}"
    );
    let (hd, hf) = host_state(case_root);
    let (vd, vf) = volume_state(&vol, &store);
    assert_eq!(vd, hd, "listings after {step:?}");
    assert_eq!(vf, hf, "files after {step:?}");
  }
}

/// Measured: 300 cases of up to 40 steps run in about two seconds on tmpfs; CI raises the count
/// through `PROPTEST_CASES`.
fn cases() -> u32 {
  std::env::var("PROPTEST_CASES")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(300)
}

/// AC-1.2: the volume and the host filesystem agree on every abstract state under the policy.
#[test]
fn the_volume_agrees_with_the_host_filesystem_on_every_history() {
  let Some(base) = std::env::var_os(RAMDIR_VAR) else {
    println!(
      "differential: skipped — {RAMDIR_VAR} is not set (name a RAM-backed directory; CI Linux uses /dev/shm)"
    );
    return;
  };
  let base = PathBuf::from(base);
  assert!(
    base.is_dir(),
    "{RAMDIR_VAR}={} is not a directory",
    base.display()
  );
  let root = HostRoot::create(&base).unwrap();
  let policy = probe_policy(&root.path);
  println!(
    "differential: host {} folds names: {}",
    base.display(),
    policy == NameEquivalence::Fold
  );
  let mut runner = TestRunner::new(Config {
    cases: cases(),
    max_shrink_iters: 2000,
    // Never write a regression file into the tree (CLAUDE.md §4).
    failure_persistence: None,
    ..Config::default()
  });
  let counter = std::cell::Cell::new(0u64);
  let strategy = prop::collection::vec(step(), 1..40);
  let result = runner.run(&strategy, |steps| {
    let n = counter.get();
    counter.set(n + 1);
    let case_root = root.fresh_case(n).unwrap();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      run_case(&steps, &case_root, policy);
    }));
    let _ = fs::remove_dir_all(&case_root);
    outcome.map_err(|e| {
      TestCaseError::fail(
        e.downcast_ref::<String>()
          .cloned()
          .or_else(|| e.downcast_ref::<&str>().map(|s| (*s).to_owned()))
          .unwrap_or_else(|| "panic".to_owned()),
      )
    })?;
    Ok(())
  });
  if let Err(e) = result {
    panic!("differential: {e}");
  }
  println!(
    "differential: {} histories agreed with the host",
    counter.get()
  );
}
