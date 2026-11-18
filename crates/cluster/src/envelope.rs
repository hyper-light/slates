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
//!
//! A wrap is made a slice at a time ([`Wrapping`], A-323): the owner shard seals a seal's archive in turns of its step
//! quantum, so no turn seals more than one chunk past its share, whatever the volume's size. Until 2026-11-18 the whole
//! archive was wrapped in one turn: 98 ms for 64 MiB of 64 KiB chunks (`envelope_bench`, 2026-10-05), so at about a GiB
//! one turn ran past the daemon's watchdog window of a second, and a volume that large crash-looped its owner at every
//! seal. [`wrap`] is one whole advance and makes the very same envelope.
//!
//! An open is made a slice at a time the same way ([`Opening`], A-347): a takeover's successor opens a sealed volume's
//! envelope in turns of its owner shard's clock, a chunk at least a turn, and [`open`] is one whole advance.

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
  /// The wrap already ended: it made its envelope, or a refusal ended it ([`Wrapping::advance`]).
  Spent,
}

impl From<SealedError> for EnvelopeError {
  fn from(error: SealedError) -> EnvelopeError {
    EnvelopeError::Sealed(error)
  }
}

/// Wraps `archive` in an envelope (the module doc): every chunk sealed under `lineage` in segments of `segment`
/// plaintext bytes and named under `namer`, and the archive without its chunks sealed as one more. One whole advance of
/// a [`Wrapping`], so a wrap made in slices is this very envelope.
pub fn wrap(
  archive: &Archive,
  lineage: &WrappingKey,
  namer: &Namer,
  segment: u32,
) -> Result<Archive, EnvelopeError> {
  let mut wrapping = Wrapping::new(archive.clone(), segment);
  // Each advance seals at least one chunk or the manifest, so the loop ends: at once with the whole budget.
  loop {
    if let Some(envelope) = wrapping.advance(lineage, namer, u64::MAX)? {
      return Ok(envelope);
    }
  }
}

/// An envelope made a slice at a time (A-323; the module doc): the archive's chunks sealed in the order it lists them,
/// at least a byte budget's worth a slice ([`Wrapping::advance`]), a chunk whose keyed name is already sealed skipped,
/// then the archive without its chunks sealed last as the manifest. The order and every sealed byte are [`wrap`]'s, so
/// the envelope is the same however the slices fall. A refusal ends the wrap ([`EnvelopeError::Spent`] after it): its
/// maker starts again from the archive.
pub struct Wrapping {
  /// The archive being wrapped: its chunks are read in order and let go at the end, when the rest of it is sealed.
  archive: Archive,
  /// The index in the archive's chunk list of the next chunk to seal.
  next: usize,
  /// The keyed names already sealed: a chunk's keyed name depends on its identity alone, so a repeat is skipped before
  /// it is sealed.
  named: std::collections::BTreeSet<[u8; NAME]>,
  /// The envelope's entries and chunks so far.
  sealing: Sealing,
  /// The plaintext bytes each sealed segment carries.
  segment: u32,
  /// Whether the wrap has ended: its envelope made, or a refusal met.
  spent: bool,
}

impl Wrapping {
  /// Begins wrapping `archive` in segments of `segment` plaintext bytes; nothing is sealed until the first
  /// [`Self::advance`].
  pub fn new(archive: Archive, segment: u32) -> Wrapping {
    let capacity = archive.chunks.len().saturating_add(1);
    Wrapping {
      archive,
      next: 0,
      named: std::collections::BTreeSet::new(),
      sealing: Sealing {
        entries: Vec::with_capacity(capacity),
        chunks: Vec::with_capacity(capacity),
        stored: std::collections::BTreeSet::new(),
        largest: 0,
      },
      segment,
      spent: false,
    }
  }

  /// The archive's chunks not yet sealed or skipped: what the wrap has left before its manifest.
  pub fn chunks_left(&self) -> usize {
    self.archive.chunks.len().saturating_sub(self.next)
  }

