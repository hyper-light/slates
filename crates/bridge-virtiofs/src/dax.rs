//! The DAX cache region (§4.6, A-9, AC-4.12): the host-side window the device maps content blocks' pages into, so a
//! guest reaches arena bytes through its own page table with no daemon call (FUSE_SETUPMAPPING / FUSE_REMOVEMAPPING).
//!
//! This is the **fd-sharing** form: the VMM shares the cache window as a memory object, the device reserves the whole
//! window at construction (`PROT_NONE`, inaccessible), and each SETUPMAPPING overlays a page-aligned sub-range with a
//! shared mapping of a content block's descriptor (`MAP_FIXED`). A REMOVEMAPPING — and the teardown half of a revoke —
//! restores the inert reservation, so a guest's stale page table reaches a faulting hole, never a reused block. The
//! block's descriptor is the content arena's shared object and the block is **pinned by the gate** while mapped
//! ([`slates_vfs::dax`]), so its pages are stable. A message-based VMM (QEMU's vhost-user SHMEM_MAP) drives the same
//! gate verbs over the wire instead; this module is the fd-sharing path a VMM like crosvm/libkrun uses, and the one the
//! sim exercises end to end without a VMM.
//!
//! Unix only (the DAX window is a Linux/vhost-user feature; the primitive is exercised on macOS over an `shm` object).
#![cfg(unix)]

use std::os::fd::BorrowedFd;

use memmap2::{MmapMut, MmapOptions};
use rustix::mm::{MapFlags, ProtFlags};

/// Why a DAX cache-region operation was refused (a typed refusal, never a panic).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaxCacheError {
  /// A length or `[moffset, moffset+len)` range was zero, not page-aligned, or outside the window.
  BadRange,
  /// The host `mmap`/`munmap` refused, with its errno.
  Host(i32),
}

/// A VMM's DAX cache region, reserved whole and overlaid per mapping. One per guest device; owned by the device, so it
/// is not behind the guest-memory borrow (the SETUPMAPPING path needs both at once). The reservation is an `MmapMut`,
/// so the type is `Send` (it holds no raw pointer) and its window reads are safe slices; the device overlays sub-ranges
/// with `MAP_FIXED`, and dropping the `MmapMut` unmaps the whole window, overlays included.
pub struct DaxCacheRegion {
  /// The reservation of the VMM's cache object: an owned shared mapping the device overlays with `MAP_FIXED`.
  window: MmapMut,
  /// The page granule a map offset and length must be a multiple of.
  page: usize,
  /// Mappings established and torn down so far — non-vacuity counters a test asserts moved.
  mapped: u64,
  unmapped: u64,
}

impl std::fmt::Debug for DaxCacheRegion {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("DaxCacheRegion")
      .field("len", &self.window.len())
      .field("mapped", &self.mapped)
      .field("unmapped", &self.unmapped)
      .finish()
  }
}

impl DaxCacheRegion {
  /// Reserves the whole cache window `fd` presents as a shared mapping. `len` must be a non-zero whole number of pages.
  /// The guest sees the same object; the device overlays block pages with [`DaxCacheRegion::map`] and restores the
  /// reservation with [`DaxCacheRegion::unmap`]. Dropping the region unmaps the whole window.
  pub fn new(fd: BorrowedFd<'_>, len: u64) -> Result<Self, DaxCacheError> {
    let page = rustix::param::page_size().max(1);
    let len = usize::try_from(len).map_err(|_| DaxCacheError::BadRange)?;
    if len == 0 || !len.is_multiple_of(page) {
      return Err(DaxCacheError::BadRange);
    }
    // SAFETY: a shared mapping of the VMM's cache object `fd` (valid for this call; `map_mut` maps it and does not take
    // ownership of the descriptor). The device is the window's only user, on its shard thread; the `MAP_FIXED` overlays
    // below stay within `[0, len)`, and `MmapMut` owns and unmaps the whole range (overlays included) on drop.
    let window = unsafe {
      MmapOptions::new()
        .len(len)
        .map_mut(std::os::fd::AsRawFd::as_raw_fd(&fd))
    }
    .map_err(|e| DaxCacheError::Host(e.raw_os_error().unwrap_or(0)))?;
    Ok(DaxCacheRegion {
      window,
      page,
      mapped: 0,
      unmapped: 0,
    })
  }

  /// The window's page granule — the multiple a map offset and length must align to (the host page here, or the VMM's
  /// negotiated `FUSE_MAP_ALIGNMENT` for a message-based VMM). The device rounds a file's length up to it to bound the
  /// authorized window, so a guest never maps a whole page that lies entirely past its file.
  #[must_use]
  pub fn page(&self) -> usize {
    self.page
  }

