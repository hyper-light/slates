//! The commits the server makes (A-664 part 3, its commit; §4.17.1 "A push is a work" items 3 and 4; §4.17.2 "The
//! repository in git's terms" item 5 and "Pieces (git)" piece 13; A-662; after A-664 parts 1 and 2 and part 3's first
//! piece).
//!
//! **What.** A work submitted to a git repository's green carries no head operation (§4.17.1 item 3), so the server makes
//! the version's commit after the verdict, as the version commits on the owner ([`make`], from
//! `verbs::judge_submission`, once the version's map and git trees are kept by `super::paths`): A-664 part 2's
//! `Commits` over the version's trees (`paths::Head::trees`), its parent, the submit's message and the merging
//! principal's git identity (A-664 part 1: the session's identities, read from the principal's record and written over
//! any a client sent), dated at the owner's wall clock in whole seconds, in UTC. The green's ref then names the commit
//! (A-682's lender lists each branch at its green's newest store version), so a stock clone fetches native work.
//!
//! **Its parent.** The commit of the version before as git's history reads it: the head of the green's newest store
//! version that names one, whether a client pushed it or the server made it. A green with none (a branch
//! `CreateBranch` made, whose first version this is) takes the commit its branch was made at, read the same way from
//! its source's store versions at or before the version it was made at, a member a step towards the lineage's root
//! (`MemberOrigin::Branch`); a root with none makes a root commit. A version whose commit was not made (below) is
//! passed over by its successor's, whose tree holds both versions' files.
//!
//! **Recorded as a push is (A-662).** The made commit's objects (the commit, and each tree and blob of the version the
//! repository lacks) are admitted to the repository's store through the push path's own steps (`super::GitPush`:
//! admitted, sealed, the green's move to the commit at the version admitted, the record written, the store committed),
//! and recorded by a push's record (`super::record`): one move (the green's key, the version and the commit), and every
//! object the repository lacked, with its bytes. Recorded, not derived again: a recovery must make every made commit
//! again, and a version a checkpoint folded holds no tree to derive one from. The record rides an entry of the
//! repository's register that holds it alone (`RepositoryEntry::objects`, nothing else): it moves no log and takes no
//! position (A-662, A-636 part 2b), and the register's guard and apply take it as they take a push's, so the commit needs
//! no entry kind of its own. It is appended in the submit's own transaction, after the version's own record
//! (`record_accepted`'s `GreenAdvanced`), so the version and its commit are one durable atom (`slates_db::replay`'s
//! begin through commit), and a recovery replays it with every push's record, in log order, through the same steps
//! (`super::replay_git`), so the store made again holds every made commit again.
//!
//! **Guard then apply**, as `repository::push::push_sealed` does: the entry judged by the register's guard first, the
//! store committed next, the entry appended last; a store whose entry the log then refuses is made again from the log
//! (`super::rebuild`), with the version's commit not made.
//!
//! **Not made, and counted** ([`UNMADE`]): a version whose trees are not kept (its step of the map was refused), an
//! identity git would not write as it is, a commit git's strict fsck refuses (a NUL in the message), a store's refusal
//! (the budget), the register's refusal. The version stands, since its own record is in the transaction already; the
//! green's ref stays at the commit before; nothing new is held.
//!
//! **Counters.** [`MADE`] (the path's non-vacuity count), [`UNMADE`].
//!
//! **Owed** (GAPS, A-664): the holders' commits (A-636's fleet half: the merge record carrying the message, the identity
//! and the time, so every holder makes the same commit; a register entry's objects reach no holder today); a submit
//! refused before its verdict when git cannot hold its post-state or its message; Mercurial's changesets and
//! Subversion's revisions by the same rule.

use std::collections::BTreeSet;

use slates_db::catalog::{MemberOrigin, RepositoryEntry, RepositoryId, VolumeId as DbVolumeId};
use slates_db::op::Op;
use slates_git::derived::{
  Commits, CommitsRefused, Fields, Identity, IdentityRefused, Item, TreesRefused,
};
use slates_git::object_name::{Kind, ObjectFormat, ObjectId};
use slates_git::objects::tree::Mode as TreeMode;
use slates_git::store::{Incoming, Repository};
use slates_ipc::protocol::{Authorship, Refusal};
use slates_vcs::{ContentId, ContentStore, Mode};
use slates_vfs::clock::Clock;

use super::paths::{GreenPaths, GreenTrees, Head, Heads};
use super::{GitPush, GitRefused, GitStore, SealedPush, VcsStores, green_key};
use crate::error::refusal_of_db;
use crate::state::ShardState;

#[cfg(test)]
mod tests;

/// The status count of the commits the server made.
pub(crate) const MADE: &str = "vcs_commits.made";
/// The status count of the versions whose commit was not made (module docs).
pub(crate) const UNMADE: &str = "vcs_commits.unmade";

/// Format: the nanoseconds in a second: the wall clock's unit over a commit date's.
const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// The commits' type over a git repository's greens.
pub(crate) type GreenCommits = Commits<Heads>;

/// The commits' type for a repository of `format` over a store of `window`.
pub(crate) fn declare(format: ObjectFormat, window: usize) -> GreenCommits {
  Commits::declare(format, window, Mode::Eager)
}

/// Why a version's commit was not made (module docs). Each leaves the store and the register as they were. The shard
/// counts it ([`UNMADE`]) and the tests read why, so a daemon build reads no payload.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum Unmade {
  /// The version's git trees are not kept: its step of the map was refused (`super::paths`).
  NoTrees,
  /// The submit carried no message or no git identity, which a repository's green refuses before its verdict
  /// (`crate::identities`), so never here.
  Authorship,
  /// The identity is one git would not write as it is.
  Identity(IdentityRefused),
  /// The commits' type refused: git's strict fsck (a NUL in the message), the date, the store.
  Commit(CommitsRefused),
  /// The version's trees did not read.
  Trees(TreesRefused),
  /// A file the version's trees name that its map does not hold: the two disagree.
  Unread {
    /// The file's path.
    path: Vec<u8>,
  },
  /// The repository's store refused the objects, the move or the record.
  Store(GitRefused),
  /// The register refused the entry: its guard (the merge budget), or its append.
  Entry(Refusal),
}

