//! The store owner tests' harness: a scratch directory, an owner of a test's store in it, and a runtime of one shard
//! to ask it from. Each measuring test lives in a binary of its own, since the allocator's counters are the process's.

use std::path::{Path, PathBuf};

use slates_land::store_root::{StoreGrant, StoreRoot};
use slates_rt::runtime::{LocalRuntime, RuntimeConfig};
use slates_store::owner::StoreOwner;
use slates_store::store::Sizing;

/// A fresh scratch directory for test `name` of binary `binary`.
pub(crate) fn scratch(binary: &str, name: &str) -> PathBuf {
  let dir =
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{binary}-{name}-{}", std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).unwrap();
  dir.canonicalize().unwrap()
}

/// The owner of a test's store in `dir`, sized as the daemon sizes one.
pub(crate) fn start(dir: &Path) -> StoreOwner {
  let grant = StoreGrant {
    repository: 1,
    target: StoreRoot::identify(dir).unwrap(),
    issued_ns: 0,
  };
  let sizing = Sizing {
    memory: 512 << 10,
    disk: 1 << 30,
    extent_bytes: 64 << 10,
    fanout: 8,
  };
  StoreOwner::start(StoreRoot::open(dir, &grant).unwrap(), sizing, (16, 0)).unwrap()
}

/// Shape: one shard, a few tasks.
pub(crate) fn runtime() -> LocalRuntime {
  LocalRuntime::new(&RuntimeConfig {
    shards: 1,
    tasks_per_shard: 8,
    timers_per_shard: 8,
    ring_entries: 64,
    step_budget_ns: 1_000_000,
    timer_tick_ns: 100_000,
    batch: 8,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
    wake_tracking: None,
  })
  .unwrap()
}
