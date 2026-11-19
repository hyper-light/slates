//! The archive: build a snapshot into one self-verifying byte stream, and parse one back with
//! every integrity check (§2.6). The layout is header, then chunk records in manifest order, then
//! the manifest, then the seek table, then the trailer:
//!
//! - the **header** fixes the format, counts, identities and the manifest hash;
//! - each **chunk record** carries a BLAKE3 identity a reader verifies against the payload before
//!   any use, so a single flipped bit anywhere in a chunk is caught and named (AC-7.3);
//! - the **manifest** is the snapshot's tree, hashed to the identity the header declares;
//! - the **seek table** (a zstd skippable frame) maps each chunk's identity to its offset, so a
//!   reader locates a chunk without scanning;
//! - the **trailer** holds the section offsets, the BLAKE3 over the whole archive (so a truncated
//!   or altered stream is detected), and the tail magic.
//!
//! This module is pure: it builds and reads byte buffers. slates never writes an archive to disk
//! (R1); it lives in RAM, is handed to a caller, or is the replication/clone container.
//!
//! A reader decodes in one call ([`Archive::decode`]) or fed the stream a slice at a time ([`Decoding`], A-453, A-494):
//! each slice folded into the whole-archive hash and its records framed as it arrives, nothing held but what the
//! archive keeps and one fixed-width field, then the checks a piece at a time, so a shard decoding an archive a caller
//! handed it never holds the stream whole and gives its thread back between slices and pieces. The one call feeds the
//! whole stream and runs the pieces to their end, so one code path decodes either way.
//!
//! Scope: raw- and LZ4-encoded chunks, with the manifest a Merkle tree ([`crate::manifest`]).
//! zstd, dictionaries, the cost model and export/restore are later Phase 7 work (owed); the format
//! reserves their fields so an archive written then stays readable now.

use crate::format::{
  ArchiveError, Chunk, Encoding, FORMAT_MAJOR, FORMAT_MINOR, MAGIC, MAX_CHUNK_BYTES,
  SEEK_TABLE_MAGIC, TRAILER_MAGIC, flag,
};
use crate::manifest::{self, Node, NodeMeta};
use crate::stream::RECORD_HEADER_BYTES;
use crate::wire::{Reader, Writer};

/// Format: the header is a fixed set of little-endian fields (major 1 has no variable-length
/// header fields), so its length is the sum of the field sizes: magic, major, minor, header
/// length, flags, base page size, two chunk-size parameters, the manifest hash, the chunk count,
/// the raw and stored byte totals, the creation time, the volume and snapshot ids, and the
/// name-policy id and Unicode version.
const HEADER_BYTES: usize = size_of::<u32>()
  + size_of::<u16>()
  + size_of::<u16>()
  + size_of::<u32>()
  + size_of::<u32>()
  + size_of::<u32>()
  + size_of::<u32>()
  + size_of::<u32>()
  + size_of::<[u8; 32]>()
  + size_of::<u64>()
  + size_of::<u64>()
  + size_of::<u64>()
  + size_of::<u64>()
  + size_of::<u64>()
  + size_of::<u64>()
  + size_of::<u32>()
  + size_of::<u32>();

/// Format: [`HEADER_BYTES`] as a stream offset: where the chunk records begin.
pub(crate) const HEADER_LEN: u64 = HEADER_BYTES as u64;

/// Format: the trailer's section index: four `u64` offsets (the header's, the chunk records', the manifest's and the
/// seek table's), inside the whole-archive hash.
const SECTIONS_BYTES: usize =
  size_of::<u64>() + size_of::<u64>() + size_of::<u64>() + size_of::<u64>();

/// Format: the whole-archive BLAKE3 the trailer carries after its section index.
const HASH_BYTES: usize = size_of::<[u8; 32]>();

/// Format: the trailer magic, the stream's last bytes.
const MAGIC_BYTES: usize = size_of::<u32>();

/// Format: the bytes after the whole-archive hash's coverage: the hash itself and the magic.
const HASH_AND_MAGIC: u64 = (HASH_BYTES + MAGIC_BYTES) as u64;

/// Format: the trailer is the section index (four `u64` offsets), the whole-archive BLAKE3, and
/// the tail magic.
pub(crate) const TRAILER_LEN: u64 = (SECTIONS_BYTES + HASH_BYTES + MAGIC_BYTES) as u64;

/// The larger of two lengths, for a constant.
const fn larger(first: usize, second: usize) -> usize {
  if first > second { first } else { second }
}

/// Format: the longest fixed-width field a [`Decoding`] assembles before it parses it: the largest of the header, a
/// chunk record's fixed fields ([`RECORD_HEADER_BYTES`]), the manifest's length, the seek table's magic and the
/// trailer's three pieces. Its stage never holds more ([`Decoding::bound`]).
pub const LONGEST_FIELD: usize = larger(
  larger(HEADER_BYTES, RECORD_HEADER_BYTES),
  larger(
    larger(SECTIONS_BYTES, HASH_BYTES),
    larger(size_of::<u64>(), MAGIC_BYTES),
  ),
);

/// A snapshot ready to archive, or one parsed from an archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Archive {
  /// The base page size the volume was chunked at (informational for a reader).
  pub base_page_size: u32,
  /// The minimum chunk size parameter.
  pub chunk_min: u32,
  /// The maximum chunk size parameter.
  pub chunk_max: u32,
  /// The creation time (Unix seconds, informational).
  pub created_unix: u64,
  /// The volume identity.
  pub volume_id: u64,
  /// The snapshot identity.
  pub snapshot_id: u64,
  /// The name-equivalence policy id.
  pub name_policy_id: u32,
  /// The Unicode version the policy used.
  pub unicode_version: u32,
  /// The root directory's own metadata (its mode, owner, times): the tree's entries name every
  /// node but the root (format minor 2).
  pub root_meta: NodeMeta,
  /// The manifest tree (the snapshot's canonical, Merkle-hashed directory tree).
  pub manifest: Node,
  /// The chunks, in manifest order.
  pub chunks: Vec<Chunk>,
}

/// The BLAKE3 of `bytes`.
/// The content identity of `bytes` — the BLAKE3 of the raw bytes, the address every chunk is stored
/// and verified under, whatever encoding it is stored in.
pub fn hash_of(bytes: &[u8]) -> [u8; 32] {
  *blake3::hash(bytes).as_bytes()
}

impl Archive {
  /// A convenience: a chunk record for raw bytes, with the identity computed from them.
  pub fn raw_chunk(bytes: Vec<u8>) -> Chunk {
    let identity = hash_of(&bytes);
    let len = bytes.len() as u64;
    Chunk {
      identity,
      raw_len: len,
      stored_len: len,
      encoding: Encoding::Raw,
      level: 0,
      dictionary: [0u8; 32],
      payload: bytes,
    }
  }

  /// A chunk record with the compress-or-not decision (D-17): compress with LZ4 and keep the
  /// compressed form only when it is smaller than the raw bytes (the format-derived floor — a
  /// compressed chunk must save space); otherwise store raw. The identity is always the BLAKE3 of
  /// the raw bytes. The calibrated cost model, the Btrfs sampler, the LZ4-to-zstd regression and
  /// zstd are the rest of the codec pass (owed; GAPS §8g).
  pub fn compressed_chunk(bytes: Vec<u8>) -> Chunk {
    let identity = hash_of(&bytes);
    let raw_len = bytes.len() as u64;
    let compressed = lz4_flex::block::compress(&bytes);
    if (compressed.len() as u64) < raw_len {
      Chunk {
        identity,
        raw_len,
        stored_len: compressed.len() as u64,
        encoding: Encoding::Lz4,
        level: 0,
        dictionary: [0u8; 32],
        payload: compressed,
      }
    } else {
      Self::raw_chunk(bytes)
    }
  }

