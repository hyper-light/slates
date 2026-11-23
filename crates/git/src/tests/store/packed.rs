//! `store::packed`'s oracle (A-722's memory per push, ruling B part 1): every object of each captured push's pack, read in
//! place from the pack the content store holds, is the object git's reader made of the pack whole.

use std::collections::BTreeMap;
use std::convert::Infallible;

use slates_vcs::{ContentId, ContentStore};

use crate::Admission;
use crate::object_name::{Kind, ObjectFormat, ObjectId};
use crate::store::packed::{Base, Bases, InPack, PackedRefused, Scratch, Waited, read};
use crate::store::{GitNaming, Incoming, Push, Repository, StoredAt};
use crate::unpack::{self, NoIndex, Outside};

/// Shape: a window small enough that the captured packs' entries cross windows (the fixtures' packs are a few hundred
/// bytes to a few kilobytes).
const SMALL_WINDOW: usize = 64;

/// Shape: windows small enough that the `create` push's pack lies in several, large enough for its index rows (A-864 piece 2).
const PIECE2_WINDOW: usize = 256;

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
  let (repository, store, objects, _) = landed_create_in(1 << 12);
  (repository, store, objects)
}

/// [`landed_create`] in a content store of `window`-byte windows, with the pack as pushed.
fn landed_create_in(window: usize) -> (Repository, ContentStore, Vec<unpack::Object>, Vec<u8>) {
  let (_, request, before) = PUSHES[0];
  let (pack, held) = captured(request, before);
  let whole = read_whole(&pack, &held);
  let mut store = ContentStore::new(1 << 26, window).unwrap();
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
  (repository, store, whole.objects, pack)
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

/// `clone` streamed from `repository` within `room` bytes of admission, a turn at a time, bringing back before the next
/// turn the window of `pack` (its bytes, and its identity and length `root`) each turn stopped at (`Upload::waiting`,
/// cut from the pack as the node's store gives it), each stop checked to name a window of `pack` and, where
/// `reads_next` (a stream sending the pack's entries as they lie), to read next in it (`Upload::reading_next`): the
/// pack streamed, and the stops made.
fn streamed_bringing_back(
  repository: &Repository,
  store: &mut ContentStore,
  (clone, room): (super::Answer, u64),
  (pack, root, reads_next): (&[u8], (ContentId, u64), bool),
) -> (Vec<u8>, usize) {
  use crate::store::{Progress, Turn, Upload};
  let mut upload = Upload::new(clone, repository);
  let mut clock = super::Ticks(0);
  let mut admission = Admission::new(room);
  let mut out = Vec::new();
  let mut stops = 0;
  for _ in 0..10_000 {
    let turn = Turn {
      clock: &mut clock,
      exchange: super::EXCHANGE,
      quantum_ns: u64::MAX,
    };
    let progress = upload
      .advance(repository, store, turn, &mut admission, &mut out)
      .unwrap_or_else(|refused| {
        panic!("the same clone within {room} bytes after {stops} window stops: {refused:?}")
      });
    if progress == Progress::Done {
      break;
    }
    let Some(waited) = upload.waiting() else {
      continue;
    };
    stops += 1;
    assert_eq!((waited.pack, waited.len), root);
    assert!(
      !reads_next
        || upload
          .reading_next()
          .is_some_and(|next| next.pack == root.0)
    );
    let (window, start, len) = store
      .away_window(&waited.pack, waited.len, waited.offset)
      .expect("the window met is let go");
    let from = usize::try_from(start).unwrap();
    let to = from + usize::try_from(len).unwrap();
    store.bring_back(&window, &pack[from..to]).unwrap();
  }
  (out, stops)
}

/// A-864 piece 2's invariant (main 01a20182's ruling of 2026-11-22: a turn stops cleanly at a unit boundary, nothing of
/// the unit written, where it meets a let-go window). Do: land the `create` push in a store of 64-byte windows and
/// stream its clone; then let every window of its pack go and stream the clone again, a turn at a time, bringing back
/// the window each turn stopped at (`Upload::waiting`, cut from the pack as the node's store gives it) before the next.
/// Expect: the second clone stopped at least once and is the first byte for byte; each stop named a window of the
/// pack, and where the next turn reads (`Upload::reading_next`) lies in that pack.
#[test]
fn a864_a_turn_that_meets_a_let_go_window_stops_cleanly_and_the_clone_is_byte_identical() {
  let (repository, mut store, _, pack) = landed_create_in(PIECE2_WINDOW);
  let clone = super::Answer::objects(&super::reached_now(&repository, &store, 1));
  let mut measured = Admission::new(u64::MAX);
  let (whole, _) =
    super::streamed_within(&repository, &store, clone.clone(), u64::MAX, &mut measured);
  let room = u64::MAX - measured.left();
  let (resident, _) = super::streamed_within(
    &repository,
    &store,
    clone.clone(),
    u64::MAX,
    &mut Admission::new(room),
  );
  assert_eq!(
    resident, whole,
    "the resident pack fits in {room} bytes of admission"
  );
  let root = repository.packs().next().unwrap();
  for window in pack.chunks(PIECE2_WINDOW).map(ContentId::of) {
    assert!(store.mark_landed(&window));
    store.let_go(&window).unwrap();
  }
  let (out, stops) =
    streamed_bringing_back(&repository, &mut store, (clone, room), (&pack, root, true));
  assert!(stops > 0, "a let-go window met");
  assert!(
    out == whole,
    "the clone byte for byte, {} against {} bytes",
    out.len(),
    whole.len()
  );
}

/// Main 01a20182's r165 (git/git's mirror clone after the push's let-go, 16,270,345 bytes in: `Pack(Store(Unreadable))`,
/// stock git's "possible repository corruption"): every read of an object the stream makes, not only a reused entry's,
/// stops cleanly at a let-go window. Do: land the `create` push in a store of 64-byte windows with one entry's CRC32
/// kept a bit off (`push::MISKEEP_NEXT_CRC`), so that object is read and rebuilt, not sent on as it lies; stream its
/// clone; then let every window of the pack go and stream the clone again, a turn at a time, bringing back each window
/// a turn stopped at. Expect: the second clone stopped at least once and is the first byte for byte. A guard, green
/// before and after the fix: the rebuild's own reads stop already.
#[test]
fn r165_an_object_read_to_be_rebuilt_from_a_let_go_window_stops_and_the_clone_is_byte_identical() {
  crate::store::push::MISKEEP_NEXT_CRC.with(|next| next.set(true));
  let (repository, mut store, _, pack) = landed_create_in(PIECE2_WINDOW);
  let clone = super::Answer::objects(&super::reached_now(&repository, &store, 1));
  let mut measured = Admission::new(u64::MAX);
  let (whole, counters) =
    super::streamed_within(&repository, &store, clone.clone(), u64::MAX, &mut measured);
  assert_eq!(counters.reuse_crc_missed, 1, "{counters:?}");
  let room = u64::MAX - measured.left();
  let root = repository.packs().next().unwrap();
  for window in pack.chunks(PIECE2_WINDOW).map(ContentId::of) {
    assert!(store.mark_landed(&window));
    store.let_go(&window).unwrap();
  }
  let (out, stops) =
    streamed_bringing_back(&repository, &mut store, (clone, room), (&pack, root, true));
  assert!(stops > 0, "a let-go window met");
  assert!(
    out == whole,
    "the clone byte for byte, {} against {} bytes",
    out.len(),
    whole.len()
  );
}

/// Stock git's captured offset-delta pack (`pack_fixtures`, "ofs") landed as a push's in a store of `window`-byte
/// windows, its first commit moved to green 1: the repository, the store, the pack's objects, and the pack.
fn landed_ofs_in(window: usize) -> (Repository, ContentStore, Vec<unpack::Object>, Vec<u8>) {
  let pack = crate::pack_fixtures::packs()[0].bytes.to_vec();
  let whole = read_whole(&pack, &Held(BTreeMap::new()));
  let mut store = ContentStore::new(1 << 26, window).unwrap();
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
  (repository, store, whole.objects, pack)
}

/// Main 01a20182's r165 (git/git's mirror clone after the push's let-go, 16,270,345 bytes in: `Pack(Store(Unreadable))`,
/// stock git's "possible repository corruption"): an object the stream encodes afresh, read from where it lies in its
/// pushed pack, stops cleanly at a let-go window as a reused entry does. Do: land stock git's offset-delta pack in a
/// store of 256-byte windows; answer every object its head reaches but the bases of its deltas (so each such delta is
/// read and encoded afresh, its base not sent); then let every window of the pack go and stream the same answer a turn
/// at a time, bringing back each window a turn stopped at. Expect: the first answer encoded at least one object afresh;
/// the second stopped at least once and is the first byte for byte. Red before: the read refused `Unreadable` mid-pack.
#[test]
fn r165_an_object_encoded_afresh_from_a_let_go_window_stops_and_the_answer_is_byte_identical() {
  let (repository, mut store, objects, pack) = landed_ofs_in(PIECE2_WINDOW);
  let listing = &crate::pack_fixtures::packs()[0].listing;
  let bases: std::collections::BTreeSet<ObjectId> =
    listing.iter().filter_map(|entry| entry.base).collect();
  let positions = objects
    .iter()
    .filter(|object| !bases.contains(&object.id))
    .filter_map(|object| repository.object(&store, &object.id))
    .map(|row| row.position)
    .collect();
  let answer = super::Answer::objects(&crate::store::Positions::of(positions));
  let mut measured = Admission::new(u64::MAX);
  let (whole, counters) =
    super::streamed_within(&repository, &store, answer.clone(), u64::MAX, &mut measured);
  assert!(
    counters.loose_whole + counters.loose_deltas >= 1,
    "an object encoded afresh: {counters:?}"
  );
  let room = u64::MAX - measured.left();
  let root = repository.packs().next().unwrap();
  for window in pack.chunks(PIECE2_WINDOW).map(ContentId::of) {
    assert!(store.mark_landed(&window));
    store.let_go(&window).unwrap();
  }
  let (out, stops) =
    streamed_bringing_back(&repository, &mut store, (answer, room), (&pack, root, true));
  assert!(stops > 0, "a let-go window met");
  assert!(
    out == whole,
    "the answer byte for byte, {} against {} bytes",
    out.len(),
    whole.len()
  );
}

/// Main 01a20182's r165, its path (`parts_copied=2`, `parts_by_entry=4`, `entries_judged=3712`, then `Unreadable`): an
/// entry of a kept part the client cannot read as it lies is made whole from its object where it lies in its pushed
/// pack, and that read stops cleanly at a let-go window. Do: land stock git's offset-delta pack in a store of 256-byte
/// windows and encode its version's fragments (kept parts, which stay resident); answer every object its head reaches
/// but the bases of its deltas, so its parts are read entry by entry and each delta whose base is not sent is made
/// whole; then let every window of the pushed pack go and stream the same answer a turn at a time, bringing back each
/// window a turn stopped at. Expect: the first answer read a part by entry and made an entry whole; the second stopped
/// at least once and is the first byte for byte. Red before: the entry made whole refused `Unreadable` mid-pack.
#[test]
fn r165_an_entry_made_whole_from_a_let_go_window_stops_and_the_answer_is_byte_identical() {
  let (mut repository, mut store, objects, pack) = landed_ofs_in(PIECE2_WINDOW);
  repository
    .encode_fragments(&mut store, (1, 0), &mut super::Ticks(0), super::EXCHANGE)
    .unwrap();
  let listing = &crate::pack_fixtures::packs()[0].listing;
  let bases: std::collections::BTreeSet<ObjectId> =
    listing.iter().filter_map(|entry| entry.base).collect();
  let positions = objects
    .iter()
    .filter(|object| !bases.contains(&object.id))
    .filter_map(|object| repository.object(&store, &object.id))
    .map(|row| row.position)
    .collect();
  let answer = super::Answer::objects(&crate::store::Positions::of(positions));
  let mut measured = Admission::new(u64::MAX);
  let (whole, counters) =
    super::streamed_within(&repository, &store, answer.clone(), u64::MAX, &mut measured);
  assert!(
    counters.parts_by_entry >= 1 && counters.made_whole >= 1,
    "a part read by entry, an entry made whole: {counters:?}"
  );
  let room = u64::MAX - measured.left();
  let root = repository.packs().next().unwrap();
  for window in pack.chunks(PIECE2_WINDOW).map(ContentId::of) {
    assert!(store.mark_landed(&window));
    store.let_go(&window).unwrap();
  }
  let (out, stops) = streamed_bringing_back(
    &repository,
    &mut store,
    (answer, room),
    (&pack, root, false),
  );
  assert!(stops > 0, "a let-go window met");
  assert!(
    out == whole,
    "the answer byte for byte, {} against {} bytes",
    out.len(),
    whole.len()
  );
}

/// A-864 piece 2's encoding side (main 01a20182's ruling of 2026-11-22: a due version's packs never made resident whole).
/// Do: land the `create` push twice in stores of 256-byte windows; encode one's version as it lies; let every window of
/// the other's pack go and encode its version a turn at a time, bringing back the window each turn stopped at
/// (`Encoding::waiting`) before the next; then clone both. Expect: the second encoding stopped at least once, every
/// object sent on as its entry lies in both, and the two clones byte for byte the same. Red before: a unit meeting a
/// let-go window refused the encoding.
#[test]
fn a864_an_encoding_that_meets_a_let_go_window_stops_cleanly_and_keeps_the_same_fragments() {
  use crate::store::{Encoding, Progress, Turn};
  let encoded = |let_go: bool| {
    let (mut repository, mut store, objects, pack) = landed_create_in(PIECE2_WINDOW);
    if let_go {
      for window in pack.chunks(PIECE2_WINDOW).map(ContentId::of) {
        assert!(store.mark_landed(&window));
        store.let_go(&window).unwrap();
      }
    }
    let mut encoding = Encoding::fragments(&repository, &store, 1, 0).unwrap();
    let mut clock = super::Ticks(0);
    let mut stops = 0;
    for _ in 0..100_000 {
      let turn = Turn {
        clock: &mut clock,
        exchange: super::EXCHANGE,
        quantum_ns: u64::MAX,
      };
      if encoding.advance(&mut repository, &mut store, turn).unwrap() == Progress::Done {
        break;
      }
      if let Some(waited) = encoding.waiting() {
        stops += 1;
        let (window, start, len) = store
          .away_window(&waited.pack, waited.len, waited.offset)
          .expect("the window met is let go");
        let from = usize::try_from(start).unwrap();
        let to = from + usize::try_from(len).unwrap();
        store.bring_back(&window, &pack[from..to]).unwrap();
      }
    }
    let counted = encoding.counters();
    let clone = super::Answer::objects(&super::reached_now(&repository, &store, 1));
    let (cloned, _) = super::streamed(&repository, &store, clone, u64::MAX);
    (
      stops,
      counted.reused_whole + counted.reused_deltas,
      crate::to_u64(objects.len()),
      cloned,
    )
  };
  let (_, reused, objects, kept) = encoded(false);
  let (stops, reused_again, _, kept_again) = encoded(true);
  assert!(stops > 0, "a let-go window met");
  assert_eq!((reused, reused_again), (objects, objects));
  assert!(kept_again == kept, "the same clone of the same fragments");
}

/// Main 01a20182's r165 on the encoding side: an object the encoding reads where it lies in its pushed pack, its delta's
/// base in another window, stops cleanly at a let-go window as an entry's header read does. Do: land stock git's
/// offset-delta pack twice in stores of 256-byte windows; encode one's version as it lies; let every window of the
/// other's pack go and encode its version a turn at a time, bringing back the window each turn stopped at
/// (`Encoding::waiting`) before the next; then clone both. Expect: the second encoding stopped at least once, and the
/// two clones byte for byte the same. A guard, green before and after the stream's fix: here the encoding's header
/// reads stop first and bring each window back before its object's read (the object read's own stop is owed a red).
#[test]
fn r165_an_encoding_reading_a_delta_over_let_go_windows_stops_and_keeps_the_same_fragments() {
  use crate::store::{Encoding, Progress, Turn};
  let encoded = |let_go: bool| {
    let (mut repository, mut store, _, pack) = landed_ofs_in(PIECE2_WINDOW);
    if let_go {
      for window in pack.chunks(PIECE2_WINDOW).map(ContentId::of) {
        assert!(store.mark_landed(&window));
        store.let_go(&window).unwrap();
      }
    }
    let mut encoding = Encoding::fragments(&repository, &store, 1, 0).unwrap();
    let mut clock = super::Ticks(0);
    let mut stops = 0;
    for _ in 0..100_000 {
      let turn = Turn {
        clock: &mut clock,
        exchange: super::EXCHANGE,
        quantum_ns: u64::MAX,
      };
      let progress = encoding
        .advance(&mut repository, &mut store, turn)
        .unwrap_or_else(|refused| panic!("the encoding after {stops} stops: {refused:?}"));
      if progress == Progress::Done {
        break;
      }
      if let Some(waited) = encoding.waiting() {
        stops += 1;
        let (window, start, len) = store
          .away_window(&waited.pack, waited.len, waited.offset)
          .expect("the window met is let go");
        let from = usize::try_from(start).unwrap();
        let to = from + usize::try_from(len).unwrap();
        store.bring_back(&window, &pack[from..to]).unwrap();
      }
    }
    let clone = super::Answer::objects(&super::reached_now(&repository, &store, 1));
    let (cloned, _) = super::streamed(&repository, &store, clone, u64::MAX);
    (stops, cloned)
  };
  let (_, kept) = encoded(false);
  let (stops, kept_again) = encoded(true);
  assert!(stops > 0, "a let-go window met");
  assert!(kept_again == kept, "the same clone of the same fragments");
}

/// Main 01a20182's order of 2026-11-23 (item 2: the encoding's object read, a latent `Unreadable`, its red built
/// directly): an encoding past an object's header reads, its windows then let go, then the read. Do: land stock git's
/// offset-delta pack in a store of 256-byte windows, no pushed entry sent on as it lies (`encoding::NO_REUSE`, so each
/// object is read where it lies), and encode its version a unit a turn (a quantum of a nanosecond),
/// nothing let go, until it stands at an object's read where it lies in the pack, nothing of it read
/// (`Encoding::reading_unread_in_pack`); then let every window of the pack go and take the next turn; then bring back
/// each window a turn stops at and encode to the end; then clone. Expect: the turn of the read ends cleanly, stopped at
/// a let-go window (`Encoding::waiting`); the clone is the one an encoding with nothing let go makes, byte for byte.
/// Red before: the read refused the encoding `Unreadable`.
#[test]
fn r165_an_encodings_object_read_past_its_header_reads_stops_at_a_window_let_go_since() {
  use crate::store::{Encoding, Progress, Turn};
  let encoded = |let_go: bool| {
    // No pushed entry sent on as it lies: every object read where it lies, the state the red needs.
    crate::store::encoding::NO_REUSE.with(|off| off.set(true));
    let (mut repository, mut store, _, pack) = landed_ofs_in(PIECE2_WINDOW);
    let mut encoding = Encoding::fragments(&repository, &store, 1, 0).unwrap();
    let mut clock = super::Ticks(0);
    let mut stopped_at_the_read = None;
    for _ in 0..100_000 {
      if let_go && stopped_at_the_read.is_none() && encoding.reading_unread_in_pack() {
        let_go_every_window(&mut store, &pack);
        stopped_at_the_read = Some(false);
      }
      let turn = Turn {
        clock: &mut clock,
        exchange: super::EXCHANGE,
        quantum_ns: 1,
      };
      let progress = encoding
        .advance(&mut repository, &mut store, turn)
        .unwrap_or_else(|refused| panic!("the encoding's read refused: {refused:?}"));
      if progress == Progress::Done {
        break;
      }
      if let Some(waited) = encoding.waiting() {
        if stopped_at_the_read == Some(false) {
          stopped_at_the_read = Some(true);
        }
        bring_back_the_window_met(&mut store, &pack, &waited);
      }
    }
    bring_back_every_window(&mut store, &pack);
    let clone = super::Answer::objects(&super::reached_now(&repository, &store, 1));
    let (cloned, _) = super::streamed(&repository, &store, clone, u64::MAX);
    (stopped_at_the_read, cloned)
  };
  let (_, kept) = encoded(false);
  let (stopped, kept_again) = encoded(true);
  assert_eq!(
    stopped,
    Some(true),
    "the read's turn stopped at a let-go window"
  );
  assert!(kept_again == kept, "the same clone of the same fragments");
}

/// Lets go every `PIECE2_WINDOW` window of `pack` the store still holds, each marked landed first.
fn let_go_every_window(store: &mut ContentStore, pack: &[u8]) {
  for window in pack.chunks(PIECE2_WINDOW).map(ContentId::of) {
    if !store.is_away(&window) {
      assert!(store.mark_landed(&window));
      store.let_go(&window).unwrap();
    }
  }
}

/// Brings back the window of `pack` a turn stopped at (`waited`), from the pack's own bytes.
fn bring_back_the_window_met(store: &mut ContentStore, pack: &[u8], waited: &Waited) {
  let (window, start, len) = store
    .away_window(&waited.pack, waited.len, waited.offset)
    .expect("the window met is let go");
  let from = usize::try_from(start).unwrap();
  let to = from + usize::try_from(len).unwrap();
  store.bring_back(&window, &pack[from..to]).unwrap();
}

/// Brings back every `PIECE2_WINDOW` window of `pack` still let go.
fn bring_back_every_window(store: &mut ContentStore, pack: &[u8]) {
  for window in pack.chunks(PIECE2_WINDOW) {
    let id = ContentId::of(window);
    if store.is_away(&id) {
      store.bring_back(&id, window).unwrap();
    }
  }
}

/// Lets go every window of `windows` the store holds and `brought` does not name: a let-go since a read last met it.
fn let_go_since(
  store: &mut ContentStore,
  windows: &[ContentId],
  brought: &std::collections::BTreeSet<ContentId>,
) {
  for window in windows {
    if !brought.contains(window) && store.contains(window) && !store.is_away(window) {
      assert!(store.mark_landed(window));
      store.let_go(window).unwrap();
    }
  }
}

/// `answer` streamed from `repository` a unit a turn (a quantum of a nanosecond), every window of `pack` not brought back
/// yet let go before each turn, each window a turn stopped at brought back (cut from `pack`): the pack streamed, the
/// stops made, and the counters. A read in any unit meets a window let go since the last.
fn streamed_letting_go(
  repository: &Repository,
  store: &mut ContentStore,
  answer: super::Answer,
  pack: &[u8],
) -> (Vec<u8>, usize, crate::store::UploadCounters) {
  use crate::store::{Progress, Turn, Upload};
  let windows: Vec<ContentId> = pack.chunks(PIECE2_WINDOW).map(ContentId::of).collect();
  let mut brought = std::collections::BTreeSet::new();
  let mut upload = Upload::new(answer, repository);
  let mut clock = super::Ticks(0);
  let mut admission = Admission::new(u64::MAX);
  let mut out = Vec::new();
  let mut stops = 0;
  for _ in 0..100_000 {
    let_go_since(store, &windows, &brought);
    let turn = Turn {
      clock: &mut clock,
      exchange: super::EXCHANGE,
      quantum_ns: 1,
    };
    let progress = upload
      .advance(repository, store, turn, &mut admission, &mut out)
      .unwrap_or_else(|refused| panic!("the stream after {stops} stops: {refused:?}"));
    if progress == Progress::Done {
      break;
    }
    if let Some(waited) = upload.waiting() {
      stops += 1;
      let (window, start, len) = store
        .away_window(&waited.pack, waited.len, waited.offset)
        .expect("the window met is let go");
      let from = usize::try_from(start).unwrap();
      let to = from + usize::try_from(len).unwrap();
      store.bring_back(&window, &pack[from..to]).unwrap();
      brought.insert(window);
    }
  }
  (out, stops, upload.counters())
}

/// The clone of stock git's offset-delta pack landed in 256-byte windows, streamed with nothing let go and then letting
/// go before each turn every window not brought back ([`streamed_letting_go`]): the two packs, the stops, and the
/// resident one's counters.
fn clone_both_ways() -> (Vec<u8>, Vec<u8>, usize, crate::store::UploadCounters) {
  let (repository, store, _, _) = landed_ofs_in(PIECE2_WINDOW);
  let clone = super::Answer::objects(&super::reached_now(&repository, &store, 1));
  let (whole, counters) = super::streamed(&repository, &store, clone, u64::MAX);
  let (repository, mut store, _, pack) = landed_ofs_in(PIECE2_WINDOW);
  let clone = super::Answer::objects(&super::reached_now(&repository, &store, 1));
  let (out, stops, _) = streamed_letting_go(&repository, &mut store, clone, &pack);
  (whole, out, stops, counters)
}

/// Main 01a20182's order of 2026-11-23 (item 2, the stream's delta check, its reds built directly): every pushed entry
/// rebuilt to be checked (`upload::KEPT_OFF`: its raw entry, its base and its object read), the windows those reads need
/// let go since the last turn. Do: clone stock git's offset-delta pack resident, then a unit a turn letting go before each
/// every window not brought back ([`clone_both_ways`]). Expect: the resident clone rebuilt entries and sent deltas on as
/// they lie; the other stopped at least once and is it byte for byte. Red before: a let-go base sent its delta encoded
/// afresh, or the check's read refused `Unreadable`.
#[test]
fn r165_a_streams_entry_check_reads_stop_at_windows_let_go_since_and_the_clone_is_byte_identical() {
  crate::store::upload::KEPT_OFF.with(|off| off.set(true));
  let (whole, out, stops, counters) = clone_both_ways();
  assert!(
    counters.reuse_rebuilt >= 1 && counters.reused_deltas >= 1,
    "entries rebuilt to be checked, deltas sent on: {counters:?}"
  );
  assert!(stops > 0, "a let-go window met");
  assert!(
    out == whole,
    "the clone byte for byte, {} against {} bytes",
    out.len(),
    whole.len()
  );
}

/// Main 01a20182's order of 2026-11-23 (item 2, the stream's encode and search reads, their reds built directly): no
/// pushed entry sent on as it lies (`upload::REUSE_OFF`: each object read where it lies and encoded, one past a window
/// searched against those before it), the windows those reads need let go since the last turn. Do: clone stock git's
/// offset-delta pack resident, then a unit a turn letting go before each every window not brought back
/// ([`clone_both_ways`]). Expect: the resident clone encoded objects afresh, a delta among them; the other stopped at least
/// once and is it byte for byte. Red before: a read refused `Unreadable`.
#[test]
fn r165_a_streams_encode_and_search_reads_stop_at_windows_let_go_since_and_the_clone_is_byte_identical()
 {
  crate::store::upload::REUSE_OFF.with(|off| off.set(true));
  let (whole, out, stops, counters) = clone_both_ways();
  eprintln!("W-GIT-PROBE counters {counters:?}");
  assert!(
    counters.loose_whole + counters.loose_deltas >= 1,
    "objects encoded afresh: {counters:?}"
  );
  assert!(stops > 0, "a let-go window met");
  assert!(
    out == whole,
    "the clone byte for byte, {} against {} bytes",
    out.len(),
    whole.len()
  );
}

/// A-864 piece 2's progress (main 01a20182's r29: git/git's clone stalled after 183 KB on 1930df65d, a stop that never
/// went on). Do: land the `create` push in a store of 256-byte windows, let every window of its pack go, and stream its
/// clone a turn at a time, never bringing a window back, for at most a turn a window and a few. Expect: the stream stops
/// once at the first window it meets, and on the next turn, that window still let go, refuses typed rather than
/// stopping again: a stop always makes progress or ends. Red before: it stopped every turn, never ending.
#[test]
fn a864_a_window_never_brought_back_is_stopped_at_once_then_refused_never_waited_for_again() {
  use crate::store::{Progress, Turn, Upload};
  let (repository, mut store, _, pack) = landed_create_in(PIECE2_WINDOW);
  for window in pack.chunks(PIECE2_WINDOW).map(ContentId::of) {
    assert!(store.mark_landed(&window));
    store.let_go(&window).unwrap();
  }
  let clone = super::Answer::objects(&super::reached_now(&repository, &store, 1));
  let mut upload = Upload::new(clone, &repository);
  let mut clock = super::Ticks(0);
  let mut admission = Admission::new(u64::MAX);
  let mut out = Vec::new();
  let bound = pack.len() / PIECE2_WINDOW + 4;
  let mut stops = 0;
  let mut ended = None;
  for _ in 0..bound {
    let turn = Turn {
      clock: &mut clock,
      exchange: super::EXCHANGE,
      quantum_ns: u64::MAX,
    };
    match upload.advance(&repository, &store, turn, &mut admission, &mut out) {
      Ok(Progress::Done) => {
        ended = Some(Ok(()));
        break;
      }
      Ok(Progress::More) => stops += usize::from(upload.waiting().is_some()),
      Err(refused) => {
        ended = Some(Err(refused));
        break;
      }
    }
  }
  assert_eq!(stops, 1, "stopped once at the first window met");
  assert!(
    matches!(ended, Some(Err(_))),
    "then refused, not waited for: {ended:?}"
  );
}
