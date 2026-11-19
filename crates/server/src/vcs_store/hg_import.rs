//! A Mercurial repository's directory imported into its store on its owner shard (A-675; §4.17.3 piece 3; §4.15; §4.3;
//! A-649 parts 3b and 4; A-662).
//!
//! **What it does.** A Mercurial repository's store is made with its record as an empty index (`HgStore`). An import
//! gives it the history of a repository on the host: the repository's directory is read as a base, read-only (§4.15),
//! through the server's adapter (`crate::hg_base`), and `slates-hg`'s import (`Import`) is advanced a turn at a time on
//! the repository's owner shard ([`turn`], [`run`]), each turn lent the host and the shard's content store, within the
//! shard's quantum (§4.3's step budget) on the shard's clock, with a yield between turns. Once it is done, its index
//! takes the store's place and the empty index it replaces is given back, so the store holds one index and nothing
//! keeps a second; what it found beside the history, its phase roots, bookmarks and counts, is answered ([`Finished`])
//! for the register's first entry to record. Refused, abandoned, or met at its end by a store that is no longer empty,
//! it gives back everything it held, and the store is as it was.
//!
//! **Bounds.** An import holds, between its turns as within them, at most its room (`Admission`): the listing, the log
//! it reads (an inline revlog whole), a text being rebuilt and the text before it. The room is one client region's bytes
//! (`RegionGeometry::total_bytes`), what one request of the version-control edge may hold (`vcs_http`'s `Bounds`),
//! since an import is asked for by one client; it is charged to the shard's budget as retention when the import starts
//! and given back when it ends, so the shard's charge covers what the import may hold while it waits between turns. What
//! the import keeps in the content store is charged as every store's is (the map's module doc): each turn is lent every
//! byte the shard could admit now, then the arena is grown from its pool by what the turn kept and the charge trued up
//! to what the content store holds. One import a repository at a time, so the imports under way are bounded by the
//! partition's repositories. Each holds its own host, which its caller opened for the directory and which is closed
//! when the import ends; the adapter's lend, made and ended within each turn, holds at most a directory and two files.
//!
//! **Cancellation.** The import lives in the shard's map, keyed by its repository, and the task that runs it holds the
//! key alone, so a task dropped mid-import leaks nothing: the import waits in the map, holding what it held, until a
//! task runs it again or its repository is freed, which gives it back ([`abandon`]).
//!
//! **Owed** (GAPS' §4.17.3 entry). The verb that asks for a directory's import and answers when it ends; the import made
//! durable: the register's first entry recording the phase roots and bookmarks [`Finished`] carries, and a recovery that
//! imports the directory again or replays its record (until then an import is not served, and a restart leaves its
//! store empty); a host's pending fetch retried at the next turn rather than ending the import; and the adapter's
//! handles held between turns beside the import, now that the import lives in the shard's map.

use slates_db::catalog::RepositoryId;
use slates_hg::import::{Clock, Import, ImportRefused, Imported, Progress, Turn};
use slates_hg::index::HgIndex;
use slates_hg::phases::PhaseHead;
use slates_hg::{Admission, Node};
use slates_vcs::ContentStore;
use slates_vfs::host::{HostDir, HostFs};

use super::{RELEASE_REFUSED, RepositoryStore, VcsStores, lend, settle};
use crate::hg_base::{BaseRefused, HgBase};
use crate::state::ShardState;

#[cfg(test)]
mod tests;

/// Counter: an import started, its requirements and listing read. Format: a counter name in the status report.
pub(crate) const STARTED: &str = "vcs_store.hg_import.started";
/// Counter: a turn of an import; more turns than imports shows the imports ran a turn at a time. Format: a counter name.
pub(crate) const TURNS: &str = "vcs_store.hg_import.turns";
/// Counter: an import whose index took its store's place. Format: a counter name.
pub(crate) const DONE: &str = "vcs_store.hg_import.done";
/// Counter: an import refused midway, everything it held given back. Format: a counter name.
pub(crate) const REFUSED: &str = "vcs_store.hg_import.refused";
/// Counter: an import given back before its index took its store's place: its repository freed, or its store no longer
/// empty when it ended. Format: a counter name.
pub(crate) const ABANDONED: &str = "vcs_store.hg_import.abandoned";

