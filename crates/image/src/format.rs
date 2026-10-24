//! The ext2 on-disk format this crate writes and reads: revision 1 ("dynamic"), as e2fsprogs defines it in
//! `lib/ext2fs/ext2_fs.h` (read 2026-10-23 at e2fsprogs `master`, 1.47.4). Every offset and code here is that header's,
//! named after its field; nothing is invented. The little-endian field codecs at the end build a structure by
//! appending its fields in order and read one by checked offset, so no structure is ever indexed out of bounds.
//!
//! What is written is the subset every Linux guest mounts read-write: the ext2 driver, and the ext4 driver, which
//! serves ext2 on kernels built with `CONFIG_EXT4_USE_FOR_EXT2` (the box's 7.2.9, 2026-10-23). Its features are
//! `filetype`, `sparse_super` and `large_file`, the set `mkfs.ext2 -O ^resize_inode,^dir_index,^ext_attr` makes, and an
//! image made that way is the reference the differential test holds this format to.

/// Format: the byte offset of the primary superblock from the start of the image (`SUPERBLOCK_OFFSET`).
pub const SUPERBLOCK_OFFSET: u64 = 1024;
/// Format: the superblock's length (`SUPERBLOCK_SIZE`).
pub const SUPERBLOCK_BYTES: usize = 1024;
/// Format: `EXT2_SUPER_MAGIC`.
pub const MAGIC: u16 = 0xEF53;
/// Format: `EXT2_DYNAMIC_REV`, the revision with variable inode sizes and feature flags.
pub const DYNAMIC_REV: u32 = 1;
/// Format: `EXT2_OS_LINUX`, the creator OS whose inode fields (`osd1`, `osd2`) this format uses.
pub const OS_LINUX: u32 = 0;
/// Format: `EXT2_VALID_FS`, the state bit of a cleanly unmounted file system.
pub const STATE_VALID: u16 = 0x0001;
/// Format: `EXT2_ERRORS_CONTINUE`, the default behaviour on an error.
pub const ERRORS_CONTINUE: u16 = 1;
/// Format: `EXT2_FLAGS_SIGNED_HASH`, set by `mke2fs` on a platform whose `char` is signed. It names the hash a
/// directory index would use; no index is written, and the flag is set as the reference image sets it.
pub const FLAGS_SIGNED_HASH: u32 = 0x0001;
/// Format: the smallest block size, `EXT2_MIN_BLOCK_SIZE`; a block size is this shifted left by `s_log_block_size`.
pub const MIN_BLOCK_BYTES: u32 = 1024;
/// Format: the original inode size, `EXT2_GOOD_OLD_INODE_SIZE`; the fields past it are the large inode's.
pub const GOOD_OLD_INODE_BYTES: usize = 128;
/// Format: the bytes of a group descriptor without the 64-bit feature (`struct ext2_group_desc`).
pub const GROUP_DESC_BYTES: usize = 32;
/// Format: `EXT2_ROOT_INO`.
pub const ROOT_INO: u32 = 2;
/// Format: `EXT2_GOOD_OLD_FIRST_INO`, the first inode that is not reserved; every inode below it is marked in use.
pub const FIRST_INO: u32 = 11;
/// Format: the direct block pointers of an inode, `EXT2_NDIR_BLOCKS`.
pub const DIRECT_BLOCKS: u64 = 12;
/// Format: the block pointers an inode holds, `EXT2_N_BLOCKS`: the direct ones, then the single, double and triple
/// indirect pointers.
pub const BLOCK_POINTERS: usize = 15;
/// Format: the bytes of one block pointer (a 32-bit block number), in an inode or an indirect block.
pub const POINTER_BYTES: usize = 4;
/// Format: the bytes of an inode's block-pointer array, where a fast symlink keeps its target instead.
pub const BLOCK_POINTER_BYTES: usize = BLOCK_POINTERS * POINTER_BYTES;
/// Format: the `i_block` slot of the single indirect pointer (`EXT2_IND_BLOCK`).
pub const SINGLE_INDIRECT: usize = 12;
/// Format: the `i_block` slot of the double indirect pointer (`EXT2_DIND_BLOCK`).
pub const DOUBLE_INDIRECT: usize = 13;
/// Format: the `i_block` slot of the triple indirect pointer (`EXT2_TIND_BLOCK`).
pub const TRIPLE_INDIRECT: usize = 14;
/// Format: the levels of indirection below an inode's top pointers (single, double, triple).
pub const INDIRECT_LEVELS: usize = 3;
/// Format: the bases whose powers are the groups carrying superblock copies under `sparse_super` (with groups 0 and
/// 1; `ext2fs_bg_has_super`).
pub const SPARSE_SUPER_BASES: [u64; 3] = [3, 5, 7];
/// Format: the 512-byte sectors `i_blocks` counts in.
pub const SECTOR_BYTES: u64 = 512;
/// Format: the longest name a directory entry holds, `EXT2_NAME_LEN`.
pub const NAME_MAX: usize = 255;
/// Format: the fixed part of a directory entry: inode (4), record length (2), name length (1), file type (1).
pub const DIRENT_HEADER_BYTES: usize = 8;
/// Format: a directory entry's record length, after its inode number.
pub const DIRENT_RECORD_LENGTH_AT: usize = 4;
/// Format: a directory entry's name length, after its record length.
pub const DIRENT_NAME_LENGTH_AT: usize = 6;
/// Format: directory entries are padded to this multiple (`EXT2_DIR_PAD`).
pub const DIRENT_PAD: usize = 4;
/// Format: the most links an inode may carry under the ext2 driver, `EXT2_LINK_MAX` (`fs/ext2/ext2.h`); the ext4
/// driver allows 65000, so this is the bound both accept.
pub const LINK_MAX: u64 = 32000;