/// One object of a made commit, as the store admits it: its type, id and content.
type Object = (Kind, ObjectId, Vec<u8>);

/// The commit's text as git signs it: its message, and its identity's name and email, with its date in seconds.
type Authored<'a> = (&'a str, &'a str, &'a str, u64);

/// Makes git's commit of `green`'s `version`, which a submit carrying `authorship` just committed on the owner outside
/// a recorded request (module docs): its date the owner's instant now, its making a job of the shard's queue
/// ([`MadeAfter`]) that lands it on the node's store and records it (C5 part 2b piece 3: every record names its
/// changeset); nothing for a green of no git repository, or a version whose commit the store holds already (a submit
/// sent again, which the engine answered from its idempotent accept). Counted, made or not.
pub(crate) fn make(
  state: &mut ShardState,
  green: DbVolumeId,
  version: u64,
  authorship: &Authorship,
) {
  let Some(repository) = super::paths::git_repository_of(state, green) else {
    return;
  };
  if made_already(&state.vcs_stores, repository, green_key(green), version) {
    return;
  }
  match authored_now(state, authorship) {
    Ok(authored) => enqueue(
      state,
      MadeJob {
        repository,
        green,
        version,
        authored,
      },
    ),
    Err(_) => state.count(UNMADE, 1),
  }
}

/// A made commit's text as git signs it, owned: its message, its identity's name and email, and its date in seconds.
#[derive(Clone, Debug)]
struct OwnedAuthored {
  /// The message.
  message: String,
  /// The identity's name.
  name: String,
  /// The identity's email.
  email: String,
  /// The date, in whole seconds of the owner's wall clock.
  seconds: u64,
}

impl OwnedAuthored {
  /// Borrowed as the commit's maker takes it.
  fn borrowed(&self) -> Authored<'_> {
    (
      self.message.as_str(),
      self.name.as_str(),
      self.email.as_str(),
      self.seconds,
    )
  }
}

/// `authorship` dated at the owner's instant now, read once: the commit's bytes carry it, so a replay reads no clock.
fn authored_now(state: &mut ShardState, authorship: &Authorship) -> Result<OwnedAuthored, Unmade> {
  let message = authorship.message.as_deref().ok_or(Unmade::Authorship)?;
  let identity = authorship
    .identities
    .git
    .as_ref()
    .ok_or(Unmade::Authorship)?;
  Ok(OwnedAuthored {
    message: message.to_owned(),
    name: identity.name.clone(),
    email: identity.email.clone(),
    seconds: u64::try_from(state.clock.wall_ns().div_euclid(NANOS_PER_SECOND)).unwrap_or(0),
  })
}

/// The status count of the commits owed by submits whose maps are built whole after them (A-664's live whole builds,
/// part 2, `super::owed`).
pub(crate) const OWED: &str = "vcs_commits.owed";

/// What a submit to `green` that committed `version` on the owner leaves of git's commit (module docs; A-664's live
/// whole builds, part 2): where the green's map of the version before is kept, the map stepped and the commit made in
/// this step, as before; where it must be built from the whole head, the commit owed, staged in this transaction
/// (`super::owed`), and the map built in slices after it (`super::paths::whole_build`), whose end makes the commit from
/// what was staged. Nothing for a green of no git repository. Counted.
pub(crate) fn after_submit(
  state: &mut ShardState,
  green: DbVolumeId,
  version: u64,
  authorship: &Authorship,
) {
  // A made commit's one step (C5 2b piece 3, [`make_deferred`]): the map was stepped and the commit landed before it,
  // and the step's own commit records it.
  if crate::made_commit::committing(state) {
    return;
  }
  let whole = super::paths::git_repository_of(state, green)
    .filter(|repository| super::paths::needs_whole(state, (*repository, green), version));
  let Some(repository) = whole else {
    super::paths::advance(state, green, version);
    make(state, green, version, authorship);
    return;
  };
  match staged(state, (repository, green, version), authorship) {
    Ok(()) => {
      state.count(OWED, 1);
      super::paths::whole_build::start(state, repository, green, version);
    }
    Err(_) => {
      // Nothing was staged: the map is stepped here, as before, and the commit made or counted unmade.
      state.count(UNMADE, 1);
      super::paths::advance(state, green, version);
    }
  }
}

/// Stages the commit `authorship` owes of `green`'s `version` (`super::owed`): its record appended in the running
/// transaction in an entry of the register holding it alone, the register's guard first, then kept by the store.
fn staged(
  state: &mut ShardState,
  (repository, green, version): (RepositoryId, DbVolumeId, u64),
  authorship: &Authorship,
) -> Result<(), Unmade> {
  let message = authorship.message.as_deref().ok_or(Unmade::Authorship)?;
  let identity = authorship
    .identities
    .git
    .as_ref()
    .ok_or(Unmade::Authorship)?;
  let key = green_key(green);
  let owed = super::owed::Owed {
    key,
    version,
    seconds: u64::try_from(state.clock.wall_ns().div_euclid(NANOS_PER_SECOND)).unwrap_or(0),
    message: message.to_owned(),
    name: identity.name.clone(),
    email: identity.email.clone(),
  };
  let entry = Op::RepositoryAdvanced {
    entry: RepositoryEntry {
      repository,
      objects: owed.encode(),
      ..RepositoryEntry::default()
    },
  };
  let now = state.clock.monotonic_ns();
  state
    .db
    .partition()
    .check(&entry, now)
    .map_err(|error| Unmade::Entry(refusal_of_db(&error)))?;
  state
    .db
    .mutate(&mut state.segment, &entry, now)
    .map_err(|error| Unmade::Entry(refusal_of_db(&error)))?;
  if let Some((_, git)) = state.vcs_stores.git(repository) {
    git.owed.insert(key, owed);
  }
  Ok(())
}

