//! The repository verbs (§4.17.1 "The repository and its greens", items 1 to 5; A-515's records, A-575's verbs): a
//! repository's greens are members of one lineage on the repository's partition ([`crate::greens`]), so a branch made at
//! another branch's version shares every node of that version's state and costs what changed after it.
//!
//! **The verbs.** `CreateRepository` records the repository and its lineage's root, a new empty green or an unbound
//! green of the partition that joins as it stands, bound to the default branch. `CreateBranch` makes a member at a
//! version of a branch (its head when none) through `Lineage::branch` and binds the new name to it. `BindBranch` binds a
//! name to a member no name binds, which restores a retired branch; `RetireBranch` lets a name go and keeps its green
//! (item 5). Destroying a member is the green's own destroy verb: it retires the member from its lineage at once, gives
//! back its map of paths to git's files (A-664 part 3, `crate::vcs_store::paths`), and queues what only it held, which
//! [`run_releases`] gives back a node a unit under the shard's slice.
//!
//! **Rights.** A repository's owner holds every right on it (§4.13) until §4.17.1's policy (shared piece 7) says
//! otherwise, so the branch verbs are its owner's, refused `Forbidden` to another principal; a green that becomes a root
//! is one its creator administers. The policy itself is its owner's to set (`SetRepositoryPolicy`, A-622): whole, from
//! the version its owner read, held in the repository's record.
//!
//! **Bounds and refusals.** Every refusal is typed: a repository or branch that does not exist `NotFound`; a name taken,
//! a green bound or a member twice `AlreadyExists`; an empty name or one holding a NUL `InvalidName`, and so is a
//! member's volume name longer than a listing holds; a version past the source's head, or below the oldest version its
//! recovery can still branch from, `UnknownBase`; a fenced source `ContentUnavailable`; the partition's tables at their
//! derived capacity `TableAtCapacity`.
//!
//! **The logs (A-603, rebuilt by A-642).** Every change a verb makes of a repository's namespace is one entry
//! (`slates_db::repository`): a repository's root joining and its default branch's binding, a branch's member joining
//! and its name's binding, a bind, a retire, each moving the names log alone; a member's version is an append to its
//! own green's log (`Op::GreenAdvanced`), never a position of the whole repository; and a push of several is one entry,
//! all or none (`push`).
//!
//! **Durability.** Each verb's records commit as one atom with its completion record (the verb's transaction), the
//! engine installed only after them: a refused record leaves no engine ahead of its log, and a rolled-back one is undone
//! by `verbs::reconcile_unpublished_effects`. A member is rebuilt at recovery the way it joined, in the order it joined
//! (`verbs::rebuild_member_sliced`); the oldest version a live branch was made at is the floor its source's checkpoint
//! may not pass (`merge_service::reachable_floor`), so that version is there to branch from again.
//!
//! **Names.** A member's volume name is `<repository>:<branch>:<id>`: readable in a listing, unique on its partition by
//! its id.
//!
//! **The store.** `CreateRepository` makes the repository's tool store beside its record, on this owner shard
//! (`crate::vcs_store`, A-662). A Subversion repository's first entry also holds its creation's record (A-655 part 2b:
//! its UUID and revision 0's date, `slates_svn::commit::record`), from which its store's records are made, as a
//! recovery makes them again.
//!
//! **A creator's defaults (A-685).** What a client names when it creates a repository is decided here, beside the verb,
//! so every surface creates alike (`slates repository create`): the formats each tool names its objects in
//! ([`object_formats`]) and the namespace name its root is bound to ([`root_binding`]): git's branch by its ref under
//! `refs/heads/`, Mercurial's branch by its first line, Subversion's one name.

use slates_db::catalog::{
  BaseRecord, MemberOrigin, MemberRecord, NameChange, NamePolicy as DbNamePolicy, PolicyRecord,
  Principal, RepositoryEntry, RepositoryId, RepositoryPolicy, RepositoryRecord, Role,
  SnapshotId as DbSnapshotId, VolumeId as DbVolumeId, VolumeRecord, VolumeState,
};
use slates_db::op::Op;
use slates_ipc::protocol::{ObjectFormat, Refusal, ReplyBody, VcsTool, VolumeId};
use slates_vfs::clock::Clock;

use crate::error::refusal_of_db;
use crate::greens::ReleaseProgress;
use crate::state::ShardState;
use crate::verbs::{forbidden, refused, to_db_volume, to_wire_volume};

// The push's callers, git's receive-pack and Mercurial's unbundle, land after it (A-620's second part; §4.17.3's push):
// until one calls it, a daemon build calls none of it.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod push;
// Mercurial's unbundle landed through the push (A-657): until the hg site serves unbundle (A-650's), a daemon build
// calls none of it.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod hg_push;
// A Mercurial push landed as a run of segment changesets under its repository's landing turn (main's ruling (b1)).
pub(crate) mod hg_run;
// Subversion's commit landed through the push (A-655 part 2b): a dump's import commits through it (A-652 part 3); until
// the svn:// connection task (A-656 part 2) calls it, a daemon build calls none of a client's half (the judging again).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod svn_commit;
// The import verb (A-678): a Mercurial repository's directory imported into the repository's store through
// `vcs_store::hg_import`, the request answered when the import ends.
pub(crate) mod import;
// The export verb (A-652 part 3): a Subversion repository's range of revisions written as a dump into a new volume.
pub(crate) mod svn_export;

/// Format: where RFC 4122's form puts a `-` among a UUID's sixteen bytes, written as two hexadecimal digits each:
/// before bytes 4, 6, 8 and 10 (8-4-4-4-12 digits).
const UUID_DASHES_BEFORE: [usize; 4] = [4, 6, 8, 10];

/// The UUID a Subversion repository is made with (A-655 part 2b): its id's sixteen bytes in RFC 4122's form, in
/// lower-case hexadecimal, so every holder and every recovery names it alike and no two repositories share one, their
/// ids differing (`verbs::fresh_family_id_at`).
fn svn_uuid(repository: RepositoryId) -> Option<slates_svn::dump::Uuid> {
  let mut text = String::new();
  for (at, byte) in repository.bytes.iter().enumerate() {
    if UUID_DASHES_BEFORE.contains(&at) {
      text.push('-');
    }
    text.push_str(&format!("{byte:02x}"));
  }
  slates_svn::dump::Uuid::parse(text.as_bytes())
}

/// The record a Subversion repository is made with (A-655 part 2b; `slates_svn::commit::record`): its UUID
/// ([`svn_uuid`]) and revision 0's `svn:date`, the owner's instant as `svn_time_to_cstring` writes it, as `svnadmin
/// create` dates revision 0. Refused `Unsupported` for an instant that writer cannot write (before 1970 or past the
/// year 9999), which no owner's clock reads.
fn svn_created(state: &mut ShardState, repository: RepositoryId) -> Result<Vec<u8>, Refusal> {
  let unwritable = || Refusal::Unsupported {
    feature: "a Subversion repository dated before 1970 or past the year 9999".to_owned(),
  };
  let uuid = svn_uuid(repository).ok_or_else(unwritable)?;
  let now = slates_svn::lock::AprTime::from_wall_ns(state.clock.wall_ns());
  let date = slates_svn::dav::response::svn_time(now).map_err(|_| unwritable())?;
  let record = slates_svn::commit::record::Record::Created {
    uuid,
    date: date.into_bytes(),
  };
  Ok(record.encode())
}

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

/// The formats `tool` names its objects in, its own default first (§4.17.1 item 1: "its object format (git's SHA-1 or
/// SHA-256, Mercurial's node format, Subversion's revision numbers)"; A-685): git's SHA-1, the format git takes when
/// none is named (A-600), or SHA-256; Mercurial's SHA-1 node ids alone; Subversion's revision numbers alone. What a
/// repository of the tool may be created with (`slates repository create --format`).
pub fn object_formats(tool: VcsTool) -> &'static [ObjectFormat] {
  match tool {
    VcsTool::Git => &[ObjectFormat::Sha1, ObjectFormat::Sha256],
    VcsTool::Mercurial => &[ObjectFormat::Sha1],
    VcsTool::Subversion => &[ObjectFormat::Revisions],
  }
}

/// Format: the git branch a repository's root is bound to when its creator names none (A-685): `main`, the branch this
/// server's push tests and its captures of stock git 2.50.1 bind (`git_push/tests.rs`, `crates/git/fixtures/v0`). A
/// stock clone of the empty repository takes it from the unborn `HEAD` the site advertises (`ls-refs=unborn`, §4.17.2
/// "The wire").
const GIT_DEFAULT_BRANCH: &str = "main";

/// Format: the Mercurial branch whose first line a repository's root is when its creator names none (A-685): `default`,
/// the branch Mercurial puts a changeset on when none is named, whose first line a new repository's first push takes
/// (`hg_push`'s module docs: "a Mercurial repository's first line, the green its verb makes, is `branch:default:0`").
const HG_DEFAULT_BRANCH: &str = "default";

/// Format: the number of a Mercurial branch's first line (`hg_push`'s namespace: a line a push makes takes the lowest
/// number its branch does not use, so the first is this).
const HG_FIRST_LINE: u32 = 0;

/// Format: the one name a Subversion repository's namespace binds (A-685): its one green holds the repository from its
/// root, its branches directories in it (§4.17.4), so the name says that.
const SVN_ROOT: &str = "root";

/// Why a branch cannot be a new repository's default branch ([`root_binding`]; A-685).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootRefused {
  /// git's: no branch's name. Branch `N` is the ref `refs/heads/N`, which git's rules must take (`git
  /// check-ref-format`), and a name beginning `refs/` is a ref of its own in slates' namespace, never a branch
  /// (`git_push`'s module docs).
  NotAGitBranch,
  /// Subversion's: its repository is one green, its branches directories in it (§4.17.4), so it binds no branch.
  NoSubversionBranch,
}

impl core::fmt::Display for RootRefused {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    f.write_str(match self {
      Self::NotAGitBranch => "not a git branch's name (as `git init --initial-branch` takes one)",
      Self::NoSubversionBranch => "Subversion's branches are directories in its one green",
    })
  }
}

/// The namespace name a new repository's root is bound to, `CreateRepository`'s `default_branch` (§4.17.1 item 1;
/// A-685), from the branch its creator names in the tool's own terms, else the tool's default: git's branch `N` is the
/// name `N` (the ref `refs/heads/N`), `GIT_DEFAULT_BRANCH` when none is named; Mercurial's branch `B` is its first
/// line, the green the verb makes (`hg_push`'s namespace), `HG_DEFAULT_BRANCH`'s when none is named; Subversion's is
/// `SVN_ROOT`, and a branch named for it is refused.
pub fn root_binding(tool: VcsTool, branch: Option<&str>) -> Result<String, RootRefused> {
  match (tool, branch) {
    (VcsTool::Git, _) => git_branch(branch.unwrap_or(GIT_DEFAULT_BRANCH)),
    (VcsTool::Mercurial, _) => Ok(hg_push::line_name(
      branch.unwrap_or(HG_DEFAULT_BRANCH).as_bytes(),
      HG_FIRST_LINE,
    )),
    (VcsTool::Subversion, None) => Ok(SVN_ROOT.to_owned()),
    (VcsTool::Subversion, Some(_)) => Err(RootRefused::NoSubversionBranch),
  }
}

