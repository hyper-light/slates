//! Volume recovery images (§4.8, A-9): a faithful, handle-free image of a volume's durable state
//! — every inode with its number, generation, birth epoch, POSIX attributes, home and body; every
//! directory's entries by name and child number; every file's bytes; and the volume's roots
//! (prefix, name policy, epoch, inode counter, quota parameters). The image is canonical
//! [`slates_wire::Wire`] bytes, so a running daemon publishes it into anchor-owned RAM (the content
//! object) at a barrier and a restarted daemon rebuilds the volume from it, recovering the
//! acknowledged data §4.8 forbids losing: "Rebuilding a scratch volume from only a quota and id
//! loses acknowledged data." Keeping the content mapping alive is not enough — its object
//! references and committed roots must be recoverable too, which is exactly what this image holds.
//!
//! Why `Wire` and not the ad-hoc encoding of [`crate::derive`]: a recovery image is read back from
//! the content object after a possible mid-write crash, so it is external, possibly-corrupt input.
//! `Wire` checks every length against the remaining bytes before allocating and refuses a bad tag,
//! a truncated body or a non-canonical value, so a corrupt image is a typed [`VfsError`], never a
//! panic (the no-panic law) and never a silently-smaller volume ([`VfsError::RecoveryIncomplete`]).
//!
//! What this slice captures and what it does not: it captures a scratch volume in full (the case
//! §4.8 step two asks for — "create a scratch volume, write bytes, kill the daemon, restart, read
//! the same bytes"). It refuses, rather than silently drops, a base-backed body or a whiteout (the
//! base-plane recovery gate: "reopening a path alone cannot substitute another base"); CoW
//! snapshots, referenced-but-unlinked orphans and the live pressure source of a dynamic quota are
//! not yet in the image and are recorded as their own gates. The rebuild half (`from_image`) and
//! the daemon/content-object wiring follow in their own slices.

use std::collections::BTreeMap;

use slates_mem::Handle;
use slates_wire::Wire;

use crate::clock::Clock;
use crate::dir::{Child, DirNode};
use crate::error::VfsError;
use crate::ids::{Epoch, InodeNo};
use crate::inode::{Attrs, Body, Home, Inode, Kind};
use crate::names::NameEquivalence;
use crate::quota::Quota;
use crate::trie;
use crate::volume::{Store, Volume, VolumeSeed};

/// Format: the image's magic (`"SLR1"` little-endian), so an all-zero or foreign content object
/// decodes to a mismatch and is refused rather than read as a valid empty volume.
const IMAGE_MAGIC: u32 = u32::from_le_bytes(*b"SLR1");
/// Format: the image layout version, bumped with any change to the types below.
const IMAGE_VERSION: u16 = 1;

/// The name-equivalence policy in an image (§4.4 [`NameEquivalence`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Wire)]
pub enum PolicyImage {
  /// Bytes must match.
  Exact,
  /// Normalization- and case-insensitive.
  Fold,
}

/// The quota parameters in an image (§4.4 [`Quota`]). A dynamic quota's live pressure source is
/// not serialized — it is re-supplied on recovery exactly as the clock is — so only the counters
/// it maintains travel in the image.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Wire)]
pub enum QuotaImage {
  /// A fixed quota reserved at creation.
  Bounded {
    /// Bytes.
    limit: u64,
  },
  /// A quota that grew under a pressure source, with the counters at the barrier.
  Dynamic {
    /// The ceiling.
    max: u64,
    /// Bytes the source had granted.
    granted: u64,
    /// Growth requests the source had refused.
    denied: u64,
  },
}

/// What an inode is (§4.5 [`Kind`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Wire)]
pub enum KindImage {
  /// A regular file.
  File,
  /// A directory.
  Dir,
  /// A symbolic link.
  Symlink,
}

/// POSIX attributes in an image (§4.5 `Attrs`); every field is fixed-width, so the encoding is
/// identical on every platform (a determinism gate).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Wire)]
pub struct AttrsImage {
  /// Permission bits.
  pub mode: u32,
  /// Owner.
  pub uid: u32,
  /// Group.
  pub gid: u32,
  /// Hard links.
  pub nlink: u32,
  /// Size in bytes.
  pub size: u64,
  /// Access time, ns.
  pub atime: i64,
  /// Modification time, ns.
  pub mtime: i64,
  /// Change time, ns.
  pub ctime: i64,
  /// Birth time, ns.
  pub btime: i64,
}

/// A file's home in an image (§4.5 `Home`): the parent directory's inode number and the hash of
/// the entry's folded name there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Wire)]
pub struct HomeImage {
  /// The parent directory's inode number.
  pub parent: u64,
  /// The hash of the entry's folded name.
  pub hash: u64,
}

