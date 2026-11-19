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
  /// `slates_svn::commit::record`); a git push's is its sealed push's own ([`push_sealed`]).
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

/// One change, planned: what it does to which green.
enum Step {
  /// `green` moves by `work`.
  Update { green: DbVolumeId, work: Increment },
  /// A new green, `record`, made at `version` of `from` and bound to `name`, moved by `work` (based at 0) if any.
  Create {
    name: String,
    from: DbVolumeId,
    version: u64,
    record: VolumeRecord,
    work: Option<Increment>,
  },
  /// `name` retired from `green`, which must be at `version`.
  Delete {
    name: String,
    green: DbVolumeId,
    version: u64,
  },
}

impl Step {
  /// The green the step's work moves, and the work.
  fn work(&self) -> Option<(DbVolumeId, &Increment)> {
    match self {
      Step::Update { green, work } => Some((*green, work)),
      Step::Create { record, work, .. } => work.as_ref().map(|work| (record.id, work)),
      Step::Delete { .. } => None,
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
  push_holding(state, (principal, repository), changes, parts, None)
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
    changes,
    parts,
    Some((sealed, moves)),
  )
}

/// A sealed push and the moves its append commits.
type Sealed<'p> = (&'p mut crate::vcs_store::SealedPush, &'p [StoreMove]);

/// [`push`] and [`push_sealed`]'s one path, with the sealed push if any.
fn push_holding(
  state: &mut ShardState,
  (principal, repository): (&Principal, RepositoryId),
  changes: Vec<BranchChange>,
  parts: EntryParts,
  sealed: Option<Sealed<'_>>,
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
  let steps = plan(state, &record, principal, changes).map_err(AppendRefused::Changes)?;
  let mut held = Held::default();
  let pushed = push_planned(state, &record, (steps, parts), &mut held, sealed);
  if pushed.is_err() {
    held.give_back(state);
  }
  pushed
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
  let id = crate::verbs::fresh_volume_id_at(state, 0)
    .ok_or_else(|| refused(crate::verbs::ids_exhausted()))?;
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
    record,
    work,
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
  (steps, parts): (Vec<Step>, EntryParts),
  held: &mut Held,
  mut sealed: Option<Sealed<'_>>,
) -> Result<Appended, AppendRefused> {
  make(state, &steps, held).map_err(AppendRefused::Changes)?;
  secure(state, &steps, held).map_err(AppendRefused::Changes)?;
  let verdicts = judge(state, &steps, held).map_err(AppendRefused::Changes)?;
  let made: Vec<VolumeRecord> = steps
    .iter()
    .filter_map(|step| match step {
      Step::Create { record, .. } => Some(record.clone()),
      _ => None,
    })
    .collect();
  let mut entry = entry(record.id, (&steps, &verdicts), parts);
  if let Some((sealed, moves)) = sealed.as_mut() {
    let resolved = resolve(state, (&steps, &entry), moves)?;
    if let Err(refused) = sealed.admit((&mut state.store, &mut state.vcs_stores), resolved) {
      return Err(AppendRefused::Entry(crate::vcs_store::refusal_of(
        state, &refused,
      )));
    }
    match sealed.record(&state.vcs_stores) {
      Ok(objects) => entry.objects = objects,
      Err(refused) => {
        return Err(AppendRefused::Entry(crate::vcs_store::refusal_of(
          state, &refused,
        )));
      }
    }
  }
  let changes_anything = !(entry.joined.is_empty()
    && entry.versions.is_empty()
    && entry.names.is_empty()
    && entry.state.is_empty());
  admit(state, &made, &entry, &steps)?;
  if !changes_anything {
    // Every work was sent before and nothing else changes: nothing is recorded, and what was secured is given back.
    settle_secured(state, held);
    return Ok(answer(&steps, &verdicts));
  }
  // The store commits the push's objects between the guard and the apply (A-662), so a refusal here applies nothing.
  let store_committed = match sealed {
    Some((sealed, _)) => {
      if let Err(refused) = crate::vcs_store::commit(state, sealed) {
        return Err(AppendRefused::Entry(crate::vcs_store::refusal_of(
          state, &refused,
        )));
      }
      true
    }
    None => false,
  };
  let moved: Vec<(DbVolumeId, u64)> = entry
    .versions
    .iter()
    .map(|version| (version.green, version.version))
    .collect();
  let applied = apply_planned(state, (&steps, verdicts), (entry, made), held);
  if applied.is_err() && store_committed {
    // The store is ahead of a log that refused the entry: made again from the log (A-662).
    crate::vcs_store::rebuild(state, record.id);
  }
  if applied.is_ok() {
    // Each green the entry moved keeps its version's map of paths to git's files (A-664 part 3, `vcs_store::paths`).
    for (green, version) in moved {
      crate::vcs_store::paths::advance(state, green, version);
    }
  }
  applied
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
  held: &mut Held,
) -> Result<Appended, AppendRefused> {
  let committed = commit(state, steps, verdicts)?;
  let entry = recorded_entry(state, entry, steps, &committed);
  let mut ops: Vec<Op> = made
    .into_iter()
    .map(|record| Op::VolumeCreated { record })
    .collect();
  ops.push(Op::RepositoryAdvanced { entry });
  record_all(state, &ops)?;
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
  Held(Judged),
  /// Accepted before as this version.
  Before(u64),
}

/// Each work's verdict, held (`Green::judge`), in the order of the steps (none for a step with no work); or every
/// work refused.
fn judge(
  state: &mut ShardState,
  steps: &[Step],
  held: &mut Held,
) -> Result<Vec<Option<Verdict>>, Vec<ChangeRefused>> {
  let mut verdicts = Vec::new();
  let mut refused = Vec::new();
  for (index, step) in steps.iter().enumerate() {
    let Some((green, work)) = step.work() else {
      verdicts.push(None);
      continue;
    };
    let judgment = state
      .greens
      .get_mut(&green)
      .map(|mut engine| engine.judge(work));
    let why = match judgment {
      Some(Judgment::Accepted(judged)) => {
        verdicts.push(Some(Verdict::Held(judged)));
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
    }
  }
  for (step, verdict) in steps.iter().zip(verdicts) {
    if let (Some((green, work)), Some(Verdict::Held(judged))) = (step.work(), verdict) {
      let bound = slates_merge::chain_entry::entry_bound(work.encode().len(), work);
      entry.versions.push(GreenVersion {
        green,
        version: judged.at().saturating_add(1),
        entry: vec![0; bound],
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

/// The entry judged whole by the register's guard, the new greens counted held, before anything is applied; a chain
/// past the merge budget is checkpointed at each green's floor first and judged again (A-238, as a submit's admission).
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
    for step in steps {
      if let Step::Update { green, .. } = step {
        crate::merge_service::checkpoint_green_when(
          state,
          *green,
          crate::merge_service::Doubling::Now,
        );
      }
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
  }
  let mut committed = Vec::new();
  for (step, verdict) in steps.iter().zip(verdicts) {
    let made = match (step.work(), verdict) {
      (Some((green, _)), Some(Verdict::Held(judged))) => {
        let done = state
          .greens
          .get_mut(&green)
          .map(|mut engine| engine.commit_judged(judged));
        match done {
          Some(Ok((version, applied))) => Some((version, applied, false)),
          // Confirmed above in this same step; counted should it ever be met.
          _ => {
            state.count(PUSH_COMMIT_REFUSED, 1);
            None
          }
        }
      }
      (_, Some(Verdict::Before(version))) => Some((version, Vec::new(), true)),
      _ => None,
    };
    committed.push(made);
  }
  Ok(committed)
}

/// The entry with each version's real chain entry: the increment, what its verdict resolved it to, and, where the
/// version's record goes to holders, the head's identity for a replay to compare (as `verbs::record_accepted` records
/// a submit's).
fn recorded_entry(
  state: &mut ShardState,
  mut entry: RepositoryEntry,
  steps: &[Step],
  committed: &Committed,
) -> RepositoryEntry {
  let mut versions = Vec::new();
  for (step, made) in steps.iter().zip(committed) {
    let (Some((green, work)), Some((version, applied, false))) = (step.work(), made) else {
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
    let bytes =
      slates_merge::chain_entry::encode(&work.encode(), work, applied, None, identity, None);
    versions.push(GreenVersion {
      green,
      version: *version,
      entry: bytes,
    });
  }
  entry.versions = versions;
  entry
}

/// Records `ops` as one log record: in the caller's transaction when one is open, else in one of the push's own, closed
/// whatever it met, as a verb's is (`verbs::run_recorded`), and a refused publication's in-memory effects reconciled
/// with what the log holds (`verbs::reconcile_unpublished_effects`). Each op was judged whole beforehand
/// (`Partition::check_creating`), so none is refused here; one that is, is counted.
fn record_all(state: &mut ShardState, ops: &[Op]) -> Result<(), AppendRefused> {
  let now = state.clock.monotonic_ns();
  let own = !state.db.in_transaction();
  if own {
    state.db.begin();
  }
  let mut recorded = Ok(());
  for op in ops {
    if let Err(error) = state.db.mutate(&mut state.segment, op, now) {
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
      Step::Create { record, .. } => BranchAppended::Created {
        green: record.id,
        version: made.as_ref().map_or(0, |(version, ..)| *version),
      },
      Step::Delete { green, .. } => BranchAppended::Deleted { green: *green },
    })
    .collect();
  Appended { changes, recorded }
}
