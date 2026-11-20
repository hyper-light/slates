//! A git repository's greens as the path map A-664's types read (A-664 part 3, its first piece; A-647's owed map:
//! "the green's state as the path map"; §4.17.1 "A version"; §4.17.2 "The repository in git's terms" item 5).
//!
//! **What it keeps.** For each member green of a git repository, the map of paths to git's files of its head
//! (`slates_vcs::derived::paths::PathMap`): content in the shard's store, whose root the repository's store holds once
//! with the version it is of ([`Head`]). A file is what git holds at the path: a regular file `100644`, or `100755`
//! when its owner may execute it (git keeps a regular file's owner execute bit alone \[C: git `read-cache.c`
//! `ce_mode_from_stat` and `cache.h` `create_ce_mode`, v2.50.1\]); a symbolic link `120000`, whose content is its
//! target; a submodule's link `160000`, whose content is the commit's id (A-637). A hard link is a file with its
//! primary's bytes and class; a named pipe, a socket or a device node is nothing git holds, and is no file of the map.
//! The classes are `slates_git::derived::FileClass`'s, so the trees A-664 part 2 derives read these maps' changes as
//! they are.
//!
//! **And its git trees** (A-664 part 3, its commit). Beside each map the store keeps the head's git trees
//! (`slates_git::derived::Trees`, A-664 part 2's value: a node a directory naming its tree object as git hashes it),
//! made in the same step over the same change, or built whole from the same files where the map is: the tree the
//! version's commit names (`super::commits`). A trees value is a function of the files alone, so the value derived
//! version by version is the value built from scratch, identity for identity (A-664 part 2's oracle), and a recovery
//! that builds it whole holds what the live store held.
//!
//! **The head's alone, and why.** A green's next map is derived from its head's, and nothing else reads an earlier one,
//! so the store holds one map a green, replaced as each version commits, and what two heads share is held once (path
//! copying). A recovery makes it again: A-662's rule is that a store made again from the log equals the live store
//! whole, every content with its holders, and an earlier version's map could not be made again from a log whose
//! versions a checkpoint folds; the head's is built from the green itself ([`remake_green`]). So the store holds a map
//! for each member green that committed a version, and for no other: a member's map goes as it leaves its lineage (its
//! destroy, a fence, a refused push's branch made in memory; [`forget`], from `repository::retire_member`), as a store
//! made again would hold none for it.
//!
//! **When.** As each version commits on the owner: a submit's, once its merge record is appended (`verbs`'
//! `judge_submission`, after `record_accepted`), and a push's, once its register entry is applied
//! (`repository::push::push_sealed`). The change is read from the green, never from a client: the paths the version
//! changed (`Green::changed_between`, the change index, A-264), and the hard links naming each, which read the file's
//! bytes and mode and which the index leaves out (`Green::links_naming`); each set to its file at the head or, where
//! the head's map holds a file and the new head none, removed. A directory needs nothing of its own: the map keeps a
//! directory while a file stands beneath it, and the index names each file a version makes, moves or removes. So a step
//! costs the change's paths and, in the map, their directories, never the green's tree. A version no longer at the
//! green's head (an idempotent resubmit's, accepted before) was stepped then.
//!
//! **Whole, where it must be.** Where the store keeps no map of the green's version before (a green's first version, a
//! branch's first, the first after a refused step), or a copy was made at or above a path the change names
//! (`Green::copied_after`: the index names a copy at its destination alone, standing for the tree it brought, A-586),
//! the map is built from the whole head instead, once.
//!
//! **Budget.** The step's contents and nodes are the content store's, charged to the shard as A-662 charges a push:
//! lent the room the shard's budget could admit now, grown first by the bytes the step brings, then trued up to what
//! the store holds. A refused step (that room, a path the map refuses, a tree git's strict fsck refuses, the store's
//! own) keeps nothing new, holds nothing new and is counted: neither the map nor the trees, so the next step builds both
//! whole again; the version's merge record is durable already, and its commit is not made (`super::commits`). A green
//! that committed a version keeps a map after a recovery even where a refusal had left it none.
//!
//! **Counters.** [`DERIVED`] (a map derived from the one before: the non-vacuity count of the incremental path),
//! [`BUILT`] (built from the whole head as a version commits), [`REMADE`] (built by a recovery), [`REFUSED`].
//!
//! **Sliced, in a recovery** (A-664's sliced whole builds): a recovery's remake puts the head's files one a unit of its
//! slices ([`remake_green_sliced`]), so a green of any size steps through it; the map and trees are then built of them
//! in one unit.
//!
//! **Owed** (GAPS, A-664): a submit refused before its verdict when its post-state is one git cannot hold (a named
//! pipe, a socket, a device, a `.git` name); a live whole build (a push's, a green's first submit's, past a copy) and a
//! whole map's release sliced a unit at a time, and the map's and trees' build of the put files in slices of their
//! own; and a copy made through a work, which no client verb makes yet (A-626 part 4), driven through this module's
//! whole build by a test.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use slates_db::catalog::{MemberOrigin, RepositoryId, VolumeId as DbVolumeId};
use slates_git::derived::{FileClass, Trees, TreesRefused};
use slates_git::object_name::ObjectFormat;
use slates_merge::engine::Green;
use slates_merge::special::SpecialKind;
use slates_vcs::derived::paths::{Change, Entry, PathMap, PathsRefused};
use slates_vcs::{ContentId, ContentRefused, ContentStore, Mode};
use slates_vfs::volume::Store;

