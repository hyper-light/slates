//! A git push's record as its entry holds it (A-662; §4.17.1 "Durability and residency"; A-767 C5): what a recovery
//! replays through the repository's store to make it again, and nothing derived.
//!
//! **What it holds** (version 6, C5 and A-850). The deepest delta chain the push's pack carried, which the store's
//! learned depth reads (A-614 part 2); each ref's move, in the order the store committed them, by the green's key in the
//! store, the green's version once the entry moved it (what a later push's old value is read back to,
//! `super::GitStore::version_of`), the commit its new head names, or none for a delete, and the version it was made at
//! (A-850: the green and its version whose head is the move's head or the commit its tags peel to, found at its
//! admission, `slates_git::store::Move::base`), or none; then the names of the objects of the push that the
//! repository lacks when the record is written (`super::SealedPush::names_now`: the predicate the store's commit gives
//! positions by), kept as one content (`super::record_names`), and the push's pack as another, each by its identity
//! (the BLAKE3 of its bytes) with the names' count and the pack's length; then the sequence of the changeset the node's
//! store landed them as (its `Names` and `Payload` sections). Positions, rows, D(k), reached sets, derived values and
//! pack fragments are computed from these again, and never recorded; the base is recorded because what a version's sets
//! are relative to is decided by the heads its admission met, which a replay must not decide again
//! (`slates_git::store::Repository::based`).
//!
//! **Why no object and no pack.** Version 1 held each object's bytes inflated (a 3.43 MB push of git/git cut at v1.0.0
//! made a 63.5 MiB entry, BENCHMARKS.md, "Concurrent large pushes"); version 2 (A-723) held the pack as the client sent
//! it with each object's name. Both put a push's bytes in the register's entry. Version 6, as version 5 before it, is
//! written only once the names
//! and the pack are durable at `f + 1` homes in the region (`super::homes`) and the node's store answered the changeset
//! durable (main's ruling of 2026-11-20: land first, then the entry naming the sequence), so its append is O(moves). A
//! git commit the server makes lands the same way (C5 part 2b piece 3, `super::commits`); versions 1 and 2 are no
//! longer written or read: a log holding one is refused `RecordRefused::Retired` (never skipped); 3, 4 and 5, which
//! version 6 replaced, are refused as any unknown version is (A-850: a version-5 record names no move's base, so its
//! replay could not hold its sets to the ones the live store derived; there is no shim reading it).
//!
//! **Written when an append commits.** The writer notes each object as the push admits it: its type, id, identity and
//! length ([`RecordWriter::names_of`] reads the names an append records); the record is written by
//! [`RecordWriter::finish_packed`] from the identities the homing gave. An append that brings no object the repository
//! lacks (a delete, a ref moved to a commit it holds, a non-atomic push's later appends) names none: the names' and the
//! pack's identities all zeros with a count and a length of 0, and a sequence of 0.
//!
//! **The encoding**, little-endian as a green's chain entry is (`slates_merge::chain_entry`): the magic and the version;
//! the deepest chain; the moves' count, then each move's green key, the green's version, a byte that marks a head
//! present or absent, the head's raw id in the repository's format when present, a byte that marks a base present or
//! absent, and the base's green key and version when present; the names' identity and count; the
//! pack's identity and length; the sequence. The moves' count is checked against the bytes that remain before anything
//! is allocated, an id of another length is refused, and so are bytes after the sequence, each typed
//! ([`RecordRefused`]), never a panic. A replay checks the objects it reads from the pack against the names, as sets,
//! before the store keeps them (`super::recorded_alike`).

use slates_git::object_name::{Kind, ObjectFormat, ObjectId};
use slates_vcs::ContentId;
use slates_vcs::content::IDENTITY_BYTES;

/// Format: the record's magic, "SGPR" (a slates git push's record) little-endian, so bytes of another kind are refused
/// at the first word.
const MAGIC: u32 = u32::from_le_bytes(*b"SGPR");

