//! Large allocations mapped directly and unmapped when freed (§4.2 "memory returns to the OS"; A-742, main's
//! ruling of 2026-11-20: freed memory returns to the OS, never by allocator luck).
//!
//! **Why.** A 1,000-revision Subversion import (git/git, 223 MiB) left the daemon at 2,435 to 2,776 MB of footprint,
//! about 900 MiB of it in the system allocator's regions holding no live allocation: `vmmap` named them `MALLOC_LARGE
//! (empty)` 712.6 MiB, `MALLOC_REALLOC (empty)` 135.9 MiB and `MALLOC_SMALL (empty)` 47.0 MiB. The import made and freed
//! 4,188 MiB in 698 blocks of a MiB or more (register entries, checkpoints, chain entries and snapshots encoded, cloned
//! and decoded a revision at a time), and the system allocator keeps freed large blocks for reuse, giving them back on
//! its own schedule: five seconds later the footprint had fallen to 1,565 to 1,834 MB in three runs and not at all in two
//! others. `malloc_zone_pressure_relief` released 0 bytes. Measured on the Mac Studio, 2026-11-20.
//!
//! **What it does.** [`LargeMapped`] is the global allocator the `slates` binary installs. An allocation of at least
//! [`floor`] bytes whose alignment a page satisfies is a fresh anonymous mapping, rounded up to whole pages, and its free
//! unmaps it, so its pages go back to the OS when it is freed. Every other allocation is the system allocator's. Which
//! path a block takes is decided by its layout alone, so an allocation and its free always agree; a reallocation across
//! the floor moves the block. The same import then ended at 1,562 to 1,645 MB at once, but took 2.04 to 2.87 s against
//! 1.81 to 1.90 s: every large temporary was now fresh zero-filled pages (5.77 GB faulted in). With the temporaries made
//! fewer and sized once (A-742's other half: `Wire::encoded_len`, `Db::mutate_owned`, `Checkpoint::into_encoded`), it
//! ends at 1,431 to 1,495 MB against 2,417 to 2,551 MB, in 1.69 to 1.78 s against 1.73 to 1.83 s (BENCHMARKS.md).
//!
//! **Rejected.** Asking the system allocator to give its free pages back after a large step
//! (`malloc_zone_pressure_relief`) released 0 bytes; and a bounded cache of freed mappings would keep pages the OS could
//! have, which is the luck this module removes.
//!
//! **Bound.** The floor is [`FLOOR_PAGES`] pages, so a mapped block wastes at most one page in that many to rounding.
//! The counters ([`stats`]) are relaxed statistics on a path taken by large blocks alone (698 in the import above),
//! never a per-item path.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Derived: a block mapped directly is at least this many pages, so page rounding wastes at most one page in this many
/// (1/64, 1.6 %) of its mapping. On 16 KiB pages (Apple silicon) the floor is 1 MiB; on 4 KiB pages, 256 KiB.
pub const FLOOR_PAGES: usize = 64;

/// The OS page size, read once (zero until then).
static PAGE: AtomicUsize = AtomicUsize::new(0);

/// The counters (module docs, "Bound").
static MAPPED: AtomicU64 = AtomicU64::new(0);
static UNMAPPED: AtomicU64 = AtomicU64::new(0);
static MAPPED_BYTES: AtomicU64 = AtomicU64::new(0);
static RETURNED_BYTES: AtomicU64 = AtomicU64::new(0);
static REFUSED: AtomicU64 = AtomicU64::new(0);

/// What the large path has done since the process began: the give-back counters `slates status` reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LargeStats {
  /// Blocks mapped.
  pub mapped: u64,
  /// Blocks unmapped (freed).
  pub unmapped: u64,
  /// Bytes mapped now (blocks mapped less blocks unmapped, in whole pages).
  pub mapped_bytes: u64,
  /// Bytes given back to the OS by unmapping, in whole pages.
  pub returned_bytes: u64,
  /// Mappings the OS refused (each an allocation failure the caller sees).
  pub refused: u64,
}

/// The counters now.
pub fn stats() -> LargeStats {
  let mapped_bytes = MAPPED_BYTES.load(Ordering::Relaxed);
  let returned_bytes = RETURNED_BYTES.load(Ordering::Relaxed);
  LargeStats {
    mapped: MAPPED.load(Ordering::Relaxed),
    unmapped: UNMAPPED.load(Ordering::Relaxed),
    mapped_bytes: mapped_bytes.saturating_sub(returned_bytes),
    returned_bytes,
    refused: REFUSED.load(Ordering::Relaxed),
  }
}

