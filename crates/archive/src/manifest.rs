//! The archive's manifest tree (§2.6 item 4; D-17). An archive's manifest is a canonical,
//! sorted, Merkle-hashed directory tree: a directory node holds its entries (a name and a child)
//! sorted by name; a file node holds its extent list (offset, length, chunk identity, chunk
//! offset), with a hole written as a zero-chunk extent. Every node has a BLAKE3 identity computed
//! over an encoding that names its children *by identity*, so the root identity is a Merkle
//! fingerprint of the whole tree — the value the archive header records as the manifest hash, and
//! the value a reader recomputes to verify the tree.
//!
//! Two encodings, both canonical and deterministic: the Merkle encoding (children by identity)
//! defines the identity; the tree encoding (children inlined) is the bytes the archive stores and
//! a reader parses back. Sorting the directory entries makes both, and the identity, independent
//! of the order entries were added. Parsing is bounds-checked through [`crate::wire`], so a
//! truncated or malformed tree is a typed refusal, never a panic (§4.9). This module is pure: no
//! I/O, no clock, no randomness.
//!
//! Scope: the tree shape (directories, files, extents) and its Merkle identity. Per-node metadata
//! (mode, times, size, nlink, xattr flags) is a documented extension of the node encoding (owed;
//! GAPS §8g); adding it changes the node encoding and therefore the identity, so it is versioned
//! with the archive format.

use crate::wire::{Reader, Writer};

/// Format: a directory node's kind byte.
const KIND_DIRECTORY: u8 = 0;
/// Format: a file node's kind byte.
const KIND_FILE: u8 = 1;

/// One extent of a file: `len` bytes at `offset` in the file come from `chunk` at `chunk_offset`.
/// A hole is a zero-chunk extent (all-zero identity), which reads as zero bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
  /// The offset in the file.
  pub offset: u64,
  /// The length in bytes.
  pub len: u64,
  /// The chunk's content identity (all zeros for a hole).
  pub chunk: [u8; 32],
  /// The offset within the chunk.
  pub chunk_offset: u64,
}

/// One entry in a directory: a name and the child it points to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
  /// The entry's name (one path component).
  pub name: String,
  /// The child node.
  pub node: Node,
}

/// A manifest tree node: a directory of named entries, or a file of extents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
  /// A directory and its entries (canonicalized to sorted-by-name order).
  Directory(Vec<Entry>),
  /// A file and its extent list.
  File(Vec<Extent>),
}

/// A refusal from the manifest reader.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManifestError {
  /// The stream ends before a field the tree declares.
  Truncated,
  /// A node kind byte is neither a directory nor a file.
  BadKind {
    /// The byte found.
    found: u8,
  },
  /// A declared count or length is larger than the stream can hold, or a name is not UTF-8.
  BadNode,
}

impl core::fmt::Display for ManifestError {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    match self {
      Self::Truncated => f.write_str("manifest tree is truncated"),
      Self::BadKind { found } => write!(f, "manifest node has unknown kind {found}"),
      Self::BadNode => f.write_str("manifest node is malformed"),
    }
  }
}

impl std::error::Error for ManifestError {}

/// Writes an extent's fields.
fn write_extent(writer: &mut Writer, extent: &Extent) {
  writer.u64(extent.offset);
  writer.u64(extent.len);
  writer.hash(&extent.chunk);
  writer.u64(extent.chunk_offset);
}

/// Reads an extent's fields.
fn read_extent(reader: &mut Reader<'_>) -> Result<Extent, ManifestError> {
  let offset = reader.u64().map_err(|_| ManifestError::Truncated)?;
  let len = reader.u64().map_err(|_| ManifestError::Truncated)?;
  let chunk = reader.hash().map_err(|_| ManifestError::Truncated)?;
  let chunk_offset = reader.u64().map_err(|_| ManifestError::Truncated)?;
  Ok(Extent {
    offset,
    len,
    chunk,
    chunk_offset,
  })
}

/// The directory entries in canonical (sorted-by-name) order.
fn sorted_entries(entries: &[Entry]) -> Vec<&Entry> {
  let mut sorted: Vec<&Entry> = entries.iter().collect();
  sorted.sort_by(|a, b| a.name.cmp(&b.name));
  sorted
}

