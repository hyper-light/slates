//! Writing a tree as an ext2 image, in steps a caller bounds (§4.3: bounded work everywhere). A [`Builder`] plans the
//! geometry when it is made, from the capacity and the node count alone. Each [`Builder::step`] then works until a
//! byte budget is spent: first the layout, a node at a time — each node's blocks measured, the total held to the
//! capacity, then each node's blocks placed — and then the writing: the superblock and the group descriptors (with
//! their `sparse_super` copies), each group's bitmaps, the inode-table blocks that hold an inode in use, then each
//! node's blocks in order, its data first and its indirect blocks after. The caller sizes the image before the first
//! step ([`Builder::image_bytes`]); every byte not written is a zero, so free space, unused inodes and a file's zero
//! blocks cost the sink nothing.
//!
//! Inode numbers follow node order: the root is inode 2, and node `n` is inode `10 + n`, so the inodes in use are
//! exactly the reserved ones and a prefix after them. A file system's identity (its UUID and creation time) is the
//! caller's, so one tree and one identity always make the same bytes (the determinism gate).
//!
//! No unit costs what a directory, the tree or the geometry holds (A-160): a directory is measured a block of entries
//! a unit, the groups' blocks in use are counted a group a unit, the nodes with no data block are passed a node a
//! unit, and the per-node layout grows a segment at a time ([`Segments`]). Before (2026-10-24, `image_bench`'s least
//! time of each unit over five rounds, Apple M5 Max): measuring a directory of 100,000 entries took one unit of
//! 175 µs, and a bitmap block, set a bit at a time, 42–60 µs.

use std::collections::BTreeMap;

use crate::error::{Failed, ImageRefusal};
use crate::format::{
  self, BLOCK_POINTER_BYTES, DIRENT_HEADER_BYTES, DIRENT_PAD, DYNAMIC_REV, ERRORS_CONTINUE,
  EXTRA_ISIZE, FIRST_INO, FLAGS_SIGNED_HASH, Fields, GROUP_DESC_BYTES, INCOMPAT_FILETYPE, MAGIC,
  OS_LINUX, RO_COMPAT_LARGE_FILE, RO_COMPAT_SPARSE_SUPER, ROOT_INO, STATE_VALID, SUPERBLOCK_BYTES,
  SUPERBLOCK_OFFSET,
};
use crate::layout::{
  self, Allocator, BLOCK_BYTES, BLOCKS_PER_GROUP, Geometry, INODE_BYTES, INODES_PER_BLOCK,
  Indirect, LOG_BLOCK_SIZE, Map, Run, SECTORS_PER_BLOCK,
};
use crate::segments::Segments;
use crate::tree::{Kind, NodeId, Timestamp, Tree};

/// Where a file's bytes come from and the image's bytes go while an image is written: one object, since a caller's
/// files and its image usually live in the same store and are borrowed together.
pub trait ImageIo<E> {
  /// Fills `buf` with node `node`'s bytes from `offset`, returning how many it filled; fewer than asked only at the
  /// file's end.
  fn read_content(&mut self, node: NodeId, offset: u64, buf: &mut [u8]) -> Result<usize, E>;
  /// Writes `bytes` at `offset` of the image. The image is [`Builder::image_bytes`] long before the first write, and
  /// every byte never written reads as zero.
  fn write_image(&mut self, offset: u64, bytes: &[u8]) -> Result<(), E>;
}

/// A file system's identity: what makes two images of one tree tell themselves apart, and what keeps one tree's image
/// the same bytes every time it is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
  /// The file system's UUID (`s_uuid`).
  pub uuid: [u8; 16],
  /// When it was made (`s_mkfs_time`, `s_wtime`, `s_lastcheck`).
  pub created: Timestamp,
}

/// What an image holds once written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Written {
  /// Its length in bytes.
  pub image_bytes: u64,
  /// The blocks in use: the file system's structures and the tree's blocks.
  pub blocks_used: u64,
  /// The inodes in use, the reserved ones included.
  pub inodes_used: u64,
}

/// Where a [`Builder`] stands after a step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
  /// More remains: step again.
  More,
  /// The image is written whole.
  Done(Written),
}

/// One node's blocks: its map and the runs its slots fill, a run for each group they span.
#[derive(Clone, Debug)]
struct Placement {
  map: Map,
  runs: Segments<Run>,
}

/// A directory being measured: the next of its entries to pack (`.` and `..` are its first two) and the blocks its
/// entries before that fill.
#[derive(Clone, Copy, Debug)]
struct Packing {
  entry: usize,
  blocks: u64,
}

/// Where a builder is in its layout and its writing.
#[derive(Clone, Debug)]
enum Cursor {
  /// Measuring node `node`'s blocks; for a directory, how far its entries are packed.
  Measure {
    node: usize,
    directory: Option<Packing>,
  },
  /// Placing node `node`'s blocks: the runs placed so far (one a group they span), and the slots they cover.
  Place {
    node: usize,
    placed: Option<(Segments<Run>, u64)>,
  },
  /// Counting the blocks in use from group `group` on, `used` counted before it.
  Tally { group: u64, used: u64 },
  /// The superblock copy of group `group`, the first group with a copy at or after it.
  Superblock { group: u64 },
  /// Block `block` of group `group`'s copy of the group descriptor table.
  Descriptors { group: u64, block: u64 },
  /// Group `group`'s block bitmap, or its inode bitmap.
  Bitmap { group: u64, inodes: bool },
  /// Block `block` of group `group`'s inode table.
  InodeTable { group: u64, block: u64 },
  /// Seeking the first node at or after `node` that has a data block.
  Seek { node: usize },
  /// Data block `index` of node `node`; for a directory, `entry` is the next of its entries to pack.
  Data {
    node: usize,
    index: u64,
    entry: usize,
  },
  /// Indirect block `index` of node `node` ([`Map::indirect_at`]).
  Indirect { node: usize, index: u64 },
  /// Written whole.
  Done,
}

