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
//! **A pushed entry sent on as it lies** (A-859, git's pack reuse). An object a push brought in a pack is written into
//! its part as its entry lies there, its compressed data copied a window a unit, once its check holds (a whole entry's
//! data inflates to the object its name says; a delta's, applied to its base, to the object, which its name says) and
//! its base precedes it: an earlier entry of the part (an offset delta, its chain within the deepest any push
//! carried) or an object of an earlier version (a reference delta, beginning a chain). Otherwise, or where its check
//! fails (counted `reuse_refused`), it is encoded as below. Counted `reused_whole` and `reused_deltas`.
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
use std::collections::{BTreeMap, BTreeSet, HashMap};

use slates_vcs::{ContentId, ContentStore, Writing};

use crate::entry::{self, PACK_HEADER_LEN, Representation};
use crate::object_name::{Kind, ObjectId};
use crate::to_u64;

use super::fragment::{self, Encoded, Encoder, Named, Placed, Put, Putting, Stored, ToEncode};
use super::learn::{Clock, Exchange};
use super::packed::{self, InPack, PackSource};
use super::positions::Positions;
use super::push::StoredAt;
use super::repository::{Repository, StoreRefused};
use super::rows::{self, ObjectRow};
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
  /// Where each placed object's record is in `placed`, by its id (A-859: a reused delta's base found in one lookup,
  /// however many the part holds).
  by_id: HashMap<ObjectId, usize>,
}

impl Part {
  /// A part with nothing encoded, its earlier entries named by their offsets back.
  fn new() -> Part {
    Part {
      encoder: Encoder::new(true),
      bytes: Vec::new(),
      placed: Vec::new(),
      by_id: HashMap::new(),
    }
  }

  /// `record` placed in the part, its entry's bytes written already.
  fn place(&mut self, record: Placed) {
    self.by_id.insert(record.id, self.placed.len());
    self.placed.push(fragment::in_fragment(record));
  }

  /// The record of `id` placed in the part; none where it is not.
  fn placed_of(&self, id: &ObjectId) -> Option<&Placed> {
    self.placed.get(*self.by_id.get(id)?)
  }
}

/// An object being encoded (A-718): its bytes read a window a unit, then its named base's, then its entry made a
/// window's work a unit by the part's encoder.
enum Current {
  /// Reading the object's bytes, then its named base's.
  Read {
    object: Box<Reading>,
    named: Option<Box<Reading>>,
  },
  /// Its entry being made (boxed: the encoder's state is most of the object's size).
  Put(Box<Putting>),
  /// Its pushed entry's data being copied a window a unit (A-859), its header written.
  Copy(Box<Copying>),
}

/// Whether a pushed entry may be sent on for `object` (A-859): its name its bytes', and then, whole, its data (inflated in
/// `scratch`) the object its name says; a delta, its data applied to its base `base_id` the object's bytes.
fn entry_checked(
  repository: &Repository,
  store: &ContentStore,
  (object, raw, scratch, base_id): (&Reading, &packed::RawEntry, &[u8], Option<ObjectId>),
) -> bool {
  let format = repository.format();
  let named = |bytes: &[u8]| {
    crate::object_name::name_of(format, object.kind, bytes).is_ok_and(|named| named == object.id)
  };
  if !named(&object.bytes) {
    return false;
  }
  match base_id {
    None => raw.representation == Representation::Whole(object.kind) && named(scratch),
    Some(base) => repository
      .indexed(store, &base)
      .and_then(|(row, content)| {
        repository.read_stored(store, &repository.stored_of(&row, content), row.len)
      })
      .is_some_and(|base_bytes| {
        let mut built = Vec::new();
        crate::delta::apply_to(&base_bytes, scratch, &mut built).is_ok() && built == object.bytes
      }),
  }
}

