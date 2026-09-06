//! Inode lifetime under references (§4.6, the inode-addressed-io design): a file unlinked while a
//! transport holds it open keeps its content until the last reference drops (POSIX
//! unlink-while-open), and its content is reclaimed exactly at that last reference. Driven against
//! an in-memory scratch volume on every host.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{store, volume};
use slates_vfs::ids::InodeNo;

/// An inode unlinked while a reference is held keeps its content until the last reference drops,
/// then is reclaimed. Without the deferral this fails: `drop_link` releases the content at
/// `nlink == 0` regardless of open references (the gap Ada identified at `drop_link`).
#[test]
fn an_unlinked_referenced_inode_survives_until_the_last_reference() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 30);
  let root = vol.root_inode(&store).unwrap();
  let file = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, file, 0, b"hello").unwrap();

  // A transport holds the file open.
  vol.reference(&store, file).unwrap();
  // It is unlinked from the namespace.
  vol.unlink_no(&mut store, root, "f").unwrap();
  assert!(
    vol.lookup_no(&store, root, "f").is_err(),
    "the name is gone after unlink"
  );

  // The open object still serves its content.
  let mut buf = [0u8; 5];
  let read = vol.read(&store, file, 0, &mut buf).unwrap();
  assert_eq!(
    &buf[..read],
    b"hello",
    "an unlinked-but-open file keeps its content"
  );

  // The last close reclaims it.
  vol.unreference(&mut store, file).unwrap();
  assert!(
    vol.read(&store, file, 0, &mut buf).is_err(),
    "the content is reclaimed after the last reference"
  );
}

/// A file replaced by a rename while it is held open keeps its own content until the last
/// reference drops; the name resolves to the renamed file.
#[test]
fn rename_over_an_open_file_preserves_it() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 30);
  let root = vol.root_inode(&store).unwrap();
  let a = vol.create_file_no(&mut store, root, "a", 0o644).unwrap();
  vol.write(&mut store, a, 0, b"aaa").unwrap();
  let b = vol.create_file_no(&mut store, root, "b", 0o644).unwrap();
  vol.write(&mut store, b, 0, b"bbb").unwrap();

  // Hold b open, then rename a over b, so b's inode leaves the namespace.
  vol.reference(&store, b).unwrap();
  vol.rename_no(&mut store, root, "a", root, "b").unwrap();

  // b's replaced inode still serves its own content.
  let mut buf = [0u8; 3];
  let read = vol.read(&store, b, 0, &mut buf).unwrap();
  assert_eq!(
    &buf[..read],
    b"bbb",
    "the replaced-but-open file keeps its content"
  );
  // The name now resolves to the renamed file.
  assert_eq!(
    vol.lookup_no(&store, root, "b").unwrap().inode,
    a,
    "the name resolves to the renamed file"
  );

  // The last close reclaims the replaced inode.
  vol.unreference(&mut store, b).unwrap();
  assert!(
    vol.read(&store, b, 0, &mut buf).is_err(),
    "the replaced inode is reclaimed after the last reference"
  );
}

/// With several references, the content is reclaimed only at the last unreference, not the first.
#[test]
fn content_reclaims_only_at_the_last_of_several_references() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 30);
  let root = vol.root_inode(&store).unwrap();
  let file = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, file, 0, b"data").unwrap();

  vol.reference(&store, file).unwrap();
  vol.reference(&store, file).unwrap();
  vol.unlink_no(&mut store, root, "f").unwrap();

  let mut buf = [0u8; 4];
  vol.unreference(&mut store, file).unwrap();
  assert_eq!(
    vol.read(&store, file, 0, &mut buf).unwrap(),
    4,
    "still alive with one reference remaining"
  );
  vol.unreference(&mut store, file).unwrap();
  assert!(
    vol.read(&store, file, 0, &mut buf).is_err(),
    "reclaimed at the last reference"
  );
}

/// Writes reach an unlinked-but-open file, not just reads (POSIX unlink-while-open covers both).
#[test]
fn writes_reach_an_unlinked_open_file() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 30);
  let root = vol.root_inode(&store).unwrap();
  let file = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, file, 0, b"hello").unwrap();
  vol.reference(&store, file).unwrap();
  vol.unlink_no(&mut store, root, "f").unwrap();

  vol.write(&mut store, file, 5, b" world").unwrap();
  let mut buf = [0u8; 11];
  let read = vol.read(&store, file, 0, &mut buf).unwrap();
  assert_eq!(
    &buf[..read],
    b"hello world",
    "a write reaches an unlinked open file"
  );

  vol.unreference(&mut store, file).unwrap();
  assert!(
    vol.read(&store, file, 0, &mut buf).is_err(),
    "reclaimed after the last reference"
  );
}

/// A reference to an inode number no inode holds is refused, so the reference map cannot grow past
/// the inode table's cap (hardening: references are validated, not arbitrary).
#[test]
fn a_reference_to_an_absent_inode_is_refused() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 30);
  assert!(
    vol.reference(&store, InodeNo(999_999)).is_err(),
    "cannot reference an inode that does not exist"
  );
}
