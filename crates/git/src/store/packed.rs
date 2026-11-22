//! An object read in place from a held pack (A-722's memory per push, main's ruling B, part 1; Ada's
//! MEMORY SIZING rule of 2026-11-20: "if we don't need to allocate or can architect things in such a way that we do not
//! allocate, we do not allocate").
//!
//! **Why.** A push of git/git's v1.5.0 (a 9.4 MiB pack, 34,296 objects) left the content store holding 309 MiB, every
//! object inflated (git counts 304.6 MiB: blobs 231.0, trees 69.1, commits 4.5), 33 times the pack (measured on
//! adas-mac-studio.local-2, release, 2026-11-20). The pack itself holds every object, compressed and as the client's
//! deltas; held once, an object is read from it where it lies.
//!
//! **One seam for where a pack is held** ([`PackSource`]; Ada's storage ruling of 2026-11-20: an in-flight push's pack
//! lives in RAM, a landed repository's content on disk, in w-store's store). A source lends a pack's bytes from an offset
//! a piece at a time; the content store is the first ([`ContentStore`]'s windows, a pushed pack held once by identity),
//! the disk store the next, with no second path here.
//!
//! **What.** [`read`] answers the object whose entry begins at a place in a held pack ([`InPack`]): its header read
//! across the source's pieces ([`entry::read`]); for a delta, its base followed down to a whole object, an OFS_DELTA's
//! base an earlier entry of the same pack and a REF_DELTA's the object its caller locates ([`Bases`]: an entry of a
//! pack, or an object it hands over whole); then the base inflated and each delta inflated and applied
//! going back up (`delta::apply_to`), into buffers the caller keeps and reuses ([`Scratch`]), so a read allocates nothing
//! once its scratch has grown to the largest object it read. Each entry's data is inflated from the windows it lies in,
//! a window lent at a time, never copied whole.
//!
//! **Objects read lately** ([`Recent`]; git's delta base cache, `core.deltaBaseCacheLimit`). A chain read whole for
//! each of its members costs its depth for each (git/git's v1.5.0 pack: 23,653 deltas, chains 12.6 deep on average,
//! 297,700 deltas applied where 23,653 suffice). A read stops at the first entry of its chain the scratch keeps, and
//! keeps each object it builds, the oldest given back past a budget its caller learns from the work (a push's: its
//! pack's length, so what it keeps is held to what it was sent).
//!
//! **Bounds (§4.9).** Every size a header names is held to the caller's room before any buffer grows to it. An
//! OFS_DELTA's base lies before its entry ([`entry::read`] refuses one that does not), so a chain within one pack ends; a
//! REF_DELTA's base that leads back to an entry already on the chain is refused (`Cycle`), so no chain loops. A stream
//! that inflates to other than its header's size, or bytes no inflater takes, are refused where they begin.

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};

use flate2::{Decompress, FlushDecompress, Status};
use slates_vcs::{ContentId, ContentStore};

use crate::admission::Admission;
use crate::delta;
use crate::entry::{self, Representation};
use crate::object_name::{Kind, ObjectFormat, ObjectId};

/// Where a pack is held (the module docs, "One seam"): its bytes lent a piece at a time.
pub trait PackSource {
  /// The pack `pack` (of `len` bytes) from `offset` on, as far as one lent piece reaches; none where it is not held or
  /// `offset` is past its end.
  fn piece(&self, pack: &ContentId, len: u64, offset: u64) -> Option<Cow<'_, [u8]>>;
}

/// A pack the content store holds, by its identity: a piece is the window `offset` lies in, from `offset` on.
impl PackSource for ContentStore {
  fn piece(&self, pack: &ContentId, len: u64, offset: u64) -> Option<Cow<'_, [u8]>> {
    let window = u64::try_from(self.window())
      .ok()
      .filter(|window| *window > 0)?;
    if offset >= len {
      return None;
    }
    let within = usize::try_from(offset.checked_rem(window)?).ok()?;
    match self.window_at(pack, len, offset.checked_div(window)?)? {
      Cow::Borrowed(lent) => lent.get(within..).map(Cow::Borrowed),
      Cow::Owned(copied) => copied.get(within..).map(|rest| Cow::Owned(rest.to_vec())),
    }
  }
}

