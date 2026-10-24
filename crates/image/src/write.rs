//! Writing a tree as an ext2 image, in steps a caller bounds (§4.3: bounded work everywhere). A [`Builder`] lays the
//! whole image out when it is made: the geometry, every node's blocks and inode number. Each [`Builder::step`] then
//! writes until a byte budget is spent: the superblock and the group descriptors (with their `sparse_super` copies),
//! each group's bitmaps, the inode-table blocks that hold an inode in use, then each node's blocks in order, its data
//! first and its indirect blocks after. The caller sizes the image before the first step ([`Builder::image_bytes`]);
//! every byte not written is a zero, so free space, unused inodes and a file's zero blocks cost the sink nothing.
//!
//! Inode numbers follow node order: the root is inode 2, and node `n` is inode `10 + n`, so the inodes in use are
//! exactly the reserved ones and a prefix after them. A file system's identity (its UUID and creation time) is the
//! caller's, so one tree and one identity always make the same bytes (the determinism gate).

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
use crate::tree::{Kind, NodeId, Timestamp, Tree};

/// Where a file's bytes come from while its image is written.
pub trait ContentSource<E> {
  /// Fills `buf` with node `node`'s bytes from `offset`, returning how many it filled; fewer than asked only at the
  /// file's end.
  fn read(&mut self, node: NodeId, offset: u64, buf: &mut [u8]) -> Result<usize, E>;
}

/// Where an image's bytes go.
pub trait ImageSink<E> {
  /// Writes `bytes` at `offset` of the image. The image is [`Builder::image_bytes`] long before the first write, and
  /// every byte never written reads as zero.
  fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<(), E>;
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

/// One node's blocks: its map and the runs its slots fill.
#[derive(Clone, Debug)]
struct Placement {
  map: Map,
  runs: Vec<Run>,
}

/// Where a builder is in its writing.
#[derive(Clone, Debug)]
enum Cursor {
  /// The superblock copy of group `group`, the first group with a copy at or after it.
  Superblock { group: u64 },
  /// Block `block` of group `group`'s copy of the group descriptor table.
  Descriptors { group: u64, block: u64 },
  /// Group `group`'s block bitmap, or its inode bitmap.
  Bitmap { group: u64, inodes: bool },
  /// Block `block` of group `group`'s inode table.
  InodeTable { group: u64, block: u64 },
  /// Data block `index` of node `node`; for a directory, `entry` is the next of its entries to pack.
  Data {
    node: usize,
    index: u64,
    entry: usize,
  },
  /// Indirect block `index` of node `node`.
  Indirect {
    node: usize,
    index: usize,
    blocks: Vec<Indirect>,
  },
  /// Written whole.
  Done,
}

/// An image being written: laid out at construction, written in [`Builder::step`]s.
#[derive(Debug)]
pub struct Builder<'t> {
  tree: &'t Tree,
  geometry: Geometry,
  identity: Identity,
  placements: Vec<Placement>,
  /// The allocator's cursor after every node: each group's data blocks before it are in use, none after.
  allocated: u64,
  /// The directories whose inodes are in each group (`bg_used_dirs_count`).
  directories: Vec<u64>,
  cursor: Cursor,
}

