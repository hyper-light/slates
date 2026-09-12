//! The content plane (§4.8 mechanism 1, "sealed content and records, under one quorum rule"; §4.10
//! "Content replication"; §4.11 "Archive"; D-17): a sealed snapshot's archive travels from its owner
//! to the snapshot's candidate holders over the fleet transport and is **verified before it is
//! held**, so an acknowledgement means "every byte the manifest references is now on this holder,
//! checked against its identity" — the design's "a holder acknowledges only after … verifying all
//! required bytes" (§4.10 "Placement closure"). The archive is the transfer unit and the missing
//! set is the resumption unit (§4.11: "resumable by missing set; the same container is the
//! replication transfer unit and the clone-from-archive source").
//!
//! Three exchanges, each one request/reply on its own stream of the holder's record session
//! ([`CONTENT_OFFER_STREAM`], [`CONTENT_PUT_STREAM`], [`CONTENT_FETCH_STREAM`] — so the holder's
//! `serve_once` dispatches by kind, never by guessing at bytes, and the exchanges of one round, which
//! follow each other back to back, never reuse a stream the holder is still closing):
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

use slates_archive::format::{ArchiveError, Chunk};
use slates_archive::{Archive, ContentStore, Node, chunks_for};
use slates_db::register::{HostId, ObjectId, Placement, Quorum};
use slates_rt::error::RtError;
use slates_rt::futures::{detach, now_ns, spawn_child};
use slates_transport::endpoint::Endpoint;

