//! The node's changeset store as the daemon's tests hold it (A-767; C2 of Ada's close list; main's ruling of 2026-11-20
//! 21:05: "Tests move onto store-backed daemons through ONE shared helper"). A Subversion repository's history lands on
//! the store, and a daemon started without one makes no Subversion repository, so every test that makes one runs on a
//! daemon holding a store from here.
//!
//! **Where.** One directory a store, beside the test binary in the build output, named with the process id and a
//! number of its own (tests run side by side in one process), never `/tmp` (R1); the store's owner started over it.
//! **Removed at the end** (CLAUDE.md §4): the guard [`TestStore`] stops the owner and removes the directory when it
//! drops. Declare it before the daemon that holds its handle, so the daemon drops (and stops) first and the owner, its
//! last handle gone, ends.

use std::path::{Path, PathBuf};

use slates_land::store_root::{StoreGrant, StoreRoot};
use slates_store::owner::{StoreHandle, StoreOwner};
use slates_store::store::Sizing;

/// Shape: a test's store: 8 MiB of memory, 1 GiB of disk, 128 KiB extents, fanout 8.
pub(crate) const SIZING: Sizing = Sizing {
  memory: 8 << 20,
  disk: 1 << 30,
  extent_bytes: 128 << 10,
  fanout: 8,
};

/// Shape: the requests a test's store's owner queues.
const WAITING: usize = 64;

/// A test's store: its directory and its owner, both given back when it drops (module docs).
#[derive(Debug)]
pub(crate) struct TestStore {
  /// The owner, until the guard stops it.
  owner: Option<StoreOwner>,
  /// The directory.
  dir: PathBuf,
}

impl TestStore {
  /// A fresh store named `name`, its owner started.
  pub(crate) fn start(name: &str) -> TestStore {
    TestStore::start_sized(name, SIZING)
  }

