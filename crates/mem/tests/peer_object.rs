//! A same-kernel peer's object by use (§4.6): the copy of one file a daemon hands a client process to map read alone
//! (A-171), or read and write (A-186). Each test plays the client with a duplicate of the object's handed descriptor,
//! as `SCM_RIGHTS` delivers one, and checks what the client sees of slates' stores and zeroing, what slates sees of a
//! writable copy's client's stores, and what the client cannot do to the object.
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
#![cfg(unix)]

use std::os::fd::OwnedFd;

use slates_mem::{MemError, PeerAccess, PeerObject, PeerView, PeerWriteView};

/// The page size, which an object's size is a whole number of.
fn page() -> u64 {
  u64::try_from(rustix::param::page_size()).unwrap()
}

/// A duplicate of the object's handed descriptor: what a client receives.
fn handed(object: &PeerObject) -> OwnedFd {
  rustix::io::fcntl_dupfd_cloexec(object.descriptor(), 0).unwrap()
}

/// The object's whole bytes as the client loads them through its own view.
fn as_the_client_sees(object: &PeerObject) -> Vec<u8> {
  let view = PeerView::map(handed(object), object.len()).unwrap();
  let mut bytes = vec![0u8; usize::try_from(view.len()).unwrap()];
  view.load(0, &mut bytes).unwrap();
  bytes
}

/// A pattern no zeroed or untouched byte equals.
fn pattern(len: usize, seed: u8) -> Vec<u8> {
  (0..len)
    .map(|index| u8::try_from(index % 251).unwrap().wrapping_add(seed) | 1)
    .collect()
}

/// Do: ask the size of the object for files of 0, 1, a page, a page and a byte, and make one for a file five pages
/// less three bytes long, reading its size through the handed descriptor. Expect: an object is the file's page span and
/// never empty (a page for an empty file), and the made object's descriptor reports exactly that size.
/// Serves AC-A-171 (one file's copy, the client maps all of it).
#[test]
#[cfg_attr(miri, ignore)] // a shared memory object is not modelled by Miri, as at `shared_region.rs`
fn an_object_is_the_files_page_span_and_never_empty() {
  let page = page();
  assert_eq!(PeerObject::bytes_for(0).unwrap(), page);
  assert_eq!(PeerObject::bytes_for(1).unwrap(), page);
  assert_eq!(PeerObject::bytes_for(page).unwrap(), page);
  assert_eq!(PeerObject::bytes_for(page + 1).unwrap(), 2 * page);
  assert!(
    PeerObject::bytes_for(u64::MAX).is_err(),
    "a span past every address is refused"
  );
  let object = PeerObject::create("slates-peer-test", 5 * page - 3, PeerAccess::Read).unwrap();
  assert_eq!(object.len(), 5 * page);
  let size = rustix::fs::fstat(object.descriptor()).unwrap().st_size;
  assert_eq!(u64::try_from(size).unwrap(), 5 * page);
}

/// Do: store a pattern across page boundaries, read it through the client's view, then zero a span with partial pages
/// at both ends and read again through a view taken before the zeroing. Expect: the client sees every stored byte, and
/// then the zeroed span reads zero and the bytes either side of it are kept.
/// Serves AC-A-171 (the copy the client maps follows what slates writes, as memory alone).
#[test]
#[cfg_attr(miri, ignore)] // a shared memory object is not modelled by Miri, as at `shared_region.rs`
fn the_client_sees_every_store_and_every_zeroing() {
  let page = page();
  let len = 4 * page;
  let mut object = PeerObject::create("slates-peer-test", len, PeerAccess::Read).unwrap();
  let bytes = pattern(usize::try_from(len).unwrap(), 7);
  object.store(0, &bytes).unwrap();
  assert_eq!(as_the_client_sees(&object), bytes, "every stored byte");

  // A client's view taken before a change sees it: the view is live.
  let view = PeerView::map(handed(&object), object.len()).unwrap();
  let zero_from = page / 2 + 3;
  let zero_to = 3 * page + 5;
  object.zero(zero_from, zero_to - zero_from).unwrap();
  let mut seen = vec![0u8; usize::try_from(len).unwrap()];
  view.load(0, &mut seen).unwrap();
  let (from, to) = (
    usize::try_from(zero_from).unwrap(),
    usize::try_from(zero_to).unwrap(),
  );
  assert_eq!(
    &seen[..from],
    &bytes[..from],
    "the bytes before the span are kept"
  );
  assert!(
    seen[from..to].iter().all(|byte| *byte == 0),
    "the span reads zero"
  );
  assert_eq!(
    &seen[to..],
    &bytes[to..],
    "the bytes after the span are kept"
  );
}

