//! A file's DAX object on Windows by use (§4.6, the Shards route; plan G-shards-windows step 5): the reserved section a
//! VMM maps one file's ranges from. The section is sized from the machine ([`slates_mem::dax_object_bytes`]: a section's
//! making costs in proportion to its size, measured on jupiter 2026-10-26, its doc has the numbers), committed a range
//! at a time, and handed as a duplicate of no more access than an ask needs, which its descriptor lets no holder widen
//! (A-175's discipline). Here a second handle stands for the VMM's: it maps what slates prepared and stored, slates
//! loads what it stored, and every widening of it is refused `ERROR_ACCESS_DENIED`.
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

use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicU8, Ordering};

use slates_mem::{DAX_RANGE_BYTES, DaxObject, dax_object_bytes};
use windows_sys::Win32::Foundation::{
  CloseHandle, DuplicateHandle, ERROR_ACCESS_DENIED, GetLastError, HANDLE,
};
use windows_sys::Win32::Storage::FileSystem::WRITE_DAC;
use windows_sys::Win32::System::Memory::{
  FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, UnmapViewOfFile,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// Shape: the views slates may hold at once in these tests.
const MOST_VIEWS: usize = 4;

/// A view of one range of a section through a handed handle, as a VMM maps it; unmapped on drop.
struct VmmRange(MEMORY_MAPPED_VIEW_ADDRESS);

impl VmmRange {
  /// Maps range `range` of `handle`'s section with `access`, or the error the system answered.
  fn map(handle: &OwnedHandle, access: u32, range: u64) -> Result<VmmRange, u32> {
    let offset = range * DAX_RANGE_BYTES;
    // SAFETY: a view at an address the system chooses of a live handle's section; refused views are null.
    let view = unsafe {
      MapViewOfFile(
        handle.as_raw_handle(),
        access,
        u32::try_from(offset >> 32).unwrap(),
        u32::try_from(offset & u64::from(u32::MAX)).unwrap(),
        usize::try_from(DAX_RANGE_BYTES).unwrap(),
      )
    };
    if view.Value.is_null() {
      // SAFETY: reads this thread's last error, which the failed call set.
      return Err(unsafe { GetLastError() });
    }
    Ok(VmmRange(view))
  }

  /// The byte at `at` within the range, as an atomic: the other side stores into it at any moment.
  fn byte(&self, at: usize) -> &AtomicU8 {
    assert!(at < usize::try_from(DAX_RANGE_BYTES).unwrap());
    // SAFETY: inside this live view, which outlives the borrow; an `AtomicU8` has no alignment need.
    unsafe { &*self.0.Value.cast::<u8>().add(at).cast::<AtomicU8>() }
  }
}

impl Drop for VmmRange {
  fn drop(&mut self) {
    // SAFETY: the view this value mapped, unmapped once.
    unsafe { UnmapViewOfFile(self.0) };
  }
}

/// Whether a duplicate of `handle` with `access` is granted, or the error the system answered; a granted one is closed.
fn duplicate(handle: &OwnedHandle, access: u32) -> Result<(), u32> {
  let mut copy: HANDLE = std::ptr::null_mut();
  // SAFETY: this process on both sides, a live handle and an out-pointer to a local; a granted copy is closed below.
  let done = unsafe {
    DuplicateHandle(
      GetCurrentProcess(),
      handle.as_raw_handle(),
      GetCurrentProcess(),
      &mut copy,
      access,
      0,
      0,
    )
  };
  if done == 0 {
    // SAFETY: reads this thread's last error, which the failed call set.
    return Err(unsafe { GetLastError() });
  }
  // SAFETY: the copy just granted, closed once.
  unsafe { CloseHandle(copy) };
  Ok(())
}

/// What T-shards-windows-5.1 checks through the VMM's view of ranges 0 and `last`, after slates stored "first" at 5
/// and "last" 7 bytes into the last range.
fn the_vmm_and_slates_see_each_others_stores(
  object: &mut DaxObject,
  handed: &OwnedHandle,
  last: u64,
) {
  let first = VmmRange::map(handed, FILE_MAP_READ | FILE_MAP_WRITE, 0).unwrap();
  let end = VmmRange::map(handed, FILE_MAP_READ | FILE_MAP_WRITE, last).unwrap();
  let read = |view: &VmmRange, at: usize, len: usize| -> Vec<u8> {
    (at..at + len)
      .map(|i| view.byte(i).load(Ordering::Relaxed))
      .collect()
  };
  assert_eq!(read(&first, 5, 5), b"first");
  assert_eq!(read(&end, 7, 4), b"last");
  // A prepared byte nothing stored reads zero rather than faulting the VMM.
  let far = usize::try_from(DAX_RANGE_BYTES).unwrap() - 1;
  assert_eq!(read(&end, far, 1), [0]);

  for (at, byte) in b"guest".iter().enumerate() {
    first.byte(100 + at).store(*byte, Ordering::Relaxed);
  }
  let mut loaded = [0u8; 5];
  object.load(100, &mut loaded).unwrap();
  assert_eq!(&loaded, b"guest");
}

/// T-shards-windows-5.1. Do: make a DAX object, prepare and store into its first and last ranges, and map those ranges
/// through a writable handed handle. Expect the object to be the machine's memory in whole ranges, every prepared byte
/// readable through the VMM's view (zero where nothing was stored), slates' stores seen there, the VMM's stores loaded
/// by slates, and a range past the object refused.
#[test]
fn a_dax_section_is_sized_from_the_machine_and_both_sides_see_each_others_stores() {
  let started = std::time::Instant::now();
  let mut object = DaxObject::create(MOST_VIEWS).unwrap();
  let made = started.elapsed();
  let bytes = dax_object_bytes();
  println!(
    "dax section: {bytes} bytes ({} ranges) made in {made:?}",
    bytes / DAX_RANGE_BYTES
  );
  assert_eq!(object.len(), bytes);
  assert_eq!(bytes % DAX_RANGE_BYTES, 0);
  assert!(bytes >= DAX_RANGE_BYTES);
  let last = bytes / DAX_RANGE_BYTES - 1;
  for range in [0, last] {
    object.prepare_range(range).unwrap();
  }
  let start_of_last = last * DAX_RANGE_BYTES;
  object.store(5, b"first").unwrap();
  object.store(start_of_last + 7, b"last").unwrap();

  let handed = object.handed(true).unwrap();
  the_vmm_and_slates_see_each_others_stores(&mut object, &handed, last);

  assert!(
    object.store(bytes, b"x").is_err(),
    "a store past the object"
  );
  assert!(
    object.prepare_range(last + 1).is_err(),
    "a range past the object"
  );
  assert!(
    VmmRange::map(&handed, FILE_MAP_READ, last + 1).is_err(),
    "a VMM's map past the section"
  );
}

/// T-shards-windows-5.2. Do: hand a read-only and a writable duplicate of a DAX object's section, and try to widen each
/// as a hostile holder would: a duplicate that may rewrite the section's descriptor, a duplicate that writes from the
/// read-only one, and a writable map of the read-only one. Expect each refused `ERROR_ACCESS_DENIED`, the descriptor's
/// own refusal, while a duplicate of no more access is granted.
#[test]
fn a_handed_dax_section_cannot_be_widened() {
  let mut object = DaxObject::create(MOST_VIEWS).unwrap();
  object.prepare_range(0).unwrap();
  let read_only = object.handed(false).unwrap();
  let writable = object.handed(true).unwrap();
  for handle in [&read_only, &writable] {
    assert_eq!(duplicate(handle, WRITE_DAC), Err(ERROR_ACCESS_DENIED));
    assert_eq!(duplicate(handle, FILE_MAP_READ), Ok(()));
  }
  assert_eq!(
    duplicate(&read_only, FILE_MAP_READ | FILE_MAP_WRITE),
    Err(ERROR_ACCESS_DENIED)
  );
  assert_eq!(
    VmmRange::map(&read_only, FILE_MAP_READ | FILE_MAP_WRITE, 0).err(),
    Some(ERROR_ACCESS_DENIED)
  );
  assert!(VmmRange::map(&writable, FILE_MAP_READ | FILE_MAP_WRITE, 0).is_ok());
}
