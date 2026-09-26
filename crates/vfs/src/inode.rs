//! Inodes: number, generation, birth epoch, POSIX attributes with nanosecond timestamps, the
//! body, and the extended-attribute table (§4.5). An inode is copy-on-write like a node: a version
//! born in the current epoch is mutated in place; an older one is copied and the number's table
//! entry re-pointed, so a snapshot keeps the old version and the number never changes (AC-1.6).
//!
//! Extended attributes (§4.5 "Extended attributes"): each attribute's value is the body of an
//! *attribute inode* — a file-bodied inode outside the namespace, named by the owner's
//! [`XattrTable`] — so a value is content like any file's: chunked, sealed, deduplicated, charged to
//! the quota, frozen by a snapshot at chunk granularity, and readable or writable at an offset (an
//! NFSv4 named attribute is a file, RFC 8881 §5.3). Copying an owner for a new epoch copies only its
//! table (names and numbers), never a value. The archive already records attribute values as chunks,
//! not manifest bytes (`slates-archive` `NodeMeta::xattr_flags`).

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
  /// A named pipe; its transient stream belongs to the mounting kernel (A-26).
  Fifo,
  /// A UNIX socket name; listeners and connections are not volume content (A-26).
  Socket,
}

impl Kind {
  /// Whether regular-file I/O must refuse this metadata-only IPC name (A-26).
  pub const fn is_special(self) -> bool {
    matches!(self, Self::Fifo | Self::Socket)
  }
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
  pub atime: i64,
  /// Modification time.
  pub mtime: i64,
  /// Change time.
  pub ctime: i64,
  /// Birth time.
  pub btime: i64,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, slates_wire::Wire)]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, slates_wire::Wire)]
pub struct Fingerprint {
  /// Device.
  pub dev: u64,
  /// Inode on the base.
  pub ino: u64,
  /// Size.
  pub size: u64,
  /// Modification time, ns.
  pub mtime_ns: i64,
  /// Change time, ns.
  pub ctime_ns: i64,
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

/// An inode's extended attributes (§4.5): each name and the attribute inode that holds its value,
/// sorted by name bytes so a lookup is a binary search and a listing is canonical. Names are byte
/// strings, as the hosts' are (Linux names are C strings in a namespace, `user.x`; macOS names are
/// UTF-8; an NFSv4 named attribute is a UTF-8 component), checked by [`crate::xattr::check_name`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct XattrTable {
  /// `(name, attribute inode)`, ascending by name, names unique.
  entries: Vec<(Box<[u8]>, InodeNo)>,
}

impl XattrTable {
  /// The attribute inode holding `name`'s value, if the name is set.
  pub fn get(&self, name: &[u8]) -> Option<InodeNo> {
    self
      .entries
      .binary_search_by(|(held, _)| held.as_ref().cmp(name))
      .ok()
      .map(|at| self.entries[at].1)
  }

  /// Sets `name` to the attribute inode `no`, returning the inode it replaces.
  pub fn insert(&mut self, name: &[u8], no: InodeNo) -> Option<InodeNo> {
    match self
      .entries
      .binary_search_by(|(held, _)| held.as_ref().cmp(name))
    {
      Ok(at) => Some(std::mem::replace(&mut self.entries[at].1, no)),
      Err(at) => {
        self.entries.insert(at, (name.into(), no));
        None
      }
    }
  }

  /// Removes `name`, returning the attribute inode that held its value.
  pub fn remove(&mut self, name: &[u8]) -> Option<InodeNo> {
    self
      .entries
      .binary_search_by(|(held, _)| held.as_ref().cmp(name))
      .ok()
      .map(|at| self.entries.remove(at).1)
  }

  /// The names and attribute inodes, ascending by name.
  pub fn iter(&self) -> impl Iterator<Item = (&[u8], InodeNo)> {
    self.entries.iter().map(|(name, no)| (name.as_ref(), *no))
  }

  /// How many attributes are set.
  pub fn len(&self) -> usize {
    self.entries.len()
  }

  /// Whether no attribute is set.
  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }

  /// A table from `(name, attribute inode)` pairs in any order (a recovery rebuild); `None` when a
  /// name repeats, which no volume produces.
  pub fn from_pairs(mut pairs: Vec<(Box<[u8]>, InodeNo)>) -> Option<Self> {
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    if pairs.windows(2).any(|pair| pair[0].0 == pair[1].0) {
      return None;
    }
    Some(Self { entries: pairs })
  }
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
  /// The extended attributes, `None` while the inode has none (most inodes: the table is never
  /// allocated for them).
  pub xattrs: Option<Box<XattrTable>>,
  /// For an attribute inode, the inode whose attribute it holds; `None` for every namespace inode.
  /// An attribute inode is in no directory, has one link (its owner's table) and is reclaimed with
  /// its owner or when the attribute is removed or replaced.
  pub attribute_of: Option<InodeNo>,
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
      xattrs: None,
      attribute_of: None,
    }
  }
}
