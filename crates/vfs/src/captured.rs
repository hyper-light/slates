//! What a clone of a captured base changed since its capture (§4.15 `capture_base` and "Plan"; A-291's landing,
//! stage 1; GAPS g267): the paths where two frozen trees of one store differ, and what each tree holds at a path, so
//! the landing of such a clone is planned as the change since its capture and judged by content
//! (`slates_land::manifest::PlanWalk::against_capture`).
//!
//! A clone of a captured snapshot shares that snapshot's nodes (D-5), so what it changed since is where its tree and the
//! snapshot's part. The inode tables name the records that differ ([`crate::trie::changed`], which never enters a node
//! the two share, so its cost follows the copied nodes, not the volume). A changed directory record names a directory
//! whose entries moved: its two nodes are compared by name, and a subtree one side holds at a name the other does not
//! (a directory made, removed or moved there) is walked whole on that side, since every path beneath it changed even
//! where no record beneath it moved (a renamed directory keeps its children's records). A changed leaf record (a file's
//! bytes or attributes, a symlink's target) is named through its home, as the deriver names one (§4.16), or by a walk
//! for an inode that has had more than one link. Skipping the directory nodes the two trees share is not enough alone:
//! a file written in place keeps its directory's node, and only its own record moves.
//!
//! The result is a set of candidate paths, each judged afterwards by what the two trees hold there ([`held_at`]); a
//! candidate both hold alike adds nothing to a landing. The cost is the changed records, the entries of each changed
//! directory, and the subtrees made, removed or moved, which a landing writes or removes path by path anyway; it is
//! stepped a record or a directory a unit ([`ChangedPaths::unit`]), so no step grows with the change (§4.3 "Bounded
//! work everywhere"), except the walk that names a changed inode with more than one link, which reads the tree once
//! (hard links are rare in the source trees a capture holds, and the deriver pays the same).
//!
//! Evidence: `crates/vfs/src/captured.rs`'s tests compare the candidates with a walk of both whole trees after every
//! kind of change, and hold that a file in place in a touched directory and a file in an untouched one are never
//! candidates (the bound is the change, not the tree).

use std::collections::{BTreeMap, BTreeSet};

use slates_mem::Handle;

use crate::dir::{Child, DirNode};
use crate::error::VfsError;
use crate::ids::{InodeNo, SnapshotId};
use crate::inode::{Body, Inode, Kind, WindowsAttributes};
use crate::names::NameEquivalence;
use crate::trie::{self, ChangedRecord, TrieNode};
use crate::volume::{Store, Volume, snapshot_handle};

/// Format: the most directories a path is built through, the bound `Volume::path_of_dir` keeps: a parent chain longer
/// than this is a cycle a corrupt tree could hold, never a path a host can name (a host path's own limit, 4,096 bytes
/// on Linux, is at most 2,048 components).
const MAX_DEPTH: u16 = u16::MAX;

/// A tree as a snapshot froze it (or a head, read in one call): its root directory node and its inode table, both in
/// the one store whose nodes a clone shares with the snapshot it was cloned from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrozenTree {
  /// The root directory node.
  pub root: Handle<DirNode>,
  /// The inode table's root.
  pub inodes: Handle<TrieNode>,
}

impl Volume {
  /// Snapshot `id`'s tree: what a landing of a captured clone is judged against when `id` is the capture, or what it
  /// lands when `id` is the clone's own snapshot.
  pub fn frozen_tree(&self, id: SnapshotId) -> Result<FrozenTree, VfsError> {
    let snapshot = self
      .snapshots
      .get(snapshot_handle(id))
      .map_err(|_| VfsError::StaleHandle)?;
    Ok(FrozenTree {
      root: snapshot.root,
      inodes: snapshot.inode_root,
    })
  }

  /// The head's tree as it stands now. Its nodes move with the next write, so a walk stepped between writes walks a
  /// snapshot instead.
  pub fn head_tree(&self) -> FrozenTree {
    FrozenTree {
      root: self.root,
      inodes: self.inode_root,
    }
  }
}

