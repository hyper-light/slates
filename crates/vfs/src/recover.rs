//! Volume recovery images (§4.8, A-9): a faithful, handle-free image of a volume's durable state
//! — every inode with its number, generation, birth epoch, POSIX attributes, home and body; every
//! directory's entries by name and child number; every file's held bytes, as runs at their offsets
//! (a hole costs nothing in the volume and nothing here); and the volume's roots
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
//! Scratch and overlay images include CoW snapshots, quota counters and referenced-but-unlinked
//! orphans. An overlay also retains source-directory identities, witnesses, whiteouts, redirects
//! and private windows. Recovery reopens source components without following links and verifies
//! their full fingerprints before restoring lazy reads; a changed or absent source refuses.
//! Directory caches start invalid so external edits are rechecked. Every recorded attachment's references
//! (version 10, A-61) are carried too: the owner settles them after recovery, giving them back to the attachments
//! whose records survived (a FUSE mount the anchor held) and releasing every other holder's, so an orphan whose
//! holders all died with the process is reclaimed rather than kept forever.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::mem::Discriminant;

use slates_mem::Handle;
use slates_wire::Wire;
use slates_wire::crc32c::{crc32c, crc32c_append};

use crate::clock::Clock;
use crate::content::{Chunk, Extent, ExtentSrc, OpenExtent};
use crate::dir::{Child, DirNode};
use crate::error::VfsError;
use crate::ids::{Epoch, InodeNo, SnapshotId};
use crate::inode::{Attrs, Body, Home, Inode, Kind};
use crate::names::NameEquivalence;
use crate::quota::{BudgetGrowth, Quota};
use crate::snapshot::{Dead, Deadlist, Snapshot};
use crate::trie::{self, TrieNode};
use crate::volume::{Store, Volume, VolumeSeed, body_extents};

/// Format: a volume image's magic (`"SLR1"` little-endian), so an all-zero or foreign content
/// object decodes to a mismatch and is refused rather than read as a valid empty volume.
const IMAGE_MAGIC: u32 = u32::from_le_bytes(*b"SLR1");
/// Format: a shard image's magic (`"SLS1"` little-endian), distinct from a single volume's so one
/// is never decoded as the other.
const SHARD_MAGIC: u32 = u32::from_le_bytes(*b"SLS1");
/// The image layout's history: 3 (2026-09-15) a file's body is its held runs at their offsets, not
/// one vector of its logical length; 4 (A-26) FIFO/socket kinds with empty bodies and zero size; 5
/// (§4.5, 2026-09-26) each inode's extended-attribute table and, for an attribute inode, its owner;
/// 6 (§4.6) each inode's AppleDouble working copy; 7 (A-48, 2026-09-30) the base plane's witness,
/// home, whiteout and redirect tables with every version a snapshot still reads; 8 (AUD-29-59, 2026-09-30)
/// the shard's held replicas, so a content acknowledgement survives a warm restart; 9 (AUD-29-55,
/// 2026-10-01) the held replicas' transfers in progress (stages), so a cut transfer resumes from its
/// verified chunks after a warm restart; 10 (A-61, 2026-10-03) every recorded attachment's references, so a
/// FUSE mount the anchor held gets its kernel's references back after a restart; 11 (A-61, 2026-10-03) the
/// replies a barrier publishes with its effect, so a request the dead daemon applied and never answered is
/// answered from the record, never applied twice; 12 (A-64, 2026-10-03) a file's body names its chunks and open
/// extent by their blocks in the shard's arena range instead of carrying its bytes, so a barrier costs the
/// shard's metadata, not its content; 13 (A-64, 2026-10-03) the held replicas' image names their blocks the same way,
/// and a clone's image carries only its own inodes and the numbers it shares with its origin snapshot.
/// 14 (A-89, 2026-10-05) a directory's entries are ordered by their folded names under the volume's policy, so a
/// delta's replay finds a name by binary search instead of scanning the directory. 15 (A-96, 2026-10-05) a delta
/// carries only the attachments' reference counts that changed, not every attachment's whole list. 16 (2026-10-05) a
/// base entry's fingerprint carries its owner, so an overlay reports a base file's owner as the disk holds it.
/// 17 (A-99, 2026-10-05) a sealed chunk's image carries its key identity, its version and its segments' tags, so
/// a restarted daemon re-opens the ciphertext the arena still holds instead of reading it as clear bytes.
/// Format: the image layout version, bumped with any change to the types below or to the held replicas'
/// image they carry.
const IMAGE_VERSION: u16 = 17;

/// A recorded attachment's references in an image (A-61): its durable id and its share by inode.
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
pub struct AttachmentReferences {
  /// The attachment's durable id.
  pub attachment: u64,
  /// Its references, by inode number, in number order.
  pub inodes: Vec<InodeReferences>,
}

/// One inode's references held by one attachment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Wire)]
pub struct InodeReferences {
  /// The inode number.
  pub inode: u64,
  /// The references held.
  pub count: u32,
}

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
  /// A FIFO name, without kernel stream state.
  Fifo,
  /// A socket name, without a listener or connection.
  Socket,
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
  pub child: Option<u64>,
}

/// An inode's body as the store holds it (§4.5 `Body`, A-64): a directory becomes its entries; a file's
/// inline bytes are carried, while its sealed chunks and open extent are named by their blocks in the shard's
/// arena range, whose bytes live in anchor RAM and survive the daemon; a symlink becomes its target. `Empty`
/// is a body with no content yet.
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
pub enum BodyImage {
  /// No content yet.
  Empty,
  /// A directory's entries, in canonical order ([`entry_order`]); none in a delta's image of a changed directory.
  Directory {
    /// The entries.
    entries: Vec<EntryImage>,
    /// Whether the source directory shows through or the overlay is opaque.
    base: crate::dir::BaseDirState,
    /// The original base path of an overlay directory rename.
    origin: Option<String>,
  },
  /// Small content kept in the inode.
  Inline {
    /// The bytes.
    bytes: Vec<u8>,
  },
  /// Content in the arena: sealed extents, ascending and non-overlapping, and the open extent over them.
  Chunked {
    /// The sealed extents.
    extents: Vec<ExtentImage>,
    /// The open extent, written in place until sealed.
    open: Option<OpenImage>,
  },
  /// A live base-backed file and the ranges already owned by the overlay.
  Base {
    /// The original observed source, independent of later source changes.
    witness: Option<crate::inode::Witness>,
    /// Private ranges, in ascending non-overlapping order.
    pinned: Vec<ExtentImage>,
    /// Source length before private extensions or truncations.
    base_len: u64,
    /// Unpinned reads already lost their source witness.
    lost: bool,
  },
  /// A symlink's target.
  Symlink {
    /// The target path.
    target: String,
  },
}

/// A block of the shard's arena (A-64): its region, its offset within it and its length (a power-of-two number
/// of granules, as the buddy allocator hands out).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Wire)]
pub struct BlockImage {
  /// The arena region.
  pub region: u16,
  /// The offset within the region.
  pub offset: u64,
  /// The block's length.
  pub len: u64,
}

/// How a chunk's bytes are sealed in the arena (A-99): the key's stable identity (the cipher derives the key from it on
/// recovery), the version its segments were sealed at, and their tags in segment order.
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
pub struct SealImage {
  /// The key's identity, as the store's cipher names it.
  pub key: [u8; 32],
  /// The version the segments were sealed at.
  pub version: u64,
  /// The segments' tags, in order.
  pub tags: Vec<[u8; 16]>,
}

/// A sealed chunk (§4.5 `Chunk`): its block, the bytes used from it, its birth epoch and its identity once
/// computed.
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
pub struct ChunkImage {
  /// The block holding the bytes.
  pub block: BlockImage,
  /// The bytes used, from the block's start.
  pub used: u32,
  /// The birth epoch.
  pub born: u64,
  /// BLAKE3 of the bytes, once computed.
  pub identity: Option<[u8; 32]>,
  /// How its bytes are sealed in the arena (A-99), when they are.
  pub seal: Option<SealImage>,
}

/// Where a sealed extent's bytes are.
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
pub enum ExtentSourceImage {
  /// Zeros.
  Zero,
  /// A chunk, from byte `at` within it.
  Chunk {
    /// The chunk.
    chunk: ChunkImage,
    /// The offset within the chunk.
    at: u32,
  },
}

/// A sealed extent: `len` bytes at file offset `offset`.
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
pub struct ExtentImage {
  /// The file offset.
  pub offset: u64,
  /// The length.
  pub len: u64,
  /// The source.
  pub source: ExtentSourceImage,
}

