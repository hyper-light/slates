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
  /// Set when a drift check failed: reads of unpinned ranges refuse with `BaseDrift`.
  pub lost: bool,
  /// The disk file the inode was made from (A-151), recorded when the listing named it and never changed after: a name
  /// the disk gives another file becomes a new entry, and a descriptor opened by name must find this file.
  pub file: FileTag,
}

/// Which disk file an untouched base inode was made from (A-151; §4.5): its device, inode number and birth time
/// ([`Fingerprint::same_object`]'s three fields), digested to 56 bits. The birth time is what tells a file from one that took its number since: ext4 and XFS hand a
/// freed inode number to the next file, so two editor saves in a row leave the name at the first file's number with
/// other bytes. The tag sits in the seven bytes that alignment left unused beside [`BaseBody::lost`], so no inode grows
/// (AC-1.5; `Inode` stays 168 bytes, `cargo run -p slates-vfs --example tree_heap`). Two different files at one name
/// share a tag with odds of about one in 2^56.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FileTag([u8; FILE_TAG_BYTES]);

/// Format: the bytes a [`FileTag`] keeps, the seven an inode's base body had as padding beside `lost`.
pub const FILE_TAG_BYTES: usize = 7;
/// Format: splitmix64's first multiplier (Steele, Lea and Flood, "Fast Splittable Pseudorandom Number Generators",
/// OOPSLA 2014; Vigna's reference `splitmix64.c`).
const SPLITMIX_FIRST: u64 = 0xbf58_476d_1ce4_e5b9;
/// Format: splitmix64's second multiplier (the same sources).
const SPLITMIX_SECOND: u64 = 0x94d0_49bb_1331_11eb;

/// The splitmix64 finalizer: a bijection on 64-bit words that spreads every input bit across the output.
const fn splitmix(word: u64) -> u64 {
  let word = (word ^ (word >> 30)).wrapping_mul(SPLITMIX_FIRST);
  let word = (word ^ (word >> 27)).wrapping_mul(SPLITMIX_SECOND);
  word ^ (word >> 31)
}

impl FileTag {
  /// The tag of the file `fingerprint` describes: its device, inode number and birth time, each folded through the
  /// splitmix64 finalizer and the result cut to 56 bits. A filesystem that records no birth time gives zero, so its tag
  /// is the device and number alone ([`Fingerprint::birth_ns`]).
  pub fn of(fingerprint: &Fingerprint) -> FileTag {
    let born = u64::from_le_bytes(fingerprint.birth_ns.to_le_bytes());
    let mixed = splitmix(splitmix(splitmix(fingerprint.dev) ^ fingerprint.ino) ^ born);
    let [a, b, c, d, e, f, g, _] = mixed.to_le_bytes();
    FileTag([a, b, c, d, e, f, g])
  }

  /// The tag as a word, as an image carries it: the 56 bits in the low bytes, the top byte zero.
  pub fn word(self) -> u64 {
    let [a, b, c, d, e, f, g] = self.0;
    u64::from_le_bytes([a, b, c, d, e, f, g, 0])
  }

  /// The tag `word` holds ([`FileTag::word`]); a top byte is ignored.
  pub fn from_word(word: u64) -> FileTag {
    let [a, b, c, d, e, f, g, _] = word.to_le_bytes();
    FileTag([a, b, c, d, e, f, g])
  }
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
  /// The file's birth time, ns, as the filesystem records it (`statx`'s `stx_btime` on Linux, `st_birthtime` on macOS,
  /// the creation time on Windows), zero where it records none. A rename keeps it and a replacement does not, so with
  /// the device and inode number it tells a file from one that took its number since (A-151, [`FileTag`]). A host
  /// reports the same value for a file whether it lists it or opens it.
  pub birth_ns: i64,
}

impl Fingerprint {
  /// Whether `other` describes the same disk object as this one: the same device, inode number and birth time (A-151,
  /// A-153). A change to a file's bytes or a directory's entries moves its times and size but not these, so a directory
  /// whose entries changed is still the same source, and one another directory replaced is not. On a filesystem that
  /// records no birth time (both zero) the device and inode number alone decide. [`FileTag`] is a digest of the same
  /// three fields.
  pub fn same_object(&self, other: &Fingerprint) -> bool {
    self.dev == other.dev && self.ino == other.ino && self.birth_ns == other.birth_ns
  }
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
  /// An untouched base symlink (§4.5): its target as the disk held it when its directory was listed. The symlink
  /// counterpart of [`Body::Base`]: in no diverged set, and leaving a merged directory when the disk no longer lists
  /// it. A copy-up makes it a plain [`Body::Symlink`]. Before 2026-10-08 a base symlink was a plain symlink, so a
  /// landing planned every one of them as a change and a fresh listing dropped every symlink the volume created.
  BaseLink(Box<str>),
}

/// An inode's extended attributes (§4.5): each name and the attribute inode that holds its value,
/// sorted by name bytes so a lookup is a binary search and a listing is canonical. Names are byte
/// strings, as the hosts' are (Linux names are C strings in a namespace, `user.x`; macOS names are
/// UTF-8; an NFSv4 named attribute is a UTF-8 component), checked by [`crate::xattr::check_name`].
///
/// Sized to what it holds, because every file a macOS client creates carries one attribute
/// (`com.apple.provenance`): the entries are a boxed slice with exactly one slot per attribute (a
/// `Vec`'s first growth reserved four, 96 bytes for one), and the working copy is a non-zero word
/// (an inode number is never zero), so the table is 24 bytes, not 40 (2026-10-06, `create_heap`).
/// A set or a removal rebuilds the slice; an inode holds a handful of attributes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct XattrTable {
  /// `(name, attribute inode)`, ascending by name, names unique.
  entries: Box<[(Box<[u8]>, InodeNo)]>,
  /// A transport's working copy of an encoding of these attributes (the macOS AppleDouble `._`
  /// file a client writes over NFSv3, §4.6): an attribute inode holding the bytes exactly as the
  /// client last wrote them, so its reads are stable. Not an attribute; dropped whenever the
  /// attributes change through any other path. Read through [`XattrTable::sidecar`].
  sidecar: Option<std::num::NonZeroU64>,
}

