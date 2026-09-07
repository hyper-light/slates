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
    let path_count = reader.u32()? as usize;
    let op_count = reader.u32()? as usize;
    // A path is at least its 4-byte length prefix and an op is a fixed 28-byte record; if the header's
    // counts cannot fit in the bytes that remain, the entry is corrupt — checked before any allocation
    // so a wild count never reserves memory it cannot fill.
    if path_count > reader.remaining() / PATH_MIN_BYTES {
      return Err(DocDecodeError::Truncated);
    }
    let mut paths = PathTable::new();
    for _ in 0..path_count {
      let len = reader.u32()? as usize;
      let raw = reader.bytes(len)?;
      let path = std::str::from_utf8(raw).map_err(|_| DocDecodeError::BadPath)?;
      paths.intern(path);
    }
    if op_count > reader.remaining() / OP_BYTES {
      return Err(DocDecodeError::Truncated);
    }
    let mut ops = Vec::with_capacity(op_count);
    for _ in 0..op_count {
      let kind = OpKind::from_wire(reader.u8()?).ok_or(DocDecodeError::BadKind)?;
      let flags = reader.u8()?;
      let path = reader.u16()?;
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
    Ok(OpsDoc { paths, ops })
  }
}

/// The smallest a path entry can be in the encoding: its length prefix (a `u32`), for an empty path.
const PATH_MIN_BYTES: usize = size_of::<u32>();
/// A fixed operation record's size: kind + flags (`u8` each), path (`u16`), and `at`, `len`, `src`
/// (`u64` each) — the fields of [`Op`] as they are written by [`OpsDoc::encode`].
const OP_BYTES: usize = size_of::<u8>()
  + size_of::<u8>()
  + size_of::<u16>()
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
    Ok(b[0])
  }

  pub(crate) fn u16(&mut self) -> Result<u16, DocDecodeError> {
    let b = self.bytes(size_of::<u16>())?;
    let mut word = [0u8; size_of::<u16>()];
    word.copy_from_slice(b);
    Ok(u16::from_le_bytes(word))
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
