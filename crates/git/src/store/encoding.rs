//! A version's fragments, or a green's base pack, encoded a unit at a time (A-653 continued (2); §4.17.2 "The
//! repository in git's terms" item 4; A-614 part 2's fragments, A-639's turns). CLAUDE.md §3's bounded-work rule
//! chunks every loop over user-scaled data into cooperative slices under the shard's budget. A version brings any
//! number of objects and a base pack holds every object a head reaches, so the owner encodes either a turn at a time
//! ([`Encoding::advance`]), each turn units of work until the owner's quantum (§4.3's step budget) is spent, at least
//! one, as upload-pack's stream does ([`super::Upload`]).
//!
//! **The units** (A-718: each a window's work at most, by its bytes, not by wall time, so no poll of the task that
//! drives it outgrows its turn by more than a window's work; found by use: a 56 MB push of git/git cut at v2.0.0 spun
//! the shard 3.5 s in one unit, an object's whole delta trials and level samples, and the daemon died into a restart
//! loop). The walk of a version's root tree beside the one before it, for the bases a reference delta may name: one
//! pair of trees a unit, the pairs still to walk held. The objects' rows read into the encoding's order (by type, the
//! largest first, then by id, which each object's row gives without its bytes): one row a unit (a window's worth of
//! handles had been 32,768 rows, 315 ms of one unit in a debug build), the order held as two sets, one a part. One
//! object encoded: its row and its named base's row read; a window of its bytes, then of its base's, read a unit; then
//! its entry made by the part's encoder a window's work a unit ([`super::fragment::Encoder::step`]): a step of one
//! candidate base's delta search (`deltas::Search`, a window's blocks indexed or a window of the target scanned; the
//! same delta `delta_of` makes), one level sampled on the data's first window, or a window of the data deflated (the
//! same stream one call deflates). Last, each part put into the content store a window a unit
//! (`ContentStore::put_window`, then its lists), and both recorded.
//!
//! **Between turns** the encoding holds no borrow of the store or the repository: it names objects by id and position
//! and holds the order, the walk's bases, the object under way, the part's bytes so far and its encoder's recent
//! entries, those its learned window can try. Each unit learns into the repository's own window and levels, as one
//! pass did, so a version encoded over any turns is the version encoded in one, byte for byte
//! (`tests::store::encoding`); an object no longer than a window is the entry one call made before A-718, byte for
//! byte, and a longer one samples its kind's levels on its first window, as upload-pack's stream does.
//!
//! **What one unit still costs** (owed): a pair of trees is read whole; the last unit records both parts, sorting each
//! part's entries by position and gathering its position sets in one pass (in memory, a few words an entry); and the
//! object, its base and the candidates its learned window tries are held whole while it is encoded, as before.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

use slates_vcs::{ContentId, ContentStore, Writing};

use crate::entry::PACK_HEADER_LEN;
use crate::object_name::{Kind, ObjectId};
use crate::to_u64;

use super::fragment::{self, Encoded, Encoder, Named, Placed, Put, Putting, ToEncode};
use super::learn::{Clock, Exchange};
use super::positions::Positions;
use super::repository::{Repository, StoreRefused};
use super::upload::{Progress, Turn};

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

/// An object being encoded (A-718): its bytes read a window a unit, then its named base's, then its entry made a
/// window's work a unit by the part's encoder.
enum Current {
  /// Reading the object's bytes, then its named base's.
  Read {
    object: Reading,
    named: Option<Reading>,
  },
  /// Its entry being made.
  Put(Putting),
}

/// An object's bytes being read a window a unit: its id, position, type, length, content and the bytes read so far.
struct Reading {
  id: ObjectId,
  position: u64,
  kind: Kind,
  len: u64,
  content: ContentId,
  bytes: Vec<u8>,
}

impl Reading {
  /// The next window of the object read, when one is left: whether one was.
  fn read_window(&mut self, store: &ContentStore) -> Result<bool, StoreRefused> {
    let id = self.id;
    if to_u64(self.bytes.len()) >= self.len {
      return Ok(false);
    }
    let window = to_u64(store.window().max(1));
    let index = to_u64(self.bytes.len()) / window;
    let bytes = store
      .window_at(&self.content, self.len, index)
      .ok_or(StoreRefused::Unreadable { id })?;
    self.bytes.extend_from_slice(&bytes);
    Ok(true)
  }
}

