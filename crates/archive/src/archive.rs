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
//! A reader decodes in one call ([`Archive::decode`]) or a bounded piece at a time ([`Decoding`], A-453): the
//! whole-archive hash a slice a piece, then a chunk record a piece, the same checks in the same order, so a shard
//! decoding an archive a caller handed it gives its thread back between pieces. The one call is the pieces run to their
//! end, so one code path decodes either way.
//!
//! Scope: raw- and LZ4-encoded chunks, with the manifest a Merkle tree ([`crate::manifest`]).
//! zstd, dictionaries, the cost model and export/restore are later Phase 7 work (owed); the format
//! reserves their fields so an archive written then stays readable now.

use crate::format::{
  ArchiveError, Chunk, Encoding, FORMAT_MAJOR, FORMAT_MINOR, MAGIC, MAX_CHUNK_BYTES,
  SEEK_TABLE_MAGIC, TRAILER_MAGIC, flag,
};
use crate::manifest::{self, Node, NodeMeta};
use crate::wire::{Reader, Writer};

/// Format: the header is a fixed set of little-endian fields (major 1 has no variable-length
/// header fields), so its length is the sum of the field sizes: magic, major, minor, header
/// length, flags, base page size, two chunk-size parameters, the manifest hash, the chunk count,
/// the raw and stored byte totals, the creation time, the volume and snapshot ids, and the
/// name-policy id and Unicode version.
const HEADER_LEN: u64 = (size_of::<u32>()
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
  + size_of::<u32>()) as u64;

/// Format: the trailer is the section index (four `u64` offsets), the whole-archive BLAKE3, and
/// the tail magic.
const TRAILER_LEN: u64 = (size_of::<u64>()
  + size_of::<u64>()
  + size_of::<u64>()
  + size_of::<u64>()
  + size_of::<[u8; 32]>()
  + size_of::<u32>()) as u64;

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
  /// The [`Decoding`] run to its end in one call, so one code path decodes either way (A-453).
  pub fn decode(bytes: &[u8]) -> Result<Archive, ArchiveError> {
    // Stepped with no bound, a fresh decoding runs to its archive or its refusal in this one call; `None` comes back
    // only from a decoding that already gave its archive, which this one has not.
    Decoding::new(bytes)?
      .step(usize::MAX, || false)?
      .ok_or(ArchiveError::Truncated)
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

/// An archive parsed and verified a bounded piece at a time (A-453; CLAUDE.md "Bounded work everywhere"), as
/// [`Archive::decode`] parses it in one call, with the same checks in the same order and the same refusals:
/// - at once ([`Decoding::new`]), the header's fixed fields, so a foreign magic, an unknown major or required flag and
///   a declared chunk maximum past the format's cap are refused before a byte past the header is read, then the
///   stream's length against the header and trailer it must hold and the trailer's magic;
/// - the whole-archive BLAKE3, at most `slice_bytes` of the stream a piece, compared with the trailer's once whole, so
///   nothing past the header is decoded before the checksum holds (the hostile-input rule, §4.9);
/// - each chunk record a piece: its declared sizes admitted before its payload is copied, its payload decoded and held
///   to its identity, as a chunk is wherever it is read;
/// - the totals the header declares against the chunks' own;
/// - the manifest, decoded and held to the identity the header declares, and the seek table's magic, one piece.
///
/// A shard decoding an archive a caller handed it (`ArchiveImport`) gives its thread back between steps. Owed: the
/// manifest's piece is the whole tree's decode and Merkle identity (GAPS A-453), as the takeover's restore once was.
pub struct Decoding<'bytes> {
  bytes: &'bytes [u8],
  header: Header,
  stage: DecodeStage<'bytes>,
  /// The chunk records read so far, in the archive's order.
  chunks: Vec<Chunk>,
  /// The raw and stored bytes of the chunks read so far.
  totals: (u64, u64),
}

/// Where a [`Decoding`] stands.
enum DecodeStage<'bytes> {
  /// The whole-archive BLAKE3 over the stream before its hash field, hashed up to `at`, which ends at `end`.
  Hashing {
    hasher: Box<blake3::Hasher>,
    at: usize,
    end: usize,
  },
  /// The chunk records read, `index` of them so far, the reader at the next.
  Chunks {
    sections: Sections,
    reader: Reader<'bytes>,
    index: u64,
  },
  /// The manifest and the seek table.
  Manifest { sections: Sections },
  /// The archive given.
  Done,
}

