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
//! ([`CONTENT_OFFER_STREAM`], [`CONTENT_CHUNK_STREAM`], [`CONTENT_FETCH_STREAM`] — so the holder's
//! `serve_once` dispatches by kind, never by guessing at bytes; every exchange rides a fresh stream id
//! in the `Bulk` class, so a content transfer never queues a record commit or a probe sharing the
//! session behind it):
//! - **`Offer` → `Missing` or `Ack`**: the owner names the object and sequence and sends the manifest (the
//!   archive's header and tree, no chunks). The holder checks it, opens a **stage** for it (or finds the one
//!   a cut transfer left), and answers with the referenced chunks it holds neither for the object nor in the
//!   stage — or, when nothing is missing, completes the placement and acknowledges at once.
//! - **`Chunk` → `Staged` or `Ack`**: one chunk per exchange, many exchanges in flight on the session. The
//!   holder checks the chunk is referenced by the staged manifest, verifies it against its identity and keeps
//!   it in the stage (§4.9 "verified ranges and resumable progress"); the reply names how many chunks the
//!   stage still lacks. The chunk that completes the closure draws the acknowledgement — **bound** to the
//!   object, sequence and manifest, so a stale or foreign one cannot count toward a placement (the record
//!   plane's same discipline: network receipt is not acceptance).
//! - **`Fetch` → `Have`, then `FetchChunk` → `Piece`**: a reader — a takeover successor materializing the
//!   volume, a remote attach — asks a recorded holder for a manifest by identity (§4.10 "fetches the
//!   manifest by identity from a recorded holder"), then for each chunk it lacks, one per exchange, many in
//!   flight. It stages what it fetches through the same stage a placement fills, so every chunk is
//!   verified as it arrives and a cut fetch keeps what it verified (AUD-29-55): the next fetch asks only for
//!   the rest.
//!
//! **Resumption (AUD-29-55).** A stage keeps every chunk that arrived verified, so a transfer cut at any
//! point — a deadline, a cancelled round, a session replaced — loses at most the chunks still in flight: the
//! next offer's missing set names exactly the verified work still owed. Stages ride the shard's recovery
//! image at each publish it makes, so a warm restart keeps the progress a publish carried; a stage is not
//! an acknowledgement, so progress since the last publish carries no durability promise and is re-sent. A stage is not a placement and is never acknowledged; it is charged
//! like held content, one per object, and released by events, never by time: its promotion, a newer stage
//! for the object, or the holder's retention rule (a record past its sequence that does not name it, a newer
//! placement, the object's tombstone, the stale-copy reclaim). Until 2026-10-01 a put was one whole archive
//! in one stream body, verified only after its last byte: a cut discarded every byte it had carried, and the
//! next offer named them all again.
//!
//! The owner's dispatch ([`put_content`]) runs each holder's offer and, as that holder's missing set
//! arrives, its chunks — every holder independently, so a slow offer never holds back a fast holder's
//! transfer (AUD-29-58: until 2026-10-01 every put waited for the slowest offer). It collects bound
//! acknowledgements to the quorum under the commit budget's progress-extension policy, the same policy the
//! record commit uses. Holders still in flight at the return hand their sessions back through
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

use slates_archive::format::{ArchiveError, Chunk, Encoding, MAX_CHUNK_BYTES};
use slates_archive::{Archive, chunks_for};
use slates_db::register::{HostId, ObjectId, Placement, Quorum};
use slates_mem::arena::{ChunkArena, Extent};
use slates_mem::budget::{MetadataBudget, ShardBudget};
use slates_mem::error::MemError;
use slates_rt::error::RtError;
use slates_rt::futures::{detach, now_ns, spawn_child};
use slates_transport::connection::Priority;
use slates_transport::endpoint::{Endpoint, EndpointError};
use slates_transport::streams::StreamRefusal;
use slates_wire::Wire;