  /// A chunk record compressed with zstd (D-17's ratio-bearing codec) at the library's default level,
  /// kept only when it is smaller than the raw bytes (the format-derived floor); otherwise stored raw.
  /// The identity is always the BLAKE3 of the raw bytes. The calibrated per-chunk level is
  /// [`chunk_with`](Archive::chunk_with)'s (the cost model); this is the default-level form a caller
  /// without a policy uses. Behind the `zstd` feature (on by default).
  #[cfg(feature = "zstd")]
  pub fn zstd_chunk(bytes: Vec<u8>) -> Chunk {
    // Format: zstd level 0 is the library's default level.
    Self::zstd_chunk_at(bytes, 0)
  }

  /// A chunk record compressed with zstd at `level`, kept only when it is smaller than the raw bytes
  /// (the format floor); otherwise stored raw. The level is recorded on the chunk (the format's `level`
  /// byte; a level outside the byte is stored as its low byte — the reader needs no level to decode).
  #[cfg(feature = "zstd")]
  pub fn zstd_chunk_at(bytes: Vec<u8>, level: i32) -> Chunk {
    let identity = hash_of(&bytes);
    let raw_len = bytes.len() as u64;
    match zstd::bulk::compress(&bytes, level) {
      Ok(compressed) if (compressed.len() as u64) < raw_len => Chunk {
        identity,
        raw_len,
        stored_len: compressed.len() as u64,
        encoding: Encoding::Zstd,
        level: u8::try_from(level.max(0)).unwrap_or(u8::MAX),
        dictionary: [0u8; 32],
        payload: compressed,
      },
      _ => Self::raw_chunk(bytes),
    }
  }

  /// A chunk record under the **cost model** (§4.11, D-17; [`crate::codec::decide`]): the sampler,
  /// the LZ4 probe and the per-level prediction choose raw, LZ4 or a zstd level from the policy's
  /// measured points; the chosen codec's output is kept only when it is smaller than the raw bytes (the
  /// format floor, checked again on the real output — a prediction never stores a larger chunk). The
  /// identity is always the BLAKE3 of the raw bytes, so the decision never touches a manifest identity.
  /// A zstd verdict without the `zstd` feature falls to LZ4 (the probe's output, already computed).
  pub fn chunk_with(bytes: Vec<u8>, policy: &crate::codec::CodecPolicy) -> Chunk {
    let (verdict, probe) = crate::codec::decide(policy, &bytes);
    match verdict {
      crate::codec::Verdict::Raw => Self::raw_chunk(bytes),
      crate::codec::Verdict::Lz4 => Self::lz4_chunk_from(bytes, probe),
      #[cfg(feature = "zstd")]
      crate::codec::Verdict::Zstd(level) => Self::zstd_chunk_at(bytes, level),
      #[cfg(not(feature = "zstd"))]
      crate::codec::Verdict::Zstd(_) => Self::lz4_chunk_from(bytes, probe),
    }
  }

  /// An LZ4 chunk from the probe's output when the probe ran (so the bytes are not compressed twice),
  /// else compressed here; raw when the output does not save space.
  fn lz4_chunk_from(bytes: Vec<u8>, probe: Option<Vec<u8>>) -> Chunk {
    let identity = hash_of(&bytes);
    let raw_len = bytes.len() as u64;
    let compressed = probe.unwrap_or_else(|| lz4_flex::block::compress(&bytes));
    if (compressed.len() as u64) < raw_len {
      Chunk {
        identity,
        raw_len,
        stored_len: compressed.len() as u64,
        encoding: Encoding::Lz4,
        level: 0,
        dictionary: [0u8; 32],
        payload: compressed,
      }
    } else {
      Self::raw_chunk(bytes)
    }
  }

  /// The chunk's raw content, decoding the payload by its encoding. A raw chunk returns its bytes;
  /// an LZ4 or zstd chunk is decompressed to its declared raw length. A payload that will not
  /// decode, or whose decoded content fails the chunk's identity, is a typed refusal.
  pub fn content(chunk: &Chunk) -> Result<Vec<u8>, ArchiveError> {
    let bytes = decode_payload(chunk, 0)?;
    if hash_of(&bytes) != chunk.identity {
      return Err(ArchiveError::ChunkIdentityMismatch { index: 0 });
    }
    Ok(bytes)
  }

  /// Verifies `chunk` against its identity without allocating its content (AUD-29-43): a raw chunk is hashed
  /// where it is; an LZ4 or zstd chunk is decoded into `scratch` — which must hold its declared raw length —
  /// and hashed there. So a holder verifies a put in memory it has already admitted (one arena block for the
  /// largest encoded chunk) instead of a heap buffer per chunk. The refusals are [`Archive::content`]'s, plus
  /// `BadPayload` for a scratch too short to hold the content.
  pub fn verify_into(chunk: &Chunk, scratch: &mut [u8]) -> Result<(), ArchiveError> {
    let index = 0;
    if chunk.raw_len > MAX_CHUNK_BYTES {
      return Err(ArchiveError::ChunkTooLarge { index });
    }
    check_canonical_chunk(
      chunk.raw_len,
      u64::try_from(chunk.payload.len()).unwrap_or(u64::MAX),
      chunk.encoding,
      index,
    )?;
    let raw_len = usize::try_from(chunk.raw_len).map_err(|_| ArchiveError::BadPayload { index })?;
    let content: &[u8] = match chunk.encoding {
      Encoding::Raw => &chunk.payload,
      Encoding::Lz4 | Encoding::Zstd => {
        let out = scratch
          .get_mut(..raw_len)
          .ok_or(ArchiveError::BadPayload { index })?;
        let written = decode_payload_into(chunk, out, index)?;
        if written != raw_len {
          return Err(ArchiveError::BadPayload { index });
        }
        out
      }
    };
    if hash_of(content) != chunk.identity {
      return Err(ArchiveError::ChunkIdentityMismatch { index });
    }
    Ok(())
  }

