//! A-160 (a volume's disk-image form, the volume's side): a snapshot walked and written as an image reads back as the
//! volume; an image merged into a clone of its snapshot makes the clone hold the image's tree, each build node keeping
//! its inode. The merge oracle generates guest histories over an image's tree — renames, moves of whole directories,
//! rewrites and patches, attribute changes, deletions, creations, hard links, swaps of two names, a file deleted and
//! made again at its inode number — and holds the clone to the edited tree and to the identity rule.
// Test harness code: an unwrap here is a failed test; proptest's strategy union carries an `Arc` (a test harness, the
// third D-8 exception).
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::disallowed_types,
  clippy::unwrap_in_result
)]

mod common;

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use slates_image::{
  Builder, Identity, Image, ImageTree, Kind as ImageKind, Node, NodeId, Progress, Timestamp, Tree,
  TreeBuilder,
};
use slates_vfs::clock::StepClock;
use slates_vfs::ids::{InodeNo, SnapshotId};
use slates_vfs::image::{
  BuildIo, ImageContent, ImageError, Merge, MergeContent, MergeReport, SnapshotFile, SnapshotTree,
  SnapshotWalk,
};
use slates_vfs::inode::Kind;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, Volume, VolumeConfig};

/// Shape: each test volume's bounded quota.
const QUOTA: u64 = 6 << 20;
/// Shape: the capacity of the test images.
const CAPACITY: u64 = 2 << 20;
/// Shape: the entries a walk may read.
const MOST: usize = 1 << 16;
/// Shape: the smallest step budget, one item a step.
const SMALLEST: u64 = 1;
/// Shape: the identity of the test images.
const IDENTITY: Identity = Identity {
  uuid: [7; 16],
  created: Timestamp {
    secs: 1_700_000_000,
    nanos: 0,
  },
};
/// Format: the permission bits of a mode.
const PERMISSIONS: u32 = 0o7777;
/// Format: nanoseconds in a second.
const NANOS: i64 = 1_000_000_000;

/// A volume with prefix `prefix`, names judged by `names`, a clock in the image format's range.
fn volume(store: &mut Store, prefix: u16, names: NameEquivalence) -> Volume {
  Volume::create(store, config(prefix, names)).unwrap()
}

/// A volume configuration: bounded at [`QUOTA`], the clock stepping from a thousand seconds past the epoch.
fn config(prefix: u16, names: NameEquivalence) -> VolumeConfig {
  VolumeConfig {
    prefix,
    names,
    quota: Quota::Bounded { limit: QUOTA },
    journal_bytes: 1 << 20,
    clock: Box::new(StepClock::new(1_000 * 1_000_000_000, 1_000)),
  }
}

/// One entry of a tree's canonical form: its path, the least path naming its node when another does (a hard link),
/// the node, and a file's bytes (on the least path only).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
  path: Vec<u8>,
  linked_to: Option<Vec<u8>>,
  node: Node,
  bytes: Vec<u8>,
}

/// The canonical form of `items` (path, node key, node, bytes): sorted by path, each node named by its least path.
fn canonical_of(mut items: Vec<(Vec<u8>, u64, Node, Vec<u8>)>) -> Vec<Entry> {
  items.sort_by(|left, right| left.0.cmp(&right.0));
  let mut least: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
  for (path, key, _, _) in &items {
    least.entry(*key).or_insert_with(|| path.clone());
  }
  items
    .into_iter()
    .map(|(path, key, node, bytes)| {
      let first = least.get(&key).cloned().unwrap();
      if first == path {
        Entry {
          path,
          linked_to: None,
          node,
          bytes,
        }
      } else {
        Entry {
          path,
          linked_to: Some(first),
          node,
          bytes: Vec::new(),
        }
      }
    })
    .collect()
}

/// A volume time as an image's.
fn timestamp(nanos: i64) -> Timestamp {
  Timestamp {
    secs: nanos.div_euclid(NANOS),
    nanos: u32::try_from(nanos.rem_euclid(NANOS)).unwrap(),
  }
}

/// The canonical form of `volume`'s head.
fn volume_entries(volume: &Volume, store: &Store) -> Vec<Entry> {
  let root = volume.root_inode(store).unwrap();
  let node = |kind: ImageKind, no: InodeNo| {
    let attrs = volume.stat(store, no).unwrap();
    Node {
      kind,
      permissions: u16::try_from(attrs.mode & PERMISSIONS).unwrap(),
      uid: attrs.uid,
      gid: attrs.gid,
      atime: timestamp(attrs.atime),
      mtime: timestamp(attrs.mtime),
      ctime: timestamp(attrs.ctime),
    }
  };
  let mut items = vec![(
    Vec::new(),
    root.0,
    node(ImageKind::Directory, root),
    Vec::new(),
  )];
  let mut stack = vec![(Vec::new(), root)];
  while let Some((prefix, directory)) = stack.pop() {
    for row in volume.readdir_no(store, directory).unwrap() {
      let mut path = prefix.clone();
      path.push(b'/');
      path.extend_from_slice(row.name.as_bytes());
      let size = volume.stat(store, row.inode).unwrap().size;
      let (kind, bytes) = match row.kind {
        Kind::Dir => {
          stack.push((path.clone(), row.inode));
          (ImageKind::Directory, Vec::new())
        }
        Kind::File => {
          let mut bytes = vec![0u8; usize::try_from(size).unwrap()];
          let read = volume.read(store, row.inode, 0, &mut bytes).unwrap();
          assert_eq!(read, bytes.len());
          (ImageKind::File { size }, bytes)
        }
        Kind::Symlink => (
          ImageKind::Symlink {
            target: volume
              .readlink(store, row.inode)
              .unwrap()
              .as_bytes()
              .to_vec(),
          },
          Vec::new(),
        ),
        Kind::Fifo => (ImageKind::Fifo, Vec::new()),
        Kind::Socket => (ImageKind::Socket, Vec::new()),
      };
      items.push((path, row.inode.0, node(kind, row.inode), bytes));
    }
  }
  canonical_of(items)
}

/// The canonical form of `tree`, each file's bytes from `content`.
fn tree_entries(tree: &Tree, content: &mut dyn FnMut(NodeId) -> Vec<u8>) -> Vec<Entry> {
  let mut items = vec![(
    Vec::new(),
    0u64,
    tree.node(NodeId::ROOT).unwrap().clone(),
    Vec::new(),
  )];
  let mut stack = vec![(Vec::new(), NodeId::ROOT)];
  while let Some((prefix, directory)) = stack.pop() {
    for (name, child) in tree.entries(directory) {
      let mut path = prefix.clone();
      path.push(b'/');
      path.extend_from_slice(name);
      let node = tree.node(child).unwrap().clone();
      let bytes = match node.kind {
        ImageKind::File { .. } => content(child),
        ImageKind::Directory => {
          stack.push((path.clone(), child));
          Vec::new()
        }
        _ => Vec::new(),
      };
      items.push((path, u64::try_from(child.index()).unwrap(), node, bytes));
    }
  }
  canonical_of(items)
}

