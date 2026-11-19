//! The pack reader (A-608; §4.17.2 "Pieces (git)", piece 3, and "A push", item 3): a pack read as its bytes arrive,
//! each entry inflated to exactly the size its header names, each delta resolved against its base through the applier,
//! every object named, and the trailer checked.
//!
//! **Sources** (read 2026-11-19, git v2.50.1). The document \[B\]: `gitformat-pack.adoc`, a pack's header, entries and
//! trailer (lines 35-68) and its deltas (95-175). The code \[C\], which git's receive-pack runs on every push and which
//! is followed where the document leaves room: `builtin/index-pack.c`,
//! - `parse_pack_header`, the header;
//! - `unpack_entry_data` (476-518), each entry inflated into a buffer of exactly its size, refused unless the stream
//!   ends there ("inflate returned %d");
//! - `parse_pack_objects`, every byte before the trailer hashed and the trailer compared ("pack is corrupted (SHA1
//!   mismatch)"), and "premature end of pack file";
//! - `resolve_deltas` with `find_ofs_delta_children` and `find_ref_delta_children`, a delta resolved once its base is,
//!   its children found in tables sorted by their base's offset and by their base's name;
//! - `conclude_pack`, a delta never resolved refused ("pack has %d unresolved deltas").
//!
//! The pack's header and trailer are A-591's ([`pack::read_header`], [`pack::Refusal`]); each entry's header and each
//! delta are A-582 part 3's ([`entry::read`], [`delta::apply`]); each name is A-600's ([`name_of`]). The inflater is
//! flate2 over zlib-rs, the workspace's one deflate engine (A-607).
//!
//! **What it does.** [`Reader::read`] takes the bytes a stream has delivered, in any cuts, and reads as far as they go:
//! the pack's header, then each entry's header, then its data inflated as it arrives, then the trailer, which must be
//! the checksum of every byte before it. An object stored whole is named as soon as its data ends. A delta is applied
//! as soon as its base is resolved: an offset delta's base always lies before it; a reference delta's base is placed by
//! the caller's index ([`Locate`], which A-591's [`Idx`] answers) or found among the pack's own objects by name. A
//! delta whose base is not yet resolved waits. [`Reader::finish`] resolves the waiting deltas, each once its base is,
//! in tables sorted by their base's entry and by their base's name, with an explicit stack in place of recursion, and
//! answers every object in pack order; [`Reader::finish_with`] reads a thin pack, taking the bases it names from
//! outside it ([`Outside`], the receiver's repository, A-620). [`read_whole`] reads a pack held whole; [`check_index`]
//! holds an index to the objects read from its pack, each id at its object's offset, which A-591 left to this piece.
//!
//! **Hostile input (§4.9).** Every byte is untrusted, and every refusal is a typed variant naming where it stopped:
//! - a stream that is no zlib stream, or whose data or Adler-32 is wrong (`Inflate`); one that inflates past its
//!   header's size (`LongerThanSize`) or ends short of it (`ShorterThanSize`); and one that has not ended when the
//!   bytes do (`Truncated`), as git refuses each;
//! - a base offset that names no entry's first byte (`BaseNotAnEntry`); a reference delta's base found nowhere
//!   (`DeltaBaseMissing`); a base the index placed at an object of another name (`BaseMisplaced`); a chain of deltas
//!   whose bases lead back to itself, which only an index can make, an offset delta's base always lying before it
//!   (`Cycle`);
//! - the pack's header and trailer as A-591 refuses them, an entry's header as A-582 refuses it, a delta as the applier
//!   refuses it, and an object whose content carries a SHA-1 collision attack (A-600).
//!
//! **Bounds, none of them a literal.** The input bounds what is read; the caller's [`Admission`] bounds what is built,
//! each size charged before it is allocated: a record for each object the header counts, each entry's inflated size
//! (charged when its header is whole), each delta's target (charged by the applier), and the tables of waiting deltas
//! and of the answer. So a chain's depth is bounded by the admission, never by a number: each delta in a chain is a
//! target charged. Memory is reserved with `try_reserve_exact`, so no allocation can panic. The inflater's own state is
//! one fixed size for a reader, zlib's documented window and state, not scaled by the input, and is not charged.

use flate2::{Decompress, FlushDecompress, Status};

use crate::delta;
use crate::entry::{self, PACK_HEADER_LEN, Representation};
use crate::object_name::{CollisionAttack, Hasher, Kind, ObjectFormat, ObjectId, name_of};
use crate::pack::{self, idx::Idx};
use crate::{Admission, PastAdmission, to_u64};

