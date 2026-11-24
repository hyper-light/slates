//! A push's objects before its verdict (A-614; §4.17.2 "A push", items 2 and 3; §4.17.1 "Objects" items 4 and 5): each
//! named once by git's rule, checked against an id its source claims, and held as content by the push alone, so a
//! refused or abandoned push leaves nothing behind. A commit is read as it arrives for its tree, its parents and its
//! committer's date; its generation number waits for the push to be sealed ([`crate::store::Repository::seal`]), since
//! a pack sends a commit before its parents as often as after.
//!
//! **Read in place while judged** (main's ruling B, part 2; Ada's storage ruling of 2026-11-20: changes live in RAM).
//! A push whose objects arrive as a pack keeps the pack as the client sent it ([`Push::extend_pack`]) and each object as
//! where its entry begins ([`Push::admit_placed`]), never inflated beside it: git/git's v1.5.0 pack is 9.4 MiB, its
//! objects 304.6 MiB inflated. An object is read from the pack where it lies ([`Push::bytes`], `store::packed::read`),
//! a chain resolved from the objects read lately ([`super::packed::Recent`], held to the pack's length). A push's
//! staging puts the pack in the content store as one content, a window a unit ([`Push::set_stored_pack`]), and each
//! object's row names where it lies there (A-722's record v4, rows by pack place): what lands is the pack, never its
//! objects inflated.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::ops::Bound;

use slates_vcs::{
  AdmitRefused, Admitted, ContentId, ContentRefused, ContentStore, Index, Naming, Quarantine,
};

use crate::Admission;
use crate::object_name::{Kind, ObjectFormat, ObjectId};
use crate::objects::Ident;
use crate::objects::commit::Commit;

use super::naming::NameRefused;
use super::packed::{self, Base, Bases, InPack, PackSource, PackedRefused, Scratch};

#[cfg(test)]
std::thread_local! {
  /// Tests only (A-864's hostile red): the next entry's kept CRC32 made to disagree with its raw bytes by one bit, as a
  /// bit flipped in its stored bytes would, taken once.
  pub(crate) static MISKEEP_NEXT_CRC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The identity the push's own pack is read under ([`PackSource`] for [`Push`]): it is held by the push, not the content
/// store, so no store identity names it.
const OWN_PACK: ContentId = ContentId([0; 32]);

/// Objects coming into a repository from outside (module docs): a pack as a reader resolves it (A-608's `unpack`), an
/// import's loose objects (piece 15). The store depends on this trait and on no reader's module.
pub trait Entries {
  /// Why the source stopped.
  type Refused;

  /// The next object, `None` once the source is whole.
  fn next_entry(&mut self) -> Option<Result<Incoming<'_>, Self::Refused>>;
}

/// One object as its source hands it in.
#[derive(Clone, Copy, Debug)]
pub struct Incoming<'a> {
  /// Its type.
  pub kind: Kind,
  /// The id its source names it by (a reader names each object it resolves; an import's loose object is filed under
  /// one), checked against git's naming; `None` when the source names none.
  pub claimed: Option<ObjectId>,
  /// Its content: the bytes git hashes after the header.
  pub content: &'a [u8],
  /// The depth of the delta chain its source's pack stored it at, zero whole: what the
  /// deployment's own clients chose, which the fragments' depth learns from (A-614 part 2).
  pub depth: u64,
}

/// Why a push's admission was refused. The push keeps what it held before, for its caller to abandon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PushRefused<R> {
  /// The source refused (a pack cut short, a delta whose base is missing): its own reason.
  Source(R),
  /// git's naming refused the object: a collision attack, in git's words through [`NameRefused::words`].
  NotNamed {
    /// git's reason.
    reason: NameRefused,
  },
  /// The object does not hash to the id its source claimed.
  IdMismatch {
    /// The id claimed.
    id: ObjectId,
  },
  /// The push brought the id once more with other bytes.
  Collision {
    /// The id.
    id: ObjectId,
  },
  /// A commit git's parser refuses, or whose committer names no date it can read (`fsck`'s whole judgement is piece
  /// 6's).
  BadCommit {
    /// The commit.
    id: ObjectId,
  },
  /// The store refused the bytes (the budget).
  Content(ContentRefused),
}

