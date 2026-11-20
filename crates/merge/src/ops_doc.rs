//! The ops document (§4.16 "Data model", "Composition at seal"). An increment's declared
//! operations, composed per path by the deriver, are serialized into one canonical document
//! whose BLAKE3 is part of the increment's identity. The document must be byte-for-byte
//! identical on every platform for the same operations — "its identity is the test" — so the
//! encoding is fixed little-endian with a sorted path table and no padding that varies, and it
//! is built through a sequential writer, never a struct transmute.
//!
//! Bytes are never in the document (§4.16 "Declared operations": bytes are never in the
//! journal); an operation names its range and, for content it adds, a source offset into the
//! increment's post-state chunks, which the splice resolves later.
//!
//! **Names without a fixed cap (A-621).** A path index is 64 bits, in memory and on the wire, and the header counts its
//! paths and operations in 64 bits, so a work names as many paths as its caller's admission allows: a first push of a
//! kernel tree (about 97,000 files) is one work. Until version 2 an index was 16 bits and interning answered `u16::MAX`
//! for every name past the 65,535th, so the 65,536th and every later name aliased one path: a correctness bug, not a
//! limit. Interning looks a name up in an ordered map, so a table of n names costs O(n log n) comparisons where the
//! scan it replaced cost O(n²); its tests count them.
//!
//! **A hard link names a node, never a path (A-646).** Since version 3 the document holds a node table after its paths:
//! each record a path of the table and an origin, the node at that path at the work's base or the node the work makes
//! there, and a `Link` names a record. Until version 3 a link named a path, which the engine read at the head, so a
//! link followed whatever stood at its target's path; the node a work saw is the one it linked to.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

/// The declared operation kinds (§4.16 `OpKind`), in their wire order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum OpKind {
  /// Format: bytes overwritten at a range.
  Overwrite = 0,
  /// Format: bytes appended past the old end.
  Extend = 1,
  /// Format: truncated to a length.
  Truncate = 2,
  /// Format: bytes inserted, shifting the rest.
  Insert = 3,
  /// Format: bytes removed, shifting the rest.
  Delete = 4,
  /// Format: a file created.
  Create = 5,
  /// Format: a name removed.
  Unlink = 6,
  /// Format: a directory created.
  Mkdir = 7,
  /// Format: a directory removed.
  Rmdir = 8,
  /// Format: a name renamed (the source path is `src` into the path table).
  Rename = 9,
  /// Format: a hard link created: `path` is the link, `src` the index in the node table of the node it names (A-646; a
  /// path index until version 3).
  Link = 10,
  /// Format: a symlink created.
  Symlink = 11,
  /// Format: the mode changed.
  SetMode = 12,
  /// Format: an xattr set.
  SetXattr = 13,
  /// Format: an xattr removed.
  RemoveXattr = 14,
  /// Format: a FIFO/socket node whose fixed metadata is at `src` for `len` bytes.
  Mknod = 15,
  /// Format: a path's tree copied by reference (A-586, Subversion's copy, §4.17.4 item 3): `path` is the destination,
  /// `src` the source's index in the path table (as a rename's), `at` the version of the source copied (Subversion's
  /// copy-from revision), `len` unused. Judged node-exclusively whatever its flags: it adds its destination, or with
  /// [`REPLACE`] replaces what the destination holds (A-626).
  Copy = 16,
}

impl OpKind {
  /// The wire value.
  pub fn to_wire(self) -> u8 {
    self as u8
  }

  /// The kind for a wire value, or `None` for an unknown one.
  pub fn from_wire(value: u8) -> Option<OpKind> {
    ALL_KINDS.iter().copied().find(|k| k.to_wire() == value)
  }
}

/// Every op kind, so `from_wire` needs no number of its own.
const ALL_KINDS: &[OpKind] = &[
  OpKind::Overwrite,
  OpKind::Extend,
  OpKind::Truncate,
  OpKind::Insert,
  OpKind::Delete,
  OpKind::Create,
  OpKind::Unlink,
  OpKind::Mkdir,
  OpKind::Rmdir,
  OpKind::Rename,
  OpKind::Link,
  OpKind::Symlink,
  OpKind::SetMode,
  OpKind::SetXattr,
  OpKind::RemoveXattr,
  OpKind::Mknod,
  OpKind::Copy,
];

