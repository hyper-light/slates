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

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::mem::Discriminant;

use slates_mem::Handle;
use slates_wire::Wire;
use slates_wire::crc32c::crc32c;

use crate::clock::Clock;
use crate::dir::{Child, DirNode};
use crate::error::VfsError;
use crate::ids::{Epoch, InodeNo, SnapshotId};
use crate::inode::{Attrs, Body, Home, Inode, Kind};
use crate::names::NameEquivalence;
use crate::quota::{BudgetGrowth, Quota};
use crate::snapshot::{Dead, Deadlist, Snapshot};
use crate::trie::{self, TrieNode};
use crate::volume::{Store, Volume, VolumeSeed};

/// Format: a volume image's magic (`"SLR1"` little-endian), so an all-zero or foreign content
/// object decodes to a mismatch and is refused rather than read as a valid empty volume.
const IMAGE_MAGIC: u32 = u32::from_le_bytes(*b"SLR1");
/// Format: a shard image's magic (`"SLS1"` little-endian), distinct from a single volume's so one
/// is never decoded as the other.
const SHARD_MAGIC: u32 = u32::from_le_bytes(*b"SLS1");
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

/// A snapshot's slot and generation in the image — the same pair a [`crate::ids::SnapshotId`]
/// carries, so a snapshot id a client holds still resolves after recovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Wire)]
pub struct SnapshotRef {
  /// The snapshot slab slot.
  pub index: u32,
  /// The slot's generation when the snapshot was taken.
  pub generation: u32,
}

/// Where a snapshot's deduplicated file or symlink lives in the image (§4.2/§4.8): the head (a CoW
/// handle it shares with the head; the rebuild shares the head's inode) or an earlier snapshot (a
/// version byte-identical to one an earlier snapshot already captured; the rebuild reconstructs an
/// independent copy from that snapshot's entry, so the recovered store is exactly what a full image
/// would have rebuilt — only the image is smaller).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Wire)]
pub enum SharedSource {
  /// The version the head still holds, shared by CoW handle identity.
  Head,
  /// A byte-identical version an earlier snapshot captured in full.
  Snapshot {
    /// The snapshot that holds the canonical copy.
    at: SnapshotRef,
  },
}

/// A file or symlink a snapshot does not carry in its own `inodes` because an identical version is
/// already in the image: its inode number and where the canonical copy is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Wire)]
pub struct SharedInode {
  /// The inode number in this snapshot.
  pub number: u64,
  /// Where the identical version already lives.
  pub source: SharedSource,
}

/// One copy-on-write snapshot in the image (§4.8): its identity and accounting, its links to the
/// neighbouring snapshots, and the tree frozen at it — every inode reachable from the snapshot's
/// own inode table, with its content *as the snapshot holds it* (read through the snapshot, not the
/// head). Captured independently of the head; the rebuild re-establishes it (its sharing with the
/// head is a §4.2 efficiency refinement, not a correctness property of the content).
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
pub struct SnapshotImage {
  /// The snapshot's id.
  pub id: SnapshotRef,
  /// The epoch the snapshot froze.
  pub epoch: u64,
  /// `referenced_bytes` at the snapshot (restored so accounting survives, §4.2/D-13).
  pub referenced_bytes: u64,
  /// The op-log head sequence at the snapshot (the deriver reads records after it).
  pub seq: u64,
  /// Clones that pin this snapshot as their origin.
  pub clone_refs: u32,
  /// The previous snapshot, if any.
  pub previous: Option<SnapshotRef>,
  /// The next snapshot, if any.
  pub next: Option<SnapshotRef>,
  /// The inodes the snapshot carries in full — its directories, and the files or symlinks whose
  /// bytes are not already elsewhere in the image — in number order. A file or symlink that is
  /// identical to the head's version, or to one an earlier snapshot already captured, is *not* here;
  /// it is named in `shared`, so its bytes live once across the whole image.
  pub inodes: Vec<InodeImage>,
  /// The files and symlinks the snapshot deduplicates against the head or an earlier snapshot
  /// (§4.2/§4.8): each names its inode number and where the identical version lives, so a
  /// heavily-snapshotted volume — the same file frozen in many snapshots, or edited after a run of
  /// snapshots — holds each distinct version once, the difference between an image that fits its
  /// content-object slice and a `RecoveryIncomplete`. Sorted by number for a deterministic image.
  pub shared: Vec<SharedInode>,
}

/// A whole volume's recoverable state (§4.8, A-9). The inodes are in number order (the order the
/// inode table yields them) and snapshots are in id order, so two images of equal state are
/// byte-identical (a determinism gate).
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
  /// The volume's most recent snapshot, if any.
  pub last_snapshot: Option<SnapshotRef>,
  /// Every inode of the head, in number order.
  pub inodes: Vec<InodeImage>,
  /// Every copy-on-write snapshot, in id order.
  pub snapshots: Vec<SnapshotImage>,
}

