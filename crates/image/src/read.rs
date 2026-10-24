//! Reading an image back: what a merge reads after a guest has written to it (§4.6, A-160). The image is external
//! input, written by a guest kernel the daemon does not trust, so every structure is checked before it is used and
//! every count is held to the image's length (the hostile-input rule, §4.9):
//! - the superblock must be what [`crate::write`] writes — revision 1, 4 KiB blocks, 256-byte inodes, the group
//!   geometry it lays out, no feature beyond the known ones — and cleanly unmounted;
//! - each group's inode table must lie inside its group, and every block pointer inside the file system;
//! - a directory's records must each fit their block and hold their name, and every name must be one a directory
//!   entry may hold;
//! - the walk of the tree refuses a directory named twice (which is what a cycle comes to) and holds the entries it
//!   reads to the caller's bound, so a hostile image cannot make it loop or grow without end.
//!
//! The walk runs in steps a caller bounds ([`Walk::step`]; §4.3): it reads a directory a block at a time, checks each
//! entry as it comes ([`TreeGrower`]) and keeps its place between steps, so the daemon reads a guest's image in
//! slices. An [`Image`] keeps no hold on its source: each read is handed it, so a caller whose source is borrowed a
//! slice at a time keeps the image between slices.
//!
//! A hole (a zero block pointer) reads as zeros, as the kernel reads it. Hard links come back as one node named by
//! several entries.

use std::collections::{BTreeMap, VecDeque};

use crate::error::{Failed, ImageRefusal};
use crate::format::{
  self, BLOCK_POINTER_BYTES, BLOCK_POINTERS, DIRENT_HEADER_BYTES, DIRENT_PAD, FIRST_INO,
  GOOD_OLD_INODE_BYTES, GROUP_DESC_BYTES, INCOMPAT_FILETYPE, INODE_FLAG_EXTENTS,
  INODE_FLAG_INLINE_DATA, MAGIC, RO_COMPAT_EXTRA_ISIZE, RO_COMPAT_LARGE_FILE,
  RO_COMPAT_SPARSE_SUPER, ROOT_INO, STATE_VALID, SUPERBLOCK_BYTES, SUPERBLOCK_OFFSET,
  superblock as sb, u8_at, u16_at, u32_at,
};
use crate::layout::{
  self, BLOCK_BYTES, BLOCKS_PER_GROUP, INODE_BYTES, INODES_PER_BLOCK, LOG_BLOCK_SIZE, Path,
};
use crate::tree::{Kind, Node, NodeId, Timestamp, Tree, TreeGrower};

/// Where an image's bytes are read from.
pub trait ImageSource<E> {
  /// The image's length in bytes.
  fn image_len(&self) -> u64;
  /// Fills `buf` with the image's bytes from `offset`; the range lies inside the image.
  fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), E>;
}

/// An inode as read from an image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inode {
  /// Its number.
  pub ino: u32,
  /// Its mode: type and permission bits.
  pub mode: u16,
  /// Its owner.
  pub uid: u32,
  /// Its group.
  pub gid: u32,
  /// Its size in bytes.
  pub size: u64,
  /// Its links.
  pub links: u16,
  /// Its sectors in use (`i_blocks`).
  pub sectors: u32,
  /// Its generation ([`format::BUILT_GENERATION`] for an inode the writer wrote).
  pub generation: u32,
  /// Its access, modification and change times.
  pub times: [Timestamp; 3],
  /// Its block pointers (or a fast symlink's target, or a device's number).
  pub pointers: [u32; BLOCK_POINTERS],
  /// The raw bytes of its block-pointer array.
  pub raw_pointers: Vec<u8>,
}

impl Inode {
  /// Its type bits.
  pub fn type_bits(&self) -> u16 {
    self.mode & format::S_IFMT
  }

  /// Whether it is a directory.
  pub fn is_directory(&self) -> bool {
    self.type_bits() == format::S_IFDIR
  }
}

/// A tree read from an image, with the inode each node was read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageTree {
  /// The tree.
  pub tree: Tree,
  /// For each node, by index, the inode it was read from.
  pub inodes: Vec<u32>,
  /// For each node, by index, its inode's generation.
  pub generations: Vec<u32>,
}

