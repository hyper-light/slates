//! A Subversion dump imported into its repository on the repository's owner shard (A-652 part 3; §4.17.4 piece 6 and
//! item 3.5; §4.15; §4.3; A-628, A-655).
//!
//! **What it does.** A Subversion repository `CreateRepository` made holds revision 0 and its creation's record. An
//! import gives it the history a stock `svnadmin dump` wrote (format 2 or 3): the dump's file is read from the host as a
//! base is, read-only (§4.15), and `slates-svn`'s import (`Importer`, A-628) reads it as `svnadmin load` does, handing
//! over each record it makes. Each revision is committed through the owner's commit step a client's commit takes
//! (`repository::svn_commit::commit`: the green's next version and the revision's record one entry of the repository's
//! logs, then applied); the UUID and revision 0's properties through the step for a record that moves no version
//! (`svn_commit::record`). So every imported revision is durable as it lands and a recovery reaches it, and a refusal
//! keeps the revisions before it, as `svnadmin load` keeps them. The history keeps the stream's revision numbers: a dump
//! continues the repository's history (revision 0 into one that holds no revision but 0, else the revision after its
//! youngest), so a dump made in parts loads part after part.
//!
//! **Turns.** An import runs on the owner shard a turn at a time ([`turn`], [`run`]): within a turn it reads the file a
//! slice at a time (the machine's copy slice, `DaemonConfig::copy_slice_bytes`, or what the reader asks for when that is
//! more), hands each to the import and commits what it hands back, until the turn's time is spent
//! (`DaemonConfig::copy_slice_ns`), at least one unit; a yield follows, so the shard serves its inbox between turns. A
//! revision's step is one unit, as a client's commit is (the step is not sliced, A-655 part 2b's).
//!
//! **Bounds.** The reader holds at most its admission: one client region's bytes, what one request of the
//! version-control edge may hold, as a Mercurial import's room is (`hg_import::room_of`); a line, a property entry or an
//! svndiff window past it refuses the dump. The records are lent every byte the shard could admit for each call
//! (`lend_svn`), so a revision's texts may be as large as the shard can hold; what the import holds between its turns
//! (its read buffer, the open revision's properties and texts, its transaction's new contents) is charged to the shard's
//! budget as retention at the end of each turn, trued up, and given back when it ends. One import a repository at a time.
//!
//! **Cancellation.** The import lives in the shard's map keyed by its repository, and the task that runs it holds the key
//! alone, so a task dropped between turns leaks nothing; freeing the repository gives the import back ([`abandon`]).
//!
//! **Across a restart.** A restart ends an import between its records: the revisions committed are the log's and are
//! recovered; the import is not, and a request sent again begins a new one, which refuses a stream that starts before
//! the revision after the youngest (`E160006`), so a dump of the revisions after it is what loads next.

use slates_db::catalog::{Principal, RepositoryId};
use slates_svn::commit::record::Record;
use slates_svn::dump::Admission;
use slates_svn::migration::{Importer, Importing as Imported, MigrationRefusal};
use slates_vfs::host::{HostDir, HostError, HostFile, HostFs};

use super::hg_import::room_of;
use crate::repository::svn_commit::{self, Uncommitted};
use crate::state::ShardState;

#[cfg(test)]
mod tests;

/// Counter: an import started, its file open. Format: a counter name in the status report.
pub(crate) const STARTED: &str = "vcs_store.svn_import.started";
/// Counter: a turn of an import; more turns than imports shows the imports ran a turn at a time. Format: a counter name.
pub(crate) const TURNS: &str = "vcs_store.svn_import.turns";
/// Counter: a revision an import committed through the owner's step. Format: a counter name.
pub(crate) const REVISIONS: &str = "vcs_store.svn_import.revisions";
/// Counter: an import that read its stream to the end, every record committed. Format: a counter name.
pub(crate) const DONE: &str = "vcs_store.svn_import.done";
/// Counter: an import refused midway, what it held given back and the revisions before kept. Format: a counter name.
pub(crate) const REFUSED: &str = "vcs_store.svn_import.refused";
/// Counter: an import given back before its end: its repository freed. Format: a counter name.
pub(crate) const ABANDONED: &str = "vcs_store.svn_import.abandoned";

