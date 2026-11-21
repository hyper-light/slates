//! `store::packed`'s oracle (A-722's memory per push, ruling B part 1): every object of each captured push's pack, read in
//! place from the pack the content store holds, is the object git's reader made of the pack whole.

use std::collections::BTreeMap;
use std::convert::Infallible;

use slates_vcs::ContentStore;

use crate::Admission;
use crate::object_name::{Kind, ObjectFormat, ObjectId};
use crate::store::packed::{Base, Bases, InPack, PackedRefused, Scratch, read};
use crate::store::{GitNaming, Incoming, Push};
use crate::unpack::{self, NoIndex, Outside};

/// Shape: a window small enough that the captured packs' entries cross windows (the fixtures' packs are a few hundred
/// bytes to a few kilobytes).
const SMALL_WINDOW: usize = 64;

/// Shape: room enough for every fixture's object.
const ROOM: u64 = 1 << 20;

/// The captured pushes: each one's request (commands, then the pack) and the pack of objects its server held before.
const PUSHES: [(&str, &[u8], &[u8]); 6] = [
  (
    "create",
    include_bytes!("../../../fixtures/push/create.request.bin"),
    include_bytes!("../../../fixtures/push/create.before.pack"),
  ),
  (
    "update",
    include_bytes!("../../../fixtures/push/update.request.bin"),
    include_bytes!("../../../fixtures/push/update.before.pack"),
  ),
  (
    "non-ff",
    include_bytes!("../../../fixtures/push/non-ff.request.bin"),
    include_bytes!("../../../fixtures/push/non-ff.before.pack"),
  ),
  (
    "atomic",
    include_bytes!("../../../fixtures/push/atomic.request.bin"),
    include_bytes!("../../../fixtures/push/atomic.before.pack"),
  ),
  (
    "hidden",
    include_bytes!("../../../fixtures/push/hidden.request.bin"),
    include_bytes!("../../../fixtures/push/hidden.before.pack"),
  ),
  (
    "fsck",
    include_bytes!("../../../fixtures/push/fsck.request.bin"),
    include_bytes!("../../../fixtures/push/fsck.before.pack"),
  ),
];

/// The objects a server held before a push, by name: a thin pack's outside bases.
struct Held(BTreeMap<ObjectId, (Kind, Vec<u8>)>);

impl Outside for Held {
  fn object(&self, id: &ObjectId) -> Option<(Kind, &[u8])> {
    self
      .0
      .get(id)
      .map(|(kind, bytes)| (*kind, bytes.as_slice()))
  }
}

/// The objects of `pack`, read whole, `outside` its thin bases.
fn read_whole(pack: &[u8], outside: &Held) -> unpack::Unpacked {
  let mut admission = Admission::new(u64::MAX);
  let mut reader = unpack::Reader::new(ObjectFormat::Sha1, &NoIndex);
  reader.read(pack, &mut admission).unwrap();
  reader.finish_with(outside, &mut admission).unwrap()
}

/// A push's REF_DELTA bases: the pack's own entries by name, and what its server held before, handed over whole.
struct Located<'h> {
  /// The pack's entries.
  entries: BTreeMap<ObjectId, InPack>,
  /// What the server held.
  held: &'h Held,
}

impl Bases for Located<'_> {
  fn locate(&self, id: &ObjectId, whole: &mut Vec<u8>, room: u64) -> Option<Base> {
    if let Some(entry) = self.entries.get(id) {
      return Some(Base::InPack(*entry));
    }
    let (kind, bytes) = self.held.0.get(id)?;
    if u64::try_from(bytes.len()).unwrap() > room {
      return None;
    }
    whole.clear();
    whole.extend_from_slice(bytes);
    Some(Base::Whole(*kind))
  }
}

