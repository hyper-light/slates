//! The Shards route's write-through (§4.6; Shards part 3, step 4; `docs/wip/shards/dax-protocol.md`, guarantee 4) against
//! a model. One file has a DAX object registered in the store, its first range filled. Generated histories mix two kinds
//! of act:
//! - changes through the volume: writes, past the end too, truncates both ways, and SDK edits, whose tail shifts;
//! - a guest's stores straight into the object, as through its map.
//!
//! After every act the object's filled range must hold, byte for byte, what the model says. Each byte is the latest
//! write to it from either side, and zero past the file's end wherever a size change cut or opened it. The volume's
//! bytes must equal the model's file.
//!
//! The model states the protocol's rule, not the implementation's:
//! - a change reaches the object over its filled ranges;
//! - a store stays until a change overwrites it;
//! - a size change zeroes what it cuts or opens;
//! - an edit re-copies what it shifted.
// Test harness code: an unwrap here is a failed test. proptest's strategy macros expand to `Arc`-carrying unions; the test
// harness exception of D-8 covers it.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::indexing_slicing,
  clippy::disallowed_types
)]
#![cfg(unix)]

use proptest::prelude::*;
use slates_mem::{DAX_RANGE_BYTES, DaxObject};
use slates_vfs::ids::InodeNo;
use slates_vfs::shards_backing::ShardsBacking;
use slates_vfs::volume::{Store, Volume};

mod common;

/// `value` as an index, checked.
fn ix(value: u64) -> usize {
  usize::try_from(value).unwrap()
}

/// Shape: the bytes of the object's one filled range, the model's whole object.
fn filled() -> usize {
  ix(DAX_RANGE_BYTES)
}
/// Shape: how far past the filled range an act may reach, so changes cross its end.
const REACH: u64 = 64 * 1024;
/// Shape: the longest single write, store or insert.
const LONGEST: u64 = 16 * 1024;

/// One act of a history.
#[derive(Clone, Debug)]
enum Act {
  /// Bytes written through the volume at `at` (another path: NFS, an SDK client, another attachment).
  Write { at: u64, len: u64, seed: u8 },
  /// Bytes the guest stores through its map at `at`.
  Store { at: u64, len: u64, seed: u8 },
  /// The file truncated, or extended, to `len` through the volume.
  Truncate { len: u64 },
  /// The SDK's edit: `delete` bytes removed at `at` and `insert` bytes put there, the tail shifting.
  Edit {
    at: u64,
    delete: u64,
    insert: u64,
    seed: u8,
  },
}

fn act() -> impl Strategy<Value = Act> {
  let at = 0..DAX_RANGE_BYTES + REACH;
  let len = 1..=LONGEST;
  prop_oneof![
    (at.clone(), len.clone(), any::<u8>()).prop_map(|(at, len, seed)| Act::Write { at, len, seed }),
    (at.clone(), len.clone(), any::<u8>()).prop_map(|(at, len, seed)| Act::Store { at, len, seed }),
    (0..DAX_RANGE_BYTES + REACH).prop_map(|len| Act::Truncate { len }),
    (at, 0..=LONGEST, 0..=LONGEST, any::<u8>()).prop_map(|(at, delete, insert, seed)| Act::Edit {
      at,
      delete,
      insert,
      seed
    }),
  ]
}

/// `len` bytes of a pattern seeded by `seed`, never all zeros, so a store or write is always visible.
fn pattern(len: u64, seed: u8) -> Vec<u8> {
  (0..len).map(|at| (at % 251) as u8 ^ seed | 1).collect()
}

/// The model: the file's bytes, and the object's filled range.
struct Model {
  file: Vec<u8>,
  object: Vec<u8>,
}

impl Model {
  /// Zeroes `from .. to` of the object where it lies in the filled range.
  fn zero_object(&mut self, from: u64, to: u64) {
    let (from, to) = (clip(from), clip(to));
    if from < to {
      self.object[from..to].fill(0);
    }
  }

  /// Writes `bytes` at `at` into the object where they lie in the filled range.
  fn put_object(&mut self, at: u64, bytes: &[u8]) {
    for (offset, byte) in bytes.iter().enumerate() {
      let index = ix(at) + offset;
      if index < filled() {
        self.object[index] = *byte;
      }
    }
  }

