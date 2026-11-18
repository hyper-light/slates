//! What a Shards stream's guest held, carried by a VM's save and taken back at its restore (§4.6 the Shards form;
//! A-303): `Volume::held_by` reads what one attachment holds, and `Volume::adopt_held` gives a fresh attachment the
//! same on the volume saved or on a clone of its cut, whole or not at all. Driven against an in-memory scratch volume
//! on every host.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{clone_config, store, volume};
use slates_vfs::Volume;
use slates_vfs::ids::{InodeNo, RefOwner};
use slates_vfs::recover::InodeReferences;
use slates_vfs::volume::Store;

/// A volume where attachment 7 holds a directory looked up twice, a file it opened (a reference and an open), and a
/// file it holds open with its last name unlinked: the volume, then those three inodes.
fn held_volume(store: &mut Store) -> (Volume, [InodeNo; 3]) {
  let mut vol = volume(store, 1 << 30);
  let root = vol.root_inode(store).unwrap();
  let dir = vol.mkdir_no(store, root, "d", 0o755).unwrap();
  let file = vol.create_file_no(store, root, "f", 0o644).unwrap();
  vol.write(store, file, 0, b"kept").unwrap();
  let nameless = vol.create_file_no(store, root, "g", 0o644).unwrap();
  vol.write(store, nameless, 0, b"nameless").unwrap();
  let saved = RefOwner::Attachment(7);
  vol.reference_for(store, dir, saved).unwrap();
  vol.reference_for(store, dir, saved).unwrap();
  vol.reference_for(store, file, saved).unwrap();
  vol.open_for(file, saved);
  vol.reference_for(store, nameless, saved).unwrap();
  vol.unlink_no(store, root, "g").unwrap();
  (vol, [dir, file, nameless])
}

/// What attachment 7 of [`held_volume`] holds, in inode order.
fn expected(nodes: [InodeNo; 3]) -> Vec<InodeReferences> {
  let [dir, file, nameless] = nodes;
  let mut held = vec![
    InodeReferences {
      inode: dir.0,
      count: 2,
      opens: 0,
    },
    InodeReferences {
      inode: file.0,
      count: 1,
      opens: 1,
    },
    InodeReferences {
      inode: nameless.0,
      count: 1,
      opens: 0,
    },
  ];
  held.sort_by_key(|entry| entry.inode);
  held
}

/// A-303. Do: read what a stream's attachment holds; snapshot the volume; clone the snapshot; give a fresh attachment
/// on the clone what the first held; read the nameless file through it on the clone; give the open back as a handle
/// the restored stream never issued does at its release; forget the rest. Expect the holdings in inode order with
/// their opens; the clone's attachment to hold exactly those; the nameless file's bytes on the clone while it is held;
/// the open given back as a restored one; and nothing held after.
#[test]
fn what_a_stream_held_is_taken_back_on_a_clone_of_its_cut() {
  let mut store = store();
  let (mut vol, nodes) = held_volume(&mut store);
  let [dir, file, nameless] = nodes;
  let held = vol.held_by(7);
  assert_eq!(held, expected(nodes));
  assert_eq!(vol.held_by(8), Vec::new(), "an attachment holding nothing");
  let cut = vol.snapshot(&mut store).unwrap();
  let mut clone = Volume::clone_of(&store, &mut vol, cut, clone_config(9)).unwrap();
  clone.adopt_held(&store, 11, &held).unwrap();
  let restored = RefOwner::Attachment(11);
  assert_eq!(
    clone.held_by(11),
    held,
    "the clone's attachment holds what the saved one did"
  );
  let mut buf = [0u8; 8];
  let read = clone.read(&store, nameless, 0, &mut buf).unwrap();
  assert_eq!(
    &buf[..read],
    b"nameless",
    "the nameless file is the clone's while it is held"
  );
  assert!(
    clone
      .release_restored_open(&mut store, file, restored)
      .unwrap(),
    "the saved stream's handle gives its open back as one from before a takeover"
  );
  clone.forget_for(&mut store, dir, restored, 2).unwrap();
  clone.forget_for(&mut store, nameless, restored, 1).unwrap();
  assert_eq!(
    clone.held_by(11),
    Vec::new(),
    "nothing is held once the guest lets go"
  );
}

/// A-303. Do: give a fresh attachment tables naming an inode twice, inodes out of order, an entry of no references,
/// one of more opens than references, an inode the volume never had, and a count past what the inode's count takes;
/// then give an attachment that already holds something whole tables. Expect each refused, and no attachment to hold
/// anything it did not hold before: a refusal gives nothing, not even the entries before the one refused.
#[test]
fn tables_naming_what_no_stream_could_hold_give_nothing() {
  let mut store = store();
  let (mut vol, nodes) = held_volume(&mut store);
  let [dir, file, _] = nodes;
  let good = InodeReferences {
    inode: dir.0,
    count: 1,
    opens: 0,
  };
  let at = |inode: InodeNo, count: u32, opens: u32| InodeReferences {
    inode: inode.0,
    count,
    opens,
  };
  let (low, high) = if dir.0 < file.0 {
    (dir, file)
  } else {
    (file, dir)
  };
  let refused: Vec<Vec<InodeReferences>> = vec![
    vec![good, good],
    vec![at(high, 1, 0), at(low, 1, 0)],
    vec![good, at(file, 0, 0)],
    vec![good, at(file, 1, 2)],
    vec![good, at(InodeNo(u64::MAX >> 1), 1, 0)],
    vec![good, at(file, u32::MAX, 0)],
  ];
  for tables in refused {
    assert!(vol.adopt_held(&store, 12, &tables).is_err(), "{tables:?}");
    assert_eq!(vol.held_by(12), Vec::new(), "nothing given for {tables:?}");
  }
  assert!(
    vol.adopt_held(&store, 7, &[good]).is_err(),
    "an attachment that holds something takes no tables"
  );
  assert_eq!(vol.held_by(7), expected(nodes), "and keeps what it held");
}

/// A-303. Do: let the saved stream's attachment go (its sweep, as the stream's end does) and give a fresh attachment on
/// the same volume what it held. Expect the refusal: the nameless file went with the last reference, so the volume no
/// longer holds what the saved guest did, and nothing is given.
#[test]
fn the_volume_saved_refuses_once_a_nameless_file_it_held_is_gone() {
  let mut store = store();
  let (mut vol, nodes) = held_volume(&mut store);
  let held = vol.held_by(7);
  vol
    .sweep_attachment(&mut store, RefOwner::Attachment(7))
    .unwrap();
  let [_, _, nameless] = nodes;
  let mut buf = [0u8; 8];
  assert!(
    vol.read(&store, nameless, 0, &mut buf).is_err(),
    "the nameless file was reclaimed"
  );
  assert!(vol.adopt_held(&store, 13, &held).is_err());
  assert_eq!(vol.held_by(13), Vec::new());
}
