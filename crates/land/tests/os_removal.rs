//! Removal under outsider interference over a real directory (§4.15 step 6; D-26; AUD-29-04): the removal
//! oracle of `tests/removal.rs`, held to the same rules (`common::removal`), with the operating system as the
//! disk — so the rename that replaces nothing (`renameat2(RENAME_NOREPLACE)` on Linux, `renameatx_np(
//! RENAME_EXCL)` on macOS), its `EEXIST`, the unfollowed `statat` of a moved entry and the exchange are the
//! kernel's, not the simulation's. The directory is the RAM-backed one `SLATES_TEST_RAMDIR` names (CI Linux:
//! `/dev/shm`); without it every test here skips loudly and passes. Nothing is written outside it.
//!
//! The OS writer sits behind [`Interfering`], which delegates every seam call, counts it, and makes an armed
//! outsider's save — its bytes written beside the target, then renamed over the interfered path (a directory
//! there removed first) — just before the call it was armed for. It withholds the exchange from the engine
//! in the fallback's histories, so tmpfs (which has one) runs the fallback too.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg(unix)]

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use slates_land::engine::{LandingRefusal, LandingReport, LandingRequest, Unobserved};
use slates_land::grant::{GrantScope, Surface};
use slates_land::os::OsLand;
use slates_vfs::base::BaseConfig;
use slates_vfs::host::{
  BaseEntry, Hint, HostDir, HostError, HostFacts, HostFile, HostFs, LandCapabilities, LandFs,
  WatchState,
};
use slates_vfs::inode::Fingerprint;

mod common;
use common::removal::{
  Fired, LATER_OUTSIDER, OUTSIDER, REMOVALS, Removal, Seen, assert_reference, judge,
};
use common::{
  LARGE, Session, Setup, TERM_NS, config, mkdir, rename, request, rm_r, store, unlink, write_file,
};

/// Format: the environment variable naming the RAM-backed directory.
const RAM_DIR: &str = "SLATES_TEST_RAMDIR";

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

/// A fresh working directory for one history, named with the process id, removed on drop.
struct Workspace {
  path: PathBuf,
}

impl Workspace {
  fn new(base: &Path, name: &str) -> Self {
    let path = base.join(format!("slates-os-removal-{}-{name}", std::process::id()));
    // The test harness is the one place a test writes a host path outside a landing: the RAM-backed
    // scratch directory it removes at the end (CLAUDE.md §4).
    #[allow(clippy::disallowed_methods)]
    std::fs::create_dir_all(path.join("target")).unwrap();
    #[allow(clippy::disallowed_methods)]
    std::fs::create_dir_all(path.join("outsider")).unwrap();
    Self { path }
  }

  fn target(&self) -> PathBuf {
    self.path.join("target")
  }
}

impl Drop for Workspace {
  fn drop(&mut self) {
    #[allow(clippy::disallowed_methods)]
    let _ = std::fs::remove_dir_all(&self.path);
  }
}

/// The OS writer with an outsider in front of it (the module doc).
struct Interfering {
  inner: OsLand,
  calls: u64,
  /// Outsider saves armed for later calls: the call each fires at, and its bytes.
  armed: Vec<(u64, Vec<u8>)>,
  fired: Vec<Fired>,
  /// The path the outsider saves over.
  path: PathBuf,
  /// Where the outsider writes its bytes before renaming them over `path` (same filesystem).
  outsider_dir: PathBuf,
  /// Whether the engine may see the exchange.
  exchange: bool,
}

impl Interfering {
  fn arm(&mut self, n: u64, bytes: &[u8]) {
    self.armed.push((
      self.calls.saturating_add(n).saturating_add(1),
      bytes.to_vec(),
    ));
  }

  fn tick(&mut self) {
    self.calls += 1;
    let now = self.calls;
    let due: Vec<Vec<u8>> = self
      .armed
      .extract_if(.., |(at, _)| *at == now)
      .map(|(_, bytes)| bytes)
      .collect();
    for bytes in due {
      let fired = self.outsider_save(&bytes);
      self.fired.push(fired);
    }
  }

