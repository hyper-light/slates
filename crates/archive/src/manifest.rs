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
//! Each directory entry carries its child's per-node metadata (inode number, mode, access,
//! modification, change and birth times as signed nanoseconds, size, link count, owner, and the
//! extended attributes — each a name and its value's extents over the archive's chunks), the field
//! list of §2.6 item 4 (`research/compression-archive-dedup.md` §"Manifest"). The metadata is written
//! into both encodings and hashed into the Merkle identity, so a change to any entry's mode, times or
//! attributes changes the root identity exactly as a change to its content does. Minor 1 added the
//! metadata, minor 2 the owner and the root's own metadata ahead of the tree, and minor 3 (2026-09-30,
//! AUD-29-56) the signed times, the access and birth times and the attribute values: before it an
//! archive carried only an "has attributes" flag (always written zero), so a placed snapshot could be
//! verified and served by a successor with its attributes and access times gone.
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

/// A node's metadata (§2.6 item 4; §4.5): every field a restore needs to reproduce the node as the volume
/// served it, and a fingerprint needs to detect drift. Times are signed nanoseconds since the Unix epoch,
/// as the volume holds them, so a pre-epoch time survives (format minor 3; before it negative times
/// clamped to zero and access and birth times were not carried, AUD-29-56). Every field, the extended
/// attributes' names and value extents included, is written into the manifest and hashed into its
/// identity, so two snapshots that differ only in an attribute or a time never share an identity.
/// Carried and hashed by the archive; applied to a host path only by a granted landing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeMeta {
  /// The inode number the entry had in the volume (identity across renames, not a host inode).
  pub ino: u64,
  /// The permission and type bits.
  pub mode: u32,
  /// The access time. Format minor 3.
  pub atime_ns: i64,
  /// The modification time.
  pub mtime_ns: i64,
  /// The change time.
  pub ctime_ns: i64,
  /// The birth (creation) time. Format minor 3.
  pub btime_ns: i64,
  /// The size in bytes (the file's length; a directory's is the archiver's own value).
  pub size: u64,
  /// The hard-link count.
  pub nlink: u32,
  /// The owner's uid, as the volume held it (POSIX ownership; a restore reproduces it, a granted
  /// landing applies it where the grant allows). Format minor 2.
  pub uid: u32,
  /// The owning group's gid, as the volume held it. Format minor 2.
  pub gid: u32,
  /// The node's extended attributes, each a name and its value's extents over the archive's chunks
  /// (format minor 3). Written in strictly increasing name order; a reader refuses any other order.
  pub xattrs: Vec<Xattr>,
}

/// One extended attribute (§4.5 "Extended attributes"; format minor 3): its name and its value. The value
/// is content like a file's (the volume holds it as an attribute inode's body), so it is carried the
/// same way, as extents over content-addressed chunks: a large value (a macOS resource fork) costs its
/// chunks once and deduplicates with any file holding the same bytes. The value's length is the length
/// its extents tile; no separate size is written, so the two can never disagree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Xattr {
  /// The attribute's name: 1 to [`XATTR_NAME_MAX_BYTES`] bytes, no NUL.
  pub name: Vec<u8>,
  /// The value's extents, tiling it from offset zero (empty for an empty value).
  pub extents: Vec<Extent>,
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
  /// An extended attribute's name is empty, longer than [`XATTR_NAME_MAX_BYTES`] or holds a NUL; or a
  /// node's attribute names are not in strictly increasing order (AUD-29-56).
  BadXattr,
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
      Self::BadXattr => f.write_str("manifest extended attribute is malformed or out of order"),
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
/// Format: the longest extended-attribute name any supported host accepts: Linux's `XATTR_NAME_MAX` (255,
/// `include/uapi/linux/limits.h`), the largest of the hosts slates serves (macOS's `XATTR_MAXNAMELEN` is
/// 127). The volume core takes its own limit from this, so a volume can never hold a name its archive
/// refuses.
pub const XATTR_NAME_MAX_BYTES: usize = 255;
/// Format: one encoded entry's fixed metadata bytes ([`write_meta`]), field by field in write order; the
/// attributes follow the count.
const META_BYTES: usize = size_of::<u64>() // inode number
  + size_of::<u32>() // mode
  + size_of::<i64>() // access time
  + size_of::<i64>() // modification time
  + size_of::<i64>() // change time
  + size_of::<i64>() // birth time
  + size_of::<u64>() // size
  + size_of::<u32>() // link count
  + size_of::<u32>() // owner
  + size_of::<u32>() // group
  + size_of::<u32>(); // attribute count
/// Derived: the fewest bytes one extended attribute encodes to — its name length, a one-byte name (an
/// empty name is refused) and its extent count. A node's declared attribute count past what the bytes
/// left could hold at this size is refused before anything is reserved.
const MIN_XATTR_BYTES: usize = size_of::<u32>() + size_of::<u8>() + size_of::<u64>();
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