use super::{GitStore, RepositoryStore, VcsStores};
use crate::state::ShardState;

// Its works and submits serve the made commits' tests too (`super::commits`, A-664 part 3).
#[cfg(test)]
pub(crate) mod tests;

/// The status count of the maps derived from the one before.
pub(crate) const DERIVED: &str = "vcs_paths.derived";
/// The status count of the maps built from the whole head as a version commits.
pub(crate) const BUILT: &str = "vcs_paths.built";
/// The status count of the maps a recovery built.
pub(crate) const REMADE: &str = "vcs_paths.remade";
/// The status count of the steps refused (module docs).
pub(crate) const REFUSED: &str = "vcs_paths.refused";

/// Format: the bit of a regular file's mode that lets its owner execute it, the one permission bit git keeps (module
/// docs).
const OWNER_EXECUTE: u32 = 0o100;

/// Format: the byte a green's paths begin with, which a map's paths leave out.
const ROOT: char = '/';

/// The history a green's maps are made over: none, since each is its head's alone (module docs) and never A-647's value
/// of a version.
#[derive(Debug)]
pub(crate) enum Heads {}

/// The path map's type over a git repository's greens.
pub(crate) type GreenPaths = PathMap<Heads>;

/// The path map's type for a store of `window` (module docs); `None` for a window too small for the map's branches,
/// which no content plane's window is.
pub(crate) fn declare(window: usize) -> Option<GreenPaths> {
  PathMap::declare(window, Mode::Eager).ok()
}

/// The git trees' type over a git repository's greens (module docs, "And its git trees").
pub(crate) type GreenTrees = Trees<Heads>;

/// The trees' type for a repository of `format` over a store of `window` (module docs); `None` for a window too small
/// for a directory's node naming one subtree, or a list of two, which no content plane's window is.
pub(crate) fn declare_trees(format: ObjectFormat, window: usize) -> Option<GreenTrees> {
  Trees::declare(format, window, Mode::Eager).ok()
}

/// A green's map and its git trees: the version they are of, and their roots, each held once by the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Head {
  /// The version.
  pub(crate) version: u64,
  /// The map's root.
  pub(crate) root: ContentId,
  /// The git trees' root: the node of the version's root tree, which its commit names (`super::commits`).
  pub(crate) trees: ContentId,
}

/// A file as git holds it (module docs): its class and its bytes.
#[derive(Debug)]
struct GitFile<'g> {
  /// The class.
  class: FileClass,
  /// The bytes: the file's, a link's target, a submodule commit's id.
  bytes: Cow<'g, [u8]>,
}

