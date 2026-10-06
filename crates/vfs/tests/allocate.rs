//! A-108, `fallocate` mode 0 at the volume: `admit_allocation` charges a range's holes all or none, and `allocate`
//! materializes them with zeros, never touching a held byte. These tests drive the volume and an overlay over the
//! simulated host and assert what a caller observes: the bytes read back, the size, the charge, and which later writes
//! the quota still admits.
// Test harness code: an unwrap here is a failed test, which is what it should be. proptest's strategy types carry
// `Arc` (D-8's harness exception).
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::disallowed_types,
  clippy::indexing_slicing
)]

mod common;

use common::{PAGE, store, volume};
use proptest::prelude::*;
use slates_vfs::InodeNo;
use slates_vfs::base::BaseConfig;
use slates_vfs::clock::StepClock;
use slates_vfs::error::VfsError;
use slates_vfs::host::HostFs;
use slates_vfs::host::sim::SimHost;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, Volume, VolumeConfig};

/// The whole of file `no`, read back.
fn contents(vol: &Volume, store: &Store, no: InodeNo) -> Vec<u8> {
  let size = vol.stat(store, no).unwrap().size;
  let mut back = vec![0u8; usize::try_from(size).unwrap()];
  let n = vol.read(store, no, 0, &mut back).unwrap();
  back.truncate(n);
  back
}

/// Allocates `[off, end)` of `no` as the FUSE channel does: admitted whole, then materialized `slice` bytes at a time.
fn allocate_in_slices(
  vol: &mut Volume,
  store: &mut Store,
  no: InodeNo,
  (off, end): (u64, u64),
  slice: u64,
) -> Result<(), VfsError> {
  vol.admit_allocation(store, no, off, end)?;
  let mut next = off;
  while next < end {
    let stop = end.min(next.saturating_add(slice));
    vol.allocate(store, no, next, stop)?;
    next = stop;
  }
  Ok(())
}

/// A-108. Do: write two islands into a file of a bounded volume, allocate six windows over them in page slices, fill
/// the rest of the quota with another file, then overwrite the whole allocated range. Expect: the islands read back
/// unchanged and every other byte reads zero; the size is the range's end; with the quota full, a new window anywhere
/// else is refused `NoSpace` while the overwrite of the allocated range is admitted whole: the promise
/// `posix_fallocate` makes.
#[test]
fn an_allocated_range_keeps_its_bytes_and_takes_a_later_write_with_the_quota_full() {
  let mut store = store();
  let chunk = u64::try_from(store.content.chunk_bytes()).unwrap();
  let windows = 6;
  let quota = (windows + 2) * chunk;
  let mut vol = volume(&mut store, quota);
  let root = vol.root_inode(&store).unwrap();
  let f = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, f, 0, b"head").unwrap();
  vol.write(&mut store, f, 3 * chunk + 7, b"island").unwrap();
  let page = u64::try_from(PAGE).unwrap();
  allocate_in_slices(&mut vol, &mut store, f, (0, windows * chunk), page).unwrap();

  let mut expected = vec![0u8; usize::try_from(windows * chunk).unwrap()];
  expected[..4].copy_from_slice(b"head");
  let island = usize::try_from(3 * chunk + 7).unwrap();
  expected[island..island + 6].copy_from_slice(b"island");
  assert_eq!(
    contents(&vol, &store, f),
    expected,
    "held bytes kept, holes zero, size the range's end"
  );

  let filler = vol.create_file_no(&mut store, root, "fill", 0o644).unwrap();
  let mut at = 0;
  while vol
    .write(
      &mut store,
      filler,
      at,
      &vec![b'f'; usize::try_from(chunk).unwrap()],
    )
    .is_ok()
  {
    at += chunk;
  }
  assert_eq!(
    vol.write(&mut store, filler, at, b"x"),
    Err(VfsError::NoSpace),
    "the quota is full: a new window is refused"
  );
  let overwrite = vec![b'w'; usize::try_from(windows * chunk).unwrap()];
  assert_eq!(
    vol.write(&mut store, f, 0, &overwrite),
    Ok(overwrite.len()),
    "a write into the allocated range never fails for space"
  );
}

/// A-108. Do: allocate a range larger than a bounded volume's quota. Expect: `NoSpace` from the admission, and the
/// file's size, bytes and the volume's charge exactly as before: all of it or none.
#[test]
fn an_allocation_past_the_quota_is_refused_before_anything_changes() {
  let mut store = store();
  let chunk = u64::try_from(store.content.chunk_bytes()).unwrap();
  let mut vol = volume(&mut store, 4 * chunk);
  let root = vol.root_inode(&store).unwrap();
  let f = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, f, 0, b"kept").unwrap();
  let charged = vol.accounting().referenced_bytes;
  assert_eq!(
    allocate_in_slices(&mut vol, &mut store, f, (0, 5 * chunk), chunk),
    Err(VfsError::NoSpace)
  );
  assert_eq!(contents(&vol, &store, f), b"kept");
  assert_eq!(vol.accounting().referenced_bytes, charged);
}

/// A-108. Do: allocate a directory. Expect: `IsDirectory`, as a write is refused.
#[test]
fn a_directory_is_not_allocated() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 22);
  let root = vol.root_inode(&store).unwrap();
  assert_eq!(
    vol.admit_allocation(&mut store, root, 0, 1),
    Err(VfsError::IsDirectory)
  );
  assert_eq!(
    vol.allocate(&mut store, root, 0, 1),
    Err(VfsError::IsDirectory)
  );
}