  /// The outsider's save: its bytes beside the target, then renamed over the path (a directory there is
  /// removed first; a real rename cannot replace one with a file).
  #[allow(clippy::disallowed_methods)]
  fn outsider_save(&mut self, bytes: &[u8]) -> Fired {
    let temp = self.outsider_dir.join(format!("save-{}", self.fired.len()));
    std::fs::write(&temp, bytes).unwrap();
    let created = std::fs::symlink_metadata(&temp).unwrap().ino();
    let before = std::fs::symlink_metadata(&self.path).ok();
    if before.as_ref().is_some_and(std::fs::Metadata::is_dir) {
      std::fs::remove_dir_all(&self.path).unwrap();
    }
    std::fs::rename(&temp, &self.path).unwrap();
    Fired {
      created,
      displaced: before.map(|m| m.ino()),
    }
  }
}

impl HostFs for Interfering {
  fn facts(&mut self, dir: HostDir) -> Result<HostFacts, HostError> {
    self.tick();
    self.inner.facts(dir)
  }
  fn fingerprint_dir(&mut self, dir: HostDir) -> Result<Fingerprint, HostError> {
    self.tick();
    self.inner.fingerprint_dir(dir)
  }
  fn list(&mut self, dir: HostDir) -> Result<Vec<BaseEntry>, HostError> {
    self.tick();
    self.inner.list(dir)
  }
  fn open_dir(&mut self, parent: HostDir, name: &str) -> Result<HostDir, HostError> {
    self.tick();
    self.inner.open_dir(parent, name)
  }
  fn open_file(&mut self, dir: HostDir, name: &str) -> Result<HostFile, HostError> {
    self.tick();
    self.inner.open_file(dir, name)
  }
  fn fstat(&mut self, file: HostFile) -> Result<Fingerprint, HostError> {
    self.tick();
    self.inner.fstat(file)
  }
  fn read_at(&mut self, file: HostFile, off: u64, buf: &mut [u8]) -> Result<usize, HostError> {
    self.tick();
    self.inner.read_at(file, off, buf)
  }
  fn read_link(&mut self, dir: HostDir, name: &str) -> Result<Box<str>, HostError> {
    self.tick();
    self.inner.read_link(dir, name)
  }
  fn close_file(&mut self, file: HostFile) {
    self.tick();
    self.inner.close_file(file);
  }
  fn close_dir(&mut self, dir: HostDir) {
    self.tick();
    self.inner.close_dir(dir);
  }
  fn watch(&mut self, dir: HostDir) -> WatchState {
    self.tick();
    self.inner.watch(dir)
  }
  fn hints(&mut self) -> Vec<Hint> {
    self.tick();
    self.inner.hints()
  }
  fn now_ns(&mut self) -> i64 {
    self.tick();
    self.inner.now_ns()
  }
}

impl LandFs for Interfering {
  fn capabilities(&mut self, dir: HostDir) -> Result<LandCapabilities, HostError> {
    self.tick();
    let caps = self.inner.capabilities(dir)?;
    Ok(LandCapabilities {
      exchange: caps.exchange && self.exchange,
      ..caps
    })
  }
  fn create_temp(&mut self, dir: HostDir, name: &str) -> Result<HostFile, HostError> {
    self.tick();
    self.inner.create_temp(dir, name)
  }
  fn write_at(&mut self, file: HostFile, off: u64, bytes: &[u8]) -> Result<(), HostError> {
    self.tick();
    self.inner.write_at(file, off, bytes)
  }
  fn sync_file(&mut self, file: HostFile) -> Result<(), HostError> {
    self.tick();
    self.inner.sync_file(file)
  }
  fn set_mode(&mut self, file: HostFile, mode: u32) -> Result<(), HostError> {
    self.tick();
    self.inner.set_mode(file, mode)
  }
  fn set_mtime(&mut self, file: HostFile, mtime_ns: i64) -> Result<(), HostError> {
    self.tick();
    self.inner.set_mtime(file, mtime_ns)
  }
  fn place(&mut self, file: HostFile, dir: HostDir, name: &str) -> Result<(), HostError> {
    self.tick();
    self.inner.place(file, dir, name)
  }
  fn exchange(&mut self, dir: HostDir, a: &str, b: &str) -> Result<(), HostError> {
    self.tick();
    self.inner.exchange(dir, a, b)
  }
  fn rename_noreplace(
    &mut self,
    dir: HostDir,
    from: &str,
    to_dir: HostDir,
    to: &str,
  ) -> Result<(), HostError> {
    self.tick();
    self.inner.rename_noreplace(dir, from, to_dir, to)
  }
  fn entry_fingerprint(&mut self, dir: HostDir, name: &str) -> Result<Fingerprint, HostError> {
    self.tick();
    self.inner.entry_fingerprint(dir, name)
  }
  fn unlink(&mut self, dir: HostDir, name: &str) -> Result<(), HostError> {
    self.tick();
    self.inner.unlink(dir, name)
  }
  fn mkdir(&mut self, dir: HostDir, name: &str, mode: u32) -> Result<(), HostError> {
    self.tick();
    self.inner.mkdir(dir, name, mode)
  }
  fn rmdir(&mut self, dir: HostDir, name: &str) -> Result<(), HostError> {
    self.tick();
    self.inner.rmdir(dir, name)
  }
  fn symlink(&mut self, dir: HostDir, name: &str, target: &str) -> Result<(), HostError> {
    self.tick();
    self.inner.symlink(dir, name, target)
  }
  fn sync_dir(&mut self, dir: HostDir) -> Result<(), HostError> {
    self.tick();
    self.inner.sync_dir(dir)
  }
  fn sync_media(&mut self, dir: HostDir) -> Result<(), HostError> {
    self.tick();
    self.inner.sync_media(dir)
  }
}

