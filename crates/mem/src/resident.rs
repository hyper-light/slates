//! What of a mapped range is in RAM (§4.2, §4.14; A-744, main's ruling of 2026-11-20: split the anchor content
//! object's resident bytes by range before deciding where a green's bytes live).
//!
//! **Why.** After a 1,000-revision Subversion import the daemon held 581 MiB of dirty pages in the anchor's content
//! object (`vmmap`: one shared mapping, 43.8 GB of address space), and nothing said which of its ranges they were in: a
//! shard's write log, its two image slots, its delta log, its DAX range table or its chunk arena. A shard's ranges are
//! offsets of that one object, so only the process that maps it can ask the kernel, page by page.
//!
//! **What it does.** [`of_range`] asks the kernel's page table (`mincore`) about every page of a mapped range, a bounded
//! number of pages a call (one vector page's worth, so the scratch is one page however large the range), and counts
//! the pages it holds in RAM and, where the kernel says (macOS's `MINCORE_MODIFIED`, the page dirty in this or another
//! process), the pages written. Linux reports residency alone: there `modified` is `None`. Windows has no `mincore`;
//! its working-set query is owed, and the answer is a typed refusal. It reads no byte of the range and changes nothing.
//!
//! **A mapping whole, without a page walk** ([`of_mapping`]; main's ruling of 2026-11-20 on g228's finding). A walk
//! costs a kernel call per vector page of pages, so the anchor's content object, tens of GiB of address space, cost tens
//! of milliseconds of `mincore` a sweep whatever little of it was in RAM. The kernel already keeps per-mapping figures:
//! macOS answers `mach_vm_region`'s top information from each VM object's resident count, and Linux writes each
//! mapping's `Rss` and dirty pages in `/proc/self/smaps`. Measured on a Mac Studio (2026-11-20, 16 KiB pages, a 32 GiB
//! shared mapping with 2 GiB written): the top information 0.11 ms, the extended information (which adds dirtied pages
//! but walks the resident pages in the kernel) 1.9-3.7 ms, `mincore` 30.0-31.7 ms, all three agreeing to the page; at
//! 100 MiB written, 0.10 ms, 0.16 ms and 3.5 ms. So macOS reads the top information and reports no written bytes there.

use crate::error::MemError;

/// What [`of_range`] found: the range's bytes, those in RAM, and those written where the OS says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Residency {
  /// The range's length.
  pub bytes: u64,
  /// The bytes of its pages in RAM (whole pages, within the range).
  pub resident: u64,
  /// The bytes of its pages written (dirty), where the OS reports it; `None` where it does not (Linux).
  pub modified: Option<u64>,
}

impl Residency {
  /// The sum of two ranges' residencies (`modified` known only where both know it).
  pub fn plus(self, other: Residency) -> Residency {
    Residency {
      bytes: self.bytes.saturating_add(other.bytes),
      resident: self.resident.saturating_add(other.resident),
      modified: self
        .modified
        .zip(other.modified)
        .map(|(a, b)| a.saturating_add(b)),
    }
  }
}

/// The residency of the `len` bytes mapped at `base` (module docs), walked a page's worth of pages a call.
///
/// The range must be mapped by this process for the call's length: the kernel answers about the addresses only, so an
/// unmapped one is the OS's refusal, never undefined behaviour.
pub fn of_range(base: *const u8, len: usize, page: usize) -> Result<Residency, MemError> {
  let page = page.max(1);
  let address = base as usize;
  // The page holding `base`: its address less its offset into the page.
  let start = address.saturating_sub(address.checked_rem(page).unwrap_or(0));
  let end = (base as usize).saturating_add(len);
  let pages = end.saturating_sub(start).div_ceil(page);
  let mut found = Residency {
    bytes: u64::try_from(len).unwrap_or(u64::MAX),
    resident: 0,
    modified: os::MODIFIED_KNOWN.then_some(0),
  };
  // Derived: one page of vector entries a call, one entry a page, so the scratch is a page whatever the range.
  let mut vector = vec![0u8; page];
  let mut done = 0usize;
  while done < pages {
    let batch = pages.saturating_sub(done).min(vector.len());
    let at = start.saturating_add(done.saturating_mul(page));
    let span = batch.saturating_mul(page);
    let Some(entries) = vector.get_mut(..batch) else {
      break;
    };
    os::query(at, span, entries)?;
    let page_bytes = u64::try_from(page).unwrap_or(u64::MAX);
    for entry in entries.iter() {
      if os::is_resident(*entry) {
        found.resident = found.resident.saturating_add(page_bytes);
      }
      if let Some(modified) = found.modified.as_mut()
        && os::is_modified(*entry)
      {
        *modified = modified.saturating_add(page_bytes);
      }
    }
    done = done.saturating_add(batch);
  }
  // Pages straddling the range's ends count whole; never more than the range.
  found.resident = found.resident.min(found.bytes);
  found.modified = found.modified.map(|modified| modified.min(found.bytes));
  Ok(found)
}

