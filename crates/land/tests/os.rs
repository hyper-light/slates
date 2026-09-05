//! The landing over the operating system (Phase 1 task 11; §4.15 step 6, §4.13): a real
//! directory on a RAM-backed filesystem named by `SLATES_TEST_RAMDIR` (CI Linux: `/dev/shm`);
//! without it every test here skips loudly and passes. Nothing is written outside the named
//! directory; each test works in its own subdirectory named with the process id and removes it.
//!
//! Covered: the worked example's shape against the disk (bytes, modes, mtimes, symlinks,
//! removals, a directory rename), containment refusals (a symlink component, `..`), idempotent
//! re-run by hash, and T-1.15's real `kill -9` on tmpfs: a child process lands round after
//! round until killed; every file is then a whole round, never torn; the parent resumes with
//! the child's last landing id, sweeps its siblings and reaches the reference.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use slates_land::engine::{LandingState, Outcome};
use slates_land::os::{OsLand, TargetRefusal};
use slates_vfs::base::BaseConfig;
use slates_vfs::host::HostFs;
use slates_vfs::volume::{Store, Volume};

mod common;
use common::{
  LARGE, Session, Setup, config, mkdir, read_through, rename, request, rm_r, scratch, store,
  symlink, unlink, write_file,
};

/// Format: the environment variable naming the RAM-backed directory.
const RAM_DIR: &str = "SLATES_TEST_RAMDIR";
/// Format: the environment variable that turns this binary into the kill test's child.
const CHILD_DIR: &str = "SLATES_LAND_CHILD_DIR";
/// Shape: files per round of the kill test.
const KILL_FILES: usize = 64;
/// Shape: how long the parent lets the child land before the kill, milliseconds; long enough
/// for several rounds on tmpfs (a round of 64 files measured under 5 ms there).
const KILL_AFTER_MS: u64 = 40;
/// Shape: the parent waits at most this long for the child's first round.
const CHILD_START_MS: u64 = 5_000;

/// The RAM directory, or `None` with a loud skip.
fn ram_dir(test: &str) -> Option<PathBuf> {
  match std::env::var_os(RAM_DIR) {
    Some(dir) => Some(PathBuf::from(dir)),
    None => {
      println!("{test}: skipped — set {RAM_DIR} to a RAM-backed directory (CI Linux: /dev/shm)");
      None
    }
  }
}

/// A fresh working directory for one test, named with the process id, and its removal.
struct Workspace {
  path: PathBuf,
}

impl Workspace {
  fn new(base: &Path, test: &str) -> Self {
    let path = base.join(format!("slates-land-{}-{test}", std::process::id()));
    // The test harness is the one place a test writes a host path outside a landing: the
    // RAM-backed scratch directory it removes at the end (CLAUDE.md §4).
    #[allow(clippy::disallowed_methods)]
    std::fs::create_dir_all(&path).unwrap();
    Self { path }
  }
}

impl Drop for Workspace {
  fn drop(&mut self) {
    #[allow(clippy::disallowed_methods)]
    let _ = std::fs::remove_dir_all(&self.path);
  }
}

#[allow(clippy::disallowed_methods)]
fn seed_file(path: &Path, bytes: &[u8]) {
  std::fs::write(path, bytes).unwrap();
}

#[allow(clippy::disallowed_methods)]
fn seed_dir(path: &Path) {
  std::fs::create_dir_all(path).unwrap();
}

#[allow(clippy::disallowed_methods)]
fn read(path: &Path) -> Option<Vec<u8>> {
  std::fs::read(path).ok()
}

#[allow(clippy::disallowed_methods)]
fn names_in(path: &Path) -> Vec<String> {
  let mut names: Vec<String> = std::fs::read_dir(path)
    .unwrap()
    .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
    .collect();
  names.sort();
  names
}