/// An image opened for reading: its superblock and group descriptors checked. Every read is handed the source the
/// image was opened from.
#[derive(Debug)]
pub struct Image {
  len: u64,
  blocks: u64,
  inodes: u64,
  inodes_per_group: u64,
  /// Each group's inode table, by group.
  tables: Vec<u64>,
  /// The last indirect block read, and its pointers.
  cached: Option<(u64, Vec<u8>)>,
}

impl Image {
  /// Opens the image `source` holds, or says why it cannot be trusted.
  pub fn open<E>(source: &mut dyn ImageSource<E>) -> Result<Image, Failed<E>> {
    let len = source.image_len();
    let mut superblock = vec![0u8; SUPERBLOCK_BYTES];
    read_exact(source, len, SUPERBLOCK_OFFSET, &mut superblock)?;
    let field32 = |at: usize| u32_at(&superblock, at).map(u64::from).unwrap_or(0);
    let field16 = |at: usize| u16_at(&superblock, at).unwrap_or(0);
    let magic = field16(sb::MAGIC);
    if magic != MAGIC {
      return Err(Failed::Image(ImageRefusal::BadMagic { found: magic }));
    }
    require(
      "revision",
      field32(sb::REV_LEVEL),
      u64::from(format::DYNAMIC_REV),
    )?;
    require(
      "block size",
      field32(sb::LOG_BLOCK_SIZE),
      u64::from(LOG_BLOCK_SIZE),
    )?;
    require(
      "inode size",
      u64::from(field16(sb::INODE_SIZE)),
      INODE_BYTES,
    )?;
    require("first inode", field32(sb::FIRST_INO), u64::from(FIRST_INO))?;
    require("first data block", field32(sb::FIRST_DATA_BLOCK), 0)?;
    require(
      "blocks per group",
      field32(sb::BLOCKS_PER_GROUP),
      BLOCKS_PER_GROUP,
    )?;
    check_features(&superblock)?;
    if field16(sb::STATE) & STATE_VALID == 0 {
      return Err(Failed::Image(ImageRefusal::NotClean));
    }
    let blocks = field32(sb::BLOCKS_COUNT);
    let need = blocks.saturating_mul(BLOCK_BYTES);
    if blocks == 0 || need > len {
      return Err(Failed::Image(ImageRefusal::Truncated { need, have: len }));
    }
    let groups = blocks.div_ceil(BLOCKS_PER_GROUP);
    let inodes_per_group = field32(sb::INODES_PER_GROUP);
    if inodes_per_group == 0
      || !inodes_per_group.is_multiple_of(INODES_PER_BLOCK)
      || inodes_per_group > layout::MAX_INODES_PER_GROUP
    {
      return Err(Failed::Image(ImageRefusal::Geometry {
        reason: "inodes per group",
      }));
    }
    let inodes = field32(sb::INODES_COUNT);
    if inodes != groups.saturating_mul(inodes_per_group) {
      return Err(Failed::Image(ImageRefusal::Geometry {
        reason: "inode count",
      }));
    }
    let tables = read_tables(source, len, blocks, groups, inodes_per_group)?;
    Ok(Image {
      len,
      blocks,
      inodes,
      inodes_per_group,
      tables,
      cached: None,
    })
  }

  /// The inodes the file system holds.
  pub fn inode_count(&self) -> u64 {
    self.inodes
  }

  /// Inode `ino`, read and checked.
  pub fn inode<E>(&self, source: &mut dyn ImageSource<E>, ino: u32) -> Result<Inode, Failed<E>> {
    let number = u64::from(ino);
    if number == 0 || number > self.inodes {
      return Err(Failed::Image(ImageRefusal::InodeOutOfRange { ino: number }));
    }
    let index = number.saturating_sub(1);
    let group = usize::try_from(index.checked_div(self.inodes_per_group).unwrap_or(u64::MAX))
      .unwrap_or(usize::MAX);
    let table = *self
      .tables
      .get(group)
      .ok_or(ImageRefusal::InodeOutOfRange { ino: number })?;
    let offset = table.saturating_mul(BLOCK_BYTES).saturating_add(
      index
        .checked_rem(self.inodes_per_group)
        .unwrap_or(0)
        .saturating_mul(INODE_BYTES),
    );
    let mut raw = vec![0u8; usize::try_from(INODE_BYTES).unwrap_or(0)];
    read_exact(source, self.len, offset, &mut raw)?;
    parse_inode(ino, &raw).map_err(Failed::Image)
  }