/// Where an entry begins in a held pack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InPack {
  /// The pack's identity: its root content.
  pub pack: ContentId,
  /// The pack's length.
  pub len: u64,
  /// Where the entry begins in it.
  pub offset: u64,
}

/// Where a REF_DELTA's base is: an entry of a held pack, or an object its locator wrote whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Base {
  /// An entry of a pack.
  InPack(InPack),
  /// An object written whole into the buffer [`Bases::locate`] was handed: its type.
  Whole(Kind),
}

/// Where a REF_DELTA's base is found by its name: the repository's index, and the push's own objects.
pub trait Bases {
  /// The object `id` names, where it is held: an entry of a pack, or, for one held whole outside every pack, its bytes
  /// written into `whole` (a buffer of the reader's scratch, reused); none for one held nowhere or past `room`.
  fn locate(&self, id: &ObjectId, whole: &mut Vec<u8>, room: u64) -> Option<Base>;
}

/// Why an object could not be read from its pack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackedRefused {
  /// The store does not hold the bytes at this offset of the pack.
  Unread {
    /// Where.
    at: u64,
  },
  /// An entry's header is not one (`entry::read`'s refusal).
  Entry(entry::Refusal),
  /// An entry's data does not inflate.
  Inflate {
    /// Where its entry begins.
    at: u64,
  },
  /// An entry's data inflates to other than its header's size.
  Size {
    /// Where its entry begins.
    at: u64,
  },
  /// A size past the caller's room.
  PastRoom {
    /// The size.
    size: u64,
  },
  /// A delta does not apply to its base.
  Delta(delta::Refusal),
  /// A REF_DELTA's base is held nowhere.
  MissingBase {
    /// Its name.
    id: ObjectId,
  },
  /// A REF_DELTA's base leads back to an entry already on the chain.
  Cycle {
    /// Where the entry it led back to begins.
    at: u64,
  },
}

/// One delta on a chain: where its data begins in its pack, and the size it inflates to.
#[derive(Clone, Copy, Debug)]
struct Link {
  /// Its entry.
  entry: InPack,
  /// Where its compressed data begins.
  data: u64,
  /// The bytes it inflates to.
  size: u64,
}

/// The buffers a reader keeps between reads (the module docs): the object built so far, the next one, a delta's data,
/// the chain followed, and an entry's header's bytes. Each grows to the largest it held and is reused; [`Scratch::shrink`]
/// gives back what the work no longer needs.
#[derive(Debug, Default)]
pub struct Scratch {
  /// The object built so far, up the chain.
  built: Vec<u8>,
  /// The next one, a delta applied to `built`.
  next: Vec<u8>,
  /// A delta's inflated data.
  delta: Vec<u8>,
  /// The deltas followed, from the object read down to the last before its base.
  chain: Vec<Link>,
  /// An entry's header's bytes, gathered across windows.
  header: Vec<u8>,
  /// The objects read lately (module docs, "Objects read lately").
  recent: Recent,
}

/// Objects read lately, by where their entries begin (module docs, "Objects read lately"): none kept past `budget`
/// bytes, the oldest given back first. Their bytes lie in chunks of the mapped allocation's floor, laid end to end, the
/// oldest chunk given back whole (goal 93, main 01a20182's ruling of 2026-11-22 04:56): kept as a buffer each, a git/git
/// push's 164,488 objects passed through the system allocator one by one, freed oldest first as newer came, and it kept
/// 550-600 MB of them after the push. Each chunk takes the mapped path, so its going gives its pages back to the OS; an
/// object as long as a chunk is a chunk of its own, taken whole. Nothing grows by copying: a chunk is reserved once at
/// its length, and a budget that grows (a push's, as its pack arrives) adds chunks.
#[derive(Debug, Default)]
pub struct Recent {
  /// Where each object kept lies: its type and its chunk's range. A hash table, which once grown to the budget's objects
  /// allocates nothing as they come and go, where a B-tree split and gave back a node every few (142 bytes an object).
  objects: HashMap<(ContentId, u64), (Kind, Lent)>,
  /// The chunks, oldest first.
  chunks: VecDeque<Chunk>,
  /// The objects kept, oldest first, each with its chunk's number: one queue, which stops growing once the budget is
  /// reached, where a list a chunk was made and given back with each.
  order: VecDeque<((ContentId, u64), u64)>,
  /// The number of the oldest chunk: every chunk ever laid has the next number, so an object's chunk gone is a miss by
  /// its number, never another's bytes.
  first: u64,
  /// The bytes of the objects kept.
  bytes: usize,
  /// The bytes the chunks hold reserved.
  held: usize,
  /// The most bytes the chunks hold: zero keeps none.
  budget: usize,
  /// What its owner's content store is charged for the chunks ([`Recent::true_up`]), so the store's room counts them.
  charged: u64,
}