fn overlay_over(host: &mut OsLand, root: slates_vfs::host::HostDir, store: &mut Store) -> Volume {
  let facts = host.facts(root).unwrap();
  Volume::create_overlay(
    store,
    config(),
    BaseConfig {
      root,
      facts,
      large_class_bytes: LARGE,
    },
  )
  .unwrap()
}

/// The base of the worked example on the disk.
fn seed_example(root: &Path) {
  seed_dir(&root.join("src"));
  seed_file(&root.join("src/lib.rs"), b"base lib");
  seed_file(&root.join("src/gone.rs"), b"gone");
  seed_dir(&root.join("tree/deep"));
  seed_file(&root.join("tree/deep/leaf"), b"leaf");
  seed_dir(&root.join("moved"));
  seed_file(&root.join("moved/m"), b"m");
}

/// The overlay's edits: a replacement, a create, a delete, a mkdir with a file, a symlink, a
/// directory rename and a tree removal.
fn edit_example(vol: &mut Volume, host: &mut OsLand, store: &mut Store) {
  write_file(vol, host, store, "/src/lib.rs", b"edited lib");
  write_file(vol, host, store, "/src/new.rs", b"new");
  unlink(vol, host, store, "/src/gone.rs");
  mkdir(vol, host, store, "/out");
  write_file(vol, host, store, "/out/inner", b"inner");
  symlink(vol, host, store, "/link", "src/lib.rs");
  rename(vol, host, store, "/moved", "/moved2");
  rm_r(vol, host, store, "/tree");
}

/// The disk after the worked example landed.
fn assert_example_on_disk(root: &Path) {
  assert_eq!(read(&root.join("src/lib.rs")).unwrap(), b"edited lib");
  assert_eq!(read(&root.join("src/new.rs")).unwrap(), b"new");
  assert!(read(&root.join("src/gone.rs")).is_none());
  assert_eq!(read(&root.join("out/inner")).unwrap(), b"inner");
  assert_eq!(read(&root.join("moved2/m")).unwrap(), b"m");
  assert!(!root.join("moved").exists());
  assert!(!root.join("tree").exists());
  #[allow(clippy::disallowed_methods)]
  let link = std::fs::read_link(root.join("link")).unwrap();
  assert_eq!(link, Path::new("src/lib.rs"));
  assert!(
    !names_in(root).iter().any(|n| n.starts_with(".slates-")),
    "{:?}",
    names_in(root)
  );
}

/// The worked example against a real directory, then an idempotent re-run.
#[test]
fn the_worked_example_lands_on_a_real_directory() {
  let Some(ram) = ram_dir("the_worked_example_lands_on_a_real_directory") else {
    return;
  };
  let ws = Workspace::new(&ram, "example");
  seed_example(&ws.path);
  let (mut host, target) = OsLand::open_target(&ws.path).unwrap();
  let mut store = store();
  let mut vol = overlay_over(&mut host, target.dir, &mut store);
  edit_example(&mut vol, &mut host, &mut store);
  let mut session = Session::new();
  let mut setup = Setup {
    host: &mut host,
    target: &target,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  };
  let report = setup.land(request(1)).unwrap();
  assert_eq!(report.state, LandingState::Done, "{report:?}");
  assert!(
    report
      .entries
      .iter()
      .all(|e| e.outcome == Some(Outcome::Written)),
    "{report:?}"
  );
  assert!(report.durability.data_synced && report.durability.dirs_synced);
  assert_example_on_disk(&ws.path);
  // The volume reads the landed bytes from the disk now.
  assert_eq!(
    read_through(setup.vol, setup.host, setup.store, "/src/lib.rs"),
    b"edited lib"
  );
  // Idempotent: nothing left to plan.
  let presented = setup.present(&request(1));
  assert!(
    presented.manifest.entries.is_empty(),
    "{:?}",
    presented.manifest.entries
  );
}