/// git's branch `name` as its namespace name: refused unless slates' namespace maps `name` to the ref `refs/heads/name`
/// and git's rules take that ref (`git_push::ref_of`).
fn git_branch(name: &str) -> Result<String, RootRefused> {
  match crate::git_push::ref_of(name) {
    Some(full) if full.starts_with(crate::git_push::HEADS.as_bytes()) => Ok(name.to_owned()),
    _ => Err(RootRefused::NotAGitBranch),
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
  // A change of the repository's own state logs is admitted only while the parts this shard owes their holders are
  // within their bound (A-636 part 2b).
  if crate::state_log::moves_state_logs(op) {
    crate::state_log::admit(state)?;
  }
  let now = state.clock.monotonic_ns();
  state
    .db
    .mutate(&mut state.segment, op, now)
    .map(|_| ())
    .map_err(|error| refusal_of_db(&error))
}

/// An entry of `repository`'s logs, changing nothing yet (A-603, A-642): `NotFound` for no such repository.
fn next_entry(state: &ShardState, repository: RepositoryId) -> Result<RepositoryEntry, Refusal> {
  state
    .db
    .partition()
    .repositories()
    .repository(repository)
    .ok_or(Refusal::NotFound)?;
  Ok(RepositoryEntry {
    repository,
    ..RepositoryEntry::default()
  })
}

/// The repository named `name` on this partition, when `principal` owns it: `NotFound` for none, `Forbidden` for
/// another principal's (module docs, "Rights"). The refusal is boxed: a reply is far larger than a record.
fn owned_repository(
  state: &ShardState,
  principal: &Principal,
  name: &str,
  verb: &str,
) -> Result<RepositoryRecord, Box<ReplyBody>> {
  let record = state
    .db
    .partition()
    .repositories()
    .repository_named(name)
    .cloned()
    .ok_or_else(|| Box::new(refused(Refusal::NotFound)))?;
  if &record.owner != principal {
    return Err(Box::new(forbidden(verb)));
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
/// in a new lineage. Answered `GreenCreated` with the root. With the node's changeset store, in the one landing order
/// (A-767 item 7): the creation landed as changeset 0 first, then recorded, then answered ([`land_then_record`]).
pub(crate) fn create_repository(
  state: &mut ShardState,
  principal: &Principal,
  asked: (&str, VcsTool, ObjectFormat, &str),
  green: Option<VolumeId>,
) -> ReplyBody {
  if let Some(refusal) = no_store_for(state, asked.1) {
    return refused(refusal);
  }
  let adopted = match green.map(|volume| adoptable(state, principal, volume)) {
    Some(Ok(id)) => Some(id),
    Some(Err(refusal)) => return refused(refusal),
    None => None,
  };
  let ids = match creation_ids(state, adopted) {
    Ok(ids) => ids,
    Err(refusal) => return refused(refusal),
  };
  if let Some(disk) = state.vcs_disk.clone() {
    return land_then_record(state, (disk, principal), asked, (adopted, ids));
  }
  record_created(state, principal, asked, (adopted, ids), Vec::new())
}

/// Format: the refusal's words for a Mercurial repository a daemon without a changeset store cannot keep (C1; main's
/// ruling of 2026-11-20, inbox w-svn-from-main-c2-rulings decision 1).
const NEEDS_A_STORE: &str = "a Mercurial repository's history is kept on the node's changeset store: start the daemon \
                             with --vcs-store DIR";

/// Format: the refusal's words for a git repository a daemon without a changeset store cannot keep (C5; the same
/// ruling: one rule for all three tools).
const GIT_NEEDS_A_STORE: &str = "a git repository's history is kept on the node's changeset store: start the daemon \
                                 with --vcs-store DIR";

/// A repository a daemon without a changeset store refuses (main's ruling of 2026-11-20, decision 1): a Mercurial or a
/// Subversion repository's history is the node's store (C1, C2), and a register-held copy for a daemon without one
/// would be a second path (CLAUDE.md §2 item 7). Typed `Unsupported`, naming `--vcs-store`, for all three tools (git's
/// since C5: its pushes land on the store, A-767).
fn no_store_for(state: &ShardState, tool: VcsTool) -> Option<Refusal> {
  if state.vcs_disk.is_some() {
    return None;
  }
  match tool {
    VcsTool::Subversion => Some(unstored_subversion()),
    VcsTool::Mercurial => Some(Refusal::Unsupported {
      feature: NEEDS_A_STORE.to_owned(),
    }),
    VcsTool::Git => Some(Refusal::Unsupported {
      feature: GIT_NEEDS_A_STORE.to_owned(),
    }),
  }
}

/// The refusal of a Subversion repository on a daemon started without `--vcs-store` (C2; main's ruling of 2026-11-20
/// 21:05): its history has no store to land on.
pub(crate) fn unstored_subversion() -> Refusal {
  Refusal::Unsupported {
    feature: "a Subversion repository on a daemon started without --vcs-store DIR, the store its history lands on"
      .to_owned(),
  }
}

/// A creation's ids: the repository's, and its new root's (the number after it, in its family) unless a green is
/// adopted.
fn creation_ids(
  state: &mut ShardState,
  adopted: Option<DbVolumeId>,
) -> Result<(RepositoryId, Option<DbVolumeId>), Refusal> {
  let repository = repository_id(state, adopted)?;
  let root = match adopted {
    Some(_) => None,
    None => Some(
      crate::verbs::fresh_family_id_at(state, crate::verbs::family_of(repository.bytes), 1)
        .ok_or_else(crate::verbs::family_ids_exhausted)?,
    ),
  };
  Ok((repository, root))
}

/// Records a creation in the partition and installs its root: `GreenCreated`, or the refusal. A Subversion
/// repository's creation record `svn_created` (its UUID and revision 0's date) is its store's changeset 0's, landed
/// already, which its register's entry names by reference and its records are made from (C2); empty for another tool.
fn record_created(
  state: &mut ShardState,
  principal: &Principal,
  (name, tool, format, default_branch): (&str, VcsTool, ObjectFormat, &str),
  (adopted, (repository, minted)): (Option<DbVolumeId>, (RepositoryId, Option<DbVolumeId>)),
  svn_created: Vec<u8>,
) -> ReplyBody {
  match record_repository(
    state,
    principal,
    (repository, name, tool, format, default_branch),
    (adopted, minted),
    !svn_created.is_empty(),
  ) {
    Ok(root) => {
      install_root(state, repository, root, (adopted.is_some(), &svn_created));
      ReplyBody::GreenCreated {
        id: to_wire_volume(root),
      }
    }
    Err(refusal) => refused(refusal),
  }
}

/// Counter: creations landed into the node's version-control store (A-767).
pub(crate) const CREATION_LANDED: &str = "repository.create.landed";
/// Counter: creations the store refused (its owner busy or gone, or its write failed): the repository stands in its
/// partition, and its store holds no creation for it.
pub(crate) const CREATION_UNLANDED: &str = "repository.create.unlanded";
/// Counter: creations whose landing task a test ended between its two steps ([`KILL_BETWEEN_LANDING_AND_RECORD`]).
#[cfg(test)]
pub(crate) const CREATION_KILLED: &str = "repository.create.killed";

#[cfg(test)]
thread_local! {
  /// A test's kill between a creation's landing and its record: the landing task ends there, nothing after it run, as a
  /// `kill -9` leaves the register (main's red test of 2026-11-20).
  pub(crate) static KILL_BETWEEN_LANDING_AND_RECORD: std::cell::Cell<bool> =
    const { std::cell::Cell::new(false) };
}

/// Counter: creations whose landing task could not be spawned (the shard's arena full).
pub(crate) const CREATION_UNRUN: &str = "repository.create.unrun";

/// Lands a new repository's creation into the node's version-control store, then records it (A-767 item 7, the one
/// landing order; main's order of 2026-11-20 after w-svn's finding): one changeset, sequence 0, whose record names the
/// repository as the client asked for it, under the node's store grant; once it is durable, the repository is recorded
/// in its partition, its root installed and its completion recorded in one transaction, and the client answered. A
/// kill between the two leaves a changeset no repository names, which recovery passes over, and no repository the
/// store cannot recover; the client's retry creates it afresh. The repository's record is judged before the landing (a
/// name already taken lands nothing), and judged again when it is recorded, since another creation may have taken the
/// name meanwhile. Its ids are reserved now (`ShardState::family_reserved_through`). The verb defers its acceptance
/// (AUD-11) and the client is answered by its reply route once the creation is recorded. Every creation has one: a
/// repository's verbs are served on the node the client reached, on the partition its name hashes to (A-575;
/// `RequestBody::volume` names no volume for them, so `verbs::serve_forward` never carries one to another node), and a
/// same-node forward between shards carries its route; so a fleet answers a creation as a laptop does (R8). A dispatch
/// with no request or no route (a shard's own call) is refused `Unsupported`, typed, on every node alike.
fn land_then_record(
  state: &mut ShardState,
  (disk, principal): (slates_store::owner::StoreHandle, &Principal),
  (name, tool, format, default_branch): (&str, VcsTool, ObjectFormat, &str),
  (adopted, (repository, minted)): (Option<DbVolumeId>, (RepositoryId, Option<DbVolumeId>)),
) -> ReplyBody {
  let Some((request, route)) = state.current_request.zip(state.reply_route) else {
    return refused(Refusal::Unsupported {
      feature:
        "a repository created with no client request to answer once it is landed (A-767 item 7)"
          .to_owned(),
    });
  };
  let (header, parts, svn_created) = match creation_changeset(
    state,
    (&disk, principal),
    (name, tool, format, default_branch),
    (repository, minted),
  ) {
    Ok(changeset) => changeset,
    Err(reply) => return *reply,
  };
  let principal = principal.clone();
  let asked = (name.to_owned(), default_branch.to_owned());
  let task = async move {
    let landed = disk.land(header, parts).await;
    #[cfg(test)]
    if KILL_BETWEEN_LANDING_AND_RECORD.with(std::cell::Cell::get) {
      crate::state::with_state(|state| state.count(CREATION_KILLED, 1));
      return;
    }
    let answered = crate::state::with_state_counted(move |state| {
      state.count(
        if landed.is_ok() {
          CREATION_LANDED
        } else {
          CREATION_UNLANDED
        },
        1,
      );
      let landed = landed.map(drop).map_err(|error| {
        // The store's reason is logged and answered (goal 93; jupiter's red: an owner that could not open its store
        // was answered `Overloaded` and logged nothing).
        slates_machine::log_line!(
          "slates-server: partition {}: repository {}'s creation was not landed: {error}",
          state.partition,
          asked.0
        );
        refusal_of_owner(state, &error)
      });
      let reply = recorded_after_landing(
        state,
        &principal,
        (&asked.0, tool, format, &asked.1),
        (adopted, (repository, minted)),
        (landed, Some(request), svn_created),
      );
      (reply, state.shard)
    });
    if let Some((reply, here)) = answered {
      crate::told::deliver_to(route, reply, here);
    }
  };
  match slates_rt::futures::spawn(task).and_then(slates_rt::futures::detach) {
    Ok(()) => {
      state.reply_route = None;
      state.acceptance_deferred = true;
      ReplyBody::Acknowledged
    }
    Err(_) => {
      state.count(CREATION_UNRUN, 1);
      refused(Refusal::Overloaded { shard: state.shard })
    }
  }
}

/// A new repository's creation changeset (A-767 item 7): its record judged (a name already taken lands nothing, its
/// refusal the answer), its ids reserved (`ShardState::family_reserved_through`), and the changeset's header (sequence
/// 0, the store's standing grant) and sections: the store's own `Record` ([`creation_record`]) and, for a Subversion
/// repository, its creation record after it (C2), which is given back too, since the repository is recorded from it.
fn creation_changeset(
  state: &mut ShardState,
  (disk, principal): (&slates_store::owner::StoreHandle, &Principal),
  (name, tool, format, default_branch): (&str, VcsTool, ObjectFormat, &str),
  (repository, minted): (RepositoryId, Option<DbVolumeId>),
) -> Result<
  (
    slates_store::log::Header,
    Vec<slates_store::owner::Part>,
    Vec<u8>,
  ),
  Box<ReplyBody>,
> {
  let judged = admits(
    state,
    &Op::RepositoryCreated {
      record: RepositoryRecord {
        index_bits: 0,
        id: repository,
        name: name.to_owned(),
        tool: db_tool(tool),
        format: db_format(format),
        default_branch: Some(default_branch.to_owned()),
        owner: principal.clone(),
        policy: RepositoryPolicy::default(),
      },
    },
  );
  if let Err(refusal) = judged {
    return Err(Box::new(refused(refusal)));
  }
  // The repository's number and its root's, so nothing minted while the landing runs takes either.
  let reserved = minted.unwrap_or(DbVolumeId {
    bytes: repository.bytes,
  });
  if let Some(number) = slates_db::register::ObjectId(reserved.bytes).family_number() {
    state.family_reserved_through = state
      .family_reserved_through
      .max(u64::from(number).saturating_add(1));
  }
  let header = slates_store::log::Header {
    tool: match tool {
      VcsTool::Git => slates_store::log::Tool::Git,
      VcsTool::Mercurial => slates_store::log::Tool::Mercurial,
      VcsTool::Subversion => slates_store::log::Tool::Subversion,
    },
    repository: u128::from_be_bytes(repository.bytes),
    sequence: 0,
    judged_over: 0,
    // The store's standing grant (its `G` row, R10): the landing's provenance.
    grant: disk.grant(),
    work: 0,
    time_ns: slates_rt::futures::now_ns(),
    sections: 0,
  };
  let record = creation_record(name, tool, format, default_branch);
  // A Subversion repository's creation record (its UUID and revision 0's date) is its history's first: landed in
  // changeset 0 after the store's own record, read back by a recovery as the changeset's last `Record` section
  // (`svn_commit::landing::changeset_record`), and the records made from it when the repository is recorded (C2).
  let svn_created = match tool {
    VcsTool::Subversion => match svn_created(state, repository) {
      Ok(created) => created,
      Err(refusal) => return Err(Box::new(refused(refusal))),
    },
    VcsTool::Git | VcsTool::Mercurial => Vec::new(),
  };
  let mut parts = vec![(slates_store::log::SectionKind::Record, record)];
  if !svn_created.is_empty() {
    parts.push((slates_store::log::SectionKind::Record, svn_created.clone()));
  }
  Ok((header, parts, svn_created))
}

/// The refusal a creation the store did not land answers (goal 93): a full owner queue is the shard's `Overloaded`, to
/// retry; anything else is the store's own reason, `StoreRefused` (an owner gone, a store that could not be opened, a
/// refused write). Until jupiter's red of 2026-11-21 every one was `Overloaded`, which hid the store's reason.
fn refusal_of_owner(state: &ShardState, error: &slates_store::owner::OwnerError) -> Refusal {
  match error {
    slates_store::owner::OwnerError::Busy => Refusal::Overloaded { shard: state.shard },
    other => Refusal::StoreRefused {
      reason: other.to_string(),
    },
  }
}

/// A creation's landing as [`recorded_after_landing`] records it: what the store answered, the client's request (its origin
/// and id) where a client asked, and a Subversion repository's creation record.
type CreationLanded = (
  Result<(), Refusal>,
  Option<(u64, slates_wire::request::RequestId)>,
  Vec<u8>,
);

/// A landed creation recorded (A-767 item 7, the second step): in one transaction the repository, its root and, for a
/// client's `request`, its completion, a Subversion repository's from its creation record `svn_created` (C2); a
/// creation the store did not land answered its refusal ([`refusal_of_owner`]), nothing recorded of it.
fn recorded_after_landing(
  state: &mut ShardState,
  principal: &Principal,
  asked: (&str, VcsTool, ObjectFormat, &str),
  ids: (Option<DbVolumeId>, (RepositoryId, Option<DbVolumeId>)),
  (landed, request, svn_created): CreationLanded,
) -> ReplyBody {
  state.db.begin();
  let reply = match landed {
    Ok(()) => record_created(state, principal, asked, ids, svn_created),
    Err(refusal) => refused(refusal),
  };
  let reply = match request {
    Some((origin, id)) => crate::verbs::record_completion(state, origin, id, reply),
    None => reply,
  };
  match state.db.commit(&mut state.segment) {
    Ok(_) => reply,
    Err(error) => {
      crate::verbs::reconcile_unpublished_effects(state);
      refused(refusal_of_db(&error))
    }
  }
}

/// [`create_repository`] for a test's synchronous history on a shard of a node keeping a store (A-767 item 7's one
/// landing order): the creation's changeset landed and waited for on a thread of its own
/// (`crate::test_store::land_blocking`), then recorded, so the repository stands when this returns, as a client's
/// creation stands when it is answered.
#[cfg(test)]
pub(crate) fn create_blocking(
  state: &mut ShardState,
  (disk, principal): (slates_store::owner::StoreHandle, &Principal),
  asked: (&str, VcsTool, ObjectFormat, &str),
  green: Option<VolumeId>,
) -> ReplyBody {
  let adopted = match green.map(|volume| adoptable(state, principal, volume)) {
    Some(Ok(id)) => Some(id),
    Some(Err(refusal)) => return refused(refusal),
    None => None,
  };
  let ids = match creation_ids(state, adopted) {
    Ok(ids) => ids,
    Err(refusal) => return refused(refusal),
  };
  let (header, parts, svn_created) = match creation_changeset(state, (&disk, principal), asked, ids)
  {
    Ok(changeset) => changeset,
    Err(reply) => return *reply,
  };
  let landed = crate::test_store::land_answered_blocking(disk, header, parts)
    .map(drop)
    .map_err(|error| refusal_of_owner(state, &error));
  recorded_after_landing(
    state,
    principal,
    asked,
    (adopted, ids),
    (landed, None, svn_created),
  )
}

/// Format: a repository's creation record in its store (A-767): one line each of a marker, the name, the default
/// branch, the tool and the object format, as the client asked for them.
fn creation_record(
  name: &str,
  tool: VcsTool,
  format: ObjectFormat,
  default_branch: &str,
) -> Vec<u8> {
  format!("slates repository create\n{name}\n{default_branch}\n{tool:?}\n{format:?}\n").into_bytes()
}

/// A new repository's id (A-636 part 2b-2): of the adopted green's family, so the root joins it, or of a family of its
/// own. A green minted under no family (an older build's) is never adopted: its id would place it apart from the
/// repository's state logs, and taking it into a family would move every register it has (its chain, its catalog, its
/// holders' ledgers) to another id, which a client holding its id would lose; so it is refused `Unsupported`, and a
/// repository is made over a new root instead.
fn repository_id(
  state: &mut ShardState,
  adopted: Option<DbVolumeId>,
) -> Result<RepositoryId, Refusal> {
  let family = match adopted {
    Some(green) => {
      Some(
        crate::verbs::family_of(green.bytes).ok_or_else(|| Refusal::Unsupported {
          feature: "a repository adopting a green minted under no family (A-636 part 2b-2)"
            .to_owned(),
        })?,
      )
    }
    None => None,
  };
  let id = crate::verbs::fresh_family_id_at(state, family, 0)
    .ok_or_else(crate::verbs::family_ids_exhausted)?;
  Ok(RepositoryId { bytes: id.bytes })
}

/// The records of a new repository: its record, its root (a new green unless one is adopted), the root's membership
/// and the default branch's binding, which names a Subversion repository's creation changeset by reference where
/// `svn_landed` (C2). The root's id.
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
  (adopted, minted): (Option<DbVolumeId>, Option<DbVolumeId>),
  svn_landed: bool,
) -> Result<DbVolumeId, Refusal> {
  // A new root's record is judged first: once the repository's is recorded, a refusal would leave it rootless.
  let root_created = match adopted {
    Some(_) => None,
    None => {
      // In the repository's family, the number after the repository's own, minted with it (`creation_ids`).
      let id = minted.ok_or_else(crate::verbs::family_ids_exhausted)?;
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
        index_bits: 0,
        id: repository,
        name: name.to_owned(),
        tool: db_tool(tool),
        format: db_format(format),
        default_branch: Some(default_branch.to_owned()),
        owner: principal.clone(),
        policy: RepositoryPolicy::default(),
      },
    },
  )?;
  let root = match (adopted, root_created) {
    (Some(id), _) => id,
    (None, Some((id, root_created))) => {
      record(state, &root_created)?;
      id
    }
    (None, None) => return Err(Refusal::NotFound),
  };
  // The root joins and the default branch binds it in one entry of the new register (A-603), which names a Subversion
  // repository's creation's changeset, sequence 0, by reference (C2; A-655 part 2b).
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
  if svn_landed {
    entry.objects = crate::repository::svn_commit::landing::reference(0);
  }
  record(state, &Op::RepositoryAdvanced { entry })?;
  Ok(root)
}

/// The root's engine in a new lineage, once its records stand: the adopted green's own engine, moved in as it stands,
/// or a new empty one, whose version 0's merge record is enqueued as a new green's is.
fn install_root(
  state: &mut ShardState,
  repository: slates_db::catalog::RepositoryId,
  root: DbVolumeId,
  (adopted, svn_created): (bool, &[u8]),
) {
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
  // The repository's tool store, beside its record, on its owner shard (A-662), made from the records its entries hold,
  // as a recovery makes it; a Subversion repository's from its creation record in hand (A-655 part 2b), landed on the
  // store as changeset 0 (C2).
  if let Some(record) = state
    .db
    .partition()
    .repositories()
    .repository(repository)
    .cloned()
  {
    if record.tool == slates_db::catalog::VcsTool::Subversion {
      crate::vcs_store::make_created(state, &record, svn_created);
    } else {
      crate::vcs_store::make_from_log(state, &record);
    }
  }
}

/// Counter: a lineage the records named that could not be installed (its repository already had one): zero in every
/// correct use. Format: a counter name in the daemon's status report.
pub(crate) const LINEAGE_UNINSTALLED: &str = "repository.lineage_uninstalled";

/// The version of `source` a branch is made at, checked: its head when `version` is none; `UnknownBase` past the head
/// or below the oldest version the source reconstructs, in memory or at a recovery (its checkpoint's). git's push asks
/// it of the version a create is made at (A-654).
pub(crate) fn branch_version(
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

/// A branch's name as the owner's verbs give it, spelled as `record`'s namespace holds it: a Mercurial repository's
/// names its bookmarks (`hg_push::owner_name`), every other tool's as given.
fn spelled(record: &RepositoryRecord, name: &str) -> String {
  match record.tool {
    slates_db::catalog::VcsTool::Mercurial => hg_push::owner_name(name),
    _ => name.to_owned(),
  }
}

/// Whether `from` names a branch of `record` as Mercurial resolves a name ([`hg_source`]): a Mercurial repository's
/// name its namespace does not spell already.
pub(crate) fn named_as_mercurial_resolves(record: &RepositoryRecord, from: &str) -> bool {
  let spelled_already = hg_push::line_of(from).is_some() || hg_push::bookmark_of(from).is_some();
  record.tool == slates_db::catalog::VcsTool::Mercurial && !spelled_already
}

/// The green a branch named `from` is made from (A-575): a name the namespace spells, or any name of a repository of
/// a tool other than Mercurial, as given; a Mercurial repository's other names as Mercurial resolves them
/// ([`hg_source`]). `NotFound` for a name nothing binds.
fn source_of(
  state: &mut ShardState,
  record: &RepositoryRecord,
  from: &str,
) -> Result<DbVolumeId, Refusal> {
  if !named_as_mercurial_resolves(record, from) {
    return state
      .db
      .partition()
      .repositories()
      .branch(record.id, from)
      .ok_or(Refusal::NotFound);
  }
  hg_source(state, record.id, from)
}

/// The green a Mercurial repository's branch named `from` is made from (A-575; main's ruling of 2026-11-20: "follow
/// Mercurial's own name lookup, bookmark, then tag, then branch"). `from` is resolved over the served view as 7.2's
/// `lookup` resolves it (`slates_hg::serve::lookup::resolve_name`: a revision number or node first, then a bookmark,
/// a global tag, a branch's tip), so `default` names its branch's tip-most open head. The green is the one whose head
/// is that changeset: the bookmark `from` names where it heads it, else the line that does (each line is one head of a
/// branch), else any bookmark that does. Refused `NotFound` for a name Mercurial does not know; `ContentUnavailable`
/// where the view cannot be made here (an import filling it, a store this shard does not hold); and `Unsupported` for
/// a changeset no line or bookmark heads (a tag inside a line's history), since a green is branched at a version and a
/// changeset below a head names none yet (GAPS A-575).
fn hg_source(
  state: &mut ShardState,
  repository: slates_db::catalog::RepositoryId,
  from: &str,
) -> Result<DbVolumeId, Refusal> {
  let mut room = slates_hg::Admission::new(crate::vcs_share::room(state, repository).bytes);
  let resolved =
    crate::vcs_store::hg_view::resolved_in(state, repository, from.as_bytes(), &mut room)
      .ok_or(Refusal::ContentUnavailable)?
      .map_err(|_| Refusal::ContentUnavailable)?;
  let node = resolved.ok_or(Refusal::NotFound)?;
  let repositories = state.db.partition().repositories();
  let heads = |green: &DbVolumeId| crate::vcs_store::hg_made::head_of(state, *green) == Some(node);
  let bookmark = repositories.branch(repository, &hg_push::bookmark_name(from.as_bytes()));
  let line = || {
    repositories
      .branches(repository, hg_push::LINE_TAG)
      .map(|(_, green)| green)
      .find(heads)
  };
  let any_bookmark = || {
    repositories
      .branches(repository, hg_push::BOOKMARK_TAG)
      .map(|(_, green)| green)
      .find(heads)
  };
  bookmark
    .filter(heads)
    .or_else(line)
    .or_else(any_bookmark)
    .ok_or_else(|| Refusal::Unsupported {
      feature: format!(
        "a branch made at `{from}`, changeset {}, which no line or bookmark of the repository heads",
        hex(&node)
      ),
    })
}

/// A changeset's node in hexadecimal, as Mercurial writes it.
fn hex(node: &slates_hg::Node) -> String {
  let mut out = Vec::new();
  slates_hg::pushkey::encode_hex(node, &mut out);
  String::from_utf8_lossy(&out).into_owned()
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
    Err(reply) => return *reply,
  };
  if let Some(reply) = no_subversion_branch(&record) {
    return reply;
  }
  let name = spelled(&record, name);
  let source = match source_of(state, &record, from) {
    Ok(source) => source,
    Err(refusal) => return refused(refusal),
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
    (&record, &name),
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
  // Git's site serves every name the namespace binds (A-743): the new branch's ref, at the commit it was made at.
  crate::vcs_store::commits::branch_made(state, id);
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
  let id = crate::verbs::fresh_family_id_at(state, crate::verbs::family_of(repository.id.bytes), 0)
    .ok_or_else(crate::verbs::family_ids_exhausted)?;
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
    Err(reply) => return *reply,
  };
  if let Some(reply) = no_subversion_branch(&record) {
    return reply;
  }
  // One entry of the repository's register (A-603), whose guard binds a member of this lineage only.
  let entry = next_entry(state, record.id).map(|mut entry| {
    entry.names.push(NameChange::Bind {
      name: spelled(&record, name),
      green: to_db_volume(green),
    });
    entry
  });
  match entry.and_then(|entry| self::record(state, &Op::RepositoryAdvanced { entry })) {
    Ok(()) => ReplyBody::Acknowledged,
    Err(refusal) => refused(refusal),
  }
}

/// The refusal of a lineage branch verb on a Subversion repository (§4.17.4: the repository is one green whose version N
/// is revision N, and a branch is a directory in it, made by a copy a client commits): `Unsupported`, by kind, so no verb
/// makes a green no Subversion path serves or unbinds the one they serve; none for another tool's repository.
fn no_subversion_branch(record: &RepositoryRecord) -> Option<ReplyBody> {
  (record.tool == slates_db::catalog::VcsTool::Subversion).then(|| {
    refused(Refusal::Unsupported {
      feature: "a branch of a Subversion repository as a green of its own: Subversion's branches are directories in \
                its one green, made by a copy (`svn copy`)"
        .to_owned(),
    })
  })
}

/// `RetireBranch` (module docs): `name` leaves the namespace and its green stays a member. Answered `Acknowledged`.
pub(crate) fn retire_branch(
  state: &mut ShardState,
  principal: &Principal,
  (repository, name): (&str, &str),
) -> ReplyBody {
  let record = match owned_repository(state, principal, repository, "retire_branch") {
    Ok(record) => record,
    Err(reply) => return *reply,
  };
  if let Some(reply) = no_subversion_branch(&record) {
    return reply;
  }
  // One entry of the repository's register (A-603).
  let entry = next_entry(state, record.id).map(|mut entry| {
    entry.names.push(NameChange::Retire {
      name: spelled(&record, name),
    });
    entry
  });
  match entry.and_then(|entry| self::record(state, &Op::RepositoryAdvanced { entry })) {
    Ok(()) => ReplyBody::Acknowledged,
    Err(refusal) => refused(refusal),
  }
}

/// `DestroyRepository` (§4.17.5, A-843): its owner destroys repository `repository` whole on this, its owner shard.
/// Everything a later step could refuse is judged before the first record, since what a refused verb recorded commits
/// with its refusal: the repository's own destroy (`Op::RepositoryDestroyed`: its record, its namespace and its logs),
/// and each member green, held on this partition and administered by `principal`. Then, in the verb's transaction: the
/// repository's destroy; each member's own destroy (`merge_service::destroy_merge_volume`), leaves first, in the reverse
/// of the order they joined, so no green is destroyed while a member made from it stands (`BranchedFrom`); the holds
/// its git records kept of their names and packs, read while its register still named them; and its tool store and its
/// usage row (`vcs_store::free`). Nothing another repository holds is touched: a content both held keeps the other's
/// hold. A push or fetch of the repository under way finds its store gone at its next turn and is refused as for a
/// name never made. Answered `Destroyed`.
pub(crate) fn destroy_repository(
  state: &mut ShardState,
  principal: &Principal,
  repository: &str,
) -> ReplyBody {
  let record = match owned_repository(state, principal, repository, "destroy_repository") {
    Ok(record) => record,
    Err(reply) => return *reply,
  };
  let destroyed = Op::RepositoryDestroyed {
    repository: record.id,
  };
  if let Err(refusal) = admits(state, &destroyed) {
    return refused(refusal);
  }
  // Leaves first: `members_of` lists a member after the member it was made from.
  let mut members: Vec<DbVolumeId> = state
    .db
    .partition()
    .repositories()
    .members_of(record.id)
    .map(|member| member.green)
    .collect();
  members.reverse();
  for green in &members {
    let Some(volume) = state.db.partition().volume(*green) else {
      return refused(Refusal::NotFound);
    };
    if !matches!(volume.policy.role, Role::Green { .. }) {
      return refused(Refusal::NotFound);
    }
    if !crate::verbs::rights_of(volume, principal).admin {
      return forbidden("destroy_repository");
    }
  }
  let holds = crate::vcs_store::record_holds(state, record.id);
  if let Err(refusal) = self::record(state, &destroyed) {
    return refused(refusal);
  }
  for green in members {
    match crate::merge_service::destroy_merge_volume(state, principal, to_wire_volume(green)) {
      Some(ReplyBody::Destroyed) => {}
      // Judged above (a green, administered by `principal`), so only a refusal no guard foresaw, the log at its
      // capacity: the verb's refusal.
      Some(reply) => return reply,
      None => return refused(Refusal::NotFound),
    }
  }
  for identity in holds {
    crate::vcs_store::release_names(state, &identity);
  }
  crate::vcs_store::free(state, record.id);
  ReplyBody::Destroyed
}

/// What [`set_policy`] sets a policy to, as the wire carries it: its rules, its hooks, its hooks-env sections and the
/// keys trusted to sign its commits.
pub(crate) type PolicyParts<'a> = (
  &'a [slates_ipc::protocol::PolicyRule],
  &'a [slates_ipc::protocol::HookConfig],
  &'a [slates_ipc::protocol::HooksEnvSection],
  &'a [Vec<u8>],
);