  /// Encodes the archive to its byte stream. Deterministic: the same snapshot yields the same
  /// bytes on every platform (the identity gate).
  pub fn encode(&self) -> Vec<u8> {
    let mut manifest_bytes = manifest::encode_root_meta(&self.root_meta);
    manifest_bytes.extend_from_slice(&self.manifest.encode());
    let manifest_hash = self.manifest_identity();
    let raw_bytes: u64 = self.chunks.iter().map(|chunk| chunk.raw_len).sum();
    let stored_bytes: u64 = self.chunks.iter().map(|chunk| chunk.stored_len).sum();

    let mut writer = Writer::new();
    // Header.
    writer.u32(MAGIC);
    writer.u16(FORMAT_MAJOR);
    writer.u16(FORMAT_MINOR);
    writer.u32(u32::try_from(HEADER_LEN).unwrap_or(u32::MAX));
    writer.u32(flag::SEEK_TABLE);
    writer.u32(self.base_page_size);
    writer.u32(self.chunk_min);
    writer.u32(self.chunk_max);
    writer.hash(&manifest_hash);
    writer.u64(self.chunks.len() as u64);
    writer.u64(raw_bytes);
    writer.u64(stored_bytes);
    writer.u64(self.created_unix);
    writer.u64(self.volume_id);
    writer.u64(self.snapshot_id);
    writer.u32(self.name_policy_id);
    writer.u32(self.unicode_version);

    // Chunk records, remembering each record's offset for the seek table.
    let chunks_offset = writer.position();
    let mut seek: Vec<([u8; 32], u64)> = Vec::with_capacity(self.chunks.len());
    for chunk in &self.chunks {
      seek.push((chunk.identity, writer.position()));
      writer.hash(&chunk.identity);
      writer.u64(chunk.raw_len);
      writer.u64(chunk.stored_len);
      writer.u8(chunk.encoding.to_wire());
      writer.u8(chunk.level);
      writer.hash(&chunk.dictionary);
      writer.raw(&chunk.payload);
    }

    // Manifest.
    let manifest_offset = writer.position();
    writer.u64(manifest_bytes.len() as u64);
    writer.raw(&manifest_bytes);

    // Seek table: a zstd skippable frame (magic, frame size, then the entries).
    let seek_offset = writer.position();
    let mut frame = Writer::new();
    frame.u64(seek.len() as u64);
    for (identity, offset) in &seek {
      frame.hash(identity);
      frame.u64(*offset);
    }
    let frame_bytes = frame.finish();
    writer.u32(SEEK_TABLE_MAGIC);
    writer.u32(u32::try_from(frame_bytes.len()).unwrap_or(u32::MAX));
    writer.raw(&frame_bytes);

    // Trailer: the section index, the whole-archive hash over everything so far, then the magic.
    writer.u64(0);
    writer.u64(chunks_offset);
    writer.u64(manifest_offset);
    writer.u64(seek_offset);
    let archive_hash = hash_of(writer.as_slice());
    writer.hash(&archive_hash);
    writer.u32(TRAILER_MAGIC);
    writer.finish()
  }

  /// Parses and fully verifies an archive: the magic and major, the whole-archive hash (so a
  /// truncated or altered stream is refused), every chunk's identity, and the manifest's identity.
  /// A [`Decoding`] fed the whole stream and run to its end in one call, so one code path decodes either way (A-453,
  /// A-494).
  pub fn decode(bytes: &[u8]) -> Result<Archive, ArchiveError> {
    let len = u64::try_from(bytes.len()).map_err(|_| ArchiveError::BadLength)?;
    let mut decoding = Decoding::new(len);
    decoding.feed(bytes)?;
    // Stepped with no bound, a decoding fed whole runs to its archive or its refusal in this one call; `None` comes
    // back only from a decoding that already gave its archive, which this one has not.
    decoding.step(|| false)?.ok_or(ArchiveError::Truncated)
  }

  /// The identity the header pins for the manifest: the root's own metadata and the tree's Merkle
  /// identity together ([`manifest::manifest_identity`]).
  pub fn manifest_identity(&self) -> [u8; 32] {
    manifest::manifest_identity(&self.root_meta, &self.manifest)
  }

  /// The distinct chunks the archive's manifest references — file extents and every extended attribute's
  /// value extents, the root's included — in first-reference order ([`manifest::referenced_chunks`]).
  pub fn referenced_chunks(&self) -> Vec<[u8; 32]> {
    manifest::referenced_chunks(&self.root_meta, &self.manifest)
  }

  /// Reads one chunk by identity using the seek table, verifying its payload, without scanning the
  /// whole archive. Returns `None` when no chunk has that identity.
  pub fn chunk_by_identity(
    bytes: &[u8],
    identity: &[u8; 32],
  ) -> Result<Option<Chunk>, ArchiveError> {
    let header = Header::parse(bytes)?;
    verify_archive_hash(bytes)?;
    let sections = Sections::parse(bytes)?;
    let mut seek = Reader::at(bytes, sections.seek_offset)?;
    if seek.u32()? != SEEK_TABLE_MAGIC {
      return Err(ArchiveError::BadSeekTable);
    }
    let _frame_size = seek.u32()?;
    let count = seek.u64()?;
    for _ in 0..count {
      let entry_identity = seek.hash()?;
      let offset = seek.u64()?;
      if &entry_identity == identity {
        let mut reader = Reader::at(bytes, offset)?;
        return Ok(Some(read_chunk(&mut reader, 0, header.chunk_max)?));
      }
    }
    Ok(None)
  }
}

/// An archive parsed and verified from its stream fed a slice at a time, in order (A-453, A-494; CLAUDE.md "Bounded
/// work everywhere"), so a caller never holds the stream whole. Each chunk record's payload is copied into its own
/// chunk as it arrives and the manifest's bytes into the manifest's buffer, the parts the archive keeps; a fixed-width
/// field (the header, a record's fixed fields, the manifest's length, the seek table's magic, the trailer's pieces) is
/// assembled in a stage no longer than [`LONGEST_FIELD`]. [`Archive::decode`] feeds a whole stream in one call, so one
/// code path decodes either way.
///
/// Its checks are the whole decode's, in its order, with its refusals:
/// - fed ([`Decoding::feed`]): the header's fixed fields as soon as they are whole, so a foreign magic, an unknown
///   major or required flag and a declared chunk maximum past the format's cap are refused before a byte past the
///   header is taken; then the stream's length against the header and trailer it must hold. Each slice is folded into
///   the whole-archive BLAKE3 as it is fed. Each chunk record's declared sizes are admitted (its raw length against
///   the archive's chunk maximum, its canonical form, its payload against the stream left) before its payload's room
///   is allocated, and the manifest's length against the stream left before its own. A record or section that breaks
///   the format is kept, not refused at once, and the rest of the body is hashed through to the trailer: a stream the
///   checksum refuses is refused as altered, whatever framing its alteration broke;
/// - stepped once the stream is fed whole ([`Decoding::step`]), a piece at a time: the trailer's magic, the checksum
///   against the trailer's and where the records begin, one piece; then each chunk's payload decoded and held to its
///   identity, a chunk a piece, so nothing a record carries is decoded before the checksum holds (the hostile-input
///   rule, §4.9); then, one piece, the kept fault, the totals the header declares, the manifest decoded and held to
///   its identity, and the seek table's magic.
///
/// The layout is the one every writer makes ([`Archive::encode`], `StreamWriter`): the records from the header's end,
/// each section where the one before it ends, as the trailer's index names them. An index that names another place is
/// refused [`ArchiveError::BadLayout`] once the checksum holds.
///
/// A shard decoding an archive a caller handed it (`ArchiveImport`) feeds it a slice a unit and gives its thread back
/// between units and between pieces. Owed: the manifest's piece is the whole tree's decode and Merkle identity (GAPS
/// A-453), as the takeover's restore once was.
pub struct Decoding {
  /// The stream's whole length, as its holder admitted it.
  len: u64,
  /// The bytes fed so far: where the stream's next byte lies.
  fed: u64,
  /// The whole-archive BLAKE3, folded over every byte before the trailer's hash field as it is fed.
  hasher: Box<blake3::Hasher>,
  /// The header, once its fixed fields arrived whole.
  header: Option<Header>,
  /// The stage: a fixed-width field being assembled, never longer than [`LONGEST_FIELD`].
  field: Vec<u8>,
  /// What the stream holds at `fed`.
  frame: Frame,
  /// The chunk records framed so far, in the archive's order, their payloads not yet decoded.
  chunks: Vec<Chunk>,
  /// The raw and stored bytes of the records framed so far.
  totals: (u64, u64),
  /// The manifest's bytes, held whole while it is decoded (owed in pieces, GAPS A-453).
  manifest: Vec<u8>,
  /// Where the manifest section began, once the records were framed.
  manifest_at: Option<u64>,
  /// Where the seek table began, once the manifest was framed.
  seek_at: Option<u64>,
  /// The seek table's magic, once framed.
  seek_magic: Option<u32>,
  /// The trailer's section index, once fed.
  sections: Option<Sections>,
  /// The trailer's whole-archive hash, once fed.
  stored_hash: Option<[u8; 32]>,
  /// The trailer magic, once fed.
  magic: Option<u32>,
  /// The first record or section that broke the format, refused where the whole decode refuses it.
  fault: Option<(Section, ArchiveError)>,
  /// Where the checks stand once the stream is fed whole.
  check: Check,
  /// The stage's largest length so far.
  high_water: usize,
}

