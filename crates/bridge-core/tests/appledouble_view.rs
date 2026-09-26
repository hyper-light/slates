//! The AppleDouble view through the shared operation layer (§4.6 "Extended attributes over NFSv3"),
//! driven as macOS drives a `._` file over NFSv3, with the bytes macOS 26.4.1 itself wrote
//! (`tests/vectors/appledouble`): a create, whole and piecewise writes, a truncate, a removal. After
//! each step the owner's attributes in the volume's store are what the file says; the view reads back
//! the client's bytes; a change through another path is what the next read encodes.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_bridge_core::appledouble::{Decoded, MAX_HEADER_BYTES, decode};
use slates_bridge_core::{
  Attachments, Bridge, ObjectId, OpContext, Rights, SetAttr, View, VolumeBridge,
};
use slates_db::catalog::{Principal, VolumeId};
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::HostClock;
use slates_vfs::error::VfsError;
use slates_vfs::ids::InodeNo;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};
use slates_vfs::xattr::XattrSet;

/// Shape: the page and a small arena for the test volume.
const PAGE: usize = 4096;
const REGION_PAGES: usize = 4096;

fn rw_cx() -> OpContext {
  let mut attachments = Attachments::new();
  let id = attachments
    .attach(
      VolumeId { bytes: [0; 16] },
      View::Current,
      Principal::Uid { uid: 0 },
      Rights {
        read: true,
        write: true,
      },
    )
    .unwrap();
  attachments.context(id).unwrap()
}

fn oid(ino: u64) -> ObjectId {
  ObjectId::new(ino, 0)
}

fn store() -> Store {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(PAGE * REGION_PAGES, PAGE, false).unwrap())
    .unwrap();
  Store::new(
    &StoreConfig {
      page: PAGE,
      cache_line: 128,
      max_dirs: 64,
      max_inodes: 256,
      max_chunks: REGION_PAGES,
      max_dir_blocks: 64,
      dir_cutover: 16,
    },
    arena,
    0,
  )
}

fn volume(store: &mut Store) -> Volume {
  Volume::create(
    store,
    VolumeConfig {
      prefix: 1,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 16,
      clock: Box::new(HostClock::default()),
    },
  )
  .unwrap()
}

fn vector(name: &str) -> Vec<u8> {
  let path = format!(
    "{}/tests/vectors/appledouble/{name}",
    env!("CARGO_MANIFEST_DIR")
  );
  #[allow(clippy::disallowed_methods)] // reading a checked-in test vector
  std::fs::read(path).unwrap()
}

/// The owner's attributes in the store, `(name, value)` in name order.
fn stored(vol: &Volume, store: &Store, owner: InodeNo) -> Vec<(Vec<u8>, Vec<u8>)> {
  vol
    .xattr_names(store, owner)
    .unwrap()
    .into_iter()
    .map(|name| {
      let len = vol.xattr_len(store, owner, &name).unwrap();
      let mut value = vec![0u8; usize::try_from(len).unwrap()];
      vol.xattr_read(store, owner, &name, 0, &mut value).unwrap();
      (name.to_vec(), value)
    })
    .collect()
}

/// The attributes a sidecar's bytes decode to, `(name, value)` in name order.
fn decoded(file: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
  let prefix = &file[..file.len().min(MAX_HEADER_BYTES)];
  let Decoded::Complete(layout) = decode(prefix, file.len() as u64, &mut |_, _: &mut [u8]| 0)
  else {
    panic!("the bytes decode");
  };
  let mut pairs: Vec<_> = layout
    .attributes
    .into_iter()
    .map(|(name, span)| {
      let start = usize::try_from(span.offset).unwrap();
      let end = usize::try_from(span.end()).unwrap();
      (name, file[start..end].to_vec())
    })
    .collect();
  pairs.sort();
  pairs
}

/// The whole view of `owner`.
fn read_view(bridge: &mut VolumeBridge<'_>, cx: &OpContext, owner: InodeNo) -> Vec<u8> {
  let view = oid(owner.derived().0);
  let size = bridge.getattr(view, cx).unwrap().size;
  let mut out = Vec::new();
  bridge
    .read(view, cx, 0, u32::try_from(size).unwrap(), &mut out)
    .unwrap();
  out
}