/// The residency of the mappings covering the `len` bytes mapped at `base`, from the kernel's per-mapping figures (module
/// docs, "A mapping whole"): the cost follows the number of mappings, not the bytes. Each mapping counts whole, so the
/// range should be whole mappings, an object's own map. macOS counts each VM object there once, its resident pages
/// wherever they lie in it, and reports no written bytes; Linux sums `Rss` and the dirty pages of each mapping meeting
/// the range, from `/proc/self/smaps`. Windows has no per-mapping form; there it is [`of_range`]'s bounded walk.
pub fn of_mapping(base: *const u8, len: usize, page: usize) -> Result<Residency, MemError> {
  mapping::query(base as usize, len, page.max(1))
}

/// A mapping's resident and dirty bytes as `/proc/self/smaps` reports them, summed over every mapping that meets
/// `[start, end)`: `None` when no mapping does. Cfg-free, so its parse is tested on every host.
pub fn smaps_figures(smaps: &str, start: usize, end: usize) -> Option<(u64, u64)> {
  /// Format: `smaps` reports every size in kibibytes, so its numbers are scaled by this to bytes.
  const BYTES_PER_KIB: u64 = 1024;
  /// Format: `smaps` writes a mapping's start and end addresses in hexadecimal.
  const ADDRESS_RADIX: u32 = 16;
  /// Format: the dirty pages of a mapping, written by this process or shared ones.
  const DIRTY_FIELDS: [&str; 2] = ["Private_Dirty:", "Shared_Dirty:"];
  let kib = |field: &str| {
    field
      .trim()
      .trim_end_matches("kB")
      .trim()
      .parse::<u64>()
      .ok()
  };
  let (mut in_range, mut found, mut rss, mut dirty) = (false, false, 0u64, 0u64);
  for line in smaps.lines() {
    // A mapping's header is `start-end perms ...`; its fields follow until the next header.
    if let Some((from, rest)) = line.split_once('-')
      && let Some((to, _)) = rest.split_once(' ')
      && let (Ok(from), Ok(to)) = (
        usize::from_str_radix(from, ADDRESS_RADIX),
        usize::from_str_radix(to, ADDRESS_RADIX),
      )
    {
      in_range = from < end && to > start;
      found |= in_range;
      continue;
    }
    if !in_range {
      continue;
    }
    if let Some(value) = line.strip_prefix("Rss:").and_then(kib) {
      rss = rss.saturating_add(value);
    } else if let Some(value) = DIRTY_FIELDS
      .iter()
      .find_map(|field| line.strip_prefix(field))
      .and_then(kib)
    {
      dirty = dirty.saturating_add(value);
    }
  }
  found.then(|| {
    (
      rss.saturating_mul(BYTES_PER_KIB),
      dirty.saturating_mul(BYTES_PER_KIB),
    )
  })
}

#[cfg(target_os = "macos")]
mod mapping {
  use super::Residency;
  use crate::error::MemError;

  /// Format: `VM_REGION_TOP_INFO`, the flavor of `mach_vm_region` answered from each VM object's resident count
  /// (`<mach/vm_region.h>`).
  const VM_REGION_TOP_INFO: i32 = 12;

