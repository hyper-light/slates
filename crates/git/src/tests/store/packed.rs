//! `store::packed`'s oracle (A-722's memory per push, ruling B part 1): every object of each captured push's pack, read in
//! place from the pack the content store holds, is the object git's reader made of the pack whole.

use std::collections::BTreeMap;
use std::convert::Infallible;

use slates_vcs::{ContentId, ContentStore};

use crate::Admission;
use crate::object_name::{Kind, ObjectFormat, ObjectId};
use crate::store::packed::{Base, Bases, InPack, PackedRefused, Scratch, read};
use crate::store::{GitNaming, Incoming, Push, Repository, StoredAt};
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

/// A-722's record v4, rows by pack place (main's ruling: content once, the store's bytes a push must not grow). Do: push
/// the captured `create` push into an empty repository as a live push does, its objects held where they lie in its
/// pack; seal, stage and land it, moving a green to its commit. Expect: every object read back through the repository
/// as the pack makes it, from where it lies in the pack its staging put in the store; the store grown by less than the
/// objects inflated, the pack once and its rows.
#[test]
fn a_landed_push_is_read_where_it_lies_in_its_stored_pack() {
  let (_, request, before) = PUSHES[0];
  let (pack, held) = captured(request, before);
  assert!(held.0.is_empty(), "the create push brings every object");
  let whole = read_whole(&pack, &held);
  let mut store = ContentStore::new(1 << 26, 1 << 12).unwrap();
  let mut repository = Repository::new(ObjectFormat::Sha1);
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
  }
  repository.seal(&store, &mut push).unwrap();
  repository.stage(&mut store, &mut push).unwrap();
  let commit = whole
    .objects
    .iter()
    .find(|object| object.kind == Kind::Commit)
    .unwrap()
    .id;
  let moved = repository.admit(&store, &push, 1, Some(commit)).unwrap();
  repository.commit(&mut store, &mut push, &[moved]).unwrap();
  super::derive_all(&repository, &mut store);
  push.abandon(&mut store).unwrap();
  for object in &whole.objects {
    let (kind, stored, len) = repository.stored(&store, &object.id).unwrap();
    assert!(
      matches!(stored, StoredAt::InPack(_)),
      "{} held whole",
      object.id
    );
    let read = repository.read_stored(&store, &stored, len).unwrap();
    assert_eq!(
      (kind, read.as_ref()),
      (object.kind, object.content.as_slice())
    );
  }
  let inflated: usize = whole
    .objects
    .iter()
    .map(|object| object.content.len())
    .sum();
  let grown = usize::try_from(store.used() - used).unwrap();
  println!(
    "rows by pack place: {} objects, {inflated} bytes inflated, a {}-byte pack; the store grew by {grown}",
    whole.objects.len(),
    pack.len()
  );
  assert!(
    grown < inflated,
    "the store grew by {grown} for {inflated} inflated"
  );
}

/// Goal 93, item 2a (main 01a20182's ruling of 2026-11-22 05:45: a home's RAM copy may go once its node's store holds
/// the pack, brought back by identity on a read). Do: hold the "update" push's pack in a content store of 64-byte windows,
/// mark every window landed and let each go; read every object in place, and at each read refused `Away`, bring back the
/// window it names (its bytes cut from the pack, as the node's store gives them) and read again. Expect: every read of a
/// let-go window refused `Away` naming the pack, its length and an offset inside a window let go, never `Unread`; and
/// each object, once its windows are back, the object read whole. Failing first: `Unread`, nothing named to bring back.
#[test]
fn a_read_of_a_let_go_window_names_it_and_reads_once_it_is_brought_back() {
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
  // A window is named by the plain BLAKE3 of its bytes, so the pack's cuts name its windows.
  let windows: Vec<_> = pack.chunks(SMALL_WINDOW).map(ContentId::of).collect();
  for window in &windows {
    assert!(store.mark_landed(window));
    store.let_go(window).unwrap();
  }
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
  let mut scratch = Scratch::default();
  let mut out = Vec::new();
  let mut brought = 0;
  for object in &whole.objects {
    let place = InPack {
      pack: root,
      len,
      offset: object.offset,
    };
    let kind = loop {
      match read(
        &store,
        (place, ObjectFormat::Sha1),
        &bases,
        (&mut scratch, ROOM),
        &mut out,
      ) {
        Ok(kind) => break kind,
        Err(refused) => {
          let waited = refused
            .waited()
            .unwrap_or_else(|| panic!("at {}: {refused:?}, not Away", object.offset));
          assert_eq!((waited.pack, waited.len), (root, len));
          let (window, start, window_len) = store
            .away_window(&waited.pack, waited.len, waited.offset)
            .expect("the offset met lies in a window let go");
          let from = usize::try_from(start).unwrap();
          let to = from + usize::try_from(window_len).unwrap();
          store.bring_back(&window, &pack[from..to]).unwrap();
          brought += 1;
        }
      }
    };
    assert_eq!((kind, &out), (object.kind, &object.content));
  }
  assert!(brought > 0, "no read met a let-go window");
}

