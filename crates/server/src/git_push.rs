//! git's push on its repository's owner shard (A-654 part 2; §4.17.1 "A push is a work" items 1 to 5 and "Policy";
//! §4.17.2 "A push" items 1 to 7, "Access", "Protected branches, in git's terms" and "Hooks, in git's terms"; A-620's
//! session, A-622's policy, A-635's append, A-638's views, A-642, A-644 and its part 2's check point, A-662's store):
//! the one driver both edges call, the HTTP site (A-625) and the SSH channel (A-651).
//!
//! **A step at a time.** [`ReceivePack::start`] makes one push's session for a principal and writes the advertisement
//! when the edge's transport opens with it (SSH; smart HTTP's `info/refs`, whose session then ends);
//! [`ReceivePack::input`] and [`ReceivePack::end`] hand it the client's bytes as they arrive, a thin pack's bases and
//! the connectivity check read from the repository's index through A-638's view; [`ReceivePack::answer`] decides the
//! checked push and writes git's report to the edge's bytes. Each call is one step on the owner shard and holds nothing
//! of the shard after it, so an edge's task keeps the driver across its own awaits. Every read of the store is lent the
//! room the shard can admit now (`vcs_store::with_room`), since A-614's store parses within its content store's room
//! and a settled store has none (A-662); the request, the pack and what reading and answering them builds are charged
//! to the room the edge gives the push, as each request on the HTTP site is (A-625).
//!
//! **What is advertised** (item 1). Each branch whose green holds a git head in the repository's store (A-662), by its
//! ref, in git's order: a namespace name `N` is the ref `refs/heads/N`, a name beginning `refs/` is the ref of that
//! name (a pushed tag `refs/tags/v1` is bound under that name), and a name beginning `refs/heads/` is no git ref, since
//! its ref is another name's; so names and refs map one to one. A green no push gave a git head (a new repository's
//! default branch, a branch the CLI made) is not advertised: git sees an unborn branch. A ref the pusher may not read
//! under the repository's policy (A-622's Read) is hidden as git's `receive.hideRefs` hides one: left out, and a
//! command naming it refused `deny updating a hidden ref`. The policy that hides is the one the push started under, as
//! git reads its configuration once a run.
//!
//! **Answering**, in this order:
//! 1. A git hook configured for the repository (`pre-receive`, `update`, `post-receive`) refuses every command, `hook
//!    not run` with a line naming the hook, until its read-only view is served to the hook driver (A-665 part 2): no
//!    ref moves that a configured hook was not asked about, and no hook is skipped without a word.
//! 2. The pushed objects are admitted to the repository's quarantine and sealed (A-662's `GitPush`); a push whose every
//!    command the protocol's checks refused admits none. A store that does not admit them (the shard's budget) refuses
//!    every command, `objects not admitted`, with why.
//! 3. Each command under the policy, through A-644 part 2's check point (`git_protect`): A-644's store answers how the
//!    change moves its ref and so the right it needs (Submit for a create or a fast-forward, Rewrite for a move that is
//!    no fast-forward, Delete for a delete), and A-622's `admit` judges that right and, where a rule imposes them, the
//!    requirements on what the change brings (a linear history, commits a trusted key signed). A refusal is in git's
//!    words (`git_protect::declined`): `non-fast-forward` and `deletion prohibited` as git refuses them itself, every
//!    other as git answers an update hook refusing it. A change whose ref names no born green is judged against a key
//!    no green has, so a create brings every commit its head reaches, as A-644 judges one. The policy is asked before
//!    the work is built, as git's `update()` refuses before it runs its hook: the cheaper refusal first, where
//!    §4.17.2's item 6 lists the work first.
//! 4. Each command's verdict, A-654 part 1's `Verdicts` over the shard's greens ([`ShardGreens`]), the repository's
//!    index and the quarantine, each work built within the room the shard can admit now.
//! 5. The append: each accepted command's entry made A-635's change and A-662's store move and appended through
//!    `push_sealed`, one entry each without `atomic`, all as one entry under it, each green first held to the position
//!    its verdict read (compare and swap, below). A refusal is git's `failed to update ref` (`atomic transaction
//!    failed` under `atomic`): a ref that moved for a conflict, a green moved, or a name taken or gone since the push
//!    read it; the append's own reason, in slates' words, for anything else.
//! 6. The report, in git's words (A-620's writer); then the sealed push is given back, holding only what no append
//!    committed.
//!
//! **A ref's base is its green's head.** A command's old value names a version only when it is the head of the newest
//! of the store's versions of its green and that version is the green's head now. A push that read an older version, or
//! a green that moved without git (a submit), is a ref that moved until A-664's server-made commits give every version
//! a commit, so that a work based on an older version can be composed and its result named. An annotated tag's tree is
//! that of the commit or tree it names.
//!
//! **A create** is made at the version of the member whose head its first parent is, where it can still be branched
//! from (the default branch's first, then the members as they joined), else at the default branch's newest version
//! (A-654 part 1), and appended as A-635's new branch made at that version, its work moving it to the pushed tree.
//! While the default branch is unborn, a create is built over an empty green and made from the default branch's green,
//! which must be empty at its version 0: the default branch itself is moved by the work, any other ref is a new branch
//! made at that empty version. A default branch that holds files no git commit names refuses it, typed: its commit is
//! A-664's.
//!
//! **A push's name** (A-618: two submits of one push carry one name, and every work's identity begins with it) is
//! hashed from its repository and its commands, so a push sent again makes the same works, and A-635 answers each the
//! version it made before.
//!
//! **Tests** (`git_push/tests.rs`): six pushes stock git 2.50.1 made, each advertisement and answer held to the bytes
//! git's server wrote and each ref's green to the tree its commit names; the policy hiding a ref and refusing a delete
//! and a submission in git's words through the check point, and a configured hook refusing typed, each refusal
//! recording nothing; `atomic` all or none, at the verdict and at the append, and a push without it a ref at a time. A
//! protected name's linear history and signed commits are A-644 part 2's own tests (`git_protect/tests.rs`).
//!
//! **Called by** the HTTP site's receive-pack (`vcs_http::git`'s `receive`, A-654 part 3). **Owed** (GAPS' §4.17
//! entry): the SSH channel's call (g294); the hooks run over A-665 part 2's view; a push based on an older version (A-664); a principal the policy allows who is
//! not the repository's owner, whom A-635's append refuses `Forbidden` until its owner check gives way to the policy;
//! push options; review requests (`refs/for/`); the new versions' pack fragments encoded off the answer's path (item
//! 8); a tag of a tag.