/// Format: version 6 (A-850: each move's base after its head, module docs; it replaces version 5, A-767 C5, git onto
/// the changeset store, which replaced version 4, A-722's record v4, which replaced version 3, A-724 piece 8c): the
/// objects' names kept as one content (`super::record_names`) and the push's pack as
/// another (the one its staging put in the store, its objects' rows naming places in it), the record holding each one's
/// identity (the BLAKE3 of its bytes, the identity rule) and the names' count and the pack's length, then the sequence of
/// the changeset the node's store landed them as (its `Names` and `Payload` sections; 0 where the daemon holds no
/// store). Written only once both contents are durable at `f + 1` homes in the region (`super::homes`), so a replay on
/// any node can read them, and once the store answered the landing durable, so the register names only a changeset
/// that is there (main's ruling of 2026-11-20: land first, then the entry naming the sequence). The record so holds no
/// object and no pack: its append is O(moves). An append that brings no object the repository lacks (a delete, a ref
/// moved to a commit it holds, a non-atomic push's later appends) names none: the names' and the pack's identities all
/// zeros with a count and a length of 0, and a sequence of 0 (C5 part 2b).
/// Format: this version's number on the wire.
pub(crate) const VERSION_LANDED: u32 = 6;

/// Format: the versions retired by A-824 (C5 part 2b piece 3): 1, each object inflated, and 2 (A-723), the objects
/// named and the pack inline. Refused [`RecordRefused::Retired`]; 3 and 4, which version 5 replaced, and 5, which
/// version 6 replaced (A-850), are refused as any unknown version is.
const RETIRED: [u32; 2] = [1, 2];

/// Format: a record's identity of names or of a pack it does not name (an append that brings no object): all
/// zeros, which no BLAKE3 output is in practice.
const NONE: ContentId = ContentId([0; IDENTITY_BYTES]);

/// Format: the bytes of a word: the magic, the version.
const WORD_BYTES: usize = size_of::<u32>();

/// Format: the bytes of a count, a green's key, an object's length and the deepest chain.
const LONG_BYTES: usize = size_of::<u64>();

/// Format: the bytes of a move's marker and of an object's type number.
const BYTE_BYTES: usize = size_of::<u8>();

/// Format: the bytes before the moves: the magic, the version, the deepest chain and the moves' count.
const HEAD_BYTES: usize = WORD_BYTES + WORD_BYTES + LONG_BYTES + LONG_BYTES;

/// Format: the byte that marks a move's head, or its base, present.
const PRESENT: u8 = 1;

/// Format: the byte that marks a move's head absent (a delete), or its base absent.
const ABSENT: u8 = 0;

/// Format: the fewest bytes a move takes: its key, its version, its head's marker and its base's marker.
const MOVE_MIN_BYTES: usize = LONG_BYTES + LONG_BYTES + BYTE_BYTES + BYTE_BYTES;

/// Format: the bytes a present base takes after its marker: its green's key and its version.
const BASE_BYTES: usize = LONG_BYTES + LONG_BYTES;

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
  /// A retired version, which carried a push's objects in the register's entry: version 1 (each object inflated) or
  /// version 2 (A-723, the objects named and the pack inline), retired by A-824 (C5 part 2b piece 3: every record names
  /// its changeset on the node's store, version 5). A log holding one was written before that change and is not
  /// replayed: refused here, never skipped.
  Retired {
    /// The version read.
    version: u32,
  },
  /// A count past what the bytes that remain could hold.
  Count,
  /// A move's head or base marker that is neither present nor absent.
  Marker,
  /// An id not of the repository's format.
  Malformed,
  /// Bytes after the sequence.
  Trailing,
}

/// Why a record was not written. The push and the store are as they were.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriteRefused {
  /// The record's length is past what an address can hold, or the allocator refused its room.
  Allocation,
  /// The moves and their bases are not one each.
  Unaligned,
}

/// One move a record holds: the green's key in the store, the green's version once moved, and the commit its new head
/// names, none for a delete.
pub(crate) type RecordedMove = (u64, u64, Option<ObjectId>);

/// One move's base as a record holds it (A-850): the green's key and its version the move was made at, or none.
pub(crate) type RecordedBase = Option<(u64, u64)>;