/// Sets repository `repository`'s policy whole (§4.17.1 "Policy", A-622): its owner's verb, made from policy version
/// `read`, the version its owner read: its rules, its hooks, for Subversion its hooks-env (A-648), and for git the keys
/// trusted to sign its commits (A-644). One entry of the repository's register: `Op::RepositoryPolicySet` takes its next
/// position. The database's guard refuses a version the policy has moved past (`PolicyChanged`, naming the version now)
/// and a rule, hook or hooks-env section the repository cannot hold (`Database`, naming the first by index), before
/// anything is recorded; this verb refuses a trusted signer A-321's verifier cannot read, or signers for a repository
/// that is not git's (`BadRequest`), and keeps each key once, in the order given.
pub(crate) fn set_policy(
  state: &mut ShardState,
  principal: &Principal,
  (repository, read): (&str, u64),
  (rules, hooks, hooks_env, signers): PolicyParts<'_>,
) -> ReplyBody {
  let record = match owned_repository(state, principal, repository, "set_repository_policy") {
    Ok(record) => record,
    Err(reply) => return *reply,
  };
  let Some(version) = read.checked_add(1) else {
    return refused(Refusal::PolicyChanged {
      current: record.policy.version,
    });
  };
  let signers = match commit_signers(record.tool, signers) {
    Ok(signers) => signers,
    Err(refusal) => return refused(refusal),
  };
  let policy = RepositoryPolicy {
    version,
    rules: rules.iter().map(db_rule).collect(),
    hooks: hooks.iter().map(db_hook).collect(),
    hooks_env: hooks_env.iter().map(db_hooks_env_section).collect(),
    signers,
  };
  let op = Op::RepositoryPolicySet {
    repository: record.id,
    policy,
  };
  match self::record(state, &op) {
    Ok(()) => ReplyBody::Acknowledged,
    Err(refusal) => refused(refusal),
  }
}