/// What a push holds of a commit beyond its bytes: its tree, parents and date, read as it arrived, and its generation
/// number once the push is sealed (zero before).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Arrived {
  pub(crate) tree: ObjectId,
  pub(crate) parents: Vec<ObjectId>,
  pub(crate) date: u64,
  pub(crate) generation: u64,
}

/// A sealed push's rows, written before it lands in its own fork of its repository's index (A-722 part 3;
/// [`crate::store::Repository::stage`]): each object new to the repository given a position after the repository's
/// next when the fork was made, tentatively, and its rows written into the fork, sealed into tables. The commit that
/// lands the push adopts the fork whole ([`slates_vcs::Index::adopt`]).
#[derive(Debug)]
pub(crate) struct Staged {
  /// The fork the rows are written into.
  pub(crate) fork: Index,
  /// The position each staged object was given, by id.
  pub(crate) positions: BTreeMap<ObjectId, u64>,
  /// The repository's next position when the fork was made, which the positions follow.
  pub(crate) base: u64,
  /// The runs of positions given each type, `[start, end)` (A-639: by type, then id): what a position's type is read
  /// from once the push lands, with no row read (`Repository::kind_at`). One run a type the push brought.
  pub(crate) kinds: Vec<(u64, u64, Kind)>,
}

/// Where a held object's bytes are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeldAt {
  /// In the content store, held by the push: its root.
  Stored(ContentId),
  /// In the push's own pack (module docs, "Read in place while judged"): where its entry begins.
  Pack(u64),
}

/// An object the push holds: its type's number, its length, its content identity (the BLAKE3 of its bytes, what a
/// record names it by) and where its bytes are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeldObject {
  /// Its type's number, as a pack names it.
  pub number: u8,
  /// Its length.
  pub len: u64,
  /// Its content identity.
  pub identity: ContentId,
  /// Where its bytes are.
  pub at: HeldAt,
}

/// An object of the push's own pack (module docs, "Read in place while judged").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Placed {
  /// Its type's number.
  number: u8,
  /// Where its entry begins in the pack.
  offset: u64,
  /// Its length.
  len: u64,
  /// Its content identity.
  identity: ContentId,
}

/// A push's objects (module docs).
#[derive(Debug)]
pub struct Push {
  format: ObjectFormat,
  quarantine: Quarantine,
  commits: BTreeMap<ObjectId, Arrived>,
  /// Each admitted annotated tag's target object id, by tag id: stored in the tag's `ObjectRow` at
  /// staging time so peeling never inflates pack data after let-go.
  tags: BTreeMap<ObjectId, ObjectId>,
  /// The parents the held commits name, all together: what their rows' parts take beside their count
  /// ([`Push::commit_parts`], `Repository::stage_room`), kept as each commit arrives so no step of a push's decision
  /// counts them again (it did, every step: git/git's whole history, about 75,000 commits, decided at about 0.5 ms a
  /// step, its 1,027 refs unanswered after 1,200 s; w-git, 2026-11-21).
  parents_held: usize,
  sealed: bool,
  /// The deepest delta chain the push's source carried.
  deepest: u64,
  /// The rows staged once the push was sealed (A-722 part 3).
  staged: Option<Staged>,
  /// The pack as the client sent it (module docs, "Read in place while judged"). Kept because it is the push's objects'
  /// one copy, compressed and with their deltas, and what its record writes; it grows by what each call of the client's
  /// bytes brought.
  pack: Vec<u8>,
  /// The objects of `pack`, by name. One entry an object: what names it to where it lies.
  placed: BTreeMap<ObjectId, Placed>,
  /// Where each entry of `pack` begins, every one its source sent, those the repository held already among them, and
  /// the object it holds (A-864): what tells an entry's raw length (to the next entry, or the pack's trailer) and an
  /// offset delta's base, as git's pack index does.
  entries: BTreeMap<u64, ObjectId>,
  /// Offsets in `pack` for objects the repository already indexed and the push passed over: allows
  /// in-pack delta resolution and connectivity checks to read them directly from RAM (`pack`/`recent`)
  /// rather than hitting a let-go window of an older landed pack on disk.
  passed_over: BTreeMap<ObjectId, u64>,
  /// The pack put in the content store as one content by the push's staging, a window a unit (A-722's record v4: rows
  /// name their objects by where they lie in it, never a second copy inflated): its identity and length, held by the
  /// push until its rows hold it.
  stored_pack: Option<(ContentId, u64)>,
  /// The longest object of its own pack: what a read of its objects, and once it lands its repository's, is held to.
  largest: u64,
  /// The buffers an object is read from the pack with, and the objects read lately (`packed::Scratch`): reused read
  /// after read; what it keeps lately is held to the pack's length. In a cell because a read is a look, not a change.
  reading: RefCell<Scratch>,
}