/// One chunk of [`Recent`]'s: its bytes, laid end to end within the room it was reserved with.
#[derive(Debug)]
struct Chunk {
  /// The objects' bytes.
  bytes: Vec<u8>,
  /// Whether it holds one object taken whole, which nothing else is laid after.
  whole: bool,
}

/// Where a kept object's bytes lie: its chunk's number, and its range there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Lent {
  /// The chunk's number ([`Recent::first`]).
  chunk: u64,
  /// Where its bytes begin in the chunk.
  at: usize,
  /// Their length.
  len: usize,
}

impl Recent {
  /// The object kept for the entry at `place`; none when none is, or its chunk has gone.
  fn get(&self, place: InPack) -> Option<(Kind, &[u8])> {
    let (kind, lent) = self.objects.get(&(place.pack, place.offset))?;
    let index = usize::try_from(lent.chunk.checked_sub(self.first)?).ok()?;
    let bytes = self
      .chunks
      .get(index)?
      .bytes
      .get(lent.at..lent.at.checked_add(lent.len)?)?;
    Some((*kind, bytes))
  }

  /// Keeps `bytes`, the object of type `kind` whose entry begins at `place`: taken whole as a chunk of its own when it is
  /// a chunk long or more, else copied into the newest chunk and its buffer given back at once. None kept where it alone
  /// passes the budget or the entry's object is kept already.
  pub fn keep(&mut self, place: InPack, kind: Kind, bytes: Vec<u8>) {
    if !self.takes(place, bytes.len()) {
      return;
    }
    if bytes.len() >= self.chunk_len() {
      self.keep_whole(place, kind, bytes);
    } else {
      self.lay(place, kind, &bytes);
    }
  }

  /// Keeps a copy of `bytes` (as [`Recent::keep`]).
  fn keep_copy(&mut self, place: InPack, kind: Kind, bytes: &[u8]) {
    if !self.takes(place, bytes.len()) {
      return;
    }
    if bytes.len() >= self.chunk_len() {
      let mut whole = Vec::new();
      if whole.try_reserve_exact(bytes.len()).is_err() {
        return;
      }
      whole.extend_from_slice(bytes);
      self.keep_whole(place, kind, whole);
    } else {
      self.lay(place, kind, bytes);
    }
  }

  /// Whether an object `len` bytes long whose entry begins at `place` is kept.
  fn takes(&self, place: InPack, len: usize) -> bool {
    len <= self.budget && !self.objects.contains_key(&(place.pack, place.offset))
  }

  /// A chunk's length: the mapped allocation's floor, so each is given back whole when it goes (`slates_mem::large`), or
  /// the budget where that is shorter.
  fn chunk_len(&self) -> usize {
    slates_mem::large::floor().min(self.budget).max(1)
  }

  /// Copies `bytes` to the end of the newest chunk, or of a new one where it has no room.
  fn lay(&mut self, place: InPack, kind: Kind, bytes: &[u8]) {
    let room = self
      .chunks
      .back()
      .filter(|chunk| !chunk.whole)
      .map_or(0, |chunk| {
        chunk.bytes.capacity().saturating_sub(chunk.bytes.len())
      });
    if room < bytes.len() {
      let len = self.chunk_len();
      self.make_room(len);
      let mut fresh = Vec::new();
      if fresh.try_reserve_exact(len).is_err() {
        return;
      }
      self.push_chunk(fresh, false);
    }
    let number = self.newest();
    let Some(chunk) = self.chunks.back_mut() else {
      return;
    };
    let at = chunk.bytes.len();
    chunk.bytes.extend_from_slice(bytes);
    self.record(
      place,
      kind,
      Lent {
        chunk: number,
        at,
        len: bytes.len(),
      },
    );
  }