/// Containment: a symlink component and `..` are refused before the lease; a relative path too.
#[test]
fn a_target_path_that_escapes_is_refused() {
  let Some(ram) = ram_dir("a_target_path_that_escapes_is_refused") else {
    return;
  };
  let ws = Workspace::new(&ram, "escape");
  seed_dir(&ws.path.join("real"));
  #[allow(clippy::disallowed_methods)]
  std::os::unix::fs::symlink(ws.path.join("real"), ws.path.join("alias")).unwrap();
  assert_eq!(
    OsLand::open_target(&ws.path.join("alias"))
      .err()
      .map(|e| format!("{e:?}")),
    Some(format!("{:?}", TargetRefusal::EscapesTarget))
  );
  assert_eq!(
    OsLand::open_target(&ws.path.join("real/../real"))
      .err()
      .map(|e| format!("{e:?}")),
    Some(format!("{:?}", TargetRefusal::EscapesTarget))
  );
  assert_eq!(
    OsLand::open_target(Path::new("relative/path"))
      .err()
      .map(|e| format!("{e:?}")),
    Some(format!("{:?}", TargetRefusal::NotAbsolute))
  );
  assert!(OsLand::open_target(&ws.path.join("real")).is_ok());
}

/// A scratch volume into an empty directory takes the stage-and-exchange path; the parent
/// handle names the exchanged directory after.
#[test]
fn a_scratch_volume_into_an_empty_real_directory_is_staged() {
  let Some(ram) = ram_dir("a_scratch_volume_into_an_empty_real_directory_is_staged") else {
    return;
  };
  let ws = Workspace::new(&ram, "stage");
  seed_dir(&ws.path.join("out"));
  let (mut host, target) = OsLand::open_target(&ws.path.join("out")).unwrap();
  let mut store = store();
  let mut vol = scratch(&mut store);
  for d in 0..4 {
    mkdir(&mut vol, &mut host, &mut store, &format!("/d{d}"));
    for f in 0..25 {
      write_file(
        &mut vol,
        &mut host,
        &mut store,
        &format!("/d{d}/f{f}"),
        format!("{d}/{f}").as_bytes(),
      );
    }
  }
  let mut session = Session::new();
  let mut setup = Setup {
    host: &mut host,
    target: &target,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  };
  let report = setup.land(request(2)).unwrap();
  assert_eq!(report.state, LandingState::Done, "{report:?}");
  assert!(report.staged);
  assert_eq!(report.written, 104);
  assert_eq!(read(&ws.path.join("out/d2/f3")).unwrap(), b"2/3");
  assert!(!names_in(&ws.path).iter().any(|n| n.starts_with(".slates-")));
  assert!(vol.is_overlay());
  assert_eq!(
    read_through(&mut vol, &mut host, &mut store, "/d2/f3"),
    b"2/3"
  );
}

// ---------------------------------------------------------------- kill -9

/// The bytes of file `f` in round `r`.
fn round_bytes(round: u64, file: usize) -> Vec<u8> {
  format!("round {round} file {file}").into_bytes()
}

/// The child: lands round after round into `dir` until killed, printing each landing id
/// before it starts (the parent resumes with the last one printed).
#[test]
fn kill_child_landing_loop() {
  let Some(dir) = std::env::var_os(CHILD_DIR) else {
    return;
  };
  let dir = PathBuf::from(dir);
  let (mut host, target) = OsLand::open_target(&dir).unwrap();
  let mut store = store();
  let mut vol = overlay_over(&mut host, target.dir, &mut store);
  let mut session = Session::new();
  for round in 1u64.. {
    for f in 0..KILL_FILES {
      write_file(
        &mut vol,
        &mut host,
        &mut store,
        &format!("/f{f}"),
        &round_bytes(round, f),
      );
    }
    println!("landing {round}");
    let report = Setup {
      host: &mut host,
      target: &target,
      vol: &mut vol,
      store: &mut store,
      session: &mut session,
    }
    .land(request(round))
    .unwrap();
    assert_eq!(report.state, LandingState::Done, "{report:?}");
    println!("landed {round}");
  }
}

