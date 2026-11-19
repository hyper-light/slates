//! The repository verbs (§4.17.1 "The repository and its greens", items 1 to 5; A-515's records, A-575's verbs): a
//! repository's greens are members of one lineage on the repository's partition ([`crate::greens`]), so a branch made at
//! another branch's version shares every node of that version's state and costs what changed after it.
//!
//! **The verbs.** `CreateRepository` records the repository and its lineage's root, a new empty green or an unbound
//! green of the partition that joins as it stands, bound to the default branch. `CreateBranch` makes a member at a
//! version of a branch (its head when none) through `Lineage::branch` and binds the new name to it. `BindBranch` binds a
//! name to a member no name binds, which restores a retired branch; `RetireBranch` lets a name go and keeps its green
//! (item 5). Destroying a member is the green's own destroy verb: it retires the member from its lineage at once and
//! queues what only it held, which [`run_releases`] gives back a node a unit under the shard's slice.
//!
//! **Rights.** A repository's owner holds every right on it (§4.13) until §4.17.1's policy (shared piece 7) says
//! otherwise, so the branch verbs are its owner's, refused `Forbidden` to another principal; a green that becomes a root
//! is one its creator administers.
//!
//! **Bounds and refusals.** Every refusal is typed: a repository or branch that does not exist `NotFound`; a name taken,
//! a green bound or a member twice `AlreadyExists`; an empty name or one holding a NUL `InvalidName`, and so is a
//! member's volume name longer than a listing holds; a version past the source's head, or below the oldest version its
//! recovery can still branch from, `UnknownBase`; a fenced source `ContentUnavailable`; the partition's tables at their
//! derived capacity `TableAtCapacity`.
//!
//! **Durability.** Each verb's records commit as one atom with its completion record (the verb's transaction), the
//! engine installed only after them: a refused record leaves no engine ahead of its log, and a rolled-back one is undone
//! by `verbs::reconcile_unpublished_effects`. A member is rebuilt at recovery the way it joined, in the order it joined
//! (`verbs::rebuild_member_sliced`); the oldest version a live branch was made at is the floor its source's checkpoint
//! may not pass (`merge_service::reachable_floor`), so that version is there to branch from again.
//!
//! **Names.** A member's volume name is `<repository>:<branch>:<id>`: readable in a listing, unique on its partition by
//! its id.

use slates_db::catalog::{
  BaseRecord, BranchRecord, MemberOrigin, MemberRecord, NamePolicy as DbNamePolicy, PolicyRecord,
  RepositoryId, RepositoryRecord, Role, SnapshotId as DbSnapshotId, VolumeId as DbVolumeId,
  VolumeRecord, VolumeState,
};
use slates_db::op::Op;
use slates_ipc::protocol::{ObjectFormat, Principal, Refusal, ReplyBody, VcsTool, VolumeId};

use crate::error::refusal_of_db;
use crate::greens::ReleaseProgress;
use crate::state::ShardState;
use crate::verbs::{forbidden, refused, to_db_volume, to_wire_volume};

/// The database's form of a repository's tool.
fn db_tool(tool: VcsTool) -> slates_db::catalog::VcsTool {
  match tool {
    VcsTool::Git => slates_db::catalog::VcsTool::Git,
    VcsTool::Mercurial => slates_db::catalog::VcsTool::Mercurial,
    VcsTool::Subversion => slates_db::catalog::VcsTool::Subversion,
  }
}

/// The database's form of a repository's object format.
fn db_format(format: ObjectFormat) -> slates_db::catalog::ObjectFormat {
  match format {
    ObjectFormat::Sha1 => slates_db::catalog::ObjectFormat::Sha1,
    ObjectFormat::Sha256 => slates_db::catalog::ObjectFormat::Sha256,
    ObjectFormat::Revisions => slates_db::catalog::ObjectFormat::Revisions,
  }
}

/// Records `op` in the verb's transaction; its refusal typed.
fn record(state: &mut ShardState, op: &Op) -> Result<(), Refusal> {
  let now = state.clock.monotonic_ns();
  state
    .db
    .mutate(&mut state.segment, op, now)
    .map(|_| ())
    .map_err(|error| refusal_of_db(&error))
}

/// The repository named `name` on this partition, when `principal` owns it: `NotFound` for none, `Forbidden` for
/// another principal's (module docs, "Rights").
fn owned_repository(
  state: &ShardState,
  principal: &Principal,
  name: &str,
  verb: &str,
) -> Result<RepositoryRecord, ReplyBody> {
  let record = state
    .db
    .partition()
    .repositories()
    .repository_named(name)
    .cloned()
    .ok_or_else(|| refused(Refusal::NotFound))?;
  if &record.owner != principal {
    return Err(forbidden(verb));
  }
  Ok(record)
}