/// An open extent: `len` bytes of `block` at file offset `offset`, written in place until sealed. The block's
/// bytes past `len` may hold writes made after this image (A-64); none is served, since reads stop at `len` and
/// a write zero-fills any gap it extends over.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Wire)]
pub struct OpenImage {
  /// The file offset of the block's first byte.
  pub offset: u64,
  /// The bytes used.
  pub len: u64,
  /// The block.
  pub block: BlockImage,
  /// The block's birth epoch.
  pub born: u64,
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
  /// The extended attributes (§4.5): each name and the attribute inode holding its value, ascending
  /// by name. Image version 5.
  pub xattrs: Vec<XattrImage>,
  /// For an attribute inode, the inode whose attribute it holds. Image version 5.
  pub attribute_of: Option<u64>,
  /// The attribute inode holding a transport's working copy of this inode's attributes (the
  /// AppleDouble `._` bytes a client wrote, §4.6). Image version 6.
  pub sidecar: Option<u64>,
}

/// One extended attribute of an inode in an image: its name and the attribute inode (captured like
/// any inode in the same image) that holds its value.
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
pub struct XattrImage {
  /// The attribute name.
  pub name: Vec<u8>,
  /// The attribute inode's number.
  pub attribute: u64,
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
  /// A clone's snapshot holding a version from the clone's origin snapshot (born at or before the origin epoch,
  /// A-64): the origin's record, shared.
  Origin,
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
  /// Every inode of the head, in number order — for a clone, only its own (born after its origin epoch).
  pub inodes: Vec<InodeImage>,
  /// For a clone, the inode numbers its head still holds from its origin snapshot (born at or before the origin
  /// epoch, so the very records the origin's snapshot holds, A-64): recovered by sharing those records, as
  /// [`Volume::clone_of`] shares them, never by rebuilding copies a clone's destroy would leave to the origin.
  pub origin_shared: Vec<u64>,
  /// Every copy-on-write snapshot, in id order.
  pub snapshots: Vec<SnapshotImage>,
  /// Inode numbers that have left the namespace (`nlink == 0`) but are still held open — orphans
  /// (POSIX unlink-while-open). Their inodes are captured with every other (the walk covers the whole
  /// table), so their content already survives; this restores the *tracking* so a recovered orphan is
  /// reclaimed when its handle, reacquired through the anchor handoff, finally closes — not leaked.
  pub orphans: Vec<u64>,
  /// Source identities and the complete durable overlay plane.
  pub base: Option<crate::base::BaseImage>,
  /// Every recorded attachment's references, in attachment order (A-61): given back after a restart to the
  /// attachments that survive it, and released for the rest ([`Volume::settle_recovered_references`]).
  pub references: Vec<AttachmentReferences>,
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
  /// The content this shard holds for other owners as their candidate holder (AUD-29-59): the hold's own
  /// canonical image, opaque here (the cluster plane encodes and decodes it), empty when nothing is held.
  /// A holder acknowledges a content put only once an image carrying it is committed.
  pub held: Vec<u8>,
  /// The replies a barrier published with its effect and had not yet delivered (A-61): at most one per FUSE
  /// mount, opaque here (the transport encodes them).
  pub replies: Vec<HeldReply>,
}

/// A transport's reply published with the effect it answers (A-61): the mount's attachment, the request's unique
/// id (without the kernel's resend bit), and the reply's bytes, opaque here.
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
pub struct HeldReply {
  /// The attachment of the mount the request came through.
  pub attachment: u64,
  /// The request's unique id.
  pub unique: u64,
  /// The reply's bytes.
  pub reply: Vec<u8>,
}

impl ShardImage {
  /// A shard image of the given volumes, in key order, holding nothing for others.
  pub fn new(mut volumes: Vec<KeyedImage>) -> ShardImage {
    volumes.sort_by_key(|v| v.key);
    ShardImage {
      magic: SHARD_MAGIC,
      version: IMAGE_VERSION,
      volumes,
      held: Vec::new(),
      replies: Vec::new(),
    }
  }

  /// This image carrying `held`, the shard's held replicas' canonical image (AUD-29-59).
  pub fn with_held(self, held: Vec<u8>) -> ShardImage {
    ShardImage { held, ..self }
  }

