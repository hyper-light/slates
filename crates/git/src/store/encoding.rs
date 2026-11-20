//! A version's fragments, or a green's base pack, encoded a unit at a time (A-653 continued (2); §4.17.2 "The
//! repository in git's terms" item 4; A-614 part 2's fragments, A-639's turns). CLAUDE.md §3's bounded-work rule
//! chunks every loop over user-scaled data into cooperative slices under the shard's budget. A version brings any
//! number of objects and a base pack holds every object a head reaches, so the owner encodes either a turn at a time
//! ([`Encoding::advance`]), each turn units of work until the owner's quantum (§4.3's step budget) is spent, at least
//! one, as upload-pack's stream does ([`super::Upload`]).
//!
//! **The units.** The walk of a version's root tree beside the one before it, for the bases a reference delta may
//! name: one pair of trees a unit, the pairs still to walk held. The objects' rows read into the encoding's order (by
//! type, the largest first, then by id, which each object's row gives without its bytes): a window's worth of handles
//! a unit, the order held as two sets, one a part. One object encoded: its bytes and its named base's read, then one
//! entry put through the part's encoder, whose delta trials the learned window bounds. Last, both parts kept as content
//! and recorded, one unit.
//!
//! **Between turns** the encoding holds no borrow of the store or the repository: it names objects by id and position
//! and holds the order, the walk's bases, the part's bytes so far and its encoder's recent entries, those its learned
//! window can try. Each unit learns into the repository's own window and levels, as one pass did, so a version encoded
//! over any turns is the version encoded in one, byte for byte (`tests::store::encoding`).
//!
//! **What one unit still costs** (owed): one object's encoding is bounded by that object and the learned window, not by
//! a window of bytes (an object past a window encoded a window a unit is A-639's owed delta search past a window); a
//! pair of trees is read whole; the last unit puts each part whole into the content store, which has no put by pieces.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

use slates_vcs::ContentStore;

use crate::entry::PACK_HEADER_LEN;
use crate::object_name::{Kind, ObjectId};
use crate::to_u64;

use super::fragment::{self, Encoded, Encoder, Placed, ToEncode};
use super::learn::{Clock, Exchange};
use super::positions::Positions;
use super::repository::{Repository, StoreRefused};
use super::upload::{Progress, Turn, handles};

/// An object's place in the encoding's order: its type, its length (the largest first), its id, and its position.
type Key = (Kind, Reverse<u64>, ObjectId, u64);

/// What an encoding makes: a version's two fragments, or a green's base pack from its head version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
  /// `green`'s version `version`'s fragments.
  Version { green: u64, version: usize },
  /// `green`'s base pack, made from version `version`, its head when the making began.
  Base { green: u64, version: usize },
}

/// One part being encoded: its encoder, its bytes so far and its entries.
struct Part {
  encoder: Encoder,
  bytes: Vec<u8>,
  placed: Vec<Placed>,
}

impl Part {
  /// A part with nothing encoded, its earlier entries named by their offsets back.
  fn new() -> Part {
    Part {
      encoder: Encoder::new(true),
      bytes: Vec::new(),
      placed: Vec::new(),
    }
  }
}

/// Where an encoding is.
enum Stage {
  /// Walking the two root trees for the bases a reference delta may name: the pairs still to walk.
  Walk { pending: Vec<(ObjectId, ObjectId)> },
  /// Reading the objects' rows into the order, from position `from` on.
  Gather { from: u64 },
  /// Encoding the commits, trees and tags' part, or the blobs' when `blobs`.
  Encode { blobs: bool, part: Part },
  /// Keeping both parts as content and recording them.
  Keep { objects: Encoded, blobs: Encoded },
  /// Kept, or nothing to encode.
  Done,
  /// Refused; every later turn is refused the same.
  Refused(StoreRefused),
}

/// What an encoding did, for a test to see its turns sliced.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EncodingCounters {
  /// Turns taken.
  pub turns: u64,
  /// Units of work done.
  pub units: u64,
  /// Pairs of trees walked for the bases a reference delta may name.
  pub trees_walked: u64,
  /// Objects' rows read into the encoding's order.
  pub rows_read: u64,
  /// Objects encoded.
  pub objects: u64,
}

/// A version's fragments, or a green's base pack, being encoded (module docs).
pub struct Encoding {
  target: Target,
  /// The objects to encode, by position.
  positions: Positions,
  /// For each object a reference delta may name a base for, that base.
  named: BTreeMap<ObjectId, ObjectId>,
  /// The commits, trees and tags not yet encoded, in order.
  others: BTreeSet<Key>,
  /// The blobs not yet encoded, in order.
  blobs: BTreeSet<Key>,
  /// The commits, trees and tags' part, once encoded.
  objects: Option<Encoded>,
  stage: Stage,
  /// The owner's time the units so far took: a base pack's making, taught to the rebuild rule.
  spent_ns: u64,
  counters: EncodingCounters,
}