/// Format: `EXT2_FEATURE_INCOMPAT_FILETYPE`: directory entries carry their target's type.
pub const INCOMPAT_FILETYPE: u32 = 0x0002;
/// Format: `EXT2_FEATURE_RO_COMPAT_SPARSE_SUPER`: superblock backups only in groups 0, 1 and powers of 3, 5 and 7.
pub const RO_COMPAT_SPARSE_SUPER: u32 = 0x0001;
/// Format: `EXT2_FEATURE_RO_COMPAT_LARGE_FILE`: a regular file's size has a high 32-bit half.
pub const RO_COMPAT_LARGE_FILE: u32 = 0x0002;
/// Format: `EXT4_FEATURE_RO_COMPAT_EXTRA_ISIZE`, which a guest's ext4 driver may set; it changes nothing this crate
/// reads, so an image carrying it is still read.
pub const RO_COMPAT_EXTRA_ISIZE: u32 = 0x0040;
/// Format: `EXT4_EXTENTS_FL`, an inode mapped by extents, which a file system without the extents feature never has.
pub const INODE_FLAG_EXTENTS: u32 = 0x0008_0000;
/// Format: `EXT4_INLINE_DATA_FL`, an inode whose data is inline, which a file system without that feature never has.
pub const INODE_FLAG_INLINE_DATA: u32 = 0x1000_0000;

/// Format: the file-type bits of a mode (`S_IFMT`).
pub const S_IFMT: u16 = 0o170_000;
/// Format: a directory's type bits.
pub const S_IFDIR: u16 = 0o040_000;
/// Format: a regular file's type bits.
pub const S_IFREG: u16 = 0o100_000;
/// Format: a symbolic link's type bits.
pub const S_IFLNK: u16 = 0o120_000;
/// Format: a character device's type bits.
pub const S_IFCHR: u16 = 0o020_000;
/// Format: a block device's type bits.
pub const S_IFBLK: u16 = 0o060_000;
/// Format: a FIFO's type bits.
pub const S_IFIFO: u16 = 0o010_000;
/// Format: a socket's type bits.
pub const S_IFSOCK: u16 = 0o140_000;
/// Format: the permission bits of a mode, with set-user-id, set-group-id and sticky.
pub const PERMISSION_BITS: u16 = 0o7777;

/// Format: a directory entry's file type for a regular file (`EXT2_FT_REG_FILE`).
pub const FT_REG_FILE: u8 = 1;
/// Format: `EXT2_FT_DIR`.
pub const FT_DIR: u8 = 2;
/// Format: `EXT2_FT_CHRDEV`.
pub const FT_CHRDEV: u8 = 3;
/// Format: `EXT2_FT_BLKDEV`.
pub const FT_BLKDEV: u8 = 4;
/// Format: `EXT2_FT_FIFO`.
pub const FT_FIFO: u8 = 5;
/// Format: `EXT2_FT_SOCK`.
pub const FT_SOCK: u8 = 6;
/// Format: `EXT2_FT_SYMLINK`.
pub const FT_SYMLINK: u8 = 7;