impl<'t> Builder<'t> {
  /// Lays `tree` out in an image of `capacity` bytes with `identity`, or says why it cannot.
  pub fn new(
    tree: &'t Tree,
    capacity: u64,
    identity: Identity,
  ) -> Result<Builder<'t>, ImageRefusal> {
    let nodes = u64::try_from(tree.nodes().len()).unwrap_or(u64::MAX);
    // The reserved inodes (the root among them), then one per node after the root.
    let inodes_needed = u64::from(FIRST_INO)
      .saturating_sub(1)
      .saturating_add(nodes.saturating_sub(1));
    let geometry = Geometry::plan(capacity, inodes_needed)?;
    let mut maps = Vec::with_capacity(tree.nodes().len());
    let mut slots = 0u64;
    for id in tree.ids() {
      let map = Map::new(data_blocks(tree, id))?;
      slots = slots.saturating_add(map.slots());
      maps.push(map);
    }
    let need = geometry.meta_total().saturating_add(slots);
    if need > geometry.blocks {
      return Err(ImageRefusal::TooSmall {
        need,
        have: geometry.blocks,
      });
    }
    let mut allocator = Allocator::new(&geometry);
    let mut placements = Vec::with_capacity(maps.len());
    for map in maps {
      let runs = allocator.take(map.slots())?;
      placements.push(Placement { map, runs });
    }
    let allocated = allocator.next();
    let mut directories = vec![0u64; usize::try_from(geometry.groups).unwrap_or(0)];
    let per_group = geometry.inodes_per_group.max(1);
    for id in tree.ids() {
      if tree
        .node(id)
        .is_some_and(|node| node.kind == Kind::Directory)
      {
        let group = u64::from(ino_of(id))
          .saturating_sub(1)
          .checked_div(per_group)
          .unwrap_or(u64::MAX);
        if let Some(count) = usize::try_from(group)
          .ok()
          .and_then(|group| directories.get_mut(group))
        {
          *count = count.saturating_add(1);
        }
      }
    }
    Ok(Builder {
      tree,
      geometry,
      identity,
      placements,
      allocated,
      directories,
      cursor: Cursor::Superblock { group: 0 },
    })
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
  pub fn step<E>(
    &mut self,
    content: &mut dyn ContentSource<E>,
    sink: &mut dyn ImageSink<E>,
    budget: u64,
  ) -> Result<Progress, Failed<E>> {
    let mut spent = 0u64;
    loop {
      if matches!(self.cursor, Cursor::Done) {
        return Ok(Progress::Done(self.written()));
      }
      spent = spent.saturating_add(self.advance(content, sink)?);
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
      blocks_used: (0..self.geometry.groups)
        .map(|group| self.blocks_used_in(group))
        .fold(0, u64::saturating_add),
      inodes_used: self.inodes_used(),
    }
  }

  /// Writes the structure or block the cursor names and moves it on; returns the bytes written.
  fn advance<E>(
    &mut self,
    content: &mut dyn ContentSource<E>,
    sink: &mut dyn ImageSink<E>,
  ) -> Result<u64, Failed<E>> {
    match std::mem::replace(&mut self.cursor, Cursor::Done) {
      Cursor::Superblock { group } => self.write_superblock(group, sink),
      Cursor::Descriptors { group, block } => self.write_descriptors(group, block, sink),
      Cursor::Bitmap { group, inodes } => self.write_bitmap(group, inodes, sink),
      Cursor::InodeTable { group, block } => self.write_inode_block(group, block, sink),
      Cursor::Data { node, index, entry } => self.write_data(node, index, entry, content, sink),
      Cursor::Indirect {
        node,
        index,
        blocks,
      } => self.write_indirect(node, index, blocks, sink),
      Cursor::Done => Ok(0),
    }
  }