  /// Seals the archive's next chunks under `lineage`, each named under `namer`, until this call has sealed at least
  /// `byte_budget` of their stored bytes (one chunk at least, so a wrap always moves on); once no chunk is left, the
  /// archive without its chunks as the manifest, and the envelope is returned. `None` while chunks remain. Every call
  /// passes the keys the first did. A refusal ends the wrap, and every call after an end is refused
  /// [`EnvelopeError::Spent`].
  pub fn advance(
    &mut self,
    lineage: &WrappingKey,
    namer: &Namer,
    byte_budget: u64,
  ) -> Result<Option<Archive>, EnvelopeError> {
    if self.spent {
      return Err(EnvelopeError::Spent);
    }
    let advanced = self.seal_chunks(lineage, namer, byte_budget);
    let made = match advanced {
      Ok(true) => self.finish(lineage, namer).map(Some),
      Ok(false) => return Ok(None),
      Err(error) => Err(error),
    };
    self.spent = true;
    made
  }

  /// Seals the next chunks until `byte_budget` of their stored bytes are sealed, one at least: whether none is left.
  fn seal_chunks(
    &mut self,
    lineage: &WrappingKey,
    namer: &Namer,
    byte_budget: u64,
  ) -> Result<bool, EnvelopeError> {
    let mut sealed_bytes: u64 = 0;
    let mut sealed_one = false;
    while let Some(chunk) = self.archive.chunks.get(self.next) {
      if sealed_one && sealed_bytes >= byte_budget {
        return Ok(false);
      }
      let keyed = namer
        .name(&chunk.identity)
        .map_err(|e| EnvelopeError::Sealed(SealedError::Seal(e)))?;
      if self.named.insert(keyed) {
        let sealed = seal_chunk(lineage, namer, chunk, self.segment)?;
        let len = u64::try_from(chunk.payload.len()).map_err(|_| EnvelopeError::TooLarge)?;
        sealed_bytes = sealed_bytes.saturating_add(len);
        sealed_one = true;
        let name = entry_name(CHUNK_PREFIX, &sealed.name);
        self.sealing.push(sealed, name)?;
      }
      self.next = self.next.saturating_add(1);
    }
    Ok(true)
  }