  /// The object copied again from the file from `from` on, zeros past its end: an edit's re-copy.
  fn recopy_object(&mut self, from: u64) {
    for index in ix(from)..filled() {
      self.object[index] = self.file.get(index).copied().unwrap_or(0);
    }
  }
}

/// `offset` within the filled range.
fn clip(offset: u64) -> usize {
  ix(offset).min(filled())
}

/// Applies `act` to the volume, the object and the model alike.
fn apply(
  act: &Act,
  volume: &mut Volume,
  store: &mut Store,
  no: InodeNo,
  handle: Handle,
  model: &mut Model,
) {
  match *act {
    Act::Write { at, len, seed } => {
      let bytes = pattern(len, seed);
      volume.write(store, no, at, &bytes).unwrap();
      let old = model.file.len() as u64;
      if at > old {
        model.zero_object(old, at);
        model.file.resize(ix(at), 0);
      }
      let end = ix(at + len);
      if model.file.len() < end {
        model.file.resize(end, 0);
      }
      model.file[ix(at)..end].copy_from_slice(&bytes);
      model.put_object(at, &bytes);
    }
    Act::Store { at, len, seed } => {
      let bytes = pattern(len, seed);
      let fits = (at + len).min(DAX_RANGE_BYTES).saturating_sub(at);
      if fits > 0 {
        store
          .shards_backings
          .get_mut(handle)
          .unwrap()
          .object_mut()
          .store(at, &bytes[..ix(fits)])
          .unwrap();
        model.put_object(at, &bytes[..ix(fits)]);
      }
    }
    Act::Truncate { len } => {
      volume.truncate(store, no, len).unwrap();
      let old = model.file.len() as u64;
      model.zero_object(len.min(old), len.max(old));
      model.file.resize(ix(len), 0);
    }
    Act::Edit {
      at,
      delete,
      insert,
      seed,
    } => {
      let size = model.file.len() as u64;
      if at > size {
        return;
      }
      let bytes = pattern(insert, seed);
      volume.edit(store, no, at, delete, &bytes).unwrap();
      let cut = ix(delete.min(size - at));
      model
        .file
        .splice(ix(at)..ix(at) + cut, bytes.iter().copied());
      model.recopy_object(at);
    }
  }
}

type Handle = slates_mem::Handle<ShardsBacking>;

/// A volume with one file of `size` patterned bytes and a DAX object of it registered, its first range filled: the
/// store, the volume, the file, the backing's handle and the model.
fn backed_file(size: u64) -> (Store, Volume, InodeNo, Handle, Model) {
  let mut store = common::store();
  let mut volume = common::volume(&mut store, 64 << 20);
  let root = volume.root();
  let no = volume
    .create_file(&mut store, root, "mapped", 0o644)
    .unwrap();
  let content = pattern(size, 0x5a);
  volume.write(&mut store, no, 0, &content).unwrap();
  let mut object = DaxObject::create(4).unwrap();
  let held = content.len().min(filled());
  object.store(0, &content[..held]).unwrap();
  let mut backing = ShardsBacking::new(object, volume.prefix(), no);
  backing.mark_filled(0);
  let handle = store.shards_backings.insert(backing).unwrap();
  let mut model_object = vec![0u8; filled()];
  model_object[..held].copy_from_slice(&content[..held]);
  let model = Model {
    file: content,
    object: model_object,
  };
  (store, volume, no, handle, model)
}

/// The object's filled range as it stands.
fn object_bytes(store: &mut Store, handle: Handle) -> Vec<u8> {
  let mut bytes = vec![0u8; filled()];
  store
    .shards_backings
    .get_mut(handle)
    .unwrap()
    .object_mut()
    .load(0, &mut bytes)
    .unwrap();
  bytes
}