use core::convert::Infallible;

use slates_db::catalog::{
  HookConfig, Principal, RepositoryId, RepositoryRecord, VcsTool, VolumeId as DbVolumeId,
};
use slates_git::Admission;
use slates_git::object_name::{Kind, ObjectId};
use slates_git::objects::tag::Tag;
use slates_git::out::Out;
use slates_git::receive::works::{self, Entry, GreenRead, Unappended, Verdicts, Version};
use slates_git::receive::{
  self, Advertised, Checked, Command, Declined, HiddenRefs, Judge, Pushed, Received, Reported,
  Repository as _, Request, Session,
};
use slates_git::refname;
use slates_git::store::Incoming;
use slates_git::store::views::{IndexedObjects, PushedObjects};
use slates_git::unpack::Object;
use slates_ipc::protocol::{DatabaseRefusal, Refusal};
use slates_merge::Increment;
use slates_vcs::ContentStore;

use crate::git_protect::{StoreBrings, declined, right_of};
use crate::repository::push::{
  AppendRefused, BranchChange, ChangeRefusal, ChangeRefused, EntryParts, StoreMove, push_sealed,
};
use crate::state::ShardState;
use crate::vcs_policy::{Asked, Brings, Compiled, Name, PushRefusal, Right, admit, admit_right};
use crate::vcs_store::{GitPush, GitRefused, GitStore, SealedPush, Target, green_key, with_room};
use crate::verbs::to_wire_volume;

#[cfg(test)]
mod tests;

/// Format: the agent receive-pack advertises (`agent=`), the name upload-pack gives this server too (A-640).
const AGENT: &[u8] = slates_git::upload::AGENT.as_bytes();

/// Format: the BLAKE3 key-derivation context a push's name is hashed under, so it is never another hash's.
const PUSH_NAME_CONTEXT: &str = "slates 2026-11-19 git push name (A-654)";

/// Format: a branch's ref is this and its namespace name (module docs).
const HEADS: &str = "refs/heads/";

/// Format: every other ref a namespace name can be begins with this (git's `check_refname_format` for a pushed ref).
const REFS: &str = "refs/";

/// Slates' words: the reason every command of a push is given while a git hook is configured for its repository and
/// the hook's read-only view is not served yet (A-665 part 2).
pub(crate) const HOOK_NOT_RUN: &[u8] = b"hook not run";

/// Slates' words: the reason every command of a push is given whose objects its repository's store did not admit (the
/// shard's budget, or an object the store refused).
pub(crate) const NOT_ADMITTED: &[u8] = b"objects not admitted";

/// Slates' words: why a create of a ref no namespace name holds is refused (a ref outside `refs/`, not UTF-8, or one
/// the mapping gives another name's place; module docs).
const NOT_A_NAME: &[u8] = b"ref name not held";

/// Slates' words: why a create made while the default branch is unborn is refused when that branch binds no green, or
/// one holding files no git commit names (A-664's to commit).
const NO_EMPTY_DEFAULT: &[u8] = b"the default branch holds files no commit names";

/// Why a push cannot go on, typed: the edge answers each in its transport's words. A command's own refusal is no
/// failure: the report answers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PushFailed {
  /// No repository of this id on this shard's partition.
  NotFound,
  /// The repository serves another tool.
  NotGit,
  /// Its git store is not held here: none was made, or a recovery could not make it again (`vcs_store`).
  NoStore,
  /// Its policy holds a rule git's matcher cannot read (A-622 part 2): refused, never judged without the rule.
  Policy(PushRefusal),
  /// receive-pack refused the exchange: a request git's protocol does not allow, or one past the push's room.
  Exchange(receive::Refusal),
  /// `answer` asked of a push not yet read whole and checked, or already answered.
  Unchecked,
}

