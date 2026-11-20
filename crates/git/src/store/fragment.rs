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

use std::collections::{BTreeMap, VecDeque};

use flate2::{Compress, Compression, FlushCompress, Status};

use crate::entry::{self, Header, PACK_HEADER_LEN, Representation};
use crate::object_name::{Kind, ObjectId};

use super::deltas::{Search, SearchStep};
use super::learn::{Exchange, LEVELS, Levels, Window};

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

/// The encoder's learned rules, with the owner's exchange (its measured rates among it).
pub(crate) struct Learning<'a> {
  pub(crate) window: &'a mut Window,
  pub(crate) levels: &'a mut Levels,
  /// The deepest chain to make; zero makes no delta.
  pub(crate) depth: u64,
  pub(crate) exchange: Exchange,
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

/// A candidate base an object's trials try: the object its path held before, or an earlier entry
/// of its kind, by its index among the encoder's recent entries of that kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Candidate {
  Named,
  Recent(usize),
}

/// Where an object's encoding stands (A-718).
enum PutStage {
  /// Trying its candidate bases, a step of a search at a time.
  Trials,
  /// Sampling the levels on the data's first window, `level` next.
  Sample { level: u32 },
  /// Deflating the data a window a step: the stream, the entry's CRC so far, the next byte to
  /// feed, and the entry's header, written with the first step.
  Deflate {
    compress: Compress,
    crc: crc32fast::Hasher,
    next: usize,
    header: Vec<u8>,
  },
}

/// An object being encoded a step at a time ([`Encoder::begin`], [`Encoder::step`]).
pub(crate) struct Putting {
  object: ToEncode,
  /// Where its entry begins in its pack.
  at: u64,
  /// Whether an earlier entry is named by its offset back.
  by_offset: bool,
  /// The candidate bases, in the order they are tried.
  tries: Vec<Candidate>,
  /// The candidate being tried, by its index in `tries`.
  trying: usize,
  /// Its search.
  search: Search,
  /// The owner's nanoseconds its trial has taken so far.
  trial_ns: u64,
  /// The shortest representation so far: the object's length, or the best delta's.
  best: usize,
  /// The representation chosen so far.
  choice: Choice,
  stage: PutStage,
}

impl Putting {
  /// The bytes its entry holds: the delta chosen, or the object.
  fn data(&self) -> &[u8] {
    self.choice.delta.as_deref().unwrap_or(&self.object.bytes)
  }

  /// How its entry holds its object, as chosen.
  fn representation(&self) -> (Representation, Stored, Option<u64>) {
    match self.choice.base {
      Base::Whole => (Representation::Whole(self.object.kind), Stored::Whole, None),
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
    }
  }

  /// The deflate stage at `level`: the entry's header written, its CRC begun.
  fn deflating(&self, level: u32) -> Result<PutStage, FragmentRefused> {
    let (representation, _, _) = self.representation();
    let header = Header {
      representation,
      size: to_u64(self.data().len()),
    };
    let mut written = Vec::new();
    entry::write(header, self.at, &mut written).map_err(FragmentRefused::Entry)?;
    Ok(PutStage::Deflate {
      compress: Compress::new(Compression::new(level), true),
      crc: crc32fast::Hasher::new(),
      next: 0,
      header: written,
    })
  }
}

/// What a step of an object's encoding gave.
pub(crate) enum Put {
  /// The entry's next bytes, and more to come.
  More(Vec<u8>),
  /// The entry's last bytes, and its record.
  Done(Vec<u8>, Placed),
}

/// An earlier entry a delta may name: its object, where its entry begins, its chain's depth.
struct Recent {
  id: ObjectId,
  position: u64,
  bytes: Vec<u8>,
  at: u64,
  depth: u64,
}

/// The incremental encoder (module docs): the earlier entries a delta may name, kept per kind.
pub(crate) struct Encoder {
  /// Whether a delta names an earlier entry by its offset back, as `ofs-delta` allows, or by id.
  by_offset: bool,
  /// For each kind, the entries a delta may name, the nearest last.
  recent: BTreeMap<Kind, VecDeque<Recent>>,
}

impl Encoder {
  /// An encoder with nothing before it; `by_offset` names earlier entries by offset.
  pub(crate) fn new(by_offset: bool) -> Encoder {
    Encoder {
      by_offset,
      recent: BTreeMap::new(),
    }
  }

