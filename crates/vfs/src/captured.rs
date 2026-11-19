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
//!
//! **A clone that moved (A-503, stage 2).** A move hands another member the clone's head alone, and the capture stays
//! with the node that made it. So the move carries the part of the capture its clone changed: the captured state of
//! every path where the capture and the head it hands over differ ([`CarryWalk`], [`CarriedCapture`]: each file's
//! BLAKE3, mode and Windows bits, each link's target, each directory's kind, each name the capture did not hold). The
//! rest of the capture is that head itself, which the new owner keeps as a snapshot, its **mark**. A landing there is
//! judged against [`CaptureBase::Carried`]: the paths where its source parts from the mark (found as above, in one
//! store) and the carried paths are its candidates, and what the capture held at a path is the carried state, else the
//! mark's. That is the stage-1 judgement exactly, since every path the carried list leaves out held at the move what the
//! capture held. A move from the new owner carries again from its mark and its carried list ([`CaptureBase::state_at`]).
//! The carried list is bounded by what the clone changed, never by the tree, and the mark costs what the clone changes
//! after the move (it shares every node with the head until then, D-5).

use std::collections::{BTreeMap, BTreeSet};

use slates_mem::Handle;
use slates_wire::Wire;

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

/// What a capture held at one path, as a landing judged by content reads it (§4.15 "Plan"; A-291's landing, A-503): the
/// kind of entry, the BLAKE3 of a file's bytes or a link's target, and the metadata a landing carries (a file's mode and
/// Windows bits, A-451 and A-466). Read from a tree ([`state_in`]), or carried by a move ([`CarriedCapture`]).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum CapturedState {
  /// Nothing (a whiteout counts as nothing).
  Absent,
  /// A regular file.
  File {
    /// The BLAKE3 of its bytes.
    identity: [u8; 32],
    /// Its size in bytes.
    size: u64,
    /// Its mode bits.
    mode: u32,
    /// Its Windows attribute bits ([`WindowsAttributes::bits`]).
    windows: u16,
  },
  /// A symlink.
  Symlink {
    /// Its target.
    target: String,
    /// Its Windows attribute bits, a directory link's kind among them (A-275).
    windows: u16,
  },
  /// A directory.
  Dir {
    /// Its mode bits.
    mode: u32,
    /// Its Windows attribute bits.
    windows: u16,
  },
  /// A FIFO, socket or device node: a name with no bytes, which no landing writes.
  Special,
}

impl CapturedState {
  /// The identity a landing judges this state by: a file's bytes', a link's target's, and zero for anything else (a
  /// content witness's identity, `slates_land::manifest`).
  pub fn identity(&self) -> [u8; 32] {
    match self {
      CapturedState::File { identity, .. } => *identity,
      CapturedState::Symlink { target, .. } => *blake3::hash(target.as_bytes()).as_bytes(),
      CapturedState::Absent | CapturedState::Dir { .. } | CapturedState::Special => [0; 32],
    }
  }
}

/// The BLAKE3 of file `no`'s bytes in `tree` (`size` of them), read whole as a landing's plan reads a file.
pub fn file_identity(
  store: &Store,
  tree: FrozenTree,
  no: InodeNo,
  size: u64,
) -> Result<[u8; 32], VfsError> {
  let mut bytes = vec![0u8; usize::try_from(size).map_err(|_| VfsError::FileTooLarge)?];
  let read = read_file(store, tree, no, 0, &mut bytes)?;
  bytes.truncate(read);
  Ok(*blake3::hash(&bytes).as_bytes())
}

/// What `tree` holds at `path` as a [`CapturedState`], names compared under `policy`: [`held_at`] with a file's bytes
/// hashed.
pub fn state_in(
  store: &Store,
  tree: FrozenTree,
  policy: NameEquivalence,
  path: &str,
) -> Result<CapturedState, VfsError> {
  Ok(match held_at(store, tree, policy, path)? {
    Held::Absent => CapturedState::Absent,
    Held::File {
      no,
      size,
      mode,
      windows,
      ..
    } => CapturedState::File {
      identity: file_identity(store, tree, no, size)?,
      size,
      mode,
      windows: windows.bits(),
    },
    Held::Symlink { target, windows } => CapturedState::Symlink {
      target: target.into(),
      windows: windows.bits(),
    },
    Held::Dir { mode, windows, .. } => CapturedState::Dir {
      mode,
      windows: windows.bits(),
    },
    Held::Special => CapturedState::Special,
  })
}

