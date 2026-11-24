//! A push of several works at once (§4.17.1 "A push is a work" and "The repository and its greens" item 2; §4.17.2 "A
//! push" steps 6 and 7; §4.17.3 "Push" step 6; A-635): the verdict over every branch a push changes, all of them or
//! none, recorded as one entry of the repository's logs (A-603, rebuilt by A-642): one append on each green it moves,
//! each only while its green is at the position the push read, and one atomic record when it moves two logs or more.
//!
//! **One interface**, [`RepositoryLogs::append`], for git's receive-pack, Mercurial's unbundle and the LFS locks, which
//! the shard's state implements ([`push`]) and their tests may fake. Each branch the push changes is a
//! [`BranchChange`]: a work A-618's `slates_merge::push::build` made against a branch's green; a new branch made at a
//! version of another without a walk and moved by such a work; or a branch retired, its green kept. Beside them,
//! [`EntryParts`] carries what else the entry holds: checks of logs' positions (Mercurial's `check:heads`), names, and
//! the repository's own state (phase roots, obsolescence markers).
//!
//! **All or none.** Nothing is applied until everything has passed, in this order:
//! 1. the rules of each change: a member of the repository, a free or bound name, a version a branch can be made at, a
//!    green changed once, and each green's merge gates (its write policy, A-285; its evidence and signature
//!    requirements, §4.16 and A-321);
//! 2. each new branch made in memory (`Lineage::branch`, nothing recorded), and each green's retention secured;
//! 3. each work's verdict, held (`Green::judge`);
//! 4. the whole entry, by the register's guard, the new greens counted held (`Partition::check_creating`).
//!
//! Only then does a git push's store commit its objects (`crate::vcs_store::commit` through [`push_sealed`], A-662: the
//! entry holds their record, which the guard charged), then the verdicts are committed (`Green::commit_judged`), and the
//! new greens' records and the entry recorded as one log record. A store that committed for an entry then not recorded
//! is made again from the log (`crate::vcs_store::rebuild`); a push not committed is left whole with its caller, for its
//! next ref or to give back. A refusal names every change refused and why ([`ChangeRefused`]), or the entry's refusal, and leaves
//! the greens, the namespace and the register as they were: a branch made in memory is retired, and every retention
//! secured is given back.
//!
//! **A work sent again** (a client that heard nothing after its push committed) is answered the version it made, and
//! records nothing again.
//!
//! **Owed:** the repository policy's check (A-622's next part) before the verdict; in a fleet, the answer waits for every
//! version's placement, and the entry becomes one record of the register (A-636).

use std::collections::BTreeSet;

use slates_db::catalog::{
  GreenVersion, MemberOrigin, MemberRecord, NameChange, Principal, RepositoryCheck,
  RepositoryEntry, RepositoryId, RepositoryRecord, Role, StateChange, VolumeId as DbVolumeId,
  VolumeRecord,
};
use slates_db::op::Op;
use slates_ipc::protocol::Refusal;
use slates_merge::engine::{ConflictWindow, Increment, Judged, Judgment};
use slates_vfs::clock::Clock;

use crate::error::refusal_of_db;
use crate::state::ShardState;
use crate::verbs::to_wire_volume;

#[cfg(test)]
mod tests;

/// One branch a push changes (§4.17.2 "A push" step 6).
#[derive(Clone, Debug)]
pub(crate) enum BranchChange {
  /// A branch moves: `green`, a member of the repository, by the work A-618's `push::build` made against it.
  Update {
    /// The branch's green.
    green: DbVolumeId,
    /// The work.
    work: Increment,
  },
  /// A new branch `name`, made at `version` of the member `from` without a walk, and moved by `work` when the pushed head
  /// is not that version's. The work is made against `from` at `version` (`push::build` with that base) and applies to
  /// the new green at its version 0, which holds what `from` held at `version`.
  Create {
    /// The new branch's name.
    name: String,
    /// The member it is made from.
    from: DbVolumeId,
    /// The version of `from` it is made at.
    version: u64,
    /// The work moving it, if any.
    work: Option<Increment>,
  },
  /// A new branch `name`, made at `version` of the member `from` as [`BranchChange::Create`] makes one, whose green owes
  /// `push` as its version 1 instead of holding its built work (P2; `slates_merge::push::DeferredPush`): the push's tree
  /// is built a unit a turn when the green is first reached (`crate::green_ready`), and the entry records the push, not
  /// the work. A tool whose create would compose a whole tree (git's tags) makes this one.
  CreateDeferred {
    /// The new branch's name.
    name: String,
    /// The member it is made from.
    from: DbVolumeId,
    /// The version of `from` it is made at.
    version: u64,
    /// The push it owes, based at the new green's version 0.
    push: slates_merge::push::DeferredPush,
  },
  /// Green `green`, a member, owes `push` as its next version instead of moving by a built work (P2): the update a
  /// [`BranchChange::CreateDeferred`] is for a green already named, recorded and replayed as its version the same way.
  /// git's unborn default branch takes it (its first version over the empty green it is named by at version 0): git/git's
  /// master, 4,876 files, was built whole in one step.
  UpdateDeferred {
    /// The green.
    green: DbVolumeId,
    /// The push it owes, based at its head.
    push: slates_merge::push::DeferredPush,
  },
  /// Branch `name` leaves the namespace and its green is kept (§4.17.1 item 5), unless the green moved past `version`,
  /// the version whose head the client said it saw.
  Delete {
    /// The branch's name.
    name: String,
    /// The version the client saw.
    version: u64,
  },
}

/// What a push's register entry carries beside its branches' own changes (A-603).
#[derive(Clone, Debug, Default)]
pub(crate) struct EntryParts {
  /// Checks of logs' positions read before its changes (Mercurial's `check:heads`, §4.17.3 decision 6; A-642).
  pub(crate) checks: Vec<RepositoryCheck>,
  /// Names bound or retired after the branches' own (Mercurial's bookmarks).
  pub(crate) names: Vec<NameChange>,
  /// The repository's own state moved (Mercurial's phase roots and markers).
  pub(crate) state: Vec<StateChange>,
  /// The tool's record the entry holds (A-662's `RepositoryEntry::objects`): a Subversion revision's (A-655 part 2b,
  /// `slates_svn::commit::record`), or a Subversion repository's UUID or revision properties, which an entry may hold
  /// alone (A-652 part 3); a git push's is its sealed push's own ([`push_sealed`]).
  pub(crate) objects: Vec<u8>,
}

/// What a push did to one branch, in the order of its changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BranchAppended {
  /// `green` moved to `version`; `before` when an earlier send of the same work made that version.
  Moved {
    /// The branch's green.
    green: DbVolumeId,
    /// The version it is at.
    version: u64,
    /// Whether an earlier send made it.
    before: bool,
  },
  /// A new green was made for the branch, at `version` of its own: 1 when a work moved it, else 0.
  Created {
    /// The new green.
    green: DbVolumeId,
    /// Its version.
    version: u64,
  },
  /// The branch left the namespace; its green is kept.
  Deleted {
    /// The green it bound.
    green: DbVolumeId,
  },
}

/// What a push did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Appended {
  /// Each change's result, in the order given.
  pub(crate) changes: Vec<BranchAppended>,
  /// Whether it recorded anything: false when every work was sent before and nothing else changed.
  pub(crate) recorded: bool,
}

/// A repository's logs, as a version-control server appends a push to them (§4.17.1 shared piece 2; A-635, A-642): one
/// record, all of it or none. The owner shard's state implements it ([`push`]); a tool's push code (git's receive-pack,
/// Mercurial's unbundle, the LFS locks) is written against it, and its tests may implement it.
pub(crate) trait RepositoryLogs {
  /// Appends `changes` with `parts` to `repository`'s logs as one record, by `principal`, its owner (module docs).
  fn append(
    &mut self,
    principal: &Principal,
    repository: RepositoryId,
    changes: Vec<BranchChange>,
    parts: EntryParts,
  ) -> Result<Appended, AppendRefused>;
}

impl RepositoryLogs for ShardState {
  fn append(
    &mut self,
    principal: &Principal,
    repository: RepositoryId,
    changes: Vec<BranchChange>,
    parts: EntryParts,
  ) -> Result<Appended, AppendRefused> {
    push(self, principal, repository, changes, parts)
  }
}

