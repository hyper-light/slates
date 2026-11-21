//! The machine's memory as a budget that follows it reads it (docs/research/65): what the machine
//! could give this process now without writing anything out first.
//!
//! On Linux the kernel's estimate of the memory a new allocation can take (`MemAvailable`,
//! proc(5)) counts the whole file LRU, dirty and writeback pages with the clean (mm/show_mem.c
//! `si_mem_available`), though reclaim cannot take those back before they are written
//! (mm/vmscan.c `shrink_folio_list`); the kernel's own room for a cgroup's dirty memory counts only
//! clean file pages (mm/page-writeback.c `mdtc_calc_avail`). And when the kernel moves a process's
//! pages out to swap (on a disk, or compressed in memory by zram), the memory it frees reads as
//! available: a process that sized itself by the estimate would grow by what the kernel took from
//! it. So the room is the estimate less the dirty memory the kernel holds and less this process's
//! own pages in swap (`VmSwap`, proc(5)).
//!
//! The dirty memory the kernel holds is what its flusher lets build up: dirty and writeback pages
//! accumulate to the background threshold before the flusher writes them out
//! (`dirty_background_ratio` or `dirty_background_bytes`, Documentation/admin-guide/sysctl/vm.rst;
//! mm/page-writeback.c `balance_dirty_pages`: "If we're over `background_thresh' then the
//! writeback threads are woken to perform some writeout"), the threshold `/proc/vmstat` states as
//! `nr_dirty_background_threshold` in pages. So the room holds back that threshold, or the dirty
//! and writeback pages when they run past it. Read at each moment, the dirty pages rose and fell
//! with each of the flusher's cycles, and a budget that followed them gave memory back and took it
//! again with every cycle (docs/research/65 §9). Elsewhere the room is hyper-rt's reading of the
//! OS's own estimate.

/// The memory the machine could give this process now without writing anything out first; none
/// without a reading.
pub fn room_now() -> Option<u64> {
  #[cfg(target_os = "linux")]
  {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    // A kernel that does not state its threshold holds back the dirty pages alone.
    let vmstat = std::fs::read_to_string("/proc/vmstat").unwrap_or_default();
    room_of(&meminfo, &status, &vmstat, crate::port::mmap::page_size())
  }
  #[cfg(not(target_os = "linux"))]
  {
    slates_machine::facts::Facts::memory_available_now()
  }
}

/// The room in `meminfo` (`/proc/meminfo`'s text) for the process whose `/proc/self/status` text
/// is `status`, with `/proc/vmstat`'s text `vmstat` in pages of `page` bytes: `MemAvailable` less
/// the larger of the background dirty threshold and the `Dirty` and `Writeback` pages, and less
/// the process's `VmSwap`, none past nothing. None without `MemAvailable`; a count missing or
/// unreadable is none of that kind.
#[cfg(any(target_os = "linux", test))]
fn room_of(meminfo: &str, status: &str, vmstat: &str, page: usize) -> Option<u64> {
  let available = kib(meminfo, "MemAvailable:")?;
  let dirty = kib(meminfo, "Dirty:")
    .unwrap_or(0)
    .saturating_add(kib(meminfo, "Writeback:").unwrap_or(0));
  let threshold = pages(vmstat, "nr_dirty_background_threshold", page).unwrap_or(0);
  let held_back = dirty
    .max(threshold)
    .saturating_add(kib(status, "VmSwap:").unwrap_or(0));
  Some(available.saturating_sub(held_back))
}

/// The bytes a `name kB` line of a proc file states, none when it is missing or unreadable.
#[cfg(any(target_os = "linux", test))]
fn kib(text: &str, name: &str) -> Option<u64> {
  text
    .lines()
    .find_map(|line| line.strip_prefix(name))
    .and_then(|rest| rest.trim().strip_suffix("kB"))
    .and_then(|count| count.trim().parse::<u64>().ok())
    .and_then(|count| count.checked_mul(1024))
}

/// The bytes a `name count` line of `/proc/vmstat` states in pages of `page` bytes, none when it
/// is missing or unreadable.
#[cfg(any(target_os = "linux", test))]
fn pages(text: &str, name: &str, page: usize) -> Option<u64> {
  text
    .lines()
    .find_map(|line| line.strip_prefix(name)?.strip_prefix(' '))
    .and_then(|count| count.trim().parse::<u64>().ok())
    .and_then(|count| count.checked_mul(u64::try_from(page).ok()?))
}

#[cfg(test)]
mod tests {
  use super::*;

  const MEMINFO: &str = "MemTotal:       98690668 kB\nMemFree:        74950608 kB\nMemAvailable:   80540128 kB\nCached:          6094500 kB\nDirty:            315480 kB\nWriteback:          1024 kB\nAnonPages:      15177928 kB\n";
  const STATUS: &str = "Name:\tranges\nVmRSS:\t62100876 kB\nRssAnon:\t62096492 kB\nVmSwap:\t41459496 kB\nThreads:\t40\n";
  /// The VM's thresholds (2026-10-23): 2,107,184 pages of 4 KiB in the background, past the
  /// dirty pages above; `nr_dirty_threshold` is not the one held back.
  const VMSTAT: &str = "nr_dirty 78870\nnr_writeback 256\nnr_dirty_threshold 4219520\nnr_dirty_background_threshold 2107184\n";
  const PAGE: usize = 4096;

  #[test]
  fn the_room_is_the_estimate_less_the_background_threshold_and_the_process_s_swap() {
    let room = room_of(MEMINFO, STATUS, VMSTAT, PAGE).unwrap();
    assert_eq!(
      room,
      80_540_128 * 1024 - 2_107_184 * 4096 - 41_459_496 * 1024
    );
  }

  #[test]
  fn dirty_pages_past_the_threshold_are_held_back_instead() {
    let vmstat = "nr_dirty_background_threshold 16384\n";
    let room = room_of(MEMINFO, STATUS, vmstat, PAGE).unwrap();
    assert_eq!(room, (80_540_128 - 315_480 - 1_024 - 41_459_496) * 1024);
  }

  #[test]
  fn a_count_missing_is_none_of_its_kind_and_no_estimate_is_no_reading() {
    assert_eq!(
      room_of(MEMINFO, "Name:\tranges\n", "", PAGE).unwrap(),
      (80_540_128 - 315_480 - 1_024) * 1024
    );
    // A threshold line by another name, or unreadable, is no threshold.
    assert_eq!(
      room_of(MEMINFO, "", "nr_dirty_background_thresholds 9\n", PAGE).unwrap(),
      (80_540_128 - 315_480 - 1_024) * 1024
    );
    assert_eq!(
      room_of(MEMINFO, "", "nr_dirty_background_threshold x\n", PAGE).unwrap(),
      (80_540_128 - 315_480 - 1_024) * 1024
    );
    assert_eq!(room_of("MemTotal: 1 kB\n", STATUS, VMSTAT, PAGE), None);
    assert_eq!(
      room_of("MemAvailable: 12x kB\n", STATUS, VMSTAT, PAGE),
      None
    );
  }

  #[test]
  fn a_process_swapped_past_the_estimate_has_no_room() {
    let status = "VmSwap:\t90000000 kB\n";
    assert_eq!(room_of(MEMINFO, status, VMSTAT, PAGE), Some(0));
  }
}