impl XattrTable {
  /// The attribute inode holding `name`'s value, if the name is set.
  pub fn get(&self, name: &[u8]) -> Option<InodeNo> {
    self
      .entries
      .binary_search_by(|(held, _)| held.as_ref().cmp(name))
      .ok()
      .and_then(|at| self.entries.get(at))
      .map(|(_, no)| *no)
  }

  /// Sets `name` to the attribute inode `no`, returning the inode it replaces.
  pub fn insert(&mut self, name: &[u8], no: InodeNo) -> Option<InodeNo> {
    match self
      .entries
      .binary_search_by(|(held, _)| held.as_ref().cmp(name))
    {
      Ok(at) => self
        .entries
        .get_mut(at)
        .map(|(_, held)| std::mem::replace(held, no)),
      Err(at) => {
        let mut entries = Vec::with_capacity(self.entries.len().saturating_add(1));
        let mut held = std::mem::take(&mut self.entries).into_vec().into_iter();
        entries.extend(held.by_ref().take(at));
        entries.push((name.into(), no));
        entries.extend(held);
        self.entries = entries.into_boxed_slice();
        None
      }
    }
  }

  /// Removes `name`, returning the attribute inode that held its value.
  pub fn remove(&mut self, name: &[u8]) -> Option<InodeNo> {
    let at = self
      .entries
      .binary_search_by(|(held, _)| held.as_ref().cmp(name))
      .ok()?;
    let mut entries = std::mem::take(&mut self.entries).into_vec();
    let (_, removed) = entries.remove(at);
    self.entries = entries.into_boxed_slice();
    Some(removed)
  }

  /// The attribute inode holding the transport's working copy, if one is kept.
  pub fn sidecar(&self) -> Option<InodeNo> {
    self.sidecar.map(|no| InodeNo(no.get()))
  }

  /// Forgets the working copy, returning its attribute inode.
  pub fn take_sidecar(&mut self) -> Option<InodeNo> {
    self.sidecar.take().map(|no| InodeNo(no.get()))
  }

  /// Keeps `copy` as the working copy; an inode number of zero, which no inode has, keeps none.
  pub fn set_sidecar(&mut self, copy: InodeNo) {
    self.sidecar = std::num::NonZeroU64::new(copy.0);
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
    if pairs
      .windows(2)
      .any(|pair| matches!(pair, [a, b] if a.0 == b.0))
    {
      return None;
    }
    Some(Self {
      entries: pairs.into_boxed_slice(),
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
  /// its owner or when the attribute is removed or replaced. Kept as a non-zero word (an inode number is never zero:
  /// a volume's counter starts at 2), so `None` costs no tag: 8 bytes in every inode, not 16 ([`Inode::attribute_of`]).
  attribute_owner: Option<std::num::NonZeroU64>,
}

/// A file's place in the namespace: the parent directory's inode number and the hash of the
/// entry's folded name in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Home {
  /// The parent directory, as a non-zero word (an inode number is never zero), so `Option<Home>` costs no tag: 16 bytes
  /// in every inode, not 24 (AC-1.5, 2026-10-05). Read through [`Home::parent`].
  parent: std::num::NonZeroU64,
  /// The hash of the entry's name under the volume's policy.
  pub hash: u64,
}

impl Home {
  /// The place under directory `parent` of a name hashing to `hash`; `None` for parent number zero, which no inode has.
  pub fn new(parent: InodeNo, hash: u64) -> Option<Home> {
    std::num::NonZeroU64::new(parent.0).map(|parent| Home { parent, hash })
  }

  /// The parent directory.
  pub fn parent(&self) -> InodeNo {
    InodeNo(self.parent.get())
  }
}

impl Inode {
  /// For an attribute inode, the inode whose attribute it holds; `None` for a namespace inode.
  pub fn attribute_of(&self) -> Option<InodeNo> {
    self.attribute_owner.map(|owner| InodeNo(owner.get()))
  }

  /// Records which inode this attribute inode holds an attribute of (`None` for a namespace inode).
  pub fn set_attribute_of(&mut self, owner: Option<InodeNo>) {
    self.attribute_owner = owner.and_then(|owner| std::num::NonZeroU64::new(owner.0));
  }

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
      attribute_owner: None,
    }
  }

  /// Records a change to the object: its change time becomes `ctime` and its change counter moves.
  /// Every change goes through here, so the counter moves wherever the change time is stamped, even
  /// when the wall clock repeats or steps back (A-38).
  pub(crate) fn stamp_change(&mut self, ctime: i64) {
    self.attrs.ctime = ctime;
    self.version = self.version.saturating_add(1);
  }

  /// Moves the change counter by `by` with no change to the object's own attributes: its AppleDouble
  /// working copy was made or dropped. A view's counter is its owner's plus its working copy's, so
  /// the owner absorbs a dropped copy's counter (and one more) and a made copy's first step, and
  /// the sum never repeats (A-38).
  pub(crate) fn fold_counter(&mut self, by: u64) {
    self.version = self.version.saturating_add(by);
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