/// The volume's bytes of the file.
fn file_bytes(store: &Store, volume: &Volume, no: InodeNo, size: usize) -> Vec<u8> {
  let mut bytes = vec![0u8; size];
  let read = volume.read(store, no, 0, &mut bytes).unwrap();
  bytes.truncate(read);
  bytes
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig {
    cases: slates_test_seeds::cases(16),
    ..ProptestConfig::default()
  }))]

  /// AC (Shards part 3, step 4), the oracle. Do: a generated history of up to 24 acts over a backed 1–3 MiB file. Expect
  /// after every act the object's filled range to equal the model's, so every byte holds the latest write to it from
  /// either side and zeros where a size change cut or opened it, and the volume's bytes to equal the model's file.
  #[test]
  fn every_change_reaches_the_object_and_every_store_stays_until_overwritten(
    size in (1u64 << 20)..(3u64 << 20),
    acts in prop::collection::vec(act(), 1..24),
  ) {
    let (mut store, mut volume, no, handle, mut model) = backed_file(size);
    for (step, act) in acts.iter().enumerate() {
      apply(act, &mut volume, &mut store, no, handle, &mut model);
      let object = object_bytes(&mut store, handle);
      if let Some(first) = object.iter().zip(&model.object).position(|(a, b)| a != b) {
        prop_assert!(
          false,
          "after step {step} ({act:?}) the object differs from the model first at {first}: {} against {}",
          object[first],
          model.object[first]
        );
      }
      let file = file_bytes(&store, &volume, no, model.file.len() + 1);
      prop_assert_eq!(file.len(), model.file.len(), "after step {} the file's size", step);
      prop_assert!(file == model.file, "after step {} ({:?}) the file differs from the model", step, act);
    }
  }
}

/// AC (Shards part 3, step 4), the ingest's guard. Do: register two backings of one file, suppress the first (as an
/// ingest writing its stores into the volume does), write through the volume, then lift the suppression and write
/// again. Expect the suppressed backing untouched by the first write and reached by the second, and the other backing
/// reached by both: a guest's newer store in the source can never be overwritten with what the ingest read.
#[test]
fn a_suppressed_backing_is_passed_over_and_the_others_are_not() {
  let (mut store, mut volume, no, source, _model) = backed_file(1 << 20);
  let mut other = ShardsBacking::new(DaxObject::create(4).unwrap(), volume.prefix(), no);
  other.mark_filled(0);
  let other = store.shards_backings.insert(other).unwrap();
  store.shards_backings.suppress(Some(source));
  volume
    .write(&mut store, no, 100, b"while suppressed")
    .unwrap();
  store.shards_backings.suppress(None);
  assert_ne!(
    &object_bytes(&mut store, source)[100..116],
    b"while suppressed"
  );
  assert_eq!(
    &object_bytes(&mut store, other)[100..116],
    b"while suppressed"
  );
  volume.write(&mut store, no, 200, b"after").unwrap();
  assert_eq!(&object_bytes(&mut store, source)[200..205], b"after");
  assert_eq!(&object_bytes(&mut store, other)[200..205], b"after");
}

/// A-163, the volume a change is of. A clone shares its origin's nodes, and so its inode numbers
/// (`Volume::clone_of`); a snapshot a device presents is such a clone, in the same store. Do: back a file of the origin,
/// as a guest of the origin maps it; clone a snapshot of the origin into the same store; write the clone's copy of the
/// file, truncate it and extend it again. Expect the origin's backing untouched by all three, and still reached by a
/// write of the origin's own file: a change of one volume never reaches a guest's map of another.
#[test]
fn a_change_of_a_clone_never_reaches_a_backing_of_its_origin() {
  let (mut store, mut origin, no, handle, _model) = backed_file(1 << 20);
  let snapshot = origin.snapshot(&mut store).unwrap();
  let mut clone = Volume::clone_of(&store, &mut origin, snapshot, common::clone_config(8)).unwrap();
  let before = object_bytes(&mut store, handle);
  clone
    .write(&mut store, no, 100, b"the clone's own bytes")
    .unwrap();
  assert!(
    object_bytes(&mut store, handle) == before,
    "a write of the clone's file reached the origin's map"
  );
  clone.truncate(&mut store, no, 4096).unwrap();
  clone.truncate(&mut store, no, 1 << 20).unwrap();
  assert!(
    object_bytes(&mut store, handle) == before,
    "a size change of the clone's file reached the origin's map"
  );
  origin.write(&mut store, no, 200, b"origin").unwrap();
  assert_eq!(
    &object_bytes(&mut store, handle)[200..206],
    b"origin",
    "non-vacuity: the origin's own write reaches its map"
  );
}

