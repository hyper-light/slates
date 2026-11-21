//! An import's revisions advanced in RAM and committed as one entry (C2 stage C, the green's side; A-814; main's order
//! of 2026-11-21 04:11 and 06:11; the split proposed in inbox/w-svn-from-w-rewrite-ops-stage-c-split.md).
//!
//! **Why.** A Subversion import landed each revision as its own changeset on the node's store, each one register
//! commit, so git-2000's 2,001 revisions took 2,001 landings: 24.5 s against 8.2 s before the store, about 8 ms a
//! revision, one `F_FULLFSYNC` of the store's owner per landing (inbox/main-from-w-svn-stage-c-analysis.md). One landing
//! an import needs the green, and the records (w-svn's side), advanced through the import's revisions in RAM without
//! their chain entries, then committed in one transaction, a refusal giving both back.
//!
//! **What.** An [`ImportBatch`] holds a scratch member of the green's lineage made at its head
//! (`Greens::preview_at_head`, O(1)):
//! - Each revision's work, built against the scratch member ([`ImportBatch::with_engine`]), is judged and committed
//!   into it ([`ImportBatch::stage`]). The green itself shows nothing of the import.
//! - Each work is numbered as the green's: based at the green's head and each revision after it. The scratch member,
//!   whose own versions count from 0, judges it with its base read in its own numbering, so each chain entry is the
//!   real one a holder recomputes (§4.16 "Apply on holders").
//! - [`ImportBatch::commit`] appends every version's chain entry and the register entry (the import's one changeset's
//!   reference, w-svn's) as one log record (`repository::push::append_versions`). It then applies each version to the
//!   green as its entry records it (`Green::apply_recorded`, what a recovery replays) and retires the scratch member.
//! - A refusal before the record ([`ImportBatch::abandon`], or the commit refused) leaves the green and its chain as
//!   they were: nothing to roll back in the engine.
//!
//! **A crash.** Before the record, nothing is durable and the green is as it was. After it, the record holds every
//! version, and a replay applies them as it applies any chain.
//!
//! **Bounds.** A batch holds its staged works until it commits, so its caller bounds it: a batch of the revisions one
//! changeset holds, within the store's changeset bound (w-svn's side).

use slates_db::catalog::{Principal, RepositoryId, VolumeId as DbVolumeId};
use slates_ipc::protocol::Refusal;
use slates_merge::chain_entry::Applied;
use slates_merge::engine::{Green, Increment, Outcome};

use crate::greens::Preview;
use crate::repository::push::{AppendRefused, EntryParts};
use crate::state::ShardState;

/// Counter: a revision staged into an import's batch. Format: a counter name in the daemon's status report.
pub(crate) const STAGED: &str = "repository.import_batch.staged";
/// Counter: an import's batch committed as one entry. Format: a counter name.
pub(crate) const COMMITTED: &str = "repository.import_batch.committed";

/// One revision's work staged in a batch: the work as the green numbers it, what its verdict resolved it to, the head
/// identity at its version where its record goes to holders, and the retention its verdict reserved.
pub(crate) struct Staged {
  /// The work, based at the green's version before it.
  pub(crate) increment: Increment,
  /// What its verdict resolved it to.
  pub(crate) applied: Vec<Applied>,
  /// The head identity at its version, where the record goes to holders.
  pub(crate) identity: Option<[u8; 32]>,
  /// The history and change index its verdict reserved.
  pub(crate) reserved: usize,
}

/// An import's revisions advanced in RAM (module docs).
pub(crate) struct ImportBatch {
  /// The import's green.
  green: DbVolumeId,
  /// The green's head when the batch began: the first work's base.
  base: u64,
  /// The scratch member the works are judged and committed into.
  preview: Option<Preview>,
  /// The works staged, in order.
  staged: Vec<Staged>,
}

/// Why a revision's work could not be staged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StageRefused {
  /// The work is not based at the batch's head.
  NotAtHead {
    /// The batch's head.
    head: u64,
  },
  /// The verdict did not accept it: the outcome.
  NotAccepted(Outcome),
  /// The scratch member is gone or busy.
  Unavailable,
}

impl ImportBatch {
  /// A batch over `green`, a member of its repository's lineage, at its head; refused `ContentUnavailable` where no
  /// scratch member can be made (a green standing alone, its lineage busy).
  pub(crate) fn begin(state: &mut ShardState, green: DbVolumeId) -> Result<ImportBatch, Refusal> {
    let base = state
      .greens
      .get(&green)
      .map(|engine| engine.head())
      .ok_or(Refusal::NotFound)?;
    let preview = state
      .greens
      .preview_at_head(&green)
      .map_err(|_| Refusal::ContentUnavailable)?;
    Ok(ImportBatch {
      green,
      base,
      preview: Some(preview),
      staged: Vec::new(),
    })
  }

  /// The version the next work is based at, in the green's numbering: its head and every revision staged.
  pub(crate) fn head(&self) -> u64 {
    self
      .base
      .saturating_add(u64::try_from(self.staged.len()).unwrap_or(u64::MAX))
  }

  /// The revisions staged.
  pub(crate) fn staged(&self) -> usize {
    self.staged.len()
  }