  /// Keeps `bytes` whole as a chunk of its own.
  fn keep_whole(&mut self, place: InPack, kind: Kind, bytes: Vec<u8>) {
    self.make_room(bytes.capacity());
    let len = bytes.len();
    self.push_chunk(bytes, true);
    let number = self.newest();
    self.record(
      place,
      kind,
      Lent {
        chunk: number,
        at: 0,
        len,
      },
    );
  }

  /// Adds `bytes` as the newest chunk.
  fn push_chunk(&mut self, bytes: Vec<u8>, whole: bool) {
    self.held = self.held.saturating_add(bytes.capacity());
    self.chunks.push_back(Chunk { bytes, whole });
  }

  /// The newest chunk's number.
  fn newest(&self) -> u64 {
    self
      .first
      .saturating_add(u64::try_from(self.chunks.len()).unwrap_or(u64::MAX))
      .saturating_sub(1)
  }

  /// Names the object laid at `lent` in the newest chunk.
  fn record(&mut self, place: InPack, kind: Kind, lent: Lent) {
    let key = (place.pack, place.offset);
    self.order.push_back((key, lent.chunk));
    self.bytes = self.bytes.saturating_add(lent.len);
    self.objects.insert(key, (kind, lent));
  }

  /// Gives back the oldest chunks, whole, until `len` bytes more fit the budget, forgetting the objects laid in them.
  fn make_room(&mut self, len: usize) {
    while self.held.saturating_add(len) > self.budget && !self.chunks.is_empty() {
      self.drop_oldest();
    }
  }

  /// Gives back the oldest chunk whole, forgetting the objects laid in it.
  fn drop_oldest(&mut self) {
    let Some(oldest) = self.chunks.pop_front() else {
      return;
    };
    let number = self.first;
    self.first = self.first.saturating_add(1);
    self.held = self.held.saturating_sub(oldest.bytes.capacity());
    while let Some(&(key, chunk)) = self.order.front()
      && chunk == number
    {
      self.order.pop_front();
      if let Some((_, lent)) = self.objects.get(&key)
        && lent.chunk == number
      {
        self.bytes = self.bytes.saturating_sub(lent.len);
        self.objects.remove(&key);
      }
    }
  }

  /// The bytes kept.
  pub fn bytes(&self) -> usize {
    self.bytes
  }

  /// Gives back every object kept, its budget kept for the reads after (A-860: a repository gone idle holds none): every
  /// chunk given back whole, so the pages they took go back to the OS, and the index's own room with them. The chunks'
  /// numbers go on from past the last, so nothing named before is found in a chunk laid after.
  pub fn let_go(&mut self) {
    self.first = self
      .first
      .saturating_add(u64::try_from(self.chunks.len()).unwrap_or(u64::MAX));
    self.chunks = VecDeque::new();
    self.objects = HashMap::new();
    self.order = VecDeque::new();
    self.bytes = 0;
    self.held = 0;
  }

  /// Charges `store` for the chunks held now past what it charged before, or gives back what they no longer hold (main
  /// 01a20182's ruling of 2026-11-22 05:17: an uncharged holder makes the admission's numbers lie). Where the store's room
  /// cannot take the charge, the oldest chunks are given back until it can: the store's room bounds what is kept, as the
  /// budget does. Run by its owner wherever it holds the store mutably (a push's admission turns, a repository's encoding
  /// turns); a read between two only lays chunks the next charges.
  pub fn true_up(&mut self, store: &mut ContentStore) {
    let held = u64::try_from(self.held).unwrap_or(u64::MAX);
    if held < self.charged {
      if store.discharge(self.charged.saturating_sub(held)).is_ok() {
        self.charged = held;
      }
      return;
    }
    // What the chunks may hold: what they are charged now and the store's room, less what the store is past its budget
    // (a budget lowered under what is charged).
    let past = store.used().saturating_sub(store.budget());
    let fits = self
      .charged
      .saturating_add(store.room())
      .saturating_sub(past);
    let fits = usize::try_from(fits).unwrap_or(usize::MAX);
    while self.held > fits && !self.chunks.is_empty() {
      self.drop_oldest();
    }
    let held = u64::try_from(self.held).unwrap_or(u64::MAX);
    if held > self.charged {
      if store.charge(held.saturating_sub(self.charged)).is_ok() {
        self.charged = held;
      }
    } else if store.discharge(self.charged.saturating_sub(held)).is_ok() {
      self.charged = held;
    }
  }