/// A repository's import under way, kept in the shard's map between its turns (the module doc).
pub(crate) struct Importing {
  /// The import.
  importer: Importer,
  /// The host the dump is read through: the import's own, closed when the import ends.
  host: Box<dyn HostFs>,
  /// The directory holding the dump, on the host.
  directory: HostDir,
  /// The dump's file, on the host.
  file: HostFile,
  /// The bytes of the file read so far.
  read: u64,
  /// Whether the file was read to its end.
  ended: bool,
  /// The bytes read and not yet consumed by the import.
  buffer: Vec<u8>,
  /// The bytes the import needs held, from the buffer's start, before its next call reads on.
  needed: usize,
  /// The repository's owner, whom each record is committed as.
  principal: Principal,
  /// What the import holds of the shard's budget between its turns, charged as retention.
  charged: u64,
  /// The revisions committed.
  revisions: u64,
  /// The turns taken.
  turns: u64,
}

impl std::fmt::Debug for Importing {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("Importing")
      .field("read", &self.read)
      .field("ended", &self.ended)
      .field("held", &self.buffer.len())
      .field("charged", &self.charged)
      .field("revisions", &self.revisions)
      .finish_non_exhaustive()
  }
}

/// Why an import did not start, or ended before the stream's end. The revisions committed before it stay.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum LoadFailed {
  /// No Subversion repository of the id is held here with its records made.
  NoStore,
  /// An import of the repository is under way.
  Busy,
  /// The host refused the dump's file: its open or a read.
  Host(HostError),
  /// The shard's budget could not take what the import holds or a revision brings: what it could admit.
  Budget {
    /// The bytes it could admit.
    available: u64,
  },
  /// The import refused the stream (A-628's refusal, with Subversion's number), at the revision it was loading if any.
  Refused {
    /// Why.
    refusal: MigrationRefusal,
    /// The revision being loaded.
    revision: Option<u64>,
  },
  /// The owner's step refused a record the import made (A-655's), at the revision it names.
  Uncommitted {
    /// Why.
    refusal: Uncommitted,
    /// The revision the record makes or changes.
    revision: u64,
  },
}

/// What a finished import did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Loaded {
  /// The revisions it committed.
  pub(crate) revisions: u64,
  /// The repository's youngest revision after it.
  pub(crate) youngest: u64,
  /// The turns it took.
  pub(crate) turns: u64,
}

/// Where a turn left an import.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Turned {
  /// More of the stream to import.
  More,
  /// Imported to the stream's end.
  Done(Loaded),
}

/// Where one unit of a turn left the import.
enum Unit {
  /// Not at the stream's end.
  More,
  /// At the stream's end, every record committed.
  Done,
}

/// Starts repository `id`'s import, for `principal`, its owner, of the dump `name` in `directory` on `host` (the module
/// doc): its records made here and no import of it under way. Nothing is read until its turns ([`turn`]). Refused, the
/// host's file and directory are closed and nothing is kept.
pub(crate) fn begin(
  state: &mut ShardState,
  (principal, id): (&Principal, RepositoryId),
  (mut host, directory, name): (Box<dyn HostFs>, HostDir, &str),
) -> Result<(), LoadFailed> {
  let taken = takes_import(state, id)
    .and_then(|()| host.open_file(directory, name).map_err(LoadFailed::Host));
  let file = match taken {
    Ok(file) => file,
    Err(failed) => {
      host.close_dir(directory);
      return Err(failed);
    }
  };
  let buffer_bytes = usize::try_from(room_of(state)).unwrap_or(usize::MAX);
  let importing = Importing {
    importer: Importer::new(Admission { buffer_bytes }),
    host,
    directory,
    file,
    read: 0,
    ended: false,
    buffer: Vec::new(),
    needed: 1,
    principal: principal.clone(),
    charged: 0,
    revisions: 0,
    turns: 0,
  };
  state.vcs_stores.svn_imports.insert(id, importing);
  state.count(STARTED, 1);
  Ok(())
}

/// Whether repository `id` takes an import now: a Subversion repository whose records are made, and no import of it
/// under way.
fn takes_import(state: &ShardState, id: RepositoryId) -> Result<(), LoadFailed> {
  if state.vcs_stores.importing(id) {
    return Err(LoadFailed::Busy);
  }
  if state.vcs_stores.subversion_records(id).is_none() {
    return Err(LoadFailed::NoStore);
  }
  Ok(())
}

