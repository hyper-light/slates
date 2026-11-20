//! A Mercurial repository's directory imported into its store on its owner shard (A-675; §4.17.3 piece 3; §4.15; §4.3;
//! A-649 parts 3b and 4; A-662), and made durable by the register's entry it lands (A-687).
//!
//! **What it does.** A Mercurial repository's store is made with its record as an empty index (`HgStore`). An import
//! gives it the history of a repository on the host: the repository's directory is read as a base, read-only (§4.15),
//! through the server's adapter (`crate::hg_base`), and `slates-hg`'s import (`Import`) is advanced a turn at a time on
//! the repository's owner shard ([`turn`], [`run`]), each turn lent the host and the shard's content store, within the
//! shard's quantum (§4.3's step budget) on the shard's clock, with a yield between turns. Once it is done, its host is
//! closed and what it found is answered ([`Finished`]: its counts, phase roots and bookmarks), and it waits in the map,
//! holding its index, until it lands ([`land`]). Refused, abandoned, or met at its landing by a store that is no longer
//! empty, it gives back everything it held, and the store is as it was.
//!
//! **Durable** (A-687; §4.17.1 "Durability and residency"; R8). An import lands as one entry of the repository's logs
//! (A-635's append, `repository::push::push_imported`), in its caller's transaction, which records the request's
//! completion beside it (`repository::import`): the entry's objects are the import's record (`slates_hg::import::record`:
//! every revision it kept, whole, and the markers), so a recovery, or a successor that cannot read the importing host's
//! directory, replays it into the same index (the map's `replay_hg`); its state is the phase roots, each added to the
//! phases log (§4.17.3 decision 3); and each bookmark is a green bound to `bookmark:<label>:` (decision 2), made from
//! the repository's root at its head and moved to the bookmark's changeset by a work A-618's builder makes from the
//! imported index, as a push makes a bookmark's (A-657). The store takes the index between the register's guard and the
//! entry's apply ([`place_imported`]), as a git push's store commits its objects (A-662), so an entry the guard refuses
//! never reaches the store, and an apply refused after it makes the store again from the log. A line for each branch
//! head (decision 1) is not made here: owed (GAPS), so a push after an import finds the root as its first line.
//!
//! **Bounds.** An import holds, between its turns as within them, at most its room (`Admission`): the listing, the log
//! it reads (an inline revlog whole), a text being rebuilt and the text before it. The room is what the shard could admit
//! when the import starts (A-720 part 3; it was one client region's bytes, 278,528 B, which refused Mercurial's own
//! repository), a bound only: the shard is charged as retention what the import holds between turns
//! (`Import::holding`), trued up after each turn and given back when it is done, so the charge covers what the import
//! holds while it waits.
//! What the import keeps in the content store is charged as every store's is (the map's module doc): each turn is lent
//! every byte the shard could admit now, then the arena is grown from its pool by what the turn kept and the charge
//! trued up to what the content store holds. One import a repository at a time, so the imports under way or waiting to
//! land are bounded by the partition's repositories. Each holds its own host, which its caller opened for the directory
//! and which is closed when the import is done; the adapter's lend, made and ended within each turn, holds at most a
//! directory and two files. Its landing holds the record, whole, and the bookmarks' works, each within what the room
//! leaves of the others; the record is then charged to the partition's merge budget with the entry, as a git push's is.
//!
//! **Cancellation.** The import lives in the shard's map, keyed by its repository, and the task that runs it holds the
//! key alone, so a task dropped mid-import leaks nothing: the import waits in the map, holding what it held, until a
//! task runs or lands it, or its repository is freed, which gives it back ([`abandon`]).
//!
//! **Owed** (GAPS' §4.17.3 entry). The lines of an imported repository's branch heads (decision 1); a host's pending
//! fetch retried at the next turn rather than ending the import; the adapter's handles held between turns beside the
//! import; and, for a large repository, the bookmarks' works made in slices rather than whole at the landing (the record
//! is written a part a turn before it since A-732, `Stage::Writing`), and the snapshot carrying the store's tables, so a
//! recovery replays only the log's suffix.

