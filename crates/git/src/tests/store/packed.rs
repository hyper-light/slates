//! `store::packed`'s oracle (A-722's memory per push, ruling B part 1): every object of each captured push's pack, read in
//! place from the pack the content store holds, is the object git's reader made of the pack whole.

use std::collections::BTreeMap;

use slates_vcs::ContentStore;

use crate::Admission;
use crate::object_name::{Kind, ObjectFormat, ObjectId};
use crate::store::packed::{Base, Bases, InPack, PackedRefused, Scratch, read};
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