/// What a step does: nothing, a change over the green's map and trees, or the map and trees of the whole head.
#[derive(Debug)]
enum Plan<'g> {
  /// The green's map is of the version already.
  Kept,
  /// The version's change over the green's map and trees, the version before's: each path set to its file, or removed.
  Change(Head, Vec<(Vec<u8>, Option<GitFile<'g>>)>),
  /// Every file of the head.
  Whole(Vec<(Vec<u8>, GitFile<'g>)>),
  /// Every file of the head put already, a recovery's unit each ([`remake_green_sliced`]): the map and trees built of
  /// them.
  Put(BTreeMap<Vec<u8>, Entry>),
}

/// How a green's map was had.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stepped {
  /// Derived from the one before.
  Derived,
  /// Built from the whole head.
  Built,
  /// Of the version already.
  Kept,
}

/// Why a step was refused (module docs). Each keeps nothing new and holds nothing new. The shard counts a refusal
/// ([`REFUSED`]) and its tests read why, so a daemon build reads no payload.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
enum StepRefused {
  /// The repository's store is not git's, or declares no map or no trees.
  NoMap,
  /// The map's own.
  Paths(PathsRefused),
  /// The trees' own: above all a tree git's strict fsck refuses (a `.git` name).
  Trees(TreesRefused),
  /// The content store's own.
  Content(ContentRefused),
}

/// Makes `green`'s map of `version`, just committed on the owner (module docs); nothing for a green of no git
/// repository, a version 0, or a version no longer at the green's head.
pub(crate) fn advance(state: &mut ShardState, green: DbVolumeId, version: u64) {
  if version == 0 {
    return;
  }
  if let Some(answer) = step(state, green, version, false) {
    count(state, answer, BUILT);
  }
}

/// Builds the map of each member green of `repository` that committed a version, from its head (module docs): what a
/// store made again from the log holds again.
pub(crate) fn remake(state: &mut ShardState, repository: RepositoryId) {
  let greens: Vec<DbVolumeId> = state
    .db
    .partition()
    .repositories()
    .members_of(repository)
    .map(|member| member.green)
    .collect();
  for green in greens {
    remake_green(state, green);
  }
}

/// [`remake`]'s unit: `green`'s map built from its head, where it committed a version.
pub(crate) fn remake_green(state: &mut ShardState, green: DbVolumeId) {
  let Some(version) = state.greens.get(&green).map(|engine| engine.head()) else {
    return;
  };
  if version == 0 {
    return;
  }
  if let Some(answer) = step(state, green, version, true) {
    count(state, answer, REMADE);
  }
}

/// [`remake_green`] in a recovery's slices (A-664's sliced whole builds): the head's files put one a unit of the
/// recovery's ([`crate::recovery_slices`]), so a green of any size steps through its build and the watchdog sees the
/// shard step, then the map and trees built of them and kept, as [`remake_green`] keeps them. A recovery runs alone on
/// its shard, so nothing reads the green or its store between the units.
pub(crate) async fn remake_green_sliced(
  state: &mut ShardState,
  green: DbVolumeId,
  slices: &mut crate::recovery_slices::RecoverySlices,
) {
  let Some(repository) = git_repository_of(state, green) else {
    return;
  };
  let key = super::green_key(green);
  let (answer, charged_whole) = {
    let ShardState {
      store,
      vcs_stores,
      greens,
      ..
    } = &mut *state;
    let Some(engine) = greens.get(&green) else {
      return;
    };
    let version = engine.head();
    if version == 0 {
      return;
    }
    let whole = Plan::Whole(whole_head(&engine));
    super::lend(store, vcs_stores, plan_bytes(&whole));
    let Plan::Whole(files) = whole else {
      return;
    };
    let mut held = Vec::new();
    let answer = match put_sliced(vcs_stores, repository, files, &mut held, slices).await {
      Ok(entries) => made_holding(
        vcs_stores,
        repository,
        (key, version),
        Plan::Put(entries),
        held,
      ),
      Err(refused) => {
        give_back(vcs_stores, repository, &held);
        Err(refused)
      }
    };
    (answer, super::settle_in(store, vcs_stores))
  };
  if !charged_whole {
    state.count(super::CHARGE_SHORT, 1);
  }
  count(state, answer, REMADE);
}

/// Puts each of `files` in `repository`'s content store, a recovery's unit each, noting each hold.
async fn put_sliced(
  stores: &mut VcsStores,
  repository: RepositoryId,
  files: Vec<(Vec<u8>, GitFile<'_>)>,
  held: &mut Vec<ContentId>,
  slices: &mut crate::recovery_slices::RecoverySlices,
) -> Result<BTreeMap<Vec<u8>, Entry>, StepRefused> {
  let mut entries = BTreeMap::new();
  for (name, file) in files {
    let (content, _) = stores.git(repository).ok_or(StepRefused::NoMap)?;
    entries.insert(name, put_file(content, &file, held)?);
    slices.unit_done().await;
  }
  Ok(entries)
}

/// Gives back `green`'s map and trees as it leaves `repository`'s lineage (`repository::retire_member`: its destroy, a
/// fence, a refused push's branch made in memory): a store made again from the log keeps no map for a green that is no
/// member, so the live one keeps none either (module docs). Nothing where the store keeps no map for it.
pub(crate) fn forget(state: &mut ShardState, repository: RepositoryId, green: DbVolumeId) {
  let key = super::green_key(green);
  let released = {
    let Some((content, git)) = state.vcs_stores.git(repository) else {
      return;
    };
    let Some(head) = git.heads.remove(&key) else {
      return;
    };
    release_head(content, &head)
  };
  if !released {
    state.count(super::RELEASE_REFUSED, 1);
  }
  super::settle(state);
}

/// Gives back a head's two holds, its map's root and its trees': whether both went back (the second is given back past
/// a refusal of the first).
pub(crate) fn release_head(content: &mut ContentStore, head: &Head) -> bool {
  let map = content.release(&head.root).is_ok();
  content.release(&head.trees).is_ok() && map
}

/// Counts a step's answer, a build under `built`.
fn count(state: &mut ShardState, answer: Result<Stepped, StepRefused>, built: &'static str) {
  match answer {
    Ok(Stepped::Derived) => state.count(DERIVED, 1),
    Ok(Stepped::Built) => state.count(built, 1),
    Ok(Stepped::Kept) => {}
    Err(_) => state.count(REFUSED, 1),
  }
}

/// One step for `green` at `version`, its head (`whole`: built from the head whatever the green's map holds): the
/// answer, or nothing for a green of no git repository or a version not at its head.
fn step(
  state: &mut ShardState,
  green: DbVolumeId,
  version: u64,
  whole: bool,
) -> Option<Result<Stepped, StepRefused>> {
  let repository = git_repository_of(state, green)?;
  let key = super::green_key(green);
  let source = branch_source(state, green);
  let (answer, charged_whole) = {
    let ShardState {
      store,
      vcs_stores,
      greens,
      ..
    } = &mut *state;
    let engine = greens.get(&green)?;
    if engine.head() != version {
      return None;
    }
    stepped(
      &engine,
      (store, vcs_stores),
      repository,
      (key, version, whole),
      source,
    )
  };
  if !charged_whole {
    state.count(super::CHARGE_SHORT, 1);
  }
  Some(answer)
}

/// The key of the green `green` was branched from and the version it was made at, for a branch.
fn branch_source(state: &ShardState, green: DbVolumeId) -> Option<(u64, u64)> {
  match state.db.partition().repositories().member(green)?.origin {
    MemberOrigin::Branch { from, version } => Some((super::green_key(from), version)),
    MemberOrigin::Root => None,
  }
}

/// The git repository `green` is a member of, when its store is git's.
pub(super) fn git_repository_of(state: &ShardState, green: DbVolumeId) -> Option<RepositoryId> {
  let repository = state
    .db
    .partition()
    .repositories()
    .member(green)?
    .repository;
  matches!(
    state.vcs_stores.store(repository),
    Some(RepositoryStore::Git(_))
  )
  .then_some(repository)
}

/// Plans the step, lends the stores its room, makes and keeps the map, and settles (module docs): the answer, and
/// whether the shard took every byte the step left held.
fn stepped(
  engine: &Green,
  (store, stores): (&mut Store, &mut VcsStores),
  repository: RepositoryId,
  (key, version, whole): (u64, u64, bool),
  source: Option<(u64, u64)>,
) -> (Result<Stepped, StepRefused>, bool) {
  let plan = match plan(engine, stores, repository, (key, version, whole), source) {
    Ok(plan) => plan,
    Err(refused) => return (Err(refused), true),
  };
  super::lend(store, stores, plan_bytes(&plan));
  let answer = made(stores, repository, (key, version), plan);
  let charged_whole = super::settle_in(store, stores);
  (answer, charged_whole)
}

/// What the step does (module docs), read from the store and the head alone. A branch's first version with no map of
/// its own derives over its source's head when that head is the version the branch was made at (`source`: the
/// source's key and that version), since the branch's version 0 is that version (`Lineage::branch`).
fn plan<'g>(
  engine: &'g Green,
  stores: &VcsStores,
  repository: RepositoryId,
  (key, version, whole): (u64, u64, bool),
  source: Option<(u64, u64)>,
) -> Result<Plan<'g>, StepRefused> {
  let (content, git) = stores.git_ref(repository).ok_or(StepRefused::NoMap)?;
  let paths = git.paths.as_ref().ok_or(StepRefused::NoMap)?;
  if git.trees.is_none() {
    return Err(StepRefused::NoMap);
  }
  let held = git.heads.get(&key).copied();
  if !whole && held.is_some_and(|head| head.version == version) {
    return Ok(Plan::Kept);
  }
  let before = held
    .filter(|head| !whole && head.version.checked_add(1) == Some(version))
    .or_else(|| branch_before(git, held, (whole, version), source));
  let Some(before) = before else {
    return Ok(Plan::Whole(whole_head(engine)));
  };
  let changed = changed(
    engine,
    (content, paths, before.root),
    (before.version, version),
  )?;
  Ok(changed.map_or_else(
    || Plan::Whole(whole_head(engine)),
    |files| Plan::Change(before, files),
  ))
}

/// The source's head standing for a branch's version 0 (see [`plan`]): for the branch's first version, with no map of
/// its own, whose source's head is the version the branch was made at.
fn branch_before(
  git: &GitStore,
  held: Option<Head>,
  (whole, version): (bool, u64),
  source: Option<(u64, u64)>,
) -> Option<Head> {
  let (source, made_at) = source?;
  if whole || held.is_some() || version != 1 {
    return None;
  }
  let head = git.heads.get(&source).copied()?;
  (head.version == made_at).then_some(Head { version: 0, ..head })
}

/// The version's change over the map `before` (module docs): each path [`named`] gives, set to its file at the head or
/// removed where `before` holds a file and the head none; `None` when a copy names a tree by its destination alone.
fn changed<'g>(
  engine: &'g Green,
  (content, paths, before): (&ContentStore, &GreenPaths, ContentId),
  (from, to): (u64, u64),
) -> Result<Option<Vec<(Vec<u8>, Option<GitFile<'g>>)>>, StepRefused> {
  let Some(named) = named(engine, from, to) else {
    return Ok(None);
  };
  let mut files = Vec::new();
  for path in named {
    let name = map_path(&path);
    if name.is_empty() {
      continue;
    }
    if let Some(file) = git_file(engine, &path) {
      files.push((name.to_vec(), Some(file)));
    } else if held_before(content, paths, before, name)? {
      files.push((name.to_vec(), None));
    }
  }
  Ok(Some(files))
}