/// One directory entry: the name as created and the child's inode number (a subdirectory's number
/// is its node's own inode, so an entry never carries a position-dependent handle).
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
pub struct EntryImage {
  /// The entry name.
  pub name: String,
  /// The child's inode number.
  pub child: u64,
}

/// An inode's body reduced to its recoverable content (§4.5 `Body`): a directory becomes its
/// entries; a file becomes its bytes (whether they were inline, sealed in chunks or in an open
/// extent — the read path serves them the same); a symlink becomes its target. `Empty` is a file
/// with no content yet.
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
pub enum BodyImage {
  /// No content yet.
  Empty,
  /// A directory's entries, in the order the tree yields them.
  Directory {
    /// The entries.
    entries: Vec<EntryImage>,
  },
  /// A file's bytes.
  File {
    /// The bytes.
    bytes: Vec<u8>,
  },
  /// A symlink's target.
  Symlink {
    /// The target path.
    target: String,
  },
}

/// One inode in an image: its identity, attributes, home and body. The number is the volume-wide
/// inode number (prefix and counter), so a rebuild restores the same numbers and a file handle a
/// client held before the restart still resolves.
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
pub struct InodeImage {
  /// The inode number.
  pub no: u64,
  /// The generation.
  pub generation: u32,
  /// The birth epoch.
  pub born: u64,
  /// The kind.
  pub kind: KindImage,
  /// The attributes.
  pub attrs: AttrsImage,
  /// The per-inode version counter the journal records.
  pub version: u64,
  /// Where a file or symlink hangs, if it has a home.
  pub home: Option<HomeImage>,
  /// Whether the inode has ever had more than one link.
  pub multi: bool,
  /// The body.
  pub body: BodyImage,
}

/// A whole volume's recoverable state (§4.8, A-9). The inodes are in number order (the order the
/// inode table yields them), so two images of equal state are byte-identical (a determinism gate).
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
pub struct VolumeImage {
  /// The format magic; checked before anything else on decode.
  pub magic: u32,
  /// The format version.
  pub version: u16,
  /// The volume's inode-number prefix.
  pub prefix: u16,
  /// The name-equivalence policy.
  pub policy: PolicyImage,
  /// The head epoch.
  pub epoch: u64,
  /// The next inode counter to hand out.
  pub next_counter: u64,
  /// The origin epoch, for a clone; `None` for a scratch volume.
  pub origin_epoch: Option<u64>,
  /// The quota parameters.
  pub quota: QuotaImage,
  /// The root directory's inode number.
  pub root_no: u64,
  /// Every inode, in number order.
  pub inodes: Vec<InodeImage>,
}

impl VolumeImage {
  /// Decodes an image from content-object bytes, refusing a foreign magic, an unknown version,
  /// trailing bytes or any malformed field with [`VfsError::RecoveryIncomplete`] (§4.8: missing or
  /// unreadable state refuses, it never presents as an empty success).
  pub fn from_content(bytes: &[u8]) -> Result<VolumeImage, VfsError> {
    let image = VolumeImage::from_bytes(bytes).map_err(|_| VfsError::RecoveryIncomplete)?;
    if image.magic != IMAGE_MAGIC || image.version != IMAGE_VERSION {
      return Err(VfsError::RecoveryIncomplete);
    }
    Ok(image)
  }

  /// The canonical content-object bytes of this image.
  pub fn to_content(&self) -> Vec<u8> {
    self.to_bytes()
  }
}

/// The image of `kind`.
const fn kind_image(kind: Kind) -> KindImage {
  match kind {
    Kind::File => KindImage::File,
    Kind::Dir => KindImage::Dir,
    Kind::Symlink => KindImage::Symlink,
  }
}

/// The image of a name policy.
const fn policy_image(policy: NameEquivalence) -> PolicyImage {
  match policy {
    NameEquivalence::Exact => PolicyImage::Exact,
    NameEquivalence::Fold => PolicyImage::Fold,
  }
}

/// The image of a quota's parameters (the live pressure source is dropped; see [`QuotaImage`]).
fn quota_image(quota: &Quota) -> QuotaImage {
  match quota {
    Quota::Bounded { limit } => QuotaImage::Bounded { limit: *limit },
    Quota::Dynamic {
      max,
      granted,
      denied,
      ..
    } => QuotaImage::Dynamic {
      max: *max,
      granted: *granted,
      denied: *denied,
    },
  }
}