/// A member's volume name (module docs, "Names"), refused `InvalidName` when a listing could not hold it.
fn member_name(repository: &str, branch: &str, id: DbVolumeId) -> Result<String, Refusal> {
  let name = format!("{repository}:{branch}:{}", crate::verbs::hex_of(&id.bytes));
  if name.len() > crate::verbs::longest_name() {
    return Err(Refusal::InvalidName);
  }
  Ok(name)
}

/// The catalog record of a new member green: a green owned by `principal`, its versions judged with `require_evidence`.
fn member_record(
  state: &ShardState,
  principal: &Principal,
  (id, name): (DbVolumeId, String),
  require_evidence: bool,
) -> VolumeRecord {
  VolumeRecord {
    id,
    name,
    owner_shard: state.partition,
    policy: PolicyRecord {
      size: crate::verbs::db_size(slates_ipc::protocol::SizeClass::Dynamic { max: 0 }),
      names: DbNamePolicy::Exact,
      require_locked: false,
      role: Role::Green {
        require_evidence,
        head_version: 0,
      },
      live_shipped: false,
      live_holders: None,
      write: None,
      write_version: 0,
      signers: Vec::new(),
    },
    base: BaseRecord::Scratch,
    head: DbSnapshotId::default(),
    epoch: 0,
    referenced_bytes: 0,
    unique_bytes: 0,
    state: VolumeState::Live,
    lease: None,
    owner: principal.clone(),
    access: Vec::new(),
    created_ns: state.clock.monotonic_ns(),
    catalog_version: 0,
    taken_over: None,
  }
}

/// A green of this partition that may become a repository's root: one `principal` administers, whose engine stands
/// alone here (not fenced, not another lineage's member).
fn adoptable(
  state: &ShardState,
  principal: &Principal,
  volume: VolumeId,
) -> Result<DbVolumeId, Refusal> {
  let record = crate::merge_service::find_record(state, volume)?;
  if !matches!(record.policy.role, Role::Green { .. }) {
    return Err(Refusal::NotGreen);
  }
  if !crate::verbs::rights_of(&record, principal).admin {
    return Err(Refusal::Forbidden {
      verb: "create_repository".to_owned(),
    });
  }
  if state.fenced_greens.contains_key(&record.id) {
    return Err(Refusal::ContentUnavailable);
  }
  if state.greens.lineage_of(&record.id).is_some() {
    return Err(Refusal::AlreadyExists { existing: volume });
  }
  if !state.greens.contains_key(&record.id) {
    return Err(Refusal::NotFound);
  }
  Ok(record.id)
}

/// `CreateRepository` (module docs): the repository, its root and the default branch's binding, then the root's engine
/// in a new lineage. Answered `GreenCreated` with the root.
pub(crate) fn create_repository(
  state: &mut ShardState,
  principal: &Principal,
  (name, tool, format, default_branch): (&str, VcsTool, ObjectFormat, &str),
  green: Option<VolumeId>,
) -> ReplyBody {
  let adopted = match green.map(|volume| adoptable(state, principal, volume)) {
    Some(Ok(id)) => Some(id),
    Some(Err(refusal)) => return refused(refusal),
    None => None,
  };
  let Some(repository) = crate::verbs::fresh_volume_id(state) else {
    return refused(crate::verbs::ids_exhausted());
  };
  let repository = RepositoryId {
    bytes: repository.bytes,
  };
  match record_repository(
    state,
    principal,
    (repository, name, tool, format, default_branch),
    adopted,
  ) {
    Ok(root) => {
      install_root(state, repository, root, adopted.is_some());
      ReplyBody::GreenCreated {
        id: to_wire_volume(root),
      }
    }
    Err(refusal) => refused(refusal),
  }
}

/// The records of a new repository: its record, its root (a new green unless one is adopted), the root's membership
/// and the default branch's binding. The root's id.
fn record_repository(
  state: &mut ShardState,
  principal: &Principal,
  (repository, name, tool, format, default_branch): (
    RepositoryId,
    &str,
    VcsTool,
    ObjectFormat,
    &str,
  ),
  adopted: Option<DbVolumeId>,
) -> Result<DbVolumeId, Refusal> {
  record(
    state,
    &Op::RepositoryCreated {
      record: RepositoryRecord {
        id: repository,
        name: name.to_owned(),
        tool: db_tool(tool),
        format: db_format(format),
        default_branch: Some(default_branch.to_owned()),
        owner: principal.clone(),
      },
    },
  )?;
  let root = match adopted {
    Some(id) => id,
    None => {
      let id = crate::verbs::fresh_volume_id(state).ok_or_else(crate::verbs::ids_exhausted)?;
      let named = member_name(name, default_branch, id)?;
      let volume = member_record(state, principal, (id, named), false);
      record(state, &Op::VolumeCreated { record: volume })?;
      id
    }
  };
  record(
    state,
    &Op::RepositoryGreenJoined {
      record: MemberRecord {
        repository,
        green: root,
        origin: MemberOrigin::Root,
      },
    },
  )?;
  record(
    state,
    &Op::RepositoryBranchAdded {
      record: BranchRecord {
        repository,
        name: default_branch.to_owned(),
        green: root,
      },
    },
  )?;
  Ok(root)
}

