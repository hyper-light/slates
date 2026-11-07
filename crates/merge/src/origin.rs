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
  /// string and byte run length-prefixed, all little-endian. An origin not already canonical is canonicalized in a
  /// copy first, so two equal origins encode identically whatever their declaration order; a canonical one (what a
  /// walk of a snapshot gives) is written as it stands, with no copy.
  pub fn encode(&self) -> Vec<u8> {
    if !self.is_canonical() {
      let mut canonical = self.clone();
      canonical.canonicalize();
      return canonical.encode();
    }
    let mut out = Vec::new();
    let mut at = Some(Cursor::Header);
    while let Some(cursor) = at {
      at = self.emit(cursor, &mut |bytes| out.extend_from_slice(bytes));
    }
    out
  }

  /// The origin's identity: the BLAKE3 of its canonical bytes, hashed as they are written rather than gathered first
  /// (a kernel-sized origin's encoding was a second whole copy of the tree, made only to be hashed).
  pub fn identity(&self) -> [u8; 32] {
    let Some(mut cursor) = IdentityCursor::new(self) else {
      let mut canonical = self.clone();
      canonical.canonicalize();
      return canonical.identity();
    };
    loop {
      if let Some(identity) = cursor.step(self, usize::MAX) {
        return identity;
      }
    }
  }

  /// Whether every table is in the order [`Origin::canonicalize`] leaves it: strictly increasing by its key, so
  /// sorted and with no key twice.
  pub fn is_canonical(&self) -> bool {
    strictly(&self.files, |a, b| a.0 < b.0)
      && strictly(&self.dirs, |a, b| a < b)
      && strictly(&self.modes, |a, b| a.0 < b.0)
      && strictly(&self.symlinks, |a, b| a.0 < b.0)
      && strictly(&self.hardlinks, |a, b| a.0 < b.0)
      && strictly(&self.xattrs, |a, b| (&a.0, &a.1) < (&b.0, &b.1))
      && strictly(&self.specials, |a, b| a.0 < b.0)
  }

  /// Writes the canonical encoding's unit at `at` (the header, one table's count, or one entry) to `sink`; the next
  /// unit's place, `None` past the last. The origin must be canonical; [`Origin::encode`] and [`IdentityCursor`] see
  /// to it.
  fn emit(&self, at: Cursor, sink: &mut impl FnMut(&[u8])) -> Option<Cursor> {
    match at {
      Cursor::Header => {
        sink(&MAGIC.to_le_bytes());
        sink(&VERSION.to_le_bytes());
        Some(Cursor::Count(Table::Files))
      }
      Cursor::Count(table) => {
        put_count(sink, self.table_len(table));
        Some(self.after(table, 0))
      }
      Cursor::Entry(table, index) => {
        self.emit_entry(table, index, sink);
        Some(self.after(table, index.saturating_add(1)))
      }
      Cursor::Done => None,
    }
  }

  /// The place after `table`'s entries up to `next`: its entry `next`, the next table's count, or [`Cursor::Done`] past
  /// the last table.
  fn after(&self, table: Table, next: usize) -> Cursor {
    if next < self.table_len(table) {
      return Cursor::Entry(table, next);
    }
    match table.next() {
      Some(following) => Cursor::Count(following),
      None => Cursor::Done,
    }
  }

  fn table_len(&self, table: Table) -> usize {
    match table {
      Table::Files => self.files.len(),
      Table::Dirs => self.dirs.len(),
      Table::Modes => self.modes.len(),
      Table::Symlinks => self.symlinks.len(),
      Table::Hardlinks => self.hardlinks.len(),
      Table::Xattrs => self.xattrs.len(),
      Table::Specials => self.specials.len(),
    }
  }

  fn emit_entry(&self, table: Table, index: usize, sink: &mut impl FnMut(&[u8])) {
    match table {
      Table::Files => {
        if let Some((path, bytes)) = self.files.get(index) {
          put_bytes(sink, path.as_bytes());
          put_bytes(sink, bytes);
        }
      }
      Table::Dirs => {
        if let Some(path) = self.dirs.get(index) {
          put_bytes(sink, path.as_bytes());
        }
      }
      Table::Modes => {
        if let Some((path, mode)) = self.modes.get(index) {
          put_bytes(sink, path.as_bytes());
          sink(&mode.to_le_bytes());
        }
      }
      Table::Symlinks => {
        if let Some((path, target)) = self.symlinks.get(index) {
          put_bytes(sink, path.as_bytes());
          put_bytes(sink, target.as_bytes());
        }
      }
      Table::Hardlinks => {
        if let Some((path, target)) = self.hardlinks.get(index) {
          put_bytes(sink, path.as_bytes());
          put_bytes(sink, target.as_bytes());
        }
      }
      Table::Xattrs => {
        if let Some((path, name, value)) = self.xattrs.get(index) {
          put_bytes(sink, path.as_bytes());
          put_bytes(sink, name.as_bytes());
          put_bytes(sink, value);
        }
      }
      Table::Specials => {
        if let Some((path, node)) = self.specials.get(index) {
          put_bytes(sink, path.as_bytes());
          sink(&node.encode());
        }
      }
    }
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

/// A canonical encoding's table, in the order the encoding writes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Table {
  Files,
  Dirs,
  Modes,
  Symlinks,
  Hardlinks,
  Xattrs,
  Specials,
}