/// Makes the commit owed of `green` (`super::owed`), its map kept at the owed version, from what was staged and never the
/// clock: a job of the shard's queue ([`MadeAfter`]), taken before the submits parked behind the build run again, so
/// each names it as its parent; counted. Nothing where none is owed.
pub(crate) fn make_owed(state: &mut ShardState, green: DbVolumeId) {
  let Some(repository) = super::paths::git_repository_of(state, green) else {
    return;
  };
  let key = green_key(green);
  let Some(owed) = state
    .vcs_stores
    .git_ref(repository)
    .and_then(|(_, git)| git.owed.get(&key).cloned())
  else {
    return;
  };
  let authored = OwnedAuthored {
    message: owed.message,
    name: owed.name,
    email: owed.email,
    seconds: owed.seconds,
  };
  enqueue(
    state,
    MadeJob {
      repository,
      green,
      version: owed.version,
      authored,
    },
  );
}

/// The status count of the branches `CreateBranch` made whose ref the store could not record (A-743): zero in every
/// correct use.
pub(crate) const BRANCH_REF_UNMADE: &str = "vcs_commits.branch_ref_unmade";

/// Records the ref of the branch `green` that `CreateBranch` just made (A-743; §4.17's concept table, a slates branch a
/// git ref): the store's first version of it, at its head version, naming the commit it was made at (module docs, "Its
/// parent"), as a push of no objects moving one ref is recorded (module docs, "Recorded as a push is"), so git's site
/// serves the name the namespace binds and a push to it moves this green. Nothing for a green of no git repository, or
/// one made from a branch git sees unborn; a refusal counted.
pub(crate) fn branch_made(state: &mut ShardState, green: DbVolumeId) {
  let Some(repository) = super::paths::git_repository_of(state, green) else {
    return;
  };
  let Some(head) = parent_of(state, repository, green) else {
    return;
  };
  let version = state.db.partition().green_head(green);
  if admitted(
    state,
    repository,
    (green_key(green), version),
    (Vec::new(), head),
  )
  .is_err()
  {
    state.count(BRANCH_REF_UNMADE, 1);
  }
}

/// Whether the store holds a version of the green keyed `key` at or past its version `version`: a commit made already.
fn made_already(stores: &VcsStores, repository: RepositoryId, key: u64, version: u64) -> bool {
  stores.git_ref(repository).is_some_and(|(_, git)| {
    git
      .versions(key)
      .last()
      .is_some_and(|last| *last >= version)
  })
}

/// The commit a made commit of `green` names as its parent (module docs, "Its parent"); none for a root that has none.
fn parent_of(state: &ShardState, repository: RepositoryId, green: DbVolumeId) -> Option<ObjectId> {
  let (content, git) = state.vcs_stores.git_ref(repository)?;
  let repositories = state.db.partition().repositories();
  let mut at = (green, None);
  // A member a step towards the lineage's root, so never more steps than the lineage has members.
  for _ in repositories.members_of(repository) {
    if let Some(head) = newest_head(content, git, at) {
      return Some(head);
    }
    match &repositories.member(at.0)?.origin {
      MemberOrigin::Branch { from, version } => at = (*from, Some(*version)),
      MemberOrigin::Root => return None,
    }
  }
  None
}

/// The head of the newest of the store's versions of `green` that names a commit, at or before its version `bound`
/// where one is given: the green's versions are kept in order, so the first past the bound is found by a binary search,
/// and only a delete is passed over before it.
fn newest_head(
  content: &ContentStore,
  git: &GitStore,
  (green, bound): (DbVolumeId, Option<u64>),
) -> Option<ObjectId> {
  let key = green_key(green);
  let versions = git.versions(key);
  let within = bound.map_or(versions.len(), |bound| {
    versions.partition_point(|version| *version <= bound)
  });
  (0..within)
    .rev()
    .find_map(|at| git.repository.head(content, key, at))
}

/// The made commit's objects, the commit last, and its id (module docs): the commit made over the version's trees, and
/// each tree and blob of the version the repository lacks, read within the room the shard lends the step. The
/// commit's node is given back after: the store holds the commit once it admits it.
fn commit_objects(
  state: &mut ShardState,
  repository: RepositoryId,
  (key, version): (u64, u64),
  authored: Authored<'_>,
  parent: Option<ObjectId>,
) -> Result<(Vec<Object>, ObjectId), Unmade> {
  super::within_budget(state, 0, |stores| {
    let (content, git) = stores
      .git(repository)
      .ok_or(Unmade::Store(GitRefused::NoStore))?;
    let GitStore {
      repository: store,
      commits,
      trees,
      paths,
      heads,
      ..
    } = git;
    let head = heads
      .get(&key)
      .copied()
      .filter(|head| head.version == version)
      .ok_or(Unmade::NoTrees)?;
    let (Some(commits), Some(trees), Some(paths)) =
      (commits.as_mut(), trees.as_ref(), paths.as_ref())
    else {
      return Err(Unmade::NoTrees);
    };
    let node = made_commit(content, commits, &head, authored, parent)?;
    let read = read_made(&*content, (&*store, &*commits, trees, paths), node, &head);
    // The node holds the commit's object and the version's trees; the store holds the object once it admits it.
    let released = content.release(&node);
    let objects = read?;
    released.map_err(|refused| Unmade::Commit(CommitsRefused::Content(refused)))?;
    Ok(objects)
  })
}

/// The node of the commit the server makes of `head`'s version (module docs), held once by the caller.
fn made_commit(
  content: &mut ContentStore,
  commits: &mut GreenCommits,
  head: &Head,
  (message, name, email, seconds): Authored<'_>,
  parent: Option<ObjectId>,
) -> Result<ContentId, Unmade> {
  let identity = Identity::new(name, email).map_err(Unmade::Identity)?;
  let fields = Fields {
    tree: head.trees,
    identity,
    seconds,
    message,
  };
  let parents: Vec<ObjectId> = parent.into_iter().collect();
  commits
    .make(content, &fields, &parents)
    .map_err(Unmade::Commit)
}