/// Writes a node's metadata fields in canonical order, its extended attributes sorted by name.
fn write_meta(writer: &mut Writer, meta: &NodeMeta) {
  writer.u64(meta.ino);
  writer.u32(meta.mode);
  writer.i64(meta.atime_ns);
  writer.i64(meta.mtime_ns);
  writer.i64(meta.ctime_ns);
  writer.i64(meta.btime_ns);
  writer.u64(meta.size);
  writer.u32(meta.nlink);
  writer.u32(meta.uid);
  writer.u32(meta.gid);
  let mut xattrs: Vec<&Xattr> = meta.xattrs.iter().collect();
  xattrs.sort_by(|a, b| a.name.cmp(&b.name));
  writer.u32(u32::try_from(xattrs.len()).unwrap_or(u32::MAX));
  for xattr in xattrs {
    writer.u32(u32::try_from(xattr.name.len()).unwrap_or(u32::MAX));
    writer.raw(&xattr.name);
    writer.u64(xattr.extents.len() as u64);
    for extent in &xattr.extents {
      write_extent(writer, extent);
    }
  }
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

/// Reads a node's metadata fields, refusing a truncated stream, and its extended attributes, refusing a
/// malformed or misordered one.
fn read_meta(reader: &mut Reader<'_>) -> Result<NodeMeta, ManifestError> {
  let truncated = |_| ManifestError::Truncated;
  let ino = reader.u64().map_err(truncated)?;
  let mode = reader.u32().map_err(truncated)?;
  let atime_ns = reader.i64().map_err(truncated)?;
  let mtime_ns = reader.i64().map_err(truncated)?;
  let ctime_ns = reader.i64().map_err(truncated)?;
  let btime_ns = reader.i64().map_err(truncated)?;
  let size = reader.u64().map_err(truncated)?;
  let nlink = reader.u32().map_err(truncated)?;
  let uid = reader.u32().map_err(truncated)?;
  let gid = reader.u32().map_err(truncated)?;
  let xattrs = read_xattrs(reader)?;
  Ok(NodeMeta {
    ino,
    mode,
    atime_ns,
    mtime_ns,
    ctime_ns,
    btime_ns,
    size,
    nlink,
    uid,
    gid,
    xattrs,
  })
}

/// Reads a node's extended attributes: a count bounded by the bytes left, then each name (valid, and
/// greater than the one before) and its value's extents (bounded the same way, and tiling the value).
fn read_xattrs(reader: &mut Reader<'_>) -> Result<Vec<Xattr>, ManifestError> {
  let count = reader.u32().map_err(|_| ManifestError::Truncated)?;
  let count = usize::try_from(count).map_err(|_| ManifestError::BadNode)?;
  if count > reader.remaining().checked_div(MIN_XATTR_BYTES).unwrap_or(0) {
    return Err(ManifestError::BadNode);
  }
  let mut xattrs: Vec<Xattr> = Vec::with_capacity(count);
  for _ in 0..count {
    let name_len = reader.u32().map_err(|_| ManifestError::Truncated)?;
    let name_len = usize::try_from(name_len).map_err(|_| ManifestError::BadXattr)?;
    if name_len == 0 || name_len > XATTR_NAME_MAX_BYTES {
      return Err(ManifestError::BadXattr);
    }
    let name = reader.raw(name_len).map_err(|_| ManifestError::Truncated)?;
    if name.contains(&0)
      || xattrs
        .last()
        .is_some_and(|previous| previous.name.as_slice() >= name)
    {
      return Err(ManifestError::BadXattr);
    }
    let extent_count = reader.u64().map_err(|_| ManifestError::Truncated)?;
    let extent_count = usize::try_from(extent_count).map_err(|_| ManifestError::BadNode)?;
    if extent_count > reader.remaining().checked_div(EXTENT_BYTES).unwrap_or(0) {
      return Err(ManifestError::BadNode);
    }
    let mut extents = Vec::with_capacity(extent_count);
    for _ in 0..extent_count {
      extents.push(read_extent(reader)?);
    }
    tiled_length(&extents)?;
    xattrs.push(Xattr {
      name: name.to_vec(),
      extents,
    });
  }
  Ok(xattrs)
}

/// The distinct chunk identities a tree and its root's metadata reference — every file extent and every
/// extended attribute's value extent — in first-reference order (a hole's zero identity is not a chunk and
/// is skipped). Iterative, so a tree at [`MAX_DEPTH`] costs an explicit stack, never the thread's. This is
/// the one definition of what an archive needs: placement, transfer and retention all read it.
pub fn referenced_chunks(root_meta: &NodeMeta, root: &Node) -> Vec<[u8; 32]> {
  let mut seen = std::collections::BTreeSet::new();
  let mut out = Vec::new();
  let mut take = |extents: &[Extent]| {
    for extent in extents {
      if extent.chunk != [0u8; 32] && seen.insert(extent.chunk) {
        out.push(extent.chunk);
      }
    }
  };
  for xattr in &root_meta.xattrs {
    take(&xattr.extents);
  }
  let mut pending: Vec<&Node> = vec![root];
  while let Some(node) = pending.pop() {
    match node {
      Node::File(extents) => take(extents),
      Node::Directory(entries) => {
        // Reversed onto the stack, so entries are visited in their order.
        for entry in entries.iter().rev() {
          pending.push(&entry.node);
        }
        for entry in entries {
          for xattr in &entry.meta.xattrs {
            take(&xattr.extents);
          }
        }
      }
    }
  }
  out
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