/// One git push over one connection, on its repository's owner shard (module docs).
pub(crate) struct ReceivePack {
  /// The repository.
  repository: RepositoryId,
  /// Who pushes.
  principal: Principal,
  /// The push's exchange (A-620).
  session: Session<'static, PolicyHidden>,
  /// The push's room: its request and pack, and what reading and answering them builds.
  admission: Admission,
}

impl ReceivePack {
  /// Starts a push of `principal`'s to `repository` (module docs), its request, its pack and what reading them builds
  /// admitted `room` bytes, the edge's request room; writes the advertisement to `out` when `advertise`. Refused for a
  /// repository not here or not git's, a store not held, or a policy git's matcher cannot read.
  pub(crate) fn start(
    state: &mut ShardState,
    (principal, repository): (&Principal, RepositoryId),
    (room, advertise): (u64, bool),
    out: &mut impl Out,
  ) -> Result<ReceivePack, PushFailed> {
    let record = record_of(state, repository)?;
    if record.tool != VcsTool::Git {
      return Err(PushFailed::NotGit);
    }
    let format = state
      .vcs_stores
      .git_ref(repository)
      .map(|(_, git)| git.repository.format())
      .ok_or(PushFailed::NoStore)?;
    let hidden = PolicyHidden::of(&record, principal).map_err(PushFailed::Policy)?;
    let advertised = Advertised {
      format,
      atomic: true,
      ofs_delta: true,
      push_options: false,
      session_id: None,
      agent: AGENT,
    };
    let session = Session::new(&advertised, hidden);
    if advertise {
      let refs = with_room(state, |state| advertised_refs(state, repository));
      let listed: Vec<(ObjectId, &[u8])> = refs
        .iter()
        .map(|(id, name)| (*id, name.as_slice()))
        .collect();
      session
        .advertise(&listed, out)
        .map_err(PushFailed::Exchange)?;
    }
    Ok(ReceivePack {
      repository,
      principal: principal.clone(),
      session,
      admission: Admission::new(room),
    })
  }

  /// Hands the push the client's next `bytes`: whether it is now read whole and checked, waiting for its answer. A
  /// refusal of the exchange ends the push.
  pub(crate) fn input(&mut self, state: &mut ShardState, bytes: &[u8]) -> Result<bool, PushFailed> {
    let Self {
      repository,
      session,
      admission,
      ..
    } = self;
    with_room(state, |state| {
      let (content, git) = state
        .vcs_stores
        .git_ref(*repository)
        .ok_or(PushFailed::NoStore)?;
      let indexed = IndexedObjects::new(&git.repository, content, admission.left());
      session
        .input(bytes, &indexed, admission)
        .map_err(PushFailed::Exchange)
    })
  }

  /// The client's bytes have ended: whether the push is now checked. A request cut short ends the push; a pack cut
  /// short is checked as git answers a short pack.
  pub(crate) fn end(&mut self, state: &mut ShardState) -> Result<bool, PushFailed> {
    let Self {
      repository,
      session,
      admission,
      ..
    } = self;
    with_room(state, |state| {
      let (content, git) = state
        .vcs_stores
        .git_ref(*repository)
        .ok_or(PushFailed::NoStore)?;
      let indexed = IndexedObjects::new(&git.repository, content, admission.left());
      session
        .end(&indexed, admission)
        .map_err(PushFailed::Exchange)
    })
  }

  /// Decides the checked push and writes git's report to `out` (module docs, "Answering"): what was received. Refused
  /// for a push not yet checked or answered already, a repository gone, a store not held, or a report git's protocol
  /// cannot carry.
  pub(crate) fn answer(
    &mut self,
    state: &mut ShardState,
    out: &mut impl Out,
  ) -> Result<Received, PushFailed> {
    let record = record_of(state, self.repository)?;
    let checked = self.session.checked().ok_or(PushFailed::Unchecked)?;
    let prepared = prepare(state, &record, checked)?;
    let Self {
      principal,
      session,
      admission,
      ..
    } = self;
    let answered = match prepared {
      Prepared::Declined(mut declining) => session.answer(&mut declining, out, admission),
      Prepared::Sealed { mut sealed, name } => {
        let mut judge = ServerJudge {
          state: &mut *state,
          push: &mut sealed,
          record: &record,
          principal,
          name,
        };
        let answered = session.answer(&mut judge, out, admission);
        sealed.abandon(state);
        answered
      }
    };
    answered
      .map_err(PushFailed::Exchange)?
      .ok_or(PushFailed::Unchecked)
  }
}

/// `repository`'s record on this shard's partition.
fn record_of(state: &ShardState, repository: RepositoryId) -> Result<RepositoryRecord, PushFailed> {
  let repositories = state.db.partition().repositories();
  repositories
    .repository(repository)
    .cloned()
    .ok_or(PushFailed::NotFound)
}

