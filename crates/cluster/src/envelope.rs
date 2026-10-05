//! The envelope archive (A-92 piece 3b; hyper-raft `docs/seal.md` §7 and §13): how a sealed snapshot rides the content
//! plane unchanged.
//!
//! Before a put, the owner wraps the snapshot's archive in a synthetic one. Each of the real archive's chunks is sealed
//! ([`crate::sealed::seal_chunk`]) and its sealed bytes become a raw chunk of the envelope, so the chunk's identity is
//! the BLAKE3 of its ciphertext. The real archive without its chunks (its header fields, root metadata and manifest
//! tree) is sealed the same way as one more chunk. The envelope's clear tree has one file per sealed chunk, named
//! `c` and the chunk's keyed name in hex, and one named `m` and the manifest's keyed name in hex. Holders verify,
//! stage, retain, heal and serve the envelope as any archive, hold no key, and learn only counts, sizes and keyed names
//! (seal.md §13). Sealing is keyed by content, so equal chunks seal to equal bytes and holders still deduplicate.
//!
//! A reader opens the envelope with the lineage key and the tenant's naming key: the manifest entry first, then each
//! chunk the real manifest references, by its keyed name. Every sealed chunk is checked against its keyed name when it
//! is opened, so a holder cannot substitute one chunk for another, and every chunk the manifest needs must be present.
//! The envelope's own header carries nothing of the plaintext: no times, no name policy, no root metadata.
//!
//! An envelope says it is one in its root metadata ([`ENVELOPE_ROOT_INO`]), which the manifest identity a head pins
//! covers, so a holder cannot make a reader take an envelope for a plain archive or the reverse. Wrapping is
//! deterministic: the sealed manifest carries the archive's header with its creation time zeroed (informational, and
//! never part of the manifest identity), so a snapshot sealed again — the healer's re-offer — is the same envelope.

use hyper_seal::keys::WrappingKey;
use hyper_seal::name::{NAME, Namer};
use slates_archive::format::Chunk;
use slates_archive::{Archive, Entry, Extent, Node, NodeMeta};

use crate::sealed::{SealedChunk, SealedError, open_chunk, seal_chunk};

/// Format: the first character of a sealed chunk's entry name.
const CHUNK_PREFIX: char = 'c';
/// Format: the first character of the sealed manifest's entry name.
const MANIFEST_PREFIX: char = 'm';
/// Format: the radix of the keyed name's digits in an entry's name (lowercase hexadecimal).
const NAME_RADIX: u32 = 16;
/// Format: a regular file's type and permission bits for an envelope entry (owner read only; the bits are
/// informational, since no one restores an envelope).
const ENTRY_MODE: u32 = 0o100_400;

/// Format: the inode number an envelope's root metadata carries: a volume's root is never `u64::MAX` (inode numbers are
/// a volume prefix and a counter, D-4), so a plain archive never reads as an envelope.
pub const ENVELOPE_ROOT_INO: u64 = u64::MAX;

/// Whether `archive` is an envelope (its root metadata says so; the module doc).
pub fn is_envelope(archive: &Archive) -> bool {
  archive.root_meta.ino == ENVELOPE_ROOT_INO
}

/// A refusal to wrap or open an envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvelopeError {
  /// Sealing or opening one chunk refused.
  Sealed(SealedError),
  /// The envelope has no sealed manifest entry, or more than one.
  Manifest,
  /// An entry is not a file of one extent over one of the envelope's chunks.
  Shape,
  /// A chunk the real manifest references has no entry in the envelope.
  Missing,
  /// The opened manifest is not an archive.
  Malformed,
  /// A sealed chunk is larger than an archive's chunk can be.
  TooLarge,
}

impl From<SealedError> for EnvelopeError {
  fn from(error: SealedError) -> EnvelopeError {
    EnvelopeError::Sealed(error)
  }
}

/// Wraps `archive` in an envelope (the module doc): every chunk sealed under `lineage` in segments of `segment`
/// plaintext bytes and named under `namer`, and the archive without its chunks sealed as one more.
pub fn wrap(
  archive: &Archive,
  lineage: &WrappingKey,
  namer: &Namer,
  segment: u32,
) -> Result<Archive, EnvelopeError> {
  let mut entries = Vec::with_capacity(archive.chunks.len().saturating_add(1));
  let mut chunks = Vec::with_capacity(entries.capacity());
  let mut largest = 0u64;
  // A chunk's keyed name depends on its identity alone, so a repeat is skipped before it is sealed.
  let mut named = std::collections::BTreeSet::new();
  for chunk in &archive.chunks {
    let keyed = namer
      .name(&chunk.identity)
      .map_err(|e| EnvelopeError::Sealed(SealedError::Seal(e)))?;
    if !named.insert(keyed) {
      continue;
    }
    let sealed = seal_chunk(lineage, namer, chunk, segment)?;
    let name = entry_name(CHUNK_PREFIX, &sealed.name);
    push_sealed(sealed, name, &mut entries, &mut chunks, &mut largest)?;
  }
  let without_chunks = Archive {
    chunks: Vec::new(),
    created_unix: 0,
    ..archive.clone()
  };
  let manifest = seal_chunk(
    lineage,
    namer,
    &Archive::raw_chunk(without_chunks.encode()),
    segment,
  )?;
  let name = entry_name(MANIFEST_PREFIX, &manifest.name);
  push_sealed(manifest, name, &mut entries, &mut chunks, &mut largest)?;
  entries.sort_by(|a, b| a.name.cmp(&b.name));
  let largest = u32::try_from(largest).map_err(|_| EnvelopeError::TooLarge)?;
  Ok(Archive {
    base_page_size: archive.base_page_size,
    chunk_min: archive.chunk_min.min(largest),
    chunk_max: largest.max(archive.chunk_max),
    created_unix: 0,
    volume_id: archive.volume_id,
    snapshot_id: archive.snapshot_id,
    name_policy_id: 0,
    unicode_version: 0,
    root_meta: NodeMeta {
      ino: ENVELOPE_ROOT_INO,
      ..NodeMeta::default()
    },
    manifest: Node::Directory(entries),
    chunks,
  })
}