/// One object a push admitted, as the writer notes it: never its bytes, which its root names in the content store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Noted {
  /// Its type.
  pub(crate) kind: Kind,
  /// Its id.
  pub(crate) id: ObjectId,
  /// The content identity it is held under: its root.
  pub(crate) identity: ContentId,
  /// Its length.
  pub(crate) len: u64,
}

/// A record read back (module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Record {
  /// The names: the identity of the content holding them (`super::record_names`) and their count; none for an append
  /// that brings no object.
  pub(crate) named: Option<(ContentId, u64)>,
  /// The pack: the identity of the content holding it and its length; none for an append that brings no object.
  pub(crate) packed: Option<(ContentId, u64)>,
  /// The changeset: the sequence the node's store landed the names and the pack as; 0 for none (a daemon with no store,
  /// or an append that brings no object).
  pub(crate) sequence: u64,
  /// The deepest delta chain the push's pack carried.
  pub(crate) deepest: u64,
  /// The moves, in the order the store committed them.
  pub(crate) moves: Vec<RecordedMove>,
  /// Each move's base, one for each of `moves` and in their order (A-850).
  pub(crate) bases: Vec<RecordedBase>,
}

/// A record written as its push's objects are admitted (the module docs' "written when an append commits"): each object
/// noted as it comes, the record written when an append commits ([`RecordWriter::finish_packed`]).
#[derive(Clone, Debug, Default)]
pub(crate) struct RecordWriter {
  /// The objects noted, in the order they were admitted.
  noted: Vec<Noted>,
}

impl RecordWriter {
  /// Notes one object as the push admits it.
  pub(crate) fn object(&mut self, noted: Noted) {
    self.noted.push(noted);
  }

  /// The objects noted that `lacks` says the repository lacks, in the order they were admitted.
  pub(crate) fn lacked<'w>(
    &'w self,
    lacks: impl Fn(&ObjectId) -> bool + 'w,
  ) -> impl Iterator<Item = &'w Noted> + 'w {
    self.noted.iter().filter(move |noted| lacks(&noted.id))
  }

  /// The names a version-3 record of the objects `lacks` says the repository lacks keeps as a content
  /// (`super::record_names::names_to_content`), in the order they were admitted.
  pub(crate) fn names_of(&self, lacks: impl Fn(&ObjectId) -> bool) -> Vec<(Kind, ContentId, u64)> {
    self
      .lacked(lacks)
      .map(|noted| (noted.kind, noted.identity, noted.len))
      .collect()
  }

  /// The record as version 6 (module docs' [`VERSION_LANDED`]): `deepest` and `moves` with their `bases`, one each
  /// (refused [`WriteRefused::Unaligned`] otherwise), then the names the content
  /// `named` holds (its identity and count) and the pack the content `packed` holds (its identity and length), both put
  /// and homed by the caller before, in place of each name and of the pack's bytes, then `sequence`, the changeset the
  /// node's store landed them as (0 for none).
  pub(crate) fn finish_packed(
    (deepest, moves, bases): (u64, &[RecordedMove], &[RecordedBase]),
    named: Option<(ContentId, u64)>,
    packed: Option<(ContentId, u64)>,
    sequence: u64,
  ) -> Result<Vec<u8>, WriteRefused> {
    if moves.len() != bases.len() {
      return Err(WriteRefused::Unaligned);
    }
    let len = record_len(moves, bases)
      .and_then(|len| len.checked_add(2 * IDENTITY_BYTES))
      .and_then(|len| len.checked_add(LONG_BYTES))
      .ok_or(WriteRefused::Allocation)?;
    let mut out = Vec::new();
    out
      .try_reserve_exact(len)
      .map_err(|_| WriteRefused::Allocation)?;
    write_head(&mut out, VERSION_LANDED, (deepest, moves, bases));
    for (identity, count) in [named, packed].map(|named| named.unwrap_or((NONE, 0))) {
      out.extend_from_slice(&identity.0);
      out.extend_from_slice(&count.to_le_bytes());
    }
    out.extend_from_slice(&sequence.to_le_bytes());
    Ok(out)
  }
}

