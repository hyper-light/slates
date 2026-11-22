//! An import's revisions staged in RAM and landed as one changeset, committed as one register entry (C2 stage C, the
//! records' side; main's rulings of 2026-11-21 01:02 and 08:17; the green's side is A-814's `crate::import_batch`).
//!
//! **Why.** An import landed each revision as its own changeset, one store flush and one register commit each:
//! git-2000's 2,001 revisions took 24.5 s against 8.2 s before the store (GAPS, "C2 stage C waits").
//!
//! **What.** A [`Staging`] holds the repository's landing turn for its life, so no other landing moves the records it
//! advances. Each revision ([`Staging::stage`]) arrives with its texts in the content store already, put by the import's
//! sink as the dump was read and named stored (`crate::vcs_store::svn_import::texts`; goal 93, P7 finding (2)), their
//! holds handed to the batch (`svn_commit::stage_texts`); its copies from older revisions planned (`super::restore`); it is checked and applied to
//! the records in RAM, and its increment, built against the batch's scratch member, staged there (`ImportBatch::stage`).
//! [`Staging::land`] lands one changeset: every text the batch put as its `Payload` with `Texts` rows, then one `Record`
//! section a revision in order. Then it commits every version and the changeset's reference as one register entry
//! (`ImportBatch::commit`), and gives the content store's holds back.
//!
//! **A refusal** before the entry ([`Staging::abandon`]) gives the batch's scratch member and the content store's holds
//! back and makes the records again from the changesets the register names (`crate::vcs_store::rebuild`). The batch's
//! revisions were never committed, never answered, and never visible: a Subversion repository serves no reads while it
//! imports. A crash before the entry leaves a changeset no entry names, which the next landing replaces at its sequence.
//!
//! **Bounded by the import's admission** (main's ruling of 08:17). The batch's increments' post-state bytes are charged
//! to the shard as the import's holding (`vcs_store::svn_import`), beside the texts the content store charges as it
//! takes them. A revision that the shard could not admit flushes the batch first. So under concurrency the batch shrinks
//! by itself, and a flush's cost amortizes over what memory allows. A changeset holds at most [`MAX_RECORDS`] records.

use slates_db::catalog::{Principal, RepositoryId, VolumeId as DbVolumeId};
use slates_svn::commit::revision::Revision;
use slates_svn::index::ContentIdentity;

use super::{Building, Uncommitted, brought, checked, green_of, grown, identity, landing, restore};
use crate::import_batch::ImportBatch;
use crate::repository::push::EntryParts;
use crate::state::ShardState;

/// Format: the most records one changeset holds: the store's sections are counted in a `u16` (`slates_store::log`),
/// and a changeset that brings texts spends two on its `Payload` and `Texts`.
pub(crate) const MAX_RECORDS: usize = (u16::MAX as usize) - 2;

/// Counter: a batch landed because the next revision's charge would not be admitted. Format: a counter name.
pub(crate) const FLUSHED_ADMISSION: &str = "repository.svn_import.flushed_admission";
/// Counter: a batch landed at [`MAX_RECORDS`]. Format: a counter name.
pub(crate) const FLUSHED_SECTIONS: &str = "repository.svn_import.flushed_sections";
/// Counter: a batch landed at the import's end, or before a record that moves no version. Format: a counter name.
pub(crate) const FLUSHED_END: &str = "repository.svn_import.flushed_end";

#[cfg(test)]
thread_local! {
  /// A test's import admission (main's oracle of 2026-11-21 08:17): what the shard would admit the import, less what its
  /// batch holds (its texts and its increments' post-state), where it is smaller than the shard's own. The shard's room
  /// in a test process is shared by every test's daemon, so a tight room made by withholding capacity moved under the
  /// import; this one does not. [`Staging::due_before`] reads it.
  pub(crate) static ADMISSION_CAP: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
thread_local! {
  /// A test's kill of a batch's landing after this many slices of its recorded apply (the sliced commit's crash test):
  /// nothing after it runs, as after a `kill -9`; it clears itself as it fires.
  pub(crate) static KILL_AT_COMMIT_SLICE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// Why a batch lands now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Flush {
  /// The next revision's charge would not be admitted.
  Admission,
  /// The changeset holds [`MAX_RECORDS`].
  Sections,
  /// The import's end, or a record that moves no version next.
  End,
}

impl Flush {
  /// The counter this cause is counted under.
  fn counter(self) -> &'static str {
    match self {
      Flush::Admission => FLUSHED_ADMISSION,
      Flush::Sections => FLUSHED_SECTIONS,
      Flush::End => FLUSHED_END,
    }
  }
}