/// Shape: how long ago the seeded base file was last modified: far outside any racy window (§4.5: the
/// filesystem's timestamp granularity; FAT's 2 s is the widest in the design's table), as a file that has
/// lived on the disk for a while is. A file seeded just now would be witnessed racy or not by timing, and
/// the engine's seam calls (the racy witness is hashed) would differ from one run to the next.
const SEED_AGE: Duration = Duration::from_secs(3_600);

/// The base entry at `/doomed` on the disk, for `removal`.
#[allow(clippy::disallowed_methods)]
fn seed(removal: Removal, target: &Path) {
  let doomed = target.join("doomed");
  match removal {
    Removal::File | Removal::Replace { .. } => {
      std::fs::write(&doomed, b"base bytes").unwrap();
      let aged = SystemTime::now().checked_sub(SEED_AGE).unwrap();
      std::fs::File::options()
        .write(true)
        .open(&doomed)
        .unwrap()
        .set_modified(aged)
        .unwrap();
    }
    Removal::Symlink => std::os::unix::fs::symlink("somewhere", &doomed).unwrap(),
    Removal::Directory | Removal::Clear { .. } | Removal::Rename | Removal::RenameOnto => {
      std::fs::create_dir(&doomed).unwrap();
      std::fs::write(doomed.join("inner"), b"base inner").unwrap();
    }
  }
}

/// Every entry under `target` (unfollowed), by inode, with its path from the target (`/doomed`).
#[allow(clippy::disallowed_methods)]
fn inodes(target: &Path) -> BTreeMap<u64, String> {
  let mut out = BTreeMap::new();
  let mut stack = vec![(target.to_path_buf(), String::new())];
  while let Some((dir, prefix)) = stack.pop() {
    for entry in std::fs::read_dir(&dir).unwrap() {
      let entry = entry.unwrap();
      let name = entry.file_name().to_string_lossy().into_owned();
      let meta = std::fs::symlink_metadata(entry.path()).unwrap();
      let path = format!("{prefix}/{name}");
      if meta.is_dir() {
        stack.push((entry.path(), path.clone()));
      }
      out.insert(meta.ino(), path);
    }
  }
  out
}

/// One landed history over the real directory.
struct Run {
  result: Result<LandingReport, LandingRefusal>,
  calls: u64,
  witnessed: u64,
  fired: Vec<Fired>,
  all_fired: bool,
  inodes: BTreeMap<u64, String>,
}

impl Run {
  fn seen(&self) -> Seen<'_> {
    Seen {
      all_fired: self.all_fired,
      fired: self.fired.clone(),
      inodes: self.inodes.clone(),
      witnessed: self.witnessed,
      result: &self.result,
    }
  }
}