  /// The host address of a validated `[moffset, moffset+len)` sub-range of the window (for a `MAP_FIXED` overlay), with
  /// both bounds page-aligned and the range inside the window — else [`DaxCacheError::BadRange`]. The address is formed
  /// by `wrapping_add` (not dereferenced here; it is a syscall address argument) and stays within the reservation.
  fn fixed_addr(
    &self,
    moffset: u64,
    len: u64,
  ) -> Result<(*mut core::ffi::c_void, usize), DaxCacheError> {
    let moffset = usize::try_from(moffset).map_err(|_| DaxCacheError::BadRange)?;
    let len = usize::try_from(len).map_err(|_| DaxCacheError::BadRange)?;
    if len == 0 || !moffset.is_multiple_of(self.page) || !len.is_multiple_of(self.page) {
      return Err(DaxCacheError::BadRange);
    }
    let end = moffset.checked_add(len).ok_or(DaxCacheError::BadRange)?;
    if end > self.window.len() {
      return Err(DaxCacheError::BadRange);
    }
    let addr = self
      .window
      .as_ptr()
      .wrapping_add(moffset)
      .cast::<core::ffi::c_void>()
      .cast_mut();
    Ok((addr, len))
  }

  /// Maps `len` bytes at `foffset` of block descriptor `fd` into the window at `moffset`, read-only or writable per
  /// `writable` (FUSE_SETUPMAPPING). The block is pinned by the gate, so its pages are stable for the mapping's life.
  pub fn map(
    &mut self,
    moffset: u64,
    len: u64,
    fd: BorrowedFd<'_>,
    foffset: u64,
    writable: bool,
  ) -> Result<(), DaxCacheError> {
    let (addr, len) = self.fixed_addr(moffset, len)?;
    let prot = if writable {
      ProtFlags::READ | ProtFlags::WRITE
    } else {
      ProtFlags::READ
    };
    // SAFETY: `addr` is a page-aligned sub-range strictly within the window reservation (`fixed_addr`); `MAP_FIXED`
    // replaces only that sub-range (never memory outside the reservation); `fd` is the pinned content block's object,
    // valid and stable for this call and while the mapping lives. The reservation's `MmapMut` still owns the range.
    unsafe {
      rustix::mm::mmap(
        addr,
        len,
        prot,
        MapFlags::SHARED | MapFlags::FIXED,
        fd,
        foffset,
      )
    }
    .map_err(|e| DaxCacheError::Host(e.raw_os_error()))?;
    self.mapped = self.mapped.saturating_add(1);
    Ok(())
  }

  /// Restores `[moffset, moffset+len)` to an inert, inaccessible mapping (FUSE_REMOVEMAPPING, and the teardown half of a
  /// revoke): a fresh anonymous `MAP_FIXED` over the sub-range, so the block's descriptor is no longer reachable through
  /// the window and a stale guest access faults. The caller unpins the block only after this returns.
  pub fn unmap(&mut self, moffset: u64, len: u64) -> Result<(), DaxCacheError> {
    let (addr, len) = self.fixed_addr(moffset, len)?;
    // SAFETY: as `map` — a page-aligned sub-range within the reservation; `MAP_FIXED | ANON` replaces it with a
    // private, inaccessible mapping, detaching the block's descriptor from the window. The reservation still owns it.
    unsafe {
      rustix::mm::mmap_anonymous(
        addr,
        len,
        ProtFlags::empty(),
        MapFlags::PRIVATE | MapFlags::FIXED,
      )
    }
    .map_err(|e| DaxCacheError::Host(e.raw_os_error()))?;
    self.unmapped = self.unmapped.saturating_add(1);
    Ok(())
  }

  /// The window bytes at `[moffset, moffset+len)` — how a simulated guest reads its DAX window (a real guest's CPU
  /// reaches the same pages directly). Sub-page ranges are allowed. `None` for an out-of-window range. A safe slice of
  /// the owned mapping (the `MAP_FIXED` overlays stay within it), so no `unsafe` and no fault for an in-window range.
  #[must_use]
  pub fn window_bytes(&self, moffset: u64, len: u64) -> Option<&[u8]> {
    let moffset = usize::try_from(moffset).ok()?;
    let len = usize::try_from(len).ok()?;
    let end = moffset.checked_add(len)?;
    self.window.get(moffset..end)
  }

  /// The window bytes at `[moffset, moffset+len)`, writable — how a simulated guest writes its DAX window.
  #[must_use]
  pub fn window_bytes_mut(&mut self, moffset: u64, len: u64) -> Option<&mut [u8]> {
    let moffset = usize::try_from(moffset).ok()?;
    let len = usize::try_from(len).ok()?;
    let end = moffset.checked_add(len)?;
    self.window.get_mut(moffset..end)
  }

  /// The window length in bytes.
  #[must_use]
  pub fn len(&self) -> usize {
    self.window.len()
  }

  /// Whether the window is empty (never, for a constructed region).
  #[must_use]
  pub fn is_empty(&self) -> bool {
    self.window.is_empty()
  }

  /// Mappings established so far (non-vacuity).
  #[must_use]
  pub fn mapped(&self) -> u64 {
    self.mapped
  }

  /// Mappings torn down so far (non-vacuity).
  #[must_use]
  pub fn unmapped(&self) -> u64 {
    self.unmapped
  }
}