/// The refs `repository` advertises (module docs): each branch whose green holds a git head, by its ref, in git's
/// order (by name).
fn advertised_refs(state: &ShardState, repository: RepositoryId) -> Vec<(ObjectId, Vec<u8>)> {
  let Some(store) = state.vcs_stores.git_ref(repository) else {
    return Vec::new();
  };
  let repositories = state.db.partition().repositories();
  let mut refs: Vec<(ObjectId, Vec<u8>)> = repositories
    .branches(repository, "")
    .filter_map(|(name, green)| {
      let name = ref_of(name)?;
      Some((git_head(store, green_key(green))?, name))
    })
    .collect();
  refs.sort_unstable_by(|left, right| left.1.cmp(&right.1));
  refs
}

/// The ref the namespace name `name` is (module docs): none for a name that is no git ref. upload-pack's lender names
/// the refs it lends by the same rule (`crate::vcs_store::upload`, A-682), so both services see one ref a name.
pub(crate) fn ref_of(name: &str) -> Option<Vec<u8>> {
  if name.starts_with(HEADS) {
    return None;
  }
  let full = if name.starts_with(REFS) {
    name.as_bytes().to_vec()
  } else {
    [HEADS.as_bytes(), name.as_bytes()].concat()
  };
  refname::is_valid(&full).then_some(full)
}

/// The namespace name the ref `name` is bound under (module docs): none for a ref no namespace name is.
fn namespace_of(name: &[u8]) -> Option<&str> {
  let name = core::str::from_utf8(name).ok()?;
  let namespace = name.strip_prefix(HEADS).unwrap_or(name);
  (ref_of(namespace)?.as_slice() == name.as_bytes()).then_some(namespace)
}

/// The head of the newest of the store's versions of the green keyed `key`: none while it is unborn, or deleted.
fn git_head((content, git): (&ContentStore, &GitStore), key: u64) -> Option<ObjectId> {
  let newest = git.repository.versions(key).checked_sub(1)?;
  git.repository.head(content, key, newest)
}

/// The green the ref `name` names in `repository`'s namespace (module docs).
fn bound(state: &ShardState, repository: RepositoryId, name: &[u8]) -> Option<DbVolumeId> {
  let namespace = namespace_of(name)?;
  state
    .db
    .partition()
    .repositories()
    .branch(repository, namespace)
}

/// The key of the green the ref `name` names in `repository`, where it is born: bound, and holding a git head in the
/// store.
fn born(
  state: &ShardState,
  (repository, name): (RepositoryId, &[u8]),
  store: (&ContentStore, &GitStore),
) -> Option<u64> {
  let key = green_key(bound(state, repository, name)?);
  git_head(store, key).map(|_| key)
}

/// A key no green of `repository` has in its git store, which a change is judged against where its ref names no born
/// green: the repository's own id's, minted by the counter its greens' ids are (`verbs::fresh_volume_id`), so it is no
/// green's key. A create is so judged against no position of its own, as A-644 judges one: it brings every commit its
/// head reaches.
fn unheld_key(repository: RepositoryId) -> u64 {
  green_key(DbVolumeId {
    bytes: repository.bytes,
  })
}

/// The member of `repository` its git store keys `key` (`vcs_store::green_key`).
fn keyed(state: &ShardState, repository: RepositoryId, key: u64) -> Option<DbVolumeId> {
  let repositories = state.db.partition().repositories();
  repositories
    .members_of(repository)
    .map(|member| member.green)
    .find(|green| green_key(*green) == key)
}

/// The refs a pusher may not read under its repository's policy (module docs, "What is advertised"; A-622's Read).
/// upload-pack's lender hides the same refs from its reader by it (`crate::vcs_store::upload`, A-682), so a ref is
/// hidden alike from both services.
#[derive(Clone, Debug)]
pub(crate) struct PolicyHidden {
  /// The policy the push started under, compiled.
  policy: Compiled,
  /// The repository's record it was read from: its owner holds what no rule grants.
  record: RepositoryRecord,
  /// Who pushes.
  principal: Principal,
}

impl PolicyHidden {
  /// What `record`'s policy hides from `principal`; refused for a policy git's matcher cannot read.
  pub(crate) fn of(
    record: &RepositoryRecord,
    principal: &Principal,
  ) -> Result<PolicyHidden, PushRefusal> {
    Ok(PolicyHidden {
      policy: Compiled::of(record)?,
      record: record.clone(),
      principal: principal.clone(),
    })
  }
}

impl HiddenRefs for PolicyHidden {
  fn is_hidden(&self, name: &[u8]) -> bool {
    let asked = Asked {
      name: Name::Ref(name),
      right: Right::Read,
      brings: None,
    };
    admit_right(&self.policy, &self.record, &self.principal, &asked, 0).is_err()
  }
}

/// What a checked push is decided with (module docs, "Answering", items 1 and 2).
enum Prepared {
  /// Every command refused before any verdict.
  Declined(Declining),
  /// The push's objects sealed in quarantine.
  Sealed {
    /// The sealed push.
    sealed: SealedPush,
    /// The push's name (module docs).
    name: [u8; 32],
  },
}