/// Do: load, store and zero across the object's last byte, through slates' side and the client's view. Expect: each is
/// refused `OutOfRange`, and nothing past the object is reached. Serves AC-A-171 (the client maps exactly the object).
#[test]
#[cfg_attr(miri, ignore)] // a shared memory object is not modelled by Miri, as at `shared_region.rs`
fn an_access_past_the_object_is_refused() {
  let len = page();
  let mut object = PeerObject::create("slates-peer-test", len, PeerAccess::Read).unwrap();
  let view = PeerView::map(handed(&object), object.len()).unwrap();
  let mut past = [0u8; 2];
  assert!(matches!(
    object.load(len - 1, &mut past),
    Err(MemError::OutOfRange { .. })
  ));
  assert!(matches!(
    object.store(len - 1, &past),
    Err(MemError::OutOfRange { .. })
  ));
  assert!(matches!(
    object.zero(len - 1, 2),
    Err(MemError::OutOfRange { .. })
  ));
  assert!(matches!(
    view.load(len - 1, &mut past),
    Err(MemError::OutOfRange { .. })
  ));
}

/// Do: zero a span of an object nothing was stored into, and a span of one whose data is only a page in the middle.
/// Expect: both read zero after, and the untouched object still holds no data (`SEEK_DATA` finds none), so zeroing
/// commits nothing for pages that held nothing (Linux, where the object answers `SEEK_DATA`).
/// Serves AC-A-171 (a cut zeroes by stores over what holds bytes alone).
#[cfg(target_os = "linux")]
#[test]
#[cfg_attr(miri, ignore)] // a shared memory object is not modelled by Miri, as at `shared_region.rs`
fn zeroing_passes_over_what_holds_nothing() {
  let page = page();
  let mut untouched = PeerObject::create("slates-peer-test", 8 * page, PeerAccess::Read).unwrap();
  untouched.zero(page / 3, 7 * page).unwrap();
  assert!(
    rustix::fs::seek(untouched.descriptor(), rustix::fs::SeekFrom::Data(0)).is_err(),
    "zeroing an untouched object committed a page"
  );
  let mut sparse = PeerObject::create("slates-peer-test", 8 * page, PeerAccess::Read).unwrap();
  let middle = pattern(usize::try_from(page).unwrap(), 3);
  sparse.store(4 * page, &middle).unwrap();
  sparse.zero(0, 8 * page).unwrap();
  assert!(as_the_client_sees(&sparse).iter().all(|byte| *byte == 0));
}

