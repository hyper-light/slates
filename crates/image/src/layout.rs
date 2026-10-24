//! Where everything goes in an image: the group geometry, the allocation of each node's blocks, the block map an
//! inode keeps, and the encodings of a time and a device number. Pure arithmetic, all of it checked.
//!
//! **Geometry.** Blocks are 4 KiB and a group is one bitmap block's worth of them (32,768). Group 0 starts with the
//! boot block (the superblock at its byte 1024), then the group descriptor table; groups 1 and every power of 3, 5
//! and 7 carry copies of both (`sparse_super`); every group then has its block bitmap, its inode bitmap and its inode
//! table, and data after. A last group too small for its own structures is dropped, as `mke2fs` drops it.
//!
//! **Allocation** is one cursor through the groups' data areas, in node order, so the blocks in use are exactly each
//! group's structures plus a prefix of the data area: a group's bitmap is computed from the cursor alone, with no
//! per-block state. A node's blocks are one contiguous run, broken only where a group's structures interrupt it.
//!
//! **The block map** follows ext2's classic order: twelve direct blocks, then the single indirect block followed by
//! the data it maps, then the double indirect block, each of its single blocks followed by its data, then the triple.
//! Every indirect block precedes the blocks it points to, so a file's blocks, indirect ones included, are one
//! sequence of *slots*, and the slot of any data block or indirect block is arithmetic on its index
//! ([`Map::data_slot`]). The reader walks the same structure from the pointers the inode holds.

use crate::error::ImageRefusal;
use crate::format::{DIRECT_BLOCKS, EPOCH_BITS, EPOCH_MASK, GROUP_DESC_BYTES, SECTOR_BYTES};
use crate::segments::Segments;
use crate::tree::Timestamp;

/// Shape: the block size the writer uses: 4096 bytes, `mke2fs.conf`'s default, and the page size of x86_64 and of
/// arm64 kernels built with 4 KiB pages. DAX on a pmem device needs the file system's block to be the page size.
pub const BLOCK_BYTES: u64 = 4096;
/// Format: `s_log_block_size` for [`BLOCK_BYTES`]: the block size is 1024 shifted left by it.
pub const LOG_BLOCK_SIZE: u32 = 2;
/// Shape: the inode size: 256 bytes, `mke2fs.conf`'s default, the large inode that carries nanosecond times.
pub const INODE_BYTES: u64 = 256;
/// Shape: bytes of capacity per inode: 16384, `mke2fs.conf`'s default `inode_ratio`, so a guest has inodes to create
/// files with in proportion to the space it has to fill.
pub const INODE_RATIO: u64 = 16384;
/// Format: the block pointers an indirect block holds, four bytes each.
pub const POINTERS: u64 = BLOCK_BYTES / 4;
/// Format: the blocks of a group: one bitmap block's bits.
pub const BLOCKS_PER_GROUP: u64 = BLOCK_BYTES * 8;
/// Format: the most inodes a group holds: one bitmap block's bits.
pub const MAX_INODES_PER_GROUP: u64 = BLOCK_BYTES * 8;
/// Format: the inodes of one inode-table block.
pub const INODES_PER_BLOCK: u64 = BLOCK_BYTES / INODE_BYTES;
/// Format: the sectors of one block, the unit of an inode's `i_blocks`.
pub const SECTORS_PER_BLOCK: u64 = BLOCK_BYTES / SECTOR_BYTES;
/// Format: the most sectors `i_blocks` counts without the `huge_file` feature.
const MAX_SECTORS: u64 = u32::MAX as u64;
/// Format: the most blocks 32-bit block numbers address.
const MAX_BLOCKS: u64 = u32::MAX as u64;
/// Format: nanoseconds in a second; a time's nanoseconds are below it.
pub const NANOS_PER_SECOND: u32 = 1_000_000_000;
/// Format: the earliest second the format's times hold: the 32-bit low word read as signed, with no epoch bits.
const EARLIEST_SECOND: i64 = i32::MIN as i64;
/// Format: one past the latest second: the low word's full range shifted by the most the two epoch bits add.
const LATEST_SECOND: i64 = EARLIEST_SECOND + (1_i64 << 34);

