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

use crate::archive::hash_of;
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
  /// The owner's uid, as the volume held it (POSIX ownership; a restore reproduces it, a granted
  /// landing applies it where the grant allows). Format minor 2.
  pub uid: u32,
  /// The owning group's gid, as the volume held it. Format minor 2.
  pub gid: u32,
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
  /// Bytes remain after the tree: the encoding is not exact (AUD-29-15).
  TrailingBytes,
  /// A name is not one valid path component: empty, `.` or `..`, holding a separator or a NUL, or longer
  /// than a host's component limit (AUD-29-15).
  BadName,
  /// A directory's names are not in strictly increasing order: unsorted, or a name repeated (AUD-29-15).
  Unordered,
  /// The tree nests deeper than any host path can reach (AUD-29-13).
  TooDeep,
  /// A file's extents do not tile it: a gap, an overlap, a misordering, an empty extent, a hole with a chunk
  /// offset, or an end past the offset range (AUD-29-14).
  BadExtents,
  /// A file's extents cover a length other than the size its metadata records (AUD-29-14).
  SizeMismatch,
}

impl core::fmt::Display for ManifestError {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    match self {
      Self::Truncated => f.write_str("manifest tree is truncated"),
      Self::BadKind { found } => write!(f, "manifest node has unknown kind {found}"),
      Self::BadNode => f.write_str("manifest node is malformed"),
      Self::TrailingBytes => f.write_str("manifest tree has trailing bytes"),
      Self::BadName => f.write_str("manifest entry name is not one valid component"),
      Self::Unordered => f.write_str("manifest directory names are unsorted or repeated"),
      Self::TooDeep => f.write_str("manifest tree nests past the path limit"),
      Self::BadExtents => f.write_str("manifest file extents do not tile the file"),
      Self::SizeMismatch => f.write_str("manifest file extents disagree with its recorded size"),
    }
  }
}

impl std::error::Error for ManifestError {}

/// Format: the longest path a supported host resolves (Linux's `PATH_MAX`, the largest of the supported
/// hosts' limits).
const PATH_MAX_BYTES: usize = 4096;
/// Derived: the deepest a manifest tree may nest — a path `d` levels deep needs at least `2d` bytes (a
/// one-byte name and a separator per level), so no host path reaches past `PATH_MAX_BYTES / 2` levels.
pub const MAX_DEPTH: usize = PATH_MAX_BYTES / 2;
/// Format: the longest one name component may be (`NAME_MAX` on every supported host).
pub const NAME_MAX_BYTES: usize = 255;

/// Format: one encoded extent's bytes — its offset, length, chunk identity and chunk offset
/// ([`write_extent`]).
const EXTENT_BYTES: usize =
  size_of::<u64>() + size_of::<u64>() + size_of::<[u8; 32]>() + size_of::<u64>();
/// Format: one encoded entry's metadata bytes ([`write_meta`]).
const META_BYTES: usize = size_of::<u64>() * 4 + size_of::<u32>() * 5;
/// Derived: the fewest bytes one directory entry encodes to — its name length (the name itself may be
/// empty in the grammar; validation refuses it by name), its metadata and its node's kind and count. A
/// directory's declared entry count past what the bytes left could hold at this size is refused before
/// anything is read (AUD-29-13).
const MIN_ENTRY_BYTES: usize = size_of::<u32>() + META_BYTES + size_of::<u8>() + size_of::<u64>();

/// Whether `name` is one valid path component: non-empty, not `.` or `..`, no separator (`/`, and `\`
/// for hosts that treat it as one) and no NUL, at most [`NAME_MAX_BYTES`] bytes.
pub fn valid_component(name: &str) -> bool {
  !name.is_empty()
    && name.len() <= NAME_MAX_BYTES
    && name != "."
    && name != ".."
    && !name.bytes().any(|byte| matches!(byte, b'/' | b'\\' | 0))
}