  /// Gives back every chunk ([`Recent::let_go`]) and everything charged for them to `store`: what its owner runs when it
  /// ends (a push landed or abandoned) or goes idle.
  pub fn release(&mut self, store: &mut ContentStore) {
    self.let_go();
    if store.discharge(self.charged).is_ok() {
      self.charged = 0;
    }
  }

  /// What its owner's store is charged for it now.
  pub fn charged(&self) -> u64 {
    self.charged
  }

  /// Holds what is kept to `budget` bytes from now on, the oldest given back to fit.
  pub fn set_budget(&mut self, budget: usize) {
    self.budget = budget;
    self.make_room(0);
  }
}

impl Scratch {
  /// The objects read lately (module docs, "Objects read lately").
  pub fn recent(&mut self) -> &mut Recent {
    &mut self.recent
  }

  /// What it holds now (the memory status's gauges): its buffers' room, and the bytes of the objects read lately.
  pub fn held(&self) -> (u64, u64) {
    let room = [&self.built, &self.next, &self.delta, &self.header]
      .iter()
      .map(|buffer| buffer.capacity())
      .fold(0usize, usize::saturating_add);
    (
      u64::try_from(room).unwrap_or(u64::MAX),
      u64::try_from(self.recent.bytes).unwrap_or(u64::MAX),
    )
  }

  /// Gives back every buffer's room past `keep` bytes: the work's working set, learned by its caller (MEMORY SIZING:
  /// nothing stays resident because a past object was large).
  pub fn shrink(&mut self, keep: usize) {
    for buffer in [&mut self.built, &mut self.next, &mut self.delta] {
      buffer.clear();
      buffer.shrink_to(keep);
    }
    self.chain.clear();
    self.chain.shrink_to_fit();
    self.header.clear();
  }
}

/// Reads the object whose entry begins at `place` (the module docs) into `out`, its buffer handed over whole rather than
/// copied: its type. Every size held to `room`.
pub fn read(
  store: &dyn PackSource,
  (place, format): (InPack, ObjectFormat),
  bases: &dyn Bases,
  (scratch, room): (&mut Scratch, u64),
  out: &mut Vec<u8>,
) -> Result<Kind, PackedRefused> {
  scratch.chain.clear();
  let mut at = place;
  let (kind, base) = loop {
    if let Some((kind, kept)) = scratch.recent.get(at) {
      scratch.built.clear();
      scratch.built.extend_from_slice(kept);
      break (kind, None);
    }
    let (header, data) = header_at(store, at, format, (&mut scratch.header, room))?;
    match header.representation {
      Representation::Whole(kind) => {
        break (
          kind,
          Some(Link {
            entry: at,
            data,
            size: header.size,
          }),
        );
      }
      Representation::OfsDelta { base } => {
        scratch.chain.push(Link {
          entry: at,
          data,
          size: header.size,
        });
        at = InPack { offset: base, ..at };
      }
      Representation::RefDelta { base } => {
        scratch.chain.push(Link {
          entry: at,
          data,
          size: header.size,
        });
        match bases.locate(&base, &mut scratch.built, room) {
          Some(Base::InPack(entry)) => {
            if scratch.chain.iter().any(|link| link.entry == entry) {
              return Err(PackedRefused::Cycle { at: entry.offset });
            }
            at = entry;
          }
          Some(Base::Whole(kind)) => break (kind, None),
          None => return Err(PackedRefused::MissingBase { id: base }),
        }
      }
    }
  };
  // A base whole in a pack is inflated now; one held whole outside every pack is in `built` already.
  if let Some(link) = base {
    inflate_into(store, link, room, &mut scratch.built)?;
    scratch.recent.keep_copy(link.entry, kind, &scratch.built);
  }
  let Scratch {
    built,
    next,
    delta: delta_bytes,
    chain,
    recent,
    ..
  } = scratch;
  for link in chain.iter().rev() {
    inflate_into(store, *link, room, delta_bytes)?;
    next.clear();
    delta::apply_to(built, delta_bytes, next).map_err(PackedRefused::Delta)?;
    core::mem::swap(built, next);
    recent.keep_copy(link.entry, kind, built);
  }
  core::mem::swap(out, built);
  Ok(kind)
}

