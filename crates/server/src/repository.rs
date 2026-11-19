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
//! **The register (A-603).** Every change a verb makes of a repository is one entry of the repository's register at its
//! next position (`slates_db::repository`): a repository's root joining and its default branch's binding, a branch's
//! member joining and its name's binding, a bind, a retire, and every version of a member ([`version_record`]), so
//! the register counts every change and its checks read one position.
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
  BaseRecord, GreenVersion, MemberOrigin, MemberRecord, NameChange, NamePolicy as DbNamePolicy,
  PolicyRecord, Principal, RepositoryEntry, RepositoryId, RepositoryRecord, Role,
  SnapshotId as DbSnapshotId, VolumeId as DbVolumeId, VolumeRecord, VolumeState,
};
use slates_db::op::Op;
use slates_ipc::protocol::{ObjectFormat, Refusal, ReplyBody, VcsTool, VolumeId};
use slates_vfs::clock::Clock;

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

/// `op` judged by the partition's guard without recording it: a verb that records several operations judges each one a
/// later step could refuse before its first, since what a refused verb recorded commits with its refusal.
fn admits(state: &mut ShardState, op: &Op) -> Result<(), Refusal> {
  let now = state.clock.monotonic_ns();
  state
    .db
    .partition()
    .check(op, now)
    .map_err(|error| refusal_of_db(&error))
}