/// Where a file system's structures and data lie.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
  /// The blocks of the file system (`s_blocks_count`).
  pub blocks: u64,
  /// Its groups.
  pub groups: u64,
  /// The inodes of each group (`s_inodes_per_group`).
  pub inodes_per_group: u64,
  /// The blocks of the group descriptor table.
  pub gdt_blocks: u64,
  /// The blocks of each group's inode table.
  pub inode_table_blocks: u64,
}

impl Geometry {
  /// The geometry of a file system of `capacity` bytes holding at least `inodes_needed` inodes (the reserved ones
  /// included), or why it cannot be.
  pub fn plan(capacity: u64, inodes_needed: u64) -> Result<Geometry, ImageRefusal> {
    let mut blocks = capacity / BLOCK_BYTES;
    if blocks > MAX_BLOCKS {
      return Err(ImageRefusal::CapacityTooLarge { blocks });
    }
    let wanted = inodes_needed.max(capacity / INODE_RATIO);
    loop {
      let groups = blocks.div_ceil(BLOCKS_PER_GROUP);
      if groups == 0 {
        return Err(ImageRefusal::TooSmall {
          need: 1,
          have: blocks,
        });
      }
      let gdt_blocks = groups
        .saturating_mul(u64::try_from(GROUP_DESC_BYTES).unwrap_or(u64::MAX))
        .div_ceil(BLOCK_BYTES);
      let per_group = wanted
        .div_ceil(groups)
        .div_ceil(INODES_PER_BLOCK)
        .saturating_mul(INODES_PER_BLOCK)
        .max(INODES_PER_BLOCK);
      let inodes_per_group = if per_group > MAX_INODES_PER_GROUP {
        let most = groups.saturating_mul(MAX_INODES_PER_GROUP);
        if inodes_needed > most {
          return Err(ImageRefusal::TooManyInodes {
            need: inodes_needed,
            most,
          });
        }
        MAX_INODES_PER_GROUP
      } else {
        per_group
      };
      let geometry = Geometry {
        blocks,
        groups,
        inodes_per_group,
        gdt_blocks,
        inode_table_blocks: inodes_per_group / INODES_PER_BLOCK,
      };
      let last = groups.saturating_sub(1);
      let last_blocks = geometry.group_blocks(last);
      // A last group with no room for a data block past its own structures is dropped, as mke2fs drops it.
      if last_blocks <= geometry.meta_blocks(last) {
        if groups == 1 {
          return Err(ImageRefusal::TooSmall {
            need: geometry.meta_blocks(0).saturating_add(1),
            have: blocks,
          });
        }
        blocks = blocks.saturating_sub(last_blocks);
        continue;
      }
      if geometry.inodes() < inodes_needed {
        return Err(ImageRefusal::TooManyInodes {
          need: inodes_needed,
          most: geometry.inodes(),
        });
      }
      return Ok(geometry);
    }
  }

  /// The inodes of the file system.
  pub fn inodes(&self) -> u64 {
    self.groups.saturating_mul(self.inodes_per_group)
  }

  /// The first block of group `group`.
  pub fn group_start(&self, group: u64) -> u64 {
    group.saturating_mul(BLOCKS_PER_GROUP)
  }

  /// The blocks of group `group`: a whole group's, or the remainder for the last.
  pub fn group_blocks(&self, group: u64) -> u64 {
    self
      .blocks
      .saturating_sub(self.group_start(group))
      .min(BLOCKS_PER_GROUP)
  }

  /// Whether group `group` carries a copy of the superblock and the group descriptors (`sparse_super`: groups 0 and 1
  /// and every power of 3, 5 and 7).
  pub fn has_super(group: u64) -> bool {
    group <= 1
      || crate::format::SPARSE_SUPER_BASES
        .iter()
        .any(|base| is_power_of(group, *base))
  }

