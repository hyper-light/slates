//! A git push's record as its entry holds it (A-662; §4.17.1 "Durability and residency"): what a recovery replays
//! through the repository's store to make it again, and nothing derived.
//!
//! **What it holds** (version 2, A-723). The deepest delta chain the push's pack carried, which the store's learned depth
//! reads (A-614 part 2); each ref's move, in the order the store committed them, by the green's key in the store, the
//! green's version once the entry moved it (what a later push's old value is read back to, `super::GitStore::version_of`)
//! and the commit its new head names, or none for a delete; then every object of the push that the repository lacks when
//! the record is written (`super::record_of`: the predicate the store's commit gives positions by), in the order they
//! were admitted, each NAMED by its type, its content identity (`slates_vcs::ContentStore::object_id`) and its length;
//! then the push's pack as the client sent it, compressed and with its deltas, from which a replay reads the objects
//! again (`super::replay_git`). Positions, rows, D(k), reached sets, derived values and pack fragments are computed from
//! these again, and never recorded.
//!
//! **Why the pack, not the objects.** Version 1 held each object's bytes inflated: on the Mac Studio a 3.43 MB push of
//! git/git cut at v1.0.0 (12,232 objects) made a 63.5 MiB entry, 18 times its pack, beside the store's own inflated copy
//! (BENCHMARKS.md, "Concurrent large pushes"). The pack is git's own durable form of the same objects, so the entry now
//! costs the pack and 41 bytes a name (A-723: content once; a register entry names content by identity). Version 1
//! records are still read, since a segment holds entries written by them.
//!
//! **Written when an append commits.** The writer notes each object as the push admits it: its type, id, identity and
//! length; and it is handed the pack the receive session kept as it arrived (`slates_git::receive::Session::take_pack`).
//! The record is written whole when an append commits ([`RecordWriter::finish`]); only the first append of a push that
//! brings objects carries the pack, and an append that brings none carries no pack. Objects no client sent as a pack (the
//! server's own commits, A-664 part 3) are written as a pack of whole objects, deflated (`slates_git::pack::of_whole`),
//! their bytes read back from the content store, so every record is a pack and a replay reads it one way.
//!
//! **The encoding**, little-endian as a green's chain entry is (`slates_merge::chain_entry`): the magic and the version;
//! the deepest chain; the moves' count, then each move's green key, the green's version, a byte that marks a head
//! present or absent, and the head's raw id in the repository's format when present; the objects' count, then each
//! object's type number as a pack names it, its content identity and its length; then the pack's length and its bytes
//! (version 1: each object's type, identity, length and bytes, and no pack). Every count is checked against the bytes
//! that remain before anything is allocated, an id of another length or a type git does not name is refused, and so are
//! bytes after the pack, each typed ([`RecordRefused`]), never a panic. A replay checks the objects it reads from the
//! pack against the names, in order, before the store keeps them (`super::recorded_alike`).
//!
//! **Owed** (GAPS' §4.17 entry). A pack past an entry's bound recorded in bounded parts across entries (git/git's whole
//! pack is 607 MiB against a 746,962,928-byte segment region on the Studio); the objects the repository already holds
//! left out of the recorded pack; S3's index reading the kept pack in place (w-git's A-722).

use slates_git::object_name::{Kind, ObjectFormat, ObjectId};
use slates_git::store::kind_of;
use slates_vcs::content::IDENTITY_BYTES;
use slates_vcs::{ContentId, ContentStore};

/// Format: the record's magic, "SGPR" (a slates git push's record) little-endian, so bytes of another kind are refused
/// at the first word.
const MAGIC: u32 = u32::from_le_bytes(*b"SGPR");

/// Format: the version this build writes (A-723): the objects named, then the pack as the client sent it.
const VERSION: u32 = 2;

/// Format: the first version, each object's bytes inflated: still read, since a segment holds entries written by it.
const VERSION_INFLATED: u32 = 1;

/// Format: version 3 (A-724 piece 8c, main's ruling of 2026-11-20 (ii)): the objects' names kept as one content
/// (`super::record_names`), the record holding its identity (the BLAKE3 of the names' bytes, the identity rule) and
/// the count; written only once the names are durable at `f + 1` homes in the region (`super::homes`), so a replay on
/// any node can read them. Read always; written only where the writer is asked for it (records stay version 2 until a
/// takeover proves the region path).
const VERSION_NAMED: u32 = 3;

/// Format: the bytes of a word: the magic, the version.
const WORD_BYTES: usize = size_of::<u32>();

/// Format: the bytes of a count, a green's key, an object's length and the deepest chain.
const LONG_BYTES: usize = size_of::<u64>();

/// Format: the bytes of a move's marker and of an object's type number.
const BYTE_BYTES: usize = size_of::<u8>();

/// Format: the bytes before the moves: the magic, the version, the deepest chain and the moves' count.
const HEAD_BYTES: usize = WORD_BYTES + WORD_BYTES + LONG_BYTES + LONG_BYTES;

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