/// The captured `create` push landed in an empty repository as a live push lands it, its objects held where they lie in
/// its stored pack, every object's row keeping its entry (A-864): the repository, its store and the push's objects.
fn landed_create() -> (Repository, ContentStore, Vec<unpack::Object>) {
  let (_, request, before) = PUSHES[0];
  let (pack, held) = captured(request, before);
  let whole = read_whole(&pack, &held);
  let mut store = ContentStore::new(1 << 26, 1 << 12).unwrap();
  let mut repository = Repository::new(ObjectFormat::Sha1);
  let naming = GitNaming {
    format: ObjectFormat::Sha1,
  };
  let mut push = Push::new(ObjectFormat::Sha1);
  push.extend_pack(&pack);
  for object in &whole.objects {
    push
      .admit_placed::<_, Infallible>(&store, &naming, (incoming(object), object.offset))
      .unwrap();
  }
  repository.seal(&store, &mut push).unwrap();
  repository.stage(&mut store, &mut push).unwrap();
  let commit = whole
    .objects
    .iter()
    .find(|object| object.kind == Kind::Commit)
    .unwrap()
    .id;
  let moved = repository.admit(&store, &push, 1, Some(commit)).unwrap();
  repository.commit(&mut store, &mut push, &[moved]).unwrap();
  super::derive_all(&repository, &mut store);
  push.abandon(&mut store).unwrap();
  (repository, store, whole.objects)
}

/// The objects of `pack`, read whole as `index-pack` reads one, by name with their bytes.
fn objects_of(pack: &[u8]) -> BTreeMap<ObjectId, Vec<u8>> {
  read_whole(pack, &Held(BTreeMap::new()))
    .objects
    .into_iter()
    .map(|object| (object.id, object.content))
    .collect()
}

/// A-864 (main 01a20182's r17, 2026-11-22 07:08: git/git's clone reused 147,449 entries in 120 s, each rebuilt to be
/// checked, inflated, its delta applied and named; ruled 07:31: checked by the CRC32 git's pack index keeps). Do: land
/// stock git's captured `create` push and stream its clone before any version is encoded. Expect: every object sent on
/// as its entry lies, none rebuilt and no CRC32 missed; the clone's objects the push's, each once. Red before: each
/// reused entry rebuilt.
#[test]
fn a864_a_clone_sends_each_pushed_entry_on_by_its_kept_crc_rebuilding_none() {
  let (repository, store, objects) = landed_create();
  let clone = super::Answer::objects(&super::reached_now(&repository, &store, 1));
  let (pack, counters) = super::streamed(&repository, &store, clone, u64::MAX);
  let sent = counters.reused_whole + counters.reused_deltas;
  assert_eq!(
    (sent, counters.reuse_rebuilt, counters.reuse_crc_missed),
    (crate::to_u64(objects.len()), 0, 0),
    "{counters:?}"
  );
  let expected: BTreeMap<ObjectId, Vec<u8>> = objects
    .into_iter()
    .map(|object| (object.id, object.content))
    .collect();
  assert_eq!(objects_of(&pack), expected);
}

/// A-864's hostile red (main 01a20182's ruling of 2026-11-22 07:31: a reused entry whose raw bytes do not match its kept
/// CRC32 is rebuilt, as git's `write_no_reuse_object` is). Do: land the `create` push with its first entry's kept CRC32
/// one bit off its raw bytes (`push::MISKEEP_NEXT_CRC`), as a bit flipped in its stored bytes would leave it, then stream
/// its clone. Expect: one CRC32 missed and that one object rebuilt to be checked, every other sent on by its CRC32; the
/// clone's objects the push's, each once, byte for byte.
#[test]
fn a864_an_entry_whose_crc_misses_is_rebuilt_and_the_clone_is_whole() {
  crate::store::push::MISKEEP_NEXT_CRC.with(|next| next.set(true));
  let (repository, store, objects) = landed_create();
  let clone = super::Answer::objects(&super::reached_now(&repository, &store, 1));
  let (pack, counters) = super::streamed(&repository, &store, clone, u64::MAX);
  assert_eq!(
    (counters.reuse_crc_missed, counters.reuse_rebuilt),
    (1, 1),
    "{counters:?}"
  );
  let expected: BTreeMap<ObjectId, Vec<u8>> = objects
    .into_iter()
    .map(|object| (object.id, object.content))
    .collect();
  assert_eq!(objects_of(&pack), expected);
}