/// Whether `tree` holds at `path` what `was` says, a file's bytes hashed only where its kind and size already agree.
fn holds(
  store: &Store,
  tree: FrozenTree,
  policy: NameEquivalence,
  path: &str,
  was: &CapturedState,
) -> Result<bool, VfsError> {
  let now = held_at(store, tree, policy, path)?;
  Ok(match (was, &now) {
    (
      CapturedState::File {
        identity,
        size,
        mode,
        windows,
      },
      Held::File {
        no,
        size: now_size,
        mode: now_mode,
        windows: now_windows,
        ..
      },
    ) => {
      size == now_size
        && mode == now_mode
        && *windows == now_windows.bits()
        && *identity == file_identity(store, tree, *no, *now_size)?
    }
    (CapturedState::Absent, Held::Absent) | (CapturedState::Special, Held::Special) => true,
    (
      CapturedState::Symlink { target, windows },
      Held::Symlink {
        target: now_target,
        windows: now_windows,
      },
    ) => target.as_str() == now_target.as_ref() && *windows == now_windows.bits(),
    (
      CapturedState::Dir { mode, windows },
      Held::Dir {
        mode: now_mode,
        windows: now_windows,
        ..
      },
    ) => mode == now_mode && *windows == now_windows.bits(),
    _ => false,
  })
}

/// One path a move carries: what the capture held there.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct CarriedPath {
  /// The path, absolute from the volume's root.
  pub path: String,
  /// What the capture held there.
  pub state: CapturedState,
}

/// The part of a clone's capture its move carries (A-503): the captured state of every path where the capture and the
/// head the move hands over differ, in path order, each path once. The rest of the capture is that head, which the new
/// owner keeps as its mark ([`CaptureBase::Carried`]). Bounded by what the clone changed, never by the tree.
#[derive(Wire, Clone, Debug, Default, PartialEq, Eq)]
pub struct CarriedCapture {
  /// The paths, in path order.
  pub entries: Vec<CarriedPath>,
}

impl CarriedCapture {
  /// What the capture held at `path`, when the move carried it.
  pub fn get(&self, path: &str) -> Option<&CapturedState> {
    self
      .entries
      .binary_search_by(|entry| entry.path.as_str().cmp(path))
      .ok()
      .and_then(|at| self.entries.get(at))
      .map(|entry| &entry.state)
  }

  /// A carried capture from the bytes another node's catalog carried (its canonical encoding, [`Wire::to_bytes`]):
  /// exactly one value, its paths absolute, made of plain names (no empty, `.` or `..` component, no NUL) and in
  /// strictly rising order; anything else is refused [`VfsError::Invalid`], since a landing writes and removes at these
  /// paths beneath its target.
  pub fn parse(bytes: &[u8]) -> Result<CarriedCapture, VfsError> {
    let carried = <CarriedCapture as Wire>::from_bytes(bytes).map_err(|_| VfsError::Invalid)?;
    let plain = |path: &str| {
      path.strip_prefix('/').is_some_and(|rest| {
        !rest.contains('\0')
          && rest
            .split('/')
            .all(|name| !name.is_empty() && name != "." && name != "..")
      })
    };
    let rising = carried
      .entries
      .windows(2)
      .all(|pair| matches!(pair, [a, b] if a.path < b.path));
    if !rising || !carried.entries.iter().all(|entry| plain(&entry.path)) {
      return Err(VfsError::Invalid);
    }
    Ok(carried)
  }
}

/// What a captured clone's landing is judged against (§4.15 "Plan"; A-291's landing): its capture, on the node that made
/// the clone (stage 1), or what a move carried of it, on the node the clone moved to (stage 2, A-503).
#[derive(Clone, Debug)]
pub enum CaptureBase {
  /// The capture itself: a snapshot's tree in the clone's own store, sharing its nodes.
  Tree(FrozenTree),
  /// A capture a move carried: the head it handed over, kept as the mark, and the captured state of every path where
  /// the capture and that head differ.
  Carried {
    /// The mark's tree: a snapshot of the clone itself.
    mark: FrozenTree,
    /// What the move carried.
    carried: CarriedCapture,
  },
}