/// What a [`Decoding`] reads at the stream's next byte.
enum Frame {
  /// The header's fixed fields, assembled whole before they are parsed.
  Header,
  /// Chunk record `index`'s fixed fields, assembled whole before they are parsed.
  Record { index: u64 },
  /// Chunk record `index`'s payload, copied into its chunk to the stored length it declared.
  Payload { index: u64, chunk: Chunk },
  /// The manifest section's length.
  ManifestLength,
  /// The manifest's bytes, copied into the decoding's manifest buffer to `len`.
  Manifest { len: usize },
  /// The seek table's magic.
  SeekMagic,
  /// Bytes hashed and passed over up to the trailer: the seek table past its magic, or the body past a record or
  /// section that broke the format.
  Through,
  /// The trailer's section index, inside the hash.
  Sections,
  /// The trailer's whole-archive hash, outside it.
  Hash,
  /// The trailer magic.
  Magic,
  /// Every byte of the stream fed.
  Fed,
}

/// The part of the body a kept fault broke, so the checks refuse it where the whole decode does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
  /// A chunk record.
  Records,
  /// The manifest section.
  Manifest,
  /// The seek table.
  Seek,
}

/// Where a [`Decoding`]'s checks stand once the stream is fed whole.
#[derive(Clone, Copy)]
enum Check {
  /// The trailer's magic, the checksum and where the records begin.
  Trailer,
  /// Chunk `index`'s payload decoded and held to its identity.
  Chunk { index: usize },
  /// The kept fault, the totals, the manifest and the seek table.
  Tail,
  /// The archive given.
  Given,
}

impl Decoding {
  /// A decoding of a stream `len` bytes long, nothing fed yet.
  pub fn new(len: u64) -> Decoding {
    Decoding {
      len,
      fed: 0,
      hasher: Box::new(blake3::Hasher::new()),
      header: None,
      field: Vec::new(),
      frame: Frame::Header,
      chunks: Vec::new(),
      totals: (0, 0),
      manifest: Vec::new(),
      manifest_at: None,
      seek_at: None,
      seek_magic: None,
      sections: None,
      stored_hash: None,
      magic: None,
      fault: None,
      check: Check::Trailer,
      high_water: 0,
    }
  }

  /// The most a caller that feeds at most `feed_bytes` a call holds of the stream at once: its slice, and the one field
  /// the stage may hold beside it. What the archive keeps (each chunk's payload, the manifest's bytes until it is
  /// decoded) is not stage: it is the archive.
  pub fn bound(feed_bytes: usize) -> usize {
    feed_bytes.saturating_add(LONGEST_FIELD)
  }

  /// The manifest identity the header declares, once the header is fed: the archive's own once [`Decoding::step`] has
  /// given it, since the decode refuses a tree that does not hash to it.
  pub fn manifest_identity(&self) -> Option<[u8; 32]> {
    self.header.map(|header| header.manifest_hash)
  }

  /// The bytes fed so far: where the stream's next byte lies.
  pub fn fed(&self) -> u64 {
    self.fed
  }

  /// The bytes of the stream still to feed.
  pub fn remaining(&self) -> u64 {
    self.len.saturating_sub(self.fed)
  }

  /// The stage's length now: the part of a fixed-width field fed and not yet parsed.
  pub fn staged(&self) -> usize {
    self.field.len()
  }

  /// The stage's largest length so far, never past [`LONGEST_FIELD`].
  pub fn high_water(&self) -> usize {
    self.high_water
  }

  /// Takes the stream's next `bytes`, in order: folds them into the whole-archive hash and frames what they complete.
  /// Refused at once, typed: a header this reader does not read, a stream shorter than the header and trailer it must
  /// hold, a payload or manifest whose room the allocator refuses, and bytes past the stream's length (`BadLength`).
  /// A record or section that breaks the format is kept and refused by [`Decoding::step`] where the whole decode
  /// refuses it.
  pub fn feed(&mut self, bytes: &[u8]) -> Result<(), ArchiveError> {
    let count = u64::try_from(bytes.len()).map_err(|_| ArchiveError::BadLength)?;
    if count > self.remaining() {
      return Err(ArchiveError::BadLength);
    }
    let hashed = usize::try_from(self.hashed_end().saturating_sub(self.fed))
      .unwrap_or(usize::MAX)
      .min(bytes.len());
    self.hasher.update(bytes.get(..hashed).unwrap_or_default());
    let mut rest = bytes;
    loop {
      while self.wanted() == 0 && !matches!(self.frame, Frame::Fed) {
        self.advance()?;
      }
      if rest.is_empty() {
        return Ok(());
      }
      let taken = self.take(rest);
      if taken == 0 {
        // Only a decoding that refused already (its frame given up) takes nothing more.
        return Err(ArchiveError::BadLength);
      }
      rest = rest.get(taken..).unwrap_or_default();
    }
  }

  /// Checks the stream fed whole until `spent` says the step's share is spent, a piece at least: `Some` with the
  /// archive once every piece is done, and `None` after (the archive is given once). A stream fed short of its length
  /// is refused `Truncated`, as one that ends before its frame; a stream that does not decode is refused typed, as
  /// [`Archive::decode`] refuses it.
  pub fn step(&mut self, mut spent: impl FnMut() -> bool) -> Result<Option<Archive>, ArchiveError> {
    loop {
      if matches!(self.check, Check::Given) {
        return Ok(None);
      }
      if let Some(archive) = self.piece()? {
        return Ok(Some(archive));
      }
      if spent() {
        return Ok(None);
      }
    }
  }

  /// One piece of the checks; the archive once the last is done.
  fn piece(&mut self) -> Result<Option<Archive>, ArchiveError> {
    match self.check {
      Check::Trailer => {
        self.check_trailer()?;
        self.check = Check::Chunk { index: 0 };
      }
      Check::Chunk { index } => {
        self.check = match self.chunks.get(index) {
          Some(chunk) => {
            verify_chunk(chunk, u64::try_from(index).unwrap_or(u64::MAX))?;
            Check::Chunk {
              index: index.saturating_add(1),
            }
          }
          None => Check::Tail,
        };
      }
      Check::Tail => {
        let archive = self.tail()?;
        self.check = Check::Given;
        return Ok(Some(archive));
      }
      Check::Given => {}
    }
    Ok(None)
  }