use crate::{
  ClusterError, CommitBudget, DispatchWait, Reply, Stragglers, TimedReply, request_within,
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
/// The stream a **chunk** exchange rides (see [`CONTENT_OFFER_STREAM`] for why each exchange has its own).
/// Format: one stream id per RPC kind on a connection; a fixed label, not a tunable.
pub const CONTENT_CHUNK_STREAM: u64 = 5;
/// The stream a **fetch** rides (see [`CONTENT_OFFER_STREAM`] for why each exchange has its own).
/// Format: one stream id per RPC kind on a connection; a fixed label, not a tunable.
pub const CONTENT_FETCH_STREAM: u64 = 6;

/// Whether `stream` carries a content exchange a holder answers with [`ContentHold::serve`].
pub fn is_content_stream(stream: u64) -> bool {
  matches!(
    stream,
    CONTENT_OFFER_STREAM | CONTENT_CHUNK_STREAM | CONTENT_FETCH_STREAM
  )
}

/// Format: the message kind byte that leads every content message.
const KIND_OFFER: u8 = 1;
/// Format: the kind byte of a holder's missing-set reply.
const KIND_MISSING: u8 = 2;
/// Format: the kind byte of one chunk sent into a stage.
const KIND_CHUNK: u8 = 3;
/// Format: the kind byte of a holder's acknowledgement.
const KIND_ACK: u8 = 4;
/// Format: the kind byte of a fetch by manifest identity.
const KIND_FETCH: u8 = 5;
/// Format: the kind byte of a fetch's answer.
const KIND_HAVE: u8 = 6;
/// Format: the kind byte of a holder's progress reply to a chunk its stage kept.
const KIND_STAGED: u8 = 7;
/// Format: the kind byte of a fetch of one chunk of a held manifest.
const KIND_FETCH_CHUNK: u8 = 8;
/// Format: the kind byte of a fetched chunk.
const KIND_PIECE: u8 = 9;
/// Format: the width of a BLAKE3 identity on the wire.
const HASH_BYTES: usize = 32;
/// Format: the width of an object id on the wire.
const OBJECT_BYTES: usize = size_of::<ObjectId>();

/// A content-plane message (§4.10), one per exchange direction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContentMessage {
  /// The owner offers a snapshot's content: the object and head sequence it belongs to, and the manifest
  /// — the archive's header and tree with no chunks ([`Archive::encode`]) — so the holder can stage it and
  /// name the referenced chunks it lacks.
  Offer {
    /// The object (the volume) the content belongs to.
    object: ObjectId,
    /// The head sequence the content is placed for.
    sequence: u64,
    /// The encoded manifest-only archive.
    archive: Vec<u8>,
  },
  /// The holder's answer to an offer it staged: the chunks it lacks.
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
  /// One chunk for the stage of `manifest` on `object`.
  Chunk {
    /// The object.
    object: ObjectId,
    /// The sequence the content is placed for.
    sequence: u64,
    /// The staged manifest the chunk belongs to.
    manifest: [u8; 32],
    /// The chunk, payload as stored.
    chunk: Chunk,
  },
  /// The holder's reply to a chunk its stage verified and kept: how many referenced chunks it still lacks.
  /// Progress, never an acknowledgement.
  Staged {
    /// The object.
    object: ObjectId,
    /// The sequence.
    sequence: u64,
    /// The staged manifest.
    manifest: [u8; 32],
    /// How many chunks the stage still lacks.
    remaining: u64,
  },
  /// The holder's bound acknowledgement of content it verified and holds whole.
  Ack(ContentAck),
  /// A reader asks for a manifest by identity, **for an object**: a holder serves it only to a host with
  /// authority over that object, and only from what it holds for that object (AUD-29-45).
  Fetch {
    /// The object whose content is asked for.
    object: ObjectId,
    /// The manifest identity.
    manifest: [u8; 32],
  },
  /// A holder's answer to a fetch: the manifest — the archive's header and tree with no chunks.
  Have {
    /// The encoded manifest-only archive.
    archive: Vec<u8>,
  },
  /// A reader asks for one chunk of a manifest the object holds (AUD-29-55).
  FetchChunk {
    /// The object whose content is asked for.
    object: ObjectId,
    /// The held manifest the chunk belongs to.
    manifest: [u8; 32],
    /// The chunk's identity.
    chunk: [u8; 32],
  },
  /// A holder's answer to a chunk fetch: the chunk, payload as stored.
  Piece {
    /// The object.
    object: ObjectId,
    /// The manifest.
    manifest: [u8; 32],
    /// The chunk.
    chunk: Chunk,
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

  /// One chunk's record: its identity, lengths, encoding, level, dictionary and payload. Each length is
  /// checked against the format's chunk cap and the payload against the stored length before it is copied.
  fn chunk(&mut self) -> Result<Chunk, ContentError> {
    let identity = self.hash()?;
    let raw_len = self.u64()?;
    let stored_len = self.u64()?;
    let encoding = Encoding::from_wire(self.u8()?).ok_or(ContentError::BadLength)?;
    let level = self.u8()?;
    let dictionary = self.hash()?;
    if raw_len > MAX_CHUNK_BYTES || stored_len > MAX_CHUNK_BYTES {
      return Err(ContentError::BadLength);
    }
    let payload = self.blob()?;
    if u64::try_from(payload.len()).ok() != Some(stored_len) {
      return Err(ContentError::BadLength);
    }
    Ok(Chunk {
      identity,
      raw_len,
      stored_len,
      encoding,
      level,
      dictionary,
      payload,
    })
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

fn put_chunk(out: &mut Vec<u8>, chunk: &Chunk) {
  out.extend_from_slice(&chunk.identity);
  out.extend_from_slice(&chunk.raw_len.to_le_bytes());
  out.extend_from_slice(&chunk.stored_len.to_le_bytes());
  out.push(chunk.encoding.to_wire());
  out.push(chunk.level);
  out.extend_from_slice(&chunk.dictionary);
  put_blob(out, &chunk.payload);
}

/// A chunk's record as the content plane writes it: the bytes a sealed chunk seals whole (A-92), its identity among
/// them, so nothing of the plaintext travels outside the seal.
pub(crate) fn chunk_record(chunk: &Chunk) -> Vec<u8> {
  let mut out = Vec::with_capacity(chunk.payload.len().saturating_add(CHUNK_RECORD_FIELDS));
  put_chunk(&mut out, chunk);
  out
}

/// Format: the bytes of a chunk record besides its payload: the identity (32), the raw and stored lengths (8 + 8),
/// the encoding and level (1 + 1), the dictionary (32) and the payload's length (4).
const CHUNK_RECORD_FIELDS: usize = 32 + 8 + 8 + 1 + 1 + 32 + 4;

/// The chunk a whole record holds ([`chunk_record`]'s inverse), refusing a record with bytes past it.
pub(crate) fn chunk_from_record(bytes: &[u8]) -> Result<Chunk, ContentError> {
  let mut reader = Reader { bytes, at: 0 };
  let chunk = reader.chunk()?;
  reader.finish()?;
  Ok(chunk)
}

/// The head every placement message after the kind byte carries: the object, the sequence and the manifest.
fn put_placement(out: &mut Vec<u8>, object: &ObjectId, sequence: u64, manifest: &[u8; 32]) {
  out.extend_from_slice(&object.0);
  out.extend_from_slice(&sequence.to_le_bytes());
  out.extend_from_slice(manifest);
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
        archive,
      } => {
        out.push(KIND_OFFER);
        out.extend_from_slice(&object.0);
        out.extend_from_slice(&sequence.to_le_bytes());
        put_blob(&mut out, archive);
      }
      ContentMessage::Missing {
        object,
        sequence,
        manifest,
        missing,
      } => {
        out.push(KIND_MISSING);
        put_placement(&mut out, object, *sequence, manifest);
        put_hashes(&mut out, missing);
      }
      ContentMessage::Chunk {
        object,
        sequence,
        manifest,
        chunk,
      } => {
        out.push(KIND_CHUNK);
        put_placement(&mut out, object, *sequence, manifest);
        put_chunk(&mut out, chunk);
      }
      ContentMessage::Staged {
        object,
        sequence,
        manifest,
        remaining,
      } => {
        out.push(KIND_STAGED);
        put_placement(&mut out, object, *sequence, manifest);
        out.extend_from_slice(&remaining.to_le_bytes());
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
      ContentMessage::FetchChunk {
        object,
        manifest,
        chunk,
      } => {
        out.push(KIND_FETCH_CHUNK);
        out.extend_from_slice(&object.0);
        out.extend_from_slice(manifest);
        out.extend_from_slice(chunk);
      }
      ContentMessage::Piece {
        object,
        manifest,
        chunk,
      } => {
        out.push(KIND_PIECE);
        out.extend_from_slice(&object.0);
        out.extend_from_slice(manifest);
        put_chunk(&mut out, chunk);
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
        archive: reader.blob()?,
      },
      KIND_MISSING => ContentMessage::Missing {
        object: reader.object()?,
        sequence: reader.u64()?,
        manifest: reader.hash()?,
        missing: reader.hashes()?,
      },
      KIND_CHUNK => ContentMessage::Chunk {
        object: reader.object()?,
        sequence: reader.u64()?,
        manifest: reader.hash()?,
        chunk: reader.chunk()?,
      },
      KIND_STAGED => ContentMessage::Staged {
        object: reader.object()?,
        sequence: reader.u64()?,
        manifest: reader.hash()?,
        remaining: reader.u64()?,
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
      KIND_FETCH_CHUNK => ContentMessage::FetchChunk {
        object: reader.object()?,
        manifest: reader.hash()?,
        chunk: reader.hash()?,
      },
      KIND_PIECE => ContentMessage::Piece {
        object: reader.object()?,
        manifest: reader.hash()?,
        chunk: reader.chunk()?,
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
  /// An offer for an object whose stage holds a different manifest placed for the same or a newer sequence
  /// (AUD-29-55): the owner has one placement in flight per object, so the older offer is stale.
  StaleStage,
  /// A chunk names a manifest the object has no stage for — never staged, or released since (AUD-29-55).
  Unstaged,
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
    root_meta: archive.root_meta.clone(),
    manifest: archive.manifest.clone(),
    chunks,
  }
}

/// A hold's canonical image (AUD-29-59), carried in its shard's recovery image so a content acknowledgement
/// survives a warm restart: every distinct chunk stored (held or staged), once, in identity order, every
/// held manifest with its object and latest placement, in (object, identity) order, and every stage with
/// the chunks it verified (AUD-29-55: a transfer's progress survives a warm restart too) — so two holds of
/// equal content image byte-identically (a determinism gate).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
struct HoldImage {
  /// The distinct chunks stored.
  chunks: Vec<ChunkImage>,
  /// The held manifests.
  manifests: Vec<ManifestImage>,
  /// The transfers in progress.
  stages: Vec<StageImage>,
}

/// A block of the shard's arena a [`HoldImage`] names (A-64): its region, offset and length, and the bytes of it
/// in use. The hold's bytes stay in the arena range of the anchor's content object across a restart, so the image
/// names them instead of carrying them, and a barrier costs the hold's index, not its replicas' bytes.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
struct BlockRef {
  region: u16,
  offset: u64,
  block_len: u64,
  used: u64,
}

impl BlockRef {
  fn of(extent: &Extent, used: usize) -> BlockRef {
    BlockRef {
      region: extent.region(),
      offset: as_count(extent.offset()),
      block_len: as_count(extent.len()),
      used: as_count(used),
    }
  }
}

/// One stage of a [`HoldImage`]: its object, the sequence it is placed for, the block holding its manifest-only
/// archive, and the chunks it verified, in identity order.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
struct StageImage {
  object: [u8; 16],
  sequence: u64,
  archive: BlockRef,
  staged: Vec<[u8; 32]>,
}

/// One chunk of a [`HoldImage`]: a `Chunk`'s fields, the encoding as its wire byte, and the block holding its
/// payload.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
struct ChunkImage {
  identity: [u8; 32],
  raw_len: u64,
  stored_len: u64,
  encoding: u8,
  level: u8,
  dictionary: [u8; 32],
  payload: BlockRef,
}

/// One held manifest of a [`HoldImage`]: its object, the sequence it was last placed for, and the block holding
/// the archive it arrived as with no chunks (its header and tree).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
struct ManifestImage {
  object: [u8; 16],
  sequence: u64,
  archive: BlockRef,
}

/// A hold image whose blocks are claimed in the arena, between [`ContentHold::claim_image`] and
/// [`ContentHold::from_claimed`]: the decoded image and each claimed block, by the place the image names it.
#[derive(Debug, Default)]
pub struct ClaimedHold {
  image: Option<HoldImage>,
  blocks: BTreeMap<(u16, u64), Extent>,
}

impl ClaimedHold {
  /// Gives every claim back (a refused claim, before anything is committed).
  fn give_back(&mut self, arena: &mut ChunkArena) {
    for (_, extent) in std::mem::take(&mut self.blocks) {
      let _ = arena.free(extent);
    }
  }
}

/// Why a hold image could not be recovered (AUD-29-59): its bytes do not decode as one image, a block it names
/// cannot be claimed (A-64: one no allocation could have made, or one another claim holds), a chunk names an
/// unknown encoding, a manifest's archive does not decode, or a manifest refused to hold again (a chunk it
/// references is missing or fails its identity).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HoldImageError {
  /// The bytes are not exactly one hold image.
  Malformed,
  /// A block the image names cannot be claimed in the arena (A-64).
  Unclaimable,
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
  /// One transfer in progress per object (AUD-29-55).
  stages: BTreeMap<ObjectId, Stage>,
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
  /// While a hold is rebuilt from its image (A-64), the claimed block the image names for each thing it stores:
  /// storing those bytes takes the block, checked byte for byte, instead of allocating a copy. Empty otherwise.
  adoptable: BTreeMap<Adopt, Extent>,
}

/// What a claimed block holds, as the image names it (A-64): a chunk's payload by its identity, or a manifest-only
/// archive (a held manifest's or a stage's) by its object and manifest identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Adopt {
  Chunk([u8; 32]),
  Manifest([u8; 16], [u8; 32]),
}

/// A chunk stored in the arena once, whatever objects reference it: its block, its charge (the block's
/// length), the fields a `Chunk` carries besides its payload, how many objects reference it, and how many
/// stages have it verified and kept (AUD-29-55). Its block goes when both counts are zero.
#[derive(Debug)]
struct StoredChunk {
  extent: Extent,
  raw_len: u64,
  stored_len: u64,
  encoding: Encoding,
  level: u8,
  dictionary: [u8; 32],
  objects: u64,
  staged: u64,
}

/// A transfer in progress for one object (AUD-29-55; §4.9 "verified ranges and resumable progress"): the
/// manifest being placed and the sequence it is placed for, the arena block holding its manifest-only
/// archive, the chunk cap its header declares, the referenced chunks still missing, the chunks it has
/// verified and kept, and the index bytes it is charged — released whole with it.
#[derive(Debug)]
struct Stage {
  manifest: [u8; 32],
  placed: Placed,
  extent: Extent,
  len: usize,
  chunk_max: u64,
  missing: BTreeSet<[u8; 32]>,
  staged: BTreeSet<[u8; 32]>,
  missing_charged: u64,
  index: u64,
}

/// What opening a stage found: the manifest already held whole for the object (its placement refreshed), or
/// a stage and the chunks it lacks.
enum Opened {
  Held,
  Missing(Vec<[u8; 32]>),
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

/// The index cost of a new stage: its entry and the roots of its two sets.
fn stage_entry_bytes() -> u64 {
  btree_entry_bytes::<ObjectId, Stage>()
    .saturating_add(btree_map_bytes::<[u8; 32], ()>().saturating_mul(2))
}

/// The index cost of one identity in a stage's missing or staged set.
fn stage_set_entry_bytes() -> u64 {
  btree_entry_bytes::<[u8; 32], ()>()
}

/// The identities of a count, as a charge multiplier.
fn as_count(items: usize) -> u64 {
  u64::try_from(items).unwrap_or(u64::MAX)
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

  /// The chunks the stage for `object` has verified and kept, if a transfer of `manifest` is in progress —
  /// the progress a cut transfer resumes from (AUD-29-55).
  pub fn staged_of(&self, object: ObjectId, manifest: &[u8; 32]) -> Option<usize> {
    self
      .stages
      .get(&object)
      .filter(|stage| &stage.manifest == manifest)
      .map(|stage| stage.staged.len())
  }

  /// How many transfers are in progress (stages), across objects.
  pub fn stage_count(&self) -> usize {
    self.stages.len()
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

  /// Holds `archive` — a manifest and chunks — for `object`, placed as `placed`, in `space`, through the same
  /// stage a transfer fills (AUD-29-55), and returns the manifest identity now held: only chunks the manifest
  /// references may be shipped (refused `Unreferenced` before any is decoded), every chunk it references must
  /// be shipped, already held for this object or already staged, every shipped chunk not yet stored is
  /// verified, and every byte and index entry is admitted from the shard's unpromised capacity or refused
  /// (`NoCapacity`). Holding a manifest already held stores nothing and refreshes its placement. A refusal
  /// leaves nothing this call took, except progress into a stage that already existed for the manifest.
  pub fn hold(
    &mut self,
    space: &mut HoldSpace<'_>,
    object: ObjectId,
    placed: Placed,
    archive: Archive,
  ) -> Result<[u8; 32], ContentRefusal> {
    let referenced: BTreeSet<[u8; 32]> = archive.referenced_chunks().into_iter().collect();
    if archive
      .chunks
      .iter()
      .any(|chunk| !referenced.contains(&chunk.identity))
    {
      return Err(ContentRefusal::Unreferenced);
    }
    let manifest = with_chunks(&archive, Vec::new());
    let identity = manifest.manifest_identity();
    let (opened, created) = self.open_stage(space, object, placed, &manifest)?;
    if let Opened::Held = opened {
      return Ok(identity);
    }
    let mut outcome = Ok(identity);
    for chunk in archive.chunks {
      if let Err(refusal) = self.stage_chunk(space, object, &identity, chunk) {
        outcome = Err(refusal);
        break;
      }
    }
    let outcome = outcome.and_then(|_| self.promote(space, object));
    if outcome.is_err() && created {
      self.drop_stage(space, object);
    }
    outcome
  }

  /// The arena block length `len` bytes take, as a charge; `NoCapacity` when no block class fits.
  fn block_bytes(arena: &ChunkArena, len: u64) -> Result<u64, ContentRefusal> {
    usize::try_from(len)
      .ok()
      .and_then(|len| arena.block_len(len))
      .map(as_count)
      .ok_or(ContentRefusal::NoCapacity {
        requested: len,
        available: 0,
      })
  }

  /// Takes a charge whole or refuses with nothing taken: `index` bytes on the metadata ledger, and `kept`
  /// plus `transient` bytes from the shard's unpromised capacity — `kept` stays charged to this hold (blocks
  /// it keeps), `transient` is a verification scratch the caller gives back once verified.
  fn take(
    &mut self,
    space: &mut HoldSpace<'_>,
    kept: u64,
    transient: u64,
    index: u64,
  ) -> Result<(), ContentRefusal> {
    let credit = space.metadata.reserve(index).map_err(|e| no_capacity(&e))?;
    // A holder whose shard claims its arena lazily grows it from the pool first (A-98).
    space
      .arena
      .make_room(space.budget, kept.saturating_add(transient));
    if let Err(e) = space
      .budget
      .charge_replicated(kept.saturating_add(transient))
    {
      space.metadata.release(credit);
      return Err(no_capacity(&e));
    }
    self.charged_bytes = self.charged_bytes.saturating_add(kept);
    self.index_bytes = self.index_bytes.saturating_add(index);
    Ok(())
  }

  /// Gives back a charge [`take`](Self::take) took (or the part of it still held).
  fn give(&mut self, space: &mut HoldSpace<'_>, kept: u64, transient: u64, index: u64) {
    space
      .budget
      .credit_replicated(kept.saturating_add(transient));
    space
      .metadata
      .release(slates_mem::budget::MetadataCredit { bytes: index });
    self.charged_bytes = self.charged_bytes.saturating_sub(kept);
    self.index_bytes = self.index_bytes.saturating_sub(index);
  }

  /// Whether `object`'s held content references the chunk `identity`.
  fn held_for(&self, object: ObjectId, identity: &[u8; 32]) -> bool {
    self
      .objects
      .get(&object)
      .is_some_and(|held| held.chunks.contains_key(identity))
  }

  /// Opens the stage for `manifest` (an archive with no chunks) on `object`, placed as `placed` — or finds
  /// the one a cut transfer left and resumes it — and returns what it found and whether this call created it.
  /// A manifest already held for the object only refreshes its placement. A stage of another manifest is
  /// replaced by a newer placement and refuses an older one (`StaleStage`). The stage's manifest block and
  /// index are admitted whole or refused.
  fn open_stage(
    &mut self,
    space: &mut HoldSpace<'_>,
    object: ObjectId,
    placed: Placed,
    manifest: &Archive,
  ) -> Result<(Opened, bool), ContentRefusal> {
    let identity = manifest.manifest_identity();
    if let Some(record) = self
      .objects
      .get_mut(&object)
      .and_then(|held| held.manifests.get_mut(&identity))
    {
      record.placed = placed;
      return Ok((Opened::Held, false));
    }
    let referenced: BTreeSet<[u8; 32]> = manifest.referenced_chunks().into_iter().collect();
    if let Some(stage) = self.stages.get(&object) {
      if stage.manifest == identity {
        return self
          .resume_stage(space, object, placed, &referenced)
          .map(|missing| (Opened::Missing(missing), false));
      }
      if placed.sequence <= stage.placed.sequence {
        return Err(ContentRefusal::StaleStage);
      }
      self.drop_stage(space, object);
    }
    let missing: BTreeSet<[u8; 32]> = referenced
      .iter()
      .filter(|identity| !self.held_for(object, identity))
      .copied()
      .collect();
    let bytes = manifest.encode();
    let block = Self::block_bytes(space.arena, as_count(bytes.len()))?;
    let index = stage_entry_bytes()
      .saturating_add(stage_set_entry_bytes().saturating_mul(as_count(missing.len())));
    self.take(space, block, 0, index)?;
    let extent = match self.place_bytes(space.arena, &bytes, Adopt::Manifest(object.0, identity)) {
      Ok(extent) => extent,
      Err(refusal) => {
        self.give(space, block, 0, index);
        return Err(refusal);
      }
    };
    let listed: Vec<[u8; 32]> = missing.iter().copied().collect();
    self.stages.insert(
      object,
      Stage {
        manifest: identity,
        placed,
        extent,
        len: bytes.len(),
        chunk_max: u64::from(manifest.chunk_max),
        missing_charged: as_count(missing.len()),
        missing,
        staged: BTreeSet::new(),
        index,
      },
    );
    Ok((Opened::Missing(listed), true))
  }

  /// Resumes `object`'s stage for a re-offer of its manifest: the placement moves to the newer sequence, and
  /// the missing set is recomputed — a chunk held for the object when the stage opened may have been released
  /// since — with any growth in it charged. Returns what is still missing.
  fn resume_stage(
    &mut self,
    space: &mut HoldSpace<'_>,
    object: ObjectId,
    placed: Placed,
    referenced: &BTreeSet<[u8; 32]>,
  ) -> Result<Vec<[u8; 32]>, ContentRefusal> {
    let staged = self
      .stages
      .get(&object)
      .map(|stage| stage.staged.clone())
      .unwrap_or_default();
    let missing: BTreeSet<[u8; 32]> = referenced
      .iter()
      .filter(|identity| !self.held_for(object, identity) && !staged.contains(*identity))
      .copied()
      .collect();
    let charged = self
      .stages
      .get(&object)
      .map_or(0, |stage| stage.missing_charged);
    let growth = as_count(missing.len()).saturating_sub(charged);
    let extra = stage_set_entry_bytes().saturating_mul(growth);
    if extra > 0 {
      self.take(space, 0, 0, extra)?;
    }
    let listed: Vec<[u8; 32]> = missing.iter().copied().collect();
    if let Some(stage) = self.stages.get_mut(&object) {
      stage.placed = Placed {
        sequence: stage.placed.sequence.max(placed.sequence),
      };
      stage.missing = missing;
      stage.missing_charged = stage.missing_charged.saturating_add(growth);
      stage.index = stage.index.saturating_add(extra);
    }
    Ok(listed)
  }

  /// Keeps one chunk in `object`'s stage of `manifest` (AUD-29-55): the chunk must be one the stage lacks
  /// (one it already has is a duplicate, answered with the progress unchanged; one the manifest does not
  /// reference is refused `Unreferenced`), no larger than the manifest's header declares, and — unless its
  /// bytes are already stored — verified against its identity in a charged scratch block and stored in a
  /// charged arena block. Returns how many chunks the stage still lacks.
  fn stage_chunk(
    &mut self,
    space: &mut HoldSpace<'_>,
    object: ObjectId,
    manifest: &[u8; 32],
    chunk: Chunk,
  ) -> Result<u64, ContentRefusal> {
    let identity = chunk.identity;
    let Some(stage) = self
      .stages
      .get(&object)
      .filter(|stage| &stage.manifest == manifest)
    else {
      return Err(ContentRefusal::Unstaged);
    };
    let remaining = as_count(stage.missing.len());
    if !stage.missing.contains(&identity) {
      return if stage.staged.contains(&identity) || self.held_for(object, &identity) {
        Ok(remaining)
      } else {
        Err(ContentRefusal::Unreferenced)
      };
    }
    if chunk.raw_len > stage.chunk_max {
      return Err(ContentRefusal::Malformed(ArchiveError::ChunkTooLarge {
        index: 0,
      }));
    }
    let entry = stage_set_entry_bytes();
    if let Some(stored) = self.chunks.get(&identity) {
      // The bytes are stored already (for another object or stage): this stage takes a reference only.
      let _ = stored;
      self.take(space, 0, 0, entry)?;
      if let Some(stored) = self.chunks.get_mut(&identity) {
        stored.staged = stored.staged.saturating_add(1);
      }
    } else {
      self.store_new_chunk(space, &chunk, entry)?;
    }
    let Some(stage) = self.stages.get_mut(&object) else {
      return Err(ContentRefusal::Unstaged);
    };
    stage.missing.remove(&identity);
    stage.staged.insert(identity);
    stage.index = stage.index.saturating_add(entry);
    Ok(as_count(stage.missing.len()))
  }

  /// Verifies and stores a chunk no stage or object has stored yet, as one stage's reference: its block, its
  /// verification scratch (an encoded chunk), its index entry and the stage's set entry `entry` are charged
  /// first, and a refusal at any step gives every charge back.
  fn store_new_chunk(
    &mut self,
    space: &mut HoldSpace<'_>,
    chunk: &Chunk,
    entry: u64,
  ) -> Result<(), ContentRefusal> {
    let block = Self::block_bytes(space.arena, chunk.stored_len)?;
    let scratch = if chunk.encoding == Encoding::Raw {
      0
    } else {
      Self::block_bytes(space.arena, chunk.raw_len)?
    };
    let index = chunk_entry_bytes().saturating_add(entry);
    self.take(space, block, scratch, index)?;
    if let Err(refusal) = Self::verify(space, &[chunk], scratch) {
      self.give(space, block, scratch, index);
      return Err(refusal);
    }
    // Verification is done: its scratch goes back before anything is stored.
    space.budget.credit_replicated(scratch);
    let extent = match self.place_bytes(space.arena, &chunk.payload, Adopt::Chunk(chunk.identity)) {
      Ok(extent) => extent,
      Err(refusal) => {
        self.give(space, block, 0, index);
        return Err(refusal);
      }
    };
    self.chunks.insert(
      chunk.identity,
      StoredChunk {
        extent,
        raw_len: chunk.raw_len,
        stored_len: chunk.stored_len,
        encoding: chunk.encoding,
        level: chunk.level,
        dictionary: chunk.dictionary,
        objects: 0,
        staged: 1,
      },
    );
    Ok(())
  }

  /// Completes `object`'s stage into a held manifest once every chunk its manifest references is held for
  /// the object or staged (§4.10 placement closure): the manifest's block moves from the stage to the held
  /// record, the object gains a reference to every referenced chunk, and the stage's own index goes back.
  /// A stage still short (a held chunk released while it filled) records what it lacks and is refused
  /// `Incomplete`; the stage stays for the next offer.
  fn promote(
    &mut self,
    space: &mut HoldSpace<'_>,
    object: ObjectId,
  ) -> Result<[u8; 32], ContentRefusal> {
    let Some(stage) = self.stages.get(&object) else {
      return Err(ContentRefusal::Unstaged);
    };
    let (identity, placed) = (stage.manifest, stage.placed);
    let decoded = space
      .arena
      .bytes(stage.extent)
      .and_then(|bytes| bytes.get(..stage.len))
      .map(Archive::decode);
    let manifest = match decoded {
      Some(Ok(manifest)) => manifest,
      Some(Err(error)) => {
        self.drop_stage(space, object);
        return Err(ContentRefusal::Malformed(error));
      }
      None => {
        self.drop_stage(space, object);
        return Err(ContentRefusal::Unstaged);
      }
    };
    let referenced: BTreeSet<[u8; 32]> = manifest.referenced_chunks().into_iter().collect();
    let lacking: Vec<[u8; 32]> = referenced
      .iter()
      .filter(|chunk| !self.held_for(object, chunk) && !stage.staged.contains(*chunk))
      .copied()
      .collect();
    if !lacking.is_empty() {
      let missing = lacking.len();
      self.resume_stage(space, object, placed, &referenced)?;
      return Err(ContentRefusal::Incomplete { missing });
    }
    if self
      .objects
      .get(&object)
      .is_some_and(|held| held.manifests.contains_key(&identity))
    {
      self.drop_stage(space, object);
      return Ok(identity);
    }
    let new_references = referenced
      .iter()
      .filter(|chunk| !self.held_for(object, chunk))
      .count();
    let new_object = !self.objects.contains_key(&object);
    let index = manifest_entry_bytes()
      .saturating_add(reference_entry_bytes().saturating_mul(as_count(new_references)))
      .saturating_add(if new_object { object_entry_bytes() } else { 0 });
    self.take(space, 0, 0, index)?;
    let Some(stage) = self.stages.remove(&object) else {
      self.give(space, 0, 0, index);
      return Err(ContentRefusal::Unstaged);
    };
    let held = self.objects.entry(object).or_default();
    for chunk_identity in &referenced {
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
        extent: stage.extent,
        len: stage.len,
        placed: stage.placed,
      },
    );
    for chunk_identity in &stage.staged {
      if let Some(chunk) = self.chunks.get_mut(chunk_identity) {
        chunk.staged = chunk.staged.saturating_sub(1);
      }
    }
    self.give(space, 0, 0, stage.index);
    Ok(identity)
  }

  /// Releases `object`'s stage (AUD-29-55): its manifest block and its index go back, and each chunk it kept
  /// loses the stage's reference — a chunk no object and no other stage references goes with its block and
  /// charge. Returns whether a stage was there.
  fn drop_stage(&mut self, space: &mut HoldSpace<'_>, object: ObjectId) -> bool {
    let Some(stage) = self.stages.remove(&object) else {
      return false;
    };
    let mut bytes = as_count(stage.extent.len());
    let mut index = stage.index;
    let _ = space.arena.free(stage.extent);
    for chunk_identity in &stage.staged {
      let gone = self.chunks.get_mut(chunk_identity).is_some_and(|chunk| {
        chunk.staged = chunk.staged.saturating_sub(1);
        chunk.objects == 0 && chunk.staged == 0
      });
      if gone && let Some(chunk) = self.chunks.remove(chunk_identity) {
        bytes = bytes.saturating_add(as_count(chunk.extent.len()));
        index = index.saturating_add(chunk_entry_bytes());
        let _ = space.arena.free(chunk.extent);
      }
    }
    self.give(space, bytes, 0, index);
    true
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

  /// A block holding `bytes`: during a rebuild from an image, the claimed block the image names for `key` (A-64),
  /// when it holds exactly these bytes; otherwise a new block they are copied into.
  fn place_bytes(
    &mut self,
    arena: &mut ChunkArena,
    bytes: &[u8],
    key: Adopt,
  ) -> Result<Extent, ContentRefusal> {
    if !self.adoptable.is_empty() {
      let same = self.adoptable.get(&key).is_some_and(|extent| {
        arena
          .bytes(*extent)
          .and_then(|block| block.get(..bytes.len()))
          .is_some_and(|held| held == bytes)
      });
      if same && let Some(extent) = self.adoptable.remove(&key) {
        return Ok(extent);
      }
    }
    Self::store_bytes(arena, bytes)
  }

  /// Allocates a block for `bytes` and copies them in; `NoCapacity` when the arena has no block that fits.
  fn store_bytes(arena: &mut ChunkArena, bytes: &[u8]) -> Result<Extent, ContentRefusal> {
    let extent = arena.alloc(bytes.len()).map_err(|e| no_capacity(&e))?;
    let Some(block) = arena
      .bytes_mut(extent)
      .and_then(|block| block.get_mut(..bytes.len()))
    else {
      let _ = arena.free(extent);
      return Err(ContentRefusal::NoCapacity {
        requested: as_count(bytes.len()),
        available: 0,
      });
    };
    block.copy_from_slice(bytes);
    Ok(extent)
  }

  /// The archive (header and tree, no chunks) a held manifest's block holds, or `None` if it does not decode
  /// (the hold wrote it canonically; a failure is memory corruption, answered as not held).
  fn manifest_of(arena: &ChunkArena, record: &HeldManifest) -> Option<Archive> {
    let bytes = arena.bytes(record.extent)?.get(..record.len)?;
    Archive::decode(bytes).ok()
  }

  /// Keeps, of what is held for `object`, only the manifests `keep` accepts given their identity and latest
  /// placement, releasing the rest exactly as [`forget_manifest`](Self::forget_manifest) does (AUD-29-43:
  /// the holder's retention rule is the caller's, from its accepted records); the object's stage is judged
  /// by the same rule and released whole when it fails it (AUD-29-55). Returns how many manifests and
  /// stages were released.
  pub fn retain(
    &mut self,
    space: &mut HoldSpace<'_>,
    object: ObjectId,
    mut keep: impl FnMut(&[u8; 32], Placed) -> bool,
  ) -> usize {
    let stage_released = self
      .stages
      .get(&object)
      .is_some_and(|stage| !keep(&stage.manifest, stage.placed))
      && self.drop_stage(space, object);
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
      .saturating_add(usize::from(stage_released))
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
      .map(|archive| archive.referenced_chunks().into_iter().collect())
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
        chunk.objects == 0 && chunk.staged == 0
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
  /// object (the bytes go when no object or stage references them), and the object's stage (AUD-29-55). The
  /// authoritative releases call it — a destroyed object's tombstone, a copy reclaimed as stale. Returns how
  /// many manifests were held.
  pub fn forget_object(&mut self, space: &mut HoldSpace<'_>, object: ObjectId) -> usize {
    self.drop_stage(space, object);
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
    let objects: Vec<ObjectId> = self
      .objects
      .keys()
      .chain(self.stages.keys())
      .copied()
      .collect::<BTreeSet<_>>()
      .into_iter()
      .collect();
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
    for referenced in archive.referenced_chunks() {
      chunks.push(Self::chunk_of(
        arena,
        &referenced,
        self.chunks.get(&referenced)?,
      )?);
    }
    Some(with_chunks(&archive, chunks))
  }

  /// The manifest-only archive bytes of a manifest held for `object`, as stored, or `None` if not held for it.
  fn manifest_bytes(
    &self,
    arena: &ChunkArena,
    object: ObjectId,
    identity: &[u8; 32],
  ) -> Option<Vec<u8>> {
    let record = self.objects.get(&object)?.manifests.get(identity)?;
    Some(arena.bytes(record.extent)?.get(..record.len)?.to_vec())
  }

  /// The chunk `chunk` for a reader of `object`'s manifest `identity`: served only when the object holds that
  /// manifest and its held content references the chunk (AUD-29-45: never another object's bytes).
  fn piece_of(
    &self,
    arena: &ChunkArena,
    object: ObjectId,
    identity: &[u8; 32],
    chunk: &[u8; 32],
  ) -> Option<Chunk> {
    let held = self.objects.get(&object)?;
    if !held.manifests.contains_key(identity) || !held.chunks.contains_key(chunk) {
      return None;
    }
    Self::chunk_of(arena, chunk, self.chunks.get(chunk)?)
  }

  /// Stages a fetched `manifest` (an archive with no chunks) for `object`, placed as `placed` (AUD-29-55): a
  /// reader's fetch fills the same stage a placement does. `None` when the manifest is already held for the
  /// object; else the chunks still to fetch — what neither a held manifest nor an earlier, cut fetch kept.
  pub fn stage_fetched(
    &mut self,
    space: &mut HoldSpace<'_>,
    object: ObjectId,
    placed: Placed,
    manifest: &Archive,
  ) -> Result<Option<Vec<[u8; 32]>>, ContentRefusal> {
    if !manifest.chunks.is_empty() {
      return Err(ContentRefusal::Unreferenced);
    }
    match self.open_stage(space, object, placed, manifest)? {
      (Opened::Held, _) => Ok(None),
      (Opened::Missing(missing), _) => Ok(Some(missing)),
    }
  }

  /// Keeps one fetched chunk in `object`'s stage of `manifest`, verified against its identity; returns how
  /// many chunks the stage still lacks.
  pub fn stage_piece(
    &mut self,
    space: &mut HoldSpace<'_>,
    object: ObjectId,
    manifest: &[u8; 32],
    chunk: Chunk,
  ) -> Result<u64, ContentRefusal> {
    self.stage_chunk(space, object, manifest, chunk)
  }

  /// Completes `object`'s stage into a held manifest once its closure is whole; `Incomplete` while chunks
  /// are still owed (the stage stays for the next fetch).
  pub fn complete_stage(
    &mut self,
    space: &mut HoldSpace<'_>,
    object: ObjectId,
  ) -> Result<[u8; 32], ContentRefusal> {
    self.promote(space, object)
  }

  /// The hold's canonical image (AUD-29-59), naming every byte by its block (A-64): the stored chunks, each held
  /// manifest's archive and each stage's, in key order. No byte is copied; the blocks are in the arena range of the
  /// anchor's content object and survive the daemon. Empty bytes for an empty hold.
  pub fn to_image(&self) -> Vec<u8> {
    if self.objects.is_empty() && self.stages.is_empty() {
      return Vec::new();
    }
    let mut manifests = Vec::new();
    for (object, held) in &self.objects {
      for record in held.manifests.values() {
        manifests.push(ManifestImage {
          object: object.0,
          sequence: record.placed.sequence,
          archive: BlockRef::of(&record.extent, record.len),
        });
      }
    }
    let chunks = self
      .chunks
      .iter()
      .map(|(identity, stored)| ChunkImage {
        identity: *identity,
        raw_len: stored.raw_len,
        stored_len: stored.stored_len,
        encoding: stored.encoding.to_wire(),
        level: stored.level,
        dictionary: stored.dictionary,
        payload: BlockRef::of(
          &stored.extent,
          usize::try_from(stored.stored_len).unwrap_or(usize::MAX),
        ),
      })
      .collect();
    let stages = self
      .stages
      .iter()
      .map(|(object, stage)| StageImage {
        object: object.0,
        sequence: stage.placed.sequence,
        archive: BlockRef::of(&stage.extent, stage.len),
        staged: stage.staged.iter().copied().collect(),
      })
      .collect();
    HoldImage {
      chunks,
      manifests,
      stages,
    }
    .to_bytes()
  }

  /// The first half of rebuilding a hold from its image (A-64): the image decoded and every block it names claimed
  /// in the arena, before anything else allocates there, so no block it names is handed to anything else. Empty
  /// bytes claim nothing. A block that cannot be claimed, or whose used length exceeds it, refuses with every claim
  /// given back.
  pub fn claim_image(arena: &mut ChunkArena, bytes: &[u8]) -> Result<ClaimedHold, HoldImageError> {
    if bytes.is_empty() {
      return Ok(ClaimedHold::default());
    }
    let image = HoldImage::from_bytes(bytes).map_err(|_| HoldImageError::Malformed)?;
    let named = image
      .chunks
      .iter()
      .map(|chunk| chunk.payload)
      .chain(image.manifests.iter().map(|manifest| manifest.archive))
      .chain(image.stages.iter().map(|stage| stage.archive));
    let mut claimed = ClaimedHold {
      image: None,
      blocks: BTreeMap::new(),
    };
    for block in named {
      // Every stored thing has a block of its own: a block named twice is an image no hold could have written.
      if claimed.blocks.contains_key(&(block.region, block.offset)) {
        claimed.give_back(arena);
        return Err(HoldImageError::Unclaimable);
      }
      let extent = (block.used <= block.block_len)
        .then(|| {
          let offset = usize::try_from(block.offset).ok()?;
          let len = usize::try_from(block.block_len).ok()?;
          arena.claim(block.region, offset, len).ok()
        })
        .flatten();
      let Some(extent) = extent else {
        claimed.give_back(arena);
        return Err(HoldImageError::Unclaimable);
      };
      claimed.blocks.insert((block.region, block.offset), extent);
    }
    claimed.image = Some(image);
    Ok(claimed)
  }

  /// The second half (AUD-29-59, A-64): every manifest held again through [`hold`](Self::hold) and every stage
  /// through the transfer's own path, so each chunk is re-verified against its identity, re-owned per object and
  /// re-charged exactly as a put would leave it — an image is recovered, never trusted — with the bytes read where
  /// they lie and each block adopted rather than copied. A claimed block nothing adopts goes back. A refusal
  /// part-way gives back everything the partial rebuild took and every claim.
  pub fn from_claimed(
    space: &mut HoldSpace<'_>,
    claimed: ClaimedHold,
  ) -> Result<ContentHold, HoldImageError> {
    let mut hold = ContentHold::new();
    let ClaimedHold { image, blocks } = claimed;
    let Some(image) = image else {
      return Ok(hold);
    };
    let read = |space: &HoldSpace<'_>, block: &BlockRef| -> Option<Vec<u8>> {
      let extent = blocks.get(&(block.region, block.offset))?;
      let used = usize::try_from(block.used).ok()?;
      space.arena.bytes(*extent)?.get(..used).map(<[u8]>::to_vec)
    };
    let named = |block: &BlockRef| blocks.get(&(block.region, block.offset)).copied();
    for chunk in &image.chunks {
      if let Some(extent) = named(&chunk.payload) {
        hold.adoptable.insert(Adopt::Chunk(chunk.identity), extent);
      }
    }
    let manifests = image.manifests.iter().map(|m| (m.object, &m.archive));
    let stages = image.stages.iter().map(|s| (s.object, &s.archive));
    let mut kept: BTreeSet<(u16, usize)> = hold
      .adoptable
      .values()
      .map(|extent| (extent.region(), extent.offset()))
      .collect();
    for (object, block) in manifests.chain(stages) {
      let identity = read(space, block)
        .and_then(|bytes| Archive::decode(&bytes).ok())
        .map(|archive| archive.manifest_identity());
      if let (Some(identity), Some(extent)) = (identity, named(block)) {
        hold
          .adoptable
          .insert(Adopt::Manifest(object, identity), extent);
        kept.insert((extent.region(), extent.offset()));
      }
    }
    // A claimed block the image names for nothing that decodes goes back now.
    for extent in blocks.values() {
      if !kept.contains(&(extent.region(), extent.offset())) {
        let _ = space.arena.free(*extent);
      }
    }
    let mut encodings: BTreeMap<[u8; 32], &ChunkImage> = BTreeMap::new();
    for chunk in &image.chunks {
      if Encoding::from_wire(chunk.encoding).is_none() {
        hold.abandon(space);
        return Err(HoldImageError::UnknownEncoding);
      }
      encodings.insert(chunk.identity, chunk);
    }
    let chunk_of = |space: &HoldSpace<'_>, identity: &[u8; 32]| -> Option<Chunk> {
      let image = encodings.get(identity)?;
      Some(Chunk {
        identity: image.identity,
        raw_len: image.raw_len,
        stored_len: image.stored_len,
        encoding: Encoding::from_wire(image.encoding)?,
        level: image.level,
        dictionary: image.dictionary,
        payload: read(space, &image.payload)?,
      })
    };
    for manifest in &image.manifests {
      let rebuilt = read(space, &manifest.archive)
        .ok_or(HoldImageError::Unclaimable)
        .and_then(|bytes| Archive::decode(&bytes).map_err(HoldImageError::Archive))
        .and_then(|mut archive| {
          archive.chunks = archive
            .referenced_chunks()
            .iter()
            .filter_map(|identity| chunk_of(space, identity))
            .collect();
          let placed = Placed {
            sequence: manifest.sequence,
          };
          hold
            .hold(space, ObjectId(manifest.object), placed, archive)
            .map_err(HoldImageError::Refused)
        });
      if let Err(refused) = rebuilt {
        hold.abandon(space);
        return Err(refused);
      }
    }
    for stage in &image.stages {
      let restaged = read(space, &stage.archive)
        .ok_or(HoldImageError::Unclaimable)
        .and_then(|archive| {
          let staged = stage
            .staged
            .iter()
            .map(|identity| chunk_of(space, identity).ok_or(*identity))
            .collect::<Vec<_>>();
          hold.restage(space, &archive, stage, staged)
        });
      if let Err(refused) = restaged {
        hold.abandon(space);
        return Err(refused);
      }
    }
    hold.release_unadopted(space);
    Ok(hold)
  }

  /// Gives back the claimed blocks nothing adopted (A-64): deferred while the committed image names them.
  fn release_unadopted(&mut self, space: &mut HoldSpace<'_>) {
    for (_, extent) in std::mem::take(&mut self.adoptable) {
      let _ = space.arena.free(extent);
    }
  }

  /// A refused rebuild: everything it took goes back, and the claimed blocks nothing adopted.
  fn abandon(&mut self, space: &mut HoldSpace<'_>) {
    self.forget_all(space);
    self.release_unadopted(space);
  }

  /// Rebuilds one imaged stage through the transfer's own path: the manifest staged again, then each chunk it
  /// had verified, verified again from its block (`staged`: the chunk, or the identity of one the image lacks).
  fn restage(
    &mut self,
    space: &mut HoldSpace<'_>,
    archive: &[u8],
    stage: &StageImage,
    staged: Vec<Result<Chunk, [u8; 32]>>,
  ) -> Result<(), HoldImageError> {
    let manifest = Archive::decode(archive).map_err(HoldImageError::Archive)?;
    let object = ObjectId(stage.object);
    let identity = manifest.manifest_identity();
    let placed = Placed {
      sequence: stage.sequence,
    };
    self
      .open_stage(space, object, placed, &manifest)
      .map_err(HoldImageError::Refused)?;
    for chunk in staged {
      let chunk =
        chunk.map_err(|_| HoldImageError::Refused(ContentRefusal::Incomplete { missing: 1 }))?;
      self
        .stage_chunk(space, object, &identity, chunk)
        .map_err(HoldImageError::Refused)?;
    }
    Ok(())
  }

  /// Serves one content request as `holder` in `space`, once `authorized` has allowed the access it asks for
  /// the object it names (checked before any lookup or allocation):
  /// - an offer, once `admits` accepts its object, sequence and manifest (the holder's retention rule, so a
  ///   placement its accepted records already supersede is never staged, AUD-29-43), opens or resumes the
  ///   object's stage and is answered with what the stage lacks, or with a bound acknowledgement when nothing
  ///   is missing;
  /// - a chunk is verified and kept in its stage and answered with the progress, or — the chunk that
  ///   completes the closure — with the bound acknowledgement;
  /// - a fetch is answered with the object's archive.
  ///
  /// Anything refused, malformed, unverifiable, past the holder's capacity, unstaged or unheld is answered
  /// with the same empty reply. Returns the reply and, for content that was completed and held, its object,
  /// so the caller applies its retention rule and publishes before the acknowledgement leaves.
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
      ContentMessage::Offer { object, .. } | ContentMessage::Chunk { object, .. } => {
        Some((ContentAccess::Place, *object))
      }
      ContentMessage::Fetch { object, .. } | ContentMessage::FetchChunk { object, .. } => {
        Some((ContentAccess::Read, *object))
      }
      _ => None,
    };
    let Some((access, object)) = asked else {
      return (Vec::new(), None);
    };
    if !authorized(access, object) {
      self.unauthorized = self.unauthorized.saturating_add(1);
      return (Vec::new(), None);
    }
    match message {
      ContentMessage::Offer {
        object,
        sequence,
        archive,
      } => self.serve_offer(space, holder, (object, sequence), &archive, admits),
      ContentMessage::Chunk {
        object,
        sequence,
        manifest,
        chunk,
      } => self.serve_chunk(space, holder, (object, sequence), manifest, chunk),
      ContentMessage::Fetch { object, manifest } => {
        let reply = match self.manifest_bytes(space.arena, object, &manifest) {
          Some(archive) => ContentMessage::Have { archive }.encode(),
          None => Vec::new(),
        };
        (reply, None)
      }
      ContentMessage::FetchChunk {
        object,
        manifest,
        chunk,
      } => {
        let reply = match self.piece_of(space.arena, object, &manifest, &chunk) {
          Some(chunk) => ContentMessage::Piece {
            object,
            manifest,
            chunk,
          }
          .encode(),
          None => Vec::new(),
        };
        (reply, None)
      }
      _ => (Vec::new(), None),
    }
  }

  /// An offer: the manifest staged (or its stage resumed) and the missing set, or the acknowledgement when
  /// the stage is already complete.
  fn serve_offer(
    &mut self,
    space: &mut HoldSpace<'_>,
    holder: HostId,
    (object, sequence): (ObjectId, u64),
    archive: &[u8],
    admits: impl FnOnce(ObjectId, u64, &[u8; 32]) -> bool,
  ) -> (Vec<u8>, Option<ObjectId>) {
    let Ok(manifest) = Archive::decode(archive) else {
      return (Vec::new(), None);
    };
    if !manifest.chunks.is_empty() {
      return (Vec::new(), None); // An offer carries the manifest only; chunks travel one per exchange.
    }
    let identity = manifest.manifest_identity();
    if !admits(object, sequence, &identity) {
      self.superseded = self.superseded.saturating_add(1);
      return (Vec::new(), None);
    }
    let missing = match self.open_stage(space, object, Placed { sequence }, &manifest) {
      Ok((Opened::Held, _)) => return acknowledged(holder, object, sequence, identity),
      Ok((Opened::Missing(missing), _)) => missing,
      Err(refusal) => {
        self.count_refusal(&refusal);
        return (Vec::new(), None);
      }
    };
    if missing.is_empty() {
      return self.complete(space, holder, object, sequence);
    }
    let reply = ContentMessage::Missing {
      object,
      sequence,
      manifest: identity,
      missing,
    }
    .encode();
    (reply, None)
  }

  /// A chunk: kept in its stage and answered with the progress, or the acknowledgement once it completes it.
  fn serve_chunk(
    &mut self,
    space: &mut HoldSpace<'_>,
    holder: HostId,
    (object, sequence): (ObjectId, u64),
    manifest: [u8; 32],
    chunk: Chunk,
  ) -> (Vec<u8>, Option<ObjectId>) {
    let staged = |remaining: u64| {
      (
        ContentMessage::Staged {
          object,
          sequence,
          manifest,
          remaining,
        }
        .encode(),
        None,
      )
    };
    match self.stage_chunk(space, object, &manifest, chunk) {
      Ok(0) => match self.promote(space, object) {
        Ok(held) => acknowledged(holder, object, sequence, held),
        // A chunk held for the object when the stage opened was released while it filled: the stage stays,
        // recomputed, and the reply is its progress — the next offer names what is owed.
        Err(ContentRefusal::Incomplete { missing }) => staged(as_count(missing)),
        Err(refusal) => {
          self.count_refusal(&refusal);
          (Vec::new(), None)
        }
      },
      Ok(remaining) => staged(remaining),
      Err(refusal) => {
        self.count_refusal(&refusal);
        (Vec::new(), None)
      }
    }
  }

  /// Promotes `object`'s complete stage and answers the bound acknowledgement, or the empty reply.
  fn complete(
    &mut self,
    space: &mut HoldSpace<'_>,
    holder: HostId,
    object: ObjectId,
    sequence: u64,
  ) -> (Vec<u8>, Option<ObjectId>) {
    match self.promote(space, object) {
      Ok(manifest) => acknowledged(holder, object, sequence, manifest),
      Err(refusal) => {
        self.count_refusal(&refusal);
        (Vec::new(), None)
      }
    }
  }

  /// Counts a refusal the status report names: a capacity refusal, or a stale placement.
  fn count_refusal(&mut self, refusal: &ContentRefusal) {
    match refusal {
      ContentRefusal::NoCapacity { .. } => {
        self.refused_capacity = self.refused_capacity.saturating_add(1);
      }
      ContentRefusal::StaleStage => self.superseded = self.superseded.saturating_add(1),
      _ => {}
    }
  }
}

/// The bound acknowledgement of `manifest` held for `object` at `sequence`, and the object it completed.
fn acknowledged(
  holder: HostId,
  object: ObjectId,
  sequence: u64,
  manifest: [u8; 32],
) -> (Vec<u8>, Option<ObjectId>) {
  (
    ContentMessage::Ack(ContentAck {
      holder,
      object,
      sequence,
      manifest,
    })
    .encode(),
    Some(object),
  )
}

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

/// The offer of `archive`'s content for `object` at `sequence`: its manifest with no chunks (AUD-29-55).
pub fn offer_request(archive: &Archive, object: ObjectId, sequence: u64) -> Vec<u8> {
  ContentMessage::Offer {
    object,
    sequence,
    archive: with_chunks(archive, Vec::new()).encode(),
  }
  .encode()
}

/// The chunk requests a holder's `missing` set calls for: one `Chunk` exchange per missing chunk the archive
/// holds, each a copy of exactly the bytes that must cross the wire to that holder.
pub fn chunk_requests(
  archive: &Archive,
  object: ObjectId,
  sequence: u64,
  missing: &[[u8; 32]],
) -> Vec<Vec<u8>> {
  let manifest = archive.manifest_identity();
  let wanted: BTreeSet<[u8; 32]> = missing.iter().copied().collect();
  chunks_for(archive, &wanted)
    .into_iter()
    .map(|chunk| {
      ContentMessage::Chunk {
        object,
        sequence,
        manifest,
        chunk,
      }
      .encode()
    })
    .collect()
}

/// What a holder's reply to an offer says, for this placement.
enum Answer {
  /// The holder already holds the content whole: a bound acknowledgement.
  Ack,
  /// The holder staged the manifest and lacks these chunks.
  Missing(Vec<[u8; 32]>),
  /// A refusal, a timeout, or a reply for another placement.
  Nothing,
}

/// Reads `host`'s reply to the offer of `manifest` for `object` at `sequence`.
fn answer_of(reply: &[u8], host: HostId, binding: (ObjectId, u64, &[u8; 32])) -> Answer {
  let (object, sequence, manifest) = binding;
  match ContentMessage::decode(reply) {
    Ok(ContentMessage::Ack(ack)) if ack.holder == host && ack.binds(object, sequence, manifest) => {
      Answer::Ack
    }
    Ok(ContentMessage::Missing {
      object: offered_object,
      sequence: offered_sequence,
      manifest: offered_manifest,
      missing,
    }) if offered_object == object
      && offered_sequence == sequence
      && offered_manifest == *manifest =>
    {
      Answer::Missing(missing)
    }
    _ => Answer::Nothing,
  }
}

/// Whether `reply` is `host`'s bound acknowledgement of `manifest` for `object` at `sequence`.
fn acknowledges(reply: &[u8], host: HostId, binding: (ObjectId, u64, &[u8; 32])) -> bool {
  let (object, sequence, manifest) = binding;
  matches!(
    ContentMessage::decode(reply),
    Ok(ContentMessage::Ack(ack)) if ack.holder == host && ack.binds(object, sequence, manifest)
  )
}

/// Sends `requests` — one holder's chunk exchanges — over `endpoint`, as many in flight as the peer's stream
/// credit admits (`Endpoint::begin` refuses `Backlogged` past it; the sender then drives the session until
/// replies free slots), and returns the holder's bound acknowledgement once the chunk completing its stage
/// draws it. A refusal (an empty reply), a reply for another placement, a transport error, or every chunk
/// answered without an acknowledgement ends the transfer with nothing; the stage keeps what was verified.
async fn drive_chunks(
  endpoint: &mut Endpoint,
  requests: Vec<Vec<u8>>,
  host: HostId,
  binding: (ObjectId, u64, [u8; 32]),
) -> Option<Vec<u8>> {
  let (object, sequence, manifest) = binding;
  let mut pending: std::collections::VecDeque<Vec<u8>> = requests.into();
  let mut open: Vec<u64> = Vec::new();
  loop {
    while let Some(next) = pending.front() {
      match endpoint.begin(CONTENT_CHUNK_STREAM, Priority::Bulk, next) {
        Ok(id) => {
          open.push(id);
          pending.pop_front();
        }
        Err(EndpointError::Stream(StreamRefusal::Backlogged { .. })) if !open.is_empty() => break,
        Err(_) => return None,
      }
    }
    if open.is_empty() {
      return None;
    }
    endpoint.drive().await.ok()?;
    let mut still_open = Vec::with_capacity(open.len());
    for id in open {
      let Some(reply) = endpoint.take_reply(id) else {
        still_open.push(id);
        continue;
      };
      match ContentMessage::decode(&reply) {
        Ok(ContentMessage::Ack(ack))
          if ack.holder == host && ack.binds(object, sequence, &manifest) =>
        {
          return Some(reply);
        }
        Ok(ContentMessage::Staged {
          object: staged_object,
          sequence: staged_sequence,
          manifest: staged_manifest,
          ..
        }) if staged_object == object
          && staged_sequence == sequence
          && staged_manifest == manifest => {}
        _ => return None,
      }
    }
    open = still_open;
  }
}

/// One holder's chunk transfer, bounded by `deadline_ns`: [`drive_chunks`], then every exchange still open
/// abandoned (the session is this task's alone), so the endpoint goes back clean whatever the outcome. The
/// reply is the acknowledgement, or empty.
async fn transfer_chunks(
  mut endpoint: Endpoint,
  requests: Vec<Vec<u8>>,
  host: HostId,
  binding: (ObjectId, u64, [u8; 32]),
  deadline_ns: u64,
) -> (TimedReply, Endpoint) {
  let sent_ns = now_ns();
  let acknowledgement = slates_rt::futures::within(
    deadline_ns,
    drive_chunks(&mut endpoint, requests, host, binding),
  )
  .await
  .ok()
  .flatten()
  .flatten();
  endpoint.abandon_all();
  let round_trip_ns = acknowledgement
    .is_some()
    .then(|| now_ns().saturating_sub(sent_ns));
  (
    TimedReply {
      bytes: acknowledgement.unwrap_or_default(),
      round_trip_ns,
    },
    endpoint,
  )
}

/// One placement round on the owner (AUD-29-55, AUD-29-58): what it places and when it started, and what its
/// collection loop has gathered so far.
struct Round<'a> {
  archive: &'a Archive,
  binding: (ObjectId, u64, [u8; 32]),
  shape: Placement,
  started_ns: u64,
  deadline_ns: u64,
  transfer_tx: std::sync::mpsc::Sender<Reply>,
  acked: Vec<HostId>,
  reusable: Vec<(HostId, Endpoint)>,
  latencies_ns: Vec<(HostId, u64)>,
  refilled: Vec<HostId>,
  transfers_started: usize,
  transfers_reported: usize,
}