  /// `vm_region_top_info_data_t` (`<mach/vm_region.h>`), as the kernel writes it.
  #[repr(C)]
  #[derive(Clone, Copy, Default)]
  struct TopInfo {
    obj_id: u32,
    ref_count: u32,
    private_pages_resident: u32,
    shared_pages_resident: u32,
    share_mode: u8,
  }

  unsafe extern "C" {
    /// This task's own port, set by libSystem before `main`; what the C macro `mach_task_self()` reads
    /// (`<mach/mach_init.h>`), as `slates_ipc`'s owner name reads it.
    static mach_task_self_: libc::mach_port_t;
    /// `mach_vm_region` (`<mach/mach_vm.h>`): the region at or after `*address`, its start and size written back, and
    /// `flavor`'s information about it written into `info`, at most `*count` natural words.
    fn mach_vm_region(
      task: libc::mach_port_t,
      address: *mut u64,
      size: *mut u64,
      flavor: i32,
      info: *mut i32,
      count: *mut u32,
      object_name: *mut libc::mach_port_t,
    ) -> i32;
  }

  /// The resident bytes of the VM objects mapped in `[at, at + len)`, each object once (module docs).
  pub(super) fn query(at: usize, len: usize, page: usize) -> Result<Residency, MemError> {
    let end = u64::try_from(at.saturating_add(len)).unwrap_or(u64::MAX);
    let mut address = u64::try_from(at).unwrap_or(u64::MAX);
    let mut seen: Vec<u32> = Vec::new();
    let mut pages = 0u64;
    let words = u32::try_from(
      std::mem::size_of::<TopInfo>()
        .checked_div(std::mem::size_of::<u32>())
        .unwrap_or(0),
    )
    .unwrap_or(0);
    while address < end {
      let (mut size, mut info, mut count, mut name) = (0u64, TopInfo::default(), words, 0);
      // SAFETY: every pointer is to a live local of the type the call writes (`info` is `vm_region_top_info_data_t`
      // and `count` its length in natural words), and `mach_task_self_` is the port libSystem set before `main`, which
      // nothing changes after.
      let answered = unsafe {
        mach_vm_region(
          mach_task_self_,
          &raw mut address,
          &raw mut size,
          VM_REGION_TOP_INFO,
          (&raw mut info).cast(),
          &raw mut count,
          &raw mut name,
        )
      };
      if answered != 0 {
        return Err(MemError::OsRefused {
          call: "mach_vm_region",
          code: Some(answered),
        });
      }
      if address >= end || size == 0 {
        break;
      }
      if !seen.contains(&info.obj_id) {
        seen.push(info.obj_id);
        pages = pages
          .saturating_add(u64::from(info.private_pages_resident))
          .saturating_add(u64::from(info.shared_pages_resident));
      }
      address = address.saturating_add(size);
    }
    let bytes = u64::try_from(len).unwrap_or(u64::MAX);
    Ok(Residency {
      bytes,
      resident: pages
        .saturating_mul(u64::try_from(page).unwrap_or(u64::MAX))
        .min(bytes),
      modified: None,
    })
  }
}

#[cfg(target_os = "linux")]
mod mapping {
  use super::Residency;
  use crate::error::MemError;

  /// The `Rss` and dirty bytes of the mappings meeting `[at, at + len)`, from `/proc/self/smaps` (module docs).
  pub(super) fn query(at: usize, len: usize, page: usize) -> Result<Residency, MemError> {
    let smaps = read_smaps(page)?;
    let (rss, dirty) =
      super::smaps_figures(&smaps, at, at.saturating_add(len)).ok_or(MemError::OsRefused {
        call: "/proc/self/smaps (no mapping at the range)",
        code: None,
      })?;
    let bytes = u64::try_from(len).unwrap_or(u64::MAX);
    Ok(Residency {
      bytes,
      resident: rss.min(bytes),
      modified: Some(dirty.min(bytes)),
    })
  }

