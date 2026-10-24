//! A file's DAX object by use (§4.6, the Shards route; `docs/wip/shards/dax-protocol.md`): the memory a VMM maps one
//! file's DAX ranges from. Each test plays the VMM with a mapping of its own over a duplicate of the object's
//! descriptor, as Shards' window maps what an `SCM_RIGHTS` answer gave it, and checks what the two sides see of each
//! other, what the peer cannot do to the object, and what slates' zeroing leaves.
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
#![cfg(unix)]

use std::os::fd::{AsFd, OwnedFd};

use slates_mem::{DAX_OBJECT_BYTES, DAX_RANGE_BYTES, DaxObject, MemError};

/// Shape: the most views the tests' objects allow at once.
const VIEWS: usize = 4;

/// The peer's own mapping of one range of the object, through a duplicate of its descriptor: the VMM's view.
struct PeerRange {
  at: *mut u8,
  len: usize,
  _fd: OwnedFd,
}

impl PeerRange {
  fn map(object: &DaxObject, range: u64) -> PeerRange {
    let fd = rustix::io::fcntl_dupfd_cloexec(object.descriptor(), 0).unwrap();
    let len = usize::try_from(DAX_RANGE_BYTES).unwrap();
    // SAFETY: a fresh shared mapping of the duplicate at a range inside the object (the object never shrinks: sealed
    // on Linux, sized once on macOS), unmapped in `Drop`; the test reads and writes it only through this struct.
    let at = unsafe {
      rustix::mm::mmap(
        std::ptr::null_mut(),
        len,
        rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
        rustix::mm::MapFlags::SHARED,
        &fd,
        range * DAX_RANGE_BYTES,
      )
    }
    .unwrap();
    PeerRange {
      at: at.cast(),
      len,
      _fd: fd,
    }
  }

  /// The peer's store, as a guest's through its window: a plain volatile write.
  fn store(&self, within: usize, bytes: &[u8]) {
    assert!(within + bytes.len() <= self.len);
    for (index, byte) in bytes.iter().enumerate() {
      // SAFETY: inside the peer's live mapping (asserted above).
      unsafe { self.at.add(within + index).write_volatile(*byte) };
    }
  }

  /// The peer's load, as a guest's through its window.
  fn load(&self, within: usize, len: usize) -> Vec<u8> {
    assert!(within + len <= self.len);
    // SAFETY: inside the peer's live mapping (asserted above).
    (0..len)
      .map(|index| unsafe { self.at.add(within + index).read_volatile() })
      .collect()
  }

  /// Which of the range's pages hold memory now, by `mincore` over the peer's mapping.
  fn resident_pages(&self) -> usize {
    let page = rustix::param::page_size();
    let mut marks = vec![0u8; self.len.div_ceil(page)];
    // SAFETY: the peer's whole live mapping, with one byte of `marks` per page.
    let failed = unsafe { libc::mincore(self.at.cast(), self.len, marks.as_mut_ptr().cast()) };
    assert_eq!(failed, 0, "mincore: {}", std::io::Error::last_os_error());
    marks.iter().filter(|mark| **mark & 1 != 0).count()
  }
}

impl Drop for PeerRange {
  fn drop(&mut self) {
    // SAFETY: the mapping `map` made, unmapped once.
    let _ = unsafe { rustix::mm::munmap(self.at.cast(), self.len) };
  }
}

/// The last range of the object, the one at its very end.
fn last_range() -> u64 {
  DAX_OBJECT_BYTES / DAX_RANGE_BYTES - 1
}

/// Do: make an object, read its size through its descriptor, store a marker at its last bytes and load it back, and map
/// that last range as the peer. Expect: the size is the largest a guest's file can have (2^63 − 2 MiB, a whole number
/// of ranges), and both slates and the peer see the marker at the very end.
/// Serves Shards part 3, step 2 (a confined per-file DAX backing).
#[test]
#[cfg_attr(miri, ignore)] // a shared memory object is not modelled by Miri, as at `shared_region.rs`
fn an_object_is_the_largest_file_a_guest_can_have_and_its_last_range_maps() {
  let mut object = DaxObject::create(VIEWS).unwrap();
  let size = rustix::fs::fstat(object.descriptor()).unwrap().st_size;
  assert_eq!(u64::try_from(size).unwrap(), DAX_OBJECT_BYTES);
  assert_eq!(DAX_OBJECT_BYTES, (1u64 << 63) - (2 << 20));
  assert_eq!(DAX_OBJECT_BYTES % DAX_RANGE_BYTES, 0);
  let marker = b"the last bytes of the largest file";
  let at = DAX_OBJECT_BYTES - marker.len() as u64;
  object.store(at, marker).unwrap();
  let mut back = vec![0u8; marker.len()];
  object.load(at, &mut back).unwrap();
  assert_eq!(back, marker);
  let peer = PeerRange::map(&object, last_range());
  let within = usize::try_from(DAX_RANGE_BYTES).unwrap() - marker.len();
  assert_eq!(peer.load(within, marker.len()), marker);
}