/// One volume's image under the routing key its owner (the server) files it by — the volume id's
/// sixteen bytes, which this crate does not interpret, so a whole shard of volumes recovers without
/// vfs knowing what a volume id is beyond its width.
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
pub struct KeyedImage {
  /// The owner's routing key for the volume (a volume id's bytes).
  pub key: [u8; 16],
  /// The volume's image.
  pub image: VolumeImage,
}

/// Every volume a shard holds, imaged together (§4.8): one shard has one content object, and it
/// publishes all of its volumes into it, so a restarted shard recovers them all from anchor-owned
/// RAM in one read. Volumes are in key order, so the shard image is deterministic.
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
pub struct ShardImage {
  /// The format magic; checked before anything else on decode.
  pub magic: u32,
  /// The format version.
  pub version: u16,
  /// The volumes, in key order.
  pub volumes: Vec<KeyedImage>,
}

impl ShardImage {
  /// A shard image of the given volumes, in key order.
  pub fn new(mut volumes: Vec<KeyedImage>) -> ShardImage {
    volumes.sort_by_key(|v| v.key);
    ShardImage {
      magic: SHARD_MAGIC,
      version: IMAGE_VERSION,
      volumes,
    }
  }

  /// Decodes a shard image from content-object bytes, refusing a foreign magic, an unknown version
  /// or any malformed field with [`VfsError::RecoveryIncomplete`].
  pub fn from_content(bytes: &[u8]) -> Result<ShardImage, VfsError> {
    let shard = ShardImage::from_bytes(bytes).map_err(|_| VfsError::RecoveryIncomplete)?;
    if shard.magic != SHARD_MAGIC || shard.version != IMAGE_VERSION {
      return Err(VfsError::RecoveryIncomplete);
    }
    Ok(shard)
  }

  /// The canonical content-object bytes of this shard image.
  pub fn to_content(&self) -> Vec<u8> {
    self.to_bytes()
  }

  /// Publishes this shard image into a content-object buffer as an atomic double-buffered commit
  /// (§4.8): the write lands in the slot that does not hold the last committed image, so an
  /// interrupted or torn publish preserves it. Refuses [`VfsError::NoSpace`] if a slot cannot hold
  /// the frame (and leaves the committed slot untouched).
  pub fn write_to(&self, buf: &mut [u8]) -> Result<usize, VfsError> {
    publish_committed(buf, &self.to_content())
  }

  /// Reads the last committed shard image back from a double-buffered content-object buffer (§4.8):
  /// `Ok(None)` for a fresh object or one where no publish ever committed, `Err(RecoveryIncomplete)`
  /// for a committed slot that is CRC-valid but decodes wrong (never a false success). A publish torn
  /// mid-write is skipped in favour of the previous committed image.
  pub fn read_from(buf: &[u8]) -> Result<Option<ShardImage>, VfsError> {
    match recover_committed(buf) {
      None => Ok(None),
      Some(bytes) => ShardImage::from_content(bytes).map(Some),
    }
  }
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

  /// Publishes this image into a content-object buffer as an atomic double-buffered commit (§4.8):
  /// the write lands in the slot that does not hold the last committed image, so an interrupted or
  /// torn publish preserves it. Refuses [`VfsError::NoSpace`] if a slot cannot hold the frame.
  pub fn write_to(&self, buf: &mut [u8]) -> Result<usize, VfsError> {
    publish_committed(buf, &self.to_content())
  }

  /// Reads the last committed image back from a double-buffered content-object buffer (§4.8).
  /// `Ok(None)` means nothing ever committed — a fresh object — so the caller starts a new volume
  /// rather than failing. A committed slot that is CRC-valid but malformed is
  /// [`VfsError::RecoveryIncomplete`], never an empty success; a publish torn mid-write is skipped in
  /// favour of the previous committed image.
  pub fn read_from(buf: &[u8]) -> Result<Option<VolumeImage>, VfsError> {
    match recover_committed(buf) {
      None => Ok(None),
      Some(bytes) => VolumeImage::from_content(bytes).map(Some),
    }
  }
}

/// Format: the width of the frame's length and CRC fields.
const LEN_WIDTH: usize = size_of::<u32>();
/// Format: the frame header — a little-endian byte length then a CRC-32C of the payload bytes.
const FRAME_HEADER: usize = 2 * LEN_WIDTH;