/// AC (Shards part 3, step 4), the size bound. Do: on a backed file, write and truncate past the object's size; and on a
/// file with no backing, write past it. Expect the backed file's two refused `FileTooLarge` with nothing changed, and
/// the unbacked file's not refused for the object's sake.
#[test]
fn a_change_past_the_objects_size_is_refused_only_for_a_backed_file() {
  let (mut store, mut volume, no, _handle, model) = backed_file(1 << 20);
  let past = slates_mem::DAX_OBJECT_BYTES;
  assert!(matches!(
    volume.write(&mut store, no, past - 1, b"xy"),
    Err(slates_vfs::error::VfsError::FileTooLarge)
  ));
  assert!(matches!(
    volume.truncate(&mut store, no, past + 1),
    Err(slates_vfs::error::VfsError::FileTooLarge)
  ));
  assert_eq!(
    file_bytes(&store, &volume, no, model.file.len() + 1),
    model.file
  );
  let root = volume.root();
  let plain = volume
    .create_file(&mut store, root, "plain", 0o644)
    .unwrap();
  assert!(!matches!(
    volume.truncate(&mut store, plain, past + 1),
    Err(slates_vfs::error::VfsError::FileTooLarge)
  ));
}

// --- Beneath an overlay (D-25): the disk can change a backed file without a write. ---

/// Shape: the large-file class boundary of these overlays, one chunk window, so the backed file is large class and its
/// bytes are read from the disk's file through the volume's descriptor.
const LARGE_CLASS: u64 = 65_536;

fn overlay(host: &mut slates_vfs::host::sim::SimHost, store: &mut Store) -> Volume {
  use slates_vfs::host::HostFs;
  let root = host.root();
  let facts = host.facts(root).unwrap();
  Volume::create_overlay(
    store,
    slates_vfs::volume::VolumeConfig {
      prefix: 7,
      names: slates_vfs::names::NameEquivalence::Exact,
      quota: slates_vfs::quota::Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 20,
      clock: Box::new(slates_vfs::clock::StepClock::new(1_000_000, 1_000)),
    },
    slates_vfs::base::BaseConfig {
      root,
      facts,
      large_class_bytes: LARGE_CLASS,
    },
  )
  .unwrap()
}

/// A backing of file `no` of the volume of prefix `volume` registered in the store, its first range filled: as a guest's
/// read-only map leaves it.
fn back(store: &mut Store, volume: u16, no: InodeNo) -> Handle {
  let mut backing = ShardsBacking::new(DaxObject::create(4).unwrap(), volume, no);
  backing.mark_filled(0);
  store.shards_backings.insert(backing).unwrap()
}

/// Takes `handle`'s stale mark, as a refill does: whether it had one.
fn take_stale(store: &mut Store, handle: Handle) -> bool {
  store.shards_backings.get_mut(handle).unwrap().take_stale()
}

/// An overlay over a simulated host holding `/big`, a large-class file, and its inode, looked up and observed once.
fn overlay_with_big() -> (slates_vfs::host::sim::SimHost, Store, Volume, InodeNo) {
  let mut host = slates_vfs::host::sim::SimHost::new();
  host.replace_file("/big", &pattern(1 << 20, 0x21));
  let mut store = common::store();
  let mut vol = overlay(&mut host, &mut store);
  let no = {
    let mut o = vol.with_host(&mut host);
    let no = o.resolve(&mut store, "/big").unwrap().inode;
    o.stat(&mut store, no).unwrap();
    no
  };
  (host, store, vol, no)
}

/// AC (Shards part 3, step 4), beneath an overlay. Do: back a base file no one has written; observe it with the disk
/// unchanged; edit it in place on the host and observe it again. Expect no stale mark while the disk stands still, and
/// a mark, counted, once the edit is adopted (the untouched file follows the disk, so its maps must be refilled), taken
/// once. The simulated watcher's hint has the directory's listing read again, so this is the listing's adoption; the
/// descriptor's own `fstat` adopting it is the server's test over a real disk
/// (`a_read_only_map_of_an_untouched_base_file_shows_an_outsiders_edit_before_the_reply_that_reveals_it`).
#[test]
fn an_outsiders_edit_adopted_marks_an_untouched_files_backings_stale_and_a_still_disk_does_not() {
  let (mut host, mut store, mut vol, no) = overlay_with_big();
  let handle = back(&mut store, vol.prefix(), no);
  vol.with_host(&mut host).stat(&mut store, no).unwrap();
  assert!(
    !take_stale(&mut store, handle),
    "a still disk marks nothing"
  );
  assert_eq!(store.shards_backings.marked_stale(), 0);
  host.write_in_place("/big", &pattern(1 << 20, 0x5c), 1_000);
  vol.with_host(&mut host).stat(&mut store, no).unwrap();
  assert_eq!(store.shards_backings.marked_stale(), 1);
  assert!(
    take_stale(&mut store, handle),
    "the adopted edit marks the backing"
  );
  assert!(!take_stale(&mut store, handle), "a mark is taken once");
}

