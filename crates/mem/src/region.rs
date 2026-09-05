//! A region: an anonymous private mapping, optionally huge-page advised, optionally locked,
//! whose pages are faulted in by the pre-fault scheduler rather than on the hot path (§4.2,
//! `LockedRegion`).
//!
//! Regions are the only memory the chunk arena hands out. A region knows its NUMA node when the
//! OS says (recorded, not acted on until Phase 8 measures a placement policy). Locking is a
//! separate step ([`Region::lock`]) so the locking sequence of [`crate::lock`] can run it in
//! priority order and report refusals per region (D-12: "RAM-only policy per OS with honest
//! degradation").

use std::ptr::NonNull;

use crate::error::MemError;

/// A mapped region.
#[derive(Debug)]
pub struct Region {
  base: NonNull<u8>,
  len: usize,
  page: usize,
  locked: bool,
  huge: bool,
  numa_node: u16,
}

// SAFETY: a region is a plain mapping; it may move between threads (the arena that owns it is
// single-shard, and the runtime moves whole shards, never shares them).
unsafe impl Send for Region {}

impl Region {
  /// Maps `len` bytes (rounded up to whole pages of `page` bytes). `huge` asks the OS to back
  /// the region with transparent huge pages where that exists (Linux `MADV_HUGEPAGE`).
  pub fn map(len: usize, page: usize, huge: bool) -> Result<Region, MemError> {
    let page = page.max(1);
    let len = len.max(page).next_multiple_of(page);
    let base = os::map(len)?;
    let huge = huge && os::advise_huge(base, len);
    Ok(Region {
      base,
      len,
      page,
      locked: false,
      huge,
      numa_node: 0,
    })
  }

  /// The base address.
  pub const fn base(&self) -> NonNull<u8> {
    self.base
  }

  /// Length in bytes.
  pub const fn len(&self) -> usize {
    self.len
  }

  /// Whether the region is empty (never, for a mapped region).
  pub const fn is_empty(&self) -> bool {
    self.len == 0
  }

  /// The page size the region was mapped with.
  pub const fn page(&self) -> usize {
    self.page
  }

  /// Whether the region is locked in RAM.
  pub const fn locked(&self) -> bool {
    self.locked
  }

  /// Whether the OS accepted the huge-page advice.
  pub const fn huge(&self) -> bool {
    self.huge
  }

  /// The NUMA node the region was recorded on.
  pub const fn numa_node(&self) -> u16 {
    self.numa_node
  }

  /// Records the NUMA node.
  pub const fn set_numa_node(&mut self, node: u16) {
    self.numa_node = node;
  }

  /// Locks the region in RAM. On refusal the region stays mapped and usable, unlocked, and the
  /// error says how many bytes the OS locked before refusing (none: locking is all-or-nothing
  /// per call, and this call is one region).
  pub fn lock(&mut self) -> Result<(), MemError> {
    if self.locked {
      return Ok(());
    }
    os::lock(self.base, self.len)?;
    self.locked = true;
    Ok(())
  }

  /// Unlocks the region.
  pub fn unlock(&mut self) {
    if self.locked {
      os::unlock(self.base, self.len);
      self.locked = false;
    }
  }

  /// Touches one byte per page in `[from_page, to_page)` so the pages are resident. Called by
  /// the pre-fault scheduler in batches; safe to call on any page range within the region.
  pub fn touch_pages(&mut self, from_page: usize, to_page: usize) {
    let pages = self.len / self.page;
    let to = to_page.min(pages);
    let mut p = from_page;
    while p < to {
      let at = p * self.page;
      // SAFETY: `at < len` because `p < pages`; the mapping is private and writable, and a
      // volatile zero write is idempotent on fresh anonymous memory.
      unsafe { self.base.as_ptr().add(at).write_volatile(0) };
      p += 1;
    }
  }

  /// Pages in the region.
  pub const fn pages(&self) -> usize {
    self.len / self.page
  }

  /// The bytes as a slice; the caller owns the region, so this is the single access path.
  pub fn bytes(&self) -> &[u8] {
    // SAFETY: the mapping is `len` readable bytes for the life of `self`.
    unsafe { std::slice::from_raw_parts(self.base.as_ptr(), self.len) }
  }

