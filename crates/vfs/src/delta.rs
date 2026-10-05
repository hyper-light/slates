//! Incremental recovery images (A-68; §4.8, D-18): what changed in a volume since it was last published, as a
//! delta a barrier publishes instead of the whole volume.
//!
//! **Why.** A barrier (every NFS `COMMIT`, FUSE `fsync` and the flush every `close` sends) published the shard's
//! whole image: every inode of every volume re-imaged and re-encoded (`Volume::to_image`). Its cost grew with
//! the volume, so a workload's total grew with its square. Measured 2026-10-04 through a real kernel mount
//! (`docs/wip/BENCHMARKS.md`, "The VFS on a busy machine"): 2,000 small creates took 4.9 s into an empty volume
//! and 50.2 s into one holding 12,000 files, and a sampled daemon spent its time in `publish_shard` →
//! `to_image` → `image_of_inode`. A-64 had removed the content bytes from the image; the metadata was never
//! diffed.
//!
//! **What.** A volume records the inodes and directory entries it changes ([`Dirty`]) at the chokepoints every
//! mutation passes: `make_current_inode` (copy-on-write requires every write to make its inode current first),
//! the inode table's set and remove, and the four entry helpers (place, unplace, set, remove). A delta
//! ([`VolumeDelta`]) carries the volume's small roots in full, the changed inodes' images (a directory's without
//! its entries), the removed inode numbers, and each changed entry by name. [`VolumeImage::apply`] is a pure
//! function: the previous image with a delta applied equals the volume's full image now — the oracle the tests
//! hold every generated history to, and the path recovery replays, so the rebuild (`Volume::from_image`) is
//! unchanged. This is the shape of a filesystem intent log (ZFS's ZIL records the changed blocks of a
//! synchronous write instead of committing a whole transaction group; JBD2 journals changed metadata blocks;
//! Rosenblum and Ousterhout's log-structured filesystem writes only what changed and checkpoints).
//!
//! **When a delta is not taken.** A volume with snapshots, a clone origin or a base plane publishes in full: a
//! head change can move a snapshot's shared file into the snapshot's own image, and the base plane images through
//! the host; their deltas are owed (GAPS). So is a volume's first publication, and one whose dirty set has grown
//! to its live inode count, where a full image is no larger than the delta would be (the bound that keeps the
//! dirty set from growing past the volume).

use std::collections::BTreeSet;

use slates_wire::Wire;

use crate::error::VfsError;
use crate::recover::{
  AttachmentReferences, BodyImage, EntryImage, InodeImage, InodeReferences, QuotaImage,
  SnapshotRef, VolumeImage, entry_order,
};
use crate::volume::{Store, Volume};

/// What a volume changed since it was last published.
#[derive(Clone, Debug, Default)]
pub struct Dirty {
  /// Inode numbers created, written or removed.
  inodes: BTreeSet<u64>,
  /// Directory entries placed, re-pointed or removed: the directory's inode number and the name as the
  /// operation named it.
  entries: BTreeSet<(u64, String)>,
  /// The recorded attachments' reference counts taken, forgotten or swept: the attachment's id and the inode number
  /// (A-96). An owner this process alone knows is not recorded, as the image does not carry it.
  references: BTreeSet<(u64, u64)>,
  /// Whether anything changed since the last publication, recorded or not: what lets a volume that records no
  /// details (its next publication is full) still answer clean when nothing changed.
  changed: bool,
  /// Whether the last publication of this volume committed, so a delta has an image to apply to.
  published: bool,
  /// The volume's shape when it was last published: a delta carries only inodes and entries, so a volume whose
  /// snapshots, clone origin or base plane differ from what was published — or that has any — publishes in full.
  published_shape: Shape,
  /// The volume's roots when it was last published: a resize or an attachment changes them without touching an
  /// inode, so a volume whose roots moved is not clean.
  published_roots: Option<Roots>,
}

/// A volume's small roots, carried in full by every delta.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Roots {
  epoch: u64,
  next_counter: u64,
  quota: QuotaImage,
  root_no: u64,
  last_snapshot: Option<SnapshotRef>,
  orphans: Vec<u64>,
}

/// The parts of a volume a delta does not carry (the module doc's "When a delta is not taken").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Shape {
  snapshots: usize,
  clone: bool,
  base: bool,
}