/// The header of the entry at `place`, and where its data begins: its bytes gathered across the windows they lie in into
/// `header`, at most the longest a header can be.
fn header_at(
  store: &dyn PackSource,
  place: InPack,
  format: ObjectFormat,
  (header, room): (&mut Vec<u8>, u64),
) -> Result<(entry::Header, u64), PackedRefused> {
  header.clear();
  let longest = entry::longest_header(format);
  each_window_from(store, place, |piece| {
    let want = longest.saturating_sub(header.len());
    header.extend_from_slice(piece.get(..want).unwrap_or(piece));
    header.len() < longest
  })?;
  let mut admission = Admission::new(room);
  let read =
    entry::read(header, place.offset, format, &mut admission).map_err(|refused| match refused {
      entry::Refusal::PastAdmission(_) => PackedRefused::PastRoom { size: room },
      other => PackedRefused::Entry(other),
    })?;
  let (found, len) = read.ok_or(PackedRefused::Unread { at: place.offset })?;
  Ok((
    found,
    place
      .offset
      .saturating_add(u64::try_from(len).unwrap_or(u64::MAX)),
  ))
}

/// Hands `each` the pack's bytes from `place`'s offset on, a piece lent at a time, until it answers `false` or the pack
/// ends. Refused where a piece is not held.
fn each_window_from(
  store: &dyn PackSource,
  place: InPack,
  mut each: impl FnMut(&[u8]) -> bool,
) -> Result<(), PackedRefused> {
  let mut at = place.offset;
  while at < place.len {
    let lent = store
      .piece(&place.pack, place.len, at)
      .ok_or(PackedRefused::Unread { at })?;
    if lent.is_empty() {
      return Err(PackedRefused::Unread { at });
    }
    if !each(&lent) {
      return Ok(());
    }
    at = at.saturating_add(u64::try_from(lent.len()).unwrap_or(u64::MAX));
  }
  Ok(())
}

/// Inflates `link`'s data into `out`, exactly its size: where its compressed data ends in the pack.
fn inflate_into(
  store: &dyn PackSource,
  link: Link,
  room: u64,
  out: &mut Vec<u8>,
) -> Result<u64, PackedRefused> {
  if link.size > room {
    return Err(PackedRefused::PastRoom { size: link.size });
  }
  let size = usize::try_from(link.size).map_err(|_| PackedRefused::PastRoom { size: link.size })?;
  out.clear();
  out.resize(size, 0);
  let at = link.entry.offset;
  let mut inflater = Decompress::new(true);
  let (mut filled, mut ended, mut failed) = (0usize, false, false);
  each_window_from(
    store,
    InPack {
      offset: link.data,
      ..link.entry
    },
    |piece| {
      let mut input = piece;
      loop {
        let (before_in, before_out) = (inflater.total_in(), inflater.total_out());
        let mut spare = [0u8; 1];
        let room: &mut [u8] = match out.get_mut(filled..) {
          Some(rest) if !rest.is_empty() => rest,
          _ => &mut spare,
        };
        let status = inflater.decompress(input, room, FlushDecompress::None);
        let wrote =
          usize::try_from(inflater.total_out().saturating_sub(before_out)).unwrap_or(usize::MAX);
        let took =
          usize::try_from(inflater.total_in().saturating_sub(before_in)).unwrap_or(usize::MAX);
        filled = filled.saturating_add(wrote);
        input = input.get(took..).unwrap_or_default();
        match status {
          Ok(Status::StreamEnd) => {
            ended = true;
            return false;
          }
          Ok(_) if took == 0 && wrote == 0 => return true,
          Ok(_) if filled > size => {
            failed = true;
            return false;
          }
          Ok(_) => {}
          Err(_) => {
            failed = true;
            return false;
          }
        }
      }
    },
  )?;
  if failed {
    return Err(PackedRefused::Inflate { at });
  }
  if !ended || filled != size {
    return Err(PackedRefused::Size { at });
  }
  Ok(link.data.saturating_add(inflater.total_in()))
}

