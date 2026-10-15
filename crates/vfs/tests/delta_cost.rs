//! A-68's promise, held by count (§4.8): a delta costs what changed, not the directory it changed in. One create's
//! publication, and its replay over the previous image, must allocate the same in a large directory as in a small
//! one. The allocator calls are counted per thread, so the verdict is exact and independent of the host's load.
//! Measured before this test (2026-10-05, the hot-directory storm through Linux's own NFS client): every create
//! re-imaged its parent's whole entry list, cloned and sorted, only to drop it, and a replay cloned the list again.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use slates_vfs::base::BaseConfig;
use slates_vfs::clock::StepClock;
use slates_vfs::host::HostFs;
use slates_vfs::host::sim::SimHost;
use slates_vfs::ids::RefOwner;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::recover::VolumeImage;
use slates_vfs::volume::{Store, Volume, VolumeConfig};

mod common;
use common::{clone_config, store, volume, volume_with};

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

// --- Overlays: the A-68 delta now covers base-backed volumes too (condition 2/12; 2026-10-11) ---

/// Format: the large-class boundary for the overlay harness. New files sit far below it, so its value does not affect
/// this test (it governs copy-up windowing, which creating a fresh file does not do).
const OVERLAY_LARGE_CLASS: u64 = 1 << 20;

/// An overlay volume over an empty `SimHost` base, with `upper` files created in its root, imaged once (the whole
/// image every overlay's first publication is) and marked published — so its next publication may be a delta.
fn published_overlay(host: &mut SimHost, store: &mut Store, upper: usize) -> (Volume, VolumeImage) {
  let root = host.root();
  let facts = host.facts(root).unwrap();
  let mut vol = Volume::create_overlay(
    store,
    VolumeConfig {
      prefix: 7,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 20,
      clock: Box::new(StepClock::new(1_000_000, 1_000)),
    },
    BaseConfig {
      root,
      facts,
      large_class_bytes: OVERLAY_LARGE_CLASS,
    },
  )
  .unwrap();
  let vroot = vol.root_inode(store).unwrap();
  for at in 0..upper {
    vol
      .create_file_no(store, vroot, &format!("seed{at:05}"), 0o644)
      .unwrap();
  }
  let image = vol.to_image(store, Some(host)).unwrap();
  vol.mark_published(store);
  (vol, image)
}

/// One create's publication and replay in an overlay whose upper holds `upper` files: the allocations each made, after
/// checking the overlay now publishes a **delta** (not its whole image) and that the replayed delta equals the
/// volume's full image over the same base.
fn one_create_overlay(upper: usize) -> (usize, usize) {
  let mut host = SimHost::new();
  let mut store = store();
  let (mut vol, mut image) = published_overlay(&mut host, &mut store, upper);
  let root = vol.root_inode(&store).unwrap();
  vol
    .create_file_no(&mut store, root, "Fresh.txt", 0o644)
    .unwrap();
  let (published_calls, record) = counted(|| vol.publication(&store, None).unwrap());
  let slates_vfs::delta::VolumeRecord::Delta { delta } = record else {
    panic!("one create in a published overlay is published as a delta, not its whole image");
  };
  let (replayed_calls, applied) = counted(|| image.apply(&delta));
  applied.unwrap();
  assert_eq!(
    image,
    vol.to_image(&store, Some(&mut host)).unwrap(),
    "the replayed overlay delta equals the volume's full image over the same base"
  );
  (published_calls, replayed_calls)
}

/// Condition 2/12, A-68 extended to overlays (2026-10-11): an overlay volume — every OCI/container volume — now
/// publishes a delta per barrier while its base plane is unchanged, so one create's publication and its replay
/// allocate the same in a 4,096-entry upper directory as in a 64-entry one. Before the fix an overlay's publication
/// was always its whole image, allocating with the directory (O(volume) per op, O(N²) to fill a directory); the
/// recovery oracle (`crates/server/tests/recovery.rs::an_overlays_copied_up_and_created_files_survive_a_restart_over_their_base`)
/// proves the delta recovers byte-for-byte.
#[test]
fn an_overlay_create_publishes_a_delta_costing_the_same_in_a_large_upper_as_in_a_small_one() {
  let small = one_create_overlay(SMALL);
  let large = one_create_overlay(LARGE);
  assert_eq!(
    large, small,
    "(publication, replay) allocations in a {LARGE}-entry overlay upper vs a {SMALL}-entry one"
  );
}

// --- Clones: a clone publishes a delta for its own new files, whole only on divergence (condition 2; 2026-10-11) ---

/// A published clone of an origin snapshot with `upper` of its own new files in its root. The clone's first create
/// diverges its inherited root directory (a whole image — the root leaves `origin_shared`), so the clone is marked
/// published after growing, and the create measured below (root already current) is the delta A-68 now allows.
fn published_clone(store: &mut Store, upper: usize) -> (Volume, VolumeImage) {
  let mut origin = volume(store, 1 << 30);
  let oroot = origin.root_inode(store).unwrap();
  origin
    .create_file_no(store, oroot, "inherited", 0o644)
    .unwrap();
  let snap = origin.snapshot(store).unwrap();
  let mut clone = Volume::clone_of(store, &mut origin, snap, clone_config(9)).unwrap();
  let croot = clone.root_inode(store).unwrap();
  for at in 0..upper.max(1) {
    clone
      .create_file_no(store, croot, &format!("own{at:05}"), 0o644)
      .unwrap();
  }
  let image = clone.to_image(store, None).unwrap();
  clone.mark_published(store);
  (clone, image)
}