  /// Group `group`'s block bitmap.
  pub fn block_bitmap(&self, group: u64) -> u64 {
    let copies = if Geometry::has_super(group) {
      self.gdt_blocks.saturating_add(1)
    } else {
      0
    };
    self.group_start(group).saturating_add(copies)
  }

  /// Group `group`'s inode bitmap, after its block bitmap.
  pub fn inode_bitmap(&self, group: u64) -> u64 {
    self.block_bitmap(group).saturating_add(1)
  }

  /// The first block of group `group`'s inode table, after its inode bitmap.
  pub fn inode_table(&self, group: u64) -> u64 {
    self.inode_bitmap(group).saturating_add(1)
  }

  /// The first data block of group `group`, after its inode table.
  pub fn data_start(&self, group: u64) -> u64 {
    self
      .inode_table(group)
      .saturating_add(self.inode_table_blocks)
  }

  /// The blocks group `group`'s own structures take at its start.
  pub fn meta_blocks(&self, group: u64) -> u64 {
    self
      .data_start(group)
      .saturating_sub(self.group_start(group))
  }

  /// The blocks all groups' structures take.
  pub fn meta_total(&self) -> u64 {
    (0..self.groups)
      .map(|group| self.meta_blocks(group))
      .fold(0, u64::saturating_add)
  }
}

/// Whether `value` is a power of `base` (`base`⁰ = 1 included).
fn is_power_of(value: u64, base: u64) -> bool {
  let mut power = 1u64;
  while power < value {
    match power.checked_mul(base) {
      Some(next) => power = next,
      None => return false,
    }
  }
  power == value
}

/// A run of consecutive blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
  /// The slot of the run's first block in its node's sequence.
  pub slot: u64,
  /// Its first block.
  pub start: u64,
  /// Its blocks.
  pub len: u64,
}

/// The cursor that hands out data blocks in order through the groups' data areas.
#[derive(Debug)]
pub struct Allocator<'g> {
  geometry: &'g Geometry,
  next: u64,
}

