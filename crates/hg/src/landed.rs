//! A Mercurial repository's history on the changeset log (A-767 C4; Ada's ruling of 2026-11-20 ~13:35: the changeset
//! is the one stored unit, everything else derived; main's ruling of 18:30: hg's side built against slates-store's
//! seam, the store behind a double until w-store's server wiring lands).
//!
//! **Landing.** A push lands as one changeset: its `Record` section is its head alone
//! (`record::write_push_record`: the source, the room its intake ran within, the changegroup's version), its `Payload`
//! section the changegroup as it arrived, held once by the log. The `Texts` section (each kept revision's text
//! identity, base and place, `push::intake::KeptText`) is written by w-perf's encoder in slates-store, agreed
//! 2026-11-20, and is appended here once that encoder lands.
//!
//! **Replay.** A restart walks the repository's changesets in order and takes each again through the live push's
//! intake (`record::Replay::begin_sectioned`), a unit a step, each step handed the payload as the log lends it, so a
//! large changeset never holds its shard past a step (A-732) and nothing is copied out of the log first.
//!
//! **The seam.** [`ChangesetLog`] is what this module asks of the store: append a changeset's sections, lend one
//! section of a landed changeset, and name the next sequence. slates-store's `Store` answers it on a shard that holds
//! the repository's store; the tests answer it from memory.

use std::borrow::Cow;

use slates_vcs::{ContentStore, IndexRefused};

use crate::changegroup::Version;
use crate::import::record::{self, Progress, Refused as RecordRefused, Replay, Replayed};
use crate::index::HgIndex;

/// A changeset's section, as Mercurial's history uses the changeset log's (slates-store's `SectionKind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
  /// The tool's own record: a push's head (`record::write_push_record`).
  Record,
  /// The bytes the client sent: a push's changegroup as it arrived.
  Payload,
  /// Each kept revision's text identity, base and place in the payload.
  Texts,
}

/// The changeset log of one repository, as Mercurial's landing and replay use it (module docs, "The seam").
pub trait ChangesetLog {
  /// The log's refusal, carried through untouched.
  type Refused;

  /// Appends one changeset of `sections`, durable when it answers: its sequence in the repository.
  fn append(&mut self, sections: &[(Section, &[u8])]) -> Result<u64, Self::Refused>;

  /// The `section` of the landed changeset at `sequence`, lent where the log can; `None` where it has none.
  fn section(&self, sequence: u64, section: Section) -> Option<Cow<'_, [u8]>>;

  /// The sequence of the first landed changeset after `after` (the first of all for `None`); `None` past the last.
  fn next(&self, after: Option<u64>) -> Option<u64>;
}

/// Lands a push as one changeset (module docs, "Landing"): its head as the `Record` section, `changegroup` (of
/// `version`) as the `Payload`, `room` the room its intake ran within. Its sequence.
pub fn land_push<L: ChangesetLog>(
  log: &mut L,
  room: u64,
  (version, changegroup): (Version, &[u8]),
) -> Result<u64, L::Refused> {
  let head = record::write_push_record(room, version);
  log.append(&[(Section::Record, &head), (Section::Payload, changegroup)])
}

/// Why a replay of the changeset log was refused; the index is as it was before the changeset that refused.
#[derive(Debug)]
pub enum Refused {
  /// A landed changeset with no `Record` or no `Payload` section.
  Missing {
    /// Its sequence.
    sequence: u64,
    /// The section it lacks.
    section: Section,
  },
  /// The changeset's record refused its replay.
  Record {
    /// Its sequence.
    sequence: u64,
    /// Why.
    refused: RecordRefused,
  },
  /// The changeset under way refused, and its fork could not be given back whole: the content store's holds are off.
  Unreleased {
    /// Its sequence.
    sequence: u64,
    /// Why it refused.
    cause: Box<Refused>,
    /// Why the fork was not given back.
    release: IndexRefused,
  },
}

/// `cause`, the refusal of the changeset at `sequence` under way, once `replay` is given back: [`Refused::Unreleased`]
/// where it could not be.
fn given_back(sequence: u64, replay: Replay, store: &mut ContentStore, cause: Refused) -> Refused {
  match replay.abandon(store) {
    Ok(()) => cause,
    Err(release) => Refused::Unreleased {
      sequence,
      cause: Box::new(cause),
      release,
    },
  }
}

/// Whether a replay has more to take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stepped {
  /// More units follow.
  More,
  /// Every changeset is replayed.
  Done,
}

/// A replay of a repository's changeset log into its index (module docs, "Replay"), a unit a step.
#[derive(Default)]
pub struct Replaying {
  /// The sequence replayed last, none before the first.
  done: Option<u64>,
  /// The changeset under way: its sequence and its replay.
  under_way: Option<(u64, Replay)>,
  /// What the finished changesets brought.
  replayed: Vec<Replayed>,
}

impl Replaying {
  /// A replay from the log's first changeset.
  pub fn new() -> Replaying {
    Replaying::default()
  }

  /// One unit: the next changeset begun, or its next unit taken, or it finished and put in the index's place. Refused,
  /// the changeset under way is given back, the index as it was before it.
  pub fn step<L: ChangesetLog>(
    &mut self,
    log: &L,
    index: &mut HgIndex,
    store: &mut ContentStore,
  ) -> Result<Stepped, Refused> {
    let Some((sequence, mut replay)) = self.under_way.take() else {
      let Some(sequence) = log.next(self.done) else {
        return Ok(Stepped::Done);
      };
      let record = section(log, sequence, Section::Record)?;
      let replay = Replay::begin_sectioned(&record)
        .map_err(|refused| Refused::Record { sequence, refused })?;
      self.under_way = Some((sequence, replay));
      return Ok(Stepped::More);
    };
    let payload = match section(log, sequence, Section::Payload) {
      Ok(payload) => payload,
      Err(refused) => return Err(given_back(sequence, replay, store, refused)),
    };
    match replay.step_sectioned(payload.as_ref(), index, store) {
      Ok(Progress::More) => {
        self.under_way = Some((sequence, replay));
        Ok(Stepped::More)
      }
      // The log lends a section whole: a want names bytes past its end.
      Ok(Progress::Wanted(_)) => Err(given_back(
        sequence,
        replay,
        store,
        Refused::Record {
          sequence,
          refused: RecordRefused::Truncated,
        },
      )),
      Ok(Progress::Done) => {
        let replayed = replay
          .finish(index, store)
          .map_err(|refused| Refused::Record { sequence, refused })?;
        self.replayed.push(replayed);
        self.done = Some(sequence);
        Ok(Stepped::More)
      }
      Err(refused) => Err(given_back(
        sequence,
        replay,
        store,
        Refused::Record { sequence, refused },
      )),
    }
  }

  /// What each finished changeset brought, in the log's order.
  pub fn replayed(&self) -> &[Replayed] {
    &self.replayed
  }
}

/// `section` of the changeset at `sequence`, refused by kind where the log has none.
fn section<L: ChangesetLog>(
  log: &L,
  sequence: u64,
  section: Section,
) -> Result<Cow<'_, [u8]>, Refused> {
  log
    .section(sequence, section)
    .ok_or(Refused::Missing { sequence, section })
}

#[cfg(test)]
#[path = "landed_tests.rs"]
mod tests;