/// The inode `volume`'s head names at `path`.
fn inode_at(volume: &Volume, store: &Store, path: &[u8]) -> InodeNo {
  let path = std::str::from_utf8(path).unwrap();
  volume
    .resolve(store, if path.is_empty() { "/" } else { path })
    .unwrap()
    .inode
}

/// A node to make in a generated volume.
#[derive(Clone, Debug)]
enum Make {
  Directory,
  File(Vec<u8>),
  /// A file of `len` bytes cut from a pattern seeded by `seed`: one past a chunk, so a merge compares several.
  Large(usize, u8),
  Symlink(String),
  Fifo,
  Socket,
  /// A hard link to the file made `n`th, modulo the files made.
  Link(usize),
}

/// A generated node's permissions, owner, group and times.
type Meta = (u32, u32, u32, [i64; 3]);

/// A time both forms hold: the image format's from 1901, a volume's nanoseconds in an `i64` to 2262.
fn time() -> impl Strategy<Value = i64> {
  (-(1_i64 << 31)..(i64::MAX / NANOS), 0..NANOS).prop_map(|(secs, nanos)| secs * NANOS + nanos)
}

fn meta() -> impl Strategy<Value = Meta> {
  (
    0u32..=PERMISSIONS,
    any::<u32>(),
    any::<u32>(),
    [time(), time(), time()],
  )
}

/// A name from a small alphabet, so names collide and sort every way.
fn name() -> impl Strategy<Value = String> {
  prop::collection::vec(prop::sample::select(b"abcAB0._-".to_vec()), 1..8).prop_map(|bytes| {
    let name = String::from_utf8(bytes).unwrap();
    if name == "." || name == ".." {
      format!("{name}x")
    } else {
      name
    }
  })
}

fn makes() -> impl Strategy<Value = Vec<(Make, usize, String, Meta)>> {
  let make = prop_oneof![
    3 => Just(Make::Directory),
    4 => prop::collection::vec(any::<u8>(), 0..3_000).prop_map(Make::File),
    1 => (65_537usize..140_000, any::<u8>()).prop_map(|(len, seed)| Make::Large(len, seed)),
    2 => "[a-z/.]{1,80}".prop_map(Make::Symlink),
    1 => Just(Make::Fifo),
    1 => Just(Make::Socket),
    2 => any::<usize>().prop_map(Make::Link),
  ];
  prop::collection::vec((make, any::<usize>(), name(), meta()), 0..24)
}

/// The bytes of a [`Make::Large`] file.
fn pattern(len: usize, seed: u8) -> Vec<u8> {
  (0..len)
    .map(|at| u8::try_from((at * 7 + usize::from(seed)) % 251).unwrap())
    .collect()
}

/// Makes `makes` in `volume`: each node under a directory picked among those made, its name made unique there, its
/// attributes set last.
fn populate(volume: &mut Volume, store: &mut Store, makes: Vec<(Make, usize, String, Meta)>) {
  let root = volume.root_inode(store).unwrap();
  let mut directories = vec![root];
  let mut files: Vec<InodeNo> = Vec::new();
  let mut names: BTreeSet<(u64, String)> = BTreeSet::new();
  for (index, (make, parent, name, (permissions, uid, gid, [atime, mtime, ctime]))) in
    makes.into_iter().enumerate()
  {
    let parent = directories[parent % directories.len()];
    let folded = name.to_ascii_lowercase();
    let name = if names.insert((parent.0, folded.clone())) {
      name
    } else {
      let unique = format!("{name}~{index}");
      names.insert((parent.0, unique.to_ascii_lowercase()));
      unique
    };
    let no = match make {
      Make::Directory => {
        let no = volume.mkdir_no(store, parent, &name, 0o755).unwrap();
        directories.push(no);
        no
      }
      Make::File(bytes) => {
        let no = volume.create_file_no(store, parent, &name, 0o644).unwrap();
        volume.write(store, no, 0, &bytes).unwrap();
        files.push(no);
        no
      }
      Make::Large(len, seed) => {
        let no = volume.create_file_no(store, parent, &name, 0o644).unwrap();
        volume.write(store, no, 0, &pattern(len, seed)).unwrap();
        files.push(no);
        no
      }
      Make::Symlink(target) => volume.symlink_no(store, parent, &name, &target).unwrap(),
      Make::Fifo => volume
        .mknod_no(store, parent, &name, 0o644, Kind::Fifo)
        .unwrap(),
      Make::Socket => volume
        .mknod_no(store, parent, &name, 0o644, Kind::Socket)
        .unwrap(),
      Make::Link(target) => {
        if files.is_empty() {
          continue;
        }
        let target = files[target % files.len()];
        volume.link_no(store, parent, &name, target).unwrap();
        continue;
      }
    };
    volume.chmod(store, no, permissions).unwrap();
    volume.chown(store, no, uid, gid).unwrap();
    volume
      .set_times(store, no, Some(atime), Some(mtime), Some(ctime), None)
      .unwrap();
  }
}

/// An image built from a snapshot into a volume of its own.
struct Built {
  image_volume: Volume,
  file: InodeNo,
  snapshot: SnapshotId,
  len: u64,
  base: SnapshotTree,
  walk_steps: usize,
}

/// Walks `snapshot` of `source` and writes its image into a new volume, both in the smallest steps.
fn build(store: &mut Store, source: &Volume, snapshot: SnapshotId) -> Built {
  let mut walk = SnapshotWalk::new(source, store, snapshot, MOST).unwrap();
  let mut walk_steps = 0;
  let base = loop {
    walk_steps += 1;
    if let Some(tree) = walk.step(source, store, SMALLEST).unwrap() {
      break tree;
    }
  };
  let mut image_volume = volume(store, 11, NameEquivalence::Exact);
  let root = image_volume.root_inode(store).unwrap();
  let file = image_volume
    .create_file_no(store, root, "image.ext2", 0o644)
    .unwrap();
  let mut builder = Builder::new(base.tree.clone(), CAPACITY, IDENTITY).unwrap();
  let len = builder.image_bytes();
  image_volume.truncate(store, file, len).unwrap();
  loop {
    let mut io = BuildIo {
      store,
      source,
      snapshot,
      inodes: &base.inodes,
      image: &mut image_volume,
      file,
    };
    if let Progress::Done(_) = builder.step(&mut io, SMALLEST).unwrap() {
      break;
    }
  }
  let snapshot = image_volume.snapshot(store).unwrap();
  Built {
    image_volume,
    file,
    snapshot,
    len,
    base,
    walk_steps,
  }
}

