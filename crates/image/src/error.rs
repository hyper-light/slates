//! Why an image could not be written or read: the crate's closed refusal taxonomy (CLAUDE.md, "errors, not panics").
//! A refusal of the tree or the capacity comes before a byte is written; a refusal of an image read back names what in
//! it was out of range, so a hostile or damaged image is refused by kind and never read past.

/// Why an image could not be written, or one read back could not be trusted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageRefusal {
  /// The tree's root is not a directory.
  RootNotDirectory,
  /// A name is empty, longer than a directory entry holds, `.` or `..`, or contains `/` or a NUL byte.
  NameInvalid {
    /// The name's length in bytes.
    len: usize,
  },
  /// Two entries of one directory have the same name.
  NameRepeated,
  /// An entry names a parent that is not a directory.
  ParentNotDirectory,
  /// A node the tree does not hold was named.
  NodeUnknown {
    /// The node's index.
    node: usize,
  },
  /// A directory is named by more than one entry, or by none though it is not the root: a directory has exactly one
  /// parent, and a tree with a cycle has a directory named twice.
  DirectoryLinkedTwice {
    /// The directory's node index (writing) or inode number (reading).
    node: u64,
  },
  /// A node no entry reaches from the root.
  Unreachable {
    /// The node's index.
    node: usize,
  },
  /// A node carries more links than the format allows ([`crate::format::LINK_MAX`]).
  TooManyLinks {
    /// The links it would carry.
    links: u64,
  },
  /// A symbolic link's target does not fit one block, or is empty.
  SymlinkInvalid {
    /// The target's length in bytes.
    len: usize,
  },
  /// A file is larger than an inode of this format can map or count.
  FileTooLarge {
    /// The file's size in bytes.
    size: u64,
  },
  /// A time lies outside the range the format's 34-bit seconds hold (1901-12-13 to 2446-05-10).
  TimeOutOfRange {
    /// The time's seconds since the Unix epoch.
    secs: i64,
  },
  /// The capacity holds fewer blocks than the tree and the file system's own structures need.
  TooSmall {
    /// The blocks needed.
    need: u64,
    /// The blocks the capacity holds.
    have: u64,
  },
  /// The tree needs more inodes than the capacity's groups can hold.
  TooManyInodes {
    /// The inodes needed.
    need: u64,
    /// The most the groups hold.
    most: u64,
  },
  /// The capacity is past what 32-bit block numbers address.
  CapacityTooLarge {
    /// The blocks asked for.
    blocks: u64,
  },
  /// The content source returned fewer bytes than a file's size promised.
  ContentShort {
    /// The file's node index.
    node: usize,
    /// The offset at which it ended.
    at: u64,
  },
  /// The image is shorter than a structure it names.
  Truncated {
    /// The bytes needed.
    need: u64,
    /// The image's length.
    have: u64,
  },
  /// The superblock's magic is not ext2's.
  BadMagic {
    /// The magic present.
    found: u16,
  },
  /// A superblock field this reader requires to be what the writer wrote is not: the revision, the block size, the
  /// inode size, the first inode, the group geometry.
  Unsupported {
    /// Which field, in words.
    field: &'static str,
    /// Its value.
    value: u64,
  },
  /// The image uses a feature this reader does not read.
  FeaturesUnsupported {
    /// The incompatible features present beyond the known ones.
    incompat: u32,
    /// The read-only-compatible features present beyond the known ones.
    ro_compat: u32,
  },
  /// The file system was not cleanly unmounted, so its structures may be half-written.
  NotClean,
  /// The counts of the superblock and the group descriptors do not agree with the image's length or each other.
  Geometry {
    /// Which count, in words.
    reason: &'static str,
  },
  /// An inode number out of the file system's range.
  InodeOutOfRange {
    /// The inode number.
    ino: u64,
  },
  /// A block number out of the file system's range.
  BlockOutOfRange {
    /// The block number.
    block: u64,
  },
  /// An inode this reader cannot read: an unknown type, extents, inline data, an extra-fields size out of range.
  InodeUnsupported {
    /// The inode number.
    ino: u32,
    /// Why, in words.
    reason: &'static str,
  },
  /// A directory block whose entries do not parse: a record length out of range, a name past its record.
  DirectoryCorrupt {
    /// The directory's inode number.
    ino: u32,
    /// Why, in words.
    reason: &'static str,
  },
  /// The image names more entries than the caller allowed a tree to hold.
  TreeTooLarge {
    /// The entries allowed.
    most: usize,
  },
}

/// A step of writing or reading that failed: refused by this crate, or by the caller's source or sink.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failed<E> {
  /// This crate refused it.
  Image(ImageRefusal),
  /// The caller's content source, image sink or image source failed.
  Source(E),
}

impl<E> From<ImageRefusal> for Failed<E> {
  fn from(refusal: ImageRefusal) -> Failed<E> {
    Failed::Image(refusal)
  }
}