/// The objects of the made commit `node` the repository lacks, the commit last, and its id: each tree from the
/// version's root down that the repository lacks, with each blob it names that the repository lacks, each once. A tree
/// the repository holds is held whole with everything it names (its push or its made commit brought them all), so it
/// is not read.
fn read_made(
  content: &ContentStore,
  (store, commits, trees, paths): (&Repository, &GreenCommits, &GreenTrees, &GreenPaths),
  node: ContentId,
  head: &Head,
) -> Result<(Vec<Object>, ObjectId), Unmade> {
  let id = commits.id(content, node).map_err(Unmade::Commit)?;
  let bytes = commits
    .object(content, node)
    .map_err(Unmade::Commit)?
    .ok_or(Unmade::Commit(CommitsRefused::Malformed { node }))?;
  let mut lacked = Lacked {
    content,
    store,
    seen: BTreeSet::new(),
    objects: Vec::new(),
  };
  let mut below = vec![(Vec::new(), head.trees)];
  while let Some(directory) = below.pop() {
    lacked.directory((trees, paths, head.root), directory, &mut below)?;
  }
  let mut objects = lacked.objects;
  objects.push((Kind::Commit, id, bytes));
  Ok((objects, id))
}

/// A directory still to read: its path (the root's empty) and its node in the version's trees.
type Below = (Vec<u8>, ContentId);

/// The objects a made commit brings, as [`read_made`] finds them: what the repository lacks, each once.
struct Lacked<'r> {
  /// The content store.
  content: &'r ContentStore,
  /// The repository's store.
  store: &'r Repository,
  /// The ids found already.
  seen: BTreeSet<ObjectId>,
  /// The objects found, in the order found.
  objects: Vec<Object>,
}

impl Lacked<'_> {
  /// Notes the tree of the directory `(path, node)` of the version's `trees` when the repository lacks it, then each
  /// blob it names that the repository lacks, its bytes its file's in the version's map `root`, and each subtree onto
  /// `below`, to be read in turn.
  fn directory(
    &mut self,
    (trees, paths, root): (&GreenTrees, &GreenPaths, ContentId),
    (path, node): Below,
    below: &mut Vec<Below>,
  ) -> Result<(), Unmade> {
    if !self.tree(trees, node)? {
      return Ok(());
    }
    for (name, item) in trees.names(self.content, node).map_err(Unmade::Trees)? {
      let at = joined(&path, &name);
      match item {
        Item::Tree { node, .. } => below.push((at, node)),
        Item::File { mode, id } => self.blob((paths, root), (mode, id), at)?,
      }
    }
    Ok(())
  }

  /// Notes the tree of `directory`, a node of `trees`, when the repository lacks it and it was not found already:
  /// whether it was, so its names are read.
  fn tree(&mut self, trees: &GreenTrees, directory: ContentId) -> Result<bool, Unmade> {
    let read = trees
      .directory(self.content, directory)
      .map_err(Unmade::Trees)?;
    if self.store.object(self.content, &read.id).is_some() || !self.seen.insert(read.id) {
      return Ok(false);
    }
    let bytes = self
      .content
      .object_bytes(&read.object, read.len)
      .ok_or(Unmade::Trees(TreesRefused::Malformed { node: directory }))?;
    self.objects.push((Kind::Tree, read.id, bytes));
    Ok(true)
  }

  /// Notes the blob `id` at `path` of the version's map `root`, when the repository lacks it and it was not found
  /// already: its bytes are its file's in the map. A submodule's link names another repository's commit, no object of
  /// this one.
  fn blob(
    &mut self,
    (paths, root): (&GreenPaths, ContentId),
    (mode, id): (TreeMode, ObjectId),
    path: Vec<u8>,
  ) -> Result<(), Unmade> {
    if mode == TreeMode::Gitlink
      || self.store.object(self.content, &id).is_some()
      || !self.seen.insert(id)
    {
      return Ok(());
    }
    let Some(file) = paths.get(self.content, root, &path).ok().flatten() else {
      return Err(Unmade::Unread { path });
    };
    let mut bytes = Vec::new();
    for window in self.content.windows(&file.content) {
      bytes.extend_from_slice(&window);
    }
    self.objects.push((Kind::Blob, id, bytes));
    Ok(())
  }
}

/// `name` beneath the directory at `path` (the root's path empty), as the map names a file.
fn joined(path: &[u8], name: &[u8]) -> Vec<u8> {
  let mut joined = path.to_vec();
  if !joined.is_empty() {
    joined.push(b'/');
  }
  joined.extend_from_slice(name);
  joined
}

/// Admits the made commit's objects and moves the green keyed `key` to the commit at `version` through the push path's
/// steps, then records and commits them (module docs, "Recorded as a push is" and "Guard then apply"). Refused, the
/// push gives back what it held.
fn admitted(
  state: &mut ShardState,
  repository: RepositoryId,
  (key, version): (u64, u64),
  (objects, commit): (Vec<Object>, ObjectId),
) -> Result<(), Unmade> {
  let mut push =
    GitPush::begin(&state.vcs_stores, repository).ok_or(Unmade::Store(GitRefused::NoStore))?;
  let arriving: Vec<Incoming<'_>> = objects
    .iter()
    .map(|(kind, id, bytes)| Incoming {
      kind: *kind,
      claimed: Some(*id),
      content: bytes,
      depth: 0,
    })
    .collect();
  if let Err(refused) = push.admit_all(state, &arriving) {
    push.abandon(state);
    return Err(Unmade::Store(refused));
  }
  let mut sealed = push.seal(state).map_err(Unmade::Store)?;
  let answer = recorded(state, repository, &mut sealed, (key, version, commit));
  // Committed, the push holds nothing and this gives back nothing; refused, it gives back what the push held.
  sealed.abandon(state);
  answer
}