  /// Reads regular file `ino`'s bytes from `offset` into `buf`; how many it read (fewer only at the end).
  pub fn read<E>(
    &mut self,
    source: &mut dyn ImageSource<E>,
    ino: u32,
    offset: u64,
    buf: &mut [u8],
  ) -> Result<usize, Failed<E>> {
    let inode = self.inode(source, ino)?;
    if inode.type_bits() != format::S_IFREG {
      return Err(Failed::Image(ImageRefusal::InodeUnsupported {
        ino,
        reason: "not a regular file",
      }));
    }
    self.read_inode(source, &inode, offset, buf)
  }

  /// Reads `inode`'s bytes from `offset` into `buf`, holes as zeros.
  fn read_inode<E>(
    &mut self,
    source: &mut dyn ImageSource<E>,
    inode: &Inode,
    offset: u64,
    buf: &mut [u8],
  ) -> Result<usize, Failed<E>> {
    let available = inode.size.saturating_sub(offset);
    let want =
      usize::try_from(available.min(u64::try_from(buf.len()).unwrap_or(u64::MAX))).unwrap_or(0);
    let mut done = 0usize;
    while done < want {
      let at = offset.saturating_add(u64::try_from(done).unwrap_or(u64::MAX));
      let (index, within) = (at / BLOCK_BYTES, at % BLOCK_BYTES);
      let take = usize::try_from(
        BLOCK_BYTES
          .saturating_sub(within)
          .min(u64::try_from(want.saturating_sub(done)).unwrap_or(u64::MAX)),
      )
      .unwrap_or(0);
      let into = buf
        .get_mut(done..done.saturating_add(take))
        .ok_or(ImageRefusal::Truncated {
          need: at,
          have: self.len,
        })?;
      match self.block_of(source, inode, index)? {
        Some(block) => {
          let from = block.saturating_mul(BLOCK_BYTES).saturating_add(within);
          read_exact(source, self.len, from, into)?;
        }
        None => into.fill(0),
      }
      done = done.saturating_add(take);
    }
    Ok(want)
  }

  /// The tree the image holds, its entries held to `most`: a [`Walk`] run to its end.
  pub fn tree<E>(
    &mut self,
    source: &mut dyn ImageSource<E>,
    most: usize,
  ) -> Result<ImageTree, Failed<E>> {
    let mut walk = Walk::new(self, source, most)?;
    loop {
      if let Walked::Done(tree) = walk.step(self, source, u64::MAX)? {
        return Ok(tree);
      }
    }
  }

  /// The entries of block `index` of `directory` (without `.` and `..`), checked.
  fn directory_block<E>(
    &mut self,
    source: &mut dyn ImageSource<E>,
    directory: &Inode,
    index: u64,
  ) -> Result<Vec<(Vec<u8>, u32)>, Failed<E>> {
    let ino = directory.ino;
    let Some(physical) = self.block_of(source, directory, index)? else {
      return Err(Failed::Image(ImageRefusal::DirectoryCorrupt {
        ino,
        reason: "a hole",
      }));
    };
    let mut block = vec![0u8; usize::try_from(BLOCK_BYTES).unwrap_or(0)];
    read_exact(
      source,
      self.len,
      physical.saturating_mul(BLOCK_BYTES),
      &mut block,
    )?;
    parse_directory_block(ino, &block, self.inodes).map_err(Failed::Image)
  }

