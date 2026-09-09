//! A C ABI over `serve` for the in-process transport form (§4.6), built only under the `test-harness`
//! feature — the shipped crate does not include it.
//!
//! Both transport forms the Phase 4 spike weighs end in `serve(request, bridge, cx)`: the forwarding
//! form reaches it over the app-group ring in the daemon; the in-process form calls it in the extension
//! with the core linked in. This exposes that shared entry as three C functions so the Swift `FSVolume`
//! handler can drive the real `Bridge` over a real volume in one process — no ring, no mount — which
//! exercises the whole handler↔codec↔bridge stack end to end (R5), the one link the Rust-side `serve`
//! tests and the Swift-side mock-channel tests each cover only half of.
//!
//! This is a verification harness, not the shipped transport: the final form and its packaging are the
//! spike's decision (§4.6), and the volume it opens is an empty scratch volume for the handler to act
//! on. The store and volume are leaked to `'static` (the process-lifetime `Box::leak` singleton the
//! codebase uses for process-lived state), so the bridge borrows them for the process; freeing the
//! handle drops the bridge, and the harness process reclaims the rest on exit.

use crate::serve;
use slates_bridge_core::{Attachments, OpContext, Rights, View, VolumeBridge};
use slates_db::catalog::{Principal, VolumeId};
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::HostClock;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// Format: the harness store's page size — 4 KiB, the arena page the rest of the system uses.
const HARNESS_PAGE: usize = 4096;
/// Format: the harness store's region size in pages — one 16 MiB region, ample for the handler's
/// by-use exercises; matches the store in `crates/bridge-fskit/tests/shim.rs`.
const HARNESS_REGION_PAGES: usize = 4096;
/// Format: the harness store's cache-line width, for the store's padding; 128 bytes on Apple silicon.
const HARNESS_CACHE_LINE: usize = 128;
/// Format: harness store capacities — directories, inodes, chunks and directory blocks — sized to hold
/// the handler's small exercises; matches the test store's dimensions.
const HARNESS_MAX_DIRS: usize = 64;
/// Format: see [`HARNESS_MAX_DIRS`].
const HARNESS_MAX_INODES: usize = 256;
/// Format: see [`HARNESS_MAX_DIRS`].
const HARNESS_MAX_DIR_BLOCKS: usize = 64;
/// Format: the directory-representation cutover (entries) for the harness store; matches the test.
const HARNESS_DIR_CUTOVER: usize = 16;
/// Format: the harness volume's quota ceiling — 1 GiB, far above what the exercises touch.
const HARNESS_QUOTA_BYTES: u64 = 1 << 30;
/// Format: the harness volume's journal size — 64 KiB, matching the test volume.
const HARNESS_JOURNAL_BYTES: usize = 1 << 16;
/// Format: the harness volume's inode-number prefix — a non-zero prefix (1) so the harness exercises
/// the real `compose(prefix, 1)` root, not the degenerate inode-1 case.
const HARNESS_PREFIX: u16 = 1;
/// Format: the harness volume id — a fixed 16-byte tag; matches the test's `VolumeId`.
const HARNESS_VOLUME_ID: [u8; 16] = [0x11; 16];

/// A live in-process volume: the `Bridge` over a leaked store and volume, plus the write context every
/// request runs under. Held behind an opaque pointer the Swift side keeps for the session.
pub struct InProcessVolume {
  bridge: VolumeBridge<'static>,
  cx: OpContext,
}