  /// Seals the archive without its chunks (its creation time zeroed, the module doc) as the manifest and makes the
  /// envelope; the archive's chunks are let go first, so the manifest is sealed without a copy of them.
  fn finish(&mut self, lineage: &WrappingKey, namer: &Namer) -> Result<Archive, EnvelopeError> {
    // The chunks are all sealed into the envelope by now; the plaintext copies go before the manifest is encoded.
    self.archive.chunks = Vec::new();
    let without_chunks = Archive {
      created_unix: 0,
      ..self.archive.clone()
    };
    let manifest = seal_chunk(
      lineage,
      namer,
      &Archive::raw_chunk(without_chunks.encode()),
      self.segment,
    )?;
    let name = entry_name(MANIFEST_PREFIX, &manifest.name);
    self.sealing.push(manifest, name)?;
    let Sealing {
      mut entries,
      chunks,
      largest,
      ..
    } = std::mem::take(&mut self.sealing);
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let largest = u32::try_from(largest).map_err(|_| EnvelopeError::TooLarge)?;
    let archive = &self.archive;
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
}

/// Opens `envelope` (the module doc): the real archive, every chunk its manifest references opened under `lineage`
/// and checked against its keyed name under `namer`, in the order the manifest first references them. Its creation
/// time is zero (the module doc). An archive that is not an envelope is refused [`EnvelopeError::Shape`]. One whole
/// advance of an [`Opening`], so an open made in slices is this very archive.
pub fn open(
  envelope: &Archive,
  lineage: &WrappingKey,
  namer: &Namer,
) -> Result<Archive, EnvelopeError> {
  let mut opening = Opening::new(envelope.clone(), lineage, namer)?;
  // Each advance opens at least one chunk, or returns the archive, so the loop ends: at once with the whole budget.
  loop {
    if let Some(archive) = opening.advance(lineage, namer, u64::MAX)? {
      return Ok(archive);
    }
  }
}

/// An envelope opened a slice at a time (A-347; the module doc): its sealed manifest at [`Opening::new`], then the chunks
/// the real manifest references, in the order it first references them, at least a byte budget's worth a slice
/// ([`Opening::advance`]), each checked against its keyed name; the archive once every one is open. The order and every
/// opened byte are [`open`]'s, so the archive is the same however the slices fall. A refusal ends the open
/// ([`EnvelopeError::Spent`] after it). Until 2026-11-18 a takeover opened a sealed volume's whole envelope in one turn
/// of the successor's owner shard, a seal's open and a BLAKE3 pass a byte: about a second a GiB, the watchdog's window.
pub struct Opening {
  /// The envelope: its entries name the sealed chunks, and its chunks hold them.
  envelope: Archive,
  /// Each of the envelope's chunks by identity, as its index in the envelope's chunk list, so an entry finds its chunk
  /// in log time (a scan per entry made opening `n` chunks cost `n²` comparisons).
  stored: std::collections::BTreeMap<[u8; 32], usize>,
  /// The real archive, its manifest opened; its chunks are set when the last is open, and it is handed back then.
  archive: Option<Archive>,
  /// The identities of the chunks the real manifest references, in the order it first references them.
  wanted: Vec<[u8; 32]>,
  /// The index in `wanted` of the next chunk to open.
  next: usize,
  /// The chunks opened so far, in `wanted`'s order.
  chunks: Vec<Chunk>,
  /// Whether the open has ended: its archive returned, or a refusal met.
  spent: bool,
}

impl Opening {
  /// Begins opening `envelope` under `lineage` and `namer`: checks it is an envelope of one sealed manifest and opens
  /// that manifest (the real archive without its chunks: its tree and metadata, never its content); nothing else is
  /// opened until the first [`Self::advance`]. Refused as [`open`] refuses.
  pub fn new(
    envelope: Archive,
    lineage: &WrappingKey,
    namer: &Namer,
  ) -> Result<Opening, EnvelopeError> {
    let Node::Directory(entries) = &envelope.manifest else {
      return Err(EnvelopeError::Shape);
    };
    if !is_envelope(&envelope) {
      return Err(EnvelopeError::Shape);
    }
    let mut manifests = entries
      .iter()
      .filter(|entry| entry.name.starts_with(MANIFEST_PREFIX));
    let manifest_entry = manifests.next().ok_or(EnvelopeError::Manifest)?;
    if manifests.next().is_some() {
      return Err(EnvelopeError::Manifest);
    }
    let stored: std::collections::BTreeMap<[u8; 32], usize> = envelope
      .chunks
      .iter()
      .enumerate()
      .map(|(at, chunk)| (chunk.identity, at))
      .collect();
    let manifest_chunk = open_entry(&envelope.chunks, &stored, manifest_entry, lineage, namer)?;
    let bytes = Archive::content(&manifest_chunk).map_err(|_| EnvelopeError::Malformed)?;
    let archive = Archive::decode(&bytes).map_err(|_| EnvelopeError::Malformed)?;
    let wanted: Vec<[u8; 32]> = archive
      .referenced_chunks()
      .into_iter()
      .filter(|identity| *identity != [0u8; 32])
      .collect();
    Ok(Opening {
      chunks: Vec::with_capacity(wanted.len()),
      envelope,
      stored,
      archive: Some(archive),
      wanted,
      next: 0,
      spent: false,
    })
  }

  /// The chunks not yet opened.
  pub fn chunks_left(&self) -> usize {
    self.wanted.len().saturating_sub(self.next)
  }