/// Do: as the client, with the handed descriptor, try every way to change the object: a shared writable map, `write`,
/// a resize either way, a read-only map turned writable; on Linux also a reopen read-write through `/proc/self/fd` and
/// its writable map, a hole punch and a further seal. Expect: each is refused, and the bytes slates stored are what the
/// client still reads. Serves AC-A-171 (no client can write the object it was handed).
#[test]
#[cfg_attr(miri, ignore)] // a shared memory object is not modelled by Miri, as at `shared_region.rs`
fn the_client_cannot_write_resize_or_reach_past_the_object() {
  let page = page();
  let len = 2 * page;
  let mut object = PeerObject::create("slates-peer-test", len, PeerAccess::Read).unwrap();
  let bytes = pattern(usize::try_from(len).unwrap(), 11);
  object.store(0, &bytes).unwrap();
  let fd = handed(&object);
  let span = usize::try_from(len).unwrap();

  // SAFETY: a fresh mapping at an address the kernel chooses; tested and unmapped at once, never touched.
  let writable = unsafe {
    rustix::mm::mmap(
      std::ptr::null_mut(),
      span,
      rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
      rustix::mm::MapFlags::SHARED,
      &fd,
      0,
    )
  };
  if let Ok(at) = writable {
    // SAFETY: this call's own mapping, unmapped once.
    let _ = unsafe { rustix::mm::munmap(at, span) };
  }
  assert!(
    writable.is_err(),
    "a shared writable map of the handed descriptor"
  );

  assert!(
    rustix::io::pwrite(&fd, b"x", 0).is_err(),
    "a write through the handed descriptor"
  );
  #[allow(clippy::disallowed_methods)] // the memory object: RAM, not a host path (R1).
  let shrink = rustix::fs::ftruncate(&fd, page);
  assert!(shrink.is_err(), "a shrink through the handed descriptor");
  #[allow(clippy::disallowed_methods)] // the memory object: RAM, not a host path (R1).
  let grow = rustix::fs::ftruncate(&fd, 4 * page);
  assert!(grow.is_err(), "a growth through the handed descriptor");

  // A read-only shared map is allowed, and may never be turned writable.
  // SAFETY: a fresh read-only mapping at an address the kernel chooses; only its protection is changed, and it is
  // unmapped once below.
  let readable = unsafe {
    rustix::mm::mmap(
      std::ptr::null_mut(),
      span,
      rustix::mm::ProtFlags::READ,
      rustix::mm::MapFlags::SHARED,
      &fd,
      0,
    )
  }
  .unwrap();
  // SAFETY: `readable` is this test's own mapping of `span` bytes; the call changes its protection alone.
  let upgraded = unsafe { rustix::mm::mprotect(readable, span, rustix::mm::MprotectFlags::WRITE) };
  // SAFETY: this test's own mapping, unmapped once and never used again.
  let _ = unsafe { rustix::mm::munmap(readable, span) };
  assert!(
    upgraded.is_err(),
    "a read-only map of the handed descriptor turned writable"
  );

  #[cfg(target_os = "linux")]
  linux_only_attacks(&fd, span);

  assert_eq!(
    as_the_client_sees(&object),
    bytes,
    "the object is as slates left it"
  );
}

/// The attacks only Linux offers a holder of a memfd: a reopen read-write through `/proc/self/fd` and its writable
/// map, a hole punch, and a seal that would stop slates' own writes.
#[cfg(target_os = "linux")]
fn linux_only_attacks(fd: &OwnedFd, span: usize) {
  use std::os::fd::{AsFd, AsRawFd};

  let path = format!("/proc/self/fd/{}", fd.as_raw_fd());
  // The reopen itself is granted (a memfd is reopenable); what it reaches must still be read-only.
  #[allow(clippy::disallowed_methods)]
  // reopening this process's own memory object through /proc: RAM, not disk.
  let reopened = std::fs::OpenOptions::new()
    .read(true)
    .write(true)
    .open(&path);
  println!(
    "reopen read-write through /proc: {}",
    if reopened.is_ok() {
      "granted (its writable map and write are judged)"
    } else {
      "refused"
    }
  );
  if let Ok(reopened) = reopened {
    // SAFETY: a fresh mapping at an address the kernel chooses; tested and unmapped at once, never touched.
    let writable = unsafe {
      rustix::mm::mmap(
        std::ptr::null_mut(),
        span,
        rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
        rustix::mm::MapFlags::SHARED,
        reopened.as_fd(),
        0,
      )
    };
    if let Ok(at) = writable {
      // SAFETY: this call's own mapping, unmapped once.
      let _ = unsafe { rustix::mm::munmap(at, span) };
    }
    assert!(writable.is_err(), "a writable map of the read-write reopen");
    assert!(
      rustix::io::pwrite(reopened.as_fd(), b"x", 0).is_err(),
      "a write through the read-write reopen"
    );
  }
  #[allow(clippy::disallowed_methods)] // the memory object: RAM, not a host path (R1).
  let punched = rustix::fs::fallocate(
    fd,
    rustix::fs::FallocateFlags::PUNCH_HOLE | rustix::fs::FallocateFlags::KEEP_SIZE,
    0,
    u64::try_from(span).unwrap(),
  );
  assert!(
    punched.is_err(),
    "a hole punched through the handed descriptor"
  );
  assert!(
    rustix::fs::fcntl_add_seals(fd, rustix::fs::SealFlags::WRITE).is_err(),
    "a seal added by the client"
  );
  let seals = rustix::fs::fcntl_get_seals(fd).unwrap();
  for seal in [
    rustix::fs::SealFlags::FUTURE_WRITE,
    rustix::fs::SealFlags::SHRINK,
    rustix::fs::SealFlags::GROW,
    rustix::fs::SealFlags::SEAL,
  ] {
    assert!(seals.contains(seal), "{seal:?} in {seals:?}");
  }
}