/// Where the object named `id` begins in the pack being read: what an index answers, such as A-591's [`Idx`].
pub trait Locate {
  /// The offset of the entry that holds the object named `id`, if this index knows it.
  fn offset_of(&self, id: &ObjectId) -> Option<u64>;
}

impl Locate for Idx<'_> {
  fn offset_of(&self, id: &ObjectId) -> Option<u64> {
    Idx::offset_of(self, id)
  }
}

/// No index: a reference delta's base is found among the objects the pack itself names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NoIndex;

impl Locate for NoIndex {
  fn offset_of(&self, _id: &ObjectId) -> Option<u64> {
    None
  }
}

/// Objects outside the pack that a thin pack's reference deltas may name: what the receiver's repository holds
/// (§4.17.2 "A push", item 3: a delta resolved "against a base in the repository's index"; A-620).
pub trait Outside {
  /// The object named `id`, its type and content, if it is held outside the pack.
  fn object(&self, id: &ObjectId) -> Option<(Kind, &[u8])>;
}

/// Nothing outside the pack: every base must be one of its own objects.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NoObjects;

impl Outside for NoObjects {
  fn object(&self, _id: &ObjectId) -> Option<(Kind, &[u8])> {
    None
  }
}

/// A delta's place in its chain, as `git verify-pack -v` lists it: the object it was applied to, and how many deltas
/// deep it lies (1 for a delta against an object stored whole).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chain {
  /// The name of the object the delta was applied to.
  pub base: ObjectId,
  /// The deltas between the object and one stored whole, itself included.
  pub depth: u64,
}

/// An object read from a pack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Object {
  /// Where its entry begins in the pack.
  pub offset: u64,
  /// The bytes its entry takes in the pack: its header and its compressed data.
  pub stored: u64,
  /// Its type; a delta's is its base's.
  pub kind: Kind,
  /// Its name.
  pub id: ObjectId,
  /// Its content.
  pub content: Vec<u8>,
  /// For an object stored as a delta, its place in its chain.
  pub chain: Option<Chain>,
}

/// A pack read whole: its objects in pack order, and its trailer, the checksum of every byte before it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unpacked {
  /// Its objects, in pack order.
  pub objects: Vec<Object>,
  /// Its trailer.
  pub checksum: ObjectId,
}

/// Why bytes are not a pack, or not one this reader could resolve within its admission. Each names where it stopped,
/// as an offset into the pack: an entry's refusal names where the entry begins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// The pack's header or trailer, as A-591 refuses them: a signature or a version git does not read, a trailer that is
  /// not the checksum of the bytes before it, or bytes that carry a collision attack.
  Pack(pack::Refusal),
  /// An entry's header, as A-582 part 3 refuses it, its size past the admission included.
  Entry(entry::Refusal),
  /// An entry's data is no zlib stream the inflater accepts: a bad header, bad data, a wrong Adler-32 or a preset
  /// dictionary (git's "inflate returned %d").
  Inflate {
    /// Where the entry begins.
    at: u64,
  },
  /// An entry's data inflates past the size its header names.
  LongerThanSize {
    /// Where the entry begins.
    at: u64,
    /// The size its header names.
    size: u64,
  },
  /// An entry's stream ends before the size its header names.
  ShorterThanSize {
    /// Where the entry begins.
    at: u64,
    /// The size its header names.
    size: u64,
    /// The bytes it inflated to.
    inflated: u64,
  },
  /// The bytes ended inside the pack: in its header, an entry, a stream not yet ended, or its trailer (git's "premature
  /// end of pack file").
  Truncated {
    /// The bytes read.
    at: u64,
  },
  /// Bytes followed the trailer of a pack given whole (git's "pack has junk at the end").
  TrailingBytes {
    /// Where the trailer ended.
    at: u64,
  },
  /// A delta's base offset, its own or the one an index gave, names no entry's first byte.
  BaseNotAnEntry {
    /// Where the delta begins.
    at: u64,
    /// The offset named.
    base: u64,
  },
  /// A reference delta's base is neither among the pack's objects nor placed by the index (git's "pack has %d
  /// unresolved deltas").
  DeltaBaseMissing {
    /// Where the delta begins.
    at: u64,
    /// The base's name.
    base: ObjectId,
  },
  /// The index placed a reference delta's base at an object of another name.
  BaseMisplaced {
    /// Where the delta begins.
    at: u64,
    /// The base's name, as the delta gives it.
    base: ObjectId,
    /// The name of the object the index placed it at.
    found: ObjectId,
  },
  /// A chain of deltas whose bases lead back to itself.
  Cycle {
    /// Where the first delta of the chain, in pack order, begins.
    at: u64,
  },
  /// The applier refused a delta against its base, its target's admission included.
  Delta {
    /// Where the delta begins.
    at: u64,
    /// The applier's refusal.
    refusal: delta::Refusal,
  },
  /// An object's content carries a known SHA-1 collision attack's disturbance (A-600).
  Collision {
    /// Where the object's entry begins.
    at: u64,
    /// The attack, named by its plain SHA-1.
    attack: CollisionAttack,
  },
  /// The records of the objects the header counts, or a table of waiting deltas, or the answer, would take more than
  /// the admission had left.
  PastAdmission(PastAdmission),
  /// The system refused memory the admission held.
  AllocationRefused {
    /// The bytes asked for.
    bytes: u64,
  },
}