/// AC (Shards part 3, step 4), beneath an overlay. Do: copy a base file up for a writable map
/// (`Overlay::witness_content`), back it, then edit it in place on the host, observe and read it. Expect no stale mark:
/// a witnessed file never follows the disk. The read is refused `BaseDrift` rather than serve bytes the guest's map
/// does not hold, and the edit is drift.
#[test]
fn a_file_witnessed_for_a_writable_map_is_never_changed_beneath_it_by_the_disk() {
  let (mut host, mut store, mut vol, no) = overlay_with_big();
  vol
    .with_host(&mut host)
    .witness_content(&mut store, no)
    .unwrap();
  let handle = back(&mut store, vol.prefix(), no);
  host.write_in_place("/big", &pattern(1 << 20, 0x5c), 1_000);
  let mut o = vol.with_host(&mut host);
  o.stat(&mut store, no).unwrap();
  let mut buf = vec![0u8; 4096];
  assert_eq!(
    o.read(&mut store, no, 0, &mut buf),
    Err(slates_vfs::error::VfsError::BaseDrift)
  );
  assert!(!o.status(&mut store).unwrap().drift.is_empty());
  drop(o);
  assert!(!take_stale(&mut store, handle));
  assert_eq!(store.shards_backings.marked_stale(), 0);
}

/// AC (Shards part 3, step 4), `rewitness` beneath a map. Do: witness a base file, drift it on the host, and back it
/// once writable; rewitness every drifted entry; then make the backing read-only and rewitness again. Expect the first
/// refused `DaxMapped` with the drift still recorded, since a refill would lose the guest's unsynced stores; the
/// second to succeed and mark the backing stale, since the entry's bytes now come from the disk's file as it is.
#[test]
fn rewitness_refuses_a_file_mapped_writable_and_marks_one_mapped_to_read() {
  let (mut host, mut store, mut vol, no) = overlay_with_big();
  vol
    .with_host(&mut host)
    .witness_content(&mut store, no)
    .unwrap();
  host.write_in_place("/big", &pattern(1 << 20, 0x5c), 1_000);
  let mut buf = vec![0u8; 4096];
  let _ = vol.with_host(&mut host).read(&mut store, no, 0, &mut buf);
  let mut writable = ShardsBacking::new(DaxObject::create(4).unwrap(), vol.prefix(), no);
  writable.mark_filled(0);
  writable.mark_writable(0);
  let writable = store.shards_backings.insert(writable).unwrap();
  assert_eq!(
    vol.with_host(&mut host).rewitness(&mut store, None),
    Err(slates_vfs::error::VfsError::DaxMapped)
  );
  assert!(
    !vol
      .with_host(&mut host)
      .status(&mut store)
      .unwrap()
      .drift
      .is_empty(),
    "a refused rewitness changes nothing"
  );
  store.shards_backings.remove(writable);
  let handle = back(&mut store, vol.prefix(), no);
  assert_eq!(
    vol
      .with_host(&mut host)
      .rewitness(&mut store, None)
      .unwrap()
      .len(),
    1
  );
  assert!(take_stale(&mut store, handle));
}

/// AC (Shards part 3, step 4), the routes exclude each other. Do: on a volume that allows writable DAX, back a file for
/// a Shards guest and map it writable in process; let the backing go and map it again. Expect the first refused
/// `DaxMapped`, an in-process guest's stores would bypass the write boundary the backing relies on, and the second to
/// succeed with the registry reporting the file mapped writable.
#[test]
fn an_in_process_writable_map_and_a_shards_backing_exclude_each_other() {
  let mut store = common::store();
  let mut volume = common::volume(&mut store, 64 << 20);
  volume.enable_writable_dax();
  let root = volume.root();
  let no = volume
    .create_file(&mut store, root, "mapped", 0o644)
    .unwrap();
  volume
    .write(&mut store, no, 0, &pattern(4096, 0x13))
    .unwrap();
  let handle = back(&mut store, volume.prefix(), no);
  assert!(matches!(
    volume.dax_map_write(&mut store, no),
    Err(slates_vfs::error::VfsError::DaxMapped)
  ));
  assert!(!store.dax.has_writable_of(no));
  store.shards_backings.remove(handle);
  volume.dax_map_write(&mut store, no).unwrap();
  assert!(store.dax.has_writable_of(no));
}

