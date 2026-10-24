//! A volume's disk-image form (§4.6, A-160): the volume's side of the daemon's `image build` and `image merge`, each
//! in steps a caller bounds by bytes (§4.3). The format itself is `slates-image`'s; this module walks snapshots into
//! its trees, feeds its writer, reads its images out of volume files, and merges an image's tree into a clone.
//!
//! **Build.** [`SnapshotWalk`] walks a snapshot into the tree the writer takes: depth first, each directory's entries
//! in name order, so one snapshot always walks to the same tree, node for node. That is what lets a merge find the
//! build's nodes again. [`BuildIo`] hands the writer each file's bytes, read at the snapshot, and writes the image's
//! blocks into a file of the image volume. A volume over a host directory is refused: its snapshot may still read
//! the disk, and an image of it would freeze whatever the disk held while the walk ran.
//!
//! **Merge.** A guest writes the image. [`Merge`] makes a clone of the build's snapshot equal to the tree the image
//! then holds. It is not an inferred merge (D-27): the clone starts at the snapshot the image was built from,
//! nothing else writes it, and the image's tree becomes its state. What the merge preserves is identity. An inode
//! at its written number that still carries the build's generation ([`slates_image::format::BUILT_GENERATION`]) is
//! the build's node `ino - 10`, renamed or rewritten perhaps. A guest's kernel gives every inode it allocates a
//! generation of its own, so a new file never passes for an old one but at a chance of one in 2³². The build's node
//! keeps its inode in the clone: a renamed file is renamed, not copied, and an unchanged file's chunks stay shared.
//! Identity is only ever a hint: a node taken for the wrong one still ends with the image's names, attributes and
//! bytes, since every one of those is compared and set.
//!
//! The merge runs in phases, each an item a step:
//! 1. **identify** each image node with the build's node, where it is one;
//! 2. **plan**: walk the base's entries; an entry the image keeps (the same name in the same directory, naming the
//!    same node) stays; one whose name the image gives to another node is moved aside, into a staging directory made
//!    in the clone's root under a name neither tree's root uses, to free the name; every other one is noted where it
//!    is;
//! 3. **attach**: walk the image's tree from the root, breadth first, so every directory is in its place before its
//!    entries are; an entry not kept is made by renaming one of its node's noted names (a directory has one), by
//!    linking a node that has none left, or by creating a node the base never had;
//! 4. **remove** every noted name left: every name of a node that is not a directory first, then the directories,
//!    from the last walked down, so each directory is empty when it goes. (A file is walked where an entry first names
//!    it, so its other names may lie in directories walked after it: taken node by node from the last down, a removed
//!    directory still held such a link, and its removal was refused, 2026-10-24.)
//! 5. **unstage**: remove the staging directory, if one was made, empty by then;
//! 6. **settle** each node: a file's size and the pages whose bytes differ, then its permissions, owner and times
//!    (the change time set from the image), last, after every change that would move them.
//!
//! A merge of an image no one changed makes no change to the clone: every entry is kept, no staging directory is
//! made, and every page and attribute compares equal.
//!
//! Cost: the merge reads every file of the image and its counterpart in the clone, a page (the store's granule) a
//! step, and writes only the pages that differ; a chunk none of whose pages differs is never written, and stays
//! shared. The page is the unit, not the chunk, so a step stays small: comparing a whole 256 KiB chunk took about
//! 40 µs on macOS arm64 (2026-10-24, `examples/image_bench.rs`), longer than a daemon's slice. The namespace work is
//! proportional to the entries, each a step.
//!
//! Evidence: `tests/image.rs` builds images of generated volumes and reads them back equal, merges generated guest
//! histories (renames, rewrites, hard links, swaps, kind changes, moves of whole directories) into clones held to the
//! image's tree and to every build node's inode, and merges images whose inodes were all renumbered (identity
//! misplaced throughout) to the same tree.

use std::collections::{BTreeMap, VecDeque};

use slates_image::format::{BUILT_GENERATION, FIRST_INO};
use slates_image::{
  Failed, ImageIo, ImageRefusal, ImageSource, ImageTree, Kind as ImageKind, Node, NodeId,
  Timestamp, Tree, TreeGrower,
};
use slates_mem::Handle;

use crate::dir::{Child, DirNode};
use crate::error::VfsError;
use crate::ids::{InodeNo, SnapshotId};
use crate::inode::{Attrs, Kind};
use crate::names::NameEquivalence;
use crate::volume::{Store, Volume};

/// Format: nanoseconds in a second, between a volume's times (nanoseconds since the epoch) and an image's.
const NANOS_PER_SECOND: i64 = 1_000_000_000;
/// Format: the permission bits of a mode (POSIX: the user, group and other bits with set-user-id, set-group-id and
/// sticky).
const PERMISSION_BITS: u32 = 0o7777;
/// Format: the name of a merge's staging directory in the clone's root: this prefix, then the first counter from zero
/// up whose name neither tree's root holds.
const STAGING_PREFIX: &str = ".slates-merge-";
/// Format: the name an entry moved aside takes in the staging directory: this prefix and a counter.
const STAGED_PREFIX: &str = "s";
/// Format: the staging directory's permissions: its owner's alone, for the moment it exists in a clone no one else
/// sees.
const STAGING_PERMISSIONS: u32 = 0o700;

/// Why a walk, a build or a merge stopped: the closed set of its refusals.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageError {
  /// The image format refused: a tree it cannot hold, or an image it cannot trust.
  Image(ImageRefusal),
  /// The volume refused an operation: its quota, its names' rules, a stale snapshot.
  Volume(VfsError),
  /// The volume is an overlay over a host directory, whose snapshot may still read the disk.
  OverHostDirectory,
  /// A name in the image is not UTF-8, which every name a volume holds is.
  NameNotUtf8 {
    /// The path of the entry, its bytes shown lossily.
    path: String,
  },
  /// A symbolic link's target in the image is not UTF-8.
  TargetNotUtf8 {
    /// The path of the link.
    path: String,
  },
  /// The image holds a node a volume cannot: a character or block device.
  NodeUnsupported {
    /// The path of the node.
    path: String,
  },
  /// The merge's own bookkeeping disagreed with itself: a defect, refused rather than guessed around.
  Inconsistent {
    /// What disagreed, in words.
    reason: &'static str,
  },
}