impl Encoding {
  /// The encoding of `green`'s version `version` as its two fragments; one with nothing to do once they are kept.
  pub fn fragments(
    repository: &Repository,
    store: &ContentStore,
    green: u64,
    version: usize,
  ) -> Result<Encoding, StoreRefused> {
    let target = Target::Version { green, version };
    let Some(brought) = repository.unencoded(store, green, version)? else {
      return Ok(Encoding::new(target, Positions::new(), Stage::Done));
    };
    let old = version
      .checked_sub(1)
      .and_then(|previous| repository.tree(store, green, previous));
    let pending = match (repository.tree(store, green, version), old) {
      (Some(new), Some(old)) => vec![(new, old)],
      _ => Vec::new(),
    };
    Ok(Encoding::new(target, brought, Stage::Walk { pending }))
  }

  /// The making of `green`'s base pack again, from every object its head reaches, no base named.
  pub fn base(
    repository: &Repository,
    store: &ContentStore,
    green: u64,
  ) -> Result<Encoding, StoreRefused> {
    let (version, reached) = repository.head_reach(store, green)?;
    let target = Target::Base { green, version };
    Ok(Encoding::new(target, reached, Stage::Gather { from: 0 }))
  }

  /// An encoding of `positions` for `target`, at `stage`.
  fn new(target: Target, positions: Positions, stage: Stage) -> Encoding {
    Encoding {
      target,
      positions,
      named: BTreeMap::new(),
      others: BTreeSet::new(),
      blobs: BTreeSet::new(),
      objects: None,
      stage,
      spent_ns: 0,
      counters: EncodingCounters::default(),
    }
  }

  /// What the encoding did.
  pub fn counters(&self) -> EncodingCounters {
    self.counters
  }

  /// One turn: units of work until the turn's quantum is spent, at least one (module docs). Refused, nothing is kept,
  /// and every later turn is refused the same.
  pub fn advance(
    &mut self,
    repository: &mut Repository,
    store: &mut ContentStore,
    turn: Turn<'_>,
  ) -> Result<Progress, StoreRefused> {
    let Turn {
      clock,
      exchange,
      quantum_ns,
    } = turn;
    self.counters.turns = self.counters.turns.saturating_add(1);
    let start = clock.now_ns();
    loop {
      let unit_start = clock.now_ns();
      let progress = match self.unit(repository, store, &mut *clock, exchange) {
        Ok(progress) => progress,
        Err(refused) => {
          self.stage = Stage::Refused(refused);
          return Err(refused);
        }
      };
      let unit_ns = clock.now_ns().saturating_sub(unit_start);
      self.spent_ns = self.spent_ns.saturating_add(unit_ns);
      if progress == Progress::Done {
        return Ok(Progress::Done);
      }
      if clock.now_ns().saturating_sub(start) >= quantum_ns {
        return Ok(Progress::More);
      }
    }
  }

  /// The encoding driven to its end in one turn whose quantum is never spent: the one-call form of
  /// `Repository::encode_fragments` and `Repository::rebuild_base`, for tests and tools.
  pub(super) fn finish(
    mut self,
    repository: &mut Repository,
    store: &mut ContentStore,
    clock: &mut dyn Clock,
    exchange: Exchange,
  ) -> Result<(), StoreRefused> {
    loop {
      let turn = Turn {
        clock: &mut *clock,
        exchange,
        quantum_ns: u64::MAX,
      };
      if self.advance(repository, store, turn)? == Progress::Done {
        return Ok(());
      }
    }
  }

  /// One unit of work: the stage's next step.
  fn unit(
    &mut self,
    repository: &mut Repository,
    store: &mut ContentStore,
    clock: &mut dyn Clock,
    exchange: Exchange,
  ) -> Result<Progress, StoreRefused> {
    let stage = core::mem::replace(&mut self.stage, Stage::Done);
    self.stage = match stage {
      Stage::Walk { pending } => self.walk(repository, store, pending)?,
      Stage::Gather { from } => self.gather(repository, store, from)?,
      Stage::Encode { blobs, part } => {
        self.encode(repository, store, clock, exchange, (blobs, part))?
      }
      Stage::Keep { objects, blobs } => {
        self.keep(repository, store, (objects, blobs))?;
        Stage::Done
      }
      Stage::Done => return Ok(Progress::Done),
      Stage::Refused(refused) => return Err(refused),
    };
    self.counters.units = self.counters.units.saturating_add(1);
    Ok(if matches!(self.stage, Stage::Done) {
      Progress::Done
    } else {
      Progress::More
    })
  }