/// The copy of `object`'s pushed entry (`raw`, at `place`) written at `at` as `placed` says, its header `header`
/// written and counted into its CRC.
fn copying_of(
  object: &Reading,
  (place, raw): (InPack, &packed::RawEntry),
  (at, depth, stored, base): (u64, u64, Stored, Option<u64>),
  header: &[u8],
) -> Copying {
  let mut crc = crc32fast::Hasher::new();
  crc.update(header);
  Copying {
    placed: Placed {
      id: object.id,
      position: object.position,
      kind: object.kind,
      offset: at,
      crc: 0,
      depth,
      stored,
      base,
    },
    object: Some(ToEncode {
      id: object.id,
      position: object.position,
      kind: object.kind,
      bytes: object.bytes.clone(),
      named: None,
    }),
    place,
    from: raw.data,
    end: raw.end,
    crc,
  }
}

/// A pushed entry being copied into a part (A-859): its record (its CRC once its last byte is copied), its object (kept
/// as a candidate base after, where it was read: one sent on by its kept CRC32 is not, A-864), where its data lies and
/// how far it is copied, and the CRC of what is copied so far.
struct Copying {
  placed: Placed,
  object: Option<ToEncode>,
  place: InPack,
  from: u64,
  end: u64,
  crc: crc32fast::Hasher,
}

/// An object's bytes being read a window a unit: its id, position, type, length, content and the bytes read so far.
struct Reading {
  id: ObjectId,
  position: u64,
  kind: Kind,
  len: u64,
  stored: StoredAt,
  bytes: Vec<u8>,
}