impl From<VfsError> for ImageError {
  fn from(error: VfsError) -> ImageError {
    ImageError::Volume(error)
  }
}

impl From<ImageRefusal> for ImageError {
  fn from(refusal: ImageRefusal) -> ImageError {
    ImageError::Image(refusal)
  }
}

impl From<Failed<VfsError>> for ImageError {
  fn from(failed: Failed<VfsError>) -> ImageError {
    match failed {
      Failed::Image(refusal) => ImageError::Image(refusal),
      Failed::Source(error) => ImageError::Volume(error),
    }
  }
}

/// A snapshot's tree as the image writer takes it, with the volume inode each node is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotTree {
  /// The tree.
  pub tree: Tree,
  /// For each node, by index, its inode in the volume.
  pub inodes: Vec<InodeNo>,
}

/// One entry of the directory a [`SnapshotWalk`] is reading.
#[derive(Debug)]
struct Row {
  name: String,
  child: Child,
}

/// The walk of a snapshot into the tree the image writer takes, in steps a caller bounds. Directories come in the order
/// a depth-first walk meets them and each one's entries in name order, so one snapshot always walks to the same tree,
/// node for node. A file with more than one link is one node, named by each entry.
#[derive(Debug)]
pub struct SnapshotWalk {
  snapshot: SnapshotId,
  /// The tree so far; taken when the walk ends.
  grower: Option<TreeGrower>,
  inodes: Vec<InodeNo>,
  /// The node each inode with more than one link became.
  linked: BTreeMap<InodeNo, NodeId>,
  /// Directories named and still to read.
  pending: Vec<(Handle<DirNode>, NodeId)>,
  /// The entries of the directory being read, still to add.
  reading: Option<VecDeque<Row>>,
  /// The entries read so far, held to `most`.
  entries: usize,
  most: usize,
}

impl SnapshotWalk {
  /// A walk of snapshot `snapshot` of `volume`, its entries held to `most`.
  pub fn new(
    volume: &Volume,
    store: &Store,
    snapshot: SnapshotId,
    most: usize,
  ) -> Result<SnapshotWalk, ImageError> {
    if volume.base_plane().is_some() {
      return Err(ImageError::OverHostDirectory);
    }
    let (_, root) = volume.snapshot_info(snapshot)?;
    let root_no = store
      .dirs
      .get(root)
      .map_err(|_| VfsError::StaleHandle)?
      .inode;
    let attrs = volume.stat_in(store, snapshot, root_no)?;
    let grower = TreeGrower::new(node_of(ImageKind::Directory, &attrs))?;
    Ok(SnapshotWalk {
      snapshot,
      grower: Some(grower),
      inodes: vec![root_no],
      linked: BTreeMap::new(),
      pending: vec![(root, NodeId::ROOT)],
      reading: None,
      entries: 0,
      most,
    })
  }

  /// Walks until at least `budget` bytes are handled (names read, attributes and targets copied) or the tree is
  /// whole: at least one entry, or one directory opened or closed, a step. The tree once whole.
  pub fn step(
    &mut self,
    volume: &Volume,
    store: &Store,
    budget: u64,
  ) -> Result<Option<SnapshotTree>, ImageError> {
    let mut spent = 0u64;
    loop {
      match self.advance(volume, store)? {
        Some(cost) => {
          spent = spent.saturating_add(cost.max(1));
          if spent >= budget {
            return Ok(None);
          }
        }
        None => return self.finish().map(Some),
      }
    }
  }

  /// The tree, once every directory is read.
  fn finish(&mut self) -> Result<SnapshotTree, ImageError> {
    let grower = self.grower.take().ok_or(ImageRefusal::GrowthOutOfOrder {
      reason: "a walk stepped after its end",
    })?;
    Ok(SnapshotTree {
      tree: grower.finish()?,
      inodes: std::mem::take(&mut self.inodes),
    })
  }

  /// One unit: an entry of the directory being read, its close, or the next directory's open. Its cost in bytes, or
  /// `None` once nothing is left.
  fn advance(&mut self, volume: &Volume, store: &Store) -> Result<Option<u64>, ImageError> {
    let grower = self.grower.as_mut().ok_or(ImageRefusal::GrowthOutOfOrder {
      reason: "a walk stepped after its end",
    })?;
    if let Some(rows) = self.reading.as_mut() {
      if let Some(row) = rows.pop_front() {
        return self.add_entry(volume, store, row).map(Some);
      }
      grower.close()?;
      self.reading = None;
      return Ok(Some(0));
    }
    let Some((directory, id)) = self.pending.pop() else {
      return Ok(None);
    };
    grower.open(id)?;
    let node = store
      .dirs
      .get(directory)
      .map_err(|_| VfsError::StaleHandle)?;
    let mut rows: Vec<Row> = node
      .iter(&store.blocks)
      .filter(|entry| !matches!(entry.child, Child::Whiteout))
      .map(|entry| Row {
        name: entry.name.to_owned(),
        child: entry.child,
      })
      .collect();
    rows.sort_by(|left, right| left.name.as_bytes().cmp(right.name.as_bytes()));
    let cost = rows
      .iter()
      .map(|row| u64::try_from(row.name.len()).unwrap_or(u64::MAX))
      .fold(0u64, u64::saturating_add);
    self.reading = Some(rows.into());
    Ok(Some(cost))
  }