/// Do: store through the peer's mapping at an unaligned offset across a word boundary, then load through slates; and
/// store through slates, then load through the peer. Expect: each side sees the other's bytes exactly, words and edge
/// bytes alike.
/// Serves Shards part 3, step 2 (a confined per-file DAX backing).
#[test]
#[cfg_attr(miri, ignore)] // a shared memory object is not modelled by Miri, as at `shared_region.rs`
fn the_peer_and_slates_see_each_others_stores() {
  let mut object = DaxObject::create(VIEWS).unwrap();
  let peer = PeerRange::map(&object, 3);
  let from_peer: Vec<u8> = (0u8..=200).collect();
  peer.store(5, &from_peer);
  let mut seen = vec![0u8; from_peer.len()];
  object.load(3 * DAX_RANGE_BYTES + 5, &mut seen).unwrap();
  assert_eq!(seen, from_peer, "slates sees the peer's stores");
  let from_slates: Vec<u8> = (0u8..=200).rev().collect();
  object
    .store(3 * DAX_RANGE_BYTES + 1000, &from_slates)
    .unwrap();
  assert_eq!(
    peer.load(1000, from_slates.len()),
    from_slates,
    "the peer sees slates' stores"
  );
}

/// Do: store a run that crosses from one range into the next, and load it back across the boundary. Expect: the bytes
/// are whole, and slates holds a view of each range.
/// Serves Shards part 3, step 2 (a confined per-file DAX backing).
#[test]
#[cfg_attr(miri, ignore)] // a shared memory object is not modelled by Miri, as at `shared_region.rs`
fn a_copy_crossing_ranges_is_whole() {
  let mut object = DaxObject::create(VIEWS).unwrap();
  let run: Vec<u8> = (0..4096u32).map(|n| (n % 251) as u8).collect();
  let at = DAX_RANGE_BYTES - 1000;
  object.store(at, &run).unwrap();
  let mut back = vec![0u8; run.len()];
  object.load(at, &mut back).unwrap();
  assert_eq!(back, run);
  assert_eq!(object.views(), 2);
}

/// Do: as the peer, try to resize the object through its duplicate descriptor, both smaller and larger. Expect: each is
/// refused and the size is unchanged; on Linux the object carries the grow, shrink and seal seals.
/// Serves Shards part 3, step 2 (a confined per-file DAX backing).
#[test]
#[cfg_attr(miri, ignore)] // a shared memory object is not modelled by Miri, as at `shared_region.rs`
fn the_peer_cannot_resize_the_object() {
  let object = DaxObject::create(VIEWS).unwrap();
  let fd = rustix::io::fcntl_dupfd_cloexec(object.descriptor(), 0).unwrap();
  #[allow(clippy::disallowed_methods)] // the memory object: RAM, not a host path (R1).
  let shrink = rustix::fs::ftruncate(&fd, DAX_RANGE_BYTES);
  assert!(shrink.is_err(), "a shrink by the peer is refused");
  #[allow(clippy::disallowed_methods)] // the memory object: RAM, not a host path (R1).
  let grow = rustix::fs::ftruncate(&fd, DAX_OBJECT_BYTES + DAX_RANGE_BYTES);
  assert!(grow.is_err(), "a growth by the peer is refused");
  let size = rustix::fs::fstat(fd.as_fd()).unwrap().st_size;
  assert_eq!(u64::try_from(size).unwrap(), DAX_OBJECT_BYTES);
  #[cfg(target_os = "linux")]
  {
    let seals = rustix::fs::fcntl_get_seals(&fd).unwrap();
    for seal in [
      rustix::fs::SealFlags::GROW,
      rustix::fs::SealFlags::SHRINK,
      rustix::fs::SealFlags::SEAL,
    ] {
      assert!(seals.contains(seal), "{seal:?} in {seals:?}");
    }
    assert!(!seals.contains(rustix::fs::SealFlags::WRITE));
  }
}