/// Why an index does not index the objects read from its pack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexMismatch {
  /// The index is of another pack: its copy of the trailer differs.
  Checksum {
    /// The index's copy.
    index: ObjectId,
    /// The pack's trailer.
    pack: ObjectId,
  },
  /// The index names another number of objects.
  Count {
    /// The index's count.
    index: u64,
    /// The objects read.
    pack: u64,
  },
  /// An object of the pack the index does not place at its entry.
  Offset {
    /// The object's name.
    id: ObjectId,
    /// Where its entry begins.
    pack: u64,
    /// Where the index places it, if anywhere.
    index: Option<u64>,
  },
}

/// What comes next in the pack.
enum Stage {
  /// The pack's header.
  PackHeader,
  /// An entry's header.
  EntryHeader,
  /// An entry's data, being inflated.
  Data(Data),
  /// The trailer.
  Trailer,
  /// Nothing: the trailer has been read and holds.
  Done(ObjectId),
}

/// An entry whose header is whole and whose data is being inflated.
struct Data {
  /// Its header.
  header: entry::Header,
  /// Where it begins.
  began: u64,
  /// Room for exactly its size, filled from the start.
  content: Vec<u8>,
  /// The bytes inflated so far.
  filled: usize,
}

/// An entry read.
struct Record {
  /// Where it begins.
  offset: u64,
  /// The bytes it takes.
  stored: u64,
  /// Its object, or its delta waiting for its base.
  state: State,
}

/// An entry's object, or its delta waiting for its base.
enum State {
  /// The object, named.
  Resolved(Resolved),
  /// A delta whose base is not yet resolved.
  Waiting(Waiting),
}

/// An object resolved.
struct Resolved {
  /// Its type.
  kind: Kind,
  /// Its name.
  id: ObjectId,
  /// Its content.
  content: Vec<u8>,
  /// For a delta, its place in its chain.
  chain: Option<Chain>,
}

/// An object a delta is applied to: one of the pack's, or one outside it.
struct Source<'s> {
  /// Its type.
  kind: Kind,
  /// Its name.
  id: ObjectId,
  /// Its content.
  content: &'s [u8],
  /// The deltas it lies under: none for an object stored whole, or held outside the pack.
  depth: u64,
}

/// A delta waiting for its base.
struct Waiting {
  /// The delta, inflated.
  delta: Vec<u8>,
  /// Where its base is, as far as is known.
  base: Base,
  /// For a reference delta placed by an index, the base's name, which the object found there must have.
  expected: Option<ObjectId>,
}

/// Where a waiting delta's base is.
#[derive(Clone, Copy)]
enum Base {
  /// The entry at this position, already read.
  Entry(usize),
  /// The entry at this offset, which an index gave and which had not yet been read.
  Offset(u64),
  /// The object of this name, wherever the pack holds it.
  Id(ObjectId),
}

/// A waiting delta's base, as the table of waiting deltas sorts it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
  /// The entry at this position.
  Entry(usize),
  /// The object of this name.
  Id(ObjectId),
}

/// A pack being read: the bytes so far, the objects resolved, and the deltas waiting.
pub struct Reader<'index, L: Locate + ?Sized = NoIndex> {
  /// The hash the pack's objects and trailer are in.
  format: ObjectFormat,
  /// Where reference deltas' bases are, by name.
  index: &'index L,
  /// The checksum of every byte read before the trailer.
  checksum: Hasher,
  /// The inflater, reset for each entry.
  inflater: Decompress,
  /// The bytes of the pack read so far.
  at: u64,
  /// What comes next.
  stage: Stage,
  /// The first bytes of a field whose bytes have not all arrived: the pack's header, an entry's header, the trailer.
  partial: Vec<u8>,
  /// The objects the header counts that have not yet begun.
  remaining: u32,
  /// Every entry read, in pack order.
  entries: Vec<Record>,
}