  /// Adds the entry `row` to the directory being read, making its node the first time an entry names it. Its cost: the
  /// name, the attributes, and a symlink's target.
  fn add_entry(&mut self, volume: &Volume, store: &Store, row: Row) -> Result<u64, ImageError> {
    self.entries = self.entries.saturating_add(1);
    if self.entries > self.most {
      return Err(ImageError::Image(ImageRefusal::TreeTooLarge {
        most: self.most,
      }));
    }
    let grower = self.grower.as_mut().ok_or(ImageRefusal::GrowthOutOfOrder {
      reason: "a walk stepped after its end",
    })?;
    let mut cost =
      u64::try_from(row.name.len().saturating_add(size_of::<Attrs>())).unwrap_or(u64::MAX);
    let (no, directory) = match row.child {
      Child::Dir(handle) => (
        store
          .dirs
          .get(handle)
          .map_err(|_| VfsError::StaleHandle)?
          .inode,
        Some(handle),
      ),
      Child::File(no) | Child::Symlink(no) | Child::Fifo(no) | Child::Socket(no) => (no, None),
      Child::Whiteout => return Ok(cost),
    };
    let known = self.linked.get(&no).copied();
    let id = match known {
      Some(id) => id,
      None => {
        let attrs = volume.stat_in(store, self.snapshot, no)?;
        let kind = match row.child {
          Child::Dir(_) => ImageKind::Directory,
          Child::File(_) => ImageKind::File { size: attrs.size },
          Child::Symlink(_) => {
            let target = volume.readlink_in(store, self.snapshot, no)?;
            cost = cost.saturating_add(u64::try_from(target.len()).unwrap_or(u64::MAX));
            ImageKind::Symlink {
              target: target.as_bytes().to_vec(),
            }
          }
          Child::Fifo(_) => ImageKind::Fifo,
          Child::Socket(_) => ImageKind::Socket,
          Child::Whiteout => return Ok(cost),
        };
        let id = grower.add_node(node_of(kind, &attrs))?;
        self.inodes.push(no);
        if let Some(handle) = directory {
          self.pending.push((handle, id));
        } else if attrs.nlink > 1 {
          self.linked.insert(no, id);
        }
        id
      }
    };
    grower.add_entry(row.name.into_bytes(), id)?;
    Ok(cost)
  }
}

/// The image node of `kind` with a volume inode's attributes.
fn node_of(kind: ImageKind, attrs: &Attrs) -> Node {
  Node {
    kind,
    permissions: u16::try_from(attrs.mode & PERMISSION_BITS).unwrap_or(0),
    uid: attrs.uid,
    gid: attrs.gid,
    atime: timestamp(attrs.atime),
    mtime: timestamp(attrs.mtime),
    ctime: timestamp(attrs.ctime),
  }
}

/// A volume's time (nanoseconds since the epoch) as an image's.
fn timestamp(nanos: i64) -> Timestamp {
  Timestamp {
    secs: nanos.div_euclid(NANOS_PER_SECOND),
    nanos: u32::try_from(nanos.rem_euclid(NANOS_PER_SECOND)).unwrap_or(0),
  }
}

/// An image's time as a volume's, or `None` past what an `i64` of nanoseconds holds.
fn nanos_of(time: Timestamp) -> Option<i64> {
  time
    .secs
    .checked_mul(NANOS_PER_SECOND)?
    .checked_add(i64::from(time.nanos))
}

/// The image writer's input and output in one store: each file's bytes read at a snapshot of `source`, and the
/// image's blocks written into file `file` of `image`.
pub struct BuildIo<'a> {
  /// The store both volumes live in.
  pub store: &'a mut Store,
  /// The volume walked.
  pub source: &'a Volume,
  /// The snapshot walked.
  pub snapshot: SnapshotId,
  /// For each node of the tree being written, its inode in `source` ([`SnapshotTree::inodes`]).
  pub inodes: &'a [InodeNo],
  /// The volume the image is written into.
  pub image: &'a mut Volume,
  /// The image file in `image`, already sized to the image.
  pub file: InodeNo,
}

impl ImageIo<VfsError> for BuildIo<'_> {
  fn read_content(&mut self, node: NodeId, offset: u64, buf: &mut [u8]) -> Result<usize, VfsError> {
    let no = *self.inodes.get(node.index()).ok_or(VfsError::NotFound)?;
    self
      .source
      .read_in(self.store, self.snapshot, no, offset, buf)
  }

  fn write_image(&mut self, offset: u64, bytes: &[u8]) -> Result<(), VfsError> {
    let written = self.image.write(self.store, self.file, offset, bytes)?;
    if written != bytes.len() {
      return Err(VfsError::NoSpace);
    }
    Ok(())
  }
}

/// An image read out of a file of a volume, as a snapshot holds it, so a guest's later writes cannot change it under
/// a merge.
pub struct SnapshotFile<'a> {
  /// The volume holding the image.
  pub volume: &'a Volume,
  /// Its store.
  pub store: &'a Store,
  /// The snapshot read.
  pub snapshot: SnapshotId,
  /// The image file.
  pub file: InodeNo,
  /// The image file's length at the snapshot.
  pub len: u64,
}

impl ImageSource<VfsError> for SnapshotFile<'_> {
  fn image_len(&self) -> u64 {
    self.len
  }

  fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), VfsError> {
    let read = self
      .volume
      .read_in(self.store, self.snapshot, self.file, offset, buf)?;
    if read != buf.len() {
      // Inside the length the image had at the snapshot, a frozen file reads whole; short is a stale snapshot.
      return Err(VfsError::StaleHandle);
    }
    Ok(())
  }
}

/// Where a merge reads the bytes of the image's files: the image read out of its volume file, or a test's contents.
pub trait MergeContent {
  /// Fills `buf` with image inode `ino`'s bytes from `offset`, returning how many it filled; fewer only at the end.
  fn read(
    &mut self,
    store: &Store,
    ino: u32,
    offset: u64,
    buf: &mut [u8],
  ) -> Result<usize, ImageError>;
}