/// A repository's import under way, kept in the shard's map between its turns (the module doc).
pub(crate) struct Importing {
  /// The import.
  import: Import,
  /// The host its directory is read through: the import's own, closed when the import ends.
  host: Box<dyn HostFs>,
  /// The repository's directory, on the host.
  directory: HostDir,
  /// What the import's room holds of the shard's budget: charged as retention when it started, given back at its end.
  reserved: u64,
}

impl std::fmt::Debug for Importing {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("Importing")
      .field("turns", &self.import.turns())
      .field("units", &self.import.units())
      .field("reserved", &self.reserved)
      .finish_non_exhaustive()
  }
}

/// Why an import did not start, or ended without its index in its store's place. Each leaves the store as it was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ImportFailed {
  /// No Mercurial store of the repository is held here.
  NoStore,
  /// The store holds a history: an import takes the place of the empty index its record was made with.
  NotEmpty,
  /// An import of the repository is under way.
  Busy,
  /// The shard's budget could not take the import's room: what it could admit.
  Budget {
    /// The bytes it could admit.
    available: u64,
  },
  /// The import refused: the store, its requirements, a file, or the host's own refusal (a fetch still pending on an
  /// S3 base among them).
  Refused(ImportRefused<BaseRefused>),
}

/// Where a turn left an import.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Turned {
  /// More of the repository to import.
  More,
  /// Imported: its index is the store's, and what it found beside it.
  Done(Box<Finished>),
}

/// What a finished import found beside the history it handed its store (the module doc): its counts, and the state the
/// register's first entry is to record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Finished {
  /// The changesets.
  pub(crate) changesets: u32,
  /// The manifest revisions.
  pub(crate) manifests: u32,
  /// The file logs.
  pub(crate) file_logs: u64,
  /// The file revisions, over every file log.
  pub(crate) file_revisions: u64,
  /// The obsolescence markers, kept as the index's marker rows.
  pub(crate) markers: u64,
  /// The roots of the draft, secret, archived and internal phases, sorted by phase and node.
  pub(crate) phase_roots: Vec<PhaseHead>,
  /// Phase roots dropped, as 7.2 drops them.
  pub(crate) dropped_phase_roots: u64,
  /// The bookmarks, each its name and a changeset the changelog holds, sorted by name.
  pub(crate) bookmarks: Vec<(Vec<u8>, Node)>,
  /// Bookmarks dropped, as 7.2 drops them.
  pub(crate) dropped_bookmarks: u64,
  /// Bookmark lines passed over, as 7.2 passes them over.
  pub(crate) malformed_bookmarks: u64,
  /// The turns the import took.
  pub(crate) turns: u64,
}

impl Finished {
  /// What `imported` found, its index handed on, over `turns` turns.
  fn of(imported: Imported, turns: u64) -> Finished {
    Finished {
      changesets: imported.changesets,
      manifests: imported.manifests,
      file_logs: imported.file_logs,
      file_revisions: imported.file_revisions,
      markers: imported.markers,
      phase_roots: imported.phase_roots,
      dropped_phase_roots: imported.dropped_phase_roots,
      bookmarks: imported.bookmarks,
      dropped_bookmarks: imported.dropped_bookmarks,
      malformed_bookmarks: imported.malformed_bookmarks,
      turns,
    }
  }
}

/// The shard's clock (§4.3), which a turn's quantum is spent on.
struct ShardClock;

impl Clock for ShardClock {
  fn now_ns(&mut self) -> u64 {
    slates_rt::futures::now_ns()
  }
}

/// Derived: the most an import holds at once, its room (the module doc): one client region's bytes, what one request of
/// the version-control edge may hold.
fn room_of(state: &ShardState) -> u64 {
  u64::try_from(state.config.region.total_bytes()).unwrap_or(u64::MAX)
}

/// Derived: the nanoseconds a turn may spend: the runtime's step quantum (§4.3), as the shard's other long work is cut,
/// else the configuration's step budget off the runtime.
fn quantum_of(state: &ShardState) -> u64 {
  slates_rt::futures::step_budget_ns().unwrap_or(state.config.runtime.step_budget_ns)
}