/// Seeds `removal`'s base in a fresh workspace, makes the overlay's edit, presents and grants the landing,
/// arms each outsider save `(n, bytes)` before the `n`-th seam call of the granted landing, and lands.
#[allow(clippy::disallowed_methods)]
fn run(ram: &Path, label: &str, removal: Removal, edits: &[(u64, &[u8])]) -> Run {
  let ws = Workspace::new(ram, label);
  let target_path = ws.target();
  seed(removal, &target_path);
  let witnessed = std::fs::symlink_metadata(target_path.join("doomed"))
    .unwrap()
    .ino();
  let (inner, target) = OsLand::open_target(&target_path).unwrap();
  let mut host = Interfering {
    inner,
    calls: 0,
    armed: Vec::new(),
    fired: Vec::new(),
    path: target_path.join(removal.interfered().trim_start_matches('/')),
    outsider_dir: ws.path.join("outsider"),
    exchange: removal.exchange(),
  };
  let mut store = store();
  let facts = host.facts(target.dir).unwrap();
  let mut vol = slates_vfs::volume::Volume::create_overlay(
    &mut store,
    config(),
    BaseConfig {
      root: target.dir,
      facts,
      large_class_bytes: LARGE,
    },
  )
  .unwrap();
  let (v, h, s) = (&mut vol, &mut host, &mut store);
  match removal {
    Removal::File | Removal::Symlink => unlink(v, h, s, "/doomed"),
    Removal::Directory => rm_r(v, h, s, "/doomed"),
    Removal::Replace { .. } => write_file(v, h, s, "/doomed", b"the landing's bytes"),
    Removal::Clear { .. } => {
      rm_r(v, h, s, "/doomed");
      mkdir(v, h, s, "/doomed");
    }
    Removal::Rename | Removal::RenameOnto => rename(v, h, s, "/doomed", "/moved"),
  }
  let mut session = Session::new();
  let presented = Setup {
    host: &mut host,
    target: &target,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  }
  .present(&request(1));
  let granted = LandingRequest {
    grant: session.grants.issue(
      Surface::Cli,
      presented.manifest.hash,
      presented.binding.clone(),
      GrantScope::Once,
      1,
      TERM_NS,
    ),
    ..request(1)
  };
  for (n, bytes) in edits {
    host.arm(*n, bytes);
  }
  let before = host.calls;
  let result = Setup {
    host: &mut host,
    target: &target,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  }
  .try_land(&granted, &mut Unobserved);
  Run {
    calls: host.calls.saturating_sub(before),
    witnessed,
    all_fired: host.armed.is_empty(),
    fired: host.fired.clone(),
    inodes: inodes(&target_path),
    result,
  }
}

/// AUD-29-04 over a real directory. Do: for every kind of `common::removal`, land it once to count its seam
/// calls, then once per call with a real outsider save over the name just before that call, then once per
/// pair of calls with two saves. Expect: every history keeps `common::removal`'s rules with the kernel's own
/// renames, stats and exchange behind the engine, and across each kind's single-save histories both a stale
/// entry and a written one occurred (neither rule passes vacuously).
#[test]
fn a_real_outsider_save_at_any_seam_call_survives_every_removal() {
  let Some(ram) = ram_dir("a_real_outsider_save_at_any_seam_call_survives_every_removal") else {
    return;
  };
  for (kind, removal) in REMOVALS.into_iter().enumerate() {
    let reference = run(&ram, &format!("{kind}-ref"), removal, &[]);
    assert_reference(removal, &reference.seen());
    let (mut stale, mut written) = (0u64, 0u64);
    for n in 0..reference.calls {
      let one = run(&ram, &format!("{kind}-{n}"), removal, &[(n, OUTSIDER)]);
      let verdict = judge(removal, &format!("{removal:?} at call {n}"), &one.seen());
      stale += u64::from(verdict.stale);
      written += u64::from(verdict.written);
      for second in n.saturating_add(1)..one.calls {
        let two = run(
          &ram,
          &format!("{kind}-{n}-{second}"),
          removal,
          &[(n, OUTSIDER), (second, LATER_OUTSIDER)],
        );
        judge(
          removal,
          &format!("{removal:?} at calls {n} and {second}"),
          &two.seen(),
        );
      }
    }
    assert!(
      written > 0 && (stale > 0 || matches!(removal, Removal::RenameOnto)),
      "{removal:?}: {stale} stale and {written} written histories of {}",
      reference.calls
    );
  }
}
