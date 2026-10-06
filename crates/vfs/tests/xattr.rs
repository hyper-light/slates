//! Extended attributes (§4.5 "Extended attributes", A-32): the volume verbs driven by use. A value set
//! reads back; the create/replace flags refuse as `setxattr(2)` does; a snapshot keeps the old value
//! while the head moves on; a refused set changes nothing; removing an owner reclaims its attributes;
//! an attribute's value is never reachable as a namespace object by its inode number; and a recovery
//! image rebuilds every attribute. The model oracle (`tests/model.rs`) covers generated histories and
//! the charges; the host differential compares with the host's own `user.*` attributes.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::unwrap_in_result)]

mod common;

use common::{store, volume};
use slates_vfs::VfsError;
use slates_vfs::clock::StepClock;
use slates_vfs::ids::InodeNo;
use slates_vfs::recover::VolumeImage;
use slates_vfs::volume::{Store, Volume};
use slates_vfs::xattr::XattrSet;

/// A volume with one file `f`, its inode number returned.
fn with_file(quota: u64) -> (Store, Volume, InodeNo) {
  let mut store = store();
  let mut vol = volume(&mut store, quota);
  let root = vol.root_inode(&store).unwrap();
  let file = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  (store, vol, file)
}

/// The whole value of attribute `name` on `no`.
fn value(vol: &Volume, store: &Store, no: InodeNo, name: &[u8]) -> Result<Vec<u8>, VfsError> {
  let len = vol.xattr_len(store, no, name)?;
  let mut buf = vec![0u8; usize::try_from(len).unwrap()];
  let n = vol.xattr_read(store, no, name, 0, &mut buf)?;
  buf.truncate(n);
  Ok(buf)
}

/// The names set on `no`.
fn names(vol: &Volume, store: &Store, no: InodeNo) -> Vec<Vec<u8>> {
  vol
    .xattr_names(store, no)
    .unwrap()
    .into_iter()
    .map(|name| name.to_vec())
    .collect()
}

/// A-32: do set two attributes, replace one, remove the other; expect each value to read back as
/// set, the names listed in byte order, and a removed name to be `NoAttribute`.
#[test]
fn a_set_value_reads_back_and_a_removed_one_is_gone() {
  let (mut store, mut vol, file) = with_file(1 << 20);
  vol
    .xattr_set(&mut store, file, b"user.b", b"second", XattrSet::Either)
    .unwrap();
  vol
    .xattr_set(&mut store, file, b"user.a", b"first", XattrSet::Either)
    .unwrap();
  assert_eq!(
    names(&vol, &store, file),
    vec![b"user.a".to_vec(), b"user.b".to_vec()]
  );
  vol
    .xattr_set(&mut store, file, b"user.a", b"replaced", XattrSet::Either)
    .unwrap();
  assert_eq!(value(&vol, &store, file, b"user.a").unwrap(), b"replaced");
  vol.xattr_remove(&mut store, file, b"user.b").unwrap();
  assert_eq!(
    value(&vol, &store, file, b"user.b"),
    Err(VfsError::NoAttribute)
  );
  assert_eq!(
    vol.xattr_remove(&mut store, file, b"user.b"),
    Err(VfsError::NoAttribute)
  );
  assert_eq!(names(&vol, &store, file), vec![b"user.a".to_vec()]);
}

/// A-32, `setxattr(2)`: do a create over an existing name and a replace of a missing one; expect
/// `AlreadyExists` and `NoAttribute`, and the existing value untouched. An empty name and one with a
/// NUL are refused before anything changes.
#[test]
fn the_create_and_replace_flags_refuse_as_setxattr_does() {
  let (mut store, mut vol, file) = with_file(1 << 20);
  vol
    .xattr_set(&mut store, file, b"user.a", b"kept", XattrSet::Create)
    .unwrap();
  assert_eq!(
    vol.xattr_set(&mut store, file, b"user.a", b"other", XattrSet::Create),
    Err(VfsError::AlreadyExists)
  );
  assert_eq!(
    vol.xattr_set(&mut store, file, b"user.z", b"other", XattrSet::Replace),
    Err(VfsError::NoAttribute)
  );
  assert_eq!(
    vol.xattr_set(&mut store, file, b"", b"v", XattrSet::Either),
    Err(VfsError::Invalid)
  );
  assert_eq!(
    vol.xattr_set(&mut store, file, b"user.\0x", b"v", XattrSet::Either),
    Err(VfsError::InvalidName)
  );
  assert_eq!(value(&vol, &store, file, b"user.a").unwrap(), b"kept");
  assert_eq!(names(&vol, &store, file), vec![b"user.a".to_vec()]);
}

