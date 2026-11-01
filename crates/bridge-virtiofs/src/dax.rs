//! The DAX cache region (§4.6, A-9, A-168): the host-side window the fd-sharing placer maps per-file backings into, so a
//! guest reaches a file's bytes through its own page table with no daemon call (FUSE_SETUPMAPPING / FUSE_REMOVEMAPPING).
//!
//! This is the **fd-sharing** form: the VMM shares the cache window as a memory object, the device reserves the whole
//! window at construction (`PROT_NONE`, inaccessible), and each SETUPMAPPING overlays page-aligned sub-ranges with a
//! shared mapping of the file's backing (`MAP_FIXED`), one overlay a piece of the window (`crate::backed`). A
//! REMOVEMAPPING restores the inert reservation, so a guest's stale page table reaches a faulting hole. The backing is the
//! file's own DAX object, which the volume's write boundary keeps equal to the file before any change returns, so nothing
//! the daemon does to the file ever needs a mapping torn down. An in-process VMM (libkrun) shares its window this way,
//! and the sim exercises it end to end without a VMM; an out-of-process VMM (crosvm, QEMU) is sent each piece over
//! vhost-user `SHMEM_MAP` instead (`crate::vhost_user`, Linux). Both map the same backings in the same pieces (A-195).
//!
//! Unix only (the primitive is exercised on macOS over a shared-memory object).
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

  /// Whether `[moffset, moffset+len)` is a range this window can map: non-empty, page-aligned at both ends and inside the
  /// window; else [`DaxCacheError::BadRange`].
  pub fn check_window(&self, moffset: u64, len: u64) -> Result<(), DaxCacheError> {
    self.fixed_addr(moffset, len).map(|_| ())
  }

  /// Maps `len` bytes at `foffset` of the backing descriptor `fd` into the window at `moffset`, read-only or writable per
  /// `writable` (one piece of a FUSE_SETUPMAPPING's window). The mapping holds the backing's object for its own life.
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
    // replaces only that sub-range (never memory outside the reservation); `fd` is a file's DAX object, valid for this
    // call, which no process resizes while a view of it lives (`slates_mem::DaxObject`). No reference to the mapped
    // bytes is formed here, and the reservation's `MmapMut` still owns the range.
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

  /// Restores `[moffset, moffset+len)` to an inert, inaccessible mapping (FUSE_REMOVEMAPPING, and the teardown): a fresh
  /// anonymous `MAP_FIXED` over the sub-range, so the backing is no longer reachable through the window and a stale guest
  /// access faults.
  pub fn unmap(&mut self, moffset: u64, len: u64) -> Result<(), DaxCacheError> {
    let (addr, len) = self.fixed_addr(moffset, len)?;
    // SAFETY: as `map` — a page-aligned sub-range within the reservation; `MAP_FIXED | ANON` replaces it with a
    // private, inaccessible mapping, detaching the backing from the window. The reservation still owns it.
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
  /// reaches the same pages directly). Sub-page ranges are allowed. `None` for an out-of-window range.
  ///
  /// **Reading a torn-down range faults.** The slice is in-bounds of the reservation, so this is `unsafe`-free and
  /// bounds-checked, but [`DaxCacheRegion::unmap`] replaces a slot with a `PROT_NONE` anonymous mapping — by
  /// design, so a stale window reaches nothing — and touching those bytes raises a fault the bounds check cannot
  /// see. An earlier version of this line promised "no fault for an in-window range", which is true only while
  /// every page of the range is mapped; a chaos schedule that unmapped a slot and then read it took `SIGBUS`. A
  /// caller that scans the region must know which slots are live, or read only what it has just mapped.
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

/// How a device places a file's backing into a guest's DAX window (§4.6, A-168), chosen at admission by the kind of VMM
/// the device serves. The device holds one of these when DAX was offered; both map the same backings in the same pieces
/// (`crate::backed::window_pieces`), and differ only in who maps them.
pub enum DaxPlacer {
  /// An **in-process / fd-sharing** VMM (libkrun, and the sim): the VMM shared the cache window as a memory object, so
  /// the device reserved it whole and overlays each piece of a window itself with `MAP_FIXED` ([`DaxCacheRegion`]). A map
  /// and an unmap complete on the placing call — there is no separate front-end process to await.
  Local(DaxCacheRegion),
  /// An **out-of-process / message-based** VMM (crosvm, QEMU): the back end does not own the window; it asks the front
  /// end to map each piece over the back-end-request channel and awaits the reply-acks
  /// ([`crate::vhost_user::MessagePlacer`]). Linux only (the back-end-request channel is a Linux/vhost-user feature).
  #[cfg(target_os = "linux")]
  Message(crate::vhost_user::MessagePlacer<crate::backed::BackedPiece>),
}

impl DaxPlacer {
  /// The map granule — the multiple a window offset and length must be aligned to (the host page here, or a
  /// negotiated FUSE_MAP_ALIGNMENT). Both forms expose it so a SETUPMAPPING is bounded the same way.
  #[must_use]
  pub fn page(&self) -> usize {
    match self {
      DaxPlacer::Local(cache) => cache.page(),
      #[cfg(target_os = "linux")]
      DaxPlacer::Message(placer) => placer.page(),
    }
  }

  /// Whether `[moffset, moffset+len)` is a range of the guest's window this placer can map: non-empty, whole granules,
  /// inside the window.
  #[must_use]
  pub fn window_fits(&self, moffset: u64, len: u64) -> bool {
    match self {
      DaxPlacer::Local(cache) => cache.check_window(moffset, len).is_ok(),
      #[cfg(target_os = "linux")]
      DaxPlacer::Message(placer) => placer.check_window(moffset, len).is_ok(),
    }
  }
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
