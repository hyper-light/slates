//! A git repository's greens as the history A-647's types read (A-664 part 3, its first piece; A-647's owed map;
//! §4.17.1 "A version" and "A push is a work" item 3; §4.17.2 "The repository in git's terms" item 5).
//!
//! **What it keeps.** For each version a member green of a git repository commits on the owner, the version's map of
//! paths to git's files (`slates_vcs::derived::paths::PathMap`), kept with the repository's store's derived values
//! under the green's key and the version ([`GreenAt`]), as A-647 keeps a value: content in the shard's store, named by a
//! row that never changes. A file is what git holds at the path: a regular file `100644`, or `100755` when its owner
//! may execute it (git keeps a regular file's owner execute bit alone \[C: git `read-cache.c` `ce_mode_from_stat` and
//! `cache.h` `create_ce_mode`, v2.50.1\]); a symbolic link `120000`, whose content is its target; a submodule's link
//! `160000`, whose content is the commit's id (A-637). A hard link is a file with its primary's bytes and class; a named
//! pipe, a socket or a device node is nothing git holds, and is no file of the map. The classes are
//! `slates_git::derived::FileClass`'s, so the trees A-664 part 2 derives read this map's changes as they are.
//!
//! **When.** As each version commits on the owner: a submit's, once its merge record is appended (`verbs`'
//! `record_accepted`), and a push's, once its register entry is applied (`repository::push::push_sealed`). The change
//! is read from the green, never from a client: the paths the version changed (`Green::changed_between`, the change
//! index, A-264), each set to its file at the head or, where the parent's map holds a file and the head holds none,
//! removed. So a step costs the change's paths and, in the map, their directories, never the green's tree (A-647's
//! path copying). A version no longer at the head (an idempotent resubmit's, accepted before) was stepped then.
//!
//! **Whole, where it must be.** A version's map is derived from its parent's, the previous version of the same green.
//! Where that map is not kept (a green's first version, a branch's, the first after a recovery, which keeps no derived
//! value (A-662), the first after a refused step) or the change names a directory (a directory renamed or a tree copied,
//! whose files the change index names by the directory alone), the version's map is built from the whole head instead,
//! once, and the next is derived from it.
//!
//! **Budget.** The step's contents and nodes are the content store's, charged to the shard as A-662 charges a push: lent
//! the room the shard's budget could admit now, grown first by the bytes the step brings, then trued up to what the
//! store holds. A refused step (that room, a path the map refuses, the store's own) keeps nothing, holds nothing and is
//! counted: the version's merge record is durable already, and its green's next map is built whole.
//!
//! **Counters.** [`DERIVED`] (a version's map derived from its parent's: the non-vacuity count of the incremental path),
//! [`BUILT`] (built from the whole head), [`REFUSED`].
//!
//! **Owed** (GAPS, A-664): the trees and the commit derived from these changes, admitted to the store with the green's
//! head moved and recorded with the version; a submit refused before its verdict when its post-state is one git cannot
//! hold (a named pipe, a socket, a device, a `.git` name).

use std::borrow::Cow;
use std::collections::BTreeMap;

use slates_db::catalog::{RepositoryId, VolumeId as DbVolumeId};
use slates_git::derived::FileClass;
use slates_merge::engine::Green;
use slates_merge::special::SpecialKind;
use slates_vcs::derived::paths::{Change, Entry, PathChanges, PathMap, PathsRefused};
use slates_vcs::derived::{DerivedRefused, Graph};
use slates_vcs::{ContentId, ContentRefused, ContentStore, Derivations, Mode};
use slates_vfs::volume::Store;

use super::{GitStore, RepositoryStore, VcsStores};
use crate::state::ShardState;

#[cfg(test)]
mod tests;

/// The status count of the versions whose map was derived from their parent's.
pub(crate) const DERIVED: &str = "vcs_paths.derived";
/// The status count of the versions whose map was built from the whole head.
pub(crate) const BUILT: &str = "vcs_paths.built";
/// The status count of the steps refused (module docs): nothing kept, nothing held.
pub(crate) const REFUSED: &str = "vcs_paths.refused";

/// Format: the bit of a regular file's mode that lets its owner execute it, the one permission bit git keeps (module
/// docs).
const OWNER_EXECUTE: u32 = 0o100;

/// Format: the byte a green's paths begin with, which a map's paths leave out.
const ROOT: char = '/';

/// A version of a green of a git repository, as its values are keyed: the green's key in its repository's store
/// (`vcs_store::green_key`) and the version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct GreenAt {
  /// The green's key.
  pub(crate) green: u64,
  /// The version.
  pub(crate) version: u64,
}