  /// Where the whole-archive hash's coverage ends: before the trailer's hash field.
  fn hashed_end(&self) -> u64 {
    self.len.saturating_sub(HASH_AND_MAGIC)
  }

  /// Where the body (the records, the manifest and the seek table) ends: at the trailer.
  fn body_end(&self) -> u64 {
    self.len.saturating_sub(TRAILER_LEN)
  }

  /// The body's bytes still to come.
  fn body_room(&self) -> u64 {
    self.body_end().saturating_sub(self.fed)
  }

  /// The bytes the current frame still takes before it is whole: zero once it is (then it is parsed), or for a frame
  /// that takes none.
  fn wanted(&self) -> usize {
    let field = |size: usize| size.saturating_sub(self.field.len());
    match &self.frame {
      Frame::Header => field(HEADER_BYTES),
      Frame::Record { .. } => field(RECORD_HEADER_BYTES),
      Frame::Payload { chunk, .. } => usize::try_from(chunk.stored_len)
        .unwrap_or(usize::MAX)
        .saturating_sub(chunk.payload.len()),
      Frame::ManifestLength => field(size_of::<u64>()),
      Frame::Manifest { len } => len.saturating_sub(self.manifest.len()),
      Frame::SeekMagic | Frame::Magic => field(MAGIC_BYTES),
      Frame::Through => usize::try_from(self.body_room()).unwrap_or(usize::MAX),
      Frame::Sections => field(SECTIONS_BYTES),
      Frame::Hash => field(HASH_BYTES),
      Frame::Fed => 0,
    }
  }

  /// Takes as much of `bytes` as the current frame still wants into where it belongs: a payload into its chunk, the
  /// manifest's bytes into its buffer, a field into the stage, the rest passed over. The bytes taken.
  fn take(&mut self, bytes: &[u8]) -> usize {
    let count = self.wanted().min(bytes.len());
    let piece = bytes.get(..count).unwrap_or_default();
    match &mut self.frame {
      Frame::Payload { chunk, .. } => chunk.payload.extend_from_slice(piece),
      Frame::Manifest { .. } => self.manifest.extend_from_slice(piece),
      Frame::Through | Frame::Fed => {}
      Frame::Header
      | Frame::Record { .. }
      | Frame::ManifestLength
      | Frame::SeekMagic
      | Frame::Sections
      | Frame::Hash
      | Frame::Magic => {
        self.field.extend_from_slice(piece);
        self.high_water = self.high_water.max(self.field.len());
      }
    }
    self.fed = self
      .fed
      .saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
    count
  }

  /// Parses the frame just made whole and moves to the next.
  fn advance(&mut self) -> Result<(), ArchiveError> {
    let frame = std::mem::replace(&mut self.frame, Frame::Fed);
    self.frame = match frame {
      Frame::Header => self.after_header()?,
      Frame::Record { index } => self.after_record(index)?,
      Frame::Payload { index, chunk } => self.after_payload(index, chunk),
      Frame::ManifestLength => self.after_manifest_length()?,
      Frame::Manifest { .. } => self.after_manifest(),
      Frame::SeekMagic => {
        self.seek_magic = Some(Reader::new(&self.field).u32()?);
        self.field.clear();
        Frame::Through
      }
      Frame::Through => Frame::Sections,
      Frame::Sections => {
        self.sections = Some(Sections::from_index(&self.field)?);
        self.field.clear();
        Frame::Hash
      }
      Frame::Hash => {
        self.stored_hash = Some(Reader::new(&self.field).hash()?);
        self.field.clear();
        Frame::Magic
      }
      Frame::Magic => {
        self.magic = Some(Reader::new(&self.field).u32()?);
        self.field.clear();
        Frame::Fed
      }
      Frame::Fed => Frame::Fed,
    };
    Ok(())
  }

  /// The header parsed and the stream's length held to the header and trailer it must hold, both refused at once;
  /// then the first record, or the manifest of an archive with none.
  fn after_header(&mut self) -> Result<Frame, ArchiveError> {
    let header = Header::parse(&self.field)?;
    self.field.clear();
    if self.len < HEADER_LEN.saturating_add(TRAILER_LEN) {
      return Err(ArchiveError::Truncated);
    }
    self.header = Some(header);
    Ok(self.body_frame(0))
  }

  /// Record `index`'s fixed fields admitted in the whole decode's order — its encoding, its raw length against the
  /// chunk maximum, its canonical form, its payload against the body left — and its payload's room allocated; a record
  /// that breaks the format is kept.
  fn after_record(&mut self, index: u64) -> Result<Frame, ArchiveError> {
    let chunk_max = self.header.map_or(0, |header| header.chunk_max);
    let fields = record_fields(&self.field, index, chunk_max);
    self.field.clear();
    let mut chunk = match fields {
      Ok(chunk) => chunk,
      Err(fault) => return Ok(self.kept(Section::Records, fault)),
    };
    let Some(stored) = usize::try_from(chunk.stored_len)
      .ok()
      .filter(|_| chunk.stored_len <= self.body_room())
    else {
      return Ok(self.kept(Section::Records, ArchiveError::Truncated));
    };
    chunk
      .payload
      .try_reserve_exact(stored)
      .map_err(|_| ArchiveError::OverBudget {
        needed: chunk.stored_len,
        budget: 0,
      })?;
    Ok(Frame::Payload { index, chunk })
  }

  /// Record `index` whole: kept with its sizes counted; then the next record, or the manifest after the last.
  fn after_payload(&mut self, index: u64, chunk: Chunk) -> Frame {
    self.totals = (
      self.totals.0.saturating_add(chunk.raw_len),
      self.totals.1.saturating_add(chunk.stored_len),
    );
    self.chunks.push(chunk);
    self.body_frame(index.saturating_add(1))
  }

  /// The manifest's length admitted against the body left and its room allocated; a length past the body is kept.
  fn after_manifest_length(&mut self) -> Result<Frame, ArchiveError> {
    let declared = Reader::new(&self.field).u64()?;
    self.field.clear();
    let Some(len) = usize::try_from(declared)
      .ok()
      .filter(|_| declared <= self.body_room())
    else {
      return Ok(self.kept(Section::Manifest, ArchiveError::Truncated));
    };
    self
      .manifest
      .try_reserve_exact(len)
      .map_err(|_| ArchiveError::OverBudget {
        needed: declared,
        budget: 0,
      })?;
    Ok(Frame::Manifest { len })
  }

  /// The manifest whole: the seek table begins here.
  fn after_manifest(&mut self) -> Frame {
    self.seek_at = Some(self.fed);
    self.in_body(Frame::SeekMagic, MAGIC_BYTES, Section::Seek)
  }

  /// Record `index` next, or the manifest once every record the header counts is framed.
  fn body_frame(&mut self, index: u64) -> Frame {
    if index < self.header.map_or(0, |header| header.chunk_count) {
      return self.in_body(
        Frame::Record { index },
        RECORD_HEADER_BYTES,
        Section::Records,
      );
    }
    self.manifest_at = Some(self.fed);
    self.in_body(Frame::ManifestLength, size_of::<u64>(), Section::Manifest)
  }

  /// `frame`, a field of `size` bytes, when the body holds it; else a fault kept in `section`: the body ends first.
  fn in_body(&mut self, frame: Frame, size: usize, section: Section) -> Frame {
    if u64::try_from(size).unwrap_or(u64::MAX) <= self.body_room() {
      frame
    } else {
      self.kept(section, ArchiveError::Truncated)
    }
  }