  /// The node `inode` is: its kind (a symlink's target and a device's number read here) and attributes.
  pub fn node<E>(
    &mut self,
    source: &mut dyn ImageSource<E>,
    inode: &Inode,
  ) -> Result<Node, Failed<E>> {
    let ino = inode.ino;
    let kind = match inode.type_bits() {
      format::S_IFDIR => Kind::Directory,
      format::S_IFREG => Kind::File { size: inode.size },
      format::S_IFLNK => Kind::Symlink {
        target: self.link_target(source, inode)?,
      },
      format::S_IFCHR | format::S_IFBLK => {
        let rdev = layout::decode_rdev(inode.pointers[0], inode.pointers[1]).ok_or(
          ImageRefusal::InodeUnsupported {
            ino,
            reason: "device number",
          },
        )?;
        if inode.type_bits() == format::S_IFCHR {
          Kind::CharDevice { rdev }
        } else {
          Kind::BlockDevice { rdev }
        }
      }
      format::S_IFIFO => Kind::Fifo,
      format::S_IFSOCK => Kind::Socket,
      _ => {
        return Err(Failed::Image(ImageRefusal::InodeUnsupported {
          ino,
          reason: "type",
        }));
      }
    };
    let [atime, mtime, ctime] = inode.times;
    Ok(Node {
      kind,
      permissions: inode.mode & format::PERMISSION_BITS,
      uid: inode.uid,
      gid: inode.gid,
      atime,
      mtime,
      ctime,
    })
  }

  /// A symlink's target: in the inode when it has no blocks (a fast symlink), else in its first block.
  fn link_target<E>(
    &mut self,
    source: &mut dyn ImageSource<E>,
    inode: &Inode,
  ) -> Result<Vec<u8>, Failed<E>> {
    let ino = inode.ino;
    let length = usize::try_from(inode.size).unwrap_or(usize::MAX);
    let bad = ImageRefusal::InodeUnsupported {
      ino,
      reason: "symlink target",
    };
    if length == 0 || length >= usize::try_from(BLOCK_BYTES).unwrap_or(usize::MAX) {
      return Err(Failed::Image(bad));
    }
    if inode.sectors == 0 {
      return inode
        .raw_pointers
        .get(..length)
        .filter(|_| length < BLOCK_POINTER_BYTES)
        .map(<[u8]>::to_vec)
        .ok_or(Failed::Image(bad));
    }
    let mut target = vec![0u8; length];
    self.read_inode(source, inode, 0, &mut target)?;
    Ok(target)
  }

  /// The block holding logical block `index` of `inode`, or `None` for a hole.
  fn block_of<E>(
    &mut self,
    source: &mut dyn ImageSource<E>,
    inode: &Inode,
    index: u64,
  ) -> Result<Option<u64>, Failed<E>> {
    let path = layout::path(index).ok_or(ImageRefusal::InodeUnsupported {
      ino: inode.ino,
      reason: "beyond what an inode maps",
    })?;
    let top = |slot: usize| inode.pointers.get(slot).copied().unwrap_or(0);
    let mut chain: Vec<u64> = Vec::with_capacity(format::INDIRECT_LEVELS);
    let mut block = match path {
      Path::Direct(slot) => top(slot),
      Path::Single(first) => {
        chain.push(first);
        top(format::SINGLE_INDIRECT)
      }
      Path::Double(first, second) => {
        chain.extend([first, second]);
        top(format::DOUBLE_INDIRECT)
      }
      Path::Triple(first, second, third) => {
        chain.extend([first, second, third]);
        top(format::TRIPLE_INDIRECT)
      }
    };
    for index in chain {
      if block == 0 {
        return Ok(None);
      }
      let checked = self.checked_block(block)?;
      block = self.pointer_in(source, checked, index)?;
    }
    if block == 0 {
      return Ok(None);
    }
    Ok(Some(self.checked_block(block)?))
  }

  /// Block `block`, refused outside the file system.
  fn checked_block<E>(&self, block: u32) -> Result<u64, Failed<E>> {
    let block = u64::from(block);
    if block >= self.blocks {
      return Err(Failed::Image(ImageRefusal::BlockOutOfRange { block }));
    }
    Ok(block)
  }

  /// Pointer `index` of indirect block `block`.
  fn pointer_in<E>(
    &mut self,
    source: &mut dyn ImageSource<E>,
    block: u64,
    index: u64,
  ) -> Result<u32, Failed<E>> {
    let fresh = !matches!(&self.cached, Some((cached, _)) if *cached == block);
    if fresh {
      let mut bytes = vec![0u8; usize::try_from(BLOCK_BYTES).unwrap_or(0)];
      read_exact(
        source,
        self.len,
        block.saturating_mul(BLOCK_BYTES),
        &mut bytes,
      )?;
      self.cached = Some((block, bytes));
    }
    let width = u64::try_from(format::POINTER_BYTES).unwrap_or(u64::MAX);
    let at = usize::try_from(index.saturating_mul(width)).unwrap_or(usize::MAX);
    self
      .cached
      .as_ref()
      .and_then(|(_, bytes)| u32_at(bytes, at))
      .ok_or(Failed::Image(ImageRefusal::BlockOutOfRange { block }))
  }
}

