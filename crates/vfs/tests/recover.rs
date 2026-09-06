//! Volume recovery-image capture (§4.8, A-9): a scratch volume's whole durable state — its tree,
//! every file's bytes (inline and chunked alike), symlink targets, hard links and roots — is
//! captured into a canonical image, that image round-trips through its content bytes unchanged and
//! byte-identically, and a corrupt or foreign image is refused with a typed error rather than
//! read as a smaller volume. This is the capture half of the write→kill→restart→read proof: what a
//! running daemon would publish into anchor-owned RAM. Driven against an in-memory scratch volume
//! on every host.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{store, volume};
use slates_mem::SharedObject;
use slates_vfs::VfsError;
use slates_vfs::clock::StepClock;
use slates_vfs::ids::InodeNo;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::recover::{
  BodyImage, InodeImage, KeyedImage, KindImage, PolicyImage, ShardImage, VolumeImage,
};
use slates_vfs::volume::{Volume, VolumeConfig};

/// Shape: a content object comfortably larger than the built fixture's image (a 256 KiB file plus
/// its metadata), so the frame fits with room to spare.
const CONTENT_LEN: usize = 4 * 1024 * 1024;

/// A per-process content-object name within macOS's 31-character `shm_open` limit.
fn object_name(tag: &str) -> String {
  format!("slates-rec-{tag}-{}", std::process::id())
}

/// A deterministic, varied byte at index `i`, spread so a file crosses several chunks with no
/// repeat short enough to hide a mis-ordered chunk on recovery.
fn pattern_byte(i: u32) -> u8 {
  i.wrapping_mul(2_654_435_761).to_le_bytes()[0]
}

/// The inode image with number `no`; the walk yields each inode once, so this is unique.
fn inode(image: &VolumeImage, no: InodeNo) -> &InodeImage {
  let mut matches = image.inodes.iter().filter(|i| i.no == no.0);
  let found = matches.next().expect("the inode is present in the image");
  assert!(
    matches.next().is_none(),
    "each inode appears exactly once in the image (no {no:?})"
  );
  found
}

/// The child number of `name` in a directory inode's entries.
fn entry(image: &VolumeImage, dir: InodeNo, name: &str) -> u64 {
  let BodyImage::Directory { entries } = &inode(image, dir).body else {
    panic!("inode {dir:?} is a directory in the image");
  };
  entries
    .iter()
    .find(|e| e.name == name)
    .expect("the entry is present")
    .child
}

/// A scratch volume with a small inline file, a multi-chunk file in a subdirectory, a symlink and a
/// hard link, together with its captured image and the ids the assertions check.
struct Built {
  image: VolumeImage,
  root: InodeNo,
  dir: InodeNo,
  small: InodeNo,
  big: InodeNo,
  link: InodeNo,
  big_bytes: Vec<u8>,
}

/// Builds that volume and captures its image.
fn built() -> Built {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 30);
  let root = vol.root_inode(&store).unwrap();
  let dir = vol.mkdir_no(&mut store, root, "dir", 0o755).unwrap();
  let small = vol
    .create_file_no(&mut store, root, "small", 0o644)
    .unwrap();
  vol.write(&mut store, small, 0, b"hello").unwrap();
  let big = vol.create_file_no(&mut store, dir, "big", 0o600).unwrap();
  let big_bytes: Vec<u8> = (0..256u32 * 1024).map(pattern_byte).collect();
  vol.write(&mut store, big, 0, &big_bytes).unwrap();
  let link = vol.symlink_no(&mut store, root, "link", "dir/big").unwrap();
  // A second name for `small`, in another directory: a hard link.
  vol.link_no(&mut store, dir, "small_alias", small).unwrap();
  let image = vol.to_image(&store).unwrap();
  Built {
    image,
    root,
    dir,
    small,
    big,
    link,
    big_bytes,
  }
}

/// AC (§4.8, A-9): the image captures the volume's roots (prefix, policy, root number, origin).
#[test]
fn to_image_captures_the_roots() {
  let b = built();
  assert_eq!(b.image.prefix, 7, "the volume's inode prefix");
  assert_eq!(b.image.policy, PolicyImage::Fold, "the name policy");
  assert_eq!(b.image.root_no, b.root.0, "the root inode number");
  assert!(
    b.image.origin_epoch.is_none(),
    "a scratch volume has no origin"
  );
}