/// The bytes of an image's files, read through [`slates_image::Image`] from its volume file at a snapshot.
pub struct ImageContent<'a> {
  /// The image, opened.
  pub image: &'a mut slates_image::Image,
  /// The volume holding the image file.
  pub volume: &'a Volume,
  /// The snapshot it is read at.
  pub snapshot: SnapshotId,
  /// The image file.
  pub file: InodeNo,
  /// The image file's length at the snapshot.
  pub len: u64,
}

impl MergeContent for ImageContent<'_> {
  fn read(
    &mut self,
    store: &Store,
    ino: u32,
    offset: u64,
    buf: &mut [u8],
  ) -> Result<usize, ImageError> {
    let mut source = SnapshotFile {
      volume: self.volume,
      store,
      snapshot: self.snapshot,
      file: self.file,
      len: self.len,
    };
    Ok(self.image.read(&mut source, ino, offset, buf)?)
  }
}

/// What a merge did, counted: the report its verb returns, and the counters its tests hold non-vacuous.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MergeReport {
  /// Image nodes that are the build's nodes.
  pub kept: u64,
  /// Nodes made that the base never had.
  pub created: u64,
  /// Names given to a build's node by renaming one of its old names.
  pub moved: u64,
  /// Names given as links: a node's second name, or a build's node with no old name left.
  pub linked: u64,
  /// Base nodes the image no longer holds, removed.
  pub removed: u64,
  /// Old names removed: the removed nodes' and the dropped links of nodes kept.
  pub names_removed: u64,
  /// Entries moved aside to free a name the image gives to another node.
  pub staged: u64,
  /// File bytes compared with the image.
  pub bytes_compared: u64,
  /// File bytes written: the pages that differed.
  pub bytes_written: u64,
  /// Nodes whose permissions, owner or times were set.
  pub attributes_set: u64,
}

/// Where a merge is.
#[derive(Debug)]
enum Phase {
  /// Identifying image node `node` with the build's node.
  Identify { node: usize },
  /// Planning base directory `node`'s entry `entry`.
  Plan { node: usize, entry: usize },
  /// Attaching the image's entries: the directories still to visit, and the one being visited with its next entry.
  Attach {
    queue: VecDeque<usize>,
    current: Option<(usize, usize)>,
  },
  /// Removing base node `node`'s noted names: every node's that is not a directory, `node` counting up; then the
  /// directories', `node` counting down from one past the last.
  Remove { node: usize, directories: bool },
  /// Removing the staging directory.
  Unstage,
  /// Settling image node `node`: its file's page at `offset` (`None` before its size is set), then its attributes.
  Settle { node: usize, offset: Option<u64> },
  /// Done.
  Done,
}

/// The merge of an image's tree into a clone of the snapshot it was built from, in steps a caller bounds (the module
/// doc states the phases). The clone is the caller's, untouched by anything else while the merge runs.
pub struct Merge {
  base: SnapshotTree,
  target: ImageTree,
  /// For each image node, the base node it is, if it is the build's.
  identity: Vec<Option<usize>>,
  /// For each base node, the image node that is it.
  image_of: Vec<Option<usize>>,
  /// For each image node, its inode in the clone, once known.
  inode: Vec<Option<InodeNo>>,
  /// For each base node, its names the image does not keep, where they are now: a directory's inode and a name.
  old_names: Vec<Vec<(InodeNo, String)>>,
  /// For each image node, where its entries start in `kept`.
  entry_start: Vec<usize>,
  /// For each image entry, whether the base holds it already.
  kept: Vec<bool>,
  /// The staging directory, once made: its inode and its name in the clone's root.
  staging: Option<(InodeNo, String)>,
  /// The names moved aside so far, which numbers the next.
  staged_names: u64,
  /// The image directory being planned against, by base directory, and its entries' lookup when names fold.
  folded: Option<(usize, Vec<(String, usize)>)>,
  policy: NameEquivalence,
  /// The bytes compared, and written when they differ, a step: the store's granule.
  unit: u64,
  phase: Phase,
  report: MergeReport,
}

impl std::fmt::Debug for Merge {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Merge")
      .field("phase", &self.phase)
      .field("report", &self.report)
      .finish()
  }
}

impl Merge {
  /// A merge of `target` (the image's tree) into `clone`, a clone of the snapshot `base` was walked from. `trusted`
  /// says whether the image is the one the build wrote (its file system's identity is the build's); an image that is
  /// not keeps no node's identity, and is merged by names and bytes alone.
  pub fn new(
    base: SnapshotTree,
    target: ImageTree,
    clone: &Volume,
    store: &Store,
    trusted: bool,
  ) -> Merge {
    let base_nodes = base.tree.nodes().len();
    let image_nodes = target.tree.nodes().len();
    Merge {
      identity: vec![None; image_nodes],
      image_of: vec![None; base_nodes],
      inode: vec![None; image_nodes],
      old_names: vec![Vec::new(); base_nodes],
      entry_start: vec![0; image_nodes],
      kept: Vec::new(),
      staging: None,
      staged_names: 0,
      folded: None,
      policy: clone.policy,
      unit: u64::try_from(store.content.granule().max(1)).unwrap_or(u64::MAX),
      phase: if trusted {
        Phase::Identify { node: 0 }
      } else {
        Phase::Identify { node: usize::MAX }
      },
      base,
      target,
      report: MergeReport::default(),
    }
  }

  /// What the merge has done so far.
  pub fn report(&self) -> MergeReport {
    self.report
  }