use crate::{
  ClusterError, CommitBudget, DispatchWait, Reply, Stragglers, collect_bound, is_placed,
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
  /// A reader asks for a manifest's whole archive by identity.
  Fetch {
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
    Ok(self.take(1)?[0])
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
      ContentMessage::Fetch { manifest } => {
        out.push(KIND_FETCH);
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

/// Whether a chunk's payload decodes and hashes to its declared identity (the archive reader's own
/// check, [`Archive::content`], which refuses a mismatch by chunk).
fn verified(chunk: &Chunk) -> bool {
  Archive::content(chunk).is_ok()
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
    manifest: archive.manifest.clone(),
    chunks,
  }
}

/// What a node holds as a content candidate for other owners' snapshots (§4.10): the distinct chunks
/// by identity (deduplicated across every manifest held) and each held manifest by its identity, with
/// the archive header it arrived under so the whole archive can be reassembled for a reader. Serves
/// the holder side of every content exchange ([`ContentHold::serve`]); every archive is verified
/// before anything is stored, and a manifest is held only once every chunk it references is.
#[derive(Debug, Default)]
pub struct ContentHold {
  store: ContentStore,
  manifests: BTreeMap<[u8; 32], Archive>,
}

impl ContentHold {
  /// An empty hold.
  pub fn new() -> ContentHold {
    ContentHold::default()
  }

  /// Whether the manifest with `identity` is held whole.
  pub fn holds_manifest(&self, identity: &[u8; 32]) -> bool {
    self.manifests.contains_key(identity)
  }

  /// The distinct chunks held — the non-vacuity counter a test reads (a put of one missing chunk
  /// raises it by exactly one).
  pub fn chunk_count(&self) -> usize {
    self.store.unique_count()
  }

  /// The manifests held.
  pub fn manifest_count(&self) -> usize {
    self.manifests.len()
  }

  /// Of `chunks`, the identities this hold lacks — the missing set an offer is answered with.
  pub fn missing_of(&self, chunks: &[[u8; 32]]) -> Vec<[u8; 32]> {
    chunks
      .iter()
      .copied()
      .filter(|identity| !self.store.contains(identity))
      .collect()
  }

  /// Holds `archive` — every chunk it ships verified against its identity, every chunk its manifest
  /// references required to be shipped or already held — and returns the manifest identity now held.
  /// Nothing is stored on a refusal.
  pub fn hold(&mut self, archive: Archive) -> Result<[u8; 32], ContentRefusal> {
    if !archive.chunks.iter().all(verified) {
      return Err(ContentRefusal::IdentityMismatch);
    }
    let shipped: BTreeSet<[u8; 32]> = archive.chunks.iter().map(|chunk| chunk.identity).collect();
    let missing = referenced_chunks(&archive.manifest)
      .into_iter()
      .filter(|identity| !shipped.contains(identity) && !self.store.contains(identity))
      .count();
    if missing > 0 {
      return Err(ContentRefusal::Incomplete { missing });
    }
    let identity = archive.manifest.identity();
    let record = with_chunks(&archive, Vec::new());
    for chunk in archive.chunks {
      self.store.insert(chunk);
    }
    self.manifests.insert(identity, record);
    Ok(identity)
  }

  /// The whole archive for a held manifest — its header, manifest, and every referenced chunk in
  /// reference order — or `None` if the manifest is not held whole.
  pub fn archive_of(&self, identity: &[u8; 32]) -> Option<Archive> {
    let record = self.manifests.get(identity)?;
    let mut chunks = Vec::new();
    for referenced in referenced_chunks(&record.manifest) {
      chunks.push(self.store.get(&referenced)?.clone());
    }
    Some(with_chunks(record, chunks))
  }

  /// Serves one content request as `holder`: an offer is answered with the missing set, a put with a
  /// bound acknowledgement once verified and held whole, a fetch with the whole archive; anything
  /// malformed, unverifiable or incomplete is answered with an empty reply (the owner counts nothing).
  pub fn serve(&mut self, holder: HostId, request: &[u8]) -> Vec<u8> {
    match ContentMessage::decode(request) {
      Ok(ContentMessage::Offer {
        object,
        sequence,
        manifest,
        chunks,
      }) => ContentMessage::Missing {
        object,
        sequence,
        manifest,
        missing: self.missing_of(&chunks),
      }
      .encode(),
      Ok(ContentMessage::Put {
        object,
        sequence,
        archive,
      }) => match Archive::decode(&archive).map_err(ContentRefusal::Malformed) {
        Ok(archive) => match self.hold(archive) {
          Ok(manifest) => ContentMessage::Ack(ContentAck {
            holder,
            object,
            sequence,
            manifest,
          })
          .encode(),
          Err(_) => Vec::new(),
        },
        Err(_) => Vec::new(),
      },
      Ok(ContentMessage::Fetch { manifest }) => match self.archive_of(&manifest) {
        Some(archive) => ContentMessage::Have {
          archive: archive.encode(),
        }
        .encode(),
        None => Vec::new(),
      },
      _ => Vec::new(),
    }
  }
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
      let (reply, endpoint) = request_within(endpoint, stream, &bytes, deadline_ns).await;
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
) -> (Vec<HolderRequest>, Vec<(HostId, Endpoint)>) {
  let mut puts = Vec::new();
  let mut reusable = Vec::new();
  for Reply(host, reply, endpoint) in offered {
    match ContentMessage::decode(&reply) {
      Ok(ContentMessage::Missing {
        object: offered_object,
        sequence: offered_sequence,
        manifest: offered_manifest,
        missing,
      }) if offered_object == object
        && offered_sequence == sequence
        && offered_manifest == *manifest =>
      {
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
  (puts, reusable)
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
  let manifest = archive.manifest.identity();
  let mut acked: Vec<HostId> = if candidates.contains(&owner) {
    vec![owner]
  } else {
    Vec::new()
  };
  let build = |acked: &[HostId]| Placement {
    candidates: candidates.to_vec(),
    acked: acked.to_vec(),
    mirror_acked: None,
  };
  if is_placed(candidates, &acked, quorum) {
    return ContentPlaced {
      outcome: Ok(build(&acked)),
      reusable: Vec::new(),
      stragglers: Stragglers::none(),
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
  let offered = match dispatch_round(
    remote_holders
      .into_iter()
      .map(|(host, endpoint)| (host, endpoint, offer.clone()))
      .collect(),
    CONTENT_OFFER_STREAM,
    deadline_ns,
  ) {
    Ok(mut rx) => gather(&mut rx, budget).await,
    Err((error, mut rx)) => {
      // The tasks already started end with this task (children) after handing their sessions back.
      return ContentPlaced {
        outcome: Err(ClusterError::Runtime(error)),
        reusable: sessions_of(gather(&mut rx, budget).await),
        stragglers: Stragglers::none(),
      };
    }
  };

  // The put round: each holder that answered its offer gets the manifest and exactly its missing chunks.
  let (puts, mut reusable) = puts_for(archive, object, sequence, &manifest, offered);
  if puts.is_empty() {
    return ContentPlaced {
      outcome: Err(ClusterError::NotPlaced {
        placement: build(&acked),
      }),
      reusable,
      stragglers: Stragglers::none(),
    };
  }
  let mut rx = match dispatch_round(puts, CONTENT_PUT_STREAM, deadline_ns) {
    Ok(rx) => rx,
    Err((error, mut rx)) => {
      reusable.extend(sessions_of(gather(&mut rx, budget).await));
      return ContentPlaced {
        outcome: Err(ClusterError::Runtime(error)),
        reusable,
        stragglers: Stragglers::none(),
      };
    }
  };
  let (mut returned, timed_out) = collect_bound(
    &mut rx,
    candidates,
    quorum,
    budget,
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
  let stragglers = Stragglers::pending(rx);
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
  }
}

/// Fetches the whole archive of `manifest` from a recorded holder over `endpoint`, bounded by
/// `deadline_ns`, verified on arrival (the reader checks every chunk and the manifest hash, and the
/// manifest's identity must be the one asked for). Returns the archive, if the holder had it whole,
/// and the endpoint for reuse whatever the outcome.
pub async fn fetch_content(
  endpoint: Endpoint,
  manifest: [u8; 32],
  deadline_ns: u64,
) -> (Option<Archive>, Endpoint) {
  let request = ContentMessage::Fetch { manifest }.encode();
  let (reply, endpoint) =
    request_within(endpoint, CONTENT_FETCH_STREAM, &request, deadline_ns).await;
  let archive = match ContentMessage::decode(&reply) {
    Ok(ContentMessage::Have { archive }) => Archive::decode(&archive)
      .ok()
      .filter(|archive| archive.manifest.identity() == manifest),
    _ => None,
  };
  (archive, endpoint)
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
      manifest: Node::Directory(vec![Entry {
        name: "file".to_owned(),
        meta: NodeMeta::default(),
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
      ContentMessage::Fetch { manifest: hash(1) },
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
    let mut fetch = ContentMessage::Fetch { manifest: hash(1) }.encode();
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
    assert_eq!(hold.missing_of(&identities), identities);

    let partial = with_chunks(&archive, vec![archive.chunks[0].clone()]);
    assert_eq!(
      hold.hold(partial),
      Err(ContentRefusal::Incomplete { missing: 1 }),
      "a manifest referencing an unshipped, unheld chunk is not held"
    );
    assert_eq!(hold.chunk_count(), 0, "nothing stored on a refusal");

    let manifest = hold.hold(archive.clone()).unwrap();
    assert_eq!(manifest, archive.manifest.identity());
    assert!(hold.holds_manifest(&manifest));
    assert_eq!(hold.chunk_count(), 2);
    assert!(hold.missing_of(&identities).is_empty());
    let whole = hold.archive_of(&manifest).unwrap();
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
    // The holder already has the first chunk (say from an earlier snapshot).
    let mut earlier = with_chunks(&archive, vec![archive.chunks[0].clone()]);
    earlier.manifest = Node::File(vec![Extent {
      offset: 0,
      len: archive.chunks[0].raw_len,
      chunk: archive.chunks[0].identity,
      chunk_offset: 0,
    }]);
    hold.hold(earlier).unwrap();

    let offer = ContentMessage::Offer {
      object: OBJECT,
      sequence: SEQUENCE,
      manifest: archive.manifest.identity(),
      chunks: archive.chunks.iter().map(|c| c.identity).collect(),
    };
    let Ok(ContentMessage::Missing { missing, .. }) =
      ContentMessage::decode(&hold.serve(holder, &offer.encode()))
    else {
      panic!("an offer is answered with the missing set");
    };
    assert_eq!(missing, vec![archive.chunks[1].identity]);

    // A corrupt put: flip a bit in the missing chunk's payload.
    let mut corrupt = with_chunks(&archive, vec![archive.chunks[1].clone()]);
    corrupt.chunks[0].payload[0] ^= 0x01;
    let refused = hold.serve(
      holder,
      &ContentMessage::Put {
        object: OBJECT,
        sequence: SEQUENCE,
        archive: corrupt.encode(),
      }
      .encode(),
    );
    assert!(refused.is_empty(), "a corrupt chunk is refused, not held");
    assert_eq!(hold.chunk_count(), 1);

    let partial = with_chunks(&archive, vec![archive.chunks[1].clone()]);
    let reply = hold.serve(
      holder,
      &ContentMessage::Put {
        object: OBJECT,
        sequence: SEQUENCE,
        archive: partial.encode(),
      }
      .encode(),
    );
    let Ok(ContentMessage::Ack(ack)) = ContentMessage::decode(&reply) else {
      panic!("a complete put is acknowledged");
    };
    assert!(ack.binds(OBJECT, SEQUENCE, &archive.manifest.identity()));
    assert!(!ack.binds(OBJECT, SEQUENCE + 1, &archive.manifest.identity()));
    assert_eq!(ack.holder, holder);
    assert_eq!(hold.chunk_count(), 2, "exactly the missing chunk was added");

    let fetched = hold.serve(
      holder,
      &ContentMessage::Fetch {
        manifest: archive.manifest.identity(),
      }
      .encode(),
    );
    let Ok(ContentMessage::Have { archive: bytes }) = ContentMessage::decode(&fetched) else {
      panic!("a fetch is answered with the archive");
    };
    assert_eq!(Archive::decode(&bytes).unwrap(), archive);
    assert!(
      hold
        .serve(
          holder,
          &ContentMessage::Fetch {
            manifest: hash(0xCC)
          }
          .encode()
        )
        .is_empty(),
      "a manifest not held is an empty answer"
    );
  }
}