impl Round<'_> {
  /// Counts `host` as acknowledging, once, if it is a candidate, timing it from the round's start.
  fn acknowledge(&mut self, host: HostId) {
    if self.shape.candidates.contains(&host) && !self.acked.contains(&host) {
      self.acked.push(host);
      self
        .latencies_ns
        .push((host, now_ns().saturating_sub(self.started_ns)));
    }
  }

  /// A holder's offer reply: an acknowledgement counts at once; a missing set starts that holder's chunk
  /// transfer now, bounded by a span of its own (the put's clock starts when its chunks go out, as the record
  /// commit's does when its records go out); anything else hands the session back. Returns whether a transfer
  /// started.
  fn on_offer(&mut self, Reply(host, reply, endpoint): Reply) -> bool {
    let (object, sequence, manifest) = self.binding;
    match answer_of(&reply.bytes, host, (object, sequence, &manifest)) {
      Answer::Ack => {
        self.acknowledge(host);
        self.reusable.push((host, *endpoint));
        false
      }
      Answer::Missing(missing) => {
        if !missing.is_empty() {
          self.refilled.push(host);
        }
        let requests = chunk_requests(self.archive, object, sequence, &missing);
        let (tx, binding, span_ns) = (self.transfer_tx.clone(), self.binding, self.deadline_ns);
        let spawned = spawn_child(async move {
          let (reply, endpoint) =
            transfer_chunks(*endpoint, requests, host, binding, span_ns).await;
          let _ = tx.send(Reply(host, reply, Box::new(endpoint)));
        });
        let Ok(task) = spawned else {
          return false;
        };
        let _ = detach(task);
        self.transfers_started = self.transfers_started.saturating_add(1);
        true
      }
      Answer::Nothing => {
        self.reusable.push((host, *endpoint));
        false
      }
    }
  }

  /// A holder's finished chunk transfer: its acknowledgement counts; its session comes back.
  fn on_transfer(&mut self, Reply(host, reply, endpoint): Reply) {
    let (object, sequence, manifest) = self.binding;
    self.transfers_reported = self.transfers_reported.saturating_add(1);
    if acknowledges(&reply.bytes, host, (object, sequence, &manifest)) {
      self.acknowledge(host);
    }
    self.reusable.push((host, *endpoint));
  }

  /// Collects offer replies and finished transfers until the placement is placed, every holder has answered,
  /// or the `budget`'s progress-extension policy gives up; returns whether it gave up (timed out). The policy's
  /// clock starts with the round and starts again with each transfer, so a transfer begun late in the offer
  /// span still has the whole span the separate put round had: the round is bounded by the offer span plus
  /// one transfer span. The channels are owned across the waits (a borrowed receiver is not `Send`) and
  /// handed back for the stragglers.
  async fn collect(
    &mut self,
    (offers, transfers): (Receiver<Reply>, Receiver<Reply>),
    quorum: Quorum,
    budget: CommitBudget,
  ) -> (bool, Receiver<Reply>, Receiver<Reply>) {
    let mut wait = DispatchWait::new(budget, self.started_ns);
    let mut offers_open = true;
    while !self.shape.placed_with(&self.acked, quorum) {
      let mut progressed = false;
      match offers.try_recv() {
        Ok(reply) => {
          progressed = true;
          if self.on_offer(reply) {
            wait = DispatchWait::new(budget, now_ns());
          }
        }
        Err(TryRecvError::Empty) => {}
        Err(TryRecvError::Disconnected) => offers_open = false,
      }
      if let Ok(reply) = transfers.try_recv() {
        progressed = true;
        self.on_transfer(reply);
      }
      if progressed {
        continue;
      }
      if !offers_open && self.transfers_reported >= self.transfers_started {
        return (false, offers, transfers); // every holder has answered: the outcome is settled
      }
      if !wait.keep_waiting(self.acked.len()).await {
        return (true, offers, transfers);
      }
    }
    (false, offers, transfers)
  }
}