/// Do: store bytes over most of a range (whole pages and partial ones at both ends), zero that span, and load it; then
/// zero a range nothing ever touched. Expect: the stored bytes were there first (non-vacuous) and read zero after,
/// bytes just outside the span are kept, and zeroing the untouched range makes none of its pages resident: no memory is
/// committed to zero headroom.
/// Serves Shards part 3, step 2 (a confined per-file DAX backing).
#[test]
#[cfg_attr(miri, ignore)] // a shared memory object is not modelled by Miri, as at `shared_region.rs`
fn zeroing_leaves_zeros_and_commits_nothing_for_untouched_pages() {
  let mut object = DaxObject::create(VIEWS).unwrap();
  let page = rustix::param::page_size();
  let span = 3 * page + 100;
  let at = DAX_RANGE_BYTES + 50;
  let filled = vec![b'X'; span + 20];
  object.store(at - 10, &filled).unwrap();
  let mut before = vec![0u8; span];
  object.load(at, &mut before).unwrap();
  assert!(
    before.iter().all(|b| *b == b'X'),
    "the span held bytes first"
  );
  object.zero(at, span as u64).unwrap();
  let mut after = vec![1u8; span];
  object.load(at, &mut after).unwrap();
  assert!(after.iter().all(|b| *b == 0), "the span reads zeros");
  let mut edges = [0u8; 10];
  object.load(at - 10, &mut edges).unwrap();
  assert_eq!(edges, [b'X'; 10], "the bytes before the span are kept");
  object.load(at + span as u64, &mut edges).unwrap();
  assert_eq!(edges, [b'X'; 10], "the bytes after the span are kept");

  let peer = PeerRange::map(&object, 2);
  assert_eq!(
    peer.resident_pages(),
    0,
    "an untouched range holds no memory"
  );
  object.zero(2 * DAX_RANGE_BYTES, DAX_RANGE_BYTES).unwrap();
  assert_eq!(
    peer.resident_pages(),
    0,
    "zeroing an untouched range commits no memory"
  );
}

/// Do: zero a range the peer stored into through its own mapping. Expect: the peer's bytes were there (non-vacuous) and
/// read zero afterwards, both to slates and to the peer.
/// Serves Shards part 3, step 2 (a confined per-file DAX backing).
#[test]
#[cfg_attr(miri, ignore)] // a shared memory object is not modelled by Miri, as at `shared_region.rs`
fn zeroing_clears_what_the_peer_stored() {
  let mut object = DaxObject::create(VIEWS).unwrap();
  let peer = PeerRange::map(&object, 1);
  peer.store(0, &[7u8; 8192]);
  let mut seen = vec![0u8; 8192];
  object.load(DAX_RANGE_BYTES, &mut seen).unwrap();
  assert!(seen.iter().all(|b| *b == 7));
  object.zero(DAX_RANGE_BYTES, 8192).unwrap();
  object.load(DAX_RANGE_BYTES, &mut seen).unwrap();
  assert!(seen.iter().all(|b| *b == 0), "slates reads zeros");
  assert!(
    peer.load(0, 8192).iter().all(|b| *b == 0),
    "the peer reads zeros"
  );
}

/// Do: with an object allowing two views, touch three ranges; release one view and touch the third again. Expect: the
/// third is refused `ViewsExhausted` with nothing written, and is served once a view was released.
/// Serves Shards part 3, step 2 (a confined per-file DAX backing).
#[test]
#[cfg_attr(miri, ignore)] // a shared memory object is not modelled by Miri, as at `shared_region.rs`
fn views_past_the_owners_bound_are_refused_until_one_is_released() {
  let mut object = DaxObject::create(2).unwrap();
  object.store(0, b"a").unwrap();
  object.store(DAX_RANGE_BYTES, b"b").unwrap();
  assert!(matches!(
    object.store(2 * DAX_RANGE_BYTES, b"c"),
    Err(MemError::ViewsExhausted { most: 2 })
  ));
  assert!(object.release_view(0));
  object.store(2 * DAX_RANGE_BYTES, b"c").unwrap();
  let mut back = [0u8; 1];
  object.load(0, &mut back).unwrap_err();
  assert!(object.release_view(1));
  object.load(0, &mut back).unwrap();
  assert_eq!(back, *b"a", "a released view's bytes stay in the object");
}

/// Do: store and zero past the object's end, and at an offset whose end overflows. Expect: each is refused
/// `OutOfRange` and nothing is mapped.
/// Serves Shards part 3, step 2 (a confined per-file DAX backing).
#[test]
#[cfg_attr(miri, ignore)] // a shared memory object is not modelled by Miri, as at `shared_region.rs`
fn an_access_past_the_object_is_refused() {
  let mut object = DaxObject::create(VIEWS).unwrap();
  assert!(matches!(
    object.store(DAX_OBJECT_BYTES - 1, b"ab"),
    Err(MemError::OutOfRange { .. })
  ));
  assert!(matches!(
    object.store(u64::MAX, b"a"),
    Err(MemError::OutOfRange { .. })
  ));
  assert!(matches!(
    object.zero(DAX_OBJECT_BYTES, 1),
    Err(MemError::OutOfRange { .. })
  ));
  assert_eq!(object.views(), 0);
}
