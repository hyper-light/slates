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
  fn create(base: &Path, test: &str) -> io::Result<Self> {
    let path = base.join(format!("slates-differential-{test}-{}", std::process::id()));
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

/// The refusals a rename `step` meets in the state `dirs`, each one POSIX names for `rename(2)`: the
/// source missing, or a component of either path missing (`ENOENT`) or not a directory (`ENOTDIR`); a
/// non-directory onto a directory (`EISDIR`); a directory onto a non-directory (`ENOTDIR`); a directory
/// onto a non-empty one (`ENOTEMPTY`, or `EEXIST`); a directory into its own subtree (`EINVAL`). When more than one applies, POSIX lets the
/// implementation report any of them (XSH 2.3, "Error Numbers": "If more than one error occurs in
/// processing a function call, any one of the possible errors may be returned, as the order of
/// detection is undefined"), and hosts differ: renaming a file onto a non-empty directory is
/// `ENOTEMPTY` on Linux's tmpfs and `EISDIR` on APFS and in the volume (CI run 36261369758); a missing
/// source into a path through a symlink is `ENOENT` on APFS and `ENOTDIR` in the volume; a directory
/// moved into itself onto a file there is `ENOTDIR` on APFS and `EINVAL` in the volume.
fn rename_refusals(
  step: &Step,
  dirs: &[(String, Vec<(String, char)>)],
  policy: NameEquivalence,
) -> Vec<&'static str> {
  let Step::Rename(from_dir, from_name, to_dir, to_name) = step else {
    return Vec::new();
  };
  // Names compare as the volume compares them: folded on a folding host (APFS), exact elsewhere.
  let same = |a: &str, b: &str| policy.fold(a) == policy.fold(b);
  let names_in = |path: &str| {
    dirs
      .iter()
      .find(|(listed, _)| listed == path)
      .map(|(_, names)| names)
  };
  // The directory a path of components names, or the refusal its walk meets.
  let walk = |dir: &[String]| -> Result<String, &'static str> {
    let mut path = String::new();
    for part in dir {
      let kind = names_in(&path)
        .and_then(|names| names.iter().find(|(name, _)| same(name, part)))
        .map(|(name, kind)| (name.clone(), *kind));
      match kind {
        None => return Err("ENOENT"),
        Some((name, 'd')) => path.push_str(&format!("/{name}")),
        Some(_) => return Err("ENOTDIR"),
      }
    }
    Ok(path)
  };
  let entry_in = |path: &str, name: &str| {
    names_in(path)
      .and_then(|names| names.iter().find(|(entry, _)| same(entry, name)))
      .map(|(entry, kind)| (entry.clone(), *kind))
  };
  let mut refusals = Vec::new();
  let source = match walk(from_dir) {
    Ok(path) => {
      let entry = entry_in(&path, from_name);
      if entry.is_none() {
        refusals.push("ENOENT");
      }
      entry.map(|(name, kind)| (format!("{path}/{name}"), kind))
    }
    Err(refusal) => {
      refusals.push(refusal);
      None
    }
  };
  let target_parent = match walk(to_dir) {
    Ok(path) => Some(path),
    Err(refusal) => {
      refusals.push(refusal);
      None
    }
  };
  // A directory moved into its own subtree: the new path's parent is the directory or below it.
  if let (Some((source_path, 'd')), Some(parent)) = (&source, &target_parent)
    && (parent == source_path || parent.starts_with(&format!("{source_path}/")))
  {
    refusals.push("EINVAL");
  }
  let target = target_parent
    .as_ref()
    .and_then(|path| entry_in(path, to_name).map(|(name, kind)| (format!("{path}/{name}"), kind)));
  if let (Some((_, source)), Some((target_path, target))) = (&source, &target) {
    match (*source == 'd', *target == 'd') {
      (false, true) => refusals.push("EISDIR"),
      (true, false) => refusals.push("ENOTDIR"),
      _ => {}
    }
    if *target == 'd' && names_in(target_path).is_some_and(|names| !names.is_empty()) {
      refusals.extend(["ENOTEMPTY", "EEXIST"]);
    }
  }
  refusals
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