/// Frames Wire `payload` into a content-object buffer: a little-endian byte length and a CRC-32C of
/// the payload, then the payload. The header is what a restarted daemon reads to find and validate
/// what was published; the CRC turns a write torn by a crash into a typed refusal rather than a
/// garbage decode. Refuses [`VfsError::NoSpace`] if the buffer cannot hold the frame.
fn frame(payload: &[u8], buf: &mut [u8]) -> Result<usize, VfsError> {
  let total = FRAME_HEADER
    .checked_add(payload.len())
    .ok_or(VfsError::FileTooLarge)?;
  if buf.len() < total {
    return Err(VfsError::NoSpace);
  }
  let len = u32::try_from(payload.len()).map_err(|_| VfsError::FileTooLarge)?;
  buf[..LEN_WIDTH].copy_from_slice(&len.to_le_bytes());
  buf[LEN_WIDTH..FRAME_HEADER].copy_from_slice(&crc32c(payload).to_le_bytes());
  buf[FRAME_HEADER..total].copy_from_slice(payload);
  Ok(total)
}

/// The framed payload in a content-object buffer: `None` if the slot is empty (a fresh object),
/// `Err(RecoveryIncomplete)` if the frame is present but unreadable (a length past the buffer or a
/// CRC mismatch from a torn write), else the validated payload bytes for a decoder.
fn unframe(buf: &[u8]) -> Result<Option<&[u8]>, VfsError> {
  if buf.len() < FRAME_HEADER {
    return Ok(None);
  }
  let mut len_bytes = [0u8; LEN_WIDTH];
  len_bytes.copy_from_slice(&buf[..LEN_WIDTH]);
  let len = usize::try_from(u32::from_le_bytes(len_bytes)).unwrap_or(usize::MAX);
  if len == 0 {
    return Ok(None);
  }
  let mut crc_bytes = [0u8; LEN_WIDTH];
  crc_bytes.copy_from_slice(&buf[LEN_WIDTH..FRAME_HEADER]);
  let want_crc = u32::from_le_bytes(crc_bytes);
  let end = FRAME_HEADER
    .checked_add(len)
    .ok_or(VfsError::RecoveryIncomplete)?;
  if buf.len() < end {
    return Err(VfsError::RecoveryIncomplete);
  }
  let payload = &buf[FRAME_HEADER..end];
  if crc32c(payload) != want_crc {
    return Err(VfsError::RecoveryIncomplete);
  }
  Ok(Some(payload))
}

/// Format: the generation counter prefixed to a published slot's payload, so a restart can order the
/// two slots and pick the newer. A `u64`: at any realistic publish rate it never wraps.
const SLOT_GEN_WIDTH: usize = size_of::<u64>();

/// Publishes `image` into `slice` as one of two alternating, generation-tagged slots (§4.8), so that
/// an interrupted, torn or too-large publish never destroys the last committed image. The write
/// always lands in the slot that does *not* currently hold the committed image (the CRC-valid slot
/// with the higher generation), and the commit *is* the CRC becoming valid over `[generation ++
/// image]`. A crash mid-write leaves that slot's CRC wrong, so recovery ignores it and reads the
/// other slot — untouched, still the last committed. Returns the slot's frame length; refuses
/// [`VfsError::NoSpace`] if a slot cannot hold the frame, and on that refusal, too, the committed
/// slot is untouched — a publish that cannot fit preserves the last state rather than tearing it.
fn publish_committed(slice: &mut [u8], image: &[u8]) -> Result<usize, VfsError> {
  let half = slice.len() / 2;
  let committed = committed_generation(slice, half);
  let next = committed.checked_add(1).ok_or(VfsError::FileTooLarge)?;
  // Write the slot that does not hold the committed image (the older, empty, or torn one), so the
  // committed one survives whatever happens to this write.
  let slot_zero_committed = slot_generation(&slice[..half]) == Some(committed) && committed > 0;
  let mut payload = Vec::with_capacity(SLOT_GEN_WIDTH.saturating_add(image.len()));
  payload.extend_from_slice(&next.to_le_bytes());
  payload.extend_from_slice(image);
  let target = if slot_zero_committed {
    &mut slice[half..]
  } else {
    &mut slice[..half]
  };
  frame(&payload, target)
}

/// The last committed image in a double-buffered `slice` (§4.8): the CRC-valid slot with the higher
/// generation, or `None` if neither slot holds one (a fresh object, or both torn). A torn slot fails
/// its CRC and is skipped, so an interrupted publish falls back to the previous committed image; a
/// slot that is CRC-valid but decodes wrong is left for the caller to refuse, never a false success.
fn recover_committed(slice: &[u8]) -> Option<&[u8]> {
  let half = slice.len() / 2;
  let slot_zero = slot_payload(&slice[..half]);
  let slot_one = slot_payload(slice.get(half..).unwrap_or(&[]));
  match (slot_zero, slot_one) {
    (Some((g0, p0)), Some((g1, p1))) => Some(if g0 >= g1 { p0 } else { p1 }),
    (Some((_, p0)), None) => Some(p0),
    (None, Some((_, p1))) => Some(p1),
    (None, None) => None,
  }
}