  /// This image carrying `replies`, the barrier replies not yet delivered (A-61), in attachment order.
  pub fn with_replies(self, mut replies: Vec<HeldReply>) -> ShardImage {
    replies.sort_by_key(|reply| reply.attachment);
    ShardImage { replies, ..self }
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
  pub fn write_to<S: ImageWrite + ?Sized>(&self, slots: &mut S) -> Result<usize, VfsError> {
    publish_committed(slots, &self.to_content(), None).map(|(total, _)| total)
  }

  /// [`ShardImage::write_to`] for the one process publishing into `slots`, which knows the committed slot from its
  /// last publish (`known`; `None` checks both slots): the frame is written in one pass, and the new committed slot
  /// is returned for the next.
  pub fn write_after<S: ImageWrite + ?Sized>(
    &self,
    slots: &mut S,
    known: Option<CommittedSlot>,
  ) -> Result<(usize, CommittedSlot), VfsError> {
    publish_committed(slots, &self.to_content(), known)
  }

  /// Reads the last committed shard image back from a double-buffered content-object buffer (§4.8):
  /// `Ok(None)` for a fresh object or one where no publish ever committed, `Err(RecoveryIncomplete)`
  /// for a committed slot that is CRC-valid but decodes wrong (never a false success). A publish torn
  /// mid-write is skipped in favour of the previous committed image.
  pub fn read_from<S: ImageRead + ?Sized>(slots: &S) -> Result<Option<ShardImage>, VfsError> {
    match recover_committed(slots)? {
      None => Ok(None),
      Some(bytes) => ShardImage::from_content(&bytes).map(Some),
    }
  }

  /// The generation of the last committed shard image in `slots`, the one [`ShardImage::read_from`] reads: what a
  /// transport's write log since that publication is stamped with (A-63), so recovery replays only the writes the
  /// recovered image does not already carry. `None` when nothing committed.
  pub fn committed_generation<S: ImageRead + ?Sized>(slots: &S) -> Option<u64> {
    committed_slot(slots).map(|slot| slot.generation)
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
  pub fn write_to<S: ImageWrite + ?Sized>(&self, slots: &mut S) -> Result<usize, VfsError> {
    publish_committed(slots, &self.to_content(), None).map(|(total, _)| total)
  }

  /// Reads the last committed image back from a double-buffered content-object buffer (§4.8).
  /// `Ok(None)` means nothing ever committed — a fresh object — so the caller starts a new volume
  /// rather than failing. A committed slot that is CRC-valid but malformed is
  /// [`VfsError::RecoveryIncomplete`], never an empty success; a publish torn mid-write is skipped in
  /// favour of the previous committed image.
  pub fn read_from<S: ImageRead + ?Sized>(slots: &S) -> Result<Option<VolumeImage>, VfsError> {
    match recover_committed(slots)? {
      None => Ok(None),
      Some(bytes) => VolumeImage::from_content(&bytes).map(Some),
    }
  }
}

/// Where a double-buffered image is read from (§4.8), by copies: the content object is sparse (backed
/// only where it is touched, `slates_mem::SparseObject`) and hands out no reference to its bytes
/// (AUD-29-09), so a reader copies the header, then streams the payload it names through a bounded
/// buffer to check it, and copies out only the image it recovers. A byte slice is one; the daemon's slice
/// of the content object is another.
pub trait ImageRead {
  /// The bytes the two slots share.
  fn image_len(&self) -> usize;
  /// Copies the `into.len()` bytes at `offset` out; `RecoveryIncomplete` for a span past the end.
  fn image_read(&self, offset: usize, into: &mut [u8]) -> Result<(), VfsError>;
}

/// Where a double-buffered image is published (§4.8), by copies.
pub trait ImageWrite: ImageRead {
  /// Copies `from` in at `offset`; `NoSpace` for a span past the end.
  fn image_write(&mut self, offset: usize, from: &[u8]) -> Result<(), VfsError>;
}

impl ImageRead for [u8] {
  fn image_len(&self) -> usize {
    self.len()
  }

  fn image_read(&self, offset: usize, into: &mut [u8]) -> Result<(), VfsError> {
    let from = offset
      .checked_add(into.len())
      .and_then(|end| self.get(offset..end))
      .ok_or(VfsError::RecoveryIncomplete)?;
    into.copy_from_slice(from);
    Ok(())
  }
}

impl ImageWrite for [u8] {
  fn image_write(&mut self, offset: usize, from: &[u8]) -> Result<(), VfsError> {
    let into = offset
      .checked_add(from.len())
      .and_then(|end| self.get_mut(offset..end))
      .ok_or(VfsError::NoSpace)?;
    into.copy_from_slice(from);
    Ok(())
  }
}

impl ImageRead for Vec<u8> {
  fn image_len(&self) -> usize {
    self.as_slice().image_len()
  }

  fn image_read(&self, offset: usize, into: &mut [u8]) -> Result<(), VfsError> {
    self.as_slice().image_read(offset, into)
  }
}

impl ImageWrite for Vec<u8> {
  fn image_write(&mut self, offset: usize, from: &[u8]) -> Result<(), VfsError> {
    self.as_mut_slice().image_write(offset, from)
  }
}

/// Format: the width of the frame's length and CRC fields.
const LEN_WIDTH: usize = size_of::<u32>();
/// Format: the frame header — a little-endian byte length then a CRC-32C of the payload bytes.
const FRAME_HEADER: usize = 2 * LEN_WIDTH;
/// Shape: the span a slot's payload is streamed through to check its CRC, so checking a slot costs a
/// bounded buffer whatever the image's size (any span is correct; this one keeps the per-span call cost a
/// small fraction of the CRC's own over each span).
const CRC_SPAN_BYTES: usize = 1 << 16;

/// One of the two slots: its offset and length within the image memory.
#[derive(Clone, Copy)]
struct Slot {
  offset: usize,
  len: usize,
}

/// Frames `generation` and the Wire `image` into `slot`: a little-endian byte length and a CRC-32C of the payload
/// (the generation's bytes then the image's), then the payload, written in its two parts without first copying
/// them into one buffer. The header is what a restarted daemon reads to find and validate what was published; the
/// CRC turns a write torn by a crash into a typed refusal rather than a garbage decode. Refuses
/// [`VfsError::NoSpace`] if the slot cannot hold the frame.
fn frame<S: ImageWrite + ?Sized>(
  generation: u64,
  image: &[u8],
  slots: &mut S,
  slot: Slot,
) -> Result<usize, VfsError> {
  let payload_len = SLOT_GEN_WIDTH
    .checked_add(image.len())
    .ok_or(VfsError::FileTooLarge)?;
  let total = FRAME_HEADER
    .checked_add(payload_len)
    .ok_or(VfsError::FileTooLarge)?;
  if slot.len < total {
    return Err(VfsError::NoSpace);
  }
  let len = u32::try_from(payload_len).map_err(|_| VfsError::FileTooLarge)?;
  let generation = generation.to_le_bytes();
  let crc = crc32c_append(crc32c(&generation), image);
  let mut header = [0u8; FRAME_HEADER];
  let (len_field, crc_field) = header.split_at_mut(LEN_WIDTH);
  len_field.copy_from_slice(&len.to_le_bytes());
  crc_field.copy_from_slice(&crc.to_le_bytes());
  let payload_at = slot.offset.saturating_add(FRAME_HEADER);
  slots.image_write(slot.offset, &header)?;
  slots.image_write(payload_at, &generation)?;
  slots.image_write(payload_at.saturating_add(SLOT_GEN_WIDTH), image)?;
  Ok(total)
}

/// A slot whose frame is present and CRC-valid: where its payload starts and how long it is.
#[derive(Clone, Copy)]
struct Framed {
  at: usize,
  len: usize,
}

/// The framed payload in `slot`: `None` if the slot is empty (a fresh object),
/// `Err(RecoveryIncomplete)` if the frame is present but unreadable (a length past the slot or a CRC
/// mismatch from a torn write), else where the validated payload lies. The payload is streamed through a
/// bounded buffer to check it, never copied whole.
fn unframe<S: ImageRead + ?Sized>(slots: &S, slot: Slot) -> Result<Option<Framed>, VfsError> {
  if slot.len < FRAME_HEADER {
    return Ok(None);
  }
  let mut header = [0u8; FRAME_HEADER];
  slots.image_read(slot.offset, &mut header)?;
  let (len_field, crc_field) = header.split_at(LEN_WIDTH);
  let word = |field: &[u8]| {
    let mut bytes = [0u8; LEN_WIDTH];
    bytes.copy_from_slice(field);
    u32::from_le_bytes(bytes)
  };
  let len = usize::try_from(word(len_field)).unwrap_or(usize::MAX);
  if len == 0 {
    return Ok(None);
  }
  let end = FRAME_HEADER
    .checked_add(len)
    .ok_or(VfsError::RecoveryIncomplete)?;
  if slot.len < end {
    return Err(VfsError::RecoveryIncomplete);
  }
  let at = slot.offset.saturating_add(FRAME_HEADER);
  if streamed_crc(slots, at, len)? != word(crc_field) {
    return Err(VfsError::RecoveryIncomplete);
  }
  Ok(Some(Framed { at, len }))
}

/// The CRC-32C of the `len` bytes at `at`, streamed through a bounded buffer.
fn streamed_crc<S: ImageRead + ?Sized>(slots: &S, at: usize, len: usize) -> Result<u32, VfsError> {
  let mut span = vec![0u8; len.min(CRC_SPAN_BYTES)];
  let mut crc = 0;
  let mut done = 0usize;
  while done < len {
    let take = (len - done).min(span.len());
    let chunk = span.get_mut(..take).ok_or(VfsError::RecoveryIncomplete)?;
    slots.image_read(at.saturating_add(done), chunk)?;
    crc = crc32c_append(crc, chunk);
    done = done.saturating_add(take);
  }
  Ok(crc)
}

/// Format: the generation counter prefixed to a published slot's payload, so a restart can order the
/// two slots and pick the newer. A `u64`: at any realistic publish rate it never wraps.
const SLOT_GEN_WIDTH: usize = size_of::<u64>();

/// The two slots of image memory `total` bytes long: the first half and the rest.
fn slots_of(total: usize) -> (Slot, Slot) {
  let half = total / 2;
  (
    Slot {
      offset: 0,
      len: half,
    },
    Slot {
      offset: half,
      len: total - half,
    },
  )
}

/// Which slot of an image memory holds the committed image, and its generation (§4.8). A publisher learns it once —
/// [`committed_slot`] checks both slots' CRCs — and then keeps it from what each of its own publishes returns, so a
/// publish writes its frame in one pass instead of re-reading both slots to learn what it already knows. Only the
/// one process publishing into the memory may keep it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommittedSlot {
  /// The committed image's generation.
  pub generation: u64,
  /// Whether it is the second slot.
  second: bool,
}

impl CommittedSlot {
  /// The same committed slot, with the generation the publisher's last publication reached (a delta logged after
  /// the checkpoint, A-68), so the next checkpoint's generation is newer than every frame before it.
  pub(crate) fn advanced_to(self, generation: u64) -> CommittedSlot {
    CommittedSlot {
      generation: generation.max(self.generation),
      ..self
    }
  }
}

/// The committed slot of image memory `slots`, found by checking both slots' CRCs: `None` when neither holds a
/// committed image (a fresh object, or both torn).
pub fn committed_slot<S: ImageRead + ?Sized>(slots: &S) -> Option<CommittedSlot> {
  let (zero, one) = slots_of(slots.image_len());
  match (slot_generation(slots, zero), slot_generation(slots, one)) {
    (Some(g0), Some(g1)) if g1 > g0 => Some(CommittedSlot {
      generation: g1,
      second: true,
    }),
    (Some(g0), _) => Some(CommittedSlot {
      generation: g0,
      second: false,
    }),
    (None, Some(g1)) => Some(CommittedSlot {
      generation: g1,
      second: true,
    }),
    (None, None) => None,
  }
}

/// Publishes `image` into one of two alternating, generation-tagged slots (§4.8), so that an interrupted, torn or
/// too-large publish never destroys the last committed image. The write always lands in the slot that does *not*
/// hold the committed image, and the commit *is* the CRC becoming valid over `[generation ++ image]`. A crash
/// mid-write leaves that slot's CRC wrong, so recovery ignores it and reads the other slot — untouched, still the
/// last committed. `known` is the committed slot as the publisher last learned it ([`CommittedSlot`]); `None`
/// checks both slots first. Returns the frame's length and the new committed slot; refuses [`VfsError::NoSpace`] if
/// a slot cannot hold the frame, and on that refusal, too, the committed slot is untouched.
fn publish_committed<S: ImageWrite + ?Sized>(
  slots: &mut S,
  image: &[u8],
  known: Option<CommittedSlot>,
) -> Result<(usize, CommittedSlot), VfsError> {
  let committed = known.or_else(|| committed_slot(slots));
  let next = committed
    .map_or(0, |slot| slot.generation)
    .checked_add(1)
    .ok_or(VfsError::FileTooLarge)?;
  // The slot the committed image is not in; the first slot when nothing is committed yet.
  let second = committed.is_some_and(|slot| !slot.second);
  let (zero, one) = slots_of(slots.image_len());
  let target = if second { one } else { zero };
  let total = frame(next, image, slots, target)?;
  Ok((
    total,
    CommittedSlot {
      generation: next,
      second,
    },
  ))
}

/// The last committed image in double-buffered `slots` (§4.8): the CRC-valid slot with the higher
/// generation, copied out, or `None` if neither slot holds one (a fresh object, or both torn). A torn
/// slot fails its CRC and is skipped, so an interrupted publish falls back to the previous committed
/// image; a slot that is CRC-valid but decodes wrong is left for the caller to refuse, never a false
/// success.
fn recover_committed<S: ImageRead + ?Sized>(slots: &S) -> Result<Option<Vec<u8>>, VfsError> {
  let (zero, one) = slots_of(slots.image_len());
  let chosen = match (slot_image(slots, zero), slot_image(slots, one)) {
    (Some((g0, p0)), Some((g1, p1))) => Some(if g0 >= g1 { p0 } else { p1 }),
    (Some((_, p0)), None) => Some(p0),
    (None, Some((_, p1))) => Some(p1),
    (None, None) => None,
  };
  chosen
    .map(|framed| {
      let mut image = vec![0u8; framed.len];
      slots.image_read(framed.at, &mut image).map(|()| image)
    })
    .transpose()
}

/// A slot's generation, if its frame is CRC-valid and carries one.
fn slot_generation<S: ImageRead + ?Sized>(slots: &S, slot: Slot) -> Option<u64> {
  slot_image(slots, slot).map(|(generation, _)| generation)
}

/// A CRC-valid slot's generation and where the image after it lies, or `None` for an empty or torn slot.
fn slot_image<S: ImageRead + ?Sized>(slots: &S, slot: Slot) -> Option<(u64, Framed)> {
  let framed = unframe(slots, slot).ok().flatten()?;
  if framed.len < SLOT_GEN_WIDTH {
    return None;
  }
  let mut generation = [0u8; SLOT_GEN_WIDTH];
  slots.image_read(framed.at, &mut generation).ok()?;
  Some((
    u64::from_le_bytes(generation),
    Framed {
      at: framed.at.saturating_add(SLOT_GEN_WIDTH),
      len: framed.len - SLOT_GEN_WIDTH,
    },
  ))
}

/// The image of `kind`.
const fn kind_image(kind: Kind) -> KindImage {
  match kind {
    Kind::File => KindImage::File,
    Kind::Dir => KindImage::Dir,
    Kind::Symlink => KindImage::Symlink,
    Kind::Fifo => KindImage::Fifo,
    Kind::Socket => KindImage::Socket,
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
pub(crate) fn quota_image(quota: &Quota) -> QuotaImage {
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
  /// path, a symlink's target. An overlay's base plane is imaged through `host` (A-48); without a host it
  /// refuses with [`VfsError::RecoveryIncomplete`], so a base-backed volume is never imaged as if it were
  /// only its overlay.
  pub fn to_image(
    &self,
    store: &Store,
    host: Option<&mut dyn crate::host::HostFs>,
  ) -> Result<VolumeImage, VfsError> {
    let base = match (&self.base, host) {
      (Some(plane), Some(host)) => Some(plane.image(host)?),
      (Some(_), None) => return Err(VfsError::RecoveryIncomplete),
      (None, _) => None,
    };
    let (inodes, origin_shared) = self.capture_head(store)?;
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
      orphans: self.orphans.keys().map(|no| no.0).collect(),
      origin_shared,
      base,
      references: self
        .attachment_references()
        .into_iter()
        .map(|(attachment, inodes)| AttachmentReferences {
          attachment,
          inodes: inodes
            .into_iter()
            .map(|(no, count)| InodeReferences { inode: no.0, count })
            .collect(),
        })
        .collect(),
    })
  }

  /// The head's inodes, and for a clone the numbers it shares with its origin snapshot (A-64): an inode born at or
  /// before the origin epoch is the origin snapshot's record (everything a clone makes is born after it), carried
  /// by number only.
  fn capture_head(&self, store: &Store) -> Result<(Vec<InodeImage>, Vec<u64>), VfsError> {
    let Some(origin) = self.origin_epoch else {
      return Ok((self.capture_tree(store, self.inode_root)?, Vec::new()));
    };
    let mut handles = Vec::new();
    trie::walk(&store.tries, self.inode_root, &mut handles);
    let mut inodes = Vec::with_capacity(handles.len());
    let mut shared = Vec::new();
    for handle in handles {
      let inode = store.inodes.get(handle)?;
      if inode.born <= origin {
        shared.push(inode.no.0);
      } else {
        inodes.push(self.image_of_inode(store, inode)?);
      }
    }
    Ok((inodes, shared))
  }

  /// Captures every inode reachable from `inode_root`, in number order: the versions that root holds, so a
  /// snapshot's frozen inodes are captured as frozen.
  fn capture_tree(
    &self,
    store: &Store,
    inode_root: Handle<TrieNode>,
  ) -> Result<Vec<InodeImage>, VfsError> {
    let mut handles = Vec::new();
    trie::walk(&store.tries, inode_root, &mut handles);
    let mut inodes = Vec::with_capacity(handles.len());
    for handle in handles {
      let inode = store.inodes.get(handle)?;
      inodes.push(self.image_of_inode(store, inode)?);
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
      if !matches!(
        inode.kind,
        Kind::File | Kind::Symlink | Kind::Fifo | Kind::Socket
      ) {
        // Directories are always carried in full; their entries are small and their structure is
        // this snapshot's own.
        inodes.push(self.image_of_inode(store, inode)?);
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
      // A clone's snapshot holding a version from the clone's origin shares the origin's record (A-64).
      if self.origin_epoch.is_some_and(|origin| inode.born <= origin) {
        shared.push(SharedInode {
          number: no.0,
          source: SharedSource::Origin,
        });
        continue;
      }
      // A version that diverged from the head: dedup it against earlier snapshots by content. The
      // crc buckets the lookup; the body decides the match.
      let image = self.image_of_inode(store, inode)?;
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

  /// The image of one inode, capturing its body faithfully or refusing an un-captured kind.
  pub(crate) fn image_of_inode(
    &self,
    store: &Store,
    inode: &Inode,
  ) -> Result<InodeImage, VfsError> {
    self.inode_image(store, inode, Entries::Captured)
  }

  /// The image of one inode as a delta carries it (A-68): a directory's own fields without its entries, which the
  /// delta carries by name. Enumerating them here cost every create its parent's whole entry list (A-89).
  pub(crate) fn image_of_inode_without_entries(
    &self,
    store: &Store,
    inode: &Inode,
  ) -> Result<InodeImage, VfsError> {
    self.inode_image(store, inode, Entries::Omitted)
  }

  fn inode_image(
    &self,
    store: &Store,
    inode: &Inode,
    entries: Entries,
  ) -> Result<InodeImage, VfsError> {
    let body = if let Body::Base(base) = &inode.body {
      let pinned = base
        .pinned
        .iter()
        .map(|extent| extent_image(store, extent))
        .collect::<Result<_, VfsError>>()?;
      BodyImage::Base {
        witness: base.witness.as_deref().copied(),
        pinned,
        base_len: base.base_len,
        lost: base.lost,
      }
    } else {
      match inode.kind {
        Kind::Dir => {
          let Body::Directory(handle) = inode.body else {
            return Err(VfsError::RecoveryIncomplete);
          };
          let directory = store.dirs.get(handle)?;
          BodyImage::Directory {
            entries: match entries {
              Entries::Captured => self.dir_entries(store, inode)?,
              Entries::Omitted => Vec::new(),
            },
            base: directory.base,
            origin: directory.origin.as_ref().map(|path| path.to_string()),
          }
        }
        Kind::Symlink => match &inode.body {
          Body::Symlink(target) => BodyImage::Symlink {
            target: target.to_string(),
          },
          _ => return Err(VfsError::RecoveryIncomplete),
        },
        Kind::File => file_body_image(store, &inode.body)?,
        Kind::Fifo | Kind::Socket => BodyImage::Empty,
      }
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
      xattrs: inode
        .xattrs
        .as_deref()
        .map(|table| {
          table
            .iter()
            .map(|(name, attribute)| XattrImage {
              name: name.to_vec(),
              attribute: attribute.0,
            })
            .collect()
        })
        .unwrap_or_default(),
      attribute_of: inode.attribute_of.map(|owner| owner.0),
      sidecar: inode
        .xattrs
        .as_deref()
        .and_then(|table| table.sidecar)
        .map(|copy| copy.0),
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
        Child::File(no) | Child::Symlink(no) | Child::Fifo(no) | Child::Socket(no) => Some(no),
        Child::Dir(handle) => Some(
          store
            .dirs
            .get(handle)
            .map_err(|_| VfsError::StaleHandle)?
            .inode,
        ),
        Child::Whiteout => None,
      };
      entries.push(EntryImage {
        name: entry.name.to_string(),
        child: child.map(|inode| inode.0),
      });
    }
    // Canonical order: by folded name under the volume's policy (the name itself under `Exact`). Names are
    // distinct within a directory under that policy, so this total order is independent of the small/indexed
    // representation the entries happened to be stored in, which makes the image deterministic and a rebuild's
    // re-capture byte-identical; and a delta's replay finds a name's entry by binary search (A-89).
    let policy = self.policy;
    entries.sort_by(|a, b| entry_order(policy, &a.name, &b.name));
    Ok(entries)
  }
}

/// Whether an inode's image captures a directory's entries.
#[derive(Clone, Copy)]
enum Entries {
  /// Every entry, in canonical order: a full image.
  Captured,
  /// None: a delta, which carries changed entries by name.
  Omitted,
}

/// The canonical order of a directory image's entries (A-89): by folded name under `policy`, which is the name's own
/// order under `Exact`. Allocation-free for an ASCII name under `Fold` (the common case).
pub(crate) fn entry_order(policy: NameEquivalence, a: &str, b: &str) -> std::cmp::Ordering {
  policy.folded(a).cmp(policy.folded(b))
}

/// A block's image (A-64).
fn block_image(block: &slates_mem::arena::Extent) -> BlockImage {
  BlockImage {
    region: block.region(),
    offset: u64::try_from(block.offset()).unwrap_or(u64::MAX),
    len: u64::try_from(block.len()).unwrap_or(u64::MAX),
  }
}

/// A sealed extent's image: a chunk-backed one names its chunk's block, born epoch and identity (A-64).
fn extent_image(store: &Store, extent: &Extent) -> Result<ExtentImage, VfsError> {
  let source = match extent.src {
    ExtentSrc::Zero => ExtentSourceImage::Zero,
    ExtentSrc::Chunk { chunk, at } => {
      let chunk = store
        .content
        .chunk(chunk)
        .ok_or(VfsError::RecoveryIncomplete)?;
      ExtentSourceImage::Chunk {
        chunk: ChunkImage {
          block: block_image(&chunk.block),
          used: chunk.len,
          born: chunk.born.0,
          identity: chunk.identity,
          seal: match &chunk.seal {
            Some(seal) => Some(SealImage {
              // A key the cipher cannot name could not be opened by a later daemon: refused, never imaged unopenable.
              key: store
                .content
                .key_identity(seal.key)
                .ok_or(VfsError::RecoveryIncomplete)?,
              version: seal.version,
              tags: store.content.chunk_tags(seal),
            }),
            None => None,
          },
        },
        at,
      }
    }
  };
  Ok(ExtentImage {
    offset: extent.off,
    len: extent.len,
    source,
  })
}

/// A file's body image, as the store holds it (A-64): no byte is copied but inline content.
fn file_body_image(store: &Store, body: &Body) -> Result<BodyImage, VfsError> {
  let extents = |sealed: &[Extent]| {
    sealed
      .iter()
      .map(|extent| extent_image(store, extent))
      .collect::<Result<Vec<_>, VfsError>>()
  };
  match body {
    Body::None => Ok(BodyImage::Empty),
    Body::Inline(bytes) => Ok(BodyImage::Inline {
      bytes: bytes.clone(),
    }),
    Body::Sealed(sealed) => Ok(BodyImage::Chunked {
      extents: extents(sealed)?,
      open: None,
    }),
    Body::Open { open, sealed } => Ok(BodyImage::Chunked {
      extents: extents(sealed)?,
      open: Some(OpenImage {
        offset: open.off,
        len: open.len,
        block: block_image(&open.block),
        born: open.born.0,
      }),
    }),
    Body::Directory(_) | Body::Symlink(_) | Body::Base(_) => Err(VfsError::RecoveryIncomplete),
  }
}

/// The image reference for a snapshot id.
pub(crate) const fn snap_ref(id: SnapshotId) -> SnapshotRef {
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
    KindImage::Fifo => Kind::Fifo,
    KindImage::Socket => Kind::Socket,
  }
}

/// The name policy an image policy names.
pub(crate) const fn policy_from_image(policy: PolicyImage) -> NameEquivalence {
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
  /// entries are rebuilt, and each file's body is placed over the blocks `claims` took for it in the arena
  /// range, whose bytes survived the daemon (A-64), so no byte is copied; the head's content histogram is
  /// recounted from the placed bodies. `clock` and `journal_bytes` are re-supplied, as they are on any construction; a dynamic
  /// quota is restored too (its source is the stateless [`BudgetGrowth`], so only its counters need
  /// travel), and its granted growth is re-acquired from the rebuilt budget by the caller.
  ///
  /// The rebuild runs entirely at the image's head epoch, so nothing copies-on-write while it is
  /// built; each inode's true birth epoch and version are restored at the end. CoW snapshots (with
  /// their ids, deadlists and cross-snapshot sharing), clone lineage and referenced-but-unlinked
  /// orphans (content and tracking) are all rebuilt; a base plane is refused rather than dropped (its
  /// own gate). What an orphan still awaits is the anchor handle handoff that restores its open
  /// references.
  pub fn from_image(
    store: &mut Store,
    image: &VolumeImage,
    claims: &Claims,
    clock: Box<dyn Clock>,
    journal_bytes: usize,
    source: Option<(&mut dyn crate::host::HostFs, crate::host::HostDir)>,
  ) -> Result<Volume, VfsError> {
    // A clone shares its origin snapshot's records, so it recovers only beside its origin.
    if image.origin_epoch.is_some() {
      return Err(VfsError::RecoveryIncomplete);
    }
    Self::rebuild(store, image, claims, None, clock, journal_bytes, source)
  }

  /// A clone rebuilt from its image beside its recovered origin (A-64): it starts from `snapshot` of `origin` — its
  /// tree, its inode table and every record in them, shared as [`Volume::clone_of`] shares them — then the numbers
  /// the clone no longer holds leave its table and its own inodes (born after the origin epoch) are placed, each
  /// directory of its own entered with its entries. The origin's pin on the snapshot came back with the origin's
  /// image, so none is taken here. Refuses `RecoveryIncomplete` for an image that is not a clone of that snapshot,
  /// or that names a shared number the snapshot does not hold.
  #[allow(clippy::too_many_arguments)] // the recovery inputs, each one the caller holds apart from the others
  pub fn clone_from_image(
    store: &mut Store,
    image: &VolumeImage,
    claims: &Claims,
    origin: &Volume,
    snapshot: SnapshotId,
    clock: Box<dyn Clock>,
    journal_bytes: usize,
    source: Option<(&mut dyn crate::host::HostFs, crate::host::HostDir)>,
  ) -> Result<Volume, VfsError> {
    let snap = origin
      .snapshots
      .get(crate::volume::snapshot_handle(snapshot))
      .map_err(|_| VfsError::RecoveryIncomplete)?;
    if image.origin_epoch != Some(snap.epoch.0) {
      return Err(VfsError::RecoveryIncomplete);
    }
    let from = CloneOrigin {
      root: snap.root,
      inode_root: snap.inode_root,
    };
    Self::rebuild(
      store,
      image,
      claims,
      Some(from),
      clock,
      journal_bytes,
      source,
    )
  }

  /// The rebuild both [`Volume::from_image`] and [`Volume::clone_from_image`] run.
  fn rebuild(
    store: &mut Store,
    image: &VolumeImage,
    claims: &Claims,
    origin: Option<CloneOrigin>,
    clock: Box<dyn Clock>,
    journal_bytes: usize,
    source: Option<(&mut dyn crate::host::HostFs, crate::host::HostDir)>,
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
    let mut vol = match origin {
      None => Volume::recovery_shell(store, seed, clock, journal_bytes)?,
      Some(from) => Volume::shell_over(
        store,
        seed,
        (from.root, from.inode_root),
        clock,
        journal_bytes,
      ),
    };
    let mut source = source;
    let outcome = (|| {
      vol.base = match (&image.base, source.as_mut()) {
        (Some(image), Some((host, root))) => Some(crate::base::BasePlane::recover(
          image, *host, *root, root_no,
        )?),
        (None, None) => None,
        _ => return Err(VfsError::RecoveryIncomplete),
      };

      // The head, into the shell's roots. Recovery places inodes by number (not `next_no`), so set the
      // live-inode count (§4.2) to the recovered head's inode count directly.
      match origin {
        None => vol.rebuild_passes(store, claims, &image.inodes, root_no, epoch)?,
        Some(from) => vol.rebuild_clone_head(store, claims, image, from)?,
      }
      let held = image.inodes.len().saturating_add(image.origin_shared.len());
      vol.live_inodes = u64::try_from(held).unwrap_or(u64::MAX);
      // The head's live-entry count (built by rebuild_entries); snapshot rebuilds below run through the
      // same dir_insert and perturb it, so keep it and restore after (§4.2 namespace, head-reachable).
      let head_entries = vol.live_entries;

      // The head's inode table and each inode's image, so a snapshot can share an inode it holds
      // unchanged with the head instead of rebuilding a private copy (§4.2 CoW-sharing efficiency).
      let head_inode_root = vol.inode_root;
      let head_images: BTreeMap<u64, &InodeImage> =
        image.inodes.iter().map(|i| (i.no, i)).collect();
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
        origin_inode_root: origin.map(|from| from.inode_root),
        head_images: &head_images,
        canonical_inode: &canonical_inode,
      };

      // Each snapshot, into its own roots, in id order so a fresh slab reproduces its id and a
      // cross-snapshot reference always resolves to an already-rebuilt canonical. Deadlists are left
      // empty here and filled by one global pass below, once every tree (and its sharing) exists.
      let mut snapshots: Vec<&SnapshotImage> = image.snapshots.iter().collect();
      snapshots.sort_by_key(|s| (s.id.index, s.id.generation));
      for snap in &snapshots {
        vol.rebuild_snapshot(store, claims, snap, root_no, &refs)?;
      }
      vol.rebuild_deadlists(store, &snapshots)?;
      vol.recount_head_bytes(store);
      vol.live_entries = head_entries;
      vol.last_snapshot = image.last_snapshot.map(to_snapshot_id);
      // Restore the orphan tracking (§4.8): the inodes are already rebuilt with the rest, and marking
      // them orphans again means a reacquired handle's last close reclaims them rather than leaking.
      vol.orphans = image.orphans.iter().map(|no| (InodeNo(*no), 0)).collect();
      // And every recorded attachment's references (A-61): the owner then settles them, giving them back to the
      // attachments that survived and releasing the rest. A reference to an inode the image lacks is a corrupt
      // image, refused.
      for owner in &image.references {
        for held in &owner.inodes {
          vol
            .restore_references(store, owner.attachment, InodeNo(held.inode), held.count)
            .map_err(|_| VfsError::RecoveryIncomplete)?;
        }
      }
      // Re-establish the retention the rebuilt deadlists and orphans hold against the shard budgets
      // (§4.2 accounting through recovery); a shard that cannot back what was admitted before the
      // restart refuses, and the half-built volume returns its slots and blocks rather than leaking.
      vol.reestablish_retention(store)?;
      Ok(())
    })();
    if let Err(refusal) = outcome {
      if let Some(base) = vol.base.take()
        && let Some((host, _)) = source
      {
        base.release_sources(host);
      }
      vol.discard_partial(store)?;
      return Err(refusal);
    }
    // Every open extent the image names is due an idle sweep (A-99), as if written before this daemon's first one: no
    // write of this life stamps it, and it would otherwise stay plaintext until the file is written again.
    for inode in &image.inodes {
      if matches!(inode.body, BodyImage::Chunked { open: Some(_), .. }) {
        vol.written.insert(InodeNo(inode.no), 0);
      }
    }
    Ok(vol)
  }

  /// A clone's head over its origin snapshot's (A-64): the numbers the origin snapshot holds that the clone neither
  /// shares nor owns leave the clone's table (copy-on-write, so the origin's table is untouched), the clone's own
  /// inodes are placed, its root is its own node when it owns the root, and each directory of its own gets its
  /// entries — a shared subdirectory named by the origin's node, as the live clone names it.
  fn rebuild_clone_head(
    &mut self,
    store: &mut Store,
    claims: &Claims,
    image: &VolumeImage,
    from: CloneOrigin,
  ) -> Result<(), VfsError> {
    let epoch = self.epoch;
    let owned: BTreeSet<u64> = image.inodes.iter().map(|i| i.no).collect();
    let shared: BTreeSet<u64> = image.origin_shared.iter().copied().collect();
    if !owned.is_disjoint(&shared) {
      return Err(VfsError::RecoveryIncomplete);
    }
    let mut kinds: BTreeMap<u64, KindImage> = BTreeMap::new();
    let mut dirs: BTreeMap<u64, Handle<DirNode>> = BTreeMap::new();
    let mut gone = Vec::new();
    let mut handles = Vec::new();
    trie::walk(&store.tries, from.inode_root, &mut handles);
    let mut shared_entries = 0usize;
    for handle in handles {
      let inode = store.inodes.get(handle)?;
      let no = inode.no.0;
      if shared.contains(&no) {
        kinds.insert(no, kind_image(inode.kind));
        if let Body::Directory(node) = inode.body {
          dirs.insert(no, node);
          shared_entries =
            shared_entries.saturating_add(store.dirs.get(node)?.live_len(&store.blocks));
        }
      } else if !owned.contains(&no) {
        gone.push(InodeNo(no));
      }
    }
    if shared.iter().any(|no| !kinds.contains_key(no)) {
      return Err(VfsError::RecoveryIncomplete);
    }
    for no in gone {
      self.table_remove(store, no)?;
    }
    for image_inode in &image.inodes {
      let no = InodeNo(image_inode.no);
      kinds.insert(image_inode.no, image_inode.kind);
      let body = body_for(store, claims, image_inode, no, epoch, &mut dirs)?;
      let inode = Inode::new(no, epoch, kind_from_image(image_inode.kind), 0, body);
      let handle = store.inodes.insert(inode)?;
      self.table_set(store, no, handle)?;
    }
    if owned.contains(&image.root_no) {
      self.root = *dirs
        .get(&image.root_no)
        .ok_or(VfsError::RecoveryIncomplete)?;
    }
    self.rebuild_entries(store, &image.inodes, &kinds, &dirs)?;
    self.restore_identities(store, &image.inodes)?;
    self.live_entries = self
      .live_entries
      .saturating_add(u64::try_from(shared_entries).unwrap_or(u64::MAX));
    Ok(())
  }

  /// Runs the rebuild passes for one tree (the head or a snapshot) against the volume's current
  /// roots and epoch: place every inode at its number with its body, rebuild directory entries, then
  /// restore each inode's true identity.
  fn rebuild_passes(
    &mut self,
    store: &mut Store,
    claims: &Claims,
    inodes: &[InodeImage],
    root_no: InodeNo,
    epoch: Epoch,
  ) -> Result<(), VfsError> {
    let kinds: BTreeMap<u64, KindImage> = inodes.iter().map(|i| (i.no, i.kind)).collect();
    let mut dirs: BTreeMap<u64, Handle<DirNode>> = BTreeMap::new();
    dirs.insert(root_no.0, self.root);
    self.place_inodes(store, claims, inodes, root_no, epoch, &mut dirs)?;
    self.rebuild_entries(store, inodes, &kinds, &dirs)?;
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
    claims: &Claims,
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

    let outcome = self.rebuild_snapshot_tree(store, claims, snap, root_no, epoch, refs);

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
  /// Processing newest first and skipping anything the head reaches or an already-processed (newer) snapshot
  /// claimed puts each object where the live volume kept it, so a drop in any order neither leaks nor
  /// double-frees.
  fn rebuild_deadlists(
    &mut self,
    store: &mut Store,
    snapshots: &[&SnapshotImage],
  ) -> Result<(), VfsError> {
    let mut order: Vec<&SnapshotImage> = snapshots.to_vec();
    order.sort_by_key(|s| std::cmp::Reverse(s.epoch));
    let mut claimed: HashSet<(Discriminant<Dead>, u32, u32)> = HashSet::new();
    // Every chunk the head reaches is the head's, even under a snapshot's diverged inode: the images name
    // chunks, so a diverged version and the head share every window the head did not rewrite (A-64), and the
    // live volume released only the rewritten ones onto the deadlist.
    let mut head_inodes = Vec::new();
    trie::walk(&store.tries, self.inode_root, &mut head_inodes);
    for handle in head_inodes {
      if let Ok(inode) = store.inodes.get(handle) {
        for dead in crate::volume::body_chunks(store, &inode.body) {
          claimed.insert(dead_key(&dead));
        }
      }
    }
    for snap in order {
      // Objects the head still holds are the head's, not this snapshot's; everything else the
      // snapshot reaches is a candidate, then filtered to what no newer snapshot already claimed.
      let head_shared: BTreeSet<u64> = snap
        .shared
        .iter()
        .filter_map(|s| match s.source {
          // The head's records, and a clone's origin's (A-64), are not this snapshot's to free.
          SharedSource::Head | SharedSource::Origin => Some(s.number),
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
    claims: &Claims,
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
        SharedSource::Origin => {
          let source_root = refs.origin_inode_root.ok_or(VfsError::RecoveryIncomplete)?;
          let handle =
            trie::get(&store.tries, source_root, no).ok_or(VfsError::RecoveryIncomplete)?;
          (source_root, kind_image(store.inodes.get(handle)?.kind))
        }
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
      let body = body_for(store, claims, image_inode, no, epoch, &mut dirs)?;
      let inode = Inode::new(no, epoch, kind_from_image(image_inode.kind), 0, body);
      let handle = store.inodes.insert(inode)?;
      self.table_set(store, no, handle)?;
    }
    self.rebuild_entries(store, &snap.inodes, &kinds, &dirs)?;
    self.restore_identities(store, &snap.inodes)?;
    // A clone's snapshot rebuilds its directories privately; a record the live snapshot shared with the origin
    // (born at or before the origin epoch) is this snapshot's own copy now, born with it, so the snapshot's drop
    // frees it rather than leaving it to the origin (A-64).
    if let Some(origin) = self.origin_epoch {
      for image_inode in snap.inodes.iter().filter(|i| Epoch(i.born) <= origin) {
        let handle = trie::get(&store.tries, self.inode_root, InodeNo(image_inode.no))
          .ok_or(VfsError::RecoveryIncomplete)?;
        store.inodes.get_mut(handle)?.born = epoch;
      }
    }
    Ok(())
  }

  /// Pass one: place every non-root inode at its own number, born at the head epoch, with a fresh
  /// directory node (parent and name fixed up when the parent's entries are rebuilt), a symlink's
  /// target, or a file's body over its claimed blocks. The root already exists.
  fn place_inodes(
    &mut self,
    store: &mut Store,
    claims: &Claims,
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
      let body = body_for(store, claims, image_inode, no, epoch, dirs)?;
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
      let BodyImage::Directory {
        entries,
        base,
        origin,
      } = &image_inode.body
      else {
        continue;
      };
      let parent_no = InodeNo(image_inode.no);
      let parent = *dirs
        .get(&image_inode.no)
        .ok_or(VfsError::RecoveryIncomplete)?;
      store.dirs.get_mut(parent)?.base = *base;
      store.dirs.get_mut(parent)?.origin = origin.as_ref().map(|path| path.as_str().into());
      for e in entries {
        let child = child_for(store, e, parent_no, kinds, dirs)?;
        self.dir_insert(store, parent, &e.name, child)?;
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
      inode.attribute_of = image_inode.attribute_of.map(InodeNo);
    }
    // Tables after every identity: an attribute inode is checked in the rebuilt table, where a
    // snapshot's shared attribute inode (in its `shared` list, not `inodes`) is also found.
    for image_inode in inodes
      .iter()
      .filter(|image| !image.xattrs.is_empty() || image.sidecar.is_some())
    {
      let owner = InodeNo(image_inode.no);
      let mut table = self.xattrs_from_image(store, &image_inode.xattrs, owner)?;
      if let Some(copy) = image_inode.sidecar {
        let copy = InodeNo(copy);
        if !self.attribute_of_owner(store, copy, owner) {
          return Err(VfsError::RecoveryIncomplete);
        }
        table.sidecar = Some(copy);
      }
      let handle =
        trie::get(&store.tries, self.inode_root, owner).ok_or(VfsError::RecoveryIncomplete)?;
      store.inodes.get_mut(handle)?.xattrs = Some(table);
    }
    Ok(())
  }

  /// Whether the rebuilt table holds `attribute` as an attribute inode of `owner`.
  fn attribute_of_owner(&self, store: &Store, attribute: InodeNo, owner: InodeNo) -> bool {
    trie::get(&store.tries, self.inode_root, attribute)
      .and_then(|handle| store.inodes.get(handle).ok())
      .and_then(|inode| inode.attribute_of)
      == Some(owner)
  }

  /// An owner's attribute table from its image. Refuses `RecoveryIncomplete` when a name repeats or
  /// is not a valid attribute name, or when a named inode is not in the rebuilt table as an attribute
  /// of this owner: an image no volume could have written, so the rebuild refuses rather than present
  /// attributes that are not the volume's (§4.8).
  fn xattrs_from_image(
    &self,
    store: &Store,
    xattrs: &[XattrImage],
    owner: InodeNo,
  ) -> Result<Box<crate::inode::XattrTable>, VfsError> {
    let mut pairs = Vec::with_capacity(xattrs.len());
    for xattr in xattrs {
      crate::xattr::check_name(&xattr.name).map_err(|_| VfsError::RecoveryIncomplete)?;
      let attribute = InodeNo(xattr.attribute);
      if !self.attribute_of_owner(store, attribute, owner) {
        return Err(VfsError::RecoveryIncomplete);
      }
      pairs.push((xattr.name.clone().into_boxed_slice(), attribute));
    }
    crate::inode::XattrTable::from_pairs(pairs)
      .map(Box::new)
      .ok_or(VfsError::RecoveryIncomplete)
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
/// The origin snapshot a clone is rebuilt over (A-64): its directory root and inode table.
#[derive(Clone, Copy)]
struct CloneOrigin {
  root: Handle<DirNode>,
  inode_root: Handle<TrieNode>,
}

struct SharingRefs<'a> {
  head_inode_root: Handle<TrieNode>,
  /// A clone's origin snapshot's inode table (A-64).
  origin_inode_root: Option<Handle<TrieNode>>,
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
    // A chunked body's references and offsets are its identity (the same blocks at another offset are a
    // different file), so the crc covers the body's canonical wire form, as for a base body.
    BodyImage::Inline { .. } | BodyImage::Chunked { .. } => crc32c(&body.to_bytes()),
    BodyImage::Symlink { target } => crc32c(target.as_bytes()),
    BodyImage::Empty => crc32c(&[]),
    BodyImage::Directory { .. } => 0,
    BodyImage::Base { .. } => crc32c(&body.to_bytes()),
  }
}

fn body_for(
  store: &mut Store,
  claims: &Claims,
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
    KindImage::Fifo | KindImage::Socket => {
      if !matches!(image_inode.body, BodyImage::Empty) || image_inode.attrs.size != 0 {
        return Err(VfsError::RecoveryIncomplete);
      }
      Ok(Body::None)
    }
    KindImage::Symlink => match &image_inode.body {
      BodyImage::Symlink { target } => Ok(Body::Symlink(target.as_str().into())),
      _ => Err(VfsError::RecoveryIncomplete),
    },
    KindImage::File => file_body_from_image(claims, &image_inode.body, image_inode.attrs.size),
  }
}

/// A file's body over its claimed blocks (A-64), refusing one no volume could hold: content past the file's
/// size, or sealed extents empty, out of order or overlapping (a corrupt image is a typed refusal, never a
/// silently different file).
fn file_body_from_image(claims: &Claims, body: &BodyImage, size: u64) -> Result<Body, VfsError> {
  match body {
    BodyImage::Empty => Ok(Body::None),
    BodyImage::Inline { bytes } => {
      if u64::try_from(bytes.len()).map_or(true, |len| len > size) {
        return Err(VfsError::RecoveryIncomplete);
      }
      Ok(Body::Inline(bytes.clone()))
    }
    BodyImage::Chunked { extents, open } => {
      let sealed = extents_from_image(claims, extents, size)?;
      match open {
        None => Ok(Body::Sealed(sealed)),
        Some(open) => {
          let end = open
            .offset
            .checked_add(open.len)
            .ok_or(VfsError::RecoveryIncomplete)?;
          if end > size {
            return Err(VfsError::RecoveryIncomplete);
          }
          Ok(Body::Open {
            open: Box::new(claims.open(open)?),
            sealed,
          })
        }
      }
    }
    BodyImage::Base {
      witness,
      pinned,
      base_len,
      lost,
    } => Ok(Body::Base(crate::inode::BaseBody {
      witness: witness.map(Box::new),
      pinned: extents_from_image(claims, pinned, size)?,
      base_len: *base_len,
      descriptor: None,
      lost: *lost,
    })),
    BodyImage::Directory { .. } | BodyImage::Symlink { .. } => Err(VfsError::RecoveryIncomplete),
  }
}

/// Sealed extents over their claimed chunks, checked ascending, non-overlapping, non-empty and within `size`.
fn extents_from_image(
  claims: &Claims,
  extents: &[ExtentImage],
  size: u64,
) -> Result<Vec<Extent>, VfsError> {
  let mut previous_end = 0u64;
  let mut out = Vec::with_capacity(extents.len());
  for extent in extents {
    let end = extent
      .offset
      .checked_add(extent.len)
      .ok_or(VfsError::RecoveryIncomplete)?;
    if extent.len == 0 || extent.offset < previous_end || end > size {
      return Err(VfsError::RecoveryIncomplete);
    }
    previous_end = end;
    let src = match &extent.source {
      ExtentSourceImage::Zero => ExtentSrc::Zero,
      &ExtentSourceImage::Chunk { ref chunk, at } => ExtentSrc::Chunk {
        chunk: claims.chunk(chunk)?,
        at,
      },
    };
    out.push(Extent {
      off: extent.offset,
      len: extent.len,
      src,
    });
  }
  Ok(out)
}

/// What a claimed block holds (A-64).
#[derive(Clone, Debug)]
enum Held {
  /// A sealed chunk, recorded in the chunk slab.
  Chunk(Handle<Chunk>, ChunkImage),
  /// An open extent's block.
  Open(slates_mem::arena::Extent, OpenImage),
}

/// The blocks a shard's recovered images name, claimed in its arena before any volume is rebuilt (A-64). Every
/// image is read first, so a block two volumes share (a clone and its origin's snapshot) is claimed once and
/// both rebuilt bodies name one chunk, as the live store did. The claims are then committed (the image in the
/// content object names them), so a block a refused volume's teardown frees is deferred, not reused, until a
/// newer image commits. [`Claims::sweep`] frees every claimed block no rebuilt volume reaches.
#[derive(Debug, Default)]
pub struct Claims {
  held: BTreeMap<(u16, u64), Held>,
}

impl Claims {
  /// Claims every block `images` name and commits them. Refused `RecoveryIncomplete`, with every claim given
  /// back, when a block is one no allocation could have made, overlaps another, or is named two different
  /// ways (as a chunk and an open extent, or with different lengths).
  pub fn prepare<'a>(
    store: &mut Store,
    images: impl IntoIterator<Item = &'a VolumeImage>,
  ) -> Result<Claims, VfsError> {
    let mut claims = Claims::default();
    let mut claimed = Ok(());
    'images: for image in images {
      let snapshot_inodes = image.snapshots.iter().flat_map(|snap| snap.inodes.iter());
      for inode in image.inodes.iter().chain(snapshot_inodes) {
        claimed = claims.claim_body(store, &inode.body);
        if claimed.is_err() {
          break 'images;
        }
      }
    }
    if let Err(refusal) = claimed {
      claims.give_back(store);
      return Err(refusal);
    }
    store.content.arena_mut().commit_live();
    Ok(claims)
  }

  /// The blocks claimed.
  pub fn blocks(&self) -> usize {
    self.held.len()
  }

  fn claim_body(&mut self, store: &mut Store, body: &BodyImage) -> Result<(), VfsError> {
    match body {
      BodyImage::Chunked { extents, open } => {
        for extent in extents {
          self.claim_extent(store, extent)?;
        }
        if let Some(open) = open {
          self.claim_open(store, open)?;
        }
        Ok(())
      }
      BodyImage::Base { pinned, .. } => pinned
        .iter()
        .try_for_each(|extent| self.claim_extent(store, extent)),
      _ => Ok(()),
    }
  }

  fn claim_extent(&mut self, store: &mut Store, extent: &ExtentImage) -> Result<(), VfsError> {
    let &ExtentSourceImage::Chunk { ref chunk, at } = &extent.source else {
      return Ok(());
    };
    let end = u64::from(at)
      .checked_add(extent.len)
      .ok_or(VfsError::RecoveryIncomplete)?;
    if end > u64::from(chunk.used) {
      return Err(VfsError::RecoveryIncomplete);
    }
    let key = (chunk.block.region, chunk.block.offset);
    match self.held.get(&key) {
      Some(Held::Chunk(_, held)) if held == chunk => Ok(()),
      Some(_) => Err(VfsError::RecoveryIncomplete),
      None => {
        let block =
          store
            .content
            .claim_block(chunk.block.region, chunk.block.offset, chunk.block.len)?;
        let sealed = match &chunk.seal {
          Some(seal) => {
            let key = store
              .content
              .cipher_mut()
              .ok_or(VfsError::RecoveryIncomplete)
              .and_then(|cipher| cipher.reference(&seal.key));
            match key {
              Ok(key) => Some((key, seal.version, seal.tags.as_slice())),
              Err(refusal) => {
                let _ = store.content.give_back_block(block);
                return Err(refusal);
              }
            }
          }
          None => None,
        };
        let adopted = store.content.adopt_chunk(
          Chunk {
            born: Epoch(chunk.born),
            len: chunk.used,
            block,
            identity: chunk.identity,
            seal: None,
          },
          sealed,
        );
        let handle = match adopted {
          Ok(handle) => handle,
          Err(refusal) => {
            let _ = store.content.give_back_block(block);
            return Err(refusal);
          }
        };
        self.held.insert(key, Held::Chunk(handle, chunk.clone()));
        Ok(())
      }
    }
  }

  fn claim_open(&mut self, store: &mut Store, open: &OpenImage) -> Result<(), VfsError> {
    if open.len > open.block.len {
      return Err(VfsError::RecoveryIncomplete);
    }
    let key = (open.block.region, open.block.offset);
    match self.held.get(&key) {
      Some(Held::Open(_, held)) if held == open => Ok(()),
      Some(_) => Err(VfsError::RecoveryIncomplete),
      None => {
        let block =
          store
            .content
            .claim_block(open.block.region, open.block.offset, open.block.len)?;
        // The bytes past the imaged length (writes made after this image) are never served: every read stops
        // at the length and every write zero-fills a gap it extends over (`ChunkStore::write_open`).
        self.held.insert(key, Held::Open(block, *open));
        Ok(())
      }
    }
  }

  /// Gives back every claim of a refused preparation, before anything is committed.
  fn give_back(&mut self, store: &mut Store) {
    for (_, held) in std::mem::take(&mut self.held) {
      let _ = match held {
        Held::Chunk(handle, _) => store.content.give_back_chunk(handle),
        Held::Open(block, _) => store.content.give_back_block(block),
      };
    }
  }

  fn chunk(&self, chunk: &ChunkImage) -> Result<Handle<Chunk>, VfsError> {
    match self.held.get(&(chunk.block.region, chunk.block.offset)) {
      Some(Held::Chunk(handle, held)) if held == chunk => Ok(*handle),
      _ => Err(VfsError::RecoveryIncomplete),
    }
  }

  fn open(&self, open: &OpenImage) -> Result<OpenExtent, VfsError> {
    match self.held.get(&(open.block.region, open.block.offset)) {
      Some(Held::Open(block, held)) if held == open => Ok(OpenExtent {
        off: open.offset,
        len: open.len,
        block: *block,
        born: Epoch(open.born),
      }),
      _ => Err(VfsError::RecoveryIncomplete),
    }
  }

  /// Frees every claimed block that none of `volumes` reaches (A-64): a refused volume's, whatever its
  /// teardown did not free. Deferred, since the committed image names them. Returns the blocks freed.
  pub fn sweep<'a>(
    self,
    store: &mut Store,
    volumes: impl IntoIterator<Item = &'a Volume>,
  ) -> Result<usize, VfsError> {
    let mut reached = BTreeSet::new();
    for volume in volumes {
      volume.blocks_reached(store, &mut reached);
    }
    let mut freed = 0usize;
    for (key, held) in self.held {
      if reached.contains(&key) {
        continue;
      }
      match held {
        Held::Chunk(handle, _) if store.content.chunk(handle).is_some() => {
          store.content.free_chunk(handle)?;
        }
        Held::Open(block, _) if store.content.holds(block) => {
          store.content.release_block(block)?;
        }
        Held::Chunk(..) | Held::Open(..) => continue,
      }
      freed = freed.saturating_add(1);
    }
    Ok(freed)
  }
}