/// Opens `envelope` (the module doc): the real archive, every chunk its manifest references opened under `lineage`
/// and checked against its keyed name under `namer`, in the order the manifest first references them. Its creation
/// time is zero (the module doc). An archive that is not an envelope is refused [`EnvelopeError::Shape`].
pub fn open(
  envelope: &Archive,
  lineage: &WrappingKey,
  namer: &Namer,
) -> Result<Archive, EnvelopeError> {
  let Node::Directory(entries) = &envelope.manifest else {
    return Err(EnvelopeError::Shape);
  };
  if !is_envelope(envelope) {
    return Err(EnvelopeError::Shape);
  }
  let mut manifests = entries
    .iter()
    .filter(|entry| entry.name.starts_with(MANIFEST_PREFIX));
  let manifest_entry = manifests.next().ok_or(EnvelopeError::Manifest)?;
  if manifests.next().is_some() {
    return Err(EnvelopeError::Manifest);
  }
  let manifest_chunk = open_entry(envelope, manifest_entry, lineage, namer)?;
  let bytes = Archive::content(&manifest_chunk).map_err(|_| EnvelopeError::Malformed)?;
  let mut archive = Archive::decode(&bytes).map_err(|_| EnvelopeError::Malformed)?;
  let wanted = archive.referenced_chunks();
  let mut chunks = Vec::with_capacity(wanted.len());
  for identity in wanted.iter().filter(|identity| **identity != [0u8; 32]) {
    let name = entry_name(
      CHUNK_PREFIX,
      &namer
        .name(identity)
        .map_err(|e| EnvelopeError::Sealed(SealedError::Seal(e)))?,
    );
    let entry = entries
      .binary_search_by(|entry| entry.name.as_str().cmp(&name))
      .ok()
      .and_then(|at| entries.get(at))
      .ok_or(EnvelopeError::Missing)?;
    let chunk = open_entry(envelope, entry, lineage, namer)?;
    if chunk.identity != *identity {
      return Err(EnvelopeError::Sealed(SealedError::Name));
    }
    chunks.push(chunk);
  }
  archive.chunks = chunks;
  Ok(archive)
}

/// Adds one sealed chunk to the envelope: its bytes as a raw chunk, and a file entry naming it.
fn push_sealed(
  sealed: SealedChunk,
  name: String,
  entries: &mut Vec<Entry>,
  chunks: &mut Vec<Chunk>,
  largest: &mut u64,
) -> Result<(), EnvelopeError> {
  let identity = sealed.sealed_hash();
  let len = u64::try_from(sealed.bytes.len()).map_err(|_| EnvelopeError::TooLarge)?;
  *largest = (*largest).max(len);
  entries.push(Entry {
    name,
    meta: NodeMeta {
      mode: ENTRY_MODE,
      size: len,
      nlink: 1,
      ..NodeMeta::default()
    },
    node: Node::File(vec![Extent {
      offset: 0,
      len,
      chunk: identity,
      chunk_offset: 0,
    }]),
  });
  if !chunks
    .iter()
    .any(|chunk: &Chunk| chunk.identity == identity)
  {
    chunks.push(Archive::raw_chunk(sealed.bytes));
  }
  Ok(())
}

/// The sealed chunk `entry` names, opened: the entry must be a file of one whole extent over one of the envelope's
/// chunks, and its name must carry the keyed name the opened chunk is checked against.
fn open_entry(
  envelope: &Archive,
  entry: &Entry,
  lineage: &WrappingKey,
  namer: &Namer,
) -> Result<Chunk, EnvelopeError> {
  let Node::File(extents) = &entry.node else {
    return Err(EnvelopeError::Shape);
  };
  let [extent] = extents.as_slice() else {
    return Err(EnvelopeError::Shape);
  };
  let name = keyed_name(&entry.name).ok_or(EnvelopeError::Shape)?;
  let stored = envelope
    .chunks
    .iter()
    .find(|chunk| chunk.identity == extent.chunk)
    .ok_or(EnvelopeError::Missing)?;
  let bytes = Archive::content(stored).map_err(|_| EnvelopeError::Shape)?;
  if extent.offset != 0
    || extent.chunk_offset != 0
    || u64::try_from(bytes.len()).ok() != Some(extent.len)
  {
    return Err(EnvelopeError::Shape);
  }
  Ok(open_chunk(lineage, namer, &SealedChunk { name, bytes })?)
}

/// An entry's name: its prefix and the keyed name in lowercase hex.
fn entry_name(prefix: char, name: &[u8; NAME]) -> String {
  let mut out = String::with_capacity(NAME.saturating_mul(2).saturating_add(1));
  out.push(prefix);
  for byte in name {
    out.push_str(&format!("{byte:02x}"));
  }
  out
}

/// The keyed name an entry's name carries after its one-character prefix, if it is exactly that many hex digits.
fn keyed_name(entry: &str) -> Option<[u8; NAME]> {
  let digits = entry.get(1..)?;
  if digits.len() != NAME.saturating_mul(2) {
    return None;
  }
  let mut name = [0u8; NAME];
  for (at, byte) in name.iter_mut().enumerate() {
    let from = at.checked_mul(2)?;
    *byte = u8::from_str_radix(digits.get(from..from.checked_add(2)?)?, NAME_RADIX).ok()?;
  }
  Some(name)
}
