//! §4.2 (D-12, BUG-1): locking a region keeps its content in RAM without faulting the whole range in. On Linux a
//! plain `mlock` populates every page before it returns, holding the process's memory-map lock throughout, and a
//! strict volume's lock of one shard's arena stalled every other shard's `mmap` and `munmap` behind it for seconds
//! (docs/bugs/2026-10-03-locking-an-arena-stalled-every-shard-on-the-memory-map-lock.md). These tests lock a
//! region far larger than anything touched and read the process's resident pages: the lock charges the whole range
//! (the OS reports it locked) while committing none of it.
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[cfg(target_os = "linux")]
mod linux {
  use slates_mem::region::{Region, os_locked_bytes};
  use slates_mem::{ExclusiveObject, MemError, SharedObject, Words};

  /// Shape: the base page the regions are mapped at.
  const PAGE: usize = 4096;
  /// Shape: a region far above this test process's resident set (a few MiB), so a populating lock would show.
  const LEN: usize = 256 << 20;
  /// Shape: the growth allowed beyond the region's own pages: the test's own allocations and page tables, a
  /// sixteenth of the region, far below the whole region a populating lock commits.
  const SLACK: usize = LEN / 16;

  /// The process's resident bytes, from `/proc/self/statm` (its second field, in pages).
  fn resident() -> usize {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap();
    let pages: usize = statm.split_whitespace().nth(1).unwrap().parse().unwrap();
    pages * PAGE
  }

  /// Locks `region` and checks the outcome: refused is a loud skip (no memlock capacity here); locked must
  /// charge the whole range and commit none of it.
  fn lock_and_check(name: &str, mut region: Region) {
    let before = resident();
    let locked_before = os_locked_bytes().unwrap_or(0);
    match region.lock() {
      Ok(()) => {}
      Err(MemError::LockRefused { .. } | MemError::OsRefused { .. }) => {
        eprintln!(
          "skipped {name}: the environment refuses mlock of {LEN} bytes (no memlock capacity)"
        );
        return;
      }
      Err(e) => panic!("an unexpected error locking the region: {e}"),
    }
    let grown = resident().saturating_sub(before);
    assert!(
      grown < SLACK,
      "{name}: locking committed {grown} bytes of an untouched {LEN}-byte region"
    );
    let locked = os_locked_bytes().unwrap_or(0).saturating_sub(locked_before);
    assert!(
      usize::try_from(locked).unwrap() >= LEN,
      "{name}: the OS reports {locked} bytes locked, below the {LEN}-byte region"
    );
    // A touched page is resident and locked from then on.
    region.bytes_mut()[LEN / 2] = 7;
    assert_eq!(region.bytes()[LEN / 2], 7);
  }

  /// §4.2, D-12. Do: lock a 256 MiB private region nothing has touched. Expect: the OS charges it locked and
  /// the process's resident set does not grow by it.
  #[test]
  fn locking_a_private_region_charges_it_without_faulting_it_in() {
    lock_and_check(
      "locking_a_private_region_charges_it_without_faulting_it_in",
      Region::map(LEN, PAGE, false).unwrap(),
    );
  }

  /// §4.2, D-12, A-64. Do: lock a 256 MiB region over a shared memory object (the shape of a shard's content
  /// arena). Expect: the OS charges it locked and the process's resident set does not grow by it.
  #[test]
  fn locking_a_shared_object_region_charges_it_without_faulting_it_in() {
    let name = format!("slates-lockfault-{}", std::process::id());
    let object = SharedObject::create(&name, LEN, Words::new()).unwrap();
    // SAFETY: this test is the object's only mapper, and the object declares no words.
    let region = Region::shared(unsafe { ExclusiveObject::new(object) }, PAGE);
    lock_and_check(
      "locking_a_shared_object_region_charges_it_without_faulting_it_in",
      region,
    );
  }
}

/// No on-fault lock outside Linux: macOS `mlock` wires the range whole and Windows `VirtualLock` commits it.
#[cfg(not(target_os = "linux"))]
#[test]
fn locking_without_faulting_in_is_linux_only() {
  eprintln!("skipped lock_on_fault: only Linux locks a range page by page as it is touched");
}