impl<'g> Allocator<'g> {
  /// A cursor at the first data block.
  pub fn new(geometry: &'g Geometry) -> Allocator<'g> {
    Allocator {
      geometry,
      next: geometry.data_start(0),
    }
  }

  /// A cursor resumed at block `next`, where an earlier cursor over `geometry` stopped: an allocation in steps keeps
  /// only the block number between them.
  pub fn at(geometry: &'g Geometry, next: u64) -> Allocator<'g> {
    Allocator { geometry, next }
  }

  /// The next block the cursor would hand out: every data block before it is in use.
  pub fn next(&self) -> u64 {
    self.next
  }

  /// The next run of at most `left` blocks for the slots from `slot` on: the rest of the cursor's group's data area,
  /// or the next group's when this one's is used up. One run a call, so a node spanning many groups is placed a group a
  /// step (A-160; §4.3). Every group has room past its structures (the geometry drops a last group that has none), so a
  /// call passes one used-up group at most.
  pub fn take_run(&mut self, slot: u64, left: u64) -> Result<Run, ImageRefusal> {
    let mut group = self.next.checked_div(BLOCKS_PER_GROUP).unwrap_or(u64::MAX);
    let mut start = self.next.max(self.geometry.data_start(group));
    if start >= self.group_end(group) {
      group = group.saturating_add(1);
      start = self.geometry.data_start(group);
    }
    let end = self.group_end(group);
    if group >= self.geometry.groups || start >= end {
      return Err(ImageRefusal::TooSmall {
        need: self.geometry.blocks.saturating_add(left),
        have: self.geometry.blocks,
      });
    }
    let len = left.min(end.saturating_sub(start));
    self.next = start.saturating_add(len);
    Ok(Run { slot, start, len })
  }

  /// One past group `group`'s last block.
  fn group_end(&self, group: u64) -> u64 {
    self
      .geometry
      .group_start(group)
      .saturating_add(self.geometry.group_blocks(group))
  }
}

/// The block `slot` of a node whose blocks are `runs`, if it has one.
pub fn physical(runs: &Segments<Run>, slot: u64) -> Option<u64> {
  // The first run starting past `slot`, by binary search over the runs in slot order; the run before it holds `slot`.
  let (mut low, mut high) = (0usize, runs.len());
  while low < high {
    let middle = low.saturating_add(high.saturating_sub(low) / 2);
    if runs.get(middle)?.slot <= slot {
      low = middle.saturating_add(1);
    } else {
      high = middle;
    }
  }
  let run = runs.get(low.checked_sub(1)?)?;
  let offset = slot.checked_sub(run.slot)?;
  (offset < run.len).then(|| run.start.saturating_add(offset))
}

/// A node's block map: its data blocks, and where each data and indirect block falls in its sequence of slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Map {
  /// The data blocks.
  pub data: u64,
}

/// The first data index each indirect level maps, and the slot of the level's top block.
const SINGLE_FIRST: u64 = DIRECT_BLOCKS;
/// See [`SINGLE_FIRST`]: the double level's first data index.
const DOUBLE_FIRST: u64 = SINGLE_FIRST + POINTERS;
/// See [`SINGLE_FIRST`]: the triple level's first data index.
const TRIPLE_FIRST: u64 = DOUBLE_FIRST + POINTERS * POINTERS;
/// See [`SINGLE_FIRST`]: one past the last data index an inode maps.
const MAPPED_END: u64 = TRIPLE_FIRST + POINTERS * POINTERS * POINTERS;
/// The slot of the double indirect block: after the direct blocks, the single block and the data it maps.
const DOUBLE_SLOT: u64 = DIRECT_BLOCKS + 1 + POINTERS;
/// The slots a double indirect block's subtree takes after it: each single block and the data it maps.
const DOUBLE_SPAN: u64 = POINTERS * (1 + POINTERS);
/// The slot of the triple indirect block.
const TRIPLE_SLOT: u64 = DOUBLE_SLOT + 1 + DOUBLE_SPAN;

/// An indirect block of a map: its slot and what its pointers name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Indirect {
  /// A block of pointers to data blocks `first..first + count`.
  Data {
    /// Its slot.
    slot: u64,
    /// The first data index it maps.
    first: u64,
    /// The data blocks it maps.
    count: u64,
  },
  /// A block of pointers to the indirect blocks at `children`.
  Blocks {
    /// Its slot.
    slot: u64,
    /// The slots of the blocks it points to, in order.
    children: Vec<u64>,
  },
}

impl Map {
  /// The map of `data` data blocks, if an inode can map and count that many.
  pub fn new(data: u64) -> Result<Map, ImageRefusal> {
    let map = Map { data };
    if data > MAPPED_END || map.slots().saturating_mul(SECTORS_PER_BLOCK) > MAX_SECTORS {
      return Err(ImageRefusal::FileTooLarge {
        size: data.saturating_mul(BLOCK_BYTES),
      });
    }
    Ok(map)
  }

  /// The slot of data block `index`.
  pub fn data_slot(index: u64) -> u64 {
    if index < SINGLE_FIRST {
      index
    } else if index < DOUBLE_FIRST {
      // After the direct blocks and the single indirect block.
      index
        .saturating_sub(SINGLE_FIRST)
        .saturating_add(DIRECT_BLOCKS + 1)
    } else if index < TRIPLE_FIRST {
      let within = index.saturating_sub(DOUBLE_FIRST);
      Map::double_single_slot(within / POINTERS)
        .saturating_add((within % POINTERS).saturating_add(1))
    } else {
      let within = index.saturating_sub(TRIPLE_FIRST);
      let double = within / (POINTERS * POINTERS);
      let single = (within % (POINTERS * POINTERS)) / POINTERS;
      Map::triple_single_slot(double, single).saturating_add((within % POINTERS).saturating_add(1))
    }
  }

  /// The slot of the double indirect block's single block `single`.
  fn double_single_slot(single: u64) -> u64 {
    DOUBLE_SLOT
      .saturating_add(1)
      .saturating_add(single.saturating_mul(1 + POINTERS))
  }