  /// A fresh store named `name`, its owner started with `sizing` (one the engine refuses makes a store that cannot open).
  #[allow(clippy::disallowed_methods)] // test code: its own scratch directory
  pub(crate) fn start_sized(name: &str, sizing: Sizing) -> TestStore {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let number = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let binary = std::env::current_exe().expect("the test binary's path");
    let dir = binary
      .parent()
      .expect("the test binary's directory")
      .join(format!("vcs-store-{name}-{}-{number}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the store's directory");
    let dir = dir.canonicalize().expect("the store's directory");
    let owner = StoreOwner::start(root(&dir), sizing, WAITING).expect("the store's owner");
    TestStore {
      owner: Some(owner),
      dir,
    }
  }

  /// The store's handle, as a daemon holds it (`DaemonConfig::vcs_disk`).
  pub(crate) fn handle(&self) -> StoreHandle {
    self
      .owner
      .as_ref()
      .expect("the owner runs until the guard drops")
      .handle()
  }

  /// The store's directory.
  pub(crate) fn dir(&self) -> &Path {
    &self.dir
  }

  /// Stops the owner, keeping the directory to read until the guard drops. Every handle must be let go first (each
  /// daemon holding one stopped, and any configuration naming one dropped or cleared), or the owner never ends.
  pub(crate) fn stop_owner(&mut self) {
    if let Some(owner) = self.owner.take() {
      owner.stop().expect("the store's owner stops");
    }
  }

  /// Starts the owner again over the directory, as a restarted node opens its store (recovering it).
  pub(crate) fn restart_owner(&mut self) {
    self.stop_owner();
    self.owner =
      Some(StoreOwner::start(root(&self.dir), SIZING, WAITING).expect("the store's owner"));
  }
}

impl Drop for TestStore {
  fn drop(&mut self) {
    // A test that failed may unwind while a shard that died with it still holds the store's handle, so its owner would
    // never end: the owner is left to the process's end then, so the failure reads as itself rather than as a hang.
    if let Some(owner) = self.owner.take()
      && !std::thread::panicking()
    {
      let _ = owner.stop();
    }
    #[allow(clippy::disallowed_methods)] // test code: its own scratch directory
    let _ = std::fs::remove_dir_all(&self.dir);
  }
}

/// The store root granted over `dir`, as the daemon's `--vcs-store` grant opens it.
pub(crate) fn root(dir: &Path) -> StoreRoot {
  let grant = StoreGrant {
    repository: 0,
    target: StoreRoot::identify(dir).expect("the store's directory identified"),
    issued_ns: 0,
  };
  StoreRoot::open(dir, &grant).expect("the store's root")
}

/// [`crate::daemon::audit_daemon`] holding a fresh store: the store first, so a test binding both as
/// `let (_store, daemon)` drops the daemon first.
pub(crate) fn audit_daemon_stored() -> (TestStore, crate::Daemon) {
  audit_daemon_stored_configured(|_| {})
}

/// [`audit_daemon_stored`] with its configuration changed by `configure` first.
pub(crate) fn audit_daemon_stored_configured(
  configure: impl FnOnce(&mut crate::DaemonConfig),
) -> (TestStore, crate::Daemon) {
  let store = TestStore::start("audit-kept");
  let handle = store.handle();
  let daemon = crate::daemon::audit_daemon_configured(move |config| {
    config.vcs_disk = Some(handle);
    configure(config);
  });
  (store, daemon)
}

/// Runs `history` as a task on `daemon`'s first shard to its end and gives back its output: what a test whose steps
/// await a landing on the store does (C2: a Subversion record lands on the store before the register names it).
/// Bounded by `patience`; a history that does not end within it fails the test.
pub(crate) fn run_on_first<T: Send + 'static, F: std::future::Future<Output = T> + 'static>(
  daemon: &crate::Daemon,
  history: impl FnOnce() -> F + Clone + Send + 'static,
  patience: std::time::Duration,
) -> T {
  run_at(daemon, 0, history, patience)
}

/// Runs `history` as a task on `daemon`'s shard of `partition` to its end and gives back its output, as
/// [`run_on_first`] runs one on its first.
pub(crate) fn run_at<T: Send + 'static, F: std::future::Future<Output = T> + 'static>(
  daemon: &crate::Daemon,
  partition: usize,
  history: impl FnOnce() -> F + Clone + Send + 'static,
  patience: std::time::Duration,
) -> T {
  let (sent, got) = std::sync::mpsc::sync_channel(1);
  crate::daemon::observe_at(daemon, partition, move |_state| {
    let history = history.clone();
    let sent = sent.clone();
    let task = slates_rt::futures::spawn(async move {
      let _ = sent.send(history().await);
    })
    .expect("the history's task");
    slates_rt::futures::detach(task).expect("the history's task detached");
  });
  got.recv_timeout(patience).expect("the history ended")
}

/// A daemon holding a test's store, read as the daemon it is: the daemon stopped first when it drops or stops, then the
/// store given back (module docs).
pub(crate) struct StoredDaemon {
  /// The daemon, until it stops.
  daemon: Option<crate::Daemon>,
  /// Its store, held until the daemon stops.
  _store: TestStore,
}

impl StoredDaemon {
  /// Starts a daemon under `config`, `source` its segment, holding a fresh store named `name`.
  pub(crate) fn start(
    name: &str,
    profile: &slates_machine::MachineProfile,
    mut config: crate::DaemonConfig,
    source: crate::SegmentSource,
  ) -> Result<StoredDaemon, crate::error::ServerError> {
    let store = TestStore::start(name);
    config.vcs_disk = Some(store.handle());
    let daemon = crate::Daemon::start(profile, config, source)?;
    Ok(StoredDaemon {
      daemon: Some(daemon),
      _store: store,
    })
  }

  /// Stops the daemon, then gives the store back.
  pub(crate) fn stop(mut self) {
    if let Some(daemon) = self.daemon.take() {
      daemon.stop();
    }
  }
}

impl std::ops::Deref for StoredDaemon {
  type Target = crate::Daemon;

  fn deref(&self) -> &crate::Daemon {
    self
      .daemon
      .as_ref()
      .expect("the daemon runs until it stops")
  }
}

impl Drop for StoredDaemon {
  fn drop(&mut self) {
    if let Some(daemon) = self.daemon.take() {
      daemon.stop();
    }
  }
}

/// Shape: a reply route's client slot no client holds, so a test creation's answer is counted undeliverable.
const NO_CLIENT: u32 = 4096;

/// What a test asks a creation for: the repository's name, its tool, its object format and its default branch.
pub(crate) type Asked<'a> = (
  &'a str,
  slates_ipc::protocol::VcsTool,
  slates_ipc::protocol::ObjectFormat,
  &'a str,
);

