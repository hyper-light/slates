//! A same-kernel peer's object on Windows by use (§4.6, A-175): the section a daemon copies one file into and hands a
//! client process a read-only handle of. First G-direct step 5's measurement (the plan's research f8): whether a
//! process holding a read-only handle of a pagefile-backed section can duplicate it into a handle that writes, or one
//! that rewrites the section's descriptor. Windows checks a duplicate's access against the object's security descriptor
//! when it asks for more than its source holds (`DuplicateHandle`: "In some cases, the new handle can have more access
//! rights than the original handle"), so the answer is the descriptor's, and a peer is the same user as the process
//! here. Measured 2026-10-25 on Windows 10.0.26200 (jupiter): with the default descriptor both are granted; with one
//! that grants the user reading alone and cuts the owner's implicit rights to reading the descriptor (`OWNER RIGHTS`),
//! both are refused `ERROR_ACCESS_DENIED`, and the creator's own handle still maps writable. Then
//! [`slates_mem::PeerObject`] by use: its handed handle turns into nothing more, and a client's view of it sees every
//! store and every zeroing.
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

use windows_sys::Win32::Foundation::{
  CloseHandle, DuplicateHandle, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, GetLastError, HANDLE,
  INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
  ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::Storage::FileSystem::WRITE_DAC;
use windows_sys::Win32::System::Memory::{
  CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MapViewOfFile, PAGE_PROTECTION_FLAGS,
  PAGE_READWRITE, UnmapViewOfFile, VirtualProtect,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// Shape: the section's size, one allocation granule.
const SECTION_BYTES: u32 = 64 << 10;

/// A NUL-terminated UTF-16 copy of `text`.
fn wide(text: &str) -> Vec<u16> {
  text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A new unnamed pagefile-backed section of `SECTION_BYTES`, with the descriptor `sddl` names, or the default one.
fn section(sddl: Option<&str>) -> HANDLE {
  let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
  if let Some(sddl) = sddl {
    let text = wide(sddl);
    // SAFETY: a NUL-terminated SDDL string; the descriptor is freed below.
    let parsed = unsafe {
      ConvertStringSecurityDescriptorToSecurityDescriptorW(
        text.as_ptr(),
        SDDL_REVISION_1,
        &mut descriptor,
        std::ptr::null_mut(),
      )
    };
    assert_ne!(parsed, 0, "the descriptor parses: {sddl}");
  }
  let attributes = SECURITY_ATTRIBUTES {
    nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>()).unwrap(),
    lpSecurityDescriptor: descriptor,
    bInheritHandle: 0,
  };
  // SAFETY: a pagefile-backed section with no name; the attributes outlive the call.
  let handle = unsafe {
    CreateFileMappingW(
      INVALID_HANDLE_VALUE,
      if sddl.is_some() {
        &attributes
      } else {
        std::ptr::null()
      },
      PAGE_READWRITE,
      0,
      SECTION_BYTES,
      std::ptr::null(),
    )
  };
  if !descriptor.is_null() {
    // SAFETY: the descriptor `ConvertStringSecurityDescriptorToSecurityDescriptorW` allocated.
    unsafe { LocalFree(descriptor) };
  }
  assert!(!handle.is_null(), "the section is made");
  handle
}

/// A duplicate of `handle` in this process with `access`: the handle, or the error code.
fn duplicate(handle: HANDLE, access: u32) -> Result<HANDLE, u32> {
  let mut out: HANDLE = std::ptr::null_mut();
  // SAFETY: this process's pseudo-handle on both sides, a live handle, and an out-pointer to a local.
  let done = unsafe {
    DuplicateHandle(
      GetCurrentProcess(),
      handle,
      GetCurrentProcess(),
      &mut out,
      access,
      0,
      0,
    )
  };
  if done == 0 {
    // SAFETY: reads this thread's last error.
    Err(unsafe { GetLastError() })
  } else {
    Ok(out)
  }
}

/// A view of all of `handle`'s section with `access`, unmapped at once: `Ok` or the error code.
fn maps(handle: HANDLE, access: u32) -> Result<(), u32> {
  // SAFETY: a view at an address the system chooses; unmapped below, never touched.
  let view = unsafe { MapViewOfFile(handle, access, 0, 0, 0) };
  if view.Value.is_null() {
    // SAFETY: reads this thread's last error.
    return Err(unsafe { GetLastError() });
  }
  // SAFETY: the view just made.
  unsafe { UnmapViewOfFile(view) };
  Ok(())
}

/// A read-only view of `handle`'s section, its first page turned writable with `VirtualProtect`: `Ok` or the error code.
fn turns_writable(handle: HANDLE) -> Result<(), u32> {
  // SAFETY: a read-only view at an address the system chooses; only its protection is changed, then it is unmapped.
  let view = unsafe { MapViewOfFile(handle, FILE_MAP_READ, 0, 0, 0) };
  assert!(!view.Value.is_null(), "a read-only view is granted");
  let mut old: PAGE_PROTECTION_FLAGS = 0;
  // SAFETY: the view just made, its first page, which every section here has.
  let turned = unsafe {
    VirtualProtect(
      view.Value,
      usize::try_from(page_bytes()).unwrap(),
      PAGE_READWRITE,
      &mut old,
    )
  };
  // SAFETY: reads this thread's last error, before any other call.
  let error = unsafe { GetLastError() };
  // SAFETY: the view made above.
  unsafe { UnmapViewOfFile(view) };
  if turned == 0 { Err(error) } else { Ok(()) }
}

/// Closes a handle a step of the measurement made.
fn close(handle: HANDLE) {
  // SAFETY: a handle this test made and holds alone.
  unsafe { CloseHandle(handle) };
}

/// What a read-only duplicate of a section could be turned into: each attempt's outcome, `Ok` granted or the error code.
#[derive(Debug)]
struct Escalations {
  /// A duplicate that writes.
  to_write: Result<(), u32>,
  /// A duplicate that may rewrite the section's descriptor.
  to_rewrite_its_descriptor: Result<(), u32>,
  /// A writable view of the read-only handle.
  writable_view: Result<(), u32>,
  /// A read-only view turned writable.
  turned: Result<(), u32>,
}

/// What a read-only duplicate of `section` can be turned into, printed under `label`.
fn escalations(label: &str, section: HANDLE) -> Escalations {
  let read_only = duplicate(section, FILE_MAP_READ).expect("a read-only duplicate is granted");
  let outcome = Escalations {
    to_write: duplicate(read_only, FILE_MAP_WRITE | FILE_MAP_READ).map(close),
    to_rewrite_its_descriptor: duplicate(read_only, WRITE_DAC).map(close),
    writable_view: maps(read_only, FILE_MAP_WRITE),
    turned: turns_writable(read_only),
  };
  println!("{label}: {outcome:?}");
  close(read_only);
  outcome
}

/// G-direct step 5's measurement (research f8). Do: make a section with the default descriptor and one with a peer
/// section's (`slates_mem::dax_object::PEER_SECTION_SDDL`: `OWNER RIGHTS` granted reading the descriptor alone); from a read-only
/// duplicate of each, ask a duplicate that writes, one that may rewrite the descriptor, a writable view, and a read-only
/// view turned writable; and map the confining section's own creation handle writable. Expect, as measured on Windows
/// 10.0.26200 (2026-10-25): from the default descriptor's read-only handle a duplicate that writes and one that may
/// rewrite the descriptor are granted; from the confining one's, both are refused `ERROR_ACCESS_DENIED`; the creator's
/// handle maps writable; under either, a writable view of the read-only handle is refused `ERROR_ACCESS_DENIED` and a
/// read-only view turned writable `ERROR_INVALID_PARAMETER`.
#[test]
fn a_read_only_section_handle_escalates_unless_its_descriptor_refuses() {
  let default = section(None);
  let by_default = escalations("default descriptor", default);
  let confined = section(Some(slates_mem::dax_object::PEER_SECTION_SDDL));
  println!(
    "a peer section's descriptor ({}): the creator's handle maps writable {:?}",
    slates_mem::dax_object::PEER_SECTION_SDDL,
    maps(confined, FILE_MAP_WRITE)
  );
  assert_eq!(
    maps(confined, FILE_MAP_WRITE),
    Ok(()),
    "the creator's handle keeps every access"
  );
  let confining = escalations("a peer section's descriptor", confined);
  close(default);
  close(confined);
  assert!(
    by_default.to_write.is_ok(),
    "the default descriptor lets a read-only handle's holder duplicate one that writes"
  );
  assert!(
    by_default.to_rewrite_its_descriptor.is_ok(),
    "the default descriptor lets it duplicate one that may rewrite the descriptor"
  );
  for outcome in [&by_default, &confining] {
    assert_eq!(outcome.writable_view, Err(ERROR_ACCESS_DENIED));
    assert_eq!(outcome.turned, Err(ERROR_INVALID_PARAMETER));
  }
  assert_eq!(confining.to_write, Err(ERROR_ACCESS_DENIED));
  assert_eq!(
    confining.to_rewrite_its_descriptor,
    Err(ERROR_ACCESS_DENIED)
  );
}

/// AC-A-175: a peer object's handed handle turns into nothing more. Do: make a peer object, take the read-only handle
/// it hands, and ask of it what [`escalations`] asks. Expect: a duplicate that writes, one that may rewrite the
/// descriptor and a writable view refused `ERROR_ACCESS_DENIED`, a read-only view turned writable `ERROR_INVALID_PARAMETER`. The
/// same attempts on a section with the default descriptor are granted (the measurement above), so the refusals here
/// are the peer object's descriptor's.
#[test]
fn a_peer_objects_handed_handle_turns_into_nothing_more() {
  use std::os::windows::io::AsRawHandle;

  let object = slates_mem::PeerObject::create("unused-on-windows", 5 << 10).unwrap();
  let handed = object.read_only_handle().unwrap();
  let outcome = escalations("a peer object's handed handle", handed.as_raw_handle());
  assert_eq!(outcome.to_write, Err(ERROR_ACCESS_DENIED));
  assert_eq!(outcome.to_rewrite_its_descriptor, Err(ERROR_ACCESS_DENIED));
  assert_eq!(outcome.writable_view, Err(ERROR_ACCESS_DENIED));
  assert_eq!(outcome.turned, Err(ERROR_INVALID_PARAMETER));
}

/// AC-A-175: a client's view of a peer object sees every store and every zeroing, and maps the whole object alone. Do:
/// make an object for a file of five pages less three bytes, store a pattern across page boundaries, map the handed
/// handle read alone, zero a span with partial pages at both ends; then ask for a view longer than the object and an
/// empty one. Expect: the object is the file's page span; the view sees every stored byte, then zeros exactly over the
/// zeroed span; the longer and the empty views refused `OutOfRange`.
#[test]
fn the_client_sees_every_store_and_every_zeroing() {
  let page = page_bytes();
  let mut object = slates_mem::PeerObject::create("unused-on-windows", 5 * page - 3).unwrap();
  assert_eq!(object.len(), 5 * page);
  let pattern: Vec<u8> = (0..usize::try_from(3 * page).unwrap())
    .map(|index| u8::try_from(index % 251).unwrap() | 1)
    .collect();
  object.store(page / 2, &pattern).unwrap();
  let view = slates_mem::PeerView::map(object.read_only_handle().unwrap(), object.len()).unwrap();
  let mut seen = vec![0u8; usize::try_from(object.len()).unwrap()];
  view.load(0, &mut seen).unwrap();
  let start = usize::try_from(page / 2).unwrap();
  assert!(seen[..start].iter().all(|byte| *byte == 0));
  assert_eq!(&seen[start..start + pattern.len()], &pattern[..]);
  object.zero(page - 7, page + 14).unwrap();
  view.load(0, &mut seen).unwrap();
  let (from, to) = (
    usize::try_from(page - 7).unwrap(),
    usize::try_from(2 * page + 7).unwrap(),
  );
  assert!(
    seen[from..to].iter().all(|byte| *byte == 0),
    "the zeroed span reads zero"
  );
  assert_eq!(
    &seen[start..from],
    &pattern[..from - start],
    "the bytes before it are kept"
  );
  assert_eq!(
    &seen[to..start + pattern.len()],
    &pattern[to - start..],
    "the bytes after it are kept"
  );
  views_longer_than_the_object_or_empty_are_refused(&object);
}

/// A view of `object`'s handed handle one byte longer than the object, and an empty one: each refused `OutOfRange`.
fn views_longer_than_the_object_or_empty_are_refused(object: &slates_mem::PeerObject) {
  for len in [object.len() + 1, 0] {
    assert!(
      matches!(
        slates_mem::PeerView::map(object.read_only_handle().unwrap(), len),
        Err(slates_mem::MemError::OutOfRange { .. })
      ),
      "a view of {len} bytes of a {} byte object",
      object.len()
    );
  }
}

/// The system's page size.
fn page_bytes() -> u64 {
  let mut info = windows_sys::Win32::System::SystemInformation::SYSTEM_INFO::default();
  // SAFETY: fills the struct this function owns.
  unsafe { windows_sys::Win32::System::SystemInformation::GetSystemInfo(&mut info) };
  u64::from(info.dwPageSize)
}