/// Format: the epoch bits of an inode's extra time field (`EXT4_EPOCH_BITS`): the low two bits extend the 32-bit
/// seconds, the rest are nanoseconds.
pub const EPOCH_BITS: u32 = 2;
/// Format: the mask of the epoch bits (`EXT4_EPOCH_MASK`).
pub const EPOCH_MASK: u32 = (1 << EPOCH_BITS) - 1;
/// Format: the bytes of an inode's extra fields this format writes (`i_extra_isize`): the change, modification and
/// access times' extra words, the creation time and its extra word, the high version and the project id. The
/// reference image's `s_min_extra_isize` and `s_want_extra_isize` are this.
pub const EXTRA_ISIZE: u16 = 32;
/// Format: the `i_generation` every inode the writer writes carries. A guest's kernel gives each inode it allocates a
/// generation of its own: Linux's ext2 driver from a per-mount counter that starts at a random value
/// (`ext2_new_inode`, `ext2_fill_super`), its ext4 driver at random. So an inode that still carries this generation
/// at its written number is, but for a chance of one in 2³², the node the writer put there, renamed or rewritten
/// perhaps: what a merge keeps the node's identity by (A-160).
pub const BUILT_GENERATION: u32 = 0;

/// Superblock field offsets and widths, from `struct ext2_super_block`.
pub mod superblock {
  /// Format: the bytes of `s_uuid`.
  pub const UUID_BYTES: usize = 16;
  /// Format: the bytes of `s_volume_name` (`EXT2_LABEL_LEN`).
  pub const VOLUME_NAME_BYTES: usize = 16;
  /// Format: the bytes of `s_last_mounted`.
  pub const LAST_MOUNTED_BYTES: usize = 64;
  /// Format: the bytes of `s_journal_uuid`.
  pub const JOURNAL_UUID_BYTES: usize = 16;
  /// Format: the bytes of `s_hash_seed` (four 32-bit words).
  pub const HASH_SEED_BYTES: usize = 16;
  /// Format: the bytes of `s_jnl_blocks` (seventeen 32-bit words).
  pub const JOURNAL_BACKUP_BYTES: usize = 17 * 4;
  /// Format: the offset of `s_wtime_hi`, the first of the high bytes of the 64-bit times.
  pub const TIME_HIGH: usize = 0x274;
  /// Format: `s_inodes_count`.
  pub const INODES_COUNT: usize = 0x00;
  /// Format: `s_blocks_count`.
  pub const BLOCKS_COUNT: usize = 0x04;
  /// Format: `s_free_blocks_count`.
  pub const FREE_BLOCKS_COUNT: usize = 0x0C;
  /// Format: `s_free_inodes_count`.
  pub const FREE_INODES_COUNT: usize = 0x10;
  /// Format: `s_first_data_block`.
  pub const FIRST_DATA_BLOCK: usize = 0x14;
  /// Format: `s_log_block_size`.
  pub const LOG_BLOCK_SIZE: usize = 0x18;
  /// Format: `s_blocks_per_group`.
  pub const BLOCKS_PER_GROUP: usize = 0x20;
  /// Format: `s_inodes_per_group`.
  pub const INODES_PER_GROUP: usize = 0x28;
  /// Format: `s_magic`.
  pub const MAGIC: usize = 0x38;
  /// Format: `s_state`.
  pub const STATE: usize = 0x3A;
  /// Format: `s_rev_level`.
  pub const REV_LEVEL: usize = 0x4C;
  /// Format: `s_first_ino`.
  pub const FIRST_INO: usize = 0x54;
  /// Format: `s_inode_size`.
  pub const INODE_SIZE: usize = 0x58;
  /// Format: `s_feature_incompat`.
  pub const FEATURE_INCOMPAT: usize = 0x60;
  /// Format: `s_feature_ro_compat`.
  pub const FEATURE_RO_COMPAT: usize = 0x64;
}

