//! A region: an anonymous private mapping, optionally huge-page advised, optionally locked,
//! whose pages are faulted in by the pre-fault scheduler rather than on the hot path (§4.2,
//! `LockedRegion`).
//!
//! Regions are the only memory the chunk arena hands out. The mapping is `memmap2`'s
//! `MmapMut`, whose anonymous map, advice, lock and byte access are safe functions over the same
//! `mmap`, `madvise` and `mlock` calls D-12 cites, so this module holds no unsafe code on Unix;
//! Windows locking (`VirtualLock` after raising the working set) is the one place a raw call
//! remains. A region knows its NUMA node when the OS says (recorded, not acted on until Phase 8
//! measures a placement policy). Locking is a separate step ([`Region::lock`]) so the locking
//! sequence of [`crate::lock`] can run it in priority order and report refusals per region (D-12:
//! "RAM-only policy per OS with honest degradation").

use memmap2::MmapMut;

use crate::error::MemError;
use crate::shared::SharedObject;

/// What backs a region's bytes: a private anonymous mapping (the default, process-local, gone at
/// exit), or a shared memory object (§4.7, `SharedObject`) that survives the process and is
/// re-attached by a restarted daemon through its handoff. The arena addresses bytes the same way
/// over either; only the survival and ownership differ, which is what backing content in the anchor
/// (§4.8 recovery) needs. Both expose their bytes through safe accessors, so this adds no unsafe.
#[derive(Debug)]
enum Backing {
  /// A private anonymous mapping, faulted in by the pre-fault scheduler; lost when the process
  /// exits.
  Anon(MmapMut),
  /// A shared memory object the region owns; its bytes outlive the process and a restarted daemon
  /// maps the same object, so content placed here recovers (§4.8).
  Shared(SharedObject),
}

impl Backing {
  fn bytes(&self) -> &[u8] {
    match self {
      Backing::Anon(map) => map,
      Backing::Shared(object) => object.bytes(),
    }
  }

  fn bytes_mut(&mut self) -> &mut [u8] {
    match self {
      Backing::Anon(map) => map,
      Backing::Shared(object) => object.bytes_mut(),
    }
  }
}

/// A mapped region.
#[derive(Debug)]
pub struct Region {
  backing: Backing,
  page: usize,
  locked: bool,
  huge: bool,
  numa_node: u16,
}

impl Region {
  /// Maps `len` bytes (rounded up to whole pages of `page` bytes). `huge` asks the OS to back
  /// the region with transparent huge pages where that exists (Linux `MADV_HUGEPAGE`).
  pub fn map(len: usize, page: usize, huge: bool) -> Result<Region, MemError> {
    let page = page.max(1);
    let len = len.max(page).next_multiple_of(page);
    let map = MmapMut::map_anon(len).map_err(|e| MemError::RegionRefused {
      len,
      code: e.raw_os_error(),
    })?;
    let huge = huge && advise_huge(&map);
    Ok(Region {
      backing: Backing::Anon(map),
      page,
      locked: false,
      huge,
      numa_node: 0,
    })
  }

  /// A region backed by a shared memory object it takes ownership of (§4.7, §4.8): the bytes live
  /// in the object, so they survive the process and a restarted daemon re-maps the same object to
  /// recover them. Used to back the store's content arena in anchor-owned RAM rather than a private
  /// mapping, so an agent's writes survive a daemon crash (BUG-11). The object's length (rounded to
  /// whole `page`s) is the region's length; `huge` is not asked of a shared object here.
  pub fn shared(object: SharedObject, page: usize) -> Region {
    let page = page.max(1);
    Region {
      backing: Backing::Shared(object),
      page,
      locked: false,
      huge: false,
      numa_node: 0,
    }
  }

  /// Maps a region with the page facts and the huge-page decision taken from the profile.
  pub fn map_for_profile(
    len: usize,
    profile: &slates_machine::MachineProfile,
  ) -> Result<Region, MemError> {
    let page = usize::try_from(profile.facts.page.base).unwrap_or(1);
    Region::map(len, page, huge_pages_beneficial(profile).get())
  }

  /// The base address.
  pub fn base(&self) -> *const u8 {
    self.backing.bytes().as_ptr()
  }

  /// Length in bytes.
  pub fn len(&self) -> usize {
    self.backing.bytes().len()
  }