/// Prepares `checked`, a push to `record`'s repository (module docs, "Answering", items 1 and 2): every command refused
/// while a git hook is configured; else its objects admitted to the quarantine and sealed (none when the checks refused
/// every command), or every command refused when the store does not admit them.
fn prepare(
  state: &mut ShardState,
  record: &RepositoryRecord,
  checked: &Checked,
) -> Result<Prepared, PushFailed> {
  if let Some(hook) = git_hook(record) {
    return Ok(Prepared::Declined(Declining::hook(hook)));
  }
  let mut push = GitPush::begin(&state.vcs_stores, record.id).ok_or(PushFailed::NoStore)?;
  let judged = checked.early.iter().any(Option::is_none);
  let objects = if judged { checked.objects() } else { &[] };
  let incoming: Vec<Incoming<'_>> = objects.iter().map(incoming_of).collect();
  if let Err(refused) = push.admit_all(state, &incoming) {
    push.abandon(state);
    return Ok(Prepared::Declined(Declining::unadmitted(&refused)));
  }
  match push.seal(state) {
    Ok(sealed) => Ok(Prepared::Sealed {
      sealed,
      name: push_name(record.id, &checked.request),
    }),
    Err(refused) => Ok(Prepared::Declined(Declining::unadmitted(&refused))),
  }
}

/// The first git hook `record`'s policy configures, if any (A-622 part 1: one hook a git point).
fn git_hook(record: &RepositoryRecord) -> Option<&HookConfig> {
  record
    .policy
    .hooks
    .iter()
    .find(|hook| hook.point.tool() == VcsTool::Git)
}

/// `object` as the store admits it (A-614's `Incoming`): claimed by the id the pack reader named it by, at the depth of
/// the delta chain the client's pack stored it at.
fn incoming_of(object: &Object) -> Incoming<'_> {
  Incoming {
    kind: object.kind,
    claimed: Some(object.id),
    content: &object.content,
    depth: object.chain.as_ref().map_or(0, |chain| chain.depth),
  }
}

/// The push's name (module docs): its repository, then each command's old and new ids and its ref, the ref's length
/// first, hashed under the push's own context.
fn push_name(repository: RepositoryId, request: &Request) -> [u8; 32] {
  let mut hasher = blake3::Hasher::new_derive_key(PUSH_NAME_CONTEXT);
  hasher.update(&repository.bytes);
  for command in &request.commands {
    let len = u64::try_from(command.name.len()).unwrap_or(u64::MAX);
    hasher.update(command.old.as_bytes());
    hasher.update(command.new.as_bytes());
    hasher.update(&len.to_le_bytes());
    hasher.update(&command.name);
  }
  *hasher.finalize().as_bytes()
}

/// A judge refusing every command it is asked about for one reason, before any verdict (module docs, "Answering",
/// items 1 and 2).
struct Declining {
  /// The reason, in slates' words.
  reason: &'static [u8],
  /// Why, the band-2 line after each command's ref.
  why: Vec<u8>,
}

impl Declining {
  /// Every command refused while `hook` is configured and its view is not served.
  fn hook(hook: &HookConfig) -> Declining {
    let why = format!(
      "the repository's {} hook cannot run here yet: its view of the push is not served",
      hook.name
    );
    Declining {
      reason: HOOK_NOT_RUN,
      why: why.into_bytes(),
    }
  }

  /// Every command refused because the store did not admit the push's objects, for `refused`.
  fn unadmitted(refused: &GitRefused) -> Declining {
    Declining {
      reason: NOT_ADMITTED,
      why: words(refused),
    }
  }
}

impl Judge for Declining {
  type Accepted = Infallible;

  fn judge(&mut self, command: &Command, _pushed: &Pushed<'_>) -> Result<Infallible, Declined> {
    let line = [
      command.name.as_slice(),
      b": ".as_slice(),
      self.why.as_slice(),
    ]
    .concat();
    Err(Declined {
      reason: self.reason.to_vec(),
      hook: Vec::new(),
      message: Some(receive::message(&line)),
    })
  }

  fn append(
    &mut self,
    _command: &Command,
    accepted: Infallible,
  ) -> Result<Vec<Reported>, Declined> {
    match accepted {}
  }

  fn append_atomic(
    &mut self,
    accepted: Vec<(&Command, Infallible)>,
  ) -> Result<Vec<Vec<Reported>>, Declined> {
    match accepted.into_iter().next() {
      Some((_, accepted)) => match accepted {},
      None => Ok(Vec::new()),
    }
  }
}

/// One accepted command as A-635 appends it: its change, and A-662's store move.
type Planned = (BranchChange, StoreMove);

/// receive-pack's judge on the owner shard (module docs, "Answering", items 3 to 5).
struct ServerJudge<'j> {
  /// The shard's state: its greens, its records and the repository's store.
  state: &'j mut ShardState,
  /// The push's objects, sealed in quarantine.
  push: &'j mut SealedPush,
  /// The repository's record as the answer read it.
  record: &'j RepositoryRecord,
  /// Who pushes.
  principal: &'j Principal,
  /// The push's name.
  name: [u8; 32],
}