/// AC (§4.8, A-9): the image captures the tree — every entry names its child by number, and a hard
/// link is one inode reached by two names.
#[test]
fn to_image_captures_the_tree_and_hard_link() {
  let b = built();
  assert_eq!(entry(&b.image, b.root, "dir"), b.dir.0);
  assert_eq!(entry(&b.image, b.root, "small"), b.small.0);
  assert_eq!(entry(&b.image, b.root, "link"), b.link.0);
  assert_eq!(entry(&b.image, b.dir, "big"), b.big.0);
  assert_eq!(
    entry(&b.image, b.dir, "small_alias"),
    b.small.0,
    "the hard link names the same inode as `small`"
  );
  assert_eq!(
    inode(&b.image, b.small).attrs.nlink,
    2,
    "two links: `small` and its alias"
  );
}

/// AC (§4.8): an inline file's bytes and mode are captured.
#[test]
fn to_image_captures_an_inline_files_bytes() {
  let b = built();
  let small = inode(&b.image, b.small);
  assert_eq!(small.kind, KindImage::File);
  assert_eq!(small.attrs.mode & 0o777, 0o644);
  assert_eq!(
    small.body,
    BodyImage::File {
      bytes: b"hello".to_vec()
    }
  );
}

/// AC (§4.8): a multi-chunk file's every byte comes back through the read path, whole and in order.
#[test]
fn to_image_captures_a_multi_chunk_files_bytes() {
  let b = built();
  let big = inode(&b.image, b.big);
  assert_eq!(big.kind, KindImage::File);
  assert_eq!(
    big.body,
    BodyImage::File {
      bytes: b.big_bytes.clone()
    },
    "a chunked file's bytes are captured whole and in order"
  );
}

/// AC (§4.8): a symlink's target is captured.
#[test]
fn to_image_captures_a_symlink_target() {
  let b = built();
  let link = inode(&b.image, b.link);
  assert_eq!(link.kind, KindImage::Symlink);
  assert_eq!(
    link.body,
    BodyImage::Symlink {
      target: "dir/big".to_string()
    }
  );
}

/// AC (§4.8, A-9): a scratch volume rebuilt from its image is faithful. The rebuilt volume's own
/// image is byte-identical to the original's — so inode numbers, the tree, every file's bytes, the
/// attributes and the roots all came back — and a read by an inode number handed out before the
/// "restart" returns the same bytes. This is the write→[image]→[drop]→[rebuild]→read proof at the
/// volume level: dropping the store and rebuilding into a fresh one is the "kill and restart."
#[test]
fn a_volume_rebuilt_from_its_image_is_faithful() {
  let b = built();
  let original = b.image.to_content();

  // "Restart": a fresh store, rebuild the volume from the published image bytes.
  let mut fresh = store();
  let image = VolumeImage::from_content(&original).unwrap();
  let vol =
    Volume::from_image(&mut fresh, &image, Box::new(StepClock::new(0, 1)), 1 << 16).unwrap();

  // The rebuilt volume re-images byte-for-byte identically: nothing was lost or changed.
  let rebuilt = vol.to_image(&fresh).unwrap().to_content();
  assert_eq!(
    rebuilt, original,
    "the rebuilt volume re-images identically"
  );

  // The multi-chunk file reads back through its original inode number (which survived the rebuild).
  let mut got = vec![0u8; b.big_bytes.len()];
  let n = vol.read(&fresh, b.big, 0, &mut got).unwrap();
  got.truncate(n);
  assert_eq!(
    got, b.big_bytes,
    "the file reads back through its original inode number"
  );
}