/// A-32, AC-1.2: do set a value, snapshot, replace it; expect the snapshot to read the old value and
/// the head the new one.
#[test]
fn a_snapshot_keeps_the_old_value_while_the_head_moves_on() {
  let (mut store, mut vol, file) = with_file(1 << 20);
  vol
    .xattr_set(
      &mut store,
      file,
      b"user.a",
      b"version one",
      XattrSet::Either,
    )
    .unwrap();
  let snapshot = vol.snapshot(&mut store).unwrap();
  vol
    .xattr_set(
      &mut store,
      file,
      b"user.a",
      b"VERSION TWO",
      XattrSet::Either,
    )
    .unwrap();
  vol
    .xattr_set(&mut store, file, b"user.b", b"new", XattrSet::Either)
    .unwrap();
  let mut buf = [0u8; 64];
  let n = vol
    .xattr_read_in(&store, snapshot, file, b"user.a", 0, &mut buf)
    .unwrap();
  assert_eq!(&buf[..n], b"version one");
  assert_eq!(
    vol.xattr_names_in(&store, snapshot, file).unwrap(),
    vec![b"user.a".to_vec().into_boxed_slice()]
  );
  assert_eq!(
    value(&vol, &store, file, b"user.a").unwrap(),
    b"VERSION TWO"
  );
}

/// A-32, §4.2: do set a value larger than the quota leaves room for, over an existing value; expect
/// `NoSpace`, the old value intact, and the inode count and referenced bytes as they were.
#[test]
fn a_refused_set_leaves_the_old_value_and_the_charges_unchanged() {
  /// Shape: a quota that holds a small value but not a 64 KiB one.
  const QUOTA: u64 = 16 * 1024;
  let (mut store, mut vol, file) = with_file(QUOTA);
  vol
    .xattr_set(&mut store, file, b"user.a", b"small", XattrSet::Either)
    .unwrap();
  let inodes = vol.inode_usage();
  let referenced = vol.accounting().referenced_bytes;
  let big = vec![7u8; 64 * 1024];
  assert_eq!(
    vol.xattr_set(&mut store, file, b"user.a", &big, XattrSet::Either),
    Err(VfsError::NoSpace)
  );
  assert_eq!(value(&vol, &store, file, b"user.a").unwrap(), b"small");
  assert_eq!(
    vol.inode_usage(),
    inodes,
    "the fresh attribute inode was reclaimed"
  );
  assert_eq!(vol.accounting().referenced_bytes, referenced);
}

/// A-32, §4.2: do give a file two attributes, then unlink it; expect the live inode count to fall by
/// three (the file and both attribute inodes) and the referenced bytes to return to zero.
#[test]
fn removing_an_owner_reclaims_its_attributes() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 20);
  let root = vol.root_inode(&store).unwrap();
  let before = vol.inode_usage().0;
  let file = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol
    .xattr_set(&mut store, file, b"user.a", &[1u8; 300], XattrSet::Either)
    .unwrap();
  vol
    .xattr_set(&mut store, file, b"user.b", b"v", XattrSet::Either)
    .unwrap();
  assert_eq!(vol.inode_usage().0, before + 3);
  vol.unlink_no(&mut store, root, "f").unwrap();
  assert_eq!(vol.inode_usage().0, before);
  assert_eq!(vol.accounting().referenced_bytes, 0);
}

