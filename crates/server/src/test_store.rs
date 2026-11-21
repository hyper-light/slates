//! The one store-backed test daemon's store (A-767; main's ruling of 2026-11-20, inbox w-svn-from-main-c2-rulings.md
//! decision 1: a daemon without `--vcs-store` keeps no repository's history, so every test of a tool's history runs over
//! a store, made here and nowhere else). The store's directory is a real one beside the test binary in the build output,
//! named with the test's name and the process id, never `/tmp` (R1), and removed when the test's [`TestStore`] drops
//! (CLAUDE.md §4: tests never leave their directory behind).

use std::path::{Path, PathBuf};

use slates_land::store_root::{StoreGrant, StoreRoot};
use slates_store::owner::{StoreHandle, StoreOwner};
use slates_store::store::Sizing;

/// Shape: the owner's waiting requests, as the daemon's (`slates daemon --vcs-store`) holds them.
const WAITING: usize = 16;

/// A test's store: its directory and the owner serving it, stopped and the directory removed on drop.
pub(crate) struct TestStore {
  /// The directory.
  dir: PathBuf,
  /// The owner, until stopped.
  owner: Option<StoreOwner>,
}

impl TestStore {
  /// A fresh store for the test `name`: its directory made (emptied first) beside the test binary, and an owner started
  /// over it under the directory's standing grant.
  #[allow(clippy::disallowed_methods)] // test code: its own scratch directory
  pub(crate) fn start(name: &str) -> TestStore {
    let binary = std::env::current_exe().unwrap();
    let dir = binary
      .parent()
      .unwrap()
      .join(format!("vcs-store-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    let owner = StoreOwner::start(root(&dir), sizing(), WAITING).unwrap();
    TestStore {
      dir,
      owner: Some(owner),
    }
  }

  /// The handle a daemon's configuration holds (`DaemonConfig::vcs_disk`).
  pub(crate) fn handle(&self) -> StoreHandle {
    self.owner.as_ref().unwrap().handle()
  }

  /// The store's directory.
  pub(crate) fn dir(&self) -> &Path {
    &self.dir
  }

  /// Stops the owner, so the store can be read back from its files ([`root`], [`sizing`]).
  pub(crate) fn stop(&mut self) {
    if let Some(owner) = self.owner.take() {
      owner.stop().unwrap();
    }
  }

  /// Starts an owner over the same directory again (a node's restart), stopping the one serving first.
  pub(crate) fn restart(&mut self) {
    self.stop();
    self.owner = Some(StoreOwner::start(root(&self.dir), sizing(), WAITING).unwrap());
  }
}

impl Drop for TestStore {
  #[allow(clippy::disallowed_methods)] // test code: its own scratch directory
  fn drop(&mut self) {
    if let Some(owner) = self.owner.take() {
      let _ = owner.stop();
    }
    let _ = std::fs::remove_dir_all(&self.dir);
  }
}

/// The store root over `dir`, opened under the directory's standing grant.
pub(crate) fn root(dir: &Path) -> StoreRoot {
  let grant = StoreGrant {
    repository: 0,
    target: StoreRoot::identify(dir).unwrap(),
    issued_ns: 0,
  };
  StoreRoot::open(dir, &grant).unwrap()
}

/// Shape: a test's store as the daemon sizes one (a memtable of one trunk node: eight 64 KiB extents), 1 GiB of disk.
pub(crate) fn sizing() -> Sizing {
  Sizing {
    memory: 512 << 10,
    disk: 1 << 30,
    extent_bytes: 64 << 10,
    fanout: 8,
  }
}