impl<'index, L: Locate + ?Sized> Reader<'index, L> {
  /// A reader of a pack whose objects are named in `format`, finding reference deltas' bases through `index`.
  pub fn new(format: ObjectFormat, index: &'index L) -> Self {
    Self {
      format,
      index,
      checksum: Hasher::raw(format),
      inflater: Decompress::new(true),
      at: 0,
      stage: Stage::PackHeader,
      partial: Vec::new(),
      remaining: 0,
      entries: Vec::new(),
    }
  }

  /// Reads `bytes`, the next bytes of the pack, as far as they go, charging what it builds to `admission`: how many it
  /// took. It takes them all until the trailer ends, and none after.
  pub fn read(&mut self, bytes: &[u8], admission: &mut Admission) -> Result<usize, Refusal> {
    let mut used = 0usize;
    loop {
      let rest = bytes.get(used..).unwrap_or_default();
      if rest.is_empty() || matches!(self.stage, Stage::Done(_)) {
        return Ok(used);
      }
      let Some(took) = self.step(rest, admission)? else {
        return Ok(used);
      };
      used = used.saturating_add(took);
    }
  }

  /// Ends the pack: refused unless its trailer has been read; then resolves the waiting deltas and answers every object
  /// in pack order. Every base must be one of the pack's own objects.
  pub fn finish(self, admission: &mut Admission) -> Result<Unpacked, Refusal> {
    self.finish_with(&NoObjects, admission)
  }

  /// Ends a pack that may be thin: as [`Reader::finish`], a reference delta whose base is none of the pack's objects
  /// applied to the object of that name `outside` holds, as `index-pack --fix-thin` resolves it against the repository.
  /// An object so made lies one delta deep; the base itself is not among the answer's objects.
  pub fn finish_with<O: Outside + ?Sized>(
    mut self,
    outside: &O,
    admission: &mut Admission,
  ) -> Result<Unpacked, Refusal> {
    let Stage::Done(checksum) = self.stage else {
      return Err(Refusal::Truncated { at: self.at });
    };
    self.resolve_waiting(outside, admission)?;
    let count = self.entries.len();
    charge_items::<Object>(admission, count)?;
    let mut objects = Vec::new();
    reserve(&mut objects, count)?;
    for record in self.entries {
      // `resolve_waiting` has refused every delta still waiting, so each record here is resolved.
      let State::Resolved(resolved) = record.state else {
        return Err(Refusal::Cycle { at: record.offset });
      };
      objects.push(Object {
        offset: record.offset,
        stored: record.stored,
        kind: resolved.kind,
        id: resolved.id,
        content: resolved.content,
        chain: resolved.chain,
      });
    }
    Ok(Unpacked { objects, checksum })
  }

  /// Reads from `rest` what the stage takes next: how many bytes it took, or `None` when it can go no further without
  /// more. A stream may end without taking a byte, its end already inflated; that is progress, so it is `Some(0)`.
  fn step(&mut self, rest: &[u8], admission: &mut Admission) -> Result<Option<usize>, Refusal> {
    match self.stage {
      Stage::PackHeader => self.pack_header(rest, admission).map(Some),
      Stage::EntryHeader => self.entry_header(rest, admission).map(Some),
      Stage::Data(_) => self.data(rest, admission),
      Stage::Trailer => self.trailer(rest).map(Some),
      Stage::Done(_) => Ok(None),
    }
  }

  /// Takes `bytes` as read: hashed into the checksum and counted.
  fn consume(&mut self, bytes: &[u8]) {
    self.checksum.update(bytes);
    self.at = self.at.saturating_add(to_u64(bytes.len()));
  }

  /// Adds up to `want` of `rest`'s first bytes to the partial field: how many.
  fn gather(&mut self, rest: &[u8], want: usize) -> Result<usize, Refusal> {
    let take = want.min(rest.len());
    let piece = rest.get(..take).unwrap_or_default();
    self
      .partial
      .try_reserve(take)
      .map_err(|_| Refusal::AllocationRefused {
        bytes: to_u64(take),
      })?;
    self.partial.extend_from_slice(piece);
    Ok(take)
  }

  /// The pack's header: once whole, the records of the objects it counts are charged and reserved.
  fn pack_header(&mut self, rest: &[u8], admission: &mut Admission) -> Result<usize, Refusal> {
    let header_len = usize::try_from(PACK_HEADER_LEN).unwrap_or(usize::MAX);
    let take = self.gather(rest, header_len.saturating_sub(self.partial.len()))?;
    let header = pack::read_header(&self.partial).map_err(Refusal::Pack)?;
    self.consume(rest.get(..take).unwrap_or_default());
    let Some(header) = header else {
      return Ok(take);
    };
    self.partial.clear();
    let count = usize::try_from(header.objects).unwrap_or(usize::MAX);
    charge_items::<Record>(admission, count)?;
    reserve(&mut self.entries, count)?;
    self.remaining = header.objects;
    self.stage = self.after_entry();
    Ok(take)
  }

