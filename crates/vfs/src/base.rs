//! The base plane of an overlay volume (§4.5, §4.15, D-25; Phase 1 task 10): a host directory
//! serves every untouched entry on demand through the read-only seam ([`crate::host`]); the
//! first write copies an entry up and records the witnessed base (the stat fingerprint and the
//! BLAKE3 of the bytes the edit was based on); deletes leave whiteouts; renamed base directories
//! record their origin (a redirect); drift is detected by fingerprints under the racy rule and
//! reported, never absorbed; watcher hints make reports prompt and are never the truth.
//!
//! Ownership: the host is owned by whoever opened the base directory (the shard in Phase 2, the
//! test here) and lent to each operation as `&mut dyn HostFs`; a volume holds only handles and
//! tables ([`BasePlane`]), so clones of an overlay snapshot share one host without sharing
//! anything mutable (D-7, D-8). Every base-aware verb lives on [`Overlay`], a borrow of a volume
//! and a host together; the plain `Volume` verbs stay exact for scratch volumes and for entries
//! already copied up, and refuse (`BaseUnavailable`) where they would need the disk.
//!
//! Invariants: the overlay's directory nodes hold exactly the entries the volume touched
//! (looked up, listed, created, whiteouted) and the diverged set is the witnessed, created,
//! whiteouted and redirected ones (AC-1.10); an unwitnessed entry always shows the live disk;
//! a witnessed entry's unpinned bytes are read only after an `fstat` matches the witness, else
//! the read is `BaseDrift` and the entry is `lost` (AC-1.11); memory after `create` is one
//! directory handle and empty tables (AC-1.9).

use std::collections::{BTreeMap, BTreeSet};

use slates_mem::Handle;

use crate::dir::{BaseDirState, Child, DirNode};
use crate::error::VfsError;
use crate::host::{
  BaseEntry, Hint, HostDir, HostError, HostFacts, HostFile, HostFs, HostKind, WatchState,
};
use crate::ids::InodeNo;
use crate::inode::{BaseBody, Body, Fingerprint, Home, Inode, Kind, Witness};
use crate::journal::Op;
use crate::volume::{DirRow, Located, Store, Volume, VolumeConfig};

/// How an overlay volume is created: the root directory handle the caller opened, the host's
/// facts about it, and the large-file class boundary.
#[derive(Clone, Copy, Debug)]
pub struct BaseConfig {
  /// The base directory, opened by the caller (`O_DIRECTORY|O_NOFOLLOW`); never closed by the
  /// volume.
  pub root: HostDir,
  /// The filesystem facts the drift rules need.
  pub facts: HostFacts,
  /// Derived: files up to this size are copied up whole; larger ones keep their descriptor and
  /// pin only the written windows. Until Phase 7 measures the CDC threshold that D-6 names as
  /// this boundary, the caller passes the profile's `arena_region_bytes` (one mapped region),
  /// so a whole small-class copy-up never spans a region.
  pub large_class_bytes: u64,
}

/// A directory's cached listing: the host handle, the fingerprint the entries were read under,
/// and the entries in the volume's canonical order.
#[derive(Debug)]
pub(crate) struct Listing {
  pub(crate) dir: HostDir,
  fingerprint: Option<Fingerprint>,
  entries: Option<Vec<BaseEntry>>,
  /// The volume clock when the entries were read, for the racy rule.
  read_at_ns: u64,
  watch: WatchState,
}

/// What changed on disk beneath a witnessed entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DriftKind {
  /// Same inode, different bytes or attributes.
  Modified,
  /// The entry is gone.
  Deleted,
  /// Another inode sits at the path.
  Replaced,
  /// A file became a directory or the reverse.
  TypeChanged,
}

/// What `status` reports for an overlay volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseStatus {
  /// Every witnessed entry whose disk no longer matches its witness, by path.
  pub drift: Vec<(String, DriftKind)>,
  /// The watcher's state.
  pub watcher: WatchState,
}

/// Why an entry is in the diverged set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Divergence {
  /// Created by the volume (no base beneath, or an opaque directory over a removed one).
  Created,
  /// A base entry copied up: the volume holds a witness for it.
  Witnessed,
  /// A base name deleted by the volume.
  Whiteout,
  /// A base directory renamed by the volume; the path is the new one.
  Redirect,
}

/// One diverged entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diverged {
  /// The path in the volume.
  pub path: String,
  /// Why.
  pub kind: Divergence,
}

/// Entries a fresh listing lacks (by name) and unwitnessed files it still lists with their
/// fingerprints.
type Stale = (Vec<String>, Vec<(InodeNo, Fingerprint)>);

/// Which copy-up a mutation needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CopyUp {
  /// Bytes will change: the small class is read whole, the large class keeps its descriptor.
  Content,
  /// Only attributes change: the witness is recorded and nothing is pinned.
  Metadata,
}

/// The base plane's tables, owned by the volume.
#[derive(Debug)]
pub struct BasePlane {
  root: HostDir,
  facts: HostFacts,
  large_class_bytes: u64,
  /// Listings by directory inode number.
  pub(crate) listings: BTreeMap<InodeNo, Listing>,
  /// Witnesses by inode number.
  witnesses: BTreeMap<InodeNo, Witness>,
  /// Where on the disk each witness was taken: the base directory's inode number and the
  /// entry name there. A renamed base file keeps its bytes at the old disk path (§4.5), so
  /// drift checks and descriptors follow this, not the volume's current name.
  witness_homes: BTreeMap<InodeNo, (InodeNo, Box<str>)>,
  /// Drift by inode number.
  drift: BTreeMap<InodeNo, DriftKind>,
  /// Open file descriptors by inode number (large-class copies and read-through).
  descriptors: BTreeMap<InodeNo, HostFile>,
  watch: WatchState,
  /// Directories whose listings a hint invalidated and whose witnessed entries want a check.
  recheck: BTreeSet<InodeNo>,
  recheck_all: bool,
}

impl BasePlane {
  fn new(config: BaseConfig, root_no: InodeNo) -> Self {
    let mut listings = BTreeMap::new();
    listings.insert(
      root_no,
      Listing {
        dir: config.root,
        fingerprint: None,
        entries: None,
        read_at_ns: 0,
        watch: WatchState::Unavailable,
      },
    );
    Self {
      root: config.root,
      facts: config.facts,
      large_class_bytes: config.large_class_bytes,
      listings,
      witnesses: BTreeMap::new(),
      witness_homes: BTreeMap::new(),
      drift: BTreeMap::new(),
      descriptors: BTreeMap::new(),
      watch: WatchState::Unavailable,
      recheck: BTreeSet::new(),
      recheck_all: false,
    }
  }

  /// A clone's plane: the same root and facts, the origin's witnesses, its own listings.
  pub(crate) fn for_clone(&self, root_no: InodeNo) -> Self {
    let mut plane = Self::new(
      BaseConfig {
        root: self.root,
        facts: self.facts,
        large_class_bytes: self.large_class_bytes,
      },
      root_no,
    );
    plane.witnesses = self.witnesses.clone();
    plane.witness_homes = self.witness_homes.clone();
    plane
  }

  /// The witness of an inode, if it was copied up.
  pub fn witness(&self, no: InodeNo) -> Option<Witness> {
    self.witnesses.get(&no).copied()
  }

