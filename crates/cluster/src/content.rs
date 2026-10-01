//! The content plane (§4.8 mechanism 1, "sealed content and records, under one quorum rule"; §4.10
//! "Content replication"; §4.11 "Archive"; D-17): a sealed snapshot's archive travels from its owner
//! to the snapshot's candidate holders over the fleet transport and is **verified before it is
//! held**, so an acknowledgement means "every byte the manifest references is now on this holder,
//! checked against its identity" — the design's "a holder acknowledges only after … verifying all
//! required bytes" (§4.10 "Placement closure"). The archive is the transfer unit and the missing
//! set is the resumption unit (§4.11: "resumable by missing set; the same container is the
//! replication transfer unit and the clone-from-archive source").
//!
//! Three exchanges, each one request/reply of its own kind on the holder's record session
//! ([`CONTENT_OFFER_STREAM`], [`CONTENT_PUT_STREAM`], [`CONTENT_FETCH_STREAM`] — so the holder's
//! `serve_once` dispatches by kind, never by guessing at bytes; every exchange rides a fresh stream id
//! in the `Bulk` class, so a content transfer never queues a record commit or a probe sharing the
//! session behind it):
//! - **`Offer` → `Missing`**: the owner names the object, sequence, manifest and every chunk identity
//!   the archive holds; the holder answers with the identities it lacks — a chunk already present on
//!   a candidate is never transferred again.
//! - **`Put` → `Ack`**: the owner ships the manifest with exactly the missing chunks as one archive;
//!   the holder decodes it (every chunk against its identity, the manifest against its hash, the
//!   whole against its trailer hash), refuses unless every chunk the manifest references is now
//!   held, stores, and acknowledges — an acknowledgement **bound** to the object, sequence and
//!   manifest, so a stale or foreign one cannot count toward a placement (the record plane's same
//!   discipline: network receipt is not acceptance).
//! - **`Fetch` → `Have`**: a reader — a takeover successor materializing the volume, a remote attach
//!   — asks a recorded holder for a manifest's whole archive by identity (§4.10 "fetches the
//!   manifest by identity from a recorded holder"), verified on arrival.
//!
//! The owner's dispatch ([`put_content`]) runs the offer round to every holder concurrently, builds
//! each holder's partial archive from the one archive in RAM (only the chunks it lacks are copied —
//! exactly the bytes that must cross the wire), then runs the put round and collects bound
//! acknowledgements to the quorum under the commit budget's progress-extension policy, the same
//! collector the record commit uses ([`crate::collect_bound`]). Both rounds are bounded by the
//! dispatch span, and holders still in flight at the return hand their sessions back through
//! [`Stragglers`]. The owner counts as a holder of its own content (it is a candidate, and it has
//! the bytes), so at `f = 0` the local hold is the placement with no dispatch — the same code path
//! (R8). Content goes to `f + 1` candidates first and is hedged to the rest after the measured p95
//! put latency (§4.8); the caller chooses each round's holders and the hedge trigger, this module
//! carries one round.
//!
//! Every parser here checks a declared count or length against what the bytes can hold before it
//! allocates (§4.9, the hostile-input rule) and refuses with a typed [`ContentError`]; the archive
//! itself is verified by `slates-archive`'s reader, which names a flipped bit by chunk.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use slates_archive::format::{ArchiveError, Chunk, Encoding};
use slates_archive::{Archive, Node, chunks_for};
use slates_db::register::{HostId, ObjectId, Placement, Quorum};
use slates_mem::arena::{ChunkArena, Extent};
use slates_mem::budget::{MetadataBudget, ShardBudget};
use slates_mem::error::MemError;
use slates_rt::error::RtError;
use slates_rt::futures::{detach, now_ns, spawn_child};
use slates_transport::connection::Priority;
use slates_transport::endpoint::Endpoint;
use slates_wire::Wire;

use crate::{
  ClusterError, Collected, CommitBudget, DispatchWait, Reply, Stragglers, collect_bound,
  request_within,
};

/// The stream an **offer** rides on a holder's record session — distinct from the record commit and
/// promotion streams so the holder dispatches by kind, and distinct from the put's and fetch's streams
/// because the exchanges of one round follow each other **back to back** on one session. Observed
/// (2026-09-10, instrumented over loopback): with the offer and the put on one id, the holder served the
/// offer and never the put that immediately followed, every round; on their own ids both are served. The
/// analysed cause (not itself traced at the transport): the put's frames reach the holder while it is
/// still closing the offer's exchange — its reply sent, the acknowledgement not yet in, the stream not yet
/// forgotten — and land in that finished assembler, to be discarded with it. A stream id reused only a
/// protocol period later, as the record plane does, never meets its predecessor still open; allocating
/// fresh ids per exchange, as QUIC does, is the transport's owed stream lifecycle.
/// Format: one stream id per RPC kind on a connection; a fixed label, not a tunable.
pub const CONTENT_OFFER_STREAM: u64 = 4;
/// The stream a **put** rides (see [`CONTENT_OFFER_STREAM`] for why each exchange has its own).
/// Format: one stream id per RPC kind on a connection; a fixed label, not a tunable.
pub const CONTENT_PUT_STREAM: u64 = 5;
/// The stream a **fetch** rides (see [`CONTENT_OFFER_STREAM`] for why each exchange has its own).
/// Format: one stream id per RPC kind on a connection; a fixed label, not a tunable.
pub const CONTENT_FETCH_STREAM: u64 = 6;

/// Whether `stream` carries a content exchange a holder answers with [`ContentHold::serve`].
pub fn is_content_stream(stream: u64) -> bool {
  matches!(
    stream,
    CONTENT_OFFER_STREAM | CONTENT_PUT_STREAM | CONTENT_FETCH_STREAM
  )
}

/// Format: the message kind byte that leads every content message.
const KIND_OFFER: u8 = 1;
/// Format: the kind byte of a holder's missing-set reply.
const KIND_MISSING: u8 = 2;
/// Format: the kind byte of an archive put.
const KIND_PUT: u8 = 3;
/// Format: the kind byte of a holder's acknowledgement.
const KIND_ACK: u8 = 4;
/// Format: the kind byte of a fetch by manifest identity.
const KIND_FETCH: u8 = 5;
/// Format: the kind byte of a fetch's answer.
const KIND_HAVE: u8 = 6;
/// Format: the width of a BLAKE3 identity on the wire.
const HASH_BYTES: usize = 32;
/// Format: the width of an object id on the wire.
const OBJECT_BYTES: usize = size_of::<ObjectId>();

/// A content-plane message (§4.10), one per exchange direction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContentMessage {
  /// The owner offers a snapshot's content: the object and head sequence it belongs to, the manifest
  /// identity, and every chunk identity the archive holds.
  Offer {
    /// The object (the volume) the content belongs to.
    object: ObjectId,
    /// The head sequence the content is placed for.
    sequence: u64,
    /// The manifest's identity.
    manifest: [u8; 32],
    /// Every distinct chunk identity the archive holds.
    chunks: Vec<[u8; 32]>,
  },
  /// The holder's answer to an offer: the chunks it lacks.
  Missing {
    /// The offered object.
    object: ObjectId,
    /// The offered sequence.
    sequence: u64,
    /// The offered manifest.
    manifest: [u8; 32],
    /// The identities the holder does not hold, so the owner ships exactly those.
    missing: Vec<[u8; 32]>,
  },
  /// The owner ships an archive: the manifest and the chunks the holder was missing.
  Put {
    /// The object.
    object: ObjectId,
    /// The sequence.
    sequence: u64,
    /// The encoded archive ([`Archive::encode`]).
    archive: Vec<u8>,
  },
  /// The holder's bound acknowledgement of a put it verified and holds whole.
  Ack(ContentAck),
  /// A reader asks for a manifest's whole archive by identity, **for an object**: a holder serves it only
  /// to a host with authority over that object, and only from what it holds for that object (AUD-29-45).
  Fetch {
    /// The object whose content is asked for.
    object: ObjectId,
    /// The manifest identity.
    manifest: [u8; 32],
  },
  /// A holder's answer to a fetch: the whole archive (manifest and every referenced chunk).
  Have {
    /// The encoded archive.
    archive: Vec<u8>,
  },
}

/// A holder's acknowledgement of content, bound to what was put so a reply for another object,
/// sequence or manifest — or a redelivered stale one — cannot count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentAck {
  /// The holder that verified and holds the content.
  pub holder: HostId,
  /// The object the content belongs to.
  pub object: ObjectId,
  /// The head sequence the content is placed for.
  pub sequence: u64,
  /// The manifest identity held.
  pub manifest: [u8; 32],
}

impl ContentAck {
  /// Whether this acknowledgement answers a put of `manifest` for `object` at `sequence`.
  pub fn binds(&self, object: ObjectId, sequence: u64, manifest: &[u8; 32]) -> bool {
    self.object == object && self.sequence == sequence && &self.manifest == manifest
  }
}

/// A refusal decoding a content message: every way the bytes can be malformed, typed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentError {
  /// The bytes end before a field they declare.
  Truncated,
  /// The kind byte names no content message.
  UnknownKind {
    /// The byte found.
    kind: u8,
  },
  /// A declared count or length is larger than the bytes can hold, or bytes trail the message.
  BadLength,
}

impl std::fmt::Display for ContentError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      ContentError::Truncated => f.write_str("content message truncated"),
      ContentError::UnknownKind { kind } => write!(f, "unknown content message kind {kind}"),
      ContentError::BadLength => f.write_str("content message length does not fit its bytes"),
    }
  }
}

impl std::error::Error for ContentError {}

/// A bounds-checked reader over a message's bytes.
struct Reader<'a> {
  bytes: &'a [u8],
  at: usize,
}

impl<'a> Reader<'a> {
  fn take(&mut self, len: usize) -> Result<&'a [u8], ContentError> {
    let end = self.at.checked_add(len).ok_or(ContentError::BadLength)?;
    let slice = self
      .bytes
      .get(self.at..end)
      .ok_or(ContentError::Truncated)?;
    self.at = end;
    Ok(slice)
  }

  fn u8(&mut self) -> Result<u8, ContentError> {
    self
      .take(1)?
      .first()
      .copied()
      .ok_or(ContentError::Truncated)
  }

  fn u32(&mut self) -> Result<u32, ContentError> {
    let bytes = self.take(size_of::<u32>())?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap_or([0; 4])))
  }

  fn u64(&mut self) -> Result<u64, ContentError> {
    let bytes = self.take(size_of::<u64>())?;
    Ok(u64::from_le_bytes(bytes.try_into().unwrap_or([0; 8])))
  }

  fn hash(&mut self) -> Result<[u8; 32], ContentError> {
    let bytes = self.take(HASH_BYTES)?;
    Ok(bytes.try_into().unwrap_or([0; HASH_BYTES]))
  }

  fn object(&mut self) -> Result<ObjectId, ContentError> {
    let bytes = self.take(OBJECT_BYTES)?;
    Ok(ObjectId(bytes.try_into().unwrap_or([0; OBJECT_BYTES])))
  }

  /// A count-prefixed identity list; the count is checked against the bytes before any allocation.
  fn hashes(&mut self) -> Result<Vec<[u8; 32]>, ContentError> {
    let count = usize::try_from(self.u32()?).map_err(|_| ContentError::BadLength)?;
    let needed = count
      .checked_mul(HASH_BYTES)
      .ok_or(ContentError::BadLength)?;
    if needed > self.bytes.len().saturating_sub(self.at) {
      return Err(ContentError::BadLength);
    }
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
      out.push(self.hash()?);
    }
    Ok(out)
  }

  /// A length-prefixed byte string; the length is checked against the bytes before any allocation.
  fn blob(&mut self) -> Result<Vec<u8>, ContentError> {
    let len = usize::try_from(self.u32()?).map_err(|_| ContentError::BadLength)?;
    if len > self.bytes.len().saturating_sub(self.at) {
      return Err(ContentError::BadLength);
    }
    Ok(self.take(len)?.to_vec())
  }

  fn finish(self) -> Result<(), ContentError> {
    if self.at == self.bytes.len() {
      Ok(())
    } else {
      Err(ContentError::BadLength)
    }
  }
}

fn put_hashes(out: &mut Vec<u8>, hashes: &[[u8; 32]]) {
  out.extend_from_slice(
    &u32::try_from(hashes.len())
      .unwrap_or(u32::MAX)
      .to_le_bytes(),
  );
  for hash in hashes {
    out.extend_from_slice(hash);
  }
}

fn put_blob(out: &mut Vec<u8>, blob: &[u8]) {
  out.extend_from_slice(&u32::try_from(blob.len()).unwrap_or(u32::MAX).to_le_bytes());
  out.extend_from_slice(blob);
}