/// Puts `archive` — the content of `object`'s head at `sequence` — to the `remote_holders` (its candidate
/// holders, each over a connected session) at `quorum`. Every holder progresses on its own (AUD-29-58): its
/// offer goes out at once, and the moment its missing set arrives its chunk transfer starts, so a slow offer
/// never holds back a fast holder. Bound acknowledgements are collected as they arrive, to `f + 1` distinct
/// candidates under `budget`'s progress-extension policy; each acknowledgement's latency is timed from the
/// round's start, offer included (the whole placement the hedge trigger is sized for). The `owner` holds its
/// own content, so it counts from the start when it is a candidate; at `f = 0` that is the placement and
/// nothing is dispatched (R8). Returns the [`Placement`] on a quorum, [`ClusterError::Uncertain`] at the
/// deadline, [`ClusterError::NotPlaced`] when every holder answered short of it — with the sessions that came
/// back and the stragglers still in flight (an offer whose transfer never started returns its session there;
/// the holder's stage keeps what arrived, so the next round resumes it).
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
  // Content places on its one current cohort (§4.10): it is fetched by identity from the holders the head
  // names, so a neighbourhood change in flight never needs it joined.
  let shape = Placement::of(candidates);
  let owner_holds: Vec<HostId> = candidates
    .iter()
    .copied()
    .filter(|host| *host == owner)
    .collect();
  if shape.placed_with(&owner_holds, quorum) {
    return ContentPlaced {
      outcome: Ok(Placement {
        acked: owner_holds,
        ..shape
      }),
      reusable: Vec::new(),
      stragglers: Stragglers::none(),
      latencies_ns: Vec::new(),
      refilled: Vec::new(),
    };
  }
  let deadline_ns = budget.max_deadline_ns();
  let started_ns = now_ns();
  let offer = offer_request(archive, object, sequence);
  let offers = match dispatch_round(
    remote_holders
      .into_iter()
      .map(|(host, endpoint)| (host, endpoint, offer.clone()))
      .collect(),
    CONTENT_OFFER_STREAM,
    deadline_ns,
  ) {
    Ok(rx) => rx,
    Err((error, rx)) => {
      // Keep the channel of the tasks already admitted: their bounded replies return sessions.
      return ContentPlaced {
        outcome: Err(ClusterError::Runtime(error)),
        reusable: Vec::new(),
        stragglers: Stragglers::pending(rx),
        latencies_ns: Vec::new(),
        refilled: Vec::new(),
      };
    }
  };
  let (transfer_tx, transfers) = channel::<Reply>();
  let mut round = Round {
    archive,
    binding: (object, sequence, archive.manifest_identity()),
    shape,
    started_ns,
    deadline_ns,
    transfer_tx,
    acked: owner_holds,
    reusable: Vec::new(),
    latencies_ns: Vec::new(),
    refilled: Vec::new(),
    transfers_started: 0,
    transfers_reported: 0,
  };
  let (timed_out, offers, transfers) = round.collect((offers, transfers), quorum, budget).await;
  let Round {
    shape,
    transfer_tx,
    acked,
    mut reusable,
    latencies_ns,
    refilled,
    ..
  } = round;
  drop(transfer_tx); // so the stragglers' channel disconnects once every transfer has ended
  // Recover the sessions of tasks that already finished, so they too are reused.
  while let Ok(Reply(host, _, endpoint)) = transfers.try_recv() {
    reusable.push((host, *endpoint));
  }
  let placement = Placement { acked, ..shape };
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
    stragglers: Stragglers::two_rounds(offers, transfers),
    latencies_ns,
    refilled,
  }
}