impl Push {
  /// An empty push into a repository of `format`.
  pub fn new(format: ObjectFormat) -> Push {
    Push {
      format,
      quarantine: Quarantine::new(),
      commits: BTreeMap::new(),
      tags: BTreeMap::new(),
      parents_held: 0,
      sealed: false,
      deepest: 0,
      staged: None,
      pack: Vec::new(),
      placed: BTreeMap::new(),
      entries: BTreeMap::new(),
      passed_over: BTreeMap::new(),
      stored_pack: None,
      largest: 0,
      reading: RefCell::new(Scratch::default()),
    }
  }

  /// The repository's format.
  pub fn format(&self) -> ObjectFormat {
    self.format
  }

  /// Admits one object: named by `naming` once, checked against the id its source claims, its bytes held; a commit
  /// read for its tree, parents and date. The id, and whether the push held it already.
  pub fn admit<N, R>(
    &mut self,
    store: &mut ContentStore,
    naming: &N,
    incoming: Incoming<'_>,
  ) -> Result<(ObjectId, Admitted), PushRefused<R>>
  where
    N: Naming<Id = ObjectId, Refused = NameRefused>,
  {
    let number = incoming.kind.pack_number();
    let id = naming
      .name(number, &[], incoming.content)
      .map_err(|reason| PushRefused::NotNamed { reason })?;
    if let Some(claimed) = incoming.claimed
      && claimed != id
    {
      return Err(PushRefused::IdMismatch { id: claimed });
    }
    let admitted = self
      .quarantine
      .hold_named(store, id.as_bytes(), number, incoming.content)
      .map_err(|refused| held_refused(refused, id))?;
    if incoming.kind == Kind::Commit && admitted == Admitted::New {
      let room = store.room();
      self.read_commit(id, incoming.content, room)?;
    } else if incoming.kind == Kind::Tag && admitted == Admitted::New {
      self.read_tag(id, incoming.content, store.room());
    }
    self.sealed = false;
    self.deepest = self.deepest.max(incoming.depth);
    Ok((id, admitted))
  }