  /// Opens the next chunks under `lineage`, each checked against its keyed name under `namer`, until this call has
  /// opened at least `byte_budget` of their bytes (one chunk at least, so an open always moves on); once none is left,
  /// the archive is returned. `None` while chunks remain. Every call passes the keys [`Self::new`] did. A refusal ends
  /// the open, and every call after an end is refused [`EnvelopeError::Spent`].
  pub fn advance(
    &mut self,
    lineage: &WrappingKey,
    namer: &Namer,
    byte_budget: u64,
  ) -> Result<Option<Archive>, EnvelopeError> {
    if self.spent {
      return Err(EnvelopeError::Spent);
    }
    match self.open_chunks(lineage, namer, byte_budget) {
      Ok(false) => Ok(None),
      Ok(true) => {
        self.spent = true;
        let mut archive = self.archive.take().ok_or(EnvelopeError::Spent)?;
        archive.chunks = std::mem::take(&mut self.chunks);
        Ok(Some(archive))
      }
      Err(error) => {
        self.spent = true;
        Err(error)
      }
    }
  }

  /// Opens the next chunks until `byte_budget` of their bytes are open, one at least: whether none is left.
  fn open_chunks(
    &mut self,
    lineage: &WrappingKey,
    namer: &Namer,
    byte_budget: u64,
  ) -> Result<bool, EnvelopeError> {
    let Node::Directory(entries) = &self.envelope.manifest else {
      return Err(EnvelopeError::Shape);
    };
    let mut opened_bytes: u64 = 0;
    let mut opened_one = false;
    while let Some(identity) = self.wanted.get(self.next) {
      if opened_one && opened_bytes >= byte_budget {
        return Ok(false);
      }
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
      let chunk = open_entry(&self.envelope.chunks, &self.stored, entry, lineage, namer)?;
      if chunk.identity != *identity {
        return Err(EnvelopeError::Sealed(SealedError::Name));
      }
      let len = u64::try_from(chunk.payload.len()).map_err(|_| EnvelopeError::TooLarge)?;
      opened_bytes = opened_bytes.saturating_add(len);
      opened_one = true;
      self.chunks.push(chunk);
      self.next = self.next.saturating_add(1);
    }
    Ok(true)
  }
}

/// An envelope being made ([`Wrapping`]): its entries, its chunks, the identities of the chunks it already stores, and
/// the largest sealed chunk's length.
#[derive(Default)]
struct Sealing {
  entries: Vec<Entry>,
  chunks: Vec<Chunk>,
  /// The identities in `chunks`, so a sealed chunk already stored is found in log time: a scan of `chunks` per push
  /// made wrapping `n` chunks cost `n²` identity comparisons (about 1.3e8 at 1 GiB of 64 KiB chunks), in one turn of the
  /// owner shard.
  stored: std::collections::BTreeSet<[u8; 32]>,
  largest: u64,
}

impl Sealing {
  /// Adds one sealed chunk to the envelope: a file entry naming it, and its bytes as a raw chunk unless the envelope
  /// already stores a chunk of that identity (the manifest can seal to the bytes of a chunk only if it is that chunk's
  /// plaintext).
  fn push(&mut self, sealed: SealedChunk, name: String) -> Result<(), EnvelopeError> {
    let identity = sealed.sealed_hash();
    let len = u64::try_from(sealed.bytes.len()).map_err(|_| EnvelopeError::TooLarge)?;
    self.largest = self.largest.max(len);
    self.entries.push(Entry {
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
    if self.stored.insert(identity) {
      self.chunks.push(Archive::raw_chunk(sealed.bytes));
    }
    Ok(())
  }
}

/// The sealed chunk `entry` names, opened: the entry must be a file of one whole extent over one of the envelope's
/// chunks, and its name must carry the keyed name the opened chunk is checked against.
fn open_entry(
  chunks: &[Chunk],
  stored: &std::collections::BTreeMap<[u8; 32], usize>,
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
  let stored = stored
    .get(&extent.chunk)
    .and_then(|at| chunks.get(*at))
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