impl Volume {
  /// Adds the arena block of every chunk and open extent this volume reaches — through its head, every
  /// snapshot's tree and every deadlist — to `reached`, as (region, offset) (A-64, [`Claims::sweep`]).
  pub(crate) fn blocks_reached(&self, store: &Store, reached: &mut BTreeSet<(u16, u64)>) {
    let mut handles = Vec::new();
    trie::walk(&store.tries, self.inode_root, &mut handles);
    let mut key = |block: &slates_mem::arena::Extent| {
      reached.insert((
        block.region(),
        u64::try_from(block.offset()).unwrap_or(u64::MAX),
      ));
    };
    for (_, snap) in self.snapshots.iter() {
      trie::walk(&store.tries, snap.inode_root, &mut handles);
      for dead in snap.deadlist.items() {
        match dead {
          Dead::Chunk(chunk, _) => {
            if let Some(chunk) = store.content.chunk(*chunk) {
              key(&chunk.block);
            }
          }
          Dead::Inode(inode, _) => handles.push(*inode),
          Dead::Dir(..) | Dead::DirBlock(..) | Dead::Trie(..) => {}
        }
      }
    }
    for handle in handles {
      let Ok(inode) = store.inodes.get(handle) else {
        continue;
      };
      if let Body::Open { open, .. } = &inode.body {
        key(&open.block);
      }
      for extent in body_extents(&inode.body) {
        if let ExtentSrc::Chunk { chunk, .. } = extent.src
          && let Some(chunk) = store.content.chunk(chunk)
        {
          key(&chunk.block);
        }
      }
    }
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
  let Some(child) = entry.child else {
    return Ok(Child::Whiteout);
  };
  let child_no = InodeNo(child);
  match kinds.get(&child).ok_or(VfsError::RecoveryIncomplete)? {
    KindImage::File => Ok(Child::File(child_no)),
    KindImage::Symlink => Ok(Child::Symlink(child_no)),
    KindImage::Fifo => Ok(Child::Fifo(child_no)),
    KindImage::Socket => Ok(Child::Socket(child_no)),
    KindImage::Dir => {
      let handle = *dirs.get(&child).ok_or(VfsError::RecoveryIncomplete)?;
      let node = store.dirs.get_mut(handle)?;
      node.parent = Some(parent_no);
      node.name = entry.name.as_str().into();
      Ok(Child::Dir(handle))
    }
  }
}