/// An import's revisions staged since its last landing (module docs).
pub(crate) struct Staging {
  /// The repository's landing turn, held for the batch's life.
  turn: Option<landing::Turn>,
  /// The green's side: the revisions' versions, staged in a scratch member.
  batch: Option<ImportBatch>,
  /// The repository.
  repository: RepositoryId,
  /// The green.
  green: DbVolumeId,
  /// Each revision's record, in order.
  records: Vec<Vec<u8>>,
  /// The texts the batch put into the content store, one hold each, given back once it lands or is abandoned.
  put: Vec<ContentIdentity>,
  /// What the changeset carries of them: a path and a length each.
  carried: Vec<landing::LandedText>,
  /// The staged increments' post-state bytes: the batch's holding beside the content store's own charge.
  held: u64,
  /// The revisions staged since the batch began (what an abandon makes the records again for).
  staged: usize,
  /// The texts the green's held table holds at the paths the batch changes, as it held them before the batch: let go
  /// once the batch commits (S4 c1, `vcs_store::svn_texts`).
  released: Vec<(ContentIdentity, String)>,
  /// The text each path the batch changed holds after it, by path: held once the batch commits.
  holding: std::collections::BTreeMap<String, ContentIdentity>,
}

impl Staging {
  /// A batch of repository `repository`'s import: its landing turn taken and the green's scratch member made. None
  /// where the shard's state is gone; refused `NotHeld` where the green cannot be staged in.
  pub(crate) async fn begin(repository: RepositoryId) -> Option<Result<Staging, Uncommitted>> {
    let turn = landing::turn(repository).await?;
    crate::state::with_state(|state| {
      let begun = green_of(state, repository).and_then(|green| {
        ImportBatch::begin(state, green)
          .map(|batch| (green, batch))
          .map_err(|_| Uncommitted::NotHeld)
      });
      match begun {
        Ok((green, batch)) => Ok(Staging {
          turn: Some(turn),
          batch: Some(batch),
          repository,
          green,
          records: Vec::new(),
          put: Vec::new(),
          carried: Vec::new(),
          held: 0,
          staged: 0,
          released: Vec::new(),
          holding: std::collections::BTreeMap::new(),
        }),
        Err(refused) => {
          turn.give_back(state);
          Err(refused)
        }
      }
    })
  }

  /// The batch's holding beyond what the content store charges: its increments' post-state bytes.
  pub(crate) fn held(&self) -> u64 {
    self.held
  }

  /// Why the batch must land before `revision` is staged, if it must: the shard could not admit what the revision
  /// brings twice (its texts in the content store, and again in its increment's post-state), or the changeset is full.
  /// An empty batch never flushes.
  pub(crate) fn due_before(&self, state: &ShardState, revision: &Revision) -> Option<Flush> {
    if self.records.is_empty() {
      return None;
    }
    if self.records.len() >= MAX_RECORDS {
      return Some(Flush::Sections);
    }
    // Its texts arrive named stored, put by the import's sink as the dump was read, and count twice as contents do:
    // in the content store, and again in its increment's post-state.
    let texts = revision
      .stored
      .values()
      .fold(0u64, |total, len| total.saturating_add(*len));
    let charge = brought(revision).saturating_add(texts).saturating_mul(2);
    let admittable = state.store.admittable();
    #[cfg(test)]
    let admittable = ADMISSION_CAP
      .with(std::cell::Cell::get)
      .map_or(admittable, |cap| {
        let carried = self
          .carried
          .iter()
          .fold(0u64, |total, text| total.saturating_add(text.len));
        admittable.min(cap.saturating_sub(carried.saturating_add(self.held)))
      });
    (charge > admittable).then_some(Flush::Admission)
  }