/// An image being written: its geometry planned at construction, laid out and written in [`Builder::step`]s.
#[derive(Debug)]
pub struct Builder {
  tree: Tree,
  geometry: Geometry,
  identity: Identity,
  /// Each node's block map, measured.
  maps: Segments<Map>,
  /// The slots of the nodes measured so far.
  slots: u64,
  placements: Segments<Placement>,
  /// The allocator's cursor after the nodes placed so far: each group's data blocks before it are in use, none after.
  allocated: u64,
  /// The directories whose inodes are in each group that holds one (`bg_used_dirs_count`).
  directories: BTreeMap<u64, u64>,
  /// The blocks in use, once counted.
  used: Option<u64>,
  cursor: Cursor,
}

impl Builder {
  /// Plans `tree`'s image of `capacity` bytes with `identity`, or says why the capacity cannot hold its inodes. Whether
  /// it holds the tree's blocks is known once they are measured, in the first steps.
  pub fn new(tree: Tree, capacity: u64, identity: Identity) -> Result<Builder, ImageRefusal> {
    let nodes = u64::try_from(tree.node_count()).unwrap_or(u64::MAX);
    // The reserved inodes (the root among them), then one per node after the root.
    let inodes_needed = u64::from(FIRST_INO)
      .saturating_sub(1)
      .saturating_add(nodes.saturating_sub(1));
    let geometry = Geometry::plan(capacity, inodes_needed)?;
    Ok(Builder {
      directories: BTreeMap::new(),
      allocated: geometry.data_start(0),
      tree,
      geometry,
      identity,
      maps: Segments::new(),
      slots: 0,
      placements: Segments::new(),
      used: None,
      cursor: Cursor::Measure {
        node: 0,
        directory: None,
      },
    })
  }

  /// The tree being written.
  pub fn tree(&self) -> &Tree {
    &self.tree
  }

  /// The image's length: what the caller sizes it to before the first step.
  pub fn image_bytes(&self) -> u64 {
    self.geometry.blocks.saturating_mul(BLOCK_BYTES)
  }

  /// The image's geometry.
  pub fn geometry(&self) -> &Geometry {
    &self.geometry
  }

  /// Writes until at least `budget` bytes are written or the image is whole: at least one structure or block a step,
  /// so every step makes progress.
  pub fn step<E>(&mut self, io: &mut dyn ImageIo<E>, budget: u64) -> Result<Progress, Failed<E>> {
    let mut spent = 0u64;
    loop {
      if matches!(self.cursor, Cursor::Done) {
        return Ok(Progress::Done(self.written()));
      }
      spent = spent.saturating_add(self.advance(io)?);
      if spent >= budget {
        return Ok(if matches!(self.cursor, Cursor::Done) {
          Progress::Done(self.written())
        } else {
          Progress::More
        });
      }
    }
  }

  /// What the image holds.
  fn written(&self) -> Written {
    Written {
      image_bytes: self.image_bytes(),
      blocks_used: self.used.unwrap_or(0),
      inodes_used: self.inodes_used(),
    }
  }

  /// Releases one piece of what the builder holds — a node's placement, its map, then the tree's pieces — and says
  /// whether anything is left: a builder of any size is released a piece a call, never in one drop (A-160; §4.3).
  pub fn shed(&mut self) -> bool {
    self.cursor = Cursor::Done;
    self.placements.shed()
      || self.maps.shed()
      || self.tree.shed()
      || self.directories.pop_first().is_some()
  }

  /// Lays out or writes what the cursor names and moves it on; returns the bytes it cost.
  fn advance<E>(&mut self, io: &mut dyn ImageIo<E>) -> Result<u64, Failed<E>> {
    match std::mem::replace(&mut self.cursor, Cursor::Done) {
      Cursor::Measure { node, directory } => self.measure(node, directory).map_err(Failed::Image),
      Cursor::Place { node, placed } => self.place(node, placed).map_err(Failed::Image),
      Cursor::Tally { group, used } => Ok(self.tally(group, used)),
      Cursor::Seek { node } => Ok(self.seek(node)),
      Cursor::Superblock { group } => self.write_superblock(group, io),
      Cursor::Descriptors { group, block } => self.write_descriptors(group, block, io),
      Cursor::Bitmap { group, inodes } => self.write_bitmap(group, inodes, io),
      Cursor::InodeTable { group, block } => self.write_inode_block(group, block, io),
      Cursor::Data { node, index, entry } => self.write_data(node, index, entry, io),
      Cursor::Indirect { node, index } => self.write_indirect(node, index, io),
      Cursor::Done => Ok(0),
    }
  }

  /// Measures node `node`'s blocks — a directory's a block of its entries a call, `directory` saying how far they are
  /// packed — and counts a directory among its group's; after the last node, holds the whole to the capacity. A unit is
  /// charged its inode's bytes, and for a directory the block its entries were packed into: the work of measuring it.
  fn measure(&mut self, node: usize, directory: Option<Packing>) -> Result<u64, ImageRefusal> {
    let Some(id) = self.tree.id(node) else {
      let need = self.geometry.meta_total().saturating_add(self.slots);
      if need > self.geometry.blocks {
        return Err(ImageRefusal::TooSmall {
          need,
          have: self.geometry.blocks,
        });
      }
      self.cursor = Cursor::Place {
        node: 0,
        placed: None,
      };
      return Ok(0);
    };
    let is_directory = self
      .tree
      .node(id)
      .is_some_and(|node| node.kind == Kind::Directory);
    let blocks = if is_directory {
      let packing = directory.unwrap_or(Packing {
        entry: 0,
        blocks: 0,
      });
      let next = block_end(&self.tree, id, packing.entry);
      let blocks = packing.blocks.saturating_add(1);
      if directory_entry(&self.tree, id, next).is_some() {
        self.cursor = Cursor::Measure {
          node,
          directory: Some(Packing {
            entry: next,
            blocks,
          }),
        };
        return Ok(BLOCK_BYTES);
      }
      let group = u64::from(ino_of(id))
        .saturating_sub(1)
        .checked_div(self.geometry.inodes_per_group.max(1))
        .unwrap_or(u64::MAX);
      let count = self.directories.entry(group).or_insert(0);
      *count = count.saturating_add(1);
      blocks
    } else {
      data_blocks(&self.tree, id)
    };
    let map = Map::new(blocks)?;
    self.slots = self.slots.saturating_add(map.slots());
    self.maps.push(map);
    self.cursor = Cursor::Measure {
      node: node.saturating_add(1),
      directory: None,
    };
    Ok(if is_directory {
      INODE_BYTES.saturating_add(BLOCK_BYTES)
    } else {
      INODE_BYTES
    })
  }

