//! A pack fragment: objects encoded as a pack's entries (A-614 part 2; §4.17.2 "The repository in
//! git's terms", item 4; `gitformat-pack`). Each entry is written as `pack-objects` writes one,
//! through A-582's writers: its header (the type and size, then a delta's base) and its data
//! deflated by zlib through flate2 (A-607's engine), with the CRC32 of the whole entry kept, as a
//! pack index keeps it, so entries can be copied from pack to pack unchanged.
//!
//! **Deltas.** An object may be stored as a delta against an earlier entry of the fragment, named
//! by its offset back (`OBJ_OFS_DELTA`), or against the object its path held in the previous
//! version, named by its id (`OBJ_REF_DELTA`), which a fetch of the versions after it finds
//! earlier in its pack or, thin, held by the client. The previous version's object is tried first,
//! then the earlier entries of the same kind, nearest first, as many as the learned window says
//! ([`crate::store::learn`]); the smallest delta shorter than the object wins. A chain is at most
//! the learned depth: a REF delta's base lies outside the fragment, so it begins a chain, as the
//! client that completes a thin pack stores the base whole. Every trial is timed by the owner's
//! clock and taught to the window.
//!
//! **Offsets.** A fragment is written as if it began right after a pack's header
//! ([`entry::PACK_HEADER_LEN`]); an offset delta keeps only the distance back to its base, so the
//! fragment's bytes are valid wherever a pack places them after its header.
//!
//! **One object at a time (A-639).** The encoder ([`Encoder`]) takes one object at a time at any
//! place in a pack and keeps, of the entries before it, only those its learned window can try: for
//! each kind, the nearest ones whose chains have room, as many as the window holds now. The window
//! grows only by an object's own trials, which are recorded before the next object chooses, so the
//! entries let go are never ones the next object would try, and a fragment encoded so is the one
//! encoded with every earlier entry at hand. upload-pack's stream (`upload`) encodes the objects no
//! fragment covers with it, a turn's object at a time, naming an earlier entry by its id where the
//! client did not say `ofs-delta`. A version's fragments and a base pack are encoded with it an
//! object a unit too, under the owner's quantum (`encoding`, A-653 continued (2)).

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use flate2::{Compress, Compression, FlushCompress, Status};

use crate::entry::{self, Header, PACK_HEADER_LEN, Representation};
use crate::object_name::{Kind, ObjectId};

use super::deltas::delta_of;
use super::learn::{Clock, Exchange, LEVELS, Levels, Window};

/// An object to encode: its id, position, type and bytes, and the object its path held in the
/// previous version, a base a REF delta may name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ToEncode {
  pub(crate) id: ObjectId,
  pub(crate) position: u64,
  pub(crate) kind: Kind,
  pub(crate) bytes: Vec<u8>,
  pub(crate) named: Option<Named>,
}

/// The object a path held in the previous version: its id, its position and its bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Named {
  pub(crate) id: ObjectId,
  pub(crate) position: u64,
  pub(crate) bytes: Vec<u8>,
}

/// How an entry holds its object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stored {
  /// Whole.
  Whole,
  /// As a delta against an earlier entry of the fragment.
  Offset,
  /// As a delta against an object named by its id.
  Reference,
}

/// One entry of a fragment: its object, where the entry begins in the fragment, the CRC32 of the
/// whole entry, its delta chain's depth (zero whole), how it holds its object, and its base.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placed {
  /// The object.
  pub id: ObjectId,
  /// The object's position in its repository.
  pub position: u64,
  /// The object's type.
  pub kind: Kind,
  /// Where its entry begins in the fragment.
  pub offset: u64,
  /// The CRC32 of its entry, header and data.
  pub crc: u32,
  /// Its delta chain's depth.
  pub depth: u64,
  /// How it holds its object.
  pub stored: Stored,
  /// Its delta's base: for an offset delta, where the base's entry begins, counted as `offset`
  /// is; for a reference delta, the base object's position; `None` for an object stored whole.
  pub base: Option<u64>,
}

/// Why a fragment could not be written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FragmentRefused {
  /// zlib refused to deflate.
  Deflate,
  /// An entry's header could not be written (a size past what git reads).
  Entry(entry::Refusal),
}

/// The encoder's learned rules, with the owner's clock and exchange.
pub(crate) struct Learning<'a> {
  pub(crate) window: &'a mut Window,
  pub(crate) levels: &'a mut Levels,
  /// The deepest chain to make; zero makes no delta.
  pub(crate) depth: u64,
  pub(crate) exchange: Exchange,
  pub(crate) clock: &'a mut dyn Clock,
}

/// What an entry's base is.
enum Base {
  Whole,
  /// An earlier entry: where it begins in the pack, and its object's id and position.
  Earlier {
    at: u64,
    id: ObjectId,
    position: u64,
  },
  /// The object the path held before, by its id and position.
  Named {
    id: ObjectId,
    position: u64,
  },
}