/// What a tree holds at one path, as a landing judged by content compares two trees there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Held {
  /// Nothing (a whiteout counts as nothing).
  Absent,
  /// A regular file.
  File {
    /// Its inode, to read its bytes by ([`read_file`]).
    no: InodeNo,
    /// Its size in bytes.
    size: u64,
    /// Its mode bits.
    mode: u32,
    /// Its modification time, nanoseconds.
    mtime_ns: i64,
    /// Its Windows attribute bits (A-262), which a landing carries (A-451).
    windows: WindowsAttributes,
  },
  /// A symlink.
  Symlink {
    /// Its target.
    target: Box<str>,
    /// Its Windows attribute bits, a directory link's kind among them (A-275), which a landing carries (A-451).
    windows: WindowsAttributes,
  },
  /// A directory.
  Dir {
    /// Its mode bits.
    mode: u32,
    /// Its modification time, nanoseconds.
    mtime_ns: i64,
    /// Its Windows attribute bits (A-262), which a landing carries (A-451).
    windows: WindowsAttributes,
  },
  /// A FIFO, socket or device node: a name with no bytes, which no landing writes.
  Special,
}

/// What `tree` holds at `path` (absolute from its root; `/` is the root), names compared under `policy`. A path through
/// a leaf or a missing name holds nothing.
pub fn held_at(
  store: &Store,
  tree: FrozenTree,
  policy: NameEquivalence,
  path: &str,
) -> Result<Held, VfsError> {
  let mut parts = path.split('/').filter(|part| !part.is_empty()).peekable();
  let mut dir = tree.root;
  while let Some(part) = parts.next() {
    let node = store.dirs.get(dir).map_err(|_| VfsError::StaleHandle)?;
    let Some(entry) = node.lookup(&store.blocks, policy, part) else {
      return Ok(Held::Absent);
    };
    let last = parts.peek().is_none();
    match entry.child {
      Child::Dir(child) if !last => dir = child,
      Child::Dir(child) => {
        let no = store
          .dirs
          .get(child)
          .map_err(|_| VfsError::StaleHandle)?
          .inode;
        return directory_held(store, tree, no);
      }
      Child::Whiteout => return Ok(Held::Absent),
      _ if !last => return Ok(Held::Absent),
      Child::File(no) => return file_held(store, tree, no),
      Child::Symlink(no) => return symlink_held(store, tree, no),
      Child::Fifo(_) | Child::Socket(_) | Child::BlockDevice(_) | Child::CharDevice(_) => {
        return Ok(Held::Special);
      }
    }
  }
  let root = store
    .dirs
    .get(tree.root)
    .map_err(|_| VfsError::StaleHandle)?
    .inode;
  directory_held(store, tree, root)
}

/// The bytes of file `no` in `tree` from `off`, as far as `buf` holds: how many were read.
pub fn read_file(
  store: &Store,
  tree: FrozenTree,
  no: InodeNo,
  off: u64,
  buf: &mut [u8],
) -> Result<usize, VfsError> {
  let inode = record(store, tree, no)?;
  if inode.kind != Kind::File {
    return Err(VfsError::Invalid);
  }
  crate::volume::read_frozen_body(store, inode, off, buf)
}

/// The record `tree`'s inode table holds for `no`.
fn record(store: &Store, tree: FrozenTree, no: InodeNo) -> Result<&Inode, VfsError> {
  let handle = trie::get(&store.tries, tree.inodes, no).ok_or(VfsError::NotFound)?;
  let inode = store.inodes.get(handle)?;
  if inode.no != no {
    return Err(VfsError::NotFound);
  }
  Ok(inode)
}

fn directory_held(store: &Store, tree: FrozenTree, no: InodeNo) -> Result<Held, VfsError> {
  let inode = record(store, tree, no)?;
  Ok(Held::Dir {
    mode: inode.attrs.mode,
    mtime_ns: inode.attrs.mtime,
    windows: inode.windows,
  })
}

