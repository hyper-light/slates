//! Adopting a Windows section held by the anchor (A-198): a refused adoption gives its handle back to the OS. This is
//! its own test binary, with one test in it, because the count it reads is the whole process's: a test running beside
//! it would open and close handles of its own.
// Test harness code: an unwrap here is a failed test.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::panic_in_result_fn,
  clippy::unwrap_in_result,
  clippy::indexing_slicing
)]
#![cfg(windows)]

use std::os::windows::io::{BorrowedHandle, OwnedHandle};

use slates_mem::{MemError, SharedObject, SparseObject, Words};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};

/// Shape: the held section's length, a mapping granule on every Windows target.
const SECTION: usize = 1 << 16;
/// Shape: a length past the section's end, which `MapViewOfFile` refuses.
const PAST_THE_SECTION: usize = SECTION * 2;
/// Shape: the refusals the audit's plan names; a leak of one handle each is unmistakable in the count.
const REFUSALS: usize = 100;

/// The handles this process holds, as the OS counts them.
fn handles() -> u32 {
  let mut count = 0u32;
  // SAFETY: the current process's pseudo-handle and one writable local.
  let answered = unsafe { GetProcessHandleCount(GetCurrentProcess(), &raw mut count) };
  assert_ne!(answered, 0, "the OS reports the handle count");
  count
}

/// An owned duplicate of the section handle `value`, as the anchor's hand-off gives a successor.
fn duplicate(value: u64) -> OwnedHandle {
  let raw = std::ptr::with_exposed_provenance_mut(usize::try_from(value).unwrap());
  // SAFETY: `value` names a section handle a live object in this test holds until after the call.
  unsafe { BorrowedHandle::borrow_raw(raw) }
    .try_clone_to_owned()
    .unwrap()
}

/// Audit plan step 5. Do: adopt a held section with a length its view refuses, 100 times, as a shared and as a sparse
/// object. Expect: every adoption refused by `MapViewOfFile`, and the process's handle count exactly where it began,
/// each time; then an adoption at the section's own length still maps it, so the refusals were the length's.
#[test]
fn a_refused_adoption_of_a_held_section_leaks_no_handle() {
  let shared = SharedObject::unnamed(SECTION, Words::new()).unwrap();
  let sparse = SparseObject::unnamed(SECTION, Words::new()).unwrap();
  let refused_by_the_view = |refusal: MemError| {
    assert!(
      matches!(
        refusal,
        MemError::OsRefused {
          call: "MapViewOfFile",
          ..
        }
      ),
      "the view is what refused: {refusal:?}"
    );
  };
  // Made once, so the only change the loops can make is a leaked handle.
  let before = handles();
  for _ in 0..REFUSALS {
    let adopted = SharedObject::from_held_handle(
      duplicate(shared.handle_value().unwrap()),
      PAST_THE_SECTION,
      Words::new(),
    );
    refused_by_the_view(adopted.unwrap_err());
    let adopted = SparseObject::from_held_handle(
      duplicate(sparse.handle_value().unwrap()),
      PAST_THE_SECTION,
      Words::new(),
    );
    refused_by_the_view(adopted.unwrap_err());
  }
  let after = handles();
  eprintln!("{before} handles before {REFUSALS} refused adoptions of each kind, {after} after");
  assert_eq!(
    after.saturating_sub(before),
    0,
    "handles leaked by {REFUSALS} refused adoptions of each kind"
  );
  let mut adopted = SharedObject::from_held_handle(
    duplicate(shared.handle_value().unwrap()),
    SECTION,
    Words::new(),
  )
  .unwrap();
  adopted.write(0, &[1, 2, 3, 4]).unwrap();
  let mut back = [0u8; 4];
  shared.read(0, &mut back).unwrap();
  assert_eq!(back, [1, 2, 3, 4], "the same section at its own length");
}