impl Judge for ServerJudge<'_> {
  type Accepted = Entry;

  fn judge(&mut self, command: &Command, pushed: &Pushed<'_>) -> Result<Entry, Declined> {
    self.admitted(command)?;
    self.verdict(command, pushed)
  }

  fn append(&mut self, command: &Command, entry: Entry) -> Result<Vec<Reported>, Declined> {
    let declined = |unappended: Unappended| works::append_declined(&command.name, &unappended);
    let planned = self.planned(command, entry, 0).map_err(declined)?;
    self.appended(vec![planned]).map_err(declined)?;
    Ok(Vec::new())
  }

  fn append_atomic(
    &mut self,
    accepted: Vec<(&Command, Entry)>,
  ) -> Result<Vec<Vec<Reported>>, Declined> {
    let names: Vec<&[u8]> = accepted
      .iter()
      .map(|&(command, _)| command.name.as_slice())
      .collect();
    let declined = |unappended: Unappended| works::atomic_declined(&names, &unappended);
    let count = accepted.len();
    let mut planned = Vec::with_capacity(count);
    for (at, (command, entry)) in accepted.into_iter().enumerate() {
      planned.push(self.planned(command, entry, at).map_err(declined)?);
    }
    self.appended(planned).map_err(declined)?;
    Ok((0..count).map(|_| Vec::new()).collect())
  }
}

impl ServerJudge<'_> {
  /// The policy's verdict on `command` (module docs, "Answering", item 3): A-644's store answers how the change moves
  /// its ref and so the right it needs, and A-622's check point judges that right and, where a rule imposes one, the
  /// requirements on what the change brings (A-644 part 2's `StoreBrings`: a linear history, signed commits); a
  /// refusal in git's words (`git_protect::declined`).
  fn admitted(&mut self, command: &Command) -> Result<(), Declined> {
    let Self {
      state,
      push,
      record,
      principal,
      ..
    } = self;
    let name = command.name.as_slice();
    with_room(state, |state| {
      let Some(store) = state.vcs_stores.git_ref(record.id) else {
        return Err(refused_declined(name, words(&GitRefused::NoStore)));
      };
      let (content, git) = store;
      let green = born(state, (record.id, name), store).unwrap_or_else(|| unheld_key(record.id));
      let to = (!command.is_delete()).then_some(command.new);
      let change = git
        .repository
        .change(content, push.quarantine(), green, to)
        .map_err(|refused| refused_declined(name, words(&refused)))?;
      let right = right_of(change);
      let brings = to.map(|to| StoreBrings {
        repository: &git.repository,
        store: content,
        push: push.quarantine(),
        green,
        to,
      });
      let asked = [Asked {
        name: Name::Ref(name),
        right,
        brings: brings.as_ref().map(|brings| brings as &dyn Brings),
      }];
      admit(record, principal, &asked).map_err(|refusal| declined(&refusal, name, right))
    })
  }

  /// `command`'s entry (module docs, "Answering", item 4): A-654 part 1's verdicts over the shard's greens, the
  /// repository's index and the quarantine, each work built within the room the shard can admit now.
  fn verdict(&mut self, command: &Command, pushed: &Pushed<'_>) -> Result<Entry, Declined> {
    let Self {
      state,
      push,
      record,
      name,
      ..
    } = self;
    with_room(state, |state| {
      let Some(store) = state.vcs_stores.git_ref(record.id) else {
        return Err(refused_declined(&command.name, words(&GitRefused::NoStore)));
      };
      let (content, git) = store;
      let room = state.store.admittable();
      let indexed = IndexedObjects::new(&git.repository, content, room);
      let objects = PushedObjects::new(&git.repository, content, push.quarantine());
      let greens = ShardGreens {
        state,
        record,
        store,
        indexed: &indexed,
      };
      Verdicts::new(&greens, &indexed, &objects, *name, room).judge(command, pushed)
    })
  }

  /// `entry`, the `at`th of those appended together, as A-635's change and A-662's store move (module docs,
  /// "Answering", item 5), its green still at the position its verdict read; or why not.
  fn planned(&self, command: &Command, entry: Entry, at: usize) -> Result<Planned, Unappended> {
    let state = &*self.state;
    let repository = self.record.id;
    match entry {
      Entry::Work {
        green,
        position,
        increment,
      } => {
        let green = at_position(state, repository, (green, position), at)?;
        let change = BranchChange::Update {
          green,
          work: increment,
        };
        Ok((change, (Target::Green(green), Some(command.new))))
      }
      Entry::Retire { green, position } => {
        let green = at_position(state, repository, (green, position), at)?;
        let repositories = state.db.partition().repositories();
        let (_, name) = repositories
          .binding_of(green)
          .ok_or(Unappended::Moved { at })?;
        let change = BranchChange::Delete {
          name: name.to_owned(),
          version: position,
        };
        Ok((change, (Target::Green(green), None)))
      }
      Entry::Create {
        name,
        from,
        increment,
      } => created(
        state,
        self.record,
        (&name, from, increment),
        (command.new, at),
      ),
    }
  }

  /// Appends `planned` as one entry through A-635's append with the push's objects (A-662's `push_sealed`); why not, at
  /// the change refused.
  fn appended(&mut self, planned: Vec<Planned>) -> Result<(), Unappended> {
    let (changes, moves): (Vec<BranchChange>, Vec<StoreMove>) = planned.into_iter().unzip();
    let greens: Vec<Option<DbVolumeId>> = moves
      .iter()
      .map(|(target, _)| match target {
        Target::Green(green) => Some(*green),
        Target::Created(_) => None,
      })
      .collect();
    let who = (self.principal, self.record.id);
    push_sealed(
      &mut *self.state,
      who,
      changes,
      EntryParts::default(),
      (&mut *self.push, &moves),
    )
    .map(|_| ())
    .map_err(|refused| unappended(&refused, &greens))
  }
}