/// One declared operation (§4.16 `OpRecord`), naming a path (by index into the document's path
/// table), a range, and a source offset into the post-state for content it adds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Op {
  /// The kind.
  pub kind: OpKind,
  /// Kind-specific flags (e.g. a rename's replace semantics); zero when unused.
  pub flags: u8,
  /// The path this operates on, as an index into the path table.
  pub path: u64,
  /// The operation's offset in the file (or the target path index for a rename's source).
  pub at: u64,
  /// The operation's length in bytes (or the new mode for `SetMode`).
  pub len: u64,
  /// The source offset into the post-state chunks for content this adds (unused: `u64::MAX`).
  pub src: u64,
}

/// The flag bit that makes an operation node-exclusive (§4.17.4 item 3, A-542): the path it changes is judged whole, so
/// any change to that path after the increment's base, in any dimension, is a conflict, however disjoint its bytes and
/// however identical the change. A Subversion client's commit declares it: FSFS's commit-time merge fails a node changed
/// on both sides, "even an identical replacement" (`libsvn_fs_fs/tree.c` `merge()`), and the client records each node it
/// committed as made at the new revision with its own bytes and properties.
/// Format: the high bit of `Op::flags`, which no operation kind sets otherwise, so a document that never sets it means
/// what it always meant.
pub const NODE_EXCLUSIVE: u8 = 0x80;

/// The flag bit that makes a `Copy` replace what its destination holds (A-626; Subversion's replace with history, a
/// changed path `R` with a copy source, §4.17.4 item 3.5): the destination's tree is taken away first, each key of it
/// removed as a removal is recorded, and the source's tree is copied there. A `Copy` without it refuses a destination
/// that holds anything. The removal is the flag's: the document's other operations on paths in the destination's tree
/// land after the copy, on what it brought.
/// Format: the low bit of `Op::flags`, which no other kind sets, so a document that never sets it means what it always
/// meant.
pub const REPLACE: u8 = 0x01;

impl Op {
  /// Whether the operation is node-exclusive ([`NODE_EXCLUSIVE`]).
  pub fn is_node_exclusive(&self) -> bool {
    self.flags & NODE_EXCLUSIVE != 0
  }

  /// Whether the operation is a `Copy` that replaces its destination's tree ([`REPLACE`]).
  pub fn replaces(&self) -> bool {
    self.kind == OpKind::Copy && self.flags & REPLACE != 0
  }
}

/// A path as the table's index orders it: by its bytes, as `String` orders them, so the index's order is the canonical
/// order [`OpsDoc::canonicalize`] imposes. Its own type only so the tests can count the comparisons interning makes.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Name(String);

impl Ord for Name {
  fn cmp(&self, other: &Name) -> Ordering {
    #[cfg(test)]
    tests::count_comparison();
    self.0.cmp(&other.0)
  }
}

impl PartialOrd for Name {
  fn partial_cmp(&self, other: &Name) -> Option<Ordering> {
    Some(self.cmp(other))
  }
}

/// The path table: the distinct paths an ops document names, sorted, so a path index is stable
/// and the document's identity does not depend on the order operations were declared in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PathTable {
  /// The paths, by index.
  paths: Vec<String>,
  /// Each path's index, by path: what interning looks a name up in (A-621), O(log n) comparisons a name, where a scan
  /// of `paths` was O(n). Its order is the canonical one, which [`OpsDoc::canonicalize`] reads off it.
  index: BTreeMap<Name, u64>,
}

impl PathTable {
  /// An empty table.
  pub fn new() -> PathTable {
    PathTable::default()
  }

  /// Interns `path`, returning its stable index (deduplicated, insertion order); the sorted,
  /// canonical order is imposed later by [`OpsDoc::canonicalize`]. Every distinct path has its own index, however many
  /// the table holds: a table's length always fits 64 bits on the targets slates builds for, and the conversion's
  /// unreachable refusal answers `u64::MAX`, which names no path (A-621).
  pub fn intern(&mut self, path: &str) -> u64 {
    let next = u64::try_from(self.paths.len()).unwrap_or(u64::MAX);
    match self.index.entry(Name(path.to_owned())) {
      Entry::Occupied(found) => *found.get(),
      Entry::Vacant(slot) => {
        slot.insert(next);
        self.paths.push(path.to_owned());
        next
      }
    }
  }