  /// The slot of the triple indirect block's double block `double`.
  fn triple_double_slot(double: u64) -> u64 {
    TRIPLE_SLOT
      .saturating_add(1)
      .saturating_add(double.saturating_mul(1 + DOUBLE_SPAN))
  }

  /// The slot of single block `single` under the triple indirect block's double block `double`.
  fn triple_single_slot(double: u64, single: u64) -> u64 {
    Map::triple_double_slot(double)
      .saturating_add(1)
      .saturating_add(single.saturating_mul(1 + POINTERS))
  }

  /// The slots of the map: its data blocks and its indirect blocks.
  pub fn slots(&self) -> u64 {
    match self.data.checked_sub(1) {
      Some(last) => Map::data_slot(last).saturating_add(1),
      None => 0,
    }
  }

  /// The slots of the top-level pointers an inode keeps: the twelve direct blocks' (those in use), then the single,
  /// double and triple indirect blocks' (those in use), in `i_block` order.
  pub fn pointer_slots(&self) -> Vec<u64> {
    let mut slots: Vec<u64> = (0..self.data.min(DIRECT_BLOCKS)).collect();
    if self.data > SINGLE_FIRST {
      slots.push(DIRECT_BLOCKS);
    }
    if self.data > DOUBLE_FIRST {
      slots.push(DOUBLE_SLOT);
    }
    if self.data > TRIPLE_FIRST {
      slots.push(TRIPLE_SLOT);
    }
    slots
  }

  /// Whether the map has an indirect block: past the twelve direct blocks.
  pub fn has_indirect(&self) -> bool {
    self.data > SINGLE_FIRST
  }

  /// Indirect block `index` of the map in slot order — the single block; the double block and each single block under
  /// it; the triple block and, for each double block under it, that block and its single blocks — or `None` past the
  /// last. Computed from the index alone, so a writer asks for one indirect block a step whatever the file's size, never
  /// for the whole list (A-160; §4.3): a block of pointers names at most [`POINTERS`] children.
  pub fn indirect_at(&self, index: u64) -> Option<Indirect> {
    if !self.has_indirect() {
      return None;
    }
    if index == 0 {
      return Some(Indirect::Data {
        slot: DIRECT_BLOCKS,
        first: SINGLE_FIRST,
        count: self.data.min(DOUBLE_FIRST).saturating_sub(SINGLE_FIRST),
      });
    }
    if self.data <= DOUBLE_FIRST {
      return None;
    }
    let mut at = index.saturating_sub(1);
    let singles = self
      .data
      .min(TRIPLE_FIRST)
      .saturating_sub(DOUBLE_FIRST)
      .div_ceil(POINTERS);
    if at == 0 {
      return Some(Indirect::Blocks {
        slot: DOUBLE_SLOT,
        children: (0..singles).map(Map::double_single_slot).collect(),
      });
    }
    at = at.saturating_sub(1);
    if at < singles {
      let first = DOUBLE_FIRST.saturating_add(at.saturating_mul(POINTERS));
      return Some(self.data_block(Map::double_single_slot(at), first));
    }
    at = at.saturating_sub(singles);
    if self.data <= TRIPLE_FIRST {
      return None;
    }
    self.triple_at(at)
  }

  /// Indirect block `at` of the triple level: the triple block, then for each double block under it that block and its
  /// single blocks. Every double block but the last maps all [`POINTERS`] squared of its data blocks, so it and its
  /// single blocks are `1 + POINTERS` entries, and the double block `at` falls under is found by division.
  fn triple_at(&self, at: u64) -> Option<Indirect> {
    let span = POINTERS.saturating_mul(POINTERS);
    let doubles = self.data.saturating_sub(TRIPLE_FIRST).div_ceil(span);
    if at == 0 {
      return Some(Indirect::Blocks {
        slot: TRIPLE_SLOT,
        children: (0..doubles).map(Map::triple_double_slot).collect(),
      });
    }
    let within_triple = at.saturating_sub(1);
    let entries = POINTERS.saturating_add(1);
    let double = within_triple.checked_div(entries)?;
    if double >= doubles {
      return None;
    }
    let double_first = TRIPLE_FIRST.saturating_add(double.saturating_mul(span));
    let singles = self
      .data
      .min(double_first.saturating_add(span))
      .saturating_sub(double_first)
      .div_ceil(POINTERS);
    match within_triple.checked_rem(entries)?.checked_sub(1) {
      None => Some(Indirect::Blocks {
        slot: Map::triple_double_slot(double),
        children: (0..singles)
          .map(|single| Map::triple_single_slot(double, single))
          .collect(),
      }),
      Some(single) if single < singles => {
        let first = double_first.saturating_add(single.saturating_mul(POINTERS));
        Some(self.data_block(Map::triple_single_slot(double, single), first))
      }
      Some(_) => None,
    }
  }