/// The timing of one fetch (§4.10 "chunk reads fault to hedged fetches by identity from the recorded holders"),
/// every value the caller's to derive.
#[derive(Clone, Copy, Debug)]
pub struct FetchTiming {
  /// The whole fetch's span: what is unanswered by then is left to the next fetch, whose stage keeps every chunk
  /// that arrived verified (AUD-29-55).
  pub deadline_ns: u64,
  /// How long a request is outstanding at the holders asked so far before it is also asked of the next-ranked
  /// holder: the measured p95 of the fetch class (Dean & Barroso, CACM 2013: "send the request to a replica …
  /// after the request has been outstanding for longer than the 95th-percentile expected latency").
  pub hedge_after_ns: u64,
  /// How long a holder's worker drives its session before it looks for new work, and the coordinator between
  /// looks: the latency a hedge, a cancellation or a stop waits at most before its worker sees it.
  pub poll_ns: u64,
}

/// What a fetch did: whether the manifest was found and every wanted chunk kept, every holder's session back
/// (whatever the outcome), each kept chunk's latency from its first request (the fetch class's readings for the
/// hedge trigger), and how many requests were hedges, steals and cancellations, and how many holders failed.
pub struct Fetched {
  /// Whether a holder answered the manifest (verified against its identity).
  pub manifest_found: bool,
  /// Whether the manifest was found and every chunk the stage wanted was kept.
  pub complete: bool,
  /// Every holder's session, returned whatever the outcome.
  pub sessions: Vec<(HostId, Endpoint)>,
  /// Each kept chunk's latency, from the chunk's first request to its verified arrival.
  pub latencies_ns: Vec<u64>,
  /// Requests sent as hedges (asked of a further holder after the hedge delay).
  pub hedges: u64,
  /// Requests an idle holder took from another's outstanding work (work stealing).
  pub steals: u64,
  /// Requests cancelled because another holder answered first (tied requests).
  pub cancelled: u64,
  /// Holders whose session failed or that answered falsely, their work moved to the others.
  pub failed_holders: u64,
}

/// The rank of `host` for the item `identity` (a chunk or the manifest): the higher, the earlier the holder is
/// asked. Identities are uniform hashes, so a holder's rank across chunks is pseudo-random and a fetch stripes its
/// chunks across the recorded holders; the MurmurHash3 `fmix64` finalizer (as the register's rendezvous uses)
/// avalanches the host's id into it.
fn chunk_rank(identity: &[u8; 32], host: HostId) -> u64 {
  /// Format: MurmurHash3 `fmix64` first multiplier (Austin Appleby, public domain).
  const FMIX_A: u64 = 0xff51_afd7_ed55_8ccd;
  /// Format: MurmurHash3 `fmix64` second multiplier (Austin Appleby, public domain).
  const FMIX_B: u64 = 0xc4ce_b9fe_1a85_ec53;
  /// Format: MurmurHash3 `fmix64` shift distance (Austin Appleby, public domain).
  const FMIX_SHIFT: u32 = 33;
  let mut prefix = [0u8; size_of::<u64>()];
  if let Some(head) = identity.get(..size_of::<u64>()) {
    prefix.copy_from_slice(head);
  }
  let mut hash = u64::from_le_bytes(prefix) ^ host.0;
  hash ^= hash >> FMIX_SHIFT;
  hash = hash.wrapping_mul(FMIX_A);
  hash ^= hash >> FMIX_SHIFT;
  hash = hash.wrapping_mul(FMIX_B);
  hash ^= hash >> FMIX_SHIFT;
  hash
}

/// What a fetch item is: the manifest, or one chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Item {
  Manifest,
  Chunk([u8; 32]),
}

/// Work for one holder's fetch worker.
enum FetchCommand {
  /// Ask this holder for the item.
  Ask(Item),
  /// Another holder answered the item first: abandon this holder's request for it.
  Cancel(Item),
  /// Abandon what is open and hand the session back.
  Stop,
}

/// What a fetch worker reports, by the holder's index in the fetch.
enum FetchEvent {
  /// The holder answered the manifest, verified against its identity.
  Manifest(usize, Archive),
  /// The holder answered the chunk asked for, for this manifest (not yet verified: `keep` does that).
  Piece(usize, [u8; 32], Chunk),
  /// The holder answered with anything but the item asked, or its session failed: it is dropped.
  Failed(usize),
  /// The worker stopped and hands its session back.
  Returned(usize, Box<Endpoint>),
}

/// One holder's fetch worker's own state.
struct FetchWorker {
  index: usize,
  endpoint: Endpoint,
  binding: (ObjectId, [u8; 32]),
  pending: std::collections::VecDeque<Item>,
  open: Vec<(u64, Item)>,
  failed: bool,
}

impl FetchWorker {
  /// Takes the coordinator's new work; `false` when it said stop (or is gone).
  fn take_commands(&mut self, commands: &Receiver<FetchCommand>) -> bool {
    loop {
      match commands.try_recv() {
        Ok(FetchCommand::Ask(item)) => {
          if !self.failed {
            self.pending.push_back(item);
          }
        }
        Ok(FetchCommand::Cancel(item)) => self.cancel(item),
        Ok(FetchCommand::Stop) | Err(TryRecvError::Disconnected) => return false,
        Err(TryRecvError::Empty) => return true,
      }
    }
  }

  /// Drops this holder's request for `item`: still queued, it never goes out; open, its exchange is abandoned so
  /// the path stops carrying it.
  fn cancel(&mut self, item: Item) {
    self.pending.retain(|queued| *queued != item);
    let endpoint = &mut self.endpoint;
    self.open.retain(|(id, open)| {
      if *open == item {
        endpoint.abandon(*id);
        false
      } else {
        true
      }
    });
  }

  /// Begins the pending requests while the peer's stream credit admits them.
  fn begin_pending(&mut self) {
    let (object, manifest) = self.binding;
    while let Some(next) = self.pending.front().copied() {
      let request = match next {
        Item::Manifest => ContentMessage::Fetch { object, manifest }.encode(),
        Item::Chunk(chunk) => ContentMessage::FetchChunk {
          object,
          manifest,
          chunk,
        }
        .encode(),
      };
      match self
        .endpoint
        .begin(CONTENT_FETCH_STREAM, Priority::Bulk, &request)
      {
        Ok(id) => {
          self.open.push((id, next));
          self.pending.pop_front();
        }
        Err(EndpointError::Stream(StreamRefusal::Backlogged { .. })) if !self.open.is_empty() => {
          return;
        }
        Err(_) => {
          self.failed = true;
          return;
        }
      }
    }
  }

  /// Reports every answered request: the manifest or a piece for the item asked, or the holder failed.
  fn collect_replies(&mut self, events: &std::sync::mpsc::Sender<FetchEvent>) {
    let open = std::mem::take(&mut self.open);
    for (id, asked) in open {
      let Some(reply) = self.endpoint.take_reply(id) else {
        self.open.push((id, asked));
        continue;
      };
      match self.answer(asked, &reply) {
        Some(event) => {
          let _ = events.send(event);
        }
        None => self.failed = true,
      }
    }
  }

  /// The event `reply` makes for the item `asked`, or `None` when it answers anything else.
  fn answer(&self, asked: Item, reply: &[u8]) -> Option<FetchEvent> {
    let (object, manifest) = self.binding;
    match (asked, ContentMessage::decode(reply).ok()?) {
      (Item::Manifest, ContentMessage::Have { archive }) => Archive::decode(&archive)
        .ok()
        .filter(|archive| archive.chunks.is_empty() && archive.manifest_identity() == manifest)
        .map(|archive| FetchEvent::Manifest(self.index, archive)),
      (
        Item::Chunk(asked),
        ContentMessage::Piece {
          object: answered_object,
          manifest: answered_manifest,
          chunk,
        },
      ) if answered_object == object
        && answered_manifest == manifest
        && chunk.identity == asked =>
      {
        Some(FetchEvent::Piece(self.index, asked, chunk))
      }
      _ => None,
    }
  }
}

/// One holder's side of a fetch: asks for each item the coordinator sends, as many in flight as the peer's stream
/// credit admits, drives the session in slices of `poll_ns` so new work, a cancellation and a stop are seen within
/// one, and reports every answer. A failed holder is reported once; the worker then only waits for the stop.
async fn fetch_worker(
  mut worker: FetchWorker,
  commands: Receiver<FetchCommand>,
  events: std::sync::mpsc::Sender<FetchEvent>,
  poll_ns: u64,
) {
  let mut reported = false;
  while worker.take_commands(&commands) {
    if worker.failed {
      if slates_rt::futures::sleep(poll_ns).await.is_err() {
        break;
      }
      continue;
    }
    worker.begin_pending();
    if !worker.failed
      && !matches!(
        slates_rt::futures::within(poll_ns, worker.endpoint.drive()).await,
        Ok(None | Some(Ok(())))
      )
    {
      worker.failed = true;
    }
    worker.collect_replies(&events);
    if worker.failed && !reported {
      reported = true;
      worker.open.clear();
      worker.pending.clear();
      let _ = events.send(FetchEvent::Failed(worker.index));
    }
  }
  worker.endpoint.abandon_all();
  let _ = events.send(FetchEvent::Returned(
    worker.index,
    Box::new(worker.endpoint),
  ));
}

/// One wanted item's progress in a fetch.
struct Wanted {
  item: Item,
  /// The holders' indices in rank order for this item, highest first.
  ranked: Vec<usize>,
  /// The holders asked for it so far.
  asked_of: Vec<usize>,
  /// When the item was first asked for, and when it was last asked of a further holder.
  first_asked_ns: u64,
  last_asked_ns: u64,
  done: bool,
}

impl Wanted {
  fn new(item: Item, hosts: &[HostId], identity: &[u8; 32], now: u64) -> Wanted {
    let mut ranked: Vec<usize> = (0..hosts.len()).collect();
    ranked.sort_by_key(|&index| {
      std::cmp::Reverse(
        hosts
          .get(index)
          .map_or(0, |host| chunk_rank(identity, *host)),
      )
    });
    Wanted {
      item,
      ranked,
      asked_of: Vec::new(),
      first_asked_ns: now,
      last_asked_ns: now,
      done: false,
    }
  }

  /// Whether a live holder is still working on it.
  fn outstanding_at_a_live_holder(&self, live: &[bool]) -> bool {
    self
      .asked_of
      .iter()
      .any(|holder| live.get(*holder).copied().unwrap_or(false))
  }
}

/// The coordinator of one fetch: each holder's worker, which holders are live and how much each has outstanding, and
/// every wanted item (the manifest first, then the chunks the stage wants).
struct FetchRun {
  hosts: Vec<HostId>,
  commands: Vec<Option<std::sync::mpsc::Sender<FetchCommand>>>,
  live: Vec<bool>,
  /// Items asked of each holder and not yet answered or cancelled.
  outstanding: Vec<usize>,
  /// How many items each holder may have outstanding: its first share of the chunks (at least one), the depth it
  /// was trusted with at the start, so stealing never asks a holder for more than it was first given.
  window: Vec<usize>,
  running: usize,
  items: Vec<Wanted>,
  manifest: Option<Archive>,
  refused: bool,
  fetched: Fetched,
}

impl FetchRun {
  /// Spawns a worker per holder over its session, each reporting on `events`; a holder the runtime cannot admit a
  /// worker for keeps its session here and counts as failed.
  fn start(
    holders: Vec<(HostId, Endpoint)>,
    binding: (ObjectId, [u8; 32]),
    poll_ns: u64,
    events: &std::sync::mpsc::Sender<FetchEvent>,
  ) -> FetchRun {
    let hosts: Vec<HostId> = holders.iter().map(|(host, _)| *host).collect();
    let mut run = FetchRun {
      commands: Vec::with_capacity(hosts.len()),
      live: Vec::with_capacity(hosts.len()),
      outstanding: vec![0; hosts.len()],
      window: vec![1; hosts.len()],
      running: 0,
      items: Vec::new(),
      manifest: None,
      refused: false,
      fetched: Fetched {
        manifest_found: false,
        complete: false,
        sessions: Vec::new(),
        latencies_ns: Vec::new(),
        hedges: 0,
        steals: 0,
        cancelled: 0,
        failed_holders: 0,
      },
      hosts,
    };
    for (index, (host, endpoint)) in holders.into_iter().enumerate() {
      run.spawn_worker(index, (host, endpoint), binding, poll_ns, events);
    }
    run
  }

  /// One holder's worker; the session moves to it over a channel once the runtime admitted it, so a refused spawn
  /// leaves the session here, never dropped with the refused future.
  fn spawn_worker(
    &mut self,
    index: usize,
    (host, endpoint): (HostId, Endpoint),
    binding: (ObjectId, [u8; 32]),
    poll_ns: u64,
    events: &std::sync::mpsc::Sender<FetchEvent>,
  ) {
    let (command_tx, command_rx) = channel::<FetchCommand>();
    let worker_events = events.clone();
    let (park_tx, park_rx) = channel::<Endpoint>();
    let spawned = spawn_child(async move {
      loop {
        match park_rx.try_recv() {
          Ok(endpoint) => {
            let worker = FetchWorker {
              index,
              endpoint,
              binding,
              pending: std::collections::VecDeque::new(),
              open: Vec::new(),
              failed: false,
            };
            fetch_worker(worker, command_rx, worker_events, poll_ns).await;
            return;
          }
          Err(TryRecvError::Disconnected) => return,
          Err(TryRecvError::Empty) => slates_rt::futures::yield_now().await,
        }
      }
    });
    if let Ok(task) = spawned {
      let _ = park_tx.send(endpoint);
      let _ = detach(task);
      self.commands.push(Some(command_tx));
      self.live.push(true);
      self.running = self.running.saturating_add(1);
    } else {
      self.fetched.sessions.push((host, endpoint));
      self.commands.push(None);
      self.live.push(false);
      self.fetched.failed_holders = self.fetched.failed_holders.saturating_add(1);
    }
  }

  /// Asks item `at` of holder `holder`, at `now`: whether the request went out.
  fn ask(&mut self, at: usize, holder: usize, now: u64) -> bool {
    let Some(wanted) = self.items.get_mut(at) else {
      return false;
    };
    if wanted.done
      || wanted.asked_of.contains(&holder)
      || !self.live.get(holder).copied().unwrap_or(false)
    {
      return false;
    }
    let Some(Some(command)) = self.commands.get(holder) else {
      return false;
    };
    if command.send(FetchCommand::Ask(wanted.item)).is_err() {
      return false;
    }
    if wanted.asked_of.is_empty() {
      wanted.first_asked_ns = now;
    }
    wanted.asked_of.push(holder);
    wanted.last_asked_ns = now;
    if let Some(count) = self.outstanding.get_mut(holder) {
      *count = count.saturating_add(1);
    }
    true
  }

  /// Asks item `at` of its next-ranked live holder not yet asked: whether a request went out.
  fn ask_next(&mut self, at: usize, now: u64) -> bool {
    let ranked = self
      .items
      .get(at)
      .map(|wanted| wanted.ranked.clone())
      .unwrap_or_default();
    ranked.into_iter().any(|holder| self.ask(at, holder, now))
  }

  /// Item `at` was answered by `answering`: done, and every other holder still asked for it is told to cancel.
  fn finish(&mut self, at: usize, answering: usize) {
    let Some(wanted) = self.items.get_mut(at) else {
      return;
    };
    wanted.done = true;
    let item = wanted.item;

    for holder in wanted.asked_of.clone() {
      if let Some(count) = self.outstanding.get_mut(holder) {
        *count = count.saturating_sub(1);
      }
      if holder != answering
        && self.live.get(holder).copied().unwrap_or(false)
        && let Some(Some(command)) = self.commands.get(holder)
        && command.send(FetchCommand::Cancel(item)).is_ok()
      {
        self.fetched.cancelled = self.fetched.cancelled.saturating_add(1);
      }
    }
  }

