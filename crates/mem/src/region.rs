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
use crate::shared::ExclusiveObject;

/// What backs a region's bytes: a private anonymous mapping (the default, process-local, gone at
/// exit), or a shared memory object (§4.7, `SharedObject`) that survives the process and is
/// re-attached by a restarted daemon through its handoff. The arena addresses bytes the same way
/// over either; only the survival and ownership differ, which is what backing content in the anchor
/// (§4.8 recovery) needs. A shared object hands out no reference to its bytes (AUD-29-09); this region
/// takes an [`ExclusiveObject`], whose constructor carries the promise that only this region reaches it.
#[derive(Debug)]
enum Backing {
  /// A private anonymous mapping, faulted in by the pre-fault scheduler; lost when the process
  /// exits.
  Anon(MmapMut),
  /// A shared memory object the region owns; its bytes outlive the process and a restarted daemon
  /// maps the same object, so content placed here recovers (§4.8).
  Shared(ExclusiveObject),
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
  /// The ranges whose pages may carry the OS's reusable mark (A-110, macOS), cleared by [`Region::prepare`] before
  /// any of them is handed out. Empty on every platform whose discard leaves no mark.
  reusable: crate::ranges::UnitSet,
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
      reusable: crate::ranges::UnitSet::new(if os::DISCARD_LEAVES_A_MARK { len } else { 0 }, page),
    })
  }

  /// A region backed by a shared memory object it takes ownership of (§4.7, §4.8): the bytes live
  /// in the object, so they survive the process and a restarted daemon re-maps the same object to
  /// recover them. Used to back the store's content arena in anchor-owned RAM rather than a private
  /// mapping, so an agent's writes survive a daemon crash (BUG-11). The object's length (rounded to
  /// whole `page`s) is the region's length; `huge` is not asked of a shared object here.
  /// The object is an [`ExclusiveObject`]: the region hands out references to its bytes, which only
  /// storage no other process reaches may do (AUD-29-09).
  ///
  /// Where a discard leaves the kernel's reusable mark (macOS, A-110), the whole region is recorded as possibly
  /// marked: the marks are on the object's pages and outlive the process or shard that set them, so a range of the
  /// content object another holder or an earlier daemon purged may carry them. Each block's first allocation clears
  /// its own range ([`Region::prepare`]), a few microseconds, instead of one call over the region at mapping time
  /// (2 to 37 ms over 2 GiB, measured), which would stall the write that claimed the region.
  pub fn shared(object: ExclusiveObject, page: usize) -> Region {
    let page = page.max(1);
    let len = object.bytes().len();
    Region {
      backing: Backing::Shared(object),
      page,
      locked: false,
      huge: false,
      numa_node: 0,
      reusable: if os::DISCARD_LEAVES_A_MARK {
        crate::ranges::UnitSet::whole(len, page)
      } else {
        crate::ranges::UnitSet::new(0, page)
      },
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

  /// Locks bytes `offset .. offset + len` (one block) against swapping, whatever backs the region: the unit a
  /// locked arena locks, so locked memory is the content allocated, not the address space mapped (§4.2 D-12). A
  /// range outside the region is refused `OutOfRange`; the OS's refusal (its locked-memory limit) is `LockRefused`.
  pub fn lock_range(&mut self, offset: usize, len: usize) -> Result<(), MemError> {
    let region_len = self.len();
    let bytes = offset
      .checked_add(len)
      .and_then(|end| self.backing.bytes_mut().get_mut(offset..end))
      .ok_or(MemError::OutOfRange {
        offset,
        len: region_len,
      })?;
    os::lock_slice(bytes).map_err(|code| MemError::LockRefused {
      requested: len,
      locked: 0,
      code,
    })
  }

  /// Unlocks bytes `offset .. offset + len`, a block [`Region::lock_range`] locked; a range outside the region is
  /// nothing to unlock.
  pub fn unlock_range(&mut self, offset: usize, len: usize) {
    if let Some(bytes) = offset
      .checked_add(len)
      .and_then(|end| self.backing.bytes_mut().get_mut(offset..end))
    {
      os::unlock_slice(bytes);
    }
  }

  /// Gives the whole pages of bytes `offset .. offset + len` (one released, unlocked block that holds only zeros) back
  /// to the OS and zeroes any partial page at either end, so the block's RAM is freed rather than kept resident full
  /// of zeros. On Linux the pages become a hole that reads zeros. On macOS they are marked reusable (A-110): they read
  /// their zeros until the kernel takes them, then zero pages, and the range is recorded so [`Region::prepare`] clears
  /// the mark before the block is handed out again. `false`, the bytes unchanged, where the OS takes no pages back (see
  /// `os::discard_slice`), the block holds no whole page, or the range lies outside the region.
  pub fn discard(&mut self, offset: usize, len: usize) -> bool {
    let page = self.page.max(1);
    let Some(end) = offset.checked_add(len).filter(|end| *end <= self.len()) else {
      return false;
    };
    let first = offset.next_multiple_of(page);
    let last = end.saturating_sub(end.checked_rem(page).unwrap_or(0));
    if first >= last {
      return false;
    }
    let shared = matches!(self.backing, Backing::Shared(_));
    let Some(interior) = self.backing.bytes_mut().get_mut(first..last) else {
      return false;
    };
    match os::discard_slice(interior, shared) {
      os::Discarded::Refused => return false,
      os::Discarded::Hole => {}
      os::Discarded::Marked => self.reusable.mark(first, last),
    }
    for edge in [offset..first, last..end] {
      if let Some(bytes) = self.backing.bytes_mut().get_mut(edge) {
        bytes.fill(0);
      }
    }
    true
  }

  /// Clears the reusable mark from every page of `offset .. offset + len` that may carry one (A-110), before the range is
  /// handed out: the bytes cleared, zero when none was recorded (every platform but macOS, and a range never
  /// discarded). The record is a page's bit, so whole pages are cleared. Refused, with the uncleared pages still
  /// recorded, when the OS refuses: the caller must not hand the range out. Allocates nothing (the arena's hot path).
  pub fn prepare(&mut self, offset: usize, len: usize) -> Result<usize, MemError> {
    if self.reusable.is_empty() {
      return Ok(0);
    }
    let size = self.len();
    // The range is widened to whole words of the record, within each recorded run: one call clears a word of pages
    // (1 MiB of 16 KiB pages) for about what one block's call costs. Measured on a fresh daemon's 512 MiB write through
    // the NFS mount: one call per 64 KiB block cost a fifth of the throughput (964-978 against 1,197-1,240 MB/s). A
    // neighbour's mark cleared early only counts its page in the footprint again.
    let word = self.reusable.word_bytes().max(1);
    let start = offset.saturating_sub(offset.checked_rem(word).unwrap_or(0));
    let end = offset
      .saturating_add(len)
      .checked_next_multiple_of(word)
      .map_or(size, |end| end.min(size));
    let mut cleared = 0usize;
    let mut at = start;
    while let Some((from, reach)) = self.reusable.next_run_within(at, end) {
      let (first, last) = (from.max(start), reach.min(end));
      let refused = match self.backing.bytes_mut().get_mut(first..last) {
        Some(pages) => os::reuse_slice(pages).err(),
        None => None,
      };
      if let Some(code) = refused {
        return Err(MemError::OsRefused {
          call: "madvise(MADV_FREE_REUSE)",
          code,
        });
      }
      cleared = cleared.saturating_add(self.reusable.take(first, last));
      at = last;
    }
    Ok(cleared)
  }

  /// Forgets any reusable mark recorded over `offset .. offset + len` without a call to the OS: for a range a recovery
  /// claims, which no mark can be on (A-110: marks go only on free blocks, a block leaves the free lists only through
  /// an allocation, which clears it, and a recovery image names only blocks live at its commit, since a freed block
  /// stays out of the free lists until no committed image names it, A-64).
  pub fn forget_marks(&mut self, offset: usize, len: usize) {
    self.reusable.take(offset, offset.saturating_add(len));
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
    for at in (from_page..to).map_while(|p| p.checked_mul(page)) {
      let Some(byte) = bytes.get_mut(at) else {
        break;
      };
      *byte = 0;
      std::hint::black_box(byte);
    }
  }

  /// Pages in the region.
  pub fn pages(&self) -> usize {
    self
      .backing
      .bytes()
      .len()
      .checked_div(self.page)
      .unwrap_or(0)
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
    lock_map(map).map_err(|e| MemError::LockRefused {
      requested: map.len(),
      locked: 0,
      code: e.raw_os_error(),
    })
  }

  pub(super) fn unlock(map: &mut MmapMut) {
    let _ = map.unlock();
  }

  /// Locks `map` against swapping, each page as it is first touched (Linux `mlock2(MLOCK_ONFAULT)`, 4.4+). A
  /// plain `mlock` faults in the whole range before it returns while holding the process's memory-map lock, so
  /// every other thread's `mmap`, `munmap` and `mprotect` waits for it: locking one shard's arena stalled every
  /// shard of the daemon at boot for seconds under CPU load (measured 2026-10-03, Linux container, 108 burners
  /// on 18 cores: one shard in `mlock` → `__mm_populate`, the other seventeen in state D in `mmap`/`munmap`;
  /// `docs/bugs/2026-10-03-locking-an-arena-stalled-every-shard-on-the-memory-map-lock.md`). On fault, the call
  /// holds the lock only to mark the mapping and charges `RLIMIT_MEMLOCK` for the whole range as before, so the
  /// guarantee (no locked page ever swaps) and the refusal are unchanged; an untouched page holds no content.
  /// A kernel without the flag refuses (`EINVAL`), typed by the caller.
  #[cfg(target_os = "linux")]
  pub(crate) fn lock_map(map: &mut MmapMut) -> std::io::Result<()> {
    // SAFETY: the range is exactly `map`'s own live mapping, borrowed mutably for the call; locking changes no
    // byte of it and no other mapping.
    unsafe {
      rustix::mm::mlock_with(
        map.as_mut_ptr().cast(),
        map.len(),
        rustix::mm::MlockFlags::ONFAULT,
      )
    }
    .map_err(std::io::Error::from)
  }

  /// Locks `map` against swapping. macOS has no on-fault lock: `mlock` wires the range whole.
  #[cfg(not(target_os = "linux"))]
  pub(crate) fn lock_map(map: &mut MmapMut) -> std::io::Result<()> {
    map.lock()
  }

  /// Locks bytes `offset .. offset + len` of `map` against swapping (the kernel rounds to whole pages) and, on
  /// Linux, keeps them out of core dumps (`MADV_DONTDUMP`). A range outside the map is refused `InvalidInput`.
  pub(crate) fn lock_map_range(
    map: &mut MmapMut,
    offset: usize,
    len: usize,
  ) -> std::io::Result<()> {
    let end = offset
      .checked_add(len)
      .ok_or(std::io::ErrorKind::InvalidInput)?;
    let range = map
      .get_mut(offset..end)
      .ok_or(std::io::ErrorKind::InvalidInput)?;
    // SAFETY: `range` is a live part of `map`'s own mapping, borrowed mutably for the call; locking changes no byte
    // of it and no other mapping.
    unsafe { rustix::mm::mlock(range.as_mut_ptr().cast(), range.len()) }
      .map_err(std::io::Error::from)?;
    #[cfg(target_os = "linux")]
    map.advise_range(memmap2::Advice::DontDump, offset, len)?;
    Ok(())
  }

  /// Locks `bytes`, a block of a region's own mapping, against swapping (the kernel rounds to whole pages; a
  /// block is a page multiple on a page boundary, so no other block's page is touched). The refusal is the OS code.
  pub(super) fn lock_slice(bytes: &mut [u8]) -> Result<(), Option<i32>> {
    // SAFETY: `bytes` is a live part of the region's own mapping, borrowed mutably for the call; locking changes no
    // byte of it and no other mapping.
    unsafe { rustix::mm::mlock(bytes.as_mut_ptr().cast(), bytes.len()) }
      .map_err(|e| Some(e.raw_os_error()))
  }

  /// Unlocks `bytes`, a block [`lock_slice`] locked.
  pub(super) fn unlock_slice(bytes: &mut [u8]) {
    // SAFETY: as for `lock_slice`; unlocking changes no byte.
    let _ = unsafe { rustix::mm::munlock(bytes.as_mut_ptr().cast(), bytes.len()) };
  }

  /// Gives the pages of `bytes` (whole pages of a region's own, unlocked mapping) back to the OS, leaving zeros:
  /// `MADV_REMOVE` frees a shared object's own pages (every mapping of it, the anchor's included, then reads a hole),
  /// `MADV_DONTNEED` a private mapping's. [`Discarded::Refused`] when the OS refused, the bytes then unchanged.
  #[cfg(all(target_os = "linux", not(miri)))]
  pub(super) fn discard_slice(bytes: &mut [u8], shared: bool) -> Discarded {
    let advice = if shared {
      rustix::mm::Advice::LinuxRemove
    } else {
      rustix::mm::Advice::LinuxDontNeed
    };
    // SAFETY: `bytes` is a live, page-aligned run of whole pages of the region's own mapping, borrowed mutably for the
    // call, and unlocked (the caller unlocks a locked block first); the advice replaces its bytes with zeros, which is
    // what the caller asks for, and touches no byte outside it.
    match unsafe { rustix::mm::madvise(bytes.as_mut_ptr().cast(), bytes.len(), advice) } {
      Ok(()) => Discarded::Hole,
      Err(_) => Discarded::Refused,
    }
  }

  /// Marks the pages of `bytes` (whole pages of zeros, a region's own, unlocked mapping) reusable (A-110,
  /// `MADV_FREE_REUSABLE`): the footprint drops at once and the kernel may take them, after which they read zeros.
  /// The caller records the range and clears the mark with [`reuse_slice`] before handing it out.
  #[cfg(all(target_os = "macos", not(miri)))]
  pub(super) fn discard_slice(bytes: &mut [u8], _shared: bool) -> Discarded {
    // SAFETY: `bytes` is a live, page-aligned run of whole pages of the region's own mapping, borrowed mutably for the
    // call, and unlocked; the advice changes how the kernel accounts and may reclaim those pages and nothing else, and
    // the bytes are zeros, which a reclaimed page also reads.
    let refused = unsafe {
      libc::madvise(
        bytes.as_mut_ptr().cast(),
        bytes.len(),
        libc::MADV_FREE_REUSABLE,
      )
    };
    if refused == 0 {
      Discarded::Marked
    } else {
      Discarded::Refused
    }
  }

  /// No page goes back here (Miri models no `madvise`; other Unix targets are not built): the caller keeps them.
  #[cfg(not(any(
    all(target_os = "linux", not(miri)),
    all(target_os = "macos", not(miri))
  )))]
  pub(super) fn discard_slice(_bytes: &mut [u8], _shared: bool) -> Discarded {
    Discarded::Refused
  }

  /// Clears the reusable mark from the pages of `bytes` (`MADV_FREE_REUSE`, A-110), so the kernel accounts and keeps
  /// them again; the OS's error code when it refuses.
  #[cfg(all(target_os = "macos", not(miri)))]
  pub(super) fn reuse_slice(bytes: &mut [u8]) -> Result<(), Option<i32>> {
    // SAFETY: `bytes` is a live run of whole pages of the region's own mapping, borrowed mutably for the call; the
    // advice only returns the pages to normal accounting, leaving every byte as it is.
    let refused = unsafe {
      libc::madvise(
        bytes.as_mut_ptr().cast(),
        bytes.len(),
        libc::MADV_FREE_REUSE,
      )
    };
    if refused == 0 {
      Ok(())
    } else {
      Err(std::io::Error::last_os_error().raw_os_error())
    }
  }

  /// Nothing to clear where a discard leaves no mark.
  #[cfg(not(all(target_os = "macos", not(miri))))]
  pub(super) fn reuse_slice(_bytes: &mut [u8]) -> Result<(), Option<i32>> {
    Ok(())
  }

  /// Whether a discard here leaves a mark the pages keep until cleared (macOS's reusable mark, A-110).
  pub(super) const DISCARD_LEAVES_A_MARK: bool = cfg!(all(target_os = "macos", not(miri)));

  /// What a discard did to a range's pages.
  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  pub(super) enum Discarded {
    /// The OS took no page; the bytes are unchanged.
    Refused,
    /// The pages are a hole that reads zeros and costs nothing (Linux).
    #[cfg_attr(not(all(target_os = "linux", not(miri))), allow(dead_code))]
    Hole,
    /// The pages are marked reusable and must be cleared before reuse (macOS).
    #[cfg_attr(not(all(target_os = "macos", not(miri))), allow(dead_code))]
    Marked,
  }
}