  /// Whether the inode is witnessed.
  pub fn is_witnessed(&self, no: InodeNo) -> bool {
    self.witnesses.contains_key(&no)
  }
}

fn host_refusal(e: HostError) -> VfsError {
  match e {
    HostError::NotFound => VfsError::NotFound,
    HostError::NotDirectory => VfsError::NotDirectory,
    HostError::NotFile => VfsError::IsDirectory,
    HostError::StaleHandle => VfsError::StaleHandle,
    HostError::Unavailable(errno) => VfsError::BaseUnavailable(errno),
  }
}

impl Volume {
  /// Creates an overlay volume over an opened host directory: one handle recorded, empty
  /// tables, no walk, no hashing, no copy (AC-1.9).
  pub fn create_overlay(
    store: &mut Store,
    config: VolumeConfig,
    base: BaseConfig,
  ) -> Result<Volume, VfsError> {
    let mut vol = Volume::create(store, config)?;
    let root = vol.root();
    store.dirs.get_mut(root)?.base = BaseDirState::Merged;
    let root_no = store.dirs.get(root)?.inode;
    vol.base = Some(BasePlane::new(base, root_no));
    Ok(vol)
  }

  /// Whether the volume has a base.
  pub fn is_overlay(&self) -> bool {
    self.base.is_some()
  }

  /// The base plane's tables, for inspection.
  pub fn base_plane(&self) -> Option<&BasePlane> {
    self.base.as_ref()
  }

  /// Borrows the volume together with its host for base-aware operations.
  pub fn with_host<'a>(&'a mut self, host: &'a mut dyn HostFs) -> Overlay<'a> {
    Overlay { vol: self, host }
  }

  /// The diverged set (AC-1.10): witnessed, created, whiteouted and redirected entries, by
  /// path, over the loaded nodes only (cost proportional to what the volume touched).
  pub fn diverged(&self, store: &Store) -> Vec<Diverged> {
    let mut out = Vec::new();
    let mut stack = vec![(String::new(), self.root)];
    while let Some((prefix, dir)) = stack.pop() {
      let Ok(node) = store.dirs.get(dir) else {
        continue;
      };
      for e in node.iter(&store.blocks) {
        let path = format!("{prefix}/{}", e.name);
        match e.child {
          Child::Whiteout => out.push(Diverged {
            path,
            kind: Divergence::Whiteout,
          }),
          Child::Dir(h) => {
            if let Ok(child) = store.dirs.get(h) {
              if child.origin.is_some() {
                out.push(Diverged {
                  path: path.clone(),
                  kind: Divergence::Redirect,
                });
              } else if child.base == BaseDirState::Opaque {
                out.push(Diverged {
                  path: path.clone(),
                  kind: Divergence::Created,
                });
              }
              stack.push((path, h));
            }
          }
          Child::File(no) | Child::Symlink(no) => {
            let witnessed = self.base.as_ref().is_some_and(|b| b.is_witnessed(no));
            let is_base = matches!(self.inode(store, no).map(|i| &i.body), Ok(Body::Base(_)));
            if witnessed {
              out.push(Diverged {
                path,
                kind: Divergence::Witnessed,
              });
            } else if !is_base {
              out.push(Diverged {
                path,
                kind: Divergence::Created,
              });
            }
          }
        }
      }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
  }

  /// Whether the base beneath `dir` holds `name` (loaded listing only; the caller loads it).
  pub(crate) fn base_listing_has(&self, dir_no: InodeNo, name: &str) -> bool {
    let policy = self.policy;
    self
      .base
      .as_ref()
      .and_then(|b| b.listings.get(&dir_no))
      .and_then(|l| l.entries.as_ref())
      .is_some_and(|entries| entries.iter().any(|e| policy.same(&e.name, name)))
  }

  /// Whether a directory is empty for `rmdir`: no live overlay entry, and every base name
  /// under it whiteouted (the listing must be loaded, which the overlay verbs do).
  pub(crate) fn empty_for_rmdir(
    &self,
    store: &Store,
    dir: Handle<DirNode>,
  ) -> Result<bool, VfsError> {
    let node = store.dirs.get(dir)?;
    if node.live_len(&store.blocks) > 0 {
      return Ok(false);
    }
    if node.base != BaseDirState::Merged {
      return Ok(true);
    }
    let policy = self.policy;
    let Some(entries) = self
      .base
      .as_ref()
      .and_then(|b| b.listings.get(&node.inode))
      .and_then(|l| l.entries.as_ref())
    else {
      return Ok(true);
    };
    Ok(entries.iter().all(|e| {
      node
        .lookup(&store.blocks, policy, &e.name)
        .is_some_and(|x| x.child == Child::Whiteout)
    }))
  }

  /// Closes the descriptor an inode held, when its last link goes.
  pub(crate) fn base_forget(&mut self, no: InodeNo) -> Option<HostFile> {
    let plane = self.base.as_mut()?;
    plane.witnesses.remove(&no);
    plane.witness_homes.remove(&no);
    plane.drift.remove(&no);
    plane.descriptors.remove(&no)
  }
}

/// A volume borrowed together with its host: the base-aware verbs.
pub struct Overlay<'a> {
  vol: &'a mut Volume,
  host: &'a mut dyn HostFs,
}

