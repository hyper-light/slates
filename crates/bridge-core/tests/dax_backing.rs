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

use slates_bridge_core::dax_backing::{Ask, Counts, DaxBackingSession, FillStep, guarded_layout};
use slates_bridge_core::{Attachments, Bridge, ObjectId, OpContext, Rights, View, VolumeBridge};
use slates_db::catalog::{Principal, VolumeId};
use slates_mem::DAX_RANGE_BYTES;
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::HostClock;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::shards_backing::PagePlacement;
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
      Principal::Uid { host: slates_db::HostId(0), uid: 0 },
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

/// §4.6 (DAX confinement, the "one predicate per method" discipline; AUD-29-76 scoped exports): a guest's ask through
/// a scoped mount is answered for a file in its subtree and refused for one outside it before any backing is made, so
/// no window opens onto content the scope does not grant. Do: a volume with `/shared/f` and `/private/secret`; through
/// a `ScopedBridge` on `/shared`, ask a range of `f`, then of `secret`; then ask `secret` through the unscoped bridge.
/// Expect `f` answered and filled; `secret` refused as the scope's miss (`NotFound`) with no backing made and nothing
/// charged; and the unscoped ask answered, so the refusal is the scope's, not the file's.
#[test]
fn a_scoped_mounts_ask_is_confined_to_its_subtree() {
  use slates_bridge_core::dax_backing::Refused;
  use slates_bridge_core::scoped::ScopedBridge;
  use slates_vfs::error::VfsError;
  let mut store = store();
  let mut vol = volume(&mut store);
  let cx = rw_cx();
  let ask = |node| Ask {
    node,
    writable: false,
    range: Some((0, DAX_RANGE_BYTES)),
  };
  let mut session = DaxBackingSession::new();
  let mut inner = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let root = inner.root(&cx).unwrap();
  let shared = inner
    .mkdir(ObjectId::new(root, 0), &cx, "shared", 0o755)
    .unwrap();
  let private = inner
    .mkdir(ObjectId::new(root, 0), &cx, "private", 0o755)
    .unwrap();
  let (f, _) = inner
    .create(ObjectId::new(shared.ino, 0), &cx, "f", 0o644, 0)
    .unwrap();
  inner
    .write(ObjectId::new(f.ino, 0), &cx, 0, &[0xAB; PAGE])
    .unwrap();
  let (secret, _) = inner
    .create(ObjectId::new(private.ino, 0), &cx, "secret", 0o644, 0)
    .unwrap();
  inner
    .write(ObjectId::new(secret.ino, 0), &cx, 0, &[0xCD; PAGE])
    .unwrap();

  {
    let mut scoped = ScopedBridge::new(&mut inner, shared.ino);
    assert_eq!(
      session.ask(&mut scoped, &cx, ask(f.ino)),
      Ok(f.ino),
      "the scoped mount maps its own file"
    );
    let descriptor = filled(&mut session, &mut scoped, &cx);
    assert!(
      object_bytes(&descriptor, 0, PAGE) == vec![0xAB; PAGE],
      "the file's backing holds the file"
    );
  }
  let held = inner.dax_state().unwrap().backings.vmms_held();
  {
    let mut scoped = ScopedBridge::new(&mut inner, shared.ino);
    assert_eq!(
      session.ask(&mut scoped, &cx, ask(secret.ino)),
      Err(Refused::Unreachable(VfsError::NotFound)),
      "the scoped mount cannot map a file outside its subtree"
    );
  }
  assert_eq!(
    inner.dax_state().unwrap().backings.vmms_held(),
    held,
    "the refused ask made no backing and charged nothing"
  );
  // Non-vacuity: the private file is mappable, so the scope alone refused it.
  assert_eq!(
    session.ask(&mut inner, &cx, ask(secret.ino)),
    Ok(secret.ino),
    "the unscoped bridge maps the private file"
  );
}

/// The bytes the object `descriptor` names holds in memory: its allocated blocks, as `fstat` counts them in 512-byte
/// units (`st_blocks`, POSIX).
fn object_resident(descriptor: &OwnedFd) -> u64 {
  /// Format: the unit of `st_blocks` (POSIX `<sys/stat.h>`).
  const BLOCK_BYTES: u64 = 512;
  u64::try_from(rustix::fs::fstat(descriptor).unwrap().st_blocks).unwrap() * BLOCK_BYTES
}