impl Reading {
  /// The next window of the object read, when one is left: whether one was. An object read where it lies in a pack
  /// (A-722's record v4) is read whole at its first window, its chain resolved at once.
  fn read_window(
    &mut self,
    repository: &Repository,
    store: &ContentStore,
  ) -> Result<bool, StoreRefused> {
    let id = self.id;
    if to_u64(self.bytes.len()) >= self.len {
      return Ok(false);
    }
    let content = match &self.stored {
      StoredAt::Whole(content) => *content,
      StoredAt::InPack(_) => {
        let bytes = repository
          .read_stored(store, &self.stored, self.len)
          .ok_or(StoreRefused::Unreadable { id })?;
        self.bytes.extend_from_slice(&bytes);
        return Ok(true);
      }
    };
    let window = to_u64(store.window().max(1));
    let index = to_u64(self.bytes.len()).checked_div(window).unwrap_or(0);
    let bytes = store
      .window_at(&content, self.len, index)
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
    part: Box<Part>,
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
    writing: Box<Writing>,
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
  /// [`EncodingCounters::units_past_quantum`] by the stage the unit was in: the walk, the gather, the encoding, the
  /// keeping (as [`EncodingCounters::longest_unit_ns`]), so a unit to split is named by its stage.
  /// Shape: one a stage, four.
  pub units_past_quantum_by_stage: [u64; 4],
  /// The longest unit of each stage, in the owner's nanoseconds: the walk, the gather, the encoding, the keeping.
  /// Shape: one a stage, four.
  pub longest_unit_ns: [u64; 4],
  /// The most bytes one unit read, put into the store, or was given to search or deflate (A-718: a window at most).
  pub most_bytes_a_unit: u64,
  /// The most bytes one search or deflate step of an object's entry was given (A-718, main's ruling of 2026-11-21
  /// 07:30: a unit sized from its measured cost within the step budget).
  pub most_put_bytes: u64,
  /// The most rows one unit read into the order (A-718: one).
  pub most_rows_a_unit: u64,
  /// Objects sent on whole as their pushed entry lies, its compressed data copied (A-859, git's pack reuse).
  pub reused_whole: u64,
  /// Objects sent on as their pushed delta (A-859): its data copied, its base named in this part or an earlier version.
  pub reused_deltas: u64,
  /// Objects whose stored entry failed its check (its data not the object its name says) and were encoded instead.
  pub reuse_refused: u64,
  /// Objects that lie in no pack the store holds (kept as content), encoded with no entry to reuse (A-859).
  pub reuse_no_entry: u64,
  /// Objects whose pushed entry's raw bytes did not match the CRC32 its row keeps (A-864), read and judged instead.
  pub reuse_crc_missed: u64,
  /// Objects whose pushed entry could not be read, or whose offset delta's base is no object of this encoding, encoded
  /// instead (A-859).
  pub reuse_unread: u64,
}

/// A version's fragments, or a green's base pack, being encoded (module docs).
pub struct Encoding {
  target: Target,
  /// The objects to encode, by position.
  positions: Positions,
  /// Each object of the encoding read from a pack, by its pack and where its entry begins there (A-859): an OFS_DELTA's
  /// base named by its place, found by its id.
  offsets: BTreeMap<(ContentId, u64), ObjectId>,
  /// For each object a reference delta may name a base for, that base.
  named: BTreeMap<ObjectId, ObjectId>,
  /// The commits, trees and tags not yet encoded, in order.
  others: BTreeSet<Key>,
  /// The blobs not yet encoded, in order.
  blobs: BTreeSet<Key>,
  /// The objects the part encodes before it goes on in its order, the last first (A-859, git's `write_one`: a delta's
  /// base written before it): an object whose pushed entry is a delta against one still in the order, and that base
  /// above it; as deep as the deepest chain any push carried (`Repository::learned_depth`).
  ahead: Vec<Key>,
  /// The let-go window a unit met and stopped at, its work kept to be taken again once it is brought back (A-864 piece
  /// 2's encoding side): taken by [`Encoding::waiting`].
  waiting: core::cell::Cell<Option<packed::Waited>>,
  /// The let-go windows the encoding has stopped at, each by its pack and index: one stop a window (A-864 piece 2's
  /// progress).
  stopped_at: BTreeSet<(ContentId, u64)>,
  /// The commits, trees and tags' part, once encoded.
  objects: Option<Encoded>,
  stage: Stage,
  /// The owner's time the units so far took: a base pack's making, taught to the rebuild rule.
  spent_ns: u64,
  counters: EncodingCounters,
  /// The bytes of its parts it holds between units, charged to the content store's budget as they grow (A-856: what an
  /// encoding holds is charged, never held outside every budget), given back as they are kept or the encoding ends.
  charged: u64,
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
      offsets: BTreeMap::new(),
      named: BTreeMap::new(),
      others: BTreeSet::new(),
      blobs: BTreeSet::new(),
      ahead: Vec::new(),
      waiting: core::cell::Cell::new(None),
      stopped_at: BTreeSet::new(),
      objects: None,
      stage,
      spent_ns: 0,
      counters: EncodingCounters::default(),
      charged: 0,
    }
  }

  /// The bytes of its parts it holds now: the part under way, the commits', trees' and tags' part once encoded, and both
  /// parts while they are kept (A-856).
  pub fn held_bytes(&self) -> u64 {
    let objects = self
      .objects
      .as_ref()
      .map_or(0, |(bytes, _)| to_u64(bytes.len()));
    let stage = match &self.stage {
      Stage::Encode { part, .. } => to_u64(part.bytes.len()),
      Stage::Keep { objects, blobs, .. } => {
        to_u64(objects.0.len()).saturating_add(to_u64(blobs.0.len()))
      }
      _ => 0,
    };
    objects.saturating_add(stage)
  }

  /// Charges `store` for what it holds now past what it charged before, or gives back what it no longer holds (A-856):
  /// refused where the store's budget has no room, the encoding then refused, a refusal that costs a fetch time, never
  /// its answer.
  fn true_up(&mut self, store: &mut ContentStore) -> Result<(), StoreRefused> {
    let held = self.held_bytes();
    if held > self.charged {
      store.charge(held.saturating_sub(self.charged))?;
    } else {
      store.discharge(self.charged.saturating_sub(held))?;
    }
    self.charged = held;
    Ok(())
  }

  /// Gives back everything it charged `store` (A-856): what its owner calls as it drops an encoding not done.
  pub fn release(&mut self, store: &mut ContentStore) {
    let _ = store.discharge(self.charged);
    self.charged = 0;
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
      let progress = match self
        .unit(repository, store, (exchange, quantum_ns))
        .and_then(|progress| self.true_up(store).map(|()| progress))
        .inspect(|_| repository.true_up_recent(store))
      {
        Ok(progress) => progress,
        Err(refused) => {
          self.stage = Stage::Refused(refused);
          self.objects = None;
          self.release(store);
          return Err(refused);
        }
      };
      let unit_ns = clock.now_ns().saturating_sub(unit_start);
      self.spent_ns = self.spent_ns.saturating_add(unit_ns);
      if unit_ns > quantum_ns {
        self.counters.units_past_quantum = self.counters.units_past_quantum.saturating_add(1);
        if let Some(past) = self.counters.units_past_quantum_by_stage.get_mut(stage) {
          *past = past.saturating_add(1);
        }
      }
      if let Some(longest) = self.counters.longest_unit_ns.get_mut(stage) {
        *longest = (*longest).max(unit_ns);
      }
      if progress == Progress::Done {
        return Ok(Progress::Done);
      }
      if self.stopped() || clock.now_ns().saturating_sub(start) >= quantum_ns {
        return Ok(Progress::More);
      }
    }
  }

  /// Stops the turn where `refused`, a read's refusal, met a let-go window, the first time the encoding meets that window
  /// ([`Encoding::waiting`]): `Err(Stopped)` then. A window met again after it was to be brought back is read on, its
  /// read refusing as one that cannot be read does, so a stop always makes progress or ends (A-864 piece 2).
  fn stopped_by(
    &mut self,
    store: &ContentStore,
    refused: &packed::PackedRefused,
  ) -> Result<(), Stopped> {
    match refused.waited() {
      Some(waited) if self.stop_at(store, waited) => Err(Stopped),
      _ => Ok(()),
    }
  }

  /// Notes `waited`'s window to be brought back, the first time it is met: whether the encoding stops for it.
  fn stop_at(&mut self, store: &ContentStore, waited: packed::Waited) -> bool {
    let window = to_u64(store.window()).max(1);
    if !self
      .stopped_at
      .insert((waited.pack, waited.offset / window))
    {
      return false;
    }
    self.waiting.set(Some(waited));
    true
  }

  /// Whether a unit stopped at a let-go window ([`Encoding::waiting`]) not yet taken: its driver ends the turn there.
  pub fn stopped(&self) -> bool {
    let waiting = self.waiting.take();
    self.waiting.set(waiting);
    waiting.is_some()
  }

  /// The let-go window the encoding's last turn stopped at, its unit kept to be taken again once the window is brought
  /// back (A-864 piece 2's encoding side; a pack's windows brought back as the encoding reads them, never all at once):
  /// taken. None where the turn met none.
  pub fn waiting(&self) -> Option<packed::Waited> {
    self.waiting.take()
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
      // Format: each stage's index, in the order of `longest_unit_ns`.
      Stage::Keep { .. } | Stage::Done | Stage::Refused(_) => 3,
    }
  }

  /// One unit of work: the stage's next step, within `quantum_ns` where its work is sized by its measured cost.
  fn unit(
    &mut self,
    repository: &mut Repository,
    store: &mut ContentStore,
    (exchange, quantum_ns): (Exchange, u64),
  ) -> Result<Progress, StoreRefused> {
    let stage = core::mem::replace(&mut self.stage, Stage::Done);
    self.stage = match stage {
      Stage::Walk { pending } => self.walk(repository, store, pending)?,
      Stage::Gather { from } => self.gather(repository, store, from)?,
      Stage::Encode {
        blobs,
        part,
        current,
      } => self.encode(
        repository,
        store,
        (exchange, quantum_ns),
        (blobs, *part, current),
      )?,
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
        part: Box::new(Part::new()),
        current: None,
      });
    };
    let id = repository
      .id_at(store, position)
      .ok_or(StoreRefused::NoPosition { position })?;
    let (row, content) = repository
      .indexed(store, &id)
      .ok_or(StoreRefused::Missing { id })?;
    if let Some(packed) = row.pack {
      self.offsets.insert((content, packed.offset), id);
    }
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
    (exchange, quantum_ns): (Exchange, u64),
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
        let mut read = object.read_window(repository, store)?;
        let mut taken = object.bytes.len().saturating_sub(before);
        if !read && let Some(named) = named.as_mut() {
          let before = named.bytes.len();
          read = named.read_window(repository, store)?;
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
          // A-859: an object pushed in a pack is sent on as its entry lies there, where its check holds and its base
          // the client reads (main 01a20182's ruling of 2026-11-22 05:38: whenever it has one); otherwise it is encoded.
          if let Some((header, copying)) = self.reused(repository, store, (&part, &object, at)) {
            part.bytes.extend_from_slice(&header);
            return Ok(Stage::Encode {
              blobs,
              part: Box::new(part),
              current: Some(Box::new(Current::Copy(Box::new(copying)))),
            });
          }
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
          let learning = repository.learning(exchange);
          let putting = part.encoder.begin(to_encode, at, &learning);
          Some(Box::new(Current::Put(Box::new(putting))))
        }
      }
      Current::Copy(ref mut copying) => {
        let Some(lent) = store.piece(&copying.place.pack, copying.place.len, copying.from) else {
          // A piece let go stops the unit, nothing copied, to be taken again once it is brought back.
          let waited = packed::Waited {
            pack: copying.place.pack,
            len: copying.place.len,
            offset: copying.from,
          };
          if store.away(&copying.place.pack, copying.place.len, copying.from)
            && self.stop_at(store, waited)
          {
            return Ok(Stage::Encode {
              blobs,
              part: Box::new(part),
              current: Some(current),
            });
          }
          return Err(StoreRefused::FragmentUnreadable);
        };
        let want = usize::try_from(copying.end.saturating_sub(copying.from)).unwrap_or(usize::MAX);
        let piece = lent
          .get(..want.min(lent.len()).min(window))
          .unwrap_or_default();
        if piece.is_empty() {
          return Err(StoreRefused::FragmentUnreadable);
        }
        part.bytes.extend_from_slice(piece);
        copying.crc.update(piece);
        copying.from = copying.from.saturating_add(to_u64(piece.len()));
        self.counters.most_bytes_a_unit = self.counters.most_bytes_a_unit.max(to_u64(piece.len()));
        if copying.from < copying.end {
          Some(current)
        } else {
          let Current::Copy(copying) = *current else {
            return Err(StoreRefused::FragmentUnreadable);
          };
          let Copying {
            mut placed,
            object,
            crc,
            ..
          } = *copying;
          placed.crc = crc.finalize();
          if let Some(object) = object {
            let learning = repository.learning(exchange);
            part
              .encoder
              .remember(object, (placed.offset, placed.depth), &learning);
          }
          part.place(placed);
          self.counters.objects = self.counters.objects.saturating_add(1);
          None
        }
      }
      Current::Put(ref mut putting) => {
        let mut learning = repository.learning(exchange);
        let bytes = putting.unit_bytes(&exchange.rates, quantum_ns, window);
        if !putting.sampling() {
          self.counters.most_put_bytes = self.counters.most_put_bytes.max(to_u64(bytes));
        }
        let put = part
          .encoder
          .step(putting, &mut learning, bytes)
          .map_err(StoreRefused::Encoding)?;
        self.counters.most_bytes_a_unit = self.counters.most_bytes_a_unit.max(to_u64(bytes));
        match put {
          Put::More(bytes) => {
            part.bytes.extend_from_slice(&bytes);
            Some(current)
          }
          Put::Done(bytes, record) => {
            part.bytes.extend_from_slice(&bytes);
            part.place(record);
            self.counters.objects = self.counters.objects.saturating_add(1);
            None
          }
        }
      }
    };
    Ok(Stage::Encode {
      blobs,
      part: Box::new(part),
      current: next,
    })
  }

  /// `object`'s pushed entry, read whole, to be sent on as it lies (A-859, git's pack reuse), a whole one only where
  /// encoding it `tries` no delta: its header written at `at`
  /// and its data to copy, once its check holds (a whole entry's data inflates to the object its name says; a delta's,
  /// applied to its base, to the object's bytes, which its name says), and where the client reads its base: a delta's
  /// base an earlier entry of this part (an offset delta, its chain within the deepest any push carried) or an object of
  /// an earlier version (a reference delta, beginning a chain). None to encode it instead: no pushed entry, a base this
  /// encoding places later, or a check that failed, counted [`EncodingCounters::reuse_refused`].
  fn reused(
    &mut self,
    repository: &Repository,
    store: &ContentStore,
    (part, object, at): (&Part, &Reading, u64),
  ) -> Option<(Vec<u8>, Copying)> {
    let StoredAt::InPack(place) = object.stored else {
      self.counters.reuse_no_entry = self.counters.reuse_no_entry.saturating_add(1);
      return None;
    };
    let mut scratch = Vec::new();
    let Ok(raw) = packed::raw_entry(
      store,
      (place, repository.format()),
      (&mut scratch, repository.largest_object().max(object.len)),
    ) else {
      self.counters.reuse_unread = self.counters.reuse_unread.saturating_add(1);
      return None;
    };
    let base_id = match raw.representation {
      Representation::Whole(_) => None,
      Representation::OfsDelta { base } => {
        let Some(base) = self.offsets.get(&(place.pack, base)) else {
          self.counters.reuse_unread = self.counters.reuse_unread.saturating_add(1);
          return None;
        };
        Some(*base)
      }
      Representation::RefDelta { base } => Some(base),
    };
    let (representation, depth, stored, base) =
      self.placement(repository, store, (part, object.kind, base_id))?;
    if !entry_checked(repository, store, (object, &raw, &scratch, base_id)) {
      self.counters.reuse_refused = self.counters.reuse_refused.saturating_add(1);
      return None;
    }
    let mut header = Vec::new();
    entry::write(
      entry::Header {
        representation,
        size: raw.size,
      },
      at,
      &mut header,
    )
    .ok()?;
    if stored == Stored::Whole {
      self.counters.reused_whole = self.counters.reused_whole.saturating_add(1);
    } else {
      self.counters.reused_deltas = self.counters.reused_deltas.saturating_add(1);
    }
    let copying = copying_of(object, (place, &raw), (at, depth, stored, base), &header);
    Some((header, copying))
  }

  /// `id`'s pushed entry sent on as it lies by what its row keeps of it (A-864, `rows::PackedRow`; git's
  /// `write_reuse_object`): its header read, the CRC32 of its raw bytes checked against the kept one, a delta's base by
  /// the position its row names, placed as [`Encoding::placement`] places one; its header written at `at` and its data to
  /// copy, no object read. None where its CRC32 misses (counted `reuse_crc_missed`), its header cannot be read, or its
  /// base is not placed: the object is then read and judged as before.
  fn reused_as_kept(
    &mut self,
    repository: &Repository,
    store: &ContentStore,
    (part, (id, position, kind), place, packed): (
      &Part,
      (ObjectId, u64, Kind),
      InPack,
      rows::PackedRow,
    ),
    at: u64,
  ) -> Result<Option<(Vec<u8>, Copying)>, Stopped> {
    let room = repository.largest_object();
    let (header, data) = match packed::entry_header(store, (place, repository.format()), room) {
      Ok(found) => found,
      Err(refused) => {
        self.stopped_by(store, &refused)?;
        return Ok(None);
      }
    };
    let crc = match packed::raw_crc(store, place, packed.len) {
      Ok(crc) => Some(crc),
      Err(refused) => {
        self.stopped_by(store, &refused)?;
        None
      }
    };
    if crc != Some(packed.crc) {
      self.counters.reuse_crc_missed = self.counters.reuse_crc_missed.saturating_add(1);
      return Ok(None);
    }
    Ok(self.kept_copy(
      repository,
      store,
      (part, (id, position, kind), place, packed),
      (header, data, at),
    ))
  }

  /// [`Encoding::reused_as_kept`]'s entry, its header read and its CRC32 held: placed, its header written at `at`, and
  /// its data to copy; none where its base is not placed.
  fn kept_copy(
    &mut self,
    repository: &Repository,
    store: &ContentStore,
    (part, (id, position, kind), place, packed): (
      &Part,
      (ObjectId, u64, Kind),
      InPack,
      rows::PackedRow,
    ),
    (header, data, at): (entry::Header, u64, u64),
  ) -> Option<(Vec<u8>, Copying)> {
    let base_id = match (header.representation, packed.base) {
      (Representation::Whole(whole), None) if whole == kind => None,
      (Representation::OfsDelta { .. } | Representation::RefDelta { .. }, Some(base)) => {
        Some(repository.id_at(store, base)?)
      }
      _ => return None,
    };
    let (representation, depth, stored, base) =
      self.placement(repository, store, (part, kind, base_id))?;
    let mut written = Vec::new();
    entry::write(
      entry::Header {
        representation,
        size: header.size,
      },
      at,
      &mut written,
    )
    .ok()?;
    if stored == Stored::Whole {
      self.counters.reused_whole = self.counters.reused_whole.saturating_add(1);
    } else {
      self.counters.reused_deltas = self.counters.reused_deltas.saturating_add(1);
    }
    let mut crc = crc32fast::Hasher::new();
    crc.update(&written);
    let copying = Copying {
      placed: Placed {
        id,
        position,
        kind,
        offset: at,
        crc: 0,
        depth,
        stored,
        base,
      },
      object: None,
      place,
      from: data,
      end: place.offset.saturating_add(packed.len),
      crc,
    };
    Some((written, copying))
  }

  /// How a reused entry of `kind` names its base `base_id` (A-859): whole; an offset delta to an earlier entry of this
  /// part, its chain within the deepest any push carried; or a reference delta to an object of an earlier version,
  /// beginning a chain. Its representation, its chain's depth, how it holds its object and its base's record; none where
  /// the base is one this encoding places later (it is encoded, so no delta precedes its base) or the chain would be
  /// past the depth.
  fn placement(
    &self,
    repository: &Repository,
    store: &ContentStore,
    (part, kind, base_id): (&Part, Kind, Option<ObjectId>),
  ) -> Option<(Representation, u64, Stored, Option<u64>)> {
    let Some(base) = base_id else {
      return Some((Representation::Whole(kind), 0, Stored::Whole, None));
    };
    if let Some(placed) = part.placed_of(&base) {
      let depth = placed.depth.saturating_add(1);
      if depth > repository.learned_depth() {
        return None;
      }
      let base_at = placed.offset.saturating_add(PACK_HEADER_LEN);
      return Some((
        Representation::OfsDelta { base: base_at },
        depth,
        Stored::Offset,
        Some(base_at),
      ));
    }
    let (row, _) = repository.indexed(store, &base)?;
    if self.positions.contains(row.position) {
      return None;
    }
    Some((
      Representation::RefDelta { base },
      1,
      Stored::Reference,
      Some(row.position),
    ))
  }

  /// The base to place before the object `row` names (A-859, git's `write_one`): where its pushed entry is a delta
  /// against an object still in the part's order, not yet placed, within the deepest chain any push carried, that
  /// base taken out of the order to go first. Read from the base its row keeps (A-864); none to go on with the object.
  fn base_first(
    &mut self,
    repository: &Repository,
    store: &ContentStore,
    (part, blobs): (&Part, bool),
    (row, _content): (&ObjectRow, ContentId),
  ) -> Option<Key> {
    if to_u64(self.ahead.len()) >= repository.learned_depth() {
      return None;
    }
    // A-864: a delta's base by the position its row keeps, of any version, no header read.
    let base = repository.id_at(store, row.pack?.base?)?;
    if part.placed_of(&base).is_some() {
      return None;
    }
    let (base_row, _) = repository.indexed(store, &base)?;
    let key = (
      base_row.kind,
      Reverse(base_row.len),
      base,
      base_row.position,
    );
    let order = if blobs {
      &mut self.blobs
    } else {
      &mut self.others
    };
    order.remove(&key).then_some(key)
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
    let Some(key) = self.ahead.pop().or_else(|| order.pop_first()) else {
      return Ok(self.part_ended(blobs, part));
    };
    let (_, _, id, position) = key;
    let (row, content) = repository
      .indexed(store, &id)
      .ok_or(StoreRefused::Missing { id })?;
    if let Some(base) = self.base_first(repository, store, (&part, blobs), (&row, content)) {
      self.ahead.push(key);
      self.ahead.push(base);
      return Ok(Stage::Encode {
        blobs,
        part: Box::new(part),
        current: None,
      });
    }
    // A-864: an entry its row keeps is sent on by its CRC32, its object never read.
    if let Some(packed) = row.pack {
      let at = PACK_HEADER_LEN.saturating_add(to_u64(part.bytes.len()));
      let place = repository.stored_of(&row, content);
      if let StoredAt::InPack(place) = place {
        match self.reused_as_kept(
          repository,
          store,
          (&part, (id, position, row.kind), place, packed),
          at,
        ) {
          // One whose entry's read met a let-go window stops before anything of it is kept, to be taken again.
          Err(Stopped) => {
            self.ahead.push(key);
            return Ok(Stage::Encode {
              blobs,
              part: Box::new(part),
              current: None,
            });
          }
          Ok(Some((header, copying))) => {
            let mut part = part;
            part.bytes.extend_from_slice(&header);
            return Ok(Stage::Encode {
              blobs,
              part: Box::new(part),
              current: Some(Box::new(Current::Copy(Box::new(copying)))),
            });
          }
          Ok(None) => {}
        }
      }
    }
    let object = Reading {
      id,
      position,
      kind: row.kind,
      len: row.len,
      stored: repository.stored_of(&row, content),
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
          stored: repository.stored_of(&row, content),
          bytes: Vec::new(),
        })
      }
      None => None,
    };
    Ok(Stage::Encode {
      blobs,
      part: Box::new(part),
      current: Some(Box::new(Current::Read {
        object: Box::new(object),
        named: named.map(Box::new),
      })),
    })
  }

  /// A part with nothing left to encode: the blobs' part next, or both parts kept.
  fn part_ended(&mut self, blobs: bool, part: Part) -> Stage {
    let encoded = (part.bytes, part.placed);
    if !blobs {
      self.objects = Some(encoded);
      return Stage::Encode {
        blobs: true,
        part: Box::new(Part::new()),
        current: None,
      };
    }
    Stage::Keep {
      objects: self.objects.take().unwrap_or_default(),
      blobs: encoded,
      putting_blobs: false,
      next: 0,
      writing: Box::default(),
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
        give_back(store, *writing, objects_root);
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
        give_back(store, *writing, objects_root);
        return Err(refused.into());
      }
    };
    if !putting_blobs {
      return Ok(Stage::Keep {
        objects,
        blobs,
        putting_blobs: true,
        next: 0,
        writing: Box::default(),
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

/// An encoding unit stopped at a let-go window (A-864 piece 2, [`Encoding::waiting`]).
struct Stopped;