/// The paths whose files the versions in `(from, to]` may have changed (module docs): each path the change index names,
/// and each hard link naming one, which reads the file's bytes and mode and which the index leaves out; `None` when a
/// copy at or above a named path was made in the span, since the index names a copy at its destination alone, standing
/// for the tree it brought (A-586).
fn named(engine: &Green, from: u64, to: u64) -> Option<BTreeSet<String>> {
  let mut named = BTreeSet::new();
  for path in engine.changed_between(from, to) {
    if engine.copied_after(&path, from) {
      return None;
    }
    named.extend(engine.links_naming(&path).map(str::to_owned));
    named.insert(path);
  }
  Some(named)
}

/// Whether the map `before` holds a file at `name`.
fn held_before(
  content: &ContentStore,
  paths: &GreenPaths,
  before: ContentId,
  name: &[u8],
) -> Result<bool, StepRefused> {
  paths
    .get(content, before, name)
    .map(|entry| entry.is_some())
    .map_err(StepRefused::Paths)
}

/// Every file git holds at the head, by its map's path (module docs).
fn whole_head(engine: &Green) -> Vec<(Vec<u8>, GitFile<'_>)> {
  let names = engine
    .files()
    .map(|(path, _)| path)
    .chain(engine.symlinks().map(|(path, _)| path))
    .chain(engine.hardlinks().map(|(path, _)| path))
    .chain(engine.specials().map(|(path, _)| path));
  let mut files = BTreeMap::new();
  for path in names {
    let name = map_path(&path);
    if name.is_empty() {
      continue;
    }
    if let Some(file) = git_file(engine, &path) {
      files.insert(name.to_vec(), file);
    }
  }
  files.into_iter().collect()
}