/// §4.6: do what macOS does to set attributes on `f` (look up `._f`, find none, create it, write the
/// AppleDouble bytes); expect the store to hold exactly the attributes the bytes carry, the view to
/// read back those bytes, and no directory entry named `._f`.
#[test]
fn a_client_written_sidecar_becomes_the_owners_attributes() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let cx = rw_cx();
  let file = {
    let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
    let root = bridge.root(&cx).unwrap();
    let (attr, _) = bridge.create(oid(root), &cx, "f", 0o644, 0).unwrap();
    assert_eq!(
      bridge.appledouble_lookup(oid(root), &cx, "f"),
      Err(VfsError::NotFound),
      "no attributes, no view"
    );
    let view = bridge
      .appledouble_create(oid(root), &cx, "f", true)
      .unwrap();
    assert_eq!((view.size, view.mode, view.nlink), (0, 0o644, 1));
    assert_eq!(view.ino, InodeNo(attr.ino).derived().0);
    let bytes = vector("one-attr.bin");
    bridge.write(oid(view.ino), &cx, 0, &bytes).unwrap();
    assert_eq!(read_view(&mut bridge, &cx, InodeNo(attr.ino)), bytes);
    let dir = bridge.opendir(oid(root), &cx).unwrap();
    let names: Vec<String> = bridge
      .readdir(oid(root), &cx, dir, 0)
      .unwrap()
      .into_iter()
      .map(|entry| entry.name)
      .filter(|name| name != "." && name != "..")
      .collect();
    assert_eq!(
      names,
      vec!["f".to_owned()],
      "the view is not a directory entry"
    );
    InodeNo(attr.ino)
  };
  assert_eq!(stored(&vol, &store, file), decoded(&vector("one-attr.bin")));
}

/// §4.6: do overwrite the sidecar with each later capture, the last one in two pieces split inside the
/// attribute header; expect the store to follow each file (an attribute added, one removed, the Finder
/// Info set), and the piecewise write to end where the whole write would.
#[test]
fn every_later_sidecar_state_is_the_owners_attributes() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let cx = rw_cx();
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let root = bridge.root(&cx).unwrap();
  let (attr, _) = bridge.create(oid(root), &cx, "f", 0o644, 0).unwrap();
  let owner = InodeNo(attr.ino);
  let view = oid(
    bridge
      .appledouble_create(oid(root), &cx, "f", false)
      .unwrap()
      .ino,
  );
  for name in ["one-attr.bin", "two-attrs.bin", "finderinfo.bin"] {
    bridge.write(view, &cx, 0, &vector(name)).unwrap();
    let read = read_view(&mut bridge, &cx, owner);
    assert_eq!(decoded(&read), decoded(&vector(name)), "{name}");
  }
  let last = vector("after-remove.bin");
  bridge.write(view, &cx, 0x60, &last[0x60..]).unwrap();
  bridge.write(view, &cx, 0, &last[..0x60]).unwrap();
  drop(bridge);
  assert_eq!(stored(&vol, &store, owner), decoded(&last));
}

/// §4.6: do set attributes through another path (the store's own verb, as FUSE or the SDK would),
/// then read the view; expect a sidecar that decodes to those attributes. Then remove the view; expect
/// the attributes gone and the view with them.
#[test]
fn another_paths_attributes_are_encoded_and_removing_the_view_removes_them() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let cx = rw_cx();
  let owner = {
    let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
    let root = bridge.root(&cx).unwrap();
    InodeNo(bridge.mkdir(oid(root), &cx, "d", 0o755).unwrap().ino)
  };
  vol
    .xattr_set(
      &mut store,
      owner,
      b"user.from.linux",
      b"value",
      XattrSet::Either,
    )
    .unwrap();
  vol
    .xattr_set(
      &mut store,
      owner,
      b"com.apple.FinderInfo",
      &[1u8; 32],
      XattrSet::Either,
    )
    .unwrap();
  let expected = stored(&vol, &store, owner);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let root = bridge.root(&cx).unwrap();
  let view = bridge.appledouble_lookup(oid(root), &cx, "d").unwrap();
  assert_eq!(view.mode, 0o755 & 0o666);
  assert_eq!(decoded(&read_view(&mut bridge, &cx, owner)), expected);
  bridge.appledouble_remove(oid(root), &cx, "d").unwrap();
  assert_eq!(
    bridge.appledouble_lookup(oid(root), &cx, "d"),
    Err(VfsError::NotFound)
  );
  assert_eq!(
    bridge.getattr(oid(owner.derived().0), &cx),
    Err(VfsError::NotFound)
  );
  drop(bridge);
  assert!(stored(&vol, &store, owner).is_empty());
}