impl Dirty {
  /// Whether a change is worth recording: only while the next publication may be a delta. Before a volume's first
  /// committed publication, and while its published shape has snapshots, a clone origin or a base plane, the next one
  /// is its full image whatever changed, so recording would hold a name and a number per change for nothing (measured
  /// 2026-10-05: 134 heap bytes a file, a million files created into a never-published volume, 443 → 578 MB).
  fn recording(&self) -> bool {
    self.published && self.published_shape == Shape::default()
  }

  /// Records a changed inode.
  pub(crate) fn inode(&mut self, no: u64) {
    self.changed = true;
    if self.recording() {
      self.inodes.insert(no);
    }
  }

  /// Records a changed directory entry.
  pub(crate) fn entry(&mut self, dir: u64, name: &str) {
    self.changed = true;
    if self.recording() {
      self.entries.insert((dir, name.to_owned()));
    }
  }

  /// Records a changed reference count of `owner` on inode `no`; an owner this process alone knows is not imaged, so
  /// its changes are not recorded.
  pub(crate) fn reference(&mut self, owner: crate::ids::RefOwner, no: crate::ids::InodeNo) {
    if matches!(owner, crate::ids::RefOwner::Attachment(_)) {
      self.changed = true;
    }
    if !self.recording() {
      return;
    }
    if let crate::ids::RefOwner::Attachment(attachment) = owner {
      self.references.insert((attachment, no.0));
    }
  }
}

/// One recorded attachment's reference count on one inode as a delta carries it (A-96): the count now, zero when the
/// attachment holds none.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Wire)]
pub struct ReferenceChange {
  /// The attachment's durable id.
  pub attachment: u64,
  /// The inode number.
  pub inode: u64,
  /// The references the attachment holds on the inode now.
  pub count: u32,
}

/// One directory entry as a delta carries it: present (`entry`, a whiteout's child `None`) or gone.
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
pub struct EntryChange {
  /// The directory's inode number.
  pub dir: u64,
  /// The name the change was made under (matched under the volume's name policy when applied).
  pub name: String,
  /// The entry now, or `None` when the name is absent.
  pub entry: Option<EntryImage>,
}

/// What changed in a volume since its last publication (the module doc).
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
pub struct VolumeDelta {
  /// The head epoch now.
  pub epoch: u64,
  /// The next inode counter now.
  pub next_counter: u64,
  /// The quota parameters now.
  pub quota: QuotaImage,
  /// The root directory's inode number now.
  pub root_no: u64,
  /// The most recent snapshot now.
  pub last_snapshot: Option<SnapshotRef>,
  /// The orphans now.
  pub orphans: Vec<u64>,
  /// The recorded attachments' reference counts that changed, by attachment then inode (A-96: every attachment's
  /// whole list made a FUSE mount's delta grow with every inode its kernel had seen).
  pub references: Vec<ReferenceChange>,
  /// The changed inodes still present, in number order; a directory's body carries its own fields but no entries
  /// (they come from `entries` over what the previous image held).
  pub inodes: Vec<InodeImage>,
  /// The inode numbers removed, ascending.
  pub removed: Vec<u64>,
  /// The changed entries, by directory then name.
  pub entries: Vec<EntryChange>,
}

/// A volume's publication: its whole image, or a delta over its last one.
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
pub enum VolumeRecord {
  /// The whole image.
  Full {
    /// The image.
    image: VolumeImage,
  },
  /// The changes since the last publication.
  Delta {
    /// The changes.
    delta: VolumeDelta,
  },
}

impl Volume {
  /// This volume's publication now (the module doc): a delta when one applies, else its full image. Read-only:
  /// the changes stay recorded until [`Volume::mark_published`] is told the publication committed.
  pub fn publication(
    &self,
    store: &Store,
    host: Option<&mut dyn crate::host::HostFs>,
  ) -> Result<VolumeRecord, VfsError> {
    let changed = u64::try_from(self.dirty.inodes.len()).unwrap_or(u64::MAX);
    let shape = self.shape();
    let full = !self.dirty.published
      || shape != Shape::default()
      || self.dirty.published_shape != shape
      || changed >= self.live_inodes;
    if full {
      return Ok(VolumeRecord::Full {
        image: self.to_image(store, host)?,
      });
    }
    Ok(VolumeRecord::Delta {
      delta: self.delta(store)?,
    })
  }

  /// Whether nothing changed since this volume's last committed publication: a barrier need not record it.
  pub fn is_clean(&self, store: &Store) -> bool {
    self.dirty.published
      && !self.dirty.changed
      && self.dirty.inodes.is_empty()
      && self.dirty.entries.is_empty()
      && self.dirty.references.is_empty()
      && self.dirty.published_shape == self.shape()
      && self.dirty.published_roots.is_some()
      && self.dirty.published_roots == self.roots(store).ok()
  }