/// Where a [`Walk`] stands after a step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Walked {
  /// More remains: step again.
  More,
  /// The tree is read whole.
  Done(ImageTree),
}

/// The directory a [`Walk`] is reading: its inode, the next of its blocks to read, and the entries of the block read
/// last still to add.
#[derive(Debug)]
struct Reading {
  directory: Inode,
  next_block: u64,
  entries: VecDeque<(Vec<u8>, u32)>,
}

/// A walk of an image's tree in steps a caller bounds (§4.3). Each [`Walk::step`] reads until a byte budget is spent:
/// a directory a block at a time, and each entry of the block, reading the inode it names the first time one does.
/// The directories come in the order a depth-first walk meets them, so one image always walks to the same tree. A
/// second entry naming an inode already met is a hard link, or a directory named twice, which the tree refuses.
#[derive(Debug)]
pub struct Walk {
  /// The tree so far; taken when the walk ends.
  grower: Option<TreeGrower>,
  inodes: Vec<u32>,
  generations: Vec<u32>,
  /// The node each inode met so far became.
  met: BTreeMap<u32, NodeId>,
  /// Directories named and still to read.
  pending: Vec<(Inode, NodeId)>,
  /// The directory being read.
  reading: Option<Reading>,
  /// The entries read so far, held to `most`.
  read: usize,
  most: usize,
}

impl Walk {
  /// A walk of the tree `image` holds, its entries held to `most`: the root's inode read and checked.
  pub fn new<E>(
    image: &mut Image,
    source: &mut dyn ImageSource<E>,
    most: usize,
  ) -> Result<Walk, Failed<E>> {
    let root = image.inode(source, ROOT_INO)?;
    if !root.is_directory() {
      return Err(Failed::Image(ImageRefusal::RootNotDirectory));
    }
    let grower = TreeGrower::new(image.node(source, &root)?).map_err(Failed::Image)?;
    let generation = root.generation;
    Ok(Walk {
      grower: Some(grower),
      inodes: vec![ROOT_INO],
      generations: vec![generation],
      met: BTreeMap::from([(ROOT_INO, NodeId::ROOT)]),
      pending: vec![(root, NodeId::ROOT)],
      reading: None,
      read: 0,
      most,
    })
  }

  /// Reads until at least `budget` bytes are read or the tree is whole: at least one directory block or entry a step,
  /// so every step makes progress. A step after the end is refused.
  pub fn step<E>(
    &mut self,
    image: &mut Image,
    source: &mut dyn ImageSource<E>,
    budget: u64,
  ) -> Result<Walked, Failed<E>> {
    let mut spent = 0u64;
    loop {
      match self.advance(image, source)? {
        Some(cost) => {
          spent = spent.saturating_add(cost.max(1));
          if spent >= budget {
            return Ok(Walked::More);
          }
        }
        None => return self.finish().map(Walked::Done),
      }
    }
  }

  /// The tree, once every directory is read.
  fn finish<E>(&mut self) -> Result<ImageTree, Failed<E>> {
    let grower = self.grower.take().ok_or(ImageRefusal::GrowthOutOfOrder {
      reason: "a walk stepped after its end",
    })?;
    Ok(ImageTree {
      tree: grower.finish()?,
      inodes: std::mem::take(&mut self.inodes),
      generations: std::mem::take(&mut self.generations),
    })
  }