  /// Encodes `object` as the entry beginning `at` bytes into its pack: the entry's bytes, and its
  /// record, whose offset and offset base are the pack's. [`Encoder::begin`]'s steps, driven to
  /// their end in one call, `window` bytes a step.
  pub(crate) fn put(
    &mut self,
    object: ToEncode,
    at: u64,
    learning: &mut Learning<'_>,
  ) -> Result<(Vec<u8>, Placed), FragmentRefused> {
    let window = object.bytes.len().max(1);
    let mut putting = self.begin(object, at, learning);
    let mut entry = Vec::new();
    loop {
      match self.step(&mut putting, learning, window)? {
        Put::More(bytes) => entry.extend_from_slice(&bytes),
        Put::Done(bytes, placed) => {
          entry.extend_from_slice(&bytes);
          return Ok((entry, placed));
        }
      }
    }
  }

  /// Begins encoding `object` as the entry beginning `at` bytes into its pack (A-718: an object a
  /// window's work a step, [`Encoder::step`]): its candidate bases chosen now, as [`Encoder::put`]
  /// tries them, the named base first, then the window's earlier entries of its kind, the nearest
  /// first, whose chains have room.
  pub(crate) fn begin(&mut self, object: ToEncode, at: u64, learning: &Learning<'_>) -> Putting {
    let mut tries = Vec::new();
    if learning.depth > 0 {
      if object.named.is_some() {
        tries.push(Candidate::Named);
      }
      let recent = self.recent.get(&object.kind).map_or(0, VecDeque::len);
      let window = learning.window.size(learning.exchange, recent);
      tries.extend((0..recent).rev().take(window).map(Candidate::Recent));
    }
    let best = object.bytes.len();
    Putting {
      object,
      at,
      by_offset: self.by_offset,
      tries,
      trying: 0,
      search: Search::new(),
      trial_ns: 0,
      best,
      choice: Choice {
        base: Base::Whole,
        delta: None,
        depth: 0,
      },
      stage: PutStage::Trials,
    }
  }

  /// One step of `putting`, at most `window` bytes of work (A-718): a step of one candidate's delta
  /// search, one level sampled on the data's first window, or a window of the data deflated; the
  /// entry's bytes made so far, and once done its record. The object is then kept as a candidate.
  pub(crate) fn step(
    &mut self,
    putting: &mut Putting,
    learning: &mut Learning<'_>,
    window: usize,
  ) -> Result<Put, FragmentRefused> {
    let window = window.max(1);
    match putting.stage {
      PutStage::Trials => {
        self.trial_step(putting, learning, window)?;
        Ok(Put::More(Vec::new()))
      }
      PutStage::Sample { level } => {
        let data = putting.data();
        let sample = data.get(..window.min(data.len())).unwrap_or_default();
        sample_level(putting.object.kind, sample, level, learning)?;
        let next = level.saturating_add(1);
        putting.stage = if next < LEVELS {
          PutStage::Sample { level: next }
        } else {
          let chosen = learning
            .levels
            .level(putting.object.kind, learning.exchange)
            .unwrap_or(0);
          putting.deflating(chosen)?
        };
        Ok(Put::More(Vec::new()))
      }
      PutStage::Deflate { .. } => self.deflate_step(putting, learning, window),
    }
  }

