//! A git push's record as its entry holds it (A-662; §4.17.1 "Durability and residency"): what a recovery replays
//! through the repository's store to make it again, and nothing derived.
//!
//! **What it holds.** The deepest delta chain the push's pack carried, which the store's learned depth reads (A-614 part
//! 2); each ref's move, in the order the store committed them, by the green's key in the store, the green's version
//! once the entry moved it (what a later push's old value is read back to, `super::GitStore::version_of`) and the commit
//! its new head names, or none for a delete; then every object the push brought that the repository did not hold, in the order
//! they were admitted, each by its type, its content identity (`slates_vcs::ContentStore::object_id`: the BLAKE3 the
//! content plane holds it under, over its windows when it is longer than one) and its bytes. Positions, rows, D(k),
//! reached sets, derived values and pack fragments are computed from these again, and never recorded.
//!
//! **The encoding**, little-endian as a green's chain entry is (`slates_merge::chain_entry`): the magic and the version;
//! the deepest chain; the moves' count, then each move's green key, the green's version, a byte that marks a head
//! present or absent, and the head's raw id in the repository's format when present; the objects' count, then each object's type number as a pack
//! names it, its content identity, its length and its bytes. Every count is checked against the bytes that remain before
//! anything is allocated, an id of another length or a type git does not name is refused, and so are bytes after the
//! last object, each typed ([`RecordRefused`]), never a panic. The identity is checked against the bytes by the replay,
//! before it admits them (`super::replay_git`).

use slates_git::object_name::{Kind, ObjectFormat, ObjectId};
use slates_git::store::kind_of;
use slates_vcs::ContentId;
use slates_vcs::content::IDENTITY_BYTES;

/// Format: the record's magic, "SGPR" (a slates git push's record) little-endian, so bytes of another kind are refused
/// at the first word.
const MAGIC: u32 = u32::from_le_bytes(*b"SGPR");

/// Format: the record's version; a decoder refuses any other.
const VERSION: u32 = 1;

/// Format: the bytes of a word: the magic, the version.
const WORD_BYTES: usize = size_of::<u32>();

/// Format: the bytes of a count, a green's key, an object's length and the deepest chain.
const LONG_BYTES: usize = size_of::<u64>();

/// Format: the bytes of a move's marker and of an object's type number.
const BYTE_BYTES: usize = size_of::<u8>();

/// Format: the byte that marks a move's head present.
const PRESENT: u8 = 1;

/// Format: the byte that marks a move's head absent: a delete.
const ABSENT: u8 = 0;

/// Format: the fewest bytes a move takes: its key, its version and its marker.
const MOVE_MIN_BYTES: usize = LONG_BYTES + LONG_BYTES + BYTE_BYTES;

/// Format: the fewest bytes an object takes: its type number, its identity and its length.
const OBJECT_MIN_BYTES: usize = BYTE_BYTES + IDENTITY_BYTES + LONG_BYTES;

/// Why a record was refused. A refused record replays nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecordRefused {
  /// The bytes end before what they declare.
  Truncated,
  /// Bytes of another kind: the magic is not a record's.
  Magic,
  /// A version this build does not read.
  Version {
    /// The version read.
    version: u32,
  },
  /// A count past what the bytes that remain could hold.
  Count,
  /// A move's marker that is neither present nor absent.
  Marker,
  /// An id not of the repository's format, or a type number git does not name for an object.
  Malformed,
  /// Bytes after the last object.
  Trailing,
}

/// One move a record holds: the green's key in the store, the green's version once moved, and the commit its new head
/// names, none for a delete.
pub(crate) type RecordedMove = (u64, u64, Option<ObjectId>);

/// One object a record holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RecordedObject<'a> {
  /// Its type.
  pub(crate) kind: Kind,
  /// The content identity it is held under.
  pub(crate) identity: ContentId,
  /// Its content: the bytes git hashes after the header.
  pub(crate) content: &'a [u8],
}

/// A record read back (module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Record<'a> {
  /// The deepest delta chain the push's pack carried.
  pub(crate) deepest: u64,
  /// The moves, in the order the store committed them.
  pub(crate) moves: Vec<RecordedMove>,
  /// The objects, in the order they were admitted.
  pub(crate) objects: Vec<RecordedObject<'a>>,
}

/// A record written as its push's objects are admitted (the module docs' encoding): the objects as they come, then the
/// deepest chain and the moves once the push is sealed ([`RecordWriter::finish`]).
#[derive(Clone, Debug, Default)]
pub(crate) struct RecordWriter {
  /// The objects' part, encoded.
  objects: Vec<u8>,
  /// How many objects it holds.
  count: u64,
}

impl RecordWriter {
  /// Writes one object: its type, the identity it is held under, and its content.
  pub(crate) fn object(&mut self, kind: Kind, identity: ContentId, content: &[u8]) {
    self.objects.push(kind.pack_number());
    self.objects.extend_from_slice(&identity.0);
    let len = u64::try_from(content.len()).unwrap_or(u64::MAX);
    self.objects.extend_from_slice(&len.to_le_bytes());
    self.objects.extend_from_slice(content);
    self.count = self.count.saturating_add(1);
  }