/// Do: map a view of the object longer than it, and an empty one. Expect: both refused before any map, so a client is
/// never left with a view that faults past the object's end.
/// Serves AC-A-171 (the client maps exactly the object).
#[test]
#[cfg_attr(miri, ignore)] // a shared memory object is not modelled by Miri, as at `shared_region.rs`
fn a_view_longer_than_the_object_or_empty_is_refused() {
  let object = PeerObject::create("slates-peer-test", page(), PeerAccess::Read).unwrap();
  assert!(matches!(
    PeerView::map(handed(&object), object.len() + page()),
    Err(MemError::OutOfRange { .. })
  ));
  assert!(matches!(
    PeerView::map(handed(&object), 0),
    Err(MemError::OutOfRange { .. })
  ));
}

/// Do: make a writable copy and map it as its client does (a read-write view of a duplicate of the handed descriptor);
/// store a pattern across a page boundary through the client's view, and another through slates' own at a disjoint
/// offset; then store one byte past the view's end, and two bytes straddling it. Expect: each side loads the other's
/// stores at once; the stores past the end are refused `OutOfRange` and change nothing.
/// Serves AC-A-186 (a writable copy carries its client's stores to the daemon, which compares them back).
#[test]
#[cfg_attr(miri, ignore)] // a shared memory object is not modelled by Miri, as at `shared_region.rs`
fn a_writable_copy_carries_stores_both_ways() {
  let page = page();
  let len = 3 * page - 5;
  let mut object = PeerObject::create("slates-peer-test", len, PeerAccess::ReadWrite).unwrap();
  assert_eq!(object.access(), PeerAccess::ReadWrite);
  let client = PeerWriteView::map(handed(&object), object.len()).unwrap();
  let theirs = pattern(usize::try_from(page).unwrap() + 3, 5);
  client.store(page - 1, &theirs).unwrap();
  let mut seen = vec![0u8; theirs.len()];
  object.load(page - 1, &mut seen).unwrap();
  assert_eq!(seen, theirs, "slates loads the client's store");
  let ours = pattern(17, 9);
  object.store(2 * page + 7, &ours).unwrap();
  let mut back = vec![0u8; ours.len()];
  client.load(2 * page + 7, &mut back).unwrap();
  assert_eq!(back, ours, "the client loads slates' store");
  let end = object.len();
  assert!(matches!(
    client.store(end, b"x"),
    Err(MemError::OutOfRange { .. })
  ));
  assert!(matches!(
    client.store(end - 1, b"xy"),
    Err(MemError::OutOfRange { .. })
  ));
  let mut last = [0u8; 1];
  object.load(end - 1, &mut last).unwrap();
  assert_eq!(last, [0], "a refused store changed nothing");
}

/// Do: as the client of a writable copy, try what goes past changing its bytes: a resize either way, and on Linux a
/// further seal and a write past the end; and on Linux change bytes by a `write` and by a hole punch, which a holder of
/// a writable memfd can do. Expect: the resizes, the seal and the write past the end refused and the object still its
/// size; the write and the punch land as stores of those bytes and of zeros would; the copy is sealed against resizing
/// and further seals and not against writes.
/// Serves AC-A-186 (a writable copy authorizes its client to change its bytes and nothing else).
#[test]
#[cfg_attr(miri, ignore)] // a shared memory object is not modelled by Miri, as at `shared_region.rs`
fn a_writable_copys_client_changes_its_bytes_and_nothing_else() {
  let page = page();
  let len = 2 * page;
  let mut object = PeerObject::create("slates-peer-test", len, PeerAccess::ReadWrite).unwrap();
  object
    .store(0, &pattern(usize::try_from(len).unwrap(), 3))
    .unwrap();
  let fd = handed(&object);
  #[allow(clippy::disallowed_methods)] // the memory object: RAM, not a host path (R1).
  let shrink = rustix::fs::ftruncate(&fd, page);
  assert!(shrink.is_err(), "a shrink through the handed descriptor");
  #[allow(clippy::disallowed_methods)] // the memory object: RAM, not a host path (R1).
  let grow = rustix::fs::ftruncate(&fd, 4 * page);
  assert!(grow.is_err(), "a growth through the handed descriptor");
  let size = rustix::fs::fstat(&fd).unwrap().st_size;
  assert_eq!(
    u64::try_from(size).unwrap(),
    len,
    "the object keeps its size"
  );
  #[cfg(target_os = "linux")]
  linux_writable_bounds(&mut object, &fd, len);
}