fn file_held(store: &Store, tree: FrozenTree, no: InodeNo) -> Result<Held, VfsError> {
  let inode = record(store, tree, no)?;
  Ok(Held::File {
    no,
    size: inode.attrs.size,
    mode: inode.attrs.mode,
    mtime_ns: inode.attrs.mtime,
    windows: inode.windows,
  })
}

fn symlink_held(store: &Store, tree: FrozenTree, no: InodeNo) -> Result<Held, VfsError> {
  let inode = record(store, tree, no)?;
  match &inode.body {
    Body::Symlink(target) | Body::BaseLink(target) => Ok(Held::Symlink {
      target: target.clone(),
      windows: inode.windows,
    }),
    _ => Err(VfsError::Invalid),
  }
}

/// `name` beneath the directory path `dir`.
fn join(dir: &str, name: &str) -> String {
  if dir == "/" {
    format!("/{name}")
  } else {
    format!("{dir}/{name}")
  }
}

/// The path of directory node `node` in `tree`, through each node's own name and its parent's number resolved in
/// `tree`'s inode table; `None` when the chain breaks or runs past [`MAX_DEPTH`].
fn dir_path(store: &Store, tree: FrozenTree, node: Handle<DirNode>) -> Option<String> {
  let mut names: Vec<&str> = Vec::new();
  let mut current = node;
  loop {
    let held = store.dirs.get(current).ok()?;
    let Some(parent) = held.parent else {
      break;
    };
    if names.len() >= usize::from(MAX_DEPTH) {
      return None;
    }
    names.push(&held.name);
    let Body::Directory(up) = record(store, tree, parent).ok()?.body else {
      return None;
    };
    current = up;
  }
  names.reverse();
  Some(format!("/{}", names.join("/")))
}

/// Every path `tree` names leaf `inode` by: through its home when it never had a second link, else by a walk of the
/// whole tree; none for an inode no directory names (unlinked, or an attribute's).
fn leaf_paths(store: &Store, tree: FrozenTree, inode: &Inode) -> Vec<String> {
  if inode.attribute_of().is_some() || inode.attrs.nlink == 0 {
    return Vec::new();
  }
  if !inode.multi
    && let Some(home) = inode.home
    && let Ok(parent) = record(store, tree, home.parent())
    && let Body::Directory(dir) = parent.body
    && let Ok(node) = store.dirs.get(dir)
    && let Some(name) = node.name_of(&store.blocks, home.hash, inode.no)
    && let Some(at) = dir_path(store, tree, dir)
  {
    return vec![join(&at, name)];
  }
  paths_naming(store, tree, inode.no)
}

/// Every path in `tree` whose entry names leaf `no` (the fallback for an inode with more than one link).
fn paths_naming(store: &Store, tree: FrozenTree, no: InodeNo) -> Vec<String> {
  let mut out = Vec::new();
  let mut stack = vec![(String::from("/"), tree.root)];
  while let Some((at, dir)) = stack.pop() {
    let Ok(node) = store.dirs.get(dir) else {
      continue;
    };
    for entry in node.iter(&store.blocks) {
      match entry.child {
        Child::Dir(child) => stack.push((join(&at, entry.name), child)),
        child if child.leaf() == Some(no) => out.push(join(&at, entry.name)),
        _ => {}
      }
    }
  }
  out
}

/// A directory's entries by their names as created.
fn entries_of(store: &Store, node: &DirNode) -> BTreeMap<Box<str>, Child> {
  node
    .iter(&store.blocks)
    .map(|entry| (Box::<str>::from(entry.name), entry.child))
    .collect()
}