  /// Folds one worker's report in at `now`.
  fn on_event(&mut self, event: FetchEvent, keep: &mut impl FnMut(Chunk) -> bool, now: u64) {
    match event {
      FetchEvent::Manifest(index, archive) => {
        if self.manifest.is_none() {
          self.manifest = Some(archive);
          self.fetched.manifest_found = true;
          if let Some(at) = self
            .items
            .iter()
            .position(|wanted| wanted.item == Item::Manifest)
          {
            self.finish(at, index);
          }
        }
      }
      FetchEvent::Piece(index, asked, piece) => {
        let Some(at) = self
          .items
          .iter()
          .position(|wanted| wanted.item == Item::Chunk(asked) && !wanted.done)
        else {
          return; // A tied request's loser: the chunk was already kept.
        };
        if keep(piece) {
          let first = self
            .items
            .get(at)
            .map_or(now, |wanted| wanted.first_asked_ns);
          self.fetched.latencies_ns.push(now.saturating_sub(first));
          self.finish(at, index);
        } else {
          self.refused = true;
        }
      }
      FetchEvent::Failed(index) => self.drop_holder(index, now),
      FetchEvent::Returned(index, endpoint) => self.take_back(index, *endpoint),
    }
  }

  /// Drops holder `index`: every item asked only of dropped holders moves to its next live holder at once.
  fn drop_holder(&mut self, index: usize, now: u64) {
    let Some(alive) = self.live.get_mut(index) else {
      return;
    };
    if !*alive {
      return;
    }
    *alive = false;
    self.fetched.failed_holders = self.fetched.failed_holders.saturating_add(1);
    for at in 0..self.items.len() {
      let orphaned = self
        .items
        .get(at)
        .is_some_and(|wanted| !wanted.done && !wanted.outstanding_at_a_live_holder(&self.live));
      if orphaned {
        self.ask_next(at, now);
      }
    }
  }

  /// The chunks only holder `holder` has been asked for and not yet answered: its exclusive backlog, the work that
  /// waits on it alone.
  fn exclusive_backlog(&self, holder: usize) -> impl Iterator<Item = usize> + '_ {
    self
      .items
      .iter()
      .enumerate()
      .filter_map(move |(at, wanted)| {
        let exclusive = matches!(wanted.item, Item::Chunk(_))
          && !wanted.done
          && wanted.asked_of.contains(&holder)
          && wanted
            .asked_of
            .iter()
            .all(|asked| *asked == holder || !self.live.get(*asked).copied().unwrap_or(false));
        exclusive.then_some(at)
      })
  }

  /// One scheduling step at `now`: every item outstanding past `hedge_after_ns` is hedged to its next live holder;
  /// then each holder with room in its window steals from the holder whose exclusive backlog is longest, while that
  /// backlog is longer than the stealer's own outstanding work — taking the deepest-queued chunk, the one least likely
  /// to be in service (work stealing from the tail, as a deque scheduler steals). Holders keeping pace stay balanced and
  /// duplicate nothing; a slow or silent holder's backlog drains to the others, so completion follows the holders'
  /// real rates even when one is slow throughout and the p95 has learned its latency as normal.
  fn schedule(&mut self, now: u64, hedge_after_ns: u64) {
    for at in 0..self.items.len() {
      let due = self.items.get(at).is_some_and(|wanted| {
        !wanted.done
          && !wanted.asked_of.is_empty()
          && now.saturating_sub(wanted.last_asked_ns) >= hedge_after_ns
      });
      if due && self.ask_next(at, now) {
        self.fetched.hedges = self.fetched.hedges.saturating_add(1);
      }
    }
    for stealer in 0..self.hosts.len() {
      while self.live.get(stealer).copied().unwrap_or(false)
        && self.outstanding.get(stealer).copied().unwrap_or(0)
          < self.window.get(stealer).copied().unwrap_or(0)
      {
        let mine = self.outstanding.get(stealer).copied().unwrap_or(0);
        let victim = (0..self.hosts.len())
          .filter(|holder| *holder != stealer && self.live.get(*holder).copied().unwrap_or(false))
          .map(|holder| (self.exclusive_backlog(holder).count(), holder))
          .filter(|(backlog, _)| *backlog > mine)
          .max();
        let Some((_, victim)) = victim else {
          break;
        };
        let Some(deepest) = self.exclusive_backlog(victim).last() else {
          break;
        };
        if !self.ask(deepest, stealer, now) {
          break;
        }
        self.fetched.steals = self.fetched.steals.saturating_add(1);
      }
    }
  }

  /// A worker's session, back.
  fn take_back(&mut self, index: usize, endpoint: Endpoint) {
    if let Some(host) = self.hosts.get(index) {
      self.fetched.sessions.push((*host, endpoint));
    }
    self.running = self.running.saturating_sub(1);
  }

  /// Whether every wanted item is done.
  fn all_done(&self) -> bool {
    self.items.iter().all(|wanted| wanted.done)
  }

  /// Runs the asked items until all are done, the deadline (measured from `started`) passes, no holder is live, or
  /// `keep` refused a chunk.
  /// The receiver is owned across the waits and handed back, so the fetch's future stays `Send`.
  async fn drive(
    &mut self,
    events: Receiver<FetchEvent>,
    keep: &mut impl FnMut(Chunk) -> bool,
    (started, timing): (u64, FetchTiming),
  ) -> Receiver<FetchEvent> {
    loop {
      let now = now_ns();
      while let Ok(event) = events.try_recv() {
        self.on_event(event, keep, now);
      }
      if self.refused || self.all_done() {
        return events;
      }
      if now.saturating_sub(started) >= timing.deadline_ns || !self.live.iter().any(|alive| *alive)
      {
        return events;
      }
      self.schedule(now, timing.hedge_after_ns);
      if slates_rt::futures::sleep(timing.poll_ns).await.is_err() {
        return events;
      }
    }
  }

  /// Stops every worker and waits for each session back: each worker sees the stop within one poll of its session.
  async fn stop(&mut self, events: Receiver<FetchEvent>, poll_ns: u64) {
    for command in self.commands.iter().flatten() {
      let _ = command.send(FetchCommand::Stop);
    }
    while self.running > 0 {
      match events.try_recv() {
        Ok(FetchEvent::Returned(index, endpoint)) => self.take_back(index, *endpoint),
        Ok(_) => {}
        Err(TryRecvError::Disconnected) => return,
        Err(TryRecvError::Empty) => {
          if slates_rt::futures::sleep(poll_ns).await.is_err() {
            return;
          }
        }
      }
    }
  }
}

/// Fetches `object`'s content `manifest` from its recorded `holders` (each over a connected session) — the manifest,
/// then the chunks `stage` wants — striped, hedged and tied (§4.10 "fetches the manifest by identity from a recorded
/// holder … chunk reads fault to hedged fetches by identity from the recorded holders, verified on arrival"; failure
/// matrix: "a recorded holder unreachable during fetch: Masked (another recorded holder, hedged)").
///
/// Every item — the manifest, each chunk — is asked first of the holder its identity ranks highest ([`chunk_rank`]),
/// which spreads the chunks across every holder. An item still unanswered after `timing.hedge_after_ns` is asked of
/// the next-ranked holder too (a hedged request), an idle holder takes the item outstanding longest elsewhere (work
/// stealing), and the first verified answer wins, the other copies cancelled (tied requests; Dean & Barroso, CACM
/// 2013). A holder whose session fails, or that answers anything but the item asked, is dropped, and every item it
/// alone held moves on at once. The manifest is verified against its identity and handed to `stage`, which returns
/// the chunks still wanted (`None` refuses the fetch); `keep` verifies and stages each chunk as it arrives
/// (AUD-29-55), and its refusal ends the fetch incomplete. One holder is the degenerate: no hedge, no steal, the
/// same code (R8). Every session is returned whatever the outcome; a chunk never answered stays wanted, and the stage
/// keeps what arrived, so the next fetch asks only for the rest.
pub async fn fetch(
  holders: Vec<(HostId, Endpoint)>,
  (object, manifest): (ObjectId, [u8; 32]),
  timing: FetchTiming,
  stage: impl FnOnce(&Archive) -> Option<Vec<[u8; 32]>>,
  mut keep: impl FnMut(Chunk) -> bool,
) -> Fetched {
  let (event_tx, event_rx) = channel::<FetchEvent>();
  let mut run = FetchRun::start(holders, (object, manifest), timing.poll_ns, &event_tx);
  drop(event_tx);
  let started = now_ns();
  run
    .items
    .push(Wanted::new(Item::Manifest, &run.hosts, &manifest, started));
  run.ask_next(0, started);
  let event_rx = run.drive(event_rx, &mut keep, (started, timing)).await;
  let wanted = run.manifest.as_ref().map(stage);
  let event_rx = match wanted {
    Some(Some(chunks)) => {
      let now = now_ns();
      for identity in chunks {
        let at = run.items.len();
        run.items.push(Wanted::new(
          Item::Chunk(identity),
          &run.hosts,
          &identity,
          now,
        ));
        run.ask_next(at, now);
      }
      // Each holder's window is the share it was first given (its outstanding now), at least one.
      for (window, outstanding) in run.window.iter_mut().zip(&run.outstanding) {
        *window = (*outstanding).max(1);
      }
      let event_rx = run.drive(event_rx, &mut keep, (started, timing)).await;
      run.fetched.complete = !run.refused && run.all_done();
      event_rx
    }
    Some(None) | None => event_rx,
  };
  run.stop(event_rx, timing.poll_ns).await;
  run.fetched
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

  /// The room a daemon restart rebuilds a hold into (A-64): a fresh room whose arena holds this one's bytes at the
  /// same offsets, as the anchor's RAM survives the daemon.
  pub(crate) fn restarted(&self) -> TestSpace {
    let mut fresh = TestSpace::new();
    let bytes = self
      .arena
      .region(0)
      .map(|region| region.bytes().to_vec())
      .unwrap();
    fresh
      .arena
      .region_mut(0)
      .unwrap()
      .bytes_mut()
      .copy_from_slice(&bytes);
    fresh
  }

  /// A hold rebuilt from `image` in this room, both halves: claimed, then rebuilt.
  pub(crate) fn recover(&mut self, image: &[u8]) -> Result<ContentHold, HoldImageError> {
    let claimed = ContentHold::claim_image(&mut self.arena, image)?;
    ContentHold::from_claimed(&mut self.space(), claimed)
  }
}

