//! SEEK_DATA and SEEK_HOLE over a volume's file bodies (RFC 7862 §15.11; A-35's NFSv4.2 SEEK and
//! READ_PLUS rest on it): on generated write patterns, a data search never skips a written byte, a hole
//! is never reported on a written byte, and the end of the file is always a hole. The exact hole
//! boundaries follow the content's extents (page-multiple open extents, sealed chunks), so the test
//! asserts the protocol's guarantees, not the granularity.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{store, volume};
use proptest::prelude::*;
use slates_vfs::VfsError;
use slates_vfs::volume::Seek;

/// Shape: the largest offset a generated write lands at: a few chunk windows, so writes leave gaps
/// that cross sealed-chunk and open-extent boundaries.
const SPAN: u64 = 1 << 20;

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::with_cases(64)))]

  /// A-35 (RFC 7862 §15.11): after writes of nonzero bytes at generated offsets, for every probed
  /// offset inside the file: SEEK_DATA answers a byte at or after it with no written byte skipped, and
  /// SEEK_HOLE answers a byte at or after it that is not a written byte (or the end of the file).
  #[test]
  fn a_seek_never_skips_data_nor_calls_data_a_hole(
    writes in proptest::collection::vec((0..SPAN, 1..4096usize), 1..8),
    probes in proptest::collection::vec(0..SPAN, 1..32),
  ) {
    let mut store = store();
    let mut vol = volume(&mut store, 1 << 30);
    let root = vol.root_inode(&store).unwrap();
    let file = vol.create_file_no(&mut store, root, "sparse", 0o644).unwrap();
    let mut written: Vec<(u64, u64)> = Vec::new();
    for (offset, len) in &writes {
      vol.write(&mut store, file, *offset, &vec![0xA5; *len]).unwrap();
      written.push((*offset, *offset + u64::try_from(*len).unwrap()));
    }
    let size = written.iter().map(|(_, end)| *end).max().unwrap();
    let is_written = |byte: u64| written.iter().any(|(start, end)| *start <= byte && byte < *end);
    for probe in probes.into_iter().filter(|probe| *probe < size) {
      match vol.seek(&store, file, probe, Seek::Data).unwrap() {
        Some(data) => {
          prop_assert!(data >= probe && data < size);
          prop_assert!(!(probe..data).any(is_written), "SEEK_DATA skipped a written byte");
        }
        None => prop_assert!(!(probe..size).any(is_written), "data after {} was missed", probe),
      }
      let hole = vol.seek(&store, file, probe, Seek::Hole).unwrap().expect("the end is a hole");
      prop_assert!(hole >= probe && hole <= size);
      prop_assert!(hole == size || !is_written(hole), "a written byte reported as a hole");
    }
  }
}

/// A-35: a seek at or past the end finds nothing, and a directory is refused.
#[test]
fn a_seek_past_the_end_finds_nothing_and_a_directory_is_refused() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 20);
  let root = vol.root_inode(&store).unwrap();
  let file = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, file, 0, b"abc").unwrap();
  assert_eq!(vol.seek(&store, file, 0, Seek::Data).unwrap(), Some(0));
  assert_eq!(vol.seek(&store, file, 0, Seek::Hole).unwrap(), Some(3));
  assert_eq!(vol.seek(&store, file, 3, Seek::Data).unwrap(), None);
  assert_eq!(vol.seek(&store, file, 3, Seek::Hole).unwrap(), None);
  assert_eq!(
    vol.seek(&store, root, 0, Seek::Data),
    Err(VfsError::IsDirectory)
  );
}

/// A-35, non-vacuity: two writes [`SPAN`] apart leave a hole between them that SEEK_HOLE finds before
/// the second write and SEEK_DATA skips to it, so the property above is not satisfied by calling the
/// whole file data.
#[test]
fn a_gap_between_writes_is_found_as_a_hole() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 30);
  let root = vol.root_inode(&store).unwrap();
  let file = vol.create_file_no(&mut store, root, "gap", 0o644).unwrap();
  vol.write(&mut store, file, 0, b"head").unwrap();
  vol.write(&mut store, file, SPAN, b"tail").unwrap();
  let hole = vol.seek(&store, file, 0, Seek::Hole).unwrap().unwrap();
  assert!(
    (4..SPAN).contains(&hole),
    "a hole between the writes: {hole}"
  );
  assert_eq!(
    vol.seek(&store, file, hole, Seek::Data).unwrap(),
    Some(SPAN)
  );
}