impl Volume {
  /// A faithful image of this volume's durable state for recovery (§4.8, A-9). Read-only: it walks
  /// the inode table in number order and, for each inode, captures its identity, attributes, home
  /// and body — a directory's entries by name and child number, a file's bytes through the read
  /// path, a symlink's target. It refuses with [`VfsError::RecoveryIncomplete`] a body this slice
  /// does not yet capture (a base-backed entry or a whiteout over one), so a base-backed volume is
  /// never imaged as if it were only its overlay (the base-plane recovery gate).
  pub fn to_image(&self, store: &Store) -> Result<VolumeImage, VfsError> {
    let mut handles = Vec::new();
    trie::walk(&store.tries, self.inode_root, &mut handles);
    let mut inodes = Vec::with_capacity(handles.len());
    for handle in handles {
      let inode = store.inodes.get(handle)?;
      inodes.push(self.image_of_inode(store, inode)?);
    }
    let root_no = store
      .dirs
      .get(self.root)
      .map_err(|_| VfsError::StaleHandle)?
      .inode;
    Ok(VolumeImage {
      magic: IMAGE_MAGIC,
      version: IMAGE_VERSION,
      prefix: self.prefix,
      policy: policy_image(self.policy),
      epoch: self.epoch.0,
      next_counter: self.next_counter,
      origin_epoch: self.origin_epoch.map(|e| e.0),
      quota: quota_image(&self.quota),
      root_no: root_no.0,
      inodes,
    })
  }

  /// The image of one inode, capturing its body faithfully or refusing an un-captured kind.
  fn image_of_inode(&self, store: &Store, inode: &Inode) -> Result<InodeImage, VfsError> {
    if matches!(inode.body, Body::Base(_)) {
      return Err(VfsError::RecoveryIncomplete);
    }
    let body = match inode.kind {
      Kind::Dir => BodyImage::Directory {
        entries: self.dir_entries(store, inode)?,
      },
      Kind::Symlink => match &inode.body {
        Body::Symlink(target) => BodyImage::Symlink {
          target: target.to_string(),
        },
        _ => return Err(VfsError::RecoveryIncomplete),
      },
      Kind::File => self.file_body(store, inode)?,
    };
    Ok(InodeImage {
      no: inode.no.0,
      generation: inode.generation,
      born: inode.born.0,
      kind: kind_image(inode.kind),
      attrs: AttrsImage {
        mode: inode.attrs.mode,
        uid: inode.attrs.uid,
        gid: inode.attrs.gid,
        nlink: inode.attrs.nlink,
        size: inode.attrs.size,
        atime: inode.attrs.atime,
        mtime: inode.attrs.mtime,
        ctime: inode.attrs.ctime,
        btime: inode.attrs.btime,
      },
      version: inode.version,
      home: inode.home.map(|h| HomeImage {
        parent: h.parent.0,
        hash: h.hash,
      }),
      multi: inode.multi,
      body,
    })
  }

  /// The entries of a directory inode, by name and child inode number. A subdirectory child's
  /// number is its node's own inode; a whiteout is a base overlay this slice does not capture.
  fn dir_entries(&self, store: &Store, inode: &Inode) -> Result<Vec<EntryImage>, VfsError> {
    let Body::Directory(node) = inode.body else {
      return Err(VfsError::RecoveryIncomplete);
    };
    let dir = store.dirs.get(node).map_err(|_| VfsError::StaleHandle)?;
    let mut entries = Vec::with_capacity(dir.len());
    for entry in dir.iter(&store.blocks) {
      let child = match entry.child {
        Child::File(no) | Child::Symlink(no) => no,
        Child::Dir(handle) => {
          store
            .dirs
            .get(handle)
            .map_err(|_| VfsError::StaleHandle)?
            .inode
        }
        Child::Whiteout => return Err(VfsError::RecoveryIncomplete),
      };
      entries.push(EntryImage {
        name: entry.name.to_string(),
        child: child.0,
      });
    }
    // Canonical order: by name. Names are distinct within a directory under the volume's policy, so
    // this total order is independent of the small/indexed representation the entries happened to be
    // stored in, which makes the image deterministic and a rebuild's re-capture byte-identical.
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
  }

  /// A file inode's bytes, read through the volume's own read path so inline, sealed and open
  /// bodies are all captured the same. An empty file (no content yet) images as `Empty`.
  fn file_body(&self, store: &Store, inode: &Inode) -> Result<BodyImage, VfsError> {
    let size = usize::try_from(inode.attrs.size).map_err(|_| VfsError::FileTooLarge)?;
    if size == 0 {
      return Ok(BodyImage::Empty);
    }
    let mut bytes = vec![0u8; size];
    let mut read = 0;
    while read < size {
      let at = u64::try_from(read).map_err(|_| VfsError::FileTooLarge)?;
      let got = self.read(store, inode.no, at, &mut bytes[read..])?;
      if got == 0 {
        break;
      }
      read += got;
    }
    bytes.truncate(read);
    Ok(BodyImage::File { bytes })
  }
}

