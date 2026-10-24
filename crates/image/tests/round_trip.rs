//! The round-trip oracle (A-160): a tree written as an image and read back is the same tree, at the format's full
//! precision — every name, kind, permission, owner, nanosecond time, device number, symlink target, hard link and
//! byte. Generated trees by proptest (with its seeds kept by `slates-test-seeds`), files at every block-map boundary
//! by hand, and the determinism gate: one tree written twice, in different step sizes, is the same bytes.
// Test harness code: an unwrap here is a failed test; proptest's strategy union carries an `Arc` (a test harness, the
// third D-8 exception, as in `slates-vfs`'s `model.rs`).
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::disallowed_types,
  clippy::unwrap_in_result
)]

mod common;

use std::convert::Infallible;

use common::{Canonical, Contents, IDENTITY, Memory, canonical, node, sample, write};
use proptest::prelude::*;
use slates_image::format::{DIRENT_HEADER_BYTES, INDIRECT_LEVELS, NAME_MAX};
use slates_image::layout::{BLOCK_BYTES, INODE_BYTES, POINTERS};
use slates_image::{
  Failed, Image, ImageRefusal, ImageSource, ImageTree, Kind, Node, NodeId, Timestamp, Tree,
  TreeBuilder, Walk, Walked,
};

/// Shape: the capacity of a generated tree's image: room for its files (each below 64 KiB here) many times over.
const GENERATED_CAPACITY: u64 = 16 << 20;
/// Shape: a step budget smaller than a block, so every generated image is written one structure a step.
const ONE_BLOCK_A_STEP: u64 = 1;
/// Shape: a step budget past any test image, so the image is written in one step.
const ONE_STEP: u64 = u64::MAX;
/// Shape: the entries a test tree may hold when read back.
const MOST_ENTRIES: usize = 1 << 20;

/// Derived: the most bytes one unit of a walk reads from its source: an entry's record with the longest name, the
/// inode it names and a slow symlink's block, the block reached through every level of indirect blocks.
const MOST_A_UNIT_READS: u64 = (DIRENT_HEADER_BYTES + NAME_MAX) as u64
  + INODE_BYTES
  + BLOCK_BYTES * (1 + INDIRECT_LEVELS as u64);

/// An image source that counts the bytes read from it.
struct Counting<'m> {
  image: &'m mut Memory,
  read: u64,
}

impl ImageSource<Infallible> for Counting<'_> {
  fn image_len(&self) -> u64 {
    self.image.image_len()
  }

  fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), Infallible> {
    self.read += u64::try_from(buf.len()).unwrap();
    self.image.read_at(offset, buf)
  }
}

/// The tree `image` holds, walked in the smallest steps: one unit each, none reading past [`MOST_A_UNIT_READS`].
/// Returns the tree and the steps taken.
fn walk_in_smallest_steps(image: &mut Memory) -> (ImageTree, usize) {
  let mut counting = Counting { image, read: 0 };
  let mut opened = Image::open(&mut counting).unwrap();
  let mut walk = Walk::new(&mut opened, &mut counting, MOST_ENTRIES).unwrap();
  let mut steps = 0;
  loop {
    steps += 1;
    counting.read = 0;
    let walked = walk.step(&mut opened, &mut counting, 1).unwrap();
    assert!(
      counting.read <= MOST_A_UNIT_READS,
      "a step of one unit read {} bytes",
      counting.read
    );
    if let Walked::Done(tree) = walked {
      return (tree, steps);
    }
  }
}