use slates_db::catalog::{
  MemberOrigin, Principal, RepositoryId, StateChange, VolumeId as DbVolumeId,
};
use slates_hg::import::record::{self, WriteRefused};
use slates_hg::import::{Clock, Import, ImportRefused, Imported, Progress, Turn};
use slates_hg::index::HgIndex;
use slates_hg::phases::PhaseHead;
use slates_hg::push::held::IndexHeld;
use slates_hg::push::tree::{self, LayoutRefusal, Move, WorkRefused};
use slates_hg::push::verify::Verified;
use slates_hg::{Admission, Node};
use slates_ipc::protocol::{ImportFault, Refusal};
use slates_merge::push::{HEAD_ATTRIBUTE, PushRefused, ROOT};
use slates_vcs::ContentStore;
use slates_vfs::host::{HostDir, HostFs};

use super::{RELEASE_REFUSED, RepositoryStore, VcsStores, lend, settle};
use crate::hg_base::{BaseRefused, HgBase};
use crate::repository::push::{AppendRefused, BranchChange, ChangeRefusal, EntryParts};
use crate::state::ShardState;

#[cfg(test)]
pub(crate) mod tests;

/// Counter: an import started, its requirements and listing read. Format: a counter name in the status report.
pub(crate) const STARTED: &str = "vcs_store.hg_import.started";
/// Counter: a turn of an import; more turns than imports shows the imports ran a turn at a time. Format: a counter name.
pub(crate) const TURNS: &str = "vcs_store.hg_import.turns";
/// Counter: an import done, waiting to land. Format: a counter name.
pub(crate) const DONE: &str = "vcs_store.hg_import.done";
/// Counter: an import refused midway, everything it held given back. Format: a counter name.
pub(crate) const REFUSED: &str = "vcs_store.hg_import.refused";
/// Counter: an import given back before it landed: its repository freed. Format: a counter name.
pub(crate) const ABANDONED: &str = "vcs_store.hg_import.abandoned";
/// Counter: an import landed: its entry appended and its index in its store's place (A-687). Format: a counter name.
pub(crate) const LANDED: &str = "vcs_store.hg_import.landed";
/// Counter: an import whose landing was refused, its index given back (A-687). Format: a counter name.
pub(crate) const LANDING_REFUSED: &str = "vcs_store.hg_import.landing_refused";

/// A repository's import, kept in the shard's map between its turns and until it lands (the module doc).
pub(crate) struct Importing {
  /// Where it stands.
  stage: Stage,
  /// What the import's room holds of the shard's budget: charged as retention when it started, given back once it is
  /// done, refused or abandoned.
  reserved: u64,
}

/// Where an import stands.
enum Stage {
  /// Under way: the import, the host its directory is read through (the import's own, closed when it is done) and the
  /// repository's directory on the host.
  Running {
    /// The import, boxed: it is by far the largest of a stage.
    import: Box<Import>,
    /// The host.
    host: Box<dyn HostFs>,
    /// The directory.
    directory: HostDir,
  },
  /// Its record being written a revision a turn (A-732: written whole at the landing, a large repository's held its
  /// shard past the watchdog's window): what it imported, the room the landing runs within, the writer, and what its
  /// end answers.
  Writing {
    /// What it imported.
    imported: Box<Imported>,
    /// The room the landing runs within.
    room: u64,
    /// The record's writer.
    writer: Box<record::Writer>,
    /// What its end answers, its turns counted on.
    finished: Box<Finished>,
  },
  /// Done: its index and state, waiting to land ([`land`]), the room it ran within, which its record carries, and its
  /// record, written.
  Done {
    /// What it imported.
    imported: Box<Imported>,
    /// The room it ran within.
    room: u64,
    /// Its record.
    record: Vec<u8>,
  },
}

impl std::fmt::Debug for Importing {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let mut shown = formatter.debug_struct("Importing");
    match &self.stage {
      Stage::Running { import, .. } => shown
        .field("turns", &import.turns())
        .field("units", &import.units()),
      Stage::Writing { finished, .. } => shown
        .field("writing", &true)
        .field("turns", &finished.turns),
      Stage::Done { room, .. } => shown.field("done", &true).field("room", room),
    };
    shown
      .field("reserved", &self.reserved)
      .finish_non_exhaustive()
  }
}

/// Why an import did not start, did not end, or did not land. Each leaves the store as it was.
#[derive(Clone, Debug, PartialEq)]
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
  /// Its landing was refused (A-687): the entry by the register's guard or the budget, a bookmark's green by the
  /// namespace's rules, or its record or a bookmark's work past what the landing may hold; the wire's refusal.
  Landing(Refusal),
}