impl ContentMessage {
  /// The canonical bytes: the kind byte, then the fields little-endian, identity lists and byte
  /// strings count- or length-prefixed by a `u32`.
  pub fn encode(&self) -> Vec<u8> {
    let mut out = Vec::new();
    match self {
      ContentMessage::Offer {
        object,
        sequence,
        manifest,
        chunks,
      } => {
        out.push(KIND_OFFER);
        out.extend_from_slice(&object.0);
        out.extend_from_slice(&sequence.to_le_bytes());
        out.extend_from_slice(manifest);
        put_hashes(&mut out, chunks);
      }
      ContentMessage::Missing {
        object,
        sequence,
        manifest,
        missing,
      } => {
        out.push(KIND_MISSING);
        out.extend_from_slice(&object.0);
        out.extend_from_slice(&sequence.to_le_bytes());
        out.extend_from_slice(manifest);
        put_hashes(&mut out, missing);
      }
      ContentMessage::Put {
        object,
        sequence,
        archive,
      } => {
        out.push(KIND_PUT);
        out.extend_from_slice(&object.0);
        out.extend_from_slice(&sequence.to_le_bytes());
        put_blob(&mut out, archive);
      }
      ContentMessage::Ack(ack) => {
        out.push(KIND_ACK);
        out.extend_from_slice(&ack.holder.0.to_le_bytes());
        out.extend_from_slice(&ack.object.0);
        out.extend_from_slice(&ack.sequence.to_le_bytes());
        out.extend_from_slice(&ack.manifest);
      }
      ContentMessage::Fetch { object, manifest } => {
        out.push(KIND_FETCH);
        out.extend_from_slice(&object.0);
        out.extend_from_slice(manifest);
      }
      ContentMessage::Have { archive } => {
        out.push(KIND_HAVE);
        put_blob(&mut out, archive);
      }
    }
    out
  }

  /// Parses a message, refusing truncation, an unknown kind, a count or length the bytes cannot hold,
  /// and trailing bytes.
  pub fn decode(bytes: &[u8]) -> Result<ContentMessage, ContentError> {
    let mut reader = Reader { bytes, at: 0 };
    let kind = reader.u8()?;
    let message = match kind {
      KIND_OFFER => ContentMessage::Offer {
        object: reader.object()?,
        sequence: reader.u64()?,
        manifest: reader.hash()?,
        chunks: reader.hashes()?,
      },
      KIND_MISSING => ContentMessage::Missing {
        object: reader.object()?,
        sequence: reader.u64()?,
        manifest: reader.hash()?,
        missing: reader.hashes()?,
      },
      KIND_PUT => ContentMessage::Put {
        object: reader.object()?,
        sequence: reader.u64()?,
        archive: reader.blob()?,
      },
      KIND_ACK => ContentMessage::Ack(ContentAck {
        holder: HostId(reader.u64()?),
        object: reader.object()?,
        sequence: reader.u64()?,
        manifest: reader.hash()?,
      }),
      KIND_FETCH => ContentMessage::Fetch {
        object: reader.object()?,
        manifest: reader.hash()?,
      },
      KIND_HAVE => ContentMessage::Have {
        archive: reader.blob()?,
      },
      kind => return Err(ContentError::UnknownKind { kind }),
    };
    reader.finish()?;
    Ok(message)
  }
}

/// Why a holder refused to hold an archive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentRefusal {
  /// The archive's bytes are malformed or corrupt (the reader's typed refusal).
  Malformed(ArchiveError),
  /// The manifest references chunks the archive did not ship and the holder does not hold — the
  /// content would not be whole, so it is not acknowledged (§4.10 "placement closure").
  Incomplete {
    /// How many referenced chunks are missing.
    missing: usize,
  },
  /// A chunk's payload does not hash to its declared identity.
  IdentityMismatch,
  /// The archive ships a chunk its manifest does not reference (AUD-29-43: shipped content is restricted to
  /// the manifest's closure; an owner ships exactly the referenced chunks a holder lacks). Refused before any
  /// chunk is decoded or hashed.
  Unreferenced,
  /// The holder cannot admit the put (§4.2; AUD-29-43): its charge — the put's arena blocks, its
  /// verification scratch and its index — is past the shard's unpromised capacity or metadata ledger, or the
  /// arena has no block that fits. Refused whole, with nothing stored and nothing charged.
  NoCapacity {
    /// The bytes the refused charge or block asked for.
    requested: u64,
    /// The bytes that were available.
    available: u64,
  },
}

/// The distinct chunk identities a manifest references, in first-reference order (a hole's zero
/// identity is not a chunk and is skipped).
pub fn referenced_chunks(node: &Node) -> Vec<[u8; 32]> {
  fn walk(node: &Node, seen: &mut BTreeSet<[u8; 32]>, out: &mut Vec<[u8; 32]>) {
    match node {
      Node::File(extents) => {
        for extent in extents {
          if extent.chunk != [0u8; HASH_BYTES] && seen.insert(extent.chunk) {
            out.push(extent.chunk);
          }
        }
      }
      Node::Directory(entries) => {
        for entry in entries {
          walk(&entry.node, seen, out);
        }
      }
    }
  }
  let mut seen = BTreeSet::new();
  let mut out = Vec::new();
  walk(node, &mut seen, &mut out);
  out
}

/// The archive's header fields and manifest with `chunks` in place of its own — the partial archive an
/// owner ships to a holder that lacks exactly those.
fn with_chunks(archive: &Archive, chunks: Vec<Chunk>) -> Archive {
  Archive {
    base_page_size: archive.base_page_size,
    chunk_min: archive.chunk_min,
    chunk_max: archive.chunk_max,
    created_unix: archive.created_unix,
    volume_id: archive.volume_id,
    snapshot_id: archive.snapshot_id,
    name_policy_id: archive.name_policy_id,
    unicode_version: archive.unicode_version,
    // The root's own metadata is part of the manifest identity the holder's acknowledgement binds
    // (format minor 2): a partial archive shipped without it would carry another identity than the
    // owner's head names, and the head would never place.
    root_meta: archive.root_meta,
    manifest: archive.manifest.clone(),
    chunks,
  }
}

/// A hold's canonical image (AUD-29-59), carried in its shard's recovery image so a content acknowledgement
/// survives a warm restart: every distinct chunk the held manifests reference, once, in identity order, and
/// every held manifest with its object and latest placement, in (object, identity) order — so two holds of
/// equal content image byte-identically (a determinism gate).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
struct HoldImage {
  /// The distinct chunks the held manifests reference.
  chunks: Vec<ChunkImage>,
  /// The held manifests.
  manifests: Vec<ManifestImage>,
}

/// One chunk of a [`HoldImage`]: a `Chunk`'s fields, the encoding as its wire byte.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
struct ChunkImage {
  identity: [u8; 32],
  raw_len: u64,
  stored_len: u64,
  encoding: u8,
  level: u8,
  dictionary: [u8; 32],
  payload: Vec<u8>,
}

/// One held manifest of a [`HoldImage`]: its object, the sequence it was last placed for, and the archive
/// it arrived as with no chunks (its header and tree).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
struct ManifestImage {
  object: [u8; 16],
  sequence: u64,
  archive: Vec<u8>,
}

/// Why a hold image could not be recovered (AUD-29-59): its bytes do not decode as one image, a chunk names
/// an unknown encoding, a manifest's archive does not decode, or a manifest refused to hold again (a chunk it
/// references is missing or fails its identity).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HoldImageError {
  /// The bytes are not exactly one hold image.
  Malformed,
  /// A chunk's encoding byte names no encoding.
  UnknownEncoding,
  /// A manifest's archive did not decode.
  Archive(ArchiveError),
  /// A manifest refused to hold again.
  Refused(ContentRefusal),
}

/// What a request asks of a holder, for the authority check that precedes any lookup (AUD-29-45).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentAccess {
  /// Placing content for an object: an offer (which asks what the holder lacks) or a put.
  Place,
  /// Reading an object's content back: a fetch.
  Read,
}

/// The shard memory a hold stores in and is charged against (§4.2 "a remote holder makes the same admission
/// against its own machine before acknowledging placement"; AUD-29-43): the shard's chunk arena, which the
/// held chunk payloads and manifest encodings occupy; its byte budget, which charges them (and a put's
/// transient verification scratch) at their arena block length, from unpromised capacity only
/// (`ShardBudget::charge_replicated`); and its metadata ledger, which charges the hold's own index. The
/// shard keeps all three in its store; a hold borrows them for one operation, so the shard stays the one
/// capacity owner and the hold never holds a reference across operations.
pub struct HoldSpace<'a> {
  /// The shard's chunk arena.
  pub arena: &'a mut ChunkArena,
  /// The shard's byte budget.
  pub budget: &'a mut ShardBudget,
  /// The shard's metadata ledger.
  pub metadata: &'a mut MetadataBudget,
}

/// The branching factor of the standard library's B-tree (`B` in `alloc::collections::btree::node`, Rust
/// std source; tier C): a node holds at most `2B − 1` entries and every node but the root at least `B − 1`.
/// The hold's index charge is derived from it ([`btree_entry_bytes`]).
/// Format: the standard library's constant, restated so the charge follows the structure it bounds.
const BTREE_B: usize = 6;

/// The heap bytes one node of a `BTreeMap<K, V>` can take, leaf and internal: a leaf holds `2B − 1` keys and
/// values, its parent link, its index in the parent and its length; an internal node adds `2B` child links.
/// Each is rounded up to a power of two, the largest size-class rounding a general-purpose allocator applies,
/// so the charge bounds what the allocator hands out, not only what the node asks for.
fn btree_node_bytes<K, V>() -> (u64, u64) {
  let capacity = BTREE_B.saturating_mul(2).saturating_sub(1);
  let link = size_of::<usize>();
  let leaf = link
    .saturating_add(size_of::<u16>().saturating_mul(2))
    .saturating_add(
      size_of::<K>()
        .saturating_add(size_of::<V>())
        .saturating_mul(capacity),
    );
  let internal = leaf.saturating_add(link.saturating_mul(capacity.saturating_add(1)));
  let rounded = |bytes: usize| {
    u64::try_from(bytes.checked_next_power_of_two().unwrap_or(bytes)).unwrap_or(u64::MAX)
  };
  (rounded(leaf), rounded(internal))
}

/// The heap bytes one entry of a `BTreeMap<K, V>` can cost at most, amortized (§4.2 "an uncharged heap
/// allocation cannot sit outside the bound"; AUD-29-43): every node but the root holds at least `B − 1`
/// entries and the tree has fewer internal nodes than leaves, so `n` entries take at most
/// `n / (B − 1)` leaves and as many internal nodes — `(leaf + internal) / (B − 1)` per entry, rounded up.
fn btree_entry_bytes<K, V>() -> u64 {
  let (leaf, internal) = btree_node_bytes::<K, V>();
  leaf
    .saturating_add(internal)
    .div_ceil(u64::try_from(BTREE_B.saturating_sub(1)).unwrap_or(1).max(1))
}

/// The heap bytes a non-empty `BTreeMap<K, V>`'s root can take beyond its entries' amortized share (a root
/// holds as few as one entry): one leaf and one internal node.
fn btree_map_bytes<K, V>() -> u64 {
  let (leaf, internal) = btree_node_bytes::<K, V>();
  leaf.saturating_add(internal)
}

/// What a node holds as a content candidate for other owners' snapshots (§4.10), **scoped by object**
/// (§4.13 "Content identity and sharing"; AUD-29-45). A chunk hash proves bytes, not permission to read them
/// or to ask whether they exist: every answer — the missing set an offer draws, which already-held chunks a
/// put may lean on, the archive a fetch returns — is computed from what this node holds **for the object the
/// request names**, and only after the caller's authority check for that object passed. The bytes themselves
/// are kept once whatever objects reference them (one stored chunk, one reference per object), so dedup
/// saves memory without answering across objects. A refused or unheld request draws the same empty reply, so
/// an answer never reveals whether another object's content exists.
///
/// **Residency and admission (AUD-29-43).** Every held byte lives in the shard's arena, charged at its block
/// length against the shard's unpromised capacity; the hold's own index is charged to the shard's metadata
/// ledger at a derived per-entry cost. A put is admitted whole or refused typed: its charges are taken before
/// any chunk is verified or stored, its new encoded chunks are verified in one charged scratch block, and a
/// refusal at any step gives back every charge and every block. A release frees exactly what its hold took.
#[derive(Debug, Default)]
pub struct ContentHold {
  chunks: BTreeMap<[u8; 32], StoredChunk>,
  objects: BTreeMap<ObjectId, Held>,
  /// Requests refused by the authority check (a non-vacuity counter for the scope).
  unauthorized: u64,
  /// Puts refused because the holder's accepted records already supersede them (AUD-29-43; a non-vacuity
  /// counter for the retention rule at the door).
  superseded: u64,
  /// The bytes the hold is charged on the shard's byte budget (its blocks).
  charged_bytes: u64,
  /// The bytes the hold is charged on the shard's metadata ledger (its index).
  index_bytes: u64,
  /// Puts refused because the shard could not admit them (`NoCapacity`; AUD-29-43's typed refusal at the
  /// bound, counted).
  refused_capacity: u64,
}

/// A chunk stored in the arena once, whatever objects reference it: its block, its charge (the block's
/// length), the fields a `Chunk` carries besides its payload, and how many objects reference it.
#[derive(Debug)]
struct StoredChunk {
  extent: Extent,
  raw_len: u64,
  stored_len: u64,
  encoding: Encoding,
  level: u8,
  dictionary: [u8; 32],
  objects: u64,
}