/// An entry of a held pack as it lies there (A-859, git's pack reuse): how it holds its object, the size its header
/// names, where its compressed data begins and where it ends, so the entry's data can be sent on as it was received.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawEntry {
  /// How it holds its object: whole of a type, or a delta against its base (an OFS_DELTA's base where that entry
  /// begins in the same pack, a REF_DELTA's by its name).
  pub representation: Representation,
  /// The size its header names: the object's, or the delta's.
  pub size: u64,
  /// Where its compressed data begins in the pack, past its header.
  pub data: u64,
  /// Where its compressed data ends: the next byte past it.
  pub end: u64,
}

/// The header of the entry at `place` in a held pack (A-859): how it holds its object, its base where a delta's, read
/// with nothing inflated, so a delta's base is found before its data is read. Refused as [`raw_entry`] refuses one.
pub fn entry_header(
  store: &dyn PackSource,
  (place, format): (InPack, ObjectFormat),
  room: u64,
) -> Result<entry::Header, PackedRefused> {
  let mut header = Vec::new();
  header_at(store, place, format, (&mut header, room)).map(|(found, _)| found)
}

/// The entry at `place` in a held pack as it lies there ([`RawEntry`], A-859): its header read, and its data inflated
/// once into `scratch` (its size, held to `room` first) to find where it ends and that it inflates whole to its header's
/// size. Refused as [`read`] refuses an entry it cannot take.
pub fn raw_entry(
  store: &dyn PackSource,
  (place, format): (InPack, ObjectFormat),
  (scratch, room): (&mut Vec<u8>, u64),
) -> Result<RawEntry, PackedRefused> {
  let mut header = Vec::new();
  let (found, data) = header_at(store, place, format, (&mut header, room))?;
  let end = inflate_into(
    store,
    Link {
      entry: place,
      data,
      size: found.size,
    },
    room,
    scratch,
  )?;
  Ok(RawEntry {
    representation: found.representation,
    size: found.size,
    data,
    end,
  })
}

#[cfg(test)]
mod recent_tests {
  use super::{InPack, Recent};
  use crate::object_name::Kind;
  use slates_vcs::ContentId;

  /// The entry at `offset` of one pack.
  fn at(offset: u64) -> InPack {
    InPack {
      pack: ContentId([1; 32]),
      len: u64::MAX,
      offset,
    }
  }

  /// The chunks' length these tests lay objects in.
  fn chunk() -> usize {
    slates_mem::large::floor()
  }

  /// Goal 93 (main 01a20182 04:56, rule 3: a range overwritten is a typed miss, never another's bytes). Do: with a budget
  /// of two chunks, keep objects of a quarter chunk each, each its own bytes, until five chunks' worth have been kept;
  /// then read each back. Expect: the newest read back byte for byte; the oldest, whose chunks went, found nowhere (none
  /// read from a chunk laid after); what is kept never past the budget.
  #[test]
  fn an_object_whose_chunk_went_is_a_miss_never_another_objects_bytes() {
    let mut recent = Recent::default();
    recent.set_budget(2 * chunk());
    let len = chunk() / 4;
    let objects = 20u64;
    for n in 0..objects {
      recent.keep(at(n), Kind::Blob, vec![u8::try_from(n).unwrap(); len]);
      assert!(
        recent.held <= 2 * chunk(),
        "held {} past the budget",
        recent.held
      );
    }
    for n in 0..objects {
      match recent.get(at(n)) {
        Some((kind, bytes)) => {
          assert_eq!(kind, Kind::Blob);
          assert!(
            bytes.iter().all(|byte| u64::from(*byte) == n),
            "object {n}'s bytes"
          );
          assert!(n >= objects - 8, "object {n} kept past its chunk's going");
        }
        None => assert!(n < objects - 4, "the newest object {n} was given back"),
      }
    }
  }

