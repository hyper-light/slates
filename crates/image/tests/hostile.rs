//! Hostile input (A-160; §4.9): an image read back was written by a guest kernel slates does not trust, so every way
//! it can be wrong is refused by kind, and none makes the reader panic, loop or read past the image. Targeted cases
//! for each check, then every single-bit flip of the structures a reader trusts first.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

mod common;

use common::{Contents, Memory, node, write};
use slates_image::layout::BLOCK_BYTES;
use slates_image::{Failed, Image, ImageRefusal, Kind, NodeId, Tree, TreeBuilder};

/// Shape: the capacity of the hostile tests' image: small, so every bit of its first structures can be flipped.
const CAPACITY: u64 = 1 << 20;
/// Shape: the entries a read-back tree may hold.
const MOST: usize = 1 << 16;

/// The sample tree: a subdirectory holding a file and a symlink, a file in the root, a hard link.
fn sample() -> (Tree, Contents) {
  let mut builder = TreeBuilder::new(node(Kind::Directory));
  let mut contents = Contents::default();
  let sub = builder.add_node(node(Kind::Directory));
  builder.add_entry(NodeId::ROOT, b"sub".to_vec(), sub);
  let file = builder.add_node(node(Kind::File { size: 9000 }));
  contents.files.insert(file.index(), vec![7u8; 9000]);
  builder.add_entry(sub, b"file".to_vec(), file);
  builder.add_entry(NodeId::ROOT, b"link".to_vec(), file);
  let symlink = builder.add_node(node(Kind::Symlink {
    target: b"sub/file".to_vec(),
  }));
  builder.add_entry(sub, b"to-file".to_vec(), symlink);
  (builder.build().unwrap(), contents)
}

/// The sample's image.
fn image() -> Memory {
  let (tree, contents) = sample();
  write(&tree, &contents, CAPACITY, u64::MAX).0
}

/// What reading `image` whole comes to: the refusal, if any; every file read too.
fn read_whole(image: &mut Memory) -> Result<usize, ImageRefusal> {
  let refusal = |failed: Failed<std::convert::Infallible>| match failed {
    Failed::Image(refusal) => refusal,
    Failed::Source(never) => match never {},
  };
  let mut opened = Image::open(image).map_err(refusal)?;
  let read = opened.tree(image, MOST).map_err(refusal)?;
  for (index, node) in read.tree.nodes().iter().enumerate() {
    if let Kind::File { size } = node.kind {
      let mut bytes = vec![0u8; usize::try_from(size.min(1 << 20)).unwrap_or(0)];
      opened
        .read(image, read.inodes[index], 0, &mut bytes)
        .map_err(refusal)?;
    }
  }
  Ok(read.tree.nodes().len())
}