  /// What follows an entry, or the pack's header: another entry, or the trailer.
  fn after_entry(&self) -> Stage {
    if self.remaining == 0 {
      Stage::Trailer
    } else {
      Stage::EntryHeader
    }
  }

  /// An entry's header: once whole (its size charged by [`entry::read`]), room for its data is reserved and inflation
  /// begins.
  fn entry_header(&mut self, rest: &[u8], admission: &mut Admission) -> Result<usize, Refusal> {
    let begun = self.partial.len();
    let longest = entry::longest_header(self.format);
    let take = self.gather(rest, longest.saturating_sub(begun))?;
    let began = self.at.saturating_sub(to_u64(begun));
    let read = entry::read(&self.partial, began, self.format, admission).map_err(Refusal::Entry)?;
    let Some((header, len)) = read else {
      self.consume(rest.get(..take).unwrap_or_default());
      return Ok(take);
    };
    let mine = len.saturating_sub(begun);
    self.consume(rest.get(..mine).unwrap_or_default());
    self.partial.clear();
    let size = usize::try_from(header.size)
      .map_err(|_| Refusal::AllocationRefused { bytes: header.size })?;
    let mut content = Vec::new();
    reserve(&mut content, size)?;
    content.resize(size, 0);
    self.inflater.reset(true);
    self.stage = Stage::Data(Data {
      header,
      began,
      content,
      filled: 0,
    });
    Ok(mine)
  }

  /// An entry's data, inflated from `rest` into the room its size made; once the stream ends at exactly that size, the
  /// entry is resolved. `None` when the inflater took nothing and the stream has not ended.
  fn data(&mut self, rest: &[u8], admission: &mut Admission) -> Result<Option<usize>, Refusal> {
    let Stage::Data(data) = &mut self.stage else {
      return Ok(None);
    };
    let inflated = inflate(&mut self.inflater, data, rest)?;
    let took = usize::try_from(inflated.took).unwrap_or(usize::MAX);
    let piece = rest.get(..took).unwrap_or_default();
    self.checksum.update(piece);
    self.at = self.at.saturating_add(inflated.took);
    if !inflated.ended {
      return Ok((took > 0).then_some(took));
    }
    let Stage::Data(data) = core::mem::replace(&mut self.stage, Stage::EntryHeader) else {
      return Ok(Some(took));
    };
    let stored = self.at.saturating_sub(data.began);
    let record = self.record(data, stored, admission)?;
    self.entries.push(record);
    self.remaining = self.remaining.saturating_sub(1);
    self.stage = self.after_entry();
    Ok(Some(took))
  }

  /// The record of an entry whose data has ended: its object named, its delta applied, or its delta waiting.
  fn record(&self, data: Data, stored: u64, admission: &mut Admission) -> Result<Record, Refusal> {
    let Data {
      header,
      began,
      content,
      ..
    } = data;
    let state = match header.representation {
      Representation::Whole(kind) => State::Resolved(Resolved {
        kind,
        id: name_object(self.format, kind, &content, began)?,
        content,
        chain: None,
      }),
      Representation::OfsDelta { base } => {
        let entry = self
          .entry_at(base)
          .ok_or(Refusal::BaseNotAnEntry { at: began, base })?;
        self.delta_state(content, Base::Entry(entry), None, began, admission)?
      }
      Representation::RefDelta { base } => {
        let placed = self.index.offset_of(&base);
        let expected = placed.map(|_| base);
        let base = match placed {
          Some(offset) if offset < began => {
            Base::Entry(self.entry_at(offset).ok_or(Refusal::BaseNotAnEntry {
              at: began,
              base: offset,
            })?)
          }
          Some(offset) => Base::Offset(offset),
          None => Base::Id(base),
        };
        self.delta_state(content, base, expected, began, admission)?
      }
    };
    Ok(Record {
      offset: began,
      stored,
      state,
    })
  }

  /// A delta's state: applied at once when its base is an entry already resolved, waiting otherwise.
  fn delta_state(
    &self,
    delta: Vec<u8>,
    base: Base,
    expected: Option<ObjectId>,
    at: u64,
    admission: &mut Admission,
  ) -> Result<State, Refusal> {
    if let Base::Entry(entry) = base
      && let Some(resolved) = self.applied(entry, &delta, expected, at, admission)?
    {
      return Ok(State::Resolved(resolved));
    }
    Ok(State::Waiting(Waiting {
      delta,
      base,
      expected,
    }))
  }

