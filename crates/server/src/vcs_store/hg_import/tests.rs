//! A-675's tests: a repository directory imported into its store on its owner shard, a turn at a time, held to the
//! import of the same directory in one call; refused, abandoned and freed imports giving back everything they held; and
//! the shard's own task running an import to its end.

use std::time::{Duration, Instant};

use slates_db::catalog::{Principal, RepositoryId};
use slates_hg::Admission;
use slates_hg::import::{self, Clock, Imported};
use slates_ipc::protocol::{ObjectFormat as WireFormat, ReplyBody, RequestBody, VcsTool};
use slates_vcs::ContentStore;

use super::{
  ABANDONED, DONE, Finished, ImportFailed, REFUSED, STARTED, TURNS, Turned, begin, run, turn,
};
use crate::hg_base::HgBase;
use crate::hg_base::tests::{Files, STATES, laid_out};
use crate::state::ShardState;
use crate::verbs::dispatch;

/// An index's rows, each its key and its body.
type Rows = Vec<(Vec<u8>, Vec<u8>)>;

/// Shape: a content store's budget and an import's room for the one-call import the tests hold the shard's to, each the
/// fixture's history many times over.
const ROOM: u64 = 1 << 24;

/// Shape: more turns than any fixture's units of work, so an import that never ends fails the test rather than hangs it.
const TURNS_AT_MOST: u64 = 1 << 16;

/// Shape: how long a test waits for the shard's task to end an import: far past a debug build's import of the fixture
/// (milliseconds), so only a task that never ends reaches it.
const TASK_WAIT: Duration = Duration::from_secs(60);

/// Shape: the store file the damaged fixture cuts, read once the changelog's revisions are kept.
const CUT_FILE: &str = "store/00manifest.i";

/// A clock a tick a reading, so a turn of quantum 1 ends after its first unit.
struct Ticking(u64);

impl Clock for Ticking {
  fn now_ns(&mut self) -> u64 {
    self.0 = self.0.saturating_add(1);
    self.0
  }
}

/// The principal that creates the tests' repositories.
fn owner() -> Principal {
  Principal::Uid {
    host: slates_db::HostId(0),
    uid: 1234,
  }
}

/// Creates repository `name` of `tool` on the shard: its id.
fn created(state: &mut ShardState, name: &str, tool: VcsTool) -> RepositoryId {
  let reply = dispatch(
    state,
    1,
    &owner(),
    RequestBody::CreateRepository {
      name: name.to_owned(),
      tool,
      format: WireFormat::Sha1,
      default_branch: "default".to_owned(),
      green: None,
    },
  );
  assert!(
    matches!(reply, ReplyBody::GreenCreated { .. }),
    "no repository: {reply:?}"
  );
  let repositories = state.db.partition().repositories();
  repositories.repository_named(name).expect("the record").id
}

/// `files` laid out on a simulated host, as an import takes them: the host, and the repository's directory on it.
fn directory(
  files: impl IntoIterator<Item = (&'static str, &'static [u8])>,
) -> (Box<dyn slates_vfs::host::HostFs>, slates_vfs::host::HostDir) {
  let (host, directory) = laid_out(files);
  (Box::new(host), directory)
}

/// `files` imported in one call through the same adapter, into a content store with the shard's own window: the rows,
/// and what the import found.
fn in_one_call(state: &ShardState, files: Files) -> (Rows, Imported) {
  let (mut host, repository) = laid_out(files.iter().copied());
  let window = state.store.content.chunk_bytes();
  let mut store = ContentStore::new(ROOM, window).expect("a content store");
  let mut base = HgBase::new(&mut host, repository);
  let mut imported = import::import(&mut base, &mut store, Admission::new(ROOM)).expect("imported");
  let rows = imported
    .index
    .rows()
    .iter(&store)
    .map(|row| (row.key().to_vec(), row.body().to_vec()))
    .collect();
  (rows, imported)
}

/// Repository `id`'s index rows, each its key and its body.
fn rows_of(state: &mut ShardState, id: RepositoryId) -> Rows {
  let (content, store) = state.vcs_stores.mercurial(id).expect("the store");
  store
    .index
    .rows()
    .iter(content)
    .map(|row| (row.key().to_vec(), row.body().to_vec()))
    .collect()
}

/// Counter `name` on the shard.
fn count(state: &ShardState, name: &str) -> u64 {
  state.refusals.get(name).copied().unwrap_or(0)
}

/// What the content store holds.
fn used(state: &ShardState) -> u64 {
  state.vcs_stores.content().map_or(0, ContentStore::used)
}