/// Repository `repository`'s policy read whole by `principal`, its owner alone (§4.17.1 "Policy"; A-622): its version
/// and every part, in the wire's forms, so a setter changes what it names and keeps the rest (`slates repository
/// policy`). Refused `NotFound` and `Forbidden` as setting it is, and `BadRequest` for a rule naming a principal the wire
/// cannot carry, so no client ever sets back a policy it read short.
pub(crate) fn describe_policy(
  state: &ShardState,
  principal: &Principal,
  repository: &str,
) -> ReplyBody {
  let record = match owned_repository(state, principal, repository, "describe_repository_policy") {
    Ok(record) => record,
    Err(reply) => return *reply,
  };
  let policy = &record.policy;
  let rules: Option<Vec<slates_ipc::protocol::PolicyRule>> =
    policy.rules.iter().map(wire_rule).collect();
  let Some(rules) = rules else {
    return refused(Refusal::BadRequest {
      reason: "a rule names a principal the wire cannot carry".to_owned(),
    });
  };
  ReplyBody::RepositoryPolicy {
    version: policy.version,
    rules,
    hooks: policy.hooks.iter().map(wire_hook).collect(),
    hooks_env: policy
      .hooks_env
      .iter()
      .map(wire_hooks_env_section)
      .collect(),
    signers: policy.signers.clone(),
  }
}

/// The wire's form of a policy rule (A-622), [`db_rule`]'s inverse; none for a principal the wire cannot carry.
fn wire_rule(rule: &slates_db::catalog::PolicyRule) -> Option<slates_ipc::protocol::PolicyRule> {
  use slates_db::catalog::{PolicyEffect, PolicyNames, PolicyRight};
  use slates_ipc::protocol as wire;
  Some(wire::PolicyRule {
    names: match rule.names {
      PolicyNames::Refs => wire::PolicyNames::Refs,
      PolicyNames::Branches => wire::PolicyNames::Branches,
      PolicyNames::Bookmarks => wire::PolicyNames::Bookmarks,
      PolicyNames::Topics => wire::PolicyNames::Topics,
      PolicyNames::Paths => wire::PolicyNames::Paths,
    },
    pattern: rule.pattern.clone(),
    right: match rule.right {
      PolicyRight::Read => wire::PolicyRight::Read,
      PolicyRight::Submit => wire::PolicyRight::Submit,
      PolicyRight::Rewrite => wire::PolicyRight::Rewrite,
      PolicyRight::Delete => wire::PolicyRight::Delete,
      PolicyRight::MergeCommit => wire::PolicyRight::MergeCommit,
      PolicyRight::UnsignedCommit => wire::PolicyRight::UnsignedCommit,
    },
    effect: match rule.effect {
      PolicyEffect::Allow => wire::PolicyEffect::Allow,
      PolicyEffect::Deny => wire::PolicyEffect::Deny,
    },
    who: wire_who(&rule.who)?,
  })
}

/// The wire's form of the principals a rule speaks to, [`db_who`]'s inverse; none for a principal it cannot carry.
fn wire_who(who: &slates_db::catalog::PolicyWho) -> Option<slates_ipc::protocol::PolicyWho> {
  use slates_db::catalog::{PolicyEntry, PolicyWho};
  use slates_ipc::protocol as wire;
  Some(match who {
    PolicyWho::Anyone => wire::PolicyWho::Anyone,
    PolicyWho::AnyOf { entries } => wire::PolicyWho::AnyOf {
      entries: entries
        .iter()
        .map(|entry| match entry {
          PolicyEntry::Is { principal } => crate::verbs::to_wire_principal(principal)
            .map(|principal| wire::PolicyEntry::Is { principal }),
          PolicyEntry::IsNot { principal } => crate::verbs::to_wire_principal(principal)
            .map(|principal| wire::PolicyEntry::IsNot { principal }),
        })
        .collect::<Option<Vec<_>>>()?,
    },
  })
}