/// Dispatches `CreateRepository` for `asked` by `principal` as the verb runner leaves a client's request numbered
/// `request` (its origin, its id and a reply route to a slot no client holds), so a creation on a daemon holding a store,
/// which lands its changeset 0 before it records the repository and answers by the route (A-767 item 7), can answer:
/// `Acknowledged` there, the reply itself otherwise.
pub(crate) fn routed_create(
  state: &mut crate::state::ShardState,
  (principal, request): (&slates_db::catalog::Principal, u64),
  (name, tool, format, default_branch): Asked<'_>,
) -> slates_ipc::protocol::ReplyBody {
  state.current_request = Some((1, slates_wire::request::RequestId::from_word(request)));
  state.reply_route = Some(crate::merge_service::ReplyRoute {
    shard: state.shard,
    client_index: NO_CLIENT,
    request,
    through_work: false,
  });
  let reply = crate::verbs::dispatch(
    state,
    1,
    principal,
    slates_ipc::protocol::RequestBody::CreateRepository {
      name: name.to_owned(),
      tool,
      format,
      default_branch: default_branch.to_owned(),
      green: None,
    },
  );
  // Left as the verb runner leaves them after it took a deferred acceptance.
  state.current_request = None;
  state.reply_route = None;
  state.acceptance_deferred = false;
  reply
}

/// Repository `name`'s id and the green its branch `branch` binds, once the partition records it.
fn recorded(
  state: &crate::state::ShardState,
  (name, branch): (&str, &str),
) -> Option<(
  slates_db::catalog::RepositoryId,
  slates_db::catalog::VolumeId,
)> {
  let repositories = state.db.partition().repositories();
  let id = repositories.repository_named(name)?.id;
  Some((id, repositories.branch(id, branch)?))
}

/// Shape: how long a test's creation may take to land and be recorded before the test fails.
const CREATION_PATIENCE: std::time::Duration = std::time::Duration::from_secs(30);

/// Creates repository `asked` by `principal` from a history running as a task on its owner shard ([`routed_create`]),
/// waiting for its creation to land and be recorded: its id and its root green.
pub(crate) async fn created(
  principal: &slates_db::catalog::Principal,
  asked: Asked<'_>,
) -> (
  slates_db::catalog::RepositoryId,
  slates_db::catalog::VolumeId,
) {
  let reply = crate::state::with_state(|state| routed_create(state, (principal, 1), asked))
    .expect("the shard's state");
  assert!(
    !matches!(reply, slates_ipc::protocol::ReplyBody::Refused { .. }),
    "{reply:?}"
  );
  let deadline = slates_rt::futures::now_ns()
    .saturating_add(u64::try_from(CREATION_PATIENCE.as_nanos()).unwrap_or(u64::MAX));
  loop {
    if let Some(Some(made)) = crate::state::with_state(|state| recorded(state, (asked.0, asked.3)))
    {
      return made;
    }
    assert!(
      slates_rt::futures::now_ns() < deadline,
      "repository {} was never recorded",
      asked.0
    );
    slates_rt::futures::yield_now().await;
  }
}

/// [`created`] from the test's thread, on `daemon`'s shard of partition `partition`.
pub(crate) fn create_on(
  daemon: &crate::Daemon,
  partition: usize,
  principal: &slates_db::catalog::Principal,
  asked: Asked<'_>,
) -> (
  slates_db::catalog::RepositoryId,
  slates_db::catalog::VolumeId,
) {
  let (principal, name, tool, format, branch) = (
    principal.clone(),
    asked.0.to_owned(),
    asked.1,
    asked.2,
    asked.3.to_owned(),
  );
  let reply = {
    let (name, branch) = (name.clone(), branch.clone());
    crate::daemon::observe_at(daemon, partition, move |state| {
      routed_create(state, (&principal, 1), (&name, tool, format, &branch))
    })
  };
  assert!(
    !matches!(reply, slates_ipc::protocol::ReplyBody::Refused { .. }),
    "{reply:?}"
  );
  let deadline = std::time::Instant::now() + CREATION_PATIENCE;
  loop {
    let (named, branch) = (name.clone(), branch.clone());
    if let Some(made) = crate::daemon::observe_at(daemon, partition, move |state| {
      recorded(state, (&named, &branch))
    }) {
      return made;
    }
    assert!(
      std::time::Instant::now() < deadline,
      "repository {name} was never recorded"
    );
    std::thread::yield_now();
  }
}