/// Measured, not a gate (Shards part 3, step 4): what write-through adds to a write. Do: write 1 MiB into a file in
/// 64 KiB writes, once into a file no guest maps and once into a file with one backing whose range is filled, the two
/// interleaved round by round, 25 rounds each; print every round and the best of each. Run by hand in release:
/// `cargo test --release -p slates-vfs --test shards_backing -- --ignored write_through_cost --nocapture`.
#[test]
#[ignore = "a measurement, run by hand in release"]
fn write_through_cost_per_mib() {
  /// Shape: the rounds of each kind, interleaved.
  const ROUNDS: usize = 25;
  /// Shape: the bytes of one write, a FUSE write's 64 KiB.
  const PIECE: usize = 64 * 1024;
  let mut store = common::store();
  let mut volume = common::volume(&mut store, 64 << 20);
  let root = volume.root();
  let bytes = pattern(1 << 20, 0x3c);
  let plain = volume
    .create_file(&mut store, root, "plain", 0o644)
    .unwrap();
  let backed = volume
    .create_file(&mut store, root, "backed", 0o644)
    .unwrap();
  volume.write(&mut store, plain, 0, &bytes).unwrap();
  volume.write(&mut store, backed, 0, &bytes).unwrap();
  let mut backing = ShardsBacking::new(DaxObject::create(4).unwrap(), volume.prefix(), backed);
  backing.mark_filled(0);
  store.shards_backings.insert(backing).unwrap();
  let mut write_mib = |no: InodeNo, seed: u8| {
    let started = std::time::Instant::now();
    for (index, chunk) in bytes.chunks(PIECE).enumerate() {
      let mut piece = chunk.to_vec();
      piece[0] = seed;
      let at = u64::try_from(index * PIECE).unwrap();
      volume.write(&mut store, no, at, &piece).unwrap();
    }
    started.elapsed().as_nanos()
  };
  let (mut plain_ns, mut backed_ns) = (Vec::new(), Vec::new());
  for round in 0..ROUNDS {
    let seed = u8::try_from(round % 251).unwrap() | 1;
    plain_ns.push(write_mib(plain, seed));
    backed_ns.push(write_mib(backed, seed));
  }
  println!(
    "plain_ns_per_mib\t{}\truns={plain_ns:?}",
    plain_ns.iter().min().unwrap()
  );
  println!(
    "backed_ns_per_mib\t{}\truns={backed_ns:?}",
    backed_ns.iter().min().unwrap()
  );
  assert!(store.shards_backings.written() > 0, "write-through ran");
}

/// A same-kernel client's copy of the file `no` of `volume` (A-171), the file's page span, every range of it filled as
/// the direct map's fill leaves it, with `content` stored: the backing's handle.
fn client_copy(store: &mut Store, volume: &Volume, no: InodeNo, content: &[u8]) -> Handle {
  let size = u64::try_from(content.len()).unwrap();
  let budget = slates_mem::PeerObject::bytes_for(size)
    .unwrap()
    .saturating_mul(4);
  store.shards_backings.set_budget(budget);
  let mut object = slates_mem::PeerObject::create("slates-vfs-test", size).unwrap();
  object.store(0, content).unwrap();
  let handle = store
    .shards_backings
    .insert_peer(object, volume.prefix(), no)
    .unwrap();
  let ranges = size.div_ceil(DAX_RANGE_BYTES).max(1);
  let backing = store.shards_backings.get_mut(handle).unwrap();
  for range in 0..ranges {
    backing.mark_filled(range);
  }
  handle
}

/// A client copy's bytes, the whole object.
fn copy_bytes(store: &mut Store, handle: Handle) -> Vec<u8> {
  let backing = store.shards_backings.get_mut(handle).unwrap();
  let mut bytes = vec![0u8; ix(backing.object().len())];
  backing.object_mut().load(0, &mut bytes).unwrap();
  bytes
}