  /// `delta` applied to the entry at `base`, or `None` when that entry is itself a delta still waiting. The object
  /// found must have the name an index placed it by.
  fn applied(
    &self,
    base: usize,
    delta: &[u8],
    expected: Option<ObjectId>,
    at: u64,
    admission: &mut Admission,
  ) -> Result<Option<Resolved>, Refusal> {
    let Some(Record {
      state: State::Resolved(source),
      ..
    }) = self.entries.get(base)
    else {
      return Ok(None);
    };
    if let Some(name) = expected
      && name != source.id
    {
      return Err(Refusal::BaseMisplaced {
        at,
        base: name,
        found: source.id,
      });
    }
    let depth = source.chain.map_or(0, |chain| chain.depth);
    let source = Source {
      kind: source.kind,
      id: source.id,
      content: &source.content,
      depth,
    };
    self.apply_to_source(source, delta, at, admission).map(Some)
  }

  /// `delta`, the entry at `at`'s, applied to `source`: the object it makes, of the source's type, one delta deeper.
  fn apply_to_source(
    &self,
    source: Source<'_>,
    delta: &[u8],
    at: u64,
    admission: &mut Admission,
  ) -> Result<Resolved, Refusal> {
    let content = delta::apply(source.content, delta, admission)
      .map_err(|refusal| Refusal::Delta { at, refusal })?;
    Ok(Resolved {
      kind: source.kind,
      id: name_object(self.format, source.kind, &content, at)?,
      content,
      chain: Some(Chain {
        base: source.id,
        depth: source.depth.saturating_add(1),
      }),
    })
  }

  /// The position of the entry that begins at `offset`, if one does.
  fn entry_at(&self, offset: u64) -> Option<usize> {
    self
      .entries
      .binary_search_by_key(&offset, |record| record.offset)
      .ok()
  }

  /// The trailer: once whole, it must be the checksum of every byte before it.
  fn trailer(&mut self, rest: &[u8]) -> Result<usize, Refusal> {
    let take = self.gather(
      rest,
      self.format.raw_len().saturating_sub(self.partial.len()),
    )?;
    self.at = self.at.saturating_add(to_u64(take));
    if self.partial.len() < self.format.raw_len() {
      return Ok(take);
    }
    let checksum = core::mem::replace(&mut self.checksum, Hasher::raw(self.format))
      .finish()
      .map_err(|attack| Refusal::Pack(pack::Refusal::Collision(attack)))?;
    if checksum.as_bytes() != self.partial.as_slice() {
      return Err(Refusal::Pack(pack::Refusal::Checksum));
    }
    self.partial.clear();
    self.stage = Stage::Done(checksum);
    Ok(take)
  }

  /// Resolves the waiting deltas, each once its base is: their table sorted by base, then every entry's children found
  /// in it by the entry's position and by its name, newly resolved ones on a stack; then those whose base no object of
  /// the pack is, from `outside`. A delta still waiting after is refused.
  fn resolve_waiting<O: Outside + ?Sized>(
    &mut self,
    outside: &O,
    admission: &mut Admission,
  ) -> Result<(), Refusal> {
    let count = self
      .entries
      .iter()
      .filter(|record| matches!(record.state, State::Waiting(_)))
      .count();
    if count == 0 {
      return Ok(());
    }
    let waiting = self.waiting_table(count, admission)?;
    charge_items::<usize>(admission, count)?;
    let mut stack = Vec::new();
    reserve(&mut stack, count)?;
    for entry in 0..self.entries.len() {
      self.resolve_children(entry, &waiting, &mut stack, admission)?;
      while let Some(child) = stack.pop() {
        self.resolve_children(child, &waiting, &mut stack, admission)?;
      }
    }
    self.resolve_outside(&waiting, &mut stack, outside, admission)?;
    let first = self
      .entries
      .iter()
      .position(|record| matches!(record.state, State::Waiting(_)));
    match first {
      Some(first) => Err(self.unresolved(first)),
      None => Ok(()),
    }
  }