  /// The superblock copy of the first group at or after `group` that carries one; then its descriptor table.
  fn write_superblock<E>(
    &mut self,
    group: u64,
    sink: &mut dyn ImageSink<E>,
  ) -> Result<u64, Failed<E>> {
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
    sink.write_at(at, &bytes).map_err(Failed::Source)?;
    self.cursor = Cursor::Descriptors { group, block: 0 };
    Ok(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
  }

  /// Block `block` of group `group`'s descriptor table copy; then the next, or the next group's superblock copy.
  fn write_descriptors<E>(
    &mut self,
    group: u64,
    block: u64,
    sink: &mut dyn ImageSink<E>,
  ) -> Result<u64, Failed<E>> {
    let first = self
      .geometry
      .group_start(group)
      .saturating_add(1)
      .saturating_add(block);
    let bytes = self.descriptor_block(block);
    sink
      .write_at(self.block_offset(first)?, &bytes)
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
    sink: &mut dyn ImageSink<E>,
  ) -> Result<u64, Failed<E>> {
    let (block, bits) = if inodes {
      (self.geometry.inode_bitmap(group), self.inode_bitmap(group))
    } else {
      (self.geometry.block_bitmap(group), self.block_bitmap(group))
    };
    sink
      .write_at(self.block_offset(block)?, &bits)
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
    sink: &mut dyn ImageSink<E>,
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
    sink
      .write_at(self.block_offset(at)?, &bytes)
      .map_err(Failed::Source)?;
    self.cursor = Cursor::InodeTable {
      group,
      block: block.saturating_add(1),
    };
    Ok(BLOCK_BYTES)
  }

  /// The cursor at the data of the first node at or after `node` that has a block, or done.
  fn first_data(&self, node: usize) -> Cursor {
    match (node..self.placements.len()).find(|node| {
      self
        .placements
        .get(*node)
        .is_some_and(|placement| placement.map.data > 0)
    }) {
      Some(node) => Cursor::Data {
        node,
        index: 0,
        entry: 0,
      },
      None => Cursor::Done,
    }
  }

  /// Data block `index` of node `node`; then its next, or its indirect blocks, or the next node.
  fn write_data<E>(
    &mut self,
    node: usize,
    index: u64,
    entry: usize,
    content: &mut dyn ContentSource<E>,
    sink: &mut dyn ImageSink<E>,
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
      Kind::File { size } => fill_file(content, id, index, size, &mut bytes)?,
      Kind::CharDevice { .. } | Kind::BlockDevice { .. } | Kind::Fifo | Kind::Socket => {}
    }
    // A zero block of a file is left unwritten: the image reads it as zero, and a sparse source stays cheap.
    let skip = matches!(
      self.tree.node(id).map(|node| &node.kind),
      Some(Kind::File { .. })
    ) && bytes.iter().all(|byte| *byte == 0);
    if !skip {
      sink
        .write_at(self.block_offset(block)?, &bytes)
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
    let blocks = self
      .placements
      .get(node)
      .map(|placement| placement.map.indirect())
      .unwrap_or_default();
    if blocks.is_empty() {
      self.first_data(node.saturating_add(1))
    } else {
      Cursor::Indirect {
        node,
        index: 0,
        blocks,
      }
    }
  }

  /// Indirect block `index` of node `node`; then its next, or the next node.
  fn write_indirect<E>(
    &mut self,
    node: usize,
    index: usize,
    blocks: Vec<Indirect>,
    sink: &mut dyn ImageSink<E>,
  ) -> Result<u64, Failed<E>> {
    let placement = self
      .placements
      .get(node)
      .ok_or(ImageRefusal::NodeUnknown { node })?;
    let Some(block) = blocks.get(index) else {
      self.cursor = self.first_data(node.saturating_add(1));
      return Ok(0);
    };
    let mut fields = Fields::with_capacity(usize::try_from(BLOCK_BYTES).unwrap_or(0));
    let slot = match block {
      Indirect::Data { slot, first, count } => {
        for data in *first..first.saturating_add(*count) {
          fields.u32(pointer(&placement.runs, Map::data_slot(data), node)?);
        }
        *slot
      }
      Indirect::Blocks { slot, children } => {
        for child in children {
          fields.u32(pointer(&placement.runs, *child, node)?);
        }
        *slot
      }
    };
    let at = layout::physical(&placement.runs, slot).ok_or(ImageRefusal::NodeUnknown { node })?;
    let bytes = fields.finish(usize::try_from(BLOCK_BYTES).unwrap_or(0));
    sink
      .write_at(self.block_offset(at)?, &bytes)
      .map_err(Failed::Source)?;
    let next = index.saturating_add(1);
    self.cursor = if next < blocks.len() {
      Cursor::Indirect {
        node,
        index: next,
        blocks,
      }
    } else {
      self.first_data(node.saturating_add(1))
    };
    Ok(BLOCK_BYTES)
  }

  /// Packs directory `id`'s entries from `entry` into one block (`.` and `..` are its first two entries); returns the
  /// next entry to pack. The block's last entry reaches to the block's end, as the format requires.
  fn pack_directory(
    &self,
    id: NodeId,
    entry: usize,
    block: &mut [u8],
  ) -> Result<usize, ImageRefusal> {
    let mut offset = 0usize;
    let mut last: Option<usize> = None;
    let mut next = entry;
    let block_bytes = usize::try_from(BLOCK_BYTES).unwrap_or(0);
    while let Some((name, child)) = directory_entry(self.tree, id, next) {
      let length = record_length(name.len());
      if offset.saturating_add(length) > block_bytes {
        break;
      }
      let ino = ino_of(child);
      let file_type = file_type(self.tree, child);
      let mut fields = Fields::with_capacity(length);
      fields
        .u32(ino)
        .u16(u16::try_from(length).unwrap_or(u16::MAX))
        .u8(u8::try_from(name.len()).unwrap_or(u8::MAX))
        .u8(file_type)
        .bytes(name);
      let record = fields.finish(length);
      block
        .get_mut(offset..offset.saturating_add(length))
        .ok_or(ImageRefusal::NodeUnknown { node: id.index() })?
        .copy_from_slice(&record);
      last = Some(offset);
      offset = offset.saturating_add(length);
      next = next.saturating_add(1);
    }
    // The last record's length reaches the end of the block.
    if let Some(at) = last {
      let reach = u16::try_from(block_bytes.saturating_sub(at)).unwrap_or(u16::MAX);
      block
        .get_mut(
          at.saturating_add(format::DIRENT_RECORD_LENGTH_AT)
            ..at.saturating_add(format::DIRENT_NAME_LENGTH_AT),
        )
        .ok_or(ImageRefusal::NodeUnknown { node: id.index() })?
        .copy_from_slice(&reach.to_le_bytes());
    }
    Ok(next)
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
    let nodes = u64::try_from(self.tree.nodes().len()).unwrap_or(u64::MAX);
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
    usize::try_from(group)
      .ok()
      .and_then(|group| self.directories.get(group))
      .copied()
      .unwrap_or(0)
  }

  /// The free blocks of the file system.
  fn free_blocks(&self) -> u64 {
    (0..self.geometry.groups)
      .map(|group| {
        self
          .geometry
          .group_blocks(group)
          .saturating_sub(self.blocks_used_in(group))
      })
      .fold(0, u64::saturating_add)
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
      .u32(0) // i_generation
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

/// The data blocks node `id` takes: a directory's packed entries, a file's bytes, a slow symlink's one block.
fn data_blocks(tree: &Tree, id: NodeId) -> u64 {
  match tree.node(id).map(|node| &node.kind) {
    Some(Kind::Directory) => directory_blocks(tree, id),
    Some(Kind::File { size }) => size.div_ceil(BLOCK_BYTES),
    Some(Kind::Symlink { target }) if target.len() >= BLOCK_POINTER_BYTES => 1,
    _ => 0,
  }
}

/// The blocks directory `id`'s entries pack into, `.` and `..` first: an entry never spans two blocks.
fn directory_blocks(tree: &Tree, id: NodeId) -> u64 {
  let block_bytes = usize::try_from(BLOCK_BYTES).unwrap_or(0);
  let mut blocks = 1u64;
  let mut offset = 0usize;
  let mut index = 0usize;
  while let Some((name, _)) = directory_entry(tree, id, index) {
    index = index.saturating_add(1);
    let length = record_length(name.len());
    if offset.saturating_add(length) > block_bytes {
      blocks = blocks.saturating_add(1);
      offset = 0;
    }
    offset = offset.saturating_add(length);
  }
  blocks
}

/// Entry `index` of directory `id` as the image keeps its entries: `.`, `..`, then its children in name order.
fn directory_entry(tree: &Tree, id: NodeId, index: usize) -> Option<(&[u8], NodeId)> {
  match index {
    0 => Some((b".", id)),
    1 => Some((b"..", tree.parent(id))),
    _ => tree
      .children(id)
      .get(index.saturating_sub(2))
      .map(|(name, child)| (name.as_slice(), *child)),
  }
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
  content: &mut dyn ContentSource<E>,
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
    let read = content.read(id, offset, into).map_err(Failed::Source)?;
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
fn pointer(runs: &[Run], slot: u64, node: usize) -> Result<u32, ImageRefusal> {
  let block = layout::physical(runs, slot).ok_or(ImageRefusal::NodeUnknown { node })?;
  u32::try_from(block).map_err(|_| ImageRefusal::CapacityTooLarge { blocks: block })
}

/// A bitmap block with its first `used` bits set, and every bit from `extent` to the block's end set as padding.
fn bitmap(used: u64, extent: u64) -> Vec<u8> {
  let length = usize::try_from(BLOCK_BYTES).unwrap_or(0);
  let bits = BLOCK_BYTES.saturating_mul(u64::from(u8::BITS));
  let mut bytes = vec![0u8; length];
  for bit in (0..used.min(bits)).chain(extent.min(bits)..bits) {
    let per_byte = u64::from(u8::BITS);
    let (byte, shift) = (
      bit.checked_div(per_byte).unwrap_or(0),
      bit.checked_rem(per_byte).unwrap_or(0),
    );
    if let Some(slot) = usize::try_from(byte)
      .ok()
      .and_then(|byte| bytes.get_mut(byte))
    {
      *slot |= 1 << shift;
    }
  }
  bytes
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