/// The kind an image kind names.
const fn kind_from_image(kind: KindImage) -> Kind {
  match kind {
    KindImage::File => Kind::File,
    KindImage::Dir => Kind::Dir,
    KindImage::Symlink => Kind::Symlink,
  }
}

/// The name policy an image policy names.
const fn policy_from_image(policy: PolicyImage) -> NameEquivalence {
  match policy {
    PolicyImage::Exact => NameEquivalence::Exact,
    PolicyImage::Fold => NameEquivalence::Fold,
  }
}

/// The quota an image quota names. A dynamic quota is refused for now (its live pressure source is
/// not in the image and must be re-supplied by a future recovery path); a bounded quota rebuilds
/// exactly.
const fn quota_from_image(quota: QuotaImage) -> Result<Quota, VfsError> {
  match quota {
    QuotaImage::Bounded { limit } => Ok(Quota::Bounded { limit }),
    QuotaImage::Dynamic { .. } => Err(VfsError::RecoveryIncomplete),
  }
}

/// The attributes an image's attributes name.
const fn attrs_from_image(a: &AttrsImage) -> Attrs {
  Attrs {
    mode: a.mode,
    uid: a.uid,
    gid: a.gid,
    nlink: a.nlink,
    size: a.size,
    atime: a.atime,
    mtime: a.mtime,
    ctime: a.ctime,
    btime: a.btime,
  }
}

impl Volume {
  /// A volume rebuilt from a recovery image (§4.8, A-9), the other half of [`Volume::to_image`]. It
  /// is faithful: every inode is placed at its own number with its identity, attributes, home and
  /// body, so a client's file handle from before the restart still resolves; directories and their
  /// entries are rebuilt, and file bytes are re-established through the volume's own write path, so
  /// the arena, the chunk store and the quota accounting end in the same state a live volume would
  /// hold. `clock` and `journal_bytes` are re-supplied, as they are on any construction; a dynamic
  /// quota is refused for now (its live pressure source is not in the image).
  ///
  /// The rebuild runs entirely at the image's head epoch, so nothing copies-on-write while it is
  /// built; each inode's true birth epoch and version are restored at the end. It does not yet
  /// rebuild CoW snapshots, clone lineage, referenced-but-unlinked orphans or a base plane — those
  /// are their own gates — so it is exact for the scratch volume §4.8 step two asks for.
  pub fn from_image(
    store: &mut Store,
    image: &VolumeImage,
    clock: Box<dyn Clock>,
    journal_bytes: usize,
  ) -> Result<Volume, VfsError> {
    let policy = policy_from_image(image.policy);
    let quota = quota_from_image(image.quota)?;
    let epoch = Epoch(image.epoch);
    let root_no = InodeNo(image.root_no);
    let seed = VolumeSeed {
      prefix: image.prefix,
      policy,
      epoch,
      next_counter: image.next_counter,
      origin_epoch: image.origin_epoch.map(Epoch),
      quota,
    };
    let mut vol = Volume::recovery_shell(store, seed, clock, journal_bytes)?;

    let kinds: BTreeMap<u64, KindImage> = image.inodes.iter().map(|i| (i.no, i.kind)).collect();
    let mut dirs: BTreeMap<u64, Handle<DirNode>> = BTreeMap::new();
    dirs.insert(root_no.0, vol.root);

    vol.place_inodes(store, image, root_no, epoch, &mut dirs)?;
    vol.rebuild_entries(store, image, &kinds, &dirs)?;
    vol.fill_content(store, image)?;
    vol.restore_identities(store, image)?;
    Ok(vol)
  }

  /// Pass one: place every non-root inode at its own number, born at the head epoch, with a fresh
  /// directory node (parent and name fixed up when the parent's entries are rebuilt), a symlink's
  /// target, or an empty file body to be filled by the write path. The root already exists.
  fn place_inodes(
    &mut self,
    store: &mut Store,
    image: &VolumeImage,
    root_no: InodeNo,
    epoch: Epoch,
    dirs: &mut BTreeMap<u64, Handle<DirNode>>,
  ) -> Result<(), VfsError> {
    for image_inode in &image.inodes {
      let no = InodeNo(image_inode.no);
      if no == root_no {
        continue;
      }
      let body = body_for(store, image_inode, no, epoch, dirs)?;
      let inode = Inode::new(no, epoch, kind_from_image(image_inode.kind), 0, body);
      let handle = store.inodes.insert(inode)?;
      self.table_set(store, no, handle)?;
    }
    Ok(())
  }