  /// Works until at least `budget` bytes are handled or the clone holds the image's tree: at least one item a step.
  /// The report once done.
  pub fn step(
    &mut self,
    clone: &mut Volume,
    store: &mut Store,
    content: &mut dyn MergeContent,
    budget: u64,
  ) -> Result<Option<MergeReport>, ImageError> {
    let mut spent = 0u64;
    loop {
      if matches!(self.phase, Phase::Done) {
        return Ok(Some(self.report));
      }
      let cost = self.advance(clone, store, content)?;
      spent = spent.saturating_add(cost.max(1));
      if spent >= budget {
        return Ok(matches!(self.phase, Phase::Done).then_some(self.report));
      }
    }
  }

  /// One item of the phase the merge is in; its cost in bytes.
  fn advance(
    &mut self,
    clone: &mut Volume,
    store: &mut Store,
    content: &mut dyn MergeContent,
  ) -> Result<u64, ImageError> {
    match std::mem::replace(&mut self.phase, Phase::Done) {
      Phase::Identify { node } => self.identify(clone, store, node),
      Phase::Plan { node, entry } => self.plan(clone, store, node, entry),
      Phase::Attach { queue, current } => self.attach(clone, store, queue, current),
      Phase::Remove { node, directories } => self.remove(clone, store, node, directories),
      Phase::Unstage => self.unstage(clone, store),
      Phase::Settle { node, offset } => self.settle(clone, store, content, node, offset),
      Phase::Done => Ok(0),
    }
  }

  /// Identifies image node `node` with the build's node; after the last node, the roots with each other, where each
  /// node's entries start, and the plan. An untrusted image (`node` past every node) identifies only the roots.
  fn identify(&mut self, clone: &Volume, store: &Store, node: usize) -> Result<u64, ImageError> {
    let count = self.target.tree.nodes().len();
    if node >= count {
      // The roots are one another whatever the image is; the entries' offsets are counted for every node.
      if let (Some(slot), Some(back)) = (self.identity.first_mut(), self.image_of.first_mut()) {
        *slot = Some(0);
        *back = Some(0);
        self.report.kept = self.report.kept.saturating_add(1);
      }
      let mut start = 0usize;
      for (id, offset) in self.target.tree.ids().zip(self.entry_start.iter_mut()) {
        *offset = start;
        start = start.saturating_add(self.target.tree.children(id).len());
      }
      self.kept = vec![false; start];
      if let Some(root) = self.inode.first_mut() {
        *root = Some(clone.root_inode(store)?);
      }
      self.phase = Phase::Plan { node: 0, entry: 0 };
      return Ok(u64::try_from(count).unwrap_or(u64::MAX));
    }
    if let Some(base) = self.build_node_of(node) {
      if let Some(slot) = self.identity.get_mut(node) {
        *slot = Some(base);
      }
      if let Some(back) = self.image_of.get_mut(base) {
        *back = Some(node);
      }
      self.report.kept = self.report.kept.saturating_add(1);
    }
    self.phase = Phase::Identify {
      node: node.saturating_add(1),
    };
    Ok(1)
  }

  /// The base node image node `node` is, if it is the build's: at its written number, with the build's generation,
  /// of the same kind (a symlink with the same target, since a target is never changed in place).
  fn build_node_of(&self, node: usize) -> Option<usize> {
    let ino = *self.target.inodes.get(node)?;
    let generation = *self.target.generations.get(node)?;
    if node == 0 || generation != BUILT_GENERATION {
      return None;
    }
    let base = usize::try_from(ino.checked_sub(FIRST_INO.checked_sub(1)?)?).ok()?;
    if base == 0 {
      return None;
    }
    let image = &self.target.tree.node(self.target.tree.id(node)?)?.kind;
    let built = &self.base.tree.node(self.base.tree.id(base)?)?.kind;
    let same = match (image, built) {
      (ImageKind::Directory, ImageKind::Directory)
      | (ImageKind::File { .. }, ImageKind::File { .. })
      | (ImageKind::Fifo, ImageKind::Fifo)
      | (ImageKind::Socket, ImageKind::Socket) => true,
      (ImageKind::Symlink { target: image }, ImageKind::Symlink { target: built }) => {
        image == built
      }
      _ => false,
    };
    same.then_some(base)
  }

  /// The staging directory, made in the clone's root under the first name neither root holds the first time an entry
  /// is moved aside.
  fn staging(&mut self, clone: &mut Volume, store: &mut Store) -> Result<InodeNo, ImageError> {
    if let Some((directory, _)) = &self.staging {
      return Ok(*directory);
    }
    let root = clone.root_inode(store)?;
    let policy = self.policy;
    let taken = |name: &str| {
      let in_base = self
        .base
        .tree
        .children(NodeId::ROOT)
        .iter()
        .any(|(held, _)| std::str::from_utf8(held).is_ok_and(|held| policy.same(held, name)));
      let in_image = self
        .target
        .tree
        .children(NodeId::ROOT)
        .iter()
        .any(|(held, _)| std::str::from_utf8(held).is_ok_and(|held| policy.same(held, name)));
      in_base || in_image
    };
    // Each name either root holds rules out one counter, so the first free one is within one past their entries.
    let most = self
      .base
      .tree
      .children(NodeId::ROOT)
      .len()
      .saturating_add(self.target.tree.children(NodeId::ROOT).len())
      .saturating_add(1);
    let name = (0..most)
      .map(|counter| format!("{STAGING_PREFIX}{counter}"))
      .find(|name| !taken(name))
      .ok_or(ImageError::Inconsistent {
        reason: "no staging name free",
      })?;
    let directory = clone.mkdir_no(store, root, &name, STAGING_PERMISSIONS)?;
    self.staging = Some((directory, name));
    Ok(directory)
  }