/// Starts repository `id`'s import from `directory` on `host` (the module doc): its store must be the empty index its
/// record was made with, and no import of it under way. The import's room is charged to the shard's budget, and the
/// repository's requirements and listing are read; nothing is kept in the store until its turns ([`turn`]). Refused,
/// nothing is kept, the room is given back and the host closed.
pub(crate) fn begin(
  state: &mut ShardState,
  id: RepositoryId,
  (mut host, directory): (Box<dyn HostFs>, HostDir),
) -> Result<(), ImportFailed> {
  takes_import(&mut state.vcs_stores, id)?;
  let room = room_of(state);
  reserve(state, room)?;
  let started = Import::start(
    &mut HgBase::new(&mut *host, directory),
    Admission::new(room),
  );
  match started {
    Ok(import) => {
      let importing = Importing {
        import,
        host,
        directory,
        reserved: room,
      };
      state.vcs_stores.imports.insert(id, importing);
      state.count(STARTED, 1);
      Ok(())
    }
    Err(refused) => {
      state.store.budget.credit_retention(room);
      Err(ImportFailed::Refused(refused))
    }
  }
}

/// Whether repository `id`'s store takes an import now: a Mercurial store holding no row, and no import of it under way.
fn takes_import(stores: &mut VcsStores, id: RepositoryId) -> Result<(), ImportFailed> {
  if stores.imports.contains_key(&id) {
    return Err(ImportFailed::Busy);
  }
  match stores.entries.get_mut(&id) {
    Some(RepositoryStore::Mercurial(store)) => {
      if holds_none(&mut store.index) {
        Ok(())
      } else {
        Err(ImportFailed::NotEmpty)
      }
    }
    _ => Err(ImportFailed::NoStore),
  }
}

/// Whether `index` holds no row: the empty index a repository's record was made with.
fn holds_none(index: &mut HgIndex) -> bool {
  index.rows().rows() == 0
}

/// Charges an import's `room` to the shard's budget as retention, the arena first grown from its pool by it.
fn reserve(state: &mut ShardState, room: u64) -> Result<(), ImportFailed> {
  state.store.make_room(room);
  if state.store.budget.charge_retention(room).is_err() {
    return Err(ImportFailed::Budget {
      available: state.store.admittable(),
    });
  }
  Ok(())
}

/// One turn of repository `id`'s import (the module doc): units of work until `quantum_ns` is spent on `clock`, at
/// least one, lent the host and the content store. `More` while there is more; once it is done, its index is in its
/// store's place and what it found is answered; refused, everything it held is given back. `None` when no import of
/// `id` is under way.
pub(crate) fn turn(
  state: &mut ShardState,
  id: RepositoryId,
  (clock, quantum_ns): (&mut dyn Clock, u64),
) -> Option<Result<Turned, ImportFailed>> {
  let advanced = advance(state, id, Turn { clock, quantum_ns })?;
  state.count(TURNS, 1);
  Some(match advanced {
    Ok(Progress::More) => Ok(Turned::More),
    Ok(Progress::Done) => finish(state, id),
    Err(refused) => {
      end(state, id);
      state.count(REFUSED, 1);
      Err(ImportFailed::Refused(refused))
    }
  })
}

/// One turn of repository `id`'s import over the map, lent every byte the shard could admit now, then the arena grown
/// by what it kept and the charge trued up (the module doc). `None` when no import of `id` is under way.
fn advance(
  state: &mut ShardState,
  id: RepositoryId,
  turn: Turn<'_>,
) -> Option<Result<Progress, ImportRefused<BaseRefused>>> {
  if !state.vcs_stores.imports.contains_key(&id) {
    return None;
  }
  lend(&mut state.store, &mut state.vcs_stores, 0);
  let stores = &mut state.vcs_stores;
  let advanced = match (stores.content.as_mut(), stores.imports.get_mut(&id)) {
    (Some(content), Some(importing)) => {
      let mut base = HgBase::new(&mut *importing.host, importing.directory);
      Some(importing.import.advance(&mut base, content, turn))
    }
    _ => None,
  };
  grown_and_settled(state);
  advanced
}