/// One object's held content: its manifests and how many of them reference each chunk.
#[derive(Debug, Default)]
struct Held {
  manifests: BTreeMap<[u8; 32], HeldManifest>,
  chunks: BTreeMap<[u8; 32], u64>,
}

/// A held manifest: the arena block holding the archive it arrived as with no chunks (its header and tree,
/// canonically encoded), the encoding's length, and how it was last placed.
#[derive(Debug)]
struct HeldManifest {
  extent: Extent,
  len: usize,
  placed: Placed,
}

/// How content was last placed on this holder (AUD-29-43): the register sequence the owner placed it for.
/// The holder's retention rule reads it — content placed for a sequence no accepted record has reached yet is
/// in flight toward its record, and the newest such placement of an object is kept until an event supersedes
/// it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Placed {
  /// The register sequence the content was placed for.
  pub sequence: u64,
}

/// What a put takes, charged before anything is verified or stored, so a refusal gives it all back.
#[derive(Debug, Default, Clone, Copy)]
struct Charge {
  bytes: u64,
  scratch: u64,
  index: u64,
}

/// The index cost of a new stored chunk.
fn chunk_entry_bytes() -> u64 {
  btree_entry_bytes::<[u8; 32], StoredChunk>()
}

/// The index cost of a new object: its entry and the roots of its two maps.
fn object_entry_bytes() -> u64 {
  btree_entry_bytes::<ObjectId, Held>()
    .saturating_add(btree_map_bytes::<[u8; 32], HeldManifest>())
    .saturating_add(btree_map_bytes::<[u8; 32], u64>())
}

/// The index cost of a new manifest for an object.
fn manifest_entry_bytes() -> u64 {
  btree_entry_bytes::<[u8; 32], HeldManifest>()
}

/// The index cost of an object's reference to a chunk.
fn reference_entry_bytes() -> u64 {
  btree_entry_bytes::<[u8; 32], u64>()
}

/// The typed refusal a budget or arena refusal becomes.
fn no_capacity(error: &MemError) -> ContentRefusal {
  match error {
    MemError::BudgetExceeded {
      requested,
      available,
    } => ContentRefusal::NoCapacity {
      requested: *requested,
      available: *available,
    },
    MemError::ArenaExhausted {
      requested,
      largest_free,
    } => ContentRefusal::NoCapacity {
      requested: u64::try_from(*requested).unwrap_or(u64::MAX),
      available: u64::try_from(*largest_free).unwrap_or(u64::MAX),
    },
    MemError::TooLarge { len, max } => ContentRefusal::NoCapacity {
      requested: u64::try_from(*len).unwrap_or(u64::MAX),
      available: u64::try_from(*max).unwrap_or(u64::MAX),
    },
    _ => ContentRefusal::NoCapacity {
      requested: 0,
      available: 0,
    },
  }
}

impl ContentHold {
  /// An empty hold.
  pub fn new() -> ContentHold {
    ContentHold::default()
  }

  /// Whether the manifest with `identity` is held whole for `object`.
  pub fn holds_manifest(&self, object: ObjectId, identity: &[u8; 32]) -> bool {
    self
      .objects
      .get(&object)
      .is_some_and(|held| held.manifests.contains_key(identity))
  }

  /// Whether any object's content includes the manifest with `identity` — an operator's and a test's view of
  /// this node, never an answer on the wire.
  pub fn holds_manifest_for_any_object(&self, identity: &[u8; 32]) -> bool {
    self
      .objects
      .values()
      .any(|held| held.manifests.contains_key(identity))
  }

  /// The distinct chunks held — the non-vacuity counter a test reads (a put of one missing chunk
  /// raises it by exactly one).
  pub fn chunk_count(&self) -> usize {
    self.chunks.len()
  }

  /// The manifests held, across objects.
  pub fn manifest_count(&self) -> usize {
    self.objects.values().map(|held| held.manifests.len()).sum()
  }

  /// The bytes this hold is charged on the shard's byte budget — its arena blocks (AUD-29-43).
  pub fn charged_bytes(&self) -> u64 {
    self.charged_bytes
  }

  /// The bytes this hold is charged on the shard's metadata ledger — its index (AUD-29-43).
  pub fn index_bytes(&self) -> u64 {
    self.index_bytes
  }

  /// Requests the authority check refused.
  pub fn unauthorized(&self) -> u64 {
    self.unauthorized
  }

  /// Puts refused because the holder's accepted records already supersede them (AUD-29-43).
  pub fn superseded(&self) -> u64 {
    self.superseded
  }

  /// Puts refused because the shard could not admit them (`NoCapacity`; AUD-29-43).
  pub fn refused_capacity(&self) -> u64 {
    self.refused_capacity
  }

  /// Of `chunks`, the identities this hold lacks **for `object`** — the missing set an offer is answered
  /// with. A chunk held only for another object counts as lacking: its presence is not this object's to know.
  pub fn missing_of(&self, object: ObjectId, chunks: &[[u8; 32]]) -> Vec<[u8; 32]> {
    let held = self.objects.get(&object);
    chunks
      .iter()
      .copied()
      .filter(|identity| !held.is_some_and(|held| held.chunks.contains_key(identity)))
      .collect()
  }

  /// Holds `archive` for `object`, placed as `placed`, in `space` — only chunks its manifest references may
  /// be shipped, every chunk the manifest references must be shipped or already held **for this object**,
  /// every shipped chunk not yet stored is verified, and the whole put is admitted from the shard's
  /// unpromised capacity or refused (`NoCapacity`) — and returns the manifest identity now held. Nothing is
  /// stored and nothing stays charged on a refusal; holding a manifest already held stores nothing and
  /// refreshes its placement.
  pub fn hold(
    &mut self,
    space: &mut HoldSpace<'_>,
    object: ObjectId,
    placed: Placed,
    archive: Archive,
  ) -> Result<[u8; 32], ContentRefusal> {
    let referenced: BTreeSet<[u8; 32]> = referenced_chunks(&archive.manifest).into_iter().collect();
    if archive
      .chunks
      .iter()
      .any(|chunk| !referenced.contains(&chunk.identity))
    {
      return Err(ContentRefusal::Unreferenced);
    }
    let held = self.objects.get(&object);
    let held_for_object =
      |identity: &[u8; 32]| held.is_some_and(|held| held.chunks.contains_key(identity));
    let shipped = |identity: &[u8; 32]| {
      archive
        .chunks
        .iter()
        .find(|chunk| chunk.identity == *identity)
    };
    let missing = referenced
      .iter()
      .filter(|identity| !held_for_object(identity) && shipped(identity).is_none())
      .count();
    if missing > 0 {
      return Err(ContentRefusal::Incomplete { missing });
    }
    // What this put adds: the object's new references, and of those the chunks not stored for any object.
    let new_references: Vec<[u8; 32]> = referenced
      .iter()
      .filter(|identity| !held_for_object(identity))
      .copied()
      .collect();
    let identity = archive.manifest_identity();
    if let Some(record) = self
      .objects
      .get_mut(&object)
      .and_then(|held| held.manifests.get_mut(&identity))
    {
      record.placed = placed;
      return Ok(identity);
    }
    let new_chunks: Vec<&Chunk> = new_references
      .iter()
      .filter(|identity| !self.chunks.contains_key(*identity))
      .filter_map(shipped)
      .collect();
    let manifest_bytes = with_chunks(&archive, Vec::new()).encode();
    let charge = self.charge_for(
      space,
      &new_references,
      &new_chunks,
      manifest_bytes.len(),
      object,
    )?;
    if let Err(refusal) = Self::verify(space, &new_chunks, charge.scratch) {
      self.refund(space, charge);
      return Err(refusal);
    }
    // Verification is done: its scratch charge goes back before anything is stored.
    space.budget.credit_replicated(charge.scratch);
    let stored = match Self::store(space, &new_chunks, &manifest_bytes) {
      Ok(stored) => stored,
      Err(refusal) => {
        self.refund(
          space,
          Charge {
            scratch: 0,
            ..charge
          },
        );
        return Err(refusal);
      }
    };
    self.install(
      object,
      identity,
      placed,
      &referenced,
      stored,
      manifest_bytes.len(),
    );
    Ok(identity)
  }

  /// Takes every charge a put needs — the index entries it adds, its new blocks, and one scratch block for
  /// verifying its largest new encoded chunk — whole or refused with nothing taken.
  fn charge_for(
    &mut self,
    space: &mut HoldSpace<'_>,
    new_references: &[[u8; 32]],
    new_chunks: &[&Chunk],
    manifest_len: usize,
    object: ObjectId,
  ) -> Result<Charge, ContentRefusal> {
    let block = |len: u64| {
      usize::try_from(len)
        .ok()
        .and_then(|len| space.arena.block_len(len))
        .map(|bytes| u64::try_from(bytes).unwrap_or(u64::MAX))
        .ok_or(ContentRefusal::NoCapacity {
          requested: len,
          available: 0,
        })
    };
    let mut bytes = block(u64::try_from(manifest_len).unwrap_or(u64::MAX))?;
    for chunk in new_chunks {
      bytes = bytes.saturating_add(block(chunk.stored_len)?);
    }
    let largest_encoded = new_chunks
      .iter()
      .filter(|chunk| chunk.encoding != Encoding::Raw)
      .map(|chunk| chunk.raw_len)
      .max();
    let scratch = match largest_encoded {
      Some(raw_len) => block(raw_len)?,
      None => 0,
    };
    let new_object = !self.objects.contains_key(&object);
    let count = |items: usize| u64::try_from(items).unwrap_or(u64::MAX);
    let index = manifest_entry_bytes()
      .saturating_add(reference_entry_bytes().saturating_mul(count(new_references.len())))
      .saturating_add(chunk_entry_bytes().saturating_mul(count(new_chunks.len())))
      .saturating_add(if new_object { object_entry_bytes() } else { 0 });
    let credit = space.metadata.reserve(index).map_err(|e| no_capacity(&e))?;
    if let Err(e) = space
      .budget
      .charge_replicated(bytes.saturating_add(scratch))
    {
      space.metadata.release(credit);
      return Err(no_capacity(&e));
    }
    self.charged_bytes = self.charged_bytes.saturating_add(bytes);
    self.index_bytes = self.index_bytes.saturating_add(index);
    Ok(Charge {
      bytes,
      scratch,
      index,
    })
  }

  /// Gives back every charge of a refused put.
  fn refund(&mut self, space: &mut HoldSpace<'_>, charge: Charge) {
    space
      .budget
      .credit_replicated(charge.bytes.saturating_add(charge.scratch));
    space.metadata.release(slates_mem::budget::MetadataCredit {
      bytes: charge.index,
    });
    self.charged_bytes = self.charged_bytes.saturating_sub(charge.bytes);
    self.index_bytes = self.index_bytes.saturating_sub(charge.index);
  }

  /// Verifies every new chunk against its identity: a raw one where it lies, an encoded one decoded into one
  /// scratch block of `scratch` bytes allocated from the arena for the duration (already charged).
  fn verify(
    space: &mut HoldSpace<'_>,
    new_chunks: &[&Chunk],
    scratch: u64,
  ) -> Result<(), ContentRefusal> {
    let extent = if scratch > 0 {
      let len = usize::try_from(scratch).map_err(|_| ContentRefusal::NoCapacity {
        requested: scratch,
        available: 0,
      })?;
      Some(space.arena.alloc(len).map_err(|e| no_capacity(&e))?)
    } else {
      None
    };
    let mut verdict = Ok(());
    for chunk in new_chunks {
      let buffer: &mut [u8] = match extent.and_then(|extent| space.arena.bytes_mut(extent)) {
        Some(buffer) => buffer,
        None => &mut [],
      };
      if Archive::verify_into(chunk, buffer).is_err() {
        verdict = Err(ContentRefusal::IdentityMismatch);
        break;
      }
    }
    if let Some(extent) = extent {
      let _ = space.arena.free(extent);
    }
    verdict
  }

  /// Allocates and fills a block for each new chunk's payload and one for the manifest's encoding; on an
  /// allocation refusal frees what it took and refuses `NoCapacity`.
  fn store(
    space: &mut HoldSpace<'_>,
    new_chunks: &[&Chunk],
    manifest_bytes: &[u8],
  ) -> Result<(Vec<(StoredChunkKey, StoredChunk)>, Extent), ContentRefusal> {
    let mut taken: Vec<Extent> = Vec::new();
    let mut put = |bytes: &[u8], taken: &mut Vec<Extent>| -> Result<Extent, ContentRefusal> {
      let extent = space
        .arena
        .alloc(bytes.len())
        .map_err(|e| no_capacity(&e))?;
      taken.push(extent);
      let block = space
        .arena
        .bytes_mut(extent)
        .and_then(|block| block.get_mut(..bytes.len()))
        .ok_or(ContentRefusal::NoCapacity {
          requested: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
          available: 0,
        })?;
      block.copy_from_slice(bytes);
      Ok(extent)
    };
    let mut stored = Vec::with_capacity(new_chunks.len());
    let mut outcome = Ok(());
    for chunk in new_chunks {
      match put(&chunk.payload, &mut taken) {
        Ok(extent) => stored.push((
          chunk.identity,
          StoredChunk {
            extent,
            raw_len: chunk.raw_len,
            stored_len: chunk.stored_len,
            encoding: chunk.encoding,
            level: chunk.level,
            dictionary: chunk.dictionary,
            objects: 0,
          },
        )),
        Err(refusal) => {
          outcome = Err(refusal);
          break;
        }
      }
    }
    let manifest = outcome.and_then(|()| put(manifest_bytes, &mut taken));
    match manifest {
      Ok(extent) => Ok((stored, extent)),
      Err(refusal) => {
        for extent in taken {
          let _ = space.arena.free(extent);
        }
        Err(refusal)
      }
    }
  }