/// The OS page size.
fn page() -> usize {
  match PAGE.load(Ordering::Relaxed) {
    0 => {
      let page = os::page_size().max(1);
      PAGE.store(page, Ordering::Relaxed);
      page
    }
    page => page,
  }
}

/// The smallest allocation mapped directly: [`FLOOR_PAGES`] pages.
pub fn floor() -> usize {
  page().saturating_mul(FLOOR_PAGES)
}

/// Whether `layout` takes the mapped path.
fn mapped(layout: Layout) -> bool {
  layout.size() >= floor() && layout.align() <= page()
}

/// `len` rounded up to whole pages; `None` past the address space.
fn rounded(len: usize) -> Option<usize> {
  let page = page();
  len.div_ceil(page).checked_mul(page)
}

/// A fresh mapping of at least `len` bytes, zeroed, counted; null when the OS refuses it.
fn map(len: usize) -> *mut u8 {
  let Some(span) = rounded(len) else {
    REFUSED.fetch_add(1, Ordering::Relaxed);
    return std::ptr::null_mut();
  };
  let block = os::map(span);
  if block.is_null() {
    REFUSED.fetch_add(1, Ordering::Relaxed);
    return block;
  }
  MAPPED.fetch_add(1, Ordering::Relaxed);
  MAPPED_BYTES.fetch_add(u64::try_from(span).unwrap_or(u64::MAX), Ordering::Relaxed);
  block
}

/// Unmaps the block [`map`] made for `len` bytes, counted.
///
/// # Safety
/// `block` is a mapping [`map`] returned for `len` bytes, not yet unmapped.
unsafe fn unmap(block: *mut u8, len: usize) {
  let Some(span) = rounded(len) else {
    return;
  };
  // SAFETY: the caller's promise: `block` is `map`'s mapping of `span` bytes, unmapped once.
  unsafe { os::unmap(block, span) };
  UNMAPPED.fetch_add(1, Ordering::Relaxed);
  RETURNED_BYTES.fetch_add(u64::try_from(span).unwrap_or(u64::MAX), Ordering::Relaxed);
}

/// The global allocator (module docs, "What it does").
#[derive(Debug, Default)]
pub struct LargeMapped;

// SAFETY: a mapped block is a fresh, page-aligned, zeroed mapping of at least the layout's size, which no other block
// overlaps, and is unmapped only by the free of the same layout, which takes the same path (the layout decides it); a
// block below the floor is the system allocator's throughout, which upholds `GlobalAlloc` itself.
unsafe impl GlobalAlloc for LargeMapped {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    if mapped(layout) {
      return map(layout.size());
    }
    // SAFETY: the caller's layout, forwarded.
    unsafe { System.alloc(layout) }
  }

  unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
    if mapped(layout) {
      // A fresh anonymous mapping is zeroed already.
      return map(layout.size());
    }
    // SAFETY: the caller's layout, forwarded.
    unsafe { System.alloc_zeroed(layout) }
  }

  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    if mapped(layout) {
      // SAFETY: the layout took the mapped path when `ptr` was allocated (it decides the path), so `ptr` is `map`'s.
      unsafe { unmap(ptr, layout.size()) };
      return;
    }
    // SAFETY: the system allocator's block, with the layout it was allocated with.
    unsafe { System.dealloc(ptr, layout) }
  }

  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    let Ok(new_layout) = Layout::from_size_align(new_size, layout.align()) else {
      return std::ptr::null_mut();
    };
    match (mapped(layout), mapped(new_layout)) {
      // SAFETY: the system allocator's block on both sides, forwarded.
      (false, false) => unsafe { System.realloc(ptr, layout, new_size) },
      (true, true) if rounded(layout.size()) == rounded(new_size) => ptr,
      _ => {
        // SAFETY: a new block by its own path, the bytes both hold copied (the blocks are distinct), then the old
        // block freed by its own path.
        unsafe {
          let moved = self.alloc(new_layout);
          if !moved.is_null() {
            std::ptr::copy_nonoverlapping(ptr, moved, layout.size().min(new_size));
            self.dealloc(ptr, layout);
          }
          moved
        }
      }
    }
  }
}

#[cfg(unix)]
mod os {
  /// The OS page size.
  pub(super) fn page_size() -> usize {
    rustix::param::page_size()
  }

  /// An anonymous private read-write mapping of `span` bytes; null when refused.
  pub(super) fn map(span: usize) -> *mut u8 {
    // SAFETY: an anonymous mapping at an address the OS chooses overlaps nothing the process holds.
    let mapped = unsafe {
      rustix::mm::mmap_anonymous(
        std::ptr::null_mut(),
        span,
        rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
        rustix::mm::MapFlags::PRIVATE,
      )
    };
    mapped.map_or(std::ptr::null_mut(), |block| block.cast())
  }