/// The tree `built`'s image holds, read out of its volume file, and the image opened.
fn read_image(store: &Store, built: &Built) -> (ImageTree, Image) {
  let mut source = SnapshotFile {
    volume: &built.image_volume,
    store,
    snapshot: built.snapshot,
    file: built.file,
    len: built.len,
  };
  let mut image = Image::open(&mut source).unwrap();
  let tree = image.tree(&mut source, MOST).unwrap();
  (tree, image)
}

/// Runs `merge` into `clone` in the smallest steps; the report.
fn run(
  merge: &mut Merge,
  clone: &mut Volume,
  store: &mut Store,
  content: &mut dyn MergeContent,
) -> Result<MergeReport, ImageError> {
  loop {
    if let Some(report) = merge.step(clone, store, content, SMALLEST)? {
      return Ok(report);
    }
  }
}

/// The bytes of image files by inode: a guest history's contents.
struct MapContent {
  files: BTreeMap<u32, Vec<u8>>,
}

impl MergeContent for MapContent {
  fn read(
    &mut self,
    _store: &Store,
    ino: u32,
    offset: u64,
    buf: &mut [u8],
  ) -> Result<usize, ImageError> {
    let bytes = self.files.get(&ino).map_or(&[][..], Vec::as_slice);
    let start = usize::try_from(offset).unwrap().min(bytes.len());
    let take = buf.len().min(bytes.len() - start);
    buf[..take].copy_from_slice(&bytes[start..start + take]);
    Ok(take)
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig {
    cases: slates_test_seeds::cases(32),
    ..ProptestConfig::default()
  }))]

  /// A-160 (a snapshot's image reads back as the volume). Do: make a generated volume (directories, files of up to two
  /// chunks, symlinks, FIFOs, sockets, hard links, any permissions, owners and nanosecond times the format holds),
  /// snapshot it, walk and write its image in the smallest steps into a file of another volume, and read the image back
  /// out of that file. Expect the volume's tree, name for name and byte for byte; and the walk to take a step an entry.
  #[test]
  fn a_snapshot_written_as_an_image_reads_back_as_the_volume(makes in makes()) {
    let mut store = common::store();
    let mut source = volume(&mut store, 7, NameEquivalence::Exact);
    populate(&mut source, &mut store, makes);
    let snapshot = source.snapshot(&mut store).unwrap();
    let built = build(&mut store, &source, snapshot);
    let (tree, mut image) = read_image(&store, &built);
    let mut file = SnapshotFile {
      volume: &built.image_volume,
      store: &store,
      snapshot: built.snapshot,
      file: built.file,
      len: built.len,
    };
    let read = tree_entries(&tree.tree, &mut |id| {
      let ImageKind::File { size } = tree.tree.node(id).unwrap().kind else { panic!() };
      let mut bytes = vec![0u8; usize::try_from(size).unwrap()];
      image.read(&mut file, *tree.inodes.get(id.index()).unwrap(), 0, &mut bytes).unwrap();
      bytes
    });
    prop_assert_eq!(read, volume_entries(&source, &store));
    prop_assert!(built.walk_steps > built.base.tree.node_count(), "{} steps", built.walk_steps);
  }

  /// A-160 (an image no one changed). Do: build a generated volume's image, read it out of its file, and merge it into
  /// a clone of the snapshot through the image itself, in the smallest steps. Expect no change at all: every node
  /// kept, nothing created, moved, linked, removed or staged, no byte and no attribute written, though every byte was
  /// compared; and the clone equal to the volume with each of its paths at the volume's inode.
  #[test]
  fn an_image_no_one_changed_merges_as_no_change(makes in makes()) {
    let mut store = common::store();
    let mut source = volume(&mut store, 7, NameEquivalence::Exact);
    populate(&mut source, &mut store, makes);
    let snapshot = source.snapshot(&mut store).unwrap();
    let built = build(&mut store, &source, snapshot);
    let (tree, mut image) = read_image(&store, &built);
    let nodes = u64::try_from(tree.tree.node_count()).unwrap();
    let bytes: u64 = tree
      .tree
      .nodes()
      .map(|node| match node.kind { ImageKind::File { size } => size, _ => 0 })
      .sum();
    let mut clone = Volume::clone_of(&store, &mut source, snapshot, config(9, NameEquivalence::Exact)).unwrap();
    let mut merge = Merge::new(built.base.clone(), tree, &clone, &store, true);
    let mut content = ImageContent {
      image: &mut image,
      volume: &built.image_volume,
      snapshot: built.snapshot,
      file: built.file,
      len: built.len,
    };
    let report = run(&mut merge, &mut clone, &mut store, &mut content).unwrap();
    // Each linked file's bytes are compared once per node.
    prop_assert_eq!(report, MergeReport { kept: nodes, bytes_compared: bytes, ..MergeReport::default() });
    let merged = volume_entries(&clone, &store);
    prop_assert_eq!(&merged, &volume_entries(&source, &store));
    for entry in &merged {
      prop_assert_eq!(inode_at(&clone, &store, &entry.path), inode_at(&source, &store, &entry.path));
    }
  }
}

/// What a model node is.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Body {
  Directory,
  File(Vec<u8>),
  Symlink(Vec<u8>),
  Fifo,
  Socket,
}

/// A node of a guest history's model of the image: what it is, its attributes, and its inode number and generation.
#[derive(Clone, Debug)]
struct ModelNode {
  body: Body,
  permissions: u16,
  uid: u32,
  gid: u32,
  times: [Timestamp; 3],
  ino: u32,
  generation: u32,
  /// Whether the node is one the build wrote and keeps its identity (not a new node, not a number reused).
  built: bool,
}

/// A guest's view of the image: its nodes and its entries, by directory and name.
#[derive(Clone, Debug)]
struct Model {
  nodes: Vec<ModelNode>,
  entries: BTreeMap<(usize, Vec<u8>), usize>,
  next_ino: u32,
}

