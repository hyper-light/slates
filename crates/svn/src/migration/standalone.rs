//! Records standing alone, their texts held apart (the KeptText move, main's 10:35 item 3 of 2026-11-21): what a test,
//! a fixture, an example or a tool keeps where a daemon keeps records and the node's store.
//!
//! **Why.** A daemon's records name every text stored (C2 stage B part 2 (b)): its bytes are its changeset's on the
//! node's store, read through a lent [`TextSource`]. Records standing alone kept their texts themselves, as deltas
//! (`KeptText`), a second text path the daemon no longer takes. [`Standalone`] takes them the daemon's way instead:
//! each revision's texts the records do not hold are taken out before its step ([`Repository::take_texts`]) and held
//! here by identity, the records naming each stored by its length; reads, the drives and the export read them through
//! this holder, the import's delta bases are supplied from it.
//!
//! **What.** It is the records ([`core::ops::Deref`]) with a text source beside them: a read source, a green and a
//! text source over both, as [`WithTexts`] is over a daemon's.

use std::borrow::Cow;
use std::collections::BTreeMap;

use super::records::{TextReading, TextSource, from_offset};
use super::{Importer, Importing, MigrationRefusal, Repository, WithTexts};
use crate::commit::drive::Green;
use crate::commit::record::Record;
use crate::commit::revision::{Revision as RevisionToCommit, StepRefusal};
use crate::index::{ContentIdentity, Revision};
use crate::lock::{AprTime, CommitLocks, LockEntry};
use crate::mergeinfo::Contents;
use crate::reads::{Repository as ReadSource, WindowCache};

/// Records standing alone with their texts held apart (module docs).
#[derive(Clone, Debug)]
pub struct Standalone {
  /// The records.
  records: Repository,
  /// Every text the records name stored, by identity, whole.
  texts: BTreeMap<ContentIdentity, Vec<u8>>,
}

impl Standalone {
  /// `records` standing alone, holding no text apart yet.
  pub fn new(records: Repository) -> Self {
    Self {
      records,
      texts: BTreeMap::new(),
    }
  }

  /// The records.
  pub fn records(&self) -> &Repository {
    &self.records
  }

  /// The records with their texts read from here: what a lender hands a read, a drive or an export.
  pub fn with_texts(&self) -> WithTexts<'_, Self> {
    WithTexts {
      records: &self.records,
      texts: self,
    }
  }

  /// The texts held apart, each by identity.
  pub fn texts_apart(&self) -> impl Iterator<Item = (&ContentIdentity, &[u8])> + '_ {
    self
      .texts
      .iter()
      .map(|(identity, bytes)| (identity, bytes.as_slice()))
  }

  /// Commits `revision` through the records' step ([`Repository::commit`]), its texts the records do not hold taken
  /// out first and held here ([`Repository::take_texts`]). Refused, nothing changes here or in the records.
  ///
  /// # Errors
  /// The records' step's refusal.
  pub fn commit(&mut self, mut revision: RevisionToCommit) -> Result<Revision, StepRefusal> {
    let taken = self.records.take_texts(&mut revision);
    let committed = self.records.commit(revision)?;
    for (identity, bytes) in taken {
      self.texts.entry(identity).or_insert(bytes);
    }
    Ok(committed)
  }

  /// Checks `revision` as [`Repository::check`] does, its texts the records do not hold taken out first, held by the
  /// answer until [`Standalone::apply`] keeps them here.
  ///
  /// # Errors
  /// As [`Repository::check`].
  pub fn check(&self, mut revision: RevisionToCommit) -> Result<Checked, StepRefusal> {
    let texts = self.records.take_texts(&mut revision);
    let checked = self.records.check(revision)?;
    Ok(Checked { checked, texts })
  }

  /// Checks a client's work as [`Repository::check_work`] does, its texts taken out first ([`Standalone::check`]).
  ///
  /// # Errors
  /// As [`Repository::check_work`].
  pub fn check_work(
    &self,
    mut revision: RevisionToCommit,
    locks: &CommitLocks,
    now: AprTime,
  ) -> Result<Checked, StepRefusal> {
    let texts = self.records.take_texts(&mut revision);
    let checked = self.records.check_work(revision, locks, now)?;
    Ok(Checked { checked, texts })
  }

  /// Applies `checked` as [`Repository::apply`] does, keeping its texts here. Refused, nothing changes.
  ///
  /// # Errors
  /// As [`Repository::apply`].
  pub fn apply(&mut self, checked: Checked) -> Result<Revision, StepRefusal> {
    let Checked { checked, texts } = checked;
    let applied = self.records.apply(checked)?;
    for (identity, bytes) in texts {
      self.texts.entry(identity).or_insert(bytes);
    }
    Ok(applied)
  }

  /// Applies lock entries as [`Repository::apply_lock_entries`] does.
  ///
  /// # Errors
  /// As [`Repository::apply_lock_entries`].
  pub fn apply_lock_entries(&mut self, entries: &[LockEntry]) -> Result<(), StepRefusal> {
    self.records.apply_lock_entries(entries)
  }

  /// Commits `record` as [`Repository::commit_record`] does, a revision's texts held here ([`Standalone::commit`]).
  ///
  /// # Errors
  /// As [`Repository::commit_record`].
  pub fn commit_record(&mut self, record: Record) -> Result<(), StepRefusal> {
    match record {
      Record::Revision(revision) => self.commit(revision).map(drop),
      other => self.records.commit_record(other),
    }
  }

  /// Hands `importer` the base it asked for ([`Importing::Base`]) from the texts held here.
  ///
  /// # Errors
  /// [`MigrationRefusal::Corrupt`] for a text not held here; [`Importer::supply_base`]'s refusal.
  pub fn supply_base(
    &self,
    importer: &mut Importer,
    text: &ContentIdentity,
  ) -> Result<(), MigrationRefusal> {
    let bytes = TextSource::text(self, text).ok_or(MigrationRefusal::Corrupt)?;
    importer.supply_base(&self.records, text, bytes.into_owned())
  }

  /// Runs `importer` over `buffer` as [`Importer::import`] does, answering its asks for a base from here; a record it
  /// hands over is the caller's to commit ([`Standalone::commit_record`]).
  ///
  /// # Errors
  /// As [`Importer::import`] and [`Standalone::supply_base`].
  pub fn import(
    &mut self,
    importer: &mut Importer,
    buffer: &[u8],
    ended: bool,
  ) -> Result<Importing, MigrationRefusal> {
    let mut offset = 0usize;
    loop {
      let rest = buffer.get(offset..).unwrap_or_default();
      match importer.import(&mut self.records, rest, ended)? {
        Importing::Base { consumed, text } => {
          self.supply_base(importer, &text)?;
          offset = offset
            .checked_add(consumed)
            .ok_or(MigrationRefusal::Corrupt)?;
        }
        Importing::NeedMore { consumed, needed } => {
          return Ok(Importing::NeedMore {
            consumed: offset
              .checked_add(consumed)
              .ok_or(MigrationRefusal::Corrupt)?,
            needed,
          });
        }
        Importing::Commit { consumed, record } => {
          return Ok(Importing::Commit {
            consumed: offset
              .checked_add(consumed)
              .ok_or(MigrationRefusal::Corrupt)?,
            record,
          });
        }
        Importing::Done => return Ok(Importing::Done),
      }
    }
  }

  /// The text held under `identity`, whole: held apart, or a property list's bytes the records hold.
  pub fn text(&self, identity: &ContentIdentity) -> Option<Cow<'_, [u8]>> {
    TextSource::text(self, identity)
  }

  /// The records' mutable side, for a test that changes them under the texts.
  pub fn records_mut(&mut self) -> &mut Repository {
    &mut self.records
  }
}