/// Runs repository `id`'s import a unit a turn to its end: the turns taken, and how it ended.
fn to_its_end(
  state: &mut ShardState,
  id: RepositoryId,
) -> (u64, Result<Box<Finished>, ImportFailed>) {
  let mut clock = Ticking(0);
  for turns in 1..=TURNS_AT_MOST {
    match turn(state, id, (&mut clock, 1)) {
      Some(Ok(Turned::More)) => {}
      Some(Ok(Turned::Done(finished))) => return (turns, Ok(finished)),
      Some(Err(failed)) => return (turns, Err(failed)),
      None => panic!("no import of the repository under way at turn {turns}"),
    }
  }
  panic!("no end after {TURNS_AT_MOST} turns")
}

/// Holds what the shard's import found equal to what the one-call import found.
fn check_found(finished: &Finished, expected: &Imported) {
  let found = (
    finished.changesets,
    finished.manifests,
    finished.file_logs,
    finished.file_revisions,
    finished.markers,
  );
  let wanted = (
    expected.changesets,
    expected.manifests,
    expected.file_logs,
    expected.file_revisions,
    expected.markers,
  );
  assert_eq!(found, wanted);
  assert_eq!(
    (&finished.phase_roots, &finished.bookmarks),
    (&expected.phase_roots, &expected.bookmarks)
  );
  let dropped = (
    finished.dropped_phase_roots,
    finished.dropped_bookmarks,
    finished.malformed_bookmarks,
  );
  assert_eq!(
    dropped,
    (
      expected.dropped_phase_roots,
      expected.dropped_bookmarks,
      expected.malformed_bookmarks
    )
  );
}

/// Holds the shard's charge settled to what its content store holds, and no import under way for `id`.
fn check_settled(state: &ShardState, id: RepositoryId) {
  assert!(!state.vcs_stores.importing(id));
  assert_eq!(state.vcs_stores.charged(), used(state));
}

/// A-675 (the oracle): do import the `states` repository 7.2 kept, laid out on the simulated host, into a Mercurial
/// repository's store on its owner shard, a unit a turn; and again in one call through the same adapter. Expect more
/// than one turn, the store's index equal to the one call's row for row with what it found beside (counts, phase roots,
/// bookmarks), no import under way after, the shard's charge settled to what the content store holds, and each turn
/// and the import's start and end counted.
#[test]
fn t_a675_a_directory_imports_into_its_store_a_turn_at_a_time_as_one_call_imports_it() {
  crate::daemon::audit_on_shard(|state| {
    let id = created(state, "hg", VcsTool::Mercurial);
    let (expected_rows, expected) = in_one_call(state, STATES);
    assert_eq!(begin(state, id, directory(STATES.iter().copied())), Ok(()));
    assert!(state.vcs_stores.importing(id));
    let (turns, ended) = to_its_end(state, id);
    let finished = ended.expect("imported");
    assert!(
      turns > 1 && finished.turns == turns,
      "{turns} turns, {finished:?}"
    );
    assert!(!expected_rows.is_empty());
    assert_eq!(rows_of(state, id), expected_rows);
    check_found(&finished, &expected);
    check_settled(state, id);
    let counted = [
      count(state, STARTED),
      count(state, DONE),
      count(state, TURNS),
    ];
    assert_eq!(counted, [1, 1, turns]);
  });
}

/// A-675 (refused midway): do import the `states` repository with its manifest log cut in half, a unit a turn. Expect
/// the import refused after more than one turn, by the import's own refusal; the store's index still empty, the content
/// store holding what it held before, the shard's retention charge what it was before (the room and the kept texts
/// given back), no import under way, and the refusal counted.
#[test]
fn t_a675_an_import_refused_midway_gives_back_everything_it_held() {
  crate::daemon::audit_on_shard(|state| {
    let id = created(state, "hg", VcsTool::Mercurial);
    let cut = STATES.iter().map(|&(path, bytes)| {
      let half = bytes.len() / 2;
      (
        path,
        if path == CUT_FILE {
          bytes.get(..half).unwrap_or_default()
        } else {
          bytes
        },
      )
    });
    let (held, retained) = (used(state), state.store.budget.retained());
    assert_eq!(begin(state, id, directory(cut)), Ok(()));
    let (turns, ended) = to_its_end(state, id);
    assert!(
      turns > 1 && matches!(ended, Err(ImportFailed::Refused(_))),
      "{turns}: {ended:?}"
    );
    assert!(rows_of(state, id).is_empty());
    assert_eq!(
      (used(state), state.store.budget.retained()),
      (held, retained)
    );
    check_settled(state, id);
    assert_eq!(count(state, REFUSED), 1);
  });
}