impl From<FrozenTree> for CaptureBase {
  fn from(tree: FrozenTree) -> CaptureBase {
    CaptureBase::Tree(tree)
  }
}

impl CaptureBase {
  /// What the capture held at `path`: read from its tree, or the carried state where the move carried one, else the
  /// mark's (the head at the move held what the capture held at every path the move did not carry).
  pub fn state_at(
    &self,
    store: &Store,
    policy: NameEquivalence,
    path: &str,
  ) -> Result<CapturedState, VfsError> {
    match self {
      CaptureBase::Tree(tree) => state_in(store, *tree, policy, path),
      CaptureBase::Carried { mark, carried } => match carried.get(path) {
        Some(state) => Ok(state.clone()),
        None => state_in(store, *mark, policy, path),
      },
    }
  }

  /// The search for the paths where `source` may part from the capture: where it parts from the capture's tree, or
  /// from the mark and at every carried path.
  pub fn search(&self, source: FrozenTree) -> CaptureSearch {
    let (tree, carried) = match self {
      CaptureBase::Tree(tree) => (*tree, Vec::new()),
      CaptureBase::Carried { mark, carried } => (
        *mark,
        carried
          .entries
          .iter()
          .map(|entry| entry.path.clone())
          .collect(),
      ),
    };
    CaptureSearch {
      changed: ChangedPaths::new(tree, source),
      carried,
    }
  }
}

/// The paths where a source may part from its capture ([`CaptureBase::search`]), found a unit at a time.
#[derive(Debug)]
pub struct CaptureSearch {
  changed: ChangedPaths,
  /// The carried paths, in path order, merged in at the end.
  carried: Vec<String>,
}

impl CaptureSearch {
  /// One unit of [`ChangedPaths::unit`]; once it ends, its candidates and the carried paths, sorted, each once.
  pub fn unit(&mut self, store: &Store) -> Result<Option<Vec<String>>, VfsError> {
    let Some(found) = self.changed.unit(store)? else {
      return Ok(None);
    };
    if self.carried.is_empty() {
      return Ok(Some(found));
    }
    let mut paths: BTreeSet<String> = found.into_iter().collect();
    paths.extend(std::mem::take(&mut self.carried));
    Ok(Some(paths.into_iter().collect()))
  }
}

/// The capture a move carries for a clone (A-503): every path where `base` (the clone's capture, or what an earlier move
/// carried of it) and `head` (the head the move hands over) differ, with what the capture held there, found a unit at a
/// time: a unit of the search, then one candidate a unit (a file the capture held hashed in that unit, as a landing's
/// plan hashes one).
#[derive(Debug)]
pub struct CarryWalk {
  base: CaptureBase,
  head: FrozenTree,
  search: CaptureSearch,
  /// The candidates, once found.
  paths: Option<Vec<String>>,
  /// The next candidate.
  next: usize,
  /// What is carried so far.
  entries: Vec<CarriedPath>,
}

impl CarryWalk {
  /// A carry of `base` against `head`, from its start.
  pub fn new(base: CaptureBase, head: FrozenTree) -> CarryWalk {
    let search = base.search(head);
    CarryWalk {
      base,
      head,
      search,
      paths: None,
      next: 0,
      entries: Vec::new(),
    }
  }

  /// One unit; the carried capture once the last candidate is judged.
  pub fn unit(
    &mut self,
    store: &Store,
    policy: NameEquivalence,
  ) -> Result<Option<CarriedCapture>, VfsError> {
    let Some(paths) = self.paths.as_ref() else {
      self.paths = self.search.unit(store)?;
      return Ok(None);
    };
    let Some(path) = paths.get(self.next) else {
      return Ok(Some(CarriedCapture {
        entries: std::mem::take(&mut self.entries),
      }));
    };
    let was = self.base.state_at(store, policy, path)?;
    if !holds(store, self.head, policy, path, &was)? {
      self.entries.push(CarriedPath {
        path: path.clone(),
        state: was,
      });
    }
    self.next = self.next.saturating_add(1);
    Ok(None)
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
    assert_each_kind_held(&at);
    let Held::File { no, .. } = at("/d/f") else {
      panic!("/d/f is no file");
    };
    let mut bytes = [0u8; 8];
    assert_eq!(read_file(&store, tree, no, 0, &mut bytes).unwrap(), 4);
    assert_eq!(&bytes[..4], b"four");
  }