/// What a holder of a writable memfd may and may not do beyond its shared map: a further seal and a write past the
/// end, which would grow it, are refused; a `write` and a hole punch change bytes as stores would.
#[cfg(target_os = "linux")]
fn linux_writable_bounds(object: &mut PeerObject, fd: &OwnedFd, len: u64) {
  linux_writable_seals(fd);
  assert!(
    rustix::io::pwrite(fd, b"x", len).is_err(),
    "a write past the end, which would grow the object"
  );
  linux_writable_bytes(object, fd, len);
}

/// A writable copy's seals: the size seals and `F_SEAL_SEAL`, no write seal, and no seal its holder can add.
#[cfg(target_os = "linux")]
fn linux_writable_seals(fd: &OwnedFd) {
  let seals = rustix::fs::fcntl_get_seals(fd).unwrap();
  for seal in [
    rustix::fs::SealFlags::SHRINK,
    rustix::fs::SealFlags::GROW,
    rustix::fs::SealFlags::SEAL,
  ] {
    assert!(seals.contains(seal), "{seal:?} in {seals:?}");
  }
  assert!(
    !seals.intersects(rustix::fs::SealFlags::WRITE | rustix::fs::SealFlags::FUTURE_WRITE),
    "no write seal on a writable copy: {seals:?}"
  );
  for seal in [
    rustix::fs::SealFlags::WRITE,
    rustix::fs::SealFlags::FUTURE_WRITE,
  ] {
    assert!(
      rustix::fs::fcntl_add_seals(fd, seal).is_err(),
      "{seal:?} added by the client"
    );
  }
}

/// A `write` and a hole punch by a writable copy's holder land as stores of those bytes and of zeros would, and leave
/// the object its size.
#[cfg(target_os = "linux")]
fn linux_writable_bytes(object: &mut PeerObject, fd: &OwnedFd, len: u64) {
  assert_eq!(rustix::io::pwrite(fd, b"written", 5).unwrap(), 7);
  let mut seen = [0u8; 7];
  object.load(5, &mut seen).unwrap();
  assert_eq!(&seen, b"written", "a write lands as a store would");
  let page = page();
  #[allow(clippy::disallowed_methods)] // the memory object: RAM, not a host path (R1).
  rustix::fs::fallocate(
    fd,
    rustix::fs::FallocateFlags::PUNCH_HOLE | rustix::fs::FallocateFlags::KEEP_SIZE,
    0,
    page,
  )
  .unwrap();
  let mut punched = vec![7u8; usize::try_from(page).unwrap()];
  object.load(0, &mut punched).unwrap();
  assert!(
    punched.iter().all(|byte| *byte == 0),
    "a hole punch lands as stores of zeros would"
  );
  let size = rustix::fs::fstat(fd).unwrap().st_size;
  assert_eq!(
    u64::try_from(size).unwrap(),
    len,
    "the object keeps its size"
  );
}

/// Do: make a read-only copy and try to map it as a writable copy's client does. Expect: refused by the OS (Linux's
/// write seal, macOS's read-only descriptor), so a read-only copy is never written through a view the client chose.
/// Serves AC-A-171 and AC-A-186 (an object keeps the access it was made with).
#[test]
#[cfg_attr(miri, ignore)] // a shared memory object is not modelled by Miri, as at `shared_region.rs`
fn a_read_only_copy_refuses_a_writable_view() {
  let object = PeerObject::create("slates-peer-test", page(), PeerAccess::Read).unwrap();
  assert_eq!(object.access(), PeerAccess::Read);
  assert!(matches!(
    PeerWriteView::map(handed(&object), object.len()),
    Err(MemError::OsRefused { .. })
  ));
}