  /// Admits one object of the push's own pack (module docs, "Read in place while judged"), as [`Push::admit`] admits
  /// one, its entry beginning at `offset` in the pack [`Push::extend_pack`] was handed: named, checked and read as a
  /// commit from `incoming`'s bytes, which are then kept no further than the objects read lately (`recent`, taken whole,
  /// no copy). The id, and whether the push held it already.
  pub fn admit_placed<N, R>(
    &mut self,
    store: &ContentStore,
    naming: &N,
    (incoming, offset): (Incoming<'_>, u64),
  ) -> Result<(ObjectId, Admitted), PushRefused<R>>
  where
    N: Naming<Id = ObjectId, Refused = NameRefused>,
  {
    let number = incoming.kind.pack_number();
    let id = naming
      .name(number, &[], incoming.content)
      .map_err(|reason| PushRefused::NotNamed { reason })?;
    if let Some(claimed) = incoming.claimed
      && claimed != id
    {
      return Err(PushRefused::IdMismatch { id: claimed });
    }
    self.admit_placed_named(store, id, (incoming, offset))
  }

  /// Admits an object already unpacked and named with collision-detecting SHA-1/SHA-256 by
  /// [`crate::unpack::Reader`] without hashing its content a second time.
  pub fn admit_unpacked<R>(
    &mut self,
    store: &ContentStore,
    object: &crate::unpack::Object,
  ) -> Result<(ObjectId, Admitted), PushRefused<R>> {
    if object.id.format() != self.format {
      return Err(PushRefused::IdMismatch { id: object.id });
    }
    let incoming = Incoming {
      kind: object.kind,
      claimed: Some(object.id),
      content: &object.content,
      depth: object.chain.as_ref().map_or(0, |chain| chain.depth),
    };
    self.admit_placed_named(store, object.id, (incoming, object.offset))
  }

  /// Shared placement path once `id` has been verified against `incoming.content`.
  fn admit_placed_named<R>(
    &mut self,
    store: &ContentStore,
    id: ObjectId,
    (incoming, offset): (Incoming<'_>, u64),
  ) -> Result<(ObjectId, Admitted), PushRefused<R>> {
    let number = incoming.kind.pack_number();
    let identity = store.object_id(incoming.content);
    let again = match (self.placed.get(&id), self.quarantine.object(id.as_bytes())) {
      (Some(placed), _) => Some((placed.number, placed.identity)),
      (None, Some((number, content, _))) => Some((number, content)),
      (None, None) => None,
    };
    self.entries.insert(offset, id);
    if let Some(held) = again {
      return if held == (number, identity) {
        Ok((id, Admitted::Again))
      } else {
        Err(PushRefused::Collision { id })
      };
    }
    let len = u64::try_from(incoming.content.len())
      .map_err(|_| PushRefused::Content(ContentRefused::CountExhausted))?;
    if incoming.kind == Kind::Commit {
      self.read_commit(id, incoming.content, store.room())?;
    } else if incoming.kind == Kind::Tag {
      self.read_tag(id, incoming.content, store.room());
    }
    self.largest = self.largest.max(len);
    self.placed.insert(
      id,
      Placed {
        number,
        offset,
        len,
        identity,
      },
    );
    self.sealed = false;
    self.deepest = self.deepest.max(incoming.depth);
    Ok((id, Admitted::New))
  }

  /// Appends `bytes`, the next of the pack the client sent, to the push's own (module docs, "Read in place while
  /// judged"); what the push keeps lately is held to the pack's length from now on.
  pub fn extend_pack(&mut self, bytes: &[u8]) {
    self.pack.extend_from_slice(bytes);
    self.reading.get_mut().recent().set_budget(self.pack.len());
  }

  /// Keeps `bytes`, the object of type `kind` whose entry begins at `offset` in the push's pack, as read lately (taken
  /// whole, no copy): a delta resolved later starts from it.
  pub fn keep_read(&mut self, offset: u64, kind: Kind, bytes: Vec<u8>) {
    let place = self.own(offset);
    self.reading.get_mut().recent().keep(place, kind, bytes);
  }

  /// The push's own pack as the client sent it (A-723: what its record writes).
  pub fn pack(&self) -> &[u8] {
    &self.pack
  }

  /// The bytes the push's reading keeps lately ([`super::packed::Recent`]).
  pub fn kept_lately(&self) -> usize {
    self.reading.borrow_mut().recent().bytes()
  }

  /// Where the entry at `offset` of the push's own pack is.
  fn own(&self, offset: u64) -> InPack {
    InPack {
      pack: OWN_PACK,
      len: u64::try_from(self.pack.len()).unwrap_or(u64::MAX),
      offset,
    }
  }

  /// The bytes of the object held under `id`: lent from the content store where it is one window there, else copied
  /// out of it; or read from the push's own pack where it lies (module docs), a REF_DELTA's base outside the push read
  /// from `outside` (the repository's). None where the push holds no such object.
  pub fn bytes<'s>(
    &'s self,
    store: &'s ContentStore,
    outside: &dyn Bases,
    id: &ObjectId,
  ) -> Option<Result<Cow<'s, [u8]>, PackedRefused>> {
    let held = self.object(id)?;
    Some(match held.at {
      HeldAt::Stored(content) => {
        stored_bytes(store, &content, held.len).ok_or(PackedRefused::Unread { at: 0 })
      }
      HeldAt::Pack(offset) => self.read_placed(store, outside, offset).map(Cow::Owned),
    })
  }