  /// Records an admitted put: its stored chunks, one more reference from the object to every chunk its
  /// manifest references (a chunk the object references for the first time gains the object as one of its
  /// referrers), and the manifest.
  fn install(
    &mut self,
    object: ObjectId,
    identity: [u8; 32],
    placed: Placed,
    referenced: &BTreeSet<[u8; 32]>,
    (stored, manifest): (Vec<(StoredChunkKey, StoredChunk)>, Extent),
    manifest_len: usize,
  ) {
    for (key, chunk) in stored {
      self.chunks.insert(key, chunk);
    }
    let held = self.objects.entry(object).or_default();
    for chunk_identity in referenced {
      let references = held.chunks.entry(*chunk_identity).or_insert(0);
      if *references == 0
        && let Some(chunk) = self.chunks.get_mut(chunk_identity)
      {
        chunk.objects = chunk.objects.saturating_add(1);
      }
      *references = references.saturating_add(1);
    }
    held.manifests.insert(
      identity,
      HeldManifest {
        extent: manifest,
        len: manifest_len,
        placed,
      },
    );
  }

  /// The archive (header and tree, no chunks) a held manifest's block holds, or `None` if it does not decode
  /// (the hold wrote it canonically; a failure is memory corruption, answered as not held).
  fn manifest_of(arena: &ChunkArena, record: &HeldManifest) -> Option<Archive> {
    let bytes = arena.bytes(record.extent)?.get(..record.len)?;
    Archive::decode(bytes).ok()
  }

  /// Keeps, of what is held for `object`, only the manifests `keep` accepts given their identity and latest
  /// placement, releasing the rest exactly as [`forget_manifest`](Self::forget_manifest) does (AUD-29-43:
  /// the holder's retention rule is the caller's, from its accepted records). Returns how many were released.
  pub fn retain(
    &mut self,
    space: &mut HoldSpace<'_>,
    object: ObjectId,
    mut keep: impl FnMut(&[u8; 32], Placed) -> bool,
  ) -> usize {
    let released: Vec<[u8; 32]> = self
      .objects
      .get(&object)
      .map(|held| {
        held
          .manifests
          .iter()
          .filter(|(identity, record)| !keep(identity, record.placed))
          .map(|(identity, _)| *identity)
          .collect()
      })
      .unwrap_or_default();
    released
      .iter()
      .filter(|identity| self.forget_manifest(space, object, identity))
      .count()
  }

  /// The newest sequence any manifest held for `object` was placed for, or `None` when nothing is held for it.
  pub fn newest_placed(&self, object: ObjectId) -> Option<u64> {
    self
      .objects
      .get(&object)?
      .manifests
      .values()
      .map(|record| record.placed.sequence)
      .max()
  }

  /// Forgets a manifest held for `object` in `space`: its block is freed and its charge given back, and each
  /// chunk it references loses one of the object's references (a chunk the object no longer references loses
  /// the object as a referrer, and its block and charge go when no object references it); the index entries
  /// that go give back their charge. Returns whether the manifest was held. The release the retention rule, a
  /// tombstone and a stale-copy reclaim go through (AUD-29-43), and what a holder's **content loss** looks
  /// like from the owner's side — in a real deployment a whole-anchor loss (a warm restart keeps what was
  /// acknowledged, A-51); in-process, a test's injection — the condition the healer repairs (§4.10
  /// "anti-entropy … repairs only differing subtrees"). Nothing here is reachable from the wire.
  pub fn forget_manifest(
    &mut self,
    space: &mut HoldSpace<'_>,
    object: ObjectId,
    identity: &[u8; 32],
  ) -> bool {
    let Some(held) = self.objects.get_mut(&object) else {
      return false;
    };
    let Some(record) = held.manifests.remove(identity) else {
      return false;
    };
    let referenced: BTreeSet<[u8; 32]> = Self::manifest_of(space.arena, &record)
      .map(|archive| referenced_chunks(&archive.manifest).into_iter().collect())
      .unwrap_or_default();
    let mut bytes = u64::try_from(record.extent.len()).unwrap_or(u64::MAX);
    let mut index = manifest_entry_bytes();
    let _ = space.arena.free(record.extent);
    for chunk_identity in referenced {
      let Some(references) = held.chunks.get_mut(&chunk_identity) else {
        continue;
      };
      *references = references.saturating_sub(1);
      if *references > 0 {
        continue;
      }
      held.chunks.remove(&chunk_identity);
      index = index.saturating_add(reference_entry_bytes());
      let gone = self.chunks.get_mut(&chunk_identity).is_some_and(|chunk| {
        chunk.objects = chunk.objects.saturating_sub(1);
        chunk.objects == 0
      });
      if gone && let Some(chunk) = self.chunks.remove(&chunk_identity) {
        bytes = bytes.saturating_add(u64::try_from(chunk.extent.len()).unwrap_or(u64::MAX));
        index = index.saturating_add(chunk_entry_bytes());
        let _ = space.arena.free(chunk.extent);
      }
    }
    if held.manifests.is_empty() {
      self.objects.remove(&object);
      index = index.saturating_add(object_entry_bytes());
    }
    space.budget.credit_replicated(bytes);
    space
      .metadata
      .release(slates_mem::budget::MetadataCredit { bytes: index });
    self.charged_bytes = self.charged_bytes.saturating_sub(bytes);
    self.index_bytes = self.index_bytes.saturating_sub(index);
    true
  }

  /// Forgets everything held for `object` (AUD-29-43): every manifest, and each chunk's reference for the
  /// object (the bytes go when no object references them). The authoritative releases call it — a destroyed
  /// object's tombstone, a copy reclaimed as stale. Returns how many manifests were held.
  pub fn forget_object(&mut self, space: &mut HoldSpace<'_>, object: ObjectId) -> usize {
    let manifests: Vec<[u8; 32]> = self
      .objects
      .get(&object)
      .map(|held| held.manifests.keys().copied().collect())
      .unwrap_or_default();
    manifests
      .iter()
      .filter(|identity| self.forget_manifest(space, object, identity))
      .count()
  }

  /// Forgets the manifest with `identity` for every object holding it (a test's injection of content loss,
  /// [`forget_manifest`](Self::forget_manifest)); whether any held it.
  pub fn forget_manifest_for_every_object(
    &mut self,
    space: &mut HoldSpace<'_>,
    identity: &[u8; 32],
  ) -> bool {
    let holding: Vec<ObjectId> = self
      .objects
      .iter()
      .filter(|(_, held)| held.manifests.contains_key(identity))
      .map(|(object, _)| *object)
      .collect();
    let mut forgot = false;
    for object in holding {
      forgot |= self.forget_manifest(space, object, identity);
    }
    forgot
  }

  /// Forgets everything this hold holds, freeing every block and giving back every charge — what a hold
  /// rebuilt from a damaged image does before refusing, so a refused recovery leaves nothing charged.
  pub fn forget_all(&mut self, space: &mut HoldSpace<'_>) {
    let objects: Vec<ObjectId> = self.objects.keys().copied().collect();
    for object in objects {
      self.forget_object(space, object);
    }
  }

  /// A stored chunk as a `Chunk`, its payload copied out of the arena (for a reply or an image).
  fn chunk_of(arena: &ChunkArena, identity: &[u8; 32], stored: &StoredChunk) -> Option<Chunk> {
    let len = usize::try_from(stored.stored_len).ok()?;
    let payload = arena.bytes(stored.extent)?.get(..len)?.to_vec();
    Some(Chunk {
      identity: *identity,
      raw_len: stored.raw_len,
      stored_len: stored.stored_len,
      encoding: stored.encoding,
      level: stored.level,
      dictionary: stored.dictionary,
      payload,
    })
  }

  /// The whole archive for a manifest held for `object` — its header, manifest, and every referenced chunk
  /// in reference order, copied out of `arena` — or `None` if it is not held whole for that object.
  pub fn archive_of(
    &self,
    arena: &ChunkArena,
    object: ObjectId,
    identity: &[u8; 32],
  ) -> Option<Archive> {
    let record = self.objects.get(&object)?.manifests.get(identity)?;
    let archive = Self::manifest_of(arena, record)?;
    let mut chunks = Vec::new();
    for referenced in referenced_chunks(&archive.manifest) {
      chunks.push(Self::chunk_of(
        arena,
        &referenced,
        self.chunks.get(&referenced)?,
      )?);
    }
    Some(with_chunks(&archive, chunks))
  }

  /// This hold's canonical image (AUD-29-59; see [`HoldImage`]), read out of `arena`, or no bytes when
  /// nothing is held.
  pub fn to_image(&self, arena: &ChunkArena) -> Vec<u8> {
    if self.objects.is_empty() {
      return Vec::new();
    }
    let mut manifests = Vec::new();
    for (object, held) in &self.objects {
      for record in held.manifests.values() {
        let Some(bytes) = arena
          .bytes(record.extent)
          .and_then(|bytes| bytes.get(..record.len))
        else {
          continue;
        };
        manifests.push(ManifestImage {
          object: object.0,
          sequence: record.placed.sequence,
          archive: bytes.to_vec(),
        });
      }
    }
    let chunks = self
      .chunks
      .iter()
      .filter_map(|(identity, stored)| Self::chunk_of(arena, identity, stored))
      .map(|chunk| ChunkImage {
        identity: chunk.identity,
        raw_len: chunk.raw_len,
        stored_len: chunk.stored_len,
        encoding: chunk.encoding.to_wire(),
        level: chunk.level,
        dictionary: chunk.dictionary,
        payload: chunk.payload,
      })
      .collect();
    HoldImage { chunks, manifests }.to_bytes()
  }

  /// A hold rebuilt in `space` from its canonical image (AUD-29-59): every manifest held again through
  /// [`hold`](Self::hold), so each is re-verified against its identities, re-owned per object and re-charged
  /// exactly as a put would leave it — an image is recovered, never trusted. Empty bytes are an empty hold. A
  /// refusal part-way gives back everything the partial rebuild took.
  pub fn from_image(
    space: &mut HoldSpace<'_>,
    bytes: &[u8],
  ) -> Result<ContentHold, HoldImageError> {
    let mut hold = ContentHold::new();
    if bytes.is_empty() {
      return Ok(hold);
    }
    let image = HoldImage::from_bytes(bytes).map_err(|_| HoldImageError::Malformed)?;
    let mut chunks: BTreeMap<[u8; 32], Chunk> = BTreeMap::new();
    for chunk in image.chunks {
      let encoding = Encoding::from_wire(chunk.encoding).ok_or(HoldImageError::UnknownEncoding)?;
      chunks.insert(
        chunk.identity,
        Chunk {
          identity: chunk.identity,
          raw_len: chunk.raw_len,
          stored_len: chunk.stored_len,
          encoding,
          level: chunk.level,
          dictionary: chunk.dictionary,
          payload: chunk.payload,
        },
      );
    }
    for manifest in image.manifests {
      let rebuilt = Archive::decode(&manifest.archive)
        .map_err(HoldImageError::Archive)
        .and_then(|mut archive| {
          archive.chunks = referenced_chunks(&archive.manifest)
            .iter()
            .filter_map(|identity| chunks.get(identity).cloned())
            .collect();
          let placed = Placed {
            sequence: manifest.sequence,
          };
          hold
            .hold(space, ObjectId(manifest.object), placed, archive)
            .map_err(HoldImageError::Refused)
        });
      if let Err(refused) = rebuilt {
        hold.forget_all(space);
        return Err(refused);
      }
    }
    Ok(hold)
  }