/// One turn of repository `id`'s import (the module doc): units until the turn's time is spent, at least one. `More`
/// while there is more, what the import holds then charged; at the stream's end what it did; refused, why. Ended either
/// way, what it held is given back and its file closed. `None` when no import of `id` is under way.
pub(crate) fn turn(state: &mut ShardState, id: RepositoryId) -> Option<Result<Turned, LoadFailed>> {
  let mut importing = state.vcs_stores.svn_imports.remove(&id)?;
  importing.turns = importing.turns.saturating_add(1);
  state.count(TURNS, 1);
  let began = slates_rt::futures::now_ns();
  let spend = state.config.copy_slice_ns();
  let outcome = loop {
    match unit(state, id, &mut importing) {
      Ok(Unit::More) if slates_rt::futures::now_ns().saturating_sub(began) < spend => {}
      Ok(Unit::More) => break charge_holding(state, &mut importing).map(|()| Turned::More),
      Ok(Unit::Done) => break done(state, id, &importing).map(Turned::Done),
      Err(failed) => break Err(failed),
    }
  };
  match outcome {
    Ok(Turned::More) => {
      state.vcs_stores.svn_imports.insert(id, importing);
    }
    Ok(Turned::Done(_)) => {
      end(state, importing);
      state.count(DONE, 1);
    }
    Err(_) => {
      end(state, importing);
      state.count(REFUSED, 1);
    }
  }
  Some(outcome)
}

/// One unit of the import: the file read on where the import needs more, then one call of the import over the records,
/// lent every byte the shard could admit, and the record it hands over committed.
fn unit(
  state: &mut ShardState,
  id: RepositoryId,
  importing: &mut Importing,
) -> Result<Unit, LoadFailed> {
  let slice = usize::try_from(state.config.copy_slice_bytes()).unwrap_or(usize::MAX);
  importing.fill(slice)?;
  crate::vcs_store::lend_svn(state, id, 0);
  let answer = match state
    .vcs_stores
    .subversion(id)
    .and_then(|svn| svn.records.as_mut())
  {
    Some(records) => Ok(
      importing
        .importer
        .import(records, &importing.buffer, importing.ended),
    ),
    None => Err(LoadFailed::NoStore),
  };
  crate::vcs_store::settle_svn(state, id);
  match answer? {
    Ok(Imported::NeedMore { consumed, needed }) => {
      importing.consume(consumed);
      importing.needed = needed;
      Ok(Unit::More)
    }
    Ok(Imported::Commit { consumed, record }) => {
      importing.consume(consumed);
      importing.needed = 0;
      commit_one(state, id, importing, *record)?;
      Ok(Unit::More)
    }
    Ok(Imported::Done) => Ok(Unit::Done),
    Err(refusal) => Err(refused(state, refusal, importing.importer.loading())),
  }
}

impl Importing {
  /// Reads the file on when the buffer holds fewer bytes than the import needs: what it needs, or `slice` when that is
  /// more. A read of nothing marks the file's end.
  fn fill(&mut self, slice: usize) -> Result<(), LoadFailed> {
    if self.ended || self.buffer.len() >= self.needed {
      return Ok(());
    }
    let want = self
      .needed
      .saturating_sub(self.buffer.len())
      .max(slice)
      .max(1);
    let start = self.buffer.len();
    self.buffer.resize(start.saturating_add(want), 0);
    let got = match self.buffer.get_mut(start..) {
      Some(room) => self.host.read_at(self.file, self.read, room),
      None => Ok(0),
    };
    let got = match got {
      Ok(got) => got.min(want),
      Err(error) => {
        self.buffer.truncate(start);
        return Err(LoadFailed::Host(error));
      }
    };
    self.buffer.truncate(start.saturating_add(got));
    if got == 0 {
      self.ended = true;
    }
    self.read = self
      .read
      .saturating_add(u64::try_from(got).unwrap_or(u64::MAX));
    Ok(())
  }

  /// Drops the first `consumed` bytes of the buffer, which the import is done with.
  fn consume(&mut self, consumed: usize) {
    let consumed = consumed.min(self.buffer.len());
    self.buffer.drain(..consumed);
  }

  /// The bytes the import holds outside the records: its buffer, and the open revision's properties, texts and new
  /// contents.
  fn holding(&self) -> u64 {
    u64::try_from(self.buffer.len())
      .unwrap_or(u64::MAX)
      .saturating_add(self.importer.holding())
  }
}