/// The paths where two trees of one store differ, found a unit at a time ([`ChangedPaths::unit`]): the candidates a
/// landing judged against a captured base reads both trees at.
#[derive(Debug)]
pub struct ChangedPaths {
  before: FrozenTree,
  after: FrozenTree,
  /// The records the two inode tables differ in, once read (the first unit).
  records: Option<Vec<ChangedRecord>>,
  /// The next record to judge.
  next: usize,
  /// The subtrees still to walk whole, on whichever side holds each: the path it sits at and its node.
  subtrees: Vec<(String, Handle<DirNode>)>,
  /// The candidates found so far.
  found: BTreeSet<String>,
}

impl ChangedPaths {
  /// A search for where `after` differs from `before`, from its start.
  pub fn new(before: FrozenTree, after: FrozenTree) -> ChangedPaths {
    ChangedPaths {
      before,
      after,
      records: None,
      next: 0,
      subtrees: Vec::new(),
      found: BTreeSet::new(),
    }
  }

  /// One unit: the two inode tables compared (the first unit), one directory of a subtree walked, or one changed record
  /// judged; the candidate paths, sorted, once nothing is left.
  pub fn unit(&mut self, store: &Store) -> Result<Option<Vec<String>>, VfsError> {
    let Some(records) = self.records.as_ref() else {
      let mut records = Vec::new();
      trie::changed(
        &store.tries,
        self.before.inodes,
        self.after.inodes,
        &mut records,
      );
      self.records = Some(records);
      return Ok(None);
    };
    if let Some((at, dir)) = self.subtrees.pop() {
      let node = store.dirs.get(dir).map_err(|_| VfsError::StaleHandle)?;
      for entry in node.iter(&store.blocks) {
        let path = join(&at, entry.name);
        if let Child::Dir(child) = entry.child {
          self.subtrees.push((path.clone(), child));
        }
        self.found.insert(path);
      }
      return Ok(None);
    }
    let Some(&(before, after)) = records.get(self.next) else {
      return Ok(Some(std::mem::take(&mut self.found).into_iter().collect()));
    };
    self.next = self.next.saturating_add(1);
    self.judge(store, before, after)?;
    Ok(None)
  }

  /// One changed record: a directory's two nodes compared by name, a leaf named on each side that holds it.
  fn judge(
    &mut self,
    store: &Store,
    before: Option<Handle<Inode>>,
    after: Option<Handle<Inode>>,
  ) -> Result<(), VfsError> {
    let before = before.map(|h| store.inodes.get(h)).transpose()?;
    let after = after.map(|h| store.inodes.get(h)).transpose()?;
    if let (Some(Body::Directory(was)), Some(Body::Directory(now))) =
      (before.map(|i| &i.body), after.map(|i| &i.body))
    {
      self.compare_directories(store, *was, *now)?;
    }
    for (tree, inode) in [(self.before, before), (self.after, after)] {
      if let Some(inode) = inode
        && inode.kind != Kind::Dir
      {
        self.found.extend(leaf_paths(store, tree, inode));
      }
    }
    Ok(())
  }

  /// The names where directory node `was` (in the before tree) and `now` (in the after tree) differ: each such name is a
  /// candidate on the side that holds it, and a directory one side holds there that the other does not hold as the same
  /// directory is walked whole on its side.
  fn compare_directories(
    &mut self,
    store: &Store,
    was: Handle<DirNode>,
    now: Handle<DirNode>,
  ) -> Result<(), VfsError> {
    if was == now {
      return Ok(());
    }
    let before_node = store.dirs.get(was).map_err(|_| VfsError::StaleHandle)?;
    let after_node = store.dirs.get(now).map_err(|_| VfsError::StaleHandle)?;
    let before_entries = entries_of(store, before_node);
    let after_entries = entries_of(store, after_node);
    let sides = [
      (
        dir_path(store, self.after, now),
        &after_entries,
        &before_entries,
      ),
      (
        dir_path(store, self.before, was),
        &before_entries,
        &after_entries,
      ),
    ];
    for (at, held, other) in sides {
      let Some(at) = at else {
        continue;
      };
      for (name, child) in held {
        let across = other.get(name).copied();
        if across == Some(*child) {
          continue;
        }
        let path = join(&at, name);
        if let Child::Dir(node) = child
          && !same_directory(store, across, *node)
        {
          self.subtrees.push((path.clone(), *node));
        }
        self.found.insert(path);
      }
    }
    Ok(())
  }
}