/// Why one change was refused.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ChangeRefusal {
  /// The verdict's windows (§4.16): the head another push moved first (`MetaMeta` on `slates.vcs.head`), or paths.
  Conflict {
    /// The windows.
    windows: Vec<ConflictWindow>,
  },
  /// The branch moved past the version the client saw (a delete's old value): its green is at `head`.
  Moved {
    /// The green's version now.
    head: u64,
  },
  /// The push changes one branch or one green twice.
  Twice,
  /// The green's write policy asks for an approval of each merge (A-285), which a push does not carry: `identity` is
  /// the work an approver would approve.
  ApprovalRequired {
    /// The work's identity.
    identity: [u8; 32],
  },
  /// Refused by a rule, typed as the wire types it: `NotFound` (no such member or branch), `AlreadyExists` (a name
  /// taken), `InvalidName`, `UnknownBase` (a base or a version past what the green still holds), `PolicyForbids`,
  /// `EvidenceRequired`, `SignatureRequired`, `BudgetExceeded`, `ContentUnavailable` (a fenced or busy green).
  Refused(Refusal),
}

/// One change refused: its index among the push's changes, and why.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ChangeRefused {
  /// The change's index.
  pub(crate) index: usize,
  /// Why.
  pub(crate) why: ChangeRefusal,
}

/// A push refused, typed; nothing of it applied.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum AppendRefused {
  /// The repository: none here (`NotFound`), or another principal's (`Forbidden`, until the policy of piece 7).
  Repository(Refusal),
  /// The changes refused, each named by its index.
  Changes(Vec<ChangeRefused>),
  /// The entry the push would record, refused: a green moved since it was read (`Database { Moved }`, naming it), a
  /// check (`Database { CheckFailed }`), a name of `EntryParts`, the state (`Database { StateChanged }`), the budget
  /// (`TableAtCapacity`), or its record not made durable.
  Entry(Refusal),
}

/// Counter: a held verdict whose green moved before its commit, or a commit refused after the entry was judged: zero in
/// every correct use, since a push runs in one step of the owner shard. Format: a counter name in the status report.
pub(crate) const PUSH_COMMIT_REFUSED: &str = "repository.push.commit_refused";

/// Counter: an update a push deferred, its green owing the push rather than moved by a built work (P2,
/// [`BranchChange::UpdateDeferred`]): git's unborn default branch's first version. Format: a counter name in the status
/// report.
pub(crate) const UPDATES_DEFERRED: &str = "repository.push.updates_deferred";

/// The green a step leaves owing a push, and the push: a deferred create's new green, or a deferred update's (P2).
fn owed(step: &Step) -> Option<(DbVolumeId, &slates_merge::push::DeferredPush)> {
  match step {
    Step::Create {
      record,
      deferred: Some(push),
      ..
    } => Some((record.id, push)),
    Step::Defer { green, push } => Some((*green, push)),
    _ => None,
  }
}

/// One change, planned: what it does to which green.
enum Step {
  /// `green` moves by `work`.
  Update { green: DbVolumeId, work: Increment },
  /// A new green, `record`, made at `version` of `from` and bound to `name`, moved by `work` (based at 0) if any. The
  /// record is boxed: it is far larger than the other variants.
  Create {
    name: String,
    from: DbVolumeId,
    version: u64,
    record: Box<VolumeRecord>,
    work: Option<Increment>,
    /// The push its green owes as version 1, where the create defers its work (P2).
    deferred: Option<slates_merge::push::DeferredPush>,
  },
  /// `name` retired from `green`, which must be at `version`.
  Delete {
    name: String,
    green: DbVolumeId,
    version: u64,
  },
  /// `green` owes `push` as its next version (P2, [`BranchChange::UpdateDeferred`]): deferred once the entry is judged,
  /// never before, so a refused entry leaves the green as it was.
  Defer {
    green: DbVolumeId,
    push: slates_merge::push::DeferredPush,
  },
}

impl Step {
  /// The green the step's work moves, and the work.
  fn work(&self) -> Option<(DbVolumeId, &Increment)> {
    match self {
      Step::Update { green, work } => Some((*green, work)),
      Step::Create { record, work, .. } => work.as_ref().map(|work| (record.id, work)),
      Step::Delete { .. } | Step::Defer { .. } => None,
    }
  }

  /// [`Step::work`], the work lent mutably: its verdict takes its named runs' buffers (A-760, `Green::judge_named`).
  fn work_mut(&mut self) -> Option<(DbVolumeId, &mut Increment)> {
    match self {
      Step::Update { green, work } => Some((*green, work)),
      Step::Create { record, work, .. } => work.as_mut().map(|work| (record.id, work)),
      Step::Delete { .. } | Step::Defer { .. } => None,
    }
  }
}

/// What a push holds before it commits, given back if it is refused: the branches made in memory, and the greens whose
/// retention it secured.
#[derive(Default)]
struct Held {
  /// New members made in memory and not recorded.
  made: Vec<DbVolumeId>,
  /// Greens whose retention was secured, and the work a conflict left in the rejected cache, if any.
  secured: Vec<(DbVolumeId, Option<[u8; 32]>)>,
}

impl Held {
  /// Gives back what the push held: each branch made in memory retired, each retention trued up to what was retained.
  fn give_back(self, state: &mut ShardState) {
    for green in self.made {
      crate::merge_service::release_green_retention(state, green);
      super::retire_member(state, green);
    }
    for (green, conflicted) in self.secured {
      settle(state, green, conflicted);
    }
  }
}

/// Trues `green`'s retention up to what it retains; a conflict's rejected result, when even that cannot be held, is
/// dropped from the cache and the settle made again (as a submit's conflict is settled).
fn settle(state: &mut ShardState, green: DbVolumeId, conflicted: Option<[u8; 32]>) {
  if crate::merge_service::settle_green_retention(state, green).is_ok() {
    return;
  }
  if let (Some(increment), Some(mut engine)) = (conflicted, state.greens.get_mut(&green)) {
    engine.forget_rejected(&increment);
  }
  let _ = crate::merge_service::settle_green_retention(state, green);
}

/// The push (module docs): every change in `changes` judged, all or none, with `parts`, as one entry of `repository`'s
/// register, by `principal`, its owner.
pub(crate) fn push(
  state: &mut ShardState,
  principal: &Principal,
  repository: RepositoryId,
  changes: Vec<BranchChange>,
  parts: EntryParts,
) -> Result<Appended, AppendRefused> {
  push_holding(state, (principal, repository), (changes, &[]), parts, None)
}

/// Counter: a version of an import's batch whose recorded apply did not reach its version, the green made again from
/// its chain: zero in every correct use, as the scratch member judged it at that head. Format: a counter name.
pub(crate) const BATCH_APPLY_REFUSED: &str = "repository.import_batch.apply_refused";

/// Appends `staged`, a batch of `green`'s versions in order (C2 stage C, `crate::import_batch`), with `parts` as one entry
/// of `repository`'s register by `principal`, its owner: every version's chain entry and the register's own parts in one
/// log record. The batch's retention is secured first and the entry judged by the register's guard; refused, nothing is
/// applied or recorded. What it answers applies the versions to the green a slice at a time ([`RecordedBatch`]): the
/// apply is work over a batch's versions, user-scaled, so it runs a slice a turn under the shard's quantum (CLAUDE.md
/// §3; w-svn for main's ruling of 2026-11-21 12:43: in one step, git-2000's 2,000 versions held the shard past its
/// liveness window and its watchdog killed the daemon).
pub(crate) fn append_versions(
  state: &mut ShardState,
  (principal, repository): (&Principal, RepositoryId),
  green: DbVolumeId,
  staged: Vec<crate::import_batch::Staged>,
  parts: EntryParts,
) -> Result<RecordedBatch, AppendRefused> {
  let record = state
    .db
    .partition()
    .repositories()
    .repository(repository)
    .cloned()
    .ok_or(AppendRefused::Repository(Refusal::NotFound))?;
  if &record.owner != principal {
    return Err(AppendRefused::Repository(Refusal::Forbidden {
      verb: "push".to_owned(),
    }));
  }
  if !is_member(state, repository, green) {
    return Err(AppendRefused::Repository(Refusal::NotFound));
  }
  let head = state
    .greens
    .get(&green)
    .map(|engine| engine.head())
    .ok_or(AppendRefused::Repository(Refusal::NotFound))?;
  let at = |index: usize| {
    head
      .saturating_add(u64::try_from(index).unwrap_or(u64::MAX))
      .saturating_add(1)
  };
  if staged
    .iter()
    .enumerate()
    .any(|(index, step)| step.increment.base.saturating_add(1) != at(index))
  {
    return Err(AppendRefused::Entry(Refusal::ContentUnavailable));
  }
  let reserved: usize = staged
    .iter()
    .map(|step| step.reserved)
    .fold(0, usize::saturating_add);
  let secured = u64::try_from(reserved).unwrap_or(u64::MAX);
  crate::verbs::secure_retention(state, green, secured)
    .map_err(|available| AppendRefused::Entry(Refusal::BudgetExceeded { available }))?;
  let mut entry = RepositoryEntry {
    repository,
    ..RepositoryEntry::default()
  };
  // Each revision's increment held in the content store, where its chain entry names it (chain entry format 6), before
  // anything is recorded; what was held is given back if the entry is refused.
  let mut increments = Vec::with_capacity(staged.len());
  for (index, step) in staged.iter().enumerate() {
    let version = at(index);
    let held =
      match crate::merge_service::hold_increment(state, green, version, &step.increment.encode()) {
        Ok(held) => held,
        Err(refusal) => {
          for held in increments {
            crate::merge_service::release_refused_increment(state, green, held);
          }
          let _ = crate::merge_service::settle_green_retention(state, green);
          return Err(AppendRefused::Entry(refusal));
        }
      };
    increments.push(held);
    let bytes = slates_merge::chain_entry::encode(
      held.inputs,
      &step.increment,
      &step.applied,
      None,
      step.identity,
      None,
    );
    entry.versions.push(GreenVersion {
      green,
      version,
      entry: bytes,
    });
  }
  entry.checks.extend(parts.checks);
  entry.names.extend(parts.names);
  entry.state = parts.state;
  entry.objects = parts.objects;
  let admitted = admit(state, &[], &entry, &[])
    .and_then(|()| crate::verbs::publish_held_increments(state).map_err(AppendRefused::Entry))
    .and_then(|()| record_all(state, vec![Op::RepositoryAdvanced { entry }]));
  if let Err(refused) = admitted {
    for held in increments {
      crate::merge_service::release_refused_increment(state, green, held);
    }
    let _ = crate::merge_service::settle_green_retention(state, green);
    return Err(refused);
  }
  Ok(RecordedBatch {
    green,
    first: at(0),
    staged: staged.into(),
    made: Vec::new(),
  })
}