/// The root's engine in a new lineage, once its records stand: the adopted green's own engine, moved in as it stands,
/// or a new empty one, whose version 0's merge record is enqueued as a new green's is.
fn install_root(state: &mut ShardState, repository: RepositoryId, root: DbVolumeId, adopted: bool) {
  let engine = if adopted {
    state.greens.remove(&root).unwrap_or_default()
  } else {
    let mut engine = slates_merge::engine::Green::new();
    engine.set_rejected_budget(crate::merge_service::rejected_cache_budget(state));
    engine
  };
  if state.greens.found(repository, root, engine).is_err() {
    state.count(LINEAGE_UNINSTALLED, 1);
    return;
  }
  if !adopted {
    crate::merge_service::enqueue_record(state, root, 0, [0u8; 32], 0, Vec::new(), (None, None));
  }
}

/// Counter: a lineage the records named that could not be installed (its repository already had one): zero in every
/// correct use. Format: a counter name in the daemon's status report.
pub(crate) const LINEAGE_UNINSTALLED: &str = "repository.lineage_uninstalled";

/// The version of `source` a branch is made at, checked: its head when `version` is none; `UnknownBase` past the head
/// or below the oldest version the source reconstructs, in memory or at a recovery (its checkpoint's).
fn branch_version(
  state: &ShardState,
  source: DbVolumeId,
  version: Option<u64>,
) -> Result<u64, Refusal> {
  if state.fenced_greens.contains_key(&source) {
    return Err(Refusal::ContentUnavailable);
  }
  let (head, floor) = state
    .greens
    .get(&source)
    .map(|engine| {
      (
        engine.head(),
        engine.folded_below().max(engine.mappable_from()),
      )
    })
    .ok_or(Refusal::NotFound)?;
  let durable = state.db.partition().green_floor(source);
  let version = version.unwrap_or(head);
  if version > head || version < floor.max(durable) {
    return Err(Refusal::UnknownBase {
      green: to_wire_volume(source),
      version,
    });
  }
  Ok(version)
}

/// `CreateBranch` (module docs): branch `name` of `repository` at `version` of branch `from`, made without a walk.
/// Answered `GreenCreated` with the new member.
pub(crate) fn create_branch(
  state: &mut ShardState,
  principal: &Principal,
  (repository, name, from): (&str, &str, &str),
  version: Option<u64>,
) -> ReplyBody {
  let record = match owned_repository(state, principal, repository, "create_branch") {
    Ok(record) => record,
    Err(reply) => return reply,
  };
  let Some(source) = state.db.partition().repositories().branch(record.id, from) else {
    return refused(Refusal::NotFound);
  };
  let version = match branch_version(state, source, version) {
    Ok(version) => version,
    Err(refusal) => return refused(refusal),
  };
  let require_evidence = state
    .db
    .partition()
    .volume(source)
    .is_some_and(|volume| {
      matches!(
        volume.policy.role,
        Role::Green {
          require_evidence: true,
          ..
        }
      )
    });
  let made = record_branch(state, principal, (&record, name), (source, version), require_evidence);
  let id = match made {
    Ok(id) => id,
    Err(refusal) => return refused(refusal),
  };
  // Checked above on this single-threaded shard, so it is made; a lineage that refuses it is counted, never served.
  if state.greens.branch(&source, version, id).is_err() {
    state.count(BRANCH_UNINSTALLED, 1);
    return refused(Refusal::ContentUnavailable);
  }
  let budget = crate::merge_service::rejected_cache_budget(state);
  if let Some(mut engine) = state.greens.get_mut(&id) {
    engine.set_rejected_budget(budget);
  }
  crate::merge_service::enqueue_record(state, id, 0, [0u8; 32], 0, Vec::new(), (None, None));
  ReplyBody::GreenCreated {
    id: to_wire_volume(id),
  }
}

/// Counter: a branch the records named that its lineage refused to make: zero in every correct use. Format: a counter
/// name in the daemon's status report.
pub(crate) const BRANCH_UNINSTALLED: &str = "repository.branch_uninstalled";