/// A-32: an attribute's value is reached only through its owner. Do find the attribute inode's
/// number and use it as a namespace object; expect stat, read, write, truncate, chmod, a hard link
/// and a reference all to refuse, and an attribute of an attribute to be `NotPermitted`.
#[test]
fn an_attribute_inode_is_never_a_namespace_object() {
  let (mut store, mut vol, file) = with_file(1 << 20);
  let root = vol.root_inode(&store).unwrap();
  vol
    .xattr_set(&mut store, file, b"user.a", b"secret", XattrSet::Either)
    .unwrap();
  let attribute = vol.xattr_inode(&store, file, b"user.a").unwrap();
  let mut buf = [0u8; 16];
  assert_eq!(vol.stat(&store, attribute), Err(VfsError::NotFound));
  assert_eq!(
    vol.read(&store, attribute, 0, &mut buf),
    Err(VfsError::NotFound)
  );
  assert_eq!(
    vol.write(&mut store, attribute, 0, b"x"),
    Err(VfsError::NotFound)
  );
  assert_eq!(
    vol.truncate(&mut store, attribute, 0),
    Err(VfsError::NotFound)
  );
  assert_eq!(
    vol.chmod(&mut store, attribute, 0o777),
    Err(VfsError::NotFound)
  );
  assert_eq!(vol.reference(&store, attribute), Err(VfsError::NotFound));
  assert_eq!(
    vol.link_no(&mut store, root, "leak", attribute),
    Err(VfsError::NotFound)
  );
  assert_eq!(
    vol.xattr_set(&mut store, attribute, b"user.b", b"v", XattrSet::Either),
    Err(VfsError::NotPermitted)
  );
  assert_eq!(value(&vol, &store, file, b"user.a").unwrap(), b"secret");
}

/// A-32, §4.8: do give a file and a directory attributes (one value past the inline threshold),
/// snapshot, change one, then rebuild the volume from its image in a fresh store; expect every
/// attribute of the head and of the snapshot to read back as it was.
#[test]
fn a_recovery_image_rebuilds_every_attribute() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 22);
  let root = vol.root_inode(&store).unwrap();
  let file = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  let dir = vol.mkdir_no(&mut store, root, "d", 0o755).unwrap();
  let long: Vec<u8> = (0..20_000u32).map(|i| i.to_le_bytes()[0]).collect();
  vol
    .xattr_set(&mut store, file, b"user.long", &long, XattrSet::Either)
    .unwrap();
  vol
    .xattr_set(
      &mut store,
      dir,
      b"com.apple.FinderInfo",
      &[3u8; 32],
      XattrSet::Either,
    )
    .unwrap();
  let snapshot = vol.snapshot(&mut store).unwrap();
  vol
    .xattr_set(
      &mut store,
      file,
      b"user.long",
      b"short now",
      XattrSet::Either,
    )
    .unwrap();
  let bytes = vol.to_image(&store, None).unwrap().to_content();

  let mut fresh = common::surviving(&store);
  let image = VolumeImage::from_content(&bytes).unwrap();
  let claims = common::claims(&mut fresh, &[&image]);
  let rebuilt = Volume::from_image(
    &mut fresh,
    &image,
    &claims,
    Box::new(StepClock::new(0, 1)),
    1 << 16,
    None,
  )
  .unwrap();
  assert_eq!(
    value(&rebuilt, &fresh, file, b"user.long").unwrap(),
    b"short now"
  );
  assert_eq!(
    value(&rebuilt, &fresh, dir, b"com.apple.FinderInfo").unwrap(),
    vec![3u8; 32]
  );
  let mut buf = vec![0u8; long.len()];
  let n = rebuilt
    .xattr_read_in(&fresh, snapshot, file, b"user.long", 0, &mut buf)
    .unwrap();
  assert_eq!(&buf[..n], &long[..], "the snapshot's value came back");
}

/// The whole working copy of `no`, or `None` when it has none.
fn working_copy(vol: &Volume, store: &Store, no: InodeNo) -> Option<Vec<u8>> {
  let attrs = vol.sidecar_attrs(store, no).unwrap()?;
  let mut buf = vec![0u8; usize::try_from(attrs.size).unwrap()];
  let n = vol.sidecar_read(store, no, 0, &mut buf).unwrap();
  buf.truncate(n);
  Some(buf)
}

