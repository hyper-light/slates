//! Inodes: number, generation, birth epoch, POSIX attributes with nanosecond timestamps, and the
//! body (§4.5). An inode is copy-on-write like a node: a version born in the current epoch is
//! mutated in place; an older one is copied and the number's table entry re-pointed, so a
//! snapshot keeps the old version and the number never changes (AC-1.6).

use slates_mem::Handle;

use crate::content::{Extent, OpenExtent};
use crate::dir::DirNode;
use crate::ids::{Epoch, InodeNo};

/// What an inode is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
  /// A regular file.
  File,
  /// A directory.
  Dir,
  /// A symbolic link.
  Symlink,
}

/// POSIX attributes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attrs {
  /// Permission bits.
  pub mode: u32,
  /// Owner.
  pub uid: u32,
  /// Group.
  pub gid: u32,
  /// Hard links (2 + subdirectories for a directory).
  pub nlink: u32,
  /// Size in bytes (the file length; a directory reports 0).
  pub size: u64,
  /// Access time, ns since the Unix epoch.
  pub atime: i128,
  /// Modification time.
  pub mtime: i128,
  /// Change time.
  pub ctime: i128,
  /// Birth time.
  pub btime: i128,
}

/// The base-plane fields of a base-backed body (§4.5 `Body::Base`), filled in by `slates-base`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BaseBody {
  /// The witnessed fingerprint and identity, once copied up.
  pub witness: Option<Witness>,
  /// Ranges pinned into the arena (whole file for the small class; written ranges for the large).
  pub pinned: Vec<Extent>,
  /// Length as the base holds it (or held it at the witness).
  pub base_len: u64,
  /// The base plane's descriptor token for the file, if one is held.
  pub descriptor: Option<u64>,
  /// Set when a drift check failed: reads of unpinned ranges refuse with `BaseDrift`.
  pub lost: bool,
}

/// A witnessed base: the fingerprint and content identity the agent's edit was based on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Witness {
  /// The stat fingerprint at the witness.
  pub fingerprint: Fingerprint,
  /// BLAKE3 of the bytes at the witness.
  pub identity: [u8; 32],
  /// When witnessed, monotonic ns.
  pub witnessed_at: u64,
  /// Whether the fingerprint was inside the racy window and the identity was taken by hashing.
  pub racy: bool,
}

/// A stat fingerprint (§4.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Fingerprint {
  /// Device.
  pub dev: u64,
  /// Inode on the base.
  pub ino: u64,
  /// Size.
  pub size: u64,
  /// Modification time, ns.
  pub mtime_ns: i128,
  /// Change time, ns.
  pub ctime_ns: i128,
  /// Mode.
  pub mode: u32,
}

/// The body.
#[derive(Clone, Debug)]
pub enum Body {
  /// No content yet (a fresh file, or a body moved out during an update).
  None,
  /// A directory: the current directory node of this inode in the volume that owns the inode
  /// record. The record is copied whenever the node is, so the volume's inode table always
  /// names the head's node and a parent link (an inode number) resolves through it.
  Directory(Handle<DirNode>),
  /// Small content kept inline.
  Inline(Vec<u8>),
  /// Sealed extents, non-overlapping, ascending by offset.
  Sealed(Vec<Extent>),
  /// An open mutable extent over sealed extents.
  Open {
    /// The open extent.
    open: OpenExtent,
    /// Sealed extents beneath it.
    sealed: Vec<Extent>,
  },
  /// A symlink target.
  Symlink(Box<str>),
  /// A base-backed entry.
  Base(BaseBody),
}

/// An inode version.
#[derive(Clone, Debug)]
pub struct Inode {
  /// The number.
  pub no: InodeNo,
  /// The generation, bumped when the number's slot is reused (never within a volume).
  pub generation: u32,
  /// The birth epoch of this version.
  pub born: Epoch,
  /// The kind.
  pub kind: Kind,
  /// Attributes.
  pub attrs: Attrs,
  /// The body.
  pub body: Body,
  /// The per-inode version counter the journal records (`prev_version`).
  pub version: u64,
  /// Where a file or symlink hangs: its parent directory and the hash of its name there, so
  /// the deriver finds its path without walking the tree (§4.16). Kept current by create,
  /// link and rename; `None` for directories and the root.
  pub home: Option<Home>,
  /// Whether the inode has ever had more than one link since it was made; then `home` names
  /// one of its paths and the deriver walks for the rest.
  pub multi: bool,
}

/// A file's place in the namespace: the parent directory's inode number and the hash of the
/// entry's folded name in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Home {
  /// The parent directory.
  pub parent: InodeNo,
  /// The hash of the entry's name under the volume's policy.
  pub hash: u64,
}

impl Inode {
  /// A new inode with zero timestamps (the volume stamps them).
  pub fn new(no: InodeNo, born: Epoch, kind: Kind, mode: u32, body: Body) -> Self {
    Self {
      no,
      generation: 0,
      born,
      kind,
      attrs: Attrs {
        mode,
        uid: 0,
        gid: 0,
        nlink: 1,
        size: 0,
        atime: 0,
        mtime: 0,
        ctime: 0,
        btime: 0,
      },
      body,
      version: 0,
      home: None,
      multi: false,
    }
  }
}