  /// Sorts the paths and returns, for each old index, its new index (so the ops can be
  /// remapped). Called by [`OpsDoc::canonicalize`]. The index map holds the paths in the canonical order already, so the
  /// sort is one pass over it, each path moved, none copied.
  fn sort_and_remap(&mut self) -> Vec<u64> {
    let mut old_paths = std::mem::take(&mut self.paths);
    let mut remap = vec![u64::MAX; old_paths.len()];
    for (rank, index) in (0u64..).zip(self.index.values_mut()) {
      let old = usize::try_from(*index).unwrap_or(usize::MAX);
      if let Some(slot) = remap.get_mut(old) {
        *slot = rank;
      }
      self.paths.push(
        old_paths
          .get_mut(old)
          .map(std::mem::take)
          .unwrap_or_default(),
      );
      *index = rank;
    }
    remap
  }

  /// The path at `index`.
  pub fn path(&self, index: u64) -> Option<&str> {
    self
      .paths
      .get(usize::try_from(index).ok()?)
      .map(String::as_str)
  }

  /// The paths, sorted.
  pub fn paths(&self) -> &[String] {
    &self.paths
  }
}

/// Format: where the node a hard link names is (A-646, §4.16 "Data model"), a node record's origin byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum NodeOrigin {
  /// Format: the node at the path at the work's base: a link's node while that link stood unchanged since, a file or
  /// an IPC node while it was not removed since.
  Base = 0,
  /// Format: the node the work makes at the path: a file it creates or renames there, an IPC node it makes, or the
  /// node a link it makes there names.
  Made = 1,
}

impl NodeOrigin {
  /// The wire value.
  pub fn to_wire(self) -> u8 {
    self as u8
  }

  /// The origin for a wire value, or `None` for an unknown one.
  pub fn from_wire(value: u8) -> Option<NodeOrigin> {
    [NodeOrigin::Base, NodeOrigin::Made]
      .into_iter()
      .find(|origin| origin.to_wire() == value)
  }
}

/// A node a hard link names (A-646): a path of the document's table, and where the node is. Ordered by path, then
/// origin: the node table's canonical order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NodeRef {
  /// The path, as an index into the path table.
  pub path: u64,
  /// Where the node is: at the path at the work's base, or made there by the work.
  pub origin: NodeOrigin,
}

/// The node table (A-646): the distinct nodes the document's hard links name, each once, so a link names a node and
/// links naming one node name one record. Sorted by path, then origin, by [`OpsDoc::canonicalize`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeTable {
  /// The nodes, by index.
  nodes: Vec<NodeRef>,
  /// Each node's index, by node: what interning looks a node up in, O(log n).
  index: BTreeMap<NodeRef, u64>,
}

impl NodeTable {
  /// An empty table.
  pub fn new() -> NodeTable {
    NodeTable::default()
  }

  /// Interns `node`, returning its index (deduplicated, insertion order; the canonical order is imposed by
  /// [`OpsDoc::canonicalize`]). The conversion's unreachable refusal answers `u64::MAX`, which names no node.
  pub fn intern(&mut self, node: NodeRef) -> u64 {
    let next = u64::try_from(self.nodes.len()).unwrap_or(u64::MAX);
    match self.index.entry(node) {
      Entry::Occupied(found) => *found.get(),
      Entry::Vacant(slot) => {
        slot.insert(next);
        self.nodes.push(node);
        next
      }
    }
  }

  /// The node at `index`.
  pub fn node(&self, index: u64) -> Option<NodeRef> {
    self.nodes.get(usize::try_from(index).ok()?).copied()
  }

  /// The nodes, by index.
  pub fn nodes(&self) -> &[NodeRef] {
    &self.nodes
  }

  /// Moves each node's path through `paths` (the path table's remap), puts the table in canonical order, and returns,
  /// for each old index, its new index (so the links can be remapped). A path the remap does not know becomes
  /// `u64::MAX`, which names none.
  fn sort_and_remap(&mut self, paths: &[u64]) -> Vec<u64> {
    let old = std::mem::take(&mut self.nodes);
    self.index.clear();
    let mut remap = vec![u64::MAX; old.len()];
    let mut by_node: BTreeMap<NodeRef, Vec<usize>> = BTreeMap::new();
    for (position, node) in old.iter().enumerate() {
      let path = usize::try_from(node.path)
        .ok()
        .and_then(|index| paths.get(index))
        .copied()
        .unwrap_or(u64::MAX);
      by_node
        .entry(NodeRef {
          path,
          origin: node.origin,
        })
        .or_default()
        .push(position);
    }
    for (node, positions) in by_node {
      let rank = self.intern(node);
      for position in positions {
        if let Some(slot) = remap.get_mut(position) {
          *slot = rank;
        }
      }
    }
    remap
  }
}