/// How a device places a content block's pages into a guest's DAX window (§4.6), chosen at admission by the kind of
/// VMM the device serves. The device holds one of these when DAX was offered; `serve_dax` and the teardown paths drive
/// it. A DAX window is delivered two ways (§4.6, [`crate::vhost_user`]); this names which.
pub enum DaxPlacer {
  /// An **in-process / fd-sharing** VMM (libkrun, and the sim): the VMM shared the cache window as a memory object, so
  /// the device reserved it whole and overlays each content block itself with `MAP_FIXED` ([`DaxCacheRegion`]). A map
  /// and an unmap complete synchronously on the placing call — there is no separate front-end process to await. The
  /// **message-based** sibling (an out-of-process crosvm/QEMU/cloud-hypervisor, where the device sends
  /// `VHOST_USER_BACKEND_SHMEM_MAP` over the back-end-request channel and the front end maps the block into the guest's
  /// BAR, completing on the front end's acknowledgement) is added next (Piece C).
  Local(DaxCacheRegion),
}

#[cfg(test)]
mod tests {
  // Test harness code: an unwrap here is a failed test.
  #![allow(clippy::unwrap_used, clippy::expect_used)]
  use super::*;
  use slates_mem::{SharedObject, Words};

  fn name(tag: &str) -> String {
    format!("sl-dax-{tag}-{}", std::process::id())
  }

  /// The host page size (16 KiB on Apple Silicon, 4 KiB on x86), and a window of a few pages — map offsets and lengths
  /// must be multiples of the real page, which the region enforces.
  fn page() -> usize {
    rustix::param::page_size().max(1)
  }
  fn window() -> usize {
    8 * page()
  }

  /// §4.6 (the DAX cache region, fd-sharing form). Do: reserve a cache window, map a content block's descriptor into
  /// it, read the window and write through it, then unmap. Expect: the window shows the block's bytes (a real shared
  /// mapping — the guest would reach them through its own page table); a write through the window reaches the block;
  /// the map/unmap counters move. This is the core DAX mechanic proven without a VMM.
  #[test]
  fn the_window_maps_a_block_and_write_through_reaches_it() {
    let (page, window) = (page(), window());
    let cache = SharedObject::create(&name("cache"), window, Words::new()).unwrap();
    let mut block = SharedObject::create(&name("block"), page, Words::new()).unwrap();
    block.write(0, &[0x5A; 64]).unwrap();

    let mut region = DaxCacheRegion::new(cache.as_fd(), window as u64).unwrap();
    // Map the block writable at the second page of the window.
    region
      .map(page as u64, page as u64, block.as_fd(), 0, true)
      .unwrap();

    // The window shows the block's bytes (shared mapping of the same object).
    let seen = region.window_bytes(page as u64, 64).unwrap();
    assert_eq!(seen, &[0x5A; 64], "the window reaches the block's bytes");

    // A write through the window reaches the block (the guest's writable DAX).
    region
      .window_bytes_mut(page as u64, 4)
      .unwrap()
      .copy_from_slice(b"HERE");
    let mut back = [0u8; 4];
    block.read(0, &mut back).unwrap();
    assert_eq!(
      &back, b"HERE",
      "a write through the window reached the block"
    );

    region.unmap(page as u64, page as u64).unwrap();
    assert_eq!(region.mapped(), 1, "one mapping established");
    assert_eq!(region.unmapped(), 1, "one mapping torn down");
  }

  /// §4.6 (bounds). Do: map and unmap ranges outside the window, unaligned, or zero-length. Expect: each refused
  /// `BadRange`, never a map past the reservation (a hostile or confused guest cannot reach beyond its window).
  #[test]
  fn a_range_outside_the_window_or_unaligned_is_refused() {
    let (page, window) = (page(), window());
    let cache = SharedObject::create(&name("bounds-c"), window, Words::new()).unwrap();
    let block = SharedObject::create(&name("bounds-b"), page, Words::new()).unwrap();
    let mut region = DaxCacheRegion::new(cache.as_fd(), window as u64).unwrap();

    // Past the window.
    assert_eq!(
      region.map(window as u64, page as u64, block.as_fd(), 0, false),
      Err(DaxCacheError::BadRange)
    );
    // Straddling the end.
    assert_eq!(
      region.map(
        (window - page) as u64,
        (2 * page) as u64,
        block.as_fd(),
        0,
        false
      ),
      Err(DaxCacheError::BadRange)
    );
    // Unaligned offset.
    assert_eq!(
      region.map(1, page as u64, block.as_fd(), 0, false),
      Err(DaxCacheError::BadRange)
    );
    // Zero length.
    assert_eq!(
      region.map(0, 0, block.as_fd(), 0, false),
      Err(DaxCacheError::BadRange)
    );
    assert_eq!(region.mapped(), 0, "nothing was mapped");
    // Unmap of an out-of-window range is refused too.
    assert_eq!(
      region.unmap(window as u64, page as u64),
      Err(DaxCacheError::BadRange)
    );
  }
}