  /// Like [`Push::bytes`], also serving objects that arrived in this push's pack but were passed
  /// over because the repository already indexed them ([`Push::entry_at`]), reading directly from
  /// the push's in-RAM pack and recent cache rather than a let-go repository pack on disk.
  pub fn pack_bytes<'s>(
    &'s self,
    store: &'s ContentStore,
    outside: &dyn Bases,
    id: &ObjectId,
  ) -> Option<Result<Cow<'s, [u8]>, PackedRefused>> {
    if let Some(read) = self.bytes(store, outside, id) {
      return Some(read);
    }
    let &offset = self.passed_over.get(id)?;
    Some(self.read_placed(store, outside, offset).map(Cow::Owned))
  }

  /// Reads the object whose entry begins at `offset` in the push's pack. Its buffer is the reader's, handed over: an
  /// object's bytes are allocated once for the caller that takes them.
  fn read_placed(
    &self,
    store: &ContentStore,
    outside: &dyn Bases,
    offset: u64,
  ) -> Result<Vec<u8>, PackedRefused> {
    let bases = PushBases {
      push: self,
      store,
      outside,
    };
    let mut out = Vec::new();
    let mut reading = self.reading.borrow_mut();
    let source = PushSource { push: self, store };
    packed::read(
      &source,
      (self.own(offset), self.format),
      &bases,
      (&mut *reading, self.largest.max(store.room())),
      &mut out,
    )?;
    Ok(out)
  }

  /// Admits every object `entries` hands in, in order; how many were new to the push.
  pub fn admit_all<N, E>(
    &mut self,
    store: &mut ContentStore,
    naming: &N,
    entries: &mut E,
  ) -> Result<u64, PushRefused<E::Refused>>
  where
    N: Naming<Id = ObjectId, Refused = NameRefused>,
    E: Entries,
  {
    let mut new: u64 = 0;
    while let Some(next) = entries.next_entry() {
      let incoming = next.map_err(PushRefused::Source)?;
      if self.admit(store, naming, incoming)?.1 == Admitted::New {
        new = new.saturating_add(1);
      }
    }
    Ok(new)
  }

  /// Records an admitted annotated tag's target object id when it parses cleanly.
  fn read_tag(&mut self, id: ObjectId, content: &[u8], room: u64) {
    if let Ok(tag) = crate::objects::tag::Tag::parse(content, &mut Admission::new(room))
      && let Some(target) = tag.object(self.format)
    {
      self.tags.insert(id, target);
    }
  }

  /// The target object id of an admitted annotated tag, if parsed at admission.
  pub(crate) fn tag_target(&self, id: &ObjectId) -> Option<ObjectId> {
    self.tags.get(id).copied()
  }

  /// Whether this push holds any object admitted loose (outside a pack) in its quarantine.
  pub(crate) fn has_loose(&self) -> bool {
    !self.quarantine.is_empty()
  }

  /// Reads a commit's tree, parents and committer's date, within an admission of the store's room.
  fn read_commit<R>(
    &mut self,
    id: ObjectId,
    content: &[u8],
    room: u64,
  ) -> Result<(), PushRefused<R>> {
    let bad = || PushRefused::BadCommit { id };
    let commit = Commit::parse(content, &mut Admission::new(room)).map_err(|_| bad())?;
    let tree = commit.tree(self.format).ok_or_else(bad)?;
    let parents: Vec<ObjectId> = commit.parents(self.format).collect();
    let named = parents.len();
    let date = commit
      .committer()
      .and_then(|line| Ident::parse(line).ok())
      .map(|ident| ident.seconds)
      .ok_or_else(bad)?;
    let replaced = self.commits.insert(
      id,
      Arrived {
        tree,
        parents,
        date,
        generation: 0,
      },
    );
    let gone = replaced.map_or(0, |replaced| replaced.parents.len());
    self.parents_held = self.parents_held.saturating_sub(gone).saturating_add(named);
    Ok(())
  }

  /// The objects held.
  pub fn len(&self) -> usize {
    self.quarantine.len().saturating_add(self.placed.len())
  }

  /// Whether no object is held.
  pub fn is_empty(&self) -> bool {
    self.quarantine.is_empty() && self.placed.is_empty()
  }

  /// The deepest delta chain the push's source carried.
  pub fn deepest(&self) -> u64 {
    self.deepest
  }

  /// Hands the push a delta chain `depth` deep that its source carried, as a push's record names it (A-662,
  /// `slates-server`'s `vcs_store`): a push replayed from a record holding no object (a later ref of a push without
  /// `atomic`, whose first commit took every object, or a push whose objects another push committed first) carries
  /// its pack's chain to the store's learned depth at its commit, as the live push did.
  pub fn carried(&mut self, depth: u64) {
    self.deepest = self.deepest.max(depth);
  }

  /// Whether the push is sealed: every commit's generation number is known.
  pub fn is_sealed(&self) -> bool {
    self.sealed
  }

  /// The kind, root content and length of the object held under `id`: what a push's record names an object by, its
  /// bytes read back from its root when the record is written (A-662, `slates-server`'s `vcs_store::record`).
  pub fn object(&self, id: &ObjectId) -> Option<HeldObject> {
    if let Some(placed) = self.placed.get(id) {
      return Some(placed.held());
    }
    let (number, content, len) = self.quarantine.object(id.as_bytes())?;
    Some(HeldObject {
      number,
      len,
      identity: content,
      at: HeldAt::Stored(content),
    })
  }

  /// The commits held, with what was read of each.
  pub(crate) fn commits(&self) -> &BTreeMap<ObjectId, Arrived> {
    &self.commits
  }

  /// The commits held and the parents they name, all together: what their rows' parts take.
  pub(crate) fn commit_parts(&self) -> (usize, usize) {
    (self.commits.len(), self.parents_held)
  }

  /// Seals the push with every commit's generation number.
  pub(crate) fn seal_with(&mut self, generations: &BTreeMap<ObjectId, u64>) {
    for (id, arrived) in &mut self.commits {
      arrived.generation = generations.get(id).copied().unwrap_or(0);
    }
    self.sealed = true;
  }

  /// Every object held, in its ids' order.
  pub(crate) fn held(&self) -> impl Iterator<Item = (ObjectId, HeldObject)> + '_ {
    let mut after = None;
    core::iter::from_fn(move || {
      let next = self.held_after(after.as_ref())?;
      after = Some(next.0);
      Some(next)
    })
  }

  /// The first object held whose id is after `after` (the first for none), as [`Push::held`] answers it (A-722 part 4):
  /// the first of the pack's and the content store's.
  pub(crate) fn held_after(&self, after: Option<&ObjectId>) -> Option<(ObjectId, HeldObject)> {
    let from = after.map_or(Bound::Unbounded, Bound::Excluded);
    let placed = self
      .placed
      .range((from, Bound::Unbounded))
      .next()
      .map(|(id, placed)| (*id, placed.held()));
    let stored = self
      .quarantine
      .object_after(after.map(ObjectId::as_bytes))
      .and_then(|(id, number, content, len)| {
        let id = ObjectId::from_bytes(self.format, id)?;
        let at = HeldAt::Stored(content);
        Some((
          id,
          HeldObject {
            number,
            len,
            identity: content,
            at,
          },
        ))
      });
    match (placed, stored) {
      (Some(placed), Some(stored)) => Some(if placed.0 <= stored.0 { placed } else { stored }),
      (placed, stored) => placed.or(stored),
    }
  }

  /// Whether the push holds an object of its own pack.
  pub(crate) fn has_placed(&self) -> bool {
    !self.placed.is_empty()
  }

  /// Where the object held under `id` is in the content store: its own content, for one held whole; where its entry
  /// lies in the push's pack once the staging has put the pack there ([`Push::set_stored_pack`]); none before.
  pub(crate) fn stored(&self, id: &ObjectId) -> Option<StoredAt> {
    match self.placed.get(id) {
      Some(placed) => {
        let (pack, len) = self.stored_pack?;
        Some(StoredAt::InPack(InPack {
          pack,
          len,
          offset: placed.offset,
        }))
      }
      None => self
        .quarantine
        .object(id.as_bytes())
        .map(|(_, content, _)| StoredAt::Whole(content)),
    }
  }

  /// Notes that the entry beginning at `offset` of the push's pack holds `id`, an object the repository holds already and
  /// the push passes over (A-864): so the entry before it is known to end there, and a delta against it is named.
  pub fn entry_at(&mut self, offset: u64, id: ObjectId) {
    self.entries.insert(offset, id);
    self.passed_over.entry(id).or_insert(offset);
  }

  /// What a row keeps of placed object `id`'s entry in the push's pack (A-864, `rows::PackedRow`): its raw length, to the
  /// next entry or the pack's trailer; the CRC32 of those raw bytes; and for a delta, the object it is a delta against
  /// (an offset delta's by the entry it names, a reference delta's by its name). None for an object not placed, or an
  /// entry whose header cannot be read, which a fetch then encodes.
  pub(crate) fn entry_of(&self, id: &ObjectId) -> Option<(u64, u32, Option<ObjectId>)> {
    let offset = self.placed.get(id)?.offset;
    let trailer =
      crate::to_u64(self.pack.len()).checked_sub(crate::to_u64(self.format.raw_len()))?;
    let end = self
      .entries
      .range(offset.saturating_add(1)..)
      .next()
      .map_or(trailer, |(next, _)| *next);
    let raw = self
      .pack
      .get(usize::try_from(offset).ok()?..usize::try_from(end).ok()?)?;
    let mut admission = Admission::new(u64::MAX);
    let (header, _) = crate::entry::read(raw, offset, self.format, &mut admission).ok()??;
    let base = match header.representation {
      crate::entry::Representation::Whole(_) => None,
      crate::entry::Representation::OfsDelta { base } => Some(*self.entries.get(&base)?),
      crate::entry::Representation::RefDelta { base } => Some(base),
    };
    let crc = crc32fast::hash(raw);
    #[cfg(test)]
    let crc = crc ^ u32::from(MISKEEP_NEXT_CRC.with(|next| next.replace(false)));
    Some((end.saturating_sub(offset), crc, base))
  }

  /// The longest object of the push's own pack.
  pub(crate) fn largest(&self) -> u64 {
    self.largest
  }

  /// The pack as the staging put it in the content store, held by the push: its identity and length.
  pub fn stored_pack(&self) -> Option<(ContentId, u64)> {
    self.stored_pack
  }

  /// Keeps `identity`, the push's pack the staging put in the content store `len` bytes long, holding it (the put's
  /// hold) until the push's rows hold it.
  pub(crate) fn set_stored_pack(&mut self, identity: ContentId, len: u64) {
    self.stored_pack = Some((identity, len));
  }

  /// Gives back the push's holds once its objects' rows hold them, leaving the push empty: the stored pack's one hold,
  /// whatever its objects' count, and each object held whole.
  pub(crate) fn release_holds(&mut self, store: &mut ContentStore) -> Result<(), ContentRefused> {
    self.commits.clear();
    self.parents_held = 0;
    let placed = self.release_placed(store);
    self.quarantine.abandon(store).and(placed)
  }

  /// Gives back the stored pack's hold and lets go of the pack, its objects, their order and its reading.
  fn release_placed(&mut self, store: &mut ContentStore) -> Result<(), ContentRefused> {
    let given = match self.stored_pack.take() {
      Some((identity, _)) => store.release(&identity).map(drop),
      None => Ok(()),
    };
    self.placed = BTreeMap::new();
    self.entries = BTreeMap::new();
    self.passed_over = BTreeMap::new();
    self.pack = Vec::new();
    // Discharge `recent` before resetting `Scratch` so its `ContentStore` charge is never leaked.
    self.release_recent(store);
    *self.reading.get_mut() = Scratch::default();
    given
  }

  /// The rows staged, if the push was staged.
  pub(crate) fn staged(&self) -> Option<&Staged> {
    self.staged.as_ref()
  }

  /// Keeps `staged` as the push's rows (A-722 part 3).
  pub(crate) fn set_staged(&mut self, staged: Staged) {
    self.staged = Some(staged);
  }

  /// The rows staged, taken: a landing adopts them or gives them back.
  pub(crate) fn take_staged(&mut self) -> Option<Staged> {
    self.staged.take()
  }

  /// Charges `store` for the objects the push keeps read lately as they stand ([`super::packed::Recent::true_up`],
  /// A-861): run after each turn of its admission.
  pub fn true_up_recent(&mut self, store: &mut ContentStore) {
    self.reading.get_mut().recent().true_up(store);
  }

  /// Gives back the objects the push keeps read lately and their charge on `store` (A-861): what its landing and its
  /// abandoning run.
  pub fn release_recent(&mut self, store: &mut ContentStore) {
    self.reading.get_mut().recent().release(store);
  }

  /// Releases buffers only needed while unpacking and checking the incoming pack once the push
  /// enters sealing: always drops `recent` (discharging its `ContentStore` hold), and when every
  /// object in the pack was already indexed by the repository (`!has_placed() && !has_loose()`),
  /// drops the raw pack bytes, entry maps, and read scratch immediately so concurrent pushes that
  /// introduce no new objects hold zero pack memory while awaiting turns, homing, or landing.
  pub fn trim_for_sealing(&mut self, store: &mut ContentStore) {
    self.release_recent(store);
    if self.placed.is_empty() && self.quarantine.is_empty() {
      self.entries = BTreeMap::new();
      self.passed_over = BTreeMap::new();
      self.pack = Vec::new();
      *self.reading.get_mut() = Scratch::default();
    }
  }

  /// Gives back every hold the push took, its staged rows' included, leaving nothing behind.
  pub fn abandon(mut self, store: &mut ContentStore) -> Result<(), ContentRefused> {
    self.release_recent(store);
    let staged = self.staged.take().map(|staged| staged.fork.release(store));
    let placed = self.release_placed(store);
    let quarantine = self.quarantine.abandon(store).and(placed);
    match staged {
      Some(Err(slates_vcs::IndexRefused::Content(refused))) => quarantine.and(Err(refused)),
      _ => quarantine,
    }
  }
}