/// Checks that `extents` tile a file exactly (the canonical form, §2.6 D-17: holes are explicit zero-chunk
/// extents): each starts where the previous ended, from offset zero, none is empty, a hole names no chunk
/// offset, and no end passes the offset range. Returns the file length they cover.
pub fn tiled_length(extents: &[Extent]) -> Result<u64, ManifestError> {
  let mut end = 0u64;
  for extent in extents {
    let hole_offset = extent.chunk == [0u8; 32] && extent.chunk_offset != 0;
    if extent.offset != end || extent.len == 0 || hole_offset {
      return Err(ManifestError::BadExtents);
    }
    end = end
      .checked_add(extent.len)
      .ok_or(ManifestError::BadExtents)?;
  }
  Ok(end)
}

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
  writer.u32(meta.uid);
  writer.u32(meta.gid);
}

/// The root directory's own metadata, as the manifest section carries it ahead of the tree (format
/// minor 2): the tree's entries name every node but the root, and a clone or a takeover successor
/// rebuilds the root's mode, owner and times from this.
pub fn encode_root_meta(meta: &NodeMeta) -> Vec<u8> {
  let mut writer = Writer::new();
  write_meta(&mut writer, meta);
  writer.finish()
}

/// Reads the root's metadata from the head of a manifest section, returning it and the tree bytes
/// that follow.
pub fn decode_root_meta(bytes: &[u8]) -> Result<(NodeMeta, &[u8]), ManifestError> {
  let mut reader = Reader::new(bytes);
  let meta = read_meta(&mut reader)?;
  let consumed = usize::try_from(reader.position()).map_err(|_| ManifestError::Truncated)?;
  Ok((meta, bytes.get(consumed..).unwrap_or_default()))
}