  /// One unit of the walk: an entry of the block read last, the directory's next block, its close, or the next
  /// directory's open. Returns its cost in bytes read, or `None` once nothing is left.
  fn advance<E>(
    &mut self,
    image: &mut Image,
    source: &mut dyn ImageSource<E>,
  ) -> Result<Option<u64>, Failed<E>> {
    let grower = self.grower.as_mut().ok_or(ImageRefusal::GrowthOutOfOrder {
      reason: "a walk stepped after its end",
    })?;
    if let Some(reading) = self.reading.as_mut() {
      if let Some((name, child)) = reading.entries.pop_front() {
        return self.add_entry(image, source, name, child).map(Some);
      }
      if reading.next_block < reading.directory.size / BLOCK_BYTES {
        let entries = image.directory_block(source, &reading.directory, reading.next_block)?;
        reading.next_block = reading.next_block.saturating_add(1);
        reading.entries = entries.into();
        return Ok(Some(BLOCK_BYTES));
      }
      grower.close()?;
      self.reading = None;
      return Ok(Some(0));
    }
    let Some((directory, id)) = self.pending.pop() else {
      return Ok(None);
    };
    if !directory.size.is_multiple_of(BLOCK_BYTES) {
      return Err(Failed::Image(ImageRefusal::DirectoryCorrupt {
        ino: directory.ino,
        reason: "size not whole blocks",
      }));
    }
    grower.open(id)?;
    self.reading = Some(Reading {
      directory,
      next_block: 0,
      entries: VecDeque::new(),
    });
    Ok(Some(0))
  }

  /// Adds the entry naming inode `child` `name` to the directory being read, reading the inode the first time an
  /// entry names it. Returns the bytes read: the entry's record, and the inode and a slow symlink's block when read.
  fn add_entry<E>(
    &mut self,
    image: &mut Image,
    source: &mut dyn ImageSource<E>,
    name: Vec<u8>,
    child: u32,
  ) -> Result<u64, Failed<E>> {
    self.read = self.read.saturating_add(1);
    if self.read > self.most {
      return Err(Failed::Image(ImageRefusal::TreeTooLarge {
        most: self.most,
      }));
    }
    let mut cost =
      u64::try_from(DIRENT_HEADER_BYTES.saturating_add(name.len())).unwrap_or(u64::MAX);
    let grower = self.grower.as_mut().ok_or(ImageRefusal::GrowthOutOfOrder {
      reason: "a walk stepped after its end",
    })?;
    let id = match self.met.get(&child) {
      Some(known) => *known,
      None => {
        let inode = image.inode(source, child)?;
        cost = cost.saturating_add(INODE_BYTES);
        if inode.type_bits() == format::S_IFLNK && inode.sectors > 0 {
          cost = cost.saturating_add(BLOCK_BYTES);
        }
        let id = grower.add_node(image.node(source, &inode)?)?;
        self.inodes.push(child);
        self.generations.push(inode.generation);
        self.met.insert(child, id);
        if inode.is_directory() {
          self.pending.push((inode, id));
        }
        id
      }
    };
    // The tree names a directory by its node; a reader names it by its inode, as the image does.
    grower
      .add_entry(name, id)
      .map_err(|refusal| match refusal {
        ImageRefusal::DirectoryLinkedTwice { .. } => ImageRefusal::DirectoryLinkedTwice {
          node: u64::from(child),
        },
        other => other,
      })?;
    Ok(cost)
  }
}

/// Reads exactly `buf.len()` bytes from `offset`, refused when the range passes the image's end.
fn read_exact<E>(
  source: &mut dyn ImageSource<E>,
  len: u64,
  offset: u64,
  buf: &mut [u8],
) -> Result<(), Failed<E>> {
  let need = offset.saturating_add(u64::try_from(buf.len()).unwrap_or(u64::MAX));
  if need > len {
    return Err(Failed::Image(ImageRefusal::Truncated { need, have: len }));
  }
  source.read_at(offset, buf).map_err(Failed::Source)
}

/// A superblock field that must be what the writer wrote.
fn require<E>(field: &'static str, value: u64, expected: u64) -> Result<(), Failed<E>> {
  if value != expected {
    return Err(Failed::Image(ImageRefusal::Unsupported { field, value }));
  }
  Ok(())
}

