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

/// Makes, records and admits git's commit of `green`'s `version`, which a submit carrying `authorship` just committed on
/// the owner (module docs); nothing for a green of no git repository, or a version whose commit the store holds already
/// (a submit sent again, which the engine answered from its idempotent accept). Counted, made or not.
pub(crate) fn make(
  state: &mut ShardState,
  green: DbVolumeId,
  version: u64,
  authorship: &Authorship,
) {
  match made(state, green, version, authorship) {
    Ok(true) => state.count(MADE, 1),
    Ok(false) => {}
    Err(_) => state.count(UNMADE, 1),
  }
}

/// [`make`]'s steps: whether a commit was made.
fn made(
  state: &mut ShardState,
  green: DbVolumeId,
  version: u64,
  authorship: &Authorship,
) -> Result<bool, Unmade> {
  let Some(repository) = super::paths::git_repository_of(state, green) else {
    return Ok(false);
  };
  let key = green_key(green);
  if made_already(&state.vcs_stores, repository, key, version) {
    return Ok(false);
  }
  let message = authorship.message.as_deref().ok_or(Unmade::Authorship)?;
  let identity = authorship
    .identities
    .git
    .as_ref()
    .ok_or(Unmade::Authorship)?;
  // The commit's date is the owner's instant, read once; the commit's bytes carry it, so a replay reads no clock.
  let seconds = u64::try_from(state.clock.wall_ns().div_euclid(NANOS_PER_SECOND)).unwrap_or(0);
  let authored = (
    message,
    identity.name.as_str(),
    identity.email.as_str(),
    seconds,
  );
  made_authored(state, (repository, green, version), authored)?;
  Ok(true)
}

/// The commit of `green`'s `version` made over its kept trees as `authored`, its parent the commit before, recorded and
/// admitted: [`made`]'s and [`made_owed`]'s last steps.
fn made_authored(
  state: &mut ShardState,
  (repository, green, version): (RepositoryId, DbVolumeId, u64),
  authored: Authored<'_>,
) -> Result<(), Unmade> {
  let key = green_key(green);
  let parent = parent_of(state, repository, green);
  let objects = commit_objects(state, repository, (key, version), authored, parent)?;
  admitted(state, repository, (key, version), objects)
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
/// clock, in a transaction of its own; counted. Nothing where none is owed, or the map is of another version.
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
  state.db.begin();
  let authored = (
    owed.message.as_str(),
    owed.name.as_str(),
    owed.email.as_str(),
    owed.seconds,
  );
  let made = made_authored(state, (repository, green, owed.version), authored);
  match (made, state.db.commit(&mut state.segment)) {
    (Ok(()), Ok(_)) => state.count(MADE, 1),
    (Ok(()), Err(_)) => {
      // The store is ahead of a log that refused the entry: made again from the log, where the commit is owed still.
      super::rebuild(state, repository);
      state.count(UNMADE, 1);
    }
    (Err(_), _) => state.count(UNMADE, 1),
  }
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