  /// The waiting deltas, `count` of them, each by its base's key, sorted: an index's offset becomes the entry there.
  fn waiting_table(
    &self,
    count: usize,
    admission: &mut Admission,
  ) -> Result<Vec<(Key, usize)>, Refusal> {
    charge_items::<(Key, usize)>(admission, count)?;
    let mut table = Vec::new();
    reserve(&mut table, count)?;
    for (position, record) in self.entries.iter().enumerate() {
      let State::Waiting(waiting) = &record.state else {
        continue;
      };
      let key = match waiting.base {
        Base::Entry(entry) => Key::Entry(entry),
        Base::Id(id) => Key::Id(id),
        Base::Offset(offset) => {
          Key::Entry(self.entry_at(offset).ok_or(Refusal::BaseNotAnEntry {
            at: record.offset,
            base: offset,
          })?)
        }
      };
      table.push((key, position));
    }
    table.sort_unstable();
    Ok(table)
  }

  /// Applies every waiting delta whose base is the entry at `parent`, once it is resolved, by its position and by its
  /// name; each one resolved goes on `stack`, so its own children follow.
  fn resolve_children(
    &mut self,
    parent: usize,
    waiting: &[(Key, usize)],
    stack: &mut Vec<usize>,
    admission: &mut Admission,
  ) -> Result<(), Refusal> {
    let Some(Record {
      state: State::Resolved(resolved),
      ..
    }) = self.entries.get(parent)
    else {
      return Ok(());
    };
    for key in [Key::Entry(parent), Key::Id(resolved.id)] {
      let start = waiting.partition_point(|(base, _)| *base < key);
      let children = waiting.get(start..).unwrap_or_default();
      for (_, child) in children.iter().take_while(|(base, _)| *base == key) {
        if self.resolve_child(parent, *child, admission)? {
          stack.push(*child);
        }
      }
    }
    Ok(())
  }

  /// Applies the deltas still waiting for a base named outside the pack: one table group at a time, the base taken from
  /// `outside` when it holds it, each delta resolved then its own children as the pack's are.
  fn resolve_outside<O: Outside + ?Sized>(
    &mut self,
    waiting: &[(Key, usize)],
    stack: &mut Vec<usize>,
    outside: &O,
    admission: &mut Admission,
  ) -> Result<(), Refusal> {
    for (key, child) in waiting {
      let Key::Id(id) = key else {
        continue;
      };
      let Some((kind, content)) = outside.object(id) else {
        continue;
      };
      let source = Source {
        kind,
        id: *id,
        content,
        depth: 0,
      };
      if self.resolve_child_on(source, *child, admission)? {
        stack.push(*child);
        while let Some(next) = stack.pop() {
          self.resolve_children(next, waiting, stack, admission)?;
        }
      }
    }
    Ok(())
  }

  /// Applies the delta at `child`, if still waiting, to `source`, an object outside the pack: whether it did.
  fn resolve_child_on(
    &mut self,
    source: Source<'_>,
    child: usize,
    admission: &mut Admission,
  ) -> Result<bool, Refusal> {
    let Some(Record {
      offset,
      state: State::Waiting(waiting),
      ..
    }) = self.entries.get(child)
    else {
      return Ok(false);
    };
    let resolved = self.apply_to_source(source, &waiting.delta, *offset, admission)?;
    if let Some(record) = self.entries.get_mut(child) {
      record.state = State::Resolved(resolved);
    }
    Ok(true)
  }

  /// Applies the delta at `child`, if still waiting, to the object at `parent`: whether it did.
  fn resolve_child(
    &mut self,
    parent: usize,
    child: usize,
    admission: &mut Admission,
  ) -> Result<bool, Refusal> {
    let Some(Record {
      offset,
      state: State::Waiting(waiting),
      ..
    }) = self.entries.get(child)
    else {
      return Ok(false);
    };
    let Some(resolved) =
      self.applied(parent, &waiting.delta, waiting.expected, *offset, admission)?
    else {
      return Ok(false);
    };
    if let Some(record) = self.entries.get_mut(child) {
      record.state = State::Resolved(resolved);
    }
    Ok(true)
  }

  /// Why the delta at `first`, the first still waiting in pack order, never resolved: its chain walked base by base, at
  /// most once round every entry, ends at a base found nowhere, or comes back on itself.
  fn unresolved(&self, first: usize) -> Refusal {
    let mut current = first;
    for _ in 0..=self.entries.len() {
      let Some(Record {
        offset,
        state: State::Waiting(waiting),
        ..
      }) = self.entries.get(current)
      else {
        break;
      };
      current = match waiting.base {
        Base::Entry(entry) => entry,
        Base::Id(base) => return Refusal::DeltaBaseMissing { at: *offset, base },
        Base::Offset(offset_given) => match self.entry_at(offset_given) {
          Some(entry) => entry,
          None => {
            return Refusal::BaseNotAnEntry {
              at: *offset,
              base: offset_given,
            };
          }
        },
      };
    }
    let at = self
      .entries
      .get(first)
      .map_or(self.at, |record| record.offset);
    Refusal::Cycle { at }
  }
}