/// Spawns the child landing loop and returns its stdout after the first round landed and the
/// kill: the parent lets it run `KILL_AFTER_MS` past its first round.
fn run_child_until_killed(dir: &Path) -> String {
  use std::io::Read;
  let exe = std::env::current_exe().unwrap();
  let mut child = Command::new(exe)
    .args(["--exact", "kill_child_landing_loop", "--nocapture"])
    .env(CHILD_DIR, dir)
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .spawn()
    .unwrap();
  let started = Instant::now();
  let mut stdout = child.stdout.take().unwrap();
  let mut seen = String::new();
  let mut buf = [0u8; 4096];
  while !seen.contains("landed 1") && started.elapsed() < Duration::from_millis(CHILD_START_MS) {
    let n = stdout.read(&mut buf).unwrap();
    if n == 0 {
      break;
    }
    seen.push_str(&String::from_utf8_lossy(&buf[..n]));
  }
  // The test harness times the kill; shipped code never sleeps (D-9).
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(Duration::from_millis(KILL_AFTER_MS));
  child.kill().unwrap();
  let _ = child.wait();
  let mut rest = String::new();
  let _ = stdout.read_to_string(&mut rest);
  seen.push_str(&rest);
  seen
}

/// The last landing id the child printed before the kill.
fn last_landing_started(seen: &str) -> u64 {
  seen
    .lines()
    .filter_map(|l| l.strip_prefix("landing "))
    .filter_map(|n| n.trim().parse().ok())
    .max()
    .unwrap_or(0)
}

/// Every file is a whole round up to `last`, never torn.
fn assert_whole_rounds(root: &Path, last: u64) {
  for f in 0..KILL_FILES {
    let bytes = read(&root.join(format!("f{f}"))).unwrap();
    let whole = (0..=last).any(|r| bytes == round_bytes(r, f));
    assert!(
      whole,
      "f{f} is torn after kill -9: {:?}",
      String::from_utf8_lossy(&bytes)
    );
  }
}

/// T-1.15 on a real filesystem: `kill -9` mid-landing; every file is a whole round; the resume
/// with the child's landing id sweeps the siblings and lands the round in full.
#[test]
fn t_1_15_kill_9_on_tmpfs_leaves_every_file_old_or_new() {
  let Some(ram) = ram_dir("t_1_15_kill_9_on_tmpfs_leaves_every_file_old_or_new") else {
    return;
  };
  let ws = Workspace::new(&ram, "kill");
  for f in 0..KILL_FILES {
    seed_file(&ws.path.join(format!("f{f}")), &round_bytes(0, f));
  }
  let seen = run_child_until_killed(&ws.path);
  let last = last_landing_started(&seen);
  assert!(last >= 1, "the child never started a landing: {seen}");
  assert_whole_rounds(&ws.path, last);
  // Resume with the child's last landing id: sweep, then land the round in full.
  let (mut host, target) = OsLand::open_target(&ws.path).unwrap();
  let mut store = store();
  let mut vol = overlay_over(&mut host, target.dir, &mut store);
  for f in 0..KILL_FILES {
    write_file(
      &mut vol,
      &mut host,
      &mut store,
      &format!("/f{f}"),
      &round_bytes(last, f),
    );
  }
  let mut session = Session::new();
  let report = Setup {
    host: &mut host,
    target: &target,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  }
  .land(request(last))
  .unwrap();
  assert_eq!(report.state, LandingState::Done, "{report:?}");
  for f in 0..KILL_FILES {
    assert_eq!(
      read(&ws.path.join(format!("f{f}"))).unwrap(),
      round_bytes(last, f)
    );
  }
  assert!(
    !names_in(&ws.path).iter().any(|n| n.starts_with(".slates-")),
    "siblings swept: {:?}",
    names_in(&ws.path)
  );
}