  /// Plans base directory `node`'s entry `entry` against the image: kept, moved aside to free its name, or noted.
  fn plan(
    &mut self,
    clone: &mut Volume,
    store: &mut Store,
    node: usize,
    entry: usize,
  ) -> Result<u64, ImageError> {
    let Some(base_id) = self.base.tree.id(node) else {
      self.phase = Phase::Attach {
        queue: VecDeque::from([0]),
        current: None,
      };
      return Ok(0);
    };
    let Some((name, child)) = self.base.tree.children(base_id).get(entry).cloned() else {
      self.folded = None;
      self.phase = Phase::Plan {
        node: node.saturating_add(1),
        entry: 0,
      };
      return Ok(0);
    };
    self.phase = Phase::Plan {
      node,
      entry: entry.saturating_add(1),
    };
    let name = String::from_utf8(name).map_err(|_| ImageError::Inconsistent {
      reason: "a volume name not UTF-8",
    })?;
    let cost = u64::try_from(name.len()).unwrap_or(u64::MAX);
    let parent = *self.base.inodes.get(node).ok_or(ImageError::Inconsistent {
      reason: "a base node without an inode",
    })?;
    let Some(image_dir) = self.image_of.get(node).copied().flatten() else {
      // A directory the image no longer holds: every entry of it is noted, to be moved out or removed.
      self.note(child.index(), parent, name)?;
      return Ok(cost);
    };
    match self.image_entry(node, image_dir, &name) {
      Some((index, image_child)) => {
        if self.identity.get(image_child).copied().flatten() == Some(child.index()) {
          let slot = self
            .entry_start
            .get(image_dir)
            .copied()
            .unwrap_or(0)
            .saturating_add(index);
          if let Some(kept) = self.kept.get_mut(slot) {
            *kept = true;
          }
        } else {
          // The image gives this name to another node: the entry moves aside, freeing the name.
          let staging = self.staging(clone, store)?;
          let aside = format!("{STAGED_PREFIX}{}", self.staged_names);
          self.staged_names = self.staged_names.saturating_add(1);
          clone.rename_no(store, parent, &name, staging, &aside)?;
          self.report.staged = self.report.staged.saturating_add(1);
          self.note(child.index(), staging, aside)?;
        }
      }
      None => self.note(child.index(), parent, name)?,
    }
    Ok(cost)
  }

  /// Notes that base node `node` has an old name `name` in directory `parent`.
  fn note(&mut self, node: usize, parent: InodeNo, name: String) -> Result<(), ImageError> {
    self
      .old_names
      .get_mut(node)
      .ok_or(ImageError::Inconsistent {
        reason: "an entry naming no base node",
      })?
      .push((parent, name));
    Ok(())
  }

  /// The entry of image directory `image_dir` holding name `name` under the clone's names' rules: its index among the
  /// directory's entries and its child. Under folding, the directory's lookup is made once, when base directory
  /// `base_dir` is first planned against it.
  fn image_entry(
    &mut self,
    base_dir: usize,
    image_dir: usize,
    name: &str,
  ) -> Option<(usize, usize)> {
    let id = self.target.tree.id(image_dir)?;
    let children = self.target.tree.children(id);
    match self.policy {
      NameEquivalence::Exact => children
        .binary_search_by(|(held, _)| held.as_slice().cmp(name.as_bytes()))
        .ok()
        .and_then(|index| Some((index, children.get(index)?.1.index()))),
      NameEquivalence::Fold => {
        if self.folded.as_ref().is_none_or(|(dir, _)| *dir != base_dir) {
          let mut lookup: Vec<(String, usize)> = children
            .iter()
            .enumerate()
            .filter_map(|(index, (held, _))| {
              let held = std::str::from_utf8(held).ok()?;
              Some((self.policy.fold(held).into_owned(), index))
            })
            .collect();
          lookup.sort();
          self.folded = Some((base_dir, lookup));
        }
        let folded = self.policy.fold(name);
        let (_, lookup) = self.folded.as_ref()?;
        let at = lookup
          .binary_search_by(|(held, _)| held.as_str().cmp(folded.as_ref()))
          .ok()?;
        let index = lookup.get(at)?.1;
        Some((index, children.get(index)?.1.index()))
      }
    }
  }

  /// Attaches the next image entry: the image's directories visited from the root, breadth first.
  fn attach(
    &mut self,
    clone: &mut Volume,
    store: &mut Store,
    mut queue: VecDeque<usize>,
    current: Option<(usize, usize)>,
  ) -> Result<u64, ImageError> {
    let Some((directory, entry)) = current.or_else(|| queue.pop_front().map(|next| (next, 0)))
    else {
      self.phase = Phase::Remove {
        node: 0,
        directories: false,
      };
      return Ok(0);
    };
    let id = self
      .target
      .tree
      .id(directory)
      .ok_or(ImageError::Inconsistent {
        reason: "an image directory out of range",
      })?;
    let Some((name, child)) = self.target.tree.children(id).get(entry).cloned() else {
      self.phase = Phase::Attach {
        queue,
        current: None,
      };
      return Ok(0);
    };
    let cost = u64::try_from(name.len()).unwrap_or(u64::MAX);
    let child = child.index();
    let is_directory = matches!(
      self
        .target
        .tree
        .node(id_of(&self.target.tree, child)?)
        .map(|node| &node.kind),
      Some(ImageKind::Directory)
    );
    let slot = self
      .entry_start
      .get(directory)
      .copied()
      .unwrap_or(0)
      .saturating_add(entry);
    if self.kept.get(slot).copied().unwrap_or(false) {
      let base = self
        .identity
        .get(child)
        .copied()
        .flatten()
        .ok_or(ImageError::Inconsistent {
          reason: "a kept entry of a node not the build's",
        })?;
      let no = *self.base.inodes.get(base).ok_or(ImageError::Inconsistent {
        reason: "a base node without an inode",
      })?;
      self.set_inode(child, no);
    } else {
      self.attach_entry(clone, store, directory, &name, child)?;
    }
    if is_directory {
      queue.push_back(child);
    }
    self.phase = Phase::Attach {
      queue,
      current: Some((directory, entry.saturating_add(1))),
    };
    Ok(cost)
  }