/// The canonical form of the tree `image` holds, with its files' bytes read from it. The tree is walked whole and in
/// the smallest steps, and both walks must read the same tree, inode for inode.
fn read_back(image: &mut Memory) -> Vec<Canonical> {
  let (stepped, steps) = walk_in_smallest_steps(image);
  let mut opened = Image::open(image).unwrap();
  let read = opened.tree(image, MOST_ENTRIES).unwrap();
  assert_eq!(
    stepped, read,
    "a walk in steps reads the tree a whole walk reads"
  );
  // Non-vacuity: every entry was a step of its own, and every directory's open and close one each.
  assert!(
    steps > read.tree.nodes().len(),
    "{steps} steps for {} nodes",
    read.tree.nodes().len()
  );
  let inodes = read.inodes.clone();
  canonical(&read.tree, &mut |id: NodeId, size| {
    let mut bytes = vec![0u8; usize::try_from(size).unwrap()];
    let got = opened
      .read(image, inodes[id.index()], 0, &mut bytes)
      .unwrap();
    assert_eq!(got, bytes.len(), "a file reads back whole");
    bytes
  })
}

/// The canonical form of `tree`, with its files' bytes from `contents`.
fn expected(tree: &Tree, contents: &Contents) -> Vec<Canonical> {
  canonical(tree, &mut |id, _| {
    contents.files.get(&id.index()).cloned().unwrap_or_default()
  })
}

/// A generated node's permissions, owner, group and access, modification and change times.
type Attributes = (u16, u32, u32, [Timestamp; 3]);

/// A generated non-root node: its spec, the directory it goes in, its name, its attributes.
type Generated = (Spec, usize, Vec<u8>, Attributes);

/// A generated node's attributes: any permissions, owners and times the format holds.
fn attributes() -> impl Strategy<Value = Attributes> {
  let time = (
    -(1_i64 << 31)..(1_i64 << 34) - (1_i64 << 31),
    0u32..1_000_000_000,
  )
    .prop_map(|(secs, nanos)| Timestamp { secs, nanos });
  (
    0u16..=0o7777,
    any::<u32>(),
    any::<u32>(),
    [time.clone(), time.clone(), time],
  )
}

/// What a generated non-root node is, and its bytes for a file.
#[derive(Clone, Debug)]
enum Spec {
  Directory,
  File(Vec<u8>),
  Symlink(Vec<u8>),
  Device {
    block: bool,
    major: u32,
    minor: u32,
  },
  Fifo,
  Socket,
  /// A hard link to the `n`th file made so far (wrapping).
  HardLink(usize),
}

/// A generated node: its spec, the directory it goes in (the `n`th made so far, wrapping), its name, and attributes.
fn specs() -> impl Strategy<Value = Vec<Generated>> {
  let spec = prop_oneof![
    3 => Just(Spec::Directory),
    5 => prop::collection::vec(any::<u8>(), 0..70_000).prop_map(Spec::File),
    2 => prop::collection::vec(1u8..=255, 1..200).prop_map(Spec::Symlink),
    1 => (any::<bool>(), 0u32..4096, 0u32..(1 << 20)).prop_map(|(block, major, minor)| Spec::Device { block, major, minor }),
    1 => Just(Spec::Fifo),
    1 => Just(Spec::Socket),
    2 => any::<usize>().prop_map(Spec::HardLink),
  ];
  // Any of these bytes in any order, so names sort in every way; `.` and `..` are no entry's name, so they are mended.
  let name = prop::collection::vec(prop::sample::select(b"abcAB0._-\xc3\xa9".to_vec()), 1..12)
    .prop_map(|mut name| {
      if name == b"." || name == b".." {
        name.push(b'x');
      }
      name
    });
  prop::collection::vec((spec, any::<usize>(), name, attributes()), 0..40)
}