/// The superblock's features must be among the known ones: a guest's kernel adds none of its own to an ext2 file
/// system in ordinary use, and one this reader does not know may change what a structure means.
fn check_features<E>(superblock: &[u8]) -> Result<(), Failed<E>> {
  let incompat = u32_at(superblock, sb::FEATURE_INCOMPAT).unwrap_or(u32::MAX);
  let ro_compat = u32_at(superblock, sb::FEATURE_RO_COMPAT).unwrap_or(u32::MAX);
  let unknown_incompat = incompat & !INCOMPAT_FILETYPE;
  let unknown_ro_compat =
    ro_compat & !(RO_COMPAT_SPARSE_SUPER | RO_COMPAT_LARGE_FILE | RO_COMPAT_EXTRA_ISIZE);
  if unknown_incompat != 0 || unknown_ro_compat != 0 {
    return Err(Failed::Image(ImageRefusal::FeaturesUnsupported {
      incompat: unknown_incompat,
      ro_compat: unknown_ro_compat,
    }));
  }
  Ok(())
}

/// Each group's inode table from the group descriptors, refused unless it lies inside its own group.
fn read_tables<E>(
  source: &mut dyn ImageSource<E>,
  len: u64,
  blocks: u64,
  groups: u64,
  inodes_per_group: u64,
) -> Result<Vec<u64>, Failed<E>> {
  let descriptor = u64::try_from(GROUP_DESC_BYTES).unwrap_or(u64::MAX);
  let table_bytes = groups.saturating_mul(descriptor);
  let mut raw = vec![
    0u8;
    usize::try_from(table_bytes)
      .map_err(|_| ImageRefusal::Geometry { reason: "groups" })?
  ];
  // The descriptor table starts in the block after the superblock's (block 1 for 4 KiB blocks).
  read_exact(source, len, BLOCK_BYTES, &mut raw)?;
  let table_blocks = inodes_per_group / INODES_PER_BLOCK;
  let mut tables = Vec::with_capacity(usize::try_from(groups).unwrap_or(0));
  for group in 0..groups {
    let at = usize::try_from(group.saturating_mul(descriptor)).unwrap_or(usize::MAX);
    let field = |offset: usize| {
      u32_at(&raw, at.saturating_add(offset))
        .map(u64::from)
        .ok_or(ImageRefusal::Geometry {
          reason: "group descriptors",
        })
    };
    let block_bitmap = field(format::group_desc::BLOCK_BITMAP)?;
    let inode_bitmap = field(format::group_desc::INODE_BITMAP)?;
    let table = field(format::group_desc::INODE_TABLE)?;
    let start = group.saturating_mul(BLOCKS_PER_GROUP);
    let end = start.saturating_add(BLOCKS_PER_GROUP).min(blocks);
    // Each of a group's structures lies inside the group, as `ext2fs_check_desc` requires: a descriptor read at the
    // wrong offset fails here, before any inode is read from the wrong block (2026-10-23: a 36-byte descriptor put
    // group 1's block bitmap "at 0").
    let inside = |first: u64, count: u64| first >= start && first.saturating_add(count) <= end;
    if !inside(block_bitmap, 1) || !inside(inode_bitmap, 1) || !inside(table, table_blocks) {
      return Err(Failed::Image(ImageRefusal::Geometry {
        reason: "a group's structures outside it",
      }));
    }
    tables.push(table);
  }
  Ok(tables)
}