/// A-722's memory per push (ruling B part 1). Do: for each captured push, read its pack whole as git's reader does, its
/// thin bases from what its server held; hold the pack once in a content store of 64-byte windows; read every object
/// in place at its entry's offset, through one scratch reused across all. Expect: each object's type and bytes those of
/// the whole read, deltas against an earlier entry and against an object held outside the pack among them.
#[test]
fn every_object_read_in_place_from_a_held_pack_is_the_object_read_whole() {
  let mut scratch = Scratch::default();
  let mut out = Vec::new();
  for (name, request, before) in PUSHES {
    let at = request
      .windows(4)
      .position(|magic| magic == b"PACK")
      .unwrap();
    let pack = &request[at..];
    let held = Held(
      read_whole(before, &Held(BTreeMap::new()))
        .objects
        .into_iter()
        .map(|object| (object.id, (object.kind, object.content)))
        .collect(),
    );
    let whole = read_whole(pack, &held);
    let mut store = ContentStore::new(1 << 26, SMALL_WINDOW).unwrap();
    let root = store.put_object(pack).unwrap();
    let len = u64::try_from(pack.len()).unwrap();
    let entries = whole
      .objects
      .iter()
      .map(|object| {
        (
          object.id,
          InPack {
            pack: root,
            len,
            offset: object.offset,
          },
        )
      })
      .collect();
    let bases = Located {
      entries,
      held: &held,
    };
    for object in &whole.objects {
      let place = InPack {
        pack: root,
        len,
        offset: object.offset,
      };
      let kind = read(
        &store,
        (place, ObjectFormat::Sha1),
        &bases,
        (&mut scratch, ROOM),
        &mut out,
      )
      .unwrap_or_else(|refused| panic!("{name} at {}: {refused:?}", object.offset));
      assert_eq!(
        (kind, &out),
        (object.kind, &object.content),
        "{name} at {}",
        object.offset
      );
    }
  }
}

/// A-722's memory per push (ruling B part 1; §4.9). Do: read an entry whose REF_DELTA base is held nowhere, and one
/// whose size is past the room. Expect: `MissingBase` naming the base, and `PastRoom`, typed.
#[test]
fn an_entry_whose_base_is_held_nowhere_or_past_the_room_is_refused_typed() {
  let (_, request, before) = PUSHES[1];
  let at = request
    .windows(4)
    .position(|magic| magic == b"PACK")
    .unwrap();
  let pack = &request[at..];
  let held = Held(
    read_whole(before, &Held(BTreeMap::new()))
      .objects
      .into_iter()
      .map(|object| (object.id, (object.kind, object.content)))
      .collect(),
  );
  let whole = read_whole(pack, &held);
  let mut store = ContentStore::new(1 << 26, SMALL_WINDOW).unwrap();
  let root = store.put_object(pack).unwrap();
  let len = u64::try_from(pack.len()).unwrap();
  let nowhere = Located {
    entries: BTreeMap::new(),
    held: &Held(BTreeMap::new()),
  };
  let (mut scratch, mut out) = (Scratch::default(), Vec::new());
  let mut missing = 0;
  for object in &whole.objects {
    let place = InPack {
      pack: root,
      len,
      offset: object.offset,
    };
    match read(
      &store,
      (place, ObjectFormat::Sha1),
      &nowhere,
      (&mut scratch, ROOM),
      &mut out,
    ) {
      Err(PackedRefused::MissingBase { .. }) => missing += 1,
      Ok(_) => {}
      Err(other) => panic!("{other:?}"),
    }
    let tight = read(
      &store,
      (place, ObjectFormat::Sha1),
      &nowhere,
      (&mut scratch, 0),
      &mut out,
    );
    assert!(
      matches!(
        tight,
        Err(PackedRefused::PastRoom { .. } | PackedRefused::MissingBase { .. })
      ),
      "{tight:?}"
    );
  }
  assert!(
    missing > 0,
    "the thin pack has a delta on a base outside it"
  );
}