#[cfg(unix)]
pub(crate) use os::{lock_map, lock_map_range};

#[cfg(windows)]
mod os {
  use super::MemError;
  use memmap2::MmapMut;
  use std::ffi::c_void;
  use windows_sys::Win32::System::Memory::{VirtualLock, VirtualUnlock};
  use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessWorkingSetSize, SetProcessWorkingSetSize,
  };

  /// VirtualLock is bounded by the working set: raises both bounds by `len` (a refusal is reported by the lock).
  fn raise_working_set(len: usize) {
    let mut min: usize = 0;
    let mut max: usize = 0;
    // SAFETY: the current process pseudo-handle and two writable outputs.
    if unsafe { GetProcessWorkingSetSize(GetCurrentProcess(), &raw mut min, &raw mut max) } != 0 {
      // SAFETY: raising both bounds by `len`; a refusal is reported by VirtualLock after.
      unsafe {
        SetProcessWorkingSetSize(
          GetCurrentProcess(),
          min.saturating_add(len),
          max.saturating_add(len),
        )
      };
    }
  }

  /// Locks the whole region, the working set raised by it first.
  pub(super) fn lock(map: &mut MmapMut) -> Result<(), MemError> {
    let len = map.len();
    raise_working_set(len);
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

  /// Locks `bytes`, a block of a region's own committed mapping, the working set raised by it first.
  pub(super) fn lock_slice(bytes: &mut [u8]) -> Result<(), Option<i32>> {
    raise_working_set(bytes.len());
    // SAFETY: `bytes` is a live, committed part of the region's own mapping; locking changes no byte.
    if unsafe { VirtualLock(bytes.as_mut_ptr().cast::<c_void>(), bytes.len()) } != 0 {
      Ok(())
    } else {
      Err(std::io::Error::last_os_error().raw_os_error())
    }
  }

  /// Unlocks `bytes`, a block [`lock_slice`] locked.
  pub(super) fn unlock_slice(bytes: &mut [u8]) {
    // SAFETY: as for `lock_slice`; unlocking changes no byte.
    unsafe { VirtualUnlock(bytes.as_mut_ptr().cast::<c_void>(), bytes.len()) };
  }

  /// No page goes back here yet (`DiscardVirtualMemory` leaves the contents undefined, not zero, so it would still need
  /// the zeroing it saves; owed with a measurement): the caller keeps them.
  pub(super) fn discard_slice(_bytes: &mut [u8], _shared: bool) -> Discarded {
    Discarded::Refused
  }

  /// Nothing to clear where a discard leaves no mark.
  pub(super) fn reuse_slice(_bytes: &mut [u8]) -> Result<(), Option<i32>> {
    Ok(())
  }

  /// Whether a discard here leaves a mark the pages keep until cleared: no page goes back on Windows yet.
  pub(super) const DISCARD_LEAVES_A_MARK: bool = false;

  /// What a discard did to a range's pages.
  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  pub(super) enum Discarded {
    /// The OS took no page; the bytes are unchanged.
    Refused,
    /// The pages are a hole that reads zeros and costs nothing.
    #[allow(dead_code)]
    Hole,
    /// The pages are marked reusable and must be cleared before reuse.
    #[allow(dead_code)]
    Marked,
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
    let mut profile =
      slates_machine::MachineProfile::measure(options).expect("the machine profile measures");
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