/// A branch name the namespace takes: not empty, holding no NUL, not taken in `repository` (§4.17.1, A-515), judged
/// before anything is recorded.
fn free_name(state: &ShardState, repository: RepositoryId, name: &str) -> Result<(), Refusal> {
  if name.is_empty() || name.contains('\0') {
    return Err(Refusal::InvalidName);
  }
  match state.db.partition().repositories().branch(repository, name) {
    Some(bound) => Err(Refusal::AlreadyExists {
      existing: to_wire_volume(bound),
    }),
    None => Ok(()),
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

/// An entry of `repository`'s register at its next position, changing nothing yet (A-603): `NotFound` for no such
/// repository.
fn next_entry(state: &ShardState, repository: RepositoryId) -> Result<RepositoryEntry, Refusal> {
  let position = state
    .db
    .partition()
    .repositories()
    .position(repository)
    .ok_or(Refusal::NotFound)?;
  Ok(RepositoryEntry {
    repository,
    position,
    ..RepositoryEntry::default()
  })
}

/// The record of `green`'s `version` and its chain entry: an entry of its repository's register when it is a member of
/// a repository that stands, which every version of a repository's green is (§4.17.1 shared piece 2, A-603), else the
/// green's own `GreenAdvanced`.
pub(crate) fn version_record(
  state: &ShardState,
  green: DbVolumeId,
  version: u64,
  entry: Vec<u8>,
) -> Op {
  let repositories = state.db.partition().repositories();
  let register = repositories
    .member(green)
    .map(|member| member.repository)
    .and_then(|repository| Some((repository, repositories.position(repository)?)));
  match register {
    Some((repository, position)) => Op::RepositoryAdvanced {
      entry: RepositoryEntry {
        repository,
        position,
        versions: vec![GreenVersion {
          green,
          version,
          entry,
        }],
        ..RepositoryEntry::default()
      },
    },
    None => Op::GreenAdvanced {
      green,
      increment: entry,
    },
  }
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
  state: &mut ShardState,
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
  if !state.greens.contains_key(&record.id) || record.id.partition() != state.partition {
    return Err(Refusal::NotFound);
  }
  if state
    .db
    .partition()
    .repositories()
    .binding_of(record.id)
    .is_some()
  {
    return Err(Refusal::AlreadyExists { existing: volume });
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
  // A new root's record is judged first: once the repository's is recorded, a refusal would leave it rootless.
  let created = match adopted {
    Some(_) => None,
    None => {
      let id = crate::verbs::fresh_volume_id(state).ok_or_else(crate::verbs::ids_exhausted)?;
      let named = member_name(name, default_branch, id)?;
      let created = Op::VolumeCreated {
        record: member_record(state, principal, (id, named), false),
      };
      admits(state, &created)?;
      Some((id, created))
    }
  };
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
  let root = match (adopted, created) {
    (Some(id), _) => id,
    (None, Some((id, created))) => {
      record(state, &created)?;
      id
    }
    (None, None) => return Err(Refusal::NotFound),
  };
  // The root joins and the default branch binds it in one entry of the new register (A-603).
  let mut entry = next_entry(state, repository)?;
  entry.joined.push(MemberRecord {
    repository,
    green: root,
    origin: MemberOrigin::Root,
  });
  entry.names.push(NameChange::Bind {
    name: default_branch.to_owned(),
    green: root,
  });
  record(state, &Op::RepositoryAdvanced { entry })?;
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
  let require_evidence = state.db.partition().volume(source).is_some_and(|volume| {
    matches!(
      volume.policy.role,
      Role::Green {
        require_evidence: true,
        ..
      }
    )
  });
  let made = record_branch(
    state,
    principal,
    (&record, name),
    (source, version),
    require_evidence,
  );
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
  // Everything a later record could refuse is judged before the first: the name, the source's membership of this
  // lineage, the new green's record.
  free_name(state, repository.id, name)?;
  let member = state.db.partition().repositories().member(source).cloned();
  if member.is_none_or(|member| member.repository != repository.id) {
    return Err(Refusal::NotFound);
  }
  let id = crate::verbs::fresh_volume_id(state).ok_or_else(crate::verbs::ids_exhausted)?;
  let named = member_name(&repository.name, name, id)?;
  let created = Op::VolumeCreated {
    record: member_record(state, principal, (id, named), require_evidence),
  };
  admits(state, &created)?;
  // The member joins and its name binds it in one entry of the repository's register (A-603).
  let mut entry = next_entry(state, repository.id)?;
  entry.joined.push(MemberRecord {
    repository: repository.id,
    green: id,
    origin: MemberOrigin::Branch {
      from: source,
      version,
    },
  });
  entry.names.push(NameChange::Bind {
    name: name.to_owned(),
    green: id,
  });
  record(state, &created)?;
  record(state, &Op::RepositoryAdvanced { entry })?;
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
  // One entry of the repository's register (A-603), whose guard binds a member of this lineage only.
  let entry = next_entry(state, record.id).map(|mut entry| {
    entry.names.push(NameChange::Bind {
      name: name.to_owned(),
      green: to_db_volume(green),
    });
    entry
  });
  match entry.and_then(|entry| self::record(state, &Op::RepositoryAdvanced { entry })) {
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
  // One entry of the repository's register (A-603).
  let entry = next_entry(state, record.id).map(|mut entry| {
    entry.names.push(NameChange::Retire {
      name: name.to_owned(),
    });
    entry
  });
  match entry.and_then(|entry| self::record(state, &Op::RepositoryAdvanced { entry })) {
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

#[cfg(test)]
mod tests {
  use slates_db::catalog::Principal;
  use slates_ipc::protocol::{
    ObjectFormat, Refusal, ReplyBody, RequestBody, VcsTool, VolumeId, VolumeRefusal,
  };

  use crate::greens::ReleaseProgress;
  use crate::state::ShardState;
  use crate::verbs::{dispatch, to_db_volume};

  fn owner() -> Principal {
    Principal::Uid {
      host: slates_db::HostId(0),
      uid: 1234,
    }
  }

  fn stranger() -> Principal {
    Principal::Uid {
      host: slates_db::HostId(0),
      uid: 4321,
    }
  }

  fn ask(state: &mut ShardState, principal: &Principal, body: RequestBody) -> ReplyBody {
    dispatch(state, 1, principal, body)
  }

  fn refusal(reply: ReplyBody) -> Refusal {
    match reply {
      ReplyBody::Refused { refusal } => refusal,
      other => panic!("not refused: {other:?}"),
    }
  }

  fn green_of(reply: ReplyBody) -> VolumeId {
    match reply {
      ReplyBody::GreenCreated { id } => id,
      other => panic!("no green: {other:?}"),
    }
  }

  fn create(state: &mut ShardState, name: &str, default: &str) -> ReplyBody {
    ask(
      state,
      &owner(),
      RequestBody::CreateRepository {
        name: name.to_owned(),
        tool: VcsTool::Git,
        format: ObjectFormat::Sha1,
        default_branch: default.to_owned(),
        green: None,
      },
    )
  }

  fn branch(
    state: &mut ShardState,
    (repository, name, from): (&str, &str, &str),
    version: Option<u64>,
  ) -> ReplyBody {
    ask(
      state,
      &owner(),
      RequestBody::CreateBranch {
        repository: repository.to_owned(),
        name: name.to_owned(),
        from: from.to_owned(),
        version,
      },
    )
  }

  /// Makes `green`'s next version through a work of its own, file `f` set to `bytes`; the work is destroyed after, so
  /// no reader is left holding the green's floor. The version made.
  fn write(state: &mut ShardState, green: VolumeId, bytes: &[u8], serial: u32) -> u64 {
    let ReplyBody::WorkCreated { id: work, .. } = ask(
      state,
      &owner(),
      RequestBody::CreateWork {
        green,
        name: format!("work-{serial}"),
      },
    ) else {
      panic!("no work over {green:?}")
    };
    let held = file(state, green).map_or(0, |bytes| bytes.len() as u64);
    let edited = ask(
      state,
      &owner(),
      RequestBody::Edit {
        work,
        path: "/f".to_owned(),
        at: 0,
        delete_len: held,
        bytes: bytes.to_vec(),
      },
    );
    assert!(matches!(edited, ReplyBody::Edited), "{edited:?}");
    let submitted = ask(
      state,
      &owner(),
      RequestBody::Submit {
        work,
        evidence: Vec::new(),
      },
    );
    let ReplyBody::Submitted {
      version: Some(version),
      ..
    } = submitted
    else {
      panic!("not accepted: {submitted:?}")
    };
    let destroyed = ask(state, &owner(), RequestBody::Destroy { volume: work });
    assert!(matches!(destroyed, ReplyBody::Destroyed), "{destroyed:?}");
    version
  }

  /// `green`'s file `f` at its head.
  fn file(state: &ShardState, green: VolumeId) -> Option<Vec<u8>> {
    state
      .greens
      .get(&to_db_volume(green))
      .and_then(|engine| engine.content("f").map(<[u8]>::to_vec))
  }

  /// The repository record of `name`.
  fn repository(state: &ShardState, name: &str) -> slates_db::catalog::RepositoryId {
    state
      .db
      .partition()
      .repositories()
      .repository_named(name)
      .map(|record| record.id)
      .expect("the repository")
  }

  /// A-575 (§4.17.1 items 1 to 5). Do: create a repository, write two versions of its root while a work holds version
  /// 1, branch it at version 1 and at its head, retire a branch and bind it again, and try every wrong verb: a taken
  /// repository name, an unknown branch, a version past the head, another principal, a name with a NUL, a bind of a
  /// green outside the lineage.
  /// Expect: each branch reads its version's bytes and the root its head's; no branch walked the lineage's maps; a
  /// retired name leaves its green, and binding it again restores it; each wrong verb refused by its kind.
  #[test]
  fn branches_share_their_lineage_and_each_wrong_verb_is_refused_by_its_kind() {
    crate::daemon::audit_on_shard(|state| {
      let root = green_of(create(state, "linux", "main"));
      assert_eq!(write(state, root, b"one", 1), 1);
      // A work based at version 1 holds it, as any reader does, so the root's checkpoint stays at or below it and a
      // branch can be made there; with no reader the checkpoint follows the head and an older version is refused.
      let held = ask(
        state,
        &owner(),
        RequestBody::CreateWork {
          green: root,
          name: "holder".to_owned(),
        },
      );
      assert!(matches!(held, ReplyBody::WorkCreated { .. }), "{held:?}");
      assert_eq!(write(state, root, b"two", 2), 2);
      let repo = repository(state, "linux");
      let walked = state.greens.lineage_walked(repo);
      let at_one = green_of(branch(state, ("linux", "topic", "main"), Some(1)));
      let at_head = green_of(branch(state, ("linux", "head", "main"), None));
      assert_eq!(
        state.greens.lineage_walked(repo),
        walked,
        "a branch walked the lineage's maps"
      );
      assert_eq!(file(state, at_one).as_deref(), Some(b"one".as_slice()));
      assert_eq!(file(state, at_head).as_deref(), Some(b"two".as_slice()));
      assert_eq!(file(state, root).as_deref(), Some(b"two".as_slice()));
      assert_eq!(write(state, at_one, b"ONE", 3), 1);
      assert_eq!(file(state, at_one).as_deref(), Some(b"ONE".as_slice()));
      assert_eq!(file(state, root).as_deref(), Some(b"two".as_slice()));
      let retired = ask(
        state,
        &owner(),
        RequestBody::RetireBranch {
          repository: "linux".to_owned(),
          name: "topic".to_owned(),
        },
      );
      assert!(matches!(retired, ReplyBody::Acknowledged), "{retired:?}");
      assert_eq!(file(state, at_one).as_deref(), Some(b"ONE".as_slice()));
      let bound = ask(
        state,
        &owner(),
        RequestBody::BindBranch {
          repository: "linux".to_owned(),
          name: "topic".to_owned(),
          green: at_one,
        },
      );
      assert!(matches!(bound, ReplyBody::Acknowledged), "{bound:?}");
      assert!(matches!(
        refusal(create(state, "linux", "main")),
        Refusal::AlreadyExists { .. }
      ));
      assert_eq!(
        refusal(branch(state, ("linux", "x", "nope"), None)),
        Refusal::NotFound
      );
      assert!(matches!(
        refusal(branch(state, ("linux", "x", "main"), Some(9))),
        Refusal::UnknownBase { version: 9, .. }
      ));
      assert_eq!(
        refusal(branch(state, ("linux", "bad\0name", "main"), None)),
        Refusal::InvalidName
      );
      let foreign = ask(
        state,
        &stranger(),
        RequestBody::CreateBranch {
          repository: "linux".to_owned(),
          name: "x".to_owned(),
          from: "main".to_owned(),
          version: None,
        },
      );
      assert!(matches!(refusal(foreign), Refusal::Forbidden { .. }));
      let lone = green_of(ask(
        state,
        &owner(),
        RequestBody::CreateGreen {
          name: "lone".to_owned(),
          require_evidence: false,
          base: None,
        },
      ));
      let outside = ask(
        state,
        &owner(),
        RequestBody::BindBranch {
          repository: "linux".to_owned(),
          name: "lone".to_owned(),
          green: lone,
        },
      );
      assert_eq!(refusal(outside), Refusal::NotFound);
      assert_eq!(state.greens.busy_lends(), 0);
    });
  }

  /// A-603 (§4.17.1 shared piece 2: every change of a repository is one entry of its register). Do: create a repository,
  /// write its root twice, branch it, retire the branch's name and bind it again, write the branch; then write a green
  /// no repository holds, and send a bind the register refuses. Expect: the create is one entry, and each verb and each
  /// member's version takes exactly one position more, its chain entry recorded under the green; the lone green's
  /// version and the refused bind take none.
  #[test]
  fn every_verb_and_every_members_version_is_one_entry_of_the_register() {
    crate::daemon::audit_on_shard(|state| {
      let root = green_of(create(state, "tree", "main"));
      let repo = repository(state, "tree");
      let position =
        |state: &ShardState| state.db.partition().repositories().position(repo);
      assert_eq!(position(state), Some(1), "the create is one entry");
      write(state, root, b"v1", 1);
      write(state, root, b"v2", 2);
      assert_eq!(position(state), Some(3));
      assert_eq!(state.db.partition().green_head(to_db_volume(root)), 2);
      let topic = green_of(branch(state, ("tree", "topic", "main"), Some(1)));
      assert_eq!(position(state), Some(4));
      let retired = ask(
        state,
        &owner(),
        RequestBody::RetireBranch {
          repository: "tree".to_owned(),
          name: "topic".to_owned(),
        },
      );
      assert!(matches!(retired, ReplyBody::Acknowledged), "{retired:?}");
      let bound = ask(
        state,
        &owner(),
        RequestBody::BindBranch {
          repository: "tree".to_owned(),
          name: "topic".to_owned(),
          green: topic,
        },
      );
      assert!(matches!(bound, ReplyBody::Acknowledged), "{bound:?}");
      assert_eq!(position(state), Some(6));
      write(state, topic, b"t1", 3);
      assert_eq!(position(state), Some(7));
      assert_eq!(state.db.partition().green_head(to_db_volume(topic)), 1);
      let lone = green_of(ask(
        state,
        &owner(),
        RequestBody::CreateGreen {
          name: "lone".to_owned(),
          require_evidence: false,
          base: None,
        },
      ));
      write(state, lone, b"l1", 4);
      assert_eq!(state.db.partition().green_head(to_db_volume(lone)), 1);
      let taken = ask(
        state,
        &owner(),
        RequestBody::BindBranch {
          repository: "tree".to_owned(),
          name: "main".to_owned(),
          green: topic,
        },
      );
      assert!(matches!(refusal(taken), Refusal::AlreadyExists { .. }));
      assert_eq!(position(state), Some(7));
    });
  }

  /// A-575 (§4.16 the retire sliced; §4.17.1 item 4's destroy). Do: branch a repository's root, write the branch, then
  /// destroy the root, then the branch, and give the branch's holds back one unit at a time. Expect: the root refuses its
  /// destroy `Pinned` while the branch lives; the branch's destroy answers at once with its engine gone and its release
  /// owed; no unit frees more than one node; once the release is done the lineage holds what the root alone held.
  #[test]
  fn a_destroyed_branch_is_released_a_node_a_unit_and_its_source_is_pinned_until_then() {
    crate::daemon::audit_on_shard(|state| {
      let root = green_of(create(state, "kernel", "main"));
      write(state, root, b"root bytes", 1);
      let repo = repository(state, "kernel");
      let alone = state.greens.lineage_nodes(repo).expect("its lineage");
      let topic = green_of(branch(state, ("kernel", "topic", "main"), None));
      write(state, topic, b"topic bytes", 2);
      let pinned = ask(state, &owner(), RequestBody::Destroy { volume: root });
      assert_eq!(
        refusal(pinned),
        Refusal::Volume {
          refusal: VolumeRefusal::Pinned
        }
      );
      let destroyed = ask(state, &owner(), RequestBody::Destroy { volume: topic });
      assert!(matches!(destroyed, ReplyBody::Destroyed), "{destroyed:?}");
      assert!(!state.greens.contains_key(&to_db_volume(topic)));
      assert_eq!(state.greens.releases_owed(), 1);
      loop {
        let before = state.greens.lineage_nodes(repo).expect("its lineage");
        let progress = state.greens.release_unit();
        let after = state.greens.lineage_nodes(repo).expect("its lineage");
        assert!(before - after <= 1, "a unit freed more than one node");
        if progress == ReleaseProgress::Done {
          break;
        }
      }
      assert_eq!(state.greens.lineage_nodes(repo), Some(alone));
      assert_eq!(file(state, root).as_deref(), Some(b"root bytes".as_slice()));
    });
  }

  /// Every green of the shard rebuilt as a recovery rebuilds it, in recovery's order, into empty engines: what the shard
  /// held before, to compare with.
  fn recover(state: &mut ShardState) -> crate::greens::Greens {
    let live = std::mem::take(&mut state.greens);
    let mut records: Vec<slates_db::catalog::VolumeRecord> = state
      .db
      .partition()
      .volumes()
      .into_iter()
      .filter(|record| matches!(record.policy.role, slates_db::catalog::Role::Green { .. }))
      .cloned()
      .collect();
    records.sort_by_cached_key(|record| crate::verbs::member_rank(state, record.id));
    for record in &records {
      crate::verbs::rebuild_member(state, record);
    }
    live
  }

  /// A green's head, its identity and its file at each version from a floor.
  type Answers = (u64, [u8; 32], Vec<Option<Vec<u8>>>);

  /// What `green` answers from `floor` up ([`Answers`]).
  fn answers(greens: &mut crate::greens::Greens, green: VolumeId, floor: u64) -> Option<Answers> {
    let mut engine = greens.get_mut(&to_db_volume(green))?;
    let files = (floor..=engine.head())
      .map(|version| engine.content_at("f", version))
      .collect();
    Some((engine.head(), engine.head_identity(), files))
  }

  /// The oldest version both engines of `green` reconstruct: a rebuilt one begins at its checkpoint.
  fn common_floor(
    first: &crate::greens::Greens,
    second: &crate::greens::Greens,
    green: VolumeId,
  ) -> u64 {
    [first, second]
      .iter()
      .filter_map(|greens| {
        greens
          .get(&to_db_volume(green))
          .map(|engine| engine.folded_below().max(engine.mappable_from()))
      })
      .max()
      .unwrap_or(0)
  }

  /// A-575 (§4.17.1 item 4, a branch rebuilt the way it joined). Do: branch a repository's root at version 1, write it,
  /// branch the branch, retire a name, write the root on past version 1 and force its checkpoint, then rebuild every
  /// green as a recovery does; then corrupt the root's chain and rebuild again. Expect: the root's checkpoint stays at or
  /// below version 1, the version a live branch was made at; every member reads after the rebuild as before it (head,
  /// identity, every version); and with the root's chain corrupt, the root and every member made from it are fenced,
  /// none served.
  #[test]
  fn a_recovery_rebuilds_every_member_the_way_it_joined_and_fences_what_its_source_cannot_give() {
    crate::daemon::audit_on_shard(|state| {
      let root = green_of(create(state, "tree", "main"));
      write(state, root, b"v1", 1);
      let topic = green_of(branch(state, ("tree", "topic", "main"), Some(1)));
      write(state, topic, b"topic", 2);
      let deep = green_of(branch(state, ("tree", "deep", "topic"), None));
      write(state, deep, b"deep", 3);
      let retired = ask(
        state,
        &owner(),
        RequestBody::RetireBranch {
          repository: "tree".to_owned(),
          name: "deep".to_owned(),
        },
      );
      assert!(matches!(retired, ReplyBody::Acknowledged), "{retired:?}");
      for serial in 4..10 {
        write(state, root, format!("v{serial}").as_bytes(), serial);
      }
      let root_id = to_db_volume(root);
      crate::merge_service::checkpoint_green_when(
        state,
        root_id,
        crate::merge_service::Doubling::Now,
      );
      assert!(
        state.db.partition().green_floor(root_id) <= 1,
        "the root's checkpoint passed the version its branch was made at"
      );
      let mut live = recover(state);
      for green in [root, topic, deep] {
        let floor = common_floor(&state.greens, &live, green);
        let rebuilt = answers(&mut state.greens, green, floor);
        assert!(rebuilt.is_some(), "{green:?} not rebuilt");
        assert_eq!(
          rebuilt,
          answers(&mut live, green, floor),
          "{green:?} after the rebuild"
        );
      }
      let corrupt = slates_db::op::Op::GreenAdvanced {
        green: root_id,
        increment: vec![0xff; 16],
      };
      state.db.mutate(&mut state.segment, &corrupt, 0).unwrap();
      recover(state);
      for green in [root, topic, deep] {
        let id = to_db_volume(green);
        assert!(!state.greens.contains_key(&id), "{green:?} served");
        assert!(
          state.fenced_greens.contains_key(&id),
          "{green:?} not fenced"
        );
      }
    });
  }
}