/// A refusal of holding the named object `id`, in the push's terms.
fn held_refused<R>(refused: AdmitRefused<Infallible>, id: ObjectId) -> PushRefused<R> {
  match refused {
    AdmitRefused::Content(content) => PushRefused::Content(content),
    AdmitRefused::Collision { .. } => PushRefused::Collision { id },
    AdmitRefused::IdMismatch { .. } => PushRefused::IdMismatch { id },
    AdmitRefused::NotNamed { reason, .. } => match reason {},
  }
}

impl Placed {
  /// As the push answers a held object.
  fn held(&self) -> HeldObject {
    HeldObject {
      number: self.number,
      len: self.len,
      identity: self.identity,
      at: HeldAt::Pack(self.offset),
    }
  }
}

/// Where an object is in the content store (A-722's record v4): its own content, held whole; or where its entry lies in
/// a pack held as one content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoredAt {
  /// Its own content: its root.
  Whole(ContentId),
  /// An entry of a pack.
  InPack(InPack),
}

/// The push's own pack, as its objects are read from it: a piece is the rest of the pack from `offset`.
impl PackSource for Push {
  fn piece(&self, _pack: &ContentId, _len: u64, offset: u64) -> Option<Cow<'_, [u8]>> {
    let offset = usize::try_from(offset).ok()?;
    self
      .pack
      .get(offset..)
      .filter(|rest| !rest.is_empty())
      .map(Cow::Borrowed)
  }
}

