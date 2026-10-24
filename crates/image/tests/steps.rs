//! Bounded steps (A-160; §4.3): what lets the daemon build and read an image in slices. A tree is grown a directory
//! at a time and refused, by kind, when grown out of order; a builder lays a tree out a node at a time before it
//! writes a byte; and every inode the writer writes carries the generation a merge knows the build's nodes by.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::unwrap_in_result
)]

mod common;

use std::convert::Infallible;

use common::{Contents, Memory, node, sample, write};
use slates_image::format::{BUILT_GENERATION, LINK_MAX, inode};
use slates_image::layout::BLOCK_BYTES;
use slates_image::{
  Builder, Image, ImageIo, ImageRefusal, Kind, NodeId, Progress, TreeBuilder, TreeGrower,
};

/// Shape: the capacity of the tests' images.
const CAPACITY: u64 = 16 << 20;
/// Shape: the entries a read-back tree may hold.
const MOST: usize = 1 << 16;

/// Whether `refusal` is a growth out of order.
fn out_of_order(refusal: ImageRefusal) -> bool {
  matches!(refusal, ImageRefusal::GrowthOutOfOrder { .. })
}

/// A-160 (growth out of order). Do: grow a tree out of order — an entry with no directory open, a directory opened
/// before an entry names it, a directory opened while another is, a directory opened again once filled — and name the
/// root, a directory twice, and one name twice in a directory. Expect each refused by its own kind.
#[test]
fn growing_a_tree_out_of_order_is_refused_by_kind() {
  let mut grower = TreeGrower::new(node(Kind::Directory)).unwrap();
  let fifo = grower.add_node(node(Kind::Fifo)).unwrap();
  assert!(out_of_order(
    grower.add_entry(b"early".to_vec(), fifo).unwrap_err()
  ));
  let directory = grower.add_node(node(Kind::Directory)).unwrap();
  assert_eq!(
    grower.open(directory).unwrap_err(),
    ImageRefusal::Unreachable { node: 2 }
  );
  grower.open(NodeId::ROOT).unwrap();
  assert!(out_of_order(grower.open(NodeId::ROOT).unwrap_err()));
  assert_eq!(
    grower
      .add_entry(b"root".to_vec(), NodeId::ROOT)
      .unwrap_err(),
    ImageRefusal::DirectoryLinkedTwice { node: 0 }
  );
  grower.add_entry(b"dir".to_vec(), directory).unwrap();
  assert_eq!(
    grower.add_entry(b"again".to_vec(), directory).unwrap_err(),
    ImageRefusal::DirectoryLinkedTwice { node: 2 }
  );
  grower.add_entry(b"pipe".to_vec(), fifo).unwrap();
  assert_eq!(
    grower.add_entry(b"pipe".to_vec(), fifo).unwrap_err(),
    ImageRefusal::NameRepeated
  );
  grower.close().unwrap();
  assert!(out_of_order(grower.open(NodeId::ROOT).unwrap_err()));
}

/// A-160 (a tree finished early). Do: finish a tree with a directory still open, with a directory named and never
/// filled, and with a node no entry names. Expect each refused by its own kind.
#[test]
fn a_tree_finished_before_it_is_whole_is_refused_by_kind() {
  let mut still_open = TreeGrower::new(node(Kind::Directory)).unwrap();
  still_open.open(NodeId::ROOT).unwrap();
  assert!(out_of_order(still_open.finish().unwrap_err()));
  let mut unfilled = TreeGrower::new(node(Kind::Directory)).unwrap();
  let directory = unfilled.add_node(node(Kind::Directory)).unwrap();
  unfilled.open(NodeId::ROOT).unwrap();
  unfilled.add_entry(b"dir".to_vec(), directory).unwrap();
  unfilled.close().unwrap();
  assert!(out_of_order(unfilled.finish().unwrap_err()));
  let mut unnamed = TreeGrower::new(node(Kind::Directory)).unwrap();
  unnamed.add_node(node(Kind::Fifo)).unwrap();
  unnamed.open(NodeId::ROOT).unwrap();
  unnamed.close().unwrap();
  assert_eq!(
    unnamed.finish().unwrap_err(),
    ImageRefusal::Unreachable { node: 1 }
  );
}

/// A-160 (one check). Do: grow a tree a directory at a time, and give the batch builder the same entries in another
/// order. Expect the same tree.
#[test]
fn a_grown_tree_is_the_tree_the_batch_builder_makes() {
  let mut whole = TreeGrower::new(node(Kind::Directory)).unwrap();
  let fifo = whole.add_node(node(Kind::Fifo)).unwrap();
  let directory = whole.add_node(node(Kind::Directory)).unwrap();
  whole.open(NodeId::ROOT).unwrap();
  whole.add_entry(b"pipe".to_vec(), fifo).unwrap();
  whole.add_entry(b"dir".to_vec(), directory).unwrap();
  whole.close().unwrap();
  whole.open(directory).unwrap();
  whole.add_entry(b"link".to_vec(), fifo).unwrap();
  whole.close().unwrap();
  let mut builder = TreeBuilder::new(node(Kind::Directory));
  let fifo = builder.add_node(node(Kind::Fifo));
  let directory = builder.add_node(node(Kind::Directory));
  builder.add_entry(directory, b"link".to_vec(), fifo);
  builder.add_entry(NodeId::ROOT, b"dir".to_vec(), directory);
  builder.add_entry(NodeId::ROOT, b"pipe".to_vec(), fifo);
  assert_eq!(whole.finish().unwrap(), builder.build().unwrap());
}