/// The wire's form of a configured hook, [`db_hook`]'s inverse.
fn wire_hook(hook: &slates_db::catalog::HookConfig) -> slates_ipc::protocol::HookConfig {
  use slates_db::catalog::HookPointName as Point;
  use slates_ipc::protocol::HookPointName as Given;
  slates_ipc::protocol::HookConfig {
    point: match hook.point {
      Point::GitPreReceive => Given::GitPreReceive,
      Point::GitUpdate => Given::GitUpdate,
      Point::GitPostReceive => Given::GitPostReceive,
      Point::HgPretxnchangegroup => Given::HgPretxnchangegroup,
      Point::HgChangegroup => Given::HgChangegroup,
      Point::SvnPreCommit => Given::SvnPreCommit,
      Point::SvnPreRevpropChange => Given::SvnPreRevpropChange,
      Point::SvnPostCommit => Given::SvnPostCommit,
      Point::SvnStartCommit => Given::SvnStartCommit,
      Point::SvnPostRevpropChange => Given::SvnPostRevpropChange,
      Point::SvnPreLock => Given::SvnPreLock,
      Point::SvnPostLock => Given::SvnPostLock,
      Point::SvnPreUnlock => Given::SvnPreUnlock,
      Point::SvnPostUnlock => Given::SvnPostUnlock,
    },
    name: hook.name.clone(),
    program: hook.program.clone(),
  }
}

/// The wire's form of a hooks-env section, [`db_hooks_env_section`]'s inverse.
fn wire_hooks_env_section(
  section: &slates_db::catalog::HooksEnvSection,
) -> slates_ipc::protocol::HooksEnvSection {
  slates_ipc::protocol::HooksEnvSection {
    name: section.name.clone(),
    variables: section
      .variables
      .iter()
      .map(|variable| slates_ipc::protocol::HooksEnvVariable {
        name: variable.name.clone(),
        value: variable.value.clone(),
      })
      .collect(),
  }
}

/// The keys a repository's policy trusts to sign its commits, as its record keeps them (A-644): each one A-321's
/// verifier reads (`TrustedKey::from_wire`), once, in the order given; and none but a git repository's, the one tool
/// whose push judges a commit's signature.
fn commit_signers(
  tool: slates_db::catalog::VcsTool,
  keys: &[Vec<u8>],
) -> Result<Vec<Vec<u8>>, Refusal> {
  if tool != slates_db::catalog::VcsTool::Git && !keys.is_empty() {
    return Err(Refusal::BadRequest {
      reason: "trusted commit signers are a git repository's alone".to_owned(),
    });
  }
  let mut seen: std::collections::BTreeSet<&[u8]> = std::collections::BTreeSet::new();
  let mut kept: Vec<Vec<u8>> = Vec::new();
  for key in keys {
    if crate::signed_work::TrustedKey::from_wire(key.clone()).is_err() {
      return Err(Refusal::BadRequest {
        reason:
          "a trusted signer is an ssh-ed25519 or sk-ssh-ed25519@openssh.com key in SSH wire form"
            .to_owned(),
      });
    }
    if seen.insert(key.as_slice()) {
      kept.push(key.clone());
    }
  }
  Ok(kept)
}

/// The database's form of a policy rule (A-622).
fn db_rule(rule: &slates_ipc::protocol::PolicyRule) -> slates_db::catalog::PolicyRule {
  use slates_db::catalog::{PolicyEffect, PolicyNames, PolicyRight};
  use slates_ipc::protocol as wire;
  slates_db::catalog::PolicyRule {
    names: match rule.names {
      wire::PolicyNames::Refs => PolicyNames::Refs,
      wire::PolicyNames::Branches => PolicyNames::Branches,
      wire::PolicyNames::Bookmarks => PolicyNames::Bookmarks,
      wire::PolicyNames::Topics => PolicyNames::Topics,
      wire::PolicyNames::Paths => PolicyNames::Paths,
    },
    pattern: rule.pattern.clone(),
    right: match rule.right {
      wire::PolicyRight::Read => PolicyRight::Read,
      wire::PolicyRight::Submit => PolicyRight::Submit,
      wire::PolicyRight::Rewrite => PolicyRight::Rewrite,
      wire::PolicyRight::Delete => PolicyRight::Delete,
      wire::PolicyRight::MergeCommit => PolicyRight::MergeCommit,
      wire::PolicyRight::UnsignedCommit => PolicyRight::UnsignedCommit,
    },
    effect: match rule.effect {
      wire::PolicyEffect::Allow => PolicyEffect::Allow,
      wire::PolicyEffect::Deny => PolicyEffect::Deny,
    },
    who: db_who(&rule.who),
  }
}

/// The database's form of the principals a rule speaks to, each principal as the catalog names it (A-622).
fn db_who(who: &slates_ipc::protocol::PolicyWho) -> slates_db::catalog::PolicyWho {
  use slates_db::catalog::{PolicyEntry, PolicyWho};
  use slates_ipc::protocol as wire;
  match who {
    wire::PolicyWho::Anyone => PolicyWho::Anyone,
    wire::PolicyWho::AnyOf { entries } => PolicyWho::AnyOf {
      entries: entries
        .iter()
        .map(|entry| match entry {
          wire::PolicyEntry::Is { principal } => PolicyEntry::Is {
            principal: crate::verbs::to_db_principal(principal),
          },
          wire::PolicyEntry::IsNot { principal } => PolicyEntry::IsNot {
            principal: crate::verbs::to_db_principal(principal),
          },
        })
        .collect(),
    },
  }
}

/// The database's form of a configured hook (A-622).
fn db_hook(hook: &slates_ipc::protocol::HookConfig) -> slates_db::catalog::HookConfig {
  use slates_db::catalog::HookPointName as Point;
  use slates_ipc::protocol::HookPointName as Given;
  slates_db::catalog::HookConfig {
    point: match hook.point {
      Given::GitPreReceive => Point::GitPreReceive,
      Given::GitUpdate => Point::GitUpdate,
      Given::GitPostReceive => Point::GitPostReceive,
      Given::HgPretxnchangegroup => Point::HgPretxnchangegroup,
      Given::HgChangegroup => Point::HgChangegroup,
      Given::SvnPreCommit => Point::SvnPreCommit,
      Given::SvnPreRevpropChange => Point::SvnPreRevpropChange,
      Given::SvnPostCommit => Point::SvnPostCommit,
      Given::SvnStartCommit => Point::SvnStartCommit,
      Given::SvnPostRevpropChange => Point::SvnPostRevpropChange,
      Given::SvnPreLock => Point::SvnPreLock,
      Given::SvnPostLock => Point::SvnPostLock,
      Given::SvnPreUnlock => Point::SvnPreUnlock,
      Given::SvnPostUnlock => Point::SvnPostUnlock,
    },
    name: hook.name.clone(),
    program: hook.program.clone(),
  }
}

/// The database's form of a hooks-env section (A-648).
fn db_hooks_env_section(
  section: &slates_ipc::protocol::HooksEnvSection,
) -> slates_db::catalog::HooksEnvSection {
  slates_db::catalog::HooksEnvSection {
    name: section.name.clone(),
    variables: section
      .variables
      .iter()
      .map(|variable| slates_db::catalog::HooksEnvVariable {
        name: variable.name.clone(),
        value: variable.value.clone(),
      })
      .collect(),
  }
}