/// The representation chosen for one object: its base, the delta when it has one, its depth.
struct Choice {
  base: Base,
  delta: Option<Vec<u8>>,
  depth: u64,
}

/// An earlier entry a delta may name: its object, where its entry begins, its chain's depth.
struct Recent {
  id: ObjectId,
  position: u64,
  bytes: Vec<u8>,
  at: u64,
  depth: u64,
}

/// The incremental encoder (module docs): the earlier entries a delta may name, kept per kind, and
/// the kinds a level has been sampled on.
pub(crate) struct Encoder {
  /// Whether a delta names an earlier entry by its offset back, as `ofs-delta` allows, or by id.
  by_offset: bool,
  /// For each kind, the entries a delta may name, the nearest last.
  recent: BTreeMap<Kind, VecDeque<Recent>>,
  sampled: BTreeSet<Kind>,
}

impl Encoder {
  /// An encoder with nothing before it; `by_offset` names earlier entries by offset.
  pub(crate) fn new(by_offset: bool) -> Encoder {
    Encoder {
      by_offset,
      recent: BTreeMap::new(),
      sampled: BTreeSet::new(),
    }
  }

  /// Encodes `object` as the entry beginning `at` bytes into its pack: the entry's bytes, and its
  /// record, whose offset and offset base are the pack's.
  pub(crate) fn put(
    &mut self,
    object: ToEncode,
    at: u64,
    learning: &mut Learning<'_>,
  ) -> Result<(Vec<u8>, Placed), FragmentRefused> {
    let choice = self.choose(&object, learning);
    let sample = self.sampled.insert(object.kind);
    let (representation, stored, base) = match choice.base {
      Base::Whole => (Representation::Whole(object.kind), Stored::Whole, None),
      Base::Earlier { at: base, .. } if self.by_offset => (
        Representation::OfsDelta { base },
        Stored::Offset,
        Some(base),
      ),
      Base::Earlier { id, position, .. } | Base::Named { id, position } => (
        Representation::RefDelta { base: id },
        Stored::Reference,
        Some(position),
      ),
    };
    let data = choice.delta.as_deref().unwrap_or(&object.bytes);
    let header = Header {
      representation,
      size: to_u64(data.len()),
    };
    let mut entry = Vec::new();
    entry::write(header, at, &mut entry).map_err(FragmentRefused::Entry)?;
    entry.extend_from_slice(&compress(object.kind, data, sample, learning)?);
    let placed = Placed {
      id: object.id,
      position: object.position,
      kind: object.kind,
      offset: at,
      crc: crc32fast::hash(&entry),
      depth: choice.depth,
      stored,
      base,
    };
    self.remember(object, (at, choice.depth), learning);
    Ok((entry, placed))
  }

  /// The representation of `object` (module docs): the named base first, then the window's
  /// earlier entries of its kind whose chains have room.
  fn choose(&self, object: &ToEncode, learning: &mut Learning<'_>) -> Choice {
    let mut choice = Choice {
      base: Base::Whole,
      delta: None,
      depth: 0,
    };
    if learning.depth == 0 {
      return choice;
    }
    let mut best = object.bytes.len();
    let mut rank = 0_usize;
    if let Some(named) = &object.named {
      rank = rank.saturating_add(1);
      if let Some(delta) = trial(&named.bytes, &object.bytes, rank, &mut best, learning) {
        choice = Choice {
          base: Base::Named {
            id: named.id,
            position: named.position,
          },
          delta: Some(delta),
          depth: 1,
        };
      }
    }
    let Some(recent) = self.recent.get(&object.kind) else {
      return choice;
    };
    let window = learning.window.size(learning.exchange, recent.len());
    for candidate in recent.iter().rev().take(window) {
      rank = rank.saturating_add(1);
      if let Some(delta) = trial(&candidate.bytes, &object.bytes, rank, &mut best, learning) {
        choice = Choice {
          base: Base::Earlier {
            at: candidate.at,
            id: candidate.id,
            position: candidate.position,
          },
          delta: Some(delta),
          depth: candidate.depth.saturating_add(1),
        };
      }
    }
    choice
  }

  /// Keeps `object`, written at `at` with a chain `depth` deep, as a candidate while its chain has
  /// room, and lets go of the candidates of its kind past what the window tries now.
  fn remember(&mut self, object: ToEncode, (at, depth): (u64, u64), learning: &Learning<'_>) {
    if depth >= learning.depth {
      return;
    }
    let keep = learning.window.size(learning.exchange, usize::MAX);
    let recent = self.recent.entry(object.kind).or_default();
    recent.push_back(Recent {
      id: object.id,
      position: object.position,
      bytes: object.bytes,
      at,
      depth,
    });
    while recent.len() > keep {
      recent.pop_front();
    }
  }
}