/// Format: the ops document magic, `SLOD` in little-endian ASCII.
const MAGIC: u32 = 0x444f_4c53;
/// Format: the document version. 3 since A-646: the header counts a node table's records too, and a `Link` names one of
/// them, a node, where it named a path; a version 2 document, whose links named paths, is refused (`BadVersion`), never
/// read under the other meaning. 2 since A-621: a path index and the header's counts are 64 bits.
const VERSION: u32 = 3;

/// An ops document: the sorted path table, the node table its hard links name, and the operations, in a canonical
/// order (by path index, then by offset), so the same declared work always serializes to the same bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpsDoc {
  /// The path table.
  pub paths: PathTable,
  /// The node table (A-646): the nodes the document's hard links name.
  pub nodes: NodeTable,
  /// The operations, canonically ordered.
  pub ops: Vec<Op>,
}

impl OpsDoc {
  /// An empty document.
  pub fn new() -> OpsDoc {
    OpsDoc {
      paths: PathTable::new(),
      nodes: NodeTable::new(),
      ops: Vec::new(),
    }
  }

  /// Puts the operations into canonical order: by path index, then offset, then kind. Called
  /// before encoding so the identity is independent of the order operations were added.
  pub fn canonicalize(&mut self) {
    let remap = self.paths.sort_and_remap();
    let nodes = self.nodes.sort_and_remap(&remap);
    for op in &mut self.ops {
      if matches!(op.kind, OpKind::Rename | OpKind::Symlink | OpKind::Copy) {
        op.src = usize::try_from(op.src)
          .ok()
          .and_then(|index| remap.get(index))
          .map_or(u64::MAX, |index| *index);
      }
      if op.kind == OpKind::Link {
        op.src = usize::try_from(op.src)
          .ok()
          .and_then(|index| nodes.get(index))
          .map_or(u64::MAX, |index| *index);
      }
      if matches!(op.kind, OpKind::SetXattr | OpKind::RemoveXattr) {
        op.at = usize::try_from(op.at)
          .ok()
          .and_then(|index| remap.get(index))
          .map_or(u64::MAX, |index| *index);
      }
      if let Some(new) = usize::try_from(op.path)
        .ok()
        .and_then(|index| remap.get(index))
      {
        op.path = *new;
      }
    }
    self.ops.sort_by(|a, b| {
      a.path
        .cmp(&b.path)
        .then(a.at.cmp(&b.at))
        .then(a.kind.to_wire().cmp(&b.kind.to_wire()))
    });
  }