/// The map's path of a green's path: its names, without the root's separator.
fn map_path(path: &str) -> &[u8] {
  path.trim_start_matches(ROOT).as_bytes()
}

/// The file git holds at the head's `path` (module docs): `None` for a directory, a name the green does not hold, or a
/// node git holds no file of.
fn git_file<'g>(engine: &'g Green, path: &str) -> Option<GitFile<'g>> {
  if let Some(node) = engine.special(path) {
    return gitlink(node.kind);
  }
  let primary = engine.hardlink(path).unwrap_or(path);
  if let Some(target) = engine.symlink(primary) {
    return Some(GitFile {
      class: FileClass::Symlink,
      bytes: Cow::Borrowed(target.as_bytes()),
    });
  }
  let bytes = engine.content(primary)?;
  let executable = engine
    .mode(primary)
    .is_some_and(|mode| mode & OWNER_EXECUTE != 0);
  let class = if executable {
    FileClass::Executable
  } else {
    FileClass::File
  };
  Some(GitFile {
    class,
    bytes: Cow::Borrowed(bytes),
  })
}

/// A submodule's link as git holds it; nothing for another special node (module docs).
fn gitlink<'g>(kind: SpecialKind) -> Option<GitFile<'g>> {
  match kind {
    SpecialKind::Gitlink { commit } => Some(GitFile {
      class: FileClass::Gitlink,
      bytes: Cow::Owned(commit.as_bytes().to_vec()),
    }),
    _ => None,
  }
}