/// The bytes a record's head, `moves` and their `bases` take; none past what an address can hold.
fn record_len(moves: &[RecordedMove], bases: &[RecordedBase]) -> Option<usize> {
  let mut len = HEAD_BYTES;
  for ((_, _, head), base) in moves.iter().zip(bases) {
    let head = head.map_or(0, |id| id.as_bytes().len());
    let base = base.map_or(0, |_| BASE_BYTES);
    len = len
      .checked_add(MOVE_MIN_BYTES)?
      .checked_add(head)?
      .checked_add(base)?;
  }
  Some(len)
}

/// Writes the record's head, of `version`, and its moves with their bases.
fn write_head(
  out: &mut Vec<u8>,
  version: u32,
  (deepest, moves, bases): (u64, &[RecordedMove], &[RecordedBase]),
) {
  out.extend_from_slice(&MAGIC.to_le_bytes());
  out.extend_from_slice(&version.to_le_bytes());
  out.extend_from_slice(&deepest.to_le_bytes());
  let count = u64::try_from(moves.len()).unwrap_or(u64::MAX);
  out.extend_from_slice(&count.to_le_bytes());
  for ((key, version, head), base) in moves.iter().zip(bases) {
    out.extend_from_slice(&key.to_le_bytes());
    out.extend_from_slice(&version.to_le_bytes());
    match head {
      Some(id) => {
        out.push(PRESENT);
        out.extend_from_slice(id.as_bytes());
      }
      None => out.push(ABSENT),
    }
    match base {
      Some((green, at)) => {
        out.push(PRESENT);
        out.extend_from_slice(&green.to_le_bytes());
        out.extend_from_slice(&at.to_le_bytes());
      }
      None => out.push(ABSENT),
    }
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
    let bytes =
      <[u8; WORD_BYTES]>::try_from(self.take(WORD_BYTES)?).map_err(|_| RecordRefused::Truncated)?;
    Ok(u32::from_le_bytes(bytes))
  }

  /// The next long.
  fn long(&mut self) -> Result<u64, RecordRefused> {
    let bytes =
      <[u8; LONG_BYTES]>::try_from(self.take(LONG_BYTES)?).map_err(|_| RecordRefused::Truncated)?;
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

  /// One move, its head an id of `format`, and its base.
  fn one_move(
    &mut self,
    format: ObjectFormat,
  ) -> Result<(RecordedMove, RecordedBase), RecordRefused> {
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
    let base = match self.byte()? {
      PRESENT => Some((self.long()?, self.long()?)),
      ABSENT => None,
      _ => return Err(RecordRefused::Marker),
    };
    Ok(((key, version, head), base))
  }
}

impl Record {
  /// Reads `bytes` as the record of a push to a repository of `format` (module docs).
  pub(crate) fn decode(bytes: &[u8], format: ObjectFormat) -> Result<Record, RecordRefused> {
    let mut reader = Reader { rest: bytes };
    if reader.word()? != MAGIC {
      return Err(RecordRefused::Magic);
    }
    let version = reader.word()?;
    if RETIRED.contains(&version) {
      return Err(RecordRefused::Retired { version });
    }
    if version != VERSION_LANDED {
      return Err(RecordRefused::Version { version });
    }
    let deepest = reader.long()?;
    let count = reader.count(MOVE_MIN_BYTES)?;
    let (mut moves, mut bases) = (Vec::with_capacity(count), Vec::with_capacity(count));
    for _ in 0..count {
      let (recorded, base) = reader.one_move(format)?;
      moves.push(recorded);
      bases.push(base);
    }
    let (mut named, mut packed) = (None, None);
    for kept in [&mut named, &mut packed] {
      let identity =
        ContentId::from_slice(reader.take(IDENTITY_BYTES)?).ok_or(RecordRefused::Truncated)?;
      let count = reader.long()?;
      *kept = (identity != NONE).then_some((identity, count));
    }
    let sequence = reader.long()?;
    if !reader.rest.is_empty() {
      return Err(RecordRefused::Trailing);
    }
    Ok(Record {
      named,
      packed,
      sequence,
      deepest,
      moves,
      bases,
    })
  }
}
