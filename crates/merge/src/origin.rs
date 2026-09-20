//! A green's origin (§4.16 "The chain: … the origin version from a snapshot"; D-27; the A-9
//! integration requirement: "Green's immutable version chain starts from scratch or a complete
//! immutable base, never an implicitly live host directory"): the state a green holds at version 0,
//! before any increment — every file with its bytes, the directories, the modes, the symbolic and
//! hard links and the extended attributes — captured once by the service from a complete immutable
//! snapshot and given to the engine ([`crate::engine::Green::with_origin`]). A scratch green's
//! origin is empty.
//!
//! The service persists the origin as the green's first durable chain entry and replays it on
//! recovery, and in a fleet places it to the green's candidate holders before the version-0 merge
//! record names it (§4.16 "issued only when every identity the version references is placed"), so
//! a holder recomputes version 0 from the same bytes the owner used. The encoding is canonical —
//! every table sorted by its key, little-endian, length-delimited — so one origin encodes to one
//! byte sequence on every host (its BLAKE3 is what the merge record names it by), and `decode` is
//! exact and hostile-checked: every count and length is checked against the bytes that remain
//! before anything is allocated, and any malformation is a typed [`DocDecodeError`], never a panic.

use crate::ops_doc::{DocDecodeError, Reader};

/// Format: the origin encoding's magic, "GORG" little-endian, so a chain entry of another kind
/// (an increment) is refused at the first word rather than misread.
const MAGIC: u32 = u32::from_le_bytes(*b"GORG");
/// Format: the origin encoding's version; a decoder refuses any other.
const VERSION: u32 = 2;
/// Format: the smallest an entry of any table can be in the encoding — its length prefix (a `u32`)
/// — the divisor a declared count is checked against before allocation.
const ENTRY_MIN_BYTES: usize = size_of::<u32>();

/// A green's state at version 0: the tables the engine seeds itself from. Paths are the volume's
/// absolute paths without the leading slash, as the ops document names them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Origin {
  /// Every regular file with its bytes.
  pub files: Vec<(String, Vec<u8>)>,
  /// Every directory.
  pub dirs: Vec<String>,
  /// Every path with an explicit mode.
  pub modes: Vec<(String, u32)>,
  /// Every symbolic link with its target.
  pub symlinks: Vec<(String, String)>,
  /// Every hard link with the file it names (a namespace edge, as the engine merges it).
  pub hardlinks: Vec<(String, String)>,
  /// Every FIFO/socket inode, without endpoint state. Aliases are hard-link edges.
  pub specials: Vec<(String, crate::special::SpecialNode)>,
  /// Every extended attribute as `(path, name, value)`.
  pub xattrs: Vec<(String, String, Vec<u8>)>,
}

impl Origin {
  /// Whether the origin holds nothing (a scratch green's).
  pub fn is_empty(&self) -> bool {
    self.files.is_empty()
      && self.dirs.is_empty()
      && self.modes.is_empty()
      && self.symlinks.is_empty()
      && self.hardlinks.is_empty()
      && self.xattrs.is_empty()
      && self.specials.is_empty()
  }

  /// Sorts every table by its key and drops a repeated key (the last declared wins), so the
  /// encoding is one sequence whatever order the service walked the snapshot in.
  pub fn canonicalize(&mut self) {
    dedup_by_key(&mut self.specials, |(path, _)| path.clone());
    dedup_by_key(&mut self.files, |(path, _)| path.clone());
    self.dirs.sort();
    self.dirs.dedup();
    dedup_by_key(&mut self.modes, |(path, _)| path.clone());
    dedup_by_key(&mut self.symlinks, |(path, _)| path.clone());
    dedup_by_key(&mut self.hardlinks, |(path, _)| path.clone());
    dedup_by_key(&mut self.xattrs, |(path, name, _)| {
      (path.clone(), name.clone())
    });
  }