impl Model {
  /// The model of an image tree, its files' bytes from `content`.
  fn of(tree: &ImageTree, content: &mut dyn FnMut(NodeId) -> Vec<u8>) -> Model {
    let mut nodes = Vec::new();
    let mut entries = BTreeMap::new();
    for id in tree.tree.ids() {
      let node = tree.tree.node(id).unwrap();
      let body = match &node.kind {
        ImageKind::Directory => Body::Directory,
        ImageKind::File { .. } => Body::File(content(id)),
        ImageKind::Symlink { target } => Body::Symlink(target.clone()),
        ImageKind::Fifo => Body::Fifo,
        ImageKind::Socket => Body::Socket,
        kind => panic!("a volume's image holds no {kind:?}"),
      };
      nodes.push(ModelNode {
        body,
        permissions: node.permissions,
        uid: node.uid,
        gid: node.gid,
        times: [node.atime, node.mtime, node.ctime],
        ino: *tree.inodes.get(id.index()).unwrap(),
        generation: *tree.generations.get(id.index()).unwrap(),
        built: true,
      });
      for (name, child) in tree.tree.entries(id) {
        entries.insert((id.index(), name.to_vec()), child.index());
      }
    }
    let next_ino = tree.inodes.iter().copied().max().unwrap_or(0) + 1;
    Model {
      nodes,
      entries,
      next_ino,
    }
  }

  /// The directories reachable from the root, the root first.
  fn directories(&self) -> Vec<usize> {
    let mut found = vec![0usize];
    let mut at = 0;
    while at < found.len() {
      let directory = found[at];
      for ((parent, _), child) in &self.entries {
        if *parent == directory && self.nodes[*child].body == Body::Directory {
          found.push(*child);
        }
      }
      at += 1;
    }
    found
  }

  /// Whether `node` is `ancestor` or lies inside it.
  fn within(&self, node: usize, ancestor: usize) -> bool {
    let mut at = node;
    for _ in 0..=self.nodes.len() {
      if at == ancestor {
        return true;
      }
      match self.entries.iter().find(|(_, child)| **child == at) {
        Some(((parent, _), _)) => at = *parent,
        None => return false,
      }
    }
    false
  }

  /// Removes entry `key` and, for a directory, every entry inside it.
  fn remove(&mut self, key: &(usize, Vec<u8>)) {
    let Some(child) = self.entries.remove(key) else {
      return;
    };
    if self.nodes[child].body == Body::Directory {
      let inside: Vec<(usize, Vec<u8>)> = self
        .entries
        .keys()
        .filter(|(parent, _)| *parent == child)
        .cloned()
        .collect();
      for key in inside {
        self.remove(&key);
      }
    }
  }

  /// Adds a node made by the guest: a new inode number and a generation of the kernel's.
  fn add(&mut self, body: Body, (permissions, uid, gid, times): Meta, generation: u32) -> usize {
    let ino = self.next_ino;
    self.next_ino += 1;
    self.nodes.push(ModelNode {
      body,
      permissions: u16::try_from(permissions).unwrap(),
      uid,
      gid,
      times: times.map(timestamp),
      ino,
      generation: generation.max(1),
      built: false,
    });
    self.nodes.len() - 1
  }

  /// A name not taken in `directory`, from `wanted`.
  fn free_name(&self, directory: usize, wanted: &str) -> Vec<u8> {
    let mut name = wanted.as_bytes().to_vec();
    while self.entries.contains_key(&(directory, name.clone())) {
      name.push(b'~');
    }
    name
  }

  /// The image tree of the model's reachable nodes, its inode numbers, generations and files' bytes; and each tree
  /// node's model node.
  fn tree(&self) -> (ImageTree, BTreeMap<u32, Vec<u8>>, Vec<usize>) {
    let mut reachable = vec![false; self.nodes.len()];
    reachable[0] = true;
    for directory in self.directories() {
      for ((parent, _), child) in &self.entries {
        if *parent == directory {
          reachable[*child] = true;
        }
      }
    }
    let node_of = |model: &ModelNode| Node {
      kind: match &model.body {
        Body::Directory => ImageKind::Directory,
        Body::File(bytes) => ImageKind::File {
          size: u64::try_from(bytes.len()).unwrap(),
        },
        Body::Symlink(target) => ImageKind::Symlink {
          target: target.clone(),
        },
        Body::Fifo => ImageKind::Fifo,
        Body::Socket => ImageKind::Socket,
      },
      permissions: model.permissions,
      uid: model.uid,
      gid: model.gid,
      atime: model.times[0],
      mtime: model.times[1],
      ctime: model.times[2],
    };
    let mut builder = TreeBuilder::new(node_of(&self.nodes[0]));
    let mut ids = vec![None; self.nodes.len()];
    ids[0] = Some(NodeId::ROOT);
    let mut order = vec![0usize];
    for (index, model) in self.nodes.iter().enumerate().skip(1) {
      if reachable[index] {
        ids[index] = Some(builder.add_node(node_of(model)));
        order.push(index);
      }
    }
    for ((parent, name), child) in &self.entries {
      if let (Some(parent), Some(child)) = (ids[*parent], ids[*child]) {
        builder.add_entry(parent, name.clone(), child);
      }
    }
    let tree = builder.build().unwrap();
    let inodes = order.iter().map(|index| self.nodes[*index].ino).collect();
    let generations = order
      .iter()
      .map(|index| self.nodes[*index].generation)
      .collect();
    let mut files = BTreeMap::new();
    for index in &order {
      if let Body::File(bytes) = &self.nodes[*index].body {
        files.insert(self.nodes[*index].ino, bytes.clone());
      }
    }
    (
      ImageTree {
        tree,
        inodes,
        generations,
      },
      files,
      order,
    )
  }
}

/// A guest's change to the image, its picks taken modulo what the model holds when it runs.
#[derive(Clone, Debug)]
enum Edit {
  /// Renames entry `a` into directory `b` as `name` (a directory never into itself).
  Rename(usize, usize, String),
  /// Replaces file `a`'s bytes.
  Rewrite(usize, Vec<u8>),
  /// Writes `bytes` into file `a` at offset `b`, extending it if need be.
  Patch(usize, usize, Vec<u8>),
  /// Sets node `a`'s attributes.
  Attributes(usize, Meta),
  /// Deletes entry `a` (a directory with everything inside it).
  Delete(usize),
  /// Makes a node in directory `a` named `name`: a directory, a file of `bytes`, a symlink, a FIFO.
  Create(usize, String, u8, Vec<u8>, Meta, u32),
  /// Links file `a` into directory `b` as `name`.
  Link(usize, usize, String),
  /// Swaps the children of two entries of directory `a`.
  Swap(usize, usize, usize),
  /// Deletes the file entry `a` names and makes a new file at its name and inode number, under a new generation.
  Reuse(usize, Vec<u8>, u32),
}