/// Lands `parts` under `header` through `disk` and waits for the store's answer, for a test's synchronous history on a
/// shard: the landing runs on a thread of its own with a runtime of its own, which parks until the store's owner
/// answers (the shard's own thread cannot wait on a wake its runtime would deliver). Whether it landed.
pub(crate) fn land_blocking(
  disk: StoreHandle,
  header: slates_store::log::Header,
  parts: Vec<slates_store::owner::Part>,
) -> bool {
  land_answered_blocking(disk, header, parts).is_ok()
}

/// [`land_blocking`]'s landing with the store's answer: where it was committed, or why the owner refused it (`Gone` for
/// a waiting thread that could not run it).
pub(crate) fn land_answered_blocking(
  disk: StoreHandle,
  header: slates_store::log::Header,
  parts: Vec<slates_store::owner::Part>,
) -> Result<slates_store::log::Located, slates_store::owner::OwnerError> {
  std::thread::scope(|scope| {
    scope
      .spawn(move || {
        waiting_runtime()
          .block_on(async move { disk.land(header, parts).await })
          .unwrap_or(Err(slates_store::owner::OwnerError::Gone))
      })
      .join()
      .unwrap_or(Err(slates_store::owner::OwnerError::Gone))
  })
}

/// Shape: one shard, a few tasks: [`land_blocking`]'s waiting thread's runtime.
fn waiting_runtime() -> slates_rt::runtime::LocalRuntime {
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
  .expect("the waiting thread's runtime")
}

/// [`crate::daemon::audit_on_shard`] over a daemon holding a fresh [`TestStore`] named `name`: `history` runs on its
/// shard; the daemon stops, dropping every handle, before the store goes (module docs).
pub(crate) fn audit_on_stored_shard<T: Send + 'static>(
  name: &str,
  history: impl FnOnce(&mut crate::state::ShardState) -> T + Clone + Send + 'static,
) -> T {
  audit_on_stored_shard_configured(name, history, |_| {})
}

/// [`audit_on_stored_shard`] for a history that begins by making a repository: `asked` is made first, by `principal`, as
/// a client's creation is made, the test's thread waiting for its record ([`create_on`]); then `history` runs on the
/// shard with the repository's id and its root green. The shard is never held through the creation's commit
/// (docs/bugs/2026-11-24-a-test-creation-held-its-shard-through-the-stores-commit.md), and the history keeps its landed
/// packs resident, as [`audit_on_stored_shard`]'s does.
pub(crate) fn audit_on_stored_shard_made<T: Send + 'static>(
  (principal, asked): (&slates_db::catalog::Principal, Asked<'_>),
  history: impl FnOnce(
    &mut crate::state::ShardState,
    (
      slates_db::catalog::RepositoryId,
      slates_db::catalog::VolumeId,
    ),
  ) -> T
  + Clone
  + Send
  + 'static,
) -> T {
  let (store, daemon) = audit_daemon_stored();
  keep_landed(&daemon);
  let made = create_on(&daemon, 0, principal, asked);
  let answer = crate::daemon::observe_first(&daemon, move |state| history(state, made));
  daemon.stop();
  drop(store);
  answer
}