/// Retires member `id` from its lineage at its destroy (module docs): its engine is gone at once, and so is its map of
/// paths to git's files (A-664 part 3, `vcs_store::paths::forget`), and what only it held in the lineage is queued for
/// [`run_releases`], which starts when none runs. False when `id` is no member.
pub(crate) fn retire_member(state: &mut ShardState, id: DbVolumeId) -> bool {
  // Read before the retire, which ends the membership the lineage answers by.
  let lineage = state.greens.lineage_of(&id);
  match state.greens.retire_queued(&id) {
    None => false,
    Some(Ok(start)) => {
      if let Some(repository) = lineage {
        crate::vcs_store::paths::forget(state, repository, id);
      }
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
#[path = "repository/store_tests.rs"]
mod store_tests;

#[cfg(test)]
mod tests {
  use slates_db::catalog::Principal;
  use slates_ipc::protocol::{
    ObjectFormat, Refusal, ReplyBody, RequestBody, VcsTool, VolumeId, VolumeRefusal,
  };

  use crate::greens::ReleaseProgress;
  use crate::state::ShardState;
  use crate::verbs::to_db_volume;

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
    crate::test_store::create(state, principal, body)
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
    crate::test_store::create(
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

  /// Makes `green`'s next version through a work of its own, file `f` set to `bytes`, submitted with a message and the
  /// session's identities as a repository's green requires (A-664); the work is destroyed after, so no reader is left
  /// holding the green's floor. The version made.
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
    let message = format!("f, version {serial}");
    let submitted = ask(state, &owner(), crate::identities::authored(work, &message));
    // A green's first submit builds its map in slices after it (A-664's live whole builds): driven to its end here.
    crate::vcs_store::paths::whole_build::settle(state);
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
    crate::test_store::audit_on_stored_shard("repository", |state| {
      let at_one = branches_read_their_versions(state);
      retired_and_bound_again(state, at_one);
      wrong_verbs_refused_by_kind(state);
      assert_eq!(state.greens.busy_lends(), 0);
    });
  }

  /// A-843 (§4.17.5: a repository destroyed while others are served disturbs none of them). Do: make repositories
  /// `doomed` and `kept`, write each root, branch each, and branch `doomed`'s branch again; destroy `doomed` as another
  /// principal, then as its owner, then once more; then make a repository named `doomed` again.
  /// Expect: the stranger refused `Forbidden` with nothing changed; the owner answered `Destroyed`: the name gone (a
  /// branch of it `NotFound`), each of its three greens gone from the catalog and from the engines, its store and its
  /// usage row no longer held; `kept` whole (its record, both greens' bytes, its store); the second destroy `NotFound`;
  /// the name made again as a new, empty repository.
  #[test]
  fn a_destroyed_repository_goes_whole_and_leaves_every_other_untouched() {
    crate::test_store::audit_on_stored_shard("repository-destroy", |state| {
      let destroy = |state: &mut ShardState, principal: &Principal| {
        ask(
          state,
          principal,
          RequestBody::DestroyRepository {
            repository: "doomed".to_owned(),
          },
        )
      };
      let (doomed_greens, kept_greens) = doomed_and_kept(state);
      let [doomed_root, doomed_branch, doomed_leaf] = doomed_greens;
      let (doomed, kept) = (repository(state, "doomed"), repository(state, "kept"));

      assert!(matches!(
        refusal(destroy(state, &stranger())),
        Refusal::Forbidden { .. }
      ));
      assert_eq!(
        file(state, doomed_leaf).as_deref(),
        Some(b"doomed".as_slice())
      );

      let destroyed = destroy(state, &owner());
      assert!(matches!(destroyed, ReplyBody::Destroyed), "{destroyed:?}");
      doomed_gone(state, doomed, [doomed_root, doomed_branch, doomed_leaf]);
      kept_whole(state, kept, kept_greens);

      assert!(matches!(
        refusal(destroy(state, &owner())),
        Refusal::NotFound
      ));
      let again = green_of(create(state, "doomed", "main"));
      assert_eq!(file(state, again), None);
      assert_ne!(repository(state, "doomed"), doomed);
    });
  }

  /// Repositories `doomed` (its root written, a branch of it and a branch of that) and `kept` (its root written and a
  /// branch), each store held: `doomed`'s three greens and `kept`'s two.
  fn doomed_and_kept(state: &mut ShardState) -> ([VolumeId; 3], (VolumeId, VolumeId)) {
    let doomed_root = green_of(create(state, "doomed", "main"));
    assert_eq!(write(state, doomed_root, b"doomed", 1), 1);
    let doomed_branch = green_of(branch(state, ("doomed", "topic", "main"), None));
    let doomed_leaf = green_of(branch(state, ("doomed", "leaf", "topic"), None));
    let kept_root = green_of(create(state, "kept", "main"));
    assert_eq!(write(state, kept_root, b"kept", 2), 1);
    let kept_branch = green_of(branch(state, ("kept", "topic", "main"), None));
    let (doomed, kept) = (repository(state, "doomed"), repository(state, "kept"));
    assert!(state.vcs_stores.holds(doomed) && state.vcs_stores.holds(kept));
    (
      [doomed_root, doomed_branch, doomed_leaf],
      (kept_root, kept_branch),
    )
  }

  /// Repository `doomed` (`repository`, its `greens`) gone whole after its destroy: its name, each green from the catalog
  /// and the engines, its store; a branch of it `NotFound`.
  fn doomed_gone(
    state: &mut ShardState,
    repository: slates_db::catalog::RepositoryId,
    greens: [VolumeId; 3],
  ) {
    let partition = state.db.partition();
    assert!(
      partition
        .repositories()
        .repository_named("doomed")
        .is_none()
    );
    for green in greens {
      assert!(
        partition.volume(to_db_volume(green)).is_none(),
        "{green:?} still recorded"
      );
      assert!(
        state.greens.get(&to_db_volume(green)).is_none(),
        "{green:?} still served"
      );
    }
    assert!(
      !state.vcs_stores.holds(repository),
      "the destroyed repository's store is held"
    );
    assert!(matches!(
      refusal(branch(state, ("doomed", "again", "main"), None)),
      Refusal::NotFound
    ));
  }

  /// Repository `kept` (`repository`, its root and branch) whole beside a destroy: its store held, both greens' bytes,
  /// and its branch still written.
  fn kept_whole(
    state: &mut ShardState,
    repository: slates_db::catalog::RepositoryId,
    (root, branch): (VolumeId, VolumeId),
  ) {
    assert!(state.vcs_stores.holds(repository));
    assert_eq!(file(state, root).as_deref(), Some(b"kept".as_slice()));
    assert_eq!(file(state, branch).as_deref(), Some(b"kept".as_slice()));
    assert_eq!(write(state, branch, b"still", 3), 1);
  }

  /// Repository `linux` made, its root written twice while a work holds version 1, and branched at version 1 and at
  /// its head without a walk: each branch reads its version's bytes and the root its head's. The branch at version 1.
  fn branches_read_their_versions(state: &mut crate::state::ShardState) -> VolumeId {
    let root = held_root_written_twice(state);
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
    at_one
  }

  /// Repository `linux` made and its root written once, a work made at version 1 to hold it, and the root written
  /// again: the root.
  fn held_root_written_twice(state: &mut crate::state::ShardState) -> VolumeId {
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
    root
  }

  /// Branch `topic`, at `at_one`, retired and bound again: the retired name leaves its green.
  fn retired_and_bound_again(state: &mut crate::state::ShardState, at_one: VolumeId) {
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
  }

  /// Each wrong verb on `linux`, refused by its kind: a taken name, an unknown branch, a version past the head, a name
  /// with a NUL, another principal, and a bind of a green outside the lineage.
  fn wrong_verbs_refused_by_kind(state: &mut crate::state::ShardState) {
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
  }

  /// The line key `id` is placed and taken over by (A-636 part 2b-2), and whether it is a family id.
  fn line_of(id: [u8; 16]) -> (slates_db::register::ObjectId, bool) {
    let id = slates_db::register::ObjectId(id);
    (id.line_key(), id.is_family())
  }

  /// A-636 part 2b-2 (§4.8 "Promotion and takeover"; §2 item 12: ids route to owners). Do: create a repository, branch
  /// it twice, make a green alone and create a second repository adopting it as its root, then branch that one. Expect:
  /// every id a family id, each distinct; the first repository, its root and its branches on one line key, which places
  /// them on one set of holders and names one successor for all of them; the second repository and its branch on the
  /// adopted green's line, another one.
  #[test]
  fn a_repository_and_every_green_made_for_it_share_one_line() {
    crate::test_store::audit_on_stored_shard("repository", |state| {
      let root = green_of(create(state, "linux", "main"));
      let topic = green_of(branch(state, ("linux", "topic", "main"), None));
      let other = green_of(branch(state, ("linux", "other", "main"), None));
      let repo = repository(state, "linux");
      let alone = green_of(ask(
        state,
        &owner(),
        RequestBody::CreateGreen {
          name: "alone".to_owned(),
          require_evidence: false,
          base: None,
        },
      ));
      let adopted = green_of(ask(
        state,
        &owner(),
        RequestBody::CreateRepository {
          name: "adopter".to_owned(),
          tool: VcsTool::Git,
          format: ObjectFormat::Sha1,
          default_branch: "main".to_owned(),
          green: Some(alone),
        },
      ));
      assert_eq!(adopted, alone, "the adopted green is the root");
      let late = green_of(branch(state, ("adopter", "late", "main"), None));
      let adopter = repository(state, "adopter");
      let first: Vec<[u8; 16]> = [root, topic, other]
        .map(|green| to_db_volume(green).bytes)
        .into_iter()
        .chain([repo.bytes])
        .collect();
      let second: Vec<[u8; 16]> = [alone, late]
        .map(|green| to_db_volume(green).bytes)
        .into_iter()
        .chain([adopter.bytes])
        .collect();
      let distinct: std::collections::BTreeSet<[u8; 16]> =
        first.iter().chain(&second).copied().collect();
      assert_eq!(
        distinct.len(),
        first.len() + second.len(),
        "ids are distinct"
      );
      let lines = |ids: &[[u8; 16]]| {
        ids
          .iter()
          .map(|id| line_of(*id))
          .collect::<std::collections::BTreeSet<_>>()
      };
      let (first_lines, second_lines) = (lines(&first), lines(&second));
      assert_eq!(first_lines.len(), 1, "{first_lines:?}");
      assert_eq!(second_lines.len(), 1, "{second_lines:?}");
      assert!(
        first_lines
          .iter()
          .chain(&second_lines)
          .all(|(_, family)| *family)
      );
      assert_ne!(
        first_lines, second_lines,
        "two repositories' families are told apart"
      );
    });
  }

  /// A-603, A-642 (§4.17.1 shared piece 2: each green keeps its own log; the namespace is a log of its own). Do: create a
  /// repository, write its root until the service folds its first version (`write_until_folded`: as many writes as the
  /// doubling rule's own inputs ask), branch it at that version and then at its head, retire the branch's name and bind
  /// it again, write the branch; then write a green no repository holds, and send a bind the guard refuses.
  /// Expect: each verb moves the names log alone, by each change it makes, and each record the repository's register
  /// keeps moves it by one (A-724 piece 8c: a version's record rides the names log to the repository's candidates); each
  /// member's version moves its own green's log alone; the refused branch, the lone green's version and the refused bind
  /// move no log of the repository; and no verb or lone version is an atomic record.
  #[test]
  fn each_verb_moves_the_names_log_and_each_members_version_its_own_log() {
    crate::test_store::audit_on_stored_shard("repository", |state| {
      let root = green_of(create(state, "tree", "main"));
      let repo = repository(state, "tree");
      let names = |state: &ShardState| {
        state
          .db
          .partition()
          .repositories()
          .log_position(repo, slates_db::catalog::StateLog::Names)
      };
      // The names log moves by each verb's changes and by each record the repository's register keeps (A-724 piece 8c:
      // a version's record rides the names log to the repository's candidates, so a successor replays it).
      let records = |state: &ShardState| {
        u64::try_from(state.db.partition().repositories().objects(repo).count()).unwrap()
      };
      let moved = |state: &ShardState, verbs: u64| Some(verbs + records(state));
      let head_of =
        |state: &ShardState, green| state.db.partition().green_head(to_db_volume(green));
      assert_eq!(
        names(state),
        moved(state, 2),
        "the root joined and its name bound"
      );
      write(state, root, b"v1", 1);
      let mut serial = 1;
      let root_head = write_until_folded(state, root, &mut serial);
      assert_eq!(
        (names(state), head_of(state, root)),
        (moved(state, 2), root_head)
      );
      // No reader held version 1, so the root folded it: a branch there is refused before any record.
      assert!(matches!(
        refusal(branch(state, ("tree", "early", "main"), Some(1))),
        Refusal::UnknownBase { .. }
      ));
      let topic = green_of(branch(state, ("tree", "topic", "main"), None));
      assert_eq!(names(state), moved(state, 4));
      retire_and_bind_again(state, topic);
      assert_eq!(names(state), moved(state, 6));
      write(state, topic, b"t1", serial + 1);
      assert_eq!(
        (names(state), head_of(state, topic), head_of(state, root)),
        (moved(state, 6), 1, root_head)
      );
      assert_a_lone_green_and_a_refused_bind_move_nothing(state, topic, serial + 2);
      assert_eq!(names(state), moved(state, 6));
      assert_eq!(
        state.db.partition().repositories().atomic_records().count(),
        0
      );
    });
  }

  /// Writes `green` a version at a time, each through a work destroyed after (`write`), so no reader holds a version,
  /// until the service's doubling rule takes a checkpoint past the one `green` holds
  /// (`merge_service::checkpoint_green_when`: a checkpoint is taken once the increments it would fold reach the size of
  /// the one held). How many writes that takes follows the checkpoint's and the increments' encodings, so it is read
  /// from the rule's own inputs after each write (`merge_service::fold_inputs`), never fixed here: while no checkpoint
  /// is taken the rule must be waiting on them, and each write must have added bytes to fold, so the writes end within
  /// the held checkpoint's size in bytes. Returns the head reached, `serial` left at the last serial used.
  fn write_until_folded(state: &mut ShardState, green: VolumeId, serial: &mut u32) -> u64 {
    let id = to_db_volume(green);
    let held = state.db.partition().green_floor(id);
    let mut waited_on = 0;
    loop {
      *serial += 1;
      let head = write(state, green, format!("v{serial}").as_bytes(), *serial);
      // A checkpoint the write's floor move began may be held in turns (A-579), with a build's task waiting on this
      // shard: taken whole here, since no turn runs inside this step.
      crate::merge_service::settle_checkpoints(state);
      if state.db.partition().green_floor(id) > held {
        return head;
      }
      let inputs =
        crate::merge_service::fold_inputs(state, id).expect("versions past the checkpoint");
      assert!(
        inputs.waits(),
        "the doubling rule's inputs say fold, yet no checkpoint was taken: {inputs:?}"
      );
      assert!(
        inputs.to_fold > waited_on,
        "a version added no bytes to fold: {inputs:?}"
      );
      waited_on = inputs.to_fold;
    }
  }

  /// Retires `topic`'s name and binds it again.
  fn retire_and_bind_again(state: &mut ShardState, topic: VolumeId) {
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
  }

  /// Writes a green no repository holds (its work named by `serial`), and binds a taken name: the one moves its own
  /// log, the other nothing.
  fn assert_a_lone_green_and_a_refused_bind_move_nothing(
    state: &mut ShardState,
    topic: VolumeId,
    serial: u32,
  ) {
    let lone = green_of(ask(
      state,
      &owner(),
      RequestBody::CreateGreen {
        name: "lone".to_owned(),
        require_evidence: false,
        base: None,
      },
    ));
    write(state, lone, b"l1", serial);
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
  }

  /// A-575 (§4.16 the retire sliced; §4.17.1 item 4's destroy). Do: branch a repository's root, write the branch, then
  /// destroy the root, then the branch, and give the branch's holds back one unit at a time. Expect: the root refuses its
  /// destroy `Pinned` while the branch lives; the branch's destroy answers at once with its engine gone and its release
  /// owed; no unit frees more than one node; once the release is done the lineage holds what the root alone held.
  #[test]
  fn a_destroyed_branch_is_released_a_node_a_unit_and_its_source_is_pinned_until_then() {
    crate::test_store::audit_on_stored_shard("repository", |state| {
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
    crate::test_store::audit_on_stored_shard("repository", |state| {
      let members = members_made_past_a_held_checkpoint(state);
      assert_rebuilt_as_before(state, members);
      let [root, ..] = members;
      let corrupt = slates_db::op::Op::GreenAdvanced {
        green: to_db_volume(root),
        increment: vec![0xff; 16],
      };
      state.db.mutate(&mut state.segment, &corrupt, 0).unwrap();
      recover(state);
      for green in members {
        let id = to_db_volume(green);
        assert!(!state.greens.contains_key(&id), "{green:?} served");
        assert!(
          state.fenced_greens.contains_key(&id),
          "{green:?} not fenced"
        );
      }
    });
  }

  /// Repository `tree`'s root branched at version 1 and written, the branch branched and retired, the root written on
  /// and its checkpoint forced: it stays at or below version 1. The root, the branch and the branch's branch.
  fn members_made_past_a_held_checkpoint(state: &mut ShardState) -> [VolumeId; 3] {
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
    [root, topic, deep]
  }

  /// Every member rebuilt as a recovery does reads as before it: head, identity, every version.
  fn assert_rebuilt_as_before(state: &mut ShardState, members: [VolumeId; 3]) {
    let mut live = recover(state);
    for green in members {
      let floor = common_floor(&state.greens, &live, green);
      let rebuilt = answers(&mut state.greens, green, floor);
      assert!(rebuilt.is_some(), "{green:?} not rebuilt");
      assert_eq!(
        rebuilt,
        answers(&mut live, green, floor),
        "{green:?} after the rebuild"
      );
    }
  }

  /// A-622 (§4.17.1 "Policy"). Do: create a git repository; set its policy from version 0 with a rule denying every
  /// principal but its owner the submit of `main`, and a pre-receive hook; set it from 0 again; set it as another
  /// principal; set one holding a branch rule, which a git repository has not, and one running a relative program; then
  /// set an empty one from 1.
  /// Expect: the first acknowledged, and the record holds it at version 1 with the rule's principal as the catalog names
  /// it; the stale setting refused `PolicyChanged` naming 1; the stranger `Forbidden`; the malformed ones `Database`
  /// naming the rule or the hook by index, nothing changed; the last acknowledged, the policy empty at version 2.
  #[test]
  fn a_repositorys_policy_is_its_owners_to_set_whole_from_the_version_read() {
    use slates_db::catalog::RepositoryPolicy;
    crate::test_store::audit_on_stored_shard("repository", |state| {
      green_of(create(state, "linux", "main"));
      let first = first_policy_held(state);
      assert_eq!(
        refusal(set_linux_policy(
          state,
          &owner(),
          0,
          (Vec::new(), Vec::new())
        )),
        Refusal::PolicyChanged { current: 1 }
      );
      assert!(matches!(
        refusal(set_linux_policy(
          state,
          &stranger(),
          1,
          (Vec::new(), Vec::new())
        )),
        Refusal::Forbidden { .. }
      ));
      assert_malformed_policies_refused(state);
      assert_eq!(
        linux_policy(state),
        first,
        "a refused setting changes nothing"
      );
      assert_eq!(
        set_linux_policy(state, &owner(), 1, (Vec::new(), Vec::new())),
        ReplyBody::Acknowledged
      );
      assert_eq!(
        linux_policy(state),
        RepositoryPolicy {
          version: 2,
          ..RepositoryPolicy::default()
        }
      );
    });
  }

  /// Repository `linux`'s policy set from version 0 to a rule denying all but its owner the submit of `main` and a
  /// pre-receive hook: acknowledged, held at version 1 with the rule's principal as the catalog names it. The policy.
  fn first_policy_held(state: &mut ShardState) -> slates_db::catalog::RepositoryPolicy {
    use slates_ipc::protocol::PolicyNames;
    let program = std::env::current_exe()
      .unwrap()
      .to_string_lossy()
      .into_owned();
    let main = vec![deny_submit_but_owner(PolicyNames::Refs, "refs/heads/main")];
    assert_eq!(
      set_linux_policy(state, &owner(), 0, (main, vec![pre_receive(&program)])),
      ReplyBody::Acknowledged
    );
    let first = linux_policy(state);
    assert_eq!(first.version, 1);
    assert_eq!(first.hooks.len(), 1);
    assert_eq!(
      first.rules.first().map(|rule| rule.who.clone()),
      Some(slates_db::catalog::PolicyWho::AnyOf {
        entries: vec![slates_db::catalog::PolicyEntry::IsNot { principal: owner() }],
      })
    );
    first
  }

  /// A rule denying every principal but the owner the submit of `pattern` among `names`.
  fn deny_submit_but_owner(
    names: slates_ipc::protocol::PolicyNames,
    pattern: &str,
  ) -> slates_ipc::protocol::PolicyRule {
    use slates_ipc::protocol::{PolicyEffect, PolicyEntry, PolicyRight, PolicyRule, PolicyWho};
    PolicyRule {
      names,
      pattern: pattern.to_owned(),
      right: PolicyRight::Submit,
      effect: PolicyEffect::Deny,
      who: PolicyWho::AnyOf {
        entries: vec![PolicyEntry::IsNot {
          principal: slates_ipc::protocol::Principal::Uid { host: 0, uid: 1234 },
        }],
      },
    }
  }

  /// A pre-receive hook running `program`.
  fn pre_receive(program: &str) -> slates_ipc::protocol::HookConfig {
    slates_ipc::protocol::HookConfig {
      point: slates_ipc::protocol::HookPointName::GitPreReceive,
      name: "pre-receive".to_owned(),
      program: program.to_owned(),
    }
  }

  /// Repository `linux`'s policy set by `principal` from version `read` to `rules` and `hooks`: the reply.
  fn set_linux_policy(
    state: &mut ShardState,
    principal: &Principal,
    read: u64,
    (rules, hooks): (
      Vec<slates_ipc::protocol::PolicyRule>,
      Vec<slates_ipc::protocol::HookConfig>,
    ),
  ) -> ReplyBody {
    ask(
      state,
      principal,
      RequestBody::SetRepositoryPolicy {
        repository: "linux".to_owned(),
        read,
        rules,
        hooks,
        hooks_env: Vec::new(),
        signers: Vec::new(),
      },
    )
  }

  /// Repository `linux`'s policy as its record holds it.
  fn linux_policy(state: &ShardState) -> slates_db::catalog::RepositoryPolicy {
    state
      .db
      .partition()
      .repositories()
      .repository_named("linux")
      .map(|record| record.policy.clone())
      .unwrap()
  }

  /// A policy holding a branch rule, which a git repository has not, and one running a relative program: each refused
  /// `Database`, naming the rule or the hook by index.
  fn assert_malformed_policies_refused(state: &mut ShardState) {
    use slates_ipc::protocol::{DatabaseRefusal, PolicyNames};
    let branch_rule = vec![
      deny_submit_but_owner(PolicyNames::Refs, "refs/tags/*"),
      deny_submit_but_owner(PolicyNames::Branches, "main"),
    ];
    assert_eq!(
      refusal(set_linux_policy(
        state,
        &owner(),
        1,
        (branch_rule, Vec::new())
      )),
      Refusal::Database {
        refusal: DatabaseRefusal::PolicyRuleMalformed { index: 1 }
      }
    );
    assert_eq!(
      refusal(set_linux_policy(
        state,
        &owner(),
        1,
        (Vec::new(), vec![pre_receive("hooks/pre-receive")])
      )),
      Refusal::Database {
        refusal: DatabaseRefusal::PolicyHookMalformed { index: 0 }
      }
    );
  }

  /// A-648 (§4.17.4 item 9). Do: create a Subversion repository; set its policy with a hook at each of the six points
  /// the record gained and a hooks-env of a default section and a pre-lock one; then set one whose second section names
  /// a variable holding `=`; and, in a git repository, one holding a hooks-env at all. Expect: the first acknowledged and
  /// held whole, each point and section as given; the second refused naming section 1 and the git one section 0, with
  /// nothing changed.
  #[test]
  fn a_subversion_repositorys_policy_holds_its_six_new_points_and_its_hooks_env() {
    use slates_db::catalog::HookPointName as Held;
    use slates_ipc::protocol::{
      DatabaseRefusal, HookConfig, HookPointName, HooksEnvSection, HooksEnvVariable,
    };
    // A Subversion repository: a node keeping a store (C2; main's ruling of 2026-11-20).
    crate::test_store::audit_on_stored_shard("vcs", |state| {
      let svn = RequestBody::CreateRepository {
        name: "svn".to_owned(),
        tool: VcsTool::Subversion,
        format: ObjectFormat::Sha1,
        default_branch: "trunk".to_owned(),
        green: None,
      };
      green_of(crate::test_store::create(state, &owner(), svn));
      let program = std::env::current_exe()
        .unwrap()
        .to_string_lossy()
        .into_owned();
      let points = [
        (
          HookPointName::SvnStartCommit,
          "start-commit",
          Held::SvnStartCommit,
        ),
        (
          HookPointName::SvnPostRevpropChange,
          "post-revprop-change",
          Held::SvnPostRevpropChange,
        ),
        (HookPointName::SvnPreLock, "pre-lock", Held::SvnPreLock),
        (HookPointName::SvnPostLock, "post-lock", Held::SvnPostLock),
        (
          HookPointName::SvnPreUnlock,
          "pre-unlock",
          Held::SvnPreUnlock,
        ),
        (
          HookPointName::SvnPostUnlock,
          "post-unlock",
          Held::SvnPostUnlock,
        ),
      ];
      let hooks: Vec<HookConfig> = points
        .iter()
        .map(|(point, name, _)| HookConfig {
          point: *point,
          name: (*name).to_owned(),
          program: program.clone(),
        })
        .collect();
      let section = |name: &str, variable: &str, value: &[u8]| HooksEnvSection {
        name: name.to_owned(),
        variables: vec![HooksEnvVariable {
          name: variable.to_owned(),
          value: value.to_vec(),
        }],
      };
      let env = vec![
        section("default", "LANG", b"en_US.UTF-8"),
        section("pre-lock", "ONLY", b"pre-lock"),
      ];
      let set = |state: &mut ShardState, repository: &str, read: u64, hooks, hooks_env| {
        ask(
          state,
          &owner(),
          RequestBody::SetRepositoryPolicy {
            repository: repository.to_owned(),
            read,
            rules: Vec::new(),
            hooks,
            hooks_env,
            signers: Vec::new(),
          },
        )
      };
      let held = |state: &ShardState, name: &str| {
        state
          .db
          .partition()
          .repositories()
          .repository_named(name)
          .map(|record| record.policy.clone())
          .unwrap()
      };
      assert_eq!(set(state, "svn", 0, hooks, env), ReplyBody::Acknowledged);
      let first = held(state, "svn");
      let points_held: Vec<Held> = first.hooks.iter().map(|hook| hook.point).collect();
      let expected: Vec<Held> = points.iter().map(|(_, _, held)| *held).collect();
      assert_eq!(points_held, expected);
      let held_section =
        |name: &str, variable: &str, value: &[u8]| slates_db::catalog::HooksEnvSection {
          name: name.to_owned(),
          variables: vec![slates_db::catalog::HooksEnvVariable {
            name: variable.to_owned(),
            value: value.to_vec(),
          }],
        };
      let given = vec![
        held_section("default", "LANG", b"en_US.UTF-8"),
        held_section("pre-lock", "ONLY", b"pre-lock"),
      ];
      assert_eq!(first.hooks_env, given);
      let bad = vec![
        section("default", "LANG", b"C"),
        section("pre-lock", "A=B", b"1"),
      ];
      assert_eq!(
        refusal(set(state, "svn", 1, Vec::new(), bad)),
        Refusal::Database {
          refusal: DatabaseRefusal::PolicyHooksEnvMalformed { index: 1 }
        }
      );
      green_of(create(state, "linux", "main"));
      let git_env = vec![section("default", "LANG", b"C")];
      assert_eq!(
        refusal(set(state, "linux", 0, Vec::new(), git_env)),
        Refusal::Database {
          refusal: DatabaseRefusal::PolicyHooksEnvMalformed { index: 0 }
        }
      );
      assert_eq!(
        held(state, "svn"),
        first,
        "a refused setting changes nothing"
      );
    });
  }

  /// An `ssh-ed25519` public key in SSH wire form, `fill` in every byte of its key.
  fn signer_key(fill: u8) -> Vec<u8> {
    let mut blob = b"\0\0\0\x0bssh-ed25519\0\0\0\x20".to_vec();
    blob.extend_from_slice(&[fill; 32]);
    blob
  }

  /// Sets repository `repository`'s policy from version `read`: `rules` and `signers`, no hook.
  fn set_signed(
    state: &mut ShardState,
    (repository, read): (&str, u64),
    rules: Vec<slates_ipc::protocol::PolicyRule>,
    signers: Vec<Vec<u8>>,
  ) -> ReplyBody {
    let body = RequestBody::SetRepositoryPolicy {
      repository: repository.to_owned(),
      read,
      rules,
      hooks: Vec::new(),
      hooks_env: Vec::new(),
      signers,
    };
    ask(state, &owner(), body)
  }

  /// A rule denying everyone `right` on `pattern` of `names`.
  fn denying(
    names: slates_ipc::protocol::PolicyNames,
    pattern: &str,
    right: slates_ipc::protocol::PolicyRight,
  ) -> slates_ipc::protocol::PolicyRule {
    slates_ipc::protocol::PolicyRule {
      names,
      pattern: pattern.to_owned(),
      right,
      effect: slates_ipc::protocol::PolicyEffect::Deny,
      who: slates_ipc::protocol::PolicyWho::Anyone,
    }
  }

  /// C1, C2, C5 (main's ruling of 2026-11-20, inbox w-svn-from-main-c2-rulings decision 1: one rule for all three tools).
  /// Do: on a daemon started without a changeset store, create a Mercurial, a Subversion and a git repository. Expect
  /// each refused `Unsupported`, naming `--vcs-store`, and no repository recorded for any. Red before: the daemon made a
  /// Mercurial repository whose history it kept in its register, and (before C5's part 2) a git one.
  #[test]
  fn a_daemon_without_a_store_refuses_mercurial_and_subversion_naming_the_flag() {
    let (hg, svn, git, recorded) = crate::daemon::audit_on_shard(|state| {
      let made = [VcsTool::Mercurial, VcsTool::Subversion, VcsTool::Git].map(|tool| {
        ask(
          state,
          &owner(),
          RequestBody::CreateRepository {
            name: format!("{tool:?}"),
            tool,
            format: ObjectFormat::Sha1,
            default_branch: "main".to_owned(),
            green: None,
          },
        )
      });
      let recorded = ["Mercurial", "Subversion", "Git"].map(|name| {
        state
          .db
          .partition()
          .repositories()
          .repository_named(name)
          .is_some()
      });
      let [hg, svn, git] = made;
      (hg, svn, git, recorded)
    });
    for reply in [hg, svn, git] {
      let Refusal::Unsupported { feature } = refusal(reply) else {
        panic!("refused by kind");
      };
      assert!(feature.contains("--vcs-store"), "{feature}");
    }
    assert_eq!(recorded, [false, false, false]);
  }

  /// A-644 (§4.17.2 "Protected branches", configured through A-622's policy). Do: set a git repository's policy with
  /// rules denying merge commits and unsigned commits on `main` and three signers, one repeated; set one whose signer
  /// is no key; create a Mercurial repository and set its policy with a signer, then with a rule denying merge commits.
  /// Expect: the first acknowledged, its record holding both rules and the two keys once each, in order; the malformed
  /// key refused `BadRequest`, nothing changed; the Mercurial signer refused `BadRequest` and its rule refused by the
  /// database's guard naming it, since no Mercurial push judges either.
  #[test]
  fn protections_and_trusted_signers_are_a_git_repositorys_policy() {
    use slates_ipc::protocol::{PolicyNames, PolicyRight};
    // A Mercurial repository among them: a node keeping a store (main's ruling of 2026-11-20).
    crate::test_store::audit_on_stored_shard("vcs", |state| {
      green_of(create(state, "linux", "main"));
      let rules = vec![
        denying(
          PolicyNames::Refs,
          "refs/heads/main",
          PolicyRight::MergeCommit,
        ),
        denying(
          PolicyNames::Refs,
          "refs/heads/main",
          PolicyRight::UnsignedCommit,
        ),
      ];
      let keys = vec![signer_key(1), signer_key(2), signer_key(1)];
      let set = set_signed(state, ("linux", 0), rules, keys);
      assert_eq!(set, ReplyBody::Acknowledged);
      let policy = policy_of(state, "linux");
      let rights: Vec<_> = policy.rules.iter().map(|rule| rule.right).collect();
      use slates_db::catalog::PolicyRight as Held;
      assert_eq!(rights, vec![Held::MergeCommit, Held::UnsignedCommit]);
      assert_eq!(policy.signers, vec![signer_key(1), signer_key(2)]);
      let malformed = set_signed(state, ("linux", 1), Vec::new(), vec![b"not a key".to_vec()]);
      assert!(matches!(refusal(malformed), Refusal::BadRequest { .. }));
      assert_eq!(
        policy_of(state, "linux"),
        policy,
        "a refused setting changes nothing"
      );
      assert_mercurial_refuses_signers_and_protections(state);
    });
  }

  /// Repository `name`'s policy as its record holds it.
  fn policy_of(state: &ShardState, name: &str) -> slates_db::catalog::RepositoryPolicy {
    let repositories = state.db.partition().repositories();
    repositories
      .repository_named(name)
      .map(|record| record.policy.clone())
      .unwrap()
  }

  /// A Mercurial repository made, its policy set with a signer and then with a rule denying merge commits: the signer
  /// refused `BadRequest` and the rule by the database's guard, naming it.
  fn assert_mercurial_refuses_signers_and_protections(state: &mut ShardState) {
    use slates_ipc::protocol::{DatabaseRefusal, PolicyNames, PolicyRight};
    let created = crate::test_store::create(
      state,
      &owner(),
      RequestBody::CreateRepository {
        name: "hg".to_owned(),
        tool: VcsTool::Mercurial,
        format: ObjectFormat::Sha1,
        default_branch: "default".to_owned(),
        green: None,
      },
    );
    assert!(!matches!(created, ReplyBody::Refused { .. }), "{created:?}");
    let signed = set_signed(state, ("hg", 0), Vec::new(), vec![signer_key(1)]);
    assert!(matches!(refusal(signed), Refusal::BadRequest { .. }));
    let merges = vec![denying(
      PolicyNames::Branches,
      "default",
      PolicyRight::MergeCommit,
    )];
    assert_eq!(
      refusal(set_signed(state, ("hg", 0), merges, Vec::new())),
      Refusal::Database {
        refusal: DatabaseRefusal::PolicyRuleMalformed { index: 0 }
      }
    );
  }

  /// A-622 (`slates repository policy`). Do: make a repository; read its policy as its owner; set a hook from the
  /// version read; read it again; then read and set it as another principal, and set it again from the stale version.
  /// Expect: version 0 and no part at first; the hook read back at version 1; the stranger's read and set each
  /// `Forbidden`, and the stale set `PolicyChanged` with the version now, so a setter never drops what another set.
  #[test]
  fn a_repositorys_policy_is_read_and_set_by_its_owner_alone() {
    use slates_ipc::protocol::{HookConfig, HookPointName};
    crate::test_store::audit_on_stored_shard("repository", |state| {
      assert!(matches!(
        create(state, "policy", "main"),
        ReplyBody::GreenCreated { .. }
      ));
      let describe = |state: &mut ShardState, principal: &Principal| {
        ask(
          state,
          principal,
          RequestBody::DescribeRepositoryPolicy {
            repository: "policy".to_owned(),
          },
        )
      };
      let hook = HookConfig {
        point: HookPointName::GitPreReceive,
        name: "pre-receive".to_owned(),
        program: "/srv/hooks/pre-receive".to_owned(),
      };
      let set = |state: &mut ShardState, principal: &Principal, read: u64| {
        ask(
          state,
          principal,
          RequestBody::SetRepositoryPolicy {
            repository: "policy".to_owned(),
            read,
            rules: Vec::new(),
            hooks: vec![hook.clone()],
            hooks_env: Vec::new(),
            signers: Vec::new(),
          },
        )
      };
      let first = describe(state, &owner());
      let made = set(state, &owner(), 0);
      let after = describe(state, &owner());
      let foreign_read = refusal(describe(state, &stranger()));
      let foreign_set = refusal(set(state, &stranger(), 1));
      let stale = refusal(set(state, &owner(), 0));
      let empty = |reply: &ReplyBody| {
        matches!(reply, ReplyBody::RepositoryPolicy { version: 0, rules, hooks, hooks_env, signers }
          if rules.is_empty() && hooks.is_empty() && hooks_env.is_empty() && signers.is_empty())
      };
      let hooked = |reply: &ReplyBody| matches!(reply, ReplyBody::RepositoryPolicy { version: 1, hooks, .. } if hooks == &vec![hook.clone()]);
      assert_eq!(
        (
          empty(&first),
          made,
          hooked(&after),
          matches!(foreign_read, Refusal::Forbidden { .. }),
          matches!(foreign_set, Refusal::Forbidden { .. }),
          stale,
        ),
        (
          true,
          ReplyBody::Acknowledged,
          true,
          true,
          true,
          Refusal::PolicyChanged { current: 1 },
        )
      );
    });
  }

  /// Writes one version of `green` that sets `files` distinct paths (`/d0`, `/d1`, ...) through one work, destroyed
  /// after: a version that changes as many identity keys as files.
  fn write_files(state: &mut ShardState, green: VolumeId, files: usize, serial: u32) -> u64 {
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
    for file in 0..files {
      let edited = ask(
        state,
        &owner(),
        RequestBody::Edit {
          work,
          path: format!("/d{file}"),
          at: 0,
          delete_len: 0,
          bytes: format!("file {file} at {serial}").into_bytes(),
        },
      );
      assert!(matches!(edited, ReplyBody::Edited), "{edited:?}");
    }
    let submitted = ask(
      state,
      &owner(),
      crate::identities::authored(work, "many files"),
    );
    crate::vcs_store::paths::whole_build::settle(state);
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

  /// A-579 (goal 89b: the server's own p99 under load; a green's first checkpoint at 1,000 clients folded its identity's
  /// backlog in one 73.7 ms step on its owner, and every request the owner served waited behind it). Do: with another
  /// task ready on the shard throughout (someone waiting), write a green a version that changes 400 files through a
  /// work and destroy the work, which moves the green's floor and so asks its checkpoint (`verbs::owner_floor`, the
  /// path the 1,000-client run took). Expect: the destroy returns with the checkpoint's identity fold paced, not taken
  /// whole (the green marked as checkpointing, keys left to fold, no checkpoint yet); the shard then finishes it in turns
  /// (`merge.checkpoint.in_turns` 1, more than one identity step) and takes the checkpoint, the identity then equal to
  /// its recount. Before, the destroy folded every key and recorded the checkpoint in its own step.
  #[test]
  fn a_checkpoint_with_someone_waiting_folds_the_identity_in_turns() {
    /// Shape: the files the version changes: many keys against a step quantum of a few microseconds.
    const FILES: usize = 400;
    /// Shape: the looks the test waits for the turns to end, each after a shard's step has run.
    const LOOKS: usize = 10_000;
    // A git repository's history is kept on the node's changeset store: a store-backed audit daemon.
    let (_store, daemon) = crate::test_store::audit_daemon_stored();
    let green = crate::daemon::observe_first(&daemon, |state| {
      // Someone waits: a task ready on this shard, polled only once this closure has returned.
      let waiting = slates_rt::futures::spawn(async {}).unwrap();
      let _ = slates_rt::futures::detach(waiting);
      let root = green_of(create(state, "tree", "main"));
      let id = to_db_volume(root);
      let floor_before = state.db.partition().green_floor(id);
      write_files(state, root, FILES, 1);
      let backlog = state
        .greens
        .get(&id)
        .map_or(0, |engine| engine.identity_backlog());
      assert!(
        state.merge.checkpointing.contains(&id),
        "the checkpoint was taken whole with someone waiting"
      );
      assert!(backlog > 0, "no key left to fold in turns");
      assert_eq!(
        state.db.partition().green_floor(id),
        floor_before,
        "taken in the destroy"
      );
      id
    });
    let mut looks = 0;
    while crate::daemon::observe_first(&daemon, move |state| {
      state.merge.checkpointing.contains(&green)
    }) {
      looks += 1;
      assert!(looks < LOOKS, "the checkpoint's turns never ended");
    }
    let (in_turns, steps, taken, identity) = crate::daemon::observe_first(&daemon, move |state| {
      let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
      let identity = state
        .greens
        .get_mut(&green)
        .map(|mut engine| (engine.head_identity(), engine.head_identity_from_scratch()));
      (
        count(crate::merge_service::CHECKPOINT_IN_TURNS),
        count(crate::merge_service::CHECKPOINT_IDENTITY_STEPS),
        count(crate::merge_service::CHECKPOINT_TAKEN),
        identity,
      )
    });
    daemon.stop();
    assert_eq!(in_turns, 1);
    assert!(steps > 1, "{steps} identity steps");
    assert!(taken >= 1, "no checkpoint taken");
    let (running, recount) = identity.unwrap();
    assert_eq!(running, recount);
  }
}