/// Builds an empty scratch volume and a read/write context over it, or `None` if the store cannot be
/// mapped. No panics: every fallible step becomes `None` (R3/no-panic), which the C entry turns into a
/// null handle.
fn build_harness_volume() -> Option<InProcessVolume> {
  let mut arena = ChunkArena::new(HARNESS_PAGE);
  let region = Region::map(HARNESS_PAGE * HARNESS_REGION_PAGES, HARNESS_PAGE, false).ok()?;
  arena.add_region(region).ok()?;
  let config = StoreConfig {
    page: HARNESS_PAGE,
    cache_line: HARNESS_CACHE_LINE,
    max_dirs: HARNESS_MAX_DIRS,
    max_inodes: HARNESS_MAX_INODES,
    max_chunks: HARNESS_REGION_PAGES,
    max_dir_blocks: HARNESS_MAX_DIR_BLOCKS,
    dir_cutover: HARNESS_DIR_CUTOVER,
  };
  let store: &'static mut Store = Box::leak(Box::new(Store::new(&config, arena, 0)));
  let volume_value = Volume::create(
    &mut *store,
    VolumeConfig {
      prefix: HARNESS_PREFIX,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded {
        limit: HARNESS_QUOTA_BYTES,
      },
      journal_bytes: HARNESS_JOURNAL_BYTES,
      clock: Box::new(HostClock::default()),
    },
  )
  .ok()?;
  let volume: &'static mut Volume = Box::leak(Box::new(volume_value));
  let bridge = VolumeBridge::new(
    VolumeId {
      bytes: HARNESS_VOLUME_ID,
    },
    volume,
    store,
  );
  let mut attachments = Attachments::new();
  let attachment = attachments
    .attach(
      VolumeId {
        bytes: HARNESS_VOLUME_ID,
      },
      View::Current,
      Principal::Uid { uid: 0 },
      Rights {
        read: true,
        write: true,
      },
    )
    .ok()?;
  let cx = attachments.context(attachment).ok()?;
  Some(InProcessVolume { bridge, cx })
}

/// Open an in-process scratch volume; returns an opaque handle for [`slates_fskit_serve`], freed with
/// [`slates_fskit_free`]. Returns null if the store cannot be mapped.
#[unsafe(no_mangle)]
pub extern "C" fn slates_fskit_open_test_volume() -> *mut InProcessVolume {
  match build_harness_volume() {
    Some(volume) => Box::into_raw(Box::new(volume)),
    None => core::ptr::null_mut(),
  }
}

/// Dispatch one encoded shim request through `serve` on the handle's bridge, writing the reply into
/// `out` (up to `out_cap`) and returning the reply length. Returns 0 on a null argument or a malformed
/// request — which the Swift `InProcessChannel` treats as a transport failure, exactly as it would a
/// dead ring.
///
/// # Safety
/// `handle` must be a live pointer from [`slates_fskit_open_test_volume`]; `request` and `out` must
/// point to readable/writable buffers of at least `request_len`/`out_cap` bytes. The Swift caller holds
/// all three across the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slates_fskit_serve(
  handle: *mut InProcessVolume,
  request: *const u8,
  request_len: usize,
  out: *mut u8,
  out_cap: usize,
) -> usize {
  if handle.is_null() || request.is_null() || out.is_null() {
    return 0;
  }
  // SAFETY: the caller guarantees a live handle from `open_test_volume`, held for this call; it is the
  // only `&mut` to that volume for the call's duration.
  let volume = unsafe { &mut *handle };
  // SAFETY: the caller guarantees `request` points to `request_len` readable bytes (the contract).
  let request_bytes = unsafe { core::slice::from_raw_parts(request, request_len) };
  let reply = match serve(request_bytes, &mut volume.bridge, &volume.cx) {
    Ok(bytes) => bytes,
    Err(_) => return 0,
  };
  let written = reply.len().min(out_cap);
  // SAFETY: `out` is writable for `out_cap` bytes (the contract), and `written <= out_cap`.
  let out_bytes = unsafe { core::slice::from_raw_parts_mut(out, written) };
  out_bytes.copy_from_slice(&reply[..written]);
  written
}

/// Free a handle from [`slates_fskit_open_test_volume`]. A null handle is ignored.
///
/// # Safety
/// `handle` must have come from [`slates_fskit_open_test_volume`] and not been freed already.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slates_fskit_free(handle: *mut InProcessVolume) {
  if !handle.is_null() {
    // SAFETY: the handle came from `Box::into_raw` in `open_test_volume` and is freed exactly once.
    drop(unsafe { Box::from_raw(handle) });
  }
}