  /// The held-at test's tree read through `at`, each path as its kind: a file with its size, a directory, the root, a
  /// symlink with its target, and nothing at a missing name or beneath a file.
  fn assert_each_kind_held(at: &dyn Fn(&str) -> Held) {
    assert_files_and_directories_held(at);
    assert!(matches!(at("/s"), Held::Symlink { target, .. } if &*target == "d/f"));
    assert_eq!(at("/missing"), Held::Absent);
    assert_eq!(at("/d/f/beneath"), Held::Absent);
  }

  /// The held-at test's file with its size, its directory and the root, each read through `at` as its kind.
  fn assert_files_and_directories_held(at: &dyn Fn(&str) -> Held) {
    assert!(matches!(at("/d/f"), Held::File { size: 4, .. }));
    assert!(matches!(at("/d"), Held::Dir { .. }));
    assert!(matches!(at("/"), Held::Dir { .. }));
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

  /// Shape: the inode-number prefix of the volume a test's move makes on its new owner.
  const MOVED_PREFIX: u16 = 3;

  /// `from`'s whole tree made again in `to`, every directory, file and link with its mode and bytes, none of its nodes
  /// shared: the volume a move's new owner materializes from the head it was handed (A-291).
  fn copy_tree(store: &mut Store, from: FrozenTree, to: &mut Volume) {
    let mut stack = vec![(String::from("/"), from.root)];
    while let Some((at, node)) = stack.pop() {
      let entries: Vec<(String, Child)> = store
        .dirs
        .get(node)
        .unwrap()
        .iter(&store.blocks)
        .map(|entry| (entry.name.to_owned(), entry.child))
        .collect();
      for (name, child) in entries {
        let path = join(&at, &name);
        let parent = dir(to, store, &at);
        match held_at(store, from, NameEquivalence::Exact, &path).unwrap() {
          Held::Dir { mode, .. } => {
            to.mkdir(store, parent, &name, mode).unwrap();
            if let Child::Dir(child) = child {
              stack.push((path, child));
            }
          }
          Held::File { no, size, mode, .. } => {
            let mut bytes = vec![0u8; usize::try_from(size).unwrap()];
            let read = read_file(store, from, no, 0, &mut bytes).unwrap();
            bytes.truncate(read);
            let made = to.create_file(store, parent, &name, mode).unwrap();
            to.write(store, made, 0, &bytes).unwrap();
          }
          Held::Symlink { target, .. } => {
            to.symlink(store, parent, &name, &target).unwrap();
          }
          other => panic!("{path} holds {other:?}, which no test tree has"),
        }
      }
    }
  }

  /// The same change made in two volumes by path: `bytes` written over `path` (made where it is absent), or `path`
  /// removed with none.
  fn change(store: &mut Store, vols: [&mut Volume; 2], path: &str, bytes: Option<&[u8]>) {
    let (parent, name) = path.rsplit_once('/').unwrap();
    let parent = if parent.is_empty() { "/" } else { parent };
    for vol in vols {
      let at = dir(vol, store, parent);
      match bytes {
        Some(bytes) => {
          let no = match vol.resolve(store, path) {
            Ok(found) => found.inode,
            Err(_) => vol.create_file(store, at, name, FILE_MODE).unwrap(),
          };
          vol.truncate(store, no, 0).unwrap();
          vol.write(store, no, 0, bytes).unwrap();
        }
        None => vol.unlink(store, at, name).unwrap(),
      }
    }
  }

  /// A-503 (stage 2: a captured clone that moved lands as on its first runner). Do: capture a tree; in its clone write a
  /// file, make one, remove one, remove a directory with its file and point a link elsewhere; carry the capture against
  /// that head ([`CarryWalk`]); make the head again in a volume of its own (the new owner's, no node shared) and keep a
  /// snapshot of it as the mark; then make the same later changes in the clone and the moved volume: a file the capture
  /// held written, one made, a captured file removed, an earlier change undone and a file made before the move removed.
  /// Expect: what the carried capture and the mark say the capture held equals what the capture itself holds at every
  /// path of every tree; the carried list names only paths changed before the move; and the moved volume's candidates
  /// hold every path where it now parts from the capture, as the clone's do.
  #[test]
  fn a_moved_clones_carried_capture_and_mark_hold_what_its_capture_held_at_every_path() {
    let mut store = store();
    let mut origin = volume(&mut store);
    lay_out_the_origin(&mut store, &mut origin);
    let captured = origin.snapshot(&mut store).unwrap();
    let mut clone =
      Volume::clone_without_base(&store, &mut origin, captured, volume_config()).unwrap();
    let capture_tree = origin.frozen_tree(captured).unwrap();
    let capture = CaptureBase::Tree(capture_tree);
    change_the_clone_before_the_move(&mut store, &mut clone);
    let handed = clone.snapshot(&mut store).unwrap();
    let handed_tree = clone.frozen_tree(handed).unwrap();

    // The move: the capture carried against the head handed over, the head made again on the new owner, its mark kept.
    let carried = carry_and_assert_only_changes_before_the_move(&store, &capture, handed_tree);
    let mut moved = Volume::create(
      &mut store,
      VolumeConfig {
        prefix: MOVED_PREFIX,
        ..volume_config()
      },
    )
    .unwrap();
    copy_tree(&mut store, handed_tree, &mut moved);
    let mark = moved.snapshot(&mut store).unwrap();
    let carried_base = CaptureBase::Carried {
      mark: moved.frozen_tree(mark).unwrap(),
      carried,
    };

    change_both_after_the_move(&mut store, &mut clone, &mut moved);
    let clone_head = clone.snapshot(&mut store).unwrap();
    let moved_head = moved.snapshot(&mut store).unwrap();
    let (clone_tree, moved_tree) = (
      clone.frozen_tree(clone_head).unwrap(),
      moved.frozen_tree(moved_head).unwrap(),
    );
    assert_carried_base_says_what_the_capture_held(
      &store,
      (&carried_base, &capture),
      [capture_tree, handed_tree, clone_tree, moved_tree],
    );
    assert_moved_candidates_part_where_the_clone_does(
      &store,
      (&carried_base, &capture),
      (capture_tree, clone_tree, moved_tree),
    );
  }

  /// A-503's origin: the files each later change touches, a directory holding a file, and a link.
  fn lay_out_the_origin(store: &mut Store, origin: &mut Volume) {
    for d in ["src", "olddir"] {
      mkdir(origin, store, "/", d);
    }
    for (parent, name, bytes) in [
      ("/src", "edit.rs", &b"captured body"[..]),
      ("/src", "keep.rs", b"kept"),
      ("/src", "later.rs", b"written after the move"),
      ("/src", "undone.rs", b"changed, then back"),
      ("/", "gone.rs", b"removed before the move"),
      ("/", "lost.rs", b"removed after the move"),
      ("/olddir", "f.rs", b"removed with its dir"),
    ] {
      file(origin, store, parent, name, bytes);
    }
    let root = dir(origin, store, "/");
    origin.symlink(store, root, "link", "src/keep.rs").unwrap();
  }

  /// A-503's changes before the move, in the clone: a file written, one changed (undone after the move), two made, one
  /// removed, a directory removed with its file, and the link pointed elsewhere.
  fn change_the_clone_before_the_move(store: &mut Store, clone: &mut Volume) {
    let edit = leaf(clone, store, "/src/edit.rs");
    clone.truncate(store, edit, 0).unwrap();
    clone.write(store, edit, 0, b"the agent's body").unwrap();
    let undone = leaf(clone, store, "/src/undone.rs");
    clone.write(store, undone, 0, b"CHANGED").unwrap();
    file(clone, store, "/src", "made.rs", b"made before the move");
    file(clone, store, "/src", "temp.rs", b"made, then removed after");
    let root = dir(clone, store, "/");
    clone.unlink(store, root, "gone.rs").unwrap();
    let olddir = dir(clone, store, "/olddir");
    clone.unlink(store, olddir, "f.rs").unwrap();
    let root = dir(clone, store, "/");
    clone.rmdir(store, root, "olddir").unwrap();
    let root = dir(clone, store, "/");
    clone.unlink(store, root, "link").unwrap();
    let root = dir(clone, store, "/");
    clone.symlink(store, root, "link", "src/edit.rs").unwrap();
  }

  /// A-503's carry: the capture carried against the head handed over ([`CarryWalk`]) names every path changed before
  /// the move and none unchanged at it, over more units than it carries paths.
  fn carry_and_assert_only_changes_before_the_move(
    store: &Store,
    capture: &CaptureBase,
    handed_tree: FrozenTree,
  ) -> CarriedCapture {
    let mut walk = CarryWalk::new(capture.clone(), handed_tree);
    let mut units = 0usize;
    let carried = loop {
      units += 1;
      if let Some(carried) = walk.unit(store, NameEquivalence::Exact).unwrap() {
        break carried;
      }
    };
    let carried_paths: Vec<&str> = carried.entries.iter().map(|e| e.path.as_str()).collect();
    for path in [
      "/src/edit.rs",
      "/src/undone.rs",
      "/src/made.rs",
      "/src/temp.rs",
      "/gone.rs",
      "/olddir",
      "/olddir/f.rs",
      "/link",
    ] {
      assert!(
        carried_paths.contains(&path),
        "{path} changed before the move but was not carried: {carried_paths:?}"
      );
    }
    for path in ["/src/keep.rs", "/src/later.rs", "/lost.rs", "/src"] {
      assert!(
        !carried_paths.contains(&path),
        "{path} was carried, unchanged at the move"
      );
    }
    assert_eq!(carried.get("/src/made.rs"), Some(&CapturedState::Absent));
    assert!(
      units > carried.entries.len(),
      "{units} units for {} carried paths",
      carried.entries.len()
    );
    carried
  }

  /// A-503's changes after the move, made alike in the clone and the moved volume: a captured file written, one made,
  /// one removed, an earlier change undone, and a file made before the move removed.
  fn change_both_after_the_move(store: &mut Store, clone: &mut Volume, moved: &mut Volume) {
    for (path, bytes) in [
      ("/src/later.rs", Some(&b"written on the new runner"[..])),
      ("/src/new.rs", Some(b"made on the new runner")),
      ("/lost.rs", None),
      ("/src/undone.rs", Some(b"changed, then back")),
      ("/src/temp.rs", None),
    ] {
      change(store, [&mut *clone, &mut *moved], path, bytes);
    }
  }

  /// A-503's first expectation: at every path of every tree, what the carried capture and its mark say the capture held
  /// equals what the capture itself holds.
  fn assert_carried_base_says_what_the_capture_held(
    store: &Store,
    (carried_base, capture): (&CaptureBase, &CaptureBase),
    trees: [FrozenTree; 4],
  ) {
    let mut paths: BTreeSet<String> = BTreeSet::new();
    for tree in trees {
      paths.extend(whole(store, tree).into_keys());
    }
    for path in &paths {
      assert_eq!(
        carried_base
          .state_at(store, NameEquivalence::Exact, path)
          .unwrap(),
        capture
          .state_at(store, NameEquivalence::Exact, path)
          .unwrap(),
        "{path}: the carried capture and its mark say otherwise than the capture"
      );
    }
  }

  /// The paths a search of `source` against `base` names as candidates.
  fn searched_candidates(
    store: &Store,
    base: &CaptureBase,
    source: FrozenTree,
  ) -> BTreeSet<String> {
    let mut search = base.search(source);
    loop {
      if let Some(found) = search.unit(store).unwrap() {
        return found.into_iter().collect::<BTreeSet<String>>();
      }
    }
  }

  /// A-503's second expectation: the moved volume's candidates hold every path where it now parts from the capture, as
  /// the clone's do, and no path nothing touched.
  fn assert_moved_candidates_part_where_the_clone_does(
    store: &Store,
    (carried_base, capture): (&CaptureBase, &CaptureBase),
    (capture_tree, clone_tree, moved_tree): (FrozenTree, FrozenTree, FrozenTree),
  ) {
    let (was, now) = (whole(store, capture_tree), whole(store, moved_tree));
    let parted: BTreeSet<String> = was
      .keys()
      .chain(now.keys())
      .filter(|path| was.get(*path) != now.get(*path))
      .cloned()
      .collect();
    let moved_candidates = searched_candidates(store, carried_base, moved_tree);
    for path in &parted {
      assert!(
        moved_candidates.contains(path),
        "{path} parts from the capture but is no candidate: {moved_candidates:?}"
      );
    }
    assert!(
      !moved_candidates.contains("/src/keep.rs"),
      "a file nothing touched is a candidate"
    );
    let clone_whole = whole(store, clone_tree);
    assert_eq!(
      parted,
      searched_candidates(store, capture, clone_tree)
        .into_iter()
        .filter(|path| was.get(path) != clone_whole.get(path))
        .collect::<BTreeSet<String>>(),
      "the moved volume parts from the capture where the clone does"
    );
  }

  /// A-503, hostile input (CLAUDE.md §4): do: parse a carried capture's bytes as another node's catalog would bring them,
  /// well formed and then with each fault. Expect: the well-formed one back as it was; a relative path, an empty, `.` or
  /// `..` component, a NUL, paths out of order or twice, a trailing byte and a cut encoding each refused `Invalid`.
  #[test]
  fn a_carried_capture_from_another_node_is_refused_unless_well_formed() {
    let entry = |path: &str| CarriedPath {
      path: path.to_owned(),
      state: CapturedState::Absent,
    };
    let carried = CarriedCapture {
      entries: vec![entry("/a"), entry("/a/b"), entry("/c")],
    };
    let bytes = carried.to_bytes();
    assert_eq!(CarriedCapture::parse(&bytes), Ok(carried));
    for bad in [
      vec![entry("a")],
      vec![entry("/a//b")],
      vec![entry("/a/./b")],
      vec![entry("/a/../b")],
      vec![entry("/a\0b")],
      vec![entry("/b"), entry("/a")],
      vec![entry("/a"), entry("/a")],
    ] {
      let bytes = CarriedCapture {
        entries: bad.clone(),
      }
      .to_bytes();
      assert_eq!(
        CarriedCapture::parse(&bytes),
        Err(VfsError::Invalid),
        "{bad:?}"
      );
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert_eq!(CarriedCapture::parse(&trailing), Err(VfsError::Invalid));
    let cut = bytes.get(..bytes.len().saturating_sub(1)).unwrap();
    assert_eq!(CarriedCapture::parse(cut), Err(VfsError::Invalid));
  }

  /// A-503, a golden vector (CLAUDE.md §4): the carried capture is kept in partitions and shipped in catalogs, so its
  /// encoding is a format. Do: encode a carried capture of a name the capture did not hold and a link. Expect: the
  /// published bytes (each variant's tag its place in the enum, lengths `u32`, all little-endian), and the bytes back
  /// as the same value.
  #[test]
  fn a_carried_captures_encoding_is_its_golden_bytes() {
    let carried = CarriedCapture {
      entries: vec![
        CarriedPath {
          path: "/a".to_owned(),
          state: CapturedState::Absent,
        },
        CarriedPath {
          path: "/b".to_owned(),
          state: CapturedState::Symlink {
            target: "c".to_owned(),
            windows: WindowsAttributes::LINK_DIRECTORY,
          },
        },
      ],
    };
    let golden: &[u8] = &[
      2, 0, 0, 0, // two paths
      2, 0, 0, 0, b'/', b'a', // "/a"
      0, 0, 0, 0, // Absent
      2, 0, 0, 0, b'/', b'b', // "/b"
      2, 0, 0, 0, // Symlink
      1, 0, 0, 0, b'c', // its target
      0x10, 0, // its directory-link bit
    ];
    assert_eq!(carried.to_bytes(), golden);
    assert_eq!(CarriedCapture::parse(golden), Ok(carried));
  }
}