/// [`audit_on_stored_shard`] with the daemon's configuration changed by `configure` first
/// ([`crate::daemon::audit_on_shard_configured`]).
pub(crate) fn audit_on_stored_shard_configured<T: Send + 'static>(
  name: &str,
  history: impl FnOnce(&mut crate::state::ShardState) -> T + Clone + Send + 'static,
  configure: impl FnOnce(&mut crate::DaemonConfig),
) -> T {
  let store = TestStore::start(name);
  let handle = store.handle();
  // A synchronous history brings no let-go window back, so its landed packs stay resident (`vcs_store::KEEP_LANDED`).
  let kept = move |state: &mut crate::state::ShardState| {
    crate::vcs_store::KEEP_LANDED.with(|keep| keep.set(true));
    history(state)
  };
  let answer = crate::daemon::audit_on_shard_configured(kept, move |config| {
    config.vcs_disk = Some(handle);
    configure(config);
  });
  drop(store);
  answer
}

/// Keeps a push's landed pack resident on `daemon`'s first shard (`vcs_store::KEEP_LANDED`): for a test whose history
/// reads through the store's own synchronous calls in its observations (a fetch answered in one step, a rebuild settled,
/// a green made now), which bring no let-go window back as the edges, the green build and the rebuild's task do.
pub(crate) fn keep_landed(daemon: &crate::Daemon) {
  crate::daemon::observe_first(daemon, |_| {
    crate::vcs_store::KEEP_LANDED.with(|keep| keep.set(true));
  });
}

/// [`crate::daemon::audit_daemon_of`] holding a fresh store: the store first, so a test binding both as
/// `let (_store, daemon)` drops the daemon first.
pub(crate) fn audit_daemon_of_stored(shards: u16) -> (TestStore, crate::Daemon) {
  let store = TestStore::start("audit-of");
  let handle = store.handle();
  let daemon = crate::daemon::audit_daemon_of_configured(shards, move |config| {
    config.vcs_disk = Some(handle);
  });
  (store, daemon)
}

/// A `CreateRepository` request served in a test's synchronous history on a shard of a node keeping a store, answered
/// once its changeset 0 is landed and it is recorded (`repository::create_blocking`, A-767 item 7): the verb's own
/// creation answers by its reply route from a task, which a history holding the shard cannot wait for ([`created`] and
/// [`create_on`] serve a history that runs as a task, or the test's thread). On a node with no store, and for any other
/// request, the verb as a client's dispatch serves it.
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

/// A store-holding daemon over `owner`'s repository `name`, whose revision 1 adds `/trunk/a.bin` with `bytes` (a stored text, which
/// its changeset carries), every hold RAM has on the text let go after: the green's held table's and the content
/// store's (C2 stage B part 2). The store's guard comes first; keep it alive past the daemon's stop. The repository's id
/// last.
pub(crate) fn a_text_only_the_store_holds(
  owner: &slates_db::catalog::Principal,
  name: &str,
  bytes: &[u8],
) -> (TestStore, crate::Daemon, slates_db::catalog::RepositoryId) {
  a_stored_text_let_go(owner, name, bytes, false)
}

/// [`a_text_only_the_store_holds`], but the content store keeps the text's object, every window of it let go (each
/// marked landed and `ContentStore::let_go`): its name held, its bytes in the store alone (codex-review's siblings of
/// 2026-11-22, a reader that took a held name for held bytes).
pub(crate) fn a_text_whose_windows_are_away(
  owner: &slates_db::catalog::Principal,
  name: &str,
  bytes: &[u8],
) -> (TestStore, crate::Daemon, slates_db::catalog::RepositoryId) {
  a_stored_text_let_go(owner, name, bytes, true)
}