/// Counter: a slice of a recorded batch's apply ([`RecordedBatch::apply_slice`]): more than one a batch shows its apply
/// was sliced. Format: a counter name in the daemon's status report.
pub(crate) const BATCH_APPLY_SLICES: &str = "repository.import_batch.apply_slices";

#[cfg(test)]
thread_local! {
  /// A test's most versions one slice of a recorded batch's apply takes, so its slices are known (the sliced commit's
  /// crash test kills between them). None takes as many as the shard's quantum allows.
  pub(crate) static SLICE_VERSIONS: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// A batch of a green's versions recorded in the register and applied to the green a slice at a time
/// ([`append_versions`]). Its record holds every version, so a kill between slices recovers to the batch's last version,
/// as any chain replays. Nothing may read the green until the last slice: a Subversion repository serves no session
/// while it imports.
pub(crate) struct RecordedBatch {
  /// The green.
  green: DbVolumeId,
  /// The version of the next staged revision.
  first: u64,
  /// The versions still to apply, in order.
  staged: std::collections::VecDeque<crate::import_batch::Staged>,
  /// The versions applied.
  made: Vec<u64>,
}

impl RecordedBatch {
  /// Applies the next versions until the shard's step budget is spent from now, at least one, each as its entry records
  /// it (`Green::apply_recorded`, what a recovery replays) with its merge record enqueued; counted. The versions made
  /// once every one is applied (the green's retention settled and its chain checkpointed then), none while more remain.
  pub(crate) fn apply_slice(&mut self, state: &mut ShardState) -> Option<Vec<u64>> {
    let budget = slates_rt::futures::step_budget_ns().unwrap_or(0);
    let deadline = slates_rt::futures::now_ns().saturating_add(budget);
    #[cfg(test)]
    let most = SLICE_VERSIONS
      .with(std::cell::Cell::get)
      .unwrap_or(usize::MAX);
    #[cfg(not(test))]
    let most = usize::MAX;
    state.count(BATCH_APPLY_SLICES, 1);
    let mut taken = 0usize;
    while let Some(mut step) = self.staged.pop_front() {
      let version = self.first;
      self.first = version.saturating_add(1);
      // A version naming its texts (A-760) had its runs' buffers taken by the scratch member's verdict: they are read
      // again from the content store, which holds them until the batch's landing ends (`svn_commit::staging`). One read
      // in place of the copy the post-state was; adopting the scratch member's files is w-svn's piece (c).
      let read = crate::repository::svn_commit::read_named(state, &mut step.increment).is_ok();
      // The named texts its apply reads that the green holds by name alone lent it first (A-760 (b) 4d).
      let applied = if read {
        crate::vcs_store::name_holds::apply_recorded_lent(
          state,
          self.green,
          (&mut step.increment, &step.applied),
        )
      } else {
        None
      };
      if applied != Some(version) {
        // Recorded, and not applied as judged: the green is made again from its chain, which holds every version.
        state.count(BATCH_APPLY_REFUSED, 1);
        crate::verbs::reconcile_unpublished_effects(state);
      }
      crate::merge_service::enqueue_record(
        state,
        self.green,
        version,
        step.increment.id,
        step.increment.base,
        step.increment.evidence,
        (None, None),
      );
      self.made.push(version);
      taken = taken.saturating_add(1);
      if taken >= most || slates_rt::futures::now_ns() >= deadline {
        break;
      }
    }
    if !self.staged.is_empty() {
      return None;
    }
    let _ = crate::merge_service::settle_green_retention(state, self.green);
    crate::merge_service::checkpoint_green(state, self.green);
    Some(std::mem::take(&mut self.made))
  }
}

/// One move of a git push's store (A-662): the green it names, a new branch's by its name, and its new head, none to
/// delete it.
pub(crate) type StoreMove = (
  crate::vcs_store::Target,
  Option<slates_git::object_name::ObjectId>,
);

/// [`push`] of a git push whose objects `sealed` holds (A-662): the entry holds their record, and the repository's store
/// commits them with `moves` between the register's guard and the entry's apply, each move resolved once the new
/// branches are made: its green's key, and the version the entry leaves the green at. Appended, the store holds the
/// objects; refused, `sealed` is as it was, for the push's next ref or to be given back.
#[cfg_attr(not(test), allow(dead_code))] // Its caller outside tests is git's push path (A-654).
pub(crate) fn push_sealed(
  state: &mut ShardState,
  (principal, repository): (&Principal, RepositoryId),
  changes: Vec<BranchChange>,
  parts: EntryParts,
  (sealed, moves): (&mut crate::vcs_store::SealedPush, &[StoreMove]),
) -> Result<Appended, AppendRefused> {
  if sealed.repository() != repository {
    return Err(AppendRefused::Repository(Refusal::NotFound));
  }
  push_holding(
    state,
    (principal, repository),
    (changes, &[]),
    parts,
    Some(Stored::Git((sealed, moves))),
  )
}

/// [`push`] of a Mercurial import (A-687): the entry holds the import's record (`slates_hg::import::record`), and the
/// repository's store takes the imported index between the register's guard and the entry's apply
/// (`crate::vcs_store::hg_import::place_imported`), as a git push's store commits its objects there (A-662). The entry is
/// recorded even when it moves no green, since its record is the repository's history. Appended, the store holds the
/// index and `index` is empty; refused, `index` is as it was, for its caller to give back.
pub(crate) fn push_imported(
  state: &mut ShardState,
  (principal, repository): (&Principal, RepositoryId),
  changes: Vec<BranchChange>,
  parts: EntryParts,
  (index, record): (&mut Option<slates_hg::index::HgIndex>, Vec<u8>),
) -> Result<Appended, AppendRefused> {
  push_holding(
    state,
    (principal, repository),
    (changes, &[]),
    parts,
    Some(Stored::Imported((index, record))),
  )
}

/// [`push`] of a Mercurial push (A-680 part 2c): the entry holds the push's record (`slates_hg::import::record`'s
/// `Source::Push`, the revisions it brought), which a recovery replays into the index after the import's; the landing
/// puts its fork in the index's place once the entry is appended (`repository::hg_push::land`).
pub(crate) fn push_hg(
  state: &mut ShardState,
  (principal, repository): (&Principal, RepositoryId),
  (changes, ahead): (Vec<BranchChange>, &[crate::held_ahead::HeldAhead]),
  parts: EntryParts,
  record: Vec<u8>,
) -> Result<Appended, AppendRefused> {
  push_holding(
    state,
    (principal, repository),
    (changes, ahead),
    parts,
    Some(Stored::Pushed(record)),
  )
}

/// A Git LFS object an upload brings (A-629 part 3): its oid and bytes, and why the repository's LFS index refused it,
/// once it has.
pub(crate) struct LfsUpload<'u> {
  /// The object's oid, as the client named it.
  pub(crate) oid: slates_git::lfs::Oid,
  /// Its bytes.
  pub(crate) bytes: &'u [u8],
  /// Why the LFS index kept nothing, when the append was refused there: told apart so an upload whose bytes are not
  /// its oid's is answered in LFS's words.
  pub(crate) refused: Option<crate::vcs_store::LfsAdmitRefused>,
}