  /// Serves one content request as `holder` in `space`, once `authorized` has allowed the access it asks for
  /// the object it names (checked before any lookup or allocation): an offer is answered with the object's
  /// missing set, a put — once `admits` accepts its object, sequence and manifest (the holder's retention
  /// rule, so a put its accepted records already supersede is never acknowledged, AUD-29-43) — with a bound
  /// acknowledgement once verified, admitted and held whole, a fetch with the object's archive. Anything
  /// refused, malformed, unverifiable, past the holder's capacity, incomplete or unheld is answered with the
  /// same empty reply. Returns the reply and, for a put that was held, its object, so the caller applies its
  /// retention rule to what the put superseded.
  pub fn serve(
    &mut self,
    space: &mut HoldSpace<'_>,
    holder: HostId,
    request: &[u8],
    authorized: impl FnOnce(ContentAccess, ObjectId) -> bool,
    admits: impl FnOnce(ObjectId, u64, &[u8; 32]) -> bool,
  ) -> (Vec<u8>, Option<ObjectId>) {
    let Ok(message) = ContentMessage::decode(request) else {
      return (Vec::new(), None);
    };
    let asked = match &message {
      ContentMessage::Offer { object, .. } | ContentMessage::Put { object, .. } => {
        Some((ContentAccess::Place, *object))
      }
      ContentMessage::Fetch { object, .. } => Some((ContentAccess::Read, *object)),
      _ => None,
    };
    let Some((access, object)) = asked else {
      return (Vec::new(), None);
    };
    if !authorized(access, object) {
      self.unauthorized = self.unauthorized.saturating_add(1);
      return (Vec::new(), None);
    }
    let reply = match message {
      ContentMessage::Offer {
        object,
        sequence,
        manifest,
        chunks,
      } => ContentMessage::Missing {
        object,
        sequence,
        manifest,
        missing: self.missing_of(object, &chunks),
      }
      .encode(),
      ContentMessage::Put {
        object,
        sequence,
        archive,
      } => match Archive::decode(&archive).map_err(ContentRefusal::Malformed) {
        Ok(archive) if !admits(object, sequence, &archive.manifest_identity()) => {
          self.superseded = self.superseded.saturating_add(1);
          Vec::new()
        }
        Ok(archive) => match self.hold(space, object, Placed { sequence }, archive) {
          Ok(manifest) => {
            return (
              ContentMessage::Ack(ContentAck {
                holder,
                object,
                sequence,
                manifest,
              })
              .encode(),
              Some(object),
            );
          }
          Err(ContentRefusal::NoCapacity { .. }) => {
            self.refused_capacity = self.refused_capacity.saturating_add(1);
            Vec::new()
          }
          Err(_) => Vec::new(),
        },
        Err(_) => Vec::new(),
      },
      ContentMessage::Fetch { object, manifest } => {
        match self.archive_of(space.arena, object, &manifest) {
          Some(archive) => ContentMessage::Have {
            archive: archive.encode(),
          }
          .encode(),
          None => Vec::new(),
        }
      }
      _ => Vec::new(),
    };
    (reply, None)
  }
}

/// A stored chunk's key: its identity.
type StoredChunkKey = [u8; 32];

/// A content put's outcome and the holder connections handed back — the content counterpart of
/// [`crate::Committed`].
pub struct ContentPlaced {
  /// Whether the content placed (the acknowledging [`Placement`]) or why not.
  pub outcome: Result<Placement, ClusterError>,
  /// The holder connections still open, for reuse.
  pub reusable: Vec<(HostId, Endpoint)>,
  /// The holders still in flight at the return, whose sessions the caller recovers later.
  pub stragglers: Stragglers,
  /// The **put latency** of each holder that acknowledged this round, in nanoseconds from the put round's
  /// dispatch to its verified acknowledgement — the readings of the content class the owner's hedge
  /// trigger is measured from (§4.8 "hedge delay = measured p95 put latency per class"). Empty when the
  /// round dispatched nothing (the owner's local hold at `f = 0`) or no holder acknowledged.
  pub latencies_ns: Vec<(HostId, u64)>,
  /// The holders whose offer answer named chunks they **lacked** — bytes actually crossed the wire to
  /// them this round. For a fresh seal that is every holder; for the healer's re-offer of placed content
  /// (§4.10 "anti-entropy … repairs only differing subtrees") it is exactly the holders that had lost
  /// something, the repairs. A holder that lacked nothing answers with an empty missing set and is put
  /// zero chunks (it still verifies and acknowledges).
  pub refilled: Vec<HostId>,
}

/// One request of a dispatch round: the holder, its session, and the request bytes.
type HolderRequest = (HostId, Endpoint, Vec<u8>);

/// Dispatches one request per holder concurrently on `stream`, each in its own child task bounded by
/// `deadline_ns` and reporting its reply and session to the returned channel. On a spawn refusal the
/// tasks already started keep running (children of the caller, bounded) and the channel is returned with
/// the error so their sessions can still be gathered.
fn dispatch_round(
  requests: Vec<HolderRequest>,
  stream: u64,
  deadline_ns: u64,
) -> Result<Receiver<Reply>, (RtError, Receiver<Reply>)> {
  let (tx, rx) = channel::<Reply>();
  for (host, endpoint, bytes) in requests {
    let reply_tx = tx.clone();
    let spawned = spawn_child(async move {
      let (reply, endpoint) =
        request_within(endpoint, stream, Priority::Bulk, &bytes, deadline_ns).await;
      let _ = reply_tx.send(Reply(host, reply, Box::new(endpoint)));
    });
    match spawned {
      // Detach at once so the task's slot is reaped when it terminates, not held (joinable) until this
      // perpetual caller finishes — an un-detached content dispatch would leak a slot per holder per put
      // (banned item 8). Detaching does not cancel: the task still runs and hands its session back over the
      // channel the caller drains.
      Ok(task) => {
        let _ = detach(task);
      }
      Err(error) => {
        drop(tx);
        return Err((error, rx));
      }
    }
  }
  drop(tx); // so the channel disconnects once every task has ended
  Ok(rx)
}

/// The sessions of gathered replies, their bytes dropped.
fn sessions_of(replies: Vec<Reply>) -> Vec<(HostId, Endpoint)> {
  replies
    .into_iter()
    .map(|Reply(host, _, endpoint)| (host, *endpoint))
    .collect()
}

/// The puts an offer round's answers call for: each holder whose `Missing` binds to this put gets the
/// manifest with exactly the chunks it named; a holder that answered nothing usable keeps its session for
/// the next round. Returns the puts and the sessions handed straight back.
fn puts_for(
  archive: &Archive,
  object: ObjectId,
  sequence: u64,
  manifest: &[u8; 32],
  offered: Vec<Reply>,
) -> (Vec<HolderRequest>, Vec<(HostId, Endpoint)>, Vec<HostId>) {
  let mut puts = Vec::new();
  let mut reusable = Vec::new();
  let mut refilled = Vec::new();
  for Reply(host, reply, endpoint) in offered {
    match ContentMessage::decode(&reply.bytes) {
      Ok(ContentMessage::Missing {
        object: offered_object,
        sequence: offered_sequence,
        manifest: offered_manifest,
        missing,
      }) if offered_object == object
        && offered_sequence == sequence
        && offered_manifest == *manifest =>
      {
        if !missing.is_empty() {
          refilled.push(host);
        }
        let wanted: BTreeSet<[u8; 32]> = missing.into_iter().collect();
        let partial = with_chunks(archive, chunks_for(archive, &wanted));
        let put = ContentMessage::Put {
          object,
          sequence,
          archive: partial.encode(),
        }
        .encode();
        puts.push((host, *endpoint, put));
      }
      _ => reusable.push((host, *endpoint)),
    }
  }
  (puts, reusable, refilled)
}

/// Gathers every reply of a dispatch round until all its tasks have ended (the channel closes) or the
/// budget's stall policy gives up on the rest — the offer round, which needs each holder's answer,
/// not a quorum. Bounded: each task is bounded by the dispatch span.
async fn gather(rx: &mut Receiver<Reply>, budget: CommitBudget) -> Vec<Reply> {
  let mut replies = Vec::new();
  let mut wait = DispatchWait::new(budget, now_ns());
  loop {
    match rx.try_recv() {
      Ok(reply) => replies.push(reply),
      Err(TryRecvError::Empty) => {
        if !wait.keep_waiting(replies.len()).await {
          return replies;
        }
      }
      Err(TryRecvError::Disconnected) => return replies,
    }
  }
}

/// Puts `archive` — the content of `object`'s head at `sequence` — to the `remote_holders` (its
/// candidate holders, each over a connected session) at `quorum`: the offer round to every holder,
/// a partial archive of exactly what each lacks, the put round, and the bound acknowledgements
/// collected to `f + 1` distinct candidates under `budget`. The `owner` holds its own content, so it
/// counts from the start when it is a candidate; at `f = 0` that is the placement and nothing is
/// dispatched (R8). Returns the [`Placement`] on a quorum, [`ClusterError::Uncertain`] at the
/// deadline, [`ClusterError::NotPlaced`] when every holder answered short of it — with the sessions
/// that came back and the stragglers still in flight.
#[allow(clippy::too_many_arguments)]
pub async fn put_content(
  owner: HostId,
  archive: &Archive,
  object: ObjectId,
  sequence: u64,
  candidates: &[HostId],
  quorum: Quorum,
  remote_holders: Vec<(HostId, Endpoint)>,
  budget: CommitBudget,
) -> ContentPlaced {
  let manifest = archive.manifest_identity();
  let mut acked: Vec<HostId> = if candidates.contains(&owner) {
    vec![owner]
  } else {
    Vec::new()
  };
  // Content places on its one current cohort (§4.10): it is fetched by identity from the holders the head
  // names, so a neighbourhood change in flight never needs it joined.
  let shape = Placement::of(candidates);
  let build = |acked: &[HostId]| Placement {
    acked: acked.to_vec(),
    ..shape.clone()
  };
  if shape.placed_with(&acked, quorum) {
    return ContentPlaced {
      outcome: Ok(build(&acked)),
      reusable: Vec::new(),
      stragglers: Stragglers::none(),
      latencies_ns: Vec::new(),
      refilled: Vec::new(),
    };
  }
  let deadline_ns = budget.max_deadline_ns();

  // The offer round: every holder concurrently, each answering with what it lacks.
  let offer = ContentMessage::Offer {
    object,
    sequence,
    manifest,
    chunks: archive.chunks.iter().map(|chunk| chunk.identity).collect(),
  }
  .encode();
  let (offered, offers_in_flight) = match dispatch_round(
    remote_holders
      .into_iter()
      .map(|(host, endpoint)| (host, endpoint, offer.clone()))
      .collect(),
    CONTENT_OFFER_STREAM,
    deadline_ns,
  ) {
    Ok(mut rx) => (gather(&mut rx, budget).await, rx),
    Err((error, mut rx)) => {
      // Keep the channels of tasks already admitted: their bounded replies return sessions.
      return ContentPlaced {
        outcome: Err(ClusterError::Runtime(error)),
        reusable: sessions_of(gather(&mut rx, budget).await),
        stragglers: Stragglers::pending(rx),
        latencies_ns: Vec::new(),
        refilled: Vec::new(),
      };
    }
  };

  // The put round: each holder that answered its offer gets the manifest and exactly its missing chunks.
  let (puts, mut reusable, refilled) = puts_for(archive, object, sequence, &manifest, offered);
  if puts.is_empty() {
    return ContentPlaced {
      outcome: Err(ClusterError::NotPlaced {
        placement: build(&acked),
      }),
      reusable,
      stragglers: Stragglers::pending(offers_in_flight),
      latencies_ns: Vec::new(),
      refilled: Vec::new(),
    };
  }
  // The put latency is timed from the put round's dispatch: the offer round before it is the holder
  // reporting what it lacks, not the transfer the hedge trigger is sized for.
  let dispatched_ns = now_ns();
  let mut rx = match dispatch_round(puts, CONTENT_PUT_STREAM, deadline_ns) {
    Ok(rx) => rx,
    Err((error, mut rx)) => {
      reusable.extend(sessions_of(gather(&mut rx, budget).await));
      return ContentPlaced {
        outcome: Err(ClusterError::Runtime(error)),
        reusable,
        stragglers: Stragglers::two_rounds(offers_in_flight, rx),
        latencies_ns: Vec::new(),
        refilled: Vec::new(),
      };
    }
  };
  let Collected {
    reusable: mut returned,
    timed_out,
    latencies_ns,
  } = collect_bound(
    &mut rx,
    &shape,
    quorum,
    budget,
    dispatched_ns,
    &mut acked,
    |host, reply| {
      matches!(
        ContentMessage::decode(reply),
        Ok(ContentMessage::Ack(ack)) if ack.holder == host && ack.binds(object, sequence, &manifest)
      )
    },
  )
  .await;
  reusable.append(&mut returned);
  let stragglers = Stragglers::two_rounds(offers_in_flight, rx);
  let placement = build(&acked);
  let outcome = if placement.placed(quorum) {
    Ok(placement)
  } else if timed_out {
    Err(ClusterError::Uncertain { placement })
  } else {
    Err(ClusterError::NotPlaced { placement })
  };
  ContentPlaced {
    outcome,
    reusable,
    stragglers,
    latencies_ns,
    refilled,
  }
}