  /// Whether the region is empty (never, for a mapped region).
  pub fn is_empty(&self) -> bool {
    self.backing.bytes().is_empty()
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
  /// error says so (locking is all-or-nothing per call, and this call is one region).
  pub fn lock(&mut self) -> Result<(), MemError> {
    if self.locked {
      return Ok(());
    }
    match &mut self.backing {
      Backing::Anon(map) => os::lock(map)?,
      // The shared object locks its whole mapping into RAM (the same `mlock`/`VirtualLock` D-12
      // cites); a restarted daemon re-locks it after re-mapping.
      Backing::Shared(object) => object.lock()?,
    }
    self.locked = true;
    Ok(())
  }

  /// Unlocks the region.
  pub fn unlock(&mut self) {
    if self.locked {
      match &mut self.backing {
        Backing::Anon(map) => os::unlock(map),
        // A shared object holds its lock until it is dropped; there is no partial unlock verb, and
        // a region that owns one is unlocked exactly when it (and the object) drop.
        Backing::Shared(_) => {}
      }
      self.locked = false;
    }
  }

  /// Touches one byte per page in `[from_page, to_page)` so the pages are resident. Called by
  /// the pre-fault scheduler in batches; safe to call on any page range within the region.
  pub fn touch_pages(&mut self, from_page: usize, to_page: usize) {
    let pages = self.pages();
    let to = to_page.min(pages);
    let page = self.page;
    let bytes = self.backing.bytes_mut();
    let mut p = from_page;
    while p < to {
      let at = p * page;
      bytes[at] = 0;
      std::hint::black_box(&bytes[at]);
      p += 1;
    }
  }

  /// Pages in the region.
  pub fn pages(&self) -> usize {
    self.backing.bytes().len() / self.page
  }

  /// The bytes.
  pub fn bytes(&self) -> &[u8] {
    self.backing.bytes()
  }

  /// The bytes, mutably.
  pub fn bytes_mut(&mut self) -> &mut [u8] {
    self.backing.bytes_mut()
  }
}

#[cfg(target_os = "linux")]
fn advise_huge(map: &MmapMut) -> bool {
  map.advise(memmap2::Advice::HugePage).is_ok()
}

#[cfg(not(target_os = "linux"))]
fn advise_huge(_map: &MmapMut) -> bool {
  false
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

/// What the OS reports as locked for this process, for the AC-0.5 cross-check (the query lives
/// in `slates-machine`, the crate allowed to read the kernel's pseudo-files).
pub fn os_locked_bytes() -> Option<u64> {
  slates_machine::facts::locked_bytes()
}

#[cfg(unix)]
mod os {
  use super::MemError;
  use memmap2::MmapMut;

  pub(super) fn lock(map: &mut MmapMut) -> Result<(), MemError> {
    map.lock().map_err(|e| MemError::LockRefused {
      requested: map.len(),
      locked: 0,
      code: e.raw_os_error(),
    })
  }

  pub(super) fn unlock(map: &mut MmapMut) {
    let _ = map.unlock();
  }
}

#[cfg(windows)]
mod os {
  use super::MemError;
  use memmap2::MmapMut;
  use std::ffi::c_void;
  use windows_sys::Win32::System::Memory::{VirtualLock, VirtualUnlock};
  use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessWorkingSetSize, SetProcessWorkingSetSize,
  };

  /// VirtualLock is bounded by the working set: raise the maximum by the region first.
  pub(super) fn lock(map: &mut MmapMut) -> Result<(), MemError> {
    let len = map.len();
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
    // SAFETY: the committed mapping owned by `map`.
    if unsafe { VirtualLock(map.as_mut_ptr().cast::<c_void>(), len) } != 0 {
      Ok(())
    } else {
      Err(MemError::LockRefused {
        requested: len,
        locked: 0,
        code: std::io::Error::last_os_error().raw_os_error(),
      })
    }
  }

  pub(super) fn unlock(map: &mut MmapMut) {
    // SAFETY: the mapping locked by `lock`.
    unsafe { VirtualUnlock(map.as_mut_ptr().cast::<c_void>(), map.len()) };
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn page() -> usize {
    if cfg!(miri) {
      return 4096;
    }
    usize::try_from(slates_machine::facts::Facts::query().page.base).unwrap()
  }

  #[test]
  fn a_region_maps_rounds_to_pages_touches_and_unmaps() {
    let page = page();
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
    assert!(!r.base().is_null());
  }

  #[test]
  #[cfg_attr(miri, ignore)] // measures the machine
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
  #[cfg_attr(miri, ignore)] // mlock and the OS's locked-byte report are not modelled by Miri
  fn locking_a_small_region_is_reported_by_the_os_within_one_page() {
    let _serial = crate::test_serial::Guard::take();
    let page = page();
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
