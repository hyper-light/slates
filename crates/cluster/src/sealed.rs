//! Sealed chunks (A-92 piece 3; hyper-raft `docs/seal.md` §4 and §7): the form a placed snapshot's chunk takes on a
//! holder that keeps ciphertext and holds no key.
//!
//! A chunk is written once and never rewritten, so it is sealed by STREAM under a data key of its own, wrapped by
//! the volume's lineage key in the sealed chunk's header (seal.md §4's rule for a file written once; the
//! version-keyed rule is for data overwritten in place, and needs an argument this piece does not need). What is
//! sealed is the chunk's whole content-plane record ([`crate::content::chunk_record`]): its identity, lengths,
//! encoding and payload, so no plaintext hash travels outside the seal.
//!
//! The data key is keyed by the record's content (hyper-seal `FileSealer::content_keyed`, seal.md §4): derived from
//! the lineage key and the BLAKE3 of exactly the record sealed, so the same record under the same lineage always seals
//! to the same bytes. Holders keep ciphertext and deduplicate by it, so equal content stays one sealed chunk without
//! the owner keeping a table of what it sealed (seal.md §7, "shared by reference, never sealed twice"). The key is the
//! record's hash, not the chunk's identity: one identity may be stored under more than one encoding, and keying by it
//! would seal two different plaintexts under one key and nonce. The sealed chunk is named by its keyed name,
//! HMAC-SHA-256 of the BLAKE3 identity under the tenant's naming key truncated to 128 bits (seal.md §7): equal names
//! within a tenant mean equal content, and a name confirms nothing to a holder without the key.
//!
//! A holder verifies a sealed chunk by the BLAKE3 of its sealed bytes ([`SealedChunk::sealed_hash`]), which the
//! sealed manifest's chunk table lists; it never opens one. An opener checks the data key's commitment before the
//! first segment (seal.md §2, against invisible-salamander keys), opens every segment at its index and only the last
//! as last (so a cut, extended, reordered or spliced chunk fails), verifies the decoded content against its identity,
//! and checks the keyed name is the identity's, so a holder cannot relabel one tenant's chunk as another.
//!
//! Layout: the STREAM header (`hyper_seal::stream::HEADER` bytes), then each segment's ciphertext followed by its
//! 16-byte tag; every segment but the last holds exactly the segment size the header records.

use hyper_seal::keys::WrappingKey;
use hyper_seal::name::{NAME, Namer};
use hyper_seal::stream::{FileOpener, FileSealer, HEADER, Header};
use hyper_seal::{SealError, TAG};
use slates_archive::Archive;
use slates_archive::format::Chunk;

use crate::content::{chunk_from_record, chunk_record};

/// A refusal to seal or open a chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SealedError {
  /// The seal itself refused (a key that would not unwrap or commit, a segment that would not open, a size outside
  /// STREAM's bounds): hyper-seal's typed error.
  Seal(SealError),
  /// The sealed bytes are shorter than a header and one tag, or end inside a segment's tag.
  Truncated,
  /// The opened bytes are not one whole chunk record.
  Malformed,
  /// The opened chunk's content does not hash to its identity.
  Identity,
  /// The sealed chunk's name is not its identity's keyed name.
  Name,
}

impl From<SealError> for SealedError {
  fn from(error: SealError) -> SealedError {
    SealedError::Seal(error)
  }
}

/// A chunk sealed for a holder: its keyed name and its sealed bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealedChunk {
  /// The keyed name: what holders file it under and what missing sets name.
  pub name: [u8; NAME],
  /// The STREAM header and the sealed segments.
  pub bytes: Vec<u8>,
}

impl SealedChunk {
  /// The BLAKE3 of the sealed bytes: what a holder verifies a put against (the sealed manifest's chunk table lists
  /// it), with no key.
  pub fn sealed_hash(&self) -> [u8; 32] {
    *blake3::hash(&self.bytes).as_bytes()
  }
}

/// Seals `chunk` under a data key derived from `lineage` and its record's BLAKE3 (the module doc), in segments of
/// `segment` plaintext bytes, named by `namer` (the tenant's naming key). The same chunk record always seals to the
/// same bytes.
pub fn seal_chunk(
  lineage: &WrappingKey,
  namer: &Namer,
  chunk: &Chunk,
  segment: u32,
) -> Result<SealedChunk, SealedError> {
  let record = chunk_record(chunk);
  let (mut sealer, header) =
    FileSealer::content_keyed(lineage, blake3::hash(&record).as_bytes(), segment)?;
  let size = usize::try_from(segment).map_err(|_| SealedError::Seal(SealError::Size))?;
  let segments = record.len().div_ceil(size).max(1);
  let mut bytes = Vec::with_capacity(
    HEADER
      .saturating_add(record.len())
      .saturating_add(segments.saturating_mul(TAG)),
  );
  bytes.extend_from_slice(&header.encode());
  let mut pieces = record.chunks(size).peekable();
  while let Some(piece) = pieces.next() {
    let last = pieces.peek().is_none();
    let at = bytes.len();
    bytes.extend_from_slice(piece);
    let sealed = bytes.get_mut(at..).ok_or(SealedError::Truncated)?;
    let tag = sealer.seal(sealed, last)?;
    bytes.extend_from_slice(&tag);
  }
  Ok(SealedChunk {
    name: namer.name(&chunk.identity)?,
    bytes,
  })
}

/// Opens `sealed` under `lineage`: the chunk, verified against its identity and its keyed name under `namer`.
pub fn open_chunk(
  lineage: &WrappingKey,
  namer: &Namer,
  sealed: &SealedChunk,
) -> Result<Chunk, SealedError> {
  let (head, mut body) = sealed
    .bytes
    .split_at_checked(HEADER)
    .ok_or(SealedError::Truncated)?;
  let header = Header::decode(head)?;
  let opener = FileOpener::new(lineage, &header)?;
  let size = usize::try_from(header.segment).map_err(|_| SealedError::Seal(SealError::Size))?;
  let stored = size.saturating_add(TAG);
  let mut record = Vec::with_capacity(body.len());
  let mut index = 0u64;
  loop {
    let last = body.len() <= stored;
    let (piece, rest) = body
      .split_at_checked(body.len().min(stored))
      .ok_or(SealedError::Truncated)?;
    let (ciphertext, tag) = piece
      .split_at_checked(piece.len().checked_sub(TAG).ok_or(SealedError::Truncated)?)
      .ok_or(SealedError::Truncated)?;
    let tag: &[u8; TAG] = tag.try_into().map_err(|_| SealedError::Truncated)?;
    let at = record.len();
    record.extend_from_slice(ciphertext);
    let opened = record.get_mut(at..).ok_or(SealedError::Truncated)?;
    opener.open(index, last, opened, tag)?;
    if last {
      break;
    }
    body = rest;
    index = index
      .checked_add(1)
      .ok_or(SealedError::Seal(SealError::Size))?;
  }
  let chunk = chunk_from_record(&record).map_err(|_| SealedError::Malformed)?;
  Archive::content(&chunk).map_err(|_| SealedError::Identity)?;
  if namer.name(&chunk.identity)? != sealed.name {
    return Err(SealedError::Name);
  }
  Ok(chunk)
}