/// The history one step derives over: the version, its parent and its change. Every other version is the history's by
/// name alone, as a value's key reads it (A-647).
#[derive(Debug)]
pub(crate) struct GreenStep {
  /// The version.
  at: GreenAt,
  /// Its parent.
  parent: Option<GreenAt>,
  /// Its change over the parent's map.
  change: Change,
}

impl GreenStep {
  /// The history naming `at` alone: what a value's key is read through.
  pub(crate) fn named(at: GreenAt) -> GreenStep {
    GreenStep {
      at,
      parent: None,
      change: Change::new(),
    }
  }
}

impl Graph for GreenStep {
  type Version = GreenAt;

  fn parents(&self, version: GreenAt) -> Option<Vec<GreenAt>> {
    (version == self.at).then(|| self.parent.into_iter().collect())
  }

  fn name(&self, version: GreenAt, out: &mut Vec<u8>) {
    out.extend_from_slice(&version.green.to_be_bytes());
    out.extend_from_slice(&version.version.to_be_bytes());
  }
}

impl PathChanges for GreenStep {
  fn change(&self, version: GreenAt) -> Option<&Change> {
    (version == self.at).then_some(&self.change)
  }
}

/// The path map's type over a git repository's greens.
pub(crate) type GreenPaths = PathMap<GreenStep>;

/// The path map's type for a store of `window`, eager (module docs); `None` for a window too small for the map's
/// branches, which no content plane's window is.
pub(crate) fn declare(window: usize) -> Option<GreenPaths> {
  PathMap::declare(window, Mode::Eager).ok()
}

/// A file as git holds it (module docs): its class and its bytes.
#[derive(Debug)]
struct GitFile<'g> {
  /// The class.
  class: FileClass,
  /// The bytes: the file's, a link's target, a submodule commit's id.
  bytes: Cow<'g, [u8]>,
}

/// What a step does: nothing, a change over the parent's map, or a map of the whole head.
#[derive(Debug)]
enum Plan<'g> {
  /// The version's map is kept already.
  Kept,
  /// The version's change over its parent's map, the root named: each path set to its file, or removed.
  Change(ContentId, Vec<(Vec<u8>, Option<GitFile<'g>>)>),
  /// Every file of the head.
  Whole(Vec<(Vec<u8>, GitFile<'g>)>),
}

/// How a version's map was had.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stepped {
  /// Derived from its parent's.
  Derived,
  /// Built from the whole head.
  Built,
  /// Kept already.
  Kept,
}

/// Why a step was refused (module docs). Each keeps nothing and holds nothing. The shard counts a refusal ([`REFUSED`])
/// and its tests read why, so a daemon build reads no payload.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
enum StepRefused {
  /// The repository's store is not git's, or keeps no map.
  NoMap,
  /// The map's own.
  Paths(PathsRefused),
  /// The derived values' own.
  Derived(DerivedRefused<GreenAt, PathsRefused>),
  /// The content store's own.
  Content(ContentRefused),
}

/// Keeps the map of `green`'s `version`, just committed on the owner (module docs); nothing for a green of no git
/// repository, a version 0, or a version no longer at the green's head.
pub(crate) fn advance(state: &mut ShardState, green: DbVolumeId, version: u64) {
  let Some(repository) = git_repository_of(state, green) else {
    return;
  };
  let Some(previous) = version.checked_sub(1) else {
    return;
  };
  let key = super::green_key(green);
  let step = (
    GreenAt {
      green: key,
      version,
    },
    GreenAt {
      green: key,
      version: previous,
    },
  );
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
    if engine.head() != version {
      return;
    }
    stepped(&engine, (store, vcs_stores), repository, step)
  };
  if !charged_whole {
    state.count(super::CHARGE_SHORT, 1);
  }
  match answer {
    Ok(Stepped::Derived) => state.count(DERIVED, 1),
    Ok(Stepped::Built) => state.count(BUILT, 1),
    Ok(Stepped::Kept) => {}
    Err(_) => state.count(REFUSED, 1),
  }
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
  step: (GreenAt, GreenAt),
) -> (Result<Stepped, StepRefused>, bool) {
  let plan = match plan(engine, stores, repository, step) {
    Ok(plan) => plan,
    Err(refused) => return (Err(refused), true),
  };
  super::lend(store, stores, plan_bytes(&plan));
  let answer = made(stores, repository, step, plan);
  let charged_whole = super::settle_in(store, stores);
  (answer, charged_whole)
}

/// What the step does (module docs), read from the store and the head alone.
fn plan<'g>(
  engine: &'g Green,
  stores: &VcsStores,
  repository: RepositoryId,
  (at, parent): (GreenAt, GreenAt),
) -> Result<Plan<'g>, StepRefused> {
  let (content, git) = stores.git_ref(repository).ok_or(StepRefused::NoMap)?;
  let paths = git.paths.as_ref().ok_or(StepRefused::NoMap)?;
  let named = GreenStep::named(at);
  let value = |version| {
    git
      .derivations
      .value(content, paths, &named, version)
      .map_err(StepRefused::Derived)
  };
  if value(at)?.is_some() {
    return Ok(Plan::Kept);
  }
  let Some(parent_root) = value(parent)? else {
    return Ok(Plan::Whole(whole(engine)));
  };
  let changed = changed(engine, (content, paths, parent_root), (parent, at))?;
  Ok(changed.map_or_else(
    || Plan::Whole(whole(engine)),
    |files| Plan::Change(parent_root, files),
  ))
}