/// The daemon of [`a_text_only_the_store_holds`]: the content store's object released whole, or kept with every
/// window let go where `away`.
fn a_stored_text_let_go(
  owner: &slates_db::catalog::Principal,
  name: &str,
  bytes: &[u8],
  away: bool,
) -> (TestStore, crate::Daemon, slates_db::catalog::RepositoryId) {
  use slates_svn::commit::drive::TextSink;
  let (store, daemon) = crate::test_store::audit_daemon_stored();
  let asked = (
    name,
    slates_ipc::protocol::VcsTool::Subversion,
    slates_ipc::protocol::ObjectFormat::Sha1,
    "trunk",
  );
  let (id, green) = crate::test_store::create_on(&daemon, 0, owner, asked);
  let committed = bytes.to_vec();
  let landing = owner.clone();
  let landed = crate::test_store::run_on_first(
    &daemon,
    move || async move {
      let text = slates_svn::commit::drive::text_of(&committed).unwrap();
      let texts = slates_svn::commit::drive::HeldTexts::new();
      let staged = texts.stage(None).unwrap();
      texts.append(staged, &committed).unwrap();
      texts.seal(staged, text.identity).unwrap();
      let row = |path: &str, kind, text| slates_svn::index::ChangedPath {
        path: path.to_owned(),
        action: slates_svn::index::Action::Add,
        kind,
        copy_from: None,
        text,
        properties: None,
        mergeinfo_modified: false,
      };
      let work = slates_svn::commit::drive::Work {
        head: 0,
        record: slates_svn::index::RevisionRecord {
          transaction: "0-1".to_owned(),
          changes: vec![
            row("/trunk", slates_svn::index::NodeKind::Directory, None),
            row(
              "/trunk/a.bin",
              slates_svn::index::NodeKind::File,
              Some(text),
            ),
          ],
        },
        lock_entries: Vec::new(),
        revision_properties: vec![(b"svn:log".to_vec(), b"noise".to_vec())],
        texts: std::iter::once(text.identity).collect(),
        property_lists: std::collections::BTreeMap::new(),
        locks: slates_svn::lock::CommitLocks::default(),
      };
      let landed = crate::repository::svn_commit::land_work((&landing, id), work, &texts)
        .await
        .map(|landed| landed.map(|committed| committed.revision));
      // Away: the content store holds the text's object once more, every window of it landed and let go, so its name
      // stands while its bytes are in the store alone.
      if away {
        crate::state::with_state(|state| {
          let len = u64::try_from(committed.len()).unwrap_or(u64::MAX);
          crate::vcs_store::within_budget(state, len, |stores| {
            let content = stores.content_mut().unwrap();
            assert_eq!(
              content.put_object(&committed).unwrap(),
              slates_vcs::ContentId(text.identity)
            );
            let window = content.window();
            for piece in committed.chunks(window) {
              let window = slates_vcs::ContentId::of(piece);
              assert!(content.mark_landed(&window), "a window of the text");
              // Let go whatever holds it (the green's name holds the text, A-760 (b) 4c), or gone already (main's (B)).
              let gone = content.let_go_landed(&window, None);
              assert!(
                matches!(gone, Ok(_) | Err(slates_vcs::LetGoRefused::Away)),
                "{gone:?}"
              );
            }
          });
        });
      }
      // Every byte RAM has of the text let go: the green's held table's hold given back, and every window of it let go,
      // landed (A-760 (b) 4d). The green's file names it alone, its hold keeping the content store's entry, so a reader
      // brings its bytes back from the node's store.
      crate::state::with_state(|state| {
        if let Some(mut engine) = state.greens.get_mut(&green) {
          while engine.held(&text.identity).is_some() {
            let _ = engine.release_held(&text.identity);
          }
        }
        crate::vcs_store::within_budget(state, 0, |stores| {
          if let Some(content) = stores.content_mut() {
            let _ = content.let_go_object_landed(&slates_vcs::ContentId(text.identity), &|_| None);
          }
        });
      });
      landed
    },
    std::time::Duration::from_secs(60),
  );
  assert_eq!(landed, Some(Ok(1)));
  (store, daemon, id)
}

/// Shape: the text the store-read tests commit: noise, so a drive's svndiff carries its bytes as they are.
pub(crate) fn noise(len: usize) -> Vec<u8> {
  let mut state = 0x9e37_79b9_7f4a_7c15u64;
  (0..len)
    .map(|_| {
      state ^= state << 13;
      state ^= state >> 7;
      state ^= state << 17;
      state.to_le_bytes()[0]
    })
    .collect()
}