/// Asks each of `ranges` of file `node`, read-only, through `session`, and fills it: the descriptors it answers with.
/// Prints each fill's longest step, which is the one that finishes the range and places it.
fn filled_ranges(
  session: &mut DaxBackingSession,
  vol: &mut Volume,
  store: &mut Store,
  cx: &OpContext,
  node: u64,
  ranges: &[u64],
) -> Vec<OwnedFd> {
  let mut descriptors = Vec::new();
  for range in ranges {
    let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, vol, store);
    session
      .ask(
        &mut bridge,
        cx,
        Ask {
          node,
          writable: false,
          range: Some((range * DAX_RANGE_BYTES, DAX_RANGE_BYTES)),
        },
      )
      .unwrap();
    let mut longest = std::time::Duration::ZERO;
    let descriptor = loop {
      let began = std::time::Instant::now();
      let step = session.fill_step(&mut bridge, cx, SLICE);
      longest = longest.max(began.elapsed());
      match step {
        FillStep::Filling => {}
        FillStep::Done(descriptor) => break descriptor,
        other => panic!("the fill did not complete: {other:?}"),
      }
    };
    println!(
      "range {range}: the fill's longest step took {} us",
      longest.as_micros()
    );
    descriptors.push(descriptor);
  }
  descriptors
}

/// Whether the host declined every placement (`placed` none and `refused` some), which a test can prove nothing from:
/// said loudly, so the run reads as a skip. A run that placed none and refused none never asked, which is a failure the
/// caller's assertions catch, not a skip.
fn host_declined_every_placement(test: &str, placed: u64, refused: u64) -> bool {
  let declined = placed == 0 && refused > 0;
  if declined {
    eprintln!(
      "SKIP {test}: the kernel declined all {refused} placements on this host now, so no huge page is here to read"
    );
  }
  declined
}

/// Holds a session's placements to §4.6's backed L1 on a host whose PMD page is a range: every one of `ranges` filled
/// placed or counted refused, at least one placed; under the device's guarded layout, each placed page one PMD page;
/// and the object holding no more than the backings count, `charged`, and, where none was refused, exactly the pages
/// its ranges lie in, which is what the budget counts.
fn assert_placed(
  counts: &Counts,
  placements: &[PagePlacement],
  ranges: &[u64],
  resident: u64,
  charged: u64,
) {
  let layout = guarded_layout();
  assert!(
    matches!(layout, slates_mem::DaxLayout::Guarded { .. }),
    "a host that places ranges lays the device's backings out guarded"
  );
  assert_eq!(
    counts.placed + counts.placement_refused,
    ranges.len() as u64,
    "every range filled is placed or counted refused"
  );
  assert!(
    counts.placed > 0,
    "no range was placed or refused: placement never ran"
  );
  for placement in placements {
    assert_eq!(
      placement.pmd_bytes,
      Some(DAX_RANGE_BYTES),
      "page {} is one PMD page",
      placement.page
    );
  }
  if counts.placement_refused == 0 {
    assert_whole_cost(
      placements.len() as u64,
      layout.pages_holding(ranges.iter().copied()),
      resident,
      charged,
    );
  }
  assert!(
    resident <= charged,
    "the object holds no more than the backings count"
  );
}

/// Holds a placement with nothing refused to its whole cost: `placed` pages placed, exactly the `pages` its ranges lie
/// in, the object holding them (`resident` bytes) and the backings counting them (`charged`) and nothing more.
fn assert_whole_cost(placed: u64, pages: u64, resident: u64, charged: u64) {
  assert_eq!(placed, pages, "every page the ranges lie in is placed");
  assert_eq!(
    resident,
    pages * DAX_RANGE_BYTES,
    "the object holds the placed pages and nothing more"
  );
  assert_eq!(
    charged,
    pages * DAX_RANGE_BYTES,
    "the backings count exactly the pages placed"
  );
}

/// §4.6, the backed route's L1 (A-193, A-194): each range a guest maps is placed in huge pages as its fill finishes,
/// where the host has a huge page of a range's size, laid out as slates' own device lays its backings out (guarded),
/// and the placement costs no more memory than the fill budget counts. Do: make a file of one page; ask, read-only,
/// through a session in the device's layout, its first range and a range wholly past its end, and fill each; read where
/// each placed page lies and how many bytes the object holds; then let every backing go. Expect, on a host whose PMD
/// page is a range: the layout guarded; every range filled placed or counted refused, at least one placed, each placed
/// page one PMD page in slates' view; the object holding exactly the pages its ranges lie in (two a range alone under
/// the guard, four here), which is what the backings count, within the budget; and, once let go, nothing placed, held
/// or counted. Elsewhere: nothing placed and nothing counted.
#[test]
fn each_range_a_guest_maps_is_one_huge_page_within_the_fill_budget() {
  /// Shape: the ranges asked: the file's own and one wholly past its end, where the fill copies nothing.
  const RANGES: [u64; 2] = [0, 2];
  let mut store = store();
  let mut vol = volume(&mut store);
  let cx = rw_cx();
  let root = vol.root();
  let no = vol.create_file(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, no, 0, &[0x41; PAGE]).unwrap();

  let mut session = DaxBackingSession::with_layout(guarded_layout());
  let descriptors = filled_ranges(&mut session, &mut vol, &mut store, &cx, no.0, &RANGES);
  let counts = session.take_counts();
  let placements = store.shards_backings.placements();
  let charged = store.shards_backings.charged();
  let resident = object_resident(&descriptors[0]);
  println!(
    "placed {} refused {} of {} ranges; the object holds {resident} bytes, the backings count {charged} of {}",
    counts.placed,
    counts.placement_refused,
    RANGES.len(),
    store.shards_backings.budget()
  );
  if slates_machine::facts::huge_page_bytes() == Some(DAX_RANGE_BYTES) {
    if !host_declined_every_placement(
      "each_range_a_guest_maps_is_one_huge_page_within_the_fill_budget",
      counts.placed,
      counts.placement_refused,
    ) {
      assert_placed(&counts, &placements, &RANGES, resident, charged);
    }
  } else {
    assert_eq!(
      (counts.placed, counts.placement_refused, placements.len()),
      (0, 0, 0),
      "nothing placed or counted where the host has no huge page of a range's size"
    );
  }
  assert!(
    charged <= store.shards_backings.budget(),
    "the backings count within the budget"
  );

  session.release_all(&mut store.shards_backings);
  assert_eq!(
    (
      store.shards_backings.placed_pages(),
      store.shards_backings.vmms_held()
    ),
    (0, (0, 0)),
    "once let go nothing is placed, held or counted"
  );
}

