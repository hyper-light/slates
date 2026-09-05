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
//! Each directory entry carries its child's per-node metadata (inode number, mode, modification
//! and change times, size, link count, and a flag for whether the child has extended attributes),
//! the field list of §2.6 item 4 (`research/compression-archive-dedup.md` §"Manifest"). The
//! metadata is written into both encodings and hashed into the Merkle identity, so a change to any
//! entry's mode or times changes the root identity exactly as a change to its content does. This
//! addition is why the format's minor version is 1 (the root identity of a v1.0 tree and a v1.1
//! tree of the same shape differ). The root directory has no entry naming it, so its own metadata is
//! not carried; every named node's is.
//!
//! Scope: the tree shape (directories, files, extents), each entry's metadata, and the Merkle
//! identity over both. Restoring the metadata onto a host path is the landing engine's job under a
//! grant (§4.15), not the archive's; the archive carries, hashes and round-trips it.

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

/// A named node's metadata (§2.6 item 4): the fields a restore needs to reproduce the entry and a
/// fingerprint needs to detect drift. Times are nanoseconds since the Unix epoch. `xattr_flags` is
/// nonzero when the node carries extended attributes (the attributes themselves are chunks, not
/// manifest bytes). Carried and hashed by the archive; applied to a host path only by a granted
/// landing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NodeMeta {
  /// The inode number the entry had in the volume (identity across renames, not a host inode).
  pub ino: u64,
  /// The permission and type bits.
  pub mode: u32,
  /// The modification time, nanoseconds since the Unix epoch.
  pub mtime_ns: u64,
  /// The change time, nanoseconds since the Unix epoch.
  pub ctime_ns: u64,
  /// The size in bytes (the file's length; a directory's is the archiver's own value).
  pub size: u64,
  /// The hard-link count.
  pub nlink: u32,
  /// Nonzero when the node has extended attributes.
  pub xattr_flags: u32,
}

/// One entry in a directory: a name, the child's metadata, and the child it points to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
  /// The entry's name (one path component).
  pub name: String,
  /// The child's per-node metadata.
  pub meta: NodeMeta,
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

/// Writes an entry's metadata fields in canonical order.
fn write_meta(writer: &mut Writer, meta: &NodeMeta) {
  writer.u64(meta.ino);
  writer.u32(meta.mode);
  writer.u64(meta.mtime_ns);
  writer.u64(meta.ctime_ns);
  writer.u64(meta.size);
  writer.u32(meta.nlink);
  writer.u32(meta.xattr_flags);
}

/// Reads an entry's metadata fields, refusing a truncated stream.
fn read_meta(reader: &mut Reader<'_>) -> Result<NodeMeta, ManifestError> {
  let ino = reader.u64().map_err(|_| ManifestError::Truncated)?;
  let mode = reader.u32().map_err(|_| ManifestError::Truncated)?;
  let mtime_ns = reader.u64().map_err(|_| ManifestError::Truncated)?;
  let ctime_ns = reader.u64().map_err(|_| ManifestError::Truncated)?;
  let size = reader.u64().map_err(|_| ManifestError::Truncated)?;
  let nlink = reader.u32().map_err(|_| ManifestError::Truncated)?;
  let xattr_flags = reader.u32().map_err(|_| ManifestError::Truncated)?;
  Ok(NodeMeta {
    ino,
    mode,
    mtime_ns,
    ctime_ns,
    size,
    nlink,
    xattr_flags,
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
          write_meta(&mut writer, &entry.meta);
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
          write_meta(writer, &entry.meta);
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
          let meta = read_meta(reader)?;
          let node = Node::read(reader)?;
          entries.push(Entry { name, meta, node });
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