/// The bytes a step brings to the store: its files'.
fn plan_bytes(plan: &Plan<'_>) -> u64 {
  let bytes = match plan {
    Plan::Kept => 0,
    Plan::Change(_, files) => files
      .iter()
      .filter_map(|(_, file)| file.as_ref())
      .map(|file| file.bytes.len())
      .fold(0, usize::saturating_add),
    Plan::Whole(files) => files
      .iter()
      .map(|(_, file)| file.bytes.len())
      .fold(0, usize::saturating_add),
    Plan::Put(_) => 0,
  };
  u64::try_from(bytes).unwrap_or(u64::MAX)
}

/// Makes the planned map and trees and keeps them as the green's (module docs): each content put, the map made, the
/// trees made over the same files, both roots kept in place of the ones before, then every other hold the step took
/// given back, whatever the answer, so a refused step leaves the store as it was.
fn made(
  stores: &mut VcsStores,
  repository: RepositoryId,
  (key, version): (u64, u64),
  plan: Plan<'_>,
) -> Result<Stepped, StepRefused> {
  made_holding(stores, repository, (key, version), plan, Vec::new())
}

/// [`made`], with the holds `held` the plan's files took already (a sliced build's, [`Plan::Put`]) given back with the
/// step's own.
fn made_holding(
  stores: &mut VcsStores,
  repository: RepositoryId,
  (key, version): (u64, u64),
  plan: Plan<'_>,
  mut held: Vec<ContentId>,
) -> Result<Stepped, StepRefused> {
  let Some((content, git)) = stores.git(repository) else {
    give_back(stores, repository, &held);
    return Err(StepRefused::NoMap);
  };
  let GitStore {
    paths,
    trees,
    heads,
    ..
  } = git;
  let (Some(paths), Some(trees)) = (paths.as_mut(), trees.as_mut()) else {
    give_back(stores, repository, &held);
    return Err(StepRefused::NoMap);
  };
  let answer = make(content, (paths, trees), plan, &mut held);
  if let Ok(Some((root, tree_root, _))) = &answer {
    let made = Head {
      version,
      root: *root,
      trees: *tree_root,
    };
    // The step's holds on the roots are the store's now; the map and trees before are given back, what they share with
    // the new ones held still.
    if let Some(before) = heads.insert(key, made) {
      held.push(before.root);
      held.push(before.trees);
    }
  }
  // Every other hold goes back, past a refusal too; the first refusal is the answer (as `PathMap`'s own settle gives).
  let released = held.iter().fold(Ok(()), |first, id| {
    let this = content.release(id).map(drop);
    first.and(this)
  });
  let stepped = answer?.map_or(Stepped::Kept, |(_, _, stepped)| stepped);
  released.map_err(StepRefused::Content)?;
  Ok(stepped)
}