fn edits() -> impl Strategy<Value = Vec<Edit>> {
  let bytes = || prop::collection::vec(any::<u8>(), 0..2_500);
  let edit = prop_oneof![
    3 => (any::<usize>(), any::<usize>(), name()).prop_map(|(a, b, n)| Edit::Rename(a, b, n)),
    2 => (any::<usize>(), bytes()).prop_map(|(a, b)| Edit::Rewrite(a, b)),
    2 => (any::<usize>(), 0usize..80_000, bytes()).prop_map(|(a, b, c)| Edit::Patch(a, b, c)),
    2 => (any::<usize>(), meta()).prop_map(|(a, m)| Edit::Attributes(a, m)),
    2 => any::<usize>().prop_map(Edit::Delete),
    3 => (any::<usize>(), name(), 0u8..4, bytes(), meta(), any::<u32>())
      .prop_map(|(a, n, k, b, m, g)| Edit::Create(a, n, k, b, m, g)),
    1 => (any::<usize>(), any::<usize>(), name()).prop_map(|(a, b, n)| Edit::Link(a, b, n)),
    1 => (any::<usize>(), any::<usize>(), any::<usize>()).prop_map(|(a, b, c)| Edit::Swap(a, b, c)),
    1 => (any::<usize>(), bytes(), any::<u32>()).prop_map(|(a, b, g)| Edit::Reuse(a, b, g)),
  ];
  prop::collection::vec(edit, 0..12)
}

/// Applies `edit` to `model`, as a guest's kernel would to the image.
fn apply(model: &mut Model, edit: Edit) {
  let keys: Vec<(usize, Vec<u8>)> = model.entries.keys().cloned().collect();
  let directories = model.directories();
  let files: Vec<usize> = (0..model.nodes.len())
    .filter(|index| {
      matches!(model.nodes[*index].body, Body::File(_))
        && model.entries.values().any(|child| child == index)
    })
    .collect();
  match edit {
    Edit::Rename(a, b, name) if !keys.is_empty() => {
      rename(
        model,
        keys[a % keys.len()].clone(),
        directories[b % directories.len()],
        &name,
      );
    }
    Edit::Rewrite(a, bytes) if !files.is_empty() => {
      model.nodes[files[a % files.len()]].body = Body::File(bytes);
    }
    Edit::Patch(a, offset, bytes) if !files.is_empty() => {
      patch(model, files[a % files.len()], offset, &bytes)
    }
    Edit::Attributes(a, meta) => set_attributes(model, a, meta),
    Edit::Delete(a) if !keys.is_empty() => model.remove(&keys[a % keys.len()]),
    Edit::Create(a, name, kind, bytes, meta, generation) => {
      create(
        model,
        directories[a % directories.len()],
        &name,
        kind,
        bytes,
        meta,
        generation,
      );
    }
    Edit::Link(a, b, name) if !files.is_empty() => {
      let directory = directories[b % directories.len()];
      let name = model.free_name(directory, &name);
      model
        .entries
        .insert((directory, name), files[a % files.len()]);
    }
    Edit::Swap(a, b, c) => swap(model, &keys, directories[a % directories.len()], b, c),
    Edit::Reuse(a, bytes, generation) if !keys.is_empty() => {
      reuse(model, keys[a % keys.len()].clone(), bytes, generation);
    }
    _ => {}
  }
}

/// Renames entry `key` into `target` as `name` (made free), unless that would put a directory inside itself.
fn rename(model: &mut Model, key: (usize, Vec<u8>), target: usize, name: &str) {
  let child = model.entries[&key];
  if model.nodes[child].body == Body::Directory && model.within(target, child) {
    return;
  }
  let name = model.free_name(target, name);
  model.entries.remove(&key);
  model.entries.insert((target, name), child);
}

/// Writes `bytes` into file `file` at `offset`, extending it if need be.
fn patch(model: &mut Model, file: usize, offset: usize, bytes: &[u8]) {
  if let Body::File(held) = &mut model.nodes[file].body {
    let end = offset + bytes.len();
    if held.len() < end {
      held.resize(end, 0);
    }
    held[offset..end].copy_from_slice(bytes);
  }
}

/// Sets the attributes of a reachable node picked by `pick`.
fn set_attributes(model: &mut Model, pick: usize, (permissions, uid, gid, times): Meta) {
  let reachable: Vec<usize> = std::iter::once(0)
    .chain(model.entries.values().copied())
    .collect();
  let node = &mut model.nodes[reachable[pick % reachable.len()]];
  node.permissions = u16::try_from(permissions).unwrap();
  node.uid = uid;
  node.gid = gid;
  node.times = times.map(timestamp);
}

/// Makes a node of `kind` (a directory, a file of `bytes`, a symlink, a FIFO) in `directory` as `name` (made free).
fn create(
  model: &mut Model,
  directory: usize,
  name: &str,
  kind: u8,
  bytes: Vec<u8>,
  meta: Meta,
  generation: u32,
) {
  let name = model.free_name(directory, name);
  let body = match kind {
    0 => Body::Directory,
    1 => Body::File(bytes),
    2 => Body::Symlink(format!("t{}", bytes.len()).into_bytes()),
    _ => Body::Fifo,
  };
  let node = model.add(body, meta, generation);
  model.entries.insert((directory, name), node);
}

/// Swaps the children of two entries of `directory`, picked by `first` and `second`.
fn swap(
  model: &mut Model,
  keys: &[(usize, Vec<u8>)],
  directory: usize,
  first: usize,
  second: usize,
) {
  let held: Vec<(usize, Vec<u8>)> = keys
    .iter()
    .filter(|(parent, _)| *parent == directory)
    .cloned()
    .collect();
  if held.len() >= 2 {
    let (first, second) = (
      held[first % held.len()].clone(),
      held[second % held.len()].clone(),
    );
    let (one, two) = (model.entries[&first], model.entries[&second]);
    model.entries.insert(first, two);
    model.entries.insert(second, one);
  }
}

/// Deletes the file entry `key` names, when it is the file's one name, and makes a new file at its name and inode
/// number under a new generation: the kernel hands a freed number to the next file.
fn reuse(model: &mut Model, key: (usize, Vec<u8>), bytes: Vec<u8>, generation: u32) {
  let child = model.entries[&key];
  let links = model
    .entries
    .values()
    .filter(|held| **held == child)
    .count();
  if !matches!(model.nodes[child].body, Body::File(_)) || links != 1 {
    return;
  }
  let mut node = model.nodes[child].clone();
  node.body = Body::File(bytes);
  node.generation = generation.max(1);
  node.built = false;
  model.nodes.push(node);
  let new = model.nodes.len() - 1;
  model.entries.insert(key, new);
}