/// The sealed made commit's move admitted, its record written into an entry of the register, the entry judged by the
/// register's guard, the store committed, then the entry appended in the running transaction; a store whose entry the
/// log refuses after its commit is made again from the log (module docs, "Guard then apply").
fn recorded(
  state: &mut ShardState,
  repository: RepositoryId,
  sealed: &mut SealedPush,
  (key, version, commit): (u64, u64, ObjectId),
) -> Result<(), Unmade> {
  sealed
    .admit(
      (&mut state.store, &mut state.vcs_stores),
      vec![(key, version, Some(commit))],
    )
    .map_err(Unmade::Store)?;
  record_and_commit(state, repository, sealed)
}

/// [`recorded`]'s last steps, its move admitted already: the record written into an entry of the register (version 5
/// where the push's names and pack were homed and landed, C5), judged by the register's guard, the store committed, then
/// the entry appended in the running transaction.
fn record_and_commit(
  state: &mut ShardState,
  repository: RepositoryId,
  sealed: &mut SealedPush,
) -> Result<(), Unmade> {
  let objects = sealed
    .record(super::Lent::new(&mut state.store, &mut state.vcs_stores).stores)
    .map_err(Unmade::Store)?;
  let entry = Op::RepositoryAdvanced {
    entry: RepositoryEntry {
      repository,
      objects,
      ..RepositoryEntry::default()
    },
  };
  let now = state.clock.monotonic_ns();
  state
    .db
    .partition()
    .check(&entry, now)
    .map_err(|error| Unmade::Entry(refusal_of_db(&error)))?;
  super::commit(state, sealed).map_err(Unmade::Store)?;
  if let Err(error) = state.db.mutate(&mut state.segment, &entry, now) {
    // The store is ahead of a log that refused the entry: made again from the log (A-662).
    super::rebuild(state, repository);
    return Err(Unmade::Entry(refusal_of_db(&error)));
  }
  Ok(())
}