/// The pack of each captured push and the objects its server held before (its thin bases), read whole.
fn captured(request: &[u8], before: &[u8]) -> (Vec<u8>, Held) {
  let at = request
    .windows(4)
    .position(|magic| magic == b"PACK")
    .unwrap();
  let held = Held(
    read_whole(before, &Held(BTreeMap::new()))
      .objects
      .into_iter()
      .map(|object| (object.id, (object.kind, object.content)))
      .collect(),
  );
  (request[at..].to_vec(), held)
}

/// `object` as a pack's reader hands it to a push.
fn incoming(object: &unpack::Object) -> Incoming<'_> {
  Incoming {
    kind: object.kind,
    claimed: Some(object.id),
    content: &object.content,
    depth: object.chain.as_ref().map_or(0, |chain| chain.depth),
  }
}

/// B2, a push read in place while judged (main's ruling B part 2). Do: for each captured push, hand a push its pack
/// seven bytes a call, as a client's bytes arrive, and admit each object its reader resolved where its entry begins,
/// keeping none of the objects' bytes; read every object back through the push. Expect: each object's bytes those of
/// the whole read, deltas against an earlier entry and against an object held outside the pack among them; the content
/// store holding nothing more; what the push keeps lately within its pack's length.
#[test]
fn b2_every_object_of_a_push_read_where_it_lies_in_its_pack_is_the_object_read_whole() {
  for (name, request, before) in PUSHES {
    let (pack, held) = captured(request, before);
    let whole = read_whole(&pack, &held);
    let store = ContentStore::new(1 << 26, SMALL_WINDOW).unwrap();
    let used = store.used();
    let naming = GitNaming {
      format: ObjectFormat::Sha1,
    };
    let mut push = Push::new(ObjectFormat::Sha1);
    for piece in pack.chunks(7) {
      push.extend_pack(piece);
    }
    for object in &whole.objects {
      push
        .admit_placed::<_, Infallible>(&store, &naming, (incoming(object), object.offset))
        .unwrap_or_else(|refused| panic!("{name} at {}: {refused:?}", object.offset));
    }
    let outside = Located {
      entries: BTreeMap::new(),
      held: &held,
    };
    for object in &whole.objects {
      let read = push
        .bytes(&store, &outside, &object.id)
        .unwrap_or_else(|| panic!("{name}: {} not held", object.id))
        .unwrap_or_else(|refused| panic!("{name} at {}: {refused:?}", object.offset));
      assert_eq!(
        read.as_ref(),
        object.content.as_slice(),
        "{name} at {}",
        object.offset
      );
    }
    assert_eq!(
      store.used(),
      used,
      "{name}: the store holds none of the push's objects"
    );
    assert!(
      push.kept_lately() <= pack.len(),
      "{name}: {} kept lately",
      push.kept_lately()
    );
  }
}