/// AC (§4.8, A-9): a volume survives a content-object handoff — the memory-level shape of a daemon
/// restart with the anchor surviving. The running daemon publishes the image into a shared content
/// object; the restarted daemon re-opens the same object from its handoff and rebuilds; the writer's
/// mapping then goes away (the crashed process exits). Bytes written before the "restart" read back
/// after it, and the rebuilt volume is fully faithful. This is Ada's step-two proof at the library
/// level, through the actual restart-surviving primitive.
#[test]
fn a_volume_survives_a_content_object_handoff() {
  let b = built();

  // The running daemon publishes the image into the anchor content object.
  let mut object = SharedObject::create(&object_name("h"), CONTENT_LEN).unwrap();
  let handoff = object.handoff().unwrap();
  b.image.write_to(object.bytes_mut()).unwrap();

  // The restarted daemon re-opens the same object; the writer's mapping then goes away.
  let reattached = SharedObject::open(&handoff, CONTENT_LEN).unwrap();
  drop(object);
  let image = VolumeImage::read_from(reattached.bytes())
    .unwrap()
    .expect("the published image is present after the handoff");

  // Rebuild and read the bytes written before the "restart" through their original inode number.
  let mut fresh = store();
  let vol =
    Volume::from_image(&mut fresh, &image, Box::new(StepClock::new(0, 1)), 1 << 16).unwrap();
  let mut got = vec![0u8; b.big_bytes.len()];
  let n = vol.read(&fresh, b.big, 0, &mut got).unwrap();
  got.truncate(n);
  assert_eq!(
    got, b.big_bytes,
    "bytes written before the content-object handoff read back after it"
  );
  assert_eq!(
    vol.to_image(&fresh).unwrap(),
    image,
    "the rebuilt volume is fully faithful"
  );
}

/// A sixteen-byte routing key with a distinguishing final byte (a stand-in volume id).
fn key_bytes(n: u8) -> [u8; 16] {
  let mut key = [0u8; 16];
  key[15] = n;
  key
}

/// A scratch volume with the given inode-number prefix, for a multi-volume shard.
fn prefixed_volume(store: &mut slates_vfs::volume::Store, prefix: u16) -> Volume {
  Volume::create(
    store,
    VolumeConfig {
      prefix,
      names: NameEquivalence::Fold,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 16,
      clock: Box::new(StepClock::new(0, 1)),
    },
  )
  .unwrap()
}

/// AC (§4.8, A-9): a clone (a volume whose origin is another's snapshot) recovers its content —
/// both the bytes it inherited from the origin snapshot and the bytes it wrote after diverging. It
/// rebuilds as an independent volume (the O(1) sharing with the origin is a §4.2 efficiency
/// refinement, not a content property), and its origin epoch is restored.
#[test]
fn a_clone_recovers_inherited_and_diverged_content() {
  let mut src = store();
  let mut origin = prefixed_volume(&mut src, 7);
  let root = origin.root_inode(&src).unwrap();
  let inherited = origin
    .create_file_no(&mut src, root, "shared", 0o644)
    .unwrap();
  origin
    .write(&mut src, inherited, 0, b"from the origin")
    .unwrap();
  let snap = origin.snapshot(&mut src).unwrap();

  // A clone of that snapshot into a new volume (prefix 8); it inherits the origin's root and tree.
  let mut clone = Volume::clone_of(
    &src,
    &mut origin,
    snap,
    VolumeConfig {
      prefix: 8,
      names: NameEquivalence::Fold,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 16,
      clock: Box::new(StepClock::new(0, 1)),
    },
  )
  .unwrap();
  let clone_root = clone.root_inode(&src).unwrap();
  let own = clone
    .create_file_no(&mut src, clone_root, "clone-only", 0o644)
    .unwrap();
  clone.write(&mut src, own, 0, b"from the clone").unwrap();

  let image = clone.to_image(&src).unwrap();
  assert!(
    image.origin_epoch.is_some(),
    "a clone records its origin epoch"
  );

  let mut fresh = store();
  let recovered =
    Volume::from_image(&mut fresh, &image, Box::new(StepClock::new(0, 1)), 1 << 16).unwrap();
  assert_eq!(
    recovered.to_image(&fresh).unwrap(),
    image,
    "the clone rebuilds faithfully"
  );
  let mut buf = vec![0u8; 32];
  let n = recovered.read(&fresh, inherited, 0, &mut buf).unwrap();
  assert_eq!(&buf[..n], b"from the origin", "inherited content recovered");
  let n = recovered.read(&fresh, own, 0, &mut buf).unwrap();
  assert_eq!(&buf[..n], b"from the clone", "diverged content recovered");
}