/// [`push`] of a Git LFS object's upload (A-629 part 3): the entry holds the object's record
/// (`crate::vcs_store::lfs_record`), and the repository's LFS index keeps the object between the register's guard and
/// the entry's apply (`crate::vcs_store::admit_lfs`, its bytes checked against its oid there), as a git push's store
/// commits its objects there (A-662). The entry is recorded though it moves no green, since its record is the object.
/// Appended, the index holds the object; refused by the index, `upload.refused` says why and nothing is kept.
pub(crate) fn push_lfs(
  state: &mut ShardState,
  (principal, repository): (&Principal, RepositoryId),
  upload: &mut LfsUpload<'_>,
) -> Result<Appended, AppendRefused> {
  push_holding(
    state,
    (principal, repository),
    (Vec::new(), &[]),
    EntryParts::default(),
    Some(Stored::Lfs(upload)),
  )
}

/// A sealed push and the moves its append commits.
type Sealed<'p> = (&'p mut crate::vcs_store::SealedPush, &'p [StoreMove]);

/// What a push's entry holds as its objects, and how the repository's store takes them between the register's guard
/// and the entry's apply.
enum Stored<'p, 'u> {
  /// A git push's sealed objects and the moves its append commits (A-662).
  Git(Sealed<'p>),
  /// A Mercurial import's index and its record (A-687).
  Imported((&'p mut Option<slates_hg::index::HgIndex>, Vec<u8>)),
  /// A Mercurial push's record (A-680 part 2c).
  Pushed(Vec<u8>),
  /// A Git LFS object's upload (A-629 part 3).
  Lfs(&'p mut LfsUpload<'u>),
}

/// [`push`], [`push_sealed`], [`push_imported`], [`push_hg`] and [`push_lfs`]'s one path, with what the entry's objects are, if any.
fn push_holding(
  state: &mut ShardState,
  (principal, repository): (&Principal, RepositoryId),
  (changes, ahead): (Vec<BranchChange>, &[crate::held_ahead::HeldAhead]),
  parts: EntryParts,
  stored: Option<Stored<'_, '_>>,
) -> Result<Appended, AppendRefused> {
  let record = state
    .db
    .partition()
    .repositories()
    .repository(repository)
    .cloned()
    .ok_or(AppendRefused::Repository(Refusal::NotFound))?;
  if &record.owner != principal {
    return Err(AppendRefused::Repository(Refusal::Forbidden {
      verb: "push".to_owned(),
    }));
  }
  // A push that moves the repository's own state logs (a branch made or deleted, names, phases, markers, locks) is
  // admitted only while the parts this shard owes their holders are within their bound, before anything is judged; one
  // that moves greens alone waits on none of them (A-636 part 2b).
  let moves_state = !parts.names.is_empty()
    || !parts.state.is_empty()
    || changes.iter().any(|change| {
      !matches!(
        change,
        BranchChange::Update { .. } | BranchChange::UpdateDeferred { .. }
      )
    });
  if moves_state {
    crate::state_log::admit(state).map_err(AppendRefused::Repository)?;
  }
  let steps = plan(state, &record, principal, changes).map_err(AppendRefused::Changes)?;
  let mut held = Held::default();
  let pushed = push_planned(state, &record, (steps, parts), (&mut held, ahead), stored);
  if pushed.is_err() {
    held.give_back(state);
  } else {
    // The entry's objects are the repository's content, charged to this shard's memory budget as retention, never the
    // partition's merge budget (A-662; `vcs_store::settle_objects`). A push was admitted what its shard could hold by
    // its own admission (A-704, A-711), so its charge never refuses it; a Subversion revision asks first
    // (`svn_commit`'s land, `vcs_store::admit_objects`).
    crate::vcs_store::settle_objects(state, repository);
  }
  pushed
}

/// Judges, without recording anything, the entry of no change that `parts` alone would append to `repository`'s
/// register (a made commit's: a Subversion revision's reference to its changeset), as [`push`] judges it before it
/// records (`Partition::check_creating`): the guard a made commit's step runs before anything of its accept applies
/// (main's ruling of 2026-11-23 20:51, guard-then-apply), so the append after it is not refused.
pub(crate) fn check_entry(
  state: &ShardState,
  repository: RepositoryId,
  parts: &EntryParts,
) -> Result<(), AppendRefused> {
  let entry = entry(repository, (&[], &[]), parts.clone());
  state
    .db
    .partition()
    .check_creating(&[], &entry)
    .map_err(|error| AppendRefused::Entry(refusal_of_db(&error)))
}

/// Plans every change by its rules (module docs, item 1), or names every change refused.
fn plan(
  state: &mut ShardState,
  record: &RepositoryRecord,
  principal: &Principal,
  changes: Vec<BranchChange>,
) -> Result<Vec<Step>, Vec<ChangeRefused>> {
  let mut greens = BTreeSet::new();
  let mut names = BTreeSet::new();
  let mut steps = Vec::new();
  let mut refused = Vec::new();
  // Each new branch's id is minted at its own offset, since none is recorded before the entry (A-694).
  let mut minted = 0u64;
  for (index, change) in changes.into_iter().enumerate() {
    let planned = match change {
      BranchChange::Update { green, work } => {
        plan_update(state, record.id, (green, work), &mut greens)
      }
      BranchChange::Create {
        name,
        from,
        version,
        work,
      } => {
        let planned = plan_create(
          state,
          (record, principal),
          (name, from, version, work),
          (&mut names, minted),
        );
        if planned.is_ok() {
          minted = minted.saturating_add(1);
        }
        planned
      }
      BranchChange::CreateDeferred {
        name,
        from,
        version,
        push,
      } => {
        let planned = plan_create(
          state,
          (record, principal),
          (name, from, version, None),
          (&mut names, minted),
        )
        .map(|step| match step {
          Step::Create {
            name,
            from,
            version,
            record,
            work,
            ..
          } => Step::Create {
            name,
            from,
            version,
            record,
            work,
            deferred: Some(push),
          },
          other => other,
        });
        if planned.is_ok() {
          minted = minted.saturating_add(1);
        }
        planned
      }
      BranchChange::UpdateDeferred { green, push } => {
        plan_update_deferred(state, record.id, (green, push), &mut greens)
      }
      BranchChange::Delete { name, version } => {
        plan_delete(state, record.id, (name, version), (&mut names, &mut greens))
      }
    };
    match planned {
      Ok(step) => steps.push(step),
      Err(why) => refused.push(ChangeRefused { index, why }),
    }
  }
  if refused.is_empty() {
    Ok(steps)
  } else {
    Err(refused)
  }
}

/// Whether `green` is a member of `repository`'s lineage.
fn is_member(state: &ShardState, repository: RepositoryId, green: DbVolumeId) -> bool {
  state
    .db
    .partition()
    .repositories()
    .member(green)
    .is_some_and(|member| member.repository == repository)
}

/// An update's rules: a member, changed once, whose engine is here and not fenced, and whose gates let the work merge.
fn plan_update(
  state: &mut ShardState,
  repository: RepositoryId,
  (green, work): (DbVolumeId, Increment),
  greens: &mut BTreeSet<DbVolumeId>,
) -> Result<Step, ChangeRefusal> {
  if !is_member(state, repository, green) {
    return Err(ChangeRefusal::Refused(Refusal::NotFound));
  }
  if !greens.insert(green) {
    return Err(ChangeRefusal::Twice);
  }
  if state.fenced_greens.contains_key(&green) || !state.greens.contains_key(&green) {
    return Err(ChangeRefusal::Refused(Refusal::ContentUnavailable));
  }
  gates(state, green, &work)?;
  Ok(Step::Update { green, work })
}

/// A deferred update's rules (P2): an update's, a member changed once whose engine is here and not fenced, and a push
/// based at the green's head that it does not owe already. No gate is judged now: nothing merges until the push is built,
/// when its green is first reached (`crate::green_ready`).
fn plan_update_deferred(
  state: &mut ShardState,
  repository: RepositoryId,
  (green, push): (DbVolumeId, slates_merge::push::DeferredPush),
  greens: &mut BTreeSet<DbVolumeId>,
) -> Result<Step, ChangeRefusal> {
  if !is_member(state, repository, green) {
    return Err(ChangeRefusal::Refused(Refusal::NotFound));
  }
  if !greens.insert(green) {
    return Err(ChangeRefusal::Twice);
  }
  let based = state
    .greens
    .get(&green)
    .is_some_and(|engine| engine.head() == push.push().base_version);
  if state.fenced_greens.contains_key(&green) || !based {
    return Err(ChangeRefusal::Refused(Refusal::ContentUnavailable));
  }
  Ok(Step::Defer { green, push })
}

/// The gates every merge into `green` passes (§4.16; A-285, A-321): a write policy that lets merges happen without an
/// approval, evidence where the green requires it, and no requirement of a signature, which a push does not carry.
fn gates(state: &mut ShardState, green: DbVolumeId, work: &Increment) -> Result<(), ChangeRefusal> {
  match crate::verbs::merge_mode_of(state, green) {
    slates_db::catalog::WriteMode::Forbid => {
      state.count("policy_forbids.merge", 1);
      return Err(ChangeRefusal::Refused(Refusal::PolicyForbids {
        act: slates_ipc::protocol::WriteAct::Merge,
        by: slates_ipc::protocol::PolicySubject::Volume {
          volume: to_wire_volume(green),
        },
      }));
    }
    slates_db::catalog::WriteMode::Ask => {
      return Err(ChangeRefusal::ApprovalRequired { identity: work.id });
    }
    slates_db::catalog::WriteMode::Bypass => {}
  }
  if work.evidence.is_empty() && requires_evidence(state, green) {
    return Err(ChangeRefusal::Refused(Refusal::EvidenceRequired));
  }
  if crate::verbs::requires_signed_merges(state, green) {
    state.count("signature_required.merge", 1);
    return Err(ChangeRefusal::Refused(Refusal::SignatureRequired {
      by: slates_ipc::protocol::PolicySubject::Volume {
        volume: to_wire_volume(green),
      },
      verification: slates_ipc::protocol::Verification::Unsigned,
    }));
  }
  Ok(())
}

/// Whether `green` requires evidence on every submit (`Role::Green { require_evidence }`).
fn requires_evidence(state: &ShardState, green: DbVolumeId) -> bool {
  state.db.partition().volume(green).is_some_and(|volume| {
    matches!(
      volume.policy.role,
      Role::Green {
        require_evidence: true,
        ..
      }
    )
  })
}

/// A create's rules: a valid name the repository does not hold and the push names once; a member to make it from, at a
/// version it can be made at; a new id, minted at `minted`, the count of creates planned before it (A-694), and a member
/// name a listing holds; and evidence where the new green, which keeps its source's requirement, requires it.
fn plan_create(
  state: &mut ShardState,
  (record, principal): (&RepositoryRecord, &Principal),
  (name, from, version, work): (String, DbVolumeId, u64, Option<Increment>),
  (names, minted): (&mut BTreeSet<String>, u64),
) -> Result<Step, ChangeRefusal> {
  let refused = ChangeRefusal::Refused;
  super::free_name(state, record.id, &name).map_err(refused)?;
  if !names.insert(name.clone()) {
    return Err(ChangeRefusal::Twice);
  }
  if !is_member(state, record.id, from) {
    return Err(refused(Refusal::NotFound));
  }
  let version = super::branch_version(state, from, Some(version)).map_err(refused)?;
  // In the repository's family (A-636 part 2b-2), each create at its own offset (A-694).
  let id =
    crate::verbs::fresh_family_id_at(state, crate::verbs::family_of(record.id.bytes), minted)
      .ok_or_else(|| refused(crate::verbs::family_ids_exhausted()))?;
  let named = super::member_name(&record.name, &name, id).map_err(refused)?;
  let require_evidence = requires_evidence(state, from);
  if require_evidence && work.as_ref().is_some_and(|work| work.evidence.is_empty()) {
    return Err(refused(Refusal::EvidenceRequired));
  }
  let record = super::member_record(state, principal, (id, named), require_evidence);
  // The work was made against `from` at `version`, which the new green holds as its version 0.
  let work = work.map(|work| Increment { base: 0, ..work });
  Ok(Step::Create {
    name,
    from,
    version,
    record: Box::new(record),
    work,
    deferred: None,
  })
}

/// A delete's rules: a name the repository binds, named once in the push, its green not moved past the version the
/// client saw.
fn plan_delete(
  state: &ShardState,
  repository: RepositoryId,
  (name, version): (String, u64),
  (names, greens): (&mut BTreeSet<String>, &mut BTreeSet<DbVolumeId>),
) -> Result<Step, ChangeRefusal> {
  let green = state
    .db
    .partition()
    .repositories()
    .branch(repository, &name)
    .ok_or(ChangeRefusal::Refused(Refusal::NotFound))?;
  if !names.insert(name.clone()) || !greens.insert(green) {
    return Err(ChangeRefusal::Twice);
  }
  let head = state.db.partition().green_head(green);
  if head != version {
    return Err(ChangeRefusal::Moved { head });
  }
  Ok(Step::Delete {
    name,
    green,
    version,
  })
}

/// The push once planned (module docs, items 2 to 4, then the commit): what it held is in `held` until it is recorded.
fn push_planned(
  state: &mut ShardState,
  record: &RepositoryRecord,
  (mut steps, parts): (Vec<Step>, EntryParts),
  (held, ahead): (&mut Held, &[crate::held_ahead::HeldAhead]),
  mut stored: Option<Stored<'_, '_>>,
) -> Result<Appended, AppendRefused> {
  make(state, &steps, held).map_err(AppendRefused::Changes)?;
  secure(state, &steps, held).map_err(AppendRefused::Changes)?;
  let verdicts = judge(state, &mut steps, held).map_err(AppendRefused::Changes)?;
  let made: Vec<VolumeRecord> = steps
    .iter()
    .filter_map(|step| match step {
      Step::Create { record, .. } => Some(VolumeRecord::clone(record)),
      _ => None,
    })
    .collect();
  let mut entry = entry(record.id, (&steps, &verdicts), parts);
  // An entry of no change whose caller brings a record records that record alone: a Subversion repository's UUID or a
  // revision's properties (A-652 part 3), which move no green. A sealed push's objects, set below, and a revision's
  // record ride with the changes they came with.
  let brings_a_record = steps.is_empty() && !entry.objects.is_empty();
  let imports = matches!(stored, Some(Stored::Imported(_) | Stored::Pushed(_)));
  let uploads = matches!(stored, Some(Stored::Lfs(_)));
  match stored.as_mut() {
    Some(Stored::Git((sealed, moves))) => {
      entry.objects = sealed_record(state, (&steps, &entry), (&mut **sealed, *moves))?;
    }
    Some(Stored::Imported((_, bytes)) | Stored::Pushed(bytes)) => {
      entry.objects = core::mem::take(bytes);
    }
    Some(Stored::Lfs(upload)) => {
      let available = state.store.admittable();
      entry.objects = crate::vcs_store::lfs_record::encode(&upload.oid, upload.bytes)
        .map_err(|_| AppendRefused::Entry(Refusal::BudgetExceeded { available }))?;
    }
    None => {}
  }
  // An import's or a Mercurial push's entry is recorded whatever it moves: its record is the repository's history
  // (A-687, A-680 part 2c); an upload's
  // though it moves no green: its record is the object (A-629 part 3).
  let changes_anything = imports
    || brings_a_record
    || uploads
    || !(entry.joined.is_empty()
      && entry.versions.is_empty()
      && entry.names.is_empty()
      && entry.state.is_empty());
  admit(state, &made, &entry, &steps)?;
  if !changes_anything {
    // Every work was sent before and nothing else changes: nothing is recorded, and what was secured is given back.
    settle_secured(state, held);
    return Ok(answer(&steps, &verdicts));
  }
  // The store commits the push's objects, takes the import's index, or keeps the upload's LFS object, between the
  // guard and the apply (A-662, A-687, A-629 part 3), so a refusal here applies nothing.
  let store_committed = match stored {
    Some(Stored::Git((sealed, _))) => {
      if let Err(refused) = crate::vcs_store::commit(state, sealed) {
        return Err(AppendRefused::Entry(crate::vcs_store::refusal_of(
          state, &refused,
        )));
      }
      true
    }
    Some(Stored::Imported((index, _))) => {
      crate::vcs_store::hg_import::place_imported(state, record.id, index)
        .map_err(AppendRefused::Entry)?;
      true
    }
    // The landing puts its fork in the index's place once the entry is appended (A-680 part 2c).
    Some(Stored::Pushed(_)) | None => false,
    Some(Stored::Lfs(upload)) => {
      let kept = crate::vcs_store::admit_lfs(state, record.id, (&upload.oid, upload.bytes));
      if let Err(refused) = kept {
        let refusal = crate::vcs_store::lfs_refusal_of(state, &refused);
        upload.refused = Some(refused);
        return Err(AppendRefused::Entry(refusal));
      }
      true
    }
  };
  let moved: Vec<(DbVolumeId, u64)> = entry
    .versions
    .iter()
    .map(|version| (version.green, version.version))
    .collect();
  let applied = apply_planned(state, (&steps, verdicts), (entry, made), (held, ahead));
  if applied.is_err() && store_committed {
    // The store is ahead of a log that refused the entry: made again from the log (A-662).
    crate::vcs_store::rebuild(state, record.id);
  }
  if applied.is_ok() {
    // Each green the entry moved keeps its version's map of paths to git's files (A-664 part 3, `vcs_store::paths`).
    for (green, version) in moved {
      crate::vcs_store::paths::advance_after_push(state, green, version);
    }
  }
  applied
}

/// A git push's record for its entry (A-662): its moves resolved and admitted against the store as it is now, then the
/// record written; refused typed, the push keeping what it holds.
fn sealed_record(
  state: &mut ShardState,
  (steps, entry): (&[Step], &RepositoryEntry),
  (sealed, moves): (&mut crate::vcs_store::SealedPush, &[StoreMove]),
) -> Result<Vec<u8>, AppendRefused> {
  let resolved = resolve(state, (steps, entry), moves)?;
  if let Err(refused) = sealed.admit((&mut state.store, &mut state.vcs_stores), resolved) {
    return Err(AppendRefused::Entry(crate::vcs_store::refusal_of(
      state, &refused,
    )));
  }
  let recorded =
    sealed.record(crate::vcs_store::Lent::new(&mut state.store, &mut state.vcs_stores).stores);
  recorded.map_err(|refused| AppendRefused::Entry(crate::vcs_store::refusal_of(state, &refused)))
}

/// Each of a git push's store moves resolved (A-662): a green of the repository keyed by its id, a new branch's by the
/// green its step made, each with the version the entry leaves its green at (its entry's version, a new green's 0
/// when no work moves it, a deleted branch's head). A move naming no green of the push is refused `NotFound`.
fn resolve(
  state: &ShardState,
  (steps, entry): (&[Step], &RepositoryEntry),
  moves: &[StoreMove],
) -> Result<Vec<crate::vcs_store::record::RecordedMove>, AppendRefused> {
  let version_of = |green: DbVolumeId, otherwise: u64| {
    entry
      .versions
      .iter()
      .rev()
      .find(|version| version.green == green)
      .map_or(otherwise, |version| version.version)
  };
  moves
    .iter()
    .map(|(target, head)| {
      let (green, otherwise) = match target {
        crate::vcs_store::Target::Green(green) => (*green, state.db.partition().green_head(*green)),
        crate::vcs_store::Target::Created(name) => steps
          .iter()
          .find_map(|step| match step {
            Step::Create {
              name: made, record, ..
            } if made == name => Some((record.id, 0)),
            _ => None,
          })
          .ok_or(AppendRefused::Entry(Refusal::NotFound))?,
      };
      Ok((
        crate::vcs_store::green_key(green),
        version_of(green, otherwise),
        *head,
      ))
    })
    .collect()
}

/// The push once judged and its store committed: the verdicts committed, then the new greens' records and the entry
/// recorded as one log record, and each version's merge record enqueued.
fn apply_planned(
  state: &mut ShardState,
  (steps, verdicts): (&[Step], Vec<Option<Verdict>>),
  (entry, made): (RepositoryEntry, Vec<VolumeRecord>),
  (held, ahead): (&mut Held, &[crate::held_ahead::HeldAhead]),
) -> Result<Appended, AppendRefused> {
  let increments = hold_increments(state, (steps, &verdicts), ahead)?;
  let committed = match commit(state, steps, verdicts) {
    Ok(committed) => committed,
    Err(refused) => {
      release_increments(state, steps, increments);
      return Err(refused);
    }
  };
  let entry = recorded_entry(state, entry, steps, &committed, &increments);
  let mut ops: Vec<Op> = made
    .into_iter()
    .map(|record| Op::VolumeCreated { record })
    .collect();
  ops.push(Op::RepositoryAdvanced { entry });
  if let Err(refusal) = crate::verbs::publish_held_increments(state) {
    release_increments(state, steps, increments);
    return Err(AppendRefused::Entry(refusal));
  }
  record_all(state, ops)?;
  // Recorded: the new greens are the repository's now, and nothing is given back.
  held.made.clear();
  after(state, steps, &committed, held);
  Ok(answer_committed(steps, &committed, true))
}

/// Makes each new branch's member in memory (`Lineage::branch`), nothing recorded.
fn make(state: &mut ShardState, steps: &[Step], held: &mut Held) -> Result<(), Vec<ChangeRefused>> {
  let budget = crate::merge_service::rejected_cache_budget(state);
  for (index, step) in steps.iter().enumerate() {
    let Step::Create {
      from,
      version,
      record,
      ..
    } = step
    else {
      continue;
    };
    if state.greens.branch(from, *version, record.id).is_err() {
      return Err(vec![ChangeRefused {
        index,
        why: ChangeRefusal::Refused(Refusal::ContentUnavailable),
      }]);
    }
    held.made.push(record.id);
    if let Some(mut engine) = state.greens.get_mut(&record.id) {
      engine.set_rejected_budget(budget);
    }
    // A deferred create's green owes its push from here (P2): its content stays `from`'s at `version`.
    if let Step::Create {
      deferred: Some(push),
      ..
    } = step
      && state.greens.defer(&record.id, push.clone()).is_none()
    {
      return Err(vec![ChangeRefused {
        index,
        why: ChangeRefusal::Refused(Refusal::ContentUnavailable),
      }]);
    }
  }
  Ok(())
}

/// Secures each work's retention on its green before the verdict (A-16), folding first to leave it room.
fn secure(
  state: &mut ShardState,
  steps: &[Step],
  held: &mut Held,
) -> Result<(), Vec<ChangeRefused>> {
  let mut refused = Vec::new();
  for (index, step) in steps.iter().enumerate() {
    let Some((green, work)) = step.work() else {
      continue;
    };
    let reserved = state.greens.get(&green).map(|engine| {
      engine
        .history_reservation(work)
        .saturating_add(engine.changes_reservation(work))
    });
    let Some(reserved) = reserved else {
      refused.push(ChangeRefused {
        index,
        why: ChangeRefusal::Refused(Refusal::ContentUnavailable),
      });
      continue;
    };
    let secured = u64::try_from(reserved).unwrap_or(u64::MAX);
    let room = usize::try_from(secured).unwrap_or(usize::MAX);
    let charged = crate::merge_service::settle_green_retention_leaving(state, green, room)
      .and_then(|()| crate::merge_service::secure_green_retention(state, green, secured));
    match charged {
      Ok(()) => held.secured.push((green, None)),
      Err(available) => refused.push(ChangeRefused {
        index,
        why: ChangeRefusal::Refused(Refusal::BudgetExceeded { available }),
      }),
    }
  }
  if refused.is_empty() {
    Ok(())
  } else {
    Err(refused)
  }
}

/// A work's held verdict: accepted and held, or accepted before as a version.
enum Verdict {
  /// Accepted, held until the commit.
  Held(Box<Judged>),
  /// Accepted before as this version.
  Before(u64),
}

/// Counter: the bytes of named runs (A-760) a work still held once an accepted verdict was taken: a verdict that cloned
/// them rather than took them (`Green::judge` against `Green::judge_named`), a copy of every named text a push brings.
/// Format: a counter name in the status report.
pub(crate) const READ_CLONED_BYTES: &str = "repository.push.read_cloned_bytes";

/// `work`'s verdict on `green` (`Green::judge_named`), judged again once lent from the content plane every named text
/// its first judge read unlent (A-760 (b) 4d: a copied or spliced base held by name alone), let go once committed
/// ([`commit`]). A text the plane cannot give now leaves the first refusal; one read unlent again is counted, a bug.
fn judged_lent(
  state: &mut ShardState,
  green: DbVolumeId,
  work: &mut Increment,
) -> Option<Judgment> {
  use crate::vcs_store::name_holds;
  let base_unlent = |judgment: &Judgment, work: &Increment| {
    matches!(judgment, Judgment::ContentUnread { identity }
      if !work.named.runs.iter().any(|run| run.identity == *identity))
  };
  let first = state.greens.get_mut(&green)?.judge_named(work);
  if !base_unlent(&first, work) {
    return Some(first);
  }
  let wanted = state.greens.get_mut(&green)?.unlent_reads(work);
  if name_holds::lend_from_plane(state, green, &wanted).is_err() {
    return Some(first);
  }
  state.count(name_holds::NAME_LENT_VERDICTS, 1);
  let again = state.greens.get_mut(&green)?.judge_named(work);
  if base_unlent(&again, work) {
    state.count(name_holds::NAME_LEND_MISSED, 1);
  }
  Some(again)
}

/// Each work's verdict, held (`Green::judge_named`: a work naming its content gives its read runs' buffers to the
/// verdict, which the commit makes its files with no copy, A-760), in the order of the steps (none for a step with no
/// work); or every work refused. Before, `Green::judge` cloned every read buffer into the verdict (goal 93, main
/// 01a20182's ruling of 2026-11-22 ~11:50).
fn judge(
  state: &mut ShardState,
  steps: &mut [Step],
  held: &mut Held,
) -> Result<Vec<Option<Verdict>>, Vec<ChangeRefused>> {
  let mut verdicts = Vec::new();
  let mut refused = Vec::new();
  for (index, step) in steps.iter_mut().enumerate() {
    let Some((green, work)) = step.work_mut() else {
      verdicts.push(None);
      continue;
    };
    let judgment = judged_lent(state, green, work);
    // An accepted verdict takes the runs' buffers: any the work still holds were cloned. A refused one takes none, and
    // they go with the work.
    let kept = (0..work.named.runs.len())
      .filter_map(|at| work.read_run(at))
      .fold(0u64, |total, run| {
        total.saturating_add(u64::try_from(run.len()).unwrap_or(u64::MAX))
      });
    if kept > 0 && matches!(judgment, Some(Judgment::Accepted(_))) {
      state.count(READ_CLONED_BYTES, kept);
    }
    let why = match judgment {
      Some(Judgment::Accepted(judged)) => {
        verdicts.push(Some(Verdict::Held(Box::new(judged))));
        continue;
      }
      Some(Judgment::AcceptedBefore { version }) => {
        verdicts.push(Some(Verdict::Before(version)));
        continue;
      }
      Some(Judgment::Conflict { windows }) => {
        if let Some(secured) = held.secured.iter_mut().find(|(at, _)| *at == green) {
          secured.1 = Some(work.id);
        }
        ChangeRefusal::Conflict { windows }
      }
      Some(Judgment::BaseUnavailable { base, .. }) => {
        ChangeRefusal::Refused(Refusal::UnknownBase {
          green: to_wire_volume(green),
          version: base,
        })
      }
      Some(Judgment::CopyRefused { path, case }) => {
        ChangeRefusal::Refused(crate::verbs::copy_refused(path, case))
      }
      Some(Judgment::ContentUnread { .. }) => ChangeRefusal::Refused(Refusal::ContentUnavailable),
      None => ChangeRefusal::Refused(Refusal::ContentUnavailable),
    };
    verdicts.push(None);
    refused.push(ChangeRefused { index, why });
  }
  if refused.is_empty() {
    Ok(verdicts)
  } else {
    Err(refused)
  }
}

/// The entry the push records: the deletes' checks then `parts`'; the new members; each held verdict's version, the one
/// after the head its verdict was held at (the position the push read, A-642), its chain entry as large as one for its
/// work can be (the real one is made once the verdict commits, A-257); the deletes' retires, the creates' binds, then
/// `parts`' names; `parts`' state.
fn entry(
  repository: RepositoryId,
  (steps, verdicts): (&[Step], &[Option<Verdict>]),
  parts: EntryParts,
) -> RepositoryEntry {
  let mut entry = RepositoryEntry {
    repository,
    ..RepositoryEntry::default()
  };
  for step in steps {
    match step {
      Step::Delete {
        name,
        green,
        version,
      } => {
        entry.checks.push(RepositoryCheck::Head {
          green: *green,
          version: *version,
        });
        entry.names.push(NameChange::Retire { name: name.clone() });
      }
      Step::Create {
        from,
        version,
        record,
        ..
      } => entry.joined.push(MemberRecord {
        repository,
        green: record.id,
        origin: MemberOrigin::Branch {
          from: *from,
          version: *version,
        },
      }),
      Step::Update { .. } => {}
      // The register's guard holds the green at its push's base, as a delete's holds the version the client saw.
      Step::Defer { green, push } => entry.checks.push(RepositoryCheck::Head {
        green: *green,
        version: push.push().base_version,
      }),
    }
  }
  for (step, verdict) in steps.iter().zip(verdicts) {
    if let (Some((green, work)), Some(Verdict::Held(judged))) = (step.work(), verdict) {
      let bound = slates_merge::chain_entry::entry_bound(work);
      entry.versions.push(GreenVersion {
        green,
        version: judged.at().saturating_add(1),
        entry: vec![0; bound],
      });
    }
    // A deferred create's version 1, and a deferred update's next version, is its push's record (P2): judged by the
    // register at the record's own length.
    if let Some((green, push)) = owed(step) {
      entry.versions.push(GreenVersion {
        green,
        version: push.version(),
        entry: vec![0; push.encode().len()],
      });
    }
  }
  for step in steps {
    if let Step::Create { name, record, .. } = step {
      entry.names.push(NameChange::Bind {
        name: name.clone(),
        green: record.id,
      });
    }
  }
  entry.checks.extend(parts.checks);
  entry.names.extend(parts.names);
  entry.state = parts.state;
  entry.objects = parts.objects;
  entry
}

/// Counter: a green checkpointed inside an append's own step, its entry meeting the chains at their cap (A-238): each a
/// whole fold of the green's identity in the step that appends (the git/git push's watchdog kill, 2026-11-21: about a
/// thousand members folded in one step, 1,142 ms). Format: a counter name.
pub(crate) const APPEND_CHECKPOINTED: &str = "repository.push.append_checkpointed";

/// The entry judged whole by the register's guard, the new greens counted held, before anything is applied; a chain
/// past the merge budget is checkpointed at each green's floor first and judged again (A-238, as a submit's admission):
/// each green the entry updates, and every other member of its repository (A-753: an entry that only makes branches
/// updates none, and the room its new greens need is held by the chains of the repository's other branches).
fn admit(
  state: &mut ShardState,
  made: &[VolumeRecord],
  entry: &RepositoryEntry,
  steps: &[Step],
) -> Result<(), AppendRefused> {
  let first = state.db.partition().check_creating(made, entry);
  if let Err(slates_db::DbError::Capacity {
    table: "green_chains",
  }) = first
  {
    let members: Vec<DbVolumeId> = state
      .db
      .partition()
      .repositories()
      .members_of(entry.repository)
      .map(|member| member.green)
      .collect();
    let updated = steps.iter().filter_map(|step| match step {
      Step::Update { green, .. } => Some(*green),
      _ => None,
    });
    let folded: std::collections::BTreeSet<DbVolumeId> = updated.chain(members).collect();
    for green in folded {
      // Counted: each a fold run inside the append's own step, which a push's judge makes room for in its turns before
      // (`git_push`'s chains' room, A-238 moved off the append's step), so for a push this counts what its turns left.
      state.count(APPEND_CHECKPOINTED, 1);
      crate::merge_service::checkpoint_green_when(
        state,
        green,
        crate::merge_service::Doubling::Now,
      );
    }
  } else {
    return first.map_err(|error| AppendRefused::Entry(refusal_of_db(&error)));
  }
  state
    .db
    .partition()
    .check_creating(made, entry)
    .map_err(|error| AppendRefused::Entry(refusal_of_db(&error)))
}

/// What a step's commit made: the version, and what its chain entry records of it.
type Committed = Vec<Option<(u64, Vec<slates_merge::chain_entry::Applied>, bool)>>;

/// Commits every held verdict (`Green::commit_judged`), each at the head it was judged at, which nothing moved since:
/// every held verdict's head is confirmed before the first commits, so the commits happen all or none.
fn commit(
  state: &mut ShardState,
  steps: &[Step],
  verdicts: Vec<Option<Verdict>>,
) -> Result<Committed, AppendRefused> {
  for (step, verdict) in steps.iter().zip(&verdicts) {
    if let (Some((green, _)), Some(Verdict::Held(judged))) = (step.work(), verdict) {
      let head = state.greens.get(&green).map(|engine| engine.head());
      if head != Some(judged.at()) {
        state.count(PUSH_COMMIT_REFUSED, 1);
        return Err(AppendRefused::Entry(Refusal::ContentUnavailable));
      }
    }
    // A deferred update's green still at its push's base, owing nothing: confirmed before anything commits.
    if let Step::Defer { green, push } = step {
      let head = state.greens.get(green).map(|engine| engine.head());
      if head != Some(push.push().base_version) {
        state.count(PUSH_COMMIT_REFUSED, 1);
        return Err(AppendRefused::Entry(Refusal::ContentUnavailable));
      }
    }
  }
  let mut committed = Vec::new();
  for (step, verdict) in steps.iter().zip(verdicts) {
    let made = match (step.work(), verdict) {
      (Some((green, _)), Some(Verdict::Held(judged))) => {
        // The named texts its commit reads lent it first (A-760 (b) 4d), let go after.
        let done = crate::vcs_store::name_holds::commit_judged_lent(state, green, *judged);
        match done {
          Some((version, applied)) => Some((version, applied, false)),
          // Confirmed above in this same step; counted should it ever be met.
          _ => {
            state.count(PUSH_COMMIT_REFUSED, 1);
            None
          }
        }
      }
      (_, Some(Verdict::Before(version))) => Some((version, Vec::new(), true)),
      // The deferred update's green owes its push from here (P2), its content its head's until first reached.
      _ => match step {
        Step::Defer { green, push } => match state.greens.defer(green, push.clone()) {
          Some(version) => {
            state.count(UPDATES_DEFERRED, 1);
            Some((version, Vec::new(), false))
          }
          None => {
            state.count(PUSH_COMMIT_REFUSED, 1);
            None
          }
        },
        _ => None,
      },
    };
    committed.push(made);
  }
  Ok(committed)
}

/// Each held verdict's increment held in the content store, where its chain entry names it (chain entry format 6, as
/// `verbs::submit` holds a submit's), before any verdict commits: refused typed at the shard's room with what it held
/// given back. In the order of the steps, none for a step with no held verdict.
fn hold_increments(
  state: &mut ShardState,
  (steps, verdicts): (&[Step], &[Option<Verdict>]),
  ahead: &[crate::held_ahead::HeldAhead],
) -> Result<Vec<Option<crate::merge_service::HeldIncrement>>, AppendRefused> {
  let mut increments = Vec::new();
  for (step, verdict) in steps.iter().zip(verdicts) {
    let held = match (step.work(), verdict) {
      (Some((green, work)), Some(Verdict::Held(judged))) => {
        let version = judged.at().saturating_add(1);
        match hold_increment(state, (green, version), work, ahead) {
          Ok(held) => Some(held),
          Err(refusal) => {
            release_increments(state, steps, increments);
            return Err(AppendRefused::Entry(refusal));
          }
        }
      }
      _ => None,
    };
    increments.push(held);
  }
  Ok(increments)
}

/// `work`'s increment held as `green`'s `version`'s inputs: named where it was held ahead of this step and the content
/// store still holds it (`crate::held_ahead`, nothing copied or hashed), else held whole now.
fn hold_increment(
  state: &mut ShardState,
  (green, version): (DbVolumeId, u64),
  work: &Increment,
  ahead: &[crate::held_ahead::HeldAhead],
) -> Result<crate::merge_service::HeldIncrement, Refusal> {
  let named = crate::held_ahead::held_for(ahead, green, &work.id).filter(|held| {
    state.held_content.holds_manifest(
      slates_db::register::ObjectId(green.bytes),
      &held.inputs.identity,
    )
  });
  match named {
    Some(held) => {
      state.count(crate::held_ahead::AHEAD_NAMED, 1);
      Ok(crate::merge_service::HeldIncrement {
        inputs: held.inputs,
        fresh: held.fresh,
      })
    }
    None => crate::merge_service::hold_increment(state, green, version, &work.encode()),
  }
}

/// Gives back what [`hold_increments`] stored, for a push that records nothing.
fn release_increments(
  state: &mut ShardState,
  steps: &[Step],
  increments: Vec<Option<crate::merge_service::HeldIncrement>>,
) {
  for (step, held) in steps.iter().zip(increments) {
    if let (Some((green, _)), Some(held)) = (step.work(), held) {
      crate::merge_service::release_refused_increment(state, green, held);
    }
  }
}

/// The entry with each version's real chain entry: the increment by where the content store holds it, what its verdict
/// resolved it to, and, where the version's record goes to holders, the head's identity for a replay to compare (as
/// `verbs::record_accepted` records a submit's).
fn recorded_entry(
  state: &mut ShardState,
  mut entry: RepositoryEntry,
  steps: &[Step],
  committed: &Committed,
  increments: &[Option<crate::merge_service::HeldIncrement>],
) -> RepositoryEntry {
  let mut versions = Vec::new();
  for ((step, made), held) in steps.iter().zip(committed).zip(increments) {
    let (Some((green, work)), Some((version, applied, false)), Some(held)) =
      (step.work(), made, held)
    else {
      continue;
    };
    let identity = crate::merge_service::record_goes_to_holders(state, green)
      .then(|| {
        state
          .greens
          .get_mut(&green)
          .map(|mut engine| engine.head_identity())
      })
      .flatten();
    let bytes = slates_merge::chain_entry::encode(held.inputs, work, applied, None, identity, None);
    versions.push(GreenVersion {
      green,
      version: *version,
      entry: bytes,
    });
  }
  // Each deferred create's version 1 and deferred update's next version: its push's record, never a built work (P2).
  for (green, push) in steps.iter().filter_map(owed) {
    versions.push(GreenVersion {
      green,
      version: push.version(),
      entry: push.encode(),
    });
  }
  entry.versions = versions;
  entry
}

/// Records `ops` as one log record: in the caller's transaction when one is open, else in one of the push's own, closed
/// whatever it met, as a verb's is (`verbs::run_recorded`), and a refused publication's in-memory effects reconciled
/// with what the log holds (`verbs::reconcile_unpublished_effects`). Each op was judged whole beforehand
/// (`Partition::check_creating`), so none is refused here; one that is, is counted.
fn record_all(state: &mut ShardState, ops: Vec<Op>) -> Result<(), AppendRefused> {
  let now = state.clock.monotonic_ns();
  let own = !state.db.in_transaction();
  if own {
    state.db.begin();
  }
  let mut recorded = Ok(());
  for op in ops {
    // Moved into the record, never copied: the entry carries the push's or revision's objects (A-742).
    if let Err(error) = state.db.mutate_owned(&mut state.segment, op, now) {
      state.count(PUSH_COMMIT_REFUSED, 1);
      recorded = Err(AppendRefused::Entry(refusal_of_db(&error)));
      break;
    }
  }
  if own && let Err(error) = state.db.commit(&mut state.segment) {
    crate::verbs::reconcile_unpublished_effects(state);
    return Err(AppendRefused::Entry(refusal_of_db(&error)));
  }
  recorded
}

/// After the record: each version's merge record enqueued for its holders (version 0 first for a new green, as a new
/// branch's is), each green's retention trued up to what it retained.
fn after(state: &mut ShardState, steps: &[Step], committed: &Committed, held: &mut Held) {
  for (step, made) in steps.iter().zip(committed) {
    if let Step::Create { record, .. } = step {
      crate::merge_service::enqueue_record(
        state,
        record.id,
        0,
        [0u8; 32],
        0,
        Vec::new(),
        (None, None),
      );
    }
    if let (Some((green, work)), Some((version, _, false))) = (step.work(), made) {
      crate::merge_service::enqueue_record(
        state,
        green,
        *version,
        work.id,
        work.base,
        work.evidence.clone(),
        (None, None),
      );
    }
    // A deferred create's version 1 and a deferred update's next version go to holders as any version does (P2, R8):
    // its push's record is its inputs, which a holder defers its replica by, so a takeover adopts a green that owes the
    // same push.
    if let Some((green, push)) = owed(step) {
      let pushed = push.push();
      crate::merge_service::enqueue_record(
        state,
        green,
        push.version(),
        pushed.name,
        pushed.base_version,
        Vec::new(),
        (None, None),
      );
    }
  }
  settle_secured(state, held);
}

/// Trues each secured green's retention up to what it retained, and forgets it.
fn settle_secured(state: &mut ShardState, held: &mut Held) {
  for (green, _) in std::mem::take(&mut held.secured) {
    let _ = crate::merge_service::settle_green_retention(state, green);
  }
}

/// The answer of a push that recorded nothing: every work's version from before.
fn answer(steps: &[Step], verdicts: &[Option<Verdict>]) -> Appended {
  let committed: Committed = verdicts
    .iter()
    .map(|verdict| match verdict {
      Some(Verdict::Before(version)) => Some((*version, Vec::new(), true)),
      _ => None,
    })
    .collect();
  answer_committed(steps, &committed, false)
}

/// The answer, change by change.
fn answer_committed(steps: &[Step], committed: &Committed, recorded: bool) -> Appended {
  let changes = steps
    .iter()
    .zip(committed)
    .map(|(step, made)| match step {
      Step::Update { green, .. } => {
        let (version, before) = made
          .as_ref()
          .map_or((0, false), |(version, _, before)| (*version, *before));
        BranchAppended::Moved {
          green: *green,
          version,
          before,
        }
      }
      Step::Create {
        record, deferred, ..
      } => BranchAppended::Created {
        green: record.id,
        version: made.as_ref().map_or_else(
          || {
            deferred
              .as_ref()
              .map_or(0, slates_merge::push::DeferredPush::version)
          },
          |(version, ..)| *version,
        ),
      },
      Step::Delete { green, .. } => BranchAppended::Deleted { green: *green },
      Step::Defer { green, push } => BranchAppended::Moved {
        green: *green,
        version: push.version(),
        before: false,
      },
    })
    .collect();
  Appended { changes, recorded }
}