  /// The canonical bytes: the magic and version, then each table as a count and its entries, every
  /// string and byte run length-prefixed, all little-endian. Canonicalizes a copy first, so two
  /// equal origins encode identically whatever their declaration order.
  pub fn encode(&self) -> Vec<u8> {
    let mut canonical = self.clone();
    canonical.canonicalize();
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&VERSION.to_le_bytes());
    put_count(&mut out, canonical.files.len());
    for (path, bytes) in &canonical.files {
      put_bytes(&mut out, path.as_bytes());
      put_bytes(&mut out, bytes);
    }
    put_count(&mut out, canonical.dirs.len());
    for path in &canonical.dirs {
      put_bytes(&mut out, path.as_bytes());
    }
    put_count(&mut out, canonical.modes.len());
    for (path, mode) in &canonical.modes {
      put_bytes(&mut out, path.as_bytes());
      out.extend_from_slice(&mode.to_le_bytes());
    }
    put_count(&mut out, canonical.symlinks.len());
    for (path, target) in &canonical.symlinks {
      put_bytes(&mut out, path.as_bytes());
      put_bytes(&mut out, target.as_bytes());
    }
    put_count(&mut out, canonical.hardlinks.len());
    for (path, target) in &canonical.hardlinks {
      put_bytes(&mut out, path.as_bytes());
      put_bytes(&mut out, target.as_bytes());
    }
    put_count(&mut out, canonical.xattrs.len());
    for (path, name, value) in &canonical.xattrs {
      put_bytes(&mut out, path.as_bytes());
      put_bytes(&mut out, name.as_bytes());
      put_bytes(&mut out, value);
    }
    put_count(&mut out, canonical.specials.len());
    for (path, node) in &canonical.specials {
      put_bytes(&mut out, path.as_bytes());
      out.extend_from_slice(&node.encode());
    }
    out
  }

  /// The origin's identity: the BLAKE3 of its canonical bytes.
  pub fn identity(&self) -> [u8; 32] {
    *blake3::hash(&self.encode()).as_bytes()
  }

  /// Decodes [`Origin::encode`]'s bytes — the inverse a recovery or a holder uses. Every count is
  /// checked against the bytes that remain (at the smallest entry size) before a table is allocated,
  /// every length against what remains before it is read, and the result must consume the bytes
  /// exactly; each failure is a typed [`DocDecodeError`].
  pub fn decode(bytes: &[u8]) -> Result<Origin, DocDecodeError> {
    let mut reader = Reader::new(bytes);
    if reader.u32()? != MAGIC {
      return Err(DocDecodeError::BadMagic);
    }
    if reader.u32()? != VERSION {
      return Err(DocDecodeError::BadVersion);
    }
    let mut origin = Origin::default();
    for _ in 0..take_count(&mut reader)? {
      let path = take_str(&mut reader)?;
      let bytes = reader_bytes(&mut reader)?.to_vec();
      origin.files.push((path, bytes));
    }
    for _ in 0..take_count(&mut reader)? {
      origin.dirs.push(take_str(&mut reader)?);
    }
    for _ in 0..take_count(&mut reader)? {
      let path = take_str(&mut reader)?;
      let mode = reader.u32()?;
      origin.modes.push((path, mode));
    }
    for _ in 0..take_count(&mut reader)? {
      let path = take_str(&mut reader)?;
      let target = take_str(&mut reader)?;
      origin.symlinks.push((path, target));
    }
    for _ in 0..take_count(&mut reader)? {
      let path = take_str(&mut reader)?;
      let target = take_str(&mut reader)?;
      origin.hardlinks.push((path, target));
    }
    for _ in 0..take_count(&mut reader)? {
      let path = take_str(&mut reader)?;
      let name = take_str(&mut reader)?;
      let value = reader_bytes(&mut reader)?.to_vec();
      origin.xattrs.push((path, name, value));
    }
    for _ in 0..take_count(&mut reader)? {
      let path = take_str(&mut reader)?;
      let node = crate::special::SpecialNode::decode(reader.bytes(crate::special::ENCODED_BYTES)?)?;
      origin.specials.push((path, node));
    }
    if !reader.is_empty() {
      return Err(DocDecodeError::TrailingBytes);
    }
    Ok(origin)
  }
}

/// Sorts `table` by `key` and keeps the last entry of each repeated key.
fn dedup_by_key<T, K: Ord>(table: &mut Vec<T>, key: impl Fn(&T) -> K) {
  // A stable sort keeps declaration order among equal keys, so "the last declared wins" is
  // reversing before the dedup (which keeps the first of a run) and reversing back.
  table.sort_by_key(&key);
  table.reverse();
  table.dedup_by(|a, b| key(a) == key(b));
  table.reverse();
}

fn put_count(out: &mut Vec<u8>, count: usize) {
  out.extend_from_slice(&u32::try_from(count).unwrap_or(u32::MAX).to_le_bytes());
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
  put_count(out, bytes.len());
  out.extend_from_slice(bytes);
}

/// A table's declared count, refused before any allocation when the bytes that remain could not
/// hold that many entries at the smallest entry size.
fn take_count(reader: &mut Reader<'_>) -> Result<usize, DocDecodeError> {
  let count = reader.u32()? as usize;
  if count > reader.remaining() / ENTRY_MIN_BYTES {
    return Err(DocDecodeError::Truncated);
  }
  Ok(count)
}

fn reader_bytes<'a>(reader: &mut Reader<'a>) -> Result<&'a [u8], DocDecodeError> {
  let len = reader.u32()? as usize;
  reader.bytes(len)
}

fn take_str(reader: &mut Reader<'_>) -> Result<String, DocDecodeError> {
  let raw = reader_bytes(reader)?;
  std::str::from_utf8(raw)
    .map(str::to_owned)
    .map_err(|_| DocDecodeError::BadPath)
}