/// Fetches the whole archive of `manifest` from a recorded holder over `endpoint`, bounded by
/// `deadline_ns`, verified on arrival (the reader checks every chunk and the manifest hash, and the
/// manifest's identity must be the one asked for). Returns the archive, if the holder had it whole,
/// and the endpoint for reuse whatever the outcome.
pub async fn fetch_content(
  endpoint: Endpoint,
  object: ObjectId,
  manifest: [u8; 32],
  deadline_ns: u64,
) -> (Option<Archive>, Endpoint) {
  let request = ContentMessage::Fetch { object, manifest }.encode();
  let (reply, endpoint) = request_within(
    endpoint,
    CONTENT_FETCH_STREAM,
    Priority::Bulk,
    &request,
    deadline_ns,
  )
  .await;
  let archive = match ContentMessage::decode(&reply.bytes) {
    Ok(ContentMessage::Have { archive }) => Archive::decode(&archive)
      .ok()
      .filter(|archive| archive.manifest_identity() == manifest),
    _ => None,
  };
  (archive, endpoint)
}

/// A real shard memory for the hold's tests: an arena over an anonymous mapping of whole pages, a byte budget
/// over its usable capacity with no headroom, and an unbounded metadata ledger.
#[cfg(test)]
pub(crate) struct TestSpace {
  pub(crate) arena: ChunkArena,
  pub(crate) budget: ShardBudget,
  pub(crate) metadata: MetadataBudget,
}

#[cfg(test)]
impl TestSpace {
  /// Shape: the blocks the largest fixture can hold at once — the ownership oracle's pool (4 chunks), its
  /// objects × manifests (2 × 2) and one verification scratch block, for a hold and its recovered copy at once:
  /// 2 × 9 = 18, rounded up to the buddy's power of two. Every other fixture holds fewer.
  const BLOCKS: usize = 32;

  pub(crate) fn new() -> TestSpace {
    let page = rustix::param::page_size();
    let mut arena = ChunkArena::new(page);
    arena
      .add_region(slates_mem::region::Region::map(page * Self::BLOCKS, page, false).unwrap())
      .unwrap();
    let capacity = u64::try_from(arena.capacity()).unwrap();
    TestSpace {
      arena,
      budget: ShardBudget::new(capacity, 0),
      metadata: MetadataBudget::new(u64::MAX),
    }
  }

  pub(crate) fn space(&mut self) -> HoldSpace<'_> {
    HoldSpace {
      arena: &mut self.arena,
      budget: &mut self.budget,
      metadata: &mut self.metadata,
    }
  }
}

#[cfg(test)]
mod tests {
  use slates_archive::{Entry, Extent, NodeMeta};

  use super::*;

  /// Shape: a fixed object and sequence for the vectors below.
  const OBJECT: ObjectId = ObjectId::new(HostId(3), 9);
  const SEQUENCE: u64 = 4;

  fn hash(seed: u8) -> [u8; 32] {
    [seed; 32]
  }

  /// An archive of one file split across two raw chunks.
  fn two_chunk_archive() -> Archive {
    let first = Archive::raw_chunk(b"first chunk bytes".to_vec());
    let second = Archive::raw_chunk(b"second chunk bytes".to_vec());
    let extents = vec![
      Extent {
        offset: 0,
        len: first.raw_len,
        chunk: first.identity,
        chunk_offset: 0,
      },
      Extent {
        offset: first.raw_len,
        len: second.raw_len,
        chunk: second.identity,
        chunk_offset: 0,
      },
    ];
    Archive {
      base_page_size: 4096,
      chunk_min: 4096,
      chunk_max: 4096,
      created_unix: 1,
      volume_id: 5,
      snapshot_id: 6,
      name_policy_id: 0,
      unicode_version: 0,
      root_meta: NodeMeta::default(),
      manifest: Node::Directory(vec![Entry {
        name: "file".to_owned(),
        // The canonical form: the file's recorded size is the length its extents tile.
        meta: NodeMeta {
          size: first.raw_len + second.raw_len,
          ..NodeMeta::default()
        },
        node: Node::File(extents),
      }]),
      chunks: vec![first, second],
    }
  }

  fn every_message() -> Vec<ContentMessage> {
    vec![
      ContentMessage::Offer {
        object: OBJECT,
        sequence: SEQUENCE,
        manifest: hash(1),
        chunks: vec![hash(2), hash(3)],
      },
      ContentMessage::Missing {
        object: OBJECT,
        sequence: SEQUENCE,
        manifest: hash(1),
        missing: vec![hash(3)],
      },
      ContentMessage::Put {
        object: OBJECT,
        sequence: SEQUENCE,
        archive: vec![7, 8, 9],
      },
      ContentMessage::Ack(ContentAck {
        holder: HostId(11),
        object: OBJECT,
        sequence: SEQUENCE,
        manifest: hash(1),
      }),
      ContentMessage::Fetch {
        object: OBJECT,
        manifest: hash(1),
      },
      ContentMessage::Have {
        archive: vec![1, 2],
      },
    ]
  }

  #[test]
  fn every_message_round_trips() {
    for message in every_message() {
      assert_eq!(ContentMessage::decode(&message.encode()), Ok(message));
    }
  }

  /// Golden vector (§4.9): the acknowledgement's bytes are fixed — kind, holder, object, sequence,
  /// manifest — so two hosts encode one acknowledgement identically.
  #[test]
  fn an_acknowledgement_has_a_golden_encoding() {
    let bytes = ContentMessage::Ack(ContentAck {
      holder: HostId(0x0102),
      object: ObjectId([0xAA; 16]),
      sequence: 0x0304,
      manifest: hash(0xBB),
    })
    .encode();
    let mut expected = vec![KIND_ACK];
    expected.extend_from_slice(&0x0102u64.to_le_bytes());
    expected.extend_from_slice(&[0xAA; 16]);
    expected.extend_from_slice(&0x0304u64.to_le_bytes());
    expected.extend_from_slice(&[0xBB; 32]);
    assert_eq!(bytes, expected);
  }

  /// Hostile input: every message truncated at every length is refused, never a panic.
  #[test]
  fn truncated_input_is_refused() {
    for message in every_message() {
      let bytes = message.encode();
      for cut in 0..bytes.len() {
        assert!(
          ContentMessage::decode(&bytes[..cut]).is_err(),
          "{message:?} cut to {cut} bytes must be refused"
        );
      }
    }
  }

  /// Hostile input: a count larger than the bytes can hold is refused before any allocation, an unknown
  /// kind is refused, and trailing bytes are refused.
  #[test]
  fn oversized_counts_unknown_kinds_and_trailing_bytes_are_refused() {
    let mut offer = ContentMessage::Offer {
      object: OBJECT,
      sequence: SEQUENCE,
      manifest: hash(1),
      chunks: vec![hash(2)],
    }
    .encode();
    let count_at = 1 + OBJECT_BYTES + size_of::<u64>() + HASH_BYTES;
    offer[count_at..count_at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(ContentMessage::decode(&offer), Err(ContentError::BadLength));

    let mut put = ContentMessage::Put {
      object: OBJECT,
      sequence: SEQUENCE,
      archive: vec![1],
    }
    .encode();
    let len_at = 1 + OBJECT_BYTES + size_of::<u64>();
    put[len_at..len_at + 4].copy_from_slice(&(1u32 << 30).to_le_bytes());
    assert_eq!(ContentMessage::decode(&put), Err(ContentError::BadLength));

    assert_eq!(
      ContentMessage::decode(&[0xEE]),
      Err(ContentError::UnknownKind { kind: 0xEE })
    );
    let mut fetch = ContentMessage::Fetch {
      object: OBJECT,
      manifest: hash(1),
    }
    .encode();
    fetch.push(0);
    assert_eq!(ContentMessage::decode(&fetch), Err(ContentError::BadLength));
  }

  /// §4.10 placement closure: a hold refuses an archive whose manifest references a chunk neither
  /// shipped nor held, holds a complete one, answers an offer with exactly what it lacks, and hands
  /// the whole archive back by identity.
  #[test]
  fn a_hold_requires_every_referenced_chunk_and_reassembles_the_archive() {
    let archive = two_chunk_archive();
    let identities: Vec<[u8; 32]> = archive.chunks.iter().map(|c| c.identity).collect();
    let mut hold = ContentHold::new();
    let mut room = TestSpace::new();
    assert_eq!(hold.missing_of(OBJECT, &identities), identities);

    let partial = with_chunks(&archive, vec![archive.chunks[0].clone()]);
    assert_eq!(
      hold.hold(&mut room.space(), OBJECT, Placed::default(), partial),
      Err(ContentRefusal::Incomplete { missing: 1 }),
      "a manifest referencing an unshipped, unheld chunk is not held"
    );
    assert_eq!(hold.chunk_count(), 0, "nothing stored on a refusal");

    let manifest = hold
      .hold(
        &mut room.space(),
        OBJECT,
        Placed::default(),
        archive.clone(),
      )
      .unwrap();
    assert_eq!(manifest, archive.manifest_identity());
    assert!(hold.holds_manifest(OBJECT, &manifest));
    assert_eq!(hold.chunk_count(), 2);
    assert!(hold.missing_of(OBJECT, &identities).is_empty());
    let whole = hold.archive_of(&room.arena, OBJECT, &manifest).unwrap();
    assert_eq!(
      whole.encode(),
      archive.encode(),
      "the archive reassembles byte for byte"
    );
    assert_eq!(referenced_chunks(&archive.manifest), identities);
  }

  /// The holder side served end to end in-process: an offer to a hold with one chunk already present
  /// answers the one missing identity; a put of just that chunk is acknowledged bound to the object,
  /// sequence and manifest; a put whose chunk is corrupted (a flipped bit) is refused with an empty
  /// reply; a fetch hands the whole archive back.
  #[test]
  fn serve_answers_offers_puts_and_fetches_and_refuses_a_corrupt_chunk() {
    let archive = two_chunk_archive();
    let holder = HostId(21);
    let mut hold = ContentHold::new();
    let mut room = TestSpace::new();
    // The holder already has the first chunk (say from an earlier snapshot).
    let mut earlier = with_chunks(&archive, vec![archive.chunks[0].clone()]);
    earlier.manifest = Node::File(vec![Extent {
      offset: 0,
      len: archive.chunks[0].raw_len,
      chunk: archive.chunks[0].identity,
      chunk_offset: 0,
    }]);
    hold
      .hold(&mut room.space(), OBJECT, Placed::default(), earlier)
      .unwrap();

    let offer = ContentMessage::Offer {
      object: OBJECT,
      sequence: SEQUENCE,
      manifest: archive.manifest_identity(),
      chunks: archive.chunks.iter().map(|c| c.identity).collect(),
    };
    let Ok(ContentMessage::Missing { missing, .. }) = ContentMessage::decode(
      &hold
        .serve(
          &mut room.space(),
          holder,
          &offer.encode(),
          |_, _| true,
          |_, _, _| true,
        )
        .0,
    ) else {
      panic!("an offer is answered with the missing set");
    };
    assert_eq!(missing, vec![archive.chunks[1].identity]);

    // A corrupt put: flip a bit in the missing chunk's payload.
    let mut corrupt = with_chunks(&archive, vec![archive.chunks[1].clone()]);
    corrupt.chunks[0].payload[0] ^= 0x01;
    let refused = hold
      .serve(
        &mut room.space(),
        holder,
        &ContentMessage::Put {
          object: OBJECT,
          sequence: SEQUENCE,
          archive: corrupt.encode(),
        }
        .encode(),
        |_, _| true,
        |_, _, _| true,
      )
      .0;
    assert!(refused.is_empty(), "a corrupt chunk is refused, not held");
    assert_eq!(hold.chunk_count(), 1);

    let partial = with_chunks(&archive, vec![archive.chunks[1].clone()]);
    let reply = hold
      .serve(
        &mut room.space(),
        holder,
        &ContentMessage::Put {
          object: OBJECT,
          sequence: SEQUENCE,
          archive: partial.encode(),
        }
        .encode(),
        |_, _| true,
        |_, _, _| true,
      )
      .0;
    let Ok(ContentMessage::Ack(ack)) = ContentMessage::decode(&reply) else {
      panic!("a complete put is acknowledged");
    };
    assert!(ack.binds(OBJECT, SEQUENCE, &archive.manifest_identity()));
    assert!(!ack.binds(OBJECT, SEQUENCE + 1, &archive.manifest_identity()));
    assert_eq!(ack.holder, holder);
    assert_eq!(hold.chunk_count(), 2, "exactly the missing chunk was added");

    let fetched = hold
      .serve(
        &mut room.space(),
        holder,
        &ContentMessage::Fetch {
          object: OBJECT,
          manifest: archive.manifest_identity(),
        }
        .encode(),
        |_, _| true,
        |_, _, _| true,
      )
      .0;
    let Ok(ContentMessage::Have { archive: bytes }) = ContentMessage::decode(&fetched) else {
      panic!("a fetch is answered with the archive");
    };
    assert_eq!(Archive::decode(&bytes).unwrap(), archive);
    assert!(
      hold
        .serve(
          &mut room.space(),
          holder,
          &ContentMessage::Fetch {
            object: OBJECT,
            manifest: hash(0xCC)
          }
          .encode(),
          |_, _| true,
          |_, _, _| true,
        )
        .0
        .is_empty(),
      "a manifest not held is an empty answer"
    );
  }

  /// A second object, for the scoping tests.
  const OTHER: ObjectId = ObjectId([9; OBJECT_BYTES]);

  /// AUD-29-45 (§4.13 "Content identity and sharing"): do: hold one object's archive, then ask about the same
  /// chunks and manifest for another object — an offer's missing set, a put leaning on them unshipped, a
  /// fetch; expect every answer as if nothing were held: the chunks missing, the put refused incomplete, the
  /// fetch empty — while the first object's own answers are unchanged, and the bytes stored once.
  #[test]
  fn another_objects_content_is_neither_revealed_nor_lent() {
    let archive = two_chunk_archive();
    let identities: Vec<[u8; 32]> = archive.chunks.iter().map(|c| c.identity).collect();
    let mut hold = ContentHold::new();
    let mut room = TestSpace::new();
    let manifest = hold
      .hold(
        &mut room.space(),
        OBJECT,
        Placed::default(),
        archive.clone(),
      )
      .unwrap();
    assert_eq!(hold.missing_of(OTHER, &identities), identities);
    let leaning = with_chunks(&archive, Vec::new());
    assert_eq!(
      hold.hold(&mut room.space(), OTHER, Placed::default(), leaning),
      Err(ContentRefusal::Incomplete { missing: 2 })
    );
    assert!(hold.archive_of(&room.arena, OTHER, &manifest).is_none());
    let fetched = hold
      .serve(
        &mut room.space(),
        HostId(3),
        &ContentMessage::Fetch {
          object: OTHER,
          manifest,
        }
        .encode(),
        |_, _| true,
        |_, _, _| true,
      )
      .0;
    assert!(
      fetched.is_empty(),
      "another object's manifest is not served"
    );
    assert!(hold.missing_of(OBJECT, &identities).is_empty());
    hold
      .hold(&mut room.space(), OTHER, Placed::default(), archive)
      .unwrap();
    assert_eq!(
      hold.chunk_count(),
      2,
      "the bytes are kept once for both objects"
    );
    assert!(hold.forget_manifest(&mut room.space(), OBJECT, &manifest));
    assert!(
      hold.archive_of(&room.arena, OTHER, &manifest).is_some(),
      "forgetting one object's copy keeps the other's"
    );
  }

  /// AUD-29-45: do: serve an offer, a put and a fetch whose authority check refuses; expect the same empty
  /// reply an unheld request draws, nothing stored, the check asked with the right access and object, and
  /// each refusal counted.
  #[test]
  fn a_refused_authority_draws_the_empty_reply_and_is_counted() {
    let archive = two_chunk_archive();
    let mut hold = ContentHold::new();
    let mut room = TestSpace::new();
    let requests = [
      (
        ContentAccess::Place,
        ContentMessage::Offer {
          object: OBJECT,
          sequence: SEQUENCE,
          manifest: archive.manifest_identity(),
          chunks: archive.chunks.iter().map(|c| c.identity).collect(),
        },
      ),
      (
        ContentAccess::Place,
        ContentMessage::Put {
          object: OBJECT,
          sequence: SEQUENCE,
          archive: archive.encode(),
        },
      ),
      (
        ContentAccess::Read,
        ContentMessage::Fetch {
          object: OBJECT,
          manifest: archive.manifest_identity(),
        },
      ),
    ];
    for (expected, request) in &requests {
      let (reply, _) = hold.serve(
        &mut room.space(),
        HostId(4),
        &request.encode(),
        |access, object| {
          assert_eq!((access, object), (*expected, OBJECT));
          false
        },
        |_, _, _| true,
      );
      assert!(reply.is_empty());
    }
    assert_eq!(hold.chunk_count(), 0);
    assert_eq!(hold.unauthorized(), 3);
  }
}

/// The content hold's manifest-to-chunk ownership oracle (AUD-29-44; AC-7.3, AC-8.12). Each generated case
/// is a **shape** — a set of distinct manifests, each referencing a non-empty subset of a chunk pool — and a
/// **history** over it: puts (any object, any manifest, any subset of the pool shipped: the referenced
/// chunks, unreferenced ones, or none; retries included) and forgets. The history runs against the hold and
/// against a serial model that knows only which object holds which manifests. After every step the two must
/// agree: every manifest the model holds is held and reconstructible to exactly its referenced chunks,
/// nothing the model does not hold is, the store keeps exactly the chunks some held manifest references (no
/// orphan, no premature eviction), and a refused put changes nothing.
///
/// Nothing about the shape is hand-picked: the manifests' overlaps are generated. The generator's bounds are
/// the smallest at which every case the ownership rule distinguishes can occur (each bound's `Derived:` line
/// names the case that needs it), and a **case census** counts each case as the run meets it; the run fails
/// if any case was never met, so a generator that silently stopped reaching a case cannot pass as an oracle.
/// The run is deterministic (a fixed-seed runner), so the census is a stable fact, not a probability.
#[cfg(test)]
mod ownership_oracle {
  use std::cell::RefCell;
  use std::collections::{BTreeMap, BTreeSet};