  /// Places node `node`'s blocks at the allocator's cursor a run a call — a group's share of them, `placed` holding the
  /// runs so far and the slots they cover — so a node spanning many groups is placed a group a unit; after the last
  /// node, the counting begins. Charged the node's inode's bytes.
  fn place(
    &mut self,
    node: usize,
    placed: Option<(Segments<Run>, u64)>,
  ) -> Result<u64, ImageRefusal> {
    let Some(map) = self.maps.get(node).copied() else {
      self.cursor = Cursor::Tally { group: 0, used: 0 };
      return Ok(0);
    };
    let (mut runs, mut covered) = placed.unwrap_or_default();
    let slots = map.slots();
    if covered < slots {
      let mut allocator = Allocator::at(&self.geometry, self.allocated);
      let run = allocator.take_run(covered, slots.saturating_sub(covered))?;
      self.allocated = allocator.next();
      covered = covered.saturating_add(run.len);
      runs.push(run);
      if covered < slots {
        self.cursor = Cursor::Place {
          node,
          placed: Some((runs, covered)),
        };
        return Ok(INODE_BYTES);
      }
    }
    self.placements.push(Placement { map, runs });
    self.cursor = Cursor::Place {
      node: node.saturating_add(1),
      placed: None,
    };
    Ok(INODE_BYTES)
  }

  /// Adds group `group`'s blocks in use to `used`; after the last group, keeps the whole and the writing begins. A
  /// group a unit, so a geometry of any size is counted in bounded units.
  fn tally(&mut self, group: u64, used: u64) -> u64 {
    if group >= self.geometry.groups {
      self.used = Some(used);
      self.cursor = Cursor::Superblock { group: 0 };
      return 0;
    }
    self.cursor = Cursor::Tally {
      group: group.saturating_add(1),
      used: used.saturating_add(self.blocks_used_in(group)),
    };
    GROUP_DESC_BYTES_U64
  }

  /// Moves to node `node`'s data if it has a block, or on to the next node: a node a unit, so a run of nodes with no
  /// data block (empty files, devices) is passed in bounded units.
  fn seek(&mut self, node: usize) -> u64 {
    let Some(placement) = self.placements.get(node) else {
      self.cursor = Cursor::Done;
      return 0;
    };
    self.cursor = if placement.map.data > 0 {
      Cursor::Data {
        node,
        index: 0,
        entry: 0,
      }
    } else {
      Cursor::Seek {
        node: node.saturating_add(1),
      }
    };
    INODE_BYTES
  }