proptest! {
  #![proptest_config(slates_test_seeds::seeded(ProptestConfig {
    cases: slates_test_seeds::cases(64),
    ..ProptestConfig::default()
  }, include_str!("image.proptest-regressions")).unwrap())]

  /// A-160 (the merge oracle). Do: build a generated volume's image, apply a generated guest history to the image's
  /// tree (renames and moves of directories, rewrites and patches, attribute changes, deletions, creations, hard links,
  /// swaps, a file made again at a freed inode number), and merge the edited tree into a clone of the snapshot in the
  /// smallest steps. Expect the clone to hold the edited tree exactly (names, kinds, links, attributes, nanosecond
  /// times, bytes); every build node that kept its identity at its build inode at each of its paths, and every other
  /// node at an inode the base never had; and the report to count exactly the nodes kept, made and removed.
  #[test]
  fn a_guest_history_merges_into_the_clone_as_the_edited_tree(makes in makes(), history in edits()) {
    let mut store = common::store();
    let mut source = volume(&mut store, 7, NameEquivalence::Exact);
    populate(&mut source, &mut store, makes);
    let snapshot = source.snapshot(&mut store).unwrap();
    let built = build(&mut store, &source, snapshot);
    let (written, mut image) = read_image(&store, &built);
    let mut file = SnapshotFile {
      volume: &built.image_volume,
      store: &store,
      snapshot: built.snapshot,
      file: built.file,
      len: built.len,
    };
    let mut model = Model::of(&written, &mut |id| {
      let ImageKind::File { size } = written.tree.node(id).unwrap().kind else { panic!() };
      let mut bytes = vec![0u8; usize::try_from(size).unwrap()];
      image.read(&mut file, *written.inodes.get(id.index()).unwrap(), 0, &mut bytes).unwrap();
      bytes
    });
    for edit in history {
      apply(&mut model, edit);
    }
    let (edited, files, order) = model.tree();
    let expected = tree_entries(&edited.tree, &mut |id| files[edited.inodes.get(id.index()).unwrap()].clone());
    let mut clone = Volume::clone_of(&store, &mut source, snapshot, config(9, NameEquivalence::Exact)).unwrap();
    let mut merge = Merge::new(built.base.clone(), edited.clone(), &clone, &store, true);
    let report = run(&mut merge, &mut clone, &mut store, &mut MapContent { files }).unwrap();
    prop_assert_eq!(volume_entries(&clone, &store), expected.clone());
    // The identity rule, stated by the design: a node the build wrote, at its number with the build's generation.
    let base_inodes: BTreeSet<InodeNo> = built.base.inodes.iter().copied().collect();
    let mut kept_identity = BTreeSet::new();
    for (path, id) in paths(&edited.tree) {
      let index = order[id.index()];
      let held = inode_at(&clone, &store, &path);
      if model.nodes[index].built {
        let base = if index == 0 { 0 } else { usize::try_from(model.nodes[index].ino).unwrap() - 10 };
        prop_assert_eq!(held, *built.base.inodes.get(base).unwrap(), "{:?}", String::from_utf8_lossy(&path));
        kept_identity.insert(base);
      } else {
        prop_assert!(!base_inodes.contains(&held), "a new node at a base inode: {:?}", path);
      }
    }
    let kept = u64::try_from(kept_identity.len()).unwrap();
    let base_nodes = u64::try_from(built.base.tree.node_count()).unwrap();
    let image_nodes = u64::try_from(edited.tree.node_count()).unwrap();
    prop_assert_eq!(report.kept, kept);
    prop_assert_eq!(report.created, image_nodes - kept);
    prop_assert_eq!(report.removed, base_nodes - kept);
  }
}

/// Every path of `tree` with the node it names, the root's (empty) first.
fn paths(tree: &Tree) -> Vec<(Vec<u8>, NodeId)> {
  let mut found = vec![(Vec::new(), NodeId::ROOT)];
  let mut stack = vec![(Vec::new(), NodeId::ROOT)];
  while let Some((prefix, directory)) = stack.pop() {
    for (name, child) in tree.entries(directory) {
      let mut path = prefix.clone();
      path.push(b'/');
      path.extend_from_slice(name);
      if tree.node(child).unwrap().kind == ImageKind::Directory {
        stack.push((path.clone(), child));
      }
      found.push((path, child));
    }
  }
  found
}

/// The base volume of the scenario tests: `/d/a` (`aaa`), `/d/b` (`bbb`), `/d/sub/c` (`ccc`), `/x` (`xxx`), a link
/// `/d/link` to `/x`, a symlink `/s` and a FIFO `/p`.
fn scenario_base(store: &mut Store) -> Volume {
  let mut source = volume(store, 7, NameEquivalence::Exact);
  let root = source.root_inode(store).unwrap();
  let d = source.mkdir_no(store, root, "d", 0o755).unwrap();
  let sub = source.mkdir_no(store, d, "sub", 0o755).unwrap();
  for (directory, name, bytes) in [
    (d, "a", b"aaa"),
    (d, "b", b"bbb"),
    (sub, "c", b"ccc"),
    (root, "x", b"xxx"),
  ] {
    let no = source
      .create_file_no(store, directory, name, 0o644)
      .unwrap();
    source.write(store, no, 0, bytes).unwrap();
  }
  let x = source.lookup_no(store, root, "x").unwrap().inode;
  source.link_no(store, d, "link", x).unwrap();
  source.symlink_no(store, root, "s", "d/a").unwrap();
  source
    .mknod_no(store, root, "p", 0o644, Kind::Fifo)
    .unwrap();
  source
}

/// The model node at `path` of `model`.
fn model_node(model: &Model, path: &str) -> usize {
  let mut at = 0usize;
  for part in path.split('/').filter(|part| !part.is_empty()) {
    at = model.entries[&(at, part.as_bytes().to_vec())];
  }
  at
}