  /// Goal 93 (rule 2: a large object never forces a grow-and-copy). Do: with a budget of four chunks, keep a quarter
  /// chunk's object, then one of twice a chunk (taken whole), then one past the budget, then another small one. Expect: the
  /// large one read back, kept as a chunk of its own (two chunks of the ring: the small one's and its own, then the small
  /// one's after it); the one past the budget kept nowhere; nothing kept past the budget.
  #[test]
  fn an_object_a_chunk_long_is_its_own_chunk_and_one_past_the_budget_is_not_kept() {
    let mut recent = Recent::default();
    recent.set_budget(4 * chunk());
    recent.keep(at(0), Kind::Blob, vec![1; chunk() / 4]);
    recent.keep(at(1), Kind::Blob, vec![2; 2 * chunk()]);
    recent.keep(at(2), Kind::Blob, vec![3; 5 * chunk()]);
    recent.keep(at(3), Kind::Blob, vec![4; chunk() / 4]);
    assert!(
      recent
        .get(at(1))
        .is_some_and(|(_, bytes)| bytes.len() == 2 * chunk())
    );
    assert!(
      recent.get(at(2)).is_none(),
      "an object past the budget is not kept"
    );
    assert!(recent.get(at(3)).is_some());
    assert!(recent.chunks.get(1).is_some_and(|chunk| chunk.whole));
    assert!(recent.held <= 4 * chunk());
  }

  /// A-861's charge (main 01a20182, 2026-11-22 05:17: an uncharged holder makes the admission's numbers lie). Do: with a
  /// budget of four chunks over a store whose room is four and a half, keep objects until four chunks' worth and true
  /// up; then take the store's room down to one and a half chunks' worth of charge and true up again; then release.
  /// Expect: the store charged exactly the chunks held after each true-up, the second giving back the oldest chunks
  /// until the charge fits the room (two chunks kept); none charged and none held after the release.
  #[test]
  fn the_chunks_are_charged_on_the_stores_room_and_given_back_with_it() {
    let room = u64::try_from(9 * chunk() / 2).unwrap();
    let mut store = slates_vcs::ContentStore::new(room, 4096).unwrap();
    let mut recent = Recent::default();
    recent.set_budget(4 * chunk());
    for n in 0..16 {
      recent.keep(at(n), Kind::Blob, vec![1; chunk() / 4]);
    }
    recent.true_up(&mut store);
    assert_eq!(recent.charged(), u64::try_from(recent.held).unwrap());
    assert_eq!(store.charged(), recent.charged());
    store.set_budget(
      recent
        .charged()
        .saturating_sub(u64::try_from(chunk() * 2).unwrap())
        + u64::try_from(chunk() / 2).unwrap(),
    );
    recent.true_up(&mut store);
    assert_eq!(store.charged(), recent.charged());
    assert!(
      store.charged() <= store.budget(),
      "charged {} past {}",
      store.charged(),
      store.budget()
    );
    assert_eq!(
      recent.chunks.len(),
      2,
      "the oldest given back to fit the room"
    );
    recent.release(&mut store);
    assert_eq!((store.charged(), recent.charged(), recent.held), (0, 0, 0));
  }

  /// A-860 with the chunks (w-git's let_go): do: keep objects, let go, read one back, keep one more. Expect: nothing held
  /// or kept after the let-go (no chunk, no index room), the old object a miss, and the new one kept under the same
  /// budget.
  #[test]
  fn let_go_gives_back_every_chunk_and_keeps_the_budget() {
    let mut recent = Recent::default();
    recent.set_budget(2 * chunk());
    for n in 0..6 {
      recent.keep(at(n), Kind::Blob, vec![5; chunk() / 4]);
    }
    recent.let_go();
    assert_eq!((recent.held, recent.bytes, recent.chunks.len()), (0, 0, 0));
    assert_eq!(recent.objects.capacity(), 0);
    assert!(recent.get(at(5)).is_none());
    recent.keep(at(9), Kind::Blob, vec![6; chunk() / 4]);
    assert!(recent.get(at(9)).is_some_and(|(_, bytes)| bytes[0] == 6));
  }
}