/// The highest generation among the two CRC-valid slots (0 if neither is valid).
fn committed_generation(slice: &[u8], half: usize) -> u64 {
  let g0 = slot_generation(&slice[..half]).unwrap_or(0);
  let g1 = slot_generation(slice.get(half..).unwrap_or(&[])).unwrap_or(0);
  g0.max(g1)
}

/// A slot's generation, if its frame is CRC-valid and carries one.
fn slot_generation(slot: &[u8]) -> Option<u64> {
  slot_payload(slot).map(|(generation, _)| generation)
}

/// A CRC-valid slot's generation and the image bytes after it, or `None` for an empty or torn slot.
fn slot_payload(slot: &[u8]) -> Option<(u64, &[u8])> {
  match unframe(slot) {
    Ok(Some(payload)) if payload.len() >= SLOT_GEN_WIDTH => {
      let mut generation = [0u8; SLOT_GEN_WIDTH];
      generation.copy_from_slice(&payload[..SLOT_GEN_WIDTH]);
      Some((u64::from_le_bytes(generation), &payload[SLOT_GEN_WIDTH..]))
    }
    _ => None,
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
    let inodes = self.capture_tree(store, self.inode_root, None)?;
    let snapshots = self.capture_snapshots(store)?;
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
      last_snapshot: self.last_snapshot.map(snap_ref),
      inodes,
      snapshots,
    })
  }

  /// Captures every inode reachable from `inode_root`, in number order. File content is read through
  /// `snapshot` when set (the snapshot's own bytes) or the head when `None` — the version at that
  /// root, not the head's, which is what makes a snapshot's frozen content captured faithfully.
  fn capture_tree(
    &self,
    store: &Store,
    inode_root: Handle<TrieNode>,
    snapshot: Option<SnapshotId>,
  ) -> Result<Vec<InodeImage>, VfsError> {
    let mut handles = Vec::new();
    trie::walk(&store.tries, inode_root, &mut handles);
    let mut inodes = Vec::with_capacity(handles.len());
    for handle in handles {
      let inode = store.inodes.get(handle)?;
      inodes.push(self.image_of_inode(store, inode, snapshot)?);
    }
    Ok(inodes)
  }

  /// Captures every copy-on-write snapshot, in id order, with the tree frozen at each (§4.8).
  /// Capture a snapshot's tree as a delta against the head: a file or symlink whose handle *is* the
  /// head's very handle (CoW-shared, established when nothing rewrote it after the snapshot froze)
  /// contributes only its number to `shared`; everything else — the snapshot's directories and the
  /// files or symlinks that diverged — is captured in full. `self.inode_root` is the head.
  fn capture_snapshot_tree(
    &self,
    store: &Store,
    snap_inode_root: Handle<TrieNode>,
    id: SnapshotId,
    canonical: &mut BTreeMap<(u64, u32), Vec<(SnapshotRef, BodyImage)>>,
  ) -> Result<(Vec<InodeImage>, Vec<SharedInode>), VfsError> {
    let mut handles = Vec::new();
    trie::walk(&store.tries, snap_inode_root, &mut handles);
    let mut inodes = Vec::with_capacity(handles.len());
    let mut shared = Vec::new();
    for handle in handles {
      let inode = store.inodes.get(handle)?;
      let no = inode.no;
      if !matches!(inode.kind, Kind::File | Kind::Symlink) {
        // Directories are always carried in full; their entries are small and their structure is
        // this snapshot's own.
        inodes.push(self.image_of_inode(store, inode, Some(id))?);
        continue;
      }
      // A file or symlink whose handle is the head's very handle is CoW-shared with the head; the
      // rebuild shares the head's inode (O(1), no content to compare).
      if trie::get(&store.tries, self.inode_root, no) == Some(handle) {
        shared.push(SharedInode {
          number: no.0,
          source: SharedSource::Head,
        });
        continue;
      }
      // A version that diverged from the head: dedup it against earlier snapshots by content. The
      // crc buckets the lookup; the body decides the match.
      let image = self.image_of_inode(store, inode, Some(id))?;
      let crc = body_crc(&image.body);
      let bucket = canonical.entry((no.0, crc)).or_default();
      if let Some((src, _)) = bucket.iter().find(|(_, body)| *body == image.body) {
        shared.push(SharedInode {
          number: no.0,
          source: SharedSource::Snapshot { at: *src },
        });
        continue;
      }
      // This snapshot is the canonical holder of this version; record it for later snapshots.
      bucket.push((snap_ref(id), image.body.clone()));
      inodes.push(image);
    }
    shared.sort_by_key(|s| s.number);
    Ok((inodes, shared))
  }

  fn capture_snapshots(&self, store: &Store) -> Result<Vec<SnapshotImage>, VfsError> {
    // Cross-snapshot content dedup: a file or symlink version captured in full by one snapshot is
    // referenced, not re-captured, by a later snapshot that holds the byte-identical version. The
    // map is (number, body crc) → the snapshots that captured that version in full, with the body
    // kept to verify against a crc collision (a wrong match would corrupt content, so bytes decide,
    // not the checksum). `self.snapshots.iter()` yields ascending slot order, which is ascending id
    // order, and `from_image` rebuilds in the same order, so a reference always points to an
    // already-captured, earlier-rebuilt canonical.
    let mut canonical: BTreeMap<(u64, u32), Vec<(SnapshotRef, BodyImage)>> = BTreeMap::new();
    let mut out = Vec::with_capacity(self.snapshots.iter().count());
    for (handle, snap) in self.snapshots.iter() {
      let id = SnapshotId {
        index: handle.index(),
        generation: handle.generation(),
      };
      let (inodes, shared) =
        self.capture_snapshot_tree(store, snap.inode_root, id, &mut canonical)?;
      out.push(SnapshotImage {
        id: snap_ref(id),
        epoch: snap.epoch.0,
        referenced_bytes: snap.referenced_bytes,
        seq: snap.seq,
        clone_refs: snap.clone_refs,
        previous: snap.previous.map(snap_ref),
        next: snap.next.map(snap_ref),
        inodes,
        shared,
      });
    }
    out.sort_by_key(|s| (s.id.index, s.id.generation));
    Ok(out)
  }

  /// The image of one inode, capturing its body faithfully or refusing an un-captured kind. File
  /// content is read through `snapshot` when set (see [`Volume::capture_tree`]).
  fn image_of_inode(
    &self,
    store: &Store,
    inode: &Inode,
    snapshot: Option<SnapshotId>,
  ) -> Result<InodeImage, VfsError> {
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
      Kind::File => self.file_body(store, inode, snapshot)?,
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

  /// A file inode's bytes, read through the volume's own read path so inline, sealed and open bodies
  /// are all captured the same, from `snapshot` when set (its frozen bytes) or the head. An empty
  /// file (no content yet) images as `Empty`.
  fn file_body(
    &self,
    store: &Store,
    inode: &Inode,
    snapshot: Option<SnapshotId>,
  ) -> Result<BodyImage, VfsError> {
    let size = usize::try_from(inode.attrs.size).map_err(|_| VfsError::FileTooLarge)?;
    if size == 0 {
      return Ok(BodyImage::Empty);
    }
    let mut bytes = vec![0u8; size];
    let mut read = 0;
    while read < size {
      let at = u64::try_from(read).map_err(|_| VfsError::FileTooLarge)?;
      let got = match snapshot {
        Some(id) => self.read_in(store, id, inode.no, at, &mut bytes[read..])?,
        None => self.read(store, inode.no, at, &mut bytes[read..])?,
      };
      if got == 0 {
        break;
      }
      read += got;
    }
    bytes.truncate(read);
    Ok(BodyImage::File { bytes })
  }
}