  /// Gives image node `child` the name `name` in image directory `directory`, which the clone does not hold yet.
  fn attach_entry(
    &mut self,
    clone: &mut Volume,
    store: &mut Store,
    directory: usize,
    name: &[u8],
    child: usize,
  ) -> Result<(), ImageError> {
    let parent = self
      .inode
      .get(directory)
      .copied()
      .flatten()
      .ok_or(ImageError::Inconsistent {
        reason: "an entry attached before its directory",
      })?;
    let name = std::str::from_utf8(name).map_err(|_| ImageError::NameNotUtf8 {
      path: self.path_of(directory, name),
    })?;
    if let Some(base) = self.identity.get(child).copied().flatten() {
      let no = *self.base.inodes.get(base).ok_or(ImageError::Inconsistent {
        reason: "a base node without an inode",
      })?;
      let old = self.old_names.get_mut(base).and_then(Vec::pop);
      match old {
        Some((from, old_name)) => {
          clone.rename_no(store, from, &old_name, parent, name)?;
          self.report.moved = self.report.moved.saturating_add(1);
        }
        None => {
          let directory_node = matches!(
            self
              .base
              .tree
              .node(id_of(&self.base.tree, base)?)
              .map(|node| &node.kind),
            Some(ImageKind::Directory)
          );
          if directory_node {
            return Err(ImageError::Inconsistent {
              reason: "a directory to attach with no old name",
            });
          }
          clone.link_no(store, parent, name, no)?;
          self.report.linked = self.report.linked.saturating_add(1);
        }
      }
      self.set_inode(child, no);
      return Ok(());
    }
    if let Some(no) = self.inode.get(child).copied().flatten() {
      clone.link_no(store, parent, name, no)?;
      self.report.linked = self.report.linked.saturating_add(1);
      return Ok(());
    }
    let node = self
      .target
      .tree
      .node(id_of(&self.target.tree, child)?)
      .cloned()
      .ok_or(ImageError::Inconsistent {
        reason: "an image node out of range",
      })?;
    let permissions = u32::from(node.permissions);
    let no = match &node.kind {
      ImageKind::Directory => clone.mkdir_no(store, parent, name, permissions)?,
      ImageKind::File { .. } => clone.create_file_no(store, parent, name, permissions)?,
      ImageKind::Symlink { target } => {
        let target = std::str::from_utf8(target).map_err(|_| ImageError::TargetNotUtf8 {
          path: self.path_of(directory, name.as_bytes()),
        })?;
        clone.symlink_no(store, parent, name, target)?
      }
      ImageKind::Fifo => clone.mknod_no(store, parent, name, permissions, Kind::Fifo)?,
      ImageKind::Socket => clone.mknod_no(store, parent, name, permissions, Kind::Socket)?,
      ImageKind::CharDevice { .. } | ImageKind::BlockDevice { .. } => {
        return Err(ImageError::NodeUnsupported {
          path: self.path_of(directory, name.as_bytes()),
        });
      }
    };
    self.report.created = self.report.created.saturating_add(1);
    self.set_inode(child, no);
    Ok(())
  }

  /// Notes image node `node`'s inode in the clone.
  fn set_inode(&mut self, node: usize, no: InodeNo) {
    if let Some(slot) = self.inode.get_mut(node) {
      *slot = Some(no);
    }
  }

  /// The path of entry `name` of image directory `directory`, its bytes shown lossily: for a refusal's words.
  fn path_of(&self, directory: usize, name: &[u8]) -> String {
    let mut parts: Vec<Vec<u8>> = vec![name.to_vec()];
    let mut at = self.target.tree.id(directory);
    // A tree's depth is below its node count, so the walk up ends.
    for _ in 0..self.target.tree.nodes().len() {
      let Some(id) = at.filter(|id| *id != NodeId::ROOT) else {
        break;
      };
      let parent = self.target.tree.parent(id);
      if let Some((held, _)) = self
        .target
        .tree
        .children(parent)
        .iter()
        .find(|(_, child)| *child == id)
      {
        parts.push(held.clone());
      }
      at = Some(parent);
    }
    parts.reverse();
    let mut path = String::new();
    for part in parts {
      path.push('/');
      path.push_str(&String::from_utf8_lossy(&part));
    }
    path
  }

  /// Removes a noted name of base node `node` that is not a directory (`node` counting up), or then of directory node
  /// `node - 1` (`node` counting down), so every name inside a directory is gone before the directory goes: a
  /// directory's entries are its subdirectories, walked after it, and names of other nodes, all removed first. Then
  /// the staging directory goes.
  fn remove(
    &mut self,
    clone: &mut Volume,
    store: &mut Store,
    node: usize,
    directories: bool,
  ) -> Result<u64, ImageError> {
    let index = if directories {
      let Some(index) = node.checked_sub(1) else {
        self.phase = Phase::Unstage;
        return Ok(0);
      };
      index
    } else {
      if node >= self.base.tree.nodes().len() {
        self.phase = Phase::Remove {
          node: self.base.tree.nodes().len(),
          directories: true,
        };
        return Ok(0);
      }
      node
    };
    let is_directory = matches!(
      self
        .base
        .tree
        .node(id_of(&self.base.tree, index)?)
        .map(|node| &node.kind),
      Some(ImageKind::Directory)
    );
    let next = if directories {
      index
    } else {
      node.saturating_add(1)
    };
    let name = if is_directory == directories {
      self.old_names.get_mut(index).and_then(Vec::pop)
    } else {
      None
    };
    let Some((parent, name)) = name else {
      self.phase = Phase::Remove {
        node: next,
        directories,
      };
      return Ok(0);
    };
    if is_directory {
      clone.rmdir_no(store, parent, &name)?;
    } else {
      clone.unlink_no(store, parent, &name)?;
    }
    self.report.names_removed = self.report.names_removed.saturating_add(1);
    let gone = self.image_of.get(index).copied().flatten().is_none();
    let last = self.old_names.get(index).is_none_or(Vec::is_empty);
    if gone && last {
      self.report.removed = self.report.removed.saturating_add(1);
    }
    // The node's next name, if any, comes in the next item.
    self.phase = Phase::Remove { node, directories };
    Ok(u64::try_from(name.len()).unwrap_or(u64::MAX))
  }

