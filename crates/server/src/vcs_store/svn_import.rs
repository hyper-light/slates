//! A Subversion dump imported into its repository on the repository's owner shard (A-652 part 3; §4.17.4 piece 6 and
//! item 3.5; §4.15; §4.3; A-628, A-655).
//!
//! **What it does.** A Subversion repository `CreateRepository` made holds revision 0 and its creation's record. An
//! import gives it the history a stock `svnadmin dump` wrote (format 2 or 3): the dump's file is read from the host as a
//! base is, read-only (§4.15), and `slates-svn`'s import (`Importer`, A-628) reads it as `svnadmin load` does, handing
//! over each record it makes. Each revision is landed as a client's commit lands one (`repository::svn_commit::
//! land_revision`: checked, landed whole on the node's changeset store, then the green's next version and a reference
//! to the changeset one entry of the register, then applied; C2); the UUID and revision 0's properties through the
//! landing for a record that moves no version (`svn_commit::land_record`). A record handed over ends its turn and lands
//! before the import reads on ([`land_owed`]). So every imported revision is durable as it lands and a recovery reaches it, and a refusal
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
use crate::repository::svn_commit::staging::{Flush, Staging};
use crate::repository::svn_commit::{self, Uncommitted};
use crate::state::ShardState;

// Its import helpers serve the export's tests too (`crate::repository::svn_export`, A-652 part 3).
#[cfg(test)]
pub(crate) mod tests;

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
  /// The record the import handed over last, owed its landing before the import reads on ([`land_owed`]).
  owed: Option<Record>,
  /// The delta base the import asked for that RAM does not hold, owed through the node's store before it reads on
  /// ([`supply_owed_base`]; C2 stage B part 2 (b)).
  base_owed: Option<slates_svn::index::ContentIdentity>,
  /// The revisions staged since the last landing, landed as one changeset and one register entry (C2 stage C,
  /// `svn_commit::staging`).
  staging: Option<Staging>,
  /// The stream ended with revisions staged: they land before the import ends ([`land_owed`]).
  flush_end: bool,
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
    /// The last revision that landed: the repository's youngest as the refusal left it (C2 stage C, a batch atomic).
    landed: u64,
  },
  /// The owner's step refused a record the import made (A-655's), at the revision it names.
  Uncommitted {
    /// Why.
    refusal: Uncommitted,
    /// The revision the record makes or changes.
    revision: u64,
    /// The last revision that landed, as [`LoadFailed::Refused`]'s.
    landed: u64,
  },
}

impl LoadFailed {
  /// This refusal naming the last revision repository `id` landed (C2 stage C): its green's durable head, the
  /// register's, which a batch given up never moved.
  fn landed_in(self, state: &ShardState, id: RepositoryId) -> LoadFailed {
    let landed = svn_commit::green_of(state, id)
      .ok()
      .map_or(0, |green| state.db.partition().green_head(green));
    match self {
      LoadFailed::Refused {
        refusal, revision, ..
      } => LoadFailed::Refused {
        refusal,
        revision,
        landed,
      },
      LoadFailed::Uncommitted {
        refusal, revision, ..
      } => LoadFailed::Uncommitted {
        refusal,
        revision,
        landed,
      },
      other => other,
    }
  }
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
  /// A record handed over, owed its landing ([`land_owed`]).
  Owed,
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
    owed: None,
    base_owed: None,
    staging: None,
    flush_end: false,
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
  // A record owed its landing first, or a base owed: the import reads on only from the records it made.
  if importing.owed.is_some() || importing.base_owed.is_some() || importing.flush_end {
    state.vcs_stores.svn_imports.insert(id, importing);
    return Some(Ok(Turned::More));
  }
  importing.turns = importing.turns.saturating_add(1);
  state.count(TURNS, 1);
  let began = slates_rt::futures::now_ns();
  let spend = state.config.copy_slice_ns();
  let outcome = loop {
    match unit(state, id, &mut importing) {
      Ok(Unit::More) if slates_rt::futures::now_ns().saturating_sub(began) < spend => {}
      // A record handed over ends the turn: it lands before the import reads on ([`land_owed`]).
      Ok(Unit::More | Unit::Owed) => {
        break charge_holding(state, &mut importing).map(|()| Turned::More);
      }
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
  Some(outcome.map_err(|failed| failed.landed_in(state, id)))
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
      importing.owed = Some(*record);
      Ok(Unit::Owed)
    }
    Ok(Imported::Base { consumed, text }) => {
      importing.consume(consumed);
      importing.needed = 0;
      // The base read whole from what RAM holds (the green's head text, the content store), else owed through the
      // node's store before the import reads on.
      match base_in_ram(state, id, &text) {
        Some(bytes) => {
          supply(state, id, importing, (&text, bytes))?;
          Ok(Unit::More)
        }
        None => {
          importing.base_owed = Some(text);
          Ok(Unit::Owed)
        }
      }
    }
    // The revisions staged land before the import ends.
    Ok(Imported::Done) if importing.staging.is_some() => {
      importing.flush_end = true;
      Ok(Unit::Owed)
    }
    Ok(Imported::Done) => Ok(Unit::Done),
    Err(refusal) => Err(refused(state, refusal, importing.importer.loading())),
  }
}