  /// The bytes, mutably.
  pub fn bytes_mut(&mut self) -> &mut [u8] {
    // SAFETY: the mapping is `len` writable bytes and `&mut self` makes this the only access.
    unsafe { std::slice::from_raw_parts_mut(self.base.as_ptr(), self.len) }
  }
}

impl Drop for Region {
  fn drop(&mut self) {
    self.unlock();
    os::unmap(self.base, self.len);
  }
}

/// Whether huge pages pay on this machine: the profile measured a transparent-huge-page region
/// faulting cheaper per base page than base pages do (Linux only; elsewhere the OS offers none).
pub fn huge_pages_beneficial(
  profile: &slates_machine::MachineProfile,
) -> slates_machine::Derived<bool> {
  let base = profile.faults.base_ns.max(1);
  slates_machine::derived!(
    profile.faults.huge_ns.is_some_and(|huge| huge < base),
    "huge-page fault cost per base page < base-page fault cost",
    ["faults.huge_ns", "faults.base_ns"]
  )
}

impl Region {
  /// Maps a region with the page facts and the huge-page decision taken from the profile.
  pub fn map_for_profile(
    len: usize,
    profile: &slates_machine::MachineProfile,
  ) -> Result<Region, MemError> {
    let page = usize::try_from(profile.facts.page.base).unwrap_or(1);
    Region::map(len, page, huge_pages_beneficial(profile).get())
  }
}

/// What the OS reports as locked for this process, for the AC-0.5 cross-check (the query
/// lives in `slates-machine`, the crate allowed to read the kernel's pseudo-files).
pub fn os_locked_bytes() -> Option<u64> {
  slates_machine::facts::locked_bytes()
}

#[cfg(unix)]
mod os {
  use super::MemError;
  use std::ffi::c_void;
  use std::ptr::NonNull;

  pub(super) fn map(len: usize) -> Result<NonNull<u8>, MemError> {
    // SAFETY: an anonymous private mapping; the result is checked.
    let p = unsafe {
      libc::mmap(
        std::ptr::null_mut(),
        len,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
        -1,
        0,
      )
    };
    if p == libc::MAP_FAILED {
      return Err(MemError::RegionRefused {
        len,
        code: std::io::Error::last_os_error().raw_os_error(),
      });
    }
    NonNull::new(p.cast::<u8>()).ok_or(MemError::RegionRefused { len, code: None })
  }

  pub(super) fn unmap(base: NonNull<u8>, len: usize) {
    // SAFETY: `base`/`len` are the mapping from `map`.
    unsafe { libc::munmap(base.as_ptr().cast::<c_void>(), len) };
  }

  #[cfg(target_os = "linux")]
  pub(super) fn advise_huge(base: NonNull<u8>, len: usize) -> bool {
    // SAFETY: the mapping from `map`; the advice is a hint the kernel may ignore.
    unsafe { libc::madvise(base.as_ptr().cast::<c_void>(), len, libc::MADV_HUGEPAGE) == 0 }
  }

  #[cfg(not(target_os = "linux"))]
  pub(super) fn advise_huge(_base: NonNull<u8>, _len: usize) -> bool {
    false
  }

  pub(super) fn lock(base: NonNull<u8>, len: usize) -> Result<(), MemError> {
    // SAFETY: the mapping from `map`.
    if unsafe { libc::mlock(base.as_ptr().cast::<c_void>(), len) } == 0 {
      Ok(())
    } else {
      Err(MemError::LockRefused {
        requested: len,
        locked: 0,
        code: std::io::Error::last_os_error().raw_os_error(),
      })
    }
  }

  pub(super) fn unlock(base: NonNull<u8>, len: usize) {
    // SAFETY: the mapping from `map`, locked by `lock`.
    unsafe { libc::munlock(base.as_ptr().cast::<c_void>(), len) };
  }
}