  /// One pair of trees walked (module docs); with none left, the rows read next.
  fn walk(
    &mut self,
    repository: &Repository,
    store: &ContentStore,
    mut pending: Vec<(ObjectId, ObjectId)>,
  ) -> Result<Stage, StoreRefused> {
    let Some((new, old)) = pending.pop() else {
      return Ok(Stage::Gather { from: 0 });
    };
    if new != old && self.named.insert(new, old).is_none() {
      self.counters.trees_walked = self.counters.trees_walked.saturating_add(1);
      self.compare(repository, store, (new, old), &mut pending)?;
    }
    Ok(Stage::Walk { pending })
  }

  /// `new`'s entries beside `old`'s: a subtree replaced at its name held to walk, any other object replaced by one of
  /// its kind named as its base.
  fn compare(
    &mut self,
    repository: &Repository,
    store: &ContentStore,
    (new, old): (ObjectId, ObjectId),
    pending: &mut Vec<(ObjectId, ObjectId)>,
  ) -> Result<(), StoreRefused> {
    let olds = repository.tree_entries(store, &old)?;
    for (name, (is_dir, id)) in repository.tree_entries(store, &new)? {
      let Some(&(was_dir, was)) = olds.get(&name) else {
        continue;
      };
      if was_dir != is_dir || was == id {
        continue;
      }
      if is_dir {
        pending.push((id, was));
      } else {
        self.named.insert(id, was);
      }
    }
    Ok(())
  }

  /// A window's worth of the objects' rows read into the order (module docs); with none left, the commits, trees and
  /// tags encoded next.
  fn gather(
    &mut self,
    repository: &Repository,
    store: &ContentStore,
    from: u64,
  ) -> Result<Stage, StoreRefused> {
    let mut at = from;
    for _ in 0..handles(store) {
      let Some(position) = self.positions.next_from(at) else {
        return Ok(Stage::Encode {
          blobs: false,
          part: Part::new(),
        });
      };
      let id = repository
        .id_at(store, position)
        .ok_or(StoreRefused::NoPosition { position })?;
      let (row, _) = repository
        .indexed(store, &id)
        .ok_or(StoreRefused::Missing { id })?;
      let order = if row.kind == Kind::Blob {
        &mut self.blobs
      } else {
        &mut self.others
      };
      order.insert((row.kind, Reverse(row.len), id, position));
      self.counters.rows_read = self.counters.rows_read.saturating_add(1);
      at = position.saturating_add(1);
    }
    Ok(Stage::Gather { from: at })
  }

  /// One object encoded into the part (module docs); with none left, the part ended.
  fn encode(
    &mut self,
    repository: &mut Repository,
    store: &ContentStore,
    clock: &mut dyn Clock,
    exchange: Exchange,
    (blobs, mut part): (bool, Part),
  ) -> Result<Stage, StoreRefused> {
    let order = if blobs {
      &mut self.blobs
    } else {
      &mut self.others
    };
    let Some((_, _, id, position)) = order.pop_first() else {
      return Ok(self.part_ended(blobs, part));
    };
    let (kind, bytes) = repository.read(store, &id)?;
    let named = match self.named.get(&id) {
      Some(old) => Some(repository.named(store, old)?),
      None => None,
    };
    let object = ToEncode {
      id,
      position,
      kind,
      bytes,
      named,
    };
    let at = PACK_HEADER_LEN.saturating_add(to_u64(part.bytes.len()));
    let mut learning = repository.learning(exchange, clock);
    let (entry, record) = part
      .encoder
      .put(object, at, &mut learning)
      .map_err(StoreRefused::Encoding)?;
    part.bytes.extend_from_slice(&entry);
    part.placed.push(fragment::in_fragment(record));
    self.counters.objects = self.counters.objects.saturating_add(1);
    Ok(Stage::Encode { blobs, part })
  }

  /// A part with nothing left to encode: the blobs' part next, or both parts kept.
  fn part_ended(&mut self, blobs: bool, part: Part) -> Stage {
    let encoded = (part.bytes, part.placed);
    if !blobs {
      self.objects = Some(encoded);
      return Stage::Encode {
        blobs: true,
        part: Part::new(),
      };
    }
    Stage::Keep {
      objects: self.objects.take().unwrap_or_default(),
      blobs: encoded,
    }
  }

  /// Both parts kept as content and recorded (module docs): a version's fragments, or a base pack in place of the old.
  fn keep(
    &self,
    repository: &mut Repository,
    store: &mut ContentStore,
    parts: (Encoded, Encoded),
  ) -> Result<(), StoreRefused> {
    match self.target {
      Target::Version { green, version } => repository.keep_version(store, (green, version), parts),
      Target::Base { green, version } => {
        repository.keep_base(store, (green, version), parts, self.spent_ns)
      }
    }
  }
}