impl Node {
  /// The node's BLAKE3 identity: a Merkle hash over an encoding that names each child by its own
  /// identity, so the root identity fingerprints the whole tree and any change to any node changes
  /// it.
  pub fn identity(&self) -> [u8; 32] {
    let mut writer = Writer::new();
    match self {
      Node::Directory(entries) => {
        writer.u8(KIND_DIRECTORY);
        let sorted = sorted_entries(entries);
        writer.u64(sorted.len() as u64);
        for entry in sorted {
          writer.u32(u32::try_from(entry.name.len()).unwrap_or(u32::MAX));
          writer.raw(entry.name.as_bytes());
          // The child is named by its identity — the Merkle step.
          writer.hash(&entry.node.identity());
        }
      }
      Node::File(extents) => {
        writer.u8(KIND_FILE);
        writer.u64(extents.len() as u64);
        for extent in extents {
          write_extent(&mut writer, extent);
        }
      }
    }
    *blake3::hash(writer.as_slice()).as_bytes()
  }

  /// The canonical tree encoding: children are inlined (not by identity), so the whole tree round
  /// trips through [`Node::decode`]. Directory entries are written in sorted-by-name order.
  pub fn encode(&self) -> Vec<u8> {
    let mut writer = Writer::new();
    self.write(&mut writer);
    writer.finish()
  }

  /// Writes this node (and its children) into `writer` in canonical order.
  fn write(&self, writer: &mut Writer) {
    match self {
      Node::Directory(entries) => {
        writer.u8(KIND_DIRECTORY);
        let sorted = sorted_entries(entries);
        writer.u64(sorted.len() as u64);
        for entry in sorted {
          writer.u32(u32::try_from(entry.name.len()).unwrap_or(u32::MAX));
          writer.raw(entry.name.as_bytes());
          entry.node.write(writer);
        }
      }
      Node::File(extents) => {
        writer.u8(KIND_FILE);
        writer.u64(extents.len() as u64);
        for extent in extents {
          write_extent(writer, extent);
        }
      }
    }
  }

  /// Parses a tree from its canonical encoding, refusing a truncated or malformed one.
  pub fn decode(bytes: &[u8]) -> Result<Node, ManifestError> {
    let mut reader = Reader::new(bytes);
    Node::read(&mut reader)
  }

  /// Reads one node (and its children) from the reader.
  fn read(reader: &mut Reader<'_>) -> Result<Node, ManifestError> {
    let kind = reader.u8().map_err(|_| ManifestError::Truncated)?;
    match kind {
      KIND_DIRECTORY => {
        let count = reader.u64().map_err(|_| ManifestError::Truncated)?;
        let count = usize::try_from(count).map_err(|_| ManifestError::BadNode)?;
        // A directory of `count` entries needs at least `count` more bytes; refuse an absurd
        // count before allocating for it.
        if count > reader.remaining() {
          return Err(ManifestError::BadNode);
        }
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
          let name_len = reader.u32().map_err(|_| ManifestError::Truncated)?;
          let name_len = usize::try_from(name_len).map_err(|_| ManifestError::BadNode)?;
          let name_bytes = reader.raw(name_len).map_err(|_| ManifestError::Truncated)?;
          let name = std::str::from_utf8(name_bytes)
            .map_err(|_| ManifestError::BadNode)?
            .to_owned();
          let node = Node::read(reader)?;
          entries.push(Entry { name, node });
        }
        Ok(Node::Directory(entries))
      }
      KIND_FILE => {
        let count = reader.u64().map_err(|_| ManifestError::Truncated)?;
        let count = usize::try_from(count).map_err(|_| ManifestError::BadNode)?;
        if count > reader.remaining() {
          return Err(ManifestError::BadNode);
        }
        let mut extents = Vec::with_capacity(count);
        for _ in 0..count {
          extents.push(read_extent(reader)?);
        }
        Ok(Node::File(extents))
      }
      other => Err(ManifestError::BadKind { found: other }),
    }
  }
}