#[cfg(windows)]
mod os {
  use super::MemError;
  use std::ffi::c_void;
  use std::ptr::NonNull;
  use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree, VirtualLock,
    VirtualUnlock,
  };
  use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessWorkingSetSize, SetProcessWorkingSetSize,
  };

  pub(super) fn map(len: usize) -> Result<NonNull<u8>, MemError> {
    // SAFETY: a fresh committed read/write region; the result is checked.
    let p = unsafe {
      VirtualAlloc(
        std::ptr::null(),
        len,
        MEM_RESERVE | MEM_COMMIT,
        PAGE_READWRITE,
      )
    };
    NonNull::new(p.cast::<u8>()).ok_or_else(|| MemError::RegionRefused {
      len,
      code: std::io::Error::last_os_error().raw_os_error(),
    })
  }

  pub(super) fn unmap(base: NonNull<u8>, _len: usize) {
    // SAFETY: `base` came from VirtualAlloc.
    unsafe { VirtualFree(base.as_ptr().cast::<c_void>(), 0, MEM_RELEASE) };
  }

  pub(super) fn advise_huge(_base: NonNull<u8>, _len: usize) -> bool {
    false
  }

  /// VirtualLock is bounded by the working set: raise the maximum by the region first.
  pub(super) fn lock(base: NonNull<u8>, len: usize) -> Result<(), MemError> {
    let mut min: usize = 0;
    let mut max: usize = 0;
    // SAFETY: the current process pseudo-handle and two writable outputs.
    if unsafe { GetProcessWorkingSetSize(GetCurrentProcess(), &raw mut min, &raw mut max) } != 0 {
      // SAFETY: raising both bounds by the region; a refusal is reported by VirtualLock below.
      unsafe {
        SetProcessWorkingSetSize(
          GetCurrentProcess(),
          min.saturating_add(len),
          max.saturating_add(len),
        )
      };
    }
    // SAFETY: the committed region from `map`.
    if unsafe { VirtualLock(base.as_ptr().cast::<c_void>(), len) } != 0 {
      Ok(())
    } else {
      Err(MemError::LockRefused {
        requested: len,
        locked: 0,
        code: std::io::Error::last_os_error().raw_os_error(),
      })
    }
  }

  pub(super) fn unlock(base: NonNull<u8>, len: usize) {
    // SAFETY: the region locked by `lock`.
    unsafe { VirtualUnlock(base.as_ptr().cast::<c_void>(), len) };
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_region_maps_rounds_to_pages_touches_and_unmaps() {
    let page = slates_machine::facts::Facts::query().page.base;
    let page = usize::try_from(page).unwrap();
    let mut r = Region::map(page * 3 + 1, page, false).unwrap();
    assert_eq!(r.len(), page * 4);
    assert_eq!(r.pages(), 4);
    assert!(!r.is_empty());
    r.touch_pages(0, 4);
    r.bytes_mut()[page * 2] = 7;
    assert_eq!(r.bytes()[page * 2], 7);
    assert!(!r.locked());
    assert_eq!(r.numa_node(), 0);
    r.set_numa_node(1);
    assert_eq!(r.numa_node(), 1);
  }

  #[test]
  fn the_huge_page_decision_follows_the_measured_fault_costs() {
    let options = slates_machine::ProfileOptions {
      budget_per_probe: std::time::Duration::from_millis(20),
      codecs: false,
      core_matrix: false,
    };
    let mut profile = slates_machine::MachineProfile::measure(options);
    profile.faults.base_ns = 1000;
    profile.faults.huge_ns = Some(100);
    assert!(huge_pages_beneficial(&profile).get());
    profile.faults.huge_ns = Some(1000);
    assert!(!huge_pages_beneficial(&profile).get());
    profile.faults.huge_ns = None;
    assert!(!huge_pages_beneficial(&profile).get());
    let r = Region::map_for_profile(1, &profile).unwrap();
    assert_eq!(r.len(), usize::try_from(profile.facts.page.base).unwrap());
  }

  #[test]
  fn locking_a_small_region_is_reported_by_the_os_within_one_page() {
    let page = usize::try_from(slates_machine::facts::Facts::query().page.base).unwrap();
    let mut r = Region::map(page * 64, page, false).unwrap();
    r.touch_pages(0, r.pages());
    let Some(before) = os_locked_bytes() else {
      eprintln!("skipping: the OS does not report locked bytes here");
      return;
    };
    r.lock().unwrap();
    assert!(r.locked());
    let after = os_locked_bytes().unwrap();
    let delta = after.saturating_sub(before);
    let len = u64::try_from(r.len()).unwrap();
    let page = u64::try_from(page).unwrap();
    assert!(
      delta + page >= len && delta <= len + page * 8,
      "locked delta {delta} for a {len}-byte region"
    );
    r.unlock();
    assert!(!r.locked());
  }
}