/// What [`make`] made: the map's root and the trees' root, each held once for the store, and how they were had; none
/// when the green's map is of the version already.
type Made = Option<(ContentId, ContentId, Stepped)>;

/// [`made`]'s work: the new map and trees ([`Made`]); every other hold it takes is noted in `held`, for [`made`] to give
/// back.
fn make(
  content: &mut ContentStore,
  (paths, trees): (&mut GreenPaths, &mut GreenTrees),
  plan: Plan<'_>,
  held: &mut Vec<ContentId>,
) -> Result<Made, StepRefused> {
  match plan {
    Plan::Kept => Ok(None),
    Plan::Change(before, files) => {
      let change = put_change(content, files, held)?;
      let root = paths
        .apply(content, Some(before.root), &change)
        .map_err(StepRefused::Paths)?;
      let made = trees.apply(content, Some(before.trees), &change);
      with_trees(made, root, held).map(|(root, trees)| Some((root, trees, Stepped::Derived)))
    }
    Plan::Whole(files) => {
      let entries = put_whole(content, files, held)?;
      build_whole(content, (paths, trees), &entries, held)
    }
    Plan::Put(entries) => build_whole(content, (paths, trees), &entries, held),
  }
}

/// The map and trees built of every file of the head, put already.
fn build_whole(
  content: &mut ContentStore,
  (paths, trees): (&mut GreenPaths, &mut GreenTrees),
  entries: &BTreeMap<Vec<u8>, Entry>,
  held: &mut Vec<ContentId>,
) -> Result<Made, StepRefused> {
  let root = paths.build(content, entries).map_err(StepRefused::Paths)?;
  let made = trees.build(content, entries);
  with_trees(made, root, held).map(|(root, trees)| Some((root, trees, Stepped::Built)))
}

/// Gives back each hold of `held` in `repository`'s content store, as [`made`] gives back a refused step's.
fn give_back(stores: &mut VcsStores, repository: RepositoryId, held: &[ContentId]) {
  if let Some((content, _)) = stores.git(repository) {
    for id in held {
      let _ = content.release(id);
    }
  }
}

/// The map's `root` with the trees `made` over the same files; the trees refused, the map's root goes back with the
/// step's other holds, so a step keeps both or neither (module docs, "Budget").
fn with_trees(
  made: Result<ContentId, TreesRefused>,
  root: ContentId,
  held: &mut Vec<ContentId>,
) -> Result<(ContentId, ContentId), StepRefused> {
  match made {
    Ok(trees) => Ok((root, trees)),
    Err(refused) => {
      held.push(root);
      Err(StepRefused::Trees(refused))
    }
  }
}

/// Puts each file of a change, noting each hold, and gives the change the map reads.
fn put_change(
  content: &mut ContentStore,
  files: Vec<(Vec<u8>, Option<GitFile<'_>>)>,
  held: &mut Vec<ContentId>,
) -> Result<Change, StepRefused> {
  let mut change = Change::new();
  for (name, file) in files {
    let entry = file
      .map(|file| put_file(content, &file, held))
      .transpose()?;
    change.insert(name, entry);
  }
  Ok(change)
}

/// Puts each file of the whole head, noting each hold, and gives the files the map is built of.
fn put_whole(
  content: &mut ContentStore,
  files: Vec<(Vec<u8>, GitFile<'_>)>,
  held: &mut Vec<ContentId>,
) -> Result<BTreeMap<Vec<u8>, Entry>, StepRefused> {
  let mut entries = BTreeMap::new();
  for (name, file) in files {
    entries.insert(name, put_file(content, &file, held)?);
  }
  Ok(entries)
}

/// Puts `file`'s bytes as an object, as the git store keeps a blob's (A-664 part 2), noting the hold.
fn put_file(
  content: &mut ContentStore,
  file: &GitFile<'_>,
  held: &mut Vec<ContentId>,
) -> Result<Entry, StepRefused> {
  let id = content
    .put_object(&file.bytes)
    .map_err(StepRefused::Content)?;
  held.push(id);
  Ok(Entry {
    class: file.class.class(),
    content: id,
  })
}