/// Format: the environment variable that makes this test binary, re-invoked, play the child whose huge pages the kernel
/// refuses.
#[cfg(target_os = "linux")]
const DECLINED_ROLE: &str = "SLATES_TEST_PLACEMENT_DECLINED_ROLE";
/// Format: the line the child prints once its checks passed. The parent looks for it anywhere in a line, since libtest
/// prints its own words before the child's on the same line.
#[cfg(target_os = "linux")]
const DECLINED_ROLE_PASSED: &str = "PLACEMENT-DECLINED-ROLE-PASSED";

/// §4.6, the backed route's L1 (A-193): a decline likely to hold is remembered until re-armed, so a host out of huge
/// pages is asked once, not once a range, and a guest churning its ranges cannot make the shard pay the kernel's
/// compaction effort again and again. Do: in a child process whose huge pages the kernel refuses (`PR_SET_THP_DISABLE`,
/// so every `MADV_COLLAPSE` is declined `EINVAL`), fill three ranges of a file; re-arm the memo as the daemon's reap
/// tick does; fill a fourth. Expect: none placed and all four counted refused; the kernel asked once for the first
/// three, the decline remembered after them, and asked again once re-armed.
#[cfg(target_os = "linux")]
#[test]
fn a_declined_placement_is_remembered_until_re_armed() {
  if std::env::var_os(DECLINED_ROLE).is_some() {
    declined_placement_role();
    return;
  }
  if slates_machine::facts::huge_page_bytes() != Some(DAX_RANGE_BYTES) {
    eprintln!(
      "SKIP a_declined_placement_is_remembered_until_re_armed: this host has no PMD page of a range's size"
    );
    return;
  }
  let output = std::process::Command::new(std::env::current_exe().unwrap())
    .args([
      "--exact",
      "a_declined_placement_is_remembered_until_re_armed",
      "--nocapture",
      "--test-threads",
      "1",
    ])
    .env(DECLINED_ROLE, "1")
    .output()
    .unwrap();
  let stdout = String::from_utf8_lossy(&output.stdout);
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    output.status.success(),
    "the child failed: {stdout} {stderr}"
  );
  assert!(
    stdout.contains(DECLINED_ROLE_PASSED),
    "the child ran its checks: {stdout} {stderr}"
  );
}

/// The child's part: its huge pages refused, every placement is declined, and the memo holds the kernel to one ask
/// until it is re-armed.
#[cfg(target_os = "linux")]
fn declined_placement_role() {
  rustix::thread::disable_transparent_huge_pages(true).unwrap();
  let mut store = store();
  let mut vol = volume(&mut store);
  let cx = rw_cx();
  let root = vol.root();
  let no = vol.create_file(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, no, 0, &[0x41; PAGE]).unwrap();
  let mut session = DaxBackingSession::new();
  let _first = filled_ranges(&mut session, &mut vol, &mut store, &cx, no.0, &[0, 1, 2]);
  let counts = session.take_counts();
  assert_eq!(
    (counts.placed, counts.placement_refused),
    (0, 3),
    "none placed, all three refused"
  );
  assert_eq!(
    store.shards_backings.placements_asked(),
    1,
    "the kernel asked once for the three"
  );
  assert!(
    store.shards_backings.placement_declined(),
    "the decline remembered"
  );
  store.shards_backings.rearm_placement();
  let _second = filled_ranges(&mut session, &mut vol, &mut store, &cx, no.0, &[3]);
  let counts = session.take_counts();
  assert_eq!((counts.placed, counts.placement_refused), (0, 1));
  assert_eq!(
    store.shards_backings.placements_asked(),
    2,
    "asked again once re-armed"
  );
  println!("{DECLINED_ROLE_PASSED}");
}