impl Overlay<'_> {
  /// The volume.
  pub fn volume(&mut self) -> &mut Volume {
    self.vol
  }

  fn plane(&mut self) -> Result<&mut BasePlane, VfsError> {
    self.vol.base.as_mut().ok_or(VfsError::NotOverlay)
  }

  fn granularity(&self) -> u64 {
    self
      .vol
      .base
      .as_ref()
      .map_or(1, |b| b.facts.timestamp_granularity_ns.max(1))
  }

  // ---------------------------------------------------------------- listings

  /// Loads or refreshes the listing of a merged directory: the directory's fingerprint is
  /// compared on every use and the entries are re-read when it moved or a hint invalidated
  /// them; unloaded (unwitnessed) entries the disk no longer has leave the node.
  fn load_listing(&mut self, store: &mut Store, dir: Handle<DirNode>) -> Result<(), VfsError> {
    let dir_no = store.dirs.get(dir)?.inode;
    let now = self.vol.clock.monotonic_ns();
    let policy = self.vol.policy;
    let plane = self.vol.base.as_mut().ok_or(VfsError::NotOverlay)?;
    let listing = plane
      .listings
      .get_mut(&dir_no)
      .ok_or(VfsError::NotOverlay)?;
    if listing.watch == WatchState::Unavailable {
      listing.watch = self.host.watch(listing.dir);
      if listing.watch == WatchState::Live && plane.watch == WatchState::Unavailable {
        plane.watch = WatchState::Live;
      }
    }
    let fingerprint = self
      .host
      .fingerprint_dir(listing.dir)
      .map_err(host_refusal)?;
    if listing.entries.is_some() && listing.fingerprint == Some(fingerprint) {
      return Ok(());
    }
    let mut entries = self.host.list(listing.dir).map_err(host_refusal)?;
    entries.sort_by(|a, b| {
      (policy.hash(&a.name), policy.fold(&a.name))
        .cmp(&(policy.hash(&b.name), policy.fold(&b.name)))
    });
    listing.fingerprint = Some(fingerprint);
    listing.read_at_ns = now;
    listing.entries = Some(entries);
    self.prune_unloaded(store, dir)
  }

  /// Brings a node's unwitnessed base entries in line with the fresh listing: an untouched
  /// entry shows the live disk, so one the disk no longer has leaves the node, and one the
  /// disk changed drops its descriptor and takes the listing's attributes.
  fn prune_unloaded(&mut self, store: &mut Store, dir: Handle<DirNode>) -> Result<(), VfsError> {
    let (gone, changed) = self.stale_entries(store, dir)?;
    for (no, fp) in changed {
      self.refresh_unloaded(store, no, fp)?;
    }
    for name in gone {
      self.drop_unloaded(store, dir, &name)?;
    }
    Ok(())
  }

  /// The node's entries the fresh listing lacks (by name) and the unwitnessed files it still
  /// lists (with their listed fingerprints).
  fn stale_entries(&self, store: &Store, dir: Handle<DirNode>) -> Result<Stale, VfsError> {
    let dir_no = store.dirs.get(dir)?.inode;
    let policy = self.vol.policy;
    let plane = self.vol.base.as_ref().ok_or(VfsError::NotOverlay)?;
    let Some(entries) = plane.listings.get(&dir_no).and_then(|l| l.entries.as_ref()) else {
      return Ok((Vec::new(), Vec::new()));
    };
    let mut gone = Vec::new();
    let mut changed = Vec::new();
    for e in store.dirs.get(dir)?.iter(&store.blocks) {
      let listed = entries.iter().find(|b| policy.same(&b.name, e.name));
      match (listed, e.child) {
        (Some(l), Child::File(no)) if l.kind == HostKind::File && self.unloaded_file(store, no) => {
          changed.push((no, l.fingerprint));
        }
        (Some(_), _) => {}
        (None, Child::File(no) | Child::Symlink(no)) if self.unloaded_file(store, no) => {
          gone.push(e.name.to_owned());
        }
        (None, Child::Dir(h)) => {
          if store
            .dirs
            .get(h)
            .is_ok_and(|n| n.base == BaseDirState::Merged && n.is_empty() && n.origin.is_none())
          {
            gone.push(e.name.to_owned());
          }
        }
        (None, _) => {}
      }
    }
    Ok((gone, changed))
  }

  /// Whether an inode is an untouched base entry (unwitnessed, base-backed or a base symlink,
  /// one link).
  fn unloaded_file(&self, store: &Store, no: InodeNo) -> bool {
    let witnessed = self.vol.base.as_ref().is_some_and(|b| b.is_witnessed(no));
    !witnessed
      && self
        .vol
        .inode(store, no)
        .is_ok_and(|i| matches!(i.body, Body::Base(_) | Body::Symlink(_)) && i.attrs.nlink == 1)
  }

  /// Removes an untouched entry the disk no longer has from the node (no whiteout, no journal).
  fn drop_unloaded(
    &mut self,
    store: &mut Store,
    dir: Handle<DirNode>,
    name: &str,
  ) -> Result<(), VfsError> {
    let located = self.vol.lookup(store, dir, name)?;
    let d = self.vol.make_current_dir(store, dir)?;
    let mut retired = crate::dirtree::Retired::new();
    let epoch = self.vol.epoch;
    let policy = self.vol.policy;
    let cutover = store.dir_cutover;
    let _ = store.dirs.get_mut(d)?.remove(
      &mut store.blocks,
      epoch,
      &mut retired,
      policy,
      name,
      cutover,
    )?;
    self.vol.retire_blocks(store, retired)?;
    match located.child {
      Child::Dir(h) => {
        self.vol.release_dir_node(store, h)?;
        self.vol.drop_link(store, located.inode)?;
        self.vol.drop_link(store, located.inode)?;
        if let Some(plane) = self.vol.base.as_mut()
          && let Some(l) = plane.listings.remove(&located.inode)
        {
          self.host.close_dir(l.dir);
        }
      }
      _ => self.vol.drop_link(store, located.inode)?,
    }
    Ok(())
  }

  /// An unwitnessed base inode follows the disk: the descriptor it held is closed (the disk
  /// may hold another inode at the name now) and its attributes take the listing's.
  fn refresh_unloaded(
    &mut self,
    store: &mut Store,
    no: InodeNo,
    fp: Fingerprint,
  ) -> Result<(), VfsError> {
    if let Some(f) = self.plane()?.descriptors.remove(&no) {
      self.host.close_file(f);
    }
    let handle = self.vol.make_current_inode(store, no)?;
    let inode = store.inodes.get_mut(handle)?;
    inode.attrs.size = fp.size;
    inode.attrs.mode = fp.mode;
    inode.attrs.mtime = fp.mtime_ns;
    inode.attrs.ctime = fp.ctime_ns;
    if let Body::Base(b) = &mut inode.body {
      b.base_len = fp.size;
    }
    Ok(())
  }

  /// The attributes of an inode, live for an unwitnessed base entry (its directory's listing
  /// validated and its descriptor `fstat`ed), as a bridge's `getattr` needs them.
  pub fn stat(&mut self, store: &mut Store, no: InodeNo) -> Result<crate::inode::Attrs, VfsError> {
    self.follow_live_disk(store, no)?;
    self.vol.stat(store, no)
  }

  /// For an unwitnessed base entry: validates the directory's listing (one `fstat` of the
  /// directory; a changed one reloads and refreshes the entry) and takes the size from the
  /// descriptor, so an in-place change shows through too.
  fn follow_live_disk(&mut self, store: &mut Store, no: InodeNo) -> Result<(), VfsError> {
    let (is_base, witnessed) = {
      let inode = self.vol.inode(store, no)?;
      (
        matches!(inode.body, Body::Base(_)),
        self.vol.base.as_ref().is_some_and(|b| b.is_witnessed(no)),
      )
    };
    if !is_base || witnessed {
      return Ok(());
    }
    if let Some(parent) = self.vol.inode(store, no)?.home.map(|h| h.parent)
      && let Ok(dir) = self.vol.current_dir(store, parent)
    {
      self.load_listing(store, dir)?;
    }
    if !matches!(self.vol.inode(store, no)?.body, Body::Base(_)) {
      return Ok(());
    }
    let file = self.descriptor(store, no)?;
    let fp = self.host.fstat(file).map_err(host_refusal)?;
    let handle = self.vol.make_current_inode(store, no)?;
    let inode = store.inodes.get_mut(handle)?;
    inode.attrs.size = fp.size;
    inode.attrs.mtime = fp.mtime_ns;
    inode.attrs.ctime = fp.ctime_ns;
    if let Body::Base(b) = &mut inode.body {
      b.base_len = fp.size;
    }
    Ok(())
  }

  /// The listing entry named `name` beneath `dir`, if the base has it.
  fn base_entry(
    &mut self,
    store: &mut Store,
    dir: Handle<DirNode>,
    name: &str,
  ) -> Result<Option<BaseEntry>, VfsError> {
    if store.dirs.get(dir)?.base != BaseDirState::Merged {
      return Ok(None);
    }
    self.load_listing(store, dir)?;
    let dir_no = store.dirs.get(dir)?.inode;
    let policy = self.vol.policy;
    Ok(
      self
        .vol
        .base
        .as_ref()
        .and_then(|b| b.listings.get(&dir_no))
        .and_then(|l| l.entries.as_ref())
        .and_then(|es| es.iter().find(|e| policy.same(&e.name, name)).cloned()),
    )
  }

  /// Gives a base entry its inode and its place in the node: an unloaded `Base` body for a
  /// file, a merged node for a directory, the target for a symlink. Not a mutation of the
  /// delta: nothing is journaled and no timestamp moves.
  fn materialize(
    &mut self,
    store: &mut Store,
    dir: Handle<DirNode>,
    entry: &BaseEntry,
  ) -> Result<Located, VfsError> {
    let dir = self.vol.make_current_dir(store, dir)?;
    let parent_no = store.dirs.get(dir)?.inode;
    let no = self.vol.next_no();
    let fp = entry.fingerprint;
    let epoch = self.vol.epoch;
    let child = match entry.kind {
      HostKind::File => {
        let mut inode = Inode::new(
          no,
          epoch,
          Kind::File,
          fp.mode,
          Body::Base(BaseBody {
            base_len: fp.size,
            ..BaseBody::default()
          }),
        );
        inode.attrs.size = fp.size;
        inode.attrs.mtime = fp.mtime_ns;
        inode.attrs.ctime = fp.ctime_ns;
        inode.home = Some(Home {
          parent: parent_no,
          hash: self.vol.policy.hash(&entry.name),
        });
        let handle = store.inodes.insert(inode)?;
        self.vol.table_set(store, no, handle)?;
        Child::File(no)
      }
      HostKind::Symlink => {
        let parent_dir = self.listing_dir(parent_no)?;
        let target = self
          .host
          .read_link(parent_dir, &entry.name)
          .map_err(host_refusal)?;
        let mut inode = Inode::new(no, epoch, Kind::Symlink, fp.mode, Body::Symlink(target));
        inode.attrs.size = fp.size;
        inode.home = Some(Home {
          parent: parent_no,
          hash: self.vol.policy.hash(&entry.name),
        });
        let handle = store.inodes.insert(inode)?;
        self.vol.table_set(store, no, handle)?;
        Child::Symlink(no)
      }
      HostKind::Dir => {
        let parent_dir = self.listing_dir(parent_no)?;
        let opened = self
          .host
          .open_dir(parent_dir, &entry.name)
          .map_err(host_refusal)?;
        let mut node = DirNode::new(epoch, Some(parent_no), no, &entry.name);
        node.base = BaseDirState::Merged;
        let child = store.dirs.insert(node)?;
        let mut inode = Inode::new(no, epoch, Kind::Dir, fp.mode, Body::Directory(child));
        inode.attrs.nlink = 2;
        inode.attrs.mtime = fp.mtime_ns;
        inode.attrs.ctime = fp.ctime_ns;
        let handle = store.inodes.insert(inode)?;
        self.vol.table_set(store, no, handle)?;
        let plane = self.plane()?;
        plane.listings.insert(
          no,
          Listing {
            dir: opened,
            fingerprint: None,
            entries: None,
            read_at_ns: 0,
            watch: WatchState::Unavailable,
          },
        );
        Child::Dir(child)
      }
      HostKind::Other => return Err(VfsError::NotFound),
    };
    self.vol.dir_insert(store, dir, &entry.name, child)?;
    Ok(Located { child, inode: no })
  }

  fn listing_dir(&mut self, dir_no: InodeNo) -> Result<HostDir, VfsError> {
    self
      .vol
      .base
      .as_ref()
      .and_then(|b| b.listings.get(&dir_no))
      .map(|l| l.dir)
      .ok_or(VfsError::NotOverlay)
  }

  // ---------------------------------------------------------------- namespace

  /// A lookup that consults the base beneath a merged directory: an overlay entry wins, a
  /// whiteout is `ENOENT`, otherwise the listing (a hit gets its inode now).
  pub fn lookup(
    &mut self,
    store: &mut Store,
    dir: Handle<DirNode>,
    name: &str,
  ) -> Result<Located, VfsError> {
    match self.vol.lookup(store, dir, name) {
      // An untouched entry shows the live disk: the directory's listing is validated (one
      // `fstat` of the directory) and the entry looked up again, in case the disk lost it.
      Ok(l) if self.is_unloaded(store, &l) => {
        let d = self.vol.head_dir(store, dir)?;
        self.load_listing(store, d)?;
        match self.vol.lookup(store, d, name) {
          Ok(l) => return Ok(l),
          Err(VfsError::NotFound) => {}
          Err(e) => return Err(e),
        }
      }
      Ok(l) => return Ok(l),
      Err(VfsError::NotFound) => {}
      Err(e) => return Err(e),
    }
    let dir = self.vol.head_dir(store, dir)?;
    let node = store.dirs.get(dir)?;
    if node.base != BaseDirState::Merged
      || node
        .lookup(&store.blocks, self.vol.policy, name)
        .is_some_and(|e| e.child == Child::Whiteout)
    {
      return Err(VfsError::NotFound);
    }
    let Some(entry) = self.base_entry(store, dir, name)? else {
      return Err(VfsError::NotFound);
    };
    self.materialize(store, dir, &entry)
  }

  /// Whether a located entry is an untouched base entry (unwitnessed, base-backed).
  fn is_unloaded(&self, store: &Store, located: &Located) -> bool {
    match located.child {
      Child::File(no) | Child::Symlink(no) => self.unloaded_file(store, no),
      Child::Dir(_) | Child::Whiteout => false,
    }
  }

  /// Resolves an absolute path through merged directories.
  pub fn resolve(&mut self, store: &mut Store, path: &str) -> Result<Located, VfsError> {
    let root = self.vol.root();
    let mut last = Located {
      child: Child::Dir(root),
      inode: store.dirs.get(root)?.inode,
    };
    for part in path.split('/').filter(|p| !p.is_empty()) {
      let Child::Dir(d) = last.child else {
        return Err(VfsError::NotDirectory);
      };
      last = self.lookup(store, d, part)?;
    }
    Ok(last)
  }

  /// Lists a directory with its base merged in: every base entry not shadowed by an overlay
  /// entry or a whiteout gets its inode, then the node lists in canonical order.
  pub fn readdir<'s>(
    &mut self,
    store: &'s mut Store,
    dir: Handle<DirNode>,
  ) -> Result<Vec<DirRow<'s>>, VfsError> {
    let dir = self.vol.head_dir(store, dir)?;
    if store.dirs.get(dir)?.base == BaseDirState::Merged {
      self.load_listing(store, dir)?;
      let dir_no = store.dirs.get(dir)?.inode;
      let entries: Vec<BaseEntry> = self
        .vol
        .base
        .as_ref()
        .and_then(|b| b.listings.get(&dir_no))
        .and_then(|l| l.entries.clone())
        .unwrap_or_default();
      let policy = self.vol.policy;
      for e in entries {
        let current = self.vol.head_dir(store, dir)?;
        if store
          .dirs
          .get(current)?
          .lookup(&store.blocks, policy, &e.name)
          .is_none()
          && e.kind != HostKind::Other
        {
          self.materialize(store, current, &e)?;
        }
      }
    }
    let dir = self.vol.head_dir(store, dir)?;
    self.vol.readdir(store, dir)
  }

  /// Whether `name` exists beneath `dir` in the overlay or the base (for the create verbs).
  fn exists(
    &mut self,
    store: &mut Store,
    dir: Handle<DirNode>,
    name: &str,
  ) -> Result<bool, VfsError> {
    match self.lookup(store, dir, name) {
      Ok(_) => Ok(true),
      Err(VfsError::NotFound) => Ok(false),
      Err(e) => Err(e),
    }
  }

  /// `create_file` that refuses a name the base holds.
  pub fn create_file(
    &mut self,
    store: &mut Store,
    dir: Handle<DirNode>,
    name: &str,
    mode: u32,
  ) -> Result<InodeNo, VfsError> {
    if self.exists(store, dir, name)? {
      return Err(VfsError::AlreadyExists);
    }
    self.vol.create_file(store, dir, name, mode)
  }

  /// `mkdir` that refuses a name the base holds; a directory created in an overlay volume is
  /// opaque (only overlay entries show), including one recreated over a whiteout.
  pub fn mkdir(
    &mut self,
    store: &mut Store,
    dir: Handle<DirNode>,
    name: &str,
    mode: u32,
  ) -> Result<Handle<DirNode>, VfsError> {
    if self.exists(store, dir, name)? {
      return Err(VfsError::AlreadyExists);
    }
    let created = self.vol.mkdir(store, dir, name, mode)?;
    store.dirs.get_mut(created)?.base = BaseDirState::Opaque;
    Ok(created)
  }

  /// `symlink` that refuses a name the base holds.
  pub fn symlink(
    &mut self,
    store: &mut Store,
    dir: Handle<DirNode>,
    name: &str,
    target: &str,
  ) -> Result<InodeNo, VfsError> {
    if self.exists(store, dir, name)? {
      return Err(VfsError::AlreadyExists);
    }
    self.vol.symlink(store, dir, name, target)
  }

  /// `link` that refuses a name the base holds; a link to an unwitnessed base file copies its
  /// witness up first (the link count is a metadata change).
  pub fn link(
    &mut self,
    store: &mut Store,
    dir: Handle<DirNode>,
    name: &str,
    target: InodeNo,
  ) -> Result<(), VfsError> {
    if self.exists(store, dir, name)? {
      return Err(VfsError::AlreadyExists);
    }
    self.copy_up(store, target, CopyUp::Metadata)?;
    self.vol.link(store, dir, name, target)
  }

  /// `unlink` over a merged directory: a base name leaves a whiteout, journaled as such.
  pub fn unlink(
    &mut self,
    store: &mut Store,
    dir: Handle<DirNode>,
    name: &str,
  ) -> Result<(), VfsError> {
    let located = self.lookup(store, dir, name)?;
    if matches!(located.child, Child::Dir(_)) {
      return Err(VfsError::IsDirectory);
    }
    let dir = self.vol.head_dir(store, dir)?;
    let _ = self.base_entry(store, dir, name)?;
    self.vol.unlink(store, dir, name)
  }

  /// `rmdir` over a merged directory: empty means no live overlay entry and every base name
  /// whiteouted; the removed name leaves a whiteout.
  pub fn rmdir(
    &mut self,
    store: &mut Store,
    dir: Handle<DirNode>,
    name: &str,
  ) -> Result<(), VfsError> {
    let located = self.lookup(store, dir, name)?;
    let Child::Dir(child) = located.child else {
      return Err(VfsError::NotDirectory);
    };
    if store.dirs.get(child)?.base == BaseDirState::Merged {
      self.load_listing(store, child)?;
    }
    let child = self.vol.head_dir(store, child)?;
    if !self.vol.empty_for_rmdir(store, child)? {
      return Err(VfsError::NotEmpty);
    }
    let dir = self.vol.head_dir(store, dir)?;
    let _ = self.base_entry(store, dir, name)?;
    let removed_no = located.inode;
    self.vol.rmdir(store, dir, name)?;
    if let Some(plane) = self.vol.base.as_mut()
      && let Some(l) = plane.listings.remove(&removed_no)
    {
      self.host.close_dir(l.dir);
    }
    Ok(())
  }

  /// `rename` over merged directories: a base file copies its witness up and leaves a
  /// whiteout; a base directory records its origin (a redirect) and leaves a whiteout.
  pub fn rename(
    &mut self,
    store: &mut Store,
    from_dir: Handle<DirNode>,
    from_name: &str,
    to_dir: Handle<DirNode>,
    to_name: &str,
  ) -> Result<(), VfsError> {
    let source = self.lookup(store, from_dir, from_name)?;
    let _ = self.lookup(store, to_dir, to_name);
    let from_dir = self.vol.head_dir(store, from_dir)?;
    let to_dir = self.vol.head_dir(store, to_dir)?;
    let from_base = self.base_entry(store, from_dir, from_name)?;
    let _ = self.base_entry(store, to_dir, to_name)?;
    let origin = match (source.child, &from_base) {
      (Child::File(no) | Child::Symlink(no), Some(_)) => {
        self.copy_up(store, no, CopyUp::Metadata)?;
        None
      }
      (Child::Dir(h), Some(_)) => {
        let node = store.dirs.get(h)?;
        (node.base == BaseDirState::Merged && node.origin.is_none())
          .then(|| self.vol.path_of(store, from_dir, from_name))
      }
      _ => None,
    };
    self
      .vol
      .rename(store, from_dir, from_name, to_dir, to_name)?;
    if let Some(from) = origin {
      let moved = self.vol.lookup(store, to_dir, to_name)?;
      if let Child::Dir(h) = moved.child {
        let h = self.vol.make_current_dir_node(store, h)?;
        store.dirs.get_mut(h)?.origin = Some(from.clone().into());
        let to_path = self.vol.path_of(store, to_dir, to_name);
        self.vol.record(
          Op::Redirect { from: from.into() },
          &to_path,
          Some(moved.inode),
          0,
        );
      }
    }
    Ok(())
  }

  // ---------------------------------------------------------------- content

  /// Reads through pinned extents and the disk; a witnessed entry's disk bytes are read only
  /// after an `fstat` matches the witness, else `BaseDrift` (AC-1.11).
  pub fn read(
    &mut self,
    store: &mut Store,
    no: InodeNo,
    off: u64,
    buf: &mut [u8],
  ) -> Result<usize, VfsError> {
    self.follow_live_disk(store, no)?;
    let (base_len, lost, size, covered) = match &self.vol.inode(store, no)?.body {
      Body::Base(b) => {
        let end = off.saturating_add(u64::try_from(buf.len()).unwrap_or(u64::MAX));
        let covered = b
          .pinned
          .iter()
          .any(|e| e.off <= off && end <= e.off + e.len);
        (
          b.base_len,
          b.lost,
          self.vol.inode(store, no)?.attrs.size,
          covered,
        )
      }
      _ => return self.vol.read(store, no, off, buf),
    };
    if off >= size {
      return Ok(0);
    }
    // A lost entry still serves the agent's own pinned bytes; anything else would be torn.
    if lost && !(covered || off >= base_len) {
      return Err(VfsError::BaseDrift);
    }
    let want =
      usize::try_from((size - off).min(u64::try_from(buf.len()).unwrap_or(u64::MAX))).unwrap_or(0);
    let out = &mut buf[..want];
    out.fill(0);
    // Disk bytes first (within the valid base length), then the pinned extents over them.
    if off < base_len && !covered {
      let disk_want =
        usize::try_from((base_len - off).min(u64::try_from(want).unwrap_or(u64::MAX))).unwrap_or(0);
      self.read_disk(store, no, off, &mut out[..disk_want])?;
    }
    if let Body::Base(b) = &self.vol.inode(store, no)?.body {
      for e in &b.pinned {
        if let Some(bytes) = store.content.extent_bytes(e) {
          crate::volume::copy_range(bytes, e.off, off, out);
        }
      }
    }
    Ok(want)
  }

  /// Fills `out` from the disk at `off` through the inode's descriptor, after the drift check
  /// when the entry is witnessed.
  fn read_disk(
    &mut self,
    store: &mut Store,
    no: InodeNo,
    off: u64,
    out: &mut [u8],
  ) -> Result<(), VfsError> {
    let witnessed = self.vol.base.as_ref().is_some_and(|b| b.is_witnessed(no));
    let file = self.descriptor(store, no)?;
    if witnessed {
      self.check_drift(store, no)?;
    }
    let mut done = 0usize;
    while done < out.len() {
      let n = self
        .host
        .read_at(
          file,
          off + u64::try_from(done).unwrap_or(0),
          &mut out[done..],
        )
        .map_err(host_refusal)?;
      if n == 0 {
        break;
      }
      done += n;
    }
    Ok(())
  }

  /// `write` with copy-up: the small class is read whole first, the large class pins the
  /// touched windows from disk, then the plain write applies.
  pub fn write(
    &mut self,
    store: &mut Store,
    no: InodeNo,
    off: u64,
    bytes: &[u8],
  ) -> Result<usize, VfsError> {
    self.copy_up(store, no, CopyUp::Content)?;
    let end = off.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
    self.pin_windows(store, no, off, end)?;
    self.vol.write(store, no, off, bytes)
  }

  /// `truncate` with copy-up.
  pub fn truncate(&mut self, store: &mut Store, no: InodeNo, len: u64) -> Result<(), VfsError> {
    self.copy_up(store, no, CopyUp::Content)?;
    self.vol.truncate(store, no, len)
  }

  /// `edit` with copy-up: the whole file is pinned first, as its tail is rewritten.
  pub fn edit(
    &mut self,
    store: &mut Store,
    no: InodeNo,
    at: u64,
    delete_len: u64,
    bytes: &[u8],
  ) -> Result<(), VfsError> {
    self.copy_up(store, no, CopyUp::Content)?;
    let size = self.vol.inode(store, no)?.attrs.size;
    self.pin_windows(store, no, 0, size)?;
    self.vol.edit(store, no, at, delete_len, bytes)
  }

  /// `chmod` with a metadata-only copy-up.
  pub fn chmod(&mut self, store: &mut Store, no: InodeNo, mode: u32) -> Result<(), VfsError> {
    self.copy_up(store, no, CopyUp::Metadata)?;
    self.vol.chmod(store, no, mode)
  }

  /// The open descriptor of a base-backed inode, opened through its home if needed.
  fn descriptor(&mut self, store: &Store, no: InodeNo) -> Result<HostFile, VfsError> {
    if let Some(f) = self.vol.base.as_ref().and_then(|b| b.descriptors.get(&no)) {
      return Ok(*f);
    }
    let (dir, name) = self.home_of(store, no)?;
    let file = self.host.open_file(dir, &name).map_err(host_refusal)?;
    self.plane()?.descriptors.insert(no, file);
    Ok(file)
  }

  /// The host directory and entry name of a file inode on the disk: where its witness was
  /// taken when it has one, else its home in the volume.
  fn home_of(&mut self, store: &Store, no: InodeNo) -> Result<(HostDir, String), VfsError> {
    if let Some((parent, name)) = self
      .vol
      .base
      .as_ref()
      .and_then(|b| b.witness_homes.get(&no).cloned())
    {
      let host_dir = self.listing_dir(parent)?;
      return Ok((host_dir, name.to_string()));
    }
    let inode = self.vol.inode(store, no)?;
    let home = inode.home.ok_or(VfsError::NotOverlay)?;
    let dir = self.vol.current_dir(store, home.parent)?;
    let name = store
      .dirs
      .get(dir)?
      .name_of(&store.blocks, home.hash, no)
      .ok_or(VfsError::NotFound)?
      .to_owned();
    let host_dir = self.listing_dir(home.parent)?;
    Ok((host_dir, name))
  }

  /// Copies an entry up (§4.5): `fstat` the descriptor, apply the racy rule against the
  /// listing's read time, hash the bytes, record the witness; the small class becomes content,
  /// the large class keeps its descriptor. Already witnessed or not base-backed: nothing.
  pub(crate) fn copy_up(
    &mut self,
    store: &mut Store,
    no: InodeNo,
    kind: CopyUp,
  ) -> Result<(), VfsError> {
    let Some(plane) = self.vol.base.as_ref() else {
      return Ok(());
    };
    if plane.is_witnessed(no) {
      return Ok(());
    }
    let is_base = matches!(self.vol.inode(store, no)?.body, Body::Base(_));
    if !is_base {
      return Ok(());
    }
    let (host_dir, name) = self.home_of(store, no)?;
    let file = self.host.open_file(host_dir, &name).map_err(host_refusal)?;
    let fp = self.host.fstat(file).map_err(host_refusal)?;
    let home_parent = self.vol.inode(store, no)?.home.map(|h| h.parent);
    let read_at = home_parent
      .and_then(|p| {
        self
          .vol
          .base
          .as_ref()?
          .listings
          .get(&p)
          .map(|l| l.read_at_ns)
      })
      .unwrap_or(0);
    let granularity = i128::from(self.granularity());
    let racy = i128::from(read_at) - fp.mtime_ns <= granularity;
    let bytes = self.read_whole(file, fp.size)?;
    let identity = *blake3::hash(&bytes).as_bytes();
    let witness = Witness {
      fingerprint: fp,
      identity,
      witnessed_at: self.vol.clock.monotonic_ns(),
      racy,
    };
    let large = fp.size
      > self
        .vol
        .base
        .as_ref()
        .map_or(u64::MAX, |b| b.large_class_bytes);
    let handle = self.vol.make_current_inode(store, no)?;
    let prev = store.inodes.get(handle)?.version;
    {
      let inode = store.inodes.get_mut(handle)?;
      inode.attrs.size = fp.size;
      inode.attrs.mode = fp.mode;
      inode.attrs.mtime = fp.mtime_ns;
      inode.attrs.ctime = fp.ctime_ns;
      if let Body::Base(b) = &mut inode.body {
        b.witness = Some(witness);
        b.base_len = fp.size;
      }
    }
    let plane = self.plane()?;
    plane.witnesses.insert(no, witness);
    if let Some(home) = home_parent {
      plane.witness_homes.insert(no, (home, name.clone().into()));
    }
    if kind == CopyUp::Content && !large {
      // The small class: the bytes come in whole and the body becomes plain content.
      self.host.close_file(file);
      self.plane()?.descriptors.remove(&no);
      let handle = self.vol.make_current_inode(store, no)?;
      let charge = self.vol.write_charge(store, no, 0, fp.size)?;
      if !self.vol.quota.admit(self.vol.bytes.total(), charge) {
        return Err(VfsError::NoSpace);
      }
      let before = crate::volume::content_by_epoch(store, handle);
      store.inodes.get_mut(handle)?.body = Body::Inline(Vec::new());
      self.vol.reconcile(before, Vec::new());
      if !bytes.is_empty() {
        self.vol.apply_write(store, handle, 0, &bytes)?;
      }
    } else {
      self.plane()?.descriptors.insert(no, file);
    }
    let path = self.vol.path_of_inode(store, no).unwrap_or_default();
    self.vol.record(Op::Witness, &path, Some(no), prev);
    Ok(())
  }

  fn read_whole(&mut self, file: HostFile, size: u64) -> Result<Vec<u8>, VfsError> {
    let mut bytes = vec![0u8; usize::try_from(size).map_err(|_| VfsError::FileTooLarge)?];
    let mut done = 0usize;
    while done < bytes.len() {
      let n = self
        .host
        .read_at(file, u64::try_from(done).unwrap_or(0), &mut bytes[done..])
        .map_err(host_refusal)?;
      if n == 0 {
        break;
      }
      done += n;
    }
    bytes.truncate(done);
    Ok(bytes)
  }

  /// Pins the chunk windows of `[off, end)` of a large-class base body: each window not yet
  /// pinned is read from disk (after the drift check) into an extent of the volume's own.
  fn pin_windows(
    &mut self,
    store: &mut Store,
    no: InodeNo,
    off: u64,
    end: u64,
  ) -> Result<(), VfsError> {
    let (base_len, pinned): (u64, Vec<u64>) = match &self.vol.inode(store, no)?.body {
      Body::Base(b) => (b.base_len, b.pinned.iter().map(|e| e.off).collect()),
      _ => return Ok(()),
    };
    let chunk = u64::try_from(store.content.chunk_bytes()).unwrap_or(u64::MAX);
    let first = off / chunk;
    let last = end.saturating_sub(1) / chunk;
    if off >= end {
      return Ok(());
    }
    self.check_drift(store, no)?;
    let file = self.descriptor(store, no)?;
    for window in first..=last {
      let start = window * chunk;
      if start >= base_len || pinned.contains(&start) {
        continue;
      }
      let len = usize::try_from((base_len - start).min(chunk)).unwrap_or(0);
      let mut bytes = vec![0u8; len];
      let mut done = 0usize;
      while done < len {
        let n = self
          .host
          .read_at(
            file,
            start + u64::try_from(done).unwrap_or(0),
            &mut bytes[done..],
          )
          .map_err(host_refusal)?;
        if n == 0 {
          break;
        }
        done += n;
      }
      bytes.truncate(done);
      let charge = u64::try_from(bytes.len()).unwrap_or(0);
      if !self.vol.quota.admit(self.vol.bytes.total(), charge) {
        return Err(VfsError::NoSpace);
      }
      let handle = self.vol.make_current_inode(store, no)?;
      let before = crate::volume::content_by_epoch(store, handle);
      let epoch = self.vol.epoch;
      let mut open = store.content.open(start, bytes.len(), epoch)?;
      store.content.write_open(&mut open, 0, &bytes)?;
      if let Some(extent) = store.content.seal(open)?
        && let Body::Base(b) = &mut store.inodes.get_mut(handle)?.body
      {
        crate::volume::insert_extent(&mut b.pinned, extent);
      }
      self
        .vol
        .reconcile(before, crate::volume::content_by_epoch(store, handle));
    }
    Ok(())
  }

  // ---------------------------------------------------------------- drift

  /// Re-checks a witnessed entry against the disk (§4.5): what the held descriptor serves is
  /// compared first, and an in-place change there marks the body `lost` and refuses reads with
  /// `BaseDrift` (the bytes would be torn); then the path is compared, and an entry deleted,
  /// replaced or retyped there is recorded as drift while the descriptor keeps serving the
  /// witnessed inode (its data is alive). Each drift is journaled once.
  pub(crate) fn check_drift(&mut self, store: &mut Store, no: InodeNo) -> Result<(), VfsError> {
    let Some(witness) = self.vol.base.as_ref().and_then(|b| b.witness(no)) else {
      return Ok(());
    };
    let lost = matches!(&self.vol.inode(store, no)?.body, Body::Base(b) if b.lost);
    if lost {
      return Err(VfsError::BaseDrift);
    }
    // 1. The inode the descriptor serves.
    if let Some(file) = self
      .vol
      .base
      .as_ref()
      .and_then(|b| b.descriptors.get(&no).copied())
    {
      let served = self.host.fstat(file).map_err(host_refusal)?;
      let same = served == witness.fingerprint
        && (!witness.racy || self.identity_of(file, served.size)? == witness.identity);
      if !same {
        return Err(self.mark_lost(store, no));
      }
    }
    // 2. The path on the disk.
    let at_path = match self.home_of(store, no) {
      Ok((dir, name)) => match self.host.open_file(dir, &name) {
        Ok(file) => {
          let fp = self.host.fstat(file);
          self.host.close_file(file);
          Some(fp.map_err(host_refusal)?)
        }
        Err(HostError::NotFound) => None,
        Err(HostError::NotFile | HostError::NotDirectory) => {
          self.record_drift(store, no, DriftKind::TypeChanged);
          return Ok(());
        }
        Err(e) => return Err(host_refusal(e)),
      },
      Err(_) => None,
    };
    match at_path {
      None => self.record_drift(store, no, DriftKind::Deleted),
      Some(fp) if fp.ino != witness.fingerprint.ino => {
        self.record_drift(store, no, DriftKind::Replaced)
      }
      Some(fp) if fp != witness.fingerprint => {
        // The same inode changed in place; a held descriptor would have seen it above, so
        // this is a small-class entry whose bytes are safe in memory: drift, not a loss.
        self.record_drift(store, no, DriftKind::Modified);
      }
      Some(_) if witness.racy => {
        if self.identity_now(store, no)? != Some(witness.identity) {
          self.record_drift(store, no, DriftKind::Modified);
        }
      }
      Some(_) => {}
    }
    Ok(())
  }

  /// The BLAKE3 of an open file's bytes.
  fn identity_of(&mut self, file: HostFile, size: u64) -> Result<[u8; 32], VfsError> {
    let bytes = self.read_whole(file, size)?;
    Ok(*blake3::hash(&bytes).as_bytes())
  }

  /// Records a drift of `kind` for the inode (once per kind) and journals it.
  fn record_drift(&mut self, store: &mut Store, no: InodeNo, kind: DriftKind) {
    let fresh = self
      .vol
      .base
      .as_mut()
      .is_some_and(|b| b.drift.insert(no, kind) != Some(kind));
    if fresh {
      let path = self.vol.path_of_inode(store, no).unwrap_or_default();
      self.vol.record(Op::Drift, &path, Some(no), 0);
    }
  }

  /// The descriptor's inode changed in place: the unpinned bytes are gone; the body is lost
  /// and every read of it refuses.
  fn mark_lost(&mut self, store: &mut Store, no: InodeNo) -> VfsError {
    self.record_drift(store, no, DriftKind::Modified);
    if let Ok(handle) = self.vol.make_current_inode(store, no)
      && let Ok(inode) = store.inodes.get_mut(handle)
      && let Body::Base(b) = &mut inode.body
    {
      b.lost = true;
    }
    VfsError::BaseDrift
  }

  /// The BLAKE3 of the entry's bytes as the disk holds them now.
  fn identity_now(&mut self, store: &Store, no: InodeNo) -> Result<Option<[u8; 32]>, VfsError> {
    let Ok((dir, name)) = self.home_of(store, no) else {
      return Ok(None);
    };
    let file = self.host.open_file(dir, &name).map_err(host_refusal)?;
    let fp = self.host.fstat(file).map_err(host_refusal);
    let bytes = fp.and_then(|fp| self.read_whole(file, fp.size));
    self.host.close_file(file);
    Ok(Some(*blake3::hash(&bytes?).as_bytes()))
  }

  /// `status`: every witnessed entry re-checked now, the drift list by path, the watcher.
  pub fn status(&mut self, store: &mut Store) -> Result<BaseStatus, VfsError> {
    self.process_hints(store)?;
    let witnessed: Vec<InodeNo> = self
      .vol
      .base
      .as_ref()
      .map(|b| b.witnesses.keys().copied().collect())
      .unwrap_or_default();
    for no in witnessed {
      let _ = self.check_drift(store, no);
    }
    let plane = self.vol.base.as_ref().ok_or(VfsError::NotOverlay)?;
    let mut drift: Vec<(String, DriftKind)> = plane
      .drift
      .iter()
      .map(|(no, kind)| {
        (
          self
            .vol
            .path_of_inode(store, *no)
            .unwrap_or_else(|| format!("inode {}", no.0)),
          *kind,
        )
      })
      .collect();
    drift.sort();
    Ok(BaseStatus {
      drift,
      watcher: plane.watch,
    })
  }

  /// Drains the watcher's hints: a changed directory invalidates its listing and re-checks
  /// the witnessed entries homed there; an overflow invalidates everything and re-checks all.
  pub fn process_hints(&mut self, store: &mut Store) -> Result<(), VfsError> {
    let hints = self.host.hints();
    let plane = self.vol.base.as_mut().ok_or(VfsError::NotOverlay)?;
    for hint in hints {
      match hint {
        Hint::Changed(dir) => {
          let hit = plane
            .listings
            .iter()
            .find(|(_, l)| l.dir == dir)
            .map(|(no, _)| *no);
          if let Some(no) = hit {
            if let Some(l) = plane.listings.get_mut(&no) {
              l.entries = None;
            }
            plane.recheck.insert(no);
          }
        }
        Hint::Overflow => {
          plane.watch = WatchState::Overflowed;
          for l in plane.listings.values_mut() {
            l.entries = None;
          }
          plane.recheck_all = true;
        }
      }
    }
    let all = std::mem::take(&mut plane.recheck_all);
    let dirs = std::mem::take(&mut plane.recheck);
    let witnessed: Vec<InodeNo> = plane.witnesses.keys().copied().collect();
    let targets: Vec<InodeNo> = witnessed
      .into_iter()
      .filter(|no| {
        all
          || self
            .vol
            .inode(store, *no)
            .ok()
            .and_then(|i| i.home)
            .is_some_and(|h| dirs.contains(&h.parent))
      })
      .collect();
    for no in targets {
      let _ = self.check_drift(store, no);
    }
    Ok(())
  }

  // ---------------------------------------------------------------- verbs

  /// `read_base`: the entry as the disk holds it right now, through the base's directories by
  /// name; a read, never a write.
  pub fn read_base(&mut self, path: &str) -> Result<Vec<u8>, VfsError> {
    let root = self.vol.base.as_ref().ok_or(VfsError::NotOverlay)?.root;
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let Some((name, dirs)) = parts.split_last() else {
      return Err(VfsError::IsDirectory);
    };
    let mut opened = Vec::new();
    let mut dir = root;
    let result = (|| {
      for d in dirs {
        dir = self.host.open_dir(dir, d).map_err(host_refusal)?;
        opened.push(dir);
      }
      let file = self.host.open_file(dir, name).map_err(host_refusal)?;
      let fp = self.host.fstat(file).map_err(host_refusal);
      let bytes = fp.and_then(|fp| self.read_whole(file, fp.size));
      self.host.close_file(file);
      bytes
    })();
    for d in opened {
      self.host.close_dir(d);
    }
    result
  }

  /// `rewitness`: re-witness the named drifted entries (all drifted when none are named) to
  /// the disk as it is now; the volume's content is untouched; their drift records clear.
  pub fn rewitness(
    &mut self,
    store: &mut Store,
    paths: Option<&[String]>,
  ) -> Result<Vec<String>, VfsError> {
    let targets: Vec<InodeNo> = match paths {
      Some(paths) => {
        let mut v = Vec::new();
        for p in paths {
          v.push(self.resolve(store, p)?.inode);
        }
        v
      }
      None => self
        .vol
        .base
        .as_ref()
        .map(|b| b.drift.keys().copied().collect())
        .unwrap_or_default(),
    };
    let mut done = Vec::new();
    for no in targets {
      if let Some(f) = self.plane()?.descriptors.remove(&no) {
        self.host.close_file(f);
      }
      let (dir, name) = self.home_of(store, no)?;
      let file = self.host.open_file(dir, &name).map_err(host_refusal)?;
      let fp = self.host.fstat(file).map_err(host_refusal)?;
      let bytes = self.read_whole(file, fp.size)?;
      let witness = Witness {
        fingerprint: fp,
        identity: *blake3::hash(&bytes).as_bytes(),
        witnessed_at: self.vol.clock.monotonic_ns(),
        racy: false,
      };
      let plane = self.plane()?;
      plane.witnesses.insert(no, witness);
      plane.drift.remove(&no);
      let handle = self.vol.make_current_inode(store, no)?;
      let prev = store.inodes.get(handle)?.version;
      let keep_descriptor = if let Body::Base(b) = &mut store.inodes.get_mut(handle)?.body {
        b.witness = Some(witness);
        b.lost = false;
        b.base_len = fp.size;
        true
      } else {
        false
      };
      if keep_descriptor {
        self.plane()?.descriptors.insert(no, file);
      } else {
        self.host.close_file(file);
      }
      let path = self.vol.path_of_inode(store, no).unwrap_or_default();
      self.vol.record(Op::Witness, &path, Some(no), prev);
      done.push(path);
    }
    Ok(done)
  }

  /// `pin`: read the named subtrees (the whole base when none are named) into the store and
  /// witness them; cost proportional to what is pinned, reported as the entry count.
  pub fn pin(&mut self, store: &mut Store, paths: Option<&[String]>) -> Result<usize, VfsError> {
    let roots: Vec<String> = paths.map_or_else(|| vec!["/".to_owned()], <[String]>::to_vec);
    let mut pinned = 0usize;
    for path in roots {
      let located = self.resolve(store, &path)?;
      let mut stack = vec![located];
      while let Some(l) = stack.pop() {
        match l.child {
          Child::Dir(h) => {
            let rows: Vec<(Kind, InodeNo, String)> = self
              .readdir(store, h)?
              .iter()
              .map(|r| (r.kind, r.inode, r.name.to_owned()))
              .collect();
            let h = self.vol.head_dir(store, h)?;
            for (kind, no, name) in rows {
              let child = match kind {
                Kind::Dir => match self.vol.lookup(store, h, &name)?.child {
                  Child::Dir(d) => Child::Dir(d),
                  other => other,
                },
                Kind::File => Child::File(no),
                Kind::Symlink => Child::Symlink(no),
              };
              stack.push(Located { child, inode: no });
            }
          }
          Child::File(no) => {
            self.copy_up(store, no, CopyUp::Content)?;
            let size = self.vol.inode(store, no)?.attrs.size;
            self.pin_windows(store, no, 0, size)?;
            pinned += 1;
          }
          Child::Symlink(_) | Child::Whiteout => {}
        }
      }
    }
    Ok(pinned)
  }
}