  /// A step of the trials: a step of the current candidate's search, or, with none left, the level
  /// chosen: the one its kind's levels learned, or, where none is yet (the first object of its kind
  /// the repository encodes), sampled first. A kind is sampled once (main's ruling, 2026-11-20: an
  /// object compressed one way, and one input one output), its samples' times the measured rates.
  fn trial_step(
    &self,
    putting: &mut Putting,
    learning: &mut Learning<'_>,
    window: usize,
  ) -> Result<(), FragmentRefused> {
    let Some(candidate) = putting.tries.get(putting.trying).copied() else {
      let learned = learning
        .levels
        .level(putting.object.kind, learning.exchange);
      putting.stage = match learned {
        Some(level) => putting.deflating(level)?,
        None => PutStage::Sample { level: 0 },
      };
      return Ok(());
    };
    let base: &[u8] = match candidate {
      Candidate::Named => putting
        .object
        .named
        .as_ref()
        .map_or(&[][..], |named| &named.bytes),
      Candidate::Recent(index) => self
        .recent
        .get(&putting.object.kind)
        .and_then(|recent| recent.get(index))
        .map_or(&[][..], |recent| &recent.bytes),
    };
    let step = putting.search.step(base, &putting.object.bytes, window);
    let SearchStep::Done(delta) = step else {
      return Ok(());
    };
    // The trial's time is the machine's measured rate over the bytes it searched (`Rates`), never the clock's.
    putting.trial_ns = learning
      .exchange
      .rates
      .search_ns(to_u64(base.len()), to_u64(putting.object.bytes.len()));
    let rank = putting.trying.saturating_add(1);
    let len = delta.as_ref().map_or(usize::MAX, Vec::len);
    let saved = u64::try_from(putting.best.saturating_sub(len)).unwrap_or(u64::MAX);
    learning.window.record(rank, saved, putting.trial_ns);
    if len < putting.best {
      putting.best = len;
      putting.choice = match candidate {
        Candidate::Named => Choice {
          base: putting
            .object
            .named
            .as_ref()
            .map_or(Base::Whole, |named| Base::Named {
              id: named.id,
              position: named.position,
            }),
          delta,
          depth: 1,
        },
        Candidate::Recent(index) => {
          let recent = self
            .recent
            .get(&putting.object.kind)
            .and_then(|recent| recent.get(index));
          match recent {
            Some(recent) => Choice {
              base: Base::Earlier {
                at: recent.at,
                id: recent.id,
                position: recent.position,
              },
              delta,
              depth: recent.depth.saturating_add(1),
            },
            None => Choice {
              base: Base::Whole,
              delta: None,
              depth: 0,
            },
          }
        }
      };
    }
    putting.trying = putting.trying.saturating_add(1);
    putting.search = Search::new();
    putting.trial_ns = 0;
    Ok(())
  }

  /// A step of the deflate: the entry's header first, then a window of the data; once the last is
  /// fed, the entry's record, and the object kept as a candidate.
  fn deflate_step(
    &mut self,
    putting: &mut Putting,
    learning: &mut Learning<'_>,
    window: usize,
  ) -> Result<Put, FragmentRefused> {
    let PutStage::Deflate {
      ref mut compress,
      ref mut crc,
      ref mut next,
      ref mut header,
    } = putting.stage
    else {
      return Ok(Put::More(Vec::new()));
    };
    let mut bytes = core::mem::take(header);
    let data = match &putting.choice.delta {
      Some(delta) => delta.as_slice(),
      None => putting.object.bytes.as_slice(),
    };
    let from = (*next).min(data.len());
    let to = from.saturating_add(window).min(data.len());
    let last = to >= data.len();
    bytes.extend_from_slice(&deflate_more(
      compress,
      data.get(from..to).unwrap_or_default(),
      last,
    )?);
    *next = to;
    crc.update(&bytes);
    if !last {
      return Ok(Put::More(bytes));
    }
    let crc = crc.clone().finalize();
    let (_, stored, base) = putting.representation();
    let placed = Placed {
      id: putting.object.id,
      position: putting.object.position,
      kind: putting.object.kind,
      offset: putting.at,
      crc,
      depth: putting.choice.depth,
      stored,
      base,
    };
    let object = core::mem::replace(
      &mut putting.object,
      ToEncode {
        id: placed.id,
        position: placed.position,
        kind: placed.kind,
        bytes: Vec::new(),
        named: None,
      },
    );
    self.remember(object, (putting.at, putting.choice.depth), learning);
    Ok(Put::Done(bytes, placed))
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

/// One level sampled on `sample`: deflated at `level`, timed and taught to `kind`'s levels.
fn sample_level(
  kind: Kind,
  sample: &[u8],
  level: u32,
  learning: &mut Learning<'_>,
) -> Result<(), FragmentRefused> {
  let output = deflate(sample, level)?;
  let ns = learning
    .exchange
    .rates
    .deflate_ns(level, to_u64(sample.len()));
  learning.levels.record(
    kind,
    level,
    (to_u64(sample.len()), to_u64(output.len()), ns),
  );
  Ok(())
}

/// `data` deflated at every level, each timed and taught to `kind`'s levels; the outputs, by level.
fn sampled(
  kind: Kind,
  data: &[u8],
  learning: &mut Learning<'_>,
) -> Result<Vec<Vec<u8>>, FragmentRefused> {
  let mut outputs = Vec::new();
  for level in 0..LEVELS {
    let output = deflate(data, level)?;
    let ns = learning
      .exchange
      .rates
      .deflate_ns(level, to_u64(data.len()));
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