  /// Keeps the first fault the body's framing met, in `section`; the rest of the body passes over to the trailer.
  fn kept(&mut self, section: Section, fault: ArchiveError) -> Frame {
    if self.fault.is_none() {
      self.fault = Some((section, fault));
    }
    Frame::Through
  }

  /// The kept fault, when it broke `section`.
  fn kept_in(&self, section: Section) -> Result<(), ArchiveError> {
    match self.fault {
      Some((at, fault)) if at == section => Err(fault),
      _ => Ok(()),
    }
  }

  /// The first piece of the checks: a stream that ended inside its header refused as its header is, one fed short of
  /// its length `Truncated`; then the trailer's magic, the checksum, and the records at the header's end.
  fn check_trailer(&mut self) -> Result<(), ArchiveError> {
    if self.header.is_none() {
      // A stream that ends inside its header: the header's own refusal, as a whole stream that short is refused.
      return Err(
        Header::parse(&self.field)
          .err()
          .unwrap_or(ArchiveError::Truncated),
      );
    }
    if !matches!(self.frame, Frame::Fed) {
      return Err(ArchiveError::Truncated);
    }
    if self.magic != Some(TRAILER_MAGIC) {
      return Err(ArchiveError::BadTrailer);
    }
    if Some(*self.hasher.finalize().as_bytes()) != self.stored_hash {
      return Err(ArchiveError::ArchiveHashMismatch);
    }
    if self.sections.map(|sections| sections.chunks_offset) != Some(HEADER_LEN) {
      return Err(ArchiveError::BadLayout);
    }
    Ok(())
  }

  /// The last piece of the checks, in the whole decode's order: a record's kept fault, the totals, the manifest's
  /// place and its kept fault, the manifest decoded and held to its identity, the seek table's place, its kept fault and
  /// its magic; then the archive.
  fn tail(&mut self) -> Result<Archive, ArchiveError> {
    let header = self.header.ok_or(ArchiveError::Truncated)?;
    let sections = self.sections.ok_or(ArchiveError::Truncated)?;
    self.kept_in(Section::Records)?;
    if self.totals != (header.raw_bytes, header.stored_bytes) {
      return Err(ArchiveError::TotalsMismatch);
    }
    if self.manifest_at != Some(sections.manifest_offset) {
      return Err(ArchiveError::BadLayout);
    }
    self.kept_in(Section::Manifest)?;
    let (root_meta, tree_bytes) =
      manifest::decode_root_meta(&self.manifest).map_err(|_| ArchiveError::ManifestHashMismatch)?;
    let manifest = Node::decode(tree_bytes).map_err(|_| ArchiveError::ManifestHashMismatch)?;
    if manifest::manifest_identity(&root_meta, &manifest) != header.manifest_hash {
      return Err(ArchiveError::ManifestHashMismatch);
    }
    if self.seek_at != Some(sections.seek_offset) {
      return Err(ArchiveError::BadLayout);
    }
    self.kept_in(Section::Seek)?;
    if self.seek_magic != Some(SEEK_TABLE_MAGIC) {
      return Err(ArchiveError::BadSeekTable);
    }
    self.manifest = Vec::new();
    Ok(Archive {
      base_page_size: header.base_page_size,
      chunk_min: header.chunk_min,
      chunk_max: header.chunk_max,
      created_unix: header.created_unix,
      volume_id: header.volume_id,
      snapshot_id: header.snapshot_id,
      name_policy_id: header.name_policy_id,
      unicode_version: header.unicode_version,
      root_meta,
      manifest,
      chunks: std::mem::take(&mut self.chunks),
    })
  }
}

/// The parsed header fields a reader needs.
#[derive(Clone, Copy)]
struct Header {
  base_page_size: u32,
  chunk_min: u32,
  chunk_max: u32,
  manifest_hash: [u8; 32],
  chunk_count: u64,
  raw_bytes: u64,
  stored_bytes: u64,
  created_unix: u64,
  volume_id: u64,
  snapshot_id: u64,
  name_policy_id: u32,
  unicode_version: u32,
}

impl Header {
  fn parse(bytes: &[u8]) -> Result<Header, ArchiveError> {
    let mut reader = Reader::new(bytes);
    if reader.u32()? != MAGIC {
      return Err(ArchiveError::BadMagic);
    }
    let major = reader.u16()?;
    if major != FORMAT_MAJOR {
      return Err(ArchiveError::UnsupportedMajor { found: major });
    }
    let _minor = reader.u16()?;
    let _header_len = reader.u32()?;
    let flags = reader.u32()?;
    if flags & !flag::KNOWN != 0 {
      return Err(ArchiveError::UnknownRequiredFlag {
        unknown: flags & !flag::KNOWN,
      });
    }
    let base_page_size = reader.u32()?;
    let chunk_min = reader.u32()?;
    let chunk_max = reader.u32()?;
    if u64::from(chunk_max) > MAX_CHUNK_BYTES {
      return Err(ArchiveError::ChunkMaxTooLarge { found: chunk_max });
    }
    let manifest_hash = reader.hash()?;
    let chunk_count = reader.u64()?;
    let raw_bytes = reader.u64()?;
    let stored_bytes = reader.u64()?;
    let created_unix = reader.u64()?;
    let volume_id = reader.u64()?;
    let snapshot_id = reader.u64()?;
    let name_policy_id = reader.u32()?;
    let unicode_version = reader.u32()?;
    Ok(Header {
      base_page_size,
      chunk_min,
      chunk_max,
      manifest_hash,
      chunk_count,
      raw_bytes,
      stored_bytes,
      created_unix,
      volume_id,
      snapshot_id,
      name_policy_id,
      unicode_version,
    })
  }
}

/// The trailer's section offsets.
#[derive(Clone, Copy)]
struct Sections {
  chunks_offset: u64,
  manifest_offset: u64,
  seek_offset: u64,
}

impl Sections {
  /// The section index of a whole stream, read where its trailer begins.
  fn parse(bytes: &[u8]) -> Result<Sections, ArchiveError> {
    let total = bytes.len() as u64;
    let trailer_start = total
      .checked_sub(TRAILER_LEN)
      .ok_or(ArchiveError::Truncated)?;
    let mut reader = Reader::at(bytes, trailer_start)?;
    Sections::from_index(reader.raw(SECTIONS_BYTES)?)
  }

  /// The section index read from its own bytes: the header's offset (always the stream's start, so not kept), then the
  /// chunk records', the manifest's and the seek table's.
  fn from_index(index: &[u8]) -> Result<Sections, ArchiveError> {
    let mut reader = Reader::new(index);
    let _header_offset = reader.u64()?;
    let chunks_offset = reader.u64()?;
    let manifest_offset = reader.u64()?;
    let seek_offset = reader.u64()?;
    Ok(Sections {
      chunks_offset,
      manifest_offset,
      seek_offset,
    })
  }
}