/// Commits `record`, which the import handed over, through the owner's step (the module doc): a revision as a client's
/// commit is, counted; the UUID or a revision's properties as a record that moves no version.
fn commit_one(
  state: &mut ShardState,
  id: RepositoryId,
  importing: &mut Importing,
  record: Record,
) -> Result<(), LoadFailed> {
  let principal = importing.principal.clone();
  let failed = |refusal: Uncommitted, revision: u64| match refusal {
    Uncommitted::Budget { available } => LoadFailed::Budget { available },
    refusal => LoadFailed::Uncommitted { refusal, revision },
  };
  match record {
    Record::Revision(revision) => {
      let number = revision.head.saturating_add(1);
      svn_commit::commit(state, (&principal, id), revision, None)
        .map_err(|refusal| failed(refusal, number))?;
      importing.revisions = importing.revisions.saturating_add(1);
      state.count(REVISIONS, 1);
    }
    other => {
      let revision = match &other {
        Record::RevisionProperties { revision, .. } => *revision,
        _ => 0,
      };
      svn_commit::record(state, (&principal, id), other)
        .map_err(|refusal| failed(refusal, revision))?;
    }
  }
  Ok(())
}

/// The import's refusal of the stream, at `revision`: past the records' room it is the shard's budget that could not
/// take it (the room was every byte the shard could admit), else the import's own.
fn refused(state: &ShardState, refusal: MigrationRefusal, revision: Option<u64>) -> LoadFailed {
  match refusal {
    MigrationRefusal::Room { .. } => LoadFailed::Budget {
      available: state.store.admittable(),
    },
    refusal => LoadFailed::Refused { refusal, revision },
  }
}

/// Charges what the import holds between its turns to the shard's budget as retention, trued up from what it charged
/// (the module doc); refused when the budget cannot take it.
fn charge_holding(state: &mut ShardState, importing: &mut Importing) -> Result<(), LoadFailed> {
  let holding = importing.holding();
  if holding > importing.charged {
    let more = holding.saturating_sub(importing.charged);
    state.store.make_room(more);
    if state.store.budget.charge_retention(more).is_err() {
      return Err(LoadFailed::Budget {
        available: state.store.admittable(),
      });
    }
  } else {
    let less = importing.charged.saturating_sub(holding);
    state.store.budget.credit_retention(less);
  }
  importing.charged = holding;
  Ok(())
}

/// What a finished import did: its revisions, the repository's youngest revision, its turns.
fn done(state: &ShardState, id: RepositoryId, importing: &Importing) -> Result<Loaded, LoadFailed> {
  let youngest = state
    .vcs_stores
    .subversion_records(id)
    .map(slates_svn::migration::Repository::youngest)
    .ok_or(LoadFailed::NoStore)?;
  Ok(Loaded {
    revisions: importing.revisions,
    youngest,
    turns: importing.turns,
  })
}

/// Ends an import: what it charged given back, its file and directory closed, its host let go.
fn end(state: &mut ShardState, mut importing: Importing) {
  state.store.budget.credit_retention(importing.charged);
  importing.host.close_file(importing.file);
  importing.host.close_dir(importing.directory);
}

/// Gives back repository `id`'s import under way, if there is one (the module doc's cancellation): what it charged, its
/// file and directory. What freeing the repository does first (the map's `free`).
pub(crate) fn abandon(state: &mut ShardState, id: RepositoryId) {
  if let Some(importing) = state.vcs_stores.svn_imports.remove(&id) {
    end(state, importing);
    state.count(ABANDONED, 1);
  }
}

/// Runs repository `id`'s import to its end on its owner shard, a turn at a time (the module doc): a yield, then one
/// turn, so the shard serves its inbox and pulse between turns. What the import did once at the stream's end, why it
/// ended otherwise, or `None` when no import of `id` is under way or the shard's state is gone.
pub(crate) async fn run(id: RepositoryId) -> Option<Result<Loaded, LoadFailed>> {
  loop {
    slates_rt::futures::yield_now().await;
    let turned = crate::state::with_state(|state| turn(state, id))??;
    match turned {
      Ok(Turned::More) => {}
      Ok(Turned::Done(loaded)) => return Some(Ok(loaded)),
      Err(failed) => return Some(Err(failed)),
    }
  }
}