  /// `read` over the green as the staged revisions leave it (the scratch member): what the next revision's work is built
  /// against. None where the scratch member is gone or busy.
  pub(crate) fn with_engine<R>(
    &self,
    state: &mut ShardState,
    read: impl FnOnce(&Green) -> R,
  ) -> Option<R> {
    let preview = self.preview.as_ref()?;
    state
      .greens
      .with_preview(preview, |engine| read(engine))
      .ok()
  }

  /// [`ImportBatch::with_engine`] over `greens` alone, so the caller reads the rest of the shard's state beside it (a
  /// Subversion revision's increment is built from its records and the scratch member together, C2 stage C).
  pub(crate) fn with_engine_in<R>(
    &self,
    greens: &mut crate::greens::Greens,
    read: impl FnOnce(&Green) -> R,
  ) -> Option<R> {
    let preview = self.preview.as_ref()?;
    greens.with_preview(preview, |engine| read(engine)).ok()
  }

  /// Judges `work`, based at the batch's head in the green's numbering, into the scratch member, and keeps it: the
  /// version it is in the green's numbering. Refused, nothing kept, where it is not based at the head or not accepted.
  pub(crate) fn stage(
    &mut self,
    state: &mut ShardState,
    mut work: Increment,
  ) -> Result<u64, StageRefused> {
    let head = self.head();
    if work.base != head {
      return Err(StageRefused::NotAtHead { head });
    }
    let preview = self.preview.as_ref().ok_or(StageRefused::Unavailable)?;
    let holders = crate::merge_service::record_goes_to_holders(state, self.green);
    let judged = state.greens.with_preview_mut(preview, |engine| {
      // The scratch member counts its versions from 0: the work's base, read there, and each copy's source version
      // (a Subversion copy names the head it copies from, w-svn's C2 stage C) moved into its numbering for the verdict.
      let offset = head.saturating_sub(engine.head());
      work.base = engine.head();
      in_numbering(&mut work, |version| version.checked_sub(offset));
      let reserved = engine
        .history_reservation(&work)
        .saturating_add(engine.changes_reservation(&work));
      let (outcome, mut applied) = engine.submit_resolved(&work);
      let identity = holders.then(|| engine.head_identity());
      // Back into the green's numbering: the work's copies, and those its verdict records.
      in_numbering(&mut work, |version| version.checked_add(offset));
      if let Some(applied) = applied.as_mut() {
        for step in applied.iter_mut() {
          if let Applied::CopyTree { version, .. } = step {
            *version = version.saturating_add(offset);
          }
        }
      }
      (outcome, applied, identity, reserved)
    });
    work.base = head;
    let (outcome, applied, identity, reserved) = judged.map_err(|_| StageRefused::Unavailable)?;
    let (Outcome::Accepted { .. } | Outcome::Identical { .. }, Some(applied)) = (&outcome, applied)
    else {
      return Err(StageRefused::NotAccepted(outcome));
    };
    self.staged.push(Staged {
      increment: work,
      applied,
      identity,
      reserved,
    });
    state.count(STAGED, 1);
    Ok(head.saturating_add(1))
  }

  /// Commits the staged revisions with `parts` (the import's changeset reference, w-svn's) as one entry of
  /// `repository`'s register by `principal`, its owner (module docs): the versions it made, in order. The scratch member
  /// is retired either way; refused, the green and its chain are as they were.
  pub(crate) fn commit(
    mut self,
    state: &mut ShardState,
    (principal, repository): (&Principal, RepositoryId),
    parts: EntryParts,
  ) -> Result<Vec<u64>, AppendRefused> {
    let staged = std::mem::take(&mut self.staged);
    let green = self.green;
    self.retire(state);
    let made = crate::repository::push::append_versions(
      state,
      (principal, repository),
      green,
      staged,
      parts,
    )?;
    state.count(COMMITTED, 1);
    Ok(made)
  }

  /// Gives the batch up: nothing of it applied or recorded, the scratch member retired.
  pub(crate) fn abandon(mut self, state: &mut ShardState) {
    self.retire(state);
  }

  /// Retires the scratch member, once.
  fn retire(&mut self, state: &mut ShardState) {
    if let Some(preview) = self.preview.take() {
      state.greens.retire_preview(preview);
    }
  }
}

/// Moves each `Copy` operation of `work` to the version `moved` answers for its own: from the green's numbering into a
/// scratch member's and back ([`ImportBatch::stage`]). A copy `moved` answers none for (a version before the scratch
/// member's first) is left as it is, and its verdict refuses it as a copy past what the member holds.
fn in_numbering(work: &mut Increment, moved: impl Fn(u64) -> Option<u64>) {
  for op in work
    .doc
    .ops
    .iter_mut()
    .filter(|op| op.kind == slates_merge::ops_doc::OpKind::Copy)
  {
    if let Some(version) = moved(op.at) {
      op.at = version;
    }
  }
}

impl Drop for ImportBatch {
  /// A batch dropped without its commit or abandon (a task ended early) gives its scratch member back where the shard's
  /// state is not borrowed then; counted otherwise.
  fn drop(&mut self) {
    if let Some(preview) = self.preview.take() {
      let _ = crate::state::with_state_counted(|state| state.greens.retire_preview(preview));
    }
  }
}