/// The tree and contents `specs` describe; names repeated within a directory are made unique by a suffix.
fn build(specs: Vec<Generated>) -> (Tree, Contents) {
  let mut builder = TreeBuilder::new(node(Kind::Directory));
  let mut directories = vec![NodeId::ROOT];
  let mut files: Vec<NodeId> = Vec::new();
  let mut contents = Contents::default();
  let mut names: std::collections::BTreeSet<(usize, Vec<u8>)> = std::collections::BTreeSet::new();
  for (index, (spec, parent, mut name, (permissions, uid, gid, [atime, mtime, ctime]))) in
    specs.into_iter().enumerate()
  {
    let parent = directories[parent % directories.len()];
    if !names.insert((parent.index(), name.clone())) {
      name.extend_from_slice(format!("~{index}").as_bytes());
      names.insert((parent.index(), name.clone()));
    }
    let make = |kind: Kind| Node {
      kind,
      permissions,
      uid,
      gid,
      atime,
      mtime,
      ctime,
    };
    let child = match spec {
      Spec::HardLink(target) if !files.is_empty() => files[target % files.len()],
      Spec::HardLink(_) | Spec::Fifo => builder.add_node(make(Kind::Fifo)),
      Spec::Directory => {
        let id = builder.add_node(make(Kind::Directory));
        directories.push(id);
        id
      }
      Spec::File(bytes) => {
        let id = builder.add_node(make(Kind::File {
          size: u64::try_from(bytes.len()).unwrap(),
        }));
        contents.files.insert(id.index(), bytes);
        files.push(id);
        id
      }
      Spec::Symlink(target) => builder.add_node(make(Kind::Symlink { target })),
      Spec::Device {
        block,
        major,
        minor,
      } => {
        let rdev = slates_image::layout::new_encode(major, minor).unwrap();
        builder.add_node(make(if block {
          Kind::BlockDevice { rdev }
        } else {
          Kind::CharDevice { rdev }
        }))
      }
      Spec::Socket => builder.add_node(make(Kind::Socket)),
    };
    builder.add_entry(parent, name, child);
  }
  (builder.build().unwrap(), contents)
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig {
    cases: slates_test_seeds::cases(96),
    ..ProptestConfig::default()
  }))]

  /// A-160 (the oracle). Do: generate a tree of up to 40 nodes (directories, files up to 70,000 bytes, symlinks either
  /// side of the 60-byte fast limit, devices, FIFOs, sockets, hard links) with any attributes the format holds; write
  /// it one structure a step and read it back. Expect the same canonical tree: every name, kind, attribute, nanosecond
  /// time, target, device number, link and byte.
  #[test]
  fn a_generated_tree_reads_back_as_written(specs in specs()) {
    let (tree, contents) = build(specs);
    let (mut image, _, steps) = write(&tree, &contents, GENERATED_CAPACITY, ONE_BLOCK_A_STEP);
    prop_assert!(steps > 1, "written in steps");
    prop_assert_eq!(read_back(&mut image), expected(&tree, &contents));
  }
}

/// A-160 (every block-map boundary). Do: write files whose block counts sit at and either side of each boundary of an
/// inode's map — none, one, the 12 direct blocks, the first single-indirect block, the single level full, the first
/// double-indirect block and a second single block under it — plus a file with a zero block inside it, and read each
/// back. Expect every byte back, and the zero block left unwritten.
#[test]
fn files_at_every_block_map_boundary_read_back_whole() {
  let block = usize::try_from(BLOCK_BYTES).unwrap();
  let pointers = usize::try_from(POINTERS).unwrap();
  let blocks = [
    0,
    1,
    11,
    12,
    13,
    12 + pointers,
    12 + pointers + 1,
    12 + pointers + pointers + 1,
  ];
  let mut builder = TreeBuilder::new(node(Kind::Directory));
  let mut contents = Contents::default();
  for (index, count) in blocks.iter().enumerate() {
    // A byte past the last whole block, so the final block is partial too.
    let length = count * block + usize::from(*count > 0) * 7;
    let bytes: Vec<u8> = (0..length)
      .map(|at| u8::try_from((at * 31 + index) % 251).unwrap())
      .collect();
    let id = builder.add_node(node(Kind::File {
      size: u64::try_from(length).unwrap(),
    }));
    contents.files.insert(id.index(), bytes);
    builder.add_entry(NodeId::ROOT, format!("f{index}").into_bytes(), id);
  }
  let mut holed = vec![1u8; 3 * block];
  holed[block..2 * block].fill(0);
  let id = builder.add_node(node(Kind::File {
    size: u64::try_from(holed.len()).unwrap(),
  }));
  contents.files.insert(id.index(), holed);
  builder.add_entry(NodeId::ROOT, b"holed".to_vec(), id);
  let tree = builder.build().unwrap();
  let (mut image, written, _) = write(&tree, &contents, 64 << 20, ONE_STEP);
  assert_eq!(read_back(&mut image), expected(&tree, &contents));
  assert!(written.blocks_used > 0);
}

