//! The base plane of an overlay volume (§4.5, §4.15, D-25; Phase 1 task 10): a host directory
//! serves every untouched entry on demand through the read-only seam ([`crate::host`]); the
//! first write copies an entry up and records the witnessed base (the stat fingerprint and the
//! BLAKE3 of the bytes the edit was based on); deletes leave whiteouts; renamed base directories
//! record their origin (a redirect); drift is detected by fingerprints under the racy rule and
//! reported, never absorbed; watcher hints make reports prompt and are never the truth. A clean
//! file — an untouched base entry — exports a verified content digest ([`Overlay::digest`],
//! §4.15): the BLAKE3 of the bytes the disk holds, verified current at the export by the file's
//! identity and fingerprint, refused typed rather than ever stale.
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

#[path = "base_recovery.rs"]
mod recovery;
pub use recovery::BaseImage;

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

/// Format: a directory's own two links (`.` and its name); each subdirectory adds one (POSIX).
const ROOT_LINKS: u32 = 2;

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
  /// The immutable source path below the base root, independent of overlay renames.
  source: Vec<String>,
  fingerprint: Option<Fingerprint>,
  entries: Option<Vec<BaseEntry>>,
  /// The host's clock when the entries were read (`HostFs::now_ns`, the fingerprints' own
  /// domain), for the racy rule (§4.5): a witness is racy when the listing was read within the
  /// timestamp granularity of the file's last change, a comparison that only means something
  /// between two readings of the filesystem's clock.
  read_at_ns: i64,
  watch: WatchState,
}

/// What changed on disk beneath a witnessed entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, slates_wire::Wire)]
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

/// A moved base directory retains its origin; a directory with no live base is created.
/// Scratch directories use `None`, while overlay replacements use `Opaque` (AC-1.10).
fn directory_divergence(node: &DirNode) -> Option<Divergence> {
  if node.origin.is_some() {
    Some(Divergence::Redirect)
  } else if matches!(node.base, BaseDirState::None | BaseDirState::Opaque) {
    Some(Divergence::Created)
  } else {
    None
  }
}

/// One diverged entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diverged {
  /// The path in the volume.
  pub path: String,
  /// Why.
  pub kind: Divergence,
}

/// A clean file's verified content digest (§4.15): the BLAKE3 of the bytes the disk holds for an
/// untouched base entry and their length, verified against the file's identity and fingerprint
/// at the moment of export.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Digest {
  /// BLAKE3 of the file's bytes.
  pub identity: [u8; 32],
  /// The length digested, in bytes.
  pub size: u64,
}

/// A digest hash in progress across cooperative slices (§4.15; clean-digest.md §7 "cooperative
/// slicing"): a large clean base file is hashed a bounded slice per shard step ([`Overlay::
/// digest_advance`]) rather than in one step, so the owner shard is never held for the whole file at
/// the machine's BLAKE3 throughput. The state is **pure data** — the running BLAKE3 hasher, the byte
/// offset reached, the file's inode and the fingerprint captured at [`Overlay::digest_begin`] — and
/// holds no host resource, so a partial digest an abandoning client leaves behind is simply dropped.
/// Each slice re-opens the file and re-checks the fingerprint, so an outsider edit part-way through is
/// caught (`DigestUnverified`), never hashed into a torn identity.
#[derive(Clone, Debug)]
pub struct PartialDigest {
  no: InodeNo,
  fingerprint: Fingerprint,
  size: u64,
  done: u64,
  hasher: blake3::Hasher,
}

impl PartialDigest {
  /// The bytes hashed so far, and the total — a progress fraction for the operator.
  pub fn progress(&self) -> (u64, u64) {
    (self.done, self.size)
  }
}

/// What [`Overlay::digest_begin`] resolved: a digest ready at once (a cache hit, or nothing to
/// re-verify), or a hash to advance in slices.
pub enum DigestStart {
  /// The digest is known now (the reused cache entry).
  Ready(Digest),
  /// A hash to carry forward with [`Overlay::digest_advance`]; boxed, since its running BLAKE3 state
  /// is kilobytes against the ready digest's forty bytes (one allocation per digest begun).
  Pending(Box<PartialDigest>),
}

/// The digest verb's counters (§4.15: "validated by a counter and a byte oracle"): every path
/// the verb and its cache can take, so a test asserts the one it drove moved and a silently dead
/// reuse path can never pass as a working one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DigestStats {
  /// Digests computed by reading and hashing the file.
  pub computed: u64,
  /// Exports served from a kept digest after the disk re-verified its fingerprint (the reuse
  /// path).
  pub revalidated: u64,
  /// Kept digests dropped because the volume was about to mutate, or removed, the entry
  /// (§4.15 "invalidated before any mutation").
  pub invalidated: u64,
  /// Kept digests dropped because the disk no longer matched them (a listing refresh, an export's
  /// re-check, or a watcher hint's revalidation).
  pub stale: u64,
  /// Exports refused `DigestUnverified`: the file changed while it was being digested.
  pub unverified: u64,
  /// Digests not kept because the shard's cache was at its bound (`DigestCacheFull`).
  pub cache_full: u64,
  /// Digests not kept because they were computed inside the racy window (§4.5): a same-tick
  /// write could change the bytes without moving the fingerprint.
  pub racy_uncached: u64,
  /// Kept digests re-verified against the disk because a watcher hint named their directory
  /// (§4.15 "watcher hints backed by revalidation"); the stale ones count under `stale` too.
  pub hint_rechecked: u64,
  /// Kept digests dropped because the watcher overflowed (§4.15 "watcher overflow invalidates
  /// affected cache knowledge").
  pub dropped_on_overflow: u64,
  /// Digests held now.
  pub cached: u64,
}

/// One kept digest: the directory the entry is homed in (a watcher hint names a directory), the
/// fingerprint the digest was verified under, and the identity of the bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CachedDigest {
  home: InodeNo,
  fingerprint: Fingerprint,
  identity: [u8; 32],
}

/// A cache record as the shard's budget sizes it: the key and the kept digest.
pub(crate) type DigestRecord = (InodeNo, CachedDigest);

/// The divisor between the inode table's bytes and the clean-file digest cache's (§4.15 "bounded
/// cache discovery"). The inode table is a sixth of a shard's reserve (the daemon's
/// `STORE_TABLE_DIVISOR`), so the cache is about one percent of the reserve — a planned
/// optimization's share ("no performance gain is assumed"), to be re-derived from the measured
/// digest hit rate once the fast path it serves is measured.
/// Shape: one sixteenth of the inode table's bytes.
const DIGEST_SHARE_OF_INODE_TABLE: usize = 16;