  /// `/proc/self/smaps` whole, read through rustix's open and read (the kernel's pseudo-file, never a host path: R1's
  /// structural check refuses `std::fs` here), a page of bytes a call. The file is the process's own mappings, so its
  /// length follows them.
  fn read_smaps(page: usize) -> Result<String, MemError> {
    let refused = |call: &'static str| {
      move |error: rustix::io::Errno| MemError::OsRefused {
        call,
        code: Some(error.raw_os_error()),
      }
    };
    let file = rustix::fs::open(
      "/proc/self/smaps",
      rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
      rustix::fs::Mode::empty(),
    )
    .map_err(refused("open /proc/self/smaps"))?;
    let mut text = Vec::new();
    let mut chunk = vec![0u8; page.max(1)];
    loop {
      let read = rustix::io::read(&file, &mut chunk).map_err(refused("read /proc/self/smaps"))?;
      let Some(bytes) = chunk.get(..read).filter(|bytes| !bytes.is_empty()) else {
        break;
      };
      text.extend_from_slice(bytes);
    }
    String::from_utf8(text).map_err(|_| MemError::OsRefused {
      call: "/proc/self/smaps (not UTF-8)",
      code: None,
    })
  }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod mapping {
  use super::Residency;
  use crate::error::MemError;

  /// No per-mapping form here (Windows): the bounded walk, paid only by the request that asks.
  pub(super) fn query(at: usize, len: usize, page: usize) -> Result<Residency, MemError> {
    super::of_range(std::ptr::with_exposed_provenance(at), len, page)
  }
}

#[cfg(unix)]
mod os {
  use crate::error::MemError;

  /// Whether the OS reports written pages (macOS does; Linux reports residency alone).
  pub(super) const MODIFIED_KNOWN: bool = cfg!(target_os = "macos");

  /// The kernel's answer for `span` bytes at page-aligned `at`, one entry a page into `entries`.
  pub(super) fn query(at: usize, span: usize, entries: &mut [u8]) -> Result<(), MemError> {
    // `mincore` reads only the page table for the addresses, and writes one byte a page into `entries`, which holds
    // `span / page` bytes by the caller's arithmetic; it never dereferences the range.
    // SAFETY: `entries` is a live, writable buffer of at least one byte per page of `span`, and `at` is page-aligned.
    let answered = unsafe {
      libc::mincore(
        std::ptr::with_exposed_provenance_mut::<libc::c_void>(at),
        span,
        entries.as_mut_ptr().cast(),
      )
    };
    if answered == 0 {
      Ok(())
    } else {
      Err(MemError::OsRefused {
        call: "mincore",
        code: std::io::Error::last_os_error().raw_os_error(),
      })
    }
  }

  /// Whether a page's entry says it is in RAM.
  pub(super) fn is_resident(entry: u8) -> bool {
    entry & 1 != 0
  }

  /// Whether a page's entry says it was written (macOS: by this process or another).
  #[cfg(target_os = "macos")]
  pub(super) fn is_modified(entry: u8) -> bool {
    let modified = libc::MINCORE_MODIFIED | libc::MINCORE_MODIFIED_OTHER;
    i32::from(entry) & modified != 0
  }

  /// Linux reports no written bit.
  #[cfg(not(target_os = "macos"))]
  pub(super) fn is_modified(_entry: u8) -> bool {
    false
  }
}

#[cfg(windows)]
mod os {
  use crate::error::MemError;

  /// Windows reports neither through this path (owed: `QueryWorkingSetEx`).
  pub(super) const MODIFIED_KNOWN: bool = false;

  /// Refused: Windows has no `mincore` (owed: `QueryWorkingSetEx`).
  pub(super) fn query(_at: usize, _span: usize, _entries: &mut [u8]) -> Result<(), MemError> {
    Err(MemError::OsRefused {
      call: "mincore (Windows: QueryWorkingSetEx owed)",
      code: None,
    })
  }

  /// Never asked.
  pub(super) fn is_resident(_entry: u8) -> bool {
    false
  }

  /// Never asked.
  pub(super) fn is_modified(_entry: u8) -> bool {
    false
  }
}

#[cfg(test)]
mod smaps_tests {
  #![allow(clippy::unwrap_used)]

  use super::smaps_figures;