/// A-171, the write-through's bound for a client's copy. Do: copy a file of three pages less a few bytes, then write
/// across the copy's end and truncate the file to one byte. Expect the part of the write inside the copy in it, the rest
/// nowhere (the copy is the file's page span when it was made), and the truncate's cut zero in the copy.
#[test]
fn a_client_copy_takes_what_falls_inside_it_and_nothing_past_it() {
  let page = u64::try_from(rustix::param::page_size()).unwrap();
  let mut store = common::store();
  let mut volume = common::volume(&mut store, 64 << 20);
  let root = volume.root();
  let no = volume.create_file(&mut store, root, "f", 0o644).unwrap();
  let content = pattern(3 * page - 5, 0x31);
  volume.write(&mut store, no, 0, &content).unwrap();
  let handle = client_copy(&mut store, &volume, no, &content);
  let across = pattern(2 * page, 0x77);
  volume.write(&mut store, no, 2 * page, &across).unwrap();
  let copy = copy_bytes(&mut store, handle);
  assert_eq!(
    copy.len(),
    ix(3 * page),
    "the copy is the page span it was made with"
  );
  assert!(
    copy[ix(2 * page)..] == across[..ix(page)],
    "the write's part inside the copy"
  );
  volume.truncate(&mut store, no, 1).unwrap();
  let copy = copy_bytes(&mut store, handle);
  assert_eq!(copy[0], content[0]);
  assert!(
    copy[1..].iter().all(|byte| *byte == 0),
    "the cut reads zero in the copy"
  );
}

/// A-171 (G-direct step 2), a destroyed volume's copies. Do: copy a file for a client and back it for a VMM too, detach
/// the volume's client copies as its teardown does, write the file, then remove the client copy. Expect the detach to
/// count one; the write to reach the VMM's backing and not the client's copy, which a volume given the prefix again
/// would otherwise write into; the client copy held and counted until its removal, and given back with it.
#[test]
fn a_detached_client_copy_is_written_no_more_and_held_until_removed() {
  let (mut store, mut volume, no, vmm, _model) = backed_file(1 << 20);
  let content = file_bytes(&store, &volume, no, 1 << 20);
  let copy = client_copy(&mut store, &volume, no, &content);
  let held = store.shards_backings.peers_held();
  let charged = store.shards_backings.charged();
  assert_eq!(held.0, 1);
  assert_eq!(
    store.shards_backings.detach_volume_peers(volume.prefix()),
    1
  );
  volume
    .write(&mut store, no, 100, b"after the detach")
    .unwrap();
  assert_eq!(
    &object_bytes(&mut store, vmm)[100..116],
    b"after the detach",
    "a VMM's backing is not the teardown's to detach"
  );
  assert!(
    copy_bytes(&mut store, copy)[100..116] == content[100..116],
    "the detached copy is written no more"
  );
  assert_eq!(
    store.shards_backings.peers_held(),
    held,
    "held until removed"
  );
  assert_eq!(store.shards_backings.charged(), charged, "and counted");
  assert!(store.shards_backings.remove(copy).is_some());
  assert_eq!(store.shards_backings.peers_held(), (0, 0));
  assert_eq!(
    store.shards_backings.charged(),
    charged - held.1,
    "the removal gives its bytes back"
  );
}

/// A-171 (G-direct step 2), a removed file's copies. Do: copy a file for a client, then unlink it while nothing holds it
/// open, which reclaims it. Expect the file to have no backing in the index any more (`backs` answers false) while the
/// copy stays held until its client lets it go.
#[test]
fn a_reclaimed_files_client_copy_is_detached_and_still_held() {
  let mut store = common::store();
  let mut volume = common::volume(&mut store, 64 << 20);
  let root = volume.root();
  let no = volume.create_file(&mut store, root, "f", 0o644).unwrap();
  let content = pattern(64 << 10, 0x42);
  volume.write(&mut store, no, 0, &content).unwrap();
  let copy = client_copy(&mut store, &volume, no, &content);
  assert!(store.shards_backings.backs(volume.prefix(), no));
  volume.unlink(&mut store, root, "f").unwrap();
  assert!(
    !store.shards_backings.backs(volume.prefix(), no),
    "the reclaimed file's copy left the index"
  );
  assert_eq!(store.shards_backings.peers_held().0, 1, "and stays held");
  let held = copy_bytes(&mut store, copy);
  assert!(
    held[..content.len()] == content[..] && held[content.len()..].iter().all(|byte| *byte == 0),
    "with the file's last bytes"
  );
}