/// A-160 (the determinism gate). Do: write one generated-size tree twice, once in one step and once one structure a
/// step. Expect identical bytes; and a different identity changes them.
#[test]
fn one_tree_writes_the_same_bytes_in_any_steps() {
  let (tree, contents) = sample();
  let (once, _, one) = write(&tree, &contents, GENERATED_CAPACITY, ONE_STEP);
  let (stepped, _, many) = write(&tree, &contents, GENERATED_CAPACITY, ONE_BLOCK_A_STEP);
  assert_eq!(one, 1);
  assert!(many > 10, "{many} steps");
  assert!(once.bytes == stepped.bytes, "the same bytes in any steps");
  let other = slates_image::Identity {
    uuid: [1; 16],
    ..IDENTITY
  };
  let (image, _, _) = common::write_with(&tree, &contents, GENERATED_CAPACITY, ONE_STEP, other);
  assert!(image.bytes != once.bytes, "the identity is in the bytes");
}

/// Format: the BLAKE3 digest of [`sample`]'s image at [`GENERATED_CAPACITY`] with [`IDENTITY`]: a golden vector, so a
/// change to any byte the writer makes is seen and justified. The same on macOS arm64 and Linux x86_64 (2026-10-23), and
/// an image `e2fsck -fn` passes. Regenerate with `-- --ignored print_the_golden_digest`.
const GOLDEN: &str = "aae68f50874b4f2a938a52d0b3fed6a49fa79a577a23d421db528cd1456e382d";

/// A-160 (the golden vector). Do: write the sample tree. Expect its image's digest to be [`GOLDEN`].
#[test]
fn the_sample_image_has_its_golden_digest() {
  let (tree, contents) = sample();
  let (image, _, _) = write(&tree, &contents, GENERATED_CAPACITY, ONE_STEP);
  assert_eq!(blake3::hash(&image.bytes).to_hex().to_string(), GOLDEN);
}

/// Prints the sample image's digest, for [`GOLDEN`].
#[test]
#[ignore = "a regenerator, run by hand when the format changes on purpose"]
fn print_the_golden_digest() {
  let (tree, contents) = sample();
  let (image, _, _) = write(&tree, &contents, GENERATED_CAPACITY, ONE_STEP);
  println!("{}", blake3::hash(&image.bytes).to_hex());
}

/// The refusal a tree with one entry naming a FIFO `name` meets.
fn refusal_of_name(name: Vec<u8>) -> ImageRefusal {
  let mut builder = TreeBuilder::new(node(Kind::Directory));
  let id = builder.add_node(node(Kind::Fifo));
  builder.add_entry(NodeId::ROOT, name, id);
  builder.build().unwrap_err()
}

/// The refusal a tree with one entry naming `child` meets.
fn refusal_of_node(child: Node) -> ImageRefusal {
  let mut builder = TreeBuilder::new(node(Kind::Directory));
  let id = builder.add_node(child);
  builder.add_entry(NodeId::ROOT, b"child".to_vec(), id);
  builder.build().unwrap_err()
}