  /// Shape: three mappings as `/proc/self/smaps` writes them, the middle one the object asked about.
  const SMAPS: &str = "\
7f0000000000-7f0000001000 r--p 00000000 00:00 0
Rss:                   4 kB
Private_Dirty:         0 kB
7f0000100000-7f0000500000 rw-s 00000000 00:01 7 /memfd:slates-content (deleted)
Size:               4096 kB
Rss:                1536 kB
Shared_Clean:        512 kB
Shared_Dirty:       1024 kB
Private_Dirty:         0 kB
7f0000600000-7f0000601000 rw-p 00000000 00:00 0
Rss:                   4 kB
Private_Dirty:         4 kB
";

  /// g228, main's ruling (a mapping's residency from `smaps`, no walk). Do: read the figures of the middle mapping, of a
  /// range meeting none, and of one spanning all three. Expect: the middle mapping's `Rss` and dirty bytes alone; none
  /// for a range no mapping meets; every mapping summed for the range spanning them.
  #[test]
  fn smaps_gives_the_resident_and_dirty_bytes_of_the_mappings_a_range_meets() {
    assert_eq!(
      smaps_figures(SMAPS, 0x7f00_0010_0000, 0x7f00_0050_0000),
      Some((1536 * 1024, 1024 * 1024))
    );
    assert_eq!(
      smaps_figures(SMAPS, 0x7f00_0070_0000, 0x7f00_0080_0000),
      None
    );
    assert_eq!(
      smaps_figures(SMAPS, 0x7f00_0000_0000, 0x7f00_0070_0000),
      Some((1544 * 1024, 1028 * 1024))
    );
  }
}

#[cfg(all(test, unix))]
mod tests {
  #![allow(clippy::unwrap_used)]

  use super::*;

  /// g228, main's ruling. Do: map a shared object of 64 pages, write 16 of them, and ask the mapping whole by
  /// [`of_mapping`] and by the page walk. Expect: the mapping's bytes are its length, at least the 16 written pages are
  /// resident by both, and the two agree on the resident bytes (the kernel's per-mapping count and its page table say
  /// the same).
  #[cfg(any(target_os = "macos", target_os = "linux"))]
  #[test]
  fn a_mapping_read_whole_agrees_with_its_page_walk() {
    let page = rustix::param::page_size();
    let name = format!("slates-mem-mapping-{}", std::process::id());
    let mut object =
      crate::shared::SparseObject::create(&name, 64 * page, crate::words::Words::new()).unwrap();
    for index in 0..16 {
      object.write(index * page, &[1]).unwrap();
    }
    let whole = object.mapped_residency().unwrap();
    let walked = object.residency(0, object.len()).unwrap();
    assert_eq!(whole.bytes, u64::try_from(64 * page).unwrap());
    assert!(
      whole.resident >= u64::try_from(16 * page).unwrap(),
      "{whole:?}"
    );
    assert_eq!(
      whole.resident, walked.resident,
      "{whole:?} against {walked:?}"
    );
  }

  /// A-744. Do: map 64 pages, write every other page of the first half, and ask the residency of the whole map and of
  /// its untouched half. Expect: the whole map's bytes are its length; at least the 16 written pages are resident (and,
  /// on macOS, modified); the untouched half has no modified page; nothing exceeds the range.
  #[test]
  fn written_pages_are_resident_and_modified_and_untouched_ones_are_not_modified() {
    let page = rustix::param::page_size();
    let mut region = crate::region::Region::map(64 * page, page, false).unwrap();
    for index in (0..32).step_by(2) {
      region.bytes_mut()[index * page] = 1;
    }
    let whole = of_range(region.base(), region.len(), page).unwrap();
    assert_eq!(whole.bytes, u64::try_from(64 * page).unwrap());
    assert!(
      whole.resident >= u64::try_from(16 * page).unwrap(),
      "{whole:?}"
    );
    assert!(whole.resident <= whole.bytes);
    let half = region.base().wrapping_add(32 * page);
    let untouched = of_range(half, 32 * page, page).unwrap();
    if cfg!(target_os = "macos") {
      assert!(
        whole.modified.unwrap() >= u64::try_from(16 * page).unwrap(),
        "{whole:?}"
      );
      assert_eq!(untouched.modified, Some(0), "{untouched:?}");
    } else {
      assert_eq!(whole.modified, None);
    }
  }
}