/// Text `text` of repository `id` whole, as its view reads it from RAM: the green's held head text, else the content
/// store's; none where neither holds it.
fn base_in_ram(
  state: &mut ShardState,
  id: RepositoryId,
  text: &slates_svn::index::ContentIdentity,
) -> Option<Vec<u8>> {
  crate::vcs_store::svn_view::with_view(state, (id, None), |view| {
    slates_svn::migration::TextSource::text(view.texts, text).map(std::borrow::Cow::into_owned)
  })
  .flatten()
}

/// Hands repository `id`'s import the delta base `text`'s `bytes` it asked for.
fn supply(
  state: &mut ShardState,
  id: RepositoryId,
  importing: &mut Importing,
  (text, bytes): (&slates_svn::index::ContentIdentity, Vec<u8>),
) -> Result<(), LoadFailed> {
  // Charged within the room the shard lends the records, as an import's call is.
  crate::vcs_store::lend_svn(state, id, 0);
  let supplied = state
    .vcs_stores
    .subversion_records(id)
    .map(|records| importing.importer.supply_base(records, text, bytes));
  crate::vcs_store::settle_svn(state, id);
  supplied
    .ok_or(LoadFailed::NoStore)?
    .map_err(|refusal| refused(state, refusal, importing.importer.loading()))
}

/// Brings the delta base repository `id`'s import owes whole through the node's store (C2 stage B part 2 (b)), hands it
/// to the import and gives the content store's hold back. Refused, the import ends as a refused turn ends it. None
/// where no import of `id` is under way or the shard's state is gone.
pub(crate) async fn supply_owed_base(id: RepositoryId) -> Option<Result<(), LoadFailed>> {
  let owed = crate::state::with_state(|state| {
    let importing = state.vcs_stores.svn_imports.get_mut(&id)?;
    let text = importing.base_owed?;
    let len = state.vcs_stores.subversion_records(id)?.stored_len(&text)?;
    Some((text, len))
  })?;
  let Some((text, len)) = owed else {
    return Some(Ok(()));
  };
  let brought = super::svn_texts::bring_whole(id, text, len).await;
  crate::state::with_state(|state| {
    let mut importing = state.vcs_stores.svn_imports.remove(&id)?;
    importing.base_owed = None;
    let bytes = brought.then(|| base_in_ram(state, id, &text)).flatten();
    if brought {
      svn_commit::release_texts(state, None, &[text]);
    }
    let supplied = match bytes {
      Some(bytes) => supply(state, id, &mut importing, (&text, bytes)),
      None => Err(LoadFailed::Refused {
        refusal: MigrationRefusal::Corrupt,
        revision: importing.importer.loading(),
        landed: 0,
      }),
    };
    match supplied {
      Ok(()) => {
        state.vcs_stores.svn_imports.insert(id, importing);
      }
      Err(_) => {
        end(state, importing);
        state.count(REFUSED, 1);
      }
    }
    Some(supplied.map_err(|failed| failed.landed_in(state, id)))
  })
  .flatten()
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
      .saturating_add(self.staging.as_ref().map_or(0, Staging::held))
  }
}