fn put16(image: &mut Memory, at: usize, value: u16) {
  image.bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn put32(image: &mut Memory, at: usize, value: u32) {
  image.bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn get32(image: &Memory, at: usize) -> u32 {
  u32::from_le_bytes(image.bytes[at..at + 4].try_into().unwrap())
}

/// A broken image: what was broken, how, and the refusal it must meet.
type Case = (
  &'static str,
  Box<dyn Fn(&mut Memory)>,
  fn(&ImageRefusal) -> bool,
);

/// Format: where the superblock and the first group descriptor lie in a 4 KiB-block image.
const SUPERBLOCK: usize = 1024;
/// See [`SUPERBLOCK`].
const DESCRIPTORS: usize = 4096;

/// The byte offset of inode `ino` in `image` (group 0).
fn inode_at(image: &Memory, ino: u32) -> usize {
  let table = usize::try_from(get32(image, DESCRIPTORS + 8)).unwrap();
  let block = usize::try_from(BLOCK_BYTES).unwrap();
  table * block + (usize::try_from(ino).unwrap() - 1) * 256
}

/// The byte offset of inode `ino`'s first data block.
fn first_block_at(image: &Memory, ino: u32) -> usize {
  usize::try_from(get32(image, inode_at(image, ino) + 0x28)).unwrap()
    * usize::try_from(BLOCK_BYTES).unwrap()
}

/// A-160 (each superblock check). Do: open the sample's image with one superblock field broken at a time — the
/// magic, the revision, the block size, the inode size, the first inode, the blocks per group, an unknown feature, the
/// clean flag, a block count past the image, an inode count that disagrees, inodes per group not whole blocks — and
/// with the image cut short. Expect each refused by its own kind.
#[test]
fn each_broken_superblock_field_is_refused_by_kind() {
  let cases: Vec<Case> = vec![
    (
      "magic",
      Box::new(|i| put16(i, SUPERBLOCK + 0x38, 0x1234)),
      |r| matches!(r, ImageRefusal::BadMagic { found: 0x1234 }),
    ),
    (
      "revision",
      Box::new(|i| put32(i, SUPERBLOCK + 0x4C, 0)),
      |r| {
        matches!(
          r,
          ImageRefusal::Unsupported {
            field: "revision",
            ..
          }
        )
      },
    ),
    (
      "block size",
      Box::new(|i| put32(i, SUPERBLOCK + 0x18, 0)),
      |r| {
        matches!(
          r,
          ImageRefusal::Unsupported {
            field: "block size",
            ..
          }
        )
      },
    ),
    (
      "inode size",
      Box::new(|i| put16(i, SUPERBLOCK + 0x58, 128)),
      |r| {
        matches!(
          r,
          ImageRefusal::Unsupported {
            field: "inode size",
            ..
          }
        )
      },
    ),
    (
      "first inode",
      Box::new(|i| put32(i, SUPERBLOCK + 0x54, 12)),
      |r| {
        matches!(
          r,
          ImageRefusal::Unsupported {
            field: "first inode",
            ..
          }
        )
      },
    ),
    (
      "blocks per group",
      Box::new(|i| put32(i, SUPERBLOCK + 0x20, 1024)),
      |r| {
        matches!(
          r,
          ImageRefusal::Unsupported {
            field: "blocks per group",
            ..
          }
        )
      },
    ),
    (
      "extents feature",
      Box::new(|i| put32(i, SUPERBLOCK + 0x60, 0x2 | 0x40)),
      |r| {
        matches!(
          r,
          ImageRefusal::FeaturesUnsupported {
            incompat: 0x40,
            ro_compat: 0
          }
        )
      },
    ),
    (
      "not clean",
      Box::new(|i| put16(i, SUPERBLOCK + 0x3A, 0)),
      |r| matches!(r, ImageRefusal::NotClean),
    ),
    (
      "block count",
      Box::new(|i| put32(i, SUPERBLOCK + 0x04, u32::MAX)),
      |r| matches!(r, ImageRefusal::Truncated { .. }),
    ),
    (
      "inode count",
      Box::new(|i| put32(i, SUPERBLOCK, 12345)),
      |r| {
        matches!(
          r,
          ImageRefusal::Geometry {
            reason: "inode count"
          }
        )
      },
    ),
    (
      "inodes per group",
      Box::new(|i| put32(i, SUPERBLOCK + 0x28, 17)),
      |r| {
        matches!(
          r,
          ImageRefusal::Geometry {
            reason: "inodes per group"
          }
        )
      },
    ),
    ("cut short", Box::new(|i| i.bytes.truncate(1500)), |r| {
      matches!(r, ImageRefusal::Truncated { .. })
    }),
  ];
  for (what, break_it, expected) in cases {
    let mut broken = image();
    break_it(&mut broken);
    let refusal = read_whole(&mut broken).expect_err(what);
    assert!(expected(&refusal), "{what}: {refusal:?}");
  }
}

/// A-160 (each structure past the superblock). Do: break, one at a time, the inode table's place, a file's block
/// pointer, a file's flags (extents), a file's link count (a free inode named by an entry), a directory record's
/// length, a record's name length, a name (a slash in it), an entry naming the root, and an entry naming its own
/// directory (a loop); and read with a bound below the sample's entries. Expect each refused by kind.
#[test]
fn each_broken_structure_is_refused_by_kind() {
  let clean = image();
  // The sample's inodes: root 2, `sub` 11, `file` 12, `to-file` 13.
  let sub_block = first_block_at(&clean, 11);
  let root_block = first_block_at(&clean, 2);
  let cases: Vec<Case> = vec![
    (
      "table outside its group",
      Box::new(|i| put32(i, DESCRIPTORS + 8, 100_000)),
      |r| matches!(r, ImageRefusal::Geometry { .. }),
    ),
    (
      "block past the end",
      Box::new(|i| {
        let at = inode_at(i, 12) + 0x28;
        put32(i, at, 1_000_000)
      }),
      |r| matches!(r, ImageRefusal::BlockOutOfRange { block: 1_000_000 }),
    ),
    (
      "extents flag",
      Box::new(|i| {
        let at = inode_at(i, 12) + 0x20;
        put32(i, at, 0x0008_0000)
      }),
      |r| matches!(r, ImageRefusal::InodeUnsupported { ino: 12, .. }),
    ),
    (
      "a free inode named",
      Box::new(|i| {
        let at = inode_at(i, 12) + 0x1A;
        put16(i, at, 0)
      }),
      |r| matches!(r, ImageRefusal::InodeUnsupported { ino: 12, .. }),
    ),
    (
      "record length zero",
      Box::new(move |i| put16(i, root_block + 4, 0)),
      |r| {
        matches!(
          r,
          ImageRefusal::DirectoryCorrupt {
            ino: 2,
            reason: "record length"
          }
        )
      },
    ),
    (
      "name past its record",
      Box::new(move |i| i.bytes[root_block + 6] = 200),
      |r| matches!(r, ImageRefusal::DirectoryCorrupt { ino: 2, .. }),
    ),
    (
      "a slash in a name",
      Box::new(move |i| {
        // The root's third record is `link` (after `.` and `..`, each 12 bytes): make its first byte a slash.
        i.bytes[root_block + 24 + 8] = b'/';
      }),
      |r| {
        matches!(
          r,
          ImageRefusal::DirectoryCorrupt {
            ino: 2,
            reason: "name"
          }
        )
      },
    ),
    (
      "an entry naming the root",
      Box::new(move |i| put32(i, sub_block + 24, 2)),
      |r| matches!(r, ImageRefusal::DirectoryLinkedTwice { node: 2 }),
    ),
    (
      "a directory inside itself",
      Box::new(move |i| put32(i, sub_block + 24, 11)),
      |r| matches!(r, ImageRefusal::DirectoryLinkedTwice { .. }),
    ),
  ];
  for (what, break_it, expected) in cases {
    let mut broken = clean.clone();
    break_it(&mut broken);
    let refusal = read_whole(&mut broken).expect_err(what);
    assert!(expected(&refusal), "{what}: {refusal:?}");
  }
  let mut bounded = clean.clone();
  let mut opened = Image::open(&mut bounded).unwrap();
  assert_eq!(
    opened.tree(&mut bounded, 2).unwrap_err(),
    Failed::Image(ImageRefusal::TreeTooLarge { most: 2 })
  );
}

/// A-160 (every bit flip). Do: flip each single bit, in turn, of the superblock, the first group descriptor, the root's
/// inode and the root's directory block, and read the image whole (the tree and every file). Expect each read to give a
/// tree or a typed refusal — never a panic, a loop or a read past the image — with both outcomes reached, and every
/// flip of the magic refused.
#[test]
fn every_bit_flip_of_the_trusted_structures_reads_or_is_refused() {
  let clean = image();
  let regions = [
    (SUPERBLOCK, 1024),
    (DESCRIPTORS, 32),
    (inode_at(&clean, 2), 256),
    (
      first_block_at(&clean, 2),
      usize::try_from(BLOCK_BYTES).unwrap(),
    ),
  ];
  let (mut read, mut refused) = (0usize, 0usize);
  for (start, length) in regions {
    for byte in start..start + length {
      for bit in 0..8 {
        let mut flipped = clean.clone();
        flipped.bytes[byte] ^= 1 << bit;
        match read_whole(&mut flipped) {
          Ok(_) => read += 1,
          Err(_) => refused += 1,
        }
      }
    }
  }
  // Measured 2026-10-23: 42,623 flips read and 641 refused; most bits are counts, times and padding a reader need not
  // trust. Each of the magic's 16 bits must be refused, so at least that many are; and some flips must still read.
  assert!(
    read > 0 && refused >= 16,
    "both outcomes reached: {read} read, {refused} refused"
  );
  for bit in 0..16 {
    let mut flipped = clean.clone();
    flipped.bytes[SUPERBLOCK + 0x38 + bit / 8] ^= 1 << (bit % 8);
    assert!(
      matches!(read_whole(&mut flipped), Err(ImageRefusal::BadMagic { .. })),
      "magic bit {bit}"
    );
  }
}