/// Answers what a streamed answer asks of the changeset store (`slates_hg::render::StoreWants`), each want asked of
/// `disk` for `repository` and waited for on a thread of its own ([`land_blocking`]'s way), for a test's synchronous
/// history on a shard: what `vcs_store::hg_view::answer_store_wants` answers between a daemon's turns. Whether every
/// want was answered.
pub(crate) fn answer_wants_blocking(
  disk: &StoreHandle,
  repository: u128,
  wants: &mut dyn slates_hg::render::StoreWants,
) -> bool {
  use slates_hg::render::{StoredPlace, StoredRevision, Want};
  while let Some(want) = wants.wants().cloned() {
    let disk = disk.clone();
    let given = std::thread::scope(|scope| {
      scope
        .spawn(move || {
          waiting_runtime()
            .block_on(async move {
              match want {
                Want::Rows { logs } => {
                  let asked = logs
                    .into_iter()
                    .map(|(log, nodes)| (log, nodes.iter().map(|node| node.to_vec()).collect()))
                    .collect();
                  disk
                    .texts_by_tool(repository, asked)
                    .await
                    .ok()
                    .map(|found| {
                      slates_hg::serve::getbundle::Given::Rows(
                        found
                          .into_iter()
                          .map(|log| {
                            log
                              .into_iter()
                              .map(|row| {
                                row.map(|(text, row)| StoredRevision {
                                  text,
                                  base: row.base,
                                  place: StoredPlace {
                                    payload: row.place.payload,
                                    offset: row.place.offset,
                                    len: row.place.len,
                                  },
                                })
                              })
                              .collect()
                          })
                          .collect(),
                      )
                    })
                }
                Want::Chunks { places } => {
                  let asked = places
                    .iter()
                    .map(|place| slates_store::texts::Place {
                      payload: place.payload,
                      offset: place.offset,
                      len: place.len,
                    })
                    .collect();
                  disk
                    .places(asked)
                    .await
                    .ok()
                    .map(slates_hg::serve::getbundle::Given::Chunks)
                }
                Want::Texts { texts, most } => {
                  crate::vcs_store::hg_view::texts_made(&disk, repository, &texts, most)
                    .await
                    .map(|made| match made {
                      crate::vcs_store::hg_view::MadeTexts::Made(made) => {
                        slates_hg::serve::getbundle::Given::Texts(made)
                      }
                      crate::vcs_store::hg_view::MadeTexts::Past => {
                        slates_hg::serve::getbundle::Given::TextsPast
                      }
                    })
                }
              }
            })
            .ok()
            .flatten()
        })
        .join()
        .ok()
        .flatten()
    });
    match given {
      Some(slates_hg::serve::getbundle::Given::Rows(rows)) => wants.give_rows(rows),
      Some(slates_hg::serve::getbundle::Given::Chunks(chunks)) => wants.give_chunks(chunks),
      Some(slates_hg::serve::getbundle::Given::Texts(texts)) => wants.give_texts(texts),
      Some(slates_hg::serve::getbundle::Given::TextsPast) => wants.texts_past(),
      None => return false,
    }
  }
  true
}

/// The principal the Subversion helpers below act as: uid 1234 on host 0, as the store tests' owner.
pub(crate) fn svn_owner() -> slates_db::catalog::Principal {
  slates_db::catalog::Principal::Uid {
    host: slates_db::HostId(0),
    uid: 1234,
  }
}

/// Creates Subversion repository `name` on `daemon`'s one shard through a client's request, waiting for its creation to
/// land on the store and be recorded (A-767 item 7's one order).
pub(crate) fn created_subversion(
  daemon: &crate::Daemon,
  name: &str,
) -> slates_db::catalog::RepositoryId {
  let asked = (
    name,
    slates_ipc::protocol::VcsTool::Subversion,
    slates_ipc::protocol::ObjectFormat::Revisions,
    "trunk",
  );
  create_on(daemon, 0, &svn_owner(), asked).0
}