/// AC (§4.8, A-9): a whole shard of volumes — one content object holds them all — survives a handoff.
/// Two volumes with distinct prefixes are imaged together into one shard image, published into a
/// content object, and after the "restart" every volume is recovered by key and rebuilds faithfully.
#[test]
fn a_whole_shard_of_volumes_survives_a_content_object_handoff() {
  let mut original = store();
  let mut a = prefixed_volume(&mut original, 7);
  let a_root = a.root_inode(&original).unwrap();
  let a_file = a.create_file_no(&mut original, a_root, "a", 0o644).unwrap();
  a.write(&mut original, a_file, 0, b"volume a bytes")
    .unwrap();
  let mut b = prefixed_volume(&mut original, 8);
  let b_root = b.root_inode(&original).unwrap();
  let b_file = b.create_file_no(&mut original, b_root, "b", 0o644).unwrap();
  b.write(&mut original, b_file, 0, b"volume b bytes")
    .unwrap();

  // The running daemon publishes the whole shard into its one content object.
  let shard = ShardImage::new(vec![
    KeyedImage {
      key: key_bytes(7),
      image: a.to_image(&original).unwrap(),
    },
    KeyedImage {
      key: key_bytes(8),
      image: b.to_image(&original).unwrap(),
    },
  ]);
  let mut object = SharedObject::create(&object_name("s"), CONTENT_LEN).unwrap();
  let handoff = object.handoff().unwrap();
  shard.write_to(object.bytes_mut()).unwrap();

  // The restarted daemon re-opens the object and recovers every volume into a fresh store.
  let reattached = SharedObject::open(&handoff, CONTENT_LEN).unwrap();
  drop(object);
  let recovered = ShardImage::read_from(reattached.bytes())
    .unwrap()
    .expect("the shard image is present after the handoff");
  let keys: Vec<[u8; 16]> = recovered.volumes.iter().map(|v| v.key).collect();
  assert_eq!(
    keys,
    vec![key_bytes(7), key_bytes(8)],
    "both volumes recovered, in key order"
  );

  let mut fresh = store();
  for keyed in &recovered.volumes {
    let vol = Volume::from_image(
      &mut fresh,
      &keyed.image,
      Box::new(StepClock::new(0, 1)),
      1 << 16,
    )
    .unwrap();
    assert_eq!(
      vol.to_image(&fresh).unwrap(),
      keyed.image,
      "volume with key {:?} rebuilds faithfully",
      keyed.key
    );
  }
}

/// A robustness gate on the content frame: an empty (fresh) object reads as "nothing to recover"
/// (`None`, so the caller starts a new volume), a published image reads back, a write torn by a
/// crash is caught by the CRC and refused rather than decoded, and a buffer too small to hold the
/// frame refuses the publish.
#[test]
fn the_content_frame_signals_empty_and_refuses_a_torn_write() {
  let b = built();
  let mut buf = vec![0u8; CONTENT_LEN];

  assert!(
    VolumeImage::read_from(&buf).unwrap().is_none(),
    "a fresh content object holds no image"
  );

  let total = b.image.write_to(&mut buf).unwrap();
  assert!(
    VolumeImage::read_from(&buf).unwrap().is_some(),
    "a published image reads back"
  );

  buf[total - 1] ^= 0xFF;
  assert!(
    matches!(
      VolumeImage::read_from(&buf),
      Err(VfsError::RecoveryIncomplete)
    ),
    "a torn write is refused, not decoded"
  );

  let mut tiny = vec![0u8; 4];
  assert!(
    matches!(b.image.write_to(&mut tiny), Err(VfsError::NoSpace)),
    "a buffer too small refuses the publish"
  );
}

/// AC (§4.8, A-9): a copy-on-write snapshot is captured with the tree frozen at it — a file
/// modified after the snapshot reads its new bytes in the head image and its old bytes in the
/// snapshot image, so the snapshot's content is captured through the snapshot, not the head.
#[test]
fn to_image_captures_a_snapshots_frozen_content() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 30);
  let root = vol.root_inode(&store).unwrap();
  let f = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, f, 0, b"before").unwrap();
  let _snap = vol.snapshot(&mut store).unwrap();
  vol.write(&mut store, f, 0, b"after-the-snapshot").unwrap();

  let image = vol.to_image(&store).unwrap();
  assert_eq!(image.snapshots.len(), 1, "the snapshot is captured");

  let head = image.inodes.iter().find(|i| i.no == f.0).unwrap();
  assert_eq!(
    head.body,
    BodyImage::File {
      bytes: b"after-the-snapshot".to_vec()
    },
    "the head image holds the post-snapshot content"
  );
  let frozen = image.snapshots[0]
    .inodes
    .iter()
    .find(|i| i.no == f.0)
    .unwrap();
  assert_eq!(
    frozen.body,
    BodyImage::File {
      bytes: b"before".to_vec()
    },
    "the snapshot image holds the content frozen at the snapshot"
  );
}