  use proptest::prelude::*;
  use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};
  use slates_archive::format::{MAX_BASE_PAGE_BYTES, MAX_CHUNK_BYTES};
  use slates_archive::{Entry, Extent, NodeMeta};

  use super::*;

  /// Derived: two objects is the smallest count at which one chunk can be held for two objects at once and a
  /// put can be refused although the chunk it lacks is held — for another object (cross-object isolation).
  const OBJECTS: usize = 2;
  /// Derived: two manifests is the smallest count at which manifests share a chunk, so a put can complete
  /// from a chunk another held manifest owns and a forget can keep a chunk another manifest still references.
  const MANIFESTS: usize = 2;
  /// Derived: two shared-or-private roles per manifest pair need three chunks (one shared, one private to
  /// each of the two manifests), and a put shipping a chunk no manifest references needs a fourth.
  const POOL: usize = 3 + 1;
  /// Derived: filling every (object, manifest) slot and emptying it again, so every model state is reachable
  /// from the empty one within one history.
  const HISTORY: usize = 2 * OBJECTS * MANIFESTS;

  /// The chunk at pool index `at`: its bytes are the index, so every pool chunk has its own identity.
  fn chunk(at: usize) -> Chunk {
    Archive::raw_chunk(at.to_le_bytes().to_vec())
  }

  /// The manifest referencing `referenced`, with `shipped` of the pool attached (by pool index). The header
  /// is the format's own bounds and zero ids: ownership reads only the manifest and the chunks.
  fn archive(referenced: &BTreeSet<usize>, shipped: &BTreeSet<usize>) -> Archive {
    let entries = referenced
      .iter()
      .map(|&at| {
        let chunk = chunk(at);
        Entry {
          name: format!("c{at}"),
          meta: NodeMeta {
            size: chunk.raw_len,
            ..NodeMeta::default()
          },
          node: Node::File(vec![Extent {
            offset: 0,
            len: chunk.raw_len,
            chunk: chunk.identity,
            chunk_offset: 0,
          }]),
        }
      })
      .collect();
    Archive {
      base_page_size: u32::try_from(MAX_BASE_PAGE_BYTES).unwrap(),
      chunk_min: u32::try_from(MAX_CHUNK_BYTES).unwrap(),
      chunk_max: u32::try_from(MAX_CHUNK_BYTES).unwrap(),
      created_unix: 0,
      volume_id: 0,
      snapshot_id: 0,
      name_policy_id: 0,
      unicode_version: 0,
      root_meta: NodeMeta::default(),
      manifest: Node::Directory(entries),
      chunks: shipped.iter().map(|&at| chunk(at)).collect(),
    }
  }

  fn identity(referenced: &BTreeSet<usize>) -> [u8; 32] {
    archive(referenced, &BTreeSet::new()).manifest_identity()
  }

  fn object(at: usize) -> ObjectId {
    ObjectId::new(HostId(1), u64::try_from(at).unwrap())
  }

  #[derive(Clone, Debug)]
  enum Step {
    Put {
      object: usize,
      manifest: usize,
      shipped: BTreeSet<usize>,
      /// Whether the put is **offered**, as an owner ships it: after an offer, exactly the manifest's chunks
      /// the object does not yet hold (the missing set), whatever `shipped` says. The other arm ships
      /// `shipped` as drawn and reaches the refusals.
      offered: bool,
    },
    Forget {
      object: usize,
      manifest: usize,
    },
  }

  /// A step, generated as a plain tuple so the strategy needs no `Arc`-backed `prop_oneof!` (R2).
  fn step() -> impl Strategy<Value = Step> {
    (
      any::<bool>(),
      0..OBJECTS,
      0..MANIFESTS,
      proptest::collection::btree_set(0..POOL, 0..=POOL),
      any::<bool>(),
    )
      .prop_map(|(put, object, manifest, shipped, offered)| {
        if put {
          Step::Put {
            object,
            manifest,
            shipped,
            offered,
          }
        } else {
          Step::Forget { object, manifest }
        }
      })
  }

  /// A shape: distinct manifests (equal chunk sets are one manifest, one identity), each non-empty.
  fn shape() -> impl Strategy<Value = Vec<BTreeSet<usize>>> {
    proptest::collection::btree_set(
      proptest::collection::btree_set(0..POOL, 1..=POOL),
      1..=MANIFESTS,
    )
    .prop_map(|manifests| manifests.into_iter().collect())
  }

  /// Every case the ownership rule distinguishes; the census counts each as a history meets it.
  #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
  enum Case {
    /// A put completed with every referenced chunk shipped.
    PutWholeShipped,
    /// A put completed with a referenced chunk not shipped but held by another of the object's manifests.
    PutCompletedFromHeld,
    /// A put refused: a referenced chunk neither shipped nor held for the object.
    PutRefused,
    /// A put refused although the missing chunk is held — for another object.
    PutRefusedHeldElsewhere,
    /// A put of a manifest the object already holds.
    Retry,
    /// A put refused for shipping a chunk the manifest does not reference.
    PutRefusedUnreferenced,
    /// One chunk held for two objects at once.
    SharedAcrossObjects,
    /// A forget that kept a chunk another of the object's manifests still references.
    ForgetKept,
    /// A forget that released a chunk nothing references any more.
    ForgetReleased,
    /// A forget of a manifest the object does not hold.
    ForgetNotHeld,
  }

  /// Every case, for the census's completeness check.
  const CASES: [Case; 10] = [
    Case::PutWholeShipped,
    Case::PutCompletedFromHeld,
    Case::PutRefused,
    Case::PutRefusedHeldElsewhere,
    Case::Retry,
    Case::PutRefusedUnreferenced,
    Case::SharedAcrossObjects,
    Case::ForgetKept,
    Case::ForgetReleased,
    Case::ForgetNotHeld,
  ];

  /// The model: for each object, the indices of the manifests it holds.
  type Model = BTreeMap<usize, BTreeSet<usize>>;

  /// The chunks `object`'s held manifests reference.
  fn chunks_of(model: &Model, manifests: &[BTreeSet<usize>], object: usize) -> BTreeSet<usize> {
    model
      .get(&object)
      .into_iter()
      .flatten()
      .filter_map(|&index| manifests.get(index))
      .flatten()
      .copied()
      .collect()
  }

  /// Applies `step` to the model by the rule — a put shipping a chunk its manifest does not reference is
  /// refused; otherwise a put completes when every referenced chunk is shipped now or referenced by a
  /// manifest the object already holds; a held manifest's retry completes — and records the
  /// cases it meets. Returns whether the step succeeded, or `None` for a step naming no manifest of the shape.
  fn model_apply(
    model: &mut Model,
    manifests: &[BTreeSet<usize>],
    step: &Step,
    census: &mut BTreeSet<Case>,
  ) -> Option<bool> {
    match step {
      Step::Put {
        object,
        manifest,
        shipped,
        ..
      } => {
        let referenced = manifests.get(*manifest)?;
        if !shipped.is_subset(referenced) {
          census.insert(Case::PutRefusedUnreferenced);
          return Some(false);
        }
        if model
          .get(object)
          .is_some_and(|held| held.contains(manifest))
        {
          census.insert(Case::Retry);
          return Some(true);
        }
        let have = chunks_of(model, manifests, *object);
        let missing: BTreeSet<usize> = referenced
          .iter()
          .filter(|chunk| !shipped.contains(chunk) && !have.contains(chunk))
          .copied()
          .collect();
        if !missing.is_empty() {
          census.insert(Case::PutRefused);
          let elsewhere = (0..OBJECTS)
            .filter(|other| other != object)
            .any(|other| !missing.is_disjoint(&chunks_of(model, manifests, other)));
          if elsewhere {
            census.insert(Case::PutRefusedHeldElsewhere);
          }
          return Some(false);
        }
        census.insert(if referenced.is_subset(shipped) {
          Case::PutWholeShipped
        } else {
          Case::PutCompletedFromHeld
        });
        model.entry(*object).or_default().insert(*manifest);
        Some(true)
      }
      Step::Forget { object, manifest } => {
        let referenced = manifests.get(*manifest)?;
        let removed = model
          .get_mut(object)
          .is_some_and(|held| held.remove(manifest));
        if !removed {
          census.insert(Case::ForgetNotHeld);
          return Some(false);
        }
        let still = chunks_of(model, manifests, *object);
        if !referenced.is_disjoint(&still) {
          census.insert(Case::ForgetKept);
        }
        if !referenced.is_subset(&still) {
          census.insert(Case::ForgetReleased);
        }
        Some(true)
      }
    }
  }