impl Table {
  fn next(self) -> Option<Table> {
    match self {
      Table::Files => Some(Table::Dirs),
      Table::Dirs => Some(Table::Modes),
      Table::Modes => Some(Table::Symlinks),
      Table::Symlinks => Some(Table::Hardlinks),
      Table::Hardlinks => Some(Table::Xattrs),
      Table::Xattrs => Some(Table::Specials),
      Table::Specials => None,
    }
  }
}

/// A place in a canonical encoding: the unit [`Origin::emit`] writes next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cursor {
  /// The magic and the version.
  Header,
  /// A table's count.
  Count(Table),
  /// A table's entry.
  Entry(Table, usize),
  /// Past the last table: nothing more to write.
  Done,
}

/// An origin's identity hashed a share at a time (§4.16, A-217; §4.3 "Bounded work everywhere"): the BLAKE3 of its
/// canonical bytes, fed unit by unit from where the last share stopped, so a kernel-sized origin's identity is taken in
/// slices of a shard's step and never gathered into one buffer first. It borrows nothing between shares; its owner
/// keeps the origin unchanged until the identity is out.
#[derive(Debug)]
pub struct IdentityCursor {
  hasher: blake3::Hasher,
  at: Cursor,
}

impl IdentityCursor {
  /// A cursor at the start of `origin`'s canonical bytes; `None` when the origin is not canonical, since its bytes as
  /// it stands would not be the canonical ones ([`Origin::canonicalize`] first).
  pub fn new(origin: &Origin) -> Option<IdentityCursor> {
    origin.is_canonical().then(|| IdentityCursor {
      hasher: blake3::Hasher::new(),
      at: Cursor::Header,
    })
  }

  /// Hashes `origin`'s next units until at least `budget_bytes` have gone in or the encoding ends (a unit is one
  /// entry, so a share overruns its budget by at most one file's bytes); the identity once the last unit is in.
  pub fn step(&mut self, origin: &Origin, budget_bytes: usize) -> Option<[u8; 32]> {
    let mut fed = 0usize;
    let hasher = &mut self.hasher;
    while self.at != Cursor::Done {
      self.at = origin
        .emit(self.at, &mut |bytes| {
          fed = fed.saturating_add(bytes.len());
          hasher.update(bytes);
        })
        .unwrap_or(Cursor::Done);
      if fed >= budget_bytes {
        break;
      }
    }
    (self.at == Cursor::Done).then(|| *self.hasher.finalize().as_bytes())
  }
}

/// Whether every neighbouring pair of `table` is in order by `before` (strictly: a key twice is out of order).
fn strictly<T>(table: &[T], before: impl Fn(&T, &T) -> bool) -> bool {
  table.windows(2).all(|pair| match pair {
    [a, b] => before(a, b),
    _ => true,
  })
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

fn put_count(sink: &mut impl FnMut(&[u8]), count: usize) {
  sink(&u32::try_from(count).unwrap_or(u32::MAX).to_le_bytes());
}

fn put_bytes(sink: &mut impl FnMut(&[u8]), bytes: &[u8]) {
  put_count(sink, bytes.len());
  sink(bytes);
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