/// Inode field offsets, from `struct ext2_inode_large`.
pub mod inode {
  /// Format: `i_mode`.
  pub const MODE: usize = 0x00;
  /// Format: `i_uid` (low 16 bits).
  pub const UID: usize = 0x02;
  /// Format: `i_size` (low 32 bits).
  pub const SIZE: usize = 0x04;
  /// Format: `i_atime`.
  pub const ATIME: usize = 0x08;
  /// Format: `i_ctime`.
  pub const CTIME: usize = 0x0C;
  /// Format: `i_mtime`.
  pub const MTIME: usize = 0x10;
  /// Format: `i_gid` (low 16 bits).
  pub const GID: usize = 0x18;
  /// Format: `i_links_count`.
  pub const LINKS_COUNT: usize = 0x1A;
  /// Format: `i_blocks`, in 512-byte sectors.
  pub const BLOCKS: usize = 0x1C;
  /// Format: `i_flags`.
  pub const FLAGS: usize = 0x20;
  /// Format: `i_block`, the 15 block pointers.
  pub const BLOCK: usize = 0x28;
  /// Format: `i_generation`, the inode's version for file handles.
  pub const GENERATION: usize = 0x64;
  /// Format: `i_size_high`.
  pub const SIZE_HIGH: usize = 0x6C;
  /// Format: `l_i_uid_high`.
  pub const UID_HIGH: usize = 0x78;
  /// Format: `l_i_gid_high`.
  pub const GID_HIGH: usize = 0x7A;
  /// Format: `i_extra_isize`.
  pub const EXTRA_ISIZE: usize = 0x80;
  /// Format: `i_ctime_extra`.
  pub const CTIME_EXTRA: usize = 0x84;
  /// Format: `i_mtime_extra`.
  pub const MTIME_EXTRA: usize = 0x88;
  /// Format: `i_atime_extra`.
  pub const ATIME_EXTRA: usize = 0x8C;
}

/// Group descriptor field offsets, from `struct ext2_group_desc`.
pub mod group_desc {
  /// Format: `bg_block_bitmap`.
  pub const BLOCK_BITMAP: usize = 0x00;
  /// Format: `bg_inode_bitmap`.
  pub const INODE_BITMAP: usize = 0x04;
  /// Format: `bg_inode_table`.
  pub const INODE_TABLE: usize = 0x08;
}

/// A structure under construction: its fields appended in order, little-endian, then padded to its length.
#[derive(Debug, Default)]
pub(crate) struct Fields {
  bytes: Vec<u8>,
}

impl Fields {
  /// An empty structure with room for `capacity` bytes.
  pub(crate) fn with_capacity(capacity: usize) -> Fields {
    Fields {
      bytes: Vec::with_capacity(capacity),
    }
  }

  /// Appends a byte.
  pub(crate) fn u8(&mut self, value: u8) -> &mut Fields {
    self.bytes.push(value);
    self
  }

  /// Appends a 16-bit field.
  pub(crate) fn u16(&mut self, value: u16) -> &mut Fields {
    self.bytes.extend_from_slice(&value.to_le_bytes());
    self
  }

  /// Appends a 32-bit field.
  pub(crate) fn u32(&mut self, value: u32) -> &mut Fields {
    self.bytes.extend_from_slice(&value.to_le_bytes());
    self
  }

  /// Appends raw bytes.
  pub(crate) fn bytes(&mut self, value: &[u8]) -> &mut Fields {
    self.bytes.extend_from_slice(value);
    self
  }

  /// Appends `count` zero bytes.
  pub(crate) fn zeros(&mut self, count: usize) -> &mut Fields {
    self.bytes.resize(self.bytes.len().saturating_add(count), 0);
    self
  }

  /// The bytes appended so far.
  pub(crate) fn len(&self) -> usize {
    self.bytes.len()
  }

  /// The structure, zero-padded (or cut) to exactly `length` bytes.
  pub(crate) fn finish(mut self, length: usize) -> Vec<u8> {
    self.bytes.resize(length, 0);
    self.bytes
  }
}

/// The 16-bit little-endian field at `at` in `bytes`, if it lies within them.
pub(crate) fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
  let field = bytes.get(at..at.checked_add(size_of::<u16>())?)?;
  Some(u16::from_le_bytes(field.try_into().ok()?))
}

/// The 32-bit little-endian field at `at` in `bytes`, if it lies within them.
pub(crate) fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
  let field = bytes.get(at..at.checked_add(size_of::<u32>())?)?;
  Some(u32::from_le_bytes(field.try_into().ok()?))
}

/// The byte at `at` in `bytes`, if it lies within them.
pub(crate) fn u8_at(bytes: &[u8], at: usize) -> Option<u8> {
  bytes.get(at).copied()
}