/// Where a push's objects are read from: its own pack, and the packs the repository holds in the content store, which
/// a thin pack's REF_DELTA base may lie in (A-722's record v4), each by its identity.
struct PushSource<'a> {
  push: &'a Push,
  store: &'a ContentStore,
}

impl PackSource for PushSource<'_> {
  fn piece(&self, pack: &ContentId, len: u64, offset: u64) -> Option<Cow<'_, [u8]>> {
    if *pack == OWN_PACK {
      return self.push.piece(pack, len, offset);
    }
    self.store.piece(pack, len, offset)
  }

  fn away(&self, pack: &ContentId, len: u64, offset: u64) -> bool {
    *pack != OWN_PACK && self.store.away(pack, len, offset)
  }
}

/// Where a REF_DELTA's base is for an object of the push's pack: an entry of the pack, an object the push holds in the
/// content store, else outside the push.
struct PushBases<'a> {
  push: &'a Push,
  store: &'a ContentStore,
  outside: &'a dyn Bases,
}

impl Bases for PushBases<'_> {
  fn locate(&self, id: &ObjectId, whole: &mut Vec<u8>, room: u64) -> Option<Base> {
    let Some(held) = self.push.object(id) else {
      return self.outside.locate(id, whole, room);
    };
    match held.at {
      HeldAt::Pack(offset) => Some(Base::InPack(self.push.own(offset))),
      HeldAt::Stored(content) => {
        if held.len > room {
          return None;
        }
        let bytes = stored_bytes(self.store, &content, held.len)?;
        whole.clear();
        whole.extend_from_slice(&bytes);
        Some(Base::Whole(super::kind_of(held.number)?))
      }
    }
  }
}

/// The bytes of the content `len` bytes long at `content`: lent where the store holds it as one window, else assembled.
pub fn stored_bytes<'s>(
  store: &'s ContentStore,
  content: &ContentId,
  len: u64,
) -> Option<Cow<'s, [u8]>> {
  let mut windows = store.windows(content);
  if let (Some(window), None) = (windows.next(), windows.next())
    && u64::try_from(window.len()).ok() == Some(len)
  {
    return Some(window);
  }
  store.object_bytes(content, len).map(Cow::Owned)
}