/// A-160 (names refused before a byte is written). Do: build trees whose one name has a slash, is 256 bytes, or is
/// `..`, and one naming a FIFO twice under one name. Expect each refused by kind.
#[test]
fn a_name_the_format_cannot_hold_is_refused_by_kind() {
  assert_eq!(
    refusal_of_name(b"a/b".to_vec()),
    ImageRefusal::NameInvalid { len: 3 }
  );
  assert_eq!(
    refusal_of_name(vec![b'n'; 256]),
    ImageRefusal::NameInvalid { len: 256 }
  );
  assert_eq!(
    refusal_of_name(b"..".to_vec()),
    ImageRefusal::NameInvalid { len: 2 }
  );
  let mut builder = TreeBuilder::new(node(Kind::Directory));
  let one = builder.add_node(node(Kind::Fifo));
  builder.add_entry(NodeId::ROOT, b"same".to_vec(), one);
  builder.add_entry(NodeId::ROOT, b"same".to_vec(), one);
  assert_eq!(builder.build().unwrap_err(), ImageRefusal::NameRepeated);
}

/// A-160 (nodes refused before a byte is written). Do: build trees with a file root, a directory named twice, a time
/// past 2446, an empty symlink; and lay out a tree in an image too small for it. Expect each refused by kind.
#[test]
fn a_tree_the_format_cannot_hold_is_refused_by_kind() {
  let file_root = TreeBuilder::new(node(Kind::File { size: 0 })).build();
  assert_eq!(file_root.unwrap_err(), ImageRefusal::RootNotDirectory);
  let mut builder = TreeBuilder::new(node(Kind::Directory));
  let directory = builder.add_node(node(Kind::Directory));
  builder.add_entry(NodeId::ROOT, b"a".to_vec(), directory);
  builder.add_entry(NodeId::ROOT, b"b".to_vec(), directory);
  assert!(matches!(
    builder.build().unwrap_err(),
    ImageRefusal::DirectoryLinkedTwice { .. }
  ));
  let mut late = node(Kind::Fifo);
  late.mtime = Timestamp {
    secs: 1 << 35,
    nanos: 0,
  };
  assert_eq!(
    refusal_of_node(late),
    ImageRefusal::TimeOutOfRange { secs: 1 << 35 }
  );
  assert_eq!(
    refusal_of_node(node(Kind::Symlink { target: Vec::new() })),
    ImageRefusal::SymlinkInvalid { len: 0 }
  );
  let (tree, contents) = sample();
  let too_small = common::try_write(&tree, &contents, 8 * BLOCK_BYTES, ONE_STEP, IDENTITY)
    .map(|_| ())
    .unwrap_err();
  assert!(
    matches!(too_small, Failed::Image(ImageRefusal::TooSmall { .. })),
    "{too_small:?}"
  );
}

/// A-160 (several groups). Do: write the sample tree into an image of three block groups (a superblock copy in group
/// 1, a short last group) and read it back. Expect the same canonical tree.
#[test]
fn an_image_of_several_groups_reads_back_as_written() {
  let (tree, contents) = sample();
  let (mut image, written, _) = write(&tree, &contents, 300 << 20, ONE_STEP);
  assert_eq!(read_back(&mut image), expected(&tree, &contents));
  assert_eq!(written.image_bytes, 300 << 20);
}

/// A-160 (inodes in a second group). Do: write 5,000 files, more than one group's inodes, into an image of two groups,
/// and read every file back. Expect the same canonical tree: the second group's descriptor locates its inode table.
/// Red before the fix of 2026-10-23, when each descriptor was written 36 bytes long and the second group's table was
/// read from its inode bitmap.
#[test]
fn an_image_whose_inodes_span_groups_reads_back_as_written() {
  let mut builder = TreeBuilder::new(node(Kind::Directory));
  let mut contents = Contents::default();
  for index in 0..5_000u32 {
    let bytes = index.to_le_bytes().to_vec();
    let id = builder.add_node(node(Kind::File { size: 4 }));
    contents.files.insert(id.index(), bytes);
    builder.add_entry(NodeId::ROOT, format!("f{index:05}").into_bytes(), id);
  }
  let tree = builder.build().unwrap();
  let (mut image, written, _) = write(&tree, &contents, 130 << 20, ONE_STEP);
  assert!(
    written.inodes_used > 4_160,
    "past the first group's inodes: {}",
    written.inodes_used
  );
  assert_eq!(read_back(&mut image), expected(&tree, &contents));
}