/// After a turn: the arena grown from its pool by what the content store holds past the shard's charge, then the
/// charge trued up to what it holds (the map's `settle`).
fn grown_and_settled(state: &mut ShardState) {
  let used = state
    .vcs_stores
    .content
    .as_ref()
    .map_or(0, ContentStore::used);
  state
    .store
    .make_room(used.saturating_sub(state.vcs_stores.charged));
  settle(state);
}

/// Ends repository `id`'s done import (the module doc): its room given back and its host closed, its index put in its
/// store's place and the empty index there given back, and what it found answered. A store gone, or holding a history
/// now, keeps what it holds, and the import's index is given back instead.
fn finish(state: &mut ShardState, id: RepositoryId) -> Result<Turned, ImportFailed> {
  let Some(Importing {
    import, reserved, ..
  }) = state.vcs_stores.imports.remove(&id)
  else {
    return Err(ImportFailed::NoStore);
  };
  state.store.budget.credit_retention(reserved);
  let turns = import.turns();
  let mut imported = import
    .into_imported()
    .ok_or(ImportFailed::Refused(ImportRefused::Ended))?;
  let index = core::mem::take(&mut imported.index);
  let (placed, whole) = place(&mut state.vcs_stores, id, index);
  if !whole {
    state.count(RELEASE_REFUSED, 1);
  }
  settle(state);
  match placed {
    Ok(()) => {
      state.count(DONE, 1);
      Ok(Turned::Done(Box::new(Finished::of(imported, turns))))
    }
    Err(failed) => {
      state.count(ABANDONED, 1);
      Err(failed)
    }
  }
}

/// Puts `index` in repository `id`'s Mercurial store's place, giving back the empty index it replaces; a store gone, or
/// holding a history now, keeps what it holds and `index` is given back instead. Whether every hold went back.
fn place(
  stores: &mut VcsStores,
  id: RepositoryId,
  index: HgIndex,
) -> (Result<(), ImportFailed>, bool) {
  let Some(content) = stores.content.as_mut() else {
    return (Err(ImportFailed::NoStore), true);
  };
  let Some(RepositoryStore::Mercurial(store)) = stores.entries.get_mut(&id) else {
    return (Err(ImportFailed::NoStore), index.release(content).is_ok());
  };
  if !holds_none(&mut store.index) {
    return (Err(ImportFailed::NotEmpty), index.release(content).is_ok());
  }
  let empty = core::mem::replace(&mut store.index, index);
  (Ok(()), empty.release(content).is_ok())
}

/// Ends repository `id`'s refused import: the import gave its index back as it refused; its room is given back and its
/// host closed.
fn end(state: &mut ShardState, id: RepositoryId) {
  if let Some(Importing { reserved, .. }) = state.vcs_stores.imports.remove(&id) {
    state.store.budget.credit_retention(reserved);
  }
}

/// Gives back repository `id`'s import under way, if there is one (the module doc's cancellation): its index's holds and
/// its room, its host closed. What freeing the repository does first (the map's `free`).
pub(crate) fn abandon(state: &mut ShardState, id: RepositoryId) {
  let Some(Importing {
    import, reserved, ..
  }) = state.vcs_stores.imports.remove(&id)
  else {
    return;
  };
  state.store.budget.credit_retention(reserved);
  let whole = match state.vcs_stores.content.as_mut() {
    Some(content) => import.abandon(content).is_ok(),
    None => true,
  };
  if !whole {
    state.count(RELEASE_REFUSED, 1);
  }
  state.count(ABANDONED, 1);
  settle(state);
}

/// Runs repository `id`'s import to its end on its owner shard, a turn at a time (the module doc): a yield, then one turn
/// within the shard's quantum on its clock, so the shard serves its inbox and pulse between turns. What the import found
/// once done, why it ended otherwise, or `None` when no import of `id` is under way or the shard's state is gone.
pub(crate) async fn run(id: RepositoryId) -> Option<Result<Box<Finished>, ImportFailed>> {
  loop {
    slates_rt::futures::yield_now().await;
    let turned = crate::state::with_state(|state| {
      let quantum_ns = quantum_of(state);
      turn(state, id, (&mut ShardClock, quantum_ns))
    })??;
    match turned {
      Ok(Turned::More) => {}
      Ok(Turned::Done(finished)) => return Some(Ok(finished)),
      Err(failed) => return Some(Err(failed)),
    }
  }
}
