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
//! the store holds. A refused step (that room, a path the map refuses, the store's own) keeps nothing new, holds
//! nothing new and is counted; the version's merge record is durable already, and the green's next map is built whole.
//! A green that committed a version keeps a map after a recovery even where a refusal had left it none.
//!
//! **Counters.** [`DERIVED`] (a map derived from the one before: the non-vacuity count of the incremental path),
//! [`BUILT`] (built from the whole head as a version commits), [`REMADE`] (built by a recovery), [`REFUSED`].
//!
//! **Owed** (GAPS, A-664): the trees and the commit derived from these changes, admitted to the store with the green's
//! head moved and recorded with the version; a submit refused before its verdict when its post-state is one git cannot
//! hold (a named pipe, a socket, a device, a `.git` name); a whole build and a whole map's release sliced a unit at a
//! time (each is one step, as A-662's replay of a record is); a branch's first map derived from its source's where the
//! source's head is the version it was made at; the holders' maps (A-636's fleet half); and a copy made through a work,
//! which no client verb makes yet (A-626 part 4), driven through this module's whole build by a test.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use slates_db::catalog::{RepositoryId, VolumeId as DbVolumeId};
use slates_git::derived::FileClass;
use slates_merge::engine::Green;
use slates_merge::special::SpecialKind;
use slates_vcs::derived::paths::{Change, Entry, PathMap, PathsRefused};
use slates_vcs::{ContentId, ContentRefused, ContentStore, Mode};
use slates_vfs::volume::Store;

use super::{GitStore, RepositoryStore, VcsStores};
use crate::state::ShardState;

#[cfg(test)]
mod tests;

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

/// A green's map: the version it is of, and its root, which the store holds once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Head {
  /// The version.
  pub(crate) version: u64,
  /// The map's root.
  pub(crate) root: ContentId,
}

/// A file as git holds it (module docs): its class and its bytes.
#[derive(Debug)]
struct GitFile<'g> {
  /// The class.
  class: FileClass,
  /// The bytes: the file's, a link's target, a submodule commit's id.
  bytes: Cow<'g, [u8]>,
}

/// What a step does: nothing, a change over the green's map, or a map of the whole head.
#[derive(Debug)]
enum Plan<'g> {
  /// The green's map is of the version already.
  Kept,
  /// The version's change over the green's map, its root named: each path set to its file, or removed.
  Change(ContentId, Vec<(Vec<u8>, Option<GitFile<'g>>)>),
  /// Every file of the head.
  Whole(Vec<(Vec<u8>, GitFile<'g>)>),
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
  /// The repository's store is not git's, or declares no map.
  NoMap,
  /// The map's own.
  Paths(PathsRefused),
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

/// Gives back `green`'s map as it leaves `repository`'s lineage (`repository::retire_member`: its destroy, a fence, a
/// refused push's branch made in memory): a store made again from the log keeps no map for a green that is no member,
/// so the live one keeps none either (module docs). Nothing where the store keeps no map for it.
pub(crate) fn forget(state: &mut ShardState, repository: RepositoryId, green: DbVolumeId) {
  let key = super::green_key(green);
  let released = {
    let Some((content, git)) = state.vcs_stores.git(repository) else {
      return;
    };
    let Some(head) = git.heads.remove(&key) else {
      return;
    };
    content.release(&head.root).is_ok()
  };
  if !released {
    state.count(super::RELEASE_REFUSED, 1);
  }
  super::settle(state);
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
    )
  };
  if !charged_whole {
    state.count(super::CHARGE_SHORT, 1);
  }
  Some(answer)
}

/// The git repository `green` is a member of, when its store is git's.
fn git_repository_of(state: &ShardState, green: DbVolumeId) -> Option<RepositoryId> {
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
) -> (Result<Stepped, StepRefused>, bool) {
  let plan = match plan(engine, stores, repository, (key, version, whole)) {
    Ok(plan) => plan,
    Err(refused) => return (Err(refused), true),
  };
  super::lend(store, stores, plan_bytes(&plan));
  let answer = made(stores, repository, (key, version), plan);
  let charged_whole = super::settle_in(store, stores);
  (answer, charged_whole)
}

/// What the step does (module docs), read from the store and the head alone.
fn plan<'g>(
  engine: &'g Green,
  stores: &VcsStores,
  repository: RepositoryId,
  (key, version, whole): (u64, u64, bool),
) -> Result<Plan<'g>, StepRefused> {
  let (content, git) = stores.git_ref(repository).ok_or(StepRefused::NoMap)?;
  let paths = git.paths.as_ref().ok_or(StepRefused::NoMap)?;
  let held = git.heads.get(&key).copied();
  if !whole && held.is_some_and(|head| head.version == version) {
    return Ok(Plan::Kept);
  }
  let before = held.filter(|head| !whole && head.version.checked_add(1) == Some(version));
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
    |files| Plan::Change(before.root, files),
  ))
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
  };
  u64::try_from(bytes).unwrap_or(u64::MAX)
}

/// Makes the planned map and keeps it as the green's (module docs): each content put, the map made, its root kept in
/// place of the map before, then every other hold the step took given back, whatever the answer, so a refused step
/// leaves the store as it was.
fn made(
  stores: &mut VcsStores,
  repository: RepositoryId,
  (key, version): (u64, u64),
  plan: Plan<'_>,
) -> Result<Stepped, StepRefused> {
  let (content, git) = stores.git(repository).ok_or(StepRefused::NoMap)?;
  let GitStore { paths, heads, .. } = git;
  let paths = paths.as_mut().ok_or(StepRefused::NoMap)?;
  let mut held = Vec::new();
  let answer = make(content, paths, plan, &mut held);
  if let Ok(Some((root, _))) = &answer {
    let made = Head {
      version,
      root: *root,
    };
    // The step's hold on the root is the store's now; the map before is given back, what the two share held still.
    if let Some(before) = heads.insert(key, made) {
      held.push(before.root);
    }
  }
  // Every other hold goes back, past a refusal too; the first refusal is the answer (as `PathMap`'s own settle gives).
  let released = held.iter().fold(Ok(()), |first, id| {
    let this = content.release(id).map(drop);
    first.and(this)
  });
  let stepped = answer?.map_or(Stepped::Kept, |(_, stepped)| stepped);
  released.map_err(StepRefused::Content)?;
  Ok(stepped)
}

/// [`made`]'s work: the new map's root, held once for the store, and how it was had, or nothing when the green's map is
/// of the version already; every other hold it takes is noted in `held`, for [`made`] to give back.
fn make(
  content: &mut ContentStore,
  paths: &mut GreenPaths,
  plan: Plan<'_>,
  held: &mut Vec<ContentId>,
) -> Result<Option<(ContentId, Stepped)>, StepRefused> {
  match plan {
    Plan::Kept => Ok(None),
    Plan::Change(before, files) => {
      let change = put_change(content, files, held)?;
      let root = paths
        .apply(content, Some(before), &change)
        .map_err(StepRefused::Paths)?;
      Ok(Some((root, Stepped::Derived)))
    }
    Plan::Whole(files) => {
      let entries = put_whole(content, files, held)?;
      let root = paths.build(content, &entries).map_err(StepRefused::Paths)?;
      Ok(Some((root, Stepped::Built)))
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
