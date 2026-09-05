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
  /// Format: a hard link created.
  Link = 10,
  /// Format: a symlink created.
  Symlink = 11,
  /// Format: the mode changed.
  SetMode = 12,
  /// Format: an xattr set.
  SetXattr = 13,
  /// Format: an xattr removed.
  RemoveXattr = 14,
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
  pub path: u16,
  /// The operation's offset in the file (or the target path index for a rename's source).
  pub at: u64,
  /// The operation's length in bytes (or the new mode for `SetMode`).
  pub len: u64,
  /// The source offset into the post-state chunks for content this adds (unused: `u64::MAX`).
  pub src: u64,
}

/// The path table: the distinct paths an ops document names, sorted, so a path index is stable
/// and the document's identity does not depend on the order operations were declared in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PathTable {
  paths: Vec<String>,
}

impl PathTable {
  /// An empty table.
  pub fn new() -> PathTable {
    PathTable { paths: Vec::new() }
  }

  /// Interns `path`, returning its stable index (deduplicated, insertion order); the sorted,
  /// canonical order is imposed later by [`OpsDoc::canonicalize`].
  pub fn intern(&mut self, path: &str) -> u16 {
    if let Some(i) = self.paths.iter().position(|p| p == path) {
      return u16::try_from(i).unwrap_or(u16::MAX);
    }
    let i = self.paths.len();
    self.paths.push(path.to_owned());
    u16::try_from(i).unwrap_or(u16::MAX)
  }

  /// Sorts the paths and returns, for each old index, its new index (so the ops can be
  /// remapped). Called by [`OpsDoc::canonicalize`].
  fn sort_and_remap(&mut self) -> Vec<u16> {
    let mut order: Vec<usize> = (0..self.paths.len()).collect();
    order.sort_by(|&a, &b| self.paths[a].cmp(&self.paths[b]));
    let mut new_index = vec![0u16; self.paths.len()];
    for (new, &old) in order.iter().enumerate() {
      new_index[old] = u16::try_from(new).unwrap_or(u16::MAX);
    }
    let sorted: Vec<String> = order.into_iter().map(|i| self.paths[i].clone()).collect();
    self.paths = sorted;
    new_index
  }

  /// The path at `index`.
  pub fn path(&self, index: u16) -> Option<&str> {
    self.paths.get(usize::from(index)).map(String::as_str)
  }

  /// The paths, sorted.
  pub fn paths(&self) -> &[String] {
    &self.paths
  }
}

/// Format: the ops document magic, `SLOD` in little-endian ASCII.
const MAGIC: u32 = 0x444f_4c53;
/// Format: the document version.
const VERSION: u32 = 1;

/// An ops document: the sorted path table and the operations, in a canonical order (by path
/// index, then by offset), so the same declared work always serializes to the same bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpsDoc {
  /// The path table.
  pub paths: PathTable,
  /// The operations, canonically ordered.
  pub ops: Vec<Op>,
}

impl OpsDoc {
  /// An empty document.
  pub fn new() -> OpsDoc {
    OpsDoc {
      paths: PathTable::new(),
      ops: Vec::new(),
    }
  }

  /// Puts the operations into canonical order: by path index, then offset, then kind. Called
  /// before encoding so the identity is independent of the order operations were added.
  pub fn canonicalize(&mut self) {
    let remap = self.paths.sort_and_remap();
    for op in &mut self.ops {
      if let Some(new) = remap.get(usize::from(op.path)) {
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

  /// The canonical byte encoding: the header (magic, version, path count, op count), then each
  /// path as a length-prefixed byte string, then each operation as a fixed record. Little-endian
  /// throughout; no host-dependent padding.
  pub fn encode(&self) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(
      &u32::try_from(self.paths.paths().len())
        .unwrap_or(u32::MAX)
        .to_le_bytes(),
    );
    out.extend_from_slice(
      &u32::try_from(self.ops.len())
        .unwrap_or(u32::MAX)
        .to_le_bytes(),
    );
    for path in self.paths.paths() {
      out.extend_from_slice(&u32::try_from(path.len()).unwrap_or(u32::MAX).to_le_bytes());
      out.extend_from_slice(path.as_bytes());
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
}