/// §4.6: do write a resource fork's middle through the view in pieces, as a client writes a large
/// value; expect each piece in the stored value at its place and the rest intact. Then truncate the
/// sidecar inside its header; expect the attributes left as they were (an incomplete file changes
/// nothing).
#[test]
fn a_value_written_in_pieces_lands_in_place_and_a_torn_file_changes_nothing() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let cx = rw_cx();
  let owner = {
    let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
    let root = bridge.root(&cx).unwrap();
    InodeNo(bridge.create(oid(root), &cx, "f", 0o600, 0).unwrap().0.ino)
  };
  let fork = vec![0u8; 20_000];
  vol
    .xattr_set(
      &mut store,
      owner,
      b"com.apple.ResourceFork",
      &fork,
      XattrSet::Either,
    )
    .unwrap();
  vol
    .xattr_set(&mut store, owner, b"user.a", b"alpha", XattrSet::Either)
    .unwrap();
  let in_place = vol.attribute_writes_in_place();
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let view = oid(owner.derived().0);
  let file = read_view(&mut bridge, &cx, owner);
  let fork_at = file.len() - fork.len();
  bridge
    .write(
      view,
      &cx,
      u64::try_from(fork_at + 100).unwrap(),
      b"piece-one",
    )
    .unwrap();
  bridge
    .write(
      view,
      &cx,
      u64::try_from(fork_at + 19_000).unwrap(),
      b"piece-two",
    )
    .unwrap();
  bridge
    .setattr(
      view,
      &cx,
      SetAttr {
        size: Some(0x40),
        ..SetAttr::default()
      },
    )
    .unwrap();
  drop(bridge);
  assert_eq!(
    vol.attribute_writes_in_place() - in_place,
    2,
    "both pieces took the write-through path, not a whole-value rewrite"
  );
  let mut expected = fork;
  expected[100..109].copy_from_slice(b"piece-one");
  expected[19_000..19_009].copy_from_slice(b"piece-two");
  assert_eq!(
    stored(&vol, &store, owner),
    vec![
      (b"com.apple.ResourceFork".to_vec(), expected),
      (b"user.a".to_vec(), b"alpha".to_vec())
    ]
  );
}

/// §4.6: a view's mode and owner are its owner's, so a `setattr` that would change them, or its times,
/// is refused rather than acknowledged and ignored.
#[test]
fn a_views_mode_owner_and_times_are_not_its_own_to_change() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let cx = rw_cx();
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let root = bridge.root(&cx).unwrap();
  bridge.create(oid(root), &cx, "f", 0o644, 0).unwrap();
  let view = oid(
    bridge
      .appledouble_create(oid(root), &cx, "f", false)
      .unwrap()
      .ino,
  );
  for changes in [
    SetAttr {
      mode: Some(0o600),
      ..SetAttr::default()
    },
    SetAttr {
      uid: Some(501),
      ..SetAttr::default()
    },
    SetAttr {
      mtime: Some(1),
      ..SetAttr::default()
    },
  ] {
    assert_eq!(
      bridge.setattr(view, &cx, changes),
      Err(VfsError::NotPermitted)
    );
  }
  assert!(
    bridge
      .setattr(
        view,
        &cx,
        SetAttr {
          mode: Some(0o644),
          ..SetAttr::default()
        }
      )
      .is_ok(),
    "the view's own mode is accepted"
  );
}