/// Where the whole-archive BLAKE3's coverage ends, the hash field's start, once the stream is checked long enough for a
/// header and a trailer (else `Truncated`) and its trailer magic checked (else `BadTrailer`).
fn hashed_end(bytes: &[u8]) -> Result<usize, ArchiveError> {
  let total = bytes.len() as u64;
  if total < HEADER_LEN + TRAILER_LEN {
    return Err(ArchiveError::Truncated);
  }
  let magic_at =
    usize::try_from(total.saturating_sub(u64::try_from(size_of::<u32>()).unwrap_or(0)))
      .unwrap_or(usize::MAX);
  let tail = bytes.get(magic_at..).ok_or(ArchiveError::Truncated)?;
  if u32::from_le_bytes(tail.try_into().unwrap_or_default()) != TRAILER_MAGIC {
    return Err(ArchiveError::BadTrailer);
  }
  // The hash covers everything up to the hash field (the section index included), which ends
  // `size_of::<[u8;32]>() + size_of::<u32>()` bytes before the end.
  let hash_field = size_of::<[u8; 32]>().saturating_add(size_of::<u32>());
  Ok(
    usize::try_from(total)
      .unwrap_or(usize::MAX)
      .saturating_sub(hash_field),
  )
}

/// Verifies the trailer magic and the whole-archive BLAKE3 (the bytes before the hash field).
fn verify_archive_hash(bytes: &[u8]) -> Result<(), ArchiveError> {
  let covered_end = hashed_end(bytes)?;
  let covered = bytes.get(..covered_end).ok_or(ArchiveError::Truncated)?;
  let stored = bytes
    .get(covered_end..covered_end.saturating_add(size_of::<[u8; 32]>()))
    .ok_or(ArchiveError::Truncated)?;
  if hash_of(covered).as_slice() != stored {
    return Err(ArchiveError::ArchiveHashMismatch);
  }
  Ok(())
}

/// Reads and verifies one chunk record at the reader's cursor, admitting its declared sizes before any
/// allocation (AUD-29-13, [`record_fields`]), then its payload held to its identity ([`verify_chunk`]).
fn read_chunk(reader: &mut Reader<'_>, index: u64, chunk_max: u32) -> Result<Chunk, ArchiveError> {
  let mut chunk = record_fields(reader.raw(RECORD_HEADER_BYTES)?, index, chunk_max)?;
  let stored = usize::try_from(chunk.stored_len).unwrap_or(usize::MAX);
  chunk.payload = reader.raw(stored)?.to_vec();
  verify_chunk(&chunk, index)?;
  Ok(chunk)
}

/// A chunk record's fixed fields read from `fields` and admitted in the order every reader admits them (AUD-29-13):
/// its encoding known, its raw length at most the archive's `chunk_max`, and its form canonical
/// ([`check_canonical_chunk`]), before a byte of its payload is read or room for it allocated. The chunk, its payload
/// still to come.
fn record_fields(fields: &[u8], index: u64, chunk_max: u32) -> Result<Chunk, ArchiveError> {
  let mut reader = Reader::new(fields);
  let identity = reader.hash()?;
  let raw_len = reader.u64()?;
  let stored_len = reader.u64()?;
  let encoding = Encoding::from_wire(reader.u8()?).ok_or(ArchiveError::BadLength)?;
  if raw_len > u64::from(chunk_max) {
    return Err(ArchiveError::ChunkTooLarge { index });
  }
  check_canonical_chunk(raw_len, stored_len, encoding, index)?;
  let level = reader.u8()?;
  let dictionary = reader.hash()?;
  Ok(Chunk {
    identity,
    raw_len,
    stored_len,
    encoding,
    level,
    dictionary,
    payload: Vec::new(),
  })
}

/// Holds a chunk's payload to its identity: a chunk is content-addressed by the BLAKE3 of its decoded bytes, so it is
/// decoded by its encoding (within the format's cap and canonical form; a raw chunk decodes to itself) and hashed.
fn verify_chunk(chunk: &Chunk, index: u64) -> Result<(), ArchiveError> {
  let content = decode_payload(chunk, index)?;
  if hash_of(&content) != chunk.identity {
    return Err(ArchiveError::ChunkIdentityMismatch { index });
  }
  Ok(())
}

/// Refuses a chunk record that is not canonical (AUD-29-13): an empty chunk (no extent can name one), a
/// raw chunk whose declared raw length is not its payload's, or an encoded chunk no smaller than its raw
/// bytes (the writer keeps an encoding only when it saves space, `Archive::compressed_chunk`).
fn check_canonical_chunk(
  raw_len: u64,
  stored_len: u64,
  encoding: Encoding,
  index: u64,
) -> Result<(), ArchiveError> {
  let canonical = raw_len > 0
    && match encoding {
      Encoding::Raw => stored_len == raw_len,
      Encoding::Lz4 | Encoding::Zstd => stored_len < raw_len,
    };
  if canonical {
    Ok(())
  } else {
    Err(ArchiveError::NonCanonicalChunk { index })
  }
}

/// Decodes a chunk's stored payload to its raw bytes by its encoding: a raw chunk is its bytes; an
/// LZ4 chunk is decompressed to its declared raw length; a zstd chunk is owed. A payload that will
/// not decode is a typed refusal naming the chunk, and no chunk decodes past the format's cap
/// ([`MAX_CHUNK_BYTES`]) or out of canonical form, whoever built the record (AUD-29-13).
fn decode_payload(chunk: &Chunk, index: u64) -> Result<Vec<u8>, ArchiveError> {
  if chunk.raw_len > MAX_CHUNK_BYTES {
    return Err(ArchiveError::ChunkTooLarge { index });
  }
  check_canonical_chunk(
    chunk.raw_len,
    u64::try_from(chunk.payload.len()).unwrap_or(u64::MAX),
    chunk.encoding,
    index,
  )?;
  match chunk.encoding {
    Encoding::Raw => Ok(chunk.payload.clone()),
    Encoding::Lz4 => {
      let raw_len = usize::try_from(chunk.raw_len).unwrap_or(usize::MAX);
      lz4_flex::block::decompress(&chunk.payload, raw_len)
        .map_err(|_| ArchiveError::BadPayload { index })
    }
    Encoding::Zstd => decode_zstd(chunk, index),
  }
}

/// Decodes an encoded chunk's payload into `out` (its declared raw length); returns the bytes written.
fn decode_payload_into(chunk: &Chunk, out: &mut [u8], index: u64) -> Result<usize, ArchiveError> {
  match chunk.encoding {
    Encoding::Raw => Err(ArchiveError::BadPayload { index }),
    Encoding::Lz4 => lz4_flex::block::decompress_into(&chunk.payload, out)
      .map_err(|_| ArchiveError::BadPayload { index }),
    Encoding::Zstd => decode_zstd_into(chunk, out, index),
  }
}

/// Decodes a zstd chunk into `out` (the `zstd` feature's C decoder).
#[cfg(feature = "zstd")]
fn decode_zstd_into(chunk: &Chunk, out: &mut [u8], index: u64) -> Result<usize, ArchiveError> {
  zstd::bulk::decompress_to_buffer(&chunk.payload, out)
    .map_err(|_| ArchiveError::BadPayload { index })
}

/// Without the `zstd` feature, a zstd chunk cannot be decoded (owed `ruzstd` fallback).
#[cfg(not(feature = "zstd"))]
fn decode_zstd_into(_chunk: &Chunk, _out: &mut [u8], index: u64) -> Result<usize, ArchiveError> {
  Err(ArchiveError::BadPayload { index })
}

/// Decodes a zstd chunk to its declared raw length. With the `zstd` feature (default) this is the
/// C-backed decoder; without it (a target with no C toolchain, e.g. the cross-compile lint gate)
/// zstd content cannot be decoded here and is a typed refusal — the `ruzstd` decode-only fallback is
/// owed (GAPS §8g).
#[cfg(feature = "zstd")]
fn decode_zstd(chunk: &Chunk, index: u64) -> Result<Vec<u8>, ArchiveError> {
  let raw_len = usize::try_from(chunk.raw_len).unwrap_or(usize::MAX);
  zstd::bulk::decompress(&chunk.payload, raw_len).map_err(|_| ArchiveError::BadPayload { index })
}