/// Why a record was not written. The push and the store are as they were.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriteRefused {
  /// The content store does not hold an object's bytes whole, for a record that writes its pack from them.
  Unreadable {
    /// The object.
    id: ObjectId,
  },
  /// A pack of whole objects not made (`slates_git::pack::of_whole`).
  Whole(slates_git::pack::WholeRefused),
  /// The record's length is past what an address can hold, or the allocator refused its room.
  Allocation,
}

/// One move a record holds: the green's key in the store, the green's version once moved, and the commit its new head
/// names, none for a delete.
pub(crate) type RecordedMove = (u64, u64, Option<ObjectId>);

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
  /// Version 2's objects, named by type, content identity and length, in the order they were admitted; empty for
  /// version 1, whose objects carry their bytes, and for version 3, whose names are a content ([`Record::named`]).
  pub(crate) names: Vec<(Kind, ContentId, u64)>,
  /// Version 3's names: the identity of the content holding them (`super::record_names`) and their count.
  pub(crate) named: Option<(ContentId, u64)>,
  /// Version 2's pack as the client sent it, compressed and with its deltas, which a replay reads its objects from;
  /// empty for version 1, and for a record that brought no object.
  pub(crate) pack: &'a [u8],
  /// The deepest delta chain the push's pack carried.
  pub(crate) deepest: u64,
  /// The moves, in the order the store committed them.
  pub(crate) moves: Vec<RecordedMove>,
  /// The objects, in the order they were admitted.
  pub(crate) objects: Vec<RecordedObject<'a>>,
}

/// A record written as its push's objects are admitted (the module docs' "written when an append commits"): each object
/// noted as it comes, the record written whole when an append commits ([`RecordWriter::finish`]).
#[derive(Clone, Debug, Default)]
pub(crate) struct RecordWriter {
  /// The push's pack as the client sent it (A-723), written in place of the objects' bytes.
  pack: Vec<u8>,
  /// The objects noted, in the order they were admitted.
  noted: Vec<Noted>,
}

impl RecordWriter {
  /// Keeps the push's pack as the client sent it (A-723), which the record holds in place of its objects' bytes.
  pub(crate) fn keep_pack(&mut self, pack: Vec<u8>) {
    self.pack = pack;
  }

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

  /// The record whole (module docs): `deepest`, `moves`, then each object noted that `lacks` says the repository lacks,
  /// its bytes read back from `content`. Refused when the content store does not hold an object's bytes whole, or the
  /// record's room is refused.
  pub(crate) fn finish(
    &self,
    (content, format): (&ContentStore, ObjectFormat),
    lacks: impl Fn(&ObjectId) -> bool,
    (deepest, moves): (u64, &[RecordedMove]),
  ) -> Result<Vec<u8>, WriteRefused> {
    self.finish_as((content, format), lacks, (deepest, moves), None)
  }

  /// The names a version-3 record of the objects `lacks` says the repository lacks keeps as a content
  /// (`super::record_names::names_to_content`), in the order they were admitted.
  pub(crate) fn names_of(&self, lacks: impl Fn(&ObjectId) -> bool) -> Vec<(Kind, ContentId, u64)> {
    self
      .lacked(lacks)
      .map(|noted| (noted.kind, noted.identity, noted.len))
      .collect()
  }

  /// [`RecordWriter::finish`] as version 3 (module docs' [`VERSION_NAMED`]): the names the content `named` holds (its
  /// identity and count, put and homed by the caller before) in place of each name.
  pub(crate) fn finish_named(
    &self,
    (content, format): (&ContentStore, ObjectFormat),
    lacks: impl Fn(&ObjectId) -> bool,
    (deepest, moves): (u64, &[RecordedMove]),
    named: (ContentId, u64),
  ) -> Result<Vec<u8>, WriteRefused> {
    self.finish_as((content, format), lacks, (deepest, moves), Some(named))
  }

