//! A-68's promise, held by count (§4.8): a delta costs what changed, not the directory it changed in. One create's
//! publication, and its replay over the previous image, must allocate the same in a large directory as in a small
//! one. The allocator calls are counted per thread, so the verdict is exact and independent of the host's load.
//! Measured before this test (2026-10-05, the hot-directory storm through Linux's own NFS client): every create
//! re-imaged its parent's whole entry list, cloned and sorted, only to drop it, and a replay cloned the list again.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use slates_vfs::ids::RefOwner;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::recover::VolumeImage;
use slates_vfs::volume::{Store, Volume};

mod common;
use common::{store, volume_with};

struct Counting;
thread_local! {
  // Const initialization and no destructor: observing an allocation never allocates recursively.
  static CALLS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: forwards the allocator contract unchanged; the counter observes calls only.
unsafe impl GlobalAlloc for Counting {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    CALLS.set(CALLS.get() + 1);
    // SAFETY: forwarded unchanged.
    unsafe { System.alloc(layout) }
  }
  unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
    // SAFETY: forwarded unchanged.
    unsafe { System.dealloc(pointer, layout) }
  }
  unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
    CALLS.set(CALLS.get() + 1);
    // SAFETY: forwarded unchanged.
    unsafe { System.realloc(pointer, layout, size) }
  }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The allocator calls `work` makes on this thread, and its result.
fn counted<T>(work: impl FnOnce() -> T) -> (usize, T) {
  let before = CALLS.get();
  let out = work();
  (CALLS.get() - before, out)
}

/// Shape: the small directory's entries.
const SMALL: usize = 64;
/// Shape: the large directory's entries: sixty-four times the small one, so a cost that follows the directory is
/// thousands of allocations apart, not one.
const LARGE: usize = 4096;

/// A volume whose root holds `entries` files, published (its next record a delta), with its full image.
fn published(store: &mut Store, entries: usize, names: NameEquivalence) -> (Volume, VolumeImage) {
  let mut vol = volume_with(store, Quota::Bounded { limit: 1 << 24 }, names);
  let root = vol.root_inode(store).unwrap();
  for at in 0..entries {
    vol
      .create_file_no(store, root, &format!("seed{at:05}"), 0o644)
      .unwrap();
  }
  let image = vol.to_image(store, None).unwrap();
  vol.mark_published(store);
  (vol, image)
}

/// One create's publication and replay in a directory of `entries`: the allocations each made, after checking the
/// replayed image equals the volume's full image (the oracle a cheaper path must still meet).
fn one_create(entries: usize, names: NameEquivalence) -> (usize, usize) {
  let mut store = store();
  let (mut vol, mut image) = published(&mut store, entries, names);
  let root = vol.root_inode(&store).unwrap();
  vol
    .create_file_no(&mut store, root, "Fresh.txt", 0o644)
    .unwrap();
  let (published_calls, record) = counted(|| vol.publication(&store, None).unwrap());
  let slates_vfs::delta::VolumeRecord::Delta { delta } = record else {
    panic!("one create in a published volume is published as a delta");
  };
  let (replayed_calls, applied) = counted(|| image.apply(&delta));
  applied.unwrap();
  assert_eq!(
    image,
    vol.to_image(&store, None).unwrap(),
    "the replayed image is the volume's image"
  );
  (published_calls, replayed_calls)
}

/// A-68: do publish one create into a directory of 64 entries and into one of 4,096, then replay each delta over the
/// previous image; expect both to allocate the same (under both name policies), so a create's cost on the barrier
/// path and on recovery does not grow with its directory.
#[test]
fn one_creates_delta_costs_the_same_in_a_large_directory_as_in_a_small_one() {
  for names in [NameEquivalence::Exact, NameEquivalence::Fold] {
    let small = one_create(SMALL, names);
    let large = one_create(LARGE, names);
    assert_eq!(
      large, small,
      "{names:?}: (publication, replay) allocations in a {LARGE}-entry directory vs a {SMALL}-entry one"
    );
  }
}

/// Format: the attachment whose kernel references the reference tests hold (any recorded attachment id).
const MOUNT: u64 = 7;

/// A published volume whose root holds `entries` files, each referenced once by the recorded attachment [`MOUNT`]
/// (as a FUSE mount's kernel holds a lookup reference on every inode it has seen), with its full image.
fn published_and_referenced(store: &mut Store, entries: usize) -> (Volume, VolumeImage) {
  let mut vol = volume_with(
    store,
    Quota::Bounded { limit: 1 << 24 },
    NameEquivalence::Exact,
  );
  let root = vol.root_inode(store).unwrap();
  for at in 0..entries {
    let no = vol
      .create_file_no(store, root, &format!("seed{at:05}"), 0o644)
      .unwrap();
    vol
      .reference_for(store, no, RefOwner::Attachment(MOUNT))
      .unwrap();
  }
  let image = vol.to_image(store, None).unwrap();
  vol.mark_published(store);
  (vol, image)
}

/// One create its mount then references, and one forget of an older reference, published and replayed with
/// `entries` references held: the published delta's encoded bytes and the allocations its publication and its replay
/// made, after checking the replayed image equals the volume's.
fn one_referenced_create(entries: usize) -> (usize, usize, usize) {
  let mut store = store();
  let (mut vol, mut image) = published_and_referenced(&mut store, entries);
  let root = vol.root_inode(&store).unwrap();
  let fresh = vol
    .create_file_no(&mut store, root, "Fresh.txt", 0o644)
    .unwrap();
  vol
    .reference_for(&store, fresh, RefOwner::Attachment(MOUNT))
    .unwrap();
  let older = vol.lookup_no(&store, root, "seed00000").unwrap().inode;
  vol
    .forget_for(&mut store, older, RefOwner::Attachment(MOUNT), 1)
    .unwrap();
  let (published_calls, record) = counted(|| vol.publication(&store, None).unwrap());
  let slates_vfs::delta::VolumeRecord::Delta { delta } = record else {
    panic!("one create in a published volume is published as a delta");
  };
  let mut encoded = Vec::new();
  slates_wire::Wire::encode(&delta, &mut encoded);
  let (replayed_calls, applied) = counted(|| image.apply(&delta));
  applied.unwrap();
  assert_eq!(
    image,
    vol.to_image(&store, None).unwrap(),
    "the replayed image, references included, is the volume's image"
  );
  (encoded.len(), published_calls, replayed_calls)
}

/// A-96: do publish one create and its kernel reference, plus one forget, in a volume whose mount references 64
/// inodes and in one whose mount references 4,096, then replay each delta; expect both deltas to encode to the same
/// bytes and to allocate the same, and the replay to equal the volume's image. Before (2026-10-05, 2,000 creates through Linux's FUSE mount at about 0.8 ms
/// each): every delta carried every attachment's whole reference list, so a FUSE create's barrier grew with the
/// inodes its kernel had seen.
#[test]
fn one_reference_changes_delta_costs_the_same_with_many_references_held() {
  let small = one_referenced_create(SMALL);
  let large = one_referenced_create(LARGE);
  assert_eq!(
    large, small,
    "(delta bytes, publication allocations, replay allocations) with {LARGE} references held vs {SMALL}"
  );
}