/// Without the `zstd` feature, a zstd chunk cannot be decoded (owed `ruzstd` fallback).
#[cfg(not(feature = "zstd"))]
fn decode_zstd(_chunk: &Chunk, index: u64) -> Result<Vec<u8>, ArchiveError> {
  Err(ArchiveError::BadPayload { index })
}

#[cfg(test)]
mod tests {
  use proptest::prelude::*;

  use super::*;
  use crate::manifest::{Entry, Extent};

  /// Shape: the chunk the test archives are cut at, small so a few kilobytes make many records and slices.
  const TEST_CHUNK: usize = 512;

  /// An archive of `files` in one directory, each cut into [`TEST_CHUNK`]-byte chunks, each LZ4-coded where that saves
  /// room, each distinct chunk kept once: what a writer makes, so its encoding is the oracle's input.
  fn archive_of(files: &[Vec<u8>]) -> Archive {
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut entries = Vec::new();
    for (number, bytes) in files.iter().enumerate() {
      let mut extents = Vec::new();
      for (piece, part) in bytes.chunks(TEST_CHUNK).enumerate() {
        let chunk = Archive::compressed_chunk(part.to_vec());
        extents.push(Extent {
          offset: (piece * TEST_CHUNK) as u64,
          len: part.len() as u64,
          chunk: chunk.identity,
          chunk_offset: 0,
        });
        if !chunks.iter().any(|kept| kept.identity == chunk.identity) {
          chunks.push(chunk);
        }
      }
      entries.push(Entry {
        name: format!("file-{number}"),
        meta: NodeMeta {
          mode: 0o100_644,
          size: bytes.len() as u64,
          nlink: 1,
          ..NodeMeta::default()
        },
        node: Node::File(extents),
      });
    }
    Archive {
      base_page_size: 4096,
      chunk_min: u32::try_from(TEST_CHUNK).unwrap(),
      chunk_max: u32::try_from(TEST_CHUNK).unwrap(),
      created_unix: 0,
      volume_id: 1,
      snapshot_id: 1,
      name_policy_id: 0,
      unicode_version: 0,
      root_meta: NodeMeta {
        mode: 0o040_755,
        ..NodeMeta::default()
      },
      manifest: Node::Directory(entries),
      chunks,
    }
  }

  /// `stream` fed in slices whose sizes cycle through `sizes`, the stage checked within one field after every slice,
  /// then checked a piece a step: the decode, and how many slices were fed.
  fn fed_in_slices(stream: &[u8], sizes: &[usize]) -> (Result<Archive, ArchiveError>, usize) {
    let mut decoding = Decoding::new(stream.len() as u64);
    let mut rest = stream;
    let mut feeds = 0;
    while !rest.is_empty() {
      let size = sizes[feeds % sizes.len()].clamp(1, rest.len());
      let (slice, after) = rest.split_at(size);
      feeds += 1;
      if let Err(refusal) = decoding.feed(slice) {
        return (Err(refusal), feeds);
      }
      assert!(
        decoding.staged() <= LONGEST_FIELD && decoding.high_water() <= LONGEST_FIELD,
        "the stage holds one field at most: {} now, {} at most",
        decoding.staged(),
        decoding.high_water()
      );
      rest = after;
    }
    loop {
      match decoding.step(|| true) {
        Ok(Some(archive)) => {
          assert!(
            matches!(decoding.step(|| true), Ok(None)),
            "the archive is given once"
          );
          return (Ok(archive), feeds);
        }
        Ok(None) => {}
        Err(refusal) => return (Err(refusal), feeds),
      }
    }
  }

  /// Noisy bytes no codec shrinks, from a xorshift seeded by `seed` (an odd seed: an even one starts as the odd one
  /// after it).
  fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
      .map(|_| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state.to_le_bytes()[0]
      })
      .collect()
  }

  proptest! {
    #![proptest_config(slates_test_seeds::seeded(proptest::test_runner::Config::default(), include_str!("archive.proptest-regressions")).unwrap())]

    /// A-494 (the oracle): do encode a generated archive (zero to three files of noisy or repeated bytes, raw and LZ4
    /// records) and feed its stream to a decoding in slices of generated sizes, whole, or cut short, or with one byte
    /// flipped; then check it a piece a step. Expect the whole stream to give the archive encoded, a cut or flipped
    /// one the very refusal the stream fed at once gives (typed, never the original), the stage within one field after
    /// every slice, and the whole stream fed in at least as many slices as the largest cuts it into (the sliced path
    /// ran; a refused stream stops feeding at its refusal, a header too short for its frame at the header's end).
    #[test]
    fn an_archive_fed_in_any_slices_decodes_as_its_whole_stream(
      files in prop::collection::vec((any::<bool>(), any::<u64>(), 0usize..1_500), 0..4),
      sizes in prop::collection::vec(1usize..300, 1..4),
      hostile in prop::option::of((any::<prop::sample::Index>(), any::<bool>())),
    ) {
      let files: Vec<Vec<u8>> = files
        .into_iter()
        .map(|(repeated, seed, len)| if repeated { vec![seed.to_le_bytes()[0]; len] } else { noise(seed, len) })
        .collect();
      let archive = archive_of(&files);
      let bytes = archive.encode();
      let stream = match hostile {
        None => bytes.clone(),
        Some((at, true)) => {
          let mut flipped = bytes.clone();
          flipped[at.index(bytes.len())] ^= 0x80;
          flipped
        }
        Some((at, false)) => bytes[..at.index(bytes.len())].to_vec(),
      };
      let (sliced, feeds) = fed_in_slices(&stream, &sizes);
      prop_assert_eq!(&sliced, &Archive::decode(&stream));
      if hostile.is_none() {
        prop_assert_eq!(&sliced, &Ok(archive));
      } else {
        prop_assert_ne!(&sliced, &Ok(archive));
      }
      if hostile.is_none() {
        let largest = *sizes.iter().max().unwrap();
        prop_assert!(feeds >= stream.len().div_ceil(largest), "{feeds} feeds for {} bytes", stream.len());
      }
    }
  }

  /// A-494 (the bound): do feed an archive more than a hundred times longer than the stage's bound at a 64-byte slice,
  /// a slice at a time (eight files of noisy bytes, raw records the length of a chunk). Expect the archive encoded, the
  /// stage never past one field (so a caller holding its slice holds `Decoding::bound` at most), and a feed for every
  /// slice the stream cuts into.
  #[test]
  fn a_stream_many_times_its_bound_is_fed_within_it() {
    let slice = 64;
    let files: Vec<Vec<u8>> = (0..8).map(|number| noise(2 * number + 1, 3_000)).collect();
    let archive = archive_of(&files);
    let bytes = archive.encode();
    let bound = Decoding::bound(slice);
    assert!(
      bytes.len() > 100 * bound,
      "{} bytes against a bound of {bound}",
      bytes.len()
    );
    let (decoded, feeds) = fed_in_slices(&bytes, &[slice]);
    assert_eq!(decoded, Ok(archive));
    assert_eq!(feeds, bytes.len().div_ceil(slice));
  }
}