/// The records of a new branch: its green, its membership (made from `source` at `version`) and its name's binding.
/// The new green's id.
fn record_branch(
  state: &mut ShardState,
  principal: &Principal,
  (repository, name): (&RepositoryRecord, &str),
  (source, version): (DbVolumeId, u64),
  require_evidence: bool,
) -> Result<DbVolumeId, Refusal> {
  let id = crate::verbs::fresh_volume_id(state).ok_or_else(crate::verbs::ids_exhausted)?;
  let named = member_name(&repository.name, name, id)?;
  let volume = member_record(state, principal, (id, named), require_evidence);
  record(state, &Op::VolumeCreated { record: volume })?;
  record(
    state,
    &Op::RepositoryGreenJoined {
      record: MemberRecord {
        repository: repository.id,
        green: id,
        origin: MemberOrigin::Branch {
          from: source,
          version,
        },
      },
    },
  )?;
  record(
    state,
    &Op::RepositoryBranchAdded {
      record: BranchRecord {
        repository: repository.id,
        name: name.to_owned(),
        green: id,
      },
    },
  )?;
  Ok(id)
}

/// `BindBranch` (module docs): `name` bound to `green`, a member of the repository's lineage. Answered `Acknowledged`.
pub(crate) fn bind_branch(
  state: &mut ShardState,
  principal: &Principal,
  (repository, name): (&str, &str),
  green: VolumeId,
) -> ReplyBody {
  let record = match owned_repository(state, principal, repository, "bind_branch") {
    Ok(record) => record,
    Err(reply) => return reply,
  };
  let green = to_db_volume(green);
  let member = state.db.partition().repositories().member(green).cloned();
  if member.is_none_or(|member| member.repository != record.id) {
    return refused(Refusal::NotFound);
  }
  let op = Op::RepositoryBranchAdded {
    record: BranchRecord {
      repository: record.id,
      name: name.to_owned(),
      green,
    },
  };
  match self::record(state, &op) {
    Ok(()) => ReplyBody::Acknowledged,
    Err(refusal) => refused(refusal),
  }
}

/// `RetireBranch` (module docs): `name` leaves the namespace and its green stays a member. Answered `Acknowledged`.
pub(crate) fn retire_branch(
  state: &mut ShardState,
  principal: &Principal,
  (repository, name): (&str, &str),
) -> ReplyBody {
  let record = match owned_repository(state, principal, repository, "retire_branch") {
    Ok(record) => record,
    Err(reply) => return reply,
  };
  let op = Op::RepositoryBranchRetired {
    repository: record.id,
    name: name.to_owned(),
  };
  match self::record(state, &op) {
    Ok(()) => ReplyBody::Acknowledged,
    Err(refusal) => refused(refusal),
  }
}

/// Retires member `id` from its lineage at its destroy (module docs): its engine is gone at once, and what only it held
/// is queued for [`run_releases`], which starts when none runs. False when `id` is no member.
pub(crate) fn retire_member(state: &mut ShardState, id: DbVolumeId) -> bool {
  match state.greens.retire_queued(&id) {
    None => false,
    Some(Ok(start)) => {
      if start {
        let spawned =
          slates_rt::futures::spawn(run_releases()).and_then(slates_rt::futures::detach);
        if spawned.is_err() {
          state.greens.release_task_refused();
          state.count(RELEASE_UNSTARTED, 1);
        }
      }
      true
    }
    // Refused only while a member of its lineage is taken out (a recovery's replay): counted; the engine stays.
    Some(Err(_)) => {
      state.count(RETIRE_REFUSED, 1);
      true
    }
  }
}

/// Counter: a release task that could not start; its releases wait for the next retire. Format: a counter name in the
/// daemon's status report.
pub(crate) const RELEASE_UNSTARTED: &str = "repository.release_unstarted";

/// Counter: a member's retire its lineage refused (a member taken out): zero in every correct use. Format: a counter
/// name in the daemon's status report.
pub(crate) const RETIRE_REFUSED: &str = "repository.retire_refused";

/// Gives the retired members' queued holds back a node a unit, in the shard's slices (`crate::image::sliced`), and
/// ends once nothing is owed; a slice whose lineage has a member taken out yields and tries again.
pub(crate) async fn run_releases() {
  loop {
    let progress = crate::state::with_state(|s| {
      crate::image::sliced(s, |s| match s.greens.release_unit() {
        ReleaseProgress::More => Ok(None),
        ReleaseProgress::Done => Ok(Some(true)),
        ReleaseProgress::Blocked => Ok(Some(false)),
      })
    });
    match progress {
      Some(Ok(None | Some(false))) => slates_rt::futures::yield_now().await,
      _ => return,
    }
  }
}