/// Where an encoding is.
enum Stage {
  /// Walking the two root trees for the bases a reference delta may name: the pairs still to walk.
  Walk { pending: Vec<(ObjectId, ObjectId)> },
  /// Reading the objects' rows into the order, from position `from` on.
  Gather { from: u64 },
  /// Encoding the commits, trees and tags' part, or the blobs' when `blobs`: the object under way, if any.
  Encode {
    blobs: bool,
    part: Part,
    current: Option<Box<Current>>,
  },
  /// Putting a part's bytes into the content store a window a unit (A-718): the commits, trees and tags' part first
  /// (`blobs` false), then the blobs', the next byte to put, the object being written, and the first part's root once
  /// it is kept.
  Keep {
    objects: Encoded,
    blobs: Encoded,
    putting_blobs: bool,
    next: usize,
    writing: Writing,
    objects_root: Option<ContentId>,
  },
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
  /// Units that alone took longer than their turn's quantum (A-718: a unit past the step budget, the signal of a unit
  /// to split).
  pub units_past_quantum: u64,
  /// The longest unit of each stage, in the owner's nanoseconds: the walk, the gather, the encoding, the keeping.
  pub longest_unit_ns: [u64; 4],
  /// The most bytes one unit read, put into the store, or was given to search or deflate (A-718: a window at most).
  pub most_bytes_a_unit: u64,
  /// The most rows one unit read into the order (A-718: one).
  pub most_rows_a_unit: u64,
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
      let stage = self.stage_index();
      let progress = match self.unit(repository, store, &mut *clock, exchange) {
        Ok(progress) => progress,
        Err(refused) => {
          self.stage = Stage::Refused(refused);
          return Err(refused);
        }
      };
      let unit_ns = clock.now_ns().saturating_sub(unit_start);
      self.spent_ns = self.spent_ns.saturating_add(unit_ns);
      if unit_ns > quantum_ns {
        self.counters.units_past_quantum = self.counters.units_past_quantum.saturating_add(1);
      }
      if let Some(longest) = self.counters.longest_unit_ns.get_mut(stage) {
        *longest = (*longest).max(unit_ns);
      }
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