/// Lands the record repository `id`'s import handed over last, if one is owed (the module doc): a revision as a
/// client's commit lands one (`svn_commit::land_revision`: checked, landed on the node's store, then the green's next
/// version), counted; the UUID or a revision's properties as a record that moves no version
/// (`svn_commit::land_record`). Refused, the import ends as a refused turn ends it. None where no import of `id` is
/// under way or the shard's state is gone.
pub(crate) async fn land_owed(id: RepositoryId) -> Option<Result<(), LoadFailed>> {
  let owed = crate::state::with_state(|state| {
    let importing = state.vcs_stores.svn_imports.get_mut(&id)?;
    Some((
      importing.owed.take(),
      core::mem::take(&mut importing.flush_end),
      importing.principal.clone(),
      importing.staging.take(),
    ))
  })??;
  let (record, flush_end, principal, mut staging) = owed;
  let failed = |refusal: Uncommitted, revision: u64| match refusal {
    Uncommitted::Budget { available } => LoadFailed::Budget { available },
    refusal => LoadFailed::Uncommitted {
      refusal,
      revision,
      landed: 0,
    },
  };
  let landed = match record {
    Some(Record::Revision(revision)) => {
      let number = revision.head.saturating_add(1);
      stage_owed((&principal, id), &mut staging, revision)
        .await?
        .map(|()| true)
        .map_err(|refusal| failed(refusal, number))
    }
    Some(other) => {
      let revision = match &other {
        Record::RevisionProperties { revision, .. } => *revision,
        _ => 0,
      };
      // A record that moves no version lands after the revisions staged before it.
      let flushed = match staging.take() {
        Some(batch) => batch.land(&principal, Flush::End).await?.map(drop),
        None => Ok(()),
      };
      match flushed {
        Ok(()) => svn_commit::land_record((&principal, id), other)
          .await?
          .map(|()| false)
          .map_err(|refusal| failed(refusal, revision)),
        Err(refusal) => Err(failed(refusal, revision)),
      }
    }
    // The stream's end: the revisions staged land before the import answers.
    None if flush_end => match staging.take() {
      Some(batch) => batch
        .land(&principal, Flush::End)
        .await?
        .map(|_| false)
        .map_err(|refusal| failed(refusal, 0)),
      None => Ok(false),
    },
    None => Ok(false),
  };
  crate::state::with_state(|state| match landed {
    Ok(revision) => {
      match state.vcs_stores.svn_imports.get_mut(&id) {
        Some(importing) => {
          importing.staging = staging;
          if revision {
            importing.revisions = importing.revisions.saturating_add(1);
          }
        }
        None => {
          if let Some(staging) = staging {
            staging.abandon(state);
          }
        }
      }
      if revision {
        state.count(REVISIONS, 1);
      }
      Ok(())
    }
    Err(failed) => {
      if let Some(staging) = staging {
        staging.abandon(state);
      }
      if let Some(importing) = state.vcs_stores.svn_imports.remove(&id) {
        end(state, importing);
        state.count(REFUSED, 1);
      }
      Err(failed.landed_in(state, id))
    }
  })
}

/// Stages an imported `revision` in `staging`'s batch, landing the batch first where the revision's charge would not be
/// admitted or the changeset is full (`Staging::due_before`), and beginning one where none is under way (C2 stage C).
/// Refused, the batch is given up. None where the shard's state is gone.
async fn stage_owed(
  (principal, id): (&Principal, RepositoryId),
  staging: &mut Option<Staging>,
  revision: slates_svn::commit::revision::Revision,
) -> Option<Result<(), Uncommitted>> {
  let due = staging.as_ref().and_then(|batch| {
    crate::state::with_state(|state| batch.due_before(state, &revision)).flatten()
  });
  if let Some(cause) = due
    && let Some(batch) = staging.take()
    && let Err(refused) = batch.land(principal, cause).await?
  {
    return Some(Err(refused));
  }
  let mut batch = match staging.take() {
    Some(batch) => batch,
    None => match Staging::begin(id).await? {
      Ok(batch) => batch,
      Err(refused) => return Some(Err(refused)),
    },
  };
  match batch.stage(revision).await? {
    Ok(()) => {
      *staging = Some(batch);
      Some(Ok(()))
    }
    Err(refused) => {
      let _ = crate::state::with_state(|state| batch.abandon(state));
      Some(Err(refused))
    }
  }
}

/// The import's refusal of the stream, at `revision`: past the records' room it is the shard's budget that could not
/// take it (the room was every byte the shard could admit), else the import's own.
fn refused(state: &ShardState, refusal: MigrationRefusal, revision: Option<u64>) -> LoadFailed {
  match refusal {
    MigrationRefusal::Room { .. } => LoadFailed::Budget {
      available: state.store.admittable(),
    },
    refusal => LoadFailed::Refused {
      refusal,
      revision,
      landed: 0,
    },
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
  // Revisions staged and never landed are given up: the records made again from what the register names.
  if let Some(staging) = importing.staging.take() {
    staging.abandon(state);
  }
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
    if let Err(failed) = land_owed(id).await? {
      return Some(Err(failed));
    }
    if let Err(failed) = supply_owed_base(id).await? {
      return Some(Err(failed));
    }
    let turned = crate::state::with_state(|state| turn(state, id))??;
    match turned {
      Ok(Turned::More) => {}
      Ok(Turned::Done(loaded)) => return Some(Ok(loaded)),
      Err(failed) => return Some(Err(failed)),
    }
  }
}