  /// [`RecordWriter::finish`] and [`RecordWriter::finish_named`]: version 3 when `named`, else version 2.
  fn finish_as(
    &self,
    (content, format): (&ContentStore, ObjectFormat),
    lacks: impl Fn(&ObjectId) -> bool,
    (deepest, moves): (u64, &[RecordedMove]),
    named: Option<(ContentId, u64)>,
  ) -> Result<Vec<u8>, WriteRefused> {
    let objects: Vec<&Noted> = self.lacked(lacks).collect();
    // A record that brings no object (a later append of a push, a delete) carries no pack; objects no client sent as a
    // pack (the server's own commits) are written as one, each whole, so every record is a pack.
    let made;
    let pack: &[u8] = if objects.is_empty() {
      &[]
    } else if self.pack.is_empty() {
      made = whole_pack(content, format, &objects)?;
      &made
    } else {
      &self.pack
    };
    let inline: &[&Noted] = if named.is_some() { &[] } else { &objects };
    let len = record_len(moves, inline, pack.len()).ok_or(WriteRefused::Allocation)?;
    let mut out = Vec::new();
    out
      .try_reserve_exact(len.saturating_add(IDENTITY_BYTES))
      .map_err(|_| WriteRefused::Allocation)?;
    write_head(
      &mut out,
      if named.is_some() { VERSION_NAMED } else { VERSION },
      (deepest, moves),
    );
    match named {
      Some((identity, count)) => {
        out.extend_from_slice(&identity.0);
        out.extend_from_slice(&count.to_le_bytes());
      }
      None => {
        let count = u64::try_from(objects.len()).unwrap_or(u64::MAX);
        out.extend_from_slice(&count.to_le_bytes());
        for noted in objects {
          out.push(noted.kind.pack_number());
          out.extend_from_slice(&noted.identity.0);
          out.extend_from_slice(&noted.len.to_le_bytes());
        }
      }
    }
    let pack_len = u64::try_from(pack.len()).unwrap_or(u64::MAX);
    out.extend_from_slice(&pack_len.to_le_bytes());
    out.extend_from_slice(pack);
    Ok(out)
  }
}

/// The bytes a record of `moves` and `objects` takes; none past what an address can hold.
/// `objects`' bytes read back from `content`, as a pack of whole objects in `format` (A-723: what a record holds for
/// objects no client sent as a pack).
fn whole_pack(
  content: &ContentStore,
  format: ObjectFormat,
  objects: &[&Noted],
) -> Result<Vec<u8>, WriteRefused> {
  let mut read = Vec::with_capacity(objects.len());
  for noted in objects {
    let mut bytes = Vec::new();
    for window in content.windows(&noted.identity) {
      bytes.extend_from_slice(&window);
    }
    if u64::try_from(bytes.len()).ok() != Some(noted.len) {
      return Err(WriteRefused::Unreadable { id: noted.id });
    }
    read.push((noted.kind, bytes));
  }
  slates_git::pack::of_whole(
    format,
    read.iter().map(|(kind, bytes)| (*kind, bytes.as_slice())),
  )
  .map_err(WriteRefused::Whole)
}

fn record_len(moves: &[RecordedMove], objects: &[&Noted], pack: usize) -> Option<usize> {
  let mut len = HEAD_BYTES.checked_add(LONG_BYTES)?;
  for (_, _, head) in moves {
    let head = head.map_or(0, |id| id.as_bytes().len());
    len = len.checked_add(MOVE_MIN_BYTES)?.checked_add(head)?;
  }
  len = len.checked_add(OBJECT_MIN_BYTES.checked_mul(objects.len())?)?;
  len.checked_add(LONG_BYTES)?.checked_add(pack)
}

/// Writes the record's head, of `version`, and its moves.
fn write_head(out: &mut Vec<u8>, version: u32, (deepest, moves): (u64, &[RecordedMove])) {
  out.extend_from_slice(&MAGIC.to_le_bytes());
  out.extend_from_slice(&version.to_le_bytes());
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
  fn name(&mut self) -> Result<(Kind, ContentId, u64), RecordRefused> {
    let kind = kind_of(self.byte()?).ok_or(RecordRefused::Malformed)?;
    let identity =
      ContentId::from_slice(self.take(IDENTITY_BYTES)?).ok_or(RecordRefused::Truncated)?;
    Ok((kind, identity, self.long()?))
  }

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
    if version != VERSION && version != VERSION_INFLATED && version != VERSION_NAMED {
      return Err(RecordRefused::Version { version });
    }
    let deepest = reader.long()?;
    let count = reader.count(MOVE_MIN_BYTES)?;
    let mut moves = Vec::with_capacity(count);
    for _ in 0..count {
      moves.push(reader.one_move(format)?);
    }
    let (mut objects, mut names, mut named) = (Vec::new(), Vec::new(), None);
    let count = if version == VERSION_NAMED {
      let identity =
        ContentId::from_slice(reader.take(IDENTITY_BYTES)?).ok_or(RecordRefused::Truncated)?;
      named = Some((identity, reader.long()?));
      0
    } else {
      reader.count(OBJECT_MIN_BYTES)?
    };
    if version == VERSION_INFLATED {
      objects.reserve(count);
      for _ in 0..count {
        objects.push(reader.object()?);
      }
    } else {
      names.reserve(count);
      for _ in 0..count {
        names.push(reader.name()?);
      }
    }
    let pack = if version == VERSION_INFLATED {
      &[][..]
    } else {
      let len = usize::try_from(reader.long()?).map_err(|_| RecordRefused::Count)?;
      reader.take(len)?
    };
    if !reader.rest.is_empty() {
      return Err(RecordRefused::Trailing);
    }
    Ok(Record {
      names,
      named,
      pack,
      deepest,
      moves,
      objects,
    })
  }
}