/// The member keyed `key` while its log is still at `position`, the position its verdict read (compare and swap,
/// module docs): moved otherwise.
fn at_position(
  state: &ShardState,
  repository: RepositoryId,
  (key, position): (u64, u64),
  at: usize,
) -> Result<DbVolumeId, Unappended> {
  keyed(state, repository, key)
    .filter(|green| state.db.partition().green_head(*green) == position)
    .ok_or(Unappended::Moved { at })
}

/// A create's change and move (module docs, "A create"), the `at`th of those appended together: a new branch made at
/// the version its verdict chose; where it chose none, the default branch's empty green moved by the work for the
/// default branch itself, or a new branch made at that green's empty version 0 for any other ref.
fn created(
  state: &ShardState,
  record: &RepositoryRecord,
  (name, from, work): (&[u8], Option<(u64, u64)>, Increment),
  (new, at): (ObjectId, usize),
) -> Result<Planned, Unappended> {
  let namespace = namespace_of(name)
    .ok_or_else(|| refused_at(at, NOT_A_NAME))?
    .to_owned();
  let (from, version) = match from {
    Some((key, version)) => (
      keyed(state, record.id, key).ok_or(Unappended::Moved { at })?,
      version,
    ),
    None => {
      let empty = empty_default(state, record).ok_or_else(|| refused_at(at, NO_EMPTY_DEFAULT))?;
      let repositories = state.db.partition().repositories();
      if repositories.branch(record.id, &namespace) == Some(empty) {
        let change = BranchChange::Update { green: empty, work };
        return Ok((change, (Target::Green(empty), Some(new))));
      }
      (empty, 0)
    }
  };
  let change = BranchChange::Create {
    name: namespace.clone(),
    from,
    version,
    work: Some(work),
  };
  Ok((change, (Target::Created(namespace), Some(new))))
}

/// The default branch's green while it is unborn and empty: no git version in the store, and at its version 0 holding
/// no file (module docs, "A create").
fn empty_default(state: &ShardState, record: &RepositoryRecord) -> Option<DbVolumeId> {
  let name = record.default_branch.as_deref()?;
  let green = state
    .db
    .partition()
    .repositories()
    .branch(record.id, name)?;
  let (_, git) = state.vcs_stores.git_ref(record.id)?;
  let unborn = git.repository.versions(green_key(green)) == 0;
  let empty = state
    .greens
    .get(&green)
    .is_some_and(|engine| engine.head() == 0 && engine.files().next().is_none());
  (unborn && empty).then_some(green)
}

/// A-635's refusal as receive-pack's (module docs, "Answering", item 5), `greens` naming the green each change moves,
/// in order.
fn unappended(refused: &AppendRefused, greens: &[Option<DbVolumeId>]) -> Unappended {
  match refused {
    AppendRefused::Changes(changes) => changes
      .first()
      .map_or_else(|| refused_at(0, &words(refused)), change_unappended),
    AppendRefused::Entry(Refusal::Database {
      refusal: DatabaseRefusal::Moved { green, .. },
    }) => {
      let at = greens
        .iter()
        .position(|held| held.map(to_wire_volume).as_ref() == Some(green))
        .unwrap_or_default();
      Unappended::Moved { at }
    }
    AppendRefused::Entry(_) | AppendRefused::Repository(_) => refused_at(0, &words(refused)),
  }
}

/// One change A-635 refused, as receive-pack's: a conflict is another push first; a green moved, a name taken or a
/// green gone since the push read it is a ref that moved; anything else the append's own reason.
fn change_unappended(change: &ChangeRefused) -> Unappended {
  let at = change.index;
  match &change.why {
    ChangeRefusal::Conflict { windows } => Unappended::Conflict {
      at,
      windows: windows.clone(),
    },
    ChangeRefusal::Moved { .. }
    | ChangeRefusal::Refused(Refusal::AlreadyExists { .. } | Refusal::NotFound) => {
      Unappended::Moved { at }
    }
    why => refused_at(at, &words(why)),
  }
}

/// An append's own refusal of the `at`th entry, `why` in slates' words.
fn refused_at(at: usize, why: &[u8]) -> Unappended {
  Unappended::Refused {
    at,
    why: why.to_vec(),
  }
}

/// The refusal of `name`'s command for a reason of slates' own, `why`: git's `failed to update ref`, `why` on band 2.
fn refused_declined(name: &[u8], why: Vec<u8>) -> Declined {
  works::append_declined(name, &Unappended::Refused { at: 0, why })
}