  /// Records that this volume's last [`Volume::publication`] was published and committed: the changes it carried are
  /// forgotten, and the next record may be a delta over it.
  pub fn mark_published(&mut self, store: &Store) {
    self.dirty.inodes.clear();
    self.dirty.entries.clear();
    self.dirty.references.clear();
    self.dirty.changed = false;
    self.dirty.published = true;
    self.dirty.published_shape = self.shape();
    self.dirty.published_roots = self.roots(store).ok();
  }

  /// This volume's roots now.
  fn roots(&self, store: &Store) -> Result<Roots, VfsError> {
    let root_no = store
      .dirs
      .get(self.root)
      .map_err(|_| VfsError::StaleHandle)?
      .inode;
    Ok(Roots {
      epoch: self.epoch.0,
      next_counter: self.next_counter,
      quota: crate::recover::quota_image(&self.quota),
      root_no: root_no.0,
      last_snapshot: self.last_snapshot.map(crate::recover::snap_ref),
      orphans: self.orphans.keys().map(|no| no.0).collect(),
    })
  }

  fn shape(&self) -> Shape {
    Shape {
      snapshots: self.snapshots.len(),
      clone: self.origin_epoch.is_some(),
      base: self.base.is_some(),
    }
  }

  /// Forgets that this volume was ever published, so its next record is its full image (a restarted owner, or a
  /// publication abandoned after this volume's record was taken).
  pub fn mark_unpublished(&mut self) {
    self.dirty.published = false;
  }

  fn delta(&self, store: &Store) -> Result<VolumeDelta, VfsError> {
    let roots = self.roots(store)?;
    let mut inodes = Vec::new();
    let mut removed = Vec::new();
    for no in &self.dirty.inodes {
      match crate::trie::get(&store.tries, self.inode_root, crate::ids::InodeNo(*no)) {
        Some(handle) => {
          let inode = store.inodes.get(handle)?;
          inodes.push(self.image_of_inode_without_entries(store, inode)?);
        }
        None => removed.push(*no),
      }
    }
    let mut entries = Vec::with_capacity(self.dirty.entries.len());
    for (dir, name) in &self.dirty.entries {
      entries.push(EntryChange {
        dir: *dir,
        name: name.clone(),
        entry: self.entry_image(store, crate::ids::InodeNo(*dir), name)?,
      });
    }
    let references = self
      .dirty
      .references
      .iter()
      .map(|(attachment, inode)| ReferenceChange {
        attachment: *attachment,
        inode: *inode,
        count: self.attachment_reference_count(*attachment, crate::ids::InodeNo(*inode)),
      })
      .collect();
    Ok(VolumeDelta {
      epoch: roots.epoch,
      next_counter: roots.next_counter,
      quota: roots.quota,
      root_no: roots.root_no,
      last_snapshot: roots.last_snapshot,
      orphans: roots.orphans,
      references,
      inodes,
      removed,
      entries,
    })
  }
}

impl Volume {
  /// The entry `name` in directory `dir` now, as an image carries it; `None` when the name is absent or the
  /// directory is gone.
  fn entry_image(
    &self,
    store: &Store,
    dir: crate::ids::InodeNo,
    name: &str,
  ) -> Result<Option<EntryImage>, VfsError> {
    let Some(handle) = crate::trie::get(&store.tries, self.inode_root, dir) else {
      return Ok(None);
    };
    let crate::inode::Body::Directory(node) = store.inodes.get(handle)?.body else {
      return Ok(None);
    };
    let node = store.dirs.get(node).map_err(|_| VfsError::StaleHandle)?;
    let Some(entry) = node.lookup(&store.blocks, self.policy, name) else {
      return Ok(None);
    };
    let child = match entry.child {
      crate::dir::Child::File(no)
      | crate::dir::Child::Symlink(no)
      | crate::dir::Child::Fifo(no)
      | crate::dir::Child::Socket(no) => Some(no.0),
      crate::dir::Child::Dir(handle) => Some(
        store
          .dirs
          .get(handle)
          .map_err(|_| VfsError::StaleHandle)?
          .inode
          .0,
      ),
      crate::dir::Child::Whiteout => None,
    };
    Ok(Some(EntryImage {
      name: entry.name.to_string(),
      child,
    }))
  }
}