/// A-675 (one empty Mercurial store, one import at a time): do ask a git repository's store for an import; start one
/// into a Mercurial repository and ask for a second; import another to its end and ask it for one more. Expect `NoStore`,
/// `Busy` and `NotEmpty`, each leaving the store as it was: the first import still under way, the imported store's
/// rows unchanged.
#[test]
fn t_a675_an_import_takes_an_empty_mercurial_store_one_at_a_time() {
  crate::daemon::audit_on_shard(|state| {
    let git = created(state, "git", VcsTool::Git);
    assert_eq!(
      begin(state, git, directory(STATES.iter().copied())),
      Err(ImportFailed::NoStore)
    );
    let first = created(state, "first", VcsTool::Mercurial);
    assert_eq!(
      begin(state, first, directory(STATES.iter().copied())),
      Ok(())
    );
    assert_eq!(
      begin(state, first, directory(STATES.iter().copied())),
      Err(ImportFailed::Busy)
    );
    assert!(state.vcs_stores.importing(first));
    let full = created(state, "full", VcsTool::Mercurial);
    assert_eq!(
      begin(state, full, directory(STATES.iter().copied())),
      Ok(())
    );
    assert!(to_its_end(state, full).1.is_ok());
    let rows = rows_of(state, full);
    assert_eq!(
      begin(state, full, directory(STATES.iter().copied())),
      Err(ImportFailed::NotEmpty)
    );
    assert_eq!(rows_of(state, full), rows);
  });
}

/// A-675 (cancellation): do start an import, run it a unit a turn until the content store holds some of it, then free
/// the repository; and start one into another repository and run it to its end before freeing that one too. Expect the
/// freed import given back (no import under way, counted abandoned) and, with both repositories freed, nothing charged
/// and the shard's retention charge what it was before either import.
#[test]
fn t_a675_a_repository_freed_midway_gives_its_import_back() {
  crate::daemon::audit_on_shard(|state| {
    let retained = state.store.budget.retained();
    let midway = created(state, "midway", VcsTool::Mercurial);
    assert_eq!(
      begin(state, midway, directory(STATES.iter().copied())),
      Ok(())
    );
    let mut clock = Ticking(0);
    while used(state) == 0 {
      assert_eq!(turn(state, midway, (&mut clock, 1)), Some(Ok(Turned::More)));
    }
    super::super::free(state, midway);
    assert!(!state.vcs_stores.importing(midway));
    assert_eq!(count(state, ABANDONED), 1);
    let whole = created(state, "whole", VcsTool::Mercurial);
    assert_eq!(
      begin(state, whole, directory(STATES.iter().copied())),
      Ok(())
    );
    assert!(to_its_end(state, whole).1.is_ok());
    super::super::free(state, whole);
    assert_eq!(
      (state.vcs_stores.charged(), state.store.budget.retained()),
      (0, retained)
    );
  });
}

/// A-675 (the shard's task): do start an import of the `states` repository on a running shard and hand it to a task of
/// the shard ([`run`]), then wait for the import to end. Expect it ended within the wait, its index the one-call
/// import's row for row, counted done once, over at least one turn of the shard's own quantum and clock.
#[test]
fn t_a675_the_shards_task_runs_an_import_to_its_end() {
  let daemon = crate::daemon::audit_daemon();
  let id = crate::daemon::observe_first(&daemon, |state| {
    let id = created(state, "hg", VcsTool::Mercurial);
    assert_eq!(begin(state, id, directory(STATES.iter().copied())), Ok(()));
    let task = slates_rt::futures::spawn(async move {
      drop(run(id).await);
    });
    task
      .and_then(slates_rt::futures::detach)
      .expect("the import's task");
    id
  });
  let began = Instant::now();
  let (importing, done, turns) = loop {
    let seen = crate::daemon::observe_first(&daemon, move |state| {
      (
        state.vcs_stores.importing(id),
        count(state, DONE),
        count(state, TURNS),
      )
    });
    if !seen.0 || began.elapsed() > TASK_WAIT {
      break seen;
    }
  };
  let (rows, expected_rows) = crate::daemon::observe_first(&daemon, move |state| {
    (rows_of(state, id), in_one_call(state, STATES).0)
  });
  daemon.stop();
  assert!(!importing, "still importing after {TASK_WAIT:?}");
  assert_eq!(done, 1);
  assert!(turns >= 1);
  assert_eq!(rows, expected_rows);
}