/// A fragment's bytes and entries.
pub(crate) type Encoded = (Vec<u8>, Vec<Placed>);

/// A record's offsets moved from its pack's count to its fragment's: its own, and an offset
/// delta's base.
pub(crate) fn in_fragment(mut record: Placed) -> Placed {
  record.offset = record.offset.saturating_sub(PACK_HEADER_LEN);
  if record.stored == Stored::Offset {
    record.base = record.base.map(|base| base.saturating_sub(PACK_HEADER_LEN));
  }
  record
}

/// One timed trial of a delta against `base`, taught to the window at `rank`; the delta when it
/// is shorter than `best`, which it then becomes.
fn trial(
  base: &[u8],
  target: &[u8],
  rank: usize,
  best: &mut usize,
  learning: &mut Learning<'_>,
) -> Option<Vec<u8>> {
  let start = learning.clock.now_ns();
  let delta = delta_of(base, target);
  let ns = learning.clock.now_ns().saturating_sub(start);
  let len = delta.as_ref().map_or(usize::MAX, Vec::len);
  let saved = u64::try_from(best.saturating_sub(len)).unwrap_or(u64::MAX);
  learning.window.record(rank, saved, ns);
  if len < *best {
    *best = len;
    return delta;
  }
  None
}

/// `data` deflated at its kind's learned level; a sample deflates it at every level, each timed
/// and taught, and keeps the output of the level the samples then choose.
fn compress(
  kind: Kind,
  data: &[u8],
  sample: bool,
  learning: &mut Learning<'_>,
) -> Result<Vec<u8>, FragmentRefused> {
  if !sample && let Some(level) = learning.levels.level(kind, learning.exchange) {
    return deflate(data, level);
  }
  let outputs = sampled(kind, data, learning)?;
  let chosen = learning.levels.level(kind, learning.exchange).unwrap_or(0);
  let chosen = usize::try_from(chosen).unwrap_or(0);
  outputs
    .into_iter()
    .nth(chosen)
    .ok_or(FragmentRefused::Deflate)
}

/// `data` deflated at every level, each timed and taught to `kind`'s levels; the outputs, by level.
fn sampled(
  kind: Kind,
  data: &[u8],
  learning: &mut Learning<'_>,
) -> Result<Vec<Vec<u8>>, FragmentRefused> {
  let mut outputs = Vec::new();
  for level in 0..LEVELS {
    let start = learning.clock.now_ns();
    let output = deflate(data, level)?;
    let ns = learning.clock.now_ns().saturating_sub(start);
    let sizes = (to_u64(data.len()), to_u64(output.len()), ns);
    learning.levels.record(kind, level, sizes);
    outputs.push(output);
  }
  Ok(outputs)
}

/// The level to deflate an object of `kind` at whose first bytes are `sample` (A-639: a stream
/// deflating an object a window at a time): the learned one; with none learned yet, the one its
/// levels choose once `sample` is deflated at every level, each timed and taught.
pub(crate) fn level(
  kind: Kind,
  sample: &[u8],
  learning: &mut Learning<'_>,
) -> Result<u32, FragmentRefused> {
  if let Some(level) = learning.levels.level(kind, learning.exchange) {
    return Ok(level);
  }
  sampled(kind, sample, learning)?;
  Ok(learning.levels.level(kind, learning.exchange).unwrap_or(0))
}

fn to_u64(len: usize) -> u64 {
  u64::try_from(len).unwrap_or(u64::MAX)
}

/// `data` deflated as a zlib stream at `level`, through flate2.
pub(crate) fn deflate(data: &[u8], level: u32) -> Result<Vec<u8>, FragmentRefused> {
  let mut compress = Compress::new(Compression::new(level), true);
  deflate_more(&mut compress, data, true)
}

/// `data` fed to `compress` whole, and what it gave out: all of it once `last`, the stream ended;
/// otherwise what it gave so far, the rest kept for the input after.
pub(crate) fn deflate_more(
  compress: &mut Compress,
  data: &[u8],
  last: bool,
) -> Result<Vec<u8>, FragmentRefused> {
  let flush = if last {
    FlushCompress::Finish
  } else {
    FlushCompress::None
  };
  let before = compress.total_in();
  let mut out: Vec<u8> = Vec::with_capacity(data.len().saturating_add(1));
  loop {
    let consumed = compress.total_in().saturating_sub(before);
    let consumed = usize::try_from(consumed).map_err(|_| FragmentRefused::Deflate)?;
    let rest = data.get(consumed..).unwrap_or_default();
    let status = compress
      .compress_vec(rest, &mut out, flush)
      .map_err(|_| FragmentRefused::Deflate)?;
    let taken = compress.total_in().saturating_sub(before);
    if status == Status::StreamEnd || (!last && taken == to_u64(data.len())) {
      return Ok(out);
    }
    out.reserve(out.capacity().max(1));
  }
}
