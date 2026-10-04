//! Escapes from a granted landing under an outsider's link swaps (§4.15 step 6, §4.13; R1, goal condition 4,
//! 2026-10-04): a landing writes only beneath its granted target, whatever an outsider does to the target while
//! it runs. Before the `n`-th seam call of a real landing, for every `n` the landing reaches, an outsider swaps in
//! one of the links an attacker on the host would use to redirect a write:
//! - the directory the landing writes into, replaced by a symlink to a directory outside the target;
//! - a file the landing overwrites, replaced by a symlink to a file outside;
//! - that file replaced by a hard link to the file outside (a write in place would change the outside file).
//!
//! The landing may land, conflict or refuse. What must hold on every history: the outside directory is exactly
//! as it was — the same entries, the same inodes, the same bytes and modification times — and no entry of the
//! target names an outside inode except the outsider's own link. The directory is a real one in the build output
//! (`CARGO_TARGET_TMPDIR`; A-50), removed at the end.

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
use common::{LARGE, Session, Setup, TERM_NS, config, mkdir, request, store, write_file};

/// Shape: how long ago the seeded files were last modified: outside any racy window (§4.5), as in
/// `os_removal.rs`, so the engine's seam calls do not vary with the clock.
const SEED_AGE: Duration = Duration::from_secs(3_600);
/// Format: the bytes of the file outside the target the attacker aims a write at.
const SECRET: &[u8] = b"the file outside the target";

/// The link an outsider swaps in.
#[derive(Clone, Copy, Debug)]
enum Swap {
  /// `target/dir` becomes a symlink to `outside/`.
  DirectorySymlink,
  /// `target/dir/existing` becomes a symlink to `outside/secret`.
  FileSymlink,
  /// `target/dir/existing` becomes a hard link to `outside/secret`.
  FileHardlink,
}

const SWAPS: [Swap; 3] = [
  Swap::DirectorySymlink,
  Swap::FileSymlink,
  Swap::FileHardlink,
];

/// A fresh working directory, removed on drop.
struct Workspace {
  path: PathBuf,
}

