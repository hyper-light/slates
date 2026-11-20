//! The heap's bytes, as the allocator itself counts them (§4.14; A-232's gotcha, main's ruling of 2026-11-20).
//!
//! **Why.** An operator who wants the daemon's memory by kind ran `vmmap` and `heap` against it, and both suspend the
//! whole process while they read it: past the anchor's window the anchor killed the daemon as frozen, by design (the bug
//! record of 2026-11-20, the anchor's kills of w-hg's daemon). The daemon reports its own memory instead, read in its own
//! process with nothing suspended; this module is the heap's part of that report.
//!
//! **What it reads.** The system allocator's own statistics, which take only the allocator's lock for the read: macOS's
//! `malloc_zone_statistics` over every zone [B: `malloc/malloc.h`], and glibc's `mallinfo2` on Linux [B: man 3
//! mallinfo2]. Beside them, the large path's mappings ([`crate::large`]: an allocation of 64 pages or more is a mapping of
//! its own, outside the system allocator), counted by that path itself. Elsewhere (musl, Windows) the system allocator
//! is not read, and the large path alone is reported, said so by [`HeapStats::system_read`].

/// The heap's bytes now: reserved (what the allocator holds from the OS) and in use (what live allocations hold).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeapStats {
  /// Bytes the allocators hold from the OS: the system allocator's allocated size and the large path's mappings.
  pub reserved: u64,
  /// Bytes live allocations hold: the system allocator's size in use and the large path's mappings (each a live
  /// allocation, whole pages).
  pub in_use: u64,
  /// The large path's live mappings: one per live large allocation.
  pub large_mappings: u64,
  /// Whether the system allocator was read here (macOS, glibc); where it is not, the figures are the large path's alone.
  pub system_read: bool,
}

/// The heap's bytes now (module docs).
pub fn stats() -> HeapStats {
  let large = crate::large::stats();
  let large_mappings = large.mapped.saturating_sub(large.unmapped);
  let (system, system_read) = match system() {
    Some(system) => (system, true),
    None => ((0, 0), false),
  };
  HeapStats {
    reserved: system.0.saturating_add(large.mapped_bytes),
    in_use: system.1.saturating_add(large.mapped_bytes),
    large_mappings,
    system_read,
  }
}

/// The system allocator's (allocated, in use) bytes over every zone (macOS).
#[cfg(target_os = "macos")]
fn system() -> Option<(u64, u64)> {
  // SAFETY: an all-zero `malloc_statistics_t` is a valid value of the C struct (integers only).
  let mut statistics: libc::malloc_statistics_t = unsafe { std::mem::zeroed() };
  // SAFETY: a null zone asks for every zone's statistics summed [B: `malloc/malloc.h`]; `statistics` is this frame's own
  // record for the call to write, of the type it writes.
  unsafe { libc::malloc_zone_statistics(std::ptr::null_mut(), &raw mut statistics) };
  Some((
    u64::try_from(statistics.size_allocated).unwrap_or(u64::MAX),
    u64::try_from(statistics.size_in_use).unwrap_or(u64::MAX),
  ))
}

/// The system allocator's (allocated, in use) bytes (glibc): its main and other arenas' bytes from the OS (`arena`) and
/// its own large mappings (`hblkhd`), against the bytes in use in the arenas (`uordblks`) and those mappings.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn system() -> Option<(u64, u64)> {
  // SAFETY: `mallinfo2` takes nothing and returns its record by value [B: man 3 mallinfo2].
  let info = unsafe { libc::mallinfo2() };
  let bytes = |value: usize| u64::try_from(value).unwrap_or(u64::MAX);
  Some((
    bytes(info.arena).saturating_add(bytes(info.hblkhd)),
    bytes(info.uordblks).saturating_add(bytes(info.hblkhd)),
  ))
}

/// No system allocator read here (module docs).
#[cfg(not(any(target_os = "macos", all(target_os = "linux", target_env = "gnu"))))]
fn system() -> Option<(u64, u64)> {
  None
}

#[cfg(test)]
mod tests {
  use super::*;

  /// §4.14, the heap's part of the daemon's memory report. Do: read the heap, hold a box of a megabyte live, read it
  /// again. Expect the second reading's bytes in use and reserved at least the first's plus the box, in use within
  /// reserved, where the system allocator or the large path holds the box (the large path's floor is 64 pages, so a
  /// megabyte allocation is large on every page size this runs on, and counted wherever the system allocator is not read).
  #[test]
  fn a_live_allocation_shows_in_the_heaps_bytes_in_use_and_reserved() {
    const HELD: usize = 1 << 20;
    let before = stats();
    let held = vec![1u8; HELD];
    let after = stats();
    assert!(after.in_use <= after.reserved, "{after:?}");
    // The test binary's allocator is the system's, not the large path, so the box shows only where the system allocator
    // is read; elsewhere the reading must at least hold.
    if after.system_read {
      assert!(
        after.in_use >= before.in_use.saturating_add(HELD as u64 / 2),
        "the megabyte held shows in use: {before:?} then {after:?}"
      );
    }
    drop(held);
  }
}