/// Derived: the digest records a shard may keep — the bytes of a sixteenth of its inode table over
/// one record's size — so the cache is bounded by the same reserve the tables are sized from
/// (§4.2) and never grows with the base tree.
pub fn digest_capacity(max_inodes: usize) -> slates_machine::Derived<usize> {
  slates_machine::derived!(
    max_inodes.saturating_mul(std::mem::size_of::<Inode>())
      / DIGEST_SHARE_OF_INODE_TABLE
      / std::mem::size_of::<DigestRecord>().max(1),
    "max_inodes × size_of::<Inode>() / DIGEST_SHARE_OF_INODE_TABLE / size_of::<DigestRecord>()",
    ["store.max_inodes", "reserve_per_shard"]
  )
}

/// The shard's digest-cache budget (§4.15 "bounded cache discovery", §4.2): one counted capacity
/// every overlay volume on the shard keeps its verified digests under, so the caches together
/// never exceed their derived share. At the bound admission refuses `DigestCacheFull` — the
/// digest is still exported, only not kept — and the plane counts the refusal.
#[derive(Debug)]
pub struct DigestBudget {
  capacity: usize,
  live: usize,
}

impl DigestBudget {
  /// A budget of `capacity` records, none kept.
  pub(crate) fn new(capacity: usize) -> Self {
    Self { capacity, live: 0 }
  }

  /// The records the shard may keep.
  pub fn capacity(&self) -> usize {
    self.capacity
  }

  /// The records kept now, across every volume on the shard.
  pub fn live(&self) -> usize {
    self.live
  }

  /// Takes one record's slot, or refuses at the bound.
  fn take(&mut self) -> Result<(), VfsError> {
    if self.live >= self.capacity {
      return Err(VfsError::DigestCacheFull);
    }
    self.live += 1;
    Ok(())
  }

  /// Returns one record's slot.
  fn give(&mut self) {
    self.live = self.live.saturating_sub(1);
  }
}

/// A live base entry an attached transport must expire from its kernel's cache: a watcher hint
/// said the directory holding it changed beneath the volume, so its name and attributes may no
/// longer be what the kernel holds (§4.6). Only untouched entries — an unwitnessed file or
/// symlink, a merged subdirectory — follow the disk; the volume's own entries are unaffected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StaleBaseEntry {
  /// The directory the entry hangs in.
  pub dir: InodeNo,
  /// The entry's name there.
  pub name: Box<str>,
  /// The entry's inode.
  pub child: InodeNo,
}

/// What a hint left stale for a transport: the hinted directories themselves (their attributes,
/// and a listing cache if the transport keeps one) and the live entries beneath them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StaleBaseEntries {
  /// The hinted directories.
  pub dirs: Vec<InodeNo>,
  /// The live entries beneath them.
  pub entries: Vec<StaleBaseEntry>,
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
  /// The base entry each whiteout hides, by (directory inode, name): its fingerprint at the
  /// removal, the landing's witnessed base for a delete (§4.15).
  whiteouts: BTreeMap<(InodeNo, Box<str>), Fingerprint>,
  /// The base directory each redirect moved, by the directory's inode: its fingerprint at the
  /// rename, the landing's witnessed base for the rename.
  redirects: BTreeMap<InodeNo, Fingerprint>,
  watch: WatchState,
  /// Directories whose listings a hint invalidated and whose witnessed entries want a check.
  recheck: BTreeSet<InodeNo>,
  recheck_all: bool,
  /// The digest verb's counters.
  digest_stats: DigestStats,
  /// Kept digests by the clean file's inode number (§4.15), each holding a slot of the shard's
  /// [`DigestBudget`]; dropped before any mutation of the entry and whenever the disk no longer
  /// matches.
  digests: BTreeMap<InodeNo, CachedDigest>,
  /// The kept digests homed in each directory, so a watcher hint naming a directory revalidates
  /// exactly the digests beneath it.
  digests_by_dir: BTreeMap<InodeNo, BTreeSet<InodeNo>>,
  /// The hint sequence: one more per watcher hint drained, so an attached transport can ask
  /// which directories were hinted since it last delivered kernel invalidations (§4.6 "A drift
  /// report or a watcher hint on a base path invalidates the kernel's entry and attributes").
  hint_seq: u64,
  /// The hint sequence at which each loaded directory's listing was last invalidated by a hint.
  /// Bounded by the listings table: pruned to the loaded directories on every drain.
  stale_since: BTreeMap<InodeNo, u64>,
}

