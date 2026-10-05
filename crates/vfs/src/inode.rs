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
//! table (names and numbers), never a value. A placed archive carries each value the same way, as
//! extents over chunks (`slates-archive` `Xattr`, format minor 3).

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
  /// The witnessed fingerprint and identity, once copied up; boxed, since most base entries are never copied up and an
  /// inline witness (104 B) made every inode pay for it (an `Inode` was 296 B, its `Body` 160; AC-1.5, 2026-10-05).
  pub witness: Option<Box<Witness>>,
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
  /// The owner's user id (`st_uid`): what an untouched base entry reports through a mount, as the disk holds it.
  pub uid: u32,
  /// The owner's group id (`st_gid`).
  pub gid: u32,
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
    /// The open extent, boxed: a body is open only while its file is being written (the idle sweep seals it within two
    /// ticks, A-99), so an inline 64 B extent made every inode pay for a transient state (AC-1.5, 2026-10-05).
    open: Box<OpenExtent>,
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
  /// A transport's working copy of an encoding of these attributes (the macOS AppleDouble `._`
  /// file a client writes over NFSv3, §4.6): an attribute inode holding the bytes exactly as the
  /// client last wrote them, so its reads are stable. Not an attribute; dropped whenever the
  /// attributes change through any other path.
  pub sidecar: Option<InodeNo>,
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

  /// Whether the table holds nothing at all: no attribute and no working copy, so it need not exist.
  pub fn is_vacant(&self) -> bool {
    self.entries.is_empty() && self.sidecar.is_none()
  }

  /// A table from `(name, attribute inode)` pairs in any order (a recovery rebuild); `None` when a
  /// name repeats, which no volume produces.
  pub fn from_pairs(mut pairs: Vec<(Box<[u8]>, InodeNo)>) -> Option<Self> {
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    if pairs.windows(2).any(|pair| pair[0].0 == pair[1].0) {
      return None;
    }
    Some(Self {
      entries: pairs,
      sidecar: None,
    })
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
  /// The per-inode change counter: it moves on every change to the object — every stamp of
  /// `ctime`, through [`Inode::stamp_change`] — whatever the wall clock reads, so a transport serves
  /// it as a change attribute that never repeats (NFSv4 `change`, A-38). The journal records it
  /// (`prev_version`).
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

  /// Records a change to the object: its change time becomes `ctime` and its change counter moves.
  /// Every change goes through here, so the counter moves wherever the change time is stamped, even
  /// when the wall clock repeats or steps back (A-38).
  pub(crate) fn stamp_change(&mut self, ctime: i64) {
    self.attrs.ctime = ctime;
    self.version += 1;
  }

  /// Moves the change counter by `by` with no change to the object's own attributes: its AppleDouble
  /// working copy was made or dropped. A view's counter is its owner's plus its working copy's, so
  /// the owner absorbs a dropped copy's counter (and one more) and a made copy's first step, and
  /// the sum never repeats (A-38).
  pub(crate) fn fold_counter(&mut self, by: u64) {
    self.version += by;
  }

  /// Takes attributes observed on the host beneath an overlay (an outsider's edit, §4.5): the size,
  /// the mode and owner when the observation carries them, and the host's times. The change counter
  /// moves when any of them differs from what the volume held, so a client caching by the counter
  /// sees the edit, and stays where it is when none does, so an unchanged file's cache is kept (A-38).
  pub(crate) fn adopt_observed(
    &mut self,
    size: u64,
    mode_and_owner: Option<(u32, u32, u32)>,
    mtime: i64,
    ctime: i64,
  ) {
    let changed = self.attrs.size != size
      || mode_and_owner.is_some_and(|(mode, uid, gid)| {
        (mode, uid, gid) != (self.attrs.mode, self.attrs.uid, self.attrs.gid)
      })
      || self.attrs.mtime != mtime
      || self.attrs.ctime != ctime;
    if !changed {
      return;
    }
    self.attrs.size = size;
    if let Some((mode, uid, gid)) = mode_and_owner {
      self.attrs.mode = mode;
      self.attrs.uid = uid;
      self.attrs.gid = gid;
    }
    self.attrs.mtime = mtime;
    self.stamp_change(ctime);
  }
}