/// AC (§4.8, A-9): a volume with a copy-on-write snapshot rebuilds faithfully — the rebuilt
/// volume re-images byte-identically (head and snapshot content, snapshot ids and metadata all
/// came back), the head serves its post-snapshot content by inode number, and the snapshot serves
/// its frozen content through the same snapshot id the original issued (so the id survived).
#[test]
fn a_volume_with_a_snapshot_rebuilds_faithfully() {
  let mut src = store();
  let mut vol = volume(&mut src, 1 << 30);
  let root = vol.root_inode(&src).unwrap();
  let f = vol.create_file_no(&mut src, root, "f", 0o644).unwrap();
  vol.write(&mut src, f, 0, b"v1").unwrap();
  let snap = vol.snapshot(&mut src).unwrap();
  vol.write(&mut src, f, 0, b"v2-modified").unwrap();
  vol.create_file_no(&mut src, root, "g", 0o644).unwrap();

  let original = vol.to_image(&src).unwrap();

  let mut fresh = store();
  let recovered = Volume::from_image(
    &mut fresh,
    &original,
    Box::new(StepClock::new(0, 1)),
    1 << 16,
  )
  .unwrap();

  assert_eq!(
    recovered.to_image(&fresh).unwrap(),
    original,
    "the volume with a snapshot re-images identically after rebuild"
  );

  let mut buf = vec![0u8; 16];
  let n = recovered.read(&fresh, f, 0, &mut buf).unwrap();
  assert_eq!(
    &buf[..n],
    b"v2-modified",
    "the head serves its post-snapshot content"
  );
  let n = recovered.read_in(&fresh, snap, f, 0, &mut buf).unwrap();
  assert_eq!(
    &buf[..n],
    b"v1",
    "the recovered snapshot serves its frozen content through the original snapshot id"
  );
}

/// AC (§4.8): an image round-trips through its content bytes unchanged — the exact state a
/// restarted daemon would read back equals what the running one published.
#[test]
fn an_image_round_trips_through_its_content_bytes_unchanged() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 30);
  let root = vol.root_inode(&store).unwrap();
  let f = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, f, 0, b"round trip").unwrap();
  vol.mkdir_no(&mut store, root, "d", 0o755).unwrap();

  let image = vol.to_image(&store).unwrap();
  let bytes = image.to_content();
  let back = VolumeImage::from_content(&bytes).expect("a well-formed image decodes");
  assert_eq!(
    image, back,
    "the image survives the round trip through bytes"
  );
}

/// A determinism gate: two images of the same unchanged volume are byte-identical, so a recovery
/// image can be compared and its identity is stable (a golden the daemon-wiring slice will hash).
#[test]
fn two_images_of_the_same_volume_are_byte_identical() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 30);
  let root = vol.root_inode(&store).unwrap();
  for name in ["a", "b", "c"] {
    let f = vol.create_file_no(&mut store, root, name, 0o644).unwrap();
    vol.write(&mut store, f, 0, name.as_bytes()).unwrap();
  }

  let first = vol.to_image(&store).unwrap().to_content();
  let second = vol.to_image(&store).unwrap().to_content();
  assert_eq!(first, second, "the image is deterministic");
}

/// Whether content is refused as an unreadable image.
fn refused(bytes: &[u8]) -> bool {
  matches!(
    VolumeImage::from_content(bytes),
    Err(VfsError::RecoveryIncomplete)
  )
}

/// A hostile-input gate (§4.8: missing or unreadable state refuses, it is never an empty success):
/// empty, all-zero, truncated, foreign-magic and trailing-garbage content all refuse with a typed
/// [`VfsError::RecoveryIncomplete`], and none panics.
#[test]
fn a_corrupt_or_foreign_image_is_refused_not_read_as_empty() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 30);
  let root = vol.root_inode(&store).unwrap();
  let f = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, f, 0, b"payload").unwrap();
  let good = vol.to_image(&store).unwrap().to_content();

  assert!(refused(&[]), "empty content");
  assert!(refused(&[0u8; 16]), "an all-zero (fresh) content object");
  assert!(refused(&good[..good.len() / 2]), "a truncated image");

  let mut foreign = good.clone();
  foreign[0] ^= 0xFF;
  assert!(refused(&foreign), "a foreign magic");

  let mut trailing = good.clone();
  trailing.push(0);
  assert!(refused(&trailing), "trailing bytes after a whole image");
}