#[cfg(test)]
mod tests {
  use slates_archive::{Entry, Extent, Node, NodeMeta};

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
        archive: vec![7, 8, 9],
      },
      ContentMessage::Missing {
        object: OBJECT,
        sequence: SEQUENCE,
        manifest: hash(1),
        missing: vec![hash(3)],
      },
      ContentMessage::Chunk {
        object: OBJECT,
        sequence: SEQUENCE,
        manifest: hash(1),
        chunk: Archive::raw_chunk(b"one chunk".to_vec()),
      },
      ContentMessage::Staged {
        object: OBJECT,
        sequence: SEQUENCE,
        manifest: hash(1),
        remaining: 2,
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
      ContentMessage::FetchChunk {
        object: OBJECT,
        manifest: hash(1),
        chunk: hash(2),
      },
      ContentMessage::Piece {
        object: OBJECT,
        manifest: hash(1),
        chunk: Archive::raw_chunk(b"a piece".to_vec()),
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
    let mut missing = ContentMessage::Missing {
      object: OBJECT,
      sequence: SEQUENCE,
      manifest: hash(1),
      missing: vec![hash(2)],
    }
    .encode();
    let count_at = 1 + OBJECT_BYTES + size_of::<u64>() + HASH_BYTES;
    missing[count_at..count_at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
      ContentMessage::decode(&missing),
      Err(ContentError::BadLength)
    );

    let mut offer = ContentMessage::Offer {
      object: OBJECT,
      sequence: SEQUENCE,
      archive: vec![1],
    }
    .encode();
    let len_at = 1 + OBJECT_BYTES + size_of::<u64>();
    offer[len_at..len_at + 4].copy_from_slice(&(1u32 << 30).to_le_bytes());
    assert_eq!(ContentMessage::decode(&offer), Err(ContentError::BadLength));

    // A chunk whose payload disagrees with its stored length, and one past the format's chunk cap.
    let chunk = |raw_len, stored_len| {
      ContentMessage::Chunk {
        object: OBJECT,
        sequence: SEQUENCE,
        manifest: hash(1),
        chunk: Chunk {
          raw_len,
          stored_len,
          ..Archive::raw_chunk(b"bytes".to_vec())
        },
      }
      .encode()
    };
    assert_eq!(
      ContentMessage::decode(&chunk(5, 4)),
      Err(ContentError::BadLength)
    );
    assert_eq!(
      ContentMessage::decode(&chunk(MAX_CHUNK_BYTES + 1, 5)),
      Err(ContentError::BadLength)
    );

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
    assert_eq!(archive.referenced_chunks(), identities);
  }

  /// The holder side served end to end in-process: an offer to a hold with one chunk already present
  /// answers the one missing identity; that chunk, corrupted (a flipped bit), is refused with an empty reply
  /// and nothing stored; the chunk itself completes the stage and is acknowledged bound to the object,
  /// sequence and manifest; a fetch hands the manifest back and a chunk fetch each referenced chunk, from
  /// which a reader's stage rebuilds the archive byte for byte (AUD-29-55).
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

    let offer = offer_request(&archive, OBJECT, SEQUENCE);
    let Ok(ContentMessage::Missing { missing, .. }) = ContentMessage::decode(
      &hold
        .serve(
          &mut room.space(),
          holder,
          &offer,
          |_, _| true,
          |_, _, _| true,
        )
        .0,
    ) else {
      panic!("an offer is answered with the missing set");
    };
    assert_eq!(missing, vec![archive.chunks[1].identity]);

    // A corrupt chunk: flip a bit in the missing chunk's payload.
    let mut corrupt = archive.chunks[1].clone();
    corrupt.payload[0] ^= 0x01;
    let refused = hold
      .serve(
        &mut room.space(),
        holder,
        &ContentMessage::Chunk {
          object: OBJECT,
          sequence: SEQUENCE,
          manifest: archive.manifest_identity(),
          chunk: corrupt,
        }
        .encode(),
        |_, _| true,
        |_, _, _| true,
      )
      .0;
    assert!(refused.is_empty(), "a corrupt chunk is refused, not held");
    assert_eq!(hold.chunk_count(), 1);

    let [request] = chunk_requests(&archive, OBJECT, SEQUENCE, &missing)
      .try_into()
      .unwrap();
    let reply = hold
      .serve(
        &mut room.space(),
        holder,
        &request,
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

    assert_fetches_rebuild(&mut hold, &mut room, holder, archive);
  }

  /// The fetch half of the serve test: the manifest comes back alone, a reader's stage rebuilds the archive
  /// byte for byte from chunk fetches, and a manifest not held draws the empty answer.
  fn assert_fetches_rebuild(
    hold: &mut ContentHold,
    room: &mut TestSpace,
    holder: HostId,
    archive: Archive,
  ) {
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
      panic!("a fetch is answered with the manifest");
    };
    let manifest = Archive::decode(&bytes).unwrap();
    assert_eq!(manifest, with_chunks(&archive, Vec::new()));
    assert_eq!(
      fetched_through_a_stage(hold, room, holder, &manifest),
      Some(archive),
      "the fetched archive rebuilds byte for byte"
    );
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

  /// A reader's fetch of `manifest` from `hold` (as `OBJECT`'s content), staged for `OTHER` in a fresh hold:
  /// every chunk the stage wants fetched with `FetchChunk`, kept, and the stage completed; the rebuilt archive.
  fn fetched_through_a_stage(
    hold: &mut ContentHold,
    room: &mut TestSpace,
    holder: HostId,
    manifest: &Archive,
  ) -> Option<Archive> {
    let identity = manifest.manifest_identity();
    let mut reader = ContentHold::new();
    let mut reader_room = TestSpace::new();
    let wanted = reader
      .stage_fetched(&mut reader_room.space(), OTHER, Placed::default(), manifest)
      .ok()??;
    for chunk in wanted {
      let request = ContentMessage::FetchChunk {
        object: OBJECT,
        manifest: identity,
        chunk,
      }
      .encode();
      let reply = hold
        .serve(
          &mut room.space(),
          holder,
          &request,
          |_, _| true,
          |_, _, _| true,
        )
        .0;
      let Ok(ContentMessage::Piece { chunk, .. }) = ContentMessage::decode(&reply) else {
        return None;
      };
      reader
        .stage_piece(&mut reader_room.space(), OTHER, &identity, chunk)
        .ok()?;
    }
    let completed = reader
      .complete_stage(&mut reader_room.space(), OTHER)
      .ok()?;
    reader.archive_of(&reader_room.arena, OTHER, &completed)
  }

  /// AUD-29-55 (the fetch half): do: stage a fetched manifest of two chunks, keep the first chunk, cut, and
  /// stage the manifest again; expect the first staging to want both chunks, completion refused while one is
  /// owed, the second staging to want exactly the chunk still owed, and the stage complete once it is kept.
  #[test]
  fn a_cut_fetch_resumes_with_exactly_the_chunks_still_owed() {
    let archive = two_chunk_archive();
    let manifest = with_chunks(&archive, Vec::new());
    let identity = manifest.manifest_identity();
    let mut reader = ContentHold::new();
    let mut room = TestSpace::new();
    let wanted = reader
      .stage_fetched(&mut room.space(), OBJECT, Placed::default(), &manifest)
      .unwrap()
      .unwrap();
    assert_eq!(wanted.len(), 2);
    reader
      .stage_piece(
        &mut room.space(),
        OBJECT,
        &identity,
        archive.chunks[0].clone(),
      )
      .unwrap();
    assert_eq!(
      reader.complete_stage(&mut room.space(), OBJECT),
      Err(ContentRefusal::Incomplete { missing: 1 })
    );
    let resumed = reader
      .stage_fetched(&mut room.space(), OBJECT, Placed::default(), &manifest)
      .unwrap()
      .unwrap();
    assert_eq!(resumed, vec![archive.chunks[1].identity]);
    reader
      .stage_piece(
        &mut room.space(),
        OBJECT,
        &identity,
        archive.chunks[1].clone(),
      )
      .unwrap();
    assert_eq!(
      reader.complete_stage(&mut room.space(), OBJECT),
      Ok(identity)
    );
    assert!(reader.holds_manifest(OBJECT, &identity));
  }

  /// A second object, for the scoping tests.
  const OTHER: ObjectId = ObjectId([9; OBJECT_BYTES]);

  /// AUD-29-45 (§4.13 "Content identity and sharing"): do: hold one object's archive, then ask about the same
  /// chunks and manifest for another object — an offer's missing set, a put leaning on them unshipped, a
  /// fetch, a chunk fetch; expect every answer as if nothing were held: the chunks missing, the put refused
  /// incomplete, the fetches empty — while the first object's own answers are unchanged, and the bytes stored
  /// once.
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
    let piece = hold
      .serve(
        &mut room.space(),
        HostId(3),
        &ContentMessage::FetchChunk {
          object: OTHER,
          manifest,
          chunk: identities[0],
        }
        .encode(),
        |_, _| true,
        |_, _, _| true,
      )
      .0;
    assert!(piece.is_empty(), "another object's chunk is not served");
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
          archive: with_chunks(&archive, Vec::new()).encode(),
        },
      ),
      (
        ContentAccess::Place,
        ContentMessage::Chunk {
          object: OBJECT,
          sequence: SEQUENCE,
          manifest: archive.manifest_identity(),
          chunk: archive.chunks[0].clone(),
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

/// The content hold's manifest-to-chunk ownership oracle (AUD-29-44, AUD-29-55; AC-7.3, AC-7.7, AC-8.12). Each
/// generated case is a **shape** — a set of distinct manifests, each referencing a non-empty subset of a chunk
/// pool — and a **history** over it: whole puts (any object, any manifest, any subset of the pool shipped: the
/// referenced chunks, unreferenced ones, or none; retries included), forgets of one manifest or of a whole
/// object, and the transfer protocol a cut can interrupt anywhere — offers at any of a few sequences and single
/// chunks sent into a stage, some corrupted. The history runs against the hold and against a serial model of
/// the design's rule. After every step the two must agree on the reply (held, refused, the missing set, or
/// progress); every manifest the model holds is held and reconstructible to exactly its referenced chunks with
/// verified bytes; every stage keeps exactly the chunks the model says it verified (a cut loses nothing it
/// kept); the store keeps exactly the chunks some held manifest references or some stage kept (no orphan, no
/// premature eviction); and the shard's charges equal the hold's account.
///
/// The model states the rule, not the code: an offer is answered with the referenced chunks the object holds
/// neither in a held manifest nor in its stage of that manifest (or held at once when none are lacking); a
/// chunk is kept only for a stage of a manifest referencing it, and a corrupt one only when its verified bytes
/// are already stored; the acknowledgement comes exactly when the closure is complete; an older placement
/// never replaces a newer stage.
///
/// Nothing about the shape is hand-picked: the manifests' overlaps are generated. The generator's bounds are
/// the smallest at which every case the rule distinguishes can occur (each bound's `Derived:` line names the
/// case that needs it), and a **case census** counts each case as the run meets it; the run fails if any case
/// was never met, so a generator that silently stopped reaching a case cannot pass as an oracle. The run is
/// deterministic (a fixed-seed runner), so the census is a stable fact, not a probability.
#[cfg(test)]
mod ownership_oracle {
  use std::cell::RefCell;
  use std::collections::{BTreeMap, BTreeSet};

  use proptest::prelude::*;
  use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};
  use slates_archive::format::{MAX_BASE_PAGE_BYTES, MAX_CHUNK_BYTES};
  use slates_archive::{Entry, Extent, Node, NodeMeta};

  use super::*;

  /// Derived: two objects is the smallest count at which one chunk can be held for two objects at once and a
  /// put can be refused although the chunk it lacks is held — for another object (cross-object isolation).
  const OBJECTS: usize = 2;
  /// Derived: two manifests is the smallest count at which manifests share a chunk, so a put can complete
  /// from a chunk another held manifest owns, a forget can keep a chunk another manifest still references,
  /// and a stage of one manifest can be replaced by an offer of the other.
  const MANIFESTS: usize = 2;
  /// Derived: two shared-or-private roles per manifest pair need three chunks (one shared, one private to
  /// each of the two manifests), and a put shipping a chunk no manifest references needs a fourth.
  const POOL: usize = 3 + 1;
  /// Derived: three offer sequences are the fewest that put a second offer below, at and above a stage's
  /// (stale, resumed, replacing).
  const SEQUENCES: u64 = 3;
  /// Derived: filling every (object, manifest) slot and emptying it again (`2 × OBJECTS × MANIFESTS`), plus
  /// one transfer of a whole pool one chunk per step with its offer (`POOL + 1`), so every model state and a
  /// transfer cut at any chunk are reachable within one history.
  const HISTORY: usize = 2 * OBJECTS * MANIFESTS + POOL + 1;
  /// Derived: the whole-put history's step kinds — put and forget.
  const WHOLE_KINDS: u8 = 2;
  /// Derived: the transfer history's step kinds — those two, then offer, send and forget an object.
  const TRANSFER_KINDS: u8 = 5;

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
    /// An offer of `manifest` for `object` at `sequence` (AUD-29-55).
    Offer {
      object: usize,
      manifest: usize,
      sequence: u64,
    },
    /// One chunk of the pool sent into `object`'s stage of `manifest`, its payload corrupted or not.
    Send {
      object: usize,
      manifest: usize,
      chunk: usize,
      corrupt: bool,
      /// Whether the chunk is **aimed**, as an owner sends it: one the object's stage lacks, for the stage's
      /// manifest, whatever `manifest` and `chunk` say. The other arm sends as drawn and reaches the
      /// refusals and duplicates.
      aimed: bool,
    },
    /// Every manifest and the stage of `object` forgotten (a tombstone, a stale-copy reclaim).
    ForgetObject {
      object: usize,
    },
  }

  /// A step, generated as a plain tuple so the strategy needs no `Arc`-backed `prop_oneof!` (R2).
  fn step(kinds: u8) -> impl Strategy<Value = Step> {
    (
      0..kinds,
      (0..OBJECTS, 0..MANIFESTS),
      proptest::collection::btree_set(0..POOL, 0..=POOL),
      (any::<bool>(), any::<bool>(), 0..SEQUENCES, 0..POOL),
    )
      .prop_map(
        |(kind, (object, manifest), shipped, (flag, aimed, sequence, chunk))| match kind {
          0 => Step::Put {
            object,
            manifest,
            shipped,
            offered: flag,
          },
          1 => Step::Forget { object, manifest },
          2 => Step::Offer {
            object,
            manifest,
            sequence,
          },
          3 => Step::Send {
            object,
            manifest,
            chunk,
            corrupt: flag,
            aimed,
          },
          _ => Step::ForgetObject { object },
        },
      )
  }

  /// A shape: distinct manifests (equal chunk sets are one manifest, one identity), each non-empty.
  fn shape() -> impl Strategy<Value = Vec<BTreeSet<usize>>> {
    proptest::collection::btree_set(
      proptest::collection::btree_set(0..POOL, 1..=POOL),
      1..=MANIFESTS,
    )
    .prop_map(|manifests| manifests.into_iter().collect())
  }

  /// Every case the rule distinguishes; the census counts each as a history meets it.
  #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
  enum Case {
    /// A put completed with every referenced chunk shipped.
    PutWholeShipped,
    /// A put completed with a referenced chunk not shipped but held by another of the object's manifests.
    PutCompletedFromHeld,
    /// A put completed with a referenced chunk not shipped but kept by the object's stage of the manifest.
    PutCompletedFromStage,
    /// A put refused: a referenced chunk neither shipped nor held for the object.
    PutRefused,
    /// A put refused although the missing chunk is held — for another object.
    PutRefusedHeldElsewhere,
    /// A put of a manifest the object already holds.
    Retry,
    /// A put refused for shipping a chunk the manifest does not reference.
    PutRefusedUnreferenced,
    /// A put refused because the object's stage holds another manifest at a newer or equal sequence.
    PutRefusedStale,
    /// One chunk held for two objects at once.
    SharedAcrossObjects,
    /// A forget that kept a chunk another of the object's manifests still references.
    ForgetKept,
    /// A forget that released a chunk nothing references any more.
    ForgetReleased,
    /// A forget of a manifest the object does not hold.
    ForgetNotHeld,
    /// An offer answered with a missing set (a stage opened or resumed).
    OfferMissing,
    /// An offer of a manifest the object already holds, acknowledged at once.
    OfferHeld,
    /// An offer whose closure was already complete, acknowledged at once.
    OfferCompletedAtOnce,
    /// An offer that resumed a stage holding verified chunks (a cut transfer resuming).
    OfferResumed,
    /// An offer at a newer sequence replacing the object's stage of another manifest.
    OfferReplaced,
    /// An offer at an older or equal sequence refused against a stage of another manifest.
    OfferStale,
    /// A chunk kept in its stage, with chunks still lacking (progress, no acknowledgement).
    SendStaged,
    /// A chunk completing its stage: acknowledged.
    SendCompleted,
    /// A chunk the stage already kept, answered with the same progress.
    SendDuplicate,
    /// A chunk for a manifest the object has no stage of.
    SendUnstaged,
    /// A chunk the staged manifest does not reference.
    SendUnreferenced,
    /// A corrupt chunk whose bytes are not stored: refused, nothing kept.
    SendCorruptRefused,
    /// A stage found short when its last missing chunk arrived (a held chunk was released meanwhile).
    SendShort,
    /// An object forgotten while it had a stage.
    ForgetObjectWithStage,
  }

  /// The cases a history of whole puts and forgets can meet, for its census's completeness check.
  const WHOLE_CASES: [Case; 10] = [
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

  /// The cases only a history with transfers can meet (AUD-29-55), for its census's completeness check.
  const TRANSFER_CASES: [Case; 16] = [
    Case::PutCompletedFromStage,
    Case::PutRefusedStale,
    Case::OfferMissing,
    Case::OfferHeld,
    Case::OfferCompletedAtOnce,
    Case::OfferResumed,
    Case::OfferReplaced,
    Case::OfferStale,
    Case::SendStaged,
    Case::SendCompleted,
    Case::SendDuplicate,
    Case::SendUnstaged,
    Case::SendUnreferenced,
    Case::SendCorruptRefused,
    Case::SendShort,
    Case::ForgetObjectWithStage,
  ];

  /// A transfer in progress, as the rule sees it.
  #[derive(Clone, Debug)]
  struct ModelStage {
    manifest: usize,
    sequence: u64,
    missing: BTreeSet<usize>,
    staged: BTreeSet<usize>,
  }

  /// The model: for each object, the manifests it holds and its stage.
  #[derive(Clone, Debug, Default)]
  struct Model {
    held: BTreeMap<usize, BTreeSet<usize>>,
    stages: BTreeMap<usize, ModelStage>,
  }

  /// What a step's reply says, on both sides.
  #[derive(Clone, Debug, PartialEq, Eq)]
  enum Outcome {
    /// Held or acknowledged.
    Held,
    /// Refused: an error or the empty reply.
    Refused,
    /// An offer's missing set, by pool index.
    Missing(BTreeSet<usize>),
    /// A chunk kept, with chunks still lacking.
    Progress,
  }

  /// The chunks `object`'s held manifests reference.
  fn chunks_of(model: &Model, manifests: &[BTreeSet<usize>], object: usize) -> BTreeSet<usize> {
    model
      .held
      .get(&object)
      .into_iter()
      .flatten()
      .filter_map(|&index| manifests.get(index))
      .flatten()
      .copied()
      .collect()
  }

  /// Every chunk whose bytes are stored: referenced by a held manifest or kept by a stage.
  fn stored(model: &Model, manifests: &[BTreeSet<usize>]) -> BTreeSet<usize> {
    (0..OBJECTS)
      .flat_map(|object| chunks_of(model, manifests, object))
      .chain(model.stages.values().flat_map(|stage| stage.staged.clone()))
      .collect()
  }

  /// Why the rule refuses a stage operation.
  enum Refused {
    Stale,
    Unstaged,
    Unreferenced,
    Corrupt,
  }

  /// Opens or resumes `object`'s stage of `manifest` at `sequence` by the rule; `Ok(None)` when the manifest is
  /// held already, else the missing set and whether the stage was created.
  fn model_open(
    model: &mut Model,
    manifests: &[BTreeSet<usize>],
    (object, manifest, sequence): (usize, usize, u64),
  ) -> Result<Option<(BTreeSet<usize>, bool)>, Refused> {
    let referenced = manifests.get(manifest).cloned().unwrap_or_default();
    if model
      .held
      .get(&object)
      .is_some_and(|held| held.contains(&manifest))
    {
      return Ok(None);
    }
    let have = chunks_of(model, manifests, object);
    if let Some(stage) = model.stages.get_mut(&object) {
      if stage.manifest == manifest {
        stage.sequence = stage.sequence.max(sequence);
        stage.missing = referenced
          .iter()
          .filter(|chunk| !have.contains(chunk) && !stage.staged.contains(chunk))
          .copied()
          .collect();
        return Ok(Some((stage.missing.clone(), false)));
      }
      if sequence <= stage.sequence {
        return Err(Refused::Stale);
      }
      model.stages.remove(&object);
    }
    let missing: BTreeSet<usize> = referenced.difference(&have).copied().collect();
    model.stages.insert(
      object,
      ModelStage {
        manifest,
        sequence,
        missing: missing.clone(),
        staged: BTreeSet::new(),
      },
    );
    Ok(Some((missing, true)))
  }

  /// Keeps `chunk` in `object`'s stage of `manifest` by the rule; the chunks still missing, or why not.
  fn model_send(
    model: &mut Model,
    manifests: &[BTreeSet<usize>],
    (object, manifest, chunk): (usize, usize, usize),
    corrupt: bool,
  ) -> Result<(usize, bool), Refused> {
    let have = chunks_of(model, manifests, object);
    let stored_now = stored(model, manifests);
    let Some(stage) = model
      .stages
      .get_mut(&object)
      .filter(|stage| stage.manifest == manifest)
    else {
      return Err(Refused::Unstaged);
    };
    if !stage.missing.contains(&chunk) {
      return if stage.staged.contains(&chunk) || have.contains(&chunk) {
        Ok((stage.missing.len(), true))
      } else {
        Err(Refused::Unreferenced)
      };
    }
    if corrupt && !stored_now.contains(&chunk) {
      return Err(Refused::Corrupt);
    }
    stage.missing.remove(&chunk);
    stage.staged.insert(chunk);
    Ok((stage.missing.len(), false))
  }

  /// Completes `object`'s stage by the rule: held when every referenced chunk is held for the object or
  /// staged; otherwise the stage stays, its missing set recomputed, and the count lacking is returned.
  fn model_promote(
    model: &mut Model,
    manifests: &[BTreeSet<usize>],
    object: usize,
  ) -> Result<(), usize> {
    let have = chunks_of(model, manifests, object);
    let Some(stage) = model.stages.get_mut(&object) else {
      return Err(0);
    };
    let referenced = manifests.get(stage.manifest).cloned().unwrap_or_default();
    let lacking: BTreeSet<usize> = referenced
      .iter()
      .filter(|chunk| !have.contains(chunk) && !stage.staged.contains(chunk))
      .copied()
      .collect();
    if !lacking.is_empty() {
      let count = lacking.len();
      stage.missing = lacking;
      return Err(count);
    }
    let manifest = stage.manifest;
    model.stages.remove(&object);
    model.held.entry(object).or_default().insert(manifest);
    Ok(())
  }

  /// A whole put by the rule (the hold's `hold`, through the same stage), recording its cases.
  fn model_put(
    model: &mut Model,
    manifests: &[BTreeSet<usize>],
    (object, manifest, shipped): (usize, usize, &BTreeSet<usize>),
    census: &mut BTreeSet<Case>,
  ) -> Outcome {
    let Some(referenced) = manifests.get(manifest).cloned() else {
      return Outcome::Refused;
    };
    if !shipped.is_subset(&referenced) {
      census.insert(Case::PutRefusedUnreferenced);
      return Outcome::Refused;
    }
    let staged_before = model
      .stages
      .get(&object)
      .filter(|stage| stage.manifest == manifest)
      .map(|stage| stage.staged.clone())
      .unwrap_or_default();
    let created = match model_open(model, manifests, (object, manifest, 0)) {
      Err(_) => {
        census.insert(Case::PutRefusedStale);
        return Outcome::Refused;
      }
      Ok(None) => {
        census.insert(Case::Retry);
        return Outcome::Held;
      }
      Ok(Some((_, created))) => created,
    };
    let held_before = chunks_of(model, manifests, object);
    for &chunk in shipped {
      let _ = model_send(model, manifests, (object, manifest, chunk), false);
    }
    match model_promote(model, manifests, object) {
      Ok(()) => {
        let unshipped: BTreeSet<usize> = referenced.difference(shipped).copied().collect();
        census.insert(if unshipped.is_empty() {
          Case::PutWholeShipped
        } else if !unshipped.is_disjoint(&staged_before) {
          Case::PutCompletedFromStage
        } else {
          Case::PutCompletedFromHeld
        });
        let _ = held_before;
        Outcome::Held
      }
      Err(_) => {
        census.insert(Case::PutRefused);
        let have = chunks_of(model, manifests, object);
        let lacking: BTreeSet<usize> = referenced
          .iter()
          .filter(|chunk| !shipped.contains(chunk) && !have.contains(chunk))
          .copied()
          .collect();
        let elsewhere = (0..OBJECTS)
          .filter(|other| *other != object)
          .any(|other| !lacking.is_disjoint(&chunks_of(model, manifests, other)));
        if elsewhere {
          census.insert(Case::PutRefusedHeldElsewhere);
        }
        if created {
          model.stages.remove(&object);
        }
        Outcome::Refused
      }
    }
  }

  /// An offer by the rule, recording its cases.
  fn model_offer(
    model: &mut Model,
    manifests: &[BTreeSet<usize>],
    (object, manifest, sequence): (usize, usize, u64),
    census: &mut BTreeSet<Case>,
  ) -> Outcome {
    let before = model.stages.get(&object).cloned();
    match model_open(model, manifests, (object, manifest, sequence)) {
      Err(_) => {
        census.insert(Case::OfferStale);
        Outcome::Refused
      }
      Ok(None) => {
        census.insert(Case::OfferHeld);
        Outcome::Held
      }
      Ok(Some((missing, created))) => {
        match &before {
          Some(stage) if stage.manifest != manifest => {
            census.insert(Case::OfferReplaced);
          }
          Some(stage) if !created && !stage.staged.is_empty() => {
            census.insert(Case::OfferResumed);
          }
          _ => {}
        }
        if missing.is_empty() {
          let _ = model_promote(model, manifests, object);
          census.insert(Case::OfferCompletedAtOnce);
          return Outcome::Held;
        }
        census.insert(Case::OfferMissing);
        Outcome::Missing(missing)
      }
    }
  }

  /// A chunk sent by the rule, recording its cases.
  fn model_chunk(
    model: &mut Model,
    manifests: &[BTreeSet<usize>],
    (object, manifest, chunk): (usize, usize, usize),
    corrupt: bool,
    census: &mut BTreeSet<Case>,
  ) -> Outcome {
    match model_send(model, manifests, (object, manifest, chunk), corrupt) {
      Err(Refused::Unstaged | Refused::Stale) => {
        census.insert(Case::SendUnstaged);
        Outcome::Refused
      }
      Err(Refused::Unreferenced) => {
        census.insert(Case::SendUnreferenced);
        Outcome::Refused
      }
      Err(Refused::Corrupt) => {
        census.insert(Case::SendCorruptRefused);
        Outcome::Refused
      }
      Ok((0, _)) => match model_promote(model, manifests, object) {
        Ok(()) => {
          census.insert(Case::SendCompleted);
          Outcome::Held
        }
        Err(_) => {
          census.insert(Case::SendShort);
          Outcome::Progress
        }
      },
      Ok((_, duplicate)) => {
        census.insert(if duplicate {
          Case::SendDuplicate
        } else {
          Case::SendStaged
        });
        Outcome::Progress
      }
    }
  }

  /// Applies `step` to the model by the rule, recording the cases it meets; `None` for a step naming no
  /// manifest of the shape.
  fn model_apply(
    model: &mut Model,
    manifests: &[BTreeSet<usize>],
    step: &Step,
    census: &mut BTreeSet<Case>,
  ) -> Option<Outcome> {
    let named = |manifest: &usize| manifests.get(*manifest).map(|_| ());
    match step {
      Step::Put {
        object,
        manifest,
        shipped,
        ..
      } => {
        named(manifest)?;
        Some(model_put(
          model,
          manifests,
          (*object, *manifest, shipped),
          census,
        ))
      }
      Step::Forget { object, manifest } => {
        let referenced = manifests.get(*manifest)?;
        let removed = model
          .held
          .get_mut(object)
          .is_some_and(|held| held.remove(manifest));
        if !removed {
          census.insert(Case::ForgetNotHeld);
          return Some(Outcome::Refused);
        }
        let still = chunks_of(model, manifests, *object);
        if !referenced.is_disjoint(&still) {
          census.insert(Case::ForgetKept);
        }
        if !referenced.is_subset(&still) {
          census.insert(Case::ForgetReleased);
        }
        Some(Outcome::Held)
      }
      Step::Offer {
        object,
        manifest,
        sequence,
      } => {
        named(manifest)?;
        Some(model_offer(
          model,
          manifests,
          (*object, *manifest, *sequence),
          census,
        ))
      }
      Step::Send {
        object,
        manifest,
        chunk,
        corrupt,
        ..
      } => {
        named(manifest)?;
        Some(model_chunk(
          model,
          manifests,
          (*object, *manifest, *chunk),
          *corrupt,
          census,
        ))
      }
      Step::ForgetObject { object } => {
        if model.stages.remove(object).is_some() {
          census.insert(Case::ForgetObjectWithStage);
        }
        model.held.remove(object);
        Some(Outcome::Held)
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
    for at in 0..OBJECTS {
      for (index, chunks) in manifests.iter().enumerate() {
        let id = identity(chunks);
        let staged = model
          .stages
          .get(&at)
          .filter(|stage| stage.manifest == index)
          .map(|stage| stage.staged.len());
        if hold.staged_of(object(at), &id) != staged {
          return Err(format!(
            "object {at} manifest {index}: staged {:?}, the model {staged:?}",
            hold.staged_of(object(at), &id)
          ));
        }
        let held = model
          .held
          .get(&at)
          .is_some_and(|held| held.contains(&index));
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
        if archive
          .chunks
          .iter()
          .any(|chunk| Archive::verify_into(chunk, &mut []).is_err())
        {
          return Err(format!(
            "object {at} manifest {index} holds bytes failing their identity"
          ));
        }
        let got: BTreeSet<[u8; 32]> = archive.chunks.iter().map(|c| c.identity).collect();
        let expected: BTreeSet<[u8; 32]> = chunks.iter().map(|&c| chunk(c).identity).collect();
        if got != expected {
          return Err(format!(
            "object {at} manifest {index} reconstructed other chunks"
          ));
        }
      }
    }
    let manifest_count: usize = model.held.values().map(BTreeSet::len).sum();
    if hold.manifest_count() != manifest_count || hold.stage_count() != model.stages.len() {
      return Err(format!(
        "{} manifests and {} stages, the model {manifest_count} and {}",
        hold.manifest_count(),
        hold.stage_count(),
        model.stages.len()
      ));
    }
    let stored = stored(model, manifests);
    if hold.chunk_count() != stored.len() {
      return Err(format!(
        "{} chunks stored, the model stores {}",
        hold.chunk_count(),
        stored.len()
      ));
    }
    Ok(())
  }

  /// `step` as it runs against `manifests` from the model's state: an offered put ships exactly its manifest's
  /// chunks the object neither holds nor has kept in its stage of the manifest — the missing set the offer
  /// draws — and an aimed send carries a chunk
  /// its object's stage lacks, for that stage's manifest.
  fn resolve(step: &Step, manifests: &[BTreeSet<usize>], model: &Model) -> Step {
    match step {
      Step::Send {
        object,
        chunk,
        corrupt,
        aimed: true,
        ..
      } if model.stages.contains_key(object) => {
        let stage = &model.stages[object];
        let lacking: Vec<usize> = stage.missing.iter().copied().collect();
        Step::Send {
          object: *object,
          manifest: stage.manifest,
          chunk: lacking
            .get(chunk % lacking.len().max(1))
            .copied()
            .unwrap_or(*chunk),
          corrupt: *corrupt,
          aimed: false,
        }
      }
      Step::Put {
        object,
        manifest,
        offered: true,
        ..
      } => {
        let mut have = chunks_of(model, manifests, *object);
        if let Some(stage) = model
          .stages
          .get(object)
          .filter(|stage| stage.manifest == *manifest)
        {
          have.extend(stage.staged.iter().copied());
        }
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

  /// The pool index of each pool chunk's identity, to read a missing set back.
  fn pool_index() -> BTreeMap<[u8; 32], usize> {
    (0..POOL).map(|at| (chunk(at).identity, at)).collect()
  }

  /// What a served reply says.
  fn outcome_of(reply: &[u8]) -> Outcome {
    match ContentMessage::decode(reply) {
      Ok(ContentMessage::Ack(_)) => Outcome::Held,
      Ok(ContentMessage::Missing { missing, .. }) => {
        let index = pool_index();
        Outcome::Missing(
          missing
            .iter()
            .filter_map(|identity| index.get(identity).copied())
            .collect(),
        )
      }
      Ok(ContentMessage::Staged { .. }) => Outcome::Progress,
      _ => Outcome::Refused,
    }
  }

  /// Runs `step` on the hold and reports its outcome.
  fn hold_apply(
    hold: &mut ContentHold,
    room: &mut TestSpace,
    manifests: &[BTreeSet<usize>],
    step: &Step,
  ) -> Outcome {
    let served = |hold: &mut ContentHold, room: &mut TestSpace, request: Vec<u8>| {
      outcome_of(
        &hold
          .serve(
            &mut room.space(),
            HostId(2),
            &request,
            |_, _| true,
            |_, _, _| true,
          )
          .0,
      )
    };
    let empty = BTreeSet::new();
    match step {
      Step::Put {
        object: at,
        manifest,
        shipped,
        ..
      } => {
        let chunks = manifests.get(*manifest).unwrap_or(&empty);
        match hold.hold(
          &mut room.space(),
          object(*at),
          Placed::default(),
          archive(chunks, shipped),
        ) {
          Ok(_) => Outcome::Held,
          Err(_) => Outcome::Refused,
        }
      }
      Step::Forget {
        object: at,
        manifest,
      } => {
        let chunks = manifests.get(*manifest).unwrap_or(&empty);
        if hold.forget_manifest(&mut room.space(), object(*at), &identity(chunks)) {
          Outcome::Held
        } else {
          Outcome::Refused
        }
      }
      Step::Offer {
        object: at,
        manifest,
        sequence,
      } => {
        let chunks = manifests.get(*manifest).unwrap_or(&empty);
        let request = offer_request(&archive(chunks, &empty), object(*at), *sequence);
        served(hold, room, request)
      }
      Step::Send {
        object: at,
        manifest,
        chunk: index,
        corrupt,
        ..
      } => {
        let chunks = manifests.get(*manifest).unwrap_or(&empty);
        let mut sent = chunk(*index);
        if *corrupt && let Some(byte) = sent.payload.first_mut() {
          *byte ^= 0x01;
        }
        let request = ContentMessage::Chunk {
          object: object(*at),
          sequence: 0,
          manifest: identity(chunks),
          chunk: sent,
        }
        .encode();
        served(hold, room, request)
      }
      Step::ForgetObject { object: at } => {
        hold.forget_object(&mut room.space(), object(*at));
        Outcome::Held
      }
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
    let mut model = Model::default();
    for step in steps {
      let step = &resolve(step, manifests, &model);
      let Some(expected) = model_apply(&mut model, manifests, step, census) else {
        continue;
      };
      let got = hold_apply(&mut hold, &mut room, manifests, step);
      if got != expected {
        return Err(format!(
          "{step:?}: the hold answered {got:?}, the model {expected:?}"
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
    // AUD-29-59, AUD-29-55: what a restart recovers from the hold's image is the same hold, its stages'
    // verified progress included — the oracle's whole check holds against the recovered hold too, and its
    // image is byte-identical (deterministic).
    // A-64: the image names blocks, so the restart carries the arena's bytes, and the recovered hold adopts the very
    // blocks it names (its image is the same bytes) and charges exactly what the live hold did.
    let image = hold.to_image();
    let mut restarted = room.restarted();
    let mut recovered = restarted
      .recover(&image)
      .map_err(|refused| format!("image refused: {refused:?}"))?;
    check(&recovered, &restarted.arena, &model, manifests)
      .map_err(|disagreement| format!("recovered: {disagreement}"))?;
    if recovered.to_image() != image {
      return Err("the recovered hold images differently".to_owned());
    }
    if (
      restarted.budget.replicated(),
      restarted.arena.allocated_bytes(),
    ) != (room.budget.replicated(), room.arena.allocated_bytes())
    {
      return Err("the recovered hold holds other blocks or charges than the live one".to_owned());
    }
    recovered.forget_all(&mut restarted.space());
    if restarted.arena.allocated_bytes() != 0 {
      return Err("the recovered hold's release left blocks".to_owned());
    }
    // AUD-29-43, AUD-29-55: releasing everything — stages included — gives back every charge and every block.
    hold.forget_all(&mut room.space());
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

  /// Hostile input (§4.9) on the hold image (AUD-29-59, A-64): do: image a hold of two overlapping manifests,
  /// then, over the surviving arena, decode every truncation, the image with a trailing byte, one naming a block
  /// past the arena, one naming an unknown encoding, and the whole image after a byte of a chunk's block is
  /// flipped; expect the whole image to recover with nothing copied, and each damaged one refused typed with every
  /// claim given back — never a panic, never a hold of content that fails its identity.
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
    let image = hold.to_image();
    let mut whole = room.restarted();
    let recovered = whole.recover(&image).unwrap();
    assert_eq!(
      (recovered.manifest_count(), recovered.chunk_count()),
      (2, 3)
    );
    assert_eq!(recovered.newest_placed(a), Some(4));
    assert_eq!(
      whole.arena.allocated_bytes(),
      room.arena.allocated_bytes(),
      "the recovered hold adopted the blocks its image names, nothing copied"
    );
    refused_damage(&room, &image);
  }

  /// Every damaged form of `image` refused typed over a surviving copy of `room`, every claim given back.
  fn refused_damage(room: &TestSpace, image: &[u8]) {
    let mut damaged = room.restarted();
    for cut in 0..image.len() {
      assert!(
        damaged.recover(&image[..cut]).is_err() || cut == 0,
        "cut {cut}"
      );
    }
    let mut padded = image.to_vec();
    padded.push(0);
    assert_eq!(
      damaged.recover(&padded).err(),
      Some(HoldImageError::Malformed)
    );
    let mut far = HoldImage::from_bytes(image).unwrap();
    far.chunks[0].payload.offset = 1 << 40;
    assert_eq!(
      damaged.recover(&far.to_bytes()).err(),
      Some(HoldImageError::Unclaimable)
    );
    let mut foreign = HoldImage::from_bytes(image).unwrap();
    foreign.chunks[0].encoding = u8::MAX;
    assert_eq!(
      damaged.recover(&foreign.to_bytes()).err(),
      Some(HoldImageError::UnknownEncoding)
    );
    let decoded = HoldImage::from_bytes(image).unwrap();
    let block = decoded.chunks[0].payload;
    let at = usize::try_from(block.offset).unwrap();
    if let Some(byte) = damaged
      .arena
      .region_mut(block.region)
      .unwrap()
      .bytes_mut()
      .get_mut(at)
    {
      *byte ^= 0x01;
    }
    assert_eq!(
      damaged.recover(image).err(),
      Some(HoldImageError::Refused(ContentRefusal::IdentityMismatch))
    );
    assert_eq!(
      damaged.arena.allocated_bytes(),
      0,
      "every refused recovery gave its claims back"
    );
  }

  /// Runs generated shapes and histories of `kinds` steps, at most `history` long, with a fixed-seed runner at
  /// proptest's default case count, and returns the cases of `cases` the census never met.
  fn census_of(kinds: u8, history: usize, cases: &[Case]) -> Vec<Case> {
    let census = RefCell::new(BTreeSet::new());
    let mut runner = TestRunner::new_with_rng(
      slates_test_seeds::unseeded(Config::default()),
      TestRng::deterministic_rng(RngAlgorithm::ChaCha),
    );
    let strategy = (shape(), proptest::collection::vec(step(kinds), 1..=history));
    let result = runner.run(&strategy, |(manifests, steps)| {
      run(&manifests, &steps, &mut census.borrow_mut()).map_err(TestCaseError::fail)
    });
    if let Err(failure) = result {
      panic!("{failure}");
    }
    let met = census.into_inner();
    cases
      .iter()
      .copied()
      .filter(|case| !met.contains(case))
      .collect()
  }

  /// AUD-29-44: do: run generated shapes and histories of puts and forgets on a hold and on the serial model;
  /// expect them to agree after every step (see the module doc), and expect the census to have met every case
  /// the ownership rule distinguishes.
  #[test]
  fn the_hold_owns_exactly_what_its_manifests_reference() {
    let unmet = census_of(WHOLE_KINDS, 2 * OBJECTS * MANIFESTS, &WHOLE_CASES);
    assert!(unmet.is_empty(), "the generator never reached {unmet:?}");
  }

  /// AUD-29-55 (§4.9 "verified ranges and resumable progress"; AC-7.7): do: run generated histories that mix
  /// whole puts and forgets with offers, single chunks (aimed and drawn, some corrupt) and whole-object
  /// forgets — a transfer cut and resumed at every point a history can reach; expect the hold and the model to
  /// agree after every step (replies, staged progress, stored bytes, charges), the image to carry the stages
  /// across a restart, everything to release to zero, and the census to have met every transfer case.
  #[test]
  fn a_cut_transfer_keeps_its_verified_chunks_and_resumes_from_them() {
    let unmet = census_of(TRANSFER_KINDS, HISTORY, &TRANSFER_CASES);
    assert!(unmet.is_empty(), "the generator never reached {unmet:?}");
  }
}