  /// The stage's index among [`EncodingCounters::longest_unit_ns`]'s.
  pub fn stage_index(&self) -> usize {
    match self.stage {
      Stage::Walk { .. } => 0,
      Stage::Gather { .. } => 1,
      Stage::Encode { .. } => 2,
      Stage::Keep { .. } | Stage::Done | Stage::Refused(_) => 3,
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
      Stage::Encode {
        blobs,
        part,
        current,
      } => self.encode(repository, store, clock, exchange, (blobs, part, current))?,
      keeping @ Stage::Keep { .. } => self.keep(repository, store, keeping)?,
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

  /// One object's row read into the order (module docs; A-718: a row a unit); with none left, the commits, trees and
  /// tags encoded next.
  fn gather(
    &mut self,
    repository: &Repository,
    store: &ContentStore,
    from: u64,
  ) -> Result<Stage, StoreRefused> {
    let Some(position) = self.positions.next_from(from) else {
      return Ok(Stage::Encode {
        blobs: false,
        part: Part::new(),
        current: None,
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
    self.counters.most_rows_a_unit = self.counters.most_rows_a_unit.max(1);
    Ok(Stage::Gather {
      from: position.saturating_add(1),
    })
  }

  /// One unit of the part's encoding (module docs; A-718): the next object begun (its row and its named base's
  /// read), a window of its bytes or its base's read, or a step of its entry; with none left, the part ended.
  fn encode(
    &mut self,
    repository: &mut Repository,
    store: &ContentStore,
    clock: &mut dyn Clock,
    exchange: Exchange,
    (blobs, mut part, current): (bool, Part, Option<Box<Current>>),
  ) -> Result<Stage, StoreRefused> {
    let Some(mut current) = current else {
      return self.begin_object(repository, store, (blobs, part));
    };
    let window = store.window().max(1);
    let next = match *current {
      Current::Read {
        ref mut object,
        ref mut named,
      } => {
        let before = object.bytes.len();
        let mut read = object.read_window(store)?;
        let mut taken = object.bytes.len().saturating_sub(before);
        if !read && let Some(named) = named.as_mut() {
          let before = named.bytes.len();
          read = named.read_window(store)?;
          taken = named.bytes.len().saturating_sub(before);
        }
        self.counters.most_bytes_a_unit = self.counters.most_bytes_a_unit.max(to_u64(taken));
        if read {
          Some(current)
        } else {
          let Current::Read { object, named } = *current else {
            return Err(StoreRefused::FragmentUnreadable);
          };
          let at = PACK_HEADER_LEN.saturating_add(to_u64(part.bytes.len()));
          let to_encode = ToEncode {
            id: object.id,
            position: object.position,
            kind: object.kind,
            bytes: object.bytes,
            named: named.map(|named| Named {
              id: named.id,
              position: named.position,
              bytes: named.bytes,
            }),
          };
          let learning = repository.learning(exchange, clock);
          let putting = part.encoder.begin(to_encode, at, &learning);
          Some(Box::new(Current::Put(putting)))
        }
      }
      Current::Put(ref mut putting) => {
        let mut learning = repository.learning(exchange, clock);
        let put = part
          .encoder
          .step(putting, &mut learning, window)
          .map_err(StoreRefused::Encoding)?;
        self.counters.most_bytes_a_unit = self.counters.most_bytes_a_unit.max(to_u64(window));
        match put {
          Put::More(bytes) => {
            part.bytes.extend_from_slice(&bytes);
            Some(current)
          }
          Put::Done(bytes, record) => {
            part.bytes.extend_from_slice(&bytes);
            part.placed.push(fragment::in_fragment(record));
            self.counters.objects = self.counters.objects.saturating_add(1);
            None
          }
        }
      }
    };
    Ok(Stage::Encode {
      blobs,
      part,
      current: next,
    })
  }

  /// The part's next object begun: its row and its named base's row read, its bytes left to read a window a unit;
  /// with none left, the part ended.
  fn begin_object(
    &mut self,
    repository: &Repository,
    store: &ContentStore,
    (blobs, part): (bool, Part),
  ) -> Result<Stage, StoreRefused> {
    let order = if blobs {
      &mut self.blobs
    } else {
      &mut self.others
    };
    let Some((_, _, id, position)) = order.pop_first() else {
      return Ok(self.part_ended(blobs, part));
    };
    let (row, content) = repository
      .indexed(store, &id)
      .ok_or(StoreRefused::Missing { id })?;
    let object = Reading {
      id,
      position,
      kind: row.kind,
      len: row.len,
      content,
      bytes: Vec::new(),
    };
    let named = match self.named.get(&id) {
      Some(old) => {
        let (row, content) = repository
          .indexed(store, old)
          .ok_or(StoreRefused::Missing { id: *old })?;
        Some(Reading {
          id: *old,
          position: row.position,
          kind: row.kind,
          len: row.len,
          content,
          bytes: Vec::new(),
        })
      }
      None => None,
    };
    Ok(Stage::Encode {
      blobs,
      part,
      current: Some(Box::new(Current::Read { object, named })),
    })
  }

  /// A part with nothing left to encode: the blobs' part next, or both parts kept.
  fn part_ended(&mut self, blobs: bool, part: Part) -> Stage {
    let encoded = (part.bytes, part.placed);
    if !blobs {
      self.objects = Some(encoded);
      return Stage::Encode {
        blobs: true,
        part: Part::new(),
        current: None,
      };
    }
    Stage::Keep {
      objects: self.objects.take().unwrap_or_default(),
      blobs: encoded,
      putting_blobs: false,
      next: 0,
      writing: Writing::default(),
      objects_root: None,
    }
  }

  /// One unit of keeping both parts (module docs; A-718): a window of a part put into the content store, a part's
  /// object finished, or, both kept, both recorded (a version's fragments, or a base pack in place of the old).
  /// Refused, what was put is given back.
  fn keep(
    &mut self,
    repository: &mut Repository,
    store: &mut ContentStore,
    keeping: Stage,
  ) -> Result<Stage, StoreRefused> {
    let Stage::Keep {
      objects,
      blobs,
      putting_blobs,
      next,
      mut writing,
      objects_root,
    } = keeping
    else {
      return Ok(keeping);
    };
    let window = store.window().max(1);
    let bytes = if putting_blobs { &blobs.0 } else { &objects.0 };
    let give_back = |store: &mut ContentStore, writing: Writing, root: Option<ContentId>| {
      store.abandon_object(writing);
      if let Some(root) = root {
        let _ = store.release(&root);
      }
    };
    if next < bytes.len() {
      let to = next.saturating_add(window).min(bytes.len());
      if let Err(refused) = store.put_window(&mut writing, bytes.get(next..to).unwrap_or_default())
      {
        give_back(store, writing, objects_root);
        return Err(refused.into());
      }
      self.counters.most_bytes_a_unit = self
        .counters
        .most_bytes_a_unit
        .max(to_u64(to.saturating_sub(next)));
      return Ok(Stage::Keep {
        objects,
        blobs,
        putting_blobs,
        next: to,
        writing,
        objects_root,
      });
    }
    let root = match store.finish_object(&mut writing) {
      Ok(root) => root,
      Err(refused) => {
        give_back(store, writing, objects_root);
        return Err(refused.into());
      }
    };
    if !putting_blobs {
      return Ok(Stage::Keep {
        objects,
        blobs,
        putting_blobs: true,
        next: 0,
        writing: Writing::default(),
        objects_root: Some(root),
      });
    }
    let Some(objects_root) = objects_root else {
      let _ = store.release(&root);
      return Err(StoreRefused::FragmentUnreadable);
    };
    let parts = ((objects, objects_root), (blobs, root));
    match self.target {
      Target::Version { green, version } => {
        repository.keep_version(store, (green, version), parts)?
      }
      Target::Base { green, version } => {
        repository.keep_base(store, (green, version), parts, self.spent_ns)?;
      }
    }
    Ok(Stage::Done)
  }
}