  /// The superblock copy of the first group at or after `group` that carries one; then its descriptor table.
  fn write_superblock<E>(&mut self, group: u64, io: &mut dyn ImageIo<E>) -> Result<u64, Failed<E>> {
    let Some(group) = (group..self.geometry.groups).find(|group| Geometry::has_super(*group))
    else {
      self.cursor = Cursor::Bitmap {
        group: 0,
        inodes: false,
      };
      return Ok(0);
    };
    let at = if group == 0 {
      SUPERBLOCK_OFFSET
    } else {
      self.block_offset(self.geometry.group_start(group))?
    };
    let bytes = self.superblock(group)?;
    io.write_image(at, &bytes).map_err(Failed::Source)?;
    self.cursor = Cursor::Descriptors { group, block: 0 };
    Ok(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
  }

  /// Block `block` of group `group`'s descriptor table copy; then the next, or the next group's superblock copy.
  fn write_descriptors<E>(
    &mut self,
    group: u64,
    block: u64,
    io: &mut dyn ImageIo<E>,
  ) -> Result<u64, Failed<E>> {
    let first = self
      .geometry
      .group_start(group)
      .saturating_add(1)
      .saturating_add(block);
    let bytes = self.descriptor_block(block);
    io.write_image(self.block_offset(first)?, &bytes)
      .map_err(Failed::Source)?;
    let next = block.saturating_add(1);
    self.cursor = if next < self.geometry.gdt_blocks {
      Cursor::Descriptors { group, block: next }
    } else {
      Cursor::Superblock {
        group: group.saturating_add(1),
      }
    };
    Ok(BLOCK_BYTES)
  }

  /// Group `group`'s block or inode bitmap; then the next bitmap, or the inode tables.
  fn write_bitmap<E>(
    &mut self,
    group: u64,
    inodes: bool,
    io: &mut dyn ImageIo<E>,
  ) -> Result<u64, Failed<E>> {
    let (block, bits) = if inodes {
      (self.geometry.inode_bitmap(group), self.inode_bitmap(group))
    } else {
      (self.geometry.block_bitmap(group), self.block_bitmap(group))
    };
    io.write_image(self.block_offset(block)?, &bits)
      .map_err(Failed::Source)?;
    self.cursor = match (inodes, group.saturating_add(1) < self.geometry.groups) {
      (false, _) => Cursor::Bitmap {
        group,
        inodes: true,
      },
      (true, true) => Cursor::Bitmap {
        group: group.saturating_add(1),
        inodes: false,
      },
      (true, false) => Cursor::InodeTable { group: 0, block: 0 },
    };
    Ok(BLOCK_BYTES)
  }

  /// Block `block` of group `group`'s inode table, if it holds an inode in use; then the next such block, or the
  /// nodes' data.
  fn write_inode_block<E>(
    &mut self,
    group: u64,
    block: u64,
    io: &mut dyn ImageIo<E>,
  ) -> Result<u64, Failed<E>> {
    let used = self.inodes_used_in(group);
    if block.saturating_mul(INODES_PER_BLOCK) >= used {
      let next = group.saturating_add(1);
      self.cursor = if next < self.geometry.groups && self.inodes_used_in(next) > 0 {
        Cursor::InodeTable {
          group: next,
          block: 0,
        }
      } else {
        self.first_data(0)
      };
      return Ok(0);
    }
    let first_ino = group
      .saturating_mul(self.geometry.inodes_per_group)
      .saturating_add(block.saturating_mul(INODES_PER_BLOCK))
      .saturating_add(1);
    let mut bytes = Vec::with_capacity(usize::try_from(BLOCK_BYTES).unwrap_or(0));
    for ino in first_ino..first_ino.saturating_add(INODES_PER_BLOCK) {
      bytes.extend_from_slice(&self.inode(ino)?);
    }
    let at = self.geometry.inode_table(group).saturating_add(block);
    io.write_image(self.block_offset(at)?, &bytes)
      .map_err(Failed::Source)?;
    self.cursor = Cursor::InodeTable {
      group,
      block: block.saturating_add(1),
    };
    Ok(BLOCK_BYTES)
  }

  /// The cursor seeking the data of the first node at or after `node` that has a block ([`Builder::seek`]).
  fn first_data(&self, node: usize) -> Cursor {
    Cursor::Seek { node }
  }

  /// Data block `index` of node `node`; then its next, or its indirect blocks, or the next node.
  fn write_data<E>(
    &mut self,
    node: usize,
    index: u64,
    entry: usize,
    io: &mut dyn ImageIo<E>,
  ) -> Result<u64, Failed<E>> {
    let placement = self
      .placements
      .get(node)
      .ok_or(ImageRefusal::NodeUnknown { node })?;
    let block = layout::physical(&placement.runs, Map::data_slot(index))
      .ok_or(ImageRefusal::NodeUnknown { node })?;
    let data = placement.map.data;
    let id = self.node_id(node)?;
    let kind = self
      .tree
      .node(id)
      .map(|node| node.kind.clone())
      .ok_or(ImageRefusal::NodeUnknown { node })?;
    let mut bytes = vec![0u8; usize::try_from(BLOCK_BYTES).unwrap_or(0)];
    let mut next_entry = entry;
    match kind {
      Kind::Directory => next_entry = self.pack_directory(id, entry, &mut bytes)?,
      Kind::Symlink { target } => {
        if let Some(head) = bytes.get_mut(..target.len()) {
          head.copy_from_slice(&target);
        }
      }
      Kind::File { size } => fill_file(io, id, index, size, &mut bytes)?,
      Kind::CharDevice { .. } | Kind::BlockDevice { .. } | Kind::Fifo | Kind::Socket => {}
    }
    // A zero block of a file is left unwritten: the image reads it as zero, and a sparse source stays cheap.
    let skip = matches!(
      self.tree.node(id).map(|node| &node.kind),
      Some(Kind::File { .. })
    ) && bytes.iter().all(|byte| *byte == 0);
    if !skip {
      io.write_image(self.block_offset(block)?, &bytes)
        .map_err(Failed::Source)?;
    }
    let next = index.saturating_add(1);
    self.cursor = if next < data {
      Cursor::Data {
        node,
        index: next,
        entry: next_entry,
      }
    } else {
      self.indirect_or_next(node)
    };
    Ok(BLOCK_BYTES)
  }

  /// The cursor at node `node`'s indirect blocks, or at the next node's data when it has none.
  fn indirect_or_next(&self, node: usize) -> Cursor {
    let indirect = self
      .placements
      .get(node)
      .is_some_and(|placement| placement.map.has_indirect());
    if indirect {
      Cursor::Indirect { node, index: 0 }
    } else {
      self.first_data(node.saturating_add(1))
    }
  }

  /// Indirect block `index` of node `node`, computed from its index ([`Map::indirect_at`]); then its next, or the next
  /// node.
  fn write_indirect<E>(
    &mut self,
    node: usize,
    index: u64,
    io: &mut dyn ImageIo<E>,
  ) -> Result<u64, Failed<E>> {
    let placement = self
      .placements
      .get(node)
      .ok_or(ImageRefusal::NodeUnknown { node })?;
    let Some(block) = placement.map.indirect_at(index) else {
      self.cursor = self.first_data(node.saturating_add(1));
      return Ok(0);
    };
    let mut fields = Fields::with_capacity(usize::try_from(BLOCK_BYTES).unwrap_or(0));
    let slot = match block {
      Indirect::Data { slot, first, count } => {
        for data in first..first.saturating_add(count) {
          fields.u32(pointer(&placement.runs, Map::data_slot(data), node)?);
        }
        slot
      }
      Indirect::Blocks { slot, children } => {
        for child in children {
          fields.u32(pointer(&placement.runs, child, node)?);
        }
        slot
      }
    };
    let at = layout::physical(&placement.runs, slot).ok_or(ImageRefusal::NodeUnknown { node })?;
    let bytes = fields.finish(usize::try_from(BLOCK_BYTES).unwrap_or(0));
    io.write_image(self.block_offset(at)?, &bytes)
      .map_err(Failed::Source)?;
    self.cursor = Cursor::Indirect {
      node,
      index: index.saturating_add(1),
    };
    Ok(BLOCK_BYTES)
  }

  /// Packs directory `id`'s entries from `entry` into one block (`.` and `..` are its first two entries); returns the
  /// next entry to pack. The block's last entry reaches to the block's end, as the format requires. Each record is
  /// written in place, so a block costs its entries' bytes and no allocation.
  fn pack_directory(
    &self,
    id: NodeId,
    entry: usize,
    block: &mut [u8],
  ) -> Result<usize, ImageRefusal> {
    let corrupt = ImageRefusal::NodeUnknown { node: id.index() };
    let end = block_end(&self.tree, id, entry);
    let mut offset = 0usize;
    let mut last: Option<usize> = None;
    for index in entry..end {
      let (name, child) = directory_entry(&self.tree, id, index).ok_or(corrupt.clone())?;
      let length = record_length(name.len());
      let record = block
        .get_mut(offset..offset.saturating_add(length))
        .ok_or(corrupt.clone())?;
      put(record, 0, &ino_of(child).to_le_bytes()).ok_or(corrupt.clone())?;
      let length_field = u16::try_from(length).unwrap_or(u16::MAX).to_le_bytes();
      put(record, format::DIRENT_RECORD_LENGTH_AT, &length_field).ok_or(corrupt.clone())?;
      let name_length = u8::try_from(name.len()).unwrap_or(u8::MAX);
      put(
        record,
        format::DIRENT_NAME_LENGTH_AT,
        &[name_length, file_type(&self.tree, child)],
      )
      .ok_or(corrupt.clone())?;
      put(record, DIRENT_HEADER_BYTES, name).ok_or(corrupt.clone())?;
      last = Some(offset);
      offset = offset.saturating_add(length);
    }
    // The last record's length reaches the end of the block.
    if let Some(at) = last {
      let block_bytes = usize::try_from(BLOCK_BYTES).unwrap_or(0);
      let reach = u16::try_from(block_bytes.saturating_sub(at)).unwrap_or(u16::MAX);
      put(
        block,
        at.saturating_add(format::DIRENT_RECORD_LENGTH_AT),
        &reach.to_le_bytes(),
      )
      .ok_or(corrupt)?;
    }
    Ok(end)
  }

  /// The superblock as group `group`'s copy carries it.
  fn superblock(&self, group: u64) -> Result<Vec<u8>, ImageRefusal> {
    let (free_blocks, free_inodes) = (self.free_blocks(), self.free_inodes());
    let (created_low, created_high) = superblock_time(self.identity.created);
    let mut fields = Fields::with_capacity(SUPERBLOCK_BYTES);
    fields
      .u32(narrow(self.geometry.inodes())?)
      .u32(narrow(self.geometry.blocks)?)
      .u32(0) // s_r_blocks_count: no blocks reserved, so a guest's ordinary user may fill the image
      .u32(narrow(free_blocks)?)
      .u32(narrow(free_inodes)?)
      .u32(0) // s_first_data_block: 0 for blocks larger than 1 KiB
      .u32(LOG_BLOCK_SIZE)
      .u32(LOG_BLOCK_SIZE) // s_log_cluster_size: a cluster is a block without bigalloc
      .u32(narrow(BLOCKS_PER_GROUP)?)
      .u32(narrow(BLOCKS_PER_GROUP)?)
      .u32(narrow(self.geometry.inodes_per_group)?)
      .u32(0) // s_mtime: never mounted
      .u32(created_low)
      .u16(0) // s_mnt_count
      .u16(u16::MAX) // s_max_mnt_count: -1, no forced check
      .u16(MAGIC)
      .u16(STATE_VALID)
      .u16(ERRORS_CONTINUE)
      .u16(0) // s_minor_rev_level
      .u32(created_low) // s_lastcheck
      .u32(0) // s_checkinterval
      .u32(OS_LINUX)
      .u32(DYNAMIC_REV)
      .u16(0) // s_def_resuid
      .u16(0) // s_def_resgid
      .u32(FIRST_INO)
      .u16(narrow16(INODE_BYTES)?)
      // s_block_group_nr is 16 bits; e2fsprogs writes a copy's group number truncated to them (`closefs.c`), and only
      // a copy past group 65,535 of an image over 8 TiB is affected. Informational: nothing reads it to locate data.
      .u16(u16::try_from(group & u64::from(u16::MAX)).unwrap_or(0))
      .u32(0) // s_feature_compat
      .u32(INCOMPAT_FILETYPE)
      .u32(RO_COMPAT_SPARSE_SUPER | RO_COMPAT_LARGE_FILE)
      .bytes(&self.identity.uuid)
      .zeros(format::superblock::VOLUME_NAME_BYTES + format::superblock::LAST_MOUNTED_BYTES)
      .u32(0) // s_algorithm_usage_bitmap
      .u8(0) // s_prealloc_blocks
      .u8(0) // s_prealloc_dir_blocks
      .u16(0) // s_reserved_gdt_blocks: no resize inode
      .zeros(format::superblock::JOURNAL_UUID_BYTES)
      .u32(0) // s_journal_inum
      .u32(0) // s_journal_dev
      .u32(0) // s_last_orphan
      .zeros(format::superblock::HASH_SEED_BYTES) // no directory index
      .u8(0) // s_def_hash_version
      .u8(0) // s_jnl_backup_type
      .u16(0) // s_desc_size: 32-byte descriptors
      .u32(0) // s_default_mount_opts
      .u32(0) // s_first_meta_bg
      .u32(created_low) // s_mkfs_time
      .zeros(format::superblock::JOURNAL_BACKUP_BYTES)
      .u32(0) // s_blocks_count_hi
      .u32(0) // s_r_blocks_count_hi
      .u32(0) // s_free_blocks_hi
      .u16(EXTRA_ISIZE) // s_min_extra_isize
      .u16(EXTRA_ISIZE) // s_want_extra_isize
      .u32(FLAGS_SIGNED_HASH);
    // The rest is zero up to the high bytes of the 64-bit times.
    fields.zeros(format::superblock::TIME_HIGH.saturating_sub(fields.len()));
    fields
      .u8(created_high) // s_wtime_hi
      .u8(0) // s_mtime_hi
      .u8(created_high) // s_mkfs_time_hi
      .u8(created_high); // s_lastcheck_hi
    Ok(fields.finish(SUPERBLOCK_BYTES))
  }

  /// Block `block` of the group descriptor table: the descriptors of the groups it holds, zero after the last.
  fn descriptor_block(&self, block: u64) -> Vec<u8> {
    let per_block = DESCRIPTORS_PER_BLOCK;
    let first = block.saturating_mul(per_block);
    let mut fields = Fields::with_capacity(usize::try_from(BLOCK_BYTES).unwrap_or(0));
    for group in first..first.saturating_add(per_block).min(self.geometry.groups) {
      let free_blocks = self
        .geometry
        .group_blocks(group)
        .saturating_sub(self.blocks_used_in(group));
      let free_inodes = self
        .geometry
        .inodes_per_group
        .saturating_sub(self.inodes_used_in(group));
      // One descriptor, finished to exactly its length: the fields in use, then zeros for `bg_flags` and the checksum
      // fields. Until 2026-10-23 the padding was miscounted and each descriptor ran 36 bytes, so every group past the
      // first was read four bytes off: e2fsck found group 1's block bitmap "at 0" and its inode table in its bitmap.
      let mut descriptor = Fields::with_capacity(GROUP_DESC_BYTES);
      descriptor
        .u32(u32::try_from(self.geometry.block_bitmap(group)).unwrap_or(u32::MAX))
        .u32(u32::try_from(self.geometry.inode_bitmap(group)).unwrap_or(u32::MAX))
        .u32(u32::try_from(self.geometry.inode_table(group)).unwrap_or(u32::MAX))
        .u16(u16::try_from(free_blocks).unwrap_or(u16::MAX))
        .u16(u16::try_from(free_inodes).unwrap_or(u16::MAX))
        .u16(u16::try_from(self.directories_in(group)).unwrap_or(u16::MAX));
      fields.bytes(&descriptor.finish(GROUP_DESC_BYTES));
    }
    fields.finish(usize::try_from(BLOCK_BYTES).unwrap_or(0))
  }

  /// The blocks of group `group` in use: its structures, and the data blocks the allocator handed out in it.
  fn blocks_used_in(&self, group: u64) -> u64 {
    let start = self.geometry.group_start(group);
    let end = start.saturating_add(self.geometry.group_blocks(group));
    let data_start = self.geometry.data_start(group);
    let data_used = self
      .allocated
      .clamp(data_start, end)
      .saturating_sub(data_start);
    self.geometry.meta_blocks(group).saturating_add(data_used)
  }

  /// Group `group`'s block bitmap: its structures and its allocated prefix set, and the bits past the group's last
  /// block (only the last group's differ) set as padding.
  fn block_bitmap(&self, group: u64) -> Vec<u8> {
    let used = self.blocks_used_in(group);
    let blocks = self.geometry.group_blocks(group);
    bitmap(used, blocks)
  }

  /// The inodes in use: the reserved ones and one per node after the root.
  fn inodes_used(&self) -> u64 {
    let nodes = u64::try_from(self.tree.node_count()).unwrap_or(u64::MAX);
    u64::from(FIRST_INO)
      .saturating_sub(1)
      .saturating_add(nodes.saturating_sub(1))
  }

  /// The inodes of group `group` in use: a prefix of its inodes.
  fn inodes_used_in(&self, group: u64) -> u64 {
    let first = group.saturating_mul(self.geometry.inodes_per_group);
    self
      .inodes_used()
      .saturating_sub(first)
      .min(self.geometry.inodes_per_group)
  }

  /// Group `group`'s inode bitmap: its inodes in use set, and the bits past its last inode set as padding.
  fn inode_bitmap(&self, group: u64) -> Vec<u8> {
    bitmap(self.inodes_used_in(group), self.geometry.inodes_per_group)
  }

  /// The directories whose inodes are in group `group`.
  fn directories_in(&self, group: u64) -> u64 {
    self.directories.get(&group).copied().unwrap_or(0)
  }

  /// The free blocks of the file system: its blocks less those in use, counted before the writing began.
  fn free_blocks(&self) -> u64 {
    self.geometry.blocks.saturating_sub(self.used.unwrap_or(0))
  }

  /// The free inodes of the file system.
  fn free_inodes(&self) -> u64 {
    self.geometry.inodes().saturating_sub(self.inodes_used())
  }

  /// The node inode `ino` holds, if any: the root for inode 2, node `ino - 10` past the reserved inodes.
  fn node_of(&self, ino: u64) -> Option<NodeId> {
    if ino == u64::from(ROOT_INO) {
      return Some(NodeId::ROOT);
    }
    let index = ino.checked_sub(u64::from(FIRST_INO).saturating_sub(1))?;
    if index == 0 {
      return None;
    }
    self.tree.id(usize::try_from(index).ok()?)
  }

  /// The id of node index `node`.
  fn node_id(&self, node: usize) -> Result<NodeId, ImageRefusal> {
    self.tree.id(node).ok_or(ImageRefusal::NodeUnknown { node })
  }

  /// Inode `ino` as the inode table holds it: zero for a reserved inode with no node, else its node's.
  fn inode(&self, ino: u64) -> Result<Vec<u8>, ImageRefusal> {
    let length = usize::try_from(INODE_BYTES).unwrap_or(0);
    let Some(id) = self.node_of(ino) else {
      return Ok(vec![0u8; length]);
    };
    let node = self
      .tree
      .node(id)
      .ok_or(ImageRefusal::NodeUnknown { node: id.index() })?;
    let placement = self
      .placements
      .get(id.index())
      .ok_or(ImageRefusal::NodeUnknown { node: id.index() })?;
    let (type_bits, size) = match &node.kind {
      Kind::Directory => (
        format::S_IFDIR,
        placement.map.data.saturating_mul(BLOCK_BYTES),
      ),
      Kind::File { size } => (format::S_IFREG, *size),
      Kind::Symlink { target } => (
        format::S_IFLNK,
        u64::try_from(target.len()).unwrap_or(u64::MAX),
      ),
      Kind::CharDevice { .. } => (format::S_IFCHR, 0),
      Kind::BlockDevice { .. } => (format::S_IFBLK, 0),
      Kind::Fifo => (format::S_IFIFO, 0),
      Kind::Socket => (format::S_IFSOCK, 0),
    };
    let (atime, atime_extra) = layout::encode_time(node.atime)?;
    let (ctime, ctime_extra) = layout::encode_time(node.ctime)?;
    let (mtime, mtime_extra) = layout::encode_time(node.mtime)?;
    let sectors = placement.map.slots().saturating_mul(SECTORS_PER_BLOCK);
    let links = u16::try_from(self.tree.links(id)).map_err(|_| ImageRefusal::TooManyLinks {
      links: self.tree.links(id),
    })?;
    let mut fields = Fields::with_capacity(length);
    fields
      .u16(type_bits | (node.permissions & format::PERMISSION_BITS))
      .u16(low16(node.uid))
      .u32(low32(size))
      .u32(atime)
      .u32(ctime)
      .u32(mtime)
      .u32(0) // i_dtime
      .u16(low16(node.gid))
      .u16(links)
      .u32(u32::try_from(sectors).map_err(|_| ImageRefusal::FileTooLarge { size })?)
      .u32(0) // i_flags
      .u32(0) // osd1
      .bytes(&self.block_pointers(id, &node.kind, placement)?)
      .u32(format::BUILT_GENERATION)
      .u32(0) // i_file_acl
      .u32(if matches!(node.kind, Kind::File { .. }) {
        high32(size)
      } else {
        0
      })
      .u32(0) // i_faddr
      .u16(0) // l_i_blocks_hi
      .u16(0) // l_i_file_acl_high
      .u16(high16(node.uid))
      .u16(high16(node.gid))
      .u16(0) // l_i_checksum_lo
      .u16(0) // l_i_reserved
      .u16(EXTRA_ISIZE)
      .u16(0) // i_checksum_hi
      .u32(ctime_extra)
      .u32(mtime_extra)
      .u32(atime_extra)
      .u32(ctime) // i_crtime: the tree keeps no birth time; the change time stands in, so the bytes stay determined
      .u32(ctime_extra)
      .u32(0) // i_version_hi
      .u32(0); // i_projid
    Ok(fields.finish(length))
  }

  /// The 60 bytes of `i_block`: the block pointers of a mapped node, a fast symlink's target, or a device's number.
  fn block_pointers(
    &self,
    id: NodeId,
    kind: &Kind,
    placement: &Placement,
  ) -> Result<Vec<u8>, ImageRefusal> {
    let mut fields = Fields::with_capacity(BLOCK_POINTER_BYTES);
    match kind {
      Kind::Symlink { target } if placement.map.data == 0 => {
        fields.bytes(target);
      }
      Kind::CharDevice { rdev } | Kind::BlockDevice { rdev } => {
        let (first, second) = layout::encode_rdev(*rdev);
        fields.u32(first).u32(second);
      }
      _ => {
        for slot in placement.map.pointer_slots() {
          fields.u32(pointer(&placement.runs, slot, id.index())?);
        }
      }
    }
    Ok(fields.finish(BLOCK_POINTER_BYTES))
  }

  /// The byte offset of block `block`.
  fn block_offset(&self, block: u64) -> Result<u64, ImageRefusal> {
    block
      .checked_mul(BLOCK_BYTES)
      .ok_or(ImageRefusal::CapacityTooLarge { blocks: block })
  }
}

/// Format: the group descriptors one block of the table holds.
const DESCRIPTORS_PER_BLOCK: u64 = BLOCK_BYTES / GROUP_DESC_BYTES as u64;
/// Format: a group descriptor's bytes, the charge of a unit that counts one group's blocks in use.
const GROUP_DESC_BYTES_U64: u64 = GROUP_DESC_BYTES as u64;

/// The data blocks a node that is not a directory takes: a file's bytes, a slow symlink's one block. A directory's are
/// measured a block of entries at a time ([`Builder::measure`]).
fn data_blocks(tree: &Tree, id: NodeId) -> u64 {
  match tree.node(id).map(|node| &node.kind) {
    Some(Kind::File { size }) => size.div_ceil(BLOCK_BYTES),
    Some(Kind::Symlink { target }) if target.len() >= BLOCK_POINTER_BYTES => 1,
    _ => 0,
  }
}

/// The entry after the last of directory `id`'s entries from `entry` that one block holds: an entry never spans two
/// blocks, and one always fits an empty block (a record is at most 264 bytes), so every block takes at least one.
fn block_end(tree: &Tree, id: NodeId, entry: usize) -> usize {
  let block_bytes = usize::try_from(BLOCK_BYTES).unwrap_or(0);
  let mut offset = 0usize;
  let mut next = entry;
  while let Some((name, _)) = directory_entry(tree, id, next) {
    let length = record_length(name.len());
    if offset > 0 && offset.saturating_add(length) > block_bytes {
      break;
    }
    offset = offset.saturating_add(length);
    next = next.saturating_add(1);
  }
  next
}

/// Entry `index` of directory `id` as the image keeps its entries: `.`, `..`, then its children in name order.
fn directory_entry(tree: &Tree, id: NodeId, index: usize) -> Option<(&[u8], NodeId)> {
  match index {
    0 => Some((b".", id)),
    1 => Some((b"..", tree.parent(id))),
    _ => tree.entry(id, index.saturating_sub(2)),
  }
}

/// Writes `bytes` at `at` of `into`; `None` when they do not fit.
fn put(into: &mut [u8], at: usize, bytes: &[u8]) -> Option<()> {
  into
    .get_mut(at..at.checked_add(bytes.len())?)?
    .copy_from_slice(bytes);
  Some(())
}

/// A directory record's length for a name of `name_len` bytes: the header and the name, padded to four bytes.
fn record_length(name_len: usize) -> usize {
  DIRENT_HEADER_BYTES
    .saturating_add(name_len)
    .div_ceil(DIRENT_PAD)
    .saturating_mul(DIRENT_PAD)
}

/// The inode number of node `id`: 2 for the root, `10 + index` for every other.
pub(crate) fn ino_of(id: NodeId) -> u32 {
  if id == NodeId::ROOT {
    ROOT_INO
  } else {
    u32::try_from(id.index())
      .unwrap_or(u32::MAX)
      .saturating_add(FIRST_INO.saturating_sub(1))
  }
}

/// The directory-entry file type of node `id`.
fn file_type(tree: &Tree, id: NodeId) -> u8 {
  match tree.node(id).map(|node| &node.kind) {
    Some(Kind::Directory) => format::FT_DIR,
    Some(Kind::File { .. }) => format::FT_REG_FILE,
    Some(Kind::Symlink { .. }) => format::FT_SYMLINK,
    Some(Kind::CharDevice { .. }) => format::FT_CHRDEV,
    Some(Kind::BlockDevice { .. }) => format::FT_BLKDEV,
    Some(Kind::Fifo) => format::FT_FIFO,
    Some(Kind::Socket) => format::FT_SOCK,
    None => 0,
  }
}

/// Fills `block` with data block `index` of file `id` of `size` bytes from `content`, zero past the file's end.
fn fill_file<E>(
  io: &mut dyn ImageIo<E>,
  id: NodeId,
  index: u64,
  size: u64,
  block: &mut [u8],
) -> Result<(), Failed<E>> {
  let start = index.saturating_mul(BLOCK_BYTES);
  let want = usize::try_from(size.saturating_sub(start).min(BLOCK_BYTES)).unwrap_or(0);
  let mut filled = 0usize;
  while filled < want {
    let into = block
      .get_mut(filled..want)
      .ok_or(ImageRefusal::NodeUnknown { node: id.index() })?;
    let offset = start.saturating_add(u64::try_from(filled).unwrap_or(u64::MAX));
    let read = io.read_content(id, offset, into).map_err(Failed::Source)?;
    if read == 0 {
      return Err(Failed::Image(ImageRefusal::ContentShort {
        node: id.index(),
        at: offset,
      }));
    }
    filled = filled.saturating_add(read);
  }
  Ok(())
}

/// The block slot `slot` of runs `runs` falls on, as a 32-bit pointer.
fn pointer(runs: &Segments<Run>, slot: u64, node: usize) -> Result<u32, ImageRefusal> {
  let block = layout::physical(runs, slot).ok_or(ImageRefusal::NodeUnknown { node })?;
  u32::try_from(block).map_err(|_| ImageRefusal::CapacityTooLarge { blocks: block })
}

/// A bitmap block with its first `used` bits set, and every bit from `extent` to the block's end set as padding. The
/// bits are set a byte at a time: whole bytes filled, the two partial bytes masked. Setting them a bit at a time took
/// 42–60 µs a block (2026-10-24, `image_bench`), the longest unit of a write.
fn bitmap(used: u64, extent: u64) -> Vec<u8> {
  let length = usize::try_from(BLOCK_BYTES).unwrap_or(0);
  let bits = BLOCK_BYTES.saturating_mul(u64::from(u8::BITS));
  let mut bytes = vec![0u8; length];
  set_bits(&mut bytes, 0, used.min(bits));
  set_bits(&mut bytes, extent.min(bits), bits);
  bytes
}

/// Sets bits `from..to` of `bytes` (bit `n` is bit `n % 8` of byte `n / 8`): the whole bytes in one fill, the bits of
/// a partial byte at either end one at a time (seven at most each).
fn set_bits(bytes: &mut [u8], from: u64, to: u64) {
  let per_byte = u64::from(u8::BITS);
  let mut bit = from;
  while bit < to {
    let byte = usize::try_from(bit.checked_div(per_byte).unwrap_or(0)).unwrap_or(usize::MAX);
    let shift = bit.checked_rem(per_byte).unwrap_or(0);
    if shift == 0 && to.saturating_sub(bit) >= per_byte {
      let whole = to.saturating_sub(bit).checked_div(per_byte).unwrap_or(0);
      let end = byte.saturating_add(usize::try_from(whole).unwrap_or(0));
      let Some(run) = bytes.get_mut(byte..end) else {
        return;
      };
      run.fill(u8::MAX);
      bit = bit.saturating_add(whole.saturating_mul(per_byte));
    } else {
      let Some(slot) = bytes.get_mut(byte) else {
        return;
      };
      *slot |= 1u8
        .checked_shl(u32::try_from(shift).unwrap_or(0))
        .unwrap_or(0);
      bit = bit.saturating_add(1);
    }
  }
}

/// The low and high parts of a superblock time: 32 bits, and the high byte of a 40-bit extension; a time before the
/// epoch is written as the epoch, since the superblock's times are unsigned.
fn superblock_time(time: Timestamp) -> (u32, u8) {
  let secs = u64::try_from(time.secs).unwrap_or(0);
  (
    u32::try_from(secs & u64::from(u32::MAX)).unwrap_or(0),
    u8::try_from((secs >> u32::BITS) & u64::from(u8::MAX)).unwrap_or(0),
  )
}

/// A count as a superblock's 32-bit field, refused past it.
fn narrow(value: u64) -> Result<u32, ImageRefusal> {
  u32::try_from(value).map_err(|_| ImageRefusal::CapacityTooLarge { blocks: value })
}

/// A size as a superblock's 16-bit field, refused past it.
fn narrow16(value: u64) -> Result<u16, ImageRefusal> {
  u16::try_from(value).map_err(|_| ImageRefusal::CapacityTooLarge { blocks: value })
}

/// The low 16 bits of an id.
fn low16(value: u32) -> u16 {
  u16::try_from(value & u32::from(u16::MAX)).unwrap_or(0)
}

/// The high 16 bits of an id.
fn high16(value: u32) -> u16 {
  u16::try_from(value >> u16::BITS).unwrap_or(0)
}

/// The low 32 bits of a size.
fn low32(value: u64) -> u32 {
  u32::try_from(value & u64::from(u32::MAX)).unwrap_or(0)
}

/// The high 32 bits of a size.
fn high32(value: u64) -> u32 {
  u32::try_from(value >> u32::BITS).unwrap_or(0)
}