#[cfg(test)]
thread_local! {
  /// A test's kill of a git made commit's task after its changeset landed and before its step commits (as a `kill -9`
  /// there leaves the register): the task ends there, nothing after it run. It fires once, clearing itself, so a test
  /// waits for it to read false.
  pub(crate) static KILL_BEFORE_MADE_COMMIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The status count of git commits the server made under the deferred-submit hook (C5 2b piece 3, [`make_deferred`]):
/// landed on the node's store as one changeset and recorded with their version in one step. The path's non-vacuity
/// count.
pub(crate) const MADE_LANDED: &str = "vcs_commits.made_landed";

/// Makes git's commit of the version `pending` holds, its verdict `judged` held and not applied (C5 2b piece 3, A-816,
/// `crate::made_commit`): a preview of the verdict kept across turns, then a task holding the repository's landing turn
/// steps the version's map over it a turn at a time, makes the commit's objects and seals them as a push, homes its
/// names and pack in the region and lands them on the node's store as one changeset, then in one step applies the
/// verdict and commits the version with the record naming that changeset (version 5), then answers. Nothing of the
/// accept is durable or seen before that step.
pub(crate) fn make_deferred(
  state: &mut ShardState,
  turn: crate::repository::svn_commit::landing::Turn,
  mut pending: crate::made_commit::Pending,
  judged: slates_merge::engine::Judged,
) {
  let (green, _) = pending.version();
  if !crate::made_commit::hold_preview(state, &mut pending, judged) {
    // No preview (the lineage busy): nothing of the accept is applied or durable.
    state.count(UNMADE, 1);
    turn.give_back(state);
    crate::made_commit::give_up(state, pending);
    return;
  }
  let request = pending.request();
  if slates_rt::futures::spawn(made_landing(turn, pending))
    .and_then(slates_rt::futures::detach)
    .is_err()
  {
    // The task could not start (the shard's arena full): nothing of the accept is applied or durable.
    state.count(UNMADE, 1);
    crate::made_commit::abandon(state, (request, green));
  }
}

/// Where a made commit stands before its one step ([`made_before_step`]).
enum Before {
  /// Its changeset landed: the sealed push, its record version 5 once written.
  Landed(Box<SealedPush>),
  /// Its commit not made (counted [`UNMADE`]): the version stands alone, as a version no commit was made of does
  /// (module docs).
  Unmade,
  /// Its changeset refused by the node's store or the region: nothing of the accept may be durable.
  Unlanded,
  /// The shard's state is gone (the daemon stopping).
  Gone,
}

/// [`make_deferred`]'s task: the commit made and landed before its step, then the step (`made_commit::commit`), the
/// turn given back after.
async fn made_landing(
  turn: crate::repository::svn_commit::landing::Turn,
  mut pending: crate::made_commit::Pending,
) {
  let (green, version) = pending.version();
  let before = made_before_step(&mut pending).await;
  #[cfg(test)]
  if matches!(before, Before::Landed(_)) && KILL_BEFORE_MADE_COMMIT.with(|kill| kill.replace(false))
  {
    // A test's kill between the landing and the commit: nothing after it runs, as after a `kill -9`.
    return;
  }
  let _ = crate::state::with_state(move |state| {
    match before {
      Before::Landed(mut sealed) => {
        let repository = sealed.repository();
        match crate::made_commit::commit(state, pending, |state| {
          record_and_commit(state, repository, &mut sealed)
        }) {
          Some(Ok(())) => {
            state.count(MADE, 1);
            state.count(MADE_LANDED, 1);
          }
          Some(Err(_)) => {
            sealed.release_homed(state);
            state.count(UNMADE, 1);
          }
          // The green moved meanwhile: nothing of the accept committed.
          None => {
            sealed.release_homed(state);
            super::paths::forget_preview_head(state, green, version);
          }
        }
        // Committed, the push holds nothing and this gives back nothing; refused, it gives back what it held.
        sealed.abandon(state);
      }
      Before::Unmade => {
        state.count(UNMADE, 1);
        let _ = crate::made_commit::commit(state, pending, |_| Ok::<(), Unmade>(()));
      }
      Before::Unlanded | Before::Gone => {
        super::paths::forget_preview_head(state, green, version);
        crate::made_commit::give_up(state, pending);
      }
    }
    turn.give_back(state);
  });
}

/// The made commit before its step: the version's map stepped over the held verdict's preview a turn at a time, the
/// commit's objects made and sealed as a push with its move admitted, then homed and landed.
async fn made_before_step(pending: &mut crate::made_commit::Pending) -> Before {
  let mut head_first = false;
  let mut building = None;
  loop {
    match crate::state::with_state(|state| {
      map_turn(state, pending, (&mut head_first, &mut building))
    }) {
      None => return Before::Gone,
      Some(Ok(true)) => break,
      Some(Ok(false)) => slates_rt::futures::yield_now().await,
      Some(Err(_)) => {
        // A build begun and not finished gives back what its puts held.
        if let Some(change) = building.take() {
          let _ = crate::state::with_state(|state| change.abandon(state));
        }
        return Before::Unmade;
      }
    }
  }
  let mut sealed = match crate::state::with_state(|state| sealed_made(state, pending)) {
    None => return Before::Gone,
    Some(Err(_)) => return Before::Unmade,
    Some(Ok(sealed)) => sealed,
  };
  if home_and_land(&mut sealed).await.is_some() {
    return Before::Landed(Box::new(sealed));
  }
  let _ = crate::state::with_state(move |state| {
    sealed.release_homed(state);
    sealed.abandon(state);
  });
  Before::Unlanded
}

/// The status count of the turns git commits the server makes took to put their versions' maps over the preview
/// ([`map_turn`], A-824): the slicing's non-vacuity count.
pub(crate) const MAP_TURNS: &str = "vcs_commits.made_map_turns";

#[cfg(test)]
thread_local! {
  /// A test's budget for a made commit's map turn, in place of the shard's step quantum: 0 puts one path a turn,
  /// whatever the machine's load, as `whole_build::settle` gives each slice.
  pub(crate) static TEST_MAP_BUDGET: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// The nanoseconds a made commit's map turn may spend putting paths: the shard's step quantum (a test's own where set).
fn map_budget(state: &ShardState) -> u64 {
  #[cfg(test)]
  if let Some(budget) = TEST_MAP_BUDGET.with(std::cell::Cell::get) {
    return budget;
  }
  state.config.step_quantum_ns()
}

/// What a turn of the version's map over the preview did ([`map_turn`]).
enum MapTurned {
  /// The map is of the version already.
  Kept,
  /// The head before holds no map: built first.
  HeadFirst,
  /// Paths put; more to put.
  More,
  /// Every path put: the map is made next.
  Put,
  /// A put refused.
  Refused,
}

/// One turn of the version's map over the held verdict's preview (`super::paths::over_preview_step`, A-824): the change's
/// paths put from the preview a slice a turn, within the shard's step quantum (`whole_build::PreviewChange`, `building`
/// between turns), then the change applied to the map before and kept; a root's first version's whole put the same way.
/// Whether the map is kept. A head before with no map has its map built first, in the whole build's slices, once
/// (`head_first`); a second need of it, or a put refused, leaves the commit unmade.
fn map_turn(
  state: &mut ShardState,
  pending: &mut crate::made_commit::Pending,
  (head_first, building): (
    &mut bool,
    &mut Option<super::paths::whole_build::PreviewChange>,
  ),
) -> Result<bool, Unmade> {
  use super::paths::OverPreviewStep;
  let (green, version) = pending.version();
  if super::paths::whole_build::building(state, green) {
    return Ok(false);
  }
  let over =
    super::paths::over_preview(state, green, version).ok_or(Unmade::Store(GitRefused::NoStore))?;
  let budget = map_budget(state);
  let mut charged = true;
  let turned = crate::made_commit::with_held_preview_and_stores(state, pending, |engine, lent| {
    if building.is_none() {
      let change = match super::paths::over_preview_step(engine, lent.1, &over)? {
        OverPreviewStep::Kept => return Some(MapTurned::Kept),
        OverPreviewStep::HeadFirst => return Some(MapTurned::HeadFirst),
        OverPreviewStep::Change(before, named) => Some((before, named)),
        OverPreviewStep::Whole => None,
      };
      *building = Some(super::paths::whole_build::preview_change(
        engine,
        (over.repository, version),
        change,
      ));
    }
    let (answer, whole) = building.as_mut()?.turn(engine, lent, budget);
    charged = whole;
    Some(match answer {
      Some(true) => MapTurned::Put,
      Some(false) => MapTurned::More,
      None => MapTurned::Refused,
    })
  })
  .flatten()
  .ok_or(Unmade::NoTrees)?;
  if !charged {
    state.count(super::CHARGE_SHORT, 1);
  }
  match turned {
    MapTurned::Kept => Ok(true),
    MapTurned::HeadFirst if !*head_first => {
      *head_first = true;
      super::paths::whole_build::start(state, over.repository, green, version.saturating_sub(1));
      Ok(false)
    }
    MapTurned::More => {
      state.count(MAP_TURNS, 1);
      Ok(false)
    }
    MapTurned::Put => {
      let kept = building
        .take()
        .is_some_and(|change| change.finish(state, green));
      if !kept {
        return Err(Unmade::NoTrees);
      }
      state.count(MAP_TURNS, 1);
      Ok(true)
    }
    MapTurned::HeadFirst | MapTurned::Refused => {
      if let Some(change) = building.take() {
        change.abandon(state);
      }
      Err(Unmade::NoTrees)
    }
  }
}

/// The made commit's objects over the version's kept map and trees (module docs), the submit's authorship dated now:
/// [`sealed_of`] for the version `pending` holds.
fn sealed_made(
  state: &mut ShardState,
  pending: &crate::made_commit::Pending,
) -> Result<SealedPush, Unmade> {
  let (green, version) = pending.version();
  let repository =
    super::paths::git_repository_of(state, green).ok_or(Unmade::Store(GitRefused::NoStore))?;
  let authored = authored_now(state, pending.authorship())?;
  sealed_of(state, (repository, green, version), &authored)
}

/// The commit of `green`'s `version` made over its kept map and trees as `authored`, its parent the commit before
/// (module docs), its objects admitted to a push of their own as a pack, sealed, and its move to the commit at the
/// version admitted: what its record and changeset are written from.
fn sealed_of(
  state: &mut ShardState,
  (repository, green, version): (RepositoryId, DbVolumeId, u64),
  authored: &OwnedAuthored,
) -> Result<SealedPush, Unmade> {
  let key = green_key(green);
  let parent = parent_of(state, repository, green);
  let (objects, commit) = commit_objects(
    state,
    repository,
    (key, version),
    authored.borrowed(),
    parent,
  )?;
  let mut push =
    GitPush::begin(&state.vcs_stores, repository).ok_or(Unmade::Store(GitRefused::NoStore))?;
  let arriving: Vec<Incoming<'_>> = objects
    .iter()
    .map(|(kind, id, bytes)| Incoming {
      kind: *kind,
      claimed: Some(*id),
      content: bytes,
      depth: 0,
    })
    .collect();
  if let Err(refused) = push.admit_all(state, &arriving) {
    push.abandon(state);
    return Err(Unmade::Store(refused));
  }
  let mut sealed = push.seal(state).map_err(Unmade::Store)?;
  if let Err(refused) = sealed.admit(
    (&mut state.store, &mut state.vcs_stores),
    vec![(key, version, Some(commit))],
  ) {
    sealed.abandon(state);
    return Err(Unmade::Store(refused));
  }
  Ok(sealed)
}

/// Where a made commit's homing stands between its carries (C5 part 2b, as a client's push's edge carries it): its names
/// carried to their homes, then its pack, then its changeset to the node's store.
enum Homing {
  /// The names asked homed, by their identity and count.
  Names((slates_vcs::ContentId, u64)),
  /// The pack asked homed, by its identity and length, with the names' rows the names' homing gave back.
  Pack(Vec<u8>, (slates_vcs::ContentId, u64)),
  /// The changeset asked landed, at its sequence.
  Land(u64),
}

/// A carry asked of the made commit's carrier, and where its homing then stands.
type Ask = (crate::git_push::Carry, Homing);

/// The made commit's first ask: its names put as one content and asked homed. None where it brings no object or its
/// names were not put.
fn first_ask(state: &mut ShardState, sealed: &mut SealedPush) -> Option<Ask> {
  let names = {
    let lent = super::Lent::new(&mut state.store, &mut state.vcs_stores);
    sealed.names_now(lent.stores).ok()
  };
  let (bytes, count) = names.filter(|(_, count)| *count > 0)?;
  let identity = super::put_names(state, &bytes)?;
  // The attempt its holds in the region are for until its record commits (A-727 (d1a)).
  let owner = sealed.home_owner(state);
  Some((
    crate::git_push::Carry::Home(bytes, owner),
    Homing::Names((identity, count)),
  ))
}

/// What follows `carried`, the carrier's answer to the ask `homing` stands at: the next ask, `Some(None)` once the
/// changeset landed (the push then names it), or None where any of it fell short (the caller gives back what it holds).
fn next_ask(
  state: &mut ShardState,
  sealed: &mut SealedPush,
  homing: Homing,
  carried: crate::git_push::Carried,
) -> Option<Option<Ask>> {
  use crate::git_push::{Carried, Carry};
  match (homing, carried) {
    (Homing::Names((identity, count)), Carried::Homed(homed)) => {
      if homed.outcome.as_ref().ok() != Some(&identity) {
        sealed.release_unhomed(state, &identity);
        return None;
      }
      sealed.homed_as((identity, count));
      let (bytes, packed) = sealed.pack_to_home()?;
      if !super::hold_for_record(state, &packed.0) {
        return None;
      }
      let owner = sealed.home_owner(state);
      Some(Some((
        Carry::Home(bytes, owner),
        Homing::Pack(homed.bytes, packed),
      )))
    }
    (Homing::Pack(names, packed), Carried::Homed(homed)) => {
      if homed.outcome.as_ref().ok() != Some(&packed.0) {
        sealed.release_unhomed(state, &packed.0);
        return None;
      }
      sealed.pack_homed_as(packed);
      let landing = crate::git_push::landing_of(state, sealed.repository(), (names, homed.bytes))?;
      let sequence = landing.sequence();
      Some(Some((
        Carry::Land(Box::new(landing)),
        Homing::Land(sequence),
      )))
    }
    (Homing::Land(sequence), Carried::Landed(landed)) => {
      let ok = landed.is_ok();
      state.count(
        if ok {
          crate::git_push::PUSH_LANDED
        } else {
          crate::git_push::PUSH_UNLANDED
        },
        1,
      );
      ok.then(|| {
        sealed.landed_as(sequence);
        None
      })
    }
    _ => None,
  }
}

/// Homes `sealed`'s names and then its pack in the region, and lands them on the node's store as one changeset, through
/// the edges' own carry (`crate::git_push::carry`, C5 part 2b): the push then names the changeset. None where any of it
/// fell short; the caller gives back what it holds.
async fn home_and_land(sealed: &mut SealedPush) -> Option<()> {
  let (origin, shards, mut ask) = crate::state::with_state(|state| {
    let ask = first_ask(state, sealed)?;
    Some((state.shard, state.shards.clone(), ask))
  })
  .flatten()?;
  loop {
    let (carry, homing) = ask;
    let carried = crate::git_push::carry(origin, &shards, carry).await;
    match crate::state::with_state(|state| next_ask(state, sealed, homing, carried)).flatten()? {
      Some(next) => ask = next,
      None => return Some(()),
    }
  }
}

/// [`home_and_land`] carried as a test's synchronous driver carries a push (`git_push::tests::carried_blocking`).
#[cfg(test)]
pub(crate) fn home_and_land_blocking(
  state: &mut ShardState,
  sealed: &mut SealedPush,
) -> Option<()> {
  let mut ask = first_ask(state, sealed)?;
  loop {
    let (carry, homing) = ask;
    let carried = crate::git_push::tests::carried_blocking(state, carry);
    match next_ask(state, sealed, homing, carried)? {
      Some(next) => ask = next,
      None => return Some(()),
    }
  }
}

/// A git commit the server makes of a version already committed (a submit outside a recorded request, or a commit owed
/// past a whole build, A-664): its repository, green, version and authorship, dated when the version committed, and the
/// repository's landing turn where it was free then.
struct MadeJob {
  /// The repository.
  repository: RepositoryId,
  /// The green.
  green: DbVolumeId,
  /// The version.
  version: u64,
  /// Its text as git signs it.
  authored: OwnedAuthored,
}

/// The shard's made commits of committed versions, made a job at a time by one task ([`drive_made_after`]), in the order
/// their versions committed: each landed on the node's store, then recorded in a transaction of its own (C5 part 2b
/// piece 3: no record carries its objects). Bounded by the shard's client seats, as parked submits are; past it a
/// version stands with no commit made, counted [`UNMADE`], its successor's commit holding its files.
#[derive(Default)]
pub(crate) struct MadeAfter {
  /// The jobs, oldest first, each with the landing turn it took where it was free when it came.
  jobs: std::collections::VecDeque<(
    MadeJob,
    Option<crate::repository::svn_commit::landing::Turn>,
  )>,
  /// Whether a task drives them.
  driving: bool,
}

impl Drop for MadeAfter {
  /// The stores holding the queue are being let go with the shard's state, the landing gates its turns came from with
  /// them: a turn still queued is forgotten, never handed back, since handing it back reaches for that state while it
  /// is dropped (a thread-local's drop that reaches itself aborts the process).
  fn drop(&mut self) {
    for (_, turn) in self.jobs.drain(..) {
      let _ = std::mem::ManuallyDrop::new(turn);
    }
  }
}

impl std::fmt::Debug for MadeAfter {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("MadeAfter")
      .field("jobs", &self.jobs.len())
      .field("driving", &self.driving)
      .finish()
  }
}

/// Queues `job`, its repository's landing turn taken now where it is free (so a recorded submit run after it in this
/// step lands after it), and starts the task that drives the queue where none runs.
fn enqueue(state: &mut ShardState, job: MadeJob) {
  let bound = state
    .config
    .clients_per_shard
    .saturating_mul(usize::try_from(state.config.region.slots).unwrap_or(1));
  if state.vcs_stores.made_after.jobs.len() >= bound {
    state.count(UNMADE, 1);
    return;
  }
  let turn = crate::repository::svn_commit::landing::Turn::take_now(state, job.repository);
  state.vcs_stores.made_after.jobs.push_back((job, turn));
  if state.vcs_stores.made_after.driving {
    return;
  }
  state.vcs_stores.made_after.driving = slates_rt::futures::spawn(drive_made_after())
    .and_then(slates_rt::futures::detach)
    .is_ok();
}

/// [`MadeAfter`]'s task: each job in turn, its repository's landing turn awaited where it took none, until none is left.
async fn drive_made_after() {
  loop {
    let next = crate::state::with_state(|state| {
      let next = state.vcs_stores.made_after.jobs.pop_front();
      if next.is_none() {
        state.vcs_stores.made_after.driving = false;
      }
      next
    })
    .flatten();
    let Some((job, turn)) = next else {
      return;
    };
    let turn = match turn {
      Some(turn) => Some(turn),
      None => crate::repository::svn_commit::landing::turn(job.repository).await,
    };
    made_after(job).await;
    if let Some(turn) = turn {
      let _ = crate::state::with_state(move |state| turn.give_back(state));
    }
  }
}

/// One job of [`MadeAfter`]: the commit sealed, homed and landed, then recorded.
async fn made_after(job: MadeJob) {
  let sealed = crate::state::with_state(|state| {
    sealed_of(
      state,
      (job.repository, job.green, job.version),
      &job.authored,
    )
  });
  let mut sealed = match sealed {
    Some(Ok(sealed)) => sealed,
    Some(Err(_)) => {
      let _ = crate::state::with_state(|state| state.count(UNMADE, 1));
      return;
    }
    None => return,
  };
  if home_and_land(&mut sealed).await.is_none() {
    let _ = crate::state::with_state(move |state| {
      sealed.release_homed(state);
      sealed.abandon(state);
      state.count(UNMADE, 1);
    });
    return;
  }
  let repository = job.repository;
  let _ = crate::state::with_state(move |state| recorded_after(state, repository, sealed));
}

/// A landed made commit of a committed version recorded in a transaction of its own (module docs, "Guard then apply");
/// counted. A store whose entry the log then refuses is made again from the log.
fn recorded_after(state: &mut ShardState, repository: RepositoryId, mut sealed: SealedPush) {
  state.db.begin();
  let made = record_and_commit(state, repository, &mut sealed);
  match (made, state.db.commit(&mut state.segment)) {
    (Ok(()), Ok(_)) => state.count(MADE, 1),
    (Ok(()), Err(_)) => {
      // The store is ahead of a log that refused the entry: made again from the log.
      super::rebuild(state, repository);
      state.count(UNMADE, 1);
    }
    (Err(_), _) => {
      sealed.release_homed(state);
      state.count(UNMADE, 1);
    }
  }
  // Committed, the push holds nothing and this gives back nothing; refused, it gives back what it held.
  sealed.abandon(state);
}

/// Every queued job of [`MadeAfter`] made now, as its task would, its carries through a test's synchronous driver
/// ([`home_and_land_blocking`]): for a test that holds the shard, so the task cannot run until it returns.
#[cfg(test)]
pub(crate) fn made_after_now(state: &mut ShardState) {
  while let Some((job, turn)) = state.vcs_stores.made_after.jobs.pop_front() {
    match sealed_of(
      state,
      (job.repository, job.green, job.version),
      &job.authored,
    ) {
      Ok(mut sealed) => {
        if home_and_land_blocking(state, &mut sealed).is_some() {
          recorded_after(state, job.repository, sealed);
        } else {
          sealed.release_homed(state);
          sealed.abandon(state);
          state.count(UNMADE, 1);
        }
      }
      Err(_) => state.count(UNMADE, 1),
    }
    if let Some(turn) = turn {
      turn.give_back(state);
    }
  }
}