impl VolumeImage {
  /// Replaces or inserts each changed inode's image; a directory keeps the entries the previous image held (moved,
  /// not copied), for the entry changes to bring up to date.
  fn apply_inodes(&mut self, changed_inodes: &[InodeImage]) {
    for changed in changed_inodes {
      let at = self
        .inodes
        .binary_search_by_key(&changed.no, |inode| inode.no);
      let mut image = changed.clone();
      if let BodyImage::Directory { entries, .. } = &mut image.body {
        // The entries the previous image held (none for a directory new in this delta), moved, not copied: the
        // previous image of this inode is replaced below. The entry changes then bring them up to date.
        if let Ok(at) = at
          && let Some(BodyImage::Directory { entries: held, .. }) =
            self.inodes.get_mut(at).map(|previous| &mut previous.body)
        {
          *entries = std::mem::take(held);
        }
      }
      match at {
        Ok(at) => {
          if let Some(slot) = self.inodes.get_mut(at) {
            *slot = image;
          }
        }
        Err(at) => self.inodes.insert(at, image),
      }
    }
  }

  /// Applies `delta` (the module doc): afterwards this image equals the volume's full image at the delta's
  /// publication. A change naming a directory the image does not hold is refused
  /// [`VfsError::RecoveryIncomplete`] — the delta does not belong to this image.
  pub fn apply(&mut self, delta: &VolumeDelta) -> Result<(), VfsError> {
    self.epoch = delta.epoch;
    self.next_counter = delta.next_counter;
    self.quota = delta.quota;
    self.root_no = delta.root_no;
    self.last_snapshot = delta.last_snapshot;
    self.orphans = delta.orphans.clone();
    for change in &delta.references {
      apply_reference(&mut self.references, change);
    }
    self.apply_inodes(&delta.inodes);
    let policy = crate::recover::policy_from_image(self.policy);
    for change in &delta.entries {
      let Ok(at) = self
        .inodes
        .binary_search_by_key(&change.dir, |inode| inode.no)
      else {
        if delta.removed.binary_search(&change.dir).is_ok() {
          continue;
        }
        return Err(VfsError::RecoveryIncomplete);
      };
      let Some(BodyImage::Directory { entries, .. }) =
        self.inodes.get_mut(at).map(|inode| &mut inode.body)
      else {
        return Err(VfsError::RecoveryIncomplete);
      };
      // Entries are in canonical order (`entry_order`), and at most one holds a folded name, so the entry the
      // change replaces or removes is found by binary search (A-89: a scan cost every replayed change its
      // directory). The present entry's name folds as the change's does: it was looked up under it.
      let found = entries.binary_search_by(|held| entry_order(policy, &held.name, &change.name));
      match (found, &change.entry) {
        (Ok(at), Some(entry)) => {
          if let Some(slot) = entries.get_mut(at) {
            slot.clone_from(entry);
          }
        }
        (Ok(at), None) => {
          entries.remove(at);
        }
        (Err(at), Some(entry)) => entries.insert(at, entry.clone()),
        (Err(_), None) => {}
      }
    }
    for no in &delta.removed {
      if let Ok(at) = self.inodes.binary_search_by_key(no, |inode| inode.no) {
        self.inodes.remove(at);
      }
    }
    Ok(())
  }
}

/// Sets one attachment's count on one inode in an image's reference lists (by attachment, then by inode), removing the
/// inode at zero and the attachment when it holds nothing.
fn apply_reference(references: &mut Vec<AttachmentReferences>, change: &ReferenceChange) {
  let held = references.binary_search_by_key(&change.attachment, |held| held.attachment);
  let at = match (held, change.count) {
    (Ok(at), _) => at,
    (Err(_), 0) => return,
    (Err(at), _) => {
      references.insert(
        at,
        AttachmentReferences {
          attachment: change.attachment,
          inodes: Vec::new(),
        },
      );
      at
    }
  };
  let Some(owner) = references.get_mut(at) else {
    return;
  };
  let reference = InodeReferences {
    inode: change.inode,
    count: change.count,
  };
  match (
    owner
      .inodes
      .binary_search_by_key(&change.inode, |held| held.inode),
    change.count,
  ) {
    (Ok(slot), 0) => {
      owner.inodes.remove(slot);
    }
    (Ok(slot), _) => {
      if let Some(held) = owner.inodes.get_mut(slot) {
        *held = reference;
      }
    }
    (Err(_), 0) => {}
    (Err(slot), _) => owner.inodes.insert(slot, reference),
  }
  if owner.inodes.is_empty() {
    references.remove(at);
  }
}