/// Inode `ino` from its raw bytes, checked.
fn parse_inode(ino: u32, raw: &[u8]) -> Result<Inode, ImageRefusal> {
  use crate::format::inode as at;
  let field16 = |offset: usize| u16_at(raw, offset).unwrap_or(0);
  let field32 = |offset: usize| u32_at(raw, offset).unwrap_or(0);
  let unsupported = |reason: &'static str| ImageRefusal::InodeUnsupported { ino, reason };
  let mode = field16(at::MODE);
  let links = field16(at::LINKS_COUNT);
  if mode == 0 || links == 0 {
    return Err(unsupported("an entry names a free inode"));
  }
  let flags = field32(at::FLAGS);
  if flags & (INODE_FLAG_EXTENTS | INODE_FLAG_INLINE_DATA) != 0 {
    return Err(unsupported("extents or inline data"));
  }
  let extra = usize::from(field16(at::EXTRA_ISIZE));
  let room = usize::try_from(INODE_BYTES)
    .unwrap_or(0)
    .saturating_sub(GOOD_OLD_INODE_BYTES);
  if extra > room || !extra.is_multiple_of(DIRENT_PAD) {
    return Err(unsupported("extra fields size"));
  }
  // An extra time word counts only when the inode's extra fields reach past it.
  let extra_word = |offset: usize| {
    let end = offset
      .saturating_add(size_of::<u32>())
      .saturating_sub(GOOD_OLD_INODE_BYTES);
    if extra >= end { field32(offset) } else { 0 }
  };
  let time = |low: usize, high: usize| {
    layout::decode_time(field32(low), extra_word(high)).ok_or(unsupported("time"))
  };
  let times = [
    time(at::ATIME, at::ATIME_EXTRA)?,
    time(at::MTIME, at::MTIME_EXTRA)?,
    time(at::CTIME, at::CTIME_EXTRA)?,
  ];
  let regular = mode & format::S_IFMT == format::S_IFREG;
  let size = if regular {
    u64::from(field32(at::SIZE)) | (u64::from(field32(at::SIZE_HIGH)) << u32::BITS)
  } else {
    u64::from(field32(at::SIZE))
  };
  if regular && layout::Map::new(size.div_ceil(BLOCK_BYTES)).is_err() {
    return Err(unsupported("size"));
  }
  let raw_pointers = raw
    .get(at::BLOCK..at::BLOCK.saturating_add(BLOCK_POINTER_BYTES))
    .map(<[u8]>::to_vec)
    .ok_or(unsupported("block pointers"))?;
  let mut pointers = [0u32; BLOCK_POINTERS];
  for (slot, pointer) in pointers.iter_mut().enumerate() {
    *pointer = u32_at(&raw_pointers, slot.saturating_mul(format::POINTER_BYTES)).unwrap_or(0);
  }
  Ok(Inode {
    ino,
    mode,
    uid: u32::from(field16(at::UID)) | (u32::from(field16(at::UID_HIGH)) << u16::BITS),
    gid: u32::from(field16(at::GID)) | (u32::from(field16(at::GID_HIGH)) << u16::BITS),
    size,
    links,
    sectors: field32(at::BLOCKS),
    generation: field32(at::GENERATION),
    times,
    pointers,
    raw_pointers,
  })
}

/// The entries one directory block of directory `ino` holds (without `.` and `..`), checked: every record fits the
/// block and holds its name, every name is one an entry may hold, every inode is in range.
fn parse_directory_block(
  ino: u32,
  block: &[u8],
  inodes: u64,
) -> Result<Vec<(Vec<u8>, u32)>, ImageRefusal> {
  let corrupt = |reason: &'static str| ImageRefusal::DirectoryCorrupt { ino, reason };
  let mut entries = Vec::new();
  let mut offset = 0usize;
  while offset < block.len() {
    let child = u32_at(block, offset).ok_or(corrupt("a record past the block"))?;
    let length = usize::from(
      u16_at(
        block,
        offset.saturating_add(format::DIRENT_RECORD_LENGTH_AT),
      )
      .ok_or(corrupt("a record past the block"))?,
    );
    let name_len = usize::from(
      u8_at(block, offset.saturating_add(format::DIRENT_NAME_LENGTH_AT))
        .ok_or(corrupt("a record past the block"))?,
    );
    if length < DIRENT_HEADER_BYTES
      || !length.is_multiple_of(DIRENT_PAD)
      || offset.saturating_add(length) > block.len()
    {
      return Err(corrupt("record length"));
    }
    if child != 0 {
      if DIRENT_HEADER_BYTES.saturating_add(name_len) > length {
        return Err(corrupt("a name past its record"));
      }
      let start = offset.saturating_add(DIRENT_HEADER_BYTES);
      let name = block
        .get(start..start.saturating_add(name_len))
        .ok_or(corrupt("a name past its record"))?;
      if name != b"." && name != b".." {
        crate::tree::check_name(name).map_err(|_| corrupt("name"))?;
        if child == 0 || u64::from(child) > inodes {
          return Err(ImageRefusal::InodeOutOfRange {
            ino: u64::from(child),
          });
        }
        entries.push((name.to_vec(), child));
      }
    }
    offset = offset.saturating_add(length);
  }
  Ok(entries)
}