  /// The indirect block at `slot` holding pointers to data blocks from `first`: up to [`POINTERS`] of them, as many as
  /// the map has.
  fn data_block(&self, slot: u64, first: u64) -> Indirect {
    Indirect::Data {
      slot,
      first,
      count: self
        .data
        .min(first.saturating_add(POINTERS))
        .saturating_sub(first),
    }
  }
}

/// Which pointers a logical data block is reached through: the direct pointer, or the chain of indexes through the
/// single, double or triple indirect block. The reader follows it from the inode's `i_block`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Path {
  /// `i_block[index]`.
  Direct(usize),
  /// `i_block[12]`, then pointer `index` of that block.
  Single(u64),
  /// `i_block[13]`, then pointers `first` and `second`.
  Double(u64, u64),
  /// `i_block[14]`, then pointers `first`, `second` and `third`.
  Triple(u64, u64, u64),
}

/// The path to logical data block `index`, if an inode maps that far.
pub fn path(index: u64) -> Option<Path> {
  if index < SINGLE_FIRST {
    Some(Path::Direct(usize::try_from(index).ok()?))
  } else if index < DOUBLE_FIRST {
    Some(Path::Single(index.saturating_sub(SINGLE_FIRST)))
  } else if index < TRIPLE_FIRST {
    let within = index.saturating_sub(DOUBLE_FIRST);
    Some(Path::Double(within / POINTERS, within % POINTERS))
  } else if index < MAPPED_END {
    let within = index.saturating_sub(TRIPLE_FIRST);
    Some(Path::Triple(
      within / (POINTERS * POINTERS),
      (within % (POINTERS * POINTERS)) / POINTERS,
      within % POINTERS,
    ))
  } else {
    None
  }
}

/// The largest regular file an inode of this format maps and counts, in bytes.
pub fn max_file_bytes() -> u64 {
  // The largest data block count whose map passes `Map::new`, found by bisection over the mapped range.
  let (mut low, mut high) = (0u64, MAPPED_END);
  while low < high {
    let middle = low.saturating_add(high.saturating_sub(low).div_ceil(2));
    if Map::new(middle).is_ok() {
      low = middle;
    } else {
      high = middle.saturating_sub(1);
    }
  }
  low.saturating_mul(BLOCK_BYTES)
}

/// A time as an inode keeps it: the low 32 bits of its seconds, and the extra word (two epoch bits that extend the
/// seconds, then the nanoseconds; `ext4_encode_extra_time`). Refused outside the 34-bit range.
pub fn encode_time(time: Timestamp) -> Result<(u32, u32), ImageRefusal> {
  if time.secs < EARLIEST_SECOND || time.secs >= LATEST_SECOND || time.nanos >= NANOS_PER_SECOND {
    return Err(ImageRefusal::TimeOutOfRange { secs: time.secs });
  }
  // Format: the low word is the seconds' low 32 bits; the epoch bits count how far past it the seconds lie.
  let low = u32::try_from(time.secs & i64::from(u32::MAX))
    .map_err(|_| ImageRefusal::TimeOutOfRange { secs: time.secs })?;
  let signed_low = i64::from(i32::from_le_bytes(low.to_le_bytes()));
  let above = time
    .secs
    .checked_sub(signed_low)
    .ok_or(ImageRefusal::TimeOutOfRange { secs: time.secs })?;
  let epoch = u32::try_from((above >> u32::BITS) & i64::from(EPOCH_MASK))
    .map_err(|_| ImageRefusal::TimeOutOfRange { secs: time.secs })?;
  Ok((low, epoch | (time.nanos << EPOCH_BITS)))
}

