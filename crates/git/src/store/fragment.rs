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

use std::collections::BTreeSet;

use flate2::{Compress, Compression, FlushCompress, Status};

use crate::entry::{self, Header, PACK_HEADER_LEN, Representation};
use crate::object_name::{Kind, ObjectId};

use super::deltas::delta_of;
use super::learn::{Clock, Exchange, LEVELS, Levels, Window};

/// An object to encode: its id, type and bytes, and the object its path held in the previous
/// version with that object's bytes, a base a REF delta may name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ToEncode {
  pub(crate) id: ObjectId,
  pub(crate) kind: Kind,
  pub(crate) bytes: Vec<u8>,
  pub(crate) named: Option<(ObjectId, Vec<u8>)>,
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
/// whole entry, its delta chain's depth (zero whole), and how it holds its object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placed {
  /// The object.
  pub id: ObjectId,
  /// Where its entry begins in the fragment.
  pub offset: u64,
  /// The CRC32 of its entry, header and data.
  pub crc: u32,
  /// Its delta chain's depth.
  pub depth: u64,
  /// How it holds its object.
  pub stored: Stored,
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
  /// An earlier entry, by where it begins in the fragment.
  Offset(u64),
  Reference(ObjectId),
}

/// The representation chosen for one object: its base, the delta when it has one, its depth.
struct Choice {
  base: Base,
  delta: Option<Vec<u8>>,
  depth: u64,
}

/// A fragment's bytes and entries.
pub(crate) type Encoded = (Vec<u8>, Vec<Placed>);

/// Encodes `objects`, in the caller's order, as one fragment (module docs).
pub(crate) fn encode(
  objects: &[ToEncode],
  learning: &mut Learning<'_>,
) -> Result<Encoded, FragmentRefused> {
  let mut bytes = Vec::new();
  let mut placed: Vec<Placed> = Vec::with_capacity(objects.len());
  let mut sampled: BTreeSet<Kind> = BTreeSet::new();
  for (at, object) in objects.iter().enumerate() {
    let choice = choose(objects, &placed, at, object, learning);
    let sample = sampled.insert(object.kind);
    write_entry(&mut bytes, &mut placed, object, choice, (sample, learning))?;
  }
  Ok((bytes, placed))
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

/// The representation of `objects[at]` (module docs): the named base first, then the window's
/// earlier entries of its kind whose chains have room.
fn choose(
  objects: &[ToEncode],
  placed: &[Placed],
  at: usize,
  object: &ToEncode,
  learning: &mut Learning<'_>,
) -> Choice {
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
  if let Some((named, named_bytes)) = &object.named {
    rank = rank.saturating_add(1);
    if let Some(delta) = trial(named_bytes, &object.bytes, rank, &mut best, learning) {
      choice = Choice {
        base: Base::Reference(*named),
        delta: Some(delta),
        depth: 1,
      };
    }
  }
  let candidates = earlier(objects, placed, at, object.kind, learning.depth);
  let window = learning.window.size(learning.exchange, candidates.len());
  for (bytes, offset, depth) in candidates.into_iter().take(window) {
    rank = rank.saturating_add(1);
    if let Some(delta) = trial(bytes, &object.bytes, rank, &mut best, learning) {
      choice = Choice {
        base: Base::Offset(offset),
        delta: Some(delta),
        depth: depth.saturating_add(1),
      };
    }
  }
  choice
}

/// One earlier entry a delta may name: its object's bytes, where it begins, its chain's depth.
type Candidate<'a> = (&'a [u8], u64, u64);

/// The entries before `at` of `kind` whose chains are shorter than `limit`, nearest first.
fn earlier<'a>(
  objects: &'a [ToEncode],
  placed: &[Placed],
  at: usize,
  kind: Kind,
  limit: u64,
) -> Vec<Candidate<'a>> {
  objects
    .iter()
    .zip(placed)
    .take(at)
    .rev()
    .filter(|(object, entry)| object.kind == kind && entry.depth < limit)
    .map(|(object, entry)| (object.bytes.as_slice(), entry.offset, entry.depth))
    .collect()
}

/// Writes one entry (module docs): its header at its offset, its data deflated, its CRC32 kept.
fn write_entry(
  bytes: &mut Vec<u8>,
  placed: &mut Vec<Placed>,
  object: &ToEncode,
  choice: Choice,
  (sample, learning): (bool, &mut Learning<'_>),
) -> Result<(), FragmentRefused> {
  let offset = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
  let (representation, stored) = match choice.base {
    Base::Whole => (Representation::Whole(object.kind), Stored::Whole),
    Base::Offset(base) => {
      let base = PACK_HEADER_LEN.saturating_add(base);
      (Representation::OfsDelta { base }, Stored::Offset)
    }
    Base::Reference(base) => (Representation::RefDelta { base }, Stored::Reference),
  };
  let data = choice.delta.as_deref().unwrap_or(&object.bytes);
  let header = Header {
    representation,
    size: u64::try_from(data.len()).unwrap_or(u64::MAX),
  };
  let start = bytes.len();
  let at = PACK_HEADER_LEN.saturating_add(offset);
  entry::write(header, at, bytes).map_err(FragmentRefused::Entry)?;
  bytes.extend_from_slice(&compress(object.kind, data, sample, learning)?);
  placed.push(Placed {
    id: object.id,
    offset,
    crc: crc32fast::hash(bytes.get(start..).unwrap_or_default()),
    depth: choice.depth,
    stored,
  });
  Ok(())
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
  let mut outputs = Vec::new();
  for level in 0..LEVELS {
    let start = learning.clock.now_ns();
    let output = deflate(data, level)?;
    let ns = learning.clock.now_ns().saturating_sub(start);
    let sizes = (to_u64(data.len()), to_u64(output.len()), ns);
    learning.levels.record(kind, level, sizes);
    outputs.push(output);
  }
  let chosen = learning.levels.level(kind, learning.exchange).unwrap_or(0);
  let chosen = usize::try_from(chosen).unwrap_or(0);
  outputs
    .into_iter()
    .nth(chosen)
    .ok_or(FragmentRefused::Deflate)
}

fn to_u64(len: usize) -> u64 {
  u64::try_from(len).unwrap_or(u64::MAX)
}

/// `data` deflated as a zlib stream at `level`, through flate2.
pub(crate) fn deflate(data: &[u8], level: u32) -> Result<Vec<u8>, FragmentRefused> {
  let mut compress = Compress::new(Compression::new(level), true);
  let mut out: Vec<u8> = Vec::with_capacity(data.len().saturating_add(1));
  loop {
    let consumed = usize::try_from(compress.total_in()).map_err(|_| FragmentRefused::Deflate)?;
    let rest = data.get(consumed..).unwrap_or_default();
    let status = compress
      .compress_vec(rest, &mut out, FlushCompress::Finish)
      .map_err(|_| FragmentRefused::Deflate)?;
    if status == Status::StreamEnd {
      return Ok(out);
    }
    out.reserve(out.capacity().max(1));
  }
}