  /// Pass two: rebuild every directory's entries, naming each child with the right kind, and fix
  /// each subdirectory node's parent and name from the entry that reaches it.
  fn rebuild_entries(
    &mut self,
    store: &mut Store,
    image: &VolumeImage,
    kinds: &BTreeMap<u64, KindImage>,
    dirs: &BTreeMap<u64, Handle<DirNode>>,
  ) -> Result<(), VfsError> {
    for image_inode in &image.inodes {
      let BodyImage::Directory { entries } = &image_inode.body else {
        continue;
      };
      let parent_no = InodeNo(image_inode.no);
      let parent = *dirs
        .get(&image_inode.no)
        .ok_or(VfsError::RecoveryIncomplete)?;
      for e in entries {
        let child = child_for(store, e, parent_no, kinds, dirs)?;
        self.dir_insert(store, parent, &e.name, child)?;
      }
    }
    Ok(())
  }

  /// Pass three: fill every non-empty file's content through the write path, so the chunk store and
  /// quota accounting end where a live write would leave them.
  fn fill_content(&mut self, store: &mut Store, image: &VolumeImage) -> Result<(), VfsError> {
    for image_inode in &image.inodes {
      if let BodyImage::File { bytes } = &image_inode.body
        && !bytes.is_empty()
      {
        self.write(store, InodeNo(image_inode.no), 0, bytes)?;
      }
    }
    Ok(())
  }

  /// Pass four: restore each inode's true identity — generation, birth epoch, version, multi-link
  /// flag, home and exact attributes — over the placeholders the earlier passes left (the write
  /// path stamped fresh times and sizes; here they become the image's).
  fn restore_identities(&mut self, store: &mut Store, image: &VolumeImage) -> Result<(), VfsError> {
    for image_inode in &image.inodes {
      let no = InodeNo(image_inode.no);
      let handle =
        trie::get(&store.tries, self.inode_root, no).ok_or(VfsError::RecoveryIncomplete)?;
      let inode = store.inodes.get_mut(handle)?;
      inode.generation = image_inode.generation;
      inode.born = Epoch(image_inode.born);
      inode.version = image_inode.version;
      inode.multi = image_inode.multi;
      inode.home = image_inode.home.map(|h| Home {
        parent: InodeNo(h.parent),
        hash: h.hash,
      });
      inode.attrs = attrs_from_image(&image_inode.attrs);
    }
    Ok(())
  }
}

/// The body to place an inode with in pass one: a fresh directory node (recorded in `dirs`), a
/// symlink's target, or an empty file body the write pass fills.
fn body_for(
  store: &mut Store,
  image_inode: &InodeImage,
  no: InodeNo,
  epoch: Epoch,
  dirs: &mut BTreeMap<u64, Handle<DirNode>>,
) -> Result<Body, VfsError> {
  match image_inode.kind {
    KindImage::Dir => {
      let node = store.dirs.insert(DirNode::new(epoch, None, no, ""))?;
      dirs.insert(image_inode.no, node);
      Ok(Body::Directory(node))
    }
    KindImage::Symlink => match &image_inode.body {
      BodyImage::Symlink { target } => Ok(Body::Symlink(target.as_str().into())),
      _ => Err(VfsError::RecoveryIncomplete),
    },
    KindImage::File => Ok(Body::Inline(Vec::new())),
  }
}

/// The child an entry names, resolving a subdirectory to its node handle and fixing that node's
/// parent and name from the reaching entry.
fn child_for(
  store: &mut Store,
  entry: &EntryImage,
  parent_no: InodeNo,
  kinds: &BTreeMap<u64, KindImage>,
  dirs: &BTreeMap<u64, Handle<DirNode>>,
) -> Result<Child, VfsError> {
  let child_no = InodeNo(entry.child);
  match kinds
    .get(&entry.child)
    .ok_or(VfsError::RecoveryIncomplete)?
  {
    KindImage::File => Ok(Child::File(child_no)),
    KindImage::Symlink => Ok(Child::Symlink(child_no)),
    KindImage::Dir => {
      let handle = *dirs.get(&entry.child).ok_or(VfsError::RecoveryIncomplete)?;
      let node = store.dirs.get_mut(handle)?;
      node.parent = Some(parent_no);
      node.name = entry.name.as_str().into();
      Ok(Child::Dir(handle))
    }
  }
}
