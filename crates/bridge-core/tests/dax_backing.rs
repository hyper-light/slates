//! The backed DAX session by use through a real volume bridge (§4.6; A-163; goal G-vhost): what a guest maps from a
//! file's backing stays equal to what the volume holds, whatever path changes it. The object is read through the
//! descriptor the session hands out, mapped as a VMM maps it.
#![cfg(unix)]
// Test harness code: an unwrap here is a failed test.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::os::fd::OwnedFd;

use slates_bridge_core::dax_backing::{Ask, DaxBackingSession, FillStep};
use slates_bridge_core::{Attachments, Bridge, ObjectId, OpContext, Rights, View, VolumeBridge};
use slates_db::catalog::{Principal, VolumeId};
use slates_mem::DAX_RANGE_BYTES;
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::HostClock;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// Shape: the store's page size.
const PAGE: usize = 4096;
/// Shape: the pages the store's region maps: room for the files these tests write.
const REGION_PAGES: usize = 4096;
/// Shape: a journal that holds every record these tests make.
const JOURNAL: usize = 1 << 16;
/// Shape: the fill budget, in ranges: more than any test fills.
const BUDGET_RANGES: u64 = 8;
/// Shape: the bytes one fill slice copies: a few pages, so a fill takes several slices.
const SLICE: u64 = 4 * PAGE as u64;

fn store() -> Store {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(PAGE * REGION_PAGES, PAGE, false).unwrap())
    .unwrap();
  let mut store = Store::new(
    &StoreConfig {
      page: PAGE,
      cache_line: 128,
      max_dirs: 64,
      max_inodes: 256,
      max_chunks: REGION_PAGES,
      max_dir_blocks: 64,
      dir_cutover: 16,
    },
    arena,
    0,
  );
  store
    .shards_backings
    .set_budget(BUDGET_RANGES * DAX_RANGE_BYTES);
  store
}

fn volume(store: &mut Store) -> Volume {
  Volume::create(
    store,
    VolumeConfig {
      prefix: 1,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: JOURNAL,
      clock: Box::new(HostClock::default()),
    },
  )
  .unwrap()
}

fn rw_cx() -> OpContext {
  let mut attachments = Attachments::new();
  let id = attachments
    .attach(
      VolumeId { bytes: [0; 16] },
      View::Current,
      Principal::Uid { uid: 0 },
      Rights {
        read: true,
        write: true,
      },
    )
    .unwrap();
  attachments.context(id).unwrap()
}

/// Runs the session's pending fill to its end: the descriptor it answers with.
fn filled(session: &mut DaxBackingSession, bridge: &mut dyn Bridge, cx: &OpContext) -> OwnedFd {
  loop {
    match session.fill_step(bridge, cx, SLICE) {
      FillStep::Filling => {}
      FillStep::Done(descriptor) => return descriptor,
      other => panic!("the fill did not complete: {other:?}"),
    }
  }
}

/// `len` bytes of the object `descriptor` names, from `at`, read through a shared mapping as a VMM maps it.
fn object_bytes(descriptor: &OwnedFd, at: u64, len: usize) -> Vec<u8> {
  // SAFETY: a fresh read-only shared mapping at an address the kernel chooses, of `len` bytes of a descriptor this
  // test owns, at a page-aligned offset; it is copied out and unmapped below with the same address and length before
  // the descriptor can be dropped.
  let mapped = unsafe {
    rustix::mm::mmap(
      std::ptr::null_mut(),
      len,
      rustix::mm::ProtFlags::READ,
      rustix::mm::MapFlags::SHARED,
      descriptor,
      at,
    )
  }
  .unwrap();
  // SAFETY: `mapped` is the mapping just made, `len` bytes long, and nothing writes it for this borrow.
  let bytes = unsafe { std::slice::from_raw_parts(mapped.cast::<u8>(), len) }.to_vec();
  // SAFETY: unmapping exactly the mapping made above, with its own address and length.
  unsafe { rustix::mm::munmap(mapped, len) }.unwrap();
  bytes
}

/// §4.6, the backed window's coherence past the end. A guest maps the 2 MiB range that holds the offset it touches,
/// by its own idea of the file's size; after another path cuts the file, that idea is stale, and the guest maps a
/// range wholly past the end. Do: make a 4 KiB file; ask its second range, [2 MiB, 4 MiB), read-only, as such a guest
/// does, and fill it; then write 4 KiB at 2.5 MiB through the volume, extending the file into that range. Expect the
/// object to hold the write: the guest's map of the range shows what the file holds, as its map of any range does.
#[test]
fn a_range_asked_past_the_files_end_takes_a_later_write_into_it() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let cx = rw_cx();
  let root = vol.root();
  let no = vol.create_file(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, no, 0, &[0x41; PAGE]).unwrap();

  let mut session = DaxBackingSession::new();
  let descriptor = {
    let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
    let inode = session
      .ask(
        &mut bridge,
        &cx,
        Ask {
          node: no.0,
          writable: false,
          range: Some((DAX_RANGE_BYTES, DAX_RANGE_BYTES)),
        },
      )
      .unwrap();
    assert_eq!(inode, no.0, "the node resolves to the file");
    filled(&mut session, &mut bridge, &cx)
  };

  let at = DAX_RANGE_BYTES + DAX_RANGE_BYTES / 4;
  {
    let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
    bridge
      .write(ObjectId::new(no.0, 0), &cx, at, &[0x42; PAGE])
      .unwrap();
  }
  assert!(
    object_bytes(&descriptor, at, PAGE) == vec![0x42; PAGE],
    "the extending write did not reach the object over the range the guest maps past the old end"
  );
  assert!(
    object_bytes(&descriptor, DAX_RANGE_BYTES, PAGE) == vec![0; PAGE],
    "the hole the write opened reads zero"
  );
}