impl<'bytes> Decoding<'bytes> {
  /// A decoding of `bytes`, its header parsed and its length and trailer magic checked: refused at once, typed, for a
  /// stream that is no archive this reader reads or is shorter than its frame.
  pub fn new(bytes: &'bytes [u8]) -> Result<Decoding<'bytes>, ArchiveError> {
    let header = Header::parse(bytes)?;
    let end = hashed_end(bytes)?;
    Ok(Decoding {
      bytes,
      header,
      stage: DecodeStage::Hashing {
        hasher: Box::new(blake3::Hasher::new()),
        at: 0,
        end,
      },
      chunks: Vec::new(),
      totals: (0, 0),
    })
  }

  /// The manifest identity the header declares: the archive's own once [`Decoding::step`] has returned it, since the
  /// decode refuses a tree that does not hash to it.
  pub fn manifest_identity(&self) -> [u8; 32] {
    self.header.manifest_hash
  }

  /// Decodes until `spent` says the step's share is spent, a piece at least, no piece hashing more than `slice_bytes`
  /// of the stream (a byte at least): `Some` with the archive once every piece is done, and `None` after (the archive
  /// is given once). A stream that does not decode is refused typed, as [`Archive::decode`] refuses it.
  pub fn step(
    &mut self,
    slice_bytes: usize,
    mut spent: impl FnMut() -> bool,
  ) -> Result<Option<Archive>, ArchiveError> {
    loop {
      if matches!(self.stage, DecodeStage::Done) {
        return Ok(None);
      }
      if let Some(archive) = self.piece(slice_bytes)? {
        return Ok(Some(archive));
      }
      if spent() {
        return Ok(None);
      }
    }
  }

  /// One piece of the decode; the archive once every piece is done.
  fn piece(&mut self, slice_bytes: usize) -> Result<Option<Archive>, ArchiveError> {
    match std::mem::replace(&mut self.stage, DecodeStage::Done) {
      DecodeStage::Hashing {
        mut hasher,
        at,
        end,
      } => {
        let stop = at.saturating_add(slice_bytes.max(1)).min(end);
        hasher.update(self.bytes.get(at..stop).ok_or(ArchiveError::Truncated)?);
        self.stage = if stop < end {
          DecodeStage::Hashing {
            hasher,
            at: stop,
            end,
          }
        } else {
          let stored = self
            .bytes
            .get(end..end.saturating_add(size_of::<[u8; 32]>()))
            .ok_or(ArchiveError::Truncated)?;
          if hasher.finalize().as_bytes().as_slice() != stored {
            return Err(ArchiveError::ArchiveHashMismatch);
          }
          let sections = Sections::parse(self.bytes)?;
          DecodeStage::Chunks {
            sections,
            reader: Reader::at(self.bytes, sections.chunks_offset)?,
            index: 0,
          }
        };
        Ok(None)
      }
      DecodeStage::Chunks {
        sections,
        mut reader,
        index,
      } => {
        if index >= self.header.chunk_count {
          if self.totals != (self.header.raw_bytes, self.header.stored_bytes) {
            return Err(ArchiveError::TotalsMismatch);
          }
          self.stage = DecodeStage::Manifest { sections };
          return Ok(None);
        }
        let chunk = read_chunk(&mut reader, index, self.header.chunk_max)?;
        self.totals = (
          self.totals.0.saturating_add(chunk.raw_len),
          self.totals.1.saturating_add(chunk.stored_len),
        );
        self.chunks.push(chunk);
        self.stage = DecodeStage::Chunks {
          sections,
          reader,
          index: index.saturating_add(1),
        };
        Ok(None)
      }
      DecodeStage::Manifest { sections } => self.manifest(sections).map(Some),
      DecodeStage::Done => Ok(None),
    }
  }

  /// The manifest decoded and held to the header's identity, the seek table's magic checked, and the archive.
  fn manifest(&mut self, sections: Sections) -> Result<Archive, ArchiveError> {
    let mut manifest_reader = Reader::at(self.bytes, sections.manifest_offset)?;
    let manifest_len = usize::try_from(manifest_reader.u64()?).unwrap_or(usize::MAX);
    let manifest_bytes = manifest_reader.raw(manifest_len)?;
    let (root_meta, tree_bytes) =
      manifest::decode_root_meta(manifest_bytes).map_err(|_| ArchiveError::ManifestHashMismatch)?;
    let manifest = Node::decode(tree_bytes).map_err(|_| ArchiveError::ManifestHashMismatch)?;
    if manifest::manifest_identity(&root_meta, &manifest) != self.header.manifest_hash {
      return Err(ArchiveError::ManifestHashMismatch);
    }
    // The seek table's magic is checked so a malformed one is refused.
    let mut seek_reader = Reader::at(self.bytes, sections.seek_offset)?;
    if seek_reader.u32()? != SEEK_TABLE_MAGIC {
      return Err(ArchiveError::BadSeekTable);
    }
    let header = &self.header;
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
  fn parse(bytes: &[u8]) -> Result<Sections, ArchiveError> {
    let total = bytes.len() as u64;
    let trailer_start = total
      .checked_sub(TRAILER_LEN)
      .ok_or(ArchiveError::Truncated)?;
    let mut reader = Reader::at(bytes, trailer_start)?;
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
/// allocation (AUD-29-13): at most the archive's `chunk_max` raw bytes, and in the canonical form
/// ([`check_canonical_chunk`]).
fn read_chunk(reader: &mut Reader<'_>, index: u64, chunk_max: u32) -> Result<Chunk, ArchiveError> {
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
  let stored = usize::try_from(stored_len).unwrap_or(usize::MAX);
  let payload = reader.raw(stored)?.to_vec();
  let chunk = Chunk {
    identity,
    raw_len,
    stored_len,
    encoding,
    level,
    dictionary,
    payload,
  };
  // A chunk is content-addressed by the BLAKE3 of its decoded bytes; decode and verify before any
  // use (a raw chunk decodes to itself).
  let content = decode_payload(&chunk, index)?;
  if hash_of(&content) != identity {
    return Err(ArchiveError::ChunkIdentityMismatch { index });
  }
  Ok(chunk)
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