  /// Compares the hold with the model; `Err` names the first disagreement.
  fn check(
    hold: &ContentHold,
    arena: &ChunkArena,
    model: &Model,
    manifests: &[BTreeSet<usize>],
  ) -> Result<(), String> {
    let mut referenced = BTreeSet::new();
    for at in 0..OBJECTS {
      for (index, chunks) in manifests.iter().enumerate() {
        let id = identity(chunks);
        let held = model.get(&at).is_some_and(|held| held.contains(&index));
        if !held {
          if hold.holds_manifest(object(at), &id) {
            return Err(format!(
              "object {at} holds manifest {index} the model does not"
            ));
          }
          continue;
        }
        let archive = hold
          .archive_of(arena, object(at), &id)
          .ok_or_else(|| format!("object {at} manifest {index} is not reconstructible"))?;
        let got: BTreeSet<[u8; 32]> = archive.chunks.iter().map(|c| c.identity).collect();
        let expected: BTreeSet<[u8; 32]> = chunks.iter().map(|&c| chunk(c).identity).collect();
        if got != expected {
          return Err(format!(
            "object {at} manifest {index} reconstructed other chunks"
          ));
        }
        referenced.extend(chunks.iter().copied());
      }
    }
    let manifest_count: usize = model.values().map(BTreeSet::len).sum();
    if hold.manifest_count() != manifest_count {
      return Err(format!(
        "{} manifests held, the model holds {manifest_count}",
        hold.manifest_count()
      ));
    }
    if hold.chunk_count() != referenced.len() {
      return Err(format!(
        "{} chunks stored, held manifests reference {}",
        hold.chunk_count(),
        referenced.len()
      ));
    }
    Ok(())
  }

  /// `step` as it runs against `manifests` from the model's state: an offered put ships exactly its manifest's
  /// chunks the object does not yet hold — the missing set the offer draws.
  fn resolve(step: &Step, manifests: &[BTreeSet<usize>], model: &Model) -> Step {
    match step {
      Step::Put {
        object,
        manifest,
        offered: true,
        ..
      } => {
        let have = chunks_of(model, manifests, *object);
        Step::Put {
          object: *object,
          manifest: *manifest,
          shipped: manifests
            .get(*manifest)
            .map(|referenced| referenced.difference(&have).copied().collect())
            .unwrap_or_default(),
          offered: false,
        }
      }
      other => other.clone(),
    }
  }

  /// Runs one shape and history on a fresh hold against the model, recording the cases met.
  fn run(
    manifests: &[BTreeSet<usize>],
    steps: &[Step],
    census: &mut BTreeSet<Case>,
  ) -> Result<(), String> {
    let mut hold = ContentHold::new();
    let mut room = TestSpace::new();
    let mut model = Model::new();
    for step in steps {
      let step = &resolve(step, manifests, &model);
      let Some(expected) = model_apply(&mut model, manifests, step, census) else {
        continue;
      };
      let got = match step {
        Step::Put {
          object: at,
          manifest,
          shipped,
          ..
        } => manifests.get(*manifest).is_some_and(|chunks| {
          hold
            .hold(
              &mut room.space(),
              object(*at),
              Placed::default(),
              archive(chunks, shipped),
            )
            .is_ok()
        }),
        Step::Forget {
          object: at,
          manifest,
        } => manifests.get(*manifest).is_some_and(|chunks| {
          hold.forget_manifest(&mut room.space(), object(*at), &identity(chunks))
        }),
      };
      if got != expected {
        return Err(format!(
          "{step:?}: the hold answered {got}, the model {expected}"
        ));
      }
      check(&hold, &room.arena, &model, manifests)
        .map_err(|disagreement| format!("after {step:?}: {disagreement}"))?;
      // AUD-29-43: what the hold says it is charged is exactly what the shard's budget and ledger hold.
      if (room.budget.replicated(), room.metadata.committed())
        != (hold.charged_bytes(), hold.index_bytes())
      {
        return Err(format!(
          "after {step:?}: the charges drifted from the hold's account"
        ));
      }
      let shared = chunks_of(&model, manifests, 0);
      if (1..OBJECTS).any(|other| !shared.is_disjoint(&chunks_of(&model, manifests, other))) {
        census.insert(Case::SharedAcrossObjects);
      }
    }
    // AUD-29-59: what a restart recovers from the hold's image is the same hold — the oracle's whole check
    // holds against the recovered hold too, and its image is byte-identical (deterministic).
    let image = hold.to_image(&room.arena);
    let mut recovered = ContentHold::from_image(&mut room.space(), &image)
      .map_err(|refused| format!("image refused: {refused:?}"))?;
    check(&recovered, &room.arena, &model, manifests)
      .map_err(|disagreement| format!("recovered: {disagreement}"))?;
    if recovered.to_image(&room.arena) != image {
      return Err("the recovered hold images differently".to_owned());
    }
    // AUD-29-43: releasing everything gives back every charge and every block — nothing leaks.
    hold.forget_all(&mut room.space());
    recovered.forget_all(&mut room.space());
    let left = (
      room.budget.replicated(),
      room.metadata.committed(),
      room.arena.allocated_bytes(),
    );
    if left != (0, 0, 0) {
      return Err(format!(
        "released everything, left (bytes, index, arena) = {left:?}"
      ));
    }
    Ok(())
  }

  /// AUD-29-44's three witnesses, as the audit ran them, over two manifests that share one chunk: do: (1)
  /// put one manifest twice and forget it once, (2) hold the first, hold the second without shipping the
  /// shared chunk, then forget the first, (3) ship an unreferenced chunk with a manifest; expect (1) no chunk
  /// retained, (2) the second still reconstructible with the shared chunk kept, (3) the put refused
  /// `Unreferenced` before anything is stored (AUD-29-43 restricts shipped content to the manifest's closure),
  /// so nothing can be orphaned.
  #[test]
  fn the_audits_three_ownership_witnesses_hold() {
    let (shared, only_first, only_second, unreferenced) = (0, 1, 2, 3);
    let first = BTreeSet::from([shared, only_first]);
    let second = BTreeSet::from([shared, only_second]);
    let a = object(0);

    let mut hold = ContentHold::new();

    let mut room = TestSpace::new();
    hold
      .hold(
        &mut room.space(),
        a,
        Placed::default(),
        archive(&first, &first),
      )
      .unwrap();
    hold
      .hold(
        &mut room.space(),
        a,
        Placed::default(),
        archive(&first, &first),
      )
      .unwrap();
    hold.forget_manifest(&mut room.space(), a, &identity(&first));
    assert_eq!(
      (hold.manifest_count(), hold.chunk_count()),
      (0, 0),
      "(1) nothing retained"
    );

    let mut hold = ContentHold::new();

    let mut room = TestSpace::new();
    hold
      .hold(
        &mut room.space(),
        a,
        Placed::default(),
        archive(&first, &first),
      )
      .unwrap();
    hold
      .hold(
        &mut room.space(),
        a,
        Placed::default(),
        archive(&second, &BTreeSet::from([only_second])),
      )
      .expect("the shared chunk is already held for the object");
    hold.forget_manifest(&mut room.space(), a, &identity(&first));
    let kept = hold
      .archive_of(&room.arena, a, &identity(&second))
      .expect("(2) the second manifest is still whole");
    assert_eq!(kept.chunks.len(), second.len());

    let mut hold = ContentHold::new();

    let mut room = TestSpace::new();
    let with_extra: BTreeSet<usize> = first.iter().copied().chain([unreferenced]).collect();
    assert_eq!(
      hold.hold(
        &mut room.space(),
        a,
        Placed::default(),
        archive(&first, &with_extra)
      ),
      Err(ContentRefusal::Unreferenced)
    );
    assert_eq!(
      (hold.manifest_count(), hold.chunk_count()),
      (0, 0),
      "(3) nothing stored, so nothing orphaned"
    );
  }

  /// AUD-29-43 (§4.2): do: hold a manifest in a shard memory whose unpromised capacity is one block short of
  /// the put's charge, then in one with room; expect the first refused `NoCapacity` with nothing held, charged
  /// or allocated, and the second held with its blocks charged.
  #[test]
  fn a_put_past_the_unpromised_capacity_is_refused_whole() {
    let a = object(0);
    let first = BTreeSet::from([0, 1]);
    let mut room = TestSpace::new();
    let capacity = room.budget.capacity();
    // Every block but one is promised to someone else: the put needs three (two chunks, the manifest).
    let page = u64::try_from(rustix::param::page_size()).unwrap();
    let promised = room.budget.reserve(capacity - 2 * page).unwrap();
    let mut hold = ContentHold::new();
    let refused = hold.hold(
      &mut room.space(),
      a,
      Placed::default(),
      archive(&first, &first),
    );
    assert!(
      matches!(refused, Err(ContentRefusal::NoCapacity { .. })),
      "{refused:?}"
    );
    assert_eq!(
      (
        hold.manifest_count(),
        room.budget.replicated(),
        room.metadata.committed(),
        room.arena.allocated_bytes()
      ),
      (0, 0, 0, 0)
    );
    room.budget.release(promised);
    hold
      .hold(
        &mut room.space(),
        a,
        Placed::default(),
        archive(&first, &first),
      )
      .unwrap();
    assert_eq!(room.budget.replicated(), 3 * page);
    assert_eq!(hold.charged_bytes(), 3 * page);
  }

  /// Hostile input (§4.9) on the hold image (AUD-29-59): do: image a hold of two overlapping manifests, then
  /// decode every truncation, the image with a trailing byte, one with a chunk's payload byte flipped, and one
  /// naming an unknown encoding; expect the whole image to recover, and each damaged one refused typed —
  /// never a panic, never a hold of content that fails its identity.
  #[test]
  fn a_damaged_hold_image_is_refused_and_a_whole_one_recovers() {
    let a = object(0);
    let first = BTreeSet::from([0, 1]);
    let second = BTreeSet::from([1, 2]);
    let mut hold = ContentHold::new();
    let mut room = TestSpace::new();
    hold
      .hold(
        &mut room.space(),
        a,
        Placed { sequence: 3 },
        archive(&first, &first),
      )
      .unwrap();
    hold
      .hold(
        &mut room.space(),
        a,
        Placed { sequence: 4 },
        archive(&second, &BTreeSet::from([2])),
      )
      .unwrap();
    let image = hold.to_image(&room.arena);
    let recovered = ContentHold::from_image(&mut room.space(), &image).unwrap();
    assert_eq!(
      (recovered.manifest_count(), recovered.chunk_count()),
      (2, 3)
    );
    assert_eq!(recovered.newest_placed(a), Some(4));
    for cut in 0..image.len() {
      assert!(
        ContentHold::from_image(&mut room.space(), &image[..cut]).is_err() || cut == 0,
        "cut {cut}"
      );
    }
    let mut padded = image.clone();
    padded.push(0);
    assert_eq!(
      ContentHold::from_image(&mut room.space(), &padded).err(),
      Some(HoldImageError::Malformed)
    );
    let mut decoded = HoldImage::from_bytes(&image).unwrap();
    if let Some(byte) = decoded.chunks[0].payload.first_mut() {
      *byte ^= 0x01;
    }
    assert_eq!(
      ContentHold::from_image(&mut room.space(), &decoded.to_bytes()).err(),
      Some(HoldImageError::Refused(ContentRefusal::IdentityMismatch))
    );
    let mut foreign = HoldImage::from_bytes(&image).unwrap();
    foreign.chunks[0].encoding = u8::MAX;
    assert_eq!(
      ContentHold::from_image(&mut room.space(), &foreign.to_bytes()).err(),
      Some(HoldImageError::UnknownEncoding)
    );
  }

  /// AUD-29-44: do: run generated shapes and histories of puts and forgets on a hold and on the serial model,
  /// with a fixed-seed runner at proptest's default case count; expect them to agree after every step (see
  /// the module doc), and expect the census to have met every case the ownership rule distinguishes.
  #[test]
  fn the_hold_owns_exactly_what_its_manifests_reference() {
    let census = RefCell::new(BTreeSet::new());
    let mut runner = TestRunner::new_with_rng(
      slates_test_seeds::unseeded(Config::default()),
      TestRng::deterministic_rng(RngAlgorithm::ChaCha),
    );
    let strategy = (shape(), proptest::collection::vec(step(), 1..=HISTORY));
    let result = runner.run(&strategy, |(manifests, steps)| {
      run(&manifests, &steps, &mut census.borrow_mut()).map_err(TestCaseError::fail)
    });
    if let Err(failure) = result {
      panic!("AUD-29-44: {failure}");
    }
    let met = census.into_inner();
    let unmet: Vec<Case> = CASES
      .into_iter()
      .filter(|case| !met.contains(case))
      .collect();
    assert!(unmet.is_empty(), "the generator never reached {unmet:?}");
  }
}