  /// Removes the staging directory, empty by now; then the nodes are settled.
  fn unstage(&mut self, clone: &mut Volume, store: &mut Store) -> Result<u64, ImageError> {
    if let Some((_, name)) = self.staging.take() {
      let root = clone.root_inode(store)?;
      clone.rmdir_no(store, root, &name)?;
    }
    self.phase = Phase::Settle {
      node: 0,
      offset: None,
    };
    Ok(1)
  }

  /// Settles image node `node`: a file's size, then its pages one a step, then its attributes.
  fn settle(
    &mut self,
    clone: &mut Volume,
    store: &mut Store,
    content: &mut dyn MergeContent,
    node: usize,
    offset: Option<u64>,
  ) -> Result<u64, ImageError> {
    let Some(id) = self.target.tree.id(node) else {
      self.phase = Phase::Done;
      return Ok(0);
    };
    let image = self
      .target
      .tree
      .node(id)
      .cloned()
      .ok_or(ImageError::Inconsistent {
        reason: "an image node out of range",
      })?;
    let no = self
      .inode
      .get(node)
      .copied()
      .flatten()
      .ok_or(ImageError::Inconsistent {
        reason: "an image node never attached",
      })?;
    if let ImageKind::File { size } = image.kind {
      match offset {
        None => {
          if clone.stat(store, no)?.size != size {
            clone.truncate(store, no, size)?;
          }
          self.phase = Phase::Settle {
            node,
            offset: Some(0),
          };
          return Ok(1);
        }
        Some(at) if at < size => {
          let cost = self.settle_page(clone, store, content, node, no, at, size)?;
          self.phase = Phase::Settle {
            node,
            offset: Some(at.saturating_add(self.unit)),
          };
          return Ok(cost);
        }
        Some(_) => {}
      }
    }
    self.settle_attributes(clone, store, no, &image)?;
    self.phase = Phase::Settle {
      node: node.saturating_add(1),
      offset: None,
    };
    Ok(1)
  }

  /// Compares the page at `at` of image node `node` with clone inode `no`'s, and writes the image's when they differ.
  #[allow(clippy::too_many_arguments)]
  fn settle_page(
    &mut self,
    clone: &mut Volume,
    store: &mut Store,
    content: &mut dyn MergeContent,
    node: usize,
    no: InodeNo,
    at: u64,
    size: u64,
  ) -> Result<u64, ImageError> {
    let ino = *self
      .target
      .inodes
      .get(node)
      .ok_or(ImageError::Inconsistent {
        reason: "an image node without an inode",
      })?;
    let length = usize::try_from(self.unit.min(size.saturating_sub(at))).map_err(|_| {
      ImageError::Inconsistent {
        reason: "a page past the address space",
      }
    })?;
    let mut wanted = vec![0u8; length];
    let mut held = vec![0u8; length];
    let read = content.read(store, ino, at, &mut wanted)?;
    if read != length {
      return Err(ImageError::Image(ImageRefusal::ContentShort { node, at }));
    }
    let had = clone.read(store, no, at, &mut held)?;
    if had != length || held != wanted {
      let written = clone.write(store, no, at, &wanted)?;
      if written != length {
        return Err(ImageError::Volume(VfsError::NoSpace));
      }
      self.report.bytes_written = self
        .report
        .bytes_written
        .saturating_add(u64::try_from(length).unwrap_or(u64::MAX));
    }
    let compared = u64::try_from(length).unwrap_or(u64::MAX);
    self.report.bytes_compared = self.report.bytes_compared.saturating_add(compared);
    Ok(compared.saturating_mul(2))
  }

  /// Sets clone inode `no`'s permissions, owner and times to image node `image`'s where they differ, the times last.
  fn settle_attributes(
    &mut self,
    clone: &mut Volume,
    store: &mut Store,
    no: InodeNo,
    image: &Node,
  ) -> Result<(), ImageError> {
    let held = clone.stat(store, no)?;
    let permissions = u32::from(image.permissions);
    let mut changed = false;
    if held.mode & PERMISSION_BITS != permissions {
      clone.chmod(store, no, (held.mode & !PERMISSION_BITS) | permissions)?;
      changed = true;
    }
    if held.uid != image.uid || held.gid != image.gid {
      clone.chown(store, no, image.uid, image.gid)?;
      changed = true;
    }
    let times = (
      nanos_of(image.atime),
      nanos_of(image.mtime),
      nanos_of(image.ctime),
    );
    let (Some(atime), Some(mtime), Some(ctime)) = times else {
      return Err(ImageError::Image(ImageRefusal::TimeOutOfRange {
        secs: image.mtime.secs,
      }));
    };
    let held = clone.stat(store, no)?;
    if held.atime != atime || held.mtime != mtime || held.ctime != ctime {
      clone.set_times(store, no, Some(atime), Some(mtime), Some(ctime), None)?;
      changed = true;
    }
    if changed {
      self.report.attributes_set = self.report.attributes_set.saturating_add(1);
    }
    Ok(())
  }
}

/// The id of node `index` of `tree`, refused when out of range.
fn id_of(tree: &Tree, index: usize) -> Result<NodeId, ImageError> {
  tree.id(index).ok_or(ImageError::Inconsistent {
    reason: "a node out of range",
  })
}
