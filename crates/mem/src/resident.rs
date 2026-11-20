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

#[cfg(all(test, unix))]
mod tests {
  #![allow(clippy::unwrap_used)]

  use super::*;

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