/// Builds the scenario base's image, applies `change` to its model, merges the result into a clone, and checks the
/// clone holds the changed tree; the merge's report.
fn scenario(change: &dyn Fn(&mut Model)) -> MergeReport {
  let mut store = common::store();
  let mut source = scenario_base(&mut store);
  let snapshot = source.snapshot(&mut store).unwrap();
  let built = build(&mut store, &source, snapshot);
  let (written, mut image) = read_image(&store, &built);
  let mut file = SnapshotFile {
    volume: &built.image_volume,
    store: &store,
    snapshot: built.snapshot,
    file: built.file,
    len: built.len,
  };
  let mut model = Model::of(&written, &mut |id| {
    let ImageKind::File { size } = written.tree.node(id).unwrap().kind else {
      panic!()
    };
    let mut bytes = vec![0u8; usize::try_from(size).unwrap()];
    image
      .read(
        &mut file,
        *written.inodes.get(id.index()).unwrap(),
        0,
        &mut bytes,
      )
      .unwrap();
    bytes
  });
  change(&mut model);
  let (edited, files, _) = model.tree();
  let expected = tree_entries(&edited.tree, &mut |id| {
    files[edited.inodes.get(id.index()).unwrap()].clone()
  });
  let mut clone = Volume::clone_of(
    &store,
    &mut source,
    snapshot,
    config(9, NameEquivalence::Exact),
  )
  .unwrap();
  let mut merge = Merge::new(built.base.clone(), edited, &clone, &store, true);
  let report = run(
    &mut merge,
    &mut clone,
    &mut store,
    &mut MapContent { files },
  )
  .unwrap();
  assert_eq!(volume_entries(&clone, &store), expected);
  report
}

/// A-160 (each change its own path). Do: merge, one at a time, a swap of two names, a file renamed into another
/// directory, a directory moved with what it holds, a hard link added and one dropped, a file rewritten, a file
/// deleted and made again at its inode number, a name given to a node of another kind, and a directory deleted with
/// a file inside it that moved out first. Expect each clone to hold the changed tree, and each change to take its own
/// path: names moved aside only for the swap and the kind change, renames for moves, links for links, pages written
/// for rewrites, nodes removed for deletions.
#[test]
fn each_kind_of_guest_change_takes_its_own_path() {
  let swap = scenario(&|model| {
    let d = model_node(model, "/d");
    let (a, b) = (model_node(model, "/d/a"), model_node(model, "/d/b"));
    model.entries.insert((d, b"a".to_vec()), b);
    model.entries.insert((d, b"b".to_vec()), a);
  });
  assert_eq!(
    (swap.staged, swap.moved, swap.bytes_written),
    (2, 2, 0),
    "{swap:?}"
  );
  let renamed = scenario(&|model| {
    let a = model_node(model, "/d/a");
    let sub = model_node(model, "/d/sub");
    model
      .entries
      .remove(&(model_node(model, "/d"), b"a".to_vec()));
    model.entries.insert((sub, b"renamed".to_vec()), a);
  });
  assert_eq!(
    (renamed.staged, renamed.moved, renamed.created),
    (0, 1, 0),
    "{renamed:?}"
  );
  let moved = scenario(&|model| {
    let sub = model_node(model, "/d/sub");
    model
      .entries
      .remove(&(model_node(model, "/d"), b"sub".to_vec()));
    model.entries.insert((0, b"top".to_vec()), sub);
  });
  assert_eq!(
    (moved.moved, moved.removed, moved.created),
    (1, 0, 0),
    "{moved:?}"
  );
  let links = scenario(&|model| {
    let x = model_node(model, "/x");
    model
      .entries
      .remove(&(model_node(model, "/d"), b"link".to_vec()));
    model
      .entries
      .insert((model_node(model, "/d/sub"), b"again".to_vec()), x);
    model.entries.insert((0, b"third".to_vec()), x);
  });
  assert_eq!(
    (links.moved, links.linked, links.removed),
    (1, 1, 0),
    "{links:?}"
  );
  let rewritten = scenario(&|model| {
    let b = model_node(model, "/d/b");
    model.nodes[b].body = Body::File(vec![b'B'; 70_000]);
  });
  assert_eq!(rewritten.bytes_written, 70_000, "{rewritten:?}");
  let reused = scenario(&|model| {
    // `/d/a` deleted and a new file made at its name and inode number, under a generation of the kernel's.
    let (d, a) = (model_node(model, "/d"), model_node(model, "/d/a"));
    let mut node = model.nodes[a].clone();
    node.body = Body::File(b"fresh".to_vec());
    node.generation = 5;
    node.built = false;
    model.nodes.push(node);
    let new = model.nodes.len() - 1;
    model.entries.insert((d, b"a".to_vec()), new);
  });
  assert_eq!((reused.created, reused.removed), (1, 1), "{reused:?}");
  let kind = scenario(&|model| {
    let d = model_node(model, "/d");
    model.entries.remove(&(d, b"a".to_vec()));
    let directory = model.add(Body::Directory, (0o700, 1, 2, [3, 4, 5]), 9);
    model.entries.insert((d, b"a".to_vec()), directory);
  });
  assert_eq!(
    (kind.created, kind.removed, kind.staged),
    (1, 1, 1),
    "{kind:?}"
  );
  let deleted = scenario(&|model| {
    let c = model_node(model, "/d/sub/c");
    model.entries.insert((0, b"c".to_vec()), c);
    model.remove(&(model_node(model, "/d"), b"sub".to_vec()));
  });
  assert_eq!((deleted.moved, deleted.removed), (1, 1), "{deleted:?}");
}

/// An image held in memory, read as a source.
struct Memory {
  bytes: Vec<u8>,
}

impl slates_image::ImageSource<std::convert::Infallible> for Memory {
  fn image_len(&self) -> u64 {
    u64::try_from(self.bytes.len()).unwrap()
  }

  fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), std::convert::Infallible> {
    let start = usize::try_from(offset).unwrap();
    buf.copy_from_slice(&self.bytes[start..start + buf.len()]);
    Ok(())
  }
}

/// The bytes of an image's files read out of the image in memory.
struct MemoryContent {
  image: Image,
  memory: Memory,
}

impl MergeContent for MemoryContent {
  fn read(
    &mut self,
    _store: &Store,
    ino: u32,
    offset: u64,
    buf: &mut [u8],
  ) -> Result<usize, ImageError> {
    self
      .image
      .read(&mut self.memory, ino, offset, buf)
      .map_err(|failed| match failed {
        slates_image::Failed::Image(refusal) => ImageError::Image(refusal),
        slates_image::Failed::Source(never) => match never {},
      })
  }
}

/// A writer's input and output over a model's files by node and an image in memory.
struct Writing<'a> {
  files: &'a BTreeMap<usize, Vec<u8>>,
  image: &'a mut Memory,
}