/// A-160 (links held as they come). Do: name one FIFO [`LINK_MAX`] times, then once more. Expect the last refused
/// `TooManyLinks` the moment it comes.
#[test]
fn a_link_past_the_format_is_refused_when_it_comes() {
  let mut grower = TreeGrower::new(node(Kind::Directory)).unwrap();
  let fifo = grower.add_node(node(Kind::Fifo)).unwrap();
  grower.open(NodeId::ROOT).unwrap();
  for index in 0..LINK_MAX {
    grower
      .add_entry(format!("l{index}").into_bytes(), fifo)
      .unwrap();
  }
  assert_eq!(
    grower.add_entry(b"one-more".to_vec(), fifo).unwrap_err(),
    ImageRefusal::TooManyLinks {
      links: LINK_MAX + 1
    }
  );
}

/// An image sink that counts its writes, over contents with none.
struct CountingWrites<'m> {
  image: &'m mut Memory,
  writes: usize,
}

impl ImageIo<Infallible> for CountingWrites<'_> {
  fn read_content(
    &mut self,
    _node: NodeId,
    _offset: u64,
    _buf: &mut [u8],
  ) -> Result<usize, Infallible> {
    Ok(0)
  }

  fn write_image(&mut self, offset: u64, bytes: &[u8]) -> Result<(), Infallible> {
    self.writes += 1;
    let start = usize::try_from(offset).unwrap();
    self.image.bytes[start..start + bytes.len()].copy_from_slice(bytes);
    Ok(())
  }
}

/// A-160 (layout in steps). Do: build an image of 2,000 FIFOs in the smallest steps. Expect the layout to take at
/// least a step per node to measure and another to place before the first byte is written, so no step lays out more
/// than one node; and the image to read back with every FIFO.
#[test]
fn a_builder_lays_its_tree_out_a_node_a_step_before_it_writes() {
  let fifos = 2_000;
  let mut tree = TreeBuilder::new(node(Kind::Directory));
  for index in 0..fifos {
    let id = tree.add_node(node(Kind::Fifo));
    tree.add_entry(NodeId::ROOT, format!("p{index}").into_bytes(), id);
  }
  let tree = tree.build().unwrap();
  let mut builder = Builder::new(tree, CAPACITY, common::IDENTITY).unwrap();
  let mut image = Memory {
    bytes: vec![0; usize::try_from(builder.image_bytes()).unwrap()],
  };
  let mut steps_before_a_write = 0;
  loop {
    let mut io = CountingWrites {
      image: &mut image,
      writes: 0,
    };
    let progress = builder.step(&mut io, 1).unwrap();
    if io.writes > 0 {
      break;
    }
    steps_before_a_write += 1;
    assert_eq!(progress, Progress::More);
  }
  assert!(
    steps_before_a_write >= 2 * (fifos + 1),
    "{steps_before_a_write} steps of layout for {} nodes",
    fifos + 1
  );
  loop {
    let mut io = CountingWrites {
      image: &mut image,
      writes: 0,
    };
    if let Progress::Done(_) = builder.step(&mut io, u64::MAX).unwrap() {
      break;
    }
  }
  let mut opened = Image::open(&mut image).unwrap();
  let read = opened.tree(&mut image, MOST).unwrap();
  assert_eq!(read.tree.children(NodeId::ROOT).len(), fifos);
}

/// The byte offset of inode `ino` in `image` (group 0): its group descriptor's inode table, then the inode's place.
fn inode_at(image: &Memory, ino: u32) -> usize {
  let descriptor = usize::try_from(BLOCK_BYTES).unwrap();
  let table = u32::from_le_bytes(
    image.bytes[descriptor + 8..descriptor + 12]
      .try_into()
      .unwrap(),
  );
  usize::try_from(table).unwrap() * usize::try_from(BLOCK_BYTES).unwrap()
    + (usize::try_from(ino).unwrap() - 1) * 256
}

/// A-160 (the build's generation). Do: write the sample tree and read it back; then give one file's inode another
/// generation, as a guest kernel gives an inode it allocates, and read it again. Expect every inode the writer wrote to
/// carry [`BUILT_GENERATION`], and the changed one to read back changed, the rest untouched.
#[test]
fn every_written_inode_carries_the_built_generation_and_a_new_one_reads_back() {
  let (tree, contents): (_, Contents) = sample();
  let (mut image, _, _) = write(&tree, &contents, CAPACITY, u64::MAX);
  let mut opened = Image::open(&mut image).unwrap();
  let read = opened.tree(&mut image, MOST).unwrap();
  assert!(
    read
      .generations
      .iter()
      .all(|generation| *generation == BUILT_GENERATION),
    "{:?}",
    read.generations
  );
  let file = read
    .tree
    .ids()
    .find(|id| matches!(read.tree.node(*id).unwrap().kind, Kind::File { .. }))
    .unwrap();
  let ino = read.inodes[file.index()];
  let at = inode_at(&image, ino) + inode::GENERATION;
  image.bytes[at..at + 4].copy_from_slice(&0x5eed_0001_u32.to_le_bytes());
  let mut opened = Image::open(&mut image).unwrap();
  let again = opened.tree(&mut image, MOST).unwrap();
  for (index, generation) in again.generations.iter().enumerate() {
    let expected = if again.inodes[index] == ino {
      0x5eed_0001
    } else {
      BUILT_GENERATION
    };
    assert_eq!(*generation, expected, "inode {}", again.inodes[index]);
  }
}