  /// The canonical byte encoding: the header (magic and version, `u32` each; path, node and op counts, `u64` each), then
  /// each path as a `u32` length-prefixed byte string, then each node as a fixed record of [`NODE_BYTES`] (its origin, a
  /// byte, and its path, a `u64`; A-646), then each operation as a fixed record of [`OP_BYTES`] (kind and flags, a byte
  /// each; path, offset, length and source, `u64` each). Little-endian throughout; no host-dependent padding. A count's
  /// conversion to 64 bits cannot fail on the targets slates builds for.
  pub fn encode(&self) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&VERSION.to_le_bytes());
    for count in [
      self.paths.paths().len(),
      self.nodes.nodes().len(),
      self.ops.len(),
    ] {
      out.extend_from_slice(&u64::try_from(count).unwrap_or(u64::MAX).to_le_bytes());
    }
    for path in self.paths.paths() {
      out.extend_from_slice(&u32::try_from(path.len()).unwrap_or(u32::MAX).to_le_bytes());
      out.extend_from_slice(path.as_bytes());
    }
    for node in self.nodes.nodes() {
      out.push(node.origin.to_wire());
      out.extend_from_slice(&node.path.to_le_bytes());
    }
    for op in &self.ops {
      out.push(op.kind.to_wire());
      out.push(op.flags);
      out.extend_from_slice(&op.path.to_le_bytes());
      out.extend_from_slice(&op.at.to_le_bytes());
      out.extend_from_slice(&op.len.to_le_bytes());
      out.extend_from_slice(&op.src.to_le_bytes());
    }
    out
  }

  /// The increment identity's ops-document half: the BLAKE3 of the canonical encoding. The same
  /// declared work yields the same hash on every platform (the determinism gate).
  pub fn identity(&self) -> [u8; 32] {
    *blake3::hash(&self.encode()).as_bytes()
  }

  /// Decodes the canonical encoding produced by [`OpsDoc::encode`] — the inverse used to replay a
  /// green's persisted chain on recovery (§4.16, §4.8). It reads OUR OWN persisted bytes, but a torn
  /// or corrupted db entry could present anything, so every field is bounds-checked against the bytes
  /// that remain before it is read, no count from the header is trusted for allocation, and any
  /// mismatch is a typed [`DocDecodeError`] rather than a panic. The decode is exact: `decode(encode(d))`
  /// equals `d` for every document, the round-trip gated by test.
  pub fn decode(bytes: &[u8]) -> Result<OpsDoc, DocDecodeError> {
    let mut reader = Reader::new(bytes);
    if reader.u32()? != MAGIC {
      return Err(DocDecodeError::BadMagic);
    }
    if reader.u32()? != VERSION {
      return Err(DocDecodeError::BadVersion);
    }
    let path_count = usize::try_from(reader.u64()?).map_err(|_| DocDecodeError::Truncated)?;
    let node_count = usize::try_from(reader.u64()?).map_err(|_| DocDecodeError::Truncated)?;
    let op_count = usize::try_from(reader.u64()?).map_err(|_| DocDecodeError::Truncated)?;
    // A path is at least its 4-byte length prefix and an op is a fixed record of `OP_BYTES`; if the header's
    // counts cannot fit in the bytes that remain, the entry is corrupt — checked before any allocation
    // so a wild count never reserves memory it cannot fill.
    if path_count > reader.remaining() / PATH_MIN_BYTES {
      return Err(DocDecodeError::Truncated);
    }
    let mut paths = PathTable::new();
    for expected in (0u64..).take(path_count) {
      let len = reader.u32()? as usize;
      let raw = reader.bytes(len)?;
      let path = std::str::from_utf8(raw).map_err(|_| DocDecodeError::BadPath)?;
      // A table never holds a path twice (interning deduplicates), so a second one is a corrupt entry, refused: read
      // as the first, every later index would have named the path after its own (A-621).
      if paths.intern(path) != expected {
        return Err(DocDecodeError::DuplicatePath);
      }
    }
    let nodes = take_nodes(&mut reader, node_count, paths.paths().len())?;
    if op_count > reader.remaining() / OP_BYTES {
      return Err(DocDecodeError::Truncated);
    }
    let mut ops = Vec::with_capacity(op_count);
    for _ in 0..op_count {
      let kind = OpKind::from_wire(reader.u8()?).ok_or(DocDecodeError::BadKind)?;
      let flags = reader.u8()?;
      let path = reader.u64()?;
      let at = reader.u64()?;
      let len = reader.u64()?;
      let src = reader.u64()?;
      ops.push(Op {
        kind,
        flags,
        path,
        at,
        len,
        src,
      });
    }
    if !reader.is_empty() {
      return Err(DocDecodeError::TrailingBytes);
    }
    Ok(OpsDoc { paths, nodes, ops })
  }
}

/// Reads `count` node records (A-646), checked against the bytes that remain before anything is reserved: each origin
/// one this build knows and each path one of the `paths` the table holds (`BadNode`), and no node twice
/// (`DuplicateNode`, as a path twice is refused).
fn take_nodes(
  reader: &mut Reader<'_>,
  count: usize,
  paths: usize,
) -> Result<NodeTable, DocDecodeError> {
  if count > reader.remaining() / NODE_BYTES {
    return Err(DocDecodeError::Truncated);
  }
  let paths = u64::try_from(paths).unwrap_or(u64::MAX);
  let mut nodes = NodeTable::new();
  for expected in (0u64..).take(count) {
    let origin = NodeOrigin::from_wire(reader.u8()?).ok_or(DocDecodeError::BadNode)?;
    let path = reader.u64()?;
    if path >= paths {
      return Err(DocDecodeError::BadNode);
    }
    if nodes.intern(NodeRef { path, origin }) != expected {
      return Err(DocDecodeError::DuplicateNode);
    }
  }
  Ok(nodes)
}