/// Where a turn left an import.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Turned {
  /// More of the repository to import.
  More,
  /// Imported, waiting to land, and what it found beside its history.
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
  /// What `imported` found, over `turns` turns; its index stays with it.
  fn of(imported: &Imported, turns: u64) -> Finished {
    Finished {
      changesets: imported.changesets,
      manifests: imported.manifests,
      file_logs: imported.file_logs,
      file_revisions: imported.file_revisions,
      markers: imported.markers,
      phase_roots: imported.phase_roots.clone(),
      dropped_phase_roots: imported.dropped_phase_roots,
      bookmarks: imported.bookmarks.clone(),
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

/// Derived: the most an import may hold at once, its room (the module doc; A-720 part 3): what its shard could admit
/// when it starts (`Store::admittable`, as a push is admitted, `crate::git_push::push_room`), never one request's room,
/// since a repository's logs, and so what reading one holds, are as large as the repository. The room is a bound; what
/// the shard is charged is what the import holds ([`Import::holding`]), trued up after each turn.
pub(crate) fn room_of(state: &ShardState) -> u64 {
  state.store.admittable()
}

/// Derived: the nanoseconds a turn may spend: the runtime's step quantum (§4.3), as the shard's other long work is cut,
/// else the configuration's step budget off the runtime.
pub(crate) fn quantum_of(state: &ShardState) -> u64 {
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
  let started = Import::start(
    &mut HgBase::new(&mut *host, directory),
    Admission::new(room),
  );
  match started {
    Ok(import) => {
      let holding = import.holding();
      reserve(state, holding)?;
      let importing = Importing {
        stage: Stage::Running {
          import: Box::new(import),
          host,
          directory,
        },
        reserved: holding,
      };
      state.vcs_stores.imports.insert(id, importing);
      state.count(STARTED, 1);
      Ok(())
    }
    Err(refused) => Err(ImportFailed::Refused(refused)),
  }
}

/// Whether repository `id`'s store takes an import now: a Mercurial store holding no row, and no import of it under way
/// or waiting to land.
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

/// Whether `index` holds no row: the empty index a repository's record was made with, which an import's index, live or
/// replayed, takes the place of (the map's `replay_hg`).
pub(super) fn holds_none(index: &mut HgIndex) -> bool {
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
/// least one, lent the host and the content store. `More` while there is more; once it is done, what it found, its
/// index waiting to land; refused, everything it held is given back. `None` when no import of `id` is under way.
pub(crate) fn turn(
  state: &mut ShardState,
  id: RepositoryId,
  (clock, quantum_ns): (&mut dyn Clock, u64),
) -> Option<Result<Turned, ImportFailed>> {
  if matches!(
    state.vcs_stores.imports.get(&id),
    Some(Importing {
      stage: Stage::Writing { .. },
      ..
    })
  ) {
    state.count(TURNS, 1);
    return Some(write_turn(state, id, (clock, quantum_ns)));
  }
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
  let running = matches!(
    state.vcs_stores.imports.get(&id),
    Some(Importing {
      stage: Stage::Running { .. },
      ..
    })
  );
  if !running {
    return None;
  }
  lend(&mut state.store, &mut state.vcs_stores, 0);
  let lent = super::Lent::new(&mut state.store, &mut state.vcs_stores);
  let stores = &mut *lent.stores;
  let advanced = match (stores.content.as_mut(), stores.imports.get_mut(&id)) {
    (
      Some(content),
      Some(Importing {
        stage: Stage::Running {
          import,
          host,
          directory,
        },
        ..
      }),
    ) => {
      let mut base = HgBase::new(&mut **host, *directory);
      Some(import.advance(&mut base, content, turn))
    }
    _ => None,
  };
  drop(lent);
  grown_and_settled(state);
  true_up_holding(state, id);
  advanced
}

/// Trues the shard's charge for repository `id`'s running import up to what it holds between turns
/// ([`Import::holding`], A-720 part 3): more charged as retention, less credited back. A charge the shard refuses is
/// counted ([`super::CHARGE_SHORT`]) and the import goes on within its room.
fn true_up_holding(state: &mut ShardState, id: RepositoryId) {
  let Some(Importing {
    stage: Stage::Running { import, .. },
    reserved,
  }) = state.vcs_stores.imports.get(&id)
  else {
    return;
  };
  let (holding, charged) = (import.holding(), *reserved);
  let short = if holding > charged {
    state
      .store
      .budget
      .charge_retention(holding.saturating_sub(charged))
      .is_err()
  } else {
    state
      .store
      .budget
      .credit_retention(charged.saturating_sub(holding));
    false
  };
  if short {
    state.count(super::CHARGE_SHORT, 1);
  } else if let Some(Importing { reserved, .. }) = state.vcs_stores.imports.get_mut(&id) {
    *reserved = holding;
  }
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

/// Repository `id`'s import done (the module doc): its room given back and its host closed, and what it imported kept in
/// the map, waiting to land; what it found answered.
fn finish(state: &mut ShardState, id: RepositoryId) -> Result<Turned, ImportFailed> {
  let (import, reserved) = match state.vcs_stores.imports.remove(&id) {
    Some(Importing {
      stage: Stage::Running { import, .. },
      reserved,
    }) => (*import, reserved),
    Some(done) => {
      state.vcs_stores.imports.insert(id, done);
      return Err(ImportFailed::Busy);
    }
    None => return Err(ImportFailed::NoStore),
  };
  state.store.budget.credit_retention(reserved);
  let turns = import.turns();
  let imported = import
    .into_imported()
    .ok_or(ImportFailed::Refused(ImportRefused::Ended))?;
  let finished = Finished::of(&imported, turns);
  // The landing's room, which bounds its record and its bookmarks' works: what the shard could admit now (A-720 part 3),
  // the import's own holding given back above.
  let room = room_of(state);
  let content = state
    .vcs_stores
    .content
    .as_ref()
    .ok_or(ImportFailed::NoStore)?;
  let writer = match record::Writer::begin(&imported.index, content, room) {
    Ok(writer) => writer,
    Err(refused) => {
      give_back(state, imported.index);
      return Err(ImportFailed::Landing(write_refusal(state, &refused)));
    }
  };
  let writing = Importing {
    stage: Stage::Writing {
      imported: Box::new(imported),
      room,
      writer: Box::new(writer),
      finished: Box::new(finished),
    },
    reserved: 0,
  };
  state.vcs_stores.imports.insert(id, writing);
  Ok(Turned::More)
}

/// One turn of repository `id`'s record being written (A-732): parts written until the record is whole or `quantum_ns`
/// is spent on `clock`, at least one; whole, the import is done, waiting to land. Refused, everything it held is given
/// back.
fn write_turn(
  state: &mut ShardState,
  id: RepositoryId,
  (clock, quantum_ns): (&mut dyn Clock, u64),
) -> Result<Turned, ImportFailed> {
  let start = clock.now_ns();
  let lent = super::Lent::new(&mut state.store, &mut state.vcs_stores);
  let stores = &mut *lent.stores;
  let written = match (stores.content.as_ref(), stores.imports.get_mut(&id)) {
    (
      Some(content),
      Some(Importing {
        stage:
          Stage::Writing {
            imported,
            writer,
            finished,
            ..
          },
        ..
      }),
    ) => {
      finished.turns = finished.turns.saturating_add(1);
      loop {
        match writer.step(&imported.index, content) {
          Ok(true) if clock.now_ns().saturating_sub(start) < quantum_ns => {}
          other => break other,
        }
      }
    }
    _ => return Err(ImportFailed::NoStore),
  };
  drop(lent);
  match written {
    Ok(true) => Ok(Turned::More),
    Ok(false) => written_whole(state, id),
    Err(refused) => {
      let refusal = write_refusal(state, &refused);
      end(state, id);
      state.count(REFUSED, 1);
      Err(ImportFailed::Landing(refusal))
    }
  }
}

/// Repository `id`'s record written whole: the import done, waiting to land with its record; what it found answered.
fn written_whole(state: &mut ShardState, id: RepositoryId) -> Result<Turned, ImportFailed> {
  let Some(Importing {
    stage: Stage::Writing {
      imported,
      room,
      writer,
      finished,
    },
    ..
  }) = state.vcs_stores.imports.remove(&id)
  else {
    return Err(ImportFailed::NoStore);
  };
  let Some(record) = writer.finish() else {
    give_back(state, imported.index);
    return Err(ImportFailed::Landing(Refusal::ContentUnavailable));
  };
  let done = Importing {
    stage: Stage::Done {
      imported,
      room,
      record,
    },
    reserved: 0,
  };
  state.vcs_stores.imports.insert(id, done);
  state.count(DONE, 1);
  Ok(Turned::Done(finished))
}

/// The register's refusal for a record that could not be written.
fn write_refusal(state: &ShardState, refused: &WriteRefused) -> Refusal {
  match refused {
    WriteRefused::Allocation => Refusal::BudgetExceeded {
      available: state.store.admittable(),
    },
    WriteRefused::Unreadable { .. } | WriteRefused::UnreadableMarker { .. } => {
      Refusal::ContentUnavailable
    }
  }
}

/// Puts `index` in repository `id`'s Mercurial store's place, giving back the empty index it replaces: whether every
/// hold of that one went back. A store gone, or holding a history now, keeps what it holds, and `index` comes back.
fn place(
  stores: &mut VcsStores,
  id: RepositoryId,
  index: HgIndex,
) -> Result<bool, Box<(ImportFailed, HgIndex)>> {
  let Some(content) = stores.content.as_mut() else {
    return Err(Box::new((ImportFailed::NoStore, index)));
  };
  let Some(RepositoryStore::Mercurial(store)) = stores.entries.get_mut(&id) else {
    return Err(Box::new((ImportFailed::NoStore, index)));
  };
  if !holds_none(&mut store.index) {
    return Err(Box::new((ImportFailed::NotEmpty, index)));
  }
  let empty = core::mem::replace(&mut store.index, index);
  let released = empty.release(content).is_ok();
  stores.index_changed(id);
  Ok(released)
}

/// Puts the index an import's entry carries in repository `id`'s store's place, between the register's guard and the
/// entry's apply (the module doc; `repository::push::push_imported`). Placed, `index` is empty; refused, the index is
/// back in `index` and the wire's refusal answered: `NotEmpty` for a store that took a history meanwhile, or
/// `ContentUnavailable` for a store gone.
pub(crate) fn place_imported(
  state: &mut ShardState,
  id: RepositoryId,
  index: &mut Option<HgIndex>,
) -> Result<(), Refusal> {
  let Some(taken) = index.take() else {
    return Err(Refusal::ContentUnavailable);
  };
  match place(&mut state.vcs_stores, id, taken) {
    Ok(whole) => {
      if !whole {
        state.count(RELEASE_REFUSED, 1);
      }
      settle(state);
      Ok(())
    }
    Err(refused) => {
      let (failed, back) = *refused;
      *index = Some(back);
      Err(match failed {
        ImportFailed::NotEmpty => Refusal::ImportRefused {
          fault: ImportFault::NotEmpty,
        },
        _ => Refusal::ContentUnavailable,
      })
    }
  }
}

/// Ends repository `id`'s refused import: the import gave its index back as it refused; its room is given back and its
/// host closed.
fn end(state: &mut ShardState, id: RepositoryId) {
  if let Some(Importing { reserved, .. }) = state.vcs_stores.imports.remove(&id) {
    state.store.budget.credit_retention(reserved);
  }
}

/// Gives back repository `id`'s import, under way or waiting to land, if there is one (the module doc's cancellation):
/// its index's holds and its room, its host closed. What freeing the repository does first (the map's `free`).
pub(crate) fn abandon(state: &mut ShardState, id: RepositoryId) {
  let Some(Importing { stage, reserved }) = state.vcs_stores.imports.remove(&id) else {
    return;
  };
  state.store.budget.credit_retention(reserved);
  let whole = match (state.vcs_stores.content.as_mut(), stage) {
    (Some(content), Stage::Running { import, .. }) => (*import).abandon(content).is_ok(),
    (Some(content), Stage::Done { imported, .. } | Stage::Writing { imported, .. }) => {
      let Imported { index, .. } = *imported;
      index.release(content).is_ok()
    }
    (None, _) => true,
  };
  if !whole {
    state.count(RELEASE_REFUSED, 1);
  }
  state.count(ABANDONED, 1);
  settle(state);
}

/// Gives back an index a landing did not place, then settles.
fn give_back(state: &mut ShardState, index: HgIndex) {
  let whole = match state.vcs_stores.content.as_mut() {
    Some(content) => index.release(content).is_ok(),
    None => true,
  };
  if !whole {
    state.count(RELEASE_REFUSED, 1);
  }
  settle(state);
}

/// Lands repository `id`'s done import (A-687, the module doc's "Durable"): its entry appended through A-635's append,
/// in the caller's transaction when one is open, its index put in its store's place between the register's guard and
/// the entry's apply. Refused, nothing is recorded, and the index and everything it held are given back. `Busy` while
/// the import is still under way, `NoStore` when none is held for `id`.
pub(crate) fn land(state: &mut ShardState, id: RepositoryId) -> Result<(), ImportFailed> {
  let (mut imported, room, record) = match state.vcs_stores.imports.remove(&id) {
    Some(Importing {
      stage: Stage::Done {
        imported,
        room,
        record,
      },
      ..
    }) => (imported, room, record),
    Some(running) => {
      state.vcs_stores.imports.insert(id, running);
      return Err(ImportFailed::Busy);
    }
    None => return Err(ImportFailed::NoStore),
  };
  let entry = super::lent_state(state, |state| {
    landing_entry(state, id, (&*imported, room), record)
  });
  let mut index = Some(core::mem::take(&mut imported.index));
  let appended = entry.and_then(|(owner, changes, parts, record)| {
    crate::repository::push::push_imported(
      state,
      (&owner, id),
      changes,
      parts,
      (&mut index, record),
    )
    .map_err(landing_refusal)
  });
  if let Some(index) = index.take() {
    give_back(state, index);
  }
  match appended {
    Ok(_) => {
      state.count(LANDED, 1);
      Ok(())
    }
    Err(refusal) => {
      state.count(LANDING_REFUSED, 1);
      Err(ImportFailed::Landing(refusal))
    }
  }
}

/// What an import's entry is made of: its owner, the bookmarks' greens, the phase roots, and its record.
type Landing = (Principal, Vec<BranchChange>, EntryParts, Vec<u8>);

/// The entry repository `id`'s import lands (the module doc's "Durable"): its owner, each bookmark's green, the phase
/// roots added, and the import's record carrying `room`, the room it ran within; or the wire's refusal.
fn landing_entry(
  state: &ShardState,
  id: RepositoryId,
  (imported, room): (&Imported, u64),
  record: Vec<u8>,
) -> Result<Landing, Refusal> {
  let content = state
    .vcs_stores
    .content
    .as_ref()
    .ok_or(Refusal::ContentUnavailable)?;
  let repositories = state.db.partition().repositories();
  let owner = repositories
    .repository(id)
    .map(|record| record.owner.clone())
    .ok_or(Refusal::NotFound)?;
  let root = repositories
    .members_of(id)
    .find(|member| matches!(member.origin, MemberOrigin::Root))
    .map(|member| member.green)
    .ok_or(Refusal::NotFound)?;
  let held = IndexHeld::new(&imported.index, content);
  let name = import_name(&record);
  let changes = bookmark_greens(state, (root, &imported.bookmarks), (&held, name, room))?;
  // The record rides as the import's stored objects (`push_imported`), never as the parts' own.
  let parts = EntryParts {
    checks: Vec::new(),
    names: Vec::new(),
    state: phase_roots_added(&imported.phase_roots),
    objects: Vec::new(),
  };
  Ok((owner, changes, parts, record))
}

/// Format: the BLAKE3 context an import's name is derived from its record in: every work of its entry begins its
/// identity with it (A-618's push name).
const NAME_CONTEXT: &str = "slates 2026-11-19 a Mercurial import's name";

/// The import's name: BLAKE3 under [`NAME_CONTEXT`] over its record, so two imports of other histories never share one.
fn import_name(record: &[u8]) -> [u8; 32] {
  let mut hasher = blake3::Hasher::new_derive_key(NAME_CONTEXT);
  hasher.update(record);
  *hasher.finalize().as_bytes()
}

/// Each phase root as the phases log adds it (§4.17.3 decision 3): its phase's number and its node, in the import's
/// order (by phase, then node).
fn phase_roots_added(roots: &[PhaseHead]) -> Vec<StateChange> {
  roots
    .iter()
    .filter_map(|root| {
      let phase = u8::try_from(root.phase.number()).ok()?;
      Some(StateChange::PhaseRootAdded {
        phase,
        node: root.node.to_vec(),
      })
    })
    .collect()
}

/// What a bookmark's work is built from: the imported index as a push's `Held`, the import's name, and the room the
/// works may take between them.
type Works<'h> = (&'h IndexHeld<'h>, [u8; 32], u64);

/// Each bookmark a green (§4.17.3 decision 2): made from `root` at its head, as a push makes a bookmark from a line's
/// version, and moved to the bookmark's changeset by a work built from the imported index (none where the root's head is
/// that changeset already); the works within the room between them.
fn bookmark_greens(
  state: &ShardState,
  (root, bookmarks): (DbVolumeId, &[(Vec<u8>, Node)]),
  (held, name, room): Works<'_>,
) -> Result<Vec<BranchChange>, Refusal> {
  let engine = state.greens.get(&root).ok_or(Refusal::ContentUnavailable)?;
  let version = state.db.partition().green_head(root);
  let head = engine
    .xattr(ROOT, HEAD_ATTRIBUTE)
    .and_then(|bytes| Node::try_from(bytes).ok());
  let mut admission = Admission::new(room);
  let mut changes = Vec::with_capacity(bookmarks.len());
  for (mark, node) in bookmarks {
    let work = if head == Some(*node) {
      None
    } else {
      let moved = Move {
        name,
        base_version: version,
        base: head.as_ref(),
        head: node,
      };
      let built = tree::work(
        &moved,
        &engine,
        (&Verified::default(), held),
        admission.left(),
        &mut admission,
      )
      .map_err(|refused| work_refused(&refused, room))?;
      admission
        .charge(u64::try_from(built.increment.post_state.len()).unwrap_or(u64::MAX))
        .map_err(|_| past_room(room))?;
      Some(built.increment)
    };
    changes.push(BranchChange::Create {
      name: crate::repository::hg_push::bookmark_name(mark),
      from: root,
      version,
      work,
    });
  }
  Ok(changes)
}

/// `ImportRefused { PastRoom }` naming the import's room.
fn past_room(room: u64) -> Refusal {
  Refusal::ImportRefused {
    fault: ImportFault::PastRoom { room },
  }
}

/// The wire's refusal for a bookmark's work not made: past the room; a path or a link's target slates cannot hold, or a
/// tree that holds a name twice, which is the repository's format (`Format`); a changeset or manifest the imported
/// index does not answer, which a landed import never meets (`ContentUnavailable`).
fn work_refused(refused: &WorkRefused, room: u64) -> Refusal {
  match refused {
    WorkRefused::Layout(LayoutRefusal::PastAdmission(_))
    | WorkRefused::Push(PushRefused::PastRoom { .. }) => past_room(room),
    WorkRefused::Push(
      PushRefused::NameNotHeld { .. }
      | PushRefused::TargetNotHeld { .. }
      | PushRefused::DuplicateName { .. }
      | PushRefused::KindChanged { .. },
    ) => Refusal::ImportRefused {
      fault: ImportFault::Format,
    },
    _ => Refusal::ContentUnavailable,
  }
}

/// The wire's refusal for a landing's append refused: the entry's or the repository's own, or the first change's.
fn landing_refusal(refused: AppendRefused) -> Refusal {
  match refused {
    AppendRefused::Repository(refusal) | AppendRefused::Entry(refusal) => refusal,
    AppendRefused::Changes(changes) => changes
      .into_iter()
      .find_map(|change| match change.why {
        ChangeRefusal::Refused(refusal) => Some(refusal),
        _ => None,
      })
      .unwrap_or(Refusal::ContentUnavailable),
  }
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
      Ok(Turned::Done(finished)) => {
        // Its record names its contents by identity (A-729 part 2), so its landing commits only once the shard's image
        // holding them is published (durable before named); one not published gives the import back.
        let shard = crate::state::with_state(|state| state.shard)?;
        if super::homes::kept_here_durable(shard).await {
          return Some(Ok(finished));
        }
        crate::state::with_state(|state| abandon(state, id))?;
        return Some(Err(ImportFailed::Landing(Refusal::ContentUnavailable)));
      }
      Err(failed) => return Some(Err(failed)),
    }
  }
}