/// B2's resident bound (Ada's MEMORY SIZING rule: resident ≤ k×N + base). Do: push 64 blobs of 64 KiB each, each one
/// byte repeated (git/git's v1.5.0 push is 9.4 MiB of pack for 304.6 MiB of objects; these deflate further), once held
/// in the content store as they arrive (`Push::admit`, what a push did before) and once where each lies in the pack
/// (`Push::admit_placed`), its reader's bytes kept as read lately. Expect: held in place, the push's resident (what the
/// store grew by, its pack and what it keeps lately) within twice its pack; held in the store, past it, every object
/// inflated. Both printed, before and after.
#[test]
fn b2_an_in_flight_push_holds_its_pack_and_not_its_objects_inflated() {
  const BLOBS: u8 = 64;
  const BLOB_LEN: usize = 64 << 10;
  let blobs: Vec<Vec<u8>> = (0..BLOBS).map(|byte| vec![byte; BLOB_LEN]).collect();
  let pack = crate::pack::of_whole(
    ObjectFormat::Sha1,
    blobs.iter().map(|blob| (Kind::Blob, blob.as_slice())),
  )
  .unwrap();
  let whole = read_whole(&pack, &Held(BTreeMap::new()));
  let naming = GitNaming {
    format: ObjectFormat::Sha1,
  };
  let inflated: usize = blobs.iter().map(Vec::len).sum();

  let mut store = ContentStore::new(1 << 30, 1 << 16).unwrap();
  let used = store.used();
  let mut stored = Push::new(ObjectFormat::Sha1);
  for object in &whole.objects {
    stored
      .admit::<_, Infallible>(&mut store, &naming, incoming(object))
      .unwrap();
  }
  let before = usize::try_from(store.used().saturating_sub(used)).unwrap();
  stored.abandon(&mut store).unwrap();

  let used = store.used();
  let mut placed = Push::new(ObjectFormat::Sha1);
  placed.extend_pack(&pack);
  for object in read_whole(&pack, &Held(BTreeMap::new())).objects {
    placed
      .admit_placed::<_, Infallible>(&store, &naming, (incoming(&object), object.offset))
      .unwrap();
    placed.keep_read(object.offset, object.kind, object.content);
  }
  let grown = usize::try_from(store.used().saturating_sub(used)).unwrap();
  let after = grown + placed.pack().len() + placed.kept_lately();
  println!(
    "b2 in-flight resident: {BLOBS} objects, {inflated} bytes inflated, a pack of {} bytes; held in the store {before} \
     bytes, held in place {after} bytes ({grown} in the store, {} kept lately)",
    pack.len(),
    placed.kept_lately()
  );
  assert!(
    after <= 2 * pack.len(),
    "held in place: {after} bytes for a {}-byte pack",
    pack.len()
  );
  assert!(
    before >= inflated,
    "held in the store: {before} bytes for {inflated} inflated"
  );
}

/// A clock that moves one tick each read: a quantum of one tick is one unit a turn.
struct EachRead(u64);

impl crate::store::learn::Clock for EachRead {
  fn now_ns(&mut self) -> u64 {
    self.0 += 1;
    self.0
  }
}

/// A-722's landing step (the holds' release out of the landing). Do: admit a captured push's objects in place and put
/// each in the content store as staging does; have the push give back in turns, then land its holds; turn the
/// give-back one unit a turn. Expect: the landing gives back none (the store holds what it held); each turn gives back
/// one object's hold; the last leaves the store as before the puts.
#[test]
fn a_landed_push_gives_its_holds_back_in_turns_not_in_the_landing() {
  let (_, request, before) = PUSHES[0];
  let (pack, held) = captured(request, before);
  let whole = read_whole(&pack, &held);
  let mut store = ContentStore::new(1 << 26, SMALL_WINDOW).unwrap();
  let naming = GitNaming {
    format: ObjectFormat::Sha1,
  };
  let mut push = Push::new(ObjectFormat::Sha1);
  push.extend_pack(&pack);
  let used = store.used();
  for object in &whole.objects {
    push
      .admit_placed::<_, Infallible>(&store, &naming, (incoming(object), object.offset))
      .unwrap();
    push
      .put_placed(&mut store, &object.id, &object.content)
      .unwrap();
  }
  let put = store.used();
  assert!(put > used, "the puts hold the objects");
  push.give_back_in_turns();
  push.release_holds(&mut store).unwrap();
  assert_eq!(store.used(), put, "the landing gives back no hold");
  assert!(push.is_empty(), "the landed push holds no object");
  let mut turns = 0;
  loop {
    let mut clock = EachRead(0);
    let quantum = crate::store::hook_view::Quantum {
      clock: &mut clock,
      ns: 1,
    };
    turns += 1;
    if push.give_back_turn(&mut store, quantum).unwrap() {
      break;
    }
  }
  assert_eq!(store.used(), used, "every hold given back");
  assert!(
    turns >= whole.objects.len(),
    "{turns} turns for {} objects",
    whole.objects.len()
  );
}
