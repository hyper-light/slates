//! The one shared test support for a node's changeset store (A-767; main's ruling of 2026-11-20, inbox
//! w-svn-from-main-c2-rulings decision 1: "tests move onto store-backed daemons through ONE shared helper"). A
//! [`TestStore`] is a real store in a fresh directory beside the test binary, in the build output, named with the
//! process id, the test's name and a number of its own, never `/tmp` (R1); its owner thread runs while it lives, and
//! dropping it stops the owner and removes the directory (CLAUDE.md §4: removed at the end).
//!
//! **Order.** The owner ends only once every handle is gone, so a test makes its store before the shard state that
//! holds a handle (`state.vcs_disk`) and lets the store go after that state: a store dropped while a shard still holds
//! its handle would wait on it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use slates_land::store_root::{StoreGrant, StoreRoot};
use slates_store::owner::{StoreHandle, StoreOwner};
use slates_store::store::Sizing;

/// Each store's own number within the process, so tests running side by side never share a directory.
static NEXT: AtomicU64 = AtomicU64::new(0);

/// A store for one test (module docs).
pub(crate) struct TestStore {
  /// Its directory.
  dir: PathBuf,
  /// Its owner, until dropped.
  owner: Option<StoreOwner>,
}

impl TestStore {
  /// A fresh store for the test `name`.
  pub(crate) fn new(name: &str) -> TestStore {
    let number = NEXT.fetch_add(1, Ordering::Relaxed);
    let binary = std::env::current_exe().unwrap();
    let dir = binary
      .parent()
      .unwrap()
      .join(format!("vcs-store-{name}-{}-{number}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    let grant = StoreGrant {
      repository: 0,
      target: StoreRoot::identify(&dir).unwrap(),
      issued_ns: 0,
    };
    // Shape: a store as the daemon sizes one (a memtable of one trunk node: eight 64 KiB extents), 1 GiB of disk.
    let sizing = Sizing {
      memory: 512 << 10,
      disk: 1 << 30,
      extent_bytes: 64 << 10,
      fanout: 8,
    };
    let owner = StoreOwner::start(StoreRoot::open(&dir, &grant).unwrap(), sizing, 16).unwrap();
    TestStore {
      dir,
      owner: Some(owner),
    }
  }

  /// A shard's way to the store.
  pub(crate) fn handle(&self) -> StoreHandle {
    self.owner.as_ref().unwrap().handle()
  }
}

impl Drop for TestStore {
  fn drop(&mut self) {
    if let Some(owner) = self.owner.take() {
      if std::thread::panicking() {
        // A test failing on its shard may leave that shard's handle unreleased, and the owner ends only once every
        // handle is gone: joining it here would turn the failure into a hang that hides its message. The owner thread
        // is let go with the failing test's process.
        core::mem::forget(owner);
      } else {
        let _ = owner.stop();
      }
    }
    let _ = std::fs::remove_dir_all(&self.dir);
  }
}

/// Lands `parts` under `header` through `disk` and waits for the store's answer, for a test on a shard: the landing
/// runs on a thread of its own with a runtime of its own, which parks until the store's owner answers (the shard's own
/// thread cannot wait on a wake its runtime would deliver). Whether it landed.
pub(crate) fn land_blocking(
  disk: StoreHandle,
  header: slates_store::log::Header,
  parts: Vec<slates_store::owner::Part>,
) -> bool {
  std::thread::scope(|scope| {
    scope
      .spawn(move || {
        runtime()
          .block_on(async move { disk.land(header, parts).await.is_ok() })
          .unwrap_or(false)
      })
      .join()
      .unwrap_or(false)
  })
}

/// Shape: one shard, a few tasks: the waiting thread's runtime.
fn runtime() -> slates_rt::runtime::LocalRuntime {
  slates_rt::runtime::LocalRuntime::new(&slates_rt::runtime::RuntimeConfig {
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

/// [`crate::daemon::audit_on_shard`] over a daemon holding a fresh [`TestStore`] (`DaemonConfig::vcs_disk`): `history`
/// runs on its shard; the daemon stops, dropping every handle, before the store goes (module docs, "Order").
pub(crate) fn audit_on_stored_shard<T: Send + 'static>(
  name: &str,
  history: impl FnOnce(&mut crate::state::ShardState) -> T + Clone + Send + 'static,
) -> T {
  let store = TestStore::new(name);
  let handle = store.handle();
  let answer = crate::daemon::audit_on_shard_configured(history, move |config| {
    config.vcs_disk = Some(handle);
  });
  drop(store);
  answer
}

/// [`crate::daemon::audit_daemon`] holding `store` (`DaemonConfig::vcs_disk`), for a test whose work outlives one
/// observation: the test stops the daemon, and makes `store` before it so the store goes after (module docs, "Order").
pub(crate) fn audit_daemon_stored(store: &TestStore) -> crate::Daemon {
  let handle = store.handle();
  crate::daemon::audit_daemon_configured(move |config| {
    config.vcs_disk = Some(handle);
  })
}

/// A `CreateRepository` request served on a test's shard as a client's creation is served on a node keeping a store,
/// answered once its changeset 0 is landed and it is recorded (`repository::create_blocking`, A-767 item 7): a test on a
/// store-backed daemon creates its repositories here, since the verb's own creation waits for its landing in a task the
/// test's shard cannot run while the test holds it. On a node with no store, and for any other request, the verb as a
/// client's dispatch serves it.
pub(crate) fn create(
  state: &mut crate::state::ShardState,
  principal: &slates_db::catalog::Principal,
  request: slates_ipc::protocol::RequestBody,
) -> slates_ipc::protocol::ReplyBody {
  let Some(disk) = state.vcs_disk.clone() else {
    return crate::verbs::dispatch(state, 1, principal, request);
  };
  match request {
    slates_ipc::protocol::RequestBody::CreateRepository {
      name,
      tool,
      format,
      default_branch,
      green,
    } => crate::repository::create_blocking(
      state,
      (disk, principal),
      (&name, tool, format, &default_branch),
      green,
    ),
    other => crate::verbs::dispatch(state, 1, principal, other),
  }
}