/// One create's publication and replay in a published clone whose root already diverged (so the create touches no
/// origin inode): the allocations each made, after checking the clone publishes a **delta** and the replayed delta
/// equals the clone's full image.
fn one_create_clone(upper: usize) -> (usize, usize) {
  let mut store = store();
  let (mut clone, mut image) = published_clone(&mut store, upper);
  let root = clone.root_inode(&store).unwrap();
  clone
    .create_file_no(&mut store, root, "Fresh.txt", 0o644)
    .unwrap();
  let (published_calls, record) = counted(|| clone.publication(&store, None).unwrap());
  let slates_vfs::delta::VolumeRecord::Delta { delta } = record else {
    panic!("a clone that only grew its own files publishes a delta, not its whole image");
  };
  let (replayed_calls, applied) = counted(|| image.apply(&delta));
  applied.unwrap();
  assert_eq!(
    image,
    clone.to_image(&store, None).unwrap(),
    "the replayed clone delta equals the clone's full image"
  );
  (published_calls, replayed_calls)
}

/// Condition 2, A-68 extended to clones (2026-10-11): a clone — an agent's work volume — publishes a delta for its own
/// new files, so one create's publication and replay allocate the same in a 4,096-file clone as in a 64-file one; an
/// agent growing a cloned version no longer re-images the whole volume per op. A create that *diverges* an inherited
/// inode still publishes the whole image (the shared→private move a delta cannot carry); growing with own files does
/// not. Recovery byte-correctness: `crates/server/tests/recovery.rs::a_clone_recovers_over_its_recovered_origin...`.
#[test]
fn a_clone_create_publishes_a_delta_costing_the_same_in_a_large_clone_as_in_a_small_one() {
  let small = one_create_clone(SMALL);
  let large = one_create_clone(LARGE);
  assert_eq!(
    large, small,
    "(publication, replay) allocations in a {LARGE}-file clone vs a {SMALL}-file one"
  );
}

// --- Own-snapshot volumes: a delta for new files, whole only on divergence (condition 2; 2026-10-11) ---

/// A published volume that holds a snapshot and `upper` of its own new files born after it. The first create after
/// the snapshot diverges the snapshot-pinned root directory (a whole image), so the volume is marked published after
/// growing, and the create measured below (root already current, no snapshot-pinned inode touched) is a delta.
fn published_snapshotted(store: &mut Store, upper: usize) -> (Volume, VolumeImage) {
  let mut vol = volume(store, 1 << 30);
  let root = vol.root_inode(store).unwrap();
  vol.create_file_no(store, root, "pinned", 0o644).unwrap();
  let _snap = vol.snapshot(store).unwrap();
  for at in 0..upper.max(1) {
    vol
      .create_file_no(store, root, &format!("own{at:05}"), 0o644)
      .unwrap();
  }
  let image = vol.to_image(store, None).unwrap();
  vol.mark_published(store);
  (vol, image)
}

/// One create's publication and replay in a published snapshotted volume whose root already diverged (so the create
/// touches no snapshot-pinned inode): the allocations each made, after checking it publishes a **delta** and the
/// replayed delta equals the volume's full image.
fn one_create_snapshotted(upper: usize) -> (usize, usize) {
  let mut store = store();
  let (mut vol, mut image) = published_snapshotted(&mut store, upper);
  let root = vol.root_inode(&store).unwrap();
  vol
    .create_file_no(&mut store, root, "Fresh.txt", 0o644)
    .unwrap();
  let (published_calls, record) = counted(|| vol.publication(&store, None).unwrap());
  let slates_vfs::delta::VolumeRecord::Delta { delta } = record else {
    panic!(
      "a snapshotted volume that only grew its own files publishes a delta, not its whole image"
    );
  };
  let (replayed_calls, applied) = counted(|| image.apply(&delta));
  applied.unwrap();
  assert_eq!(
    image,
    vol.to_image(&store, None).unwrap(),
    "the replayed snapshotted-volume delta equals its full image"
  );
  (published_calls, replayed_calls)
}

/// Condition 2, A-68 extended to own-snapshot volumes (2026-10-11): a volume holding a snapshot publishes a delta for
/// its own new files — one create's publication and replay allocate the same in a 4,096-file volume as in a 64-file
/// one — so a green merge target written while it holds a snapshot no longer re-images the whole volume per op. A
/// write or delete of a snapshot-pinned inode still publishes the whole image (the shared→private move into the
/// snapshot's private records a delta cannot carry).
#[test]
fn a_snapshotted_create_publishes_a_delta_costing_the_same_in_a_large_volume_as_in_a_small_one() {
  let small = one_create_snapshotted(SMALL);
  let large = one_create_snapshotted(LARGE);
  assert_eq!(
    large, small,
    "(publication, replay) allocations in a {LARGE}-file snapshotted volume vs a {SMALL}-file one"
  );
}
