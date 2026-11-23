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
  /// The system allocator's part of [`HeapStats::reserved`]: its allocated size, what it holds from the OS.
  pub system_reserved: u64,
  /// The system allocator's part of [`HeapStats::in_use`]: its size in use, what its live blocks hold.
  pub system_in_use: u64,
  /// The large path's part of both: its live mappings' spans, whole pages. Address space, not RAM: a mapping's pages are
  /// in RAM only once written (`crate::large::heap`'s `mapped_resident` reads them).
  pub mapped_bytes: u64,
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
    system_reserved: system.0,
    system_in_use: system.1,
    mapped_bytes: large.mapped_bytes,
    system_read,
  }
}

/// The system allocator's (allocated, in use) bytes over every zone (macOS).
#[cfg(all(target_os = "macos", not(miri)))]
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

/// The system allocator's (allocated, in use) bytes (glibc): its arenas' bytes from the OS (`arena`) and its own large
/// mappings (`hblkhd`), against the bytes in use in the arenas (`uordblks`) and those mappings.
#[cfg(all(target_os = "linux", target_env = "gnu", not(miri)))]
fn system() -> Option<(u64, u64)> {
  let malloc = malloc();
  Some((
    malloc.arena.saturating_add(malloc.mapped),
    malloc.in_use.saturating_add(malloc.mapped),
  ))
}

/// glibc's own figures for the system allocator, the memory status's `process.malloc_*` gauges (main 01a20182's ruling
/// of 2026-11-23): what it holds from the OS, what of that is free, and what it mapped for blocks of their own.
///
/// **What they cover**, read in glibc 2.39's `malloc/malloc.c` (the version the CI runners carry), not from its man
/// page: `__libc_mallinfo2` walks every arena, from `main_arena` round its `next` ring, taking each arena's lock in turn,
/// and `int_mallinfo` adds that arena's figures into one record. So the sums are over every arena (the main one and each
/// thread's), each arena read consistently but the arenas one after another, not at one instant.
/// - `arena`: each arena's `system_mem` summed, the bytes it holds from the OS (`brk` for the main arena, its own heaps
///   for the others), its mapped blocks apart.
/// - `free` (`fordblks`): each arena's free bytes summed: its top chunk, its fastbin chunks and the chunks in its bins.
///   The fastbins' bytes (`fsmblks`) are already in it (`avail += fastavail` before `fordblks += avail`), so they are not
///   added again. These are freed bytes the allocator keeps: it gives them back only by trimming the top past
///   `M_TRIM_THRESHOLD` or by `malloc_trim`.
/// - `mapped` (`hblkhd`): `mp_.mmapped_mem`, process-wide (set once, at the main arena): the bytes of blocks glibc
///   mapped each on its own, past its mmap threshold (128 KiB, rising with the blocks freed up to 32 MiB on 64-bit), and
///   unmapped when freed. In the `slates` binary a block of [`crate::large::floor`] or more never reaches glibc, so this
///   is the blocks between glibc's threshold and that floor.
/// - `in_use` (`uordblks`): each arena's `system_mem` less its free bytes, summed: the arenas' bytes live blocks hold.
#[cfg(all(target_os = "linux", target_env = "gnu", not(miri)))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Malloc {
  /// The arenas' bytes from the OS (`arena`).
  pub arena: u64,
  /// The arenas' free bytes, fastbins with them (`fordblks`).
  pub free: u64,
  /// The bytes glibc mapped for blocks of their own (`hblkhd`).
  pub mapped: u64,
  /// The arenas' bytes in live blocks (`uordblks`).
  pub in_use: u64,
}

/// glibc's figures now ([`Malloc`]); a read takes each arena's lock in turn, so only a memory status asks.
#[cfg(all(target_os = "linux", target_env = "gnu", not(miri)))]
pub fn malloc() -> Malloc {
  // SAFETY: `mallinfo2` takes nothing and returns its record by value [B: man 3 mallinfo2].
  let info = unsafe { libc::mallinfo2() };
  let bytes = |value: usize| u64::try_from(value).unwrap_or(u64::MAX);
  Malloc {
    arena: bytes(info.arena),
    free: bytes(info.fordblks),
    mapped: bytes(info.hblkhd),
    in_use: bytes(info.uordblks),
  }
}

/// No system allocator read here (module docs): none is, or Miri runs the program, which models its own allocations and
/// calls no foreign allocator's statistics (`mallinfo2`, `malloc_zone_statistics`: "unsupported operation", the nightly's
/// Miri lane, run 45017186325).
#[cfg(any(
  miri,
  not(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))
))]
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

  /// The memory status's `process.malloc_*` gauges (main 01a20182, 2026-11-23). Do: read glibc's figures, hold a block
  /// past glibc's largest mmap threshold, read them, free it, read them again. Expect the arenas' bytes and free bytes
  /// present (the main arena's top at least), the mapped bytes up by the block while it is held and down by it once freed.
  #[cfg(all(target_os = "linux", target_env = "gnu", not(miri)))]
  #[test]
  fn glibcs_mapped_bytes_follow_a_block_it_maps_until_it_is_freed() {
    /// Derived: twice glibc's largest mmap threshold on 64-bit (`DEFAULT_MMAP_THRESHOLD_MAX`, 4 MiB times
    /// `sizeof (long)`, 32 MiB), so glibc maps the block on its own however far the threshold has risen.
    const HELD: usize = 64 << 20;
    let before = malloc();
    let mut held = Vec::<u8>::with_capacity(HELD);
    std::hint::black_box(&mut held);
    let during = malloc();
    drop(held);
    let after = malloc();
    assert!(before.arena > 0 && before.free > 0, "{before:?}");
    assert!(
      during.mapped >= before.mapped.saturating_add(HELD as u64 / 2),
      "the block held shows in the mapped bytes: {before:?} then {during:?}"
    );
    assert!(
      after.mapped.saturating_add(HELD as u64 / 2) <= during.mapped,
      "the block freed leaves the mapped bytes: {during:?} then {after:?}"
    );
  }
}