  /// Unmaps `span` bytes at `block`.
  ///
  /// # Safety
  /// `block` is a mapping [`map`] made of `span` bytes.
  pub(super) unsafe fn unmap(block: *mut u8, span: usize) {
    // SAFETY: the caller's promise. A refusal (impossible for a whole mapping) leaves the pages mapped, never freed twice.
    let _ = unsafe { rustix::mm::munmap(block.cast(), span) };
  }
}

#[cfg(windows)]
mod os {
  use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
  };
  use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};

  /// The OS page size.
  pub(super) fn page_size() -> usize {
    let mut info = SYSTEM_INFO::default();
    // SAFETY: `GetSystemInfo` fills the structure it is given.
    unsafe { GetSystemInfo(&mut info) };
    usize::try_from(info.dwPageSize).unwrap_or(1)
  }

  /// A reserved and committed read-write range of `span` bytes; null when refused.
  pub(super) fn map(span: usize) -> *mut u8 {
    // SAFETY: a range at an address the OS chooses overlaps nothing the process holds.
    unsafe {
      VirtualAlloc(
        std::ptr::null(),
        span,
        MEM_RESERVE | MEM_COMMIT,
        PAGE_READWRITE,
      )
      .cast()
    }
  }

  /// Releases the range at `block`.
  ///
  /// # Safety
  /// `block` is a range [`map`] made.
  pub(super) unsafe fn unmap(block: *mut u8, _span: usize) {
    // SAFETY: the caller's promise; a release takes the whole reservation by its base and a size of 0.
    let _ = unsafe { VirtualFree(block.cast(), 0, MEM_RELEASE) };
  }
}

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

  use super::*;

  /// A-742. Do: through the allocator itself, allocate a block at the floor, write it, grow it to twice its size,
  /// shrink it below the floor, free it; and allocate and free a small block. Expect: the large blocks are mapped (two:
  /// the first and the grown one) and both unmapped by the end, the bytes mapped now back where they started and the
  /// bytes returned grown by both mappings; every byte written survives each move; the small block never maps.
  #[test]
  fn a_large_block_is_mapped_moved_and_returned_whole() {
    let allocator = LargeMapped;
    let floor = floor();
    let before = stats();
    let layout = Layout::from_size_align(floor, 8).unwrap();
    // SAFETY (test): the layouts are valid and each block is freed once, by the layout it was last given.
    unsafe {
      let block = allocator.alloc(layout);
      assert!(!block.is_null());
      std::slice::from_raw_parts_mut(block, floor).fill(7);
      let grown = allocator.realloc(block, layout, floor * 2);
      assert!(!grown.is_null());
      assert!(
        std::slice::from_raw_parts(grown, floor)
          .iter()
          .all(|byte| *byte == 7)
      );
      let small = allocator.realloc(grown, Layout::from_size_align(floor * 2, 8).unwrap(), 64);
      assert!(
        std::slice::from_raw_parts(small, 64)
          .iter()
          .all(|byte| *byte == 7)
      );
      allocator.dealloc(small, Layout::from_size_align(64, 8).unwrap());
      let tiny = allocator.alloc(Layout::from_size_align(16, 8).unwrap());
      allocator.dealloc(tiny, Layout::from_size_align(16, 8).unwrap());
    }
    let after = stats();
    // Other tests in the process may map blocks meanwhile; this test's own moves are at least these.
    assert!(after.mapped >= before.mapped + 2, "{before:?} -> {after:?}");
    assert!(
      after.unmapped >= before.unmapped + 2,
      "{before:?} -> {after:?}"
    );
    let ours = u64::try_from(floor * 3).unwrap();
    assert!(
      after.returned_bytes >= before.returned_bytes + ours,
      "{before:?} -> {after:?}"
    );
  }

  /// A-742. Do: allocate zeroed blocks above and below the floor. Expect: both read zero.
  #[test]
  fn zeroed_blocks_read_zero_on_both_paths() {
    let allocator = LargeMapped;
    for size in [64, floor(), floor() + 1] {
      let layout = Layout::from_size_align(size, 8).unwrap();
      // SAFETY (test): a valid layout, freed once.
      unsafe {
        let block = allocator.alloc_zeroed(layout);
        assert!(
          std::slice::from_raw_parts(block, size)
            .iter()
            .all(|byte| *byte == 0)
        );
        allocator.dealloc(block, layout);
      }
    }
  }
}