/// Whether `across` (the other tree's entry at the same name) is directory `node` itself, changed in place: then its
/// own changed record compares its entries, and nothing beneath it needs walking whole.
fn same_directory(store: &Store, across: Option<Child>, node: Handle<DirNode>) -> bool {
  let Some(Child::Dir(other)) = across else {
    return false;
  };
  match (store.dirs.get(other), store.dirs.get(node)) {
    (Ok(other), Ok(node)) => other.inode == node.inode,
    _ => false,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::clock::StepClock;
  use crate::quota::Quota;
  use crate::volume::{StoreConfig, VolumeConfig};
  use slates_mem::arena::ChunkArena;
  use slates_mem::region::Region;

  /// Shape: the test store's page.
  const PAGE: usize = 4096;
  /// Shape: pages of content the test store maps (4 MiB).
  const ARENA_PAGES: usize = 1024;
  /// Shape: slots per slab in the test store.
  const CAP: usize = 1 << 14;
  /// Shape: a file's mode.
  const FILE_MODE: u32 = 0o100_644;
  /// Shape: a directory's mode.
  const DIR_MODE: u32 = 0o040_755;

  fn store() -> Store {
    let mut arena = ChunkArena::new(PAGE);
    arena
      .add_region(Region::map(PAGE * ARENA_PAGES, PAGE, false).unwrap())
      .unwrap();
    Store::new(
      &StoreConfig {
        page: PAGE,
        cache_line: 64,
        max_dirs: CAP,
        max_inodes: CAP,
        max_chunks: CAP,
        max_dir_blocks: CAP,
        dir_cutover: 4,
      },
      arena,
      0,
    )
  }

  fn volume(store: &mut Store) -> Volume {
    Volume::create(
      store,
      VolumeConfig {
        prefix: 1,
        names: NameEquivalence::Exact,
        quota: Quota::Bounded { limit: 1 << 24 },
        journal_bytes: 1 << 16,
        clock: Box::new(StepClock::new(1_000, 1)),
      },
    )
    .unwrap()
  }

  /// The directory node at `path` in the head.
  fn dir(vol: &Volume, store: &Store, path: &str) -> Handle<DirNode> {
    match vol.resolve(store, path).unwrap().child {
      Child::Dir(handle) => handle,
      other => panic!("{path} is {other:?}, not a directory"),
    }
  }

  fn mkdir(vol: &mut Volume, store: &mut Store, parent: &str, name: &str) {
    let at = dir(vol, store, parent);
    vol.mkdir(store, at, name, DIR_MODE).unwrap();
  }

  fn file(vol: &mut Volume, store: &mut Store, parent: &str, name: &str, bytes: &[u8]) -> InodeNo {
    let at = dir(vol, store, parent);
    let no = vol.create_file(store, at, name, FILE_MODE).unwrap();
    vol.write(store, no, 0, bytes).unwrap();
    no
  }

  fn leaf(vol: &Volume, store: &Store, path: &str) -> InodeNo {
    vol.resolve(store, path).unwrap().inode
  }

  /// What a whole tree holds at every path, by a walk of all of it, a file by the BLAKE3 of its bytes as a landing
  /// judges one: the oracle the candidates are held to.
  fn whole(store: &Store, tree: FrozenTree) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut stack = vec![(String::from("/"), tree.root)];
    while let Some((at, node)) = stack.pop() {
      for entry in store.dirs.get(node).unwrap().iter(&store.blocks) {
        let path = join(&at, entry.name);
        if let Child::Dir(child) = entry.child {
          stack.push((path.clone(), child));
        }
        let held = match held_at(store, tree, NameEquivalence::Exact, &path).unwrap() {
          Held::File { no, size, .. } => {
            let mut bytes = vec![0u8; usize::try_from(size).unwrap()];
            let read = read_file(store, tree, no, 0, &mut bytes).unwrap();
            bytes.truncate(read);
            format!("file {}", blake3::hash(&bytes))
          }
          Held::Dir { .. } => "directory".to_owned(),
          Held::Symlink { target, .. } => format!("symlink {target}"),
          other => format!("{other:?}"),
        };
        out.insert(path, held);
      }
    }
    out
  }

  fn candidates(store: &Store, before: FrozenTree, after: FrozenTree) -> (Vec<String>, usize) {
    let mut walk = ChangedPaths::new(before, after);
    let mut units = 0usize;
    loop {
      units += 1;
      if let Some(found) = walk.unit(store).unwrap() {
        return (found, units);
      }
    }
  }

  /// A clone of a captured base after every kind of change: a file written in place, one made, one removed, a
  /// directory made, one removed and one renamed with a subtree beneath it, a symlink pointed elsewhere and a hard link
  /// made. Do the changes, then compare the two trees: every path whose content or kind differs is a candidate, and a
  /// file nothing touched is not, neither in an untouched directory nor beside a changed file (AC-1.12's landing of a
  /// captured clone, A-291 stage 1).
  #[test]
  fn the_candidates_hold_every_change_and_nothing_untouched() {
    let mut store = store();
    let mut origin = volume(&mut store);
    for d in ["keep", "mixed", "olddir", "removed"] {
      mkdir(&mut origin, &mut store, "/", d);
    }
    mkdir(&mut origin, &mut store, "/olddir", "inner");
    file(&mut origin, &mut store, "/keep", "a.txt", b"kept");
    file(&mut origin, &mut store, "/keep", "b.txt", b"linked later");
    file(&mut origin, &mut store, "/mixed", "untouched.txt", b"same");
    file(&mut origin, &mut store, "/mixed", "written.txt", b"before");
    file(&mut origin, &mut store, "/", "gone.txt", b"deleted later");
    file(&mut origin, &mut store, "/olddir/inner", "f.txt", b"moved");
    file(
      &mut origin,
      &mut store,
      "/removed",
      "y.txt",
      b"removed with its dir",
    );
    let root = dir(&origin, &store, "/");
    origin
      .symlink(&mut store, root, "link", "keep/a.txt")
      .unwrap();
    let captured = origin.snapshot(&mut store).unwrap();
    let mut clone =
      Volume::clone_without_base(&store, &mut origin, captured, volume_config()).unwrap();
    let base = origin.frozen_tree(captured).unwrap();

    let written = leaf(&clone, &store, "/mixed/written.txt");
    clone.write(&mut store, written, 0, b"after!").unwrap();
    file(&mut clone, &mut store, "/mixed", "new.txt", b"made");
    let root = dir(&clone, &store, "/");
    clone.unlink(&mut store, root, "gone.txt").unwrap();
    mkdir(&mut clone, &mut store, "/", "fresh");
    file(&mut clone, &mut store, "/fresh", "x.txt", b"fresh");
    let removed = dir(&clone, &store, "/removed");
    clone.unlink(&mut store, removed, "y.txt").unwrap();
    let root = dir(&clone, &store, "/");
    clone.rmdir(&mut store, root, "removed").unwrap();
    let root = dir(&clone, &store, "/");
    clone
      .rename(&mut store, root, "olddir", root, "newdir")
      .unwrap();
    let root = dir(&clone, &store, "/");
    clone.unlink(&mut store, root, "link").unwrap();
    let root = dir(&clone, &store, "/");
    clone
      .symlink(&mut store, root, "link", "keep/b.txt")
      .unwrap();
    let linked = leaf(&clone, &store, "/keep/b.txt");
    let mixed = dir(&clone, &store, "/mixed");
    clone.link(&mut store, mixed, "hard", linked).unwrap();
    let head = clone.snapshot(&mut store).unwrap();
    let after = clone.frozen_tree(head).unwrap();

    let (found, units) = candidates(&store, base, after);
    let found: BTreeSet<String> = found.into_iter().collect();
    let (was, now) = (whole(&store, base), whole(&store, after));
    let differing: BTreeSet<String> = was
      .keys()
      .chain(now.keys())
      .filter(|path| was.get(*path) != now.get(*path))
      .cloned()
      .collect();
    for path in &differing {
      assert!(
        found.contains(path),
        "{path} changed but is no candidate: {found:?}"
      );
    }
    for path in [
      "/mixed/written.txt",
      "/mixed/new.txt",
      "/gone.txt",
      "/fresh",
      "/fresh/x.txt",
      "/removed",
      "/removed/y.txt",
      "/olddir",
      "/olddir/inner",
      "/olddir/inner/f.txt",
      "/newdir",
      "/newdir/inner",
      "/newdir/inner/f.txt",
      "/link",
      "/mixed/hard",
    ] {
      assert!(found.contains(path), "{path} is no candidate: {found:?}");
    }
    for path in ["/keep", "/keep/a.txt", "/mixed/untouched.txt"] {
      assert!(
        !found.contains(path),
        "{path} changed nothing but is a candidate"
      );
    }
    // Non-vacuity: the walk stepped through records and subtrees rather than answering at once.
    assert!(units > 3, "{units} units");
  }

  /// Two trees that share every node differ nowhere: one unit reads the tables, the next answers with nothing.
  #[test]
  fn a_clone_nothing_changed_has_no_candidates() {
    let mut store = store();
    let mut origin = volume(&mut store);
    mkdir(&mut origin, &mut store, "/", "d");
    file(&mut origin, &mut store, "/d", "f", b"bytes");
    let captured = origin.snapshot(&mut store).unwrap();
    let mut clone =
      Volume::clone_without_base(&store, &mut origin, captured, volume_config()).unwrap();
    let head = clone.snapshot(&mut store).unwrap();
    let (found, units) = candidates(
      &store,
      origin.frozen_tree(captured).unwrap(),
      clone.frozen_tree(head).unwrap(),
    );
    assert!(found.is_empty(), "{found:?}");
    assert_eq!(units, 2);
  }

  /// What a tree holds at a path, each kind: a file with its size, a directory, a symlink with its target, the root,
  /// and nothing at a missing name or beneath a file.
  #[test]
  fn held_at_reads_each_kind_and_nothing_beneath_a_file() {
    let mut store = store();
    let mut vol = volume(&mut store);
    mkdir(&mut vol, &mut store, "/", "d");
    file(&mut vol, &mut store, "/d", "f", b"four");
    let root = dir(&vol, &store, "/");
    vol.symlink(&mut store, root, "s", "d/f").unwrap();
    let id = vol.snapshot(&mut store).unwrap();
    let tree = vol.frozen_tree(id).unwrap();
    let at = |path: &str| held_at(&store, tree, NameEquivalence::Exact, path).unwrap();
    assert!(matches!(at("/d/f"), Held::File { size: 4, .. }));
    assert!(matches!(at("/d"), Held::Dir { .. }));
    assert!(matches!(at("/"), Held::Dir { .. }));
    assert!(matches!(at("/s"), Held::Symlink { target, .. } if &*target == "d/f"));
    assert_eq!(at("/missing"), Held::Absent);
    assert_eq!(at("/d/f/beneath"), Held::Absent);
    let Held::File { no, .. } = at("/d/f") else {
      panic!("/d/f is no file");
    };
    let mut bytes = [0u8; 8];
    assert_eq!(read_file(&store, tree, no, 0, &mut bytes).unwrap(), 4);
    assert_eq!(&bytes[..4], b"four");
  }

  /// The clone's configuration: its own prefix, as every volume on a host has.
  fn volume_config() -> VolumeConfig {
    VolumeConfig {
      prefix: 2,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 24 },
      journal_bytes: 1 << 16,
      clock: Box::new(StepClock::new(1_000, 1)),
    }
  }
}