  /// Stages `revision` (module docs): the texts the import put for it, `imported`, taken with their holds, any of its
  /// contents put into the content store and named stored, its copies planned, then checked, applied to the records in
  /// RAM and its version staged in the batch. None where the shard's state is gone; refused as `land_revision` refuses,
  /// the batch then to be abandoned.
  pub(crate) async fn stage(
    &mut self,
    revision: Revision,
    imported: Vec<ContentIdentity>,
  ) -> Option<Result<(), Uncommitted>> {
    let repository = self.repository;
    let mut revision = revision;
    let (storing, carried) = match super::stage_texts(repository, &mut revision, imported).await? {
      Ok(staged) => staged,
      Err(refused) => return Some(Err(refused)),
    };
    self.put.extend(storing.put.iter().copied());
    self.carried.extend(carried);
    // A stored text a row names that the content store let go, and the texts its copies set back, are brought back
    // whole for the increment, and given back once it is built.
    let brought_texts = match super::bring_stored(repository, &revision.record.changes).await? {
      Ok(brought) => brought,
      Err(refused) => return Some(Err(refused)),
    };
    let plans = match restore::plan(repository, &revision.record.changes).await? {
      Ok(plans) => plans,
      Err(refused) => {
        let _ = crate::state::with_state(|state| super::release_texts(state, None, &brought_texts));
        return Some(Err(refused));
      }
    };
    let restoring = match super::bring_texts(repository, restore::texts_of(&plans).into_iter())
      .await?
    {
      Ok(restoring) => restoring,
      Err(refused) => {
        let _ = crate::state::with_state(|state| super::release_texts(state, None, &brought_texts));
        return Some(Err(refused));
      }
    };
    crate::state::with_state(|state| {
      let staged = self.stage_in(state, repository, (revision, plans));
      super::release_texts(state, None, &brought_texts);
      super::release_texts(state, None, &restoring);
      staged
    })
  }

  /// [`Staging::stage`]'s step: the check, the increment built against the scratch member and staged, the records
  /// applied, the record kept.
  fn stage_in(
    &mut self,
    state: &mut ShardState,
    repository: RepositoryId,
    (revision, plans): (Revision, restore::CopyPlans),
  ) -> Result<(), Uncommitted> {
    // The records' room lent for the whole step, check to apply, as a landing's commit lends it.
    let lent = brought(&revision);
    if !crate::vcs_store::lend_svn(state, repository, lent) {
      return Err(Uncommitted::NotHeld);
    }
    let staged = self.staged_in(state, repository, (revision, plans));
    crate::vcs_store::settle_svn(state, repository);
    staged
  }

  /// [`Staging::stage_in`] within the records' lent room.
  fn staged_in(
    &mut self,
    state: &mut ShardState,
    repository: RepositoryId,
    (revision, plans): (Revision, restore::CopyPlans),
  ) -> Result<(), Uncommitted> {
    let (checked, record) = checked(state, repository, revision, None)?;
    let batch = self.batch.as_mut().ok_or(Uncommitted::NotHeld)?;
    let head = batch.head();
    let work_identity = identity(&record);
    let built = crate::vcs_store::lent_state(state, |state| {
      let ShardState {
        greens, vcs_stores, ..
      } = state;
      let records = vcs_stores
        .subversion_records(repository)
        .ok_or(Uncommitted::NotHeld)?;
      if records.youngest() != head {
        return Err(Uncommitted::Diverged);
      }
      let store = vcs_stores.content();
      batch
        .with_engine_in(greens, |engine| {
          Building::new((records, store), engine, (&checked, plans)).build(head, work_identity)
        })
        .ok_or(Uncommitted::NotHeld)?
    })?;
    let post = u64::try_from(built.post_state.len()).unwrap_or(u64::MAX);
    let growth = grown(&checked);
    crate::vcs_store::reserve_svn(state, repository, growth)
      .map_err(|available| Uncommitted::Budget { available })?;
    if let Err(refused) = batch.stage(state, built) {
      slates_machine::log_line!(
        "slates-server: partition {}: an imported revision's version was not staged: {refused:?}",
        state.partition
      );
      crate::vcs_store::unreserve_svn(state, repository, growth);
      state.count(super::DIVERGED, 1);
      return Err(Uncommitted::Diverged);
    }
    let changes = checked.changes().to_vec();
    let before = state
      .vcs_stores
      .subversion_records(repository)
      .map(|records| {
        crate::vcs_store::svn_texts::released_before(records, &changes, records.youngest())
      })
      .unwrap_or_default();
    let applied = state
      .vcs_stores
      .subversion(repository)
      .and_then(|svn| svn.records.as_mut())
      .map(|records| records.apply(checked))
      .ok_or(Uncommitted::NotHeld)?;
    let revision = applied.map_err(Uncommitted::Step)?;
    self.staged = self.staged.saturating_add(1);
    let after = state
      .vcs_stores
      .subversion_records(repository)
      .map(|records| crate::vcs_store::svn_texts::held_after(records, &changes, revision))
      .unwrap_or_default();
    // The held table follows the batch's net effect: a path's text from before the batch let go the first time the
    // batch reaches it; each path's last text held.
    for (identity, path) in before {
      if self.holding.remove(&path).is_none() && !self.released.iter().any(|(_, at)| *at == path) {
        self.released.push((identity, path));
      }
    }
    for (identity, path) in after {
      self.holding.insert(path, identity);
    }
    self.records.push(record);
    self.held = self.held.saturating_add(post);
    Ok(())
  }