/// §4.6 (the AppleDouble working copy): do write a working copy in two pieces and truncate it;
/// expect the bytes as written, no attribute listed for it, and a set under `Sidecar::Keep` to leave
/// it while a plain set (any other path) drops it.
#[test]
fn a_working_copy_holds_its_bytes_until_another_path_changes_the_attributes() {
  use slates_vfs::xattr::Sidecar;
  let (mut store, mut vol, file) = with_file(1 << 20);
  assert_eq!(working_copy(&vol, &store, file), None);
  vol.sidecar_write(&mut store, file, 0, b"header").unwrap();
  vol.sidecar_write(&mut store, file, 10, b"tail").unwrap();
  assert_eq!(
    working_copy(&vol, &store, file).unwrap(),
    b"header\0\0\0\0tail".to_vec()
  );
  vol.sidecar_truncate(&mut store, file, 6).unwrap();
  assert_eq!(
    working_copy(&vol, &store, file).unwrap(),
    b"header".to_vec()
  );
  assert!(vol.xattr_names(&store, file).unwrap().is_empty());
  vol
    .xattr_set_with(
      &mut store,
      file,
      b"user.a",
      b"v",
      XattrSet::Either,
      Sidecar::Keep,
    )
    .unwrap();
  assert_eq!(
    working_copy(&vol, &store, file).unwrap(),
    b"header".to_vec()
  );
  vol
    .xattr_set(&mut store, file, b"user.a", b"w", XattrSet::Either)
    .unwrap();
  assert_eq!(
    working_copy(&vol, &store, file),
    None,
    "another path dropped it"
  );
}

/// §4.6, §4.2: do give a file a working copy and an attribute, then unlink it; expect every inode
/// (the file, the attribute, the working copy) reclaimed. And a working copy dropped on its own
/// returns its inode too.
#[test]
fn a_working_copy_is_reclaimed_with_its_owner_or_when_dropped() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 20);
  let root = vol.root_inode(&store).unwrap();
  let before = vol.inode_usage().0;
  let file = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol.sidecar_write(&mut store, file, 0, &[9u8; 500]).unwrap();
  assert_eq!(vol.inode_usage().0, before + 2);
  vol.sidecar_drop(&mut store, file).unwrap();
  assert_eq!(vol.inode_usage().0, before + 1);
  vol.sidecar_write(&mut store, file, 0, b"again").unwrap();
  vol
    .xattr_set(&mut store, file, b"user.a", b"v", XattrSet::Either)
    .unwrap();
  vol.sidecar_write(&mut store, file, 0, b"again").unwrap();
  vol.unlink_no(&mut store, root, "f").unwrap();
  assert_eq!(vol.inode_usage().0, before);
  assert_eq!(vol.accounting().referenced_bytes, 0);
}

/// §4.6: do write into an attribute's value in place (a resource fork arriving in pieces); expect the
/// value to read back with each piece where it was written, the working copy kept.
#[test]
fn an_attribute_value_takes_a_write_in_place() {
  let (mut store, mut vol, file) = with_file(1 << 20);
  vol
    .xattr_set(
      &mut store,
      file,
      b"com.apple.ResourceFork",
      &[0u8; 8],
      XattrSet::Either,
    )
    .unwrap();
  vol.sidecar_write(&mut store, file, 0, b"copy").unwrap();
  vol
    .xattr_write_at(&mut store, file, b"com.apple.ResourceFork", 2, b"ab")
    .unwrap();
  vol
    .xattr_write_at(&mut store, file, b"com.apple.ResourceFork", 8, b"cd")
    .unwrap();
  assert_eq!(
    value(&vol, &store, file, b"com.apple.ResourceFork").unwrap(),
    b"\0\0ab\0\0\0\0cd".to_vec()
  );
  assert_eq!(working_copy(&vol, &store, file).unwrap(), b"copy".to_vec());
}

/// §4.8, §4.6: do give a file a working copy and no attributes, then rebuild from the image; expect
/// the working copy's bytes back.
#[test]
fn a_recovery_image_rebuilds_a_working_copy() {
  let (mut store, mut vol, file) = with_file(1 << 20);
  vol
    .sidecar_write(&mut store, file, 0, b"pending bytes")
    .unwrap();
  let bytes = vol.to_image(&store, None).unwrap().to_content();
  let mut fresh = common::surviving(&store);
  let image = VolumeImage::from_content(&bytes).unwrap();
  let claims = common::claims(&mut fresh, &[&image]);
  let rebuilt = Volume::from_image(
    &mut fresh,
    &image,
    &claims,
    Box::new(StepClock::new(0, 1)),
    1 << 16,
    None,
  )
  .unwrap();
  assert_eq!(
    working_copy(&rebuilt, &fresh, file).unwrap(),
    b"pending bytes".to_vec()
  );
}