/// One generated file: islands written at offsets, then a range allocated.
#[derive(Clone, Debug)]
struct Shape {
  islands: Vec<(u64, Vec<u8>)>,
  range: (u64, u64),
  slice: u64,
}

fn shape(chunk: u64) -> impl Strategy<Value = Shape> {
  let span = 5 * chunk;
  (
    prop::collection::vec((0..span, prop::collection::vec(1u8..=255, 1..64)), 0..6),
    (0..span, 1..span),
    1..2 * chunk,
  )
    .prop_map(|(islands, (start, len), slice)| Shape {
      islands,
      range: (start, start + len),
      slice,
    })
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig { cases: 128, .. ProptestConfig::default() }))]

  /// A-108's oracle. Do: on generated files, allocate a range once whole and once in generated slices, and model the
  /// result as the written bytes over zeros. Expect: both volumes read back the model byte for byte, with the model's
  /// size, and both charge the same: slicing changes nothing a caller can observe.
  #[test]
  fn allocating_in_slices_equals_allocating_whole_and_the_model(generated in shape(65_536)) {
    let mut whole_store = store();
    let mut sliced_store = store();
    let chunk = u64::try_from(whole_store.content.chunk_bytes()).unwrap();
    prop_assume!(chunk == 65_536);
    let mut whole = volume(&mut whole_store, 1 << 23);
    let mut sliced = volume(&mut sliced_store, 1 << 23);
    let mut model: Vec<u8> = Vec::new();
    let mut files = Vec::new();
    for (vol, store) in [(&mut whole, &mut whole_store), (&mut sliced, &mut sliced_store)] {
      let root = vol.root_inode(store).unwrap();
      let f = vol.create_file_no(store, root, "f", 0o644).unwrap();
      for (at, bytes) in &generated.islands {
        vol.write(store, f, *at, bytes).unwrap();
      }
      files.push(f);
    }
    for (at, bytes) in &generated.islands {
      let at = usize::try_from(*at).unwrap();
      if model.len() < at + bytes.len() {
        model.resize(at + bytes.len(), 0);
      }
      model[at..at + bytes.len()].copy_from_slice(bytes);
    }
    let end = usize::try_from(generated.range.1).unwrap();
    if model.len() < end {
      model.resize(end, 0);
    }
    allocate_in_slices(&mut whole, &mut whole_store, files[0], generated.range, u64::MAX).unwrap();
    allocate_in_slices(&mut sliced, &mut sliced_store, files[1], generated.range, generated.slice).unwrap();
    prop_assert_eq!(contents(&whole, &whole_store, files[0]), model.clone());
    prop_assert_eq!(contents(&sliced, &sliced_store, files[1]), model);
    prop_assert_eq!(whole.accounting().referenced_bytes, sliced.accounting().referenced_bytes);
  }
}

/// The large-file class boundary for the overlay test: one window, so a base file of three windows pins per window.
const LARGE: u64 = 65_536;

fn overlay(host: &mut SimHost, store: &mut Store) -> Volume {
  let root = host.root();
  let facts = host.facts(root).unwrap();
  Volume::create_overlay(
    store,
    VolumeConfig {
      prefix: 7,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 23 },
      journal_bytes: 1 << 20,
      clock: Box::new(StepClock::new(1_000_000, 1_000)),
    },
    BaseConfig {
      root,
      facts,
      large_class_bytes: LARGE,
    },
  )
  .unwrap()
}

/// A-108 on an overlay. Do: over a base holding a small file and a large one (three windows and a tail), allocate
/// each from its start to one window past its end, in page slices. Expect: every base byte reads back unchanged, the
/// added range reads zero, and the size is the range's end: a window still on the disk is never zeroed.
#[test]
fn an_overlay_allocation_keeps_every_base_byte() {
  let mut host = SimHost::new();
  let large: Vec<u8> = (0..3 * LARGE + 100).map(|n| (n % 251) as u8 + 1).collect();
  host.replace_file("/small", b"small base bytes");
  host.replace_file("/large", &large);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  let page = u64::try_from(PAGE).unwrap();
  for (path, base) in [("/small", b"small base bytes".to_vec()), ("/large", large)] {
    let mut o = vol.with_host(&mut host);
    let no = o.resolve(&mut store, path).unwrap().inode;
    let end = u64::try_from(base.len()).unwrap() + LARGE;
    o.admit_allocation(&mut store, no, 0, end).unwrap();
    let mut next = 0;
    while next < end {
      let stop = end.min(next + page);
      o.allocate(&mut store, no, next, stop).unwrap();
      next = stop;
    }
    let size = o.stat(&mut store, no).unwrap().size;
    assert_eq!(size, end, "{path}: the size is the range's end");
    let mut back = vec![0u8; usize::try_from(size).unwrap()];
    let n = o.read(&mut store, no, 0, &mut back).unwrap();
    assert_eq!(n, back.len());
    let mut expected = base.clone();
    expected.resize(back.len(), 0);
    assert!(
      back == expected,
      "{path}: base bytes kept and the added range zero"
    );
  }
}