/// `head_state` uses absolute VFS paths; resolve them inside this case's RAM directory.
fn selected_host_file(root: &Path, target: &str) -> PathBuf {
  root.join(
    target
      .strip_prefix('/')
      .expect("head_state paths start at the VFS root"),
  )
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
      fs::hard_link(selected_host_file(root, target), host_path(root, p, n))
    }
    Step::Write(pick, off, bytes) => {
      let target = pick_file(files, *pick)?;
      fs::OpenOptions::new()
        .write(true)
        .open(selected_host_file(root, target))
        .and_then(|f| f.write_all_at(bytes, u64::from(*off)))
    }
    Step::Truncate(pick, len) => {
      let target = pick_file(files, *pick)?;
      fs::OpenOptions::new()
        .write(true)
        .open(selected_host_file(root, target))
        .and_then(|f| f.set_len(u64::from(*len)))
    }
    Step::Edit(pick, at, del, bytes) => {
      let target = pick_file(files, *pick)?;
      let path = selected_host_file(root, target);
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
    let (dirs_before, files_before) = volume_state(&vol, &store);
    let refusals = rename_refusals(step, &dirs_before, policy);
    let files: Vec<String> = files_before.into_keys().collect();
    let (Some(host), Some(real)) = (
      apply_host(step, case_root, &files),
      apply_volume(step, &mut vol, &mut store, &files),
    ) else {
      continue;
    };
    let (host, real) = (host_outcome(host), volume_outcome(real));
    // Several refusals apply: either side may report any of them (see [`rename_refusals`]).
    let distinct = {
      let mut distinct = refusals.clone();
      distinct.retain(|refusal| *refusal != "EEXIST");
      distinct.sort_unstable();
      distinct.dedup();
      distinct.len()
    };
    let any_applicable_refusal =
      distinct > 1 && refusals.contains(&real.as_str()) && refusals.contains(&host.as_str());
    assert!(
      equivalent(&real, &host) || any_applicable_refusal,
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

/// AC-1.2: selected files and their aliases keep the same bytes and links at both depths.
#[test]
fn selected_files_and_aliases_agree_at_the_root_and_in_a_directory() {
  let Some(base) = std::env::var_os(RAMDIR_VAR) else {
    println!("differential: skipped — {RAMDIR_VAR} is not set (name a RAM-backed directory)");
    return;
  };
  let root = HostRoot::create(Path::new(&base), "selected-files").unwrap();
  let policy = probe_policy(&root.path);
  for (case, directory) in [Vec::new(), vec!["directory".to_owned()]]
    .into_iter()
    .enumerate()
  {
    let mut history = Vec::new();
    if let Some(name) = directory.first() {
      history.push(Step::Mkdir(Vec::new(), name.clone()));
    }
    history.extend([
      Step::Create(directory.clone(), "file".to_owned()),
      Step::Link(directory.clone(), "file".to_owned(), 0),
      Step::Write(0, 0, b"original".to_vec()),
      Step::Link(directory.clone(), "alias".to_owned(), 0),
      Step::Write(1, 0, b"changed".to_vec()),
      Step::Truncate(0, 4),
      Step::Edit(1, 1, 2, b"inserted".to_vec()),
      Step::Unlink(directory, "file".to_owned()),
      Step::Write(0, 0, b"survives".to_vec()),
    ]);
    let case_root = root.fresh_case(u64::try_from(case).unwrap()).unwrap();
    run_case(&history, &case_root, policy);
  }
}

/// AC-1.2: a file renamed onto a non-empty directory, the history CI run 36261369758 shrank to: `b/C`
/// is a second link of `C`, and `b/C` is renamed onto `b` itself. Two refusals apply at once (the
/// target is a directory; it is not empty) and POSIX lets either be reported. Linux's tmpfs reports
/// `ENOTEMPTY`, the volume `EISDIR`. Do: run the history on both sides. Expect: agreement, the state
/// unchanged on both.
#[test]
fn a_file_renamed_onto_a_non_empty_directory_may_report_either_refusal() {
  let Some(base) = std::env::var_os(RAMDIR_VAR) else {
    println!("differential: skipped — {RAMDIR_VAR} is not set (name a RAM-backed directory)");
    return;
  };
  let root = HostRoot::create(Path::new(&base), "rename-onto-directory").unwrap();
  let policy = probe_policy(&root.path);
  let b = || vec!["b".to_owned()];
  let history = [
    Step::Mkdir(Vec::new(), "b".to_owned()),
    Step::Create(Vec::new(), "C".to_owned()),
    Step::Link(b(), "C".to_owned(), 0),
    Step::Rename(b(), "C".to_owned(), Vec::new(), "b".to_owned()),
  ];
  let case_root = root.fresh_case(0).unwrap();
  run_case(&history, &case_root, policy);
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
  let root = HostRoot::create(&base, "histories").unwrap();
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