/// The identity the archive header pins for its manifest: the root's own metadata and the tree's
/// Merkle identity, hashed together — so a change to the root's mode or owner changes the archive's
/// identity exactly as a change to any entry's does.
pub fn manifest_identity(root_meta: &NodeMeta, root: &Node) -> [u8; 32] {
  let mut writer = Writer::new();
  write_meta(&mut writer, root_meta);
  writer.hash(&root.identity());
  hash_of(writer.as_slice())
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
  let uid = reader.u32().map_err(|_| ManifestError::Truncated)?;
  let gid = reader.u32().map_err(|_| ManifestError::Truncated)?;
  Ok(NodeMeta {
    ino,
    mode,
    mtime_ns,
    ctime_ns,
    size,
    nlink,
    xattr_flags,
    uid,
    gid,
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

  /// Parses a tree from its canonical encoding — exactly: nothing may follow it, every name is one valid
  /// component, a directory's names strictly increase, every file's extents tile it and match its recorded
  /// size, and the tree nests no deeper than [`MAX_DEPTH`] (AUD-29-13–15). Iterative, so a deep tree costs a
  /// bounded explicit stack, never the thread's. A non-canonical encoding is refused typed, so every decoded
  /// tree is canonical and its identity unambiguous.
  pub fn decode(bytes: &[u8]) -> Result<Node, ManifestError> {
    let mut reader = Reader::new(bytes);
    let node = read_tree(&mut reader)?;
    if reader.remaining() != 0 {
      return Err(ManifestError::TrailingBytes);
    }
    Ok(node)
  }
}

/// A directory being read: the entry header naming it in its parent (none for the root), the entries read so
/// far, and how many remain.
struct Open {
  named: Option<(String, NodeMeta)>,
  entries: Vec<Entry>,
  remaining: usize,
}

/// What reading one node header produced: a whole file, or a directory whose entries follow.
enum Header {
  File(Vec<Extent>),
  Directory(usize),
}

/// Reads a node's kind and, for a file, its extents; for a directory, its entry count. Each count is
/// bounded by what the bytes left could encode at the item's smallest size, so a declared count never
/// reserves memory the stream does not back (AUD-29-13): a file's extents are reserved exactly, and a
/// directory reserves nothing ahead — its entries grow only as their bytes are read.
fn read_header(reader: &mut Reader<'_>) -> Result<Header, ManifestError> {
  let kind = reader.u8().map_err(|_| ManifestError::Truncated)?;
  if kind != KIND_DIRECTORY && kind != KIND_FILE {
    return Err(ManifestError::BadKind { found: kind });
  }
  let count = reader.u64().map_err(|_| ManifestError::Truncated)?;
  let count = usize::try_from(count).map_err(|_| ManifestError::BadNode)?;
  let item_bytes = if kind == KIND_FILE {
    EXTENT_BYTES
  } else {
    MIN_ENTRY_BYTES
  };
  if count > reader.remaining().checked_div(item_bytes).unwrap_or(0) {
    return Err(ManifestError::BadNode);
  }
  match kind {
    KIND_DIRECTORY => Ok(Header::Directory(count)),
    KIND_FILE => {
      let mut extents = Vec::with_capacity(count);
      for _ in 0..count {
        extents.push(read_extent(reader)?);
      }
      Ok(Header::File(extents))
    }
    other => Err(ManifestError::BadKind { found: other }),
  }
}

/// Reads one entry header (its name and metadata) of the directory on top of `stack`, checking the name is
/// a valid component greater than the entry before it.
fn read_entry_header(
  reader: &mut Reader<'_>,
  open: &Open,
) -> Result<(String, NodeMeta), ManifestError> {
  let name_len = reader.u32().map_err(|_| ManifestError::Truncated)?;
  let name_len = usize::try_from(name_len).map_err(|_| ManifestError::BadNode)?;
  let name_bytes = reader.raw(name_len).map_err(|_| ManifestError::Truncated)?;
  let name = std::str::from_utf8(name_bytes).map_err(|_| ManifestError::BadNode)?;
  if !valid_component(name) {
    return Err(ManifestError::BadName);
  }
  if open
    .entries
    .last()
    .is_some_and(|previous| previous.name.as_str() >= name)
  {
    return Err(ManifestError::Unordered);
  }
  let meta = read_meta(reader)?;
  Ok((name.to_owned(), meta))
}

/// Reads a whole tree iteratively (see [`Node::decode`]).
fn read_tree(reader: &mut Reader<'_>) -> Result<Node, ManifestError> {
  let mut stack: Vec<Open> = Vec::new();
  let mut named: Option<(String, NodeMeta)> = None;
  loop {
    let mut completed = match read_header(reader)? {
      Header::File(extents) => {
        let length = tiled_length(&extents)?;
        if named.as_ref().is_some_and(|(_, meta)| meta.size != length) {
          return Err(ManifestError::SizeMismatch);
        }
        Some((named.take(), Node::File(extents)))
      }
      Header::Directory(count) => {
        stack.push(Open {
          named: named.take(),
          entries: Vec::new(),
          remaining: count,
        });
        None
      }
    };
    // Attach what completed to its parent, closing every directory whose entries are all read; then read
    // the next entry header of the innermost directory still open.
    loop {
      if let Some((header, node)) = completed.take() {
        let Some((name, meta)) = header else {
          return Ok(node);
        };
        let open = stack.last_mut().ok_or(ManifestError::BadNode)?;
        open.entries.push(Entry { name, meta, node });
        open.remaining = open.remaining.saturating_sub(1);
      }
      let Some(open) = stack.last() else {
        return Err(ManifestError::BadNode);
      };
      if open.remaining > 0 {
        // The entry's path has one component per open directory below the root.
        if stack.len() > MAX_DEPTH {
          return Err(ManifestError::TooDeep);
        }
        named = Some(read_entry_header(reader, open)?);
        break;
      }
      let Some(closed) = stack.pop() else {
        return Err(ManifestError::BadNode);
      };
      completed = Some((closed.named, Node::Directory(closed.entries)));
    }
  }
}
