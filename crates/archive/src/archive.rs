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
//! Scope: raw- and LZ4-encoded chunks, with the manifest a Merkle tree ([`crate::manifest`]).
//! zstd, dictionaries, the cost model and export/restore are later Phase 7 work (owed); the format
//! reserves their fields so an archive written then stays readable now.

use crate::format::{
  ArchiveError, Chunk, Encoding, FORMAT_MAJOR, FORMAT_MINOR, MAGIC, SEEK_TABLE_MAGIC,
  TRAILER_MAGIC, flag,
};
use crate::manifest::Node;
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
  /// The manifest tree (the snapshot's canonical, Merkle-hashed directory tree).
  pub manifest: Node,
  /// The chunks, in manifest order.
  pub chunks: Vec<Chunk>,
}

/// The BLAKE3 of `bytes`.
fn hash_of(bytes: &[u8]) -> [u8; 32] {
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

  /// The chunk's raw content, decoding the payload by its encoding. A raw chunk returns its bytes;
  /// an LZ4 chunk is decompressed to its declared raw length. A payload that will not decode, or
  /// whose decoded content fails the chunk's identity, is a typed refusal.
  pub fn content(chunk: &Chunk) -> Result<Vec<u8>, ArchiveError> {
    let bytes = decode_payload(chunk, 0)?;
    if hash_of(&bytes) != chunk.identity {
      return Err(ArchiveError::ChunkIdentityMismatch { index: 0 });
    }
    Ok(bytes)
  }

  /// Encodes the archive to its byte stream. Deterministic: the same snapshot yields the same
  /// bytes on every platform (the identity gate).
  pub fn encode(&self) -> Vec<u8> {
    let manifest_bytes = self.manifest.encode();
    let manifest_hash = self.manifest.identity();
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
  pub fn decode(bytes: &[u8]) -> Result<Archive, ArchiveError> {
    let header = Header::parse(bytes)?;
    verify_archive_hash(bytes)?;
    let sections = Sections::parse(bytes)?;

    let mut reader = Reader::at(bytes, sections.chunks_offset)?;
    let mut chunks = Vec::new();
    for index in 0..header.chunk_count {
      let chunk = read_chunk(&mut reader, index)?;
      chunks.push(chunk);
    }

    let mut manifest_reader = Reader::at(bytes, sections.manifest_offset)?;
    let manifest_len = usize::try_from(manifest_reader.u64()?).unwrap_or(usize::MAX);
    let manifest_bytes = manifest_reader.raw(manifest_len)?;
    let manifest = Node::decode(manifest_bytes).map_err(|_| ArchiveError::ManifestHashMismatch)?;
    if manifest.identity() != header.manifest_hash {
      return Err(ArchiveError::ManifestHashMismatch);
    }

    // The seek table's magic is checked so a malformed one is refused.
    let mut seek_reader = Reader::at(bytes, sections.seek_offset)?;
    if seek_reader.u32()? != SEEK_TABLE_MAGIC {
      return Err(ArchiveError::BadSeekTable);
    }

    Ok(Archive {
      base_page_size: header.base_page_size,
      chunk_min: header.chunk_min,
      chunk_max: header.chunk_max,
      created_unix: header.created_unix,
      volume_id: header.volume_id,
      snapshot_id: header.snapshot_id,
      name_policy_id: header.name_policy_id,
      unicode_version: header.unicode_version,
      manifest,
      chunks,
    })
  }

  /// Reads one chunk by identity using the seek table, verifying its payload, without scanning the
  /// whole archive. Returns `None` when no chunk has that identity.
  pub fn chunk_by_identity(
    bytes: &[u8],
    identity: &[u8; 32],
  ) -> Result<Option<Chunk>, ArchiveError> {
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
        return Ok(Some(read_chunk(&mut reader, 0)?));
      }
    }
    Ok(None)
  }
}

/// The parsed header fields a reader needs.
struct Header {
  base_page_size: u32,
  chunk_min: u32,
  chunk_max: u32,
  manifest_hash: [u8; 32],
  chunk_count: u64,
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
    let manifest_hash = reader.hash()?;
    let chunk_count = reader.u64()?;
    let _raw_bytes = reader.u64()?;
    let _stored_bytes = reader.u64()?;
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
      created_unix,
      volume_id,
      snapshot_id,
      name_policy_id,
      unicode_version,
    })
  }
}

/// The trailer's section offsets.
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

/// Verifies the trailer magic and the whole-archive BLAKE3 (the bytes before the hash field).
fn verify_archive_hash(bytes: &[u8]) -> Result<(), ArchiveError> {
  let total = bytes.len() as u64;
  if total < HEADER_LEN + TRAILER_LEN {
    return Err(ArchiveError::Truncated);
  }
  let magic_at =
    usize::try_from(total - u64::try_from(size_of::<u32>()).unwrap_or(0)).unwrap_or(usize::MAX);
  let tail = bytes.get(magic_at..).ok_or(ArchiveError::Truncated)?;
  if u32::from_le_bytes(tail.try_into().unwrap_or_default()) != TRAILER_MAGIC {
    return Err(ArchiveError::BadTrailer);
  }
  // The hash covers everything up to the hash field (the section index included), which ends
  // `size_of::<[u8;32]>() + size_of::<u32>()` bytes before the end.
  let hash_field = size_of::<[u8; 32]>() + size_of::<u32>();
  let covered_end = usize::try_from(total)
    .unwrap_or(usize::MAX)
    .saturating_sub(hash_field);
  let covered = bytes.get(..covered_end).ok_or(ArchiveError::Truncated)?;
  let stored = bytes
    .get(covered_end..covered_end + size_of::<[u8; 32]>())
    .ok_or(ArchiveError::Truncated)?;
  if hash_of(covered).as_slice() != stored {
    return Err(ArchiveError::ArchiveHashMismatch);
  }
  Ok(())
}

/// Reads and verifies one chunk record at the reader's cursor.
fn read_chunk(reader: &mut Reader<'_>, index: u64) -> Result<Chunk, ArchiveError> {
  let identity = reader.hash()?;
  let raw_len = reader.u64()?;
  let stored_len = reader.u64()?;
  let encoding = Encoding::from_wire(reader.u8()?).ok_or(ArchiveError::BadLength)?;
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

/// Decodes a chunk's stored payload to its raw bytes by its encoding: a raw chunk is its bytes; an
/// LZ4 chunk is decompressed to its declared raw length; a zstd chunk is owed. A payload that will
/// not decode is a typed refusal naming the chunk.
fn decode_payload(chunk: &Chunk, index: u64) -> Result<Vec<u8>, ArchiveError> {
  match chunk.encoding {
    Encoding::Raw => Ok(chunk.payload.clone()),
    Encoding::Lz4 => {
      let raw_len = usize::try_from(chunk.raw_len).unwrap_or(usize::MAX);
      lz4_flex::block::decompress(&chunk.payload, raw_len)
        .map_err(|_| ArchiveError::BadPayload { index })
    }
    Encoding::Zstd => Err(ArchiveError::BadPayload { index }),
  }
}