/// The image reference for a snapshot id.
const fn snap_ref(id: SnapshotId) -> SnapshotRef {
  SnapshotRef {
    index: id.index,
    generation: id.generation,
  }
}

/// The snapshot id an image reference names.
const fn to_snapshot_id(r: SnapshotRef) -> SnapshotId {
  SnapshotId {
    index: r.index,
    generation: r.generation,
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
fn quota_from_image(quota: QuotaImage) -> Result<Quota, VfsError> {
  match quota {
    QuotaImage::Bounded { limit } => Ok(Quota::Bounded { limit }),
    // A dynamic quota's growth source is now the stateless [`BudgetGrowth`], so it need not be
    // serialized — only the counters travel in the image. The recovered volume grows against the
    // rebuilt shard budget exactly as it did before, and the server re-acquires its `granted` from
    // the budget so the reservation is accounted through recovery (§4.2).
    QuotaImage::Dynamic {
      max,
      granted,
      denied,
    } => Ok(Quota::Dynamic {
      max,
      source: Box::new(BudgetGrowth),
      granted,
      denied,
    }),
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
      root_no,
      policy,
      epoch,
      next_counter: image.next_counter,
      origin_epoch: image.origin_epoch.map(Epoch),
      quota,
    };
    let mut vol = Volume::recovery_shell(store, seed, clock, journal_bytes)?;

    // The head, into the shell's roots. Recovery places inodes by number (not `next_no`), so set the
    // live-inode count (§4.2) to the recovered head's inode count directly.
    vol.rebuild_passes(store, &image.inodes, root_no, epoch)?;
    vol.live_inodes = u64::try_from(image.inodes.len()).unwrap_or(u64::MAX);
    // The head's live-entry count (built by rebuild_entries); snapshot rebuilds below run through the
    // same dir_insert and perturb it, so keep it and restore after (§4.2 namespace, head-reachable).
    let head_entries = vol.live_entries;
    // The head's accounting is head-reachable content only; keep it aside so the snapshot rebuilds
    // (which write through the same counters) do not perturb it.
    let head_bytes = vol.bytes.clone();

    // The head's inode table and each inode's image, so a snapshot can share an inode it holds
    // unchanged with the head instead of rebuilding a private copy (§4.2 CoW-sharing efficiency).
    let head_inode_root = vol.inode_root;
    let head_images: BTreeMap<u64, &InodeImage> = image.inodes.iter().map(|i| (i.no, i)).collect();
    // Each snapshot's own full inodes, keyed by (its id, number), so a later snapshot that
    // deduplicated a byte-identical version against it can share that snapshot's rebuilt inode
    // (§4.2 cross-snapshot dedup). Keyed by the id pair since `SnapshotRef` is not ordered.
    let mut canonical_inode: BTreeMap<((u32, u32), u64), &InodeImage> = BTreeMap::new();
    for snap in &image.snapshots {
      for inode in &snap.inodes {
        canonical_inode.insert(((snap.id.index, snap.id.generation), inode.no), inode);
      }
    }
    let refs = SharingRefs {
      head_inode_root,
      head_images: &head_images,
      canonical_inode: &canonical_inode,
    };

    // Each snapshot, into its own roots, in id order so a fresh slab reproduces its id and a
    // cross-snapshot reference always resolves to an already-rebuilt canonical. Deadlists are left
    // empty here and filled by one global pass below, once every tree (and its sharing) exists.
    let mut snapshots: Vec<&SnapshotImage> = image.snapshots.iter().collect();
    snapshots.sort_by_key(|s| (s.id.index, s.id.generation));
    for snap in &snapshots {
      vol.rebuild_snapshot(store, snap, root_no, &refs)?;
    }
    vol.rebuild_deadlists(store, &snapshots)?;
    vol.bytes = head_bytes;
    vol.live_entries = head_entries;
    vol.last_snapshot = image.last_snapshot.map(to_snapshot_id);
    Ok(vol)
  }

  /// Runs the rebuild passes for one tree (the head or a snapshot) against the volume's current
  /// roots and epoch: place every inode at its number, rebuild directory entries, fill file content
  /// through the write path, then restore each inode's true identity.
  fn rebuild_passes(
    &mut self,
    store: &mut Store,
    inodes: &[InodeImage],
    root_no: InodeNo,
    epoch: Epoch,
  ) -> Result<(), VfsError> {
    let kinds: BTreeMap<u64, KindImage> = inodes.iter().map(|i| (i.no, i.kind)).collect();
    let mut dirs: BTreeMap<u64, Handle<DirNode>> = BTreeMap::new();
    dirs.insert(root_no.0, self.root);
    self.place_inodes(store, inodes, root_no, epoch, &mut dirs)?;
    self.rebuild_entries(store, inodes, &kinds, &dirs)?;
    self.fill_content(store, inodes)?;
    self.restore_identities(store, inodes)?;
    Ok(())
  }

  /// Rebuilds one copy-on-write snapshot into its own roots at its epoch, then registers it (§4.8).
  /// The volume's head roots and epoch are saved and restored around the rebuild, and its quota is
  /// lifted for it, because a snapshot's retained content is written through the same path as the
  /// head's but must not be charged against the head's quota (it is not head-reachable). The
  /// snapshot's tree is independent of the head's (its sharing is a §4.2 efficiency refinement) and
  /// its deadlist is empty (reclaiming a recovered snapshot's unique bytes on drop is owed).
  fn rebuild_snapshot(
    &mut self,
    store: &mut Store,
    snap: &SnapshotImage,
    root_no: InodeNo,
    refs: &SharingRefs,
  ) -> Result<(), VfsError> {
    let saved = (self.epoch, self.root, self.inode_root);
    let saved_quota = std::mem::replace(&mut self.quota, Quota::Bounded { limit: u64::MAX });
    let epoch = Epoch(snap.epoch);
    self.epoch = epoch;
    self.inode_root = trie::new_root(&mut store.tries, epoch)?;
    let root_handle = store
      .inodes
      .insert(Inode::new(root_no, epoch, Kind::Dir, 0, Body::None))?;
    self.table_set(store, root_no, root_handle)?;
    let root_dir = store.dirs.insert(DirNode::new(epoch, None, root_no, ""))?;
    store.inodes.get_mut(root_handle)?.body = Body::Directory(root_dir);
    self.root = root_dir;

    let outcome = self.rebuild_snapshot_tree(store, snap, root_no, epoch, refs);

    let (root, inode_root) = (self.root, self.inode_root);
    self.quota = saved_quota;
    self.epoch = saved.0;
    self.root = saved.1;
    self.inode_root = saved.2;
    outcome?;

    // Place the snapshot at the exact slot and generation it had before the crash: a `SnapshotId`
    // *is* its slab handle (§4.8), so a client that still holds the id must find the same snapshot
    // after recovery. `from_image` replays snapshots in ascending index order, so `insert_at`'s
    // append-extending contract holds and a destroyed snapshot's slot becomes a reusable gap. The
    // deadlist is filled by the global pass (`rebuild_deadlists`), once every tree exists, because a
    // shared inode belongs to exactly one snapshot's deadlist and that cannot be decided per-tree.
    self.snapshots.insert_at(
      snap.id.index,
      snap.id.generation,
      Snapshot {
        epoch,
        root,
        inode_root,
        deadlist: Deadlist::default(),
        clone_refs: snap.clone_refs,
        previous: snap.previous.map(to_snapshot_id),
        next: snap.next.map(to_snapshot_id),
        referenced_bytes: snap.referenced_bytes,
        seq: snap.seq,
        identity: None,
      },
    )?;
    Ok(())
  }

  /// Reconstruct every recovered snapshot's deadlist in one global pass, newest first (§4.8). A
  /// snapshot's deadlist is the objects it reaches that the head no longer holds; a version shared
  /// across snapshots (CoW) must sit on exactly *one* snapshot's deadlist — the newest that holds it
  /// — so that `destroy_snapshot`'s migration frees it only when its oldest referencer is destroyed.
  /// Processing newest first and skipping anything an already-processed (newer) snapshot claimed puts
  /// each object where the live volume kept it, so a drop in any order neither leaks nor double-frees.
  fn rebuild_deadlists(
    &mut self,
    store: &mut Store,
    snapshots: &[&SnapshotImage],
  ) -> Result<(), VfsError> {
    let mut order: Vec<&SnapshotImage> = snapshots.to_vec();
    order.sort_by_key(|s| std::cmp::Reverse(s.epoch));
    let mut claimed: HashSet<(Discriminant<Dead>, u32, u32)> = HashSet::new();
    for snap in order {
      // Objects the head still holds are the head's, not this snapshot's; everything else the
      // snapshot reaches is a candidate, then filtered to what no newer snapshot already claimed.
      let head_shared: BTreeSet<u64> = snap
        .shared
        .iter()
        .filter_map(|s| match s.source {
          SharedSource::Head => Some(s.number),
          SharedSource::Snapshot { .. } => None,
        })
        .collect();
      let handle = snapshot_slab_handle(snap.id);
      let (root, inode_root) = {
        let s = self.snapshots.get(handle)?;
        (s.root, s.inode_root)
      };
      let full = crate::volume::tree_deadlist_excluding(store, root, inode_root, &head_shared);
      let mut deadlist = Deadlist::default();
      for dead in full.items() {
        if claimed.insert(dead_key(dead)) {
          deadlist.push(*dead);
        }
      }
      self.snapshots.get_mut(handle)?.deadlist = deadlist;
    }
    Ok(())
  }

  /// Rebuilds a snapshot's tree into the volume's current (snapshot) roots, sharing with the head
  /// every file or symlink the snapshot holds unchanged (an identical image) instead of rebuilding a
  /// private copy — the §4.2 CoW-sharing efficiency. Returns the inode numbers shared with the head,
  /// which the caller excludes from the snapshot's deadlist so a drop never frees the head's copy.
  fn rebuild_snapshot_tree(
    &mut self,
    store: &mut Store,
    snap: &SnapshotImage,
    root_no: InodeNo,
    epoch: Epoch,
    refs: &SharingRefs,
  ) -> Result<(), VfsError> {
    // `snap.shared` names the files and symlinks the snapshot does not carry itself because an
    // identical version is already in the rebuilt store: the head still holds it (share the head's
    // inode) or an earlier snapshot captured it (share that snapshot's inode — the earlier snapshot
    // has a lower id and is already rebuilt). Sharing, not copying, is what makes the recovered store
    // match the live volume's CoW memory. The kind comes from the source so entries resolve; the
    // snapshot's own inodes (`snap.inodes`) rebuild below.
    let mut kinds: BTreeMap<u64, KindImage> = snap.inodes.iter().map(|i| (i.no, i.kind)).collect();
    for entry in &snap.shared {
      let no = InodeNo(entry.number);
      let (source_root, kind) = match entry.source {
        SharedSource::Head => {
          let image = refs
            .head_images
            .get(&entry.number)
            .ok_or(VfsError::RecoveryIncomplete)?;
          (refs.head_inode_root, image.kind)
        }
        SharedSource::Snapshot { at } => {
          let image = refs
            .canonical_inode
            .get(&((at.index, at.generation), entry.number))
            .ok_or(VfsError::RecoveryIncomplete)?;
          let source_root = self.snapshots.get(snapshot_slab_handle(at))?.inode_root;
          (source_root, image.kind)
        }
      };
      kinds.insert(entry.number, kind);
      let handle = trie::get(&store.tries, source_root, no).ok_or(VfsError::RecoveryIncomplete)?;
      self.table_set(store, no, handle)?;
    }
    let mut dirs: BTreeMap<u64, Handle<DirNode>> = BTreeMap::new();
    dirs.insert(root_no.0, self.root);
    for image_inode in &snap.inodes {
      let no = InodeNo(image_inode.no);
      if no == root_no {
        continue;
      }
      let body = body_for(store, image_inode, no, epoch, &mut dirs)?;
      let inode = Inode::new(no, epoch, kind_from_image(image_inode.kind), 0, body);
      let handle = store.inodes.insert(inode)?;
      self.table_set(store, no, handle)?;
    }
    self.rebuild_entries(store, &snap.inodes, &kinds, &dirs)?;
    self.fill_content(store, &snap.inodes)?;
    self.restore_identities(store, &snap.inodes)?;
    Ok(())
  }

  /// Pass one: place every non-root inode at its own number, born at the head epoch, with a fresh
  /// directory node (parent and name fixed up when the parent's entries are rebuilt), a symlink's
  /// target, or an empty file body to be filled by the write path. The root already exists.
  fn place_inodes(
    &mut self,
    store: &mut Store,
    inodes: &[InodeImage],
    root_no: InodeNo,
    epoch: Epoch,
    dirs: &mut BTreeMap<u64, Handle<DirNode>>,
  ) -> Result<(), VfsError> {
    for image_inode in inodes {
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
    inodes: &[InodeImage],
    kinds: &BTreeMap<u64, KindImage>,
    dirs: &BTreeMap<u64, Handle<DirNode>>,
  ) -> Result<(), VfsError> {
    for image_inode in inodes {
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
  /// Fills each non-empty file's content through the write path, skipping inodes in `shared` (a
  /// snapshot's files shared with the head already hold the head's content).
  fn fill_content(&mut self, store: &mut Store, inodes: &[InodeImage]) -> Result<(), VfsError> {
    for image_inode in inodes {
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
  /// Restores each inode's identity, skipping inodes in `shared` (they are the head's, already
  /// carrying the head's identity — restoring through the snapshot would touch the head's inode).
  fn restore_identities(
    &mut self,
    store: &mut Store,
    inodes: &[InodeImage],
  ) -> Result<(), VfsError> {
    for image_inode in inodes {
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
/// What a snapshot rebuild needs to share an inode instead of rebuilding it (§4.2/§4.8 CoW-sharing):
/// the head's rebuilt inode table and images (for a version the head still holds), and every
/// snapshot's own images keyed by (id, number) so a version an earlier snapshot captured in full can
/// be shared from that snapshot's rebuilt inode. Sharing — not copying — is what makes the recovered
/// store use the same RAM the live, pre-crash volume did (the live volume shares CoW inodes across
/// snapshots), so recovery reconstructs the volume as it was, not a bloated copy of it.
struct SharingRefs<'a> {
  head_inode_root: Handle<TrieNode>,
  head_images: &'a BTreeMap<u64, &'a InodeImage>,
  canonical_inode: &'a BTreeMap<((u32, u32), u64), &'a InodeImage>,
}

/// The slab handle a `SnapshotRef` names (its slot and generation) — a snapshot id *is* its slab
/// handle (§4.8), so this resolves a reference to the rebuilt snapshot's record.
fn snapshot_slab_handle(r: SnapshotRef) -> Handle<Snapshot> {
  Handle::from_raw(r.index, r.generation)
}

/// A stable key for a dead object — its variant and slab handle — so the global deadlist pass (§4.8)
/// claims each object for exactly one snapshot's deadlist. The variant discriminant separates the
/// five slabs (a `Dir` handle and an `Inode` handle can share an index); the born epoch is not part
/// of the key, since a slab handle at its generation already identifies one object uniquely.
fn dead_key(dead: &Dead) -> (Discriminant<Dead>, u32, u32) {
  let (index, generation) = match dead {
    Dead::Dir(h, _) => (h.index(), h.generation()),
    Dead::DirBlock(h, _) => (h.index(), h.generation()),
    Dead::Inode(h, _) => (h.index(), h.generation()),
    Dead::Trie(h, _) => (h.index(), h.generation()),
    Dead::Chunk(h, _) => (h.index(), h.generation()),
  };
  (std::mem::discriminant(dead), index, generation)
}

/// A crc of a body's bytes, used only to bucket the cross-snapshot content dedup lookup (§4.2). The
/// match is then decided by exact byte equality, so a crc collision costs one comparison, never a
/// wrong dedup (which would corrupt content). Directories do not reach here (only files and symlinks
/// are deduped); the arm exists to keep the match exhaustive.
fn body_crc(body: &BodyImage) -> u32 {
  match body {
    BodyImage::File { bytes } => crc32c(bytes),
    BodyImage::Symlink { target } => crc32c(target.as_bytes()),
    BodyImage::Empty => crc32c(&[]),
    BodyImage::Directory { .. } => 0,
  }
}

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