/// A client's work on revision `head` setting `/trunk/a.txt` to `bytes` (`action`: add or modify), `/trunk` added with
/// it at revision 0, with its text held as a commit's sink holds it.
pub(crate) fn file_work(
  head: u64,
  action: slates_svn::index::Action,
  bytes: &[u8],
) -> (
  slates_svn::commit::drive::Work,
  slates_svn::commit::drive::HeldTexts,
) {
  use slates_svn::commit::drive::TextSink;
  use slates_svn::index::{Action, ChangedPath, NodeKind};
  let text = slates_svn::commit::drive::text_of(bytes).unwrap();
  let texts = slates_svn::commit::drive::HeldTexts::new();
  let staged = texts.stage(None).unwrap();
  texts.append(staged, bytes).unwrap();
  texts.seal(staged, text.identity).unwrap();
  let mut changes = Vec::new();
  if head == 0 {
    changes.push(ChangedPath {
      path: "/trunk".to_owned(),
      action: Action::Add,
      kind: NodeKind::Directory,
      copy_from: None,
      text: None,
      properties: None,
      mergeinfo_modified: false,
    });
  }
  changes.push(ChangedPath {
    path: "/trunk/a.txt".to_owned(),
    action,
    kind: NodeKind::File,
    copy_from: None,
    text: Some(text),
    properties: None,
    mergeinfo_modified: false,
  });
  let work = slates_svn::commit::drive::Work {
    head,
    record: slates_svn::index::RevisionRecord {
      transaction: format!("{head}-1"),
      changes,
    },
    lock_entries: Vec::new(),
    revision_properties: vec![(b"svn:log".to_vec(), format!("r{}", head + 1).into_bytes())],
    texts: std::iter::once(text.identity).collect(),
    property_lists: std::collections::BTreeMap::new(),
    locks: slates_svn::lock::CommitLocks::default(),
  };
  (work, texts)
}

/// Lands `work` on `repository` through the client's commit landing (`svn_commit::land_work`), as a task on `daemon`'s
/// shard: the revision it committed, or why not.
pub(crate) fn landed_work(
  daemon: &crate::Daemon,
  repository: slates_db::catalog::RepositoryId,
  head: u64,
  (action, bytes): (slates_svn::index::Action, Vec<u8>),
) -> Result<u64, crate::repository::svn_commit::Uncommitted> {
  run_on_first(
    daemon,
    move || async move {
      let (work, texts) = file_work(head, action, &bytes);
      crate::repository::svn_commit::land_work((&svn_owner(), repository), work, &texts)
        .await
        .unwrap()
        .map(|committed| committed.revision)
    },
    std::time::Duration::from_secs(60),
  )
}

/// Drops repository `repository`'s record, keeping its greens and their memberships (A-515), and holds the record
/// pending its rebuild, as a takeover leaves a repository between adopting its greens and rebuilding it
/// (`ShardState::pending_repositories`, A-636 part 2c-1): its record. Its store goes too where `store_goes`, as a
/// successor holds none until its rebuild makes it from the log; kept otherwise, for a tool whose rebuilt store cannot
/// yet hold the objects a resend needs (A-636 part 2c-2, owed: the store's objects rebuilt by content identity; a
/// Mercurial store's texts).
pub(crate) fn awaiting_rebuild(
  state: &mut crate::state::ShardState,
  repository: slates_db::catalog::RepositoryId,
  store_goes: bool,
) -> slates_db::catalog::RepositoryRecord {
  let record = state
    .db
    .partition()
    .repositories()
    .repository(repository)
    .cloned()
    .expect("the record");
  let dropped = slates_db::op::Op::RepositoryDestroyed { repository };
  let now = slates_vfs::clock::Clock::monotonic_ns(&mut state.clock);
  state
    .db
    .mutate(&mut state.segment, &dropped, now)
    .expect("dropped");
  if store_goes {
    crate::vcs_store::free(state, repository);
  }
  state.pending_repositories.insert(
    slates_db::register::ObjectId(repository.bytes),
    (1, record.clone()),
  );
  record
}

/// Rebuilds `record`'s repository from `parts`, its state logs' parts, as its successor does once its record and logs are
/// adopted
/// (`takeover::rebuild_repository`, its store made from the log a step a turn, driven to its end here), and takes it out
/// of the pending set as the fleet's period does.
pub(crate) fn rebuilt_after_takeover(
  state: &mut crate::state::ShardState,
  (record, parts): (
    slates_db::catalog::RepositoryRecord,
    Vec<slates_db::catalog::StateLogRecord>,
  ),
) {
  let id = record.id;
  state
    .pending_repositories
    .remove(&slates_db::register::ObjectId(id.bytes));
  assert!(
    crate::takeover::rebuild_repository(state, record, parts),
    "rebuilt"
  );
  crate::vcs_store::settle_rebuild(state, id);
}