/// The version's change over the map `parent` (module docs): each path the version changed, set to its file at the head
/// or removed where `parent` holds a file and the head none; `None` when the change names a directory.
fn changed<'g>(
  engine: &'g Green,
  (content, paths, parent): (&ContentStore, &GreenPaths, ContentId),
  (from, to): (GreenAt, GreenAt),
) -> Result<Option<Vec<(Vec<u8>, Option<GitFile<'g>>)>>, StepRefused> {
  let mut files = Vec::new();
  for path in engine.changed_between(from.version, to.version) {
    let name = map_path(&path);
    if name.is_empty() {
      continue;
    }
    if engine.is_dir(&path) {
      return Ok(None);
    }
    if let Some(file) = git_file(engine, &path) {
      files.push((name.to_vec(), Some(file)));
    } else if held_before(content, paths, parent, name)? {
      files.push((name.to_vec(), None));
    }
  }
  Ok(Some(files))
}

/// Whether the map `parent` holds a file at `name`.
fn held_before(
  content: &ContentStore,
  paths: &GreenPaths,
  parent: ContentId,
  name: &[u8],
) -> Result<bool, StepRefused> {
  paths
    .get(content, parent, name)
    .map(|entry| entry.is_some())
    .map_err(StepRefused::Paths)
}

/// Every file git holds at the head, by its map's path (module docs).
fn whole(engine: &Green) -> Vec<(Vec<u8>, GitFile<'_>)> {
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

/// Makes the planned map and keeps it as the version's value (module docs): each content put, the map made, its root
/// kept, then every hold the step took given back, whatever the answer, so a refused step leaves the store as it was.
fn made(
  stores: &mut VcsStores,
  repository: RepositoryId,
  step: (GreenAt, GreenAt),
  plan: Plan<'_>,
) -> Result<Stepped, StepRefused> {
  let (content, git) = stores.git(repository).ok_or(StepRefused::NoMap)?;
  let GitStore {
    derivations, paths, ..
  } = git;
  let paths = paths.as_mut().ok_or(StepRefused::NoMap)?;
  let mut held = Vec::new();
  let answer = make_and_keep(content, (derivations, paths), step, plan, &mut held);
  // Every hold goes back, past a refusal too; the first refusal is the answer (as `PathMap`'s own settle gives back).
  let released = held.iter().fold(Ok(()), |first, id| {
    let this = content.release(id).map(drop);
    first.and(this)
  });
  let stepped = answer?;
  released.map_err(StepRefused::Content)?;
  Ok(stepped)
}

/// [`made`]'s work: every hold it takes is noted in `held`, for [`made`] to give back.
fn make_and_keep(
  content: &mut ContentStore,
  (derivations, paths): (&mut Derivations, &mut GreenPaths),
  (at, parent): (GreenAt, GreenAt),
  plan: Plan<'_>,
  held: &mut Vec<ContentId>,
) -> Result<Stepped, StepRefused> {
  let (source, root, stepped) = match plan {
    Plan::Kept => return Ok(Stepped::Kept),
    Plan::Change(parent_root, files) => {
      let change = put_change(content, files, held)?;
      let root = paths
        .apply(content, Some(parent_root), &change)
        .map_err(StepRefused::Paths)?;
      let source = GreenStep {
        at,
        parent: Some(parent),
        change,
      };
      (source, root, Stepped::Derived)
    }
    Plan::Whole(files) => {
      let entries = put_whole(content, files, held)?;
      let root = paths.build(content, &entries).map_err(StepRefused::Paths)?;
      (GreenStep::named(at), root, Stepped::Built)
    }
  };
  held.push(root);
  derivations
    .keep(content, &*paths, &source, at, root)
    .map_err(StepRefused::Derived)?;
  Ok(stepped)
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