impl BasePlane {
  fn new(config: BaseConfig, root_no: InodeNo) -> Self {
    let mut listings = BTreeMap::new();
    listings.insert(
      root_no,
      Listing {
        dir: config.root,
        source: Vec::new(),
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
      whiteouts: BTreeMap::new(),
      redirects: BTreeMap::new(),
      watch: WatchState::Unavailable,
      recheck: BTreeSet::new(),
      recheck_all: false,
      digest_stats: DigestStats::default(),
      digests: BTreeMap::new(),
      digests_by_dir: BTreeMap::new(),
      hint_seq: 0,
      stale_since: BTreeMap::new(),
    }
  }

  /// The base filesystem's timestamp granularity plus the measured clock resolution (§4.5's
  /// racy window; `HostFacts`): the window inside which a revalidation of a live entry cannot
  /// tell a change apart, hence the longest a transport may cache a live base entry's name and
  /// attributes (§4.6 "Live source names/attributes/content cannot have an indefinite kernel
  /// cache lifetime").
  pub fn timestamp_granularity_ns(&self) -> u64 {
    self.facts.timestamp_granularity_ns.max(1)
  }

  /// The hint sequence now (see [`Overlay::take_stale_base_entries`]).
  pub fn hint_seq(&self) -> u64 {
    self.hint_seq
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
    plane.whiteouts = self.whiteouts.clone();
    plane.redirects = self.redirects.clone();
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

  /// The digest verb's counters, for the tests that must see a path move.
  pub fn digest_stats(&self) -> DigestStats {
    self.digest_stats
  }

  /// Drops an inode's kept digest, returning its slot to the shard; whether one was kept. The
  /// caller counts why.
  fn forget_digest(&mut self, store: &mut Store, no: InodeNo) -> bool {
    let Some(cached) = self.digests.remove(&no) else {
      return false;
    };
    if let Some(homed) = self.digests_by_dir.get_mut(&cached.home) {
      homed.remove(&no);
      if homed.is_empty() {
        self.digests_by_dir.remove(&cached.home);
      }
    }
    store.digests.give();
    self.digest_stats.cached = u64::try_from(self.digests.len()).unwrap_or(u64::MAX);
    true
  }

  /// Keeps a verified digest under a slot of the shard's budget, or refuses `DigestCacheFull` at
  /// the bound; an earlier digest of the inode is replaced, never double-counted.
  fn remember_digest(
    &mut self,
    store: &mut Store,
    no: InodeNo,
    cached: CachedDigest,
  ) -> Result<(), VfsError> {
    self.forget_digest(store, no);
    store.digests.take()?;
    self.digests.insert(no, cached);
    self
      .digests_by_dir
      .entry(cached.home)
      .or_default()
      .insert(no);
    self.digest_stats.cached = u64::try_from(self.digests.len()).unwrap_or(u64::MAX);
    Ok(())
  }

  /// Drops every kept digest (a destroy, a watcher overflow): every slot goes back to the shard;
  /// how many were dropped.
  pub(crate) fn drop_all_digests(&mut self, store: &mut Store) -> u64 {
    let dropped = u64::try_from(self.digests.len()).unwrap_or(u64::MAX);
    for _ in 0..self.digests.len() {
      store.digests.give();
    }
    self.digests.clear();
    self.digests_by_dir.clear();
    self.digest_stats.cached = 0;
    dropped
  }

  /// The names the base listing of directory `no` found, or `None` when the listing has not been
  /// read (the directory still shows the live disk). What [`crate::coverage`] checks a frozen node
  /// against.
  pub(crate) fn listed_names(&self, no: InodeNo) -> Option<Vec<Box<str>>> {
    let entries = self.listings.get(&no)?.entries.as_ref()?;
    Some(entries.iter().map(|entry| entry.name.clone()).collect())
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

  /// Whether inode `no` follows the live disk: an untouched (unwitnessed) base file or symlink, or
  /// a merged directory whose listing is the base's. A transport may cache such an object's name
  /// and attributes only for the base filesystem's timestamp granularity, never indefinitely
  /// (§4.6 "only pinned/immutable views can justify retention without a source check"); every
  /// other object is the volume's own and is invalidated explicitly when it changes.
  pub fn is_live_source(&self, store: &Store, no: InodeNo) -> bool {
    let Some(plane) = self.base.as_ref() else {
      return false;
    };
    match self.inode(store, no).map(|i| &i.body) {
      Ok(Body::Base(_)) => !plane.is_witnessed(no),
      Ok(Body::Directory(dir)) => store
        .dirs
        .get(*dir)
        .is_ok_and(|node| node.base == BaseDirState::Merged),
      _ => false,
    }
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
              if let Some(kind) = directory_divergence(child) {
                out.push(Diverged {
                  path: path.clone(),
                  kind,
                });
              }
              stack.push((path, h));
            }
          }
          Child::File(no) | Child::Symlink(no) | Child::Fifo(no) | Child::Socket(no) => {
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

  /// The fingerprint of the base entry a whiteout at `dir/name` hides, recorded at the
  /// removal (the landing's witnessed base for a delete).
  pub fn whiteout_witness(&self, store: &Store, dir: &str, name: &str) -> Option<Fingerprint> {
    let located = self.resolve(store, dir).ok()?;
    let Child::Dir(h) = located.child else {
      return None;
    };
    let dir_no = store.dirs.get(h).ok()?.inode;
    self
      .base
      .as_ref()?
      .whiteouts
      .get(&(dir_no, name.into()))
      .copied()
  }

  /// The fingerprint of the base directory a redirect at `path` moved, recorded at the rename.
  pub fn redirect_witness(&self, store: &Store, path: &str) -> Option<Fingerprint> {
    let located = self.resolve(store, path).ok()?;
    let Child::Dir(h) = located.child else {
      return None;
    };
    let no = store.dirs.get(h).ok()?.inode;
    self.base.as_ref()?.redirects.get(&no).copied()
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

  /// Closes the descriptor an inode held, when its last link goes; a digest kept for it is
  /// dropped with it (the entry left the volume: invalidated).
  pub(crate) fn base_forget(&mut self, store: &mut Store, no: InodeNo) -> Option<HostFile> {
    let plane = self.base.as_mut()?;
    if plane.forget_digest(store, no) {
      plane.digest_stats.invalidated += 1;
    }
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
    // The read time is taken from the host's clock, never the volume's: the racy rule compares
    // it with the files' timestamps, which live in the host's domain (the daemon's `HostClock`
    // counts from its own creation, so a subtraction across the two domains called every
    // witness racy — docs/bugs/2026-09-14-racy-rule-compares-monotonic-with-wall-clock.md).
    let now = self.host.now_ns();
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
    self.prune_unloaded(store, dir)?;
    self.refresh_dir_nlink(store, dir)
  }

  /// A merged directory's link count is two plus its subdirectories, overlay and base alike
  /// (POSIX); it is known once the listing is, and the core keeps it in step from then on.
  /// Found by the landing oracle (2026-09-05): a base directory materialized with a count of
  /// two lost its last link when its base subdirectory was removed, and its own `rmdir` then
  /// refused `NotFound`.
  fn refresh_dir_nlink(&mut self, store: &mut Store, dir: Handle<DirNode>) -> Result<(), VfsError> {
    let dir = self.vol.head_dir(store, dir)?;
    let policy = self.vol.policy;
    let (dir_no, mut count) = {
      let node = store.dirs.get(dir)?;
      let own = node
        .iter(&store.blocks)
        .filter(|e| matches!(e.child, Child::Dir(_)))
        .count();
      (
        node.inode,
        ROOT_LINKS.saturating_add(u32::try_from(own).unwrap_or(u32::MAX)),
      )
    };
    let base_subdirs: Vec<Box<str>> = self
      .vol
      .base
      .as_ref()
      .and_then(|b| b.listings.get(&dir_no))
      .and_then(|l| l.entries.as_ref())
      .map(|entries| {
        entries
          .iter()
          .filter(|e| e.kind == HostKind::Dir)
          .map(|e| e.name.clone())
          .collect()
      })
      .unwrap_or_default();
    let node = store.dirs.get(dir)?;
    for name in &base_subdirs {
      if node.lookup(&store.blocks, policy, name).is_none() {
        count = count.saturating_add(1);
      }
    }
    let handle = self.vol.make_current_inode(store, dir_no)?;
    store.inodes.get_mut(handle)?.attrs.nlink = count;
    Ok(())
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
        (None, Child::File(no) | Child::Symlink(no) | Child::Fifo(no) | Child::Socket(no))
          if self.unloaded_file(store, no) =>
        {
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
    // Its content, when a snapshot pins it, is retained (§4.2): secured before the entry goes.
    let retention = self.vol.retention_of_drop(store, located.inode)?;
    self.vol.secure_retention(store, retention)?;
    let dropped = self.drop_unloaded_secured(store, dir, name, located);
    self.vol.settle_retention(store);
    dropped
  }

  /// The removal proper, under a secured retention.
  fn drop_unloaded_secured(
    &mut self,
    store: &mut Store,
    dir: Handle<DirNode>,
    name: &str,
    located: crate::volume::Located,
  ) -> Result<(), VfsError> {
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
    // A relist refreshes every untouched entry, changed or not; a kept digest is stale knowledge
    // only when the listing's fingerprint no longer matches the one it was verified under.
    let moved = self
      .plane()?
      .digests
      .get(&no)
      .is_some_and(|kept| kept.fingerprint != fp);
    if moved && self.plane()?.forget_digest(store, no) {
      self.plane()?.digest_stats.stale += 1;
    }
    self.adopt_fingerprint(store, no, fp)
  }

  /// An untouched entry takes the disk's fingerprint: its attributes and its base length.
  fn adopt_fingerprint(
    &mut self,
    store: &mut Store,
    no: InodeNo,
    fp: Fingerprint,
  ) -> Result<(), VfsError> {
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
    // A merged directory's link count is known once its listing is (`refresh_dir_nlink`).
    if let Body::Directory(dir) = self.vol.inode(store, no)?.body
      && store.dirs.get(dir)?.base == BaseDirState::Merged
    {
      self.load_listing(store, dir)?;
    }
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
    let no = self.vol.next_no()?;
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
        let mut source = plane
          .listings
          .get(&parent_no)
          .ok_or(VfsError::RecoveryIncomplete)?
          .source
          .clone();
        source.push(entry.name.to_string());
        plane.listings.insert(
          no,
          Listing {
            dir: opened,
            source,
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
      Child::File(no) | Child::Symlink(no) | Child::Fifo(no) | Child::Socket(no) => {
        self.unloaded_file(store, no)
      }
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

  /// Creates a FIFO/socket name while respecting existing base names (A-26).
  pub fn mknod_no(
    &mut self,
    store: &mut Store,
    dir_no: InodeNo,
    name: &str,
    mode: u32,
    kind: Kind,
  ) -> Result<InodeNo, VfsError> {
    let dir = self.vol.current_dir(store, dir_no)?;
    if self.exists(store, dir, name)? {
      return Err(VfsError::AlreadyExists);
    }
    self.vol.mknod_no(store, dir_no, name, mode, kind)
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
    let base = self.base_entry(store, dir, name)?;
    self.vol.unlink(store, dir, name)?;
    self.remember_whiteout(store, dir, name, base.as_ref());
    Ok(())
  }

  /// Records the fingerprint a whiteout hides, when the removal left one.
  fn remember_whiteout(
    &mut self,
    store: &Store,
    dir: Handle<DirNode>,
    name: &str,
    base: Option<&BaseEntry>,
  ) {
    let Some(entry) = base else { return };
    let Ok(dir) = self.vol.head_dir(store, dir) else {
      return;
    };
    let Ok(node) = store.dirs.get(dir) else {
      return;
    };
    let left = node
      .lookup(&store.blocks, self.vol.policy, name)
      .is_some_and(|e| e.child == Child::Whiteout);
    if left && let Some(plane) = self.vol.base.as_mut() {
      plane
        .whiteouts
        .insert((node.inode, name.into()), entry.fingerprint);
    }
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
    let base = self.base_entry(store, dir, name)?;
    let removed_no = located.inode;
    self.vol.rmdir(store, dir, name)?;
    self.remember_whiteout(store, dir, name, base.as_ref());
    if let Some(plane) = self.vol.base.as_mut() {
      plane.redirects.remove(&removed_no);
      if let Some(l) = plane.listings.remove(&removed_no) {
        self.host.close_dir(l.dir);
      }
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
      (Child::File(no) | Child::Symlink(no) | Child::Fifo(no) | Child::Socket(no), Some(_)) => {
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
    self.remember_whiteout(store, from_dir, from_name, from_base.as_ref());
    if let Some(from) = origin {
      let moved = self.vol.lookup(store, to_dir, to_name)?;
      if let Child::Dir(h) = moved.child {
        let h = self.vol.make_current_dir_node(store, h)?;
        store.dirs.get_mut(h)?.origin = Some(from.clone().into());
        if let (Some(entry), Some(plane)) = (from_base.as_ref(), self.vol.base.as_mut()) {
          plane.redirects.insert(moved.inode, entry.fingerprint);
        }
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

  /// Looks `name` up in the directory named by inode number `dir_no`, serving base entries.
  pub fn lookup_no(
    &mut self,
    store: &mut Store,
    dir_no: InodeNo,
    name: &str,
  ) -> Result<Located, VfsError> {
    let dir = self.vol.current_dir(store, dir_no)?;
    self.lookup(store, dir, name)
  }

  /// The entries of the directory named by inode number `dir_no`, base entries merged.
  pub fn readdir_no<'s>(
    &mut self,
    store: &'s mut Store,
    dir_no: InodeNo,
  ) -> Result<Vec<DirRow<'s>>, VfsError> {
    let dir = self.vol.current_dir(store, dir_no)?;
    self.readdir(store, dir)
  }

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
    // §4.15 "invalidated before any mutation": every mutation of a base entry — content or
    // metadata — copies it up first, so the digest kept for it is dropped here, before the
    // witness is recorded and before the mutation is visible.
    if self.plane()?.forget_digest(store, no) {
      self.plane()?.digest_stats.invalidated += 1;
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
    let granularity = i64::try_from(self.granularity()).unwrap_or(i64::MAX);
    let racy = read_at.saturating_sub(fp.mtime_ns) <= granularity;
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
      if !self
        .vol
        .quota
        .admit(self.vol.bytes.total(), charge, &mut store.budget)
      {
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
    // Only a window that still lives on the disk needs the disk (and the drift check first);
    // bytes the volume already pinned are its own whatever the disk does beneath them.
    let wanted: Vec<u64> = (first..=last)
      .map(|w| w * chunk)
      .filter(|start| *start < base_len && !pinned.contains(start))
      .collect();
    if wanted.is_empty() {
      return Ok(());
    }
    self.check_drift(store, no)?;
    let file = self.descriptor(store, no)?;
    for start in wanted {
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
      if !self
        .vol
        .quota
        .admit(self.vol.bytes.total(), charge, &mut store.budget)
      {
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

  // ---------------------------------------------------------------- digests

  /// `digest` (§4.15): the verified content digest of a clean file — an untouched base entry,
  /// whose bytes are exactly the disk's. The path resolves through the overlay (an absent or
  /// whiteouted name is `NotFound`, a directory `IsDirectory`); an entry the volume diverged, or
  /// a symlink, refuses `DigestNotClean`; then the file is verified current (its listing
  /// validated, the path opened afresh and matched by identity to the descriptor the volume
  /// holds), hashed in bounded windows, and its fingerprint compared again after the read, so a
  /// file changing under the hash refuses `DigestUnverified` rather than export a digest of torn
  /// bytes. A read, never a mutation: nothing is journaled and the entry does not diverge.
  pub fn digest(&mut self, store: &mut Store, path: &str) -> Result<Digest, VfsError> {
    // The convenience form: begin, then hash the whole file in one call (the whole size as the
    // slice budget). The daemon uses the cooperative [`Self::digest_begin`]/[`Self::digest_advance`]
    // split instead, so a large file's hash yields the shard between slices.
    match self.digest_begin(store, path)? {
      DigestStart::Ready(digest) => Ok(digest),
      DigestStart::Pending(mut partial) => loop {
        if let Some(digest) = self.digest_advance(store, &mut partial, u64::MAX)? {
          return Ok(digest);
        }
      },
    }
  }

  /// Begins a clean-file digest (§4.15): resolves the path, refuses a non-clean or non-file entry,
  /// verifies the file is current, and reuses a kept digest whose fingerprint the disk still matches
  /// (`DigestStart::Ready`). Otherwise it returns the hash to advance in cooperative slices
  /// (`DigestStart::Pending`), carrying the fingerprint the slices re-check against — no bytes are
  /// hashed yet, so `begin` itself is O(1).
  pub fn digest_begin(&mut self, store: &mut Store, path: &str) -> Result<DigestStart, VfsError> {
    let located = self.resolve(store, path)?;
    let no = match located.child {
      Child::File(no) => no,
      Child::Dir(_) => return Err(VfsError::IsDirectory),
      Child::Symlink(_) | Child::Fifo(_) | Child::Socket(_) => {
        return Err(VfsError::DigestNotClean);
      }
      Child::Whiteout => return Err(VfsError::NotFound),
    };
    if !self.is_clean(store, no) {
      return Err(VfsError::DigestNotClean);
    }
    let (_file, fingerprint) = self.verify_current(store, no)?;
    // Discovery (§4.15): a kept digest is reused only after the disk re-verified the fingerprint
    // it was computed under — the fingerprint is the truth, the cache never is; a kept digest
    // the disk no longer matches is stale knowledge, dropped before anything else happens.
    let kept = self
      .vol
      .base
      .as_ref()
      .and_then(|b| b.digests.get(&no).copied());
    if let Some(cached) = kept {
      if cached.fingerprint == fingerprint {
        self.plane()?.digest_stats.revalidated += 1;
        return Ok(DigestStart::Ready(Digest {
          identity: cached.identity,
          size: fingerprint.size,
        }));
      }
      if self.plane()?.forget_digest(store, no) {
        self.plane()?.digest_stats.stale += 1;
      }
    }
    Ok(DigestStart::Pending(Box::new(PartialDigest {
      no,
      fingerprint,
      size: fingerprint.size,
      done: 0,
      hasher: blake3::Hasher::new(),
    })))
  }

  /// Advances a [`PartialDigest`] by up to `budget` bytes (§4.15; clean-digest.md §7): re-opens the
  /// file and re-checks its fingerprint — an outsider edit part-way through the hash is
  /// `DigestUnverified`, never hashed into a torn identity — then reads and hashes the next slice.
  /// Returns the finished [`Digest`] once the whole file is hashed (re-verified and kept), or `None`
  /// when more slices remain. A caller loops this with its own per-step budget so the hash of a
  /// large file never holds the shard for the whole file at the machine's BLAKE3 throughput.
  pub fn digest_advance(
    &mut self,
    store: &mut Store,
    partial: &mut PartialDigest,
    budget: u64,
  ) -> Result<Option<Digest>, VfsError> {
    let file = self.reopen_verified(store, partial.no, partial.fingerprint)?;
    self.hash_slice(store, file, partial, budget)?;
    if partial.done < partial.size {
      return Ok(None);
    }
    // The whole file is hashed; re-verify once more against the disk and keep the identity.
    self.reopen_verified(store, partial.no, partial.fingerprint)?;
    let identity = *partial.hasher.finalize().as_bytes();
    self.plane()?.digest_stats.computed += 1;
    self.keep_digest(store, partial.no, partial.fingerprint, identity)?;
    Ok(Some(Digest {
      identity,
      size: partial.size,
    }))
  }

  /// Re-opens the file `no` names and refuses `DigestUnverified` (counted) unless its fingerprint
  /// still equals `expected` — the disk, not the cache, is the authority across the slices of a
  /// cooperative digest. The returned handle is the volume's cached descriptor, owned by the
  /// descriptor cache (invalidated on a base change), never closed by the caller.
  fn reopen_verified(
    &mut self,
    store: &mut Store,
    no: InodeNo,
    expected: Fingerprint,
  ) -> Result<HostFile, VfsError> {
    let (file, fingerprint) = self.verify_current(store, no)?;
    if fingerprint != expected {
      return Err(self.count_digest_refusal(VfsError::DigestUnverified));
    }
    Ok(file)
  }

  /// Reads and hashes up to `budget` bytes of `file` from `partial.done`, updating the hasher and
  /// the offset. A short read before the fingerprinted size is `DigestUnverified` (the file shrank
  /// under the hash). One window per read, so the memory is bounded whatever the budget.
  fn hash_slice(
    &mut self,
    store: &Store,
    file: HostFile,
    partial: &mut PartialDigest,
    budget: u64,
  ) -> Result<(), VfsError> {
    let window = store.content.chunk_bytes().max(1);
    let window_len = u64::try_from(window).unwrap_or(u64::MAX);
    let mut buf = vec![0u8; window];
    let mut hashed_this_slice: u64 = 0;
    while partial.done < partial.size && hashed_this_slice < budget {
      let remaining = partial.size - partial.done;
      let want = usize::try_from(remaining.min(window_len).min(budget - hashed_this_slice))
        .unwrap_or(window)
        .min(window);
      let n = self
        .host
        .read_at(file, partial.done, &mut buf[..want])
        .map_err(host_refusal)?;
      if n == 0 {
        return Err(self.count_digest_refusal(VfsError::DigestUnverified));
      }
      partial.hasher.update(&buf[..n]);
      let n = u64::try_from(n).unwrap_or(u64::MAX);
      partial.done = partial.done.saturating_add(n);
      hashed_this_slice = hashed_this_slice.saturating_add(n);
    }
    Ok(())
  }

  /// Keeps a freshly verified digest for reuse, unless the racy rule (§4.5) forbids it: a digest
  /// computed while the file's timestamp tick is still open — within the filesystem's granularity
  /// of the host's clock — could be silently invalidated by a same-tick write the fingerprint
  /// cannot show, so it is exported but never kept. At the shard's bound the digest is not kept
  /// either; both are counted, neither refuses the export.
  fn keep_digest(
    &mut self,
    store: &mut Store,
    no: InodeNo,
    fingerprint: Fingerprint,
    identity: [u8; 32],
  ) -> Result<(), VfsError> {
    let granularity = i64::try_from(self.granularity()).unwrap_or(i64::MAX);
    let last_change = fingerprint.mtime_ns.max(fingerprint.ctime_ns);
    if self.host.now_ns().saturating_sub(last_change) <= granularity {
      self.plane()?.digest_stats.racy_uncached += 1;
      return Ok(());
    }
    let Some(home) = self.vol.inode(store, no)?.home.map(|h| h.parent) else {
      return Ok(());
    };
    let cached = CachedDigest {
      home,
      fingerprint,
      identity,
    };
    match self.plane()?.remember_digest(store, no, cached) {
      Ok(()) => Ok(()),
      Err(VfsError::DigestCacheFull) => {
        self.plane()?.digest_stats.cache_full += 1;
        Ok(())
      }
      Err(other) => Err(other),
    }
  }

  /// Whether an entry is clean (§4.15): an untouched base file, so its bytes are exactly the
  /// disk's — the complement of the diverged set for files: base-backed and unwitnessed.
  fn is_clean(&self, store: &Store, no: InodeNo) -> bool {
    let witnessed = self.vol.base.as_ref().is_some_and(|b| b.is_witnessed(no));
    !witnessed
      && matches!(
        self.vol.inode(store, no).map(|i| &i.body),
        Ok(Body::Base(_))
      )
  }

  /// The descriptor and fingerprint of a clean file as the disk holds it right now (§4.15
  /// "verified current"). The directory's listing is validated first (`follow_live_disk`); then
  /// the path is opened afresh and its identity compared with the descriptor the volume holds,
  /// because a file replaced beneath a held descriptor inside the directory's timestamp
  /// granularity leaves the listing's fingerprint unchanged and the old inode alive behind the
  /// descriptor — the one case the listing cannot tell. Two `fstat`s of one inode that disagree
  /// mean it is changing now.
  fn verify_current(
    &mut self,
    store: &mut Store,
    no: InodeNo,
  ) -> Result<(HostFile, Fingerprint), VfsError> {
    self.follow_live_disk(store, no)?;
    let (dir, name) = self.home_of(store, no)?;
    let fresh = self.host.open_file(dir, &name).map_err(host_refusal)?;
    let at_path = match self.host.fstat(fresh) {
      Ok(fp) => fp,
      Err(e) => {
        self.host.close_file(fresh);
        return Err(host_refusal(e));
      }
    };
    let held = self
      .vol
      .base
      .as_ref()
      .and_then(|b| b.descriptors.get(&no).copied());
    let Some(held) = held else {
      self.plane()?.descriptors.insert(no, fresh);
      return Ok((fresh, at_path));
    };
    let served = match self.host.fstat(held) {
      Ok(fp) => fp,
      Err(e) => {
        self.host.close_file(fresh);
        return Err(host_refusal(e));
      }
    };
    if (served.dev, served.ino) != (at_path.dev, at_path.ino) {
      // The path holds another inode now: the descriptor serves a file the disk has replaced.
      // An untouched entry shows the live disk, so the volume adopts the new inode and rereads
      // the directory at its next use (its listing still names the old fingerprint).
      self.host.close_file(held);
      self.plane()?.descriptors.insert(no, fresh);
      if self.plane()?.forget_digest(store, no) {
        self.plane()?.digest_stats.stale += 1;
      }
      self.adopt_fingerprint(store, no, at_path)?;
      self.invalidate_listing_of(store, no);
      return Ok((fresh, at_path));
    }
    self.host.close_file(fresh);
    if served != at_path {
      return Err(self.count_digest_refusal(VfsError::DigestUnverified));
    }
    Ok((held, served))
  }

  /// Marks the listing of an entry's home directory for a reread at its next use.
  fn invalidate_listing_of(&mut self, store: &Store, no: InodeNo) {
    let Some(parent) = self
      .vol
      .inode(store, no)
      .ok()
      .and_then(|i| i.home)
      .map(|h| h.parent)
    else {
      return;
    };
    if let Some(l) = self
      .vol
      .base
      .as_mut()
      .and_then(|b| b.listings.get_mut(&parent))
    {
      l.entries = None;
    }
  }

  /// Counts a digest refusal on the path it names and hands it back.
  fn count_digest_refusal(&mut self, refusal: VfsError) -> VfsError {
    if let Some(plane) = self.vol.base.as_mut()
      && refusal == VfsError::DigestUnverified
    {
      plane.digest_stats.unverified += 1;
    }
    refusal
  }

  /// A watcher hint named a directory: every digest kept for a file homed there is re-verified
  /// against the disk now — the hint triggers the check, the fingerprint decides (§4.15 "watcher
  /// hints backed by revalidation"; "hints alone never prove a source unchanged") — and one the
  /// disk no longer matches is dropped as stale. Bounded by the digests kept beneath the
  /// directory, which the shard's budget bounds.
  fn revalidate_digests_under(&mut self, store: &mut Store, dir_no: InodeNo) {
    let kept: Vec<InodeNo> = self
      .vol
      .base
      .as_ref()
      .and_then(|b| b.digests_by_dir.get(&dir_no))
      .map(|homed| homed.iter().copied().collect())
      .unwrap_or_default();
    for no in kept {
      let Some(cached) = self
        .vol
        .base
        .as_ref()
        .and_then(|b| b.digests.get(&no).copied())
      else {
        continue;
      };
      let now = self.fingerprint_at_path(store, no);
      let Some(plane) = self.vol.base.as_mut() else {
        return;
      };
      plane.digest_stats.hint_rechecked += 1;
      if now != Some(cached.fingerprint) && plane.forget_digest(store, no) {
        plane.digest_stats.stale += 1;
      }
    }
  }

  /// The fingerprint of the entry at its disk path right now, or `None` when the path no longer
  /// holds a file the host can open.
  fn fingerprint_at_path(&mut self, store: &Store, no: InodeNo) -> Option<Fingerprint> {
    let (dir, name) = self.home_of(store, no).ok()?;
    let file = self.host.open_file(dir, &name).ok()?;
    let fingerprint = self.host.fstat(file).ok();
    self.host.close_file(file);
    fingerprint
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

  /// Drains the watcher's hints: a changed directory invalidates its listing, marks it stale for
  /// every attached transport's kernel cache (see [`Overlay::take_stale_base_entries`]),
  /// re-checks the witnessed entries homed there and re-verifies the digests kept beneath it
  /// (§4.15); an overflow does all of these for every loaded directory and drops every kept
  /// digest.
  pub fn process_hints(&mut self, store: &mut Store) -> Result<(), VfsError> {
    let hints = self.host.hints();
    let plane = self.vol.base.as_mut().ok_or(VfsError::NotOverlay)?;
    if !hints.is_empty() {
      // The stale marks are bounded by the loaded directories: a mark for a directory whose
      // listing is gone is dropped before new ones are added.
      let loaded = &plane.listings;
      plane.stale_since.retain(|no, _| loaded.contains_key(no));
    }
    for hint in hints {
      plane.hint_seq = plane.hint_seq.saturating_add(1);
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
            plane.stale_since.insert(no, plane.hint_seq);
          }
        }
        Hint::Overflow => {
          plane.watch = WatchState::Overflowed;
          let seq = plane.hint_seq;
          for (no, l) in &mut plane.listings {
            l.entries = None;
            plane.stale_since.insert(*no, seq);
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
    if all {
      let dropped = self.plane()?.drop_all_digests(store);
      self.plane()?.digest_stats.dropped_on_overflow += dropped;
    } else {
      for dir_no in dirs {
        self.revalidate_digests_under(store, dir_no);
      }
    }
    Ok(())
  }

  // ---------------------------------------------------------------- landing

  /// After a landing (§4.15 step 9): every path that landed leaves the overlay. A written file
  /// becomes an untouched base entry again (the disk holds it now; the next read comes from
  /// there), a landed whiteout or opaque marker is forgotten, a landed redirect clears its
  /// origin. A scratch volume becomes an overlay over the target (`Base::Path`) first.
  pub fn land_advance(
    &mut self,
    store: &mut Store,
    landed: &[String],
    base: Option<BaseConfig>,
  ) -> Result<(), VfsError> {
    if self.vol.base.is_none() {
      let base = base.ok_or(VfsError::NotOverlay)?;
      let root = self.vol.root();
      store.dirs.get_mut(root)?.base = BaseDirState::Merged;
      let root_no = store.dirs.get(root)?.inode;
      self.vol.base = Some(BasePlane::new(base, root_no));
    }
    for path in landed {
      self.forget_landed(store, path)?;
    }
    // Every listing is stale: the landing changed the directories beneath.
    if let Some(plane) = self.vol.base.as_mut() {
      for l in plane.listings.values_mut() {
        l.entries = None;
      }
    }
    Ok(())
  }

  /// One landed path leaves the overlay.
  fn forget_landed(&mut self, store: &mut Store, path: &str) -> Result<(), VfsError> {
    let (dir_path, name) = match path.rfind('/') {
      Some(0) => ("/", &path[1..]),
      Some(i) => (&path[..i], &path[i + 1..]),
      None => ("/", path),
    };
    let Ok(dir_located) = self.vol.resolve(store, dir_path) else {
      return Ok(());
    };
    let Child::Dir(dir) = dir_located.child else {
      return Ok(());
    };
    let dir = self.vol.head_dir(store, dir)?;
    let dir_no = store.dirs.get(dir)?.inode;
    let Some(entry) = store
      .dirs
      .get(dir)?
      .lookup(&store.blocks, self.vol.policy, name)
    else {
      return Ok(());
    };
    match entry.child {
      Child::Whiteout => {
        // The base no longer has the name: nothing to hide.
        self.drop_entry(store, dir, name)?;
        if let Some(plane) = self.vol.base.as_mut() {
          plane.whiteouts.remove(&(dir_no, name.into()));
        }
      }
      Child::Dir(h) => {
        let h = self.vol.make_current_dir_node(store, h)?;
        let node = store.dirs.get_mut(h)?;
        node.origin = None;
        node.base = BaseDirState::Merged;
        let no = node.inode;
        if let Some(plane) = self.vol.base.as_mut() {
          plane.redirects.remove(&no);
          plane.whiteouts.remove(&(dir_no, name.into()));
          // The directory is on the disk now: its listing is read from there.
          if !plane.listings.contains_key(&no)
            && let Some(parent) = plane.listings.get(&dir_no).map(|l| l.dir)
            && let Ok(opened) = self.host.open_dir(parent, name)
          {
            let mut source = plane
              .listings
              .get(&dir_no)
              .ok_or(VfsError::RecoveryIncomplete)?
              .source
              .clone();
            source.push(name.to_owned());
            plane.listings.insert(
              no,
              Listing {
                dir: opened,
                source,
                fingerprint: None,
                entries: None,
                read_at_ns: 0,
                watch: WatchState::Unavailable,
              },
            );
          }
        }
      }
      Child::File(no) | Child::Symlink(no) | Child::Fifo(no) | Child::Socket(no) => {
        // The disk holds the bytes: the entry leaves the overlay. The next lookup reloads it
        // from the listing as an untouched base entry, and its cached bytes go with it — retained
        // (§4.2) when a snapshot still pins them, secured before anything changes.
        let retention = self.vol.retention_of_drop(store, no)?;
        self.vol.secure_retention(store, retention)?;
        if let Some(f) = self.vol.base_forget(store, no) {
          self.host.close_file(f);
        }
        let dropped = self
          .drop_entry(store, dir, name)
          .and_then(|()| self.vol.drop_link(store, no));
        self.vol.settle_retention(store);
        dropped?;
      }
    }
    Ok(())
  }

  /// Removes an entry from a node without a whiteout or a journal record.
  fn drop_entry(
    &mut self,
    store: &mut Store,
    dir: Handle<DirNode>,
    name: &str,
  ) -> Result<(), VfsError> {
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
    self.vol.retire_blocks(store, retired)
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
                Kind::Fifo => Child::Fifo(no),
                Kind::Socket => Child::Socket(no),
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
          Child::Symlink(_) | Child::Fifo(_) | Child::Socket(_) | Child::Whiteout => {}
        }
      }
    }
    Ok(pinned)
  }

  // ------------------------------------------------- metadata copy-ups and by-inode verbs

  /// A read-only open of an untouched base file is answered from the daemon's descriptor on the
  /// backing file (§4.6 "Base files"): the descriptor is opened now, through the entry's home,
  /// so it keeps the inode's data alive if the name is unlinked or renamed over while the file is
  /// open — unlink-while-open on a base file serves the bytes the opener saw, never `ENOENT`.
  /// A witnessed, copied-up or non-base inode needs nothing here.
  pub fn open_base(&mut self, store: &mut Store, no: InodeNo) -> Result<(), VfsError> {
    let untouched = matches!(self.vol.inode(store, no)?.body, Body::Base(_))
      && !self.vol.base.as_ref().is_some_and(|b| b.is_witnessed(no));
    if !untouched {
      return Ok(());
    }
    self.descriptor(store, no).map(|_| ())
  }

  /// `chown` with a metadata-only copy-up: the witness is recorded and nothing is pinned (§4.5
  /// "Metadata-only changes copy up the witness and pin nothing"), so the landing has the base
  /// the ownership change was made against.
  pub fn chown(
    &mut self,
    store: &mut Store,
    no: InodeNo,
    uid: u32,
    gid: u32,
  ) -> Result<(), VfsError> {
    self.copy_up(store, no, CopyUp::Metadata)?;
    self.vol.chown(store, no, uid, gid)
  }

  /// `set_times` with a metadata-only copy-up (the same rule as [`Overlay::chown`]).
  pub fn set_times(
    &mut self,
    store: &mut Store,
    no: InodeNo,
    atime: Option<i64>,
    mtime: Option<i64>,
    ctime: Option<i64>,
  ) -> Result<(), VfsError> {
    self.copy_up(store, no, CopyUp::Metadata)?;
    self.vol.set_times(store, no, atime, mtime, ctime)
  }

  /// The by-inode-number forms of the base-aware namespace verbs, for the bridge (which speaks
  /// inode numbers, not handles — the same shape as the plain `Volume::*_no` wrappers). Every
  /// metadata mutation of a merged directory goes through these, never the plain verbs, so a
  /// base name gets its whiteout (with the listing reloaded first), a base entry gets its witness,
  /// and a name the base holds is refused (§4.6 "Base lookups and metadata mutations go through the
  /// same overlay rules as reads and writes"; AC-1.17).
  pub fn create_file_no(
    &mut self,
    store: &mut Store,
    dir_no: InodeNo,
    name: &str,
    mode: u32,
  ) -> Result<InodeNo, VfsError> {
    let dir = self.vol.current_dir(store, dir_no)?;
    self.create_file(store, dir, name, mode)
  }

  /// `mkdir` by inode number; the new directory's inode number (see [`Overlay::create_file_no`]).
  pub fn mkdir_no(
    &mut self,
    store: &mut Store,
    dir_no: InodeNo,
    name: &str,
    mode: u32,
  ) -> Result<InodeNo, VfsError> {
    let dir = self.vol.current_dir(store, dir_no)?;
    let handle = self.mkdir(store, dir, name, mode)?;
    Ok(store.dirs.get(handle)?.inode)
  }

  /// `symlink` by inode number (see [`Overlay::create_file_no`]).
  pub fn symlink_no(
    &mut self,
    store: &mut Store,
    dir_no: InodeNo,
    name: &str,
    target: &str,
  ) -> Result<InodeNo, VfsError> {
    let dir = self.vol.current_dir(store, dir_no)?;
    self.symlink(store, dir, name, target)
  }

  /// `link` by inode number (see [`Overlay::create_file_no`]).
  pub fn link_no(
    &mut self,
    store: &mut Store,
    dir_no: InodeNo,
    name: &str,
    target: InodeNo,
  ) -> Result<(), VfsError> {
    let dir = self.vol.current_dir(store, dir_no)?;
    self.link(store, dir, name, target)
  }

  /// `unlink` by inode number (see [`Overlay::create_file_no`]).
  pub fn unlink_no(
    &mut self,
    store: &mut Store,
    dir_no: InodeNo,
    name: &str,
  ) -> Result<(), VfsError> {
    let dir = self.vol.current_dir(store, dir_no)?;
    self.unlink(store, dir, name)
  }

  /// `rmdir` by inode number (see [`Overlay::create_file_no`]).
  pub fn rmdir_no(
    &mut self,
    store: &mut Store,
    dir_no: InodeNo,
    name: &str,
  ) -> Result<(), VfsError> {
    let dir = self.vol.current_dir(store, dir_no)?;
    self.rmdir(store, dir, name)
  }

  /// `rename` by inode numbers (see [`Overlay::create_file_no`]).
  pub fn rename_no(
    &mut self,
    store: &mut Store,
    from_dir_no: InodeNo,
    from_name: &str,
    to_dir_no: InodeNo,
    to_name: &str,
  ) -> Result<(), VfsError> {
    let from = self.vol.current_dir(store, from_dir_no)?;
    let to = self.vol.current_dir(store, to_dir_no)?;
    self.rename(store, from, from_name, to, to_name)
  }

  // ---------------------------------------------------------------- kernel coherence

  /// The directories a watcher hint invalidated since hint sequence `since`, with the live
  /// entries beneath them, for a transport to expire from its kernel's cache before the daemon
  /// answers another request (§4.6). Hints are drained first ([`Overlay::process_hints`]). Only
  /// entries the kernel can hold are listed: the materialized untouched files and symlinks and
  /// the merged subdirectories of each hinted directory — bounded by what the volume has loaded,
  /// never the base's whole tree. The caller keeps `since` and advances it to
  /// [`BasePlane::hint_seq`] once delivered; the marks stay for other transports.
  pub fn take_stale_base_entries(
    &mut self,
    store: &mut Store,
    since: u64,
  ) -> Result<StaleBaseEntries, VfsError> {
    self.process_hints(store)?;
    let plane = self.vol.base.as_ref().ok_or(VfsError::NotOverlay)?;
    let dirs: Vec<InodeNo> = plane
      .stale_since
      .iter()
      .filter(|(_, seq)| **seq > since)
      .map(|(no, _)| *no)
      .collect();
    let mut stale = StaleBaseEntries::default();
    for dir_no in dirs {
      let Ok(dir) = self.vol.current_dir(store, dir_no) else {
        continue;
      };
      stale.dirs.push(dir_no);
      for entry in store.dirs.get(dir)?.iter(&store.blocks) {
        let child = match entry.child {
          Child::File(no) | Child::Symlink(no) | Child::Fifo(no) | Child::Socket(no)
            if self.unloaded_file(store, no) =>
          {
            no
          }
          Child::Dir(handle) => match store.dirs.get(handle) {
            Ok(node) if node.base == BaseDirState::Merged => node.inode,
            _ => continue,
          },
          _ => continue,
        };
        stale.entries.push(StaleBaseEntry {
          dir: dir_no,
          name: entry.name.into(),
          child,
        });
      }
    }
    Ok(stale)
  }
}