/// The time an inode keeps as `low` and `extra` (`ext4_decode_extra_time`), or `None` for nanoseconds out of range.
pub fn decode_time(low: u32, extra: u32) -> Option<Timestamp> {
  let nanos = extra >> EPOCH_BITS;
  if nanos >= NANOS_PER_SECOND {
    return None;
  }
  let signed_low = i64::from(i32::from_le_bytes(low.to_le_bytes()));
  let secs = signed_low.checked_add(i64::from(extra & EPOCH_MASK) << u32::BITS)?;
  Some(Timestamp { secs, nanos })
}

/// Format: the kernel's old device encoding holds a major and a minor below this (`old_valid_dev`).
const OLD_DEV_LIMIT: u32 = 256;

/// Format: the kernel's new device encoding (`new_encode_dev`): the minor's low 8 bits, then 12 bits of major, then
/// the minor's high 12 bits.
const DEV_LOW_MINOR: u32 = 0xff;
/// Format: see [`DEV_LOW_MINOR`]: the major's 12 bits, and the minor's high 12 bits once shifted down onto them.
const DEV_MIDDLE: u32 = 0x000f_ff00;
/// Format: see [`DEV_LOW_MINOR`]: where the major starts.
const DEV_MAJOR_SHIFT: u32 = 8;
/// Format: see [`DEV_LOW_MINOR`]: how far the minor's high bits move up past the major.
const DEV_MINOR_HIGH_SHIFT: u32 = 12;
/// Format: the major's width in the new encoding.
const DEV_MAJOR_BITS: u32 = 12;
/// Format: the minor's width in the new encoding.
const DEV_MINOR_BITS: u32 = 20;

/// A device number in the kernel's new encoding, split into its major and minor (`new_decode_dev`).
pub fn major_minor(rdev: u32) -> (u32, u32) {
  (
    (rdev & DEV_MIDDLE) >> DEV_MAJOR_SHIFT,
    (rdev & DEV_LOW_MINOR) | ((rdev >> DEV_MINOR_HIGH_SHIFT) & DEV_MIDDLE),
  )
}

/// A major and a minor in the kernel's new encoding (`new_encode_dev`), if they fit its 12 and 20 bits.
pub fn new_encode(major: u32, minor: u32) -> Option<u32> {
  if major >= 1 << DEV_MAJOR_BITS || minor >= 1 << DEV_MINOR_BITS {
    return None;
  }
  Some(
    (minor & DEV_LOW_MINOR)
      | (major << DEV_MAJOR_SHIFT)
      | ((minor & !DEV_LOW_MINOR) << DEV_MINOR_HIGH_SHIFT),
  )
}

/// The two words an inode keeps a device number in (`i_block[0]` and `i_block[1]`): the old encoding in the first when
/// the major and minor fit it, else zero there and the new encoding in the second, as the kernel writes them.
pub fn encode_rdev(rdev: u32) -> (u32, u32) {
  let (major, minor) = major_minor(rdev);
  if major < OLD_DEV_LIMIT && minor < OLD_DEV_LIMIT {
    ((major << DEV_MAJOR_SHIFT) | minor, 0)
  } else {
    (0, rdev)
  }
}

/// The device number an inode keeps in `first` and `second` (`i_block[0]` and `i_block[1]`), in the new encoding.
pub fn decode_rdev(first: u32, second: u32) -> Option<u32> {
  if first != 0 {
    // The old encoding: the major in the second byte, the minor in the first.
    new_encode(
      (first >> DEV_MAJOR_SHIFT) & DEV_LOW_MINOR,
      first & DEV_LOW_MINOR,
    )
  } else {
    let (major, minor) = major_minor(second);
    new_encode(major, minor)
  }
}