/// The smallest a path entry can be in the encoding: its length prefix (a `u32`), for an empty path.
const PATH_MIN_BYTES: usize = size_of::<u32>();
/// Format: a node record's size (A-646, version 3): its origin, a byte, and its path's index, a `u64`.
pub(crate) const NODE_BYTES: usize = size_of::<u8>() + size_of::<u64>();
/// Format: a fixed operation record's size: kind + flags (`u8` each), and path, `at`, `len`, `src` (`u64` each), the
/// fields of [`Op`] as [`OpsDoc::encode`] writes them, 34 bytes since version 2 (A-621). The chain entry writes an op's
/// record the same way (`crate::chain_entry`).
pub(crate) const OP_BYTES: usize = size_of::<u8>()
  + size_of::<u8>()
  + size_of::<u64>()
  + size_of::<u64>()
  + size_of::<u64>()
  + size_of::<u64>();

/// A refusal decoding an ops document (§4.16): the closed set of ways the canonical bytes can be
/// malformed. Every one is a corrupt or truncated entry, never a panic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocDecodeError {
  /// The bytes ended before a field could be read.
  Truncated,
  /// The magic number was not the ops-document magic.
  BadMagic,
  /// The version was not one this build decodes.
  BadVersion,
  /// An operation kind byte named no known kind.
  BadKind,
  /// A path was not valid UTF-8.
  BadPath,
  /// Bytes remained after the last operation.
  TrailingBytes,
  /// A path table named one path twice (A-621): read as the first, every later index would name the path after its own.
  DuplicatePath,
  /// A node record named an origin this build does not know, or a path the table does not hold (A-646).
  BadNode,
  /// A node table named one node twice (A-646), as a path table naming one path twice is refused.
  DuplicateNode,
}

impl std::fmt::Display for DocDecodeError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let reason = match self {
      DocDecodeError::Truncated => "the ops document ended early",
      DocDecodeError::BadMagic => "the ops document magic is wrong",
      DocDecodeError::BadVersion => "the ops document version is unknown",
      DocDecodeError::BadKind => "an operation kind is unknown",
      DocDecodeError::BadPath => "a path is not valid UTF-8",
      DocDecodeError::TrailingBytes => "the ops document has trailing bytes",
      DocDecodeError::DuplicatePath => "the ops document names a path twice",
      DocDecodeError::BadNode => "a node record names no known origin or no path of the table",
      DocDecodeError::DuplicateNode => "the ops document names a node twice",
    };
    f.write_str(reason)
  }
}

impl std::error::Error for DocDecodeError {}

/// A cursor over the canonical bytes: every read is bounds-checked against what remains, so a torn
/// entry yields a typed [`DocDecodeError::Truncated`] rather than an out-of-bounds panic.
pub(crate) struct Reader<'a> {
  bytes: &'a [u8],
  at: usize,
}

impl<'a> Reader<'a> {
  pub(crate) fn new(bytes: &'a [u8]) -> Reader<'a> {
    Reader { bytes, at: 0 }
  }

  /// The bytes not yet read.
  pub(crate) fn remaining(&self) -> usize {
    self.bytes.len().saturating_sub(self.at)
  }

  pub(crate) fn is_empty(&self) -> bool {
    self.remaining() == 0
  }

  /// The next `len` bytes, advancing past them, or `Truncated` when fewer remain.
  pub(crate) fn bytes(&mut self, len: usize) -> Result<&'a [u8], DocDecodeError> {
    let end = self.at.checked_add(len).ok_or(DocDecodeError::Truncated)?;
    let slice = self
      .bytes
      .get(self.at..end)
      .ok_or(DocDecodeError::Truncated)?;
    self.at = end;
    Ok(slice)
  }

  pub(crate) fn u8(&mut self) -> Result<u8, DocDecodeError> {
    let b = self.bytes(size_of::<u8>())?;
    b.first().copied().ok_or(DocDecodeError::Truncated)
  }

  pub(crate) fn u32(&mut self) -> Result<u32, DocDecodeError> {
    let b = self.bytes(size_of::<u32>())?;
    let mut word = [0u8; size_of::<u32>()];
    word.copy_from_slice(b);
    Ok(u32::from_le_bytes(word))
  }

  pub(crate) fn u64(&mut self) -> Result<u64, DocDecodeError> {
    let b = self.bytes(size_of::<u64>())?;
    let mut word = [0u8; size_of::<u64>()];
    word.copy_from_slice(b);
    Ok(u64::from_le_bytes(word))
  }
}

#[cfg(test)]
mod tests;