  /// Lands the batch (module docs) for `cause`, counted: one changeset, then one register entry by `principal`, its
  /// owner. The revisions it committed; refused, the batch abandoned ([`Staging::abandon`]). None where the shard's
  /// state is gone.
  pub(crate) async fn land(
    mut self,
    principal: &Principal,
    cause: Flush,
  ) -> Option<Result<usize, Uncommitted>> {
    let repository = self.repository;
    let place = crate::state::with_state(|state| landing::landing_place(state, repository))?;
    let (disk, sequence) = match place {
      Ok(place) => place,
      Err(refused) => {
        let _ = crate::state::with_state(|state| self.abandon(state));
        return Some(Err(refused));
      }
    };
    let records = std::mem::take(&mut self.records);
    let revisions = records.len();
    let landed =
      landing::land_changeset(&disk, (repository, sequence), records, &self.carried).await;
    let recorded = crate::state::with_state(|state| {
      landed.and_then(|()| {
        if landing::next_sequence(state, repository) != sequence {
          state.count(super::DIVERGED, 1);
          return Err(Uncommitted::Diverged);
        }
        let reference = landing::reference(sequence);
        crate::vcs_store::admit_objects(state, reference.len())
          .map_err(|available| Uncommitted::Budget { available })?;
        let batch = self.batch.take().ok_or(Uncommitted::NotHeld)?;
        let parts = EntryParts {
          objects: reference,
          ..EntryParts::default()
        };
        batch
          .commit(state, (principal, repository), parts)
          .map_err(Uncommitted::Append)
      })
    })?;
    let mut recorded = match recorded {
      Ok(recorded) => recorded,
      Err(refused) => {
        let _ = crate::state::with_state(|state| self.abandon(state));
        return Some(Err(refused));
      }
    };
    // The batch is the register's now: its versions applied to the green a slice a turn (`RecordedBatch`), the
    // landing turn and the import's read gate held until the last; a kill between slices recovers from the record.
    let mut slices = 0usize;
    loop {
      #[cfg(test)]
      if KILL_AT_COMMIT_SLICE.with(|kill| kill.get() == Some(slices)) {
        KILL_AT_COMMIT_SLICE.with(|kill| kill.set(None));
        // A kill: nothing after it runs, as after a `kill -9` between two slices.
        std::future::pending::<()>().await;
      }
      if crate::state::with_state(|state| recorded.apply_slice(state))?.is_some() {
        break;
      }
      slices = slices.saturating_add(1);
      slates_rt::futures::yield_now().await;
    }
    crate::state::with_state(|state| {
      state.count(cause.counter(), 1);
      let green = self.green;
      crate::merge_service::checkpoint_green(state, green);
      super::fold_to_readers(state, green);
      let holding: Vec<(ContentIdentity, String)> = std::mem::take(&mut self.holding)
        .into_iter()
        .map(|(path, identity)| (identity, path))
        .collect();
      crate::vcs_store::svn_texts::follow(state, green, &self.released, &holding);
      super::release_texts(state, None, &self.put);
      self.put.clear();
      if let Some(turn) = self.turn.take() {
        turn.give_back(state);
      }
      Ok(revisions)
    })
  }

  /// Gives the batch up (module docs): its scratch member and the content store's holds given back, the records made
  /// again from what the register names, the landing turn given back.
  pub(crate) fn abandon(mut self, state: &mut ShardState) {
    let repository = self.repository;
    if let Some(batch) = self.batch.take() {
      batch.abandon(state);
    }
    super::release_texts(state, None, &self.put);
    self.put.clear();
    if self.staged > 0 {
      crate::vcs_store::rebuild(state, repository);
    }
    if let Some(turn) = self.turn.take() {
      turn.give_back(state);
    }
  }
}