/// A revision checked against records standing alone ([`Standalone::check`]): the records' check, and the texts taken
/// out of it, kept once it applies.
#[derive(Debug)]
pub struct Checked {
  /// The records' check.
  checked: super::Checked,
  /// The texts it brings that the records do not hold, by identity.
  texts: BTreeMap<ContentIdentity, Vec<u8>>,
}

impl core::ops::Deref for Checked {
  type Target = super::Checked;

  fn deref(&self) -> &super::Checked {
    &self.checked
  }
}

impl core::ops::Deref for Standalone {
  type Target = Repository;

  fn deref(&self) -> &Repository {
    &self.records
  }
}

impl TextSource for Standalone {
  fn text(&self, identity: &ContentIdentity) -> Option<Cow<'_, [u8]>> {
    if let Some(bytes) = self.texts.get(identity) {
      return Some(Cow::Borrowed(bytes.as_slice()));
    }
    // A text the records keep themselves (`KeptText`, a revision replayed or imported into them directly), or a
    // property list's bytes: until the records keep no text (the move's third piece).
    TextSource::text(&self.records, identity)
  }

  fn text_at(&self, identity: &ContentIdentity, offset: u64) -> Option<Cow<'_, [u8]>> {
    from_offset(TextSource::text(self, identity)?, offset)
  }

  fn read_step(&self, identity: &ContentIdentity, reading: &mut TextReading) -> Option<bool> {
    let text = TextSource::text(self, identity)?;
    Some(reading.copy_step(&text))
  }
}

impl Contents for Standalone {
  fn entries(&self, directory: &str, revision: Revision) -> Option<Vec<String>> {
    Contents::entries(&self.records, directory, revision)
  }

  fn mergeinfo_value(&self, properties: &ContentIdentity) -> Option<&[u8]> {
    Contents::mergeinfo_value(&self.records, properties)
  }
}

impl ReadSource for Standalone {
  fn revision_properties(&self, revision: Revision) -> Option<Vec<(&[u8], &[u8])>> {
    ReadSource::revision_properties(&self.records, revision)
  }

  fn properties(&self, identity: &ContentIdentity) -> Option<Vec<(&[u8], &[u8])>> {
    ReadSource::properties(&self.records, identity)
  }

  fn text(&self, identity: &ContentIdentity, offset: u64) -> Option<Cow<'_, [u8]>> {
    TextSource::text_at(self, identity, offset)
  }

  fn text_in<'a>(
    &'a self,
    identity: &ContentIdentity,
    offset: u64,
    _cache: &'a mut WindowCache,
  ) -> Option<Cow<'a, [u8]>> {
    TextSource::text_at(self, identity, offset)
  }
}

impl Green for Standalone {
  fn text(&self, identity: &ContentIdentity) -> Option<Cow<'_, [u8]>> {
    TextSource::text(self, identity)
  }

  fn properties(&self, identity: &ContentIdentity) -> Option<Vec<(&[u8], &[u8])>> {
    Green::properties(&self.records, identity)
  }

  fn name_text(&self, text: &[u8]) -> ContentIdentity {
    Green::name_text(&self.records, text)
  }

  fn name_properties(&self, properties: &[(Vec<u8>, Vec<u8>)]) -> ContentIdentity {
    Green::name_properties(&self.records, properties)
  }
}