/// Slates' words for a typed refusal: its kind and fields as its type names them, as Mercurial's push words its own
/// (`repository::hg_push`).
fn words(refusal: &impl core::fmt::Debug) -> Vec<u8> {
  format!("{refusal:?}").into_bytes()
}

/// The repository's greens as one push reads them on the owner shard (module docs): a ref's green by its namespace
/// name, born once the repository's git store holds a version of it, each lent for one call.
struct ShardGreens<'s, 'v> {
  /// The shard's state, read only.
  state: &'s ShardState,
  /// The repository's record.
  record: &'s RepositoryRecord,
  /// The repository's git store, with the content store it is in.
  store: (&'s ContentStore, &'s GitStore),
  /// The repository's index as the push reads it (A-638): where an annotated tag's bytes are read.
  indexed: &'v IndexedObjects<'s>,
}

impl works::Greens for ShardGreens<'_, '_> {
  fn read<T>(&self, name: &[u8], with: impl FnOnce(GreenRead<'_>) -> T) -> Option<T> {
    let green = bound(self.state, self.record.id, name)?;
    git_head(self.store, green_key(green))?;
    self.lent(green, with)
  }

  fn version_of(&self, green: u64, head: &ObjectId) -> Option<Version> {
    let member = keyed(self.state, self.record.id, green)?;
    let (version, newest) = self.newest(member)?;
    let current = self.state.db.partition().green_head(member);
    (newest == *head && version.number == current).then_some(version)
  }

  fn base_of_create<T>(
    &self,
    parent: Option<&ObjectId>,
    with: impl FnOnce(GreenRead<'_>, Version) -> T,
  ) -> Option<T> {
    let (green, version) = parent
      .and_then(|parent| self.holding(parent))
      .or_else(|| self.default_head())?;
    self.lent(green, |read| with(read, version))
  }
}

impl ShardGreens<'_, '_> {
  /// Runs `with` over `green` lent for one call as the push reads it: its key, its log's position and its engine.
  fn lent<T>(&self, green: DbVolumeId, with: impl FnOnce(GreenRead<'_>) -> T) -> Option<T> {
    let position = self.state.db.partition().green_head(green);
    let engine = self.state.greens.get(&green)?;
    Some(with(GreenRead {
      id: green_key(green),
      position,
      green: &engine,
    }))
  }

  /// The newest of the store's versions of `green`: its version and tree, and its head; none while it is unborn or
  /// deleted.
  fn newest(&self, green: DbVolumeId) -> Option<(Version, ObjectId)> {
    let (content, git) = self.store;
    let key = green_key(green);
    let at = git.repository.versions(key).checked_sub(1)?;
    let head = git.repository.head(content, key, at)?;
    let number = *git.versions(key).get(at)?;
    let tree = self.tree_of(&head)?;
    Some((Version { number, tree }, head))
  }

  /// The tree the object `head` names (module docs): a commit's, a tree itself, or that of the commit or tree an
  /// annotated tag names; none for anything else (a tag of a tag is owed).
  fn tree_of(&self, head: &ObjectId) -> Option<ObjectId> {
    let (content, git) = self.store;
    let mut target = *head;
    let mut row = git.repository.object(content, &target)?;
    if row.kind == Kind::Tag {
      let (_, bytes) = self.indexed.object(&target)?;
      let tag = Tag::parse(bytes, &mut Admission::new(u64::MAX)).ok()?;
      target = tag.object(head.format())?;
      row = git.repository.object(content, &target)?;
    }
    match row.kind {
      Kind::Commit => git.repository.id_at(content, row.commit?.tree),
      Kind::Tree => Some(target),
      _ => None,
    }
  }

  /// The green a create whose first parent is `parent` is made from, and its version (module docs, "A create"): the
  /// newest of the store's versions of a member whose head is `parent`, where a branch can still be made at it, the
  /// default branch's first, then the members as they joined.
  fn holding(&self, parent: &ObjectId) -> Option<(DbVolumeId, Version)> {
    let (content, git) = self.store;
    let repositories = self.state.db.partition().repositories();
    let members = repositories
      .members_of(self.record.id)
      .map(|member| member.green);
    self
      .default_green()
      .into_iter()
      .chain(members)
      .find_map(|green| {
        let (number, tree) = git.version_of(content, green_key(green), parent)?;
        self.branchable(green, Version { number, tree })
      })
  }

  /// The default branch's green.
  fn default_green(&self) -> Option<DbVolumeId> {
    let name = self.record.default_branch.as_deref()?;
    self
      .state
      .db
      .partition()
      .repositories()
      .branch(self.record.id, name)
  }

  /// `version` of `green`, where a branch can still be made at it (`repository::branch_version`).
  fn branchable(&self, green: DbVolumeId, version: Version) -> Option<(DbVolumeId, Version)> {
    crate::repository::branch_version(self.state, green, Some(version.number)).ok()?;
    Some((green, version))
  }

  /// The default branch's newest store version, where a branch can still be made at it.
  fn default_head(&self) -> Option<(DbVolumeId, Version)> {
    let green = self.default_green()?;
    let (version, _) = self.newest(green)?;
    self.branchable(green, version)
  }
}