impl slates_image::ImageIo<std::convert::Infallible> for Writing<'_> {
  fn read_content(
    &mut self,
    node: NodeId,
    offset: u64,
    buf: &mut [u8],
  ) -> Result<usize, std::convert::Infallible> {
    let bytes = self.files.get(&node.index()).map_or(&[][..], Vec::as_slice);
    let start = usize::try_from(offset).unwrap().min(bytes.len());
    let take = buf.len().min(bytes.len() - start);
    buf[..take].copy_from_slice(&bytes[start..start + take]);
    Ok(take)
  }

  fn write_image(&mut self, offset: u64, bytes: &[u8]) -> Result<(), std::convert::Infallible> {
    let start = usize::try_from(offset).unwrap();
    self.image.bytes[start..start + bytes.len()].copy_from_slice(bytes);
    Ok(())
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig {
    cases: slates_test_seeds::cases(24),
    ..ProptestConfig::default()
  }))]

  /// A-160 (identity is only a hint). Do: apply a generated guest history to a volume's image tree, write the edited
  /// tree as a fresh image (every inode renumbered, every generation the build's, so identity lands on the wrong nodes
  /// throughout), and merge it into a clone of the snapshot, once trusting the image and once not. Expect the clone to
  /// hold the edited tree both times; untrusted, every node but the root made anew.
  #[test]
  fn an_image_renumbered_throughout_still_merges_to_its_tree(makes in makes(), history in edits()) {
    let mut store = common::store();
    let mut source = volume(&mut store, 7, NameEquivalence::Exact);
    populate(&mut source, &mut store, makes);
    let snapshot = source.snapshot(&mut store).unwrap();
    let built = build(&mut store, &source, snapshot);
    let (written, mut image) = read_image(&store, &built);
    let mut file = SnapshotFile {
      volume: &built.image_volume,
      store: &store,
      snapshot: built.snapshot,
      file: built.file,
      len: built.len,
    };
    let mut model = Model::of(&written, &mut |id| {
      let ImageKind::File { size } = written.tree.node(id).unwrap().kind else { panic!() };
      let mut bytes = vec![0u8; usize::try_from(size).unwrap()];
      image.read(&mut file, *written.inodes.get(id.index()).unwrap(), 0, &mut bytes).unwrap();
      bytes
    });
    for edit in history {
      apply(&mut model, edit);
    }
    let (edited, files, _) = model.tree();
    let expected = tree_entries(&edited.tree, &mut |id| files[edited.inodes.get(id.index()).unwrap()].clone());
    let by_node: BTreeMap<usize, Vec<u8>> = edited
      .tree
      .ids()
      .filter_map(|id| Some((id.index(), files.get(edited.inodes.get(id.index()).unwrap())?.clone())))
      .collect();
    let mut builder = Builder::new(edited.tree.clone(), CAPACITY, IDENTITY).unwrap();
    let mut memory = Memory { bytes: vec![0; usize::try_from(builder.image_bytes()).unwrap()] };
    loop {
      let mut io = Writing { files: &by_node, image: &mut memory };
      if let Progress::Done(_) = builder.step(&mut io, u64::MAX).unwrap() {
        break;
      }
    }
    let mut renumbered = Image::open(&mut memory).unwrap();
    let tree = renumbered.tree(&mut memory, MOST).unwrap();
    let mut content = MemoryContent { image: renumbered, memory };
    for trusted in [true, false] {
      let mut clone = Volume::clone_of(&store, &mut source, snapshot, config(9, NameEquivalence::Exact)).unwrap();
      let mut merge = Merge::new(built.base.clone(), tree.clone(), &clone, &store, trusted);
      let report = run(&mut merge, &mut clone, &mut store, &mut content).unwrap();
      prop_assert_eq!(volume_entries(&clone, &store), expected.clone());
      if !trusted {
        prop_assert_eq!(report.kept, 1);
        prop_assert_eq!(report.created, u64::try_from(tree.tree.node_count()).unwrap() - 1);
      }
    }
  }
}

/// Merges into a clone of the scenario base an image tree with one entry naming `node` in the root, made by the
/// guest; the merge's refusal.
fn refusal_of(name: Vec<u8>, node: Node) -> ImageError {
  let mut store = common::store();
  let mut source = scenario_base(&mut store);
  let snapshot = source.snapshot(&mut store).unwrap();
  let built = build(&mut store, &source, snapshot);
  let (written, _) = read_image(&store, &built);
  let mut builder = TreeBuilder::new(written.tree.node(NodeId::ROOT).unwrap().clone());
  let child = builder.add_node(node);
  builder.add_entry(NodeId::ROOT, name, child);
  let target = ImageTree {
    tree: builder.build().unwrap(),
    inodes: [2, 500].into_iter().collect(),
    generations: [0, 77].into_iter().collect(),
  };
  let mut clone = Volume::clone_of(
    &store,
    &mut source,
    snapshot,
    config(9, NameEquivalence::Exact),
  )
  .unwrap();
  let mut merge = Merge::new(built.base.clone(), target, &clone, &store, true);
  run(
    &mut merge,
    &mut clone,
    &mut store,
    &mut MapContent {
      files: BTreeMap::new(),
    },
  )
  .unwrap_err()
}

/// A-160 (what a volume cannot hold, refused by kind). Do: merge images whose one new entry has a name that is not
/// UTF-8, is a symlink whose target is not, is a character device, or carries a time past 2262 (beyond a volume's
/// nanoseconds in an `i64`, inside the image format's 2446). Expect each refused by its own kind, the path named.
#[test]
fn what_a_volume_cannot_hold_is_refused_by_kind() {
  let fifo = |kind: ImageKind| Node {
    kind,
    permissions: 0o644,
    uid: 1,
    gid: 1,
    atime: timestamp(0),
    mtime: timestamp(0),
    ctime: timestamp(0),
  };
  assert_eq!(
    refusal_of(b"bad\xff".to_vec(), fifo(ImageKind::Fifo)),
    ImageError::NameNotUtf8 {
      path: "/bad\u{fffd}".to_owned()
    }
  );
  assert_eq!(
    refusal_of(
      b"link".to_vec(),
      fifo(ImageKind::Symlink {
        target: b"\xfe".to_vec()
      })
    ),
    ImageError::TargetNotUtf8 {
      path: "/link".to_owned()
    }
  );
  assert_eq!(
    refusal_of(b"null".to_vec(), fifo(ImageKind::CharDevice { rdev: 259 })),
    ImageError::NodeUnsupported {
      path: "/null".to_owned()
    }
  );
  let mut late = fifo(ImageKind::Fifo);
  late.mtime = Timestamp {
    secs: i64::MAX / NANOS + 1,
    nanos: 0,
  };
  assert!(matches!(
    refusal_of(b"late".to_vec(), late),
    ImageError::Image(slates_image::ImageRefusal::TimeOutOfRange { .. })
  ));
}