impl Workspace {
  #[allow(clippy::disallowed_methods)]
  fn new(name: &str) -> Self {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
      .join(format!("slates-land-escape-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(path.join("target/dir")).unwrap();
    std::fs::create_dir_all(path.join("outside")).unwrap();
    let aged = SystemTime::now().checked_sub(SEED_AGE).unwrap();
    for (file, bytes) in [
      (
        path.join("target/dir/existing"),
        b"the base's bytes".as_slice(),
      ),
      (path.join("outside/secret"), SECRET),
    ] {
      std::fs::write(&file, bytes).unwrap();
      std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(aged)
        .unwrap();
    }
    Workspace { path }
  }
}

impl Drop for Workspace {
  fn drop(&mut self) {
    #[allow(clippy::disallowed_methods)]
    let _ = std::fs::remove_dir_all(&self.path);
  }
}

/// A directory's entries (unfollowed) by path: inode, size, mtime and, for a file, bytes.
type Listing = BTreeMap<String, (u64, u64, i64, Vec<u8>)>;

/// One landing history: its result, whether the outsider struck, and the outside directory before and after.
type History = (
  Result<LandingReport, LandingRefusal>,
  bool,
  Listing,
  Listing,
);

/// Every entry under `dir` (unfollowed): its path, inode, size, mtime and, for a file, bytes.
#[allow(clippy::disallowed_methods)]
fn snapshot(dir: &Path) -> Listing {
  let mut out = BTreeMap::new();
  let mut stack = vec![(dir.to_path_buf(), String::new())];
  while let Some((at, prefix)) = stack.pop() {
    for entry in std::fs::read_dir(&at).unwrap() {
      let entry = entry.unwrap();
      let meta = std::fs::symlink_metadata(entry.path()).unwrap();
      let name = format!("{prefix}/{}", entry.file_name().to_string_lossy());
      let bytes = if meta.is_file() {
        std::fs::read(entry.path()).unwrap()
      } else {
        Vec::new()
      };
      if meta.is_dir() {
        stack.push((entry.path(), name.clone()));
      }
      out.insert(name, (meta.ino(), meta.size(), meta.mtime(), bytes));
    }
  }
  out
}

/// The OS writer with an outsider who swaps a link in before one armed seam call.
struct Interfering {
  inner: OsLand,
  calls: u64,
  armed: Option<u64>,
  fired: bool,
  swap: Swap,
  root: PathBuf,
  exchange: bool,
}

impl Interfering {
  fn tick(&mut self) {
    self.calls += 1;
    if self.armed == Some(self.calls) {
      self.armed = None;
      self.fired = true;
      self.strike();
    }
  }

  #[allow(clippy::disallowed_methods)]
  fn strike(&self) {
    let dir = self.root.join("target/dir");
    let existing = dir.join("existing");
    let outside = self.root.join("outside");
    match self.swap {
      Swap::DirectorySymlink => {
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&dir);
        std::os::unix::fs::symlink(&outside, &dir).unwrap();
      }
      Swap::FileSymlink => {
        if std::fs::symlink_metadata(&dir).is_ok_and(|meta| meta.is_dir()) {
          let _ = std::fs::remove_file(&existing);
          std::os::unix::fs::symlink(outside.join("secret"), &existing).unwrap();
        }
      }
      Swap::FileHardlink => {
        if std::fs::symlink_metadata(&dir).is_ok_and(|meta| meta.is_dir()) {
          let _ = std::fs::remove_file(&existing);
          std::fs::hard_link(outside.join("secret"), &existing).unwrap();
        }
      }
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

/// One landing history: the overlay writes `/dir/existing` and creates `/dir/new` and `/dir/sub/deep`; the
/// outsider swaps `swap` in before seam call `at` (none for the reference). Returns the result, whether the swap
/// fired, and the outside directory after.
#[allow(clippy::disallowed_methods)]
fn run(name: &str, swap: Swap, at: Option<u64>) -> History {
  let ws = Workspace::new(name);
  let before = snapshot(&ws.path.join("outside"));
  let (inner, target) = OsLand::open_target(&ws.path.join("target")).unwrap();
  let mut host = Interfering {
    inner,
    calls: 0,
    armed: None,
    fired: false,
    swap,
    root: ws.path.clone(),
    exchange: true,
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
  {
    let (v, h, s) = (&mut vol, &mut host, &mut store);
    write_file(
      v,
      h,
      s,
      "/dir/existing",
      b"the landing's bytes for existing",
    );
    write_file(v, h, s, "/dir/new", b"the landing's new file");
    mkdir(v, h, s, "/dir/sub");
    write_file(v, h, s, "/dir/sub/deep", b"the landing's deep file");
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
  host.armed = at.map(|n| host.calls.saturating_add(n).saturating_add(1));
  let result = Setup {
    host: &mut host,
    target: &target,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  }
  .try_land(&granted, &mut Unobserved);
  let fired = host.fired;
  let after = snapshot(&ws.path.join("outside"));
  (result, fired, before, after)
}

/// Goal condition 4 (escapes battle-tested), §4.15 step 6, R1. Do: for each link swap, land once per seam call
/// of the granted landing with the outsider striking just before that call, until a landing ends before its
/// armed call. Expect: the outside directory unchanged after every history (entries, inodes, sizes, mtimes,
/// bytes), and every kind of swap to have fired at least once (no rule passes vacuously).
#[test]
fn no_link_an_outsider_swaps_in_mid_landing_redirects_a_write_outside_the_target() {
  for swap in SWAPS {
    let (reference, _, before, after) = run(&format!("{swap:?}-ref"), swap, None);
    assert!(
      reference.is_ok(),
      "{swap:?}: the undisturbed landing lands: {reference:?}"
    );
    assert_eq!(
      before, after,
      "{swap:?}: the reference left the outside alone"
    );
    let mut fired_histories = 0u64;
    for n in 0.. {
      let (result, fired, before, after) = run(&format!("{swap:?}-{n}"), swap, Some(n));
      if !fired {
        break;
      }
      fired_histories += 1;
      assert_eq!(
        before, after,
        "{swap:?} before seam call {n}: the outside changed (landing result {result:?})"
      );
    }
    eprintln!(
      "escape: {swap:?} struck before {fired_histories} seam calls; the outside never changed"
    );
    assert!(
      fired_histories > 0,
      "{swap:?}: the outsider struck at least once"
    );
  }
}