/// What one turn of the inflater did with an entry's data.
struct Inflated {
  /// The input bytes it took.
  took: u64,
  /// Whether the stream ended, at exactly the entry's size.
  ended: bool,
}

/// Inflates `rest` into the room left in `data`. When the room is full, one more byte of room tells a stream that ends
/// there from one that would go on: any byte it takes is past the size. Refused for a stream past the size, one the
/// inflater refuses, and one that ends short of it.
fn inflate(inflater: &mut Decompress, data: &mut Data, rest: &[u8]) -> Result<Inflated, Refusal> {
  let at = data.began;
  let size = data.header.size;
  let before_in = inflater.total_in();
  let before_out = inflater.total_out();
  let full = data.filled >= data.content.len();
  let mut spare = [0u8; 1];
  let room: &mut [u8] = if full {
    &mut spare
  } else {
    data.content.get_mut(data.filled..).unwrap_or_default()
  };
  let result = inflater.decompress(rest, room, FlushDecompress::None);
  // A byte past the size is judged before the stream's own error: one call can write it and then fail the Adler-32
  // that follows, and the data running past its header is the refusal that names what is wrong.
  let wrote = inflater.total_out().saturating_sub(before_out);
  if full && wrote > 0 {
    return Err(Refusal::LongerThanSize { at, size });
  }
  let status = result.map_err(|_| Refusal::Inflate { at })?;
  data.filled = data
    .filled
    .saturating_add(usize::try_from(wrote).unwrap_or(usize::MAX));
  let took = inflater.total_in().saturating_sub(before_in);
  let ended = status == Status::StreamEnd;
  if ended && data.filled < data.content.len() {
    return Err(Refusal::ShorterThanSize {
      at,
      size,
      inflated: to_u64(data.filled),
    });
  }
  Ok(Inflated { took, ended })
}

/// The name of an object of `kind` whose content is `content`, in `format`: refused when the content carries a
/// collision attack.
fn name_object(
  format: ObjectFormat,
  kind: Kind,
  content: &[u8],
  at: u64,
) -> Result<ObjectId, Refusal> {
  name_of(format, kind, content).map_err(|attack| Refusal::Collision { at, attack })
}

/// Charges `count` values of `T` to `admission`.
fn charge_items<T>(admission: &mut Admission, count: usize) -> Result<(), Refusal> {
  admission
    .charge_items::<T>(count)
    .map_err(Refusal::PastAdmission)
}

/// Reserves room for exactly `count` more values in `vec`, or refuses by kind.
fn reserve<T>(vec: &mut Vec<T>, count: usize) -> Result<(), Refusal> {
  vec
    .try_reserve_exact(count)
    .map_err(|_| Refusal::AllocationRefused {
      bytes: to_u64(core::mem::size_of::<T>()).saturating_mul(to_u64(count)),
    })
}

/// Reads a pack held whole, of `format`, finding reference deltas' bases through `index`: refused, beyond what
/// [`Reader`] refuses, for bytes after its trailer.
pub fn read_whole<L: Locate + ?Sized>(
  bytes: &[u8],
  format: ObjectFormat,
  index: &L,
  admission: &mut Admission,
) -> Result<Unpacked, Refusal> {
  let mut reader = Reader::new(format, index);
  let used = reader.read(bytes, admission)?;
  if used < bytes.len() {
    return Err(Refusal::TrailingBytes { at: to_u64(used) });
  }
  reader.finish(admission)
}

/// Holds an index to the objects read from its pack, as `git verify-pack` does once it has inflated them: the same
/// trailer, the same count, and every object's name at its own entry's offset. The index's names are distinct (A-591
/// refuses a duplicate), so with the counts equal each object has exactly its own place.
pub fn check_index(unpacked: &Unpacked, idx: &Idx<'_>) -> Result<(), IndexMismatch> {
  if idx.pack_checksum() != unpacked.checksum {
    return Err(IndexMismatch::Checksum {
      index: idx.pack_checksum(),
      pack: unpacked.checksum,
    });
  }
  if idx.len() != unpacked.objects.len() {
    return Err(IndexMismatch::Count {
      index: to_u64(idx.len()),
      pack: to_u64(unpacked.objects.len()),
    });
  }
  for object in &unpacked.objects {
    let placed = idx.offset_of(&object.id);
    if placed != Some(object.offset) {
      return Err(IndexMismatch::Offset {
        id: object.id,
        pack: object.offset,
        index: placed,
      });
    }
  }
  Ok(())
}

#[cfg(test)]
#[path = "tests/unpack.rs"]
mod tests;