  /// The record whole: the objects written so far after `deepest` and `moves`.
  pub(crate) fn finish(self, deepest: u64, moves: &[RecordedMove]) -> Vec<u8> {
    let heads = moves
      .iter()
      .map(|(_, _, head)| head.map_or(0, |id| id.as_bytes().len()))
      .fold(0, usize::saturating_add);
    let capacity = [
      WORD_BYTES.saturating_mul(2),
      LONG_BYTES.saturating_mul(3),
      MOVE_MIN_BYTES.saturating_mul(moves.len()),
      heads,
      self.objects.len(),
    ]
    .into_iter()
    .fold(0, usize::saturating_add);
    let mut out = Vec::with_capacity(capacity);
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&deepest.to_le_bytes());
    let count = u64::try_from(moves.len()).unwrap_or(u64::MAX);
    out.extend_from_slice(&count.to_le_bytes());
    for (key, version, head) in moves {
      out.extend_from_slice(&key.to_le_bytes());
      out.extend_from_slice(&version.to_le_bytes());
      match head {
        Some(id) => {
          out.push(PRESENT);
          out.extend_from_slice(id.as_bytes());
        }
        None => out.push(ABSENT),
      }
    }
    out.extend_from_slice(&self.count.to_le_bytes());
    out.extend_from_slice(&self.objects);
    out
  }
}

/// The bytes a record has left, read from the front.
struct Reader<'a> {
  /// What remains.
  rest: &'a [u8],
}

impl<'a> Reader<'a> {
  /// The next `len` bytes.
  fn take(&mut self, len: usize) -> Result<&'a [u8], RecordRefused> {
    let (taken, rest) = self
      .rest
      .split_at_checked(len)
      .ok_or(RecordRefused::Truncated)?;
    self.rest = rest;
    Ok(taken)
  }

  /// The next byte.
  fn byte(&mut self) -> Result<u8, RecordRefused> {
    self
      .take(BYTE_BYTES)?
      .first()
      .copied()
      .ok_or(RecordRefused::Truncated)
  }

  /// The next word.
  fn word(&mut self) -> Result<u32, RecordRefused> {
    let bytes = <[u8; WORD_BYTES]>::try_from(self.take(WORD_BYTES)?)
      .map_err(|_| RecordRefused::Truncated)?;
    Ok(u32::from_le_bytes(bytes))
  }

  /// The next long.
  fn long(&mut self) -> Result<u64, RecordRefused> {
    let bytes = <[u8; LONG_BYTES]>::try_from(self.take(LONG_BYTES)?)
      .map_err(|_| RecordRefused::Truncated)?;
    Ok(u64::from_le_bytes(bytes))
  }

  /// A count of items each at least `least` bytes long, refused when the bytes that remain could not hold that many:
  /// so nothing is allocated for more items than the record could hold.
  fn count(&mut self, least: usize) -> Result<usize, RecordRefused> {
    let count = usize::try_from(self.long()?).map_err(|_| RecordRefused::Count)?;
    let needed = count.checked_mul(least).ok_or(RecordRefused::Count)?;
    if needed > self.rest.len() {
      return Err(RecordRefused::Count);
    }
    Ok(count)
  }

  /// One move, its head an id of `format`.
  fn one_move(&mut self, format: ObjectFormat) -> Result<RecordedMove, RecordRefused> {
    let key = self.long()?;
    let version = self.long()?;
    let head = match self.byte()? {
      PRESENT => {
        let bytes = self.take(format.raw_len())?;
        Some(ObjectId::from_bytes(format, bytes).ok_or(RecordRefused::Malformed)?)
      }
      ABSENT => None,
      _ => return Err(RecordRefused::Marker),
    };
    Ok((key, version, head))
  }

  /// One object.
  fn object(&mut self) -> Result<RecordedObject<'a>, RecordRefused> {
    let kind = kind_of(self.byte()?).ok_or(RecordRefused::Malformed)?;
    let identity =
      ContentId::from_slice(self.take(IDENTITY_BYTES)?).ok_or(RecordRefused::Truncated)?;
    let len = usize::try_from(self.long()?).map_err(|_| RecordRefused::Count)?;
    let content = self.take(len)?;
    Ok(RecordedObject {
      kind,
      identity,
      content,
    })
  }
}

impl<'a> Record<'a> {
  /// Reads `bytes` as the record of a push to a repository of `format` (module docs).
  pub(crate) fn decode(bytes: &'a [u8], format: ObjectFormat) -> Result<Record<'a>, RecordRefused> {
    let mut reader = Reader { rest: bytes };
    if reader.word()? != MAGIC {
      return Err(RecordRefused::Magic);
    }
    let version = reader.word()?;
    if version != VERSION {
      return Err(RecordRefused::Version { version });
    }
    let deepest = reader.long()?;
    let count = reader.count(MOVE_MIN_BYTES)?;
    let mut moves = Vec::with_capacity(count);
    for _ in 0..count {
      moves.push(reader.one_move(format)?);
    }
    let count = reader.count(OBJECT_MIN_BYTES)?;
    let mut objects = Vec::with_capacity(count);
    for _ in 0..count {
      objects.push(reader.object()?);
    }
    if !reader.rest.is_empty() {
      return Err(RecordRefused::Trailing);
    }
    Ok(Record {
      deepest,
      moves,
      objects,
    })
  }
}
